//! Notification scheduler for updating the tray icon based on unread GitHub notifications.
use crate::github::get_unread_notification_count;
use glib::source::timeout_add_seconds_local;
use libappindicator::AppIndicator;
use std::sync::{Arc, Mutex};

/// Starts the notification scheduler, updating the tray icon based on unread notifications.
pub fn start_notification_scheduler(
    indicator: AppIndicator,
    icon_path: String,
    icon_with_notification_path: String,
    access_token: String,
) {
    let indicator = Arc::new(Mutex::new(indicator));

    // Schedule periodic updates every 10 seconds
    timeout_add_seconds_local(10, {
        let indicator = Arc::clone(&indicator);
        let icon_path = icon_path.clone();
        let icon_with_notification_path = icon_with_notification_path.clone();
        let access_token = access_token.clone();
        let http_client = reqwest::Client::new();
        update_icon(
            &http_client,
            &indicator,
            &icon_path,
            &icon_with_notification_path,
            &access_token,
        );
        move || {
            update_icon(
                &http_client,
                &indicator,
                &icon_path,
                &icon_with_notification_path,
                &access_token,
            );
            glib::ControlFlow::Continue
        }
    });
}

fn update_icon(
    http_client: &reqwest::Client,
    indicator: &Arc<Mutex<AppIndicator>>,
    icon_path: &str,
    icon_with_notification_path: &str,
    access_token: &str,
) {
    let indicator = Arc::clone(indicator);
    let icon_path = icon_path.to_string();
    let icon_with_notification_path = icon_with_notification_path.to_string();
    let access_token = access_token.to_string();
    match get_unread_notification_count(http_client, &access_token) {
        Ok(unread_count) => {
            println!("Initial unread count: {}", unread_count);
            let icon = if unread_count > 0 {
                &icon_with_notification_path
            } else {
                &icon_path
            };
            if let Ok(mut indicator) = indicator.lock() {
                indicator.set_icon(icon);
            } else {
                eprintln!("Failed to acquire indicator lock (possible deadlock)");
            }
        }
        Err(e) => eprintln!("Failed to fetch notification count: {}", e),
    }
}
