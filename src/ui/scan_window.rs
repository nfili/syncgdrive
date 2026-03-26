//! Fenêtre de scan GTK4 (Mode Premium).
//!
//! Affiche de manière asynchrone la progression de l'analyse initiale
//! (comparaison locale vs cloud) avant de démarrer la synchronisation réelle.

use gtk4::prelude::*;
use libadwaita::prelude::*;

/// Composant visuel (Widget) responsable du dashboard d'analyse.
#[derive(Clone)]
pub struct ScanWindow {
    pub window: libadwaita::Window,
    phase_label: gtk4::Label,
    path_label: gtk4::Label,
    progress: gtk4::ProgressBar,
}

impl ScanWindow {
    /// Construit l'arbre de widgets Libadwaita.
    pub fn new(app: &libadwaita::Application) -> Self {
        let window = libadwaita::Window::builder()
            .application(app)
            .title("Synchronisation SyncGDrive")
            .default_width(540) // Fenêtre un peu plus large pour respirer
            .default_height(380)
            .modal(true)
            .deletable(false) // On empêche la fermeture par la croix pour ne pas corrompre le statut
            .build();

        // ── En-tête natif GNOME/Libadwaita ──
        let header_bar = libadwaita::HeaderBar::builder()
            .show_start_title_buttons(false)
            .show_end_title_buttons(false)
            .build();

        let main_vbox = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .build();
        main_vbox.append(&header_bar);

        // ── Conteneur central (Clamp) pour un look Premium ──
        let clamp = libadwaita::Clamp::builder()
            .maximum_size(460) // Le contenu ne dépassera jamais cette largeur
            .margin_top(32)
            .margin_bottom(32)
            .margin_start(24)
            .margin_end(24)
            .build();

        let content_box = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .spacing(24)
            .build();

        // 1. Animation de chargement géante
        let spinner = gtk4::Spinner::builder()
            .spinning(true)
            .halign(gtk4::Align::Center)
            .width_request(64)
            .height_request(64)
            .build();

        // 2. Grand titre de la phase en cours
        let phase_label = gtk4::Label::builder()
            .label("Initialisation du moteur...")
            .css_classes(["title-2"]) // Grosse police claire et moderne
            .halign(gtk4::Align::Center)
            .build();

        // 3. Carte "Dashboard" stylisée pour la progression
        let card_box = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .css_classes(["card"]) // Applique le fond blanc/gris arrondi de Libadwaita
            .build();

        let progress_vbox = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Vertical)
            .spacing(12)
            .margin_top(20)
            .margin_bottom(20)
            .margin_start(20)
            .margin_end(20)
            .build();

        let progress = gtk4::ProgressBar::builder()
            .show_text(true)
            .valign(gtk4::Align::Center)
            .build();

        let path_label = gtk4::Label::builder()
            .ellipsize(gtk4::pango::EllipsizeMode::Middle)
            .css_classes(["dim-label", "monospace"]) // Police technique et adoucie
            .halign(gtk4::Align::Center)
            .build();

        progress_vbox.append(&progress);
        progress_vbox.append(&path_label);
        card_box.append(&progress_vbox);

        // ── Assemblage final ──
        content_box.append(&spinner);
        content_box.append(&phase_label);
        content_box.append(&card_box);

        clamp.set_child(Some(&content_box));
        main_vbox.append(&clamp);

        window.set_content(Some(&main_vbox));

        Self {
            window,
            phase_label,
            path_label,
            progress,
        }
    }

    /// Met à jour les valeurs affichées à l'écran.
    pub fn update(&self, phase_name: &str, done: usize, total: usize, current: &str) {
        self.phase_label.set_label(phase_name);

        let display_path = if current.is_empty() {
            "Préparation...".to_string()
        } else {
            current.to_string()
        };
        self.path_label.set_label(&display_path);

        if total > 0 {
            // Mode calculable : mise à jour de la barre de fraction
            self.progress.set_fraction(done as f64 / total as f64);
            self.progress
                .set_text(Some(&format!("{} / {}", done, total)));
        } else {
            // Mode indéterminé : la barre fait un va-et-vient (pulse)
            self.progress.pulse();
            self.progress
                .set_text(Some(&format!("{} éléments analysés...", done)));
        }
    }
}

/// Affiche la fenêtre de scan et écoute les mises à jour en provenance du moteur.
///
/// Attache un watcher `tokio::sync::watch` sur la boucle GLib `spawn_local`.
pub fn show_scan_window_in_app(
    app: &libadwaita::Application,
    mut rx: tokio::sync::watch::Receiver<crate::engine::EngineStatus>,
) {
    // 1. On crée et on affiche la fenêtre immédiatement
    let win = ScanWindow::new(app);
    win.window.present();

    let win_clone = win.clone();

    // 2. Écouteur asynchrone attaché à la boucle principale de GTK (GLib event loop)
    gtk4::glib::MainContext::default().spawn_local(async move {
        while rx.changed().await.is_ok() {
            let status = rx.borrow().clone();
            match status {
                crate::engine::EngineStatus::ScanProgress {
                    phase,
                    done,
                    total,
                    current,
                } => {
                    let phase_name = match phase {
                        crate::engine::ScanPhase::RemoteListing => "Analyse de Google Drive",
                        crate::engine::ScanPhase::LocalListing => "Inventaire du disque local",
                        crate::engine::ScanPhase::Directories => "Vérification des dossiers",
                        crate::engine::ScanPhase::Comparing => "Comparaison des données",
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
}