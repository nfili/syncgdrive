use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

// ── Entrée de l'index de fichiers ─────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: String,   // chemin relatif à local_root
    pub hash: String,   // SHA-256 hex
    pub mtime: i64,     // secondes UNIX
}

// ── Database ──────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct Database {
    // Connection SQLite enveloppée dans Arc<Mutex> pour le partage inter-tâches.
    inner: std::sync::Arc<std::sync::Mutex<Connection>>,
}

impl Database {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("cannot open SQLite db at {}", path.display()))?;

        // WAL : lecteurs ne bloquent pas les écrivains.
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;

        Ok(Self { inner: std::sync::Arc::new(std::sync::Mutex::new(conn)) })
    }

    pub fn init_schema(&self) -> Result<()> {
        let conn = self.lock()?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS file_index (
                path  TEXT PRIMARY KEY,
                hash  TEXT NOT NULL,
                mtime INTEGER NOT NULL
            );",
        )?;
        Ok(())
    }

    // ── Opérations sur l'index ────────────────────────────────────────────────

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

    // ── Interne ───────────────────────────────────────────────────────────────

    fn lock(&self) -> Result<std::sync::MutexGuard<Connection>> {
        self.inner.lock().map_err(|_| anyhow::anyhow!("SQLite mutex poisoned"))
    }
}

