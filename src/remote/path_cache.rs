use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use super::RemoteIndex;

/// Représente une entrée dans le cache
#[derive(Debug, Clone, PartialEq)]
pub struct CacheEntry {
    pub drive_id: String,
    pub parent_id: String,
}

/// Le PathCache mappe les chemins relatifs Arch Linux vers les IDs Google Drive
#[derive(Clone, Default)]
pub struct PathCache {
    // Clé: chemin relatif (ex: "Projets/SyncGDrive/src") -> Valeur: CacheEntry
    entries: Arc<RwLock<HashMap<String, CacheEntry>>>,
}

impl PathCache {
    pub fn new() -> Self {
        Self {
            entries: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// `test_lookup_existing` & `test_lookup_missing`
    pub async fn lookup(&self, relative_path: &str) -> Option<CacheEntry> {
        let map = self.entries.read().await;
        map.get(relative_path).cloned()
    }

    /// Ajoute ou met à jour une entrée
    pub async fn insert(&self, relative_path: &str, drive_id: &str, parent_id: &str) {
        let mut map = self.entries.write().await;
        map.insert(
            relative_path.to_string(),
            CacheEntry {
                drive_id: drive_id.to_string(),
                parent_id: parent_id.to_string(),
            },
        );
    }

    /// `test_rebuild_from_index` : Reconstruit le cache à partir d'un RemoteIndex complet
    pub async fn rebuild_from_index(&self, index: &RemoteIndex) {
        let mut map = self.entries.write().await;
        map.clear();

        // On insère d'abord les dossiers
        for dir in &index.dirs {
            map.insert(
                dir.relative_path.clone(),
                CacheEntry {
                    drive_id: dir.drive_id.clone(),
                    parent_id: dir.parent_id.clone(),
                },
            );
        }

        // Puis les fichiers
        for file in &index.files {
            map.insert(
                file.relative_path.clone(),
                CacheEntry {
                    drive_id: file.drive_id.clone(),
                    parent_id: file.parent_id.clone(),
                },
            );
        }
    }

    /// `test_remove_cascades` : Supprime un dossier ET tous ses enfants du cache
    pub async fn remove_cascades(&self, relative_path: &str) {
        let mut map = self.entries.write().await;

        // Le dossier exact
        map.remove(relative_path);

        // Tous les enfants (commencent par "chemin/")
        let prefix = format!("{}/", relative_path);
        map.retain(|path, _| !path.starts_with(&prefix));
    }

    /// `test_resolve_nested_path` : Utile pour le Mkdir.
    /// Pour "a/b/c", retourne le chemin le plus profond connu et son ID.
    pub async fn resolve_deepest_known_parent(&self, relative_path: &str) -> Option<(String, CacheEntry)> {
        let map = self.entries.read().await;
        let path = std::path::Path::new(relative_path);

        let mut current_path = path.parent();
        while let Some(p) = current_path {
            let p_str = p.to_string_lossy().to_string();
            if let Some(entry) = map.get(&p_str) {
                return Some((p_str, entry.clone()));
            }
            current_path = p.parent();
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::{RemoteIndex, RemoteDir, RemoteFile};

    #[tokio::test]
    async fn test_lookup_existing_and_missing() {
        let cache = PathCache::new();
        cache.insert("projets/main.rs", "id_123", "id_parent").await;

        // test_lookup_existing
        let found = cache.lookup("projets/main.rs").await;
        assert!(found.is_some(), "L'entrée devrait exister");
        assert_eq!(found.unwrap().drive_id, "id_123");

        // test_lookup_missing
        let missing = cache.lookup("inconnu.txt").await;
        assert!(missing.is_none(), "Une entrée inexistante doit retourner None");
    }

    #[tokio::test]
    async fn test_rebuild_from_index() {
        let cache = PathCache::new();

        let index = RemoteIndex {
            dirs: vec![
                RemoteDir {
                    relative_path: "dossierA".into(),
                    drive_id: "dir_A_id".into(),
                    parent_id: "root".into(),
                }
            ],
            files: vec![
                RemoteFile {
                    relative_path: "dossierA/fichier.txt".into(),
                    drive_id: "file_1_id".into(),
                    parent_id: "dir_A_id".into(),
                    md5: "hash_md5".into(),
                    size: 1024,
                    modified_time: 1600000000,
                }
            ]
        };

        cache.rebuild_from_index(&index).await;

        // Vérification de la cohérence du cache
        let dir_lookup = cache.lookup("dossierA").await.expect("Le dossier doit être dans le cache");
        assert_eq!(dir_lookup.drive_id, "dir_A_id");

        let file_lookup = cache.lookup("dossierA/fichier.txt").await.expect("Le fichier doit être dans le cache");
        assert_eq!(file_lookup.drive_id, "file_1_id");
    }

    #[tokio::test]
    async fn test_remove_cascades() {
        let cache = PathCache::new();
        cache.insert("a", "id_a", "root").await;
        cache.insert("a/b", "id_b", "id_a").await;
        cache.insert("a/b/c.txt", "id_c", "id_b").await;
        cache.insert("d", "id_d", "root").await;

        // Action : on supprime "a/b"
        cache.remove_cascades("a/b").await;

        // Vérifications
        assert!(cache.lookup("a").await.is_some(), "Le parent 'a' doit rester intact");
        assert!(cache.lookup("d").await.is_some(), "Le dossier 'd' ne doit pas être impacté");

        assert!(cache.lookup("a/b").await.is_none(), "Le dossier ciblé 'a/b' doit être supprimé");
        assert!(cache.lookup("a/b/c.txt").await.is_none(), "L'enfant 'a/b/c.txt' doit être supprimé en cascade");
    }

    #[tokio::test]
    async fn test_resolve_nested_path() {
        let cache = PathCache::new();
        cache.insert("a", "id_a", "root").await;
        cache.insert("a/b", "id_b", "id_a").await;

        // Action : on cherche le parent le plus profond connu pour un chemin non mis en cache
        let resolved = cache.resolve_deepest_known_parent("a/b/c/file.rs").await;

        // Vérifications
        assert!(resolved.is_some(), "Doit trouver un parent existant");
        let (known_path, entry) = resolved.unwrap();

        assert_eq!(known_path, "a/b", "Le parent le plus profond est 'a/b'");
        assert_eq!(entry.drive_id, "id_b", "L'ID doit correspondre au parent trouvé");
    }
}