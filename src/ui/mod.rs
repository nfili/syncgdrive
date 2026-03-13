//! Interface systray + fenêtre Settings.
//!
//! # Architecture des threads
//!
//! ```text
//! Runtime Tokio (multi-thread)
//!   ├─ task ksni      : D-Bus StatusNotifierItem (async, pas de runtime imbriqué)
//!   ├─ task engine    : moteur de synchronisation
//!   └─ task status    : dispatch EngineStatus → Arc<Mutex> pour ksni
//!
//! Thread OS « gtk-settings » (ponctuel, fire-and-forget)
//!   └─ libadwaita::init() + app.run_with_args()  → Pause/Resume à l'ouverture/fermeture
//! ```
//!
//! ksni tourne directement sur le runtime Tokio principal via son API async.
//! Pas de `ksni::blocking` (qui créait un second runtime → panic "runtime within runtime").

pub mod settings;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use crate::config::AppConfig;
use crate::engine::{EngineCommand, EngineStatus};

/// Lance le systray ksni comme tâche Tokio sur le runtime principal.
/// Ne bloque pas — retourne immédiatement.
pub fn spawn_tray(
    cmd_tx: tokio::sync::mpsc::Sender<EngineCommand>,
    status: Arc<Mutex<EngineStatus>>,
    config: Arc<Mutex<AppConfig>>,
    open_settings: bool,
    shutdown: CancellationToken,
    log_path: PathBuf,
) -> Result<()> {
    let sd = shutdown.clone();
    let tray = SyncTray { status, cmd_tx: cmd_tx.clone(), config, shutdown, log_path };

    // API async de ksni : tourne sur le runtime Tokio principal.
    // Pas de Runtime imbriqué, pas de thread OS dédié.
    tokio::spawn(async move {
        use ksni::TrayMethods as _;
        match tray.spawn().await {
            Ok(handle) => {
                tracing::info!("systray prêt (StatusNotifierItem)");
                // Le handle ksni reste en vie jusqu'au shutdown.
                sd.cancelled().await;
                handle.shutdown().await;
                tracing::info!("systray arrêté proprement");
            }
            Err(e) => tracing::error!("ksni spawn: {e}"),
        }
    });

    if open_settings {
        open_settings_window(cmd_tx);
    }

    Ok(())
}

// ── Ouvre la fenêtre Settings dans un thread GTK ponctuel ────────────────────
fn open_settings_window(cmd_tx: tokio::sync::mpsc::Sender<EngineCommand>) {
    std::thread::Builder::new()
        .name("gtk-settings".into())
        .spawn(move || {
            if let Err(e) = settings::run_standalone(cmd_tx) {
                tracing::warn!("settings window: {e}");
            }
        })
        .ok();
}

// ── Systray ksni ─────────────────────────────────────────────────────────────

struct SyncTray {
    status:   Arc<Mutex<EngineStatus>>,
    cmd_tx:   tokio::sync::mpsc::Sender<EngineCommand>,
    config:   Arc<Mutex<AppConfig>>,
    shutdown: CancellationToken,
    log_path: PathBuf,
}

impl ksni::Tray for SyncTray {
    fn id(&self) -> String { "syncgdrive".into() }

    fn icon_name(&self) -> String {
        match &*self.status.lock().unwrap() {
            EngineStatus::Unconfigured(_)  => "dialog-warning",
            EngineStatus::Idle             => "emblem-default",
            EngineStatus::ScanProgress{..} => "emblem-synchronizing",
            EngineStatus::SyncProgress{..} => "emblem-synchronizing",
            EngineStatus::Syncing { .. }   => "emblem-synchronizing",
            EngineStatus::Paused           => "media-playback-pause",
            EngineStatus::Error(_)         => "dialog-error",
            EngineStatus::Stopped          => "emblem-default",
        }.into()
    }

    fn title(&self) -> String {
        match &*self.status.lock().unwrap() {
            EngineStatus::Unconfigured(_) => "SyncGDrive — Config requise".into(),
            EngineStatus::Idle            => "SyncGDrive — En veille".into(),
            EngineStatus::ScanProgress { done, total, .. } =>
                format!("SyncGDrive — Scan {done}/{total}"),
            EngineStatus::SyncProgress { done, total, current, .. } =>
                format!("SyncGDrive — ↑ {done}/{total} {current}"),
            EngineStatus::Syncing { active } =>
                format!("SyncGDrive — {active} transfert(s)"),
            EngineStatus::Paused          => "SyncGDrive — ⏸ En pause".into(),
            EngineStatus::Error(e)        => format!("SyncGDrive — Erreur: {e}"),
            EngineStatus::Stopped         => "SyncGDrive — Arrêté".into(),
        }
    }

    /// Tooltip affiché au survol de l'icône systray.
    fn tool_tip(&self) -> ksni::ToolTip {
        let (title, description) = match &*self.status.lock().unwrap() {
            EngineStatus::Unconfigured(reason) => (
                "SyncGDrive — Configuration requise".into(),
                format!("Ouvrez les Réglages pour configurer.\n{reason}"),
            ),
            EngineStatus::Idle => {
                let cfg = self.config.lock().unwrap();
                (
                    "SyncGDrive — En veille".into(),
                    format!("Surveillance active.\n{} → {}", cfg.local_root.display(), cfg.remote_root),
                )
            }
            EngineStatus::ScanProgress { phase, done, total, current } => {
                let phase_str = match phase {
                    crate::engine::ScanPhase::Listing     => "Inventaire",
                    crate::engine::ScanPhase::Directories => "Dossiers",
                    crate::engine::ScanPhase::Comparing   => "Comparaison",
                };
                (
                    format!("SyncGDrive — Scan ({phase_str})"),
                    format!("{done}/{total} — {current}"),
                )
            }
            EngineStatus::SyncProgress { done, total, current, size_bytes } => {
                let size = human_size(*size_bytes);
                (
                    format!("SyncGDrive — Transfert {done}/{total}"),
                    format!("{current} ({size})"),
                )
            }
            EngineStatus::Syncing { active } => (
                format!("SyncGDrive — {active} transfert(s) en cours"),
                "Transferts vers Google Drive…".into(),
            ),
            EngineStatus::Paused => (
                "SyncGDrive — ⏸ En pause".into(),
                "Réglages ouverts. Reprendra à la fermeture.".into(),
            ),
            EngineStatus::Error(e) => (
                "SyncGDrive — Erreur".into(),
                format!("{e}\nVérifiez les logs ou les tokens KIO."),
            ),
            EngineStatus::Stopped => (
                "SyncGDrive — Arrêté".into(),
                "Le moteur est arrêté.".into(),
            ),
        };
        ksni::ToolTip {
            icon_name: self.icon_name(),
            title,
            description,
            ..Default::default()
        }
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;
        vec![
            StandardItem {
                label: self.title(),
                enabled: false,
                ..Default::default()
            }.into(),
            MenuItem::Separator,
            StandardItem {
                label: "Sync maintenant".into(),
                icon_name: "emblem-synchronizing".into(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.cmd_tx.try_send(EngineCommand::ForceScan);
                }),
                ..Default::default()
            }.into(),
            StandardItem {
                label: "Réglages…".into(),
                icon_name: "preferences-system".into(),
                activate: Box::new(|t: &mut Self| {
                    open_settings_window(t.cmd_tx.clone());
                }),
                ..Default::default()
            }.into(),
            {
                let p = self.log_path.clone();
                StandardItem {
                    label: "Voir les logs".into(),
                    icon_name: "text-x-log".into(),
                    activate: Box::new(move |_: &mut Self| {
                        let _ = std::process::Command::new("xdg-open").arg(&p).spawn();
                    }),
                    ..Default::default()
                }.into()
            },
            MenuItem::Separator,
            StandardItem {
                label: "Quitter".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|t: &mut Self| {
                    t.shutdown.cancel();
                    let _ = t.cmd_tx.try_send(EngineCommand::Shutdown);
                }),
                ..Default::default()
            }.into(),
        ]
    }
}

fn human_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if bytes >= GB {
        format!("{:.1} Go", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} Mo", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.0} Ko", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} o")
    }
}

