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
