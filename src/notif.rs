//! Notifications bureau via notify-rust.
//!
//! Stratégie UX (UX_SYSTRAY.md) :
//! - **Erreurs uniquement** : seules les erreurs fatales déclenchent un pop-up.
//! - **Événements courants** (scan, transfert, pause) : affichés via le tooltip
//!   de la systray, PAS via des pop-ups.
//!
//! Les fonctions non-error sont conservées (API stable) mais sont des no-ops.

use notify_rust::{Notification, Urgency};

use crate::config::AppConfig;

// ── Événements silencieux (tooltip systray uniquement) ─────────────────────

/// Scan initial démarré — silencieux (tooltip uniquement).
pub fn scan_started(_cfg: &AppConfig) {}

/// Progression dossiers — silencieux.
pub fn scan_dirs_progress(_cfg: &AppConfig, _done: usize, _total: usize) {}

/// Scan terminé — silencieux.
pub fn scan_complete(_cfg: &AppConfig, _dirs: usize, _to_sync: usize, _skipped: usize) {}

/// Progression fichier — silencieux.
pub fn sync_progress(_cfg: &AppConfig, _done: usize, _total: usize, _name: &str, _size: u64) {}

/// Sync initiale terminée — silencieux.
pub fn sync_complete(_cfg: &AppConfig, _total: usize) {}

/// Fichier individuel synchronisé — silencieux.
pub fn file_synced(_cfg: &AppConfig, _name: &str) {}

/// Moteur en pause — silencieux.
pub fn paused(_cfg: &AppConfig) {}

/// Moteur repris — silencieux.
pub fn resumed(_cfg: &AppConfig) {}

// ── Notifications actives (pop-up bureau) ─────────────────────────────────

/// Synchronisation initiale terminée (§4A UX_SYSTRAY.md).
/// Pop-up auto-dismiss après 6 secondes.
pub fn initial_sync_complete(cfg: &AppConfig) {
    if !cfg.notifications { return; }
    send(
        "SyncGDrive — Synchronisation terminée ✓",
        "Le dossier est à jour.\nSurveillance active, vous pouvez travailler en toute sécurité.",
        "emblem-ok-symbolic",
        Urgency::Normal,
        6000,
    );
}

/// Erreur fatale (auth, chemin, quota…).
/// Reste à l'écran jusqu'à fermeture manuelle (sticky).
pub fn error(cfg: &AppConfig, message: &str) {
    if !cfg.notifications { return; }
    send(
        "SyncGDrive — Action requise ⚠",
        message,
        "dialog-error",
        Urgency::Critical,
        0, // timeout 0 = sticky (reste jusqu'à fermeture)
    );
}

/// Dossier local surveillé introuvable (§4B UX_SYSTRAY.md).
/// Sticky : reste jusqu'à fermeture manuelle.
pub fn folder_missing(cfg: &AppConfig, path: &str) {
    if !cfg.notifications { return; }
    send(
        "SyncGDrive — Dossier introuvable",
        &format!("Le dossier surveillé « {path} » a été renommé ou supprimé.\nMoteur en pause."),
        "folder-open",
        Urgency::Critical,
        0,
    );
}

/// Quota Google Drive ou disque plein (§4B UX_SYSTRAY.md).
/// Sticky : reste jusqu'à fermeture manuelle.
pub fn quota_exceeded(cfg: &AppConfig) {
    if !cfg.notifications { return; }
    send(
        "SyncGDrive — Espace insuffisant",
        "Quota Google Drive ou disque local plein.\nTransferts suspendus.",
        "drive-harddisk",
        Urgency::Critical,
        0,
    );
}

// ── Interne ───────────────────────────────────────────────────────────────

fn send(summary: &str, body: &str, icon: &str, urgency: Urgency, timeout_ms: i32) {
    // notify-rust 4.x appelle zbus::block_on() en interne dans show().
    // Si on est sur un worker Tokio, block_on panic ("runtime within runtime").
    // Solution : envoyer la notification depuis un thread OS séparé (pas de
    // contexte Tokio → block_on fonctionne normalement).
    let summary = summary.to_owned();
    let body    = body.to_owned();
    let icon    = icon.to_owned();
    std::thread::spawn(move || {
        if let Err(e) = Notification::new()
            .appname("SyncGDrive")
            .summary(&summary)
            .body(&body)
            .icon(&icon)
            .urgency(urgency)
            .timeout(timeout_ms)
            .show()
        {
            tracing::debug!(error = %e, "notification send failed (pas de serveur D-Bus ?)");
        }
    });
}
