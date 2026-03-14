# Phase 1 — Migration Config TOML V2 + Schéma DB

---

## 1. Objectif

Faire évoluer la configuration de V1 (single root, constantes en dur) vers V2 (multi-sync, section `[advanced]`, migration automatique). C'est le socle de toutes les autres phases.

---

## 2. Pré-requis

- Aucun — c'est la première phase.

---

## 3. Fichiers Impactés

| Action | Fichier | Description |
|--------|---------|-------------|
| **Modifier** | `src/config.rs` | Nouvelle structure `AppConfigV2`, `SyncPair`, `AdvancedConfig` |
| **Modifier** | `src/db.rs` | Table `schema_version`, migration `path_cache`, enrichir `dir_index` |
| **Modifier** | `src/main.rs` | Lecture config V2, migration auto au démarrage |
| **Modifier** | `src/ui/settings.rs` | UI multi-sync (liste des paires) + section Avancé |
| **Créer** | `src/migration.rs` | Logique de migration V1→V2 (config + DB) |

---

## 4. Structures de Données

### 4.1 Config TOML V2

```rust
/// Configuration principale V2.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// Paires de synchronisation (remplace local_root + remote_root).
    #[serde(default)]
    pub sync_pairs: Vec<SyncPair>,

    #[serde(default = "default_max_workers")]
    pub max_workers: usize,

    #[serde(default)]
    pub notifications: bool,

    #[serde(default = "default_rescan_interval_min")]
    pub rescan_interval_min: u64,

    #[serde(default = "default_ignore_patterns")]
    pub ignore_patterns: Vec<String>,

    #[serde(default)]
    pub retry: RetryConfig,

    #[serde(default)]
    pub advanced: AdvancedConfig,
}

/// Une paire de synchronisation local → remote.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncPair {
    /// Nom affiché dans l'UI (ex: "Documents Pro").
    pub name: String,
    /// Chemin local absolu.
    pub local_path: PathBuf,
    /// ID du dossier racine sur Google Drive.
    pub remote_folder_id: String,
    /// Type de provider (pour le futur multi-backend).
    #[serde(default = "default_provider")]
    pub provider: String,
    /// Paire active ou désactivée.
    #[serde(default = "default_true")]
    pub active: bool,
    /// Patterns d'exclusion spécifiques à cette paire (en plus des globaux).
    #[serde(default)]
    pub ignore_patterns: Vec<String>,
}

/// Paramètres avancés avec valeurs par défaut sensibles.
/// L'utilisateur n'a jamais besoin de toucher à cette section.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdvancedConfig {
    #[serde(default = "default_debounce_ms")]
    pub debounce_ms: u64,                    // 500

    #[serde(default = "default_health_check_secs")]
    pub health_check_interval_secs: u64,     // 30

    #[serde(default = "default_max_concurrent_ls")]
    pub max_concurrent_ls: usize,            // 8

    #[serde(default = "default_shutdown_timeout")]
    pub shutdown_timeout_secs: u64,          // 3

    #[serde(default = "default_log_retention")]
    pub log_retention_days: u64,             // 7

    #[serde(default = "default_channel_capacity")]
    pub engine_channel_capacity: usize,      // 32

    #[serde(default = "default_notif_timeout")]
    pub notification_timeout_ms: i32,        // 6000

    #[serde(default = "default_resumable_threshold")]
    pub resumable_upload_threshold: u64,     // 5_242_880 (5 Mo)

    #[serde(default)]
    pub upload_limit_kbps: u64,              // 0 = illimité

    #[serde(default = "default_api_rate_limit")]
    pub api_rate_limit_rps: u32,             // 10

    #[serde(default = "default_delete_mode")]
    pub delete_mode: String,                 // "trash"

    #[serde(default = "default_symlink_mode")]
    pub symlink_mode: String,                // "ignore"
}
```

### 4.2 Valeurs par Défaut

```rust
fn default_provider() -> String { "GoogleDrive".into() }
fn default_true() -> bool { true }
fn default_debounce_ms() -> u64 { 500 }
fn default_health_check_secs() -> u64 { 30 }
fn default_max_concurrent_ls() -> usize { 8 }
fn default_shutdown_timeout() -> u64 { 3 }
fn default_log_retention() -> u64 { 7 }
fn default_channel_capacity() -> usize { 32 }
fn default_notif_timeout() -> i32 { 6000 }
fn default_resumable_threshold() -> u64 { 5_242_880 }
fn default_api_rate_limit() -> u32 { 10 }
fn default_delete_mode() -> String { "trash".into() }
fn default_symlink_mode() -> String { "ignore".into() }
```

### 4.3 Schéma DB V2

```sql
-- Nouvelle table : version du schéma pour migrations automatiques
CREATE TABLE IF NOT EXISTS schema_version (
    version INTEGER NOT NULL
);

-- Table existante enrichie (V1 → V2)
-- dir_index : ajout colonne drive_id (nullable au départ, remplie par Phase 3)
ALTER TABLE dir_index ADD COLUMN drive_id TEXT;

-- Nouvelle table : cache path → ID Google Drive (Phase 3, mais schéma créé ici)
CREATE TABLE IF NOT EXISTS path_cache (
    relative_path TEXT PRIMARY KEY,
    drive_id      TEXT NOT NULL,
    parent_id     TEXT NOT NULL,
    is_folder     INTEGER NOT NULL DEFAULT 0,
    updated_at    INTEGER NOT NULL
);

-- Nouvelle table : queue offline (Phase 6, mais schéma créé ici)
CREATE TABLE IF NOT EXISTS offline_queue (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    action        TEXT NOT NULL,   -- "sync", "delete", "rename"
    relative_path TEXT NOT NULL,
    extra         TEXT,            -- JSON pour les détails (rename: from/to, etc.)
    created_at    INTEGER NOT NULL
);
```

---

## 5. Spécification Détaillée

### 5.1 Migration Config V1 → V2

Au démarrage, `AppConfig::load_or_create()` doit :

1. Tenter de désérialiser comme V2 (`sync_pairs` présent).
2. Si échec ou `sync_pairs` vide → tenter de lire les champs V1 (`local_root`, `remote_root`).
3. Si V1 détecté → convertir automatiquement :
   ```
   local_root = "/home/user/Projets"
   remote_root = "gdrive:/MonDrive/Backup"
   →
   [[sync_pairs]]
   name = "Sync principal"
   local_path = "/home/user/Projets"
   remote_folder_id = ""     # À renseigner via wizard OAuth2
   provider = "GoogleDrive"
   active = true
   ```
4. Sauvegarder le fichier converti (backup de l'ancien en `config.toml.v1.bak`).
5. Log `info!("migration config V1 → V2 effectuée")`.

### 5.2 Migration DB V1 → V2

Dans `Database::init_schema()` :

1. Vérifier si `schema_version` existe.
2. Si non → créer la table, insérer `version = 2`.
3. Si oui → lire la version.
4. Si version < 2 → exécuter les migrations séquentiellement.
5. Chaque migration est une transaction atomique.

```rust
pub fn migrate(&self) -> Result<()> {
    let version = self.schema_version()?;
    if version < 2 {
        self.migrate_v1_to_v2()?;
    }
    // Futures migrations : if version < 3 { ... }
    Ok(())
}
```

### 5.3 Validation Config V2

```rust
pub fn validate(&self) -> Result<(), ConfigError> {
    if self.sync_pairs.is_empty() {
        return Err(ConfigError::NoPairsConfigured);
    }
    for (i, pair) in self.sync_pairs.iter().enumerate() {
        if pair.local_path.as_os_str().is_empty() {
            return Err(ConfigError::PairLocalEmpty(i));
        }
        if pair.active && !pair.local_path.is_dir() {
            return Err(ConfigError::PairLocalMissing(i, pair.local_path.clone()));
        }
        if pair.name.is_empty() {
            return Err(ConfigError::PairNameEmpty(i));
        }
        // remote_folder_id peut être vide → wizard OAuth2 le remplira
    }
    Ok(())
}
```

### 5.4 Rétro-compatibilité

- Un fichier `config.toml` V1 pur doit être lisible et migré silencieusement.
- Un fichier `config.toml` V2 ne doit pas être cassé par une V1 (la V1 ne sera plus exécutée après migration).
- Le champ `remote_root` V1 (URL KIO `gdrive:/…`) n'est plus utilisé en V2 (on utilise `remote_folder_id`). Le wizard OAuth2 (Phase 2) le remplacera.

---

## 6. Cas Limites

| Cas | Comportement attendu |
|-----|---------------------|
| Config V1 avec `local_root` vide | Migration V2 avec paire inactive + `Unconfigured` |
| Config V2 avec 0 paires | `Unconfigured` → ouvrir Settings |
| Config V2 avec toutes les paires désactivées | `Idle` silencieux (rien à surveiller) |
| Fichier config corrompu (TOML invalide) | Backup + recréation par défaut + notification erreur |
| DB V1 sans `schema_version` | Création table + migration V2 |
| DB V2 avec version future (>2) | Log warning + continuer (forward-compatible) |
| `[advanced]` absent du TOML | Tous les défauts s'appliquent via `#[serde(default)]` |
| `ignore_patterns` globaux + par paire | Merge au runtime : globaux ∪ spécifiques |

---

## 7. Tests à Écrire

### Unitaires (`config.rs`)

- `test_deserialize_v2_config` : TOML V2 complet → struct correcte
- `test_deserialize_v1_config_migrates` : TOML V1 → migration auto en V2
- `test_default_advanced_values` : `AdvancedConfig::default()` a les bonnes valeurs
- `test_validate_empty_pairs` : 0 paires → erreur
- `test_validate_inactive_pair_missing_dir` : paire inactive avec dir manquant → OK
- `test_validate_active_pair_missing_dir` : paire active avec dir manquant → erreur
- `test_merge_ignore_patterns` : globaux + spécifiques = union sans doublons
- `test_advanced_partial_override` : seuls les champs présents overrident les défauts

### Unitaires (`db.rs`)

- `test_schema_version_initial` : DB vierge → version 2
- `test_migration_v1_to_v2` : DB V1 → tables ajoutées, version mise à jour
- `test_migration_idempotent` : double migration → pas d'erreur
- `test_path_cache_crud` : insert, get, update, delete sur `path_cache`
- `test_offline_queue_fifo` : insertion et lecture ordonnée

### Unitaires (`migration.rs`)

- `test_config_backup_created` : fichier `.v1.bak` créé après migration
- `test_config_v1_fields_mapped` : `local_root` → `sync_pairs[0].local_path`

---

## 8. Critères d'Acceptation

- [ ] `cargo test` : tous les tests de la Phase 1 passent
- [ ] `cargo clippy --features ui -- -W clippy::all` : 0 warning
- [ ] Un `config.toml` V1 existant est migré automatiquement au démarrage
- [ ] Le backup `config.toml.v1.bak` est créé
- [ ] Un `config.toml` V2 avec `[advanced]` absent fonctionne (défauts)
- [ ] Un `config.toml` V2 avec `[advanced]` partiel fonctionne (merge)
- [ ] La DB est migrée automatiquement (tables V2 créées)
- [ ] `schema_version` est correctement géré
- [ ] L'UI Settings affiche la liste des paires (même si le wizard OAuth2 n'est pas encore fait)
- [ ] Le moteur fonctionne avec la nouvelle config (même comportement que V1 pour une paire unique)

