//! Abstraction des opérations distantes via le trait [`KioOps`].
//!
//! Ce module définit le contrat (`trait KioOps`) que tout backend de transfert
//! doit implémenter, ainsi que l'implémentation V1 [`KioClient`] qui délègue
//! à des sous-processus `kioclient5` (KDE Frameworks).
//!
//! # Architecture backend
//!
//! ```text
//! ┌─────────────┐     ┌─────────────┐     ┌──────────────────┐
//! │ SyncEngine  │────▶│ KioOps      │◀────│ KioClient (V1)   │  kioclient5
//! │ scan.rs     │     │ (trait)     │     └──────────────────┘
//! │ worker.rs   │     │             │◀────┌──────────────────┐
//! │ watcher.rs  │     │ ls_remote   │     │ GDriveClient(V2) │  API REST native
//! └─────────────┘     │ mkdir_p     │     └──────────────────┘
//!                     │ copy_file   │
//!                     │ delete      │
//!                     │ rename      │
//!                     └─────────────┘
//! ```
//!
//! # Stratégie anti-doublon GDrive
//!
//! Google Drive autorise plusieurs fichiers avec le même nom dans un dossier.
//! Le client utilise `--overwrite copy` pour écraser les fichiers existants
//! et éviter les doublons.
//!
//! # Contournements kioclient5 (V1)
//!
//! | Bug | Contournement |
//! |-----|---------------|
//! | `copy` fichier 0 octet → exit=0 mais rien créé | Skip fichiers vides dans `worker.rs` |
//! | `--overwrite` ignoré par certains backends | Fallback `copy` sans flag |
//! | Exit codes mensongers | Pas de contournement générique (→ V2) |
//!
//! # Gestion du shutdown
//!
//! Tous les sous-processus `kioclient5` sont lancés dans leur propre groupe
//! de processus (`process_group(0)`) et enregistrés dans un `HashSet<u32>`.
//! [`KioOps::terminate_all`] envoie `SIGTERM` à chacun d'entre eux.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

// ── Trait (mockable pour les tests) ──────────────────────────────────────────

#[async_trait::async_trait]
pub trait KioOps: Clone + Send + Sync + 'static {
    /// Liste récursive du remote. Retourne les chemins complets (gdrive:/…/file).
    async fn ls_remote(&self, remote_root: &str) -> Result<HashSet<String>>;
    async fn mkdir_p(&self, remote_root: &str, rel: &Path) -> Result<()>;
    async fn copy_file(&self, local: &Path, remote: &str) -> Result<()>;
    /// mkdir si absent du cache. Ne fait rien si le dossier y est déjà.
    async fn mkdir_if_absent(&self, remote: &str, remote_index: &HashSet<String>) -> Result<()>;
    /// copy/cat selon que le fichier existe déjà ou non dans le cache.
    async fn copy_file_smart(&self, local: &Path, remote: &str, remote_index: &HashSet<String>) -> Result<()>;
    async fn delete(&self, remote: &str) -> Result<()>;
    async fn rename(&self, from: &str, to: &str) -> Result<()>;
    async fn terminate_all(&self);
}

// ── Client réel ──────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct KioClient {
    shutdown: CancellationToken,
    /// PIDs des kioclient5 en vol, pour les tuer au shutdown.
    children: Arc<Mutex<HashSet<u32>>>,
    /// Timeout pour les opérations rapides (stat, mkdir, ls, rm, move).
    op_timeout: Duration,
}

impl KioClient {
    pub fn new(shutdown: CancellationToken, op_timeout: Duration) -> Self {
        Self { shutdown, children: Arc::new(Mutex::new(HashSet::new())), op_timeout }
    }

    // ── Exécute kioclient5 et retourne (exit_status, stderr) ─────────────────

    /// Exécute kioclient5 avec le timeout configuré (ops rapides : stat, mkdir, rm…).
    async fn run(&self, args: &[&str]) -> Result<(bool, String)> {
        self.run_impl(args, Some(self.op_timeout)).await
    }

    /// Exécute kioclient5 sans timeout (transferts de fichiers potentiellement longs).
    async fn run_untimed(&self, args: &[&str]) -> Result<(bool, String)> {
        self.run_impl(args, None).await
    }

    async fn run_impl(&self, args: &[&str], timeout: Option<Duration>) -> Result<(bool, String)> {
        debug!(?args, "kioclient5");

        let mut child = Command::new("kioclient5")
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            // Nouveau groupe de processus : kioclient5 ne reçoit plus SIGINT
            // envoyé au terminal (Ctrl+C ou kill -INT sur le PID parent).
            .process_group(0)
            .spawn()
            .with_context(|| format!("cannot spawn kioclient5 {args:?}"))?;

        let pid = child.id();
        if let Some(pid) = pid {
            self.children.lock().await.insert(pid);
        }

        let mut stderr_handle = child.stderr.take()
            .ok_or_else(|| anyhow!("no stderr pipe"))?;

        let timeout_fut = async {
            match timeout {
                Some(d) => tokio::time::sleep(d).await,
                None    => std::future::pending().await,
            }
        };

        let result = tokio::select! {
            biased;
            _ = self.shutdown.cancelled() => {
                let _ = child.kill().await;
                if let Some(pid) = pid { self.children.lock().await.remove(&pid); }
                return Err(anyhow!("shutdown: kioclient5 interrupted"));
            }
            _ = timeout_fut => {
                warn!(?args, timeout_secs = ?timeout, "kioclient5 timeout — kill");
                let _ = child.kill().await;
                if let Some(pid) = pid { self.children.lock().await.remove(&pid); }
                return Err(anyhow!("timeout ({timeout:?}): kioclient5 {args:?}"));
            }
            r = async {
                // Tâche de surveillance : kill immédiat si shutdown pendant wait()
                let sd = self.shutdown.clone();
                let kill_task = tokio::spawn(async move {
                    sd.cancelled().await;
                    if let Some(p) = pid {
                        unsafe { libc::kill(p as libc::pid_t, libc::SIGTERM); }
                    }
                });

                let mut buf = Vec::new();
                tokio::io::AsyncReadExt::read_to_end(&mut stderr_handle, &mut buf).await.ok();
                let stderr = String::from_utf8_lossy(&buf).trim().to_string();
                let status = child.wait().await.context("wait kioclient5")?;

                kill_task.abort();
                Ok::<_, anyhow::Error>((status.success(), stderr))
            } => r,
        };

        if let Some(pid) = pid { self.children.lock().await.remove(&pid); }
        result
    }

    /// Vérifie l'existence d'un chemin distant (stat).
    async fn exists(&self, remote: &str) -> Result<bool> {
        let (ok, _) = self.run(&["stat", remote]).await?;
        Ok(ok)
    }

    /// Exécute kioclient5 et retourne stdout (pour ls), avec timeout.
    async fn run_stdout(&self, args: &[&str]) -> Result<(bool, String)> {
        self.run_stdout_impl(args, Some(self.op_timeout)).await
    }

    async fn run_stdout_impl(&self, args: &[&str], timeout: Option<Duration>) -> Result<(bool, String)> {
        debug!(?args, "kioclient5 (stdout)");

        let mut child = Command::new("kioclient5")
            .args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .with_context(|| format!("cannot spawn kioclient5 {args:?}"))?;

        let pid = child.id();
        if let Some(pid) = pid { self.children.lock().await.insert(pid); }

        let mut stdout_handle = child.stdout.take()
            .ok_or_else(|| anyhow!("no stdout pipe"))?;

        let timeout_fut = async {
            match timeout {
                Some(d) => tokio::time::sleep(d).await,
                None    => std::future::pending().await,
            }
        };

        let result = tokio::select! {
            biased;
            _ = self.shutdown.cancelled() => {
                let _ = child.kill().await;
                if let Some(pid) = pid { self.children.lock().await.remove(&pid); }
                return Err(anyhow!("shutdown: kioclient5 ls interrupted"));
            }
            _ = timeout_fut => {
                warn!(?args, timeout_secs = ?timeout, "kioclient5 stdout timeout — kill");
                let _ = child.kill().await;
                if let Some(pid) = pid { self.children.lock().await.remove(&pid); }
                return Err(anyhow!("timeout ({timeout:?}): kioclient5 {args:?}"));
            }
            r = async {
                let sd = self.shutdown.clone();
                let kill_task = tokio::spawn(async move {
                    sd.cancelled().await;
                    if let Some(p) = pid {
                        unsafe { libc::kill(p as libc::pid_t, libc::SIGTERM); }
                    }
                });
                let mut buf = Vec::new();
                tokio::io::AsyncReadExt::read_to_end(&mut stdout_handle, &mut buf).await.ok();
                let stdout = String::from_utf8_lossy(&buf).to_string();
                let status = child.wait().await.context("wait kioclient5 ls")?;
                kill_task.abort();
                Ok::<_, anyhow::Error>((status.success(), stdout))
            } => r,
        };

        if let Some(pid) = pid { self.children.lock().await.remove(&pid); }
        result
    }

    /// `kioclient5 ls <url>` → liste des noms enfants directs.
    async fn ls(&self, remote: &str) -> Result<Vec<String>> {
        let (ok, stdout) = self.run_stdout(&["ls", remote]).await?;
        if !ok {
            return Ok(Vec::new()); // dossier n'existe pas → vide
        }
        Ok(stdout.lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect())
    }

    /// Listing récursif : BFS concurrent (jusqu'à 8 ls simultanés).
    /// Retourne un HashSet de tous les chemins complets (gdrive:/…/dir, gdrive:/…/file).
    async fn ls_recursive_impl(&self, root: &str) -> Result<HashSet<String>> {
        const MAX_CONCURRENT_LS: usize = 8;

        let mut result = HashSet::new();
        let mut queue = vec![root.trim_end_matches('/').to_string()];
        let sem = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_LS));
        let mut dirs_listed = 0usize;

        while !queue.is_empty() {
            if self.shutdown.is_cancelled() {
                anyhow::bail!("shutdown: ls_recursive interrupted");
            }

            // Lancer tous les ls du niveau courant en parallèle (borné par sémaphore).
            let batch: Vec<String> = std::mem::take(&mut queue);
            let batch_size = batch.len();
            let mut set = tokio::task::JoinSet::new();

            for dir in batch {
                let kio = self.clone();
                let sem = sem.clone();
                set.spawn(async move {
                    let _permit = sem.acquire().await
                        .map_err(|_| anyhow!("semaphore closed"))?;
                    let children = kio.ls(&dir).await?;
                    Ok::<_, anyhow::Error>((dir, children))
                });
            }

            while let Some(res) = set.join_next().await {
                if self.shutdown.is_cancelled() {
                    set.abort_all();
                    anyhow::bail!("shutdown: ls_recursive interrupted");
                }
                let (dir, children) = res.context("ls task panicked")??;
                dirs_listed += 1;
                for name in children {
                    let full = format!("{dir}/{name}");
                    result.insert(full.clone());
                    if name.ends_with('/') {
                        let trimmed = full.trim_end_matches('/').to_string();
                        result.insert(trimmed.clone());
                        queue.push(trimmed);
                    }
                }
            }

            debug!(
                dirs_listed,
                batch_size,
                next_level = queue.len(),
                total_entries = result.len(),
                "ls_recursive: level done"
            );
        }
        Ok(result)
    }

    /// Copie avec --overwrite (écrase le fichier s'il existe déjà, évite les
    /// doublons GDrive). Fallback sans --overwrite si le flag n'est pas supporté.
    async fn copy_overwrite(&self, local: &Path, remote: &str) -> Result<()> {
        let local_str = local.to_str()
            .ok_or_else(|| anyhow!("non-UTF-8 path: {}", local.display()))?;

        // --overwrite doit être AVANT la commande (cf. kioclient5 --help).
        let (ok, _) = self.run_untimed(&["--overwrite", "copy", local_str, remote]).await?;
        if ok { return Ok(()); }

        // Fallback sans --overwrite (fichier nouveau ou backend qui ignore le flag).
        debug!(remote, "--overwrite copy failed, retrying without flag");
        let (ok2, stderr2) = self.run_untimed(&["copy", local_str, remote]).await?;
        if ok2 { return Ok(()); }

        anyhow::bail!("copy to {remote} failed: {stderr2}");
    }
}

#[async_trait::async_trait]
impl KioOps for KioClient {
    /// Listing récursif du dossier distant.
    /// Un seul `ls` par dossier au lieu d'un `stat` par fichier.
    async fn ls_remote(&self, remote_root: &str) -> Result<HashSet<String>> {
        debug!(remote_root, "building remote index via recursive ls");
        let index = self.ls_recursive_impl(remote_root).await?;
        debug!(count = index.len(), "remote index built");
        Ok(index)
    }

    /// Crée récursivement les composants de `rel` sous `remote_root`.
    /// Utilise des appels `stat` individuels (utilisé par le watcher en temps réel).
    async fn mkdir_p(&self, remote_root: &str, rel: &Path) -> Result<()> {
        let mut current = remote_root.trim_end_matches('/').to_string();
        for component in rel.components() {
            let part = component.as_os_str().to_string_lossy();
            current = format!("{current}/{part}");

            // ── Vérifier AVANT de créer ───────────────────────────────────────
            if self.exists(&current).await? {
                debug!(remote = %current, "mkdir: already exists, skip");
                continue;
            }

            // Le dossier n'existe pas → le créer.
            let (ok, stderr) = self.run(&["mkdir", &current]).await?;
            if ok { continue; }

            // mkdir a échoué — peut-être un problème de latence GDrive.
            // Attente courte puis re-vérification.
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_millis(600)) => {}
                _ = self.shutdown.cancelled() => {
                    return Err(anyhow!("shutdown: mkdir_p interrupted"));
                }
            }
            if self.exists(&current).await? { continue; }

            anyhow::bail!("mkdir {current} failed: {stderr}");
        }
        Ok(())
    }

    /// Copie un fichier local vers le remote.
    ///
    /// **Stratégie anti-doublon GDrive** :
    /// `--overwrite copy` écrase le fichier s'il existe déjà.
    /// Pas de `stat` préalable — le flag --overwrite gère les deux cas
    /// (fichier nouveau ET fichier existant).
    async fn copy_file(&self, local: &Path, remote: &str) -> Result<()> {
        self.copy_overwrite(local, remote).await
    }

    /// mkdir utilisant le cache de l'index distant (scan initial).
    /// Pas de `stat` individuel — on consulte le HashSet pré-rempli.
    /// Sécurité supplémentaire : si le chemin n'est pas dans le cache,
    /// on fait un `stat` avant `mkdir` car GDrive autorise les noms identiques.
    async fn mkdir_if_absent(&self, remote: &str, remote_index: &HashSet<String>) -> Result<()> {
        if remote_index.contains(remote) {
            debug!(remote, "mkdir: in remote index, skip");
            return Ok(());
        }
        // Sécurité anti-doublon GDrive : vérifier via stat avant de créer.
        if self.exists(remote).await.unwrap_or(false) {
            debug!(remote, "mkdir: exists (stat), skip");
            return Ok(());
        }
        let (ok, stderr) = self.run(&["mkdir", remote]).await?;
        if ok { return Ok(()); }
        // Latence GDrive : peut-être créé entre-temps
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {}
            _ = self.shutdown.cancelled() => {
                return Err(anyhow!("shutdown: mkdir_if_absent interrupted"));
            }
        }
        if self.exists(remote).await? { return Ok(()); }
        anyhow::bail!("mkdir {remote} failed: {stderr}");
    }

    /// copy utilisant --overwrite (le remote_index n'est plus nécessaire
    /// pour la copie depuis l'introduction de --overwrite, mais le paramètre
    /// est conservé pour compatibilité API).
    async fn copy_file_smart(&self, local: &Path, remote: &str, _remote_index: &HashSet<String>) -> Result<()> {
        self.copy_overwrite(local, remote).await
    }

    async fn delete(&self, remote: &str) -> Result<()> {
        // Essaie rm puis del (certaines versions de KIO n'ont qu'une des deux).
        let (ok, _) = self.run(&["rm", remote]).await?;
        if ok { return Ok(()); }
        let (ok2, stderr) = self.run(&["del", remote]).await?;
        if ok2 { return Ok(()); }
        // Si le fichier n'existe déjà plus, c'est OK.
        if !self.exists(remote).await? { return Ok(()); }
        anyhow::bail!("delete {remote} failed: {stderr}");
    }

    async fn rename(&self, from: &str, to: &str) -> Result<()> {
        let (ok, stderr) = self.run(&["move", from, to]).await?;
        if ok { return Ok(()); }
        // Si source disparue et dest présente, déjà renommé.
        if !self.exists(from).await? && self.exists(to).await? { return Ok(()); }
        anyhow::bail!("rename {from} → {to} failed: {stderr}");
    }

    async fn terminate_all(&self) {
        let pids: Vec<u32> = self.children.lock().await.iter().copied().collect();
        for pid in pids {
            // Le processus tourne dans son propre groupe (process_group(0) → pgid == pid).
            // On lui envoie SIGTERM directement via libc sans spawner une Command.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM); }
        }
    }
}

// ── Helpers publics ───────────────────────────────────────────────────────────

/// Construit l'URL distante correspondant à `local_path`.
pub fn to_remote(remote_root: &str, local_root: &Path, local_path: &Path) -> Result<String> {
    let rel = local_path.strip_prefix(local_root)
        .with_context(|| format!("{} not under {}", local_path.display(), local_root.display()))?;
    Ok(format!("{}/{}", remote_root.trim_end_matches('/'), rel.display()))
}

// ── Mock pour les tests ───────────────────────────────────────────────────────

#[cfg(test)]
pub mod mock {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Mock KioOps qui enregistre les appels pour assertions dans les tests.
    #[derive(Clone)]
    pub struct MockKio {
        pub copies: Arc<Mutex<Vec<(String, String)>>>,       // (local, remote)
        pub deletes: Arc<Mutex<Vec<String>>>,                 // remote
        pub renames: Arc<Mutex<Vec<(String, String)>>>,       // (from, to)
        pub mkdirs: Arc<Mutex<Vec<String>>>,                  // remote
        pub copy_count: Arc<AtomicUsize>,
        pub fail_next: Arc<Mutex<Option<String>>>,            // message d'erreur
    }

    impl MockKio {
        pub fn new() -> Self {
            Self {
                copies: Arc::new(Mutex::new(Vec::new())),
                deletes: Arc::new(Mutex::new(Vec::new())),
                renames: Arc::new(Mutex::new(Vec::new())),
                mkdirs: Arc::new(Mutex::new(Vec::new())),
                copy_count: Arc::new(AtomicUsize::new(0)),
                fail_next: Arc::new(Mutex::new(None)),
            }
        }

        async fn maybe_fail(&self) -> Result<()> {
            let mut guard = self.fail_next.lock().await;
            if let Some(msg) = guard.take() {
                anyhow::bail!(msg);
            }
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl KioOps for MockKio {
        async fn ls_remote(&self, _remote_root: &str) -> Result<HashSet<String>> {
            Ok(HashSet::new())
        }
        async fn mkdir_p(&self, _remote_root: &str, _rel: &Path) -> Result<()> {
            Ok(())
        }
        async fn copy_file(&self, local: &Path, remote: &str) -> Result<()> {
            self.maybe_fail().await?;
            self.copy_count.fetch_add(1, Ordering::Relaxed);
            self.copies.lock().await.push((local.display().to_string(), remote.to_string()));
            Ok(())
        }
        async fn mkdir_if_absent(&self, remote: &str, _remote_index: &HashSet<String>) -> Result<()> {
            self.mkdirs.lock().await.push(remote.to_string());
            Ok(())
        }
        async fn copy_file_smart(&self, local: &Path, remote: &str, _remote_index: &HashSet<String>) -> Result<()> {
            self.copy_file(local, remote).await
        }
        async fn delete(&self, remote: &str) -> Result<()> {
            self.maybe_fail().await?;
            self.deletes.lock().await.push(remote.to_string());
            Ok(())
        }
        async fn rename(&self, from: &str, to: &str) -> Result<()> {
            self.maybe_fail().await?;
            self.renames.lock().await.push((from.to_string(), to.to_string()));
            Ok(())
        }
        async fn terminate_all(&self) {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn to_remote_basic() {
        let remote = to_remote(
            "gdrive:/Drive/Backup",
            &PathBuf::from("/home/user/project"),
            &PathBuf::from("/home/user/project/src/main.rs"),
        ).unwrap();
        assert_eq!(remote, "gdrive:/Drive/Backup/src/main.rs");
    }

    #[test]
    fn to_remote_trailing_slash() {
        let remote = to_remote(
            "gdrive:/Drive/Backup/",
            &PathBuf::from("/home/user/project"),
            &PathBuf::from("/home/user/project/file.txt"),
        ).unwrap();
        assert_eq!(remote, "gdrive:/Drive/Backup/file.txt");
    }

    #[test]
    fn to_remote_root_file() {
        let remote = to_remote(
            "gdrive:/D",
            &PathBuf::from("/tmp"),
            &PathBuf::from("/tmp/test.txt"),
        ).unwrap();
        assert_eq!(remote, "gdrive:/D/test.txt");
    }

    #[test]
    fn to_remote_path_outside_root_fails() {
        let result = to_remote(
            "gdrive:/D",
            &PathBuf::from("/home/user/project"),
            &PathBuf::from("/other/path/file.txt"),
        );
        assert!(result.is_err());
    }

    #[test]
    fn to_remote_with_spaces() {
        let remote = to_remote(
            "gdrive:/Mon Drive",
            &PathBuf::from("/home/user/project"),
            &PathBuf::from("/home/user/project/test de fichier.txt"),
        ).unwrap();
        assert_eq!(remote, "gdrive:/Mon Drive/test de fichier.txt");
    }

    #[test]
    fn to_remote_deeply_nested() {
        let remote = to_remote(
            "gdrive:/D",
            &PathBuf::from("/root"),
            &PathBuf::from("/root/a/b/c/d/e.txt"),
        ).unwrap();
        assert_eq!(remote, "gdrive:/D/a/b/c/d/e.txt");
    }
}
