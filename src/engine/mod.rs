pub mod scan;
pub mod watcher;
pub mod worker;

use std::collections::HashSet;
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
    /// Phase de démarrage (chargement config, ouverture DB…).
    Starting,
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
    RemoteListing,
    LocalListing,
    Directories,
    Comparing,
}

#[derive(Debug, Clone)]
pub(crate) enum Task {
    SyncFile {
        path: PathBuf,
        /// Index distant pré-calculé par le scan (évite un `stat` par fichier).
        /// `None` pour les tâches du watcher (pas d'index fiable).
        remote_index: Option<Arc<HashSet<String>>>,
    },
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
        cmd_rx: mpsc::Receiver<EngineCommand>,
        status_tx: mpsc::UnboundedSender<EngineStatus>,
    ) -> Result<()> {
        let kio = KioClient::new(
            shutdown.clone(),
            std::time::Duration::from_secs(self.cfg.kio_timeout_secs),
        );
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
        let (task_tx, mut task_rx) = mpsc::channel::<Task>(1024);
        let ignore = IgnoreMatcher::from_patterns(&self.cfg.ignore_patterns)?;

        let mut paused = false;
        let mut rescan_on_resume = false;

        // ── Scan initial (interruptible par Pause/Shutdown) ──────────────────
        // Le scan est pinné pour rester vivant si on reçoit une commande non-critique.
        // - Pause   → scan annulé, moteur en pause, rescan au Resume.
        // - Shutdown → arrêt immédiat.
        // - Autres  → ignorées (ForceScan inutile, ApplyConfig arrivera après Pause).
        {
            let _ = status_tx.send(EngineStatus::Syncing { active: 0 });
            let scan = scan::run(&self.cfg, &db, &ignore, &kio, &task_tx, &shutdown, &status_tx);
            tokio::pin!(scan);

            loop {
                tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => {
                        kio.terminate_all().await;
                        finish(&kio, &status_tx).await;
                        return Ok(());
                    }
                    cmd = cmd_rx.recv() => {
                        match cmd {
                            Some(EngineCommand::Pause) => {
                                info!("engine: paused during initial scan");
                                paused = true;
                                rescan_on_resume = true;
                                let _ = status_tx.send(EngineStatus::Paused);
                                break; // scan future dropped → cancelled
                            }
                            Some(EngineCommand::Shutdown) | None => {
                                shutdown.cancel();
                                kio.terminate_all().await;
                                finish(&kio, &status_tx).await;
                                return Ok(());
                            }
                            _ => {} // ignorée pendant le scan, loop continue
                        }
                    }
                    r = &mut scan => {
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
                                crate::notif::error(&self.cfg, &e.to_string());
                            }
                        }
                        break; // scan terminé
                    }
                }
            }
        }

        if !paused {
            let _ = status_tx.send(EngineStatus::Idle);
        }

        // ── Watcher inotify ───────────────────────────────────────────────────
        let (watch_tx, watch_rx) = mpsc::channel(256);
        let mut watcher = watcher::Watcher::start(&self.cfg.local_root, watch_tx)?;

        // Dispatch watcher → task queue (avec debounce 500ms sur Modified)
        let task_tx_w = task_tx.clone();
        let sd_w = shutdown.clone();
        spawn_debounced_dispatch(watch_rx, task_tx_w, sd_w);

        // ── Boucle principale ─────────────────────────────────────────────────
        let sem = Arc::new(Semaphore::new(self.cfg.max_workers.max(1)));
        let active = Arc::new(AtomicUsize::new(0));
        let total_queued = Arc::new(AtomicUsize::new(0));
        let total_done = Arc::new(AtomicUsize::new(0));

        // Timer de rattrapage : toutes les 30s on vérifie si le watcher a
        // perdu des événements (channel plein). Si oui → rescan complet.
        // Stratégie identique à Dropbox/OneDrive.
        let mut overflow_tick = tokio::time::interval_at(
            tokio::time::Instant::now() + std::time::Duration::from_secs(30),
            std::time::Duration::from_secs(30),
        );

        // Timer de rescan périodique : vérifie l'égalité local = DB = remote
        // même si aucun événement inotify n'a été reçu. Détecte les suppressions
        // manuelles sur le remote (GDrive), les corruptions, etc.
        // 0 = désactivé.
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
                                    total_queued.store(0, Ordering::Relaxed);
                                    total_done.store(0, Ordering::Relaxed);
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
                                    db.clear_dirs()?;
                                    let (tx2, rx2) = mpsc::channel(256);
                                    watcher = watcher::Watcher::start(&self.cfg.local_root, tx2)?;
                                    spawn_debounced_dispatch(rx2, task_tx.clone(), shutdown.clone());
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
                            total_queued.store(0, Ordering::Relaxed);
                            total_done.store(0, Ordering::Relaxed);
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
                                db.clear_dirs()?;
                                let (tx2, rx2) = mpsc::channel(256);
                                watcher = watcher::Watcher::start(&self.cfg.local_root, tx2)?;
                                // Re-dispatch avec debounce
                                spawn_debounced_dispatch(rx2, task_tx.clone(), shutdown.clone());
                                // Rescan
                                let ignore3 = IgnoreMatcher::from_patterns(&self.cfg.ignore_patterns)?;
                                total_queued.store(0, Ordering::Relaxed);
                                total_done.store(0, Ordering::Relaxed);
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

                // ── Tick 30s : vérification santé + rescan si overflow ────────
                _ = overflow_tick.tick() => {
                    // §4B UX_SYSTRAY.md : dossier local disparu → notification + pause
                    if !self.cfg.local_root.is_dir() {
                        let path_str = self.cfg.local_root.display().to_string();
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
                        total_queued.store(0, Ordering::Relaxed);
                        total_done.store(0, Ordering::Relaxed);
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

                // ── Rescan périodique : vérifie local = DB = remote ─────────
                _ = rescan_tick.tick(), if rescan_enabled => {
                    info!("engine: rescan périodique (toutes les {} min)", self.cfg.rescan_interval_min);
                    total_queued.store(0, Ordering::Relaxed);
                    total_done.store(0, Ordering::Relaxed);
                    let _ = status_tx.send(EngineStatus::Syncing { active: 0 });
                    let ignore_r = IgnoreMatcher::from_patterns(&self.cfg.ignore_patterns)?;
                    tokio::select! {
                        r = scan::run(&self.cfg, &db, &ignore_r, &kio, &task_tx, &shutdown, &status_tx) => {
                            if let Err(e) = r {
                                if is_shutdown_err(&e) { shutdown.cancel(); break; }
                                let _ = status_tx.send(EngineStatus::Error(e.to_string()));
                            }
                        }
                        _ = shutdown.cancelled() => { break; }
                    }
                    let _ = status_tx.send(EngineStatus::Idle);
                }

                maybe_task = task_rx.recv() => {
                    let Some(task) = maybe_task else { break; };

                    // Extraire le nom et la taille du fichier pour le tooltip
                    let (file_name, file_size) = match &task {
                        Task::SyncFile { ref path, .. } => (
                            path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
                            std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
                        ),
                        Task::Delete(p) => (
                            p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
                            0u64,
                        ),
                        Task::Rename { to, .. } => (
                            to.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
                            0u64,
                        ),
                    };

                    let queued = total_queued.fetch_add(1, Ordering::Relaxed) + 1;

                    let permit = sem.clone().acquire_owned().await
                        .context("semaphore closed")?;
                    let db2 = db.clone();
                    let kio2 = kio.clone();
                    let cfg2 = self.cfg.clone();
                    let sd2 = shutdown.clone();
                    let stx2 = status_tx.clone();
                    let active2 = active.clone();
                    let done2 = total_done.clone();
                    let queued2 = total_queued.clone();
                    let ignore_pat = self.cfg.ignore_patterns.clone();

                    active2.fetch_add(1, Ordering::Relaxed);

                    // Envoyer SyncProgress avec le nom du fichier courant
                    let _ = stx2.send(EngineStatus::SyncProgress {
                        done: total_done.load(Ordering::Relaxed),
                        total: queued,
                        current: file_name,
                        size_bytes: file_size,
                    });

                    tokio::spawn(async move {
                        let _permit = permit;
                        // Vérifier le shutdown AVANT de commencer le travail :
                        // évite de lire/hasher un fichier alors qu'on s'arrête.
                        if sd2.is_cancelled() {
                            let done_now = done2.fetch_add(1, Ordering::Relaxed) + 1;
                            let prev = active2.fetch_sub(1, Ordering::Relaxed);
                            if prev == 1 {
                                let _ = stx2.send(EngineStatus::Idle);
                            } else {
                                let _ = stx2.send(EngineStatus::SyncProgress {
                                    done: done_now,
                                    total: queued2.load(Ordering::Relaxed),
                                    current: String::new(),
                                    size_bytes: 0,
                                });
                            }
                            return;
                        }
                        let ignore = IgnoreMatcher::from_patterns(&ignore_pat).unwrap();
                        if let Err(e) = worker::handle(task, &cfg2, &db2, &kio2, &ignore, &sd2).await {
                            // Pendant le shutdown, ne pas reporter les erreurs de lecture locale
                            if !is_shutdown_err(&e) && !sd2.is_cancelled() {
                                error!(error = %e, "worker task failed");
                                let _ = stx2.send(EngineStatus::Error(e.to_string()));
                                if scan::is_quota_err(&e) {
                                    crate::notif::quota_exceeded(&cfg2);
                                } else {
                                    crate::notif::error(&cfg2, &e.to_string());
                                }
                            }
                        }
                        let done_now = done2.fetch_add(1, Ordering::Relaxed) + 1;
                        let prev = active2.fetch_sub(1, Ordering::Relaxed);
                        if prev == 1 {
                            let _ = stx2.send(EngineStatus::Idle);
                        } else {
                            // Mettre à jour la progression globale
                            let _ = stx2.send(EngineStatus::SyncProgress {
                                done: done_now,
                                total: queued2.load(Ordering::Relaxed),
                                current: String::new(),
                                size_bytes: 0,
                            });
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

/// Dispatch watcher → task queue avec **debounce** sur les événements `Modified`.
///
/// Un seul "enregistrement" dans un éditeur peut générer 2–4 événements inotify
/// (Create, Modify(Data), Close(Write)…). Sans debounce, chaque événement déclenche
/// un upload séparé. Le debounce regroupe les événements pour le même fichier
/// dans une fenêtre de 500ms : seul le dernier événement provoque un upload.
///
/// `Delete` et `Rename` sont forwardés immédiatement (pas de coalescence).
fn spawn_debounced_dispatch(
    mut watch_rx: mpsc::Receiver<watcher::WatchEvent>,
    task_tx: mpsc::Sender<Task>,
    shutdown: CancellationToken,
) {
    use std::collections::HashMap;
    use tokio::time::{Duration, Instant, interval};
    use watcher::WatchEvent;

    const DEBOUNCE_MS: u64 = 500;

    tokio::spawn(async move {
        let mut pending: HashMap<PathBuf, Instant> = HashMap::new();
        let mut tick = interval(Duration::from_millis(200));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => break,

                // Tick : flusher les événements Modified dont le debounce a expiré.
                _ = tick.tick() => {
                    let now = Instant::now();
                    let debounce = Duration::from_millis(DEBOUNCE_MS);
                    let ready: Vec<PathBuf> = pending.iter()
                        .filter(|(_, ts)| now.duration_since(**ts) >= debounce)
                        .map(|(p, _)| p.clone())
                        .collect();
                    for path in ready {
                        pending.remove(&path);
                        let task = Task::SyncFile { path, remote_index: None };
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
                            // Enregistrer/remettre à zéro le timer debounce.
                            pending.insert(p, Instant::now());
                        }
                        WatchEvent::Deleted(p) => {
                            // Annuler tout Modified en attente pour ce fichier.
                            pending.remove(&p);
                            let task = Task::Delete(p);
                            if task_tx.send(task).await.is_err() { return; }
                        }
                        WatchEvent::Renamed { from, to } => {
                            // Annuler tout Modified en attente pour la source.
                            pending.remove(&from);
                            pending.remove(&to);
                            let task = Task::Rename { from, to };
                            if task_tx.send(task).await.is_err() { return; }
                        }
                    }
                }
            }
        }
    });
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

