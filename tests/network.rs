//! Native loopback acceptance (Python `test_network.py`): HLS incidents, HTTPS alert retry with
//! a stable event id, HTTP limits and redirects, FFmpeg TLS verification, and sender deadlines.
//! Only owned loopback endpoints are used. Needs FFmpeg with libx264/AAC for the HLS fixtures.

mod common;

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use common::*;
use dead_air::alerts::{ChildLauncher, DispatchOptions, dispatch_due, send_destination_bounded};
use dead_air::http::{GetOptions, bounded_http_get};
use dead_air::model::*;
use dead_air::netpolicy::Resolver;
use dead_air::persistence::IncidentStore;
use dead_air::process::{RunOptions, run_bounded};
use serde_json::Value;

const MAX_REQUESTS: usize = 128;

/// Routed fixture files: path -> (body, content type).
type Files = HashMap<String, (Vec<u8>, &'static str)>;

struct Ca {
    pem_path: PathBuf,
    server: Arc<rustls::ServerConfig>,
}

fn test_ca(dir: &Path) -> Ca {
    use rcgen::{
        BasicConstraints, CertificateParams, CertifiedIssuer, DnType, IsCa, KeyPair, KeyUsagePurpose, SanType,
    };
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.distinguished_name.push(DnType::CommonName, "dead-air loopback test CA");
    ca_params.key_usages =
        vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign, KeyUsagePurpose::DigitalSignature];
    let ca = CertifiedIssuer::self_signed(ca_params, KeyPair::generate().unwrap()).unwrap();
    let mut leaf_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    leaf_params.distinguished_name.push(DnType::CommonName, "dead-air-loopback-test");
    leaf_params.subject_alt_names = vec![SanType::IpAddress(IpAddr::V4(Ipv4Addr::LOCALHOST))];
    let leaf_key = KeyPair::generate().unwrap();
    let leaf = leaf_params.signed_by(&leaf_key, &ca).unwrap();
    let pem_path = dir.join("loopback-test-ca.pem");
    std::fs::write(&pem_path, ca.pem()).unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let server = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![leaf.der().clone(), ca.der().clone()],
            rustls::pki_types::PrivateKeyDer::Pkcs8(leaf_key.serialize_der().into()),
        )
        .unwrap();
    Ca { pem_path, server: Arc::new(server) }
}

struct Reply {
    status: u16,
    body: Vec<u8>,
    content_type: &'static str,
    headers: Vec<(String, String)>,
    /// Send this prefix, wait for stop (up to the duration), then the rest.
    stall: Option<(usize, Duration)>,
}

fn reply(status: u16, body: &[u8], content_type: &'static str) -> Reply {
    Reply { status, body: body.to_vec(), content_type, headers: vec![], stall: None }
}

#[derive(Default)]
struct ServerState {
    files: Files,
    playlists: HashMap<String, Vec<u8>>,
    mode: Mutex<String>,
    cross_target: Mutex<String>,
    collector: bool,
    collector_slow: AtomicBool,
    received: Mutex<Vec<(String, u16)>>,
    requests: AtomicUsize,
}

struct OwnedServer {
    port: u16,
    state: Arc<ServerState>,
    stop: Arc<AtomicBool>,
    accept: Option<std::thread::JoinHandle<()>>,
}

impl Drop for OwnedServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(thread) = self.accept.take() {
            let _ = thread.join();
        }
    }
}

fn route(state: &ServerState, method: &str, path: &str, body: &[u8]) -> Reply {
    if state.requests.fetch_add(1, Ordering::SeqCst) >= MAX_REQUESTS {
        return reply(429, b"", "text/plain");
    }
    if state.collector {
        if method != "POST" || path != "/alert" {
            return reply(404, b"", "text/plain");
        }
        let Ok(payload) = serde_json::from_slice::<Value>(body) else {
            return reply(400, b"", "text/plain");
        };
        let event_id = payload["event_id"].as_str().unwrap_or("").to_string();
        if event_id.len() != 32 || !event_id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return reply(400, b"", "text/plain");
        }
        if state.collector_slow.load(Ordering::SeqCst) {
            state.received.lock().unwrap().push((event_id, 204));
            return Reply { stall: Some((0, Duration::from_secs(6))), ..reply(204, b"", "text/plain") };
        }
        let mut received = state.received.lock().unwrap();
        let status = if received.is_empty() { 503 } else { 204 };
        received.push((event_id, status));
        return reply(status, b"", "text/plain");
    }
    let redirect =
        |location: &str| Reply { headers: vec![("Location".into(), location.into())], ..reply(302, b"", "text/plain") };
    match path {
        "/redirect" => redirect("/small"),
        "/loop-a" => redirect("/loop-b"),
        "/loop-b" => redirect("/loop-a"),
        "/cross" => redirect(&state.cross_target.lock().unwrap().clone()),
        "/small" => reply(200, b"ok", "text/plain; charset=utf-8"),
        "/large" => reply(200, &[b'x'; 8192], "text/plain"),
        "/slow-body" => Reply {
            stall: Some((9, Duration::from_secs(2))),
            ..reply(200, b"slow-body-late", "application/octet-stream")
        },
        "/slow.m3u8" => Reply {
            stall: Some((8, Duration::from_secs(2))),
            ..reply(200, b"#EXTM3U\n-late", "application/octet-stream")
        },
        "/endlist.m3u8" => {
            reply(200, b"#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-ENDLIST\n", "application/vnd.apple.mpegurl")
        }
        "/offline" => reply(410, b"offline", "text/plain"),
        "/missing" => reply(404, b"missing", "text/plain"),
        "/corrupt.ts" => reply(200, b"not a transport stream", "video/mp2t"),
        "/current/index.m3u8" => match state.playlists.get(&*state.mode.lock().unwrap()) {
            Some(body) => reply(200, body, "application/vnd.apple.mpegurl"),
            None => reply(404, b"", "text/plain"),
        },
        other => match state.files.get(other) {
            Some((body, content_type)) => reply(200, body, content_type),
            None => reply(404, b"", "text/plain"),
        },
    }
}

fn serve_connection(mut stream: impl Read + Write, state: &ServerState, stop: &AtomicBool) {
    let mut data = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => data.extend_from_slice(&chunk[..n]),
        }
        if data.len() > 65_536 {
            return;
        }
    };
    let head = String::from_utf8_lossy(&data[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split(' ');
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let path = target.split('?').next().unwrap_or("/").to_string();
    let length: usize = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse().ok())
        .unwrap_or(0);
    let mut body = data[head_end + 4..].to_vec();
    while body.len() < length.min(65_536) {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => body.extend_from_slice(&chunk[..n]),
        }
    }
    let answer = route(state, &method, &path, &body);
    let declared = answer.body.len() + if answer.stall.is_some() && answer.status == 200 { 16 } else { 0 };
    let mut out = format!(
        "HTTP/1.0 {} X\r\nContent-Type: {}\r\nContent-Length: {declared}\r\nConnection: close\r\n",
        answer.status, answer.content_type
    );
    for (name, value) in &answer.headers {
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    out.push_str("\r\n");
    let mut bytes = out.into_bytes();
    let split = answer.stall.map(|(n, _)| n.min(answer.body.len())).unwrap_or(answer.body.len());
    bytes.extend_from_slice(&answer.body[..split]);
    if let Some((_, wait)) = answer.stall {
        if answer.status != 200 {
            // Hold the reply back entirely (slow collector).
            let end = Instant::now() + wait;
            while Instant::now() < end && !stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = stream.write_all(&bytes);
            let _ = stream.flush();
            return;
        }
        let _ = stream.write_all(&bytes);
        let _ = stream.flush();
        let end = Instant::now() + wait;
        while Instant::now() < end && !stop.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = stream.write_all(&answer.body[split..]);
        let _ = stream.flush();
        return;
    }
    let _ = stream.write_all(&bytes);
    let _ = stream.flush();
}

fn own_server(state: ServerState, tls: Option<Arc<rustls::ServerConfig>>) -> OwnedServer {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let state = Arc::new(state);
    let stop = Arc::new(AtomicBool::new(false));
    let (thread_state, thread_stop) = (Arc::clone(&state), Arc::clone(&stop));
    let accept = std::thread::spawn(move || {
        let mut handlers = Vec::new();
        for connection in listener.incoming() {
            if thread_stop.load(Ordering::SeqCst) {
                break;
            }
            let Ok(socket) = connection else { continue };
            let _ = socket.set_read_timeout(Some(Duration::from_secs(5)));
            let (state, stop, tls) = (Arc::clone(&thread_state), Arc::clone(&thread_stop), tls.clone());
            handlers.push(std::thread::spawn(move || match tls {
                Some(config) => {
                    let Ok(connection) = rustls::ServerConnection::new(config) else { return };
                    let stream = rustls::StreamOwned::new(connection, socket);
                    serve_connection(stream, &state, &stop);
                }
                None => serve_connection(socket, &state, &stop),
            }));
        }
        for handler in handlers {
            let _ = handler.join();
        }
    });
    OwnedServer { port, state, stop, accept: Some(accept) }
}

struct Fixtures {
    _temp: Temp,
    root: PathBuf,
    ffmpeg: String,
    ffprobe: String,
    files: Files,
    playlists: HashMap<String, Vec<u8>>,
}

fn generate_hls(ffmpeg: &str, root: &Path, name: &str, video: &str, audio: &str) -> (Files, Vec<u8>) {
    let directory = root.join(name);
    std::fs::create_dir_all(&directory).unwrap();
    let playlist = directory.join("index.m3u8");
    let pattern = directory.join("segment-%02d.ts");
    let argv: Vec<String> = [
        ffmpeg,
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        "-y",
        "-threads",
        "1",
        "-filter_threads",
        "1",
        "-filter_complex_threads",
        "1",
        "-f",
        "lavfi",
        "-i",
        video,
        "-f",
        "lavfi",
        "-i",
        audio,
        "-t",
        "12",
        "-map",
        "0:v:0",
        "-map",
        "1:a:0",
        "-c:v",
        "libx264",
        "-preset",
        "ultrafast",
        "-pix_fmt",
        "yuv420p",
        "-threads",
        "1",
        "-g",
        "20",
        "-keyint_min",
        "20",
        "-sc_threshold",
        "0",
        "-c:a",
        "aac",
        "-ar",
        "48000",
        "-ac",
        "2",
        "-b:a",
        "64k",
        "-f",
        "hls",
        "-hls_time",
        "2",
        "-hls_list_size",
        "0",
        "-hls_flags",
        "omit_endlist",
        "-hls_segment_filename",
        &pattern.to_string_lossy(),
        &playlist.to_string_lossy(),
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let result = run_bounded(
        &argv,
        60.0,
        &RunOptions { max_output_bytes: 8192, file_limits: vec![(playlist.clone(), 4 * 1024 * 1024)] },
    );
    assert!(
        result.succeeded() && !result.bounded_failure(),
        "HLS fixture generation failed for {name}; verify FFmpeg HLS, libx264 and AAC: {result:?}"
    );
    let raw = std::fs::read_to_string(&playlist).unwrap();
    assert!(!raw.contains("#EXT-X-ENDLIST"), "a live-style fixture must not end");
    let routed: String = raw
        .lines()
        .map(|line| {
            if !line.is_empty() && !line.starts_with('#') {
                format!("/{name}/{}", Path::new(line).file_name().unwrap().to_string_lossy())
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let mut files = HashMap::new();
    for entry in std::fs::read_dir(&directory).unwrap() {
        let path = entry.unwrap().path();
        let file = path.file_name().unwrap().to_string_lossy().into_owned();
        if file.ends_with(".ts") {
            files.insert(format!("/{name}/{file}"), (std::fs::read(&path).unwrap(), "video/mp2t"));
        }
    }
    assert!((1..=6).contains(&files.len()));
    files.insert(format!("/{name}/index.m3u8"), (routed.clone().into_bytes(), "application/vnd.apple.mpegurl"));
    (files, routed.into_bytes())
}

fn fixtures() -> Option<&'static Fixtures> {
    static FIXTURES: OnceLock<Option<Fixtures>> = OnceLock::new();
    FIXTURES
        .get_or_init(|| {
            let (ffmpeg, ffprobe) = media_tools()?;
            let temp = Temp::new();
            let root = temp.path().to_path_buf();
            let mut files = HashMap::new();
            let mut playlists = HashMap::new();
            for (name, video, audio) in [
                ("normal", "testsrc2=size=160x90:rate=10", "sine=frequency=1000:sample_rate=48000"),
                ("bad", "color=c=black:size=160x90:rate=10", "anullsrc=r=48000:cl=stereo"),
            ] {
                let (set, playlist) = generate_hls(&ffmpeg, &root, name, video, audio);
                files.extend(set);
                playlists.insert(name.to_string(), playlist);
            }
            let total: usize = files.values().map(|(b, _)| b.len()).sum();
            assert!(total <= 4 * 1024 * 1024, "combined HLS fixtures exceed 4 MiB");
            Some(Fixtures { _temp: temp, root, ffmpeg, ffprobe, files, playlists })
        })
        .as_ref()
}

fn base_network(ports: &[u16]) -> NetworkConfig {
    NetworkConfig {
        private_allowlist: ports.iter().map(|p| format!("127.0.0.1:{p}")).collect(),
        max_redirects: 3,
        thumbnail_max_bytes: 1_048_576,
    }
}

fn direct_config(fixtures: &Fixtures, url: &str, network: NetworkConfig, state: &Path) -> AppConfig {
    let mut config = dead_air::config::parse_config_text(dead_air::config::EXAMPLE_TOML).unwrap();
    config.watch.sample_s = 3.0;
    config.watch.resolver_timeout_s = 0.25;
    config.watch.capture_timeout_s = 1.5;
    config.watch.analysis_timeout_s = 1.5;
    config.watch.cycle_budget_s = 5.0;
    config.watch.max_sample_bytes = 16_000_000;
    config.watch.max_output_bytes = 262_144;
    config.watch.state_path = state.to_string_lossy().into_owned();
    config.tools.ffmpeg = fixtures.ffmpeg.clone();
    config.tools.ffprobe = fixtures.ffprobe.clone();
    config.network = network;
    let stream = &mut config.streams[0];
    stream.url = url.into();
    stream.allow_private = true;
    stream.thumbnail_enabled = false;
    stream.offline_http_statuses = vec![410];
    config
}

fn observe(config: &AppConfig, deadline: f64) -> Observation {
    let resolver = Resolver::System;
    let runner = run_bounded;
    let transport = bounded_http_get;
    let deps = dead_air::observe::ObserveDeps {
        runner: &runner,
        resolver: &resolver,
        transport: &transport,
        monotonic: &now_mono,
    };
    dead_air::observe::observe_stream(config, &config.streams[0], &deps, None, Some(deadline))
}

fn event(id: &str) -> Value {
    serde_json::json!({"event_id": id, "transition": "open", "stream_id": "acceptance", "condition": "audio_silence"})
}

#[test]
fn a_real_hls_incident_https_retry_and_recovery() {
    let Some(fixtures) = fixtures() else { return };
    let temp = Temp::new();
    let ca = test_ca(temp.path());
    let media = own_server(
        ServerState {
            files: fixtures.files.clone(),
            playlists: fixtures.playlists.clone(),
            mode: Mutex::new("normal".into()),
            ..Default::default()
        },
        None,
    );
    let collector = own_server(ServerState { collector: true, ..Default::default() }, Some(Arc::clone(&ca.server)));
    let network = base_network(&[media.port, collector.port]);
    let endpoint = format!("https://127.0.0.1:{}/alert", collector.port);
    let destination = DestinationConfig {
        allow_private: true,
        ..DestinationConfig::webhook("loopback-alert", "webhook", "DEAD_AIR_TEST_ENDPOINT")
    };
    let env = |name: &str| (name == "DEAD_AIR_TEST_ENDPOINT").then(|| endpoint.clone());

    let untrusted = ChildLauncher { exe: binary(), extra_env: vec![] };
    let negative =
        send_destination_bounded(&destination, &event(&"0".repeat(32)), &network, 3.0, &env, &now_mono, &untrusted);
    assert!(!negative.ok, "self-signed loopback TLS unexpectedly succeeded without test trust");
    assert!(collector.state.received.lock().unwrap().is_empty(), "untrusted TLS reached the collector");

    let trusted = ChildLauncher {
        exe: binary(),
        extra_env: vec![("SSL_CERT_FILE".into(), ca.pem_path.to_string_lossy().into_owned())],
    };
    let state_path = temp.join("incidents.sqlite");
    let mut config =
        direct_config(fixtures, &format!("http://127.0.0.1:{}/current/index.m3u8", media.port), network, &state_path);
    config.watch.resolver_timeout_s = 1.0;
    config.watch.capture_timeout_s = 10.0;
    config.watch.analysis_timeout_s = 10.0;
    config.watch.cycle_budget_s = 25.0;
    config.destinations = vec![destination.clone()];
    {
        let stream = &mut config.streams[0];
        stream.freeze_window_s = 1.0;
        stream.silence_window_s = 1.0;
        stream.min_bad_samples = 2;
        stream.unhealthy_after_s = 1.0;
        stream.recovery_samples = 2;
        stream.max_gap_s = 60.0;
    }
    let mut store = IncidentStore::open(&state_path, 90).unwrap();
    let mut observations = Vec::new();
    let timestamps = [(100.0, 10.0), (101.0, 11.0), (131.0, 41.0), (132.0, 42.0), (133.0, 43.0)];
    for (mode, (wall, mono)) in ["bad", "bad", "bad", "normal", "normal"].into_iter().zip(timestamps) {
        *media.state.mode.lock().unwrap() = mode.into();
        let mut observer = dead_air::observe::default_observer();
        let batch = dead_air::observe::check_cycle(&config, &mut observer, &now_mono, &mut HashMap::new());
        let diagnostic = dead_air::model::observation_json(&batch[0]).to_string();
        if mode == "bad" {
            assert_eq!(batch[0].status, "UNHEALTHY", "{diagnostic}");
            assert!(batch[0].video.has_reason("VIDEO_FREEZE"), "{diagnostic}");
            assert!(batch[0].audio.has_reason("AUDIO_SILENCE"), "{diagnostic}");
        } else {
            assert_eq!(
                (batch[0].status.as_str(), batch[0].video.status.as_str(), batch[0].audio.status.as_str()),
                ("HEALTHY", "HEALTHY", "HEALTHY"),
                "{diagnostic}"
            );
        }
        observations.push(batch[0].clone());
        store.record_cycle(&config, &batch, wall, mono).unwrap();
        let options =
            DispatchOptions { now_wall: Some(wall), env: &env, launcher: Some(&trusted), ..DispatchOptions::default() };
        dispatch_due(&mut store, &config.destinations, &config.network, options).unwrap();
        let open: Vec<String> = store.open_incidents().unwrap().into_iter().map(|r| r.1).collect();
        match observations.len() {
            1 => assert!(open.is_empty()),
            2 => {
                assert_eq!(open.len(), 2);
                let pending: Vec<_> = store.deliveries().unwrap().into_iter().filter(|r| r.2 == "pending").collect();
                assert_eq!(pending.len(), 1);
                assert!(pending[0].4.is_some());
            }
            3 => assert_eq!(store.events().unwrap().len(), 2),
            4 => assert_eq!(open.len(), 2),
            _ => assert!(open.is_empty()),
        }
    }
    let events = store.events().unwrap();
    assert_eq!(events.len(), 4);
    let by_key: HashMap<(String, String), Value> =
        events.iter().map(|e| ((e.1.clone(), e.2.clone()), serde_json::from_str(&e.3).unwrap())).collect();
    for (condition, dimension) in [("audio_silence", "audio"), ("video_freeze", "video")] {
        let source = if dimension == "audio" { &observations[1].audio } else { &observations[1].video };
        let opened = &by_key[&(condition.to_string(), "open".to_string())]["evidence"];
        assert_eq!(opened["threshold"]["sample_count"], 2);
        assert_eq!(opened["threshold"]["required_samples"], 2);
        assert!((opened["threshold"]["span_s"].as_f64().unwrap() - 1.0).abs() < 1e-9);
        let metrics = &opened["affected_dimensions"][dimension];
        assert_eq!(metrics["status"], source.status.as_str());
        assert_eq!(metrics["sampled_s"], source.sampled_s);
        assert_eq!(metrics["decoded_s"].as_f64(), source.decoded_s.map(dead_air::pyjson::round3));
        let coverage = source.coverage.as_ref().unwrap();
        let key = if dimension == "video" { "frame_count" } else { "sample_count" };
        let expected = if dimension == "video" { coverage.frame_count } else { coverage.sample_count };
        assert_eq!(metrics["coverage"][key].as_u64(), Some(expected));
        let recovered = &by_key[&(condition.to_string(), "recovery".to_string())]["evidence"];
        assert_eq!(recovered["threshold"]["sample_count"], 2);
        assert_eq!(recovered["samples"].as_array().unwrap().len(), 2);
    }
    let deliveries = store.deliveries().unwrap();
    assert_eq!(deliveries.len(), 4);
    assert!(deliveries.iter().all(|r| r.2 == "sent" && r.5 == Some(204)));
    let posts = collector.state.received.lock().unwrap().clone();
    assert_eq!(posts.len(), 5);
    assert_eq!(posts[0].1, 503);
    assert_eq!(posts[1..].iter().map(|p| p.1).collect::<Vec<_>>(), vec![204; 4]);
    assert_eq!(posts.iter().filter(|p| p.0 == posts[0].0).count(), 2, "the retry kept the stable event id");
    drop(store);
    assert_eq!(std::fs::read(state_path.with_file_name(".dead-air.lock")).unwrap(), dead_air::constants::LEGACY_GUARD);
    let reopened = IncidentStore::open(&state_path, 90).unwrap();
    assert_eq!((reopened.events().unwrap().len(), reopened.deliveries().unwrap().len()), (4, 4));
    assert!(reopened.open_incidents().unwrap().is_empty());
}

#[test]
fn http_limits_redirects_off_air_and_unknown() {
    let Some(fixtures) = fixtures() else { return };
    let temp = Temp::new();
    let media = own_server(ServerState { files: fixtures.files.clone(), ..Default::default() }, None);
    let second = own_server(ServerState::default(), None);
    *media.state.cross_target.lock().unwrap() = format!("http://127.0.0.1:{}/small", second.port);
    let network = base_network(&[media.port]);
    let root = format!("http://127.0.0.1:{}", media.port);
    let resolver = Resolver::System;
    let get = |path: &str, allow_private: bool, timeout_s: f64| {
        bounded_http_get(
            &format!("{root}{path}"),
            &GetOptions {
                source: "direct",
                network: &network,
                allow_private,
                timeout_s,
                max_bytes: 256,
                resolver: &resolver,
                range_request: true,
                deadline: None,
            },
        )
    };
    let redirected = get("/redirect", true, 1.0);
    assert_eq!((redirected.status, redirected.body.as_slice()), (Some(200), b"ok".as_slice()));
    assert_eq!(get("/loop-a", true, 1.0).error_code.as_deref(), Some("REDIRECT_LIMIT"));
    let before = second.state.requests.load(Ordering::SeqCst);
    assert_eq!(get("/cross", true, 1.0).status, None);
    assert_eq!(
        second.state.requests.load(Ordering::SeqCst),
        before,
        "a cross-port redirect reached the unallowlisted server"
    );
    let large = get("/large", true, 1.0);
    assert!(large.truncated && large.body.len() <= 256);
    let before = media.state.requests.load(Ordering::SeqCst);
    assert_eq!(get("/small", false, 1.0).status, None);
    assert_eq!(media.state.requests.load(Ordering::SeqCst), before, "default private denial made a loopback request");
    let started = Instant::now();
    let slow = get("/slow-body", true, 0.25);
    assert!(started.elapsed() <= Duration::from_millis(1250), "bounded slow body exceeded its margin");
    assert!(
        slow.error_code.as_deref() == Some("HTTP_TIMEOUT")
            || slow.truncated
            || slow.body.len() < b"slow-body-late".len()
    );
    for (route, expected, reason) in [
        ("endlist.m3u8", "OFF_AIR", Some("DIRECT_HLS_ENDLIST")),
        ("offline", "OFF_AIR", Some("DIRECT_OFFLINE_STATUS")),
        ("missing", "UNKNOWN", None),
        ("slow.m3u8", "UNKNOWN", None),
        ("corrupt.ts", "UNKNOWN", None),
    ] {
        let config = direct_config(fixtures, &format!("{root}/{route}"), network.clone(), &temp.join("unused.sqlite"));
        let started = Instant::now();
        let result = observe(&config, now_mono() + 5.0);
        assert!(started.elapsed() <= Duration::from_secs(6), "{route} exceeded its scheduling margin");
        assert_eq!(result.status, expected, "{route}: {result:?}");
        match reason {
            Some(reason) => assert!(result.reason_codes.iter().any(|r| r == reason)),
            None => assert!(
                !result.reason_codes.iter().any(|r| r.starts_with("DIRECT_HLS") || r == "DIRECT_OFFLINE_STATUS")
            ),
        }
    }
    assert!(media.state.requests.load(Ordering::SeqCst) <= MAX_REQUESTS);
}

#[test]
fn https_media_capture_rejects_an_untrusted_certificate() {
    let Some(fixtures) = fixtures() else { return };
    let temp = Temp::new();
    let ca = test_ca(temp.path());
    let media = own_server(ServerState { files: fixtures.files.clone(), ..Default::default() }, Some(ca.server));
    let url = format!("https://127.0.0.1:{}/normal/index.m3u8", media.port);
    let config = direct_config(fixtures, &url, base_network(&[media.port]), &temp.join("unused.sqlite"));
    let output = fixtures.root.join(format!("capture-{}.mkv", dead_air::random::uuid4_hex()));
    let media_input = ResolvedMedia { inputs: vec![MediaInput { kind: "muxed".into(), url }] };
    let argv = dead_air::media::build_capture_argv(&media_input, &config.streams[0], &config, &output).unwrap();
    let result = run_bounded(&argv, 3.0, &RunOptions::output(8192));
    assert_ne!(result.returncode, Some(0), "FFmpeg accepted an untrusted TLS certificate");
    let message = result.stderr.to_lowercase();
    // GnuTLS (Debian/Ubuntu) names the certificate; other TLS backends word it differently.
    assert!(
        ["certificate", "verif", "tls", "ssl", "handshake"].iter().any(|w| message.contains(w)),
        "{}",
        result.stderr
    );
    assert!(result.cleanup_complete);
}

#[test]
fn https_and_smtp_children_stop_at_deadline() {
    let temp = Temp::new();
    let ca = test_ca(temp.path());
    let collector = own_server(ServerState { collector: true, ..Default::default() }, Some(ca.server));
    collector.state.collector_slow.store(true, Ordering::SeqCst);
    let smtp_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let smtp_port = smtp_listener.local_addr().unwrap().port();
    let accepted = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&accepted);
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for socket in smtp_listener.incoming().flatten() {
            counter.fetch_add(1, Ordering::SeqCst);
            held.push(socket); // accept and never greet
        }
    });
    let network = base_network(&[collector.port, smtp_port]);
    let launcher = ChildLauncher {
        exe: binary(),
        extra_env: vec![("SSL_CERT_FILE".into(), ca.pem_path.to_string_lossy().into_owned())],
    };
    let endpoint = format!("https://127.0.0.1:{}/alert", collector.port);
    let env = |name: &str| (name == "DEAD_AIR_SLOW_ENDPOINT").then(|| endpoint.clone());
    let webhook = DestinationConfig {
        allow_private: true,
        ..DestinationConfig::webhook("slow-loopback-webhook", "webhook", "DEAD_AIR_SLOW_ENDPOINT")
    };
    let id = "1".repeat(32);
    let started = Instant::now();
    let response = send_destination_bounded(&webhook, &event(&id), &network, 3.0, &env, &now_mono, &launcher);
    assert!(!response.ok, "a stalled HTTPS collector unexpectedly succeeded");
    assert!(started.elapsed() <= Duration::from_secs(4), "HTTPS sender exceeded its scheduling margin");
    assert_eq!(
        collector.state.received.lock().unwrap().iter().map(|r| r.0.clone()).collect::<Vec<_>>(),
        vec![id.clone()]
    );

    let mail = DestinationConfig {
        id: "greeting-hang-smtp".into(),
        kind: "smtp".into(),
        host: Some("127.0.0.1".into()),
        port: Some(smtp_port),
        tls: Some("starttls".into()),
        sender: Some("dead-air@example.test".into()),
        recipients: vec!["ops@example.test".into()],
        allow_private: true,
        ..Default::default()
    };
    let started = Instant::now();
    let response = send_destination_bounded(&mail, &event(&id), &network, 3.0, &|_| None, &now_mono, &launcher);
    assert!(!response.ok, "an SMTP greeting hang unexpectedly succeeded");
    assert!(started.elapsed() <= Duration::from_secs(4), "SMTP sender exceeded its scheduling margin");
    assert_eq!(accepted.load(Ordering::SeqCst), 1, "the SMTP child did not reach the owned listener");
}

#[test]
fn smtp_over_implicit_tls_delivers_to_a_verified_loopback_server() {
    // A minimal SMTP server over TLS that accepts one message.
    let temp = Temp::new();
    let ca = test_ca(temp.path());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let config = Arc::clone(&ca.server);
    let server = std::thread::spawn(move || {
        let (socket, _) = listener.accept().unwrap();
        socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut tls = rustls::StreamOwned::new(rustls::ServerConnection::new(config).unwrap(), socket);
        let mut transcript = String::new();
        let line = |tls: &mut rustls::StreamOwned<rustls::ServerConnection, TcpStream>| -> String {
            let mut out = Vec::new();
            let mut byte = [0u8; 1];
            while tls.read(&mut byte).map(|n| n == 1).unwrap_or(false) {
                out.push(byte[0]);
                if byte[0] == b'\n' {
                    break;
                }
            }
            String::from_utf8_lossy(&out).into_owned()
        };
        tls.write_all(b"220 loopback ESMTP\r\n").unwrap();
        loop {
            let command = line(&mut tls);
            transcript.push_str(&command);
            let upper = command.to_ascii_uppercase();
            if upper.starts_with("EHLO") {
                tls.write_all(b"250-loopback\r\n250 AUTH PLAIN\r\n").unwrap();
            } else if upper.starts_with("AUTH PLAIN") {
                tls.write_all(b"235 ok\r\n").unwrap();
            } else if upper.starts_with("MAIL") || upper.starts_with("RCPT") {
                tls.write_all(b"250 ok\r\n").unwrap();
            } else if upper.starts_with("DATA") {
                tls.write_all(b"354 go\r\n").unwrap();
                loop {
                    let data = line(&mut tls);
                    transcript.push_str(&data);
                    if data == ".\r\n" || data.is_empty() {
                        break;
                    }
                }
                tls.write_all(b"250 queued\r\n").unwrap();
            } else if upper.starts_with("QUIT") || command.is_empty() {
                let _ = tls.write_all(b"221 bye\r\n");
                break;
            } else {
                tls.write_all(b"500 unknown\r\n").unwrap();
            }
        }
        transcript
    });
    let network = base_network(&[port]);
    let launcher = ChildLauncher {
        exe: binary(),
        extra_env: vec![("SSL_CERT_FILE".into(), ca.pem_path.to_string_lossy().into_owned())],
    };
    let mail = DestinationConfig {
        id: "mail".into(),
        kind: "smtp".into(),
        host: Some("127.0.0.1".into()),
        port: Some(port),
        tls: Some("ssl".into()),
        sender: Some("dead-air@example.test".into()),
        recipients: vec!["ops@example.test".into()],
        username_env: Some("SMTP_USER".into()),
        password_env: Some("SMTP_PASSWORD".into()),
        allow_private: true,
        ..Default::default()
    };
    let env = |name: &str| match name {
        "SMTP_USER" => Some("user".to_string()),
        "SMTP_PASSWORD" => Some("secret-password".to_string()),
        _ => None,
    };
    let id = "2".repeat(32);
    let response = send_destination_bounded(&mail, &event(&id), &network, 5.0, &env, &now_mono, &launcher);
    assert!(response.ok, "{response:?}");
    let transcript = server.join().unwrap();
    assert!(transcript.contains("MAIL FROM:<dead-air@example.test>"));
    assert!(transcript.contains("RCPT TO:<ops@example.test>"));
    assert!(transcript.contains(&id), "the message carries the stable event id");
    assert!(!transcript.contains("secret-password"), "credentials only travel base64-encoded inside TLS");
}

#[test]
fn https_alerts_verify_the_hostname_not_just_the_chain() {
    let temp = Temp::new();
    let ca = test_ca(temp.path());
    let collector = own_server(ServerState { collector: true, ..Default::default() }, Some(ca.server));
    let network = NetworkConfig {
        private_allowlist: vec![format!("localhost:{}", collector.port), format!("127.0.0.1:{}", collector.port)],
        max_redirects: 0,
        thumbnail_max_bytes: 1024,
    };
    let launcher = ChildLauncher {
        exe: binary(),
        extra_env: vec![("SSL_CERT_FILE".into(), ca.pem_path.to_string_lossy().into_owned())],
    };
    let hook =
        DestinationConfig { allow_private: true, ..DestinationConfig::webhook("named-host", "webhook", "HOOK_URL") };
    // The certificate names 127.0.0.1 only; `localhost` reaches the same socket but must fail.
    let named = format!("https://localhost:{}/alert", collector.port);
    let env = |name: &str| (name == "HOOK_URL").then(|| named.clone());
    let outcome = send_destination_bounded(&hook, &event(&"3".repeat(32)), &network, 4.0, &env, &now_mono, &launcher);
    assert!(!outcome.ok, "a certificate for 127.0.0.1 was accepted for localhost: {outcome:?}");
    assert_eq!(
        outcome.reason.as_deref(),
        Some("HTTP_REQUEST_FAILED"),
        "the name resolved and TLS refused it: {outcome:?}"
    );
    assert!(collector.state.received.lock().unwrap().is_empty(), "the request body reached the collector");
    let exact = format!("https://127.0.0.1:{}/alert", collector.port);
    let env = |name: &str| (name == "HOOK_URL").then(|| exact.clone());
    let outcome = send_destination_bounded(&hook, &event(&"4".repeat(32)), &network, 4.0, &env, &now_mono, &launcher);
    assert_eq!(outcome.status, Some(503), "the matching name connects (and meets the collector's first 503)");
}
