//! GitHub notifications API client.
//!
//! The job of this module is to be *honest*: every outcome GitHub can produce maps to a
//! distinct variant, so the caller is never handed a plausible-looking zero in place of a
//! failure. The previous version returned `Ok(0)` for any non-2xx response, which meant an
//! expired token or a rate limit rendered as a confident "you have no notifications".

use reqwest::blocking::Client;
use reqwest::header::{HeaderMap, ACCEPT, AUTHORIZATION, ETAG, IF_NONE_MATCH, RETRY_AFTER, USER_AGENT};
use reqwest::StatusCode;
use serde::Deserialize;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const NOTIFICATIONS_URL: &str = "https://api.github.com/notifications";
const AGENT: &str = "git-system-tray";

/// Kept well under the 60s poll floor so a stalled request cannot delay the next one.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// GitHub's fallback guidance when it signals a limit without saying for how long:
/// "wait for at least one minute before retrying".
const DEFAULT_RATE_LIMIT_WAIT: Duration = Duration::from_secs(60);

/// Cap on how much of an error body ends up in a log line or tooltip.
const MAX_DETAIL_CHARS: usize = 200;

/// We only ever ask whether the unread list is non-empty, so no fields are needed.
#[derive(Debug, Deserialize)]
struct Notification {}

/// Everything GitHub can tell us, kept distinguishable because the caller must react
/// differently to each one.
#[derive(Debug)]
pub enum PollResult {
    /// A 200 with a usable body. `unread` is authoritative.
    Fresh { unread: bool, etag: Option<String> },
    /// 304 — the notification list is unchanged, so whatever we already show is still right.
    NotModified,
    /// 401 — the token is dead. Waiting will not fix this; only re-authentication will.
    Unauthorized,
    /// 403/429 carrying a rate-limit signal. Hold state and wait exactly as instructed.
    RateLimited { retry_after: Duration },
    /// Anything else: transport failure, 5xx, unparseable body, or a 403 that is not about
    /// rate limiting (a missing `notifications` scope, say). State is unknown, not clear.
    Transient(String),
}

impl PollResult {
    /// Short tag for the log. Without this a 200 and a 304 produce identical log lines, which
    /// hides exactly the distinction worth watching when the icon looks wrong.
    pub fn kind(&self) -> &'static str {
        match self {
            PollResult::Fresh { .. } => "fresh",
            PollResult::NotModified => "not-modified",
            PollResult::Unauthorized => "unauthorized",
            PollResult::RateLimited { .. } => "rate-limited",
            PollResult::Transient(_) => "transient-failure",
        }
    }
}

#[derive(Debug)]
pub struct PollResponse {
    pub result: PollResult,
    /// From `x-poll-interval`. GitHub raises this under load and we must obey it.
    pub poll_interval: Option<Duration>,
}

/// Builds the shared HTTP client.
///
/// `reqwest`'s blocking client already defaults to a 30s timeout, but 30s inside a 60s poll
/// loop makes a single stalled request eat half the interval. 10s is plenty for one small
/// JSON GET.
pub fn build_client() -> reqwest::Result<Client> {
    Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
}

/// Performs one poll. `etag` enables a conditional request; pass `None` to force a fresh read.
pub fn poll(client: &Client, token: &str, etag: Option<&str>) -> PollResponse {
    // `all=false` is already the default, but stating it makes `!list.is_empty()` provably a
    // question about *unread* items. `per_page=1` because we need presence, not a count.
    let mut request = client
        .get(NOTIFICATIONS_URL)
        .query(&[("all", "false"), ("per_page", "1")])
        .header(ACCEPT, "application/vnd.github+json")
        .header(AUTHORIZATION, format!("Bearer {token}"))
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header(USER_AGENT, AGENT);

    if let Some(tag) = etag {
        request = request.header(IF_NONE_MATCH, tag);
    }

    let response = match request.send() {
        Ok(response) => response,
        // A transport failure carries no headers, so there is no interval to learn from it.
        Err(e) => {
            return PollResponse {
                result: PollResult::Transient(format!("request failed: {e}")),
                poll_interval: None,
            }
        }
    };

    let status = response.status();
    let headers = response.headers().clone();
    let poll_interval = header_u64(&headers, "x-poll-interval").map(Duration::from_secs);

    // A 304 has no body by definition; reading one would just block on nothing.
    let body = if status == StatusCode::NOT_MODIFIED {
        String::new()
    } else {
        response.text().unwrap_or_default()
    };

    PollResponse {
        result: classify(status, &headers, &body, unix_now()),
        poll_interval,
    }
}

/// Maps one HTTP response onto a `PollResult`.
///
/// Pure on purpose — `now` is injected rather than read from the clock — so every branch below
/// is unit-testable without a network or a fixture server. All of the historical bugs lived
/// here, so this is where the tests point.
pub fn classify(status: StatusCode, headers: &HeaderMap, body: &str, now: u64) -> PollResult {
    // ORDER MATTERS. `304` is not `is_success()`, so it has to be caught before the generic
    // non-2xx arm below. Getting this backwards is exactly the old bug: the documented
    // healthy answer for "nothing changed" would be logged as a failure and reported as
    // zero unread, clearing the icon on every quiet poll.
    if status == StatusCode::NOT_MODIFIED {
        return PollResult::NotModified;
    }

    // Never transient: a rejected token stays rejected until it is replaced.
    if status == StatusCode::UNAUTHORIZED {
        return PollResult::Unauthorized;
    }

    if status == StatusCode::FORBIDDEN || status == StatusCode::TOO_MANY_REQUESTS {
        return match rate_limit_wait(headers, now) {
            Some(retry_after) => PollResult::RateLimited { retry_after },
            // A 403 with no rate-limit signal is a different animal — most likely the token
            // lacks the `notifications` scope. Backing off will not help, but claiming the
            // inbox is empty is still the one unacceptable answer.
            None => PollResult::Transient(describe(status, body)),
        };
    }

    if !status.is_success() {
        return PollResult::Transient(describe(status, body));
    }

    match serde_json::from_str::<Vec<Notification>>(body) {
        Ok(list) => PollResult::Fresh {
            unread: !list.is_empty(),
            etag: header_string(headers, ETAG.as_str()),
        },
        // Previously `.unwrap_or_default()`, which turned a garbled payload into "no unread".
        Err(e) => PollResult::Transient(format!("unparseable notification payload: {e}")),
    }
}

/// GitHub's documented precedence for how long to wait after being limited.
///
/// Returns `None` when the response carries no rate-limit signal at all, which is how the
/// caller distinguishes "slow down" from an unrelated 403.
fn rate_limit_wait(headers: &HeaderMap, now: u64) -> Option<Duration> {
    // 1. `retry-after` is an instruction, not a suggestion. Retrying inside this window is
    //    how a short secondary limit becomes a long one.
    if let Some(secs) = header_u64(headers, RETRY_AFTER.as_str()) {
        return Some(Duration::from_secs(secs));
    }

    // 2. Primary quota exhausted — wait for the reset timestamp.
    if header_u64(headers, "x-ratelimit-remaining") == Some(0) {
        let reset = header_u64(headers, "x-ratelimit-reset").unwrap_or(0);
        let wait = reset.saturating_sub(now);
        return Some(Duration::from_secs(wait.max(DEFAULT_RATE_LIMIT_WAIT.as_secs())));
    }

    None
}

fn describe(status: StatusCode, body: &str) -> String {
    format!("HTTP {status}: {}", truncate(body.trim(), MAX_DETAIL_CHARS))
}

/// Truncates on a char boundary so a multi-byte body cannot panic the formatter.
fn truncate(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((idx, _)) => format!("{}…", &text[..idx]),
        None => text.to_string(),
    }
}

fn header_string(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.get(name)?.to_str().ok().map(str::to_string)
}

fn header_u64(headers: &HeaderMap, name: &str) -> Option<u64> {
    header_string(headers, name)?.trim().parse().ok()
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                reqwest::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        map
    }

    #[test]
    fn empty_list_is_clear() {
        let result = classify(StatusCode::OK, &headers(&[]), "[]", 0);
        assert!(matches!(result, PollResult::Fresh { unread: false, .. }));
    }

    #[test]
    fn non_empty_list_is_unread_and_keeps_etag() {
        let result = classify(StatusCode::OK, &headers(&[("etag", "\"abc\"")]), "[{}]", 0);
        match result {
            PollResult::Fresh { unread, etag } => {
                assert!(unread);
                assert_eq!(etag.as_deref(), Some("\"abc\""));
            }
            other => panic!("expected Fresh, got {:?}", other),
        }
    }

    /// The regression that matters most: 304 must never be read as "nothing unread".
    #[test]
    fn not_modified_is_not_a_failure_and_not_clear() {
        let result = classify(StatusCode::NOT_MODIFIED, &headers(&[]), "", 0);
        assert!(matches!(result, PollResult::NotModified));
    }

    #[test]
    fn unauthorized_is_distinct_from_transient() {
        let result = classify(StatusCode::UNAUTHORIZED, &headers(&[]), "{}", 0);
        assert!(matches!(result, PollResult::Unauthorized));
    }

    #[test]
    fn retry_after_takes_precedence() {
        let h = headers(&[("retry-after", "42"), ("x-ratelimit-remaining", "0")]);
        match classify(StatusCode::FORBIDDEN, &h, "", 0) {
            PollResult::RateLimited { retry_after } => {
                assert_eq!(retry_after, Duration::from_secs(42))
            }
            other => panic!("expected RateLimited, got {:?}", other),
        }
    }

    #[test]
    fn exhausted_quota_waits_for_reset() {
        let h = headers(&[("x-ratelimit-remaining", "0"), ("x-ratelimit-reset", "1000")]);
        match classify(StatusCode::FORBIDDEN, &h, "", 100) {
            PollResult::RateLimited { retry_after } => {
                assert_eq!(retry_after, Duration::from_secs(900))
            }
            other => panic!("expected RateLimited, got {:?}", other),
        }
    }

    /// A reset that has already elapsed must not produce a zero-second wait.
    #[test]
    fn stale_reset_falls_back_to_the_minimum() {
        let h = headers(&[("x-ratelimit-remaining", "0"), ("x-ratelimit-reset", "50")]);
        match classify(StatusCode::FORBIDDEN, &h, "", 9_999) {
            PollResult::RateLimited { retry_after } => {
                assert_eq!(retry_after, DEFAULT_RATE_LIMIT_WAIT)
            }
            other => panic!("expected RateLimited, got {:?}", other),
        }
    }

    #[test]
    fn forbidden_without_rate_limit_signal_is_transient() {
        let result = classify(StatusCode::FORBIDDEN, &headers(&[]), "missing scope", 0);
        assert!(matches!(result, PollResult::Transient(_)));
    }

    #[test]
    fn too_many_requests_is_rate_limited() {
        let h = headers(&[("retry-after", "7")]);
        assert!(matches!(
            classify(StatusCode::TOO_MANY_REQUESTS, &h, "", 0),
            PollResult::RateLimited { .. }
        ));
    }

    #[test]
    fn server_error_is_transient() {
        let result = classify(StatusCode::INTERNAL_SERVER_ERROR, &headers(&[]), "boom", 0);
        assert!(matches!(result, PollResult::Transient(_)));
    }

    /// Previously `.unwrap_or_default()` swallowed this into "no unread".
    #[test]
    fn malformed_body_is_transient_not_clear() {
        let result = classify(StatusCode::OK, &headers(&[]), "not json at all", 0);
        assert!(matches!(result, PollResult::Transient(_)));
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate("ünïcödé", 3), "ünï…");
        assert_eq!(truncate("short", 50), "short");
    }
}
