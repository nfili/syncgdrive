use anyhow::{Context, Result};
use keyring::Entry;
use super::GoogleTokens;

/// Interface commune pour le stockage des tokens
pub trait TokenStorage: Send + Sync {
    fn store(&self, tokens: &GoogleTokens) -> Result<()>;
    fn load(&self) -> Result<Option<GoogleTokens>>;
    fn clear(&self) -> Result<()>;
}

/// Implémentation du stockage via le trousseau système sécurisé (Secret Service / KWallet)
pub struct SystemKeyring {
    entry: Entry,
}

impl SystemKeyring {
    /// Crée une nouvelle connexion au trousseau système
    pub fn new(app_name: &str, username: &str) -> Result<Self> {
        let entry = Entry::new(app_name, username)
            .context("Impossible d'initialiser l'accès au trousseau système")?;
        Ok(Self { entry })
    }
}

impl TokenStorage for SystemKeyring {
    fn store(&self, tokens: &GoogleTokens) -> Result<()> {
        let json = serde_json::to_string(tokens)
            .context("Erreur lors de la sérialisation des tokens")?;

        self.entry.set_password(&json)
            .context("Impossible de sauvegarder les tokens dans le trousseau système")?;

        Ok(())
    }

    fn load(&self) -> Result<Option<GoogleTokens>> {
        match self.entry.get_password() {
            Ok(json) => {
                let tokens = serde_json::from_str(&json)
                    .context("Erreur de lecture des tokens depuis le trousseau")?;
                Ok(Some(tokens))
            }
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e).context("Erreur d'accès au trousseau système"),
        }
    }

    fn clear(&self) -> Result<()> {
        match self.entry.delete_credential() {
            Ok(_) => Ok(()),
            Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e).context("Impossible de supprimer les tokens du trousseau"),
        }
    }
}