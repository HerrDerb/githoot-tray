use reqwest::blocking::Client;
use serde::Deserialize;
use std::path::Path;
use std::time::{Duration, Instant};

const ACCESS_TOKEN_FILE: &str = "access_token.txt";
const CLIENT_ID_FILE: &str = "client_id.txt";

const NOTIFICATION_SCOPE: &str = "notifications";

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

// ── Platform helpers ──────────────────────────────────────────────────────────

/// Shows a message-box dialog on Windows.
/// `pub` so `main.rs` can reuse it for the single-instance warning.
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
fn wait_for_user_confirmation(title: &str, msg: &str) {
    #[cfg(target_os = "windows")]
    win_msgbox(title, msg);

    #[cfg(not(target_os = "windows"))]
    {
        println!("\n{}: {}\nPress Enter to continue...", title, msg);
        let _ = std::io::stdin().read_line(&mut String::new());
    }
}

/// Displays the device-code prompt. On Windows this opens a non-blocking
/// MessageBox in a background thread so polling can proceed immediately.
fn show_auth_prompt(user_code: &str, verification_uri: &str) {
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
    #[cfg(target_os = "windows")]
    win_msgbox("GitHub Authorization", "Authorization successful!");
    #[cfg(not(target_os = "windows"))]
    println!("Authorization successful!");
}

// ── Client ID ─────────────────────────────────────────────────────────────────

/// Returns the saved OAuth client ID, prompting the user to enter one if missing.
pub fn get_client_id(app_asset_path: &Path) -> String {
    let client_id_path = app_asset_path.join(CLIENT_ID_FILE);

    if let Ok(saved) = std::fs::read_to_string(&client_id_path) {
        let id = saved.trim().to_string();
        if !id.is_empty() {
            return id;
        }
    }

    prompt_for_client_id(&client_id_path)
}

/// Writes a template file and opens it in the default editor, then waits for
/// the user to confirm before reading the entered Client ID back.
fn prompt_for_client_id(client_id_path: &Path) -> String {
    let instructions = "# GitHub OAuth App Client ID\n\
        #\n\
        # 1. Go to https://github.com/settings/developers\n\
        # 2. Create a new OAuth App (enable Device Flow, scope: notifications)\n\
        # 3. Replace the line below with your Client ID and save the file\n\
        \n\
        YOUR_CLIENT_ID_HERE\n";

    if let Err(e) = std::fs::write(client_id_path, instructions) {
        eprintln!("Failed to create client_id.txt: {e}");
        std::process::exit(1);
    }

    // Open the file in the default editor (non-blocking).
    if let Err(e) = open::that(client_id_path) {
        eprintln!("Could not open editor automatically: {e}");
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

    if client_id.is_empty() || client_id == "YOUR_CLIENT_ID_HERE" {
        eprintln!("No valid Client ID found. Please restart and enter your Client ID.");
        std::process::exit(1);
    }

    // Overwrite the file with just the clean ID (strips comments for future reads).
    let _ = std::fs::write(client_id_path, &client_id);

    client_id
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Retrieves a valid GitHub access token, using the device code flow if needed.
pub fn get_access_token(app_asset_path: &Path) -> String {
    let token_path = app_asset_path.join(ACCESS_TOKEN_FILE);
    let client_id = get_client_id(app_asset_path);

    // Re-use an existing valid token to avoid re-authenticating on every launch.
    if let Ok(saved) = std::fs::read_to_string(&token_path) {
        let token = saved.trim().to_string();
        if !token.is_empty() && verify_access_token(&token) {
            return token;
        }
        eprintln!("Saved token is no longer valid — starting re-authentication.");
    }

    let token = device_code_flow(&client_id);

    if let Err(e) = std::fs::write(&token_path, &token) {
        eprintln!("Warning: could not save access token to disk: {e}");
    }

    token
}

/// Runs the GitHub Device Code OAuth flow and returns the access token.
fn device_code_flow(client_id: &str) -> String {
    let client = Client::new();

    // ── Step 1: request a device code ─────────────────────────────────────────
    let dc: DeviceCodeResponse = client
        .post("https://github.com/login/device/code")
        .header("Accept", "application/json")
        .header("User-Agent", "git-system-tray")
        .form(&[("client_id", client_id), ("scope", NOTIFICATION_SCOPE)])
        .send()
        .expect("Failed to request device code")
        .json()
        .expect("Failed to parse device code response");

    // ── Step 2: prompt the user ────────────────────────────────────────────────
    if let Err(e) = open::that(&dc.verification_uri) {
        eprintln!("(Could not open browser automatically: {e})");
    }
    show_auth_prompt(&dc.user_code, &dc.verification_uri);

    // ── Step 3: poll until authorized or expired ───────────────────────────────
    let mut poll_interval = Duration::from_secs(dc.interval.max(5));
    let expires_at = Instant::now() + Duration::from_secs(dc.expires_in);

    loop {
        if Instant::now() >= expires_at {
            eprintln!("Device code expired. Please restart the application.");
            std::process::exit(1);
        }

        std::thread::sleep(poll_interval);

        let resp: TokenPollResponse = client
            .post("https://github.com/login/oauth/access_token")
            .header("Accept", "application/json")
            .header("User-Agent", "git-system-tray")
            .form(&[
                ("client_id", client_id),
                ("device_code", dc.device_code.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send()
            .expect("Failed to poll for access token")
            .json()
            .expect("Failed to parse token poll response");

        if let Some(token) = resp.access_token {
            show_auth_success();
            return token;
        }

        match resp.error.as_deref() {
            Some("authorization_pending") => {}
            Some("slow_down") => {
                poll_interval += Duration::from_secs(5);
            }
            Some("expired_token") => {
                eprintln!("Device code expired. Please restart the application.");
                std::process::exit(1);
            }
            Some("access_denied") => {
                eprintln!("Authorization was denied. Please restart and try again.");
                std::process::exit(1);
            }
            Some(other) => {
                eprintln!("Unexpected authorization error: {other}");
                std::process::exit(1);
            }
            None => {}
        }
    }
}

/// Verifies the token is still valid by calling the GitHub user endpoint.
fn verify_access_token(token: &str) -> bool {
    use reqwest::header::{ACCEPT, AUTHORIZATION};
    let client = Client::new();
    match client
        .get("https://api.github.com/user")
        .header(ACCEPT, "application/vnd.github+json")
        .header(AUTHORIZATION, format!("Bearer {}", token))
        .header("User-Agent", "git-system-tray")
        .send()
    {
        Ok(r) => r.status().is_success(),
        Err(e) => {
            eprintln!("Failed to verify access token: {e}");
            false
        }
    }
}

