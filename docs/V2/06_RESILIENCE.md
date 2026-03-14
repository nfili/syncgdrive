# Phase 6 — Résilience (Offline, Intégrité, Corbeille, Rate Limiter)

---

## 1. Objectif

Rendre SyncGDrive robuste face aux conditions réelles : coupures réseau, fichiers corrompus, suppressions accidentelles, et quotas Google API.

---

## 2. Pré-requis

- **Phase 3** : `RemoteProvider` opérationnel.
- **Phase 4** : Config `AdvancedConfig` lue partout.

---

## 3. Fichiers Impactés

| Action | Fichier | Description |
|--------|---------|-------------|
| **Créer** | `src/engine/offline.rs` | Détection réseau, queue offline, reprise auto |
| **Créer** | `src/engine/integrity.rs` | Vérification MD5 post-upload |
| **Créer** | `src/engine/rate_limiter.rs` | Rate limiting API Google (token bucket) |
| **Modifier** | `src/remote/gdrive.rs` | Intégrer rate limiter, trash vs delete |
| **Modifier** | `src/engine/mod.rs` | État `Offline`, gestion queue offline |
| **Modifier** | `src/engine/worker.rs` | Vérification intégrité après chaque upload |
| **Modifier** | `src/db.rs` | CRUD `offline_queue` |
| **Modifier** | `src/notif.rs` | Notifications offline/online, intégrité |

---

## 4. Structures de Données

### 4.1 Queue Offline

```rust
pub struct OfflineTask {
    pub id: i64,
    pub action: OfflineAction,
    pub relative_path: String,
    pub extra: Option<String>,   // JSON sérialisé
    pub created_at: i64,
}

pub enum OfflineAction {
    Sync,
    Delete,
    Rename { from: String },
}
```

### 4.2 Rate Limiter

```rust
pub struct ApiRateLimiter {
    max_rps: u32,
    tokens: AtomicU32,
    last_refill: Mutex<Instant>,
}

impl ApiRateLimiter {
    /// Attend qu'un slot soit disponible avant d'envoyer une requête API.
    pub async fn acquire(&self) { ... }

    /// Gère un 429 Too Many Requests — pause jusqu'à Retry-After.
    pub async fn handle_rate_limit(&self, retry_after_secs: u64) { ... }
}
```

### 4.3 Résultat de Vérification Intégrité

```rust
pub enum IntegrityResult {
    Ok,
    Mismatch { local_md5: String, remote_md5: String },
    RemoteMissing,
}
```

---

## 5. Spécification Détaillée

### 5.1 Mode Hors-Ligne

**Détection** :

```rust
/// Vérifie la connectivité vers Google Drive.
async fn check_connectivity(client: &reqwest::Client) -> bool {
    client
        .head("https://www.googleapis.com/drive/v3/about")
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .map(|r| r.status() != StatusCode::UNAUTHORIZED)  // 401 = token, pas réseau
        .unwrap_or(false)
}
```

**Machine d'état** :

```
Online ──(échec réseau)──▶ Offline
   ▲                          │
   │                          │ (check toutes les 30s)
   │                          │
   └──(réseau retrouvé)──────┘
         → flush offline_queue
         → notification "Connexion rétablie"
```

**Comportement offline** :

1. Le watcher inotify **continue** de fonctionner.
2. Les événements sont stockés dans `offline_queue` (SQLite).
3. Pas de retry réseau (économie batterie).
4. Le health check (tick 30s) teste la connectivité.
5. Au retour en ligne : flush de la queue dans l'ordre FIFO.
6. Déduplication : si un fichier est modifié 3 fois offline, seul le dernier état est synchronisé.

### 5.2 Vérification d'Intégrité Post-Upload

Après chaque upload réussi :

```rust
async fn verify_upload(
    local_path: &Path,
    upload_result: &UploadResult,
) -> IntegrityResult {
    let local_md5 = compute_md5(local_path).await?;
    if local_md5 == upload_result.md5_checksum {
        IntegrityResult::Ok
    } else {
        IntegrityResult::Mismatch {
            local_md5,
            remote_md5: upload_result.md5_checksum.clone(),
        }
    }
}
```

**En cas de mismatch** :
1. Log `warn!("integrity mismatch: {path}")`.
2. Re-upload immédiat (1 tentative).
3. Si mismatch persiste → notification erreur.

### 5.3 Corbeille vs Suppression

Selon `advanced.delete_mode` :

| Mode | Appel API | Récupérable |
|------|-----------|-------------|
| `"trash"` (défaut) | `PATCH /files/{id}` body `{"trashed": true}` | ✅ Oui (30 jours) |
| `"delete"` | `DELETE /files/{id}` | ❌ Non |

### 5.4 Rate Limiter API

- **Pré-limitation** : avant chaque requête API, `rate_limiter.acquire()`.
- **Réaction 429** : si Google retourne 429 → lire `Retry-After` → pause.
- **Quota affiché** : `check_health()` retourne `quota_used` / `quota_total` → tooltip.

```rust
// Intégration dans GDriveProvider
async fn api_request(&self, req: Request) -> Result<Response> {
    self.rate_limiter.acquire().await;
    let resp = self.client.execute(req).await?;

    if resp.status() == 429 {
        let wait = resp.headers()
            .get("Retry-After")
            .and_then(|v| v.to_str().ok()?.parse::<u64>().ok())
            .unwrap_or(60);
        self.rate_limiter.handle_rate_limit(wait).await;
        // Retry
        return self.client.execute(req).await.map_err(Into::into);
    }

    Ok(resp)
}
```

### 5.5 Gestion des Symlinks

Selon `advanced.symlink_mode` :

| Mode | Comportement |
|------|-------------|
| `"ignore"` (défaut) | Symlinks exclus du scan et du watcher |
| `"follow"` | Symlinks suivis — détection de boucle via set de device:inode visités |

```rust
// Dans scan.rs, WalkDir
let walker = WalkDir::new(&cfg.local_root)
    .follow_links(cfg.advanced.symlink_mode == "follow")
    .into_iter()
    .filter_entry(|e| {
        if cfg.advanced.symlink_mode == "ignore" && e.path_is_symlink() {
            return false;
        }
        // ...autres filtres (ignore patterns)
        true
    });
```

---

## 6. Cas Limites

| Cas | Comportement attendu |
|-----|---------------------|
| Offline pendant 24h avec beaucoup de modifications | Queue triée FIFO, déduplication au flush |
| Fichier supprimé localement pendant offline | `Delete` en queue → flush supprime sur Drive au retour |
| Fichier renommé puis supprimé pendant offline | Déduplication : seul le `Delete` final est envoyé |
| Quota Google 429 avec Retry-After=3600 | Pause 1h + notification "Quota atteint" |
| MD5 mismatch persistant (2 tentatives) | Notification erreur, fichier marqué comme "problème" en DB |
| Symlink circulaire avec mode "follow" | Détection boucle via device:inode → skip + log warning |
| DB offline_queue pleine (100 000 entrées) | Pas de limite théorique (SQLite gère). Log warning si > 10 000 |

---

## 7. Tests à Écrire

### Unitaires

- `test_offline_queue_insert_and_read` : FIFO correct
- `test_offline_queue_dedup` : 3 Sync sur même fichier → 1 seul au flush
- `test_offline_queue_delete_cancels_sync` : Sync puis Delete → seul Delete reste
- `test_integrity_ok` : MD5 match → Ok
- `test_integrity_mismatch` : MD5 differ → Mismatch
- `test_rate_limiter_acquire_within_limit` : pas de blocage
- `test_rate_limiter_acquire_over_limit` : blocage mesurable
- `test_rate_limiter_retry_after` : pause correcte
- `test_trash_mode` : delete_mode=trash → bonne requête API
- `test_delete_mode` : delete_mode=delete → bonne requête API
- `test_symlink_ignore` : symlink filtré du scan
- `test_symlink_follow_no_loop` : symlink simple suivi
- `test_symlink_follow_loop_detected` : boucle détectée → skip

### Intégration

- `test_offline_online_cycle` : simuler perte réseau → queue → retour → flush
- `test_integrity_reupload_on_mismatch` : mismatch → re-upload automatique

---

## 8. Critères d'Acceptation

- [ ] Le moteur passe en état `Offline` quand le réseau est indisponible
- [ ] Les événements sont stockés dans `offline_queue` pendant le mode offline
- [ ] Le flush s'exécute correctement au retour en ligne (FIFO, dédupliqué)
- [ ] La notification « Connexion rétablie » est envoyée
- [ ] La vérification MD5 post-upload fonctionne
- [ ] Le re-upload automatique se déclenche sur mismatch
- [ ] `delete_mode = "trash"` utilise `PATCH` avec `trashed: true`
- [ ] `delete_mode = "delete"` utilise `DELETE`
- [ ] Le rate limiter respecte `api_rate_limit_rps`
- [ ] Les réponses 429 sont gérées avec `Retry-After`
- [ ] Les symlinks sont ignorés par défaut
- [ ] `cargo test` et `cargo clippy` : clean

