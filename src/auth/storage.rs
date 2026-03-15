use anyhow::Result;
use super::GoogleTokens;

/// Interface commune pour le stockage des tokens
pub trait TokenStorage: Send + Sync {
    fn store(&self, tokens: &GoogleTokens) -> Result<()>;
    fn load(&self) -> Result<Option<GoogleTokens>>;
    fn clear(&self) -> Result<()>;
}