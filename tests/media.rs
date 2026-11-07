//! Native media acceptance with synthetic lavfi fixtures (Python `test_media.py`). Needs FFmpeg
//! and ffprobe; set `DEAD_AIR_REQUIRE_MEDIA=1` to make their absence a failure.

mod common;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use common::*;
use dead_air::model::*;
use dead_air::process::{RunOptions, run_bounded};

struct Fixtures {
    _temp: Temp,
    root: PathBuf,
    config: AppConfig,
    ffmpeg: String,
}

fn generate(ffmpeg: &str, root: &Path, name: &str, video: Option<&str>, audio: Option<&str>, duration: f64) -> PathBuf {
    let target = root.join(format!("{name}.mkv"));
    let mut argv: Vec<String> =
        [ffmpeg, "-nostdin", "-hide_banner", "-loglevel", "error", "-y", "-threads", "1", "-filter_threads", "1"]
            .iter()
            .map(|s| s.to_string())
            .collect();
    let mut index = 0;
    let (mut video_index, mut audio_index) = (None, None);
    if let Some(source) = video {
        argv.extend(["-f", "lavfi", "-i", source].map(String::from));
        video_index = Some(index);
        index += 1;
    }
    if let Some(source) = audio {
        argv.extend(["-f", "lavfi", "-i", source].map(String::from));
        audio_index = Some(index);
    }
    argv.extend(["-t".to_string(), duration.to_string()]);
    if let Some(i) = video_index {
        argv.extend(["-map".into(), format!("{i}:v:0"), "-c:v".into(), "ffv1".into()]);
    }
    if let Some(i) = audio_index {
        argv.extend(["-map".into(), format!("{i}:a:0"), "-c:a".into(), "pcm_s16le".into()]);
    } else {
        argv.push("-an".into());
    }
    argv.push(target.to_string_lossy().into_owned());
    let result = run_bounded(
        &argv,
        30.0,
        &RunOptions { max_output_bytes: 8192, file_limits: vec![(target.clone(), 16_000_000)] },
    );
    assert!(result.succeeded() && !result.bounded_failure(), "lavfi fixture generation failed for {name}: {result:?}");
    target
}

fn fixtures() -> Option<&'static Fixtures> {
    static FIXTURES: OnceLock<Option<Fixtures>> = OnceLock::new();
    FIXTURES
        .get_or_init(|| {
            let (ffmpeg, ffprobe) = media_tools()?;
            let temp = Temp::new();
            let root = temp.path().to_path_buf();
            let mut config = dead_air::config::parse_config_text(dead_air::config::EXAMPLE_TOML).unwrap();
            config.watch.sample_s = 3.0;
            config.watch.capture_timeout_s = 15.0;
            config.watch.analysis_timeout_s = 15.0;
            config.watch.max_sample_bytes = 16_000_000;
            config.watch.max_output_bytes = 65_536;
            config.tools.ffmpeg = ffmpeg.clone();
            config.tools.ffprobe = ffprobe;
            let stream = &mut config.streams[0];
            stream.freeze_window_s = 1.0;
            stream.silence_window_s = 1.0;
            stream.freeze_noise = 0.001;
            stream.silence_db = -50.0;
            let tone = "sine=frequency=1000:sample_rate=48000";
            let moving = "testsrc2=size=160x90:rate=10";
            let black = "color=c=black:size=160x90:rate=10";
            let quiet = "anullsrc=r=48000:cl=stereo";
            for (name, video, audio, duration) in [
                ("normal", Some(moving), Some(tone), 3.0),
                ("frozen", Some(black), Some(tone), 3.0),
                ("silent", Some(moving), Some(quiet), 3.0),
                ("frozen-silent", Some(black), Some(quiet), 3.0),
                ("no-audio", Some(moving), None, 3.0),
                ("audio-only", None, Some(tone), 3.0),
                ("short", Some(moving), Some(tone), 0.5),
                (
                    "long-video-short-audio",
                    Some(moving),
                    Some("sine=frequency=1000:sample_rate=48000:duration=0.1"),
                    12.0,
                ),
                ("long-video-short-silence", Some(moving), Some("anullsrc=r=48000:cl=stereo:d=0.1"), 12.0),
                ("long-audio-one-frame-video", Some("color=c=black:size=160x90:rate=10:d=0.1"), Some(tone), 12.0),
            ] {
                generate(&ffmpeg, &root, name, video, audio, duration);
            }
            std::fs::write(root.join("corrupt.mkv"), b"not a media container").unwrap();
            Some(Fixtures { _temp: temp, root, config, ffmpeg })
        })
        .as_ref()
}

fn inspect_with(name: &str, adjust: impl FnOnce(&mut StreamConfig)) -> Option<Observation> {
    let fixtures = fixtures()?;
    let mut config = fixtures.config.clone();
    adjust(&mut config.streams[0]);
    let path = fixtures.root.join(format!("{name}.mkv"));
    Some(dead_air::media::inspect_sample(&path, &config.streams[0], &config, &run_bounded, None, 0.0, &now_mono))
}

fn inspect(name: &str) -> Option<Observation> {
    inspect_with(name, |_| {})
}

fn statuses(o: &Observation) -> (&str, &str, &str) {
    (o.status.as_str(), o.audio.status.as_str(), o.video.status.as_str())
}

#[test]
fn normal_audio_video_is_healthy() {
    let Some(result) = inspect("normal") else { return };
    assert_eq!(statuses(&result), ("HEALTHY", "HEALTHY", "HEALTHY"), "{result:?}");
}

#[test]
fn frozen_picture_with_tone_only_fails_video_dimension() {
    let Some(result) = inspect("frozen") else { return };
    assert_eq!(result.video.status, "UNHEALTHY");
    assert_eq!(result.video.reason_codes, vec!["VIDEO_FREEZE"]);
    assert_eq!(result.audio.status, "HEALTHY");
    assert_ne!(result.status, "OFF_AIR");
}

#[test]
fn moving_picture_with_silence_only_fails_audio_dimension() {
    let Some(result) = inspect("silent") else { return };
    assert_eq!(result.audio.status, "UNHEALTHY");
    assert_eq!(result.audio.reason_codes, vec!["AUDIO_SILENCE"]);
    assert_eq!(result.video.status, "HEALTHY");
}

#[test]
fn frozen_and_silent_reports_both_dimensions() {
    let Some(result) = inspect("frozen-silent") else { return };
    assert_eq!((result.audio.status.as_str(), result.video.status.as_str()), ("UNHEALTHY", "UNHEALTHY"));
    let mut reasons = result.reason_codes.clone();
    reasons.sort();
    assert_eq!(reasons, vec!["AUDIO_SILENCE", "VIDEO_FREEZE"]);
}

#[test]
fn missing_required_audio_is_unknown_not_silence() {
    let Some(result) = inspect("no-audio") else { return };
    assert_eq!(result.audio.status, "UNKNOWN");
    assert_eq!(result.audio.reason_codes, vec!["AUDIO_MISSING"]);
    assert_eq!(result.video.status, "HEALTHY");
}

#[test]
fn configured_audio_only_fixture_is_supported() {
    let Some(result) = inspect_with("audio-only", |s| {
        s.expect_video = false;
        s.detect_freeze = false;
    }) else {
        return;
    };
    assert_eq!(statuses(&result), ("HEALTHY", "HEALTHY", "NOT_APPLICABLE"));
}

#[test]
fn configured_video_only_fixture_is_supported() {
    let Some(result) = inspect_with("no-audio", |s| s.expect_audio = false) else { return };
    assert_eq!(statuses(&result), ("HEALTHY", "NOT_APPLICABLE", "HEALTHY"));
}

#[test]
fn short_capture_is_unknown() {
    let Some(result) = inspect("short") else { return };
    assert_eq!((result.audio.status.as_str(), result.video.status.as_str()), ("UNKNOWN", "UNKNOWN"));
}

#[test]
fn short_audible_audio_cannot_borrow_long_video_coverage() {
    let Some(result) = inspect("long-video-short-audio") else { return };
    assert_eq!((result.audio.status.as_str(), result.video.status.as_str()), ("UNKNOWN", "HEALTHY"));
    assert!(result.audio.decoded_s.unwrap() < 3.0);
    assert!(result.video.decoded_s.unwrap() >= 2.9);
}

#[test]
fn short_silent_audio_interval_cannot_borrow_long_video_coverage() {
    let Some(result) = inspect("long-video-short-silence") else { return };
    assert_eq!(result.audio.status, "UNKNOWN");
    assert_eq!(result.video.status, "HEALTHY");
    assert!(result.audio.decoded_s.unwrap() < 3.0);
}

#[test]
fn one_frozen_video_frame_cannot_borrow_long_audio_coverage() {
    let Some(result) = inspect("long-audio-one-frame-video") else { return };
    assert_eq!((result.video.status.as_str(), result.audio.status.as_str()), ("UNKNOWN", "HEALTHY"));
    assert!(result.video.decoded_s.unwrap_or(0.0) < 3.0);
    assert!(result.audio.decoded_s.unwrap() >= 2.9);
}

#[test]
fn delayed_short_audio_does_not_certify_full_sample_window() {
    let Some(fixtures) = fixtures() else { return };
    let output = fixtures.root.join("delayed-short-audio.mkv");
    let argv: Vec<String> = [
        fixtures.ffmpeg.as_str(),
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        "-y",
        "-threads",
        "1",
        "-filter_threads",
        "1",
        "-filter_complex_threads",
        "1",
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=160x90:rate=10:duration=3",
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=1000:sample_rate=48000:duration=0.1",
        "-filter_complex",
        "[1:a]asetpts=PTS+2.9/TB[a]",
        "-map",
        "0:v:0",
        "-map",
        "[a]",
        "-c:v",
        "ffv1",
        "-c:a",
        "pcm_s16le",
        "-t",
        "3",
        &output.to_string_lossy(),
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let result = run_bounded(
        &argv,
        30.0,
        &RunOptions { max_output_bytes: 8192, file_limits: vec![(output.clone(), 16_000_000)] },
    );
    assert_eq!(result.returncode, Some(0));
    assert!(result.cleanup_complete);
    let config = &fixtures.config;
    let observed =
        dead_air::media::inspect_sample(&output, &config.streams[0], config, &run_bounded, None, 0.0, &now_mono);
    assert_eq!(observed.audio.status, "UNKNOWN");
    assert!((observed.audio.decoded_s.unwrap() - 0.1).abs() < 0.02);
    assert_eq!(observed.video.status, "HEALTHY");
    assert!((observed.video.decoded_s.unwrap() - 3.0).abs() < 0.02);
}

#[test]
fn corrupt_capture_is_unknown() {
    let Some(result) = inspect("corrupt") else { return };
    assert_eq!(result.status, "UNKNOWN");
}

#[test]
fn an_exhausted_deadline_never_starts_ffprobe() {
    let Some(fixtures) = fixtures() else { return };
    let config = &fixtures.config;
    let path = fixtures.root.join("normal.mkv");
    let spent = Some(now_mono() - 1.0);
    let result =
        dead_air::media::inspect_sample(&path, &config.streams[0], config, &run_bounded, spent, 0.0, &now_mono);
    assert_eq!(result.reason_codes, vec!["CYCLE_BUDGET_EXHAUSTED"]);
}
