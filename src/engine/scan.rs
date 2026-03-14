use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::info;
use walkdir::WalkDir;

use crate::config::AppConfig;
use crate::db::{Database, FileEntry};
use crate::engine::{is_shutdown_err, EngineStatus, ScanPhase, Task};
use crate::ignore::IgnoreMatcher;
use crate::kio::{KioOps, to_remote};
use crate::notif;

/// Scan initial en 6 phases :
/// 0. Listing récursif du remote (BFS) → remote_index.
/// 1. Inventaire complet du filesystem local (dossiers + fichiers).
/// 2. Crée les dossiers distants manquants (dir_index DB + remote_index).
/// 3. Compare chaque fichier avec la DB (mtime+hash). N'enqueue que les différences.
/// 4. Enqueue les fichiers à synchroniser.
/// 5. Supprime les orphelins DB (en DB mais absents localement → delete remote).
/// 6. Supprime les orphelins remote (sur le Drive mais absents localement et de la DB).
///
/// La DB est la source de persistance : les fichiers déjà synchronisés avec le
/// même hash sont ignorés. L'ordi local est la source de vérité.
pub(crate) async fn run<K: KioOps>(
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
        phase: ScanPhase::RemoteListing, done: 0, total: 0, current: "listing remote…".into(),
    });

    let t0 = std::time::Instant::now();
    let remote_index = match kio.ls_remote(&cfg.remote_root).await {
        Ok(idx) => {
            info!(count = idx.len(), elapsed_ms = t0.elapsed().as_millis(), "scan: remote index built");
            idx
        }
        Err(e) => {
            // Si le dossier distant n'existe pas encore, on part d'un index vide.
            tracing::warn!(error = %e, elapsed_ms = t0.elapsed().as_millis(), "scan: remote listing failed, assuming empty");
            HashSet::new()
        }
    };

    // ── Phase 1 : inventaire filesystem local ─────────────────────────────────
    let _ = status_tx.send(EngineStatus::ScanProgress {
        phase: ScanPhase::LocalListing, done: 0, total: 0, current: "inventaire local…".into(),
    });

    let mut local_dirs: Vec<PathBuf> = Vec::new();
    let mut local_files: Vec<PathBuf> = Vec::new();
    let mut local_count = 0usize;

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

        // Mise à jour progressive du tooltip (§7B UX_SYSTRAY.md)
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
    info!(dirs = total_dirs, files = total_files, "scan: inventaire terminé");

    // ── Phase 2 : dossiers distants (utilise le remote_index + dir_index DB) ───
    // known_remote : cache de TOUS les chemins-dossiers connus (BFS remote + DB
    // persistée + créés pendant ce scan). Empêche les doublons GDrive.
    let mut known_remote = remote_index;

    // Charger les dossiers déjà connus de la DB (persisté entre les runs).
    // Cela évite les stat + mkdir pour les dossiers déjà créés lors d'un précédent scan.
    let remote_prefix = format!("{}/", cfg.remote_root.trim_end_matches('/'));
    let db_clone = db.clone();
    let db_dirs = tokio::task::spawn_blocking(move || db_clone.all_dir_paths())
        .await
        .context("spawn_blocking panicked")??;
    let mut dirs_from_db = 0usize;
    for rel_path in &db_dirs {
        let full = format!("{}{rel_path}", remote_prefix);
        if known_remote.insert(full) {
            dirs_from_db += 1;
        }
    }
    if dirs_from_db > 0 {
        info!(dirs_from_db, "scan: dossiers chargés depuis la DB (skip réseau)");
    }

    let t2 = std::time::Instant::now();
    let mut dirs_cached  = 0usize;   // trouvé dans known_remote (BFS ou DB) → 0 appel réseau
    let mut dirs_verified = 0usize;  // absent de known_remote → stat réseau (existait déjà)

    // Collecte de TOUS les composants-dossiers rencontrés (pour persistance DB).
    // Utilise un HashSet pour dédupliquer (même composant vu pour plusieurs sous-dossiers).
    let mut all_dir_components: HashSet<String> = HashSet::new();

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

        if i % 10 == 0 || i + 1 == total_dirs {
            notif::scan_dirs_progress(cfg, i + 1, total_dirs);
        }

        // Crée chaque composant manquant du chemin vers ce dossier.
        let mut current = cfg.remote_root.trim_end_matches('/').to_string();
        for component in rel.components() {
            let part = component.as_os_str().to_string_lossy();
            current = format!("{current}/{part}");

            // Collecter le chemin relatif de ce composant-dossier (pour persist DB).
            if let Some(rel_dir) = current.strip_prefix(&remote_prefix) {
                all_dir_components.insert(rel_dir.to_string());
            }

            // Déjà connu (BFS remote OU DB OU créé plus tôt dans ce scan) → skip total.
            if known_remote.contains(&current) {
                dirs_cached += 1;
                continue;
            }

            // Pas dans le cache → appel réseau nécessaire (stat/mkdir).
            let cur = current.clone();
            retry(cfg, shutdown, "mkdir", || {
                let kio = kio.clone();
                let cur = cur.clone();
                async move { kio.mkdir_if_absent(&cur, &HashSet::new()).await }
            }).await?;

            // mkdir_if_absent a fait un stat. Si le dossier existait déjà,
            // il a juste été vérifié (pas de mkdir). On ne peut pas distinguer
            // stat-OK de mkdir-OK depuis ici, mais le dossier est confirmé.
            dirs_verified += 1;
            // Enregistrer immédiatement pour ne jamais re-vérifier ce dossier.
            known_remote.insert(current.clone());
        }
    }

    // Persister TOUS les composants-dossiers dans la DB (INSERT OR IGNORE = idempotent).
    // Inclut les dossiers du BFS, de la DB, et les nouveaux.
    // Au prochain run, ils seront chargés dans known_remote → 0 appel réseau.
    {
        let new_for_db: Vec<String> = all_dir_components.into_iter()
            .filter(|d| !db_dirs.contains(d.as_str()))
            .collect();
        if !new_for_db.is_empty() {
            let count = new_for_db.len();
            let db_clone = db.clone();
            tokio::task::spawn_blocking(move || db_clone.insert_dirs_batch(&new_for_db))
                .await
                .context("spawn_blocking panicked")??;
            info!(count, "scan: dossiers enregistrés en DB");
        }
    }

    info!(
        dirs = total_dirs, cached = dirs_cached, verified = dirs_verified,
        from_db = dirs_from_db,
        elapsed_ms = t2.elapsed().as_millis(),
        "scan: dossiers OK"
    );

    // Index final : inclut les chemins du BFS initial + dossiers créés pendant ce scan.
    // Partagé par Arc entre toutes les tâches pour éviter un `stat` par fichier dans les workers.
    let remote_index = Arc::new(known_remote);

    // ── Phase 3 : comparaison fichiers local ↔ DB ─────────────────────────────
    let _ = status_tx.send(EngineStatus::ScanProgress {
        phase: ScanPhase::Comparing, done: 0, total: total_files, current: "comparaison…".into(),
    });

    // spawn_blocking : all_paths() lit potentiellement des milliers de lignes SQLite.
    // Ne pas bloquer le thread Tokio pendant cette opération.
    let db_clone = db.clone();
    let db_index = tokio::task::spawn_blocking(move || db_clone.all_paths())
        .await
        .context("spawn_blocking panicked")??;
    let mut to_sync: Vec<PathBuf> = Vec::new();
    let mut skipped = 0usize;
    let mut skipped_remote = 0usize;

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
        } else if let Ok(remote_url) = to_remote(&cfg.remote_root, &cfg.local_root, file_path) {
            // Fichier absent de la DB locale — vérifier s'il existe déjà sur le remote.
            // Si oui, il a été synchronisé lors d'un précédent run (DB perdue ou
            // premier relancement). On l'enregistre dans la DB avec son hash+mtime
            // actuel pour éviter un re-upload inutile.
            if remote_index.contains(&remote_url) {
                if let Ok(hash) = hash_of(file_path).await {
                    let mtime = mtime_of(file_path);
                    let _ = db.upsert(&FileEntry { path: rel, hash, mtime });
                }
                skipped_remote += 1;
                continue;
            }
        }

        to_sync.push(file_path.clone());
    }

    // Retirer les fichiers vides (0 octet) — kioclient5 ne les gère pas correctement.
    to_sync.retain(|p| std::fs::metadata(p).map(|m| m.len() > 0).unwrap_or(false));

    info!(
        to_sync = to_sync.len(), skipped, skipped_remote,
        "scan: comparaison terminée"
    );
    notif::scan_complete(cfg, total_dirs, to_sync.len(), skipped + skipped_remote);

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

        if task_tx.send(Task::SyncFile { path, remote_index: Some(remote_index.clone()) }).await.is_err() {
            anyhow::bail!("shutdown: task queue closed");
        }
    }

    if sync_total > 0 {
        info!(files = sync_total, "scan: fichiers en queue");
    }

    // ── Ensemble de référence : tous les chemins relatifs locaux ─────────────
    // Utilisé par Phase 5 (orphelins DB) et Phase 6 (orphelins remote).
    let local_rel_set: HashSet<String> = local_files.iter()
        .filter_map(|p| p.strip_prefix(&cfg.local_root).ok())
        .map(|r| r.to_string_lossy().to_string())
        .collect();

    // ── Phase 5 : suppression des orphelins DB ──────────────────────────────
    // Fichiers dans la DB (= déjà synchronisés) mais supprimés localement.
    // Couvre les cas où l'événement inotify Remove a été raté (pause, restart,
    // overflow du channel watcher).
    {
        let mut orphans_db = 0usize;
        for db_path in &db_index {
            if shutdown.is_cancelled() {
                anyhow::bail!("shutdown: scan interrupted");
            }
            if !local_rel_set.contains(db_path) {
                let full_local = cfg.local_root.join(db_path);
                if task_tx.send(Task::Delete(full_local)).await.is_err() {
                    anyhow::bail!("shutdown: task queue closed");
                }
                orphans_db += 1;
            }
        }
        if orphans_db > 0 {
            info!(orphans_db, "scan: orphelins DB (supprimés localement) → suppression remote");
        }
    }

    // ── Phase 6 : suppression des orphelins remote ──────────────────────────
    // Fichiers présents sur le remote mais absents localement ET absents de la DB.
    // Cas typiques : upload manuel sur le Drive, DB perdue/réinitialisée,
    // ou résidu d'un précédent run interrompu.
    // Garantit l'égalité : local = DB = remote.
    {
        let remote_prefix = format!("{}/", cfg.remote_root.trim_end_matches('/'));
        let mut orphans_remote = 0usize;

        for remote_path in remote_index.iter() {
            if shutdown.is_cancelled() {
                anyhow::bail!("shutdown: scan interrupted");
            }
            // Ignorer les dossiers (trailing slash dans le BFS).
            if remote_path.ends_with('/') { continue; }

            // Extraire le chemin relatif.
            let rel = match remote_path.strip_prefix(&remote_prefix) {
                Some(r) if !r.is_empty() => r,
                _ => continue,
            };

            // Si le fichier existe localement → rien à faire (Phase 3/4 gèrent).
            if local_rel_set.contains(rel) { continue; }

            // Si c'est un dossier local connu → pas un fichier orphelin.
            let local_path = cfg.local_root.join(rel);
            if local_path.is_dir() { continue; }

            // Si le fichier est dans la DB, Phase 5 gère déjà → skip.
            if db_index.contains(rel) { continue; }

            // Si le chemin matche un pattern d'exclusion → ne pas toucher.
            if ignore.is_ignored(&local_path) { continue; }

            // Fichier remote orphelin → supprimer.
            if task_tx.send(Task::Delete(local_path)).await.is_err() {
                anyhow::bail!("shutdown: task queue closed");
            }
            orphans_remote += 1;
        }
        if orphans_remote > 0 {
            info!(orphans_remote, "scan: orphelins remote (absents localement) → suppression remote");
        }
    }

    Ok(())
}


// ── Helpers ───────────────────────────────────────────────────────────────────

fn mtime_of(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).map_err(|e| {
            std::io::Error::other(e)
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
            || s.contains("quota")
            || s.contains("insufficient storage")
            || s.contains("espace insuffisant")
            || s.contains("storage full")
            || s.contains("disk full")
            || s.contains("no space left")
    })
}

/// Détecte si l'erreur est liée à un dépassement de quota/espace.
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
            "jetons d'accès manquants pour le compte",
            "access token expired",
            "authentication required",
            "authentification requise",
            "permission denied",
            "unauthorized",
            "Error 403: forbidden",
            "Error 401: not authorized",
        ];
        for msg in cases {
            let e = anyhow::anyhow!("{msg}");
            assert!(is_fatal_kio_err(&e), "should be fatal: {msg}");
        }
    }

    #[test]
    fn fatal_quota_errors() {
        let cases = vec![
            "quota exceeded",
            "insufficient storage",
            "espace insuffisant",
            "storage full",
            "disk full",
            "no space left on device",
        ];
        for msg in &cases {
            let e = anyhow::anyhow!("{msg}");
            assert!(is_fatal_kio_err(&e), "should be fatal: {msg}");
            assert!(is_quota_err(&e), "should be quota: {msg}");
        }
    }

    #[test]
    fn non_fatal_errors() {
        let cases = vec![
            "connection timed out",
            "network unreachable",
            "kioclient5 returned exit=1",
            "copy to remote failed",
        ];
        for msg in cases {
            let e = anyhow::anyhow!("{msg}");
            assert!(!is_fatal_kio_err(&e), "should NOT be fatal: {msg}");
            assert!(!is_quota_err(&e), "should NOT be quota: {msg}");
        }
    }

    #[test]
    fn chained_error_detection() {
        let inner = anyhow::anyhow!("access token expired");
        let outer = anyhow::anyhow!(inner).context("copy failed");
        assert!(is_fatal_kio_err(&outer));
    }

    #[test]
    fn quota_is_subset_of_fatal() {
        // Toute erreur quota est aussi fatale
        let e = anyhow::anyhow!("insufficient storage");
        assert!(is_quota_err(&e));
        assert!(is_fatal_kio_err(&e));
    }

    #[test]
    fn case_insensitive_detection() {
        let e = anyhow::anyhow!("QUOTA EXCEEDED");
        assert!(is_quota_err(&e));
        let e2 = anyhow::anyhow!("Permission Denied");
        assert!(is_fatal_kio_err(&e2));
    }
}

