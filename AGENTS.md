# AGENTS.md — SyncGDrive

## Project Overview

Unidirectional sync daemon: **local folder → Google Drive**. The local machine is the **source of truth**; the remote is a backup. Written in Rust (async Tokio), Linux-only.

**V1** (tag `v1.0.0`): KDE-dependent (`kioclient5` subprocesses). Single sync pair. 55 tests, clippy clean.

**V2** (branch `V2`, in development): Native Google Drive REST API v3 via `reqwest`. Multi-sync pairs. OAuth2. No external subprocess dependency. Full design docs in `docs/V2/`.

## Build & Run

```bash
cargo build --features ui          # GTK4/libadwaita + ksni systray
cargo build                        # headless (no UI, engine only)
cargo test                         # unit tests (config, ignore)
cargo clippy --features ui -- -W clippy::all  # lint check
RUST_LOG=debug cargo run --features ui  # verbose logging
SYNCGDRIVE_DRY_RUN=1 cargo run --features ui  # V2: dry-run (no remote writes)
```

The `ui` feature gate (`#[cfg(feature = "ui")]`) controls all GTK4/libadwaita/ksni code. Always build with `--features ui` for the full application.

## Architecture

### V1 (current code on `master`)

```
main.rs           → Tokio orchestrator, POSIX signal handling (self-pipe), instance lock (flock), PID file
config.rs         → AppConfig (TOML serde), validation, XDG paths
db.rs             → SQLite WAL via Arc<Mutex<Connection>>, file_index (path, sha256, mtime) + dir_index (path)
kio.rs            → KioOps trait + KioClient (spawns kioclient5 subprocesses)
ignore.rs         → Glob-based exclusion (globset)
notif.rs          → Desktop notifications (notify-rust), errors-only UX policy
engine/mod.rs     → SyncEngine main loop: Pause/Resume, ForceScan, hot-reload config
engine/scan.rs    → 6-phase scan: remote BFS → local inventory → mkdir → diff → delete DB orphans → delete remote orphans
engine/watcher.rs → inotify (notify crate), rename modes, overflow → 30s rescan
engine/worker.rs  → Per-task handler: sync/delete/rename with retry, .part fallback
ui/mod.rs         → Module declarations, re-exports spawn_tray
ui/tray.rs        → ksni systray, dynamic tooltip/menu, single GTK thread (OnceLock + mpsc)
ui/settings.rs    → GTK4/libadwaita Settings window (runs on the gtk-ui thread)
dist/             → syncgdrive.service (systemd --user)
```

### V2 (planned — see `docs/V2/`)

```
main.rs           → Orchestrator (reads AdvancedConfig for channel capacity, shutdown timeout, log retention)
config.rs         → AppConfig V2: [[sync_pairs]], [advanced], SyncPair, AdvancedConfig, migration V1→V2
migration.rs      → Config + DB migration V1→V2 (backup .v1.bak)
db.rs             → + schema_version, path_cache (path→drive_id), offline_queue
auth/mod.rs       → OAuth2 module re-exports
auth/oauth2.rs    → Google OAuth2 loopback flow (ephemeral localhost server)
auth/storage.rs   → Token storage (secret-service keyring / encrypted file fallback)
remote/mod.rs     → RemoteProvider trait (replaces KioOps)
remote/gdrive.rs  → Google Drive REST API v3 implementation (reqwest, resumable upload)
remote/path_cache.rs → Path→ID resolution cache (SQLite-backed)
engine/bandwidth.rs  → Token bucket bandwidth limiter
engine/offline.rs    → Network detection, offline queue, auto-resume
engine/integrity.rs  → MD5 verification post-upload
engine/rate_limiter.rs → Google API rate limiter (token bucket + Retry-After)
ui/icons.rs       → SVG icon loading + ARGB32 rendering (resvg + tiny-skia)
ui/scan_window.rs → Initial scan progress window (libadwaita)
utils/path_display.rs → split_path_display(), format_path_tooltip()
assets/icons/     → 6 static SVGs + 4 animation frames
dist/             → PKGBUILD, debian/, .desktop, Makefile, sysctl, systemd
```

## V2 Design Documents

All design is complete before coding. See `docs/V2/`:

| Doc | Content |
|-----|---------|
| `00_INDEX.md` | Phase index + dependency graph |
| `01_CONFIG_V2.md` | Config TOML V2: SyncPair, AdvancedConfig, migration, DB schema |
| `02_AUTH_OAUTH2.md` | OAuth2 loopback, keyring storage, wizard UI, 100% free |
| `03_REMOTE_PROVIDER.md` | RemoteProvider trait, GDriveProvider, path→ID cache, anti-duplicate |
| `04_HARDCODE_CLEANUP.md` | Elimination of 11 hardcoded constants → [advanced] config |
| `05_PROGRESS_BANDWIDTH.md` | Byte-level progress, speed, ETA, bandwidth limiter |
| `06_RESILIENCE.md` | Offline mode, MD5 integrity, trash, rate limiter, symlinks |
| `07_UX_PREMIUM.md` | SVG icons, systray animation, scan window, path display (📂+📄) |
| `08_DRY_RUN_TESTS.md` | Dry-run mode, MockProvider, 30 integration tests |
| `09_PACKAGING.md` | PKGBUILD, .deb, .desktop, Makefile, systemd, sysctl |
| `10_QA_MANUAL_TESTS.md` | 149 manual QA tests across 15 sections |

## Key Patterns

- **RemoteProvider trait** (`remote/mod.rs`, V2): All remote operations go through `trait RemoteProvider`. `GDriveProvider` is the V2 implementation using `reqwest` + Google Drive REST API v3. V1's `KioOps` / `KioClient` (kioclient5) is replaced entirely.
- **KioOps trait** (`kio.rs`, V1 only): V1 implementation spawning `kioclient5` subprocesses. Known bugs documented in `docs/Optimisation.md` §7. Removed in V2.
- **Anti-duplicate GDrive strategy**: V1: `--overwrite copy` + BFS index + `dir_index` DB. V2: `path_cache` (path→drive_id SQLite table) + check-before-create + `files.update` for existing files.
- **CancellationToken everywhere**: `tokio_util::CancellationToken` is threaded through all async paths. Every `tokio::select!` must check `shutdown.cancelled()` with `biased;` ordering (shutdown first).
- **Retry with exponential backoff**: `scan::retry()` detects fatal errors (auth/token/403/401/quota) via `is_fatal_kio_err()` to bail immediately. V2 adds rate limiter + `Retry-After` header support.
- **Engine commands via mpsc**: `EngineCommand` channel drives Pause/Resume/ForceScan/ApplyConfig/Shutdown. Status flows back via `UnboundedSender<EngineStatus>`.
- **SyncProgress tracking**: V1: `AtomicUsize` counters (files). V2: + `AtomicU64` for bytes, `ProgressTracker` with sliding-window speed, ETA.
- **UI update throttle (V2)**: Workers write to `AtomicU64` counters with zero blocking. A dedicated `progress_publisher` Tokio task samples a snapshot every 200ms and sends ONE `EngineStatus::SyncProgress` to the UI. Max 5 updates/sec regardless of upload speed or worker count. Prevents GTK thread saturation.
- **GTK on a SINGLE persistent OS thread**: `tray.rs` creates a single `gtk-ui` thread via `OnceLock` + `std::sync::mpsc`. All GTK windows (Settings, About, Scan progress, OAuth wizard) run sequentially on this thread. `libadwaita::init()` called ONCE. `settings.rs` must NOT call it.
- **notify-rust ALWAYS on a separate OS thread**: `notif::send()` spawns `std::thread` because `notify-rust` calls `zbus::block_on()` which panics inside Tokio.
- **PID file**: `acquire_instance_lock()` writes PID to `$XDG_RUNTIME_DIR/syncgdrive.lock` via `flock`. Truncates first (`set_len(0)`).
- **Local root health check**: 30s tick verifies `local_root.is_dir()`. Missing → `notif::folder_missing()` + pause.
- **Debounce dispatch**: `spawn_debounced_dispatch()` coalesces `Modified` events within `advanced.debounce_ms` (default 500ms). `Delete`/`Rename` forwarded immediately.
- **Watcher rename modes**: `From` = left tree → Deleted. `To` = arrived → Modified. `Both` = rename within tree → Renamed.
- **Zero hardcoded values (V2)**: All configurable constants in `[advanced]` section. Only structural constants (KB/MB/GB, app IDs) allowed in code.
- **Offline mode (V2)**: Network loss detected → `Offline` state, events queued in `offline_queue` DB table, auto-flush on reconnect.
- **Trash by default (V2)**: `advanced.delete_mode = "trash"` → `files.update({ trashed: true })`. Safer than permanent delete.

## Data Flow

### V1
1. `AppConfig` loaded from `~/.config/syncgdrive/config.toml`
2. `Database` (SQLite WAL) at `~/.local/share/syncgdrive/index.db`
3. Scan: local `WalkDir` + remote `kioclient5 ls` (BFS) → diff against `file_index` DB → enqueue tasks
4. Watcher: `inotify` events → `WatchEvent` → `Task` via mpsc
5. Workers: semaphore-bounded, each runs `kioclient5 copy/cat/rm/move`
6. Logs: daily rotation at `~/.local/state/syncgdrive/logs/`, configurable retention
7. PID file: `$XDG_RUNTIME_DIR/syncgdrive.lock`

### V2 (additions)
8. OAuth2 tokens in system keyring (secret-service) or encrypted file fallback
9. `path_cache` DB table: persistent path→drive_id mapping
10. `offline_queue` DB table: events accumulated during network outage
11. `schema_version` DB table: automatic migration support
12. Resumable uploads for files > `advanced.resumable_upload_threshold` (5 Mo)
13. Rate limiting: internal token bucket + Google `Retry-After` header respect
14. MD5 integrity verification after each upload

## Conventions

- **Language**: Code comments and UI strings are in **French**. Log messages mix French and English.
- **Error handling**: `anyhow::Result` everywhere, `thiserror` for `ConfigError` only.
- **Tracing**: use `tracing::{info, warn, error, debug}`, never `println!`. Filter via `RUST_LOG` env var.
- **File paths**: always relative to `local_root` in DB/logic. V1: `kio::to_remote()` converts to URL. V2: `path_cache` resolves to drive IDs.
- **XDG compliance**: config/data/state follow `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME`.
- **Design docs**: `docs/V2/` contains all V2 design documents (consult before coding). `docs/` contains V1 UX specs and optimization notes.
- **Deployment**: `dist/` contains systemd service, PKGBUILD, .desktop, Makefile.
- **Config V2**: TOML with `[[sync_pairs]]` (multi-sync), `[retry]`, `[advanced]`. No hardcoded values in code.
- **Path display (V2)**: Always show `📂 parent_dir/` + `📄 filename` in UI (tooltip, scan window). Use `split_path_display()` utility.
