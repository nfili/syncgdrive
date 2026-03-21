//! Systray KSNI : StatusNotifierItem + tooltip dynamique + menu contextuel.
//! Intègre les icônes SVG animées et la gestion de la fenêtre de Scan (Phase 7).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use crate::config::AppConfig;
use crate::engine::{EngineCommand, EngineStatus, ScanPhase};
use crate::ui::icons::{TrayIcon, get_icon_pixmap};

use gtk4::prelude::*;

// ── Canal global pour diffuser le statut à la fenêtre de Scan ───────────────
static SCAN_TX: std::sync::OnceLock<tokio::sync::watch::Sender<EngineStatus>> = std::sync::OnceLock::new();

pub fn get_scan_rx() -> tokio::sync::watch::Receiver<EngineStatus> {
    SCAN_TX.get_or_init(|| tokio::sync::watch::channel(EngineStatus::Starting).0).subscribe()
}

pub fn show_scan_window() {
    let tx = ensure_gtk_thread();
    let _ = tx.send(GtkAction::ShowScanWindow);
}

// ══════════════════════════════════════════════════════════════════════════════
//  Public API
// ══════════════════════════════════════════════════════════════════════════════
pub fn spawn_tray(
    cmd_tx: tokio::sync::mpsc::Sender<EngineCommand>,
    mut status_rx: tokio::sync::mpsc::UnboundedReceiver<EngineStatus>,
    config: Arc<Mutex<AppConfig>>,
    open_settings: bool,
    shutdown: CancellationToken,
    log_dir: PathBuf,
) -> Result<()> {
    let sd = shutdown.clone();
    let autostart = is_autostart_enabled();

    let scan_tx = SCAN_TX.get_or_init(|| tokio::sync::watch::channel(EngineStatus::Starting).0);

    let tray = SyncTray {
        status: Arc::new(Mutex::new(EngineStatus::Starting)),
        cmd_tx: cmd_tx.clone(),
        config,
        shutdown,
        log_dir,
        last_synced: String::new(),
        autostart,
        initial_sync_notified: false,
        animation_frame: 0,
        is_animating: false,
    };

    // CORRECTION : On clone l'accès au statut pour notre boucle d'animation
    let shared_status = tray.status.clone();

    tokio::spawn(async move {
        use ksni::TrayMethods as _;
        match tray.spawn().await {
            Ok(handle) => {
                tracing::info!("systray prêt (SVG Animé)");
                let mut animation_tick = tokio::time::interval(std::time::Duration::from_millis(300));

                loop {
                    tokio::select! {
                        biased;
                        _ = sd.cancelled() => break,

                        // Boucle d'animation à 300ms
                        _ = animation_tick.tick() => {
                            let mut needs_update = false;
                            {
                                // CORRECTION : On utilise l'Arc cloné au lieu de handle.tray()
                                let status = shared_status.lock().unwrap();
                                if matches!(*status, EngineStatus::SyncProgress { .. } | EngineStatus::Syncing { .. }) {
                                    needs_update = true;
                                }
                            }
                            if needs_update {
                                handle.update(|t: &mut SyncTray| {
                                    t.animation_frame = (t.animation_frame + 1) % 4;
                                    t.is_animating = true;
                                }).await;
                            }
                        }

                        maybe = status_rx.recv() => {
                            match maybe {
                                Some(s) => {
                                    let stop = matches!(s, EngineStatus::Stopped);
                                    let _ = scan_tx.send(s.clone());

                                    handle.update(move |tray: &mut SyncTray| {
                                        if let EngineStatus::SyncProgress(ref snap) = s {
                                            tray.last_synced = snap.current_name.clone();
                                        }
                                        if matches!(s, EngineStatus::Idle)
                                            && !tray.initial_sync_notified
                                            && !tray.last_synced.is_empty()
                                        {
                                            tray.initial_sync_notified = true;
                                            let cfg = tray.config.lock().unwrap();
                                            crate::notif::initial_sync_complete(&cfg);
                                        }

                                        tray.is_animating = matches!(s, EngineStatus::SyncProgress { .. } | EngineStatus::Syncing { .. });
                                        *tray.status.lock().unwrap() = s;
                                    }).await;

                                    if stop { sd.cancel(); break; }
                                }
                                None => break,
                            }
                        }
                    }
                }
                handle.shutdown().await;
            }
            Err(e) => tracing::error!("ksni spawn: {e}"),
        }
    });

    if open_settings {
        open_settings_window(cmd_tx);
    }

    Ok(())
}

// ══════════════════════════════════════════════════════════════════════════════
//  SyncTray
// ══════════════════════════════════════════════════════════════════════════════

struct SyncTray {
    status: Arc<Mutex<EngineStatus>>,
    cmd_tx: tokio::sync::mpsc::Sender<EngineCommand>,
    config: Arc<Mutex<AppConfig>>,
    shutdown: CancellationToken,
    log_dir: PathBuf,
    last_synced: String,
    autostart: bool,
    initial_sync_notified: bool,
    // Pour l'animation
    animation_frame: usize,
    is_animating: bool,
}

impl ksni::Tray for SyncTray {
    fn id(&self) -> String {
        "syncgdrive".into()
    }

    /// On n'utilise plus les noms d'icônes natifs de l'OS.
    fn icon_name(&self) -> String {
        "".into()
    }

    /// On génère dynamiquement la Pixmap ARGB32 de notre SVG embarqué.
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        let status = &*self.status.lock().unwrap();
        let tray_icon = match status {
            EngineStatus::Starting => TrayIcon::Starting,
            EngineStatus::Unconfigured(_) => TrayIcon::Error,
            EngineStatus::Idle => TrayIcon::Idle,
            EngineStatus::ScanProgress { .. } => TrayIcon::Scanning,
            EngineStatus::SyncProgress { .. } | EngineStatus::Syncing { .. } => {
                if self.is_animating {
                    TrayIcon::Sync(self.animation_frame)
                } else {
                    TrayIcon::Sync(0)
                }
            },
            EngineStatus::Paused => TrayIcon::Paused,
            EngineStatus::Error(_) => TrayIcon::Error,
            EngineStatus::Stopped => TrayIcon::Offline,
        };

        vec![ksni::Icon {
            width: 24,
            height: 24,
            data: get_icon_pixmap(tray_icon),
        }]
    }

    fn title(&self) -> String {
        match &*self.status.lock().unwrap() {
            EngineStatus::Starting => "SyncGDrive — Démarrage…".into(),
            EngineStatus::Unconfigured(_) => "SyncGDrive — Configuration requise".into(),
            EngineStatus::Idle => "SyncGDrive — Surveillance active".into(),
            EngineStatus::ScanProgress { phase, done, total, .. } => {
                let label = match phase {
                    ScanPhase::RemoteListing => "Analyse Drive",
                    ScanPhase::LocalListing => "Analyse locale",
                    ScanPhase::Directories => "Création dossiers",
                    ScanPhase::Comparing => "Comparaison",
                };
                if *total > 0 { format!("SyncGDrive — {label} {done}/{total}") }
                else if *done > 0 { format!("SyncGDrive — {label} ({done})") }
                else { format!("SyncGDrive — {label}…") }
            }
            EngineStatus::SyncProgress(snap) => format!("SyncGDrive — ↑ {}/{} {}", snap.done_files, snap.total_files, snap.current_name),
            EngineStatus::Syncing { active } => format!("SyncGDrive — {active} transfert(s)"),
            EngineStatus::Paused => "SyncGDrive — ⏸ En pause".into(),
            EngineStatus::Error(_) => "SyncGDrive — Erreur".into(),
            EngineStatus::Stopped => "SyncGDrive — Arrêté".into(),
        }
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        let (title, description) = match &*self.status.lock().unwrap() {
            EngineStatus::Starting => (
                "SyncGDrive — Démarrage".into(),
                "Initialisation, chargement de la configuration…".into(),
            ),
            EngineStatus::Unconfigured(reason) => (
                "SyncGDrive — Configuration requise".into(),
                format!("Ouvrez les Réglages pour configurer.\n{reason}"),
            ),
            EngineStatus::Idle => {
                let cfg = self.config.lock().unwrap();
                let last = if self.last_synced.is_empty() { String::new() } else { format!("\n✅ Dernier transfert : {}", self.last_synced) };
                let local_disp = cfg.sync_pairs.first().map(|p| p.local_path.display().to_string()).unwrap_or_else(|| "Non configuré".into());
                let remote_disp = cfg.sync_pairs.first().map(|p| p.remote_folder_id.clone()).unwrap_or_else(|| "Aucun".into());
                (
                    "SyncGDrive — Surveillance active".into(),
                    format!("Surveillance active — Dossier à jour.\n{} → {}{}", local_disp, remote_disp, last),
                )
            }
            EngineStatus::ScanProgress { phase, done, total, current } => {
                let (_, clean_name) = crate::utils::path_display::split_path_display(current);
                match phase {
                    ScanPhase::RemoteListing => (
                        "SyncGDrive — Analyse Drive".into(),
                        format!("Analyse Google Drive en cours…\n(Lecture de : {clean_name})"),
                    ),
                    ScanPhase::LocalListing => {
                        let detail = if *done > 0 { format!("({done} éléments indexés)") } else { format!("({clean_name})") };
                        ("SyncGDrive — Analyse locale".into(), format!("Analyse du disque local…\n{detail}"))
                    }
                    ScanPhase::Directories => {
                        let pct = if *total > 0 { (*done as f64 / *total as f64) * 100.0 } else { 0.0 };
                        let bar = progress_bar(pct, 10);
                        ("SyncGDrive — Création dossiers".into(), format!("Création de l'arborescence : {pct:.0}% {bar}\nDossier : {clean_name}\n({done} sur {total} créés)"))
                    }
                    ScanPhase::Comparing => {
                        let pct = if *total > 0 { (*done as f64 / *total as f64) * 100.0 } else { 0.0 };
                        let bar = progress_bar(pct, 10);
                        ("SyncGDrive — Comparaison".into(), format!("Comparaison avec la base de données… {pct:.0}% {bar}\n({done}/{total} fichiers analysés)"))
                    }
                }
            }
            EngineStatus::SyncProgress(snap) => {
                let global_pct = if snap.total_bytes > 0 { ((snap.sent_bytes as f64 / snap.total_bytes as f64) * 100.0).clamp(0.0, 100.0) } else { 0.0 };
                let bar = progress_bar(global_pct, 15); // Barre plus courte pour intégration parfaite avec le texte

                // NOUVEAU : Formatage du chemin sur 2 lignes avec emojis
                let full_rel_path = if snap.current_dir.is_empty() || snap.current_dir == "/" {
                    snap.current_name.clone()
                } else {
                    format!("{}/{}", snap.current_dir, snap.current_name)
                };
                let formatted_path = crate::utils::path_display::format_path_tooltip(&full_rel_path);

                let current_idx = (snap.done_files + 1).min(snap.total_files);

                (
                    format!("Transfert {}/{}", current_idx, snap.total_files),
                    format!(
                        "{}\n{} {:.0}% · {}/s · {}\nTotal : {} / {}",
                        formatted_path, bar, global_pct, human_size(snap.speed_bps), snap.eta_string, human_size(snap.sent_bytes), human_size(snap.total_bytes)
                    ),
                )
            }
            EngineStatus::Syncing { active } => (format!("SyncGDrive — {active} transfert(s) en cours"), "Transferts vers Google Drive…".into()),
            EngineStatus::Paused => ("SyncGDrive — ⏸ En pause".into(), "Moteur suspendu.\n(Ouvrez le menu contextuel pour reprendre)".into()),
            EngineStatus::Error(e) => ("SyncGDrive — Erreur".into(), format!("{e}\nVérifiez les logs ou les tokens KIO.")),
            EngineStatus::Stopped => ("SyncGDrive — Arrêté".into(), "Le moteur est arrêté.".into()),
        };
        ksni::ToolTip { title, description, ..Default::default() }
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;
        let status = self.status.lock().unwrap().clone();
        let is_active = matches!(status, EngineStatus::ScanProgress { .. } | EngineStatus::SyncProgress { .. } | EngineStatus::Syncing { .. });
        let is_paused = matches!(status, EngineStatus::Paused);

        let mut items: Vec<ksni::MenuItem<Self>> = Vec::new();
        items.push(StandardItem { label: self.title(), enabled: false, ..Default::default() }.into());
        items.push(MenuItem::Separator);

        if is_paused {
            items.push(StandardItem { label: "▶ Reprendre la synchronisation".into(), icon_name: "media-playback-start".into(), activate: Box::new(|t: &mut Self| { let _ = t.cmd_tx.try_send(EngineCommand::Resume); }), ..Default::default() }.into());
        } else if is_active {
            items.push(StandardItem { label: "⏸ Mettre en pause".into(), icon_name: "media-playback-pause".into(), activate: Box::new(|t: &mut Self| { let _ = t.cmd_tx.try_send(EngineCommand::Pause); }), ..Default::default() }.into());
        } else {
            items.push(StandardItem { label: "Synchroniser maintenant".into(), icon_name: "emblem-synchronizing".into(), activate: Box::new(|t: &mut Self| { let _ = t.cmd_tx.try_send(EngineCommand::ForceScan); }), ..Default::default() }.into());
        }

        items.push(MenuItem::Separator);
        let local = self.config.lock().unwrap().sync_pairs.first().map(|p| p.local_path.clone()).unwrap_or_default();
        items.push(StandardItem { label: "📂 Ouvrir le dossier local".into(), icon_name: "folder-open".into(), activate: Box::new(move |_: &mut Self| { let _ = std::process::Command::new("xdg-open").arg(&local).spawn(); }), ..Default::default() }.into());

        let remote = self.config.lock().unwrap().sync_pairs.first().map(|p| p.remote_folder_id.clone()).unwrap_or_default();
        items.push(StandardItem { label: "☁ Ouvrir Google Drive".into(), icon_name: "folder-remote".into(), activate: Box::new(move |_: &mut Self| { let _ = std::process::Command::new("xdg-open").arg(format!("https://drive.google.com/drive/folders/{}", remote)).spawn(); }), ..Default::default() }.into());

        items.push(MenuItem::Separator);
        let label = if self.autostart { "🚀 Lancer au démarrage ✓" } else { "🚀 Lancer au démarrage" };
        items.push(StandardItem { label: label.into(), icon_name: "system-run".into(), activate: Box::new(|t: &mut Self| { t.autostart = !t.autostart; toggle_autostart(t.autostart); }), ..Default::default() }.into());
        items.push(StandardItem { label: "⚙ Réglages…".into(), icon_name: "preferences-system".into(), activate: Box::new(|t: &mut Self| { let _ = t.cmd_tx.try_send(EngineCommand::Pause); open_settings_window(t.cmd_tx.clone()); }), ..Default::default() }.into());
        let p = self.log_dir.clone();
        items.push(StandardItem { label: "📄 Voir les logs".into(), icon_name: "text-x-log".into(), activate: Box::new(move |_: &mut Self| { let _ = std::process::Command::new("xdg-open").arg(&p).spawn(); }), ..Default::default() }.into());
        items.push(StandardItem { label: "ℹ À propos".into(), icon_name: "help-about".into(), activate: Box::new(|_: &mut Self| { show_about(); }), ..Default::default() }.into());
        items.push(MenuItem::Separator);
        items.push(StandardItem { label: "🛑 Quitter SyncGDrive".into(), icon_name: "application-exit".into(), activate: Box::new(|t: &mut Self| { t.shutdown.cancel(); let _ = t.cmd_tx.try_send(EngineCommand::Shutdown); }), ..Default::default() }.into());

        items
    }
}

// ══════════════════════════════════════════════════════════════════════════════
//  Utilitaires
// ══════════════════════════════════════════════════════════════════════════════

fn progress_bar(percent: f64, length: usize) -> String {
    let pct = percent.clamp(0.0, 100.0);
    let filled = ((pct / 100.0) * length as f64).round() as usize;
    let empty = length.saturating_sub(filled);
    format!("[{}{}]", "█".repeat(filled), "░".repeat(empty))
}

fn human_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if bytes >= GB { format!("{:.1} Go", bytes as f64 / GB as f64) }
    else if bytes >= MB { format!("{:.1} Mo", bytes as f64 / MB as f64) }
    else if bytes >= KB { format!("{:.0} Ko", bytes as f64 / KB as f64) }
    else { format!("{bytes} o") }
}

fn is_autostart_enabled() -> bool {
    std::process::Command::new("systemctl").args(["--user", "is-enabled", "syncgdrive.service"]).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().map(|s| s.success()).unwrap_or(false)
}

fn toggle_autostart(enable: bool) {
    let action = if enable { "enable" } else { "disable" };
    let _ = std::process::Command::new("systemctl").args(["--user", action, "syncgdrive.service"]).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
}

// ── Thread GTK unique et persistant ──────────────────────────────────────────

enum GtkAction {
    OpenSettings(tokio::sync::mpsc::Sender<EngineCommand>),
    ShowAbout,
    ShowScanWindow,
}

static GTK_TX: std::sync::OnceLock<std::sync::mpsc::Sender<GtkAction>> = std::sync::OnceLock::new();

fn ensure_gtk_thread() -> &'static std::sync::mpsc::Sender<GtkAction> {
    GTK_TX.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<GtkAction>();
        std::thread::Builder::new()
            .name("gtk-ui".into())
            .spawn(move || {
                unsafe { std::env::set_var("ADWAITA_DISABLE_LEGACY_THEMING_WARNINGS", "1") };
                if libadwaita::init().is_err() {
                    tracing::error!("gtk-ui: libadwaita init failed");
                    return;
                }
                while let Ok(action) = rx.recv() {
                    match action {
                        GtkAction::OpenSettings(cmd_tx) => {
                            if let Err(e) = super::settings::run_standalone(cmd_tx) { tracing::warn!("settings window: {e}"); }
                        }
                        GtkAction::ShowAbout => { run_about_app(); }
                        GtkAction::ShowScanWindow => {
                            let rx_stream = get_scan_rx();
                            crate::ui::scan_window::run_standalone(rx_stream);
                        }
                    }
                }
            })
            .expect("cannot spawn gtk-ui thread");
        tx
    })
}

fn open_settings_window(cmd_tx: tokio::sync::mpsc::Sender<EngineCommand>) {
    let tx = ensure_gtk_thread();
    let _ = tx.send(GtkAction::OpenSettings(cmd_tx));
}

fn show_about() {
    let tx = ensure_gtk_thread();
    let _ = tx.send(GtkAction::ShowAbout);
}

fn run_about_app() {
    let app = gtk4::Application::builder().application_id("fr.clyds.syncgdrive.about").flags(gtk4::gio::ApplicationFlags::NON_UNIQUE).build();
    app.connect_activate(|app| {
        let about = libadwaita::AboutWindow::builder()
            .application_name("SyncGDrive")
            .version(env!("CARGO_PKG_VERSION"))
            .developer_name("clyds")
            .license_type(gtk4::License::MitX11)
            .comments("Synchronisation unidirectionnelle d'un dossier local vers Google Drive.\nL'ordinateur local est la source de vérité — le Drive est la sauvegarde.")
            .website("https://github.com/clyds/SyncGDrive")
            .issue_url("https://github.com/clyds/SyncGDrive/issues")
            .application_icon("emblem-synchronizing-symbolic")
            .application(app)
            .build();
        about.present();
    });
    app.run_with_args::<String>(&[]);
}