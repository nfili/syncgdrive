# Phase 4 — Élimination des Constantes Hardcodées

---

## 1. Objectif

Remplacer toutes les constantes en dur dans le code par des lectures depuis `AppConfig.advanced`. Après cette phase, le code métier ne contient plus aucune valeur numérique configurable.

---

## 2. Pré-requis

- **Phase 1** : Structure `AdvancedConfig` avec tous les champs et défauts.

---

## 3. Fichiers Impactés

| Action | Fichier | Modification |
|--------|---------|-------------|
| **Modifier** | `src/engine/mod.rs` | `DEBOUNCE_MS` → `cfg.advanced.debounce_ms`, tick 30s → `cfg.advanced.health_check_interval_secs` |
| **Modifier** | `src/engine/scan.rs` | (pas de hardcode propre, mais vérifier) |
| **Modifier** | `src/remote/gdrive.rs` | `MAX_CONCURRENT_LS` → `cfg.advanced.max_concurrent_ls` |
| **Modifier** | `src/main.rs` | Shutdown timeout, log retention, channel capacity, notif timeout |
| **Modifier** | `src/notif.rs` | Timeout notifications → `cfg.advanced.notification_timeout_ms` |

---

## 4. Mapping Exhaustif

| Valeur actuelle | Fichier:Ligne | Remplacement |
|----------------|---------------|--------------|
| `const DEBOUNCE_MS: u64 = 500` | `engine/mod.rs:525` | `cfg.advanced.debounce_ms` |
| `Duration::from_secs(30)` (tick) | `engine/mod.rs:196-197` | `Duration::from_secs(cfg.advanced.health_check_interval_secs)` |
| `const MAX_CONCURRENT_LS: usize = 8` | `kio.rs:247` → `remote/gdrive.rs` | `cfg.advanced.max_concurrent_ls` |
| `Duration::from_secs(3)` (shutdown) | `main.rs:108` | `Duration::from_secs(cfg.advanced.shutdown_timeout_secs)` |
| `cleanup_old_logs(&log_dir, 7)` | `main.rs:19` | `cleanup_old_logs(&log_dir, cfg.advanced.log_retention_days)` |
| `mpsc::channel(32)` | `main.rs:31` | `mpsc::channel(cfg.advanced.engine_channel_capacity)` |
| `.timeout(4000)` | `main.rs:160` | `.timeout(cfg.advanced.notification_timeout_ms)` |
| `timeout 0` (sticky notif) | `notif.rs:64` | Inchangé — les erreurs critiques restent sticky (choix UX intentionnel) |
| `6000` (initial sync) | `notif.rs` | `cfg.advanced.notification_timeout_ms` |
| `500ms` sleep kio fallback | `kio.rs:395` | Supprimé (kio.rs disparaît en Phase 3) |

---

## 5. Spécification Détaillée

### 5.1 Propagation de `AdvancedConfig`

Le `SyncEngine` reçoit déjà `&AppConfig`. Tous les accès se font via `self.cfg.advanced.xxx`.

Pour les fonctions utilitaires qui n'ont pas accès à la config (ex: `spawn_debounced_dispatch`), passer la valeur en paramètre :

```rust
// Avant
fn spawn_debounced_dispatch(...) {
    const DEBOUNCE_MS: u64 = 500;
    ...
}

// Après
fn spawn_debounced_dispatch(..., debounce_ms: u64) {
    ...
}
```

### 5.2 `main.rs` — Config Disponible Avant le Moteur

Problème : certaines valeurs (`log_retention_days`, `engine_channel_capacity`, `shutdown_timeout_secs`) sont utilisées **avant** le démarrage du moteur.

Solution : charger la config au tout début de `main()` :

```rust
#[tokio::main]
async fn main() -> Result<()> {
    let (cfg, _created) = AppConfig::load_or_create()?;
    let advanced = &cfg.advanced;

    cleanup_old_logs(&log_dir, advanced.log_retention_days);
    let (cmd_tx, cmd_rx) = mpsc::channel(advanced.engine_channel_capacity);
    // ...
    // shutdown timeout
    tokio::time::sleep(Duration::from_secs(advanced.shutdown_timeout_secs))
}
```

### 5.3 Notifications — Timeout Configurable

```rust
// notif.rs — avant
fn send(summary: &str, body: &str, icon: &str, urgency: Urgency, timeout_ms: i32) {

// Pas de changement de signature — le timeout est déjà paramétré.
// Les appelants passent cfg.advanced.notification_timeout_ms au lieu de 6000.
```

Exception : les notifications d'erreur critique restent en `timeout 0` (sticky) — c'est un choix UX, pas une constante configurable.

---

## 6. Cas Limites

| Cas | Comportement attendu |
|-----|---------------------|
| `debounce_ms = 0` | Pas de debounce — chaque événement est immédiat |
| `health_check_interval_secs = 0` | Health check désactivé |
| `engine_channel_capacity = 0` | Invalide — validation config rejette (min = 1) |
| `log_retention_days = 0` | Aucune rétention — les logs sont supprimés immédiatement |
| `shutdown_timeout_secs = 0` | Arrêt immédiat sans attente |
| `notification_timeout_ms = 0` | Notifications sticky (ne disparaissent pas) |

### Validation additionnelle dans `AdvancedConfig`

```rust
impl AdvancedConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.engine_channel_capacity == 0 {
            return Err(ConfigError::InvalidAdvanced("engine_channel_capacity must be > 0"));
        }
        if self.max_concurrent_ls == 0 {
            return Err(ConfigError::InvalidAdvanced("max_concurrent_ls must be > 0"));
        }
        if self.api_rate_limit_rps == 0 {
            return Err(ConfigError::InvalidAdvanced("api_rate_limit_rps must be > 0"));
        }
        Ok(())
    }
}
```

---

## 7. Tests à Écrire

### Unitaires

- `test_advanced_defaults_are_sane` : toutes les valeurs par défaut passent la validation
- `test_advanced_zero_channel_rejected` : `engine_channel_capacity = 0` → erreur
- `test_advanced_zero_concurrent_ls_rejected` : idem
- `test_debounce_zero_means_immediate` : vérifier que le debounce timer est skippé
- `test_config_partial_advanced_merge` : seul `debounce_ms` override → le reste = défauts

### Intégration

- `test_engine_uses_config_debounce` : engine avec `debounce_ms = 100` → debounce de 100ms
- `test_main_uses_config_channel_capacity` : channel créé avec la bonne taille

---

## 8. Critères d'Acceptation

- [ ] `grep -rn "const.*=" src/ | grep -v "KB\|MB\|GB\|test\|mock"` retourne 0 résultat
- [ ] Aucune valeur numérique magique dans `engine/`, `main.rs`, `notif.rs`
- [ ] Un `config.toml` sans section `[advanced]` fonctionne (tous les défauts)
- [ ] Un `config.toml` avec `[advanced]` partiel fonctionne (merge)
- [ ] La validation rejette les valeurs incohérentes (0 pour les capacités)
- [ ] `cargo test` : tous les tests passent
- [ ] `cargo clippy` : 0 warning

