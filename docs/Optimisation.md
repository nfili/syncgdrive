# SyncGDrive — Optimisations (2026-03-13)

## 1. Correction du Panic Tokio ✅

**Problème** : `Cannot start a runtime from within a runtime` — `notify-rust` 4.x
appelle `zbus::block_on()` en interne dans `Notification::show()`. Si on est sur un
worker Tokio, `block_on` panic.

**Correctifs appliqués** :

| Fichier | Action |
|---|---|
| `notif.rs` | Chaque notification est envoyée depuis un `std::thread::spawn` dédié (pas de contexte Tokio → `block_on` fonctionne) |
| `scan.rs` | `db.all_paths()` (potentiellement lourd) enveloppé dans `tokio::task::spawn_blocking` pour ne pas bloquer les workers async |
| `db.rs` | Les opérations unitaires (`get`, `upsert`, `delete`) restent synchrones derrière `Arc<Mutex>` — elles sont < 1ms en WAL mode et l'overhead de `spawn_blocking` serait contre-productif |

**Règle architecturale** : Toute opération I/O bloquante > 1ms doit passer par
`spawn_blocking`. Les micro-opérations SQLite restent inline.

## 2. Optimisation des Uploads ✅

**Problème** : Un processus `kioclient5` par fichier = overhead massif (fork, handshake,
latence réseau).

**Correctifs appliqués** :

| Paramètre | Avant | Après | Effet |
|---|---|---|---|
| `max_workers` par défaut | 2 | 4 | Sature mieux la bande passante sans atteindre le rate-limit GDrive (429) |
| Logs per-file (`synced`, `deleted`, `renamed`) | `info!` | `debug!` | Console propre avec 4+ workers en parallèle |
| Logs phases scan (`dirs OK`, `comparaison terminée`) | `info!` | `info!` (inchangé) | Résumé visible sans bruit |

**Note** : L'utilisateur peut monter `max_workers` jusqu'à 12–16 dans le
`config.toml`. Au-delà, risque de rate-limit Google Drive (erreur 429) ou
d'épuisement des descripteurs de fichiers.

**Recommandation pour les gros dépôts** (> 5000 fichiers) :
```toml
# ~/.config/syncgdrive/config.toml
max_workers = 8
```

## 3. Préparation pour la V2 (Trait KioOps) ✅

**Objectif** : Permettre le remplacement de `kioclient5` par un backend API natif
(pool de connexions HTTP Keep-Alive) sans toucher au moteur.

**État** : Déjà en place depuis la conception initiale.

```
┌─────────────┐     ┌─────────────┐     ┌──────────────────┐
│ SyncEngine  │────▶│ KioOps      │◀────│ KioClient        │ (kioclient5)
│ scan.rs     │     │ (trait)     │     └──────────────────┘
│ worker.rs   │     │             │◀────┌──────────────────┐
│ watcher.rs  │     │ ls_remote   │     │ HttpBackend      │ (V2 — API native)
└─────────────┘     │ mkdir_p     │     └──────────────────┘
                    │ copy_file   │◀────┌──────────────────┐
                    │ delete      │     │ RcloneBackend    │ (V3 — rclone)
                    │ rename      │     └──────────────────┘
                    │ terminate   │
                    └─────────────┘
```

**Trait `KioOps`** (src/kio.rs) :
```rust
#[async_trait]
pub trait KioOps: Clone + Send + Sync + 'static {
    async fn ls_remote(&self, remote_root: &str) -> Result<HashSet<String>>;
    async fn mkdir_p(&self, remote_root: &str, rel: &Path) -> Result<()>;
    async fn copy_file(&self, local: &Path, remote: &str) -> Result<()>;
    async fn mkdir_if_absent(&self, remote: &str, cache: &HashSet<String>) -> Result<()>;
    async fn copy_file_smart(&self, local: &Path, remote: &str, cache: &HashSet<String>) -> Result<()>;
    async fn delete(&self, remote: &str) -> Result<()>;
    async fn rename(&self, from: &str, to: &str) -> Result<()>;
    async fn terminate_all(&self);
}
```

**Pour brancher un nouveau backend** :
1. Implémenter le trait `KioOps` pour le nouveau backend.
2. Dans `SyncEngine::run()`, instancier le nouveau backend au lieu de `KioClient`.
3. Le moteur (scan, watcher, worker, retry, DB) reste inchangé.

## Résumé des fichiers modifiés

| Fichier | Changement |
|---|---|
| `config.rs` | `default_max_workers` : 2 → 4 |
| `db.rs` | Table `dir_index` + méthodes `insert_dir`, `has_dir`, `all_dir_paths`, `clear_dirs`, `insert_dirs_batch` |
| `engine/scan.rs` | `db.all_paths()` → `spawn_blocking` ; cache `known_remote` anti-doublon ; préchargement `dir_index` DB ; batch persist des dossiers |
| `engine/mod.rs` | `db.clear_dirs()` ajouté à côté de `db.clear()` sur changement de `local_root` |
| `engine/worker.rs` | Logs per-file : `info!` → `debug!` |
| `kio.rs` | `mkdir_if_absent` : suppression du `stat` redondant (le remote index BFS est fiable) |
| `ui/tray.rs` | `handle.update()` pour rafraîchissement systray temps réel |
| `main.rs` | `status_rx` passé directement à `spawn_tray` |

## 4. Suppression du `stat` redondant dans `mkdir_if_absent` ✅

**Problème** : pendant le scan initial, le remote index BFS est déjà complet.
Pourtant, `mkdir_if_absent` faisait un `kioclient5 stat` avant chaque `mkdir` pour
les dossiers nouveaux — **doublant** le nombre d'appels réseau pour chaque dossier.

**Correctif** : suppression du `self.exists()` (= `stat`) avant `mkdir`. Le remote
index est la source de vérité pendant le scan. Le `stat` n'est conservé qu'en
**fallback** si le `mkdir` échoue (latence GDrive).

| Avant | Après | Gain |
|---|---|---|
| `stat` + `mkdir` par dossier nouveau | `mkdir` seul | ÷2 appels KIO par dossier |
| ~5s par dossier (2 appels réseau) | ~2.5s par dossier | 148 dossiers : ~6 min → ~3 min |

**Note** : `mkdir_p` (utilisé par le watcher en temps réel) conserve ses `stat`
car il n'a pas d'index pré-construit.

## 5. Progression fichier par fichier (`SyncProgress`) ✅

**Problème** : le tooltip affichait « 3 transfert(s) en cours » (`Syncing { active }`)
sans aucune barre de progression ni nom de fichier.

**Correctif** : compteurs atomiques `total_queued` / `total_done` dans la boucle
principale de `engine/mod.rs`. `SyncProgress { done, total, current, size_bytes }`
est envoyé :
1. À la **réception** de chaque task (nom + taille du fichier)
2. À la **fin** de chaque worker (done incrémenté)
3. Compteurs **remis à 0** avant chaque nouveau scan

Le tooltip affiche maintenant :
```
Envoi en cours : 45% [████░░░░░░]
Fichier : rapport.pdf
Poids : 4.2 Mo (337 / 751 fichiers)
```

## 6. Cache persistant des dossiers (`dir_index`) ✅

**Problème** : À chaque redémarrage, le scan initial Phase 2 itérait sur TOUS les
dossiers locaux et faisait un `stat` + `mkdir` par dossier manquant du `remote_index`
BFS. Pour un projet avec 148 dossiers, cela pouvait représenter +150 appels réseau
même si tous les dossiers existaient déjà depuis le run précédent.

**Correctif** : Nouvelle table SQLite `dir_index` (chemin relatif comme PRIMARY KEY).

| Opération | Fonction DB | Usage |
|---|---|---|
| Batch insert | `insert_dirs_batch` | Fin du scan Phase 2 (transaction unique) |
| Lecture complète | `all_dir_paths` | Début du scan Phase 2 (préchargement dans `known_remote`) |
| Purge | `clear_dirs` | Changement de `local_root` (avec `clear()`) |
| Lookup unitaire | `has_dir` | Disponible pour le watcher (futur) |

**Flux Phase 2 optimisé** :

```
1. Charger dir_index DB → injecter dans known_remote HashSet
2. Charger remote_index BFS → fusionner dans known_remote
3. Pour chaque dossier local :
   └── Si known_remote.contains(full_path) → SKIP (0 appel réseau)
   └── Sinon → mkdir_if_absent + enregistrer dans known_remote + new_dirs_for_db
4. Batch insert new_dirs_for_db → dir_index DB
5. Batch insert dossiers remote non-encore en DB → dir_index DB
```

| Avant | Après | Gain |
|---|---|---|
| Phase 2 : N `stat` par dossier (même si déjà existant) | Phase 2 : 0 appel réseau pour les dossiers connus en DB | ~0s pour les dossiers déjà sync |
| 1er run : N `mkdir` + N `stat` | 1er run : N `mkdir` (inchangé) + persist en DB | Identique, mais persisté pour les runs suivants |
| 2ème run+ : N `stat` (remote_index parfois incomplet) | 2ème run+ : lookup DB O(1) + remote_index | Skip quasi-total |

**Note** : `clear_dirs()` est appelé aux mêmes endroits que `clear()` dans
`engine/mod.rs` (changement de `local_root`). Les deux tables sont purgées ensemble.

## 7. Contournements des bugs kioclient5 (V1) ✅

`kioclient5` est le backend KIO de la V1. Il présente plusieurs limitations connues
qui sont contournées dans le code. **La V2 remplacera kioclient5 par un backend API
natif Google Drive** (via `trait KioOps`, déjà en place — cf. §3).

| Bug kioclient5 | Impact | Contournement V1 |
|---|---|---|
| `copy` fichier 0 octet → exit=0 mais rien créé | Fichier enregistré en DB mais absent du Drive | `worker::sync_file` skip les fichiers vides (`file_size == 0`) |
| Espaces dans les chemins → `beaucoup trop de paramètres` | `cat` (pipe stdin) échoue sur les chemins avec espaces | Abandon de `cat`, utilisation exclusive de `--overwrite copy` |
| `--overwrite` ignoré par certains backends KIO | Doublon créé au lieu d'écraser | Fallback `copy` sans `--overwrite` si le premier échoue |
| `rename`/`move` fichier temp (.part) inexistant | Retry inutile × 4 puis erreur | `worker::rename` vérifie la DB : si source absente → fallback `sync_file(to)` |
| Exit codes mensongers (exit=0 sur erreur) | Worker croit au succès | Pas de contournement générique possible sans API native (→ V2) |

### Roadmap V2 : backend API natif

Le trait `KioOps` permet de brancher un nouveau backend sans toucher au moteur :

```
V1 (actuel)  : KioClient    → kioclient5 subprocesses (1 fork/op, lent, bugs ci-dessus)
V2 (prévu)   : GDriveClient → Google Drive REST API (reqwest, HTTP/2 keep-alive, batch)
V3 (optionnel): RcloneBackend → rclone (multi-cloud, mature)
```

Gains attendus V2 :
- **÷10 latence** : pool HTTP keep-alive vs fork kioclient5 par opération
- **Fichiers vides** : `Files.create` API gère correctement les 0 octets
- **Exit codes fiables** : codes HTTP 2xx/4xx/5xx au lieu de exit codes KDE
- **Batch mkdir** : `Files.create(mimeType=folder)` en parallèle sans stat
- **Upload résumable** : reprise après coupure réseau pour les gros fichiers

