//! Platform-independent notification state machine.
//!
//! The icon-correctness bug reduced to one thing: the code had a `bool` to describe three
//! situations — unread, clear, and "the last poll failed so I genuinely do not know". With no
//! way to say the third, every failure was reported as the second. `IconState` fixes that; the
//! rest of this module decides *when* to admit ignorance and *when* to poll next.
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

/// Consecutive failures tolerated before the icon stops asserting a stale value. Absorbs a
/// brief network blip without letting a real outage keep looking healthy.
pub const FAILURES_BEFORE_UNKNOWN: u32 = 3;

/// Ceiling on exponential backoff, so a long outage does not stretch retries into hours.
pub const MAX_BACKOFF: Duration = Duration::from_secs(15 * 60);

/// Successive waits after the user opens the notifications page, giving GitHub — and the
/// user — time to register that things were read. Cumulatively ~5s, ~20s, ~45s after the click.
pub const REFRESH_BURST: [Duration; 3] = [
    Duration::from_secs(5),
    Duration::from_secs(15),
    Duration::from_secs(25),
];

/// Shell_NotifyIcon's `szTip` holds 128 UTF-16 units including the terminator, so tooltips are
/// clamped well below that.
const MAX_TOOLTIP_CHARS: usize = 110;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IconState {
    /// Confirmed: nothing unread.
    Clear,
    /// Confirmed: something unread.
    Unread,
    /// Not confirmed either way. The one state the old code could not express.
    Unknown,
}

pub struct PollState {
    icon: IconState,
    etag: Option<String>,
    consecutive_failures: u32,
    /// Most recent `x-poll-interval`, once GitHub has told us one.
    server_interval: Option<Duration>,
    /// A wait GitHub explicitly demanded; overrides normal pacing for one cycle.
    forced_delay: Option<Duration>,
    /// Why we are unsure, for the tooltip and the log.
    detail: Option<String>,
    needs_reauth: bool,
}

impl PollState {
    pub fn new() -> Self {
        Self {
            // We have not spoken to GitHub yet, so we genuinely do not know.
            icon: IconState::Unknown,
            etag: None,
            consecutive_failures: 0,
            server_interval: None,
            forced_delay: None,
            detail: Some("starting up".to_string()),
            needs_reauth: false,
        }
    }

    /// Folds one poll response into the current state and returns what to display.
    pub fn apply(&mut self, response: PollResponse) -> IconState {
        // Learn the server's pacing whatever the outcome — it arrives on error responses too.
        if let Some(interval) = response.poll_interval {
            self.server_interval = Some(interval);
        }
        // A forced wait applies to exactly one cycle; clear last cycle's before evaluating.
        self.forced_delay = None;

        match response.result {
            PollResult::Fresh { unread, etag } => {
                self.etag = etag;
                self.consecutive_failures = 0;
                self.detail = None;
                self.icon = if unread { IconState::Unread } else { IconState::Clear };
            }

            // A healthy answer meaning the list is unchanged. What we already show is still
            // correct, and this counts as a success — so it resets the failure streak.
            PollResult::NotModified => {
                self.consecutive_failures = 0;
                self.detail = None;
            }

            // Not our data being wrong, just GitHub asking us to wait. Hold the icon and obey.
            PollResult::RateLimited { retry_after } => {
                self.forced_delay = Some(retry_after);
                self.detail = Some(format!("rate limited, waiting {}s", retry_after.as_secs()));
            }

            // No grace period: a rejected token does not recover by waiting, so pretending we
            // still know the answer would be a lie for as long as the app stays open.
            PollResult::Unauthorized => {
                self.etag = None;
                self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                self.detail = Some("GitHub rejected the access token".to_string());
                self.icon = IconState::Unknown;
                self.needs_reauth = true;
            }

            PollResult::Transient(why) => {
                self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                self.detail = Some(why);
                // Hold the last known state for the first few failures so a blip does not make
                // the icon flap, then stop claiming to know.
                if self.consecutive_failures >= FAILURES_BEFORE_UNKNOWN {
                    self.icon = IconState::Unknown;
                }
            }
        }

        self.icon
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

        // Back off only once we have actually given up on the last known state. While we are
        // still inside the grace period, keep the normal cadence — the common cause of early
        // failures is a tray app launched at login before the network is up, and backing off
        // immediately would mean not noticing WiFi for many minutes.
        let exponent = self
            .consecutive_failures
            .saturating_sub(FAILURES_BEFORE_UNKNOWN - 1);
        if exponent == 0 {
            return base;
        }

        // The trailing `.max(base)` matters: if GitHub ever advertises an interval longer than
        // MAX_BACKOFF, the cap must not pull us back under the interval it just told us to
        // respect.
        let factor = 1u32 << exponent.min(16);
        base.saturating_mul(factor).min(MAX_BACKOFF).max(base)
    }

    /// The ETag for a conditional request, if we hold one.
    pub fn etag(&self) -> Option<&str> {
        self.etag.as_deref()
    }

    /// True once GitHub has rejected the token. Clears on read so re-auth is attempted once
    /// per rejection rather than on a loop.
    pub fn take_needs_reauth(&mut self) -> bool {
        std::mem::take(&mut self.needs_reauth)
    }

    /// Dropped so the next poll is unconditional — used after re-authentication and after a
    /// user-initiated refresh.
    pub fn clear_etag(&mut self) {
        self.etag = None;
    }

    /// Current state without folding in a response. Used by the tests to assert that a
    /// failure held the previous value instead of overwriting it.
    #[cfg(test)]
    pub fn icon(&self) -> IconState {
        self.icon
    }

    /// Hover text. This is the only place `Unknown` becomes visible on Windows, so the reason
    /// belongs here rather than only in the log.
    pub fn tooltip(&self) -> String {
        let headline = match self.icon {
            IconState::Unread => "GitHub: unread notifications",
            IconState::Clear => "GitHub: no unread notifications",
            IconState::Unknown => "GitHub: state unknown",
        };

        let text = match &self.detail {
            Some(detail) => format!("{headline}\n{detail}"),
            None => headline.to_string(),
        };

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

    fn fresh(unread: bool) -> PollResponse {
        respond(PollResult::Fresh { unread, etag: Some("\"tag\"".to_string()) })
    }

    fn transient() -> PollResponse {
        respond(PollResult::Transient("network down".to_string()))
    }

    #[test]
    fn starts_unknown_rather_than_claiming_clear() {
        // The old code booted straight to the "no notifications" icon before ever asking.
        assert_eq!(PollState::new().icon(), IconState::Unknown);
    }

    #[test]
    fn fresh_results_set_state_and_store_etag() {
        let mut state = PollState::new();
        assert_eq!(state.apply(fresh(true)), IconState::Unread);
        assert_eq!(state.etag(), Some("\"tag\""));
        assert_eq!(state.apply(fresh(false)), IconState::Clear);
    }

    #[test]
    fn holds_last_known_state_for_two_failures_then_admits_ignorance() {
        let mut state = PollState::new();
        state.apply(fresh(true));

        assert_eq!(state.apply(transient()), IconState::Unread, "one blip must not flap");
        assert_eq!(state.apply(transient()), IconState::Unread, "two blips must not flap");
        assert_eq!(
            state.apply(transient()),
            IconState::Unknown,
            "a sustained failure must stop claiming to know"
        );
    }

    #[test]
    fn recovery_resets_the_failure_streak() {
        let mut state = PollState::new();
        state.apply(fresh(true));
        for _ in 0..5 {
            state.apply(transient());
        }
        assert_eq!(state.icon(), IconState::Unknown);

        assert_eq!(state.apply(fresh(false)), IconState::Clear);
        assert_eq!(state.next_delay(), MIN_POLL_INTERVAL, "backoff must not persist");
    }

    #[test]
    fn not_modified_preserves_state_and_counts_as_success() {
        let mut state = PollState::new();
        state.apply(fresh(true));
        state.apply(transient());
        state.apply(transient());

        assert_eq!(state.apply(respond(PollResult::NotModified)), IconState::Unread);
        // Two failures were pending; a 304 clears them, so pacing returns to normal.
        assert_eq!(state.next_delay(), MIN_POLL_INTERVAL);
    }

    #[test]
    fn unauthorized_is_immediately_unknown_and_requests_reauth() {
        let mut state = PollState::new();
        state.apply(fresh(true));

        assert_eq!(state.apply(respond(PollResult::Unauthorized)), IconState::Unknown);
        assert!(state.take_needs_reauth());
        assert!(!state.take_needs_reauth(), "the flag must clear on read");
        assert_eq!(state.etag(), None, "a dead token's etag is worthless");
    }

    #[test]
    fn server_interval_overrides_our_floor_when_longer() {
        let mut state = PollState::new();
        state.apply(PollResponse {
            result: PollResult::Fresh { unread: false, etag: None },
            poll_interval: Some(Duration::from_secs(120)),
        });
        assert_eq!(state.next_delay(), Duration::from_secs(120));
    }

    #[test]
    fn server_interval_below_our_floor_does_not_speed_us_up() {
        let mut state = PollState::new();
        state.apply(PollResponse {
            result: PollResult::Fresh { unread: false, etag: None },
            poll_interval: Some(Duration::from_secs(5)),
        });
        assert_eq!(state.next_delay(), MIN_POLL_INTERVAL);
    }

    #[test]
    fn retry_after_is_obeyed_but_never_below_the_floor() {
        let mut state = PollState::new();
        state.apply(respond(PollResult::RateLimited { retry_after: Duration::from_secs(600) }));
        assert_eq!(state.next_delay(), Duration::from_secs(600));

        // 30s is shorter than our floor; obeying it literally would poll too fast.
        let mut state = PollState::new();
        state.apply(respond(PollResult::RateLimited { retry_after: Duration::from_secs(30) }));
        assert_eq!(state.next_delay(), MIN_POLL_INTERVAL);
    }

    #[test]
    fn rate_limiting_holds_the_icon_rather_than_clearing_it() {
        let mut state = PollState::new();
        state.apply(fresh(true));
        // The old code turned a 403 into Ok(0) and cleared the icon here.
        assert_eq!(
            state.apply(respond(PollResult::RateLimited { retry_after: Duration::from_secs(60) })),
            IconState::Unread
        );
    }

    #[test]
    fn forced_delay_applies_to_one_cycle_only() {
        let mut state = PollState::new();
        state.apply(respond(PollResult::RateLimited { retry_after: Duration::from_secs(600) }));
        assert_eq!(state.next_delay(), Duration::from_secs(600));

        state.apply(fresh(false));
        assert_eq!(state.next_delay(), MIN_POLL_INTERVAL);
    }

    /// The offline-boot case: a tray app started at login must keep checking at the normal
    /// cadence while it still hopes, or it will not notice the network coming up for minutes.
    #[test]
    fn stays_responsive_while_still_within_the_grace_period() {
        let mut state = PollState::new();
        state.apply(fresh(false));

        state.apply(transient());
        assert_eq!(state.next_delay(), MIN_POLL_INTERVAL);
        state.apply(transient());
        assert_eq!(state.next_delay(), MIN_POLL_INTERVAL);
    }

    #[test]
    fn backoff_starts_when_we_give_up_then_stops_at_the_cap() {
        let mut state = PollState::new();
        state.apply(fresh(false));

        // Third failure is the one that flips to Unknown, so it is also the one that slows down.
        state.apply(transient());
        state.apply(transient());
        assert_eq!(state.apply(transient()), IconState::Unknown);
        assert_eq!(state.next_delay(), Duration::from_secs(120));
        state.apply(transient());
        assert_eq!(state.next_delay(), Duration::from_secs(240));
        state.apply(transient());
        assert_eq!(state.next_delay(), Duration::from_secs(480));

        for _ in 0..20 {
            state.apply(transient());
        }
        assert_eq!(state.next_delay(), MAX_BACKOFF, "must saturate, not overflow");
    }

    #[test]
    fn a_long_server_interval_survives_the_backoff_cap() {
        let mut state = PollState::new();
        let long = Duration::from_secs(30 * 60);
        state.apply(PollResponse {
            result: PollResult::Transient("nope".to_string()),
            poll_interval: Some(long),
        });
        // MAX_BACKOFF is 15 min; clamping to it would poll faster than GitHub just asked.
        assert_eq!(state.next_delay(), long);
    }

    #[test]
    fn tooltip_reports_the_reason_and_stays_short() {
        let mut state = PollState::new();
        state.apply(fresh(true));
        assert_eq!(state.tooltip(), "GitHub: unread notifications");

        state.apply(respond(PollResult::Transient("x".repeat(500))));
        let tip = state.tooltip();
        assert!(tip.chars().count() <= MAX_TOOLTIP_CHARS + 1, "got {} chars", tip.chars().count());
    }
}
