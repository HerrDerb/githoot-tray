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
use crate::github;
use crate::github_app::{AuthError, PrStatus, PrTokenStore, PR_NOT_INSTALLED};
use crate::update::{Available, RestartPlan};
use crate::logln;
use crate::state::{IconState, PollState, PrAxis, MENU_BURST, REFRESH_BURST};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

/// Floor between re-authentication attempts, so a credential GitHub keeps rejecting cannot
/// produce a storm of authorization prompts.
const MIN_REAUTH_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// How often to ask GitHub whether a newer release exists.
///
/// Once a day. The poll loop already wakes at least every 15 minutes, so this needs no timer of its
/// own — just an elapsed-time gate, the same shape as `may_retry`. Releases happen at human pace and
/// the unauthenticated rate limit is 60 requests an hour per IP, so anything faster would be spending
/// budget to learn nothing. The first check runs immediately at startup, which is when someone who has
/// just launched the app is most likely to be looking at it.
const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// Guards against two overlapping installs.
///
/// A `static` rather than a loop local because the install runs on a detached thread that outlives the
/// loop iteration that spawned it, so the flag has to live somewhere both can see. Clicking the menu
/// entry twice is the case this exists for.
static UPDATE_IN_FLIGHT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

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

/// Search query for the user's own pull requests that are approved and passing checks.
///
/// GitHub's search API has no single "mergeable" qualifier, so this is an accepted approximation:
/// it can read wrong for branch-protection setups needing more than one approval, required
/// reviewers by name, or required status checks not reflected in the combined commit status.
const MERGE_QUERY: &str = "is:pr author:@me review:approved status:success state:open \
                           draft:false archived:false";

/// Search query for the user's own pull requests where a reviewer requested changes.
const CHANGES_QUERY: &str = "is:pr author:@me review:changes_requested state:open archived:false";

/// Sort order for the browser view only. Meaningless to the API (which reads `total_count`), but
/// it is what makes the web page useful to look at.
const REVIEW_UI_SORT: &str = "sort:updated-desc";

/// The search query behind `axis`'s dot. `PrAxis` itself doesn't know about queries — issuing
/// HTTP requests is this module's job, not `state`'s — so the mapping lives here.
fn pr_query(axis: PrAxis) -> &'static str {
    match axis {
        PrAxis::ReviewRequested => REVIEW_QUERY,
        PrAxis::ReadyToMerge => MERGE_QUERY,
        PrAxis::ChangesRequested => CHANGES_QUERY,
    }
}

/// Issues `axis`'s poll.
///
/// Two axes are a `total_count` read off the Search API. `ChangesRequested` is not, because the query
/// alone cannot tell "still on me" from "handed back to the reviewer" — see
/// `github::poll_changes_requested`. Both take the same query string; they differ only in which endpoint
/// answers it and how much of the answer has to be read.
fn poll_pr(client: &reqwest::blocking::Client, token: &str, axis: PrAxis) -> github::PollResponse {
    let query = pr_query(axis);
    match axis {
        PrAxis::ChangesRequested => github::poll_changes_requested(client, token, query),
        PrAxis::ReviewRequested | PrAxis::ReadyToMerge => github::poll_reviews(client, token, query),
    }
}

/// The GitHub page listing exactly what `axis`'s dot is counting.
///
/// Built from the same query the search itself uses, so the page can never disagree with the
/// icon. A hand-written URL drifts the moment a query changes — and a dot claiming 3 next to a
/// page showing 5 is worse than no dot at all.
pub fn pr_list_url(axis: PrAxis) -> String {
    format!(
        "https://github.com/pulls?q={}",
        percent_encode(&format!("{} {REVIEW_UI_SORT}", pr_query(axis)))
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
/// The first two cause an immediate unconditional poll, differing only in what happens *after* it,
/// because they mean different things: one says the answer is about to change on GitHub's side, the
/// other says the user wants to see the current answer now. `Authenticate` is a different kind of
/// thing altogether — it asks the loop to do work first, and there is no point polling until it has.
pub enum Wake {
    /// The user opened a GitHub page, so what they have read is about to change. Needs follow-up
    /// polls, since GitHub takes a moment to register that things were read.
    Refresh,
    /// The user asked for an update directly, by clicking the icon. Nothing on GitHub's side is
    /// changing, so one poll answers it and a burst would just be traffic.
    PollNow,
    /// The user picked the Install update item.
    ///
    /// Unlike `Authenticate`, the work does **not** happen on the poll thread: downloading several
    /// megabytes would stall notification polling for as long as it takes, so this spawns a dedicated
    /// thread and returns immediately. See `run_poll_loop`.
    UpdateNow,
    /// The user picked the Authenticate item, asking for the PR-status device flow to run now.
    ///
    /// Handled on the poll thread rather than in the click handler because that is where the
    /// credential lives, and because the flow blocks for as long as the user takes — up to GitHub's
    /// 15-minute device-code lifetime. Running it on the UI thread would freeze the tray for all of
    /// it, which on Linux means the whole GTK main loop.
    Authenticate,
    /// The user picked the Settings item, which has already opened `config.txt` in whatever handles it.
    ///
    /// Routed through here rather than started in the click handler because the watcher needs the same
    /// `restart` closure the update thread uses, and that lives on this side. Handling it only *spawns*
    /// a thread, so polling is not delayed by the fifteen minutes the watch may run for.
    SettingsOpened,
}

/// One rendering instruction for the UI thread.
#[derive(Debug)]
pub struct Update {
    pub icon: IconState,
    pub tooltip: String,
    /// Text for each PR axis's menu item, carrying the exact count, indexed by `PrAxis::index`.
    /// Worded in `state` rather than in each platform's UI code so the two cannot say it
    /// differently.
    pub pr_labels: [String; 3],
    /// Text for the install-update entry, carrying the version. `None` when no update is available, in
    /// which case the entry is hidden and its label is irrelevant.
    pub update_label: Option<String>,
}

/// Everything the poll loop needs that is not a channel or a UI handle.
///
/// Grouped because both platforms' entry points were growing a parallel list of the same four values,
/// and the Linux one had reached nine parameters — at which point the order is the only thing telling
/// two `bool`s apart. Naming them at the call site is worth a struct.
pub struct PollInputs {
    pub tokens: Option<TokenStore>,
    pub pr: PrStatus,
    /// Held for the whole run because `Wake::Authenticate` can arrive at any time and
    /// `PrTokenStore::authenticate` needs somewhere to save what it obtains.
    pub app_asset_path: PathBuf,
    /// Whether to look for newer releases at all. See `config::Config::update_check`.
    pub update_check: bool,
    /// Which PR signals the user wants, indexed by `PrAxis::index`.
    ///
    /// The **configuration only**, never ANDed with whether a credential exists. See
    /// `PollState::new` for what folding those two together would break.
    pub pr_enabled: [bool; 3],
}

// ─── Shared polling core ──────────────────────────────────────────────────────

/// Spawns the poll thread.
///
/// `emit` returns `false` once the UI is gone, which ends the loop. A panic in here used to
/// freeze the icon at its last value with no trace; now it at least says so in the log.
fn spawn_poll_thread(
    inputs: PollInputs,
    wake_rx: Receiver<Wake>,
    emit: impl FnMut(Update) -> bool + Send + 'static,
    restart: impl Fn(RestartPlan) + Send + Clone + 'static,
) {
    std::thread::spawn(move || {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            run_poll_loop(inputs, wake_rx, emit, restart)
        }));
        if outcome.is_err() {
            logln!("poll thread panicked — notification updates have stopped");
        }
    });
}

/// `app_asset_path` is held for the whole run because `Wake::Authenticate` can arrive at any time,
/// and `PrTokenStore::authenticate` needs somewhere to save what it obtains.
fn run_poll_loop(
    inputs: PollInputs,
    wake_rx: Receiver<Wake>,
    mut emit: impl FnMut(Update) -> bool,
    restart: impl Fn(RestartPlan) + Send + Clone + 'static,
) {
    let PollInputs {
        mut tokens,
        pr,
        app_asset_path,
        update_check: update_check_enabled,
        pr_enabled,
    } = inputs;
    let client = match github::build_client() {
        Ok(client) => client,
        Err(e) => {
            logln!("fatal: could not build HTTP client: {e}");
            return;
        }
    };

    let (mut pr, pr_off, needs_auth) = match pr {
        PrStatus::Ready(store) => (Some(store), None, false),
        PrStatus::NeedsAuth => (None, None, true),
        PrStatus::Off(reason) => (None, Some(reason), false),
    };

    // The config, not `[pr.is_some(); 3]`. Whether a credential exists is said by the two calls
    // below; putting it here as well would make `require_pr_auth` a no-op on the very path that
    // exists to obtain one. See `PollState::new`.
    let mut state = PollState::new(tokens.is_some(), pr_enabled);
    // Both branches mean "no PR dots", and both are deliberately said differently: one has a menu
    // item waiting to be clicked, the other has a reason clicking cannot address. The three axes
    // share one credential, so whichever it is applies to all three at once.
    if needs_auth {
        state.require_pr_auth();
    }
    if let Some(reason) = pr_off {
        for axis in PrAxis::ALL {
            state.disable_pr(axis, reason.clone());
        }
    }
    let mut burst: VecDeque<Duration> = VecDeque::new();
    let mut last_reauth: Option<Instant> = None;
    let mut last_pr_reauth: Option<Instant> = None;
    // `None` means "never checked", which is what makes the first check happen immediately.
    let mut last_update_check: Option<Instant> = None;
    // The release the last check found, held so the menu click has something to install without
    // re-asking GitHub. Cleared when a check finds nothing newer.
    let mut pending_update: Option<Available> = None;

    // While a refresh burst is draining we send no `If-None-Match`. A conditional request can
    // legitimately answer 304 from a cached view, which would leave a just-read icon stuck on
    // "unread" — the exact symptom the burst exists to cure.
    let mut skip_etag = false;

    loop {
        state.begin_cycle();

        // ── Notifications (skipped entirely when the feature is off) ──────────
        let etag = if skip_etag {
            None
        } else {
            state.notifications_etag().map(str::to_string)
        };
        let notif_kind = match tokens.as_ref() {
            Some(store) => {
                let response = github::poll_notifications(&client, store.token(), etag.as_deref());
                let kind = response.result.kind();
                state.apply_notifications(response);
                Some(kind)
            }
            None => None,
        };

        // ── PR axes (serial, not concurrent: GitHub asks for serial requests) ─
        // Search has its own 30-per-minute budget and returns no ETag, so this is always an
        // unconditional request. All three axes share one credential (see `PrTokenStore`), whose
        // access is granted by installing the GitHub App rather than by a scope, so there is no
        // per-poll scope check here: whether it can see anything at all is checked once, at
        // startup, in `main.rs`.
        let mut pr_kinds: Vec<(PrAxis, &'static str)> = Vec::new();
        if let Some(store) = pr.as_ref() {
            for axis in PrAxis::ALL {
                // Skipped before the request, not after: `apply_pr` would discard the answer for an
                // axis that is not in play, and Search has its own 30-per-minute budget, so issuing
                // it would be pure cost. Also keeps the log line below free of axes whose result was
                // thrown away.
                //
                // An `if` rather than `.filter()` on the iterator: the adaptor would hold `&state`
                // across a body that needs `&mut state` for `apply_pr`.
                if !state.pr_in_play(axis) {
                    continue;
                }
                let response = poll_pr(&client, store.token(), axis);
                pr_kinds.push((axis, response.result.kind()));
                state.apply_pr(axis, response);
            }
        }

        let icon = state.icon();
        let tooltip = state.tooltip();
        let pr_labels = std::array::from_fn(|i| {
            let axis = PrAxis::ALL[i];
            state.pr_menu_label(axis)
        });

        // Update the UI before anything else here can block.
        let update_label = state.update_menu_label();
        if !emit(Update { icon, tooltip: tooltip.clone(), pr_labels, update_label }) {
            return; // UI has gone away
        }

        // ── Is there a newer release? ────────────────────────────────────────
        // Cheap: one unauthenticated GET, gated to once a day, so it runs inline rather than needing a
        // thread. Placed after `emit` deliberately — the icon is already showing this cycle's answer
        // before this can add anything to it, so a slow or hanging check delays only itself.
        //
        // Failures are logged and dropped, never surfaced. Not being able to ask whether an update
        // exists is not something the user can act on, and a dialog for it would be noise.
        if update_check_enabled
            && last_update_check.is_none_or(|at| at.elapsed() >= UPDATE_CHECK_INTERVAL)
        {
            last_update_check = Some(Instant::now());
            match crate::version::Version::current() {
                Some(current) => match crate::update::check(&client, current) {
                    Ok(Some(available)) => {
                        logln!(
                            "update available: {} (installed {current})",
                            available.version
                        );
                        state.set_update_available(Some(available.version.to_string()));
                        pending_update = Some(available);
                    }
                    Ok(None) => {
                        // Clears the arrow, which matters right after an install: the new binary is
                        // current, so this is what takes the arrow back down.
                        state.set_update_available(None);
                        pending_update = None;
                    }
                    Err(e) => logln!("update check failed: {e}"),
                },
                // Unreachable from a normal build; see `Version::current`.
                None => logln!("update check skipped: this build's version is unparseable"),
            }
        }

        // ── Credential recovery, per axis ────────────────────────────────────
        // Each credential is renewed independently: a dead review token must never stop the
        // notifications half from working, and vice versa.
        let mut retry_now = false;

        if state.take_notifications_reauth()
            && may_retry(&mut last_reauth)
        {
            match tokens.as_mut().map(TokenStore::reauthenticate) {
                Some(Ok(())) => {
                    state.clear_notifications_etag();
                    retry_now = true;
                }
                Some(Err(e)) => logln!("re-authentication failed: {e}"),
                None => {}
            }
        }

        // All three PR axes share one credential, so a rejection on any of them — or the
        // credential simply approaching its known expiry, for a GitHub App that has token expiry
        // turned on (see `PrTokenStore::needs_refresh`) — triggers one shared renewal rather than
        // three independent ones. `take_pr_reauth` is called for every axis unconditionally
        // (not through a short-circuiting `.any()`) so each axis's flag is actually consumed.
        let mut pr_needs_reauth = false;
        for axis in PrAxis::ALL {
            if state.take_pr_reauth(axis) {
                pr_needs_reauth = true;
            }
        }
        if pr.as_ref().is_some_and(PrTokenStore::needs_refresh) {
            pr_needs_reauth = true;
        }

        if pr_needs_reauth && may_retry(&mut last_pr_reauth) {
            match pr.as_mut().map(PrTokenStore::reauthenticate) {
                // The silent refresh grant worked, so the new credential is worth retrying with at
                // once. Nothing user-visible happened, which is the point.
                Some(Ok(())) => retry_now = true,
                // The grant cannot help: no refresh token, or GitHub rejected the one we have. Hand
                // it to the user rather than opening a browser unannounced — the exclamation goes up
                // and the Authenticate item appears, and `Wake::Authenticate` picks it up from there.
                Some(Err(AuthError::AuthorizationRequired)) => {
                    logln!("PR status needs authorization — waiting for the menu");
                    state.require_pr_auth();
                    pr = None;
                }
                // Anything else is a failure to *ask*, not an answer. Most often the network is
                // down. The credential is kept and the axes are left alone, so this retries on the
                // next cycle instead of costing the user a click it did not need. `MIN_REAUTH_INTERVAL`
                // is what stops that becoming a hot loop.
                Some(Err(e)) => logln!("PR credential renewal could not be attempted ({e}) — will retry"),
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

        // Each axis contributes its own segment, or none at all when it is unconfigured — unlike
        // the old always-on notifications axis, an empty poll (every feature off) is now possible
        // and must not render as a mangled arrow-to-nowhere.
        let mut polled = Vec::new();
        if let Some(k) = notif_kind {
            let conditional = if etag.is_some() { "conditional" } else { "unconditional" };
            polled.push(format!("{conditional} notifications→{k}"));
        }
        for (axis, kind) in &pr_kinds {
            polled.push(format!("{axis:?}→{kind}"));
        }
        let polled = if polled.is_empty() { "nothing configured".to_string() } else { polled.join(", ") };

        logln!(
            "poll → {polled} → {:?}/{:?}/{:?}/{:?} (next in {}s) [{}]",
            icon.notifications,
            icon.review_requested,
            icon.ready_to_merge,
            icon.changes_requested,
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
                    // Deliberately *not* on this thread, unlike `Authenticate`. The download is
                    // megabytes and the timeout is minutes, and polling has to keep working
                    // throughout — a frozen icon during an install would look like a crash.
                    Wake::UpdateNow => {
                        if let Some(available) = pending_update.clone() {
                            // `swap` rather than load-then-store: two menu clicks in quick succession
                            // must not both get past this.
                            if UPDATE_IN_FLIGHT.swap(true, Ordering::SeqCst) {
                                logln!("an update install is already in progress");
                            } else {
                                let restart = restart.clone();
                                std::thread::spawn(move || {
                                    match crate::update::install(&available) {
                                        // Hands the plan to the UI thread, which is the only one that
                                        // can take the tray down cleanly before the process ends.
                                        Ok(plan) => restart(plan),
                                        Err(crate::update::UpdateError::Declined) => {
                                            logln!("update declined by the user");
                                        }
                                        Err(e) => crate::dialog::report(
                                            "git-system-tray: update failed",
                                            &format!(
                                                "The update was not installed and the current \
                                                 version is untouched.\n\n{e}"
                                            ),
                                        ),
                                    }
                                    UPDATE_IN_FLIGHT.store(false, Ordering::SeqCst);
                                });
                            }
                        } else {
                            logln!("install requested, but no update is pending");
                        }
                    }
                    // Opening settings says nothing about GitHub, so this is the one wake that does not
                    // want a poll at all — hence `skip_etag` going back down. All it does is arm the
                    // watcher, which then lives on its own thread.
                    Wake::SettingsOpened => {
                        skip_etag = false;
                        crate::settings_watch::spawn(
                            crate::config::config_path(&app_asset_path),
                            restart.clone(),
                        );
                    }
                    // Blocks this thread for as long as the user takes, which is exactly why it is
                    // here and not in the click handler: the tray stays responsive throughout, and
                    // the only cost is that polling pauses while a credential is being obtained —
                    // which it could not usefully do anyway.
                    Wake::Authenticate => match PrTokenStore::authenticate(&app_asset_path) {
                        Ok(store) => match store.installation_count() {
                            // Authorized, but the App is installed nowhere, so search would see no
                            // repositories at all. Another click cannot fix that, so the exclamation
                            // comes down and a stated reason replaces it. Same check startup does.
                            Ok(0) => {
                                logln!("{PR_NOT_INSTALLED}");
                                state.clear_pr_auth();
                                for axis in PrAxis::ALL {
                                    state.disable_pr(axis, PR_NOT_INSTALLED.to_string());
                                }
                                pr = None;
                            }
                            // Could not confirm installations. Start polling anyway rather than
                            // refuse over a question we could not even ask — the same reasoning
                            // startup uses.
                            outcome => {
                                if let Err(e) = outcome {
                                    logln!("could not confirm installations ({e}) — continuing anyway");
                                }
                                logln!("PR status authorized");
                                state.clear_pr_auth();
                                pr = Some(store);
                            }
                        },
                        // Denied, expired, or the network went away mid-flow. The state is left as
                        // it was, so the item is still on the menu to try again.
                        Err(e) => logln!("authorization failed: {e}"),
                    },
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
    pub ready_to_merge: gtk::MenuItem,
    pub changes_requested: gtk::MenuItem,
    /// Shown only while PR status is waiting to be authorized. It is the counterpart of the four
    /// above: they are hidden when there is nothing to open, this one is hidden when there is
    /// nothing to authorize.
    pub authenticate: gtk::MenuItem,
    /// Shown only while a newer release exists. Its label carries the version, so it is relabelled as
    /// well as shown and hidden.
    pub update: gtk::MenuItem,
}

#[cfg(target_os = "linux")]
impl MenuItems {
    /// The three PR-axis items, in `PrAxis::ALL` order — so per-axis code can loop instead of
    /// hand-repeating itself three times.
    fn pr_items(&self) -> [&gtk::MenuItem; 3] {
        [&self.reviews, &self.ready_to_merge, &self.changes_requested]
    }
}

#[cfg(target_os = "linux")]
pub fn start_notification_scheduler(
    indicator: libappindicator::AppIndicator,
    icons: crate::icons::IconSet<String>,
    menu_items: MenuItems,
    inputs: PollInputs,
    wake_rx: Receiver<Wake>,
    restart_tx: std::sync::mpsc::Sender<RestartPlan>,
) {
    // Use the glib that `gtk` itself was built against, so this timer is attached by the same
    // bindings that run the main loop.
    use gtk::glib;
    // `set_label` lives on an extension trait, and the prelude is the documented way in.
    use gtk::prelude::*;

    let (update_tx, update_rx) = std::sync::mpsc::channel::<Update>();
    // A second channel rather than a variant on `Update`: a restart is a one-off instruction, not part
    // of the per-poll rendering state, and mixing them would mean every poll carried an `Option` that is
    // `None` for the whole life of the process bar once.
    spawn_poll_thread(
        inputs,
        wake_rx,
        move |update| update_tx.send(update).is_ok(),
        move |plan| {
            // Two steps, because this runs on the update thread and GTK is main-thread-only. The plan
            // goes down the channel `main` reads after `gtk::main()` returns, and then the main loop is
            // asked to return at all — without that second half the plan would sit unread forever.
            //
            // `idle_add` rather than `idle_add_local`: this is a cross-thread post, so the closure must
            // be `Send`. It captures nothing, which is what makes that hold.
            let _ = restart_tx.send(plan);
            glib::idle_add(|| {
                gtk::main_quit();
                glib::ControlFlow::Break
            });
        },
    );

    // No `Arc<Mutex<_>>` here: `timeout_add_local` requires only `FnMut + 'static`, and this
    // closure runs on the GTK main thread, so the indicator can simply be owned by it.
    // (`glib::idle_add_once` would look tidier but demands `Send`, which `AppIndicator` is not.)
    let mut indicator = indicator;
    // Index 0 is notifications; indices 1..4 are the PR axes at `PrAxis::index() + 1` — one array
    // instead of four separate bools so the loop below does not have to hand-repeat itself.
    let mut applied: Option<[bool; 4]> = None;
    let mut applied_labels: [Option<String>; 3] = [None, None, None];
    // Tracked separately from `applied` because it is not one of the four signals but a replacement
    // for all of them, and because it drives a menu item the four do not.
    let mut applied_needs_auth: Option<bool> = None;
    // Likewise separate: an available update is a fifth independent signal drawn in a corner the other
    // four never touch, and it drives its own menu entry.
    let mut applied_update: Option<bool> = None;
    let mut applied_update_label: Option<String> = None;

    glib::timeout_add_local(UI_DRAIN_INTERVAL, move || {
        while let Ok(update) = update_rx.try_recv() {
            // `Unknown` on any axis deliberately leaves that part of the picture alone — a brief
            // failure should change the words, not make the icon flap. Only a confirmed answer
            // moves the image, so an unknown axis falls back to whatever is on screen.
            let current = applied.unwrap_or([false; 4]);
            let wanted = [
                update.icon.notifications.as_confirmed().unwrap_or(current[0]),
                update.icon.review_requested.as_confirmed().unwrap_or(current[1]),
                update.icon.ready_to_merge.as_confirmed().unwrap_or(current[2]),
                update.icon.changes_requested.as_confirmed().unwrap_or(current[3]),
            ];
            let needs_auth = update.icon.needs_auth;
            let update_available = update.icon.update_available;

            // Checked before the four signals, because it overrides them: with no credential there
            // is nothing to draw a dot from. `wanted` is still computed and stored above, so the
            // moment authorization succeeds the icon can go straight back to the right variant
            // without waiting for a signal to change.
            if applied_needs_auth != Some(needs_auth) {
                menu_items.authenticate.set_visible(needs_auth);
                applied_needs_auth = Some(needs_auth);
                // Force the icon/menu block below to run: the variant it should show has just
                // changed even if none of the four signals did.
                applied = None;
            }

            if applied_update != Some(update_available) {
                menu_items.update.set_visible(update_available);
                applied_update = Some(update_available);
                // Same reason as above: the arrow is part of the variant, so the image has to be
                // re-applied even though none of the four signals moved.
                applied = None;
            }

            if applied != Some(wanted) {
                // The arrow rides along with both branches, because it is orthogonal to whether a
                // credential is missing: an update is worth showing either way, and the two marks sit
                // in opposite corners.
                if needs_auth {
                    indicator.set_icon(icons.needs_auth(update_available).as_str());
                } else {
                    indicator.set_icon(
                        icons
                            .get(wanted[0], wanted[1], wanted[2], wanted[3], update_available)
                            .as_str(),
                    );
                }

                // An entry that opens an empty list is a dead end, so it is hidden. `wanted` says
                // it directly: the icon carries a signal exactly when that entry has somewhere to
                // go. Its treatment of `Unknown` carries over too, so a failed poll leaves the menu
                // as it was rather than hiding an entry we simply could not ask about.
                //
                // The three PR entries are hidden outright while waiting to authorize, because none
                // of them can have anything behind them. Notifications are *not*: that is a separate
                // credential which may be working perfectly, and hiding a working entry because a
                // different one needs attention would take away a feature that still functions. The
                // icon cannot say both things at once, but the menu can.
                //
                // GTK has per-item visibility, so this is a flag rather than the remove-and-append
                // dance the Windows side needs. `show_all` is called once during setup and never
                // again, so nothing undoes these.
                menu_items.notifications.set_visible(wanted[0]);
                for (item, &visible) in menu_items.pr_items().into_iter().zip(&wanted[1..]) {
                    item.set_visible(!needs_auth && visible);
                }

                applied = Some(wanted);
            }

            // Only on change: relabelling a menu item is cheap, but some panels rebuild the whole
            // menu when an item changes, which would fight a user who has it open.
            for (item, (label, applied_label)) in
                menu_items.pr_items().into_iter().zip(update.pr_labels.iter().zip(&mut applied_labels))
            {
                if applied_label.as_deref() != Some(label.as_str()) {
                    item.set_label(label);
                    *applied_label = Some(label.clone());
                }
            }

            // Carries the version, so it changes whenever a newer release appears. Same
            // only-on-change guard as the PR labels, for the same reason.
            if let Some(label) = update.update_label.as_deref()
                && applied_update_label.as_deref() != Some(label)
            {
                menu_items.update.set_label(label);
                applied_update_label = Some(label.to_string());
            }

            indicator.set_title(&update.tooltip);
            // A label is the only part of an indicator visible without hovering, so an unknown
            // state gets a marker the user can actually notice.
            let unsure = update.icon.notifications == crate::state::Presence::Unknown
                || update.icon.review_requested == crate::state::Presence::Unknown
                || update.icon.ready_to_merge == crate::state::Presence::Unknown
                || update.icon.changes_requested == crate::state::Presence::Unknown;
            indicator.set_label(if unsure { "!" } else { "" }, "");
        }
        glib::ControlFlow::Continue
    });
}

// ─── Windows and macOS ────────────────────────────────────────────────────────

/// The custom event type sent into the winit event loop from outside it.
#[cfg(any(target_os = "windows", target_os = "macos"))]
pub enum TrayEvent {
    /// The polling thread has a new notification state to show.
    Update(Update),
    /// A tray menu entry was chosen.
    ///
    /// macOS only. There, menu events arrive through a `muda` callback rather than the polled
    /// channel, and a callback has no way to reach the event loop except as a user event. On
    /// Windows the polled channel is used instead, so this variant would never be constructed.
    #[cfg(target_os = "macos")]
    MenuClick(tray_icon::menu::MenuId),
    /// The tray icon itself was clicked. macOS only, for the same reason.
    #[cfg(target_os = "macos")]
    IconClick,
    /// An update has been installed; hand over to it.
    ///
    /// Arrives from the update thread. Handled on the UI thread because the tray icon has to be dropped
    /// before this process exits, or the shell is left holding a dead icon.
    Restart(RestartPlan),
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
pub fn start_notification_scheduler(
    inputs: PollInputs,
    wake_rx: Receiver<Wake>,
    proxy: winit::event_loop::EventLoopProxy<TrayEvent>,
) {
    let restart_proxy = proxy.clone();
    spawn_poll_thread(
        inputs,
        wake_rx,
        move |update| proxy.send_event(TrayEvent::Update(update)).is_ok(),
        move |plan| {
            // Same asymmetry as everywhere else in this module: Linux uses a channel, these two use the
            // event loop proxy. The tray has to come down on the UI thread before the process ends.
            let _ = restart_proxy.send_event(TrayEvent::Restart(plan));
        },
    );
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
        let url = pr_list_url(PrAxis::ReviewRequested);
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

    /// The two new PR axes get the same "URL matches the API query" guarantee the review axis
    /// already had — a dot claiming a count that the linked page doesn't show is the one thing
    /// this pairing exists to prevent.
    #[test]
    fn merge_url_carries_the_same_filters_as_the_api_query() {
        let url = pr_list_url(PrAxis::ReadyToMerge);
        assert!(url.starts_with("https://github.com/pulls?q="), "got {url}");
        for qualifier in [
            "author%3A%40me",
            "review%3Aapproved",
            "status%3Asuccess",
            "state%3Aopen",
            "draft%3Afalse",
            "archived%3Afalse",
            "sort%3Aupdated-desc",
        ] {
            assert!(url.contains(qualifier), "{qualifier} missing from {url}");
        }
    }

    #[test]
    fn changes_url_carries_the_same_filters_as_the_api_query() {
        let url = pr_list_url(PrAxis::ChangesRequested);
        assert!(url.starts_with("https://github.com/pulls?q="), "got {url}");
        for qualifier in [
            "author%3A%40me",
            "review%3Achanges_requested",
            "state%3Aopen",
            "archived%3Afalse",
            "sort%3Aupdated-desc",
        ] {
            assert!(url.contains(qualifier), "{qualifier} missing from {url}");
        }
    }
}
