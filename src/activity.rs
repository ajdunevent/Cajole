//! One-shot user activity watch on Phosh's `org.gnome.Mutter.IdleMonitor`.
//!
//! The watch fires on the next input event (a touch, a key). Setting
//! the screen power state or backlight does not count as input.

use std::cell::Cell;
use std::rc::Rc;

use gio::prelude::*;

const DEST: &str = "org.gnome.Mutter.IdleMonitor";
const PATH: &str = "/org/gnome/Mutter/IdleMonitor/Core";
const IFACE: &str = "org.gnome.Mutter.IdleMonitor";

/// Cancels the watch when dropped.
pub struct UserActivityWatch {
    conn: gio::DBusConnection,
    watch_id: Rc<Cell<u32>>,
    subscription: Option<gio::SignalSubscriptionId>,
}

/// Calls `on_active` once, on the next user input. `None` when the idle
/// monitor is unavailable.
pub async fn watch_once(
    conn: &gio::DBusConnection,
    on_active: impl Fn() + 'static,
) -> Option<UserActivityWatch> {
    let watch_id = Rc::new(Cell::new(0u32));
    let subscription = conn.signal_subscribe(
        Some(DEST),
        Some(IFACE),
        Some("WatchFired"),
        Some(PATH),
        None,
        gio::DBusSignalFlags::NONE,
        {
            let watch_id = watch_id.clone();
            move |_, _, _, _, _, params| {
                let Some((fired,)) = params.get::<(u32,)>() else {
                    return;
                };
                if fired != 0 && fired == watch_id.get() {
                    on_active();
                }
            }
        },
    );
    let reply = conn
        .call_future(
            Some(DEST),
            PATH,
            IFACE,
            "AddUserActiveWatch",
            None,
            Some(glib::VariantTy::new("(u)").expect("valid type string")),
            gio::DBusCallFlags::NONE,
            -1,
        )
        .await;
    match reply.ok().and_then(|reply| reply.get::<(u32,)>()) {
        Some((id,)) => {
            watch_id.set(id);
            Some(UserActivityWatch {
                conn: conn.clone(),
                watch_id,
                subscription: Some(subscription),
            })
        }
        None => {
            conn.signal_unsubscribe(subscription);
            None
        }
    }
}

impl Drop for UserActivityWatch {
    fn drop(&mut self) {
        if let Some(subscription) = self.subscription.take() {
            self.conn.signal_unsubscribe(subscription);
        }
        self.conn.call(
            Some(DEST),
            PATH,
            IFACE,
            "RemoveWatch",
            Some(&(self.watch_id.get(),).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            -1,
            gio::Cancellable::NONE,
            |_| {},
        );
    }
}
