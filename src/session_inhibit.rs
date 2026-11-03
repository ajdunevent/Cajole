//! Idle inhibitor on `org.gnome.SessionManager`, the call behind Phosh's
//! Caffeine tile. Phosh blanks the screen when gnome-session reports the
//! session idle, and an idle inhibitor prevents that. gnome-session also
//! drops the inhibitor if this process disconnects from the bus.

use gio::prelude::*;

const DEST: &str = "org.gnome.SessionManager";
const PATH: &str = "/org/gnome/SessionManager";
const IFACE: &str = "org.gnome.SessionManager";
const INHIBIT_IDLE: u32 = 1 << 3;

/// Releases the inhibitor when dropped.
pub struct IdleInhibit {
    conn: gio::DBusConnection,
    cookie: u32,
}

/// `None` when the session manager is unavailable.
pub async fn inhibit_idle(conn: &gio::DBusConnection, reason: &str) -> Option<IdleInhibit> {
    let params = (crate::APP_ID, 0u32, reason, INHIBIT_IDLE).to_variant();
    let reply = conn
        .call_future(
            Some(DEST),
            PATH,
            IFACE,
            "Inhibit",
            Some(&params),
            Some(glib::VariantTy::new("(u)").expect("valid type string")),
            gio::DBusCallFlags::NONE,
            -1,
        )
        .await;
    match reply {
        Ok(reply) => reply.get::<(u32,)>().map(|(cookie,)| IdleInhibit {
            conn: conn.clone(),
            cookie,
        }),
        Err(err) => {
            tracing::warn!(%err, "failed to inhibit session idle");
            None
        }
    }
}

impl Drop for IdleInhibit {
    fn drop(&mut self) {
        self.conn.call(
            Some(DEST),
            PATH,
            IFACE,
            "Uninhibit",
            Some(&(self.cookie,).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            -1,
            gio::Cancellable::NONE,
            |_| {},
        );
    }
}
