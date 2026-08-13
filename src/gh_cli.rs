//! GitHub CLI as a credential source.
//!
//! Two independent things borrow a token from `gh` here, each gated on its own scope: the review
//! search needs the classic `repo` scope (nothing narrower exists — `public_repo` covers public
//! repos only), and `access_token` prefers a `gh` token with the `notifications` scope over running
//! its own OAuth App device flow, falling back to that flow when `gh` cannot supply one. Rather than
//! escalate a purpose-built token to a read-write key for every repository the user can reach, or
//! make every user register their own OAuth App, both borrow the credential `gh` already holds in
//! the OS keyring, and never write it to disk themselves.
//!
//! `gh` is invoked at startup and after a 401, never per poll. The decision logic is a pure
//! function (`classify`) so every branch is testable without a `gh` on PATH, in the same spirit as
//! `github::classify_with`.

use crate::logln;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The one classic scope that grants read access to pull requests in private repositories.
const REQUIRED_SCOPE: &str = "repo";

/// The classic scope `GET /notifications` requires of an OAuth-app/PAT token. Used by
/// `access_token` to decide whether `gh`'s token can stand in for its own device-flow token.
pub const NOTIFICATIONS_SCOPE: &str = "notifications";

/// Overridable so GitHub Enterprise users are not locked out. `gh` reads the same variable.
const HOST_ENV: &str = "GH_HOST";
const DEFAULT_HOST: &str = "github.com";

/// `gh auth token` normally answers in well under a second, but a keyring backend can decide to
/// prompt. The poll thread must never block on that, so the child is killed at the deadline.
const GH_TIMEOUT: Duration = Duration::from_secs(5);
/// Granularity of the deadline check while waiting on the child.
const GH_WAIT_STEP: Duration = Duration::from_millis(50);

/// Host to ask `gh` about.
pub fn host() -> String {
    std::env::var(HOST_ENV)
        .ok()
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| DEFAULT_HOST.to_string())
}

// ── Errors ────────────────────────────────────────────────────────────────────

/// A failed `gh` invocation. Kept separate from `Unavailable` because "the binary is missing" and
/// "the binary said no" need different words in front of the user.
#[derive(Debug)]
pub enum GhError {
    /// No `gh` on PATH.
    NotInstalled,
    /// `gh` exited non-zero. Carries the trimmed stderr, which is where `gh` puts its reasons.
    Exit(String),
    /// The child outlived `GH_TIMEOUT` and was killed.
    Timeout,
    /// Spawn or wait failed for a reason other than a missing binary.
    Io(String),
    /// `gh` answered, but not with the JSON we expected.
    Parse(String),
}

/// Why the review dot cannot run, in words the user can act on.
///
/// A dark dot with no explanation is indistinguishable from "you have nothing to review", which is
/// the one failure mode this whole codebase is built to avoid. So every variant carries both a full
/// message for a dialog and a short line for the tooltip.
#[derive(Debug)]
pub enum Unavailable {
    NotInstalled,
    NotAuthenticated { host: String },
    MissingScope { scopes: String },
    Failed(String),
}

impl Unavailable {
    /// Full text for a startup dialog or the log. Always ends with the command that fixes it.
    pub fn message(&self) -> String {
        match self {
            Unavailable::NotInstalled => "The review dot needs the GitHub CLI.\n\n\
                 Install it from https://cli.github.com and then run:\n\n    gh auth login"
                .to_string(),
            Unavailable::NotAuthenticated { host } => format!(
                "The review dot needs the GitHub CLI to be logged in to {host}.\n\nRun:\n\n    gh auth login"
            ),
            Unavailable::MissingScope { scopes } => format!(
                "The GitHub CLI token cannot read pull requests in private repositories.\n\n\
                 It currently has: {scopes}\n\nRun:\n\n    gh auth refresh --scopes {REQUIRED_SCOPE}"
            ),
            Unavailable::Failed(why) => format!(
                "The review dot could not get a token from the GitHub CLI.\n\n{why}\n\n\
                 Check that `gh auth token` works from a terminal."
            ),
        }
    }

    /// One short line for the tooltip, which has roughly 110 characters for everything.
    pub fn short(&self) -> String {
        match self {
            Unavailable::NotInstalled => "Review dot off: gh not installed".to_string(),
            Unavailable::NotAuthenticated { .. } => {
                "Review dot off: run gh auth login".to_string()
            }
            Unavailable::MissingScope { .. } => {
                format!("Review dot off: run gh auth refresh --scopes {REQUIRED_SCOPE}")
            }
            Unavailable::Failed(_) => "Review dot off: gh call failed".to_string(),
        }
    }
}

impl From<GhError> for Unavailable {
    fn from(e: GhError) -> Self {
        match e {
            GhError::NotInstalled => Unavailable::NotInstalled,
            GhError::Timeout => Unavailable::Failed("gh did not answer within 5s".to_string()),
            GhError::Exit(stderr) => Unavailable::Failed(stderr),
            GhError::Io(why) => Unavailable::Failed(why),
            GhError::Parse(why) => Unavailable::Failed(why),
        }
    }
}

// ── `gh auth status --json hosts` ─────────────────────────────────────────────

#[derive(Deserialize)]
struct StatusEnvelope {
    /// Keyed by host, each holding one entry per logged-in account.
    #[serde(default)]
    hosts: HashMap<String, Vec<Account>>,
}

/// One account as `gh` reports it. Every field defaults, so a future `gh` dropping one of them
/// degrades to "unknown" rather than failing the whole parse.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub login: String,
    /// `keyring`, `oauth_token`, or an env var name. Logged so a surprising credential source is
    /// visible when the dot misbehaves.
    #[serde(default)]
    pub token_source: String,
    /// Comma-separated, exactly as GitHub's `X-Oauth-Scopes` header spells it. Empty when `gh`
    /// could not reach GitHub to ask, or when the credential is a fine-grained token.
    #[serde(default)]
    pub scopes: String,
}

/// Splits a scope string into a set, or `None` when there is nothing to split.
///
/// `None` means *unknown*, which is deliberately not the same as *missing*. `gh auth status` tests
/// the credential over the network, so an offline launch reports no scopes at all, and a
/// fine-grained token has no classic scopes to report. Treating either as "missing `repo`" would
/// pop a false error dialog every time the app starts before the network is up.
pub fn parse_scopes(raw: &str) -> Option<HashSet<String>> {
    let set: HashSet<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    (!set.is_empty()).then_some(set)
}

/// Whether a scope string is known to lack `scope`. `false` when the scopes are unknown — see
/// `parse_scopes` for why unknown must never read as missing.
pub fn lacks_scope(raw: &str, scope: &str) -> bool {
    parse_scopes(raw).is_some_and(|scopes| !scopes.contains(scope))
}

/// Whether a scope string is known to lack `repo`. `false` when the scopes are unknown.
pub fn lacks_required_scope(raw: &str) -> bool {
    lacks_scope(raw, REQUIRED_SCOPE)
}

/// Whether a scope string is known to include `scope`. Unlike `lacks_scope`, unknown scopes read
/// as `false` here too: a caller reaching for this wants an affirmative "yes" before trusting the
/// credential, not "not known to be missing".
pub fn has_scope(raw: &str, scope: &str) -> bool {
    parse_scopes(raw).is_some_and(|scopes| scopes.contains(scope))
}

/// Verdict on whether the review dot can run.
#[derive(Debug)]
pub enum Verdict {
    Ready(Account),
    Blocked(Unavailable),
}

/// Decides from `gh`'s own account report whether the dot can run.
///
/// Pure on purpose: the process spawn lives in `auth_status`, so all four failure branches are
/// unit-testable on a machine with no `gh` at all.
pub fn classify(host: &str, status: Result<Option<Account>, GhError>) -> Verdict {
    match status {
        Err(e) => Verdict::Blocked(e.into()),
        Ok(None) => Verdict::Blocked(Unavailable::NotAuthenticated { host: host.to_string() }),
        Ok(Some(account)) if lacks_required_scope(&account.scopes) => {
            Verdict::Blocked(Unavailable::MissingScope { scopes: account.scopes })
        }
        // Unknown scopes fall through to Ready on purpose. If `repo` really is missing, the search
        // answers 403 or 422, which `github::classify_with` maps to `Transient`, so the icon says
        // "unknown" rather than a confident zero. A wrong guess here would be worse than waiting.
        Ok(Some(account)) => Verdict::Ready(account),
    }
}

/// Asks `gh` which accounts it holds for `host`, returning the active one.
///
/// Crate-visible: `access_token` calls this too, to decide whether `gh` can supply a
/// notifications-scoped token before falling back to its own device flow.
pub(crate) fn auth_status(host: &str) -> Result<Option<Account>, GhError> {
    // `--json` makes gh exit 0 even when nothing is logged in, answering `{"hosts":{}}` on stdout
    // and a human sentence on stderr. So the absence of an entry, not the exit code, is the signal.
    let stdout = run(&["auth", "status", "--hostname", host, "--json", "hosts"])?;
    let envelope: StatusEnvelope =
        serde_json::from_str(&stdout).map_err(|e| GhError::Parse(format!("gh auth status: {e}")))?;

    let Some(accounts) = envelope.hosts.get(host) else {
        return Ok(None);
    };

    // Only the active account's token is what `gh auth token` will hand back, so any other entry
    // would describe a credential we are not going to use.
    Ok(accounts.iter().find(|a| a.active).map(|a| Account {
        active: a.active,
        login: a.login.clone(),
        token_source: a.token_source.clone(),
        scopes: a.scopes.clone(),
    }))
}

/// Fetches the active token for `host`. The value is never logged and never written to disk.
///
/// Crate-visible for the same reason as `auth_status`.
pub(crate) fn auth_token(host: &str) -> Result<String, GhError> {
    let token = run(&["auth", "token", "--hostname", host])?.trim().to_string();
    if token.is_empty() {
        // Exit 0 with nothing to show would otherwise become an empty Bearer header and a 401.
        return Err(GhError::Exit("gh auth token returned nothing".to_string()));
    }
    Ok(token)
}

/// Runs `gh` with a deadline and returns its stdout.
///
/// Output is read only after the child has exited. Both commands used here answer in well under a
/// pipe buffer, so there is no risk of the child blocking on a full pipe while we wait on it.
fn run(args: &[&str]) -> Result<String, GhError> {
    let mut command = Command::new("gh");
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Without this the app would flash a console window on every call: `main` is built with
    // `windows_subsystem = "windows"`, so this process has no console for a console child to
    // inherit and Windows would create one.
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    // A `.app` launched from Finder inherits `PATH=/usr/bin:/bin:/usr/sbin:/sbin` — not the shell's
    // — which is where `gh` is exactly never installed. Without this, a Mac where `gh` works
    // perfectly in Terminal reports `NotInstalled` from the bundle, and the review dot goes quietly
    // dark: the precise confusion `load_review_credential` exists to prevent.
    //
    // Appended, not prepended, so a user who has put their own `gh` on `PATH` still wins. A no-op
    // when these are already present, and harmless when they do not exist.
    #[cfg(target_os = "macos")]
    {
        let path = std::env::var("PATH").unwrap_or_default();
        let missing: Vec<&str> = ["/opt/homebrew/bin", "/usr/local/bin"]
            .into_iter()
            .filter(|dir| !path.split(':').any(|entry| entry == *dir))
            .collect();
        if !missing.is_empty() {
            command.env("PATH", format!("{}:{}", path, missing.join(":")));
        }
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(GhError::NotInstalled),
        Err(e) => return Err(GhError::Io(format!("could not start gh: {e}"))),
    };

    let deadline = Instant::now() + GH_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(GhError::Timeout);
            }
            Ok(None) => std::thread::sleep(GH_WAIT_STEP),
            Err(e) => return Err(GhError::Io(format!("could not wait for gh: {e}"))),
        }
    }

    let output = child
        .wait_with_output()
        .map_err(|e| GhError::Io(format!("could not read gh output: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(GhError::Exit(if stderr.is_empty() {
            format!("gh exited with {}", output.status)
        } else {
            stderr
        }));
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

// ── The credential the poll loop holds ────────────────────────────────────────

/// The review-search credential, borrowed from `gh`.
///
/// Deliberately not a file-backed store like `TokenStore`: there is nothing of ours to persist,
/// because `gh` owns this credential's lifetime, storage and renewal.
pub struct ReviewToken {
    host: String,
    token: String,
}

impl ReviewToken {
    /// Startup path. `Err` is a normal outcome (no `gh`, not logged in, wrong scopes) and must not
    /// stop the app: the notification half does not depend on any of this.
    pub fn load() -> Result<Self, Unavailable> {
        let host = host();
        let account = match classify(&host, auth_status(&host)) {
            Verdict::Ready(account) => account,
            Verdict::Blocked(why) => return Err(why),
        };

        let token = auth_token(&host).map_err(Unavailable::from)?;

        // Everything here except the token itself, so a surprising credential source or a
        // scope-related outage is diagnosable from the log alone.
        logln!(
            "review credential: gh cli, host {host}, login {}, source {}, scopes [{}]",
            account.login,
            account.token_source,
            if account.scopes.is_empty() { "unknown" } else { &account.scopes }
        );

        Ok(Self { host, token })
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    /// Re-asks `gh` after GitHub rejects the token, since `gh` may have rotated it underneath us.
    ///
    /// Returns whether the value actually changed, so the caller can skip an immediate retry that
    /// would only earn the same 401.
    pub fn refresh(&mut self) -> Result<bool, Unavailable> {
        let token = auth_token(&self.host).map_err(Unavailable::from)?;
        let changed = token != self.token;
        self.token = token;
        Ok(changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(scopes: &str) -> Account {
        Account {
            active: true,
            login: "someone".to_string(),
            token_source: "keyring".to_string(),
            scopes: scopes.to_string(),
        }
    }

    /// The exact string `gh` produced on the machine this was built for.
    #[test]
    fn parses_a_real_scope_string() {
        let scopes = parse_scopes("gist, notifications, read:org, repo, workflow").unwrap();
        assert!(scopes.contains("repo"));
        assert!(scopes.contains("read:org"));
        assert_eq!(scopes.len(), 5, "no empty entries from the separators");
    }

    #[test]
    fn scope_parsing_tolerates_spacing_and_trailing_separators() {
        assert_eq!(parse_scopes("repo").unwrap().len(), 1);
        assert_eq!(parse_scopes("repo,,gist,").unwrap().len(), 2);
        assert_eq!(parse_scopes("  repo ,  gist  ").unwrap().len(), 2);
    }

    /// The distinction the whole design rests on: nothing to report is *unknown*, not *missing*.
    #[test]
    fn empty_scopes_are_unknown_rather_than_missing() {
        assert!(parse_scopes("").is_none());
        assert!(parse_scopes("   ").is_none());
        assert!(parse_scopes(",  ,").is_none());
        assert!(!lacks_required_scope(""), "unknown must never read as missing");
        assert!(!lacks_required_scope("   "));
    }

    #[test]
    fn missing_repo_scope_is_detected_when_scopes_are_known() {
        assert!(lacks_required_scope("gist, notifications, read:org"));
        assert!(!lacks_required_scope("repo"));
        // `public_repo` is not a substitute: it cannot see private repositories at all.
        assert!(lacks_required_scope("public_repo, notifications"));
    }

    #[test]
    fn no_gh_binary_blocks_with_install_instructions() {
        match classify("github.com", Err(GhError::NotInstalled)) {
            Verdict::Blocked(why) => {
                assert!(matches!(why, Unavailable::NotInstalled));
                assert!(why.message().contains("cli.github.com"), "got {}", why.message());
                assert!(why.message().contains("gh auth login"));
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    #[test]
    fn no_account_blocks_with_login_instructions() {
        match classify("github.com", Ok(None)) {
            Verdict::Blocked(why) => {
                assert!(matches!(why, Unavailable::NotAuthenticated { .. }));
                assert!(why.message().contains("gh auth login"));
                assert!(why.message().contains("github.com"), "the host must be named");
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    #[test]
    fn known_scopes_without_repo_block_with_the_refresh_command() {
        match classify("github.com", Ok(Some(account("gist, notifications")))) {
            Verdict::Blocked(why) => {
                assert!(matches!(why, Unavailable::MissingScope { .. }));
                assert!(why.message().contains("gh auth refresh --scopes repo"));
                // The user needs to see what they *do* have to make sense of the instruction.
                assert!(why.message().contains("notifications"));
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    #[test]
    fn a_full_scope_set_is_ready() {
        assert!(matches!(
            classify("github.com", Ok(Some(account("notifications, repo")))),
            Verdict::Ready(_)
        ));
    }

    /// An offline launch must start the dot, not block it with a guess.
    #[test]
    fn unknown_scopes_are_ready_and_left_to_the_wire() {
        assert!(matches!(classify("github.com", Ok(Some(account("")))), Verdict::Ready(_)));
    }

    #[test]
    fn a_timeout_is_reported_as_a_failure_not_as_a_missing_binary() {
        match classify("github.com", Err(GhError::Timeout)) {
            Verdict::Blocked(why) => {
                assert!(matches!(why, Unavailable::Failed(_)));
                assert!(why.message().contains("gh auth token"), "got {}", why.message());
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    /// Tooltip space is roughly 110 characters for every line together, so no single reason may
    /// eat the whole budget.
    #[test]
    fn short_reasons_stay_short() {
        for why in [
            Unavailable::NotInstalled,
            Unavailable::NotAuthenticated { host: "github.com".to_string() },
            Unavailable::MissingScope { scopes: "gist, notifications".to_string() },
            Unavailable::Failed("something went wrong in a long winded way".to_string()),
        ] {
            let short = why.short();
            assert!(short.chars().count() <= 60, "{short:?} is {} chars", short.chars().count());
        }
    }

    #[test]
    fn host_defaults_to_github_but_respects_the_env_var() {
        // Not asserting on GH_HOST being unset: the test process may have inherited one.
        let resolved = host();
        assert!(!resolved.is_empty());
        match std::env::var(HOST_ENV) {
            Ok(set) if !set.trim().is_empty() => assert_eq!(resolved, set.trim()),
            _ => assert_eq!(resolved, DEFAULT_HOST),
        }
    }

    #[test]
    fn status_json_shape_from_gh_parses() {
        // Captured verbatim from `gh auth status --hostname github.com --json hosts`.
        let raw = r#"{"hosts":{"github.com":[{"state":"success","active":true,"host":"github.com",
                     "login":"HerrDerb","tokenSource":"keyring",
                     "scopes":"gist, notifications, read:org, repo, workflow",
                     "gitProtocol":"https"}]}}"#;
        let envelope: StatusEnvelope = serde_json::from_str(raw).expect("must parse");
        let accounts = envelope.hosts.get("github.com").expect("host entry");
        let active = accounts.iter().find(|a| a.active).expect("active account");
        assert_eq!(active.login, "HerrDerb");
        assert_eq!(active.token_source, "keyring", "camelCase must be mapped");
        assert!(parse_scopes(&active.scopes).unwrap().contains("repo"));
    }

    /// Exercises the one part unit tests cannot reach: the actual process spawn.
    ///
    /// `#[ignore]` because it needs a real `gh` that is logged in, which a CI runner has no business
    /// requiring. Run it by hand with `cargo test -- --ignored gh_cli` when changing `run`.
    #[test]
    #[ignore = "needs a logged-in gh on this machine"]
    fn really_talks_to_gh() {
        let host = host();
        let account = auth_status(&host).expect("gh auth status must run").expect("an active account");
        assert!(!account.login.is_empty(), "gh must name the account");
        assert!(matches!(classify(&host, Ok(Some(Account { ..account }))), Verdict::Ready(_)));

        let token = auth_token(&host).expect("gh auth token must print something");
        assert!(token.len() > 20, "a plausible token, and never printed");
        assert!(!token.contains('\n'), "trailing newline must be stripped");
    }

    /// Logged out, gh answers exit 0 with an empty map on stdout. The exit code says nothing.
    #[test]
    fn an_empty_hosts_map_is_not_authenticated() {
        let envelope: StatusEnvelope = serde_json::from_str(r#"{"hosts":{}}"#).expect("must parse");
        assert!(!envelope.hosts.contains_key("github.com"));
    }
}
