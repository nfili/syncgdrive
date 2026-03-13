//! Notifications bureau via notify-rust.
//!
//! Toutes les fonctions vérifient `cfg.notifications` avant d'envoyer.
//! Si la notification échoue (pas de serveur D-Bus, etc.), on log un warning
//! mais on ne plante pas.

use notify_rust::{Notification, Urgency};

use crate::config::AppConfig;

/// Notification de démarrage du scan initial.
pub fn scan_started(cfg: &AppConfig) {
    if !cfg.notifications { return; }
    send(
        "SyncGDrive — Scan initial",
        &format!(
            "Inventaire de <b>{}</b> en cours…\nVeuillez patienter.",
            cfg.local_root.display()
        ),
        "emblem-synchronizing",
        Urgency::Low,
    );
}

/// Progression du scan (phase dossiers).
pub fn scan_dirs_progress(cfg: &AppConfig, done: usize, total: usize) {
    if !cfg.notifications { return; }
    send(
        "SyncGDrive — Création des dossiers",
        &format!("Dossier {done}/{total} sur le Drive…"),
        "folder-new",
        Urgency::Low,
    );
}

/// Scan terminé — résumé.
pub fn scan_complete(cfg: &AppConfig, dirs: usize, to_sync: usize, skipped: usize) {
    if !cfg.notifications { return; }
    send(
        "SyncGDrive — Scan terminé ✓",
        &format!(
            "<b>{dirs}</b> dossiers, <b>{to_sync}</b> fichiers à synchroniser, <b>{skipped}</b> déjà à jour.\n\
             Vous pouvez maintenant travailler sur <b>{}</b>.",
            cfg.local_root.display()
        ),
        "emblem-default",
        Urgency::Normal,
    );
}

/// Progression de la synchronisation fichier par fichier.
pub fn sync_progress(cfg: &AppConfig, done: usize, total: usize, name: &str, size: u64) {
    if !cfg.notifications { return; }
    let size_str = human_size(size);
    send(
        &format!("SyncGDrive — {done}/{total}"),
        &format!("<b>{name}</b> ({size_str})"),
        "emblem-synchronizing",
        Urgency::Low,
    );
}

/// Synchronisation initiale terminée.
pub fn sync_complete(cfg: &AppConfig, total: usize) {
    if !cfg.notifications { return; }
    send(
        "SyncGDrive — Synchronisation terminée ✓",
        &format!("{total} fichier(s) transférés vers le Drive."),
        "emblem-default",
        Urgency::Normal,
    );
}

/// Un fichier a été modifié et re-synchronisé (watcher).
pub fn file_synced(cfg: &AppConfig, name: &str) {
    if !cfg.notifications { return; }
    send(
        "SyncGDrive",
        &format!("↑ <b>{name}</b> synchronisé"),
        "emblem-synchronizing",
        Urgency::Low,
    );
}

/// Erreur fatale (auth, etc.).
pub fn error(cfg: &AppConfig, message: &str) {
    if !cfg.notifications { return; }
    send(
        "SyncGDrive — Erreur ⚠",
        message,
        "dialog-error",
        Urgency::Critical,
    );
}

/// Moteur en pause (réglages ouverts).
pub fn paused(cfg: &AppConfig) {
    if !cfg.notifications { return; }
    send(
        "SyncGDrive — ⏸ En pause",
        "Fenêtre de réglages ouverte. La synchronisation reprendra à la fermeture.",
        "media-playback-pause",
        Urgency::Low,
    );
}

/// Moteur repris.
pub fn resumed(cfg: &AppConfig) {
    if !cfg.notifications { return; }
    send(
        "SyncGDrive — ▶ Reprise",
        "La synchronisation a repris.",
        "emblem-synchronizing",
        Urgency::Low,
    );
}

// ── Interne ───────────────────────────────────────────────────────────────────

fn send(summary: &str, body: &str, icon: &str, urgency: Urgency) {
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
            .timeout(4000)
            .show()
        {
            tracing::debug!(error = %e, "notification send failed (pas de serveur D-Bus ?)");
        }
    });
}

fn human_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if bytes >= GB {
        format!("{:.1} Go", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} Mo", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.0} Ko", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} o")
    }
}

