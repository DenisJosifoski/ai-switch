//! Main application window for ai-switch.
//!
//! Contains the menu bar, scrollable model card list, and status bar.
//! Wires up menu actions (Quit, Refresh, About, GitHub) and handles
//! async model start/stop operations on background threads.

use gtk4 as gtk;
use gtk::prelude::*;
use gtk::{
    Application, ApplicationWindow, Box as GtkBox, Label,
    Orientation, ScrolledWindow,
};
use std::rc::Rc;
use std::cell::RefCell;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::menu;
use crate::model_card::{CardState, ModelCard};
use ai_switch_core::process_manager::{ProcessError, PortState, ProcessManager, Pid};

/// Messages sent from background threads to the main GUI thread.
enum ChannelMessage {
    SwitchCompleted { target_id: String, result: Result<(), ProcessError> },
    StopCompleted { running_id: String, result: Result<(), ProcessError> },
}

/// The main application window.
pub struct MainWindow {
    widget: ApplicationWindow,
    cards: Rc<RefCell<Vec<ModelCard>>>,
    /// Tracks the keep-alive signal for the active background thread.
    current_keep_alive: Rc<RefCell<Option<Arc<AtomicBool>>>>,
}

impl MainWindow {
    pub fn new(app: &Application, config: ai_switch_core::config::Config) -> Self {
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

        // Shared process manager (Arc<Mutex> for cross-thread access).
        let pm = Arc::new(Mutex::new(
            ai_switch_core::process_manager::ProcessManager::new(config.clone()),
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
        if let Some(restored_id) = restored_model_id {
            for c in cards.borrow_mut().iter_mut() {
                if c.config().id == restored_id {
                    c.set_state(CardState::Ready);
                }
            }
        }

        // Create standard thread-safe channel
        let (sender, receiver) = std::sync::mpsc::channel::<ChannelMessage>();

        // Poll channel messages on the main context loop using a timeout
        let cards_clone = Rc::clone(&cards);
        glib::timeout_add_local(Duration::from_millis(50), move || {
            let cards_borrow = cards_clone.borrow();
            while let Ok(msg) = receiver.try_recv() {
                match msg {
                    ChannelMessage::SwitchCompleted { target_id, result } => {
                        for c in cards_borrow.iter() {
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
                        }
                    }
                    ChannelMessage::StopCompleted { running_id, result } => {
                        // Only update the card that was actually stopped.
                        // Do NOT touch other cards — their state is managed by
                        // their own toggle events or by SwitchCompleted.
                        for c in cards_borrow.iter() {
                            if c.config().id == running_id {
                                match &result {
                                    Ok(()) => c.set_state(CardState::Stopped),
                                    Err(e) => c.set_state(CardState::Error(format!(
                                        "Failed to stop: {}", e
                                    ))),
                                }
                                c.enable_toggle();
                            }
                        }
                    }
                }
            }
            glib::ControlFlow::Continue
        });

        // Wire toggle handlers — scoped to drop cards_borrow before returning Self.
        {
            let pm_clone = Arc::clone(&pm);
            let keep_alive_ref = Rc::clone(&current_keep_alive);
            let mut cards_borrow = cards.borrow_mut();
            for card in cards_borrow.iter_mut() {
                let model_id = card.config().id.clone();
                let pm_ref = Arc::clone(&pm_clone);
                let sender_ref = sender.clone();
                let cards_ref = Rc::clone(&cards);
                let ka_ref = Rc::clone(&keep_alive_ref);

                card.set_toggle_handler(move |on| {
                    let cards_inner = cards_ref.borrow();
                    if on {
                        // Turn ON
                        let target_card = match cards_inner.iter().find(|c| c.config().id == model_id) {
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

                        // Signal previous thread to exit.
                        if let Some(ref old_ka) = *ka_ref.borrow() {
                            old_ka.store(false, Ordering::SeqCst);
                        }

                        // Create new keep-alive guard.
                        let new_ka = Arc::new(AtomicBool::new(true));
                        *ka_ref.borrow_mut() = Some(Arc::clone(&new_ka));

                        let bg_model_id = model_id.clone();
                        let pm_thread = Arc::clone(&pm_ref);
                        let sender_thread = sender_ref.clone();
                        let ka_thread = new_ka;

                        std::thread::spawn(move || {
                            // Run the switch_model / start_model and release lock immediately.
                            let result = {
                                let mut pm_lock = match pm_thread.lock() {
                                    Ok(g) => g,
                                    Err(_) => return,
                                };
                                // Defense in depth: skip if target model is already running
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

                            // If spawn succeeded, block this thread to keep parent PID alive.
                            if is_ok {
                                while ka_thread.load(Ordering::SeqCst) {
                                    std::thread::sleep(Duration::from_millis(100));
                                }
                            }
                        });
                    } else {
                        // Turn OFF
                        if let Some(ref old_ka) = *ka_ref.borrow() {
                            old_ka.store(false, Ordering::SeqCst);
                        }
                        *ka_ref.borrow_mut() = None;

                        let pm_thread = Arc::clone(&pm_ref);
                        let sender_thread = sender_ref.clone();

                        std::thread::spawn(move || {
                            let mut pm_lock = match pm_thread.lock() {
                                Ok(g) => g,
                                Err(_) => return,
                            };
                            if let Some(running_id) = pm_lock.get_running_model_id().map(String::from) {
                                let result = pm_lock.stop_model(&running_id);
                                let _ = sender_thread.send(ChannelMessage::StopCompleted {
                                    running_id,
                                    result,
                                });
                            }
                        });
                    }
                });
            }
        }

        Self { widget, cards, current_keep_alive }
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
}
