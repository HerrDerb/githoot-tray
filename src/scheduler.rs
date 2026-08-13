//! Notification scheduler.
//!
//! Both platforms share one polling loop (`run_poll_loop`) and differ only in how an update is
//! handed to the UI thread. Linux forwards through an mpsc channel drained by a GTK timer;
//! Windows forwards through `EventLoopProxy::send_event`.
//!
//! The loop waits on `Receiver::recv_timeout` rather than `thread::sleep`. That one substitution
//! buys three things at once: pacing that adapts to GitHub's `x-poll-interval`, an on-demand
//! refresh when the user opens their notifications, and a clean exit when the UI goes away.

use crate::access_token::TokenStore;
use crate::gh_cli::{self, ReviewToken};
use crate::github;
use crate::logln;
use crate::state::{IconState, PollState, MENU_BURST, REFRESH_BURST};
use std::collections::VecDeque;
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
///
/// Both cause an immediate unconditional poll. They differ in what happens *after* it, because they
/// mean different things: one says the answer is about to change on GitHub's side, the other says
/// the user wants to see the current answer now.
pub enum Wake {
    /// The user opened a GitHub page, so what they have read is about to change. Needs follow-up
    /// polls, since GitHub takes a moment to register that things were read.
    Refresh,
    /// The user asked for an update directly, by clicking the icon. Nothing on GitHub's side is
    /// changing, so one poll answers it and a burst would just be traffic.
    PollNow,
}

/// One rendering instruction for the UI thread.
#[derive(Debug)]
pub struct Update {
    pub icon: IconState,
    pub tooltip: String,
    /// Text for the reviews menu item, carrying the exact count. Worded in `state` rather than in
    /// each platform's UI code so the two cannot say it differently.
    pub reviews_label: String,
}

// ─── Shared polling core ──────────────────────────────────────────────────────

/// Spawns the poll thread.
///
/// `emit` returns `false` once the UI is gone, which ends the loop. A panic in here used to
/// freeze the icon at its last value with no trace; now it at least says so in the log.
fn spawn_poll_thread(
    tokens: TokenStore,
    reviews: Option<ReviewToken>,
    reviews_off: Option<String>,
    wake_rx: Receiver<Wake>,
    emit: impl FnMut(Update) -> bool + Send + 'static,
) {
    std::thread::spawn(move || {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            run_poll_loop(tokens, reviews, reviews_off, wake_rx, emit)
        }));
        if outcome.is_err() {
            logln!("poll thread panicked — notification updates have stopped");
        }
    });
}

fn run_poll_loop(
    mut tokens: TokenStore,
    mut reviews: Option<ReviewToken>,
    reviews_off: Option<String>,
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
    // Carry the startup verdict into the tooltip, so "no dot" always has a stated reason.
    if let Some(reason) = reviews_off {
        state.disable_reviews(reason);
    }
    let mut burst: VecDeque<Duration> = VecDeque::new();
    let mut last_reauth: Option<Instant> = None;
    let mut last_review_reauth: Option<Instant> = None;

    // While a refresh burst is draining we send no `If-None-Match`. A conditional request can
    // legitimately answer 304 from a cached view, which would leave a just-read icon stuck on
    // "unread" — the exact symptom the burst exists to cure.
    let mut skip_etag = false;

    loop {
        state.begin_cycle();

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

        // ── Scope check, once GitHub has actually told us ─────────────────────
        // A token without `repo` cannot see private repositories, and search does not complain: it
        // answers 200 with `total_count: 0`. That is the one unacceptable answer, a confident zero
        // that is wrong, so the axis is turned off with an actionable reason instead. Only ever
        // reached with a credential in hand, and it disables that credential, so it fires once.
        if reviews.is_some()
            && state.review_scopes().is_some_and(gh_cli::lacks_required_scope)
        {
            let scopes = state.review_scopes().unwrap_or_default().to_string();
            let why = gh_cli::Unavailable::MissingScope { scopes };
            logln!("review dot disabled: {}", why.message().replace('\n', " "));
            state.disable_reviews(why.short());
            reviews = None;
        }

        let icon = state.icon();
        let tooltip = state.tooltip();
        let reviews_label = state.reviews_menu_label();

        // Update the UI before anything else here can block.
        if !emit(Update { icon, tooltip: tooltip.clone(), reviews_label }) {
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
            match reviews.as_mut().map(ReviewToken::refresh) {
                // gh had rotated the token underneath us, so the new one is worth trying at once.
                Some(Ok(true)) => retry_now = true,
                // Same value back, so retrying now would only earn the same 401. Wait for the
                // user to fix gh, and say so where they will see it.
                Some(Ok(false)) => {
                    logln!("gh returned the same token GitHub just rejected — run gh auth login");
                    state.disable_reviews("Review dot off: run gh auth login".to_string());
                    reviews = None;
                }
                Some(Err(why)) => {
                    logln!("review credential renewal failed: {}", why.message().replace('\n', " "));
                    state.disable_reviews(why.short());
                    reviews = None;
                }
                None => {}
            }
        }

        if retry_now {
            continue; // retry at once with the fresh credential
        }

        // A queued burst entry wins over normal pacing, so resolve the real delay before
        // logging it — otherwise the log would claim the steady-state interval during a burst,
        // which is exactly when someone is watching it.
        let delay = match next_pace(&mut burst, state.rate_limited(), state.next_delay()) {
            Pace::Burst(delay) => delay,
            // Leaving the burst also ends the unconditional streak it was running.
            Pace::Steady(delay) => {
                skip_etag = false;
                delay
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
            // Either way the next cycle starts at once, and without an `If-None-Match`: a
            // conditional request may legitimately answer 304 from a cached view, which would leave
            // a user who just asked for an update staring at the icon they were trying to change.
            Ok(wake) => {
                skip_etag = true;
                match wake {
                    Wake::Refresh => {
                        logln!("refresh requested — polling {} more times", REFRESH_BURST.len());
                        burst = REFRESH_BURST.iter().copied().collect();
                    }
                    // A short even burst rather than the widening one: one sample taken within a
                    // second of the click is thin, and anything that changes a moment later would
                    // otherwise stay invisible for the rest of the minute.
                    Wake::PollNow => {
                        logln!(
                            "menu opened — polling now and {} more times, 5s apart",
                            MENU_BURST.len()
                        );
                        burst = MENU_BURST.iter().copied().collect();
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            // The sender lives in the UI, so a closed channel means the app is shutting down.
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Which pace the next cycle runs at.
#[derive(Debug, PartialEq, Eq)]
enum Pace {
    /// A queued burst entry, deliberately faster than the floor.
    Burst(Duration),
    /// Normal pacing, whatever `PollState` worked out.
    Steady(Duration),
}

/// Picks the wait before the next cycle.
///
/// A burst beats normal pacing, but never a wait GitHub explicitly demanded. Retrying inside a
/// `retry-after` window is how a short secondary limit becomes a long one, and a burst is never
/// worth that.
///
/// The remaining burst entries are dropped rather than deferred. A burst exists to catch a change
/// the user just caused; once a rate-limit window has elapsed, that moment has passed and the normal
/// cadence is the right thing to return to.
///
/// Pure so this can be tested without a network, which is the point: the previous version read
/// GitHub's instruction, stored it, and then silently discarded it whenever a burst was in flight,
/// and nothing in the suite could see that happen.
fn next_pace(burst: &mut VecDeque<Duration>, rate_limited: bool, steady: Duration) -> Pace {
    if rate_limited {
        burst.clear();
        return Pace::Steady(steady);
    }

    match burst.pop_front() {
        Some(delay) => Pace::Burst(delay),
        None => Pace::Steady(steady),
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

/// The menu entries the poll loop keeps up to date: their text, and whether they are shown at all.
///
/// A struct rather than two more parameters, because the list was going to keep growing.
#[cfg(target_os = "linux")]
pub struct MenuItems {
    pub notifications: gtk::MenuItem,
    pub reviews: gtk::MenuItem,
}

#[cfg(target_os = "linux")]
pub fn start_notification_scheduler(
    indicator: libappindicator::AppIndicator,
    icons: crate::icons::IconSet<String>,
    menu_items: MenuItems,
    tokens: TokenStore,
    reviews: Option<ReviewToken>,
    reviews_off: Option<String>,
    wake_rx: Receiver<Wake>,
) {
    // Use the glib that `gtk` itself was built against, so this timer is attached by the same
    // bindings that run the main loop.
    use gtk::glib;
    // `set_label` lives on an extension trait, and the prelude is the documented way in.
    use gtk::prelude::*;

    let (update_tx, update_rx) = std::sync::mpsc::channel::<Update>();
    spawn_poll_thread(tokens, reviews, reviews_off, wake_rx, move |update| {
        update_tx.send(update).is_ok()
    });

    // No `Arc<Mutex<_>>` here: `timeout_add_local` requires only `FnMut + 'static`, and this
    // closure runs on the GTK main thread, so the indicator can simply be owned by it.
    // (`glib::idle_add_once` would look tidier but demands `Send`, which `AppIndicator` is not.)
    let mut indicator = indicator;
    let mut applied: Option<(bool, bool)> = None;
    let mut applied_label: Option<String> = None;

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

                // An entry that opens an empty list is a dead end, so it is hidden. `wanted` says
                // it directly: the icon carries a signal exactly when that entry has somewhere to
                // go. Its treatment of `Unknown` carries over too, so a failed poll leaves the menu
                // as it was rather than hiding an entry we simply could not ask about.
                //
                // GTK has per-item visibility, so this is a flag rather than the remove-and-append
                // dance the Windows side needs. `show_all` is called once during setup and never
                // again, so nothing undoes these.
                menu_items.notifications.set_visible(wanted.0);
                menu_items.reviews.set_visible(wanted.1);

                applied = Some(wanted);
            }

            // Only on change: relabelling a menu item is cheap, but some panels rebuild the whole
            // menu when an item changes, which would fight a user who has it open.
            if applied_label.as_deref() != Some(update.reviews_label.as_str()) {
                menu_items.reviews.set_label(&update.reviews_label);
                applied_label = Some(update.reviews_label.clone());
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
    tokens: TokenStore,
    reviews: Option<ReviewToken>,
    reviews_off: Option<String>,
    wake_rx: Receiver<Wake>,
    proxy: winit::event_loop::EventLoopProxy<TrayEvent>,
) {
    spawn_poll_thread(tokens, reviews, reviews_off, wake_rx, move |update| {
        proxy.send_event(TrayEvent::Update(update)).is_ok()
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const STEADY: Duration = Duration::from_secs(60);

    fn burst_of(secs: &[u64]) -> VecDeque<Duration> {
        secs.iter().map(|s| Duration::from_secs(*s)).collect()
    }

    #[test]
    fn a_queued_burst_drains_in_order_then_returns_to_normal_pacing() {
        let mut burst = burst_of(&[5, 5, 5]);

        for _ in 0..3 {
            assert_eq!(next_pace(&mut burst, false, STEADY), Pace::Burst(Duration::from_secs(5)));
        }
        assert_eq!(next_pace(&mut burst, false, STEADY), Pace::Steady(STEADY));
    }

    #[test]
    fn the_widening_burst_keeps_its_order() {
        // Order matters for REFRESH_BURST specifically, since its whole point is widening gaps.
        let mut burst: VecDeque<Duration> = REFRESH_BURST.iter().copied().collect();
        for expected in REFRESH_BURST {
            assert_eq!(next_pace(&mut burst, false, STEADY), Pace::Burst(expected));
        }
        assert_eq!(next_pace(&mut burst, false, STEADY), Pace::Steady(STEADY));
    }

    #[test]
    fn an_empty_burst_is_normal_pacing() {
        let mut burst = VecDeque::new();
        assert_eq!(next_pace(&mut burst, false, STEADY), Pace::Steady(STEADY));
    }

    /// The regression this exists for. A burst used to win unconditionally, so a `retry-after` was
    /// read, stored, and then thrown away: we would poll again in 5s having just been told to wait
    /// a minute, which is how a short secondary limit turns into a long one.
    #[test]
    fn a_demanded_wait_beats_a_burst() {
        let mut burst = burst_of(&[5, 5, 5]);
        let demanded = Duration::from_secs(600);

        assert_eq!(next_pace(&mut burst, true, demanded), Pace::Steady(demanded));
    }

    /// Abandoned, not merely deferred by one cycle. A burst is chasing a change the user just
    /// caused; ten minutes later that moment is gone and the burst would be pure traffic.
    #[test]
    fn a_demanded_wait_discards_the_rest_of_the_burst() {
        let mut burst = burst_of(&[5, 5, 5]);

        next_pace(&mut burst, true, Duration::from_secs(600));
        assert!(burst.is_empty(), "the burst must not resume after the limit lifts");

        // …and the cycle after really is back to normal pacing.
        assert_eq!(next_pace(&mut burst, false, STEADY), Pace::Steady(STEADY));
    }

    #[test]
    fn a_demanded_wait_with_no_burst_changes_nothing() {
        let mut burst = VecDeque::new();
        let demanded = Duration::from_secs(90);
        assert_eq!(next_pace(&mut burst, true, demanded), Pace::Steady(demanded));
    }

    /// The two bursts are deliberately different shapes, and a copy-paste that merged them would
    /// silently undo the reasoning in both.
    #[test]
    fn the_two_bursts_stay_distinct() {
        assert!(
            MENU_BURST.iter().all(|d| *d == MENU_BURST[0]),
            "the menu burst is evenly spaced: there is no server-side lag to wait out"
        );
        assert!(
            REFRESH_BURST.windows(2).all(|w| w[1] > w[0]),
            "the page-open burst widens: it is waiting out GitHub registering a read"
        );
        assert_eq!(
            MENU_BURST.iter().sum::<Duration>(),
            Duration::from_secs(15),
            "the menu burst covers 15s, as documented"
        );
    }

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
