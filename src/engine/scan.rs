//! Moteur d'analyse (Scanner) pour SyncGDrive.
//!
//! Ce module est responsable de l'inventaire et de la comparaison bidirectionnelle.
//! Il confronte l'état du disque local, l'index distant (Google Drive) et le cache
//! SQLite interne pour générer l'ensemble des opérations requises (le "diff").

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use md5::{Digest, Md5};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::info;
use walkdir::WalkDir;

use crate::config::AppConfig;
use crate::db::{FileEntry};
use crate::engine::{EngineContext, EngineStatus, ScanPhase, Task};
use crate::ignore::IgnoreMatcher;
use crate::notif;

/// Exécute l'analyse complète (scan) du système de fichiers local et distant.
///
/// Cette fonction asynchrone est le chef d'orchestre de la synchronisation. Elle opère en 6 phases :
/// 1. **Remote Listing** : Récupère l'index de Google Drive.
/// 2. **Local Listing** : Parcourt le dossier local (en ignorant les fichiers exclus).
/// 3. **Directories** : Recrée l'arborescence des dossiers manquants sur le cloud.
/// 4. **Comparing** : Vérifie les hashs MD5 et MTime pour déterminer ce qui doit être uploadé.
/// 5. **Orphans DB** : Nettoie la base de données des fichiers supprimés localement.
/// 6. **Orphans Remote** : Ordonne la suppression sur le Drive des fichiers supprimés localement.
///
/// Si `ctx.dry_run` est actif, les tâches ne sont pas envoyées et un bilan est imprimé.
pub(crate) async fn run(
    ctx: &EngineContext,
    ignore: &IgnoreMatcher,
    task_tx: &mpsc::Sender<Task>,
    status_tx: &mpsc::UnboundedSender<EngineStatus>,
    ) -> Result<()> {
    // ── Décomposition du contexte ────────────────────────────────────────────────
    let cfg = ctx.cfg.clone();
    let provider = ctx.provider.clone();
    let db = ctx.db.clone();
    let path_cache = ctx.path_cache.clone();
    let shutdown = ctx.shutdown.clone();
    let tracker = ctx.tracker.clone();
    let dry_run = ctx.dry_run;

    let primary = cfg.get_primary_pair().context("Aucun dossier")?;
    info!(root = %primary.local_path.display(), "scan: start");
    notif::scan_started(&cfg);

    // ── Phase 0 : listing récursif du remote (BFS GDrive) ──────────────────
    let _ = status_tx.send(EngineStatus::ScanProgress {
        phase: ScanPhase::RemoteListing,
        done: 0,
        total: 0,
        current: "listing remote…".into(),
    });

    let t0 = std::time::Instant::now();
    let remote_index = match provider.list_remote(&primary.remote_folder_id).await {
        Ok(idx) => {
            info!(
                count = idx.files.len() + idx.dirs.len(),
                elapsed_ms = t0.elapsed().as_millis(),
                "scan: remote index built"
            );
            idx
        }
        Err(e) => {
            tracing::warn!(error = %e, elapsed_ms = t0.elapsed().as_millis(), "scan: remote listing failed, assuming empty");
            crate::remote::RemoteIndex {
                files: vec![],
                dirs: vec![],
            }
        }
    };

    // On peuple le cache global immédiatement
    for dir in &remote_index.dirs {
        path_cache
            .insert(&dir.relative_path, &dir.drive_id, &dir.parent_id)
            .await;
    }
    for file in &remote_index.files {
        path_cache
            .insert(&file.relative_path, &file.drive_id, &file.parent_id)
            .await;
    }

    // ── Phase 1 : inventaire filesystem local ─────────────────────────────────
    let _ = status_tx.send(EngineStatus::ScanProgress {
        phase: ScanPhase::LocalListing,
        done: 0,
        total: 0,
        current: "inventaire local…".into(),
    });

    let mut local_dirs: Vec<PathBuf> = Vec::new();
    let mut local_files: Vec<PathBuf> = Vec::new();
    let mut local_count = 0usize;

    for entry in WalkDir::new(&primary.local_path)
        .into_iter()
        .filter_entry(|e| !ignore.is_ignored(e.path()))
        .filter_map(|e| e.ok())
    {
        tokio::task::yield_now().await;

        if shutdown.is_cancelled() {
            anyhow::bail!("shutdown: scan interrupted");
        }
        let path = entry.path().to_path_buf();
        if entry.file_type().is_dir() {
            if let Ok(r) = path.strip_prefix(&primary.local_path) {
                if !r.as_os_str().is_empty() {
                    local_dirs.push(path);
                }
            }
        } else if entry.file_type().is_file() {
            local_files.push(path);
        }

        local_count += 1;
        if local_count.is_multiple_of(100) {
            let _ = status_tx.send(EngineStatus::ScanProgress {
                phase: ScanPhase::LocalListing,
                done: local_count,
                total: 0,
                current: format!("{local_count} éléments indexés"),
            });
        }
    }

    let total_dirs = local_dirs.len();
    let total_files = local_files.len();
    info!(
        dirs = total_dirs,
        files = total_files,
        "scan: inventaire terminé"
    );

    // ── Phase 2 : création des dossiers distants manquants ────────────────────
    let t2 = std::time::Instant::now();
    let mut dirs_verified = 0usize;
    let mut dirs_created = 0usize;

    for (i, dir_path) in local_dirs.iter().enumerate() {
        if shutdown.is_cancelled() {
            anyhow::bail!("shutdown: scan interrupted");
        }
        let rel = dir_path.strip_prefix(&primary.local_path)?;
        let rel_str = rel.to_string_lossy().to_string();

        let dir_name = rel
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        let _ = status_tx.send(EngineStatus::ScanProgress {
            phase: ScanPhase::Directories,
            done: i + 1,
            total: total_dirs,
            current: dir_name,
        });

        if i.is_multiple_of(10) || i + 1 == total_dirs {
            notif::scan_dirs_progress(&cfg, i + 1, total_dirs);
        }

        if path_cache.lookup(&rel_str).await.is_some() {
            dirs_verified += 1;
            continue;
        }

        let mut current_rel = String::new();
        let mut current_parent = primary.remote_folder_id.clone();

        for comp in rel.components() {
            let part = comp.as_os_str().to_string_lossy().to_string();
            if !current_rel.is_empty() {
                current_rel.push('/');
            }
            current_rel.push_str(&part);

            if let Some(entry) = path_cache.lookup(&current_rel).await {
                current_parent = entry.drive_id.clone();
            } else {
                let new_id = if dry_run {
                    info!("[DRY-RUN] mkdir: {}", current_rel);
                    format!("dry_run_dir_{}", current_rel.replace('/', "_"))
                } else {
                    let p = Arc::clone(&provider);
                    let cur_parent_clone = current_parent.clone();
                    let part_clone = part.clone();

                    retry(&cfg, &shutdown, "mkdir", || {
                        let p_inner = Arc::clone(&p);
                        let parent = cur_parent_clone.clone();
                        let pt = part_clone.clone();
                        async move { p_inner.mkdir(&parent, &pt).await }
                    })
                        .await?
                };

                path_cache
                    .insert(&current_rel, &new_id, &current_parent)
                    .await;
                current_parent = new_id;
                dirs_created += 1;
            }
        }
    }

    info!(
        dirs = total_dirs,
        verified = dirs_verified,
        created = dirs_created,
        elapsed_ms = t2.elapsed().as_millis(),
        "scan: dossiers OK"
    );

    let mut orphans_db = 0;
    let mut orphans_remote = 0;

    // ── Phase 3 : comparaison fichiers local ↔ DB ─────────────────────────────
    let _ = status_tx.send(EngineStatus::ScanProgress {
        phase: ScanPhase::Comparing,
        done: 0,
        total: total_files,
        current: "comparaison…".into(),
    });

    let db_clone = db.clone();
    let db_index = tokio::task::spawn_blocking(move || db_clone.all_paths())
        .await
        .context("spawn_blocking panicked")??;

    let mut to_sync: Vec<PathBuf> = Vec::new();
    let mut skipped = 0usize;
    let mut skipped_remote = 0usize;

    for (i, file_path) in local_files.iter().enumerate() {
        tokio::task::yield_now().await;

        if shutdown.is_cancelled() {
            anyhow::bail!("shutdown: scan interrupted");
        }

        let rel = match file_path.strip_prefix(&primary.local_path) {
            Ok(r) => r.to_string_lossy().to_string(),
            Err(_) => continue,
        };

        let file_name = file_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        if i.is_multiple_of(50) || i + 1 == total_files {
            let _ = status_tx.send(EngineStatus::ScanProgress {
                phase: ScanPhase::Comparing,
                done: i + 1,
                total: total_files,
                current: file_name.clone(),
            });
        }

        let remote_exists = path_cache.lookup(&rel).await.is_some();

        if db_index.contains(&rel) && remote_exists {
            if let Ok(Some(entry)) = db.get(&rel) {
                let mtime = mtime_of(file_path);
                if mtime == entry.mtime {
                    skipped += 1;
                    continue;
                }
                if let Ok(hash) = hash_of(file_path).await {
                    if hash == entry.hash {
                        let _ = db.upsert(&FileEntry {
                            path: rel,
                            hash,
                            mtime,
                        });
                        skipped += 1;
                        continue;
                    }
                }
            }
        } else if remote_exists {
            if let Ok(hash) = hash_of(file_path).await {
                let mtime = mtime_of(file_path);
                let _ = db.upsert(&FileEntry {
                    path: rel,
                    hash,
                    mtime,
                });
            }
            skipped_remote += 1;
            continue;
        }

        to_sync.push(file_path.clone());
    }

    to_sync.retain(|p| std::fs::metadata(p).map(|m| m.len() > 0).unwrap_or(false));

    info!(
        to_sync = to_sync.len(),
        skipped, skipped_remote, "scan: comparaison terminée"
    );
    notif::scan_complete(&cfg, total_dirs, to_sync.len(), skipped + skipped_remote);

    let total_to_sync_bytes: u64 = to_sync
        .iter()
        .map(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0))
        .sum();

    if !dry_run{
        tracker.total_files.store(to_sync.len(), Ordering::Relaxed);
        tracker
            .total_bytes
            .store(total_to_sync_bytes, Ordering::Relaxed);
    }


    // ── Phase 4 : enqueue les fichiers à synchroniser ─────────────────────────
    let sync_total = to_sync.len();
    for (i, path) in to_sync.into_iter().enumerate() {
        if shutdown.is_cancelled() {
            anyhow::bail!("shutdown: scan interrupted");
        }

        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);

        let _ = status_tx.send(EngineStatus::ScanProgress {
            phase: ScanPhase::Comparing,
            done: i + 1,
            total: sync_total,
            current: file_name.clone(),
        });

        if i % 25 == 0 || i + 1 == sync_total {
            notif::sync_progress(&cfg, i + 1, sync_total, &file_name, size);
        }

        // 🌟 SÉCURITÉ DRY-RUN : On bloque l'envoi au canal
        if dry_run {
            info!("[DRY-RUN] 📤 À uploader : {}", path.display());
        } else if task_tx.send(Task::SyncFile { path }).await.is_err() {
            anyhow::bail!("shutdown: task queue closed");
        }
    }

    if sync_total > 0 {
        info!(files = sync_total, "scan: fichiers en queue");
    }

    let local_rel_set: HashSet<String> = local_files
        .iter()
        .filter_map(|p| p.strip_prefix(&primary.local_path).ok())
        .map(|r| r.to_string_lossy().to_string())
        .collect();

    // ── Phase 5 : suppression des orphelins DB ──────────────────────────────
    {
        for db_path in &db_index {
            if shutdown.is_cancelled() {
                anyhow::bail!("shutdown: scan interrupted");
            }
            if !local_rel_set.contains(db_path) {
                let full_local = primary.local_path.join(db_path);
                if !dry_run {
                    tracker.total_files.fetch_add(1, Ordering::Relaxed);
                }
                // 🌟 SÉCURITÉ DRY-RUN : On bloque l'envoi au canal
                if dry_run {
                    info!("[DRY-RUN] 🗑️ À supprimer (Local DB) : {}", full_local.display());
                } else if task_tx.send(Task::Delete(full_local)).await.is_err() {
                    anyhow::bail!("shutdown: task queue closed");
                }
                orphans_db += 1;
            }
        }
        if orphans_db > 0 {
            info!(
                orphans_db,
                "scan: orphelins DB (supprimés localement) → suppression remote"
            );
        }
    }

    // ── Phase 6 : suppression des orphelins remote ──────────────────────────
    {
        for remote_file in &remote_index.files {
            if shutdown.is_cancelled() {
                anyhow::bail!("shutdown: scan interrupted");
            }
            let rel = &remote_file.relative_path;

            if local_rel_set.contains(rel) {
                continue;
            }

            let local_path = primary.local_path.join(rel);
            if local_path.is_dir() {
                continue;
            }
            if db_index.contains(rel) {
                continue;
            }
            if ignore.is_ignored(&local_path) {
                continue;
            }

            if !dry_run {
                tracker.total_files.fetch_add(1, Ordering::Relaxed);
            }

            // 🌟 SÉCURITÉ DRY-RUN : On bloque l'envoi au canal
            if dry_run {
               info!("[DRY-RUN] 🗑️ À supprimer (Remote) : {}", local_path.display());
            } else if task_tx.send(Task::Delete(local_path)).await.is_err() {
                anyhow::bail!("shutdown: task queue closed");
            }
            orphans_remote += 1;
        }
        if orphans_remote > 0 {
            info!(
                orphans_remote,
                "scan: orphelins remote (absents localement) → suppression remote"
            );
        }
    }

    // ── Résumé final (Garanti d'apparaître en tout dernier) ─────────────────
    if dry_run {
        let total_ops = sync_total + dirs_created + orphans_db + orphans_remote;
        tracing::warn!("=== DRY-RUN SUMMARY ===");
        tracing::warn!("Files to upload:  {} ({} octets)", sync_total, total_to_sync_bytes);
        tracing::warn!("Dirs to create:   {}", dirs_created);
        tracing::warn!("Files to delete:  {}", orphans_db + orphans_remote);
        tracing::warn!("Total operations: {}", total_ops);
        tracing::warn!("=======================");

        let _ = status_tx.send(EngineStatus::Idle);
    }

    Ok(())
}
// ── Helpers ───────────────────────────────────────────────────────────────────

/// Récupère la date de modification d'un fichier en timestamp Unix.
fn mtime_of(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .and_then(|t| {
            t.duration_since(std::time::UNIX_EPOCH)
                .map_err(std::io::Error::other)
        })
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Calcule l'empreinte MD5 d'un fichier local.
async fn hash_of(path: &Path) -> Result<String> {
    let data = tokio::fs::read(path).await?;
    let mut h = Md5::new(); // 🌟 CORRECTION: Sha256 remplacé par Md5
    h.update(&data);
    Ok(format!("{:x}", h.finalize()))
}

// ── Retry avec backoff exponentiel interruptible ──────────────────────────────

/// Exécute une opération asynchrone (closure) avec un mécanisme de tentatives répétées.
///
/// Le délai entre deux tentatives augmente exponentiellement (backoff).
/// La boucle écoute le jeton d'interruption `shutdown` pour annuler la pause immédiatement.
pub async fn retry<T, F, Fut>(
    cfg: &AppConfig,
    shutdown: &CancellationToken,
    name: &str,
    mut f: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut attempt = 1;
    let max_attempts = cfg.retry.max_attempts;
    let backoff = cfg.retry.initial_backoff_ms;

    loop {
        if shutdown.is_cancelled() {
            anyhow::bail!("Annulé");
        }

        match f().await {
            Ok(val) => return Ok(val),
            Err(e) => {
                if attempt >= max_attempts {
                    return Err(e);
                }
                tracing::warn!(
                    "{} a échoué (essai {}/{}) : {}, nouvelle tentative dans {}ms",
                    name,
                    attempt,
                    max_attempts,
                    e,
                    backoff
                );
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_millis(backoff)) => {}
                    _ = shutdown.cancelled() => anyhow::bail!("Annulé proprement pendant le retry"),
                }
                attempt += 1;
            }
        }
    }
}

/// Détermine si une erreur API doit couper le moteur (Fatal).
/// Typiquement les erreurs d'authentification (jeton expiré, 403, 401).
pub fn is_fatal_remote_err(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        let s = c.to_string().to_lowercase();
        s.contains("jetons d'accès")
            || s.contains("access token")
            || s.contains("token expired")
            || s.contains("authentication required")
            || s.contains("authentification requise")
            || s.contains("permission denied")
            || s.contains("unauthorized")
            || s.contains("403")
            || s.contains("401")
            || s.contains("quota")
            || s.contains("insufficient storage")
            || s.contains("espace insuffisant")
            || s.contains("storage full")
            || s.contains("disk full")
            || s.contains("no space left")
    })
}

// Détermine si une erreur correspond spécifiquement à un dépassement de quota (Espace saturé).
pub fn is_quota_err(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        let s = c.to_string().to_lowercase();
        s.contains("quota")
            || s.contains("insufficient storage")
            || s.contains("espace insuffisant")
            || s.contains("storage full")
            || s.contains("disk full")
            || s.contains("no space left")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fatal_auth_errors() {
        let cases = vec![
            "jetons d'accès",
            "access token expired",
            "Error 403: forbidden",
            "Error 401: not authorized",
        ];
        for msg in cases {
            let e = anyhow::anyhow!("{msg}");
            assert!(is_fatal_remote_err(&e), "should be fatal: {msg}");
        }
    }

    #[test]
    fn fatal_quota_errors() {
        let cases = vec![
            "quota exceeded",
            "insufficient storage",
            "espace insuffisant",
            "disk full",
        ];
        for msg in &cases {
            let e = anyhow::anyhow!("{msg}");
            assert!(is_fatal_remote_err(&e), "should be fatal: {msg}");
            assert!(is_quota_err(&e), "should be quota: {msg}");
        }
    }

    #[test]
    fn non_fatal_errors() {
        let cases = vec!["connection timed out", "network unreachable", "copy failed"];
        for msg in cases {
            let e = anyhow::anyhow!("{msg}");
            assert!(!is_fatal_remote_err(&e), "should NOT be fatal: {msg}");
            assert!(!is_quota_err(&e), "should NOT be quota: {msg}");
        }
    }
}
