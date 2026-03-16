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
- 🔐 **Sécurité OAuth2** — flux loopback PKCE, auto-refresh des tokens, chiffrement AES-256-GCM au repos

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

### Configuration de l'API Google Drive (Gratuit)
Pour que SyncGDrive puisse communiquer avec votre compte Google Drive de manière autonome, vous devez créer vos propres identifiants OAuth2. Cette opération est 100% gratuite et ne nécessite aucune validation complexe de la part de Google grâce à l'utilisation du scope restreint drive.file (l'application ne voit que les fichiers qu'elle a elle-même créés).

1. Étape 1 : Créer le projet Google Cloud
    * Rendez-vous sur la Google Cloud Console.
    * Connectez-vous avec votre compte Google.
    * Cliquez sur la liste déroulante des projets en haut à gauche et sélectionnez Nouveau projet.
    * Nommez-le SyncGDrive (ou le nom de votre choix) et cliquez sur Créer.


2. Étape 2 : Activer l'API Google Drive
   * Dans le menu de gauche, allez dans API et services > Bibliothèque.
   * Cherchez "Google Drive API" et cliquez dessus.
   * Cliquez sur le bouton bleu Activer.


3. Étape 3 : Configurer l'écran de consentement OAuth
   * Allez dans API et services > Écran de consentement OAuth.
   * Choisissez le type d'utilisateur Externe (sauf si vous avez un compte Google Workspace payant) et cliquez sur Créer.
   * Remplissez les informations obligatoires :
   * Nom de l'application : SyncGDrive
   * Adresses e-mail d'assistance et du développeur (votre email).
   * Cliquez sur Enregistrer et continuer.
   * Sur l'écran Champs d'application (Scopes), cliquez sur Ajouter ou supprimer des champs d'application.
   * Cherchez et cochez manuellement le scope : https://www.googleapis.com/auth/drive.file.
   * Continuez jusqu'à la section Utilisateurs tests. Ajoutez l'adresse email de votre compte Google (celui que vous allez synchroniser) pour pouvoir utiliser l'application pendant la phase de test.


4. Étape 4 : Créer les identifiants
   * Allez dans API et services > Identifiants.
   * Cliquez sur + Créer des identifiants en haut, puis choisissez ID client OAuth.
   * Type d'application : Sélectionnez Application de bureau (Desktop app).
   * Nom : SyncGDrive Desktop.
   * Cliquez sur Créer.


5. Étape 5 : Configurer l'application locale
   * Une fenêtre s'affiche avec votre ID client et votre Code secret du client.
     1. Sur votre machine Arch Linux, créez le dossier de configuration s'il n'existe pas :
     ``` Bash
     mkdir -p ~/.config/syncgdrive
     ```
     2. Créez un fichier .env à l'intérieur :
     ``` Bash
     nano ~/.config/syncgdrive/.env
     ```
     3. Collez vos identifiants dans ce fichier selon le format suivant :
     ```
     SYNCGDRIVE_CLIENT_ID=votre_id_client_ici.apps.googleusercontent.com
     SYNCGDRIVE_CLIENT_SECRET=votre_code_secret_ici
     ```
     4. Verrouillez les droits d'accès au fichier pour votre sécurité :
     ``` Bash
     chmod 600 ~/.config/syncgdrive/.env
     ```
Votre application est maintenant prête à être lancée et authentifiée !


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
4. Allez dans la section **Authentification** et cliquez sur **Lier**
5. Ajustez les **exclusions** si nécessaire
6. Cliquez **Enregistrer** — la synchronisation démarre

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
max_workers = 4
notifications = false
rescan_interval_min = 30
ignore_patterns = [
]

[[sync_pairs]]
name = "Nom du couple local-remote"
local_path = "/home/user/MonDossier/"
remote_folder_id = "gdrive:/MonDrive/MonDossier/"
provider = "GoogleDrive"
active = true
ignore_patterns = [
]

[retry]
max_attempts = 3
initial_backoff_ms = 300
max_backoff_ms = 8000

[advanced]
debounce_ms = 500
health_check_interval_secs = 30
max_concurrent_ls = 8
shutdown_timeout_secs = 3
log_retention_days = 7
engine_channel_capacity = 32
notification_timeout_ms = 6000
resumable_upload_threshold = 5242880
upload_limit_kbps = 0
api_rate_limit_rps = 10
delete_mode = "trash"
symlink_mode = "ignore"

```

## Chemins

| Donnée | Emplacement                                       |
|---|---------------------------------------------------|
| Config | `~/.config/syncgdrive/config.toml`                |
| Fichiers d'environnement (API) | `~/.config/syncgdrive/.env`                       |
| Base de données | `~/.local/share/syncgdrive/index.db`              |
| Tokens chiffrés | `~/.config/syncgdrive/tokens.enc`                 |
| Logs (rotation quotidienne) | `~/.local/state/syncgdrive/logs/syncgdrive.log.*` |
| PID / Lock | `$XDG_RUNTIME_DIR/syncgdrive.lock`                |
| Service systemd | `~/.config/systemd/user/syncgdrive.service`       |

## Architecture

```
src/
├── main.rs             Orchestration Tokio + self-pipe POSIX + PID file + instance lock
├── config.rs           AppConfig TOML + validation
├── db.rs               SQLite WAL (path, hash SHA-256, mtime)
├── ignore.rs           IgnoreMatcher globset
├── kio.rs              Wrapper kioclient5 (trait KioOps)
├── auth/       
│   ├── mod.rs          Déclarations de modules, re-export GoogleAuth et OAuth2
│   ├── google_auth.rs  Flux d'authentification OAuth2 PKCE + auto-refresh 
│   └── oauth2.rs       Implémentation générique du flux OAuth2 (utilisé par GoogleAuth) 
│   └── storage.rs      Abstraction de stockage des tokens (fichier chiffré + verrouillage) + chiffrement AES-256-GCM au repos
├── notif.rs            Notifications bureau (silence par défaut, erreurs + sync initiale)
├── engine/
│   ├── mod.rs          SyncEngine : boucle principale + Pause/Resume + détection local_root
│   ├── scan.rs         Scan 4 phases + retry + détection quota
│   ├── watcher.rs      inotify temps réel + overflow detection
│   └── worker.rs       Workers de synchronisation
└── ui/
    ├── mod.rs          Déclarations de modules, re-export spawn_tray
    ├── tray.rs         Systray ksni : icônes, tooltip dynamique, menu contextuel, À propos
    └── settings.rs     Fenêtre GTK4/libadwaita
dist/
└── syncgdrive.service  Fichier de référence pour systemd --user
```

## Licence

MIT

