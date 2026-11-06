//! Continuous watch: flushed cycles, stable event ids across retries, interruption and fatal
//! sender cleanup (Python `test_dead_air.py` watch regressions), plus upgrade continuity with
//! state written by the Python 0.2.0 release.

mod common;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use common::*;
use dead_air::alerts::{DispatchOptions, SendOutcome, dispatch_due};
use dead_air::interrupt::Interrupted;
use dead_air::model::*;
use dead_air::persistence::IncidentStore;
use dead_air::watch::{WatchHooks, WatchMode, watch_cycles};
use serde_json::Value;

fn watch_observation(status: &str, index: usize) -> Observation {
    let at = format!("2026-10-23T00:00:{index:02}Z");
    match status {
        "UNHEALTHY" => observation("UNHEALTHY", Some(silence()), None, &["AUDIO_SILENCE"], &at),
        _ => healthy(&at),
    }
}

struct Harness {
    config: AppConfig,
    temp: Temp,
}

impl Harness {
    fn new() -> Self {
        let temp = Temp::new();
        let mut config = incident_config(1, 0.0, 1, 180.0, 600.0);
        config.watch.interval_s = 30.0;
        config.watch.state_path = temp.join("state.db").to_string_lossy().into_owned();
        config.destinations = vec![DestinationConfig::webhook("fake", "webhook", "FAKE_URL")];
        Harness { config, temp }
    }
}

type SenderCalls = RefCell<Vec<(String, String)>>;
type Unwound = Result<Result<(), PersistenceError>, Box<dyn std::any::Any + Send>>;
type Run = (Unwound, Vec<usize>, Vec<(f64, usize, usize)>, SenderCalls);

/// Run continuous watch over `statuses` with a fake clock; `stop_after` interrupts on that print.
fn run(
    harness: &Harness,
    statuses: &[&str],
    sender: &mut dyn FnMut(usize) -> SendOutcome,
    stop_after: Option<usize>,
) -> Run {
    let wall = Cell::new(100.0);
    let mono = Cell::new(10.0);
    let observed = Cell::new(0usize);
    let writes: RefCell<Vec<usize>> = RefCell::default();
    let sleeps: RefCell<Vec<(f64, usize, usize)>> = RefCell::default();
    let calls: SenderCalls = RefCell::default();
    let mut store = IncidentStore::open(&harness.config.watch.state_path, harness.config.retention_days).unwrap();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut cycle = |_c: &AppConfig, _h: &mut HashMap<String, ThumbnailBaseline>| {
            let index = observed.get();
            observed.set(index + 1);
            vec![watch_observation(statuses[index], index)]
        };
        let mut sleeper = |seconds: f64| {
            sleeps.borrow_mut().push((seconds, writes.borrow().len(), observed.get()));
            wall.set(wall.get() + seconds);
            mono.set(mono.get() + seconds);
        };
        let wall_now = || wall.get();
        let mono_now = || mono.get();
        let mut dispatch = |store: &mut IncidentStore, config: &AppConfig| {
            let mut send = |_d: &DestinationConfig, event: &Value, _n: &NetworkConfig| {
                calls.borrow_mut().push((
                    event["event_id"].as_str().unwrap().to_string(),
                    event["transition"].as_str().unwrap().to_string(),
                ));
                sender(calls.borrow().len())
            };
            let options = DispatchOptions {
                now_wall: Some(wall.get()),
                sender: Some(&mut send),
                monotonic: &mono_now,
                ..DispatchOptions::default()
            };
            dispatch_due(store, &config.destinations, &config.network, options)
        };
        let hooks = WatchHooks {
            cycle: &mut cycle,
            sleeper: &mut sleeper,
            clock: &mono_now,
            wall: &wall_now,
            record_mono: &mono_now,
            dispatch: &mut dispatch,
        };
        watch_cycles(&harness.config, WatchMode::Continuous, Some(&mut store), hooks, &mut |report| {
            let line =
                dead_air::watch::format_observations(report.observations, &report.store.unwrap().diagnostics()?, true);
            assert!(line.starts_with('{'));
            writes.borrow_mut().push(observed.get());
            if Some(writes.borrow().len()) == stop_after {
                std::panic::resume_unwind(Box::new(Interrupted));
            }
            Ok(())
        })
    }));
    drop(store);
    let writes = writes.into_inner();
    let sleeps = sleeps.into_inner();
    (outcome, writes, sleeps, calls)
}

#[test]
fn continuous_watch_streams_flushed_cycles_and_interrupts_cleanly() {
    let harness = Harness::new();
    let statuses = ["UNHEALTHY", "UNHEALTHY", "HEALTHY"];
    let mut sender = |call: usize| {
        if call == 1 {
            SendOutcome { status: Some(503), ..SendOutcome::failed("HTTP_STATUS") }
        } else {
            SendOutcome::sent(Some(204))
        }
    };
    let (outcome, writes, sleeps, calls) = run(&harness, &statuses, &mut sender, Some(3));
    assert!(outcome.unwrap_err().is::<Interrupted>());
    assert_eq!(writes, vec![1, 2, 3]);
    assert_eq!(sleeps, vec![(30.0, 1, 1), (30.0, 2, 2)]);
    let calls = calls.into_inner();
    assert_eq!(calls[0].0, calls[1].0, "the retry keeps the stable event id");
    assert_eq!(calls.iter().map(|c| c.1.as_str()).collect::<Vec<_>>(), vec!["open", "open", "recovery"]);
    let store = IncidentStore::open(&harness.config.watch.state_path, 90).unwrap();
    let events = store.events().unwrap();
    assert_eq!(events.iter().map(|e| e.2.as_str()).collect::<Vec<_>>(), vec!["open", "recovery"]);
    let deliveries: HashMap<String, (String, i64)> =
        store.deliveries().unwrap().into_iter().map(|r| (r.0, (r.2, r.3))).collect();
    assert_eq!(
        calls.iter().map(|c| c.0.clone()).collect::<Vec<_>>(),
        vec![events[0].0.clone(), events[0].0.clone(), events[1].0.clone()]
    );
    assert_eq!(deliveries[&events[0].0], ("sent".to_string(), 2));
    assert_eq!(deliveries[&events[1].0], ("sent".to_string(), 1));
    assert!(store.open_incidents().unwrap().is_empty());
    let _ = &harness.temp;
}

#[test]
fn continuous_sender_cleanup_failure_stops_and_persists_before_exit() {
    let harness = Harness::new();
    let statuses = ["UNHEALTHY", "HEALTHY", "HEALTHY"];
    let mut sender = |call: usize| {
        if call == 1 { SendOutcome::sent(Some(204)) } else { SendOutcome::failed("DELIVERY_CLEANUP_FAILED") }
    };
    let (outcome, writes, sleeps, calls) = run(&harness, &statuses, &mut sender, None);
    let error = outcome.unwrap().unwrap_err();
    assert_eq!(error.0, "DELIVERY_CLEANUP_FAILED");
    assert_eq!(writes, vec![1], "the failing cycle prints no observation line");
    assert_eq!(sleeps, vec![(30.0, 1, 1)]);
    assert_eq!(calls.borrow().len(), 2);
    let store = IncidentStore::open(&harness.config.watch.state_path, 90).unwrap();
    let events = store.events().unwrap();
    assert_eq!(events.iter().map(|e| e.2.as_str()).collect::<Vec<_>>(), vec!["open", "recovery"]);
    let deliveries: HashMap<String, (String, i64, Option<String>)> =
        store.deliveries().unwrap().into_iter().map(|r| (r.0, (r.2, r.3, r.4))).collect();
    assert_eq!(deliveries[&events[0].0], ("sent".to_string(), 1, None));
    assert_eq!(deliveries[&events[1].0], ("pending".to_string(), 1, Some("DELIVERY_CLEANUP_FAILED".to_string())));
    assert!(!dead_air::watch::error_line(&error.0).contains("FAKE_URL"));
}

fn python_fixture(temp: &Temp) -> AppConfig {
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    for name in ["python-0.2.0.toml", "python-0.2.0-state.sqlite3"] {
        std::fs::copy(source.join(name), temp.join(name)).unwrap();
    }
    dead_air::config::load_config(temp.join("python-0.2.0.toml")).unwrap()
}

fn strip_rust_additions(mut status: Value) -> Value {
    status.as_object_mut().unwrap().remove("briefs");
    for incident in status["open_incidents"].as_array_mut().unwrap() {
        incident.as_object_mut().unwrap().remove("incident_id");
    }
    status
}

#[test]
fn state_written_by_the_python_release_reads_identically() {
    let temp = Temp::new();
    let config = python_fixture(&temp);
    let expected: Value = serde_json::from_str(
        &std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/python-0.2.0-status.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let status = dead_air::status::read_status(&config, Some(1_792_713_660.0)).unwrap();
    assert_eq!(
        status["streams"][0]["reason_codes"],
        serde_json::json!(["AUDIO_SILENCE"]),
        "the config fingerprint matches across releases"
    );
    assert_eq!(status["open_incidents"][0]["incident_id"].as_str().map(str::len), Some(32));
    assert_eq!(strip_rust_additions(status.clone()), expected);
    assert_eq!(
        dead_air::pyjson::dumps(&strip_rust_additions(status)) + "\n",
        dead_air::pyjson::dumps(&expected) + "\n"
    );
    let failures: Value = serde_json::from_str(
        &std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/python-0.2.0-failures.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(Value::Array(dead_air::status::read_failed_deliveries(&config, 50, 0).unwrap()), failures);
}

#[test]
fn a_rust_watcher_continues_the_python_history_without_losing_incidents_or_deliveries() {
    let temp = Temp::new();
    let config = python_fixture(&temp);
    let mut store = IncidentStore::open(&config.watch.state_path, config.retention_days).unwrap();
    assert_eq!(store.open_incidents().unwrap().len(), 1);
    let before = store.deliveries().unwrap();
    assert_eq!(before[0].2, "failed");
    let good = |at: &str| observation("HEALTHY", None, None, &[], at);
    store.record_cycle(&config, &[good("2026-10-23T00:02:00Z")], 1_792_713_720.0, 1.0).unwrap();
    store.record_cycle(&config, &[good("2026-10-23T00:03:00Z")], 1_792_713_780.0, 61.0).unwrap();
    assert!(store.open_incidents().unwrap().is_empty(), "positive recovery closes the Python-opened incident");
    let events = store.events().unwrap();
    assert_eq!(events.iter().map(|e| e.2.as_str()).collect::<Vec<_>>(), vec!["open", "recovery"]);
    let after = store.deliveries().unwrap();
    assert!(after.iter().any(|r| r == &before[0]), "the failed Python delivery is untouched");
    assert!(after.iter().any(|r| r.2 == "pending" && r.0 == events[1].0));
    let event_id = before[0].0.clone();
    drop(store);
    let result = dead_air::persistence::acknowledge_failed_delivery(
        std::path::Path::new(&config.watch.state_path),
        &event_id,
        "ops-hook",
        "Alex",
        "2026-10-23T00:04:00Z",
        1_792_713_840.0,
        90,
    );
    assert_eq!(result, Ok("acknowledged"));
    let version: i64 = rusqlite::Connection::open(&config.watch.state_path)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 2, "the on-disk schema is unchanged");
}
