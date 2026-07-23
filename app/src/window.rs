//! Main application window for ai-switch.
//!
//! Contains the menu bar, scrollable model card list, and status bar.
//! Wires up menu actions (Quit, Refresh, About, GitHub, Preferences) and
//! handles async model start/stop operations on background threads.
//!
//! Phase 4 additions:
//! - Context display: polls `GET /slots` every 2s while a model is Ready
//! - Auto-restart on context full (>=98% of n_ctx)
//! - Restart button per model card
//! - Preferences dialog (Edit → Preferences)

use gtk4 as gtk;
use gtk::prelude::*;
use gtk::{
    Application, ApplicationWindow, Box as GtkBox, Label,
    MessageDialog, MessageType, Orientation, ResponseType, ScrolledWindow,
};
use std::rc::Rc;
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::menu;
use crate::model_card::{CardState, ModelCard};
use crate::preferences::PreferencesDialog;
use ai_switch_core::config::Config;
use ai_switch_core::process_manager::{ProcessError, PortState, ProcessManager, Pid};

/// Messages sent from background threads to the main GUI thread.
enum ChannelMessage {
    /// A switch (start or switch_model) completed.
    SwitchCompleted { target_id: String, result: Result<(), ProcessError> },
    /// A stop completed.
    StopCompleted { running_id: String, result: Result<(), ProcessError> },
    /// A restart was manually triggered by the user via the Restart button.
    RestartRequested { model_id: String },
}

/// Context update sent from the polling thread to the main loop.
struct SlotUpdate {
    model_id: String,
    tokens_used: usize,
    n_ctx: usize,
}

/// A polled /slots response for a single model.
struct SlotInfo {
    tokens_used: usize,
    n_ctx: usize,
}

/// The main application window.
pub struct MainWindow {
    widget: ApplicationWindow,
    cards: Rc<RefCell<Vec<ModelCard>>>,
    /// Tracks the keep-alive signal for the active background thread.
    current_keep_alive: Rc<RefCell<Option<Arc<AtomicBool>>>>,
    /// Shared config — needed by the context polling thread and preferences.
    config: Config,
    /// Path to the config file on disk (for saving preferences).
    config_path: std::path::PathBuf,
}

impl MainWindow {
    pub fn new(app: &Application, config: Config) -> Self {
        let widget = ApplicationWindow::builder()
            .application(app)
            .title("ai-switch")
            .default_width(640)
            .default_height(520)
            .build();

        Self::wire_actions(&widget, app);

        let main_vbox = GtkBox::new(Orientation::Vertical, 0);

        // Menu bar at the top (permanently visible native menu bar)
        let menubar = menu::build_menu_bar();
        main_vbox.append(&menubar);

        // Build the cards container and retrieve the list of cards.
        let cards = Rc::new(RefCell::new(
            config.models.iter().map(ModelCard::new).collect::<Vec<_>>()
        ));

        let cards_scroll = Self::build_cards_container(&cards);
        main_vbox.append(&cards_scroll);

        // Status bar at the bottom
        let status_bar = Self::build_status_bar();
        main_vbox.append(&status_bar);

        widget.set_child(Some(&main_vbox));

        // Resolve config file path for preferences saving.
        let config_path = Config::resolve_path().unwrap_or_else(|| {
            std::path::PathBuf::from("/nonexistent/config.toml")
        });

        // Shared process manager (Arc<Mutex> for cross-thread access).
        let pm = Arc::new(Mutex::new(
            ProcessManager::new(config.clone()),
        ));

        let current_keep_alive = Rc::new(RefCell::new(None::<Arc<AtomicBool>>));

       // Reconcile: detect any models already running from a previous session
        // and populate the PM's internal state so stop/switch work correctly.
        let mut restored_model_id = None;
        {
            let mut pm_guard = pm.lock().unwrap();
            let mut running_model_found = None;
            for model in pm_guard.config().models.iter() {
                if matches!(ProcessManager::check_port(model.port), PortState::OccupiedByModel) {
                    let pid = ProcessManager::get_port_pid(model.port).ok();
                    running_model_found = Some((model.clone(), pid));
                    break; // only one model can be running at a time
                }
            }
            if let Some((model, pid)) = running_model_found {
                restored_model_id = Some(model.id.clone());
                let guard = ai_switch_core::process_manager::LinuxProcessGuard {
                    pid: pid.map(|p| Pid::from_raw(p as i32)),
                    port: model.port,
                    shutdown_timeout_sec: 10,
                };
                pm_guard.set_running_model(
                    ai_switch_core::process_manager::RunningModel {
                        id: model.id.clone(),
                        guard: Box::new(guard),
                        state: ai_switch_core::process_manager::ModelState::Ready,
                    },
                );
            }
        }

        // Pre-set the restored model's card to Ready state before showing window
        if let Some(restored_id) = &restored_model_id {
            for c in cards.borrow_mut().iter_mut() {
                if c.config().id == *restored_id {
                    c.set_state(CardState::Ready);
                    c.set_active(true); // Ensure GTK toggle switch visually turns ON
                }
            }
        }

        // App exit cleanup: stop the active model when the window is closed.
        let pm_cleanup = Arc::clone(&pm);
        widget.connect_close_request(move |_win| {
            let running_id = {
                let pm_guard = pm_cleanup.lock().unwrap();
                pm_guard.get_running_model_id().map(String::from)
            };
            if let Some(id) = running_id {
                let mut pm_lock = pm_cleanup.lock().unwrap();
                let _ = pm_lock.stop_model(&id, true);
            }
            glib::Propagation::Proceed
        });

        // Create standard thread-safe channel for process management messages.
        let (sender, receiver) = std::sync::mpsc::channel::<ChannelMessage>();
        let sender_poll = sender.clone();

        // Channel for context slot updates from the polling thread.
        let (slot_sender, slot_receiver) = std::sync::mpsc::channel::<SlotUpdate>();

        // Poll channel messages on the main context loop using a timeout.
        // This handles both process management and context update messages.
        let cards_clone = Rc::clone(&cards);
        glib::timeout_add_local(std::time::Duration::from_millis(50), move || {
            let mut cards_borrow = cards_clone.borrow_mut();

            // Process process-management messages first.
            while let Ok(msg) = receiver.try_recv() {
                match msg {
                    ChannelMessage::SwitchCompleted { target_id, result } => {
                        for c in cards_borrow.iter_mut() {
                            let cid = c.config().id.clone();
                            if cid == target_id {
                                match &result {
                                    Ok(()) => {
                                        c.set_state(CardState::Ready);
                                        c.set_active(true);
                                    }
                                    Err(e) => {
                                        c.set_state(CardState::Error(format!(
                                            "Failed to start: {}", e
                                        )));
                                    }
                                }
                            } else {
                                c.set_state(CardState::Stopped);
                                c.set_active(false);
                            }
                            c.enable_toggle();
                            c.enable_restart();
                        }
                    }
                    ChannelMessage::StopCompleted { running_id, result } => {
                        for c in cards_borrow.iter_mut() {
                            if c.config().id == running_id {
                                match &result {
                                    Ok(()) => c.set_state(CardState::Stopped),
                                    Err(e) => c.set_state(CardState::Error(format!(
                                        "Failed to stop: {}", e
                                    ))),
                                }
                                c.enable_toggle();
                                c.enable_restart();
                            }
                        }
                    }
                    ChannelMessage::RestartRequested { model_id } => {
                        for c in cards_borrow.iter_mut() {
                            if c.config().id == model_id {
                                c.disable_restart();
                            } else {
                                c.set_state(CardState::Stopped);
                                c.set_active(false);
                            }
                            c.enable_toggle();
                        }
                    }
                }
            }

            // Process context slot updates.
            while let Ok(update) = slot_receiver.try_recv() {
                for c in cards_borrow.iter_mut() {
                    if c.config().id == update.model_id {
                        c.set_context(update.tokens_used, update.n_ctx);
                    }
                }
            }

            glib::ControlFlow::Continue
        });

        // Start the context polling thread.
        let pm_poll = Arc::clone(&pm);
        let auto_restart_enabled = config.auto_restart_on_context_full();
        Self::spawn_context_poller(
            pm_poll,
            sender_poll,
            slot_sender,
            auto_restart_enabled,
        );

        // Wire toggle and restart handlers.
        {
            let pm_clone = Arc::clone(&pm);
            let keep_alive_ref = Rc::clone(&current_keep_alive);
            let sender_ref = sender.clone();

            // Clone shared references BEFORE entering the closure scope so they
            // aren't moved into the toggle handler and become unavailable for
            // the restart button handler below.
            let cards_for_toggle = Rc::clone(&cards);
            let sender_for_toggle = sender_ref.clone();
            let pm_for_toggle = Arc::clone(&pm_clone);

            let mut cards_borrow = cards.borrow_mut();
            for card in cards_borrow.iter_mut() {
                let model_id = card.config().id.clone();

                // Clone model_id before the toggle handler since it moves the value.
                let model_id_toggle = model_id.clone();
                let model_id_restart = model_id.clone();

                // ── Toggle handler ───────────────────────────────────
                {
                    let ka_ref = Rc::clone(&keep_alive_ref);
                    let cards_inner = cards_for_toggle.clone();
                    let sender_inner = sender_for_toggle.clone();
                    let pm_ref = Arc::clone(&pm_for_toggle);

                    card.set_toggle_handler(move |on| {
                        let cards_inner = cards_inner.borrow();
                        if on {
                            let target_card = match cards_inner.iter().find(|c| c.config().id == model_id_toggle) {
                                Some(c) => c,
                                None => return,
                            };
                            if target_card.state().is_transitioning() {
                                return;
                            }
                            for c in cards_inner.iter() {
                                if !c.state().is_transitioning() {
                                    c.disable_toggle();
                                }
                            }
                            target_card.set_starting();

                            if let Some(ref old_ka) = *ka_ref.borrow() {
                                old_ka.store(false, Ordering::SeqCst);
                            }

                            let new_ka = Arc::new(AtomicBool::new(true));
                            *ka_ref.borrow_mut() = Some(Arc::clone(&new_ka));

                            let bg_model_id = model_id_toggle.clone();
                            let pm_thread = Arc::clone(&pm_ref);
                            let sender_thread = sender_inner.clone();
                            let ka_thread = Arc::clone(&new_ka);

                            std::thread::spawn(move || {
                                let result = {
                                    let mut pm_lock = match pm_thread.lock() {
                                        Ok(g) => g,
                                        Err(_) => return,
                                    };
                                    if pm_lock.get_running_model_id() == Some(bg_model_id.as_str()) {
                                        return;
                                    }
                                    let running_id = pm_lock.get_running_model_id().unwrap_or("").to_string();
                                    if running_id.is_empty() {
                                        pm_lock.start_model(&bg_model_id)
                                    } else {
                                        pm_lock.switch_model(&running_id, &bg_model_id)
                                    }
                                };

                                let is_ok = result.is_ok();
                                let _ = sender_thread.send(ChannelMessage::SwitchCompleted {
                                    target_id: bg_model_id,
                                    result,
                                });

                                // Keep thread alive after successful start so Linux
                                // PR_SET_PDEATHSIG does not kill the newly spawned process.
                                if is_ok {
                                    while ka_thread.load(Ordering::SeqCst) {
                                        std::thread::sleep(std::time::Duration::from_millis(100));
                                    }
                                }
                            });
                        } else {
                            if let Some(ref old_ka) = *ka_ref.borrow() {
                                old_ka.store(false, Ordering::SeqCst);
                            }
                            *ka_ref.borrow_mut() = None;

                            let pm_thread = Arc::clone(&pm_ref);
                            let sender_thread = sender_inner.clone();

                            std::thread::spawn(move || {
                                let mut pm_lock = match pm_thread.lock() {
                                    Ok(g) => g,
                                    Err(_) => return,
                                };
                                if let Some(running_id) = pm_lock.get_running_model_id().map(String::from) {
                                    let result = pm_lock.stop_model(&running_id, false);
                                    let _ = sender_thread.send(ChannelMessage::StopCompleted {
                                        running_id,
                                        result,
                                    });
                                }
                            });
                        }
                    });
                }

                // ── Restart button handler ───────────────────────────
                {
                    let cards_restart = cards_for_toggle.clone();
                    let sender_restart = sender_ref.clone();
                    let pm_restart = Arc::clone(&pm_clone);
                    let ka_ref_restart = Rc::clone(&keep_alive_ref);

                    card.restart_button.connect_clicked(move |_| {
                        let cards_inner = cards_restart.borrow();
                        let target = match cards_inner.iter().find(|c| c.config().id == model_id_restart) {
                            Some(c) => c,
                            None => return,
                        };

                        if target.state().is_transitioning() || target.restart_requested() {
                            return;
                        }

                        target.disable_restart();

                        // Signal previous keep-alive thread to exit.
                        if let Some(ref old_ka) = *ka_ref_restart.borrow() {
                            old_ka.store(false, Ordering::SeqCst);
                        }

                        // Create new keep-alive guard.
                        let new_ka = Arc::new(AtomicBool::new(true));
                        *ka_ref_restart.borrow_mut() = Some(Arc::clone(&new_ka));

                        let bg_model_id = model_id_restart.clone();
                        let pm_thread = Arc::clone(&pm_restart);
                        let sender_thread = sender_restart.clone();
                        let ka_thread = new_ka;

                        std::thread::spawn(move || {
                            let _ = sender_thread.send(ChannelMessage::RestartRequested {
                                model_id: bg_model_id.clone(),
                            });

                            let result = {
                                let mut pm_lock = match pm_thread.lock() {
                                    Ok(g) => g,
                                    Err(_) => return,
                                };

                                if pm_lock.get_running_model_id() == Some(bg_model_id.as_str()) {
                                    let _ = pm_lock.stop_model(&bg_model_id, false);
                                    std::thread::sleep(std::time::Duration::from_millis(500));
                                }

                                pm_lock.start_model(&bg_model_id)
                            };

                            let is_ok = result.is_ok();
                            let _ = sender_thread.send(ChannelMessage::SwitchCompleted {
                                target_id: bg_model_id,
                                result,
                            });

                            // Keep thread alive after successful restart so Linux
                            // PR_SET_PDEATHSIG does not kill the newly spawned process.
                            if is_ok {
                                while ka_thread.load(Ordering::SeqCst) {
                                    std::thread::sleep(std::time::Duration::from_millis(100));
                                }
                            }
                        });
                    });
                }
            }
        }

        Self {
            widget,
            cards,
            current_keep_alive,
            config,
            config_path,
        }
    }

    pub fn show(&self) {
        self.widget.show();
    }

    /// Wire up application-level actions for menu items.
    fn wire_actions(window: &ApplicationWindow, app: &Application) {
        let quit_action = gio::SimpleAction::new("quit", None);
        quit_action.connect_activate(glib::clone!(
            #[weak]
            app,
            move |_, _| {
                app.quit();
            }
        ));
        window.add_action(&quit_action);

        let refresh_action = gio::SimpleAction::new("refresh", None);
        refresh_action.connect_activate(|_, _| {
            tracing::info!("Refresh requested (stub)");
        });
        window.add_action(&refresh_action);

        let about_action = gio::SimpleAction::new("about", None);
        about_action.connect_activate(glib::clone!(
            #[weak]
            window,
            move |_, _| {
                Self::show_about_dialog(&window);
            }
        ));
        window.add_action(&about_action);

        let github_action = gio::SimpleAction::new("github", None);
        github_action.connect_activate(|_, _| {
            let _ = gio::AppInfo::launch_default_for_uri(
                "https://github.com/DenisJosifoski/ai-switch",
                None::<&gio::AppLaunchContext>,
            );
        });
        window.add_action(&github_action);

        // Preferences action — Edit → Preferences.
        let preferences_action = gio::SimpleAction::new("preferences", None);
        preferences_action.connect_activate(glib::clone!(
            #[weak]
            window,
            move |_, _| {
                Self::show_preferences_dialog(&window);
            }
        ));
        window.add_action(&preferences_action);
    }

    /// Show the preferences dialog (non-blocking).
    ///
    /// Uses GTK's `connect_response` callback so the main loop is never
    /// blocked waiting for user input — we handle Ok/Cancel inline.
    fn show_preferences_dialog(parent: &ApplicationWindow) {
        let config = match Config::load() {
            Ok(cfg) => cfg,
            Err(e) => {
                let dialog = MessageDialog::new(
                    Some(parent),
                    gtk::DialogFlags::MODAL,
                    MessageType::Error,
                    gtk::ButtonsType::Close,
                    &format!("Failed to load config:\n\n{}", e),
                );
                dialog.set_title(Some("ai-switch — Config Error"));
                dialog.connect_response(|d, _| d.destroy());
                dialog.present();
                return;
            }
        };

        let dialog = PreferencesDialog::new(parent, &config);
        let config_path = Config::resolve_path().unwrap_or_else(|| {
            std::path::PathBuf::from("/nonexistent/config.toml")
        });
        let parent_clone = parent.clone();

        // Non-blocking: handle response via callback instead of blocking.
        // Clone the dialog so we can move the original into the closure and
        // call .values() inside it — this reads the user's edited form inputs
        // after they click Save, not the stale initial values.
        let _dialog_clone = dialog.clone();

        dialog.widget.connect_response(move |d, response| {
            if response == ResponseType::Ok {
                let values = _dialog_clone.values();
                match Self::save_preferences(&values, &config_path) {
                    Ok(()) => {
                        tracing::info!("Preferences saved successfully");
                    }
                    Err(e) => {
                        let error_dialog = MessageDialog::new(
                            Some(&parent_clone),
                            gtk::DialogFlags::MODAL,
                            MessageType::Error,
                            gtk::ButtonsType::Close,
                            &e,
                        );
                        error_dialog.set_title(Some("ai-switch — Save Error"));
                        error_dialog.connect_response(|ed, _| ed.destroy());
                        error_dialog.present();
                    }
                }
            }
            d.destroy();
        });

        dialog.widget.show();
    }

    /// Save the given preferences values to disk.
    fn save_preferences(
        values: &crate::preferences::PreferencesValues,
        config_path: &std::path::Path,
    ) -> Result<(), String> {
        let mut config = Config::load().map_err(|e| format!("Failed to load config: {}", e))?;
        config.global.log_dir = values.log_dir.clone();
        config.global.proxy_port = values.proxy_port;
        config.global.auto_restart_on_context_full = Some(values.auto_restart_on_context_full);

        Config::validate(&config, config_path).map_err(|e| format!("Config validation error: {}", e))?;

        let content = toml::to_string_pretty(&config)
            .map_err(|e| format!("Failed to serialize config: {}", e))?;
        std::fs::write(config_path, &content)
            .map_err(|e| format!("Failed to write config file: {}", e))?;

        Ok(())
    }

    /// Build a scrollable container holding model cards.
    fn build_cards_container(cards: &Rc<RefCell<Vec<ModelCard>>>) -> ScrolledWindow {
        let scrolled = ScrolledWindow::new();
        scrolled.set_hexpand(true);
        scrolled.set_vexpand(true);
        scrolled.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);

        let card_box = GtkBox::new(Orientation::Vertical, 4);
        card_box.set_margin_top(8);
        card_box.set_margin_bottom(8);

        for card in cards.borrow().iter() {
            card_box.append(&card.widget);
        }

        scrolled.set_child(Some(&card_box));
        scrolled
    }

    /// Build a simple status bar at the bottom of the window.
    fn build_status_bar() -> Label {
        let label = Label::new(Some("ai-switch v0.1.0 — GTK4 native shell"));
        label.set_css_classes(&["caption"]);
        label.set_halign(gtk::Align::Start);
        label.set_margin_start(12);
        label.set_margin_end(12);
        label.set_margin_bottom(6);
        label
    }

    /// Show the About dialog.
    fn show_about_dialog(parent: &ApplicationWindow) {
        let dialog = gtk::AboutDialog::builder()
            .program_name("ai-switch")
            .version("0.1.0")
            .comments(
                "Native Linux desktop app for starting, stopping, and \
                 monitoring local llama.cpp model servers.",
            )
            .license("MIT")
            .website("https://github.com/DenisJosifoski/ai-switch")
            .website_label("GitHub")
            .authors(vec!["ai-switch contributors"])
            .build();

        dialog.set_transient_for(Some(parent));
        dialog.present();
    }

    /// Spawn the context polling thread.
    ///
    /// This thread runs on a background std::thread and polls `GET /slots`
    /// on every Ready model's port every 2 seconds. It sends SlotUpdate
    /// messages through a channel, which are processed in the main loop
    /// (where GTK widgets can be safely updated).
    fn spawn_context_poller(
        pm: Arc<Mutex<ProcessManager>>,
        sender: std::sync::mpsc::Sender<ChannelMessage>,
        slot_sender: std::sync::mpsc::Sender<SlotUpdate>,
        auto_restart_enabled: bool,
    ) {
        let http_client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .expect("failed to build reqwest blocking client");

        let poll_running = Arc::new(AtomicBool::new(true));
        let poll_running_clone = Arc::clone(&poll_running);

        std::thread::spawn(move || {
            let mut last_poll_attempt = std::time::Instant::now();

            while poll_running_clone.load(Ordering::SeqCst) {
                // Wait 2 seconds between polls.
                for _ in 0..20 {
                    if !poll_running_clone.load(Ordering::SeqCst) {
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }

                // Skip if we polled too recently.
                if last_poll_attempt.elapsed() < std::time::Duration::from_secs(1) {
                    continue;
                }

                // Collect Ready model ports from the process manager.
                let ready_ports: Vec<(String, u16)> = match pm.lock() {
                    Ok(pm_lock) => pm_lock
                        .get_running_model()
                        .map(|rm| {
                            let id = rm.id.clone();
                            let port = pm_lock
                                .config()
                                .models
                                .iter()
                                .find(|m| m.id == id)
                                .map(|m| m.port)
                                .unwrap_or(0);
                            vec![(id, port)]
                        })
                        .unwrap_or_default(),
                    Err(_) => continue,
                };

                for (model_id, port) in &ready_ports {
                    let url = format!("http://127.0.0.1:{}/slots", port);

                    match http_client.get(&url).send() {
                        Ok(resp) => {
                            if resp.status().is_success() {
                                if let Ok(body) = resp.text() {
                                    if let Some(slot_info) = Self::parse_slots_response(&body) {
                                        // Send update via channel to main thread.
                                        let _ = slot_sender.send(SlotUpdate {
                                            model_id: model_id.clone(),
                                            tokens_used: slot_info.tokens_used,
                                            n_ctx: slot_info.n_ctx,
                                        });

                                        // Auto-restart check on the background thread.
                                        if auto_restart_enabled
                                            && slot_info.n_ctx > 0
                                            && (slot_info.tokens_used as f64 / slot_info.n_ctx as f64) >= 0.98
                                        {
                                            Self::trigger_auto_restart(
                                                &pm,
                                                &sender,
                                                model_id,
                                            );
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            tracing::debug!(
                                "slots poll failed for model '{}' on port {}: {}",
                                model_id, port, e
                            );
                        }
                    }
                }

                last_poll_attempt = std::time::Instant::now();
            }

            tracing::info!("context poller thread exited");
        });
    }

    /// Parse the /slots JSON response to extract context information.
    ///
    /// Handles two llama.cpp response formats:
    /// - Top-level Array: `[{"id": 0, "n_ctx": 32000, "n_past": 100, ...}]`
    /// - Top-level Object: `{"n_ctx": 32000, "slots": [...]}`
    fn parse_slots_response(body: &str) -> Option<SlotInfo> {
        let json: serde_json::Value = serde_json::from_str(body).ok()?;

        // Case A: Top-level Array (llama-server default).
        if let Some(arr) = json.as_array() {
            if let Some(first_slot) = arr.first() {
                let n_ctx = first_slot.get("n_ctx").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let mut tokens_used: u64 = 0;
                for slot in arr {
                    let prompt = slot.get("n_prompt_tokens")
                        .or_else(|| slot.get("prompt_tokens_total"))
                        .or_else(|| slot.get("n_past"))
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);

                    let gen = slot.get("next_token")
                        .and_then(|v| v.as_array())
                        .and_then(|arr| arr.first())
                        .and_then(|tok| tok.get("n_decoded"))
                        .or_else(|| slot.get("generation_tokens_total"))
                        .or_else(|| slot.get("n_decoded"))
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);

                    tokens_used += prompt + gen;
                }
                if n_ctx > 0 {
                    return Some(SlotInfo {
                        tokens_used: tokens_used as usize,
                        n_ctx,
                    });
                }
            }
        }

        // Case B: Top-level Object ({"n_ctx": 32000, "slots": [...]}).
        let n_ctx = json.get("n_ctx").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let mut tokens_used: u64 = 0;
        if let Some(slots) = json.get("slots").and_then(|v| v.as_array()) {
            for slot in slots {
                let prompt = slot.get("n_prompt_tokens")
                    .or_else(|| slot.get("prompt_tokens_total"))
                    .or_else(|| slot.get("n_past"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);

                let gen = slot.get("next_token")
                    .and_then(|v| v.as_array())
                    .and_then(|arr| arr.first())
                    .and_then(|tok| tok.get("n_decoded"))
                    .or_else(|| slot.get("generation_tokens_total"))
                    .or_else(|| slot.get("n_decoded"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);

                tokens_used += prompt + gen;
            }
        }

        if n_ctx > 0 {
            Some(SlotInfo {
                tokens_used: tokens_used as usize,
                n_ctx,
            })
        } else {
            None
        }
    }

    /// Trigger an auto-restart when context is full (>=98% of n_ctx).
    fn trigger_auto_restart(
        pm: &Arc<Mutex<ProcessManager>>,
        sender: &std::sync::mpsc::Sender<ChannelMessage>,
        model_id: &str,
    ) {
        let bg_model_id = model_id.to_string();
        let bg_pm = Arc::clone(pm);
        let bg_sender = sender.clone();

        std::thread::spawn(move || {
            // Send RestartRequested first so the main loop clears other cards.
            let _ = bg_sender.send(ChannelMessage::RestartRequested {
                model_id: bg_model_id.clone(),
            });

            // Stop the current model, then start this one.
            let result = {
                let mut pm_lock = match bg_pm.lock() {
                    Ok(g) => g,
                    Err(_) => return,
                };

                if pm_lock.get_running_model_id() == Some(bg_model_id.as_str()) {
                    let _ = pm_lock.stop_model(&bg_model_id, false);
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }

                pm_lock.start_model(&bg_model_id)
            };

            // Send SwitchCompleted back to the main thread.
            let _ = bg_sender.send(ChannelMessage::SwitchCompleted {
                target_id: bg_model_id,
                result,
            });
        });
    }
}
