use crate::event_handler::UserEvent;
use crate::github::get_unread_notification_count;
use tokio::spawn;
use tokio::task::JoinHandle;
use tokio_schedule::{Job, every};
use tray_icon::Icon;
use winit::event_loop::EventLoopProxy;

pub async fn start_notification_scheduler(
    proxy: EventLoopProxy<UserEvent>,
    icon: Icon,
    icon_with_notification: Icon,
    access_token: String,
) -> JoinHandle<()> {
    let icon_clone = icon.clone();
    let icon_with_notification_clone = icon_with_notification.clone();
    let proxy_clone = proxy.clone();
    let access_token_clone = access_token.clone();
    let update_unread_count = every(1).minute().perform(move || {
        let icon_clone = icon_clone.clone();
        let icon_with_notification_clone = icon_with_notification_clone.clone();
        let proxy_clone = proxy_clone.clone();
        let access_token_clone = access_token_clone.clone();
        async move {
            get_notification_count_and_update_icon(
                access_token_clone,
                icon_clone,
                icon_with_notification_clone,
                proxy_clone,
            )
            .await;
        }
    });
    get_notification_count_and_update_icon(access_token, icon, icon_with_notification, proxy).await;
    return spawn(update_unread_count);
}

async fn get_notification_count_and_update_icon(
    access_token: String,
    icon_clone: Icon,
    icon_with_notification_clone: Icon,
    proxy_clone: EventLoopProxy<UserEvent>,
) {
    println!("Fetching notification count");
    match get_unread_notification_count(&access_token).await {
        Ok(unread_count) => {
            println!("Unread notification count: {}", unread_count);
            let icon = if unread_count > 0 {
                Some(icon_with_notification_clone.clone())
            } else {
                Some(icon_clone.clone())
            };
            let _ = proxy_clone.send_event(UserEvent::UpdateIcon(icon));
        }
        Err(e) => {
            eprintln!("Failed to fetch notification count: {e}");
        }
    }
}
