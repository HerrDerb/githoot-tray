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

/// Handles the "Set up review dot…" menu item.
///
/// Creates `review_token.txt` at `0600` with the instructions inside it and opens it in the
/// user's editor, then nudges the poll loop. The loop re-reads that file every cycle, so saving
/// the token is the whole setup — no restart, no terminal, no `chmod`.
fn setup_review_dot(
    app_asset_path: &std::path::Path,
    wake_tx: &std::sync::mpsc::Sender<scheduler::Wake>,
) {
    let path = access_token::ensure_review_token_file(app_asset_path);
    logln!("review dot setup: opening {}", path.display());

    if let Err(e) = open::that(&path) {
        logln!("could not open {} automatically: {e}", path.display());
        // Nothing else to fall back on: with no console on Windows, the log is the only channel.
        #[cfg(target_os = "windows")]
        access_token::win_msgbox(
            "git-system-tray",
            &format!("Could not open an editor. Paste your token into:\n{}", path.display()),
        );
    }

    // Shorten the wait for the token they are about to paste: the burst re-checks after a few
    // seconds instead of leaving them staring at an unchanged icon for a whole interval.
    let _ = wake_tx.send(scheduler::Wake::Refresh);
}

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

    let icons = match icons::create_icons(&app_asset_path) {
        Ok(icons) => icons,
        Err(e) => {
            logln!("fatal: {e}");
            std::process::exit(1);
        }
    };

    let tokens = match access_token::TokenStore::load(&app_asset_path) {
        Ok(tokens) => tokens,
        Err(e) => {
            logln!("fatal: {e}");
            std::process::exit(1);
        }
    };

    // Optional second credential for the review dot. A failure here must not stop the app: the
    // notifications half is the primary feature and works without it.
    let reviews = match access_token::ReviewTokenStore::load(&app_asset_path) {
        Ok(reviews) => reviews,
        Err(e) => {
            logln!("review dot disabled: {e}");
            None
        }
    };

    let mut indicator = AppIndicator::new("github_notifications", "");
    indicator.set_status(AppIndicatorStatus::Active);
    indicator.set_icon(icons.get(false, false).as_str());

    // The poll loop waits on this channel, so a menu click can pull the next poll forward.
    let (wake_tx, wake_rx) = std::sync::mpsc::channel::<scheduler::Wake>();

    let mut menu = Menu::new();
    let item = MenuItem::with_label("Open GitHub Notifications");
    let open_wake_tx = wake_tx.clone();
    item.connect_activate(move |_| {
        if let Err(e) = open::that(NOTIFICATIONS_URL) {
            logln!("failed to open browser: {e}");
        }
        // Whatever the user is about to read changes the answer, so re-poll soon rather than
        // leaving a stale "unread" icon up for a whole interval.
        let _ = open_wake_tx.send(scheduler::Wake::Refresh);
    });

    let reviews_item = MenuItem::with_label("Open Requested Reviews");
    let reviews_wake_tx = wake_tx.clone();
    reviews_item.connect_activate(move |_| {
        if let Err(e) = open::that(scheduler::review_list_url()) {
            logln!("failed to open browser: {e}");
        }
        // Reviewing is what clears the dot, so pull the next poll forward the same way the
        // notifications item does.
        let _ = reviews_wake_tx.send(scheduler::Wake::Refresh);
    });

    let setup_item = MenuItem::with_label("Set up review dot…");
    let setup_path = app_asset_path.clone();
    setup_item.connect_activate(move |_| {
        setup_review_dot(&setup_path, &wake_tx);
    });

    let quit_item = MenuItem::with_label("Quit");
    quit_item.connect_activate(|_| gtk::main_quit());
    menu.append(&item);
    menu.append(&reviews_item);
    menu.append(&setup_item);
    menu.append(&quit_item);
    menu.show_all();
    indicator.set_menu(&mut menu);

    scheduler::start_notification_scheduler(
        app_asset_path,
        indicator,
        icons,
        tokens,
        reviews,
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

    // Optional second credential for the review dot. A failure here must not be fatal: the
    // notifications half is the primary feature and works without it.
    let reviews = match access_token::ReviewTokenStore::load(&app_asset_path) {
        Ok(reviews) => reviews,
        Err(e) => {
            logln!("review dot disabled: {e}");
            None
        }
    };

    // Decode and composite the embedded PNG assets on the main thread.
    let tray_icons = match icons::load_tray_icons() {
        Ok(icons) => icons,
        Err(e) => fatal(&e),
    };

    // Build the tray menu.
    let open_item = MenuItem::new("Open GitHub Notifications", true, None);
    let open_item_id = open_item.id().clone();
    let reviews_item = MenuItem::new("Open Requested Reviews", true, None);
    let reviews_item_id = reviews_item.id().clone();
    let setup_item = MenuItem::new("Set up review dot\u{2026}", true, None);
    let setup_item_id = setup_item.id().clone();
    let quit_item = MenuItem::new("Quit", true, None);
    let quit_item_id = quit_item.id().clone();
    let menu = Menu::new();
    for (item, what) in [
        (&open_item, "open"),
        (&reviews_item, "reviews"),
        (&setup_item, "setup"),
        (&quit_item, "quit"),
    ] {
        if let Err(e) = menu.append(item) {
            fatal(&format!("Failed to append {what} menu item: {e}"));
        }
    }

    // Build the tray icon (must be created on the main thread on Windows).
    let tray_icon = match TrayIconBuilder::new()
        .with_tooltip("GitHub Notifications")
        .with_icon(tray_icons.get(false, false).clone())
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
    scheduler::start_notification_scheduler(
        app_asset_path.clone(),
        tokens,
        reviews,
        wake_rx,
        proxy,
    );

    // ── Application handler ──────────────────────────────────────────────────

    struct App {
        tray_icon: tray_icon::TrayIcon,
        icons: icons::IconSet<tray_icon::Icon>,
        open_item_id: tray_icon::menu::MenuId,
        reviews_item_id: tray_icon::menu::MenuId,
        setup_item_id: tray_icon::menu::MenuId,
        quit_item_id: tray_icon::menu::MenuId,
        wake_tx: std::sync::mpsc::Sender<scheduler::Wake>,
        app_asset_path: std::path::PathBuf,
        /// Which image the tray is actually showing, as `(unread, review_pending)`, as far as we
        /// know. `None` means "unproven", which forces the next update to re-apply rather than
        /// assume.
        applied: Option<(bool, bool)>,
    }

    impl App {
        fn apply(&mut self, update: Update) {
            // `Unknown` on either axis deliberately leaves that part of the picture alone — a
            // brief failure should change the words, not make the icon flap. So an unknown axis
            // falls back to whatever is currently on screen.
            let current = self.applied.unwrap_or((false, false));
            let wanted = (
                update.icon.notifications.as_confirmed().unwrap_or(current.0),
                update.icon.reviews.as_confirmed().unwrap_or(current.1),
            );

            if self.applied != Some(wanted) {
                let icon = self.icons.get(wanted.0, wanted.1).clone();
                match self.tray_icon.set_icon(Some(icon)) {
                    // Only record success. A failed update leaves this `None` so the next
                    // poll retries instead of believing the icon is already correct.
                    Ok(()) => self.applied = Some(wanted),
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
                } else if event.id == self.reviews_item_id {
                    if let Err(e) = open::that(scheduler::review_list_url()) {
                        logln!("failed to open browser: {e}");
                    }
                    // Reviewing is what clears the dot, so pull the next poll forward.
                    let _ = self.wake_tx.send(scheduler::Wake::Refresh);
                } else if event.id == self.setup_item_id {
                    setup_review_dot(&self.app_asset_path, &self.wake_tx);
                } else if event.id == self.quit_item_id {
                    event_loop.exit();
                }
            }
        }
    }

    let mut app = App {
        tray_icon,
        icons: tray_icons,
        open_item_id,
        reviews_item_id,
        setup_item_id,
        quit_item_id,
        wake_tx,
        app_asset_path,
        // The builder set the plain icon above, but treat that as unproven so the first
        // confirmed poll always writes the image it wants.
        applied: None,
    };

    if let Err(e) = event_loop.run_app(&mut app) {
        fatal(&format!("Event loop failed: {e}"));
    }
}
