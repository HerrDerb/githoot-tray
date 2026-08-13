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
        // AppleScript source, and the messages arriving here include multi-line `gh` errors that
        // contain quotes. Hence `escape`.
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
