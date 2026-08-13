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
//!   * Linux runs as a plain binary, often from a terminal, and has no dependency here worth
//!     adding a GTK dialog for.
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

/// Copies `text` to the system clipboard. Best effort, like everything else here: a clipboard
/// that cannot be reached (no X11/Wayland session, some sandboxed environment) must not stop the
/// device flow, since the code is also always shown in the dialog/console text as a fallback.
fn copy_to_clipboard(text: &str) {
    match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(text.to_string())) {
        Ok(()) => {}
        Err(e) => crate::logln!("could not copy the device code to the clipboard: {e}"),
    }
}

/// Displays a Device Flow authorization prompt, tagged with `subject` so a user running more than
/// one device flow at once (notifications and PR status can each need one) can tell them apart.
/// Where dialogs are used this opens a non-blocking one in a background thread so polling can
/// proceed immediately. The device code is copied to the clipboard first, so entering it on
/// GitHub's page is a paste rather than a manual retype — the dialog text says so instead of
/// offering a "Copy" button, which native `MessageBoxW`/`display dialog` cannot add without a
/// meaningfully bigger dialog implementation (Windows' `TaskDialog`, its own manifest, etc.) for
/// a convenience the auto-copy already delivers with one less click.
///
/// Shared by `access_token` (the notifications credential) and `github_app` (the PR-status
/// credential) — both run the same GitHub Device Flow, just against different Client IDs and
/// permissions, so the prompt itself has nothing credential-specific about it.
///
/// The details also go to the log, because this is reachable from the poll thread long after
/// startup — and on Linux launched from a desktop entry there is no terminal to print to.
pub fn show_device_code_prompt(subject: &str, user_code: &str, verification_uri: &str) {
    crate::logln!("{subject}: authorization required — open {verification_uri} and enter code {user_code}");
    copy_to_clipboard(user_code);

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    {
        let title = format!("{subject}: Authorization Required");
        let user_code = user_code.to_string();
        let verification_uri = verification_uri.to_string();
        std::thread::spawn(move || {
            message(
                &title,
                &format!(
                    "Open: {}\n\nCode (already copied to your clipboard): {}\n\nPaste it on that page, then this dialog can be closed.",
                    verification_uri, user_code
                ),
            );
        });
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        println!();
        println!("━━━  {subject}: Authorization Required  ━━━");
        println!("  1. Open:  {}", verification_uri);
        println!("  2. Enter: {} (already copied to your clipboard)", user_code);
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!();
    }
}

/// Records that a Device Flow authorization succeeded. Log only, deliberately no dialog — a
/// second "it worked!" box is one more click of friction on top of the "Authorization Required"
/// prompt the user already dismisses by hand, with nothing new to say. That first prompt is not
/// auto-closed on success (nothing here holds a handle to it), so it can briefly outlive its
/// usefulness once the code has been entered; that is judged the smaller annoyance.
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
