use anyhow::{Context, Result};
use crate::auth::oauth2::{GoogleTokens, OAuthAppCredentials};
use crate::auth::storage::{EncryptedFileStorage, TokenStorage};
use oauth2::basic::BasicClient;
use oauth2::reqwest::async_http_client;
use oauth2::{AuthUrl, ClientId, ClientSecret, TokenResponse, TokenUrl, RefreshToken};

pub struct GoogleAuth {
    storage: EncryptedFileStorage,
    creds: OAuthAppCredentials,
}

impl GoogleAuth {
    pub fn new() -> Self {
        Self {
            // On utilise les mêmes identifiants que dans storage.rs
            // Unexpect explicite si la clé de chiffrement est introuvable
            storage: EncryptedFileStorage::new().expect("Impossible d'initialiser le chiffrement (CLIENT_SECRET manquant)"),
            creds: OAuthAppCredentials::default(),
        }
    }

    /// Sauvegarde manuelle (utile après le premier login)
    pub fn save_tokens(&self, tokens: &GoogleTokens) -> Result<()> {
        self.storage.store(tokens)
    }

    /// La fonction "Pro" pour le démarrage : Charge, Rafraîchit et Valide
    pub async fn get_valid_token(&self) -> Result<String> {
        let tokens = self.storage.load()?
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

        let token_response = client
            .exchange_refresh_token(&RefreshToken::new(tokens.refresh_token.clone()))
            .request_async(async_http_client)
            .await
            .context("Le rafraîchissement a échoué (accès peut-être révoqué)")?;

        let new_tokens = GoogleTokens {
            access_token: token_response.access_token().secret().clone(),
            refresh_token: token_response.refresh_token()
                .map(|r| r.secret().clone())
                .unwrap_or(tokens.refresh_token), // On garde l'ancien si pas de nouveau
            expires_at: chrono::Utc::now().timestamp() +
                token_response.expires_in().map(|d| d.as_secs()).unwrap_or(3599) as i64,
            scope: tokens.scope.clone(),
        };

        self.save_tokens(&new_tokens)?;
        Ok(new_tokens.access_token)
    }
}