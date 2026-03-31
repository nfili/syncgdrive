//! Orchestrateur des migrations système (Configuration & Base de données).
//!
//! Ce module agit comme un chef d'orchestre au démarrage de l'application.
//! Il s'assure que le fichier de configuration (`config.toml`) et le schéma
//! de la base de données SQLite sont tous deux mis à jour vers la dernière
//! version (V2) avant que le moteur de synchronisation ne démarre.

use anyhow::{Context, Result};
use std::path::Path;
use tracing::info;

use crate::config::AppConfig;
use crate::db::Database;

/// Point d'entrée principal appelé au démarrage de l'application.
///
/// Valide et migre séquentiellement la configuration puis la base de données.
/// Garantit que l'environnement est sain et à jour avant de lancer les processus.
///
/// # Paramètres
/// * `db_path` - Le chemin absolu vers le fichier de la base SQLite.
pub fn run_all_migrations(db_path: &Path) -> Result<AppConfig> {
    info!("Vérification de l'état des migrations...");

    // 1. Migration de la Configuration (Déléguée au module expert `config.rs`)
    // `load_or_create()` gère de manière autonome la lecture, la migration V1→V2,
    // et la création sécurisée du fichier de backup `.toml.v1.bak`.
    let (config, _is_first_run) = AppConfig::load_or_create()
        .context("Échec de l'initialisation ou de la migration de la configuration")?;

    // 2. Migration de la Base de données (Déléguée au module expert `db.rs`)
    migrate_database(db_path)?;

    Ok(config)
}

/// Prépare le répertoire et migre le schéma SQLite vers la dernière version.
fn migrate_database(db_path: &Path) -> Result<()> {
    // Sécurité : On s'assure que le dossier parent (ex: ~/.local/share/syncgdrive) existe
    if let Some(parent) = db_path.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent)
                .context("Impossible de créer le dossier parent pour la base SQLite")?;
        }
    }

    let db = Database::open(db_path)?;
    db.init_and_migrate()
        .context("Échec lors de la migration du schéma SQLite")?;

    info!("Base de données prête et à jour.");
    Ok(())
}
