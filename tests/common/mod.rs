//! Shared fixtures for the integration suites: synthetic configs, observations and runners.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use dead_air::config::{EXAMPLE_TOML, parse_config_text};
use dead_air::model::*;
use dead_air::process::{ChildResult, RunOptions};
use serde_json::{Value, json};

pub const PROGRESS_2S: &str = "frame=20\nout_time_us=2000000\nprogress=end\n";

/// The starter config with a short sample and 1 s detector windows (Python `test_config`).
pub fn test_config(sample_s: f64) -> AppConfig {
    let mut config = parse_config_text(EXAMPLE_TOML).expect("example parses");
    config.watch.sample_s = sample_s;
    config.streams[0].freeze_window_s = sample_s.min(1.0);
    config.streams[0].silence_window_s = sample_s.min(1.0);
    config
}

/// Python `test_incidents.config`.
pub fn incident_config(min_bad: i64, unhealthy: f64, recovery: i64, max_gap: f64, unknown_after: f64) -> AppConfig {
    let mut config = test_config(2.0);
    let stream = &mut config.streams[0];
    stream.min_bad_samples = min_bad;
    stream.unhealthy_after_s = unhealthy;
    stream.recovery_samples = recovery;
    stream.max_gap_s = max_gap;
    stream.unknown_notice_after_s = unknown_after;
    config
}

pub fn default_incident_config() -> AppConfig {
    incident_config(2, 10.0, 2, 180.0, 600.0)
}

pub fn healthy_audio() -> DimensionEvidence {
    DimensionEvidence::decoded("HEALTHY", &["AUDIO_DECODED_NO_EVENT"], 2.0, 2.0)
}

pub fn healthy_video() -> DimensionEvidence {
    DimensionEvidence::decoded("HEALTHY", &["VIDEO_DECODED_NO_EVENT"], 2.0, 2.0)
}

pub fn silence() -> DimensionEvidence {
    DimensionEvidence::decoded("UNHEALTHY", &["AUDIO_SILENCE"], 2.0, 2.0).with_intervals(vec![Interval {
        start_s: 0.25,
        end_s: 1.5,
        open_at_sample_end: false,
    }])
}

pub fn freeze() -> DimensionEvidence {
    DimensionEvidence::decoded("UNHEALTHY", &["VIDEO_FREEZE"], 2.0, 2.0).with_intervals(vec![Interval {
        start_s: 0.25,
        end_s: 1.5,
        open_at_sample_end: false,
    }])
}

pub fn unknown_dim(reason: &str) -> DimensionEvidence {
    DimensionEvidence::new("UNKNOWN", &[reason])
}

/// Python `test_incidents.observation`.
pub fn observation(
    status: &str,
    audio: Option<DimensionEvidence>,
    video: Option<DimensionEvidence>,
    reasons: &[&str],
    at: &str,
) -> Observation {
    Observation {
        stream_id: "sample-channel".into(),
        observed_at: at.into(),
        status: status.into(),
        reason_codes: reasons.iter().map(|r| r.to_string()).collect(),
        audio: audio.unwrap_or_else(healthy_audio),
        video: video.unwrap_or_else(healthy_video),
    }
}

pub fn bad_silence() -> Observation {
    observation("UNHEALTHY", Some(silence()), None, &["AUDIO_SILENCE"], "2026-10-23T00:00:00Z")
}

pub fn healthy(at: &str) -> Observation {
    observation("HEALTHY", None, None, &[], at)
}

pub fn unknown_both() -> Observation {
    observation(
        "UNKNOWN",
        Some(unknown_dim("AUDIO_UNKNOWN")),
        Some(unknown_dim("VIDEO_UNKNOWN")),
        &[],
        "2026-10-23T00:00:00Z",
    )
}

/// A private temporary directory removed on drop.
pub struct Temp(pub PathBuf);

impl Temp {
    pub fn new() -> Self {
        let path = std::env::temp_dir().join(format!("dead-air-test-{}", dead_air::random::uuid4_hex()));
        std::fs::create_dir_all(&path).unwrap();
        Temp(path)
    }
    pub fn path(&self) -> &Path {
        &self.0
    }
    pub fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Default for Temp {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Python `probe_json` for synthetic ffprobe output.
pub struct Probe {
    pub audio_s: f64,
    pub audio_start: f64,
    pub video_s: f64,
    pub video_gap: Option<(f64, f64)>,
    pub include_audio: bool,
    pub include_video: bool,
    pub format_duration_s: f64,
}

impl Default for Probe {
    fn default() -> Self {
        Probe {
            audio_s: 2.0,
            audio_start: 0.0,
            video_s: 2.0,
            video_gap: None,
            include_audio: true,
            include_video: true,
            format_duration_s: 2.0,
        }
    }
}

impl Probe {
    pub fn json(&self) -> String {
        let mut streams = Vec::new();
        let mut frames = Vec::new();
        if self.include_video {
            let index = streams.len();
            streams.push(json!({"index": index, "codec_type": "video", "time_base": "1/1000"}));
            for i in 0..((self.video_s * 10.0).round() as i64) {
                let start = i as f64 / 10.0;
                if let Some((a, b)) = self.video_gap {
                    if a <= start && start < b {
                        continue;
                    }
                }
                frames.push(json!({"stream_index": index, "pts_time": format!("{start}"), "duration_time": "0.1"}));
            }
        }
        if self.include_audio {
            let index = streams.len();
            streams.push(json!({"index": index, "codec_type": "audio", "sample_rate": "48000", "time_base": "1/1000"}));
            frames.push(json!({"stream_index": index, "pts_time": format!("{}", self.audio_start), "nb_samples": (self.audio_s * 48000.0).round() as i64}));
        }
        json!({"streams": streams, "format": {"start_time": "0", "duration": format!("{}", self.format_duration_s)}, "frames": frames})
            .to_string()
    }
}

pub fn probe_av() -> String {
    Probe::default().json()
}

/// A fake runner: ffprobe calls get `probe`; decode calls get `progress` and `logs`.
pub struct InspectorRunner {
    pub probe: String,
    pub progress: String,
    pub logs: String,
    pub probe_rc: i32,
    pub decode_rc: i32,
    pub calls: Mutex<Vec<Vec<String>>>,
}

impl InspectorRunner {
    pub fn new(probe: String) -> Self {
        InspectorRunner {
            probe,
            progress: PROGRESS_2S.into(),
            logs: String::new(),
            probe_rc: 0,
            decode_rc: 0,
            calls: Mutex::new(Vec::new()),
        }
    }
    pub fn logs(mut self, logs: &str) -> Self {
        self.logs = logs.into();
        self
    }
    pub fn progress(mut self, progress: &str) -> Self {
        self.progress = progress.into();
        self
    }
    pub fn run(&self, argv: &[String], _timeout: f64, _options: &RunOptions) -> ChildResult {
        self.calls.lock().unwrap().push(argv.to_vec());
        if argv.iter().any(|a| a == "-show_entries") {
            return ChildResult::exited(&self.probe, "", self.probe_rc);
        }
        ChildResult::exited(&self.progress, &self.logs, self.decode_rc)
    }
}

/// A sample file that exists (contents are irrelevant to fake runners).
pub fn fixture_file(temp: &Temp, name: &str) -> PathBuf {
    let path = temp.join(name);
    std::fs::write(&path, b"fixture").unwrap();
    path
}

pub fn now_mono() -> f64 {
    dead_air::timefmt::monotonic()
}

pub fn value(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}

/// The release binary under test.
pub fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_dead-air"))
}

/// Run the binary with arguments in `cwd`, returning (code, stdout, stderr).
pub fn run_binary(cwd: &Path, args: &[&str]) -> (i32, String, String) {
    let output = std::process::Command::new(binary()).args(args).current_dir(cwd).output().expect("binary runs");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Write a config into `dir` with the state file inside it and optional extra TOML.
pub fn write_config(dir: &Path, extra: &str) -> PathBuf {
    let path = dir.join("dead-air.toml");
    let state = dir.join("state").join("dead-air.sqlite3");
    let text = EXAMPLE_TOML.replace(
        "state_path = \"state/dead-air.sqlite3\"",
        &format!("state_path = {}", dead_air::pyjson::toml_string(&state.to_string_lossy())),
    );
    std::fs::write(&path, format!("{text}\n{extra}")).unwrap();
    path
}

pub fn which(name: &str) -> Option<PathBuf> {
    dead_air::doctor::which(name)
}

/// FFmpeg and ffprobe for native suites; `DEAD_AIR_REQUIRE_MEDIA=1` turns absence into failure.
pub fn media_tools() -> Option<(String, String)> {
    match (which("ffmpeg"), which("ffprobe")) {
        (Some(ffmpeg), Some(ffprobe)) => {
            Some((ffmpeg.to_string_lossy().into_owned(), ffprobe.to_string_lossy().into_owned()))
        }
        _ => {
            if std::env::var("DEAD_AIR_REQUIRE_MEDIA").as_deref() == Ok("1") {
                panic!("required native media dependency missing: ffmpeg and ffprobe are mandatory");
            }
            eprintln!("native media tests skipped: ffmpeg and ffprobe are not both available");
            None
        }
    }
}
