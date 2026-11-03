use chrono::{DateTime, Local, NaiveDate, NaiveTime, Weekday};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A set of weekdays an alarm repeats on, stored as a bitmask (bit 0 = Monday,
/// matching `chrono::Weekday::num_days_from_monday`).
///
/// An empty set means a one-time alarm: it fires at the next occurrence of its
/// `time` (today if still ahead, else tomorrow), and the scheduler disables it
/// after it rings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RepeatDays(u8);

impl RepeatDays {
    pub const NONE: RepeatDays = RepeatDays(0);

    pub const EVERY_DAY: RepeatDays = RepeatDays(0b0111_1111);

    pub const WEEKDAYS: RepeatDays = RepeatDays(0b0001_1111);

    pub const WEEKENDS: RepeatDays = RepeatDays(0b0110_0000);

    pub fn from_days(days: impl IntoIterator<Item = Weekday>) -> Self {
        let mut mask = 0u8;
        for day in days {
            mask |= 1 << day.num_days_from_monday();
        }
        RepeatDays(mask)
    }

    pub fn contains(&self, day: Weekday) -> bool {
        (self.0 >> day.num_days_from_monday()) & 1 == 1
    }

    pub fn insert(&mut self, day: Weekday) {
        self.0 |= 1 << day.num_days_from_monday();
    }

    pub fn remove(&mut self, day: Weekday) {
        self.0 &= !(1 << day.num_days_from_monday());
    }

    pub fn is_empty(&self) -> bool {
        self.0 == 0
    }

    pub fn iter(&self) -> impl Iterator<Item = Weekday> + '_ {
        [
            Weekday::Mon,
            Weekday::Tue,
            Weekday::Wed,
            Weekday::Thu,
            Weekday::Fri,
            Weekday::Sat,
            Weekday::Sun,
        ]
        .into_iter()
        .filter(move |d| self.contains(*d))
    }
}

/// A specific audio route: a sink and, for hardware with several routes on one
/// sink, a port on it (earpiece, speaker or wired headset on a phone's single
/// hardware sink). `port: None` means the sink has no ports (Bluetooth, Null
/// Output), not "no preference". Defined here because
/// `AppSettings::backup_sink` shares its shape with the per-alarm data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioDestination {
    pub sink: String,
    pub port: Option<String>,
}

/// Where an alarm's sound plays.
///
/// - `ActiveSink`: the system's current default output.
/// - `AllSinks`: every available output at once (speaker, headphones,
///   Bluetooth), so the alarm is not missed because another output is active.
/// - `Specific`: one chosen sink and port, such as the loudspeaker port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputMode {
    ActiveSink,
    AllSinks,
    Specific(AudioDestination),
}

/// The rhythm of an alarm's vibration, separate from its strength
/// (`vibration_start_percent` and `vibration_end_percent` on `Alarm`).
/// `haptic::pattern_shape` turns a pattern into on/off segments. `Constant` is
/// a pulse every 100 ms whose length grows with intensity, so it is solid
/// only at 100%.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VibrationPattern {
    RapidFire,
    SlowRoll,
    Heartbeat,
    Constant,
}

impl Default for VibrationPattern {
    fn default() -> Self {
        VibrationPattern::Constant
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Alarm {
    pub id: Uuid,
    pub label: String,
    pub time: NaiveTime,
    pub repeat: RepeatDays,
    pub enabled: bool,

    /// Whether volume, vibration and glow ramp from start percent to end
    /// percent over `gentle_wake_duration_secs`, or hold one constant level.
    /// Independent of `play_sound`, `vibration_enabled` and `glow_enabled`: a
    /// master switch bundling them made constant-strength vibration impossible
    /// without ramping everything.
    pub gentle_wake_enabled: bool,
    pub gentle_wake_duration_secs: u32,

    /// Master toggle for sound, independent of the chosen `sound_file`, so
    /// switching it off does not lose the selected tone.
    pub play_sound: bool,
    pub sound_file: Option<String>,
    pub output_mode: OutputMode,
    pub volume_start_percent: u8,
    pub volume_end_percent: u8,

    pub vibration_enabled: bool,
    pub vibration_pattern: VibrationPattern,
    pub vibration_start_percent: u8,
    pub vibration_end_percent: u8,

    /// Screen glow: wakes the display and ramps the hardware brightness from
    /// `glow_start_percent` to `glow_end_percent`. The color curve in
    /// `display::glow_color` is unused.
    pub glow_enabled: bool,
    pub glow_start_percent: u8,
    pub glow_end_percent: u8,

    /// How long the alarm rings before stopping itself if nobody dismisses or
    /// snoozes it. Enforced in `scheduler::run`'s auto-dismiss check.
    pub alarm_duration_minutes: u32,

    /// Minutes a snooze postpones the alarm.
    pub snooze_minutes: u32,
    /// Minutes before the scheduled ring time during which an early-dismissal
    /// notification is offered. `0` disables the feature for this alarm.
    pub early_dismissal_window_minutes: u32,

    /// A pending snooze. When set, it overrides the normal schedule (see
    /// `schedule::next_trigger`) until it fires, then the scheduler clears it.
    /// Kept apart from `time` and `repeat` so a snooze never changes the daily
    /// schedule.
    pub snoozed_until: Option<DateTime<Local>>,

    /// Set when a repeating alarm's upcoming occurrence was dismissed early,
    /// to the date skipped. `next_trigger` treats any candidate on or before
    /// this date as invalid.
    ///
    /// A stored absolute `DateTime` caused a bug: editing the alarm's time
    /// afterward left the stale value overriding the edit. A date has nothing
    /// to go stale.
    pub skip_through: Option<NaiveDate>,

    /// Suspended for the duration of a Holiday Mode period, independent of
    /// `enabled`, so an alarm can be selected for exclusion before it is
    /// turned on. Cleared when `holiday::is_relevant` finds no current or
    /// future holiday. Enforced in `scheduler`'s trigger-to-ring check;
    /// `repeat` and `time` are not touched.
    #[serde(default)]
    pub holiday_excluded: bool,

    /// A holiday-only alarm added through the Holiday Mode wizard. Deleted
    /// once its `repeat` pattern has no occurrence left inside the holiday
    /// window (checked after a dismiss and when the wizard's Done reruns the
    /// check). It stores no end date of its own, which would go stale if the
    /// holiday's dates were edited.
    #[serde(default)]
    pub holiday_only: bool,
}

impl Alarm {
    pub fn new(label: impl Into<String>, time: NaiveTime) -> Self {
        Alarm {
            id: Uuid::new_v4(),
            label: label.into(),
            time,
            repeat: RepeatDays::NONE,
            enabled: true,
            gentle_wake_enabled: true,
            gentle_wake_duration_secs: 60,
            play_sound: true,
            sound_file: None,
            output_mode: OutputMode::ActiveSink,
            volume_start_percent: 5,
            volume_end_percent: 85,
            vibration_enabled: false,
            vibration_pattern: VibrationPattern::default(),
            vibration_start_percent: 20,
            vibration_end_percent: 100,
            glow_enabled: false,
            glow_start_percent: 10,
            glow_end_percent: 100,
            alarm_duration_minutes: 10,
            snooze_minutes: 9,
            early_dismissal_window_minutes: 30,
            snoozed_until: None,
            skip_through: None,
            holiday_excluded: false,
            holiday_only: false,
        }
    }

    pub fn is_one_time(&self) -> bool {
        self.repeat.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeat_days_round_trip() {
        let mut r = RepeatDays::NONE;
        assert!(r.is_empty());
        r.insert(Weekday::Mon);
        r.insert(Weekday::Fri);
        assert!(r.contains(Weekday::Mon));
        assert!(r.contains(Weekday::Fri));
        assert!(!r.contains(Weekday::Tue));
        assert_eq!(r.iter().collect::<Vec<_>>(), vec![Weekday::Mon, Weekday::Fri]);
        r.remove(Weekday::Mon);
        assert!(!r.contains(Weekday::Mon));
    }

    #[test]
    fn weekdays_preset_excludes_weekend() {
        assert!(RepeatDays::WEEKDAYS.contains(Weekday::Wed));
        assert!(!RepeatDays::WEEKDAYS.contains(Weekday::Sat));
        assert!(RepeatDays::WEEKENDS.contains(Weekday::Sun));
    }

    #[test]
    fn new_alarm_matches_the_original_bash_ramp_defaults() {
        let alarm = Alarm::new("Test", NaiveTime::from_hms_opt(7, 0, 0).unwrap());
        assert!(alarm.gentle_wake_enabled);
        assert!(alarm.play_sound);
        assert_eq!(alarm.volume_start_percent, 5);
        assert_eq!(alarm.volume_end_percent, 85);
        assert!(!alarm.vibration_enabled);
        assert!(!alarm.glow_enabled);
    }

    #[test]
    fn new_alarm_has_a_sane_default_alarm_duration() {
        let alarm = Alarm::new("Test", NaiveTime::from_hms_opt(7, 0, 0).unwrap());
        assert_eq!(alarm.alarm_duration_minutes, 10);
    }
}
