//! Perennos Tempo client (contract section D) and the read-only MCP server (section E).

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use dead_air::persistence::IncidentStore;
use serde_json::{Value, json};
use sha2::Digest;

const TOKEN: &str = "tempo-test-token-0123456789abcdefghijklmnop";

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Ok,
    Unauthorized,
    Busy,
    Hang,
}

/// `(method, path, headers, body)` for every request the fake gateway saw.
type Seen = Arc<Mutex<Vec<(String, String, Vec<(String, String)>, String)>>>;

struct FakeTempo {
    port: u16,
    requests: Seen,
    count: Arc<AtomicUsize>,
}

fn fake_tempo(mode: Mode) -> FakeTempo {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests: Seen = Arc::default();
    let count = Arc::new(AtomicUsize::new(0));
    let (log, counter) = (Arc::clone(&requests), Arc::clone(&count));
    std::thread::spawn(move || {
        for socket in listener.incoming().flatten() {
            let (log, counter) = (Arc::clone(&log), Arc::clone(&counter));
            std::thread::spawn(move || {
                let mut socket = socket;
                socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut data = Vec::new();
                let mut chunk = [0u8; 8192];
                let head_end = loop {
                    if let Some(i) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i;
                    }
                    match socket.read(&mut chunk) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => data.extend_from_slice(&chunk[..n]),
                    }
                };
                let head = String::from_utf8_lossy(&data[..head_end]).into_owned();
                let mut lines = head.split("\r\n");
                let mut first = lines.next().unwrap().split(' ');
                let (method, path) = (first.next().unwrap().to_string(), first.next().unwrap().to_string());
                let headers: Vec<(String, String)> = lines
                    .filter_map(|l| l.split_once(':'))
                    .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
                    .collect();
                let length: usize =
                    headers.iter().find(|(k, _)| k == "content-length").and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
                let mut body = data[head_end + 4..].to_vec();
                while body.len() < length {
                    match socket.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => body.extend_from_slice(&chunk[..n]),
                    }
                }
                counter.fetch_add(1, Ordering::SeqCst);
                log.lock().unwrap().push((
                    method.clone(),
                    path.clone(),
                    headers.clone(),
                    String::from_utf8_lossy(&body).into_owned(),
                ));
                let authorized = headers.iter().any(|(k, v)| k == "authorization" && v == &format!("Bearer {TOKEN}"));
                let (status, extra, payload) = match (mode, path.as_str()) {
                    (_, "/health") => (200, "", json!({"status": "ok", "mode": "test"})),
                    (Mode::Hang, _) => {
                        std::thread::sleep(Duration::from_secs(4));
                        return;
                    }
                    (Mode::Unauthorized, _) => (401, "", json!({"error": {"code": "unauthorized"}})),
                    (Mode::Busy, _) => (503, "", json!({"error": {"code": "no_engine"}})),
                    (Mode::Ok, _) if !authorized => (401, "", json!({"error": {"code": "unauthorized"}})),
                    (Mode::Ok, "/v1/models") => {
                        (200, "", json!({"data": [{"id": "tempo/auto"}, {"id": "tempo/fast"}]}))
                    }
                    (Mode::Ok, "/v1/chat/completions") => (
                        200,
                        "X-Tempo-Receipt-Id: rcpt-7f3a\r\nX-Tempo-Job-Id: job-42\r\nX-Tempo-Review-Required: true\r\n",
                        json!({"choices": [{"message": {"role": "assistant", "content": "Silence on Sample channel since 00:00 UTC.\nCheck the encoder's audio input."}}]}),
                    ),
                    _ => (404, "", json!({})),
                };
                let body = payload.to_string();
                let reply = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(reply.as_bytes());
            });
        }
    });
    FakeTempo { port, requests, count }
}

fn tempo_config(temp: &Temp, port: u16, timeout: u32) -> std::path::PathBuf {
    let home = temp.join("tempo-home");
    std::fs::create_dir_all(home.join("ledger")).unwrap();
    std::fs::write(home.join("ledger").join("gateway-token"), format!("{TOKEN}\n")).unwrap();
    let extra = format!(
        "\n[[destinations]]\nid = \"ops-hook\"\ntype = \"webhook\"\nendpoint_env = \"SECRET_HOOK_URL\"\n\n[tempo]\nenabled = true\nurl = \"http://127.0.0.1:{port}\"\nhome = {}\ntimeout_seconds = {timeout}\n",
        dead_air::pyjson::toml_string(&home.to_string_lossy())
    );
    write_config(temp.path(), &extra)
}

fn seed_incident(config_path: &std::path::Path) -> String {
    let mut config = dead_air::config::load_config(config_path).unwrap();
    config.streams[0].min_bad_samples = 1;
    config.streams[0].unhealthy_after_s = 0.0;
    let mut store = IncidentStore::open(&config.watch.state_path, 90).unwrap();
    store.record_cycle(&config, &[bad_silence()], dead_air::timefmt::wall(), 0.0).unwrap();
    store.db().query_row("SELECT incident_id FROM incidents", [], |r| r.get(0)).unwrap()
}

fn digest(path: &std::path::Path) -> Vec<u8> {
    sha2::Sha256::digest(std::fs::read(path).unwrap()).to_vec()
}

#[test]
fn explain_sends_metadata_only_and_stores_the_brief_with_its_receipt() {
    let tempo = fake_tempo(Mode::Ok);
    let temp = Temp::new();
    let config_path = tempo_config(&temp, tempo.port, 30);
    let incident = seed_incident(&config_path);
    let config = dead_air::config::load_config(&config_path).unwrap();
    let result = dead_air::tempo::explain(&config, &incident).unwrap();
    assert_eq!(result["ok"], true);
    assert_eq!(result["stored"], true);
    assert_eq!(result["receipt_id"], "rcpt-7f3a");
    assert_eq!(result["job_id"], "job-42");
    assert_eq!(result["advisory"], true);
    assert!(result["brief"].as_str().unwrap().contains("Silence"));
    let requests = tempo.requests.lock().unwrap();
    assert_eq!(requests.len(), 1, "explain makes exactly one request");
    let (method, path, headers, body) = &requests[0];
    assert_eq!((method.as_str(), path.as_str()), ("POST", "/v1/chat/completions"));
    assert!(headers.contains(&("authorization".into(), format!("Bearer {TOKEN}"))));
    let body: Value = serde_json::from_str(body).unwrap();
    assert_eq!(body["model"], "tempo/auto");
    assert_eq!(body["max_tokens"], 1024);
    assert_eq!(body["temperature"], 0.2);
    assert_eq!(body["stream"], false);
    assert_eq!(
        body["metadata"],
        json!({"task_class": "deadair-incident-brief", "privacy_class": "private", "workflow": "deadair"})
    );
    assert_eq!(body["messages"][0]["role"], "system");
    let user = body["messages"][1]["content"].as_str().unwrap();
    assert!(user.contains("Sample channel") && user.contains("audio_silence") && user.contains("AUDIO_SILENCE"));
    let text = body.to_string();
    for forbidden in ["example.invalid", "https://", "SECRET_HOOK_URL", "ops-hook", TOKEN, "sample-channel"] {
        assert!(!text.contains(forbidden), "{forbidden} leaked into the Tempo request");
    }
    for message in body["messages"].as_array().unwrap() {
        assert!(message["content"].as_str().unwrap().chars().count() <= 32_000);
    }
    drop(requests);
    let status = dead_air::status::read_status(&config, None).unwrap();
    assert_eq!(status["briefs"][0]["receipt_id"], "rcpt-7f3a");
    assert_eq!(status["briefs"][0]["incident_id"], incident.as_str());
    let human = dead_air::status::format_status(&status);
    assert!(
        human.contains("BRIEF") && human.contains("receipt rcpt-7f3a") && human.contains("review before use"),
        "{human}"
    );
    let detail = dead_air::status::incident_detail(&config, &incident).unwrap().unwrap();
    assert_eq!(detail["tempo_brief"]["receipt_id"], "rcpt-7f3a");
}

#[test]
fn tempo_failures_are_plain_and_never_retried() {
    let temp = Temp::new();
    let closed = TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_port = closed.local_addr().unwrap().port();
    drop(closed);
    let config_path = tempo_config(&temp, dead_port, 1);
    let incident = seed_incident(&config_path);
    let config = dead_air::config::load_config(&config_path).unwrap();
    let error = dead_air::tempo::explain(&config, &incident).unwrap_err();
    assert_eq!(error, (3, format!("Perennos Tempo is not running at http://127.0.0.1:{dead_port}")));
    for (mode, expected) in [
        (Mode::Unauthorized, "Tempo rejected the token; rerun Tempo setup"),
        (Mode::Busy, "Tempo has no qualified local engine or GPU lease right now"),
        (Mode::Hang, dead_air::tempo::UNKNOWN_OUTCOME),
    ] {
        let tempo = fake_tempo(mode);
        let mut config = config.clone();
        config.tempo.url = format!("http://127.0.0.1:{}", tempo.port);
        let started = Instant::now();
        let error = dead_air::tempo::explain(&config, &incident).unwrap_err();
        assert_eq!(error.1, expected);
        assert!(!error.1.contains(TOKEN));
        assert!(started.elapsed() < Duration::from_secs(3));
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(tempo.count.load(Ordering::SeqCst), 1, "no automatic replay");
    }
    let mut disabled = config.clone();
    disabled.tempo.enabled = false;
    assert_eq!(dead_air::tempo::explain(&disabled, &incident).unwrap_err().0, 3);
    assert_eq!(dead_air::tempo::explain(&config, &"f".repeat(32)).unwrap_err().0, 1);
}

#[test]
fn tempo_status_reports_health_token_and_models_without_printing_the_token() {
    let tempo = fake_tempo(Mode::Ok);
    let temp = Temp::new();
    let config_path = tempo_config(&temp, tempo.port, 30);
    let (code, out, err) = run_binary(temp.path(), &["tempo", "status", "--config", &config_path.to_string_lossy()]);
    assert_eq!(code, 0, "{out}{err}");
    let report = value(&out);
    assert_eq!(
        (report["enabled"].as_bool(), report["health"].as_str(), report["token"].as_str(), report["models"].as_i64()),
        (Some(true), Some("ok"), Some("present"), Some(2))
    );
    assert_eq!(report["alerts_use_tempo"], false);
    assert!(!out.contains(TOKEN) && !err.contains(TOKEN));
    std::fs::remove_file(temp.join("tempo-home").join("ledger").join("gateway-token")).unwrap();
    let output = Command::new(binary())
        .args(["tempo", "status", "--config", &config_path.to_string_lossy()])
        .env_remove("TEMPO_GATEWAY_TOKEN")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(value(&String::from_utf8_lossy(&output.stdout))["token"], "missing");
    let output = Command::new(binary())
        .args(["tempo", "status", "--config", &config_path.to_string_lossy()])
        .env("TEMPO_GATEWAY_TOKEN", TOKEN)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert!(!String::from_utf8_lossy(&output.stdout).contains(TOKEN));
}

#[test]
fn explain_cli_prints_an_advisory_brief_and_status_shows_it() {
    let tempo = fake_tempo(Mode::Ok);
    let temp = Temp::new();
    let config_path = tempo_config(&temp, tempo.port, 30);
    let incident = seed_incident(&config_path);
    let config_arg = config_path.to_string_lossy().into_owned();
    let (code, out, _) = run_binary(temp.path(), &["explain", &incident, "--config", &config_arg]);
    assert_eq!(code, 0);
    assert!(out.contains("advisory: review before use") && out.contains("receipt rcpt-7f3a"), "{out}");
    let (code, out, _) = run_binary(temp.path(), &["explain", &incident, "--config", &config_arg, "--json"]);
    assert_eq!((code, value(&out)["receipt_id"].as_str()), (0, Some("rcpt-7f3a")));
    let (_, out, _) = run_binary(temp.path(), &["status", "--config", &config_arg]);
    assert!(out.contains("BRIEF") && out.contains(&incident), "{out}");
    let (code, out, _) = run_binary(temp.path(), &["incidents", "--config", &config_arg]);
    assert_eq!(code, 0);
    assert_eq!(value(&out)["incidents"][0]["tempo_receipt_id"], "rcpt-7f3a");
}

#[test]
fn alerts_and_checks_never_wait_on_or_contact_tempo() {
    let tempo = fake_tempo(Mode::Hang);
    let temp = Temp::new();
    let config_path = tempo_config(&temp, tempo.port, 600);
    let started = Instant::now();
    let (code, _, _) = run_binary(temp.path(), &["check", "--config", &config_path.to_string_lossy()]);
    assert_eq!(code, 2);
    assert!(started.elapsed() < Duration::from_secs(30));
    assert_eq!(tempo.count.load(Ordering::SeqCst), 0, "check contacted Tempo");
    let (code, out, _) = run_binary(temp.path(), &["status", "--config", &config_path.to_string_lossy(), "--json"]);
    assert_eq!(code, 2);
    assert_eq!(value(&out)["network_contacted"], false);
    assert_eq!(tempo.count.load(Ordering::SeqCst), 0, "status contacted Tempo");
}

#[test]
fn doctor_reports_tempo_reachability_without_failing_the_core() {
    let temp = Temp::new();
    let closed = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    let config_path = tempo_config(&temp, port, 30);
    let config = dead_air::config::load_config(&config_path).unwrap();
    let runner = |argv: &[String], _t: f64, _o: &dead_air::process::RunOptions| {
        let text = if argv.iter().any(|a| a == "-filters") { "freezedetect silencedetect" } else { "fixture 1" };
        dead_air::process::ChildResult::exited(text, "", 0)
    };
    let which = |tool: &str| (tool != "yt-dlp").then(|| std::path::PathBuf::from(tool));
    let result = dead_air::doctor::doctor(&config, &runner, &which);
    assert_eq!(result["ok"], true, "the core does not depend on Tempo");
    assert!(result["warnings"].as_array().unwrap().contains(&json!("TEMPO_UNREACHABLE")));
    assert_eq!(result["tempo"]["token"], "present");
    assert!(
        result["advice"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a.as_str().unwrap().contains("Perennos Tempo did not answer"))
    );
}

#[test]
fn policy_and_mcp_config_are_secret_free_and_absolute() {
    let temp = Temp::new();
    let (code, out, _) = run_binary(temp.path(), &["tempo", "policy"]);
    assert_eq!(code, 0);
    let policy = value(&out);
    assert_eq!(policy["version"], 1);
    let tools = policy["profiles"]["deadair"]["tools"].as_object().unwrap();
    assert_eq!(tools.len(), 5);
    assert!(tools.values().all(|t| t["risk"] == "observe" && t["locality"] == "local"));
    assert!(policy.get("callers").is_none(), "callers are the owner's to add");
    let (code, out, _) = run_binary(temp.path(), &["tempo", "mcp-config", "--config", "relative.toml"]);
    assert_eq!(code, 0);
    let entry = &value(&out)["mcpServers"]["deadair"];
    let command = std::path::Path::new(entry["command"].as_str().unwrap());
    assert!(command.is_absolute() && command.exists());
    assert_eq!(entry["args"][0], "mcp");
    assert_eq!(entry["args"][1], "--config");
    assert!(std::path::Path::new(entry["args"][2].as_str().unwrap()).is_absolute());
}

fn call(server: &dead_air::mcp::Server, id: i64, method: &str, params: Value) -> Value {
    server.dispatch(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).unwrap()
}

#[test]
fn mcp_tools_are_read_only_views_of_persisted_state() {
    let temp = Temp::new();
    let config_path = tempo_config(&temp, 9, 30);
    let server = dead_air::mcp::Server::new(&config_path);
    let state = temp.join("state").join("dead-air.sqlite3");
    let reply = call(&server, 1, "tools/call", json!({"name": "deadair_status", "arguments": {}}));
    assert_eq!(reply["result"]["isError"], false);
    assert_eq!(reply["result"]["structuredContent"]["state_available"], false);
    assert!(!state.exists(), "an MCP read never creates state");
    let incident = seed_incident(&config_path);
    let before = digest(&state);
    let init = call(
        &server,
        1,
        "initialize",
        json!({"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}),
    );
    assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
    assert_eq!(init["result"]["capabilities"], json!({"tools": {"listChanged": false}}));
    assert_eq!(init["result"]["serverInfo"]["name"], "dead-air");
    assert_eq!(
        call(&server, 2, "initialize", json!({"protocolVersion": "1999-01-01"}))["result"]["protocolVersion"],
        "2025-06-18"
    );
    assert!(server.dispatch(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).is_none());
    assert_eq!(call(&server, 3, "ping", json!({}))["result"], json!({}));
    let tools = call(&server, 4, "tools/list", json!({}));
    let tools = tools["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 5);
    for tool in tools {
        let name = tool["name"].as_str().unwrap();
        assert!(
            name.starts_with("deadair_")
                && name.len() <= 64
                && name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
        );
        assert!((1..=400).contains(&tool["description"].as_str().unwrap().len()));
        assert_eq!(tool["inputSchema"]["additionalProperties"], false);
        assert_eq!(tool["annotations"]["readOnlyHint"], true);
        assert_eq!(tool["annotations"]["destructiveHint"], false);
        assert_eq!(tool["annotations"]["openWorldHint"], false);
    }
    let status = call(&server, 5, "tools/call", json!({"name": "deadair_status"}));
    assert_eq!(status["result"]["structuredContent"]["open_incidents"][0]["incident_id"], incident.as_str());
    let text: Value = serde_json::from_str(status["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text, status["result"]["structuredContent"]);
    let incidents =
        call(&server, 6, "tools/call", json!({"name": "deadair_incidents", "arguments": {"state": "all", "limit": 5}}));
    assert_eq!(incidents["result"]["structuredContent"]["incidents"][0]["condition"], "audio_silence");
    let detail =
        call(&server, 7, "tools/call", json!({"name": "deadair_incident", "arguments": {"incident_id": incident}}));
    assert_eq!(detail["result"]["structuredContent"]["source_label"], "Sample channel");
    assert_eq!(detail["result"]["structuredContent"]["transitions"][0]["reason_codes"], json!(["AUDIO_SILENCE"]));
    let deliveries =
        call(&server, 8, "tools/call", json!({"name": "deadair_deliveries", "arguments": {"status": "pending"}}));
    assert_eq!(deliveries["result"]["structuredContent"]["deliveries"][0]["destination_id"], "ops-hook");
    let sources = call(&server, 9, "tools/call", json!({"name": "deadair_sources", "arguments": {}}));
    let everything =
        [status, incidents, detail, deliveries, sources.clone()].iter().map(|v| v.to_string()).collect::<String>();
    for forbidden in ["example.invalid", "https://", "SECRET_HOOK_URL", TOKEN, "endpoint_env"] {
        assert!(!everything.contains(forbidden), "{forbidden} leaked through MCP");
    }
    assert_eq!(sources["result"]["structuredContent"]["sources"][0]["kind"], "direct");
    let missing =
        call(&server, 10, "tools/call", json!({"name": "deadair_incident", "arguments": {"incident_id": "nope"}}));
    assert_eq!(missing["result"]["isError"], true);
    let bad_args = call(&server, 11, "tools/call", json!({"name": "deadair_incidents", "arguments": {"limit": 1000}}));
    assert_eq!(bad_args["result"]["isError"], true);
    let extra = call(&server, 12, "tools/call", json!({"name": "deadair_status", "arguments": {"x": 1}}));
    assert_eq!(extra["result"]["isError"], true);
    assert_eq!(call(&server, 13, "tools/call", json!({"name": "deadair_delete"}))["error"]["code"], -32602);
    assert_eq!(call(&server, 14, "resources/list", json!({}))["error"]["code"], -32601);
    assert_eq!(call(&server, 15, "tools/call", json!(["x"]))["error"]["code"], -32602);
    assert_eq!(server.dispatch(&json!({"id": 16, "method": "ping"})).unwrap()["error"]["code"], -32600);
    assert_eq!(digest(&state), before, "MCP never writes state");
}

#[test]
fn mcp_stdio_round_trip_handles_parse_errors_and_oversize_lines() {
    let temp = Temp::new();
    let config_path = tempo_config(&temp, 9, 30);
    let mut child = Command::new(binary())
        .args(["mcp", "--config", &config_path.to_string_lossy()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut read = || {
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        value(&line)
    };
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2025-06-18"}}}}"#)
        .unwrap();
    assert_eq!(read()["result"]["protocolVersion"], "2025-06-18");
    writeln!(stdin, r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#).unwrap();
    writeln!(stdin, "{{not json").unwrap();
    assert_eq!(read()["error"]["code"], -32700);
    let huge = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\",\"params\":{{\"pad\":\"{}\"}}}}",
        "x".repeat(1_100_000)
    );
    writeln!(stdin, "{huge}").unwrap();
    assert_eq!(read()["error"]["code"], -32700);
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"deadair_sources","arguments":{{}}}}}}"#
    )
    .unwrap();
    let sources = read();
    assert_eq!(sources["id"], 3);
    assert_eq!(sources["result"]["structuredContent"]["sources"][0]["label"], "Sample channel");
    drop(stdin);
    assert_eq!(child.wait().unwrap().code(), Some(0));
    let mut stderr = String::new();
    child.stderr.take().unwrap().read_to_string(&mut stderr).unwrap();
    assert!(stderr.contains("read-only"));
}
