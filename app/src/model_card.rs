//! Model card — one row per configured model showing name, port, toggle,
//! status text, context usage, restart button, and a (stub) Logs button.
//!
//! The card owns its widgets and the toggle handler closure.

use gtk4 as gtk;
use gtk::prelude::*;
use gtk::{
    Box as GtkBox, Button, Label, Orientation, ToggleButton,
};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// UI-visible model state.
#[derive(Debug, Clone, PartialEq)]
pub enum CardState {
    Stopped,
    Starting,
    Loading,
    Ready,
    Error(String),
}

impl CardState {
    pub fn status_text(&self) -> &str {
        match self {
            Self::Stopped => "Stopped",
            Self::Starting => "Starting...",
            Self::Loading => "Loading...",
            Self::Ready => "Ready",
            Self::Error(msg) => msg,
        }
    }

    pub fn is_on(&self) -> bool {
        matches!(self, Self::Ready | Self::Starting | Self::Loading)
    }

    pub fn is_transitioning(&self) -> bool {
        matches!(self, Self::Starting | Self::Loading)
    }
}

/// Context polling state for a single model card.
#[derive(Debug, Clone, PartialEq)]
pub enum PollingState {
    /// No polling active (model stopped or not yet started).
    Inactive,
    /// Polling /slots is active and the last check succeeded.
    Active {
        /// Tokens used in the current slot.
        tokens_used: usize,
        /// Total context window size (n_ctx).
        n_ctx: usize,
    },
    /// Polling is active but the last /slots request failed.
    Error,
}

impl PollingState {
    fn is_active(&self) -> bool {
        !matches!(self, Self::Inactive)
    }
}

/// A single model card widget.
pub struct ModelCard {
    /// The underlying config for this model.
    config: ai_switch_core::config::ModelConfig,
    /// Current UI-visible state (interior mutability).
    state: Rc<RefCell<CardState>>,
    /// Context polling state (interior mutability).
    polling_state: Rc<RefCell<PollingState>>,
    /// The card container (horizontal box with all widgets).
    pub widget: GtkBox,
    /// The ON/OFF toggle button.
    toggle: ToggleButton,
    /// Status text label.
    status_label: Label,
    /// Context usage label (e.g. "1,024 / 32,000 (3.2%)").
    context_label: Label,
    /// Restart button.
    pub restart_button: Button,
    /// Logs button (stub — wired in Phase 6).
    logs_button: Button,
    /// Blocks the toggle handler during programmatic state changes.
    /// Prevents GTK4's re-entrant `toggled` signal from spawning
    /// unwanted stop/switch threads when set_state/set_starting
    /// programmatically changes the button's active state.
    signal_block: Rc<Cell<bool>>,
}

impl ModelCard {
    /// Create a new model card from a model config.
    pub fn new(config: &ai_switch_core::config::ModelConfig) -> Self {
        let state = Rc::new(RefCell::new(CardState::Stopped));
        let polling_state = Rc::new(RefCell::new(PollingState::Inactive));

        // ── Card layout ────────────────────────────────────────────
        let card = GtkBox::new(Orientation::Vertical, 4);
        card.set_margin_start(12);
        card.set_margin_end(12);
        card.set_margin_top(6);
        card.set_margin_bottom(6);
        card.set_hexpand(true);

        // ── Top row: name + port, status, controls ─────────────────
        let top_row = GtkBox::new(Orientation::Horizontal, 8);
        top_row.set_hexpand(true);

        // Name + port label
        let name_label = Label::new(Some(&format!(
            "{} (port {})",
            config.name, config.port
        )));
        name_label.set_halign(gtk::Align::Start);
        name_label.set_hexpand(true);

        // Status text
        let status_label = Label::new(Some("Stopped"));
        status_label.set_css_classes(&["caption"]);
        status_label.set_halign(gtk::Align::Start);

        // Controls (toggle + restart + logs)
        let controls = GtkBox::new(Orientation::Horizontal, 4);

        // ON/OFF toggle
        let toggle = ToggleButton::with_label("OFF");
        toggle.set_halign(gtk::Align::End);

        // Restart button
        let restart_button = Button::with_label("Restart");
        restart_button.set_css_classes(&["flat"]);
        restart_button.set_sensitive(false); // disabled until model is Ready/Error

        // Logs button (stub)
        let logs_button = Button::with_label("Logs");
        logs_button.set_css_classes(&["flat"]);
        logs_button.set_sensitive(false); // stub — Phase 6

        controls.append(&toggle);
        controls.append(&restart_button);
        controls.append(&logs_button);

        top_row.append(&name_label);
        top_row.append(&status_label);
        top_row.append(&controls);

        // ── Context usage label ────────────────────────────────────
        let context_label = Label::new(Some(""));
        context_label.set_css_classes(&["caption", "dim-label"]);
        context_label.set_halign(gtk::Align::Start);

        card.append(&top_row);
        card.append(&context_label);

        Self {
            config: config.clone(),
            state,
            polling_state,
            widget: card,
            toggle,
            status_label,
            context_label,
            restart_button,
            logs_button,
            signal_block: Rc::new(Cell::new(false)),
        }
    }

    /// Set the toggle callback. Called by MainWindow after construction.
    ///
    /// GTK stores the closure for the lifetime of the toggle button, so the
    /// handler stays alive automatically — no need to store it separately.
    pub fn set_toggle_handler(&mut self, handler: impl Fn(bool) + 'static) {
        let handler = Rc::new(handler);
        let guard = Rc::clone(&self.signal_block);
        let handler_ref = Rc::clone(&handler);
        self.toggle.connect_toggled(move |btn| {
            // Block re-entrant calls triggered by programmatic set_active()
            if guard.get() {
                return;
            }
            handler_ref(btn.is_active());
        });
    }

    /// Return a reference to the model's config.
    pub fn config(&self) -> &ai_switch_core::config::ModelConfig {
        &self.config
    }

    /// Get the current UI-visible state.
    pub fn state(&self) -> CardState {
        self.state.borrow().clone()
    }

    /// Get the current context polling state.
    pub fn polling_state(&self) -> PollingState {
        self.polling_state.borrow().clone()
    }

    /// Set the current UI-visible state and update all widgets.
    pub fn set_state(&self, new_state: CardState) {
        // Block re-entrant toggled signals from GTK4 before any programmatic
        // set_active() calls — the handler checks this guard and returns early.
        self.block_signals();
        let is_on = new_state.is_on();
        let transitioning = new_state.is_transitioning();

        self.toggle.set_label(if is_on { "ON" } else { "OFF" });
        self.toggle.set_active(is_on);
        self.toggle.set_sensitive(!transitioning);
        self.status_label.set_text(new_state.status_text());

        // Update restart button sensitivity: enabled only when Ready or Error.
        self.restart_button.set_sensitive(
            matches!(&new_state, CardState::Ready | CardState::Error(_)),
        );

        *self.state.borrow_mut() = new_state;
        self.unblock_signals();
    }

    /// Set the card to "Starting..." and disable the toggle.
    pub fn set_starting(&self) {
        self.block_signals();
        self.toggle.set_label("ON");
        self.toggle.set_active(true);
        self.toggle.set_sensitive(false);
        self.status_label.set_text("Starting...");
        self.restart_button.set_sensitive(false);
        *self.state.borrow_mut() = CardState::Starting;
        self.unblock_signals();
    }

    /// Disable the toggle button.
    pub fn disable_toggle(&self) {
        self.toggle.set_sensitive(false);
    }

    /// Re-enable the toggle button if not in a transitioning state.
    pub fn enable_toggle(&self) {
        let current = self.state.borrow().clone();
        if !current.is_transitioning() {
            self.toggle.set_sensitive(true);
        }
    }

    /// Block the toggle handler to prevent re-entrant `toggled` signals.
    /// Call before any programmatic state changes that will call
    /// `set_active()` on the toggle button.
    pub fn block_signals(&self) {
        self.signal_block.set(true);
    }

    /// Unblock the toggle handler after a programmatic state change.
    pub fn unblock_signals(&self) {
        self.signal_block.set(false);
    }

    /// Mark this card as the active running model (highlight border).
    pub fn set_active(&self, active: bool) {
        if active {
            self.widget.set_css_classes(&["frame"]);
        } else {
            self.widget.set_css_classes(&[]);
        }
    }

    /// Update the context usage display and polling state.
    ///
    /// Called from the main thread (via `glib::MainContext::default().invoke()`)
    /// when a new /slots response is received.
    pub fn set_context(&self, tokens_used: usize, n_ctx: usize) {
        self.block_signals();

        // Update polling state.
        *self.polling_state.borrow_mut() = PollingState::Active {
            tokens_used,
            n_ctx,
        };

        // Format the context label: "Context: 1,024 / 32,000 (3.2%)".
        let percentage = if n_ctx > 0 {
            tokens_used as f64 / n_ctx as f64 * 100.0
        } else {
            0.0
        };
        // Manual thousands separator formatting.
        let fmt = |n: usize| -> String {
            let s = n.to_string();
            let chars: Vec<char> = s.chars().rev().collect();
            let mut result = String::new();
            for (i, ch) in chars.iter().enumerate() {
                if i > 0 && i % 3 == 0 {
                    result.push(',');
                }
                result.push(*ch);
            }
            let mut padded = result.chars().rev().collect::<String>();
            while padded.len() < 8 {
                padded = format!(" {}", padded);
            }
            padded
        };
        let text = format!(
            "Context: {} / {} ({:.1}%)",
            fmt(tokens_used),
            fmt(n_ctx),
            percentage
        );
        self.context_label.set_text(&text);

        // Color-code based on usage: red when >= 90%.
        if percentage >= 90.0 {
            self.context_label.set_css_classes(&["caption", "error"]);
        } else {
            self.context_label.set_css_classes(&["caption", "dim-label"]);
        }

        self.unblock_signals();
    }

    /// Clear the context display and set polling to inactive.
    pub fn clear_context(&self) {
        self.block_signals();
        *self.polling_state.borrow_mut() = PollingState::Inactive;
        self.context_label.set_text("");
        self.unblock_signals();
    }

    /// Mark the restart button as "Restarting…" and disable it.
    pub fn disable_restart(&self) {
        self.block_signals();
        self.restart_button.set_label("Restarting…");
        self.restart_button.set_sensitive(false);
        self.unblock_signals();
    }

    /// Restore the restart button to its normal state.
    pub fn enable_restart(&self) {
        self.block_signals();
        self.restart_button.set_label("Restart");
        let current = self.state.borrow().clone();
        self.restart_button.set_sensitive(
            matches!(&current, CardState::Ready | CardState::Error(_)),
        );
        self.unblock_signals();
    }

    /// Check if a restart is currently in progress (button shows "Restarting…").
    pub fn restart_requested(&self) -> bool {
        self.restart_button.label()
            .map(|l| l.contains("Restarting"))
            .unwrap_or(false)
    }
}
