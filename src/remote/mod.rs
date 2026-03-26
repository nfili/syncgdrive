//! Couche d'abstraction pour les fournisseurs de stockage distant.
//!
//! Ce module définit le contrat `RemoteProvider` que tout service cloud
//! (Google Drive, Dropbox, AWS S3, etc.) doit respecter pour s'intégrer
//! au moteur de synchronisation. Il contient également les structures de
//! données standardisées utilisées pour normaliser les réponses des API.

use anyhow::Result;
use async_trait::async_trait;
use std::path::Path;
pub mod gdrive;
pub mod path_cache;

/// Le contrat que doit remplir tout fournisseur de stockage distant (Google Drive, etc.)
///
/// L'utilisation de `async_trait` permet de définir des méthodes asynchrones
/// dans le trait, indispensables pour les opérations réseau. Les contraintes
/// `Send + Sync` garantissent que le fournisseur peut être partagé entre les workers Tokio.
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
        existing_id: Option<&str>,
        tracker: std::sync::Arc<crate::engine::bandwidth::ProgressTracker>, // <-- NOUVEAU
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

/// Index distant : Représentation plate de l'arborescence du cloud.
#[derive(Debug, Clone)]
pub struct RemoteIndex {
    pub files: Vec<RemoteFile>,
    pub dirs: Vec<RemoteDir>,
}

/// Représentation standardisée d'un fichier distant.
#[derive(Debug, Clone)]
pub struct RemoteFile {
    pub relative_path: String,
    pub drive_id: String,
    pub parent_id: String,
    pub md5: String,
    pub size: u64,
    pub modified_time: i64,
}

/// Représentation standardisée d'un dossier distant.
#[derive(Debug, Clone)]
pub struct RemoteDir {
    pub relative_path: String,
    pub drive_id: String,
    pub parent_id: String,
}

/// Bilan renvoyé par le fournisseur après un upload réussi.
#[derive(Debug, Clone)]
pub struct UploadResult {
    pub drive_id: String,
    pub md5_checksum: String, // retourné par Google, pour vérification intégrité
    pub size_bytes: u64,
}

/// Page de résultats pour la synchronisation différentielle (Delta).
#[derive(Debug, Clone)]
pub struct ChangesPage {
    pub changes: Vec<Change>,
    pub new_cursor: String, // pour le prochain appel
    pub has_more: bool,
}

/// Type de modification survenue sur le cloud.
#[derive(Debug, Clone)]
pub enum Change {
    Modified {
        drive_id: String,
        name: String,
        parent_id: String,
    },
    Deleted {
        drive_id: String,
    },
}

/// État de santé de la connexion avec le fournisseur cloud.
#[derive(Debug, Clone)]
pub enum HealthStatus {
    Ok {
        email: String,
        quota_used: u64,
        quota_total: u64,
    },
    AuthExpired,
    Unreachable,
}