use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::info;
use walkdir::WalkDir;

use crate::config::AppConfig;
use crate::db::{Database, FileEntry};
use crate::engine::{is_shutdown_err, EngineStatus, ScanPhase, Task};
use crate::ignore::IgnoreMatcher;
use crate::kio::KioOps;
use crate::notif;

/// Scan initial en 3 phases :
/// 1. Inventaire complet du filesystem (dossiers + fichiers).
/// 2. Crée les dossiers distants manquants.
/// 3. Compare chaque fichier avec la DB (mtime+hash). N'enqueue que les différences.
///
/// La DB est la source de persistance : les fichiers déjà synchronisés avec le
/// même hash sont ignorés. L'ordi local est la source de vérité.
pub async fn run<K: KioOps>(
    cfg: &AppConfig,
    db: &Database,
    ignore: &IgnoreMatcher,
    kio: &K,
    task_tx: &mpsc::Sender<Task>,
    shutdown: &CancellationToken,
    status_tx: &mpsc::UnboundedSender<EngineStatus>,
) -> Result<()> {
    info!(root = %cfg.local_root.display(), "scan: start");
    notif::scan_started(cfg);

    // ── Phase 0 : listing récursif du remote (un seul ls par dossier) ─────────
    let _ = status_tx.send(EngineStatus::ScanProgress {
        phase: ScanPhase::Listing, done: 0, total: 0, current: "listing remote…".into(),
    });

    let remote_index = match kio.ls_remote(&cfg.remote_root).await {
        Ok(idx) => {
            info!(count = idx.len(), "scan: remote index built");
            idx
        }
        Err(e) => {
            // Si le dossier distant n'existe pas encore, on part d'un index vide.
            tracing::warn!(error = %e, "scan: remote listing failed, assuming empty");
            HashSet::new()
        }
    };

    // ── Phase 1 : inventaire filesystem local ─────────────────────────────────
    let _ = status_tx.send(EngineStatus::ScanProgress {
        phase: ScanPhase::Listing, done: 0, total: 0, current: "inventaire local…".into(),
    });

    let mut local_dirs: Vec<PathBuf> = Vec::new();
    let mut local_files: Vec<PathBuf> = Vec::new();

    for entry in WalkDir::new(&cfg.local_root)
        .into_iter()
        .filter_entry(|e| !ignore.is_ignored(e.path()))
        .filter_map(|e| e.ok())
    {
        if shutdown.is_cancelled() {
            anyhow::bail!("shutdown: scan interrupted");
        }
        let path = entry.path().to_path_buf();
        if entry.file_type().is_dir() {
            if let Ok(r) = path.strip_prefix(&cfg.local_root) {
                if !r.as_os_str().is_empty() {
                    local_dirs.push(path);
                }
            }
        } else if entry.file_type().is_file() {
            local_files.push(path);
        }
    }

    let total_dirs = local_dirs.len();
    let total_files = local_files.len();
    info!(dirs = total_dirs, files = total_files, "scan: inventaire terminé");

    // ── Phase 2 : dossiers distants (utilise le remote_index, pas de stat) ────
    for (i, dir_path) in local_dirs.iter().enumerate() {
        if shutdown.is_cancelled() {
            anyhow::bail!("shutdown: scan interrupted");
        }
        let rel = dir_path.strip_prefix(&cfg.local_root).unwrap();
        let dir_name = rel.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        let _ = status_tx.send(EngineStatus::ScanProgress {
            phase: ScanPhase::Directories,
            done: i + 1,
            total: total_dirs,
            current: dir_name,
        });

        // Crée chaque composant manquant du chemin vers ce dossier.
        let mut current = cfg.remote_root.trim_end_matches('/').to_string();
        for component in rel.components() {
            let part = component.as_os_str().to_string_lossy();
            current = format!("{current}/{part}");

            let ri = remote_index.clone();
            let cur = current.clone();
            retry(cfg, shutdown, "mkdir", || {
                let kio = kio.clone();
                let cur = cur.clone();
                let ri = ri.clone();
                async move { kio.mkdir_if_absent(&cur, &ri).await }
            }).await?;
        }
    }

    info!(dirs = total_dirs, "scan: dossiers OK");

    // ── Phase 3 : comparaison fichiers local ↔ DB ─────────────────────────────
    let _ = status_tx.send(EngineStatus::ScanProgress {
        phase: ScanPhase::Comparing, done: 0, total: total_files, current: "comparaison…".into(),
    });

    let db_index = db.all_paths()?;
    let mut to_sync: Vec<PathBuf> = Vec::new();
    let mut skipped = 0usize;

    for (i, file_path) in local_files.iter().enumerate() {
        if shutdown.is_cancelled() {
            anyhow::bail!("shutdown: scan interrupted");
        }

        let rel = match file_path.strip_prefix(&cfg.local_root) {
            Ok(r) => r.to_string_lossy().to_string(),
            Err(_) => continue,
        };

        let file_name = file_path.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        // Mise à jour de la progression toutes les 50 entrées (évite le spam)
        if i % 50 == 0 || i + 1 == total_files {
            let _ = status_tx.send(EngineStatus::ScanProgress {
                phase: ScanPhase::Comparing,
                done: i + 1,
                total: total_files,
                current: file_name.clone(),
            });
        }

        // Vérifie si le fichier est déjà indexé avec le bon mtime
        if db_index.contains(&rel) {
            if let Ok(Some(entry)) = db.get(&rel) {
                let mtime = mtime_of(file_path);
                if mtime == entry.mtime {
                    skipped += 1;
                    continue; // Pas modifié depuis la dernière synchro
                }
                // mtime différent → vérifier le hash
                if let Ok(hash) = hash_of(file_path).await {
                    if hash == entry.hash {
                        // Contenu identique, juste mtime changé → maj DB, pas d'upload
                        let _ = db.upsert(&FileEntry { path: rel, hash, mtime });
                        skipped += 1;
                        continue;
                    }
                }
            }
        }

        to_sync.push(file_path.clone());
    }

    info!(to_sync = to_sync.len(), skipped, "scan: comparaison terminée");
    notif::scan_complete(cfg, total_dirs, to_sync.len(), skipped);

    // ── Phase 4 : enqueue les fichiers à synchroniser ─────────────────────────
    let sync_total = to_sync.len();
    for (i, path) in to_sync.into_iter().enumerate() {
        if shutdown.is_cancelled() {
            anyhow::bail!("shutdown: scan interrupted");
        }

        let file_name = path.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);

        let _ = status_tx.send(EngineStatus::SyncProgress {
            done: i + 1,
            total: sync_total,
            current: file_name.clone(),
            size_bytes: size,
        });

        // Notification tous les 25 fichiers ou au premier/dernier
        if i % 25 == 0 || i + 1 == sync_total {
            notif::sync_progress(cfg, i + 1, sync_total, &file_name, size);
        }

        if task_tx.send(Task::SyncFile(path)).await.is_err() {
            anyhow::bail!("shutdown: task queue closed");
        }
    }

    if sync_total > 0 {
        info!(files = sync_total, "scan: fichiers en queue");
    }
    Ok(())
}


// ── Helpers ───────────────────────────────────────────────────────────────────

fn mtime_of(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::Other, e)
        }))
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

async fn hash_of(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let data = tokio::fs::read(path).await?;
    let mut h = Sha256::new();
    h.update(&data);
    Ok(format!("{:x}", h.finalize()))
}

// ── Retry avec backoff exponentiel interruptible ──────────────────────────────

pub async fn retry<F, Fut>(
    cfg: &AppConfig,
    shutdown: &CancellationToken,
    op_name: &str,
    mut op: F,
) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let max = cfg.retry.max_attempts.max(1);
    let mut backoff = Duration::from_millis(cfg.retry.initial_backoff_ms.max(50));
    let cap = Duration::from_millis(cfg.retry.max_backoff_ms.max(backoff.as_millis() as u64));

    for attempt in 1..=max {
        if shutdown.is_cancelled() {
            anyhow::bail!("shutdown: {op_name} aborted");
        }

        let result = tokio::select! {
            biased;
            _ = shutdown.cancelled() => anyhow::bail!("shutdown: {op_name} interrupted"),
            r = op() => r,
        };

        match result {
            Ok(()) => return Ok(()),
            Err(e) if is_shutdown_err(&e) => return Err(e),
            Err(e) if is_fatal_kio_err(&e) => {
                tracing::error!(op = op_name, error = %e, "erreur fatale KIO — abandon immédiat");
                return Err(e);
            }
            Err(e) if attempt == max => {
                anyhow::bail!("{op_name} failed after {max} attempts: {e}");
            }
            Err(e) => {
                tracing::warn!(op = op_name, attempt, max, backoff_ms = backoff.as_millis(), error = %e, "retrying");
                tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => anyhow::bail!("shutdown: {op_name} sleep interrupted"),
                    _ = tokio::time::sleep(backoff) => {}
                }
                backoff = (backoff * 2).min(cap);
            }
        }
    }

    Ok(())
}

/// Erreur non récupérable par un retry.
pub fn is_fatal_kio_err(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        let s = c.to_string().to_lowercase();
        s.contains("jetons d'accès")
            || s.contains("access token")
            || s.contains("token expired")
            || s.contains("manquants pour le compte")
            || s.contains("authentication required")
            || s.contains("authentification requise")
            || s.contains("permission denied")
            || s.contains("unauthorized")
            || s.contains("403")
            || s.contains("401")
    })
}
