//! In-app log viewer window for ai-switch.
//!
//! Displays model stdout/stderr logs and application logs with auto-tailing,
//! clearing, and export capabilities. Each model gets its own viewer window
//! scoped to that model's log file.
//!
//! Auto-tailing uses `glib::timeout_add_local` polling every 500ms to read
//! newly appended lines without blocking the GTK UI thread. The poller is
//! cleanly stopped when the viewer window is closed.

use gtk4 as gtk;
use gtk::prelude::*;

use std::cell::Cell;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// A log viewer window that displays a model's log file with auto-tailing.
///
/// Created when the user clicks the "Logs" button on a ModelCard or uses
/// the View → Toggle Logs Panel menu item. Each invocation opens a new
/// window scoped to the specified model's most recent log file.
pub struct LogViewerWindow {
    /// The GTK application window.
    widget: gtk::ApplicationWindow,
    /// The text buffer holding the current log content.
    text_buffer: gtk::TextBuffer,
    /// Path to the log file being viewed (used for clear/export).
    log_file: PathBuf,
    /// Tracks the byte offset of the last read to detect new appends.
    last_offset: Rc<Cell<usize>>,
    /// The glib timeout source ID for auto-tail polling. Stored so we can
    /// remove it when the window is destroyed.
    timeout_id: Rc<Cell<Option<glib::SourceId>>>,
}

impl LogViewerWindow {
    /// Create a new log viewer window for the given model's log file.
    ///
    /// Resolves the most recent log file from the log directory by matching
    /// the script stem (e.g., `run-llama_20260724_143022.log`).
    pub fn new(model_name: &str, script_path: &Path, log_dir: &Path) -> Self {
        let log_file = resolve_log_file(script_path, log_dir);

        // ── Window setup ───────────────────────────────────────────
        let widget = gtk::ApplicationWindow::builder()
            .title(format!("Logs — {}", model_name))
            .default_width(720)
            .default_height(500)
            .build();

        // ── Header bar with action buttons ─────────────────────────
        let header = gtk::HeaderBar::new();
        header.set_show_title_buttons(true);

        // Clear button — empties the log file and the TextView.
        let clear_btn = gtk::Button::builder()
            .label("Clear")
            .build();
        clear_btn.set_css_classes(&["flat"]);

        // Export button — opens save dialog to copy log file elsewhere.
        let export_btn = gtk::Button::builder()
            .label("Export")
            .build();
        export_btn.set_css_classes(&["flat"]);

        // Close button — destroys the window and stops the poller.
        let close_btn = gtk::Button::builder()
            .label("Close")
            .build();
        close_btn.set_css_classes(&["suggested-action", "flat"]);

        header.pack_end(&clear_btn);
        header.pack_end(&export_btn);
        header.pack_end(&close_btn);

        // ── Log file path label in the header ──────────────────────
        let filepath_label = gtk::Label::new(Some(&log_file.display().to_string()));
        filepath_label.set_css_classes(&["caption", "dim-label"]);
        filepath_label.set_halign(gtk::Align::Start);
        filepath_label.set_hexpand(true);
        filepath_label.set_margin_start(12);
        filepath_label.set_margin_end(6);
        filepath_label.set_max_width_chars(40);
        filepath_label.set_width_chars(40);

        // Put the filepath in a secondary bar below the header
        let info_bar = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        info_bar.append(&filepath_label);

        let toolbar_stack = gtk::Box::new(gtk::Orientation::Vertical, 0);
        toolbar_stack.append(&header);
        toolbar_stack.append(&info_bar);

        // ── Scrollable TextView with monospace font ────────────────
        let text_buffer = gtk::TextBuffer::new(None);
        let text_view = gtk::TextView::builder()
            .buffer(&text_buffer)
            .monospace(true)
            .editable(false)
            .wrap_mode(gtk::WrapMode::WordChar)
            .left_margin(8)
            .right_margin(8)
            .top_margin(4)
            .bottom_margin(4)
            .build();

        let scrolled = gtk::ScrolledWindow::builder()
            .hexpand(true)
            .vexpand(true)
            .build();
        scrolled.set_child(Some(&text_view));

        // ── Assemble the window ────────────────────────────────────
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.append(&toolbar_stack);
        content.append(&scrolled);

        widget.set_child(Some(&content));

        // Store the poller ID so we can clean it up on window close.
        let timeout_id = Rc::new(Cell::new(None::<glib::SourceId>));
        let last_offset = Rc::new(Cell::new(0usize));

        // Wire up button actions and start the auto-tail poller.
        // All done in a block so `lo_rc` is available for Self construction.
        let lo_rc;
        {
            let tid_rc = Rc::clone(&timeout_id);
            lo_rc = Rc::clone(&last_offset);

            // Clear button handler.
            let text_buffer_clear = text_buffer.clone();
            let log_file_clear = log_file.clone();
            clear_btn.connect_clicked(move |_| {
                if let Err(e) = fs::write(&log_file_clear, "") {
                    tracing::warn!("Failed to clear log file: {}", e);
                }
                text_buffer_clear.set_text("");
            });

            // Export button handler.
            let log_file_export = log_file.clone();
            let text_buffer_export = text_buffer.clone();
            export_btn.connect_clicked(move |_| {
                let dialog = gtk::FileChooserDialog::builder()
                    .title("Export Logs")
                    .action(gtk::FileChooserAction::Save)
                    .build();
                dialog.add_button("Cancel", gtk::ResponseType::Cancel);
                dialog.add_button("Save", gtk::ResponseType::Accept);

                if let Some(file_name) = log_file_export.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                {
                    dialog.set_current_name(&file_name);
                }

                let tb = text_buffer_export.clone();
                dialog.connect_response(move |dlg, response| {
                    if response == gtk::ResponseType::Accept {
                        if let Some(target) = dlg.file() {
                            let (start, end) = tb.bounds();
                            let content = tb.text(&start, &end, true);
                            if let Some(dest) = target.path() {
                                if let Err(e) = fs::write(dest, content.as_str()) {
                                    tracing::error!("Failed to export logs: {}", e);
                                }
                            }
                        }
                    }
                    dlg.destroy();
                });

                dialog.present();
            });

            // Clone tid_rc for the destroy handler.
            let tid_rc_destroy = Rc::clone(&tid_rc);
            widget.connect_destroy(move |_win| {
                if let Some(id) = tid_rc_destroy.take() {
                    id.remove();
                }
            });

            // Start the auto-tail poller.
            let tid = start_tail_poller(
                log_file.clone(),
                text_view.clone(),
                lo_rc.clone(),
                Rc::clone(&tid_rc),
            );
            tid_rc.set(Some(tid));
        }

        Self {
            widget,
            text_buffer,
            log_file,
            last_offset: lo_rc.clone(),
            timeout_id,
        }
    }

    /// Present the window (make it visible and raise it).
    pub fn present(&self) {
        self.widget.present();
    }
}

/// Resolve the most recent log file for a given script path.
///
/// Log files follow the pattern `{script_stem}_{YYYYMMDD_HHMMSS}.log`.
/// This function scans the log directory, filters by script stem, and
/// returns the file with the latest timestamp (alphabetical order works
/// because timestamps are fixed-width zero-padded).
fn resolve_log_file(script_path: &Path, log_dir: &Path) -> PathBuf {
    let script_stem = script_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();

    if let Ok(entries) = fs::read_dir(log_dir) {
        let mut matches: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                if !name.ends_with(".log") {
                    return None;
                }
                // Match: {script_stem}_{YYYYMMDD_HHMMSS}.log
                let prefix = format!("{}_", script_stem);
                if !name.starts_with(&prefix) {
                    return None;
                }
                Some(e.path())
            })
            .collect();

        // Sort descending — most recent first (timestamps are zero-padded).
        matches.sort_by(|a, b| b.file_name().cmp(&a.file_name()));

        if let Some(most_recent) = matches.first() {
            return most_recent.clone();
        }
    }

    // Fallback: create a new log file path with the current timestamp.
    let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
    log_dir.join(format!("{}_{}.log", script_stem, timestamp))
}

/// Start the auto-tail polling loop.
///
/// Reads newly appended bytes from the log file every 500ms and appends
/// them to the text buffer. The poller stops when the source ID is removed
/// (which happens in the window's `connect_destroy` handler).
fn start_tail_poller(
    log_file: PathBuf,
    text_view: gtk::TextView,
    last_offset: Rc<Cell<usize>>,
    _timeout_id: Rc<Cell<Option<glib::SourceId>>>,
) -> glib::SourceId {
    glib::timeout_add_local(std::time::Duration::from_millis(500), move || {
        // Read the entire file content.
        let content = match fs::read_to_string(&log_file) {
            Ok(c) => c,
            Err(_) => return glib::ControlFlow::Continue, // File doesn't exist yet or unreadable
        };

        let current_offset = last_offset.get();
        let new_bytes_len = content.len().saturating_sub(current_offset);

        if new_bytes_len > 0 {
            let text_buffer = text_view.buffer();
            let mut end_iter = text_buffer.end_iter();
            text_buffer.insert(&mut end_iter, &content[current_offset..]);

            // Auto-scroll to the bottom.
            let bot = text_buffer.end_iter();
            let mut bot_mut = bot.clone();
            text_view.scroll_to_iter(&mut bot_mut, 0.0, true, 0.0, 1.0);

            last_offset.set(current_offset + new_bytes_len);
        } else if content.is_empty() && current_offset > 0 {
            // File was truncated (e.g., by Clear button) — reset offset.
            last_offset.set(0);
        }

        glib::ControlFlow::Continue
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_log_file_fallback() {
        // When no log files exist, it should return a sensible fallback path.
        let temp_dir = std::env::temp_dir().join("ai-switch-test-logs");
        let _ = fs::create_dir_all(&temp_dir);

        let script = PathBuf::from("/tmp/test-model.sh");
        let result = resolve_log_file(&script, &temp_dir);

        assert!(result.starts_with(&temp_dir));
        // Check the filename (not the full path) starts with the expected prefix.
        let filename = result.file_name().unwrap_or_default().to_string_lossy();
        assert!(filename.starts_with("test-model_"));
        assert!(result.to_string_lossy().ends_with(".log"));

        // Cleanup.
        let _ = fs::remove_dir_all(&temp_dir);
    }
}
