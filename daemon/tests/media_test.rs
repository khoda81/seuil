//! Black-box tests for the media activity monitor.
//!
//! Covers:
//! - FR-016: the silent-tab, game-audio, communication-class, and
//!   capture-side audio cases with default settings
//! - NFR-001: activity settling within 1 s of the underlying stream change
//! - FR-018: probe hangs, missing tools, and service outages hold state
//!   without fabricating stop/start edges
//!
//! The monitor is driven through its documented test hooks: the
//! `SEUIL_MEDIA_PROBE` environment variable names a script that emits
//! `pactl list sink-inputs`-style text, and `SEUIL_MEDIA_TAP` names a script
//! that receives the stream index and writes raw s16le samples to stdout.

#![allow(dead_code)]
// The env-var lock is deliberately held for the whole async test so the
// monitor polls the scripts pinned for that test; contention resolves by
// blocking in acquire, never by waiting while held.
#![allow(clippy::await_holding_lock)]

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[path = "../src/media/mod.rs"]
mod media;

use media::{MediaEvent, MediaSettings, monitor_pipewire};

/// Serializes process environment mutation across tests in this binary.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("seuil-media-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Seconds elapsed since the epoch as seen by a shell script.
const NOW_SECS: &str = "$(date +%s)";

fn write_script(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

/// A probe script that always reports one uncorked playback stream.
fn uncorked_probe(dir: &std::path::Path, role: &str, class: &str) -> PathBuf {
    write_script(
        dir,
        "probe",
        &format!(
            r#"cat <<'EOF'
Sink Input #7
	Corked: no
	Mute: no
	Volume: front-left: 65536 / 100% / 0.00 dB
	Properties:
		application.name = "test-app"
		media.class = "{class}"
		media.role = "{role}"
EOF
"#
        ),
    )
}

/// A tap script that stays silent until a trigger file appears, then emits a
/// loud square wave while the trigger file exists.
fn trigger_tap(dir: &std::path::Path) -> PathBuf {
    let trigger = dir.join("loud");
    write_script(
        dir,
        "tap",
        &format!(
            r#"while :; do
    if [ -f "{}" ]; then
        printf '\377\177\377\177\377\177\377\177'
    else
        printf '\000\000\000\000\000\000\000\000'
    fi
done
"#,
            trigger.display()
        ),
    )
}

/// A tap script emitting a fixed mid-level tone (-26 dBFS peak, 1600/32768).
fn quiet_tap(dir: &std::path::Path) -> PathBuf {
    write_script(
        dir,
        "tap",
        r#"while :; do printf '\100\006\100\006\100\006\100\006'; done
"#,
    )
}

fn default_settings() -> MediaSettings {
    MediaSettings::default()
}

/// Starts the monitor against the scripts in `dir` and returns the event
/// receiver plus the moment the monitor was started.
fn start_monitor(
    probe: &std::path::Path,
    tap: &std::path::Path,
    settings: MediaSettings,
) -> (tokio::sync::mpsc::Receiver<MediaEvent>, Instant) {
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    // SAFETY: tests in this binary serialize on ENV_LOCK; no other thread
    // reads the environment concurrently during mutation.
    unsafe {
        std::env::set_var("SEUIL_MEDIA_PROBE", probe.display().to_string());
        std::env::set_var("SEUIL_MEDIA_TAP", tap.display().to_string());
    }
    monitor_pipewire(tx, settings);
    (rx, Instant::now())
}

/// Collects events for `duration`, enforcing a silence budget when `quiet`.
async fn collect(rx: &mut tokio::sync::mpsc::Receiver<MediaEvent>, duration: Duration) -> Vec<MediaEvent> {
    let deadline = Instant::now() + duration;
    let mut events = Vec::new();
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(event)) => events.push(event),
            Ok(None) => break,
            Err(_) => break,
        }
    }
    events
}

// [FR-016 | spec-media-activity-filtering Requirement 1 Scenario "Silent tab"]
// GIVEN a RUNNING stream whose peak stays below -60 dBFS across the window,
// WHEN the monitor evaluates it, THEN it is not active and no handoff
// request event fires.
#[tokio::test]
async fn silent_running_stream_never_becomes_activity() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let dir = scratch_dir("silent");
    let probe = uncorked_probe(&dir, "music", "Stream/Output/Audio");
    let tap = trigger_tap(&dir); // trigger never created: silent throughout
    let (mut rx, started) = start_monitor(&probe, &tap, default_settings());
    let events = collect(&mut rx, Duration::from_millis(1600)).await;
    assert!(
        !events.iter().any(|e| matches!(e, MediaEvent::AudioStarted { .. })),
        "a digitally silent RUNNING stream must not produce an AudioStarted edge, got {events:?}"
    );
    assert!(
        started.elapsed() >= Duration::from_millis(1600),
        "the monitor must have been given a full evaluation window"
    );
}

// [FR-016 | FR-NFR 2.16 Scenario "Real game audio is activity"] and
// [NFR-001 | FR-NFR 3.1] GIVEN a RUNNING stream that switches from silence to
// a peak above the threshold, WHEN the monitor settles, THEN AudioStarted
// fires within 1 s of the change at default settings, exactly once.
#[tokio::test]
async fn stream_change_to_loud_settles_within_one_second_exactly_once() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let dir = scratch_dir("latency");
    let probe = uncorked_probe(&dir, "music", "Stream/Output/Audio");
    let tap = trigger_tap(&dir);
    let (mut rx, _) = start_monitor(&probe, &tap, default_settings());

    // Give the monitor a first window on silence, then flip the stream.
    tokio::time::sleep(Duration::from_millis(700)).await;
    let flip = Instant::now();
    std::fs::write(dir.join("loud"), b"1").unwrap();

    let event = tokio::time::timeout(Duration::from_secs(4), rx.recv())
        .await
        .expect("a loud stream must eventually produce an AudioStarted edge")
        .expect("channel must stay open");
    assert!(matches!(event, MediaEvent::AudioStarted { .. }), "got {event:?}");
    let settle = flip.elapsed();
    assert!(
        settle <= Duration::from_secs(1),
        "activity must settle within 1 s of the stream change, took {settle:?}"
    );

    // Exactly one media-reason edge: a further silent-then-loud window must
    // not fabricate a duplicate start without an intervening stop.
    std::fs::remove_file(dir.join("loud")).unwrap();
    let events = collect(&mut rx, Duration::from_millis(1200)).await;
    let stops = events.iter().filter(|e| matches!(e, MediaEvent::AudioStopped { .. })).count();
    assert_eq!(stops, 1, "removing the loud samples must produce one stop, got {events:?}");
    std::fs::write(dir.join("loud"), b"1").unwrap();
    let events = collect(&mut rx, Duration::from_millis(1600)).await;
    let starts = events.iter().filter(|e| matches!(e, MediaEvent::AudioStarted { .. })).count();
    assert_eq!(
        starts, 1,
        "re-adding loud samples must produce exactly one new start edge, got {events:?}"
    );
}

#[tokio::test]
async fn empty_tap_reads_hold_activity_until_measured_silence() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let dir = scratch_dir("bursts");
    let probe = uncorked_probe(&dir, "music", "Stream/Output/Audio");
    let tap = write_script(
        &dir,
        "tap",
        // 38,400 bytes is 200 ms at 48 kHz stereo s16le.
        r#"head -c 38400 /dev/zero | tr '\000' '\177'
sleep 1.5
head -c 38400 /dev/zero
sleep 5
"#,
    );
    let (mut rx, _) = start_monitor(&probe, &tap, default_settings());

    let started = tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("the loud PCM must start activity");
    assert!(matches!(started, Some(MediaEvent::AudioStarted { .. })));
    assert!(
        tokio::time::timeout(Duration::from_millis(700), rx.recv())
            .await
            .is_err(),
        "empty reads must leave the last verdict intact"
    );
    let stopped = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("enough silent PCM must stop activity");
    assert!(matches!(stopped, Some(MediaEvent::AudioStopped { .. })));
}

#[tokio::test]
async fn stalled_tap_leaves_capacity_for_other_playback() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let dir = scratch_dir("tap-failure");
    let probe = write_script(
        &dir,
        "probe",
        r#"for index in 7 8 9 10 11; do
    printf 'Sink Input #%s\n\tCorked: no\n\tMute: no\n' "$index"
done
"#,
    );
    let tap = write_script(
        &dir,
        "tap",
        r#"case "$1" in
    7) exec sleep 4 ;;
    11) tr '\000' '\177' < /dev/zero ;;
    *) cat /dev/zero ;;
esac
"#,
    );
    let (mut rx, _) = start_monitor(&probe, &tap, default_settings());
    let events = collect(&mut rx, Duration::from_secs(1)).await;
    assert!(
        matches!(events.as_slice(), [MediaEvent::AudioStarted { .. }]),
        "the loud stream must be heard after the stalled tap frees a slot, got {events:?}"
    );
}

// [FR-016 | spec-media-activity-filtering Requirement 1 Scenario "Voice chat"]
// GIVEN a communication-class stream, WHEN the monitor evaluates it, THEN it
// does not count as media activity even when its peak clears the threshold.
#[tokio::test]
async fn communication_role_stream_is_excluded() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let dir = scratch_dir("comm");
    let probe = uncorked_probe(&dir, "phone", "Stream/Output/Audio");
    let tap = trigger_tap(&dir);
    std::fs::write(dir.join("loud"), b"1").unwrap(); // loud from the start
    let (mut rx, _) = start_monitor(&probe, &tap, default_settings());
    let events = collect(&mut rx, Duration::from_millis(1600)).await;
    assert!(
        !events.iter().any(|e| matches!(e, MediaEvent::AudioStarted { .. })),
        "a communication-role stream must be excluded from media activity, got {events:?}"
    );
}

// [FR-016 | RULE-002] Boundary: capture-side streams never count as media.
#[tokio::test]
async fn capture_class_stream_is_excluded() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let dir = scratch_dir("capture");
    let probe = uncorked_probe(&dir, "music", "Stream/Input/Audio");
    let tap = trigger_tap(&dir);
    std::fs::write(dir.join("loud"), b"1").unwrap();
    let (mut rx, _) = start_monitor(&probe, &tap, default_settings());
    let events = collect(&mut rx, Duration::from_millis(1600)).await;
    assert!(
        !events.iter().any(|e| matches!(e, MediaEvent::AudioStarted { .. })),
        "a capture-class stream must be excluded from media activity, got {events:?}"
    );
}

// [FR-016 | spec-media-activity-filtering Requirement 1 Scenario "Custom
// thresholds"] Boundary: a custom threshold above the tap's peak (-26 dBFS)
// keeps the stream inactive, while the default threshold accepts the same
// peak, proving the configured value is used.
#[tokio::test]
async fn custom_threshold_is_honored_over_the_default() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let dir = scratch_dir("threshold");
    let probe = uncorked_probe(&dir, "music", "Stream/Output/Audio");
    let tap = quiet_tap(&dir); // peak around -26 dBFS

    // Custom settings: threshold above the tap peak.
    let custom = MediaSettings {
        threshold_dbfs: -20.0,
        window: Duration::from_millis(200),
        probe_timeout: Duration::from_millis(500),
    };
    let (mut rx, _) = start_monitor(&probe, &tap, custom);
    let events = collect(&mut rx, Duration::from_millis(1500)).await;
    assert!(
        !events.iter().any(|e| matches!(e, MediaEvent::AudioStarted { .. })),
        "a -26 dBFS peak must stay inactive under a -20 dBFS threshold, got {events:?}"
    );

    // Default settings on the same tap: the same peak clears -60 dBFS.
    let dir2 = scratch_dir("threshold-default");
    let probe2 = uncorked_probe(&dir2, "music", "Stream/Output/Audio");
    let (mut rx2, _) = start_monitor(&probe2, &tap, MediaSettings::default());
    let event = tokio::time::timeout(Duration::from_secs(6), rx2.recv())
        .await
        .expect("the same peak must be active under the default threshold")
        .expect("channel must stay open");
    assert!(matches!(event, MediaEvent::AudioStarted { .. }), "got {event:?}");
}

// [FR-018 | FR-NFR 2.18 Scenario "Probe tool absent"] GIVEN the probe command
// missing, WHEN the monitor runs, THEN the last known state is held and no
// stop/start edge pair is emitted.
#[tokio::test]
async fn missing_probe_tool_holds_state_without_edges() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let dir = scratch_dir("no-probe");
    let missing = dir.join("definitely-not-on-path");
    let tap = trigger_tap(&dir);
    let (mut rx, _) = start_monitor(&missing, &tap, default_settings());
    let events = collect(&mut rx, Duration::from_millis(1500)).await;
    assert!(
        events.is_empty(),
        "a missing probe must hold state and emit no edges, got {events:?}"
    );
}

// [FR-018 | FR-NFR 2.18 Scenario "Probe hangs"] GIVEN a probe that never
// returns at first, WHEN its timeout expires, THEN the tick completes and
// later ticks continue once the probe starts answering.
#[tokio::test]
async fn hanging_probe_recovers_and_later_ticks_continue() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let dir = scratch_dir("hang");
    // The probe hangs for the first 1.2 s, then answers immediately.
    let marker = dir.join("started");
    std::fs::write(&marker, b"1").unwrap();
    let probe = write_script(
        &dir,
        "probe",
        &format!(
            r#"start=$(stat -c %Y "{}")
now={NOW_SECS}
elapsed=$((now - start))
if [ "$elapsed" -lt 2 ]; then
    sleep 5
fi
echo 'Sink Input #7'
echo '	Corked: no'
echo '	Properties:'
echo '		media.role = "music"'
echo '		media.class = "Stream/Output/Audio"'
"#
        , marker.display()
        ),
    );
    let tap = trigger_tap(&dir);
    std::fs::write(dir.join("loud"), b"1").unwrap();
    let settings = MediaSettings {
        threshold_dbfs: -60.0,
        window: Duration::from_millis(200),
        probe_timeout: Duration::from_millis(400),
    };
    let (mut rx, started) = start_monitor(&probe, &tap, settings);
    // The first probes hang past their timeout; the monitor must survive.
    tokio::time::sleep(Duration::from_millis(2300)).await;
    let events = collect(&mut rx, Duration::from_millis(2500)).await;
    assert!(
        events.iter().any(|e| matches!(e, MediaEvent::AudioStarted { .. })),
        "after the hang clears, later ticks must resume and detect activity, got {events:?}"
    );
    assert!(
        !events.iter().any(|e| matches!(e, MediaEvent::AudioStopped { .. })),
        "a hung probe must not fabricate a stop edge, got {events:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(6),
        "the tick with the hang must have completed, not blocked forever"
    );
}

// [FR-018 | FR-NFR 2.18 Scenario "Service unreachable" and Scenario "PipeWire
// restart recovery"] GIVEN the probe service failing for a stretch and then
// recovering, WHEN the monitor reconnects, THEN activity evaluation resumes
// without a fabricated edge pair and the last known state is held.
#[tokio::test]
async fn service_outage_holds_state_and_recovery_does_not_fabricate_edges() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let dir = scratch_dir("outage");
    std::fs::write(dir.join("started"), b"1").unwrap();
    // Timeline: 0-0.8 s healthy RUNNING; 0.8-2.2 s probe exits 1
    // (service unreachable); 2.2 s onward healthy RUNNING again.
    let marker = dir.join("started");
    let probe = write_script(
        &dir,
        "probe",
        &format!(
            r#"start=$(stat -c %Y "{}")
now={NOW_SECS}
elapsed=$((now - start))
if [ "$elapsed" -ge 1 ] && [ "$elapsed" -lt 3 ]; then
    exit 1
fi
echo 'Sink Input #7'
echo '	Corked: no'
echo '	Properties:'
echo '		media.role = "music"'
echo '		media.class = "Stream/Output/Audio"'
"#
        , marker.display()
        ),
    );
    let tap = trigger_tap(&dir);
    std::fs::write(dir.join("loud"), b"1").unwrap();
    let settings = MediaSettings {
        threshold_dbfs: -60.0,
        window: Duration::from_millis(200),
        probe_timeout: Duration::from_millis(500),
    };
    let (mut rx, _) = start_monitor(&probe, &tap, settings);
    // Span the whole outage: healthy, outage, recovery.
    let events = collect(&mut rx, Duration::from_millis(4200)).await;
    let starts = events.iter().filter(|e| matches!(e, MediaEvent::AudioStarted { .. })).count();
    let stops = events.iter().filter(|e| matches!(e, MediaEvent::AudioStopped { .. })).count();
    assert_eq!(starts, 1, "exactly one start edge, got {events:?}");
    assert_eq!(
        stops, 0,
        "the outage must hold the active state and emit no stop edge, got {events:?}"
    );
}

// [FR-018 | FR-NFR 2.18 Scenario "Monitor task dies"] The supervised restart
// of a dead monitor task cannot be forced hermetically: it requires the inner
// task to exit or panic on demand. Live run must observe: the monitor task
// restarts after SUPERVISOR_RESTART_DELAY, an error line is logged, and no
// duplicate start edge is replayed.
#[tokio::test]
#[ignore = "needs a live monitor fault; requires forcing the inner task to panic"]
async fn monitor_death_is_restarted_by_the_supervisor() {
    let _ = monitor_pipewire;
}
