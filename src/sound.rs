//! The hoot: one short sound, played when a PR axis goes from nothing to something.
//!
//! ## Why an embedded file and a temp copy
//!
//! The clip ships inside the binary for the same reason the tray glyphs do (`icons`): a single file to
//! install, and no way for the sound to go missing on a user's machine. Every player available on the
//! three platforms takes a *path*, not bytes, so the embedded copy is written once per process into the
//! system temp directory and every later hoot reuses it. The write is idempotent — same name, same
//! length check — so a stale copy from an older version is replaced rather than played.
//!
//! ## Why an external player rather than an audio crate
//!
//! Decoding MP3 in-process means an audio stack (`rodio` and its dependency tree) in a tray app whose
//! entire job is one HTTP poll. Every target already has a decoder: Windows in `winmm`, macOS in
//! `afplay`, Linux in whatever the user's desktop installed. The cost of that choice is honest and
//! bounded: on Linux, a machine with none of the players below stays silent. That is a missing hoot,
//! not a broken app, which is why every failure in here is logged and swallowed.
//!
//! ## Why it never blocks the caller
//!
//! The poll loop calls this between two HTTP requests, and playback takes about as long as the clip.
//! So each hoot runs on its own short-lived thread, and an `AtomicBool` gate means a second hoot
//! arriving while one is still playing is dropped rather than layered on top of it — three axes can
//! turn over in the same cycle, and three overlapping hoots is a noise, not a notification.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::{errorln, infoln};

/// The clip itself. See `assets/` and the module doc for why it is embedded.
///
/// **Not this project's own work, unlike every other bundled asset.** A sound effect by elevenlabs.io,
/// from their free sound-effects library, used under a free plan that requires attribution to
/// elevenlabs.io and permits non-commercial use only. `NOTICE` at the repository root is the full
/// statement, and the reason `LICENSE`'s public-domain dedication does not reach this one file.
///
/// The path is all the code knows, so swapping in a differently-licensed clip is a one-file change
/// with no code edit — which is what anyone wanting to use this app commercially has to do.
const HOOT: &[u8] = include_bytes!("../assets/hoot.mp3");

/// Name of the temp copy. Deliberately fixed rather than unique per process: a second instance of the
/// app would otherwise leave a second file behind on every launch, and the content is identical.
const HOOT_FILE: &str = "githoot-hoot.mp3";

/// Whether a hoot is currently playing. See the module doc: overlapping hoots are dropped.
static PLAYING: AtomicBool = AtomicBool::new(false);

/// Plays the hoot, returning immediately.
///
/// A no-op when one is already playing, or when the clip cannot be unpacked. Never panics and never
/// propagates an error: a notification sound is not worth failing a poll cycle over.
pub fn hoot() {
    // `Acquire`/`Release` rather than `Relaxed` only so the release below cannot be reordered ahead
    // of the playback it guards; the flag itself is the whole shared state.
    if PLAYING.swap(true, Ordering::Acquire) {
        return;
    }
    let Some(path) = clip_path().cloned() else {
        PLAYING.store(false, Ordering::Release);
        return;
    };
    // A detached thread: the caller is the poll loop, which has an HTTP request to get back to.
    let spawned = std::thread::Builder::new().name("hoot".to_string()).spawn(move || {
        play(&path);
        PLAYING.store(false, Ordering::Release);
    });
    if let Err(e) = spawned {
        errorln!("could not start the hoot thread: {e}");
        PLAYING.store(false, Ordering::Release);
    }
}

/// Path to the unpacked clip, written on first use.
///
/// `None` if it could not be written, which is remembered rather than retried: the reason a temp file
/// cannot be written (no permission, no space) does not usually clear inside one run, and retrying it
/// on every arrival would put the same error in the log forever.
fn clip_path() -> Option<&'static PathBuf> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| {
        let path = std::env::temp_dir().join(HOOT_FILE);
        // Only write when what is there is not already this clip. Length is enough: the bytes are
        // fixed at compile time, so a differing length is the only way a stale file can differ in
        // practice, and hashing 33KB on every launch to learn the same thing is not worth it.
        let current_len = std::fs::metadata(&path).ok().map(|m| m.len());
        if current_len != Some(HOOT.len() as u64) {
            if let Err(e) = std::fs::write(&path, HOOT) {
                errorln!("could not unpack the hoot to {}: {e}", path.display());
                return None;
            }
            // Carries the attribution the clip's licence asks for into the app itself, not just the
            // repository, and costs one line in the log once per run. See `HOOT`.
            infoln!("unpacked the hoot (sound effect by elevenlabs.io) to {}", path.display());
        }
        Some(path)
    })
    .as_ref()
}

/// Windows has a decoder in `winmm`, reachable through MCI's string interface.
///
/// `mpegvideo` is the MCI device type that handles MP3 (the name is historical — it is the MPEG
/// device, audio included). `play ... wait` blocks until the clip ends, which is what makes the
/// `close` below correct: closing a still-playing device cuts it off.
#[cfg(target_os = "windows")]
fn play(path: &Path) {
    // A fixed alias, safe because `PLAYING` guarantees one hoot at a time per process. Two *processes*
    // would each have their own MCI device list, so the alias cannot collide across them either.
    const ALIAS: &str = "githoot_hoot";

    if mci(&format!("open \"{}\" type mpegvideo alias {ALIAS}", path.display())) {
        // Whatever `play` does, the device must be closed, or the file stays locked for the life of
        // the process and the next unpack cannot replace it.
        mci(&format!("play {ALIAS} wait"));
        mci(&format!("close {ALIAS}"));
    }
}

// The one `winmm` entry point this needs, declared here rather than taken from `winapi`: that crate's
// 0.3 line binds `PlaySound` but not MCI, and `PlaySound` is PCM-only — it cannot play an MP3 at all.
// Rather than add a whole second Windows-bindings crate for one function with two pointer arguments,
// the declaration lives here next to its only caller.
#[cfg(target_os = "windows")]
#[link(name = "winmm")]
unsafe extern "system" {
    fn mciSendStringW(
        command: *const u16,
        return_string: *mut u16,
        return_length: u32,
        callback: *mut std::ffi::c_void,
    ) -> u32;
}

/// Sends one MCI command string, returning whether it succeeded.
#[cfg(target_os = "windows")]
fn mci(command: &str) -> bool {
    use std::os::windows::ffi::OsStrExt;

    let wide: Vec<u16> = std::ffi::OsStr::new(command)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `wide` is a NUL-terminated UTF-16 buffer that outlives the call, and a null return
    // buffer with length 0 is the documented way to say "no reply wanted".
    let code = unsafe { mciSendStringW(wide.as_ptr(), std::ptr::null_mut(), 0, std::ptr::null_mut()) };
    if code != 0 {
        // The numeric code rather than `mciGetErrorString`: a second FFI call and a second buffer to
        // get a sentence nobody reads, for a sound that failed to play.
        errorln!("hoot: MCI `{command}` failed with code {code}");
    }
    code == 0
}

/// macOS ships `afplay`, which is exactly this and nothing else.
#[cfg(target_os = "macos")]
fn play(path: &Path) {
    spawn_player(&[("afplay", &[])], path);
}

/// Linux has no one answer, so try the common ones in order of how likely they are to be present on a
/// desktop that plays MP3 at all. `paplay`/`aplay` are deliberately absent: both are PCM-only and
/// would "succeed" on an MP3 by playing static.
#[cfg(target_os = "linux")]
fn play(path: &Path) {
    spawn_player(
        &[
            ("mpv", &["--no-video", "--really-quiet"]),
            ("ffplay", &["-nodisp", "-autoexit", "-loglevel", "quiet"]),
            ("mpg123", &["-q"]),
            ("gst-play-1.0", &["--quiet"]),
            ("cvlc", &["--play-and-exit", "--quiet"]),
        ],
        path,
    );
}

/// Runs the first player that exists, waiting for it so the `PLAYING` gate spans the actual sound.
///
/// "Exists" is decided by trying to run it, not by looking for it: a `PATH` lookup of our own would be
/// a second, subtly different answer to the same question.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn spawn_player(candidates: &[(&str, &[&str])], path: &Path) {
    for (program, args) in candidates {
        match std::process::Command::new(program).args(*args).arg(path).status() {
            Ok(status) if status.success() => return,
            // It ran and failed: the file or the decoder is the problem, not the player's absence, so
            // trying the next one is unlikely to help — but it is cheap, and one decoder rejecting an
            // MP3 while another accepts it does happen. Log and continue.
            Ok(status) => errorln!("hoot: {program} exited with {status}"),
            // Almost always "not installed". Not worth a line each on a machine with one player.
            Err(_) => continue,
        }
    }
    errorln!("hoot: no usable audio player found — install one of mpv, ffplay or mpg123 to hear it");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The clip must actually be in the binary. An empty `include_bytes!` compiles fine and would
    /// leave every hoot silently playing nothing.
    #[test]
    fn the_clip_is_embedded() {
        assert!(HOOT.len() > 1_000, "the embedded hoot is suspiciously small: {} bytes", HOOT.len());
        // The first frame of an MP3 is either an ID3 tag or a frame sync (`0xFF 0xEx`). Cheap proof
        // that what got embedded is the audio file and not, say, a Git LFS pointer.
        let looks_like_mp3 = HOOT.starts_with(b"ID3") || (HOOT[0] == 0xFF && HOOT[1] & 0xE0 == 0xE0);
        assert!(looks_like_mp3, "the embedded hoot does not start like an MP3");
    }

    /// Actually makes a noise. Ignored by default — a test suite that plays audio on every run is a
    /// test suite people mute. Run it with `cargo test -- --ignored hoot_is_audible` when the clip or
    /// a player command changes.
    #[test]
    #[ignore = "plays audio; run deliberately"]
    fn hoot_is_audible() {
        hoot();
        // The play happens on a detached thread, so give it the length of the clip to be heard before
        // the test process exits and takes the thread with it.
        std::thread::sleep(std::time::Duration::from_secs(4));
    }

    /// Unpacking must be idempotent: the second call writes nothing and answers the same path.
    #[test]
    fn unpacking_is_idempotent_and_lands_the_whole_clip() {
        let first = clip_path().expect("the clip must unpack");
        let second = clip_path().expect("the clip must still be there");
        assert_eq!(first, second);
        assert_eq!(
            std::fs::metadata(first).expect("unpacked file must exist").len(),
            HOOT.len() as u64
        );
    }
}
