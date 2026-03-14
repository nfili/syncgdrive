//! Configuration de l'application SyncGDrive.
//!
//! Ce module gère le chargement, la validation et la sauvegarde de la
//! configuration TOML située dans `$XDG_CONFIG_HOME/syncgdrive/config.toml`
//! (défaut : `~/.config/syncgdrive/config.toml`).
//!
//! # Structure
//!
//! - [`AppConfig`] : configuration principale (chemins, workers, retry, exclusions)
//! - [`RetryConfig`] : paramètres de retry avec backoff exponentiel
//! - [`ConfigError`] : erreurs de validation typées (via `thiserror`)
//!
//! # Premier lancement
//!
//! Si le fichier n'existe pas, [`AppConfig::load_or_create`] crée un fichier
//! par défaut avec des valeurs vides pour `local_root` et `remote_root`.
//! La validation échouera → le moteur passe en mode `Unconfigured` et la
//! fenêtre Settings s'ouvre automatiquement.
//!
//! # Protocoles distants supportés
//!
//! `gdrive:/`, `smb://`, `sftp://`, `webdav://`, `ftp://`

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use thiserror::Error;

// ── Valeurs par défaut ────────────────────────────────────────────────────────

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
fn default_kio_timeout_secs() -> u64 { 120 }
fn default_rescan_interval_min() -> u64 { 30 }

// ── Structures ────────────────────────────────────────────────────────────────

/// Configuration du retry automatique avec backoff exponentiel.
///
/// Utilisé par [`scan::retry()`](crate::engine::scan::retry) pour les opérations
/// KIO qui échouent de façon transitoire (timeout réseau, latence GDrive…).
///
/// Le backoff double à chaque tentative, plafonné à [`max_backoff_ms`](Self::max_backoff_ms).
/// Les erreurs fatales (auth/token/403/401/quota) court-circuitent le retry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryConfig {
    /// Nombre maximal de tentatives (défaut : 3).
    #[serde(default = "default_retry_attempts")]
    pub max_attempts: u32,
    /// Délai initial en millisecondes avant la première re-tentative (défaut : 300ms).
    #[serde(default = "default_initial_backoff_ms")]
    pub initial_backoff_ms: u64,
    /// Plafond du backoff en millisecondes (défaut : 8000ms).
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

/// Configuration principale de l'application.
///
/// Sérialisée/désérialisée depuis le fichier TOML via `serde`.
/// Tous les champs ont des valeurs par défaut via `#[serde(default)]`.
///
/// # Exemple TOML
///
/// ```toml
/// local_root = "/home/user/Projets"
/// remote_root = "gdrive:/MonDrive/Backup"
/// max_workers = 4
/// notifications = true
/// kio_timeout_secs = 120
/// rescan_interval_min = 30
///
/// [retry]
/// max_attempts = 3
/// initial_backoff_ms = 300
/// max_backoff_ms = 8000
///
/// ignore_patterns = ["**/target/**", "**/.git/**"]
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppConfig {
    /// Répertoire local à surveiller. Vide = non configuré.
    #[serde(default)]
    pub local_root: PathBuf,
    /// URL distante KIO, ex: gdrive:/MonDrive/Backup
    #[serde(default)]
    pub remote_root: String,
    #[serde(default = "default_ignore_patterns")]
    pub ignore_patterns: Vec<String>,
    #[serde(default = "default_max_workers")]
    pub max_workers: usize,
    #[serde(default)]
    pub retry: RetryConfig,
    #[serde(default)]
    pub notifications: bool,
    /// Timeout (secondes) pour les opérations KIO rapides (stat, mkdir, ls, rm, move).
    /// Les transferts de fichiers (copy/cat) ne sont PAS limités.
    #[serde(default = "default_kio_timeout_secs")]
    pub kio_timeout_secs: u64,
    /// Intervalle (minutes) entre les rescans automatiques.
    /// Vérifie l'égalité local = DB = remote même sans événement inotify.
    /// 0 = désactivé. Défaut = 30 min.
    #[serde(default = "default_rescan_interval_min")]
    pub rescan_interval_min: u64,
}

// ── Erreurs de validation ─────────────────────────────────────────────────────

/// Erreurs de validation de la configuration.
///
/// Utilisées par [`AppConfig::validate`] pour signaler les problèmes
/// avant le démarrage du moteur. Chaque variante produit un message
/// d'erreur lisible en français.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("le chemin local n'est pas configuré — ouvrez Settings")]
    LocalRootEmpty,
    #[error("le dossier local '{0}' n'existe pas — créez-le ou modifiez Settings")]
    LocalRootMissing(PathBuf),
    #[error("le chemin local '{0}' n'est pas un dossier")]
    LocalRootNotDir(PathBuf),
    #[error("le remote n'est pas configuré — ouvrez Settings")]
    RemoteEmpty,
    #[error("protocole KIO non reconnu dans '{0}' (attendu: gdrive:/, smb:/, sftp:/…)")]
    RemoteInvalidProtocol(String),
}

// ── Chemin de la config ───────────────────────────────────────────────────────

/// Retourne le chemin du fichier de configuration TOML.
///
/// Respecte la convention XDG : `$XDG_CONFIG_HOME/syncgdrive/config.toml`.
/// Défaut si `XDG_CONFIG_HOME` n'est pas défini : `~/.config/syncgdrive/config.toml`.
pub fn config_path() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".config")
        });
    base.join("syncgdrive").join("config.toml")
}

// ── Chargement / création ─────────────────────────────────────────────────────

impl AppConfig {
    /// Charge la config depuis le disque.
    /// Retourne `(config, is_first_run)`.
    pub fn load_or_create() -> Result<(Self, bool)> {
        let path = config_path();

        if !path.exists() {
            // Premier lancement : on crée un fichier vide avec les commentaires.
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("cannot create config dir {}", parent.display()))?;
            }
            let default = AppConfig::default();
            let toml = toml::to_string_pretty(&default)
                .context("cannot serialize default config")?;
            std::fs::write(&path, toml)
                .with_context(|| format!("cannot write config to {}", path.display()))?;
            return Ok((default, true));
        }

        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read config from {}", path.display()))?;
        let mut cfg: AppConfig = toml::from_str(&raw)
            .with_context(|| format!("cannot parse config at {}", path.display()))?;

        // Canonicalise le tilde dans local_root.
        cfg.local_root = expand_tilde(&cfg.local_root);

        Ok((cfg, false))
    }

    /// Sauvegarde la config sur le disque.
    pub fn save(&self) -> Result<()> {
        let path = config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let toml = toml::to_string_pretty(self).context("cannot serialize config")?;
        std::fs::write(&path, toml)
            .with_context(|| format!("cannot write config to {}", path.display()))?;
        Ok(())
    }

    /// Vérifie que la config est suffisante pour démarrer le moteur.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.local_root.as_os_str().is_empty() {
            return Err(ConfigError::LocalRootEmpty);
        }
        if !self.local_root.exists() {
            return Err(ConfigError::LocalRootMissing(self.local_root.clone()));
        }
        if !self.local_root.is_dir() {
            return Err(ConfigError::LocalRootNotDir(self.local_root.clone()));
        }
        if self.remote_root.is_empty() {
            return Err(ConfigError::RemoteEmpty);
        }
        let supported = ["gdrive://", "gdrive:/", "smb://", "sftp://", "webdav://", "ftp://"];
        if !supported.iter().any(|p| self.remote_root.starts_with(p)) {
            return Err(ConfigError::RemoteInvalidProtocol(self.remote_root.clone()));
        }
        Ok(())
    }

    /// Raccourci booléen.
    pub fn is_valid(&self) -> bool {
        self.validate().is_ok()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_invalid() {
        let cfg = AppConfig::default();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn expand_tilde_works() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
        let p = expand_tilde(&PathBuf::from("~/foo/bar"));
        assert_eq!(p, PathBuf::from(format!("{home}/foo/bar")));
    }

    #[test]
    fn expand_tilde_no_change_without_tilde() {
        let p = expand_tilde(&PathBuf::from("/absolute/path"));
        assert_eq!(p, PathBuf::from("/absolute/path"));
    }

    #[test]
    fn validate_empty_local_root() {
        let cfg = AppConfig { remote_root: "gdrive:/test".into(), ..Default::default() };
        assert!(matches!(cfg.validate(), Err(ConfigError::LocalRootEmpty)));
    }

    #[test]
    fn validate_missing_local_root() {
        let cfg = AppConfig {
            local_root: PathBuf::from("/inexistant_syncgdrive_test_dir_xyz"),
            remote_root: "gdrive:/test".into(),
            ..Default::default()
        };
        assert!(matches!(cfg.validate(), Err(ConfigError::LocalRootMissing(_))));
    }

    #[test]
    fn validate_empty_remote() {
        let cfg = AppConfig {
            local_root: std::env::temp_dir(),
            ..Default::default()
        };
        assert!(matches!(cfg.validate(), Err(ConfigError::RemoteEmpty)));
    }

    #[test]
    fn validate_invalid_protocol() {
        let cfg = AppConfig {
            local_root: std::env::temp_dir(),
            remote_root: "http://invalid".into(),
            ..Default::default()
        };
        assert!(matches!(cfg.validate(), Err(ConfigError::RemoteInvalidProtocol(_))));
    }

    #[test]
    fn validate_valid_gdrive_config() {
        let cfg = AppConfig {
            local_root: std::env::temp_dir(),
            remote_root: "gdrive:/MonDrive/Backup".into(),
            ..Default::default()
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_all_supported_protocols() {
        for proto in &["gdrive:/", "smb://", "sftp://", "webdav://", "ftp://"] {
            let cfg = AppConfig {
                local_root: std::env::temp_dir(),
                remote_root: format!("{proto}test"),
                ..Default::default()
            };
            assert!(cfg.validate().is_ok(), "protocol {proto} should be valid");
        }
    }

    #[test]
    fn is_valid_shortcut() {
        let cfg = AppConfig::default();
        assert!(!cfg.is_valid());
    }

    #[test]
    fn default_values_via_deserialize() {
        // Les serde(default) s'appliquent au parsing TOML, pas à Default::default()
        let cfg: AppConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.max_workers, 4);
        assert_eq!(cfg.kio_timeout_secs, 120);
        assert_eq!(cfg.rescan_interval_min, 30);
        assert_eq!(cfg.retry.max_attempts, 3);
        assert!(!cfg.notifications);
    }

    #[test]
    fn deserialize_minimal_toml() {
        let toml_str = r#"
            local_root = "/tmp"
            remote_root = "gdrive:/Test"
        "#;
        let cfg: AppConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.local_root, PathBuf::from("/tmp"));
        assert_eq!(cfg.remote_root, "gdrive:/Test");
        assert_eq!(cfg.max_workers, 4); // défaut
    }
}

