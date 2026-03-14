# Phase 2 — Module OAuth2 Google Drive

---

## 1. Objectif

Implémenter l'authentification OAuth2 pour accéder à l'API Google Drive v3 sans aucun coût. L'utilisateur lie son compte Google via un flux navigateur, et le refresh token est stocké de façon sécurisée.

---

## 2. Pré-requis

- **Phase 1** : `AppConfig` V2 opérationnelle (structure `SyncPair` avec `remote_folder_id`).

---

## 3. Fichiers Impactés

| Action | Fichier | Description |
|--------|---------|-------------|
| **Créer** | `src/auth/mod.rs` | Module public, re-exports |
| **Créer** | `src/auth/oauth2.rs` | Flux OAuth2 (loopback redirect), token refresh |
| **Créer** | `src/auth/storage.rs` | Stockage sécurisé des tokens (secret-service / fichier chiffré) |
| **Modifier** | `src/lib.rs` | Déclarer `pub mod auth;` |
| **Modifier** | `src/ui/settings.rs` | Bouton « Lier un compte Google » + wizard |
| **Modifier** | `src/ui/tray.rs` | Nouveau `GtkAction::ShowOAuthWizard` |
| **Modifier** | `Cargo.toml` | Dépendances : `oauth2`, `reqwest`, `keyring` ou `secret-service` |

---

## 4. Structures de Données

### 4.1 Tokens

```rust
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
```

### 4.2 Configuration OAuth2

```rust
/// Identifiants de l'application OAuth2.
/// Embarqués dans le binaire (Desktop App — pas de secret serveur).
pub struct OAuthAppCredentials {
    pub client_id: &'static str,
    pub client_secret: &'static str,  // Pour Desktop Apps, Google le considère "public"
    pub redirect_uri: &'static str,   // "http://127.0.0.1:{port}"
}
```

---

## 5. Spécification Détaillée

### 5.1 Flux OAuth2 — Desktop App (Loopback)

Google recommande le flux **"Desktop App"** pour les applications installées :

1. **Créer un projet Google Cloud Console** (gratuit, pas de billing) :
   - APIs & Services → Enable API → Google Drive API
   - OAuth consent screen → External → App name "SyncGDrive"
   - Credentials → Create OAuth client ID → Desktop App

2. **Flux d'autorisation** :
   ```
   SyncGDrive                     Navigateur                  Google
       │                              │                          │
       │─── Ouvrir URL auth ─────────▶│                          │
       │    (xdg-open)                │──── Login + consent ────▶│
       │                              │                          │
       │                              │◀─── Redirect ────────────│
       │◀── http://127.0.0.1:PORT ────│    ?code=AUTH_CODE       │
       │    (serveur local éphémère)  │                          │
       │                              │                          │
       │─── POST token endpoint ─────────────────────────────────▶│
       │◀── { access_token, refresh_token } ─────────────────────│
       │                                                          │
       │─── Stocker tokens (secret-service) ──▶ Keyring           │
   ```

3. **Serveur loopback éphémère** :
   - Bind sur `127.0.0.1:0` (port dynamique attribué par l'OS).
   - Attend UN seul GET avec `?code=…`.
   - Répond avec une page HTML « ✅ Autorisation réussie — vous pouvez fermer cette fenêtre. »
   - Se ferme immédiatement après.

4. **Scopes requis** :
   ```
   https://www.googleapis.com/auth/drive.file
   ```
   Ce scope donne accès uniquement aux fichiers **créés par l'application**. Pas d'accès à tout le Drive. C'est le scope le moins intrusif et il ne nécessite **pas de vérification Google** (pas de review process).

### 5.2 Refresh Automatique

```rust
impl GoogleAuth {
    /// Retourne un access_token valide.
    /// Rafraîchit automatiquement si expiré (avec marge de 60s).
    pub async fn get_valid_token(&self) -> Result<String> {
        let tokens = self.load_tokens()?;
        let now = chrono::Utc::now().timestamp();

        if tokens.expires_at - 60 > now {
            return Ok(tokens.access_token);
        }

        // Refresh
        let new_tokens = self.refresh(&tokens.refresh_token).await?;
        self.store_tokens(&new_tokens)?;
        Ok(new_tokens.access_token)
    }
}
```

### 5.3 Stockage Sécurisé des Tokens

**Stratégie à deux niveaux** :

| Niveau | Méthode | Quand |
|--------|---------|-------|
| **Préféré** | `secret-service` D-Bus (GNOME Keyring / KWallet) | Session graphique disponible |
| **Fallback** | Fichier chiffré `$XDG_DATA_HOME/syncgdrive/tokens.enc` | Pas de keyring (serveur, container) |

```rust
pub trait TokenStorage: Send + Sync {
    fn store(&self, tokens: &GoogleTokens) -> Result<()>;
    fn load(&self) -> Result<Option<GoogleTokens>>;
    fn clear(&self) -> Result<()>;
}

pub struct KeyringStorage;     // via crate `keyring`
pub struct FileStorage;        // AES-256, clé dérivée de machine-id
```

**Clé de chiffrement fallback** : Dérivée de `/etc/machine-id` + salt fixe via `argon2`. Pas parfait (un root peut déchiffrer), mais protège contre la lecture casual.

### 5.4 Wizard OAuth2 dans l'UI

Fenêtre libadwaita intégrée aux Settings :

```
┌───────────────────────────────────────────────────┐
│  SyncGDrive — Lier votre compte Google         ✕  │
├───────────────────────────────────────────────────┤
│                                                   │
│  🔐 Autorisation Google Drive                     │
│                                                   │
│  SyncGDrive a besoin d'accéder à votre Google     │
│  Drive pour synchroniser vos fichiers.            │
│                                                   │
│  Cliquez sur le bouton ci-dessous pour ouvrir     │
│  votre navigateur et autoriser l'accès.           │
│                                                   │
│            [ 🌐 Autoriser dans le navigateur ]    │
│                                                   │
│  ┌─────────────────────────────────────────────┐  │
│  │ ⏳ En attente de l'autorisation…            │  │
│  │    (le navigateur va s'ouvrir)              │  │
│  └─────────────────────────────────────────────┘  │
│                                                   │
│  Confidentialité : SyncGDrive n'accède qu'aux     │
│  fichiers qu'il crée lui-même (scope drive.file). │
│  Vos autres fichiers Drive restent privés.        │
│                                                   │
└───────────────────────────────────────────────────┘
```

Après succès :
```
│  ┌─────────────────────────────────────────────┐  │
│  │ ✅ Compte lié : user@gmail.com              │  │
│  │    Token valide jusqu'au 2026-04-13         │  │
│  │                                              │  │
│  │    [ Révoquer l'accès ]                     │  │
│  └─────────────────────────────────────────────┘  │
```

### 5.5 Client ID — Distribution

Pour un usage personnel (pas de publication sur les stores) :

- Le `client_id` et `client_secret` sont embarqués dans le binaire.
- Google considère le `client_secret` des Desktop Apps comme **"public"** (pas un vrai secret).
- L'utilisateur avancé peut fournir ses propres identifiants via des variables d'env :
  ```
  SYNCGDRIVE_CLIENT_ID=xxx
  SYNCGDRIVE_CLIENT_SECRET=yyy
  ```
- La doc README explique comment créer son propre projet Cloud Console.

### 5.6 Gestion de la Révocation

- Bouton « Révoquer l'accès » dans Settings → appel API `https://oauth2.googleapis.com/revoke?token=…`
- Suppression des tokens du keyring / fichier.
- Toutes les paires passent en `Unconfigured`.

---

## 6. Cas Limites

| Cas | Comportement attendu |
|-----|---------------------|
| Utilisateur ferme le navigateur sans autoriser | Timeout 120s → message « Autorisation annulée » |
| Navigateur pas trouvé (`xdg-open` échoue) | Afficher l'URL dans la fenêtre → copier-coller manuel |
| Refresh token révoqué côté Google | Détection via 401 → notification + re-auth automatique |
| Pas de `secret-service` D-Bus disponible | Fallback fichier chiffré + log warning |
| Plusieurs comptes Google (futur) | Un jeu de tokens par `provider` dans la config |
| Port loopback déjà occupé | Bind sur `127.0.0.1:0` → port dynamique (jamais de conflit) |
| Pas de connexion Internet au moment du wizard | Erreur réseau claire + bouton « Réessayer » |

---

## 7. Tests à Écrire

### Unitaires (`auth/oauth2.rs`)

- `test_build_auth_url` : URL d'autorisation contient client_id, redirect_uri, scope
- `test_parse_callback_code` : extraction du `code` depuis la query string
- `test_parse_callback_error` : gestion de `?error=access_denied`
- `test_token_expiry_check` : token expiré détecté correctement
- `test_token_refresh_margin` : refresh déclenché 60s avant expiration

### Unitaires (`auth/storage.rs`)

- `test_file_storage_roundtrip` : store → load → tokens identiques
- `test_file_storage_clear` : clear → load retourne None
- `test_file_storage_corruption` : fichier corrompu → erreur propre (pas de panic)

### Intégration (mocks)

- `test_oauth_flow_mock` : serveur loopback + client mock → tokens reçus
- `test_refresh_flow_mock` : appel refresh endpoint mock → nouveau access_token

---

## 8. Critères d'Acceptation

- [ ] Le flux OAuth2 loopback fonctionne de bout en bout (navigateur → callback → tokens)
- [ ] Les tokens sont stockés dans le keyring (ou fichier chiffré en fallback)
- [ ] Le refresh automatique fonctionne avant chaque appel API
- [ ] Le wizard OAuth2 est intégré dans la fenêtre Settings
- [ ] La révocation supprime les tokens et passe en `Unconfigured`
- [ ] Les variables d'env `SYNCGDRIVE_CLIENT_ID/SECRET` overrident les valeurs embarquées
- [ ] `cargo test` : tous les tests passent
- [ ] `cargo clippy` : 0 warning
- [ ] Le scope `drive.file` est utilisé (pas `drive` complet)

---

## 9. Coût Google Cloud Console — Confirmation 100% Gratuit

| Élément | Coût |
|---------|------|
| Projet Google Cloud | Gratuit |
| Activer Google Drive API | Gratuit |
| OAuth consent screen (External) | Gratuit |
| Credentials Desktop App | Gratuit |
| Quotas API (12 000 req/100s) | Gratuit |
| Pas de billing requis | ✅ |
| Pas de vérification app (scope `drive.file`) | ✅ |

> Le scope `drive.file` est un **"restricted scope"** mais ne nécessite PAS
> de vérification Google pour les Desktop Apps en mode "Testing" avec < 100 utilisateurs.
> Pour une publication large, il faudra soumettre une demande de vérification (gratuite aussi).

