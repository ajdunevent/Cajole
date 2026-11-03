use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::feedback::DirectPlayback;
use crate::model::{Alarm, OutputMode};
use crate::mpris;
use crate::settings::{self, AppSettings};
use crate::volume::{run_ramp, CombinedSinkHandle, SinkControl, SinkState};
use crate::{display, haptic};

/// Everything one ringing alarm changes (sound, volume, vibration, glow,
/// screen state), plus the state needed to undo it in [`RingSession::stop`].
///
/// Restore state (volume, mute, sink, paused media players) lives on this
/// struct, so overlapping alarms do not share bookkeeping.
///
/// Everything runs on the app's single `glib::MainContext`, like GTK,
/// feedbackd, Mutter and MPRIS clients, without a second (`tokio`) runtime.
pub struct RingSession {
    conn: gio::DBusConnection,
    // `None` if the alarm has no sound selected or playback could not be
    // resolved. The alarm still rings with everything else (dialog,
    // notification, glow, vibration).
    sound: Option<DirectPlayback>,
    ramp_handle: glib::JoinHandle<()>,
    // Shared with the user-activity callback, which aborts the ramp.
    brightness_ramp_handle: Rc<RefCell<Option<glib::JoinHandle<()>>>>,
    // Separate handle for vibration, which runs as a client-side segment loop
    // (see `haptic::play_segments`) and is aborted on stop like the ramps.
    vibration_handle: glib::JoinHandle<()>,
    // Restores the feedbackd profile when dropped.
    silent_override: Option<crate::silent_override::Guard>,
    // Keeps the screen on during the ring; released when dropped.
    _idle_inhibit: Option<crate::session_inhibit::IdleInhibit>,
    // `Some` only when the glow ran. Read before the wake, which changes it.
    original_power_save_mode: Option<i32>,
    // Set when input or an unlock ended the glow. The power-save mode is then
    // not restored, since blanking would interrupt the person using the phone.
    phone_in_use: Rc<Cell<bool>>,
    glow: Rc<RefCell<Option<GlowRestore>>>,
    _activity: Option<crate::activity::UserActivityWatch>,
    _unlock_watch: Option<crate::lock_watch::UnlockWatch>,
    sink_state: SinkState,
    paused_players: Vec<String>,
    output_restore: OutputRestore,
}

/// Undo for the output-routing choice [`RingSession::start`] made, beyond the
/// volume and mute restore of [`SinkState`], which always runs. Covers
/// `Specific` and `AllSinks` only.
enum OutputRestore {
    /// `OutputMode::ActiveSink`, or a fallback from `Specific` or `AllSinks`
    /// when there was nothing to route to (see `start`). Only the `SinkState`
    /// restore applies.
    None,
    Specific {
        original_default_sink: String,
    },
    AllSinks {
        original_default_sink: String,
        combined: CombinedSinkHandle,
    },
}

/// What the glow changed on screen, restored on the first input or unlock,
/// or on stop, whichever comes first.
struct GlowRestore {
    original_brightness: Option<display::Brightness>,
    /// Turns automatic brightness back on when dropped, after the level above is restored.
    _auto_brightness: Option<crate::auto_brightness::Suspended>,
}

/// Reads the backlight once it is no longer forced to zero. FuriOS Phosh
/// writes 0 when the screen blanks and restores the level asynchronously
/// after the wake, so an early read returns 0. Levels under 1% of the
/// maximum are rejected; `None` if the level stays below that.
async fn recorded_brightness(conn: &gio::DBusConnection) -> Option<display::Brightness> {
    for _ in 0..8 {
        if let Some(level) = display::read_brightness(conn)
            .await
            .filter(display::Brightness::is_plausible_baseline)
        {
            return Some(level);
        }
        glib::timeout_future(std::time::Duration::from_millis(250)).await;
    }
    None
}

async fn restore_glow(
    glow: &Rc<RefCell<Option<GlowRestore>>>,
    conn: &gio::DBusConnection,
    reason: &str,
) {
    let Some(state) = glow.borrow_mut().take() else {
        return;
    };
    tracing::info!(reason, recorded = ?state.original_brightness, "restoring screen brightness");
    if let Some(original) = state.original_brightness {
        // Bounded so a stuck call cannot keep automatic brightness off.
        match glib::future_with_timeout(
            std::time::Duration::from_secs(3),
            display::restore_brightness(conn, original),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(err)) => tracing::warn!(%err, "failed to restore original screen brightness"),
            Err(_) => tracing::warn!("restoring the screen brightness timed out"),
        }
    }
    // Dropping `state` turns automatic brightness back on.
    drop(state);
    tracing::info!("screen brightness restored");
}

impl RingSession {
    /// Starts ringing for `alarm`. `settings` supplies `backup_sink`, used if
    /// the alarm's `OutputMode::Specific` destination is unavailable; see
    /// [`settings::resolve_speaker_destination`].
    pub async fn start(alarm: &Alarm, settings: &AppSettings) -> anyhow::Result<RingSession> {
        let conn = gio::bus_get_future(gio::BusType::Session)
            .await
            .map_err(|err| anyhow::anyhow!("failed to connect to session bus: {err}"))?;

        let idle_inhibit = crate::session_inhibit::inhibit_idle(&conn, "Alarm ringing").await;

        let mut original_power_save_mode = None;
        let mut glow_state = None;

        if alarm.glow_enabled {
            // Off before anything touches the screen, so its adjustments
            // cannot fight the ramp.
            let auto_brightness = crate::auto_brightness::suspend();

            // Read before the wake, which changes it.
            original_power_save_mode = display::get_power_save_mode(&conn).await;

            if let Err(err) = display::wake_screen(&conn).await {
                tracing::warn!(%err, "failed to wake screen, continuing anyway");
            }

            // The idle dim leaves the backlight at its dim level until the
            // first input; undimming restores the level it saved.
            if let Err(err) = display::undim(&conn).await {
                tracing::debug!(%err, "undim failed");
            }
            let original_brightness = recorded_brightness(&conn).await;
            tracing::info!(
                recorded = ?original_brightness,
                auto_brightness_was_on = ?auto_brightness.as_ref().map(|a| a.was_on()),
                "glow started"
            );
            glow_state = Some(GlowRestore {
                original_brightness,
                _auto_brightness: auto_brightness,
            });

            // Set inline, not left to the ramp task: the wake can leave the
            // backlight bright until the ramp's first step.
            let initial_glow = if alarm.gentle_wake_enabled {
                alarm.glow_start_percent
            } else {
                alarm.glow_end_percent
            };
            if let Err(err) = display::set_screen_brightness(&conn, initial_glow).await {
                tracing::warn!(%err, "failed to set initial screen brightness");
            }
        }

        let sink_state = SinkControl::snapshot().await?;

        // Unmuted on every ring. A silent alarm is Play sound off.
        if let Err(err) = SinkControl::set_mute(&sink_state.sink_name, false).await {
            tracing::warn!(%err, "failed to unmute sink, continuing anyway");
        }
        if let Err(err) = SinkControl::ensure_speaker_amp_on().await {
            tracing::warn!(%err, "failed to ensure speaker amp is on, continuing anyway");
        }

        let paused_players = mpris::pause_playing(&conn).await.unwrap_or_else(|err| {
            tracing::warn!(%err, "failed to pause media players, continuing anyway");
            Vec::new()
        });

        let (ramp_sink, output_restore) = match &alarm.output_mode {
            OutputMode::ActiveSink => (sink_state.sink_name.clone(), OutputRestore::None),

            OutputMode::Specific(preferred) => {
                match settings::resolve_speaker_destination(preferred, settings) {
                    Some(resolved) => {
                        SinkControl::force_speaker(&resolved.sink, resolved.port.as_deref()).await?;
                        (
                            resolved.sink.clone(),
                            OutputRestore::Specific {
                                original_default_sink: sink_state.sink_name.clone(),
                            },
                        )
                    }
                    None => {
                        tracing::warn!(
                            "alarm's configured destination (and any backup) isn't currently \
                             available; falling back to the active sink"
                        );
                        (sink_state.sink_name.clone(), OutputRestore::None)
                    }
                }
            }

            OutputMode::AllSinks => match SinkControl::enable_all_sinks().await? {
                Some(combined) => {
                    let ramp_sink = combined.sink_name.clone();
                    (
                        ramp_sink,
                        OutputRestore::AllSinks {
                            original_default_sink: sink_state.sink_name.clone(),
                            combined,
                        },
                    )
                }
                None => {
                    tracing::warn!(
                        "alarm output_mode is AllSinks but fewer than two sinks are available; \
                         falling back to the active sink"
                    );
                    (sink_state.sink_name.clone(), OutputRestore::None)
                }
            },
        };

        // `DirectPlayback` runs for the alarm's duration (the limit is
        // enforced by `scheduler::run`'s auto-dismiss check) plus 30 s of
        // slack.
        let watch_secs = (alarm.alarm_duration_minutes as u64 * 60) + 30;

        // `play_sound` off, or no sound chosen with it on (legacy data, or a
        // deleted sound), means the alarm rings without sound; glow, vibration
        // and the notification still fire.
        let resolved_sound_file = if alarm.play_sound { alarm.sound_file.clone() } else { None };
        if alarm.play_sound && resolved_sound_file.is_none() {
            tracing::warn!(
                label = %alarm.label,
                "alarm has \"Play sound\" on but no sound selected; it will ring without sound"
            );
        }

        // Sound plays through [`DirectPlayback`] whether or not feedbackd is
        // running. Triggering feedbackd's event with a sound-file override is
        // unreliable; GNOME Clocks' FuriOS fork hit the same problem and hid
        // its custom-sound picker (FuriLabs/issue-tracker#15).
        // `DirectPlayback` avoids feedbackd's event system, so the Sound
        // Manager keeps working.
        //
        // Vibration is unaffected: it uses a separate `Haptic.Vibrate` call
        // (see `haptic::trigger_vibration`).
        let sound = resolved_sound_file
            .clone()
            .map(|path| DirectPlayback::start(path, watch_secs));

        let total_ring_secs = alarm.alarm_duration_minutes * 60;

        let (vibration_segments, vibration_enabled) = if alarm.vibration_enabled {
            let segments = haptic::build_ring_segments(
                alarm.vibration_pattern,
                alarm.gentle_wake_enabled,
                alarm.vibration_start_percent,
                alarm.vibration_end_percent,
                alarm.gentle_wake_duration_secs,
                total_ring_secs,
            );
            (segments, true)
        } else {
            (Vec::new(), false)
        };
        let silent_override = if vibration_enabled {
            crate::silent_override::engage(&conn, settings.vibrate_in_silent)
        } else {
            None
        };
        // Never set to `false` for a real ring: `RingSession::stop` aborts
        // `vibration_handle` instead. The flag gives `play_segments` one
        // signature shared with the interruptible preview button.
        let vibration_should_continue = Rc::new(Cell::new(true));
        let vibration_handle = if vibration_enabled {
            let conn = conn.clone();
            let should_continue = vibration_should_continue.clone();
            glib::MainContext::default().spawn_local(async move {
                haptic::play_segments(&conn, &vibration_segments, &should_continue).await;
            })
        } else {
            glib::MainContext::default().spawn_local(async {})
        };

        let ramp_handle = if alarm.play_sound {
            let ramp_sink = ramp_sink.clone();
            if alarm.gentle_wake_enabled {
                let start = alarm.volume_start_percent;
                let end = alarm.volume_end_percent;
                let duration = alarm.gentle_wake_duration_secs;
                glib::MainContext::default().spawn_local(async move {
                    if let Err(err) = run_ramp(&ramp_sink, start, end, duration).await {
                        tracing::warn!(%err, "volume ramp task failed");
                    }
                })
            } else {
                // Constant volume: applied once, since the "Volume (%)" row is
                // shown and expected to take effect without Gentle Wake.
                let end = alarm.volume_end_percent;
                glib::MainContext::default().spawn_local(async move {
                    if let Err(err) = SinkControl::set_volume(&ramp_sink, end).await {
                        tracing::warn!(%err, "failed to set constant volume");
                    }
                })
            }
        } else {
            // Sound and volume are off for this alarm, so the device volume is
            // left as it was (apart from the unmute above). A no-op task keeps
            // the field type uniform instead of an `Option<JoinHandle<_>>`.
            glib::MainContext::default().spawn_local(async {})
        };

        // Floor on the time between brightness writes, as a precaution.
        // Sysfs writes 0.1 ms apart were tested on the FLX1S without a
        // problem. A ramp that would step faster takes larger steps, so it
        // ends on time.
        const BRIGHTNESS_RAMP_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
        let brightness_ramp_handle = if alarm.glow_enabled {
            let conn = conn.clone();
            if alarm.gentle_wake_enabled {
                let start = alarm.glow_start_percent;
                let end = alarm.glow_end_percent;
                let duration = alarm.gentle_wake_duration_secs;
                glib::MainContext::default().spawn_local(async move {
                    let (values, interval) =
                        crate::ramp::paced(start, end, duration, BRIGHTNESS_RAMP_MIN_INTERVAL);
                    for step in values {
                        if let Err(err) = display::set_screen_brightness(&conn, step).await {
                            tracing::warn!(%err, "brightness ramp step failed");
                            break;
                        }
                        glib::timeout_future(interval).await;
                    }
                })
            } else {
                // Constant brightness, applied once like the non-ramping
                // volume above.
                let end = alarm.glow_end_percent;
                glib::MainContext::default().spawn_local(async move {
                    if let Err(err) = display::set_screen_brightness(&conn, end).await {
                        tracing::warn!(%err, "failed to set constant screen brightness");
                    }
                })
            }
        } else {
            glib::MainContext::default().spawn_local(async {})
        };

        let brightness_ramp_handle = Rc::new(RefCell::new(Some(brightness_ramp_handle)));
        let glow = Rc::new(RefCell::new(glow_state));
        let phone_in_use = Rc::new(Cell::new(false));
        let (activity, unlock_watch) = if alarm.glow_enabled {
            // Input and unlock both end the glow; `restore_glow` runs once.
            let end_glow: Rc<dyn Fn(&'static str)> = {
                let conn = conn.clone();
                let glow = glow.clone();
                let ramp = brightness_ramp_handle.clone();
                let phone_in_use = phone_in_use.clone();
                Rc::new(move |reason| {
                    tracing::info!(reason, "phone in use during the glow");
                    phone_in_use.set(true);
                    if let Some(handle) = ramp.borrow_mut().take() {
                        handle.abort();
                    }
                    let glow = glow.clone();
                    let conn = conn.clone();
                    glib::spawn_future_local(async move {
                        restore_glow(&glow, &conn, reason).await
                    });
                })
            };
            let activity = {
                let end_glow = end_glow.clone();
                crate::activity::watch_once(&conn, move || end_glow("input")).await
            };
            let unlock_watch = {
                let end_glow = end_glow.clone();
                crate::lock_watch::watch_unlock(move || end_glow("unlock")).await
            };
            (activity, unlock_watch)
        } else {
            (None, None)
        };

        Ok(RingSession {
            conn,
            sound,
            ramp_handle,
            brightness_ramp_handle,
            vibration_handle,
            silent_override,
            _idle_inhibit: idle_inhibit,
            original_power_save_mode,
            phone_in_use,
            glow,
            _activity: activity,
            _unlock_watch: unlock_watch,
            sink_state,
            paused_players,
            output_restore,
        })
    }

    /// Stops the alarm and restores what it changed: cancels in-flight ramps
    /// and vibration, restores glow, power-save mode, volume, mute and default
    /// sink (unloading the `AllSinks` combine-sink module), and resumes paused
    /// media players.
    pub async fn stop(self) -> anyhow::Result<()> {
        self.ramp_handle.abort();
        if let Some(handle) = self.brightness_ramp_handle.borrow_mut().take() {
            handle.abort();
        }
        self.vibration_handle.abort();
        drop(self.silent_override);

        if let Some(sound) = self.sound {
            sound.stop();
        }

        // Brightness first: the reverse order leaves the screen briefly
        // awake at the wrong brightness before it blanks. The power-save
        // mode is left alone when the phone is in use.
        restore_glow(&self.glow, &self.conn, "stop").await;
        let original_power_save_mode = self
            .original_power_save_mode
            .filter(|_| !self.phone_in_use.get());
        if let Some(original) = original_power_save_mode {
            if let Err(err) = display::set_power_save_mode(&self.conn, original).await {
                tracing::warn!(%err, "failed to restore original screen power-save mode");
            }
        }

        // Each step runs after an earlier failure, so a failed volume
        // restore cannot leave the routing or the paused players unrestored.
        // The first error is returned.
        let mut first_error = None;
        let mut note = |result: anyhow::Result<()>| {
            if let Err(err) = result {
                tracing::warn!(%err, "restore step failed");
                first_error.get_or_insert(err);
            }
        };

        note(SinkControl::restore(&self.sink_state).await);

        match self.output_restore {
            OutputRestore::None => {}
            OutputRestore::Specific {
                original_default_sink,
            } => {
                note(SinkControl::restore_default_sink(&original_default_sink).await);
            }
            OutputRestore::AllSinks {
                original_default_sink,
                combined,
            } => {
                note(SinkControl::disable_all_sinks(combined, &original_default_sink).await);
            }
        }

        mpris::resume_all(&self.conn, &self.paused_players).await;

        first_error.map_or(Ok(()), Err)
    }
}
