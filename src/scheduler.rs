//! Notification scheduler.
//!
//! Both platforms share one polling loop (`run_poll_loop`) and differ only in how an update is
//! handed to the UI thread. Linux forwards through an mpsc channel drained by a GTK timer;
//! Windows forwards through `EventLoopProxy::send_event`.
//!
//! The loop waits on `Receiver::recv_timeout` rather than `thread::sleep`. That one substitution
//! buys three things at once: pacing that adapts to GitHub's `x-poll-interval`, an on-demand
//! refresh when the user opens their notifications, and a clean exit when the UI goes away.

use crate::access_token::{ReviewTokenStore, TokenStore};
use crate::github;
use crate::logln;
use crate::state::{IconState, PollState, REFRESH_BURST};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

/// Floor between re-authentication attempts, so a credential GitHub keeps rejecting cannot
/// produce a storm of authorization prompts.
const MIN_REAUTH_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Search query for pull requests awaiting the user's review.
///
/// `-label:dependencies` is the conventional Dependabot marker, but it is applied by convention
/// rather than guaranteed — a repo with custom Dependabot config, or Renovate instead, would slip
/// through — so the bot authors are excluded by name as well.
///
/// `sort:updated-desc` from the equivalent UI search is deliberately absent: it is a UI-only
/// qualifier (the REST API takes separate `sort`/`order` params) and irrelevant when the only
/// thing read from the response is `total_count`.
const REVIEW_QUERY: &str = "is:pr review-requested:@me state:open archived:false \
                            -label:dependencies -author:app/dependabot -author:app/renovate";

/// Sort order for the browser view only. Meaningless to the API (which reads `total_count`), but
/// it is what makes the web page useful to look at.
const REVIEW_UI_SORT: &str = "sort:updated-desc";

/// The GitHub page listing the very PRs the dot is counting.
///
/// Built from `REVIEW_QUERY` rather than hardcoded, so the page can never disagree with the icon.
/// A hand-written URL drifts the moment the query changes — and a dot claiming 3 next to a page
/// showing 5 is worse than no dot at all.
pub fn review_list_url() -> String {
    format!(
        "https://github.com/pulls?q={}",
        percent_encode(&format!("{REVIEW_QUERY} {REVIEW_UI_SORT}"))
    )
}

/// Percent-encodes a query for use in a URL.
///
/// Escapes everything outside the RFC 3986 unreserved set — stricter than required, but never
/// wrong, and it avoids taking a dependency on `url` just for this.
fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len() * 3);
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(byte))
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Reason the poll loop was woken early.
pub enum Wake {
    /// The user opened a GitHub page, so what they have read is about to change.
    Refresh,
}

/// One rendering instruction for the UI thread.
#[derive(Debug)]
pub struct Update {
    pub icon: IconState,
    pub tooltip: String,
}

// ─── Shared polling core ──────────────────────────────────────────────────────

/// Spawns the poll thread.
///
/// `emit` returns `false` once the UI is gone, which ends the loop. A panic in here used to
/// freeze the icon at its last value with no trace; now it at least says so in the log.
fn spawn_poll_thread(
    app_asset_path: PathBuf,
    tokens: TokenStore,
    reviews: Option<ReviewTokenStore>,
    wake_rx: Receiver<Wake>,
    emit: impl FnMut(Update) -> bool + Send + 'static,
) {
    std::thread::spawn(move || {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            run_poll_loop(app_asset_path, tokens, reviews, wake_rx, emit)
        }));
        if outcome.is_err() {
            logln!("poll thread panicked — notification updates have stopped");
        }
    });
}

fn run_poll_loop(
    app_asset_path: PathBuf,
    mut tokens: TokenStore,
    mut reviews: Option<ReviewTokenStore>,
    wake_rx: Receiver<Wake>,
    mut emit: impl FnMut(Update) -> bool,
) {
    let client = match github::build_client() {
        Ok(client) => client,
        Err(e) => {
            logln!("fatal: could not build HTTP client: {e}");
            return;
        }
    };

    let mut state = PollState::new(reviews.is_some());
    let mut burst: VecDeque<Duration> = VecDeque::new();
    let mut last_reauth: Option<Instant> = None;
    let mut last_review_reauth: Option<Instant> = None;

    // While a refresh burst is draining we send no `If-None-Match`. A conditional request can
    // legitimately answer 304 from a cached view, which would leave a just-read icon stuck on
    // "unread" — the exact symptom the burst exists to cure.
    let mut skip_etag = false;

    loop {
        state.begin_cycle();

        // ── Pick up a credential configured while we were running ────────────
        // This is what lets the tray menu's "Set up review dot…" item work with no restart: the
        // user saves the file, and the next cycle adopts it. One small file read per cycle.
        if let Some(store) = ReviewTokenStore::reload_static_token(&app_asset_path, reviews.as_ref())
        {
            reviews = Some(store);
            state.enable_reviews();
        }

        // ── Notifications ────────────────────────────────────────────────────
        let etag = if skip_etag {
            None
        } else {
            state.notifications_etag().map(str::to_string)
        };
        let response = github::poll_notifications(&client, tokens.token(), etag.as_deref());
        let notif_kind = response.result.kind();
        state.apply_notifications(response);

        // ── Reviews (serial, not concurrent: GitHub asks for serial requests) ─
        // Search has its own 30-per-minute budget and returns no ETag, so this is always an
        // unconditional request.
        let review_kind = match reviews.as_ref() {
            Some(store) => {
                let response = github::poll_reviews(&client, store.token(), REVIEW_QUERY);
                let kind = response.result.kind();
                state.apply_reviews(response);
                Some(kind)
            }
            None => None,
        };

        let icon = state.icon();
        let tooltip = state.tooltip();

        // Update the UI before anything else here can block.
        if !emit(Update { icon, tooltip: tooltip.clone() }) {
            return; // UI has gone away
        }

        // ── Credential recovery, per axis ────────────────────────────────────
        // Each credential is renewed independently: a dead review token must never stop the
        // notifications half from working, and vice versa.
        let mut retry_now = false;

        if state.take_notifications_reauth()
            && may_retry(&mut last_reauth)
        {
            match tokens.reauthenticate() {
                Ok(()) => {
                    state.clear_notifications_etag();
                    retry_now = true;
                }
                Err(e) => logln!("re-authentication failed: {e}"),
            }
        }

        if state.take_reviews_reauth()
            && may_retry(&mut last_review_reauth)
        {
            match reviews.as_mut().map(ReviewTokenStore::reauthenticate) {
                Some(Ok(())) => retry_now = true,
                Some(Err(e)) => logln!("review credential renewal failed: {e}"),
                None => {}
            }
        }

        if retry_now {
            continue; // retry at once with the fresh credential
        }

        // A queued burst entry wins over normal pacing, so resolve the real delay before
        // logging it — otherwise the log would claim the steady-state interval during a burst,
        // which is exactly when someone is watching it.
        let delay = match burst.pop_front() {
            Some(delay) => delay,
            None => {
                skip_etag = false;
                state.next_delay()
            }
        };

        let conditional = if etag.is_some() { "conditional" } else { "unconditional" };
        let reviews_part = review_kind.map_or_else(String::new, |k| format!(" reviews→{k}"));
        logln!(
            "{conditional} poll → notifications→{notif_kind}{reviews_part} → {:?}/{:?} (next in {}s) [{}]",
            icon.notifications,
            icon.reviews,
            delay.as_secs(),
            // The tooltip is deliberately multi-line for the UI; keep the log one line per poll.
            tooltip.replace('\n', " · ")
        );

        match wake_rx.recv_timeout(delay) {
            Ok(Wake::Refresh) => {
                logln!("refresh requested — polling {} more times", REFRESH_BURST.len());
                burst = REFRESH_BURST.iter().copied().collect();
                skip_etag = true;
            }
            Err(RecvTimeoutError::Timeout) => {}
            // The sender lives in the UI, so a closed channel means the app is shutting down.
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Rate-limits credential renewal attempts, stamping the clock when one is allowed.
fn may_retry(last: &mut Option<Instant>) -> bool {
    if last.is_none_or(|at| at.elapsed() >= MIN_REAUTH_INTERVAL) {
        *last = Some(Instant::now());
        true
    } else {
        logln!("skipping credential renewal — attempted too recently");
        false
    }
}

// ─── Linux ────────────────────────────────────────────────────────────────────

/// How often the GTK main loop checks for pending updates. Cheap — it only touches a channel.
#[cfg(target_os = "linux")]
const UI_DRAIN_INTERVAL: Duration = Duration::from_secs(1);

#[cfg(target_os = "linux")]
pub fn start_notification_scheduler(
    app_asset_path: PathBuf,
    indicator: libappindicator::AppIndicator,
    icons: crate::icons::IconSet<String>,
    tokens: TokenStore,
    reviews: Option<ReviewTokenStore>,
    wake_rx: Receiver<Wake>,
) {
    // Use the glib that `gtk` itself was built against, so this timer is attached by the same
    // bindings that run the main loop.
    use gtk::glib;

    let (update_tx, update_rx) = std::sync::mpsc::channel::<Update>();
    spawn_poll_thread(app_asset_path, tokens, reviews, wake_rx, move |update| {
        update_tx.send(update).is_ok()
    });

    // No `Arc<Mutex<_>>` here: `timeout_add_local` requires only `FnMut + 'static`, and this
    // closure runs on the GTK main thread, so the indicator can simply be owned by it.
    // (`glib::idle_add_once` would look tidier but demands `Send`, which `AppIndicator` is not.)
    let mut indicator = indicator;
    let mut applied: Option<(bool, bool)> = None;

    glib::timeout_add_local(UI_DRAIN_INTERVAL, move || {
        while let Ok(update) = update_rx.try_recv() {
            // `Unknown` on either axis deliberately leaves that part of the picture alone — a
            // brief failure should change the words, not make the icon flap. Only a confirmed
            // answer moves the image, so an unknown axis falls back to whatever is on screen.
            let current = applied.unwrap_or((false, false));
            let wanted = (
                update.icon.notifications.as_confirmed().unwrap_or(current.0),
                update.icon.reviews.as_confirmed().unwrap_or(current.1),
            );

            if applied != Some(wanted) {
                indicator.set_icon(icons.get(wanted.0, wanted.1).as_str());
                applied = Some(wanted);
            }

            indicator.set_title(&update.tooltip);
            // A label is the only part of an indicator visible without hovering, so an
            // unknown state gets a marker the user can actually notice.
            let unsure = update.icon.notifications == crate::state::Presence::Unknown
                || update.icon.reviews == crate::state::Presence::Unknown;
            indicator.set_label(if unsure { "!" } else { "" }, "");
        }
        glib::ControlFlow::Continue
    });
}

// ─── Windows ──────────────────────────────────────────────────────────────────

/// The custom event type sent from the polling thread to the winit event loop.
#[cfg(target_os = "windows")]
pub enum TrayEvent {
    Update(Update),
}

#[cfg(target_os = "windows")]
pub fn start_notification_scheduler(
    app_asset_path: PathBuf,
    tokens: TokenStore,
    reviews: Option<ReviewTokenStore>,
    wake_rx: Receiver<Wake>,
    proxy: winit::event_loop::EventLoopProxy<TrayEvent>,
) {
    spawn_poll_thread(app_asset_path, tokens, reviews, wake_rx, move |update| {
        proxy.send_event(TrayEvent::Update(update)).is_ok()
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_encoding_escapes_query_syntax() {
        assert_eq!(percent_encode("is:pr -label:deps"), "is%3Apr%20-label%3Adeps");
        assert_eq!(percent_encode("a~b_c.d-e"), "a~b_c.d-e", "unreserved chars pass through");
        assert_eq!(percent_encode("@me"), "%40me");
    }

    /// The browser view must show exactly what the dot counts, so the URL has to carry every
    /// qualifier from the API query — including the bot exclusions.
    #[test]
    fn review_url_carries_the_same_filters_as_the_api_query() {
        let url = review_list_url();
        assert!(url.starts_with("https://github.com/pulls?q="), "got {url}");
        for qualifier in [
            "review-requested%3A%40me",
            "state%3Aopen",
            "archived%3Afalse",
            "-label%3Adependencies",
            "-author%3Aapp%2Fdependabot",
            "-author%3Aapp%2Frenovate",
            "sort%3Aupdated-desc",
        ] {
            assert!(url.contains(qualifier), "{qualifier} missing from {url}");
        }
        assert!(!url.contains(' '), "spaces must be encoded");
    }
}
