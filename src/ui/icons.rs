//! Gestion et rendu des icônes SVG "Dossier Premium" pour le systray (Phase 7).

use resvg::tiny_skia::{Pixmap, Transform};
use resvg::usvg::{Options, Tree};

// ── Chargement statique des 10 icônes (embarquées dans le binaire) ──

const ICON_IDLE: &[u8] = include_bytes!("../../assets/icons/idle.svg");
const ICON_OFFLINE: &[u8] = include_bytes!("../../assets/icons/offline.svg");
const ICON_ERROR: &[u8] = include_bytes!("../../assets/icons/error.svg");
const ICON_PAUSED: &[u8] = include_bytes!("../../assets/icons/paused.svg");
const ICON_SCAN: &[u8] = include_bytes!("../../assets/icons/scan.svg");
const ICON_STARTING: &[u8] = include_bytes!("../../assets/icons/starting.svg");

// Frames d'animation (Dossier + Flèches vertes qui tournent)
const ICON_SYNC_FRAMES: &[&[u8]] = &[
    include_bytes!("../../assets/icons/sync_1.svg"),
    include_bytes!("../../assets/icons/sync_2.svg"),
    include_bytes!("../../assets/icons/sync_3.svg"),
    include_bytes!("../../assets/icons/sync_4.svg"),
];

/// Les différents états visuels possibles de l'icône dans la barre des tâches.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TrayIcon {
    Idle,
    Starting,
    Scanning,
    Offline,
    Error,
    Paused,
    Sync(usize), // Contient l'index de la frame (0..3)
}

/// Convertit une icône SVG en un buffer de pixels ARGB32 (format ksni/D-Bus).
pub fn get_icon_pixmap(icon: TrayIcon) -> Vec<u8> {
    let svg_data = match icon {
        TrayIcon::Idle => ICON_IDLE,
        TrayIcon::Starting => ICON_STARTING,
        TrayIcon::Scanning => ICON_SCAN,
        TrayIcon::Offline => ICON_OFFLINE,
        TrayIcon::Error => ICON_ERROR,
        TrayIcon::Paused => ICON_PAUSED,
        TrayIcon::Sync(frame) => ICON_SYNC_FRAMES[frame % ICON_SYNC_FRAMES.len()],
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