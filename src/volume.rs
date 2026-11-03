
/// Runs `argv[0]` to completion, discarding output, and errors if it exits
/// non-zero. For fire-and-forget `pactl` and `amixer` calls.
async fn run_status(argv: &[&str]) -> anyhow::Result<()> {
    let os_argv: Vec<&std::ffi::OsStr> = argv.iter().map(std::ffi::OsStr::new).collect();
    let subprocess = gio::Subprocess::newv(
        &os_argv,
        gio::SubprocessFlags::STDOUT_SILENCE | gio::SubprocessFlags::STDERR_PIPE,
    )
    .map_err(|err| anyhow::anyhow!("failed to spawn {}: {err}", argv[0]))?;
    // Command line and stderr are kept, since the exit status does not say
    // which call failed.
    let (_, stderr) = subprocess
        .communicate_utf8_future(None)
        .await
        .map_err(|err| anyhow::anyhow!("failed to run `{}`: {err}", argv.join(" ")))?;
    if !subprocess.is_successful() {
        let stderr = stderr.map(|s| s.trim().to_owned()).unwrap_or_default();
        anyhow::bail!("`{}` failed: {stderr}", argv.join(" "));
    }
    Ok(())
}

/// Runs a subprocess and captures stdout as a `String`, for `pactl` calls
/// whose output needs parsing.
async fn run_capture(argv: &[&str]) -> anyhow::Result<String> {
    let os_argv: Vec<&std::ffi::OsStr> = argv.iter().map(std::ffi::OsStr::new).collect();
    let subprocess = gio::Subprocess::newv(&os_argv, gio::SubprocessFlags::STDOUT_PIPE)
        .map_err(|err| anyhow::anyhow!("failed to spawn {}: {err}", argv[0]))?;
    let (stdout, _stderr) = subprocess
        .communicate_utf8_future(None)
        .await
        .map_err(|err| anyhow::anyhow!("failed to read output of {}: {err}", argv[0]))?;
    Ok(stdout.map(|s| s.to_string()).unwrap_or_default())
}

/// Thin wrapper over the `pactl` and `amixer` CLIs. The CLI surface is stable
/// and works on FuriOS/Phosh; a client library binding would add a heavy
/// dependency for a handful of commands per alarm.
pub struct SinkControl;

#[derive(Debug, Clone)]
pub struct SinkState {
    pub sink_name: String,
    pub volume_percent: u8,
    pub muted: bool,
    /// The sink's active port at snapshot time, e.g. `"output-earpiece"`;
    /// `None` if the sink has no ports (Bluetooth, Null Output). Restoring it
    /// undoes `force_speaker`'s port change (see [`SinkControl::restore`]).
    pub port: Option<String>,
}

/// A live `module-combine-sink` load, returned by
/// [`SinkControl::enable_all_sinks`] and consumed by
/// [`SinkControl::disable_all_sinks`] to unload that module.
#[derive(Debug, Clone)]
pub struct CombinedSinkHandle {
    module_index: u32,
    pub sink_name: String,
}

impl SinkControl {
    /// Snapshots the default sink's name, volume, mute state and active port
    /// for restoring when the alarm stops.
    pub async fn snapshot() -> anyhow::Result<SinkState> {
        let sink_name = Self::default_sink_name().await?;
        let volume_percent = Self::get_volume(&sink_name).await?;
        let muted = Self::get_mute(&sink_name).await?;
        let port = Self::current_port(&sink_name).await.unwrap_or_else(|err| {
            tracing::warn!(%err, sink = %sink_name, "failed to read current sink port; won't be restored");
            None
        });
        Ok(SinkState {
            sink_name,
            volume_percent,
            muted,
            port,
        })
    }

    /// The sink's active port, e.g. `"output-earpiece"`; `Ok(None)` for a sink
    /// with no ports (Bluetooth, Null Output). `pactl` has no single-value
    /// query for this, so the JSON sink listing is parsed for one field of one
    /// sink.
    pub async fn current_port(sink: &str) -> anyhow::Result<Option<String>> {
        let stdout = run_capture(&["pactl", "-f", "json", "list", "sinks"]).await?;
        let sinks: serde_json::Value = serde_json::from_str(&stdout)
            .map_err(|err| anyhow::anyhow!("failed to parse `pactl -f json list sinks`: {err}"))?;
        let Some(entries) = sinks.as_array() else {
            anyhow::bail!("expected a JSON array from `pactl -f json list sinks`");
        };
        for entry in entries {
            if entry.get("name").and_then(|v| v.as_str()) == Some(sink) {
                return Ok(entry
                    .get("active_port")
                    .and_then(|v| v.as_str())
                    .map(str::to_string));
            }
        }
        anyhow::bail!("sink '{sink}' not found in `pactl -f json list sinks` output")
    }

    /// Restores everything `snapshot` captured: volume, mute and the active
    /// port, which undoes `force_speaker`'s port change when the alarm started
    /// on a different port of the same sink.
    pub async fn restore(state: &SinkState) -> anyhow::Result<()> {
        // Every field is attempted; the first error is returned.
        let volume = Self::set_volume(&state.sink_name, state.volume_percent).await;
        let mute = Self::set_mute(&state.sink_name, state.muted).await;
        // Skipped when the port is unchanged: BlueZ sinks report a port but
        // reject `set-sink-port` with "Missing implementation".
        let port = match &state.port {
            Some(port)
                if Self::current_port(&state.sink_name).await.ok().flatten().as_deref()
                    != Some(port.as_str()) =>
            {
                run_status(&["pactl", "set-sink-port", &state.sink_name, port]).await
            }
            _ => Ok(()),
        };
        volume.and(mute).and(port)
    }

    pub async fn default_sink_name() -> anyhow::Result<String> {
        let stdout = run_capture(&["pactl", "info"]).await?;
        for line in stdout.lines() {
            if let Some(name) = line.strip_prefix("Default Sink: ") {
                return Ok(name.trim().to_string());
            }
        }
        anyhow::bail!("could not find 'Default Sink:' in `pactl info` output")
    }

    pub async fn get_volume(sink: &str) -> anyhow::Result<u8> {
        let stdout = run_capture(&["pactl", "get-sink-volume", sink]).await?;
        // pactl prints e.g. "Volume: front-left: 45 / 45% / ...". The first
        // "<digits>%" is used.
        for token in stdout.split_whitespace() {
            if let Some(pct) = token.strip_suffix('%') {
                if let Ok(v) = pct.parse::<u8>() {
                    return Ok(v);
                }
            }
        }
        anyhow::bail!("could not parse volume percentage from `pactl get-sink-volume` output")
    }

    pub async fn get_mute(sink: &str) -> anyhow::Result<bool> {
        let stdout = run_capture(&["pactl", "get-sink-mute", sink]).await?;
        Ok(stdout.contains("yes"))
    }

    pub async fn set_volume(sink: &str, percent: u8) -> anyhow::Result<()> {
        run_status(&["pactl", "set-sink-volume", sink, &format!("{percent}%")]).await
    }

    pub async fn set_mute(sink: &str, muted: bool) -> anyhow::Result<()> {
        run_status(&[
            "pactl",
            "set-sink-mute",
            sink,
            if muted { "1" } else { "0" },
        ])
        .await
    }

    /// Switches on the internal speaker amp. On the FLX1S and similar phones
    /// the amp has its own mixer control, which PulseAudio/PipeWire does not
    /// reliably flip, so volume alone is not enough.
    pub async fn ensure_speaker_amp_on() -> anyhow::Result<()> {
        // Best-effort: some devices lack this control, and a failure should
        // not abort the ramp.
        let _ = run_status(&["amixer", "sset", "Ext_Speaker_Amp", "on"]).await;
        Ok(())
    }

    /// Forces audio output to `speaker_sink`: every other sink and in-flight
    /// stream is muted and rerouted first (mute all, set default, move
    /// sink-inputs, unsuspend, unmute). PulseAudio/PipeWire have no single
    /// call that reliably routes one stream to the speaker across route
    /// changes mid-ramp.
    ///
    /// `speaker_sink` is a device-specific sink name supplied by
    /// `settings::resolve_speaker_destination`.
    ///
    /// `speaker_port` is needed because one hardware sink can expose several
    /// ports (earpiece, speaker, wired headset), and setting the sink does not
    /// select one. `None` skips the port, for sinks without ports (Bluetooth,
    /// Null Output).
    pub async fn force_speaker(speaker_sink: &str, speaker_port: Option<&str>) -> anyhow::Result<()> {
        for sink in Self::list_sink_names().await? {
            Self::set_mute(&sink, true).await?;
        }

        run_status(&["pactl", "set-default-sink", speaker_sink]).await?;
        if let Some(port) = speaker_port {
            run_status(&["pactl", "set-sink-port", speaker_sink, port]).await?;
        }
        glib::timeout_future(std::time::Duration::from_millis(100)).await;

        for input_id in Self::list_sink_input_ids().await? {
            run_status(&["pactl", "move-sink-input", &input_id, speaker_sink]).await?;
        }

        run_status(&["pactl", "suspend-sink", speaker_sink, "0"]).await?;
        Self::set_mute(speaker_sink, false).await?;
        Self::ensure_speaker_amp_on().await?;

        Ok(())
    }

    /// Undoes [`SinkControl::force_speaker`] by pointing the default sink back
    /// at `original_sink`. The per-sink mutes need no replay:
    /// [`SinkControl::restore`] resets volume and mute for the alarm's sink.
    pub async fn restore_default_sink(original_sink: &str) -> anyhow::Result<()> {
        run_status(&["pactl", "set-default-sink", original_sink]).await
    }

    /// Combines every available sink into one virtual sink with
    /// `module-combine-sink` and makes it the default, so the alarm plays out
    /// of all of them at once.
    ///
    /// Returns `Ok(None)` with zero or one sinks, since there is nothing to
    /// fan out to; the caller then plays on the active sink.
    pub async fn enable_all_sinks() -> anyhow::Result<Option<CombinedSinkHandle>> {
        let sinks = Self::list_sink_names().await?;
        if sinks.len() < 2 {
            return Ok(None);
        }

        // Fixed name: if an earlier run crashed mid-alarm and left a combined
        // sink behind, `pactl load-module` fails cleanly (name taken) instead
        // of orphaned modules accumulating across restarts.
        const SINK_NAME: &str = "cajole_alarm_all_sinks";

        let stdout = run_capture(&[
            "pactl",
            "load-module",
            "module-combine-sink",
            &format!("sink_name={SINK_NAME}"),
            &format!("slaves={}", sinks.join(",")),
        ])
        .await?;

        let module_index: u32 = stdout
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("pactl did not return a module index"))?;

        run_status(&["pactl", "set-default-sink", SINK_NAME]).await?;
        // Gives the combine sink time to come up before its volume is set.
        glib::timeout_future(std::time::Duration::from_millis(150)).await;

        Ok(Some(CombinedSinkHandle {
            module_index,
            sink_name: SINK_NAME.to_string(),
        }))
    }

    /// Undoes [`SinkControl::enable_all_sinks`]: restores the default sink,
    /// then unloads the combine-sink module by the index returned at load.
    /// Unloading by name could match a combine-sink module created by
    /// something else.
    pub async fn disable_all_sinks(
        handle: CombinedSinkHandle,
        original_default_sink: &str,
    ) -> anyhow::Result<()> {
        run_status(&["pactl", "set-default-sink", original_default_sink]).await?;
        run_status(&["pactl", "unload-module", &handle.module_index.to_string()]).await?;
        Ok(())
    }

    async fn list_sink_names() -> anyhow::Result<Vec<String>> {
        let stdout = run_capture(&["pactl", "list", "sinks", "short"]).await?;
        Ok(stdout
            .lines()
            .filter_map(|line| line.split_whitespace().nth(1))
            .map(str::to_string)
            .collect())
    }

    async fn list_sink_input_ids() -> anyhow::Result<Vec<String>> {
        let stdout = run_capture(&["pactl", "list", "sink-inputs", "short"]).await?;
        Ok(stdout
            .lines()
            .filter_map(|line| line.split_whitespace().next())
            .map(str::to_string)
            .collect())
    }
}


/// Runs a volume ramp from `start_percent` to `end_percent` over
/// `duration_secs` using [`crate::ramp`]. Spawned with
/// `glib::MainContext::spawn_local` and cancelled through the returned
/// `glib::JoinHandle` on dismiss or snooze; the abort lands between steps.
///
/// Mute state and the speaker amp are left to
/// [`RingSession`](crate::ring::RingSession), which sets them once whether or
/// not a volume ramp exists.
pub async fn run_ramp(
    sink: &str,
    start_percent: u8,
    end_percent: u8,
    duration_secs: u32,
) -> anyhow::Result<()> {
    let interval = crate::ramp::step_interval(start_percent, end_percent, duration_secs);
    for step in crate::ramp::steps(start_percent, end_percent, duration_secs) {
        SinkControl::set_volume(sink, step).await?;
        if !interval.is_zero() {
            glib::timeout_future(interval).await;
        }
    }

    Ok(())
}

