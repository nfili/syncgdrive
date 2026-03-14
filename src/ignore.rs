//! Filtrage des fichiers et dossiers par patterns glob.
//!
//! Ce module fournit [`IgnoreMatcher`], un matcher compilé de patterns glob
//! utilisé pour exclure des fichiers/dossiers de la synchronisation.
//!
//! # Patterns supportés
//!
//! La syntaxe glob est celle de la crate [`globset`] :
//!
//! | Pattern | Exclut |
//! |---------|--------|
//! | `**/target/**` | Tout dossier `target/` et son contenu |
//! | `**/.git/**` | Tous les dépôts Git |
//! | `**/*.log` | Tous les fichiers `.log` |
//! | `**/build/**` | Tous les dossiers `build/` |
//!
//! # Astuce trailing-slash
//!
//! Un pattern comme `**/target/**` (regex `…target/.*$`) ne matche jamais
//! le dossier lui-même (`/home/…/target`) car il exige du contenu après le `/`.
//! [`IgnoreMatcher::is_ignored`] ajoute automatiquement un `/` terminal
//! pour les répertoires, permettant au glob de matcher le dossier en plus
//! de ses enfants.

use std::path::Path;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use anyhow::Result;

/// Matcher compilé de patterns glob pour le filtrage des fichiers.
///
/// Construit une seule fois via [`from_patterns`](Self::from_patterns) à partir
/// de la liste `ignore_patterns` de [`AppConfig`](crate::config::AppConfig).
/// Rechargé automatiquement lors d'un `ApplyConfig` (hot-reload).
///
/// # Exemple
///
/// ```
/// use sync_g_drive::ignore::IgnoreMatcher;
/// use std::path::Path;
///
/// let m = IgnoreMatcher::from_patterns(&[
///     "**/target/**".to_string(),
///     "**/*.log".to_string(),
/// ]).unwrap();
///
/// assert!(m.is_ignored(Path::new("/proj/target/debug/bin")));
/// assert!(m.is_ignored(Path::new("/var/log/app.log")));
/// assert!(!m.is_ignored(Path::new("/proj/src/main.rs")));
/// ```
pub struct IgnoreMatcher {
    set: GlobSet,
}

impl IgnoreMatcher {
    /// Compile une liste de patterns glob en un matcher efficace.
    ///
    /// Retourne une erreur si un pattern est syntaxiquement invalide.
    /// Une liste vide produit un matcher qui n'ignore rien.
    pub fn from_patterns(patterns: &[String]) -> Result<Self> {
        let mut builder = GlobSetBuilder::new();
        for p in patterns {
            let glob = GlobBuilder::new(p)
                .literal_separator(false)
                .build()
                .map_err(|e| anyhow::anyhow!("invalid glob pattern '{p}': {e}"))?;
            builder.add(glob);
        }
        Ok(Self { set: builder.build()? })
    }

    /// Retourne `true` si `path` correspond à l'un des patterns d'exclusion.
    ///
    /// Pour les répertoires, teste aussi avec un `/` terminal.
    /// Sans ça, un pattern `**/target/**` (regex `…target/.*$`) ne matche
    /// jamais le dossier lui-même (`/home/…/target`) car il exige du
    /// contenu après le `/`.
    pub fn is_ignored(&self, path: &Path) -> bool {
        // Test direct (fonctionne pour les fichiers à l'intérieur d'un dir ignoré,
        // ex: /home/…/target/debug/foo → matche **/target/**)
        if self.set.is_match(path) {
            return true;
        }

        // Pour les dossiers : ajouter un `/` terminal pour que le glob
        // puisse matcher le dossier lui-même (pas seulement ses enfants).
        if path.is_dir() {
            let mut s = path.as_os_str().to_os_string();
            s.push("/");
            return self.set.is_match(Path::new(&s));
        }

        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn dir_pattern_matches_directory_itself() {
        let dir = std::env::temp_dir().join("syncgdrive_test_ignore_dir");
        fs::create_dir_all(&dir).unwrap();

        let m = IgnoreMatcher::from_patterns(&[
            "**/syncgdrive_test_ignore_dir/**".to_string(),
        ]).unwrap();

        assert!(m.is_ignored(&dir), "directory itself should be ignored");
        assert!(m.is_ignored(&dir.join("some_file.rs")),
            "file inside ignored dir should be ignored");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn file_outside_ignored_dir_not_affected() {
        let m = IgnoreMatcher::from_patterns(&[
            "**/target/**".to_string(),
        ]).unwrap();

        let p = Path::new("/home/user/project/src/main.rs");
        assert!(!m.is_ignored(p));
    }

    #[test]
    fn multiple_patterns() {
        let m = IgnoreMatcher::from_patterns(&[
            "**/target/**".into(),
            "**/.git/**".into(),
            "**/node_modules/**".into(),
        ]).unwrap();

        assert!(m.is_ignored(Path::new("/proj/target/debug/bin")));
        assert!(m.is_ignored(Path::new("/proj/.git/config")));
        assert!(m.is_ignored(Path::new("/proj/node_modules/lodash/index.js")));
        assert!(!m.is_ignored(Path::new("/proj/src/lib.rs")));
    }

    #[test]
    fn empty_patterns_ignores_nothing() {
        let m = IgnoreMatcher::from_patterns(&[]).unwrap();
        assert!(!m.is_ignored(Path::new("/any/path.txt")));
    }

    #[test]
    fn deeply_nested_match() {
        let m = IgnoreMatcher::from_patterns(&[
            "**/.git/**".into(),
        ]).unwrap();

        assert!(m.is_ignored(Path::new("/a/b/c/.git/objects/pack/abc")));
    }

    #[test]
    fn extension_pattern() {
        let m = IgnoreMatcher::from_patterns(&[
            "**/*.log".into(),
        ]).unwrap();

        assert!(m.is_ignored(Path::new("/var/log/app.log")));
        assert!(!m.is_ignored(Path::new("/var/log/app.txt")));
    }
}

