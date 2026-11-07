//! An optional loopback-only Prometheus text endpoint for `watch` (`GET /metrics`).
//!
//! Labels carry stream ids, condition names and delivery states only: no URLs, labels,
//! hosts or destination ids. The endpoint binds 127.0.0.1, checks the Host header and is
//! read-only; it never triggers a sample or an alert.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::constants::{CONDITIONS, VERSION};
use crate::model::{AppConfig, Observation};
use crate::observe::incident_signals;
use crate::persistence::IncidentStore;

const STATUSES: &[&str] = &["HEALTHY", "UNHEALTHY", "OFF_AIR", "UNKNOWN", "DEGRADED"];
const SIGNALS: &[&str] = &["good", "bad", "unknown"];

#[derive(Clone, Debug, Default, PartialEq)]
pub struct StreamMetric {
    pub stream_id: String,
    pub status: String,
    pub signals: Vec<(String, String)>,
    pub observed_wall: Option<f64>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct MetricsState {
    pub started_wall: f64,
    pub cycles_total: u64,
    pub last_cycle_duration_s: Option<f64>,
    pub last_cycle_wall: Option<f64>,
    pub cycle_budget_s: f64,
    pub interval_s: f64,
    pub streams: Vec<StreamMetric>,
    pub open_incidents: Vec<(String, String)>,
    pub deliveries: [(String, i64); 3],
    pub deliveries_retrying: i64,
    pub failed_unacknowledged: i64,
}

impl MetricsState {
    pub fn new(config: &AppConfig) -> Self {
        MetricsState {
            started_wall: crate::timefmt::wall(),
            cycle_budget_s: config.watch.cycle_budget_s,
            interval_s: config.watch.interval_s,
            streams: config
                .streams
                .iter()
                .map(|s| StreamMetric { stream_id: s.id.clone(), status: "UNKNOWN".into(), ..Default::default() })
                .collect(),
            deliveries: [("pending".into(), 0), ("sent".into(), 0), ("failed".into(), 0)],
            ..Default::default()
        }
    }

    /// Fold one completed cycle in.
    pub fn record(
        &mut self,
        observations: &[Observation],
        duration_s: f64,
        completed_wall: f64,
        store: Option<&IncidentStore>,
    ) {
        self.cycles_total += 1;
        self.last_cycle_duration_s = Some(duration_s);
        self.last_cycle_wall = Some(completed_wall);
        for observation in observations {
            if let Some(metric) = self.streams.iter_mut().find(|m| m.stream_id == observation.stream_id) {
                metric.status = observation.status.clone();
                metric.signals =
                    incident_signals(observation).into_iter().map(|(c, s)| (c.to_string(), s.to_string())).collect();
                metric.observed_wall = crate::timefmt::parse_iso(&observation.observed_at).flatten();
            }
        }
        if let Some(store) = store {
            if let Ok(snapshot) = metrics_snapshot(store) {
                self.open_incidents = snapshot.0;
                self.deliveries = snapshot.1;
                self.deliveries_retrying = snapshot.2;
                self.failed_unacknowledged = snapshot.3;
            }
        }
    }
}

type Snapshot = (Vec<(String, String)>, [(String, i64); 3], i64, i64);

fn metrics_snapshot(store: &IncidentStore) -> rusqlite::Result<Snapshot> {
    let db = store.db();
    let mut statement =
        db.prepare("SELECT stream_id,condition FROM incidents WHERE closed_wall IS NULL ORDER BY stream_id,condition")?;
    let incidents: Vec<(String, String)> =
        statement.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    let count = |sql: &str| db.query_row(sql, [], |r| r.get::<_, i64>(0));
    let deliveries = [
        ("pending".to_string(), count("SELECT COUNT(*) FROM deliveries WHERE status='pending'")?),
        ("sent".to_string(), count("SELECT COUNT(*) FROM deliveries WHERE status='sent'")?),
        ("failed".to_string(), count("SELECT COUNT(*) FROM deliveries WHERE status='failed'")?),
    ];
    let retrying = count("SELECT COUNT(*) FROM deliveries WHERE status='pending' AND attempts>0")?;
    let unacknowledged = count("SELECT COUNT(*) FROM deliveries WHERE status='failed' AND acknowledged_at IS NULL")?;
    Ok((incidents, deliveries, retrying, unacknowledged))
}

fn label(value: &str) -> String {
    value.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')).take(64).collect()
}

fn number(value: f64) -> String {
    if value.is_finite() { crate::pyjson::float_repr(value) } else { "NaN".into() }
}

/// Prometheus text exposition format 0.0.4.
pub fn render(state: &MetricsState) -> String {
    let mut out = String::new();
    let family = |out: &mut String, name: &str, kind: &str, help: &str| {
        let _ = writeln!(out, "# HELP {name} {help}");
        let _ = writeln!(out, "# TYPE {name} {kind}");
    };
    family(&mut out, "deadair_info", "gauge", "Build information.");
    let _ = writeln!(out, "deadair_info{{version=\"{}\"}} 1", label(VERSION));
    family(
        &mut out,
        "deadair_watch_started_timestamp_seconds",
        "gauge",
        "When this watch process started (Unix time).",
    );
    let _ = writeln!(out, "deadair_watch_started_timestamp_seconds {}", number(state.started_wall));
    family(&mut out, "deadair_cycles_total", "counter", "Completed serial watch cycles in this process.");
    let _ = writeln!(out, "deadair_cycles_total {}", state.cycles_total);
    family(
        &mut out,
        "deadair_last_cycle_duration_seconds",
        "gauge",
        "Duration of the most recent completed cycle, including persistence and alert dispatch.",
    );
    let _ = writeln!(
        out,
        "deadair_last_cycle_duration_seconds {}",
        state.last_cycle_duration_s.map_or("NaN".into(), number)
    );
    family(
        &mut out,
        "deadair_last_cycle_timestamp_seconds",
        "gauge",
        "When the most recent cycle completed (Unix time).",
    );
    let _ =
        writeln!(out, "deadair_last_cycle_timestamp_seconds {}", state.last_cycle_wall.map_or("NaN".into(), number));
    family(&mut out, "deadair_cycle_budget_seconds", "gauge", "Configured watch.cycle_budget_s.");
    let _ = writeln!(out, "deadair_cycle_budget_seconds {}", number(state.cycle_budget_s));
    family(&mut out, "deadair_interval_seconds", "gauge", "Configured watch.interval_s (start to start).");
    let _ = writeln!(out, "deadair_interval_seconds {}", number(state.interval_s));
    family(
        &mut out,
        "deadair_stream_status",
        "gauge",
        "1 for the latest observation status of each stream; UNKNOWN before the first sample.",
    );
    for stream in &state.streams {
        for status in STATUSES {
            let _ = writeln!(
                out,
                "deadair_stream_status{{stream=\"{}\",status=\"{status}\"}} {}",
                label(&stream.stream_id),
                u8::from(stream.status == *status)
            );
        }
    }
    family(
        &mut out,
        "deadair_condition_signal",
        "gauge",
        "1 for the latest per-condition evidence signal (good, bad or unknown) of each stream.",
    );
    for stream in &state.streams {
        for condition in CONDITIONS {
            let current =
                stream.signals.iter().find(|(c, _)| c == condition).map(|(_, s)| s.as_str()).unwrap_or("unknown");
            for signal in SIGNALS {
                let _ = writeln!(
                    out,
                    "deadair_condition_signal{{stream=\"{}\",condition=\"{condition}\",signal=\"{signal}\"}} {}",
                    label(&stream.stream_id),
                    u8::from(current == *signal)
                );
            }
        }
    }
    family(
        &mut out,
        "deadair_observation_timestamp_seconds",
        "gauge",
        "Observation time of each stream's latest sample (Unix time).",
    );
    for stream in &state.streams {
        let _ = writeln!(
            out,
            "deadair_observation_timestamp_seconds{{stream=\"{}\"}} {}",
            label(&stream.stream_id),
            stream.observed_wall.map_or("NaN".into(), number)
        );
    }
    family(&mut out, "deadair_incident_open", "gauge", "1 for each open incident by stream and condition.");
    for stream in &state.streams {
        for condition in CONDITIONS {
            let open = state.open_incidents.iter().any(|(s, c)| s == &stream.stream_id && c == condition);
            let _ = writeln!(
                out,
                "deadair_incident_open{{stream=\"{}\",condition=\"{condition}\"}} {}",
                label(&stream.stream_id),
                u8::from(open)
            );
        }
    }
    family(&mut out, "deadair_open_incidents", "gauge", "Open incidents across all streams.");
    let _ = writeln!(out, "deadair_open_incidents {}", state.open_incidents.len());
    family(&mut out, "deadair_deliveries", "gauge", "Alert delivery rows by status.");
    for (status, count) in &state.deliveries {
        let _ = writeln!(out, "deadair_deliveries{{status=\"{status}\"}} {count}");
    }
    family(
        &mut out,
        "deadair_deliveries_retrying",
        "gauge",
        "Pending deliveries that have already failed at least one attempt.",
    );
    let _ = writeln!(out, "deadair_deliveries_retrying {}", state.deliveries_retrying);
    family(
        &mut out,
        "deadair_delivery_failures_unacknowledged",
        "gauge",
        "Terminal delivery failures no operator has acknowledged.",
    );
    let _ = writeln!(out, "deadair_delivery_failures_unacknowledged {}", state.failed_unacknowledged);
    out
}

/// The running endpoint; dropping it stops the listener.
pub struct MetricsServer {
    pub port: u16,
    server: Arc<tiny_http::Server>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for MetricsServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.server.unblock();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn host_ok(request: &tiny_http::Request, port: u16) -> bool {
    let host = request
        .headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case("Host"))
        .map(|h| h.value.as_str().to_ascii_lowercase())
        .unwrap_or_default();
    let (name, port_text) = match host.split_once(':') {
        Some((name, port_text)) => (name.to_string(), Some(port_text.to_string())),
        None => (host.clone(), None),
    };
    (name == "127.0.0.1" || name == "localhost") && port_text.is_none_or(|p| p == port.to_string())
}

/// Bind `127.0.0.1:port` and serve `GET /metrics` from the shared state.
pub fn start(port: u16, state: Arc<Mutex<MetricsState>>) -> std::io::Result<MetricsServer> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", port))?;
    let bound = listener.local_addr()?.port();
    let server =
        Arc::new(tiny_http::Server::from_listener(listener, None).map_err(|e| std::io::Error::other(e.to_string()))?);
    let stop = Arc::new(AtomicBool::new(false));
    let (thread_server, thread_stop) = (Arc::clone(&server), Arc::clone(&stop));
    let thread = std::thread::Builder::new().name("dead-air-metrics".into()).spawn(move || {
        while !thread_stop.load(Ordering::SeqCst) {
            let request = match thread_server.recv_timeout(Duration::from_millis(250)) {
                Ok(Some(request)) => request,
                Ok(None) => continue,
                Err(_) => break,
            };
            let path = request.url().split('?').next().unwrap_or("").to_string();
            let (status, content_type, body) = if !host_ok(&request, bound) {
                (421, "text/plain; charset=utf-8", "unexpected Host\n".to_string())
            } else if path != "/metrics" {
                (404, "text/plain; charset=utf-8", "not found\n".to_string())
            } else if !matches!(request.method(), tiny_http::Method::Get | tiny_http::Method::Head) {
                (405, "text/plain; charset=utf-8", "method not allowed\n".to_string())
            } else {
                let text = render(&state.lock().unwrap_or_else(|e| e.into_inner()));
                (200, "text/plain; version=0.0.4; charset=utf-8", text)
            };
            let mut response = tiny_http::Response::from_string(body).with_status_code(status);
            for (name, value) in
                [("Content-Type", content_type), ("Cache-Control", "no-store"), ("X-Content-Type-Options", "nosniff")]
            {
                if let Ok(header) = tiny_http::Header::from_bytes(name.as_bytes(), value.as_bytes()) {
                    response.add_header(header);
                }
            }
            let _ = request.respond(response);
        }
    })?;
    Ok(MetricsServer { port: bound, server, stop, thread: Some(thread) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DimensionEvidence;

    fn config() -> AppConfig {
        crate::config::parse_config_text(crate::config::EXAMPLE_TOML).unwrap()
    }

    #[test]
    fn render_exposes_per_stream_conditions_without_urls() {
        let config = config();
        let mut state = MetricsState::new(&config);
        let observation = Observation {
            stream_id: "sample-channel".into(),
            observed_at: "2026-10-23T00:00:00Z".into(),
            status: "UNHEALTHY".into(),
            reason_codes: vec!["AUDIO_SILENCE".into()],
            audio: DimensionEvidence::decoded("UNHEALTHY", &["AUDIO_SILENCE"], 2.0, 2.0),
            video: DimensionEvidence::decoded("HEALTHY", &["VIDEO_DECODED_NO_EVENT"], 2.0, 2.0),
        };
        state.record(&[observation], 3.25, 1_792_713_603.0, None);
        state.open_incidents = vec![("sample-channel".into(), "audio_silence".into())];
        state.failed_unacknowledged = 2;
        let text = render(&state);
        assert!(text.contains("deadair_cycles_total 1\n"));
        assert!(text.contains("deadair_last_cycle_duration_seconds 3.25\n"));
        assert!(text.contains("deadair_stream_status{stream=\"sample-channel\",status=\"UNHEALTHY\"} 1\n"));
        assert!(text.contains("deadair_stream_status{stream=\"sample-channel\",status=\"HEALTHY\"} 0\n"));
        assert!(text.contains(
            "deadair_condition_signal{stream=\"sample-channel\",condition=\"audio_silence\",signal=\"bad\"} 1\n"
        ));
        assert!(text.contains(
            "deadair_condition_signal{stream=\"sample-channel\",condition=\"video_freeze\",signal=\"good\"} 1\n"
        ));
        assert!(
            text.contains(
                "deadair_condition_signal{stream=\"sample-channel\",condition=\"off_air\",signal=\"good\"} 1\n"
            )
        );
        assert!(text.contains("deadair_incident_open{stream=\"sample-channel\",condition=\"audio_silence\"} 1\n"));
        assert!(text.contains("deadair_open_incidents 1\n"));
        assert!(text.contains("deadair_delivery_failures_unacknowledged 2\n"));
        assert!(text.contains("deadair_observation_timestamp_seconds{stream=\"sample-channel\"} 1792713600.0\n"));
        assert!(!text.contains("example.invalid"));
        assert!(!text.contains("Sample channel"));
        for line in text.lines().filter(|l| !l.starts_with('#')) {
            let value = line.rsplit(' ').next().unwrap();
            assert!(value == "NaN" || value.parse::<f64>().is_ok(), "{line}");
        }
    }

    #[test]
    fn before_the_first_cycle_values_are_unknown_not_healthy() {
        let text = render(&MetricsState::new(&config()));
        assert!(text.contains("deadair_last_cycle_duration_seconds NaN\n"));
        assert!(text.contains("deadair_stream_status{stream=\"sample-channel\",status=\"UNKNOWN\"} 1\n"));
        assert!(text.contains(
            "deadair_condition_signal{stream=\"sample-channel\",condition=\"off_air\",signal=\"unknown\"} 1\n"
        ));
    }

    #[test]
    fn endpoint_is_loopback_only_and_checks_host() {
        let state = Arc::new(Mutex::new(MetricsState::new(&config())));
        let server = start(0, state).unwrap();
        let get = |path: &str, host: &str| -> (u16, String) {
            use std::io::{Read, Write};
            let mut stream = std::net::TcpStream::connect(("127.0.0.1", server.port)).unwrap();
            write!(stream, "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").unwrap();
            let mut text = String::new();
            stream.read_to_string(&mut text).unwrap();
            let status = text.split(' ').nth(1).unwrap().parse().unwrap();
            (status, text)
        };
        let host = format!("127.0.0.1:{}", server.port);
        let (status, text) = get("/metrics", &host);
        assert_eq!(status, 200);
        assert!(text.contains("deadair_cycles_total 0"));
        assert!(text.to_ascii_lowercase().contains("cache-control: no-store"));
        assert_eq!(get("/metrics", "evil.example").0, 421);
        assert_eq!(get("/", &host).0, 404);
        drop(server);
    }
}
