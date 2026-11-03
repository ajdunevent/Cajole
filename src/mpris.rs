use gio::prelude::*;

const MPRIS_PATH: &str = "/org/mpris/MediaPlayer2";
const MPRIS_PLAYER_IFACE: &str = "org.mpris.MediaPlayer2.Player";
const PROPERTIES_IFACE: &str = "org.freedesktop.DBus.Properties";

/// Pauses every playing MPRIS player and returns the bus names paused, for
/// [`resume_all`]. The list is scoped to one call, so overlapping alarms do
/// not share state.
pub async fn pause_playing(conn: &gio::DBusConnection) -> anyhow::Result<Vec<String>> {
    let mut paused = Vec::new();

    for name in mpris_player_names(conn).await? {
        match playback_status(conn, &name).await {
            Ok(status) if status == "Playing" => {
                if pause_one(conn, &name).await.is_ok() {
                    paused.push(name);
                }
            }
            _ => continue,
        }
    }

    Ok(paused)
}

/// Resumes the players returned by [`pause_playing`]. A player that has gone
/// away in between is skipped, so one failure does not block the alarm-stop
/// path.
pub async fn resume_all(conn: &gio::DBusConnection, players: &[String]) {
    for name in players {
        let _ = play_one(conn, name).await;
    }
}

async fn dbus_proxy(
    conn: &gio::DBusConnection,
    dest: &str,
    path: &str,
    iface: &str,
) -> anyhow::Result<gio::DBusProxy> {
    gio::DBusProxy::new_future(
        conn,
        gio::DBusProxyFlags::NONE,
        None,
        Some(dest),
        path,
        iface,
    )
    .await
    .map_err(|err| anyhow::anyhow!("failed to build D-Bus proxy for {dest}{path}: {err}"))
}

async fn mpris_player_names(conn: &gio::DBusConnection) -> anyhow::Result<Vec<String>> {
    let proxy = dbus_proxy(
        conn,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )
    .await?;
    let result = proxy
        .call_future("ListNames", None, gio::DBusCallFlags::NONE, -1)
        .await
        .map_err(|err| anyhow::anyhow!("ListNames failed: {err}"))?;
    let (names,): (Vec<String>,) = result
        .get()
        .ok_or_else(|| anyhow::anyhow!("unexpected ListNames reply shape"))?;
    Ok(names
        .into_iter()
        .filter(|n| n.starts_with("org.mpris.MediaPlayer2."))
        .collect())
}

async fn playback_status(conn: &gio::DBusConnection, dest: &str) -> anyhow::Result<String> {
    let proxy = dbus_proxy(conn, dest, MPRIS_PATH, PROPERTIES_IFACE).await?;
    let args = (MPRIS_PLAYER_IFACE, "PlaybackStatus").to_variant();
    let result = proxy
        .call_future("Get", Some(&args), gio::DBusCallFlags::NONE, -1)
        .await
        .map_err(|err| anyhow::anyhow!("Properties.Get failed for {dest}: {err}"))?;
    let (boxed,): (glib::Variant,) = result
        .get()
        .ok_or_else(|| anyhow::anyhow!("unexpected Properties.Get reply shape"))?;
    boxed
        .get::<String>()
        .ok_or_else(|| anyhow::anyhow!("PlaybackStatus was not a string"))
}

async fn pause_one(conn: &gio::DBusConnection, dest: &str) -> anyhow::Result<()> {
    call_player_method(conn, dest, "Pause").await
}

async fn play_one(conn: &gio::DBusConnection, dest: &str) -> anyhow::Result<()> {
    call_player_method(conn, dest, "Play").await
}

async fn call_player_method(
    conn: &gio::DBusConnection,
    dest: &str,
    method: &str,
) -> anyhow::Result<()> {
    let proxy = dbus_proxy(conn, dest, MPRIS_PATH, MPRIS_PLAYER_IFACE).await?;
    proxy
        .call_future(method, None, gio::DBusCallFlags::NONE, -1)
        .await
        .map_err(|err| anyhow::anyhow!("{method} failed for {dest}: {err}"))?;
    Ok(())
}
