//! Point d'entrée de l'interface graphique GTK4 / Libadwaita.
//!
//! Ce module gère la boucle principale (Main Loop) de l'interface utilisateur.
//! Il établit un pont de communication asynchrone entre le moteur de synchronisation
//! (qui tourne sous Tokio) et l'interface graphique (qui tourne sous GLib/GTK).

use gtk4::prelude::*;
use tokio::sync::mpsc;

pub mod help_window;
pub mod icons;
pub mod scan_window;
pub mod settings;
pub mod tray;

pub use tray::spawn_tray;

/// Commandes envoyées depuis le Systray ou le moteur vers le thread GTK.
pub enum UiCommand {
    ShowHelp,
    ShowSettings,
    ShowAbout,
    ShowScanWindow,
}

/// Démarre le serveur UI dans un thread OS dédié.
///
/// Cette fonction initialise l'application Libadwaita, configure le thème sombre,
/// et maintient l'application en vie via un `hold_guard` tant que le canal de
/// commandes (`ui_rx`) reste ouvert.
pub fn start_ui_server(
    cmd_tx: tokio::sync::mpsc::Sender<crate::engine::EngineCommand>,
) -> mpsc::UnboundedSender<UiCommand> {
    let (ui_tx, ui_rx) = mpsc::unbounded_channel::<UiCommand>();

    std::thread::spawn(move || {
        tracing::info!("GTK: Démarrage du thread d'interface...");

        let app = libadwaita::Application::builder()
            .application_id("fr.clyds.syncgdrive.ui")
            .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
            .build();

        app.connect_startup(|_| {
            // Tu peux choisir :
            // – ColorScheme::PreferDark (Force le thème sombre, très élégant)
            // - ColorScheme::Default (Suit le mode clair/sombre du système de manière dynamique).
            libadwaita::StyleManager::default()
                .set_color_scheme(libadwaita::ColorScheme::PreferDark);
        });

        let rx_rc = std::rc::Rc::new(std::cell::RefCell::new(Some(ui_rx)));

        // 🌟 TOUT SE PASSE ICI : au moment où l'application est activée par le système
        app.connect_activate(move |app| {
            tracing::info!("GTK: Application activée !");

            if let Some(mut rx) = rx_rc.borrow_mut().take() {
                let app_clone = app.clone();
                let cmd_tx_clone = cmd_tx.clone();

                // 🌟 CORRECTION : On crée le gardien et on le garde précieusement
                let hold_guard = app.hold();

                gtk4::glib::MainContext::default().spawn_local(async move {
                    // 🌟 On déplace le gardien dans cette boucle pour qu'il ne meure pas !
                    let _keep_alive = hold_guard;

                    tracing::info!("GTK: Boucle d'écoute prête !");

                    while let Some(cmd) = rx.recv().await {
                        tracing::info!("GTK: Commande reçue depuis le menu !");
                        match cmd {
                            UiCommand::ShowHelp => crate::ui::help_window::show_help_in_app(
                                &app_clone,
                                cmd_tx_clone.clone(),
                            ),
                            UiCommand::ShowSettings => crate::ui::settings::show_settings_in_app(
                                &app_clone,
                                cmd_tx_clone.clone(),
                            ),
                            UiCommand::ShowAbout => crate::ui::tray::show_about_in_app(&app_clone),
                            UiCommand::ShowScanWindow => {
                                crate::ui::scan_window::show_scan_window_in_app(
                                    &app_clone,
                                    crate::ui::tray::get_scan_rx(),
                                )
                            }
                        }
                    }
                    tracing::info!("GTK: Canal de communication fermé.");
                    // Quand on quittera le programme, `_keep_alive` sera détruit et fermera GTK proprement.
                    app_clone.quit();
                });
            }
        });

        // 🌟 On utilise app.run() standard pour qu'il gère les arguments système proprement
        let exit_code = app.run();
        tracing::warn!("GTK: Le thread s'est arrêté (Code: {:?})", exit_code);
    });

    ui_tx
}
