//! # SyncGDrive — Synchronisation unidirectionnelle local → distant
//!
//! Daemon de synchronisation qui réplique un dossier local vers Google Drive
//! (ou tout backend KIO : SMB, SFTP, WebDAV, FTP). L'ordinateur local est la
//! **source de vérité** ; le distant est une sauvegarde.
//!
//! ## Architecture
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
//! │            Couche d'abstraction (trait KioOps)              │
//! │  V1: KioClient (kioclient5)  ·  V2: API REST native        │
//! ├────────────────────────────────────────────────────────────┤
//! │  config.rs  ·  db.rs (SQLite WAL)  ·  ignore.rs (globset)  │
//! │  notif.rs (notify-rust D-Bus)  ·  kio.rs                   │
//! └────────────────────────────────────────────────────────────┘
//! ```
//!
//! ## Modules
//!
//! | Module | Rôle |
//! |--------|------|
//! | [`config`] | Configuration TOML (`AppConfig`), validation, chemins XDG |
//! | [`db`] | Base de données SQLite WAL — index fichiers + cache dossiers |
//! | [`engine`] | Moteur de synchronisation : scan, watcher inotify, workers |
//! | [`ignore`] | Filtrage par patterns glob (exclusions) |
//! | [`kio`] | Abstraction des opérations distantes (`trait KioOps`) |
//! | [`notif`] | Notifications bureau D-Bus (politique de silence) |
//! | [`ui`] | Interface systray ksni + fenêtre Settings GTK4/libadwaita *(feature `ui`)* |
//!
//! ## Feature gates
//!
//! | Feature | Effet |
//! |---------|-------|
//! | `ui` | Active GTK4, libadwaita et ksni (systray + fenêtre Settings) |
//! | *(aucune)* | Mode headless — moteur seul, sans interface graphique |
//!
//! ## Flux de données
//!
//! 1. `AppConfig` chargé depuis `~/.config/syncgdrive/config.toml`
//! 2. `Database` (SQLite WAL) à `~/.local/share/syncgdrive/index.db`
//! 3. Scan : `WalkDir` local + `kioclient5 ls` BFS distant → diff avec `file_index` DB → `Task::SyncFile`
//! 4. Watcher : événements inotify → `WatchEvent` → `Task` via channel mpsc
//! 5. Workers : bornés par sémaphore (`max_workers`), exécutent `kioclient5 copy/rm/move`
//! 6. Logs : rotation quotidienne à `~/.local/state/syncgdrive/logs/`, rétention 7 jours
//!
//! ## Exemple de configuration
//!
//! ```toml
//! local_root = "/home/user/Projets"
//! remote_root = "gdrive:/MonDrive/Backup"
//! max_workers = 4
//! notifications = true
//!
//! [retry]
//! max_attempts = 3
//! initial_backoff_ms = 300
//! max_backoff_ms = 8000
//!
//! ignore_patterns = [
//!     "**/target/**",
//!     "**/.git/**",
//!     "**/node_modules/**",
//! ]
//! ```

pub mod config;
pub mod db;
pub mod engine;
pub mod ignore;
pub mod kio;
pub mod notif;
pub mod migration;
#[cfg(feature = "ui")]
pub mod ui;
