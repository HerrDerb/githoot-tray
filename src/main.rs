mod event_handler;
mod github;
mod icons;
mod scheduler;
use event_handler::{EventHandler, UserEvent};
use icons::load_icons;
use scheduler::start_notification_scheduler;
use tray_icon::TrayIconBuilder;
use winit::event_loop::EventLoop;

#[tokio::main]
async fn main() {
    let access_token = "".to_string();
    println!("Using access token: {}", access_token);
    let (icon, icon_with_notification) = load_icons();
    let tray_icon = TrayIconBuilder::new()
        .with_tooltip("GitHub Notifications")
        .with_icon(icon.clone())
        .build()
        .expect("Failed to create tray icon");
    let event_loop = EventLoop::<UserEvent>::with_user_event().build().unwrap();
    let proxy = event_loop.create_proxy();
    let proxy_clone = proxy.clone();
    tray_icon::TrayIconEvent::set_event_handler(Some(move |event| {
        if let Err(e) = proxy.send_event(UserEvent::TrayIcon(event)) {
            eprintln!("Failed to send tray icon event: {e}");
        }
    }));

    start_notification_scheduler(proxy_clone, icon, icon_with_notification, access_token).await;

    if let Err(e) = event_loop.run_app(&mut EventHandler { tray_icon }) {
        eprintln!("Failed to run event loop: {e}");
    }
}
