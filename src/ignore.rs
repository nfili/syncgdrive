//! Filtrage des fichiers et dossiers par patterns glob.
//!
//! Ce module fournit [`IgnoreMatcher`], un moteur compilé de patterns glob
//! (basé sur `globset`) utilisé pour exclure des fichiers ou répertoires
//! spécifiques du processus de synchronisation.
//!
//! # Patterns supportés
//!
//! La syntaxe glob est le standard industriel :
//!
//! | Pattern | Ce qui est exclu |
//! |---------|------------------|
//! | `**/target/**` | Tout dossier nommé `target/` et son contenu entier |
//! | `**/.git/**` | Tous les dépôts Git |
//! | `**/*.log` | Tous les fichiers se terminant par `.log` |
//! | `**/build/**` | Tous les dossiers nommés `build/` |
//!
//! # L'astuce du "trailing-slash"
//!
//! Un pattern comme `**/target/**` (qui se traduit en regex par `…target/.*$`)
//! ne matche techniquement jamais le dossier lui-même (`/home/…/target`) car il
//! exige du contenu après le `/`.
//! La méthode [`IgnoreMatcher::is_ignored`] gère ce cas en ajoutant dynamiquement
//! un `/` terminal lors de l'évaluation des répertoires.

use std::path::Path;

use anyhow::Result;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

/// Moteur compilé d'évaluation des patterns glob.
///
/// Cette structure est construite une seule fois au démarrage (ou lors d'un
/// rechargement de configuration) via [`from_patterns`](Self::from_patterns).
/// Elle centralise la logique d'exclusion pour le moteur de synchronisation.
///
/// # Exemple
///
/// ```rust
/// use sync_g_drive::ignore::IgnoreMatcher;
/// use std::path::Path;
///
/// let m = IgnoreMatcher::from_patterns(&[
///     "**/target/**".to_string(),
///     "**/*.log".to_string(),
/// ]).unwrap();
///
/// assert!(m.is_ignored(Path::new("/proj/target/debug/bin"), true));
/// assert!(m.is_ignored(Path::new("/var/log/app.log"),false));
/// assert!(!m.is_ignored(Path::new("/proj/src/main.rs"),false));
/// ```
pub struct IgnoreMatcher {
    set: GlobSet,
}

impl IgnoreMatcher {
    /// Compile une liste de chaînes de caractères (patterns) en un matcher optimisé.
    ///
    /// # Erreurs
    /// Retourne une erreur `anyhow::Result` si l'un des patterns fournis possède
    /// une syntaxe glob invalide. Une liste vide produira simplement un matcher
    /// qui laisse tout passer (n'ignore rien).
    pub fn from_patterns(patterns: &[String]) -> Result<Self> {
        let mut builder = GlobSetBuilder::new();
        for p in patterns {
            let glob = GlobBuilder::new(p)
                .literal_separator(false)
                .build()
                .map_err(|e| anyhow::anyhow!("Pattern glob invalide '{}': {}", p, e))?;
            builder.add(glob);
        }
        Ok(Self {
            set: builder.build()?,
        })
    }

    /// Évalue si le chemin spécifié doit être ignoré par la synchronisation.
    ///
    /// # Fonctionnement interne
    /// 1. Tente un match direct (idéal pour les fichiers ou les sous-éléments).
    /// 2. Si le chemin est un dossier, ajoute un `/` virtuel et retente le match
    ///    pour satisfaire les patterns de type `**/dossier/**`.
    pub fn is_ignored(&self, path: &Path, is_directory: bool) -> bool {
        if self.set.is_match(path) {
            return true;
        }

        // On utilise la RAM, pas le disque !
        if is_directory {
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

        let m = IgnoreMatcher::from_patterns(&["**/syncgdrive_test_ignore_dir/**".to_string()])
            .unwrap();

        assert!(m.is_ignored(&dir, dir.is_dir()), "directory itself should be ignored");
        assert!(
            m.is_ignored(&dir.join("some_file.rs"), dir.is_dir()),
            "file inside ignored dir should be ignored"
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn file_outside_ignored_dir_not_affected() {
        let m = IgnoreMatcher::from_patterns(&["**/target/**".to_string()]).unwrap();

        let p = Path::new("/home/user/project/src/main.rs");
        assert!(!m.is_ignored(p, p.is_dir()));
    }

    #[test]
    fn multiple_patterns() {
        let m = IgnoreMatcher::from_patterns(&[
            "**/target/**".into(),
            "**/.git/**".into(),
            "**/node_modules/**".into(),
        ])
            .unwrap();

        // On passe 'false' car ce sont des fichiers
        assert!(m.is_ignored(Path::new("/proj/target/debug/bin"), false));
        assert!(m.is_ignored(Path::new("/proj/.git/config"), false));
        assert!(m.is_ignored(Path::new("/proj/node_modules/lodash/index.js"), false));
        assert!(!m.is_ignored(Path::new("/proj/src/lib.rs"), false));
    }

    #[test]
    fn empty_patterns_ignores_nothing() {
        let m = IgnoreMatcher::from_patterns(&[]).unwrap();
        assert!(!m.is_ignored(Path::new("/any/path.txt"), false));
    }

    #[test]
    fn deeply_nested_match() {
        let m = IgnoreMatcher::from_patterns(&["**/.git/**".into()]).unwrap();

        assert!(m.is_ignored(Path::new("/a/b/c/.git/objects/pack/abc"), false));
    }

    #[test]
    fn extension_pattern() {
        let m = IgnoreMatcher::from_patterns(&["**/*.log".into()]).unwrap();

        assert!(m.is_ignored(Path::new("/var/log/app.log"), false));
        assert!(!m.is_ignored(Path::new("/var/log/app.txt"), false));
    }
}