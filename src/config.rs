//! Configuration de l'application SyncGDrive V2.
//!
//! Ce module gère le chargement, la validation et la sauvegarde de la
//! configuration TOML. Il intègre la migration automatique depuis la V1.

use std::path::{Path, PathBuf};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{info};

// ── Valeurs par défaut (strictement selon 01_CONFIG_V2.md) ──────────────────

fn default_ignore_patterns() -> Vec<String> {
    vec![
        "**/target/**".into(),
        "**/.git/**".into(),
        "**/node_modules/**".into(),
        "**/.sqlx/**".into(),
        "**/.idea/**".into(),
    ]
}
fn default_max_workers() -> usize { 4 }
fn default_retry_attempts() -> u32 { 3 }
fn default_initial_backoff_ms() -> u64 { 300 }
fn default_max_backoff_ms() -> u64 { 8_000 }
fn default_rescan_interval_min() -> u64 { 30 }
fn default_provider() -> String { "GoogleDrive".into() }
fn default_true() -> bool { true }
fn default_debounce_ms() -> u64 { 500 }
fn default_health_check_secs() -> u64 { 30 }
fn default_max_concurrent_ls() -> usize { 8 }
fn default_shutdown_timeout() -> u64 { 3 }
fn default_log_retention() -> u64 { 7 }
fn default_channel_capacity() -> usize { 32 }
fn default_notif_timeout() -> i32 { 6000 }
fn default_resumable_threshold() -> u64 { 5_242_880 }
fn default_api_rate_limit() -> u32 { 10 }
fn default_delete_mode() -> String { "trash".into() }
fn default_symlink_mode() -> String { "ignore".into() }

// ── Structures ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryConfig {
    #[serde(default = "default_retry_attempts")]
    pub max_attempts: u32,
    #[serde(default = "default_initial_backoff_ms")]
    pub initial_backoff_ms: u64,
    #[serde(default = "default_max_backoff_ms")]
    pub max_backoff_ms: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: default_retry_attempts(),
            initial_backoff_ms: default_initial_backoff_ms(),
            max_backoff_ms: default_max_backoff_ms(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdvancedConfig {
    #[serde(default = "default_debounce_ms")]
    pub debounce_ms: u64,
    #[serde(default = "default_health_check_secs")]
    pub health_check_interval_secs: u64,
    #[serde(default = "default_max_concurrent_ls")]
    pub max_concurrent_ls: usize,
    #[serde(default = "default_shutdown_timeout")]
    pub shutdown_timeout_secs: u64,
    #[serde(default = "default_log_retention")]
    pub log_retention_days: u64,
    #[serde(default = "default_channel_capacity")]
    pub engine_channel_capacity: usize,
    #[serde(default = "default_notif_timeout")]
    pub notification_timeout_ms: i32,
    #[serde(default = "default_resumable_threshold")]
    pub resumable_upload_threshold: u64,
    #[serde(default)]
    pub upload_limit_kbps: u64,
    #[serde(default = "default_api_rate_limit")]
    pub api_rate_limit_rps: u32,
    #[serde(default = "default_delete_mode")]
    pub delete_mode: String,
    #[serde(default = "default_symlink_mode")]
    pub symlink_mode: String,
}

impl Default for AdvancedConfig {
    fn default() -> Self {
        Self {
            debounce_ms: default_debounce_ms(),
            health_check_interval_secs: default_health_check_secs(),
            max_concurrent_ls: default_max_concurrent_ls(),
            shutdown_timeout_secs: default_shutdown_timeout(),
            log_retention_days: default_log_retention(),
            engine_channel_capacity: default_channel_capacity(),
            notification_timeout_ms: default_notif_timeout(),
            resumable_upload_threshold: default_resumable_threshold(),
            upload_limit_kbps: 0,
            api_rate_limit_rps: default_api_rate_limit(),
            delete_mode: default_delete_mode(),
            symlink_mode: default_symlink_mode(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncPair {
    pub name: String,
    pub local_path: PathBuf,
    #[serde(default)] // Défaut vide, rempli par le wizard OAuth2
    pub remote_folder_id: String,
    #[serde(default = "default_provider")]
    pub provider: String,
    #[serde(default = "default_true")]
    pub active: bool,
    #[serde(default)]
    pub ignore_patterns: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppConfig {
    #[serde(default)]
    pub sync_pairs: Vec<SyncPair>,
    #[serde(default = "default_max_workers")]
    pub max_workers: usize,
    #[serde(default)]
    pub notifications: bool,
    #[serde(default = "default_rescan_interval_min")]
    pub rescan_interval_min: u64,
    #[serde(default = "default_ignore_patterns")]
    pub ignore_patterns: Vec<String>,
    #[serde(default)]
    pub retry: RetryConfig,
    #[serde(default)]
    pub advanced: AdvancedConfig,
}

// ── Erreurs de validation ─────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("aucune paire de synchronisation n'est configurée — ouvrez les réglages")]
    NoPairsConfigured,
    #[error("la paire #{0} a un nom vide")]
    PairNameEmpty(usize),
    #[error("la paire #{0} a un chemin local vide")]
    PairLocalEmpty(usize),
    #[error("la paire #{0} a un chemin local '{1}' qui n'existe pas ou n'est pas un répertoire")]
    PairLocalMissing(usize, PathBuf),
}

// ── Chemin de la config ───────────────────────────────────────────────────────

pub fn config_path() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".config")
        });
    base.join("syncgdrive").join("config.toml")
}

// ── Logique Principale ────────────────────────────────────────────────────────

impl AppConfig {
    /// Charge la config depuis le disque et gère la migration V1 -> V2.
    /// Retourne `(config, is_first_run)`.
    pub fn load_or_create() -> Result<(Self, bool)> {
        let path = config_path();

        if !path.exists() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let default = AppConfig::default();
            default.save()?;
            return Ok((default, true));
        }

        let raw = std::fs::read_to_string(&path)?;
        let (mut cfg, migrated) = Self::parse_and_migrate(&raw)?;

        if migrated {
            // Création du backup V1
            let backup_path = path.with_extension("toml.v1.bak");
            std::fs::write(&backup_path, &raw)
                .with_context(|| format!("Échec création backup V1: {}", backup_path.display()))?;

            info!("Migration config V1 → V2 effectuée. Backup créé dans {:?}", backup_path);

            // Sauvegarde du nouveau format V2
            cfg.save()?;
        }

        cfg.expand_tildes();
        Ok((cfg, false))
    }

    /// Fonction pure pour tester la logique de parsing et migration sans IO.
    pub fn parse_and_migrate(raw_toml: &str) -> Result<(Self, bool)> {
        let toml_val: toml::Value = toml::from_str(raw_toml)
            .context("Fichier TOML invalide")?;

        // Vérification de la présence de paires V2
        let has_v2_pairs = toml_val.get("sync_pairs")
            .and_then(|v| v.as_array())
            .map(|arr| !arr.is_empty())
            .unwrap_or(false);

        if has_v2_pairs {
            // C'est une V2 valide
            let cfg: AppConfig = toml::from_str(raw_toml)?;
            return Ok((cfg, false));
        }

        // Si pas de paires V2, on tente de migrer les données V1
        let mut cfg: AppConfig = toml::from_str(raw_toml)?; // Charge les autres champs (workers, etc.)
        let mut migrated = false;

        if let Some(local_root) = toml_val.get("local_root").and_then(|v| v.as_str()) {
            if !local_root.trim().is_empty() {
                cfg.sync_pairs.push(SyncPair {
                    name: "Sync principal".into(),
                    local_path: PathBuf::from(local_root),
                    remote_folder_id: String::new(), // Sera rempli par OAuth2
                    provider: default_provider(),
                    active: true,
                    ignore_patterns: Vec::new(),
                });
                migrated = true;
            }
        }

        Ok((cfg, migrated))
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let toml = toml::to_string_pretty(self).context("Erreur sérialisation V2")?;
        std::fs::write(&path, toml)?;
        Ok(())
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.sync_pairs.is_empty() {
            return Err(ConfigError::NoPairsConfigured);
        }
        for (i, pair) in self.sync_pairs.iter().enumerate() {
            if pair.name.trim().is_empty() {
                return Err(ConfigError::PairNameEmpty(i));
            }
            if pair.local_path.as_os_str().is_empty() {
                return Err(ConfigError::PairLocalEmpty(i));
            }
            if pair.active && !pair.local_path.is_dir() {
                return Err(ConfigError::PairLocalMissing(i, pair.local_path.clone()));
            }
        }
        Ok(())
    }

    pub fn is_valid(&self) -> bool {
        self.validate().is_ok()
    }

    fn expand_tildes(&mut self) {
        for pair in &mut self.sync_pairs {
            pair.local_path = expand_tilde(&pair.local_path);
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn expand_tilde(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if s.starts_with("~/") || s == "~" {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        PathBuf::from(home).join(&s[2..])
    } else {
        path.to_path_buf()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_v2_config() {
        let toml = r#"
            max_workers = 8
            [[sync_pairs]]
            name = "Projets"
            local_path = "/home/user/Projets"
            remote_folder_id = "12345"
        "#;
        let (cfg, migrated) = AppConfig::parse_and_migrate(toml).unwrap();
        assert!(!migrated);
        assert_eq!(cfg.max_workers, 8);
        assert_eq!(cfg.sync_pairs.len(), 1);
        assert_eq!(cfg.sync_pairs[0].name, "Projets");
    }

    #[test]
    fn test_deserialize_v1_config_migrates() {
        let toml = r#"
            local_root = "/home/user/OldV1"
            remote_root = "gdrive:/Backup"
            max_workers = 2
        "#;
        let (cfg, migrated) = AppConfig::parse_and_migrate(toml).unwrap();
        assert!(migrated);
        assert_eq!(cfg.sync_pairs.len(), 1);
        assert_eq!(cfg.sync_pairs[0].local_path, PathBuf::from("/home/user/OldV1"));
        assert_eq!(cfg.max_workers, 2); // Les autres champs sont préservés
    }

    #[test]
    fn test_validate_empty_pairs() {
        let cfg = AppConfig::default();
        assert!(matches!(cfg.validate(), Err(ConfigError::NoPairsConfigured)));
    }

    #[test]
    fn test_validate_inactive_pair_missing_dir() {
        let mut cfg = AppConfig::default();
        cfg.sync_pairs.push(SyncPair {
            name: "Test".into(),
            local_path: PathBuf::from("/chemin/inexistant/xyz"),
            remote_folder_id: "".into(),
            provider: "GoogleDrive".into(),
            active: false, // Inactif ! Donc ça doit passer la validation
            ignore_patterns: vec![],
        });
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_validate_active_pair_missing_dir() {
        let mut cfg = AppConfig::default();
        cfg.sync_pairs.push(SyncPair {
            name: "Test".into(),
            local_path: PathBuf::from("/chemin/inexistant/xyz"),
            remote_folder_id: "".into(),
            provider: "GoogleDrive".into(),
            active: true, // Actif ! Ça doit planter
            ignore_patterns: vec![],
        });
        assert!(matches!(cfg.validate(), Err(ConfigError::PairLocalMissing(_, _))));
    }

    #[test]
    fn test_default_advanced_values() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.advanced.resumable_upload_threshold, 5_242_880);
        assert_eq!(cfg.advanced.delete_mode, "trash");
    }
}