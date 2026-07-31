// On Windows, use the "windows" subsystem so no console window is created.
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

//! Main entry point for the GitHub Tray Icon application.
//! Handles cross-platform initialization and tray icon setup.

mod access_token;
mod github;
mod icons;
mod log;
mod scheduler;
mod state;

const NOTIFICATIONS_URL: &str = "https://github.com/notifications";

/// Returns the path to the application's asset directory in the user's home.
/// Creates the directory if it does not exist.
fn get_app_asset_path() -> Result<std::path::PathBuf, String> {
    let user_home = dirs::home_dir().ok_or("could not find home directory")?;
    let assets_path = user_home.join(".github-trayicon");
    std::fs::create_dir_all(&assets_path)
        .map_err(|e| format!("failed to create {}: {e}", assets_path.display()))?;
    Ok(assets_path)
}

// ─── Linux ────────────────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
fn main() {
    use gtk::prelude::*;
    use gtk::{Menu, MenuItem};
    use libappindicator::{AppIndicator, AppIndicatorStatus};

    gtk::init().expect("Failed to initialize GTK.");

    let app_asset_path = match get_app_asset_path() {
        Ok(path) => path,
        Err(e) => {
            eprintln!("Fatal: {e}");
            std::process::exit(1);
        }
    };
    log::init(&app_asset_path);

    let (icon_path, icon_with_notification_path) = icons::create_icons(&app_asset_path);

    let tokens = match access_token::TokenStore::load(&app_asset_path) {
        Ok(tokens) => tokens,
        Err(e) => {
            logln!("fatal: {e}");
            std::process::exit(1);
        }
    };

    let mut indicator = AppIndicator::new("github_notifications", "");
    indicator.set_status(AppIndicatorStatus::Active);
    indicator.set_icon(icon_path.as_str());

    // The poll loop waits on this channel, so a menu click can pull the next poll forward.
    let (wake_tx, wake_rx) = std::sync::mpsc::channel::<scheduler::Wake>();

    let mut menu = Menu::new();
    let item = MenuItem::with_label("Open GitHub Notifications");
    item.connect_activate(move |_| {
        if let Err(e) = open::that(NOTIFICATIONS_URL) {
            logln!("failed to open browser: {e}");
        }
        // Whatever the user is about to read changes the answer, so re-poll soon rather than
        // leaving a stale "unread" icon up for a whole interval.
        let _ = wake_tx.send(scheduler::Wake::Refresh);
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
        tokens,
        wake_rx,
    );

    gtk::main();
}

// ─── Windows ──────────────────────────────────────────────────────────────────

/// Reports a startup failure and exits.
///
/// With `windows_subsystem = "windows"` there is no console, so a bare `expect` would make the
/// app vanish without a word — indistinguishable, from the user's side, from a tray icon that is
/// simply wrong.
#[cfg(target_os = "windows")]
fn fatal(message: &str) -> ! {
    logln!("fatal: {message}");
    access_token::win_msgbox("git-system-tray", message);
    std::process::exit(1);
}

#[cfg(target_os = "windows")]
fn main() {
    use scheduler::{TrayEvent, Update};
    use state::IconState;
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
            access_token::win_msgbox("Already Running", "git-system-tray is already running.");
            return;
        }
        // On a fresh mutex (first instance) GetLastError() is 0 — fall through.
    }

    let app_asset_path = match get_app_asset_path() {
        Ok(path) => path,
        Err(e) => {
            access_token::win_msgbox("git-system-tray", &format!("Fatal: {e}"));
            std::process::exit(1);
        }
    };
    log::init(&app_asset_path);

    let tokens = match access_token::TokenStore::load(&app_asset_path) {
        Ok(tokens) => tokens,
        Err(e) => fatal(&format!("Could not authenticate with GitHub: {e}")),
    };

    // Decode the embedded PNG assets into Icon objects on the main thread.
    let (normal_icon, notification_icon) = match icons::load_tray_icons() {
        Ok(icons) => icons,
        Err(e) => fatal(&e),
    };

    // Build the tray menu.
    let open_item = MenuItem::new("Open GitHub Notifications", true, None);
    let open_item_id = open_item.id().clone();
    let quit_item = MenuItem::new("Quit", true, None);
    let quit_item_id = quit_item.id().clone();
    let menu = Menu::new();
    if let Err(e) = menu.append(&open_item) {
        fatal(&format!("Failed to append menu item: {e}"));
    }
    if let Err(e) = menu.append(&quit_item) {
        fatal(&format!("Failed to append quit item: {e}"));
    }

    // Build the tray icon (must be created on the main thread on Windows).
    let tray_icon = match TrayIconBuilder::new()
        .with_tooltip("GitHub Notifications")
        .with_icon(normal_icon.clone())
        .with_menu(Box::new(menu))
        .build()
    {
        Ok(tray_icon) => tray_icon,
        Err(e) => fatal(&format!("Failed to create tray icon: {e}")),
    };

    // Create the winit event loop with a custom event type so the background
    // thread can wake the loop and deliver notification updates.
    let event_loop: EventLoop<TrayEvent> = match EventLoop::with_user_event().build() {
        Ok(event_loop) => event_loop,
        Err(e) => fatal(&format!("Failed to create event loop: {e}")),
    };
    let proxy = event_loop.create_proxy();

    // The poll loop waits on this channel, so a menu click can pull the next poll forward.
    let (wake_tx, wake_rx) = std::sync::mpsc::channel::<scheduler::Wake>();

    // Launch the polling thread; it communicates back via the proxy.
    scheduler::start_notification_scheduler(tokens, wake_rx, proxy);

    // ── Application handler ──────────────────────────────────────────────────

    struct App {
        tray_icon: tray_icon::TrayIcon,
        normal_icon: tray_icon::Icon,
        notification_icon: tray_icon::Icon,
        open_item_id: tray_icon::menu::MenuId,
        quit_item_id: tray_icon::menu::MenuId,
        wake_tx: std::sync::mpsc::Sender<scheduler::Wake>,
        /// Which image the tray is actually showing, as far as we know. `None` means "unproven",
        /// which forces the next update to re-apply rather than assume.
        applied_notification_icon: Option<bool>,
    }

    impl App {
        fn apply(&mut self, update: Update) {
            // `Unknown` deliberately leaves the picture alone — a brief failure should change
            // the words, not make the icon flap. Only a confirmed answer moves the image.
            let wanted = match update.icon {
                IconState::Unread => Some(true),
                IconState::Clear => Some(false),
                IconState::Unknown => None,
            };

            if let Some(want_notification) = wanted
                && self.applied_notification_icon != Some(want_notification)
            {
                let icon = if want_notification {
                    &self.notification_icon
                } else {
                    &self.normal_icon
                };
                match self.tray_icon.set_icon(Some(icon.clone())) {
                    // Only record success. A failed update leaves this `None` so the next
                    // poll retries instead of believing the icon is already correct.
                    Ok(()) => self.applied_notification_icon = Some(want_notification),
                    Err(e) => logln!("failed to update tray icon: {e}"),
                }
            }

            if let Err(e) = self.tray_icon.set_tooltip(Some(&update.tooltip)) {
                logln!("failed to update tray tooltip: {e}");
            }
        }
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
            let TrayEvent::Update(update) = event;
            self.apply(update);
        }

        fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
            // Keep the loop sleeping until the next event arrives so we don't
            // burn CPU.
            event_loop.set_control_flow(ControlFlow::Wait);

            // Drain the whole queue. `if let` handled one event per wakeup and then slept on
            // `ControlFlow::Wait`, so a second queued click sat unhandled until something else
            // happened to wake the loop.
            while let Ok(event) = MenuEvent::receiver().try_recv() {
                if event.id == self.open_item_id {
                    if let Err(e) = open::that(NOTIFICATIONS_URL) {
                        logln!("failed to open browser: {e}");
                    }
                    // Whatever the user is about to read changes the answer, so re-poll soon
                    // rather than leaving a stale "unread" icon up for a whole interval.
                    let _ = self.wake_tx.send(scheduler::Wake::Refresh);
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
        wake_tx,
        // The builder set the normal icon above, but treat that as unproven so the first
        // confirmed poll always writes the image it wants.
        applied_notification_icon: None,
    };

    if let Err(e) = event_loop.run_app(&mut app) {
        fatal(&format!("Event loop failed: {e}"));
    }
}
