// On Windows, use the "windows" subsystem so no console window is created.
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

//! Main entry point for the GitHub Tray Icon application.
//! Handles cross-platform initialization and tray icon setup.

mod access_token;
mod dialog;
mod gh_cli;
mod github;
mod icons;
mod log;
mod scheduler;
mod state;

const NOTIFICATIONS_URL: &str = "https://github.com/notifications";

/// Loads the review-search credential from `gh`.
///
/// Returns the credential and, when there is none, a short reason for the tooltip.
///
/// Never fatal. The notification half has its own narrow credential and does not care whether `gh`
/// exists, so a missing or under-scoped `gh` must cost the user the dot and nothing else. It does
/// have to be *said*, though: a dark dot that means "nobody could ask" looks exactly like a dark dot
/// that means "nothing to review", and that confusion is the bug this whole codebase is shaped
/// around avoiding.
fn load_review_credential(app_asset_path: &std::path::Path) -> (Option<gh_cli::ReviewToken>, Option<String>) {
    // Say so once per launch, because a credential nobody reads is still a credential on disk, and
    // the user cannot delete a file they do not know went stale.
    for stale in ["review_token.txt", "review_client_id.txt", "review_refresh_token.txt"] {
        if app_asset_path.join(stale).exists() {
            logln!("note: {stale} is no longer used (gh supplies this credential) and can be deleted");
        }
    }

    match gh_cli::ReviewToken::load() {
        Ok(token) => (Some(token), None),
        Err(why) => {
            // One line in the log, because the message is deliberately multi-line for a dialog.
            logln!("review dot disabled: {}", why.message().replace('\n', " "));

            // On Windows there is no console (`windows_subsystem = "windows"`) and on macOS the app
            // ships as an `LSUIElement` bundle whose stdout goes to unified logging, so on both a
            // dialog is the only way this reaches someone who is not reading the log file.
            //
            // Not `dialog::message` on Linux: this runs during startup and nobody is waiting to be
            // asked anything, so its stdin fallback would block the app before the tray appears.
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            dialog::message("git-system-tray: review dot", &why.message());
            #[cfg(not(any(target_os = "windows", target_os = "macos")))]
            eprintln!("\ngit-system-tray: review dot disabled\n\n{}\n", why.message());

            (None, Some(why.short()))
        }
    }
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

    let (reviews, reviews_off) = load_review_credential(&app_asset_path);

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

    let reviews_item = MenuItem::with_label(state::REVIEWS_MENU_LABEL);
    let reviews_wake_tx = wake_tx.clone();
    reviews_item.connect_activate(move |_| {
        if let Err(e) = open::that(scheduler::review_list_url()) {
            logln!("failed to open browser: {e}");
        }
        // Reviewing is what clears the dot, so pull the next poll forward the same way the
        // notifications item does.
        let _ = reviews_wake_tx.send(scheduler::Wake::Refresh);
    });

    let quit_item = MenuItem::with_label("Quit");
    quit_item.connect_activate(|_| gtk::main_quit());
    menu.append(&item);
    menu.append(&reviews_item);
    menu.append(&quit_item);
    menu.show_all();

    // The closest Linux equivalent of clicking the icon: the menu being popped up. Connected after
    // `show_all` so the initial layout pass is not mistaken for a click.
    //
    // Best effort, and unverified: under a StatusNotifierItem host the menu is exported over DBus
    // and drawn by the panel, so this signal may never fire in this process. Costs nothing if it
    // does not, since the tooltip and menu label are refreshed on the normal cadence regardless.
    let menu_wake_tx = wake_tx.clone();
    menu.connect_show(move |_| {
        let _ = menu_wake_tx.send(scheduler::Wake::PollNow);
    });

    indicator.set_menu(&mut menu);

    scheduler::start_notification_scheduler(
        indicator,
        icons,
        // Cloned rather than moved: the menu keeps the originals, and these handles are what the
        // poll loop relabels and shows or hides. GTK widgets are reference-counted, so both refer
        // to the same items.
        scheduler::MenuItems { notifications: item.clone(), reviews: reviews_item.clone() },
        tokens,
        reviews,
        reviews_off,
        wake_rx,
    );

    gtk::main();
}

// ─── Windows and macOS ────────────────────────────────────────────────────────
//
// One implementation for both: `tray-icon` driven from a `winit` event loop. The only structural
// difference is *when* the tray may be created — see `App::resumed`.

/// Reports a startup failure and exits.
///
/// With `windows_subsystem = "windows"`, and inside a macOS `LSUIElement` bundle, there is no
/// console, so a bare `expect` would make the app vanish without a word — indistinguishable, from
/// the user's side, from a tray icon that is simply wrong.
#[cfg(any(target_os = "windows", target_os = "macos"))]
fn fatal(message: &str) -> ! {
    logln!("fatal: {message}");
    dialog::message("git-system-tray", message);
    std::process::exit(1);
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn main() {
    use scheduler::{TrayEvent, Update};
    use tray_icon::menu::{Menu, MenuEvent, MenuItem};
    use tray_icon::{MouseButtonState, TrayIconBuilder, TrayIconEvent};
    use winit::application::ApplicationHandler;
    use winit::event::WindowEvent;
    use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
    use winit::window::WindowId;

    // ── Single-instance guard ────────────────────────────────────────────────
    // Windows only. CreateMutexW returns the existing handle if the named mutex already exists,
    // and GetLastError() reports ERROR_ALREADY_EXISTS. We intentionally never
    // call CloseHandle so the mutex lives until the process exits.
    // SetLastError(0) clears any stale error left by DLL/runtime init so that
    // the GetLastError check is always based on CreateMutexW's own result.
    //
    // Not ported to macOS: LaunchServices already declines to start a second copy of the same
    // `.app` from Finder, so the gap only shows if you deliberately run the inner binary twice,
    // and closing it properly would mean a file lock and a new dependency for no real user.
    #[cfg(target_os = "windows")]
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
            dialog::message("Already Running", "git-system-tray is already running.");
            return;
        }
        // On a fresh mutex (first instance) GetLastError() is 0 — fall through.
    }

    let app_asset_path = match get_app_asset_path() {
        Ok(path) => path,
        Err(e) => {
            // Not `fatal`: the log has no home yet, since finding that home is what just failed.
            dialog::message("git-system-tray", &format!("Fatal: {e}"));
            std::process::exit(1);
        }
    };
    log::init(&app_asset_path);

    let tokens = match access_token::TokenStore::load(&app_asset_path) {
        Ok(tokens) => tokens,
        Err(e) => fatal(&format!("Could not authenticate with GitHub: {e}")),
    };

    let (reviews, reviews_off) = load_review_credential(&app_asset_path);

    // ── Tray ─────────────────────────────────────────────────────────────────

    /// The tray icon, its menu, and what we believe is currently on screen.
    ///
    /// Grouped into one struct rather than living directly on `App` because on macOS none of it
    /// can exist until the event loop is running, and `Option<Tray>` says that far more clearly
    /// than eight separate `Option` fields whose states would have to agree.
    struct Tray {
        tray_icon: tray_icon::TrayIcon,
        icons: icons::IconSet<tray_icon::Icon>,
        /// The menu itself, so entries can be taken out when there is nothing behind them. `muda`
        /// has no per-item visibility, only `set_enabled`, so hiding means removing and re-adding.
        menu: tray_icon::menu::Menu,
        /// The items are held, not just their ids: they are re-appended when they come back, and
        /// the review count is written into its text.
        open_item: tray_icon::menu::MenuItem,
        open_item_id: tray_icon::menu::MenuId,
        reviews_item: tray_icon::menu::MenuItem,
        reviews_item_id: tray_icon::menu::MenuId,
        quit_item: tray_icon::menu::MenuItem,
        quit_item_id: tray_icon::menu::MenuId,
        /// Which image the tray is actually showing, as `(unread, review_pending)`, as far as we
        /// know. `None` means "unproven", which forces the next update to re-apply rather than
        /// assume.
        applied: Option<(bool, bool)>,
        /// Likewise for the menu text, so an unchanged count does not rewrite the item.
        applied_label: Option<String>,
        /// And for which entries the menu currently holds. Tracked separately from `applied`
        /// because a failed `set_icon` must not also suppress the menu update.
        applied_menu: Option<(bool, bool)>,
    }

    /// Decodes the icons, builds the menu, and creates the tray icon.
    ///
    /// A function rather than inline setup because the two platforms have to call it at different
    /// moments: on Windows the tray must be created up front on the main thread, while on macOS an
    /// `NSStatusItem` has nothing to attach to until `NSApplication` is running. Returning `Result`
    /// rather than calling `fatal` directly keeps that decision with the caller.
    fn build_tray() -> Result<Tray, String> {
        // Decode and composite the embedded PNG assets.
        let icons = icons::load_tray_icons()?;

        // Build the tray menu.
        //
        // Every entry starts present. Nothing has been polled yet, so both signals are `Unknown`,
        // and starting empty would mean the first second of the app's life offers no way to reach
        // GitHub. The first confirmed answer takes out whatever turns out to be empty.
        let open_item = MenuItem::new("Open GitHub Notifications", true, None);
        let open_item_id = open_item.id().clone();
        let reviews_item = MenuItem::new(state::REVIEWS_MENU_LABEL, true, None);
        let reviews_item_id = reviews_item.id().clone();
        let quit_item = MenuItem::new("Quit", true, None);
        let quit_item_id = quit_item.id().clone();
        let menu = Menu::new();
        for (item, what) in
            [(&open_item, "open"), (&reviews_item, "reviews"), (&quit_item, "quit")]
        {
            menu.append(item)
                .map_err(|e| format!("Failed to append {what} menu item: {e}"))?;
        }

        let tray_icon = TrayIconBuilder::new()
            .with_tooltip("GitHub Notifications")
            .with_icon(icons.get(false, false).clone())
            // Cloned rather than moved: `Menu` is a reference-counted handle, and the app keeps one
            // so it can take entries out later. The tray gets the same underlying menu.
            .with_menu(Box::new(menu.clone()))
            .build()
            .map_err(|e| format!("Failed to create tray icon: {e}"))?;

        Ok(Tray {
            tray_icon,
            icons,
            menu,
            open_item,
            open_item_id,
            reviews_item,
            reviews_item_id,
            quit_item,
            quit_item_id,
            // The builder set the plain icon above, but treat that as unproven so the first
            // confirmed poll always writes the image it wants.
            applied: None,
            applied_label: None,
            // The menu was built with every entry present, and that much we did do, so it is
            // recorded as such. Only a confirmed empty answer will take one out.
            applied_menu: Some((true, true)),
        })
    }

    // On Windows the tray is created here, before the event loop, exactly as it always was: the
    // shell needs it on the main thread and nothing has to be running first. macOS cannot do this
    // — an `NSStatusItem` created before `NSApplication` exists never appears in the menu bar — so
    // there it is deferred to `App::resumed`.
    #[cfg(target_os = "windows")]
    let tray = match build_tray() {
        Ok(tray) => Some(tray),
        Err(e) => fatal(&e),
    };
    #[cfg(target_os = "macos")]
    let tray: Option<Tray> = None;

    // Create the winit event loop with a custom event type so the background
    // thread can wake the loop and deliver notification updates.
    let event_loop: EventLoop<TrayEvent> = {
        let mut builder: winit::event_loop::EventLoopBuilder<TrayEvent> =
            EventLoop::with_user_event();

        // `LSUIElement` in Info.plist is not enough on its own: winit sets the NSApplication
        // activation policy itself at launch, and its default is `Regular` — which would put a Dock
        // icon and an app menu back, plist or no plist. `Accessory` is the runtime half of the same
        // statement, and saying it here means a bare `cargo run` behaves like the bundle does.
        #[cfg(target_os = "macos")]
        {
            use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
            builder.with_activation_policy(ActivationPolicy::Accessory);
        }

        match builder.build() {
            Ok(event_loop) => event_loop,
            Err(e) => fatal(&format!("Failed to create event loop: {e}")),
        }
    };
    let proxy = event_loop.create_proxy();

    // ── macOS: interactions arrive by callback, not by polled channel ─────────
    //
    // `muda` and `tray-icon` each offer exactly one of two delivery routes: a global channel you
    // poll, or a callback — and installing a callback switches the channel off. Windows can use the
    // channel because the tray's own message window wakes the event loop, so `about_to_wait` runs
    // and drains it. On macOS nothing is guaranteed to wake a loop sitting in `ControlFlow::Wait`
    // for a menu action that AppKit dispatched inside its own nested tracking loop — the click
    // would land in the channel and stay there until some unrelated event happened along. For an
    // app with no windows, that could be a very long time, and the symptom is a menu that does
    // nothing.
    //
    // A user event is the one thing that wakes the loop by definition, so the callbacks forward
    // into it. Installed before the tray is built, and before the poll thread starts, because both
    // handlers live behind a `OnceCell` that the first event initialises to `None` if it finds it
    // empty — set it late and it can never be set at all.
    #[cfg(target_os = "macos")]
    {
        let menu_proxy = proxy.clone();
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            let _ = menu_proxy.send_event(TrayEvent::MenuClick(event.id));
        }));

        let icon_proxy = proxy.clone();
        TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
            // Same button-down-only filter as the Windows drain, and for the same reasons.
            if let TrayIconEvent::Click { button_state: MouseButtonState::Down, .. } = event {
                let _ = icon_proxy.send_event(TrayEvent::IconClick);
            }
        }));
    }

    // The poll loop waits on this channel, so a menu click can pull the next poll forward.
    let (wake_tx, wake_rx) = std::sync::mpsc::channel::<scheduler::Wake>();

    // Launch the polling thread; it communicates back via the proxy.
    scheduler::start_notification_scheduler(tokens, reviews, reviews_off, wake_rx, proxy);

    // ── Application handler ──────────────────────────────────────────────────

    struct App {
        /// `None` until the tray exists. Only ever `None` on macOS, and only until `resumed`.
        tray: Option<Tray>,
        /// An update that arrived before there was a tray to show it on.
        ///
        /// The poll thread fires its first request the moment it starts, so on macOS an answer can
        /// beat `resumed`. Holding the newest one costs nothing and saves the icon from sitting
        /// visibly wrong for a whole poll interval.
        pending: Option<Update>,
        wake_tx: std::sync::mpsc::Sender<scheduler::Wake>,
    }

    impl App {
        /// Acts on a chosen menu entry.
        ///
        /// Shared by the two ways an entry can reach us: Windows drains `MenuEvent::receiver()` in
        /// `about_to_wait`, macOS gets a `muda` callback forwarded in as a user event.
        fn on_menu(&self, id: &tray_icon::menu::MenuId, event_loop: &ActiveEventLoop) {
            let Some(tray) = &self.tray else {
                return;
            };

            if *id == tray.open_item_id {
                if let Err(e) = open::that(NOTIFICATIONS_URL) {
                    logln!("failed to open browser: {e}");
                }
                // Whatever the user is about to read changes the answer, so re-poll soon rather
                // than leaving a stale "unread" icon up for a whole interval.
                let _ = self.wake_tx.send(scheduler::Wake::Refresh);
            } else if *id == tray.reviews_item_id {
                if let Err(e) = open::that(scheduler::review_list_url()) {
                    logln!("failed to open browser: {e}");
                }
                // Reviewing is what clears the dot, so pull the next poll forward.
                let _ = self.wake_tx.send(scheduler::Wake::Refresh);
            } else if *id == tray.quit_item_id {
                event_loop.exit();
            }
        }

        /// Acts on a click on the icon itself.
        ///
        /// A click means the user is looking at the icon right now, so fetch fresh data instead of
        /// showing them whatever the last poll happened to find up to a minute ago.
        fn on_icon_click(&self) {
            let _ = self.wake_tx.send(scheduler::Wake::PollNow);
        }
    }

    impl Tray {
        /// Rebuilds the menu so it only offers actions that have something behind them.
        ///
        /// Everything is removed and re-appended in a fixed order, rather than computing insert
        /// positions from the current contents: an off-by-one there silently reorders the menu,
        /// and a full rebuild cannot.
        ///
        /// Only called when the set actually changes, which is rare. It can still land while the
        /// user has the menu open, since nothing tells us whether it is showing.
        fn rebuild_menu(&self, notifications: bool, reviews: bool) {
            while self.menu.remove_at(0).is_some() {}

            for (item, wanted, what) in [
                (&self.open_item, notifications, "notifications"),
                (&self.reviews_item, reviews, "reviews"),
                // Quit is unconditional: a tray icon with no way out is a bug, not a tidy menu.
                (&self.quit_item, true, "quit"),
            ] {
                if !wanted {
                    continue;
                }
                if let Err(e) = self.menu.append(item) {
                    logln!("failed to add the {what} menu item: {e}");
                }
            }
        }

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

            // The icon can only say "something is waiting". The number goes here, where there is
            // room for it, and in the tooltip. Only written on change, so an open menu is not
            // rebuilt underneath the user on every poll.
            if self.applied_label.as_deref() != Some(update.reviews_label.as_str()) {
                self.reviews_item.set_text(&update.reviews_label);
                self.applied_label = Some(update.reviews_label);
            }

            // An entry that opens an empty list is just a dead end, so it is taken out. `wanted`
            // serves double duty here: the icon shows a blue glyph exactly when there are unread
            // notifications, which is exactly when that menu entry has somewhere to go, and the
            // same holds for the dot and the reviews entry.
            //
            // Note this inherits `wanted`'s treatment of `Unknown`: an axis we have lost track of
            // keeps whatever it last had. A failed poll must not remove an entry, because "I could
            // not ask" is not the same as "there is nothing there".
            if self.applied_menu != Some(wanted) {
                self.rebuild_menu(wanted.0, wanted.1);
                self.applied_menu = Some(wanted);
            }
        }
    }

    impl ApplicationHandler<TrayEvent> for App {
        /// Creates the tray on the first pass, if it does not exist yet.
        ///
        /// This is the macOS path: by the time winit calls this, `NSApplication` is up and an
        /// `NSStatusItem` has something to attach to. On Windows the tray was already built before
        /// the loop started, so this returns immediately and nothing about that platform changes.
        ///
        /// Guarded rather than unconditional because `resumed` is not once-only — winit calls it
        /// again after a suspend — and building a second tray would leave two icons in the bar.
        fn resumed(&mut self, _event_loop: &ActiveEventLoop) {
            if self.tray.is_some() {
                return;
            }

            let mut tray = match build_tray() {
                Ok(tray) => tray,
                Err(e) => fatal(&e),
            };

            // Whatever the poll thread answered while there was nowhere to put it.
            if let Some(update) = self.pending.take() {
                tray.apply(update);
            }

            self.tray = Some(tray);
        }

        fn window_event(
            &mut self,
            _event_loop: &ActiveEventLoop,
            _id: WindowId,
            _event: WindowEvent,
        ) {
        }

        /// Called for anything delivered from outside the event loop: poll results on both
        /// platforms, and on macOS the forwarded menu and icon interactions too.
        // `event_loop` is only read by the macOS arms, which is also the only place Quit can be
        // reached from on that platform.
        #[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
        fn user_event(&mut self, event_loop: &ActiveEventLoop, event: TrayEvent) {
            match event {
                TrayEvent::Update(update) => match &mut self.tray {
                    Some(tray) => tray.apply(update),
                    // Keep only the newest: an older answer is strictly less true, and `apply` is
                    // idempotent, so replaying a queue would buy nothing.
                    None => self.pending = Some(update),
                },
                #[cfg(target_os = "macos")]
                TrayEvent::MenuClick(id) => self.on_menu(&id, event_loop),
                #[cfg(target_os = "macos")]
                TrayEvent::IconClick => self.on_icon_click(),
            }
        }

        fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
            // Keep the loop sleeping until the next event arrives so we don't
            // burn CPU. Set before anything else, because a loop that stops waiting is a loop that
            // spins — that must hold even on the passes before the tray exists.
            event_loop.set_control_flow(ControlFlow::Wait);

            // Windows only. There the tray's own message window wakes the loop, so draining the
            // polled channels here catches everything. macOS installs callbacks instead, which
            // switches `muda` and `tray-icon` off these channels entirely — see `main`.
            #[cfg(target_os = "windows")]
            {
                // Drain the whole queue. `if let` handled one event per wakeup and then slept on
                // `ControlFlow::Wait`, so a second queued click sat unhandled until something else
                // happened to wake the loop.
                while let Ok(event) = MenuEvent::receiver().try_recv() {
                    self.on_menu(&event.id, event_loop);
                }

                // ── Clicks on the icon itself ────────────────────────────────
                // Only the button-down edge is counted. `Up` would double every click,
                // `DoubleClick` would add a third on top, and `Enter`/`Move`/`Leave` fire
                // continuously as the pointer crosses the tray — hooking those would turn a mouse
                // drifting past the clock into a stream of GitHub requests.
                let mut clicked = false;
                while let Ok(event) = TrayIconEvent::receiver().try_recv() {
                    if let TrayIconEvent::Click { button_state: MouseButtonState::Down, .. } = event
                    {
                        clicked = true;
                    }
                }
                // At most one per pass, so a double-click asks once.
                if clicked {
                    self.on_icon_click();
                }
            }
        }
    }

    let mut app = App { tray, pending: None, wake_tx };

    if let Err(e) = event_loop.run_app(&mut app) {
        fatal(&format!("Event loop failed: {e}"));
    }
}
