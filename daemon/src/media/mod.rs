//! Media integration: PipeWire audio activity detection and MPRIS control
//!
//! [FR-016] An uncorked, unmuted media stream changes activity after enough
//! recorded PCM is consistently above or below the configured peak threshold.
//! Capture-side and communication-class streams are excluded.
//!
//! [FR-018] Every probe is timeout-bounded. A failed or missing probe holds
//! the last known activity state and logs one persistent warning instead of
//! fabricating a stop/start edge pair. The monitor runs under a supervisor so
//! a task death is visible and the monitor restarts.

use std::collections::{HashMap, HashSet};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use futures::future::join_all;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

/// Duration bound on one probe subprocess. [FR-018]
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Period for reconciling the stream listing and draining the taps. [NFR-001]
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Delay before the supervisor restarts a monitor task that died. [FR-018]
const SUPERVISOR_RESTART_DELAY: Duration = Duration::from_secs(1);

/// Upper bound on simultaneous stream taps.
///
/// Each tap is one `parec` subprocess, so the bound keeps a stream flood from
/// exhausting subprocesses. Existing taps and the first eligible streams are
/// preferred when the bound is reached. [FR-016]
const MAX_TAPS: usize = 4;

/// Time budget for draining each tap per poll. [FR-018]
const TAP_READ_TIMEOUT: Duration = Duration::from_millis(20);

/// Give other eligible streams a slot before retrying a failed tap.
const TAP_RETRY_DELAY: Duration = Duration::from_secs(1);

/// Reclaim a slot under contention after five polls without a fragment.
/// With the requested latency, the measured A2DP tap delivered its first
/// fragment within 100 ms.
const TAP_STARTUP_GRACE: Duration = Duration::from_millis(500);

const TAP_SAMPLE_RATE: usize = 48_000;
const TAP_CHANNELS: usize = 2;
/// The default parec fragment can take two seconds to arrive. Request 40 ms
/// fragments (7,680 bytes) so a typical 8 KiB pipe can fit an entire fragment.
const TAP_FRAGMENT_MS: u64 = 40;
const TAP_FRAGMENT_BYTES: usize =
    TAP_SAMPLE_RATE * TAP_CHANNELS * size_of::<i16>() * TAP_FRAGMENT_MS as usize / 1_000;
/// Drain up to 160 ms of PCM per 100 ms poll, bounded even if the pipe is full.
const TAP_FRAGMENTS_PER_POLL: usize = 4;

/// Media state event
#[allow(dead_code)] // Fields describe the source in logs.
#[derive(Debug, Clone)]
pub enum MediaEvent {
    /// Local audio activity started
    AudioStarted { stream_name: String },
    /// Local audio activity stopped
    AudioStopped { stream_name: String },
    /// MPRIS player state changed. MPRIS is a secondary signal and never
    /// triggers a handoff on its own. [FR-016]
    PlayerStateChanged { player_name: String, playing: bool },
}

/// Runtime knobs for the activity monitor, read from config.toml. [FR-016]
#[derive(Debug, Clone, Copy)]
pub struct MediaSettings {
    /// Minimum peak in dBFS for a stream to count as active.
    pub threshold_dbfs: f32,
    /// Duration of measured PCM required above or below the threshold.
    pub window: Duration,
    /// Bound on one probe. [FR-018]
    pub probe_timeout: Duration,
}

impl Default for MediaSettings {
    fn default() -> Self {
        Self {
            threshold_dbfs: -60.0,
            window: Duration::from_millis(200),
            probe_timeout: PROBE_TIMEOUT,
        }
    }
}

/// One PipeWire playback stream as reported by `pactl list sink-inputs`.
#[derive(Debug, Clone, PartialEq)]
struct StreamSample {
    /// Sink-input index, the handle the peak tap records. [FR-016]
    index: u32,
    corked: bool,
    muted: bool,
    /// Largest channel gain as a linear factor; 1.0 is unity (0 dBFS).
    /// Diagnostic only: activity is decided by the measured peak.
    volume: f32,
    media_class: String,
    media_role: String,
}

impl Default for StreamSample {
    fn default() -> Self {
        Self {
            index: 0,
            corked: true,
            muted: false,
            volume: 1.0,
            media_class: String::new(),
            media_role: String::new(),
        }
    }
}

impl StreamSample {
    /// Linear gain expressed in dBFS. Unity gain is 0 dBFS.
    fn gain_dbfs(&self) -> f32 {
        if self.volume <= 0.0 {
            f32::NEG_INFINITY
        } else {
            20.0 * self.volume.log10()
        }
    }

    /// True when this sample is a playback stream that may count as media.
    ///
    /// Capture-side sources and communication-class roles are never media
    /// activity. [FR-016]
    fn is_media_stream(&self) -> bool {
        let class = self.media_class.to_ascii_lowercase();
        let capture = class.contains("/input")
            || class.contains("capture")
            || class.contains("source");
        let role = self.media_role.to_ascii_lowercase();
        let communication = role.contains("communication")
            || role.contains("phone")
            || role.contains("voice");
        !capture && !communication
    }

    /// Cheap prefilter that decides which streams get a peak tap.
    ///
    /// This never decides activity on its own; the verdict comes from the peak
    /// the tap measures. Capture-side sources and communication-class roles
    /// are never media activity. [FR-016]
    fn is_tap_candidate(&self) -> bool {
        !self.corked && !self.muted && self.is_media_stream()
    }
}

/// Confirms a change from successive measured PCM blocks on one stream.
/// A gap in delivery leaves the verdict and the evidence so far intact.
#[derive(Debug, Default)]
struct ActivityWindow {
    confirmed: Option<bool>,
    pending: Option<(bool, Duration)>,
}

impl ActivityWindow {
    fn observe(&mut self, above: bool, window: Duration) {
        if self.confirmed == Some(above) {
            self.pending = None;
            return;
        }
        let fragment = Duration::from_millis(TAP_FRAGMENT_MS);
        let observed = match self.pending {
            Some((value, duration)) if value == above => duration + fragment,
            _ => fragment,
        };
        if window <= observed {
            self.confirmed = Some(above);
            self.pending = None;
        } else {
            self.pending = Some((above, observed));
        }
    }
}

/// Environment override for the activity probe command.
///
/// [NFR-001] A test harness points this at a script that emits synthetic
/// `pactl list sink-inputs` text, so the timed one-second settling check can
/// drive the monitor without a real audio graph.
const PROBE_COMMAND_ENV: &str = "SEUIL_MEDIA_PROBE";

/// Environment override for the stream tap command.
///
/// [NFR-001] When set, the monitor spawns this script instead of `parec` and
/// passes the stream index as its argument. The script must keep writing raw
/// little-endian s16 samples to stdout while the stream is tapped, so the same
/// monitor logic runs for silent-tab, game-audio, and communication-class
/// scenarios without a real audio graph.
const TAP_COMMAND_ENV: &str = "SEUIL_MEDIA_TAP";

/// Why the activity probe produced no result. [FR-018]
#[derive(Debug, Clone)]
enum ProbeFailure {
    MissingTool(&'static str),
    TimedOut(Duration),
    Failed(String),
}

impl std::fmt::Display for ProbeFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingTool(tool) => write!(f, "{tool} is not installed"),
            Self::TimedOut(limit) => write!(f, "the activity probe did not answer within {limit:?}"),
            Self::Failed(message) => write!(f, "{message}"),
        }
    }
}

/// Starts the supervised media monitor.
///
/// [FR-018] The inner monitor task runs under this supervisor. A death is
/// logged at error level and the monitor restarts after a short delay; the
/// last activity edge is not replayed on restart.
pub fn monitor_pipewire(tx: mpsc::Sender<MediaEvent>, settings: MediaSettings) {
    tokio::spawn(async move {
        // [FR-018] The last activity verdict survives a monitor restart so a
        // death cannot fabricate a duplicate start or stop edge.
        let last_active = Arc::new(AtomicBool::new(false));
        loop {
            let handle = tokio::spawn(run_monitor(
                tx.clone(),
                settings,
                last_active.clone(),
            ));
            match handle.await {
                Ok(true) => break,
                Ok(false) => warn!("The media monitor stopped; restarting it"),
                Err(error) if error.is_panic() => {
                    error!("The media monitor panicked; restarting it")
                }
                Err(error) => {
                    error!("The media monitor task failed ({error}); restarting it")
                }
            }
            if tx.is_closed() {
                break;
            }
            tokio::time::sleep(SUPERVISOR_RESTART_DELAY).await;
        }
        debug!("Media monitor supervisor stopped");
    });
}

/// One monitor run. Returns true when the event receiver closed.
async fn run_monitor(
    tx: mpsc::Sender<MediaEvent>,
    settings: MediaSettings,
    last_active: Arc<AtomicBool>,
) -> bool {
    info!(
        threshold_dbfs = settings.threshold_dbfs,
        window_ms = settings.window.as_millis(),
        "Starting the PipeWire activity monitor"
    );

    let mut taps = TapPool::default();
    let mut probe_healthy = true;
    let mut poll = tokio::time::interval(POLL_INTERVAL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        poll.tick().await;
        let (observation, failure) = taps.evaluate_activity(&settings).await;
        match failure {
            Some(failure) if probe_healthy => {
                warn!(reason = %failure, "PipeWire activity measurement is degraded");
                probe_healthy = false;
            }
            None if !probe_healthy => {
                info!("The PipeWire activity probe recovered");
                probe_healthy = true;
            }
            _ => {}
        }
        if let Some(active) = observation
            && active != last_active.load(Ordering::Relaxed)
        {
            last_active.store(active, Ordering::Relaxed);
            let event = if active {
                MediaEvent::AudioStarted {
                    stream_name: "pipewire".into(),
                }
            } else {
                MediaEvent::AudioStopped {
                    stream_name: "pipewire".into(),
                }
            };
            if tx.send(event).await.is_err() {
                return true;
            }
        }
    }
}

/// Runs the activity probe with a timeout. [FR-018]
async fn probe_sink_inputs(
    timeout: Duration,
) -> std::result::Result<Vec<StreamSample>, ProbeFailure> {
    let mut command = match std::env::var(PROBE_COMMAND_ENV) {
        Ok(path) if !path.trim().is_empty() => {
            let mut command = tokio::process::Command::new(path);
            command.arg("sink-inputs");
            command
        }
        _ => {
            let mut command = tokio::process::Command::new("pactl");
            command.args(["list", "sink-inputs"]);
            command
        }
    };
    let output = tokio::time::timeout(timeout, command.kill_on_drop(true).output())
        .await
        .map_err(|_| ProbeFailure::TimedOut(timeout))?
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => ProbeFailure::MissingTool("pactl"),
            _ => ProbeFailure::Failed(error.to_string()),
        })?;

    let stderr = String::from_utf8_lossy(&output.stderr);
    let lowered = stderr.to_ascii_lowercase();
    // PipeWire can be down while pactl still exits zero; treat a connection
    // failure as an unhealthy probe rather than an empty stream list.
    if !output.status.success()
        || lowered.contains("connection failure")
        || lowered.contains("failed to connect")
    {
        let reason = if stderr.trim().is_empty() {
            format!("pactl exited with {}", output.status)
        } else {
            stderr.trim().to_string()
        };
        return Err(ProbeFailure::Failed(reason));
    }

    Ok(parse_sink_inputs(&String::from_utf8_lossy(&output.stdout)))
}

/// Parses the text form of `pactl list sink-inputs`.
fn parse_sink_inputs(text: &str) -> Vec<StreamSample> {
    let mut samples = Vec::new();
    let mut current: Option<StreamSample> = None;

    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(value) = trimmed.strip_prefix("Sink Input #") {
            if let Some(sample) = current.take() {
                samples.push(sample);
            }
            current = value.trim().parse().ok().map(|index| StreamSample {
                index,
                ..StreamSample::default()
            });
            continue;
        }
        let Some(sample) = current.as_mut() else {
            continue;
        };

        // Sink inputs expose Corked, while State belongs to the sink.
        if let Some(value) = trimmed.strip_prefix("Corked:") {
            sample.corked = !value.trim().eq_ignore_ascii_case("no");
        } else if let Some(value) = trimmed.strip_prefix("Mute:") {
            sample.muted = value.trim().eq_ignore_ascii_case("yes");
        } else if let Some(value) = trimmed.strip_prefix("Volume:") {
            if let Some(volume) = parse_volume(value) {
                sample.volume = volume;
            }
        } else if let Some(value) = trimmed.strip_prefix("media.class =") {
            sample.media_class = unquote(value);
        } else if let Some(value) = trimmed.strip_prefix("media.role =") {
            sample.media_role = unquote(value);
        }
    }

    if let Some(sample) = current {
        samples.push(sample);
    }
    samples
}

/// Reads the largest channel percentage from a `Volume:` line as a factor.
fn parse_volume(value: &str) -> Option<f32> {
    let mut loudest: Option<f32> = None;
    for token in value.split_whitespace() {
        if let Some(percent) = token.strip_suffix('%')
            && let Ok(percent) = percent.parse::<f32>()
        {
            let gain = percent / 100.0;
            loudest = Some(loudest.map_or(gain, |current| current.max(gain)));
        }
    }
    loudest
}

/// Strips the surrounding quotes from a pactl property value.
fn unquote(value: &str) -> String {
    value.trim().trim_matches('"').to_string()
}

/// Owns the live peak taps and reconciles them with the stream listing.
///
/// [FR-016] Taps survive across polls and close when their stream leaves the
/// listing, is corked, or is muted.
#[derive(Default)]
struct TapPool {
    streams: HashMap<u32, MonitoredStream>,
    cap_logged: bool,
}

impl TapPool {
    /// Returns the verdict and any failure affecting how much was measured.
    async fn evaluate_activity(
        &mut self,
        settings: &MediaSettings,
    ) -> (Option<bool>, Option<ProbeFailure>) {
        let streams = match probe_sink_inputs(settings.probe_timeout).await {
            Ok(streams) => streams,
            Err(failure) => {
                // Keep the verdicts. Discard buffered PCM from before the outage.
                for stream in self.streams.values_mut() {
                    stream.tap = TapStatus::Closed;
                    stream.activity.pending = None;
                }
                return (None, Some(failure));
            }
        };
        let candidates: Vec<&StreamSample> = streams
            .iter()
            .filter(|stream| stream.is_tap_candidate())
            .collect();
        let candidate_indices: HashSet<u32> =
            candidates.iter().map(|stream| stream.index).collect();
        let had_active_stream = self.streams.values().any(MonitoredStream::is_active);

        self.streams
            .retain(|index, _| candidate_indices.contains(index));
        let mut open_taps = self
            .streams
            .values()
            .filter(|stream| matches!(stream.tap, TapStatus::Open(_)))
            .count();
        let now = Instant::now();

        for candidate in &candidates {
            if open_taps == MAX_TAPS {
                break;
            }
            let stream = self.streams.entry(candidate.index).or_default();
            match &stream.tap {
                TapStatus::Open(_) => continue,
                TapStatus::Retry { after, .. } if now < *after => continue,
                _ => {}
            }
            match StreamTap::spawn(candidate) {
                Ok(tap) => {
                    stream.tap = TapStatus::Open(Box::new(tap));
                    open_taps += 1;
                }
                Err(failure) => stream.failed(failure),
            }
        }

        let capped = open_taps == MAX_TAPS && MAX_TAPS < candidates.len();
        if capped {
            if !self.cap_logged {
                debug!(
                    max_taps = MAX_TAPS,
                    candidates = candidates.len(),
                    "Tap limit reached; extra playback streams are not measured"
                );
                self.cap_logged = true;
            }
        } else {
            self.cap_logged = false;
        }

        // One read budget for all the taps, even when several pipes are empty.
        join_all(
            self.streams
                .iter_mut()
                .map(|(&index, stream)| stream.sample(index, settings, capped)),
        )
        .await;

        let activity = if self.streams.values().any(MonitoredStream::is_active) {
            Some(true)
        } else if had_active_stream
            || candidates.is_empty()
            || self
                .streams
                .values()
                .all(|stream| stream.activity.confirmed == Some(false))
        {
            Some(false)
        } else {
            None
        };
        let failure = self.streams.values().find_map(|stream| match &stream.tap {
            TapStatus::Retry { failure, .. } => Some(failure.clone()),
            _ => None,
        });
        (activity, failure)
    }
}

/// Playback evidence belongs to the sink input across recorder restarts.
#[derive(Default)]
struct MonitoredStream {
    activity: ActivityWindow,
    tap: TapStatus,
}

#[derive(Default)]
enum TapStatus {
    #[default]
    Closed,
    Open(Box<StreamTap>),
    Retry {
        after: Instant,
        failure: ProbeFailure,
    },
}

impl MonitoredStream {
    fn is_active(&self) -> bool {
        self.activity.confirmed == Some(true)
    }

    fn failed(&mut self, failure: ProbeFailure) {
        self.tap = TapStatus::Retry {
            after: Instant::now() + TAP_RETRY_DELAY,
            failure,
        };
        self.activity.pending = None;
    }

    async fn sample(&mut self, index: u32, settings: &MediaSettings, capped: bool) {
        let TapStatus::Open(tap) = &mut self.tap else {
            return;
        };
        if let Err(error) = tap.sample_activity(&mut self.activity, settings).await {
            self.failed(ProbeFailure::Failed(format!(
                "stream tap #{index}: {error}"
            )));
        } else if capped
            && tap
                .first_fragment_due
                .is_some_and(|due| due <= Instant::now())
        {
            self.failed(ProbeFailure::Failed(format!(
                "stream tap #{index} produced no PCM in {TAP_STARTUP_GRACE:?}"
            )));
        }
    }
}

/// One live `parec` tap over a playback stream.
struct StreamTap {
    _child: tokio::process::Child,
    stdout: tokio::process::ChildStdout,
    fragment: Vec<u8>,
    filled: usize,
    first_fragment_due: Option<Instant>,
}

impl StreamTap {
    /// Spawns a tap for `stream` and starts reading its raw samples. [FR-016]
    fn spawn(stream: &StreamSample) -> std::result::Result<Self, ProbeFailure> {
        let (mut command, tool) = tap_command(stream.index);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => ProbeFailure::MissingTool(tool),
            _ => ProbeFailure::Failed(error.to_string()),
        })?;
        let stdout = child
            .stdout
            .take()
            .expect("the stream tap's stdout is piped");
        debug!(
            stream_index = stream.index,
            gain_dbfs = stream.gain_dbfs(),
            "Opened a stream tap"
        );
        Ok(Self {
            _child: child,
            stdout,
            fragment: vec![0; TAP_FRAGMENT_BYTES],
            filled: 0,
            first_fragment_due: Some(Instant::now() + TAP_STARTUP_GRACE),
        })
    }

    /// A partial read stays in the buffer for the next poll. Only complete PCM
    /// blocks provide timing evidence, regardless of how the pipe splits them.
    async fn sample_activity(
        &mut self,
        activity: &mut ActivityWindow,
        settings: &MediaSettings,
    ) -> std::io::Result<()> {
        tokio::time::timeout(TAP_READ_TIMEOUT, async {
            for _ in 0..TAP_FRAGMENTS_PER_POLL {
                while self.filled < self.fragment.len() {
                    let read = self.stdout.read(&mut self.fragment[self.filled..]).await?;
                    if read == 0 {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "the tap closed its output",
                        ));
                    }
                    self.filled += read;
                }
                activity.observe(
                    settings.threshold_dbfs <= peak_dbfs(&self.fragment),
                    settings.window,
                );
                self.filled = 0;
                self.first_fragment_due = None;
            }
            Ok(())
        })
        .await
        .unwrap_or(Ok(()))
    }
}

/// Builds the tap command for one stream index and names the tool for errors.
///
/// `SEUIL_MEDIA_TAP` replaces `parec` for the test harness. [NFR-001]
fn tap_command(index: u32) -> (tokio::process::Command, &'static str) {
    match std::env::var(TAP_COMMAND_ENV) {
        Ok(path) if !path.trim().is_empty() => {
            let mut command = tokio::process::Command::new(path);
            command.arg(index.to_string());
            (command, "the stream tap command")
        }
        _ => {
            let mut command = tokio::process::Command::new("parec");
            command.args([
                format!("--monitor-stream={index}"),
                "--raw".to_string(),
                "--format=s16le".to_string(),
                format!("--rate={TAP_SAMPLE_RATE}"),
                format!("--channels={TAP_CHANNELS}"),
                format!("--latency-msec={TAP_FRAGMENT_MS}"),
            ]);
            (command, "parec")
        }
    }
}

/// Peak amplitude of little-endian signed 16-bit samples as dBFS.
///
/// Full scale is 0 dBFS and digital silence is `-inf` dBFS. [FR-016]
fn peak_dbfs(samples_le: &[u8]) -> f32 {
    let peak = samples_le
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u32::from(i16::from_le_bytes(*pair).unsigned_abs()))
        .max()
        .unwrap_or(0);
    if peak == 0 {
        f32::NEG_INFINITY
    } else {
        20.0 * (peak as f32 / 32768.0).log10()
    }
}

/// Control MPRIS media players
pub struct MprisController;

/// Result of a pause request across every MPRIS player. [FR-019]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PauseOutcome {
    pub attempted: usize,
    pub failed: usize,
}

impl PauseOutcome {
    /// True when every player that was asked to pause accepted it.
    pub fn succeeded(&self) -> bool {
        self.failed == 0
    }
}

impl MprisController {
    /// Pause all active MPRIS players.
    ///
    /// [FR-019] The caller gets a per-player failure count so a release reply
    /// can report a pause that did not take effect instead of claiming success.
    pub async fn pause_all() -> Result<PauseOutcome> {
        info!("Pausing all MPRIS players");

        let players = mpris::PlayerFinder::new()
            .map_err(|e| anyhow::anyhow!("Failed to find MPRIS players: {}", e))?;

        let all_players = players
            .find_all()
            .map_err(|e| anyhow::anyhow!("Failed to enumerate MPRIS players: {}", e))?;

        let mut outcome = PauseOutcome {
            attempted: all_players.len(),
            failed: 0,
        };
        for player in &all_players {
            match player.pause() {
                Ok(()) => debug!("Paused MPRIS player: {}", player.identity()),
                Err(e) => {
                    outcome.failed += 1;
                    warn!("Failed to pause {}: {}", player.identity(), e);
                }
            }
        }

        if outcome.failed > 0 {
            warn!(
                attempted = outcome.attempted,
                failed = outcome.failed,
                "Some MPRIS players did not pause"
            );
        }
        Ok(outcome)
    }

    /// Resume all paused MPRIS players
    #[allow(dead_code)] // Symmetric helper for future resume-on-return flows.
    pub async fn resume_all() -> Result<()> {
        info!("Resuming all MPRIS players");

        let players = mpris::PlayerFinder::new()
            .map_err(|e| anyhow::anyhow!("Failed to find MPRIS players: {}", e))?;

        let all_players = players
            .find_all()
            .map_err(|e| anyhow::anyhow!("Failed to enumerate MPRIS players: {}", e))?;

        for player in &all_players {
            match player.play() {
                Ok(()) => debug!("Resumed MPRIS player: {}", player.identity()),
                Err(e) => warn!("Failed to resume {}: {}", player.identity(), e),
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> StreamSample {
        StreamSample {
            index: 1,
            corked: false,
            media_class: "Stream/Output/Audio".into(),
            media_role: "Music".into(),
            ..StreamSample::default()
        }
    }

    #[test]
    fn muted_and_corked_streams_are_not_tapped() {
        let playable = sample();
        let muted = StreamSample {
            muted: true,
            ..playable.clone()
        };
        let corked = StreamSample {
            corked: true,
            ..playable.clone()
        };
        assert!(playable.is_tap_candidate());
        assert!(!muted.is_tap_candidate());
        assert!(!corked.is_tap_candidate());
    }

    #[test]
    fn peak_is_measured_from_raw_s16le_samples() {
        // [FR-016] Digital silence, full scale, and half scale in dBFS.
        assert_eq!(peak_dbfs(&[0, 0, 0, 0]), f32::NEG_INFINITY);
        // 0x7fff is full scale and -32768 is also full scale.
        assert!(peak_dbfs(&[0xff, 0x7f]).abs() < 0.001);
        assert!(peak_dbfs(&[0x00, 0x80, 0x00, 0x00]).abs() < 0.001);
        // 0x4000 is half scale, about -6.02 dBFS.
        assert!((peak_dbfs(&[0x00, 0x40]) + 6.02).abs() < 0.05);
    }

    #[test]
    fn capture_and_communication_streams_are_excluded() {
        let capture = StreamSample {
            media_class: "Stream/Input/Audio".into(),
            ..sample()
        };
        let communication = StreamSample {
            media_role: "Communication".into(),
            ..sample()
        };
        assert!(!capture.is_media_stream());
        assert!(!communication.is_media_stream());
        assert!(sample().is_media_stream());
    }

    #[test]
    fn both_transitions_require_the_whole_pcm_window() {
        let window = Duration::from_millis(200);
        let mut activity = ActivityWindow::default();
        for _ in 0..4 {
            activity.observe(true, window);
            assert_eq!(activity.confirmed, None);
        }
        activity.observe(true, window);
        assert_eq!(activity.confirmed, Some(true));

        for _ in 0..4 {
            activity.observe(false, window);
            assert_eq!(activity.confirmed, Some(true));
        }
        activity.observe(false, window);
        assert_eq!(activity.confirmed, Some(false));
    }

    #[test]
    fn a_brief_reversal_does_not_carry_over() {
        let window = Duration::from_millis(200);
        let mut activity = ActivityWindow::default();
        for above in [true, true, true, true, false, true, true, true, true] {
            activity.observe(above, window);
            assert_eq!(activity.confirmed, None);
        }
    }

    #[test]
    fn parses_real_sink_inputs_without_a_state_field() {
        let text = "\
Sink Input #12
\tCorked: no
\tMute: no
\tVolume: front-left: 65536 / 100% / 0.00 dB,   front-right: 32768 / 50% / -6.02 dB
\tProperties:
\t\tmedia.class = \"Stream/Output/Audio\"
\t\tmedia.role = \"Game\"
Sink Input #13
\tCorked: yes
\tMute: no
";
        let samples = parse_sink_inputs(text);
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0].index, 12);
        assert!(samples[0].is_tap_candidate());
        assert!(!samples[1].is_tap_candidate());
        assert!(!samples[0].muted);
        assert!((samples[0].volume - 1.0).abs() < f32::EPSILON);
        assert_eq!(samples[0].media_class, "Stream/Output/Audio");
        assert_eq!(samples[0].media_role, "Game");
    }
}
