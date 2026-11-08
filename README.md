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

### Flatpak (recommended)

| <a href="https://ajdunevent.github.io/AJHub/"><picture><source media="(prefers-color-scheme: dark)" srcset="https://ajdunevent.github.io/AJHub/badges/button-dark.svg"><img src="https://ajdunevent.github.io/AJHub/badges/button-light.svg" alt="AJHub, published with FrogPak" height="48"></picture></a> | <a href="https://furihub.github.io/repo/"><picture><source media="(prefers-color-scheme: dark)" srcset="https://furihub.github.io/repo/badges/button-dark.svg"><img src="https://furihub.github.io/repo/badges/button-light.svg" alt="FuriHub, published with FrogPak" height="48"></picture></a> |
| --- | --- |
| x86_64 & aarch64 | aarch64 only |

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
