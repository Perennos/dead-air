//! The optional loopback Prometheus endpoint wired into `watch`.

mod common;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::*;

fn scrape(port: u16) -> Option<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    write!(stream, "GET /metrics HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n").ok()?;
    let mut text = String::new();
    stream.read_to_string(&mut text).ok()?;
    text.starts_with("HTTP/1.1 200").then_some(text)
}

#[test]
fn watch_exposes_cycle_and_condition_metrics_on_loopback_only() {
    let temp = Temp::new();
    let port = {
        let probe = TcpListener::bind("127.0.0.1:0").unwrap();
        probe.local_addr().unwrap().port()
    };
    let config = write_config(temp.path(), &format!("\n[metrics]\nenabled = true\nport = {port}\n"));
    let text = std::fs::read_to_string(&config).unwrap().replace("interval_s = 60", "interval_s = 1");
    std::fs::write(&config, text).unwrap();
    let mut child = Command::new(binary())
        .args(["watch", "--config", &config.to_string_lossy(), "--continuous", "--json"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let end = Instant::now() + Duration::from_secs(20);
    let mut body = None;
    while Instant::now() < end {
        if let Some(text) = scrape(port) {
            if text.contains("deadair_cycles_total 1") || text.contains("deadair_cycles_total 2") {
                body = Some(text);
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
    let mut stderr = String::new();
    child.stderr.take().unwrap().read_to_string(&mut stderr).unwrap();
    let body = body.expect("metrics never showed a completed cycle");
    assert!(stderr.contains(&format!("http://127.0.0.1:{port}/metrics")), "{stderr}");
    assert!(body.contains("text/plain; version=0.0.4"));
    assert!(body.contains("deadair_stream_status{stream=\"sample-channel\",status=\"UNKNOWN\"} 1"));
    assert!(
        body.contains("deadair_condition_signal{stream=\"sample-channel\",condition=\"off_air\",signal=\"unknown\"} 1")
    );
    assert!(body.contains("deadair_last_cycle_duration_seconds "));
    assert!(!body.contains("deadair_last_cycle_duration_seconds NaN"));
    assert!(body.contains("deadair_open_incidents 0"));
    assert!(body.contains("deadair_delivery_failures_unacknowledged 0"));
    assert!(!body.contains("example.invalid") && !body.contains("Sample channel"));
}

#[test]
fn a_busy_metrics_port_is_reported_and_never_stops_watch() {
    let temp = Temp::new();
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port();
    let config = write_config(temp.path(), &format!("\n[metrics]\nenabled = true\nport = {port}\n"));
    let output = Command::new(binary())
        .args(["watch", "--config", &config.to_string_lossy(), "--max-cycles", "1"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "the cycle still ran and reported UNKNOWN");
    assert!(String::from_utf8_lossy(&output.stderr).contains("METRICS_UNAVAILABLE"));
    assert!(String::from_utf8_lossy(&output.stdout).contains("sample-channel: UNKNOWN"));
}
