use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

pub use crate::model::AudioDestination;

/// App-wide settings, in a file of their own for values that are neither
/// per-alarm nor per-sound.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppSettings {
    /// Used when an alarm's `Specific` destination is unavailable at ring time
    /// (sink disconnected, port unavailable, headset unplugged). Ideally a
    /// permanent output, as the Settings UI advises; nothing enforces that.
    pub backup_sink: Option<AudioDestination>,
    /// Switches feedbackd from Silent to Quiet while vibrating, so
    /// `Haptic.Vibrate` is not rejected.
    #[serde(default = "default_true")]
    pub vibrate_in_silent: bool,
}

fn default_true() -> bool {
    true
}

impl Default for AppSettings {
    fn default() -> Self {
        AppSettings {
            backup_sink: None,
            vibrate_in_silent: true,
        }
    }
}

impl AppSettings {
    pub fn load(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            return Ok(Self::default());
        }
        let data = std::fs::read_to_string(path)?;
        serde_json::from_str(&data).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
    }

    pub fn save(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_string_pretty(self)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        std::fs::write(path, data)
    }
}

/// A selectable audio destination: a whole sink with no ports (Bluetooth, Null
/// Output), or one available port on a sink that has several.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DestinationInfo {
    pub destination: AudioDestination,
    /// Picker label: the port's description ("Output to speaker") for a port,
    /// else the sink's description ("soundcore Space Q45 (black)").
    pub description: String,
    /// True when the sink's `device.form_factor` is `internal`. A hint for
    /// labeling in the Settings UI; not every driver sets it, so nothing is
    /// filtered or enforced against it.
    pub is_internal: bool,
}

#[derive(Debug, Deserialize)]
struct PactlPort {
    name: String,
    description: String,
    availability: String,
}

#[derive(Debug, Deserialize)]
struct PactlSink {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    driver: String,
    #[serde(default)]
    properties: std::collections::HashMap<String, String>,
    #[serde(default)]
    ports: Vec<PactlPort>,
}

/// Discovers destinations with `pactl -f json list sinks`. Returns an empty list
/// on any failure (`pactl` missing, non-zero exit, unparseable output); callers
/// treat that as a normal case.
///
/// Two filters:
/// - Sinks whose `driver` is `module-null-sink.c` are skipped, whatever their
///   label.
/// - Ports with `"availability":"not available"` are skipped (a headset port
///   with nothing plugged in). A sink with ports is offered only through its
///   available ones, never as the bare sink.
pub fn discover_destinations() -> Vec<DestinationInfo> {
    let output = match std::process::Command::new("pactl")
        .args(["-f", "json", "list", "sinks"])
        .output()
    {
        Ok(output) if output.status.success() => output,
        Ok(output) => {
            tracing::warn!(
                status = %output.status,
                stderr = %String::from_utf8_lossy(&output.stderr),
                "pactl list sinks failed"
            );
            return Vec::new();
        }
        Err(err) => {
            tracing::warn!(%err, "failed to run pactl for destination discovery");
            return Vec::new();
        }
    };

    let raw_sinks: Vec<PactlSink> = match serde_json::from_slice(&output.stdout) {
        Ok(sinks) => sinks,
        Err(err) => {
            tracing::warn!(%err, "failed to parse pactl sink list");
            return Vec::new();
        }
    };

    let mut destinations = Vec::new();
    for sink in raw_sinks {
        if sink.driver == "module-null-sink.c" {
            continue;
        }
        let is_internal = sink
            .properties
            .get("device.form_factor")
            .is_some_and(|v| v == "internal");

        let available_ports: Vec<&PactlPort> = sink
            .ports
            .iter()
            .filter(|p| p.availability != "not available")
            .collect();

        if available_ports.len() <= 1 {
            // Zero or one port: there is no choice to offer, so the sink is
            // presented itself, labeled with its description (a device name
            // like "soundcore Space Q45 (black)") instead of a generic port
            // label like "Headset". The port name, if any, is still recorded
            // so `force_speaker` sets it.
            destinations.push(DestinationInfo {
                destination: AudioDestination {
                    sink: sink.name.clone(),
                    port: available_ports.first().map(|p| p.name.clone()),
                },
                description: sink.description.clone(),
                is_internal,
            });
        } else {
            // Several available ports (earpiece and speaker are ports on one
            // sink): each becomes its own destination, labeled with the port
            // description.
            for port in available_ports {
                destinations.push(DestinationInfo {
                    destination: AudioDestination {
                        sink: sink.name.clone(),
                        port: Some(port.name.clone()),
                    },
                    description: port.description.clone(),
                    is_internal,
                });
            }
        }
    }
    destinations
}

/// Resolves the destination to force for `OutputMode::Specific` playback,
/// checking availability at ring time instead of when the alarm was edited (a
/// Bluetooth speaker since turned off, a headset unplugged). Returns
/// `preferred` if available, else `settings.backup_sink`, else `None`;
/// `RingSession::start` falls back to the active sink with a warning on
/// `None`.
pub fn resolve_speaker_destination(
    preferred: &AudioDestination,
    settings: &AppSettings,
) -> Option<AudioDestination> {
    let live = discover_destinations();
    let is_available = |dest: &AudioDestination| live.iter().any(|d| &d.destination == dest);

    if is_available(preferred) {
        return Some(preferred.clone());
    }
    tracing::warn!(?preferred, "configured speaker destination isn't currently available");

    if let Some(backup) = &settings.backup_sink {
        if is_available(backup) {
            tracing::warn!(?backup, "using backup sink instead");
            return Some(backup.clone());
        }
        tracing::warn!(?backup, "backup sink also isn't currently available");
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> std::path::PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!("cajole-settings-test-{}", uuid::Uuid::new_v4()));
        dir
    }

    #[test]
    fn missing_file_loads_as_default() {
        let dir = tempdir();
        let path = dir.join("settings.json");
        let settings = AppSettings::load(&path).unwrap();
        assert_eq!(settings, AppSettings::default());
    }

    #[test]
    fn legacy_file_with_override_silent_still_loads() {
        let dir = tempdir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, r#"{"backup_sink": null, "override_silent": false}"#).unwrap();
        let loaded = AppSettings::load(&path).unwrap();
        assert_eq!(loaded, AppSettings::default());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_vibrate_in_silent_defaults_to_on() {
        let dir = tempdir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, r#"{"backup_sink": null}"#).unwrap();
        assert!(AppSettings::load(&path).unwrap().vibrate_in_silent);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempdir();
        let path = dir.join("settings.json");
        let settings = AppSettings {
            backup_sink: Some(AudioDestination {
                sink: "bluez_sink.AA_BB_CC".to_string(),
                port: None,
            }),
            vibrate_in_silent: false,
        };
        settings.save(&path).unwrap();
        let loaded = AppSettings::load(&path).unwrap();
        assert_eq!(loaded, settings);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_falls_back_to_backup_when_preferred_unavailable() {
        // Both destinations are nonexistent sink names, so neither is
        // available whatever PulseAudio session the test runs in.
        let preferred = AudioDestination {
            sink: "definitely.not.a.real.sink".to_string(),
            port: None,
        };
        let settings = AppSettings {
            backup_sink: None,
            vibrate_in_silent: true,
        };
        assert_eq!(resolve_speaker_destination(&preferred, &settings), None);
    }
}
