pub mod bandwidth;
pub mod integrity;
pub mod offline;
pub mod rate_limiter;
pub mod scan;
pub mod watcher;
pub mod worker;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::{mpsc, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::config::AppConfig;
use crate::db::Database;
use crate::engine::bandwidth::ProgressTracker;
use crate::ignore::IgnoreMatcher;
use crate::remote::{path_cache::PathCache, HealthStatus, RemoteProvider};

// ── Types publics ─────────────────────────────────────────────────────────────

/// Commandes externes permettant de piloter le moteur de synchronisation.
/// Ces événements sont généralement émis par l'interface utilisateur (systray).
#[derive(Debug, Clone)]
pub enum EngineCommand {
    /// Force un scan complet immédiat, même si le délai n'est pas écoulé.
    ForceScan,
    /// Demande l'arrêt gracieux du démon et de tous ses workers.
    Shutdown,
    /// Applique une nouvelle configuration à chaud et déclenche un redémarrage interne.
    ApplyConfig(Arc<AppConfig>),
    /// Suspend temporairement l'exécution des tâches de synchronisation.
    Pause,
    /// Reprend l'exécution normale du moteur après une pause.
    Resume,
    /// Met le moteur en pause pendant l'édition des réglages.
    OpenSettings,
    /// Met le moteur en pause pendant la consultation de l'aide.
    OpenHelp,
}

/// Représente l'état en temps réel du moteur pour l'interface utilisateur.
#[derive(Debug, Clone)]
pub enum EngineStatus {
    /// Le moteur démarre (avec un pourcentage de progression).
    Starting(u8),
    /// Le logiciel nécessite une configuration initiale (ID manquant, etc.).
    Unconfigured(String),
    /// Le moteur écoute les événements système (Inotify) sans activité réseau.
    Idle,
    /// Un scan global est en cours d'exécution.
    ScanProgress {
        phase: ScanPhase,
        done: usize,
        total: usize,
        current: String,
    },
    /// Une synchronisation (upload/download) est en cours avec statistiques de bande passante.
    SyncProgress(bandwidth::ProgressSnapshot),
    /// Des transferts sont actifs en arrière-plan.
    Syncing { active: usize },
    /// Le moteur a été suspendu par l'utilisateur.
    Paused,
    /// Une erreur critique ou réseau est survenue.
    Error(String),
    /// Le moteur est complètement arrêté.
    Stopped,
    /// La fenêtre des paramètres est actuellement ouverte.
    Settings,
    /// La fenêtre d'aide est actuellement ouverte.
    Help,
}

/// Les différentes étapes d'une analyse complète du système de fichiers.
#[derive(Debug, Clone)]
pub enum ScanPhase {
    /// Récupération de l'index complet depuis l'API Google Drive.
    RemoteListing,
    /// Parcours du disque dur local.
    LocalListing,
    /// Création de l'arborescence des dossiers manquants sur le cloud.
    Directories,
    /// Comparaison des empreintes locales, distantes et de la base de données.
    Comparing,
}

/// Les tâches unitaires traitées de manière asynchrone par les workers.
#[derive(Debug, Clone)]
pub(crate) enum Task {
    /// Synchronise (uploade ou met à jour) un fichier spécifique.
    SyncFile { path: PathBuf },
    /// Supprime un élément distant pour refléter une suppression locale.
    Delete(PathBuf),
    /// Renomme ou déplace un fichier distant.
    Rename { from: PathBuf, to: PathBuf },
}

// ── Contexte Global ───────────────────────────────────────────────────────────

// ── Contexte Global ───────────────────────────────────────────────────────────

/// Contexte partagé regroupant toutes les dépendances vitales du moteur de synchronisation.
///
/// L'utilisation de ce pattern (Context Object) évite l'anti-pattern "Long Parameter List"
/// dans l'orchestration des tâches asynchrones (`scan::run`, `worker::handle`) et
/// facilite considérablement l'injection de dépendances pour les tests unitaires.
#[derive(Clone)]
pub(crate) struct EngineContext {
    /// Configuration de l'application, partagée de manière thread-safe (mise à jour à chaud possible).
    pub cfg: Arc<AppConfig>,

    /// Interface avec SQLite pour persister l'état local, l'indexation et la file d'attente hors-ligne.
    pub db: Database,

    /// Client de communication avec le stockage distant (ex: Google Drive), abstrait pour le mocking.
    pub provider: Arc<dyn RemoteProvider>,

    /// Cache en mémoire ultra-rapide pour la résolution des chemins (Path ↔ Drive ID).
    pub path_cache: Arc<PathCache>,

    /// Sonde télémétrique alimentant l'interface graphique en temps réel (bande passante, fichiers restants).
    pub tracker: Arc<ProgressTracker>,

    /// Jeton d'interruption global garantissant l'arrêt gracieux et immédiat de toutes les coroutines.
    pub shutdown: CancellationToken,

    /// Si `true`, le moteur tourne à vide : aucune écriture réseau ou locale n'est effectuée (Mode Simulation).
    pub dry_run: bool,
}
// ── SyncEngine ────────────────────────────────────────────────────────────────

pub struct SyncEngine {
    pub dry_run: bool,
    cfg: Arc<AppConfig>,
}

macro_rules! await_scan_interruptible {
    ($scan_call:expr, $shutdown:expr, $cmd_rx:expr, $status_tx:expr, $tracker:expr, $paused:ident, $rescan_on_resume:ident) => {
        let mut fut = Box::pin($scan_call);
        loop {
            tokio::select! {
                r = &mut fut => {
                    if let Err(e) = r {
                        if crate::engine::is_shutdown_err(&e) { $shutdown.cancel(); }
                        else { let _ = $status_tx.send(EngineStatus::Error(e.to_string())); }
                    } else if $tracker.total_files.load(std::sync::atomic::Ordering::Relaxed) == 0 {
                        let _ = $status_tx.send(EngineStatus::Idle);
                    }
                    break;
                }
                _ = $shutdown.cancelled() => { break; }
                maybe_cmd = $cmd_rx.recv() => {
                    match maybe_cmd {
                        Some(EngineCommand::Pause) => {
                            $paused = true;
                            $rescan_on_resume = true; // Pour reprendre plus tard
                            let _ = $status_tx.send(EngineStatus::Paused);
                            break; // Coupe le scan instantanément !
                        }
                        Some(EngineCommand::OpenSettings) => {
                            $paused = true;
                            $rescan_on_resume = true;
                            let _ = $status_tx.send(EngineStatus::Settings);
                            break; // Coupe le scan instantanément !
                        }
                        Some(EngineCommand::Shutdown) | None => {
                            $shutdown.cancel();
                            break;
                        }
                        _ => {} // On ignore les autres commandes (ex : ForceScan en double).
                    }
                }
            }
        }
    };
}

impl SyncEngine {
    pub fn new(cfg: Arc<AppConfig>, dry_run: bool) -> Self {
        Self { dry_run,cfg }
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

        let _ = status_tx.send(EngineStatus::Starting(25));
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        let auth = Arc::new(GoogleAuth::new());
        let path_cache = Arc::new(PathCache::new());
        let config_arc = Arc::new(self.cfg.advanced.clone());

        let _ = status_tx.send(EngineStatus::Starting(50));
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        let provider: Arc<dyn RemoteProvider> = Arc::new(GDriveProvider::new(
            auth,
            path_cache.clone(),
            config_arc,
            shutdown.clone(),
        )?);

        let _ = status_tx.send(EngineStatus::Starting(75));
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        self.run_with_provider(provider, path_cache, db, shutdown, cmd_rx, status_tx)
            .await
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
        let primary = self
            .cfg
            .get_primary_pair()
            .context("Aucun dossier n'est configuré!")?;
        let ignore = IgnoreMatcher::from_patterns(&primary.ignore_patterns)?;

        let mut paused = false;
        let mut rescan_on_resume = false;
        let tracker = Arc::new(ProgressTracker::new());

        let is_offline = Arc::new(AtomicBool::new(false));

        let _ = status_tx.send(EngineStatus::Starting(100));
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let ctx = EngineContext{
            cfg: self.cfg.clone(),
            db: db.clone(),
            provider: provider.clone(),
            path_cache: path_cache.clone(),
            tracker: tracker.clone(),
            shutdown: shutdown.clone(),
            dry_run: self.dry_run,
        };
        {
            let _ = status_tx.send(EngineStatus::Syncing { active: 0 });
            let scan = scan::run(
                &ctx,
                &ignore,
                &task_tx,
                &status_tx,
            );
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
                            Some(EngineCommand::OpenSettings) => {
                                info!("engine: settings opened during initial scan");
                                paused = true;
                                rescan_on_resume = true; // Il faudra reprendre le scan après
                                let _ = status_tx.send(EngineStatus::Settings);
                                break;
                            }
                            Some(EngineCommand::OpenHelp) => {
                                info!("engine: help opened during initial scan");
                                paused = true;
                                rescan_on_resume = true; // Il faudra reprendre le scan après
                                let _ = status_tx.send(EngineStatus::Help);
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
                                warn!(error = %e, "initial scan failed, assuming OFFLINE and continuing with watcher");
                                is_offline.store(true, Ordering::Relaxed);
                                let _ = status_tx.send(EngineStatus::Error("Mode Hors-ligne (Scan initial échoué)".to_string()));
                            }
                        }
                        break;
                    }
                }
            }
        }

        if !paused && !is_offline.load(Ordering::Relaxed) {
            let _ = status_tx.send(EngineStatus::Idle);
        }

        let (watch_tx, watch_rx) = mpsc::channel(256);
        let primary = self
            .cfg
            .get_primary_pair()
            .context("Aucun dossier n'est configuré!")?;

        let mut watcher = watcher::Watcher::start(&primary.local_path, watch_tx)?;

        let task_tx_w = task_tx.clone();
        let sd_w = shutdown.clone();

        // ── VARIABLE DYNAMIQUE POUR LE HOT-RELOAD ──
        let mut current_local_root = primary.local_path.clone();

        spawn_debounced_dispatch(
            watch_rx,
            task_tx_w,
            sd_w,
            self.cfg.advanced.debounce_ms,
            tracker.clone(),
            is_offline.clone(),
            db.clone(),
            current_local_root.clone(),
        );

        let sem = Arc::new(Semaphore::new(self.cfg.max_workers.max(1)));
        let active = Arc::new(AtomicUsize::new(0));

        tokio::spawn(progress_publisher(
            tracker.clone(),
            status_tx.clone(),
            shutdown.clone(),
        ));

        let mut health_tick = tokio::time::interval_at(
            tokio::time::Instant::now()
                + std::time::Duration::from_secs(self.cfg.advanced.health_check_interval_secs),
            std::time::Duration::from_secs(self.cfg.advanced.health_check_interval_secs),
        );

        let rescan_secs = self.cfg.rescan_interval_min.saturating_mul(60);
        let mut rescan_tick = tokio::time::interval_at(
            tokio::time::Instant::now() + std::time::Duration::from_secs(rescan_secs.max(60)),
            std::time::Duration::from_secs(rescan_secs.max(60)),
        );
        let rescan_enabled = self.cfg.rescan_interval_min > 0;

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
                                if rescan_on_resume && !is_offline.load(Ordering::Relaxed) {
                                    rescan_on_resume = false;
                                    info!("engine: rescan after config change");
                                    tracker.total_files.store(0, Ordering::Relaxed);
                                    let _ = status_tx.send(EngineStatus::Syncing { active: 0 });

                                    let current_primary = self.cfg.get_primary_pair().context("Aucun dossier")?;
                                    let ig = IgnoreMatcher::from_patterns(&current_primary.ignore_patterns)?;

                                    await_scan_interruptible!(
                                        scan::run(&ctx,&ig, &task_tx, &status_tx),
                                        shutdown, cmd_rx, status_tx, tracker, paused, rescan_on_resume
                                    );
                                }
                                let _ = status_tx.send(EngineStatus::Idle);
                            }
                            Some(EngineCommand::Shutdown) | None => {
                                shutdown.cancel();
                                break;
                            }
                            Some(EngineCommand::ApplyConfig(new_cfg)) => {
                                let new_primary = new_cfg.get_primary_pair().context("Aucun dossier n'est configuré!")?;
                                let root_changed = new_primary.local_path != current_local_root;

                                // On met à jour la vraie configuration du moteur
                                self.cfg = new_cfg.clone();
                                rescan_on_resume = true;

                                if root_changed {
                                    current_local_root = new_primary.local_path.clone();
                                    watcher.stop();
                                    db.clear()?;
                                    db.clear_dirs()?;
                                    let (tx2, rx2) = mpsc::channel(256);
                                    watcher = watcher::Watcher::start(&current_local_root, tx2)?;
                                    spawn_debounced_dispatch(
                                        rx2, task_tx.clone(), shutdown.clone(), self.cfg.advanced.debounce_ms,
                                        tracker.clone(), is_offline.clone(), db.clone(), current_local_root.clone()
                                    );
                                }
                            }
                            Some(EngineCommand::OpenSettings) => {
                                // Le moteur est déjà en pause, on met juste à jour l'état visuel
                                let _ = status_tx.send(EngineStatus::Settings);
                            }
                            Some(EngineCommand::OpenHelp) => {
                                let _ = status_tx.send(EngineStatus::Help);
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
                            shutdown.cancel();
                            break;
                        }
                        Some(EngineCommand::Pause) => {
                            paused = true;
                            let _ = status_tx.send(EngineStatus::Paused);
                        }
                        Some(EngineCommand::Resume) => {}
                        Some(EngineCommand::ForceScan) => {
                            if !is_offline.load(Ordering::Relaxed) {
                                tracker.total_files.store(0, Ordering::Relaxed);
                                let _ = status_tx.send(EngineStatus::Syncing { active: 0 });

                                let current_primary = self.cfg.get_primary_pair().context("Aucun dossier")?;
                                let ignore2 = IgnoreMatcher::from_patterns(&current_primary.ignore_patterns)?;

                                await_scan_interruptible!(
                                    scan::run(&ctx, &ignore2, &task_tx, &status_tx),
                                    shutdown, cmd_rx, status_tx, tracker, paused, rescan_on_resume
                                );
                            }
                        }
                        Some(EngineCommand::ApplyConfig(new_cfg)) => {
                            let new_primary = new_cfg.get_primary_pair().context("Pas de dossier")?;
                            let root_changed = new_primary.local_path != current_local_root;

                            // On met à jour la vraie configuration du moteur
                            self.cfg = new_cfg.clone();

                            if root_changed {
                                current_local_root = new_primary.local_path.clone();
                                watcher.stop();
                                db.clear()?;
                                db.clear_dirs()?;
                                let (tx2, rx2) = mpsc::channel(256);
                                watcher = watcher::Watcher::start(&current_local_root, tx2)?;
                                spawn_debounced_dispatch(
                                    rx2, task_tx.clone(), shutdown.clone(), self.cfg.advanced.debounce_ms,
                                    tracker.clone(), is_offline.clone(), db.clone(), current_local_root.clone()
                                );

                                if !is_offline.load(Ordering::Relaxed) {
                                    let ignore3 = IgnoreMatcher::from_patterns(&new_primary.ignore_patterns)?;
                                    tracker.total_files.store(0, Ordering::Relaxed);
                                    let _ = status_tx.send(EngineStatus::Syncing { active: 0 });
                                    await_scan_interruptible!(
                                        scan::run(&ctx, &ignore3, &task_tx, &status_tx),
                                        shutdown, cmd_rx, status_tx, tracker, paused, rescan_on_resume
                                    );
                                }
                            }
                            let _ = status_tx.send(EngineStatus::Idle);
                        }
                        Some(EngineCommand::OpenHelp) => {
                            paused = true;
                            let _ = status_tx.send(EngineStatus::Help);
                        }
                        Some(EngineCommand::OpenSettings) => {
                            paused = true;
                            let _ = status_tx.send(EngineStatus::Settings);
                        }
                    }
                }

                _ = health_tick.tick() => {
                    match provider.check_health().await {
                        Ok(HealthStatus::Ok { .. }) => {
                            if is_offline.swap(false, Ordering::Relaxed) {
                                info!("🌐 Connexion Internet rétablie !");
                                crate::notif::connection_restored(&self.cfg);
                                let _ = status_tx.send(EngineStatus::Idle);

                                if let Err(e) = offline::flush_queue(&db, &task_tx, &current_local_root).await {
                                    warn!("Erreur lors du flush de la file d'attente hors-ligne : {}", e);
                                }
                            }
                        }
                        _ => {
                            if !is_offline.swap(true, Ordering::Relaxed) {
                                warn!("⚠️ Connexion perdue. Passage en mode SURVIE (Hors-Ligne).");

                                crate::notif::connection_lost(&self.cfg);
                                let _ = status_tx.send(EngineStatus::Error("Réseau indisponible (Mode Hors-ligne)".into()));
                            }
                        }
                    }
                }

                _ = rescan_tick.tick(), if rescan_enabled => {
                    if !is_offline.load(Ordering::Relaxed) {
                        tracker.total_files.store(0, Ordering::Relaxed);
                        let _ = status_tx.send(EngineStatus::Syncing { active: 0 });

                        let current_primary = self.cfg.get_primary_pair().context("Aucun dossier")?;
                        let ignore_r = IgnoreMatcher::from_patterns(&current_primary.ignore_patterns)?;

                        tokio::select! {
                            r = scan::run(&ctx, &ignore_r, &task_tx, &status_tx) => {
                                if let Err(e) = r {
                                    if is_shutdown_err(&e) { shutdown.cancel(); break; }
                                    let _ = status_tx.send(EngineStatus::Error(e.to_string()));
                                }else if tracker.total_files.load(Ordering::Relaxed) == 0 {
                                    let _ = status_tx.send(EngineStatus::Idle);
                                }
                            }
                            _ = shutdown.cancelled() => { break; }
                        }
                    }
                }

                maybe_task = task_rx.recv() => {
                    let Some(task) = maybe_task else { break; };

                    let permit = tokio::select! {
                        p = sem.clone().acquire_owned() => p.context("semaphore closed")?,
                        _ = shutdown.cancelled() => { break; }
                    };

                    let ctx2= EngineContext{
                        cfg: self.cfg.clone(),
                        db: db.clone(),
                        provider: provider.clone(),
                        path_cache: path_cache.clone(),
                        tracker: tracker.clone(),
                        shutdown: shutdown.clone(),
                        dry_run: self.dry_run,
                    };

                    let stx2 = status_tx.clone();
                    let active2 = active.clone();

                    let current_primary = ctx2.cfg.get_primary_pair().context("Aucun dossier")?;
                    let ignore_pat = current_primary.ignore_patterns.clone();

                    active2.fetch_add(1, Ordering::Relaxed);

                    tokio::spawn(async move {
                       let _permit = permit;
                        if ctx2.shutdown.is_cancelled() {
                            ctx2.tracker.done_files.fetch_add(1, Ordering::Relaxed);
                            let prev = active2.fetch_sub(1, Ordering::Relaxed);
                            if prev == 1 { let _ = stx2.send(EngineStatus::Idle); }
                            return;
                        }

                        let ignore = IgnoreMatcher::from_patterns(&ignore_pat).unwrap();

                        match worker::handle(task.clone(), &ctx2, &ignore).await {
                            Ok(_) => {}
                            Err(e) => {
                                if !is_shutdown_err(&e) && !ctx2.shutdown.is_cancelled() {
                                    warn!("❌ ERREUR OUVRIER : {:?}", e);
                                    let _ = stx2.send(EngineStatus::Error(e.to_string()));
                                    if scan::is_quota_err(&e) {
                                        crate::notif::quota_exceeded(&ctx2.cfg);
                                    }
                                }
                            }
                        }

                        ctx2.tracker.done_files.fetch_add(1, Ordering::Relaxed);
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

/// Exception légitime pour éviter le God Object
#[allow(clippy::too_many_arguments)]
fn spawn_debounced_dispatch(
    mut watch_rx: mpsc::Receiver<watcher::WatchEvent>,
    task_tx: mpsc::Sender<Task>,
    shutdown: CancellationToken,
    debounce_ms: u64,
    tracker: Arc<ProgressTracker>,
    is_offline: Arc<AtomicBool>,
    db: Database,
    local_root: PathBuf,
) {
    use std::collections::HashMap;
    use tokio::time::{interval, Duration, Instant};
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

                        if is_offline.load(Ordering::Relaxed) {
                            if let Ok(rel) = path.strip_prefix(&local_root) {
                                let _ = db.push_offline_task("sync", &rel.to_string_lossy(), None);
                            }
                        } else {
                            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                            tracker.total_files.fetch_add(1, Ordering::Relaxed);
                            tracker.total_bytes.fetch_add(size, Ordering::Relaxed);
                            let task = Task::SyncFile { path};
                            if task_tx.send(task).await.is_err() { return; }
                        }
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
                            if is_offline.load(Ordering::Relaxed) {
                                if let Ok(rel) = p.strip_prefix(&local_root) {
                                    let _ = db.push_offline_task("delete", &rel.to_string_lossy(), None);
                                }
                            } else {
                                tracker.total_files.fetch_add(1, Ordering::Relaxed);
                                let task = Task::Delete(p);
                                if task_tx.send(task).await.is_err() { return; }
                            }
                        }
                        WatchEvent::Renamed { from, to } => {
                            pending.remove(&from);
                            pending.remove(&to);
                            if is_offline.load(Ordering::Relaxed) {
                                if let (Ok(rel_from), Ok(rel_to)) = (from.strip_prefix(&local_root), to.strip_prefix(&local_root)) {
                                    let _ = db.push_offline_task("rename", &rel_to.to_string_lossy(), Some(&rel_from.to_string_lossy()));
                                }
                            } else {
                                tracker.total_files.fetch_add(1, Ordering::Relaxed);
                                let task = Task::Rename { from, to };
                                if task_tx.send(task).await.is_err() { return; }
                            }
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
                        let primary = cfg.get_primary_pair().context("Aucun dossier n'est configuré!")?;
                        match cfg.validate() {
                            Err(e) => {
                                warn!(reason = %e, "config rejected: still invalid");
                                let _ = status_tx.send(EngineStatus::Unconfigured(e.to_string()));
                            }
                            Ok(()) => {
                                info!(local = %primary.local_path.display(), "valid config received, starting engine");
                                let engine = SyncEngine::new(cfg,false);
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

pub(crate) async fn progress_publisher(
    tracker: Arc<ProgressTracker>,
    status_tx: mpsc::UnboundedSender<EngineStatus>,
    shutdown: CancellationToken,
) {
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(200));
    loop {
        tokio::select! {
            _ = interval.tick() => {
                let snap = tracker.snapshot();
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
    use crate::config::AppConfig;
    use std::path::PathBuf;
    use tempfile::NamedTempFile;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    fn test_db() -> Database {
        let f = NamedTempFile::new().unwrap();
        let db = Database::open(f.path()).unwrap();
        db.init_and_migrate().unwrap();
        db
    }

    #[tokio::test]
    async fn test_main_uses_config_channel_capacity() {
        let cfg = AppConfig::default();
        let capacity = cfg.advanced.engine_channel_capacity;
        let (tx, _rx) = mpsc::channel::<usize>(capacity);
        for i in 0..capacity {
            assert!(tx.try_send(i).is_ok());
        }
        assert!(tx.try_send(capacity).is_err());
    }

    #[tokio::test]
    async fn test_debounce_zero_means_immediate() {
        let (watch_tx, watch_rx) = mpsc::channel(10);
        let (task_tx, mut task_rx) = mpsc::channel(10);
        let shutdown = CancellationToken::new();
        let tracker = Arc::new(ProgressTracker::new());
        let is_offline = Arc::new(AtomicBool::new(false));

        spawn_debounced_dispatch(
            watch_rx,
            task_tx,
            shutdown.clone(),
            0,
            tracker,
            is_offline,
            test_db(),
            PathBuf::from("/"),
        );

        let start = tokio::time::Instant::now();
        watch_tx
            .send(watcher::WatchEvent::Modified(PathBuf::from("zero.txt")))
            .await
            .unwrap();
        let _ = task_rx.recv().await.unwrap();
        let elapsed = start.elapsed().as_millis() as u64;

        assert!(elapsed < 50);
    }

    #[tokio::test]
    async fn test_engine_uses_config_debounce() {
        let (watch_tx, watch_rx) = mpsc::channel(10);
        let (task_tx, mut task_rx) = mpsc::channel(10);
        let shutdown = CancellationToken::new();

        let cfg = AppConfig::default();
        let configured_debounce = cfg.advanced.debounce_ms;
        let tracker = Arc::new(ProgressTracker::new());
        let is_offline = Arc::new(AtomicBool::new(false));

        spawn_debounced_dispatch(
            watch_rx,
            task_tx,
            shutdown.clone(),
            configured_debounce,
            tracker,
            is_offline,
            test_db(),
            PathBuf::from("/"),
        );

        let start = tokio::time::Instant::now();
        watch_tx
            .send(watcher::WatchEvent::Modified(PathBuf::from("config.txt")))
            .await
            .unwrap();
        let _ = task_rx.recv().await.unwrap();
        let elapsed = start.elapsed().as_millis() as u64;

        assert!(elapsed >= configured_debounce);
    }

    #[tokio::test]
    async fn test_progress_publisher_throttle() {
        let (status_tx, mut status_rx) = mpsc::unbounded_channel();
        let shutdown = CancellationToken::new();
        let tracker = Arc::new(ProgressTracker::new());

        tracker.total_files.store(10, Ordering::Relaxed);
        tracker.done_files.store(5, Ordering::Relaxed);

        tokio::spawn(progress_publisher(
            tracker.clone(),
            status_tx,
            shutdown.clone(),
        ));

        for _ in 0..1000 {
            tracker.record_bytes(1024);
        }

        tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;
        shutdown.cancel();

        let mut count = 0;
        while let Ok(msg) = status_rx.try_recv() {
            if let EngineStatus::SyncProgress(_) = msg {
                count += 1;
            }
        }

        assert!(count >= 1);
        assert!(count <= 2);
    }

    #[tokio::test]
    async fn test_offline_online_cycle() {
        let db = test_db();
        let (task_tx, mut task_rx) = mpsc::channel(10);

        // 1. Simuler le mode Survie : on ajoute des tâches dans la base hors-ligne
        db.push_offline_task("sync", "fichier_train.txt", None)
            .unwrap();
        db.push_offline_task("delete", "vieux_brouillon.txt", None)
            .unwrap();
        db.push_offline_task("rename", "nouveau_nom.txt", Some("ancien_nom.txt"))
            .unwrap();

        // 2. Simuler le retour de la connexion : on appelle le flush avec un root local fictif
        offline::flush_queue(&db, &task_tx, std::path::Path::new(""))
            .await
            .unwrap();

        // 3. Vérifier que les tâches ont bien été réinjectées dans le circuit principal
        let task1 = task_rx.recv().await.unwrap();
        match task1 {
            // Plus de flag force ici !
            Task::SyncFile { path } => assert_eq!(path.to_string_lossy(), "fichier_train.txt"),
            _ => panic!("Tâche 1 inattendue"),
        }

        let task2 = task_rx.recv().await.unwrap();
        match task2 {
            Task::Delete(path) => assert_eq!(path.to_string_lossy(), "vieux_brouillon.txt"),
            _ => panic!("Tâche 2 inattendue"),
        }

        let task3 = task_rx.recv().await.unwrap();
        match task3 {
            Task::Rename { from, to } => {
                assert_eq!(from.to_string_lossy(), "ancien_nom.txt");
                assert_eq!(to.to_string_lossy(), "nouveau_nom.txt");
            }
            _ => panic!("Tâche 3 inattendue"),
        }

        // 4. Vérifier que l'estomac SQLite est totalement vide après le traitement
        let remaining = db.get_offline_tasks().unwrap();
        assert!(
            remaining.is_empty(),
            "La base de données devrait être vide après le flush"
        );
    }
}
