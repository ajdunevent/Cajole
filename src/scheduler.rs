use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use chrono::{DateTime, Local};
use uuid::Uuid;

use crate::wake_timer::WallClockTimer;
use crate::{next_trigger, AlarmStore, RingSession, TriggerTracker};

/// A ringing alarm's session plus the bookkeeping to auto-stop it if nobody
/// dismisses or snoozes it.
pub struct RingingEntry {
    session: RingSession,
    started_at: DateTime<Local>,
    duration_secs: u32,
}

/// Currently ringing alarms, shared between the polling loop (inserts,
/// auto-expires) and UI handlers (remove, stop). `Rc<RefCell<>>` suffices:
/// everything runs on the single `glib::MainContext`.
pub type RingingMap = Rc<RefCell<HashMap<Uuid, RingingEntry>>>;

/// The poll loop's trigger cache, shared with [`snooze`] so it can invalidate
/// an alarm's cached next trigger when `snoozed_until` changes; see
/// [`TriggerTracker::forget`].
pub type SharedTracker = Rc<RefCell<TriggerTracker>>;

/// Something the scheduler wants the UI to react to.
#[derive(Debug, Clone)]
pub enum SchedulerEvent {
    /// An alarm started ringing: the UI should show the ring screen.
    Ringing { alarm_id: Uuid, label: String },
    /// An alarm reached its `alarm_duration_minutes` and stopped on its own.
    /// The UI should close its ring screen if open.
    AutoStopped { alarm_id: Uuid },
    /// An enabled alarm's next occurrence is within its
    /// `early_dismissal_window_minutes`: the UI should offer a way to dismiss
    /// it before it rings. Fired once per occurrence, not every poll.
    ///
    /// It carries the trigger time instead of a precomputed "minutes until":
    /// `schedule::format_time_until` rounds from two timestamps, and an
    /// intermediate truncated integer would reintroduce the precision bug the
    /// window check was fixed for (see `run`).
    UpcomingAlarm {
        alarm_id: Uuid,
        label: String,
        next_trigger: DateTime<Local>,
    },
    /// A previously fired `UpcomingAlarm` for this alarm is no longer valid:
    /// the UI should withdraw its notification. Fired whenever the alarm stops
    /// being upcoming (it rang, was disabled or deleted, was edited out of its
    /// window, or the window was turned off). The poll loop re-evaluates every
    /// alarm each cycle, so one mechanism covers all of these.
    UpcomingAlarmCleared { alarm_id: Uuid },
}

/// The name systemd knows the transient timer and its service by (`systemd-run
/// --unit=` sets both). Each reschedule first stops whatever has this name,
/// whether it is about to be replaced or nothing is left to wait for.
const WAKE_UNIT_NAME: &str = "cajole-wake";

/// How long before the target moment systemd wakes the app, leaving time to
/// compute and start a `RingSession` before the moment passes.
const WAKE_BUFFER_SECS: i64 = 15;

/// How often the fallback poll runs. It is a safety net: if the systemd timer
/// scheduling silently fails (systemd unavailable, a D-Bus error), a failure
/// means a late alarm instead of a missed one. Much longer than the old fixed
/// 20-second poll, whose constant wakeups were the battery cost.
const FALLBACK_POLL_SECS: u32 = 15 * 60;

/// State a schedule re-check needs, cheap to clone (`Rc`-based handles and a
/// `PathBuf`), handed to every place that can change the next relevant moment:
/// the systemd-timer GAction, UI actions that mutate the alarm store, and the
/// fallback poll.
#[derive(Clone)]
pub struct SchedulerHandle {
    store_path: PathBuf,
    ringing: RingingMap,
    tracker: SharedTracker,
    early_dismissal_prompted: Rc<RefCell<HashMap<Uuid, DateTime<Local>>>>,
    on_event: Rc<dyn Fn(SchedulerEvent)>,
    /// Created on first use, and only inside a Flatpak.
    wall_timer: Rc<RefCell<Option<WallClockTimer>>>,
}

impl SchedulerHandle {
    pub fn new(
        store_path: PathBuf,
        ringing: RingingMap,
        tracker: SharedTracker,
        on_event: impl Fn(SchedulerEvent) + 'static,
    ) -> Self {
        SchedulerHandle {
            store_path,
            ringing,
            tracker,
            early_dismissal_prompted: Rc::new(RefCell::new(HashMap::new())),
            on_event: Rc::new(on_event),
            wall_timer: Rc::new(RefCell::new(None)),
        }
    }
}

/// Starts the scheduler: one immediate check, then a fallback poll. The
/// systemd timer (see [`reschedule_wake_timer`]) is the primary way the app
/// learns when to act. Spawn once at startup with
/// `glib::MainContext::spawn_local`.
///
/// # Why not keep polling
///
/// The original version woke every 20 seconds for as long as the app ran (it
/// stays resident, see `main()`'s `hold()`): roughly 4,300 CPU wakes a day, a
/// measurable battery cost on a phone.
///
/// GNOME Clocks uses a shared `Gnome.WallClock` tick (about once a minute),
/// which still checks on a fixed schedule; its code has a `TODO` noting that
/// waking the system from suspend would need `CLOCK_REALTIME_ALARM`.
///
/// A `systemd --user` timer aimed at the next relevant moment avoids the
/// polling cost but cannot wake a suspended system: `WakeSystem=` does not
/// work for `--user` timers (systemd issue #17564, filed by GNOME Clocks'
/// maintainers, still open). Waking hardware is left to lower-level
/// infrastructure (see `waked.rs`).
pub async fn run(handle: SchedulerHandle) {
    check_and_reschedule(&handle).await;

    loop {
        glib::timeout_future_seconds(FALLBACK_POLL_SECS).await;
        check_and_reschedule(&handle).await;
    }
}

/// Does one check: auto-stops expired ringing alarms, handles early-dismissal
/// prompts, fires alarms whose time has come, then recomputes and reschedules
/// the wake timer. Called from the fallback poll, the `check-schedule`
/// GAction, and UI handlers right after they save, delete or toggle an alarm,
/// so changes apply immediately.
pub async fn check_and_reschedule(handle: &SchedulerHandle) {
    let now = Local::now();

    // Auto-stop alarms ringing longer than their configured duration.
    let expired: Vec<Uuid> = handle
        .ringing
        .borrow()
        .iter()
        .filter(|(_, entry)| {
            let elapsed = now.signed_duration_since(entry.started_at);
            elapsed.num_seconds() >= entry.duration_secs as i64
        })
        .map(|(id, _)| *id)
        .collect();
    for id in expired {
        tracing::info!(%id, "alarm reached its configured duration, auto-stopping");
        dismiss(&handle.store_path, &handle.ringing, id).await;
        (handle.on_event)(SchedulerEvent::AutoStopped { alarm_id: id });
    }

    match AlarmStore::load(&handle.store_path) {
        Ok(mut store) => {
            // Holiday housekeeping (stale exclusions, leftover holiday alarms)
            // runs before the tracker sync, so the tracker never caches a
            // trigger for an alarm about to be deleted.
            let holiday_settings = handle
                .store_path
                .parent()
                .and_then(|dir| {
                    crate::holiday::HolidaySettings::load(dir.join("holiday.json")).ok()
                })
                .unwrap_or_default();
            if let Err(err) = crate::holiday::sweep(&mut store, &holiday_settings, now) {
                tracing::warn!(%err, "holiday housekeeping failed");
            }

            handle.tracker.borrow_mut().sync(store.alarms(), now);

            // Early-dismissal prompts: for each enabled alarm with a window,
            // check whether its next occurrence is within it, and prompt once
            // per occurrence.
            //
            // The window check uses whole seconds: `.num_minutes()` truncates,
            // so an alarm 26 seconds out computed to `0` minutes and failed a
            // `minutes_until > 0` check. The fired event carries the
            // timestamp, so the displayed time is rounded by
            // `format_time_until`.
            let mut still_relevant: std::collections::HashSet<Uuid> =
                std::collections::HashSet::new();
            {
                let mut prompted = handle.early_dismissal_prompted.borrow_mut();
                for alarm in store.alarms() {
                    if !alarm.enabled || alarm.early_dismissal_window_minutes == 0 {
                        continue;
                    }
                    if crate::holiday::should_suppress(alarm, &holiday_settings, now.date_naive()) {
                        continue;
                    }
                    let Some(next) = next_trigger(alarm, now) else {
                        continue;
                    };
                    let seconds_until = (next - now).num_seconds();
                    let window_secs = alarm.early_dismissal_window_minutes as i64 * 60;
                    let within_window = seconds_until > 0 && seconds_until <= window_secs;
                    if !within_window {
                        continue;
                    }
                    still_relevant.insert(alarm.id);
                    if prompted.get(&alarm.id) == Some(&next) {
                        continue;
                    }
                    prompted.insert(alarm.id, next);
                    (handle.on_event)(SchedulerEvent::UpcomingAlarm {
                        alarm_id: alarm.id,
                        label: alarm.label.clone(),
                        next_trigger: next,
                    });
                }
                // Anything tracked that is no longer relevant (fired,
                // disabled, deleted, edited out of its window, or the window
                // turned off) has a stale notification: tell the UI to
                // withdraw it.
                let no_longer_relevant: Vec<Uuid> = prompted
                    .keys()
                    .filter(|id| !still_relevant.contains(id))
                    .copied()
                    .collect();
                for id in no_longer_relevant {
                    prompted.remove(&id);
                    (handle.on_event)(SchedulerEvent::UpcomingAlarmCleared { alarm_id: id });
                }
            }

            let fired = handle.tracker.borrow_mut().poll(store.alarms(), now);
            for id in fired {
                let Some(alarm) = store.get(id) else {
                    continue;
                };
                tracing::info!(label = %alarm.label, "alarm firing");

                // If this fire was a snooze coming due, clear it so the alarm
                // returns to its normal schedule; otherwise `next_trigger`
                // keeps reporting the past snooze time.
                if alarm.snoozed_until.is_some() {
                    let mut cleared = alarm.clone();
                    cleared.snoozed_until = None;
                    if let Err(err) = store.upsert(cleared) {
                        tracing::warn!(%err, "failed to clear fired snooze");
                    }
                }

                let Some(alarm) = store.get(id) else {
                    continue;
                };
                // Settings sits beside the alarm store in the `cajole` data
                // directory and is loaded fresh here, since
                // `RingSession::start` resolves sink availability at ring
                // time.
                let settings_path = handle.store_path.parent().map(|dir| dir.join("settings.json"));
                let settings = settings_path
                    .and_then(|p| crate::settings::AppSettings::load(p).ok())
                    .unwrap_or_default();

                // A `holiday_excluded` alarm during an active holiday, or a
                // `holiday_only` one outside its holiday, is still tracked and
                // rescheduled above. This is the one point where a trigger
                // becomes a ring, so the suppression check lives here.
                let holiday_path = handle.store_path.parent().map(|dir| dir.join("holiday.json"));
                let holiday_settings = holiday_path
                    .and_then(|p| crate::holiday::HolidaySettings::load(p).ok())
                    .unwrap_or_default();
                if crate::holiday::should_suppress(alarm, &holiday_settings, now.date_naive()) {
                    tracing::info!(label = %alarm.label, "alarm suppressed by holiday mode");
                    continue;
                }

                match RingSession::start(alarm, &settings).await {
                    Ok(session) => {
                        handle.ringing.borrow_mut().insert(
                            id,
                            RingingEntry {
                                session,
                                started_at: now,
                                duration_secs: alarm.alarm_duration_minutes * 60,
                            },
                        );
                        (handle.on_event)(SchedulerEvent::Ringing {
                            alarm_id: id,
                            label: alarm.label.clone(),
                        });
                    }
                    Err(err) => {
                        tracing::error!(%err, label = %alarm.label, "failed to start ring session");
                    }
                }
            }
        }
        Err(err) => {
            tracing::error!(%err, "failed to load alarm store this cycle");
        }
    }

    reschedule_wake_timer(handle).await;
}

/// How many upcoming moments to schedule wakes for. `systemd-run` accepts
/// repeated `--on-calendar=`, adding one `OnCalendar=` line per occurrence to
/// the same timer unit.
///
/// Extra entries hedge against the app crashing between the timer firing and
/// this function finishing its reschedule (a lone entry would leave nothing to
/// wake it until the 15-minute fallback poll). Every place that changes an
/// alarm already calls `check_and_reschedule` directly.
///
/// The cap is generous: at `3`, early-dismissal window-start moments could
/// fill every slot and push an alarm's own trigger out of the wake schedule,
/// firing alarms late by a varying amount. `compute_next_wakes` now also
/// guarantees required moments cannot be bumped by window-start ones (see its
/// doc), so the cap matters only for an implausible number of alarms within
/// seconds of each other.
const WAKE_LOOKAHEAD_COUNT: usize = 25;

/// The required candidates of `compute_next_wakes` (alarm triggers and ringing
/// auto-stop deadlines, not early-dismissal window openings), shared by it and
/// [`soonest_required_wake`].
fn required_wakes(alarms: &[crate::model::Alarm], ringing: &RingingMap, now: DateTime<Local>) -> Vec<DateTime<Local>> {
    let mut required: Vec<DateTime<Local>> = Vec::new();
    for entry in ringing.borrow().values() {
        required.push(entry.started_at + chrono::Duration::seconds(entry.duration_secs as i64));
    }
    for alarm in alarms {
        if let Some(next) = next_trigger(alarm, now) {
            required.push(next);
        }
    }
    required.sort();
    required.dedup();
    required
}

/// The soonest moment worth waking suspended hardware for: an alarm trigger or
/// a ringing alarm's auto-stop deadline, excluding early-dismissal window
/// openings. It is handed to `waked` (see its module doc), since waking a
/// phone from deep suspend is costly and should be reserved for an alarm
/// ringing. The `systemd --user` timer (via `compute_next_wakes`) still covers
/// window-opening moments.
fn soonest_required_wake(
    alarms: &[crate::model::Alarm],
    ringing: &RingingMap,
    now: DateTime<Local>,
) -> Option<DateTime<Local>> {
    required_wakes(alarms, ringing, now).into_iter().next()
}

/// The synchronous computation of the next few moments needing attention, kept
/// apart from [`reschedule_wake_timer`]'s D-Bus and subprocess work so it can
/// be unit-tested without an async runtime or systemd. Returns up to
/// [`WAKE_LOOKAHEAD_COUNT`] distinct moments, soonest first.
///
/// Two kinds of candidate: an alarm's trigger time (or a ringing alarm's
/// auto-stop deadline) is required, since missing it means an alarm does not
/// ring, while an early-dismissal window opening is informational. The
/// informational kind once filled the whole lookahead budget and pushed an
/// alarm trigger out of the schedule, so required candidates are always
/// included first and informational ones fill only the remaining budget,
/// whatever their order by time.
fn compute_next_wakes(
    alarms: &[crate::model::Alarm],
    ringing: &RingingMap,
    now: DateTime<Local>,
) -> Vec<DateTime<Local>> {
    let required = required_wakes(alarms, ringing, now);
    let mut informational: Vec<DateTime<Local>> = Vec::new();

    for alarm in alarms {
        let Some(next) = next_trigger(alarm, now) else {
            continue;
        };
        if alarm.early_dismissal_window_minutes > 0 {
            let window_start =
                next - chrono::Duration::minutes(alarm.early_dismissal_window_minutes as i64);
            if window_start > now {
                informational.push(window_start);
            }
        }
    }

    informational.sort();
    informational.dedup();

    let mut candidates = required;
    if candidates.len() < WAKE_LOOKAHEAD_COUNT {
        let remaining_budget = WAKE_LOOKAHEAD_COUNT - candidates.len();
        candidates.extend(informational.into_iter().take(remaining_budget));
    }
    candidates.sort();
    candidates.dedup();
    // Reached only when `required` alone exceeds the cap; it is the only way
    // to lose a real trigger.
    candidates.truncate(WAKE_LOOKAHEAD_COUNT);
    candidates
}

/// Stops whatever is scheduled under [`WAKE_UNIT_NAME`] and, if there is a
/// next moment, schedules a fresh `systemd --user` timer for it (minus
/// [`WAKE_BUFFER_SECS`]). The timer's action is inert: it pokes this
/// already-running process to do a check.
///
/// Uses `OnCalendar=`, an absolute wall-clock target, instead of
/// `OnActiveSec=`: per `systemd.timer`, a monotonic timer's countdown pauses
/// during suspend, so it would fire late by the length of the suspend.
/// `AccuracySec=1s` overrides systemd's default one-minute batching window,
/// which could otherwise eat into the small `WAKE_BUFFER_SECS` margin.
///
/// Inside a Flatpak, systemd is unreachable. A single [`WallClockTimer`] is
/// armed for the soonest moment instead, with no `WAKE_BUFFER_SECS` lead since
/// nothing batches it. Arming replaces the previous target.
async fn reschedule_wake_timer(handle: &SchedulerHandle) {
    let now = Local::now();
    let sandboxed = crate::sandbox::is_flatpak();

    if !sandboxed {
        if let Err(err) = run_command(&[
            "systemctl",
            "--user",
            "stop",
            &format!("{WAKE_UNIT_NAME}.timer"),
            &format!("{WAKE_UNIT_NAME}.service"),
        ])
        .await
        {
            // Expected the first time, when there is nothing to stop:
            // systemctl exits non-zero for "unit not found". The new timer is
            // scheduled either way.
            tracing::debug!(%err, "stopping previous wake timer (may not have existed)");
        }
    }

    let store_result = AlarmStore::load(&handle.store_path);
    let next_wakes = match &store_result {
        Ok(store) => compute_next_wakes(store.alarms(), &handle.ringing, now),
        Err(err) => {
            tracing::warn!(%err, "failed to load alarm store to schedule wake timer");
            Vec::new()
        }
    };

    if next_wakes.is_empty() {
        // Nothing left to wait for, so no timer is scheduled. Adding an alarm
        // (or anything that makes one relevant) calls `check_and_reschedule`
        // from its UI action, which re-arms the timer.
        tracing::debug!("reschedule: nothing to wait for, skipping systemd-run entirely");
        if sandboxed {
            disarm_wall_timer(handle);
        }
        crate::waked::remove_wake_time().await;
        return;
    }
    tracing::debug!(count = next_wakes.len(), "reschedule: have wakes, will call systemd-run");

    // Also tells waked about the soonest moment worth waking the hardware for;
    // see `soonest_required_wake` for why this is not `next_wakes.first()`,
    // which can be an early-dismissal window opening, not worth waking a
    // suspended phone for.
    if let Some(soonest_required) = store_result
        .as_ref()
        .ok()
        .and_then(|store| soonest_required_wake(store.alarms(), &handle.ringing, now))
    {
        crate::waked::update_wake_time(soonest_required.timestamp()).await;
    } else {
        crate::waked::remove_wake_time().await;
    }

    if sandboxed {
        arm_wall_timer(handle, sandbox_wake_target(next_wakes[0], now));
        return;
    }

    // Guards against a bug: when the soonest required moment is at or before
    // `now + WAKE_BUFFER_SECS`, the `--on-calendar=` target below is clamped
    // to `now` (see `.max(now)`). Per systemd's documentation, an
    // `OnCalendar=` timer (unlike `OnBootSec=`/`OnStartupSec=`) does not catch
    // up and fire when its target has already passed at creation. On a device,
    // the wake timer fired about 15s early, `check_and_reschedule` saw the
    // alarm was not yet due and rescheduled, but the new target was already
    // past when systemd processed it, so the timer never elapsed and the alarm
    // never rang.
    //
    // The fix does not depend on systemd for this window: the process is
    // already running, so an in-process timer is more reliable than a new
    // transient unit. It runs alongside the systemd timer below, which is
    // still needed to wake the process if it is not running.
    if let Some(soonest) = next_wakes.first() {
        let lead_time = *soonest - now;
        if lead_time <= chrono::Duration::seconds(WAKE_BUFFER_SECS * 2) {
            let delay_ms = lead_time.num_milliseconds().max(0) as u64;
            let handle = handle.clone();
            glib::MainContext::default().spawn_local(async move {
                glib::timeout_future(std::time::Duration::from_millis(delay_ms)).await;
                check_and_reschedule(&handle).await;
            });
        }
    }

    // One `--on-calendar=` per upcoming moment; see `WAKE_LOOKAHEAD_COUNT` for
    // why more than the first.
    let calendar_specs: Vec<String> = next_wakes
        .iter()
        .map(|target| {
            (*target - chrono::Duration::seconds(WAKE_BUFFER_SECS))
                .max(now)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .collect();
    let object_path = format!("/{}", crate::APP_ID.replace('.', "/"));

    let mut argv: Vec<String> = vec![
        "systemd-run".into(),
        "--user".into(),
        "--collect".into(),
        format!("--unit={WAKE_UNIT_NAME}"),
    ];
    for spec in &calendar_specs {
        argv.push(format!("--on-calendar={spec}"));
    }
    argv.push("--timer-property=AccuracySec=1s".into());
    argv.extend(
        [
            "--",
            "gdbus",
            "call",
            "--session",
            "--dest",
            crate::APP_ID,
            "--object-path",
            &object_path,
            "--method",
            "org.gtk.Actions.Activate",
            "check-schedule",
            "[]",
            "{}",
        ]
        .map(String::from),
    );

    let argv_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
    tracing::debug!(argv = ?argv_refs, "spawning systemd-run for wake timer");
    let result = run_command(&argv_refs).await;
    tracing::debug!("systemd-run for wake timer returned");

    if let Err(err) = result {
        tracing::warn!(
            %err,
            targets = ?calendar_specs,
            "failed to schedule systemd wake timer; relying on the fallback poll instead"
        );
    }
}

/// Where to aim the sandbox wall-clock timer. A target at or before `now`
/// means the check that just ran did not consume it, so it is retried in a
/// second instead of re-firing in a tight loop.
fn sandbox_wake_target(soonest: DateTime<Local>, now: DateTime<Local>) -> DateTime<Local> {
    if soonest > now {
        soonest
    } else {
        now + chrono::Duration::seconds(1)
    }
}

fn arm_wall_timer(handle: &SchedulerHandle, target: DateTime<Local>) {
    let mut slot = handle.wall_timer.borrow_mut();
    if slot.is_none() {
        let on_fire = {
            let handle = handle.clone();
            move || {
                let handle = handle.clone();
                glib::MainContext::default().spawn_local(async move {
                    check_and_reschedule(&handle).await;
                });
            }
        };
        match WallClockTimer::new(on_fire) {
            Ok(timer) => *slot = Some(timer),
            Err(err) => {
                tracing::warn!(%err, "failed to create wall-clock timer; relying on the fallback poll");
                return;
            }
        }
    }
    if let Some(timer) = slot.as_ref() {
        if let Err(err) = timer.arm(target) {
            tracing::warn!(%err, %target, "failed to arm wall-clock timer; relying on the fallback poll");
        }
    }
}

fn disarm_wall_timer(handle: &SchedulerHandle) {
    if let Some(timer) = handle.wall_timer.borrow().as_ref() {
        if let Err(err) = timer.disarm() {
            tracing::warn!(%err, "failed to disarm wall-clock timer");
        }
    }
}

/// Runs a command to completion and returns whether it succeeded, with the
/// stderr text on failure, so a failure is not silent (see `feedback.rs`).
async fn run_command(argv: &[&str]) -> anyhow::Result<()> {
    let os_argv: Vec<&std::ffi::OsStr> = argv.iter().map(std::ffi::OsStr::new).collect();
    let subprocess = gio::Subprocess::newv(
        &os_argv,
        gio::SubprocessFlags::STDOUT_SILENCE | gio::SubprocessFlags::STDERR_PIPE,
    )
    .map_err(|err| anyhow::anyhow!("failed to spawn {:?}: {err}", argv.first()))?;

    let (_stdout, stderr) = subprocess
        .communicate_utf8_future(None)
        .await
        .map_err(|err| anyhow::anyhow!("failed to run {:?}: {err}", argv.first()))?;

    if !subprocess.wait_check_future().await.is_ok() {
        let stderr = stderr.unwrap_or_default();
        return Err(anyhow::anyhow!("{}", stderr.trim()));
    }
    Ok(())
}

/// Stops a ringing alarm and restores what its [`RingSession`] changed, with
/// no further bookkeeping. [`dismiss`] and [`snooze`] build on it. [`snooze`]
/// calls it directly so snoozing a one-time alarm does not trigger
/// [`dismiss`]'s "disable it" logic; a snoozed alarm comes back.
async fn stop_ringing(ringing: &RingingMap, alarm_id: Uuid) {
    let entry = ringing.borrow_mut().remove(&alarm_id);
    if let Some(entry) = entry {
        if let Err(err) = entry.session.stop().await {
            tracing::warn!(%err, "failed to cleanly stop ring session on dismiss");
        }
    }
}

/// Stops a ringing alarm as a genuine dismissal: called from a Dismiss button
/// or notification action via `spawn_local`, and by [`run`]'s auto-stop check
/// (a missed alarm counts as dismissed, as in GNOME Clocks). A one-time alarm
/// with no repeat days is then turned off, as GNOME Clocks does, since it has
/// no next occurrence.
///
/// A `holiday_only` alarm is deleted, not just disabled, once
/// [`holiday::holiday_alarm_is_exhausted`] says nothing is left inside the
/// holiday window. This runs after the one-time auto-disable, so a one-time
/// holiday alarm is judged like a repeating one, by whether it has a next
/// trigger.
pub async fn dismiss(store_path: &Path, ringing: &RingingMap, alarm_id: Uuid) {
    stop_ringing(ringing, alarm_id).await;

    match AlarmStore::load(store_path) {
        Ok(mut store) => {
            let Some(alarm) = store.get(alarm_id) else {
                return;
            };
            if alarm.repeat.is_empty() && alarm.enabled {
                let mut disabled = alarm.clone();
                disabled.enabled = false;
                if let Err(err) = store.upsert(disabled) {
                    tracing::warn!(%err, "failed to auto-disable one-time alarm after dismiss");
                }
            }

            let Some(alarm) = store.get(alarm_id) else {
                return;
            };
            if alarm.holiday_only {
                let holiday_path = store_path.parent().map(|dir| dir.join("holiday.json"));
                let holiday_settings = holiday_path
                    .and_then(|p| crate::holiday::HolidaySettings::load(p).ok())
                    .unwrap_or_default();
                if crate::holiday::holiday_alarm_is_exhausted(
                    alarm,
                    &holiday_settings,
                    Local::now(),
                ) {
                    if let Err(err) = store.remove(alarm_id) {
                        tracing::warn!(%err, "failed to delete exhausted holiday alarm");
                    }
                }
            }
        }
        Err(err) => tracing::warn!(%err, "failed to load alarm store to auto-disable one-time alarm"),
    }
}

/// Stops a ringing alarm and reschedules it to fire after its snooze duration,
/// leaving the daily and repeat schedule alone. Called from a Snooze button
/// handler via `spawn_local`.
pub async fn snooze(
    store_path: &Path,
    ringing: &RingingMap,
    tracker: &SharedTracker,
    alarm_id: Uuid,
) {
    stop_ringing(ringing, alarm_id).await;
    if let Err(err) = snooze_alarm(store_path, alarm_id).await {
        tracing::warn!(%err, "failed to reschedule snoozed alarm");
        return;
    }
    // The alarm was tracked before this snooze (it just fired), so without
    // this the next `sync` would keep its stale cached next trigger; see
    // `TriggerTracker::forget`.
    tracker.borrow_mut().forget(alarm_id);
}

/// Sets `snoozed_until` to `snooze_minutes` from now, leaving `time` and
/// `repeat` alone. `next_trigger` (see `schedule.rs`) checks this field first
/// and reports it until it fires, when [`run`] clears it.
async fn snooze_alarm(store_path: &Path, alarm_id: Uuid) -> anyhow::Result<()> {
    let mut store = AlarmStore::load(store_path)?;
    let Some(alarm) = store.get(alarm_id) else {
        return Ok(());
    };
    let mut snoozed = alarm.clone();
    snoozed.snoozed_until =
        Some(Local::now() + chrono::Duration::minutes(snoozed.snooze_minutes as i64));
    store.upsert(snoozed)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Alarm;
    use chrono::{NaiveTime, TimeZone};

    fn at(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(y, m, d, h, min, 0).unwrap()
    }

    fn empty_ringing() -> RingingMap {
        Rc::new(RefCell::new(HashMap::new()))
    }

    #[test]
    fn compute_next_wakes_is_empty_with_no_alarms_and_nothing_ringing() {
        let now = at(2026, 8, 24, 6, 0);
        assert_eq!(compute_next_wakes(&[], &empty_ringing(), now), Vec::<DateTime<Local>>::new());
    }

    #[test]
    fn compute_next_wakes_is_the_alarms_own_trigger_time_with_no_early_window() {
        let now = at(2026, 8, 24, 6, 0);
        let mut alarm = Alarm::new("Test", NaiveTime::from_hms_opt(9, 0, 0).unwrap());
        alarm.early_dismissal_window_minutes = 0;
        let next = compute_next_wakes(&[alarm], &empty_ringing(), now);
        assert_eq!(next, vec![at(2026, 8, 24, 9, 0)]);
    }

    #[test]
    fn compute_next_wakes_prefers_the_early_dismissal_window_start_when_earlier() {
        // Alarm at 9:00 with a 30-minute early-dismissal window: the scheduler
        // must wake at 8:30, when the window opens, or the notification would
        // never show before the alarm rings. Both moments appear, soonest
        // first.
        let now = at(2026, 8, 24, 6, 0);
        let mut alarm = Alarm::new("Test", NaiveTime::from_hms_opt(9, 0, 0).unwrap());
        alarm.early_dismissal_window_minutes = 30;
        let next = compute_next_wakes(&[alarm], &empty_ringing(), now);
        assert_eq!(next, vec![at(2026, 8, 24, 8, 30), at(2026, 8, 24, 9, 0)]);
    }

    #[test]
    fn compute_next_wakes_does_not_use_a_window_start_already_in_the_past() {
        // The window would already have opened (the alarm is 10 minutes out,
        // the window 30): that moment is past and not worth waking for; the
        // alarm's own trigger is still ahead.
        let now = at(2026, 8, 24, 8, 50);
        let mut alarm = Alarm::new("Test", NaiveTime::from_hms_opt(9, 0, 0).unwrap());
        alarm.early_dismissal_window_minutes = 30;
        let next = compute_next_wakes(&[alarm], &empty_ringing(), now);
        assert_eq!(next, vec![at(2026, 8, 24, 9, 0)]);
    }

    #[test]
    fn compute_next_wakes_never_drops_a_real_trigger_for_a_window_start() {
        // Regression: several alarms due close together, each with an
        // early-dismissal window, had later alarms' trigger times dropped from
        // the wake schedule, so those alarms fired late. All four trigger
        // times must survive however many window-start moments compete.
        let now = at(2026, 8, 24, 6, 0);
        let mut a1 = Alarm::new("A1", NaiveTime::from_hms_opt(7, 1, 0).unwrap());
        a1.early_dismissal_window_minutes = 1;
        let mut a2 = Alarm::new("A2", NaiveTime::from_hms_opt(7, 3, 0).unwrap());
        a2.early_dismissal_window_minutes = 1;
        let mut a3 = Alarm::new("A3", NaiveTime::from_hms_opt(7, 5, 0).unwrap());
        a3.early_dismissal_window_minutes = 1;
        let mut a4 = Alarm::new("A4", NaiveTime::from_hms_opt(7, 7, 0).unwrap());
        a4.early_dismissal_window_minutes = 1;

        let next = compute_next_wakes(&[a1, a2, a3, a4], &empty_ringing(), now);
        for expected in [
            at(2026, 8, 24, 7, 1),
            at(2026, 8, 24, 7, 3),
            at(2026, 8, 24, 7, 5),
            at(2026, 8, 24, 7, 7),
        ] {
            assert!(
                next.contains(&expected),
                "real trigger {expected:?} was dropped from the wake schedule; got {next:?}"
            );
        }
    }

    #[test]
    fn soonest_required_wake_skips_the_early_dismissal_window() {
        // Regression: an alarm with a 30-minute early-dismissal window handed
        // `waked` a timestamp 30 minutes early, because `next_wakes.first()`
        // (which the systemd timer correctly uses, window openings included)
        // does not distinguish informational candidates. The dedicated
        // function must skip the window and return the real trigger time.
        let now = at(2026, 8, 24, 6, 0);
        let mut alarm = Alarm::new("Test", NaiveTime::from_hms_opt(7, 0, 0).unwrap());
        alarm.early_dismissal_window_minutes = 30;

        // The general wake list includes the window opening as its soonest
        // entry, confirming the scenario.
        let general = compute_next_wakes(&[alarm.clone()], &empty_ringing(), now);
        assert_eq!(general.first(), Some(&at(2026, 8, 24, 6, 30)));

        // The required-only version skips to the 7:00 trigger, not the 6:30
        // window opening.
        let required = soonest_required_wake(&[alarm], &empty_ringing(), now);
        assert_eq!(required, Some(at(2026, 8, 24, 7, 0)));
    }

    #[test]
    fn compute_next_wakes_fills_remaining_budget_with_window_starts() {
        let now = at(2026, 8, 24, 6, 0);
        let mut earlier = Alarm::new("Earlier", NaiveTime::from_hms_opt(7, 0, 0).unwrap());
        earlier.early_dismissal_window_minutes = 0;
        let mut later = Alarm::new("Later", NaiveTime::from_hms_opt(9, 0, 0).unwrap());
        later.early_dismissal_window_minutes = 0;
        let next = compute_next_wakes(&[later, earlier], &empty_ringing(), now);
        assert_eq!(next, vec![at(2026, 8, 24, 7, 0), at(2026, 8, 24, 9, 0)]);
    }

    #[test]
    fn compute_next_wakes_ignores_a_disabled_alarm() {
        let now = at(2026, 8, 24, 6, 0);
        let mut alarm = Alarm::new("Test", NaiveTime::from_hms_opt(9, 0, 0).unwrap());
        alarm.enabled = false;
        assert_eq!(compute_next_wakes(&[alarm], &empty_ringing(), now), Vec::<DateTime<Local>>::new());
    }

    #[test]
    fn compute_next_wakes_caps_at_the_lookahead_count_once_genuinely_over_capacity() {
        // More alarms than `WAKE_LOOKAHEAD_COUNT` real triggers alone, before
        // counting windows: the cap still applies, and trigger candidates are
        // what is kept.
        let now = at(2026, 8, 24, 0, 0);
        let alarms: Vec<Alarm> = (0..30)
            .map(|i| {
                let hour = i / 60;
                let minute = i % 60;
                let mut alarm = Alarm::new(
                    format!("Alarm {i}"),
                    NaiveTime::from_hms_opt(hour, minute, 0).unwrap(),
                );
                alarm.early_dismissal_window_minutes = 0;
                alarm
            })
            .collect();
        let next = compute_next_wakes(&alarms, &empty_ringing(), now);
        assert_eq!(next.len(), WAKE_LOOKAHEAD_COUNT);
        assert!(next.windows(2).all(|pair| pair[0] <= pair[1]));
        // The soonest ones, not an arbitrary subset.
        assert_eq!(next[0], at(2026, 8, 24, 0, 1));
    }

    #[test]
    fn sandbox_wake_target_is_the_soonest_moment_when_it_is_ahead() {
        let now = at(2026, 10, 31, 6, 59);
        let soonest = at(2026, 10, 31, 7, 0);
        assert_eq!(sandbox_wake_target(soonest, now), soonest);
    }

    #[test]
    fn sandbox_wake_target_retries_in_a_second_when_not_ahead() {
        let now = at(2026, 10, 31, 7, 0);
        let expected = now + chrono::Duration::seconds(1);
        assert_eq!(sandbox_wake_target(now, now), expected);
        assert_eq!(sandbox_wake_target(at(2026, 10, 31, 6, 0), now), expected);
    }
}
