//! Configuration, evidence and result types.

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::fmt;
use std::path::PathBuf;

use crate::pyjson::{self, float};

macro_rules! error_type {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Debug, PartialEq, Eq)]
        pub struct $name(pub String);
        impl $name {
            pub fn new(message: impl Into<String>) -> Self {
                Self(message.into())
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
        impl std::error::Error for $name {}
    };
}

error_type!(ConfigError, "Invalid, incomplete, or unknown TOML configuration.");
error_type!(PolicyError, "Input violates the configured URL or private-network policy.");
error_type!(PersistenceError, "A durable state operation failed; monitoring must stop visibly.");

#[derive(Clone, Debug, PartialEq)]
pub struct WatchConfig {
    pub interval_s: f64,
    pub sample_s: f64,
    pub resolver_timeout_s: f64,
    pub capture_timeout_s: f64,
    pub analysis_timeout_s: f64,
    pub max_sample_bytes: u64,
    pub max_output_bytes: usize,
    pub cycle_budget_s: f64,
    pub state_path: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolsConfig {
    pub yt_dlp: String,
    pub ffmpeg: String,
    pub ffprobe: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct NetworkConfig {
    pub private_allowlist: Vec<String>,
    pub max_redirects: u32,
    pub thumbnail_max_bytes: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StreamConfig {
    pub id: String,
    pub label: String,
    pub url: String,
    pub source: String,
    pub expected_live: bool,
    pub expect_audio: bool,
    pub expect_video: bool,
    pub detect_freeze: bool,
    pub freeze_noise: f64,
    pub freeze_window_s: f64,
    pub silence_db: f64,
    pub silence_window_s: f64,
    pub unhealthy_after_s: f64,
    pub min_bad_samples: i64,
    pub recovery_samples: i64,
    pub max_gap_s: f64,
    pub unknown_notice_after_s: f64,
    pub offline_http_statuses: Vec<i64>,
    pub thumbnail_enabled: bool,
    pub thumbnail_url: String,
    pub thumbnail_stale_after_s: f64,
    pub allow_private: bool,
}

impl StreamConfig {
    /// The Python `dataclasses.asdict(stream)` view, used for the configuration fingerprint.
    pub fn as_value(&self) -> Value {
        json!({
            "id": self.id, "label": self.label, "url": self.url, "source": self.source,
            "expected_live": self.expected_live, "expect_audio": self.expect_audio,
            "expect_video": self.expect_video, "detect_freeze": self.detect_freeze,
            "freeze_noise": float(self.freeze_noise), "freeze_window_s": float(self.freeze_window_s),
            "silence_db": float(self.silence_db), "silence_window_s": float(self.silence_window_s),
            "unhealthy_after_s": float(self.unhealthy_after_s),
            "min_bad_samples": self.min_bad_samples, "recovery_samples": self.recovery_samples,
            "max_gap_s": float(self.max_gap_s), "unknown_notice_after_s": float(self.unknown_notice_after_s),
            "offline_http_statuses": self.offline_http_statuses,
            "thumbnail_enabled": self.thumbnail_enabled, "thumbnail_url": self.thumbnail_url,
            "thumbnail_stale_after_s": float(self.thumbnail_stale_after_s),
            "allow_private": self.allow_private,
        })
    }

    /// Invalidates stored health when a configured source or detector changes. The digest is
    /// byte-compatible with the Python release, so upgrades keep their freshness.
    pub fn fingerprint(&self) -> String {
        let text = pyjson::dumps(&self.as_value());
        crate::random::hex(&Sha256::digest(text.as_bytes()))
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DestinationConfig {
    pub id: String,
    pub kind: String,
    pub endpoint_env: Option<String>,
    pub token_env: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub tls: Option<String>,
    pub sender: Option<String>,
    pub recipients: Vec<String>,
    pub username_env: Option<String>,
    pub password_env: Option<String>,
    pub allow_private: bool,
}

impl DestinationConfig {
    pub fn webhook(id: &str, kind: &str, endpoint_env: &str) -> Self {
        Self { id: id.into(), kind: kind.into(), endpoint_env: Some(endpoint_env.into()), ..Self::default() }
    }

    /// Every environment variable name this destination reads.
    pub fn env_names(&self) -> Vec<&str> {
        [&self.endpoint_env, &self.token_env, &self.username_env, &self.password_env]
            .into_iter()
            .filter_map(|name| name.as_deref())
            .collect()
    }

    pub fn to_value(&self) -> Value {
        json!({
            "id": self.id, "kind": self.kind, "endpoint_env": self.endpoint_env,
            "token_env": self.token_env, "host": self.host, "port": self.port, "tls": self.tls,
            "sender": self.sender, "recipients": self.recipients, "username_env": self.username_env,
            "password_env": self.password_env, "allow_private": self.allow_private,
        })
    }

    pub fn from_value(value: &Value) -> Option<Self> {
        let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
        Some(Self {
            id: text("id")?,
            kind: text("kind")?,
            endpoint_env: text("endpoint_env"),
            token_env: text("token_env"),
            host: text("host"),
            port: value.get("port").and_then(Value::as_u64).and_then(|p| u16::try_from(p).ok()),
            tls: text("tls"),
            sender: text("sender"),
            recipients: value
                .get("recipients")
                .and_then(Value::as_array)
                .map(|items| items.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default(),
            username_env: text("username_env"),
            password_env: text("password_env"),
            allow_private: value.get("allow_private").and_then(Value::as_bool).unwrap_or(false),
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TempoConfig {
    pub enabled: bool,
    pub url: String,
    pub home: String,
    pub model: String,
    pub timeout_seconds: f64,
}

impl Default for TempoConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            url: "http://127.0.0.1:8765".into(),
            home: String::new(),
            model: "tempo/auto".into(),
            timeout_seconds: 120.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct MetricsConfig {
    pub enabled: bool,
    pub port: u16,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self { enabled: false, port: crate::constants::DEFAULT_METRICS_PORT }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AppConfig {
    pub watch: WatchConfig,
    pub tools: ToolsConfig,
    pub network: NetworkConfig,
    pub streams: Vec<StreamConfig>,
    pub retention_days: i64,
    pub destinations: Vec<DestinationConfig>,
    pub tempo: TempoConfig,
    pub metrics: MetricsConfig,
    /// The absolute TOML path when loaded from a file.
    pub config_path: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Interval {
    pub start_s: f64,
    pub end_s: f64,
    pub open_at_sample_end: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CoverageEvidence {
    pub covered_s: f64,
    pub decoded_amount_s: f64,
    pub frame_count: u64,
    pub sample_count: u64,
    pub sample_rate_hz: Option<i64>,
    pub first_s: Option<f64>,
    pub last_s: Option<f64>,
    pub largest_gap_s: f64,
    pub complete: bool,
    pub window_offset_s: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DimensionEvidence {
    pub status: String,
    pub reason_codes: Vec<String>,
    pub intervals: Vec<Interval>,
    pub sampled_s: f64,
    pub decoded_s: Option<f64>,
    pub coverage: Option<CoverageEvidence>,
}

impl DimensionEvidence {
    pub fn new(status: &str, reasons: &[&str]) -> Self {
        Self {
            status: status.into(),
            reason_codes: reasons.iter().map(|r| r.to_string()).collect(),
            intervals: Vec::new(),
            sampled_s: 0.0,
            decoded_s: None,
            coverage: None,
        }
    }

    /// Evidence with sampled and decoded seconds, as test fixtures use.
    pub fn decoded(status: &str, reasons: &[&str], sampled_s: f64, decoded_s: f64) -> Self {
        Self { sampled_s, decoded_s: Some(decoded_s), ..Self::new(status, reasons) }
    }

    pub fn with_intervals(mut self, intervals: Vec<Interval>) -> Self {
        self.intervals = intervals;
        self
    }

    pub fn has_reason(&self, reason: &str) -> bool {
        self.reason_codes.iter().any(|r| r == reason)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Observation {
    pub stream_id: String,
    pub observed_at: String,
    pub status: String,
    pub reason_codes: Vec<String>,
    pub audio: DimensionEvidence,
    pub video: DimensionEvidence,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MediaInput {
    pub kind: String,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedMedia {
    pub inputs: Vec<MediaInput>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HttpResult {
    pub status: Option<u16>,
    pub final_url: String,
    pub content_type: String,
    pub body: Vec<u8>,
    pub truncated: bool,
    pub error_code: Option<String>,
}

impl HttpResult {
    pub fn new(status: Option<u16>, final_url: &str, content_type: &str, body: &[u8]) -> Self {
        Self {
            status,
            final_url: final_url.into(),
            content_type: content_type.into(),
            body: body.to_vec(),
            truncated: false,
            error_code: None,
        }
    }

    pub fn failed(final_url: &str, code: &str) -> Self {
        Self { error_code: Some(code.into()), ..Self::new(None, final_url, "", b"") }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ThumbnailBaseline {
    pub digest: String,
    pub observed_monotonic: f64,
}

const DIMENSION_STATUSES: &[&str] = &["HEALTHY", "UNHEALTHY", "UNKNOWN", "DEGRADED", "NOT_APPLICABLE"];
const OBSERVATION_STATUSES: &[&str] = &["HEALTHY", "UNHEALTHY", "UNKNOWN", "DEGRADED", "OFF_AIR"];

/// Clamp to 0..86400 seconds and round to milliseconds; `None` for non-finite input.
pub fn bounded_number(value: f64) -> Option<f64> {
    if !value.is_finite() {
        return None;
    }
    Some(pyjson::round3(value.clamp(0.0, 86_400.0)))
}

fn bounded_opt(value: Option<f64>) -> Value {
    value.and_then(bounded_number).map(float).unwrap_or(Value::Null)
}

pub fn bounded_coverage(value: Option<&CoverageEvidence>) -> Value {
    let Some(value) = value else {
        return Value::Null;
    };
    json!({
        "covered_s": float(bounded_number(value.covered_s).unwrap_or(0.0)),
        "decoded_amount_s": float(bounded_number(value.decoded_amount_s).unwrap_or(0.0)),
        "frame_count": value.frame_count.min(100_000),
        "sample_count": value.sample_count.min(100_000_000_000),
        "sample_rate_hz": value.sample_rate_hz,
        "first_s": bounded_opt(value.first_s),
        "last_s": bounded_opt(value.last_s),
        "largest_gap_s": float(bounded_number(value.largest_gap_s).unwrap_or(0.0)),
        "complete": value.complete,
        "window_offset_s": float(bounded_number(value.window_offset_s).unwrap_or(0.0)),
    })
}

pub fn is_reason_code(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 64
        && text.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

/// Persisted evidence for one dimension: bounded, reason codes only, no free text.
pub fn bounded_dimension_evidence(value: &DimensionEvidence) -> Value {
    let status = if DIMENSION_STATUSES.contains(&value.status.as_str()) { value.status.as_str() } else { "UNKNOWN" };
    let reasons: Vec<&str> =
        value.reason_codes.iter().take(16).map(String::as_str).filter(|r| is_reason_code(r)).collect();
    let mut intervals = Vec::new();
    for item in value.intervals.iter().take(32) {
        let (Some(start), Some(end)) = (bounded_number(item.start_s), bounded_number(item.end_s)) else {
            continue;
        };
        if end < start {
            continue;
        }
        intervals.push(json!({"start_s": float(start), "end_s": float(end),
                              "open_at_sample_end": item.open_at_sample_end}));
    }
    json!({
        "status": status,
        "reason_codes": reasons,
        "intervals": intervals,
        "sampled_s": float(bounded_number(value.sampled_s).unwrap_or(0.0)),
        "decoded_s": bounded_opt(value.decoded_s),
        "coverage": bounded_coverage(value.coverage.as_ref()),
    })
}

/// The evidence of one observation that matters to one incident condition.
pub fn transition_sample(condition: &str, observation: &Observation) -> Value {
    let mut dimensions = Map::new();
    if condition.starts_with("audio_") {
        dimensions.insert("audio".into(), bounded_dimension_evidence(&observation.audio));
    } else if condition.starts_with("video_") {
        dimensions.insert("video".into(), bounded_dimension_evidence(&observation.video));
    } else {
        dimensions.insert("audio".into(), bounded_dimension_evidence(&observation.audio));
        dimensions.insert("video".into(), bounded_dimension_evidence(&observation.video));
    }
    let observed_at: String = observation.observed_at.chars().take(64).collect();
    let status = if OBSERVATION_STATUSES.contains(&observation.status.as_str()) {
        observation.status.as_str()
    } else {
        "UNKNOWN"
    };
    json!({"observed_at": observed_at, "status": status, "dimensions": dimensions})
}

pub fn transition_evidence(condition: &str, observation: &Observation, threshold: Value, samples: &[Value]) -> Value {
    let start = samples.len().saturating_sub(100);
    json!({
        "affected_dimensions": transition_sample(condition, observation)["dimensions"].clone(),
        "threshold": threshold,
        "samples": samples[start..].to_vec(),
    })
}

fn interval_json(items: &[Interval]) -> Value {
    Value::Array(
        items
            .iter()
            .map(|i| json!({"start_s": float(i.start_s), "end_s": float(i.end_s), "open_at_sample_end": i.open_at_sample_end}))
            .collect(),
    )
}

/// The `observations[]` element of `check --json` and the stored `latest` snapshot.
pub fn observation_json(item: &Observation) -> Value {
    let dimension = |value: &DimensionEvidence| {
        json!({
            "status": value.status,
            "reason_codes": value.reason_codes,
            "intervals": interval_json(&value.intervals),
            "sampled_s": float(value.sampled_s),
            "decoded_s": value.decoded_s.map(float).unwrap_or(Value::Null),
            "coverage": bounded_coverage(value.coverage.as_ref()),
        })
    };
    json!({
        "stream_id": item.stream_id,
        "observed_at": item.observed_at,
        "status": item.status,
        "reason_codes": item.reason_codes,
        "audio": dimension(&item.audio),
        "video": dimension(&item.video),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn example_stream() -> StreamConfig {
        StreamConfig {
            id: "sample-channel".into(),
            label: "Sample channel".into(),
            url: "https://example.invalid/live/sample".into(),
            source: "direct".into(),
            expected_live: true,
            expect_audio: true,
            expect_video: true,
            detect_freeze: true,
            freeze_noise: 0.001,
            freeze_window_s: 8.0,
            silence_db: -50.0,
            silence_window_s: 8.0,
            unhealthy_after_s: 180.0,
            min_bad_samples: 3,
            recovery_samples: 2,
            max_gap_s: 180.0,
            unknown_notice_after_s: 600.0,
            offline_http_statuses: vec![410],
            thumbnail_enabled: false,
            thumbnail_url: String::new(),
            thumbnail_stale_after_s: 1800.0,
            allow_private: false,
        }
    }

    #[test]
    fn fingerprint_matches_the_python_release() {
        let stream = example_stream();
        assert_eq!(
            pyjson::dumps(&stream.as_value()),
            r#"{"allow_private": false, "detect_freeze": true, "expect_audio": true, "expect_video": true, "expected_live": true, "freeze_noise": 0.001, "freeze_window_s": 8.0, "id": "sample-channel", "label": "Sample channel", "max_gap_s": 180.0, "min_bad_samples": 3, "offline_http_statuses": [410], "recovery_samples": 2, "silence_db": -50.0, "silence_window_s": 8.0, "source": "direct", "thumbnail_enabled": false, "thumbnail_stale_after_s": 1800.0, "thumbnail_url": "", "unhealthy_after_s": 180.0, "unknown_notice_after_s": 600.0, "url": "https://example.invalid/live/sample"}"#
        );
        assert_eq!(stream.fingerprint(), "8736ffe87d638ea8c3cd51b34b20313b83b69bf2b2783f056ece83fbecfbfd4e");
        let variant = StreamConfig {
            label: "D\u{ed}a \"q\" \\ \u{1F600}".into(),
            freeze_noise: 1e-06,
            silence_db: -50.5,
            offline_http_statuses: vec![404, 410],
            unhealthy_after_s: 1e16,
            ..stream
        };
        assert_eq!(variant.fingerprint(), "db8ee8cba593aafdbd735a69640a54ed09c657d7d247e5d1348184a3f6efbee0");
    }
}
