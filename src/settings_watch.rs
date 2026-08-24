//! Watches `config.txt` after the tray's Settings entry opens it, and offers a restart once the edits
//! have settled.
//!
//! **Why the file and not the editor.** The obvious design is to launch an editor, wait for it to exit,
//! then check the file. It does not work. The handler for `text/plain` is typically a DBus-activated,
//! single-instance application — gedit and VS Code both are — so the launcher sends a message and exits
//! immediately, and any process there is to wait on is gone long before the user has typed anything. A
//! "wait for close" built that way fires the instant the editor opens.
//!
//! So this watches the *content*: notice it differ from what it was, wait for it to stop changing, then
//! ask. That works with every editor, including ones that never exit, and it expresses the actual goal —
//! settings are read only at startup, so a change is what matters, not a window closing.
//!
//! Content, not mtime: editors touch mtime freely, and gedit saves by writing a temporary file and
//! renaming it over the target, which changes mtime and inode with no regard for whether the bytes
//! differ. `icons::write_icon_if_changed` compares contents for the same reason.

use crate::{errorln, infoln};
use crate::update::RestartPlan;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// How often the file is read. A settings edit is a human-scale event, so this is deliberately lazy.
const POLL_EVERY: Duration = Duration::from_secs(1);

/// How long the content must hold still before the change counts as finished. Long enough to sit out a
/// save-as-you-think editor and an atomic rename, short enough that the prompt still feels like a
/// response to what the user just did.
const SETTLE: Duration = Duration::from_secs(3);

/// How long to keep watching before giving up quietly.
///
/// The watch is scoped to the menu click that started it, so it must end on its own: a user who opens
/// settings, changes nothing and wanders off should not leave a thread reading a file until the process
/// dies, nor be restarted by an edit they make an hour later having forgotten this was armed.
const WATCH_WINDOW: Duration = Duration::from_secs(15 * 60);

/// What the tracker concluded from one reading.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing to act on yet: unchanged, still changing, or nothing readable this time round.
    KeepWatching,
    /// Changed, and the new content has held still for `SETTLE`.
    Settled,
}

/// Decides when an edit is finished, with no clock and no filesystem of its own so it can be tested.
///
/// The whole risk in this file is acting at the wrong moment — mid-save, or on a change that was undone
/// — so the judgement lives here as a value and the I/O stays in `spawn`.
pub struct SettleTracker {
    /// Content as it was when the watch began. Compared against, so an edit that is undone before the
    /// settle window elapses correctly counts as no change at all.
    baseline: Vec<u8>,
    /// The differing content last seen, and when this exact content was first seen. Reset every time the
    /// content changes again, which is what makes a burst of saves settle once rather than each time.
    pending: Option<(Vec<u8>, Instant)>,
}

impl SettleTracker {
    pub fn new(baseline: Vec<u8>) -> Self {
        Self { baseline, pending: None }
    }

    /// Folds one reading in. `content` is `None` when the file could not be read.
    pub fn observe(&mut self, content: Option<Vec<u8>>, now: Instant, settle: Duration) -> Verdict {
        let Some(content) = content else {
            // Unreadable is not evidence of anything. An atomic save briefly leaves no file at the
            // target path, so treating this as "unchanged" would throw away a pending change at exactly
            // the moment it was being made.
            return Verdict::KeepWatching;
        };

        if content == self.baseline {
            // Back to where it started — an edit typed and undone, or a save of an untouched buffer.
            self.pending = None;
            return Verdict::KeepWatching;
        }

        match &self.pending {
            // Still moving: restart the settle clock against the newest content.
            Some((seen, _)) if *seen != content => {
                self.pending = Some((content, now));
                Verdict::KeepWatching
            }
            Some((_, since)) if now.duration_since(*since) >= settle => Verdict::Settled,
            Some(_) => Verdict::KeepWatching,
            None => {
                self.pending = Some((content, now));
                Verdict::KeepWatching
            }
        }
    }
}

/// Editors that need a terminal to host them.
///
/// A tray app has no console — on Windows by `windows_subsystem = "windows"`, on macOS by being an
/// `LSUIElement` bundle, and on Linux by usually being started from a desktop entry. Spawning one of
/// these would either fail or, worse, appear to succeed while sitting invisibly on a pipe nobody reads,
/// leaving the user waiting for a window that never comes. Falling through to the desktop association is
/// the better answer for these.
const TERMINAL_EDITORS: [&str; 9] =
    ["vi", "vim", "nvim", "nano", "pico", "emacsclient", "micro", "helix", "hx"];

/// Opens the settings file in something that can actually edit it.
///
/// `$VISUAL`/`$EDITOR` first, then the desktop association. The environment variables are the user's own
/// declaration of what edits text, while the association only says what *opens* a file — on a stock
/// desktop several plain-text-ish types open in a viewer or a browser, which is a read-only look at a file
/// this menu entry exists to let you change.
///
/// `config.txt` is `text/plain`, which does normally get a real editor, so on most machines the fallback
/// would have been fine. The preference is kept because it costs one call and removes the guess.
///
/// Returns whether anything was launched, so the caller only arms the watch when a window is coming.
pub fn open_for_editing(path: &std::path::Path) -> bool {
    if let Some(editor) = preferred_editor() {
        match open::with_detached(path, &editor) {
            Ok(()) => {
                infoln!("opened {} in {editor}", path.display());
                return true;
            }
            // Not fatal: the association below may well work, and saying which step failed is the
            // difference between a fixable report and "it did nothing".
            Err(e) => errorln!("could not open {} in {editor} ({e}) — trying the desktop default", path.display()),
        }
    }

    match open::that_detached(path) {
        Ok(()) => true,
        Err(e) => {
            errorln!("failed to open {}: {e}", path.display());
            false
        }
    }
}

/// `$VISUAL`, else `$EDITOR`. The I/O half; the judgement is in [`choose_editor`].
fn preferred_editor() -> Option<String> {
    let raw = std::env::var("VISUAL").ok().or_else(|| std::env::var("EDITOR").ok());
    choose_editor(raw.as_deref())
}

/// Whether `raw` names an editor worth launching from a process with no terminal.
///
/// Pure, so the deny-list can be tested without setting environment variables — which tests run in
/// parallel and would race on. Compared on the command's file name, so `/usr/bin/vim` and `vim -u NONE`
/// are both recognised.
fn choose_editor(raw: Option<&str>) -> Option<String> {
    let command = raw?.trim();
    if command.is_empty() {
        return None;
    }

    // Only the program name matters for the comparison; any arguments are left on for the launch.
    let program = command.split_whitespace().next()?;
    let name = std::path::Path::new(program)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(program)
        .to_ascii_lowercase();
    if TERMINAL_EDITORS.contains(&name.as_str()) {
        infoln!("$EDITOR is {name}, which needs a terminal — using the desktop default instead");
        return None;
    }
    Some(command.to_string())
}

/// Guards against a second watch. Clicking Settings three times must not arm three threads that each
/// prompt about the same save — the same hazard `scheduler::UPDATE_IN_FLIGHT` exists for, kept here
/// rather than at the call site so the flag cannot be set without the thread that clears it.
static WATCHING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Watches `path` on its own thread and offers a restart once an edit settles.
///
/// One-shot: after asking once, the watch ends whatever the answer. Declining is a decision, not a
/// reason to ask again on the next save, and a user who says Later gets their edits at the next start.
///
/// `restart` is the same closure the update thread uses to hand a `RestartPlan` to the UI thread, so the
/// actual restart runs through exactly one code path on every platform.
pub fn spawn(path: PathBuf, restart: impl Fn(RestartPlan) + Send + 'static) {
    use std::sync::atomic::Ordering;

    // `swap` rather than load-then-store, so two clicks in quick succession cannot both get past.
    if WATCHING.swap(true, Ordering::SeqCst) {
        infoln!("already watching {} for edits", path.display());
        return;
    }

    std::thread::spawn(move || {
        // Every `return` below has to clear the flag, so the body is wrapped rather than trusted.
        watch(&path, restart);
        WATCHING.store(false, Ordering::SeqCst);
    });
}

/// The watch itself, split out so `spawn` can clear `WATCHING` however this returns.
fn watch(path: &std::path::Path, restart: impl Fn(RestartPlan)) {
    // A missing file reads as empty rather than aborting: `Config::load` writes a default at startup,
    // but if it could not, creating the file counts as a change worth restarting for.
    let baseline = std::fs::read(path).unwrap_or_default();
    let mut tracker = SettleTracker::new(baseline);
    let deadline = Instant::now() + WATCH_WINDOW;

    loop {
        std::thread::sleep(POLL_EVERY);
        let now = Instant::now();
        if now >= deadline {
            infoln!("stopped watching {} for edits — nothing changed", path.display());
            return;
        }

        if tracker.observe(std::fs::read(path).ok(), now, SETTLE) == Verdict::KeepWatching {
            continue;
        }

        infoln!("settings changed — asking whether to restart");
        let accepted = crate::dialog::confirm_restart(
            "githoot-tray: settings changed",
            "Settings are only read when the app starts.\n\nRestart now to apply them?",
        );
        if !accepted {
            infoln!("restart declined — the new settings apply at the next start");
            return;
        }

        match crate::update::restart_target() {
            Ok(plan) => restart(plan),
            // Nothing has been changed on disk, so there is nothing to undo — say so and leave the
            // running app alone.
            Err(e) => crate::dialog::report(
                "githoot-tray: could not restart",
                &format!("{e}\n\nThe new settings will apply the next time you start the app."),
            ),
        }
        return;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SETTLE_FOR_TEST: Duration = Duration::from_secs(3);

    fn at(base: Instant, secs: u64) -> Instant {
        base + Duration::from_secs(secs)
    }

    #[test]
    fn unchanged_content_never_settles() {
        let start = Instant::now();
        let mut t = SettleTracker::new(b"a=1".to_vec());
        for s in 0..10 {
            assert_eq!(
                t.observe(Some(b"a=1".to_vec()), at(start, s), SETTLE_FOR_TEST),
                Verdict::KeepWatching
            );
        }
    }

    #[test]
    fn a_change_settles_once_it_holds_still() {
        let start = Instant::now();
        let mut t = SettleTracker::new(b"a=1".to_vec());
        assert_eq!(t.observe(Some(b"a=2".to_vec()), start, SETTLE_FOR_TEST), Verdict::KeepWatching);
        assert_eq!(
            t.observe(Some(b"a=2".to_vec()), at(start, 2), SETTLE_FOR_TEST),
            Verdict::KeepWatching,
            "two seconds is inside the settle window"
        );
        assert_eq!(t.observe(Some(b"a=2".to_vec()), at(start, 3), SETTLE_FOR_TEST), Verdict::Settled);
    }

    /// The reason the settle clock tracks *which* content it saw: someone typing and saving repeatedly
    /// should be asked once, at the end, not on the first save.
    #[test]
    fn each_further_change_restarts_the_settle_clock() {
        let start = Instant::now();
        let mut t = SettleTracker::new(b"a=1".to_vec());
        t.observe(Some(b"a=2".to_vec()), start, SETTLE_FOR_TEST);
        assert_eq!(
            t.observe(Some(b"a=3".to_vec()), at(start, 2), SETTLE_FOR_TEST),
            Verdict::KeepWatching
        );
        assert_eq!(
            t.observe(Some(b"a=3".to_vec()), at(start, 4), SETTLE_FOR_TEST),
            Verdict::KeepWatching,
            "four seconds since the first change, but only two since the latest one"
        );
        assert_eq!(t.observe(Some(b"a=3".to_vec()), at(start, 5), SETTLE_FOR_TEST), Verdict::Settled);
    }

    /// Typed, saved, then undone and saved again. Nothing has really changed, so nothing should be asked.
    #[test]
    fn an_edit_that_is_undone_does_not_settle() {
        let start = Instant::now();
        let mut t = SettleTracker::new(b"a=1".to_vec());
        t.observe(Some(b"a=2".to_vec()), start, SETTLE_FOR_TEST);
        assert_eq!(
            t.observe(Some(b"a=1".to_vec()), at(start, 1), SETTLE_FOR_TEST),
            Verdict::KeepWatching
        );
        assert_eq!(
            t.observe(Some(b"a=1".to_vec()), at(start, 9), SETTLE_FOR_TEST),
            Verdict::KeepWatching,
            "long past the settle window, but the content is the baseline again"
        );
    }

    /// The atomic-save window: gedit writes a temporary file and renames it over the target, so a read
    /// can briefly find nothing. Discarding the pending change there would lose the edit being saved.
    #[test]
    fn an_unreadable_moment_does_not_discard_a_pending_change() {
        let start = Instant::now();
        let mut t = SettleTracker::new(b"a=1".to_vec());
        t.observe(Some(b"a=2".to_vec()), start, SETTLE_FOR_TEST);
        assert_eq!(t.observe(None, at(start, 1), SETTLE_FOR_TEST), Verdict::KeepWatching);
        assert_eq!(
            t.observe(Some(b"a=2".to_vec()), at(start, 3), SETTLE_FOR_TEST),
            Verdict::Settled,
            "the change survived the gap and settles on its original clock"
        );
    }

    /// Nor should an unreadable file be mistaken for a change in its own right.
    #[test]
    fn an_unreadable_file_alone_never_settles() {
        let start = Instant::now();
        let mut t = SettleTracker::new(b"a=1".to_vec());
        for s in 0..10 {
            assert_eq!(t.observe(None, at(start, s), SETTLE_FOR_TEST), Verdict::KeepWatching);
        }
    }

    /// A file that did not exist when the watch began, then appears.
    #[test]
    fn a_file_appearing_from_nothing_is_a_change() {
        let start = Instant::now();
        let mut t = SettleTracker::new(Vec::new());
        t.observe(Some(b"updateCheck=off".to_vec()), start, SETTLE_FOR_TEST);
        assert_eq!(
            t.observe(Some(b"updateCheck=off".to_vec()), at(start, 3), SETTLE_FOR_TEST),
            Verdict::Settled
        );
    }

    // ── Choosing an editor ──────────────────────────────────────────────────

    /// A graphical editor is exactly what we want to launch, arguments and all.
    #[test]
    fn a_graphical_editor_is_used() {
        assert_eq!(choose_editor(Some("gedit")).as_deref(), Some("gedit"));
        assert_eq!(choose_editor(Some("  code -w  ")).as_deref(), Some("code -w"));
        assert_eq!(choose_editor(Some("/usr/bin/gedit")).as_deref(), Some("/usr/bin/gedit"));
    }

    /// The failure this deny-list prevents is the quiet one: a tray app has no terminal, so spawning
    /// vim does not error, it just sits on a pipe while the user waits for a window.
    #[test]
    fn a_terminal_editor_is_refused_so_the_desktop_default_is_used() {
        for editor in TERMINAL_EDITORS {
            assert_eq!(choose_editor(Some(editor)), None, "{editor} needs a terminal");
        }
    }

    /// Recognised through a path and through arguments, since `$EDITOR` is commonly either.
    #[test]
    fn a_terminal_editor_is_recognised_however_it_is_written() {
        for written in ["/usr/bin/vim", "vim -u NONE", "  NVIM  ", "/snap/bin/nvim -p", "Nano"] {
            assert_eq!(choose_editor(Some(written)), None, "{written} should be recognised as terminal");
        }
    }

    #[test]
    fn an_unset_or_empty_editor_falls_through() {
        assert_eq!(choose_editor(None), None);
        assert_eq!(choose_editor(Some("")), None);
        assert_eq!(choose_editor(Some("   ")), None);
    }

    /// The constants have to stay in a sane relationship to each other, or the loop cannot work: the
    /// settle window must span several reads, and the watch must outlast a settle by a wide margin.
    #[test]
    fn the_timings_are_consistent_with_each_other() {
        assert!(SETTLE >= POLL_EVERY * 2, "a settle must span more than one reading");
        assert!(WATCH_WINDOW > SETTLE * 10, "the watch must comfortably outlast one settle");
    }
}
