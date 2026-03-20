use std::path::PathBuf;
use tokio::sync::mpsc;
use anyhow::Result;
use tracing::{info, warn};

use crate::db::Database;
use crate::engine::Task;

/// Vide la file d'attente hors-ligne et réinjecte les tâches dans le circuit principal
pub(crate) async fn flush_queue(db: &Database, task_tx: &mpsc::Sender<Task>) -> Result<()> {
    let tasks = db.get_offline_tasks()?;

    if tasks.is_empty() {
        return Ok(());
    }

    info!("🌐 Connexion rétablie ! Vidage de la file d'attente hors-ligne ({} tâches)...", tasks.len());

    for ot in tasks {
        let task = match ot.action.as_str() {
            "sync" => Task::SyncFile { path: PathBuf::from(&ot.relative_path) },
            "delete" => Task::Delete(PathBuf::from(&ot.relative_path)),
            "rename" => Task::Rename {
                from: PathBuf::from(ot.extra.clone().unwrap_or_default()),
                to: PathBuf::from(&ot.relative_path),
            },
            _ => {
                warn!("Action hors-ligne inconnue : {}", ot.action);
                continue;
            }
        };

        // On envoie la tâche rattrapée aux workers
        if task_tx.send(task).await.is_err() {
            warn!("Canal des tâches fermé pendant le flush hors-ligne.");
            break;
        }

        // On supprime la tâche de l'estomac SQLite une fois envoyée
        db.remove_offline_task(ot.id)?;
    }

    info!("✅ File d'attente hors-ligne traitée avec succès.");
    Ok(())
}