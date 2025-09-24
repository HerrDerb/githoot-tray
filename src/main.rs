//! Main entry point for the GitHub Tray Icon application.
//! Handles cross-platform initialization and tray icon setup.

mod access_token;
mod github;
mod icons;
mod scheduler;

use crate::scheduler::start_notification_scheduler;

/// Returns the path to the application's asset directory in the user's home.
/// Creates the directory if it does not exist.
fn get_app_asset_path() -> std::path::PathBuf {
    let user_home = dirs::home_dir().expect("Could not find home directory");
    let assets_path = user_home.join(".github-trayicon");
    std::fs::create_dir_all(&assets_path).expect("Failed to create assets directory");
    assets_path
}

#[cfg(target_os = "linux")]
#[tokio::main]
async fn main() {
    use gtk::prelude::*;
    use gtk::{Menu, MenuItem};
    use libappindicator::{AppIndicator, AppIndicatorStatus};

    gtk::init().expect("Failed to initialize GTK.");
    let app_asset_path = get_app_asset_path();
    let (icon_path, icon_with_notification_path) = icons::create_icons(&app_asset_path);
    let access_token = access_token::get_access_token(&app_asset_path);

    let mut indicator = AppIndicator::new("github_notifications", "");
    indicator.set_status(AppIndicatorStatus::Active);
    indicator.set_icon(icon_path.as_str());

    let mut menu = Menu::new();
    let item = MenuItem::with_label("Open GitHub Notifications");
    item.connect_activate(|_| {
        if let Err(e) = open::that("https://github.com/notifications") {
            eprintln!("Failed to open browser: {e}");
        }
    });
    menu.append(&item);
    menu.show_all();
    indicator.set_menu(&mut menu);

    start_notification_scheduler(
        indicator,
        icon_path.to_string(),
        icon_with_notification_path.to_string(),
        access_token,
    );
    gtk::main();
}

#[cfg(target_os = "windows")]
#[tokio::main]
async fn main() {
    use std::sync::mpsc;
    use std::thread;
    use tray_icon::{TrayIconBuilder, TrayIconEvent, menu::Menu, menu::MenuItem};
    use winit::event::Event as WinitEvent;
    use winit::event_loop::{ControlFlow, EventLoop};

    // Set up asset path and icons
    let app_asset_path = get_app_asset_path();
    let (icon_path, icon_with_notification_path) = icons::create_icons(&app_asset_path);
    let access_token = access_token::get_access_token(&app_asset_path).await;

    // Build tray icon and menu
    let mut menu = Menu::new();
    let open_github_item = MenuItem::new("Open GitHub Notifications", true, None);
    menu.append(&open_github_item);

    let tray_icon = TrayIconBuilder::new()
        .with_tooltip("GitHub Notifications")
        .with_icon(icon_path.clone())
        .with_menu(menu)
        .build()
        .expect("Failed to create tray icon");

    // Channel for tray icon events
    let (tx, rx) = mpsc::channel();
    TrayIconEvent::set_event_handler(Some(move |event| {
        let _ = tx.send(event.clone());
    }));

    // Spawn a thread to handle menu item clicks
    let open_github_item_clone = open_github_item.clone();
    thread::spawn(move || {
        loop {
            if open_github_item_clone.is_clicked() {
                if let Err(e) = open::that("https://github.com/notifications") {
                    eprintln!("Failed to open browser: {e}");
                }
            }
            thread::sleep(std::time::Duration::from_millis(200));
        }
    });

    // Start notification scheduler (dummy indicator for Windows, just pass tray_icon)
    scheduler::start_notification_scheduler(
        tray_icon,
        icon_path,
        icon_with_notification_path,
        access_token,
    )
    .await;

    // Event loop to keep the application running
    let event_loop = EventLoop::new();
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            WinitEvent::LoopDestroyed => return,
            _ => {}
        }
        // Handle tray icon events if needed (future extension)
        while let Ok(_event) = rx.try_recv() {
            // Handle tray icon events here if needed
        }
    });
}
