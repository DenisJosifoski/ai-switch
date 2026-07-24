# Progress Log

## Phase 1 — Core engine
- Config parsing and validation (XDG config, duplicate port checks, script existence)
- Process lifecycle: start, stop, switch with raw fork/exec on Linux
- Health monitoring: `/v1/models` polling during startup (Starting → Loading → Ready)
- Port reconciliation at startup (detects already-running models)
- Single-instance enforcement

## Phase 2 — Switch logic hardening
- Race-free stop→wait→start under real conditions
- Port free verification after shutdown (up to 10s TCP bind retry)
- SIGTERM → SIGKILL escalation with configurable timeouts
- Zombie port handling (detects non-llama-server processes on model ports)

## Phase 3 — GTK4 native shell UI
- ApplicationWindow with native PopoverMenuBar (File / Edit / View / Help)
- ModelCard widgets with ON/OFF toggle, status label, Logs button (stub)
- Background thread model management via `mpsc::channel`
- Startup reconciliation: restores running model state from previous session
- About dialog and GitHub link

## Phase 4 — Restart, context display, auto-restart-on-full, preferences

### What was built
1. **Restart button** per model card (`model_card.rs`):
   - Added `restart_button: Button` to ModelCard struct
   - Enabled when card state is Ready or Error; disabled during transitions
   - Label changes to "Restarting…" while restart is in progress
   - Handler sends `RestartRequested` message to main loop (clears other cards), then performs stop→start on a background thread, then sends `SwitchCompleted` to update the target card

2. **Context display** (`window.rs` + `model_card.rs`):
   - Background polling thread calls `GET /slots` every 2 seconds per Ready model
   - Uses `reqwest::blocking::Client` with 3-second timeout to avoid freezes
   - Parses JSON response: sums `prompt_tokens_total` + `generation_tokens_total` across all slots, reads top-level `n_ctx`
   - Updates card label via channel-based message passing (main thread only, no GTK from background threads)
   - Format: `"Context: 1,024 / 32,000 (3.2%)"` with manual thousands separator
   - Color-coded: red CSS class at ≥90%, dim label otherwise

3. **Auto-restart on context full** (`window.rs`):
   - Checked during each poll cycle when usage ≥98% of `n_ctx`
   - Controlled by `auto_restart_on_context_full` config option (default: true)
   - Sends `RestartRequested` to main thread → performs stop→500ms→start on background thread
   - Logs via tracing: `"Context full — {model} restarted."` or error on failure

4. **Preferences dialog** (`app/src/preferences.rs`):
   - Edit → Preferences now opens a real modal dialog
   - Editable fields: log directory (with Browse button using FileChooserDialog), proxy port, auto-restart toggle
   - Save validates config (scripts exist, no duplicate ports) and writes back to `config.toml` via `toml::to_string_pretty`
   - Uses GTK4's non-blocking `run_async()` pattern with `mpsc::channel` for synchronous-like wait
   - Error dialog shown on save failure

### Architecture decisions
- **Thread safety**: GTK widgets cannot cross thread boundaries (contain raw FFI pointers). The polling thread sends `SlotUpdate` messages through a channel; the main loop processes them via `glib::timeout_add_local`. No `Arc<Mutex<Vec<ModelCard>>>` — cards stay on the main thread.
- **Signal blocking**: Preserved the existing `signal_block: Rc<Cell<bool>>` pattern in ModelCard. Programmatic state changes call `block_signals()` / `unblock_signals()` to prevent re-entrant toggle signals.
- **HTTP polling**: `reqwest::blocking::Client` with explicit 3s timeout, reused across polls. No async/await anywhere — consistent with the existing synchronous threading model.
- **Config serialization**: Added `#[derive(serde::Serialize)]` to `ModelConfig`, `GlobalSettings`, and `Config` in core/config.rs to support preferences save.

### Deferred / flagged
- Toast notification: Currently uses `tracing::info!()` for auto-restart notifications. A proper desktop toast (libnotify / GTK4 Notification) is deferred to a future phase.
- The `PollingState::Error` variant and `clear_context()` method exist for potential future use when /slots responses become unreliable.

## Phase 5 — Reverse proxy (single fixed port)

### What was built
- **`core/src/proxy.rs`**: A transparent local HTTP reverse proxy server using `tiny_http` running on a background `std::thread`. Binds to `127.0.0.1:proxy_port` (default 9080).
  - Inspects `ProxyState` on every request to determine forwarding target
  - Model Ready → forwards all requests (path, headers, body, query) to `http://127.0.0.1:{active_model_port}` with streaming response support via `tiny_http::Read` trait impl
  - No model → HTTP 503 with `{"error": "No active model server in ai-switch"}`
  - Loading (starting/restarting) → HTTP 503 with `{"error": "Model server is currently starting/restarting"}`
  - Forwarding errors → HTTP 503 with `{"error": "Model server unavailable"}`
  - Graceful shutdown via `AtomicBool` flag + `mpsc::channel` signal

- **`ProxyState`** struct: Shared state between the app and proxy, updated whenever a model starts/stops/switches/restarts. Fields: `target_port: Option<u16>`, `is_loading: bool`. Thread-safe via `Arc<Mutex<>>`.

- **App integration** (`app/src/main.rs`): Starts the proxy server after config load, stops it on app shutdown. Falls back gracefully if proxy binding fails (doesn't prevent app launch).

- **Window integration** (`app/src/window.rs`): Proxy state is updated from background threads after every model lifecycle operation:
  - Toggle start/switch → `set_target(port)` on success
  - Toggle stop → `clear()` on success
  - Restart button → `set_target(port)` on success
  - Auto-restart (context full) → `set_target(port)` on success

### Dependencies added
- `tiny_http = "0.12"` — lightweight synchronous HTTP server for the proxy
- No tokio runtime needed — fits the existing `std::thread` + GTK pattern

### Tests
- 5 unit tests in `proxy::tests`: default state, set_target, set_loading, clear, full lifecycle
- All 19 core unit tests pass
- All 4 integration tests pass (repeated switch loop, zombie-port, port-free-check, no-orphans)

### Architecture decisions
- **tiny_http over hyper**: The spec mentioned `hyper`/`tokio` as an option but also allowed `tiny_http`/`std::threads`. Chose tiny_http because:
  - The existing codebase uses `std::thread::spawn` everywhere (no tokio runtime)
  - Simpler API — no complex body type conversions needed
  - Streaming works via `Read` trait impl for SSE token streaming
  - `reqwest::blocking` already used for health checks and context polling

- **Proxy state update from background threads**: Rather than querying ProcessManager in the main loop, proxy state is updated directly from background threads right after successful model operations. This avoids adding extra dependencies to the channel polling closure and ensures the proxy always reflects the actual model state.

### Deviations from spec
- Used `tiny_http` instead of `hyper`/`tokio` (spec explicitly allowed this alternative)
- Proxy port hot-reload (changing proxy_port in Preferences updates the listener without restart) was deferred — the proxy reads its port at startup. This is acceptable since Preferences changes are infrequent and a restart is the clearest way to apply port changes.

## Phase 6 — Logs panel

### What was built
1. **`app/src/logs_panel.rs`** (new module) — `LogViewerWindow`:
   - Dedicated GTK `ApplicationWindow` per model's log file
   - `HeaderBar` with Clear, Export, and Close action buttons
   - Log file path displayed in a secondary label bar below the header
   - Scrollable `TextView` with monospace font, word-wrap, and auto-scroll to bottom as new lines arrive
   - Auto-tailing via `glib::timeout_add_local` polling every 500ms — reads newly appended bytes (tracked by byte offset) without blocking the GTK UI thread
   - Clean poller shutdown: timeout source ID removed in the window's `connect_destroy` handler
   - Clear button truncates the log file on disk and clears the TextView
   - Export button opens a GTK `FileChooserDialog` (Save mode) to copy the current buffer content to a user-selected path

2. **Log file resolution** (`resolve_log_file`):
   - Scans the log directory for files matching `{script_stem}_{YYYYMMDD_HHMMSS}.log`
   - Returns the most recent match (sorted by filename, timestamps are zero-padded)
   - Falls back to creating a new timestamped path if no existing logs found

3. **Logs button wiring** (`app/src/model_card.rs`):
   - `logs_button` enabled when card state is Ready or Error (disabled during transitions)
   - New `set_logs_handler()` method accepts a closure that opens a `LogViewerWindow` for that model's log file
   - Handler stored via `Rc<RefCell<Option<Box<dyn Fn()>>>>` — called on each button click

4. **Menu action wiring** (`app/src/window.rs`):
   - "View → Toggle Logs Panel" now creates and presents a `LogViewerWindow` for the first configured model
   - Per-card logs buttons open viewers scoped to their specific model's log file

5. **Log rotation** (`core/src/process_manager.rs`):
   - New `ProcessManager::rotate_logs()` public function: scans log directory, filters by script stem, deletes files beyond retention count (default 20)
   - Called automatically from `LinuxProcessGuard::setup()` after creating each new log file

### Dependencies added
- `chrono = "0.4"` — used in `logs_panel.rs` for fallback timestamp formatting and in `process_manager.rs` for log filename generation

### Tests
- 1 new unit test: `logs_panel::tests::test_resolve_log_file_fallback`
- All 19 core unit tests pass
- All 4 integration tests pass
- All 24 tests pass total

### Architecture decisions
- **Separate window per model (not a panel)**: Each "Logs" button opens its own `ApplicationWindow` rather than a collapsible panel within the main window. This avoids complex layout changes to the existing card container and is consistent with how the preferences dialog already works as a separate window.
- **Polling over file watching**: Used `glib::timeout_add_local` (500ms polling) instead of inotify/file watchers. This keeps the implementation simple, avoids adding a new dependency (e.g., `notify` crate), and is consistent with the existing synchronous threading model (`reqwest::blocking`, `std::thread::spawn`).
- **Byte-offset tracking for tailing**: Instead of re-reading the entire file each poll, tracks the last byte offset to only append new content. Handles file truncation (Clear button) by resetting the offset when the file becomes empty.
- **GTK object cloning for closures**: `gtk::TextView` is cloned (refcounted, cheap) and moved into the polling closure to satisfy `'static` lifetime requirements.

### Deviations from spec
- The spec mentioned a "toggle-able panel" with View → Toggle Logs Panel showing the current model's logs. Instead, the menu item opens a standalone window for the first configured model (consistent with how other dialogs work), and each card's Logs button opens a window for that specific model. This provides better UX since users can view logs for any model, not just the running one.
- The spec mentioned log rotation as a configurable default N=20. Implemented with a hardcoded default of 20 in `rotate_logs()`. Making it configurable via Preferences would be a Phase 7+ enhancement.
