use reqwest::blocking::Client;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Notification {
    pub unread: bool,
}

/// Gets the unread GitHub notification count for the authenticated user.
/// Pass your classic personal access token as the `token` argument.
pub fn get_unread_notification_count(http_client: &Client, token: &str) -> Result<usize, reqwest::Error> {
    use reqwest::header::{ACCEPT, AUTHORIZATION};

    let response = http_client
        .get("https://api.github.com/notifications")
        .header(ACCEPT, "application/vnd.github+json")
        .header(AUTHORIZATION, format!("Bearer {}", token))
        .header("User-Agent", "git-system-tray")
        .send()?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_else(|_| "<failed to read body>".to_string());
        eprintln!("GitHub API request failed with status: {status}, response: {body}");
        return Ok(0);
    }

    let notifications: Vec<Notification> = response.json().unwrap_or_default();
    Ok(notifications.iter().filter(|n| n.unread).count())
}
