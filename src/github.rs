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
const AGENT: &str = "git-system-tray";

/// How many search hits a GraphQL poll inspects (shared by the changes-requested and merge-ready
/// axes).
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

/// The one query that cannot be a `total_count` read.
///
/// Re-requesting a review does not dismiss the reviewer's earlier `CHANGES_REQUESTED` verdict — it only
/// puts a pending request back on them. So GitHub keeps reporting `reviewDecision: CHANGES_REQUESTED`, and
/// `review:changes_requested` keeps matching, for a pull request whose ball is squarely in the reviewer's
/// court. Measured, not assumed: see the tests below for the exact payload shape.
///
/// The only field that separates "still on me" from "handed back" is `reviewRequests`, and Search has no
/// qualifier for it — `review:` accepts only `none`/`required`/`approved`/`changes_requested`, which are
/// mutually exclusive projections of the same `reviewDecision`. Hence GraphQL: the same server-side query
/// string, plus the two lists needed to intersect client-side.
const CHANGES_REQUESTED_DOCUMENT: &str = "\
query($q:String!,$hits:Int!,$reviews:Int!){\
  search(query:$q,type:ISSUE,first:$hits){\
    nodes{...on PullRequest{\
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
    let body = serde_json::json!({
        "query": CHANGES_REQUESTED_DOCUMENT,
        "variables": { "q": query, "hits": SEARCH_HITS_CAP, "reviews": CHANGES_REVIEWS_CAP },
    });
    let request = client.post(GRAPHQL_URL).json(&body);

    send(request, token, None, parse_changes_requested)
}

/// The "ready to merge" query cannot be a plain `total_count` read either.
///
/// The Search API has no qualifier for check health: `status:success` reads only GitHub's *legacy*
/// combined commit status, which is empty for repos whose CI is entirely check runs (GitHub Actions
/// and friends) — there it matches nothing and hides every genuinely mergeable pull request. GraphQL
/// exposes the real signal via `statusCheckRollup.state`, which aggregates check runs *and* legacy
/// statuses. So the same server-side search string is handed to GraphQL, and the check-health gate is
/// applied client-side in `checks_ready` — the same split the changes-requested axis already uses.
const MERGE_READY_DOCUMENT: &str = "\
query($q:String!,$hits:Int!){\
  search(query:$q,type:ISSUE,first:$hits){\
    nodes{...on PullRequest{\
      commits(last:1){nodes{commit{statusCheckRollup{state}}}}\
    }}\
  }\
}";

/// Polls the user's own pull requests that are approved *and* whose checks are healthy.
///
/// `query` is the same Search query string the other axes use (approval, open, not-draft handled
/// server-side); the check-health filter is client-side, so it works for check-run-only repos where
/// the Search `status:` qualifier reads empty. No `If-None-Match`: GraphQL is a POST and does not 304.
pub fn poll_merge_ready(client: &Client, token: &str, query: &str) -> PollResponse {
    let body = serde_json::json!({
        "query": MERGE_READY_DOCUMENT,
        "variables": { "q": query, "hits": SEARCH_HITS_CAP },
    });
    let request = client.post(GRAPHQL_URL).json(&body);

    send(request, token, None, parse_merge_ready)
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
    latest_opinionated_reviews: Option<ReviewConnection>,
    review_requests: Option<RequestConnection>,
    /// Only requested by the merge-ready query; absent (`None`) in changes-requested responses.
    commits: Option<CommitConnection>,
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

// ─── The merge-ready payload ──────────────────────────────────────────────────
//
// Every level is optional, for the same reason as the changes-requested structs: GraphQL can answer
// with partial data, and a repo with no checks at all returns a null `statusCheckRollup`. See
// `checks_ready` for how an absent rollup is read.

#[derive(Debug, Deserialize)]
struct CommitConnection {
    nodes: Vec<Option<CommitNode>>,
}

#[derive(Debug, Deserialize)]
struct CommitNode {
    commit: Option<Commit>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Commit {
    /// `None` when the repo has no checks configured at all — GitHub returns a null rollup, not an
    /// empty one.
    status_check_rollup: Option<StatusCheckRollup>,
}

#[derive(Debug, Deserialize)]
struct StatusCheckRollup {
    /// One of `SUCCESS`, `PENDING`, `FAILURE`, `ERROR`, `EXPECTED`.
    state: String,
}

/// Whether this pull request's checks are healthy enough to call it ready to merge.
///
/// Ready means the rollup is `SUCCESS`, or there is no rollup at all — a repo with no checks
/// configured has nothing that can fail, and because the search is scoped to `author:@me` an absent
/// rollup means "no checks", never "not allowed to see them". Everything else (`FAILURE`, `ERROR`,
/// a still-running `PENDING`/`EXPECTED`, or any state we do not recognise) is not ready.
///
/// The safe direction here is the *opposite* of `still_on_you`. A green bar that should be dark
/// claims a pull request is mergeable while its checks are red — the exact false signal this axis
/// exists to kill — so anything short of a clear success is dropped.
fn checks_ready(pr: &PullRequestNode) -> bool {
    let rollup_state = pr
        .commits
        .as_ref()
        .and_then(|c| c.nodes.iter().flatten().next())
        .and_then(|node| node.commit.as_ref())
        .and_then(|commit| commit.status_check_rollup.as_ref())
        .map(|rollup| rollup.state.as_str());

    match rollup_state {
        None => true,             // no checks configured: nothing to fail
        Some("SUCCESS") => true,
        Some(_) => false,
    }
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

/// Success-body parser for the changes-requested GraphQL query.
///
/// GraphQL answers `200 OK` and puts failures in an `errors` array, so `classify_with` cannot see them
/// from the status line. A parser that read only `data` would turn any such failure into a confident
/// **zero** — a dark bar meaning "the request broke". Errors are therefore checked before anything else
/// and surface as `Err`, which becomes `Transient`, which leaves the previous count standing.
fn parse_changes_requested(body: &str) -> Result<(bool, Option<u32>), String> {
    let response: GraphQlResponse = serde_json::from_str(body)
        .map_err(|e| format!("unparseable changes-requested payload: {e}"))?;

    if !response.errors.is_empty() {
        let joined =
            response.errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>().join("; ");
        return Err(format!("GraphQL reported an error: {joined}"));
    }

    let search = response
        .data
        .ok_or_else(|| "GraphQL answered with neither data nor errors".to_string())?
        .search;

    let count = search.nodes.iter().flatten().filter(|pr| still_on_you(pr)).count() as u32;
    Ok((count > 0, Some(count)))
}

/// Success-body parser for the merge-ready GraphQL query.
///
/// Same shape as `parse_changes_requested`, and for the same reason: GraphQL reports failure with a
/// `200 OK` and an `errors` array, so errors are checked before `data`. A parser reading only `data`
/// would turn a broken request into a confident **zero**, darkening the bar for a PR that is actually
/// ready. Only the client-side filter differs — `checks_ready` instead of `still_on_you`.
fn parse_merge_ready(body: &str) -> Result<(bool, Option<u32>), String> {
    let response: GraphQlResponse = serde_json::from_str(body)
        .map_err(|e| format!("unparseable merge-ready payload: {e}"))?;

    if !response.errors.is_empty() {
        let joined =
            response.errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>().join("; ");
        return Err(format!("GraphQL reported an error: {joined}"));
    }

    let search = response
        .data
        .ok_or_else(|| "GraphQL answered with neither data nor errors".to_string())?
        .search;

    let count = search.nodes.iter().flatten().filter(|pr| checks_ready(pr)).count() as u32;
    Ok((count > 0, Some(count)))
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

    /// The document has to name every variable it uses, or GitHub rejects the whole query.
    #[test]
    fn the_query_document_declares_the_variables_it_sends() {
        for var in ["$q", "$hits", "$reviews"] {
            assert!(
                CHANGES_REQUESTED_DOCUMENT.matches(var).count() >= 2,
                "{var} should be both declared and used"
            );
        }
    }

    // ── Merge-ready body parsing ──────────────────────────────────────────────

    /// Shorthand: classify a MERGE-READY (GraphQL) response.
    fn merge(status: StatusCode, h: &HeaderMap, body: &str, now: u64) -> PollResult {
        classify_with(status, h, body, now, parse_merge_ready)
    }

    /// A single merge-ready search hit whose head commit carries `state` as its check rollup.
    /// `None` renders a null rollup — GitHub's answer for a repo with no checks configured.
    fn mrhit(state: Option<&str>) -> String {
        let rollup = match state {
            Some(s) => format!(r#"{{"state":"{s}"}}"#),
            None => "null".to_string(),
        };
        format!(r#"{{"commits":{{"nodes":[{{"commit":{{"statusCheckRollup":{rollup}}}}}]}}}}"#)
    }

    fn merge_count(body: &str) -> u32 {
        match merge(StatusCode::OK, &headers(&[]), body, 0) {
            PollResult::Fresh { count: Some(n), .. } => n,
            other => panic!("expected Fresh with a count, got {:?}", other),
        }
    }

    #[test]
    fn a_success_rollup_is_ready() {
        assert_eq!(merge_count(&payload(&[mrhit(Some("SUCCESS"))])), 1);
    }

    /// A repo with no checks has nothing that can fail, so an approved PR there is ready.
    #[test]
    fn an_absent_rollup_is_ready() {
        assert_eq!(merge_count(&payload(&[mrhit(None)])), 1, "null rollup: no checks configured");
        assert_eq!(merge_count(&payload(&[r#"{}"#.to_string()])), 1, "no commits field at all");
    }

    #[test]
    fn a_failing_rollup_is_not_ready() {
        for state in ["FAILURE", "ERROR"] {
            assert_eq!(merge_count(&payload(&[mrhit(Some(state))])), 0, "{state} is not ready");
        }
    }

    /// Still-running checks are not "ready to merge" yet — only a clear success counts.
    #[test]
    fn an_unfinished_rollup_is_not_ready() {
        for state in ["PENDING", "EXPECTED"] {
            assert_eq!(merge_count(&payload(&[mrhit(Some(state))])), 0, "{state} is not ready");
        }
    }

    #[test]
    fn a_mixed_page_counts_only_the_healthy_ones() {
        let body = payload(&[
            mrhit(Some("SUCCESS")), // ready
            mrhit(Some("FAILURE")), // red
            mrhit(None),            // no checks: ready
            mrhit(Some("PENDING")), // still running
        ]);
        assert_eq!(merge_count(&body), 2);
    }

    #[test]
    fn an_empty_merge_page_is_a_clean_zero() {
        match merge(StatusCode::OK, &headers(&[]), &payload(&[]), 0) {
            PollResult::Fresh { present, count, .. } => {
                assert!(!present);
                assert_eq!(count, Some(0));
            }
            other => panic!("expected Fresh, got {:?}", other),
        }
    }

    /// GraphQL reports failure as `200 OK` + an `errors` array; reading only `data` would count a
    /// broken page as a confident zero, darkening the bar for a PR that may well be ready.
    #[test]
    fn a_merge_graphql_error_at_status_200_is_transient_not_zero() {
        let body = r#"{"data":null,"errors":[{"message":"Something went wrong"}]}"#;
        match merge(StatusCode::OK, &headers(&[]), body, 0) {
            PollResult::Transient(why) => assert!(why.contains("Something went wrong")),
            other => panic!("expected Transient, got {:?}", other),
        }
    }

    #[test]
    fn merge_errors_win_even_when_data_is_present() {
        let body = format!(
            r#"{{"data":{{"search":{{"nodes":[{}]}}}},"errors":[{{"message":"partial"}}]}}"#,
            mrhit(Some("SUCCESS"))
        );
        assert!(matches!(merge(StatusCode::OK, &headers(&[]), &body, 0), PollResult::Transient(_)));
    }

    #[test]
    fn a_merge_response_with_neither_data_nor_errors_is_transient() {
        assert!(matches!(merge(StatusCode::OK, &headers(&[]), "{}", 0), PollResult::Transient(_)));
    }

    #[test]
    fn a_malformed_merge_body_is_transient() {
        assert!(matches!(
            merge(StatusCode::OK, &headers(&[]), "not json at all", 0),
            PollResult::Transient(_)
        ));
    }

    #[test]
    fn the_merge_document_declares_the_variables_it_sends() {
        for var in ["$q", "$hits"] {
            assert!(
                MERGE_READY_DOCUMENT.matches(var).count() >= 2,
                "{var} should be both declared and used"
            );
        }
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
