# 📄 SyncGDrive V2 — Document de Pré-Développement

---

## 1. Vision et Objectifs

L'objectif de la V2 est de passer d'un prototype dépendant de processus externes (KDE/KIO) à un moteur autonome, performant et capable de gérer plusieurs sources de données simultanément.

- **Zéro Dépendance Subprocess** : Remplacement de `kioclient5` par l'API REST Google Drive v3 native via `reqwest`.
- **Multi-Dossiers (Multi-Sync)** : Surveillance de plusieurs répertoires locaux, chacun avec sa propre configuration remote.
- **Performance Native** : Utilisation de l'API Delta (`changes.list`) pour une synchronisation quasi-instantanée et un overhead minimal.
- **Fiabilité "Excellence"** : Gestion des limites système (inotify) et résilience réseau (Resumable Uploads).
- **Zéro Valeur Hardcodée** : Toute constante configurable passe par le fichier de configuration, avec des valeurs par défaut sensibles.
- **UX Premium** : Icônes SVG, animations, fenêtre de scan initial, notifications contextuelles.

---

## 2. Élimination des Valeurs Hardcodées

### 2.1 Inventaire V1 — Valeurs en dur dans le code

| Valeur | Fichier | Ligne | Actuellement |
|--------|---------|-------|-------------|
| Debounce watcher | `engine/mod.rs` | 525 | `const DEBOUNCE_MS: u64 = 500` |
| Tick overflow/health check | `engine/mod.rs` | 196-197 | `Duration::from_secs(30)` |
| Concurrence `ls` BFS | `kio.rs` | 247 | `const MAX_CONCURRENT_LS: usize = 8` |
| KIO process timeout fallback | `kio.rs` | 395 | `500ms` sleep hardcodé |
| Shutdown timeout | `main.rs` | 108 | `Duration::from_secs(3)` |
| Rétention logs | `main.rs` | 19 | `cleanup_old_logs(&log_dir, 7)` |
| Channel capacity | `main.rs` | 31 | `mpsc::channel(32)` |
| Notification timeout (doublon) | `main.rs` | 160 | `.timeout(4000)` |
| Notification timeout (sticky) | `notif.rs` | 64 | `timeout 0` |
| Notification auto-dismiss | `notif.rs` | — | `6000ms` (initial_sync_complete) |
| Protocoles supportés | `settings.rs` | 378 | `const SUPPORTED` (liste statique) |

### 2.2 Déjà Configurables en V1

| Paramètre | Fichier config | Valeur par défaut |
|-----------|---------------|-------------------|
| `max_workers` | `config.toml` | `4` |
| `rescan_interval_min` | `config.toml` | `30` min |
| `kio_timeout_secs` | `config.toml` | `120` s |
| `retry.max_attempts` | `config.toml` | `3` |
| `retry.initial_backoff_ms` | `config.toml` | `300` ms |
| `retry.max_backoff_ms` | `config.toml` | `8000` ms |
| `ignore_patterns` | `config.toml` | `[target, .git, node_modules, .sqlx, .idea]` |
| `notifications` | `config.toml` | `false` |

### 2.3 Plan V2 — Tout dans le fichier de config

Nouveaux champs à ajouter dans `config.toml` (section `[advanced]`) :

```toml
# ── config.toml V2 ─────────────────────────────────────────────

# ... champs existants (local_root, remote_root, etc.) ...

[advanced]
# Debounce des événements inotify (ms). Défaut : 500
debounce_ms = 500

# Intervalle du health-check local_root + overflow tick (s). Défaut : 30
health_check_interval_secs = 30

# Concurrence max pour le listing BFS distant. Défaut : 8
max_concurrent_ls = 8

# Timeout d'arrêt gracieux du moteur (s). Défaut : 3
shutdown_timeout_secs = 3

# Rétention des fichiers de log (jours). Défaut : 7
log_retention_days = 7

# Taille du channel de commandes moteur. Défaut : 32
engine_channel_capacity = 32

# Durée d'affichage des notifications auto-dismiss (ms). Défaut : 6000
notification_timeout_ms = 6000

# Seuil pour upload resumable au lieu de simple (octets). Défaut : 5242880 (5 Mo)
resumable_upload_threshold = 5242880
```

### 2.4 Règle de Conception V2

> **Aucune constante numérique ne doit apparaître dans le code métier.**
> Toute valeur configurable est lue depuis `AppConfig` avec un `#[serde(default)]`.
> Les seules constantes autorisées en dur sont les constantes mathématiques
> (`KB`, `MB`, `GB`) et les identifiants structurels (noms d'application, IDs D-Bus).

---

## 3. Améliorations UX (V2)

### 3.1 Icônes SVG Embarquées

**Problème V1** : Les icônes sont des noms FreeDesktop (`emblem-ok-symbolic`, `dialog-error`…).
Elles dépendent du thème d'icônes installé et varient d'un système à l'autre.

**Solution V2** : Embarquer des icônes SVG personnalisées dans le binaire.

- **Taille** : SVG = quelques Ko chacune, embarquables via `include_bytes!` ou un `GResource` compilé.
- **Cohérence** : Même rendu sur KDE, GNOME, Sway, i3.
- **Systray** : ksni supporte les icônes pixmap (`icon_pixmap()`) en plus des noms.
  On fournit le SVG rendu en ARGB32 à la taille demandée.
- **Fenêtres GTK** : `gtk4::gdk::Texture::from_bytes()` pour charger les SVG directement.

**Palette d'icônes à créer** :

| État | Icône | Description |
|------|-------|-------------|
| Idle | `syncgdrive-idle.svg` | Coche verte, nuage calme |
| Syncing | `syncgdrive-sync.svg` | Flèches circulaires (base pour animation) |
| Scanning | `syncgdrive-scan.svg` | Loupe ou radar |
| Paused | `syncgdrive-paused.svg` | Pause, couleur neutre |
| Error | `syncgdrive-error.svg` | Exclamation rouge |
| Offline | `syncgdrive-offline.svg` | Nuage barré |

**Format** : SVG optimisés (svgo), 24×24 px viewBox, mono-couleur pour les `-symbolic` + variantes couleur pour les fenêtres.

### 3.2 Icônes Animées dans le Systray

**Objectif** : Pendant un transfert ou un scan, l'icône du systray s'anime pour montrer visuellement que SyncGDrive travaille (comme Dropbox, OneDrive, Syncthing).

**Implémentation** :

- **Principe** : Rotation entre N frames de l'icône de synchronisation (ex : flèches qui tournent en 4 étapes).
- **Mécanisme** : Une tâche Tokio dédiée envoie un `handle.update()` toutes les ~300ms pendant l'état `Syncing`/`ScanProgress`, en alternant les frames.
- **Frames** : 4 SVG (`sync-frame-0.svg` → `sync-frame-3.svg`), chacune = les flèches décalées de 90°.
- **Arrêt** : Dès que l'état passe à `Idle`/`Paused`/`Error`, l'animation s'arrête et l'icône statique reprend.

```
État Idle       → icône statique (coche verte)
État Syncing    → cycle : frame0 → frame1 → frame2 → frame3 → frame0… (300ms)
État Scanning   → cycle : même animation ou animation "radar"
État Error      → icône statique (exclamation rouge)
```

**Coût** : ksni supporte `icon_pixmap()` qui accepte des données brutes ARGB32. Chaque frame est un petit buffer (~2 Ko en 24×24). L'overhead mémoire est négligeable.

### 3.3 Fenêtre de Scan Initial

**Problème** : Le premier scan peut durer plusieurs minutes (voire dizaines de minutes) sur un gros dépôt. L'utilisateur ne sait pas ce qui se passe — juste une icône dans le systray.

**Solution** : Ouvrir automatiquement une fenêtre de progression au **premier lancement** (ou après un changement de `local_root`).

**Maquette UX** :

```
┌───────────────────────────────────────────────────────┐
│  SyncGDrive — Scan initial                         ✕  │
├───────────────────────────────────────────────────────┤
│                                                       │
│  🔍 Analyse de vos fichiers en cours…                 │
│                                                       │
│  Ce premier scan indexe l'ensemble de votre dossier   │
│  pour établir la base de synchronisation.             │
│  Il peut prendre plusieurs minutes selon la taille    │
│  de vos données.                                      │
│                                                       │
│  ┌─────────────────────────────────────────────────┐  │
│  │ Phase : Inventaire local          4 521 / 12 300│  │
│  │ █████████████████░░░░░░░░░░░░░░  37%            │  │
│  │                                                  │  │
│  │ 📂 src/engine/                                   │  │
│  │ 📄 scan.rs                                       │  │
│  └─────────────────────────────────────────────────┘  │
│                                                       │
│  ┌─────────────────────────────────────────────────┐  │
│  │ Phase : Création dossiers distants    120 / 450 │  │
│  │ ████████░░░░░░░░░░░░░░░░░░░░░░░░░░  27%        │  │
│  │                                                  │  │
│  │ 📂 Projets/SyncGDrive/                           │  │
│  │ 📁 src/                                          │  │
│  └─────────────────────────────────────────────────┘  │
│                                                       │
│  ⏱ Temps écoulé : 2 min 34 s                         │
│                                                       │
│                [ Réduire dans le systray ]             │
│                                                       │
└───────────────────────────────────────────────────────┘
```

> **Clé de lecture** : Chaque bloc de progression affiche **deux lignes** de contexte :
> la première (📂) montre le **répertoire parent**, la seconde (📄/📁) montre
> l'**élément courant**. L'utilisateur sait toujours *où* il se trouve dans l'arborescence.

**Comportement** :

| Événement | Action |
|-----------|--------|
| Premier lancement OU changement de `local_root` | Fenêtre ouverte automatiquement |
| Scan déjà fait (DB non vide, même `local_root`) | Pas de fenêtre — scan silencieux via systray |
| Clic « Réduire dans le systray » | Fenêtre se ferme, le scan continue en arrière-plan |
| Clic ✕ (fermer) | Même effet que « Réduire » — le scan **ne s'arrête pas** |
| Scan terminé + fenêtre ouverte | Fenêtre se ferme automatiquement |
| Scan terminé (fenêtre fermée ou réduite) | Notification bureau envoyée |

**Implémentation** :

- La fenêtre est une `libadwaita::Window` avec des `gtk4::ProgressBar` et des `gtk4::Label`.
- Elle tourne sur le thread `gtk-ui` existant (nouveau `GtkAction::ShowScanProgress`).
- Le moteur envoie les `EngineStatus::ScanProgress` comme en V1 — la fenêtre les consomme via un `glib::MainContext::channel` pour mettre à jour les barres.
- Le bouton « Réduire » envoie juste un signal de fermeture de fenêtre, pas de `Pause`.

### 3.4 Notification « Surveillance Prête »

**Comportement** : À la fin du premier scan (ou après changement de `local_root`), une notification bureau s'affiche :

```
╭─────────────────────────────────────────────╮
│ 🟢 SyncGDrive                               │
│                                              │
│ Surveillance des dossiers prête !            │
│ Vous pouvez commencer à travailler sur vos   │
│ fichiers — ils seront synchronisés            │
│ automatiquement.                             │
│                                              │
│ 12 300 fichiers indexés · 450 dossiers       │
╰─────────────────────────────────────────────╯
```

- **Auto-dismiss** : 8 secondes (configurable via `advanced.notification_timeout_ms`).
- **Remplace** l'actuel `initial_sync_complete` (qui ne s'affiche que si au moins 1 fichier a été transféré).
- **Toujours envoyée** après le premier scan, même si tout était déjà à jour.
- **Inclut des métriques** : nombre de fichiers et dossiers indexés.
- **Pas envoyée** lors des rescans périodiques (uniquement premier scan ou changement de config).

### 3.5 Lisibilité des Chemins — Contexte « Où suis-je ? »

**Problème V1** : Le tooltip et les logs affichent juste le nom du fichier courant
(`scan.rs`, `rapport.pdf`). Sur un gros dépôt avec des milliers de fichiers,
l'utilisateur ne sait pas **dans quel répertoire** il se trouve.

**Solution V2** : Toujours afficher le **répertoire parent** au-dessus de l'élément courant,
partout dans l'UI.

#### Règle d'affichage des chemins

| Contexte | Format V1 | Format V2 |
|----------|-----------|-----------|
| Tooltip scan (inventaire) | `scan.rs` | `📂 src/engine/` → `📄 scan.rs` |
| Tooltip scan (dossiers) | `/Projets/SyncGDrive/src` | `📂 Projets/SyncGDrive/` → `📁 src/` |
| Tooltip transfert | `rapport.pdf (4 Ko)` | `📂 Documents/Travail/` → `📄 rapport.pdf (4 Ko)` |
| Fenêtre scan initial | Idem tooltip | Deux lignes par bloc (parent + courant) |
| Notification erreur | `Erreur sur main.rs` | `Erreur sur src/main.rs` (chemin relatif complet) |

#### Fonction utilitaire `split_path_display()`

```rust
/// Sépare un chemin relatif en (répertoire_parent, nom_élément).
/// Ex: "src/engine/scan.rs" → ("src/engine/", "scan.rs")
/// Ex: "src/engine/"        → ("src/", "engine/")
/// Ex: "README.md"          → ("", "README.md")
fn split_path_display(relative: &str) -> (&str, &str) {
    // ...
}
```

#### Tooltip V2 — Exemples comparés

**Transfert en cours** :
```
V1:  Transfert 12/156 — rapport.pdf (4,2 Mo)

V2:  Transfert 12/156
     📂 Documents/Travail/
     📄 rapport.pdf (4,2 Mo)
     [████████████░░░░░░░░] 62% · 2,6 Mo/s
```

**Scan — Inventaire local** :
```
V1:  Analyse locale (4521 éléments indexés) — scan.rs

V2:  Analyse locale — 4 521 / 12 300
     📂 src/engine/
     📄 scan.rs
```

**Scan — Création dossiers** :
```
V1:  Création dossiers 120/450 — src

V2:  Création dossiers — 120 / 450
     📂 Projets/SyncGDrive/
     📁 src/
```

**Idle (dernier transfert)** :
```
V1:  ✅ Dernier transfert : rapport.pdf

V2:  ✅ Dernier transfert :
     📂 Documents/Travail/
     📄 rapport.pdf
```

> **Règle UX** : Les emojis `📂` (dossier ouvert = parent),
> `📁` (dossier fermé = dossier courant) et `📄` (fichier courant)
> permettent de distinguer visuellement les niveaux sans surcharger le texte.
> Les chemins sont toujours **relatifs à `local_root`** (pas de chemins absolus dans l'UI).

### 3.6 Résumé des Améliorations UX

| Amélioration | Impact utilisateur | Effort |
|---|---|---|
| Icônes SVG embarquées | Cohérence visuelle sur tous les DE | ~6 SVG + code de chargement |
| Animation systray | Feedback visuel immédiat « ça travaille » | Tâche Tokio + 4 frames SVG |
| Fenêtre scan initial | Rassure l'utilisateur, montre la progression détaillée | Fenêtre libadwaita + canal glib |
| Notification « prête » | L'utilisateur sait quand il peut travailler | ~20 lignes dans `notif.rs` |
| Lisibilité des chemins | L'utilisateur sait toujours où il se trouve | `split_path_display()` + refacto tooltip |

---

## 4. Améliorations Moteur (V2)

### 4.1 Progression en Octets et Vitesse de Transfert

**Problème V1** : La progression est uniquement en nombre de fichiers (X/Y). Aucune information sur la vitesse ou le temps restant.

**Solution V2** : Avec `reqwest`, on contrôle le flux HTTP et on peut compter les octets envoyés en temps réel.

```rust
pub enum EngineStatus {
    // ...existing...
    ScanProgress {
        phase: ScanPhase,
        done: usize,
        total: usize,
        current_dir: String,    // NOUVEAU : répertoire parent (ex: "src/engine/")
        current_name: String,   // RENOMMÉ : nom de l'élément (ex: "scan.rs")
    },
    SyncProgress {
        done: usize,
        total: usize,
        current_dir: String,    // NOUVEAU : répertoire parent
        current_name: String,   // RENOMMÉ : nom du fichier
        size_bytes: u64,
        bytes_sent: u64,        // NOUVEAU : octets envoyés pour le fichier courant
        total_bytes: u64,       // NOUVEAU : total octets de tous les fichiers à transférer
        total_bytes_sent: u64,  // NOUVEAU : octets envoyés au total
        speed_bps: u64,         // NOUVEAU : vitesse instantanée (octets/seconde)
    },
}
```

**Affichage tooltip** :
```
Transfert 12/156
📂 Documents/Travail/
📄 rapport.pdf (4,2 Mo)
[████████████░░░░░░░░] 62% · 2,6 Mo/s · ~3 min restantes
Total : 128 Mo / 512 Mo
```

### 4.2 Limitation de Bande Passante

Permet à l'utilisateur de limiter la vitesse d'upload pour ne pas saturer sa connexion.

```toml
[advanced]
# Limite d'upload en Ko/s. 0 = illimité. Défaut : 0
upload_limit_kbps = 0
```

**Implémentation** : Token bucket ou simple `sleep` entre les chunks d'upload resumable.

### 4.3 Mode Hors-Ligne et Détection Réseau

**Problème V1** : Si le réseau tombe, les opérations KIO échouent en boucle avec retry.

**Solution V2** :
- Détecter la connectivité via `NetworkManager` D-Bus ou simple ping HTTPS vers `googleapis.com`.
- Passer en état `Offline` automatiquement (nouvelle icône nuage barré).
- Accumuler les changements locaux dans une queue persistante (DB).
- Reprendre automatiquement quand le réseau revient.
- **Pas de retry inutile** en mode offline = économie de batterie sur laptop.

### 4.4 Vérification d'Intégrité Post-Upload

**Problème** : Comment s'assurer que le fichier est bien arrivé intact sur Google Drive ?

**Solution** : Google Drive API v3 retourne un `md5Checksum` pour chaque fichier uploadé. Après chaque upload :
1. Comparer le MD5 retourné par Google avec le MD5 local.
2. Si mismatch → re-upload immédiat + log warning.
3. Stocker le checksum Drive en DB pour les vérifications futures.

### 4.5 Corbeille au Lieu de Suppression Définitive

**Problème V1** : `delete` supprime définitivement le fichier distant. Dangereux en cas de bug ou de fausse manipulation.

**Solution V2** : Par défaut, utiliser `files.update({ trashed: true })` au lieu de `files.delete()`. Configurable :

```toml
[advanced]
# Action lors de la suppression : "trash" (défaut) ou "delete" (permanent)
delete_mode = "trash"
```

### 4.6 Gestion des Liens Symboliques

**Problème V1** : Non documenté ni géré. Un symlink pourrait boucler ou pointer hors de `local_root`.

**Solution V2** :
- **Ignorer** les symlinks par défaut (sécurité).
- Option configurable pour les suivre (follow) avec détection de boucle.

```toml
[advanced]
# Gestion des symlinks : "ignore" (défaut), "follow"
symlink_mode = "ignore"
```

### 4.7 Gestion des Rate Limits Google API

Google Drive API impose des quotas :
- 12 000 requêtes/utilisateur/100 secondes (défaut)
- 750 Go d'upload/jour/compte

**Solution** : Implémenter un rate limiter interne (`governor` crate ou simple token bucket) qui :
- Respecte les headers `Retry-After` sur les réponses 429.
- Pré-limite les requêtes pour ne jamais atteindre le quota.
- Configurable :

```toml
[advanced]
# Requêtes max par seconde vers l'API Google. Défaut : 10
api_rate_limit_rps = 10
```

### 4.8 Cache Path → ID Google Drive

**Problème** : Google Drive utilise des **IDs de fichier**, pas des chemins. Chaque opération nécessite de résoudre `/Projets/SyncGDrive/src/main.rs` en un ID comme `1BxiMVs0XRA5nFMdKvBdBZjgmUUqptlbs07EVGka`.

**Solution** : Table de cache persistante en SQLite :

```sql
CREATE TABLE path_cache (
    relative_path TEXT PRIMARY KEY,
    drive_id      TEXT NOT NULL,
    parent_id     TEXT NOT NULL,
    is_folder     BOOLEAN NOT NULL DEFAULT 0,
    updated_at    INTEGER NOT NULL
);
```

- Pré-rempli au scan initial via `files.list` récursif.
- Mis à jour à chaque création/rename/delete.
- Évite les résolutions path→ID coûteuses (1 requête API par segment de chemin).

### 4.9 Migration V1 → V2

La V2 doit pouvoir reprendre là où la V1 s'était arrêtée, sans re-scanner tout depuis zéro.

| Donnée V1 | Action V2 |
|-----------|-----------|
| `file_index` (path, sha256, mtime) | Conservé tel quel — compatible |
| `dir_index` (path) | Conservé — enrichi avec `drive_id` |
| `config.toml` V1 (single root) | Migration automatique vers `[[sync_pairs]]` unique |
| Pas de `path_cache` | Construit au premier scan V2 (one-time) |
| Pas de token OAuth2 | Wizard OAuth2 au premier lancement V2 |

Script de migration automatique au démarrage : détecte la version du schéma DB et migre si nécessaire.

### 4.10 Mode Dry-Run

Pour les utilisateurs prudents ou le debug :

```bash
SYNCGDRIVE_DRY_RUN=1 cargo run --features ui
```

- Exécute le scan complet et affiche ce qui **serait** synchronisé.
- Aucune écriture distante, aucune modification DB.
- Log de chaque action simulée à `info` level.

---

## 5. Architecture Logicielle

### A. Le Modèle Manager/Worker

Le moteur adopte une structure hiérarchique pour isoler les flux de données.

**SyncManager** :
- Lit le fichier de configuration.
- Initialise le thread unique GTK/Libadwaita pour l'UI.
- Pilote le cycle de vie des Workers (Démarrage, Arrêt, Rechargement).

**SyncWorker** :
- Une instance par paire de synchronisation.
- Tâche Tokio dédiée avec son propre `CancellationToken`.
- Watcher `notify` indépendant pour chaque dossier local.

### B. Le Trait `RemoteProvider` (ex-`KioOps`)

Le trait est refactorisé pour être 100% asynchrone et supporter le pooling de connexions.

```rust
#[async_trait]
pub trait RemoteProvider: Send + Sync {
    /// Upload d'un fichier avec gestion des flux (Resumable)
    async fn upload(&self, local_path: &Path, remote_id: &str) -> Result<UploadStatus>;

    /// Récupération des changements depuis le dernier curseur
    async fn get_changes(&self, cursor: Option<String>) -> Result<DeltaPage>;

    /// Validation des identifiants (OAuth2)
    async fn check_health(&self) -> Result<bool>;
}
```

---

## 6. Spécifications Techniques

### 6.1 Structure de Configuration V2 (`config.toml`)

```toml
# ══════════════════════════════════════════════════════════════
#  SyncGDrive V2 — Configuration
# ══════════════════════════════════════════════════════════════

# ── Paires de synchronisation ─────────────────────────────────

[[sync_pairs]]
name = "Documents Pro"
local_path = "/home/user/Documents"
remote_folder_id = "GD_ID_123"
provider = "GoogleDrive"
active = true

[[sync_pairs]]
name = "Backup Photos"
local_path = "/home/user/Pictures/Family"
remote_folder_id = "GD_ID_456"
provider = "GoogleDrive"
active = true

# ── Général ───────────────────────────────────────────────────

max_workers = 4
notifications = true
rescan_interval_min = 30

ignore_patterns = [
    "**/target/**",
    "**/.git/**",
    "**/node_modules/**",
]

# ── Retry ─────────────────────────────────────────────────────

[retry]
max_attempts = 3
initial_backoff_ms = 300
max_backoff_ms = 8000

# ── Avancé (valeurs par défaut sensibles) ─────────────────────

[advanced]
debounce_ms = 500
health_check_interval_secs = 30
max_concurrent_ls = 8
shutdown_timeout_secs = 3
log_retention_days = 7
engine_channel_capacity = 32
notification_timeout_ms = 6000
resumable_upload_threshold = 5242880
```

> **Note** : La V2 reste en TOML (pas JSON) — cohérent avec l'écosystème Rust
> (`serde` + `toml`) et la V1. Les `[[sync_pairs]]` utilisent la syntaxe
> TOML native pour les tableaux d'objets.

### 6.2 Protocole de Transfert Natif

- **Reqwest Client** : Un seul client HTTP partagé entre tous les workers pour réutiliser les connexions TCP (Keep-Alive).
- **Resumable Uploads** : Pour les fichiers dépassant `advanced.resumable_upload_threshold`, utilisation du protocole d'upload fractionné de Google pour survivre aux coupures réseau.
- **API Delta** : Utilisation de `changes.list` au lieu de `files.list` pour ne récupérer que les modifications côté Drive.

---

## 7. Interface Utilisateur (V2 UI)

Le `tray.rs` actuel évolue vers une vue agrégée.

- **Menu contextuel multi-lignes** : Chaque paire de synchronisation affiche son propre statut et sa progression.
- **Wizard OAuth2** : Intégration du flux d'autorisation Libadwaita (Webview ou code de copie) pour lier chaque dossier à un compte Google sans ligne de commande.
- **Reporting d'erreurs** : Isolation visuelle des erreurs (ex : seul un dossier affiche une icône d'alerte si ses permissions sont invalides).
- **Section Avancé dans Settings** : Les paramètres `[advanced]` sont exposés dans un groupe repliable, avec des valeurs par défaut pré-remplies.
- **Fenêtre de scan initial** : Voir §3.3 — progression détaillée au premier lancement.
- **Icônes SVG personnalisées** : Voir §3.1 — cohérence visuelle sur tous les environnements.
- **Animation systray** : Voir §3.2 — feedback visuel pendant les transferts.

---

## 8. Déploiement et Maintenance (Plan d'Excellence Arch Linux)

### A. Systemd User Service

Le service sera installé en tant que `syncgdrive.service` dans `~/.config/systemd/user/`.

- `Restart=always` avec délai exponentiel.
- `ConditionEnvironment=GRAPHICAL_SESSION` pour garantir l'accès au Tray.

### B. Optimisation Système

Un script d'installation vérifiera et proposera d'augmenter les limites inotify si nécessaire :

```bash
# Exemple de réglage d'excellence pour Arch
echo "fs.inotify.max_user_watches=524288" | sudo tee /etc/sysctl.d/99-syncgdrive.conf
```

---

## 9. Prochaines Étapes Immédiates

| Phase | Description |
|-------|-------------|
| **Phase 1** | Migration config TOML V2 (`AdvancedConfig`, `[[sync_pairs]]`) + migration DB schéma |
| **Phase 2** | Module `auth` (OAuth2 loopback + stockage sécurisé `secret-service`) |
| **Phase 3** | Trait `RemoteProvider` + implémentation Google Drive (`reqwest`, path→ID cache, resumable upload) |
| **Phase 4** | Remplacement de toutes les constantes hardcodées par `AppConfig` |
| **Phase 5** | Progression en octets, vitesse, ETA + limitation bande passante |
| **Phase 6** | Mode hors-ligne, vérification intégrité, corbeille, rate limiter |
| **Phase 7** | Icônes SVG + animation systray + fenêtre scan initial |
| **Phase 8** | Mode dry-run + tests d'intégration |
| **Phase 9** | Packaging (PKGBUILD, .deb, PPA) |
