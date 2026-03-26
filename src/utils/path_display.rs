//! Utilitaire de formatage des chemins pour l'interface graphique (Phase 7).
//!
//! Contient la logique permettant de raccourcir visuellement les chemins de fichiers
//! trop longs pour éviter de casser le rendu des infobulles (tooltips) et des
//! fenêtres de progression.

/// Découpe un chemin complet en deux parties : (Dossiers_parents, Nom_cible).
///
/// Si l'arborescence est très profonde (plus de 3 dossiers parents), le chemin
/// est intelligemment tronqué au milieu pour conserver le contexte principal.
///
/// # Exemples de formatage
/// * `"file.txt"` → `("", "file.txt")`
/// * `"a/b/c.rs"` → `("a/b/", "c.rs")`
/// * `"A/B/C/D/E/file.txt"` → `("A/.../D/E/", "file.txt")`
///
/// # Retours
/// Un tuple contenant `(Chemin_des_dossiers, Nom_du_fichier_ou_dossier_final)`.
pub fn split_path_display(path: &str) -> (String, String) {
    if path.is_empty() {
        return (String::new(), String::new());
    }

    // On gère le cas particulier où le chemin pointe vers un dossier (se termine par /).
    let is_dir = path.ends_with('/');
    let clean_path = path.trim_end_matches('/');

    let parts: Vec<&str> = clean_path.split('/').collect();

    if parts.is_empty() {
        return (String::new(), String::new());
    }

    if parts.len() == 1 {
        // Fichier (ou dossier) à la racine
        let name = if is_dir {
            format!("{}/", parts[0])
        } else {
            parts[0].to_string()
        };
        return (String::new(), name);
    }

    let target_name = parts.last().unwrap();
    let folders = &parts[..parts.len() - 1];

    // La magie de la réduction à 3 dossiers max (Racine + les 2 derniers parents)
    let folder_str = if folders.len() > 3 {
        format!(
            "{}/.../{}/{}/",
            folders[0],
            folders[folders.len() - 2],
            folders[folders.len() - 1]
        )
    } else {
        format!("{}/", folders.join("/"))
    };

    let final_name = if is_dir {
        format!("{}/", target_name)
    } else {
        target_name.to_string()
    };

    (folder_str, final_name)
}

/// Formate un chemin pour un affichage élégant sur deux lignes dans le systray.
///
/// Ajoute automatiquement des emojis (📂 pour les dossiers, 📄 pour les fichiers)
/// pour faciliter la lecture rapide par l'utilisateur.
pub fn format_path_tooltip(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }

    let (folders, file) = split_path_display(path);
    // On détecte si la cible finale est un dossier grâce au "/" laissé par `split_path_display`
    let is_dir = file.ends_with('/');

    if folders.is_empty() {
        // Juste un fichier/dossier à la racine, on affiche sur une seule ligne
        let icon = if is_dir { "📁" } else { "📄" };
        format!("{} {}", icon, file)
    } else {
        // Chemin complet formaté sur deux lignes pour une meilleure lisibilité
        let file_icon = if is_dir { "📁" } else { "📄" };
        format!("📂 {}\n{} {}", folders, file_icon, file)
    }
}

/// Ouvre une URI (dossier local ou lien web) avec le programme par défaut de l'OS.
///
/// Utilisé pour ouvrir le navigateur Web lors de l'authentification OAuth2,
/// ou le gestionnaire de fichiers système (ex: Nautilus, Dolphin) pour voir les logs.
pub fn open_external(target: &str) {
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(target).spawn();

    // (Optionnel) Si jamais tu portes le projet sur d'autres OS plus tard :
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd").args(["/C", "start", target]).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(target).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_path_display_root_file() {
        assert_eq!(
            split_path_display("file.txt"),
            ("".into(), "file.txt".into())
        );
    }

    #[test]
    fn test_split_path_display_nested() {
        assert_eq!(
            split_path_display("a/b/c.rs"),
            ("a/b/".into(), "c.rs".into())
        );
    }

    #[test]
    fn test_split_path_display_dir() {
        assert_eq!(split_path_display("a/b/"), ("a/".into(), "b/".into()));
    }

    #[test]
    fn test_split_path_display_long_path_truncation() {
        // Plus de 3 dossiers -> A/.../D/E/
        assert_eq!(
            split_path_display("A/B/C/D/E/file.txt"),
            ("A/.../D/E/".into(), "file.txt".into())
        );
    }

    #[test]
    fn test_format_path_tooltip_file() {
        assert_eq!(format_path_tooltip("a/b/c.rs"), "📂 a/b/\n📄 c.rs");
    }

    #[test]
    fn test_format_path_tooltip_long_path() {
        assert_eq!(
            format_path_tooltip(
                "home/user/documents/projets/syncgdrive/rapport_annuel.pdf"
            ),
            "📂 home/.../projets/syncgdrive/\n📄 rapport_annuel.pdf"
        );
    }

    #[test]
    fn test_format_path_tooltip_root() {
        assert_eq!(format_path_tooltip("file.txt"), "📄 file.txt");
    }
}