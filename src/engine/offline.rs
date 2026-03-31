//! Gestion du mode Survie (Hors-ligne) pour SyncGDrive.
//!
//! Ce module prend le relais lorsque la connexion à l'API Google Drive est perdue.
//! Il permet d'accumuler les événements locaux dans une file d'attente persistante (SQLite)
//! et se charge de les réinjecter dans le moteur principal une fois le réseau rétabli.

use anyhow::Result;
use std::path::Path;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::db::Database;
use crate::engine::Task;

/// Vide la file d'attente hors-ligne et réinjecte les tâches dans le circuit principal.
///
/// Cette fonction est appelée automatiquement par l'orchestrateur (`SyncEngine`)
/// lorsque le `health_check` détecte le retour de la connexion Internet.
///
/// # Mécanique interne
/// 1. Lit toutes les tâches en attente dans la table SQLite dédiée.
/// 2. Reconstruit les chemins locaux absolus (la DB stocke des chemins relatifs pour la portabilité).
/// 3. Convertit les enregistrements textuels en énumérations `Task` fortement typées.
/// 4. Envoie chaque tâche au canal des workers (`task_tx`).
/// 5. Supprime la tâche de la base SQLite uniquement si l'envoi au canal a réussi.
pub(crate) async fn flush_queue(
    db: &Database,
    task_tx: &mpsc::Sender<Task>,
    local_root: &Path,
) -> Result<()> {
    let tasks = db.get_offline_tasks()?;

    if tasks.is_empty() {
        return Ok(());
    }

    info!(
        "🌐 Connexion rétablie ! Vidage de la file d'attente hors-ligne ({} tâches)...",
        tasks.len()
    );

    for ot in tasks {
        let task = match ot.action.as_str() {
            // Reconstitution du chemin absolu à partir du chemin relatif stocké
            "sync" => Task::SyncFile {
                path: local_root.join(&ot.relative_path),
            },
            "delete" => Task::Delete(local_root.join(&ot.relative_path)),
            "rename" => Task::Rename {
                from: local_root.join(ot.extra.clone().unwrap_or_default()),
                to: local_root.join(&ot.relative_path),
            },
            _ => {
                warn!("Action hors-ligne inconnue ignorée : {}", ot.action);
                continue;
            }
        };

        // Transfert de la tâche rattrapée aux workers asynchrones
        if task_tx.send(task).await.is_err() {
            warn!("Canal des tâches fermé pendant le flush hors-ligne. Interruption.");
            break;
        }

        // Suppression sécurisée de l'estomac SQLite (exécuté 1 par 1 pour éviter
        // les pertes en cas de crash en plein milieu de la boucle)
        db.remove_offline_task(ot.id)?;
    }

    info!("✅ File d'attente hors-ligne traitée avec succès.");
    Ok(())
}
