//! Parser, observation and source-resolution behaviour (Python `test_dead_air.py`).

mod common;

use std::collections::HashMap;
use std::sync::Mutex;

use common::*;
use dead_air::constants::INPUT_PROTOCOLS;
use dead_air::http::GetOptions;
use dead_air::media::*;
use dead_air::model::*;
use dead_air::netpolicy::Resolver;
use dead_air::observe::*;
use dead_air::process::{ChildResult, RunOptions};
use serde_json::json;

fn mono() -> f64 {
    dead_air::timefmt::monotonic()
}

fn inspect(runner: &InspectorRunner, config: &AppConfig, name: &str) -> Observation {
    let temp = Temp::new();
    let sample = fixture_file(&temp, name);
    let run = |a: &[String], t: f64, o: &RunOptions| runner.run(a, t, o);
    inspect_sample(&sample, &config.streams[0], config, &run, None, 0.0, &mono)
}

#[test]
fn probe_and_progress_capture_coverage() {
    let evidence = parse_probe_json(&probe_av()).unwrap();
    let progress = parse_progress(PROGRESS_2S).unwrap();
    assert_eq!((evidence.audio_streams, evidence.video_streams), (1, 1));
    assert_eq!(evidence.duration_s, Some(2.0));
    assert_eq!((evidence.audio_index, evidence.video_index), (Some(1), Some(0)));
    assert!(progress.complete);
    assert_eq!(progress.decoded_s, Some(2.0));
    assert_eq!(progress.frames, 20);
    assert_eq!(parse_probe_json("not json"), Err("PROBE_JSON_INVALID"));
    assert_eq!(parse_probe_json("{}"), Err("PROBE_STREAMS_MISSING"));
    assert_eq!(parse_progress("out_time_us=abc\n"), Err("PROGRESS_TIME_INVALID"));
}

#[test]
fn decoded_coverage_rejects_delay_gaps_and_duplicate_overlap() {
    let delayed = parse_probe_json(&Probe { audio_s: 0.1, audio_start: 1.9, ..Probe::default() }.json()).unwrap();
    let coverage = decoded_coverage(&delayed, "AUDIO", 2.0, 0.0, None).unwrap();
    assert!((coverage.covered_s - 0.1).abs() < 1e-6);
    assert!(!coverage.complete);

    let gapped = parse_probe_json(&Probe { video_gap: Some((0.8, 1.0)), ..Probe::default() }.json()).unwrap();
    let coverage = decoded_coverage(&gapped, "VIDEO", 2.0, 0.0, None).unwrap();
    assert!((coverage.largest_gap_s - 0.2).abs() < 1e-6);
    assert!(!coverage.complete);

    let duplicate = json!({
        "streams": [{"index": 0, "codec_type": "audio", "sample_rate": "48000", "time_base": "1/1000"}],
        "format": {"start_time": "0", "duration": "2"},
        "frames": vec![json!({"stream_index": 0, "pts_time": "0", "nb_samples": 24000}); 4],
    });
    let coverage =
        decoded_coverage(&parse_probe_json(&duplicate.to_string()).unwrap(), "AUDIO", 2.0, 0.0, None).unwrap();
    assert_eq!(coverage.covered_s, 0.5);
    assert_eq!(coverage.decoded_amount_s, 2.0);
    assert!(!coverage.complete);

    // Millisecond-rounded timestamps of contiguous AAC frames still prove full coverage.
    let mut frames = Vec::new();
    let mut decoded: i64 = 0;
    for samples in std::iter::repeat_n(1024i64, 140).chain([640]) {
        let pts_ms = (decoded as f64 * 1000.0 / 48000.0).round_ties_even();
        frames.push(json!({"stream_index": 0, "media_type": "audio", "pts_time": format!("{:.3}", pts_ms / 1000.0), "nb_samples": samples}));
        decoded += samples;
    }
    let rounded = json!({
        "streams": [{"index": 0, "codec_type": "audio", "sample_rate": "48000", "time_base": "1/1000"}],
        "format": {"start_time": "0", "duration": "3"}, "frames": frames,
    });
    let coverage = decoded_coverage(&parse_probe_json(&rounded.to_string()).unwrap(), "AUDIO", 3.0, 0.0, None).unwrap();
    assert_eq!(coverage.frame_count, 141);
    assert_eq!(coverage.sample_count, 140 * 1024 + 623);
    assert!(coverage.complete);
}

#[test]
fn delayed_short_audio_stays_unknown_while_video_is_independently_healthy() {
    let config = test_config(3.0);
    let runner =
        InspectorRunner::new(Probe { audio_s: 0.1, audio_start: 2.9, video_s: 3.0, ..Probe::default() }.json())
            .progress("frame=30\nout_time_us=3000000\nprogress=end\n");
    let result = inspect(&runner, &config, "delayed-public-safe.mkv");
    assert_eq!(result.audio.status, "UNKNOWN");
    assert_eq!(result.video.status, "HEALTHY");
    assert!((result.audio.decoded_s.unwrap() - 0.1).abs() < 1e-6);
    assert!((result.video.decoded_s.unwrap() - 3.0).abs() < 1e-6);
    let evidence = transition_sample("audio_silence", &result);
    let coverage = &evidence["dimensions"]["audio"]["coverage"];
    assert_eq!(coverage["complete"], false);
    assert!((coverage["covered_s"].as_f64().unwrap() - 0.1).abs() < 1e-3);
    assert_eq!((coverage["sample_count"].as_u64(), coverage["sample_rate_hz"].as_i64()), (Some(4800), Some(48000)));
    assert!(coverage.get("frames").is_none());
    let calls = runner.calls.lock().unwrap();
    assert!(calls[0].contains(&"-show_frames".to_string()));
    assert!(!calls[0].contains(&"-count_frames".to_string()));
}

#[test]
fn intervals_include_start_end_and_open_end_at_decode_coverage() {
    let (freezes, silences) = parse_intervals(
        "lavfi.freezedetect.freeze_start: 0.25\nlavfi.freezedetect.freeze_end: 1.50\nsilence_start: 0.5\n",
        2.0,
    )
    .unwrap();
    assert_eq!((freezes[0].start_s, freezes[0].end_s), (0.25, 1.5));
    assert_eq!((silences[0].start_s, silences[0].end_s), (0.5, 2.0));
    assert!(silences[0].open_at_sample_end);
    assert_eq!(parse_intervals("silence_end: 1.0\n", 2.0), Err("DETECTOR_INTERVAL_UNPAIRED"));
    assert_eq!(parse_intervals("silence_start: 0.1\nsilence_start: 0.2\n", 2.0), Err("DETECTOR_INTERVAL_OVERLAP"));
    assert_eq!(parse_intervals("silence_start: nope\n", 2.0), Err("DETECTOR_INTERVAL_MALFORMED"));
    assert_eq!(parse_intervals("silence_start: 3.0\n", 2.0), Err("DETECTOR_INTERVAL_AFTER_DECODE_END"));
    assert_eq!(parse_intervals("[silencedetect @ 0x1] silence_start: -1\n", 2.0), Err("DETECTOR_INTERVAL_INVALID"));
}

#[test]
fn freeze_is_video_unhealthy_not_off_air() {
    let config = test_config(2.0);
    let runner = InspectorRunner::new(probe_av())
        .logs("lavfi.freezedetect.freeze_start: 0.0\nlavfi.freezedetect.freeze_end: 1.5\n");
    let result = inspect(&runner, &config, "sample.mkv");
    assert_eq!(result.status, "UNHEALTHY");
    assert_eq!(result.video.status, "UNHEALTHY");
    assert_eq!(result.video.reason_codes, vec!["VIDEO_FREEZE"]);
    assert_eq!(result.audio.status, "HEALTHY");
}

#[test]
fn silence_is_audio_unhealthy_and_dimensions_are_separate() {
    let config = test_config(2.0);
    let runner =
        InspectorRunner::new(probe_av()).logs("silence_start: 0.0\nsilence_end: 1.4 | silence_duration: 1.4\n");
    let result = inspect(&runner, &config, "sample.mkv");
    assert_eq!((result.audio.status.as_str(), result.video.status.as_str()), ("UNHEALTHY", "HEALTHY"));
    assert_eq!(result.status, "UNHEALTHY");
}

#[test]
fn both_events_report_both_dimensions() {
    let config = test_config(2.0);
    let runner = InspectorRunner::new(probe_av()).logs(
        "lavfi.freezedetect.freeze_start: 0.0\nlavfi.freezedetect.freeze_end: 1.5\nsilence_start: 0.0\nsilence_end: 1.5 | silence_duration: 1.5\n",
    );
    let result = inspect(&runner, &config, "sample.mkv");
    assert_eq!((result.video.status.as_str(), result.audio.status.as_str()), ("UNHEALTHY", "UNHEALTHY"));
    let mut reasons = result.reason_codes.clone();
    reasons.sort();
    assert_eq!(reasons, vec!["AUDIO_SILENCE", "VIDEO_FREEZE"]);
}

#[test]
fn missing_required_audio_stays_unknown_while_video_can_be_proven() {
    let config = test_config(2.0);
    let runner = InspectorRunner::new(Probe { include_audio: false, ..Probe::default() }.json());
    let result = inspect(&runner, &config, "sample.mkv");
    assert_eq!(result.audio.status, "UNKNOWN");
    assert_eq!(result.audio.reason_codes, vec!["AUDIO_MISSING"]);
    assert_eq!(result.video.status, "HEALTHY");
    assert_eq!(result.status, "UNKNOWN");
}

#[test]
fn configured_audio_only_stream_can_be_healthy() {
    let mut config = test_config(2.0);
    config.streams[0].expect_video = false;
    config.streams[0].detect_freeze = false;
    let runner = InspectorRunner::new(Probe { include_video: false, ..Probe::default() }.json())
        .progress("frame=0\nout_time_us=2000000\nprogress=end\n");
    let result = inspect(&runner, &config, "audio.mkv");
    assert_eq!(
        (result.status.as_str(), result.audio.status.as_str(), result.video.status.as_str()),
        ("HEALTHY", "HEALTHY", "NOT_APPLICABLE")
    );
}

#[test]
fn short_capture_and_corrupt_sample_are_unknown() {
    let config = test_config(2.0);
    let runner = InspectorRunner::new(Probe { audio_s: 0.4, video_s: 0.4, ..Probe::default() }.json())
        .progress("frame=4\nout_time_us=400000\nprogress=end\n");
    let result = inspect(&runner, &config, "short.mkv");
    assert_eq!((result.audio.status.as_str(), result.video.status.as_str()), ("UNKNOWN", "UNKNOWN"));
    let corrupt = InspectorRunner::new("not json".into());
    assert_eq!(inspect(&corrupt, &config, "corrupt.mkv").status, "UNKNOWN");
    let failed = InspectorRunner { probe_rc: 1, ..InspectorRunner::new(probe_av()) };
    assert_eq!(inspect(&failed, &config, "failed.mkv").reason_codes, vec!["PROBE_FAILED"]);
}

#[test]
fn local_samples_must_be_regular_local_files() {
    let config = test_config(2.0);
    let runner = InspectorRunner::new(probe_av());
    let run = |a: &[String], t: f64, o: &RunOptions| runner.run(a, t, o);
    let stream = &config.streams[0];
    for source in ["https://example.invalid/x.mkv", "lavfi:testsrc", "//server/share/x.mkv", "/nonexistent/x.mkv"] {
        let result = inspect_sample(std::path::Path::new(source), stream, &config, &run, None, 0.0, &mono);
        assert_eq!(result.reason_codes, vec!["SAMPLE_UNAVAILABLE_OR_NOT_LOCAL"], "{source}");
    }
    assert!(runner.calls.lock().unwrap().is_empty(), "no child may start for a rejected sample");
}

#[test]
fn explicit_direct_offline_contract_is_separate_from_media_events() {
    let stream = test_config(2.0).streams[0].clone();
    let result = HttpResult::new(Some(410), "https://example.invalid/live/sample", "", b"");
    assert_eq!(off_air_from_direct(&result, &stream), Some("DIRECT_OFFLINE_STATUS"));
    assert_eq!(off_air_from_direct(&HttpResult { status: Some(404), ..result }, &stream), None);
}

#[test]
fn hls_endlist_requires_successful_playlist_and_tag_line() {
    let stream = test_config(2.0).streams[0].clone();
    let url = "https://example.invalid/live/list.m3u8";
    let cases: [(u16, &[u8]); 4] = [
        (403, b"#EXTM3U\n#EXT-X-ENDLIST\n"),
        (500, b"server error #EXT-X-ENDLIST"),
        (200, b"blocked page #EXT-X-ENDLIST"),
        (200, b"#EXTM3U\n# a comment mentions #EXT-X-ENDLIST\n"),
    ];
    for (status, body) in cases {
        assert_eq!(
            off_air_from_direct(&HttpResult::new(Some(status), url, "application/vnd.apple.mpegurl", body), &stream),
            None
        );
    }
    let genuine = HttpResult::new(
        Some(200),
        url,
        "application/vnd.apple.mpegurl",
        b"#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-ENDLIST\n",
    );
    assert_eq!(off_air_from_direct(&genuine, &stream), Some("DIRECT_HLS_ENDLIST"));
    let bom = HttpResult::new(Some(200), url, "", b"\xef\xbb\xbf#EXTM3U\r\n#EXT-X-ENDLIST\r\n");
    assert_eq!(off_air_from_direct(&bom, &stream), Some("DIRECT_HLS_ENDLIST"));
    let failed = HttpResult { error_code: Some("HTTP_READ_FAILED".into()), ..genuine.clone() };
    assert_eq!(off_air_from_direct(&failed, &stream), None);
    let truncated = HttpResult { truncated: true, ..genuine };
    assert_eq!(off_air_from_direct(&truncated, &stream), None);
}

#[test]
fn ytdlp_argv_is_fixed_and_positionally_safe() {
    let url = "https://www.youtube.com/watch?v=example";
    let argv = build_ytdlp_argv("yt-dlp", url);
    assert_eq!(&argv[argv.len() - 2..], &["--".to_string(), url.to_string()]);
    for flag in ["--ignore-config", "--no-plugin-dirs", "--no-remote-components", "--skip-download"] {
        assert!(argv.iter().any(|a| a == flag), "{flag}");
    }
    assert!(!argv.iter().any(|a| a.contains("cookies")));
}

#[test]
fn remote_capture_protocol_options_precede_each_input_without_file() {
    let config = test_config(2.0);
    let media = ResolvedMedia {
        inputs: vec![
            MediaInput { kind: "video".into(), url: "https://cdn.invalid/video".into() },
            MediaInput { kind: "audio".into(), url: "https://cdn.invalid/audio".into() },
        ],
    };
    let argv = build_capture_argv(&media, &config.streams[0], &config, std::path::Path::new("sample.mkv")).unwrap();
    assert!(!INPUT_PROTOCOLS.split(',').any(|p| p == "file"));
    let inputs: Vec<usize> = argv.iter().enumerate().filter(|(_, a)| *a == "-i").map(|(i, _)| i).collect();
    assert_eq!(inputs.len(), 2);
    for index in inputs {
        assert_eq!(
            &argv[index - 6..index],
            &["-protocol_whitelist", INPUT_PROTOCOLS, "-tls_verify", "1", "-rw_timeout", "10000000"]
        );
    }
    assert!(argv.windows(2).any(|w| w[0] == "-t" && w[1] == "2.1"), "100 ms acquisition prefix plus the sample");
    let plain =
        ResolvedMedia { inputs: vec![MediaInput { kind: "muxed".into(), url: "http://cdn.invalid/live.m3u8".into() }] };
    let argv = build_capture_argv(&plain, &config.streams[0], &config, std::path::Path::new("sample.mkv")).unwrap();
    assert!(!argv.iter().any(|a| a == "-tls_verify"), "plain HTTP inputs get no TLS option");
}

fn public() -> Resolver {
    Resolver::always(&["93.184.216.34"])
}

fn youtube_stream(config: &AppConfig, url: &str, thumbnail: bool) -> AppConfig {
    let mut config = config.clone();
    config.streams[0].source = "youtube".into();
    config.streams[0].url = url.into();
    config.streams[0].thumbnail_enabled = thumbnail;
    config.streams[0].thumbnail_url = String::new();
    config
}

#[test]
fn fake_resolver_and_capture_run_without_external_transport() {
    let config = youtube_stream(&test_config(2.0), "https://www.youtube.com/watch?v=fixture-id", false);
    let calls: Mutex<Vec<Vec<String>>> = Mutex::new(Vec::new());
    let resolver_json = json!({"live_status": "is_live", "formats": [{"url": "https://cdn.invalid/media", "vcodec": "avc1", "acodec": "aac", "height": 360}]}).to_string();
    let capture_probe = Probe { audio_start: 0.1, video_s: 2.1, format_duration_s: 2.1, ..Probe::default() }.json();
    let runner = |argv: &[String], _t: f64, _o: &RunOptions| -> ChildResult {
        calls.lock().unwrap().push(argv.to_vec());
        if argv[0] == "yt-dlp" {
            return ChildResult::exited(&resolver_json, "", 0);
        }
        if argv.iter().any(|a| a == "-show_entries") {
            return ChildResult::exited(&capture_probe, "", 0);
        }
        if argv.iter().any(|a| a == "matroska") {
            std::fs::write(argv.last().unwrap(), b"bounded sample").unwrap();
            return ChildResult::exited("", "", 0);
        }
        ChildResult::exited(PROGRESS_2S, "", 0)
    };
    let transport =
        |_url: &str, _o: &GetOptions<'_>| -> HttpResult { panic!("YouTube path must use the resolver, not HTTP") };
    let resolver = public();
    let deps = ObserveDeps { runner: &runner, resolver: &resolver, transport: &transport, monotonic: &mono };
    let result = observe_stream(&config, &config.streams[0], &deps, None, None);
    assert_eq!(result.status, "HEALTHY", "{result:?}");
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 5);
    assert!(calls[0].contains(&"--no-remote-components".to_string()));
    let capture = calls.iter().find(|a| a.contains(&"-rw_timeout".to_string())).unwrap();
    assert!(capture.windows(2).any(|w| w[0] == "-threads" && w[1] == "1"));
    assert!(capture.contains(&"-protocol_whitelist".to_string()));
    let decode =
        calls.iter().find(|a| a.iter().any(|x| x.starts_with("trim="))).expect("decode trims the acquisition prefix");
    assert!(decode.contains(&"-copyts".to_string()));
}

#[test]
fn resolver_unknown_uses_one_validated_youtube_thumbnail_fallback() {
    let config = youtube_stream(&test_config(2.0), "https://www.youtube.com/watch?v=AbCdEfGhijk", true);
    let resolver_calls = Mutex::new(0);
    let reads: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let runner = |_a: &[String], _t: f64, _o: &RunOptions| {
        *resolver_calls.lock().unwrap() += 1;
        ChildResult::exited("", "", 2)
    };
    let transport = |url: &str, _o: &GetOptions<'_>| {
        reads.lock().unwrap().push(url.to_string());
        HttpResult::new(Some(200), url, "image/jpeg", b"public-safe-thumbnail")
    };
    let resolver = public();
    let deps = ObserveDeps { runner: &runner, resolver: &resolver, transport: &transport, monotonic: &mono };
    let mut history = HashMap::new();
    let result = observe_stream(&config, &config.streams[0], &deps, Some(&mut history), None);
    assert_eq!(*resolver_calls.lock().unwrap(), 1);
    assert_eq!(*reads.lock().unwrap(), vec!["https://i.ytimg.com/vi/AbCdEfGhijk/hqdefault.jpg"]);
    assert_eq!(
        (result.status.as_str(), result.video.status.as_str(), result.audio.status.as_str()),
        ("DEGRADED", "DEGRADED", "UNKNOWN")
    );
    assert!(result.reason_codes.contains(&"FULL_MEDIA_UNVERIFIED".to_string()));
}

#[test]
fn resolver_off_air_policy_failure_and_exhausted_budget_skip_fallback() {
    let config = youtube_stream(&test_config(2.0), "https://www.youtube.com/watch?v=AbCdEfGhijk", true);
    let reads = Mutex::new(0);
    let transport = |url: &str, _o: &GetOptions<'_>| {
        *reads.lock().unwrap() += 1;
        HttpResult::new(Some(200), url, "image/jpeg", b"thumbnail")
    };
    let upcoming = json!({"live_status": "is_upcoming"}).to_string();
    let runner = |_a: &[String], _t: f64, _o: &RunOptions| ChildResult::exited(&upcoming, "", 0);
    let resolver = public();
    let deps = ObserveDeps { runner: &runner, resolver: &resolver, transport: &transport, monotonic: &mono };
    let mut history = HashMap::new();
    assert_eq!(observe_stream(&config, &config.streams[0], &deps, Some(&mut history), None).status, "OFF_AIR");

    let blocked = json!({"live_status": "is_live", "formats": [{"url": "https://media.invalid/blocked", "vcodec": "avc1", "acodec": "aac"}]}).to_string();
    let runner = |_a: &[String], _t: f64, _o: &RunOptions| ChildResult::exited(&blocked, "", 0);
    let split = Resolver::fixed(|host, _| {
        Ok(vec![if host == "media.invalid" { "10.0.0.8".into() } else { "93.184.216.34".into() }])
    });
    let deps = ObserveDeps { runner: &runner, resolver: &split, transport: &transport, monotonic: &mono };
    let result = observe_stream(&config, &config.streams[0], &deps, Some(&mut history), None);
    assert_eq!(result.reason_codes, vec!["RESOLVER_MEDIA_URL_REJECTED"]);

    let runner = |_a: &[String], _t: f64, _o: &RunOptions| -> ChildResult {
        panic!("exhausted budget must stop before resolver")
    };
    let deps = ObserveDeps { runner: &runner, resolver: &resolver, transport: &transport, monotonic: &mono };
    let result = observe_stream(&config, &config.streams[0], &deps, Some(&mut history), Some(mono() - 1.0));
    assert_eq!(result.reason_codes, vec!["CYCLE_BUDGET_EXHAUSTED"]);
    assert_eq!(*reads.lock().unwrap(), 0);
}

#[test]
fn fake_direct_transport_confirms_only_explicit_offline_contract() {
    let config = test_config(2.0);
    let calls = Mutex::new(0);
    let transport = |url: &str, options: &GetOptions<'_>| {
        *calls.lock().unwrap() += 1;
        assert!(options.range_request);
        HttpResult::new(Some(410), url, "", b"")
    };
    let runner =
        |_a: &[String], _t: f64, _o: &RunOptions| -> ChildResult { panic!("off-air must stop before capture") };
    let resolver = public();
    let deps = ObserveDeps { runner: &runner, resolver: &resolver, transport: &transport, monotonic: &mono };
    let result = observe_stream(&config, &config.streams[0], &deps, None, None);
    assert_eq!(result.status, "OFF_AIR");
    assert_eq!(result.reason_codes, vec!["DIRECT_OFFLINE_STATUS"]);
    assert_eq!(*calls.lock().unwrap(), 1);
}

#[test]
fn private_source_is_unknown_without_any_transport() {
    let config = test_config(2.0);
    let transport = |_u: &str, _o: &GetOptions<'_>| -> HttpResult { panic!("policy must reject first") };
    let runner = |_a: &[String], _t: f64, _o: &RunOptions| -> ChildResult { panic!("policy must reject first") };
    let resolver = Resolver::always(&["192.168.1.20"]);
    let deps = ObserveDeps { runner: &runner, resolver: &resolver, transport: &transport, monotonic: &mono };
    let result = observe_stream(&config, &config.streams[0], &deps, None, None);
    assert_eq!(
        (result.status.as_str(), result.reason_codes.clone()),
        ("UNKNOWN", vec!["PRIVATE_NETWORK_DENIED".to_string()])
    );
}

#[test]
fn short_track_keeps_unknown_while_long_other_track_is_healthy() {
    let config = test_config(2.0);
    let probe = Probe { audio_s: 0.4, ..Probe::default() }.json();
    let runner = |argv: &[String], _t: f64, _o: &RunOptions| {
        if argv.iter().any(|a| a == "-show_entries") {
            return ChildResult::exited(&probe, "", 0);
        }
        if argv.iter().any(|a| a == "0:a:0") {
            return ChildResult::exited("frame=0\nout_time_us=400000\nprogress=end\n", "silence_start: 0.0\n", 0);
        }
        ChildResult::exited(PROGRESS_2S, "", 0)
    };
    let temp = Temp::new();
    let sample = fixture_file(&temp, "asymmetric.mkv");
    let result = inspect_sample(&sample, &config.streams[0], &config, &runner, None, 0.0, &mono);
    assert_eq!((result.audio.status.as_str(), result.video.status.as_str()), ("UNKNOWN", "HEALTHY"));
    assert_eq!(result.audio.decoded_s, Some(0.4));
    assert_eq!(result.video.decoded_s, Some(2.0));
}

fn thumbnail_config(stale_after: f64) -> AppConfig {
    let mut config = test_config(2.0);
    config.streams[0].thumbnail_enabled = true;
    config.streams[0].thumbnail_url = "https://images.invalid/thumb.png".into();
    config.streams[0].thumbnail_stale_after_s = stale_after;
    config
}

fn no_sample_runner(argv: &[String], _t: f64, _o: &RunOptions) -> ChildResult {
    if argv.iter().any(|a| a == "matroska") {
        return ChildResult::exited("", "", 0); // no sample file: force an honest fallback
    }
    ChildResult::exited("", "", 1)
}

#[test]
fn thumbnail_fallback_stays_degraded_and_audio_unknown() {
    let config = thumbnail_config(1800.0);
    let stream_url = config.streams[0].url.clone();
    let transport = |url: &str, _o: &GetOptions<'_>| {
        if url == stream_url {
            HttpResult::new(Some(200), url, "video/mp2t", b"prefix")
        } else {
            HttpResult::new(Some(200), url, "image/png", b"synthetic-thumbnail")
        }
    };
    let resolver = public();
    let deps = ObserveDeps { runner: &no_sample_runner, resolver: &resolver, transport: &transport, monotonic: &mono };
    let mut history = HashMap::new();
    let result = observe_stream(&config, &config.streams[0], &deps, Some(&mut history), None);
    assert_eq!(
        (result.status.as_str(), result.video.status.as_str(), result.audio.status.as_str()),
        ("DEGRADED", "DEGRADED", "UNKNOWN")
    );
    assert!(result.reason_codes.contains(&"FULL_MEDIA_UNVERIFIED".to_string()));
    // A non-image answer is never treated as a thumbnail.
    let html = |url: &str, _o: &GetOptions<'_>| HttpResult::new(Some(200), url, "text/html", b"<html>");
    let deps = ObserveDeps { runner: &no_sample_runner, resolver: &resolver, transport: &html, monotonic: &mono };
    assert_eq!(observe_stream(&config, &config.streams[0], &deps, Some(&mut HashMap::new()), None).status, "UNKNOWN");
}

#[test]
fn youtube_thumbnail_fallback_uses_validated_id_without_configured_url() {
    let config = youtube_stream(&test_config(2.0), "https://www.youtube.com/watch?v=AbCdEfGhijk", true);
    assert_eq!(
        youtube_thumbnail_url("https://youtu.be/AbCdEfGhijk").as_deref(),
        Some("https://i.ytimg.com/vi/AbCdEfGhijk/hqdefault.jpg")
    );
    assert_eq!(youtube_thumbnail_url("https://youtu.be/not-an-id"), None);
    assert_eq!(
        youtube_thumbnail_url("https://www.youtube.com/live/AbCdEfGhijk").as_deref(),
        Some("https://i.ytimg.com/vi/AbCdEfGhijk/hqdefault.jpg")
    );
    let seen: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let hosts = std::sync::Arc::new(Mutex::new(Vec::<String>::new()));
    let recorded = std::sync::Arc::clone(&hosts);
    let resolver = Resolver::fixed(move |host, _| {
        recorded.lock().unwrap().push(host.to_string());
        Ok(vec!["93.184.216.34".to_string()])
    });
    let transport = |url: &str, _o: &GetOptions<'_>| {
        seen.lock().unwrap().push(url.to_string());
        HttpResult::new(Some(200), url, "image/jpeg", b"thumbnail")
    };
    let runner = no_sample_runner;
    let deps = ObserveDeps { runner: &runner, resolver: &resolver, transport: &transport, monotonic: &mono };
    let original = unknown_observation(&config.streams[0], "CAPTURE_FAILED", None);
    let result = fallback_observation(original, &config.streams[0], &config, &mut HashMap::new(), &deps, 100.0, None);
    assert_eq!(*seen.lock().unwrap(), vec!["https://i.ytimg.com/vi/AbCdEfGhijk/hqdefault.jpg"]);
    assert!(hosts.lock().unwrap().contains(&"i.ytimg.com".to_string()), "the derived thumbnail host is policy checked");
    assert_eq!(
        (result.status.as_str(), result.audio.status.as_str(), result.video.status.as_str()),
        ("DEGRADED", "UNKNOWN", "DEGRADED")
    );
}

#[test]
fn unchanged_thumbnail_ages_from_last_change() {
    let config = thumbnail_config(60.0);
    let stream_url = config.streams[0].url.clone();
    let transport = |url: &str, _o: &GetOptions<'_>| {
        if url == stream_url {
            HttpResult::new(Some(200), url, "video/mp2t", b"prefix")
        } else {
            HttpResult::new(Some(200), url, "image/png", b"unchanged")
        }
    };
    let resolver = public();
    let clock = Mutex::new(1000.0);
    let now = || *clock.lock().unwrap();
    let deps = ObserveDeps { runner: &no_sample_runner, resolver: &resolver, transport: &transport, monotonic: &now };
    let mut history = HashMap::new();
    let first = observe_stream(&config, &config.streams[0], &deps, Some(&mut history), None);
    *clock.lock().unwrap() = 1061.0;
    let second = observe_stream(&config, &config.streams[0], &deps, Some(&mut history), None);
    assert!(first.reason_codes.contains(&"THUMBNAIL_BASELINE_ONLY".to_string()));
    assert!(second.reason_codes.contains(&"THUMBNAIL_SUSPECT_STATIC".to_string()));
    assert_eq!(second.status, "DEGRADED");
}

#[test]
fn resolved_media_prefers_muxed_formats_within_720p_and_rejects_private_urls() {
    let config = test_config(2.0);
    let stream = &config.streams[0];
    let payload = json!({"live_status": "is_live", "formats": [
        {"url": "https://cdn.invalid/1080", "vcodec": "avc1", "acodec": "aac", "height": 1080, "tbr": 9000},
        {"url": "https://cdn.invalid/480", "vcodec": "avc1", "acodec": "aac", "height": 480, "tbr": 900},
        {"url": "https://cdn.invalid/720", "vcodec": "avc1", "acodec": "aac", "height": 720, "tbr": 2000},
    ]});
    let (media, off_air, error) = select_resolved_media(&payload, stream, &config.network, &public(), None);
    assert_eq!((off_air, error), (None, None));
    assert_eq!(media.unwrap().inputs, vec![MediaInput { kind: "muxed".into(), url: "https://cdn.invalid/720".into() }]);
    let split = json!({"formats": [
        {"url": "https://cdn.invalid/v", "vcodec": "avc1", "acodec": "none", "height": 360},
        {"url": "https://cdn.invalid/a", "vcodec": "none", "acodec": "opus", "abr": 128},
    ]});
    let media = select_resolved_media(&split, stream, &config.network, &public(), None).0.unwrap();
    assert_eq!(media.inputs.iter().map(|m| m.kind.as_str()).collect::<Vec<_>>(), vec!["video", "audio"]);
    assert_eq!(
        select_resolved_media(&json!({"formats": []}), stream, &config.network, &public(), None).2,
        Some("RESOLVER_MEDIA_MISSING")
    );
    assert_eq!(
        select_resolved_media(&json!([1]), stream, &config.network, &public(), None).2,
        Some("RESOLVER_JSON_INVALID")
    );
    assert_eq!(
        select_resolved_media(&json!({"live_status": "was_live"}), stream, &config.network, &public(), None).1,
        Some("RESOLVER_NOT_LIVE")
    );
}

#[test]
fn incident_signals_never_treat_missing_or_thumbnail_evidence_as_good() {
    let degraded = Observation {
        stream_id: "s".into(),
        observed_at: "2026-10-23T00:00:00Z".into(),
        status: "DEGRADED".into(),
        reason_codes: vec!["THUMBNAIL_BASELINE_ONLY".into(), "FULL_MEDIA_UNVERIFIED".into()],
        audio: DimensionEvidence::new("UNKNOWN", &["AUDIO_UNVERIFIED_BY_THUMBNAIL"]),
        video: DimensionEvidence::new("DEGRADED", &["THUMBNAIL_BASELINE_ONLY"]),
    };
    let signals: HashMap<_, _> = incident_signals(&degraded).into_iter().collect();
    assert_eq!(signals["off_air"], "unknown");
    assert_eq!(signals["video_freeze"], "unknown");
    assert_eq!(signals["audio_silence"], "unknown");
    assert_eq!(signals["degraded"], "bad");
    assert_eq!(signals["unknown"], "unknown");
}
