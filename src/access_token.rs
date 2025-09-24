use std::path::Path;

/// Access token management for GitHub Tray Icon.

const ACCESS_TOKEN_FILE: &str = "access_token.txt";

/// Retrieves a valid GitHub access token from the asset path, prompting the user if necessary.
pub fn get_access_token(app_asset_path: &Path) -> String {
    let access_token_path = app_asset_path.join(ACCESS_TOKEN_FILE);
    match std::fs::read_to_string(&access_token_path) {
        Ok(token) => {
            let token = token.trim();
            if !token.is_empty() && verify_access_token(token) {
                return token.to_string();
            } else {
                eprintln!("Invalid access token in access_token.txt");
            }
        }
        Err(_) => {}
    }
    let new_access_token = enter_new_access_token(app_asset_path);
    if verify_access_token(&new_access_token) {
        return new_access_token;
    }
    eprintln!("Invalid access token. Please provide a valid access token in access_token.txt");
    std::process::exit(1);
}

/// Prompts the user to enter a new access token by opening the file in the default editor.
fn enter_new_access_token(app_asset_path: &Path) -> String {
    let access_token_path = app_asset_path.join(ACCESS_TOKEN_FILE);
    // Delete file if it exists
    if access_token_path.exists() {
        if let Err(e) = std::fs::remove_file(&access_token_path) {
            eprintln!("Failed to delete empty access token file: {e}");
        }
    }
    // Create an empty file with instructions
    if let Err(e) = std::fs::write(&access_token_path, "Enter your GitHub API token here") {
        eprintln!("Failed to create access token file: {e}");
        std::process::exit(1);
    }
    // Open the file in the default editor and wait for the user to close it
    let _ = edit::edit_file(&access_token_path);
    match std::fs::read_to_string(&access_token_path) {
        Ok(token) => token.trim().to_string(),
        Err(e) => {
            eprintln!("Failed to read access token: {e}");
            std::process::exit(1);
        }
    }
}

/// Verifies the provided GitHub access token by making an API call.
fn verify_access_token(token: &str) -> bool {
    use reqwest::header::{ACCEPT, AUTHORIZATION};
    let client = reqwest::Client::new();
    let response = client
        .get("https://api.github.com/user")
        .header(ACCEPT, "application/vnd.github+json")
        .header(AUTHORIZATION, format!("Bearer {}", token))
        .header("User-Agent", "git-system-tray")
        .send();
    match futures::executor::block_on(response) {
        Ok(response) => response.status().is_success(),
        Err(e) => {
            eprintln!("Failed to verify access token: {e}");
            false
        }
    }
}
