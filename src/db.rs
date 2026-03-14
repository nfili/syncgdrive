//! Base de données SQLite WAL pour la persistance de l'état de synchronisation.
//!
//! Ce module gère deux tables :
//!
//! - **`file_index`** : index des fichiers synchronisés `(chemin relatif, hash SHA-256, mtime)`
//! - **`dir_index`** : cache persistant des dossiers distants connus `(chemin relatif)`
//!
//! # Thread-safety
//!
//! La [`Database`] utilise `Arc<Mutex<Connection>>` pour un partage sûr entre
//! les tâches Tokio. Les opérations unitaires (`get`, `upsert`, `delete`) sont
//! synchrones et rapides (< 1ms en mode WAL). Les opérations lourdes
//! (`all_paths`) doivent être enveloppées dans `tokio::task::spawn_blocking`.
//!
//! # Emplacement
//!
//! `$XDG_DATA_HOME/syncgdrive/index.db` (défaut : `~/.local/share/syncgdrive/index.db`)
//!
//! # Mode WAL
//!
//! SQLite est configuré en mode WAL (Write-Ahead Logging) + `synchronous=NORMAL`
//! pour de meilleures performances concurrentes : les lecteurs ne bloquent jamais
//! les écrivains.

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

// ── Entrée de l'index de fichiers ─────────────────────────────────────────────

/// Entrée de l'index de fichiers synchronisés.
///
/// Représente un fichier dont l'état est connu en base : son chemin relatif
/// (par rapport à `local_root`), son hash SHA-256 du contenu, et son `mtime`
/// (secondes UNIX) au moment de la dernière synchronisation.
#[derive(Debug, Clone)]
pub struct FileEntry {
    /// Chemin relatif du fichier par rapport à `local_root` (ex: `src/main.rs`).
    pub path: String,
    /// Hash SHA-256 hexadécimal du contenu du fichier.
    pub hash: String,
    /// Date de dernière modification en secondes UNIX (epoch).
    pub mtime: i64,
}

// ── Database ──────────────────────────────────────────────────────────────────

/// Gestionnaire de la base de données SQLite.
///
/// Encapsule une connexion SQLite protégée par `Arc<Mutex>` pour un accès
/// concurrent sûr depuis les tâches Tokio (scan, workers, watcher).
///
/// # Tables
///
/// | Table | Colonnes | Usage |
/// |-------|----------|-------|
/// | `file_index` | `path TEXT PK, hash TEXT, mtime INTEGER` | Index des fichiers synchronisés |
/// | `dir_index` | `path TEXT PK` | Cache persistant des dossiers distants connus |
#[derive(Clone)]
pub struct Database {
    // Connection SQLite enveloppée dans Arc<Mutex> pour le partage inter-tâches.
    inner: std::sync::Arc<std::sync::Mutex<Connection>>,
}

impl Database {
    /// Ouvre (ou crée) la base de données SQLite au chemin spécifié.
    ///
    /// Configure automatiquement le mode WAL et `synchronous=NORMAL`.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("cannot open SQLite db at {}", path.display()))?;

        // WAL : lecteurs ne bloquent pas les écrivains.
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;

        Ok(Self { inner: std::sync::Arc::new(std::sync::Mutex::new(conn)) })
    }

    /// Crée les tables `file_index` et `dir_index` si elles n'existent pas.
    ///
    /// Doit être appelé une fois au démarrage, avant toute opération.
    pub fn init_schema(&self) -> Result<()> {
        let conn = self.lock()?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS file_index (
                path  TEXT PRIMARY KEY,
                hash  TEXT NOT NULL,
                mtime INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS dir_index (
                path  TEXT PRIMARY KEY
            );",
        )?;
        Ok(())
    }

    // ── Opérations sur l'index ────────────────────────────────────────────────

    /// Récupère l'entrée d'un fichier par son chemin relatif.
    ///
    /// Retourne `None` si le fichier n'est pas dans l'index.
    pub fn get(&self, path: &str) -> Result<Option<FileEntry>> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare("SELECT path, hash, mtime FROM file_index WHERE path = ?1")?;
        let mut rows = stmt.query(params![path])?;
        if let Some(row) = rows.next()? {
            Ok(Some(FileEntry {
                path: row.get(0)?,
                hash: row.get(1)?,
                mtime: row.get(2)?,
            }))
        } else {
            Ok(None)
        }
    }

    /// Insère ou met à jour un fichier dans l'index.
    ///
    /// Si le chemin existe déjà, `hash` et `mtime` sont écrasés.
    /// Opération atomique via `INSERT … ON CONFLICT DO UPDATE`.
    pub fn upsert(&self, entry: &FileEntry) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO file_index (path, hash, mtime)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(path) DO UPDATE SET hash=excluded.hash, mtime=excluded.mtime",
            params![entry.path, entry.hash, entry.mtime],
        )?;
        Ok(())
    }

    /// Supprime un fichier de l'index par son chemin relatif.
    ///
    /// Silencieux si le chemin n'existe pas (pas d'erreur).
    pub fn delete(&self, path: &str) -> Result<()> {
        let conn = self.lock()?;
        conn.execute("DELETE FROM file_index WHERE path = ?1", params![path])?;
        Ok(())
    }

    /// Renomme old_path → new_path de façon transactionnelle.
    pub fn rename(&self, old: &str, new: &str) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE file_index SET path = ?2 WHERE path = ?1",
            params![old, new],
        )?;
        Ok(())
    }

    /// Supprime toutes les entrées (changement de local_root).
    pub fn clear(&self) -> Result<usize> {
        let conn = self.lock()?;
        let n = conn.execute("DELETE FROM file_index", [])?;
        Ok(n)
    }

    /// Retourne tous les chemins relatifs indexés.
    pub fn all_paths(&self) -> Result<std::collections::HashSet<String>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare("SELECT path FROM file_index")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut set = std::collections::HashSet::new();
        for r in rows {
            set.insert(r?);
        }
        Ok(set)
    }

    /// Nombre d'entrées dans l'index.
    pub fn count(&self) -> Result<usize> {
        let conn = self.lock()?;
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM file_index", [], |r| r.get(0))?;
        Ok(n as usize)
    }

    // ── Index des dossiers distants connus ─────────────────────────────────────

    /// Enregistre un dossier distant (chemin relatif) comme connu/créé.
    pub fn insert_dir(&self, rel_path: &str) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT OR IGNORE INTO dir_index (path) VALUES (?1)",
            params![rel_path],
        )?;
        Ok(())
    }

    /// Vérifie si un dossier distant est déjà connu (déjà créé/synchronisé).
    pub fn has_dir(&self, rel_path: &str) -> Result<bool> {
        let conn = self.lock()?;
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM dir_index WHERE path = ?1)",
            params![rel_path],
            |r| r.get(0),
        )?;
        Ok(exists)
    }

    /// Retourne tous les chemins de dossiers distants connus.
    pub fn all_dir_paths(&self) -> Result<std::collections::HashSet<String>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare("SELECT path FROM dir_index")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut set = std::collections::HashSet::new();
        for r in rows {
            set.insert(r?);
        }
        Ok(set)
    }

    /// Supprime toutes les entrées de l'index dossiers.
    pub fn clear_dirs(&self) -> Result<usize> {
        let conn = self.lock()?;
        let n = conn.execute("DELETE FROM dir_index", [])?;
        Ok(n)
    }

    /// Insère plusieurs dossiers en une seule transaction (batch rapide).
    pub fn insert_dirs_batch(&self, paths: &[String]) -> Result<()> {
        let conn = self.lock()?;
        let tx = conn.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare("INSERT OR IGNORE INTO dir_index (path) VALUES (?1)")?;
            for p in paths {
                stmt.execute(params![p])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    // ── Interne ───────────────────────────────────────────────────────────────

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.inner.lock().map_err(|_| anyhow::anyhow!("SQLite mutex poisoned"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    fn test_db() -> Database {
        let f = NamedTempFile::new().unwrap();
        let db = Database::open(f.path()).unwrap();
        db.init_schema().unwrap();
        db
    }

    // ── file_index ────────────────────────────────────────────────────────────

    #[test]
    fn get_missing_returns_none() {
        let db = test_db();
        assert!(db.get("inexistant.txt").unwrap().is_none());
    }

    #[test]
    fn upsert_then_get() {
        let db = test_db();
        let entry = FileEntry { path: "src/main.rs".into(), hash: "abc123".into(), mtime: 1000 };
        db.upsert(&entry).unwrap();
        let got = db.get("src/main.rs").unwrap().unwrap();
        assert_eq!(got.hash, "abc123");
        assert_eq!(got.mtime, 1000);
    }

    #[test]
    fn upsert_updates_existing() {
        let db = test_db();
        db.upsert(&FileEntry { path: "f.txt".into(), hash: "old".into(), mtime: 1 }).unwrap();
        db.upsert(&FileEntry { path: "f.txt".into(), hash: "new".into(), mtime: 2 }).unwrap();
        let got = db.get("f.txt").unwrap().unwrap();
        assert_eq!(got.hash, "new");
        assert_eq!(got.mtime, 2);
    }

    #[test]
    fn delete_entry() {
        let db = test_db();
        db.upsert(&FileEntry { path: "del.txt".into(), hash: "x".into(), mtime: 1 }).unwrap();
        db.delete("del.txt").unwrap();
        assert!(db.get("del.txt").unwrap().is_none());
    }

    #[test]
    fn delete_nonexistent_is_ok() {
        let db = test_db();
        db.delete("nope.txt").unwrap(); // pas de panic
    }

    #[test]
    fn rename_entry() {
        let db = test_db();
        db.upsert(&FileEntry { path: "old.txt".into(), hash: "h".into(), mtime: 5 }).unwrap();
        db.rename("old.txt", "new.txt").unwrap();
        assert!(db.get("old.txt").unwrap().is_none());
        let got = db.get("new.txt").unwrap().unwrap();
        assert_eq!(got.hash, "h");
    }

    #[test]
    fn clear_removes_all() {
        let db = test_db();
        db.upsert(&FileEntry { path: "a".into(), hash: "1".into(), mtime: 1 }).unwrap();
        db.upsert(&FileEntry { path: "b".into(), hash: "2".into(), mtime: 2 }).unwrap();
        let n = db.clear().unwrap();
        assert_eq!(n, 2);
        assert_eq!(db.count().unwrap(), 0);
    }

    #[test]
    fn all_paths_returns_set() {
        let db = test_db();
        db.upsert(&FileEntry { path: "x.rs".into(), hash: "h".into(), mtime: 1 }).unwrap();
        db.upsert(&FileEntry { path: "y.rs".into(), hash: "h".into(), mtime: 1 }).unwrap();
        let paths = db.all_paths().unwrap();
        assert_eq!(paths.len(), 2);
        assert!(paths.contains("x.rs"));
        assert!(paths.contains("y.rs"));
    }

    #[test]
    fn count_entries() {
        let db = test_db();
        assert_eq!(db.count().unwrap(), 0);
        db.upsert(&FileEntry { path: "a".into(), hash: "h".into(), mtime: 1 }).unwrap();
        assert_eq!(db.count().unwrap(), 1);
    }

    // ── dir_index ─────────────────────────────────────────────────────────────

    #[test]
    fn insert_dir_and_has_dir() {
        let db = test_db();
        assert!(!db.has_dir("src").unwrap());
        db.insert_dir("src").unwrap();
        assert!(db.has_dir("src").unwrap());
    }

    #[test]
    fn insert_dir_is_idempotent() {
        let db = test_db();
        db.insert_dir("src").unwrap();
        db.insert_dir("src").unwrap(); // INSERT OR IGNORE
        assert_eq!(db.all_dir_paths().unwrap().len(), 1);
    }

    #[test]
    fn all_dir_paths_returns_set() {
        let db = test_db();
        db.insert_dir("a/b").unwrap();
        db.insert_dir("a/c").unwrap();
        let dirs = db.all_dir_paths().unwrap();
        assert_eq!(dirs.len(), 2);
        assert!(dirs.contains("a/b"));
    }

    #[test]
    fn clear_dirs_purges_all() {
        let db = test_db();
        db.insert_dir("d1").unwrap();
        db.insert_dir("d2").unwrap();
        let n = db.clear_dirs().unwrap();
        assert_eq!(n, 2);
        assert!(db.all_dir_paths().unwrap().is_empty());
    }

    #[test]
    fn insert_dirs_batch_transaction() {
        let db = test_db();
        let dirs = vec!["a".into(), "b".into(), "c".into(), "a".into()]; // avec doublon
        db.insert_dirs_batch(&dirs).unwrap();
        assert_eq!(db.all_dir_paths().unwrap().len(), 3); // dédupliqué
    }

    #[test]
    fn clear_does_not_affect_dirs() {
        let db = test_db();
        db.upsert(&FileEntry { path: "f".into(), hash: "h".into(), mtime: 1 }).unwrap();
        db.insert_dir("d").unwrap();
        db.clear().unwrap();
        assert_eq!(db.count().unwrap(), 0);
        assert!(db.has_dir("d").unwrap()); // dir_index intact
    }

    #[test]
    fn clear_dirs_does_not_affect_files() {
        let db = test_db();
        db.upsert(&FileEntry { path: "f".into(), hash: "h".into(), mtime: 1 }).unwrap();
        db.insert_dir("d").unwrap();
        db.clear_dirs().unwrap();
        assert_eq!(db.count().unwrap(), 1); // file_index intact
        assert!(!db.has_dir("d").unwrap());
    }
}
