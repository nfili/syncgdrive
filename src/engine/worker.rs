use std::path::Path;
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::config::AppConfig;
use crate::db::{Database, FileEntry};
use crate::engine::Task;
use crate::engine::scan::retry;
use crate::ignore::IgnoreMatcher;
use crate::kio::{to_remote, KioOps};

pub async fn handle<K: KioOps>(
    task: Task,
    cfg: &AppConfig,
    db: &Database,
    kio: &K,
    ignore: &IgnoreMatcher,
    shutdown: &CancellationToken,
) -> Result<()> {
    match task {
        Task::SyncFile(path)        => sync_file(&path, cfg, db, kio, ignore, shutdown).await,
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
) -> Result<()> {
    if ignore.is_ignored(path) { return Ok(()); }
    if !path.is_file()         { return Ok(()); }

    let rel = rel_str(&cfg.local_root, path)?;

    // ── Vérification mtime (rapide) ───────────────────────────────────────────
    let mtime = mtime(path)?;
    if let Some(e) = db.get(&rel)? {
        if e.mtime == mtime {
            return Ok(());          // pas changé
        }
    }

    // ── Vérification hash (plus coûteuse) ─────────────────────────────────────
    let hash = hash_file(path).await?;
    if let Some(e) = db.get(&rel)? {
        if e.hash == hash {
            db.upsert(&FileEntry { path: rel, hash, mtime })?;
            return Ok(());          // contenu identique, mise à jour mtime seule
        }
    }

    // ── Copie vers le remote ──────────────────────────────────────────────────
    let remote = to_remote(&cfg.remote_root, &cfg.local_root, path)?;

    retry(cfg, shutdown, "copy_file", || async {
        kio.copy_file(path, &remote).await
    }).await?;

    db.upsert(&FileEntry { path: rel.clone(), hash, mtime })?;
    info!(local = %path.display(), remote, "synced");
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
    info!(remote, "deleted");
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

    let from_remote = to_remote(&cfg.remote_root, &cfg.local_root, from)?;
    let to_remote   = to_remote(&cfg.remote_root, &cfg.local_root, to)?;

    retry(cfg, shutdown, "rename", || async {
        kio.rename(&from_remote, &to_remote).await
    }).await?;

    let from_rel = rel_str(&cfg.local_root, from).unwrap_or_default();
    let to_rel   = rel_str(&cfg.local_root, to).unwrap_or_default();
    if !from_rel.is_empty() && !to_rel.is_empty() {
        db.rename(&from_rel, &to_rel)?;
    }

    info!(from = from_remote, to = to_remote, "renamed");
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

