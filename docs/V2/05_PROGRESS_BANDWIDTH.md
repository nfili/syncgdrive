# Phase 5 — Progression Octets/Vitesse + Limite Bande Passante

---

## 1. Objectif

Passer d'une progression fichier-par-fichier (V1) à une progression en octets avec vitesse instantanée, ETA, et limitation de bande passante configurable.

---

## 2. Pré-requis

- **Phase 3** : `RemoteProvider` avec upload controlé (reqwest body streamé).

---

## 3. Fichiers Impactés

| Action | Fichier | Description |
|--------|---------|-------------|
| **Modifier** | `src/engine/mod.rs` | `EngineStatus::ScanProgress` et `SyncProgress` enrichis |
| **Modifier** | `src/engine/worker.rs` | Tracking octets envoyés par chunk |
| **Modifier** | `src/remote/gdrive.rs` | Callback de progression sur l'upload stream |
| **Créer** | `src/engine/bandwidth.rs` | Token bucket pour limitation bande passante |
| **Modifier** | `src/ui/tray.rs` | Tooltip avec vitesse, ETA, barre globale |

---

## 4. Structures de Données

### 4.1 `EngineStatus` V2

```rust
pub enum EngineStatus {
    Starting,
    Unconfigured(String),
    Idle,
    ScanProgress {
        phase: ScanPhase,
        done: usize,
        total: usize,
        current_dir: String,
        current_name: String,
    },
    SyncProgress {
        done: usize,
        total: usize,
        current_dir: String,
        current_name: String,
        size_bytes: u64,
        bytes_sent: u64,
        total_bytes: u64,
        total_bytes_sent: u64,
        speed_bps: u64,
    },
    Syncing { active: usize },
    Paused,
    Offline,      // NOUVEAU (Phase 6)
    Error(String),
    Stopped,
}
```

### 4.2 Tracker de Progression

```rust
/// Compteurs atomiques partagés entre workers.
pub struct ProgressTracker {
    pub total_files: AtomicUsize,
    pub done_files: AtomicUsize,
    pub total_bytes: AtomicU64,
    pub sent_bytes: AtomicU64,
    speed_samples: Mutex<VecDeque<(Instant, u64)>>,  // fenêtre glissante 5s
}

impl ProgressTracker {
    /// Calcule la vitesse instantanée (moyenne glissante sur 5s).
    pub fn speed_bps(&self) -> u64 { ... }

    /// Estime le temps restant en secondes.
    pub fn eta_secs(&self) -> Option<u64> {
        let speed = self.speed_bps();
        if speed == 0 { return None; }
        let remaining = self.total_bytes.load(Relaxed) - self.sent_bytes.load(Relaxed);
        Some(remaining / speed)
    }

    /// Enregistre des octets envoyés (appelé par le callback upload).
    pub fn record_bytes(&self, n: u64) { ... }
}
```

### 4.3 Token Bucket (Bandwidth Limiter)

```rust
/// Limiteur de bande passante par token bucket.
pub struct BandwidthLimiter {
    limit_bps: u64,          // 0 = illimité
    tokens: AtomicU64,
    last_refill: Mutex<Instant>,
}

impl BandwidthLimiter {
    /// Attend que `n` octets soient disponibles avant de continuer.
    /// Si limit = 0, retourne immédiatement.
    pub async fn acquire(&self, n: u64) { ... }
}
```

---

## 5. Spécification Détaillée

### 5.1 Callback de Progression sur Upload

Avec `reqwest`, on peut wrapper le body dans un stream qui compte les octets :

```rust
/// Wrap un fichier dans un stream qui notifie la progression.
fn progress_stream(
    file: tokio::fs::File,
    tracker: Arc<ProgressTracker>,
    limiter: Arc<BandwidthLimiter>,
) -> impl futures::Stream<Item = Result<Bytes>> {
    // Lire par chunks de 64 Ko
    // Pour chaque chunk:
    //   1. limiter.acquire(chunk.len())
    //   2. tracker.record_bytes(chunk.len())
    //   3. yield chunk
}
```

### 5.2 Calcul de la Vitesse

- **Fenêtre glissante** de 5 secondes.
- Échantillons : `(timestamp, bytes_cumulés)` ajoutés toutes les 500ms.
- Vitesse = `(bytes_now - bytes_5s_ago) / 5.0`.
- Lissage : évite les pics (un seul gros chunk) et les creux (pause entre fichiers).

### 5.3 Estimation Temps Restant (ETA)

```
ETA = (total_bytes - sent_bytes) / speed_bps
```

- Affiché uniquement si `speed_bps > 0` et `total_bytes > 0`.
- Format : `~3 min restantes`, `~45 s restantes`, `< 1 min`.

### 5.4 Affichage Tooltip

```
Transfert 12/156
📂 Documents/Travail/
📄 rapport.pdf (4,2 Mo)
[████████████░░░░░░░░] 62% · 2,6 Mo/s · ~3 min
Total : 128 Mo / 512 Mo
```

### 5.5 Limitation Bande Passante

- Configurable : `advanced.upload_limit_kbps` (0 = illimité).
- Granularité : par chunk de 64 Ko.
- Le limiter est partagé entre tous les workers → la limite est **globale**.
- Affiché dans le tooltip : `⚡ Limité à 500 Ko/s` si actif.

### 5.6 Throttle des Mises à Jour UI (Anti-Saturation)

**Problème** : Les callbacks de progression sont déclenchés à chaque chunk uploadé
(toutes les ~1ms à 64 Ko/chunk sur une bonne connexion). Avec N workers en parallèle,
le canal vers le thread GTK (`glib::MainContext::channel`) recevrait des milliers
de messages par seconde, provoquant des lags visuels et une surconsommation CPU.

**Solution** : Le `ProgressTracker` ne publie **pas** chaque event. Il agrège en interne
(compteurs atomiques) et un tick périodique envoie un snapshot vers l'UI.

```rust
/// Tâche Tokio dédiée : publie un snapshot de progression vers l'UI
/// à intervalle fixe (200ms). Les workers écrivent dans les AtomicU64
/// en continu sans aucun coût de synchronisation.
async fn progress_publisher(
    tracker: Arc<ProgressTracker>,
    status_tx: UnboundedSender<EngineStatus>,
    shutdown: CancellationToken,
) {
    let mut interval = tokio::time::interval(Duration::from_millis(200));
    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            _ = interval.tick() => {
                let snapshot = tracker.snapshot();
                let _ = status_tx.send(EngineStatus::SyncProgress {
                    done: snapshot.done_files,
                    total: snapshot.total_files,
                    current_dir: snapshot.current_dir.clone(),
                    current_name: snapshot.current_name.clone(),
                    size_bytes: snapshot.current_file_size,
                    bytes_sent: snapshot.current_bytes_sent,
                    total_bytes: snapshot.total_bytes,
                    total_bytes_sent: snapshot.sent_bytes,
                    speed_bps: snapshot.speed_bps,
                });
            }
        }
    }
}
```

**Architecture du flux** :

```
Workers (N threads)                  ProgressTracker              UI
    │                                    │                        │
    │── record_bytes(64Ko) ───▶ AtomicU64.fetch_add()            │
    │── record_bytes(64Ko) ───▶ AtomicU64.fetch_add()            │
    │── record_bytes(64Ko) ───▶ AtomicU64.fetch_add()            │
    │                                    │                        │
    │               ┌── tick 200ms ──────┤                        │
    │               │                    │                        │
    │               │    snapshot() ─────│── SyncProgress ───────▶│
    │               │                    │   (1 msg / 200ms)      │
    │               └── tick 200ms ──────┤                        │
    │                                    │                        │
```

**Garanties** :
- **Workers** : zéro blocage (écriture atomique, pas de mutex, pas de channel send).
- **UI** : max 5 messages/seconde (1 / 200ms), quelle que soit la vitesse d'upload.
- **Précision** : les compteurs atomiques sont exacts — le snapshot est juste un échantillonnage temporel.
- **Configurable** : l'intervalle de 200ms n'est **pas** dans `[advanced]` — c'est une constante structurelle d'UI, pas un paramètre utilisateur.

---

## 6. Cas Limites

| Cas | Comportement attendu |
|-----|---------------------|
| Vitesse = 0 (pause réseau) | ETA affiché comme `⏳ En attente…` |
| Un seul fichier de 2 Go | Barre de progression sur le fichier + barre globale identique |
| 10 000 petits fichiers (1 Ko chacun) | Vitesse = fichiers/s plus pertinente — mais on affiche quand même les octets |
| `upload_limit_kbps = 1` (1 Ko/s) | Fonctionne mais très lent — pas de validation minimum |
| Upload annulé (shutdown) | Progression s'arrête, pas de division par zéro |
| 4 workers uploadent simultanément | Max 5 updates UI/s (throttle 200ms), pas de lag |
| Upload très rapide (réseau local, petit fichier) | Au moins 1 snapshot publié (tick garanti) |

---

## 7. Tests à Écrire

### Unitaires

- `test_progress_tracker_record_bytes` : record → sent_bytes augmente
- `test_progress_tracker_speed` : injection d'échantillons → vitesse calculée
- `test_progress_tracker_eta` : 50% envoyé, vitesse connue → ETA correct
- `test_progress_tracker_eta_zero_speed` : vitesse 0 → None
- `test_bandwidth_limiter_unlimited` : limit=0 → acquire retourne immédiatement
- `test_bandwidth_limiter_throttle` : limit=1024 → délai observable entre chunks
- `test_human_eta_format` : 180s → "~3 min", 45s → "~45 s", 3600s → "~1 h"
- `test_progress_publisher_throttle` : 1000 record_bytes en 100ms → max 1 snapshot publié
- `test_progress_snapshot_accuracy` : snapshot reflète la somme exacte des record_bytes

---

## 8. Critères d'Acceptation

- [ ] Le tooltip affiche la progression en octets (Mo envoyés / Mo total)
- [ ] La vitesse instantanée est affichée (Mo/s)
- [ ] L'ETA est affiché quand pertinent
- [ ] La limitation de bande passante fonctionne (vérifiable avec un gros fichier)
- [ ] La barre de progression globale est correcte (octets totaux)
- [ ] Pas de division par zéro ni de panic sur des cas limites
- [ ] Les mises à jour UI sont throttlées à max 5/s (pas de saturation du thread GTK)
- [ ] Les workers n'ont aucun blocage lié à la progression (écritures atomiques uniquement)
- [ ] `cargo test` : tous les tests passent
- [ ] `cargo clippy` : 0 warning

