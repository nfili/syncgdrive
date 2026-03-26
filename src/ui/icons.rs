//! Gestion et rendu des icônes SVG "Dossier Premium" pour le systray (Phase 7).
//!
//! Ce module embarque statiquement les fichiers SVG dans le binaire final
//! (`include_bytes!`) et les rastérise à la volée en pixels ARGB32 requis
//! par le protocole D-Bus (KSNI). Cela garantit un affichage net sur n'importe
//! quel environnement de bureau Linux (KDE, GNOME, XFCE).

use resvg::tiny_skia::{Pixmap, Transform};
use resvg::usvg::{Options, Tree};

// ── Chargement statique des 10 icônes (embarquées dans le binaire) ──

const ICON_IDLE: &[u8] = include_bytes!("../../assets/icons/idle.svg");
const ICON_OFFLINE: &[u8] = include_bytes!("../../assets/icons/offline.svg");
const ICON_ERROR: &[u8] = include_bytes!("../../assets/icons/error.svg");
const ICON_PAUSED: &[u8] = include_bytes!("../../assets/icons/paused.svg");
const ICON_SCAN_FRAMES: &[&[u8]] = &[
    include_bytes!("../../assets/icons/scan_1.svg"),
    include_bytes!("../../assets/icons/scan_2.svg"),
    include_bytes!("../../assets/icons/scan_3.svg"),
    include_bytes!("../../assets/icons/scan_4.svg"),
    include_bytes!("../../assets/icons/scan_5.svg"),
    include_bytes!("../../assets/icons/scan_6.svg"),
    include_bytes!("../../assets/icons/scan_7.svg"),
    include_bytes!("../../assets/icons/scan_8.svg"),
];
const ICON_STARTING_FRAMES: &[&[u8]] = &[
    include_bytes!("../../assets/icons/starting_1.svg"),
    include_bytes!("../../assets/icons/starting_2.svg"),
    include_bytes!("../../assets/icons/starting_3.svg"),
    include_bytes!("../../assets/icons/starting_4.svg"),
    include_bytes!("../../assets/icons/starting_5.svg"),
    include_bytes!("../../assets/icons/starting_6.svg"),
    include_bytes!("../../assets/icons/starting_7.svg"),
    include_bytes!("../../assets/icons/starting_8.svg"),
];

const ICON_SETTINGS: &[u8] = include_bytes!("../../assets/icons/settings.svg");

// Frames d'animation (Dossier + Flèches vertes qui tournent)
const ICON_SYNC_FRAMES: &[&[u8]] = &[
    include_bytes!("../../assets/icons/sync_1.svg"),
    include_bytes!("../../assets/icons/sync_2.svg"),
    include_bytes!("../../assets/icons/sync_3.svg"),
    include_bytes!("../../assets/icons/sync_4.svg"),
    include_bytes!("../../assets/icons/sync_5.svg"),
    include_bytes!("../../assets/icons/sync_6.svg"),
    include_bytes!("../../assets/icons/sync_7.svg"),
    include_bytes!("../../assets/icons/sync_8.svg"),
];

const ICON_HELP: &[u8] = include_bytes!("../../assets/icons/help.svg");

/// Les différents états visuels possibles de l'icône dans la barre des tâches.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TrayIcon {
    Idle,
    /// Démarrage avec pourcentage d'avancement.
    Starting(u8),
    /// Animation de scan. Contient l'index de la frame (0..7).
    Scanning(usize),
    Offline,
    Error,
    Paused,
    /// Animation de transfert. Contient l'index de la frame (0..7).
    Sync(usize),
    Settings,
    Help,
}

/// Convertit une icône SVG en un buffer de pixels ARGB32 (format ksni/D-Bus).
pub fn get_icon_pixmap(icon: TrayIcon) -> Vec<u8> {
    let svg_data = match icon {
        TrayIcon::Idle => ICON_IDLE,
        TrayIcon::Starting(percent) => {
            // Mappage ultra-précis selon tes intervalles (0, 15, 30, 45, 60, 75, 90, 100)
            let index = match percent {
                0..=14 => 0,  // Frame 1 : 0%
                15..=29 => 1, // Frame 2 : 15%
                30..=44 => 2, // Frame 3 : 30%
                45..=59 => 3, // Frame 4 : 45%
                60..=74 => 4, // Frame 5 : 60%
                75..=89 => 5, // Frame 6 : 75%
                90..=99 => 6, // Frame 7 : 90%
                _ => 7,       // Frame 8 : 100% (et sécurité au-delà)
            };
            ICON_STARTING_FRAMES[index]
        }
        TrayIcon::Scanning(frame) => ICON_SCAN_FRAMES[frame % ICON_SCAN_FRAMES.len()],
        TrayIcon::Offline => ICON_OFFLINE,
        TrayIcon::Error => ICON_ERROR,
        TrayIcon::Paused => ICON_PAUSED,
        TrayIcon::Sync(frame) => ICON_SYNC_FRAMES[frame % ICON_SYNC_FRAMES.len()],
        TrayIcon::Settings => ICON_SETTINGS,
        TrayIcon::Help => ICON_HELP,
    };

    render_svg_to_argb32(svg_data, 24, 24)
}

/// Moteur de rendu SVG vers Pixels ARGB32 (API resvg >= 0.40).
fn render_svg_to_argb32(svg_data: &[u8], width: u32, height: u32) -> Vec<u8> {
    let opt = Options::default();

    // 1. Parsing direct du SVG
    let tree = Tree::from_data(svg_data, &opt).unwrap_or_else(|_| {
        // SVG de secours minimaliste en cas d'erreur
        Tree::from_data(b"<svg viewBox=\"0 0 24 24\"></svg>", &opt).unwrap()
    });

    let mut pixmap = Pixmap::new(width, height).expect("Dimensions invalides");

    // 2. Calcul du ratio d'échelle pour s'adapter à la taille demandée (24x24)
    let size = tree.size();
    let scale_x = width as f32 / size.width();
    let scale_y = height as f32 / size.height();
    let transform = Transform::from_scale(scale_x, scale_y);

    // 3. Rendu direct sur la pixmap
    resvg::render(&tree, transform, &mut pixmap.as_mut());

    // 4. Conversion RGBA (tiny-skia) vers ARGB32 (D-Bus StatusNotifierItem)
    let mut argb_buffer = Vec::with_capacity((width * height * 4) as usize);
    for pixel in pixmap.pixels() {
        argb_buffer.push(pixel.alpha());
        argb_buffer.push(pixel.red());
        argb_buffer.push(pixel.green());
        argb_buffer.push(pixel.blue());
    }

    argb_buffer
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_trayicon_enum_variants() {
        let _ = TrayIcon::Starting;
        let _ = TrayIcon::Scanning;
        let _ = TrayIcon::Sync(0);
    }

    #[test]
    fn test_svg_rendering_dimensions() {
        // La pixmap doit faire 24x24x4 = 2304 octets
        let buffer = get_icon_pixmap(TrayIcon::Idle);
        assert_eq!(buffer.len(), 2304);
    }
}
