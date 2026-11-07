//! Strict TOML parsing and static input validation; no network or writes.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use toml::{Table, Value};

use crate::constants::MAX_STREAMS;
use crate::model::*;
use crate::netpolicy::{safe_hostport, static_url};

type Result<T> = std::result::Result<T, ConfigError>;

fn err<T>(message: impl Into<String>) -> Result<T> {
    Err(ConfigError::new(message))
}

fn finite(value: &Value, name: &str, low: f64, high: f64) -> Result<f64> {
    let number = match value {
        Value::Integer(i) => *i as f64,
        Value::Float(f) => *f,
        _ => return err(format!("{name} must be numeric")),
    };
    if !number.is_finite() || number < low || number > high {
        return err(format!("{name} is outside its permitted range"));
    }
    Ok(number)
}

fn integer(value: &Value, name: &str, low: i64, high: i64) -> Result<i64> {
    match value {
        Value::Integer(i) if (low..=high).contains(i) => Ok(*i),
        _ => err(format!("{name} must be an integer in range")),
    }
}

fn boolean(value: &Value, name: &str) -> Result<bool> {
    value.as_bool().ok_or_else(|| ConfigError::new(format!("{name} must be a boolean")))
}

fn table<'a>(value: &'a Value, name: &str, expected: &[&str]) -> Result<&'a Table> {
    let Value::Table(table) = value else {
        return err(format!("{name} must be a TOML table"));
    };
    let expected: BTreeSet<&str> = expected.iter().copied().collect();
    let present: BTreeSet<&str> = table.keys().map(String::as_str).collect();
    if present != expected {
        let missing: Vec<&str> = expected.difference(&present).copied().collect();
        let unknown: Vec<&str> = present.difference(&expected).copied().collect();
        let mut parts = Vec::new();
        if !missing.is_empty() {
            parts.push(format!("missing {}", missing.join(", ")));
        }
        if !unknown.is_empty() {
            parts.push(format!("unknown {}", unknown.join(", ")));
        }
        return err(format!("{name}: {}", parts.join("; ")));
    }
    Ok(table)
}

/// A table whose keys must come from `allowed` and include `required`.
fn optional_table<'a>(value: &'a Value, name: &str, required: &[&str], allowed: &[&str]) -> Result<&'a Table> {
    let Value::Table(table) = value else {
        return err(format!("{name} must be a TOML table"));
    };
    let unknown: Vec<&str> = table.keys().map(String::as_str).filter(|k| !allowed.contains(k)).collect();
    let missing: Vec<&str> = required.iter().copied().filter(|k| !table.contains_key(*k)).collect();
    if !unknown.is_empty() || !missing.is_empty() {
        let mut parts = Vec::new();
        if !missing.is_empty() {
            parts.push(format!("missing {}", missing.join(", ")));
        }
        if !unknown.is_empty() {
            parts.push(format!("unknown {}", unknown.join(", ")));
        }
        return err(format!("{name}: {}", parts.join("; ")));
    }
    Ok(table)
}

fn string(value: &Value, name: &str, max_length: usize) -> Result<String> {
    match value {
        Value::String(text)
            if !text.trim().is_empty() && text.chars().count() <= max_length && !text.contains('\0') =>
        {
            Ok(text.clone())
        }
        _ => err(format!("{name} must be a non-empty bounded string")),
    }
}

fn has_label_control(text: &str) -> bool {
    text.chars().any(|c| (c as u32) < 0x20 || c as u32 == 0x7f)
}

fn is_slug(text: &str) -> bool {
    let bytes = text.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes.iter().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_' || *b == b'-')
}

pub fn env_name(value: &Value, name: &str) -> Result<String> {
    let result = string(value, name, 64)?;
    let bytes = result.as_bytes();
    let ok = (bytes[0].is_ascii_uppercase() || bytes[0] == b'_')
        && bytes.iter().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || *b == b'_');
    if !ok {
        return err(format!("{name} must name an uppercase environment variable"));
    }
    Ok(result)
}

/// A single bare address; no display names, lists, comments or header controls.
pub fn email_address(value: &str, name: &str) -> Result<String> {
    if value.trim().is_empty() || value.chars().count() > 254 || value.contains('\0') {
        return err(format!("{name} must be a non-empty bounded string"));
    }
    if has_label_control(value) {
        return err(format!("{name} contains a header control character"));
    }
    let invalid = || ConfigError::new(format!("{name} must be a single email address"));
    if value.chars().any(|c| c.is_whitespace() || "<>\"(),:;[]\\".contains(c)) {
        return Err(invalid());
    }
    let (local, domain) = value.split_once('@').ok_or_else(invalid)?;
    if local.is_empty() || domain.contains('@') {
        return Err(invalid());
    }
    let inner_dot = domain.char_indices().any(|(i, c)| c == '.' && i > 0 && i + 1 < domain.len());
    if !inner_dot {
        return Err(invalid());
    }
    Ok(value.to_string())
}

fn email_value(value: &Value, name: &str) -> Result<String> {
    match value {
        Value::String(text) => email_address(text, name),
        _ => err(format!("{name} must be a non-empty bounded string")),
    }
}

fn parse_watch(value: &Value) -> Result<WatchConfig> {
    let w = table(
        value,
        "[watch]",
        &[
            "interval_s",
            "sample_s",
            "resolver_timeout_s",
            "capture_timeout_s",
            "analysis_timeout_s",
            "max_sample_bytes",
            "max_output_bytes",
            "cycle_budget_s",
            "state_path",
        ],
    )?;
    let watch = WatchConfig {
        interval_s: finite(&w["interval_s"], "watch.interval_s", 1.0, 86_400.0)?,
        sample_s: finite(&w["sample_s"], "watch.sample_s", 1.0, 300.0)?,
        resolver_timeout_s: finite(&w["resolver_timeout_s"], "watch.resolver_timeout_s", 0.1, 120.0)?,
        capture_timeout_s: finite(&w["capture_timeout_s"], "watch.capture_timeout_s", 0.1, 120.0)?,
        analysis_timeout_s: finite(&w["analysis_timeout_s"], "watch.analysis_timeout_s", 0.1, 120.0)?,
        max_sample_bytes: integer(&w["max_sample_bytes"], "watch.max_sample_bytes", 1024, 25_165_824)? as u64,
        max_output_bytes: integer(&w["max_output_bytes"], "watch.max_output_bytes", 1024, 1_048_576)? as usize,
        cycle_budget_s: finite(&w["cycle_budget_s"], "watch.cycle_budget_s", 1.0, 600.0)?,
        state_path: string(&w["state_path"], "watch.state_path", 512)?,
    };
    if watch.cycle_budget_s < watch.resolver_timeout_s.max(watch.capture_timeout_s).max(watch.analysis_timeout_s) {
        return err("watch.cycle_budget_s must cover at least one child deadline");
    }
    Ok(watch)
}

const STREAM_FIELDS: &[&str] = &[
    "id",
    "label",
    "url",
    "source",
    "expected_live",
    "expect_audio",
    "expect_video",
    "detect_freeze",
    "freeze_noise",
    "freeze_window_s",
    "silence_db",
    "silence_window_s",
    "unhealthy_after_s",
    "min_bad_samples",
    "recovery_samples",
    "max_gap_s",
    "unknown_notice_after_s",
    "offline_http_statuses",
    "thumbnail_enabled",
    "thumbnail_url",
    "thumbnail_stale_after_s",
    "allow_private",
];

/// Field order of the starter configuration's stream table (used when the GUI appends one).
pub const STREAM_FIELD_ORDER: &[&str] = STREAM_FIELDS;

fn parse_stream(raw: &Value, index: usize, watch: &WatchConfig, seen: &mut BTreeSet<String>) -> Result<StreamConfig> {
    let item = table(raw, &format!("streams[{index}]"), STREAM_FIELDS)?;
    let sid = string(&item["id"], "stream.id", 64)?;
    if !is_slug(&sid) || seen.contains(&sid) {
        return err("stream ids must be safe, unique slugs");
    }
    seen.insert(sid.clone());
    let label = string(&item["label"], "stream.label", 80)?;
    if has_label_control(&label) {
        return err("stream.label contains control characters");
    }
    let source = string(&item["source"], "stream.source", 16)?.to_lowercase();
    if source != "youtube" && source != "direct" {
        return err("stream.source must be youtube or direct");
    }
    let url = string(&item["url"], "stream.url", 4096)?;
    static_url(&url, &source, false)?;
    let expected_live = boolean(&item["expected_live"], "stream.expected_live")?;
    let expect_audio = boolean(&item["expect_audio"], "stream.expect_audio")?;
    let expect_video = boolean(&item["expect_video"], "stream.expect_video")?;
    let detect_freeze = boolean(&item["detect_freeze"], "stream.detect_freeze")?;
    if !(expect_audio || expect_video) || (detect_freeze && !expect_video) {
        return err("each stream needs an expected media dimension");
    }
    let freeze_noise = finite(&item["freeze_noise"], "stream.freeze_noise", 0.000001, 1.0)?;
    let freeze_window_s = finite(&item["freeze_window_s"], "stream.freeze_window_s", 0.1, watch.sample_s)?;
    let silence_db = finite(&item["silence_db"], "stream.silence_db", -120.0, 0.0)?;
    let silence_window_s = finite(&item["silence_window_s"], "stream.silence_window_s", 0.1, watch.sample_s)?;
    let Value::Array(offline_raw) = &item["offline_http_statuses"] else {
        return err("stream.offline_http_statuses must be an array");
    };
    let mut offline = Vec::new();
    for value in offline_raw {
        let status = integer(value, "stream.offline_http_statuses", 400, 599)?;
        if offline.contains(&status) {
            return err("stream.offline_http_statuses contains duplicates");
        }
        offline.push(status);
    }
    let thumbnail_enabled = boolean(&item["thumbnail_enabled"], "stream.thumbnail_enabled")?;
    let Value::String(thumbnail_url) = &item["thumbnail_url"] else {
        return err("stream.thumbnail_url must be a string");
    };
    if thumbnail_enabled {
        if !expect_video {
            return err("thumbnail fallback requires a direct or YouTube video source");
        }
        if !thumbnail_url.is_empty() {
            static_url(&string(&item["thumbnail_url"], "stream.thumbnail_url", 4096)?, "direct", true)?;
        } else if source == "direct" {
            return err("direct thumbnail fallback requires thumbnail_url");
        }
    } else if !thumbnail_url.is_empty() {
        static_url(&string(&item["thumbnail_url"], "stream.thumbnail_url", 4096)?, "direct", true)?;
    }
    Ok(StreamConfig {
        id: sid,
        label,
        url,
        source,
        expected_live,
        expect_audio,
        expect_video,
        detect_freeze,
        freeze_noise,
        freeze_window_s,
        silence_db,
        silence_window_s,
        unhealthy_after_s: finite(&item["unhealthy_after_s"], "stream.unhealthy_after_s", 0.0, 86_400.0)?,
        min_bad_samples: integer(&item["min_bad_samples"], "stream.min_bad_samples", 1, 100)?,
        recovery_samples: integer(&item["recovery_samples"], "stream.recovery_samples", 1, 100)?,
        max_gap_s: finite(&item["max_gap_s"], "stream.max_gap_s", 1.0, 86_400.0)?,
        unknown_notice_after_s: finite(
            &item["unknown_notice_after_s"],
            "stream.unknown_notice_after_s",
            0.0,
            604_800.0,
        )?,
        offline_http_statuses: offline,
        thumbnail_enabled,
        thumbnail_url: thumbnail_url.clone(),
        thumbnail_stale_after_s: finite(
            &item["thumbnail_stale_after_s"],
            "stream.thumbnail_stale_after_s",
            60.0,
            86_400.0,
        )?,
        allow_private: boolean(&item["allow_private"], "stream.allow_private")?,
    })
}

fn optional_flag(item: &Table, key: &str, name: &str) -> Result<bool> {
    match item.get(key) {
        Some(value) => boolean(value, name),
        None => Ok(false),
    }
}

fn parse_destinations(raw: Option<&Value>) -> Result<Vec<DestinationConfig>> {
    let empty = Vec::new();
    let items = match raw {
        None => &empty,
        Some(Value::Array(items)) if items.len() <= 8 => items,
        Some(_) => return err("destinations must be an array with at most 8 entries"),
    };
    let mut result: Vec<DestinationConfig> = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let Value::Table(item) = item else {
            return err(format!("destinations[{index}] must be a table"));
        };
        let kind =
            string(item.get("type").unwrap_or(&Value::Boolean(false)), &format!("destinations[{index}].type"), 16)?;
        let keys: BTreeSet<&str> = item.keys().map(String::as_str).collect();
        let check_keys = |required: &[&str], optional: &[&str]| -> Result<()> {
            let bad = keys.iter().any(|k| !required.contains(k) && !optional.contains(k))
                || required.iter().any(|k| !keys.contains(k));
            if bad { err(format!("destinations[{index}] has invalid keys")) } else { Ok(()) }
        };
        let mut destination = match kind.as_str() {
            "ntfy" | "discord" | "webhook" => {
                let optional: &[&str] =
                    if kind == "discord" { &["allow_private"] } else { &["token_env", "allow_private"] };
                check_keys(&["id", "type", "endpoint_env"], optional)?;
                DestinationConfig {
                    kind: kind.clone(),
                    endpoint_env: Some(env_name(&item["endpoint_env"], "destination.endpoint_env")?),
                    token_env: item.get("token_env").map(|v| env_name(v, "destination.token_env")).transpose()?,
                    allow_private: optional_flag(item, "allow_private", "destination.allow_private")?,
                    ..DestinationConfig::default()
                }
            }
            "smtp" => {
                check_keys(
                    &["id", "type", "host", "port", "tls", "sender", "recipients"],
                    &["username_env", "password_env", "allow_private"],
                )?;
                if item.contains_key("username_env") != item.contains_key("password_env") {
                    return err("SMTP username_env and password_env must be supplied together");
                }
                let host = string(&item["host"], "destination.host", 253)?;
                if crate::netpolicy::parse_ip(&host).is_none() {
                    let bytes = host.as_bytes();
                    let ok = bytes[0].is_ascii_alphanumeric()
                        && bytes[bytes.len() - 1].is_ascii_alphanumeric()
                        && bytes.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'.' || *b == b'-');
                    if !ok {
                        return err("destination.host is malformed");
                    }
                }
                let tls = string(&item["tls"], "destination.tls", 16)?.to_lowercase();
                if tls != "ssl" && tls != "starttls" {
                    return err("SMTP requires tls='ssl' or tls='starttls'");
                }
                let recipients = match &item["recipients"] {
                    Value::Array(values) if (1..=20).contains(&values.len()) => values,
                    _ => return err("destination.recipients must contain 1 to 20 addresses"),
                };
                DestinationConfig {
                    kind: kind.clone(),
                    host: Some(host),
                    port: Some(integer(&item["port"], "destination.port", 1, 65_535)? as u16),
                    tls: Some(tls),
                    sender: Some(email_value(&item["sender"], "destination.sender")?),
                    recipients: recipients
                        .iter()
                        .map(|v| email_value(v, "destination.recipient"))
                        .collect::<Result<Vec<_>>>()?,
                    username_env: item
                        .get("username_env")
                        .map(|v| env_name(v, "destination.username_env"))
                        .transpose()?,
                    password_env: item
                        .get("password_env")
                        .map(|v| env_name(v, "destination.password_env"))
                        .transpose()?,
                    allow_private: optional_flag(item, "allow_private", "destination.allow_private")?,
                    ..DestinationConfig::default()
                }
            }
            _ => return err(format!("destinations[{index}].type is unsupported")),
        };
        let id = string(item.get("id").unwrap_or(&Value::Boolean(false)), "destination.id", 64)?;
        if !is_slug(&id) || result.iter().any(|d| d.id == id) {
            return err("destination ids must be safe, unique slugs");
        }
        destination.id = id;
        result.push(destination);
    }
    Ok(result)
}

/// `[tempo]`: http on a loopback host only, a `tempo/<route>` model, 1..600 s.
pub fn validate_tempo_url(url: &str) -> Result<()> {
    let bad = || ConfigError::new("tempo.url must be http://127.0.0.1, http://[::1] or http://localhost with a port");
    let parsed = crate::url::urlsplit(url).map_err(|_| bad())?;
    if parsed.scheme != "http" || parsed.has_userinfo() || !parsed.query.is_empty() || !parsed.fragment.is_empty() {
        return Err(bad());
    }
    if !(parsed.path.is_empty() || parsed.path == "/") || url.contains('#') {
        return Err(bad());
    }
    let host = parsed.hostname().ok_or_else(bad)?;
    if !matches!(host.as_str(), "127.0.0.1" | "::1" | "localhost") {
        return Err(bad());
    }
    match parsed.port() {
        Ok(Some(port)) if (1..=65_535).contains(&port) => Ok(()),
        Ok(None) => Ok(()),
        _ => Err(bad()),
    }
}

pub fn valid_tempo_model(model: &str) -> bool {
    let Some(route) = model.strip_prefix("tempo/") else {
        return false;
    };
    let bytes = route.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 40
        && bytes[0].is_ascii_lowercase()
        && bytes.iter().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

fn parse_tempo(raw: Option<&Value>) -> Result<TempoConfig> {
    let mut tempo = TempoConfig::default();
    let Some(raw) = raw else {
        return Ok(tempo);
    };
    let item = optional_table(raw, "[tempo]", &["enabled"], &["enabled", "url", "home", "model", "timeout_seconds"])?;
    tempo.enabled = boolean(&item["enabled"], "tempo.enabled")?;
    if let Some(url) = item.get("url") {
        tempo.url = string(url, "tempo.url", 256)?;
    }
    validate_tempo_url(&tempo.url)?;
    if let Some(home) = item.get("home") {
        let Value::String(home) = home else {
            return err("tempo.home must be a string");
        };
        if home.chars().count() > 1024 || home.contains('\0') || has_label_control(home) {
            return err("tempo.home must be a bounded path string");
        }
        tempo.home = home.clone();
    }
    if let Some(model) = item.get("model") {
        tempo.model = string(model, "tempo.model", 64)?;
    }
    if !valid_tempo_model(&tempo.model) {
        return err("tempo.model must match tempo/<route>, for example tempo/auto");
    }
    if let Some(timeout) = item.get("timeout_seconds") {
        tempo.timeout_seconds = finite(timeout, "tempo.timeout_seconds", 1.0, 600.0)?;
    }
    Ok(tempo)
}

fn parse_metrics(raw: Option<&Value>) -> Result<MetricsConfig> {
    let mut metrics = MetricsConfig::default();
    let Some(raw) = raw else {
        return Ok(metrics);
    };
    let item = optional_table(raw, "[metrics]", &["enabled"], &["enabled", "port"])?;
    metrics.enabled = boolean(&item["enabled"], "metrics.enabled")?;
    if let Some(port) = item.get("port") {
        metrics.port = integer(port, "metrics.port", 1, 65_535)? as u16;
    }
    Ok(metrics)
}

/// Validate a parsed TOML document.
pub fn validate_config(data: &Table) -> Result<AppConfig> {
    let required = ["watch", "tools", "network", "streams"];
    let allowed = ["watch", "tools", "network", "streams", "storage", "destinations", "tempo", "metrics"];
    if required.iter().any(|k| !data.contains_key(*k)) || data.keys().any(|k| !allowed.contains(&k.as_str())) {
        return err("config has missing or unknown top-level keys");
    }
    let watch = parse_watch(&data["watch"])?;
    let tools_table = table(&data["tools"], "[tools]", &["yt_dlp", "ffmpeg", "ffprobe"])?;
    let mut tool_values = Vec::new();
    for key in ["yt_dlp", "ffmpeg", "ffprobe"] {
        let value = string(&tools_table[key], &format!("tools.{key}"), 1024)?;
        if value.chars().any(|c| (c as u32) < 32) {
            return err(format!("tools.{key} contains a control character"));
        }
        tool_values.push(value);
    }
    let tools =
        ToolsConfig { yt_dlp: tool_values[0].clone(), ffmpeg: tool_values[1].clone(), ffprobe: tool_values[2].clone() };
    let network_table =
        table(&data["network"], "[network]", &["private_allowlist", "max_redirects", "thumbnail_max_bytes"])?;
    let Value::Array(raw_allowlist) = &network_table["private_allowlist"] else {
        return err("network.private_allowlist must be an array");
    };
    let mut allowlist: Vec<String> = Vec::new();
    for item in raw_allowlist {
        let Value::String(text) = item else {
            return err("private_allowlist entries must be host:port strings");
        };
        allowlist.push(safe_hostport(text)?);
    }
    let unique: BTreeSet<&String> = allowlist.iter().collect();
    if unique.len() != allowlist.len() {
        return err("network.private_allowlist contains duplicates");
    }
    let network = NetworkConfig {
        private_allowlist: allowlist,
        max_redirects: integer(&network_table["max_redirects"], "network.max_redirects", 0, 3)? as u32,
        thumbnail_max_bytes: integer(
            &network_table["thumbnail_max_bytes"],
            "network.thumbnail_max_bytes",
            1024,
            2_097_152,
        )? as usize,
    };
    let raw_streams = match &data["streams"] {
        Value::Array(items) if (1..=MAX_STREAMS).contains(&items.len()) => items,
        _ => return err(format!("streams must contain 1 to {MAX_STREAMS} entries")),
    };
    let mut seen = BTreeSet::new();
    let mut streams = Vec::new();
    for (index, raw) in raw_streams.iter().enumerate() {
        streams.push(parse_stream(raw, index, &watch, &mut seen)?);
    }
    let retention_days = match data.get("storage") {
        None => 90,
        Some(storage) => {
            let storage = table(storage, "[storage]", &["retention_days"])?;
            integer(&storage["retention_days"], "storage.retention_days", 1, 3650)?
        }
    };
    let destinations = parse_destinations(data.get("destinations"))?;
    let tempo = parse_tempo(data.get("tempo"))?;
    let metrics = parse_metrics(data.get("metrics"))?;
    Ok(AppConfig { watch, tools, network, streams, retention_days, destinations, tempo, metrics, config_path: None })
}

/// Parse TOML text (no path resolution).
pub fn parse_config_text(text: &str) -> Result<AppConfig> {
    let data: Table = text.parse().map_err(|_| ConfigError::new("cannot read config (TOMLDecodeError)"))?;
    validate_config(&data)
}

/// `~` expansion like Python's `Path.expanduser`.
pub fn expand_user(path: &str) -> PathBuf {
    let home = || std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    if path == "~" {
        if let Some(home) = home() {
            return PathBuf::from(home);
        }
    } else if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        if let Some(home) = home() {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

/// `parent / relative` the way pathlib joins: `.` components are dropped.
pub fn join_clean(parent: &Path, relative: &Path) -> PathBuf {
    let mut joined = parent.to_path_buf();
    for component in relative.components() {
        if component != std::path::Component::CurDir {
            joined.push(component.as_os_str());
        }
    }
    joined
}

pub fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

fn io_error_name(error: &std::io::Error) -> &'static str {
    use std::io::ErrorKind::*;
    match error.kind() {
        NotFound => "FileNotFoundError",
        PermissionDenied => "PermissionError",
        IsADirectory => "IsADirectoryError",
        InvalidData => "UnicodeDecodeError",
        _ => "OSError",
    }
}

/// Read and validate a TOML file; state and explicit tool paths resolve beside it.
pub fn load_config(path: impl AsRef<Path>) -> Result<AppConfig> {
    let config_path = absolute(&expand_user(&path.as_ref().to_string_lossy()));
    let bytes = std::fs::read(&config_path)
        .map_err(|e| ConfigError::new(format!("cannot read config ({})", io_error_name(&e))))?;
    let text = String::from_utf8(bytes).map_err(|_| ConfigError::new("cannot read config (UnicodeDecodeError)"))?;
    let mut config = parse_config_text(&text)?;
    let parent = config_path.parent().map(Path::to_path_buf).unwrap_or_default();
    let state = expand_user(&config.watch.state_path);
    let state = if state.is_absolute() { state } else { join_clean(&parent, &state) };
    config.watch.state_path = state.to_string_lossy().into_owned();
    for tool in [&mut config.tools.ffmpeg, &mut config.tools.ffprobe, &mut config.tools.yt_dlp] {
        if tool.contains('/') || tool.contains('\\') {
            let candidate = expand_user(tool);
            let resolved = if candidate.is_absolute() { candidate } else { join_clean(&parent, &candidate) };
            *tool = resolved.to_string_lossy().into_owned();
        }
    }
    config.config_path = Some(config_path);
    Ok(config)
}

/// The starter configuration bundled into the binary (`init`, GUI first run, tests).
pub const EXAMPLE_TOML: &str = include_str!("../dead-air.example.toml");

#[cfg(test)]
mod tests {
    use super::*;

    fn example() -> Table {
        EXAMPLE_TOML.parse().unwrap()
    }

    #[test]
    fn example_config_is_strict_and_valid() {
        let config = validate_config(&example()).unwrap();
        assert_eq!(config.watch.sample_s, 12.0);
        assert_eq!(config.streams[0].source, "direct");
        assert!(config.network.private_allowlist.is_empty());
        assert!(!config.tempo.enabled);
        assert!(!config.metrics.enabled);
        assert_eq!(config.retention_days, 90);
    }

    #[test]
    fn unknown_and_missing_keys_are_rejected_with_python_messages() {
        let mut data = example();
        data["watch"].as_table_mut().unwrap().insert("extra".into(), Value::Boolean(true));
        assert_eq!(validate_config(&data).unwrap_err().0, "[watch]: unknown extra");
        let mut data = example();
        data["watch"].as_table_mut().unwrap().remove("sample_s");
        assert_eq!(validate_config(&data).unwrap_err().0, "[watch]: missing sample_s");
        let mut data = example();
        data.insert("bogus".into(), Value::Integer(1));
        assert_eq!(validate_config(&data).unwrap_err().0, "config has missing or unknown top-level keys");
        assert!(parse_config_text("[watch]\n").unwrap_err().0.contains("missing"));
    }

    #[test]
    fn bad_youtube_host_and_type_errors_are_config_errors() {
        let mut data = example();
        let stream = data["streams"].as_array_mut().unwrap()[0].as_table_mut().unwrap();
        stream.insert("source".into(), Value::String("youtube".into()));
        stream.insert("url".into(), Value::String("https://example.invalid/watch?v=demo".into()));
        assert!(validate_config(&data).is_err());
        let mut data = example();
        data["watch"].as_table_mut().unwrap().insert("interval_s".into(), Value::Boolean(true));
        assert_eq!(validate_config(&data).unwrap_err().0, "watch.interval_s must be numeric");
        let mut data = example();
        data["watch"].as_table_mut().unwrap().insert("interval_s".into(), Value::Float(f64::INFINITY));
        assert_eq!(validate_config(&data).unwrap_err().0, "watch.interval_s is outside its permitted range");
        let mut data = example();
        data["streams"].as_array_mut().unwrap()[0]
            .as_table_mut()
            .unwrap()
            .insert("min_bad_samples".into(), Value::Float(3.0));
        assert!(validate_config(&data).unwrap_err().0.contains("integer"));
    }

    #[test]
    fn youtube_thumbnail_fallback_can_derive_a_keyless_thumbnail() {
        let mut data = example();
        let stream = data["streams"].as_array_mut().unwrap()[0].as_table_mut().unwrap();
        stream.insert("source".into(), Value::String("youtube".into()));
        stream.insert("url".into(), Value::String("https://youtu.be/AbCdEfGhijk".into()));
        stream.insert("thumbnail_enabled".into(), Value::Boolean(true));
        assert!(validate_config(&data).unwrap().streams[0].thumbnail_enabled);
    }

    fn with_destinations(destinations: &str) -> std::result::Result<AppConfig, ConfigError> {
        parse_config_text(&format!("{EXAMPLE_TOML}\n{destinations}"))
    }

    #[test]
    fn destinations_use_named_env_and_reject_partial_smtp_auth() {
        let ok = with_destinations(
            "[[destinations]]\nid = \"ops\"\ntype = \"discord\"\nendpoint_env = \"DISCORD_WEBHOOK_URL\"\n",
        );
        assert_eq!(ok.unwrap().destinations[0].id, "ops");
        assert!(
            with_destinations("[[destinations]]\nid = \"ops\"\ntype = \"discord\"\nendpoint_env = \"url;unsafe\"\n")
                .is_err()
        );
        let hook = "[[destinations]]\nid = \"local-hook\"\ntype = \"webhook\"\nendpoint_env = \"LOCAL_HOOK\"\n";
        assert!(!with_destinations(hook).unwrap().destinations[0].allow_private);
        assert!(with_destinations(&format!("{hook}allow_private = true\n")).unwrap().destinations[0].allow_private);
        assert!(with_destinations(&format!("{hook}allow_private = \"yes\"\n")).is_err());
        let smtp = "[[destinations]]\nid = \"mail\"\ntype = \"smtp\"\nhost = \"mail.example.invalid\"\nport = 587\ntls = \"starttls\"\nsender = \"dead-air@example.invalid\"\nrecipients = [\"ops@example.invalid\"]\n";
        assert!(with_destinations(smtp).is_ok());
        assert!(with_destinations(&format!("{smtp}username_env = \"SMTP_USER\"\n")).is_err());
        assert!(with_destinations(&smtp.replace("starttls", "none")).is_err());
        for kind in ["[]", "{}", "5"] {
            assert!(with_destinations(&format!("[[destinations]]\nid = \"bad\"\ntype = {kind}\n")).is_err());
        }
    }

    #[test]
    fn email_header_crlf_is_rejected() {
        assert!(email_address("ops@example.invalid\r\nBcc: attacker@example.invalid", "sender").is_err());
        assert!(email_address("Name <ops@example.invalid>", "sender").is_err());
        assert!(email_address("a@b.c, d@e.f", "sender").is_err());
        assert!(email_address("ops@localhost", "sender").is_err());
        assert!(email_address("ops@example.invalid", "sender").is_ok());
    }

    #[test]
    fn tempo_and_metrics_tables_are_optional_and_strict() {
        let tempo = parse_config_text(&format!("{EXAMPLE_TOML}\n[tempo]\nenabled = true\n")).unwrap().tempo;
        assert!(tempo.enabled);
        assert_eq!(tempo.url, "http://127.0.0.1:8765");
        assert_eq!(tempo.model, "tempo/auto");
        for bad in [
            "url = \"https://127.0.0.1:8765\"",
            "url = \"http://10.0.0.1:8765\"",
            "url = \"http://user@127.0.0.1:8765\"",
            "url = \"http://127.0.0.1:8765/v1\"",
            "model = \"gpt-4\"",
            "model = \"tempo/Auto\"",
            "timeout_seconds = 0",
            "timeout_seconds = 601",
            "extra = 1",
        ] {
            let text = format!("{EXAMPLE_TOML}\n[tempo]\nenabled = false\n{bad}\n");
            assert!(parse_config_text(&text).is_err(), "{bad}");
        }
        for good in ["url = \"http://localhost:9000\"", "url = \"http://[::1]:8765\"", "model = \"tempo/heavy\""] {
            let text = format!("{EXAMPLE_TOML}\n[tempo]\nenabled = true\n{good}\n");
            assert!(parse_config_text(&text).is_ok(), "{good}");
        }
        let metrics =
            parse_config_text(&format!("{EXAMPLE_TOML}\n[metrics]\nenabled = true\nport = 9468\n")).unwrap().metrics;
        assert_eq!((metrics.enabled, metrics.port), (true, 9468));
        assert!(parse_config_text(&format!("{EXAMPLE_TOML}\n[metrics]\nenabled = true\nport = 0\n")).is_err());
        assert!(parse_config_text(&format!("{EXAMPLE_TOML}\n[metrics]\nport = 9468\n")).is_err());
    }

    #[test]
    fn paths_resolve_beside_config_but_bare_tool_names_stay_on_path() {
        let directory = std::env::temp_dir().join(format!("dead-air-config-{}", crate::random::uuid4_hex()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("custom.toml");
        std::fs::write(&path, EXAMPLE_TOML.replace("ffmpeg = \"ffmpeg\"", "ffmpeg = \"./bin/ffmpeg\"")).unwrap();
        let config = load_config(&path).unwrap();
        assert_eq!(PathBuf::from(&config.watch.state_path), directory.join("state/dead-air.sqlite3"));
        assert_eq!(PathBuf::from(&config.tools.ffmpeg), directory.join("bin").join("ffmpeg"));
        assert_eq!(config.tools.ffprobe, "ffprobe");
        assert!(!directory.join("state").exists());
        std::fs::write(&path, b"\xff\xfe").unwrap();
        assert_eq!(load_config(&path).unwrap_err().0, "cannot read config (UnicodeDecodeError)");
        assert_eq!(
            load_config(directory.join("missing.toml")).unwrap_err().0,
            "cannot read config (FileNotFoundError)"
        );
        std::fs::remove_dir_all(&directory).unwrap();
    }
}
