use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use sync_g_drive::config::AppConfig;
use sync_g_drive::db::Database;
use sync_g_drive::engine::{EngineCommand, EngineStatus, SyncEngine};

#[tokio::main]
async fn main() -> Result<()> {
    // ── Instance unique (File Lock POSIX) ─────────────────────────────────
    let _lock = acquire_instance_lock();

    let (cfg, is_first_run) = AppConfig::load_or_create().context("cannot load config")?;
    let needs_config = cfg.validate().is_err();

    let log_dir = log_dir();
    cleanup_old_logs(&log_dir, 7);
    let _log_guard = init_logging(&log_dir)?;

    if needs_config {
        warn!(reason = %cfg.validate().unwrap_err(), "config invalide — ouvrez Settings");
    } else {
        info!(local = %cfg.local_root.display(), remote = %cfg.remote_root, "SyncGDrive démarrage");
    }

    let db = Database::open(std::path::Path::new(&db_path())).context("cannot open db")?;
    db.init_schema()?;

    let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(32);
    let (status_tx, status_rx) = mpsc::unbounded_channel::<EngineStatus>();
    let shutdown = CancellationToken::new();

    // Statut initial pour le systray (§1 UX_SYSTRAY.md : icône "Démarrage").
    let _ = status_tx.send(EngineStatus::Starting);

    // ── Signal SIGINT/SIGTERM via self-pipe trick ─────────────────────────────
    // Technique POSIX classique : le handler de signal écrit 1 octet dans un pipe.
    // Tokio lit ce pipe de façon async — pas de conflit avec ksni ou GTK.
    let signal_fd = {
        let mut fds = [0i32; 2];
        unsafe {
            libc::pipe(fds.as_mut_ptr());
            libc::fcntl(fds[1], libc::F_SETFL, libc::O_NONBLOCK);
        }
        // Stocke le write-end dans une variable statique pour le handler
        SIGNAL_PIPE_WRITE.store(fds[1], std::sync::atomic::Ordering::SeqCst);
        unsafe {
            libc::signal(libc::SIGINT,  signal_handler as *const () as libc::sighandler_t);
            libc::signal(libc::SIGTERM, signal_handler as *const () as libc::sighandler_t);
        }
        fds[0] // read-end
    };

    // ── Moteur ────────────────────────────────────────────────────────────────
    let engine = if needs_config {
        let reason = cfg.validate().unwrap_err().to_string();
        let _ = status_tx.send(EngineStatus::Unconfigured(reason));
        tokio::spawn(sync_g_drive::engine::run_unconfigured(
            db, shutdown.clone(), cmd_rx, status_tx,
        ))
    } else {
        tokio::spawn(SyncEngine::new(cfg.clone()).run(
            db, shutdown.clone(), cmd_rx, status_tx,
        ))
    };

    // ── Systray ksni ─────────────────────────────────────────────────────────
    #[cfg(feature = "ui")]
    {
        sync_g_drive::ui::spawn_tray(
            cmd_tx.clone(),
            status_rx,
            std::sync::Arc::new(std::sync::Mutex::new(cfg)),
            is_first_run || needs_config,
            shutdown.clone(),
            log_dir,
        )?;
    }
    #[cfg(not(feature = "ui"))]
    {
        // Sans UI : drainer les status pour ne pas accumuler en mémoire.
        let _ = (cfg, log_dir, is_first_run);
        tokio::spawn(async move {
            let mut status_rx = status_rx;
            while (status_rx.recv().await).is_some() {}
        });
    }

    // ── Attente shutdown : pipe signal OU cancel depuis l'UI ─────────────────
    let async_fd = tokio::io::unix::AsyncFd::new(signal_fd)
        .context("AsyncFd on signal pipe")?;
    tokio::select! {
        _ = async_fd.readable() => {
            info!("signal reçu, arrêt…");
            shutdown.cancel();
        }
        _ = shutdown.cancelled() => {}
    }
    unsafe { libc::close(signal_fd); }

    let _ = cmd_tx.send(EngineCommand::Shutdown).await;

    // Attend max 3s que le moteur finisse proprement, puis sort de toute façon.
    tokio::select! {
        _ = engine => { info!("moteur arrêté proprement"); }
        _ = tokio::time::sleep(std::time::Duration::from_secs(3)) => {
            info!("timeout 3s dépassé — sortie forcée");
        }
    }

    info!("SyncGDrive arrêté proprement");
    Ok(())
}

// ── Self-pipe : write-end stocké en statique atomique ────────────────────────
static SIGNAL_PIPE_WRITE: std::sync::atomic::AtomicI32 =
    std::sync::atomic::AtomicI32::new(-1);

extern "C" fn signal_handler(_sig: libc::c_int) {
    let fd = SIGNAL_PIPE_WRITE.load(std::sync::atomic::Ordering::SeqCst);
    if fd >= 0 {
        unsafe { libc::write(fd, c"".as_ptr() as *const libc::c_void, 1); }
    }
}

// ── Instance unique via flock POSIX ──────────────────────────────────────────

/// Acquiert un verrou exclusif sur `$XDG_RUNTIME_DIR/syncgdrive.lock`.
/// Si une autre instance tourne déjà → notification + exit(0).
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
        .expect("cannot open lock file");

    let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if ret != 0 {
        eprintln!("SyncGDrive est déjà en cours d'exécution.");

        // CORRECTION : notify-rust utilise zbus::block_on() en interne.
        // Sous #[tokio::main] le runtime est déjà actif → panic
        // "Cannot start a runtime from within a runtime".
        // On isole l'appel D-Bus dans un thread OS classique.
        std::thread::spawn(|| {
            let _ = notify_rust::Notification::new()
                .appname("SyncGDrive")
                .summary("SyncGDrive")
                .body("Une instance est déjà en cours d'exécution.")
                .icon("dialog-information")
                .timeout(4000)
                .show();
        }).join().unwrap();

        std::process::exit(0);
    }

    // Écrit le PID dans le fichier lock (convention daemon POSIX).
    // Tronque d'abord pour effacer un ancien PID plus long, puis écrit.
    use std::io::Write;
    file.set_len(0).ok();
    let _ = write!(file, "{}", std::process::id());
    let _ = file.flush();

    file
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn db_path() -> String {
    let dir = xdg_dir("XDG_DATA_HOME", ".local/share").join("syncgdrive");
    std::fs::create_dir_all(&dir).ok();
    dir.join("index.db").to_string_lossy().into_owned()
}

/// Retourne le RÉPERTOIRE de logs (pas un fichier).
/// Les fichiers sont créés par `tracing_appender::rolling::daily`.
fn log_dir() -> std::path::PathBuf {
    let dir = xdg_dir("XDG_STATE_HOME", ".local/state").join("syncgdrive").join("logs");
    std::fs::create_dir_all(&dir).ok();
    dir
}

fn xdg_dir(env: &str, fallback: &str) -> std::path::PathBuf {
    std::env::var(env).map(std::path::PathBuf::from).unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        std::path::PathBuf::from(home).join(fallback)
    })
}

/// Supprime les fichiers de log > max_days dans le répertoire de logs.
fn cleanup_old_logs(log_dir: &std::path::Path, max_days: u64) {
    let cutoff = std::time::SystemTime::now()
        - std::time::Duration::from_secs(max_days * 24 * 3600);
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

fn init_logging(log_dir: &std::path::Path) -> Result<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,zbus=warn,globset=warn,glib=warn"));

    let timer = time::format_description::parse("[hour]:[minute]:[second]").expect("time fmt");
    let timer = tracing_subscriber::fmt::time::UtcTime::new(timer);

    let stdout = fmt::layer().with_target(false).with_timer(timer.clone()).compact();

    // Rotation quotidienne : syncgdrive.log.2026-03-13, etc.
    let (writer, guard) = tracing_appender::non_blocking(
        tracing_appender::rolling::daily(log_dir, "syncgdrive.log")
    );
    let file_layer = fmt::layer().with_target(true).with_timer(timer).with_ansi(false).with_writer(writer);

    tracing_subscriber::registry()
        .with(filter).with(stdout).with(file_layer)
        .try_init().context("cannot init tracing")?;

    Ok(guard)
}