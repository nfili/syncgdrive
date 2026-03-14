use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher as NotifyWatcher,
             event::{ModifyKind, RenameMode}};
use tokio::sync::mpsc;
use tracing::{debug, warn};

#[derive(Debug)]
pub enum WatchEvent {
    Modified(PathBuf),
    Deleted(PathBuf),
    Renamed { from: PathBuf, to: PathBuf },
}

pub struct Watcher {
    _watcher: RecommendedWatcher,
    /// Flag posé par le callback inotify quand `try_send` échoue (channel plein).
    overflow: Arc<AtomicBool>,
}

impl Watcher {
    pub fn start(root: &Path, tx: mpsc::Sender<WatchEvent>) -> Result<Self> {
        let overflow = Arc::new(AtomicBool::new(false));
        let overflow_cb = overflow.clone();

        let root = root.to_path_buf();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
            let event = match res {
                Ok(e)  => e,
                Err(e) => { warn!(error = %e, "inotify error"); return; }
            };

            let ev = match event.kind {
                // Fichier fermé après écriture — le plus fiable pour déclencher la sync.
                EventKind::Access(notify::event::AccessKind::Close(
                    notify::event::AccessMode::Write
                )) => {
                    event.paths.into_iter().next().map(WatchEvent::Modified)
                }

                // Création de fichier
                EventKind::Create(_) => {
                    event.paths.into_iter().next().map(WatchEvent::Modified)
                }

                // Suppression
                EventKind::Remove(_) => {
                    event.paths.into_iter().next().map(WatchEvent::Deleted)
                }

                // Renommage atomique (both = from+to dans le même event)
                EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
                    let mut it = event.paths.into_iter();
                    match (it.next(), it.next()) {
                        (Some(from), Some(to)) => Some(WatchEvent::Renamed { from, to }),
                        _ => None,
                    }
                }

                // Fichier qui QUITTE l'arbre surveillé (ex: mis à la corbeille).
                // inotify ne voit que le départ, pas la destination (hors scope).
                // → Traiter comme une suppression.
                EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
                    event.paths.into_iter().next().map(WatchEvent::Deleted)
                }

                // Fichier qui ARRIVE dans l'arbre depuis l'extérieur (ex: mv depuis
                // un autre dossier). inotify ne voit que l'arrivée.
                // → Traiter comme une création/modification.
                EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
                    event.paths.into_iter().next().map(WatchEvent::Modified)
                }

                // Modification de contenu
                EventKind::Modify(ModifyKind::Data(_)) => {
                    event.paths.into_iter().next().map(WatchEvent::Modified)
                }

                _ => None,
            };

            if let Some(ev) = ev {
                debug!(?ev, "inotify event");
                // try_send : non-bloquant. Si le channel est plein (burst de
                // milliers d'événements, CPU à 100%…), on pose le flag overflow
                // au lieu de bloquer le thread inotify du noyau.
                // Le moteur déclenchera un rescan de rattrapage ~30s plus tard.
                if tx.try_send(ev).is_err() {
                    // swap : un seul WARN même si 10 000 events sont droppés.
                    if !overflow_cb.swap(true, Ordering::Relaxed) {
                        warn!("watcher: channel plein, événements perdus — rescan sera déclenché");
                    }
                }
            }
        })
        .context("cannot create inotify watcher")?;

        watcher
            .watch(&root, RecursiveMode::Recursive)
            .with_context(|| format!("cannot watch {}", root.display()))?;

        Ok(Self { _watcher: watcher, overflow })
    }

    /// Retourne `true` si des événements ont été perdus (channel plein)
    /// depuis le dernier appel. Réinitialise le flag atomiquement.
    pub fn take_overflow(&self) -> bool {
        self.overflow.swap(false, Ordering::Relaxed)
    }

    /// Arrête la surveillance (drop du watcher notify).
    pub fn stop(self) {
        drop(self._watcher);
    }
}

