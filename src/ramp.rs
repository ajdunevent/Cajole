use std::time::Duration;

/// Number of steps for a ramp from `start` to `end` over `duration_secs`.
/// Capped at one step per percentage point (consumers work in integer percent)
/// and at one step per second of duration. Direction-agnostic.
fn step_count(start: u8, end: u8, duration_secs: u32) -> u32 {
    let range_abs = (end as i32 - start as i32).unsigned_abs();
    range_abs.min(duration_secs.max(1)).max(1)
}

/// The percentages a ramp steps through, from `start` to `end` inclusive.
/// Pure, so the stepping is unit-tested independently of what it drives.
pub fn steps(start: u8, end: u8, duration_secs: u32) -> Vec<u8> {
    if start == end || duration_secs == 0 {
        return vec![end];
    }
    let count = step_count(start, end, duration_secs);
    let range = end as f64 - start as f64;
    (0..=count)
        .map(|i| {
            let frac = i as f64 / count as f64;
            (start as f64 + frac * range).round() as u8
        })
        .collect()
}

/// Wait between consecutive [`steps`] values so the sequence takes
/// `duration_secs`.
pub fn step_interval(start: u8, end: u8, duration_secs: u32) -> Duration {
    if start == end || duration_secs == 0 {
        return Duration::ZERO;
    }
    let count = step_count(start, end, duration_secs);
    Duration::from_secs_f64(duration_secs as f64 / count as f64)
}

/// A ramp paced by both step size and step time: at most one step per
/// percentage point and at most one per `min_interval`. The sequence spans
/// `duration_secs`, so a shorter ramp takes larger steps instead of
/// running long. Returns the values and the wait between them.
pub fn paced(
    start: u8,
    end: u8,
    duration_secs: u32,
    min_interval: Duration,
) -> (Vec<u8>, Duration) {
    if start == end || duration_secs == 0 {
        return (vec![end], Duration::ZERO);
    }
    let range = (end as i32 - start as i32).unsigned_abs();
    let by_time = (duration_secs as f64 / min_interval.as_secs_f64().max(f64::EPSILON)) as u32;
    let count = range.min(by_time).max(1);
    let span = end as f64 - start as f64;
    let values = (0..=count)
        .map(|i| (start as f64 + i as f64 / count as f64 * span).round() as u8)
        .collect();
    (values, Duration::from_secs_f64(duration_secs as f64 / count as f64))
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUARTER_SECOND: Duration = Duration::from_millis(250);

    #[test]
    fn paced_wide_range_steps_one_percent_at_a_time() {
        let (values, interval) = paced(10, 100, 30, QUARTER_SECOND);
        assert_eq!(values.len(), 91);
        assert!(values.windows(2).all(|pair| pair[1] == pair[0] + 1));
        assert_eq!(interval, Duration::from_secs_f64(30.0 / 90.0));
        assert!(interval >= QUARTER_SECOND);
    }

    #[test]
    fn paced_short_duration_takes_larger_steps_not_more_time() {
        let (values, interval) = paced(10, 100, 10, QUARTER_SECOND);
        assert_eq!(values.len(), 41);
        assert_eq!(values.first(), Some(&10));
        assert_eq!(values.last(), Some(&100));
        assert_eq!(interval, QUARTER_SECOND);
        let total = interval.as_secs_f64() * (values.len() - 1) as f64;
        assert!((total - 10.0).abs() < 1e-9);
    }

    #[test]
    fn paced_descends_and_handles_degenerate_input() {
        let (values, _) = paced(100, 10, 10, QUARTER_SECOND);
        assert_eq!(values.first(), Some(&100));
        assert_eq!(values.last(), Some(&10));
        assert_eq!(paced(60, 60, 20, QUARTER_SECOND), (vec![60], Duration::ZERO));
        assert_eq!(paced(5, 85, 0, QUARTER_SECOND), (vec![85], Duration::ZERO));
    }

    #[test]
    fn ascending_ramp_ends_exactly_at_end() {
        let s = steps(5, 85, 20);
        assert_eq!(s.first(), Some(&5));
        assert_eq!(s.last(), Some(&85));
        for pair in s.windows(2) {
            assert!(pair[1] > pair[0]);
        }
    }

    #[test]
    fn descending_ramp_works_the_same_direction_agnostically() {
        let s = steps(90, 10, 20);
        assert_eq!(s.first(), Some(&90));
        assert_eq!(s.last(), Some(&10));
        for pair in s.windows(2) {
            assert!(pair[1] < pair[0]);
        }
    }

    #[test]
    fn flat_range_is_a_single_step() {
        assert_eq!(steps(60, 60, 20), vec![60]);
        assert_eq!(step_interval(60, 60, 20), Duration::ZERO);
    }

    #[test]
    fn zero_duration_is_a_single_step_at_end() {
        assert_eq!(steps(5, 85, 0), vec![85]);
        assert_eq!(step_interval(5, 85, 0), Duration::ZERO);
    }

    #[test]
    fn large_range_short_duration_caps_at_one_step_per_second() {
        let s = steps(5, 85, 5);
        assert_eq!(s.len(), 6); // 5 intervals -> 6 values
        assert_eq!(step_interval(5, 85, 5), Duration::from_secs(1));
    }

    #[test]
    fn small_range_long_duration_does_not_subdivide_below_one_percent() {
        let s = steps(50, 55, 60);
        assert_eq!(s, vec![50, 51, 52, 53, 54, 55]);
        assert_eq!(step_interval(50, 55, 60), Duration::from_secs(12));
    }
}
