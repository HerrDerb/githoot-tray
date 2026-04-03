//! Notification scheduler for updating the tray icon based on unread GitHub notifications.
//!
//! Linux: polling runs on a background thread; results are forwarded to the GTK main loop via a
//! `glib::MainContext` channel so the UI is never blocked.
//!
//! Windows: polling runs on a background thread; results are forwarded to the winit event loop
//! via `EventLoopProxy::send_event` so the icon is updated on the main thread.

use crate::github::get_unread_notification_count;
use std::time::Duration;

const POLL_INTERVAL: Duration = Duration::from_secs(30);

// ─── Linux ────────────────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
pub fn start_notification_scheduler(
    indicator: libappindicator::AppIndicator,
    icon_path: String,
    icon_with_notification_path: String,
    access_token: String,
) {
    use glib::MainContext;
    use std::sync::{Arc, Mutex};

    let (sender, receiver) = MainContext::channel(glib::Priority::Default);

    // Background thread: do all blocking network I/O here, never on the GTK main loop.
    std::thread::spawn(move || {
        let http_client = reqwest::blocking::Client::new();
        loop {
            let has_notifications = get_unread_notification_count(&http_client, &access_token)
                .map(|c| c > 0)
                .unwrap_or(false);
            if sender.send(has_notifications).is_err() {
                break; // receiver dropped – main loop exited
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    });

    // GTK main-loop thread: update the indicator icon whenever a result arrives.
    let indicator = Arc::new(Mutex::new(indicator));
    receiver.attach(None, move |has_notifications| {
        let icon = if has_notifications {
            icon_with_notification_path.as_str()
        } else {
            icon_path.as_str()
        };
        if let Ok(mut ind) = indicator.lock() {
            ind.set_icon(icon);
        }
        glib::ControlFlow::Continue
    });
}

// ─── Windows ──────────────────────────────────────────────────────────────────

/// The custom event type sent from the polling thread to the winit event loop.
#[cfg(target_os = "windows")]
pub enum TrayEvent {
    NotificationUpdate(bool),
}

#[cfg(target_os = "windows")]
pub fn start_notification_scheduler(
    access_token: String,
    proxy: winit::event_loop::EventLoopProxy<TrayEvent>,
) {
    // Background thread: all blocking network I/O lives here.
    std::thread::spawn(move || {
        let http_client = reqwest::blocking::Client::new();
        loop {
            let has_notifications = get_unread_notification_count(&http_client, &access_token)
                .map(|c| c > 0)
                .unwrap_or(false);
            if proxy
                .send_event(TrayEvent::NotificationUpdate(has_notifications))
                .is_err()
            {
                break; // event loop closed
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    });
}
