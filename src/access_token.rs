//! Notifications credential: `gh`, falling back to GitHub Device Code authentication.
//!
//! `gh`'s token is tried first, exactly as it is for the review search in `gh_cli` — one fewer
//! credential for a user who already has `gh` set up to create and store. It is only usable here
//! when its scopes are *known* to include `notifications`; unlike the review search, there is no
//! live check afterwards that can catch a wrong guess and turn the feature off (see
//! `gh_backed_token`), so an unknown scope set falls back rather than being optimistically
//! accepted. Falling back means the OAuth App device flow below, which is the only path that
//! existed before `gh` support was added: it prompts for a Client ID once, then authenticates via
//! browser and stores the resulting token on disk.
//!
//! Every failure path here used to call `std::process::exit(1)`. That was survivable while the
//! token was only ever fetched once from `main`, but the poll thread now re-authenticates when
//! GitHub rejects a token mid-run — and an `exit` from a background thread would take the whole
//! tray icon down instead of reporting a problem. So the flow returns `Result` and lets the
//! caller decide whether the failure is fatal.

use crate::gh_cli;
use crate::logln;
use reqwest::blocking::Client;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const ACCESS_TOKEN_FILE: &str = "access_token.txt";
const CLIENT_ID_FILE: &str = "client_id.txt";
const CLIENT_ID_PLACEHOLDER: &str = "YOUR_CLIENT_ID_HERE";

// ── Review credential ─────────────────────────────────────────────────────────
// `GET /notifications` only accepts classic OAuth-app/PAT tokens with the `notifications` scope, so
// the credential below cannot be narrowed or migrated, and it stays exactly as it was. Searching for
// review requests in private repos needs different access entirely: the only classic scope that
// grants it (`repo`) also grants write to every repository. Rather than escalate this token that
// far, the review search borrows the credential `gh` already holds. See `gh_cli`.

const NOTIFICATION_SCOPE: &str = "notifications";
const AGENT: &str = "git-system-tray";

const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// GitHub's device-flow `interval` is respected, but never dips below this.
const MIN_DEVICE_POLL_INTERVAL: u64 = 5;

/// Backoff added when GitHub answers `slow_down`.
const SLOW_DOWN_PENALTY: Duration = Duration::from_secs(5);

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: u64,
}

#[derive(Deserialize)]
struct TokenPollResponse {
    access_token: Option<String>,
    error: Option<String>,
}

// ── Errors ────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum AuthError {
    /// No usable Client ID on disk; the app cannot authenticate at all.
    NoClientId,
    /// Transport failure or an unreadable response.
    Network(String),
    /// The user declined authorization.
    Denied,
    /// The device code ran out before the user finished.
    Expired,
    /// GitHub reported something we do not have a specific case for.
    Github(String),
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::NoClientId => write!(f, "no valid GitHub OAuth Client ID configured"),
            AuthError::Network(e) => write!(f, "network error during authorization: {e}"),
            AuthError::Denied => write!(f, "authorization was denied"),
            AuthError::Expired => write!(f, "device code expired before authorization completed"),
            AuthError::Github(e) => write!(f, "GitHub reported: {e}"),
        }
    }
}

impl std::error::Error for AuthError {}

// ── Platform helpers ──────────────────────────────────────────────────────────

// Windows and macOS both run without a usable console — Windows because of
// `windows_subsystem = "windows"`, macOS because the release artefact is an `LSUIElement` app
// bundle — so both take the dialog path below wherever the two differ from Linux.

/// Blocks until the user confirms (Windows/macOS: OK button, Linux: Enter key).
///
/// Only ever called during first-run setup from the main thread — the poll thread must never
/// block on stdin. `dialog::message` already picks the right mechanism per platform; this stays a
/// named wrapper so that contract has somewhere to be written down.
fn wait_for_user_confirmation(title: &str, msg: &str) {
    crate::dialog::message(title, msg);
}

/// Displays the device-code prompt. Where dialogs are used this opens a non-blocking one in a
/// background thread so polling can proceed immediately.
///
/// The details also go to the log, because this is reachable from the poll thread long after
/// startup — and on Linux launched from a desktop entry there is no terminal to print to.
fn show_auth_prompt(user_code: &str, verification_uri: &str) {
    logln!("authorization required: open {verification_uri} and enter code {user_code}");

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    {
        let user_code = user_code.to_string();
        let verification_uri = verification_uri.to_string();
        std::thread::spawn(move || {
            crate::dialog::message(
                "GitHub Authorization Required",
                &format!(
                    "Open: {}\n\nEnter code: {}\n\nThis dialog can be closed once you have entered the code.",
                    verification_uri, user_code
                ),
            );
        });
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        println!();
        println!("━━━  GitHub Authorization Required  ━━━");
        println!("  1. Open:  {}", verification_uri);
        println!("  2. Enter: {}", user_code);
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!();
    }
}

/// Notifies the user that authorization succeeded.
fn show_auth_success() {
    logln!("authorization successful");

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    crate::dialog::message("GitHub Authorization", "Authorization successful!");
}

// ── Credential source ─────────────────────────────────────────────────────────

/// Where the current token comes from, and what renewing it takes.
///
/// `Gh` persists nothing of its own — `gh` owns that credential's lifetime, storage and renewal,
/// exactly as it does for the review credential in `gh_cli`. `DeviceFlow` is the pre-`gh` path:
/// a user-registered OAuth App, a token saved to disk, and a browser round trip to replace it.
enum Source {
    Gh { host: String },
    DeviceFlow { token_path: PathBuf, client_id: String },
}

/// Why `gh` did not supply the notifications token, as far as `load` needs to know.
///
/// Only "not on `PATH` at all" is worth interrupting startup for with a hint — everything else
/// means `gh` is present and already known to the user (not logged in, wrong scope, a slow
/// keyring), so it is logged and left there, the same way the other fallback reasons are.
enum GhFallback {
    NotInstalled,
    Other,
}

/// Tries to source the notifications token from `gh`, the same way `gh_cli::ReviewToken` borrows
/// one for the review search. `Err` on anything short of a token confirmed to carry the
/// `notifications` scope — never treated as an application error, since not having `gh` (or not
/// having refreshed its scopes) is a normal reason to fall back to the device flow.
///
/// Deliberately stricter than the review search's scope check: that one treats *unknown* scopes as
/// good enough to proceed, because a wrong guess there is caught by a live check against GitHub's
/// answer a moment later and the feature turns itself off (see `gh_cli::lacks_required_scope`'s use
/// in `scheduler`). Nothing here re-checks after the fact — notifications are core to the app, not
/// an optional dot — so an unknown scope set falls back instead of gambling on it.
fn gh_backed_token() -> Result<(String, String), GhFallback> {
    let host = gh_cli::host();

    let account = match gh_cli::auth_status(&host) {
        Ok(Some(account)) => account,
        Ok(None) => {
            logln!("gh has no active account for {host} — using the notifications OAuth App instead");
            return Err(GhFallback::Other);
        }
        Err(gh_cli::GhError::NotInstalled) => {
            logln!("gh not found on PATH — using the notifications OAuth App instead");
            return Err(GhFallback::NotInstalled);
        }
        Err(e) => {
            logln!(
                "gh unavailable ({}) — using the notifications OAuth App instead",
                gh_cli::Unavailable::from(e).short()
            );
            return Err(GhFallback::Other);
        }
    };

    if !gh_cli::has_scope(&account.scopes, gh_cli::NOTIFICATIONS_SCOPE) {
        logln!(
            "gh's token is not known to carry the notifications scope — using the notifications \
             OAuth App instead. To use gh instead, run: gh auth refresh --scopes notifications"
        );
        return Err(GhFallback::Other);
    }

    match gh_cli::auth_token(&host) {
        Ok(token) => {
            logln!("notifications credential: gh cli, host {host}, login {}", account.login);
            Ok((host, token))
        }
        Err(e) => {
            logln!(
                "gh auth token failed ({}) — using the notifications OAuth App instead",
                gh_cli::Unavailable::from(e).short()
            );
            Err(GhFallback::Other)
        }
    }
}

/// Tells the user `gh` was not found and lets them choose whether to keep going with the OAuth App
/// flow now, or exit so they can install `gh` first.
///
/// Startup only — this is `load`'s decision to make. `reauthenticate` never calls this: `gh` going
/// missing mid-run is not a reason to interrupt a tray app the user is not looking at.
fn offer_gh_install_hint() -> bool {
    let title = "git-system-tray: GitHub CLI not found";
    let msg = "This app can use the GitHub CLI (gh) for notifications if it is installed and \
               logged in with the `notifications` scope — one less sign-in to manage.\n\n\
               gh was not found on PATH. You can continue with a separate one-time sign-in \
               instead, or exit now to install gh (https://cli.github.com) and run:\n\n    \
               gh auth login\n    gh auth refresh --scopes notifications";
    crate::dialog::confirm(title, msg, "Continue", "Exit")
}

// ── Token storage ─────────────────────────────────────────────────────────────

/// Owns the access token and knows how to replace it.
///
/// Lives on the poll thread so a mid-run 401 can be recovered from without restarting the app.
pub struct TokenStore {
    source: Source,
    token: String,
    http: Client,
}

impl TokenStore {
    /// Startup path: prefer a `gh`-supplied token; otherwise reuse a saved device-flow token if it
    /// still works, or run the device flow.
    pub fn load(app_asset_path: &Path) -> Result<Self, AuthError> {
        let http = build_client()?;

        match gh_backed_token() {
            Ok((host, token)) => return Ok(Self { source: Source::Gh { host }, token, http }),
            Err(GhFallback::NotInstalled) => {
                if !offer_gh_install_hint() {
                    logln!("exiting at the user's request to install gh");
                    std::process::exit(0);
                }
            }
            Err(GhFallback::Other) => {}
        }

        let client_id = get_client_id(app_asset_path)?;
        let token_path = app_asset_path.join(ACCESS_TOKEN_FILE);

        if let Ok(saved) = std::fs::read_to_string(&token_path) {
            // Tokens written by an earlier version landed at the umask default (typically
            // 0644/0664, i.e. readable by anyone with an account on the machine). Tighten in
            // place rather than waiting for the next write.
            restrict_token_permissions(&token_path);

            let token = saved.trim().to_string();
            if !token.is_empty() {
                match check_access_token(&http, &token) {
                    TokenCheck::Valid => {
                        return Ok(Self { source: Source::DeviceFlow { token_path, client_id }, token, http });
                    }
                    // Could not reach GitHub, so this says nothing about the token. Starting
                    // with a saved token beats refusing to start: a tray app is typically
                    // launched at login, often before the network is up, and the poll loop
                    // already re-authenticates if the token turns out to be genuinely dead.
                    TokenCheck::Unreachable => {
                        logln!("could not reach GitHub to check the saved token — using it anyway");
                        return Ok(Self { source: Source::DeviceFlow { token_path, client_id }, token, http });
                    }
                    TokenCheck::Rejected => {
                        logln!("saved token was rejected by GitHub — re-authenticating");
                    }
                }
            }
        }

        let token = device_code_flow(&http, &client_id)?;
        save_token(&token_path, &token);
        Ok(Self { source: Source::DeviceFlow { token_path, client_id }, token, http })
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    /// Mid-run recovery after GitHub rejects the token. Returns `Err` rather than exiting so
    /// the caller can back off and keep the tray icon alive.
    ///
    /// The `Gh` branch never falls back to the device flow: it mirrors
    /// `gh_cli::ReviewToken::refresh`, simply re-asking `gh` for its current token, which is
    /// correct for the common case (`gh` rotated the token underneath us) and otherwise degrades
    /// the same way a rejected review token does — logged and retried no sooner than
    /// `MIN_REAUTH_INTERVAL`, rather than switching credential sources mid-run.
    pub fn reauthenticate(&mut self) -> Result<(), AuthError> {
        match &self.source {
            Source::Gh { host } => {
                logln!("re-authenticating after GitHub rejected the current token — re-asking gh");
                let token = gh_cli::auth_token(host)
                    .map_err(|e| AuthError::Github(gh_cli::Unavailable::from(e).message()))?;
                self.token = token;
                Ok(())
            }
            Source::DeviceFlow { token_path, client_id } => {
                logln!("re-authenticating after GitHub rejected the current token");
                let token = device_code_flow(&self.http, client_id)?;
                save_token(token_path, &token);
                self.token = token;
                Ok(())
            }
        }
    }
}

// ── Review credential ─────────────────────────────────────────────────────────
// Nothing here. The review search borrows `gh`'s credential; see `gh_cli::ReviewToken`.

/// First line that is neither blank nor a `#` comment.
///
/// The client ID file is handed to the user as a commented template, so the instructions have to
/// survive being read back as a value.
fn first_meaningful_line(content: &str) -> Option<&str> {
    content
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
}

fn build_client() -> Result<Client, AuthError> {
    Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|e| AuthError::Network(e.to_string()))
}

/// Restricts an existing token file to owner-only access. No-op off Unix, where the user
/// profile directory's inherited ACLs already limit access.
fn restrict_token_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
            logln!("warning: could not restrict permissions on the token file: {e}");
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Writes the token with owner-only permissions.
///
/// Creating the file at `0600` rather than writing and then `chmod`-ing avoids a window in
/// which the token sits on disk world-readable. `set_permissions` afterwards covers the case
/// where the file already existed at the old default of `0644`.
fn save_token(path: &Path, token: &str) {
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
            .and_then(|mut file| file.write_all(token.as_bytes()));

        if result.is_ok() {
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        result
    };

    #[cfg(not(unix))]
    let written = std::fs::write(path, token);

    if let Err(e) = written {
        logln!("warning: could not save access token to disk: {e}");
    }
}

// ── Client ID ─────────────────────────────────────────────────────────────────

/// Returns the saved OAuth client ID, prompting the user to enter one if missing.
fn get_client_id(app_asset_path: &Path) -> Result<String, AuthError> {
    let client_id_path = app_asset_path.join(CLIENT_ID_FILE);

    if let Ok(saved) = std::fs::read_to_string(&client_id_path) {
        let id = saved.trim().to_string();
        if !id.is_empty() && id != CLIENT_ID_PLACEHOLDER {
            return Ok(id);
        }
    }

    prompt_for_client_id(&client_id_path)
}

/// Writes a template file and opens it in the default editor, then waits for the user to
/// confirm before reading the entered Client ID back.
fn prompt_for_client_id(client_id_path: &Path) -> Result<String, AuthError> {
    let instructions = format!(
        "# GitHub OAuth App Client ID\n\
         #\n\
         # 1. Go to https://github.com/settings/developers\n\
         # 2. Create a new OAuth App (enable Device Flow, scope: notifications)\n\
         # 3. Replace the line below with your Client ID and save the file\n\
         \n\
         {CLIENT_ID_PLACEHOLDER}\n"
    );

    if let Err(e) = std::fs::write(client_id_path, instructions) {
        logln!("failed to create client_id.txt: {e}");
        return Err(AuthError::NoClientId);
    }

    // Open the file in the default editor (non-blocking).
    if let Err(e) = open::that(client_id_path) {
        logln!("could not open editor automatically: {e}");
    }

    wait_for_user_confirmation(
        "GitHub Setup",
        &format!(
            "Enter your GitHub OAuth App Client ID in the file:\n{}\n\nSave the file, then click OK.",
            client_id_path.display()
        ),
    );

    let content = std::fs::read_to_string(client_id_path).unwrap_or_default();
    let client_id = first_meaningful_line(&content).unwrap_or_default().to_string();

    if client_id.is_empty() || client_id == CLIENT_ID_PLACEHOLDER {
        return Err(AuthError::NoClientId);
    }

    // Overwrite with just the clean ID so future reads skip the comment parsing.
    let _ = std::fs::write(client_id_path, &client_id);

    Ok(client_id)
}

// ── Device code flow ──────────────────────────────────────────────────────────

/// The shape GitHub's OAuth endpoints use for a rejected request, per RFC 8628 §3.2.
#[derive(Deserialize)]
struct OAuthErrorResponse {
    error: String,
    error_description: Option<String>,
}

/// Turns a `/login/device/code` response that failed to parse as `DeviceCodeResponse` into a
/// message that names the actual problem.
fn describe_device_code_failure(status: reqwest::StatusCode, body: &str) -> String {
    if let Ok(err) = serde_json::from_str::<OAuthErrorResponse>(body) {
        // The single most common cause of this whole path: a freshly created OAuth App defaults
        // to Device Flow turned off, and this is the one error code GitHub uses for that.
        let hint = if err.error == "unauthorized_client" {
            " — in the OAuth App's settings on GitHub, check \"Enable Device Flow\""
        } else {
            ""
        };
        return match err.error_description {
            Some(desc) => format!("{desc}{hint} ({})", err.error),
            None => format!("{}{hint}", err.error),
        };
    }

    format!("GitHub answered {status} with an unexpected response: {body}")
}

/// Runs the GitHub Device Code flow and returns the access token.
///
/// Only the notifications credential uses this. The review search never does: `gh` owns that
/// credential's lifetime, so there is no refresh grant to handle here.
fn device_code_flow(http: &Client, client_id: &str) -> Result<String, AuthError> {
    // ── Step 1: request a device code ─────────────────────────────────────────
    // Read the body as text first rather than deserializing the response directly into
    // `DeviceCodeResponse`. GitHub answers a rejected request (most commonly a Client ID whose
    // OAuth App has not had "Enable Device Flow" turned on, but also a bad Client ID, or a
    // corporate proxy substituting its own page) with a *differently shaped* JSON body, and
    // `DeviceCodeResponse`'s fields are all required — so deserializing straight into it turns
    // GitHub's actual explanation into an opaque "missing field `device_code`" that names an
    // implementation detail instead of the problem.
    let response = http
        .post("https://github.com/login/device/code")
        .header("Accept", "application/json")
        .header("User-Agent", AGENT)
        .form(&[("client_id", client_id), ("scope", NOTIFICATION_SCOPE)])
        .send()
        .map_err(|e| AuthError::Network(e.to_string()))?;
    let status = response.status();
    let body = response.text().map_err(|e| AuthError::Network(e.to_string()))?;

    let dc: DeviceCodeResponse = serde_json::from_str(&body)
        .map_err(|_| AuthError::Github(describe_device_code_failure(status, &body)))?;

    // ── Step 2: prompt the user ───────────────────────────────────────────────
    if let Err(e) = open::that(&dc.verification_uri) {
        logln!("could not open browser automatically: {e}");
    }
    show_auth_prompt(&dc.user_code, &dc.verification_uri);

    // ── Step 3: poll until authorized or expired ──────────────────────────────
    let mut poll_interval = Duration::from_secs(dc.interval.max(MIN_DEVICE_POLL_INTERVAL));
    let expires_at = Instant::now() + Duration::from_secs(dc.expires_in);

    loop {
        if Instant::now() >= expires_at {
            return Err(AuthError::Expired);
        }

        std::thread::sleep(poll_interval);

        let resp: TokenPollResponse = http
            .post("https://github.com/login/oauth/access_token")
            .header("Accept", "application/json")
            .header("User-Agent", AGENT)
            .form(&[
                ("client_id", client_id),
                ("device_code", dc.device_code.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send()
            .and_then(|r| r.json())
            .map_err(|e| AuthError::Network(e.to_string()))?;

        if let Some(access_token) = resp.access_token {
            show_auth_success();
            return Ok(access_token);
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

/// What a startup token check established.
///
/// Deliberately three values, not a `bool`. Collapsing `Unreachable` into "invalid" is what made
/// the app destroy a working token and exit whenever it started before the network was ready.
enum TokenCheck {
    Valid,
    /// GitHub answered, and the answer was "no".
    Rejected,
    /// GitHub did not answer, so we learned nothing about the token.
    Unreachable,
}

/// Asks the GitHub user endpoint whether the token still works.
fn check_access_token(http: &Client, token: &str) -> TokenCheck {
    use reqwest::header::{ACCEPT, AUTHORIZATION};

    let response = http
        .get("https://api.github.com/user")
        .header(ACCEPT, "application/vnd.github+json")
        .header(AUTHORIZATION, format!("Bearer {token}"))
        .header("User-Agent", AGENT)
        .send();

    match response {
        Ok(r) if r.status().is_success() => TokenCheck::Valid,
        Ok(r) if r.status() == reqwest::StatusCode::UNAUTHORIZED => TokenCheck::Rejected,
        // A 5xx or a rate-limit 403 is GitHub having a bad day, not a verdict on the token.
        Ok(r) => {
            logln!("token check returned {} — treating as inconclusive", r.status());
            TokenCheck::Unreachable
        }
        Err(e) => {
            logln!("could not check access token: {e}");
            TokenCheck::Unreachable
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_and_blank_lines_are_skipped() {
        let content = "# a comment\n\n   # indented comment\n\n  ghp_realvalue  \nignored\n";
        assert_eq!(first_meaningful_line(content), Some("ghp_realvalue"));
    }

    #[test]
    fn a_file_of_only_comments_has_no_value() {
        assert_eq!(first_meaningful_line("# just\n# comments\n\n"), None);
    }

    /// The placeholder must never be adopted as a client ID: sending it to GitHub would earn a
    /// rejection and leave the user staring at an unexplained failure.
    #[test]
    fn the_placeholder_is_never_read_as_a_value() {
        let template = format!("# instructions\n#\n\n{CLIENT_ID_PLACEHOLDER}\n");
        assert_eq!(first_meaningful_line(&template), Some(CLIENT_ID_PLACEHOLDER));
        // The caller compares against the placeholder before using it; this documents that pairing.
        assert_eq!(first_meaningful_line("# only comments\n\n"), None);
    }

    /// The exact symptom this function exists to prevent: a bug report quoting a serde field
    /// name instead of a reason a human can act on.
    #[test]
    fn an_unauthorized_client_names_the_device_flow_checkbox() {
        let body = r#"{"error":"unauthorized_client","error_description":"The application has not enabled Device Flow."}"#;
        let msg = describe_device_code_failure(reqwest::StatusCode::BAD_REQUEST, body);
        assert!(msg.contains("Enable Device Flow"), "got {msg:?}");
        assert!(msg.contains("has not enabled Device Flow"), "the raw reason must survive too: {msg:?}");
    }

    #[test]
    fn a_different_oauth_error_is_reported_without_the_device_flow_hint() {
        let body = r#"{"error":"invalid_client"}"#;
        let msg = describe_device_code_failure(reqwest::StatusCode::UNAUTHORIZED, body);
        assert_eq!(msg, "invalid_client");
        assert!(!msg.contains("Device Flow"), "the hint is specific to unauthorized_client: {msg:?}");
    }

    /// A body that is not the OAuth error shape at all — a proxy's own error page, say — must
    /// still produce something readable rather than failing a second time.
    #[test]
    fn an_unrecognised_body_falls_back_to_status_and_raw_text() {
        let msg = describe_device_code_failure(reqwest::StatusCode::BAD_GATEWAY, "<html>blocked</html>");
        assert!(msg.contains("502"), "got {msg:?}");
        assert!(msg.contains("<html>blocked</html>"), "got {msg:?}");
    }
}
