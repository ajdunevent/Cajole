//! Alarm model, scheduling, persistence and system integrations. Kept as
//! library modules so `cargo test` can exercise them without a GTK application
//! or display.

/// The application's D-Bus and desktop-entry identifier. Used by the GTK
/// `Application`, `fbcli --app-id`, the Haptic `Vibrate` call and the
/// autostart `.desktop` filename.
pub const APP_ID: &str = "net.dunevent.Cajole";

pub mod activity;
pub mod auto_brightness;
pub mod autostart;
pub mod display;
pub mod feedback;
pub mod haptic;
pub mod holiday;
pub mod lock_watch;
pub mod model;
pub mod mpris;
pub mod ramp;
pub mod ring;
pub mod sandbox;
pub mod schedule;
pub mod scheduler;
pub mod session_inhibit;
pub mod settings;
pub mod silent_override;
pub mod sound_library;
pub mod store;
pub mod volume;
pub mod wake_timer;
pub mod waked;

pub use model::{Alarm, AudioDestination, OutputMode, RepeatDays, VibrationPattern};
pub use ring::RingSession;
pub use schedule::{next_trigger, TriggerTracker};
pub use store::{AlarmStore, StoreError};
