# Cajole

A gentle alarm clock for Phosh and other GTK4/libadwaita Linux phones.

Alarms can ramp up through volume, vibration and screen brightness instead of
starting at full strength, and can be dismissed from a notification before
they ring. It is built with Rust, GTK4 and libadwaita, and is developed on a
FuriLabs FLX1S running FuriOS.

## Features

- GNOME Clocks-style time picker and alarm list
- Gentle wake: volume, vibration and screen brightness each ramp from a start
  level to an end level over a set time, or hold one level
- Four vibration patterns (Rapid Fire, Slow Roll, Heartbeat and Constant) with
  an intensity setting
- Early-dismissal notification before an alarm rings
- Holiday Mode: pause regular alarms for a date range and add temporary
  alarms that delete themselves afterwards
- Downloadable alarm sounds and custom sound files
- Per-alarm audio destination

## Install

Follow the directions at <https://ajdunevent.github.io/AJHub/> to add the AJHub
Flatpak remote and install Cajole. Bundles for x86_64 and aarch64 are also
attached to each GitHub release.

## Building

Cajole needs GTK 4.10 or newer and libadwaita 1.5 or newer.

To build and install a Flatpak with the GNOME 51 SDK, run `build-aux/dev.sh`.
It asks for the architecture and the output (a `.flatpak` file in `dist/`, an
install, or both).

Without Flatpak, install the GTK 4, libadwaita and GLib development packages
and `glib-compile-resources`, then:

```sh
cargo build --release
cargo test --lib
```

## Releasing

1. Set the new version in `Cargo.toml` and add a `<release>` at the top of the
   list in `data/net.dunevent.Cajole.metainfo.xml`. Release builds fail until
   the two match.
2. Commit, tag `vX.Y.Z` and push the tag. The GitHub mirror builds both
   architectures, checks the tag against the metainfo, publishes a release and
   updates the Flatpak repository. A tag with a hyphen (`v0.2.0-rc1`) makes a
   pre-release, which the Flatpak repository skips.

## Notes for the FLX1S

- The vibration motor only switches on and off, so intensity sets the length of
  each pulse instead of its strength.
- The backlight ramp works through logind, with a sysfs write as a fallback.

## Permissions

| Permission | Used for |
| --- | --- |
| `--share=network` | Downloading alarm sounds |
| `--share=ipc`, `--socket=wayland`, `--socket=fallback-x11`, `--device=dri` | Display |
| `--socket=pulseaudio` | Alarm sound and volume |
| `--system-talk-name=org.freedesktop.login1` | Screen lock state and brightness |
| `--system-talk-name=de.seath.Waked` | Waking the phone from suspend for an alarm |
| `--talk-name=org.sigxcpu.Feedback` | Vibration |
| `--talk-name=org.mpris.MediaPlayer2.*` | Pausing media when an alarm rings |
| `--talk-name=org.gnome.Mutter.DisplayConfig` | Screen power state |
| `--talk-name=org.gnome.Shell.Brightness` | Undimming the screen |
| `--talk-name=org.gnome.Mutter.IdleMonitor` | Detecting activity after an alarm |
| `--talk-name=org.gnome.SessionManager` | Inhibiting idle while an alarm rings |
| `--talk-name=org.gnome.SettingsDaemon.Power` | Automatic brightness |
| `--talk-name=ca.desrt.dconf`, `--filesystem=xdg-run/dconf`, `--filesystem=~/.config/dconf:ro` | Reading and restoring the automatic brightness setting |

## Files

| Path | Contents |
| --- | --- |
| `~/.local/share/cajole/alarms.json` | Alarms |
| `~/.local/share/cajole/settings.json` | Settings |
| `~/.local/share/cajole/holiday.json` | Holiday Mode dates |
| `~/.local/share/cajole/sounds.json` | Sound library |

The Flatpak keeps these under `~/.var/app/net.dunevent.Cajole/data/cajole/`.

## AI Disclaimer

This project has relied extensively on AI-assisted development, primarily Anthropic's Claude (various models), under human direction and manual testing.

### Contribution Profile

```
Phase                    Human █                  ░ AI
───────────────────────────────────────────────────────
Requirements & Scope      100% ████████████████████  0%
Architecture & Design      70% ██████████████░░░░░░ 30%
Implementation              5% █░░░░░░░░░░░░░░░░░░░ 95%
Testing                    95% ███████████████████░  5%
Documentation              30% ██████░░░░░░░░░░░░░░ 70%
```

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).

Cajole bundles no sounds. The sound manager can download optional sounds from
GNOME Clocks, the Android Open Source Project, KDE Ocean and KDE Plasma
Mobile, each under its own license. GNOME Clocks also informed the design.
