use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Notification {
    pub unread: bool,
}

/// Gets the unread GitHub notification count for the authenticated user.
/// Pass your classic personal access token as the `token` argument.
pub async fn get_unread_notification_count(token: &str) -> Result<usize, reqwest::Error> {
    use reqwest::header::{ACCEPT, AUTHORIZATION};
    let client = reqwest::Client::new();
    let res = client
        .get("https://api.github.com/notifications")
        .header(ACCEPT, "application/vnd.github+json")
        .header(AUTHORIZATION, format!("Bearer {}", token))
        .header("User-Agent", "git-system-tray")
        .send()
        .await?;
    if !res.status().is_success() {
        eprintln!(
            "GitHub API request failed with status: {}, response: {}",
            res.status(),
            res.text().await?
        );
        return Ok(0);
    }
    let notifications: Vec<Notification> = res.json().await?;
    Ok(notifications.iter().filter(|n| n.unread).count())
}
