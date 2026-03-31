//! Module d'authentification et de gestion sécurisée des accès (OAuth2).
//!
//! Ce module centralise toute la logique nécessaire pour connecter l'application
//! à l'API Google Drive de manière robuste et sécurisée. Il orchestre trois piliers :
//! - **Le flux OAuth2** (PKCE, serveur local éphémère) pour l'approbation via le navigateur (`oauth2`).
//! - **Le cycle de vie** (rafraîchissement automatique, révocation, validation) des jetons (`google_auth`).
//! - **Le coffre-fort local** (chiffrement AES-256-GCM) pour protéger les identifiants sur le disque (`storage`).

pub mod google_auth;
pub mod oauth2;
pub mod storage;

// ── Réexportations publiques (Façade du module) ───────────────────────────────
// Ces réexportations simplifient l'API du module. Le reste de l'application
// peut importer ces structures directement via `use crate::auth::GoogleAuth;`
// sans se soucier de l'organisation interne des sous-modules.

pub use self::google_auth::GoogleAuth;
pub use self::oauth2::{GoogleTokens, OAuthAppCredentials, TokenStatus};
pub use storage::{EncryptedFileStorage, TokenStorage};
