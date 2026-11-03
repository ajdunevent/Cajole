use gio::prelude::*;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

const MUTTER_DEST: &str = "org.gnome.Mutter.DisplayConfig";
const MUTTER_PATH: &str = "/org/gnome/Mutter/DisplayConfig";
const MUTTER_IFACE: &str = "org.gnome.Mutter.DisplayConfig";
const PROPERTIES_IFACE: &str = "org.freedesktop.DBus.Properties";
const SETTINGS_DAEMON_POWER_DEST: &str = "org.gnome.SettingsDaemon.Power";
const SETTINGS_DAEMON_POWER_PATH: &str = "/org/gnome/SettingsDaemon/Power";
const SETTINGS_DAEMON_POWER_SCREEN_IFACE: &str = "org.gnome.SettingsDaemon.Power.Screen";
const SHELL_BRIGHTNESS_DEST: &str = "org.gnome.Shell.Brightness";
const SHELL_BRIGHTNESS_PATH: &str = "/org/gnome/Shell/Brightness";
const SHELL_BRIGHTNESS_IFACE: &str = "org.gnome.Shell.Brightness";
const LOGIND_DEST: &str = "org.freedesktop.login1";
const LOGIND_MANAGER_PATH: &str = "/org/freedesktop/login1";
const LOGIND_MANAGER_IFACE: &str = "org.freedesktop.login1.Manager";
const LOGIND_SESSION_IFACE: &str = "org.freedesktop.login1.Session";

/// Reads Mutter's `PowerSaveMode`, so a caller can save it before waking the
/// screen and restore it afterward.
pub async fn get_power_save_mode(conn: &gio::DBusConnection) -> Option<i32> {
    let proxy = gio::DBusProxy::new_future(
        conn,
        gio::DBusProxyFlags::NONE,
        None,
        Some(MUTTER_DEST),
        MUTTER_PATH,
        PROPERTIES_IFACE,
    )
    .await
    .ok()?;
    let args = (MUTTER_IFACE, "PowerSaveMode").to_variant();
    let result = proxy
        .call_future("Get", Some(&args), gio::DBusCallFlags::NONE, -1)
        .await
        .ok()?;
    let (boxed,): (glib::Variant,) = result.get()?;
    boxed.get::<i32>()
}

/// `Properties.Set` arguments, type `(ssv)`. A `Variant` element of a Rust
/// tuple is boxed by the tuple conversion, so the plain int32 is passed.
/// Boxing it first sends `v` inside `v`, which Mutter and Phosh reject with
/// "Expected type i but got v".
fn power_save_args(mode: i32) -> glib::Variant {
    (MUTTER_IFACE, "PowerSaveMode", mode.to_variant()).to_variant()
}

/// Sets Mutter's `PowerSaveMode`: `0` wakes the display, other values select
/// Mutter's power-save states. Equivalent to:
/// ```text
/// busctl --user set-property org.gnome.Mutter.DisplayConfig \
///   /org/gnome/Mutter/DisplayConfig org.gnome.Mutter.DisplayConfig \
///   PowerSaveMode i 0
/// ```
pub async fn set_power_save_mode(conn: &gio::DBusConnection, mode: i32) -> anyhow::Result<()> {
    let proxy = gio::DBusProxy::new_future(
        conn,
        gio::DBusProxyFlags::NONE,
        None,
        Some(MUTTER_DEST),
        MUTTER_PATH,
        PROPERTIES_IFACE,
    )
    .await
    .map_err(|err| anyhow::anyhow!("failed to build Mutter DisplayConfig proxy: {err}"))?;

    let args = power_save_args(mode);

    proxy
        .call_future("Set", Some(&args), gio::DBusCallFlags::NONE, -1)
        .await
        .map_err(|err| anyhow::anyhow!("failed to set PowerSaveMode: {err}"))?;
    Ok(())
}

/// Ends Phosh's idle dimming, which restores the backlight level Phosh saved.
/// Does nothing when the screen is not dimmed.
pub async fn undim(conn: &gio::DBusConnection) -> anyhow::Result<()> {
    let proxy = gio::DBusProxy::new_future(
        conn,
        gio::DBusProxyFlags::NONE,
        None,
        Some(SHELL_BRIGHTNESS_DEST),
        SHELL_BRIGHTNESS_PATH,
        SHELL_BRIGHTNESS_IFACE,
    )
    .await
    .map_err(|err| anyhow::anyhow!("failed to build Shell Brightness proxy: {err}"))?;
    proxy
        .call_future(
            "SetDimming",
            Some(&(false,).to_variant()),
            gio::DBusCallFlags::NONE,
            -1,
        )
        .await
        .map_err(|err| anyhow::anyhow!("SetDimming failed: {err}"))?;
    Ok(())
}

/// Wakes the display: `set_power_save_mode(conn, 0)` for callers with no
/// previous value to restore.
pub async fn wake_screen(conn: &gio::DBusConnection) -> anyhow::Result<()> {
    set_power_save_mode(conn, 0).await
}

// Screen brightness
//
// Three write tiers, each falling through to the next on failure:
//
//   1. `systemd-logind`'s `Session.SetBrightness` on the system bus. It grants
//      the session write access to the backlight's sysfs files, so it is tried
//      before a raw sysfs write.
//   2. A direct write to the detected device's `brightness` file, which needs
//      write permission granted another way (root, or a udev rule for a group
//      the process is in).
//   3. `org.gnome.SettingsDaemon.Power.Screen` on the session bus, for GNOME
//      desktop sessions. It does not work on the FLX1S, where Phosh does not
//      run that daemon's brightness plugin.
//
// Reading goes through sysfs first, which needs no special permission.

/// A detected brightness-controllable device: its sysfs class (`backlight` or
/// `leds`), its name within that class, and its directory.
struct BacklightDevice {
    class: &'static str,
    name: String,
    dir: PathBuf,
}

/// Checks `/sys/class/backlight` first, then `/sys/class/leds/lcd-backlight`
/// (the FLX1S exposes its backlight as an LED class device). The first entry
/// found wins; phones have one display backlight.
fn detect_device() -> Option<BacklightDevice> {
    if let Some(entry) = fs::read_dir("/sys/class/backlight")
        .ok()
        .and_then(|mut d| d.find_map(|e| e.ok()))
    {
        return Some(BacklightDevice {
            class: "backlight",
            name: entry.file_name().to_string_lossy().into_owned(),
            dir: entry.path(),
        });
    }

    let leds_dir = PathBuf::from("/sys/class/leds/lcd-backlight");
    if leds_dir.join("brightness").exists() {
        return Some(BacklightDevice {
            class: "leds",
            name: "lcd-backlight".to_string(),
            dir: leds_dir,
        });
    }

    None
}

fn read_u32(path: &std::path::Path) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Cached result of [`find_seated_session_path`]. Without it, every brightness
/// ramp step made an extra `ListSessions` round-trip on top of
/// `SetBrightness`. Only a successful lookup is cached, so a transient failure
/// can be retried.
static SEATED_SESSION_PATH: Mutex<Option<String>> = Mutex::new(None);

/// Finds the object path of the seat-assigned session through logind's
/// `ListSessions`, taking the entry with a non-empty seat (`seat0`). The
/// `/org/freedesktop/login1/session/self` path fails inside a Flatpak sandbox,
/// because logind matches the caller's PID against its session table and the
/// sandbox's PID namespace breaks that match.
pub(crate) async fn find_seated_session_path(conn: &gio::DBusConnection) -> Option<String> {
    if let Some(cached) = SEATED_SESSION_PATH.lock().unwrap().as_ref() {
        return Some(cached.clone());
    }

    let proxy = gio::DBusProxy::new_future(
        conn,
        gio::DBusProxyFlags::NONE,
        None,
        Some(LOGIND_DEST),
        LOGIND_MANAGER_PATH,
        LOGIND_MANAGER_IFACE,
    )
    .await
    .ok()?;
    let result = proxy
        .call_future("ListSessions", None, gio::DBusCallFlags::NONE, -1)
        .await
        .ok()?;
    let (sessions,): (Vec<(String, u32, String, String, String)>,) = result.get()?;
    let path = sessions
        .into_iter()
        .find(|(_, _, _, seat, _)| !seat.is_empty())
        .map(|(_, _, _, _, path)| path)?;

    *SEATED_SESSION_PATH.lock().unwrap() = Some(path.clone());
    Some(path)
}

/// Tier 1: logind brightness control on the system bus, over a connection of
/// its own, so callers keep using the session bus.
async fn logind_set_brightness(class: &str, name: &str, raw: u32) -> Option<()> {
    let conn = gio::bus_get_future(gio::BusType::System).await.ok()?;
    let session_path = find_seated_session_path(&conn).await?;
    let proxy = gio::DBusProxy::new_future(
        &conn,
        gio::DBusProxyFlags::NONE,
        None,
        Some(LOGIND_DEST),
        session_path.as_str(),
        LOGIND_SESSION_IFACE,
    )
    .await
    .ok()?;
    let args = (class, name, raw).to_variant();
    proxy
        .call_future("SetBrightness", Some(&args), gio::DBusCallFlags::NONE, -1)
        .await
        .ok()?;
    Some(())
}

/// Tier 2: a direct write to the device's sysfs `brightness` file; needs write
/// permission granted another way.
fn sysfs_set_brightness(dev: &BacklightDevice, raw: u32) -> Option<()> {
    fs::write(dev.dir.join("brightness"), raw.to_string()).ok()
}

/// Tier 3 (last resort): `org.gnome.SettingsDaemon.Power.Screen` on the
/// session bus, for GNOME desktop sessions.
async fn settings_daemon_get_brightness(conn: &gio::DBusConnection) -> Option<i32> {
    let proxy = gio::DBusProxy::new_future(
        conn,
        gio::DBusProxyFlags::NONE,
        None,
        Some(SETTINGS_DAEMON_POWER_DEST),
        SETTINGS_DAEMON_POWER_PATH,
        PROPERTIES_IFACE,
    )
    .await
    .ok()?;
    let args = (SETTINGS_DAEMON_POWER_SCREEN_IFACE, "Brightness").to_variant();
    let result = proxy
        .call_future("Get", Some(&args), gio::DBusCallFlags::NONE, -1)
        .await
        .ok()?;
    let (boxed,): (glib::Variant,) = result.get()?;
    boxed.get::<i32>()
}

async fn settings_daemon_set_brightness(conn: &gio::DBusConnection, percent: i32) -> anyhow::Result<()> {
    let proxy = gio::DBusProxy::new_future(
        conn,
        gio::DBusProxyFlags::NONE,
        None,
        Some(SETTINGS_DAEMON_POWER_DEST),
        SETTINGS_DAEMON_POWER_PATH,
        PROPERTIES_IFACE,
    )
    .await
    .map_err(|err| anyhow::anyhow!("failed to build SettingsDaemon.Power proxy: {err}"))?;

    let boxed_value = glib::Variant::from_variant(&percent.to_variant());
    let args = (SETTINGS_DAEMON_POWER_SCREEN_IFACE, "Brightness", boxed_value).to_variant();

    proxy
        .call_future("Set", Some(&args), gio::DBusCallFlags::NONE, -1)
        .await
        .map_err(|err| anyhow::anyhow!("failed to set screen brightness: {err}"))?;
    Ok(())
}

/// Current screen brightness as a 0-100 percentage, or `None` if nothing
/// could read it. Backs [`brightness_available`]; the ring restores from
/// [`read_brightness`].
pub async fn get_brightness(conn: &gio::DBusConnection) -> Option<i32> {
    if let Some(dev) = detect_device() {
        if let (Some(current), Some(max)) = (
            read_u32(&dev.dir.join("brightness")),
            read_u32(&dev.dir.join("max_brightness")),
        ) {
            if max > 0 {
                return Some(((current * 100) / max).clamp(0, 100) as i32);
            }
        }
    }
    settings_daemon_get_brightness(conn).await
}

/// A backlight level that can be written back unchanged. Phosh's FLX1
/// backlight mode works in raw sysfs steps, so a percentage rounds a
/// low level (30 of 2047) to a value below Phosh's minimum (21).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Brightness {
    Raw { raw: u32, max: u32 },
    Percent(u8),
}

impl Brightness {
    /// Whether the level is at least 1% of the maximum, rounded down. Lower
    /// readings (0 while Phosh has the backlight forced off) are transient.
    pub fn is_plausible_baseline(&self) -> bool {
        match self {
            Brightness::Raw { raw, max } => *raw >= (*max / 100).max(1),
            Brightness::Percent(percent) => *percent >= 1,
        }
    }
}

/// `raw` rescaled from a range of `from_max` to one of `to_max`.
fn rescale_raw(raw: u32, from_max: u32, to_max: u32) -> u32 {
    if from_max == to_max || from_max == 0 {
        return raw.min(to_max);
    }
    ((raw as u64 * to_max as u64) / from_max as u64) as u32
}

/// The current level, as a raw sysfs value when a backlight device exists.
pub async fn read_brightness(conn: &gio::DBusConnection) -> Option<Brightness> {
    if let Some(dev) = detect_device() {
        if let (Some(raw), Some(max)) = (
            read_u32(&dev.dir.join("brightness")),
            read_u32(&dev.dir.join("max_brightness")),
        ) {
            if max > 0 {
                return Some(Brightness::Raw { raw, max });
            }
        }
    }
    settings_daemon_get_brightness(conn)
        .await
        .map(|percent| Brightness::Percent(percent.clamp(0, 100) as u8))
}

/// Writes back a level from [`read_brightness`].
pub async fn restore_brightness(conn: &gio::DBusConnection, level: Brightness) -> anyhow::Result<()> {
    match level {
        Brightness::Percent(percent) => set_screen_brightness(conn, percent).await,
        Brightness::Raw { raw, max } => {
            let Some(dev) = detect_device() else {
                return set_screen_brightness(conn, ((raw * 100) / max.max(1)).min(100) as u8).await;
            };
            let current_max = read_u32(&dev.dir.join("max_brightness")).unwrap_or(max);
            let raw = rescale_raw(raw, max, current_max);
            if logind_set_brightness(dev.class, &dev.name, raw).await.is_some() {
                return Ok(());
            }
            if sysfs_set_brightness(&dev, raw).is_some() {
                return Ok(());
            }
            set_screen_brightness(conn, ((raw * 100) / current_max.max(1)).min(100) as u8).await
        }
    }
}

/// Whether brightness control appears possible, used to decide whether to
/// offer Gentle Wake's screen glow. A readable level is taken to imply that
/// writing works; probing the write tiers would change the brightness.
pub async fn brightness_available(conn: &gio::DBusConnection) -> bool {
    get_brightness(conn).await.is_some()
}

/// Sets screen brightness to a 0-100 percentage, trying each tier in turn.
/// Only the last tier's failure is returned: the first two legitimately do not
/// apply on many systems (no backlight device, no logind session access).
pub async fn set_screen_brightness(conn: &gio::DBusConnection, percent: u8) -> anyhow::Result<()> {
    let percent = percent.min(100);

    if let Some(dev) = detect_device() {
        let Some(max) = read_u32(&dev.dir.join("max_brightness")) else {
            return settings_daemon_set_brightness(conn, percent as i32).await;
        };
        let raw = (percent as u32 * max) / 100;

        if logind_set_brightness(dev.class, &dev.name, raw).await.is_some() {
            return Ok(());
        }
        if sysfs_set_brightness(&dev, raw).is_some() {
            return Ok(());
        }
    }

    settings_daemon_set_brightness(conn, percent as i32).await
}

/// Sunrise gradient key colors: dim red, gold, bright white.
const DIM_RED: (u8, u8, u8) = (40, 8, 4);
const GOLD: (u8, u8, u8) = (255, 170, 50);
const WHITE: (u8, u8, u8) = (255, 255, 255);

/// The glow's color at `progress` from `0.0` (dim red) to `1.0` (bright
/// white). Values outside `[0.0, 1.0]` are clamped.
///
/// Unused for painting: the glow is a hardware brightness ramp (see
/// [`set_screen_brightness`]), which has no color. Kept and tested for a
/// possible in-app visual.
pub fn glow_color(progress: f64) -> [u8; 3] {
    let p = progress.clamp(0.0, 1.0);
    let (from, to, local_p) = if p < 0.5 {
        (DIM_RED, GOLD, p / 0.5)
    } else {
        (GOLD, WHITE, (p - 0.5) / 0.5)
    };
    [
        lerp_u8(from.0, to.0, local_p),
        lerp_u8(from.1, to.1, local_p),
        lerp_u8(from.2, to.2, local_p),
    ]
}

fn lerp_u8(a: u8, b: u8, t: f64) -> u8 {
    (a as f64 + (b as f64 - a as f64) * t).round() as u8
}

/// [`glow_color`] scaled by a brightness envelope. Unused, kept for the same
/// reason.
pub fn glow_color_scaled(progress: f64, start_brightness_percent: u8, end_brightness_percent: u8) -> [u8; 3] {
    let base = glow_color(progress);
    let p = progress.clamp(0.0, 1.0);
    let brightness_percent = start_brightness_percent as f64
        + (end_brightness_percent as f64 - start_brightness_percent as f64) * p;
    let scale = (brightness_percent / 100.0).clamp(0.0, 1.0);
    [
        (base[0] as f64 * scale).round() as u8,
        (base[1] as f64 * scale).round() as u8,
        (base[2] as f64 * scale).round() as u8,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glow_starts_at_dim_red() {
        assert_eq!(glow_color(0.0), [DIM_RED.0, DIM_RED.1, DIM_RED.2]);
    }

    #[test]
    fn glow_passes_through_gold_at_midpoint() {
        assert_eq!(glow_color(0.5), [GOLD.0, GOLD.1, GOLD.2]);
    }

    #[test]
    fn glow_ends_at_white() {
        assert_eq!(glow_color(1.0), [WHITE.0, WHITE.1, WHITE.2]);
    }

    #[test]
    fn glow_clamps_out_of_range_progress() {
        assert_eq!(glow_color(-5.0), glow_color(0.0));
        assert_eq!(glow_color(5.0), glow_color(1.0));
    }

    #[test]
    fn glow_brightness_is_monotonically_non_decreasing() {
        let samples: Vec<u32> = (0..=20)
            .map(|i| {
                let [r, g, b] = glow_color(i as f64 / 20.0);
                r as u32 + g as u32 + b as u32
            })
            .collect();
        for pair in samples.windows(2) {
            assert!(pair[1] >= pair[0], "brightness decreased: {pair:?}");
        }
    }

    #[test]
    fn scaled_glow_at_full_brightness_matches_unscaled() {
        for i in 0..=10 {
            let progress = i as f64 / 10.0;
            assert_eq!(glow_color_scaled(progress, 100, 100), glow_color(progress));
        }
    }

    #[test]
    fn scaled_glow_at_zero_brightness_is_black() {
        assert_eq!(glow_color_scaled(0.5, 0, 0), [0, 0, 0]);
    }

    #[test]
    fn scaled_glow_brightness_ramps_between_start_and_end() {
        let dim_end = glow_color_scaled(1.0, 10, 10);
        let bright_end = glow_color_scaled(1.0, 100, 100);
        assert!(dim_end[0] < bright_end[0]);
        assert_eq!(bright_end, [255, 255, 255]);
        assert_eq!(dim_end, [26, 26, 26]);
    }

    #[test]
    fn power_save_args_box_the_value_once() {
        let args = power_save_args(0);
        assert_eq!(args.type_().as_str(), "(ssv)");
        let boxed = args.child_value(2).as_variant().expect("a boxed value");
        assert_eq!(boxed.type_().as_str(), "i");
        assert_eq!(boxed.get::<i32>(), Some(0));
    }

    #[test]
    fn rescale_keeps_a_matching_range_unchanged() {
        assert_eq!(rescale_raw(30, 2047, 2047), 30);
    }

    #[test]
    fn rescale_scales_between_ranges() {
        assert_eq!(rescale_raw(1000, 2000, 100), 50);
        assert_eq!(rescale_raw(5000, 2047, 2047), 2047);
    }

    #[test]
    fn baseline_minimum_is_one_percent_of_max_rounded_down() {
        assert!(!Brightness::Raw { raw: 0, max: 2047 }.is_plausible_baseline());
        assert!(!Brightness::Raw { raw: 19, max: 2047 }.is_plausible_baseline());
        assert!(Brightness::Raw { raw: 20, max: 2047 }.is_plausible_baseline());
        assert!(Brightness::Raw { raw: 21, max: 2047 }.is_plausible_baseline());
        assert!(!Brightness::Raw { raw: 0, max: 50 }.is_plausible_baseline());
        assert!(Brightness::Raw { raw: 1, max: 50 }.is_plausible_baseline());
        assert!(!Brightness::Percent(0).is_plausible_baseline());
        assert!(Brightness::Percent(1).is_plausible_baseline());
    }
}
