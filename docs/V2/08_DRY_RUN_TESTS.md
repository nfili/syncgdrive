# Phase 8 — Mode Dry-Run + Tests d'Intégration

---

## 1. Objectif

Ajouter un mode dry-run pour inspecter sans modifier, et écrire les tests d'intégration end-to-end qui valident l'ensemble des phases 1 à 7 fonctionnant ensemble.

---

## 2. Pré-requis

- **Phases 1–7** : Toutes implémentées.

---

## 3. Fichiers Impactés

| Action | Fichier | Description |
|--------|---------|-------------|
| **Modifier** | `src/engine/mod.rs` | Flag `dry_run`, propagation aux workers |
| **Modifier** | `src/engine/worker.rs` | Skip les opérations remote en dry-run |
| **Modifier** | `src/engine/scan.rs` | Log des actions simulées |
| **Modifier** | `src/remote/gdrive.rs` | Pas d'appel API en dry-run (retour mock) |
| **Modifier** | `src/main.rs` | Lecture env `SYNCGDRIVE_DRY_RUN` |
| **Créer** | `tests/integration/` | Tests d'intégration end-to-end |
| **Créer** | `tests/integration/helpers.rs` | Fixtures, mock provider, temp dirs |
| **Créer** | `tests/integration/test_scan.rs` | Test scan complet |
| **Créer** | `tests/integration/test_watcher.rs` | Test watcher + debounce |
| **Créer** | `tests/integration/test_offline.rs` | Test cycle offline/online |
| **Créer** | `tests/integration/test_migration.rs` | Test migration V1→V2 |
| **Créer** | `tests/integration/test_config.rs` | Test config V2 complète |

---

## 4. Spécification Détaillée

### 4.1 Mode Dry-Run

**Activation** :
```bash
# Via variable d'environnement
SYNCGDRIVE_DRY_RUN=1 syncgdrive

# Ou via argument CLI (futur)
syncgdrive --dry-run
```

**Comportement** :

| Opération | Mode normal | Mode dry-run |
|-----------|-------------|-------------|
| Scan local (WalkDir) | ✅ Exécuté | ✅ Exécuté |
| Scan remote (API) | ✅ Exécuté | ✅ Exécuté (lecture seule) |
| Diff (calcul des actions) | ✅ Exécuté | ✅ Exécuté |
| Upload fichier | ✅ Exécuté | ❌ Skippé — log `info!` |
| Mkdir remote | ✅ Exécuté | ❌ Skippé — log `info!` |
| Delete remote | ✅ Exécuté | ❌ Skippé — log `info!` |
| Rename remote | ✅ Exécuté | ❌ Skippé — log `info!` |
| Update DB | ✅ Exécuté | ❌ Skippé |
| Watcher inotify | ✅ Actif | ❌ Désactivé |

**Logs dry-run** :
```
[DRY-RUN] upload: src/engine/scan.rs (45 Ko) → GD:AAA/engine/
[DRY-RUN] mkdir: src/ui/icons/
[DRY-RUN] delete: old_file.rs (remote: BBB)
[DRY-RUN] rename: temp.rs → final.rs
```

**Résumé final** :
```
=== DRY-RUN SUMMARY ===
Files to upload:  42 (128 Mo)
Dirs to create:   8
Files to delete:  3
Files to rename:  1
Total operations: 54
```

### 4.2 Implémentation dans le Trait

```rust
pub struct SyncEngine {
    // ...existing...
    dry_run: bool,
}

// Dans worker.rs
async fn handle(task: Task, ..., dry_run: bool) -> Result<()> {
    if dry_run {
        match &task {
            Task::SyncFile(path) => {
                let size = tokio::fs::metadata(path).await?.len();
                info!("[DRY-RUN] upload: {} ({})", path.display(), human_size(size));
            }
            Task::Delete(path) => {
                info!("[DRY-RUN] delete: {}", path.display());
            }
            Task::Rename { from, to } => {
                info!("[DRY-RUN] rename: {} → {}", from.display(), to.display());
            }
        }
        return Ok(());
    }
    // ... exécution normale
}
```

### 4.3 Tests d'Intégration

**Architecture des tests** :

```
tests/
└── integration/
    ├── helpers.rs          # MockProvider, TempDir, fixtures
    ├── test_scan.rs        # Scan end-to-end
    ├── test_watcher.rs     # Watcher + debounce
    ├── test_offline.rs     # Cycle offline/online
    ├── test_migration.rs   # V1 → V2
    └── test_config.rs      # Config V2 complète
```

**`MockProvider`** : Implémentation de `RemoteProvider` en mémoire.

```rust
pub struct MockProvider {
    files: Arc<Mutex<HashMap<String, MockFile>>>,
    dirs: Arc<Mutex<HashMap<String, String>>>,  // path → id
    next_id: AtomicU64,
    calls: Arc<Mutex<Vec<MockCall>>>,  // historique des appels
}

pub enum MockCall {
    Upload { path: String, parent_id: String },
    Delete { id: String },
    Mkdir { parent_id: String, name: String },
    Rename { id: String, new_name: Option<String> },
    ListRemote { root_id: String },
}
```

---

## 5. Tests d'Intégration — Liste

### `test_scan.rs`

| Test | Description |
|------|-------------|
| `test_initial_scan_uploads_all` | Dossier local avec 10 fichiers, DB vide → 10 uploads |
| `test_rescan_skips_unchanged` | Rescan après sync → 0 uploads |
| `test_scan_detects_new_file` | Ajouter 1 fichier → 1 upload |
| `test_scan_detects_modified_file` | Modifier 1 fichier → 1 upload |
| `test_scan_detects_deleted_file` | Supprimer 1 fichier local → 1 delete remote |
| `test_scan_creates_directories` | Arborescence locale → mkdir récursifs |
| `test_scan_ignores_patterns` | Fichier `.git/config` → ignoré |
| `test_scan_handles_empty_dir` | Dossier vide → mkdir mais pas d'upload |

### `test_watcher.rs`

| Test | Description |
|------|-------------|
| `test_watcher_detects_new_file` | Créer un fichier → événement Modified → upload |
| `test_watcher_detects_delete` | Supprimer un fichier → événement Deleted → delete remote |
| `test_watcher_debounce` | 5 modifications rapides → 1 seul upload |
| `test_watcher_rename_within` | Rename dans l'arbre → Renamed → rename remote |
| `test_watcher_rename_from_outside` | Fichier arrive de dehors → Modified → upload |

### `test_offline.rs`

| Test | Description |
|------|-------------|
| `test_offline_queues_events` | Passer offline → modifier fichier → queue contient Sync |
| `test_online_flushes_queue` | Retour online → queue vidée → uploads exécutés |
| `test_offline_dedup` | 3 modifs même fichier offline → 1 seul Sync au flush |

### `test_migration.rs`

| Test | Description |
|------|-------------|
| `test_config_v1_migrated` | Config V1 → V2 avec 1 sync_pair |
| `test_config_v1_backup_created` | Fichier `.v1.bak` existe après migration |
| `test_db_v1_migrated` | DB V1 → tables V2 ajoutées |
| `test_db_migration_idempotent` | Double migration → pas d'erreur |

### `test_config.rs`

| Test | Description |
|------|-------------|
| `test_full_config_roundtrip` | Sérialiser → désérialiser → identique |
| `test_partial_advanced_defaults` | `[advanced]` partiel → défauts appliqués |
| `test_multi_sync_pairs` | 3 paires → toutes chargées |
| `test_ignore_patterns_merge` | Globaux + par paire = union |

---

## 6. Critères d'Acceptation

- [ ] `SYNCGDRIVE_DRY_RUN=1` affiche toutes les actions sans exécuter
- [ ] Le résumé dry-run est affiché à la fin (fichiers, dossiers, taille)
- [ ] Aucune écriture DB ni API en dry-run
- [ ] Le watcher est désactivé en dry-run
- [ ] Tous les tests d'intégration passent
- [ ] `MockProvider` couvre toutes les méthodes du trait
- [ ] Les tests sont reproductibles (temp dirs, pas de state partagé)
- [ ] `cargo test` (unitaires + intégration) : tous passent
- [ ] `cargo clippy` : 0 warning

