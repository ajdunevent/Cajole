//! Pauses Phosh's automatic brightness while the glow ramp runs.
//!
//! Phosh watches `ambient-enabled` and aborts its brightness transition
//! when it turns off. Left on, its adjustments fight the ramp
//! and the screen blinks.

use gio::prelude::*;

const SCHEMA: &str = "org.gnome.settings-daemon.plugins.power";
const KEY: &str = "ambient-enabled";

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

/// Turns automatic brightness off. `None` when the schema is missing or
/// the write failed.
pub fn suspend() -> Option<Suspended> {
    let source = gio::SettingsSchemaSource::default()?;
    let schema = source.lookup(SCHEMA, true)?;
    if !schema.has_key(KEY) {
        return None;
    }
    let settings = gio::Settings::new(SCHEMA);
    let prior = settings.user_value(KEY).and_then(|value| value.get::<bool>());
    let was_on = prior.unwrap_or_else(|| settings.boolean(KEY));
    if was_on {
        if let Err(err) = settings.set_boolean(KEY, false) {
            tracing::warn!(%err, "failed to turn automatic brightness off");
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
        match self.prior {
            Some(value) => {
                if let Err(err) = self.settings.set_boolean(KEY, value) {
                    tracing::warn!(%err, "failed to restore automatic brightness");
                }
            }
            None => self.settings.reset(KEY),
        }
        gio::Settings::sync();
    }
}
