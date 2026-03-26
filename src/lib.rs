//! # SyncGDrive — Synchronisation unidirectionnelle local → Google Drive
//!
//! Daemon de synchronisation asynchrone hautement optimisé qui réplique un dossier
//! local vers Google Drive. L'ordinateur local est la **source de vérité stricte** ;
//! le distant est un miroir de sauvegarde.
//!
//! ## Architecture V2 (Native API REST)
//!
//! ```text
//! ┌────────────────────────────────────────────────────────────┐
//! │                     main.rs (Tokio)                        │
//! │  PID file · flock · self-pipe POSIX · signal handler       │
//! ├────────────┬───────────────────┬───────────────────────────┤
//! │ SyncEngine │   UI (feature)    │       Logging             │
//! │ scan.rs    │   tray.rs (ksni)  │ tracing dual stdout+file  │
//! │ watcher.rs │   settings.rs     │ rotation quotidienne 7j   │
//! │ worker.rs  │   (GTK4/adw)      │                           │
//! ├────────────┴───────────────────┴───────────────────────────┤
//! │          Couche Réseau Google Drive (reqwest + OAuth2)     │
//! │  API v3 REST · Upload Streaming Resumable · Quotas & 429   │
//! ├────────────────────────────────────────────────────────────┤
//! │  config.rs  ·  db.rs (SQLite WAL)  ·  ignore.rs (globset)  │
//! │  notif.rs (notify-rust D-Bus)  ·  auth/ (PKCE OAuth2)      │
//! └────────────────────────────────────────────────────────────┘
//! ```
//!
//! ## Modules Principaux
//!
//! | Module | Rôle |
//! |--------|------|
//! | [`auth`] | Moteur d'authentification OAuth2 (PKCE) et stockage sécurisé AES-GCM. |
//! | [`config`] | Configuration TOML (`AppConfig`), validation, chemins XDG. |
//! | [`db`] | Base de données SQLite WAL — index fichiers, cache dossiers, file hors-ligne. |
//! | [`engine`] | Moteur de synchronisation : BFS scan, inotify watcher, workers asynchrones. |
//! | [`ignore`] | Filtrage ultra-rapide par patterns glob (exclusions) via `globset`. |
//! | [`migration`] | Orchestrateur de mise à jour transparente V1 → V2. |
//! | [`remote`] | Implémentation du fournisseur cloud Google Drive (Uploads par blocs, etc.). |
//! | [`ui`] | Interface système ksni + fenêtre Settings GTK4/libadwaita *(feature `ui`)*. |
//! | [`utils`] | Fonctions transverses (formatage d'affichage, appels OS). |
//!
//! ## Flux de données asynchrone
//!
//! 1. `AppConfig` est chargé et validé depuis `~/.config/syncgdrive/config.toml`.
//! 2. `Database` (SQLite) est ouverte en mode WAL pour permettre les écritures concurrentes.
//! 3. **Scan :** Analyse locale (`WalkDir`) + Analyse distante en largeur (`BFS`) → Différence → Tâches.
//! 4. **Watcher :** Écoute les événements inotify → Filtre les bruits → Injecte dans le channel `mpsc`.
//! 5. **Workers :** Bornés par un sémaphore, ils consomment les tâches et exécutent les requêtes REST HTTP.
//! 6. **Résilience :** En cas de coupure, les tâches basculent dans la `offline_queue` SQLite.

pub mod auth;
pub mod config;
pub mod db;
pub mod engine;
pub mod ignore;
pub mod migration;
pub mod notif;
pub mod remote;
#[cfg(feature = "ui")]
pub mod ui;
pub mod utils;