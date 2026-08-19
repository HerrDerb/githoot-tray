//! Whether GitHub itself is having a bad day.
//!
//! ## Why not an HTTP status code
//!
//! The obvious design is to call something on `api.github.com` and treat a non-2xx as an outage. That
//! does not work, and measurably so: while this was being written GitHub was running a real incident —
//! Pull Requests, API Requests, Issues and Actions all `degraded_performance`, one open incident — and
//! every endpoint still answered **200**. A status code tells you whether *your one request* worked, and
//! the app already learns that from `github::PollResult`. It says nothing about GitHub's own view of
//! itself.
//!
//! GitHub publishes that view on a Statuspage instance, so this reads
//! `https://www.githubstatus.com/api/v2/status.json` — 219 bytes, unauthenticated, and on a different
//! host from the API, which is the point: it keeps answering when the API does not.
//!
//! ## What counts as an outage
//!
//! Statuspage's `indicator` is a closed set, ordered by severity: `none`, `minor`, `major`, `critical`
//! (plus `maintenance`). Anything from `minor` upwards raises the mark, because `minor` is what GitHub
//! calls a component at degraded performance — and on the day this was written that meant Pull Requests
//! themselves, which is the app's entire subject. A degradation you cannot see is worse than a mark you
//! see often.
//!
//! `maintenance` is the one exception: scheduled work is expected and announced, so it is not a fault to
//! warn about.
//!
//! ## Why a failure here is not an outage
//!
//! Everywhere else in this codebase, "I could not find out" must never be reported as "there is nothing
//! there". This is the one place the reasoning inverts, because the signal is an **alarm** rather than a
//! count: raising an outage warning because *our own* request to a third-party status page failed would
//! cry wolf on the strength of the user's flaky wifi. So an unreadable answer changes nothing — it is
//! logged, and whatever was last known stands.

use crate::infoln;
use reqwest::blocking::Client;
use serde::Deserialize;

const STATUS_URL: &str = "https://www.githubstatus.com/api/v2/status.json";

/// The page people are sent to, rather than the JSON endpoint above.
pub const STATUS_PAGE_URL: &str = "https://www.githubstatus.com";

/// Indicators that raise the mark. Anything else — `none`, `maintenance`, or an indicator this version
/// has never heard of — deliberately does not, so an unfamiliar value cannot invent an outage.
const DEGRADED_INDICATORS: [&str; 3] = ["minor", "major", "critical"];

/// What GitHub says about itself.
#[derive(Debug, PartialEq, Eq)]
pub enum Health {
    /// Operational, or scheduled maintenance.
    Fine,
    /// `minor`, `major` or `critical`. Carries Statuspage's own wording, so the tooltip quotes GitHub
    /// rather than paraphrasing it — "Partially Degraded Service" is more use than anything this app
    /// would invent.
    Degraded { description: String },
}

#[derive(Debug, Deserialize)]
struct StatusResponse {
    status: StatusBody,
}

#[derive(Debug, Deserialize)]
struct StatusBody {
    indicator: String,
    /// Statuspage always sends this, but a missing one must not cost us the verdict the indicator
    /// already gave.
    #[serde(default)]
    description: Option<String>,
}

/// Asks GitHub how it is doing. `Err` means we could not find out, which is not an outage.
pub fn check(client: &Client) -> Result<Health, String> {
    let response = client
        .get(STATUS_URL)
        .header(reqwest::header::ACCEPT, "application/json")
        .header(reqwest::header::USER_AGENT, "git-system-tray")
        .send()
        .map_err(|e| format!("could not reach the status page: {e}"))?;

    let status = response.status();
    if !status.is_success() {
        // Note this is the status *page* failing, not GitHub. Treated as "unknown", per the module
        // note above.
        return Err(format!("the status page answered {status}"));
    }

    let body = response.text().map_err(|e| format!("could not read the status page: {e}"))?;
    parse(&body)
}

/// Reads one `status.json` body. Split out so every branch is testable without a network.
fn parse(body: &str) -> Result<Health, String> {
    let parsed: StatusResponse =
        serde_json::from_str(body).map_err(|e| format!("unparseable status payload: {e}"))?;

    let indicator = parsed.status.indicator.trim().to_ascii_lowercase();
    if !DEGRADED_INDICATORS.contains(&indicator.as_str()) {
        return Ok(Health::Fine);
    }

    // Falling back to the indicator itself rather than to something invented: whatever ends up in the
    // tooltip should be a word GitHub actually used.
    let description = parsed
        .status
        .description
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| indicator.clone());
    infoln!("GitHub reports {indicator}: {description}");
    Ok(Health::Degraded { description })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(indicator: &str, description: &str) -> String {
        format!(
            r#"{{"page":{{"id":"kctbh9vrtdwd"}},"status":{{"indicator":"{indicator}","description":"{description}"}}}}"#
        )
    }

    fn health(indicator: &str) -> Health {
        parse(&body(indicator, "Some Words")).expect("a well-formed body must parse")
    }

    #[test]
    fn operational_is_fine() {
        assert_eq!(health("none"), Health::Fine);
    }

    /// The threshold. This was the live state on the day the feature was written, with GitHub's own
    /// Pull Requests component degraded — exactly the case worth seeing.
    #[test]
    fn minor_is_an_outage() {
        assert!(matches!(health("minor"), Health::Degraded { .. }));
    }

    #[test]
    fn minor_major_and_critical_are_all_outages() {
        for indicator in ["minor", "major", "critical"] {
            assert!(
                matches!(health(indicator), Health::Degraded { .. }),
                "{indicator} should raise the mark"
            );
        }
    }

    /// Scheduled work is announced and expected, so it is not a fault to warn about — the one thing
    /// above `none` that stays quiet.
    #[test]
    fn maintenance_is_not_an_outage() {
        assert_eq!(health("maintenance"), Health::Fine);
    }

    /// An indicator this version has never seen must not invent an outage. Statuspage could add one,
    /// and guessing would mean a mark nobody can explain.
    #[test]
    fn an_unknown_indicator_does_not_raise_the_mark() {
        for indicator in ["", "catastrophic", "MAYBE", "42"] {
            assert_eq!(health(indicator), Health::Fine, "{indicator:?} should not alarm");
        }
    }

    #[test]
    fn the_indicator_is_read_case_insensitively_and_untrimmed() {
        for indicator in ["MAJOR", " major ", "Critical"] {
            assert!(
                matches!(health(indicator), Health::Degraded { .. }),
                "{indicator:?} should raise the mark"
            );
        }
    }

    /// The tooltip quotes GitHub's own wording, so it has to survive the parse.
    #[test]
    fn githubs_own_description_is_carried_through() {
        match parse(&body("minor", "Partially Degraded Service")) {
            Ok(Health::Degraded { description }) => {
                assert_eq!(description, "Partially Degraded Service");
            }
            other => panic!("expected Degraded, got {other:?}"),
        }
    }

    /// A verdict is not lost just because the prose is missing; the indicator stands in for it.
    #[test]
    fn a_missing_description_falls_back_to_the_indicator() {
        match parse(r#"{"status":{"indicator":"critical"}}"#) {
            Ok(Health::Degraded { description }) => assert_eq!(description, "critical"),
            other => panic!("expected Degraded, got {other:?}"),
        }
        match parse(&body("major", "")) {
            Ok(Health::Degraded { description }) => assert_eq!(description, "major"),
            other => panic!("expected Degraded, got {other:?}"),
        }
    }

    /// Unreadable is *not* an outage — the inversion this module exists to be careful about. Each of
    /// these must be an `Err`, which the caller drops, rather than a `Degraded` that cries wolf.
    #[test]
    fn an_unreadable_body_is_an_error_not_an_outage() {
        for broken in ["", "not json", "{}", r#"{"status":{}}"#, "[]", r#"{"status":"minor"}"#] {
            assert!(parse(broken).is_err(), "{broken:?} should be an error, never a verdict");
        }
    }

    /// The endpoint and the page are different things, and sending someone to the JSON would be a poor
    /// way to explain an outage.
    #[test]
    fn the_page_url_is_not_the_api_url() {
        assert_ne!(STATUS_PAGE_URL, STATUS_URL);
        assert!(STATUS_URL.starts_with(STATUS_PAGE_URL), "both should be the same host");
        assert!(!STATUS_PAGE_URL.contains("api"));
    }

    /// Hits the real status page. Ignored by default, like the updater's live check.
    #[test]
    #[ignore = "needs network; queries the real GitHub status page"]
    fn reads_the_live_status_page() {
        let client = crate::github::build_client().expect("a client");
        match check(&client) {
            Ok(health) => println!("live GitHub health: {health:?}"),
            Err(e) => panic!("could not read the live status page: {e}"),
        }
    }
}
