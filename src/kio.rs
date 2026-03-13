use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::debug;

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
}

impl KioClient {
    pub fn new(shutdown: CancellationToken) -> Self {
        Self { shutdown, children: Arc::new(Mutex::new(HashSet::new())) }
    }

    // ── Exécute kioclient5 et retourne (exit_status, stderr) ─────────────────
    async fn run(&self, args: &[&str]) -> Result<(bool, String)> {
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

        let result = tokio::select! {
            biased;
            _ = self.shutdown.cancelled() => {
                let _ = child.kill().await;
                if let Some(pid) = pid { self.children.lock().await.remove(&pid); }
                return Err(anyhow!("shutdown: kioclient5 interrupted"));
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

    /// Exécute kioclient5 et retourne stdout (pour ls).
    async fn run_stdout(&self, args: &[&str]) -> Result<(bool, String)> {
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

        let result = tokio::select! {
            biased;
            _ = self.shutdown.cancelled() => {
                let _ = child.kill().await;
                if let Some(pid) = pid { self.children.lock().await.remove(&pid); }
                return Err(anyhow!("shutdown: kioclient5 ls interrupted"));
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

    /// Listing récursif : BFS avec ls à chaque niveau.
    /// Retourne un HashSet de tous les chemins complets (gdrive:/…/dir, gdrive:/…/file).
    async fn ls_recursive_impl(&self, root: &str) -> Result<HashSet<String>> {
        let mut result = HashSet::new();
        let mut queue = vec![root.trim_end_matches('/').to_string()];

        while let Some(dir) = queue.pop() {
            if self.shutdown.is_cancelled() {
                anyhow::bail!("shutdown: ls_recursive interrupted");
            }
            let children = self.ls(&dir).await?;
            for name in children {
                let full = format!("{dir}/{name}");
                result.insert(full.clone());
                // Si le nom finit par / c'est un dossier → descendre.
                // Sinon on tente un ls dessus (coûte 1 appel mais pas grave,
                // si c'est un fichier ls échouera vite).
                if name.ends_with('/') {
                    let trimmed = full.trim_end_matches('/').to_string();
                    result.insert(trimmed.clone());
                    queue.push(trimmed);
                }
            }
        }
        Ok(result)
    }

    /// Essaie de copier via `cat local | kioclient5 cat - remote` (atomique).
    async fn copy_atomic(&self, local: &Path, remote: &str) -> Result<()> {
        let local_file = tokio::fs::File::open(local).await
            .with_context(|| format!("cannot open {}", local.display()))?;

        let mut child = Command::new("kioclient5")
            .args(["cat", "-", remote])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .process_group(0)
            .spawn()
            .context("cannot spawn kioclient5 cat")?;

        let pid = child.id();
        if let Some(pid) = pid { self.children.lock().await.insert(pid); }

        let mut stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin pipe"))?;
        let mut reader = tokio::io::BufReader::new(local_file);

        tokio::select! {
            r = async {
                tokio::io::copy(&mut reader, &mut stdin).await
                    .context("pipe local → kioclient5")?;
                stdin.shutdown().await.context("close stdin")?;
                Ok::<_, anyhow::Error>(())
            } => r?,
            _ = self.shutdown.cancelled() => {
                let _ = child.kill().await;
                if let Some(pid) = pid { self.children.lock().await.remove(&pid); }
                return Err(anyhow!("shutdown: atomic copy interrupted"));
            }
        }

        let (ok, stderr) = {
            let mut buf = Vec::new();
            let mut se = child.stderr.take().unwrap();
            tokio::io::AsyncReadExt::read_to_end(&mut se, &mut buf).await.ok();
            let stderr = String::from_utf8_lossy(&buf).trim().to_string();
            let status = child.wait().await.context("wait cat")?;
            (status.success(), stderr)
        };
        if let Some(pid) = pid { self.children.lock().await.remove(&pid); }

        if !ok {
            anyhow::bail!("kioclient5 cat failed for {remote}: {stderr}");
        }
        Ok(())
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
    /// - Fichier **existe déjà** sur le remote → `cat` (pipe stdin) pour écraser.
    ///   `copy` créerait un doublon car GDrive autorise les noms identiques.
    /// - Fichier **nouveau** → `copy` d'abord (plus rapide), fallback `cat`.
    async fn copy_file(&self, local: &Path, remote: &str) -> Result<()> {
        let remote_exists = self.exists(remote).await.unwrap_or(false);

        if remote_exists {
            // Le fichier existe déjà → écrasement via cat (atomique).
            debug!(remote, "file exists on remote, using cat (overwrite)");
            return self.copy_atomic(local, remote).await;
        }

        // Fichier nouveau → essayer copy d'abord (plus rapide, pas de pipe).
        let local_str = local.to_str()
            .ok_or_else(|| anyhow!("non-UTF-8 path: {}", local.display()))?;
        let (ok, _stderr) = self.run(&["copy", local_str, remote]).await?;
        if ok {
            return Ok(());
        }

        // Fallback cat pour les cas où copy échoue (certains protocoles KIO).
        debug!(remote, "copy failed, fallback to cat");
        self.copy_atomic(local, remote).await
    }

    /// mkdir utilisant le cache de l'index distant (scan initial).
    /// Pas de `stat` individuel — on consulte le HashSet pré-rempli.
    async fn mkdir_if_absent(&self, remote: &str, remote_index: &HashSet<String>) -> Result<()> {
        if remote_index.contains(remote) {
            debug!(remote, "mkdir: in remote index, skip");
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

    /// copy/cat utilisant le cache de l'index distant (scan initial).
    /// Si le fichier est dans l'index → `cat` (écrasement).
    /// Sinon → `copy` (création), fallback `cat`.
    async fn copy_file_smart(&self, local: &Path, remote: &str, remote_index: &HashSet<String>) -> Result<()> {
        if remote_index.contains(remote) {
            debug!(remote, "file in remote index → cat (overwrite)");
            return self.copy_atomic(local, remote).await;
        }
        // Fichier nouveau → copy d'abord
        let local_str = local.to_str()
            .ok_or_else(|| anyhow!("non-UTF-8 path: {}", local.display()))?;
        let (ok, _) = self.run(&["copy", local_str, remote]).await?;
        if ok { return Ok(()); }
        debug!(remote, "copy failed for new file, fallback to cat");
        self.copy_atomic(local, remote).await
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

