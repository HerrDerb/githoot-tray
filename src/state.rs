//! Platform-independent notification state machine.
//!
//! The original icon-correctness bug reduced to one thing: the code had a `bool` to describe
//! three situations — present, absent, and "the last poll failed so I genuinely do not know".
//! With no way to say the third, every failure was reported as the second. `Presence` makes the
//! third case representable.
//!
//! There are now four *independent* signals: unread notifications (blue glyph, optional — see
//! `crate::config`) and three PR-search axes (`PrAxis`): review-requested (red dot), ready-to-merge
//! (green dot), changes-requested (orange dot). They come from different endpoints/queries with
//! different rate-limit budgets and (for notifications vs. the PR axes) different credentials, so
//! they fail independently — which is why each gets its own `Track` rather than sharing one
//! failure counter. A search outage must never disturb the blue icon, and vice versa.
//!
//! Nothing here does I/O, so all of it is testable.

use crate::github::{PollResponse, PollResult};
use std::time::Duration;

// ── Values GitHub never sends us, so they are ours to choose ──────────────────
// Kept as named constants precisely so they stay visible as judgement calls rather than
// hiding as magic numbers next to the values GitHub dictates.

/// Floor on poll frequency. GitHub's advertised `x-poll-interval` can raise this but never
/// lower it. 60s matches what the README promises.
pub const MIN_POLL_INTERVAL: Duration = Duration::from_secs(60);

/// Consecutive failures tolerated before an axis stops asserting a stale value. Absorbs a brief
/// network blip without letting a real outage keep looking healthy.
pub const FAILURES_BEFORE_UNKNOWN: u32 = 3;

/// Ceiling on exponential backoff, so a long outage does not stretch retries into hours.
pub const MAX_BACKOFF: Duration = Duration::from_secs(15 * 60);

/// Successive waits after the user opens a GitHub page, giving GitHub — and the user — time to
/// register that things were read. Cumulatively ~5s, ~20s, ~45s after the click.
pub const REFRESH_BURST: [Duration; 3] = [
    Duration::from_secs(5),
    Duration::from_secs(15),
    Duration::from_secs(25),
];

/// Successive waits after the user opens the tray menu: every 5s, for 15s.
///
/// Evenly spaced and shorter than `REFRESH_BURST` because the two are waiting for different things.
/// That one widens to wait out GitHub's lag in registering a read; this one is answering someone who
/// is looking at the menu right now, so there is nothing to wait out and no reason to keep going.
pub const MENU_BURST: [Duration; 3] = [
    Duration::from_secs(5),
    Duration::from_secs(5),
    Duration::from_secs(5),
];

/// Shell_NotifyIcon's `szTip` holds 128 UTF-16 units including the terminator, so tooltips are
/// clamped well below that.
const MAX_TOOLTIP_CHARS: usize = 110;

/// Whether a signal is present, absent, or genuinely unknown.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Presence {
    /// Confirmed present.
    Yes,
    /// Confirmed absent.
    No,
    /// Not confirmed either way. The one state the original code could not express.
    Unknown,
}

impl Presence {
    /// How the icon should render this: only a *confirmed* answer moves the picture.
    /// `Unknown` returns `None`, meaning "leave the image alone and explain in the tooltip".
    pub fn as_confirmed(self) -> Option<bool> {
        match self {
            Presence::Yes => Some(true),
            Presence::No => Some(false),
            Presence::Unknown => None,
        }
    }
}

/// What the UI should draw: one presence per dot/tint, matching `icons::IconSet`'s four
/// independent signals, plus the one flag that overrides all of them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct IconState {
    /// PR status is waiting for the user to authorize it, so `icons::IconSet::needs_auth` is drawn
    /// and the four fields below are not consulted at all.
    ///
    /// It doubles as the "show the Authenticate menu item" signal, so the icon and the menu cannot
    /// disagree about whether a click is being waited on.
    pub needs_auth: bool,
    /// A newer release exists, so the up-arrow is drawn in the top-left corner.
    ///
    /// Independent of every other field here, including `needs_auth`: an available update says nothing
    /// about your PRs or your credentials, and the arrow sits in a corner nothing else uses. It also
    /// doubles as the "show the Install update menu item" signal, for the same reason `needs_auth`
    /// does — one source of truth means the icon and the menu cannot contradict each other.
    pub update_available: bool,
    pub notifications: Presence,
    pub review_requested: Presence,
    pub ready_to_merge: Presence,
    pub changes_requested: Presence,
}

/// Base text of the tray menu item that opens the review list.
///
/// Lives here, next to the code that appends the count to it, so the two cannot drift apart.
pub const REVIEWS_MENU_LABEL: &str = "Open Requested Reviews";

/// Text of the tray menu item that starts the PR-status Device Flow.
///
/// Shown only while `PollState::pr_needs_auth` holds. Lives here with the other menu wording so the
/// platform UI code in `main.rs` and `scheduler.rs` cannot spell it two different ways.
pub const AUTHENTICATE_MENU_LABEL: &str = "Authenticate GitHub PR Status";

/// Hover text while PR status is waiting to be authorized. Says what is wrong *and* where the fix
/// is, because the red exclamation on its own only says that something is.
const PR_NEEDS_AUTH_TOOLTIP: &str = "PR status: not authorized yet. Use the menu to authorize.";

/// Base text of the tray menu item that installs an available update.
///
/// The version is appended by `update_menu_label`, the same way `pr_menu_label` appends a count: the
/// icon can only say *that* something is available, so the number goes where there is room for it.
pub const UPDATE_MENU_LABEL: &str = "Install update";

/// One of the three independent PR-search signals.
///
/// All three are searched with the same mechanism (`github::poll_reviews`, despite its name — see
/// its own doc comment) against one shared credential, differing only in the query and in how each
/// is displayed. `ALL` fixes the order used both for tooltip-detail priority and, by convention, for
/// which corner/color `icons::IconSet` assigns each dot: review-requested (red, top-right),
/// ready-to-merge (green, bottom-right), changes-requested (orange, bottom-left).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PrAxis {
    ReviewRequested,
    ReadyToMerge,
    ChangesRequested,
}

impl PrAxis {
    pub const ALL: [PrAxis; 3] =
        [PrAxis::ReviewRequested, PrAxis::ReadyToMerge, PrAxis::ChangesRequested];

    /// Position within `ALL`, and the index this axis uses in any `[T; 3]` array associated with
    /// the three PR axes (`Update::pr_labels` in `scheduler`, the platform UI's per-axis
    /// "applied" bookkeeping in `main`) — public so those call sites don't need their own copy of
    /// this mapping.
    pub fn index(self) -> usize {
        match self {
            PrAxis::ReviewRequested => 0,
            PrAxis::ReadyToMerge => 1,
            PrAxis::ChangesRequested => 2,
        }
    }

    /// Base text for this axis's tray menu item, before any count is appended.
    pub fn menu_label(self) -> &'static str {
        match self {
            // Reuses the existing constant rather than duplicating the string, so the two can
            // never drift apart.
            PrAxis::ReviewRequested => REVIEWS_MENU_LABEL,
            PrAxis::ReadyToMerge => "Open Ready to Merge",
            PrAxis::ChangesRequested => "Open Changes Requested",
        }
    }

    /// Tooltip phrase when this axis is confirmed present.
    fn tooltip_yes(self, count: Option<u32>) -> String {
        match (self, count) {
            (PrAxis::ReviewRequested, Some(n)) => format!("{n} PR(s) awaiting your review"),
            (PrAxis::ReviewRequested, None) => "PRs awaiting your review".to_string(),
            (PrAxis::ReadyToMerge, Some(n)) => format!("{n} PR(s) ready to merge"),
            (PrAxis::ReadyToMerge, None) => "PRs ready to merge".to_string(),
            (PrAxis::ChangesRequested, Some(n)) => format!("{n} PR(s) with changes requested"),
            (PrAxis::ChangesRequested, None) => "PRs with changes requested".to_string(),
        }
    }

    /// Tooltip phrase when this axis is confirmed absent.
    fn tooltip_no(self) -> &'static str {
        match self {
            PrAxis::ReviewRequested => "No reviews requested",
            PrAxis::ReadyToMerge => "Nothing ready to merge",
            PrAxis::ChangesRequested => "No changes requested",
        }
    }

    /// Tooltip phrase when this axis's state is unknown.
    fn tooltip_unknown(self) -> &'static str {
        match self {
            PrAxis::ReviewRequested => "Review state unknown",
            PrAxis::ReadyToMerge => "Ready-to-merge state unknown",
            PrAxis::ChangesRequested => "Changes-requested state unknown",
        }
    }
}

/// Per-signal state: last known value, conditional-request tag, and failure streak.
struct Track {
    value: Presence,
    etag: Option<String>,
    failures: u32,
    detail: Option<String>,
    count: Option<u32>,
    needs_reauth: bool,
}

impl Track {
    fn new() -> Self {
        Self {
            // We have not spoken to GitHub yet, so we genuinely do not know.
            value: Presence::Unknown,
            etag: None,
            failures: 0,
            detail: Some("starting up".to_string()),
            count: None,
            needs_reauth: false,
        }
    }

    fn apply(&mut self, result: PollResult) -> Option<Duration> {
        match result {
            PollResult::Fresh { present, etag, count } => {
                self.etag = etag;
                self.count = count;
                self.failures = 0;
                self.detail = None;
                self.value = if present { Presence::Yes } else { Presence::No };
                None
            }

            // A healthy answer meaning nothing changed. What we already show is still correct,
            // and this counts as a success — so it resets the failure streak.
            PollResult::NotModified => {
                self.failures = 0;
                self.detail = None;
                None
            }

            // Not our data being wrong, just GitHub asking us to wait. Hold the value and obey.
            PollResult::RateLimited { retry_after } => {
                self.detail = Some(format!("rate limited, waiting {}s", retry_after.as_secs()));
                Some(retry_after)
            }

            // No grace period: a rejected credential does not recover by waiting, so pretending
            // we still know would be a lie for as long as the app stays open.
            PollResult::Unauthorized => {
                self.etag = None;
                self.failures = self.failures.saturating_add(1);
                self.detail = Some("GitHub rejected the credential".to_string());
                self.value = Presence::Unknown;
                self.needs_reauth = true;
                None
            }

            PollResult::Transient(why) => {
                self.failures = self.failures.saturating_add(1);
                self.detail = Some(why);
                // Hold the last known value for the first few failures so a blip does not make
                // the icon flap, then stop claiming to know.
                if self.failures >= FAILURES_BEFORE_UNKNOWN {
                    self.value = Presence::Unknown;
                }
                None
            }
        }
    }
}

pub struct PollState {
    /// `None` means notifications are off — either the user never opted in via `config.txt`, or
    /// no credential could be obtained for it.
    notifications: Option<Track>,
    /// `None` in a slot means that PR axis is off — no credential, or GitHub reported the
    /// credential lacks the scope/permission the search needs, and no search is issued for it.
    pr: [Option<Track>; 3],
    /// Whether the user asked for each PR axis at all, indexed by `PrAxis::index`.
    ///
    /// **Write-once**: set in `new` and never touched again. The distinction from `pr` is the whole
    /// point of the field existing. `pr[i]` says whether an axis is in play *right now*, which any
    /// runtime event may change — a missing credential, a permission failure. This says whether it is
    /// *allowed* to be, which nothing at runtime may change. Keeping the two apart is what lets
    /// `clear_pr_auth` put axes back after a sign-in without resurrecting one the user switched off.
    pr_enabled: [bool; 3],
    /// Why the corresponding `pr` slot is off, when we know. Without this a dark dot for a broken
    /// credential looks exactly like a dark dot because nothing needs attention, which is the one
    /// confusion this module exists to prevent.
    pr_off: [Option<String>; 3],
    /// PR status has no usable credential and needs the user to start a browser round trip.
    ///
    /// Deliberately separate from `pr_off`, even though both mean "no dots". `pr_off` is a dead end
    /// the user cannot clear from here (the GitHub App is not installed anywhere); this is a state
    /// with a menu item waiting to be clicked, so it gets its own icon and its own wording.
    pr_needs_auth: bool,
    /// The version of the newest release above this build, once a check has found one.
    ///
    /// A `String` rather than a parsed version because its only consumers are a menu label and a
    /// tooltip line, and it should be shown exactly as the release names itself.
    update_available: Option<String>,
    /// Most recent `x-poll-interval`, once GitHub has told us one.
    server_interval: Option<Duration>,
    /// A wait GitHub explicitly demanded; overrides normal pacing for one cycle.
    forced_delay: Option<Duration>,
}

impl PollState {
    /// `pr_enabled` is the user's configuration **only**.
    ///
    /// Deliberately not "configured and we hold a credential". Folding credential presence in here
    /// would make `require_pr_auth` a no-op on precisely the path that exists to recover from a
    /// missing credential — every flag would be false, so there would be no exclamation icon, no
    /// `Authenticate` menu entry, and no way to ever obtain one. A missing credential is said
    /// afterwards, by calling `require_pr_auth` or `disable_pr`.
    pub fn new(notifications_configured: bool, pr_enabled: [bool; 3]) -> Self {
        Self {
            notifications: notifications_configured.then(Track::new),
            // Derived from the value stored below, not from a second read of the argument, so the
            // two can never disagree about which axes exist.
            pr: pr_enabled.map(|enabled| enabled.then(Track::new)),
            pr_enabled,
            pr_off: [None, None, None],
            pr_needs_auth: false,
            update_available: None,
            server_interval: None,
            forced_delay: None,
        }
    }

    /// Whether notifications are configured. Exists for the tests to assert against, the same way
    /// the loop itself already knows this from its own `Option<TokenStore>`.
    #[cfg(test)]
    pub fn notifications_configured(&self) -> bool {
        self.notifications.is_some()
    }

    /// Whether a given PR axis is *enabled by config*, regardless of whether it is live right now.
    ///
    /// Split from `pr_in_play` because this change makes the two diverge: an axis can be enabled by
    /// config and still not in play, which is exactly the state a missing credential produces.
    #[cfg(test)]
    pub fn pr_enabled(&self, axis: PrAxis) -> bool {
        self.pr_enabled[axis.index()]
    }

    /// Turns a PR axis off and records why, so the tooltip can say so.
    ///
    /// Called at startup when the PR credential cannot be obtained, and mid-run if GitHub reports
    /// that the credential lacks what the search needs.
    pub fn disable_pr(&mut self, axis: PrAxis, reason: String) {
        let i = axis.index();
        // Skipped, and not merely as an optimisation. Setting `pr[i] = None` on an already-disabled
        // axis would be harmless; recording a *reason* is not. `tooltip` prints a reason as a line of
        // its own, so a config-disabled axis would grow a hover line explaining a feature the user
        // switched off. Both callers loop over all three axes for a credential-wide failure, so this
        // is a real path, not a hypothetical one.
        if !self.pr_enabled[i] {
            return;
        }
        self.pr[i] = None;
        self.pr_off[i] = Some(reason);
    }

    /// Records that PR status is waiting for the user to authorize it.
    ///
    /// Silences all three axes, because they share one credential: with none in hand there is
    /// nothing to search with, and issuing three searches that will each be rejected would only
    /// burn rate limit. No per-axis reason is stored — `tooltip` says it once for all three, and
    /// the icon and menu item say it without hovering.
    pub fn require_pr_auth(&mut self) {
        // Nothing to authorize when every axis is switched off: the exclamation and the
        // `Authenticate` entry would be demanding a credential that no search would ever use.
        //
        // The guard lives here rather than at the call sites because `pr_needs_auth` drives three
        // things at once — the icon override, a tooltip line, and the menu entry's visibility — and
        // one flag with three consumers should have one gate.
        if !self.any_pr_enabled() {
            return;
        }
        self.pr_needs_auth = true;
        for axis in PrAxis::ALL {
            self.pr[axis.index()] = None;
            self.pr_off[axis.index()] = None;
        }
    }

    /// Undoes `require_pr_auth` once a credential has been obtained, putting all three axes back in
    /// play so the next cycle can fill them in.
    ///
    /// They come back as fresh `Track`s rather than with their old values restored: whatever was
    /// last known predates the credential going away, and re-asserting it would be claiming an
    /// answer nothing has confirmed since. A fresh `Track` starts at `Presence::Unknown`, which is
    /// the honest answer and also the one that leaves the icon alone until a real poll lands.
    pub fn clear_pr_auth(&mut self) {
        self.pr_needs_auth = false;
        for axis in PrAxis::ALL {
            let i = axis.index();
            // The guard that matters, and the reason `pr_enabled` exists. This used to be an
            // unconditional `Some(Track::new())`, which meant obtaining a credential switched **on**
            // every axis — including ones the config had turned off. Getting a credential says
            // nothing about whether a signal is wanted.
            self.pr[i] = self.pr_enabled[i].then(Track::new);
            // Unconditional, unlike `pr` above: a disabled axis has no reason recorded to begin with
            // (see `disable_pr`), so this is self-healing rather than wrong.
            self.pr_off[i] = None;
        }
    }

    /// Whether any PR axis is enabled at all.
    fn any_pr_enabled(&self) -> bool {
        self.pr_enabled.iter().any(|&enabled| enabled)
    }

    /// Whether `axis` is in play: enabled by config, and not currently silenced.
    ///
    /// The poll loop's question, and so the one accessor here that is not `#[cfg(test)]`. Search has
    /// its own 30-per-minute budget and `apply_pr` discards the answer for an axis that is not in
    /// play, so issuing the request is pure cost.
    ///
    /// Deliberately the *live* predicate rather than `pr_enabled`, so an axis silenced at runtime — no
    /// credential, App not installed — is skipped too, where searching is equally pointless.
    pub fn pr_in_play(&self, axis: PrAxis) -> bool {
        self.pr[axis.index()].is_some()
    }

    /// Records the newest release above this build, or clears it.
    ///
    /// Called after each update check. Passing `None` clears the arrow, which matters after a
    /// successful install: the new binary reports its own version, so the very next check finds
    /// nothing newer and the arrow has to come back down.
    pub fn set_update_available(&mut self, version: Option<String>) {
        self.update_available = version;
    }

    /// Text for the tray menu item that installs the update, or `None` when there is none.
    ///
    /// Carries the version for the same reason `pr_menu_label` carries a count: the icon can only say
    /// that *something* is available, and "Install update: 1.4.0" is what makes it actionable without
    /// hovering.
    pub fn update_menu_label(&self) -> Option<String> {
        self.update_available.as_ref().map(|v| format!("{UPDATE_MENU_LABEL}: {v}"))
    }

    /// Whether PR status is waiting on the user.
    ///
    /// `#[cfg(test)]` like the two `*_configured` accessors above: the UI reads this through
    /// `icon().needs_auth` so the icon and the menu item cannot disagree, which leaves no non-test
    /// caller for a second way to ask.
    #[cfg(test)]
    pub fn pr_needs_auth(&self) -> bool {
        self.pr_needs_auth
    }

    /// Call once per cycle, before applying that cycle's responses.
    pub fn begin_cycle(&mut self) {
        // A forced wait applies to exactly one cycle.
        self.forced_delay = None;
    }

    /// No-op when notifications are unconfigured, so callers do not have to special-case it —
    /// same contract as `apply_pr`.
    pub fn apply_notifications(&mut self, response: PollResponse) {
        self.learn_pacing(&response);
        let Some(track) = self.notifications.as_mut() else { return };
        let forced = track.apply(response.result);
        self.record_forced(forced);
    }

    /// No-op when `axis` is unconfigured, so callers do not have to special-case it.
    pub fn apply_pr(&mut self, axis: PrAxis, response: PollResponse) {
        self.learn_pacing(&response);
        let Some(track) = self.pr[axis.index()].as_mut() else { return };
        let forced = track.apply(response.result);
        self.record_forced(forced);
    }

    fn learn_pacing(&mut self, response: &PollResponse) {
        // Learn the server's pacing whatever the outcome — it arrives on error responses too.
        if let Some(interval) = response.poll_interval {
            self.server_interval = Some(interval);
        }
    }

    /// All axes share one sleep, so the stricter demand wins.
    fn record_forced(&mut self, forced: Option<Duration>) {
        if let Some(wait) = forced {
            self.forced_delay = Some(match self.forced_delay {
                Some(existing) => existing.max(wait),
                None => wait,
            });
        }
    }

    pub fn icon(&self) -> IconState {
        // Unavailable reads as a confirmed "no dot"/"no tint" on every axis: we are not failing
        // to find out, there is simply nothing configured to ask with.
        IconState {
            needs_auth: self.pr_needs_auth,
            update_available: self.update_available.is_some(),
            notifications: self.notifications.as_ref().map_or(Presence::No, |t| t.value),
            review_requested: self.pr_value(PrAxis::ReviewRequested),
            ready_to_merge: self.pr_value(PrAxis::ReadyToMerge),
            changes_requested: self.pr_value(PrAxis::ChangesRequested),
        }
    }

    fn pr_value(&self, axis: PrAxis) -> Presence {
        self.pr[axis.index()].as_ref().map_or(Presence::No, |t| t.value)
    }

    /// Text for the tray menu item that opens `axis`'s list, carrying the exact count.
    ///
    /// The icon itself can only carry a dot. A digit is not legible at the 16px the shell asks for,
    /// so the number goes where there is room for it: the tooltip and this menu item. Unlike the
    /// icon, neither has a limit, so the real figure is shown however large it gets.
    pub fn pr_menu_label(&self, axis: PrAxis) -> String {
        let base = axis.menu_label();
        match self.pr[axis.index()].as_ref() {
            // Only a *confirmed* count is shown. An `Unknown` axis is still holding the last number
            // it saw, and putting a stale figure in a menu label would assert something we no longer
            // know, which is the one thing this module refuses to do.
            Some(track) if track.value == Presence::Yes => match track.count {
                Some(n) if n > 0 => format!("{base} ({n})"),
                _ => base.to_string(),
            },
            _ => base.to_string(),
        }
    }

    /// ETag for the notifications conditional request, if we hold one.
    pub fn notifications_etag(&self) -> Option<&str> {
        self.notifications.as_ref().and_then(|t| t.etag.as_deref())
    }

    pub fn clear_notifications_etag(&mut self) {
        if let Some(track) = self.notifications.as_mut() {
            track.etag = None;
        }
    }

    /// True once GitHub has rejected that credential. Clears on read so re-auth is attempted
    /// once per rejection rather than on a loop.
    pub fn take_notifications_reauth(&mut self) -> bool {
        self.notifications
            .as_mut()
            .map(|t| std::mem::take(&mut t.needs_reauth))
            .unwrap_or(false)
    }

    pub fn take_pr_reauth(&mut self, axis: PrAxis) -> bool {
        self.pr[axis.index()]
            .as_mut()
            .map(|t| std::mem::take(&mut t.needs_reauth))
            .unwrap_or(false)
    }

    /// Whether GitHub has explicitly demanded a wait this cycle.
    ///
    /// `next_delay` already folds this in, but a caller running a user-triggered burst bypasses
    /// normal pacing entirely, so it needs to be able to ask the question directly.
    pub fn rate_limited(&self) -> bool {
        self.forced_delay.is_some()
    }

    /// How long to wait before the next scheduled poll.
    pub fn next_delay(&self) -> Duration {
        // Never faster than our own floor, and never faster than GitHub's advertised interval.
        let base = self
            .server_interval
            .unwrap_or(MIN_POLL_INTERVAL)
            .max(MIN_POLL_INTERVAL);

        // An explicit `retry-after` wins — but it still cannot drop us below the floor.
        if let Some(forced) = self.forced_delay {
            return forced.max(base);
        }

        // Back off on the worst-affected axis, but only once it has actually given up on its
        // last known value. While inside the grace period, keep the normal cadence — the common
        // cause of early failures is a tray app launched at login before the network is up, and
        // backing off immediately would mean not noticing WiFi for many minutes.
        //
        // Written as a fold over whichever axes are actually configured, rather than named
        // fields, so it does not need hand-editing every time an axis is added or removed.
        let failures = std::iter::once(self.notifications.as_ref())
            .chain(self.pr.iter().map(Option::as_ref))
            .flatten()
            .map(|t| t.failures)
            .max()
            .unwrap_or(0);

        let exponent = failures.saturating_sub(FAILURES_BEFORE_UNKNOWN - 1);
        if exponent == 0 {
            return base;
        }

        // The trailing `.max(base)` matters: if GitHub ever advertises an interval longer than
        // MAX_BACKOFF, the cap must not pull us back under the interval it just told us to
        // respect.
        let factor = 1u32 << exponent.min(16);
        base.saturating_mul(factor).min(MAX_BACKOFF).max(base)
    }

    /// Hover text. This is the only place `Unknown` becomes visible on Windows, so the reason
    /// belongs here rather than only in the log.
    pub fn tooltip(&self) -> String {
        let mut lines = Vec::new();

        // Unlike a disabled PR axis, notifications being off is never a failure worth explaining
        // — it is either a deliberate config choice or there is nothing configured yet, so the
        // line is simply omitted rather than replaced with a reason.
        if let Some(notifications) = self.notifications.as_ref() {
            lines.push(match notifications.value {
                Presence::Yes => "GitHub: unread notifications".to_string(),
                Presence::No => "GitHub: no unread notifications".to_string(),
                Presence::Unknown => "GitHub: notification state unknown".to_string(),
            });
        }

        // One line for all three axes, not one each. They share a single credential, so three
        // copies of the same sentence would say nothing extra while eating the whole
        // `MAX_TOOLTIP_CHARS` budget — and the per-axis lines below would be claiming answers that
        // were never fetched.
        if self.pr_needs_auth {
            lines.push(PR_NEEDS_AUTH_TOOLTIP.to_string());
        } else {
            for axis in PrAxis::ALL {
                match (self.pr[axis.index()].as_ref(), self.pr_off[axis.index()].as_deref()) {
                    (Some(track), _) => lines.push(match track.value {
                        Presence::Yes => axis.tooltip_yes(track.count),
                        Presence::No => axis.tooltip_no().to_string(),
                        Presence::Unknown => axis.tooltip_unknown().to_string(),
                    }),
                    // The dot is off for a reason the user can fix, so hovering has to say which one.
                    (None, Some(reason)) => lines.push(reason.to_string()),
                    (None, None) => {}
                }
            }
        }

        // Last of the state lines, and deliberately not gated on anything above it: an available
        // update is orthogonal to notifications and PRs, so it is reported whatever else is going on.
        if let Some(version) = self.update_available.as_deref() {
            lines.push(format!("Update available: {version}"));
        }

        // Surface at most one reason, preferring notifications and then PR axes in `PrAxis::ALL`
        // order — the tooltip is 128 UTF-16 units on Windows, so more than one error string would
        // not survive truncation anyway.
        let detail = self
            .notifications
            .as_ref()
            .and_then(|t| t.detail.as_deref())
            .or_else(|| self.pr.iter().filter_map(Option::as_ref).find_map(|t| t.detail.as_deref()));
        if let Some(detail) = detail {
            lines.push(detail.to_string());
        }

        let text = lines.join("\n");
        match text.char_indices().nth(MAX_TOOLTIP_CHARS) {
            Some((idx, _)) => format!("{}…", &text[..idx]),
            None => text,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn respond(result: PollResult) -> PollResponse {
        PollResponse { result, poll_interval: None }
    }

    fn fresh(present: bool) -> PollResponse {
        respond(PollResult::Fresh {
            present,
            etag: Some("\"tag\"".to_string()),
            count: None,
        })
    }

    fn fresh_count(n: u32) -> PollResponse {
        respond(PollResult::Fresh { present: n > 0, etag: None, count: Some(n) })
    }

    fn transient() -> PollResponse {
        respond(PollResult::Transient("network down".to_string()))
    }

    /// Only the review-requested axis is configured/driven in most tests below — the two newer PR
    /// axes get their own dedicated tests further down, since most of this suite predates them and
    /// is about the state machine's general behavior, not about having three PR axes specifically.
    fn new_state(notifications: bool, review_requested: bool) -> PollState {
        PollState::new(notifications, [review_requested, false, false])
    }

    /// Drives one full cycle so `begin_cycle` bookkeeping is exercised the way the loop does it.
    fn cycle(state: &mut PollState, notifications: PollResponse, reviews: Option<PollResponse>) {
        state.begin_cycle();
        state.apply_notifications(notifications);
        if let Some(r) = reviews {
            state.apply_pr(PrAxis::ReviewRequested, r);
        }
    }

    #[test]
    fn starts_unknown_rather_than_claiming_clear() {
        // The old code booted straight to the "no notifications" icon before ever asking.
        assert_eq!(new_state(true, false).icon().notifications, Presence::Unknown);
    }

    #[test]
    fn unconfigured_reviews_read_as_a_confirmed_no_dot() {
        let state = new_state(true, false);
        assert!(!state.pr_in_play(PrAxis::ReviewRequested));
        // Not Unknown: we are not failing to find out, the feature is simply off.
        assert_eq!(state.icon().review_requested, Presence::No);
        assert_eq!(state.icon().review_requested.as_confirmed(), Some(false));
    }

    #[test]
    fn unconfigured_reviews_ignore_applied_responses() {
        let mut state = new_state(true, false);
        cycle(&mut state, fresh(false), Some(fresh_count(9)));
        assert_eq!(state.icon().review_requested, Presence::No, "must stay off when unconfigured");
    }

    /// Mirrors the review-axis test above: notifications are optional now too (off by default,
    /// opt-in via `config.txt`), and must behave identically when unconfigured.
    #[test]
    fn unconfigured_notifications_read_as_a_confirmed_no_dot() {
        let state = new_state(false, true);
        assert!(!state.notifications_configured());
        assert_eq!(state.icon().notifications, Presence::No);
        assert_eq!(state.icon().notifications.as_confirmed(), Some(false));
    }

    #[test]
    fn unconfigured_notifications_ignore_applied_responses() {
        let mut state = new_state(false, true);
        cycle(&mut state, fresh(true), Some(fresh_count(1)));
        assert_eq!(state.icon().notifications, Presence::No, "must stay off when unconfigured");
    }

    #[test]
    fn both_axes_track_independently() {
        let mut state = new_state(true, true);
        cycle(&mut state, fresh(true), Some(fresh_count(3)));
        assert_eq!(state.icon().notifications, Presence::Yes);
        assert_eq!(state.icon().review_requested, Presence::Yes);

        cycle(&mut state, fresh(false), Some(fresh_count(0)));
        assert_eq!(state.icon().notifications, Presence::No);
        assert_eq!(state.icon().review_requested, Presence::No);
    }

    /// The property that matters most for this feature: the two signals come from different
    /// endpoints and credentials, so one failing must not disturb the other.
    #[test]
    fn a_search_outage_leaves_the_notification_axis_alone() {
        let mut state = new_state(true, true);
        cycle(&mut state, fresh(true), Some(fresh_count(2)));

        for _ in 0..6 {
            cycle(&mut state, fresh(true), Some(transient()));
        }

        assert_eq!(state.icon().notifications, Presence::Yes, "blue icon must keep working");
        assert_eq!(state.icon().review_requested, Presence::Unknown, "review axis alone gives up");
    }

    #[test]
    fn a_rejected_notifications_token_leaves_the_review_axis_alone() {
        let mut state = new_state(true, true);
        cycle(&mut state, fresh(true), Some(fresh_count(4)));

        cycle(&mut state, respond(PollResult::Unauthorized), Some(fresh_count(4)));

        assert_eq!(state.icon().notifications, Presence::Unknown);
        assert_eq!(
            state.icon().review_requested,
            Presence::Yes,
            "dot must survive the other credential dying"
        );
        assert!(state.take_notifications_reauth());
        assert!(
            !state.take_pr_reauth(PrAxis::ReviewRequested),
            "only the failing axis asks for re-auth"
        );
    }

    #[test]
    fn reauth_flags_clear_on_read() {
        let mut state = new_state(true, true);
        cycle(&mut state, fresh(true), Some(respond(PollResult::Unauthorized)));
        assert!(state.take_pr_reauth(PrAxis::ReviewRequested));
        assert!(
            !state.take_pr_reauth(PrAxis::ReviewRequested),
            "the flag must clear on read"
        );
    }

    #[test]
    fn holds_last_known_state_for_two_failures_then_admits_ignorance() {
        let mut state = new_state(true, false);
        cycle(&mut state, fresh(true), None);

        cycle(&mut state, transient(), None);
        assert_eq!(state.icon().notifications, Presence::Yes, "one blip must not flap");
        cycle(&mut state, transient(), None);
        assert_eq!(state.icon().notifications, Presence::Yes, "two blips must not flap");
        cycle(&mut state, transient(), None);
        assert_eq!(
            state.icon().notifications,
            Presence::Unknown,
            "a sustained failure must stop claiming to know"
        );
    }

    #[test]
    fn not_modified_preserves_state_and_counts_as_success() {
        let mut state = new_state(true, false);
        cycle(&mut state, fresh(true), None);
        cycle(&mut state, transient(), None);
        cycle(&mut state, transient(), None);

        cycle(&mut state, respond(PollResult::NotModified), None);
        assert_eq!(state.icon().notifications, Presence::Yes);
        assert_eq!(state.next_delay(), MIN_POLL_INTERVAL, "a 304 clears the failure streak");
    }

    #[test]
    fn recovery_resets_the_failure_streak() {
        let mut state = new_state(true, false);
        cycle(&mut state, fresh(true), None);
        for _ in 0..5 {
            cycle(&mut state, transient(), None);
        }
        assert_eq!(state.icon().notifications, Presence::Unknown);

        cycle(&mut state, fresh(false), None);
        assert_eq!(state.icon().notifications, Presence::No);
        assert_eq!(state.next_delay(), MIN_POLL_INTERVAL, "backoff must not persist");
    }

    #[test]
    fn server_interval_overrides_our_floor_when_longer() {
        let mut state = new_state(true, false);
        state.begin_cycle();
        state.apply_notifications(PollResponse {
            result: PollResult::Fresh { present: false, etag: None, count: None },
            poll_interval: Some(Duration::from_secs(120)),
        });
        assert_eq!(state.next_delay(), Duration::from_secs(120));
    }

    #[test]
    fn server_interval_below_our_floor_does_not_speed_us_up() {
        let mut state = new_state(true, false);
        state.begin_cycle();
        state.apply_notifications(PollResponse {
            result: PollResult::Fresh { present: false, etag: None, count: None },
            poll_interval: Some(Duration::from_secs(5)),
        });
        assert_eq!(state.next_delay(), MIN_POLL_INTERVAL);
    }

    #[test]
    fn retry_after_is_obeyed_but_never_below_the_floor() {
        let mut state = new_state(true, false);
        cycle(&mut state, respond(PollResult::RateLimited { retry_after: Duration::from_secs(600) }), None);
        assert_eq!(state.next_delay(), Duration::from_secs(600));

        // 30s is shorter than our floor; obeying it literally would poll too fast.
        let mut state = new_state(true, false);
        cycle(&mut state, respond(PollResult::RateLimited { retry_after: Duration::from_secs(30) }), None);
        assert_eq!(state.next_delay(), MIN_POLL_INTERVAL);
    }

    /// Search has a much tighter budget (30/min) than core, so a search rate-limit must be able
    /// to slow the whole cycle down.
    fn rate_limited(secs: u64) -> PollResponse {
        respond(PollResult::RateLimited { retry_after: Duration::from_secs(secs) })
    }

    #[test]
    fn the_stricter_of_the_two_forced_delays_wins() {
        let mut state = new_state(true, true);
        cycle(&mut state, rate_limited(90), Some(rate_limited(600)));
        assert_eq!(state.next_delay(), Duration::from_secs(600), "search limit must govern");

        let mut state = new_state(true, true);
        cycle(&mut state, rate_limited(600), Some(rate_limited(90)));
        assert_eq!(state.next_delay(), Duration::from_secs(600));
    }

    #[test]
    fn rate_limiting_holds_the_icon_rather_than_clearing_it() {
        let mut state = new_state(true, true);
        cycle(&mut state, fresh(true), Some(fresh_count(1)));
        // The old code turned a 403 into Ok(0) and cleared the icon here.
        cycle(&mut state, rate_limited(60), Some(rate_limited(60)));
        assert_eq!(state.icon().notifications, Presence::Yes);
        assert_eq!(state.icon().review_requested, Presence::Yes);
    }

    /// Paired with `forced_delay_applies_to_one_cycle_only`: the scheduler reads this to decide
    /// whether a burst may run, so it has to be true for exactly as long as the delay itself is.
    #[test]
    fn rate_limited_reports_only_while_the_demand_is_live() {
        let mut state = new_state(true, false);
        assert!(!state.rate_limited(), "nothing has been demanded yet");

        cycle(&mut state, rate_limited(600), None);
        assert!(state.rate_limited());

        cycle(&mut state, fresh(false), None);
        assert!(!state.rate_limited(), "a healthy answer clears the demand");
    }

    /// A limit on either axis has to hold the whole cycle back, since both share one sleep.
    #[test]
    fn rate_limited_reports_a_demand_from_the_review_axis_too() {
        let mut state = new_state(true, true);
        cycle(&mut state, fresh(true), Some(rate_limited(120)));
        assert!(state.rate_limited(), "search has its own, much tighter budget");
    }

    #[test]
    fn forced_delay_applies_to_one_cycle_only() {
        let mut state = new_state(true, false);
        cycle(&mut state, rate_limited(600), None);
        assert_eq!(state.next_delay(), Duration::from_secs(600));

        cycle(&mut state, fresh(false), None);
        assert_eq!(state.next_delay(), MIN_POLL_INTERVAL);
    }

    #[test]
    fn stays_responsive_while_still_within_the_grace_period() {
        let mut state = new_state(true, false);
        cycle(&mut state, fresh(false), None);

        cycle(&mut state, transient(), None);
        assert_eq!(state.next_delay(), MIN_POLL_INTERVAL);
        cycle(&mut state, transient(), None);
        assert_eq!(state.next_delay(), MIN_POLL_INTERVAL);
    }

    #[test]
    fn backoff_starts_when_we_give_up_then_stops_at_the_cap() {
        let mut state = new_state(true, false);
        cycle(&mut state, fresh(false), None);

        cycle(&mut state, transient(), None);
        cycle(&mut state, transient(), None);
        cycle(&mut state, transient(), None);
        assert_eq!(state.icon().notifications, Presence::Unknown);
        assert_eq!(state.next_delay(), Duration::from_secs(120));
        cycle(&mut state, transient(), None);
        assert_eq!(state.next_delay(), Duration::from_secs(240));

        for _ in 0..20 {
            cycle(&mut state, transient(), None);
        }
        assert_eq!(state.next_delay(), MAX_BACKOFF, "must saturate, not overflow");
    }

    #[test]
    fn a_long_server_interval_survives_the_backoff_cap() {
        let mut state = new_state(true, false);
        let long = Duration::from_secs(30 * 60);
        for _ in 0..4 {
            state.begin_cycle();
            state.apply_notifications(PollResponse {
                result: PollResult::Transient("nope".to_string()),
                poll_interval: Some(long),
            });
        }
        // MAX_BACKOFF is 15 min; clamping to it would poll faster than GitHub just asked.
        assert_eq!(state.next_delay(), long);
    }

    #[test]
    fn tooltip_quotes_the_review_count() {
        let mut state = new_state(true, true);
        cycle(&mut state, fresh(false), Some(fresh_count(3)));
        let tip = state.tooltip();
        assert!(tip.contains("no unread notifications"), "got {tip:?}");
        assert!(tip.contains("3 PR(s) awaiting your review"), "got {tip:?}");
    }

    /// A dark dot with no explanation is indistinguishable from "nothing to review", so a
    /// disabled axis has to say why on hover.
    #[test]
    fn a_disabled_review_axis_explains_itself_in_the_tooltip() {
        // Enabled by config, then silenced at runtime — which is the only combination that produces a
        // reason. An axis the config never asked for is silent instead, deliberately: see
        // `a_config_disabled_axis_shows_no_dot_no_count_and_no_tooltip_line`.
        let mut state = new_state(true, true);
        state.disable_pr(PrAxis::ReviewRequested, "PR status off: sign-in failed".to_string());
        cycle(&mut state, fresh(false), None);

        let tip = state.tooltip();
        assert!(tip.contains("sign-in failed"), "got {tip:?}");
        assert_eq!(
            state.icon().review_requested,
            Presence::No,
            "off means no dot, not an unknown dot"
        );
    }

    #[test]
    fn disabling_reviews_stops_the_axis_asserting_anything() {
        let mut state = new_state(true, true);
        cycle(&mut state, fresh(false), Some(fresh_count(5)));
        assert_eq!(state.icon().review_requested, Presence::Yes);

        state.disable_pr(PrAxis::ReviewRequested, "PR status off: sign-in failed".to_string());
        cycle(&mut state, fresh(false), Some(fresh_count(5)));
        assert_eq!(
            state.icon().review_requested,
            Presence::No,
            "a disabled axis must ignore late answers"
        );
    }

    /// The menu is where the exact number lives, since the icon can only carry a dot.
    #[test]
    fn the_menu_label_carries_the_exact_count() {
        let mut state = new_state(true, true);
        cycle(&mut state, fresh(false), Some(fresh_count(3)));
        assert_eq!(state.pr_menu_label(PrAxis::ReviewRequested), "Open Requested Reviews (3)");

        // No 9+ ceiling here: unlike a 16px icon, a menu item has room for any figure.
        cycle(&mut state, fresh(false), Some(fresh_count(147)));
        assert_eq!(state.pr_menu_label(PrAxis::ReviewRequested), "Open Requested Reviews (147)");
    }

    #[test]
    fn the_menu_label_drops_the_count_when_there_is_nothing_to_count() {
        let mut state = new_state(true, true);
        cycle(&mut state, fresh(false), Some(fresh_count(0)));
        assert_eq!(
            state.pr_menu_label(PrAxis::ReviewRequested),
            REVIEWS_MENU_LABEL,
            "zero is not worth parentheses"
        );

        let mut state = new_state(true, false);
        cycle(&mut state, fresh(false), None);
        assert_eq!(
            state.pr_menu_label(PrAxis::ReviewRequested),
            REVIEWS_MENU_LABEL,
            "no credential, no count"
        );
    }

    /// A stale number in a menu label would assert something we no longer know.
    #[test]
    fn the_menu_label_drops_a_count_it_can_no_longer_vouch_for() {
        let mut state = new_state(true, true);
        cycle(&mut state, fresh(false), Some(fresh_count(4)));
        assert_eq!(state.pr_menu_label(PrAxis::ReviewRequested), "Open Requested Reviews (4)");

        // Enough failures that the axis stops claiming to know.
        for _ in 0..FAILURES_BEFORE_UNKNOWN {
            cycle(&mut state, fresh(false), Some(transient()));
        }
        assert_eq!(state.icon().review_requested, Presence::Unknown);
        assert_eq!(
            state.pr_menu_label(PrAxis::ReviewRequested),
            REVIEWS_MENU_LABEL,
            "an unknown axis must not quote its last known figure"
        );
    }

    /// Within the grace period the value is still asserted, so the count stands with it.
    #[test]
    fn the_menu_label_survives_a_single_blip() {
        let mut state = new_state(true, true);
        cycle(&mut state, fresh(false), Some(fresh_count(2)));
        cycle(&mut state, fresh(false), Some(transient()));
        assert_eq!(state.icon().review_requested, Presence::Yes, "one blip must not flap");
        assert_eq!(state.pr_menu_label(PrAxis::ReviewRequested), "Open Requested Reviews (2)");
    }

    #[test]
    fn tooltip_omits_reviews_entirely_when_unconfigured() {
        let mut state = new_state(true, false);
        cycle(&mut state, fresh(true), None);
        let tip = state.tooltip();
        assert!(!tip.to_lowercase().contains("review"), "got {tip:?}");
    }

    /// Unlike a disabled review axis, an unconfigured notifications axis gets no explanatory line
    /// at all — being off is a deliberate config choice, not a failure to explain.
    #[test]
    fn tooltip_omits_notifications_entirely_when_unconfigured() {
        let mut state = new_state(false, true);
        cycle(&mut state, fresh(true), Some(fresh_count(2)));
        let tip = state.tooltip();
        assert!(!tip.to_lowercase().contains("notification"), "got {tip:?}");
        assert!(tip.contains("2 PR(s)"), "the review line must still be present: {tip:?}");
    }

    #[test]
    fn tooltip_reports_the_reason_and_stays_short() {
        let mut state = new_state(true, true);
        cycle(&mut state, fresh(true), Some(fresh_count(1)));
        cycle(&mut state, respond(PollResult::Transient("x".repeat(500))), Some(fresh_count(1)));

        let tip = state.tooltip();
        assert!(
            tip.chars().count() <= MAX_TOOLTIP_CHARS + 1,
            "got {} chars",
            tip.chars().count()
        );
    }

    // ── The three PR axes together ─────────────────────────────────────────────
    // Everything above exercises the state machine generally through the review-requested axis,
    // which predates the other two. These tests are specifically about `PrAxis` generalizing
    // cleanly to three axes rather than two named fields.

    #[test]
    fn all_three_pr_axes_are_independently_configurable() {
        let state = PollState::new(false, [true, false, true]);
        // At construction the config notion and the live notion agree, and this is the only place
        // that is true — so both are pinned here, and their divergence is pinned in
        // `each_pr_axis_can_be_disabled_independently` below.
        for (axis, expected) in [
            (PrAxis::ReviewRequested, true),
            (PrAxis::ReadyToMerge, false),
            (PrAxis::ChangesRequested, true),
        ] {
            assert_eq!(state.pr_enabled(axis), expected, "{axis:?} config");
            assert_eq!(state.pr_in_play(axis), expected, "{axis:?} live");
        }
    }

    #[test]
    fn all_three_pr_axes_track_independently() {
        let mut state = PollState::new(false, [true, true, true]);
        state.begin_cycle();
        state.apply_pr(PrAxis::ReviewRequested, fresh_count(1));
        state.apply_pr(PrAxis::ReadyToMerge, fresh_count(0));
        state.apply_pr(PrAxis::ChangesRequested, fresh_count(2));

        let icon = state.icon();
        assert_eq!(icon.review_requested, Presence::Yes);
        assert_eq!(icon.ready_to_merge, Presence::No);
        assert_eq!(icon.changes_requested, Presence::Yes);

        // A sustained failure on one axis must not disturb the other two.
        for _ in 0..FAILURES_BEFORE_UNKNOWN {
            state.begin_cycle();
            state.apply_pr(PrAxis::ReadyToMerge, transient());
        }
        let icon = state.icon();
        assert_eq!(icon.review_requested, Presence::Yes, "unrelated axis must be unaffected");
        assert_eq!(icon.ready_to_merge, Presence::Unknown, "the failing axis alone gives up");
        assert_eq!(icon.changes_requested, Presence::Yes, "unrelated axis must be unaffected");
    }

    #[test]
    fn each_pr_axis_has_its_own_menu_label_and_count() {
        let mut state = PollState::new(false, [true, true, true]);
        state.begin_cycle();
        state.apply_pr(PrAxis::ReviewRequested, fresh_count(3));
        state.apply_pr(PrAxis::ReadyToMerge, fresh_count(1));
        state.apply_pr(PrAxis::ChangesRequested, fresh_count(5));

        assert_eq!(state.pr_menu_label(PrAxis::ReviewRequested), "Open Requested Reviews (3)");
        assert_eq!(state.pr_menu_label(PrAxis::ReadyToMerge), "Open Ready to Merge (1)");
        assert_eq!(state.pr_menu_label(PrAxis::ChangesRequested), "Open Changes Requested (5)");
    }

    #[test]
    fn each_pr_axis_can_be_disabled_independently() {
        let mut state = PollState::new(false, [true, true, true]);
        state.disable_pr(PrAxis::ReadyToMerge, "merge dot off: test reason".to_string());

        assert!(state.pr_in_play(PrAxis::ReviewRequested));
        assert!(!state.pr_in_play(PrAxis::ReadyToMerge));
        assert!(state.pr_in_play(PrAxis::ChangesRequested));

        // The divergence `pr_enabled` exists for: this axis was silenced at *runtime*, so it is out of
        // play, but it is still enabled by config — which is what lets `clear_pr_auth` know to bring it
        // back while leaving a config-disabled axis alone.
        assert!(
            state.pr_enabled(PrAxis::ReadyToMerge),
            "a runtime disable must not rewrite the user's configuration"
        );

        let tip = state.tooltip();
        assert!(tip.contains("merge dot off: test reason"), "got {tip:?}");
    }

    // ── Waiting on the user to authorize ───────────────────────────────────────

    #[test]
    fn require_pr_auth_silences_every_axis_and_flags_the_icon() {
        let mut state = PollState::new(false, [true, true, true]);
        state.require_pr_auth();

        assert!(state.pr_needs_auth());
        assert!(state.icon().needs_auth, "the icon must carry the override");
        for axis in PrAxis::ALL {
            assert!(
                !state.pr_in_play(axis),
                "{axis:?} must be silenced — one credential is missing, so all three are"
            );
        }
    }

    /// A silenced axis must not be revivable by a stray response. `apply_pr` is documented as a
    /// no-op when an axis is unconfigured, and this is the case that matters: a search already in
    /// flight when the credential died must not put a dot back on an icon that is telling the user
    /// nothing is known.
    #[test]
    fn responses_arriving_after_require_pr_auth_are_ignored() {
        let mut state = PollState::new(false, [true, true, true]);
        state.require_pr_auth();
        state.begin_cycle();
        state.apply_pr(PrAxis::ReviewRequested, fresh_count(7));

        assert_eq!(state.icon().review_requested, Presence::No);
        assert!(state.icon().needs_auth);
        assert_eq!(state.pr_menu_label(PrAxis::ReviewRequested), REVIEWS_MENU_LABEL);
    }

    /// The three axes share one credential, so the tooltip must explain it once. Three copies would
    /// fit inside no sensible budget and add nothing.
    #[test]
    fn the_needs_auth_tooltip_is_said_once_not_once_per_axis() {
        let mut state = PollState::new(false, [true, true, true]);
        state.require_pr_auth();

        let tip = state.tooltip();
        assert_eq!(tip.matches(PR_NEEDS_AUTH_TOOLTIP).count(), 1, "got {tip:?}");
        assert!(tip.chars().count() <= MAX_TOOLTIP_CHARS + 1, "got {} chars", tip.chars().count());
        // The per-axis lines must be gone entirely: "No reviews requested" alongside "not
        // authorized" would be answering a question that was never asked.
        assert!(!tip.contains(PrAxis::ReviewRequested.tooltip_no()), "got {tip:?}");
    }

    /// Notifications are a separate credential, so their line must survive. This is the split case
    /// the whole per-credential design exists for.
    #[test]
    fn needs_auth_leaves_the_notifications_line_alone() {
        let mut state = PollState::new(true, [true, true, true]);
        state.begin_cycle();
        state.apply_notifications(fresh(true));
        state.require_pr_auth();

        let tip = state.tooltip();
        assert!(tip.contains("unread notifications"), "got {tip:?}");
        assert!(tip.contains(PR_NEEDS_AUTH_TOOLTIP), "got {tip:?}");
    }

    #[test]
    fn clear_pr_auth_puts_every_axis_back_in_play_without_stale_values() {
        let mut state = PollState::new(false, [true, true, true]);
        state.begin_cycle();
        state.apply_pr(PrAxis::ReviewRequested, fresh_count(4));
        assert_eq!(state.icon().review_requested, Presence::Yes);

        state.require_pr_auth();
        state.clear_pr_auth();

        assert!(!state.pr_needs_auth());
        assert!(!state.icon().needs_auth);
        for axis in PrAxis::ALL {
            assert!(state.pr_in_play(axis), "{axis:?} must be searchable again");
        }
        // `Unknown`, not `Yes` and not `No`: the count of 4 predates the credential going away, and
        // nothing has confirmed anything since. `Unknown` is also what stops the icon flickering —
        // the UI leaves that dot as it is until a real answer lands.
        assert_eq!(state.icon().review_requested, Presence::Unknown);
        assert_eq!(state.pr_menu_label(PrAxis::ReviewRequested), REVIEWS_MENU_LABEL);
    }

    /// `require_pr_auth` must clear any earlier `disable_pr` reason, or the tooltip would carry a
    /// stale "off because…" line next to the new "not authorized" one.
    #[test]
    fn require_pr_auth_supersedes_an_earlier_disable_reason() {
        let mut state = PollState::new(false, [true, true, true]);
        state.disable_pr(PrAxis::ReadyToMerge, "merge dot off: earlier reason".to_string());
        state.require_pr_auth();

        let tip = state.tooltip();
        assert!(!tip.contains("earlier reason"), "got {tip:?}");
        assert_eq!(tip, PR_NEEDS_AUTH_TOOLTIP);
    }

    // ── Per-axis configuration ──────────────────────────────────────────────
    //
    // A config-disabled axis has to be invisible in six separate ways: no bar, no count, no tooltip
    // line, no search, no contribution to backoff, and no credential renewal. Most of those are true
    // today only as a side effect of `pr[i] == None`, with nothing saying so — which is what these pin.

    /// One line, but four modules index `[T; 3]` by `PrAxis::index` and nothing asserted that `ALL` is
    /// in that order. The config array makes a fifth.
    #[test]
    fn pr_axis_all_is_in_index_order() {
        assert_eq!(PrAxis::ALL.map(PrAxis::index), [0, 1, 2]);
    }

    #[test]
    fn a_config_disabled_axis_shows_no_dot_no_count_and_no_tooltip_line() {
        let mut state = PollState::new(false, [true, false, true]);
        state.begin_cycle();
        for axis in PrAxis::ALL {
            state.apply_pr(axis, fresh_count(4));
        }

        assert!(!state.pr_enabled(PrAxis::ReadyToMerge));
        assert!(!state.pr_in_play(PrAxis::ReadyToMerge));

        // `No`, and emphatically not `Unknown`. This is what makes the UI need no changes at all: the
        // drains use `as_confirmed().unwrap_or(current)`, so an `Unknown` would leave whatever bar is
        // already on screen lit rather than hiding it.
        assert_eq!(state.icon().ready_to_merge, Presence::No);
        assert_eq!(state.icon().ready_to_merge.as_confirmed(), Some(false));
        // No count appended, so the menu entry reads as bare even though a response arrived.
        assert_eq!(state.pr_menu_label(PrAxis::ReadyToMerge), PrAxis::ReadyToMerge.menu_label());

        let tip = state.tooltip();
        for phrase in [PrAxis::ReadyToMerge.tooltip_no(), PrAxis::ReadyToMerge.tooltip_unknown()] {
            assert!(!tip.contains(phrase), "disabled axis leaked {phrase:?} into {tip:?}");
        }
        // …while the two enabled axes still report.
        assert!(tip.contains("4 PR(s) awaiting your review"), "got {tip:?}");
    }

    /// Asserted as an exact string, not a `contains`: the point is that the PR half contributes
    /// *nothing*, and a stray blank line from `lines.join` or a leaked detail would slip past a
    /// looser check.
    #[test]
    fn every_axis_disabled_leaves_the_pr_half_completely_silent() {
        let mut state = PollState::new(true, [false; 3]);
        state.begin_cycle();
        state.apply_notifications(fresh(true));

        assert_eq!(state.tooltip(), "GitHub: unread notifications");
    }

    /// Paired with the test below on purpose. The plausible slip is writing `all` where `any` was
    /// meant, and this test alone would still pass if that happened.
    #[test]
    fn require_pr_auth_is_ignored_when_every_axis_is_config_disabled() {
        let mut state = PollState::new(false, [false; 3]);
        state.require_pr_auth();

        assert!(!state.pr_needs_auth(), "nothing to authorize when nothing is enabled");
        assert!(!state.icon().needs_auth, "no exclamation for a feature that is switched off");
        assert_eq!(state.tooltip(), "", "and no hover line either");
    }

    #[test]
    fn require_pr_auth_still_flags_the_icon_when_only_one_axis_is_enabled() {
        let mut state = PollState::new(false, [false, true, false]);
        state.require_pr_auth();

        assert!(state.pr_needs_auth());
        assert!(state.icon().needs_auth);
        assert_eq!(state.tooltip(), PR_NEEDS_AUTH_TOOLTIP);
    }

    /// Mimics the credential-wide failure loop in `scheduler`, which calls `disable_pr` for all three
    /// axes. The count is exact: a `contains` would pass even with a third line for the disabled axis.
    #[test]
    fn disable_pr_does_not_give_a_config_disabled_axis_a_reason_to_show() {
        let mut state = PollState::new(false, [true, false, true]);
        let reason = "PR status off: sign-in failed";
        for axis in PrAxis::ALL {
            state.disable_pr(axis, reason.to_string());
        }

        let tip = state.tooltip();
        assert_eq!(tip.matches(reason).count(), 2, "one line per *enabled* axis, got {tip:?}");
    }

    /// The bug this whole field exists to fix. Obtaining a credential says nothing about whether a
    /// signal is wanted, but `clear_pr_auth` used to switch all three back on regardless.
    #[test]
    fn clear_pr_auth_does_not_resurrect_a_config_disabled_axis() {
        let mut state = PollState::new(false, [true, false, true]);
        state.require_pr_auth();
        state.clear_pr_auth();

        assert!(state.pr_in_play(PrAxis::ReviewRequested), "an enabled axis must come back");
        assert!(state.pr_in_play(PrAxis::ChangesRequested), "an enabled axis must come back");
        assert!(
            !state.pr_in_play(PrAxis::ReadyToMerge),
            "authenticating must not switch on an axis the config turned off"
        );
        assert_eq!(state.icon().ready_to_merge, Presence::No);
        assert!(!state.tooltip().contains(PrAxis::ReadyToMerge.tooltip_no()));
    }

    /// Extends the documented in-flight-response race across the one moment `pr[i]` is rewritten,
    /// which is exactly where the old `clear_pr_auth` would have handed the response a live `Track`.
    #[test]
    fn a_response_for_a_config_disabled_axis_is_ignored_even_after_authenticating() {
        let mut state = PollState::new(false, [true, false, false]);
        state.require_pr_auth();
        state.clear_pr_auth();
        state.begin_cycle();
        state.apply_pr(PrAxis::ReadyToMerge, fresh_count(7));

        assert_eq!(state.icon().ready_to_merge, Presence::No);
        assert_eq!(state.pr_menu_label(PrAxis::ReadyToMerge), PrAxis::ReadyToMerge.menu_label());
    }

    /// A disabled axis must not be able to slow the notification half down. True today only because
    /// the backoff fold skips `None` slots, which nothing stated.
    #[test]
    fn a_config_disabled_axis_contributes_no_failures_to_backoff() {
        let mut state = PollState::new(true, [true, false, false]);
        for _ in 0..6 {
            state.begin_cycle();
            state.apply_pr(PrAxis::ReadyToMerge, transient());
        }
        assert_eq!(
            state.next_delay(),
            MIN_POLL_INTERVAL,
            "failures on a disabled axis must not back anything off"
        );

        // …while the same failures on an enabled axis do.
        for _ in 0..4 {
            state.begin_cycle();
            state.apply_pr(PrAxis::ReviewRequested, transient());
        }
        assert!(state.next_delay() > MIN_POLL_INTERVAL, "an enabled axis still backs off");
    }

    /// A stray 401 on a disabled axis must not start a refresh grant or a device flow.
    #[test]
    fn a_config_disabled_axis_never_asks_for_credential_re_auth() {
        let mut state = PollState::new(false, [true, false, false]);
        state.begin_cycle();
        state.apply_pr(PrAxis::ReadyToMerge, respond(PollResult::Unauthorized));
        assert!(!state.take_pr_reauth(PrAxis::ReadyToMerge));

        state.begin_cycle();
        state.apply_pr(PrAxis::ReviewRequested, respond(PollResult::Unauthorized));
        assert!(state.take_pr_reauth(PrAxis::ReviewRequested), "an enabled axis still asks");
    }

    /// "No search is issued" has to follow from state, not from a second copy of the config living in
    /// the scheduler. Walks the four situations the poll loop can observe.
    #[test]
    fn the_poll_loop_is_told_which_axes_to_skip() {
        let mut state = PollState::new(false, [true, false, true]);
        let in_play = |s: &PollState| PrAxis::ALL.map(|a| s.pr_in_play(a));

        assert_eq!(in_play(&state), [true, false, true], "from config");

        state.require_pr_auth();
        assert_eq!(in_play(&state), [false; 3], "nothing to search without a credential");

        state.clear_pr_auth();
        assert_eq!(in_play(&state), [true, false, true], "back to the config, not to all three");

        state.disable_pr(PrAxis::ReviewRequested, "x".to_string());
        assert_eq!(in_play(&state), [false, false, true], "a runtime disable also stops the search");
    }

    #[test]
    fn next_delay_backs_off_on_the_worst_of_all_configured_axes() {
        let mut state = PollState::new(false, [true, true, true]);
        state.begin_cycle();
        state.apply_pr(PrAxis::ReviewRequested, fresh_count(0));
        state.apply_pr(PrAxis::ChangesRequested, fresh_count(0));

        for _ in 0..4 {
            state.begin_cycle();
            state.apply_pr(PrAxis::ReadyToMerge, transient());
        }
        assert_eq!(
            state.next_delay(),
            Duration::from_secs(240),
            "backoff must key off the worst axis even when it is not review-requested"
        );
    }
}
