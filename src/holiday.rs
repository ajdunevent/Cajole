use std::io;
use std::path::Path;

use chrono::{DateTime, Datelike, Local, NaiveDate};
use serde::{Deserialize, Serialize};

use crate::model::Alarm;
use crate::schedule::next_trigger;
use crate::store::{AlarmStore, StoreError};

/// The single Holiday Mode window. The wizard edits this one value instead of
/// adding to a list, and every alarm checks against it directly.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct HolidaySettings {
    pub start: Option<NaiveDate>,
    pub end: Option<NaiveDate>,
}

impl HolidaySettings {
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

    /// Whether `date` falls inside the window, inclusive of both ends. `false`
    /// when either date is unset; both are required.
    pub fn is_active_on(&self, date: NaiveDate) -> bool {
        match (self.start, self.end) {
            (Some(start), Some(end)) => date >= start && date <= end,
            _ => false,
        }
    }

    /// Whether the window still matters as of `today`: active now or still
    /// ahead. `false` once the end date has passed or nothing is configured.
    /// This lets a `holiday_excluded` flag self-clear instead of carrying into
    /// the next holiday.
    pub fn is_current_or_future(&self, today: NaiveDate) -> bool {
        self.end.is_some_and(|end| end >= today)
    }
}

/// Whether `alarm` should be suppressed now, given `settings`.
/// `holiday_excluded` alarms are suppressed during an active holiday, and
/// `holiday_only` alarms outside one. Other alarms are never suppressed.
pub fn should_suppress(alarm: &Alarm, settings: &HolidaySettings, today: NaiveDate) -> bool {
    let active = settings.is_active_on(today);
    (alarm.holiday_excluded && active) || (alarm.holiday_only && !active)
}

/// Whether a `holiday_only` alarm has nothing left to fire for and should be
/// deleted. Reuses [`next_trigger`]: the soonest occurrence only moves further
/// out, so if it falls after `settings.end`, every later one does too. `None`
/// from either side (no trigger left, or no holiday configured) also means
/// nothing is left.
///
/// This is a live check against `settings`, so editing the holiday's dates
/// later takes effect without updating every `holiday_only` alarm.
pub fn holiday_alarm_is_exhausted(
    alarm: &Alarm,
    settings: &HolidaySettings,
    now: DateTime<Local>,
) -> bool {
    match (settings.end, next_trigger(alarm, now)) {
        (Some(holiday_end), Some(next)) => next.date_naive() > holiday_end,
        _ => true,
    }
}

/// Housekeeping safe at any evaluation point: clears `holiday_excluded` once
/// no holiday is current or upcoming, and deletes `holiday_only` alarms once
/// the holiday is over. It excludes [`prune_exhausted`]: an alarm's next
/// occurrence is past the holiday's end the moment its last one fires, so
/// pruning on every check would delete it mid-ring.
pub fn sweep(
    store: &mut AlarmStore,
    settings: &HolidaySettings,
    now: DateTime<Local>,
) -> Result<(), StoreError> {
    let holiday_matters = settings.is_current_or_future(now.date_naive());
    if holiday_matters {
        return Ok(());
    }
    for alarm in store.alarms().to_vec() {
        if alarm.holiday_only {
            store.remove(alarm.id)?;
        } else if alarm.holiday_excluded {
            let mut updated = alarm.clone();
            updated.holiday_excluded = false;
            store.upsert(updated)?;
        }
    }
    Ok(())
}

/// Deletes enabled `holiday_only` alarms with no occurrence left in the
/// holiday window, e.g. after the dates were shortened. Run at explicit points
/// (the wizard's Done, and after a dismiss), never periodically; see
/// [`sweep`]. Disabled alarms are kept, since switching one off is a choice.
pub fn prune_exhausted(
    store: &mut AlarmStore,
    settings: &HolidaySettings,
    now: DateTime<Local>,
) -> Result<(), StoreError> {
    for alarm in store.alarms().to_vec() {
        if alarm.holiday_only && alarm.enabled && holiday_alarm_is_exhausted(&alarm, settings, now)
        {
            store.remove(alarm.id)?;
        }
    }
    Ok(())
}

/// Data behind the pre-holiday banner: days until the holiday starts, and the
/// number of enabled alarms with an occurrence inside the window (an excluded
/// regular alarm about to be skipped, or a `holiday_only` alarm about to ring
/// for the first time).
pub struct UpcomingHoliday {
    pub days_until: i64,
    pub affected_alarms: usize,
}

/// `None` when there is nothing to mention. Checked only before the holiday
/// starts (afterwards the active-holiday banner and each row's "off during
/// Holiday Mode" label cover it). Shown when the holiday starts tomorrow,
/// whether or not an alarm is affected (the heads-up for a `holiday_only`
/// alarm about to ring first), or when any alarm has an occurrence inside the
/// window, however many days out.
pub fn upcoming_holiday(
    alarms: &[Alarm],
    settings: &HolidaySettings,
    now: DateTime<Local>,
) -> Option<UpcomingHoliday> {
    let start = settings.start?;
    let end = settings.end?;
    let today = now.date_naive();
    if today >= start {
        return None;
    }
    let days_until = (start - today).num_days();
    let affected_alarms = alarms
        .iter()
        .filter(|alarm| {
            alarm.enabled
                && (alarm.holiday_excluded || alarm.holiday_only)
                && occurs_in_window(alarm, start, end, now)
        })
        .count();
    if days_until == 1 || affected_alarms > 0 {
        Some(UpcomingHoliday {
            days_until,
            affected_alarms,
        })
    } else {
        None
    }
}

/// Whether `alarm` rings on any date in `[start, end]`, not just whether its
/// next occurrence lands there. A repeating alarm's next occurrence rolls
/// forward daily, so checking only that would make the count grow during the
/// run-up although the affected alarms were fixed when the dates were set. A
/// one-time alarm has one occurrence, so the `next_trigger` check is exact for
/// it.
fn occurs_in_window(alarm: &Alarm, start: NaiveDate, end: NaiveDate, now: DateTime<Local>) -> bool {
    if alarm.repeat.is_empty() {
        return next_trigger(alarm, now).is_some_and(|trigger| (start..=end).contains(&trigger.date_naive()));
    }
    let mut day = start;
    loop {
        if alarm.repeat.contains(day.weekday()) {
            return true;
        }
        if day == end {
            return false;
        }
        day = day.succ_opt().expect("end is a real date reachable by stepping forward from start");
    }
}

/// The banner text: "N day(s) until Holiday Mode", plus the affected alarm
/// count when nonzero. Each count is worded separately, since "1 day" and "2
/// alarms" can share a sentence.
pub fn format_upcoming_holiday_banner(info: &UpcomingHoliday) -> String {
    let day_word = if info.days_until == 1 { "day" } else { "days" };
    let mut text = format!("Holiday Mode in {} {day_word}", info.days_until);
    if info.affected_alarms > 0 {
        let alarm_word = if info.affected_alarms == 1 {
            "alarm"
        } else {
            "alarms"
        };
        text.push_str(&format!(
            "; {} {alarm_word} affected",
            info.affected_alarms
        ));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RepeatDays;
    use chrono::{NaiveTime, TimeZone, Weekday};

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn at(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(y, m, d, h, min, 0).unwrap()
    }

    #[test]
    fn inactive_with_only_one_date_set() {
        let start_only = HolidaySettings {
            start: Some(date(2026, 9, 24)),
            end: None,
        };
        assert!(!start_only.is_active_on(date(2026, 9, 25)));

        let end_only = HolidaySettings {
            start: None,
            end: Some(date(2026, 9, 28)),
        };
        assert!(!end_only.is_active_on(date(2026, 9, 25)));
    }

    #[test]
    fn active_is_inclusive_of_both_ends() {
        let settings = HolidaySettings {
            start: Some(date(2026, 9, 24)),
            end: Some(date(2026, 9, 28)),
        };
        assert!(settings.is_active_on(date(2026, 9, 24)));
        assert!(settings.is_active_on(date(2026, 9, 26)));
        assert!(settings.is_active_on(date(2026, 9, 28)));
        assert!(!settings.is_active_on(date(2026, 9, 23)));
        assert!(!settings.is_active_on(date(2026, 9, 29)));
    }

    #[test]
    fn current_or_future_is_false_once_the_end_date_passes() {
        let settings = HolidaySettings {
            start: Some(date(2026, 9, 24)),
            end: Some(date(2026, 9, 28)),
        };
        assert!(settings.is_current_or_future(date(2026, 9, 20))); // before it starts
        assert!(settings.is_current_or_future(date(2026, 9, 26))); // during it
        assert!(settings.is_current_or_future(date(2026, 9, 28))); // its last day
        assert!(!settings.is_current_or_future(date(2026, 9, 29))); // the day after

        assert!(!HolidaySettings::default().is_current_or_future(date(2026, 9, 20)));
    }

    #[test]
    fn holiday_excluded_alarm_suppressed_only_during_the_holiday() {
        let mut alarm = Alarm::new("Work", NaiveTime::from_hms_opt(6, 0, 0).unwrap());
        alarm.holiday_excluded = true;
        let settings = HolidaySettings {
            start: Some(date(2026, 9, 24)),
            end: Some(date(2026, 9, 28)),
        };
        assert!(!should_suppress(&alarm, &settings, date(2026, 9, 23)));
        assert!(should_suppress(&alarm, &settings, date(2026, 9, 26)));
        assert!(!should_suppress(&alarm, &settings, date(2026, 9, 29)));
    }

    #[test]
    fn holiday_only_alarm_suppressed_everywhere_but_the_holiday() {
        let mut alarm = Alarm::new("Sleep in", NaiveTime::from_hms_opt(9, 0, 0).unwrap());
        alarm.holiday_only = true;
        let settings = HolidaySettings {
            start: Some(date(2026, 9, 24)),
            end: Some(date(2026, 9, 28)),
        };
        assert!(should_suppress(&alarm, &settings, date(2026, 9, 23)));
        assert!(!should_suppress(&alarm, &settings, date(2026, 9, 26)));
        assert!(should_suppress(&alarm, &settings, date(2026, 9, 29)));
    }

    #[test]
    fn plain_alarm_never_suppressed() {
        let alarm = Alarm::new("Normal", NaiveTime::from_hms_opt(7, 0, 0).unwrap());
        let settings = HolidaySettings {
            start: Some(date(2026, 9, 24)),
            end: Some(date(2026, 9, 28)),
        };
        assert!(!should_suppress(&alarm, &settings, date(2026, 9, 26)));
    }

    #[test]
    fn holiday_only_alarm_exhausted_once_its_next_occurrence_is_past_the_end() {
        // 2026-09-22 is a Tuesday and the alarm repeats Tuesday only, so the
        // next occurrence is 2026-09-29, past the holiday's end of 2026-09-28
        // (a Monday).
        let mut alarm = Alarm::new("Sleep in", NaiveTime::from_hms_opt(9, 0, 0).unwrap());
        alarm.holiday_only = true;
        alarm.repeat = RepeatDays::from_days([Weekday::Tue]);
        let settings = HolidaySettings {
            start: Some(date(2026, 9, 24)),
            end: Some(date(2026, 9, 28)),
        };
        let now = at(2026, 9, 22, 9, 1); // just past today's own 9:00 alarm time
        assert!(holiday_alarm_is_exhausted(&alarm, &settings, now));
    }

    #[test]
    fn holiday_only_alarm_not_exhausted_while_an_occurrence_remains() {
        // Repeats Tuesday only; from the holiday's start (a Thursday) the next
        // Tuesday is 2026-09-29, inside the window (through Wednesday
        // 2026-09-30).
        let mut alarm = Alarm::new("Sleep in", NaiveTime::from_hms_opt(9, 0, 0).unwrap());
        alarm.holiday_only = true;
        alarm.repeat = RepeatDays::from_days([Weekday::Tue]);
        let settings = HolidaySettings {
            start: Some(date(2026, 9, 24)),
            end: Some(date(2026, 9, 30)),
        };
        let now = at(2026, 9, 24, 6, 0);
        assert!(!holiday_alarm_is_exhausted(&alarm, &settings, now));
    }

    #[test]
    fn holiday_only_alarm_exhausted_when_holiday_is_cleared() {
        let mut alarm = Alarm::new("Sleep in", NaiveTime::from_hms_opt(9, 0, 0).unwrap());
        alarm.holiday_only = true;
        alarm.repeat = RepeatDays::from_days([Weekday::Tue]);
        let now = at(2026, 9, 22, 6, 0);
        assert!(holiday_alarm_is_exhausted(
            &alarm,
            &HolidaySettings::default(),
            now
        ));
    }

    fn temp_store() -> (AlarmStore, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("cajole-holiday-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("alarms.json");
        (AlarmStore::load(&path).unwrap(), dir)
    }

    #[test]
    fn sweep_clears_exclusion_once_the_holiday_has_passed() {
        let (mut store, dir) = temp_store();
        let mut alarm = Alarm::new("Work", NaiveTime::from_hms_opt(6, 0, 0).unwrap());
        alarm.holiday_excluded = true;
        let id = alarm.id;
        store.upsert(alarm).unwrap();
        let settings = HolidaySettings {
            start: Some(date(2026, 9, 24)),
            end: Some(date(2026, 9, 28)),
        };
        // During the holiday the flag stays.
        sweep(&mut store, &settings, at(2026, 9, 26, 12, 0)).unwrap();
        assert!(store.get(id).unwrap().holiday_excluded);
        // The day after, it self-clears.
        sweep(&mut store, &settings, at(2026, 9, 29, 0, 1)).unwrap();
        assert!(!store.get(id).unwrap().holiday_excluded);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn prune_exhausted_deletes_an_exhausted_holiday_alarm_but_keeps_a_live_one() {
        let (mut store, dir) = temp_store();
        let mut exhausted = Alarm::new("Old", NaiveTime::from_hms_opt(9, 0, 0).unwrap());
        exhausted.holiday_only = true;
        exhausted.repeat = RepeatDays::from_days([Weekday::Tue]); // next is the 29th, past the end
        let mut live = Alarm::new("Live", NaiveTime::from_hms_opt(9, 0, 0).unwrap());
        live.holiday_only = true;
        live.repeat = RepeatDays::from_days([Weekday::Sat]); // the 26th
        let (exhausted_id, live_id) = (exhausted.id, live.id);
        store.upsert(exhausted).unwrap();
        store.upsert(live).unwrap();
        let settings = HolidaySettings {
            start: Some(date(2026, 9, 24)),
            end: Some(date(2026, 9, 28)),
        };
        prune_exhausted(&mut store, &settings, at(2026, 9, 24, 6, 0)).unwrap();
        assert!(store.get(exhausted_id).is_none());
        assert!(store.get(live_id).is_some());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn sweep_deletes_holiday_alarms_once_the_holiday_is_over_even_if_disabled() {
        let (mut store, dir) = temp_store();
        let mut alarm = Alarm::new("Sleep in", NaiveTime::from_hms_opt(9, 0, 0).unwrap());
        alarm.holiday_only = true;
        alarm.enabled = false;
        let id = alarm.id;
        store.upsert(alarm).unwrap();
        let settings = HolidaySettings {
            start: Some(date(2026, 9, 24)),
            end: Some(date(2026, 9, 28)),
        };
        // Disabled but the holiday is still on: kept (a choice, not exhaustion).
        sweep(&mut store, &settings, at(2026, 9, 25, 6, 0)).unwrap();
        assert!(store.get(id).is_some());
        sweep(&mut store, &settings, at(2026, 9, 29, 6, 0)).unwrap();
        assert!(store.get(id).is_none());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn upcoming_holiday_counts_an_excluded_alarm_several_days_out() {
        // Repeats Thursday; the holiday runs 2026-09-24 (a Thursday) through
        // 2026-09-28, so the next occurrence is the window's first day, three
        // days out.
        let mut alarm = Alarm::new("Work", NaiveTime::from_hms_opt(6, 0, 0).unwrap());
        alarm.holiday_excluded = true;
        alarm.repeat = RepeatDays::from_days([Weekday::Thu]);
        let settings = HolidaySettings {
            start: Some(date(2026, 9, 24)),
            end: Some(date(2026, 9, 28)),
        };
        let result = upcoming_holiday(&[alarm], &settings, at(2026, 9, 21, 6, 0)).unwrap();
        assert_eq!(result.days_until, 3);
        assert_eq!(result.affected_alarms, 1);
    }

    #[test]
    fn upcoming_holiday_counts_a_holiday_only_alarm_too() {
        let mut alarm = Alarm::new("Sleep in", NaiveTime::from_hms_opt(9, 0, 0).unwrap());
        alarm.holiday_only = true;
        alarm.repeat = RepeatDays::from_days([Weekday::Thu]);
        let settings = HolidaySettings {
            start: Some(date(2026, 9, 24)),
            end: Some(date(2026, 9, 28)),
        };
        let result = upcoming_holiday(&[alarm], &settings, at(2026, 9, 21, 6, 0)).unwrap();
        assert_eq!(result.affected_alarms, 1);
    }

    #[test]
    fn upcoming_holiday_none_when_nothing_affected_and_not_yet_tomorrow() {
        // Repeats Wednesday; the next occurrence (2026-09-23) is before the
        // window starts, and now is two days earlier.
        let mut alarm = Alarm::new("Work", NaiveTime::from_hms_opt(6, 0, 0).unwrap());
        alarm.holiday_excluded = true;
        alarm.repeat = RepeatDays::from_days([Weekday::Wed]);
        let settings = HolidaySettings {
            start: Some(date(2026, 9, 24)),
            end: Some(date(2026, 9, 28)),
        };
        assert!(upcoming_holiday(&[alarm], &settings, at(2026, 9, 21, 6, 0)).is_none());
    }

    #[test]
    fn upcoming_holiday_shows_the_day_before_even_with_no_affected_alarms() {
        let settings = HolidaySettings {
            start: Some(date(2026, 9, 24)),
            end: Some(date(2026, 9, 28)),
        };
        let result = upcoming_holiday(&[], &settings, at(2026, 9, 23, 6, 0)).unwrap();
        assert_eq!(result.days_until, 1);
        assert_eq!(result.affected_alarms, 0);
    }

    #[test]
    fn upcoming_holiday_ignores_disabled_alarms() {
        let mut alarm = Alarm::new("Work", NaiveTime::from_hms_opt(6, 0, 0).unwrap());
        alarm.holiday_excluded = true;
        alarm.enabled = false;
        alarm.repeat = RepeatDays::from_days([Weekday::Thu]);
        let settings = HolidaySettings {
            start: Some(date(2026, 9, 24)),
            end: Some(date(2026, 9, 28)),
        };
        // Two days out with nothing enabled affected: nothing to show yet.
        assert!(upcoming_holiday(&[alarm], &settings, at(2026, 9, 22, 6, 0)).is_none());
    }

    #[test]
    fn upcoming_holiday_counts_an_alarm_whose_next_hit_is_before_the_window_but_still_recurs_in_it() {
        // Repeats every weekday. The holiday runs 2026-09-28 (Mon) through
        // 2026-10-02 (Fri). Now is Monday 2026-09-21: the next occurrence is
        // tomorrow, far from the window, but the alarm rings on all five
        // window days.
        let mut alarm = Alarm::new("Work", NaiveTime::from_hms_opt(6, 0, 0).unwrap());
        alarm.holiday_excluded = true;
        alarm.repeat = RepeatDays::WEEKDAYS;
        let settings = HolidaySettings {
            start: Some(date(2026, 9, 28)),
            end: Some(date(2026, 10, 2)),
        };
        let now = at(2026, 9, 21, 6, 0);
        assert_eq!(
            next_trigger(&alarm, now).unwrap().date_naive(),
            date(2026, 9, 22),
            "sanity check: the next occurrence really is outside the window"
        );
        let result = upcoming_holiday(&[alarm], &settings, now).unwrap();
        assert_eq!(result.affected_alarms, 1);
    }

    #[test]
    fn upcoming_holiday_count_is_stable_across_the_run_up_to_the_holiday() {
        // Same alarm and window, checked three days later: the count must not
        // grow with time, since the affected alarms were fixed when the dates
        // were set.
        let mut alarm = Alarm::new("Work", NaiveTime::from_hms_opt(6, 0, 0).unwrap());
        alarm.holiday_excluded = true;
        alarm.repeat = RepeatDays::WEEKDAYS;
        let settings = HolidaySettings {
            start: Some(date(2026, 9, 28)),
            end: Some(date(2026, 10, 2)),
        };
        let earlier = upcoming_holiday(&[alarm.clone()], &settings, at(2026, 9, 21, 6, 0)).unwrap();
        let later = upcoming_holiday(&[alarm], &settings, at(2026, 9, 24, 6, 0)).unwrap();
        assert_eq!(earlier.affected_alarms, later.affected_alarms);
    }

    #[test]
    fn upcoming_holiday_none_once_the_holiday_has_started() {
        let mut alarm = Alarm::new("Work", NaiveTime::from_hms_opt(6, 0, 0).unwrap());
        alarm.holiday_excluded = true;
        alarm.repeat = RepeatDays::from_days([Weekday::Sat]); // 2026-09-26, inside the window
        let settings = HolidaySettings {
            start: Some(date(2026, 9, 24)),
            end: Some(date(2026, 9, 28)),
        };
        // Today is the window's start: the active-holiday banner applies
        // instead of the advance warning.
        assert!(upcoming_holiday(&[alarm], &settings, at(2026, 9, 24, 6, 0)).is_none());
    }

    #[test]
    fn format_upcoming_holiday_banner_pluralizes_days_and_alarms_independently() {
        assert_eq!(
            format_upcoming_holiday_banner(&UpcomingHoliday {
                days_until: 1,
                affected_alarms: 0
            }),
            "Holiday Mode in 1 day"
        );
        assert_eq!(
            format_upcoming_holiday_banner(&UpcomingHoliday {
                days_until: 1,
                affected_alarms: 1
            }),
            "Holiday Mode in 1 day; 1 alarm affected"
        );
        assert_eq!(
            format_upcoming_holiday_banner(&UpcomingHoliday {
                days_until: 3,
                affected_alarms: 2
            }),
            "Holiday Mode in 3 days; 2 alarms affected"
        );
    }
}
