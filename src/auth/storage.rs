
//! Stockage chiffré des jetons d'accès.
//!
//! Ce module protège les jetons d'authentification (Access Token et Refresh Token)
//! stockés sur le disque dur. Il utilise l'algorithme de chiffrement authentifié
//! **AES-256-GCM** pour garantir à la fois la confidentialité et l'intégrité des données.
//!
//! La clé de chiffrement maîtresse est dérivée du secret client (`SYNCGDRIVE_CLIENT_SECRET`)
//! via un hachage SHA-256.

use super::GoogleTokens;
use anyhow::{Context, Result};
use std::path::PathBuf;

use aes_gcm::{
    aead::{Aead, AeadCore, KeyInit, OsRng},
    Aes256Gcm, Key, Nonce,
};
use sha2::{Digest, Sha256};

/// Trait définissant le contrat pour le stockage persistant des jetons.
///
/// Les contraintes `Send + Sync` garantissent que l'implémentation peut être
/// partagée en toute sécurité entre plusieurs threads (workers Tokio).
pub trait TokenStorage: Send + Sync {
    /// Chiffre et sauvegarde les jetons sur le support de stockage.
    fn store(&self, tokens: &GoogleTokens) -> Result<()>;
    /// Charge et déchiffre les jetons. Retourne `None` s'ils n'existent pas.
    fn load(&self) -> Result<Option<GoogleTokens>>;
    /// Supprime définitivement les jetons du support de stockage.
    fn clear(&self) -> Result<()>;
}

/// Implémentation de `TokenStorage` utilisant un fichier local chiffré.
///
/// Le fichier généré contiendra :
/// `[Nonce (12 octets)] + [Texte chiffré + Tag d'authentification GCM]`
pub struct EncryptedFileStorage {
    /// Chemin absolu vers le fichier chiffré (ex: `~/.config/syncgdrive/tokens.enc`).
    path: PathBuf,
    /// Instance pré-configurée de l'algorithme AES-256-GCM.
    cipher: Aes256Gcm,
}

impl EncryptedFileStorage {
    /// Initialise le gestionnaire de stockage et dérive la clé cryptographique.
    ///
    /// # Erreurs
    /// Retourne une erreur si la variable d'environnement `SYNCGDRIVE_CLIENT_SECRET`
    /// est manquante, car elle est indispensable pour dériver la clé de chiffrement.
    pub fn new() -> Result<Self> {
        let path = crate::config::config_dir().join("tokens.enc");

        let app_secret = std::env::var("SYNCGDRIVE_CLIENT_SECRET")
            .context("SYNCGDRIVE_CLIENT_SECRET manquant pour le chiffrement. Avez-vous configuré votre fichier .env ?")?;

        // L'AES-256 nécessite exactement une clé de 32 octets (256 bits).
        // On utilise SHA-256 pour transformer le secret de longueur variable en une clé fixe.
        let key_bytes = Sha256::digest(app_secret.as_bytes());
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key_bytes));

        Ok(Self { path, cipher })
    }
}

impl TokenStorage for EncryptedFileStorage {
    fn store(&self, tokens: &GoogleTokens) -> Result<()> {
        let json = serde_json::to_string(tokens).context("Erreur de sérialisation des tokens vers JSON")?;

        // Génération d'un vecteur d'initialisation (Nonce) unique et aléatoire pour chaque écriture.
        // C'est vital pour la sécurité de l'AES-GCM afin d'éviter les attaques par réutilisation de Nonce.
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);

        // Chiffrement : Le résultat contient le texte chiffré ET le tag d'authentification (MAC)
        let ciphertext = self
            .cipher
            .encrypt(&nonce, json.as_bytes())
            .map_err(|e| anyhow::anyhow!("Échec du chiffrement AES-256-GCM : {:?}", e))?;

        // Format du fichier : On préfixe le texte chiffré par le Nonce en clair (il n'est pas secret)
        let mut file_content = nonce.to_vec();
        file_content.extend_from_slice(&ciphertext);

        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .context("Impossible de créer l'arborescence du dossier de configuration")?;
        }

        std::fs::write(&self.path, &file_content)
            .context("Impossible d'écrire le fichier chiffré sur le disque")?;

        // Défense en profondeur : on restreint les droits d'accès au niveau de l'OS (Unix uniquement).
        // Seul le propriétaire (l'utilisateur exécutant l'app) aura le droit de lecture/écriture.
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

        let data = std::fs::read(&self.path).context("Impossible de lire le fichier chiffré depuis le disque")?;

        // Pré-validation : un fichier valide contient au minimum le Nonce (12 octets) + le Tag GCM (16 octets)
        if data.len() < 28 {
            anyhow::bail!("Fichier de tokens corrompu (taille insuffisante pour contenir un Nonce et un MAC)");
        }

        let nonce = Nonce::from_slice(&data[..12]);
        let ciphertext = &data[12..];

        // Déchiffrement et validation de l'intégrité (vérification du Tag GCM interne)
        let plaintext = self.cipher.decrypt(nonce, ciphertext).map_err(|_| {
            anyhow::anyhow!("Échec du déchiffrement. Le CLIENT_SECRET a-t-il été modifié depuis la dernière connexion ?")
        })?;

        let json = String::from_utf8(plaintext)
            .context("Les données déchiffrées ne forment pas une chaîne de caractères UTF-8 valide")?;

        let tokens = serde_json::from_str(&json).context("Structure JSON des tokens illisible ou corrompue")?;

        Ok(Some(tokens))
    }

    fn clear(&self) -> Result<()> {
        if self.path.exists() {
            std::fs::remove_file(&self.path).context("Impossible de supprimer le fichier de tokens")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::oauth2::GoogleTokens;
    use std::env;
    use std::fs;
    use std::sync::Mutex;

    // Mutex global pour forcer l'exécution séquentielle de ces tests
    // car ils modifient des variables d'environnement globales.
    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    // Helper pour isoler les tests dans un dossier temporaire
    fn setup_test_env(test_name: &str) -> String {
        let temp_dir = env::temp_dir().join(format!("syncgdrive_test_{}", test_name));

        // On crée explicitement le sous-dossier attendu par l'application
        let app_config_dir = temp_dir.join("syncgdrive");
        fs::create_dir_all(&app_config_dir).unwrap();

        // Simule le ~/.config
        env::set_var("XDG_CONFIG_HOME", temp_dir.to_str().unwrap());
        // Clé de chiffrement factice pour le test AES-256-GCM
        env::set_var("SYNCGDRIVE_CLIENT_SECRET", "super_secret_de_test_12345");

        temp_dir.to_str().unwrap().to_string()
    }

    fn dummy_tokens() -> GoogleTokens {
        GoogleTokens {
            access_token: "access_123".into(),
            refresh_token: "refresh_456".into(),
            expires_at: 1700000000,
            scope: "drive.file".into(),
        }
    }

    #[test]
    fn test_file_storage_roundtrip() {
        let _lock = ENV_MUTEX.lock().unwrap(); // <-- On verrouille !

        setup_test_env("roundtrip");
        let storage = EncryptedFileStorage::new().expect("Init storage");
        let tokens = dummy_tokens();

        // Store
        storage.store(&tokens).expect("Store failed");

        // Load
        let loaded = storage
            .load()
            .expect("Load failed")
            .expect("Tokens should exist");

        // Identiques
        assert_eq!(loaded.access_token, tokens.access_token);
        assert_eq!(loaded.refresh_token, tokens.refresh_token);
    }

    #[test]
    fn test_file_storage_clear() {
        let _lock = ENV_MUTEX.lock().unwrap(); // <-- On verrouille !

        setup_test_env("clear");
        let storage = EncryptedFileStorage::new().unwrap();

        storage.store(&dummy_tokens()).unwrap();
        storage.clear().expect("Clear failed");

        let loaded = storage.load().unwrap();
        assert!(loaded.is_none(), "Les tokens devraient être supprimés");
    }

    #[test]
    fn test_file_storage_corruption() {
        let _lock = ENV_MUTEX.lock().unwrap(); // <-- On verrouille !

        let dir = setup_test_env("corruption");
        let storage = EncryptedFileStorage::new().unwrap();

        // On écrit volontairement un fichier poubelle de 15 octets
        let bad_data = vec![0u8; 15];
        fs::write(format!("{}/syncgdrive/tokens.enc", dir), bad_data).unwrap();

        let result = storage.load();

        // Doit retourner une erreur propre (Err), pas un panic!
        assert!(
            result.is_err(),
            "La corruption doit être gérée et retourner une erreur"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Échec du déchiffrement")
                || err_msg.contains("Fichier de tokens corrompu")
        );
    }
}
