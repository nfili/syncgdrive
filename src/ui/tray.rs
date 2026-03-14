//! Systray KSNI : StatusNotifierItem + tooltip dynamique + menu contextuel.
//!
//! Implémentation conforme à UX_SYSTRAY.md (§1–§8).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use crate::config::AppConfig;
use crate::engine::{EngineCommand, EngineStatus, ScanPhase};

use gtk4::prelude::*;

// ══════════════════════════════════════════════════════════════════════════════
//  Public API
// ══════════════════════════════════════════════════════════════════════════════

/// Lance le systray ksni comme tâche Tokio sur le runtime principal.
/// `status_rx` est consommé directement dans la tâche ksni.
/// Chaque changement d'état déclenche `handle.update()` → D-Bus refresh.
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
    let tray = SyncTray {
        status: Arc::new(Mutex::new(EngineStatus::Starting)),
        cmd_tx: cmd_tx.clone(),
        config,
        shutdown,
        log_dir,
        last_synced: String::new(),
        autostart,
        initial_sync_notified: false,
    };

    tokio::spawn(async move {
        use ksni::TrayMethods as _;
        match tray.spawn().await {
            Ok(handle) => {
                tracing::info!("systray prêt (StatusNotifierItem)");
                loop {
                    tokio::select! {
                        biased;
                        _ = sd.cancelled() => break,
                        maybe = status_rx.recv() => {
                            match maybe {
                                Some(s) => {
                                    let stop = matches!(s, EngineStatus::Stopped);
                                    handle.update(move |tray: &mut SyncTray| {
                                        // Tracker le dernier fichier synchronisé
                                        if let EngineStatus::SyncProgress { ref current, .. } = s {
                                            tray.last_synced = current.clone();
                                        }
                                        // Notification "Sync initiale terminée" (une seule fois)
                                        if matches!(s, EngineStatus::Idle)
                                            && !tray.initial_sync_notified
                                            && !tray.last_synced.is_empty()
                                        {
                                            tray.initial_sync_notified = true;
                                            let cfg = tray.config.lock().unwrap();
                                            crate::notif::initial_sync_complete(&cfg);
                                        }
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

// ══════════════════════════════════════════════════════════════════════════════
//  SyncTray — StatusNotifierItem (§1, §5, §7 UX_SYSTRAY.md)
// ══════════════════════════════════════════════════════════════════════════════

struct SyncTray {
    status:   Arc<Mutex<EngineStatus>>,
    cmd_tx:   tokio::sync::mpsc::Sender<EngineCommand>,
    config:   Arc<Mutex<AppConfig>>,
    shutdown: CancellationToken,
    log_dir:  PathBuf,
    /// Dernier fichier synchronisé (pour le tooltip Idle).
    last_synced: String,
    /// État du toggle "Lancer au démarrage" (systemctl).
    autostart: bool,
    /// true une fois que la notification "sync initiale terminée" a été envoyée.
    initial_sync_notified: bool,
}

impl ksni::Tray for SyncTray {
    fn id(&self) -> String { "syncgdrive".into() }

    /// Icône de la systray (§1 UX_SYSTRAY.md).
    fn icon_name(&self) -> String {
        match &*self.status.lock().unwrap() {
            EngineStatus::Starting             => "system-run-symbolic",
            EngineStatus::Unconfigured(_)      => "dialog-warning",
            EngineStatus::Idle                 => "emblem-ok-symbolic",
            EngineStatus::ScanProgress { phase, .. } => match phase {
                ScanPhase::RemoteListing => "network-server-symbolic",
                ScanPhase::LocalListing  => "folder-saved-search-symbolic",
                ScanPhase::Directories   => "folder-new-symbolic",
                ScanPhase::Comparing     => "edit-find-replace-symbolic",
            },
            EngineStatus::SyncProgress{..}     => "emblem-synchronizing-symbolic",
            EngineStatus::Syncing { .. }       => "emblem-synchronizing-symbolic",
            EngineStatus::Paused               => "preferences-system-symbolic",
            EngineStatus::Error(_)             => "dialog-error",
            EngineStatus::Stopped              => "system-shutdown-symbolic",
        }.into()
    }

    /// Titre court affiché par le panneau (§1).
    fn title(&self) -> String {
        match &*self.status.lock().unwrap() {
            EngineStatus::Starting            => "SyncGDrive — Démarrage…".into(),
            EngineStatus::Unconfigured(_)     => "SyncGDrive — Configuration requise".into(),
            EngineStatus::Idle                => "SyncGDrive — Surveillance active".into(),
            EngineStatus::ScanProgress { phase, done, total, .. } => {
                let label = match phase {
                    ScanPhase::RemoteListing => "Analyse Drive",
                    ScanPhase::LocalListing  => "Analyse locale",
                    ScanPhase::Directories   => "Création dossiers",
                    ScanPhase::Comparing     => "Comparaison",
                };
                if *total > 0 {
                    format!("SyncGDrive — {label} {done}/{total}")
                } else if *done > 0 {
                    format!("SyncGDrive — {label} ({done})")
                } else {
                    format!("SyncGDrive — {label}…")
                }
            }
            EngineStatus::SyncProgress { done, total, current, .. } =>
                format!("SyncGDrive — ↑ {done}/{total} {current}"),
            EngineStatus::Syncing { active } =>
                format!("SyncGDrive — {active} transfert(s)"),
            EngineStatus::Paused          => "SyncGDrive — ⏸ En pause".into(),
            EngineStatus::Error(_)        => "SyncGDrive — Erreur".into(),
            EngineStatus::Stopped         => "SyncGDrive — Arrêté".into(),
        }
    }

    /// Tooltip dynamique avec barres de progression Unicode (§7 UX_SYSTRAY.md).
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
                let last = if self.last_synced.is_empty() {
                    String::new()
                } else {
                    format!("\n✅ Dernier transfert : {}", self.last_synced)
                };
                (
                    "SyncGDrive — Surveillance active".into(),
                    format!(
                        "Surveillance active — Dossier à jour.\n{} → {}{}",
                        cfg.local_root.display(), cfg.remote_root, last
                    ),
                )
            }
            EngineStatus::ScanProgress { phase, done, total, current } => {
                match phase {
                    // §7B : "Analyse Google Drive en cours… (Lecture de : /Archives)"
                    ScanPhase::RemoteListing => (
                        "SyncGDrive — Analyse Drive".into(),
                        format!("Analyse Google Drive en cours…\n(Lecture de : {current})"),
                    ),
                    // §7B : "Analyse du disque local… (1452 fichiers indexés)"
                    ScanPhase::LocalListing => {
                        let detail = if *done > 0 {
                            format!("({done} éléments indexés)")
                        } else {
                            format!("({current})")
                        };
                        (
                            "SyncGDrive — Analyse locale".into(),
                            format!("Analyse du disque local…\n{detail}"),
                        )
                    }
                    // §7A : barre de progression + nom du dossier
                    ScanPhase::Directories => {
                        let pct = if *total > 0 { (*done as f64 / *total as f64) * 100.0 } else { 0.0 };
                        let bar = progress_bar(pct, 10);
                        (
                            "SyncGDrive — Création dossiers".into(),
                            format!(
                                "Création de l'arborescence : {pct:.0}% {bar}\nDossier : {current}\n({done} sur {total} dossiers créés)"
                            ),
                        )
                    }
                    // §7A : barre de progression
                    ScanPhase::Comparing => {
                        let pct = if *total > 0 { (*done as f64 / *total as f64) * 100.0 } else { 0.0 };
                        let bar = progress_bar(pct, 10);
                        (
                            "SyncGDrive — Comparaison".into(),
                            format!(
                                "Comparaison avec la base de données… {pct:.0}% {bar}\n({done}/{total} fichiers analysés)"
                            ),
                        )
                    }
                }
            }
            // §7A : "Envoi en cours : 80% [████████░░]\nFichier : rapport.pdf\nPoids : 4.2 Mo (8 / 10 fichiers)"
            EngineStatus::SyncProgress { done, total, current, size_bytes } => {
                let pct = if *total > 0 { (*done as f64 / *total as f64) * 100.0 } else { 0.0 };
                let bar = progress_bar(pct, 10);
                let size = human_size(*size_bytes);
                (
                    format!("SyncGDrive — Transfert {done}/{total}"),
                    format!(
                        "Envoi en cours : {pct:.0}% {bar}\nFichier : {current}\nPoids : {size} ({done} / {total} fichiers)"
                    ),
                )
            }
            EngineStatus::Syncing { active } => (
                format!("SyncGDrive — {active} transfert(s) en cours"),
                "Transferts vers Google Drive…".into(),
            ),
            // §7B : "Moteur suspendu. (Ouvrez le menu contextuel pour reprendre)"
            EngineStatus::Paused => (
                "SyncGDrive — ⏸ En pause".into(),
                "Moteur suspendu.\n(Ouvrez le menu contextuel pour reprendre)".into(),
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

    /// Menu contextuel dynamique (§5 UX_SYSTRAY.md).
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;
        let status = self.status.lock().unwrap().clone();
        let is_active = matches!(
            status,
            EngineStatus::ScanProgress{..} | EngineStatus::SyncProgress{..} | EngineStatus::Syncing{..}
        );
        let is_paused = matches!(status, EngineStatus::Paused);

        let mut items: Vec<ksni::MenuItem<Self>> = Vec::new();

        // ── [État Actuel] (grisé, non cliquable) ─────────────────────────────
        items.push(StandardItem {
            label: self.title(),
            enabled: false,
            ..Default::default()
        }.into());
        items.push(MenuItem::Separator);

        // ── Action dynamique de synchronisation ──────────────────────────────
        if is_paused {
            items.push(StandardItem {
                label: "▶ Reprendre la synchronisation".into(),
                icon_name: "media-playback-start".into(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.cmd_tx.try_send(EngineCommand::Resume);
                }),
                ..Default::default()
            }.into());
        } else if is_active {
            items.push(StandardItem {
                label: "⏸ Mettre en pause".into(),
                icon_name: "media-playback-pause".into(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.cmd_tx.try_send(EngineCommand::Pause);
                }),
                ..Default::default()
            }.into());
        } else {
            items.push(StandardItem {
                label: "Synchroniser maintenant".into(),
                icon_name: "emblem-synchronizing".into(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.cmd_tx.try_send(EngineCommand::ForceScan);
                }),
                ..Default::default()
            }.into());
        }

        items.push(MenuItem::Separator);

        // ── 📂 Ouvrir le dossier local ───────────────────────────────────────
        {
            let local = self.config.lock().unwrap().local_root.clone();
            items.push(StandardItem {
                label: "📂 Ouvrir le dossier local".into(),
                icon_name: "folder-open".into(),
                activate: Box::new(move |_: &mut Self| {
                    let _ = std::process::Command::new("xdg-open").arg(&local).spawn();
                }),
                ..Default::default()
            }.into());
        }

        // ── ☁ Ouvrir Google Drive ────────────────────────────────────────────
        {
            let remote = self.config.lock().unwrap().remote_root.clone();
            items.push(StandardItem {
                label: "☁ Ouvrir Google Drive".into(),
                icon_name: "folder-remote".into(),
                activate: Box::new(move |_: &mut Self| {
                    let _ = std::process::Command::new("kioclient5")
                        .args(["exec", &remote])
                        .spawn();
                }),
                ..Default::default()
            }.into());
        }

        items.push(MenuItem::Separator);

        // ── 🚀 Lancer au démarrage (systemctl toggle) ───────────────────────
        {
            let label = if self.autostart {
                "🚀 Lancer au démarrage ✓"
            } else {
                "🚀 Lancer au démarrage"
            };
            items.push(StandardItem {
                label: label.into(),
                icon_name: "system-run".into(),
                activate: Box::new(|t: &mut Self| {
                    t.autostart = !t.autostart;
                    toggle_autostart(t.autostart);
                }),
                ..Default::default()
            }.into());
        }

        // ── ⚙ Réglages ──────────────────────────────────────────────────────
        items.push(StandardItem {
            label: "⚙ Réglages…".into(),
            icon_name: "preferences-system".into(),
            activate: Box::new(|t: &mut Self| {
                // Pause immédiate depuis le callback ksni (même thread que
                // "Mettre en pause") — ne pas attendre le thread GTK.
                let _ = t.cmd_tx.try_send(EngineCommand::Pause);
                open_settings_window(t.cmd_tx.clone());
            }),
            ..Default::default()
        }.into());

        // ── 📄 Voir les logs ─────────────────────────────────────────────────
        {
            let p = self.log_dir.clone();
            items.push(StandardItem {
                label: "📄 Voir les logs".into(),
                icon_name: "text-x-log".into(),
                activate: Box::new(move |_: &mut Self| {
                    let _ = std::process::Command::new("xdg-open").arg(&p).spawn();
                }),
                ..Default::default()
            }.into());
        }

        // ── ℹ À propos ──────────────────────────────────────────────────────
        items.push(StandardItem {
            label: "ℹ À propos".into(),
            icon_name: "help-about".into(),
            activate: Box::new(|_: &mut Self| {
                show_about();
            }),
            ..Default::default()
        }.into());

        items.push(MenuItem::Separator);

        // ── 🛑 Quitter SyncGDrive ────────────────────────────────────────────
        items.push(StandardItem {
            label: "🛑 Quitter SyncGDrive".into(),
            icon_name: "application-exit".into(),
            activate: Box::new(|t: &mut Self| {
                t.shutdown.cancel();
                let _ = t.cmd_tx.try_send(EngineCommand::Shutdown);
            }),
            ..Default::default()
        }.into());

        items
    }
}

// ══════════════════════════════════════════════════════════════════════════════
//  Utilitaires
// ══════════════════════════════════════════════════════════════════════════════

/// Barre de progression Unicode pour le tooltip D-Bus (§8 UX_SYSTRAY.md).
/// Convertit un pourcentage (0–100) en blocs pleins `█` et ombrés `░`.
fn progress_bar(percent: f64, length: usize) -> String {
    let pct = percent.clamp(0.0, 100.0);
    let filled = ((pct / 100.0) * length as f64).round() as usize;
    let empty = length.saturating_sub(filled);
    format!("[{}{}]", "█".repeat(filled), "░".repeat(empty))
}

/// Taille humaine (octets → Ko/Mo/Go) pour le tooltip transfert.
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

// ── Autostart via systemctl (§6 UX_SYSTRAY.md) ──────────────────────────────

fn is_autostart_enabled() -> bool {
    std::process::Command::new("systemctl")
        .args(["--user", "is-enabled", "syncgdrive.service"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn toggle_autostart(enable: bool) {
    let action = if enable { "enable" } else { "disable" };
    let _ = std::process::Command::new("systemctl")
        .args(["--user", action, "syncgdrive.service"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

// ── Thread GTK unique et persistant ──────────────────────────────────────────
//
// GTK4 ne peut être initialisé que sur UN SEUL thread pendant toute la durée
// du processus. Toute tentative d'appeler `libadwaita::init()` depuis un autre
// thread provoque un panic : "Attempted to initialize GTK from two different threads".
//
// Solution : un thread OS permanent (`gtk-ui`) avec un canal de commandes.
// Settings et À propos s'exécutent séquentiellement sur ce même thread.

enum GtkAction {
    OpenSettings(tokio::sync::mpsc::Sender<EngineCommand>),
    ShowAbout,
}

static GTK_TX: std::sync::OnceLock<std::sync::mpsc::Sender<GtkAction>> =
    std::sync::OnceLock::new();

/// Retourne le sender du thread GTK, en le créant au premier appel.
fn ensure_gtk_thread() -> &'static std::sync::mpsc::Sender<GtkAction> {
    GTK_TX.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<GtkAction>();
        std::thread::Builder::new()
            .name("gtk-ui".into())
            .spawn(move || {
                // Initialisation GTK/Adwaita UNE SEULE FOIS pour tout le processus.
                unsafe { std::env::set_var("ADWAITA_DISABLE_LEGACY_THEMING_WARNINGS", "1") };
                if libadwaita::init().is_err() {
                    tracing::error!("gtk-ui: libadwaita init failed");
                    return;
                }
                tracing::debug!("gtk-ui thread ready");

                while let Ok(action) = rx.recv() {
                    match action {
                        GtkAction::OpenSettings(cmd_tx) => {
                            if let Err(e) = super::settings::run_standalone(cmd_tx) {
                                tracing::warn!("settings window: {e}");
                            }
                        }
                        GtkAction::ShowAbout => {
                            run_about_app();
                        }
                    }
                }
                tracing::debug!("gtk-ui thread exiting");
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

/// Fenêtre À propos (§5A UX_SYSTRAY.md).
/// Exécutée sur le thread `gtk-ui` — GTK déjà initialisé.
fn run_about_app() {
    let app = gtk4::Application::builder()
        .application_id("fr.clyds.syncgdrive.about")
        .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
        .build();

    app.connect_activate(|app| {
        let about = libadwaita::AboutWindow::builder()
            .application_name("SyncGDrive")
            .version(env!("CARGO_PKG_VERSION"))
            .developer_name("clyds")
            .license_type(gtk4::License::MitX11)
            .comments("Synchronisation unidirectionnelle d'un dossier local vers Google Drive (ou tout backend KIO : SMB, SFTP, WebDAV).\n\nL'ordinateur local est la source de vérité — le Drive est la sauvegarde.")
            .website("https://github.com/clyds/SyncGDrive")
            .issue_url("https://github.com/clyds/SyncGDrive/issues")
            .application_icon("emblem-synchronizing-symbolic")
            .application(app)
            .build();
        about.present();
    });

    app.run_with_args::<String>(&[]);
}

