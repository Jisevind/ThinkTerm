#![cfg(all(not(target_os = "macos"), not(windows)))]
//! See <https://developer.gnome.org/notification-spec/>

use crate::ToastNotification;
use futures_util::stream::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use zbus::proxy;
use zvariant::{Type, Value};

#[derive(Debug, Type, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct ServerInformation {
    /// The product name of the server.
    pub name: String,

    /// The vendor name. For example "KDE," "GNOME," "freedesktop.org" or "Microsoft".
    pub vendor: String,

    /// The server's version number.
    pub version: String,

    /// The specification version the server is compliant with.
    pub spec_version: String,
}

#[proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
trait Notifications {
    /// Get server information.
    ///
    /// This message returns the information on the server.
    fn get_server_information(&self) -> zbus::Result<ServerInformation>;

    /// GetCapabilities method
    fn get_capabilities(&self) -> zbus::Result<Vec<String>>;

    /// CloseNotification method
    fn close_notification(&self, nid: u32) -> zbus::Result<()>;

    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        hints: &HashMap<&str, Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;

    #[zbus(signal)]
    fn action_invoked(&self, nid: u32, action_key: String) -> zbus::Result<()>;

    #[zbus(signal)]
    fn notification_closed(&self, nid: u32, reason: u32) -> zbus::Result<()>;

    /// Sent just before `ActionInvoked`, by servers that implement version
    /// 1.2 of the spec: the token that lets the app take focus for it.
    #[zbus(signal)]
    fn activation_token(&self, nid: u32, activation_token: String) -> zbus::Result<()>;
}

/// Timeout/expiration was reached
const REASON_EXPIRED: u32 = 1;
/// User dismissed it
const REASON_USER_DISMISSED: u32 = 2;
/// CloseNotification was called with the nid
const REASON_CLOSE_NOTIFICATION: u32 = 3;

#[derive(Debug)]
enum Reason {
    Expired,
    Dismissed,
    Closed,
    #[allow(dead_code)]
    Unknown(u32),
}

impl Reason {
    fn new(n: u32) -> Self {
        match n {
            REASON_EXPIRED => Self::Expired,
            REASON_USER_DISMISSED => Self::Dismissed,
            REASON_CLOSE_NOTIFICATION => Self::Closed,
            _ => Self::Unknown(n),
        }
    }
}

async fn show_notif_impl(notif: ToastNotification) -> Result<(), Box<dyn std::error::Error>> {
    let connection = zbus::ConnectionBuilder::session()?.build().await?;

    let proxy = NotificationsProxy::new(&connection).await?;
    let caps = proxy.get_capabilities().await?;

    if notif.url.is_some() && !caps.iter().any(|cap| cap == "actions") {
        // Server doesn't support actions, so skip showing this notification
        // because it might have text that says "click to see more"
        // and that just wouldn't work.
        return Ok(());
    }

    // Every signal of the service, in the order the server sent them, and
    // from before the notification exists: the token comes just before the
    // click it belongs to, and a stream per signal lost that order.
    let mut signals = proxy.inner().receive_all_signals().await?;

    let mut hints = HashMap::new();
    hints.insert("urgency", Value::U8(2 /* Critical */));
    let mut actions = vec![];
    if notif.url.is_some() {
        actions.extend(["show", "Show"]);
    }
    if notif.target.is_some() {
        // The action of a click on the notification itself.
        actions.extend(["default", "Show"]);
    }
    let notification = proxy
        .notify(
            "ThinkTerm",
            0,
            "com.roversx.thinkterm",
            &notif.title,
            &notif.message,
            &actions,
            &hints,
            notif.timeout.map(|d| d.as_millis() as _).unwrap_or(0),
        )
        .await?;

    let mut activation_token = None;
    while let Some(signal) = signals.next().await {
        let header = signal.header();
        let Some(member) = header.member() else {
            continue;
        };
        match member.as_str() {
            "ActivationToken" => {
                let (nid, token): (u32, String) = signal.body().deserialize()?;
                if nid == notification {
                    activation_token = Some(token);
                }
            }
            "ActionInvoked" => {
                let (nid, action_key): (u32, String) = signal.body().deserialize()?;
                if nid != notification {
                    continue;
                }
                match (action_key.as_str(), &notif.url, &notif.target) {
                    ("default", _, Some(target)) => {
                        crate::clicked(target.clone(), activation_token.take())
                    }
                    (_, Some(url), _) => wezterm_open_url::open_url(url),
                    _ => continue,
                }
                break;
            }
            "NotificationClosed" => {
                let (nid, reason): (u32, u32) = signal.body().deserialize()?;
                if nid == notification {
                    let _reason = Reason::new(reason);
                    break;
                }
            }
            _ => {}
        }
    }

    Ok(())
}

pub fn show_notif(notif: ToastNotification) -> Result<(), Box<dyn std::error::Error>> {
    // Run this in a separate thread as we don't know if dbus or the notification
    // service on the other end are up, and we'd otherwise block for some time.
    std::thread::spawn(move || {
        let res = async_io::block_on(async move { show_notif_impl(notif).await });
        if let Err(err) = res {
            log::error!("while showing notification: {:#}", err);
        }
    });
    Ok(())
}
