use gio::prelude::*;

use crate::model::VibrationPattern;

const FEEDBACK_DEST: &str = "org.sigxcpu.Feedback";
const FEEDBACK_PATH: &str = "/org/sigxcpu/Feedback";
const FEEDBACK_HAPTIC_IFACE: &str = "org.sigxcpu.Feedback.Haptic";

/// Shortest pulse used. It is perceptible, if faint, on the FLX1S motor.
const MIN_PULSE_MS: u32 = 10;

/// One repeating on/off cycle for `pattern`, as `(is_on, duration_ms)` pairs.
///
/// `Constant` is a single always-on 100 ms chunk. Intensity sets the pulse length
/// inside each chunk, so it is a train of pulses that becomes continuous at
/// 100%.
fn pattern_shape(pattern: VibrationPattern) -> Vec<(bool, u32)> {
    match pattern {
        // 50ms/50ms blurs into a continuous buzz; 125ms/125ms is the
        // shortest gap that reads as distinct pulses.
        VibrationPattern::RapidFire => vec![(true, 125), (false, 125)],
        VibrationPattern::SlowRoll => vec![(true, 1000), (false, 2000)],
        // Closer than 125ms, the pulses do not land as distinct beats. 100ms
        // between the two beats, 600ms rest before the next "lub-dub".
        VibrationPattern::Heartbeat => vec![(true, 150), (false, 100), (true, 150), (false, 600)],
        VibrationPattern::Constant => vec![(true, 100)],
    }
}

/// The longest "on" segment of `shape`, which is the pulse length at 100%.
fn longest_on_ms(shape: &[(bool, u32)]) -> u32 {
    shape.iter().filter(|(on, _)| *on).map(|(_, d)| *d).max().unwrap_or(0)
}

/// Pulse length for `intensity` (0.0 to 1.0): `MIN_PULSE_MS` at the weakest,
/// `max_on_ms` at 100%. `0` for zero intensity.
///
/// The FLX1S motor is on/off only (its `brightness` node is ignored, tested),
/// so strength is the pulse length. Strength rises up to about 75 ms and
/// longer pulses feel continuous rather than stronger.
fn pulse_ms(intensity: f64, max_on_ms: u32) -> u32 {
    if intensity <= 0.0 {
        return 0;
    }
    let max_on = max_on_ms.max(MIN_PULSE_MS);
    let span = (max_on - MIN_PULSE_MS) as f64;
    (MIN_PULSE_MS as f64 + intensity.min(1.0) * span).round() as u32
}

/// Appends one shape segment of `segment_ms` at `intensity`: a pulse at the
/// start of the segment, then silence for the remainder. Magnitude `1.0` is
/// sent for pulses, since the strength is the pulse length.
fn push_segment(segments: &mut Vec<(f64, u32)>, intensity: f64, segment_ms: u32, max_on_ms: u32) {
    let pulse = pulse_ms(intensity, max_on_ms).min(segment_ms);
    if pulse > 0 {
        segments.push((1.0, pulse));
    }
    if segment_ms > pulse {
        segments.push((0.0, segment_ms - pulse));
    }
}

/// Builds the `Vibrate` segment list for a ringing alarm: `pattern`'s rhythm
/// repeated to fill `total_secs`. "On" segments are held at `end_percent`
/// (`ramping: false`), or climb from `start_percent` to `end_percent` over
/// `ramp_secs` and then hold (`ramping: true`). Each "on" segment becomes one
/// pulse followed by silence, and 100% is the pattern's longest "on" segment.
/// Magnitude `0.0` is off.
pub fn build_ring_segments(
    pattern: VibrationPattern,
    ramping: bool,
    start_percent: u8,
    end_percent: u8,
    ramp_secs: u32,
    total_secs: u32,
) -> Vec<(f64, u32)> {
    let shape = pattern_shape(pattern);
    let cycle_total_ms: u32 = shape.iter().map(|(_, d)| d).sum();
    if cycle_total_ms == 0 || total_secs == 0 {
        return Vec::new();
    }
    let max_on_ms = longest_on_ms(&shape);

    let total_ms = total_secs * 1000;
    let ramp_ms = ramp_secs * 1000;
    let mut segments = Vec::new();
    let mut elapsed_ms: u32 = 0;
    'fill: loop {
        for &(is_on, duration) in &shape {
            if elapsed_ms >= total_ms {
                break 'fill;
            }
            let this_duration = duration.min(total_ms - elapsed_ms);
            if !is_on {
                segments.push((0.0, this_duration));
            } else {
                let intensity = if !ramping || elapsed_ms >= ramp_ms {
                    end_percent as f64 / 100.0
                } else {
                    let fraction = elapsed_ms as f64 / ramp_ms as f64;
                    let percent =
                        start_percent as f64 + (end_percent as f64 - start_percent as f64) * fraction;
                    percent / 100.0
                };
                push_segment(&mut segments, intensity, this_duration, max_on_ms);
            }
            elapsed_ms += this_duration;
        }
    }
    segments
}

/// Builds the segment list for previewing `pattern` in the alarm form: no
/// ramping, at 100%, so each beat plays at its full length.
///
/// The cycle loops enough times to fill roughly `PREVIEW_TARGET_MS`, with at
/// least one loop even if one cycle exceeds the target, since a long cycle
/// needs to be heard in full. Short cycles are capped at `MAX_LOOPS` loops, or
/// `PREVIEW_MIN_MS` of playback if that allows more. Stop works throughout via
/// `play_segments`'s per-segment check.
pub fn build_preview_segments(pattern: VibrationPattern) -> Vec<(f64, u32)> {
    const PREVIEW_TARGET_MS: u32 = 10_000;
    const PREVIEW_MIN_MS: u32 = 2_500;
    const MAX_LOOPS: u32 = 10;

    let shape = pattern_shape(pattern);
    let max_on_ms = longest_on_ms(&shape);
    let cycle_ms: u32 = shape.iter().map(|(_, d)| d).sum();
    let loops = PREVIEW_TARGET_MS.checked_div(cycle_ms).map_or(1, |fit| {
        fit.clamp(1, MAX_LOOPS.max(PREVIEW_MIN_MS.checked_div(cycle_ms).unwrap_or(0)))
    });

    let mut segments = Vec::new();
    for _ in 0..loops {
        for &(is_on, duration) in &shape {
            if is_on {
                push_segment(&mut segments, 1.0, duration, max_on_ms);
            } else {
                segments.push((0.0, duration));
            }
        }
    }
    segments
}

/// Triggers a custom vibration pattern through feedbackd's Haptic interface,
/// bypassing the haptic feedback the theme assigns to `"alarm-clock-elapsed"`.
/// It plays in addition to the sound triggered through
/// [`crate::feedback::FeedbackSession`].
///
/// Equivalent `busctl` call: `org.sigxcpu.Feedback.Haptic Vibrate 'sa(du)'
/// <app_id> <count> <magnitude> <duration_ms> ...`.
pub async fn trigger_vibration(
    conn: &gio::DBusConnection,
    pattern: &[(f64, u32)],
) -> anyhow::Result<bool> {
    if pattern.is_empty() {
        return Ok(true);
    }
    let proxy = gio::DBusProxy::new_future(
        conn,
        gio::DBusProxyFlags::NONE,
        None,
        Some(FEEDBACK_DEST),
        FEEDBACK_PATH,
        FEEDBACK_HAPTIC_IFACE,
    )
    .await
    .map_err(|err| anyhow::anyhow!("failed to build feedbackd Haptic proxy: {err}"))?;

    let args = (crate::APP_ID, pattern).to_variant();
    let reply = proxy
        .call_future("Vibrate", Some(&args), gio::DBusCallFlags::NONE, -1)
        .await
        .map_err(|err| anyhow::anyhow!("Vibrate call failed: {err}"))?;
    Ok(reply.get::<(bool,)>().map(|(accepted,)| accepted).unwrap_or(true))
}

/// Plays `segments` (magnitude and duration_ms pairs, alternating on and off;
/// `0.0` is off) with one `Vibrate` call per "on" segment and client-side
/// waits between them.
///
/// A single `Vibrate` call with the whole sequence played as one continuous
/// ~15-second buzz on the FLX1S (tested with a 12-segment Slow Roll pattern
/// over `busctl`), with no difference between on and off segments. Sending
/// nothing for "off" periods makes them silent.
///
/// `should_continue` is checked between segments, so Stop can interrupt
/// mid-pattern.
pub async fn play_segments(
    conn: &gio::DBusConnection,
    segments: &[(f64, u32)],
    should_continue: &std::rc::Rc<std::cell::Cell<bool>>,
) {
    let mut warned_rejected = false;
    for &(magnitude, duration_ms) in segments {
        if !should_continue.get() {
            return;
        }
        if magnitude > 0.0 {
            match trigger_vibration(conn, &[(magnitude, duration_ms)]).await {
                Ok(true) => {}
                Ok(false) => {
                    if !warned_rejected {
                        warned_rejected = true;
                        tracing::warn!("feedbackd rejected Vibrate (Silent profile?)");
                    }
                }
                Err(err) => tracing::warn!(%err, "failed to trigger vibration segment"),
            }
        }
        glib::timeout_future(std::time::Duration::from_millis(duration_ms as u64)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn total_ms(segments: &[(f64, u32)]) -> u32 {
        segments.iter().map(|(_, d)| d).sum()
    }

    fn pulses(segments: &[(f64, u32)]) -> Vec<u32> {
        segments.iter().filter(|(m, _)| *m > 0.0).map(|(_, d)| *d).collect()
    }

    #[test]
    fn pulse_length_runs_from_the_floor_to_the_longest_beat() {
        assert_eq!(pulse_ms(0.0, 150), 0);
        assert_eq!(pulse_ms(0.001, 150), 10);
        assert_eq!(pulse_ms(1.0, 150), 150);
        assert_eq!(pulse_ms(0.5, 150), 80);
        assert_eq!(pulse_ms(2.0, 150), 150);
    }

    #[test]
    fn constant_ramp_climbs_from_a_tick_to_continuous() {
        let segments = build_ring_segments(VibrationPattern::Constant, true, 1, 100, 20, 25);
        let lengths = pulses(&segments);
        assert!(lengths.first().is_some_and(|l| (10..=11).contains(l)));
        assert_eq!(lengths.last(), Some(&100));
        assert!(lengths.windows(2).all(|pair| pair[1] >= pair[0]));
        assert_eq!(total_ms(&segments), 25_000);
        // The last chunk has no silent remainder.
        assert_eq!(segments.last(), Some(&(1.0, 100)));
    }

    #[test]
    fn constant_non_ramping_holds_flat() {
        let segments = build_ring_segments(VibrationPattern::Constant, false, 20, 60, 30, 5);
        assert!(pulses(&segments).iter().all(|l| *l == 64));
        assert_eq!(total_ms(&segments), 5000);
    }

    #[test]
    fn heartbeat_at_full_intensity_plays_the_pattern_as_designed() {
        let segments = build_ring_segments(VibrationPattern::Heartbeat, false, 0, 100, 0, 1);
        assert_eq!(segments, vec![(1.0, 150), (0.0, 100), (1.0, 150), (0.0, 600)]);
    }

    #[test]
    fn heartbeat_keeps_one_pulse_per_beat_at_low_intensity() {
        let segments = build_ring_segments(VibrationPattern::Heartbeat, false, 0, 1, 0, 1);
        let lengths = pulses(&segments);
        assert_eq!(lengths.len(), 2);
        assert!(lengths.iter().all(|l| *l == 11));
        assert_eq!(total_ms(&segments), 1000);
    }

    #[test]
    fn zero_intensity_is_silent() {
        let segments = build_ring_segments(VibrationPattern::RapidFire, false, 0, 0, 0, 2);
        assert!(segments.iter().all(|(m, _)| *m == 0.0));
        assert_eq!(total_ms(&segments), 2000);
    }

    #[test]
    fn every_pattern_fills_the_requested_time_exactly() {
        for pattern in [
            VibrationPattern::RapidFire,
            VibrationPattern::SlowRoll,
            VibrationPattern::Heartbeat,
            VibrationPattern::Constant,
        ] {
            for end in [1, 50, 100] {
                let segments = build_ring_segments(pattern, true, 1, end, 3, 7);
                assert_eq!(total_ms(&segments), 7000, "{pattern:?} at {end}%");
            }
        }
    }

    #[test]
    fn rapid_fire_is_a_simple_equal_on_off_cycle() {
        // 125ms each way is the shortest gap that reads as distinct pulses;
        // 50/50ms blurred into one buzz.
        assert_eq!(pattern_shape(VibrationPattern::RapidFire), vec![(true, 125), (false, 125)]);
    }

    #[test]
    fn constant_is_a_single_always_on_chunk() {
        assert_eq!(pattern_shape(VibrationPattern::Constant), vec![(true, 100)]);
    }

    #[test]
    fn preview_loops_pattern_ten_times_at_full_length() {
        // Heartbeat's 1000ms cycle divides evenly into the 10s preview target.
        let preview = build_preview_segments(VibrationPattern::Heartbeat);
        assert_eq!(total_ms(&preview), 10_000);
        assert_eq!(pulses(&preview).len(), 20);
        assert!(pulses(&preview).iter().all(|l| *l == 150));
    }

    #[test]
    fn preview_of_a_long_cycle_plays_at_least_once() {
        // Slow Roll's cycle is 3 seconds, so three loops fill the target.
        let preview = build_preview_segments(VibrationPattern::SlowRoll);
        assert_eq!(total_ms(&preview), 9_000);
    }

    #[test]
    fn preview_of_constant_is_one_continuous_hold() {
        let preview = build_preview_segments(VibrationPattern::Constant);
        assert!(preview.iter().all(|(m, _)| *m > 0.0));
        assert_eq!(total_ms(&preview), 2500);
    }
}
