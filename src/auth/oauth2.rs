use serde::{Deserialize, Serialize};

/// Tokens OAuth2 pour un compte Google.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoogleTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: i64,    // timestamp Unix
    pub scope: String,
}

/// Résultat d'un refresh de token.
pub enum TokenStatus {
    Valid(String),           // access_token valide
    Refreshed(GoogleTokens), // nouveau jeu de tokens
    Expired,                 // refresh_token invalide → re-auth nécessaire
}

/// Identifiants de l'application OAuth2.
#[derive(Debug, Clone)]
pub struct OAuthAppCredentials {
    pub client_id: String,
    pub client_secret: String,
    pub redirect_uri: String,
}

impl Default for OAuthAppCredentials {
    fn default() -> Self {
        Self {
            // Valeurs par défaut embarquées (overridables via env vars)
            client_id: std::env::var("SYNCGDRIVE_CLIENT_ID")
                .unwrap_or_else(|_| "À_REMPLIR_PLUS_TARD".into()),
            client_secret: std::env::var("SYNCGDRIVE_CLIENT_SECRET")
                .unwrap_or_else(|_| "À_REMPLIR_PLUS_TARD".into()),
            redirect_uri: "http://127.0.0.1".into(), // Le port sera dynamique
        }
    }
}