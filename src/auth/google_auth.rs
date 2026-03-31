//! Orchestrateur de haut niveau pour l'authentification Google Drive.
//!
//! Ce module fait le pont entre le stockage sécurisé local (`EncryptedFileStorage`)
//! et les flux réseau OAuth2 purs (`oauth2.rs`). Il gère le cycle de vie complet
//! des identifiants : chargement, validation, rafraîchissement transparent,
//! et reconnexion forcée en cas de révocation.

use crate::auth::oauth2::{GoogleTokens, OAuthAppCredentials};
use crate::auth::storage::{EncryptedFileStorage, TokenStorage};
use anyhow::{Context, Result};
use oauth2::basic::BasicClient;
use oauth2::reqwest::async_http_client;
use oauth2::{AuthUrl, ClientId, ClientSecret, RefreshToken, TokenResponse, TokenUrl};
use serde::Deserialize;

/// Structure de réponse pour récupérer les métadonnées de l'utilisateur.
#[derive(Deserialize)]
struct DriveAbout {
    user: DriveUser,
}

/// Détails de l'utilisateur retournés par l'API Drive.
#[derive(Deserialize)]
struct DriveUser {
    #[serde(rename = "emailAddress")]
    email_address: String,
}

/// Gestionnaire central de l'authentification et des sessions Google.
pub struct GoogleAuth {
    storage: EncryptedFileStorage,
    creds: OAuthAppCredentials,
}

impl Default for GoogleAuth {
    fn default() -> Self {
        Self::new()
    }
}

impl GoogleAuth {
    /// Initialise le gestionnaire d'authentification.
    ///
    /// # Panics
    /// Panique si la variable d'environnement `SYNCGDRIVE_CLIENT_SECRET` est absente,
    /// car le moteur de chiffrement sous-jacent ne peut pas être instancié sans elle.
    pub fn new() -> Self {
        Self {
            // On utilise les mêmes identifiants que dans storage.rs
            // Unexpect explicite si la clé de chiffrement est introuvable
            storage: EncryptedFileStorage::new()
                .expect("Impossible d'initialiser le chiffrement (CLIENT_SECRET manquant)"),
            creds: OAuthAppCredentials::default(),
        }
    }

    /// Sauvegarde manuelle (utile après le premier login)
    ///
    /// Persiste les jetons fournis dans le stockage chiffré local.
    pub fn save_tokens(&self, tokens: &GoogleTokens) -> Result<()> {
        self.storage.store(tokens)
    }

    /// La fonction "Pro" pour le démarrage : Charge, Rafraîchit et Valide
    /// La fonction "Pro" pour le démarrage : Charge, Rafraîchit, et Reconnecte si besoin.
    ///
    /// C'est le cœur du système d'authentification :
    /// 1. Charge les jetons depuis le disque.
    /// 2. S'ils expirent dans plus de 5 minutes (300s), les retourne directement.
    /// 3. Sinon, tente un rafraîchissement silencieux via l'API Google.
    /// 4. Si le rafraîchissement échoue (jeton révoqué), supprime le cache, notifie
    ///    l'utilisateur et lance un nouveau flux d'authentification interactif.
    pub async fn get_valid_token(&self) -> Result<String> {
        let tokens = self
            .storage
            .load()?
            .context("Aucun jeton trouvé. Veuillez vous connecter dans les paramètres.")?;

        let now = chrono::Utc::now().timestamp();

        // Si le token est encore bon (marge 5 min)
        if tokens.expires_at > now + 300 {
            return Ok(tokens.access_token);
        }

        tracing::info!("Jeton expiré, rafraîchissement via Google...");

        let client = BasicClient::new(
            ClientId::new(self.creds.client_id.clone()),
            Some(ClientSecret::new(self.creds.client_secret.clone())),
            AuthUrl::new("https://accounts.google.com/o/oauth2/v2/auth".into())?,
            Some(TokenUrl::new("https://oauth2.googleapis.com/token".into())?),
        );

        // On sépare l'exécution de l'analyse du résultat
        let token_result = client
            .exchange_refresh_token(&RefreshToken::new(tokens.refresh_token.clone()))
            .request_async(async_http_client)
            .await;

        let token_response = match token_result {
            Ok(res) => res,
            Err(e) => {
                tracing::warn!(
                    "⚠️ Le jeton de rafraîchissement a été rejeté (révoqué ou expiré) : {}",
                    e
                );
                tracing::info!("Suppression du jeton local corrompu...");

                // 1. On nettoie le fichier chiffré qui ne fonctionne plus
                let _ = self.storage.clear();

                // 2. On avertit vocalement ou visuellement (Optionnel, mais recommandé pour Bella).
                #[cfg(target_os = "linux")]
                let _ = std::process::Command::new("notify-send")
                    .args(["-a", "SyncGDrive", "-i", "dialog-warning", "Reconnexion requise", "Votre session Google a expiré. Veuillez autoriser l'application dans votre navigateur."])
                    .spawn();

                tracing::info!("🌐 Ouverture du navigateur pour re-connexion...");

                // 3. On force la nouvelle authentification !
                let new_tokens = crate::auth::oauth2::authenticate(&self.creds)
                    .await
                    .context("Échec de la nouvelle authentification via le navigateur")?;

                // 4. On sauvegarde et on renvoie le nouveau jeton tout neuf
                self.save_tokens(&new_tokens)?;
                return Ok(new_tokens.access_token);
            }
        };

        // Si le rafraîchissement standard a fonctionné :
        let new_tokens = GoogleTokens {
            access_token: token_response.access_token().secret().clone(),
            refresh_token: token_response
                .refresh_token()
                .map(|r| r.secret().clone())
                .unwrap_or(tokens.refresh_token), // On garde l'ancien si pas de nouveau
            expires_at: chrono::Utc::now().timestamp()
                + token_response
                    .expires_in()
                    .map(|d| d.as_secs())
                    .unwrap_or(3599) as i64,
            scope: tokens.scope.clone(),
        };

        self.save_tokens(&new_tokens)?;
        Ok(new_tokens.access_token)
    }

    /// Révoque l'accès côté serveur (Google) et supprime le fichier local chiffré.
    ///
    /// Contrairement à une simple déconnexion locale, cette méthode informe Google
    /// que l'application ne doit plus avoir accès au compte.
    pub async fn revoke_token(&self) -> Result<()> {
        // 1. Tenter de lire le token actuel pour le révoquer côté serveur
        if let Ok(Some(tokens)) = self.storage.load() {
            tracing::info!("Envoi de la requête de révocation à Google...");
            let client = reqwest::Client::new();

            // Envoyer le refresh_token révoque toute la chaîne (y compris l'access_token)
            let res = client
                .post("https://oauth2.googleapis.com/revoke")
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(format!("token={}", tokens.refresh_token))
                .send()
                .await;

            if let Err(e) = res {
                // On log l'erreur, mais on ne bloque pas la suppression locale (utile si on est hors-ligne).
                tracing::warn!("Impossible de joindre Google pour la révocation : {}", e);
            }
        }

        // 2. Suppression systématique du fichier local tokens.enc
        tracing::info!("Suppression du fichier chiffré local...");
        self.storage
            .clear()
            .context("Erreur lors de la suppression du fichier de tokens")?;

        Ok(())
    }

    /// Méthode utilitaire simple pour vérifier si on a un token local (sans faire d'appel réseau)
    ///
    /// Utilisé principalement pour conditionner l'affichage de l'interface utilisateur.
    pub fn is_locally_connected(&self) -> bool {
        self.storage
            .load()
            .map(|opt| opt.is_some())
            .unwrap_or(false)
    }

    /// Interroge l'API Google Drive pour récupérer l'adresse email de l'utilisateur
    ///
    /// Fait appel à l'endpoint `drive/v3/about` avec le champ `user`.
    pub async fn get_user_email(&self) -> Result<String> {
        let token = self.get_valid_token().await?;
        let client = reqwest::Client::new();

        let res = client
            .get("https://www.googleapis.com/drive/v3/about?fields=user")
            .bearer_auth(token)
            .send()
            .await?
            .error_for_status()?; // Déclenche une erreur si le statut HTTP n'est pas 2xx

        let about: DriveAbout = res
            .json()
            .await
            .context("Erreur lors de la lecture du profil utilisateur")?;

        Ok(about.user.email_address)
    }

    /// Lit la date d'expiration du jeton depuis le fichier chiffré
    ///
    /// Retourne la date formatée en `YYYY-MM-DD HH:MM` ou "Inconnue" si
    /// aucun jeton valide n'est présent localement.
    pub fn get_token_expiration_date(&self) -> String {
        if let Ok(Some(tokens)) = self.storage.load() {
            // Convertit le timestamp en DateTime
            if let Some(dt) = chrono::DateTime::from_timestamp(tokens.expires_at, 0) {
                // Formate la date selon ta spécification : YYYY-MM-DD
                return dt.format("%Y-%m-%d %H:%M").to_string();
            }
        }
        "Inconnue".to_string()
    }
}
