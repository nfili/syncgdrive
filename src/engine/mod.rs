pub mod scan;
pub mod watcher;
pub mod worker;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result};
use tokio::sync::{mpsc, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::config::AppConfig;
use crate::db::Database;
use crate::ignore::IgnoreMatcher;
use crate::kio::{KioClient, KioOps};

// ── Types publics ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum EngineCommand {
    ForceScan,
    Shutdown,
    ApplyConfig(AppConfig),
    Pause,
    Resume,
}

#[derive(Debug, Clone)]
pub enum EngineStatus {
    /// En attente d'une config valide.
    Unconfigured(String),
    Idle,
    /// Scan initial : phase dossiers ou inventaire.
    ScanProgress {
        phase: ScanPhase,
        done: usize,
        total: usize,
        current: String,
    },
    /// Transfert de fichiers en cours.
    SyncProgress {
        done: usize,
        total: usize,
        current: String,
        size_bytes: u64,
    },
    Syncing { active: usize },
    Paused,
    Error(String),
    Stopped,
}

#[derive(Debug, Clone)]
pub enum ScanPhase {
    Listing,
    Directories,
    Comparing,
}

#[derive(Debug, Clone)]
pub(crate) enum Task {
    SyncFile(PathBuf),
    Delete(PathBuf),
    Rename { from: PathBuf, to: PathBuf },
}

// ── SyncEngine ────────────────────────────────────────────────────────────────

pub struct SyncEngine {
    cfg: AppConfig,
}

impl SyncEngine {
    pub fn new(cfg: AppConfig) -> Self {
        Self { cfg }
    }

    pub async fn run(
        self,
        db: Database,
        shutdown: CancellationToken,
        mut cmd_rx: mpsc::Receiver<EngineCommand>,
        status_tx: mpsc::UnboundedSender<EngineStatus>,
    ) -> Result<()> {
        let kio = KioClient::new(shutdown.clone());
        self.run_with_kio(kio, db, shutdown, cmd_rx, status_tx).await
    }

    async fn run_with_kio<K: KioOps>(
        mut self,
        kio: K,
        db: Database,
        shutdown: CancellationToken,
        mut cmd_rx: mpsc::Receiver<EngineCommand>,
        status_tx: mpsc::UnboundedSender<EngineStatus>,
    ) -> Result<()> {
        // ── Scan initial ──────────────────────────────────────────────────────
        let _ = status_tx.send(EngineStatus::Syncing { active: 0 });

        let (task_tx, mut task_rx) = mpsc::channel::<Task>(1024);
        let ignore = IgnoreMatcher::from_patterns(&self.cfg.ignore_patterns)?;

        // Exécuté dans le contexte Tokio courant — le select! permet l'interruption.
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                kio.terminate_all().await;
                finish(&kio, &status_tx).await;
                return Ok(());
            }
            r = scan::run(&self.cfg, &db, &ignore, &kio, &task_tx, &shutdown, &status_tx) => {
                match r {
                    Ok(()) => {}
                    Err(e) if is_shutdown_err(&e) => {
                        kio.terminate_all().await;
                        finish(&kio, &status_tx).await;
                        return Ok(());
                    }
                    Err(e) => {
                        warn!(error = %e, "initial scan failed, continuing with watcher");
                        let _ = status_tx.send(EngineStatus::Error(e.to_string()));
                    }
                }
            }
        }

        let _ = status_tx.send(EngineStatus::Idle);

        // ── Watcher inotify ───────────────────────────────────────────────────
        let (watch_tx, mut watch_rx) = mpsc::channel(256);
        let mut watcher = watcher::Watcher::start(&self.cfg.local_root, watch_tx)?;
        let _ignore = IgnoreMatcher::from_patterns(&self.cfg.ignore_patterns)?;

        // Dispatch watcher → task queue
        let task_tx_w = task_tx.clone();
        let sd_w = shutdown.clone();
        tokio::spawn(async move {
            use watcher::WatchEvent;
            while let Some(ev) = watch_rx.recv().await {
                if sd_w.is_cancelled() { break; }
                let task = match ev {
                    WatchEvent::Modified(p)           => Task::SyncFile(p),
                    WatchEvent::Deleted(p)            => Task::Delete(p),
                    WatchEvent::Renamed { from, to }  => Task::Rename { from, to },
                };
                if task_tx_w.send(task).await.is_err() { break; }
            }
        });

        // ── Boucle principale ─────────────────────────────────────────────────
        let sem = Arc::new(Semaphore::new(self.cfg.max_workers.max(1)));
        let active = Arc::new(AtomicUsize::new(0));
        let mut paused = false;
        let mut rescan_on_resume = false;

        // Timer de rattrapage : toutes les 30s on vérifie si le watcher a
        // perdu des événements (channel plein). Si oui → rescan complet.
        // Stratégie identique à Dropbox/OneDrive.
        let mut overflow_tick = tokio::time::interval_at(
            tokio::time::Instant::now() + std::time::Duration::from_secs(30),
            std::time::Duration::from_secs(30),
        );

        loop {
            // Quand le moteur est en pause (ex: fenêtre Settings ouverte),
            // on n'écoute QUE les commandes — les tasks s'accumulent dans le
            // channel et seront traitées au Resume.
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
                                    let _ = status_tx.send(EngineStatus::Syncing { active: 0 });
                                    let ig = IgnoreMatcher::from_patterns(&self.cfg.ignore_patterns)?;
                                    tokio::select! {
                                        r = scan::run(&self.cfg, &db, &ig, &kio, &task_tx, &shutdown, &status_tx) => {
                                            if let Err(e) = r {
                                                if is_shutdown_err(&e) { shutdown.cancel(); break; }
                                                let _ = status_tx.send(EngineStatus::Error(e.to_string()));
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
                                info!(local = %new_cfg.local_root.display(), "engine: config hot-reload (while paused)");
                                let root_changed = new_cfg.local_root != self.cfg.local_root;
                                self.cfg = new_cfg;
                                rescan_on_resume = true;
                                if root_changed {
                                    watcher.stop();
                                    db.clear()?;
                                    let (tx2, rx2) = mpsc::channel(256);
                                    watcher = watcher::Watcher::start(&self.cfg.local_root, tx2)?;
                                    let task_tx_r = task_tx.clone();
                                    let sd_r = shutdown.clone();
                                    tokio::spawn(async move {
                                        use watcher::WatchEvent;
                                        let mut rx2 = rx2;
                                        while let Some(ev) = rx2.recv().await {
                                            if sd_r.is_cancelled() { break; }
                                            let t = match ev {
                                                WatchEvent::Modified(p)          => Task::SyncFile(p),
                                                WatchEvent::Deleted(p)           => Task::Delete(p),
                                                WatchEvent::Renamed { from, to } => Task::Rename { from, to },
                                            };
                                            if task_tx_r.send(t).await.is_err() { break; }
                                        }
                                    });
                                }
                            }
                            _ => {} // ForceScan / Pause ignorés en pause
                        }
                    }
                }
                continue;
            }

            tokio::select! {
                biased;  // shutdown vérifié EN PREMIER à chaque itération

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
                        Some(EngineCommand::Resume) => {
                            // Déjà actif — rien à faire
                        }
                        Some(EngineCommand::ForceScan) => {
                            info!("engine: force scan requested");
                            let _ = status_tx.send(EngineStatus::Syncing { active: 0 });
                            let ignore2 = IgnoreMatcher::from_patterns(&self.cfg.ignore_patterns)?;
                            tokio::select! {
                                r = scan::run(&self.cfg, &db, &ignore2, &kio, &task_tx, &shutdown, &status_tx) => {
                                    if let Err(e) = r {
                                        if is_shutdown_err(&e) { shutdown.cancel(); break; }
                                        let _ = status_tx.send(EngineStatus::Error(e.to_string()));
                                    }
                                }
                                _ = shutdown.cancelled() => { break; }
                            }
                        }
                        Some(EngineCommand::ApplyConfig(new_cfg)) => {
                            info!(local = %new_cfg.local_root.display(), "engine: config hot-reload");
                            let root_changed = new_cfg.local_root != self.cfg.local_root;
                            self.cfg = new_cfg;

                            if root_changed {
                                watcher.stop();
                                db.clear()?;
                                let (tx2, rx2) = mpsc::channel(256);
                                watcher = watcher::Watcher::start(&self.cfg.local_root, tx2)?;
                                // Re-dispatch
                                let task_tx_r = task_tx.clone();
                                let sd_r = shutdown.clone();
                                tokio::spawn(async move {
                                    use watcher::WatchEvent;
                                    let mut rx2 = rx2;
                                    while let Some(ev) = rx2.recv().await {
                                        if sd_r.is_cancelled() { break; }
                                        let t = match ev {
                                            WatchEvent::Modified(p)          => Task::SyncFile(p),
                                            WatchEvent::Deleted(p)           => Task::Delete(p),
                                            WatchEvent::Renamed { from, to } => Task::Rename { from, to },
                                        };
                                        if task_tx_r.send(t).await.is_err() { break; }
                                    }
                                });
                                // Rescan
                                let ignore3 = IgnoreMatcher::from_patterns(&self.cfg.ignore_patterns)?;
                                let _ = status_tx.send(EngineStatus::Syncing { active: 0 });
                                tokio::select! {
                                    r = scan::run(&self.cfg, &db, &ignore3, &kio, &task_tx, &shutdown, &status_tx) => {
                                        if let Err(e) = r {
                                            if is_shutdown_err(&e) { shutdown.cancel(); break; }
                                            let _ = status_tx.send(EngineStatus::Error(e.to_string()));
                                        }
                                    }
                                    _ = shutdown.cancelled() => { break; }
                                }
                            }
                            let _ = status_tx.send(EngineStatus::Idle);
                        }
                    }
                }

                // ── Rescan de rattrapage si le watcher a débordé ──────────────
                _ = overflow_tick.tick() => {
                    if watcher.take_overflow() {
                        warn!("engine: événements inotify perdus — rescan de rattrapage");
                        let _ = status_tx.send(EngineStatus::Syncing { active: 0 });
                        let ignore_o = IgnoreMatcher::from_patterns(&self.cfg.ignore_patterns)?;
                        tokio::select! {
                            r = scan::run(&self.cfg, &db, &ignore_o, &kio, &task_tx, &shutdown, &status_tx) => {
                                if let Err(e) = r {
                                    if is_shutdown_err(&e) { shutdown.cancel(); break; }
                                    let _ = status_tx.send(EngineStatus::Error(e.to_string()));
                                }
                            }
                            _ = shutdown.cancelled() => { break; }
                        }
                        let _ = status_tx.send(EngineStatus::Idle);
                    }
                }

                maybe_task = task_rx.recv() => {
                    let Some(task) = maybe_task else { break; };

                    let permit = sem.clone().acquire_owned().await
                        .context("semaphore closed")?;
                    let db2 = db.clone();
                    let kio2 = kio.clone();
                    let cfg2 = self.cfg.clone();
                    let sd2 = shutdown.clone();
                    let stx2 = status_tx.clone();
                    let active2 = active.clone();
                    let ignore_pat = self.cfg.ignore_patterns.clone();

                    active2.fetch_add(1, Ordering::Relaxed);
                    let _ = stx2.send(EngineStatus::Syncing { active: active2.load(Ordering::Relaxed) });

                    tokio::spawn(async move {
                        let _permit = permit;
                        let ignore = IgnoreMatcher::from_patterns(&ignore_pat).unwrap();
                        if let Err(e) = worker::handle(task, &cfg2, &db2, &kio2, &ignore, &sd2).await {
                            if !is_shutdown_err(&e) {
                                error!(error = %e, "worker task failed");
                                let _ = stx2.send(EngineStatus::Error(e.to_string()));
                            }
                        }
                        let prev = active2.fetch_sub(1, Ordering::Relaxed);
                        if prev == 1 {
                            let _ = stx2.send(EngineStatus::Idle);
                        }
                    });
                }
            }
        }

        finish(&kio, &status_tx).await;
        Ok(())
    }
}

// ── Utilitaires ───────────────────────────────────────────────────────────────

async fn finish<K: KioOps>(kio: &K, status_tx: &mpsc::UnboundedSender<EngineStatus>) {
    kio.terminate_all().await;
    let _ = status_tx.send(EngineStatus::Stopped);
    info!("engine stopped");
}

pub(crate) fn is_shutdown_err(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        let s = c.to_string();
        s.contains("shutdown") || s.contains("interrupted")
    })
}

/// Boucle d'attente de config valide (premier lancement).
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
                                info!(local = %cfg.local_root.display(), "valid config received, starting engine");
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

