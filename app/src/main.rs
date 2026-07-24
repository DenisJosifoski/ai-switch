//! ai-switch — GTK4 native model switcher.
//!
//! Entry point: initialize GTK, load config (showing errors as dialogs on
//! failure), start the reverse proxy server, then hand control to the main
//! window.

use std::sync::Arc;

use gtk4 as gtk;
use gtk::prelude::*;

mod logs_panel;
mod menu;
mod model_card;
mod preferences;
mod window;

fn main() -> glib::ExitCode {
    // Initialize tracing so core library logs are visible during development.
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .init();

    // Create the GTK application (single instance at the GTK level).
    let app = gtk::Application::builder()
        .application_id("com.ai-switch.app")
        .build();

    // Connect the activate signal — this is called when the application
    // is "activated" (launched, or clicked from a launcher).
    app.connect_activate(|app| {
        // Try to load the config — this is where validation errors (duplicate
        // ports, missing scripts, empty model list) surface.
        let config = match ai_switch_core::config::Config::load() {
            Ok(cfg) => cfg,
            Err(e) => {
                // Show the error as a modal dialog so the user can see it even
                // without a terminal attached.
                let dialog = gtk::MessageDialog::new(
                    None::<&gtk::Window>,
                    gtk::DialogFlags::MODAL,
                    gtk::MessageType::Error,
                    gtk::ButtonsType::Close,
                    &format!("Failed to load config:\n\n{}", e),
                );
                dialog.set_title(Some("ai-switch — Config Error"));

                let app_clone = app.clone();
                dialog.connect_response(move |dialog, _| {
                    dialog.destroy();
                    app_clone.quit();
                });
                dialog.present();
                return;
            }
        };

        // Create and start the reverse proxy server.
        // The proxy forwards all incoming requests on proxy_port to whichever
        // model is currently active, so IDE/CLI clients never need reconfiguration.
        let proxy_state = ai_switch_core::proxy::ProxyState::new();
        let proxy_state = Arc::new(std::sync::Mutex::new(proxy_state));

        let proxy_server = match ai_switch_core::proxy::ProxyServer::new(
            config.proxy_port(),
            Arc::clone(&proxy_state),
        ) {
            Ok(server) => Some(server),
            Err(e) => {
                tracing::warn!("failed to start reverse proxy: {}", e);
                // Don't fail the app — the proxy is a convenience feature.
                None
            }
        };

        // Create and show the main window.
        let main_window = window::MainWindow::new(app, config, Some(proxy_state));
        main_window.show();

        // Keep everything alive for the application lifetime using Rc so we can
        // drop them in the shutdown callback without moving.
        use std::rc::Rc;
        let main_window_rc = Rc::new(main_window);
        let proxy_server_rc = Rc::new(proxy_server);

        app.connect_shutdown(move |_| {
            drop(Rc::clone(&main_window_rc));
            if let Some(ref _server) = *proxy_server_rc {
                // Clone the Rc to keep a reference alive, then drop it.
                let _server_rc = Rc::clone(&proxy_server_rc);
                drop(_server_rc);
            }
        });
    });

    // Run the application — this calls `activate` on the application object.
    app.run()
}
