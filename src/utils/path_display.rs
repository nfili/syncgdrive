//! Utilitaire de formatage des chemins pour l'interface graphique (Phase 7).

/// Découpe un chemin en (Dossiers_parents, Nom_du_fichier_ou_dossier).
/// Réduit les dossiers intermédiaires si la profondeur dépasse 3.
pub fn split_path_display(path: &str) -> (String, String) {
    if path.is_empty() {
        return (String::new(), String::new());
    }

    // On gère le cas particulier où le chemin pointe vers un dossier (se termine par /)
    let is_dir = path.ends_with('/');
    let clean_path = path.trim_end_matches('/');

    let parts: Vec<&str> = clean_path.split('/').collect();

    if parts.is_empty() {
        return (String::new(), String::new());
    }

    if parts.len() == 1 {
        // Fichier (ou dossier) à la racine
        let name = if is_dir { format!("{}/", parts[0]) } else { parts[0].to_string() };
        return (String::new(), name);
    }

    let target_name = parts.last().unwrap();
    let folders = &parts[..parts.len() - 1];

    // La magie de la réduction à 3 dossiers max
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

    let final_name = if is_dir { format!("{}/", target_name) } else { target_name.to_string() };

    (folder_str, final_name)
}

/// Formate le chemin pour le tooltip sur deux lignes avec des icônes.
pub fn format_path_tooltip(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }

    let (folders, file) = split_path_display(path);
    let is_dir = file.ends_with('/');

    if folders.is_empty() {
        // Juste un fichier à la racine
        let icon = if is_dir { "📁" } else { "📄" };
        format!("{} {}", icon, file)
    } else {
        // Chemin complet formaté sur deux lignes
        let file_icon = if is_dir { "📁" } else { "📄" };
        format!("📂 {}\n{} {}", folders, file_icon, file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_path_display_root_file() {
        assert_eq!(split_path_display("file.txt"), ("".into(), "file.txt".into()));
    }

    #[test]
    fn test_split_path_display_nested() {
        assert_eq!(split_path_display("a/b/c.rs"), ("a/b/".into(), "c.rs".into()));
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
            format_path_tooltip("Users/clyds/Projets/UltraFsCloud/ultracloudfs-admin/2026_saint_valentin_v2.xcf"),
            "📂 Users/.../UltraFsCloud/ultracloudfs-admin/\n📄 2026_saint_valentin_v2.xcf"
        );
    }

    #[test]
    fn test_format_path_tooltip_root() {
        assert_eq!(format_path_tooltip("file.txt"), "📄 file.txt");
    }
}