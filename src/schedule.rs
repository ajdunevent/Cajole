use std::collections::HashMap;

use chrono::{DateTime, Datelike, Days, Local, LocalResult, NaiveDate, NaiveTime, TimeZone, Weekday};
use uuid::Uuid;

use crate::model::Alarm;

/// Resolves a wall-clock (date, time) pair to a `DateTime<Local>`, handling two
/// DST edge cases:
///
/// - Spring-forward gap (2:30 AM does not exist, `LocalResult::None`): resolves
///   forward to the nearest time that exists (2:30 becomes 3:30).
/// - Fall-back overlap (1:30 AM happens twice, `LocalResult::Ambiguous`): takes
///   the earlier occurrence, the first opportunity for the alarm to go off.
fn resolve_local(date: NaiveDate, time: NaiveTime) -> DateTime<Local> {
    let naive = date.and_time(time);
    match Local.from_local_datetime(&naive) {
        LocalResult::Single(dt) => dt,
        LocalResult::Ambiguous(earlier, _later) => earlier,
        LocalResult::None => {
            // Walks forward in small steps until a valid local time is found.
            // An hour covers every DST transition; minute steps avoid
            // overshooting into the next slot on unusual offsets.
            let mut probe = naive;
            for _ in 0..120 {
                probe += chrono::Duration::minutes(1);
                if let LocalResult::Single(dt) = Local.from_local_datetime(&probe) {
                    return dt;
                }
            }
            // Unreachable in practice; falls back to a UTC-anchored
            // interpretation instead of panicking on an alarm.
            Local.from_utc_datetime(&naive)
        }
    }
}

/// Computes the next time the alarm should ring, strictly after `now`. Returns
/// `None` only if the alarm is disabled.
pub fn next_trigger(alarm: &Alarm, now: DateTime<Local>) -> Option<DateTime<Local>> {
    if !alarm.enabled {
        return None;
    }

    // A pending snooze overrides the normal schedule: it is an extra near-term
    // fire inserted ahead of the next regular one. It is returned as-is even
    // when already past, so `TriggerTracker::poll` sees it as immediately due.
    if let Some(snoozed) = alarm.snoozed_until {
        return Some(snoozed);
    }

    if alarm.repeat.is_empty() {
        // One-time alarm: today if still ahead, else tomorrow.
        let today = resolve_local(now.date_naive(), alarm.time);
        return Some(if today > now {
            today
        } else {
            resolve_local(now.date_naive() + Days::new(1), alarm.time)
        });
    }

    // Repeating alarm: scans up to 8 days (a week plus one), which covers a
    // slot that passed today with the next match exactly a week out.
    for offset in 0..=7u64 {
        let candidate_date = now.date_naive() + Days::new(offset);
        let weekday: Weekday = candidate_date.weekday();
        if !alarm.repeat.contains(weekday) {
            continue;
        }
        // An early-dismissed occurrence's date and anything before it are
        // skipped, moving to the next occurrence.
        if let Some(skip_through) = alarm.skip_through {
            if candidate_date <= skip_through {
                continue;
            }
        }
        let candidate = resolve_local(candidate_date, alarm.time);
        if offset == 0 && candidate <= now {
            continue;
        }
        return Some(candidate);
    }

    // Unreachable: `repeat` is non-empty and the loop covers a full week.
    None
}

/// A "how long until then" string matching GNOME Clocks' "Alarm set for {X} from
/// now" toast, ported from `show_ring_time_toast` in `alarm-face.vala` (with
/// `ceil_time_span` and `format_time_span` in `utils.vala`/`cutils.c`).
///
/// - Rounds up to the next whole minute first (`ceil_time_span(diff, MINUTE)`),
///   so 23.2 minutes does not read as "23 minutes".
/// - Shows the largest unit that fits (days, hours or minutes), rounded to the
///   nearest whole number, with no compound breakdown. The app has no
///   localization infrastructure to match Clocks' ICU duration formatting.
pub fn format_time_until(now: DateTime<Local>, target: DateTime<Local>) -> String {
    let raw_secs = (target - now).num_seconds().max(0);
    let total_minutes = (raw_secs + 59) / 60;

    if total_minutes >= 1440 {
        let days = ((total_minutes as f64) / 1440.0).round().max(1.0) as i64;
        format!("{days} {}", if days == 1 { "day" } else { "days" })
    } else if total_minutes >= 60 {
        let hours = ((total_minutes as f64) / 60.0).round().max(1.0) as i64;
        format!("{hours} {}", if hours == 1 { "hour" } else { "hours" })
    } else {
        let minutes = total_minutes.max(1);
        format!("{minutes} {}", if minutes == 1 { "minute" } else { "minutes" })
    }
}

/// Detects when a polling loop should treat an alarm as firing now. Comparing
/// `next_trigger` against `now` on each poll misses the firing, because the
/// instant ring time passes `next_trigger` reports the following occurrence.
///
/// The tracker remembers the last computed next trigger per alarm and fires
/// when `now` reaches it, then stores the following one.
#[derive(Default)]
pub struct TriggerTracker {
    next: HashMap<Uuid, DateTime<Local>>,
}

impl TriggerTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drops an alarm from the cache so the next [`TriggerTracker::sync`]
    /// recomputes its next trigger.
    ///
    /// Used when snoozing: `sync` computes a trigger only for ids it has not
    /// seen, so setting `snoozed_until` on a tracked alarm would otherwise not
    /// be picked up.
    pub fn forget(&mut self, id: Uuid) {
        self.next.remove(&id);
    }

    /// Aligns the tracker with the current alarm set: starts tracking new
    /// alarms (computing their first trigger) and stops tracking removed or
    /// disabled ones.
    pub fn sync(&mut self, alarms: &[Alarm], now: DateTime<Local>) {
        let live_ids: std::collections::HashSet<Uuid> = alarms.iter().map(|a| a.id).collect();
        self.next.retain(|id, _| live_ids.contains(id));

        for alarm in alarms {
            if !alarm.enabled {
                self.next.remove(&alarm.id);
                continue;
            }
            self.next.entry(alarm.id).or_insert_with(|| {
                // An enabled alarm should always have a next trigger. If none
                // can be computed, it is parked far enough in the future that
                // it does not fire spuriously on the next poll.
                next_trigger(alarm, now).unwrap_or_else(|| now + chrono::Duration::days(3650))
            });
        }
    }

    /// Checks every tracked alarm against `now`. Returns the ids whose
    /// remembered trigger has arrived and reschedules each to its following
    /// occurrence.
    pub fn poll(&mut self, alarms: &[Alarm], now: DateTime<Local>) -> Vec<Uuid> {
        let mut fired = Vec::new();

        for alarm in alarms {
            if !alarm.enabled {
                continue;
            }
            let Some(&scheduled) = self.next.get(&alarm.id) else {
                continue;
            };
            if now >= scheduled {
                fired.push(alarm.id);
                if let Some(next) = next_trigger(alarm, now) {
                    self.next.insert(alarm.id, next);
                } else {
                    self.next.remove(&alarm.id);
                }
            }
        }

        fired
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RepeatDays;
    use chrono::TimeZone;

    fn at(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(y, m, d, h, min, 0).unwrap()
    }

    #[test]
    fn one_time_alarm_later_today() {
        let alarm = Alarm::new("Test", NaiveTime::from_hms_opt(9, 0, 0).unwrap());
        let now = at(2026, 8, 22, 7, 0);
        let next = next_trigger(&alarm, now).unwrap();
        assert_eq!(next, at(2026, 8, 22, 9, 0));
    }

    #[test]
    fn one_time_alarm_already_passed_rolls_to_tomorrow() {
        let alarm = Alarm::new("Test", NaiveTime::from_hms_opt(9, 0, 0).unwrap());
        let now = at(2026, 8, 22, 10, 0);
        let next = next_trigger(&alarm, now).unwrap();
        assert_eq!(next, at(2026, 8, 23, 9, 0));
    }

    #[test]
    fn disabled_alarm_has_no_next_trigger() {
        let mut alarm = Alarm::new("Test", NaiveTime::from_hms_opt(9, 0, 0).unwrap());
        alarm.enabled = false;
        assert_eq!(next_trigger(&alarm, at(2026, 8, 22, 7, 0)), None);
    }

    #[test]
    fn weekly_repeat_picks_next_matching_day_this_week() {
        // 2026-08-22 is a Saturday.
        let mut alarm = Alarm::new("Weekdays", NaiveTime::from_hms_opt(7, 30, 0).unwrap());
        alarm.repeat = RepeatDays::WEEKDAYS;
        let now = at(2026, 8, 22, 12, 0); // Saturday noon
        let next = next_trigger(&alarm, now).unwrap();
        // Next weekday is Monday 2026-08-24.
        assert_eq!(next, at(2026, 8, 24, 7, 30));
        assert_eq!(next.weekday(), Weekday::Mon);
    }

    #[test]
    fn weekly_repeat_same_day_before_ring_time_fires_today() {
        let mut alarm = Alarm::new("Weekdays", NaiveTime::from_hms_opt(7, 30, 0).unwrap());
        alarm.repeat = RepeatDays::WEEKDAYS;
        // 2026-08-24 is a Monday; asked at 6 AM, before ring time.
        let now = at(2026, 8, 24, 6, 0);
        let next = next_trigger(&alarm, now).unwrap();
        assert_eq!(next, at(2026, 8, 24, 7, 30));
    }

    #[test]
    fn weekly_repeat_same_day_after_ring_time_rolls_to_next_week() {
        let mut alarm = Alarm::new("Mondays only", NaiveTime::from_hms_opt(7, 30, 0).unwrap());
        alarm.repeat = RepeatDays::from_days([Weekday::Mon]);
        // Monday 2026-08-24, 8 AM: ring time has passed.
        let now = at(2026, 8, 24, 8, 0);
        let next = next_trigger(&alarm, now).unwrap();
        assert_eq!(next, at(2026, 8, 31, 7, 30));
        assert_eq!(next.weekday(), Weekday::Mon);
    }

    #[test]
    fn single_day_repeat_next_occurrence_is_exactly_one_week_out() {
        let mut alarm = Alarm::new("Sundays", NaiveTime::from_hms_opt(10, 0, 0).unwrap());
        alarm.repeat = RepeatDays::from_days([Weekday::Sun]);
        // 2026-08-23 is a Sunday; asked just after ring time.
        let now = at(2026, 8, 23, 10, 1);
        let next = next_trigger(&alarm, now).unwrap();
        assert_eq!(next, at(2026, 8, 30, 10, 0));
    }

    #[test]
    fn tracker_catches_a_fire_that_naive_polling_would_miss() {
        // If a poll loop called `next_trigger` fresh each cycle, it would
        // never see this alarm due, because `next_trigger` has already rolled
        // forward to tomorrow.
        let alarm = Alarm::new("Test", NaiveTime::from_hms_opt(7, 0, 0).unwrap());
        let mut tracker = TriggerTracker::new();

        let before = at(2026, 8, 22, 6, 59);
        tracker.sync(std::slice::from_ref(&alarm), before);
        assert!(tracker.poll(std::slice::from_ref(&alarm), before).is_empty());

        // The poll lands 10 seconds after ring time, as a 20s poll loop could.
        let after = at(2026, 8, 22, 7, 0) + chrono::Duration::seconds(10);
        let fired = tracker.poll(std::slice::from_ref(&alarm), after);
        assert_eq!(fired, vec![alarm.id]);

        // It does not fire again on the next poll.
        let again = after + chrono::Duration::seconds(20);
        assert!(tracker.poll(std::slice::from_ref(&alarm), again).is_empty());
    }

    #[test]
    fn tracker_stops_tracking_disabled_or_removed_alarms() {
        let mut alarm = Alarm::new("Test", NaiveTime::from_hms_opt(7, 0, 0).unwrap());
        let mut tracker = TriggerTracker::new();
        let now = at(2026, 8, 22, 6, 0);

        tracker.sync(std::slice::from_ref(&alarm), now);
        assert_eq!(tracker.next.len(), 1);

        alarm.enabled = false;
        tracker.sync(std::slice::from_ref(&alarm), now);
        assert!(tracker.next.is_empty());

        // Re-enabling and syncing picks it back up.
        alarm.enabled = true;
        tracker.sync(std::slice::from_ref(&alarm), now);
        assert_eq!(tracker.next.len(), 1);

        // Removing it from the slice drops it.
        tracker.sync(&[], now);
        assert!(tracker.next.is_empty());
    }

    #[test]
    fn snooze_overrides_repeat_schedule_even_when_sooner_would_normally_roll_to_next_week() {
        let mut alarm = Alarm::new("Weekdays", NaiveTime::from_hms_opt(7, 0, 0).unwrap());
        alarm.repeat = RepeatDays::from_days([Weekday::Mon]);
        // 2026-08-24 is a Monday past ring time; normal scheduling would roll
        // to next Monday.
        let now = at(2026, 8, 24, 8, 0);
        let snooze_target = now + chrono::Duration::minutes(9);
        alarm.snoozed_until = Some(snooze_target);

        assert_eq!(next_trigger(&alarm, now), Some(snooze_target));
    }

    #[test]
    fn snooze_in_the_past_is_still_returned_so_it_reads_as_immediately_due() {
        let mut alarm = Alarm::new("Test", NaiveTime::from_hms_opt(7, 0, 0).unwrap());
        let now = at(2026, 8, 22, 8, 0);
        let overdue_snooze = now - chrono::Duration::minutes(1);
        alarm.snoozed_until = Some(overdue_snooze);

        assert_eq!(next_trigger(&alarm, now), Some(overdue_snooze));
    }

    #[test]
    fn disabled_alarm_ignores_a_stale_snooze() {
        let mut alarm = Alarm::new("Test", NaiveTime::from_hms_opt(7, 0, 0).unwrap());
        alarm.enabled = false;
        alarm.snoozed_until = Some(at(2026, 8, 22, 8, 0));
        assert_eq!(next_trigger(&alarm, at(2026, 8, 22, 7, 0)), None);
    }

    #[test]
    fn forget_causes_the_next_sync_to_pick_up_a_new_snooze() {
        let mut alarm = Alarm::new("Test", NaiveTime::from_hms_opt(7, 0, 0).unwrap());
        let now = at(2026, 8, 22, 8, 0);
        let mut tracker = TriggerTracker::new();

        // Initial sync caches tomorrow's 7:00 occurrence.
        tracker.sync(std::slice::from_ref(&alarm), now);
        let cached_before = *tracker.next.get(&alarm.id).unwrap();
        assert_eq!(cached_before, at(2026, 8, 23, 7, 0));

        // Writing a snooze to the store without forgetting: sync leaves the
        // stale cached value alone, which is what `forget` overrides.
        let snooze_target = now + chrono::Duration::minutes(9);
        alarm.snoozed_until = Some(snooze_target);
        tracker.sync(std::slice::from_ref(&alarm), now);
        assert_eq!(*tracker.next.get(&alarm.id).unwrap(), cached_before);

        // After forgetting, sync recomputes and picks up the snooze.
        tracker.forget(alarm.id);
        tracker.sync(std::slice::from_ref(&alarm), now);
        assert_eq!(*tracker.next.get(&alarm.id).unwrap(), snooze_target);
    }

    #[test]
    fn format_time_until_shows_minutes_under_an_hour() {
        let now = at(2026, 8, 22, 6, 0);
        assert_eq!(format_time_until(now, now + chrono::Duration::minutes(23)), "23 minutes");
        assert_eq!(format_time_until(now, now + chrono::Duration::minutes(1)), "1 minute");
    }

    #[test]
    fn format_time_until_rounds_up_to_the_next_minute() {
        let now = at(2026, 8, 22, 6, 0);
        // 22 minutes 10 seconds rounds up to 23, not down to 22.
        let target = now + chrono::Duration::minutes(22) + chrono::Duration::seconds(10);
        assert_eq!(format_time_until(now, target), "23 minutes");
    }

    #[test]
    fn format_time_until_shows_hours_at_an_hour_or_more() {
        let now = at(2026, 8, 22, 6, 0);
        assert_eq!(format_time_until(now, now + chrono::Duration::hours(1)), "1 hour");
        assert_eq!(
            format_time_until(now, now + chrono::Duration::hours(3) + chrono::Duration::minutes(10)),
            "3 hours"
        );
        // 3 hours 40 minutes rounds to the nearest hour (4), not down to 3.
        assert_eq!(
            format_time_until(now, now + chrono::Duration::hours(3) + chrono::Duration::minutes(40)),
            "4 hours"
        );
    }

    #[test]
    fn format_time_until_shows_days_at_a_day_or_more() {
        let now = at(2026, 8, 22, 6, 0);
        assert_eq!(format_time_until(now, now + chrono::Duration::hours(24)), "1 day");
        assert_eq!(format_time_until(now, now + chrono::Duration::hours(50)), "2 days");
    }

    #[test]
    fn format_time_until_never_shows_zero_or_negative() {
        let now = at(2026, 8, 22, 6, 0);
        // A target at or before now floors to at least 1 minute, never "0
        // minutes" or a negative string.
        assert_eq!(format_time_until(now, now), "1 minute");
        assert_eq!(format_time_until(now, now - chrono::Duration::minutes(5)), "1 minute");
    }

    #[test]
    fn skip_through_finds_the_next_match_after_today_even_if_today_matches() {
        // 2026-08-22 is a Saturday. Repeats Saturday and Tuesday; today
        // matches but is skipped, giving Tuesday (Aug 25).
        let now = at(2026, 8, 22, 6, 0);
        let mut alarm = Alarm::new("Test", NaiveTime::from_hms_opt(7, 0, 0).unwrap());
        alarm.repeat = RepeatDays::from_days([Weekday::Sat, Weekday::Tue]);
        alarm.skip_through = Some(now.date_naive());
        let next = next_trigger(&alarm, now).unwrap();
        assert_eq!(next, at(2026, 8, 25, 7, 0));
    }

    #[test]
    fn skip_through_wraps_a_full_week_if_today_is_the_only_match_day() {
        // Repeats on Saturday only: skipping today leaves the next occurrence
        // a full week out.
        let now = at(2026, 8, 22, 6, 0);
        let mut alarm = Alarm::new("Test", NaiveTime::from_hms_opt(7, 0, 0).unwrap());
        alarm.repeat = RepeatDays::from_days([Weekday::Sat]);
        alarm.skip_through = Some(now.date_naive());
        let next = next_trigger(&alarm, now).unwrap();
        assert_eq!(next, at(2026, 8, 29, 7, 0));
    }

    #[test]
    fn skip_through_has_no_effect_once_the_date_has_passed() {
        // A skip date in the past is inert: today fires normally, and nothing
        // needs to clear the field.
        let now = at(2026, 8, 22, 6, 0);
        let mut alarm = Alarm::new("Test", NaiveTime::from_hms_opt(7, 0, 0).unwrap());
        alarm.repeat = RepeatDays::from_days([Weekday::Sat]);
        alarm.skip_through = Some(now.date_naive() - chrono::Duration::days(7));
        let next = next_trigger(&alarm, now).unwrap();
        assert_eq!(next, at(2026, 8, 22, 7, 0));
    }

    #[test]
    fn editing_the_time_after_an_early_dismissal_skip_respects_the_new_time() {
        // Regression: an upcoming occurrence of a repeating alarm is dismissed
        // early (setting `skip_through`), then the alarm's time is edited
        // before the skipped day passes. The next occurrence must use the new
        // time; a stored `DateTime` target would keep the old one.
        let now = at(2026, 8, 22, 6, 0);
        let mut alarm = Alarm::new("Test", NaiveTime::from_hms_opt(7, 0, 0).unwrap());
        alarm.repeat = RepeatDays::from_days([Weekday::Sat]);
        alarm.skip_through = Some(now.date_naive());

        // The edit: the time changes from 7am to 8am after the skip was
        // recorded.
        alarm.time = NaiveTime::from_hms_opt(8, 0, 0).unwrap();

        let next = next_trigger(&alarm, now).unwrap();
        assert_eq!(next, at(2026, 8, 29, 8, 0));
    }
}
