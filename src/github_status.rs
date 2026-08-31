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

//! ## Watching only part of GitHub
//!
//! The page-wide indicator is one verdict over everything GitHub runs, and most of what it covers is
//! nothing to do with pull requests. On the day this filter was written every component was
//! operational except `Copilot AI Model Providers`, and the page still said `minor` — an exclamation
//! on the tray for a service the app never touches. Cry wolf often enough and the mark stops meaning
//! anything, which costs more than the outage it failed to report.
//!
//! So `statusComponents` in `config.txt` may name the parts that matter. When it does, this reads
//! `components.json` instead and only those parts count. An empty list keeps the page-wide indicator,
//! because that is what every config file written before this key existed says, and those files are
//! never rewritten.
//!
//! Matching is case-insensitive but **whole-name**. `copilot` matching `Copilot AI Model Providers`
//! would quietly re-admit the exact noise the feature removes, so a name either is a component's name
//! or is reported as matching nothing. Reported rather than ignored: a typo's only other symptom is a
//! mark that never appears, which is indistinguishable from GitHub being well.

use crate::infoln;
use reqwest::blocking::Client;
use serde::Deserialize;

const STATUS_URL: &str = "https://www.githubstatus.com/api/v2/status.json";

/// The per-component view, read only when the user has named the components they care about. Bigger
/// than `status.json` (a few kB rather than 219 bytes) and on the same host, which is why the
/// unfiltered path still asks for the small one.
const COMPONENTS_URL: &str = "https://www.githubstatus.com/api/v2/components.json";

/// The page people are sent to, rather than the JSON endpoint above.
pub const STATUS_PAGE_URL: &str = "https://www.githubstatus.com";

/// Indicators that raise the mark. Anything else — `none`, `maintenance`, or an indicator this version
/// has never heard of — deliberately does not, so an unfamiliar value cannot invent an outage.
const DEGRADED_INDICATORS: [&str; 3] = ["minor", "major", "critical"];

/// Component statuses that raise the mark, the per-component mirror of `DEGRADED_INDICATORS`. The two
/// Statuspage values left out are `operational` and `under_maintenance`, for the same reasons `none`
/// and `maintenance` are left out above; an unrecognised status is left out by not being listed.
const DEGRADED_COMPONENT_STATUSES: [&str; 3] =
    ["degraded_performance", "partial_outage", "major_outage"];

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

/// A verdict, plus the configured names that matched no component GitHub publishes.
///
/// The two travel together because only the caller knows whether it has said so already: the check
/// runs every few minutes and a typo is permanent, so complaining from in here would repeat the same
/// line forever. See `scheduler`, which says it once and again only when it changes.
#[derive(Debug)]
pub struct Report {
    pub health: Health,
    /// Empty on the page-wide path, which has no names to match.
    pub unmatched: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct StatusResponse {
    status: StatusBody,
}

#[derive(Debug, Deserialize)]
struct ComponentsResponse {
    components: Vec<Component>,
}

#[derive(Debug, Deserialize)]
struct Component {
    name: String,
    status: String,
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
///
/// `watched` is `config::Config::status_components`. Empty means the page-wide indicator; anything
/// else means only those components count. The endpoint follows from that, so an unfiltered install
/// still makes the same 219-byte request it always did.
pub fn check(client: &Client, watched: &[String]) -> Result<Report, String> {
    let url = if watched.is_empty() { STATUS_URL } else { COMPONENTS_URL };
    let response = client
        .get(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .header(reqwest::header::USER_AGENT, "githoot-tray")
        .send()
        .map_err(|e| format!("could not reach the status page: {e}"))?;

    let status = response.status();
    if !status.is_success() {
        // Note this is the status *page* failing, not GitHub. Treated as "unknown", per the module
        // note above.
        return Err(format!("the status page answered {status}"));
    }

    let body = response.text().map_err(|e| format!("could not read the status page: {e}"))?;
    if watched.is_empty() {
        parse(&body).map(|health| Report { health, unmatched: Vec::new() })
    } else {
        parse_components(&body, watched)
    }
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

/// Reads one `components.json` body against the names the user asked for. Split out from `check` for
/// the same reason `parse` is: every branch is testable without a network.
fn parse_components(body: &str, watched: &[String]) -> Result<Report, String> {
    let parsed: ComponentsResponse =
        serde_json::from_str(body).map_err(|e| format!("unparseable components payload: {e}"))?;

    // Folded once, not once per component, so a long watch list does not re-lowercase every name for
    // every component in the payload.
    let wanted: Vec<String> = watched.iter().map(|name| fold(name)).collect();

    let mut matched = vec![false; wanted.len()];
    let mut broken: Vec<String> = Vec::new();

    // Driven by the payload rather than by the watch list, so the wording comes out in GitHub's own
    // order: the order of the page the user is about to open.
    for component in &parsed.components {
        let folded = fold(&component.name);
        let Some(index) = wanted.iter().position(|name| *name == folded) else {
            continue;
        };
        matched[index] = true;
        let status = fold(&component.status);
        if DEGRADED_COMPONENT_STATUSES.contains(&status.as_str()) {
            broken.push(format!("{} ({})", component.name.trim(), humanise(&status)));
        }
    }

    let unmatched: Vec<String> = watched
        .iter()
        .zip(&matched)
        .filter(|&(_, &found)| !found)
        .map(|(name, _)| name.clone())
        .collect();

    let health = if broken.is_empty() {
        Health::Fine
    } else {
        let description = broken.join(", ");
        infoln!("GitHub reports a watched component degraded: {description}");
        Health::Degraded { description }
    };
    Ok(Report { health, unmatched })
}

/// The one comparison rule, in one place: case-insensitive and untrimmed, never partial.
fn fold(name: &str) -> String {
    name.trim().to_ascii_lowercase()
}

/// `degraded_performance` to `Degraded Performance`.
///
/// `components.json` sends the machine spelling where `status.json` sends prose, and a tooltip reading
/// "Issues (degraded_performance)" looks like a leaked internal. Only the shape changes; the words
/// stay GitHub's.
fn humanise(status: &str) -> String {
    status
        .split('_')
        .filter(|word| !word.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
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

    // ── Watching only the components you care about ─────────────────────────

    /// A trimmed-down `components.json`, in the shape and order GitHub publishes.
    fn components_body(entries: &[(&str, &str)]) -> String {
        let items: Vec<String> = entries
            .iter()
            .map(|(name, status)| {
                format!(r#"{{"id":"x","name":"{name}","status":"{status}","group":false}}"#)
            })
            .collect();
        format!(r#"{{"page":{{"id":"kctbh9vrtdwd"}},"components":[{}]}}"#, items.join(","))
    }

    fn watch(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    const LIVE_SHAPE: [(&str, &str); 4] = [
        ("Git Operations", "operational"),
        ("API Requests", "operational"),
        ("Issues", "operational"),
        ("Copilot AI Model Providers", "degraded_performance"),
    ];

    /// The whole point of the feature: the page says `minor` because of a component nobody here
    /// asked about, and the mark stays down.
    #[test]
    fn a_degraded_component_nobody_watches_is_not_an_outage() {
        let report = parse_components(&components_body(&LIVE_SHAPE), &watch(&["Issues", "API Requests"]))
            .expect("a well-formed body must parse");
        assert_eq!(report.health, Health::Fine);
        assert!(report.unmatched.is_empty());
    }

    #[test]
    fn a_degraded_component_you_watch_raises_the_mark() {
        let report = parse_components(&components_body(&LIVE_SHAPE), &watch(&["Copilot AI Model Providers"]))
            .expect("a well-formed body must parse");
        assert!(matches!(report.health, Health::Degraded { .. }));
    }

    /// Same folding the config file promises: the name is matched case-insensitively and untrimmed,
    /// so `issues` is `Issues`.
    #[test]
    fn component_names_match_case_insensitively_and_untrimmed() {
        for spelling in ["issues", " ISSUES ", "IsSuEs"] {
            let body = components_body(&[("Issues", "major_outage")]);
            let report = parse_components(&body, &watch(&[spelling])).expect("parses");
            assert!(
                matches!(report.health, Health::Degraded { .. }),
                "{spelling:?} should match the component"
            );
            assert!(report.unmatched.is_empty(), "{spelling:?} matched, so nothing is unmatched");
        }
    }

    /// A partial name is **not** a match, deliberately: `copilot` would otherwise silently drag in
    /// `Copilot AI Model Providers`, which is the exact noise this feature removes.
    #[test]
    fn a_partial_name_does_not_match() {
        let body = components_body(&LIVE_SHAPE);
        let report = parse_components(&body, &watch(&["Copilot", "operations"])).expect("parses");
        assert_eq!(report.health, Health::Fine);
        assert_eq!(report.unmatched, ["Copilot", "operations"]);
    }

    /// The three component statuses that count, mirroring the indicator thresholds.
    #[test]
    fn every_faulty_component_status_raises_the_mark() {
        for status in ["degraded_performance", "partial_outage", "major_outage"] {
            let body = components_body(&[("Issues", status)]);
            let report = parse_components(&body, &watch(&["Issues"])).expect("parses");
            assert!(matches!(report.health, Health::Degraded { .. }), "{status} should alarm");
        }
    }

    /// Scheduled work is announced, and an unheard-of status must not invent an outage — the same
    /// two exemptions the page-wide indicator makes.
    #[test]
    fn maintenance_and_unknown_component_statuses_stay_quiet() {
        for status in ["operational", "under_maintenance", "", "sideways", "MAYBE"] {
            let body = components_body(&[("Issues", status)]);
            let report = parse_components(&body, &watch(&["Issues"])).expect("parses");
            assert_eq!(report.health, Health::Fine, "{status:?} should not alarm");
        }
    }

    /// The tooltip has to name the part that is broken and quote GitHub's own word for how broken,
    /// because "GitHub: Partially Degraded Service" is what sent people to the status page to find
    /// out which half.
    #[test]
    fn the_description_names_the_component_and_its_state() {
        let body = components_body(&[("Issues", "degraded_performance")]);
        match parse_components(&body, &watch(&["Issues"])).expect("parses").health {
            Health::Degraded { description } => {
                assert_eq!(description, "Issues (Degraded Performance)");
            }
            other => panic!("expected Degraded, got {other:?}"),
        }
    }

    /// More than one at once, in the order GitHub lists them rather than the order they were
    /// configured, so the wording matches the page you are about to open.
    #[test]
    fn several_broken_components_are_all_named_in_payload_order() {
        let body = components_body(&[
            ("Git Operations", "major_outage"),
            ("API Requests", "operational"),
            ("Issues", "partial_outage"),
        ]);
        match parse_components(&body, &watch(&["Issues", "Git Operations"])).expect("parses").health {
            Health::Degraded { description } => {
                assert_eq!(description, "Git Operations (Major Outage), Issues (Partial Outage)");
            }
            other => panic!("expected Degraded, got {other:?}"),
        }
    }

    /// A name that matches nothing is reported rather than swallowed: it is a typo, and the only
    /// symptom would otherwise be a mark that never appears.
    #[test]
    fn names_that_match_nothing_are_reported() {
        let body = components_body(&LIVE_SHAPE);
        let report = parse_components(&body, &watch(&["Issues", "Isues", "Pull Requests"])).expect("parses");
        assert_eq!(report.unmatched, ["Isues", "Pull Requests"]);
        assert_eq!(report.health, Health::Fine, "a typo must not invent an outage either");
    }

    /// An empty watch list never reaches this function — `check` asks the page-wide endpoint
    /// instead — but if it did it must not read as "watch everything" here.
    #[test]
    fn an_empty_watch_list_matches_nothing() {
        let report = parse_components(&components_body(&LIVE_SHAPE), &[]).expect("parses");
        assert_eq!(report.health, Health::Fine);
        assert!(report.unmatched.is_empty());
    }

    /// Unreadable stays "unknown", not "fine" and not an outage, exactly as the page-wide path.
    #[test]
    fn an_unreadable_components_body_is_an_error() {
        for broken in ["", "not json", "{}", r#"{"components":"Issues"}"#, "[]"] {
            assert!(
                parse_components(broken, &watch(&["Issues"])).is_err(),
                "{broken:?} should be an error, never a verdict"
            );
        }
    }

    /// The endpoints are siblings on the same host, and the components one is what the filtered
    /// path must ask for.
    #[test]
    fn the_components_url_is_a_sibling_of_the_status_url() {
        assert_ne!(COMPONENTS_URL, STATUS_URL);
        assert!(COMPONENTS_URL.starts_with(STATUS_PAGE_URL));
        assert!(COMPONENTS_URL.ends_with("components.json"));
    }

    /// Hits the real status page. Ignored by default, like the updater's live check.
    #[test]
    #[ignore = "needs network; queries the real GitHub status page"]
    fn reads_the_live_status_page() {
        let client = crate::github::build_client().expect("a client");
        match check(&client, &[]) {
            Ok(report) => println!("live GitHub health: {:?}", report.health),
            Err(e) => panic!("could not read the live status page: {e}"),
        }
    }

    /// The filtered path against the real payload. The only thing that can catch GitHub renaming a
    /// component out from under the list `config.rs` writes into a fresh `config.txt`.
    #[test]
    #[ignore = "needs network; queries the real GitHub status page"]
    fn every_component_named_in_the_default_config_still_exists() {
        let client = crate::github::build_client().expect("a client");
        let watched = crate::config::default_status_components();
        let report = check(&client, &watched).expect("could not read the live components");
        assert!(
            report.unmatched.is_empty(),
            "config.rs names components GitHub no longer publishes: {:?}",
            report.unmatched
        );
        println!("live watched health: {:?}", report.health);
    }
}
