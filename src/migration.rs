//! Orchestrateur de migration V1 vers V2.
//!
//! Ce module centralise la logique de transition pour garantir qu'aucune donnée
//! utilisateur n'est perdue lors du passage à la nouvelle architecture.

use std::path::Path;
use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::config::AppConfig;
use crate::db::Database;

/// Point d'entrée principal pour exécuter toutes les migrations nécessaires au démarrage.
pub fn run_all_migrations(config_path: &Path, db_path: &Path) -> Result<AppConfig> {
    info!("Vérification de l'état des migrations...");

    // 1. Migration de la Configuration
    let config = migrate_config(config_path)?;

    // 2. Migration de la Base de données
    migrate_database(db_path)?;

    Ok(config)
}

/// Gère spécifiquement la migration du fichier config.toml
fn migrate_config(config_path: &Path) -> Result<AppConfig> {
    // Si le fichier n'existe pas, on laisse load_or_create gérer la création V2 pure
    if !config_path.exists() {
        let (cfg, _) = AppConfig::load_or_create()?;
        return Ok(cfg);
    }

    let raw_toml = std::fs::read_to_string(config_path)
        .with_context(|| format!("Impossible de lire {}", config_path.display()))?;

    // On utilise la fonction pure de config.rs que nous avons écrite
    let (cfg, migrated) = AppConfig::parse_and_migrate(&raw_toml)?;

    if migrated {
        warn!("Ancienne configuration V1 détectée. Début de la migration...");

        let backup_path = config_path.with_extension("toml.v1.bak");
        std::fs::write(&backup_path, &raw_toml)
            .with_context(|| format!("Échec de la création du backup de sécurité vers {:?}", backup_path))?;

        info!("Backup de sécurité créé : {:?}", backup_path);

        // Sauvegarde immédiate du nouveau format V2
        cfg.save()?;
        info!("Configuration V2 générée avec succès.");
    }

    Ok(cfg)
}

/// Gère spécifiquement la migration du schéma SQLite
fn migrate_database(db_path: &Path) -> Result<()> {
    // Si le dossier parent de la DB n'existe pas, on le crée
    if let Some(parent) = db_path.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent)?;
        }
    }

    let db = Database::open(db_path)?;
    db.init_and_migrate().context("Échec lors de la migration du schéma SQLite")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;
    use std::io::Write;

    #[test]
    fn test_config_backup_created() {
        let mut f = NamedTempFile::new().unwrap();
        // Simulation d'un vieux TOML V1
        writeln!(f, "local_root = '/tmp/old'\nmax_workers = 2").unwrap();

        let cfg = migrate_config(f.path()).expect("La migration de test a échoué");

        // Vérifie le mapping
        assert_eq!(cfg.sync_pairs.len(), 1);

        // Vérifie la création du fichier .bak
        let backup_path = f.path().with_extension("toml.v1.bak");
        assert!(backup_path.exists(), "Le fichier de backup .v1.bak n'a pas été créé");
    }

    #[test]
    fn test_config_v1_fields_mapped() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "local_root = '/home/user/Sync'\nremote_root = 'gdrive:/Drive'").unwrap();

        let cfg = migrate_config(f.path()).unwrap();

        assert_eq!(cfg.sync_pairs.len(), 1);
        assert_eq!(cfg.sync_pairs[0].name, "Sync principal");
        assert_eq!(cfg.sync_pairs[0].local_path.to_string_lossy(), "/home/user/Sync");
        assert_eq!(cfg.sync_pairs[0].provider, "GoogleDrive");
    }
}