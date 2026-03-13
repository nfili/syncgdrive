# SyncGDrive — Guide d'utilisation

## Qu'est-ce que SyncGDrive ?

SyncGDrive synchronise automatiquement un dossier local vers Google Drive
(ou tout backend KIO : SMB, SFTP, WebDAV, FTP).

- **Unidirectionnel** : local → distant (l'ordinateur est la source de vérité)
- **Temps réel** : chaque modification est détectée et envoyée
- **Intelligent** : seuls les fichiers réellement modifiés sont transférés

---

## Prérequis

| Composant | Pourquoi |
|---|---|
| **KDE Frameworks 5** (kioclient5) | Transferts vers Google Drive via le protocole KIO |
| **kio-gdrive** | Plugin KIO pour l'accès Google Drive |
| **GTK4 + libadwaita** | Fenêtre de réglages (feature `ui`) |
| Compte Google | Configuré dans *Paramètres système KDE → Comptes en ligne* |

### Installation des dépendances

**openSUSE / Fedora :**
```bash
sudo zypper install gtk4-devel libadwaita-devel kio-gdrive
# ou
sudo dnf install gtk4-devel libadwaita-devel kio-gdrive
```

**Debian / Ubuntu :**
```bash
sudo apt install libgtk-4-dev libadwaita-1-dev kio-gdrive
```

### Configurer le compte Google Drive

1. Ouvrez **Paramètres système KDE** → **Comptes en ligne**
2. Ajoutez un **compte Google**
3. Autorisez l'accès aux fichiers
4. Vérifiez que `kioclient5 ls gdrive:/` liste bien vos fichiers

---

## Compilation

```bash
git clone <url-du-dépôt>
cd SyncGDrive
cargo build --release --features ui
```

Le binaire est dans `target/release/syncgdrive`.

---

## Premier lancement

```bash
syncgdrive
```

Au premier lancement, la fenêtre **Réglages** s'ouvre automatiquement.
Vous devez configurer au minimum :

1. **Dossier local** — le répertoire à surveiller (ex : `/home/user/Projets`)
2. **URL distante** — la destination KIO (ex : `gdrive:/MonDrive/Backup`)

Cliquez **Enregistrer** — la synchronisation démarre immédiatement.

---

## Fenêtre Réglages

Accessible via le menu systray → **Réglages…** ou automatiquement au premier lancement.

> ⏸ Pendant que les réglages sont ouverts, le moteur est **en pause**.
> La synchronisation reprend automatiquement à la fermeture de la fenêtre.

### Groupe « Chemins »

| Champ | Description |
|---|---|
| **Dossier local** | Chemin absolu du répertoire à synchroniser. Le bouton 📁 ouvre un sélecteur. Le tilde `~/` est supporté dans le fichier TOML. |
| **URL distante** | URL KIO de la destination. Protocoles supportés : `gdrive:/`, `smb://`, `sftp://`, `webdav://`, `ftp://` |

### Groupe « Exclusions »

Liste des patterns glob à ignorer. Par défaut :

- `**/target/**` — dossiers de build Rust
- `**/.git/**` — dépôts Git
- `**/node_modules/**` — dépendances Node.js
- `**/.sqlx/**` — cache SQLx
- `**/.idea/**` — config JetBrains

**Ajouter une exclusion :**
- Bouton **+** → saisir un pattern glob manuellement (ex : `**/*.log`, `**/build/**`)
- Bouton **Parcourir…** → sélection multiple de dossiers/fichiers (convertis en glob automatiquement)

**Supprimer une exclusion :**
- Bouton ❌ rouge à droite de chaque ligne

### Groupe « Options »

| Option | Description | Défaut |
|---|---|---|
| **Workers parallèles** | Nombre de transferts simultanés (1–16) | 2 |
| **Notifications bureau** | Affiche les notifications de progression | Désactivé |

### Validation

À l'enregistrement, la config est validée :
- Le dossier local doit **exister** et être un **répertoire**
- L'URL distante doit commencer par un **protocole reconnu**
- En cas d'erreur, un **toast** s'affiche en bas de la fenêtre

---

## Icône systray

SyncGDrive vit dans la zone de notification (systray KDE).
L'icône change selon l'état :

| Icône | État |
|---|---|
| ✅ Coche verte | En veille — surveillance active |
| 🔄 Flèches | Synchronisation en cours |
| ⏸ Pause | Réglages ouverts |
| ⚠️ Triangle | Configuration requise |
| ❌ Erreur | Erreur (vérifiez les logs) |

### Tooltip

Survolez l'icône pour voir :
- L'état détaillé (phase du scan, progression, fichier en cours)
- Les chemins local → distant configurés

### Menu contextuel (clic droit)

| Action | Description |
|---|---|
| **Sync maintenant** | Force un rescan complet de tous les fichiers |
| **Réglages…** | Ouvre la fenêtre de configuration |
| **Voir les logs** | Ouvre le fichier de log dans l'éditeur par défaut |
| **Quitter** | Arrêt propre du daemon |

---

## Comment fonctionne la synchronisation

### Scan initial (au démarrage)

1. **Listing du distant** — inventaire récursif du Drive (un seul passage BFS)
2. **Inventaire local** — parcours récursif du dossier local (en excluant les patterns ignorés)
3. **Création des dossiers** — les dossiers locaux absents du Drive sont créés
4. **Comparaison** — chaque fichier local est comparé à la base de données :
   - Même `mtime` → déjà synchronisé, on passe
   - `mtime` différent mais même hash SHA-256 → contenu identique, on met à jour la DB
   - Hash différent → fichier modifié, ajouté à la file de transfert
5. **Transfert** — les fichiers modifiés/nouveaux sont envoyés vers le Drive

### Surveillance temps réel (après le scan)

Les modifications sont détectées via **inotify** :

| Événement | Action |
|---|---|
| Fichier créé ou modifié | Upload vers le Drive |
| Fichier supprimé | Suppression sur le Drive |
| Fichier renommé | Renommage sur le Drive |

### Déduplication

- Un fichier déjà synchronisé avec le **même contenu** (hash SHA-256) n'est pas retransféré
- Seuls les fichiers dont le **contenu a réellement changé** sont envoyés
- La base de données SQLite persiste entre les redémarrages

### Anti-doublon Google Drive

Google Drive autorise plusieurs fichiers avec le même nom dans un dossier.
SyncGDrive évite les doublons :
- **Fichier existant** → écrasement via pipe (`cat`) au lieu de `copy`
- **Fichier nouveau** → création via `copy` (plus rapide)

---

## Fichier de configuration

Emplacement : `~/.config/syncgdrive/config.toml`

```toml
# Dossier local à synchroniser
local_root = "/home/user/Projets"

# URL distante KIO
remote_root = "gdrive:/MonDrive/Backup"

# Nombre de transferts simultanés (1–16)
max_workers = 2

# Activer les notifications bureau
notifications = true

# Patterns d'exclusion (glob)
ignore_patterns = [
    "**/target/**",
    "**/.git/**",
    "**/node_modules/**",
    "**/.sqlx/**",
    "**/.idea/**",
]

# Configuration du retry automatique
[retry]
max_attempts = 3
initial_backoff_ms = 300
max_backoff_ms = 8000
```

### Protocoles supportés

| Protocole | Exemple | Usage |
|---|---|---|
| `gdrive:/` | `gdrive:/MonDrive/Backup` | Google Drive via KIO |
| `smb://` | `smb://serveur/partage` | Partage Windows/Samba |
| `sftp://` | `sftp://user@host/chemin` | SSH/SFTP |
| `webdav://` | `webdav://cloud.example.com/dav` | WebDAV (Nextcloud, etc.) |
| `ftp://` | `ftp://host/chemin` | FTP classique |

---

## Fichiers et emplacements

| Fichier | Emplacement | Description |
|---|---|---|
| Configuration | `~/.config/syncgdrive/config.toml` | Réglages de l'application |
| Base de données | `~/.local/share/syncgdrive/index.db` | Index des fichiers synchronisés (SQLite) |
| Logs | `~/.local/state/syncgdrive/syncgdrive.log` | Journal détaillé des opérations |

> Les chemins suivent la convention **XDG** et peuvent être personnalisés
> via `$XDG_CONFIG_HOME`, `$XDG_DATA_HOME`, `$XDG_STATE_HOME`.

---

## Logs et débogage

### Voir les logs

```bash
# Depuis le systray : menu → Voir les logs

# Ou directement :
cat ~/.local/state/syncgdrive/syncgdrive.log

# Suivre en temps réel :
tail -f ~/.local/state/syncgdrive/syncgdrive.log
```

### Augmenter la verbosité

```bash
# Mode debug complet
RUST_LOG=debug syncgdrive

# Debug uniquement pour le moteur
RUST_LOG=info,sync_g_drive::engine=debug syncgdrive

# Debug KIO (voir les commandes kioclient5)
RUST_LOG=info,sync_g_drive::kio=debug syncgdrive
```

### Vérifier que KIO fonctionne

```bash
# Lister les fichiers sur le Drive
kioclient5 ls gdrive:/

# Créer un dossier test
kioclient5 mkdir gdrive:/TestSyncGDrive

# Copier un fichier
kioclient5 copy /tmp/test.txt gdrive:/TestSyncGDrive/test.txt

# Supprimer
kioclient5 rm gdrive:/TestSyncGDrive/test.txt
```

---

## Patterns d'exclusion (glob)

Les patterns utilisent la syntaxe **glob** :

| Pattern | Exclut |
|---|---|
| `**/target/**` | Tout dossier `target/` et son contenu, à n'importe quelle profondeur |
| `**/.git/**` | Tous les dépôts Git |
| `**/*.log` | Tous les fichiers `.log` |
| `**/build/**` | Tous les dossiers `build/` |
| `**/.env` | Tous les fichiers `.env` |
| `**/secret_*` | Tous les fichiers commençant par `secret_` |

> `**` signifie « n'importe quel nombre de répertoires intermédiaires ».

---

## Notifications bureau

Si activées dans les réglages, des notifications apparaissent pour :

| Événement | Message |
|---|---|
| Début du scan | « Inventaire de /chemin en cours… » |
| Création dossiers | « Dossier 3/12 sur le Drive… » |
| Scan terminé | « 12 dossiers, 156 fichiers à synchroniser, 340 déjà à jour » |
| Transfert en cours | « 42/156 main.rs (4 Ko) » |
| Sync terminée | « 156 fichier(s) transférés vers le Drive » |
| Fichier modifié (watcher) | « ↑ main.rs synchronisé » |
| Pause | « ⏸ Réglages ouverts. Reprendra à la fermeture. » |
| Reprise | « ▶ La synchronisation a repris. » |
| Erreur | « Erreur ⚠ — message KIO » |

---

## Arrêt et signaux

| Méthode | Effet |
|---|---|
| Menu systray → **Quitter** | Arrêt propre |
| `Ctrl+C` dans le terminal | SIGINT → arrêt propre |
| `kill <pid>` | SIGTERM → arrêt propre |
| `kill -9 <pid>` | Arrêt forcé (déconseillé) |

L'arrêt propre :
1. Annule toutes les tâches en cours
2. Envoie SIGTERM aux processus `kioclient5` en vol
3. Attend max 3 secondes que le moteur finisse
4. Écrit les logs de fermeture

---

## Dépannage

### « Config requise » au démarrage

→ Le fichier `config.toml` est vide ou invalide. Ouvrez les **Réglages** depuis le systray.

### « le dossier local n'existe pas »

→ Le chemin configuré dans `local_root` n'existe pas. Créez-le ou modifiez-le dans les Réglages.

### « protocole KIO non reconnu »

→ L'URL distante doit commencer par `gdrive:/`, `smb://`, `sftp://`, `webdav://` ou `ftp://`.

### Les fichiers ne se synchronisent pas

1. Vérifiez que `kioclient5 ls gdrive:/` fonctionne dans un terminal
2. Vérifiez les logs : `tail -f ~/.local/state/syncgdrive/syncgdrive.log`
3. Lancez avec `RUST_LOG=debug syncgdrive` pour plus de détails
4. Vérifiez que le fichier n'est pas exclu par un pattern d'exclusion

### Erreur « jetons d'accès » / « token expired »

→ Le token Google a expiré. Reconnectez votre compte Google dans
**Paramètres système KDE → Comptes en ligne**, puis relancez SyncGDrive.

### Erreur « permission denied » / « 403 »

→ Le compte Google n'a pas accès au dossier distant. Vérifiez les
permissions dans Google Drive et dans les Comptes en ligne KDE.

### Doublons sur Google Drive

SyncGDrive gère les doublons automatiquement. Si vous en voyez :
1. Supprimez les doublons manuellement dans Google Drive
2. Supprimez la base de données : `rm ~/.local/share/syncgdrive/index.db`
3. Relancez SyncGDrive — un scan complet recréera l'index

### Réinitialiser complètement

```bash
# Supprimer toute la configuration et les données
rm -rf ~/.config/syncgdrive
rm -rf ~/.local/share/syncgdrive
rm -rf ~/.local/state/syncgdrive

# Relancer — premier lancement
syncgdrive
```

