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
