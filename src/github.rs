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
const GRAPHQL_URL: &str = "https://api.github.com/graphql";
const AGENT: &str = "githoot-tray";

/// How many search hits the changes-requested GraphQL poll inspects.
///
/// A cap rather than pagination, and it undercounts rather than overcounts: past 100 matching pull
/// requests the extras are simply not seen. Not reachable by the inbox this app exists for, and
/// paginating would trade a real increase in complexity for a case nobody has.
const SEARCH_HITS_CAP: u32 = 100;

/// How many opinionated reviews and pending requests the changes-requested query reads per hit.
///
/// Same undercount-not-overcount trade as `SEARCH_HITS_CAP`: a pull request with more than 20
/// opinionated reviews or 20 pending review requests could be judged on a partial list, which is
/// unreachable by this app's inbox.
const CHANGES_REVIEWS_CAP: u32 = 20;

/// Kept well under the 60s poll floor so a stalled request cannot delay the next one.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// GitHub's fallback guidance when it signals a limit without saying for how long:
/// "wait for at least one minute before retrying".
const DEFAULT_RATE_LIMIT_WAIT: Duration = Duration::from_secs(60);

/// Cap on how much of an error body ends up in a log line or tooltip.
const MAX_DETAIL_CHARS: usize = 200;

/// What a success-body parser extracts from a 2xx body.
///
/// The endpoints differ only here: everything about status codes and rate limits is shared,
/// so this is the single seam between them.
struct Parsed {
    /// Whether the signal is present at all — the one field every endpoint can answer.
    present: bool,
    /// Exact match count, when the endpoint provides one (search does; notifications does not).
    count: Option<u32>,
    /// The URLs of the exact items `count` counted, when the endpoint reads its hits one by one.
    /// Only the changes-requested GraphQL query does; for everything else the count is a server
    /// total whose members were never fetched, so this is `None` — as it also is when any counted
    /// hit came back without a URL, because a partial list would open fewer pages than the dot
    /// claims. `None` means "send the user to the search page instead", never "no PRs".
    urls: Option<Vec<String>>,
}

type BodyParser = fn(&str) -> Result<Parsed, String>;

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
        /// URLs of the exact items `count` counted, when the endpoint reads its hits one by
        /// one — see `Parsed::urls`. Carried so the changes-requested menu entry can open the
        /// very pull requests its dot is counting, which no search URL can express.
        urls: Option<Vec<String>>,
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
    /// The detail worth logging when a poll did **not** go cleanly, or `None` when it did.
    ///
    /// `Fresh` and `NotModified` are the two expected outcomes, so they stay silent — logging
    /// them on every cycle is what buried the one line that mattered. Every other variant names
    /// what went wrong and carries the same message the tooltip would show, so a non-OK response
    /// or a transport failure is never swallowed the way the GraphQL `FORBIDDEN` was: the field
    /// error rides in on `Transient`'s string.
    pub fn problem(&self) -> Option<String> {
        match self {
            PollResult::Fresh { .. } | PollResult::NotModified => None,
            PollResult::Unauthorized => Some("token rejected by GitHub (401)".to_string()),
            PollResult::RateLimited { retry_after } => {
                Some(format!("rate limited — holding for {}s", retry_after.as_secs()))
            }
            PollResult::Transient(detail) => Some(detail.clone()),
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

/// Polls a pull-request search, reading nothing but its `total_count`.
///
/// Named for the axis it was written for, and now shared with the approved axis, which stopped needing
/// anything about the individual hits — see `scheduler::pr_endpoint`.
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

/// The GraphQL document behind both axes that judge pull requests by their reviews.
///
/// Search's `review:` qualifier is not a view of the reviews. It is a projection of `reviewDecision`,
/// GitHub's verdict on whether the base branch's *review policy* is satisfied, and a repository with no
/// such policy gets no verdict: `reviewDecision` stays `null` on every pull request, approved or not.
/// Measured on 2026-09-05 across a repository with no required-review rule: 100 of its last 100 pull
/// requests reported `null`, six of them carrying a genuine `APPROVED` review on the head commit, and
/// `review:approved` matched none of them. The PR page still paints its green "Approved" badge, because
/// the page reads the reviews. So this document reads what the page reads: `latestOpinionatedReviews`,
/// one verdict per reviewer, `COMMENTED` already dropped.
///
/// The changes-requested axis needs one list more. Re-requesting a review does not dismiss the reviewer's
/// earlier `CHANGES_REQUESTED` verdict — it only puts a pending request back on them — so the verdict
/// alone cannot tell "still on me" from "handed back". `reviewRequests` can, and Search has no qualifier
/// for it at all. Hence GraphQL for both: the axis's Search query string handed to `search` verbatim, plus
/// the lists needed to judge each hit client-side.
const PR_REVIEWS_DOCUMENT: &str = "\
query($q:String!,$hits:Int!,$reviews:Int!){\
  search(query:$q,type:ISSUE,first:$hits){\
    nodes{...on PullRequest{\
      url \
      latestOpinionatedReviews(first:$reviews){nodes{state author{login}}}\
      reviewRequests(first:$reviews){nodes{requestedReviewer{__typename ...on User{login}}}}\
    }}\
  }\
}";

/// Polls the user's own pull requests where a reviewer requested changes and the work is *still on them*.
///
/// `query` is the same Search query string the other axes use, handed to GraphQL's `search` verbatim, so
/// the server-side filter is unchanged and only the client-side intersection is new.
///
/// No `If-None-Match`: GraphQL is a POST and does not answer `304`.
pub fn poll_changes_requested(client: &Client, token: &str, query: &str) -> PollResponse {
    poll_reviewed(client, token, query, parse_changes_requested)
}

/// Polls the user's own open pull requests and counts the ones a reviewer approved.
///
/// `query` must *not* carry `review:approved`: that qualifier is the `reviewDecision` projection this
/// axis exists to get away from (see `PR_REVIEWS_DOCUMENT`). The server narrows to the user's open,
/// non-draft pull requests; `approved` judges each hit by its reviews.
pub fn poll_approved(client: &Client, token: &str, query: &str) -> PollResponse {
    poll_reviewed(client, token, query, parse_approved)
}

/// The shared GraphQL request behind `poll_changes_requested` and `poll_approved`: same document, same
/// variables, one parser apiece.
fn poll_reviewed(client: &Client, token: &str, query: &str, parse_ok: BodyParser) -> PollResponse {
    let body = serde_json::json!({
        "query": PR_REVIEWS_DOCUMENT,
        "variables": { "q": query, "hits": SEARCH_HITS_CAP, "reviews": CHANGES_REVIEWS_CAP },
    });
    let request = client.post(GRAPHQL_URL).json(&body);

    send(request, token, None, parse_ok)
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

    PollResponse { result: classify_with(status, &headers, &body, unix_now(), parse_ok), poll_interval }
}

/// Success-body parser for `/notifications`: presence only, no count.
fn parse_notifications(body: &str) -> Result<Parsed, String> {
    serde_json::from_str::<Vec<Notification>>(body)
        .map(|list| Parsed { present: !list.is_empty(), count: None, urls: None })
        // Previously `.unwrap_or_default()`, which turned a garbled payload into "no unread".
        .map_err(|e| format!("unparseable notification payload: {e}"))
}

/// Success-body parser for `/search/issues`: exact count, so the tooltip can quote it.
fn parse_search_total(body: &str) -> Result<Parsed, String> {
    serde_json::from_str::<SearchResult>(body)
        .map(|r| Parsed { present: r.total_count > 0, count: Some(r.total_count), urls: None })
        .map_err(|e| format!("unparseable search payload: {e}"))
}

// ─── The changes-requested payload ────────────────────────────────────────────
//
// Every level is optional or lenient on purpose. GraphQL is free to answer with partial `data`
// alongside `errors`, and a node the token cannot fully see comes back with fields missing rather
// than as a failure. Each such hole is read as "no evidence this was handed back", which keeps the
// bar lit — see `still_on_you` for why that direction is the safe one.

#[derive(Debug, Deserialize)]
struct GraphQlResponse {
    #[serde(default)]
    data: Option<GraphQlData>,
    #[serde(default)]
    errors: Vec<GraphQlError>,
}

/// Only the message is read.
///
/// GitHub also sends a `type` (`FORBIDDEN`, `RATE_LIMITED`, …) and a `path` naming the response node
/// the error applies to. The merge-ready axis needed both, to tell "this one hit is unjudgeable" from
/// "the query broke" and drop the hit by index. That axis no longer reads its hits at all, and for the
/// one axis left any error is fatal to the poll, so there is nothing left to sort them by. `serde`
/// ignores unknown fields, so both simply go unread.
#[derive(Debug, Deserialize)]
struct GraphQlError {
    message: String,
}

#[derive(Debug, Deserialize)]
struct GraphQlData {
    search: SearchConnection,
}

#[derive(Debug, Deserialize)]
struct SearchConnection {
    nodes: Vec<Option<PullRequestNode>>,
}

/// A search hit. Both fields are `Option` because a node that is not a pull request matches the
/// inline fragment with an empty object — `is:pr` should prevent that, but the type system is a
/// cheaper guarantee than the query string.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PullRequestNode {
    /// The PR's own web page. `Option` for the same lenience as everything else here: a hole
    /// in the payload must degrade the menu entry (fall back to the search page), not the count.
    url: Option<String>,
    latest_opinionated_reviews: Option<ReviewConnection>,
    review_requests: Option<RequestConnection>,
}

#[derive(Debug, Deserialize)]
struct ReviewConnection {
    nodes: Vec<Option<Review>>,
}

#[derive(Debug, Deserialize)]
struct Review {
    state: String,
    /// `None` for a review whose author has since been deleted.
    author: Option<Author>,
}

#[derive(Debug, Deserialize)]
struct Author {
    login: String,
}

#[derive(Debug, Deserialize)]
struct RequestConnection {
    nodes: Vec<Option<ReviewRequest>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewRequest {
    requested_reviewer: Option<RequestedReviewer>,
}

#[derive(Debug, Deserialize)]
struct RequestedReviewer {
    #[serde(rename = "__typename")]
    typename: String,
    /// Present for a `User`; absent for a `Team`, which is the whole reason `typename` is read.
    login: Option<String>,
}

/// Whether this pull request is still waiting on *you* rather than on a reviewer.
///
/// The rule: a pending review request from someone who actually requested changes means you handed it
/// back. A pending request from anyone else does not — adding a fresh reviewer while the original
/// blocker's objection stands leaves the work with you.
///
/// Both fallbacks below return `true`, i.e. keep the bar lit. A lit bar that should be dark costs a
/// glance; a dark bar that should be lit hides work you owe someone, which is the failure this whole
/// module is written to avoid.
fn still_on_you(pr: &PullRequestNode) -> bool {
    let blockers: Vec<&str> = pr
        .latest_opinionated_reviews
        .iter()
        .flat_map(|c| c.nodes.iter().flatten())
        .filter(|review| review.state == "CHANGES_REQUESTED")
        .filter_map(|review| review.author.as_ref().map(|a| a.login.as_str()))
        .collect();

    // GitHub matched `review:changes_requested` but names nobody we can intersect against — a deleted
    // account, or a review list truncated by the cap. Trust the server's verdict over our own reading.
    if blockers.is_empty() {
        return true;
    }

    let handed_back = pr
        .review_requests
        .iter()
        .flat_map(|c| c.nodes.iter().flatten())
        .filter_map(|request| request.requested_reviewer.as_ref())
        // A pending *team* request has no login to match. Resolving membership would cost extra
        // requests and org-level permissions to settle a case that barely occurs, since re-requesting
        // a review re-requests the individual. Treated as "not handed back".
        .filter(|reviewer| reviewer.typename == "User")
        .filter_map(|reviewer| reviewer.login.as_deref())
        .any(|login| blockers.contains(&login));

    !handed_back
}

/// Whether a reviewer approved this pull request and nobody's objection stands against it.
///
/// The rule: at least one `APPROVED` among the latest opinionated reviews, and no `CHANGES_REQUESTED`.
/// The veto is what keeps this axis and the changes-requested one disjoint now that neither is filtered
/// server-side, and it is GitHub's own precedence: one reviewer's yes does not outrank another's no.
/// Approvals on a superseded commit count — the PR page shows them, and this bar reports news, it does
/// not gate a merge.
///
/// The fallback direction is the *opposite* of `still_on_you`'s. That predicate narrows a list the server
/// already vouched for, so a hole in the payload leaves the bar lit. This one is the only filter there
/// is: the server hands over every open pull request, and a missing or empty review list is simply no
/// evidence of approval. Lighting a green bar over a PR nobody approved would be the false claim.
fn approved(pr: &PullRequestNode) -> bool {
    let mut any_approved = false;
    for review in pr.latest_opinionated_reviews.iter().flat_map(|c| c.nodes.iter().flatten()) {
        match review.state.as_str() {
            "CHANGES_REQUESTED" => return false,
            "APPROVED" => any_approved = true,
            _ => {}
        }
    }
    any_approved
}

/// Success-body parser for the changes-requested GraphQL query: `still_on_you` decides each hit.
fn parse_changes_requested(body: &str) -> Result<Parsed, String> {
    parse_reviewed(body, still_on_you)
}

/// Success-body parser for the approved GraphQL query: `approved` decides each hit.
fn parse_approved(body: &str) -> Result<Parsed, String> {
    parse_reviewed(body, approved)
}

/// Shared body of the two GraphQL parsers: one `PR_REVIEWS_DOCUMENT` answer, one predicate per axis.
///
/// GraphQL answers `200 OK` and puts failures in an `errors` array, so `classify_with` cannot see them
/// from the status line. A parser that read only `data` would turn any such failure into a confident
/// **zero** — a dark bar meaning "the request broke". Errors are therefore checked before anything else
/// and surface as `Err`, which becomes `Transient`, which leaves the previous count standing.
fn parse_reviewed(body: &str, keep: fn(&PullRequestNode) -> bool) -> Result<Parsed, String> {
    let response: GraphQlResponse =
        serde_json::from_str(body).map_err(|e| format!("unparseable PR review payload: {e}"))?;

    if !response.errors.is_empty() {
        let joined =
            response.errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>().join("; ");
        return Err(format!("GraphQL reported an error: {joined}"));
    }

    let search = response
        .data
        .ok_or_else(|| "GraphQL answered with neither data nor errors".to_string())?
        .search;

    let counted: Vec<&PullRequestNode> = search.nodes.iter().flatten().filter(|pr| keep(pr)).collect();
    let count = counted.len() as u32;
    // All-or-nothing: a counted hit without a URL would make the menu entry open fewer pages
    // than the dot claims, so one hole sends the whole click to the search-page fallback.
    let urls: Option<Vec<String>> = counted.iter().map(|pr| pr.url.clone()).collect();
    Ok(Parsed { present: count > 0, count: Some(count), urls })
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
fn classify_with(
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
        return match rate_limit_wait(headers, body, now) {
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
        Ok(parsed) => PollResult::Fresh {
            present: parsed.present,
            count: parsed.count,
            urls: parsed.urls,
            etag: header_string(headers, ETAG.as_str()),
        },
        Err(why) => PollResult::Transient(why),
    }
}

/// GitHub's documented precedence for how long to wait after being limited.
///
/// Returns `None` when the response carries no rate-limit signal at all, which is how the
/// caller distinguishes "slow down" from an unrelated 403.
///
/// The body is read, not just the headers, because a **secondary** rate limit can arrive with
/// neither `retry-after` nor an exhausted primary quota — the quota headers still say there is
/// plenty left, and the only thing naming the limit is the JSON message. Read from headers alone
/// that response looks like a permission failure, which is a `Transient` — and three of those in
/// a row make every axis give up its last known count and blank the tray. Holding the count is
/// the whole point: a secondary limit says "you asked too fast", never "there is nothing there".
fn rate_limit_wait(headers: &HeaderMap, body: &str, now: u64) -> Option<Duration> {
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

    // 3. A secondary limit that said so only in the body. Both wordings are matched: GitHub
    //    renamed "abuse detection mechanism" to "secondary rate limit" and still emits the old
    //    text from some endpoints. Lowercased first so a change of capitalisation cannot silently
    //    turn this back into a blanked tray.
    if is_secondary_rate_limit(body) {
        return Some(DEFAULT_RATE_LIMIT_WAIT);
    }

    None
}

/// Whether a 403/429 body is GitHub saying "too fast" rather than "not allowed".
fn is_secondary_rate_limit(body: &str) -> bool {
    let body = body.to_ascii_lowercase();
    body.contains("secondary rate limit") || body.contains("abuse detection mechanism")
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
            PollResult::Fresh { present, etag, count, .. } => {
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

    // ── Changes-requested body parsing ────────────────────────────────────────

    /// Shorthand: classify a CHANGES-REQUESTED (GraphQL) response.
    fn changes(status: StatusCode, h: &HeaderMap, body: &str, now: u64) -> PollResult {
        classify_with(status, h, body, now, parse_changes_requested)
    }

    /// One search hit, described by who blocked it and who has a re-review pending.
    ///
    /// `blockers` are logins whose latest opinionated review requested changes; `pending` are
    /// `(typename, login)` pairs for the pending review requests, so a `Team` can be expressed.
    fn hit(blockers: &[&str], pending: &[(&str, &str)]) -> String {
        let reviews = blockers
            .iter()
            .map(|l| format!(r#"{{"state":"CHANGES_REQUESTED","author":{{"login":"{l}"}}}}"#))
            .collect::<Vec<_>>()
            .join(",");
        let requests = pending
            .iter()
            .map(|(kind, login)| {
                let reviewer = if *kind == "Team" {
                    format!(r#"{{"__typename":"Team","name":"{login}"}}"#)
                } else {
                    format!(r#"{{"__typename":"User","login":"{login}"}}"#)
                };
                format!(r#"{{"requestedReviewer":{reviewer}}}"#)
            })
            .collect::<Vec<_>>()
            .join(",");
        format!(
            r#"{{"latestOpinionatedReviews":{{"nodes":[{reviews}]}},"reviewRequests":{{"nodes":[{requests}]}}}}"#
        )
    }

    /// A hit that also carries its web URL, the way the real payload does.
    fn hit_at(url: &str, blockers: &[&str], pending: &[(&str, &str)]) -> String {
        let bare = hit(blockers, pending);
        format!(r#"{{"url":"{url}",{}"#, &bare[1..])
    }

    fn payload(hits: &[String]) -> String {
        format!(r#"{{"data":{{"search":{{"nodes":[{}]}}}}}}"#, hits.join(","))
    }

    fn count_of(body: &str) -> u32 {
        match changes(StatusCode::OK, &headers(&[]), body, 0) {
            PollResult::Fresh { count: Some(n), .. } => n,
            other => panic!("expected Fresh with a count, got {:?}", other),
        }
    }

    /// The bug this whole endpoint switch exists for: changes applied, review re-requested, and the
    /// bar stayed lit because `review:changes_requested` still matched.
    #[test]
    fn a_re_review_pending_from_the_blocker_is_not_on_you() {
        let body = payload(&[hit(&["alice"], &[("User", "alice")])]);
        match changes(StatusCode::OK, &headers(&[]), &body, 0) {
            PollResult::Fresh { present, count, .. } => {
                assert!(!present, "handed back to the reviewer, so nothing is on you");
                assert_eq!(count, Some(0));
            }
            other => panic!("expected Fresh, got {:?}", other),
        }
    }

    #[test]
    fn changes_requested_with_nothing_pending_is_still_on_you() {
        assert_eq!(count_of(&payload(&[hit(&["alice"], &[])])), 1);
    }

    /// Adding a fresh reviewer is not the same as addressing the blocker's objection.
    #[test]
    fn a_pending_request_for_someone_else_leaves_it_on_you() {
        assert_eq!(count_of(&payload(&[hit(&["alice"], &[("User", "bob")])])), 1);
    }

    /// A team has no login to intersect, so it cannot prove the blocker was asked again.
    #[test]
    fn a_pending_team_request_does_not_count_as_handing_it_back() {
        assert_eq!(count_of(&payload(&[hit(&["alice"], &[("Team", "backend")])])), 1);
    }

    /// GitHub matched the query but named nobody we can check. Its verdict wins over our reading.
    #[test]
    fn a_hit_with_no_identifiable_blocker_stays_counted() {
        assert_eq!(count_of(&payload(&[hit(&[], &[("User", "alice")])])), 1);
        assert_eq!(count_of(&payload(&[r#"{}"#.to_string()])), 1, "a node with no fields at all");
    }

    #[test]
    fn several_blockers_need_all_of_them_asked_again() {
        let one_of_two = payload(&[hit(&["alice", "bob"], &[("User", "alice")])]);
        assert_eq!(
            count_of(&one_of_two),
            0,
            "any blocker asked again means the ball has moved, even if others also objected"
        );
    }

    #[test]
    fn a_mixed_page_counts_only_the_ones_still_on_you() {
        let body = payload(&[
            hit(&["alice"], &[("User", "alice")]), // handed back
            hit(&["bob"], &[]),                    // on you
            hit(&["carol"], &[("User", "dave")]),  // on you: wrong reviewer asked
            hit(&["erin"], &[("Team", "core")]),   // on you: team request proves nothing
        ]);
        assert_eq!(count_of(&body), 3);
    }

    /// The gotcha that makes this endpoint different from the other two: GraphQL reports failure with
    /// `200 OK` and an `errors` array. Reading only `data` would render a broken request as a
    /// confident zero — the one answer this module must never give.
    #[test]
    fn a_graphql_error_at_status_200_is_transient_not_zero() {
        let body = r#"{"data":null,"errors":[{"message":"Something went wrong"}]}"#;
        match changes(StatusCode::OK, &headers(&[]), body, 0) {
            PollResult::Transient(why) => assert!(why.contains("Something went wrong")),
            other => panic!("expected Transient, got {:?}", other),
        }
    }

    /// Partial success is still failure: an `errors` array alongside usable `data` means the page we
    /// were handed is incomplete, so counting it would undercount.
    #[test]
    fn errors_win_even_when_data_is_present() {
        let body = format!(
            r#"{{"data":{{"search":{{"nodes":[{}]}}}},"errors":[{{"message":"partial"}}]}}"#,
            hit(&["alice"], &[])
        );
        assert!(matches!(changes(StatusCode::OK, &headers(&[]), &body, 0), PollResult::Transient(_)));
    }

    #[test]
    fn a_response_with_neither_data_nor_errors_is_transient() {
        assert!(matches!(
            changes(StatusCode::OK, &headers(&[]), "{}", 0),
            PollResult::Transient(_)
        ));
    }

    #[test]
    fn malformed_changes_requested_body_is_transient() {
        assert!(matches!(
            changes(StatusCode::OK, &headers(&[]), "not json at all", 0),
            PollResult::Transient(_)
        ));
    }

    /// An empty page is a real answer, unlike every failure above.
    #[test]
    fn no_hits_means_nothing_is_on_you() {
        match changes(StatusCode::OK, &headers(&[]), &payload(&[]), 0) {
            PollResult::Fresh { present, count, .. } => {
                assert!(!present);
                assert_eq!(count, Some(0));
            }
            other => panic!("expected Fresh, got {:?}", other),
        }
    }

    /// The click target follows the count: only the still-on-you hits' URLs are handed out, in
    /// the order GitHub returned them, so the menu entry opens exactly what the dot claims.
    #[test]
    fn urls_are_collected_for_exactly_the_counted_hits() {
        let body = payload(&[
            hit_at("https://github.com/o/r/pull/1", &["alice"], &[("User", "alice")]), // handed back
            hit_at("https://github.com/o/r/pull/2", &["bob"], &[]),                    // on you
            hit_at("https://github.com/o/r/pull/3", &["carol"], &[("User", "dave")]),  // on you
        ]);
        match changes(StatusCode::OK, &headers(&[]), &body, 0) {
            PollResult::Fresh { count, urls, .. } => {
                assert_eq!(count, Some(2));
                assert_eq!(
                    urls,
                    Some(vec![
                        "https://github.com/o/r/pull/2".to_string(),
                        "https://github.com/o/r/pull/3".to_string(),
                    ])
                );
            }
            other => panic!("expected Fresh, got {:?}", other),
        }
    }

    /// One counted hit without a URL poisons the whole list: opening two pages under a dot that
    /// says three would be the count and the click disagreeing — the fallback page is honest.
    #[test]
    fn a_counted_hit_without_a_url_yields_no_list_but_keeps_the_count() {
        let body = payload(&[
            hit_at("https://github.com/o/r/pull/2", &["bob"], &[]),
            hit(&["carol"], &[]), // counted, but the payload hole ate its URL
        ]);
        match changes(StatusCode::OK, &headers(&[]), &body, 0) {
            PollResult::Fresh { count, urls, .. } => {
                assert_eq!(count, Some(2));
                assert_eq!(urls, None);
            }
            other => panic!("expected Fresh, got {:?}", other),
        }
    }

    /// A confirmed-empty page is `Some(vec![])`, not `None` — the difference between "no PRs"
    /// and "could not read the list", which the menu fallback relies on.
    #[test]
    fn no_hits_yields_a_confirmed_empty_url_list() {
        match changes(StatusCode::OK, &headers(&[]), &payload(&[]), 0) {
            PollResult::Fresh { urls, .. } => assert_eq!(urls, Some(vec![])),
            other => panic!("expected Fresh, got {:?}", other),
        }
    }

    /// The two search-total endpoints have no per-hit view, so they must never claim one.
    #[test]
    fn search_totals_carry_no_urls() {
        match search(StatusCode::OK, &headers(&[]), r#"{"total_count":7,"items":[{}]}"#, 0) {
            PollResult::Fresh { urls, .. } => assert_eq!(urls, None),
            other => panic!("expected Fresh, got {:?}", other),
        }
    }

    /// The document has to fetch the field the parser reads, or every hit loses its URL and the
    /// menu entry silently degrades to the search page forever.
    #[test]
    fn the_query_document_fetches_the_url() {
        assert!(PR_REVIEWS_DOCUMENT.contains("url"));
    }

    /// The document has to name every variable it uses, or GitHub rejects the whole query.
    #[test]
    fn the_query_document_declares_the_variables_it_sends() {
        for var in ["$q", "$hits", "$reviews"] {
            assert!(
                PR_REVIEWS_DOCUMENT.matches(var).count() >= 2,
                "{var} should be both declared and used"
            );
        }
    }

    // ── Approved body parsing ─────────────────────────────────────────────────

    /// Shorthand: classify an APPROVED (GraphQL) response.
    fn approved_resp(status: StatusCode, h: &HeaderMap, body: &str, now: u64) -> PollResult {
        classify_with(status, h, body, now, parse_approved)
    }

    /// One search hit with the given latest opinionated review states, each from a distinct reviewer.
    fn reviewed(url: &str, states: &[&str]) -> String {
        let reviews = states
            .iter()
            .enumerate()
            .map(|(i, s)| format!(r#"{{"state":"{s}","author":{{"login":"r{i}"}}}}"#))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            r#"{{"url":"{url}","latestOpinionatedReviews":{{"nodes":[{reviews}]}},"reviewRequests":{{"nodes":[]}}}}"#
        )
    }

    /// The case that motivated the axis: a real approval in a repository where `reviewDecision` is
    /// `null`, so `review:approved` never matched it. Read off the reviews, it counts.
    #[test]
    fn an_approval_lights_the_bar_and_hands_out_its_url() {
        let body = payload(&[reviewed("https://github.com/o/r/pull/2204", &["APPROVED"])]);
        match approved_resp(StatusCode::OK, &headers(&[]), &body, 0) {
            PollResult::Fresh { present, count, urls, .. } => {
                assert!(present);
                assert_eq!(count, Some(1));
                assert_eq!(urls, Some(vec!["https://github.com/o/r/pull/2204".to_string()]));
            }
            other => panic!("expected Fresh, got {:?}", other),
        }
    }

    /// One reviewer's yes does not outrank another's no. This is also what keeps the green and amber
    /// bars disjoint now that neither is filtered server-side.
    #[test]
    fn a_standing_objection_vetoes_an_approval() {
        let body = payload(&[
            reviewed("https://github.com/o/r/pull/1", &["APPROVED", "CHANGES_REQUESTED"]),
            reviewed("https://github.com/o/r/pull/2", &["CHANGES_REQUESTED", "APPROVED"]),
            reviewed("https://github.com/o/r/pull/3", &["APPROVED", "APPROVED"]),
        ]);
        match approved_resp(StatusCode::OK, &headers(&[]), &body, 0) {
            PollResult::Fresh { count, urls, .. } => {
                assert_eq!(count, Some(1), "only the unanimously approved PR counts");
                assert_eq!(urls, Some(vec!["https://github.com/o/r/pull/3".to_string()]));
            }
            other => panic!("expected Fresh, got {:?}", other),
        }
    }

    /// The server hands over *every* open PR, so this predicate is the only filter there is. Anything
    /// short of an `APPROVED` verdict — no reviews, a review list missing from the payload, or a state
    /// this parser does not know — must read as "not approved", the reverse of `still_on_you`'s lenience.
    #[test]
    fn without_an_approved_verdict_nothing_is_counted() {
        let body = payload(&[
            reviewed("https://github.com/o/r/pull/1", &[]),
            reviewed("https://github.com/o/r/pull/2", &["COMMENTED"]),
            reviewed("https://github.com/o/r/pull/3", &["DISMISSED"]),
            r#"{"url":"https://github.com/o/r/pull/4"}"#.to_string(),
        ]);
        match approved_resp(StatusCode::OK, &headers(&[]), &body, 0) {
            PollResult::Fresh { present, count, urls, .. } => {
                assert!(!present);
                assert_eq!(count, Some(0));
                assert_eq!(urls, Some(vec![]), "confirmed empty, not unreadable");
            }
            other => panic!("expected Fresh, got {:?}", other),
        }
    }

    /// Same error path as the changes-requested parser: a GraphQL-level failure must hold the last
    /// count rather than darken the bar.
    #[test]
    fn approved_graphql_errors_are_transient() {
        let body = r#"{"data":null,"errors":[{"message":"API rate limit exceeded"}]}"#;
        assert!(matches!(
            approved_resp(StatusCode::OK, &headers(&[]), body, 0),
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

    /// The real 403 that blanked the tray: a secondary limit, no `retry-after`, and quota headers
    /// that still look healthy. Header-only classification called this a permission failure.
    #[test]
    fn secondary_rate_limit_in_the_body_is_rate_limited_not_transient() {
        let h = headers(&[("x-ratelimit-remaining", "4998")]);
        let body = r#"{"message":"You have exceeded a secondary rate limit. Please wait a few minutes before you try again.","documentation_url":"https://docs.github.com/rest/overview/rate-limits-for-the-rest-api"}"#;
        match search(StatusCode::FORBIDDEN, &h, body, 0) {
            PollResult::RateLimited { retry_after } => assert_eq!(retry_after, DEFAULT_RATE_LIMIT_WAIT),
            other => panic!("expected RateLimited, got {:?}", other),
        }
        assert!(matches!(
            notif(StatusCode::FORBIDDEN, &h, body, 0),
            PollResult::RateLimited { .. }
        ));
    }

    /// GitHub's older wording for the same thing, still emitted by some endpoints.
    #[test]
    fn abuse_detection_wording_is_rate_limited() {
        let body = r#"{"message":"You have triggered an abuse detection mechanism."}"#;
        assert!(matches!(
            search(StatusCode::FORBIDDEN, &headers(&[]), body, 0),
            PollResult::RateLimited { .. }
        ));
    }

    /// An explicit `retry-after` still outranks the body's generic minute.
    #[test]
    fn retry_after_outranks_the_secondary_limit_default() {
        let h = headers(&[("retry-after", "120")]);
        match search(StatusCode::FORBIDDEN, &h, "secondary rate limit", 0) {
            PollResult::RateLimited { retry_after } => assert_eq!(retry_after, Duration::from_secs(120)),
            other => panic!("expected RateLimited, got {:?}", other),
        }
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
