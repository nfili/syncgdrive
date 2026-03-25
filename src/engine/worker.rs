use std::path::Path;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use md5::{Digest, Md5};
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::config::AppConfig;
use crate::db::{Database, FileEntry};
use crate::engine::bandwidth::ProgressTracker;
use crate::engine::scan::retry;
use crate::engine::Task;
use crate::ignore::IgnoreMatcher;
use crate::remote::{path_cache::PathCache, RemoteProvider};

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle(
    task: Task,
    cfg: &AppConfig,
    db: &Database,
    provider: &Arc<dyn RemoteProvider>,
    path_cache: &Arc<PathCache>,
    ignore: &IgnoreMatcher,
    tracker: Arc<ProgressTracker>,
    shutdown: &CancellationToken,
    dry_run: bool,
) -> Result<()> {
    if dry_run {
        match &task {
            Task::SyncFile { path } => {
                let size = tokio::fs::metadata(path).await.map(|m| m.len()).unwrap_or(0);
                tracing::info!("[DRY-RUN] upload: {} ({} octets)", path.display(), size);
            }
            Task::Delete(path) => {
                tracing::info!("[DRY-RUN] delete: {}", path.display());
            }
            Task::Rename { from, to } => {
                tracing::info!("[DRY-RUN] rename: {} → {}", from.display(), to.display());
            }
        }
        return Ok(());
    }
    match task {
        Task::SyncFile { path } => {
            sync_file(
                &path, cfg, db, provider, path_cache, ignore, tracker, shutdown,
            )
            .await
        }
        Task::Delete(path) => delete(&path, cfg, db, provider, path_cache, ignore, shutdown).await,
        Task::Rename { from, to } => {
            rename(
                &from, &to, cfg, db, provider, path_cache, ignore, tracker, shutdown,
            )
            .await
        }
    }
}

// ── Sync fichier ──────────────────────────────────────────────────────────────
#[allow(clippy::too_many_arguments)]
async fn sync_file(
    path: &Path,
    cfg: &AppConfig,
    db: &Database,
    provider: &Arc<dyn RemoteProvider>,
    path_cache: &Arc<PathCache>,
    ignore: &IgnoreMatcher,
    tracker: Arc<ProgressTracker>,
    shutdown: &CancellationToken,
) -> Result<()> {
    if ignore.is_ignored(path) {
        return Ok(());
    }
    if !path.is_file() {
        return Ok(());
    }

    let metadata = std::fs::metadata(path).context("Metadata inaccessible")?;
    let file_size = metadata.len();
    if file_size == 0 {
        return Ok(());
    }

    let primary = cfg.get_primary_pair().context("Aucun dossier")?;
    let rel = rel_str(&primary.local_path, path)?;
    let mtime = mtime(path)?;

    // Check rapide MTime
    if let Some(e) = db.get(&rel)? {
        if e.mtime == mtime && path_cache.lookup(&rel).await.is_some() {
            return Ok(());
        }
    }

    if shutdown.is_cancelled() {
        return Ok(());
    }

    let hash = match hash_file(path).await {
        Ok(h) => h,
        Err(_) if !path.is_file() => return Ok(()),
        Err(_) if shutdown.is_cancelled() => return Ok(()),
        Err(e) => return Err(e),
    };

    // Check profond Hash
    if let Some(e) = db.get(&rel)? {
        if e.hash == hash && path_cache.lookup(&rel).await.is_some() {
            db.upsert(&FileEntry {
                path: rel.clone(),
                hash,
                mtime,
            })?;
            return Ok(());
        }
    }

    let file_name = path.file_name().unwrap().to_string_lossy().to_string();
    let parent_rel = path
        .parent()
        .and_then(|p| p.strip_prefix(&primary.local_path).ok())
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    let parent_id = if parent_rel.is_empty() {
        primary.remote_folder_id.clone()
    } else {
        path_cache
            .lookup(&parent_rel)
            .await
            .map(|e| e.drive_id)
            .context(format!(
                "Dossier parent introuvable dans le cache : {}",
                parent_rel
            ))?
    };

    let existing_entry = path_cache.lookup(&rel).await;
    let mut last_attempt_id = existing_entry.map(|e| e.drive_id);

    let parent_dir_name = path
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .to_string_lossy()
        .to_string();
    tracker.set_current_file(parent_dir_name, file_name.clone(), file_size);

    // Boucle d'Upload explicite pour gérer parfaitement last_attempt_id sans closure
    let mut attempt = 1;
    let max_attempts = cfg.retry.max_attempts;
    let backoff = cfg.retry.initial_backoff_ms;

    let res = loop {
        if shutdown.is_cancelled() {
            anyhow::bail!("Annulé");
        }

        match provider
            .upload(
                path,
                &parent_id,
                &file_name,
                last_attempt_id.as_deref(),
                tracker.clone(),
            )
            .await
        {
            Ok(upload_res) => {
                match crate::engine::integrity::verify_upload(path, &upload_res).await {
                    Ok(crate::engine::integrity::IntegrityResult::Ok) => break upload_res,
                    _ => {
                        last_attempt_id = Some(upload_res.drive_id.clone());
                        if attempt >= max_attempts {
                            anyhow::bail!(
                                "mismatch md5 détecté post-upload après {} essais",
                                max_attempts
                            );
                        }
                        tracing::warn!(
                            "⚠️ Intégrité corrompue pour {}. Re-tentative automatique...",
                            path.display()
                        );
                    }
                }
            }
            Err(e) => {
                if attempt >= max_attempts {
                    anyhow::bail!("upload a échoué: {}", e);
                }
                tracing::warn!(
                    "upload a échoué (essai {}/{}): {}, nouvelle tentative dans {}ms",
                    attempt,
                    max_attempts,
                    e,
                    backoff
                );
            }
        }

        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(backoff)) => {}
            _ = shutdown.cancelled() => anyhow::bail!("Annulé proprement pendant le retry"),
        }
        attempt += 1;
    };

    path_cache.insert(&rel, &res.drive_id, &parent_id).await;
    db.upsert(&FileEntry {
        path: rel.clone(),
        hash,
        mtime,
    })?;

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
    if ignore.is_ignored(path) {
        return Ok(());
    }

    let primary = cfg.get_primary_pair().context("Aucun dossier")?;
    let rel = match rel_str(&primary.local_path, path) {
        Ok(r) => r,
        Err(_) => return Ok(()),
    };

    if let Some(entry) = path_cache.lookup(&rel).await {
        retry(cfg, shutdown, "delete", || async {
            provider.delete(&entry.drive_id).await
        })
        .await?;

        path_cache.remove_cascades(&rel).await;
    }

    db.delete(&rel)?;
    debug!(rel, "deleted");
    Ok(())
}

// ── Renommage ─────────────────────────────────────────────────────────────────
#[allow(clippy::too_many_arguments)]
async fn rename(
    from: &Path,
    to: &Path,
    cfg: &AppConfig,
    db: &Database,
    provider: &Arc<dyn RemoteProvider>,
    path_cache: &Arc<PathCache>,
    ignore: &IgnoreMatcher,
    tracker: Arc<ProgressTracker>,
    shutdown: &CancellationToken,
) -> Result<()> {
    if ignore.is_ignored(from) && ignore.is_ignored(to) {
        return Ok(());
    }

    let primary = cfg.get_primary_pair().context("Aucun dossier")?;
    let from_rel = rel_str(&primary.local_path, from).unwrap_or_default();
    let to_rel = rel_str(&primary.local_path, to).unwrap_or_default();

    let from_entry = path_cache.lookup(&from_rel).await;
    let from_in_db = !from_rel.is_empty() && db.get(&from_rel)?.is_some();

    if from_entry.is_none() || !from_in_db {
        if to.is_file() && !ignore.is_ignored(to) {
            return sync_file(to, cfg, db, provider, path_cache, ignore, tracker, shutdown).await;
        }
        return Ok(());
    }

    let file_id = from_entry.unwrap().drive_id;
    let new_name = to.file_name().unwrap().to_string_lossy().to_string();

    let new_parent_rel = to
        .parent()
        .and_then(|p| p.strip_prefix(&primary.local_path).ok())
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    let new_parent_id = if new_parent_rel.is_empty() {
        primary.remote_folder_id.clone()
    } else {
        path_cache
            .lookup(&new_parent_rel)
            .await
            .context("Dossier destination introuvable")?
            .drive_id
    };

    retry(cfg, shutdown, "rename", || async {
        provider
            .rename(&file_id, Some(&new_name), Some(&new_parent_id))
            .await
    })
    .await?;

    path_cache.remove_cascades(&from_rel).await;
    path_cache.insert(&to_rel, &file_id, &new_parent_id).await;
    db.rename(&from_rel, &to_rel)?;

    debug!(from = from_rel, to = to_rel, "renamed");
    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn rel_str(root: &Path, path: &Path) -> Result<String> {
    Ok(path
        .strip_prefix(root)
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
    let mut h = Md5::new();
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
        async fn list_remote(&self, _root: &str) -> Result<RemoteIndex> {
            Ok(RemoteIndex {
                files: vec![],
                dirs: vec![],
            })
        }
        async fn mkdir(&self, _parent: &str, _name: &str) -> Result<String> {
            Ok("mock_dir".into())
        }
        async fn upload(
            &self,
            local: &Path,
            _parent: &str,
            _name: &str,
            _exist: Option<&str>,
            _tracker: Arc<ProgressTracker>,
        ) -> Result<UploadResult> {
            self.uploads.fetch_add(1, Ordering::Relaxed);
            let data = tokio::fs::read(local).await.unwrap_or_default();
            let mut hasher = md5::Md5::new();
            md5::Digest::update(&mut hasher, &data);
            let real_hash = format!("{:x}", hasher.finalize());

            Ok(UploadResult {
                drive_id: "mock_file_id".into(),
                md5_checksum: real_hash,
                size_bytes: data.len() as u64,
            })
        }
        async fn delete(&self, _id: &str) -> Result<()> {
            self.deletes.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        async fn rename(
            &self,
            _id: &str,
            _name: Option<&str>,
            _parent: Option<&str>,
        ) -> Result<()> {
            self.renames.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        async fn get_changes(&self, _cursor: Option<&str>) -> Result<ChangesPage> {
            Ok(ChangesPage {
                changes: vec![],
                new_cursor: "".into(),
                has_more: false,
            })
        }
        async fn check_health(&self) -> Result<HealthStatus> {
            Ok(HealthStatus::Unreachable)
        }
        async fn shutdown(&self) {}
    }

    async fn setup_test() -> (
        TempDir,
        AppConfig,
        Database,
        Arc<MockProvider>,
        Arc<PathCache>,
        IgnoreMatcher,
        Arc<ProgressTracker>,
        CancellationToken,
    ) {
        let dir = TempDir::new().unwrap();
        let mut cfg = AppConfig::default();
        cfg.sync_pairs.push(SyncPair {
            local_path: dir.path().to_path_buf(),
            remote_folder_id: "root_id".into(),
            name: "Test Pair".into(),
            active: true,
            ignore_patterns: vec![],
            provider: "GoogleDrive".into(),
        });
        let db_path = dir.path().join("test.db");
        let db = Database::open(&db_path).unwrap();
        db.init_and_migrate().unwrap();

        (
            dir,
            cfg,
            db,
            Arc::new(MockProvider::new()),
            Arc::new(PathCache::new()),
            IgnoreMatcher::from_patterns(&[]).unwrap(),
            Arc::new(crate::engine::bandwidth::ProgressTracker::new()),
            CancellationToken::new(),
        )
    }

    #[tokio::test]
    async fn sync_file_uploads_and_inserts_db() {
        let (dir, cfg, db, mock, cache, ignore, tracker, sd) = setup_test().await;
        let provider: Arc<dyn RemoteProvider> = mock.clone();
        let file_path = dir.path().join("test.txt");
        tokio::fs::write(&file_path, "hello bella!").await.unwrap();

        let task = Task::SyncFile {
            path: file_path.clone(),
        };
        handle(task, &cfg, &db, &provider, &cache, &ignore, tracker, &sd,false)
            .await
            .unwrap();

        assert_eq!(mock.uploads.load(Ordering::Relaxed), 1);
        assert!(db.get("test.txt").unwrap().is_some());
    }

    #[tokio::test]
    async fn sync_file_skips_unchanged() {
        let (dir, cfg, db, mock, cache, ignore, tracker, sd) = setup_test().await;
        let provider: Arc<dyn RemoteProvider> = mock.clone();
        let file_path = dir.path().join("unchanged.txt");
        tokio::fs::write(&file_path, "same content").await.unwrap();

        let task1 = Task::SyncFile {
            path: file_path.clone(),
        };
        handle(
            task1,
            &cfg,
            &db,
            &provider,
            &cache,
            &ignore,
            tracker.clone(),
            &sd,
            false
        )
        .await
        .unwrap();
        assert_eq!(mock.uploads.load(Ordering::Relaxed), 1);

        let task2 = Task::SyncFile {
            path: file_path.clone(),
        };
        handle(task2, &cfg, &db, &provider, &cache, &ignore, tracker, &sd,false)
            .await
            .unwrap();
        assert_eq!(mock.uploads.load(Ordering::Relaxed), 1);
    }
}
