//! GitHub Device Code authentication and token storage.
//!
//! Every failure path here used to call `std::process::exit(1)`. That was survivable while the
//! token was only ever fetched once from `main`, but the poll thread now re-authenticates when
//! GitHub rejects a token mid-run — and an `exit` from a background thread would take the whole
//! tray icon down instead of reporting a problem. So the flow returns `Result` and lets the
//! caller decide whether the failure is fatal.

use crate::logln;
use reqwest::blocking::Client;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const ACCESS_TOKEN_FILE: &str = "access_token.txt";
const CLIENT_ID_FILE: &str = "client_id.txt";
const CLIENT_ID_PLACEHOLDER: &str = "YOUR_CLIENT_ID_HERE";

// ── Review credential (optional, separate on purpose) ─────────────────────────
// `GET /notifications` only accepts classic OAuth-app/PAT tokens with the `notifications` scope,
// so that credential cannot be narrowed or migrated. Searching for review requests in private
// repos needs different access entirely, and the only classic scope that grants it (`repo`) also
// grants write to every repository. So reviews get their own credential and the notifications
// token keeps its narrow scope.
//
// Two shapes are accepted, whichever the user can actually obtain:
//   * `review_token.txt`     — a fine-grained PAT (Pull requests: read). Used verbatim.
//   * `review_client_id.txt` — a GitHub App client ID; device flow, refresh token, no rotation.
const REVIEW_TOKEN_FILE: &str = "review_token.txt";
const REVIEW_CLIENT_ID_FILE: &str = "review_client_id.txt";
const REVIEW_REFRESH_TOKEN_FILE: &str = "review_refresh_token.txt";

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
    /// GitHub Apps return this when token expiration is enabled (8h access token, 6-month
    /// refresh token, and each refresh issues a fresh one — so the app rolls forward on its own).
    /// Absent for OAuth Apps and for GitHub Apps with expiration disabled.
    #[serde(default)]
    refresh_token: Option<String>,
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

/// Shows a message-box dialog on Windows.
/// `pub` so `main.rs` can reuse it for fatal-startup and single-instance notices.
#[cfg(target_os = "windows")]
pub fn win_msgbox(title: &str, msg: &str) {
    use std::ptr::null_mut;
    use winapi::um::winuser::{MessageBoxW, MB_ICONINFORMATION, MB_OK};

    let title_w: Vec<u16> = title.encode_utf16().chain(Some(0)).collect();
    let msg_w: Vec<u16> = msg.encode_utf16().chain(Some(0)).collect();
    unsafe {
        MessageBoxW(
            null_mut(),
            msg_w.as_ptr(),
            title_w.as_ptr(),
            MB_OK | MB_ICONINFORMATION,
        );
    }
}

/// Blocks until the user confirms (Windows: OK button, Linux: Enter key).
///
/// Only ever called during first-run setup from the main thread — the poll thread must never
/// block on stdin.
fn wait_for_user_confirmation(title: &str, msg: &str) {
    #[cfg(target_os = "windows")]
    win_msgbox(title, msg);

    #[cfg(not(target_os = "windows"))]
    {
        println!("\n{}: {}\nPress Enter to continue...", title, msg);
        let _ = std::io::stdin().read_line(&mut String::new());
    }
}

/// Displays the device-code prompt. On Windows this opens a non-blocking MessageBox in a
/// background thread so polling can proceed immediately.
///
/// The details also go to the log, because this is reachable from the poll thread long after
/// startup — and on Linux launched from a desktop entry there is no terminal to print to.
fn show_auth_prompt(user_code: &str, verification_uri: &str) {
    logln!("authorization required: open {verification_uri} and enter code {user_code}");

    #[cfg(target_os = "windows")]
    {
        let user_code = user_code.to_string();
        let verification_uri = verification_uri.to_string();
        std::thread::spawn(move || {
            win_msgbox(
                "GitHub Authorization Required",
                &format!(
                    "Open: {}\n\nEnter code: {}\n\nThis dialog can be closed once you have entered the code.",
                    verification_uri, user_code
                ),
            );
        });
    }
    #[cfg(not(target_os = "windows"))]
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

    #[cfg(target_os = "windows")]
    win_msgbox("GitHub Authorization", "Authorization successful!");
}

// ── Token storage ─────────────────────────────────────────────────────────────

/// Owns the access token and knows how to replace it.
///
/// Lives on the poll thread so a mid-run 401 can be recovered from without restarting the app.
pub struct TokenStore {
    token_path: PathBuf,
    client_id: String,
    token: String,
    http: Client,
}

impl TokenStore {
    /// Startup path: reuse a saved token if it still works, otherwise run the device flow.
    pub fn load(app_asset_path: &Path) -> Result<Self, AuthError> {
        let http = build_client()?;
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
                        return Ok(Self { token_path, client_id, token, http });
                    }
                    // Could not reach GitHub, so this says nothing about the token. Starting
                    // with a saved token beats refusing to start: a tray app is typically
                    // launched at login, often before the network is up, and the poll loop
                    // already re-authenticates if the token turns out to be genuinely dead.
                    TokenCheck::Unreachable => {
                        logln!("could not reach GitHub to check the saved token — using it anyway");
                        return Ok(Self { token_path, client_id, token, http });
                    }
                    TokenCheck::Rejected => {
                        logln!("saved token was rejected by GitHub — re-authenticating");
                    }
                }
            }
        }

        let grant = device_code_flow(&http, &client_id)?;
        save_token(&token_path, &grant.access_token);
        Ok(Self { token_path, client_id, token: grant.access_token, http })
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    /// Mid-run recovery after GitHub rejects the token. Returns `Err` rather than exiting so
    /// the caller can back off and keep the tray icon alive.
    pub fn reauthenticate(&mut self) -> Result<(), AuthError> {
        logln!("re-authenticating after GitHub rejected the current token");
        let grant = device_code_flow(&self.http, &self.client_id)?;
        save_token(&self.token_path, &grant.access_token);
        self.token = grant.access_token;
        Ok(())
    }
}

// ── Review credential setup (driven from the tray menu) ───────────────────────

const REVIEW_TOKEN_PLACEHOLDER: &str = "PASTE_YOUR_TOKEN_HERE";

/// The instructions live *inside* the file the user is about to edit, so the tray menu can hand
/// them the whole setup without them reading the README or touching a terminal.
const REVIEW_TOKEN_TEMPLATE: &str = "\
# ── Red dot: pull requests awaiting your review ───────────────────────────────
#
# Replace the last line of this file with a GitHub token, save, and close.
# The tray icon picks it up within a minute — no restart needed.
#
# Create one at:  https://github.com/settings/personal-access-tokens/new
#
#   Token name          anything, e.g. \"tray icon review dot\"
#   Resource owner      the ORG that owns the repos you review (not your user,
#                       unless the repos are your own)
#   Repository access   \"All repositories\", or select the repos you review.
#                       The default \"Public repositories\" will NOT work for
#                       private repos and the dot would stay dark.
#   Permissions         Repository permissions -> Pull requests -> Read-only
#                       (Metadata: Read-only is added for you)
#
# If the resource owner is an organisation, the token may need an admin to
# approve it. Until then GitHub returns no results and the dot stays dark --
# check the token's status on https://github.com/settings/personal-access-tokens
#
# This file is readable only by you. Delete its contents to turn the dot off.

PASTE_YOUR_TOKEN_HERE
";

/// Path to the review token file. `pub` so the UI can open it in the user's editor.
pub fn review_token_path(app_asset_path: &Path) -> PathBuf {
    app_asset_path.join(REVIEW_TOKEN_FILE)
}

/// Creates the review-token file with owner-only permissions and the instruction template, unless
/// it already holds something. Returns the path so the caller can open it in an editor.
///
/// Writing the template through `save_token` means the file is created at `0600` from the start,
/// so a token pasted into it is never briefly world-readable.
pub fn ensure_review_token_file(app_asset_path: &Path) -> PathBuf {
    let path = review_token_path(app_asset_path);
    if read_credential_file(&path).is_none() {
        save_token(&path, REVIEW_TOKEN_TEMPLATE);
    } else {
        // Already configured — just make sure the permissions are right before we open it.
        restrict_token_permissions(&path);
    }
    path
}

// ── Review credential ─────────────────────────────────────────────────────────

/// How the review credential was obtained, which determines how it can be renewed.
enum ReviewCredential {
    /// A fine-grained PAT read from disk. Cannot be renewed programmatically; if GitHub rejects
    /// it the user has to replace the file.
    StaticToken,
    /// A GitHub App via device flow. Renewable without any human involvement.
    DeviceFlow { client_id: String, refresh_token: Option<String> },
}

/// Optional second credential used only for the review-request search.
///
/// Absent configuration is a normal state, not an error: the app then behaves exactly as it did
/// before the dot feature existed.
pub struct ReviewTokenStore {
    token_path: PathBuf,
    refresh_path: PathBuf,
    credential: ReviewCredential,
    token: String,
    http: Client,
}

impl ReviewTokenStore {
    /// Returns `Ok(None)` when the user has not configured the feature.
    pub fn load(app_asset_path: &Path) -> Result<Option<Self>, AuthError> {
        let token_path = app_asset_path.join(REVIEW_TOKEN_FILE);
        let refresh_path = app_asset_path.join(REVIEW_REFRESH_TOKEN_FILE);

        // A pasted PAT takes precedence: it needs no network round trip to become usable.
        if let Some(token) = read_credential_file(&token_path) {
            restrict_token_permissions(&token_path);
            logln!("review credential: fine-grained token from {REVIEW_TOKEN_FILE}");
            return Ok(Some(Self {
                token_path,
                refresh_path,
                credential: ReviewCredential::StaticToken,
                token,
                http: build_client()?,
            }));
        }

        let Some(client_id) = read_credential_file(&app_asset_path.join(REVIEW_CLIENT_ID_FILE))
        else {
            return Ok(None); // feature not configured — no dot, no search requests
        };

        let http = build_client()?;
        let refresh_token = read_trimmed(&refresh_path);

        // Prefer a saved access token; fall back to refresh; fall back to a full device flow.
        if let Some(token) = read_trimmed(&token_path) {
            restrict_token_permissions(&token_path);
            return Ok(Some(Self {
                token_path,
                refresh_path,
                credential: ReviewCredential::DeviceFlow { client_id, refresh_token },
                token,
                http,
            }));
        }

        logln!("review credential: authorising GitHub App via device flow");
        let grant = device_code_flow(&http, &client_id)?;
        save_token(&token_path, &grant.access_token);
        if let Some(refresh) = &grant.refresh_token {
            save_token(&refresh_path, refresh);
        }

        Ok(Some(Self {
            token_path,
            refresh_path,
            credential: ReviewCredential::DeviceFlow {
                client_id,
                refresh_token: grant.refresh_token,
            },
            token: grant.access_token,
            http,
        }))
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    /// Picks up a token pasted into `review_token.txt` while the app is running.
    ///
    /// Called once per poll cycle, which is what makes the tray menu's setup flow work without a
    /// restart: the user saves the file and the next cycle adopts it. Deliberately limited to the
    /// static-token case — reloading a GitHub App client ID would mean starting a device flow, and
    /// a background thread must never open a browser prompt the user did not ask for.
    ///
    /// Returns `Some` only when the file holds a token that differs from the one in use, so the
    /// steady state is one small file read per minute and nothing else.
    pub fn reload_static_token(
        app_asset_path: &Path,
        current: Option<&ReviewTokenStore>,
    ) -> Option<Self> {
        let token_path = review_token_path(app_asset_path);
        let token = read_credential_file(&token_path)?;

        if current.is_some_and(|store| store.token == token) {
            return None; // unchanged
        }

        restrict_token_permissions(&token_path);
        logln!("review credential: picked up a new token from {REVIEW_TOKEN_FILE}");

        Some(Self {
            token_path,
            refresh_path: app_asset_path.join(REVIEW_REFRESH_TOKEN_FILE),
            credential: ReviewCredential::StaticToken,
            token,
            http: build_client().ok()?,
        })
    }

    /// Renews after GitHub rejects the credential.
    ///
    /// Tries the refresh grant first — that is the whole point of the GitHub App route, since it
    /// needs no human involvement — and only falls back to a full device flow (which does prompt)
    /// if refreshing fails.
    pub fn reauthenticate(&mut self) -> Result<(), AuthError> {
        let ReviewCredential::DeviceFlow { client_id, refresh_token } = &self.credential else {
            // A pasted PAT cannot be renewed from here.
            return Err(AuthError::Github(format!(
                "the token in {REVIEW_TOKEN_FILE} was rejected; replace it (it may have expired)"
            )));
        };
        let client_id = client_id.clone();

        if let Some(refresh) = refresh_token.clone() {
            logln!("review credential: refreshing without a prompt");
            match refresh_access_token(&self.http, &client_id, &refresh) {
                Ok(grant) => {
                    self.store(client_id, grant);
                    return Ok(());
                }
                Err(e) => logln!("review credential: refresh failed ({e}) — falling back to device flow"),
            }
        }

        logln!("review credential: re-authorising via device flow");
        let grant = device_code_flow(&self.http, &client_id)?;
        self.store(client_id, grant);
        Ok(())
    }

    fn store(&mut self, client_id: String, grant: Grant) {
        save_token(&self.token_path, &grant.access_token);
        if let Some(refresh) = &grant.refresh_token {
            save_token(&self.refresh_path, refresh);
        }
        self.token = grant.access_token;
        self.credential = ReviewCredential::DeviceFlow {
            client_id,
            // Each refresh issues a new refresh token; keeping the old one would break the next
            // renewal. Only overwrite when GitHub actually sent a replacement.
            refresh_token: grant.refresh_token.or_else(|| read_trimmed(&self.refresh_path)),
        };
    }
}

/// Reads a file and returns its trimmed contents, or `None` if missing or blank.
fn read_trimmed(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// First line that is neither blank nor a `#` comment.
///
/// Both credential files are handed to the user as commented templates, so the instructions have
/// to survive being read back as a value.
fn first_meaningful_line(content: &str) -> Option<&str> {
    content
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
}

/// Reads a credential out of a possibly-templated file, ignoring comments and placeholders.
fn read_credential_file(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let value = first_meaningful_line(&content)?;
    (!value.is_empty() && value != REVIEW_TOKEN_PLACEHOLDER && value != CLIENT_ID_PLACEHOLDER)
        .then(|| value.to_string())
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
    let client_id = content
        .lines()
        .find(|l| !l.trim_start().starts_with('#') && !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_string();

    if client_id.is_empty() || client_id == CLIENT_ID_PLACEHOLDER {
        return Err(AuthError::NoClientId);
    }

    // Overwrite with just the clean ID so future reads skip the comment parsing.
    let _ = std::fs::write(client_id_path, &client_id);

    Ok(client_id)
}

// ── Device code flow ──────────────────────────────────────────────────────────

/// A successful authorization. `refresh_token` is present only for GitHub Apps with token
/// expiration enabled.
struct Grant {
    access_token: String,
    refresh_token: Option<String>,
}

/// Exchanges a refresh token for a new access token. No user interaction at all.
///
/// GitHub Apps and OAuth Apps share these endpoints, so this reuses the same URL as the device
/// flow with a different `grant_type`.
fn refresh_access_token(
    http: &Client,
    client_id: &str,
    refresh_token: &str,
) -> Result<Grant, AuthError> {
    let resp: TokenPollResponse = http
        .post("https://github.com/login/oauth/access_token")
        .header("Accept", "application/json")
        .header("User-Agent", AGENT)
        .form(&[
            ("client_id", client_id),
            ("refresh_token", refresh_token),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .and_then(|r| r.json())
        .map_err(|e| AuthError::Network(e.to_string()))?;

    match resp.access_token {
        Some(access_token) => Ok(Grant { access_token, refresh_token: resp.refresh_token }),
        None => Err(AuthError::Github(
            resp.error.unwrap_or_else(|| "refresh returned no access token".to_string()),
        )),
    }
}

/// Runs the GitHub Device Code flow and returns the resulting grant.
///
/// Works unchanged for both OAuth Apps and GitHub Apps — they share
/// `POST /login/device/code` and `POST /login/oauth/access_token`.
fn device_code_flow(http: &Client, client_id: &str) -> Result<Grant, AuthError> {
    // ── Step 1: request a device code ─────────────────────────────────────────
    let dc: DeviceCodeResponse = http
        .post("https://github.com/login/device/code")
        .header("Accept", "application/json")
        .header("User-Agent", AGENT)
        .form(&[("client_id", client_id), ("scope", NOTIFICATION_SCOPE)])
        .send()
        .and_then(|r| r.json())
        .map_err(|e| AuthError::Network(e.to_string()))?;

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
            return Ok(Grant { access_token, refresh_token: resp.refresh_token });
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

    /// The template the tray menu writes must never be mistaken for a real credential. Adopting
    /// the placeholder would send it to GitHub, earn a 401, and kick off a pointless renewal.
    #[test]
    fn the_written_template_yields_no_credential() {
        let dir = std::env::temp_dir().join(format!("gst-tmpl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = ensure_review_token_file(&dir);

        assert!(path.exists(), "the menu item must create the file");
        assert_eq!(
            read_credential_file(&path),
            None,
            "an untouched template must read as unconfigured"
        );

        // …and permissions must be owner-only from the moment it is created.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "template must not be world-readable");
        }

        // A pasted token replaces the last line; the comments above it stay put.
        let pasted = std::fs::read_to_string(&path)
            .unwrap()
            .replace(REVIEW_TOKEN_PLACEHOLDER, "github_pat_example");
        std::fs::write(&path, pasted).unwrap();
        assert_eq!(read_credential_file(&path).as_deref(), Some("github_pat_example"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn placeholders_are_never_read_as_values() {
        let dir = std::env::temp_dir().join(format!("gst-ph-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("x.txt");

        std::fs::write(&path, CLIENT_ID_PLACEHOLDER).unwrap();
        assert_eq!(read_credential_file(&path), None);

        std::fs::write(&path, REVIEW_TOKEN_PLACEHOLDER).unwrap();
        assert_eq!(read_credential_file(&path), None);

        std::fs::remove_dir_all(&dir).ok();
    }
}
