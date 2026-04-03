// On Windows, use the "windows" subsystem so no console window is created.
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

//! Main entry point for the GitHub Tray Icon application.
//! Handles cross-platform initialization and tray icon setup.

mod access_token;
mod github;
mod icons;
mod scheduler;

/// Returns the path to the application's asset directory in the user's home.
/// Creates the directory if it does not exist.
fn get_app_asset_path() -> std::path::PathBuf {
    let user_home = dirs::home_dir().expect("Could not find home directory");
    let assets_path = user_home.join(".github-trayicon");
    std::fs::create_dir_all(&assets_path).expect("Failed to create assets directory");
    assets_path
}

// ─── Linux ────────────────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
fn main() {
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
    let quit_item = MenuItem::with_label("Quit");
    quit_item.connect_activate(|_| gtk::main_quit());
    menu.append(&item);
    menu.append(&quit_item);
    menu.show_all();
    indicator.set_menu(&mut menu);

    scheduler::start_notification_scheduler(
        indicator,
        icon_path,
        icon_with_notification_path,
        access_token,
    );

    gtk::main();
}

// ─── Windows ──────────────────────────────────────────────────────────────────

#[cfg(target_os = "windows")]
fn main() {
    use scheduler::TrayEvent;
    use tray_icon::menu::{Menu, MenuEvent, MenuItem};
    use tray_icon::TrayIconBuilder;
    use winit::application::ApplicationHandler;
    use winit::event::WindowEvent;
    use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
    use winit::window::WindowId;

    // ── Single-instance guard ────────────────────────────────────────────────
    // CreateMutexW returns the existing handle if the named mutex already exists,
    // and GetLastError() reports ERROR_ALREADY_EXISTS. We intentionally never
    // call CloseHandle so the mutex lives until the process exits.
    // SetLastError(0) clears any stale error left by DLL/runtime init so that
    // the GetLastError check is always based on CreateMutexW's own result.
    unsafe {
        use std::ptr::null_mut;
        use winapi::um::errhandlingapi::{GetLastError, SetLastError};
        use winapi::um::synchapi::CreateMutexW;

        let name: Vec<u16> = "Local\\GitSystemTray\0".encode_utf16().collect();
        SetLastError(0);
        let handle = CreateMutexW(null_mut(), 0, name.as_ptr());

        if handle.is_null() {
            eprintln!("Warning: could not create single-instance mutex (err {})", GetLastError());
        } else if GetLastError() == 0xB7 {
            // ERROR_ALREADY_EXISTS — another instance owns the mutex
            access_token::win_msgbox(
                "Already Running",
                "git-system-tray is already running.",
            );
            return;
        }
        // On a fresh mutex (first instance) GetLastError() is 0 — fall through.
    }

    let app_asset_path = get_app_asset_path();
    let access_token = access_token::get_access_token(&app_asset_path);

    // Decode the embedded PNG assets into Icon objects on the main thread.
    let (normal_icon, notification_icon) = icons::load_tray_icons();

    // Build the tray menu.
    let open_item = MenuItem::new("Open GitHub Notifications", true, None);
    let open_item_id = open_item.id().clone();
    let quit_item = MenuItem::new("Quit", true, None);
    let quit_item_id = quit_item.id().clone();
    let menu = Menu::new();
    menu.append(&open_item).expect("Failed to append menu item");
    menu.append(&quit_item).expect("Failed to append quit item");

    // Build the tray icon (must be created on the main thread on Windows).
    let tray_icon = TrayIconBuilder::new()
        .with_tooltip("GitHub Notifications")
        .with_icon(normal_icon.clone())
        .with_menu(Box::new(menu))
        .build()
        .expect("Failed to create tray icon");

    // Create the winit event loop with a custom event type so the background
    // thread can wake the loop and deliver notification updates.
    let event_loop: EventLoop<TrayEvent> = EventLoop::with_user_event()
        .build()
        .expect("Failed to create event loop");
    let proxy = event_loop.create_proxy();

    // Launch the polling thread; it communicates back via the proxy.
    scheduler::start_notification_scheduler(access_token, proxy);

    // ── Application handler ──────────────────────────────────────────────────

    struct App {
        tray_icon: tray_icon::TrayIcon,
        normal_icon: tray_icon::Icon,
        notification_icon: tray_icon::Icon,
        open_item_id: tray_icon::menu::MenuId,
        quit_item_id: tray_icon::menu::MenuId,
    }

    impl ApplicationHandler<TrayEvent> for App {
        fn resumed(&mut self, _event_loop: &ActiveEventLoop) {}

        fn window_event(
            &mut self,
            _event_loop: &ActiveEventLoop,
            _id: WindowId,
            _event: WindowEvent,
        ) {
        }

        /// Called when the background thread delivers a notification-state update.
        fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: TrayEvent) {
            let TrayEvent::NotificationUpdate(has_notifications) = event;
            let icon = if has_notifications {
                &self.notification_icon
            } else {
                &self.normal_icon
            };
            if let Err(e) = self.tray_icon.set_icon(Some(icon.clone())) {
                eprintln!("Failed to update tray icon: {e}");
            }
        }

        fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
            // Keep the loop sleeping until the next event arrives so we don't
            // burn CPU.
            event_loop.set_control_flow(ControlFlow::Wait);

            // Handle tray menu clicks.
            if let Ok(event) = MenuEvent::receiver().try_recv() {
                if event.id == self.open_item_id {
                    if let Err(e) = open::that("https://github.com/notifications") {
                        eprintln!("Failed to open browser: {e}");
                    }
                } else if event.id == self.quit_item_id {
                    event_loop.exit();
                }
            }
        }
    }

    let mut app = App {
        tray_icon,
        normal_icon,
        notification_icon,
        open_item_id,
        quit_item_id,
    };

    event_loop.run_app(&mut app).expect("Event loop failed");
}
