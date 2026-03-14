# SyncGDrive

Synchronisation unidirectionnelle d'un dossier local vers Google Drive (ou tout backend KIO : SMB, SFTP, WebDAV…).

L'ordinateur local est la **source de vérité** — le Drive est la sauvegarde.

## Fonctionnalités

- 🔄 **Scan initial intelligent** — inventaire local + remote index BFS, ne re-uploade que les fichiers modifiés (mtime + SHA-256)
- 👁 **Surveillance temps réel** — inotify (close_write, create, delete, rename)
- 🚀 **Upload atomique** — `cat` pipe stdin pour écraser, `copy` pour les nouveaux fichiers
- 🔁 **Retry automatique** — backoff exponentiel, détection erreurs fatales (auth/token/quota)
- 🛡 **Anti-doublon GDrive** — remote index BFS, `mkdir` sans `stat` redondant pendant le scan
- 💾 **Persistance SQLite** — les fichiers déjà synchronisés ne sont pas retransférés
- 🖥 **Systray KDE** — icône StatusNotifierItem avec tooltip dynamique, barre de progression fichier par fichier, menu contextuel complet
- ⚙️ **Fenêtre Réglages** — GTK4/libadwaita : chemins, exclusions glob, workers, notifications
- ℹ️ **À propos** — fenêtre libadwaita avec version et crédits
- 🔔 **Notifications bureau** — politique de silence, pop-ups uniquement pour erreurs fatales et fin de sync initiale
- 🧹 **Exclusions glob** — `**/target/**`, `**/.git/**`, patterns personnalisables
- ⏸ **Pause/Resume** — moteur en pause pendant l'édition des réglages
- 🛑 **Shutdown propre** — SIGINT/SIGTERM → arrêt gracieux de tous les processus KIO
- 🔒 **Instance unique** — verrou `flock` + PID file dans `$XDG_RUNTIME_DIR`
- 🚀 **Service systemd** — toggle "Lancer au démarrage" via `systemctl --user`
- 📊 **Logs rotatifs** — rotation quotidienne, rétention 7 jours, non-bloquant

## Prérequis

- **Linux** avec KDE Frameworks (kioclient5)
- **Rust** ≥ 1.70
- **GTK4** + **libadwaita** (pour la feature `ui`)
- Compte Google Drive configuré dans KDE (KIO GDrive)

### Paquets Arch Linux

```bash
sudo pacman -S gtk4 libadwaita kio-gdrive
```

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
git clone https://github.com/clyds/SyncGDrive.git
cd SyncGDrive
cargo build --release --features ui
```

Le binaire sera dans `target/release/syncgdrive`.

Pour l'installer dans `~/.cargo/bin/` (requis par le service systemd) :

```bash
cargo install --features ui --path .
```

### Service systemd (optionnel)

Un fichier de référence est fourni dans `dist/syncgdrive.service` :

```bash
cp dist/syncgdrive.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable syncgdrive.service   # démarrage auto
systemctl --user start syncgdrive.service    # démarrage immédiat
```

Le toggle "Lancer au démarrage" du menu systray exécute `systemctl --user enable/disable` automatiquement.

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
| *[État actuel]* | Ligne grisée affichant l'état du moteur |
| Synchroniser maintenant | Force un rescan complet (Idle) |
| ⏸ Mettre en pause | Suspend le moteur (pendant un scan/transfert) |
| ▶ Reprendre | Reprend la synchronisation (en pause) |
| 📂 Ouvrir le dossier local | Ouvre le dossier surveillé dans Dolphin |
| ☁ Ouvrir Google Drive | Ouvre le Drive distant via `kioclient5 exec` |
| 🚀 Lancer au démarrage | Toggle systemd `enable`/`disable` |
| ⚙ Réglages… | Ouvre la fenêtre de configuration (met le moteur en pause) |
| 📄 Voir les logs | Ouvre le dossier de logs dans l'éditeur par défaut |
| ℹ À propos | Fenêtre libadwaita avec version et crédits |
| 🛑 Quitter | Arrêt propre du daemon |

### Notifications

Politique de **silence par défaut** — les pop-ups sont réservés aux événements critiques :

| Événement | Notification |
|---|---|
| Fin de sync initiale | ✅ Pop-up auto-dismiss 6s |
| Jeton KIO expiré | ⚠ Sticky (reste jusqu'à fermeture) |
| Dossier local disparu | ⚠ Sticky + moteur en pause |
| Quota dépassé | ⚠ Sticky |
| Événements courants | Tooltip systray uniquement |

## Configuration

Fichier : `~/.config/syncgdrive/config.toml`

```toml
local_root = "/home/user/Projets"
remote_root = "gdrive:/MonDrive/Backup"
max_workers = 4
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
| Logs (rotation quotidienne) | `~/.local/state/syncgdrive/logs/syncgdrive.log.*` |
| PID / Lock | `$XDG_RUNTIME_DIR/syncgdrive.lock` |
| Service systemd | `~/.config/systemd/user/syncgdrive.service` |

## Architecture

```
src/
├── main.rs         Orchestration Tokio + self-pipe POSIX + PID file + instance lock
├── config.rs       AppConfig TOML + validation
├── db.rs           SQLite WAL (path, hash SHA-256, mtime)
├── ignore.rs       IgnoreMatcher globset
├── kio.rs          Wrapper kioclient5 (trait KioOps)
├── notif.rs        Notifications bureau (silence par défaut, erreurs + sync initiale)
├── engine/
│   ├── mod.rs      SyncEngine : boucle principale + Pause/Resume + détection local_root
│   ├── scan.rs     Scan 4 phases + retry + détection quota
│   ├── watcher.rs  inotify temps réel + overflow detection
│   └── worker.rs   Workers de synchronisation
└── ui/
    ├── mod.rs      Déclarations de modules, re-export spawn_tray
    ├── tray.rs     Systray ksni : icônes, tooltip dynamique, menu contextuel, À propos
    └── settings.rs Fenêtre GTK4/libadwaita
dist/
└── syncgdrive.service  Fichier de référence pour systemd --user
```

## Licence

MIT

