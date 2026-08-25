// On Windows, use the "windows" subsystem so no console window is created.
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

//! Main entry point for the GitHoot Tray application.
//! Handles cross-platform initialization and tray icon setup.

mod access_token;
mod config;
mod dialog;
mod github;
mod github_app;
mod github_status;
mod icons;
mod log;
mod scheduler;
mod settings_watch;
mod sound;
mod state;
mod update;
mod version;

const NOTIFICATIONS_URL: &str = "https://github.com/notifications";

// ─── Command-line contract ────────────────────────────────────────────────────
//
// Two flags, both of which exist for the self-updater rather than for people.
//
// **This is a compatibility contract with every future release.** The updater in version N downloads
// version N+1 and then relies on N+1 honouring these flags: `--print-version` is how N verifies it
// downloaded what it meant to, and `--await-exit` is how N hands the tray over to N+1. Removing or
// renaming either would break updating *from* every release that already shipped, and those releases
// cannot be fixed retroactively. Treat them as frozen.
//
// Both are parsed as the very first thing each `main` does, ahead of `gtk::init()` on Linux and ahead
// of the single-instance mutex on Windows. After either, both features break: `--print-version` would
// need a display to answer, and `--await-exit` exists precisely to wait *before* the mutex is touched.

/// Print the version and exit. Used by the updater to smoke-test a freshly downloaded binary.
const FLAG_PRINT_VERSION: &str = "--print-version";
/// Wait for the given PID to exit before starting up. Used by the updater's restart handshake.
const FLAG_AWAIT_EXIT: &str = "--await-exit";

/// How long to wait for the old process before giving up and starting anyway.
///
/// Bounded rather than unbounded: a parent that somehow never exits must not leave the user with no
/// tray icon at all. Starting anyway is the safer failure — on Windows the mutex check that follows
/// will simply report "already running", which is a visible, explicable outcome.
#[cfg(target_os = "windows")]
const AWAIT_EXIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Handles the flags that must be answered before the app initialises anything.
///
/// Returns `true` if the process should exit immediately (the caller has been served).
fn handle_startup_flags() -> bool {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == FLAG_PRINT_VERSION {
            // Deliberately plain stdout with no logging: the updater parses this, and a log line or a
            // banner would have to be stripped back off. On Windows this still works under
            // `windows_subsystem = "windows"` when the caller supplies the pipe.
            println!("{}", version::VERSION);
            return true;
        }
        if arg == FLAG_AWAIT_EXIT {
            let pid = args.next();
            await_process_exit(pid.as_deref());
        }
    }
    false
}

/// Blocks until the process identified by `pid` has exited, or a timeout elapses.
///
/// Only meaningful on Windows, where the single-instance mutex is held until the old process exits and
/// a relaunch that starts first is turned away. Elsewhere this is a no-op: Linux re-execs over itself
/// so there is never a second process, and macOS waits in a shell before launching at all.
#[cfg_attr(not(target_os = "windows"), allow(unused_variables))]
fn await_process_exit(pid: Option<&str>) {
    #[cfg(target_os = "windows")]
    {
        use winapi::um::handleapi::CloseHandle;
        use winapi::um::processthreadsapi::OpenProcess;
        use winapi::um::synchapi::WaitForSingleObject;
        use winapi::um::winnt::SYNCHRONIZE;

        let Some(pid) = pid.and_then(|p| p.parse::<u32>().ok()) else {
            return;
        };

        // SAFETY: `OpenProcess` is safe to call with any PID; it returns null when the process is gone
        // or inaccessible, which is the common and expected case here.
        unsafe {
            let handle = OpenProcess(SYNCHRONIZE, 0, pid);
            if handle.is_null() {
                // Already exited, or not ours to wait on. Either way there is nothing to wait for.
                return;
            }
            // Waiting on the process object rather than retrying the mutex: the object signals at
            // termination, which is the same instant the mutex is released, so this is a real signal
            // instead of a guessed delay.
            WaitForSingleObject(handle, AWAIT_EXIT_TIMEOUT.as_millis() as u32);
            CloseHandle(handle);
        }
    }
}

/// Brings up the shared PR-status credential (the `github_app` Device Flow) as far as it can go
/// *without involving the user*, and confirms it can actually see something.
///
/// Deliberately never opens a browser and never blocks on a human. A saved credential is reused, and
/// refreshed silently if it is expiring, but when a full sign-in is needed this returns
/// `PrStatus::NeedsAuth` and startup carries on: the tray icon appears wearing a red exclamation
/// with an `Authenticate` entry on its menu, and the user starts the flow when it suits them. That is
/// the whole point — a tray app that opens a browser window before its icon has even appeared is
/// indistinguishable from something that has gone wrong.
///
/// Never fatal either way. Notifications are a separate, optional feature (see `config`) that does
/// not care whether this succeeds. The outcome does have to be *said*, though: a dark dot that means
/// "nobody could ask" looks exactly like a dark dot that means "nothing to review", and that
/// confusion is the bug this whole codebase is shaped around avoiding.
fn load_pr_credential(app_asset_path: &std::path::Path) -> github_app::PrStatus {
    let store = match github_app::PrTokenStore::load_saved(app_asset_path) {
        Ok(Some(store)) => store,
        // Nothing usable on disk. Not an error and not worth a dialog: it is the expected state on a
        // first run, and the icon and menu now say it plainly without interrupting anyone.
        Ok(None) => return github_app::PrStatus::NeedsAuth,
        // No HTTP client could be built at all, which is a broken TLS stack rather than a missing
        // credential. Clicking `Authenticate` would fail the same way, so this is `Off`, not
        // `NeedsAuth`.
        Err(e) => {
            let msg = format!("Could not set up GitHub access for PR status: {e}");
            infoln!("PR status disabled: {msg}");

            // On Windows there is no console (`windows_subsystem = "windows"`) and on macOS the app
            // ships as an `LSUIElement` bundle whose stdout goes to unified logging, so on both a
            // dialog is the only way this reaches someone who is not reading the log file.
            //
            // Not `dialog::message` on Linux: this runs during startup and nobody is waiting to be
            // asked anything, so its stdin fallback would block the app before the tray appears.
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            dialog::message("githoot-tray: PR status", &msg);
            #[cfg(not(any(target_os = "windows", target_os = "macos")))]
            eprintln!("\ngithoot-tray: PR status disabled\n\n{msg}\n");

            return github_app::PrStatus::Off("PR status off: setup failed".to_string());
        }
    };

    match store.installation_count() {
        Ok(0) => {
            infoln!("{}", github_app::PR_NOT_INSTALLED);
            github_app::PrStatus::Off(github_app::PR_NOT_INSTALLED.to_string())
        }
        Ok(_) => github_app::PrStatus::Ready(store),
        // Could not confirm installations — start anyway rather than refuse over a question we
        // could not even ask. Same "unreachable is not the same as invalid" reasoning
        // `access_token`'s saved-token check already uses.
        Err(e) => {
            errorln!("could not confirm GitHub App installations ({e}) — continuing anyway");
            github_app::PrStatus::Ready(store)
        }
    }
}

/// Returns the path to the application's asset directory in the user's home.
/// Creates the directory if it does not exist.
fn get_app_asset_path() -> Result<std::path::PathBuf, String> {
    let user_home = dirs::home_dir().ok_or("could not find home directory")?;
    let assets_path = user_home.join(".githoot-tray");
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

    // Before `gtk::init`: `--print-version` must answer without needing a display, since the updater
    // runs it as a smoke test on a machine whose session it knows nothing about.
    if handle_startup_flags() {
        return;
    }

    gtk::init().expect("Failed to initialize GTK.");

    let app_asset_path = match get_app_asset_path() {
        Ok(path) => path,
        Err(e) => {
            eprintln!("Fatal: {e}");
            std::process::exit(1);
        }
    };
    log::init(&app_asset_path);
    // After `log::init` so its findings are recorded, and at startup rather than at the end of an
    // install: on Windows the previous `.exe` cannot be deleted until the process holding it has gone,
    // and that process is this one's predecessor.
    update::clean_up_after_update();

    let icons = match icons::create_icons(&app_asset_path) {
        Ok(icons) => icons,
        Err(e) => {
            errorln!("fatal: {e}");
            std::process::exit(1);
        }
    };

    let config = config::Config::load(&app_asset_path);
    // Apply the configured verbosity before anything past startup logs. The few lines emitted
    // earlier (icon setup, a first-run default-config write) ran at the quiet default, which is the
    // right floor for them anyway.
    log::set_level(config.log_level);
    let tokens = if config.notification_indication {
        match access_token::TokenStore::load(&app_asset_path) {
            Ok(tokens) => Some(tokens),
            Err(e) => {
                errorln!("fatal: {e}");
                std::process::exit(1);
            }
        }
    } else {
        infoln!("notification indication off (enable with \"notificationIndication=on\" in config.txt)");
        None
    };

    // Skipped entirely when every PR signal is switched off. Without this guard a disabled feature
    // would still make a network call (`installation_count`) and, on Windows and macOS, could raise a
    // sign-in dialog — for something the user turned off.
    let pr = if config.any_pr_enabled() {
        load_pr_credential(&app_asset_path)
    } else {
        infoln!("all PR signals are off in config.txt — skipping PR sign-in entirely");
        github_app::PrStatus::Off("PR status off in config.txt".to_string())
    };

    let mut indicator = AppIndicator::new("github_notifications", "");
    indicator.set_status(AppIndicatorStatus::Active);
    indicator.set_icon(icons.get(false, false, false, false, false, false).as_str());

    // The poll loop waits on this channel, so a menu click can pull the next poll forward.
    let (wake_tx, wake_rx) = std::sync::mpsc::channel::<scheduler::Wake>();
    // Carries a completed install from the update thread to this one. Read after `gtk::main()` returns,
    // because that is the only point at which the tray is gone but the process still exists.
    let (restart_tx, restart_rx) = std::sync::mpsc::channel::<update::RestartPlan>();

    let mut menu = Menu::new();
    let item = MenuItem::with_label("Open GitHub Notifications");
    let open_wake_tx = wake_tx.clone();
    item.connect_activate(move |_| {
        if let Err(e) = open::that(NOTIFICATIONS_URL) {
            errorln!("failed to open browser: {e}");
        }
        // Whatever the user is about to read changes the answer, so re-poll soon rather than
        // leaving a stale "unread" icon up for a whole interval.
        let _ = open_wake_tx.send(scheduler::Wake::Refresh);
    });

    let reviews_item = MenuItem::with_label(state::REVIEWS_MENU_LABEL);
    let reviews_wake_tx = wake_tx.clone();
    reviews_item.connect_activate(move |_| {
        if let Err(e) = open::that(scheduler::pr_list_url(state::PrAxis::ReviewRequested)) {
            errorln!("failed to open browser: {e}");
        }
        // Reviewing is what clears the dot, so pull the next poll forward the same way the
        // notifications item does.
        let _ = reviews_wake_tx.send(scheduler::Wake::Refresh);
    });

    let ready_to_merge_item = MenuItem::with_label(state::PrAxis::ReadyToMerge.menu_label());
    let ready_to_merge_wake_tx = wake_tx.clone();
    ready_to_merge_item.connect_activate(move |_| {
        if let Err(e) = open::that(scheduler::pr_list_url(state::PrAxis::ReadyToMerge)) {
            errorln!("failed to open browser: {e}");
        }
        let _ = ready_to_merge_wake_tx.send(scheduler::Wake::Refresh);
    });

    let changes_requested_item = MenuItem::with_label(state::PrAxis::ChangesRequested.menu_label());
    let changes_requested_wake_tx = wake_tx.clone();
    changes_requested_item.connect_activate(move |_| {
        if let Err(e) = open::that(scheduler::pr_list_url(state::PrAxis::ChangesRequested)) {
            errorln!("failed to open browser: {e}");
        }
        let _ = changes_requested_wake_tx.send(scheduler::Wake::Refresh);
    });

    // Placed first so it is the obvious thing to click when the icon is wearing an exclamation and
    // every other entry is hidden. The click only *asks*: the device flow itself runs on the poll
    // thread (see `scheduler::Wake::Authenticate`), because it blocks for as long as the user takes
    // and doing that here would freeze the entire GTK main loop, tray icon and all.
    let authenticate_item = MenuItem::with_label(state::AUTHENTICATE_MENU_LABEL);
    let authenticate_wake_tx = wake_tx.clone();
    authenticate_item.connect_activate(move |_| {
        let _ = authenticate_wake_tx.send(scheduler::Wake::Authenticate);
    });

    // Placed with Authenticate at the top, for the same reason: when the icon is wearing a mark, the
    // thing that clears it should be the first thing under the cursor. Starts hidden — unlike the four
    // signal entries, offering to install an update before one is known to exist would be a lie, and
    // `show_all` below would otherwise make it visible.
    let update_item = MenuItem::with_label(state::UPDATE_MENU_LABEL);
    let update_wake_tx = wake_tx.clone();
    update_item.connect_activate(move |_| {
        let _ = update_wake_tx.send(scheduler::Wake::UpdateNow);
    });

    // Opens the file and hands off to the poll thread, which arms the watcher that offers a restart once
    // the edits settle. The open happens here because it is instant; the fifteen-minute watch does not.
    let settings_item = MenuItem::with_label(state::SETTINGS_MENU_LABEL);
    let settings_path = config::config_path(&app_asset_path);
    let settings_wake_tx = wake_tx.clone();
    settings_item.connect_activate(move |_| {
        // Only arm the watch if a window is actually coming — a failed open would otherwise leave a
        // thread reading a file nobody is editing.
        if settings_watch::open_for_editing(&settings_path) {
            let _ = settings_wake_tx.send(scheduler::Wake::SettingsOpened);
        }
    });

    // Sibling of the Authenticate entry: conditional, and the counterpart of a mark on the icon. Since
    // that mark is the *same* exclamation for both, this entry is what tells the two states apart.
    let status_item = MenuItem::with_label(state::STATUS_MENU_LABEL);
    status_item.connect_activate(move |_| {
        if let Err(e) = open::that(github_status::STATUS_PAGE_URL) {
            errorln!("failed to open the GitHub status page: {e}");
        }
    });

    let quit_item = MenuItem::with_label("Quit");
    quit_item.connect_activate(|_| gtk::main_quit());
    // Two groups above the list, and one below it. Update and GitHub's own health go first because they
    // are about the app and the service rather than about your pull requests, and because when the icon
    // is wearing a mark they are two of the three things that explain it.
    //
    // The upper separator is only drawn when something is above it: a separator with nothing on one side
    // is a stray line, which reads as a rendering fault rather than a grouping.
    let top_separator = gtk::SeparatorMenuItem::new();
    let bottom_separator = gtk::SeparatorMenuItem::new();

    menu.append(&update_item);
    menu.append(&status_item);
    menu.append(&top_separator);
    menu.append(&authenticate_item);
    menu.append(&item);
    menu.append(&reviews_item);
    menu.append(&ready_to_merge_item);
    menu.append(&changes_requested_item);
    menu.append(&bottom_separator);
    menu.append(&settings_item);
    menu.append(&quit_item);
    menu.show_all();
    // After `show_all`, which shows everything it is given. The poll loop turns these on the moment a
    // check finds a newer release, or finds GitHub having a bad day — and the separator with them, since
    // it has nothing to separate until one of them is up.
    update_item.set_visible(false);
    status_item.set_visible(false);
    top_separator.set_visible(false);

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
        scheduler::MenuItems {
            notifications: item.clone(),
            reviews: reviews_item.clone(),
            ready_to_merge: ready_to_merge_item.clone(),
            changes_requested: changes_requested_item.clone(),
            status: status_item.clone(),
            top_separator: top_separator.clone(),
            authenticate: authenticate_item.clone(),
            update: update_item.clone(),
        },
        scheduler::PollInputs {
            tokens,
            pr,
            app_asset_path: app_asset_path.clone(),
            update_check: config.update_check,
            // Mapped over the axes rather than written as a literal, so the axis name appears on both
            // sides of each pairing. A literal `[a, b, c]` compiles, type-checks, and silently swaps
            // which bar a setting controls.
            pr_enabled: state::PrAxis::ALL.map(|axis| config.pr_enabled(axis)),
            sound: config.sound,
        },
        wake_rx,
        restart_tx,
    );

    gtk::main();

    // Past this point the GTK loop has unwound, so the StatusNotifierItem has been withdrawn properly
    // rather than dropped when the process image was replaced. Only now is it safe to hand over.
    //
    // `exec` rather than spawn-and-exit, deliberately: it keeps the PID, so a systemd user unit or any
    // other supervisor sees one continuous process instead of a service that died and an orphan that
    // appeared. It also inherits DISPLAY, WAYLAND_DISPLAY, DBUS_SESSION_BUS_ADDRESS and XDG_* exactly as
    // they were.
    if let Ok(plan) = restart_rx.try_recv() {
        exec_into(&plan);
    }
}

/// Replaces this process with the newly installed binary. Only returns if it failed.
#[cfg(target_os = "linux")]
fn exec_into(plan: &update::RestartPlan) -> ! {
    use std::os::unix::process::CommandExt;

    infoln!("restarting into {}", plan.target.display());
    // Args are forwarded so a launcher's own flags survive the hand-over. `current_exe`'s path is used
    // rather than argv[0], which may be relative to a working directory that has since changed.
    let error = std::process::Command::new(&plan.target)
        .args(std::env::args_os().skip(1))
        .exec();

    // `exec` only returns on failure. Try the binary that was working a moment ago before giving up.
    errorln!("could not start the updated binary ({error}) — falling back to the previous version");
    if let Some(backup) = plan.backup.as_ref()
        && backup.exists()
    {
        let _ = std::fs::rename(backup, &plan.target);
        let error = std::process::Command::new(&plan.target)
            .args(std::env::args_os().skip(1))
            .exec();
        errorln!("the previous version would not start either ({error})");
    }
    dialog::report(
        "githoot-tray: restart failed",
        "The update was installed but the app could not restart. Start it again by hand.",
    );
    std::process::exit(1);
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
    errorln!("fatal: {message}");
    dialog::message("githoot-tray", message);
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

    // Before the single-instance guard, and that order is load-bearing: `--await-exit` exists to wait
    // for the *previous* process to die, and the mutex it holds is released only when it does. Check
    // the mutex first and an update's relaunch would always be turned away as a duplicate.
    if handle_startup_flags() {
        return;
    }

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

        let name: Vec<u16> = "Local\\GitHootTray\0".encode_utf16().collect();
        SetLastError(0);
        let handle = CreateMutexW(null_mut(), 0, name.as_ptr());

        if handle.is_null() {
            eprintln!("Warning: could not create single-instance mutex (err {})", GetLastError());
        } else if GetLastError() == 0xB7 {
            // ERROR_ALREADY_EXISTS — another instance owns the mutex
            dialog::message("Already Running", "githoot-tray is already running.");
            return;
        }
        // On a fresh mutex (first instance) GetLastError() is 0 — fall through.
    }

    let app_asset_path = match get_app_asset_path() {
        Ok(path) => path,
        Err(e) => {
            // Not `fatal`: the log has no home yet, since finding that home is what just failed.
            dialog::message("githoot-tray", &format!("Fatal: {e}"));
            std::process::exit(1);
        }
    };
    log::init(&app_asset_path);
    // After `log::init` so its findings are recorded, and at startup rather than at the end of an
    // install: on Windows the previous `.exe` cannot be deleted until the process holding it has gone,
    // and that process is this one's predecessor.
    update::clean_up_after_update();

    let config = config::Config::load(&app_asset_path);
    // Apply the configured verbosity before anything past startup logs (see the other platform's
    // entry point for the reasoning).
    log::set_level(config.log_level);
    let tokens = if config.notification_indication {
        match access_token::TokenStore::load(&app_asset_path) {
            Ok(tokens) => Some(tokens),
            Err(e) => fatal(&format!("Could not authenticate with GitHub: {e}")),
        }
    } else {
        infoln!("notification indication off (enable with \"notificationIndication=on\" in config.txt)");
        None
    };

    // Skipped entirely when every PR signal is switched off. Without this guard a disabled feature
    // would still make a network call (`installation_count`) and, on Windows and macOS, could raise a
    // sign-in dialog — for something the user turned off.
    let pr = if config.any_pr_enabled() {
        load_pr_credential(&app_asset_path)
    } else {
        infoln!("all PR signals are off in config.txt — skipping PR sign-in entirely");
        github_app::PrStatus::Off("PR status off in config.txt".to_string())
    };

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
        ready_to_merge_item: tray_icon::menu::MenuItem,
        ready_to_merge_item_id: tray_icon::menu::MenuId,
        changes_requested_item: tray_icon::menu::MenuItem,
        changes_requested_item_id: tray_icon::menu::MenuId,
        /// Always present, so unlike the conditional entries it is never taken out or put back — but it
        /// still has to be re-appended by every `rebuild_menu`, which is exactly what it was missing.
        settings_item: tray_icon::menu::MenuItem,
        settings_item_id: tray_icon::menu::MenuId,
        /// Shown only while GitHub reports an incident.
        status_item: tray_icon::menu::MenuItem,
        status_item_id: tray_icon::menu::MenuId,
        /// Offered only while PR status is waiting to be authorized.
        authenticate_item: tray_icon::menu::MenuItem,
        authenticate_item_id: tray_icon::menu::MenuId,
        /// Offered only while a newer release exists. Its label carries the version, so it is
        /// relabelled as well as added and removed.
        update_item: tray_icon::menu::MenuItem,
        update_item_id: tray_icon::menu::MenuId,
        quit_item: tray_icon::menu::MenuItem,
        quit_item_id: tray_icon::menu::MenuId,
        /// Which image the tray is actually showing, as `[notifications, review_requested,
        /// ready_to_merge, changes_requested]`, as far as we know. `None` means "unproven", which
        /// forces the next update to re-apply rather than assume.
        applied: Option<[bool; 4]>,
        /// Whether the icon currently shows the needs-authorization variant. Separate from `applied`
        /// because it is not one of the four signals but a replacement for all of them.
        applied_needs_auth: Option<bool>,
        /// Likewise for the update arrow, which is a fifth independent signal rather than a replacement.
        applied_update: Option<bool>,
        /// And for the install entry's text, which carries the version.
        applied_update_label: Option<String>,
        /// Likewise for the three PR menu items' text, indexed by `PrAxis::index`, so an
        /// unchanged count does not rewrite the item.
        applied_labels: [Option<String>; 3],
        /// And for which entries the menu currently holds. Tracked separately from `applied` because a
        /// failed `set_icon` must not also suppress the menu update.
        applied_menu: Option<MenuShape>,
    }

    /// Which entries the menu should hold.
    ///
    /// A named struct rather than the `([bool; 4], bool, bool, …)` tuple this grew out of. Once there
    /// were three independent flags beside the signal array, a positional tuple was one transposition
    /// away from a menu that offers a sign-in during an outage — and it would compile, type-check, and
    /// be visible only by right-clicking the tray. The same reasoning as `config::Config::pr_enabled`.
    #[derive(Clone, Copy, PartialEq, Eq)]
    struct MenuShape {
        /// The four signals, indexed as `[notifications, then PrAxis::index + 1]`.
        wanted: [bool; 4],
        needs_auth: bool,
        status_degraded: bool,
        update: bool,
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
        let ready_to_merge_item =
            MenuItem::new(state::PrAxis::ReadyToMerge.menu_label(), true, None);
        let ready_to_merge_item_id = ready_to_merge_item.id().clone();
        let changes_requested_item =
            MenuItem::new(state::PrAxis::ChangesRequested.menu_label(), true, None);
        let changes_requested_item_id = changes_requested_item.id().clone();
        let authenticate_item = MenuItem::new(state::AUTHENTICATE_MENU_LABEL, true, None);
        let authenticate_item_id = authenticate_item.id().clone();
        let update_item = MenuItem::new(state::UPDATE_MENU_LABEL, true, None);
        let update_item_id = update_item.id().clone();
        let settings_item = MenuItem::new(state::SETTINGS_MENU_LABEL, true, None);
        let settings_item_id = settings_item.id().clone();
        let status_item = MenuItem::new(state::STATUS_MENU_LABEL, true, None);
        let status_item_id = status_item.id().clone();
        let quit_item = MenuItem::new("Quit", true, None);
        let quit_item_id = quit_item.id().clone();
        let menu = Menu::new();
        // Authenticate is the one entry that starts *absent*, unlike the four above. They start
        // present because nothing has been polled yet and an empty menu would offer no way to reach
        // GitHub in the app's first moments; this one starts absent because offering to authorize
        // something that may already be authorized is the misleading direction. The first update
        // arrives within a second and puts it in if it is needed.
        for (item, what) in [
            (&open_item, "open"),
            (&reviews_item, "reviews"),
            (&ready_to_merge_item, "ready to merge"),
            (&changes_requested_item, "changes requested"),
            (&settings_item, "settings"),
            (&quit_item, "quit"),
        ] {
            menu.append(item)
                .map_err(|e| format!("Failed to append {what} menu item: {e}"))?;
        }

        let tray_icon = TrayIconBuilder::new()
            .with_tooltip("GitHub Notifications")
            .with_icon(icons.get(false, false, false, false, false, false).clone())
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
            ready_to_merge_item,
            ready_to_merge_item_id,
            changes_requested_item,
            changes_requested_item_id,
            settings_item,
            settings_item_id,
            status_item,
            status_item_id,
            authenticate_item,
            authenticate_item_id,
            update_item,
            update_item_id,
            quit_item,
            quit_item_id,
            // The builder set the plain icon above, but treat that as unproven so the first
            // confirmed poll always writes the image it wants.
            applied: None,
            applied_needs_auth: None,
            applied_update: None,
            applied_update_label: None,
            applied_labels: [None, None, None],
            // The menu was built with the four signal entries present and every conditional entry
            // absent, and that much we did do, so it is recorded as such.
            applied_menu: Some(MenuShape {
                wanted: [true; 4],
                needs_auth: false,
                status_degraded: false,
                update: false,
            }),
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
    scheduler::start_notification_scheduler(
        scheduler::PollInputs {
            tokens,
            pr,
            app_asset_path: app_asset_path.clone(),
            update_check: config.update_check,
            // Mapped over the axes rather than written as a literal, so the axis name appears on both
            // sides of each pairing. A literal `[a, b, c]` compiles, type-checks, and silently swaps
            // which bar a setting controls.
            pr_enabled: state::PrAxis::ALL.map(|axis| config.pr_enabled(axis)),
            sound: config.sound,
        },
        wake_rx,
        proxy,
    );

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
        /// Resolved once rather than rebuilt per click, and held here for the same reason the Linux
        /// closure captures it: the click handler has no access to `app_asset_path`.
        settings_path: std::path::PathBuf,
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
                    errorln!("failed to open browser: {e}");
                }
                // Whatever the user is about to read changes the answer, so re-poll soon rather
                // than leaving a stale "unread" icon up for a whole interval.
                let _ = self.wake_tx.send(scheduler::Wake::Refresh);
            } else if *id == tray.reviews_item_id {
                if let Err(e) = open::that(scheduler::pr_list_url(state::PrAxis::ReviewRequested)) {
                    errorln!("failed to open browser: {e}");
                }
                // Reviewing is what clears the dot, so pull the next poll forward.
                let _ = self.wake_tx.send(scheduler::Wake::Refresh);
            } else if *id == tray.ready_to_merge_item_id {
                if let Err(e) = open::that(scheduler::pr_list_url(state::PrAxis::ReadyToMerge)) {
                    errorln!("failed to open browser: {e}");
                }
                let _ = self.wake_tx.send(scheduler::Wake::Refresh);
            } else if *id == tray.changes_requested_item_id {
                if let Err(e) = open::that(scheduler::pr_list_url(state::PrAxis::ChangesRequested)) {
                    errorln!("failed to open browser: {e}");
                }
                let _ = self.wake_tx.send(scheduler::Wake::Refresh);
            } else if *id == tray.authenticate_item_id {
                // Only asks. The device flow runs on the poll thread (see
                // `scheduler::Wake::Authenticate`), because it blocks for as long as the user takes
                // and doing that here would freeze the event loop and the tray with it.
                let _ = self.wake_tx.send(scheduler::Wake::Authenticate);
            } else if *id == tray.update_item_id {
                // Only asks. The download, verification and swap all happen on the update thread —
                // see `scheduler::Wake::UpdateNow`.
                let _ = self.wake_tx.send(scheduler::Wake::UpdateNow);
            } else if *id == tray.settings_item_id {
                // Opening is instant; the watch that follows is not, which is why only the open happens
                // here and the fifteen-minute watch is armed on the poll thread.
                if settings_watch::open_for_editing(&self.settings_path) {
                    let _ = self.wake_tx.send(scheduler::Wake::SettingsOpened);
                }
            } else if *id == tray.status_item_id {
                if let Err(e) = open::that(github_status::STATUS_PAGE_URL) {
                    errorln!("failed to open the GitHub status page: {e}");
                }
            } else if *id == tray.quit_item_id {
                event_loop.exit();
            }
        }

        /// Starts the newly installed binary and ends this process.
        ///
        /// On the UI thread on purpose: dropping the tray is what makes the shell remove the icon
        /// properly, and it has to happen before this process goes away or the taskbar is left holding a
        /// dead one.
        fn hand_over(&mut self, plan: update::RestartPlan, event_loop: &ActiveEventLoop) {
            infoln!("restarting into {}", plan.target.display());

            // First, so the icon is gone before its owner is.
            self.tray = None;

            match self.spawn_successor(&plan) {
                Ok(()) => {
                    event_loop.exit();
                    // Belt and braces. If `exit()` somehow does not unwind `run_app`, the Windows
                    // single-instance mutex is never released and the successor times out waiting for a
                    // process that will not die. A few lines here removes that whole hang class.
                    std::process::exit(0);
                }
                Err(e) => {
                    // Roll the swap back rather than leave the user with a binary that will not start.
                    // This is only reportable *because* the check happens before exiting — otherwise the
                    // reporter has already gone.
                    errorln!("the updated binary would not start ({e}) — rolling back");
                    if let Some(backup) = plan.backup.as_ref() {
                        let _ = std::fs::remove_file(&plan.target);
                        let _ = std::fs::rename(backup, &plan.target);
                    }
                    dialog::report(
                        "githoot-tray: update failed",
                        &format!(
                            "The update was installed but would not start, so the previous version \
                             has been put back.\n\n{e}"
                        ),
                    );
                }
            }
        }

        /// Launches the successor, and confirms it actually started.
        #[cfg(target_os = "windows")]
        fn spawn_successor(&self, plan: &update::RestartPlan) -> Result<(), String> {
            // `--await-exit` with our own PID: the successor waits on this process's handle, which
            // signals at termination — the same instant the single-instance mutex is released. Without
            // it the new instance would race the mutex and be turned away as a duplicate.
            let mut child = std::process::Command::new(&plan.target)
                .arg(FLAG_AWAIT_EXIT)
                .arg(std::process::id().to_string())
                .args(std::env::args_os().skip(1))
                .spawn()
                .map_err(|e| e.to_string())?;

            // A healthy successor is blocked in `WaitForSingleObject`, so "still running" is an
            // unambiguous "it started". An immediate exit means it could not run at all — Defender
            // quarantining an unsigned binary is the realistic cause — and that is worth catching here
            // rather than discovering as a tray icon that never came back.
            std::thread::sleep(std::time::Duration::from_millis(300));
            match child.try_wait() {
                Ok(None) => Ok(()),
                Ok(Some(status)) => Err(format!("it exited immediately with {status}")),
                Err(e) => Err(e.to_string()),
            }
        }

        #[cfg(target_os = "macos")]
        fn spawn_successor(&self, plan: &update::RestartPlan) -> Result<(), String> {
            // LaunchServices refuses to start a second instance of the same bundle identifier, and would
            // activate this one instead. So a detached shell waits for this process to disappear and only
            // then opens the bundle. `/bin/sh` is guaranteed present; the iteration count is bounded so a
            // parent that somehow never exits cannot leave an immortal waiter behind.
            //
            // The absolute path matters: `open -a <name>` resolves through the LaunchServices database,
            // which may still point at the bundle that was just renamed aside.
            let target = plan.target.display().to_string();
            if target.contains('\'') {
                return Err("the bundle path contains a quote, which is not safe to pass to sh".into());
            }
            let script = format!(
                "n=0; while kill -0 {pid} 2>/dev/null && [ $n -lt 300 ]; do sleep 0.2; n=$((n+1)); \
                 done; exec open '{target}'",
                pid = std::process::id()
            );
            std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(script)
                .spawn()
                .map(|_| ())
                .map_err(|e| e.to_string())
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
        fn rebuild_menu(&self, shape: MenuShape) {
            while self.menu.remove_at(0).is_some() {}

            let MenuShape { wanted, needs_auth, status_degraded, update } = shape;
            // A separator is only worth drawing when something sits above it; with nothing there it is a
            // stray line that reads as a rendering fault. Created fresh each rebuild rather than held on
            // `Tray`, because the whole menu is emptied first anyway and a separator carries no state.
            let top_separator = tray_icon::menu::PredefinedMenuItem::separator();
            let bottom_separator = tray_icon::menu::PredefinedMenuItem::separator();
            // Both sides have to be non-empty, or the rule has nothing to separate — see the Linux
            // equivalent in `scheduler` for the reasoning, which is deliberately identical.
            let top_group = update || status_degraded;
            let body_group = needs_auth || wanted.iter().any(|&on| on);
            let separate = top_group && body_group;

            let entries: [(&dyn tray_icon::menu::IsMenuItem, bool, &str); 11] = [
                // The top group: the app's own state and the service's, rather than anything about your
                // pull requests. First because when the icon is wearing a mark, two of the three things
                // that explain it are here.
                (&self.update_item, update, "update"),
                (&self.status_item, status_degraded, "github-status"),
                (&top_separator, separate, "top-separator"),
                (&self.authenticate_item, needs_auth, "authenticate"),
                // Notifications is *not* gated on `needs_auth`: it is a separate credential that may
                // be working perfectly, and hiding a working entry because a different one needs
                // attention would take away a feature that still functions.
                (&self.open_item, wanted[0], "notifications"),
                // The three PR entries share the credential that is missing, so none of them can
                // have anything behind them until it is obtained. Gated on `needs_auth` only, *not* on
                // the outage bit: an outage hides the bars because the icon has one exclamation to
                // give, but these counts are the last known good ones and the lists still open.
                (&self.reviews_item, !needs_auth && wanted[1], "review-requested"),
                (&self.ready_to_merge_item, !needs_auth && wanted[2], "ready-to-merge"),
                (&self.changes_requested_item, !needs_auth && wanted[3], "changes-requested"),
                // Unconditional, all three. Settings was missing from this list until now, which
                // silently deleted it on the first rebuild — the hazard a full rebuild trades for not
                // having to compute insert positions, and the reason every entry must be listed here.
                (&bottom_separator, true, "bottom-separator"),
                (&self.settings_item, true, "settings"),
                (&self.quit_item, true, "quit"),
            ];

            for (item, wanted, what) in entries {
                if !wanted {
                    continue;
                }
                if let Err(e) = self.menu.append(item) {
                    errorln!("failed to add the {what} menu item: {e}");
                }
            }
        }

        fn apply(&mut self, update: Update) {
            // `Unknown` on any axis deliberately leaves that part of the picture alone — a brief
            // failure should change the words, not make the icon flap. So an unknown axis falls
            // back to whatever is currently on screen.
            let current = self.applied.unwrap_or([false; 4]);
            let wanted = [
                update.icon.notifications.as_confirmed().unwrap_or(current[0]),
                update.icon.review_requested.as_confirmed().unwrap_or(current[1]),
                update.icon.ready_to_merge.as_confirmed().unwrap_or(current[2]),
                update.icon.changes_requested.as_confirmed().unwrap_or(current[3]),
            ];

            let needs_auth = update.icon.needs_auth;
            let update_available = update.icon.update_available;
            let exclamation = update.icon.shows_exclamation();
            let shape = MenuShape {
                wanted,
                needs_auth,
                status_degraded: update.icon.status_degraded,
                update: update_available,
            };

            if self.applied != Some(wanted)
                || self.applied_needs_auth != Some(exclamation)
                || self.applied_update != Some(update_available)
            {
                // One lookup, no override. The exclamation is a bit like any other now that it sits in
                // the bottom-left, so a missing credential, an incident or a failed poll all show the
                // mark *and* whatever counts are still known.
                let icon = self
                    .icons
                    .get(
                        wanted[0],
                        wanted[1],
                        wanted[2],
                        wanted[3],
                        update_available,
                        exclamation,
                    )
                    .clone();
                match self.tray_icon.set_icon(Some(icon)) {
                    // Only record success. A failed update leaves these `None` so the next
                    // poll retries instead of believing the icon is already correct.
                    Ok(()) => {
                        self.applied = Some(wanted);
                        // The *exclamation*, not `needs_auth`: this memo exists to decide whether the
                        // image on screen is still right, and an outage changes that image too.
                        self.applied_needs_auth = Some(exclamation);
                        self.applied_update = Some(update_available);
                    }
                    Err(e) => errorln!("failed to update tray icon: {e}"),
                }
            }

            if let Err(e) = self.tray_icon.set_tooltip(Some(&update.tooltip)) {
                errorln!("failed to update tray tooltip: {e}");
            }

            // The icon can only say "something is waiting". The number goes here, where there is
            // room for it, and in the tooltip. Only written on change, so an open menu is not
            // rebuilt underneath the user on every poll.
            //
            // Bound to a local first, not called as a method: a `&self` method here would borrow
            // all of `self` for the loop below, which conflicts with the `&mut self.applied_labels`
            // borrow the same loop needs. Direct field projections keep the two borrows disjoint.
            let pr_items = [&self.reviews_item, &self.ready_to_merge_item, &self.changes_requested_item];
            for (item, (label, applied_label)) in
                pr_items.into_iter().zip(update.pr_labels.iter().zip(&mut self.applied_labels))
            {
                if applied_label.as_deref() != Some(label.as_str()) {
                    item.set_text(label);
                    *applied_label = Some(label.clone());
                }
            }

            // An entry that opens an empty list is just a dead end, so it is taken out. `wanted`
            // serves double duty here: the icon shows a blue glyph exactly when there are unread
            // notifications, which is exactly when that menu entry has somewhere to go, and the
            // same holds for each dot and its matching entry.
            //
            // Note this inherits `wanted`'s treatment of `Unknown`: an axis we have lost track of
            // keeps whatever it last had. A failed poll must not remove an entry, because "I could
            // not ask" is not the same as "there is nothing there".
            // Relabelled before the rebuild, so a rebuild that lands in the same tick shows the new
            // text rather than the previous version's.
            if let Some(label) = update.update_label.as_deref()
                && self.applied_update_label.as_deref() != Some(label)
            {
                self.update_item.set_text(label);
                self.applied_update_label = Some(label.to_string());
            }

            if self.applied_menu != Some(shape) {
                self.rebuild_menu(shape);
                self.applied_menu = Some(shape);
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
                TrayEvent::Restart(plan) => self.hand_over(plan, event_loop),
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

    let mut app = App {
        tray,
        pending: None,
        wake_tx,
        settings_path: config::config_path(&app_asset_path),
    };

    if let Err(e) = event_loop.run_app(&mut app) {
        fatal(&format!("Event loop failed: {e}"));
    }
}
