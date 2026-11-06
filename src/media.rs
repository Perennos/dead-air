//! FFmpeg/ffprobe evidence: decoded coverage, detector intervals, and per-dimension health.
//!
//! Freeze (video) and silence (audio) are separate dimensions. A dimension is HEALTHY or
//! UNHEALTHY only when its own decoded frames cover the whole measurement window without a
//! gap; anything missing, short, gapped or malformed is UNKNOWN.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::constants::{CAPTURE_STARTUP_S, INPUT_PROTOCOLS};
use crate::model::*;
use crate::netpolicy::remaining;
use crate::process::{ChildResult, RunOptions, Runner, build_argv};
use crate::pyjson::float_repr;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProbeEvidence {
    pub audio_streams: u32,
    pub video_streams: u32,
    pub duration_s: Option<f64>,
    pub start_s: Option<f64>,
    pub audio_index: Option<i64>,
    pub audio_time_base_s: Option<f64>,
    pub audio_sample_rate: Option<i64>,
    pub video_index: Option<i64>,
    pub video_time_base_s: Option<f64>,
    pub frames: Vec<Map<String, Value>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProgressEvidence {
    pub complete: bool,
    pub decoded_s: Option<f64>,
    pub frames: i64,
}

fn python_int_text(text: &str) -> Option<i64> {
    let trimmed = text.trim().replace('_', "");
    let digits = trimmed.strip_prefix(['+', '-']).unwrap_or(&trimmed);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    trimmed.parse().ok()
}

/// Python `float(value)` for JSON strings and numbers.
fn python_float(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => {
            let t = s.trim().replace('_', "");
            match t.to_ascii_lowercase().as_str() {
                "inf" | "+inf" | "infinity" | "+infinity" => Some(f64::INFINITY),
                "-inf" | "-infinity" => Some(f64::NEG_INFINITY),
                "nan" | "+nan" | "-nan" => Some(f64::NAN),
                _ => t.parse().ok(),
            }
        }
        Value::Bool(b) => Some(f64::from(u8::from(*b))),
        _ => None,
    }
}

/// `_probe_float`: finite and within one day, else `None`.
pub fn probe_float(value: Option<&Value>) -> Option<f64> {
    let value = value?;
    if matches!(value, Value::Bool(_) | Value::Null) || value.as_str().is_some_and(|s| s.is_empty() || s == "N/A") {
        return None;
    }
    python_float(value).filter(|v| v.is_finite() && v.abs() <= 86_400.0)
}

/// `_probe_integer`: a non-negative integer (integral floats accepted), else `None`.
pub fn probe_integer(value: Option<&Value>) -> Option<i64> {
    let result = match value? {
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i
            } else {
                let f = n.as_f64()?;
                if !f.is_finite() || f.fract() != 0.0 {
                    return None;
                }
                f as i64
            }
        }
        Value::String(s) if !s.is_empty() && s != "N/A" => python_int_text(s)?,
        _ => return None,
    };
    if result >= 0 { Some(result) } else { None }
}

fn probe_time_base(value: Option<&Value>) -> Option<f64> {
    let text = value?.as_str()?.trim();
    let result = if let Some((num, den)) = text.split_once('/') {
        let num: f64 = python_int_text(num)? as f64;
        let den: f64 = python_int_text(den)? as f64;
        if den == 0.0 {
            return None;
        }
        num / den
    } else {
        text.parse::<f64>().ok()?
    };
    if result.is_finite() && result > 0.0 { Some(result) } else { None }
}

pub fn parse_probe_json(text: &str) -> Result<ProbeEvidence, &'static str> {
    let payload: Value = serde_json::from_str(text).map_err(|_| "PROBE_JSON_INVALID")?;
    let Some(streams) = payload.get("streams").and_then(Value::as_array).filter(|_| payload.is_object()) else {
        return Err("PROBE_STREAMS_MISSING");
    };
    let mut evidence = ProbeEvidence::default();
    for item in streams {
        let Value::Object(item) = item else {
            return Err("PROBE_STREAM_INVALID");
        };
        let kind = item.get("codec_type").and_then(Value::as_str);
        let index = probe_integer(item.get("index"));
        let time_base = probe_time_base(item.get("time_base"));
        match kind {
            Some("audio") => {
                evidence.audio_streams += 1;
                if evidence.audio_streams == 1 {
                    evidence.audio_index = index;
                    evidence.audio_time_base_s = time_base;
                    evidence.audio_sample_rate = probe_integer(item.get("sample_rate"));
                }
            }
            Some("video") => {
                evidence.video_streams += 1;
                if evidence.video_streams == 1 {
                    evidence.video_index = index;
                    evidence.video_time_base_s = time_base;
                }
            }
            _ => {}
        }
    }
    if let Some(Value::Object(format)) = payload.get("format") {
        let present = |key: &str| {
            format.get(key).is_some_and(|v| !v.is_null() && v.as_str() != Some("N/A") && v.as_str() != Some(""))
        };
        if present("duration") {
            match probe_float(format.get("duration")) {
                Some(d) if d >= 0.0 => evidence.duration_s = Some(d),
                _ => return Err("PROBE_DURATION_INVALID"),
            }
        }
        if present("start_time") {
            evidence.start_s = Some(probe_float(format.get("start_time")).ok_or("PROBE_START_INVALID")?);
        }
    }
    let frames = match payload.get("frames") {
        None => Vec::new(),
        Some(Value::Array(items)) => items.clone(),
        Some(_) => return Err("PROBE_FRAMES_INVALID"),
    };
    let selected: Vec<i64> = [evidence.audio_index, evidence.video_index].into_iter().flatten().collect();
    if frames.iter().any(|f| !f.is_object()) {
        return Err("PROBE_FRAME_INVALID");
    }
    evidence.frames = frames
        .into_iter()
        .filter_map(|f| match f {
            Value::Object(map) => Some(map),
            _ => None,
        })
        .filter(|f| probe_integer(f.get("stream_index")).is_some_and(|i| selected.contains(&i)))
        .collect();
    Ok(evidence)
}

fn validated_window_offset(value: f64) -> Result<f64, &'static str> {
    if !value.is_finite() || !(0.0..=CAPTURE_STARTUP_S).contains(&value) {
        return Err("WINDOW_OFFSET_INVALID");
    }
    Ok(value)
}

fn validated_window_bounds(start: f64, end: f64) -> Result<(f64, f64), &'static str> {
    if !start.is_finite() || !end.is_finite() || start.abs() > 172_800.0 || end.abs() > 172_800.0 || end <= start {
        return Err("WINDOW_BOUNDS_INVALID");
    }
    Ok((start, end))
}

/// Measure the selected stream's decoded intervals inside the window; gaps are never filled.
pub fn decoded_coverage(
    probe: &ProbeEvidence,
    name: &str,
    sample_s: f64,
    window_offset_s: f64,
    window_bounds: Option<(f64, f64)>,
) -> Result<CoverageEvidence, &'static str> {
    let window_offset_s = validated_window_offset(window_offset_s)?;
    let audio = name == "AUDIO";
    let stream_index = if audio { probe.audio_index } else { probe.video_index };
    let tick = if audio { probe.audio_time_base_s } else { probe.video_time_base_s };
    let valid_tick = tick.is_some_and(|t| t > 0.0 && t <= 0.005);
    let (origin, window_end) = match window_bounds {
        Some((start, end)) => {
            let (start, end) = validated_window_bounds(start, end)?;
            (Some(start), Some(end))
        }
        None => {
            let origin = probe.start_s.map(|s| s + window_offset_s);
            (origin, origin.map(|o| o + sample_s))
        }
    };
    let mut intervals: Vec<(f64, f64)> = Vec::new();
    let mut decoded_amount_s = 0.0;
    let mut sample_count: u64 = 0;
    let mut invalid = origin.is_none() || stream_index.is_none() || !valid_tick;
    let rate = if audio { probe.audio_sample_rate } else { None };
    if audio && !rate.is_some_and(|r| r > 0 && r <= 768_000) {
        invalid = true;
    }
    let wanted_type = if audio { "audio" } else { "video" };
    for frame in &probe.frames {
        if probe_integer(frame.get("stream_index")) != stream_index {
            continue;
        }
        match frame.get("media_type") {
            None | Some(Value::Null) => {}
            Some(Value::String(t)) if t == wanted_type => {}
            Some(_) => {
                invalid = true;
                continue;
            }
        }
        let Some(pts) =
            probe_float(frame.get("pts_time")).or_else(|| probe_float(frame.get("best_effort_timestamp_time")))
        else {
            invalid = true;
            continue;
        };
        let mut samples = 0i64;
        let duration = if audio {
            match (probe_integer(frame.get("nb_samples")), rate) {
                (Some(n), Some(r)) if n > 0 && r > 0 && r <= 768_000 => {
                    samples = n;
                    n as f64 / r as f64
                }
                _ => {
                    invalid = true;
                    continue;
                }
            }
        } else {
            let primary = probe_float(frame.get("duration_time")).filter(|d| *d > 0.0);
            match primary.or_else(|| probe_float(frame.get("pkt_duration_time")).filter(|d| *d > 0.0)) {
                Some(d) => d,
                None => {
                    invalid = true;
                    continue;
                }
            }
        };
        if !duration.is_finite() || duration <= 0.0 || duration > 86_400.0 {
            invalid = true;
            continue;
        }
        let end = pts + duration;
        if !end.is_finite() || end.abs() > 172_800.0 {
            invalid = true;
            continue;
        }
        let (Some(origin), Some(window_end)) = (origin, window_end) else {
            continue;
        };
        let start_clip = pts.max(origin);
        let end_clip = end.min(window_end);
        if end_clip <= start_clip {
            continue;
        }
        let (start_rel, end_rel) = (start_clip - origin, end_clip - origin);
        intervals.push((start_rel, end_rel));
        decoded_amount_s += end_rel - start_rel;
        if audio {
            let included = if start_clip == pts && end_clip == end {
                samples
            } else {
                samples.min((samples as f64 * (end_clip - start_clip) / duration).floor() as i64)
            };
            sample_count += included.max(0) as u64;
        }
    }
    let frame_count = intervals.len() as u64;
    intervals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mut merged: Vec<(f64, f64)> = Vec::new();
    for (start, end) in intervals {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    let covered_s: f64 = merged.iter().map(|(s, e)| e - s).sum();
    let mut gaps: Vec<f64> = Vec::new();
    if let (Some(first), Some(last)) = (merged.first(), merged.last()) {
        gaps.push(first.0);
        gaps.extend(merged.windows(2).map(|w| (w[1].0 - w[0].1).max(0.0)));
        gaps.push((sample_s - last.1).max(0.0));
    } else {
        gaps.push(sample_s);
    }
    let largest_gap_s = gaps.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let tick_value = tick.unwrap_or(0.0);
    // A timestamp tick can move each frame boundary, but cannot hide a long gap.
    let quantization_budget = tick_value * (frame_count as f64 + 1.0);
    let complete = !invalid
        && frame_count >= if audio { 1 } else { 2 }
        && largest_gap_s <= tick_value
        && covered_s + quantization_budget >= sample_s
        && (!audio || rate.is_some_and(|r| sample_count as f64 / r as f64 + tick_value >= sample_s));
    Ok(CoverageEvidence {
        covered_s,
        decoded_amount_s,
        frame_count,
        sample_count,
        sample_rate_hz: rate,
        first_s: merged.first().map(|m| m.0),
        last_s: merged.last().map(|m| m.1),
        largest_gap_s,
        complete,
        window_offset_s,
    })
}

pub fn parse_progress(text: &str) -> Result<ProgressEvidence, &'static str> {
    let mut fields: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for line in text.lines() {
        if let Some((key, value)) = line.split_once('=') {
            fields.insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    let raw_us = fields
        .get("out_time_us")
        .filter(|v| !v.is_empty())
        .or_else(|| fields.get("out_time_ms").filter(|v| !v.is_empty()));
    let mut decoded: Option<f64> = None;
    if let Some(raw) = raw_us {
        if raw != "N/A" && raw != "-1" {
            decoded = Some(python_int_text(raw).ok_or("PROGRESS_TIME_INVALID")? as f64 / 1_000_000.0);
        }
    } else if let Some(out_time) = fields.get("out_time").filter(|v| !v.is_empty()) {
        let parts: Vec<&str> = out_time.split(':').collect();
        if parts.len() == 3 {
            let parse = |p: &str| p.trim().parse::<f64>().map_err(|_| "PROGRESS_TIME_INVALID");
            decoded = Some(parse(parts[0])? * 3600.0 + parse(parts[1])? * 60.0 + parse(parts[2])?);
        }
    }
    let frames = fields.get("frame").and_then(|f| python_int_text(f)).unwrap_or(0);
    if let Some(d) = decoded {
        if !d.is_finite() || d < 0.0 {
            return Err("PROGRESS_TIME_INVALID");
        }
    }
    Ok(ProgressEvidence {
        complete: fields.get("progress").map(String::as_str) == Some("end"),
        decoded_s: decoded,
        frames: frames.max(0),
    })
}

fn word_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// The first `marker` occurrence (optionally at a word boundary) followed by `:\s*number`.
fn find_marker_value(line: &str, marker: &str, boundary: bool) -> Option<f64> {
    let bytes = line.as_bytes();
    let mut search = 0;
    while let Some(offset) = line[search..].find(marker) {
        let at = search + offset;
        search = at + 1;
        if boundary && at > 0 && word_char(bytes[at - 1]) {
            continue;
        }
        let mut i = at + marker.len();
        while i < bytes.len() && (bytes[i] == b' ' || (b'\t'..=b'\r').contains(&bytes[i])) {
            i += 1;
        }
        let start = i;
        if i < bytes.len() && bytes[i] == b'-' {
            i += 1;
        }
        let int_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        let has_int = i > int_start;
        if i < bytes.len() && bytes[i] == b'.' {
            let dot = i;
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if !has_int && i == dot + 1 {
                continue;
            }
        } else if !has_int {
            continue;
        }
        if let Ok(value) = line[start..i].parse::<f64>() {
            return Some(value);
        }
    }
    None
}

/// Pair detector start/end markers; an interval still open at the end runs to `decoded_s`.
pub fn parse_intervals(log: &str, decoded_s: f64) -> Result<(Vec<Interval>, Vec<Interval>), &'static str> {
    let mut pending: [Option<f64>; 2] = [None, None];
    let mut found: [Vec<Interval>; 2] = [Vec::new(), Vec::new()];
    let markers: [(&str, &str, usize, bool, bool); 4] = [
        ("freeze_start", "lavfi.freezedetect.freeze_start:", 0, true, false),
        ("freeze_end", "lavfi.freezedetect.freeze_end:", 0, false, false),
        ("silence_start", "silence_start:", 1, true, true),
        ("silence_end", "silence_end:", 1, false, true),
    ];
    for line in log.lines() {
        for (marker, pattern, kind, is_start, boundary) in markers {
            if !line.contains(marker) {
                continue;
            }
            let value = find_marker_value(line, pattern, boundary).ok_or("DETECTOR_INTERVAL_MALFORMED")?;
            if !value.is_finite() || value < 0.0 {
                return Err("DETECTOR_INTERVAL_INVALID");
            }
            if is_start {
                if pending[kind].is_some() {
                    return Err("DETECTOR_INTERVAL_OVERLAP");
                }
                pending[kind] = Some(value);
            } else {
                match pending[kind] {
                    Some(start) if value >= start => {
                        found[kind].push(Interval { start_s: start, end_s: value, open_at_sample_end: false });
                        pending[kind] = None;
                    }
                    _ => return Err("DETECTOR_INTERVAL_UNPAIRED"),
                }
            }
            break;
        }
    }
    for kind in 0..2 {
        if let Some(start) = pending[kind] {
            if decoded_s < start {
                return Err("DETECTOR_INTERVAL_AFTER_DECODE_END");
            }
            found[kind].push(Interval { start_s: start, end_s: decoded_s, open_at_sample_end: true });
        }
    }
    let [freezes, silences] = found;
    Ok((freezes, silences))
}

pub struct DimensionInput<'a> {
    pub name: &'a str,
    pub expected: bool,
    pub present: bool,
    pub coverage_ok: bool,
    pub decoded_s: Option<f64>,
    pub sampled_s: f64,
    pub intervals: Vec<Interval>,
    pub window_s: f64,
    pub coverage: Option<CoverageEvidence>,
    pub detector_enabled: bool,
    pub decode_error: Option<String>,
}

pub fn dimension(input: DimensionInput<'_>) -> DimensionEvidence {
    let name = input.name;
    let base = |status: &str, reason: String| DimensionEvidence {
        status: status.into(),
        reason_codes: vec![reason],
        intervals: Vec::new(),
        sampled_s: input.sampled_s,
        decoded_s: input.decoded_s,
        coverage: input.coverage.clone(),
    };
    if !input.expected {
        return base("NOT_APPLICABLE", format!("{name}_NOT_EXPECTED"));
    }
    if !input.present {
        return base("UNKNOWN", format!("{name}_MISSING"));
    }
    if let Some(error) = &input.decode_error {
        return base("UNKNOWN", error.clone());
    }
    if !input.detector_enabled {
        return base("UNKNOWN", format!("{name}_DETECTOR_DISABLED"));
    }
    if !input.coverage_ok {
        return base("UNKNOWN", format!("{name}_COVERAGE_INCOMPLETE"));
    }
    let reason = if name == "VIDEO" { "VIDEO_FREEZE" } else { "AUDIO_SILENCE" };
    if input.intervals.iter().any(|i| (i.end_s - i.start_s) + 0.1 >= input.window_s) {
        let mut evidence = base("UNHEALTHY", reason.into());
        evidence.intervals = input.intervals;
        return evidence;
    }
    let mut evidence = base("HEALTHY", format!("{name}_DECODED_NO_EVENT"));
    evidence.intervals = input.intervals;
    evidence
}

/// Combine dimensions: any UNHEALTHY wins, then any UNKNOWN, else HEALTHY.
pub fn overall(audio: &DimensionEvidence, video: &DimensionEvidence) -> (String, Vec<String>) {
    let dims = [audio, video];
    let bad: Vec<String> =
        dims.iter().filter(|d| d.status == "UNHEALTHY").flat_map(|d| d.reason_codes.clone()).collect();
    if !bad.is_empty() {
        return ("UNHEALTHY".into(), bad);
    }
    let unknown: Vec<String> =
        dims.iter().filter(|d| d.status == "UNKNOWN").flat_map(|d| d.reason_codes.clone()).collect();
    if !unknown.is_empty() {
        return ("UNKNOWN".into(), unknown);
    }
    ("HEALTHY".into(), dims.iter().flat_map(|d| d.reason_codes.clone()).collect())
}

pub fn unknown_observation(stream: &StreamConfig, reason: &str, observed_at: Option<String>) -> Observation {
    let audio = if stream.expect_audio {
        DimensionEvidence::new("UNKNOWN", &[reason])
    } else {
        DimensionEvidence::new("NOT_APPLICABLE", &["AUDIO_NOT_EXPECTED"])
    };
    let video = if stream.expect_video {
        DimensionEvidence::new("UNKNOWN", &[reason])
    } else {
        DimensionEvidence::new("NOT_APPLICABLE", &["VIDEO_NOT_EXPECTED"])
    };
    Observation {
        stream_id: stream.id.clone(),
        observed_at: observed_at.unwrap_or_else(crate::timefmt::utc_now),
        status: "UNKNOWN".into(),
        reason_codes: vec![reason.into()],
        audio,
        video,
    }
}

fn is_network_path(text: &str) -> bool {
    let unc = text.starts_with("\\\\") || text.starts_with("//");
    let verbatim_unc = text.starts_with("\\\\?\\UNC\\");
    let verbatim_local = text.starts_with("\\\\?\\") && !verbatim_unc;
    (unc && !verbatim_local) || verbatim_unc
}

/// Accept only a regular local file: no URL, network path, or symbolic link.
pub fn local_sample_path(source: &Path) -> Result<PathBuf, &'static str> {
    let raw = source.to_string_lossy();
    let lower = raw.to_ascii_lowercase();
    if raw.contains("://")
        || ["http:", "https:", "rtmp:", "rtsp:", "udp:", "tcp:", "lavfi:"].iter().any(|p| lower.starts_with(p))
    {
        return Err("SOURCE_NOT_LOCAL");
    }
    if is_network_path(&raw) {
        return Err("SOURCE_NETWORK_PATH");
    }
    let candidate = crate::config::expand_user(&raw);
    if std::fs::symlink_metadata(&candidate).map(|m| m.file_type().is_symlink()).unwrap_or(false) {
        return Err("SOURCE_SYMLINK");
    }
    let resolved = std::fs::canonicalize(&candidate).map_err(|_| "SOURCE_UNAVAILABLE")?;
    if is_network_path(&resolved.to_string_lossy()) {
        return Err("SOURCE_NETWORK_PATH");
    }
    if !resolved.is_file() {
        return Err("SOURCE_NOT_FILE");
    }
    Ok(resolved)
}

pub fn build_decode_argv(
    path: &Path,
    stream: &StreamConfig,
    config: &AppConfig,
    present_audio: bool,
    present_video: bool,
    track: &str,
    window_bounds: Option<(f64, f64)>,
) -> Result<Vec<String>, &'static str> {
    let bounds = match window_bounds {
        Some((s, e)) => Some(validated_window_bounds(s, e)?),
        None => None,
    };
    let mut args: Vec<String> = [
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "info",
        "-protocol_whitelist",
        "file",
        "-threads",
        "1",
        "-filter_threads",
        "1",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if bounds.is_some() {
        args.push("-copyts".into());
    }
    args.extend(["-i".to_string(), path.to_string_lossy().into_owned()]);
    let use_video = track == "video" && stream.expect_video && present_video;
    let use_audio = track == "audio" && stream.expect_audio && present_audio;
    if !(use_video || use_audio) {
        return Err("DECODE_TRACK_UNAVAILABLE");
    }
    let start = bounds.map(|(s, _)| format!("{s:.9}"));
    let end = bounds.map(|(_, e)| format!("{e:.9}"));
    if use_video {
        let mut filter =
            format!("freezedetect=n={}:d={}", float_repr(stream.freeze_noise), float_repr(stream.freeze_window_s));
        if let (Some(start), Some(end)) = (&start, &end) {
            filter = format!("trim=start={start}:end={end},setpts=PTS-({start})/TB,{filter}");
        }
        args.extend(["-map".into(), "0:v:0".into(), "-vf".into(), filter]);
    }
    if use_audio {
        let mut filter =
            format!("silencedetect=n={}dB:d={}", float_repr(stream.silence_db), float_repr(stream.silence_window_s));
        if let (Some(start), Some(end)) = (&start, &end) {
            filter = format!("atrim=start={start}:end={end},asetpts=PTS-({start})/TB,{filter}");
        }
        args.extend(["-map".into(), "0:a:0".into(), "-af".into(), filter]);
    }
    args.extend([
        "-t".into(),
        float_repr(config.watch.sample_s),
        "-progress".into(),
        "pipe:1".into(),
        "-nostats".into(),
        "-f".into(),
        "null".into(),
        "-".into(),
    ]);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    build_argv(&config.tools.ffmpeg, &refs).map_err(|_| "DECODE_FAILED")
}

/// Inspect one bounded local sample. Freeze and silence are separate dimensions.
pub fn inspect_sample(
    source: &Path,
    stream: &StreamConfig,
    config: &AppConfig,
    runner: Runner<'_>,
    deadline: Option<f64>,
    window_offset_s: f64,
    monotonic: &dyn Fn() -> f64,
) -> Observation {
    let observed_at = crate::timefmt::utc_now();
    let unknown = |reason: &str| unknown_observation(stream, reason, Some(observed_at.clone()));
    let Ok(window_offset_s) = validated_window_offset(window_offset_s) else {
        return unknown("SAMPLE_WINDOW_INVALID");
    };
    let path = match local_sample_path(source) {
        Ok(path) => path,
        Err(_) => return unknown("SAMPLE_UNAVAILABLE_OR_NOT_LOCAL"),
    };
    match std::fs::metadata(&path) {
        Ok(meta) if meta.len() > 0 && meta.len() <= config.watch.max_sample_bytes => {}
        Ok(_) => return unknown("SAMPLE_SIZE_INVALID"),
        Err(_) => return unknown("SAMPLE_UNAVAILABLE_OR_NOT_LOCAL"),
    }
    let interval = format!("%+{}", float_repr(config.watch.sample_s + window_offset_s));
    let path_text = path.to_string_lossy().into_owned();
    let probe_argv = match build_argv(
        &config.tools.ffprobe,
        &[
            "-protocol_whitelist",
            "file",
            "-v",
            "error",
            "-threads",
            "1",
            "-show_frames",
            "-read_intervals",
            &interval,
            "-show_entries",
            "format=start_time,duration:stream=index,codec_type,sample_rate,time_base:frame=stream_index,media_type,pts_time,best_effort_timestamp_time,duration_time,pkt_duration_time,nb_samples",
            "-of",
            "json",
            &path_text,
        ],
    ) {
        Ok(argv) => argv,
        Err(_) => return unknown("PROBE_FAILED"),
    };
    let Some(timeout) = remaining(config.watch.analysis_timeout_s, deadline, monotonic()) else {
        return unknown("CYCLE_BUDGET_EXHAUSTED");
    };
    let probe_result = runner(&probe_argv, timeout, &RunOptions::output(config.watch.max_output_bytes));
    if probe_result.bounded_failure() {
        return unknown("PROBE_BOUNDED_FAILURE");
    }
    if !probe_result.succeeded() {
        return unknown("PROBE_FAILED");
    }
    let probe = match parse_probe_json(&probe_result.stdout) {
        Ok(probe) => probe,
        Err(reason) => return unknown(reason),
    };
    if probe.start_s.is_none() && window_offset_s > 0.0 {
        return unknown("PROBE_START_MISSING");
    }
    let mut window_bounds = None;
    if let Some(start) = probe.start_s {
        let window_start = start + window_offset_s;
        match validated_window_bounds(window_start, window_start + config.watch.sample_s) {
            Ok(bounds) => window_bounds = Some(bounds),
            Err(_) => return unknown("PROBE_WINDOW_INVALID"),
        }
    }
    let present_audio = probe.audio_streams > 0;
    let present_video = probe.video_streams > 0;
    let decode_dimension = |name: &str, present: bool| -> DimensionEvidence {
        let audio = name == "AUDIO";
        let expected = if audio { stream.expect_audio } else { stream.expect_video };
        let window = if audio { stream.silence_window_s } else { stream.freeze_window_s };
        let enabled = audio || stream.detect_freeze;
        let input =
            |decode_error: Option<String>, decoded_s: Option<f64>, coverage: Option<CoverageEvidence>| DimensionInput {
                name,
                expected,
                present,
                coverage_ok: false,
                decoded_s,
                sampled_s: config.watch.sample_s,
                intervals: Vec::new(),
                window_s: window,
                coverage,
                detector_enabled: enabled,
                decode_error,
            };
        if !expected || !present {
            return dimension(input(None, None, None));
        }
        let Some(timeout) = remaining(config.watch.analysis_timeout_s, deadline, monotonic()) else {
            return dimension(input(Some("CYCLE_BUDGET_EXHAUSTED".into()), None, None));
        };
        let bounds = if window_offset_s > 0.0 { window_bounds } else { None };
        let argv = match build_decode_argv(
            &path,
            stream,
            config,
            present_audio,
            present_video,
            &name.to_lowercase(),
            bounds,
        ) {
            Ok(argv) => argv,
            Err(_) => return dimension(input(Some("DECODE_FAILED".into()), None, None)),
        };
        let decoded: ChildResult = runner(&argv, timeout, &RunOptions::output(config.watch.max_output_bytes));
        if decoded.bounded_failure() {
            return dimension(input(Some("DECODE_BOUNDED_FAILURE".into()), None, None));
        }
        if !decoded.succeeded() {
            return dimension(input(Some("DECODE_FAILED".into()), None, None));
        }
        match parse_progress(&decoded.stdout) {
            Ok(progress) if progress.complete => {}
            Ok(_) => return dimension(input(Some("DECODE_COVERAGE_MISSING".into()), None, None)),
            Err(reason) => return dimension(input(Some(reason.into()), None, None)),
        }
        let coverage = match decoded_coverage(&probe, name, config.watch.sample_s, window_offset_s, window_bounds) {
            Ok(coverage) => coverage,
            Err(reason) => return dimension(input(Some(reason.into()), None, None)),
        };
        let decoded_s = Some(coverage.covered_s);
        let detector_end = coverage.last_s.unwrap_or(0.0);
        match parse_intervals(&decoded.stderr, detector_end) {
            Err(reason) => dimension(input(Some(reason.into()), decoded_s, Some(coverage))),
            Ok((freezes, silences)) => {
                let complete = coverage.complete;
                let mut full = input(None, decoded_s, Some(coverage));
                full.coverage_ok = complete;
                full.intervals = if audio { silences } else { freezes };
                dimension(full)
            }
        }
    };
    let audio = decode_dimension("AUDIO", present_audio);
    let video = decode_dimension("VIDEO", present_video);
    let (status, reason_codes) = overall(&audio, &video);
    Observation { stream_id: stream.id.clone(), observed_at, status, reason_codes, audio, video }
}

pub fn build_capture_argv(
    media: &ResolvedMedia,
    stream: &StreamConfig,
    config: &AppConfig,
    output: &Path,
) -> Result<Vec<String>, &'static str> {
    let mut args: Vec<String> =
        ["-nostdin", "-hide_banner", "-loglevel", "error", "-threads", "1", "-filter_threads", "1"]
            .iter()
            .map(|s| s.to_string())
            .collect();
    for item in &media.inputs {
        args.extend(["-protocol_whitelist".to_string(), INPUT_PROTOCOLS.to_string()]);
        // Older FFmpeg builds reject unused TLS options on plain HTTP inputs, and some older
        // builds accept untrusted certificates unless verification is requested explicitly.
        let scheme = crate::url::urlsplit(&item.url).map(|u| u.scheme).unwrap_or_default();
        if scheme == "https" {
            args.extend(["-tls_verify".to_string(), "1".to_string()]);
        }
        args.extend(["-rw_timeout".to_string(), "10000000".to_string(), "-i".to_string(), item.url.clone()]);
    }
    let mut maps: Vec<String> = Vec::new();
    if stream.expect_video {
        if let Some(index) = media.inputs.iter().position(|i| i.kind == "video" || i.kind == "muxed") {
            maps.extend(["-map".into(), format!("{index}:v:0?")]);
        }
    }
    if stream.expect_audio {
        if let Some(index) = media.inputs.iter().position(|i| i.kind == "audio" || i.kind == "muxed") {
            maps.extend(["-map".into(), format!("{index}:a:0?")]);
        }
    }
    if maps.is_empty() {
        return Err("RESOLVED_MEDIA_HAS_NO_EXPECTED_TRACK");
    }
    args.extend(["-t".into(), float_repr(config.watch.sample_s + CAPTURE_STARTUP_S)]);
    args.extend(maps);
    args.extend([
        "-c".into(),
        "copy".into(),
        "-fs".into(),
        config.watch.max_sample_bytes.to_string(),
        "-f".into(),
        "matroska".into(),
        output.to_string_lossy().into_owned(),
    ]);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    build_argv(&config.tools.ffmpeg, &refs).map_err(|_| "CAPTURE_SETUP_FAILED")
}

/// A private temporary directory removed on drop (including during an unwind).
pub struct TempDir {
    pub path: PathBuf,
}

impl TempDir {
    pub fn new(prefix: &str) -> std::io::Result<Self> {
        let path = std::env::temp_dir().join(format!("{prefix}{}", &crate::random::uuid4_hex()[..16]));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&path)?;
        Ok(TempDir { path })
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Capture a short local Matroska sample (100 ms acquisition prefix plus the full window),
/// inspect it, and remove it.
pub fn capture_and_inspect(
    media: &ResolvedMedia,
    stream: &StreamConfig,
    config: &AppConfig,
    runner: Runner<'_>,
    deadline: Option<f64>,
    monotonic: &dyn Fn() -> f64,
) -> Observation {
    let Ok(directory) = TempDir::new("dead-air-") else {
        return unknown_observation(stream, "CAPTURE_SETUP_FAILED", None);
    };
    let output = directory.path.join("sample.mkv");
    let argv = match build_capture_argv(media, stream, config, &output) {
        Ok(argv) => argv,
        Err(_) => return unknown_observation(stream, "CAPTURE_SETUP_FAILED", None),
    };
    let Some(timeout) = remaining(config.watch.capture_timeout_s, deadline, monotonic()) else {
        return unknown_observation(stream, "CYCLE_BUDGET_EXHAUSTED", None);
    };
    let options = RunOptions {
        max_output_bytes: config.watch.max_output_bytes,
        file_limits: vec![(output.clone(), config.watch.max_sample_bytes)],
    };
    let result = runner(&argv, timeout, &options);
    if result.bounded_failure() {
        return unknown_observation(stream, "CAPTURE_BOUNDED_FAILURE", None);
    }
    if !result.succeeded() {
        return unknown_observation(stream, "CAPTURE_FAILED", None);
    }
    let size = match std::fs::metadata(&output) {
        Ok(meta) => meta.len(),
        Err(_) => return unknown_observation(stream, "CAPTURE_FILE_MISSING", None),
    };
    if size == 0 || size >= config.watch.max_sample_bytes {
        return unknown_observation(stream, "CAPTURE_SIZE_INVALID", None);
    }
    inspect_sample(&output, stream, config, runner, deadline, CAPTURE_STARTUP_S, monotonic)
}
