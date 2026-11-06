//! Read-only operator views over persisted state. No source or notification network access,
//! no writer lock, no state creation, no writes.

use std::path::Path;

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::{Map, Value, json};

use crate::constants::{CONDITIONS, SCHEMA};
use crate::model::{AppConfig, PersistenceError, StreamConfig, is_reason_code};
use crate::persistence::table_exists;
use crate::pyjson::{float, round3};

type Result<T> = std::result::Result<T, PersistenceError>;

const STATUSES: &[&str] = &["HEALTHY", "UNHEALTHY", "OFF_AIR", "UNKNOWN", "DEGRADED", "NOT_APPLICABLE"];

fn fail(code: &str) -> PersistenceError {
    PersistenceError::new(code)
}

fn read_failed<E>(_: E) -> PersistenceError {
    fail("PERSISTENCE_READ_FAILED")
}

fn is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path).map(|m| m.file_type().is_symlink()).unwrap_or(false)
}

fn is_slug(text: &str) -> bool {
    let b = text.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_' || *c == b'-')
}

pub fn is_timestamp_text(text: &str) -> bool {
    !text.is_empty() && text.len() <= 40 && text.bytes().all(|b| b.is_ascii_digit() || b"T:.+Z-".contains(&b))
}

pub fn is_record_id(text: &str) -> bool {
    !text.is_empty() && text.len() <= 64 && text.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// A read-only snapshot connection, or `None` when no state exists yet.
pub struct Snapshot {
    pub db: Connection,
    pub version: i64,
}

pub fn open_snapshot(state_path: &str) -> Result<Option<Snapshot>> {
    let path = crate::config::absolute(Path::new(state_path));
    if is_symlink(&path) || path.parent().map(is_symlink).unwrap_or(false) {
        return Err(fail("STATE_PATH_UNSAFE"));
    }
    if !path.exists() {
        return Ok(None);
    }
    let db = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .map_err(read_failed)?;
    db.busy_timeout(std::time::Duration::from_secs(1)).map_err(read_failed)?;
    db.execute_batch("PRAGMA query_only=ON; BEGIN").map_err(read_failed)?;
    let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0)).map_err(read_failed)?;
    if version != 1 && version != SCHEMA {
        return Err(fail("STATE_SCHEMA_UNSUPPORTED"));
    }
    Ok(Some(Snapshot { db, version }))
}

fn reasons(values: Option<&Value>) -> Vec<String> {
    let Some(Value::Array(items)) = values else {
        return Vec::new();
    };
    items.iter().take(16).filter_map(|v| v.as_str()).filter(|r| is_reason_code(r)).map(str::to_string).collect()
}

/// One stream's freshness-checked status from its stored state row.
pub fn stream_status(stream: &StreamConfig, state: &Map<String, Value>, when: f64) -> Value {
    let empty = Map::new();
    let latest = match state.get("latest") {
        Some(Value::Object(map)) => map,
        _ => &empty,
    };
    let wall = match state.get("last_observed_wall") {
        Some(Value::Number(n)) => n.as_f64().filter(|w| w.is_finite()),
        _ => None,
    };
    let age = wall.map(|w| when - w).filter(|a| a.is_finite());
    let reason = if latest.is_empty() || age.is_none() {
        Some("NO_OBSERVATION")
    } else if state.get("config_fingerprint").and_then(Value::as_str) != Some(stream.fingerprint().as_str()) {
        Some("CONFIG_CHANGED")
    } else if age.is_some_and(|a| a < 0.0) {
        Some("CLOCK_ROLLBACK")
    } else if age.is_some_and(|a| a > stream.max_gap_s) {
        Some("OBSERVATION_STALE")
    } else {
        None
    };
    let raw_status = latest.get("status").and_then(Value::as_str).unwrap_or("UNKNOWN");
    let status = if STATUSES.contains(&raw_status) && raw_status != "NOT_APPLICABLE" { raw_status } else { "UNKNOWN" };
    // Never pass arbitrary stored strings into output or terminal escape sequences.
    let at = latest.get("observed_at").and_then(Value::as_str).filter(|t| is_timestamp_text(t));
    let mut out = Map::new();
    for name in ["audio", "video"] {
        let dimension = match latest.get(name) {
            Some(Value::Object(map)) => map,
            _ => &empty,
        };
        let dim_status =
            dimension.get("status").and_then(Value::as_str).filter(|s| STATUSES.contains(s)).unwrap_or("UNKNOWN");
        out.insert(name.into(), json!({"status": dim_status, "reason_codes": reasons(dimension.get("reason_codes"))}));
    }
    out.insert("stream_id".into(), json!(stream.id));
    out.insert("status".into(), json!(if reason.is_some() { "UNKNOWN" } else { status }));
    out.insert("observation_status".into(), json!(status));
    out.insert("observed_at".into(), json!(at));
    out.insert("age_s".into(), age.filter(|a| *a >= 0.0).map(|a| float(round3(a))).unwrap_or(Value::Null));
    out.insert(
        "reason_codes".into(),
        match reason {
            Some(reason) => json!([reason]),
            None => json!(reasons(latest.get("reason_codes"))),
        },
    );
    Value::Object(out)
}

fn briefs_for(db: &Connection, incident_ids: &[String]) -> Result<Vec<Value>> {
    if incident_ids.is_empty() || !table_exists(db, "tempo_briefs").map_err(read_failed)? {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for id in incident_ids {
        if let Some(brief) = read_brief(db, id)? {
            out.push(brief);
        }
    }
    Ok(out)
}

/// A stored Tempo brief, bounded and sanitised for display.
pub fn read_brief(db: &Connection, incident_id: &str) -> Result<Option<Value>> {
    if !table_exists(db, "tempo_briefs").map_err(read_failed)? {
        return Ok(None);
    }
    let row: Option<(String, String, Option<String>, Option<String>, Option<i64>, String)> = db
        .query_row(
            "SELECT created_at,model,receipt_id,job_id,review_required,brief FROM tempo_briefs WHERE incident_id=?",
            [incident_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        )
        .optional()
        .map_err(read_failed)?;
    Ok(row.map(|(created_at, model, receipt, job, review, brief)| {
        let safe_id = |v: Option<String>| v.filter(|t| t.len() <= 256 && t.bytes().all(|b| b.is_ascii_graphic()));
        let text: String = brief.chars().filter(|c| *c == '\n' || !c.is_control()).take(32_000).collect();
        json!({
            "incident_id": incident_id,
            "created_at": Some(created_at).filter(|t| is_timestamp_text(t)),
            "model": model,
            "receipt_id": safe_id(receipt),
            "job_id": safe_id(job),
            "review_required": review.map(|r| r != 0),
            "advisory": true,
            "brief": text,
        })
    }))
}

/// One consistent read-only snapshot of health, incidents and delivery history.
pub fn read_status(config: &AppConfig, now_wall: Option<f64>) -> Result<Value> {
    let when = now_wall.unwrap_or_else(crate::timefmt::wall);
    let mut states: std::collections::HashMap<String, Map<String, Value>> = Default::default();
    let mut incidents: Vec<Value> = Vec::new();
    let mut incident_ids: Vec<String> = Vec::new();
    let mut counts =
        json!({"pending": 0, "sent": 0, "failed": 0, "unacknowledged_failed": 0, "acknowledged_failed": 0});
    let mut destinations: Vec<Value> = Vec::new();
    let mut briefs = Vec::new();
    let snapshot = open_snapshot(&config.watch.state_path)?;
    let exists = snapshot.is_some();
    if let Some(Snapshot { db, version }) = snapshot {
        for stream in &config.streams {
            let raw: Option<String> = db
                .query_row("SELECT state_json FROM stream_state WHERE stream_id=?", [&stream.id], |r| r.get(0))
                .optional()
                .map_err(read_failed)?;
            if let Some(raw) = raw {
                match serde_json::from_str::<Value>(&raw).map_err(read_failed)? {
                    Value::Object(state) => {
                        states.insert(stream.id.clone(), state);
                    }
                    _ => return Err(read_failed(())),
                }
            }
        }
        let mut statement = db
            .prepare(
                "SELECT incident_id,stream_id,condition,opened_at FROM incidents WHERE closed_wall IS NULL ORDER BY stream_id,condition LIMIT 448",
            )
            .map_err(read_failed)?;
        let rows: Vec<(String, String, String, String)> = statement
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .and_then(|rows| rows.collect())
            .map_err(read_failed)?;
        for (incident_id, sid, condition, opened_at) in rows {
            if is_slug(&sid) && CONDITIONS.contains(&condition.as_str()) {
                let at = Some(opened_at).filter(|t| is_timestamp_text(t));
                let id = Some(incident_id).filter(|t| is_record_id(t));
                if let Some(id) = &id {
                    incident_ids.push(id.clone());
                }
                incidents.push(json!({"stream_id": sid, "condition": condition, "opened_at": at, "incident_id": id}));
            }
        }
        let query = if version == 1 {
            "SELECT destination_id,status,COUNT(*),COUNT(*),0 FROM deliveries GROUP BY destination_id,status ORDER BY destination_id,status"
        } else {
            "SELECT destination_id,status,COUNT(*),SUM(CASE WHEN acknowledged_at IS NULL THEN 1 ELSE 0 END),\
             SUM(CASE WHEN acknowledged_at IS NOT NULL THEN 1 ELSE 0 END) FROM deliveries GROUP BY destination_id,status ORDER BY destination_id,status"
        };
        let mut statement = db.prepare(query).map_err(read_failed)?;
        let rows: Vec<(String, String, i64, i64, i64)> = statement
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
            .and_then(|rows| rows.collect())
            .map_err(read_failed)?;
        for (destination_id, status, count, unacknowledged, acknowledged) in rows {
            if !["pending", "sent", "failed"].contains(&status.as_str()) {
                return Err(read_failed(()));
            }
            counts[&status] = json!(counts[&status].as_i64().unwrap_or(0) + count);
            if status == "failed" {
                counts["unacknowledged_failed"] =
                    json!(counts["unacknowledged_failed"].as_i64().unwrap_or(0) + unacknowledged);
                counts["acknowledged_failed"] =
                    json!(counts["acknowledged_failed"].as_i64().unwrap_or(0) + acknowledged);
            }
            if is_slug(&destination_id) {
                let mut item = json!({"destination_id": destination_id, "status": status, "count": count});
                if status == "failed" {
                    item["unacknowledged"] = json!(unacknowledged);
                    item["acknowledged"] = json!(acknowledged);
                }
                destinations.push(item);
            }
        }
        briefs = briefs_for(&db, &incident_ids)?;
    }
    let empty = Map::new();
    let streams: Vec<Value> =
        config.streams.iter().map(|s| stream_status(s, states.get(&s.id).unwrap_or(&empty), when)).collect();
    let stream_is = |value: &Value, wanted: &[&str]| value["status"].as_str().is_some_and(|s| wanted.contains(&s));
    let unhealthy = streams.iter().any(|s| stream_is(s, &["UNHEALTHY", "OFF_AIR"]))
        || incidents.iter().any(|i| !matches!(i["condition"].as_str(), Some("unknown" | "degraded")))
        || counts["unacknowledged_failed"].as_i64().unwrap_or(0) > 0;
    let uncertain = streams.iter().any(|s| stream_is(s, &["UNKNOWN", "DEGRADED"])) || !incidents.is_empty();
    let status = if unhealthy {
        "UNHEALTHY"
    } else if uncertain {
        "UNKNOWN"
    } else {
        "HEALTHY"
    };
    Ok(json!({
        "ok": status == "HEALTHY", "status": status, "state_available": exists,
        "streams": streams, "open_incidents": incidents, "deliveries": counts,
        "destination_deliveries": destinations, "briefs": briefs, "network_contacted": false,
    }))
}

pub fn status_exit_code(result: &Value) -> i32 {
    match result["status"].as_str() {
        Some("HEALTHY") => 0,
        Some("UNHEALTHY") => 1,
        _ => 2,
    }
}

/// Terminal delivery failures for operators, newest first.
pub fn read_failed_deliveries(config: &AppConfig, limit: i64, offset: i64) -> std::result::Result<Vec<Value>, String> {
    if !(1..=200).contains(&limit) {
        return Err("limit must be between 1 and 200".into());
    }
    if !(0..=i64::from(i32::MAX)).contains(&offset) {
        return Err("offset must be a nonnegative bounded integer".into());
    }
    let snapshot = open_snapshot(&config.watch.state_path).map_err(|e| e.0)?;
    let Some(Snapshot { db, version }) = snapshot else {
        return Ok(Vec::new());
    };
    let query = if version == 1 {
        "SELECT event_id,destination_id,attempts,last_http_status,updated_wall,NULL,NULL FROM deliveries WHERE status='failed' \
         ORDER BY updated_wall DESC,event_id,destination_id LIMIT ? OFFSET ?"
    } else {
        "SELECT event_id,destination_id,attempts,last_http_status,updated_wall,acknowledged_at,acknowledged_by FROM deliveries \
         WHERE status='failed' ORDER BY updated_wall DESC,event_id,destination_id LIMIT ? OFFSET ?"
    };
    type Row = (Value, Value, Value, Value, Value, Value, Value);
    let fetch = || -> rusqlite::Result<Vec<Row>> {
        let mut statement = db.prepare(query)?;
        let rows = statement.query_map([limit, offset], |r| {
            let v = |i: usize| -> rusqlite::Result<Value> {
                Ok(match r.get_ref(i)? {
                    rusqlite::types::ValueRef::Null => Value::Null,
                    rusqlite::types::ValueRef::Integer(n) => json!(n),
                    rusqlite::types::ValueRef::Real(f) => float(f),
                    rusqlite::types::ValueRef::Text(t) => json!(String::from_utf8_lossy(t)),
                    rusqlite::types::ValueRef::Blob(_) => Value::Null,
                })
            };
            Ok((v(0)?, v(1)?, v(2)?, v(3)?, v(4)?, v(5)?, v(6)?))
        })?;
        rows.collect()
    };
    let rows = fetch().map_err(|_| "PERSISTENCE_READ_FAILED".to_string())?;
    let mut result = Vec::new();
    for (event_id, destination_id, attempts, http_status, updated_wall, acknowledged_at, acknowledged_by) in rows {
        let (Some(event_id), Some(destination_id)) = (event_id.as_str(), destination_id.as_str()) else {
            continue;
        };
        if !is_record_id(event_id) || !is_slug(destination_id) {
            continue;
        }
        result.push(json!({
            "event_id": event_id,
            "destination_id": destination_id,
            "attempts": attempts.as_i64().filter(|a| (0..=1000).contains(a)),
            "last_http_status": http_status.as_i64().filter(|s| (100..=599).contains(s)),
            "updated_wall": updated_wall.as_f64().filter(|w| w.is_finite()).map(float),
            "acknowledged_at": acknowledged_at.as_str().filter(|t| is_timestamp_text(t)),
            "acknowledged_by": acknowledged_by.as_str().filter(|t| (1..=80).contains(&t.chars().count()) && t.chars().all(|c| !c.is_control())),
        }));
    }
    Ok(result)
}

pub fn format_status(result: &Value) -> String {
    let mut lines = vec![format!("DEAD AIR  {}", result["status"].as_str().unwrap_or("UNKNOWN"))];
    for stream in result["streams"].as_array().into_iter().flatten() {
        let age = match stream["age_s"].as_f64() {
            None => "never sampled".to_string(),
            Some(age) => format!("{age:.0}s ago"),
        };
        let codes: Vec<&str> =
            stream["reason_codes"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        let reasons = if codes.is_empty() { "no reason codes".to_string() } else { codes.join(", ") };
        lines.push(format!(
            "  {}: {} | {} | {}",
            stream["stream_id"].as_str().unwrap_or(""),
            stream["status"].as_str().unwrap_or(""),
            age,
            reasons
        ));
    }
    for incident in result["open_incidents"].as_array().into_iter().flatten() {
        let id = incident["incident_id"].as_str().map(|id| format!(" (incident {id})")).unwrap_or_default();
        lines.push(format!(
            "  OPEN {}: {}{}",
            incident["stream_id"].as_str().unwrap_or(""),
            incident["condition"].as_str().unwrap_or(""),
            id
        ));
    }
    for brief in result["briefs"].as_array().into_iter().flatten() {
        let receipt = brief["receipt_id"].as_str().unwrap_or("none");
        let first = brief["brief"].as_str().unwrap_or("").lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
        let first: String = first.chars().take(160).collect();
        lines.push(format!(
            "  BRIEF {} (Perennos Tempo, receipt {receipt}, advisory: review before use): {first}",
            brief["incident_id"].as_str().unwrap_or("")
        ));
    }
    let counts = &result["deliveries"];
    lines.push(format!(
        "Alerts: {} pending, {} failed ({} unacknowledged, {} acknowledged), {} sent",
        counts["pending"],
        counts["failed"],
        counts["unacknowledged_failed"],
        counts["acknowledged_failed"],
        counts["sent"]
    ));
    lines.join("\n")
}

/// Incidents (newest first) with their transitions' timing; for MCP and `incidents`.
pub fn list_incidents(config: &AppConfig, open_only: bool, limit: i64) -> Result<Value> {
    let Some(Snapshot { db, .. }) = open_snapshot(&config.watch.state_path)? else {
        return Ok(json!({"state_available": false, "incidents": []}));
    };
    let filter = if open_only { "WHERE closed_wall IS NULL" } else { "" };
    let sql = format!(
        "SELECT incident_id,stream_id,condition,opened_at,closed_at,status FROM incidents {filter} ORDER BY opened_wall DESC,incident_id LIMIT ?"
    );
    let mut statement = db.prepare(&sql).map_err(read_failed)?;
    let rows: Vec<(String, String, String, String, Option<String>, String)> = statement
        .query_map([limit.clamp(1, 500)], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))
        .and_then(|rows| rows.collect())
        .map_err(read_failed)?;
    let has_briefs = table_exists(&db, "tempo_briefs").map_err(read_failed)?;
    let mut incidents = Vec::new();
    for (id, sid, condition, opened_at, closed_at, status) in rows {
        if !is_record_id(&id) || !is_slug(&sid) || !CONDITIONS.contains(&condition.as_str()) {
            continue;
        }
        let brief_receipt = if has_briefs { read_brief(&db, &id)?.map(|b| b["receipt_id"].clone()) } else { None };
        incidents.push(json!({
            "incident_id": id, "stream_id": sid, "condition": condition,
            "opened_at": Some(opened_at).filter(|t| is_timestamp_text(t)),
            "closed_at": closed_at.filter(|t| is_timestamp_text(t)),
            "status": if status == "open" || status == "resolved" { status } else { "unknown".into() },
            "has_tempo_brief": brief_receipt.is_some(),
            "tempo_receipt_id": brief_receipt.unwrap_or(Value::Null),
        }));
    }
    Ok(json!({"state_available": true, "incidents": incidents}))
}

/// Metadata-only summary of one transition's stored evidence.
pub fn evidence_summary(payload: &Value) -> Value {
    let evidence = &payload["evidence"];
    let mut dimensions = Map::new();
    if let Some(Value::Object(affected)) = evidence.get("affected_dimensions") {
        for (name, dim) in affected {
            let coverage = &dim["coverage"];
            dimensions.insert(
                name.clone(),
                json!({
                    "status": dim["status"].as_str().filter(|s| STATUSES.contains(s)),
                    "reason_codes": reasons(dim.get("reason_codes")),
                    "sampled_s": dim["sampled_s"].as_f64().map(float),
                    "decoded_s": dim["decoded_s"].as_f64().map(float),
                    "interval_count": dim["intervals"].as_array().map(|a| a.len()),
                    "longest_interval_s": dim["intervals"].as_array().and_then(|items| {
                        items.iter().filter_map(|i| Some(i["end_s"].as_f64()? - i["start_s"].as_f64()?)).fold(None, |m: Option<f64>, v| Some(m.map_or(v, |m| m.max(v))))
                    }).map(float),
                    "coverage_complete": coverage["complete"].as_bool(),
                    "largest_gap_s": coverage["largest_gap_s"].as_f64().map(float),
                }),
            );
        }
    }
    let threshold = &evidence["threshold"];
    json!({
        "transition": payload["transition"].as_str().filter(|t| *t == "open" || *t == "recovery"),
        "observed_at": payload["observed_at"].as_str().filter(|t| is_timestamp_text(t)),
        "status": payload["status"].as_str().filter(|s| STATUSES.contains(s)),
        "reason_codes": reasons(payload.get("reason_codes")),
        "sample_count": threshold["sample_count"].as_i64(),
        "required_samples": threshold["required_samples"].as_i64(),
        "span_s": threshold["span_s"].as_f64().map(float),
        "required_span_s": threshold["required_span_s"].as_f64().map(float),
        "dimensions": dimensions,
    })
}

/// One incident with metadata-only evidence, delivery counts and any stored brief.
pub fn incident_detail(config: &AppConfig, incident_id: &str) -> Result<Option<Value>> {
    if !is_record_id(incident_id) {
        return Ok(None);
    }
    let Some(Snapshot { db, version }) = open_snapshot(&config.watch.state_path)? else {
        return Ok(None);
    };
    let row: Option<(String, String, String, f64, Option<String>, Option<f64>, String)> = db
        .query_row(
            "SELECT stream_id,condition,opened_at,opened_wall,closed_at,closed_wall,status FROM incidents WHERE incident_id=?",
            [incident_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
        )
        .optional()
        .map_err(read_failed)?;
    let Some((sid, condition, opened_at, opened_wall, closed_at, closed_wall, status)) = row else {
        return Ok(None);
    };
    if !is_slug(&sid) || !CONDITIONS.contains(&condition.as_str()) {
        return Ok(None);
    }
    let mut statement = db
        .prepare("SELECT event_id,payload_json FROM events WHERE incident_id=? ORDER BY created_wall,event_id")
        .map_err(read_failed)?;
    let events: Vec<(String, String)> = statement
        .query_map([incident_id], |r| Ok((r.get(0)?, r.get(1)?)))
        .and_then(|rows| rows.collect())
        .map_err(read_failed)?;
    let mut transitions = Vec::new();
    let mut event_ids = Vec::new();
    for (event_id, payload) in events {
        let payload: Value = serde_json::from_str(&payload).unwrap_or(Value::Null);
        event_ids.push(event_id);
        transitions.push(evidence_summary(&payload));
    }
    let mut deliveries = json!({"pending": 0, "sent": 0, "failed": 0});
    for event_id in &event_ids {
        let mut statement = db
            .prepare("SELECT status,COUNT(*) FROM deliveries WHERE event_id=? GROUP BY status")
            .map_err(read_failed)?;
        let rows: Vec<(String, i64)> = statement
            .query_map([event_id], |r| Ok((r.get(0)?, r.get(1)?)))
            .and_then(|rows| rows.collect())
            .map_err(read_failed)?;
        for (status, count) in rows {
            if let Some(current) = deliveries.get(&status).and_then(Value::as_i64) {
                deliveries[&status] = json!(current + count);
            }
        }
    }
    let _ = version;
    let stream = config.streams.iter().find(|s| s.id == sid);
    let duration = closed_wall.unwrap_or_else(crate::timefmt::wall) - opened_wall;
    Ok(Some(json!({
        "incident_id": incident_id,
        "stream_id": sid,
        "source_label": stream.map(|s| s.label.clone()),
        "source_kind": stream.map(|s| s.source.clone()),
        "condition": condition,
        "status": if status == "open" || status == "resolved" { status } else { "unknown".into() },
        "opened_at": Some(opened_at).filter(|t| is_timestamp_text(t)),
        "closed_at": closed_at.filter(|t| is_timestamp_text(t)),
        "duration_s": Some(duration).filter(|d| d.is_finite() && *d >= 0.0).map(|d| float(round3(d))),
        "ongoing": closed_wall.is_none(),
        "transitions": transitions,
        "deliveries": deliveries,
        "tempo_brief": read_brief(&db, incident_id)?,
    })))
}

/// Delivery rows (no endpoints, no payloads), newest first.
pub fn list_deliveries(config: &AppConfig, status: Option<&str>, limit: i64) -> Result<Value> {
    let Some(Snapshot { db, version }) = open_snapshot(&config.watch.state_path)? else {
        return Ok(json!({"state_available": false, "deliveries": []}));
    };
    let ack = if version == 1 { "NULL,NULL" } else { "d.acknowledged_at,d.acknowledged_by" };
    let filter = if status.is_some() { "WHERE d.status=?1" } else { "WHERE ?1 IS NULL" };
    let sql = format!(
        "SELECT d.event_id,d.destination_id,d.status,d.attempts,d.last_error,d.last_http_status,d.updated_wall,{ack},e.stream_id,e.condition,e.transition \
         FROM deliveries d JOIN events e ON e.event_id=d.event_id {filter} ORDER BY d.updated_wall DESC,d.event_id LIMIT ?2"
    );
    let mut statement = db.prepare(&sql).map_err(read_failed)?;
    type Row = (
        String,
        String,
        String,
        i64,
        Option<String>,
        Option<i64>,
        f64,
        Option<String>,
        Option<String>,
        String,
        String,
        String,
    );
    let rows: Vec<Row> = statement
        .query_map(rusqlite::params![status, limit.clamp(1, 500)], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
                r.get(8)?,
                r.get(9)?,
                r.get(10)?,
                r.get(11)?,
            ))
        })
        .and_then(|rows| rows.collect())
        .map_err(read_failed)?;
    let mut out = Vec::new();
    for (
        event_id,
        destination_id,
        status,
        attempts,
        error,
        http_status,
        updated_wall,
        ack_at,
        ack_by,
        sid,
        condition,
        transition,
    ) in rows
    {
        if !is_record_id(&event_id) || !is_slug(&destination_id) || !is_slug(&sid) {
            continue;
        }
        out.push(json!({
            "event_id": event_id, "destination_id": destination_id, "status": status,
            "attempts": attempts, "last_error": error.filter(|e| is_reason_code(e)),
            "last_http_status": http_status.filter(|s| (100..=599).contains(s)),
            "updated_at": crate::timefmt::iso_seconds(updated_wall),
            "acknowledged_at": ack_at.filter(|t| is_timestamp_text(t)),
            "acknowledged_by": ack_by.filter(|t| t.chars().count() <= 80 && t.chars().all(|c| !c.is_control())),
            "stream_id": sid, "condition": Some(condition).filter(|c| CONDITIONS.contains(&c.as_str())),
            "transition": Some(transition).filter(|t| t == "open" || t == "recovery"),
        }));
    }
    Ok(json!({"state_available": true, "deliveries": out}))
}

/// Configured sources by label and kind only: never URLs, hosts or secrets.
pub fn sources(config: &AppConfig) -> Value {
    let items: Vec<Value> = config
        .streams
        .iter()
        .map(|s| {
            json!({
                "stream_id": s.id, "label": s.label, "kind": s.source,
                "expected_live": s.expected_live, "expect_audio": s.expect_audio, "expect_video": s.expect_video,
                "detect_freeze": s.detect_freeze, "thumbnail_fallback": s.thumbnail_enabled,
                "unhealthy_after_s": float(s.unhealthy_after_s), "min_bad_samples": s.min_bad_samples,
                "recovery_samples": s.recovery_samples, "max_gap_s": float(s.max_gap_s),
            })
        })
        .collect();
    let destinations: Vec<Value> =
        config.destinations.iter().map(|d| json!({"destination_id": d.id, "type": d.kind})).collect();
    json!({"sources": items, "destinations": destinations, "interval_s": float(config.watch.interval_s), "sample_s": float(config.watch.sample_s)})
}
