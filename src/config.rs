use std::path::PathBuf;

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

fn default_max_workers() -> usize { 2 }
fn default_retry_attempts() -> u32 { 3 }
fn default_initial_backoff_ms() -> u64 { 300 }
fn default_max_backoff_ms() -> u64 { 8_000 }

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
}

// ── Erreurs de validation ─────────────────────────────────────────────────────

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

fn expand_tilde(path: &PathBuf) -> PathBuf {
    let s = path.to_string_lossy();
    if s.starts_with("~/") || s == "~" {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        PathBuf::from(home).join(&s[2..])
    } else {
        path.clone()
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
}

