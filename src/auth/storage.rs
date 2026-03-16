use anyhow::{Context, Result};
use std::path::PathBuf;
use super::GoogleTokens;

// Importation des outils de cryptographie (déjà dans ton Cargo.toml et worker)
use aes_gcm::{
    aead::{Aead, AeadCore, KeyInit, OsRng},
    Aes256Gcm, Key, Nonce,
};
use sha2::{Digest, Sha256};

pub trait TokenStorage: Send + Sync {
    fn store(&self, tokens: &GoogleTokens) -> Result<()>;
    fn load(&self) -> Result<Option<GoogleTokens>>;
    fn clear(&self) -> Result<()>;
}

/// Stockage chiffré (AES-256-GCM) dans un fichier local
pub struct EncryptedFileStorage {
    path: PathBuf,
    cipher: Aes256Gcm,
}

impl EncryptedFileStorage {
    pub fn new() -> Result<Self> {
        // Le fichier portera l'extension .enc
        let path = crate::config::config_dir().join("tokens.enc");

        // On récupère le secret de ton .env pour dériver une clé de chiffrement robuste
        let app_secret = std::env::var("SYNCGDRIVE_CLIENT_SECRET")
            .context("SYNCGDRIVE_CLIENT_SECRET manquant pour le chiffrement")?;

        // Le hash SHA-256 garantit une clé de 32 octets (256 bits) parfaite pour l'AES
        let key_bytes = Sha256::digest(app_secret.as_bytes());
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key_bytes));

        Ok(Self { path, cipher })
    }
}

impl TokenStorage for EncryptedFileStorage {
    fn store(&self, tokens: &GoogleTokens) -> Result<()> {
        let json = serde_json::to_string(tokens)
            .context("Erreur de sérialisation des tokens")?;

        // Génération d'un vecteur d'initialisation unique (Nonce) de 12 octets
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);

        // Chiffrement
        let ciphertext = self.cipher.encrypt(&nonce, json.as_bytes())
            .map_err(|e| anyhow::anyhow!("Échec du chiffrement AES: {:?}", e))?;

        // On concatène le Nonce (nécessaire au déchiffrement) et le texte chiffré
        let mut file_content = nonce.to_vec();
        file_content.extend_from_slice(&ciphertext);

        std::fs::write(&self.path, &file_content)
            .context("Impossible d'écrire le fichier chiffré")?;

        // On garde quand même la ceinture et les bretelles avec chmod 600
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(metadata) = std::fs::metadata(&self.path) {
                let mut perms = metadata.permissions();
                perms.set_mode(0o600);
                let _ = std::fs::set_permissions(&self.path, perms);
            }
        }

        Ok(())
    }

    fn load(&self) -> Result<Option<GoogleTokens>> {
        if !self.path.exists() {
            return Ok(None);
        }

        let data = std::fs::read(&self.path)
            .context("Impossible de lire le fichier chiffré")?;

        // Le fichier doit faire au moins la taille du nonce (12 octets)
        if data.len() < 12 {
            anyhow::bail!("Fichier de tokens corrompu (trop court)");
        }

        let nonce = Nonce::from_slice(&data[..12]);
        let ciphertext = &data[12..];

        // Déchiffrement
        let plaintext = self.cipher.decrypt(nonce, ciphertext)
            .map_err(|_| anyhow::anyhow!("Échec du déchiffrement. Le CLIENT_SECRET a-t-il changé ?"))?;

        let json = String::from_utf8(plaintext)
            .context("Les données déchiffrées ne sont pas du texte valide")?;

        let tokens = serde_json::from_str(&json)
            .context("Structure JSON des tokens corrompue")?;

        Ok(Some(tokens))
    }

    fn clear(&self) -> Result<()> {
        if self.path.exists() {
            std::fs::remove_file(&self.path)?;
        }
        Ok(())
    }
}