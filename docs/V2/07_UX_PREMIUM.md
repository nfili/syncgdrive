# Phase 7 — UX Premium (Icônes SVG, Animation, Fenêtre Scan, Chemins Lisibles)

---

## 1. Objectif

Passer d'une UX fonctionnelle (V1 : noms d'icônes FreeDesktop, texte plat) à une UX premium (icônes SVG embarquées, animation pendant les transferts, fenêtre de scan initial, chemins contextualisés).

---

## 2. Pré-requis

- **Phase 4** : Toutes les constantes dans la config (timeouts, intervalles).
- **Phase 5** : Progression en octets disponible (pour la fenêtre de scan et les tooltips).

---

## 3. Fichiers Impactés

| Action | Fichier | Description |
|--------|---------|-------------|
| **Créer** | `assets/icons/*.svg` | 6 icônes statiques + 4 frames animation |
| **Créer** | `src/ui/icons.rs` | Chargement et rendu SVG → pixmap ARGB32 |
| **Créer** | `src/ui/scan_window.rs` | Fenêtre de progression du scan initial |
| **Créer** | `src/utils/path_display.rs` | `split_path_display()` et formatage chemins |
| **Modifier** | `src/ui/tray.rs` | `icon_pixmap()`, animation, tooltips enrichis |
| **Modifier** | `src/ui/mod.rs` | Déclarations modules |
| **Modifier** | `src/engine/mod.rs` | `GtkAction::ShowScanProgress`, `GtkAction::CloseScanProgress` |
| **Modifier** | `build.rs` | Compilation GResource (optionnel) ou `include_bytes!` |

---

## 4. Spécification Détaillée

### 4.1 Icônes SVG

**Fichiers à créer** dans `assets/icons/` :

```
assets/icons/
├── syncgdrive-idle.svg          # Coche verte / nuage calme
├── syncgdrive-paused.svg        # Symbole pause, couleur neutre
├── syncgdrive-error.svg         # Exclamation rouge
├── syncgdrive-offline.svg       # Nuage barré
├── syncgdrive-scan.svg          # Loupe / radar
├── syncgdrive-sync-0.svg        # Animation frame 0 (flèches ↑)
├── syncgdrive-sync-1.svg        # Animation frame 1 (flèches ↗)
├── syncgdrive-sync-2.svg        # Animation frame 2 (flèches →)
└── syncgdrive-sync-3.svg        # Animation frame 3 (flèches ↘)
```

**Contraintes SVG** :
- ViewBox : `0 0 24 24`
- Taille cible rendu : 24×24 px (systray) et 48×48 px (fenêtres)
- Couleur : mono-couleur (`currentColor`) pour les variantes `-symbolic`
- Optimisés avec `svgo` (< 1 Ko chacune)

### 4.2 Chargement et Rendu

```rust
// src/ui/icons.rs

use once_cell::sync::Lazy;

/// Icônes SVG embarquées dans le binaire.
pub struct IconSet {
    pub idle: &'static [u8],
    pub paused: &'static [u8],
    pub error: &'static [u8],
    pub offline: &'static [u8],
    pub scan: &'static [u8],
    pub sync_frames: [&'static [u8]; 4],
}

pub static ICONS: Lazy<IconSet> = Lazy::new(|| IconSet {
    idle: include_bytes!("../../assets/icons/syncgdrive-idle.svg"),
    paused: include_bytes!("../../assets/icons/syncgdrive-paused.svg"),
    error: include_bytes!("../../assets/icons/syncgdrive-error.svg"),
    offline: include_bytes!("../../assets/icons/syncgdrive-offline.svg"),
    scan: include_bytes!("../../assets/icons/syncgdrive-scan_1.svg"),
    sync_frames: [
        include_bytes!("../../assets/icons/syncgdrive-sync-0.svg"),
        include_bytes!("../../assets/icons/syncgdrive-sync-1.svg"),
        include_bytes!("../../assets/icons/syncgdrive-sync-2.svg"),
        include_bytes!("../../assets/icons/syncgdrive-sync-3.svg"),
    ],
});

/// Rend un SVG en pixmap ARGB32 pour ksni.
pub fn svg_to_argb32(svg_data: &[u8], size: u32) -> Vec<u8> {
    // Utiliser `resvg` + `tiny-skia` pour le rendu
    // Retourner les pixels en ARGB32 (format ksni)
    ...
}
```

### 4.3 Animation Systray

**Principe** : Pendant `Syncing` ou `ScanProgress`, une tâche Tokio alterne les frames.

```rust
// Dans tray.rs — boucle de status
loop {
    tokio::select! {
        biased;
        _ = sd.cancelled() => break,

        // Animation : tick toutes les 300ms si en cours de sync
        _ = animation_tick.tick(), if is_animating => {
            frame_index = (frame_index + 1) % 4;
            handle.update(|tray: &mut SyncTray| {
                tray.animation_frame = frame_index;
            }).await;
        }

        maybe = status_rx.recv() => {
            // ... traitement normal
            // Activer/désactiver l'animation selon l'état
            is_animating = matches!(s,
                EngineStatus::SyncProgress{..} |
                EngineStatus::ScanProgress{..}
            );
        }
    }
}
```

**Dans `icon_pixmap()`** :
```rust
fn icon_pixmap(&self) -> Vec<ksni::Icon> {
    let svg = match &*self.status.lock().unwrap() {
        EngineStatus::Idle => &ICONS.idle,
        EngineStatus::SyncProgress{..} | EngineStatus::ScanProgress{..} =>
            &ICONS.sync_frames[self.animation_frame],
        EngineStatus::Paused => &ICONS.paused,
        EngineStatus::Error(_) => &ICONS.error,
        EngineStatus::Offline => &ICONS.offline,
        _ => &ICONS.idle,
    };
    let pixels = svg_to_argb32(svg, 24);
    vec![ksni::Icon { width: 24, height: 24, data: pixels }]
}
```

### 4.4 Fenêtre de Scan Initial

**Déclenchement** : Le moteur détecte un "premier scan" (DB vide ou `local_root` changé) et envoie `GtkAction::ShowScanProgress`.

```rust
// engine/mod.rs
if self.is_first_scan() {
    if let Some(gtk_tx) = GTK_TX.get() {
        let _ = gtk_tx.send(GtkAction::ShowScanProgress);
    }
}
```

**Fenêtre** (`ui/scan_window.rs`) :

```rust
pub fn build_scan_window(
    progress_rx: glib::Receiver<ScanUpdate>,
) -> libadwaita::Window {
    let win = libadwaita::Window::builder()
        .title("SyncGDrive — Scan initial")
        .default_width(500)
        .default_height(380)
        .deletable(true)       // ✕ ferme mais n'arrête pas le scan
        .build();

    // Widgets :
    // - Label titre "🔍 Analyse de vos fichiers en cours…"
    // - Label explicatif
    // - ProgressBar phase courante
    // - Label "📂 parent_dir"
    // - Label "📄 current_name"
    // - Label "⏱ Temps écoulé : X min Y s"
    // - Bouton "Réduire dans le systray"

    // Écoute des mises à jour via glib channel
    progress_rx.attach(None, move |update| {
        // Mettre à jour les barres et labels
        glib::ControlFlow::Continue
    });

    win
}
```

**Communication** : `glib::MainContext::channel` entre le thread `gtk-ui` et le moteur.

**Fermeture automatique** : Quand le moteur envoie `ScanUpdate::Completed`, la fenêtre se ferme.

### 4.5 Fonction `split_path_display()`

```rust
// src/utils/path_display.rs

/// Sépare un chemin relatif en (répertoire_parent, nom_élément).
///
/// # Exemples
/// ```
/// assert_eq!(split_path_display("src/engine/scan.rs"), ("src/engine/", "scan.rs"));
/// assert_eq!(split_path_display("src/engine/"),        ("src/", "engine/"));
/// assert_eq!(split_path_display("README.md"),           ("", "README.md"));
/// assert_eq!(split_path_display(""),                     ("", ""));
/// ```
pub fn split_path_display(relative: &str) -> (&str, &str) {
    let trimmed = relative.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(pos) => (&relative[..=pos], &trimmed[pos + 1..]),
        None => ("", trimmed),
    }
}

/// Formate un chemin pour le tooltip avec emojis.
pub fn format_path_tooltip(relative: &str, is_dir: bool) -> String {
    let (parent, name) = split_path_display(relative);
    let icon = if is_dir { "📁" } else { "📄" };
    if parent.is_empty() {
        format!("{icon} {name}")
    } else {
        format!("📂 {parent}\n{icon} {name}")
    }
}
```

### 4.6 Tooltip V2 — Intégration

```rust
fn tool_tip(&self) -> ksni::ToolTip {
    match &*self.status.lock().unwrap() {
        EngineStatus::SyncProgress {
            done, total, current_dir, current_name,
            size_bytes, speed_bps, total_bytes, total_bytes_sent, ..
        } => {
            let pct = if *total_bytes > 0 {
                (*total_bytes_sent as f64 / *total_bytes as f64) * 100.0
            } else { 0.0 };
            let bar = progress_bar(pct, 20);
            let speed = human_size(*speed_bps);
            let eta = /* calcul ETA */;

            let description = format!(
                "Transfert {done}/{total}\n\
                 📂 {current_dir}\n\
                 📄 {current_name} ({})\n\
                 {bar} {pct:.0}% · {speed}/s · {eta}\n\
                 Total : {} / {}",
                human_size(*size_bytes),
                human_size(*total_bytes_sent),
                human_size(*total_bytes),
            );
            // ...
        }
        // ... autres états
    }
}
```

---

## 5. Cas Limites

| Cas | Comportement attendu |
|-----|---------------------|
| Pas de librsvg installé | `resvg` (pure Rust) = pas de dépendance système |
| Thème sombre / clair | SVG mono-couleur (`currentColor`) s'adapte |
| Systray ne supporte pas `icon_pixmap` | Fallback sur `icon_name()` (noms FreeDesktop comme V1) |
| Fenêtre scan fermée par l'utilisateur | Scan continue — pas de Pause |
| Scan terminé en < 2 secondes | Fenêtre ne s'affiche pas (seuil minimum configurable) |
| Fichier à la racine de `local_root` | `split_path_display("file.txt")` → `("", "file.txt")` |

---

## 6. Tests à Écrire

### Unitaires

- `test_split_path_display_nested` : `a/b/c.rs` → `("a/b/", "c.rs")`
- `test_split_path_display_root_file` : `file.txt` → `("", "file.txt")`
- `test_split_path_display_dir` : `a/b/` → `("a/", "b/")`
- `test_split_path_display_empty` : `""` → `("", "")`
- `test_format_path_tooltip_file` : `a/b/c.rs` → `"📂 a/b/\n📄 c.rs"`
- `test_format_path_tooltip_dir` : `a/b/` → `"📂 a/\n📁 b/"`
- `test_format_path_tooltip_root` : `file.txt` → `"📄 file.txt"`
- `test_svg_to_argb32_dimensions` : rendu → buffer de taille 24*24*4 octets

---

## 7. Critères d'Acceptation

- [ ] Les 10 fichiers SVG sont créés et optimisés (< 1 Ko chacun)
- [ ] L'icône systray utilise les pixmaps SVG (pas les noms FreeDesktop)
- [ ] L'animation fonctionne pendant Syncing/ScanProgress (4 frames, ~300ms)
- [ ] L'animation s'arrête quand l'état change
- [ ] La fenêtre de scan initial s'affiche au premier lancement
- [ ] La fenêtre se ferme automatiquement à la fin du scan
- [ ] Le bouton « Réduire » ferme la fenêtre sans arrêter le scan
- [ ] Les tooltips affichent les chemins avec `📂 parent / 📄 fichier`
- [ ] `split_path_display()` est correct sur tous les cas limites
- [ ] Fallback sur noms FreeDesktop si pixmap non supporté
- [ ] `cargo test` et `cargo clippy` : clean

