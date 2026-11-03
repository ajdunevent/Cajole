//! Temporary switch of the feedbackd profile from `silent` to `quiet`.
//!
//! `Haptic.Vibrate` returns false while the profile is `silent`, and
//! feedbackd's `allow-important` list does not apply to it. The profile is
//! a global setting, so it is restored as soon as the last [`Guard`] drops.
//! Calls are synchronous so the restore also runs from `Drop` on app quit.

use std::cell::RefCell;

use gio::prelude::*;

const DEST: &str = "org.sigxcpu.Feedback";
const PATH: &str = "/org/sigxcpu/Feedback";
const IFACE: &str = "org.sigxcpu.Feedback";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
const TIMEOUT_MS: i32 = 500;

const SILENT: &str = "silent";
const QUIET: &str = "quiet";

#[derive(Default)]
struct State {
    holders: u32,
    /// Profile to put back; set only while this module has changed it.
    original: Option<String>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

/// The current feedbackd profile, or `None` when feedbackd is unreachable.
pub fn current_profile(conn: &gio::DBusConnection) -> Option<String> {
    let params = glib::Variant::tuple_from_iter([IFACE.to_variant(), "Profile".to_variant()]);
    let reply = conn
        .call_sync(
            Some(DEST),
            PATH,
            PROPERTIES,
            "Get",
            Some(&params),
            Some(glib::VariantTy::new("(v)").expect("valid type string")),
            gio::DBusCallFlags::NONE,
            TIMEOUT_MS,
            gio::Cancellable::NONE,
        )
        .ok()?;
    reply
        .child_value(0)
        .as_variant()?
        .str()
        .map(str::to_owned)
}

fn set_profile(conn: &gio::DBusConnection, profile: &str) -> Result<(), glib::Error> {
    let params = glib::Variant::tuple_from_iter([
        IFACE.to_variant(),
        "Profile".to_variant(),
        glib::Variant::from_variant(&profile.to_variant()),
    ]);
    conn.call_sync(
        Some(DEST),
        PATH,
        PROPERTIES,
        "Set",
        Some(&params),
        None,
        gio::DBusCallFlags::NONE,
        TIMEOUT_MS,
        gio::Cancellable::NONE,
    )
    .map(|_| ())
}

/// Holds the `quiet` profile while alive. Restores the original profile on
/// drop, once the last guard is gone, if the profile is still `quiet`.
pub struct Guard {
    conn: gio::DBusConnection,
}

/// Switches `silent` to `quiet` for as long as the returned guard lives.
/// Returns `None` when `allowed` is false; otherwise a guard that does
/// nothing when feedbackd is absent or the profile is not `silent`.
pub fn engage(conn: &gio::DBusConnection, allowed: bool) -> Option<Guard> {
    if !allowed {
        return None;
    }
    let first = STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.holders += 1;
        state.holders == 1
    });
    if first && current_profile(conn).as_deref() == Some(SILENT) {
        match set_profile(conn, QUIET) {
            Ok(()) => STATE.with(|state| state.borrow_mut().original = Some(SILENT.to_owned())),
            Err(err) => tracing::warn!(%err, "failed to switch feedbackd profile to quiet"),
        }
    }
    Some(Guard { conn: conn.clone() })
}

impl Drop for Guard {
    fn drop(&mut self) {
        let original = STATE.with(|state| {
            let mut state = state.borrow_mut();
            state.holders = state.holders.saturating_sub(1);
            if state.holders == 0 {
                state.original.take()
            } else {
                None
            }
        });
        let Some(original) = original else { return };
        if current_profile(&self.conn).as_deref() != Some(QUIET) {
            return;
        }
        if let Err(err) = set_profile(&self.conn, &original) {
            tracing::warn!(%err, %original, "failed to restore feedbackd profile");
        }
    }
}
