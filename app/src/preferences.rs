//! Preferences dialog — Edit → Preferences.
//!
//! Allows editing of global settings that persist back to `config.toml`:
//! - Log directory (with file-chooser button)
//! - Proxy port
//! - Auto-restart on context full toggle
//!
//! The dialog is shown non-blockingly via `connect_response` in the caller
//! (window.rs), so the GTK main loop is never blocked.

use gtk4 as gtk;
use gtk::prelude::*;
use gtk::{
    Box as GtkBox, Button, Entry, FileChooserAction, Label,
    Orientation, ResponseType, Switch, Window,
};
use std::path::PathBuf;

use ai_switch_core::config::Config;

// Note: PreferencesDialog is non-blocking. The caller (window.rs) uses
// connect_response to handle Ok/Cancel without blocking the main thread.

/// A modal dialog for editing global configuration.
#[derive(Clone)]
pub struct PreferencesDialog {
    pub widget: gtk::Dialog,
    log_dir_entry: Entry,
    proxy_port_entry: Entry,
    auto_restart_switch: Switch,
}

/// The values from the preferences form.
pub struct PreferencesValues {
    pub log_dir: Option<PathBuf>,
    pub proxy_port: Option<u16>,
    pub auto_restart_on_context_full: bool,
}

impl PreferencesDialog {
    /// Extract the current form values as a serializable struct.
    pub fn values(&self) -> PreferencesValues {
        let log_dir_text = self.log_dir_entry.text().to_string();
        let log_dir = if log_dir_text.is_empty() {
            None
        } else {
            Some(PathBuf::from(&log_dir_text))
        };

        let proxy_port_text = self.proxy_port_entry.text().to_string();
        let proxy_port: Option<u16> = proxy_port_text.parse().ok();

        let auto_restart = self.auto_restart_switch.is_active();

        PreferencesValues {
            log_dir,
            proxy_port,
            auto_restart_on_context_full: auto_restart,
        }
    }

    /// Create a new preferences dialog transient to the given parent window.
    pub fn new<T: IsA<Window>>(parent: &T, config: &Config) -> Self {
        let widget = gtk::Dialog::builder()
            .title("Preferences")
            .transient_for(parent)
            .modal(true)
            .build();

        let content_box = GtkBox::new(Orientation::Vertical, 12);
        content_box.set_margin_start(24);
        content_box.set_margin_end(24);
        content_box.set_margin_top(24);
        content_box.set_margin_bottom(24);

        let log_dir_entry = Self::add_log_dir_row(&content_box, parent, config);
        content_box.append(&log_dir_entry);

        let proxy_port_entry = Self::add_proxy_port_row(&content_box, config);
        content_box.append(&proxy_port_entry);

        let auto_restart_switch = Self::add_auto_restart_row(&content_box, config);
        content_box.append(&auto_restart_switch);

        widget.content_area().append(&content_box);

        widget.add_button("_Cancel", ResponseType::Cancel);
        widget.add_button("_Save", ResponseType::Ok);

        Self {
            widget,
            log_dir_entry,
            proxy_port_entry,
            auto_restart_switch,
        }
    }

    /// Destroy the dialog window.
    pub fn destroy(&self) {
        self.widget.destroy();
    }

    /// Read current values and save back to the config file.
    pub fn save(&self, config_path: &std::path::Path) -> Result<(), String> {
        let log_dir_text = self.log_dir_entry.text().to_string();
        let log_dir = if log_dir_text.is_empty() {
            None
        } else {
            Some(PathBuf::from(&log_dir_text))
        };

        let proxy_port_text = self.proxy_port_entry.text().to_string();
        let proxy_port: Option<u16> = proxy_port_text.parse().ok();

        let auto_restart = self.auto_restart_switch.is_active();

        let mut config = Config::load().map_err(|e| format!("Failed to load config: {}", e))?;
        config.global.log_dir = log_dir;
        config.global.proxy_port = proxy_port;
        config.global.auto_restart_on_context_full = Some(auto_restart);

        Config::validate(&config, config_path).map_err(|e| format!("Config validation error: {}", e))?;

        let content = toml::to_string_pretty(&config)
            .map_err(|e| format!("Failed to serialize config: {}", e))?;
        std::fs::write(config_path, &content)
            .map_err(|e| format!("Failed to write config file: {}", e))?;

        Ok(())
    }

    fn add_log_dir_row<T: IsA<Window>>(_parent_widget: &GtkBox, dialog_parent: &T, config: &Config) -> Entry {
        let row = GtkBox::new(Orientation::Horizontal, 8);
        row.set_hexpand(true);

        let label = Label::new(Some("Log directory"));
        label.set_halign(gtk::Align::Start);
        label.set_hexpand(true);
        row.append(&label);

        let current_path = config.log_dir();
        let entry = Entry::new();
        entry.set_text(current_path.to_string_lossy().as_ref());
        entry.set_hexpand(false);
        entry.set_width_chars(24);
        row.append(&entry);

        let entry_clone = entry.clone();
        let dialog_parent_clone = dialog_parent.clone();

        let browse_btn = Button::with_label("Browse…");
        browse_btn.set_css_classes(&["flat"]);
        browse_btn.connect_clicked(move |_| {
            Self::show_folder_chooser(&entry_clone, &dialog_parent_clone);
        });
        row.append(&browse_btn);

        entry
    }

    /// Show a folder chooser dialog using the async run_async pattern.
    fn show_folder_chooser<T: IsA<Window>>(entry: &Entry, parent: &T) {
        let chooser = gtk::FileChooserDialog::new(
            Some("Select Log Directory"),
            Some(parent),
            FileChooserAction::SelectFolder,
            &[("_Cancel", ResponseType::Cancel), ("_Select", ResponseType::Ok)],
        );

        if let Ok(path) = std::env::var("HOME") {
            let _ = chooser.set_current_folder(Some(&gio::File::for_path(PathBuf::from(path))));
        }

        let entry_clone = entry.clone();
        chooser.run_async(move |chooser, response| {
            if response == ResponseType::Ok {
                if let Some(folder) = chooser.current_folder() {
                    if let Some(path) = folder.path() {
                        entry_clone.set_text(&path.to_string_lossy());
                    }
                }
            }
            chooser.destroy();
        });
    }

    fn add_proxy_port_row(_parent_widget: &GtkBox, config: &Config) -> Entry {
        let row = GtkBox::new(Orientation::Horizontal, 8);
        row.set_hexpand(true);

        let label = Label::new(Some("Proxy port"));
        label.set_halign(gtk::Align::Start);
        label.set_hexpand(true);
        row.append(&label);

        let proxy_port = config.proxy_port();
        let entry = Entry::new();
        entry.set_text(&proxy_port.to_string());
        entry.set_width_chars(6);
        entry.set_hexpand(false);
        row.append(&entry);

        entry
    }

    fn add_auto_restart_row(_parent_widget: &GtkBox, config: &Config) -> Switch {
        let row = GtkBox::new(Orientation::Horizontal, 8);
        row.set_hexpand(true);

        let label = Label::new(Some("Auto-restart on context full"));
        label.set_halign(gtk::Align::Start);
        label.set_hexpand(true);
        row.append(&label);

        let auto_restart = config.auto_restart_on_context_full();
        let switch = Switch::new();
        switch.set_active(auto_restart);
        switch.set_halign(gtk::Align::End);
        row.append(&switch);

        switch
    }
}
