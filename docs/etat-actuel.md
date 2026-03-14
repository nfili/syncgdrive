# SyncGDrive — État actuel (2026-03-13)

## Architecture

```
src/
├── main.rs          # Orchestration : Tokio runtime + self-pipe POSIX (SIGINT/SIGTERM)
│                    #   + PID file + instance lock (flock) + dual logging (stdout + fichier)
├── lib.rs           # Modules publics (ui conditionnel via feature gate)
├── config.rs        # AppConfig TOML + validation + expand ~ + sauvegarde
│                    #   Chemins XDG : ~/.config/syncgdrive/config.toml
├── db.rs            # SQLite WAL (file_index : path, hash SHA-256, mtime)
│                    #   + dir_index : cache persistant des dossiers distants connus
│                    #   Arc<Mutex<Connection>> pour partage inter-tâches
├── ignore.rs        # IgnoreMatcher globset (trailing-slash fix pour dossiers)
├── kio.rs           # Wrapper kioclient5 (process_group, kill propre, trait KioOps)
│                    #   ls récursif BFS, mkdir_if_absent, copy_file_smart, copy_overwrite
├── notif.rs         # Notifications bureau (notify-rust) : silence par défaut
│                    #   Actives : initial_sync_complete, error, folder_missing, quota_exceeded
├── engine/
│   ├── mod.rs       # SyncEngine : boucle principale, Pause/Resume, status, run_unconfigured
│   │                #   Détection disparition local_root (toutes les 30s)
│   ├── scan.rs      # Scan initial 4 phases + remote index + anti-doublon + retry
│   │                #   is_fatal_kio_err (auth+quota), is_quota_err
│   ├── watcher.rs   # inotify (notify crate) : CloseWrite/Create/Delete/Rename
│   │                #   Overflow detection (try_send) + rescan fallback 30s
│   └── worker.rs    # Traitement Task : mtime→hash→upload, delete, rename + retry
└── ui/
    ├── mod.rs       # Déclarations de modules, re-export spawn_tray
    ├── tray.rs      # Systray ksni (StatusNotifierItem) + tooltip dynamique + menu 11 entrées
    │                #   + fenêtre À propos libadwaita + toggle autostart systemd
    └── settings.rs  # Fenêtre GTK4/libadwaita : chemins, exclusions, options + toast
dist/
└── syncgdrive.service  # Service systemd --user (référence)
```

## Fonctionnalités implémentées

### Moteur de synchronisation
- ✅ Scan initial en **6 phases** (vérifie `local = DB = remote`) :
  - Phase 0 : listing récursif du remote (BFS `ls` → `HashSet` = remote index)
  - Phase 1 : inventaire filesystem local (walkdir + filtre ignore + updates progressifs tous les 100 éléments)
  - Phase 2 : création des dossiers distants manquants (via `known_remote` + `dir_index` DB + `stat` anti-doublon)
  - Phase 3 : comparaison chaque fichier local ↔ DB (mtime rapide puis hash SHA-256) + enqueue
  - Phase 5 : suppression orphelins DB (fichiers en DB mais supprimés localement → delete remote)
  - Phase 6 : suppression orphelins remote (fichiers sur le Drive mais absents localement et de la DB → delete)
- ✅ Persistance SQLite WAL : fichiers déjà synchronisés (même hash) ne sont pas re-uploadés
- ✅ **Persistance des dossiers** (`dir_index`) : les dossiers distants connus sont enregistrés en DB. Au scan suivant, ils sont injectés dans `known_remote` → skip total (ni stat, ni mkdir)
- ✅ L'ordi local est la source de vérité, le Drive est la sauvegarde
- ✅ Watcher inotify temps réel (close_write, create, delete, rename both/from/to)
- ✅ **Debounce 500ms** : coalescence des événements Modified pour le même fichier (évite les uploads multiples lors d'un seul enregistrement)
- ✅ **RenameMode::From** : fichier mis à la corbeille (quitte l'arbre surveillé) → traité comme suppression
- ✅ **RenameMode::To** : fichier arrivé de l'extérieur → traité comme création
- ✅ **Rename .part/.tmp** : si source absente de la DB → fallback `sync_file(to)` au lieu de rename distant
- ✅ Watcher non-bloquant (`try_send`) : si le channel déborde (burst d'événements), un rescan de rattrapage se déclenche ~30s plus tard (stratégie Dropbox/OneDrive)
- ✅ Upload via `--overwrite copy` (écrase si existant, crée si nouveau)
- ✅ **Skip fichiers vides** (0 octet) : kioclient5 retourne exit=0 mais ne crée rien → ignorés jusqu'à obtention de contenu
- ✅ **Remote index** : un seul `ls` récursif BFS au démarrage, cache `known_remote` enrichi pendant le scan
- ✅ **Anti-doublon GDrive (triple protection)** :
  - Cache `known_remote` : chemins déjà connus (index distant + DB `dir_index` + créés pendant le scan) → skip immédiat
  - `dir_index` DB : cache persistant entre les runs — au relancement, les dossiers connus sont préchargés sans aucun appel réseau
  - `mkdir_if_absent` fait confiance au remote index BFS + DB (pas de `stat` avant `mkdir` — 1 seul appel KIO par dossier nouveau)
  - `mkdir_p` (watcher) utilise `stat` car pas d'index pré-construit
- ✅ **Écrasement fichiers** : `--overwrite copy` pour tous les fichiers (nouveaux et existants)
- ✅ Retry avec backoff exponentiel interruptible par shutdown
- ✅ Détection erreurs fatales KIO (auth/token/403/401/quota) → pas de retry inutile
- ✅ Détection spécifique erreurs quota (`is_quota_err`) → notification dédiée
- ✅ Gestion Pause/Resume (fenêtre Settings ouverte → moteur en pause)
- ✅ Rescan automatique après changement de config pendant la pause
- ✅ Hot-reload config : changement de `local_root` → clear DB (`file_index` + `dir_index`) + nouveau watcher + rescan
- ✅ **Détection disparition local_root** : vérification toutes les 30s, notification + pause
- ✅ **Rescan périodique** : toutes les N min (`rescan_interval_min`, défaut 30), vérifie `local = DB = remote` même sans événement inotify
- ✅ Shutdown propre : SIGINT/SIGTERM → CancellationToken → kill kioclient5 enfants
- ✅ Mode `run_unconfigured` : boucle d'attente de config valide (premier lancement)

### Progression fichiers (`SyncProgress`)
- ✅ **Compteurs atomiques** `total_queued` / `total_done` (`AtomicUsize`) dans la boucle principale
- ✅ `SyncProgress { done, total, current, size_bytes }` envoyé à chaque task reçue (nom + taille du fichier) et à chaque fin de worker (done incrémenté)
- ✅ Compteurs remis à 0 avant chaque nouveau scan (ForceScan, rescan config, overflow)
- ✅ Le tooltip affiche barre de progression, nom du fichier courant, poids, X/Y fichiers

### Trait KioOps (kio.rs)
- ✅ `ls_remote` : listing BFS récursif → `HashSet<String>` de tous les chemins distants
- ✅ `mkdir_p` : création récursive avec `stat` + retry (utilisé par le watcher)
- ✅ `mkdir_if_absent` : création via cache remote index, pas de `stat` (scan initial — confiance au BFS)
- ✅ `copy_file` : `stat` → si existe `cat` sinon `copy` + fallback `cat` (watcher)
- ✅ `copy_file_smart` : même logique via remote index (scan)
- ✅ `copy_overwrite` : `--overwrite copy` pour tous les fichiers (nouveaux et existants)
- ✅ `delete` : `rm` puis fallback `del`, tolérant si déjà supprimé
- ✅ `rename` : `move`, tolérant si déjà renommé
- ✅ `terminate_all` : SIGTERM sur tous les kioclient5 enfants en vol

### Interface utilisateur (feature `ui`)
- ✅ **Systray ksni** (`tray.rs`) : StatusNotifierItem D-Bus avec icônes dynamiques par état
  - 10 états mappés : Starting, Unconfigured, Idle, ScanProgress (4 phases), SyncProgress, Syncing, Paused, Error, Stopped
- ✅ **Rafraîchissement temps réel** : chaque changement de status appelle `handle.update()` qui réémet les propriétés D-Bus (icône, titre, tooltip)
- ✅ **Tooltip dynamique** au survol : barres de progression Unicode (`█░`), nom de fichier, métriques
  - LocalListing avec compteur progressif d'éléments indexés
  - Idle avec dernier fichier transféré
- ✅ **Menu contextuel 11 entrées** (dynamique selon l'état) :
  - État actuel (grisé), Sync/Pause/Resume (dynamique), Ouvrir dossier local (`xdg-open`),
    Ouvrir Google Drive (`kioclient5 exec`), Lancer au démarrage (systemctl toggle),
    Réglages…, Voir les logs, À propos, Quitter
- ✅ **Fenêtre À propos** (`adw::AboutWindow`) : version, licence, description, crédits
- ✅ **Thread GTK unique** (`gtk-ui`) : `OnceLock` + `std::sync::mpsc` — Settings et À propos s'exécutent séquentiellement sur le même thread OS (évite le panic « Attempted to initialize GTK from two different threads »)
- ✅ **Pause immédiate** : clic sur Réglages envoie `Pause` directement depuis le callback ksni (pas d'attente du thread GTK)
- ✅ **Toggle autostart** : `systemctl --user enable/disable syncgdrive.service`
- ✅ **Fenêtre Settings** libadwaita :
  - Chemins (local avec parcourir, remote)
  - Exclusions : liste éditable + parcourir multi-sélection + saisie glob manuelle
  - Options : workers parallèles (SpinRow 1–16), notifications bureau (SwitchRow)
  - Validation à l'enregistrement + toast d'erreur
  - **Validation live** : icônes ✅/❌ en temps réel sur les champs local et remote + bouton Enregistrer grisé si invalide
- ✅ Fenêtre Settings ouverte automatiquement au premier lancement ou config invalide

### Notifications bureau (notif.rs)
Politique de **silence par défaut** (UX_SYSTRAY.md §3–§4) :

| Fonction | Type | Comportement |
|---|---|---|
| `initial_sync_complete` | Succès | Pop-up auto-dismiss 6s (une seule fois) |
| `error` | Erreur fatale | Sticky (auth, token, chemin…) |
| `folder_missing` | Dossier perdu | Sticky + moteur en pause |
| `quota_exceeded` | Quota/espace | Sticky |
| `scan_started`, `scan_complete`, `sync_progress`, `file_synced`, `paused`, `resumed` | Silencieux | No-op (tooltip uniquement) |

### Filtrage (ignore)
- ✅ Glob patterns (`**/target/**`, `**/.git/**`, `**/node_modules/**`, etc.)
- ✅ Fix trailing-slash : les dossiers eux-mêmes sont correctement ignorés
- ✅ Hot-reload via ApplyConfig (nouveau `IgnoreMatcher` à chaque rescan)
- ✅ Tests unitaires : dossier ignoré, fichier interne ignoré, fichier externe non affecté

### Logging
- ✅ Dual logging via tracing-subscriber :
  - stdout : compact, sans target, timer UTC `HH:MM:SS`
  - fichier : `~/.local/state/syncgdrive/logs/syncgdrive.log.YYYY-MM-DD` (rolling daily, avec target)
- ✅ `EnvFilter` : défaut `info,zbus=warn,globset=warn,glib=warn`, surchargeable via `RUST_LOG`
- ✅ Non-blocking writer pour le fichier (guard retourné pour flush au shutdown)
- ✅ Rétention 7 jours (`cleanup_old_logs`)

### Robustesse système
- ✅ **Instance unique** : `flock` exclusif sur `$XDG_RUNTIME_DIR/syncgdrive.lock`
- ✅ **PID file** : PID écrit dans le lock file après acquisition (truncate + write)
- ✅ **Notification doublon** : si lock échoue → notification D-Bus (thread OS isolé) + exit(0)
- ✅ **notify-rust isolé** : appels D-Bus dans `std::thread::spawn` partout (notif.rs + main.rs)
- ✅ Pas de processus zombies (`process_group(0)` + kill propre via `libc::kill`)
- ✅ SQLite WAL (lecteurs ne bloquent pas les écrivains)
- ✅ Sémaphore pour limiter les workers parallèles (`max_workers`)
- ✅ Self-pipe trick POSIX pour les signaux (compatible ksni + GTK)
- ✅ `AsyncFd` sur le pipe signal pour intégration propre avec `tokio::select!`
- ✅ Timeout 3s au shutdown si le moteur ne finit pas proprement
- ✅ **Service systemd** : `dist/syncgdrive.service` (Type=simple, Restart=on-failure, graphical-session.target)
- ✅ Config validation : chemin local (existe, est dossier), protocole remote (gdrive, smb, sftp, webdav, ftp)

## EngineStatus (machine d'état)

```rust
pub enum EngineStatus {
    Starting,                                    // Démarrage (system-run-symbolic)
    Unconfigured(String),                        // Config invalide (dialog-warning)
    Idle,                                        // Surveillance active (emblem-ok-symbolic)
    ScanProgress { phase, done, total, current },// Scan en cours (icône par phase)
    SyncProgress { done, total, current, size }, // Transfert (emblem-synchronizing-symbolic)
    Syncing { active },                          // Workers actifs
    Paused,                                      // En pause (preferences-system-symbolic)
    Error(String),                               // Erreur (dialog-error)
    Stopped,                                     // Arrêté (system-shutdown-symbolic)
}
```

## Chemins XDG

| Donnée | Chemin |
|---|---|
| Config | `$XDG_CONFIG_HOME/syncgdrive/config.toml` (défaut `~/.config/`) |
| Base de données | `$XDG_DATA_HOME/syncgdrive/index.db` (défaut `~/.local/share/`) — tables `file_index` + `dir_index` |
| Logs | `$XDG_STATE_HOME/syncgdrive/logs/syncgdrive.log.*` (défaut `~/.local/state/`) |
| PID / Lock | `$XDG_RUNTIME_DIR/syncgdrive.lock` (défaut `/run/user/<uid>/`) |
| Service systemd | `~/.config/systemd/user/syncgdrive.service` |

## Dépendances principales

| Crate | Usage |
|---|---|
| tokio + tokio-util | Runtime async, CancellationToken, Semaphore |
| rusqlite (bundled) | SQLite WAL pour l'index de fichiers |
| notify | inotify watcher (close_write, create, delete, rename) |
| walkdir | Inventaire récursif du filesystem |
| globset | Patterns d'exclusion glob |
| sha2 | Hash SHA-256 pour déduplication |
| serde + toml | Sérialisation config TOML |
| tracing + tracing-subscriber + tracing-appender | Logging structuré dual (stdout + fichier rotatif) |
| notify-rust | Notifications bureau D-Bus |
| libc | Self-pipe trick, process_group, flock, kill SIGTERM |
| anyhow + thiserror | Gestion d'erreurs |
| async-trait | Trait KioOps mockable |
| gtk4 + libadwaita (optionnel) | Fenêtre Settings + À propos |
| ksni (optionnel) | Systray StatusNotifierItem D-Bus |
