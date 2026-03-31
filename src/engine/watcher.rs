//! Surveillance en temps réel du système de fichiers local.
//!
//! Ce module agit comme les "yeux" du moteur de synchronisation. Il utilise la bibliothèque
//! native de l'OS (Inotify sur Linux, FSEvents sur macOS, etc.) pour détecter instantanément
//! toute modification locale et la remonter au dispatcheur.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use notify::{
    event::{ModifyKind, RenameMode},
    Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher as NotifyWatcher,
};
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// Événement de système de fichiers simplifié pour le moteur SyncGDrive.
///
/// Cette énumération abstrait la complexité des événements bas niveau de l'OS
/// pour ne conserver que les trois actions fondamentales de la synchronisation.
#[derive(Debug)]
pub enum WatchEvent {
    /// Un fichier a été créé ou son contenu a été modifié.
    Modified(PathBuf),
    /// Un fichier a été supprimé ou déplacé hors du dossier surveillé.
    Deleted(PathBuf),
    /// Un fichier a été renommé ou déplacé à l'intérieur du dossier surveillé.
    Renamed { from: PathBuf, to: PathBuf },
}

/// Superviseur des événements locaux.
///
/// Maintient le thread d'écoute actif et gère la protection contre les rafales
/// d'événements (bursts) pour éviter de saturer la file d'attente.
pub struct Watcher {
    /// L'instance du watcher natif de l'OS. Maintenue en vie tant que `Watcher` existe.
    _watcher: RecommendedWatcher,

    /// Drapeau (flag) atomique levé lorsque le canal de communication est plein.
    /// Signale au moteur qu'il a potentiellement manqué des événements.
    overflow: Arc<AtomicBool>,
}

impl Watcher {
    /// Démarre la surveillance récursive d'un dossier racine.
    ///
    /// Lance un thread en arrière-plan fourni par `notify`. Les événements sont filtrés,
    /// traduits en `WatchEvent`, puis poussés de manière non-bloquante (`try_send`) dans le canal.
    pub fn start(root: &Path, tx: mpsc::Sender<WatchEvent>) -> Result<Self> {
        let overflow = Arc::new(AtomicBool::new(false));
        let overflow_cb = overflow.clone();

        let root = root.to_path_buf();

        let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
            let event = match res {
                Ok(e) => e,
                Err(e) => {
                    warn!(error = %e, "inotify error");
                    return;
                }
            };

            // Traduction experte des événements natifs de l'OS vers le modèle SyncGDrive
            let ev = match event.kind {
                // Fichier fermé après écriture — le signal le plus fiable pour déclencher un upload.
                EventKind::Access(notify::event::AccessKind::Close(
                                      notify::event::AccessMode::Write,
                                  )) => event.paths.into_iter().next().map(WatchEvent::Modified),

                // Création d'un nouveau fichier
                EventKind::Create(_) => event.paths.into_iter().next().map(WatchEvent::Modified),

                // Suppression pure et simple
                EventKind::Remove(_) => event.paths.into_iter().next().map(WatchEvent::Deleted),

                // Renommage atomique (both = from + to fournis dans le même événement natif)
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
                // un autre dossier hors surveillance). inotify ne voit que l'arrivée.
                // → Traiter comme une création/modification.
                EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
                    event.paths.into_iter().next().map(WatchEvent::Modified)
                }

                // Modification directe du contenu (souvent très bruyant, lissé par le debounce en aval)
                EventKind::Modify(ModifyKind::Data(_)) => {
                    event.paths.into_iter().next().map(WatchEvent::Modified)
                }

                _ => None,
            };

            if let Some(ev) = ev {
                debug!(?ev, "inotify event");

                // try_send : non-bloquant. Si le channel est plein (burst de
                // milliers d'événements, CPU à 100%…), on pose le flag overflow
                // au lieu de bloquer le thread inotify du noyau OS.
                if tx.try_send(ev).is_err() {
                    // swap : garantit un seul WARN même si 10 000 events sont droppés d'un coup.
                    if !overflow_cb.swap(true, Ordering::Relaxed) {
                        warn!("watcher: channel plein, événements perdus — un rescan de sécurité sera nécessaire");
                    }
                }
            }
        })
            .context("Échec de la création du watcher inotify/fsevents")?;

        watcher
            .watch(&root, RecursiveMode::Recursive)
            .with_context(|| format!("Impossible d'écouter le dossier {}", root.display()))?;

        Ok(Self {
            _watcher: watcher,
            overflow,
        })
    }

    /// Vérifie si des événements locaux ont été perdus (canal saturé) depuis le dernier appel.
    ///
    /// Réinitialise automatiquement le drapeau (flag) de manière atomique lors de la lecture.
    /// Très utile pour déclencher un rescan de rattrapage global dans le moteur principal.
    pub fn take_overflow(&self) -> bool {
        self.overflow.swap(false, Ordering::Relaxed)
    }

    /// Arrête explicitement la surveillance.
    ///
    /// Ferme la connexion avec l'API système (Inotify/FSEvents) en libérant le watcher natif.
    pub fn stop(self) {
        drop(self._watcher);
    }
}
