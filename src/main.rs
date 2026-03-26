//! Point d'entrée principal de l'application SyncGDrive.
//!
//! Ce binaire orchestre le démarrage de l'application en plusieurs phases :
//! 1. Sécurisation (Verrou d'instance unique).
//! 2. Initialisation des outils (Logs, Variables d'environnement).
//! 3. Migration (Configuration et Base de données via `migration.rs`).
//! 4. Démarrage des processus concurrents (Serveur UI GTK4, Moteur Tokio).
//! 5. Gestion de l'arrêt gracieux (Interception des signaux SIGINT/SIGTERM).

use anyhow::{Context, Result};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use sync_g_drive::db::Database;
use sync_g_drive::engine::{EngineCommand, EngineStatus, SyncEngine};
use sync_g_drive::migration;

#[tokio::main]
async fn main() -> Result<()> {
    // ── Phase 0 : Variables d'environnement ───────────────────────────────
    let env_path = sync_g_drive::config::config_dir().join(".env");
    let _ = dotenvy::from_path(&env_path); // On ignore l'erreur si le fichier n'existe pas

    let dry_run = std::env::var("SYNCGDRIVE_DRY_RUN")
        .map(|v| v == "1" || v.to_lowercase() == "true")
        .unwrap_or(false);

    // ── Phase 1 : Sécurité et Logs ─────────────────────────────────────────

    // Empêche le lancement de deux instances simultanées (File Lock POSIX)
    let _lock = acquire_instance_lock();

    let log_dir = log_dir();
    let _log_guard = init_logging(&log_dir)?;

    if dry_run {
        warn!("🛡️ MODE DRY-RUN ACTIVÉ : Simulation uniquement, aucune modification ne sera appliquée.");
    }

    // Auto-détection de la session OAuth2
    let auth = sync_g_drive::auth::GoogleAuth::new();
    match auth.get_valid_token().await {
        Ok(msg) => info!("✅ Google Drive : {}", msg),
        Err(e) => warn!("⚠️ Mode déconnecté : {}", e),
    }

    // ── Phase 2 : Migration & Configuration ───────────────────────────────

    let config_path = sync_g_drive::config::config_path();
    let db_path_str = db_path();
    let db_path_buf = std::path::Path::new(&db_path_str);

    // L'orchestrateur de migration gère la mise à jour V1 → V2 en toute sécurité
    let cfg = migration::run_all_migrations(&config_path)
        .context("Échec lors de la migration ou du chargement de la configuration")?;

    let is_first_run = !config_path.exists();
    let needs_config = cfg.validate().is_err();

    if needs_config {
        if let Err(e) = cfg.validate() {
            warn!(reason = %e, "Configuration invalide — Ouverture des réglages requise");
        }
    } else {
        let primary = cfg.get_primary_pair().context("Pas de dossier configuré")?;
        info!(
            local = %primary.local_path.display(),
            remote_id = %primary.remote_folder_id,
            "SyncGDrive V2 démarrage"
        );
    }

    // Nettoyage asynchrone des anciens fichiers de logs
    cleanup_old_logs(&log_dir, cfg.advanced.log_retention_days);

    // La base de données a été migrée, on ouvre simplement la connexion
    let db = Database::open(db_path_buf).context("Impossible d'ouvrir la base de données")?;

    // ── Phase 3 : Canaux de communication ─────────────────────────────────

    let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(cfg.advanced.engine_channel_capacity);
    let (status_tx, status_rx) = mpsc::unbounded_channel::<EngineStatus>();
    let shutdown = CancellationToken::new();

    let _ = status_tx.send(EngineStatus::Starting(0));

    // Préparation de l'interception des signaux (Ctrl+C, kill)
    let signal_fd = setup_signal_pipe();

    // Démarrage du thread d'interface graphique (GTK4)
    #[cfg(feature = "ui")]
    let ui_tx = sync_g_drive::ui::start_ui_server(cmd_tx.clone());

    // ── Phase 4 : Démarrage du moteur de synchronisation ──────────────────

    let engine = if needs_config {
        let reason = cfg.validate().unwrap_err().to_string();
        let _ = status_tx.send(EngineStatus::Unconfigured(reason));
        tokio::spawn(sync_g_drive::engine::run_unconfigured(
            db,
            shutdown.clone(),
            cmd_rx,
            status_tx,
        ))
    } else {
        tokio::spawn(SyncEngine::new(Arc::from(cfg.clone()), dry_run).run(
            db,
            shutdown.clone(),
            cmd_rx,
            status_tx,
        ))
    };

    // ── Phase 5 : Démarrage du Systray ────────────────────────────────────

    let shutdown_timeout = cfg.advanced.shutdown_timeout_secs;

    #[cfg(feature = "ui")]
    {
        sync_g_drive::ui::spawn_tray(
            cmd_tx.clone(),
            status_rx,
            std::sync::Arc::new(std::sync::Mutex::new(cfg)),
            is_first_run || needs_config,
            shutdown.clone(),
            log_dir,
            ui_tx,
            dry_run,
        )?;
    }

    #[cfg(not(feature = "ui"))]
    {
        let _ = (cfg.clone(), log_dir, is_first_run);
        tokio::spawn(async move {
            let mut status_rx = status_rx;
            while status_rx.recv().await.is_some() {}
        });
    }

    // ── Phase 6 : Attente d'un signal d'arrêt ─────────────────────────────

    let async_fd = tokio::io::unix::AsyncFd::new(signal_fd).context("Erreur sur le descripteur asynchrone des signaux")?;

    tokio::select! {
        _ = async_fd.readable() => {
            info!("Signal d'arrêt reçu (SIGINT/SIGTERM), fermeture en cours...");
            shutdown.cancel();
        }
        _ = shutdown.cancelled() => {}
    }

    unsafe {
        libc::close(signal_fd);
    }

    let _ = cmd_tx.send(EngineCommand::Shutdown).await;

    // Laisse le temps au moteur de terminer ses uploads/requêtes en cours
    tokio::select! {
        _ = engine => { info!("Moteur arrêté proprement."); }
        _ = tokio::time::sleep(std::time::Duration::from_secs(shutdown_timeout)) => {
            warn!("Délai d'arrêt ({}s) dépassé — sortie forcée.", shutdown_timeout);
        }
    }

    info!("SyncGDrive arrêté.");
    std::process::exit(0);
}

// ── Interception des signaux OS (Self-pipe trick) ────────────────────────────

static SIGNAL_PIPE_WRITE: AtomicI32 = AtomicI32::new(-1);

extern "C" fn signal_handler(_sig: libc::c_int) {
    let fd = SIGNAL_PIPE_WRITE.load(Ordering::SeqCst);
    if fd >= 0 {
        unsafe {
            libc::write(fd, c"".as_ptr() as *const libc::c_void, 1);
        }
    }
}

/// Prépare un tube (pipe) non-bloquant pour capturer les signaux POSIX de 
/// manière sûre et les réinjecter dans la boucle d'événements asynchrone de Tokio.
fn setup_signal_pipe() -> i32 {
    let mut fds = [0i32; 2];
    unsafe {
        libc::pipe(fds.as_mut_ptr());
        libc::fcntl(fds[1], libc::F_SETFL, libc::O_NONBLOCK);
    }
    SIGNAL_PIPE_WRITE.store(fds[1], Ordering::SeqCst);
    unsafe {
        libc::signal(libc::SIGINT, signal_handler as *const () as usize);
        libc::signal(libc::SIGTERM, signal_handler as *const () as usize);
    }
    fds[0]
}

// ── Instance unique via flock POSIX ──────────────────────────────────────────

/// Tente d'acquérir un verrou exclusif sur le système pour s'assurer
/// qu'un seul processus SyncGDrive tourne à la fois.
fn acquire_instance_lock() -> std::fs::File {
    use std::os::unix::io::AsRawFd;

    let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
        .unwrap_or_else(|_| format!("/run/user/{}", unsafe { libc::getuid() }));
    let lock_path = std::path::PathBuf::from(runtime_dir).join("syncgdrive.lock");

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .expect("Impossible d'ouvrir le fichier de verrou");

    let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if ret != 0 {
        eprintln!("SyncGDrive est déjà en cours d'exécution.");

        std::thread::spawn(|| {
            let _ = notify_rust::Notification::new()
                .appname("SyncGDrive")
                .summary("SyncGDrive")
                .body("Une instance est déjà en cours d'exécution.")
                .icon("dialog-information")
                .timeout(4000)
                .show();
        })
            .join()
            .unwrap();

        std::process::exit(0);
    }

    use std::io::Write;
    file.set_len(0).ok();
    let _ = write!(file, "{}", std::process::id());
    let _ = file.flush();

    file
}

// ── Helpers Chemins et Dossiers ──────────────────────────────────────────────

fn db_path() -> String {
    let dir = xdg_dir("XDG_DATA_HOME", ".local/share").join("syncgdrive");
    std::fs::create_dir_all(&dir).ok();
    dir.join("index.db").to_string_lossy().into_owned()
}

fn log_dir() -> std::path::PathBuf {
    let dir = xdg_dir("XDG_STATE_HOME", ".local/state")
        .join("syncgdrive")
        .join("logs");
    std::fs::create_dir_all(&dir).ok();
    dir
}

fn xdg_dir(env: &str, fallback: &str) -> std::path::PathBuf {
    std::env::var(env)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            std::path::PathBuf::from(home).join(fallback)
        })
}

// ── Helpers Logs ─────────────────────────────────────────────────────────────

/// Supprime les fichiers de log plus anciens que `max_days` configuré.
fn cleanup_old_logs(log_dir: &std::path::Path, max_days: u64) {
    let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(max_days * 24 * 3600);
    let entries = match std::fs::read_dir(log_dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        if let Ok(meta) = entry.metadata() {
            if let Ok(modified) = meta.modified() {
                if modified < cutoff {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }
}

/// Prépare le système de traces (logs) asynchrones.
fn init_logging(log_dir: &std::path::Path) -> Result<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,zbus=warn,globset=warn,glib=warn"));

    // Format de temps précis
    let timer = time::format_description::parse("[hour]:[minute]:[second]").expect("Erreur de parsing du format temporel");
    let timer = fmt::time::UtcTime::new(timer);

    let stdout = fmt::layer()
        .with_target(false)
        .with_timer(timer.clone())
        .compact();

    // Rotation quotidienne : syncgdrive.log.2026-03-x
    let (writer, guard) =
        tracing_appender::non_blocking(tracing_appender::rolling::daily(log_dir, "syncgdrive.log"));

    let file_layer = fmt::layer()
        .with_target(true)
        .with_ansi(false)
        .with_writer(writer);

    tracing_subscriber::registry()
        .with(filter)
        .with(stdout)
        .with(file_layer)
        .try_init()
        .context("Impossible d'initialiser le système de logging")?;

    Ok(guard)
}