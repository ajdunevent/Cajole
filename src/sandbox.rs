//! Flatpak detection and the Background portal.
//!
//! Inside the sandbox `~/.config/autostart` is private and `systemd --user`
//! is unreachable, so autostart goes through
//! `org.freedesktop.portal.Background` instead. Needs no manifest
//! permission.

use std::collections::HashMap;
use std::path::Path;

use gio::prelude::*;

const PORTAL_BUS_NAME: &str = "org.freedesktop.portal.Desktop";
const PORTAL_OBJECT_PATH: &str = "/org/freedesktop/portal/desktop";
const BACKGROUND_INTERFACE: &str = "org.freedesktop.portal.Background";
const REQUEST_INTERFACE: &str = "org.freedesktop.portal.Request";

/// Whether this process runs inside a Flatpak sandbox.
pub fn is_flatpak() -> bool {
    Path::new("/.flatpak-info").exists()
}

/// Reads a host setting through `org.freedesktop.portal.Settings`. GSettings
/// inside the sandbox returns the runtime's schema default instead of the
/// host value. `None` when the portal or the key is unavailable.
pub fn read_portal_setting(namespace: &str, key: &str) -> Option<String> {
    let conn = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).ok()?;
    let params = glib::Variant::tuple_from_iter([namespace.to_variant(), key.to_variant()]);
    let reply = conn
        .call_sync(
            Some(PORTAL_BUS_NAME),
            PORTAL_OBJECT_PATH,
            "org.freedesktop.portal.Settings",
            "Read",
            Some(&params),
            Some(glib::VariantTy::new("(v)").expect("valid type string")),
            gio::DBusCallFlags::NONE,
            1000,
            gio::Cancellable::NONE,
        )
        .ok()?;
    // Read wraps the value in a second variant.
    reply
        .child_value(0)
        .as_variant()?
        .as_variant()?
        .str()
        .map(str::to_owned)
}

/// Request path the portal answers on for a given caller and token.
fn request_path(unique_name: &str, token: &str) -> String {
    let sender = unique_name.trim_start_matches(':').replace('.', "_");
    format!("{PORTAL_OBJECT_PATH}/request/{sender}/{token}")
}

/// Asks the portal to allow background running and, with `autostart`, to
/// start `cajole --hidden` at login. Returns whether autostart is now on.
/// The portal shows a permission dialog the first time.
pub async fn request_background(autostart: bool) -> anyhow::Result<bool> {
    let conn = gio::bus_get_future(gio::BusType::Session).await?;
    let unique_name = conn
        .unique_name()
        .ok_or_else(|| anyhow::anyhow!("session bus connection has no unique name"))?;
    let token = format!("cajole{}", uuid::Uuid::new_v4().simple());
    let expected_path = request_path(&unique_name, &token);

    // Subscribed before the call: the Response can arrive before the call returns.
    let (sender, receiver) = async_channel::bounded::<(u32, HashMap<String, glib::Variant>)>(1);
    let subscription = conn.signal_subscribe(
        Some(PORTAL_BUS_NAME),
        Some(REQUEST_INTERFACE),
        Some("Response"),
        Some(&expected_path),
        None,
        gio::DBusSignalFlags::NONE,
        move |_, _, _, _, _, params| {
            if let Some(response) = params.get::<(u32, HashMap<String, glib::Variant>)>() {
                let _ = sender.try_send(response);
            }
        },
    );

    let options = glib::VariantDict::new(None);
    options.insert("handle_token", token.as_str());
    options.insert(
        "reason",
        "Cajole needs to run in the background so alarms ring on time.",
    );
    options.insert("autostart", autostart);
    options.insert("commandline", vec!["cajole".to_string(), "--hidden".to_string()]);

    let call = conn
        .call_future(
            Some(PORTAL_BUS_NAME),
            PORTAL_OBJECT_PATH,
            BACKGROUND_INTERFACE,
            "RequestBackground",
            Some(&glib::Variant::tuple_from_iter([
                "".to_variant(),
                options.end(),
            ])),
            Some(glib::VariantTy::new("(o)").expect("valid type string")),
            gio::DBusCallFlags::NONE,
            -1,
        )
        .await;
    let outcome = match call {
        Ok(reply) => {
            let handle = reply
                .child_value(0)
                .str()
                .map(str::to_owned)
                .unwrap_or_default();
            if handle != expected_path {
                tracing::warn!(%handle, %expected_path, "portal answered on an unexpected request path");
            }
            receiver.recv().await.map_err(anyhow::Error::from)
        }
        Err(err) => Err(err.into()),
    };
    conn.signal_unsubscribe(subscription);

    let (code, results) = outcome?;
    match code {
        0 => Ok(results
            .get("autostart")
            .and_then(|v| v.get::<bool>())
            .unwrap_or(false)),
        1 => anyhow::bail!("background request was declined"),
        _ => anyhow::bail!("background request failed (response {code})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_path_follows_the_portal_naming_rule() {
        assert_eq!(
            request_path(":1.234", "cajole42"),
            "/org/freedesktop/portal/desktop/request/1_234/cajole42"
        );
    }
}
