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
const SEARCH_URL: &str = "https://api.github.com/search/issues";
const AGENT: &str = "git-system-tray";

/// Kept well under the 60s poll floor so a stalled request cannot delay the next one.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// GitHub's fallback guidance when it signals a limit without saying for how long:
/// "wait for at least one minute before retrying".
const DEFAULT_RATE_LIMIT_WAIT: Duration = Duration::from_secs(60);

/// Cap on how much of an error body ends up in a log line or tooltip.
const MAX_DETAIL_CHARS: usize = 200;

/// Turns a 2xx body into `(signal_present, exact_count_if_the_endpoint_gives_one)`.
///
/// The two endpoints differ only here: everything about status codes and rate limits is shared,
/// so this is the single seam between them.
type BodyParser = fn(&str) -> Result<(bool, Option<u32>), String>;

/// We only ever ask whether the unread list is non-empty, so no fields are needed.
#[derive(Debug, Deserialize)]
struct Notification {}

/// The one field we need from `/search/issues`. `total_count` is a required field of that
/// response, and it counts matches across all pages — so `per_page=1` still gives a true total.
#[derive(Debug, Deserialize)]
struct SearchResult {
    total_count: u32,
}

/// Everything GitHub can tell us, kept distinguishable because the caller must react
/// differently to each one.
#[derive(Debug)]
pub enum PollResult {
    /// A 200 with a usable body. `present` is authoritative.
    Fresh {
        present: bool,
        etag: Option<String>,
        /// Exact match count when the endpoint provides one (search does; notifications does
        /// not, because we request `per_page=1` and only ask about presence).
        count: Option<u32>,
    },
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
    /// From `x-oauth-scopes`: what the token that made *this* request is actually allowed to do.
    ///
    /// `None` means GitHub did not say, which happens on a transport failure and on every
    /// fine-grained or GitHub App token. That is "unknown", not "no scopes", and the two must not be
    /// confused: a missing header is never grounds for declaring a capability absent.
    pub scopes: Option<String>,
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

/// Polls unread notifications. `etag` enables a conditional request; pass `None` to force a
/// fresh read.
pub fn poll_notifications(client: &Client, token: &str, etag: Option<&str>) -> PollResponse {
    // `all=false` is already the default, but stating it makes `!list.is_empty()` provably a
    // question about *unread* items. `per_page=1` because we need presence, not a count.
    let request = client
        .get(NOTIFICATIONS_URL)
        .query(&[("all", "false"), ("per_page", "1")]);

    send(request, token, etag, parse_notifications)
}

/// Polls for pull requests awaiting the user's review.
///
/// No `If-None-Match` is sent: the search endpoint was measured to return no `etag` at all, and
/// replaying one yields `200` rather than `304`, so a conditional request would only add a
/// header for nothing. Note also that search has its own rate-limit resource — 30 requests per
/// *minute*, independent of the 15000/hour core budget — which is why `classify` reads the
/// rate-limit headers off whichever response it was handed rather than assuming a shared pool.
pub fn poll_reviews(client: &Client, token: &str, query: &str) -> PollResponse {
    let request = client
        .get(SEARCH_URL)
        .query(&[("q", query), ("per_page", "1")]);

    send(request, token, None, parse_search_total)
}

/// Shared request/response plumbing for both endpoints.
fn send(
    request: reqwest::blocking::RequestBuilder,
    token: &str,
    etag: Option<&str>,
    parse_ok: BodyParser,
) -> PollResponse {
    let mut request = request
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
                scopes: None,
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
        result: classify_with(status, &headers, &body, unix_now(), parse_ok),
        poll_interval,
        // Sent on every response to a classic OAuth-app token, including error responses, so this
        // is the authoritative answer to "can this credential do the job" once we have talked to
        // GitHub even once.
        scopes: header_string(&headers, "x-oauth-scopes"),
    }
}

/// Success-body parser for `/notifications`: presence only, no count.
fn parse_notifications(body: &str) -> Result<(bool, Option<u32>), String> {
    serde_json::from_str::<Vec<Notification>>(body)
        .map(|list| (!list.is_empty(), None))
        // Previously `.unwrap_or_default()`, which turned a garbled payload into "no unread".
        .map_err(|e| format!("unparseable notification payload: {e}"))
}

/// Success-body parser for `/search/issues`: exact count, so the tooltip can quote it.
fn parse_search_total(body: &str) -> Result<(bool, Option<u32>), String> {
    serde_json::from_str::<SearchResult>(body)
        .map(|r| (r.total_count > 0, Some(r.total_count)))
        .map_err(|e| format!("unparseable search payload: {e}"))
}

/// Maps one HTTP response onto a `PollResult`.
///
/// Pure on purpose — `now` is injected rather than read from the clock — so every branch below
/// is unit-testable without a network or a fixture server. All of the historical bugs lived
/// here, so this is where the tests point.
///
/// The status handling is shared by both endpoints and only the success-body parser differs,
/// via `parse_ok`. Duplicating this for search would mean two copies of the 304-before-non-2xx
/// ordering and the rate-limit precedence — i.e. two places for the original bug to come back.
pub fn classify_with(
    status: StatusCode,
    headers: &HeaderMap,
    body: &str,
    now: u64,
    parse_ok: BodyParser,
) -> PollResult {
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
            // lacks the scope or permission the endpoint needs. Backing off will not help, but
            // claiming there is nothing pending is still the one unacceptable answer.
            None => PollResult::Transient(describe(status, body)),
        };
    }

    if !status.is_success() {
        return PollResult::Transient(describe(status, body));
    }

    match parse_ok(body) {
        Ok((present, count)) => PollResult::Fresh {
            present,
            count,
            etag: header_string(headers, ETAG.as_str()),
        },
        Err(why) => PollResult::Transient(why),
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

    /// Shorthand: classify a NOTIFICATIONS response.
    fn notif(status: StatusCode, h: &HeaderMap, body: &str, now: u64) -> PollResult {
        classify_with(status, h, body, now, parse_notifications)
    }

    /// Shorthand: classify a SEARCH response.
    fn search(status: StatusCode, h: &HeaderMap, body: &str, now: u64) -> PollResult {
        classify_with(status, h, body, now, parse_search_total)
    }

    // ── Notifications body parsing ────────────────────────────────────────────

    #[test]
    fn empty_list_is_clear() {
        assert!(matches!(
            notif(StatusCode::OK, &headers(&[]), "[]", 0),
            PollResult::Fresh { present: false, .. }
        ));
    }

    #[test]
    fn non_empty_list_is_unread_and_keeps_etag() {
        match notif(StatusCode::OK, &headers(&[("etag", "\"abc\"")]), "[{}]", 0) {
            PollResult::Fresh { present, etag, count } => {
                assert!(present);
                assert_eq!(etag.as_deref(), Some("\"abc\""));
                assert_eq!(count, None, "notifications use per_page=1, so there is no true count");
            }
            other => panic!("expected Fresh, got {:?}", other),
        }
    }

    /// Previously `.unwrap_or_default()` swallowed this into "no unread".
    #[test]
    fn malformed_notifications_body_is_transient_not_clear() {
        assert!(matches!(
            notif(StatusCode::OK, &headers(&[]), "not json at all", 0),
            PollResult::Transient(_)
        ));
    }

    // ── Search body parsing ───────────────────────────────────────────────────

    #[test]
    fn zero_total_count_means_no_review_pending() {
        match search(StatusCode::OK, &headers(&[]), r#"{"total_count":0,"items":[]}"#, 0) {
            PollResult::Fresh { present, count, .. } => {
                assert!(!present);
                assert_eq!(count, Some(0));
            }
            other => panic!("expected Fresh, got {:?}", other),
        }
    }

    #[test]
    fn positive_total_count_means_review_pending_and_reports_the_count() {
        match search(StatusCode::OK, &headers(&[]), r#"{"total_count":7,"items":[{}]}"#, 0) {
            PollResult::Fresh { present, count, .. } => {
                assert!(present);
                assert_eq!(count, Some(7), "total_count is free, so the tooltip can quote it");
            }
            other => panic!("expected Fresh, got {:?}", other),
        }
    }

    /// A body without `total_count` must not read as "nothing to review".
    #[test]
    fn search_body_missing_total_count_is_transient() {
        assert!(matches!(
            search(StatusCode::OK, &headers(&[]), r#"{"items":[]}"#, 0),
            PollResult::Transient(_)
        ));
    }

    #[test]
    fn malformed_search_body_is_transient() {
        assert!(matches!(
            search(StatusCode::OK, &headers(&[]), "<html>502</html>", 0),
            PollResult::Transient(_)
        ));
    }

    // ── Status handling, shared by both endpoints ─────────────────────────────

    /// The regression that matters most: 304 must never be read as "nothing pending".
    #[test]
    fn not_modified_is_not_a_failure_and_not_clear() {
        assert!(matches!(notif(StatusCode::NOT_MODIFIED, &headers(&[]), "", 0), PollResult::NotModified));
        assert!(matches!(search(StatusCode::NOT_MODIFIED, &headers(&[]), "", 0), PollResult::NotModified));
    }

    #[test]
    fn unauthorized_is_distinct_from_transient_on_both_endpoints() {
        assert!(matches!(notif(StatusCode::UNAUTHORIZED, &headers(&[]), "{}", 0), PollResult::Unauthorized));
        assert!(matches!(search(StatusCode::UNAUTHORIZED, &headers(&[]), "{}", 0), PollResult::Unauthorized));
    }

    #[test]
    fn retry_after_takes_precedence() {
        let h = headers(&[("retry-after", "42"), ("x-ratelimit-remaining", "0")]);
        match search(StatusCode::FORBIDDEN, &h, "", 0) {
            PollResult::RateLimited { retry_after } => assert_eq!(retry_after, Duration::from_secs(42)),
            other => panic!("expected RateLimited, got {:?}", other),
        }
    }

    #[test]
    fn exhausted_quota_waits_for_reset() {
        let h = headers(&[("x-ratelimit-remaining", "0"), ("x-ratelimit-reset", "1000")]);
        match notif(StatusCode::FORBIDDEN, &h, "", 100) {
            PollResult::RateLimited { retry_after } => assert_eq!(retry_after, Duration::from_secs(900)),
            other => panic!("expected RateLimited, got {:?}", other),
        }
    }

    /// A reset that has already elapsed must not produce a zero-second wait.
    #[test]
    fn stale_reset_falls_back_to_the_minimum() {
        let h = headers(&[("x-ratelimit-remaining", "0"), ("x-ratelimit-reset", "50")]);
        match notif(StatusCode::FORBIDDEN, &h, "", 9_999) {
            PollResult::RateLimited { retry_after } => assert_eq!(retry_after, DEFAULT_RATE_LIMIT_WAIT),
            other => panic!("expected RateLimited, got {:?}", other),
        }
    }

    #[test]
    fn forbidden_without_rate_limit_signal_is_transient() {
        // e.g. the GitHub App lacks the Pull requests permission.
        assert!(matches!(
            search(StatusCode::FORBIDDEN, &headers(&[]), "missing permission", 0),
            PollResult::Transient(_)
        ));
    }

    #[test]
    fn too_many_requests_is_rate_limited() {
        let h = headers(&[("retry-after", "7")]);
        assert!(matches!(
            search(StatusCode::TOO_MANY_REQUESTS, &h, "", 0),
            PollResult::RateLimited { .. }
        ));
    }

    #[test]
    fn server_error_is_transient() {
        assert!(matches!(
            notif(StatusCode::INTERNAL_SERVER_ERROR, &headers(&[]), "boom", 0),
            PollResult::Transient(_)
        ));
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate("\u{fc}n\u{ef}c\u{f6}d\u{e9}", 3), "\u{fc}n\u{ef}\u{2026}");
        assert_eq!(truncate("short", 50), "short");
    }
}
