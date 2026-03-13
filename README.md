# SyncGDrive

Synchronisation unidirectionnelle d'un dossier local vers Google Drive (ou tout backend KIO : SMB, SFTP, WebDAV…).

L'ordinateur local est la **source de vérité** — le Drive est la sauvegarde.

## Fonctionnalités

- 🔄 **Scan initial intelligent** — inventaire local + remote index BFS, ne re-uploade que les fichiers modifiés (mtime + SHA-256)
- 👁 **Surveillance temps réel** — inotify (close_write, create, delete, rename)
- 🚀 **Upload atomique** — `cat` pipe stdin pour écraser, `copy` pour les nouveaux fichiers
- 🔁 **Retry automatique** — backoff exponentiel, détection erreurs fatales (auth/token)
- 🛡 **Anti-doublon GDrive** — `stat` avant `mkdir`, remote index pour le scan
- 💾 **Persistance SQLite** — les fichiers déjà synchronisés ne sont pas retransférés
- 🖥 **Systray KDE** — icône StatusNotifierItem avec tooltip dynamique et menu contextuel
- ⚙️ **Fenêtre Réglages** — GTK4/libadwaita : chemins, exclusions glob, workers, notifications
- 🔔 **Notifications bureau** — progression du scan et de la synchronisation
- 🧹 **Exclusions glob** — `**/target/**`, `**/.git/**`, patterns personnalisables
- ⏸ **Pause/Resume** — moteur en pause pendant l'édition des réglages
- 🛑 **Shutdown propre** — SIGINT/SIGTERM → arrêt gracieux de tous les processus KIO

## Prérequis

- **Linux** avec KDE Frameworks (kioclient5)
- **Rust** ≥ 1.70
- **GTK4** + **libadwaita** (pour la feature `ui`)
- Compte Google Drive configuré dans KDE (KIO GDrive)

### Paquets Fedora / openSUSE

```bash
sudo zypper install gtk4-devel libadwaita-devel kio-gdrive
# ou
sudo dnf install gtk4-devel libadwaita-devel kio-gdrive
```

### Paquets Debian / Ubuntu

```bash
sudo apt install libgtk-4-dev libadwaita-1-dev kio-gdrive
```

## Installation

```bash
git clone https://github.com/votre-user/SyncGDrive.git
cd SyncGDrive
cargo build --release --features ui
```

Le binaire sera dans `target/release/syncgdrive`.

## Utilisation

```bash
# Lancement (ouvre les Réglages au premier démarrage)
syncgdrive

# Avec logs détaillés
RUST_LOG=debug syncgdrive
```

### Premier lancement

1. La fenêtre **Réglages** s'ouvre automatiquement
2. Configurez le **dossier local** à synchroniser
3. Configurez l'**URL distante** (ex: `gdrive:/MonDrive/Backup`)
4. Ajustez les **exclusions** si nécessaire
5. Cliquez **Enregistrer** — la synchronisation démarre

### Menu systray

| Action | Description |
|---|---|
| Sync maintenant | Force un rescan complet |
| Réglages… | Ouvre la fenêtre de configuration (met le moteur en pause) |
| Voir les logs | Ouvre le fichier de log dans l'éditeur par défaut |
| Quitter | Arrêt propre du daemon |

## Configuration

Fichier : `~/.config/syncgdrive/config.toml`

```toml
local_root = "/home/user/Projets"
remote_root = "gdrive:/MonDrive/Backup"
max_workers = 2
notifications = true

[retry]
max_attempts = 3
initial_backoff_ms = 300
max_backoff_ms = 8000

ignore_patterns = [
    "**/target/**",
    "**/.git/**",
    "**/node_modules/**",
    "**/.sqlx/**",
    "**/.idea/**",
]
```

## Chemins

| Donnée | Emplacement |
|---|---|
| Config | `~/.config/syncgdrive/config.toml` |
| Base de données | `~/.local/share/syncgdrive/index.db` |
| Logs | `~/.local/state/syncgdrive/syncgdrive.log` |

## Architecture

```
src/
├── main.rs        Orchestration Tokio + self-pipe POSIX + systray
├── config.rs      AppConfig TOML + validation
├── db.rs          SQLite WAL (path, hash SHA-256, mtime)
├── ignore.rs      IgnoreMatcher globset
├── kio.rs         Wrapper kioclient5 (trait KioOps)
├── notif.rs       Notifications bureau
├── engine/
│   ├── mod.rs     SyncEngine : boucle principale + Pause/Resume
│   ├── scan.rs    Scan 5 phases + retry
│   ├── watcher.rs inotify temps réel
│   └── worker.rs  Workers de synchronisation
└── ui/
    ├── mod.rs     Systray ksni
    └── settings.rs Fenêtre GTK4/libadwaita
```

## Licence

MIT

