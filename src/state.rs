//! Platform-independent notification state machine.
//!
//! The original icon-correctness bug reduced to one thing: the code had a `bool` to describe
//! three situations — present, absent, and "the last poll failed so I genuinely do not know".
//! With no way to say the third, every failure was reported as the second. `Presence` makes the
//! third case representable.
//!
//! There are now two *independent* signals: unread notifications (blue glyph) and pending PR
//! reviews (red dot). They come from different endpoints with different rate-limit budgets and
//! different credentials, so they fail independently — which is why each gets its own `Track`
//! rather than sharing one failure counter. A search outage must never disturb the blue icon.
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

/// What the UI should draw.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct IconState {
    pub notifications: Presence,
    pub reviews: Presence,
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
    notifications: Track,
    /// `None` means no review credential is configured — the feature is off, no search is issued,
    /// and the app behaves exactly as it did before the dot existed.
    reviews: Option<Track>,
    /// Most recent `x-poll-interval`, once GitHub has told us one.
    server_interval: Option<Duration>,
    /// A wait GitHub explicitly demanded; overrides normal pacing for one cycle.
    forced_delay: Option<Duration>,
}

impl PollState {
    pub fn new(reviews_configured: bool) -> Self {
        Self {
            notifications: Track::new(),
            reviews: reviews_configured.then(Track::new),
            server_interval: None,
            forced_delay: None,
        }
    }

    /// Whether the review credential is configured. The loop knows this from its own
    /// `Option<ReviewTokenStore>`, so this exists for the tests to assert against.
    #[cfg(test)]
    pub fn reviews_configured(&self) -> bool {
        self.reviews.is_some()
    }

    /// Turns the review axis on after the user configures a credential mid-run, so the tray
    /// menu's setup flow needs no restart. Idempotent: an already-enabled axis keeps its state
    /// rather than being reset back to `Unknown`.
    pub fn enable_reviews(&mut self) {
        if self.reviews.is_none() {
            self.reviews = Some(Track::new());
        }
    }

    /// Call once per cycle, before applying that cycle's responses.
    pub fn begin_cycle(&mut self) {
        // A forced wait applies to exactly one cycle.
        self.forced_delay = None;
    }

    pub fn apply_notifications(&mut self, response: PollResponse) {
        self.learn_pacing(&response);
        let forced = self.notifications.apply(response.result);
        self.record_forced(forced);
    }

    /// No-op when reviews are unconfigured, so callers do not have to special-case it.
    pub fn apply_reviews(&mut self, response: PollResponse) {
        self.learn_pacing(&response);
        let Some(track) = self.reviews.as_mut() else { return };
        let forced = track.apply(response.result);
        self.record_forced(forced);
    }

    fn learn_pacing(&mut self, response: &PollResponse) {
        // Learn the server's pacing whatever the outcome — it arrives on error responses too.
        if let Some(interval) = response.poll_interval {
            self.server_interval = Some(interval);
        }
    }

    /// Both axes share one sleep, so the stricter demand wins.
    fn record_forced(&mut self, forced: Option<Duration>) {
        if let Some(wait) = forced {
            self.forced_delay = Some(match self.forced_delay {
                Some(existing) => existing.max(wait),
                None => wait,
            });
        }
    }

    pub fn icon(&self) -> IconState {
        IconState {
            notifications: self.notifications.value,
            // Unconfigured reads as a confirmed "no dot", not as Unknown: we are not failing to
            // find out, the user has simply not asked for the feature.
            reviews: self.reviews.as_ref().map_or(Presence::No, |t| t.value),
        }
    }

    /// ETag for the notifications conditional request, if we hold one.
    pub fn notifications_etag(&self) -> Option<&str> {
        self.notifications.etag.as_deref()
    }

    pub fn clear_notifications_etag(&mut self) {
        self.notifications.etag = None;
    }

    /// True once GitHub has rejected that credential. Clears on read so re-auth is attempted
    /// once per rejection rather than on a loop.
    pub fn take_notifications_reauth(&mut self) -> bool {
        std::mem::take(&mut self.notifications.needs_reauth)
    }

    pub fn take_reviews_reauth(&mut self) -> bool {
        self.reviews
            .as_mut()
            .map(|t| std::mem::take(&mut t.needs_reauth))
            .unwrap_or(false)
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
        let failures = self
            .reviews
            .as_ref()
            .map_or(self.notifications.failures, |r| r.failures.max(self.notifications.failures));

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
        let mut lines = vec![match self.notifications.value {
            Presence::Yes => "GitHub: unread notifications".to_string(),
            Presence::No => "GitHub: no unread notifications".to_string(),
            Presence::Unknown => "GitHub: notification state unknown".to_string(),
        }];

        if let Some(reviews) = self.reviews.as_ref() {
            lines.push(match (reviews.value, reviews.count) {
                (Presence::Yes, Some(n)) => format!("{n} PR(s) awaiting your review"),
                (Presence::Yes, None) => "PRs awaiting your review".to_string(),
                (Presence::No, _) => "No reviews requested".to_string(),
                (Presence::Unknown, _) => "Review state unknown".to_string(),
            });
        }

        // Surface at most one reason, preferring the notification axis — the tooltip is 128
        // UTF-16 units on Windows, so two error strings would not survive truncation anyway.
        let detail = self
            .notifications
            .detail
            .as_deref()
            .or_else(|| self.reviews.as_ref().and_then(|r| r.detail.as_deref()));
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

    /// Drives one full cycle so `begin_cycle` bookkeeping is exercised the way the loop does it.
    fn cycle(state: &mut PollState, notifications: PollResponse, reviews: Option<PollResponse>) {
        state.begin_cycle();
        state.apply_notifications(notifications);
        if let Some(r) = reviews {
            state.apply_reviews(r);
        }
    }

    #[test]
    fn starts_unknown_rather_than_claiming_clear() {
        // The old code booted straight to the "no notifications" icon before ever asking.
        assert_eq!(PollState::new(false).icon().notifications, Presence::Unknown);
    }

    #[test]
    fn unconfigured_reviews_read_as_a_confirmed_no_dot() {
        let state = PollState::new(false);
        assert!(!state.reviews_configured());
        // Not Unknown: we are not failing to find out, the feature is simply off.
        assert_eq!(state.icon().reviews, Presence::No);
        assert_eq!(state.icon().reviews.as_confirmed(), Some(false));
    }

    #[test]
    fn unconfigured_reviews_ignore_applied_responses() {
        let mut state = PollState::new(false);
        cycle(&mut state, fresh(false), Some(fresh_count(9)));
        assert_eq!(state.icon().reviews, Presence::No, "must stay off when unconfigured");
    }

    #[test]
    fn both_axes_track_independently() {
        let mut state = PollState::new(true);
        cycle(&mut state, fresh(true), Some(fresh_count(3)));
        assert_eq!(
            state.icon(),
            IconState { notifications: Presence::Yes, reviews: Presence::Yes }
        );

        cycle(&mut state, fresh(false), Some(fresh_count(0)));
        assert_eq!(
            state.icon(),
            IconState { notifications: Presence::No, reviews: Presence::No }
        );
    }

    /// The property that matters most for this feature: the two signals come from different
    /// endpoints and credentials, so one failing must not disturb the other.
    #[test]
    fn a_search_outage_leaves_the_notification_axis_alone() {
        let mut state = PollState::new(true);
        cycle(&mut state, fresh(true), Some(fresh_count(2)));

        for _ in 0..6 {
            cycle(&mut state, fresh(true), Some(transient()));
        }

        assert_eq!(state.icon().notifications, Presence::Yes, "blue icon must keep working");
        assert_eq!(state.icon().reviews, Presence::Unknown, "review axis alone gives up");
    }

    #[test]
    fn a_rejected_notifications_token_leaves_the_review_axis_alone() {
        let mut state = PollState::new(true);
        cycle(&mut state, fresh(true), Some(fresh_count(4)));

        cycle(&mut state, respond(PollResult::Unauthorized), Some(fresh_count(4)));

        assert_eq!(state.icon().notifications, Presence::Unknown);
        assert_eq!(state.icon().reviews, Presence::Yes, "dot must survive the other credential dying");
        assert!(state.take_notifications_reauth());
        assert!(!state.take_reviews_reauth(), "only the failing axis asks for re-auth");
    }

    #[test]
    fn reauth_flags_clear_on_read() {
        let mut state = PollState::new(true);
        cycle(&mut state, fresh(true), Some(respond(PollResult::Unauthorized)));
        assert!(state.take_reviews_reauth());
        assert!(!state.take_reviews_reauth(), "the flag must clear on read");
    }

    #[test]
    fn holds_last_known_state_for_two_failures_then_admits_ignorance() {
        let mut state = PollState::new(false);
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
        let mut state = PollState::new(false);
        cycle(&mut state, fresh(true), None);
        cycle(&mut state, transient(), None);
        cycle(&mut state, transient(), None);

        cycle(&mut state, respond(PollResult::NotModified), None);
        assert_eq!(state.icon().notifications, Presence::Yes);
        assert_eq!(state.next_delay(), MIN_POLL_INTERVAL, "a 304 clears the failure streak");
    }

    #[test]
    fn recovery_resets_the_failure_streak() {
        let mut state = PollState::new(false);
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
        let mut state = PollState::new(false);
        state.begin_cycle();
        state.apply_notifications(PollResponse {
            result: PollResult::Fresh { present: false, etag: None, count: None },
            poll_interval: Some(Duration::from_secs(120)),
        });
        assert_eq!(state.next_delay(), Duration::from_secs(120));
    }

    #[test]
    fn server_interval_below_our_floor_does_not_speed_us_up() {
        let mut state = PollState::new(false);
        state.begin_cycle();
        state.apply_notifications(PollResponse {
            result: PollResult::Fresh { present: false, etag: None, count: None },
            poll_interval: Some(Duration::from_secs(5)),
        });
        assert_eq!(state.next_delay(), MIN_POLL_INTERVAL);
    }

    #[test]
    fn retry_after_is_obeyed_but_never_below_the_floor() {
        let mut state = PollState::new(false);
        cycle(&mut state, respond(PollResult::RateLimited { retry_after: Duration::from_secs(600) }), None);
        assert_eq!(state.next_delay(), Duration::from_secs(600));

        // 30s is shorter than our floor; obeying it literally would poll too fast.
        let mut state = PollState::new(false);
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
        let mut state = PollState::new(true);
        cycle(&mut state, rate_limited(90), Some(rate_limited(600)));
        assert_eq!(state.next_delay(), Duration::from_secs(600), "search limit must govern");

        let mut state = PollState::new(true);
        cycle(&mut state, rate_limited(600), Some(rate_limited(90)));
        assert_eq!(state.next_delay(), Duration::from_secs(600));
    }

    #[test]
    fn rate_limiting_holds_the_icon_rather_than_clearing_it() {
        let mut state = PollState::new(true);
        cycle(&mut state, fresh(true), Some(fresh_count(1)));
        // The old code turned a 403 into Ok(0) and cleared the icon here.
        cycle(&mut state, rate_limited(60), Some(rate_limited(60)));
        assert_eq!(
            state.icon(),
            IconState { notifications: Presence::Yes, reviews: Presence::Yes }
        );
    }

    #[test]
    fn forced_delay_applies_to_one_cycle_only() {
        let mut state = PollState::new(false);
        cycle(&mut state, rate_limited(600), None);
        assert_eq!(state.next_delay(), Duration::from_secs(600));

        cycle(&mut state, fresh(false), None);
        assert_eq!(state.next_delay(), MIN_POLL_INTERVAL);
    }

    #[test]
    fn stays_responsive_while_still_within_the_grace_period() {
        let mut state = PollState::new(false);
        cycle(&mut state, fresh(false), None);

        cycle(&mut state, transient(), None);
        assert_eq!(state.next_delay(), MIN_POLL_INTERVAL);
        cycle(&mut state, transient(), None);
        assert_eq!(state.next_delay(), MIN_POLL_INTERVAL);
    }

    #[test]
    fn backoff_starts_when_we_give_up_then_stops_at_the_cap() {
        let mut state = PollState::new(false);
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
        let mut state = PollState::new(false);
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
        let mut state = PollState::new(true);
        cycle(&mut state, fresh(false), Some(fresh_count(3)));
        let tip = state.tooltip();
        assert!(tip.contains("no unread notifications"), "got {tip:?}");
        assert!(tip.contains("3 PR(s) awaiting your review"), "got {tip:?}");
    }

    #[test]
    fn tooltip_omits_reviews_entirely_when_unconfigured() {
        let mut state = PollState::new(false);
        cycle(&mut state, fresh(true), None);
        let tip = state.tooltip();
        assert!(!tip.to_lowercase().contains("review"), "got {tip:?}");
    }

    #[test]
    fn tooltip_reports_the_reason_and_stays_short() {
        let mut state = PollState::new(true);
        cycle(&mut state, fresh(true), Some(fresh_count(1)));
        cycle(&mut state, respond(PollResult::Transient("x".repeat(500))), Some(fresh_count(1)));

        let tip = state.tooltip();
        assert!(
            tip.chars().count() <= MAX_TOOLTIP_CHARS + 1,
            "got {} chars",
            tip.chars().count()
        );
    }
}
