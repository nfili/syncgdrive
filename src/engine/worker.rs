use std::collections::HashSet;
use std::path::Path;
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::config::AppConfig;
use crate::db::{Database, FileEntry};
use crate::engine::Task;
use crate::engine::scan::retry;
use crate::ignore::IgnoreMatcher;
use crate::kio::{to_remote, KioOps};

pub(crate) async fn handle<K: KioOps>(
    task: Task,
    cfg: &AppConfig,
    db: &Database,
    kio: &K,
    ignore: &IgnoreMatcher,
    shutdown: &CancellationToken,
) -> Result<()> {
    match task {
        Task::SyncFile { path, remote_index } => sync_file(&path, cfg, db, kio, ignore, shutdown, remote_index.as_deref()).await,
        Task::Delete(path)          => delete(&path, cfg, db, kio, ignore, shutdown).await,
        Task::Rename { from, to }   => rename(&from, &to, cfg, db, kio, ignore, shutdown).await,
    }
}

// ── Sync fichier ──────────────────────────────────────────────────────────────

async fn sync_file<K: KioOps>(
    path: &Path,
    cfg: &AppConfig,
    db: &Database,
    kio: &K,
    ignore: &IgnoreMatcher,
    shutdown: &CancellationToken,
    remote_index: Option<&HashSet<String>>,
) -> Result<()> {
    if ignore.is_ignored(path) { return Ok(()); }
    if !path.is_file()         { return Ok(()); }

    // ── Skip fichiers vides ─────────────────────────────────────────────────
    // kioclient5 copy renvoie exit=0 mais ne crée rien sur GDrive pour les
    // fichiers de 0 octet. On les ignore silencieusement — quand ils auront
    // du contenu, le watcher (CloseWrite) déclenchera un nouveau sync.
    let file_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if file_size == 0 {
        debug!(path = %path.display(), "sync: fichier vide, ignoré");
        return Ok(());
    }

    let rel = rel_str(&cfg.local_root, path)?;

    // ── Vérification mtime (rapide) ───────────────────────────────────────────
    let mtime = mtime(path)?;
    if let Some(e) = db.get(&rel)? {
        if e.mtime == mtime {
            return Ok(());          // pas changé
        }
    }

    // ── Vérification hash (plus coûteuse) ─────────────────────────────────────
    if shutdown.is_cancelled() { return Ok(()); }
    let hash = match hash_file(path).await {
        Ok(h) => h,
        Err(_) if !path.is_file() => return Ok(()), // fichier disparu entre-temps (TOCTOU)
        Err(_) if shutdown.is_cancelled() => return Ok(()), // shutdown en cours
        Err(e) => return Err(e),
    };
    if let Some(e) = db.get(&rel)? {
        if e.hash == hash {
            db.upsert(&FileEntry { path: rel, hash, mtime })?;
            return Ok(());          // contenu identique, mise à jour mtime seule
        }
    }

    // ── Copie vers le remote ──────────────────────────────────────────────────
    let remote = to_remote(&cfg.remote_root, &cfg.local_root, path)?;

    // Si on a un index distant (tâche issue du scan), utiliser copy_file_smart
    // pour éviter un `stat` individuel par fichier. Sinon (watcher), fallback
    // sur copy_file qui fait un `stat` avant de décider copy vs cat.
    retry(cfg, shutdown, "copy_file", || async {
        if let Some(idx) = remote_index {
            kio.copy_file_smart(path, &remote, idx).await
        } else {
            kio.copy_file(path, &remote).await
        }
    }).await?;

    db.upsert(&FileEntry { path: rel.clone(), hash, mtime })?;
    debug!(local = %path.display(), remote, "synced");
    Ok(())
}

// ── Suppression ───────────────────────────────────────────────────────────────

async fn delete<K: KioOps>(
    path: &Path,
    cfg: &AppConfig,
    db: &Database,
    kio: &K,
    ignore: &IgnoreMatcher,
    shutdown: &CancellationToken,
) -> Result<()> {
    if ignore.is_ignored(path) { return Ok(()); }

    let rel = match rel_str(&cfg.local_root, path) {
        Ok(r) => r,
        Err(_) => return Ok(()),    // hors root
    };
    let remote = to_remote(&cfg.remote_root, &cfg.local_root, path)?;

    retry(cfg, shutdown, "delete", || async {
        kio.delete(&remote).await
    }).await?;

    db.delete(&rel)?;
    debug!(remote, "deleted");
    Ok(())
}

// ── Renommage ─────────────────────────────────────────────────────────────────

async fn rename<K: KioOps>(
    from: &Path,
    to: &Path,
    cfg: &AppConfig,
    db: &Database,
    kio: &K,
    ignore: &IgnoreMatcher,
    shutdown: &CancellationToken,
) -> Result<()> {
    if ignore.is_ignored(from) && ignore.is_ignored(to) { return Ok(()); }

    let from_rel = rel_str(&cfg.local_root, from).unwrap_or_default();
    let to_rel   = rel_str(&cfg.local_root, to).unwrap_or_default();

    // ── Cas spécial : fichier temporaire jamais synchronisé ────────────────
    // Pattern fréquent : éditeurs/file managers écrivent un .part/.tmp/~ puis
    // renomment vers le nom final. Le fichier source n'a jamais été uploadé
    // sur le remote → on ne peut pas faire de `move` distant.
    // Solution : si `from` n'est pas dans la DB, traiter comme un sync du `to`.
    let from_in_db = if !from_rel.is_empty() {
        db.get(&from_rel)?.is_some()
    } else {
        false
    };

    if !from_in_db {
        // Source jamais synchronisée → upload direct du fichier destination.
        if to.is_file() && !ignore.is_ignored(to) {
            debug!(
                from = %from.display(), to = %to.display(),
                "rename: source absente de la DB → fallback sync_file"
            );
            return sync_file(to, cfg, db, kio, ignore, shutdown, None).await;
        }
        return Ok(());
    }

    // ── Renommage classique (source connue en DB) ──────────────────────────
    let from_remote = to_remote(&cfg.remote_root, &cfg.local_root, from)?;
    let to_remote   = to_remote(&cfg.remote_root, &cfg.local_root, to)?;

    retry(cfg, shutdown, "rename", || async {
        kio.rename(&from_remote, &to_remote).await
    }).await?;

    if !from_rel.is_empty() && !to_rel.is_empty() {
        db.rename(&from_rel, &to_rel)?;
    }

    debug!(from = from_remote, to = to_remote, "renamed");
    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn rel_str(root: &Path, path: &Path) -> Result<String> {
    Ok(path.strip_prefix(root)
        .with_context(|| format!("{} not under {}", path.display(), root.display()))?
        .to_string_lossy()
        .to_string())
}

fn mtime(path: &Path) -> Result<i64> {
    let m = std::fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?;
    Ok(m.modified()?.duration_since(UNIX_EPOCH)?.as_secs() as i64)
}

async fn hash_file(path: &Path) -> Result<String> {
    let data = tokio::fs::read(path).await
        .with_context(|| format!("read {}", path.display()))?;
    let mut h = Sha256::new();
    h.update(&data);
    Ok(format!("{:x}", h.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kio::mock::MockKio;
    use tempfile::TempDir;
    use std::sync::atomic::Ordering;

    fn test_cfg(local_root: &std::path::Path) -> AppConfig {
        AppConfig {
            local_root: local_root.to_path_buf(),
            remote_root: "gdrive:/Test".into(),
            max_workers: 1,
            retry: crate::config::RetryConfig {
                max_attempts: 1,
                initial_backoff_ms: 50,
                max_backoff_ms: 100,
            },
            ..Default::default()
        }
    }

    fn test_db() -> Database {
        let f = tempfile::NamedTempFile::new().unwrap();
        let db = Database::open(f.path()).unwrap();
        db.init_schema().unwrap();
        db
    }

    #[tokio::test]
    async fn sync_file_uploads_and_inserts_db() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("hello.txt");
        std::fs::write(&file, "hello world").unwrap();

        let cfg = test_cfg(dir.path());
        let db = test_db();
        let kio = MockKio::new();
        let ignore = IgnoreMatcher::from_patterns(&[]).unwrap();
        let shutdown = CancellationToken::new();

        let task = Task::SyncFile { path: file.clone(), remote_index: None };
        handle(task, &cfg, &db, &kio, &ignore, &shutdown).await.unwrap();

        assert_eq!(kio.copy_count.load(Ordering::Relaxed), 1);
        assert!(db.get("hello.txt").unwrap().is_some());
    }

    #[tokio::test]
    async fn sync_file_skips_empty_file() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("empty.txt");
        std::fs::write(&file, "").unwrap(); // 0 octets

        let cfg = test_cfg(dir.path());
        let db = test_db();
        let kio = MockKio::new();
        let ignore = IgnoreMatcher::from_patterns(&[]).unwrap();
        let shutdown = CancellationToken::new();

        let task = Task::SyncFile { path: file, remote_index: None };
        handle(task, &cfg, &db, &kio, &ignore, &shutdown).await.unwrap();

        assert_eq!(kio.copy_count.load(Ordering::Relaxed), 0);
        assert_eq!(db.count().unwrap(), 0);
    }

    #[tokio::test]
    async fn sync_file_skips_ignored() {
        let dir = TempDir::new().unwrap();
        let target_dir = dir.path().join("target").join("debug");
        std::fs::create_dir_all(&target_dir).unwrap();
        let file = target_dir.join("binary");
        std::fs::write(&file, "bin content").unwrap();

        let cfg = test_cfg(dir.path());
        let db = test_db();
        let kio = MockKio::new();
        let ignore = IgnoreMatcher::from_patterns(&["**/target/**".into()]).unwrap();
        let shutdown = CancellationToken::new();

        let task = Task::SyncFile { path: file, remote_index: None };
        handle(task, &cfg, &db, &kio, &ignore, &shutdown).await.unwrap();

        assert_eq!(kio.copy_count.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn sync_file_skips_unchanged() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("same.txt");
        std::fs::write(&file, "content").unwrap();

        let cfg = test_cfg(dir.path());
        let db = test_db();
        let kio = MockKio::new();
        let ignore = IgnoreMatcher::from_patterns(&[]).unwrap();
        let shutdown = CancellationToken::new();

        // Premier sync : upload
        let task = Task::SyncFile { path: file.clone(), remote_index: None };
        handle(task, &cfg, &db, &kio, &ignore, &shutdown).await.unwrap();
        assert_eq!(kio.copy_count.load(Ordering::Relaxed), 1);

        // Deuxième sync : même fichier, mtime identique → skip
        let task2 = Task::SyncFile { path: file, remote_index: None };
        handle(task2, &cfg, &db, &kio, &ignore, &shutdown).await.unwrap();
        assert_eq!(kio.copy_count.load(Ordering::Relaxed), 1); // pas incrémenté
    }

    #[tokio::test]
    async fn delete_removes_from_remote_and_db() {
        let dir = TempDir::new().unwrap();
        let cfg = test_cfg(dir.path());
        let db = test_db();
        let kio = MockKio::new();
        let ignore = IgnoreMatcher::from_patterns(&[]).unwrap();
        let shutdown = CancellationToken::new();

        // Pré-peupler la DB
        db.upsert(&crate::db::FileEntry {
            path: "deleted.txt".into(), hash: "h".into(), mtime: 1
        }).unwrap();

        let task = Task::Delete(dir.path().join("deleted.txt"));
        handle(task, &cfg, &db, &kio, &ignore, &shutdown).await.unwrap();

        assert_eq!(kio.deletes.lock().await.len(), 1);
        assert!(db.get("deleted.txt").unwrap().is_none());
    }

    #[tokio::test]
    async fn rename_known_file_renames_on_remote() {
        let dir = TempDir::new().unwrap();
        let to_file = dir.path().join("new_name.txt");
        std::fs::write(&to_file, "content").unwrap();

        let cfg = test_cfg(dir.path());
        let db = test_db();
        let kio = MockKio::new();
        let ignore = IgnoreMatcher::from_patterns(&[]).unwrap();
        let shutdown = CancellationToken::new();

        // Fichier source connu en DB
        db.upsert(&crate::db::FileEntry {
            path: "old_name.txt".into(), hash: "h".into(), mtime: 1
        }).unwrap();

        let task = Task::Rename {
            from: dir.path().join("old_name.txt"),
            to: to_file,
        };
        handle(task, &cfg, &db, &kio, &ignore, &shutdown).await.unwrap();

        assert_eq!(kio.renames.lock().await.len(), 1);
        assert!(db.get("old_name.txt").unwrap().is_none());
        assert!(db.get("new_name.txt").unwrap().is_some());
    }

    #[tokio::test]
    async fn rename_unknown_file_falls_back_to_sync() {
        let dir = TempDir::new().unwrap();
        let to_file = dir.path().join("final.txt");
        std::fs::write(&to_file, "content from .part").unwrap();

        let cfg = test_cfg(dir.path());
        let db = test_db();
        let kio = MockKio::new();
        let ignore = IgnoreMatcher::from_patterns(&[]).unwrap();
        let shutdown = CancellationToken::new();

        // .part n'est PAS dans la DB → fallback sync_file
        let task = Task::Rename {
            from: dir.path().join("final.txt.part"),
            to: to_file,
        };
        handle(task, &cfg, &db, &kio, &ignore, &shutdown).await.unwrap();

        // Pas de rename remote, mais un copy (fallback sync_file)
        assert_eq!(kio.renames.lock().await.len(), 0);
        assert_eq!(kio.copy_count.load(Ordering::Relaxed), 1);
        assert!(db.get("final.txt").unwrap().is_some());
    }

    #[tokio::test]
    async fn sync_file_nonexistent_path_is_ok() {
        let dir = TempDir::new().unwrap();
        let cfg = test_cfg(dir.path());
        let db = test_db();
        let kio = MockKio::new();
        let ignore = IgnoreMatcher::from_patterns(&[]).unwrap();
        let shutdown = CancellationToken::new();

        // Fichier qui n'existe pas (supprimé entre-temps)
        let task = Task::SyncFile {
            path: dir.path().join("gone.txt"),
            remote_index: None,
        };
        handle(task, &cfg, &db, &kio, &ignore, &shutdown).await.unwrap();
        assert_eq!(kio.copy_count.load(Ordering::Relaxed), 0);
    }
}
