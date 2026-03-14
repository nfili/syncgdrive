# Phase 3 — Trait RemoteProvider + Backend Google Drive

---

## 1. Objectif

Remplacer `kioclient5` par un backend natif Google Drive REST API v3. Le trait `KioOps` est remplacé par `RemoteProvider`, plus riche et 100% async. Un seul client HTTP `reqwest` partagé avec pool de connexions Keep-Alive.

---

## 2. Pré-requis

- **Phase 1** : Config V2 (`SyncPair`, `AdvancedConfig`).
- **Phase 2** : OAuth2 fonctionnel (tokens disponibles).

---

## 3. Fichiers Impactés

| Action | Fichier | Description |
|--------|---------|-------------|
| **Créer** | `src/remote/mod.rs` | Trait `RemoteProvider`, types communs |
| **Créer** | `src/remote/gdrive.rs` | Implémentation Google Drive (reqwest) |
| **Créer** | `src/remote/path_cache.rs` | Cache path→ID SQLite |
| **Modifier** | `src/engine/scan.rs` | Utiliser `RemoteProvider` au lieu de `KioOps` |
| **Modifier** | `src/engine/worker.rs` | Utiliser `RemoteProvider` au lieu de `KioOps` |
| **Modifier** | `src/engine/mod.rs` | Instancier `GDriveProvider` au lieu de `KioClient` |
| **Modifier** | `src/db.rs` | CRUD `path_cache` |
| **Supprimer** | `src/kio.rs` | Remplacé par `src/remote/` (garder temporairement pour comparaison) |
| **Modifier** | `src/lib.rs` | `pub mod remote;` remplace `pub mod kio;` |
| **Modifier** | `Cargo.toml` | Dépendances : `reqwest` (rustls), `serde_json` |

---

## 4. Structures de Données

### 4.1 Trait `RemoteProvider`

```rust
#[async_trait]
pub trait RemoteProvider: Send + Sync {
    // ── Listing ──────────────────────────────────────────
    /// Liste récursive du contenu distant. Retourne les chemins relatifs.
    async fn list_remote(&self, root_id: &str) -> Result<RemoteIndex>;

    // ── Dossiers ─────────────────────────────────────────
    /// Crée un dossier s'il n'existe pas. Retourne l'ID.
    async fn mkdir(&self, parent_id: &str, name: &str) -> Result<String>;

    // ── Fichiers ─────────────────────────────────────────
    /// Upload un fichier (simple ou resumable selon la taille).
    async fn upload(
        &self,
        local_path: &Path,
        parent_id: &str,
        file_name: &str,
        existing_id: Option<&str>,  // None = create, Some = update (overwrite)
    ) -> Result<UploadResult>;

    /// Supprime ou met à la corbeille un fichier/dossier distant.
    async fn delete(&self, file_id: &str) -> Result<()>;

    /// Renomme ou déplace un fichier distant.
    async fn rename(
        &self,
        file_id: &str,
        new_name: Option<&str>,
        new_parent_id: Option<&str>,
    ) -> Result<()>;

    // ── Delta ────────────────────────────────────────────
    /// Récupère les changements depuis le dernier cursor (changes.list).
    async fn get_changes(&self, cursor: Option<&str>) -> Result<ChangesPage>;

    // ── Santé ────────────────────────────────────────────
    /// Vérifie que les tokens sont valides et que l'API répond.
    async fn check_health(&self) -> Result<HealthStatus>;

    /// Arrêt propre (annule les uploads en cours).
    async fn shutdown(&self);
}
```

### 4.2 Types Retour

```rust
/// Index distant : fichiers et dossiers avec leurs IDs.
pub struct RemoteIndex {
    pub files: Vec<RemoteFile>,
    pub dirs: Vec<RemoteDir>,
}

pub struct RemoteFile {
    pub relative_path: String,
    pub drive_id: String,
    pub parent_id: String,
    pub md5: String,
    pub size: u64,
    pub modified_time: i64,
}

pub struct RemoteDir {
    pub relative_path: String,
    pub drive_id: String,
    pub parent_id: String,
}

pub struct UploadResult {
    pub drive_id: String,
    pub md5_checksum: String,    // retourné par Google, pour vérification intégrité
    pub size_bytes: u64,
}

pub struct ChangesPage {
    pub changes: Vec<Change>,
    pub new_cursor: String,      // pour le prochain appel
    pub has_more: bool,
}

pub enum Change {
    Modified { drive_id: String, name: String, parent_id: String },
    Deleted { drive_id: String },
}

pub enum HealthStatus {
    Ok { email: String, quota_used: u64, quota_total: u64 },
    AuthExpired,
    Unreachable,
}
```

### 4.3 Structure `GDriveProvider`

```rust
pub struct GDriveProvider {
    client: reqwest::Client,         // partagé, Keep-Alive
    auth: Arc<GoogleAuth>,           // Phase 2
    path_cache: Arc<PathCache>,      // cache path→ID
    config: Arc<AdvancedConfig>,     // seuils, rate limit, delete_mode
    shutdown: CancellationToken,
}
```

---

## 5. Spécification Détaillée

### 5.1 Client HTTP Partagé

```rust
let client = reqwest::Client::builder()
    .user_agent("SyncGDrive/2.0")
    .pool_max_idle_per_host(4)
    .timeout(Duration::from_secs(30))  // pour les requêtes metadata
    .build()?;
```

- **Un seul client** pour tout le processus (connexions TCP réutilisées).
- **Pas de timeout** sur les uploads (fichiers potentiellement gros).
- TLS via `rustls` (pas d'OpenSSL = build plus simple).

### 5.2 Upload — Simple vs Resumable

| Condition | Type d'upload | Endpoint |
|-----------|---------------|----------|
| Fichier ≤ `resumable_upload_threshold` (5 Mo) | Simple upload | `POST /upload/drive/v3/files?uploadType=multipart` |
| Fichier > seuil | Resumable upload | `POST /upload/drive/v3/files?uploadType=resumable` |

**Resumable Upload** :
1. `POST` initiation → reçoit un `upload_uri`.
2. `PUT` chunks de 256 Ko (ou configurable).
3. En cas de coupure → `PUT` avec header `Content-Range` pour reprendre.
4. Progression : octets envoyés trackés à chaque chunk.

### 5.3 Listing via `files.list` (BFS)

```
GET https://www.googleapis.com/drive/v3/files
    ?q='{parent_id}' in parents and trashed=false
    &fields=files(id,name,mimeType,md5Checksum,size,modifiedTime,parents)
    &pageSize=1000
    &pageToken={token}
```

- Pagination automatique via `pageToken`.
- BFS niveau par niveau (comme V1 `ls_recursive`).
- Résultats stockés dans `path_cache` pour la suite.
- Concurrence bornée par `advanced.max_concurrent_ls`.

### 5.4 Cache Path → ID

Le cache est indispensable car Google Drive est ID-based :

```
"src/engine/scan.rs"
    → parent: resolve("src/engine/") → drive_id "AAA"
    → fichier: lookup("src/engine/scan.rs") → drive_id "BBB"
```

**Opérations** :

```rust
impl PathCache {
    /// Résout un chemin relatif en drive_id. Crée les dossiers intermédiaires si nécessaire.
    pub async fn resolve_or_create(
        &self,
        relative_path: &str,
        provider: &dyn RemoteProvider,
    ) -> Result<String>;

    /// Recherche un fichier dans le cache.
    pub fn lookup(&self, relative_path: &str) -> Option<CacheEntry>;

    /// Met à jour après un upload/mkdir.
    pub fn update(&self, relative_path: &str, entry: CacheEntry);

    /// Supprime une entrée (après delete/rename).
    pub fn remove(&self, relative_path: &str);

    /// Reconstruit le cache complet depuis le listing distant.
    pub fn rebuild_from_index(&self, index: &RemoteIndex);
}
```

### 5.5 Anti-Duplicate Google Drive

Google Drive permet des fichiers avec le **même nom** dans le même dossier. Pour éviter les doublons :

1. **Upload nouveau fichier** : Vérifier dans `path_cache` si le fichier existe déjà.
   - Si oui → `upload()` avec `existing_id = Some(id)` → met à jour l'existant.
   - Si non → `upload()` avec `existing_id = None` → crée.
2. **Mkdir** : Vérifier dans `path_cache` / `files.list` avant de créer.
3. **Double protection** : Query `files.list` avec `name='{name}' and '{parent_id}' in parents` avant chaque création.

### 5.6 Mapping V1 → V2 des Opérations

| Opération V1 (`KioOps`) | Opération V2 (`RemoteProvider`) |
|--------------------------|--------------------------------|
| `ls_remote(url)` | `list_remote(root_id)` |
| `stat(url)` | `path_cache.lookup(path)` (pas d'appel réseau) |
| `mkdir_p(url)` | `path_cache.resolve_or_create(path)` |
| `mkdir_if_absent(url)` | `path_cache.resolve_or_create(path)` |
| `copy_file(src, dst)` | `upload(local_path, parent_id, name, None)` |
| `copy_overwrite(src, dst)` | `upload(local_path, parent_id, name, Some(id))` |
| `delete(url)` | `delete(file_id)` |
| `rename(from, to)` | `rename(id, new_name, new_parent_id)` |
| `terminate_all()` | `shutdown()` |

---

## 6. Cas Limites

| Cas | Comportement attendu |
|-----|---------------------|
| Fichier 0 octets | Simple upload (Google Drive accepte les fichiers vides via API) |
| Fichier > 5 Go | Resumable upload par chunks (limite Google = 5 TB) |
| Nom de fichier avec caractères spéciaux | L'API Drive gère UTF-8 nativement — pas d'encodage nécessaire |
| Dossier racine supprimé sur Drive | Détection via `check_health` → notification erreur |
| Token expiré pendant un upload | Refresh automatique, retry du chunk courant |
| Réseau coupé pendant un resumable upload | Retry avec reprise au dernier chunk confirmé |
| Fichier modifié pendant l'upload | Hash local vérifié après upload — si changé, re-upload |
| Cache path→ID désynchronisé | Reconstruction au prochain full scan |
| Doublon détecté sur Drive (même nom, même parent) | Utiliser le premier résultat, log warning |

---

## 7. Tests à Écrire

### Unitaires (`remote/gdrive.rs`)

- `test_build_list_query` : query string correcte pour `files.list`
- `test_parse_file_list_response` : JSON Google → `Vec<RemoteFile>`
- `test_parse_empty_response` : réponse vide → `RemoteIndex` vide
- `test_multipart_body_format` : le body multipart est bien formé
- `test_resumable_initiation_headers` : headers corrects pour l'initiation

### Unitaires (`remote/path_cache.rs`)

- `test_lookup_existing` : entrée présente → Some
- `test_lookup_missing` : entrée absente → None
- `test_rebuild_from_index` : index complet → cache cohérent
- `test_resolve_nested_path` : `a/b/c/file.rs` → résout chaque segment
- `test_remove_cascades` : supprimer un dossier → enfants supprimés du cache

### Intégration (mock HTTP)

- `test_upload_simple_mock` : upload < 5 Mo → un seul POST
- `test_upload_resumable_mock` : upload > 5 Mo → initiation + chunks
- `test_list_paginated_mock` : listing avec 2 pages
- `test_delete_trash_mock` : delete_mode=trash → `PATCH` avec `trashed: true`
- `test_delete_permanent_mock` : delete_mode=delete → `DELETE`
- `test_token_refresh_during_upload` : 401 → refresh → retry

---

## 8. Critères d'Acceptation

- [ ] Le trait `RemoteProvider` compile et est implémenté par `GDriveProvider`
- [ ] Upload simple fonctionne (fichier < 5 Mo)
- [ ] Upload resumable fonctionne (fichier > 5 Mo)
- [ ] Listing récursif fonctionne (BFS paginé)
- [ ] Le cache path→ID est fonctionnel (lookup, resolve, rebuild)
- [ ] Mkdir ne crée pas de doublons
- [ ] Delete respecte `delete_mode` (trash vs permanent)
- [ ] Rename fonctionne (nom et/ou parent)
- [ ] Le moteur (`engine/`) fonctionne avec `GDriveProvider` comme avec l'ancien `KioClient`
- [ ] `cargo test` : tous les tests passent
- [ ] `cargo clippy` : 0 warning
- [ ] L'ancien `kio.rs` peut être supprimé (ou gardé derrière un feature flag `legacy-kio`)

