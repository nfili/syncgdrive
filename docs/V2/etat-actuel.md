# État Actuel du Projet : SyncGDrive (Post-Phase 1)

## 1. Vue d'Ensemble
SyncGDrive est un daemon de synchronisation unidirectionnelle (Local → Distant) écrit en Rust. L'ordinateur local est la source de vérité, le distant sert de sauvegarde.
La **Phase 1** a permis de refondre les fondations (Configuration et Base de données) pour supporter le multi-sync et de nettoyer intégralement la dette technique (zéro warning, typage strict de l'interface graphique GTK4).

## 2. Architecture des Modules

L'application est découpée en couches strictes pour garantir la modularité, en particulier l'isolation du moteur de transfert (`KioOps`).

* **Core & Entrypoint** (`main.rs`, `lib.rs`) :
    * Gestion de l'instance unique via file lock POSIX (`flock`).
    * Interception des signaux OS (`SIGINT`, `SIGTERM`) via self-pipe pour un arrêt propre (`CancellationToken`).
    * Initialisation du logging avec rotation quotidienne (`tracing`).

* **Configuration & Migration** (`config.rs`, `migration.rs`) :
    * Structure V2 robuste (`AppConfig` contenant un vecteur de `SyncPair`).
    * Paramètres avancés extraits du code et configurables (timeouts, debounce, workers).
    * Orchestrateur de migration : transition transparente des fichiers `config.toml` de la V1 vers la V2 sans perte de données.

* **Base de Données** (`db.rs`) :
    * SQLite en mode WAL pour des performances concurrentes élevées.
    * `schema_version` implémenté pour les migrations futures.
    * Tables préparatoires pour les phases suivantes : `path_cache` (Phase 3) et `offline_queue` (Phase 6).

* **Moteur de Synchronisation** (`engine/`) :
    * `mod.rs` : Orchestrateur asynchrone (Tokio) gérant l'état global (`EngineStatus`) et le cycle de vie.
    * `scan.rs` : Algorithme de synchronisation initiale en 6 étapes (Listing BFS distant, inventaire local, comparaison DB, envoi, nettoyage des orphelins).
    * `watcher.rs` : Surveillance temps réel via `inotify` (Linux) avec gestion de l'overflow (bursts d'événements).
    * `worker.rs` : Exécution des tâches (`SyncFile`, `Delete`, `Rename`) bornée par un sémaphore asynchrone.

* **Interface Utilisateur** (`ui/`) :
    * `tray.rs` : Menu systray géré par `ksni` (D-Bus StatusNotifierItem) s'exécutant sur la boucle asynchrone sans bloquer le moteur.
    * `settings.rs` : Fenêtre de paramètres native GTK4/libadwaita tournant sur un thread OS dédié (`gtk-ui`), communiquant par channels avec le moteur.
    * `notif.rs` : Notifications bureau via `notify-rust` (réservées aux erreurs critiques pour respecter la politique de silence).

* **Couche d'Abstraction Réseau** (`kio.rs`) :
    * Définition du trait `KioOps` qui standardise les actions requises par le moteur (`ls_remote`, `mkdir_p`, `copy_file`, `delete`, `rename`).
    * Implémentation actuelle (V1) : `KioClient`, un wrapper exécutant le binaire KDE `kioclient5` via des sous-processus.

## 3. Limites Actuelles (Objectifs de la Phase 2)

Bien que le socle applicatif soit désormais parfait, l'implémentation réseau actuelle (`KioClient`) présente des limites inhérentes à KDE :
* **Dépendance externe forte :** Nécessite l'écosystème KDE Frameworks d'Arch Linux (`kio`, `kaccounts`).
* **Boîte noire :** Le traitement des erreurs se base sur le parsing de chaînes de caractères (ex: `kioclient5` crache des erreurs 404, des "tokens expirés" ou des `exit=1` génériques).
* **Performances bridées :** Chaque transfert "fork" un nouveau sous-processus système au lieu de maintenir un pool de connexions HTTP Keep-Alive.

## 4. Préparation à la Phase 2 (API REST Native)

La transition vers la Phase 2 est sécurisée par l'architecture en place :
1. Le moteur de synchronisation n'appelle **jamais** `kioclient5` directement, il consomme uniquement le trait `KioOps`.
2. Pour basculer sur l'API native, il suffira de :
    * Créer un nouveau module `gdrive.rs` implémentant `KioOps`.
    * Gérer l'authentification OAuth2 (Device Flow ou Loopback) dans la configuration ou l'interface.
    * Utiliser un client HTTP (`reqwest`) pour exécuter les opérations via l'API Google Drive v3 de manière 100% asynchrone.