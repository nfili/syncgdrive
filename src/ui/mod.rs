//! Interface systray + fenêtre Settings.
//!
//! # Architecture des threads
//!
//! ```text
//! Runtime Tokio (multi-thread)
//!   ├─ task ksni      : D-Bus StatusNotifierItem (async, pas de runtime imbriqué)
//!   ├─ task engine    : moteur de synchronisation
//!   └─ task status    : dispatch EngineStatus → handle.update() → D-Bus
//!
//! Thread OS « gtk-ui » (unique et persistant via OnceLock + std::sync::mpsc)
//!   ├─ libadwaita::init() — appelé UNE SEULE FOIS au démarrage du thread
//!   ├─ GtkAction::OpenSettings → settings::run_standalone() (Pause/Resume)
//!   └─ GtkAction::ShowAbout   → run_about_app() (AboutWindow)
//! ```
//!
//! GTK4 exige que toute l'UI vive sur un seul thread OS. Le thread `gtk-ui`
//! est créé au premier besoin par [`tray::ensure_gtk_thread()`] et réutilisé
//! pour toutes les fenêtres (Settings, À propos). La communication se fait
//! par envoi de [`tray::GtkAction`] dans le canal `std::sync::mpsc`.

pub mod settings;
pub mod tray;

pub use tray::spawn_tray;
