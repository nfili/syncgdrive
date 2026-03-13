use std::path::Path;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use anyhow::Result;

pub struct IgnoreMatcher {
    set: GlobSet,
}

impl IgnoreMatcher {
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

        // Le dossier lui-même doit être ignoré
        assert!(m.is_ignored(&dir), "directory itself should be ignored");

        // Un fichier fictif à l'intérieur doit aussi être ignoré
        assert!(m.is_ignored(&dir.join("some_file.rs")),
            "file inside ignored dir should be ignored");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn file_outside_ignored_dir_not_affected() {
        let m = IgnoreMatcher::from_patterns(&[
            "**/target/**".to_string(),
        ]).unwrap();

        // Un fichier qui n'est pas dans target/ ne doit pas être ignoré
        let p = Path::new("/home/user/project/src/main.rs");
        assert!(!m.is_ignored(p));
    }
}

