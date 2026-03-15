use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use oauth2::basic::BasicClient;
use oauth2::reqwest::async_http_client;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, PkceCodeChallenge,
    RedirectUrl, Scope, TokenResponse, TokenUrl,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use url::Url;

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
impl OAuthAppCredentials {
    /// Construit le client OAuth2 configuré pour Google
    pub fn build_client(&self, port: u16) -> Result<BasicClient> {
        let client_id = ClientId::new(self.client_id.clone());
        let client_secret = ClientSecret::new(self.client_secret.clone());
        let auth_url = AuthUrl::new("https://accounts.google.com/o/oauth2/v2/auth".to_string())?;
        let token_url = TokenUrl::new("https://oauth2.googleapis.com/token".to_string())?;
        let redirect_url = RedirectUrl::new(format!("http://127.0.0.1:{}", port))?;

        Ok(BasicClient::new(
            client_id,
            Some(client_secret),
            auth_url,
            Some(token_url),
        )
            .set_redirect_uri(redirect_url))
    }
}

/// Lance le flux d'authentification complet via le navigateur
pub async fn authenticate(creds: &OAuthAppCredentials) -> Result<GoogleTokens> {
    // 1. Démarrer un serveur local éphémère (port 0 = assigné par l'OS)
    let listener = TcpListener::bind("127.0.0.1:0").await.context("Impossible de lier un port local")?;
    let port = listener.local_addr()?.port();

    let client = creds.build_client(port)?;

    // 2. Générer l'URL d'autorisation avec sécurité PKCE
    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
    let (auth_url, csrf_token) = client
        .authorize_url(CsrfToken::new_random)
        .add_scope(Scope::new("https://www.googleapis.com/auth/drive.file".to_string())) // Scope restreint (sécurité)
        .set_pkce_challenge(pkce_challenge)
        .url();

    // 3. Ouvrir le navigateur
    tracing::info!("Ouverture du navigateur pour l'authentification Google...");
    println!("Si le navigateur ne s'ouvre pas, visitez ce lien :\n{}\n", auth_url);

    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(auth_url.as_str()).spawn();

    // 4. Attendre le retour de Google sur le port local
    let (mut stream, _) = listener.accept().await?;
    let mut reader = BufReader::new(&mut stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).await?;

    // Envoyer une réponse HTTP propre pour fermer l'onglet
    let response = "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<html><body><h1>Authentification réussie !</h1><p>Vous pouvez fermer cet onglet et retourner à l'application SyncGDrive.</p></body></html>";
    stream.write_all(response.as_bytes()).await?;

    // 5. Extraire et valider le code retourné
    let redirect_url = request_line.split_whitespace().nth(1).context("Requête HTTP invalide")?;
    let url = Url::parse(&format!("http://localhost{}", redirect_url))?;

    let code = url.query_pairs()
        .find(|(key, _)| key == "code")
        .map(|(_, value)| AuthorizationCode::new(value.into_owned()))
        .context("Code d'autorisation non trouvé dans la requête")?;

    let state = url.query_pairs()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| CsrfToken::new(value.into_owned()))
        .context("State CSRF non trouvé")?;

    if state.secret() != csrf_token.secret() {
        anyhow::bail!("Attaque CSRF détectée (le paramètre state ne correspond pas)");
    }

    // 6. Échanger le code contre les jetons
    let token_result = client
        .exchange_code(code)
        .set_pkce_verifier(pkce_verifier)
        .request_async(async_http_client)
        .await
        .context("Échec lors de l'échange du code OAuth2")?;

    let expires_in = token_result.expires_in().unwrap_or(std::time::Duration::from_secs(3599)).as_secs() as i64;
    let now = chrono::Utc::now().timestamp();

    Ok(GoogleTokens {
        access_token: token_result.access_token().secret().clone(),
        refresh_token: token_result.refresh_token().map(|r| r.secret().clone()).unwrap_or_default(),
        expires_at: now + expires_in,
        scope: "https://www.googleapis.com/auth/drive.file".to_string(),
    })
}