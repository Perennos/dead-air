//! Operator-facing regressions (Python `test_operations.py`): config, scheduling, state
//! ownership, read-only status, acknowledgment, CLI output and lifecycle.

mod common;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::*;
use dead_air::config::{EXAMPLE_TOML, parse_config_text};
use dead_air::doctor::doctor;
use dead_air::model::*;
use dead_air::persistence::{IncidentStore, StateLock, acknowledge_failed_delivery, guard_path};
use dead_air::process::{ChildResult, RunOptions};
use dead_air::status::{read_failed_deliveries, read_status, stream_status};
use dead_air::watch::{WatchHooks, WatchMode, watch_cycles};
use rusqlite::{Connection, params};
use serde_json::{Map, Value, json};

fn local_runner(argv: &[String], _t: f64, _o: &RunOptions) -> ChildResult {
    let output = if argv.iter().any(|a| a == "--help") {
        dead_air::constants::YTDLP_FLAGS.join(" ")
    } else if argv.iter().any(|a| a == "-filters") {
        "freezedetect silencedetect".into()
    } else {
        "fixture version 1".into()
    };
    ChildResult::exited(&output, "", 0)
}

#[test]
fn init_writes_the_bundled_starter_byte_for_byte_and_never_overwrites() {
    let temp = Temp::new();
    let (code, out, _) = run_binary(temp.path(), &["init", "--config", "private.toml"]);
    assert_eq!(code, 0);
    assert_eq!(value(&out)["created"], true);
    let written = std::fs::read(temp.join("private.toml")).unwrap();
    assert_eq!(written, std::fs::read(env!("CARGO_MANIFEST_DIR").to_string() + "/dead-air.example.toml").unwrap());
    assert_eq!(dead_air::config::load_config(temp.join("private.toml")).unwrap().streams.len(), 1);
    let (code, out, _) = run_binary(temp.path(), &["init", "--config", "private.toml"]);
    assert_eq!((code, value(&out)["error"].as_str()), (3, Some("CONFIG_EXISTS")));
    assert_eq!(std::fs::read(temp.join("private.toml")).unwrap(), written);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(temp.join("private.toml")).unwrap().permissions().mode() & 0o777, 0o600);
    }
}

#[test]
fn doctor_uses_fake_local_binary_checks_and_no_network() {
    let config = test_config(2.0);
    let which = |_: &str| Some(PathBuf::from("fake-tool"));
    let result = doctor(&config, &local_runner, &which);
    assert_eq!(result["ok"], true);
    assert_eq!(result["network_contacted"], false);
    assert_eq!(result["tools"]["resolver_isolation_flags_available"], true);
    assert!(result["advice"].as_array().unwrap().iter().all(|a| a.is_string()));
}

#[test]
fn direct_doctor_does_not_require_optional_youtube_tool() {
    let config = test_config(2.0);
    let which = |tool: &str| (tool != "yt-dlp").then(|| PathBuf::from(tool));
    let result = doctor(&config, &local_runner, &which);
    assert_eq!(result["ok"], true);
    assert_eq!(result["tools"]["yt_dlp"]["required"], false);
    let mut youtube = config.clone();
    youtube.streams[0].source = "youtube".into();
    let result = doctor(&youtube, &local_runner, &which);
    assert_eq!(result["ok"], false);
    assert!(result["advice"].as_array().unwrap().iter().any(|a| a.as_str().unwrap().contains("yt-dlp was not found")));
}

#[test]
fn doctor_rejects_truncated_or_unreaped_filter_check() {
    for failure in ["timed_out", "output_limited", "cleanup_incomplete"] {
        let runner = |argv: &[String], t: f64, o: &RunOptions| {
            let mut result = local_runner(argv, t, o);
            if argv.iter().any(|a| a == "-filters") {
                match failure {
                    "timed_out" => result.timed_out = true,
                    "output_limited" => result.output_limited = true,
                    _ => result.cleanup_complete = false,
                }
            }
            result
        };
        let which = |tool: &str| Some(PathBuf::from(tool));
        assert_eq!(doctor(&test_config(2.0), &runner, &which)["ok"], false, "{failure}");
    }
}

#[test]
fn malformed_inputs_are_bounded_config_errors() {
    for kind in ["[]", "{}", "5"] {
        let text = format!("{EXAMPLE_TOML}\n[[destinations]]\nid = \"bad\"\ntype = {kind}\n");
        assert!(parse_config_text(&text).is_err(), "{kind}");
    }
    assert!(dead_air::netpolicy::static_url("https://example.invalid:0/live", "direct", false).is_err());
    let huge = EXAMPLE_TOML.replace("interval_s = 60", &format!("interval_s = 1{}", "0".repeat(400)));
    assert!(parse_config_text(&huge).is_err());
    let huge_float = EXAMPLE_TOML.replace("interval_s = 60", "interval_s = 1e400");
    assert!(parse_config_text(&huge_float).is_err());
}

#[test]
fn each_stream_gets_a_share_of_the_remaining_deadline() {
    let mut config = test_config(2.0);
    config.watch.cycle_budget_s = 30.0;
    let template = config.streams[0].clone();
    config.streams = (0..3).map(|i| StreamConfig { id: format!("stream-{i}"), ..template.clone() }).collect();
    let now = Cell::new(100.0);
    let deadlines = RefCell::new(Vec::new());
    let mut observer =
        |_c: &AppConfig, s: &StreamConfig, deadline: f64, _h: &mut HashMap<String, ThumbnailBaseline>| {
            deadlines.borrow_mut().push(deadline);
            now.set(deadline);
            dead_air::media::unknown_observation(s, "FIXTURE_TIMEOUT", None)
        };
    let clock = || now.get();
    let result = dead_air::observe::check_cycle(&config, &mut observer, &clock, &mut HashMap::new());
    assert_eq!(*deadlines.borrow(), vec![110.0, 120.0, 130.0]);
    assert_eq!(
        result.iter().map(|o| o.stream_id.clone()).collect::<Vec<_>>(),
        vec!["stream-0", "stream-1", "stream-2"]
    );
}

#[test]
fn a_short_first_stream_leaves_its_unused_budget_to_later_streams() {
    let mut config = test_config(2.0);
    config.watch.cycle_budget_s = 30.0;
    let template = config.streams[0].clone();
    config.streams = vec![template.clone(), StreamConfig { id: "later".into(), ..template }];
    let now = Cell::new(100.0);
    let deadlines = RefCell::new(Vec::new());
    let mut observer =
        |_c: &AppConfig, s: &StreamConfig, deadline: f64, _h: &mut HashMap<String, ThumbnailBaseline>| {
            deadlines.borrow_mut().push(deadline);
            now.set(now.get() + 2.1); // a bounded 2 s operation plus checks
            dead_air::media::unknown_observation(s, "FIXTURE", None)
        };
    let clock = || now.get();
    dead_air::observe::check_cycle(&config, &mut observer, &clock, &mut HashMap::new());
    let later = deadlines.borrow()[1];
    assert!(later - 102.1 > 27.0, "the later stream keeps nearly the whole remaining budget ({later})");
}

#[test]
fn exhausted_cycle_budget_marks_remaining_streams_unknown_and_panics_are_contained() {
    let mut config = test_config(2.0);
    config.watch.cycle_budget_s = 10.0;
    let template = config.streams[0].clone();
    config.streams = (0..3).map(|i| StreamConfig { id: format!("s{i}"), ..template.clone() }).collect();
    let now = Cell::new(0.0);
    let mut observer =
        |_c: &AppConfig, s: &StreamConfig, _d: f64, _h: &mut HashMap<String, ThumbnailBaseline>| -> Observation {
            now.set(now.get() + 11.0);
            if s.id == "s0" {
                panic!("fixture bug");
            }
            dead_air::media::unknown_observation(s, "FIXTURE", None)
        };
    let clock = || now.get();
    let result = dead_air::observe::check_cycle(&config, &mut observer, &clock, &mut HashMap::new());
    assert_eq!(result[0].reason_codes, vec!["OBSERVATION_INTERNAL_ERROR"]);
    assert_eq!(result[1].reason_codes, vec!["CYCLE_BUDGET_EXHAUSTED"]);
    assert_eq!(result[2].reason_codes, vec!["CYCLE_BUDGET_EXHAUSTED"]);
}

fn run_watch_fake(config: &AppConfig, mode: WatchMode, cycle_s: f64, now: &Cell<f64>) -> (Vec<f64>, Vec<f64>) {
    let starts = RefCell::new(Vec::new());
    let sleeps = RefCell::new(Vec::new());
    let mut cycle = |_c: &AppConfig, _h: &mut HashMap<String, ThumbnailBaseline>| {
        starts.borrow_mut().push(now.get());
        now.set(now.get() + cycle_s);
        Vec::new()
    };
    let mut sleeper = |s: f64| {
        sleeps.borrow_mut().push(s);
        now.set(now.get() + s);
    };
    let clock = || now.get();
    let mut dispatch = |_s: &mut IncidentStore, _c: &AppConfig| Ok(());
    let hooks = WatchHooks {
        cycle: &mut cycle,
        sleeper: &mut sleeper,
        clock: &clock,
        wall: &clock,
        record_mono: &clock,
        dispatch: &mut dispatch,
    };
    watch_cycles(config, mode, None, hooks, &mut |_r| Ok(())).unwrap();
    (starts.into_inner(), sleeps.into_inner())
}

#[test]
fn watch_cycles_are_serial_and_bounded_by_requested_count() {
    let config = test_config(2.0);
    let now = Cell::new(100.0);
    let observed = Cell::new(0);
    let sleeps = RefCell::new(Vec::new());
    let mut cycle = |_c: &AppConfig, _h: &mut HashMap<String, ThumbnailBaseline>| {
        observed.set(observed.get() + 1);
        vec![healthy("2026-10-23T00:00:00Z")]
    };
    let mut sleeper = |s: f64| sleeps.borrow_mut().push(s);
    let clock = || now.get();
    let mut dispatch = |_s: &mut IncidentStore, _c: &AppConfig| Ok(());
    let hooks = WatchHooks {
        cycle: &mut cycle,
        sleeper: &mut sleeper,
        clock: &clock,
        wall: &clock,
        record_mono: &clock,
        dispatch: &mut dispatch,
    };
    let mut batches = 0;
    watch_cycles(&config, WatchMode::Cycles(3), None, hooks, &mut |_r| {
        batches += 1;
        Ok(())
    })
    .unwrap();
    assert_eq!((batches, observed.get()), (3, 3));
    assert_eq!(*sleeps.borrow(), vec![config.watch.interval_s; 2]);
}

#[test]
fn watch_interval_is_start_to_start_and_overruns_do_not_catch_up() {
    for (cycle_s, expected_sleep) in [(7.0, 23.0), (40.0, 0.0)] {
        let mut config = test_config(2.0);
        config.watch.interval_s = 30.0;
        let now = Cell::new(100.0);
        let (starts, sleeps) = run_watch_fake(&config, WatchMode::Cycles(3), cycle_s, &now);
        assert_eq!(sleeps, vec![expected_sleep, expected_sleep]);
        let step = f64::max(30.0, cycle_s);
        assert_eq!(starts, vec![100.0, 100.0 + step, 100.0 + 2.0 * step]);
    }
}

#[test]
fn watch_mode_validation_matches_the_cli_contract() {
    use dead_air::watch::validate_watch_mode;
    assert_eq!(validate_watch_mode(Some(3), false), Ok(WatchMode::Cycles(3)));
    assert_eq!(validate_watch_mode(None, true), Ok(WatchMode::Continuous));
    assert!(validate_watch_mode(Some(0), false).unwrap_err().contains("max_cycles"));
    assert!(validate_watch_mode(Some(10_001), false).is_err());
    assert!(validate_watch_mode(None, false).is_err());
    assert!(validate_watch_mode(Some(3), true).is_err());
}

fn insert_failed(store: &IncidentStore, event_id: &str, incident_id: &str, status: &str) {
    let db = store.db();
    db.execute(
        "INSERT INTO incidents VALUES(?,?,?,?,?,?,?,?)",
        params![
            incident_id,
            "sample-channel",
            "audio_silence",
            "1970-01-01T00:01:39Z",
            99.0,
            "1970-01-01T00:01:41Z",
            101.0,
            "resolved"
        ],
    )
    .unwrap();
    db.execute(
        "INSERT INTO events VALUES(?,?,?,?,?,?,?)",
        params![event_id, incident_id, "sample-channel", "audio_silence", "open", 99.0, "{}"],
    )
    .unwrap();
    let (attempts, next) = if status == "failed" { (3, None) } else { (1, Some(100.0)) };
    db.execute(
        "INSERT INTO deliveries(event_id,destination_id,status,attempts,created_wall,next_attempt_wall,last_error,last_http_status,updated_wall) \
         VALUES(?,?,?,?,99,?,'HTTP_STATUS',503,99)",
        params![event_id, "ops", status, attempts, next],
    )
    .unwrap();
}

fn config_at(path: &std::path::Path) -> AppConfig {
    let mut config = default_incident_config();
    config.watch.state_path = path.to_string_lossy().into_owned();
    config
}

#[test]
fn failed_delivery_acknowledgment_preserves_auditable_history_across_restart() {
    let temp = Temp::new();
    let path = temp.join("state.db");
    let config = config_at(&path);
    let (event_id, incident_id) = ("b".repeat(32), "a".repeat(32));
    {
        let mut store = IncidentStore::open(&path, 90).unwrap();
        store.record_cycle(&config, &[healthy("1970-01-01T00:01:40Z")], 100.0, 0.0).unwrap();
        insert_failed(&store, &event_id, &incident_id, "failed");
        let before = read_status(&config, Some(150.0)).unwrap();
        assert_eq!(before["open_incidents"], json!([]));
        assert_eq!(before["deliveries"]["unacknowledged_failed"], 1);
        assert_eq!(before["deliveries"]["acknowledged_failed"], 0);
        assert_eq!(before["status"], "UNHEALTHY");
        assert_eq!(read_failed_deliveries(&config, 50, 0).unwrap()[0]["event_id"], event_id.as_str());
        assert_eq!(
            store.acknowledge_failed_delivery(&event_id, "ops", "Alex Marzouca", "1970-01-01T00:03:20Z", 200.0),
            Ok("acknowledged")
        );
        let row = store.deliveries().unwrap()[0].clone();
        assert_eq!((row.2.as_str(), row.3, row.4.as_deref(), row.5), ("failed", 3, Some("HTTP_STATUS"), Some(503)));
        let after = read_status(&config, Some(200.0)).unwrap();
        assert_eq!(
            (after["deliveries"]["failed"].as_i64(), after["deliveries"]["unacknowledged_failed"].as_i64()),
            (Some(1), Some(0))
        );
        assert_eq!(after["deliveries"]["acknowledged_failed"], 1);
        assert_eq!(after["status"], "HEALTHY");
    }
    let store = IncidentStore::open(&path, 90).unwrap();
    assert_eq!(
        store.acknowledge_failed_delivery(&event_id, "ops", "Another operator", "1970-01-01T00:03:21Z", 201.0),
        Ok("already_acknowledged")
    );
    let failure = read_failed_deliveries(&config, 50, 0).unwrap()[0].clone();
    assert_eq!(
        (failure["acknowledged_by"].as_str(), failure["acknowledged_at"].as_str()),
        (Some("Alex Marzouca"), Some("1970-01-01T00:03:20Z"))
    );
    assert_eq!(read_status(&config, Some(201.0)).unwrap()["status"], "HEALTHY");
    let bad = store.acknowledge_failed_delivery("not valid!", "ops", "Alex", "1970-01-01T00:03:21Z", 201.0);
    assert!(matches!(bad, Err(dead_air::persistence::AckError::Invalid(_))));
    let naive = store.acknowledge_failed_delivery(&event_id, "ops", "Alex", "1970-01-01T00:03:21", 201.0);
    assert!(matches!(naive, Err(dead_air::persistence::AckError::Invalid(_))));
}

#[test]
fn pending_delivery_cannot_be_acknowledged_or_pruned() {
    let temp = Temp::new();
    let store = IncidentStore::open(temp.join("state.db"), 1).unwrap();
    let event_id = "d".repeat(32);
    insert_failed(&store, &event_id, &"c".repeat(32), "pending");
    assert_eq!(
        store.acknowledge_failed_delivery(&event_id, "ops", "Alex", "1970-01-01T00:03:20Z", 200.0),
        Ok("not_failed")
    );
    store.prune(200_000.0).unwrap();
    let count: i64 =
        store.db().query_row("SELECT COUNT(*) FROM events WHERE event_id=?", [&event_id], |r| r.get(0)).unwrap();
    assert_eq!(count, 1);
}

#[test]
fn acknowledged_failure_is_preserved_until_configured_retention() {
    let temp = Temp::new();
    let store = IncidentStore::open(temp.join("state.db"), 1).unwrap();
    let event_id = "k".repeat(32);
    insert_failed(&store, &event_id, &"j".repeat(32), "failed");
    let count = || -> i64 {
        store.db().query_row("SELECT COUNT(*) FROM events WHERE event_id=?", [&event_id], |r| r.get(0)).unwrap()
    };
    store.prune(200_000.0).unwrap();
    assert_eq!(count(), 1);
    let at = dead_air::timefmt::iso_seconds(200_000.0).unwrap();
    assert_eq!(store.acknowledge_failed_delivery(&event_id, "ops", "Alex", &at, 200_000.0), Ok("acknowledged"));
    store.prune(200_000.0).unwrap();
    assert_eq!(count(), 0);
}

fn legacy_schema_one(path: &std::path::Path, event_id: &str, incident_id: &str) {
    let db = Connection::open(path).unwrap();
    db.execute_batch(
        "CREATE TABLE stream_state(stream_id TEXT PRIMARY KEY,state_json TEXT NOT NULL);
         CREATE TABLE incidents(incident_id TEXT PRIMARY KEY,stream_id TEXT NOT NULL,condition TEXT NOT NULL,
             opened_at TEXT NOT NULL,opened_wall REAL NOT NULL,closed_at TEXT,closed_wall REAL,status TEXT NOT NULL);
         CREATE TABLE events(event_id TEXT PRIMARY KEY,incident_id TEXT NOT NULL,stream_id TEXT NOT NULL,condition TEXT NOT NULL,
             transition TEXT NOT NULL,created_wall REAL NOT NULL,payload_json TEXT NOT NULL);
         CREATE TABLE deliveries(event_id TEXT NOT NULL,destination_id TEXT NOT NULL,status TEXT NOT NULL,attempts INTEGER NOT NULL,
             created_wall REAL NOT NULL,next_attempt_wall REAL,last_error TEXT,last_http_status INTEGER,
             updated_wall REAL NOT NULL,PRIMARY KEY(event_id,destination_id));
         PRAGMA user_version=1;",
    )
    .unwrap();
    db.execute(
        "INSERT INTO incidents VALUES(?,?,?,?,?,?,?,?)",
        params![
            incident_id,
            "sample-channel",
            "audio_silence",
            "1970-01-01T00:01:39Z",
            99.0,
            "1970-01-01T00:01:41Z",
            101.0,
            "resolved"
        ],
    )
    .unwrap();
    db.execute(
        "INSERT INTO events VALUES(?,?,?,?,?,?,?)",
        params![event_id, incident_id, "sample-channel", "audio_silence", "open", 99.0, "{}"],
    )
    .unwrap();
    db.execute("INSERT INTO deliveries VALUES(?,?,'failed',3,99,NULL,'HTTP_STATUS',503,99)", params![event_id, "ops"])
        .unwrap();
}

#[test]
fn schema_one_status_is_read_only_then_writer_migrates_without_losing_failures() {
    let temp = Temp::new();
    let path = temp.join("legacy.db");
    let (event_id, incident_id) = ("f".repeat(32), "e".repeat(32));
    legacy_schema_one(&path, &event_id, &incident_id);
    let config = config_at(&path);
    let snapshot = read_status(&config, Some(150.0)).unwrap();
    assert_eq!(snapshot["deliveries"]["unacknowledged_failed"], 1);
    assert_eq!(read_failed_deliveries(&config, 50, 0).unwrap()[0]["acknowledged_by"], Value::Null);
    let version = |p: &std::path::Path| -> i64 {
        Connection::open(p).unwrap().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap()
    };
    assert_eq!(version(&path), 1, "status never migrates");
    assert_eq!(
        acknowledge_failed_delivery(&path, &event_id, "ops", "Alex", "1970-01-01T00:03:20Z", 200.0, 90),
        Ok("acknowledged")
    );
    assert_eq!(version(&path), 2);
    let row: (String, String) = Connection::open(&path)
        .unwrap()
        .query_row("SELECT status,acknowledged_by FROM deliveries WHERE event_id=?", [&event_id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(row, ("failed".into(), "Alex".into()));
    assert_eq!(read_failed_deliveries(&config, 50, 0).unwrap()[0]["acknowledged_by"], "Alex");
}

#[test]
fn ack_delivery_and_failures_cli_report_ids_and_contact_no_network() {
    let temp = Temp::new();
    let config_path = write_config(temp.path(), "");
    let config = dead_air::config::load_config(&config_path).unwrap();
    let (event_id, incident_id) = ("h".repeat(32), "g".repeat(32));
    {
        let store = IncidentStore::open(&config.watch.state_path, 90).unwrap();
        insert_failed(&store, &event_id, &incident_id, "failed");
    }
    let config_arg = config_path.to_string_lossy().into_owned();
    let (code, out, _) = run_binary(temp.path(), &["failures", "--config", &config_arg]);
    assert_eq!(code, 0);
    assert_eq!(value(&out)["failures"][0]["event_id"], event_id.as_str());
    assert_eq!(run_binary(temp.path(), &["failures", "--config", &config_arg, "--limit", "0"]).0, 3);
    let (code, out, _) = run_binary(
        temp.path(),
        &[
            "ack-delivery",
            "--config",
            &config_arg,
            "--event-id",
            &event_id,
            "--destination-id",
            "ops",
            "--operator",
            "Alex",
        ],
    );
    assert_eq!((code, value(&out)["result"].as_str()), (0, Some("acknowledged")));
    let (code, out, _) = run_binary(
        temp.path(),
        &[
            "ack-delivery",
            "--config",
            &config_arg,
            "--event-id",
            &"z".repeat(32),
            "--destination-id",
            "ops",
            "--operator",
            "Alex",
        ],
    );
    assert_eq!((code, value(&out)["result"].as_str()), (1, Some("not_found")));
}

#[test]
fn ack_delivery_works_during_watch_without_resetting_stream_counters() {
    let temp = Temp::new();
    let path = temp.join("state.db");
    let config = {
        let mut c = config_at(&path);
        c.streams[0].min_bad_samples = 2;
        c.streams[0].unhealthy_after_s = 0.0;
        c
    };
    let mut store = IncidentStore::open(&path, 90).unwrap();
    store.record_cycle(&config, &[bad_silence()], 100.0, 0.0).unwrap();
    let state_before: String = store.db().query_row("SELECT state_json FROM stream_state", [], |r| r.get(0)).unwrap();
    let event_id = "m".repeat(32);
    insert_failed(&store, &event_id, &"l".repeat(32), "failed");
    assert_eq!(
        acknowledge_failed_delivery(&path, &event_id, "ops", "Alex", "1970-01-01T00:03:20Z", 200.0, 90),
        Ok("acknowledged")
    );
    let state_after: String = store.db().query_row("SELECT state_json FROM stream_state", [], |r| r.get(0)).unwrap();
    assert_eq!(state_before, state_after);
    assert!(state_after.contains("\"audio_silence\""), "the pending bad streak survives");
}

#[test]
fn ack_delivery_waits_for_a_concurrent_sqlite_writer() {
    let temp = Temp::new();
    let path = temp.join("state.db");
    let event_id = "w".repeat(32);
    let store = IncidentStore::open(&path, 90).unwrap();
    insert_failed(&store, &event_id, &"x".repeat(32), "failed");
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let writer_path = path.clone();
    let writer = std::thread::spawn(move || {
        let db = Connection::open(&writer_path).unwrap();
        db.busy_timeout(Duration::from_secs(2)).unwrap();
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        held_tx.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        db.execute_batch("COMMIT").unwrap();
    });
    held_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(
        acknowledge_failed_delivery(&path, &event_id, "ops", "Alex", "1970-01-01T00:03:20Z", 200.0, 90),
        Ok("acknowledged")
    );
    writer.join().unwrap();
    let by: String = store
        .db()
        .query_row("SELECT acknowledged_by FROM deliveries WHERE event_id=?", [&event_id], |r| r.get(0))
        .unwrap();
    assert_eq!(by, "Alex");
}

#[test]
fn ack_delivery_does_not_create_missing_state_or_accept_an_empty_one() {
    let temp = Temp::new();
    let missing = temp.join("missing.db");
    assert_eq!(
        acknowledge_failed_delivery(&missing, &"v".repeat(32), "ops", "Alex", "1970-01-01T00:03:20Z", 200.0, 90),
        Ok("not_found")
    );
    assert!(!missing.exists());
    let empty = temp.join("empty.db");
    std::fs::write(&empty, b"").unwrap();
    let result = acknowledge_failed_delivery(&empty, &"v".repeat(32), "ops", "Alex", "1970-01-01T00:03:20Z", 200.0, 90);
    assert_eq!(result.unwrap_err().to_string(), "STATE_CORRUPT");
    assert_eq!(std::fs::metadata(&empty).unwrap().len(), 0);
}

#[test]
fn legacy_writer_cannot_open_while_new_writer_owns_database() {
    let temp = Temp::new();
    let _store = IncidentStore::open(temp.join("state.db"), 90).unwrap();
    // The original release's exact exclusive ownership step.
    let legacy = std::fs::OpenOptions::new().write(true).create_new(true).open(temp.join(".dead-air.lock"));
    assert_eq!(legacy.unwrap_err().kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read(guard_path(&temp.join("state.db"))).unwrap(), dead_air::constants::LEGACY_GUARD);
}

#[test]
fn legacy_lock_is_never_removed_or_bypassed() {
    let temp = Temp::new();
    std::fs::write(temp.join(".dead-air.lock"), "unknown owner").unwrap();
    assert!(IncidentStore::open(temp.join("state.db"), 90).is_err());
    assert_eq!(std::fs::read_to_string(temp.join(".dead-air.lock")).unwrap(), "unknown owner");
}

#[test]
fn a_late_cycle_commit_does_not_refresh_an_old_sample() {
    let temp = Temp::new();
    let path = temp.join("state.db");
    let config = {
        let mut c = test_config(2.0);
        c.watch.state_path = path.to_string_lossy().into_owned();
        c
    };
    let mut store = IncidentStore::open(&path, 90).unwrap();
    let committed_at = 100.0 + config.streams[0].max_gap_s + 1.0;
    store.record_cycle(&config, &[healthy("1970-01-01T00:01:40+00:00")], committed_at, 0.0).unwrap();
    let result = read_status(&config, Some(committed_at)).unwrap();
    assert_eq!(result["status"], "UNKNOWN");
    assert!(result["streams"][0]["reason_codes"].as_array().unwrap().contains(&json!("OBSERVATION_STALE")));
}

#[test]
fn one_writer_per_database_and_separate_databases_share_a_directory() {
    let temp = Temp::new();
    let (first, second) = (temp.join("first.db"), temp.join("second.db"));
    let _a = IncidentStore::open(&first, 90).unwrap();
    assert_eq!(IncidentStore::open(&first, 90).err().unwrap().0, "STATE_LOCKED");
    assert!(StateLock::is_held(&first));
    let _b = IncidentStore::open(&second, 90).unwrap();
    assert!(second.exists());
    assert!(!StateLock::is_held(&temp.join("third.db")));
    assert!(!temp.join("third.db.lock").exists(), "probing never creates a lock file");
}

#[test]
fn read_only_status_during_watch_preserves_counters_and_reports_deliveries() {
    let temp = Temp::new();
    let path = temp.join("state.db");
    let mut config = incident_config(1, 0.0, 2, 180.0, 600.0);
    config.destinations = vec![DestinationConfig::webhook("ops", "webhook", "FIXTURE_URL")];
    config.watch.state_path = path.to_string_lossy().into_owned();
    let bad = observation("UNHEALTHY", Some(silence()), None, &["AUDIO_SILENCE"], "1970-01-01T00:01:40Z");
    let good = healthy("1970-01-01T00:01:41Z");
    {
        let mut store = IncidentStore::open(&path, 90).unwrap();
        store.record_cycle(&config, &[bad], 100.0, 0.0).unwrap();
        store.record_cycle(&config, std::slice::from_ref(&good), 101.0, 1.0).unwrap();
        let snapshot = read_status(&config, Some(102.0)).unwrap();
        assert_eq!(snapshot["status"], "UNHEALTHY");
        assert_eq!(snapshot["streams"][0]["observation_status"], "HEALTHY");
        assert_eq!(snapshot["deliveries"]["pending"], 1);
        assert_eq!(snapshot["open_incidents"][0]["condition"], "audio_silence");
        let text = serde_json::to_string(&snapshot).unwrap();
        assert!(!text.contains("https://") && !text.contains("FIXTURE_URL"));
        store.record_cycle(&config, &[good], 102.0, 2.0).unwrap();
        assert!(store.open_incidents().unwrap().is_empty(), "the reader did not reset the recovery counter");
    }
    let _store = IncidentStore::open(&path, 90).unwrap();
    assert_eq!(read_status(&config, Some(103.0)).unwrap()["streams"][0]["observation_status"], "HEALTHY");
}

#[test]
fn status_marks_absent_stale_rollback_and_changed_sources_unknown() {
    let temp = Temp::new();
    let path = temp.join("not-created").join("state.db");
    let config = {
        let mut c = test_config(2.0);
        c.watch.state_path = path.to_string_lossy().into_owned();
        c
    };
    assert_eq!(read_status(&config, Some(100.0)).unwrap()["status"], "UNKNOWN");
    assert!(!path.parent().unwrap().exists(), "status never creates state");
    let mut store = IncidentStore::open(&path, 90).unwrap();
    store.record_cycle(&config, &[healthy("1970-01-01T00:01:40Z")], 100.0, 0.0).unwrap();
    assert_eq!(read_status(&config, Some(101.0)).unwrap()["status"], "HEALTHY");
    assert_eq!(read_status(&config, Some(100.0 + config.streams[0].max_gap_s + 1.0)).unwrap()["status"], "UNKNOWN");
    assert_eq!(read_status(&config, Some(99.0)).unwrap()["streams"][0]["reason_codes"], json!(["CLOCK_ROLLBACK"]));
    let mut changed = config.clone();
    changed.streams[0].url = "https://example.invalid/changed".into();
    assert_eq!(read_status(&changed, Some(101.0)).unwrap()["streams"][0]["reason_codes"], json!(["CONFIG_CHANGED"]));
}

#[test]
fn malformed_or_unrepresentable_observation_times_stay_unknown() {
    let stream = test_config(2.0).streams[0].clone();
    let latest = json!({"status": "HEALTHY", "observed_at": "1970-01-01T00:01:40Z"});
    let cases = [
        (json!(1e300), 100.0),
        (json!("100"), 100.0),
        (json!(true), 100.0),
        (json!(null), 100.0),
        (json!(-1e308), 1e308),
        (json!({}), 100.0),
    ];
    for (raw_wall, when) in cases {
        let mut state = Map::new();
        state.insert("latest".into(), latest.clone());
        state.insert("last_observed_wall".into(), raw_wall.clone());
        state.insert("config_fingerprint".into(), json!(stream.fingerprint()));
        let result = stream_status(&stream, &state, when);
        assert_eq!(result["status"], "UNKNOWN", "{raw_wall}");
        assert!(result["age_s"].is_null() || raw_wall == json!(1e300));
        serde_json::to_string(&result).unwrap();
    }
    let mut state = Map::new();
    state.insert(
        "latest".into(),
        json!({"status": "HEALTHY", "observed_at": "\u{1b}[31mred", "reason_codes": ["ok", "GOOD_CODE", 5]}),
    );
    state.insert("last_observed_wall".into(), json!(100.0));
    state.insert("config_fingerprint".into(), json!(stream.fingerprint()));
    let result = stream_status(&stream, &state, 101.0);
    assert_eq!(result["observed_at"], Value::Null, "stored strings never reach the terminal unchecked");
    assert_eq!(result["reason_codes"], json!(["GOOD_CODE"]));
}

#[test]
fn check_has_human_output_and_json_contains_alert_diagnostics() {
    let temp = Temp::new();
    let config_path = write_config(temp.path(), "");
    let config_arg = config_path.to_string_lossy().into_owned();
    // example.invalid never resolves: the honest result is UNKNOWN (exit 2), never LIVE.
    let (code, out, _) = run_binary(temp.path(), &["check", "--config", &config_arg]);
    assert_eq!(code, 2);
    assert!(out.contains("sample-channel: UNKNOWN"));
    assert!(out.contains("Incidents: 0 open | Alerts: 0 pending, 0 failed, 0 sent"));
    assert!(!out.starts_with('{'));
    let (code, out, _) = run_binary(temp.path(), &["check", "--config", &config_arg, "--json"]);
    assert_eq!(code, 2);
    let parsed = value(out.trim());
    assert!(parsed["observations"].is_array());
    assert!(parsed["deliveries"].is_object());
    assert!(parsed["open_incidents"].is_array());
    let (code, out, _) = run_binary(temp.path(), &["status", "--config", &config_arg, "--json"]);
    assert_eq!(code, 2);
    assert_eq!(value(&out)["network_contacted"], false);
}

#[test]
fn watch_requires_one_explicit_mode_and_rejects_invalid_count_before_state() {
    let temp = Temp::new();
    for args in [
        vec!["watch", "--config", "unused.toml"],
        vec!["watch", "--config", "unused.toml", "--max-cycles", "3", "--continuous"],
    ] {
        let (code, _, err) = run_binary(temp.path(), &args);
        assert_eq!(code, 2, "{err}");
    }
    let (code, out, _) = run_binary(temp.path(), &["watch", "--config", "unused.toml", "--max-cycles", "0"]);
    assert_eq!(code, 3);
    assert_eq!(out.lines().count(), 1);
    assert!(value(&out)["error"].as_str().unwrap().contains("max_cycles"));
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0, "no config read, no state created");
}

#[test]
fn finite_watch_runs_serial_cycles_and_flushes_one_line_each() {
    let temp = Temp::new();
    let config_path = write_config(temp.path(), "").to_string_lossy().into_owned();
    let text = std::fs::read_to_string(&config_path).unwrap().replace("interval_s = 60", "interval_s = 1");
    std::fs::write(&config_path, text).unwrap();
    let (code, out, _) = run_binary(temp.path(), &["watch", "--config", &config_path, "--max-cycles", "2", "--json"]);
    assert_eq!(code, 2);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 2);
    for line in lines {
        assert_eq!(value(line)["observations"][0]["status"], "UNKNOWN");
    }
}

#[test]
fn finite_watch_aggregates_unhealthy_over_unknown_and_healthy() {
    use dead_air::watch::aggregate;
    let fold = |codes: &[i32]| codes.iter().fold(0, |a, c| aggregate(a, *c));
    assert_eq!(fold(&[2, 1, 0]), 1);
    assert_eq!(fold(&[2, 0]), 2);
    assert_eq!(fold(&[0]), 0);
    assert_eq!(fold(&[1, 2]), 1);
}

#[cfg(unix)]
fn wait_until(mut condition: impl FnMut() -> bool, seconds: f64) -> bool {
    let end = Instant::now() + Duration::from_secs_f64(seconds);
    while Instant::now() < end {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    false
}

#[cfg(unix)]
fn stalled_source_config(temp: &Temp) -> (std::net::TcpListener, String) {
    // A loopback listener that accepts and never answers keeps `check` inside a bounded wait.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let extra = "";
    let path = write_config(temp.path(), extra);
    let text = std::fs::read_to_string(&path)
        .unwrap()
        .replace("https://example.invalid/live/sample", &format!("http://127.0.0.1:{port}/live.m3u8"))
        .replace("allow_private = false", "allow_private = true")
        .replace("private_allowlist = []", &format!("private_allowlist = [\"127.0.0.1:{port}\"]"));
    std::fs::write(&path, text).unwrap();
    (listener, path.to_string_lossy().into_owned())
}

#[cfg(unix)]
#[test]
fn sigterm_cleans_owned_state_and_exits_143() {
    let temp = Temp::new();
    let (_listener, config) = stalled_source_config(&temp);
    let state = temp.join("state").join("dead-air.sqlite3");
    let mut child =
        Command::new(binary()).args(["check", "--config", &config, "--json"]).stdout(Stdio::piped()).spawn().unwrap();
    assert!(wait_until(|| StateLock::is_held(&state), 10.0), "check never took the writer lock");
    std::thread::sleep(Duration::from_millis(200));
    // SAFETY: signalling our own child.
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(143));
    assert!(!StateLock::is_held(&state));
    let _ = IncidentStore::open(&state, 90).unwrap();
}

#[cfg(unix)]
#[test]
fn ctrl_c_during_watch_exits_130_and_releases_the_lock() {
    let temp = Temp::new();
    let (_listener, config) = stalled_source_config(&temp);
    let state = temp.join("state").join("dead-air.sqlite3");
    let mut child = Command::new(binary())
        .args(["watch", "--config", &config, "--continuous"])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    assert!(wait_until(|| StateLock::is_held(&state), 10.0));
    std::thread::sleep(Duration::from_millis(200));
    let started = Instant::now();
    // SAFETY: signalling our own child.
    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(130));
    assert!(started.elapsed() < Duration::from_secs(3), "interruption is prompt");
    assert!(!StateLock::is_held(&state));
}

#[cfg(unix)]
#[test]
fn lock_is_released_by_os_after_abrupt_process_exit() {
    let temp = Temp::new();
    let (_listener, config) = stalled_source_config(&temp);
    let state = temp.join("state").join("dead-air.sqlite3");
    let mut child = Command::new(binary())
        .args(["watch", "--config", &config, "--continuous"])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    assert!(wait_until(|| StateLock::is_held(&state), 10.0));
    child.kill().unwrap(); // SIGKILL: no cleanup code runs
    child.wait().unwrap();
    let store = IncidentStore::open(&state, 90).unwrap();
    assert!(store.open_incidents().unwrap().is_empty());
}

#[test]
fn http_client_connects_to_the_checked_address_without_resolving_the_hostname() {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let mut request = vec![0u8; 4096];
        let n = socket.read(&mut request).unwrap();
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\nok").unwrap();
        String::from_utf8_lossy(&request[..n]).into_owned()
    });
    let network = NetworkConfig {
        private_allowlist: vec![format!("media.invalid:{port}")],
        max_redirects: 0,
        thumbnail_max_bytes: 1024,
    };
    let resolver = dead_air::netpolicy::Resolver::always(&["127.0.0.1"]);
    let options = dead_air::http::GetOptions {
        source: "direct",
        network: &network,
        allow_private: true,
        timeout_s: 2.0,
        max_bytes: 64,
        resolver: &resolver,
        range_request: true,
        deadline: None,
    };
    let result = dead_air::http::bounded_http_get(&format!("http://media.invalid:{port}/start"), &options);
    assert_eq!((result.status, result.body.as_slice()), (Some(200), b"ok".as_slice()));
    let request = server.join().unwrap();
    assert!(request.starts_with("GET /start HTTP/1.1\r\n"));
    assert!(request.contains(&format!("Host: media.invalid:{port}\r\n")), "{request}");
    assert!(request.contains("Range: bytes=0-63\r\n"));
    assert!(request.contains("User-Agent: Dead-Air/"));
}

#[test]
fn state_paths_with_symlinks_are_refused() {
    #[cfg(unix)]
    {
        let temp = Temp::new();
        std::fs::create_dir(temp.join("real")).unwrap();
        std::os::unix::fs::symlink(temp.join("real"), temp.join("link")).unwrap();
        let path = temp.join("link").join("state.db");
        assert_eq!(IncidentStore::open(&path, 90).err().unwrap().0, "STATE_PATH_UNSAFE");
        let config = config_at(&path);
        assert_eq!(read_status(&config, None).unwrap_err().0, "STATE_PATH_UNSAFE");
    }
}
