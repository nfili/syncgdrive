use std::path::Path;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::config::AppConfig;
use crate::db::{Database, FileEntry};
use crate::engine::scan::retry;
use crate::engine::Task;
use crate::ignore::IgnoreMatcher;
use crate::remote::{path_cache::PathCache, RemoteProvider};

pub(crate) async fn handle(
    task: Task,
    cfg: &AppConfig,
    db: &Database,
    provider: &Arc<dyn RemoteProvider>,
    path_cache: &Arc<PathCache>,
    ignore: &IgnoreMatcher,
    shutdown: &CancellationToken,
) -> Result<()> {
    match task {
        Task::SyncFile { path, .. } => sync_file(&path, cfg, db, provider, path_cache, ignore, shutdown).await,
        Task::Delete(path)          => delete(&path, cfg, db, provider, path_cache, ignore, shutdown).await,
        Task::Rename { from, to }   => rename(&from, &to, cfg, db, provider, path_cache, ignore, shutdown).await,
    }
}

// ── Sync fichier ──────────────────────────────────────────────────────────────

async fn sync_file(
    path: &Path,
    cfg: &AppConfig,
    db: &Database,
    provider: &Arc<dyn RemoteProvider>,
    path_cache: &Arc<PathCache>,
    ignore: &IgnoreMatcher,
    shutdown: &CancellationToken,
) -> Result<()> {
    if ignore.is_ignored(path) { return Ok(()); }
    if !path.is_file()         { return Ok(()); }

    let file_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if file_size == 0 { return Ok(()); }

    let rel = rel_str(&cfg.sync_pairs[0].local_path, path)?;

    let mtime = mtime(path)?;
    if let Some(e) = db.get(&rel)? {
        if e.mtime == mtime { return Ok(()); }
    }

    if shutdown.is_cancelled() { return Ok(()); }
    let hash = match hash_file(path).await {
        Ok(h) => h,
        Err(_) if !path.is_file() => return Ok(()),
        Err(_) if shutdown.is_cancelled() => return Ok(()),
        Err(e) => return Err(e),
    };

    if let Some(e) = db.get(&rel)? {
        if e.hash == hash {
            db.upsert(&FileEntry { path: rel.clone(), hash, mtime })?;
            return Ok(());
        }
    }

    let file_name = path.file_name().unwrap().to_string_lossy().to_string();

    let parent_rel = path.parent()
        .and_then(|p| p.strip_prefix(&cfg.sync_pairs[0].local_path).ok())
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    let parent_id = if parent_rel.is_empty() {
        cfg.sync_pairs[0].remote_folder_id.clone()
    } else {
        path_cache.lookup(&parent_rel).await
            .context("Dossier parent introuvable dans le cache")?
            .drive_id
    };

    let existing_entry = path_cache.lookup(&rel).await;
    let existing_id = existing_entry.as_ref().map(|e| e.drive_id.as_str());

    // Upload !
    let res = retry(cfg, shutdown, "upload", || async {
        provider.upload(path, &parent_id, &file_name, existing_id).await
    }).await?;

    // On passe bien 3 arguments et on await !
    path_cache.insert(&rel, &res.drive_id, &parent_id).await;
    db.upsert(&FileEntry { path: rel.clone(), hash, mtime })?;

    debug!(local = %path.display(), drive_id = %res.drive_id, "synced");
    Ok(())
}

// ── Suppression ───────────────────────────────────────────────────────────────

async fn delete(
    path: &Path,
    cfg: &AppConfig,
    db: &Database,
    provider: &Arc<dyn RemoteProvider>,
    path_cache: &Arc<PathCache>,
    ignore: &IgnoreMatcher,
    shutdown: &CancellationToken,
) -> Result<()> {
    if ignore.is_ignored(path) { return Ok(()); }

    let rel = match rel_str(&cfg.sync_pairs[0].local_path, path) {
        Ok(r) => r,
        Err(_) => return Ok(()),
    };

    if let Some(entry) = path_cache.lookup(&rel).await {
        retry(cfg, shutdown, "delete", || async {
            provider.delete(&entry.drive_id).await
        }).await?;

        // ATTENTION: Si ta méthode s'appelle 'remove_cascades', change-la ici :
        path_cache.remove_cascades(&rel).await;
    }

    db.delete(&rel)?;
    debug!(rel, "deleted");
    Ok(())
}

// ── Renommage ─────────────────────────────────────────────────────────────────

async fn rename(
    from: &Path,
    to: &Path,
    cfg: &AppConfig,
    db: &Database,
    provider: &Arc<dyn RemoteProvider>,
    path_cache: &Arc<PathCache>,
    ignore: &IgnoreMatcher,
    shutdown: &CancellationToken,
) -> Result<()> {
    if ignore.is_ignored(from) && ignore.is_ignored(to) { return Ok(()); }

    let from_rel = rel_str(&cfg.sync_pairs[0].local_path, from).unwrap_or_default();
    let to_rel   = rel_str(&cfg.sync_pairs[0].local_path, to).unwrap_or_default();

    let from_entry = path_cache.lookup(&from_rel).await;
    let from_in_db = !from_rel.is_empty() && db.get(&from_rel)?.is_some();

    if from_entry.is_none() || !from_in_db {
        if to.is_file() && !ignore.is_ignored(to) {
            return sync_file(to, cfg, db, provider, path_cache, ignore, shutdown).await;
        }
        return Ok(());
    }

    let file_id = from_entry.unwrap().drive_id;
    let new_name = to.file_name().unwrap().to_string_lossy().to_string();

    let new_parent_rel = to.parent()
        .and_then(|p| p.strip_prefix(&cfg.sync_pairs[0].local_path).ok())
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    let new_parent_id = if new_parent_rel.is_empty() {
        cfg.sync_pairs[0].remote_folder_id.clone()
    } else {
        path_cache.lookup(&new_parent_rel).await
            .context("Dossier destination introuvable")?
            .drive_id
    };

    retry(cfg, shutdown, "rename", || async {
        provider.rename(&file_id, Some(&new_name), Some(&new_parent_id)).await
    }).await?;

    path_cache.remove_cascades(&from_rel).await;
    path_cache.insert(&to_rel, &file_id, &new_parent_id).await;
    db.rename(&from_rel, &to_rel)?;

    debug!(from = from_rel, to = to_rel, "renamed");
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
    let m = std::fs::metadata(path).context("stat")?;
    Ok(m.modified()?.duration_since(UNIX_EPOCH)?.as_secs() as i64)
}

async fn hash_file(path: &Path) -> Result<String> {
    let data = tokio::fs::read(path).await.context("read")?;
    let mut h = Sha256::new();
    h.update(&data);
    Ok(format!("{:x}", h.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    use crate::config::{AppConfig, SyncPair};
    use crate::remote::{ChangesPage, HealthStatus, RemoteIndex, UploadResult};

    // ─── LE FAUX FOURNISSEUR (MOCK) ───
    // Il intercepte les requêtes réseau et compte le nombre de fois où on l'appelle.
    struct MockProvider {
        pub uploads: AtomicUsize,
        pub deletes: AtomicUsize,
        pub renames: AtomicUsize,
    }

    impl MockProvider {
        fn new() -> Self {
            Self {
                uploads: AtomicUsize::new(0),
                deletes: AtomicUsize::new(0),
                renames: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl RemoteProvider for MockProvider {
        async fn check_health(&self) -> Result<HealthStatus> { Ok(HealthStatus::Unreachable) }
        async fn list_remote(&self, _root: &str) -> Result<RemoteIndex> { Ok(RemoteIndex{files: vec![], dirs: vec![]}) }
        async fn mkdir(&self, _parent: &str, _name: &str) -> Result<String> { Ok("mock_dir".into()) }

        async fn upload(&self, _local: &Path, _parent: &str, _name: &str, _exist: Option<&str>) -> Result<UploadResult> {
            self.uploads.fetch_add(1, Ordering::Relaxed);
            Ok(UploadResult {
                drive_id: "mock_file_id".into(),
                md5_checksum: "mock_md5".into(),
                size_bytes: 100,
            })
        }

        async fn delete(&self, _id: &str) -> Result<()> {
            self.deletes.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        async fn rename(&self, _id: &str, _name: Option<&str>, _parent: Option<&str>) -> Result<()> {
            self.renames.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        async fn get_changes(&self, _cursor: Option<&str>) -> Result<ChangesPage> { Ok(ChangesPage{changes: vec![], new_cursor: "".into(), has_more: false}) }
        async fn shutdown(&self) {}
    }

    // ─── HELPER DE TEST ───
    // ─── HELPER DE TEST ───
    async fn setup_test() -> (TempDir, AppConfig, Database, Arc<MockProvider>, Arc<PathCache>, IgnoreMatcher, CancellationToken) {
        let dir = TempDir::new().unwrap();

        let mut cfg = AppConfig::default();

        // On modifie la configuration par défaut au lieu de recréer un SyncPair incomplet
        if !cfg.sync_pairs.is_empty() {
            cfg.sync_pairs[0].local_path = dir.path().to_path_buf();
            cfg.sync_pairs[0].remote_folder_id = "root_id".into();
        } else {
            // Si la liste est vide, on la remplit avec les champs requis
            cfg.sync_pairs.push(SyncPair {
                local_path: dir.path().to_path_buf(),
                remote_folder_id: "root_id".into(),
                name: "Test Pair".into(),
                active: true,
                ignore_patterns: vec![],
                provider: "GoogleDrive".into(), // CORRECTION : Le champ s'appelle provider
            });
        }

        // Utilisation de Database::open() au lieu de new()
        let db_path = dir.path().join("test.db");
        let db = Database::open(&db_path).unwrap();
        db.init_and_migrate().unwrap();

        let provider = Arc::new(MockProvider::new());
        let path_cache = Arc::new(PathCache::new());
        let ignore = IgnoreMatcher::from_patterns(&[]).unwrap();
        let shutdown = CancellationToken::new();

        (dir, cfg, db, provider, path_cache, ignore, shutdown)
    }

    // ─── LES TESTS DU WORKER ───

    #[tokio::test]
    async fn sync_file_uploads_and_inserts_db() {
        let (dir, cfg, db, mock, cache, ignore, sd) = setup_test().await;
        let provider: Arc<dyn RemoteProvider> = mock.clone();

        let file_path = dir.path().join("test.txt");
        tokio::fs::write(&file_path, "hello bella!").await.unwrap();

        let task = Task::SyncFile { path: file_path.clone() };
        handle(task, &cfg, &db, &provider, &cache, &ignore, &sd).await.unwrap();

        // Vérifications
        assert_eq!(mock.uploads.load(Ordering::Relaxed), 1); // Upload a été appelé
        assert!(db.get("test.txt").unwrap().is_some()); // Fichier est bien dans la DB
        assert!(cache.lookup("test.txt").await.is_some()); // Et dans le PathCache !
    }

    #[tokio::test]
    async fn sync_file_skips_unchanged() {
        let (dir, cfg, db, mock, cache, ignore, sd) = setup_test().await;
        let provider: Arc<dyn RemoteProvider> = mock.clone();

        let file_path = dir.path().join("unchanged.txt");
        tokio::fs::write(&file_path, "same content").await.unwrap();

        // 1er passage : Ça upload
        let task1 = Task::SyncFile { path: file_path.clone() };
        handle(task1, &cfg, &db, &provider, &cache, &ignore, &sd).await.unwrap();
        assert_eq!(mock.uploads.load(Ordering::Relaxed), 1);

        // 2ème passage : Ça doit sauter (skip) car le fichier n'a pas changé
        let task2 = Task::SyncFile { path: file_path.clone() };
        handle(task2, &cfg, &db, &provider, &cache, &ignore, &sd).await.unwrap();
        assert_eq!(mock.uploads.load(Ordering::Relaxed), 1); // Le compteur reste à 1 !
    }

    #[tokio::test]
    async fn sync_file_skips_ignored() {
        let (dir, cfg, db, mock, cache, _, sd) = setup_test().await;
        let provider: Arc<dyn RemoteProvider> = mock.clone();

        // On configure une règle d'ignore
        let ignore = IgnoreMatcher::from_patterns(&["*.tmp".to_string()]).unwrap();

        let file_path = dir.path().join("secret.tmp");
        tokio::fs::write(&file_path, "do not sync").await.unwrap();

        let task = Task::SyncFile { path: file_path.clone() };
        handle(task, &cfg, &db, &provider, &cache, &ignore, &sd).await.unwrap();

        // L'upload ne doit pas avoir eu lieu
        assert_eq!(mock.uploads.load(Ordering::Relaxed), 0);
        assert!(db.get("secret.tmp").unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_removes_from_remote_and_db() {
        let (dir, cfg, db, mock, cache, ignore, sd) = setup_test().await;
        let provider: Arc<dyn RemoteProvider> = mock.clone();

        let file_path = dir.path().join("to_delete.txt");

        // On simule que le fichier était connu du système
        cache.insert("to_delete.txt", "drive_123", "parent").await;
        db.upsert(&FileEntry { path: "to_delete.txt".into(), hash: "xxx".into(), mtime: 123 }).unwrap();

        let task = Task::Delete(file_path);
        handle(task, &cfg, &db, &provider, &cache, &ignore, &sd).await.unwrap();

        // L'appel API delete a été passé
        assert_eq!(mock.deletes.load(Ordering::Relaxed), 1);
        // Le fichier est supprimé des bases locales
        assert!(db.get("to_delete.txt").unwrap().is_none());
        assert!(cache.lookup("to_delete.txt").await.is_none());
    }

    #[tokio::test]
    async fn rename_unknown_file_falls_back_to_sync() {
        let (dir, cfg, db, mock, cache, ignore, sd) = setup_test().await;
        let provider: Arc<dyn RemoteProvider> = mock.clone();

        let to_file = dir.path().join("final.txt");
        tokio::fs::write(&to_file, "content from .part").await.unwrap();

        // On renomme un fichier qui n'était pas dans la DB (typiquement un .part téléchargé)
        let task = Task::Rename {
            from: dir.path().join("final.txt.part"),
            to: to_file,
        };
        handle(task, &cfg, &db, &provider, &cache, &ignore, &sd).await.unwrap();

        // Ça ne doit PAS faire un rename API, mais déclencher un upload (fallback)
        assert_eq!(mock.renames.load(Ordering::Relaxed), 0);
        assert_eq!(mock.uploads.load(Ordering::Relaxed), 1);
        assert!(db.get("final.txt").unwrap().is_some());
    }
}