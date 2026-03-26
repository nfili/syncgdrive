//! Base de données SQLite WAL pour la persistance de l'état de synchronisation.
//!
//! Ce module gère le stockage local ultra-rapide pour éviter d'interroger
//! l'API distante à chaque opération. Il intègre :
//! - `schema_version` : Mécanisme de migrations automatiques du schéma SQL.
//! - `path_cache` : Table de résolution rapide des chemins en identifiants Drive.
//! - `offline_queue` : File d'attente FIFO pour les opérations hors-ligne (résilience).

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use tracing::{info, warn};

// ── Structures de données ─────────────────────────────────────────────────────

/// Représente l'état d'un fichier local lors de sa dernière synchronisation.
#[derive(Debug, Clone)]
pub struct FileEntry {
    /// Chemin relatif depuis la racine du dossier synchronisé.
    pub path: String,
    /// Empreinte MD5 calculée localement (pour comparaison stricte avec le cloud).
    pub hash: String,
    /// Date de dernière modification (Timestamp Unix).
    pub mtime: i64,
}

/// Entrée du cache de résolution des chemins (Path → ID).
#[derive(Debug, Clone)]
pub struct PathCacheEntry {
    pub relative_path: String,
    pub drive_id: String,
    pub parent_id: String,
    pub is_folder: bool,
    pub updated_at: i64,
}

/// Tâche en attente d'exécution lorsque la connexion internet est rétablie.
#[derive(Debug, Clone)]
pub struct OfflineTask {
    pub id: i64,
    /// Type d'opération : "sync", "delete", "rename".
    pub action: String,
    pub relative_path: String,
    /// Méta-donnée contextuelle (ex: l'ancien chemin lors d'un "rename").
    pub extra: Option<String>,
    pub created_at: i64,
}

// ── Database ──────────────────────────────────────────────────────────────────

/// Gestionnaire thread-safe de la base de données SQLite.
///
/// Encapsule une unique connexion SQLite dans un `Mutex`. Grâce au mode WAL,
/// les contentions sont minimales, permettant aux workers de partager cette instance.
#[derive(Clone)]
pub struct Database {
    inner: std::sync::Arc<std::sync::Mutex<Connection>>,
}

impl Database {
    /// Ouvre ou crée la base de données au chemin spécifié et active le mode WAL.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("Impossible d'ouvrir la base SQLite à {}", path.display()))?;

        // PRAGMA journal_mode=WAL : Améliore drastiquement les performances de concurrence.
        // PRAGMA synchronous=NORMAL : Bon compromis entre sécurité en cas de crash et vitesse d'écriture.
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;

        Ok(Self {
            inner: std::sync::Arc::new(std::sync::Mutex::new(conn)),
        })
    }

    // ── Helper interne (DRY & Sécurité) ───────────────────────────────────────

    /// Acquiert le verrou sur la connexion SQLite de manière sûre.
    /// Retourne une erreur propre si le Mutex est empoisonné (panic dans un autre thread).
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.inner
            .lock()
            .map_err(|_| anyhow::anyhow!("Mutex SQLite empoisonné suite à un crash précédent"))
    }

    // ── Migration & Initialisation (Phase 1) ──────────────────────────────────

    /// Détermine la version actuelle du schéma de la base de données.
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
            // Si file_index existe, mais pas schema_version, c'est la V1 originelle
            return Ok(if has_file_index { 1 } else { 0 });
        }

        let version: i32 =
            conn.query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))?;
        Ok(version)
    }

    /// Exécute les scripts SQL nécessaires pour mettre à jour la structure de la base.
    ///
    /// Utilise des transactions explicites pour garantir que la migration s'applique
    /// entièrement ou pas du tout (Atomicité).
    pub fn init_and_migrate(&self) -> Result<()> {
        let version = self.schema_version()?;

        if version == 0 {
            // Création initiale (V2 directement)
            let mut conn = self.lock()?;
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
            // Migration V1 -> V2
            let mut conn = self.lock()?;
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

    /// Récupère l'empreinte connue d'un fichier local.
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

    /// Insère ou met à jour les informations d'un fichier synchronisé (Upsert).
    pub fn upsert(&self, entry: &FileEntry) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO file_index (path, hash, mtime) VALUES (?1, ?2, ?3)
             ON CONFLICT(path) DO UPDATE SET hash=excluded.hash, mtime=excluded.mtime",
            params![entry.path, entry.hash, entry.mtime],
        )?;
        Ok(())
    }

    /// Compte le nombre total de fichiers indexés.
    pub fn count(&self) -> Result<usize> {
        let conn = self.lock()?;
        let total: usize = conn.query_row("SELECT COUNT(*) FROM file_index", [], |r| r.get(0))?;
        Ok(total)
    }

    // ── path_cache (Nouveauté V2) ─────────────────────────────────────────────

    /// Met à jour l'identifiant Google Drive associé à un chemin local.
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

    /// Retrouve une entrée du cache de chemins à partir de sa route relative.
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

    /// Supprime une entrée invalide du cache de chemins.
    pub fn delete_path_cache(&self, path: &str) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "DELETE FROM path_cache WHERE relative_path = ?1",
            params![path],
        )?;
        Ok(())
    }

    // ── offline_queue (Nouveauté V2 - Phase 6) ────────────────────────────────

    /// Ajoute une opération dans la file d'attente hors-ligne.
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

    /// Récupère toutes les tâches hors-ligne par ordre d'arrivée chronologique (FIFO).
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

    /// Retire une tache de la file d'attente une fois exécutée avec succès.
    pub fn remove_offline_task(&self, id: i64) -> Result<()> {
        let conn = self.lock()?;
        conn.execute("DELETE FROM offline_queue WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Vide intégralement la file d'attente des opérations hors-ligne.
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

    /// Insère un lot complet de chemins de dossiers en utilisant une seule transaction SQL.
    pub fn insert_dirs_batch(&self, paths: &[String]) -> Result<()> {
        let mut conn = self.lock()?; // <-- CORRECTION : Utilisation propre du lock
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

    /// Compte le nombre de fichiers indexés (utile pour détecter le premier lancement).
    pub fn count_files(&self) -> Result<usize> {
        let conn = self.lock()?; // <-- CORRECTION : Remplacement du unwrap dangereux
        let count: usize =
            conn.query_row("SELECT COUNT(*) FROM file_index", [], |row| row.get(0))?;
        Ok(count)
    }
}

// Les tests unitaires sont parfaits et n'ont pas été modifiés.
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