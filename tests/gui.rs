//! The local page (Python `test_gui.py`): a real server on 127.0.0.1:0 over a temporary config.
//! Nothing leaves the machine.

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use dead_air::alerts::SendOutcome;
use dead_air::gui::{Gui, make_server};
use dead_air::model::*;
use dead_air::persistence::IncidentStore;
use serde_json::{Value, json};
use sha2::Digest;

const DESTINATIONS: &str = "\n[[destinations]]\nid = \"ops-ntfy\"\ntype = \"ntfy\"\nendpoint_env = \"DEAD_AIR_TEST_NTFY_URL\"\n\n[[destinations]]\nid = \"ops-discord\"\ntype = \"discord\"\nendpoint_env = \"DEAD_AIR_TEST_DISCORD_URL\"\n";

struct Page {
    gui: Arc<Gui>,
    thread: Option<std::thread::JoinHandle<()>>,
    temp: Temp,
    config: PathBuf,
    state: PathBuf,
}

impl Page {
    fn new(destinations: bool, create_config: bool) -> Self {
        Self::with_idle(destinations, create_config, 3600.0)
    }

    fn with_idle(destinations: bool, create_config: bool, idle_s: f64) -> Self {
        let temp = Temp::new();
        let config = temp.join("dead-air.toml");
        let state = temp.join("state").join("dead-air.sqlite3");
        if create_config {
            write_config(temp.path(), if destinations { DESTINATIONS } else { "" });
        }
        let gui = make_server(&config, 0, idle_s).unwrap();
        let runner = Arc::clone(&gui);
        let thread = std::thread::spawn(move || runner.serve_forever());
        Page { gui, thread: Some(thread), temp, config, state }
    }

    fn port(&self) -> u16 {
        self.gui.port
    }

    fn raw(&self, request: &str, body: &[u8]) -> (u16, String, Vec<u8>) {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port())).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let _ = stream.write_all(body);
        let mut raw = Vec::new();
        let _ = stream.read_to_end(&mut raw);
        let split = raw.windows(4).position(|w| w == b"\r\n\r\n").expect("response head");
        let head = String::from_utf8_lossy(&raw[..split]).into_owned();
        let status = head.split(' ').nth(1).unwrap().parse().unwrap();
        (status, head, raw[split + 4..].to_vec())
    }

    fn request(&self, method: &str, path: &str, body: &[u8], headers: &[(&str, String)]) -> (u16, String, Vec<u8>) {
        let mut head = format!("{method} {path} HTTP/1.1\r\nConnection: close\r\n");
        if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("Host")) {
            head.push_str(&format!("Host: 127.0.0.1:{}\r\n", self.port()));
        }
        for (name, value) in headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        if method == "POST" && !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("Content-Length")) {
            head.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        head.push_str("\r\n");
        self.raw(&head, body)
    }

    fn get(&self, path: &str) -> (u16, String, Vec<u8>) {
        self.request("GET", path, b"", &[])
    }

    fn get_json(&self, path: &str) -> Value {
        let (status, _, body) = self.get(path);
        assert_eq!(status, 200);
        serde_json::from_slice(&body).unwrap()
    }

    fn post(&self, path: &str, body: &[u8], token: bool, extra: &[(&str, String)]) -> (u16, Value) {
        let mut headers = vec![("Content-Type", "application/json".to_string())];
        if token {
            headers.push(("X-Token", self.gui.token.clone()));
        }
        headers.extend(extra.iter().cloned());
        let (status, _, data) = self.request("POST", path, body, &headers);
        (status, serde_json::from_slice(&data).unwrap_or(Value::Null))
    }

    fn post_json(&self, path: &str, payload: Value) -> (u16, Value) {
        self.post(path, payload.to_string().as_bytes(), true, &[])
    }

    fn add(&self, name: &str, url: &str, source: &str) -> (u16, Value) {
        self.post_json("/api/streams", json!({"name": name, "url": url, "source": source}))
    }

    fn add_with(&self, extra: &[(&str, String)], token: bool) -> (u16, Value) {
        let body = json!({"name": "Second stream", "url": "https://example.invalid/live/second", "source": "direct"})
            .to_string();
        self.post("/api/streams", body.as_bytes(), token, extra)
    }
}

impl Drop for Page {
    fn drop(&mut self) {
        self.gui.stop_watch();
        self.gui.shutdown();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = &self.temp;
    }
}

fn repo(path: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(path)
}

#[test]
fn index_carries_the_per_run_token_and_no_inline_script_or_style() {
    let page = Page::new(false, true);
    let (status, head, body) = page.get("/");
    let text = String::from_utf8(body).unwrap();
    assert_eq!(status, 200);
    assert!(head.to_lowercase().contains("content-type: text/html"));
    let token = text.split("<meta name=\"x-token\" content=\"").nth(1).unwrap().split('"').next().unwrap();
    assert_eq!(token, page.gui.token);
    assert!(token.len() >= 43);
    assert!(!text.contains("__TOKEN__"));
    assert!(!text.contains("<style"));
    for chunk in text.split("<script").skip(1) {
        assert!(chunk.split('>').next().unwrap().contains(" src="), "inline script");
    }
    assert!(!text.contains(" style="));
    assert!(!text.contains(" onclick=") && !text.contains(" onload="));
}

#[test]
fn assets_are_served_and_the_stamp_leads_the_stylesheet() {
    let page = Page::new(false, true);
    let (status, _, css) = page.get("/app.css");
    assert_eq!(status, 200);
    assert!(String::from_utf8_lossy(&css).starts_with("/* Hallmark · component: local GUI · genre: atmospheric"));
    let (status, head, js) = page.get("/app.js");
    assert_eq!(status, 200);
    assert!(head.to_lowercase().contains("javascript"));
    assert!(String::from_utf8_lossy(&js).contains("/api/state"));
}

#[test]
fn brand_files_are_byte_equal_and_nothing_else_is_reachable() {
    let page = Page::new(false, true);
    for name in ["mark.svg", "mark-dead.svg", "favicon.svg"] {
        let (status, head, body) = page.get(&format!("/brand/{name}"));
        assert_eq!(status, 200);
        assert!(head.contains("Content-Type: image/svg+xml"));
        assert_eq!(body, std::fs::read(repo("brand").join(name)).unwrap());
    }
    for path in [
        "/brand/mark-light.svg",
        "/brand/NOTES.md",
        "/brand/../Cargo.toml",
        "/brand/%2e%2e/Cargo.toml",
        "/Cargo.toml",
        "/gui/app.js",
        "/api/streams",
        "/nope",
    ] {
        assert_eq!(page.get(path).0, 404, "{path}");
    }
}

#[test]
fn page_files_have_no_standalone_ai_word_and_no_emoji() {
    for name in ["index.html", "app.css", "app.js"] {
        let text = std::fs::read_to_string(repo("gui").join(name)).unwrap();
        let bytes = text.as_bytes();
        for (i, _) in text.match_indices("AI") {
            let before = i.checked_sub(1).map(|j| bytes[j]);
            let after = bytes.get(i + 2).copied();
            let word = |b: Option<u8>| b.is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_');
            assert!(word(before) || word(after), "standalone AI in {name}");
        }
        assert!(
            !text.chars().any(|c| ('\u{1F000}'..='\u{1FFFF}').contains(&c) || ('\u{2600}'..='\u{27BF}').contains(&c)),
            "{name}"
        );
    }
}

#[test]
fn security_headers_on_every_kind_of_response() {
    let page = Page::new(false, true);
    let responses = vec![
        page.get("/").1,
        page.get("/app.css").1,
        page.get("/app.js").1,
        page.get("/brand/mark.svg").1,
        page.get("/api/state").1,
        page.get("/missing").1,
        page.request("GET", "/", b"", &[("Host", "evil.example".into())]).1,
        page.request("POST", "/api/streams", b"{}", &[("Content-Type", "application/json".into())]).1,
        page.request("PUT", "/api/streams", b"", &[]).1,
    ];
    for head in responses {
        for (name, value) in dead_air::gui::SECURITY_HEADERS {
            assert!(head.contains(&format!("{name}: {value}")), "{name} missing in {head}");
        }
    }
}

#[test]
fn state_shape_before_any_check() {
    let page = Page::new(true, true);
    let state = page.get_json("/api/state");
    assert_eq!(state["stream_count"], 1);
    assert_eq!(state["config_path"], page.config.to_string_lossy().as_ref());
    assert_eq!(
        (state["config_missing"].as_bool(), state["config_error"].is_null(), state["state_error"].is_null()),
        (Some(false), true, true)
    );
    assert!(state["last_cycle"].is_null());
    assert_eq!(state["watching"], false);
    assert_eq!(state["tempo"], json!({"enabled": false}));
    assert_eq!(
        state["destinations"],
        json!([{"id": "ops-ntfy", "type": "ntfy"}, {"id": "ops-discord", "type": "discord"}])
    );
    assert_eq!(
        state["streams"][0],
        json!({"id": "sample-channel", "name": "Sample channel", "host": "example.invalid", "source": "direct",
               "status": "UNKNOWN", "severity": "uncertain", "confirming": false, "incidents": [],
               "last_alert": null, "last_checked": null})
    );
    assert_eq!(state["watcher"], json!({"running": false, "pid": null, "started_at": null, "last_line": null}));
}

#[test]
fn page_data_never_carries_full_urls_endpoints_or_secrets() {
    let page = Page::new(true, true);
    let text = String::from_utf8(page.get("/api/state").2).unwrap();
    assert!(text.contains("example.invalid"));
    for forbidden in ["/live/sample", "https://", "DEAD_AIR_TEST_NTFY_URL", "endpoint", "token_env"] {
        assert!(!text.contains(forbidden), "{forbidden}");
    }
}

fn recorder_config(page: &Page, min_bad: i64, unhealthy: f64) -> AppConfig {
    let mut config = incident_config(min_bad, unhealthy, 2, 180.0, 600.0);
    config.destinations = vec![DestinationConfig::webhook("ops-ntfy", "ntfy", "DEAD_AIR_TEST_NTFY_URL")];
    config.watch.state_path = page.state.to_string_lossy().into_owned();
    config
}

#[test]
fn open_incident_last_alert_and_cycle_time_come_from_the_state_file() {
    use dead_air::alerts::DeliveryLedger;
    let page = Page::new(true, true);
    let config = recorder_config(&page, 1, 0.0);
    let now = dead_air::timefmt::wall();
    let digest = |path: &PathBuf| sha2::Sha256::digest(std::fs::read(path).unwrap()).to_vec();
    let (stream, state, before) = {
        let mut store = IncidentStore::open(&page.state, 90).unwrap(); // held as a running watcher would
        store.record_cycle(&config, &[bad_silence()], now, 0.0).unwrap();
        let due = store.due_deliveries(now, 8, &["ops-ntfy".to_string()]).unwrap();
        store.record_delivery(&due[0], true, None, Some(204), now, None).unwrap();
        let before = digest(&page.state);
        let stream = page.get_json("/api/state")["streams"][0].clone();
        let state = page.get_json("/api/state");
        assert_eq!(digest(&page.state), before, "the page never writes state");
        (stream, state, before)
    };
    assert_eq!(
        (stream["status"].as_str(), stream["severity"].as_str(), stream["confirming"].as_bool()),
        (Some("SILENT"), Some("incident"), Some(false))
    );
    assert_eq!(stream["incidents"][0]["kind"], "silent");
    assert_eq!(stream["incidents"][0]["since"], "2026-10-23T00:00:00Z");
    assert_eq!(stream["incidents"][0]["id"].as_str().map(str::len), Some(32));
    assert_eq!(stream["last_alert"]["destination"], "ops-ntfy");
    assert_eq!(stream["last_alert"]["outcome"], "sent");
    assert!(stream["last_alert"]["at"].as_str().unwrap().ends_with('Z'));
    assert_eq!(state["last_cycle"], stream["last_checked"]);
    assert_eq!(state["watching"], true);
    page.get_json("/api/state"); // with the writer gone: still no change from the page
    assert_eq!(digest(&page.state), before);
}

#[test]
fn healthy_after_a_clean_cycle_and_confirming_while_a_bad_streak_builds() {
    let page = Page::new(false, true);
    let config = recorder_config(&page, 3, 100.0);
    let mut store = IncidentStore::open(&page.state, 90).unwrap();
    store.record_cycle(&config, &[healthy("2026-10-23T00:00:00Z")], dead_air::timefmt::wall(), 0.0).unwrap();
    let ok = page.get_json("/api/state")["streams"][0].clone();
    store.record_cycle(&config, &[bad_silence()], dead_air::timefmt::wall(), 1.0).unwrap();
    let confirming = page.get_json("/api/state")["streams"][0].clone();
    assert_eq!((ok["status"].as_str(), ok["severity"].as_str()), (Some("HEALTHY"), Some("ok")));
    assert_eq!((confirming["status"].as_str(), confirming["confirming"].as_bool()), (Some("SILENT"), Some(true)));
    assert_eq!(confirming["incidents"], json!([]));
}

#[test]
fn an_old_cycle_is_not_called_watching() {
    let page = Page::new(false, true);
    let config = recorder_config(&page, 2, 10.0);
    {
        let mut store = IncidentStore::open(&page.state, 90).unwrap();
        store
            .record_cycle(&config, &[healthy("2026-10-23T00:00:00Z")], dead_air::timefmt::wall() - 86_400.0, 0.0)
            .unwrap();
        let state = page.get_json("/api/state");
        assert_eq!(state["watching"], false);
        assert_eq!(state["streams"][0]["status"], "HEALTHY");
    }
}

#[test]
fn unreadable_or_foreign_state_is_reported_not_guessed() {
    let page = Page::new(false, true);
    std::fs::create_dir_all(page.state.parent().unwrap()).unwrap();
    std::fs::write(&page.state, "this is not a database".repeat(100)).unwrap();
    let state = page.get_json("/api/state");
    assert_eq!(state["state_error"], "STATE_UNREADABLE");
    assert_eq!(state["streams"][0]["status"], "UNKNOWN");
    std::fs::remove_file(&page.state).unwrap();
    let db = rusqlite::Connection::open(&page.state).unwrap();
    db.execute_batch("PRAGMA user_version=99; CREATE TABLE t(x);").unwrap();
    drop(db);
    assert_eq!(page.get_json("/api/state")["state_error"], "STATE_SCHEMA_UNSUPPORTED");
}

#[test]
fn a_broken_config_is_shown_with_the_loaders_message() {
    let page = Page::new(false, true);
    std::fs::write(&page.config, "[watch]\n").unwrap();
    let state = page.get_json("/api/state");
    assert!(state["config_error"].as_str().unwrap().contains("missing"));
    assert_eq!(state["streams"], json!([]));
}

#[test]
fn post_without_or_with_the_wrong_token_is_403_and_changes_nothing() {
    let page = Page::new(false, true);
    let before = std::fs::read(&page.config).unwrap();
    assert_eq!(page.add_with(&[], false).0, 403);
    assert_eq!(page.add_with(&[("X-Token", "wrong".into())], false).0, 403);
    assert_eq!(page.post("/api/test-alerts", b"{}", false, &[]).0, 403);
    assert_eq!(std::fs::read(&page.config).unwrap(), before);
}

#[test]
fn unexpected_host_is_421_on_get_and_post() {
    let page = Page::new(false, true);
    let port = page.port();
    for host in [
        "evil.example".to_string(),
        format!("evil.example:{port}"),
        "127.0.0.1:1".into(),
        "localhost.evil.example".into(),
        String::new(),
    ] {
        assert_eq!(page.request("GET", "/api/state", b"", &[("Host", host.clone())]).0, 421, "{host}");
        assert_eq!(page.add_with(&[("Host", host.clone())], true).0, 421, "{host}");
    }
    assert_eq!(page.request("GET", "/api/state", b"", &[("Host", format!("localhost:{port}"))]).0, 200);
    assert_eq!(page.request("GET", "/api/state", b"", &[("Host", "LOCALHOST".into())]).0, 200);
}

#[test]
fn a_foreign_origin_is_403_even_with_a_valid_token() {
    let page = Page::new(false, true);
    let before = std::fs::read(&page.config).unwrap();
    let port = page.port();
    for origin in [
        "https://evil.example".to_string(),
        "http://127.0.0.1:1".into(),
        "null".into(),
        format!("http://evil.example:{port}"),
    ] {
        assert_eq!(page.add_with(&[("Origin", origin.clone())], true).0, 403, "{origin}");
    }
    assert_eq!(std::fs::read(&page.config).unwrap(), before);
    assert_eq!(page.add_with(&[("Origin", format!("http://127.0.0.1:{port}"))], true).0, 200);
}

#[test]
fn body_over_64_kib_is_413_and_exactly_64_kib_is_read() {
    let page = Page::new(false, true);
    assert_eq!(page.post("/api/streams", &vec![b'x'; 64 * 1024 + 1], true, &[]).0, 413);
    let mut exact = vec![b' '; 64 * 1024 - 2];
    exact.extend_from_slice(b"[]");
    assert_eq!(page.post("/api/streams", &exact, true, &[]).0, 400, "read, then rejected as not an object");
    assert_eq!(page.post("/api/streams", &vec![b'x'; 64 * 1024 + 1], false, &[]).0, 403);
    let (status, _, _) = page.request(
        "POST",
        "/api/streams",
        b"{}",
        &[("X-Token", page.gui.token.clone()), ("Content-Length", "nope".into())],
    );
    assert_eq!(status, 411);
}

#[test]
fn malformed_bodies_and_unknown_posts() {
    let page = Page::new(false, true);
    for body in [b"not json".as_slice(), b"[]", b"\"text\"", b"\xff\xfe", b"{}"] {
        assert_eq!(page.post("/api/streams", body, true, &[]).0, 400, "{body:?}");
    }
    assert_eq!(page.post_json("/api/nope", json!({})).0, 404);
}

#[test]
fn valid_stream_is_added_and_the_real_loader_sees_two() {
    let page = Page::new(true, true);
    let before = std::fs::read_to_string(&page.config).unwrap();
    assert_eq!(
        page.add("Second stream", "https://example.invalid/live/second", "direct"),
        (200, json!({"ok": true, "id": "second-stream"}))
    );
    let config = dead_air::config::load_config(&page.config).unwrap();
    assert_eq!(
        config.streams.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
        vec!["sample-channel", "second-stream"]
    );
    let added = &config.streams[1];
    assert_eq!(
        (added.label.as_str(), added.url.as_str(), added.source.as_str()),
        ("Second stream", "https://example.invalid/live/second", "direct")
    );
    assert_eq!(added.unhealthy_after_s, config.streams[0].unhealthy_after_s);
    assert_eq!(config.destinations.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(), vec!["ops-ntfy", "ops-discord"]);
    assert!(std::fs::read_to_string(&page.config).unwrap().starts_with(before.trim_end_matches('\n')));
    let names: Vec<String> = std::fs::read_dir(page.temp.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n != "state")
        .collect();
    assert_eq!(names, vec!["dead-air.toml"], "no temporary file is left behind");
    assert_eq!(page.get_json("/api/state")["stream_count"], 2);
}

#[test]
fn quotes_backslashes_and_unicode_in_a_name_survive_toml_quoting() {
    let page = Page::new(false, true);
    let name = "Say \"hi\" \\ D\u{ed}a de Muertos";
    assert_eq!(page.add(name, "https://example.invalid/live/q", "direct").0, 200);
    assert_eq!(dead_air::config::load_config(&page.config).unwrap().streams[1].label, name);
}

#[test]
fn youtube_source_follows_the_loaders_youtube_rules() {
    let page = Page::new(false, true);
    assert_eq!(page.add("Tube", "https://www.youtube.com/watch?v=abc123", "youtube").0, 200);
    let (status, payload) = page.add("Not tube", "https://example.invalid/watch?v=1", "youtube");
    assert_eq!(status, 400);
    assert!(payload["error"].as_str().unwrap().contains("YouTube"));
}

#[test]
fn duplicates_are_409_and_leave_the_file_alone() {
    let page = Page::new(false, true);
    let before = std::fs::read(&page.config).unwrap();
    assert_eq!(page.add("Another", "https://example.invalid/live/sample", "direct").0, 409);
    assert_eq!(page.add("sample CHANNEL", "https://example.invalid/live/other", "direct").0, 409);
    assert_eq!(page.add("Sample-channel", "https://example.invalid/live/other", "direct").0, 409);
    assert_eq!(std::fs::read(&page.config).unwrap(), before);
}

#[test]
fn invalid_input_is_400_with_the_loaders_message_and_leaves_the_file_alone() {
    let page = Page::new(false, true);
    let before = std::fs::read(&page.config).unwrap();
    let long = "x".repeat(81);
    for (name, url, source) in [
        ("Bad", "ftp://example.invalid/x", "direct"),
        ("Bad", "https://user:pw@example.invalid/x", "direct"),
        ("Bad", "https://example.invalid/x", "rtmp"),
        ("Bad", "", "direct"),
        ("   ", "https://example.invalid/x", "direct"),
        (long.as_str(), "https://example.invalid/x", "direct"),
        ("Inject\"\n[watch]\ninterval_s = 1", "https://example.invalid/x", "direct"),
    ] {
        let (status, payload) = page.add(name, url, source);
        assert_eq!(status, 400, "{name} {url} {source}");
        assert!(!payload["error"].as_str().unwrap().is_empty());
    }
    assert_eq!(page.post_json("/api/streams", json!({"name": "n", "url": 5, "source": "direct"})).0, 400);
    assert_eq!(std::fs::read(&page.config).unwrap(), before);
}

#[test]
fn a_broken_config_is_never_overwritten() {
    let page = Page::new(false, true);
    std::fs::write(&page.config, "[watch]\n").unwrap();
    assert_eq!(page.add("Second stream", "https://example.invalid/live/second", "direct").0, 400);
    assert_eq!(std::fs::read_to_string(&page.config).unwrap(), "[watch]\n");
}

#[test]
fn the_64th_stream_is_accepted_and_the_65th_is_refused_by_the_loader() {
    let page = Page::new(false, true);
    for index in 2..=dead_air::constants::MAX_STREAMS {
        assert_eq!(
            page.add(&format!("Stream {index}"), &format!("https://example.invalid/live/{index}"), "direct").0,
            200
        );
    }
    let (status, payload) = page.add("One too many", "https://example.invalid/live/many", "direct");
    assert_eq!(status, 400);
    assert!(payload["error"].as_str().unwrap().contains("streams"));
    assert_eq!(dead_air::config::load_config(&page.config).unwrap().streams.len(), dead_air::constants::MAX_STREAMS);
}

#[test]
fn missing_config_starts_empty_and_adding_creates_it_from_the_example() {
    let page = Page::new(false, false);
    *page.gui.tool_dir.lock().unwrap() = None;
    let state = page.get_json("/api/state");
    assert_eq!(state["config_missing"], true);
    assert_eq!(
        (state["streams"].clone(), state["stream_count"].clone(), state["destinations"].clone()),
        (json!([]), json!(0), json!([]))
    );
    assert_eq!(page.post_json("/api/test-alerts", json!({})), (200, json!({"results": []})));
    assert_eq!(page.add("First one", "https://example.invalid/live/first", "direct").0, 200);
    let text = std::fs::read_to_string(&page.config).unwrap();
    let config = dead_air::config::load_config(&page.config).unwrap();
    assert_eq!(config.streams.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), vec!["first-one"]);
    assert!(config.destinations.is_empty());
    assert!(text.contains("# Notifications are disabled"));
    assert!(!text.contains("sample-channel"));
    assert_eq!(config.tools.yt_dlp, "yt-dlp");
    assert_eq!(page.get_json("/api/state")["config_missing"], false);
}

#[test]
fn a_created_config_points_yt_dlp_at_a_copy_beside_the_binary() {
    let page = Page::new(false, false);
    let tools = page.temp.join("program");
    std::fs::create_dir_all(&tools).unwrap();
    let tool = tools.join(if cfg!(windows) { "yt-dlp.exe" } else { "yt-dlp" });
    std::fs::write(&tool, b"").unwrap();
    *page.gui.tool_dir.lock().unwrap() = Some(tools);
    assert_eq!(page.add("With tool", "https://example.invalid/live/tool", "direct").0, 200);
    let config = dead_air::config::load_config(&page.config).unwrap();
    assert_eq!(PathBuf::from(&config.tools.yt_dlp), tool);
    assert_eq!(config.tools.ffmpeg, "ffmpeg");
}

#[test]
fn each_destination_gets_one_labelled_event_through_the_existing_sender() {
    let page = Page::new(true, true);
    let calls: Arc<Mutex<Vec<(String, Value, f64)>>> = Arc::default();
    let recorded = Arc::clone(&calls);
    *page.gui.alert_sender.lock().unwrap() =
        Arc::new(move |d: &DestinationConfig, e: &Value, _n: &NetworkConfig, timeout: f64| {
            recorded.lock().unwrap().push((d.id.clone(), e.clone(), timeout));
            if d.id == "ops-ntfy" {
                SendOutcome::sent(Some(204))
            } else {
                SendOutcome { status: Some(500), ..SendOutcome::failed("HTTP_STATUS") }
            }
        });
    let (status, payload) = page.post_json("/api/test-alerts", json!({}));
    assert_eq!(status, 200);
    assert_eq!(
        payload["results"],
        json!([{"id": "ops-ntfy", "type": "ntfy", "ok": true, "detail": "sent"}, {"id": "ops-discord", "type": "discord", "ok": false, "detail": "HTTP_STATUS (500)"}])
    );
    let calls = calls.lock().unwrap();
    assert_eq!(calls.iter().map(|c| c.0.as_str()).collect::<Vec<_>>(), vec!["ops-ntfy", "ops-discord"]);
    let event = &calls[0].1;
    assert_eq!(event["transition"], "test");
    assert!(event["message"].as_str().unwrap().starts_with("DEAD AIR test alert from the GUI, 20"));
    assert_eq!(
        dead_air::alerts::notification_summary(event).unwrap(),
        format!("Dead Air test: alert from the GUI ({})", event["event_id"].as_str().unwrap())
    );
    assert_eq!(calls[0].2, dead_air::constants::NOTIFICATION_ATTEMPT_BUDGET_S);
    assert!(!payload.to_string().contains("DEAD_AIR_TEST"));
}

#[test]
fn the_real_sender_fails_closed_when_the_endpoint_variable_is_not_set() {
    let page = Page::new(true, true);
    *page.gui.alert_sender.lock().unwrap() =
        Arc::new(|d: &DestinationConfig, e: &Value, n: &NetworkConfig, timeout: f64| {
            let launcher = dead_air::alerts::ChildLauncher { exe: binary(), extra_env: vec![] };
            dead_air::alerts::send_destination_bounded(d, e, n, timeout.max(10.0), &|_| None, &now_mono, &launcher)
        });
    let (status, payload) = page.post_json("/api/test-alerts", json!({}));
    assert_eq!(status, 200);
    let results: Vec<(bool, String)> = payload["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["ok"].as_bool().unwrap(), r["detail"].as_str().unwrap().to_string()))
        .collect();
    assert_eq!(results, vec![(false, "ENDPOINT_UNAVAILABLE".to_string()); 2]);
}

#[test]
fn a_page_load_or_state_poll_never_sends() {
    let page = Page::new(true, true);
    let calls = Arc::new(Mutex::new(0));
    let counted = Arc::clone(&calls);
    *page.gui.alert_sender.lock().unwrap() =
        Arc::new(move |_d: &DestinationConfig, _e: &Value, _n: &NetworkConfig, _t: f64| {
            *counted.lock().unwrap() += 1;
            SendOutcome::sent(None)
        });
    page.get("/");
    page.get_json("/api/state");
    page.add("Second stream", "https://example.invalid/live/second", "direct");
    assert_eq!(*calls.lock().unwrap(), 0);
}

fn watching(page: &Page) {
    let text = std::fs::read_to_string(&page.config).unwrap().replace("interval_s = 60", "interval_s = 1");
    std::fs::write(&page.config, text).unwrap();
    *page.gui.watch_argv.lock().unwrap() = vec![
        binary().to_string_lossy().into_owned(),
        "watch".into(),
        "--config".into(),
        page.config.to_string_lossy().into_owned(),
        "--continuous".into(),
        "--json".into(),
    ];
}

fn wait_for(mut condition: impl FnMut() -> bool, seconds: f64) -> bool {
    let end = Instant::now() + Duration::from_secs_f64(seconds);
    while Instant::now() < end {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

#[test]
fn start_reports_a_running_pid_and_the_last_log_line_then_stop_ends_the_child() {
    let page = Page::new(false, true);
    watching(&page);
    assert_eq!(page.post_json("/api/watch", json!({"action": "start"})), (200, json!({"ok": true})));
    let pid = page.gui.child_pid().unwrap();
    let info = page.get_json("/api/state")["watcher"].clone();
    assert_eq!((info["running"].as_bool(), info["pid"].as_u64()), (Some(true), Some(pid as u64)));
    assert!(info["started_at"].as_str().unwrap().ends_with('Z'));
    assert!(
        wait_for(|| page.get_json("/api/state")["watcher"]["last_line"]["observations"].is_array(), 20.0),
        "the watcher's JSON line reaches the page"
    );
    assert_eq!(page.post_json("/api/watch", json!({"action": "start"})).0, 409);
    assert_eq!(page.post_json("/api/watch", json!({"action": "stop"})), (200, json!({"ok": true})));
    assert!(!page.gui.running());
    assert_eq!(page.get_json("/api/state")["watcher"]["running"], false);
    assert_eq!(page.post_json("/api/watch", json!({"action": "stop"})).0, 200, "stopping nothing is fine");
    assert!(!dead_air::persistence::StateLock::is_held(&page.state), "a stopped watcher's OS lock is released");
    assert_eq!(
        std::fs::read(page.state.with_file_name(".dead-air.lock")).unwrap(),
        dead_air::constants::LEGACY_GUARD,
        "the guard is never removed"
    );
}

#[test]
fn start_is_refused_while_another_process_holds_the_state() {
    let page = Page::new(false, true);
    watching(&page);
    {
        let _writer = IncidentStore::open(&page.state, 90).unwrap();
        let (status, payload) = page.post_json("/api/watch", json!({"action": "start"}));
        assert_eq!(status, 409);
        assert!(payload["error"].as_str().unwrap().contains("another process"));
        assert!(page.gui.child_pid().is_none());
    }
    std::fs::write(page.state.with_file_name(".dead-air.lock"), "token\tpid=1\n").unwrap();
    assert_eq!(page.post_json("/api/watch", json!({"action": "start"})).0, 409, "an unknown legacy owner also blocks");
    assert_eq!(std::fs::read_to_string(page.state.with_file_name(".dead-air.lock")).unwrap(), "token\tpid=1\n");
}

#[test]
fn watch_needs_a_known_action_a_valid_config_and_the_token() {
    let page = Page::new(false, true);
    watching(&page);
    assert_eq!(page.post_json("/api/watch", json!({"action": "restart"})).0, 400);
    assert_eq!(page.post("/api/watch", json!({"action": "start"}).to_string().as_bytes(), false, &[]).0, 403);
    std::fs::write(&page.config, "[watch]\n").unwrap();
    assert_eq!(page.post_json("/api/watch", json!({"action": "start"})).0, 400);
}

#[test]
fn quit_stops_the_child_and_the_server() {
    let mut page = Page::new(false, true);
    watching(&page);
    assert_eq!(page.post_json("/api/watch", json!({"action": "start"})).0, 200);
    assert_eq!(page.post_json("/api/quit", json!({})), (200, json!({"ok": true})));
    assert!(!page.gui.running());
    let thread = page.thread.take().unwrap();
    assert!(wait_for(|| thread.is_finished(), 10.0));
    assert!(wait_for(|| TcpStream::connect(("127.0.0.1", page.port())).is_err(), 10.0));
}

#[test]
fn ping_names_the_app_and_still_checks_the_host_and_quit_needs_the_token() {
    let page = Page::new(false, true);
    watching(&page);
    assert_eq!(page.get_json("/api/ping"), json!({"app": "dead-air"}));
    assert_eq!(page.request("GET", "/api/ping", b"", &[("Host", "evil.example".into())]).0, 421);
    assert_eq!(page.post_json("/api/watch", json!({"action": "start"})).0, 200);
    assert_eq!(page.post("/api/quit", b"{}", false, &[]).0, 403);
    assert!(page.gui.running());
}

#[test]
fn idle_timeout_stops_a_server_with_no_watcher_but_not_while_the_watcher_runs() {
    let mut idle = Page::with_idle(false, true, 1.0);
    let thread = idle.thread.take().unwrap();
    assert!(wait_for(|| thread.is_finished(), 10.0), "an idle page with no watcher stops");

    let mut busy = Page::with_idle(false, true, 1.0);
    watching(&busy);
    assert_eq!(busy.gui.start_watch().0, 200);
    std::thread::sleep(Duration::from_secs(3));
    let thread = busy.thread.take().unwrap();
    assert!(!thread.is_finished(), "the idle clock never stops a page that owns a running watcher");
    busy.gui.stop_watch();
    assert!(wait_for(|| thread.is_finished(), 10.0));
}

#[test]
fn explain_is_available_only_through_the_token_and_reports_the_receipt() {
    let page = Page::new(false, true);
    let text = std::fs::read_to_string(&page.config).unwrap();
    std::fs::write(&page.config, format!("{text}\n[tempo]\nenabled = true\n")).unwrap();
    assert_eq!(page.get_json("/api/state")["tempo"], json!({"enabled": true}));
    *page.gui.explainer.lock().unwrap() = Arc::new(|_c: &AppConfig, id: &str| {
        if id == "missing" {
            return Err((1, "incident missing was not found".into()));
        }
        Ok(
            json!({"ok": true, "incident_id": id, "brief": "Silence for 3 minutes.", "receipt_id": "rcpt-1", "advisory": true}),
        )
    });
    assert_eq!(
        page.post("/api/explain", json!({"incident_id": "a".repeat(32)}).to_string().as_bytes(), false, &[]).0,
        403
    );
    let (status, payload) = page.post_json("/api/explain", json!({"incident_id": "a".repeat(32)}));
    assert_eq!((status, payload["receipt_id"].as_str()), (200, Some("rcpt-1")));
    assert_eq!(page.post_json("/api/explain", json!({"incident_id": "missing"})).0, 404);
    assert_eq!(page.post_json("/api/explain", json!({})).0, 400);
    let js = std::fs::read_to_string(repo("gui/app.js")).unwrap();
    assert!(js.contains("s.tempo") || js.contains("tempo.enabled"), "the button is gated on Tempo being enabled");
}

#[test]
fn a_second_instance_reports_the_first_and_a_foreign_listener_exits_2() {
    let page = Page::new(false, true);
    let (code, out, _) = run_binary(
        page.temp.path(),
        &["gui", "--config", &page.config.to_string_lossy(), "--no-browser", "--port", &page.port().to_string()],
    );
    assert_eq!(code, 0);
    assert!(out.contains(&format!("DEAD AIR GUI: http://127.0.0.1:{}/", page.port())));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (code, out, _) = run_binary(
        page.temp.path(),
        &["gui", "--config", &page.config.to_string_lossy(), "--no-browser", "--port", &port.to_string()],
    );
    assert_eq!(code, 2);
    assert!(out.contains("in use by another program"));
    let (code, out, _) = run_binary(page.temp.path(), &["gui", "--no-browser", "--port", "70000"]);
    assert_eq!(code, 3);
    assert!(out.contains("\"ok\": false"));
}

#[test]
fn the_default_port_and_the_watch_flag_reach_the_gui_command() {
    use clap::Parser;
    use dead_air::cli::{Cli, Command};
    let parsed = Cli::try_parse_from(["dead-air", "gui", "--config", "x.toml", "--no-browser", "--watch"]).unwrap();
    match parsed.command {
        Command::Gui { config, port, no_browser, watch } => {
            assert_eq!((config.config.as_str(), port, no_browser, watch), ("x.toml", 47824, true, true))
        }
        other => panic!("{other:?}"),
    }
    match Cli::try_parse_from(["dead-air", "gui", "--config", "x.toml"]).unwrap().command {
        Command::Gui { port, no_browser, watch, .. } => assert_eq!((port, no_browser, watch), (47824, false, false)),
        other => panic!("{other:?}"),
    }
}

#[cfg(unix)]
#[test]
fn gui_subcommand_prints_the_url_serves_it_and_stops_on_interrupt() {
    use std::io::BufRead;
    let temp = Temp::new();
    let config = write_config(temp.path(), "");
    let mut child = std::process::Command::new(binary())
        .args(["gui", "--config", &config.to_string_lossy(), "--no-browser", "--port", "0"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    std::io::BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
    let port: u16 =
        line.trim().strip_prefix("DEAD AIR GUI: http://127.0.0.1:").unwrap().trim_end_matches('/').parse().unwrap();
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(stream, "GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n").unwrap();
    let mut reply = String::new();
    stream.read_to_string(&mut reply).unwrap();
    assert!(reply.starts_with("HTTP/1.1 200"));
    // SAFETY: signalling our own child.
    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
    assert_eq!(child.wait().unwrap().code(), Some(0));
}
