use anyhow::Result;
use async_trait::async_trait;
use std::path::Path;
pub mod gdrive;
pub mod path_cache;

/// Le contrat que doit remplir tout fournisseur de stockage distant (Google Drive, etc.)
#[async_trait]
pub trait RemoteProvider: Send + Sync {
    // ── Listing ──────────────────────────────────────────
    /// Liste récursive du contenu distant. Retourne les chemins relatifs.
    async fn list_remote(&self, root_id: &str) -> Result<RemoteIndex>;

    // ── Dossiers ─────────────────────────────────────────
    /// Crée un dossier s'il n'existe pas. Retourne l'ID.
    async fn mkdir(&self, parent_id: &str, name: &str) -> Result<String>;

    // ── Fichiers ─────────────────────────────────────────
    /// Upload un fichier (simple ou resumable selon la taille).
    async fn upload(
        &self,
        local_path: &Path,
        parent_id: &str,
        file_name: &str,
        existing_id: Option<&str>,  // None = create, Some = update (overwrite)
    ) -> Result<UploadResult>;

    /// Supprime ou met à la corbeille un fichier/dossier distant.
    async fn delete(&self, file_id: &str) -> Result<()>;

    /// Renomme ou déplace un fichier distant.
    async fn rename(
        &self,
        file_id: &str,
        new_name: Option<&str>,
        new_parent_id: Option<&str>,
    ) -> Result<()>;

    // ── Delta ────────────────────────────────────────────
    /// Récupère les changements depuis le dernier cursor (changes.list).
    async fn get_changes(&self, cursor: Option<&str>) -> Result<ChangesPage>;

    // ── Santé ────────────────────────────────────────────
    /// Vérifie que les tokens sont valides et que l'API répond.
    async fn check_health(&self) -> Result<HealthStatus>;

    /// Arrêt propre (annule les uploads en cours).
    async fn shutdown(&self);
}

// ── Types de données ──────────────────────────────────

/// Index distant : fichiers et dossiers avec leurs IDs.
#[derive(Debug, Clone)]
pub struct RemoteIndex {
    pub files: Vec<RemoteFile>,
    pub dirs: Vec<RemoteDir>,
}

#[derive(Debug, Clone)]
pub struct RemoteFile {
    pub relative_path: String,
    pub drive_id: String,
    pub parent_id: String,
    pub md5: String,
    pub size: u64,
    pub modified_time: i64,
}

#[derive(Debug, Clone)]
pub struct RemoteDir {
    pub relative_path: String,
    pub drive_id: String,
    pub parent_id: String,
}

#[derive(Debug, Clone)]
pub struct UploadResult {
    pub drive_id: String,
    pub md5_checksum: String, // retourné par Google, pour vérification intégrité
    pub size_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct ChangesPage {
    pub changes: Vec<Change>,
    pub new_cursor: String, // pour le prochain appel
    pub has_more: bool,
}

#[derive(Debug, Clone)]
pub enum Change {
    Modified { drive_id: String, name: String, parent_id: String },
    Deleted { drive_id: String },
}

#[derive(Debug, Clone)]
pub enum HealthStatus {
    Ok { email: String, quota_used: u64, quota_total: u64 },
    AuthExpired,
    Unreachable,
}