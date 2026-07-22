//! Native menu bar for ai-switch.
//!
//! Builds the File / Edit / View / Help menu structure using `gio::Menu`
//! and returns a `gtk4::PopoverMenuBar` — a permanently visible native menu bar.

use gtk4 as gtk;
use gio::Menu;
use gtk::PopoverMenuBar;

/// Build the application menu bar and return a `PopoverMenuBar`.
pub fn build_menu_bar() -> PopoverMenuBar {
    let menu = Menu::new();

    // ── File menu ────────────────────────────────────────────────
    let file_menu = build_file_section();
    menu.append_submenu(Some("File"), &file_menu);

    // ── Edit menu ────────────────────────────────────────────────
    let edit_menu = build_edit_section();
    menu.append_submenu(Some("Edit"), &edit_menu);

    // ── View menu ────────────────────────────────────────────────
    let view_menu = build_view_section();
    menu.append_submenu(Some("View"), &view_menu);

    // ── Help menu ────────────────────────────────────────────────
    let help_menu = build_help_section();
    menu.append_submenu(Some("Help"), &help_menu);

    PopoverMenuBar::from_model(Some(&menu))
}

/// Build the File section: Add Model (stub), Quit.
fn build_file_section() -> Menu {
    let menu = Menu::new();
    menu.append(Some("Add Model"), None);
    menu.append(Some("Quit"), Some("win.quit"));
    menu
}

/// Build the Edit section: Preferences (stub).
fn build_edit_section() -> Menu {
    let menu = Menu::new();
    menu.append(Some("Preferences"), None);
    menu
}

/// Build the View section: Refresh, Toggle Logs Panel (stub).
fn build_view_section() -> Menu {
    let menu = Menu::new();
    menu.append(Some("Refresh"), Some("win.refresh"));
    menu.append(Some("Toggle Logs Panel"), None);
    menu
}

/// Build the Help section: About, Open GitHub Repo.
fn build_help_section() -> Menu {
    let menu = Menu::new();
    menu.append(Some("About"), Some("win.about"));
    menu.append(Some("Open GitHub Repo"), Some("win.github"));
    menu
}
