//! Integration with `waked` (<https://gitlab.com/seath1/waked>), a system
//! daemon that wakes the device from suspend at a requested time for
//! unprivileged alarm apps. `systemd --user` timers cannot set `WakeSystem=`
//! (systemd issue #17564). GNOME Clocks uses the same daemon.
//!
//! Runs on the system bus: `waked.service` needs `CLOCK_REALTIME_ALARM`, which
//! a `--user` process cannot hold.
//!
//! A missing service is logged at debug level. The timer in `scheduler.rs`
//! (`reschedule_wake_timer`) covers everything except waking hardware from
//! deep suspend.

use gio::prelude::*;

const WAKED_BUS_NAME: &str = "de.seath.Waked";
const WAKED_OBJECT_PATH: &str = "/de/seath/Waked/Alarm";
const WAKED_INTERFACE: &str = "de.seath.Waked";

/// One entry is registered, since `compute_next_wakes` collapses everything
/// into a single next moment. GNOME Clocks registers one per alarm.
const WAKED_ALARM_ID: &str = "cajole";

async fn connect() -> Option<gio::DBusConnection> {
    match gio::bus_get_future(gio::BusType::System).await {
        Ok(conn) => Some(conn),
        Err(err) => {
            tracing::debug!(%err, "failed to connect to system bus for waked integration");
            None
        }
    }
}

async fn proxy(conn: &gio::DBusConnection) -> Option<gio::DBusProxy> {
    match gio::DBusProxy::new_future(
        conn,
        gio::DBusProxyFlags::NONE,
        None,
        Some(WAKED_BUS_NAME),
        WAKED_OBJECT_PATH,
        WAKED_INTERFACE,
    )
    .await
    {
        Ok(proxy) => Some(proxy),
        Err(err) => {
            tracing::debug!(%err, "waked not available; relying on the systemd --user timer alone");
            None
        }
    }
}

/// Tells waked the next wake time so it can wake the device from suspend.
/// `unix_time` is a Unix timestamp in seconds, as waked's `update` method
/// expects.
pub async fn update_wake_time(unix_time: i64) {
    let Some(conn) = connect().await else { return };
    let Some(proxy) = proxy(&conn).await else { return };

    let args = (WAKED_ALARM_ID, unix_time as u64).to_variant();
    if let Err(err) = proxy
        .call_future("update", Some(&args), gio::DBusCallFlags::NONE, -1)
        .await
    {
        tracing::debug!(%err, "waked update call failed");
    }
}

/// Tells waked nothing is left to wake for.
pub async fn remove_wake_time() {
    let Some(conn) = connect().await else { return };
    let Some(proxy) = proxy(&conn).await else { return };

    let args = (WAKED_ALARM_ID,).to_variant();
    if let Err(err) = proxy
        .call_future("remove", Some(&args), gio::DBusCallFlags::NONE, -1)
        .await
    {
        tracing::debug!(%err, "waked remove call failed");
    }
}
