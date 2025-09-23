use tray_icon::TrayIcon;
use winit::{
    application::ApplicationHandler, event::WindowEvent, event_loop::ActiveEventLoop,
    window::WindowId,
};

#[derive(Debug)]
pub enum UserEvent {
    TrayIcon(tray_icon::TrayIconEvent),
    UpdateIcon(Option<tray_icon::Icon>),
}

pub struct EventHandler {
    pub tray_icon: TrayIcon,
}

impl ApplicationHandler<UserEvent> for EventHandler {
    fn resumed(&mut self, _: &ActiveEventLoop) {}
    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
    fn user_event(&mut self, _: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::UpdateIcon(icon) => {
                if let Err(e) = self.tray_icon.set_icon(icon) {
                    eprintln!("Failed to set tray icon: {e}");
                }
            }
            UserEvent::TrayIcon(tray_icon::TrayIconEvent::Click {
                button,
                button_state,
                ..
            }) => {
                if button == tray_icon::MouseButton::Left
                    && button_state == tray_icon::MouseButtonState::Up
                {
                    if let Err(e) = open::that("https://github.com/notifications") {
                        eprintln!("Failed to open browser: {e}");
                    }
                }
            }
            _ => {}
        }
    }
}
