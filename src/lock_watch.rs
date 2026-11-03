//! Watches logind's `LockedHint` for the session. Phosh clears it on every
//! unlock, including a face unlock, which produces no input event.

use std::collections::HashMap;


const LOGIND_DEST: &str = "org.freedesktop.login1";
const SESSION_IFACE: &str = "org.freedesktop.login1.Session";

/// Unsubscribes when dropped.
pub struct UnlockWatch {
    conn: gio::DBusConnection,
    subscription: Option<gio::SignalSubscriptionId>,
}

/// Calls `on_unlocked` each time the session goes from locked to unlocked.
/// `None` when the session cannot be found.
pub async fn watch_unlock(on_unlocked: impl Fn() + 'static) -> Option<UnlockWatch> {
    let conn = gio::bus_get_future(gio::BusType::System).await.ok()?;
    let path = crate::display::find_seated_session_path(&conn).await?;
    let subscription = conn.signal_subscribe(
        Some(LOGIND_DEST),
        Some("org.freedesktop.DBus.Properties"),
        Some("PropertiesChanged"),
        Some(&path),
        Some(SESSION_IFACE),
        gio::DBusSignalFlags::NONE,
        move |_, _, _, _, _, params| {
            let Some((_, changed, _)) =
                params.get::<(String, HashMap<String, glib::Variant>, Vec<String>)>()
            else {
                return;
            };
            if changed.get("LockedHint").and_then(|v| v.get::<bool>()) == Some(false) {
                on_unlocked();
            }
        },
    );
    Some(UnlockWatch {
        conn,
        subscription: Some(subscription),
    })
}

impl Drop for UnlockWatch {
    fn drop(&mut self) {
        if let Some(subscription) = self.subscription.take() {
            self.conn.signal_unsubscribe(subscription);
        }
    }
}
