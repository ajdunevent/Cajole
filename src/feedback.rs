use gio::prelude::*;
use std::cell::Cell;
use std::rc::Rc;

/// Whether feedbackd (`org.sigxcpu.Feedback`) is running on the session bus,
/// via a D-Bus name-ownership query. feedbackd is a mobile-shell component a
/// typical desktop session lacks, and [`FeedbackSession::start`] cannot tell
/// "not installed" from "not running" from "call failed". `ring.rs` uses this
/// to decide up front whether to use feedbackd or go straight to
/// [`DirectPlayback`].
pub async fn is_running(conn: &gio::DBusConnection) -> bool {
    let Ok(proxy) = gio::DBusProxy::new_future(
        conn,
        gio::DBusProxyFlags::NONE,
        None,
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )
    .await
    else {
        return false;
    };
    let args = ("org.sigxcpu.Feedback",).to_variant();
    match proxy
        .call_future("NameHasOwner", Some(&args), gio::DBusCallFlags::NONE, -1)
        .await
    {
        Ok(result) => result.get::<(bool,)>().map(|(has,)| has).unwrap_or(false),
        Err(_) => false,
    }
}

/// Whether vibration is possible, used to decide whether to offer Gentle
/// Wake's vibration option. An alias of [`is_running`], since vibration has no
/// fallback like [`DirectPlayback`] for sound. It cannot tell a missing
/// vibration motor from a working one; feedbackd's D-Bus interface exposes no
/// per-device check.
pub async fn vibration_available(conn: &gio::DBusConnection) -> bool {
    is_running(conn).await
}

/// A running feedback event (sound, haptic or LED per the active feedbackd
/// theme), started through `fbcli`.
///
/// Not used by `ring.rs`'s alarm pipeline: sound plays through
/// [`DirectPlayback`], since triggering this event with a `--sound-file`
/// override is unreliable on feedbackd (see `ring.rs`).
///
/// feedbackd stops a client's feedback when the client disconnects from D-Bus,
/// and `fbcli --watch` holds its connection open for the watch duration, so
/// killing the child process is the stop signal.
pub struct FeedbackSession {
    subprocess: gio::Subprocess,
    // Set just before `stop()` kills `subprocess`, so the watcher task, which
    // sees the same exit, can tell a stop from `fbcli` dying on its own. Only
    // the latter is logged.
    manually_stopped: std::rc::Rc<std::cell::Cell<bool>>,
}

impl FeedbackSession {
    /// Starts emitting `event` (a feedbackd event name such as
    /// `"alarm-clock-elapsed"`) until [`FeedbackSession::stop`] is called or
    /// `watch_timeout_secs` elapses.
    ///
    /// `important` asks feedbackd to override quiet and silent profiles for
    /// this event. It takes effect only if the app is listed in feedbackd's
    /// `allow-important` setting (`org.sigxcpu.feedbackd allow-important`), a
    /// one-time system configuration, and has no effect in the Silent profile.
    ///
    /// `sound_file`, when set, overrides the theme's sound for this event with
    /// a per-alarm tone.
    pub fn start(
        event: &str,
        important: bool,
        watch_timeout_secs: u64,
        sound_file: Option<&str>,
    ) -> anyhow::Result<FeedbackSession> {
        let mut argv: Vec<String> = vec![
            "fbcli".into(),
            "--event".into(),
            event.into(),
            "--app-id".into(),
            crate::APP_ID.to_string(),
            "--watch".into(),
            watch_timeout_secs.to_string(),
        ];
        if important {
            argv.push("--important".into());
        }
        if let Some(path) = sound_file {
            argv.push("--sound-file".into());
            argv.push(path.into());
        }

        let os_argv: Vec<&std::ffi::OsStr> = argv.iter().map(std::ffi::OsStr::new).collect();
        // stdout is silenced and stderr is piped (see the watcher task below).
        // With both silenced, `fbcli` could fail (bad `--sound-file` path,
        // rejected event) with no trace, since the process still spawns.
        let subprocess = gio::Subprocess::newv(
            &os_argv,
            gio::SubprocessFlags::STDOUT_SILENCE | gio::SubprocessFlags::STDERR_PIPE,
        )
        .map_err(|err| {
            anyhow::anyhow!("failed to spawn fbcli (is feedbackd installed and in $PATH?): {err}")
        })?;

        let manually_stopped = std::rc::Rc::new(std::cell::Cell::new(false));

        // Watches for `fbcli` exiting before being asked to, the only case
        // worth logging. A normal dismiss also makes it exit (killed in
        // `stop()`) and is not treated as a failure.
        let watch_subprocess = subprocess.clone();
        let watch_manually_stopped = manually_stopped.clone();
        let watch_event = event.to_string();
        glib::MainContext::default().spawn_local(async move {
            let result = watch_subprocess.communicate_utf8_future(None).await;
            if watch_manually_stopped.get() {
                return;
            }
            match result {
                Ok((_stdout, Some(stderr))) if !stderr.trim().is_empty() => {
                    tracing::warn!(
                        event = %watch_event,
                        stderr = %stderr.trim(),
                        "fbcli exited on its own and reported an error"
                    );
                }
                Ok(_) => {
                    // Exited cleanly with nothing on stderr: most likely the
                    // `--watch` timeout expired. Not logged.
                }
                Err(err) => {
                    tracing::warn!(%err, event = %watch_event, "fbcli exited unexpectedly");
                }
            }
        });

        Ok(FeedbackSession {
            subprocess,
            manually_stopped,
        })
    }

    /// Stops the feedback by killing the `fbcli` process, which drops its
    /// D-Bus connection so feedbackd ends the event.
    pub fn stop(self) {
        self.manually_stopped.set(true);
        self.subprocess.force_exit();
    }
}

/// Fallback for when feedbackd is not running (see [`is_running`]): plays the
/// sound file with `paplay`, looping it manually until stopped or
/// `max_duration_secs` elapses. There is no haptic or LED feedback, which are
/// feedbackd-only.
pub struct DirectPlayback {
    stopped: Rc<Cell<bool>>,
    current: Rc<std::cell::RefCell<Option<gio::Subprocess>>>,
    loop_handle: glib::JoinHandle<()>,
}

impl DirectPlayback {
    pub fn start(sound_file: String, max_duration_secs: u64) -> DirectPlayback {
        let stopped = Rc::new(Cell::new(false));
        let current: Rc<std::cell::RefCell<Option<gio::Subprocess>>> =
            Rc::new(std::cell::RefCell::new(None));

        let loop_handle = {
            let stopped = stopped.clone();
            let current = current.clone();
            glib::MainContext::default().spawn_local(async move {
                let start_time = std::time::Instant::now();
                while !stopped.get() && start_time.elapsed().as_secs() < max_duration_secs {
                    let subprocess = match gio::Subprocess::newv(
                        &[std::ffi::OsStr::new("paplay"), std::ffi::OsStr::new(&sound_file)],
                        gio::SubprocessFlags::STDOUT_SILENCE | gio::SubprocessFlags::STDERR_PIPE,
                    ) {
                        Ok(subprocess) => subprocess,
                        Err(err) => {
                            tracing::warn!(%err, "failed to spawn paplay for direct playback fallback");
                            break;
                        }
                    };
                    *current.borrow_mut() = Some(subprocess.clone());

                    let result = subprocess.communicate_utf8_future(None).await;
                    if stopped.get() {
                        // Killed by `stop()` mid-playback; not logged.
                        break;
                    }
                    if let Ok((_stdout, Some(stderr))) = &result {
                        if !stderr.trim().is_empty() {
                            tracing::warn!(
                                stderr = %stderr.trim(),
                                "paplay reported an error during direct playback fallback"
                            );
                        }
                    }
                    // No pause between loops: `paplay`'s playback time paces
                    // the loop, and a spawn failure above breaks out.
                }
                *current.borrow_mut() = None;
            })
        };

        DirectPlayback {
            stopped,
            current,
            loop_handle,
        }
    }

    /// Stops playback: kills the running `paplay` and prevents another from
    /// starting.
    pub fn stop(self) {
        self.stopped.set(true);
        self.loop_handle.abort();
        if let Some(subprocess) = self.current.borrow_mut().take() {
            subprocess.force_exit();
        }
    }
}

