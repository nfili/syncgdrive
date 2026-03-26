//! Notifications bureau via notify-rust.
//!
//! Stratégie UX (UX_SYSTRAY.md) :
//! - **Erreurs uniquement** : seules les erreurs fatales déclenchent un pop-up.
//! - **Événements courants** (scan, transfert, pause) : affichés via le tooltip
//!   de la systray, PAS via des pop-ups.
//!
//! Les fonctions non-error sont conservées (API stable) mais sont des no-ops (ne font rien).

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
///
/// Affiche un pop-up rassurant qui se ferme automatiquement après le délai configuré (par défaut 6 secondes).
pub fn initial_sync_complete(cfg: &AppConfig) {
    send(
        cfg,
        "SyncGDrive — Synchronisation terminée ✓",
        "Le dossier est à jour.\nSurveillance active, vous pouvez travailler en toute sécurité.",
        "emblem-ok-symbolic",
        Urgency::Normal,
        cfg.advanced.notification_timeout_ms,
    );
}

/// Erreur fatale (auth, chemin, quota…).
///
/// La notification est marquée "Critical" avec un timeout de `0`, ce qui la rend "sticky"
/// (elle reste affichée à l'écran tant que l'utilisateur ne la ferme pas manuellement).
pub fn error(cfg: &AppConfig, message: &str) {
    send(
        cfg,
        "SyncGDrive — Action requise ⚠",
        message,
        "dialog-error",
        Urgency::Critical,
        0,
    );
}

/// Dossier local surveillé introuvable (§4B UX_SYSTRAY.md).
///
/// Avertit l'utilisateur si le dossier racine a été déplacé ou supprimé.
pub fn folder_missing(cfg: &AppConfig, path: &str) {
    send(
        cfg,
        "SyncGDrive — Dossier introuvable",
        &format!("Le dossier surveillé « {path} » a été renommé ou supprimé.\nMoteur en pause."),
        "folder-open",
        Urgency::Critical,
        0,
    );
}

/// Quota Google Drive ou disque plein (§4B UX_SYSTRAY.md).
///
/// Alerte critique sticky pour stopper l'utilisation du dossier en attendant de faire de la place.
pub fn quota_exceeded(cfg: &AppConfig) {
    send(
        cfg,
        "SyncGDrive — Espace insuffisant",
        "Quota Google Drive ou disque local plein.\nTransferts suspendus.",
        "drive-harddisk",
        Urgency::Critical,
        0,
    );
}

/// Réseau retrouvé après coupure (Phase 6).
///
/// Pop-up bureau pour rassurer l'utilisateur sur la reprise des transferts.
pub fn connection_restored(cfg: &AppConfig) {
    send(
        cfg,
        "SyncGDrive — Connexion rétablie 🌐",
        "Le réseau est de nouveau disponible.\nSynchronisation des modifications en attente...",
        "network-transmit-receive-symbolic",
        Urgency::Normal,
        cfg.advanced.notification_timeout_ms,
    );
}

/// Connexion perdue, passage en mode survie (Phase 6).
///
/// Avertit que les actions sont désormais enregistrées dans la file d'attente (offline queue).
pub fn connection_lost(cfg: &AppConfig) {
    send(
        cfg,
        "SyncGDrive — Connexion perdue ⚠️",
        "Le réseau est indisponible. Passage en mode SURVIE.\nVos modifications sont sauvegardées en attente du retour en ligne.",
        "network-offline-symbolic",
        Urgency::Critical,
        0, // 0 = Reste affiché jusqu'au clic
    );
}

// ── Interne ───────────────────────────────────────────────────────────────

/// Helper centralisé pour l'envoi asynchrone des notifications D-Bus.
///
/// Gère la vérification des préférences utilisateur (`cfg.notifications`)
/// et isole l'appel réseau D-Bus dans un thread séparé pour éviter de bloquer
/// le runtime asynchrone Tokio (erreur "runtime within runtime" de zbus).
fn send(cfg: &AppConfig, summary: &str, body: &str, icon: &str, urgency: Urgency, timeout_ms: i32) {
    // OPTIMISATION DRY : Le contrôle des préférences se fait une seule fois ici !
    if !cfg.notifications {
        return;
    }

    let summary = summary.to_owned();
    let body = body.to_owned();
    let icon = icon.to_owned();

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