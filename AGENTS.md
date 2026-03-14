# AGENTS.md — SyncGDrive

## Project Overview

Unidirectional sync daemon: **local folder → Google Drive** (or any KIO backend: SMB, SFTP, WebDAV). The local machine is the **source of truth**; the remote is a backup. Written in Rust (async Tokio), Linux-only, KDE-dependent (`kioclient5`).

## Build & Run

```bash
cargo build --features ui          # GTK4/libadwaita + ksni systray
cargo build                        # headless (no UI, engine only)
cargo test                         # unit tests (config, ignore)
RUST_LOG=debug cargo run --features ui  # verbose logging
```

The `ui` feature gate (`#[cfg(feature = "ui")]`) controls all GTK4/libadwaita/ksni code. Always build with `--features ui` for the full application.

## Architecture

```
main.rs           → Tokio orchestrator, POSIX signal handling (self-pipe), instance lock (flock), PID file
config.rs         → AppConfig (TOML serde), validation, XDG paths
db.rs             → SQLite WAL via Arc<Mutex<Connection>>, file_index (path, sha256, mtime) + dir_index (path) for persistent directory cache
kio.rs            → KioOps trait + KioClient (spawns kioclient5 subprocesses)
ignore.rs         → Glob-based exclusion (globset)
notif.rs          → Desktop notifications (notify-rust), errors-only UX policy + folder_missing, quota_exceeded
engine/mod.rs     → SyncEngine main loop: Pause/Resume, ForceScan, hot-reload config, local_root disappearance detection
engine/scan.rs    → 6-phase scan: remote BFS listing → local inventory → mkdir → diff+enqueue → delete DB orphans → delete remote orphans. retry(), is_fatal_kio_err(), is_quota_err()
engine/watcher.rs → inotify (notify crate), handles Both/From/To rename modes, overflow detection with 30s rescan fallback
engine/worker.rs  → Per-task handler: sync/delete/rename with retry, .part fallback, empty file skip
ui/mod.rs         → Module declarations, re-exports spawn_tray
ui/tray.rs        → ksni StatusNotifierItem systray, dynamic tooltip/menu, À propos window (async Tokio task)
ui/settings.rs    → GTK4/libadwaita Settings window (runs on a dedicated OS thread)
dist/             → Deployment files: syncgdrive.service (systemd --user)
```

## Key Patterns

- **KioOps trait** (`kio.rs`): all remote operations go through `trait KioOps`. `KioClient` is the real V1 implementation spawning `kioclient5` subprocesses. **V2 will replace kioclient5 with a native Google Drive REST API backend** (same trait, zero engine changes). Known kioclient5 bugs: exit=0 on empty files, spaces in paths break `cat`, exit codes are unreliable. Workarounds are documented in `docs/Optimisation.md` §7.
- **Anti-duplicate GDrive strategy**: GDrive allows duplicate names. Use `--overwrite copy` for all files (new and existing). `mkdir_if_absent` trusts the BFS remote index + `dir_index` DB table (persistent cache of known remote directories — avoids stat+mkdir for dirs already created in previous runs). `mkdir_p` (watcher path) uses `stat` since there's no pre-built index. On `clear()` (local_root change), `clear_dirs()` must also be called.
- **CancellationToken everywhere**: `tokio_util::CancellationToken` is threaded through all async paths. Every `tokio::select!` must check `shutdown.cancelled()` with `biased;` ordering (shutdown first).
- **Retry with exponential backoff**: `scan::retry()` is the single retry helper. It detects fatal KIO errors (auth/token/403/401/quota) via `is_fatal_kio_err()` to bail immediately. `is_quota_err()` refines detection for specific notification.
- **Engine commands via mpsc**: `EngineCommand` channel (`cmd_tx`/`cmd_rx`) drives Pause/Resume/ForceScan/ApplyConfig/Shutdown. Status flows back via `UnboundedSender<EngineStatus>`.
- **SyncProgress tracking**: `engine/mod.rs` uses `AtomicUsize` counters (`total_queued`, `total_done`) to track file progress across workers. `SyncProgress { done, total, current, size_bytes }` is sent when each task starts (with file name/size) and when each worker finishes (updated done count). Counters are reset to 0 before every scan.
- **GTK on a SINGLE persistent OS thread**: GTK4 can only be initialized once, on one thread, for the entire process lifetime. A second `libadwaita::init()` from another thread causes a panic ("Attempted to initialize GTK from two different threads"). Solution: `tray.rs` creates a single `gtk-ui` thread via `OnceLock` + `std::sync::mpsc`. Both `settings::run_standalone()` and the About window run sequentially on this thread. `settings.rs` must NOT call `libadwaita::init()` — the `gtk-ui` thread handles it.
- **notify-rust ALWAYS on a separate OS thread**: `notif::send()` spawns `std::thread` because `notify-rust` calls `zbus::block_on()` which panics inside Tokio. This also applies to `acquire_instance_lock()` in `main.rs` — any `Notification::show()` inside `#[tokio::main]` must be wrapped in `std::thread::spawn().join()`.
- **PID file**: `acquire_instance_lock()` writes `std::process::id()` to `$XDG_RUNTIME_DIR/syncgdrive.lock` after acquiring `flock`. Truncates first (`set_len(0)`) to avoid stale PID remnants.
- **Local root health check**: Engine's 30s overflow tick also verifies `local_root.is_dir()`. If missing → `notif::folder_missing()` + pause + `rescan_on_resume`.
- **Debounce dispatch** (`engine/mod.rs`): `spawn_debounced_dispatch()` coalesces `Modified` events for the same file within 500ms. `Delete`/`Rename` are forwarded immediately. Prevents multiple uploads for a single file edit (editors generate 2–4 inotify events per save).
- **Watcher rename modes** (`watcher.rs`): `RenameMode::From` = file left watched tree (trash) → `Deleted`. `RenameMode::To` = file arrived from outside → `Modified`. `RenameMode::Both` = rename within tree → `Renamed`.
- **Temp file rename fallback** (`worker.rs`): on `Task::Rename`, if `from` is not in DB (temp file like `.part`, `.tmp`) → fallback to `sync_file(to)` instead of remote rename.
- **Empty files skipped** (`worker.rs` + `scan.rs`): kioclient5 returns exit=0 but creates nothing for 0-byte files. They are skipped until they get content.
- **Periodic rescan** (`engine/mod.rs`): every `rescan_interval_min` (default 30), a full 6-phase scan runs to verify `local = DB = remote` equality, catching remote-side deletions or corruption.

## Data Flow

1. `AppConfig` loaded from `~/.config/syncgdrive/config.toml`
2. `Database` (SQLite WAL) at `~/.local/share/syncgdrive/index.db` — `file_index` stores `(relative_path, sha256, mtime)`, `dir_index` stores `(relative_path)` for persistent directory cache
3. Scan: local `WalkDir` + remote `kioclient5 ls` (BFS) + `dir_index` DB → diff against `file_index` DB → enqueue `Task::SyncFile`. Known dirs from DB + remote index skip stat+mkdir entirely.
4. Watcher: `inotify` events → `WatchEvent` → `Task` via mpsc channel
5. Workers: semaphore-bounded (`max_workers`), each runs `kioclient5 copy/cat/rm/move`
6. Logs: daily rotation at `~/.local/state/syncgdrive/logs/`, 7-day retention via `cleanup_old_logs()`
7. PID file: `$XDG_RUNTIME_DIR/syncgdrive.lock` (flock + PID written)

## Conventions

- **Language**: Code comments and UI strings are in **French**. Log messages mix French and English.
- **Error handling**: `anyhow::Result` everywhere, `thiserror` for `ConfigError` only.
- **Tracing**: use `tracing::{info, warn, error, debug}`, never `println!`. Filter via `RUST_LOG` env var.
- **File paths**: always relative to `local_root` in DB/logic; `kio::to_remote()` converts to full remote URL.
- **XDG compliance**: config/data/state follow `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME`.
- **Design docs**: `docs/` contains UX specs (`UX_SYSTRAY.md`), optimization notes, and status docs — consult before changing UI behavior.
- **Deployment**: `dist/` contains systemd service file. Toggle via `systemctl --user enable/disable syncgdrive.service` from `tray.rs`.
