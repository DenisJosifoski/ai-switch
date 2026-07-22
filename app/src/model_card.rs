//! Model card — one row per configured model showing name, port, toggle,
//! status text, and a (stub) Logs button.
//!
//! The card owns its widgets and the toggle handler closure.

use gtk4 as gtk;
use gtk::prelude::*;
use gtk::{Button, Label, Orientation, Box as GtkBox, ToggleButton};
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

/// A single model card widget.
pub struct ModelCard {
    /// The underlying config for this model.
    config: ai_switch_core::config::ModelConfig,
    /// Current UI-visible state (interior mutability).
    state: Rc<RefCell<CardState>>,
    /// The card container (horizontal box with all widgets).
    pub widget: GtkBox,
    /// The ON/OFF toggle button.
    toggle: ToggleButton,
    /// Status text label.
    status_label: Label,
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

        // ── Card layout ────────────────────────────────────────────
        let card = GtkBox::new(Orientation::Horizontal, 6);
        card.set_margin_start(12);
        card.set_margin_end(12);
        card.set_margin_top(6);
        card.set_margin_bottom(6);
        card.set_hexpand(true);

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

        // ON/OFF toggle
        let toggle = ToggleButton::with_label("OFF");
        toggle.set_halign(gtk::Align::End);

        // Logs button (stub)
        let logs_button = Button::with_label("Logs");
        logs_button.set_css_classes(&["flat"]);
        logs_button.set_sensitive(false); // stub — Phase 6

        card.append(&name_label);
        card.append(&status_label);
        card.append(&toggle);
        card.append(&logs_button);

        Self {
            config: config.clone(),
            state,
            widget: card,
            toggle,
            status_label,
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

    /// Mark this card as the active running model.
    pub fn set_active(&self, active: bool) {
        if active {
            self.widget.set_css_classes(&["frame"]);
        } else {
            self.widget.set_css_classes(&[]);
        }
    }
}
