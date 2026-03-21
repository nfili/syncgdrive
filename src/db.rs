//! Base de données SQLite WAL pour la persistance de l'état de synchronisation.
//!
//! Intègre :
//! - `schema_version` pour les migrations automatiques.
//! - `path_cache` pour réduire les requêtes HTTP (Phase 3).
//! - `offline_queue` pour la gestion hors-ligne (Phase 6).

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use tracing::{info, warn};

// ── Structures de données ─────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: String,
    pub hash: String,
    pub mtime: i64,
}

#[derive(Debug, Clone)]
pub struct PathCacheEntry {
    pub relative_path: String,
    pub drive_id: String,
    pub parent_id: String,
    pub is_folder: bool,
    pub updated_at: i64,
}

// Alignée sur ton spec 06_RESILIENCE.md
#[derive(Debug, Clone)]
pub struct OfflineTask {
    pub id: i64,
    pub action: String, // "sync", "delete", "rename"
    pub relative_path: String,
    pub extra: Option<String>, // Sert pour stocker l'ancien chemin lors d'un "rename"
    pub created_at: i64,
}

// ── Database ──────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct Database {
    inner: std::sync::Arc<std::sync::Mutex<Connection>>,
}

impl Database {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("cannot open SQLite db at {}", path.display()))?;

        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
        Ok(Self {
            inner: std::sync::Arc::new(std::sync::Mutex::new(conn)),
        })
    }

    // ── Migration & Initialisation (Phase 1) ──────────────────────────────────

    pub fn schema_version(&self) -> Result<i32> {
        let conn = self.lock()?;

        let has_schema_table: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='schema_version')",
            [],
            |r| r.get(0),
        )?;

        if !has_schema_table {
            let has_file_index: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='file_index')",
                [],
                |r| r.get(0),
            )?;
            return Ok(if has_file_index { 1 } else { 0 });
        }

        let version: i32 =
            conn.query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))?;
        Ok(version)
    }

    pub fn init_and_migrate(&self) -> Result<()> {
        let version = self.schema_version()?;

        if version == 0 {
            let mut conn = self
                .inner
                .lock()
                .map_err(|_| anyhow::anyhow!("SQLite mutex poisoned"))?;
            let tx = conn.transaction()?;

            tx.execute_batch(
                "CREATE TABLE file_index (
                    path  TEXT PRIMARY KEY,
                    hash  TEXT NOT NULL,
                    mtime INTEGER NOT NULL
                );
                CREATE TABLE dir_index (
                    path     TEXT PRIMARY KEY,
                    drive_id TEXT
                );
                CREATE TABLE schema_version (version INTEGER NOT NULL);
                INSERT INTO schema_version (version) VALUES (2);

                CREATE TABLE path_cache (
                    relative_path TEXT PRIMARY KEY,
                    drive_id      TEXT NOT NULL,
                    parent_id     TEXT NOT NULL,
                    is_folder     INTEGER NOT NULL DEFAULT 0,
                    updated_at    INTEGER NOT NULL
                );

                CREATE TABLE offline_queue (
                    id            INTEGER PRIMARY KEY AUTOINCREMENT,
                    action        TEXT NOT NULL,
                    relative_path TEXT NOT NULL,
                    extra         TEXT,
                    created_at    INTEGER NOT NULL
                );",
            )?;
            tx.commit()?;
            info!("Nouvelle base de données initialisée (Schéma V2)");
        } else if version == 1 {
            let mut conn = self
                .inner
                .lock()
                .map_err(|_| anyhow::anyhow!("SQLite mutex poisoned"))?;
            let tx = conn.transaction()?;

            tx.execute_batch(
                "CREATE TABLE schema_version (version INTEGER NOT NULL);
                INSERT INTO schema_version (version) VALUES (2);

                ALTER TABLE dir_index ADD COLUMN drive_id TEXT;

                CREATE TABLE path_cache (
                    relative_path TEXT PRIMARY KEY,
                    drive_id      TEXT NOT NULL,
                    parent_id     TEXT NOT NULL,
                    is_folder     INTEGER NOT NULL DEFAULT 0,
                    updated_at    INTEGER NOT NULL
                );

                CREATE TABLE offline_queue (
                    id            INTEGER PRIMARY KEY AUTOINCREMENT,
                    action        TEXT NOT NULL,
                    relative_path TEXT NOT NULL,
                    extra         TEXT,
                    created_at    INTEGER NOT NULL
                );",
            )?;
            tx.commit()?;
            info!("Base de données migrée de V1 vers V2 avec succès");
        } else if version > 2 {
            warn!(
                "La version du schéma ({}) est supérieure à celle supportée par ce binaire (2).",
                version
            );
        }

        Ok(())
    }

    // ── file_index (Existant V1 optimisé) ─────────────────────────────────────

    pub fn get(&self, path: &str) -> Result<Option<FileEntry>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare("SELECT path, hash, mtime FROM file_index WHERE path = ?1")?;
        let entry = stmt
            .query_row(params![path], |row| {
                Ok(FileEntry {
                    path: row.get(0)?,
                    hash: row.get(1)?,
                    mtime: row.get(2)?,
                })
            })
            .optional()?;
        Ok(entry)
    }

    pub fn upsert(&self, entry: &FileEntry) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO file_index (path, hash, mtime) VALUES (?1, ?2, ?3)
             ON CONFLICT(path) DO UPDATE SET hash=excluded.hash, mtime=excluded.mtime",
            params![entry.path, entry.hash, entry.mtime],
        )?;
        Ok(())
    }

    pub fn count(&self) -> Result<usize> {
        let conn = self.lock()?;
        let total: usize = conn.query_row("SELECT COUNT(*) FROM file_index", [], |r| r.get(0))?;
        Ok(total)
    }

    // ── path_cache (Nouveauté V2) ─────────────────────────────────────────────

    pub fn upsert_path_cache(&self, entry: &PathCacheEntry) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO path_cache (relative_path, drive_id, parent_id, is_folder, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(relative_path) DO UPDATE SET
                drive_id=excluded.drive_id,
                parent_id=excluded.parent_id,
                is_folder=excluded.is_folder,
                updated_at=excluded.updated_at",
            params![
                entry.relative_path,
                entry.drive_id,
                entry.parent_id,
                entry.is_folder as i32,
                entry.updated_at
            ],
        )?;
        Ok(())
    }

    pub fn get_path_cache(&self, path: &str) -> Result<Option<PathCacheEntry>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare("SELECT relative_path, drive_id, parent_id, is_folder, updated_at FROM path_cache WHERE relative_path = ?1")?;
        let entry = stmt
            .query_row(params![path], |row| {
                Ok(PathCacheEntry {
                    relative_path: row.get(0)?,
                    drive_id: row.get(1)?,
                    parent_id: row.get(2)?,
                    is_folder: row.get::<_, i32>(3)? != 0,
                    updated_at: row.get(4)?,
                })
            })
            .optional()?;
        Ok(entry)
    }

    pub fn delete_path_cache(&self, path: &str) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "DELETE FROM path_cache WHERE relative_path = ?1",
            params![path],
        )?;
        Ok(())
    }

    // ── offline_queue (Nouveauté V2 - Phase 6) ────────────────────────────────

    pub fn push_offline_task(&self, action: &str, path: &str, extra: Option<&str>) -> Result<i64> {
        let conn = self.lock()?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs() as i64;
        conn.execute(
            "INSERT INTO offline_queue (action, relative_path, extra, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![action, path, extra, now],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Récupère toutes les tâches hors-ligne par ordre d'arrivée (FIFO)
    pub fn get_offline_tasks(&self) -> Result<Vec<OfflineTask>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare("SELECT id, action, relative_path, extra, created_at FROM offline_queue ORDER BY id ASC")?;

        let task_iter = stmt.query_map([], |row| {
            Ok(OfflineTask {
                id: row.get(0)?,
                action: row.get(1)?,
                relative_path: row.get(2)?,
                extra: row.get(3)?,
                created_at: row.get(4)?,
            })
        })?;

        let mut tasks = Vec::new();
        for task in task_iter {
            tasks.push(task?);
        }
        Ok(tasks)
    }

    pub fn remove_offline_task(&self, id: i64) -> Result<()> {
        let conn = self.lock()?;
        conn.execute("DELETE FROM offline_queue WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn clear_offline_queue(&self) -> Result<()> {
        let conn = self.lock()?;
        conn.execute("DELETE FROM offline_queue", [])?;
        Ok(())
    }

    // ── Méthodes de compatibilité Moteur V1 (Phase 1) ────────────────────────

    pub fn init_schema(&self) -> Result<()> {
        self.init_and_migrate()
    }

    pub fn delete(&self, path: &str) -> Result<()> {
        let conn = self.lock()?;
        conn.execute("DELETE FROM file_index WHERE path = ?1", params![path])?;
        Ok(())
    }

    pub fn rename(&self, from: &str, to: &str) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE file_index SET path = ?1 WHERE path = ?2",
            params![to, from],
        )?;
        Ok(())
    }

    pub fn clear(&self) -> Result<()> {
        let conn = self.lock()?;
        conn.execute("DELETE FROM file_index", [])?;
        Ok(())
    }

    pub fn all_paths(&self) -> Result<std::collections::HashSet<String>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare("SELECT path FROM file_index")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut set = std::collections::HashSet::new();
        for path in rows {
            set.insert(path?);
        }
        Ok(set)
    }

    pub fn insert_dirs_batch(&self, paths: &[String]) -> Result<()> {
        let mut conn = self.inner.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare("INSERT OR IGNORE INTO dir_index (path) VALUES (?1)")?;
            for p in paths {
                stmt.execute(params![p])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn all_dir_paths(&self) -> Result<std::collections::HashSet<String>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare("SELECT path FROM dir_index")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut set = std::collections::HashSet::new();
        for path in rows {
            set.insert(path?);
        }
        Ok(set)
    }

    pub fn clear_dirs(&self) -> Result<()> {
        let conn = self.lock()?;
        conn.execute("DELETE FROM dir_index", [])?;
        Ok(())
    }

    // ── Interne ───────────────────────────────────────────────────────────────
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.inner
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite mutex poisoned"))
    }
    /// Compte le nombre de fichiers indexés (utile pour détecter le premier lancement)
    pub fn count_files(&self) -> Result<usize> {
        let conn = self.inner.lock().unwrap();
        let count: usize = conn.query_row(
            "SELECT COUNT(*) FROM file_index",
            [],
            |row| row.get(0)
        )?;
        Ok(count)
    }
}

// ── Tests Unitaires (Critères Phase 1) ────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    fn fresh_db() -> Database {
        let f = NamedTempFile::new().unwrap();
        let db = Database::open(f.path()).unwrap();
        db.init_and_migrate().unwrap();
        db
    }

    #[test]
    fn test_schema_version_initial() {
        let db = fresh_db();
        assert_eq!(db.schema_version().unwrap(), 2);
    }

    #[test]
    fn test_migration_v1_to_v2() {
        let f = NamedTempFile::new().unwrap();
        let conn = Connection::open(f.path()).unwrap();

        // Simulation V1 pure
        conn.execute_batch("
            CREATE TABLE file_index (path TEXT PRIMARY KEY, hash TEXT NOT NULL, mtime INTEGER NOT NULL);
            CREATE TABLE dir_index (path TEXT PRIMARY KEY);
        ").unwrap();

        let db = Database {
            inner: std::sync::Arc::new(std::sync::Mutex::new(conn)),
        };
        assert_eq!(db.schema_version().unwrap(), 1); // Détecté comme V1

        db.init_and_migrate().unwrap();
        assert_eq!(db.schema_version().unwrap(), 2); // Migré vers V2
    }

    #[test]
    fn test_migration_idempotent() {
        let db = fresh_db();
        db.init_and_migrate().unwrap(); // Double appel ne doit pas paniquer
        assert_eq!(db.schema_version().unwrap(), 2);
    }

    #[test]
    fn test_path_cache_crud() {
        let db = fresh_db();
        let entry = PathCacheEntry {
            relative_path: "dossier/fichier.txt".into(),
            drive_id: "12345XYZ".into(),
            parent_id: "ABCDE".into(),
            is_folder: false,
            updated_at: 1000,
        };

        db.upsert_path_cache(&entry).unwrap();
        let got = db.get_path_cache("dossier/fichier.txt").unwrap().unwrap();
        assert_eq!(got.drive_id, "12345XYZ");

        db.delete_path_cache("dossier/fichier.txt").unwrap();
        assert!(db.get_path_cache("dossier/fichier.txt").unwrap().is_none());
    }

    #[test]
    fn test_offline_queue_fifo() {
        let db = fresh_db();
        db.push_offline_task("sync", "file1.txt", None).unwrap();
        db.push_offline_task("rename", "file2.txt", Some("file1.txt"))
            .unwrap();

        let tasks = db.get_offline_tasks().unwrap();
        assert_eq!(tasks.len(), 2);

        assert_eq!(tasks[0].action, "sync");
        assert_eq!(tasks[0].relative_path, "file1.txt");

        assert_eq!(tasks[1].action, "rename");
        assert_eq!(tasks[1].relative_path, "file2.txt");
        assert_eq!(tasks[1].extra.as_deref().unwrap(), "file1.txt");

        db.remove_offline_task(tasks[0].id).unwrap();
        let tasks_after = db.get_offline_tasks().unwrap();
        assert_eq!(tasks_after.len(), 1);
        assert_eq!(tasks_after[0].action, "rename");
    }
}
