# SyncGDrive — État actuel (2026-03-13)

## Architecture

```
src/
├── main.rs          # Orchestration : Tokio runtime + self-pipe POSIX (SIGINT/SIGTERM)
│                    #   + systray ksni + dual logging (stdout + fichier)
├── lib.rs           # Modules publics (ui conditionnel via feature gate)
├── config.rs        # AppConfig TOML + validation + expand ~ + sauvegarde
│                    #   Chemins XDG : ~/.config/syncgdrive/config.toml
├── db.rs            # SQLite WAL (file_index : path, hash SHA-256, mtime)
│                    #   Arc<Mutex<Connection>> pour partage inter-tâches
├── ignore.rs        # IgnoreMatcher globset (trailing-slash fix pour dossiers)
├── kio.rs           # Wrapper kioclient5 (process_group, kill propre, trait KioOps)
│                    #   ls récursif BFS, mkdir_if_absent, copy_file_smart, copy_atomic
├── notif.rs         # Notifications bureau (notify-rust) conditionnelles
├── engine/
│   ├── mod.rs       # SyncEngine : boucle principale, Pause/Resume, status, run_unconfigured
│   ├── scan.rs      # Scan initial 5 phases + remote index + comparaison DB + retry
│   ├── watcher.rs   # inotify (notify crate) : CloseWrite/Create/Delete/Rename
│   └── worker.rs    # Traitement Task : mtime→hash→upload, delete, rename + retry
└── ui/
    ├── mod.rs       # Systray ksni (StatusNotifierItem) + tooltip + icônes dynamiques
    └── settings.rs  # Fenêtre GTK4/libadwaita : chemins, exclusions, options + toast
```

## Fonctionnalités implémentées

### Moteur de synchronisation
- ✅ Scan initial en **5 phases** :
  - Phase 0 : listing récursif du remote (BFS `ls` → `HashSet` = remote index)
  - Phase 1 : inventaire filesystem local (walkdir + filtre ignore)
  - Phase 2 : création des dossiers distants manquants (via remote index, **sans `stat` individuel**)
  - Phase 3 : comparaison chaque fichier local ↔ DB (mtime rapide puis hash SHA-256)
  - Phase 4 : enqueue des fichiers modifiés/nouveaux vers la task queue
- ✅ Persistance SQLite WAL : fichiers déjà synchronisés (même hash) ne sont pas re-uploadés
- ✅ L'ordi local est la source de vérité, le Drive est la sauvegarde
- ✅ Watcher inotify temps réel (close_write, create, delete, rename both)
- ✅ Watcher non-bloquant (`try_send`) : si le channel déborde (burst d'événements), un rescan de rattrapage se déclenche ~30s plus tard (stratégie Dropbox/OneDrive)
- ✅ Upload atomique (`cat` pipe stdin) avec fallback `kioclient5 copy`
- ✅ **Remote index** : un seul `ls` récursif BFS au démarrage, utilisé par `mkdir_if_absent` et `copy_file_smart` pour éviter les `stat` individuels
- ✅ **Anti-doublon GDrive** : `stat` avant `mkdir` (watcher) ; remote index (scan)
- ✅ **Écrasement fichiers** : `cat` (pipe stdin) si le fichier existe, `copy` si nouveau
- ✅ Retry avec backoff exponentiel interruptible par shutdown
- ✅ Détection erreurs fatales KIO (auth/token/403/401) → pas de retry inutile
- ✅ Gestion Pause/Resume (fenêtre Settings ouverte → moteur en pause)
- ✅ Rescan automatique après changement de config pendant la pause
- ✅ Hot-reload config : changement de `local_root` → clear DB + nouveau watcher + rescan
- ✅ Shutdown propre : SIGINT/SIGTERM → CancellationToken → kill kioclient5 enfants
- ✅ Mode `run_unconfigured` : boucle d'attente de config valide (premier lancement)

### Trait KioOps (kio.rs)
- ✅ `ls_remote` : listing BFS récursif → `HashSet<String>` de tous les chemins distants
- ✅ `mkdir_p` : création récursive avec `stat` + retry (utilisé par le watcher)
- ✅ `mkdir_if_absent` : création via cache remote index (utilisé par le scan)
- ✅ `copy_file` : `stat` → si existe `cat` sinon `copy` + fallback `cat` (watcher)
- ✅ `copy_file_smart` : même logique via remote index (scan)
- ✅ `copy_atomic` : `cat - <remote>` avec pipe stdin depuis le fichier local
- ✅ `delete` : `rm` puis fallback `del`, tolérant si déjà supprimé
- ✅ `rename` : `move`, tolérant si déjà renommé
- ✅ `terminate_all` : SIGTERM sur tous les kioclient5 enfants en vol

### Interface utilisateur (feature `ui`)
- ✅ Systray ksni (StatusNotifierItem D-Bus) avec icônes dynamiques
- ✅ Tooltip au survol : état détaillé, progression, chemins local→remote
- ✅ Menu : Sync maintenant, Réglages…, Voir les logs, Quitter
- ✅ Fenêtre Settings libadwaita :
  - Chemins (local avec parcourir, remote)
  - Exclusions : liste éditable + parcourir multi-sélection + saisie glob manuelle
  - Options : workers parallèles (SpinRow 1–16), notifications bureau (SwitchRow)
  - Validation à l'enregistrement + toast d'erreur
- ✅ Fenêtre Settings ouverte automatiquement au premier lancement ou config invalide
- ✅ Notifications bureau (notify-rust) conditionnelles (`cfg.notifications`)
- ✅ Persistance config TOML (`~/.config/syncgdrive/config.toml`)

### Filtrage (ignore)
- ✅ Glob patterns (`**/target/**`, `**/.git/**`, `**/node_modules/**`, etc.)
- ✅ Fix trailing-slash : les dossiers eux-mêmes sont correctement ignorés
- ✅ Hot-reload via ApplyConfig (nouveau `IgnoreMatcher` à chaque rescan)
- ✅ Tests unitaires : dossier ignoré, fichier interne ignoré, fichier externe non affecté

### Logging
- ✅ Dual logging via tracing-subscriber :
  - stdout : compact, sans target, timer UTC `HH:MM:SS`
  - fichier : `~/.local/state/syncgdrive/syncgdrive.log` (rolling never, avec target)
- ✅ `EnvFilter` : défaut `info,zbus=warn,globset=warn,glib=warn`, surchargeable via `RUST_LOG`
- ✅ Non-blocking writer pour le fichier (guard retourné pour flush au shutdown)

### Robustesse
- ✅ Pas de processus zombies (`process_group(0)` + kill propre via `libc::kill`)
- ✅ SQLite WAL (lecteurs ne bloquent pas les écrivains)
- ✅ `Arc<Mutex<Connection>>` pour partage DB entre tasks Tokio
- ✅ Sémaphore pour limiter les workers parallèles (`max_workers`)
- ✅ Premier lancement sans config → Settings s'ouvre automatiquement
- ✅ Self-pipe trick POSIX pour les signaux (compatible ksni + GTK)
- ✅ `AsyncFd` sur le pipe signal pour intégration propre avec `tokio::select!`
- ✅ Timeout 3s au shutdown si le moteur ne finit pas proprement
- ✅ Config validation : chemin local (existe, est dossier), protocole remote (gdrive, smb, sftp, webdav, ftp)

### Chemins XDG
| Donnée | Chemin |
|---|---|
| Config | `$XDG_CONFIG_HOME/syncgdrive/config.toml` (défaut `~/.config/`) |
| Base de données | `$XDG_DATA_HOME/syncgdrive/index.db` (défaut `~/.local/share/`) |
| Logs | `$XDG_STATE_HOME/syncgdrive/syncgdrive.log` (défaut `~/.local/state/`) |

## Notifications bureau

| Événement | Notification |
|---|---|
| Démarrage scan | "Scan initial — Inventaire en cours…" |
| Progression dossiers | "Création des dossiers — X/Y" |
| Scan terminé | "X dossiers, Y fichiers à sync, Z déjà à jour" |
| Progression fichiers | "1/256 main.rs (4 Ko)" |
| Sync terminée | "X fichier(s) transférés" |
| Fichier synchronisé (watcher) | "↑ main.rs synchronisé" |
| Pause | "⏸ En pause — Réglages ouverts" |
| Reprise | "▶ La synchronisation a repris" |
| Erreur fatale | "Erreur ⚠ — message KIO" |

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
| tracing + tracing-subscriber + tracing-appender | Logging structuré dual (stdout + fichier) |
| notify-rust | Notifications bureau D-Bus |
| libc | Self-pipe trick, process_group, kill SIGTERM |
| anyhow + thiserror | Gestion d'erreurs |
| async-trait | Trait KioOps mockable |
| gtk4 + libadwaita (optionnel) | Fenêtre Settings |
| ksni (optionnel) | Systray StatusNotifierItem D-Bus |
