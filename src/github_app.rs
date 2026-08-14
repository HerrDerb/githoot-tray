//! PR-status credential: a shared GitHub App's Device Flow.
//!
//! All three PR-search axes (`state::PrAxis`) are driven by one credential from one shared,
//! fine-grained GitHub App — not a personally-registered OAuth App like the notifications
//! credential in `access_token`, and not `gh`'s token. The App's Client ID is public by design
//! (Device Flow needs no client secret to authorize), so it is safe to hardcode and ship in the
//! binary: every user still authorizes individually through their own browser and gets their own
//! token, exactly as they would with a personal OAuth App, just without having to register one.
//!
//! Read access to PRs in an organization's private repositories is granted by *installing* the
//! App on that org (GitHub's own org-approval flow for third-party access, which every classic
//! OAuth App already goes through) — not by requesting a broader scope. That is the whole reason
//! this is a GitHub App and not an OAuth App: fine-grained, installable, no scope wide enough to
//! also grant write access the way classic `repo` does.
//!
//! ## What needs verifying against a real, registered App before this is trusted
//!
//! This module was written from GitHub's public Device Flow documentation, not from a live test
//! against a real App — that needs a registered Client ID neither this codebase nor this session
//! has. Two things specifically are best-effort, not confirmed:
//!
//!   1. Whether the refresh-token grant (`grant_type=refresh_token`) succeeds *without* a client
//!      secret for this App. It is sent secret-less on purpose — a secret baked into a distributed
//!      binary is not actually secret — but if GitHub requires one anyway, `refresh` simply fails
//!      and `reauthenticate` falls back to a fresh Device Flow. So the worst case of being wrong
//!      about this is an extra browser prompt roughly once per access-token lifetime (a few hours,
//!      if the App has token expiry turned on at all — see below), not a broken credential.
//!   2. The exact response shape of `GET /user/installations`. It is assumed to match the
//!      documented "List app installations accessible to the user access token" shape
//!      (`{"total_count": N, "installations": [...]}`), read the same way `github.rs` already
//!      reads `total_count` from search responses.
//!
//! Registering the App with "Expire user authorization tokens" turned **off** sidesteps both
//! questions entirely — the token poll response is then the same simple long-lived shape the
//! notifications credential already uses, and `refresh`/`needs_refresh` never come into play.

use crate::logln;
use reqwest::blocking::Client;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Overridable for local testing against a freshly registered App without rebuilding — the
/// shipped binary falls back to `CLIENT_ID`. Not documented for end users: this App is meant to
/// be shared, not personally configured, unlike the notifications OAuth App's `client_id.txt`.
const CLIENT_ID_ENV: &str = "GITHUB_APP_CLIENT_ID";

/// The shared GitHub App's Client ID. Public by design; safe to hardcode — see this module's doc
/// comment for why. The Device Flow / refresh-token behavior behind it is still unverified
/// against the live API (also see the doc comment) even though the ID itself is real now.
const CLIENT_ID: &str = "Iv23lipB1miHw6m9SG6n";

const PR_TOKEN_FILE: &str = "pr_token.txt";
const AGENT: &str = "git-system-tray";

const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// GitHub's device-flow `interval` is respected, but never dips below this.
const MIN_DEVICE_POLL_INTERVAL: u64 = 5;
/// Backoff added when GitHub answers `slow_down`.
const SLOW_DOWN_PENALTY: Duration = Duration::from_secs(5);

/// Refresh this long before actual expiry, so a slow request or minor clock skew can never let
/// the token expire mid-flight.
const EXPIRY_SAFETY_MARGIN: Duration = Duration::from_secs(5 * 60);

/// This credential's tag for `dialog::show_device_code_prompt`/`show_auth_success`.
const AUTH_SUBJECT: &str = "GitHub PR Status";

fn client_id() -> String {
    std::env::var(CLIENT_ID_ENV)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| CLIENT_ID.to_string())
}

// ── Errors ────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum AuthError {
    Network(String),
    Denied,
    Expired,
    Github(String),
    /// Nothing left to try without the user: there is no refresh token, or GitHub rejected the one
    /// we have. The only way forward is a device flow, which needs a browser and a human, so this
    /// is reported rather than started — the caller raises the tray's Authenticate item and waits.
    ///
    /// Deliberately distinct from `Network`: unreachable is not the same as invalid, and a laptop
    /// launched before its WiFi is up must not be told to sign in again.
    AuthorizationRequired,
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::Network(e) => write!(f, "network error during authorization: {e}"),
            AuthError::Denied => write!(f, "authorization was denied"),
            AuthError::Expired => write!(f, "device code expired before authorization completed"),
            AuthError::Github(e) => write!(f, "GitHub reported: {e}"),
            AuthError::AuthorizationRequired => write!(f, "authorization required"),
        }
    }
}

impl std::error::Error for AuthError {}

// ── Device code / token responses ──────────────────────────────────────────────

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: u64,
}

/// The token poll response. `expires_in`/`refresh_token`/`refresh_token_expires_in` are present
/// only when the App has "Expire user authorization tokens" turned on — see the module doc
/// comment — so all three have to be optional here rather than assumed.
#[derive(Deserialize)]
struct TokenPollResponse {
    access_token: Option<String>,
    error: Option<String>,
    expires_in: Option<u64>,
    refresh_token: Option<String>,
}

/// The shape GitHub's OAuth endpoints use for a rejected request, per RFC 8628 §3.2. Mirrors
/// `access_token::OAuthErrorResponse` — kept as a separate copy rather than shared, since the two
/// modules otherwise have no dependency on each other and this is a two-field struct, not
/// meaningfully worth coupling them over.
#[derive(Deserialize)]
struct OAuthErrorResponse {
    error: String,
    error_description: Option<String>,
}

/// Turns a `/login/device/code` response that failed to parse as `DeviceCodeResponse` into a
/// message that names the actual problem, instead of an opaque "missing field" error — same
/// reasoning as `access_token::describe_device_code_failure`.
fn describe_oauth_failure(status: reqwest::StatusCode, body: &str) -> String {
    if let Ok(err) = serde_json::from_str::<OAuthErrorResponse>(body) {
        return match err.error_description {
            Some(desc) => format!("{desc} ({})", err.error),
            None => err.error,
        };
    }
    format!("GitHub answered {status} with an unexpected response: {body}")
}

fn build_client() -> Result<Client, AuthError> {
    Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|e| AuthError::Network(e.to_string()))
}

// ── Credential ──────────────────────────────────────────────────────────────────

/// The access token, its expiry if the App issues one, and a refresh token if it does.
struct Credential {
    access_token: String,
    /// `None` means the App has expiration turned off for user tokens — the long-lived,
    /// never-expires-until-revoked shape the notifications credential also uses.
    expires_at: Option<SystemTime>,
    refresh_token: Option<String>,
}

impl Credential {
    /// Whether this credential is expired or expiring imminently, so the poll loop can refresh
    /// proactively instead of waiting for GitHub to answer 401. Always `false` when the App
    /// issues tokens that never expire.
    fn needs_refresh(&self) -> bool {
        match self.expires_at {
            Some(at) => SystemTime::now() + EXPIRY_SAFETY_MARGIN >= at,
            None => false,
        }
    }
}

fn to_credential(access_token: String, expires_in: Option<u64>, refresh_token: Option<String>) -> Credential {
    let expires_at = expires_in.map(|secs| SystemTime::now() + Duration::from_secs(secs));
    Credential { access_token, expires_at, refresh_token }
}

/// Reads a saved credential from `path`. `None` on anything short of a usable `access_token` —
/// missing file, malformed content, or a file written by an incompatible earlier version all just
/// mean "start fresh with the device flow", never a hard failure.
fn read_credential(path: &Path) -> Option<Credential> {
    let content = std::fs::read_to_string(path).ok()?;

    let mut access_token = None;
    let mut expires_at = None;
    let mut refresh_token = None;

    for line in content.lines() {
        let Some((key, value)) = line.trim().split_once('=') else { continue };
        match key.trim() {
            "access_token" => access_token = Some(value.trim().to_string()).filter(|v| !v.is_empty()),
            "expires_at" => {
                expires_at =
                    value.trim().parse::<u64>().ok().map(|secs| UNIX_EPOCH + Duration::from_secs(secs));
            }
            "refresh_token" => {
                refresh_token = Some(value.trim().to_string()).filter(|v| !v.is_empty());
            }
            _ => {}
        }
    }

    Some(Credential { access_token: access_token?, expires_at, refresh_token })
}

/// Writes a credential with owner-only permissions — same approach as
/// `access_token::save_token`, since this file holds a live bearer token too.
fn save_credential(path: &Path, credential: &Credential) {
    let mut content = format!("access_token={}\n", credential.access_token);
    if let Some(at) = credential.expires_at
        && let Ok(since_epoch) = at.duration_since(UNIX_EPOCH)
    {
        content.push_str(&format!("expires_at={}\n", since_epoch.as_secs()));
    }
    if let Some(refresh_token) = &credential.refresh_token {
        content.push_str(&format!("refresh_token={refresh_token}\n"));
    }

    #[cfg(unix)]
    let written = {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let result = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .and_then(|mut file| file.write_all(content.as_bytes()));

        if result.is_ok() {
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        result
    };

    #[cfg(not(unix))]
    let written = std::fs::write(path, &content);

    if let Err(e) = written {
        logln!("warning: could not save PR credential to disk: {e}");
    }
}

/// Exchanges a refresh token for a new access token.
///
/// Sent *without* a client secret — see this module's doc comment for why, and what happens if
/// that turns out to be wrong for this App.
fn refresh(http: &Client, refresh_token: &str) -> Result<Credential, AuthError> {
    let response = http
        .post("https://github.com/login/oauth/access_token")
        .header("Accept", "application/json")
        .header("User-Agent", AGENT)
        .form(&[
            ("client_id", client_id().as_str()),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ])
        .send()
        .map_err(|e| AuthError::Network(e.to_string()))?;
    let status = response.status();
    let body = response.text().map_err(|e| AuthError::Network(e.to_string()))?;

    let resp: TokenPollResponse = serde_json::from_str(&body)
        .map_err(|_| AuthError::Github(describe_oauth_failure(status, &body)))?;

    match resp.access_token {
        Some(access_token) => Ok(to_credential(access_token, resp.expires_in, resp.refresh_token)),
        None => Err(AuthError::Github(
            resp.error.unwrap_or_else(|| format!("refresh failed with no error given ({status})")),
        )),
    }
}

/// Step 1 of the device flow: request a device code. Sends no `scope` — a GitHub App's
/// permissions are fixed at registration, not requested per-authorization, so the parameter is
/// meaningless here (unlike the classic OAuth App flow in `access_token`).
fn request_device_code(http: &Client) -> Result<DeviceCodeResponse, AuthError> {
    let response = http
        .post("https://github.com/login/device/code")
        .header("Accept", "application/json")
        .header("User-Agent", AGENT)
        .form(&[("client_id", client_id().as_str())])
        .send()
        .map_err(|e| AuthError::Network(e.to_string()))?;
    let status = response.status();
    let body = response.text().map_err(|e| AuthError::Network(e.to_string()))?;

    serde_json::from_str(&body).map_err(|_| AuthError::Github(describe_oauth_failure(status, &body)))
}

/// Runs the full Device Flow and returns the resulting credential.
fn device_code_flow(http: &Client) -> Result<Credential, AuthError> {
    let dc = request_device_code(http)?;

    // The prompt owns the browser launch — see `dialog::show_device_code_prompt`. Non-blocking, so
    // the poll loop below starts while the dialog is still on screen.
    crate::dialog::show_device_code_prompt(AUTH_SUBJECT, &dc.user_code, &dc.verification_uri);

    let mut poll_interval = Duration::from_secs(dc.interval.max(MIN_DEVICE_POLL_INTERVAL));
    let expires_at = Instant::now() + Duration::from_secs(dc.expires_in);

    loop {
        if Instant::now() >= expires_at {
            return Err(AuthError::Expired);
        }

        std::thread::sleep(poll_interval);

        let response = http
            .post("https://github.com/login/oauth/access_token")
            .header("Accept", "application/json")
            .header("User-Agent", AGENT)
            .form(&[
                ("client_id", client_id().as_str()),
                ("device_code", dc.device_code.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send()
            .map_err(|e| AuthError::Network(e.to_string()))?;
        let status = response.status();
        let body = response.text().map_err(|e| AuthError::Network(e.to_string()))?;
        let resp: TokenPollResponse = serde_json::from_str(&body)
            .map_err(|_| AuthError::Github(describe_oauth_failure(status, &body)))?;

        if let Some(access_token) = resp.access_token {
            crate::dialog::show_auth_success(AUTH_SUBJECT);
            return Ok(to_credential(access_token, resp.expires_in, resp.refresh_token));
        }

        match resp.error.as_deref() {
            // Both mean "keep waiting"; only the pacing differs.
            Some("authorization_pending") | None => {}
            Some("slow_down") => poll_interval += SLOW_DOWN_PENALTY,
            Some("expired_token") => return Err(AuthError::Expired),
            Some("access_denied") => return Err(AuthError::Denied),
            Some(other) => return Err(AuthError::Github(other.to_string())),
        }
    }
}

// ── GitHub App installations ────────────────────────────────────────────────────

/// `GET /user/installations`'s `total_count`. Everything else in the response is unused —
/// mirrors how `github.rs`'s `SearchResult` reads just `total_count` from search responses.
#[derive(Deserialize)]
struct InstallationsResponse {
    total_count: u64,
}

/// Number of accounts/organizations that have installed the App for this user.
///
/// A user who has never installed the App anywhere still gets a token that authenticates fine,
/// but PR search would then silently see zero repositories — a confident, wrong "nothing to
/// report" indistinguishable from genuinely having nothing to report. Called once at startup, not
/// every poll — installations do not change fast enough to justify the extra request on the hot
/// path.
fn installation_count(http: &Client, token: &str) -> Result<u64, AuthError> {
    let response = http
        .get("https://api.github.com/user/installations")
        .header("Accept", "application/vnd.github+json")
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
        .header("User-Agent", AGENT)
        .send()
        .map_err(|e| AuthError::Network(e.to_string()))?;
    let status = response.status();
    let body = response.text().map_err(|e| AuthError::Network(e.to_string()))?;

    let resp: InstallationsResponse = serde_json::from_str(&body)
        .map_err(|_| AuthError::Github(describe_oauth_failure(status, &body)))?;
    Ok(resp.total_count)
}

// ── Token storage ─────────────────────────────────────────────────────────────

/// Tooltip reason when the user is authorized but the App is installed on no account.
///
/// Shared by the two places that can discover it — `main.rs` at startup and the poll loop right
/// after a menu-driven sign-in — so the same condition cannot be worded two ways.
pub const PR_NOT_INSTALLED: &str = "PR status off: install the GitHub App to see your PRs";

/// What PR status could be brought up to without asking the user anything.
///
/// Three outcomes rather than an `Option`, because "no dots on the icon" has three quite different
/// meanings and each is said differently. Collapsing them is exactly the confusion this codebase is
/// shaped around avoiding: a dark icon that means "nothing needs you" must never look like one that
/// means "nobody could ask".
pub enum PrStatus {
    /// A usable credential. Polling starts immediately.
    Ready(PrTokenStore),
    /// Nothing usable on disk, or what was there can no longer be renewed silently. Red
    /// exclamation, `Authenticate` on the menu, one click from being fixed.
    NeedsAuth,
    /// Signed in fine, but there is nothing to see and clicking would not change that — the App is
    /// not installed on any account, or no HTTP client could be built at all. The reason travels
    /// with it, for the tooltip.
    Off(String),
}

/// Owns the PR-status credential and knows how to renew it.
///
/// Lives on the poll thread so a mid-run 401, or an approaching expiry, can be recovered from
/// without restarting the app — same role `access_token::TokenStore` plays for notifications.
pub struct PrTokenStore {
    token_path: PathBuf,
    credential: Credential,
    http: Client,
}

impl PrTokenStore {
    /// Startup path, and deliberately **non-interactive**: reuse a saved credential, refreshing it
    /// first if it is expiring and carries a refresh token.
    ///
    /// `Ok(None)` means there is nothing usable and a device flow is needed. That is reported rather
    /// than run, because launching a browser during startup is the behaviour this design replaces:
    /// the tray icon appears first, wearing the red exclamation, and the user starts the flow from
    /// the menu when it suits them. `Err` is reserved for not being able to build an HTTP client at
    /// all, which no amount of clicking would fix.
    ///
    /// A refresh grant *is* still attempted here, unlike the device flow: it needs no browser and no
    /// human, so there is nothing to defer.
    pub fn load_saved(app_asset_path: &Path) -> Result<Option<Self>, AuthError> {
        let http = build_client()?;
        let token_path = app_asset_path.join(PR_TOKEN_FILE);

        let Some(saved) = read_credential(&token_path) else {
            logln!("no saved PR credential — waiting for the user to authorize");
            return Ok(None);
        };

        if !saved.needs_refresh() {
            return Ok(Some(Self { token_path, credential: saved, http }));
        }

        let Some(refresh_token) = saved.refresh_token.clone() else {
            logln!("saved PR credential has expired and carries no refresh token");
            return Ok(None);
        };

        match refresh(&http, &refresh_token) {
            Ok(credential) => {
                save_credential(&token_path, &credential);
                Ok(Some(Self { token_path, credential, http }))
            }
            // Could not reach GitHub, which says nothing about whether the refresh token is still
            // good — the common cause is a tray app started at login before the network is up. Keep
            // the stale credential and let the poll loop's own `needs_refresh` check retry every
            // cycle, rather than demanding a click for something that heals itself.
            Err(e @ AuthError::Network(_)) => {
                logln!("could not refresh the PR credential yet ({e}) — retrying on the poll loop");
                Ok(Some(Self { token_path, credential: saved, http }))
            }
            Err(e) => {
                logln!("saved PR credential was rejected ({e}) — waiting for the user to authorize");
                Ok(None)
            }
        }
    }

    /// The interactive path: runs the full device flow and saves the result.
    ///
    /// Called only when the user picks the tray's Authenticate item, so a browser and a dialog are
    /// expected here rather than a surprise. Blocks for as long as the flow takes (up to GitHub's
    /// 15-minute device-code lifetime), so its caller must be the poll thread, never the UI thread.
    pub fn authenticate(app_asset_path: &Path) -> Result<Self, AuthError> {
        let http = build_client()?;
        let token_path = app_asset_path.join(PR_TOKEN_FILE);
        let credential = device_code_flow(&http)?;
        save_credential(&token_path, &credential);
        Ok(Self { token_path, credential, http })
    }

    pub fn token(&self) -> &str {
        &self.credential.access_token
    }

    /// Number of accounts/organizations that have installed the App, so the caller can tell a
    /// genuinely empty PR search apart from one that can't see any repositories at all yet. See
    /// `installation_count`'s own doc comment for why this matters.
    pub fn installation_count(&self) -> Result<u64, AuthError> {
        installation_count(&self.http, self.token())
    }

    /// Whether the current credential is expired or expiring imminently.
    ///
    /// The poll loop should check this once per cycle and call `reauthenticate` proactively when
    /// it answers `true`, rather than only reacting to a 401 — unlike `TokenStore`/`ReviewToken`,
    /// this credential can expire on a known schedule (see the module doc comment), so waiting
    /// for GitHub to reject it first would mean at least one guaranteed failed poll per renewal.
    pub fn needs_refresh(&self) -> bool {
        self.credential.needs_refresh()
    }

    /// Mid-run recovery, called either after GitHub rejects the token or when `needs_refresh` turns
    /// proactive. The refresh grant only.
    ///
    /// It used to fall back to a fresh device flow, which meant a browser window and a dialog could
    /// appear unannounced hours into a session. Now that path is the user's to start: when the
    /// refresh grant cannot help, this returns `AuthError::AuthorizationRequired` and the caller
    /// raises the exclamation and the Authenticate menu item instead.
    ///
    /// GitHub rejecting the *access* token does not mean the refresh token is bad too, so the grant
    /// is always worth trying first — it recovers silently in the common case.
    pub fn reauthenticate(&mut self) -> Result<(), AuthError> {
        let Some(refresh_token) = self.credential.refresh_token.clone() else {
            return Err(AuthError::AuthorizationRequired);
        };

        match refresh(&self.http, &refresh_token) {
            Ok(credential) => {
                save_credential(&self.token_path, &credential);
                self.credential = credential;
                Ok(())
            }
            // Passed through unchanged, not converted: a network error must not cost the user a
            // click, so the caller retries next cycle rather than demanding authorization.
            Err(e @ AuthError::Network(_)) => Err(e),
            Err(e) => {
                logln!("PR credential refresh was rejected ({e}) — authorization required");
                Err(AuthError::AuthorizationRequired)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_credential_with_no_expiry_never_needs_refresh() {
        let credential = Credential { access_token: "t".to_string(), expires_at: None, refresh_token: None };
        assert!(!credential.needs_refresh());
    }

    #[test]
    fn a_credential_past_its_expiry_needs_refresh() {
        let credential = Credential {
            access_token: "t".to_string(),
            expires_at: Some(SystemTime::now() - Duration::from_secs(1)),
            refresh_token: None,
        };
        assert!(credential.needs_refresh());
    }

    /// The safety margin exists precisely so a token is renewed before it can expire mid-request,
    /// not only after — so "needs refresh" must trigger before the exact expiry instant.
    #[test]
    fn a_credential_expiring_within_the_safety_margin_needs_refresh() {
        let credential = Credential {
            access_token: "t".to_string(),
            expires_at: Some(SystemTime::now() + Duration::from_secs(30)),
            refresh_token: None,
        };
        assert!(credential.needs_refresh(), "30s left is well inside the 5 minute margin");
    }

    #[test]
    fn a_credential_expiring_well_outside_the_safety_margin_does_not_need_refresh() {
        let credential = Credential {
            access_token: "t".to_string(),
            expires_at: Some(SystemTime::now() + Duration::from_secs(3600)),
            refresh_token: None,
        };
        assert!(!credential.needs_refresh());
    }

    #[test]
    fn describe_oauth_failure_decodes_the_real_reason() {
        let body = r#"{"error":"unauthorized_client","error_description":"App is not installed."}"#;
        let msg = describe_oauth_failure(reqwest::StatusCode::BAD_REQUEST, body);
        assert!(msg.contains("App is not installed"), "got {msg:?}");
        assert!(msg.contains("unauthorized_client"), "got {msg:?}");
    }

    #[test]
    fn describe_oauth_failure_falls_back_to_status_and_raw_text_for_an_unrecognised_body() {
        let msg = describe_oauth_failure(reqwest::StatusCode::BAD_GATEWAY, "<html>blocked</html>");
        assert!(msg.contains("502"), "got {msg:?}");
        assert!(msg.contains("<html>blocked</html>"), "got {msg:?}");
    }

    #[test]
    fn a_saved_credential_round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!("gh-app-token-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("test temp dir");
        let path = dir.join("pr_token.txt");

        let original = Credential {
            access_token: "ghu_abc123".to_string(),
            expires_at: Some(UNIX_EPOCH + Duration::from_secs(1_800_000_000)),
            refresh_token: Some("ghr_def456".to_string()),
        };
        save_credential(&path, &original);

        let loaded = read_credential(&path).expect("must read back what was just written");
        assert_eq!(loaded.access_token, original.access_token);
        assert_eq!(loaded.expires_at, original.expires_at);
        assert_eq!(loaded.refresh_token, original.refresh_token);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_credential_with_no_expiry_or_refresh_token_round_trips_too() {
        let dir = std::env::temp_dir().join(format!("gh-app-token-test-bare-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("test temp dir");
        let path = dir.join("pr_token.txt");

        let original = Credential { access_token: "ghu_bare".to_string(), expires_at: None, refresh_token: None };
        save_credential(&path, &original);

        let loaded = read_credential(&path).expect("must read back what was just written");
        assert_eq!(loaded.access_token, "ghu_bare");
        assert_eq!(loaded.expires_at, None);
        assert_eq!(loaded.refresh_token, None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_absent_file_reads_as_no_saved_credential() {
        let path = std::env::temp_dir().join("gh-app-token-definitely-does-not-exist.txt");
        assert!(read_credential(&path).is_none());
    }

    /// The behaviour the whole deferred-authorization design rests on: with nothing saved,
    /// `load_saved` must answer "nobody is authorized" *without* touching the network or opening a
    /// browser, so startup can put the tray icon up and wait for a click.
    ///
    /// If this ever regressed to running the device flow, the test would hang for GitHub's
    /// fifteen-minute device-code lifetime rather than fail — which is itself the signal.
    #[test]
    fn load_saved_reports_needing_authorization_without_running_a_device_flow() {
        let dir = std::env::temp_dir().join(format!("gh-app-load-saved-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("test temp dir");

        let outcome = PrTokenStore::load_saved(&dir).expect("building an HTTP client must succeed");
        assert!(outcome.is_none(), "an empty asset directory must mean authorization is needed");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A saved, non-expiring credential is reused as-is: no refresh grant, no device flow, no
    /// network at all. This is the ordinary startup path, and it has to stay silent.
    #[test]
    fn load_saved_reuses_a_credential_that_is_not_expiring() {
        let dir = std::env::temp_dir().join(format!("gh-app-load-reuse-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("test temp dir");

        save_credential(
            &dir.join(PR_TOKEN_FILE),
            &Credential {
                access_token: "ghu_still_good".to_string(),
                expires_at: None,
                refresh_token: None,
            },
        );

        let store = PrTokenStore::load_saved(&dir)
            .expect("building an HTTP client must succeed")
            .expect("a healthy saved credential must be reused");
        assert_eq!(store.token(), "ghu_still_good");
        assert!(!store.needs_refresh());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An expired credential with no refresh token cannot be renewed silently, so it must report
    /// needing authorization rather than falling through to a device flow.
    #[test]
    fn load_saved_reports_needing_authorization_for_an_unrenewable_credential() {
        let dir = std::env::temp_dir().join(format!("gh-app-load-stale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("test temp dir");

        save_credential(
            &dir.join(PR_TOKEN_FILE),
            &Credential {
                access_token: "ghu_expired".to_string(),
                // Well in the past, and no refresh token to trade in.
                expires_at: Some(UNIX_EPOCH + Duration::from_secs(1)),
                refresh_token: None,
            },
        );

        let outcome = PrTokenStore::load_saved(&dir).expect("building an HTTP client must succeed");
        assert!(outcome.is_none(), "an unrenewable credential must mean authorization is needed");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Not setting the env var here: mutating global process environment from a test risks
    /// flaking against whatever else the run does in parallel, and Rust 2024 makes that mutation
    /// `unsafe` for exactly this reason. This just checks the two are consistent with whatever
    /// the process actually inherited.
    #[test]
    fn client_id_defaults_to_the_constant_but_respects_the_env_var() {
        let resolved = client_id();
        assert!(!resolved.is_empty());
        match std::env::var(CLIENT_ID_ENV) {
            Ok(set) if !set.trim().is_empty() => assert_eq!(resolved, set.trim()),
            _ => assert_eq!(resolved, CLIENT_ID),
        }
    }
}
