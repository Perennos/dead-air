//! The local page: config and read-only state on 127.0.0.1.
//!
//! It reads SQLite state read-only (opening the writer would take its lock and reset a
//! running watcher's counters), adds a stream through the same config loader, sends a test
//! alert only when the button is pressed, and starts or stops only the watcher child it
//! owns. Host and Origin checks and a per-run token protect every state-changing request.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};

use crate::alerts::{ChildLauncher, SendOutcome, send_destination_bounded};
use crate::config::{EXAMPLE_TOML, STREAM_FIELD_ORDER, load_config};
use crate::constants::{CONDITIONS, LEGACY_GUARD, NOTIFICATION_ATTEMPT_BUDGET_S, SCHEMA};
use crate::model::{AppConfig, DestinationConfig, NetworkConfig};
use crate::persistence::{StateLock, guard_path};
use crate::pyjson::{self, toml_string};

pub const MAX_BODY: usize = 64 * 1024;
pub const IDLE_S: f64 = 30.0 * 60.0;
const MAX_HANDLERS: usize = 32;

const INDEX_HTML: &str = include_str!("../gui/index.html");
const APP_CSS: &str = include_str!("../gui/app.css");
const APP_JS: &str = include_str!("../gui/app.js");
pub const BRAND: &[(&str, &[u8])] = &[
    ("mark.svg", include_bytes!("../brand/mark.svg")),
    ("mark-dead.svg", include_bytes!("../brand/mark-dead.svg")),
    ("favicon.svg", include_bytes!("../brand/favicon.svg")),
];
pub const SECURITY_HEADERS: &[(&str, &str)] = &[
    (
        "Content-Security-Policy",
        "default-src 'self'; img-src 'self' data:; frame-ancestors 'none'; form-action 'self'; base-uri 'none'",
    ),
    ("X-Frame-Options", "DENY"),
    ("X-Content-Type-Options", "nosniff"),
    ("Referrer-Policy", "no-referrer"),
    ("Cache-Control", "no-store"),
];

/// Incident conditions in the order that decides a stream's status word; the first five are red.
const CONDITION_WORDS: &[(&str, &str)] = &[
    ("off_air", "off-air"),
    ("video_freeze", "frozen"),
    ("video_missing", "no picture"),
    ("audio_silence", "silent"),
    ("audio_missing", "no audio"),
    ("degraded", "degraded"),
    ("unknown", "unknown"),
];

fn word(condition: &str) -> String {
    CONDITION_WORDS
        .iter()
        .find(|(c, _)| *c == condition)
        .map(|(_, w)| w.to_string())
        .unwrap_or_else(|| condition.to_string())
}

pub type AlertSender = Arc<dyn Fn(&DestinationConfig, &Value, &NetworkConfig, f64) -> SendOutcome + Send + Sync>;
pub type Explainer = Arc<dyn Fn(&AppConfig, &str) -> Result<Value, (i32, String)> + Send + Sync>;

#[derive(Default)]
struct Watcher {
    child: Option<Child>,
    started_at: Option<String>,
    log_path: Option<PathBuf>,
}

pub struct Gui {
    pub port: u16,
    pub token: String,
    pub config_path: PathBuf,
    server: tiny_http::Server,
    watcher: Mutex<Watcher>,
    last_request: Mutex<Instant>,
    idle_s: f64,
    stopping: AtomicBool,
    write_lock: Mutex<()>,
    active: AtomicUsize,
    /// The watcher command; tests substitute a small script.
    pub watch_argv: Mutex<Vec<String>>,
    /// Folder searched for a bundled yt-dlp when the first stream creates the config.
    pub tool_dir: Mutex<Option<PathBuf>>,
    pub alert_sender: Mutex<AlertSender>,
    pub explainer: Mutex<Explainer>,
}

pub fn default_watch_argv(config_path: &Path) -> Vec<String> {
    vec![
        crate::alerts::helper_exe().to_string_lossy().into_owned(),
        "watch".into(),
        "--config".into(),
        config_path.to_string_lossy().into_owned(),
        "--continuous".into(),
        "--json".into(),
    ]
}

/// Bind 127.0.0.1:`port` (0 picks a free port).
pub fn make_server(config_path: &Path, port: u16, idle_s: f64) -> std::io::Result<Arc<Gui>> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", port))?;
    let bound = listener.local_addr()?.port();
    let server = tiny_http::Server::from_listener(listener, None).map_err(|e| std::io::Error::other(e.to_string()))?;
    let config_path = crate::config::absolute(config_path);
    let default_sender: AlertSender = Arc::new(|destination, event, network, timeout| {
        let env = |name: &str| std::env::var(name).ok();
        send_destination_bounded(
            destination,
            event,
            network,
            timeout,
            &env,
            &crate::timefmt::monotonic,
            &ChildLauncher::current(),
        )
    });
    let default_explainer: Explainer = Arc::new(|config, id| crate::tempo::explain(config, id));
    Ok(Arc::new(Gui {
        port: bound,
        token: crate::random::token_urlsafe(),
        watch_argv: Mutex::new(default_watch_argv(&config_path)),
        tool_dir: Mutex::new(crate::alerts::helper_exe().parent().map(Path::to_path_buf)),
        config_path,
        server,
        watcher: Mutex::new(Watcher::default()),
        last_request: Mutex::new(Instant::now()),
        idle_s,
        stopping: AtomicBool::new(false),
        write_lock: Mutex::new(()),
        active: AtomicUsize::new(0),
        alert_sender: Mutex::new(default_sender),
        explainer: Mutex::new(default_explainer),
    }))
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

impl Gui {
    pub fn running(&self) -> bool {
        let mut watcher = lock(&self.watcher);
        watcher.child.as_mut().is_some_and(|c| matches!(c.try_wait(), Ok(None)))
    }

    pub fn child_pid(&self) -> Option<u32> {
        lock(&self.watcher).child.as_ref().map(Child::id)
    }

    pub fn shutdown(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.server.unblock();
    }

    pub fn start_watch(&self) -> (u16, Value) {
        let mut watcher = lock(&self.watcher);
        if watcher.child.as_mut().is_some_and(|c| matches!(c.try_wait(), Ok(None))) {
            return (409, json!({"error": "Already watching."}));
        }
        let config = match load_config(&self.config_path) {
            Ok(config) => config,
            Err(error) => return (400, json!({"error": error.0})),
        };
        let state = crate::config::absolute(Path::new(&config.watch.state_path));
        let guard = guard_path(&state);
        let foreign_guard = guard.exists() && std::fs::read(&guard).map(|c| c != LEGACY_GUARD).unwrap_or(true);
        if StateLock::is_held(&state) || foreign_guard {
            return (409, json!({"error": "Already watched by another process."}));
        }
        let Some(directory) = state.parent().map(Path::to_path_buf) else {
            return (500, json!({"error": "could not start the watcher"}));
        };
        let log_path = directory.join("dead-air-watch.log");
        let started = (|| -> std::io::Result<Child> {
            std::fs::create_dir_all(&directory)?;
            let log = std::fs::File::create(&log_path)?; // truncated on each start
            let argv = lock(&self.watch_argv).clone();
            let mut command = Command::new(&argv[0]);
            command.args(&argv[1..]).stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
            crate::process::isolate(&mut command);
            command.spawn()
        })();
        match started {
            Ok(child) => {
                watcher.child = Some(child);
                watcher.started_at = Some(crate::timefmt::utc_now());
                watcher.log_path = Some(log_path);
                (200, json!({"ok": true}))
            }
            Err(_) => (500, json!({"error": "could not start the watcher"})),
        }
    }

    /// Stop the owned watcher: SIGTERM to its group, three seconds to clean up, then SIGKILL.
    /// Lock files are never removed; the operating system releases a stopped writer's lock.
    pub fn stop_watch(&self) -> (u16, Value) {
        let mut watcher = lock(&self.watcher);
        let Some(child) = watcher.child.as_mut() else {
            return (200, json!({"ok": true}));
        };
        if !matches!(child.try_wait(), Ok(None)) {
            return (200, json!({"ok": true}));
        }
        if crate::process::terminate_and_reap(child, 3.0).is_none() {
            return (500, json!({"error": "The watcher did not stop."}));
        }
        (200, json!({"ok": true}))
    }

    pub fn watcher_info(&self) -> Value {
        let running = self.running();
        let watcher = lock(&self.watcher);
        let mut last = Value::Null;
        if let Some(path) = &watcher.log_path {
            if let Ok(mut file) = std::fs::File::open(path) {
                use std::io::{Seek, SeekFrom};
                let size = file.metadata().map(|m| m.len()).unwrap_or(0);
                let _ = file.seek(SeekFrom::Start(size.saturating_sub(262_144)));
                let mut raw = Vec::new();
                let _ = file.read_to_end(&mut raw);
                let text = String::from_utf8_lossy(&raw);
                for line in text.lines().rev() {
                    if let Ok(Value::Object(object)) = serde_json::from_str::<Value>(line) {
                        last = Value::Object(object);
                        break;
                    }
                }
            }
        }
        json!({
            "running": running,
            "pid": if running { watcher.child.as_ref().map(Child::id) } else { None },
            "started_at": if running { watcher.started_at.clone() } else { None },
            "last_line": last,
        })
    }

    /// Serve until `shutdown`, an interrupt, or `idle_s` without a request while no watcher runs.
    pub fn serve_forever(self: &Arc<Self>) {
        while !self.stopping.load(Ordering::SeqCst) {
            if crate::interrupt::pending() != 0 && crate::interrupt::is_interruptible() {
                break;
            }
            let idle = lock(&self.last_request).elapsed().as_secs_f64() > self.idle_s;
            if idle && !self.running() {
                break;
            }
            let request = match self.server.recv_timeout(Duration::from_millis(100)) {
                Ok(Some(request)) => request,
                Ok(None) => continue,
                Err(_) => break,
            };
            *lock(&self.last_request) = Instant::now();
            if self.active.fetch_add(1, Ordering::SeqCst) >= MAX_HANDLERS {
                self.active.fetch_sub(1, Ordering::SeqCst);
                let _ = request.respond(response(
                    503,
                    "application/json; charset=utf-8",
                    b"{\"error\": \"busy\"}".to_vec(),
                ));
                continue;
            }
            let gui = Arc::clone(self);
            let spawned = std::thread::Builder::new().name("dead-air-gui".into()).spawn(move || {
                gui.handle(request);
                gui.active.fetch_sub(1, Ordering::SeqCst);
            });
            if spawned.is_err() {
                self.active.fetch_sub(1, Ordering::SeqCst);
            }
        }
        self.stopping.store(true, Ordering::SeqCst);
    }

    fn host_ok(&self, request: &tiny_http::Request) -> bool {
        let host = header(request, "Host").unwrap_or_default().to_lowercase();
        let (name, port) = match host.split_once(':') {
            Some((name, port)) => (name.to_string(), Some(port.to_string())),
            None => (host.clone(), None),
        };
        (name == "127.0.0.1" || name == "localhost") && port.is_none_or(|p| p == self.port.to_string())
    }

    fn origin_ok(&self, request: &tiny_http::Request) -> bool {
        match header(request, "Origin") {
            None => true,
            Some(origin) => {
                let origin = origin.to_lowercase();
                origin == format!("http://127.0.0.1:{}", self.port)
                    || origin == format!("http://localhost:{}", self.port)
            }
        }
    }

    fn handle(self: &Arc<Self>, mut request: tiny_http::Request) {
        let path = request.url().split(['?', '#']).next().unwrap_or("").to_string();
        let method = request.method().clone();
        let reply = match method {
            tiny_http::Method::Get | tiny_http::Method::Head => self.get(&request, &path),
            tiny_http::Method::Post => self.post(&mut request, &path),
            _ => json_reply(501, json!({"error": "unsupported method"})),
        };
        let _ = request.respond(reply);
    }

    fn get(&self, request: &tiny_http::Request, path: &str) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
        if !self.host_ok(request) {
            return json_reply(421, json!({"error": "unexpected Host"}));
        }
        match path {
            "/api/ping" => json_reply(200, json!({"app": "dead-air"})),
            "/api/state" => {
                let mut state = snapshot(&self.config_path);
                state["watcher"] = self.watcher_info();
                json_reply(200, state)
            }
            "/" => response(200, "text/html; charset=utf-8", INDEX_HTML.replace("__TOKEN__", &self.token).into_bytes()),
            "/app.css" => response(200, "text/css; charset=utf-8", APP_CSS.as_bytes().to_vec()),
            "/app.js" => response(200, "text/javascript; charset=utf-8", APP_JS.as_bytes().to_vec()),
            _ => match path.strip_prefix("/brand/").and_then(|name| BRAND.iter().find(|(n, _)| *n == name)) {
                Some((_, bytes)) => response(200, "image/svg+xml", bytes.to_vec()),
                None => json_reply(404, json!({"error": "not found"})),
            },
        }
    }

    fn post(
        self: &Arc<Self>,
        request: &mut tiny_http::Request,
        path: &str,
    ) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
        let length = header(request, "Content-Length").and_then(|v| v.trim().parse::<u64>().ok());
        let Some(length) = length else {
            return json_reply(411, json!({"error": "Content-Length required"}));
        };
        let mut body = Vec::new();
        let reader = request.as_reader();
        if reader.take(length.min(MAX_BODY as u64)).read_to_end(&mut body).is_err() {
            return json_reply(400, json!({"error": "could not read the request"}));
        }
        if length > MAX_BODY as u64 {
            // Read an oversize body (to 1 MiB) so the 413 can be delivered.
            let _ = std::io::copy(
                &mut request.as_reader().take((length - MAX_BODY as u64).min(1 << 20)),
                &mut std::io::sink(),
            );
        }
        if !self.host_ok(request) {
            return json_reply(421, json!({"error": "unexpected Host"}));
        }
        let supplied = header(request, "X-Token").unwrap_or_default();
        if !self.origin_ok(request) || !crate::random::constant_time_eq(supplied.as_bytes(), self.token.as_bytes()) {
            return json_reply(403, json!({"error": "forbidden"}));
        }
        if length > MAX_BODY as u64 {
            return json_reply(413, json!({"error": "request body too large"}));
        }
        let data: Option<Value> = std::str::from_utf8(&body).ok().and_then(|t| serde_json::from_str(t).ok());
        let (status, payload) = match path {
            "/api/test-alerts" => {
                let sender = lock(&self.alert_sender).clone();
                send_test_alerts(&self.config_path, &*sender)
            }
            "/api/quit" => {
                let (status, payload) = self.stop_watch();
                if status == 200 {
                    self.shutdown();
                }
                (status, payload)
            }
            "/api/watch" => match data.as_ref().and_then(|d| d.get("action")).and_then(Value::as_str) {
                Some("start") => self.start_watch(),
                Some("stop") => self.stop_watch(),
                _ => (400, json!({"error": "action must be start or stop"})),
            },
            "/api/streams" => match &data {
                Some(Value::Object(object)) => {
                    let _guard = lock(&self.write_lock);
                    let tool_dir = lock(&self.tool_dir).clone();
                    add_stream(
                        &self.config_path,
                        object.get("name"),
                        object.get("url"),
                        object.get("source"),
                        tool_dir.as_deref(),
                    )
                }
                _ => (400, json!({"error": "expected a JSON object"})),
            },
            "/api/explain" => {
                let id = data.as_ref().and_then(|d| d.get("incident_id")).and_then(Value::as_str).map(str::to_string);
                match (id, load_config(&self.config_path)) {
                    (_, Err(error)) => (400, json!({"error": error.0})),
                    (None, _) => (400, json!({"error": "incident_id is required"})),
                    (Some(id), Ok(config)) => {
                        let explainer = lock(&self.explainer).clone();
                        match explainer(&config, &id) {
                            Ok(brief) => (200, brief),
                            Err((1, message)) => (404, json!({"error": message})),
                            Err((_, message)) => (502, json!({"error": message})),
                        }
                    }
                }
            }
            _ => (404, json!({"error": "not found"})),
        };
        json_reply(status, payload)
    }
}

fn header(request: &tiny_http::Request, name: &str) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str().to_string())
}

fn response(status: u16, content_type: &str, body: Vec<u8>) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    let mut response = tiny_http::Response::from_data(body).with_status_code(status);
    let mut headers = vec![("Content-Type", content_type)];
    headers.extend_from_slice(SECURITY_HEADERS);
    for (name, value) in headers {
        if let Ok(header) = tiny_http::Header::from_bytes(name.as_bytes(), value.as_bytes()) {
            response.add_header(header);
        }
    }
    response
}

fn json_reply(status: u16, payload: Value) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    response(status, "application/json; charset=utf-8", pyjson::dumps(&payload).into_bytes())
}

fn iso(timestamp: Option<f64>) -> Value {
    timestamp.and_then(crate::timefmt::iso_seconds).map(Value::String).unwrap_or(Value::Null)
}

#[derive(Default)]
struct Row {
    last: Option<f64>,
    pending: BTreeSet<String>,
    open: Vec<(String, String, String)>,
    alert: Option<Value>,
}

fn read_state(path: &Path) -> (std::collections::HashMap<String, Row>, Option<&'static str>) {
    let mut rows: std::collections::HashMap<String, Row> = Default::default();
    if !path.exists() {
        return (rows, None);
    }
    let outcome = (|| -> Result<Option<&'static str>, ()> {
        let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
            .map_err(|_| ())?;
        let _ = db.busy_timeout(Duration::from_secs(2));
        let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0)).map_err(|_| ())?;
        if version != SCHEMA {
            return Ok(Some("STATE_SCHEMA_UNSUPPORTED"));
        }
        let mut statement = db.prepare("SELECT stream_id,state_json FROM stream_state").map_err(|_| ())?;
        let states: Vec<(String, String)> =
            statement.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).and_then(|r| r.collect()).map_err(|_| ())?;
        for (stream_id, raw) in states {
            let state: Value = serde_json::from_str(&raw).map_err(|_| ())?;
            let entry = rows.entry(stream_id).or_default();
            entry.last = state.get("last_wall").and_then(Value::as_f64);
            entry.pending = state
                .get("pending")
                .and_then(Value::as_object)
                .map(|p| p.keys().cloned().collect())
                .unwrap_or_default();
        }
        let mut statement = db
            .prepare("SELECT stream_id,condition,opened_at,incident_id FROM incidents WHERE closed_wall IS NULL ORDER BY opened_wall")
            .map_err(|_| ())?;
        let open: Vec<(String, String, String, String)> = statement
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .and_then(|r| r.collect())
            .map_err(|_| ())?;
        for (stream_id, condition, opened, id) in open {
            rows.entry(stream_id).or_default().open.push((condition, opened, id));
        }
        let mut statement = db
            .prepare(
                "SELECT e.stream_id,d.destination_id,d.status,d.last_error,MAX(d.updated_wall) \
                 FROM deliveries d JOIN events e ON e.event_id=d.event_id GROUP BY e.stream_id",
            )
            .map_err(|_| ())?;
        let alerts: Vec<(String, String, String, Option<String>, Option<f64>)> = statement
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
            .and_then(|r| r.collect())
            .map_err(|_| ())?;
        for (stream_id, destination, status, error, when) in alerts {
            rows.entry(stream_id).or_default().alert =
                Some(json!({"destination": destination, "outcome": status, "detail": error, "at": iso(when)}));
        }
        Ok(None)
    })();
    match outcome {
        Ok(error) => (rows, error),
        Err(()) => (Default::default(), Some("STATE_UNREADABLE")),
    }
}

fn read_briefs(path: &Path, ids: &[String]) -> std::collections::HashMap<String, Value> {
    let mut out = std::collections::HashMap::new();
    let Ok(db) = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
    else {
        return out;
    };
    for id in ids {
        if let Ok(Some(brief)) = crate::status::read_brief(&db, id) {
            out.insert(id.clone(), brief);
        }
    }
    out
}

/// What `/api/state` returns: hosts only, destination ids and types only; no URLs or secrets.
pub fn snapshot(config_path: &Path) -> Value {
    let mut out = json!({
        "config_path": config_path.to_string_lossy(), "config_missing": !config_path.exists(), "config_error": null,
        "state_error": null, "stream_count": 0, "streams": [], "destinations": [], "last_cycle": null,
        "watching": false, "tempo": {"enabled": false},
    });
    if !config_path.exists() {
        return out;
    }
    let config = match load_config(config_path) {
        Ok(config) => config,
        Err(error) => {
            out["config_error"] = json!(error.0);
            return out;
        }
    };
    let state_path = PathBuf::from(&config.watch.state_path);
    let (rows, state_error) = read_state(&state_path);
    out["state_error"] = json!(state_error);
    let incident_ids: Vec<String> = rows.values().flat_map(|r| r.open.iter().map(|(_, _, id)| id.clone())).collect();
    let briefs = if config.tempo.enabled { read_briefs(&state_path, &incident_ids) } else { Default::default() };
    let blank = Row::default();
    let mut streams = Vec::new();
    for stream in &config.streams {
        let row = rows.get(&stream.id).unwrap_or(&blank);
        let opened: BTreeSet<&str> = row.open.iter().map(|(c, _, _)| c.as_str()).collect();
        let top = CONDITION_WORDS.iter().map(|(c, _)| *c).find(|c| opened.contains(c) || row.pending.contains(*c));
        let (status, severity) = match top {
            Some(top) => {
                let red = CONDITIONS[..5].contains(&top);
                (word(top).to_uppercase(), if red { "incident" } else { "uncertain" })
            }
            None if row.last.is_none() => ("UNKNOWN".to_string(), "uncertain"),
            None => ("HEALTHY".to_string(), "ok"),
        };
        let incidents: Vec<Value> = row
            .open
            .iter()
            .map(|(condition, since, id)| {
                let mut item = json!({"kind": word(condition), "since": since, "id": id});
                if let Some(brief) = briefs.get(id) {
                    item["brief"] = brief.clone();
                }
                item
            })
            .collect();
        streams.push(json!({
            "id": stream.id, "name": stream.label,
            "host": crate::url::urlsplit(&stream.url).ok().and_then(|u| u.hostname()),
            "source": stream.source, "status": status, "severity": severity,
            "confirming": top.is_some_and(|t| !opened.contains(t)),
            "incidents": incidents, "last_alert": row.alert.clone().unwrap_or(Value::Null),
            "last_checked": iso(row.last),
        }));
    }
    out["streams"] = Value::Array(streams);
    out["stream_count"] = json!(config.streams.len());
    out["destinations"] =
        Value::Array(config.destinations.iter().map(|d| json!({"id": d.id, "type": d.kind})).collect());
    out["tempo"] = json!({"enabled": config.tempo.enabled});
    let latest = rows.values().filter_map(|r| r.last).fold(None, |m: Option<f64>, v| Some(m.map_or(v, |m| m.max(v))));
    if let Some(latest) = latest {
        out["last_cycle"] = iso(Some(latest));
        out["watching"] =
            json!(crate::timefmt::wall() - latest <= 2.0 * (config.watch.interval_s + config.watch.cycle_budget_s));
    }
    out
}

fn toml_value(value: &toml::Value) -> String {
    match value {
        toml::Value::Boolean(b) => b.to_string(),
        toml::Value::Integer(i) => i.to_string(),
        toml::Value::Float(f) => pyjson::float_repr(*f),
        toml::Value::Array(items) => format!("[{}]", items.iter().map(toml_value).collect::<Vec<_>>().join(", ")),
        toml::Value::String(s) => toml_string(s),
        _ => String::from("\"\""),
    }
}

fn slugify(name: &str) -> String {
    let lower = name.to_lowercase();
    let mut slug = String::new();
    let mut dash = false;
    for c in lower.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            slug.push(c);
            dash = false;
        } else if !dash {
            slug.push('-');
            dash = true;
        }
    }
    let trimmed: String = slug.trim_matches('-').chars().take(64).collect();
    let trimmed = trimmed.trim_matches('-').to_string();
    if trimmed.is_empty() { format!("stream-{}", &crate::random::uuid4_hex()[..6]) } else { trimmed }
}

/// Append one `[[streams]]` block to a candidate file, check it with the real loader, then
/// atomically replace the config.
pub fn add_stream(
    config_path: &Path,
    name: Option<&Value>,
    url: Option<&Value>,
    source: Option<&Value>,
    tool_dir: Option<&Path>,
) -> (u16, Value) {
    let (Some(Value::String(name)), Some(Value::String(url)), Some(Value::String(source))) = (name, url, source) else {
        return (400, json!({"error": "name, url and source must be text"}));
    };
    let (name, url) = (name.trim().to_string(), url.trim().to_string());
    let slug = slugify(&name);
    let (current, streams) = if config_path.exists() {
        let text = match std::fs::read(config_path).map(String::from_utf8) {
            Ok(Ok(text)) => text,
            _ => return (400, json!({"error": "cannot read the config file"})),
        };
        match load_config(config_path) {
            Ok(config) => (text, config.streams),
            Err(error) => return (400, json!({"error": error.0})),
        }
    } else {
        // First stream: start from the example without its sample stream.
        let start = EXAMPLE_TOML.find("[[streams]]").unwrap_or(EXAMPLE_TOML.len());
        let notes = EXAMPLE_TOML.find("# Notifications are disabled").unwrap_or(EXAMPLE_TOML.len());
        let mut text = format!("{}{}", &EXAMPLE_TOML[..start], &EXAMPLE_TOML[notes..]);
        // A yt-dlp beside the dead-air binary, so the config works without PATH changes.
        let names: &[&str] = if cfg!(windows) { &["yt-dlp.exe"] } else { &["yt-dlp"] };
        if let Some(tool) = tool_dir.and_then(|d| names.iter().map(|n| d.join(n)).find(|p| p.is_file())) {
            text = text.replace("yt_dlp = \"yt-dlp\"", &format!("yt_dlp = {}", toml_string(&tool.to_string_lossy())));
        }
        (text, Vec::new())
    };
    if streams.iter().any(|s| s.url == url || s.label.to_lowercase() == name.to_lowercase() || s.id == slug) {
        return (409, json!({"error": "A stream with that name or URL is already in the config."}));
    }
    let example: toml::Table = EXAMPLE_TOML.parse().expect("bundled example parses");
    let defaults =
        example["streams"].as_array().and_then(|a| a.first()).and_then(|t| t.as_table()).cloned().unwrap_or_default();
    let mut block = String::from("\n\n[[streams]]\n");
    for field in STREAM_FIELD_ORDER {
        let value = match *field {
            "id" => toml_string(&slug),
            "label" => toml_string(&name),
            "url" => toml_string(&url),
            "source" => toml_string(source),
            other => defaults.get(other).map(toml_value).unwrap_or_default(),
        };
        block.push_str(&format!("{field} = {value}\n"));
    }
    let candidate = format!("{}{}", current.trim_end_matches('\n'), block);
    let Some(directory) = config_path.parent() else {
        return (500, json!({"error": "could not write the config file"}));
    };
    if std::fs::create_dir_all(directory).is_err() {
        return (500, json!({"error": "could not write the config file"}));
    }
    let temporary = directory.join(format!(".dead-air-{}.tmp", &crate::random::uuid4_hex()[..12]));
    struct Cleanup<'a>(&'a Path);
    impl Drop for Cleanup<'_> {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(self.0);
        }
    }
    let _cleanup = Cleanup(&temporary);
    if std::fs::write(&temporary, candidate.as_bytes()).is_err() {
        return (500, json!({"error": "could not write the config file"}));
    }
    if let Err(error) = load_config(&temporary) {
        return (400, json!({"error": error.0}));
    }
    if let Ok(meta) = std::fs::metadata(config_path) {
        let _ = std::fs::set_permissions(&temporary, meta.permissions());
    } else {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600));
        }
    }
    if std::fs::rename(&temporary, config_path).is_err() {
        return (500, json!({"error": "could not write the config file"}));
    }
    (200, json!({"ok": true, "id": slug}))
}

/// One labelled test event through each configured destination: one bounded attempt each.
pub fn send_test_alerts(
    config_path: &Path,
    sender: &(dyn Fn(&DestinationConfig, &Value, &NetworkConfig, f64) -> SendOutcome + Send + Sync),
) -> (u16, Value) {
    if !config_path.exists() {
        return (200, json!({"results": []}));
    }
    let config = match load_config(config_path) {
        Ok(config) => config,
        Err(error) => return (400, json!({"error": error.0})),
    };
    let now = crate::timefmt::utc_now();
    // The sender's summary reads "Dead Air test: alert from the GUI (<time>)".
    let event = json!({
        "event_id": now, "incident_id": "gui-test", "stream_id": "alert", "condition": "from the GUI",
        "transition": "test", "observed_at": now, "status": "TEST", "reason_codes": [], "evidence": {},
        "message": format!("DEAD AIR test alert from the GUI, {now}"),
    });
    let mut results = Vec::new();
    for destination in &config.destinations {
        let outcome = sender(destination, &event, &config.network, NOTIFICATION_ATTEMPT_BUDGET_S);
        let detail = if outcome.ok {
            "sent".to_string()
        } else {
            let reason = outcome.reason.clone().unwrap_or_else(|| "failed".into());
            match outcome.status {
                Some(status) => format!("{reason} ({status})"),
                None => reason,
            }
        };
        results.push(json!({"id": destination.id, "type": destination.kind, "ok": outcome.ok, "detail": detail}));
    }
    (200, json!({"results": results}))
}

/// Open the page in the default browser (best effort).
pub fn open_browser(url: &str) {
    let mut command = if cfg!(windows) {
        let mut c = Command::new("rundll32");
        c.args(["url.dll,FileProtocolHandler", url]);
        c
    } else if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        c.arg(url);
        c
    } else {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };
    command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    if let Ok(mut child) = command.spawn() {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

fn ping(port: u16) -> bool {
    let address: std::net::IpAddr = "127.0.0.1".parse().expect("ip");
    let request = crate::http::Request {
        method: "GET",
        tls: false,
        host: "127.0.0.1",
        port,
        addresses: &[address],
        host_header: format!("127.0.0.1:{port}"),
        selector: "/api/ping".into(),
        headers: vec![],
        body: b"",
        max_body: 1024,
        read_body: true,
    };
    match crate::http::exchange(&request, Instant::now() + Duration::from_secs(2)) {
        Ok(reply) => serde_json::from_slice::<Value>(&reply.body).ok().is_some_and(|v| v["app"] == "dead-air"),
        Err(_) => false,
    }
}

/// `gui`: serve the page; if the port belongs to another DEAD AIR, open that one instead.
pub fn serve(config_path: &str, port: i64, open: bool, watch: bool, idle_s: f64) -> i32 {
    let Ok(port) = u16::try_from(port) else {
        crate::watch::emit(&pyjson::dumps(
            &json!({"ok": false, "error": format!("cannot listen on 127.0.0.1:{port} (OverflowError)")}),
        ));
        return 3;
    };
    let gui = match make_server(Path::new(config_path), port, idle_s) {
        Ok(gui) => gui,
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
            let url = format!("http://127.0.0.1:{port}/");
            if !ping(port) {
                crate::watch::emit(&pyjson::dumps(
                    &json!({"ok": false, "error": format!("port {port} is in use by another program; use --port N")}),
                ));
                return 2;
            }
            crate::watch::emit(&format!("DEAD AIR GUI: {url}"));
            if open {
                open_browser(&url);
            }
            return 0;
        }
        Err(error) => {
            crate::watch::emit(&pyjson::dumps(
                &json!({"ok": false, "error": format!("cannot listen on 127.0.0.1:{port} ({:?})", error.kind())}),
            ));
            return 3;
        }
    };
    let url = format!("http://127.0.0.1:{}/", gui.port);
    crate::watch::emit(&format!("DEAD AIR GUI: {url}"));
    if watch {
        let (status, payload) = gui.start_watch();
        if status != 200 {
            crate::watch::emit(&pyjson::dumps(&json!({"ok": false, "error": payload["error"]})));
        }
    }
    if open {
        open_browser(&url);
    }
    gui.serve_forever();
    gui.stop_watch();
    0
}
