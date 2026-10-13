mod dbus;
mod macos;
mod windows;

use std::sync::OnceLock;

#[derive(Debug, Clone)]
pub struct ToastNotification {
    pub title: String,
    pub message: String,
    pub url: Option<String>,
    /// What a click on the notification brings up: opaque here, and handed
    /// to the handler given to [`set_click_handler`].
    pub target: Option<String>,
    pub timeout: Option<std::time::Duration>,
}

impl ToastNotification {
    pub fn show(self) {
        show(self)
    }
}

/// A click on a notification that had a target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotificationClick {
    pub target: String,
    /// The token a Linux notification server hands over with the click,
    /// without which Wayland does not let a window take focus.
    pub activation_token: Option<String>,
}

static CLICK_HANDLER: OnceLock<Box<dyn Fn(NotificationClick) + Send + Sync>> = OnceLock::new();

/// Set what a click on a notification with a target does. It runs on
/// whichever thread the system reports the click on; only the first
/// handler set is kept.
pub fn set_click_handler(handler: impl Fn(NotificationClick) + Send + Sync + 'static) {
    if CLICK_HANDLER.set(Box::new(handler)).is_err() {
        log::warn!("a notification click handler was already set");
    }
}

#[allow(dead_code)]
fn clicked(target: String, activation_token: Option<String>) {
    match CLICK_HANDLER.get() {
        Some(handler) => handler(NotificationClick {
            target,
            activation_token,
        }),
        None => log::debug!("notification for {target} clicked, with no handler"),
    }
}

#[cfg(windows)]
use crate::windows as backend;
#[cfg(all(not(target_os = "macos"), not(windows)))]
use dbus as backend;
#[cfg(target_os = "macos")]
use macos as backend;

mod nop {
    use super::*;

    #[allow(dead_code)]
    pub fn show_notif(_: ToastNotification) -> Result<(), Box<dyn std::error::Error>> {
        Ok(())
    }
}

pub fn show(notif: ToastNotification) {
    if let Err(err) = backend::show_notif(notif) {
        log::error!("Failed to show notification: {}", err);
    }
}

pub fn persistent_toast_notification_with_click_to_open_url(title: &str, message: &str, url: &str) {
    show(ToastNotification {
        title: title.to_string(),
        message: message.to_string(),
        url: Some(url.to_string()),
        target: None,
        timeout: None,
    });
}

/// A notification that, clicked, hands `target` to the click handler.
pub fn persistent_toast_notification_with_target(title: &str, message: &str, target: &str) {
    show(ToastNotification {
        title: title.to_string(),
        message: message.to_string(),
        url: None,
        target: Some(target.to_string()),
        timeout: None,
    });
}

pub fn persistent_toast_notification(title: &str, message: &str) {
    show(ToastNotification {
        title: title.to_string(),
        message: message.to_string(),
        url: None,
        target: None,
        timeout: None,
    });
}

#[cfg(target_os = "macos")]
pub use macos::initialize as macos_initialize;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn a_click_reaches_the_handler_with_its_token() {
        static CLICKS: Mutex<Vec<NotificationClick>> = Mutex::new(vec![]);
        set_click_handler(|click| CLICKS.lock().unwrap().push(click));
        clicked("pane:7".to_string(), Some("token".to_string()));
        clicked("thread:t1".to_string(), None);
        assert_eq!(
            *CLICKS.lock().unwrap(),
            vec![
                NotificationClick {
                    target: "pane:7".to_string(),
                    activation_token: Some("token".to_string()),
                },
                NotificationClick {
                    target: "thread:t1".to_string(),
                    activation_token: None,
                },
            ]
        );
    }
}
