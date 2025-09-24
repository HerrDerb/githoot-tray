use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Notification {
    pub unread: bool,
}
/// Gets the unread GitHub notification count for the authenticated user.
/// Pass your classic personal access token as the `token` argument.
/// Returns Ok(count) or Err(error).
pub fn get_unread_notification_count(http_client: &reqwest::Client, token: &str) -> Result<usize, reqwest::Error> {
    use reqwest::header::{ACCEPT, AUTHORIZATION};

    let res = http_client
        .get("https://api.github.com/notifications")
        .header(ACCEPT, "application/vnd.github+json")
        .header(AUTHORIZATION, format!("Bearer {}", token))
        .header("User-Agent", "git-system-tray")
        .send();
    let response = futures::executor::block_on(res).unwrap();

    if !response.status().is_success() {
        let status = response.status();
        let body = futures::executor::block_on(response.text()).unwrap_or_else(|_| "<failed to read body>".to_string());
        eprintln!("GitHub API request failed with status: {status}, response: {body}");
        return Ok(0);
    }
    let notifications: Vec<Notification> = futures::executor::block_on(response.json()).unwrap_or_else(|_| Vec::new());
    Ok(notifications.iter().filter(|n| n.unread).count())
}
