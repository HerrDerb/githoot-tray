//! One blocking "here is a message, click OK" per platform.
//!
//! Every platform this app runs on has a reason to need this, and none of them can use the same
//! mechanism:
//!
//!   * Windows is built with `windows_subsystem = "windows"`, so there is no console. A `println!`
//!     goes nowhere and a startup failure would make the app vanish without a word.
//!   * macOS ships as an `LSUIElement` bundle. Launched from Finder, stdout goes to unified
//!     logging and **stdin is closed** — so the first-run flow's `read_line` would return
//!     immediately-or-never rather than waiting for a human, and the prompt explaining what to do
//!     would never be seen at all.
//!   * Linux runs as a plain binary, often from a terminal — but just as often from a `.desktop`
//!     entry, where a `println!` reaches nobody. `confirm` therefore shells out to `zenity` or
//!     `kdialog` rather than building a `gtk::MessageDialog`, even though `gtk` is already a
//!     dependency: GTK is main-thread-only, and `show_device_code_prompt` is reached both from the
//!     main thread at startup (before `gtk::main()` is even running, so a queued `glib::idle_add`
//!     would never fire) and from the poll thread during re-authentication. A subprocess is safe
//!     from any thread and needs neither of those two code paths — the same reasoning that picked
//!     `osascript` over `NSAlert` on macOS.
//!
//! Centralised in one function because the alternative is the same three-way `cfg` fork repeated
//! at five call sites in `main.rs` and `access_token.rs`.

/// Shows `msg` under `title` and blocks until the user acknowledges it.
///
/// Best effort throughout: a dialog that cannot be shown must never take the app down with it,
/// because every caller has already written the same information to the log.
pub fn message(title: &str, msg: &str) {
    #[cfg(target_os = "windows")]
    {
        use std::ptr::null_mut;
        use winapi::um::winuser::{MB_ICONINFORMATION, MB_OK, MessageBoxW};

        let title_w: Vec<u16> = title.encode_utf16().chain(Some(0)).collect();
        let msg_w: Vec<u16> = msg.encode_utf16().chain(Some(0)).collect();
        unsafe {
            MessageBoxW(null_mut(), msg_w.as_ptr(), title_w.as_ptr(), MB_OK | MB_ICONINFORMATION);
        }
    }

    #[cfg(target_os = "macos")]
    {
        // `osascript` rather than a native NSAlert: it needs no extra dependency, and it can be
        // called from any thread, which `show_auth_prompt` relies on. `display dialog` blocks
        // until a button is clicked with no implicit timeout, which is exactly MessageBoxW's
        // contract.
        //
        // The script is passed as a single argument, so nothing reaches a shell — but it *is*
        // AppleScript source, and the messages arriving here include multi-line GitHub error
        // bodies that contain quotes. Hence `escape`.
        let script = format!(
            r#"display dialog "{}" with title "{}" buttons {{"OK"}} default button "OK" with icon note"#,
            escape(msg),
            escape(title)
        );
        let _ = std::process::Command::new("osascript")
            .arg("-e")
            .arg(script)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        println!("\n{title}: {msg}\nPress Enter to continue...");
        let _ = std::io::stdin().read_line(&mut String::new());
    }
}

/// Shows `msg` under `title` with two choices and blocks until the user picks one.
///
/// Returns `true` for the accepting choice (`accept_label`), `false` for the declining one
/// (`exit_label`). Best effort like `message`: if the dialog mechanism itself is unavailable the
/// caller gets `true` back, so a platform quirk degrades to "do the thing anyway" rather than
/// leaving the user with a prompt they never saw and an action that never happened. An explicit
/// choice by the user is always honoured, including the declining one.
fn confirm(title: &str, msg: &str, accept_label: &str, exit_label: &str) -> bool {
    #[cfg(target_os = "windows")]
    {
        use std::ptr::null_mut;
        use winapi::um::winuser::{IDCANCEL, MB_ICONQUESTION, MB_OKCANCEL, MessageBoxW};

        // `MessageBoxW` cannot relabel its buttons without pulling in `TaskDialog` and a manifest
        // for ComCtl32 v6, so the choice is spelled out in the body text instead and the native
        // OK/Cancel pair carries it: OK accepts, Cancel declines.
        // Listed rather than written into a sentence, because the labels are capitalised for use as
        // real buttons on the other two platforms and read badly mid-clause.
        let full_msg = format!("{msg}\n\nOK:  {accept_label}\nCancel:  {exit_label}");
        let title_w: Vec<u16> = title.encode_utf16().chain(Some(0)).collect();
        let msg_w: Vec<u16> = full_msg.encode_utf16().chain(Some(0)).collect();
        let result = unsafe {
            MessageBoxW(null_mut(), msg_w.as_ptr(), title_w.as_ptr(), MB_OKCANCEL | MB_ICONQUESTION)
        };
        // A failed call returns 0, which must read as accept, not as Cancel — so this checks for
        // the one button that means stop rather than the one that means go.
        result != IDCANCEL
    }

    #[cfg(target_os = "macos")]
    {
        // `cancel button` binds Escape and the window's close button to the declining choice, so
        // every way out of the dialog maps to one of the two outcomes this function promises.
        let script = format!(
            r#"display dialog "{}" with title "{}" buttons {{"{}", "{}"}} default button "{}" cancel button "{}" with icon note"#,
            escape(msg),
            escape(title),
            escape(exit_label),
            escape(accept_label),
            escape(accept_label),
            escape(exit_label)
        );
        match std::process::Command::new("osascript")
            .arg("-e")
            .arg(script)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
        {
            Ok(out) if out.status.success() => {
                String::from_utf8_lossy(&out.stdout).contains(accept_label)
            }
            // A non-zero exit means the cancel-designated button (or Escape, or the close button)
            // was used — a real choice, not a failure, so it is honoured as a decline.
            Ok(_) => false,
            // `osascript` itself could not run. Unlike a real choice this says nothing about what
            // the user wants, so it must not silently swallow the action.
            Err(_) => true,
        }
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        // No display server at all (a systemd user service, a bare TTY): `zenity` is very likely
        // still *installed*, and would fail to init and exit non-zero — indistinguishable from a
        // deliberate Cancel. Checking the environment first is what keeps that from silently
        // reading as a decline.
        let has_display =
            std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some();

        if has_display {
            // Two tools, tried in order, because neither ships everywhere: `zenity` on GTK
            // desktops, `kdialog` on KDE. Text is passed as its own argument, so nothing reaches a
            // shell; it is also markup-free, so zenity's default Pango parsing has nothing to
            // misread (`--no-markup` is deliberately not used — it is missing from older zenity).
            let attempts: [(&str, Vec<String>); 2] = [
                (
                    "zenity",
                    vec![
                        "--question".to_string(),
                        format!("--title={title}"),
                        format!("--text={msg}"),
                        format!("--ok-label={accept_label}"),
                        format!("--cancel-label={exit_label}"),
                    ],
                ),
                (
                    "kdialog",
                    vec![
                        "--title".to_string(),
                        title.to_string(),
                        "--yesno".to_string(),
                        msg.to_string(),
                        "--yes-label".to_string(),
                        accept_label.to_string(),
                        "--no-label".to_string(),
                        exit_label.to_string(),
                    ],
                ),
            ];

            for (tool, args) in attempts {
                match std::process::Command::new(tool)
                    .args(&args)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                {
                    // Only a clean 0 accepts: zenity uses 1 for Cancel, 5 for its timeout and -1
                    // for an internal error, and none of those are a request to continue.
                    Ok(status) => return status.success(),
                    // The binary is absent, so nothing was shown and nothing was chosen — try the
                    // next tool rather than inventing an answer.
                    Err(_) => continue,
                }
            }
        }

        // Nothing graphical available. Print what a dialog would have said and return `true`, so
        // the caller still performs the action — for the device flow that means the browser opens
        // by itself, which is exactly what this app did on Linux before there was a dialog at all.
        //
        // Deliberately no `stdin().read_line` here, unlike `message`: this is reachable at startup
        // with nobody waiting to be asked (see the same reasoning in `main.rs`'s
        // `load_pr_credential`), and blocking there would hold the tray icon hostage.
        println!();
        println!("━━━  {title}  ━━━");
        println!("{msg}");
        println!("  (no zenity/kdialog available, continuing as if you chose \"{accept_label}\")");
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!();
        let _ = exit_label;
        true
    }
}

/// Copies `text` to the system clipboard. Best effort, like everything else here: a clipboard
/// that cannot be reached (no X11/Wayland session, some sandboxed environment) must not stop the
/// device flow, since the code is also always shown in the dialog/console text as a fallback.
fn copy_to_clipboard(text: &str) {
    match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(text.to_string())) {
        Ok(()) => {}
        Err(e) => crate::logln!("could not copy the device code to the clipboard: {e}"),
    }
}

/// The accepting choice on the authorization prompt. Named once because Windows spells it into the
/// body text and macOS matches it against `osascript`'s stdout, so the two must not drift.
const OPEN_WEBSITE_LABEL: &str = "Copy code & open website";

/// Displays a Device Flow authorization prompt and, if the user accepts, copies the device code and
/// opens GitHub's verification page.
///
/// The browser launch lives *here* rather than at the call sites on purpose. Opening it up front —
/// which is what both call sites used to do — means the browser takes focus and the dialog
/// explaining what to do with the code lands behind it, so the user is looking at a page asking for
/// a code they have not been told about. Gating it on the button makes the order deterministic:
/// read, click, page opens, paste.
///
/// The clipboard is written twice by design. Once up front, so the code is available even when no
/// dialog can be shown at all, and again on the click, because minutes may have passed with the
/// dialog sitting there and something else may own the clipboard by now.
///
/// Tagged with `subject` so a user running more than one device flow at once (notifications and PR
/// status can each need one) can tell the two dialogs apart. Shared by `access_token` (the
/// notifications credential) and `github_app` (the PR-status credential) — both run the same GitHub
/// Device Flow, just against different Client IDs and permissions, so the prompt itself has nothing
/// credential-specific about it.
///
/// Runs on a background thread, and must keep doing so: the caller's very next act is to start
/// polling GitHub for the token, and blocking that thread on a dialog would mean no token ever
/// arrives no matter what the user does in the browser.
///
/// The details also go to the log, because this is reachable from the poll thread long after
/// startup — and on Linux launched from a desktop entry there is no terminal to print to.
pub fn show_device_code_prompt(subject: &str, user_code: &str, verification_uri: &str) {
    crate::logln!("{subject}: authorization required — open {verification_uri} and enter code {user_code}");
    copy_to_clipboard(user_code);

    let title = format!("{subject}: Authorization Required");
    let body = format!(
        "GitHub needs you to authorize this app.\n\nYour one-time code is:  {user_code}\n\nPaste it at {verification_uri} to finish."
    );
    let subject = subject.to_string();
    let user_code = user_code.to_string();
    let verification_uri = verification_uri.to_string();

    std::thread::spawn(move || {
        if confirm(&title, &body, OPEN_WEBSITE_LABEL, "Close") {
            copy_to_clipboard(&user_code);
            if let Err(e) = open::that(&verification_uri) {
                crate::logln!("could not open browser automatically: {e}");
            }
        } else {
            crate::logln!(
                "{subject}: prompt dismissed. The code is on the clipboard and in this log, \
                 and polling continues until it expires"
            );
        }
    });
}

/// Records that a Device Flow authorization succeeded. Log only, deliberately no dialog — a second
/// "it worked!" box has nothing to say that the tray icon coming to life does not already say.
///
/// Nothing needs closing by the time this runs: the "Authorization Required" prompt is dismissed by
/// the button click that opens the browser, which necessarily happens *before* the user authorizes
/// anything. So there is no stale dialog to hold a handle to.
pub fn show_auth_success(subject: &str) {
    crate::logln!("{subject}: authorization successful");
}

/// Escapes a string for embedding in an AppleScript string literal.
///
/// Backslash first, or escaping the quotes would then have their own backslashes doubled.
/// Literal newlines are legal inside an AppleScript string and render as line breaks in the
/// dialog, so they are deliberately left alone.
#[cfg(target_os = "macos")]
fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn quotes_and_backslashes_are_escaped_in_that_order() {
        assert_eq!(escape(r#"a "b" c"#), r#"a \"b\" c"#);
        assert_eq!(escape(r"back\slash"), r"back\\slash");
        // The pathological case: a backslash immediately before a quote must not end up escaping
        // the escape.
        assert_eq!(escape(r#"\""#), r#"\\\""#);
    }

    #[test]
    fn newlines_survive_untouched() {
        assert_eq!(escape("one\ntwo"), "one\ntwo");
    }
}

#[cfg(test)]
mod confirm_tests {
    use super::*;

    /// The one part no unit test can reach: a real desktop session with a real button to click.
    /// `#[ignore]`d because a CI runner has no business requiring one. Run by hand with
    /// `cargo test -- --ignored shows_a_two_button_dialog` and follow the printed instruction.
    #[test]
    #[ignore = "needs a real desktop session and a human to click"]
    fn shows_a_two_button_dialog() {
        let accepted = confirm(
            "git-system-tray: dialog self-test",
            &format!("Click \"{OPEN_WEBSITE_LABEL}\" for this test to pass."),
            OPEN_WEBSITE_LABEL,
            "Close",
        );
        assert!(accepted, "the accepting button must return true");
    }

    /// With no display server the graphical path is skipped entirely and the fallback continues as
    /// if accepted, so a headless launch still opens the browser. Linux-only: it is the only arm
    /// that consults the environment.
    #[test]
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    fn headless_falls_back_to_accepting() {
        // SAFETY: single-threaded test, and both variables are restored before it returns.
        let (display, wayland) = (std::env::var_os("DISPLAY"), std::env::var_os("WAYLAND_DISPLAY"));
        unsafe {
            std::env::remove_var("DISPLAY");
            std::env::remove_var("WAYLAND_DISPLAY");
        }

        let accepted = confirm("headless", "no display here", "accept", "decline");

        unsafe {
            if let Some(v) = display {
                std::env::set_var("DISPLAY", v);
            }
            if let Some(v) = wayland {
                std::env::set_var("WAYLAND_DISPLAY", v);
            }
        }
        assert!(accepted, "no display must degrade to accepting, not to declining");
    }
}

#[cfg(test)]
mod clipboard_tests {
    use super::*;

    /// Exercises the one part a unit test cannot reach in CI: an actual clipboard/display
    /// session. `#[ignore]`d because a CI runner has no business requiring one. Run by hand with
    /// `cargo test -- --ignored copy_to_clipboard`.
    #[test]
    #[ignore = "needs a real clipboard/display session"]
    fn copies_text_that_can_be_read_back() {
        let marker = format!("git-system-tray-clipboard-test-{}", std::process::id());
        copy_to_clipboard(&marker);

        let read_back = arboard::Clipboard::new()
            .and_then(|mut cb| cb.get_text())
            .expect("must be able to read back what was just written");
        assert_eq!(read_back, marker);
    }
}
