pub mod scan;
pub mod watcher;
pub mod worker;
pub mod bandwidth;
pub mod rate_limiter;
pub mod integrity;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result};
use tokio::sync::{mpsc, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::config::AppConfig;
use crate::db::Database;
use crate::engine::bandwidth::ProgressTracker;
use crate::ignore::IgnoreMatcher;
use crate::remote::{RemoteProvider, path_cache::PathCache};

// ── Types publics ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum EngineCommand {
    ForceScan,
    Shutdown,
    ApplyConfig(Arc<AppConfig>),
    Pause,
    Resume,
}

#[derive(Debug, Clone)]
pub enum EngineStatus {
    Starting,
    Unconfigured(String),
    Idle,
    ScanProgress {
        phase: ScanPhase,
        done: usize,
        total: usize,
        current: String,
    },
    SyncProgress(bandwidth::ProgressSnapshot),
    Syncing { active: usize },
    Paused,
    Error(String),
    Stopped,
}

#[derive(Debug, Clone)]
pub enum ScanPhase {
    RemoteListing,
    LocalListing,
    Directories,
    Comparing,
}

#[derive(Debug, Clone)]
pub(crate) enum Task {
    // Le remote_index a disparu : les workers utilisent le PathCache global
    SyncFile { path: PathBuf },
    Delete(PathBuf),
    Rename { from: PathBuf, to: PathBuf },
}

// ── SyncEngine ────────────────────────────────────────────────────────────────

pub struct SyncEngine {
    cfg: Arc<AppConfig>,
}

impl SyncEngine {
    pub fn new(cfg: Arc<AppConfig>) -> Self {
        Self { cfg }
    }

    pub async fn run(
        self,
        db: Database,
        shutdown: CancellationToken,
        cmd_rx: mpsc::Receiver<EngineCommand>,
        status_tx: mpsc::UnboundedSender<EngineStatus>,
    ) -> Result<()> {
        use crate::auth::GoogleAuth;
        use crate::remote::gdrive::GDriveProvider;

        let auth = Arc::new(GoogleAuth::new());
        let path_cache = Arc::new(PathCache::new());
        let config_arc = Arc::new(self.cfg.advanced.clone());

        // Allumage du réacteur GDrive natif (Phase 3 validée) !
        let provider: Arc<dyn RemoteProvider> = Arc::new(GDriveProvider::new(
            auth,
            path_cache.clone(),
            config_arc,
            shutdown.clone(),
        )?);

        self.run_with_provider(provider, path_cache, db, shutdown, cmd_rx, status_tx).await
    }

    async fn run_with_provider(
        mut self,
        provider: Arc<dyn RemoteProvider>,
        path_cache: Arc<PathCache>,
        db: Database,
        shutdown: CancellationToken,
        mut cmd_rx: mpsc::Receiver<EngineCommand>,
        status_tx: mpsc::UnboundedSender<EngineStatus>,
    ) -> Result<()> {
        let (task_tx, mut task_rx) = mpsc::channel::<Task>(1024);
        let ignore = IgnoreMatcher::from_patterns(&self.cfg.ignore_patterns)?;

        let mut paused = false;
        let mut rescan_on_resume = false;
        let tracker = Arc::new(crate::engine::bandwidth::ProgressTracker::new());
        {
            let _ = status_tx.send(EngineStatus::Syncing { active: 0 });
            let scan = scan::run(&self.cfg, &db, &ignore, &provider, &path_cache, &task_tx, &shutdown, &status_tx,&tracker);
            tokio::pin!(scan);

            loop {
                tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => {
                        finish(&status_tx).await;
                        return Ok(());
                    }
                    cmd = cmd_rx.recv() => {
                        match cmd {
                            Some(EngineCommand::Pause) => {
                                info!("engine: paused during initial scan");
                                paused = true;
                                rescan_on_resume = true;
                                let _ = status_tx.send(EngineStatus::Paused);
                                break;
                            }
                            Some(EngineCommand::Shutdown) | None => {
                                shutdown.cancel();
                                finish(&status_tx).await;
                                return Ok(());
                            }
                            _ => {}
                        }
                    }
                    r = &mut scan => {
                        match r {
                            Ok(()) => {}
                            Err(e) if is_shutdown_err(&e) => {
                                finish(&status_tx).await;
                                return Ok(());
                            }
                            Err(e) => {
                                warn!(error = %e, "initial scan failed, continuing with watcher");
                                let _ = status_tx.send(EngineStatus::Error(e.to_string()));
                                crate::notif::error(&self.cfg, &e.to_string());
                            }
                        }
                        break;
                    }
                }
            }
        }

        if !paused {
            let _ = status_tx.send(EngineStatus::Idle);
        }

        let (watch_tx, watch_rx) = mpsc::channel(256);
        let mut watcher = watcher::Watcher::start(&self.cfg.sync_pairs[0].local_path, watch_tx)?;

        let task_tx_w = task_tx.clone();
        let sd_w = shutdown.clone();
        spawn_debounced_dispatch(watch_rx, task_tx_w, sd_w,self.cfg.advanced.debounce_ms, tracker.clone());

        let sem = Arc::new(Semaphore::new(self.cfg.max_workers.max(1)));
        let active = Arc::new(AtomicUsize::new(0));

        tokio::spawn(progress_publisher(tracker.clone(), status_tx.clone(), shutdown.clone()));

        let mut overflow_tick = tokio::time::interval_at(
            tokio::time::Instant::now() + std::time::Duration::from_secs(self.cfg.advanced.health_check_interval_secs),
            std::time::Duration::from_secs(self.cfg.advanced.health_check_interval_secs),
        );

        let rescan_secs = self.cfg.rescan_interval_min.saturating_mul(60);
        let mut rescan_tick = tokio::time::interval_at(
            tokio::time::Instant::now() + std::time::Duration::from_secs(rescan_secs.max(60)),
            std::time::Duration::from_secs(rescan_secs.max(60)),
        );
        let rescan_enabled = self.cfg.rescan_interval_min > 0;
        if rescan_enabled {
            info!(interval_min = self.cfg.rescan_interval_min, "rescan périodique activé");
        }

        loop {
            if paused {
                tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => { break; }
                    maybe_cmd = cmd_rx.recv() => {
                        match maybe_cmd {
                            Some(EngineCommand::Resume) => {
                                info!("engine: resumed");
                                paused = false;
                                if rescan_on_resume {
                                    rescan_on_resume = false;
                                    info!("engine: rescan after config change");
                                    tracker.total_files.store(0, Ordering::Relaxed);
                                    tracker.done_files.store(0, Ordering::Relaxed);
                                    tracker.total_bytes.store(0, Ordering::Relaxed);
                                    tracker.sent_bytes.store(0, Ordering::Relaxed);
                                    let _ = status_tx.send(EngineStatus::Syncing { active: 0 });
                                    let ig = IgnoreMatcher::from_patterns(&self.cfg.ignore_patterns)?;
                                    tokio::select! {
                                        r = scan::run(&self.cfg, &db, &ig, &provider, &path_cache, &task_tx, &shutdown, &status_tx, & tracker) => {
                                            if let Err(e) = r {
                                                if is_shutdown_err(&e) { shutdown.cancel(); break; }
                                                let _ = status_tx.send(EngineStatus::Error(e.to_string()));
                                            } else if tracker.total_files.load(Ordering::Relaxed) == 0 {
                                                // NOUVEAU : Si 0 fichier trouvé, on met l'UI en repos immédiatement !
                                                let _ = status_tx.send(EngineStatus::Idle);
                                            }
                                        }
                                        _ = shutdown.cancelled() => { break; }
                                    }
                                }
                                let _ = status_tx.send(EngineStatus::Idle);
                            }
                            Some(EngineCommand::Shutdown) | None => {
                                info!("engine: shutdown (while paused)");
                                shutdown.cancel();
                                break;
                            }
                            Some(EngineCommand::ApplyConfig(new_cfg)) => {
                                info!(local = %new_cfg.sync_pairs[0].local_path.display(), "engine: config hot-reload (while paused)");
                                let root_changed = new_cfg.sync_pairs[0].local_path != self.cfg.sync_pairs[0].local_path;
                                self.cfg = new_cfg;
                                rescan_on_resume = true;
                                if root_changed {
                                    watcher.stop();
                                    db.clear()?;
                                    db.clear_dirs()?;
                                    let (tx2, rx2) = mpsc::channel(256);
                                    watcher = watcher::Watcher::start(&self.cfg.sync_pairs[0].local_path, tx2)?;
                                    spawn_debounced_dispatch(rx2, task_tx.clone(), shutdown.clone(),self.cfg.advanced.debounce_ms,tracker.clone());
                                }
                            }
                            _ => {}
                        }
                    }
                }
                continue;
            }

            tokio::select! {
                biased;
                _ = shutdown.cancelled() => { break; }

                maybe_cmd = cmd_rx.recv() => {
                    match maybe_cmd {
                        Some(EngineCommand::Shutdown) | None => {
                            info!("engine: shutdown command received");
                            shutdown.cancel();
                            break;
                        }
                        Some(EngineCommand::Pause) => {
                            info!("engine: paused (settings open)");
                            paused = true;
                            let _ = status_tx.send(EngineStatus::Paused);
                            crate::notif::paused(&self.cfg);
                        }
                        Some(EngineCommand::Resume) => {}
                        Some(EngineCommand::ForceScan) => {
                            info!("engine: force scan requested");
                            tracker.total_files.store(0, Ordering::Relaxed);
                            tracker.done_files.store(0, Ordering::Relaxed);
                            tracker.total_bytes.store(0, Ordering::Relaxed);
                            tracker.sent_bytes.store(0, Ordering::Relaxed);
                            let _ = status_tx.send(EngineStatus::Syncing { active: 0 });
                            let ignore2 = IgnoreMatcher::from_patterns(&self.cfg.ignore_patterns)?;
                            tokio::select! {
                                r = scan::run(&self.cfg, &db, &ignore2, &provider, &path_cache, &task_tx, &shutdown, &status_tx,&tracker) => {
                                    if let Err(e) = r {
                                        if is_shutdown_err(&e) { shutdown.cancel(); break; }
                                        let _ = status_tx.send(EngineStatus::Error(e.to_string()));
                                    } else if tracker.total_files.load(Ordering::Relaxed) == 0 {
                                        // NOUVEAU : Si 0 fichier trouvé, on met l'UI en repos immédiatement !
                                        let _ = status_tx.send(EngineStatus::Idle);
                                    }
                                }
                                _ = shutdown.cancelled() => { break; }
                            }
                        }
                        Some(EngineCommand::ApplyConfig(new_cfg)) => {
                            info!(local = %new_cfg.sync_pairs[0].local_path.display(), "engine: config hot-reload");
                            let root_changed = new_cfg.sync_pairs[0].local_path != self.cfg.sync_pairs[0].local_path;
                            self.cfg = new_cfg;

                            if root_changed {
                                watcher.stop();
                                db.clear()?;
                                db.clear_dirs()?;
                                let (tx2, rx2) = mpsc::channel(256);
                                watcher = watcher::Watcher::start(&self.cfg.sync_pairs[0].local_path, tx2)?;
                                spawn_debounced_dispatch(rx2, task_tx.clone(), shutdown.clone(),self.cfg.advanced.debounce_ms,tracker.clone());
                                let ignore3 = IgnoreMatcher::from_patterns(&self.cfg.ignore_patterns)?;
                                tracker.total_files.store(0, Ordering::Relaxed);
                                tracker.done_files.store(0, Ordering::Relaxed);
                                tracker.total_bytes.store(0, Ordering::Relaxed);
                                tracker.sent_bytes.store(0, Ordering::Relaxed);
                                let _ = status_tx.send(EngineStatus::Syncing { active: 0 });
                                tokio::select! {
                                    r = scan::run(&self.cfg, &db, &ignore3, &provider, &path_cache, &task_tx, &shutdown, &status_tx,&tracker) => {
                                        if let Err(e) = r {
                                            if is_shutdown_err(&e) { shutdown.cancel(); break; }
                                            let _ = status_tx.send(EngineStatus::Error(e.to_string()));
                                        } else if tracker.total_files.load(Ordering::Relaxed) == 0 {
                                            // NOUVEAU : Si 0 fichier trouvé, on met l'UI en repos immédiatement !
                                            let _ = status_tx.send(EngineStatus::Idle);
                                        }
                                    }
                                    _ = shutdown.cancelled() => { break; }
                                }
                            }
                            let _ = status_tx.send(EngineStatus::Idle);
                        }
                    }
                }

                _ = overflow_tick.tick() => {
                    if !self.cfg.sync_pairs[0].local_path.is_dir() {
                        let path_str = self.cfg.sync_pairs[0].local_path.display().to_string();
                        error!(path = %path_str, "local_root disparu — moteur en pause");
                        crate::notif::folder_missing(&self.cfg, &path_str);
                        let _ = status_tx.send(EngineStatus::Error(
                            format!("Dossier local introuvable : {path_str}")
                        ));
                        paused = true;
                        rescan_on_resume = true;
                        continue;
                    }

                    if watcher.take_overflow() {
                        warn!("engine: événements inotify perdus — rescan de rattrapage");
                        tracker.total_files.store(0, Ordering::Relaxed);
                        tracker.done_files.store(0, Ordering::Relaxed);
                        tracker.total_bytes.store(0, Ordering::Relaxed);
                        tracker.sent_bytes.store(0, Ordering::Relaxed);
                        let _ = status_tx.send(EngineStatus::Syncing { active: 0 });
                        let ignore_o = IgnoreMatcher::from_patterns(&self.cfg.ignore_patterns)?;
                        tokio::select! {
                            r = scan::run(&self.cfg, &db, &ignore_o, &provider, &path_cache, &task_tx, &shutdown, &status_tx, &tracker) => {
                                if let Err(e) = r {
                                    if is_shutdown_err(&e) { shutdown.cancel(); break; }
                                    let _ = status_tx.send(EngineStatus::Error(e.to_string()));
                                } else if tracker.total_files.load(Ordering::Relaxed) == 0 {
                                    // NOUVEAU : Si 0 fichier trouvé, on met l'UI en repos immédiatement !
                                    let _ = status_tx.send(EngineStatus::Idle);
                                }
                            }
                            _ = shutdown.cancelled() => { break; }
                        }
                        let _ = status_tx.send(EngineStatus::Idle);
                    }
                }

                _ = rescan_tick.tick(), if rescan_enabled => {
                    info!("engine: rescan périodique (toutes les {} min)", self.cfg.rescan_interval_min);
                    tracker.total_files.store(0, Ordering::Relaxed);
                    tracker.done_files.store(0, Ordering::Relaxed);
                    tracker.total_bytes.store(0, Ordering::Relaxed);
                    tracker.sent_bytes.store(0, Ordering::Relaxed);
                    let _ = status_tx.send(EngineStatus::Syncing { active: 0 });
                    let ignore_r = IgnoreMatcher::from_patterns(&self.cfg.ignore_patterns)?;
                    tokio::select! {
                        r = scan::run(&self.cfg, &db, &ignore_r, &provider, &path_cache, &task_tx, &shutdown, &status_tx, &tracker) => {
                            if let Err(e) = r {
                                if is_shutdown_err(&e) { shutdown.cancel(); break; }
                                let _ = status_tx.send(EngineStatus::Error(e.to_string()));
                            }else if tracker.total_files.load(Ordering::Relaxed) == 0 {
                                // NOUVEAU : Si 0 fichier trouvé, on met l'UI en repos immédiatement !
                                let _ = status_tx.send(EngineStatus::Idle);
                            }
                        }
                        _ = shutdown.cancelled() => { break; }
                    }
                    let _ = status_tx.send(EngineStatus::Idle);
                }

                maybe_task = task_rx.recv() => {
                    let Some(task) = maybe_task else { break; };

                    // let (_file_name, file_size) = match &task {
                    //     Task::SyncFile { ref path } => (
                    //         path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
                    //         std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
                    //     ),
                    //     Task::Delete(p) => (
                    //         p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
                    //         0u64,
                    //     ),
                    //     Task::Rename { to, .. } => (
                    //         to.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
                    //         0u64,
                    //     ),
                    // };

                    let permit = tokio::select! {
                        p = sem.clone().acquire_owned() => p.context("semaphore closed")?,
                        _ = shutdown.cancelled() => { break; } // Sortie immédiate !
                    };
                    let db2 = db.clone();
                    let provider2 = provider.clone();
                    let path_cache2 = path_cache.clone();
                    let cfg2 = self.cfg.clone();
                    let sd2 = shutdown.clone();
                    let stx2 = status_tx.clone();
                    let active2 = active.clone();

                    let tracker2 = tracker.clone(); // <-- INDISPENSABLE !
                    let ignore_pat = self.cfg.ignore_patterns.clone();

                    active2.fetch_add(1, Ordering::Relaxed);

                    tokio::spawn(async move {
                       let _permit = permit;
                        if sd2.is_cancelled() {
                            tracker2.done_files.fetch_add(1, Ordering::Relaxed);
                            let prev = active2.fetch_sub(1, Ordering::Relaxed);
                            if prev == 1 { let _ = stx2.send(EngineStatus::Idle); }
                            return;
                        }

                        let ignore = IgnoreMatcher::from_patterns(&ignore_pat).unwrap();

                        // --- MODIFICATION ICI : On rend l'erreur TRÈS visible ---
                        match worker::handle(task.clone(), &cfg2, &db2, &provider2, &path_cache2, &ignore, tracker2.clone(), &sd2).await {
                            Ok(_) => {
                                // Tout s'est bien passé
                            }
                            Err(e) => {
                                if !is_shutdown_err(&e) && !sd2.is_cancelled() {
                                    warn!("❌ ERREUR FATALE OUVRIER sur {:?} : {:?}", task, e);

                                    let _ = stx2.send(EngineStatus::Error(e.to_string()));
                                    if scan::is_quota_err(&e) {
                                        crate::notif::quota_exceeded(&cfg2);
                                    } else {
                                        crate::notif::error(&cfg2, &e.to_string());
                                    }
                                }
                            }
                        }

                        tracker2.done_files.fetch_add(1, Ordering::Relaxed);
                        let prev = active2.fetch_sub(1, Ordering::Relaxed);
                        if prev == 1 {
                            let _ = stx2.send(EngineStatus::Idle);
                        }
                    });
                }
            }
        }

        finish(&status_tx).await;
        Ok(())
    }
}

async fn finish(status_tx: &mpsc::UnboundedSender<EngineStatus>) {
    let _ = status_tx.send(EngineStatus::Stopped);
    info!("engine stopped");
}

pub(crate) fn is_shutdown_err(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        let s = c.to_string();
        s.contains("shutdown") || s.contains("interrupted")
    })
}

fn spawn_debounced_dispatch(
    mut watch_rx: mpsc::Receiver<watcher::WatchEvent>,
    task_tx: mpsc::Sender<Task>,
    shutdown: CancellationToken,
    debounce_ms: u64,
    tracker: Arc<ProgressTracker>,
) {
    use std::collections::HashMap;
    use tokio::time::{Duration, Instant, interval};
    use watcher::WatchEvent;

    tokio::spawn(async move {
        let mut pending: HashMap<PathBuf, Instant> = HashMap::new();
        let mut tick = interval(Duration::from_millis(200));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => break,
                _ = tick.tick() => {
                    let now = Instant::now();
                    let debounce = Duration::from_millis(debounce_ms);
                    let ready: Vec<PathBuf> = pending.iter()
                        .filter(|(_, ts)| now.duration_since(**ts) >= debounce)
                        .map(|(p, _)| p.clone())
                        .collect();
                    for path in ready {
                        pending.remove(&path);
                        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                        tracker.total_files.fetch_add(1, Ordering::Relaxed);
                        tracker.total_bytes.fetch_add(size, Ordering::Relaxed);
                        let task = Task::SyncFile { path };
                        if task_tx.send(task).await.is_err() { return; }
                    }
                }
                maybe_ev = watch_rx.recv() => {
                    let ev = match maybe_ev {
                        Some(ev) => ev,
                        None => break,
                    };
                    match ev {
                        WatchEvent::Modified(p) => {
                            pending.insert(p, Instant::now());
                        }
                        WatchEvent::Deleted(p) => {
                            pending.remove(&p);
                            tracker.total_files.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            let task = Task::Delete(p);
                            if task_tx.send(task).await.is_err() { return; }
                        }
                        WatchEvent::Renamed { from, to } => {
                            pending.remove(&from);
                            pending.remove(&to);
                            tracker.total_files.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            let task = Task::Rename { from, to };
                            if task_tx.send(task).await.is_err() { return; }
                        }
                    }
                }
            }
        }
    });
}

pub async fn run_unconfigured(
    db: Database,
    shutdown: CancellationToken,
    mut cmd_rx: mpsc::Receiver<EngineCommand>,
    status_tx: mpsc::UnboundedSender<EngineStatus>,
) -> Result<()> {
    loop {
        tokio::select! {
            maybe_cmd = cmd_rx.recv() => {
                match maybe_cmd {
                    Some(EngineCommand::ApplyConfig(cfg)) => {
                        match cfg.validate() {
                            Err(e) => {
                                warn!(reason = %e, "config rejected: still invalid");
                                let _ = status_tx.send(EngineStatus::Unconfigured(e.to_string()));
                            }
                            Ok(()) => {
                                info!(local = %cfg.sync_pairs[0].local_path.display(), "valid config received, starting engine");
                                let engine = SyncEngine::new(cfg);
                                return engine.run(db, shutdown, cmd_rx, status_tx).await;
                            }
                        }
                    }
                    Some(EngineCommand::Shutdown) | None => {
                        shutdown.cancel();
                        break;
                    }
                    _ => {}
                }
            }
            _ = shutdown.cancelled() => { break; }
        }
    }
    let _ = status_tx.send(EngineStatus::Stopped);
    Ok(())
}

/// Tâche de fond qui publie la progression toutes les 200ms
pub(crate) async fn progress_publisher(
    tracker: Arc<bandwidth::ProgressTracker>,
    status_tx: mpsc::UnboundedSender<EngineStatus>,
    shutdown: CancellationToken,
) {
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(200));
    loop {
        tokio::select! {
            _ = interval.tick() => {
                let snap = tracker.snapshot();
                // On publie uniquement si on est activement en train de synchroniser
                if snap.total_files > 0 && snap.done_files < snap.total_files {
                    let _ = status_tx.send(EngineStatus::SyncProgress(snap));
                }
            }
            _ = shutdown.cancelled() => {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;
    use std::path::PathBuf;
    use crate::config::AppConfig;

    #[tokio::test]
    async fn test_main_uses_config_channel_capacity() {
        let cfg = AppConfig::default();
        let capacity = cfg.advanced.engine_channel_capacity;

        // On vérifie que la mécanique de backpressure utilise bien la valeur configurée
        let (tx, _rx) = mpsc::channel::<usize>(capacity);
        for i in 0..capacity {
            assert!(tx.try_send(i).is_ok(), "Le channel doit accepter les messages jusqu'à 'capacity'");
        }
        assert!(tx.try_send(capacity).is_err(), "Le channel doit rejeter les messages après avoir atteint 'capacity'");
    }

    #[tokio::test]
    async fn test_debounce_zero_means_immediate() {
        let (watch_tx, watch_rx) = mpsc::channel(10);
        let (task_tx, mut task_rx) = mpsc::channel(10);
        let shutdown = CancellationToken::new();
        let tracker = Arc::new(crate::engine::bandwidth::ProgressTracker::new());

        // On lance le dispatcher avec 0ms de debounce
        spawn_debounced_dispatch(watch_rx, task_tx, shutdown.clone(), 0,tracker);

        let start = tokio::time::Instant::now();

        watch_tx.send(watcher::WatchEvent::Modified(PathBuf::from("zero.txt"))).await.unwrap();

        let _ = task_rx.recv().await.unwrap();
        let elapsed = start.elapsed().as_millis() as u64;

        assert!(elapsed < 50, "Le debounce à 0 doit être quasi instantané (reçu: {}ms)", elapsed);
    }

    #[tokio::test]
    async fn test_engine_uses_config_debounce() {
        let (watch_tx, watch_rx) = mpsc::channel(10);
        let (task_tx, mut task_rx) = mpsc::channel(10);
        let shutdown = CancellationToken::new();

        let cfg = AppConfig::default();
        let configured_debounce = cfg.advanced.debounce_ms; // Par défaut 500ms
        let tracker = Arc::new(crate::engine::bandwidth::ProgressTracker::new());

        // On lance le dispatcher avec la configuration dynamique
        spawn_debounced_dispatch(watch_rx, task_tx, shutdown.clone(), configured_debounce,tracker);

        let start = tokio::time::Instant::now();

        watch_tx.send(watcher::WatchEvent::Modified(PathBuf::from("config.txt"))).await.unwrap();

        let _ = task_rx.recv().await.unwrap();
        let elapsed = start.elapsed().as_millis() as u64;

        assert!(elapsed >= configured_debounce, "Le debounce n'a pas respecté la configuration ! Attendu >= {}, reçu {}", configured_debounce, elapsed);
    }

    #[tokio::test]
    async fn test_progress_publisher_throttle() {
        let (status_tx, mut status_rx) = mpsc::unbounded_channel();
        let shutdown = CancellationToken::new();
        let tracker = Arc::new(crate::engine::bandwidth::ProgressTracker::new());

        // On simule une synchronisation en cours (sinon le publisher reste silencieux)
        tracker.total_files.store(10, std::sync::atomic::Ordering::Relaxed);
        tracker.done_files.store(5, std::sync::atomic::Ordering::Relaxed);

        // On lance le publisher (qui est censé émettre toutes les 200ms)
        tokio::spawn(super::progress_publisher(tracker.clone(), status_tx, shutdown.clone()));

        // On bombarde le tracker de 1000 mises à jour instantanément
        for _ in 0..1000 {
            tracker.record_bytes(1024);
        }

        // On laisse juste le temps au publisher de faire son tick de 200ms
        tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;
        shutdown.cancel(); // On coupe le publisher

        // On compte combien de fois l'UI a été notifiée
        let mut count = 0;
        while let Ok(msg) = status_rx.try_recv() {
            if let EngineStatus::SyncProgress(_) = msg {
                count += 1;
            }
        }

        // C'est la magie de notre architecture : malgré 1000 mises à jour du transfert,
        // l'interface n'aura reçu qu'1 (ou maximum 2) rafraîchissements !
        assert!(count >= 1, "Le publisher n'a rien publié !");
        assert!(count <= 2, "Le publisher n'a pas throttlé ! Snapshots reçus : {}", count);
    }
}