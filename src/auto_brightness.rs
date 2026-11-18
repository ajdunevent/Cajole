//! Pauses Phosh's automatic brightness while the glow ramp runs.
//!
//! Phosh watches `ambient-enabled` and aborts its brightness transition
//! when it turns off. Left on, its adjustments fight the ramp
//! and the screen blinks.
//!
//! The change is a real write to the host's dconf, so a marker file records
//! the value to restore. If the app is killed while paused,
//! [`restore_pending`] puts the setting back at the next start.

use std::path::{Path, PathBuf};

use gio::prelude::*;

const SCHEMA: &str = "org.gnome.settings-daemon.plugins.power";
const KEY: &str = "ambient-enabled";
const MARKER_FILE: &str = "auto-brightness-restore";

/// Automatic brightness turned off by this module. Restores the setting
/// when dropped.
pub struct Suspended {
    settings: gio::Settings,
    /// The user-set value, `None` when the key was at its default. A
    /// default is restored with `reset`, since a sandbox cannot see a host
    /// schema override and would otherwise write the wrong value back.
    prior: Option<bool>,
    was_on: bool,
}

/// The settings object for the key, `None` when the schema is missing.
fn open() -> Option<gio::Settings> {
    let source = gio::SettingsSchemaSource::default()?;
    let schema = source.lookup(SCHEMA, true)?;
    if !schema.has_key(KEY) {
        return None;
    }
    Some(gio::Settings::new(SCHEMA))
}

fn marker_path() -> PathBuf {
    glib::user_data_dir().join("cajole").join(MARKER_FILE)
}

/// Marker file text for the value to restore.
fn encode(prior: Option<bool>) -> &'static str {
    match prior {
        None => "default",
        Some(true) => "true",
        Some(false) => "false",
    }
}

/// The value to restore from marker file text. The outer `None` is text
/// that is not a marker.
fn decode(text: &str) -> Option<Option<bool>> {
    match text.trim() {
        "default" => Some(None),
        "true" => Some(Some(true)),
        "false" => Some(Some(false)),
        _ => None,
    }
}

fn write_marker(path: &Path, prior: Option<bool>) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, encode(prior))
}

fn read_marker(path: &Path) -> Option<Option<bool>> {
    decode(&std::fs::read_to_string(path).ok()?)
}

/// Writes `prior` back: the user value, or a reset to the default.
fn put_back(settings: &gio::Settings, prior: Option<bool>) {
    match prior {
        Some(value) => {
            if let Err(err) = settings.set_boolean(KEY, value) {
                tracing::warn!(%err, "failed to restore automatic brightness");
            }
        }
        None => settings.reset(KEY),
    }
    gio::Settings::sync();
}

/// Turns automatic brightness off. `None` when the schema is missing or
/// the write failed.
pub fn suspend() -> Option<Suspended> {
    let settings = open()?;
    let prior = settings.user_value(KEY).and_then(|value| value.get::<bool>());
    let was_on = prior.unwrap_or_else(|| settings.boolean(KEY));
    if was_on {
        let marker = marker_path();
        // Written first, so a kill between the two steps still restores.
        if let Err(err) = write_marker(&marker, prior) {
            tracing::warn!(%err, "failed to write the automatic brightness marker");
        }
        if let Err(err) = settings.set_boolean(KEY, false) {
            tracing::warn!(%err, "failed to turn automatic brightness off");
            let _ = std::fs::remove_file(&marker);
            return None;
        }
        gio::Settings::sync();
    }
    Some(Suspended {
        settings,
        prior,
        was_on,
    })
}

/// Restores automatic brightness after an earlier run was killed while it
/// was paused. Called once at startup. A setting that is no longer off was
/// changed by the user since, and is left alone.
pub fn restore_pending() {
    let marker = marker_path();
    let Some(prior) = read_marker(&marker) else {
        // Absent, or not a marker: nothing to restore.
        let _ = std::fs::remove_file(&marker);
        return;
    };
    if let Some(settings) = open() {
        if !settings.boolean(KEY) {
            tracing::info!("restoring automatic brightness left off by an earlier run");
            put_back(&settings, prior);
        }
    }
    let _ = std::fs::remove_file(&marker);
}

impl Suspended {
    /// Whether automatic brightness was on before it was suspended.
    pub fn was_on(&self) -> bool {
        self.was_on
    }
}

impl Drop for Suspended {
    fn drop(&mut self) {
        if !self.was_on {
            return;
        }
        put_back(&self.settings, self.prior);
        let _ = std::fs::remove_file(marker_path());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!("cajole-auto-brightness-test-{}", uuid::Uuid::new_v4()));
        dir
    }

    #[test]
    fn marker_text_round_trips() {
        for prior in [None, Some(true), Some(false)] {
            assert_eq!(decode(encode(prior)), Some(prior));
        }
    }

    #[test]
    fn marker_text_tolerates_whitespace_and_rejects_garbage() {
        assert_eq!(decode("true\n"), Some(Some(true)));
        assert_eq!(decode(""), None);
        assert_eq!(decode("yes"), None);
    }

    #[test]
    fn marker_file_round_trips_and_creates_its_directory() {
        let path = tempdir().join("nested").join(MARKER_FILE);
        write_marker(&path, Some(true)).unwrap();
        assert_eq!(read_marker(&path), Some(Some(true)));
        write_marker(&path, None).unwrap();
        assert_eq!(read_marker(&path), Some(None));
    }

    #[test]
    fn missing_marker_reads_as_nothing_to_restore() {
        assert_eq!(read_marker(&tempdir().join(MARKER_FILE)), None);
    }
}
