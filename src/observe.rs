//! Source resolution, bounded observation of one stream, and the serial cycle.

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::constants::{OFF_AIR_LIVE_STATUS, UPCOMING_LIVE_STATUS, YOUTUBE_HOSTS, YTDLP_FLAGS};
use crate::http::{GetOptions, Transport};
use crate::interrupt;
use crate::media::{capture_and_inspect, unknown_observation};
use crate::model::*;
use crate::netpolicy::{Resolver, remaining, validate_network_target};
use crate::process::{RunOptions, Runner, build_argv};
use crate::url::{query_first, urlsplit};

/// Fixed metadata-only resolver command; no config, plugins, cookies, or download.
pub fn build_ytdlp_argv(executable: &str, url: &str) -> Vec<String> {
    let mut args: Vec<&str> = YTDLP_FLAGS.to_vec();
    args.extend(["--", url]);
    build_argv(executable, &args).unwrap_or_else(|_| vec![executable.to_string()])
}

fn not_none(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::String(s)) => !(s.is_empty() || s == "none" || s == "None"),
        Some(_) => true,
    }
}

fn truthy(value: Option<&Value>) -> Option<&Value> {
    match value {
        None | Some(Value::Null) | Some(Value::Bool(false)) => None,
        Some(Value::String(s)) if s.is_empty() => None,
        Some(Value::Number(n)) if n.as_f64() == Some(0.0) => None,
        Some(Value::Array(a)) if a.is_empty() => None,
        Some(Value::Object(o)) if o.is_empty() => None,
        other => other,
    }
}

fn as_float(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        Value::Bool(b) => Some(f64::from(u8::from(*b))),
        _ => None,
    }
}

fn format_rank(item: &serde_json::Map<String, Value>) -> (f64, f64) {
    let height = truthy(item.get("height")).map_or(Some(0.0), |v| as_float(Some(v))).unwrap_or(0.0);
    let bitrate = truthy(item.get("tbr"))
        .or_else(|| truthy(item.get("abr")))
        .map_or(Some(0.0), |v| as_float(Some(v)))
        .unwrap_or(0.0);
    (if height <= 720.0 { height } else { 0.0 }, bitrate)
}

fn within_video_limit(item: &serde_json::Map<String, Value>) -> bool {
    match item.get("height") {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) if s.is_empty() || s == "0" => true,
        Some(Value::Number(n)) if n.as_f64() == Some(0.0) => true,
        Some(Value::Bool(false)) => true,
        other => as_float(other).is_some_and(|h| h <= 720.0),
    }
}

/// Python `max(items, key=rank)`: the first item with the greatest rank.
fn best<'a>(items: &[&'a serde_json::Map<String, Value>]) -> Option<&'a serde_json::Map<String, Value>> {
    type Ranked<'b> = (&'b serde_json::Map<String, Value>, (f64, f64));
    let mut chosen: Option<Ranked<'a>> = None;
    for item in items {
        let rank = format_rank(item);
        if chosen.as_ref().is_none_or(|(_, best)| rank.partial_cmp(best) == Some(std::cmp::Ordering::Greater)) {
            chosen = Some((item, rank));
        }
    }
    chosen.map(|(item, _)| item)
}

pub type Selection = (Option<ResolvedMedia>, Option<&'static str>, Option<&'static str>);

/// Choose media from resolver JSON: media, an explicit off-air reason, or an UNKNOWN reason.
pub fn select_resolved_media(
    payload: &Value,
    stream: &StreamConfig,
    network: &NetworkConfig,
    resolver: &Resolver,
    deadline: Option<f64>,
) -> Selection {
    let Value::Object(payload) = payload else {
        return (None, None, Some("RESOLVER_JSON_INVALID"));
    };
    let live_status = payload.get("live_status").and_then(Value::as_str);
    if stream.expected_live && live_status.is_some_and(|s| OFF_AIR_LIVE_STATUS.contains(&s)) {
        return (None, Some("RESOLVER_NOT_LIVE"), None);
    }
    if stream.expected_live && live_status.is_some_and(|s| UPCOMING_LIVE_STATUS.contains(&s)) {
        return (None, Some("RESOLVER_UPCOMING"), None);
    }
    let mut candidates: Vec<serde_json::Map<String, Value>> = match payload.get("formats") {
        Some(Value::Array(items)) => items.iter().filter_map(|i| i.as_object().cloned()).collect(),
        _ => Vec::new(),
    };
    if let Some(Value::String(url)) = payload.get("url") {
        let mut top = serde_json::Map::new();
        top.insert("url".into(), Value::String(url.clone()));
        for key in ["vcodec", "acodec", "height", "tbr"] {
            top.insert(key.into(), payload.get(key).cloned().unwrap_or(Value::Null));
        }
        candidates.push(top);
    }
    let candidates: Vec<&serde_json::Map<String, Value>> =
        candidates.iter().filter(|i| i.get("url").is_some_and(Value::is_string)).collect();
    let video: Vec<_> =
        candidates.iter().copied().filter(|i| not_none(i.get("vcodec")) && within_video_limit(i)).collect();
    let audio: Vec<_> = candidates.iter().copied().filter(|i| not_none(i.get("acodec"))).collect();
    let muxed: Vec<_> = candidates
        .iter()
        .copied()
        .filter(|i| not_none(i.get("vcodec")) && not_none(i.get("acodec")) && within_video_limit(i))
        .collect();
    let url_of = |item: &serde_json::Map<String, Value>| item["url"].as_str().unwrap_or_default().to_string();
    let mut chosen: Vec<MediaInput> = Vec::new();
    if stream.expect_video && stream.expect_audio && !muxed.is_empty() {
        if let Some(item) = best(&muxed) {
            chosen.push(MediaInput { kind: "muxed".into(), url: url_of(item) });
        }
    } else {
        let video_only: Vec<_> = video.iter().copied().filter(|i| !not_none(i.get("acodec"))).collect();
        let audio_only: Vec<_> = audio.iter().copied().filter(|i| !not_none(i.get("vcodec"))).collect();
        if stream.expect_video && !video.is_empty() {
            let pool = if video_only.is_empty() { &video } else { &video_only };
            if let Some(item) = best(pool) {
                chosen.push(MediaInput { kind: "video".into(), url: url_of(item) });
            }
        }
        if stream.expect_audio && !audio.is_empty() {
            let pool = if audio_only.is_empty() { &audio } else { &audio_only };
            if let Some(item) = best(pool) {
                chosen.push(MediaInput { kind: "audio".into(), url: url_of(item) });
            }
        }
    }
    if chosen.is_empty() || chosen.len() > 2 {
        return (None, None, Some("RESOLVER_MEDIA_MISSING"));
    }
    for media in &chosen {
        if validate_network_target(&media.url, "direct", network, stream.allow_private, resolver, false, deadline)
            .is_err()
        {
            return (None, None, Some("RESOLVER_MEDIA_URL_REJECTED"));
        }
    }
    (Some(ResolvedMedia { inputs: chosen }), None, None)
}

fn ascii_strip(bytes: &[u8]) -> &[u8] {
    let ws = |b: &u8| matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c);
    let start = bytes.iter().position(|b| !ws(b)).unwrap_or(bytes.len());
    let end = bytes.iter().rposition(|b| !ws(b)).map_or(start, |e| e + 1);
    &bytes[start..end.max(start)]
}

/// Python `bytes.splitlines()`.
fn split_lines(body: &[u8]) -> Vec<&[u8]> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < body.len() {
        match body[i] {
            b'\n' => {
                lines.push(&body[start..i]);
                start = i + 1;
            }
            b'\r' => {
                lines.push(&body[start..i]);
                if body.get(i + 1) == Some(&b'\n') {
                    i += 1;
                }
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    if start < body.len() {
        lines.push(&body[start..]);
    }
    lines
}

/// Explicit off-air evidence from a direct probe: a configured status or a complete HLS
/// playlist that carries `#EXT-X-ENDLIST` on its own line.
pub fn off_air_from_direct(result: &HttpResult, stream: &StreamConfig) -> Option<&'static str> {
    if !stream.expected_live {
        return None;
    }
    if result.status.is_some_and(|s| stream.offline_http_statuses.contains(&i64::from(s))) {
        return Some("DIRECT_OFFLINE_STATUS");
    }
    let path = urlsplit(&result.final_url).map(|u| u.path.to_lowercase()).unwrap_or_default();
    let is_hls = path.ends_with(".m3u8") || result.content_type.contains("mpegurl");
    if is_hls
        && result.error_code.is_none()
        && result.status.is_some_and(|s| (200..300).contains(&s))
        && !result.truncated
    {
        let lines = split_lines(&result.body);
        if let Some(first) = lines.first() {
            let first = first.strip_prefix(b"\xef\xbb\xbf".as_slice()).unwrap_or(first);
            if ascii_strip(first) == b"#EXTM3U" && lines[1..].iter().any(|l| ascii_strip(l) == b"#EXT-X-ENDLIST") {
                return Some("DIRECT_HLS_ENDLIST");
            }
        }
    }
    None
}

/// The keyless public thumbnail for a YouTube watch, live or short link.
pub fn youtube_thumbnail_url(url: &str) -> Option<String> {
    let parsed = urlsplit(url).ok()?;
    let host = parsed.hostname().unwrap_or_default();
    let video_id = if host == "youtu.be" {
        parsed.path.trim_matches('/').split('/').next().unwrap_or("").to_string()
    } else if parsed.path.trim_end_matches('/') == "/watch" {
        query_first(&parsed.query, "v", false)
    } else {
        parsed.path.strip_prefix("/live/")?.split('/').next().unwrap_or("").to_string()
    };
    let valid = video_id.len() == 11 && video_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if !YOUTUBE_HOSTS.contains(&host.as_str()) || !valid {
        return None;
    }
    Some(format!("https://i.ytimg.com/vi/{video_id}/hqdefault.jpg"))
}

/// Everything an observation reaches outside itself; tests substitute each piece.
pub struct ObserveDeps<'a> {
    pub runner: Runner<'a>,
    pub resolver: &'a Resolver,
    pub transport: Transport<'a>,
    pub monotonic: &'a dyn Fn() -> f64,
}

/// Thumbnail fallback after an UNKNOWN: at best DEGRADED, never LIVE.
pub fn fallback_observation(
    original: Observation,
    stream: &StreamConfig,
    config: &AppConfig,
    history: &mut HashMap<String, ThumbnailBaseline>,
    deps: &ObserveDeps<'_>,
    now: f64,
    deadline: Option<f64>,
) -> Observation {
    let thumbnail_url = if !stream.thumbnail_url.is_empty() {
        Some(stream.thumbnail_url.clone())
    } else if stream.source == "youtube" {
        youtube_thumbnail_url(&stream.url)
    } else {
        None
    };
    let Some(thumbnail_url) = thumbnail_url else {
        return original;
    };
    if validate_network_target(
        &thumbnail_url,
        "direct",
        &config.network,
        stream.allow_private,
        deps.resolver,
        true,
        deadline,
    )
    .is_err()
    {
        return original;
    }
    let Some(timeout) = remaining(config.watch.analysis_timeout_s, deadline, (deps.monotonic)()) else {
        return original;
    };
    let fetched = (deps.transport)(
        &thumbnail_url,
        &GetOptions {
            source: "direct",
            network: &config.network,
            allow_private: stream.allow_private,
            timeout_s: timeout,
            max_bytes: config.network.thumbnail_max_bytes,
            resolver: deps.resolver,
            range_request: false,
            deadline,
        },
    );
    if fetched.error_code.is_some()
        || !fetched.status.is_some_and(|s| (200..300).contains(&s))
        || !fetched.content_type.starts_with("image/")
        || fetched.truncated
        || fetched.body.is_empty()
    {
        return original;
    }
    let digest = crate::random::hex(&Sha256::digest(&fetched.body));
    let reason = match history.get(&stream.id) {
        None => {
            history.insert(stream.id.clone(), ThumbnailBaseline { digest, observed_monotonic: now });
            "THUMBNAIL_BASELINE_ONLY"
        }
        Some(previous) if previous.digest != digest => {
            history.insert(stream.id.clone(), ThumbnailBaseline { digest, observed_monotonic: now });
            "THUMBNAIL_VISUAL_ACTIVITY_ONLY"
        }
        Some(previous) if now - previous.observed_monotonic >= stream.thumbnail_stale_after_s => {
            "THUMBNAIL_SUSPECT_STATIC"
        }
        Some(_) => "THUMBNAIL_UNCHANGED_BELOW_STALE_WINDOW",
    };
    Observation {
        stream_id: stream.id.clone(),
        observed_at: original.observed_at,
        status: "DEGRADED".into(),
        reason_codes: vec![reason.into(), "FULL_MEDIA_UNVERIFIED".into()],
        audio: DimensionEvidence::new("UNKNOWN", &["AUDIO_UNVERIFIED_BY_THUMBNAIL"]),
        video: DimensionEvidence::new("DEGRADED", &[reason]),
    }
}

fn off_air_observation(stream: &StreamConfig, reason: &str) -> Observation {
    let audio = if stream.expect_audio {
        DimensionEvidence::new("UNKNOWN", &["NO_MEDIA_SAMPLE_AFTER_OFF_AIR"])
    } else {
        DimensionEvidence::new("NOT_APPLICABLE", &["AUDIO_NOT_EXPECTED"])
    };
    let video = if stream.expect_video {
        DimensionEvidence::new("UNKNOWN", &["NO_MEDIA_SAMPLE_AFTER_OFF_AIR"])
    } else {
        DimensionEvidence::new("NOT_APPLICABLE", &["VIDEO_NOT_EXPECTED"])
    };
    Observation {
        stream_id: stream.id.clone(),
        observed_at: crate::timefmt::utc_now(),
        status: "OFF_AIR".into(),
        reason_codes: vec![reason.into()],
        audio,
        video,
    }
}

/// Resolve one configured source and perform one bounded observation.
pub fn observe_stream(
    config: &AppConfig,
    stream: &StreamConfig,
    deps: &ObserveDeps<'_>,
    thumbnail_history: Option<&mut HashMap<String, ThumbnailBaseline>>,
    deadline: Option<f64>,
) -> Observation {
    if let Err(error) = validate_network_target(
        &stream.url,
        &stream.source,
        &config.network,
        stream.allow_private,
        deps.resolver,
        false,
        deadline,
    ) {
        return unknown_observation(stream, &error.0, None);
    }
    let mut history = thumbnail_history;
    let mut fallback_unknown = |original: Observation| -> Observation {
        if original.status != "UNKNOWN" || !stream.thumbnail_enabled {
            return original;
        }
        match history.as_deref_mut() {
            Some(history) => {
                fallback_observation(original, stream, config, history, deps, (deps.monotonic)(), deadline)
            }
            None => original,
        }
    };
    let media = if stream.source == "direct" {
        let Some(timeout) = remaining(config.watch.resolver_timeout_s, deadline, (deps.monotonic)()) else {
            return unknown_observation(stream, "CYCLE_BUDGET_EXHAUSTED", None);
        };
        let result = (deps.transport)(
            &stream.url,
            &GetOptions {
                source: "direct",
                network: &config.network,
                allow_private: stream.allow_private,
                timeout_s: timeout,
                max_bytes: 65_536,
                resolver: deps.resolver,
                range_request: true,
                deadline,
            },
        );
        if let Some(reason) = off_air_from_direct(&result, stream) {
            return off_air_observation(stream, reason);
        }
        if !result.status.is_some_and(|s| (200..300).contains(&s)) {
            return unknown_observation(stream, result.error_code.as_deref().unwrap_or("DIRECT_PROBE_UNKNOWN"), None);
        }
        if validate_network_target(
            &result.final_url,
            "direct",
            &config.network,
            stream.allow_private,
            deps.resolver,
            false,
            deadline,
        )
        .is_err()
        {
            return unknown_observation(stream, "DIRECT_REDIRECT_REJECTED", None);
        }
        ResolvedMedia { inputs: vec![MediaInput { kind: "muxed".into(), url: result.final_url }] }
    } else {
        let argv = build_ytdlp_argv(&config.tools.yt_dlp, &stream.url);
        let Some(timeout) = remaining(config.watch.resolver_timeout_s, deadline, (deps.monotonic)()) else {
            return unknown_observation(stream, "CYCLE_BUDGET_EXHAUSTED", None);
        };
        let result = (deps.runner)(&argv, timeout, &RunOptions::output(config.watch.max_output_bytes));
        if result.timed_out || result.output_limited || !result.cleanup_complete {
            return fallback_unknown(unknown_observation(stream, "RESOLVER_BOUNDED_FAILURE", None));
        }
        if !result.succeeded() {
            return fallback_unknown(unknown_observation(stream, "RESOLVER_FAILED", None));
        }
        let (media, off_air, error) = match serde_json::from_str::<Value>(&result.stdout) {
            Ok(payload) => select_resolved_media(&payload, stream, &config.network, deps.resolver, deadline),
            Err(_) => (None, None, Some("RESOLVER_JSON_INVALID")),
        };
        if let Some(reason) = off_air {
            return off_air_observation(stream, reason);
        }
        match (media, error) {
            (Some(media), None) => media,
            (_, error) => {
                let original = unknown_observation(stream, error.unwrap_or("RESOLVER_MEDIA_MISSING"), None);
                if error == Some("RESOLVER_MEDIA_URL_REJECTED") {
                    return original;
                }
                return fallback_unknown(original);
            }
        }
    };
    let observation = capture_and_inspect(&media, stream, config, deps.runner, deadline, deps.monotonic);
    fallback_unknown(observation)
}

/// Observer seam for [`check_cycle`]: `(config, stream, deadline, thumbnail_history)`.
pub type Observer<'a> =
    dyn FnMut(&AppConfig, &StreamConfig, f64, &mut HashMap<String, ThumbnailBaseline>) -> Observation + 'a;

/// Run each stream at most once. Each remaining stream gets an equal share of the remaining
/// cycle budget, so one slow source cannot consume every later source's window.
pub fn check_cycle(
    config: &AppConfig,
    observer: &mut Observer<'_>,
    clock: &dyn Fn() -> f64,
    thumbnail_history: &mut HashMap<String, ThumbnailBaseline>,
) -> Vec<Observation> {
    let started = clock();
    let count = config.streams.len();
    let mut results = Vec::with_capacity(count);
    for (index, stream) in config.streams.iter().enumerate() {
        interrupt::check();
        let now = clock();
        let left = started + config.watch.cycle_budget_s - now;
        if left <= 0.0 {
            results.push(unknown_observation(stream, "CYCLE_BUDGET_EXHAUSTED", None));
            continue;
        }
        let stream_deadline = now + left / (count - index) as f64;
        let outcome = catch_unwind(AssertUnwindSafe(|| observer(config, stream, stream_deadline, thumbnail_history)));
        let result = match outcome {
            Ok(observation) => observation,
            Err(payload) if interrupt::is_control_payload(payload.as_ref()) => resume_unwind(payload),
            Err(_) => unknown_observation(stream, "OBSERVATION_INTERNAL_ERROR", None),
        };
        results.push(result);
    }
    results
}

/// The production observer: real children, system DNS, the bounded HTTP client.
pub fn default_observer()
-> impl FnMut(&AppConfig, &StreamConfig, f64, &mut HashMap<String, ThumbnailBaseline>) -> Observation {
    |config, stream, deadline, history| {
        let resolver = Resolver::System;
        let runner = crate::process::run_bounded;
        let transport = crate::http::bounded_http_get;
        let deps = ObserveDeps {
            runner: &runner,
            resolver: &resolver,
            transport: &transport,
            monotonic: &crate::timefmt::monotonic,
        };
        observe_stream(config, stream, &deps, Some(history), Some(deadline))
    }
}

/// Per-condition `good` / `bad` / `unknown` signals, in the Python release's order.
pub fn incident_signals(observation: &Observation) -> Vec<(&'static str, &'static str)> {
    let status = observation.status.as_str();
    let known = matches!(status, "HEALTHY" | "UNHEALTHY" | "OFF_AIR");
    let unknown = status == "UNKNOWN"
        || (status != "OFF_AIR"
            && status != "DEGRADED"
            && (observation.audio.status == "UNKNOWN" || observation.video.status == "UNKNOWN"));
    let fresh = |value: &DimensionEvidence| {
        let invalid = value.reason_codes.iter().any(|r| {
            r.ends_with("_DETECTOR_DISABLED")
                || r.ends_with("_MISSING")
                || r.contains("STALE")
                || r.contains("THUMBNAIL")
        });
        let sampled = bounded_number(value.sampled_s).unwrap_or(0.0);
        let decoded = value.decoded_s.and_then(bounded_number);
        matches!(value.status.as_str(), "HEALTHY" | "UNHEALTHY")
            && !invalid
            && sampled > 0.0
            && decoded.is_some_and(|d| d > 0.0)
    };
    let healthy = |value: &DimensionEvidence| value.status == "HEALTHY" && fresh(value);
    let pick = |bad: bool, good: bool| {
        if bad {
            "bad"
        } else if good {
            "good"
        } else {
            "unknown"
        }
    };
    let media_good = fresh(&observation.audio) || fresh(&observation.video);
    vec![
        (
            "off_air",
            if media_good {
                "good"
            } else if status == "OFF_AIR" {
                "bad"
            } else {
                "unknown"
            },
        ),
        ("video_freeze", pick(observation.video.has_reason("VIDEO_FREEZE"), healthy(&observation.video))),
        ("video_missing", pick(observation.video.has_reason("VIDEO_MISSING"), fresh(&observation.video))),
        ("audio_silence", pick(observation.audio.has_reason("AUDIO_SILENCE"), healthy(&observation.audio))),
        ("audio_missing", pick(observation.audio.has_reason("AUDIO_MISSING"), fresh(&observation.audio))),
        ("unknown", pick(unknown, known)),
        ("degraded", pick(status == "DEGRADED", known)),
    ]
}
