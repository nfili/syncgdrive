//! Fenêtre GTK4 affichant la progression du scan initial (Phase 7).

use gtk4::prelude::*;
use libadwaita::prelude::*;

#[derive(Clone)]
pub struct ScanWindow {
    pub window: libadwaita::Window,
    status_label: gtk4::Label,
    path_label: gtk4::Label,
    progress: gtk4::ProgressBar,
}

impl ScanWindow {
    pub fn new(app: &gtk4::Application) -> Self {
        let window = libadwaita::Window::builder()
            .application(app)
            .title("Synchronisation")
            .default_width(420)
            .modal(true)
            .deletable(false)
            .build();

        let vbox = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .spacing(12)
            .margin_top(24)
            .margin_bottom(24)
            .margin_start(24)
            .margin_end(24)
            .build();

        let spinner = gtk4::Spinner::builder()
            .spinning(true)
            .halign(gtk4::Align::Center)
            .width_request(32)
            .height_request(32)
            .build();

        let status_label = gtk4::Label::builder()
            .label("Démarrage de l'analyse...")
            .css_classes(["title-4"])
            .build();

        let path_label = gtk4::Label::builder()
            .ellipsize(gtk4::pango::EllipsizeMode::Middle)
            .css_classes(["dim-label"])
            .build();

        let progress = gtk4::ProgressBar::builder()
            .show_text(true)
            .build();

        vbox.append(&spinner);
        vbox.append(&status_label);
        vbox.append(&progress);
        vbox.append(&path_label);

        window.set_content(Some(&vbox));

        Self {
            window,
            status_label,
            path_label,
            progress,
        }
    }

    pub fn update(&self, phase_name: &str, done: usize, total: usize, current: &str) {
        self.status_label.set_label(phase_name);

        let (folders, file) = crate::utils::path_display::split_path_display(current);
        let display_path = if folders.is_empty() {
            file
        } else {
            format!("{}{}", folders, file)
        };
        self.path_label.set_label(&display_path);

        if total > 0 {
            self.progress.set_fraction(done as f64 / total as f64);
            self.progress.set_text(Some(&format!("{} / {}", done, total)));
        } else {
            self.progress.pulse();
            self.progress.set_text(Some(&format!("{} éléments...", done)));
        }
    }
}

/// Lance la fenêtre de scan en mode autonome et écoute les mises à jour du moteur.
pub fn run_standalone(rx: tokio::sync::watch::Receiver<crate::engine::EngineStatus>) {
    let app = gtk4::Application::builder()
        .application_id("fr.clyds.syncgdrive.scan")
        .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
        .build();

    app.connect_activate(move |app| {
        let win = ScanWindow::new(app);
        win.window.present();

        let win_clone = win.clone();
        let mut rx_clone = rx.clone();

        gtk4::glib::MainContext::default().spawn_local(async move {
            while rx_clone.changed().await.is_ok() {
                let status = rx_clone.borrow().clone();
                match status {
                    crate::engine::EngineStatus::ScanProgress { phase, done, total, current } => {
                        let phase_name = match phase {
                            crate::engine::ScanPhase::RemoteListing => "Analyse Google Drive...",
                            crate::engine::ScanPhase::LocalListing => "Analyse du disque local...",
                            crate::engine::ScanPhase::Directories => "Création des dossiers...",
                            crate::engine::ScanPhase::Comparing => "Comparaison des fichiers...",
                        };
                        win_clone.update(phase_name, done, total, &current);
                    }
                    crate::engine::EngineStatus::SyncProgress(_)
                    | crate::engine::EngineStatus::Syncing { .. }
                    | crate::engine::EngineStatus::Idle => {
                        win_clone.window.close();
                        break;
                    }
                    _ => {}
                }
            }
        });
    });

    app.run_with_args::<String>(&[]);
}