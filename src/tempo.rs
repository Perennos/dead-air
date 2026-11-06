//! Perennos Tempo client (optional, off by default).
//!
//! Tempo is a local AI router on a loopback URL. DEAD AIR uses it only for `explain`, an
//! operator-requested incident brief built from incident metadata (source label, condition,
//! timings, evidence summary). Alerts never wait on Tempo and never pass through it. Requests
//! are never retried automatically, and the gateway token is never printed or persisted.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::constants::{PRODUCT, TEMPO_TASK_CLASS, VERSION};
use crate::http::{Request, exchange};
use crate::model::AppConfig;
use crate::url::urlsplit;

pub const MAX_MESSAGE_CHARS: usize = 32_000;
const MAX_RESPONSE_BYTES: usize = 1 << 20;

#[derive(Debug, Clone, PartialEq)]
pub struct TempoError(pub String);

impl std::fmt::Display for TempoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub const UNKNOWN_OUTCOME: &str = "Perennos Tempo request outcome is unknown after it was sent; no automatic replay was performed. Check Tempo receipts before retrying.";

struct Target {
    host: String,
    addresses: Vec<IpAddr>,
    port: u16,
    host_header: String,
}

fn target(url: &str) -> Result<Target, TempoError> {
    crate::config::validate_tempo_url(url).map_err(|e| TempoError(e.0))?;
    let parsed = urlsplit(url).map_err(|_| TempoError("tempo.url is malformed".into()))?;
    let host = parsed.hostname().unwrap_or_default();
    let port = parsed.port().ok().flatten().unwrap_or(80) as u16;
    let addresses: Vec<IpAddr> = match host.as_str() {
        "localhost" => vec!["127.0.0.1".parse().expect("ip"), "::1".parse().expect("ip")],
        other => vec![other.parse().map_err(|_| TempoError("tempo.url is malformed".into()))?],
    };
    let host_header = parsed.netloc.clone();
    Ok(Target { host, addresses, port, host_header })
}

fn is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path).map(|m| m.file_type().is_symlink()).unwrap_or(false)
}

/// Where the token would be read from, without reading it.
pub fn token_file(config: &AppConfig) -> Option<PathBuf> {
    let home = if !config.tempo.home.is_empty() {
        Some(config.tempo.home.clone())
    } else {
        std::env::var("TEMPO_HOME").ok().filter(|h| !h.trim().is_empty())
    }?;
    Some(crate::config::expand_user(&home).join("ledger").join("gateway-token"))
}

/// `TEMPO_GATEWAY_TOKEN`, else `<home>/ledger/gateway-token`; trimmed, at least 32 chars.
pub fn token(config: &AppConfig) -> Option<String> {
    let valid = |t: String| -> Option<String> {
        let t = t.trim().to_string();
        (t.len() >= 32 && t.len() <= 4096 && t.bytes().all(|b| b.is_ascii_graphic())).then_some(t)
    };
    if let Ok(env) = std::env::var("TEMPO_GATEWAY_TOKEN") {
        if !env.trim().is_empty() {
            return valid(env);
        }
    }
    let path = token_file(config)?;
    if is_symlink(&path) || path.parent().is_some_and(is_symlink) {
        return None;
    }
    let mut raw = String::new();
    use std::io::Read;
    std::fs::File::open(&path).ok()?.take(4097).read_to_string(&mut raw).ok()?;
    valid(raw)
}

struct Reply {
    status: u16,
    body: Vec<u8>,
    headers: Vec<(String, String)>,
}

fn call(
    url: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&Value>,
    timeout_s: f64,
) -> Result<Reply, TempoError> {
    let target = target(url)?;
    let mut headers = vec![
        ("Accept".to_string(), "application/json".to_string()),
        ("User-Agent".to_string(), format!("Dead-Air/{VERSION}")),
    ];
    if let Some(token) = token {
        headers.push(("Authorization".into(), format!("Bearer {token}")));
    }
    let payload = match body {
        Some(body) => {
            headers.push(("Content-Type".into(), "application/json".into()));
            let raw = serde_json::to_vec(body).map_err(|_| TempoError("request could not be encoded".into()))?;
            if raw.len() > MAX_RESPONSE_BYTES {
                return Err(TempoError("Perennos Tempo request exceeded its size limit".into()));
            }
            raw
        }
        None => Vec::new(),
    };
    let request = Request {
        method,
        tls: false,
        host: &target.host,
        port: target.port,
        addresses: &target.addresses,
        host_header: target.host_header.clone(),
        selector: path.to_string(),
        headers,
        body: &payload,
        max_body: MAX_RESPONSE_BYTES,
        read_body: true,
    };
    let deadline = Instant::now() + Duration::from_secs_f64(timeout_s.clamp(0.1, 660.0));
    match exchange(&request, deadline) {
        Ok(response) => {
            if response.truncated && !response.timed_out {
                return Err(TempoError("Perennos Tempo response exceeded its size limit".into()));
            }
            if (response.timed_out || response.read_error) && method == "POST" {
                return Err(TempoError(UNKNOWN_OUTCOME.into()));
            }
            if response.timed_out {
                return Err(TempoError("Perennos Tempo request timed out".into()));
            }
            Ok(Reply { status: response.status, body: response.body, headers: response.headers })
        }
        Err(error) if !error.sent && error.refused => {
            Err(TempoError(format!("Perennos Tempo is not running at {url}")))
        }
        Err(error) if !error.sent && error.timed_out => {
            Err(TempoError(format!("Perennos Tempo did not answer at {url} before the timeout")))
        }
        Err(error) if !error.sent => Err(TempoError(format!("Perennos Tempo is not reachable at {url}"))),
        Err(_) if method == "POST" => Err(TempoError(UNKNOWN_OUTCOME.into())),
        Err(_) => Err(TempoError(format!("Perennos Tempo did not answer at {url}"))),
    }
}

fn status_error(status: u16) -> TempoError {
    match status {
        401 => TempoError("Tempo rejected the token; rerun Tempo setup".into()),
        503 => TempoError("Tempo has no qualified local engine or GPU lease right now".into()),
        other => TempoError(format!("Perennos Tempo returned HTTP {other}")),
    }
}

/// `GET /health` (unauthenticated).
pub fn health(config: &AppConfig) -> Result<Value, TempoError> {
    let reply = call(&config.tempo.url, "GET", "/health", None, None, config.tempo.timeout_seconds.min(5.0))?;
    if !(200..300).contains(&reply.status) {
        return Err(status_error(reply.status));
    }
    serde_json::from_slice(&reply.body).map_err(|_| TempoError("Perennos Tempo returned an invalid response".into()))
}

/// `GET /v1/models`: the number of models the gateway lists.
pub fn model_count(config: &AppConfig, token: &str) -> Result<usize, TempoError> {
    let reply =
        call(&config.tempo.url, "GET", "/v1/models", Some(token), None, config.tempo.timeout_seconds.min(15.0))?;
    if !(200..300).contains(&reply.status) {
        return Err(status_error(reply.status));
    }
    let payload: Value = serde_json::from_slice(&reply.body)
        .map_err(|_| TempoError("Perennos Tempo returned an invalid response".into()))?;
    payload["data"]
        .as_array()
        .map(Vec::len)
        .ok_or_else(|| TempoError("Perennos Tempo returned an invalid model list".into()))
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChatResult {
    pub content: String,
    pub receipt_id: Option<String>,
    pub job_id: Option<String>,
    pub review_required: Option<bool>,
}

fn safe_header(reply: &Reply, name: &str) -> Option<String> {
    reply
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.trim().to_string())
        .filter(|v| !v.is_empty() && v.len() <= 256 && v.bytes().all(|b| b.is_ascii_graphic()))
}

fn valid_class(text: &str) -> bool {
    let b = text.as_bytes();
    (2..=40).contains(&b.len())
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// One non-streaming chat completion, `privacy_class` always `private`. Never retried.
pub fn chat(
    config: &AppConfig,
    token: &str,
    system: &str,
    user: &str,
    task_class: &str,
) -> Result<ChatResult, TempoError> {
    if system.chars().count() > MAX_MESSAGE_CHARS || user.chars().count() > MAX_MESSAGE_CHARS || user.trim().is_empty()
    {
        return Err(TempoError("Perennos Tempo message exceeds 32000 characters".into()));
    }
    if !valid_class(task_class) {
        return Err(TempoError("task_class is invalid".into()));
    }
    let body = json!({
        "model": config.tempo.model,
        "messages": [{"role": "system", "content": system}, {"role": "user", "content": user}],
        "max_tokens": 1024,
        "temperature": 0.2,
        "stream": false,
        "metadata": {"task_class": task_class, "privacy_class": "private", "workflow": PRODUCT},
    });
    let reply = call(
        &config.tempo.url,
        "POST",
        "/v1/chat/completions",
        Some(token),
        Some(&body),
        config.tempo.timeout_seconds,
    )?;
    if !(200..300).contains(&reply.status) {
        return Err(status_error(reply.status));
    }
    let payload: Value = serde_json::from_slice(&reply.body)
        .map_err(|_| TempoError("Perennos Tempo returned an invalid response".into()))?;
    let content = payload["choices"][0]["message"]["content"]
        .as_str()
        .filter(|c| !c.trim().is_empty())
        .ok_or_else(|| TempoError("Perennos Tempo returned no answer".into()))?;
    let content: String = content.chars().filter(|c| *c == '\n' || !c.is_control()).take(MAX_MESSAGE_CHARS).collect();
    let header_review =
        safe_header(&reply, "x-tempo-review-required").and_then(|v| match v.to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" => Some(true),
            "false" | "0" | "no" => Some(false),
            _ => None,
        });
    let body_review = payload["tempo"]["review_before_use"].as_bool();
    Ok(ChatResult {
        content,
        receipt_id: safe_header(&reply, "x-tempo-receipt-id"),
        job_id: safe_header(&reply, "x-tempo-job-id"),
        review_required: header_review.or(body_review),
    })
}

/// `tempo status`: enabled, URL, health, token present/missing, model count. Loopback only.
pub fn status_report(config: &AppConfig) -> Value {
    let token = token(config);
    let (health, detail) = match health(config) {
        Ok(payload) if payload["status"] == "ok" => ("ok", None),
        Ok(_) => ("unexpected", Some("Perennos Tempo answered without status ok".to_string())),
        Err(error) => ("unreachable", Some(error.0)),
    };
    let (models, models_error) = match (&token, health) {
        (Some(token), "ok") => match model_count(config, token) {
            Ok(count) => (json!(count), None),
            Err(error) => (Value::Null, Some(error.0)),
        },
        _ => (Value::Null, None),
    };
    json!({
        "enabled": config.tempo.enabled,
        "url": config.tempo.url,
        "model": config.tempo.model,
        "health": health,
        "health_detail": detail,
        "token": if token.is_some() { "present" } else { "missing" },
        "models": models,
        "models_detail": models_error,
        "alerts_use_tempo": false,
    })
}

/// Remove anything URL-shaped from operator text before it leaves the process.
pub fn redact_links(text: &str) -> String {
    text.split(' ')
        .map(|word| if word.contains("://") || word.contains('@') { "[link removed]" } else { word })
        .collect::<Vec<_>>()
        .join(" ")
}

pub const SYSTEM_PROMPT: &str = "You write short incident briefs for the operator of DEAD AIR, a 24/7 stream watchdog. \
DEAD AIR measures independent conditions: off_air (the source reported it is not live), video_freeze (frozen picture), \
audio_silence (silence), video_missing and audio_missing (a required track was absent), unknown (evidence was missing or \
inconclusive) and degraded (only a provider thumbnail was available). UNKNOWN never means the stream is live. \
Use only the facts provided. Do not guess causes as facts; you may suggest checks an operator could make. \
Do not recommend changing alert delivery. Reply in plain text, at most eight short lines: what happened, when and for how \
long, what the evidence shows, and what to check next.";

/// The metadata-only facts sent to Tempo for one incident.
pub fn incident_facts(config: &AppConfig, detail: &Value) -> Value {
    let stream = config.streams.iter().find(|s| Some(s.id.as_str()) == detail["stream_id"].as_str());
    let label = detail["source_label"].as_str().map(redact_links);
    let settings = stream.map(|s| {
        json!({
            "expect_audio": s.expect_audio, "expect_video": s.expect_video, "detect_freeze": s.detect_freeze,
            "freeze_window_s": s.freeze_window_s, "silence_window_s": s.silence_window_s, "silence_db": s.silence_db,
            "unhealthy_after_s": s.unhealthy_after_s, "min_bad_samples": s.min_bad_samples,
            "recovery_samples": s.recovery_samples, "sample_s": config.watch.sample_s, "interval_s": config.watch.interval_s,
        })
    });
    json!({
        "source_label": label,
        "source_kind": detail["source_kind"],
        "condition": detail["condition"],
        "status": detail["status"],
        "opened_at": detail["opened_at"],
        "closed_at": detail["closed_at"],
        "duration_s": detail["duration_s"],
        "ongoing": detail["ongoing"],
        "evidence": detail["transitions"],
        "alert_deliveries": detail["deliveries"],
        "detector_settings": settings,
    })
}

/// `explain`: ask Tempo for a brief about one incident and store it with its receipt id.
pub fn explain(config: &AppConfig, incident_id: &str) -> Result<Value, (i32, String)> {
    if !config.tempo.enabled {
        return Err((3, "Perennos Tempo is not enabled; add [tempo] enabled = true to the config".into()));
    }
    let detail = crate::status::incident_detail(config, incident_id)
        .map_err(|e| (3, e.0))?
        .ok_or_else(|| (1, format!("incident {incident_id} was not found in this state file")))?;
    let token = token(config)
        .ok_or_else(|| (3, "Perennos Tempo token is missing; set TEMPO_GATEWAY_TOKEN or [tempo] home".to_string()))?;
    let facts = incident_facts(config, &detail);
    let user = format!("Incident facts (JSON):\n{}", crate::pyjson::dumps(&facts));
    let result = chat(config, &token, SYSTEM_PROMPT, &user, TEMPO_TASK_CLASS).map_err(|e| (3, e.0))?;
    let record = crate::persistence::BriefRecord {
        incident_id: incident_id.to_string(),
        created_wall: crate::timefmt::wall(),
        created_at: crate::timefmt::utc_now(),
        model: config.tempo.model.clone(),
        task_class: TEMPO_TASK_CLASS.into(),
        receipt_id: result.receipt_id.clone(),
        job_id: result.job_id.clone(),
        review_required: result.review_required,
        brief: result.content.clone(),
    };
    let stored = crate::persistence::save_brief(Path::new(&config.watch.state_path), &record);
    let advisory = result.review_required != Some(false);
    let output = json!({
        "ok": stored.is_ok(),
        "incident_id": incident_id,
        "brief": result.content,
        "receipt_id": result.receipt_id,
        "job_id": result.job_id,
        "review_required": result.review_required,
        "advisory": advisory,
        "model": config.tempo.model,
        "task_class": TEMPO_TASK_CLASS,
        "stored": stored.is_ok(),
        "error": stored.err().map(|e| e.0),
    });
    Ok(output)
}

/// MCP tool definitions shared by `mcp`, `tempo policy` and the docs.
pub fn tool_definitions() -> Vec<Value> {
    let limit =
        json!({"type": "integer", "minimum": 1, "maximum": 100, "description": "Maximum rows to return (default 20)."});
    vec![
        json!({"name": "deadair_status", "title": "DEAD AIR status",
               "description": "Persisted health of every configured stream, open incidents and alert delivery counts. Reads local state only; never samples a source or sends an alert.",
               "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}}),
        json!({"name": "deadair_incidents", "title": "DEAD AIR incidents",
               "description": "Recent incidents (newest first) with stream id, condition, open and close times, and whether a Perennos Tempo brief is stored.",
               "inputSchema": {"type": "object", "properties": {
                   "state": {"type": "string", "enum": ["open", "all"], "description": "open (default) or all"},
                   "limit": limit.clone()}, "additionalProperties": false}}),
        json!({"name": "deadair_incident", "title": "DEAD AIR incident detail",
               "description": "One incident with a metadata-only evidence summary (reason codes, sample counts, decoded coverage), delivery counts and any stored brief.",
               "inputSchema": {"type": "object", "properties": {
                   "incident_id": {"type": "string", "minLength": 1, "maxLength": 64, "description": "Incident id from deadair_incidents or dead-air status"}},
                   "required": ["incident_id"], "additionalProperties": false}}),
        json!({"name": "deadair_deliveries", "title": "DEAD AIR alert deliveries",
               "description": "Alert delivery rows (newest first): event id, destination id, status, attempts and last error code. No endpoints or payloads.",
               "inputSchema": {"type": "object", "properties": {
                   "status": {"type": "string", "enum": ["pending", "sent", "failed", "all"], "description": "Filter by status (default all)"},
                   "limit": limit}, "additionalProperties": false}}),
        json!({"name": "deadair_sources", "title": "DEAD AIR sources",
               "description": "Configured sources by id, label and kind (direct or youtube) with detector settings, and destination ids and types. Never URLs, hosts or secrets.",
               "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}}),
    ]
}

/// `tempo policy`: a tool-policy fragment the owner merges (callers are added by the owner).
pub fn policy_fragment() -> Value {
    let mut tools = serde_json::Map::new();
    for tool in tool_definitions() {
        let name = tool["name"].as_str().unwrap_or_default().to_string();
        tools.insert(
            name,
            json!({"risk": "observe", "locality": "local", "description": tool["description"], "parameters": tool["inputSchema"]}),
        );
    }
    json!({"version": 1, "profiles": {PRODUCT: {"max_calls_per_request": 2, "tools": tools}}})
}

/// `tempo mcp-config`: a secret-free MCP client entry with absolute paths.
pub fn mcp_config(config_path: &Path) -> Value {
    let exe = crate::alerts::helper_exe();
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    let config = crate::config::absolute(config_path);
    json!({"mcpServers": {PRODUCT: {"command": exe.to_string_lossy(), "args": ["mcp", "--config", config.to_string_lossy()]}}})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_fragment_follows_the_tempo_schema_rules() {
        let policy = policy_fragment();
        assert_eq!(policy["version"], 1);
        let profile = &policy["profiles"]["deadair"];
        assert_eq!(profile["max_calls_per_request"], 2);
        let allowed = [
            "type",
            "properties",
            "required",
            "additionalProperties",
            "enum",
            "maxLength",
            "minLength",
            "minimum",
            "maximum",
            "items",
            "maxItems",
            "description",
        ];
        for (name, tool) in profile["tools"].as_object().unwrap() {
            assert!(name.starts_with("deadair_") && name.len() <= 64);
            assert_eq!(tool["risk"], "observe");
            assert_eq!(tool["locality"], "local");
            let description = tool["description"].as_str().unwrap();
            assert!((1..=400).contains(&description.len()));
            assert_eq!(tool["parameters"]["additionalProperties"], false);
            fn check(schema: &Value, allowed: &[&str]) {
                for (key, value) in schema.as_object().unwrap() {
                    assert!(allowed.contains(&key.as_str()), "{key}");
                    if key == "properties" {
                        for property in value.as_object().unwrap().values() {
                            check(property, allowed);
                        }
                    }
                }
            }
            check(&tool["parameters"], &allowed);
        }
    }

    #[test]
    fn links_are_redacted_from_labels() {
        assert_eq!(redact_links("Main https://cdn.example/x?token=1 feed"), "Main [link removed] feed");
        assert_eq!(redact_links("ops@example.invalid"), "[link removed]");
        assert_eq!(redact_links("Plain label"), "Plain label");
    }
}
