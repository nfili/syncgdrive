use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use sync_g_drive::config::AppConfig;
use sync_g_drive::db::Database;
use sync_g_drive::engine::{EngineCommand, EngineStatus, SyncEngine};

#[tokio::main]
async fn main() -> Result<()> {
    let (cfg, is_first_run) = AppConfig::load_or_create().context("cannot load config")?;
    let needs_config = cfg.validate().is_err();

    let log_path = dirs_log();
    let _log_guard = init_logging(&log_path)?;

    if needs_config {
        warn!(reason = %cfg.validate().unwrap_err(), "config invalide — ouvrez Settings");
    } else {
        info!(local = %cfg.local_root.display(), remote = %cfg.remote_root, "SyncGDrive démarrage");
    }

    let db = Database::open(std::path::Path::new(&db_path())).context("cannot open db")?;
    db.init_schema()?;

    let (cmd_tx, cmd_rx) = mpsc::channel::<EngineCommand>(32);
    let (status_tx, mut status_rx) = mpsc::unbounded_channel::<EngineStatus>();
    let shutdown = CancellationToken::new();

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
            libc::signal(libc::SIGINT,  signal_handler as libc::sighandler_t);
            libc::signal(libc::SIGTERM, signal_handler as libc::sighandler_t);
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
        use std::sync::{Arc, Mutex};
        let status_state = Arc::new(Mutex::new(EngineStatus::Idle));
        {
            let ss = status_state.clone();
            let sd = shutdown.clone();
            tokio::spawn(async move {
                while let Some(s) = status_rx.recv().await {
                    let stop = matches!(s, EngineStatus::Stopped);
                    *ss.lock().unwrap() = s;
                    if stop { sd.cancel(); break; }
                }
            });
        }
        sync_g_drive::ui::spawn_tray(
            cmd_tx.clone(),
            status_state,
            Arc::new(Mutex::new(cfg)),
            is_first_run || needs_config,
            shutdown.clone(),
            log_path,
        )?;
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
        unsafe { libc::write(fd, b"\x00".as_ptr() as *const libc::c_void, 1); }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn db_path() -> String {
    let dir = xdg_dir("XDG_DATA_HOME", ".local/share").join("syncgdrive");
    std::fs::create_dir_all(&dir).ok();
    dir.join("index.db").to_string_lossy().into_owned()
}

fn dirs_log() -> std::path::PathBuf {
    let dir = xdg_dir("XDG_STATE_HOME", ".local/state").join("syncgdrive");
    std::fs::create_dir_all(&dir).ok();
    dir.join("syncgdrive.log")
}

fn xdg_dir(env: &str, fallback: &str) -> std::path::PathBuf {
    std::env::var(env).map(std::path::PathBuf::from).unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        std::path::PathBuf::from(home).join(fallback)
    })
}

fn init_logging(log_path: &std::path::PathBuf) -> Result<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,zbus=warn,globset=warn,glib=warn"));

    let timer = time::format_description::parse("[hour]:[minute]:[second]").expect("time fmt");
    let timer = tracing_subscriber::fmt::time::UtcTime::new(timer);

    let stdout    = fmt::layer().with_target(false).with_timer(timer.clone()).compact();
    let dir       = log_path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let file_name = log_path.file_name().and_then(|f| f.to_str()).unwrap_or("syncgdrive.log");
    let (writer, guard) = tracing_appender::non_blocking(
        tracing_appender::rolling::never(dir, file_name)
    );
    let file_layer = fmt::layer().with_target(true).with_timer(timer).with_ansi(false).with_writer(writer);

    tracing_subscriber::registry()
        .with(filter).with(stdout).with(file_layer)
        .try_init().context("cannot init tracing")?;

    Ok(guard)
}