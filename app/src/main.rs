//! ai-switch — GTK4 native model switcher.
//!
//! Entry point: initialize GTK, load config (showing errors as dialogs on
//! failure), then hand control to the main window.

use gtk4 as gtk;
use gtk::prelude::*;

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

        // Create and show the main window.
        let main_window = window::MainWindow::new(app, config);
        main_window.show();

        // Keep the main window wrapper and its process manager alive for the application lifetime
        app.connect_shutdown(move |_| {
            let _ = &main_window;
        });
    });

    // Run the application — this calls `activate` on the application object.
    app.run()
}
