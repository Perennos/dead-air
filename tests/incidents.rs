//! Incident transitions, evidence and restart semantics (Python `test_incidents.py`).

mod common;

use std::collections::{BTreeSet, HashMap};

use common::*;
use dead_air::model::*;
use dead_air::persistence::IncidentStore;
use serde_json::Value;

fn open(temp: &Temp) -> IncidentStore {
    IncidentStore::open(temp.join("state.db"), 90).unwrap()
}

fn conditions(store: &IncidentStore) -> BTreeSet<String> {
    store.open_incidents().unwrap().into_iter().map(|r| r.1).collect()
}

fn transitions(store: &IncidentStore) -> Vec<String> {
    store.events().unwrap().into_iter().map(|r| r.2).collect()
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

#[test]
fn open_requires_sample_count_and_elapsed_span() {
    let config = incident_config(2, 10.0, 2, 180.0, 600.0);
    let temp = Temp::new();
    let mut store = open(&temp);
    let bad = bad_silence();
    store.record_cycle(&config, std::slice::from_ref(&bad), 100.0, 0.0).unwrap();
    store.record_cycle(&config, std::slice::from_ref(&bad), 105.0, 5.0).unwrap();
    assert!(store.open_incidents().unwrap().is_empty());
    store.record_cycle(&config, &[bad], 110.0, 10.0).unwrap();
    assert_eq!(store.open_incidents().unwrap()[0].1, "audio_silence");
    assert_eq!(transitions(&store), vec!["open"]);
}

#[test]
fn unknown_neither_advances_bad_samples_nor_recovers_an_incident() {
    let config = incident_config(2, 0.0, 2, 180.0, 600.0);
    let temp = Temp::new();
    let mut store = open(&temp);
    store.record_cycle(&config, &[bad_silence()], 100.0, 0.0).unwrap();
    store.record_cycle(&config, &[unknown_both()], 101.0, 1.0).unwrap();
    assert!(store.open_incidents().unwrap().is_empty());
    store.record_cycle(&config, &[bad_silence()], 102.0, 2.0).unwrap();
    assert_eq!(store.open_incidents().unwrap().len(), 1);
    store.record_cycle(&config, &[unknown_both()], 103.0, 3.0).unwrap();
    assert_eq!(store.open_incidents().unwrap().len(), 1);
    assert_eq!(transitions(&store), vec!["open"]);
}

#[test]
fn unknown_warning_is_delayed_and_deduplicated() {
    let config = incident_config(2, 10.0, 2, 180.0, 10.0);
    let temp = Temp::new();
    let mut store = open(&temp);
    store.record_cycle(&config, &[unknown_both()], 100.0, 0.0).unwrap();
    store.record_cycle(&config, &[unknown_both()], 105.0, 5.0).unwrap();
    assert!(store.open_incidents().unwrap().is_empty());
    store.record_cycle(&config, &[unknown_both()], 110.0, 10.0).unwrap();
    store.record_cycle(&config, &[unknown_both()], 120.0, 20.0).unwrap();
    assert_eq!(conditions(&store), set(&["unknown"]));
    assert_eq!(transitions(&store), vec!["open"]);
}

#[test]
fn confirmed_missing_dimension_is_independent_of_overall_unknown() {
    let config = incident_config(1, 0.0, 2, 180.0, 600.0);
    let missing_audio = observation(
        "UNKNOWN",
        Some(unknown_dim("AUDIO_MISSING")),
        Some(DimensionEvidence::new("HEALTHY", &["VIDEO_OK"])),
        &[],
        "2026-10-23T00:00:00Z",
    );
    let temp = Temp::new();
    let mut store = open(&temp);
    store.record_cycle(&config, &[missing_audio], 100.0, 0.0).unwrap();
    assert_eq!(conditions(&store), set(&["audio_missing"]));
    let missing_video = observation(
        "UNKNOWN",
        Some(DimensionEvidence::new("HEALTHY", &["AUDIO_OK"])),
        Some(unknown_dim("VIDEO_MISSING")),
        &[],
        "2026-10-23T00:00:00Z",
    );
    let temp = Temp::new();
    let mut store = open(&temp);
    store.record_cycle(&config, &[missing_video], 100.0, 0.0).unwrap();
    assert_eq!(conditions(&store), set(&["video_missing"]));
}

#[test]
fn stateful_check_persists_before_returning_and_watch_uses_same_store() {
    let config = incident_config(1, 0.0, 2, 180.0, 600.0);
    let temp = Temp::new();
    let mut store = open(&temp);
    let mut cycle = |_c: &AppConfig, _h: &mut HashMap<String, ThumbnailBaseline>| vec![bad_silence()];
    let mut dispatch = |_s: &mut IncidentStore, _c: &AppConfig| Ok(());
    let result = dead_air::watch::check_cycle_stateful(&config, &mut store, &mut cycle, &mut dispatch).unwrap();
    assert_eq!(result[0].status, "UNHEALTHY");
    assert_eq!(transitions(&store), vec!["open"]);

    let temp = Temp::new();
    let mut store = open(&temp);
    let mut sleeper = |_s: f64| {};
    let mut dispatch = |_s: &mut IncidentStore, _c: &AppConfig| Ok(());
    let hooks = dead_air::watch::WatchHooks {
        cycle: &mut cycle,
        sleeper: &mut sleeper,
        clock: &now_mono,
        wall: &dead_air::timefmt::wall,
        record_mono: &now_mono,
        dispatch: &mut dispatch,
    };
    dead_air::watch::watch_cycles(&config, dead_air::watch::WatchMode::Cycles(2), Some(&mut store), hooks, &mut |_r| {
        Ok(())
    })
    .unwrap();
    assert_eq!(transitions(&store), vec!["open"]);
}

#[test]
fn dimension_recovery_is_independent_and_requires_fresh_samples() {
    let config = incident_config(1, 0.0, 2, 180.0, 600.0);
    let both = observation(
        "UNHEALTHY",
        Some(silence()),
        Some(freeze()),
        &["AUDIO_SILENCE", "VIDEO_FREEZE"],
        "2026-10-23T00:00:00Z",
    );
    let audio_bad = bad_silence();
    let temp = Temp::new();
    let mut store = open(&temp);
    store.record_cycle(&config, &[both], 100.0, 0.0).unwrap();
    assert_eq!(conditions(&store), set(&["audio_silence", "video_freeze"]));
    store.record_cycle(&config, std::slice::from_ref(&audio_bad), 101.0, 1.0).unwrap();
    assert_eq!(store.open_incidents().unwrap().len(), 2);
    store.record_cycle(&config, &[audio_bad], 102.0, 2.0).unwrap();
    assert_eq!(conditions(&store), set(&["audio_silence"]));
    assert_eq!(transitions(&store).iter().filter(|t| *t == "recovery").count(), 1);
}

#[test]
fn recovery_uses_healthy_dimension_when_other_dimension_is_unknown() {
    let config = incident_config(1, 0.0, 2, 180.0, 600.0);
    let both = observation(
        "UNHEALTHY",
        Some(silence()),
        Some(freeze()),
        &["AUDIO_SILENCE", "VIDEO_FREEZE"],
        "2026-10-23T00:00:00Z",
    );
    let temp = Temp::new();
    let mut store = open(&temp);
    store.record_cycle(&config, &[both], 100.0, 0.0).unwrap();
    let audio_clean =
        |at: &str| observation("UNKNOWN", Some(healthy_audio()), Some(unknown_dim("VIDEO_UNKNOWN")), &[], at);
    let video_clean =
        |at: &str| observation("UNKNOWN", Some(unknown_dim("AUDIO_UNKNOWN")), Some(healthy_video()), &[], at);
    store.record_cycle(&config, &[audio_clean("2026-10-23T00:00:01Z")], 101.0, 1.0).unwrap();
    assert_eq!(conditions(&store), set(&["audio_silence", "video_freeze"]));
    store.record_cycle(&config, &[audio_clean("2026-10-23T00:00:02Z")], 102.0, 2.0).unwrap();
    assert_eq!(conditions(&store), set(&["video_freeze"]));
    store.record_cycle(&config, &[video_clean("2026-10-23T00:00:03Z")], 103.0, 3.0).unwrap();
    assert_eq!(conditions(&store), set(&["video_freeze"]));
    store.record_cycle(&config, &[video_clean("2026-10-23T00:00:04Z")], 104.0, 4.0).unwrap();
    assert!(store.open_incidents().unwrap().is_empty());
    assert_eq!(transitions(&store).iter().filter(|t| *t == "recovery").count(), 2);
}

fn off_air_observation() -> Observation {
    observation(
        "OFF_AIR",
        Some(unknown_dim("NO_MEDIA_SAMPLE_AFTER_OFF_AIR")),
        Some(unknown_dim("NO_MEDIA_SAMPLE_AFTER_OFF_AIR")),
        &[],
        "2026-10-23T00:00:00Z",
    )
}

#[test]
fn off_air_requires_a_fresh_decoded_dimension_to_recover() {
    let config = incident_config(1, 0.0, 1, 180.0, 600.0);
    let at = "2026-10-23T00:00:00Z";
    let no_decode = observation(
        "UNKNOWN",
        Some(DimensionEvidence::decoded("HEALTHY", &["AUDIO_DETECTOR_DISABLED"], 2.0, 2.0)),
        Some(DimensionEvidence::new("DEGRADED", &["VIDEO_UNVERIFIED_BY_THUMBNAIL"])),
        &[],
        at,
    );
    let contradictory = observation(
        "UNKNOWN",
        Some(DimensionEvidence::decoded("HEALTHY", &["AUDIO_MISSING"], 2.0, 2.0)),
        Some(unknown_dim("VIDEO_UNKNOWN")),
        &[],
        at,
    );
    let stale = observation(
        "UNHEALTHY",
        Some(DimensionEvidence::decoded("UNHEALTHY", &["AUDIO_STALE"], 2.0, 2.0)),
        Some(unknown_dim("VIDEO_UNKNOWN")),
        &[],
        at,
    );
    let fresh = observation("OFF_AIR", Some(healthy_audio()), Some(unknown_dim("VIDEO_MISSING")), &[], at);
    let temp = Temp::new();
    let mut store = open(&temp);
    store.record_cycle(&config, &[off_air_observation()], 100.0, 0.0).unwrap();
    assert!(conditions(&store).contains("off_air"));
    for (i, sample) in [no_decode, contradictory, stale].into_iter().enumerate() {
        store.record_cycle(&config, &[sample], 101.0 + i as f64, 1.0 + i as f64).unwrap();
        assert!(conditions(&store).contains("off_air"), "sample {i} must not recover off-air");
    }
    store.record_cycle(&config, &[fresh], 104.0, 4.0).unwrap();
    assert!(!conditions(&store).contains("off_air"));
}

#[test]
fn off_air_recovers_on_fresh_unhealthy_media_without_resolving_dimension_incidents() {
    let base = incident_config(1, 0.0, 2, 180.0, 600.0);
    let mut audio_only = base.clone();
    audio_only.streams[0].expect_video = false;
    audio_only.streams[0].detect_freeze = false;
    let not_expected = DimensionEvidence::new("NOT_APPLICABLE", &["VIDEO_NOT_EXPECTED"]);
    let at = "2026-10-23T00:00:00Z";
    let cases = [
        (
            base.clone(),
            off_air_observation(),
            observation("UNHEALTHY", Some(silence()), Some(freeze()), &["AUDIO_SILENCE", "VIDEO_FREEZE"], at),
            set(&["audio_silence", "video_freeze"]),
        ),
        (
            audio_only,
            observation(
                "OFF_AIR",
                Some(unknown_dim("NO_MEDIA_SAMPLE_AFTER_OFF_AIR")),
                Some(not_expected.clone()),
                &[],
                at,
            ),
            observation("UNHEALTHY", Some(silence()), Some(not_expected), &["AUDIO_SILENCE"], at),
            set(&["audio_silence"]),
        ),
    ];
    for (config, off_air, recovered, independent) in cases {
        let temp = Temp::new();
        let mut store = open(&temp);
        store.record_cycle(&config, &[off_air], 100.0, 0.0).unwrap();
        assert_eq!(conditions(&store), set(&["off_air"]));
        store.record_cycle(&config, std::slice::from_ref(&recovered), 101.0, 1.0).unwrap();
        let mut expected = independent.clone();
        expected.insert("off_air".into());
        assert_eq!(conditions(&store), expected);
        store.record_cycle(&config, &[recovered], 102.0, 2.0).unwrap();
        assert_eq!(conditions(&store), independent);
        let recoveries: Vec<String> =
            store.events().unwrap().into_iter().filter(|r| r.2 == "recovery").map(|r| r.1).collect();
        assert_eq!(recoveries, vec!["off_air"]);
    }
}

#[test]
fn transition_evidence_survives_restart_and_is_dimension_scoped() {
    let config = incident_config(2, 1.0, 2, 180.0, 600.0);
    let bad = |at: &str| observation("UNHEALTHY", Some(silence()), None, &["AUDIO_SILENCE"], at);
    let good_audio = DimensionEvidence::decoded("HEALTHY", &["AUDIO_DECODED_NO_EVENT"], 2.0, 2.0)
        .with_intervals(vec![Interval { start_s: 0.0, end_s: 2.0, open_at_sample_end: false }]);
    let good = |at: &str| observation("UNHEALTHY", Some(good_audio.clone()), None, &[], at);
    let temp = Temp::new();
    {
        let mut store = open(&temp);
        store.record_cycle(&config, &[bad("2026-10-23T00:00:00Z")], 100.0, 0.0).unwrap();
        store.record_cycle(&config, &[bad("2026-10-23T00:00:02Z")], 102.0, 2.0).unwrap();
        let opened: Value = serde_json::from_str(&store.events().unwrap()[0].3).unwrap();
        assert_eq!(opened["evidence"]["threshold"]["sample_count"], 2);
        assert_eq!(opened["evidence"]["threshold"]["span_s"], 2.0);
        assert!(
            opened["evidence"]["affected_dimensions"]["audio"]["reason_codes"]
                .as_array()
                .unwrap()
                .contains(&"AUDIO_SILENCE".into())
        );
        assert!(opened["evidence"]["affected_dimensions"].get("video").is_none(), "evidence is dimension scoped");
    }
    {
        let mut store = open(&temp);
        store.record_cycle(&config, &[good("2026-10-23T00:00:03Z")], 103.0, 3.0).unwrap();
        store.record_cycle(&config, &[good("2026-10-23T00:00:04Z")], 104.0, 4.0).unwrap();
    }
    let store = open(&temp);
    let events: Vec<Value> = store.events().unwrap().iter().map(|r| serde_json::from_str(&r.3).unwrap()).collect();
    let recovery = events.iter().find(|e| e["transition"] == "recovery").unwrap();
    let evidence = &recovery["evidence"];
    assert_eq!(evidence["threshold"]["sample_count"], 2);
    let times: Vec<&str> =
        evidence["samples"].as_array().unwrap().iter().map(|s| s["observed_at"].as_str().unwrap()).collect();
    assert_eq!(times, vec!["2026-10-23T00:00:03Z", "2026-10-23T00:00:04Z"]);
    assert_eq!(evidence["samples"][0]["dimensions"]["audio"]["decoded_s"], 2.0);
    let serialized = serde_json::to_string(&events).unwrap();
    assert!(!serialized.contains("https://"));
    assert!(!serialized.contains("secret-token"));
}

#[test]
fn restart_resets_pending_evidence_but_keeps_open_incidents() {
    let config = incident_config(2, 0.0, 2, 180.0, 600.0);
    let temp = Temp::new();
    {
        let mut store = open(&temp);
        store.record_cycle(&config, &[bad_silence()], 100.0, 0.0).unwrap();
    }
    let mut store = open(&temp);
    store.record_cycle(&config, &[bad_silence()], 200.0, 1.0).unwrap();
    assert!(store.open_incidents().unwrap().is_empty());
    store.record_cycle(&config, &[bad_silence()], 201.0, 2.0).unwrap();
    assert_eq!(store.open_incidents().unwrap().len(), 1);
    drop(store);
    // The open incident survives a further restart.
    let store = open(&temp);
    assert_eq!(store.open_incidents().unwrap().len(), 1);
}

#[test]
fn clock_jump_and_stale_gap_reset_pending() {
    let config = incident_config(2, 0.0, 2, 5.0, 600.0);
    let temp = Temp::new();
    let mut store = open(&temp);
    store.record_cycle(&config, &[bad_silence()], 100.0, 0.0).unwrap();
    store.record_cycle(&config, &[bad_silence()], 200.0, 1.0).unwrap(); // wall-clock jump
    assert!(store.open_incidents().unwrap().is_empty());
    store.record_cycle(&config, &[bad_silence()], 201.0, 2.0).unwrap();
    assert_eq!(store.open_incidents().unwrap().len(), 1);
    let temp = Temp::new();
    let mut store = open(&temp);
    store.record_cycle(&config, &[bad_silence()], 100.0, 0.0).unwrap();
    store.record_cycle(&config, &[bad_silence()], 110.0, 10.0).unwrap(); // stale observation gap
    assert!(store.open_incidents().unwrap().is_empty());
    store.record_cycle(&config, &[bad_silence()], 111.0, 11.0).unwrap();
    assert_eq!(store.open_incidents().unwrap().len(), 1);
}

#[test]
fn corrupt_database_and_stale_lock_are_preserved() {
    let temp = Temp::new();
    let path = temp.join("state.db");
    let original = b"owner data, not a database";
    std::fs::write(&path, original).unwrap();
    assert!(IncidentStore::open(&path, 90).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), original);
    std::fs::remove_file(&path).unwrap();
    let lock = temp.join(".dead-air.lock");
    std::fs::write(&lock, "stale owner token").unwrap();
    assert_eq!(IncidentStore::open(&path, 90).err().unwrap().0, "STATE_LOCKED");
    assert_eq!(std::fs::read_to_string(&lock).unwrap(), "stale owner token");
    let empty = temp.join("empty.db");
    std::fs::remove_file(&lock).unwrap();
    std::fs::write(&empty, b"").unwrap();
    assert_eq!(IncidentStore::open(&empty, 90).err().unwrap().0, "STATE_CORRUPT");
}

#[test]
fn persistence_failure_is_explicit() {
    let config = incident_config(1, 0.0, 2, 180.0, 600.0);
    let temp = Temp::new();
    let mut store = open(&temp);
    store.db().execute_batch("PRAGMA query_only=ON").unwrap();
    assert_eq!(store.record_cycle(&config, &[bad_silence()], 100.0, 0.0).unwrap_err().0, "PERSISTENCE_WRITE_FAILED");
    let mut unknown_stream = bad_silence();
    unknown_stream.stream_id = "not-configured".into();
    store.db().execute_batch("PRAGMA query_only=OFF").unwrap();
    assert_eq!(store.record_cycle(&config, &[unknown_stream], 100.0, 0.0).unwrap_err().0, "OBSERVATION_STREAM_UNKNOWN");
}

#[test]
fn missing_track_recovers_into_silent_or_frozen_media() {
    let config = incident_config(1, 0.0, 1, 180.0, 600.0);
    let missing = observation(
        "UNKNOWN",
        Some(unknown_dim("AUDIO_MISSING")),
        Some(unknown_dim("VIDEO_MISSING")),
        &[],
        "2026-10-23T00:00:00Z",
    );
    let both = observation(
        "UNHEALTHY",
        Some(silence()),
        Some(freeze()),
        &["AUDIO_SILENCE", "VIDEO_FREEZE"],
        "2026-10-23T00:00:01Z",
    );
    let temp = Temp::new();
    let mut store = open(&temp);
    store.record_cycle(&config, &[missing], 100.0, 0.0).unwrap();
    assert_eq!(conditions(&store), set(&["audio_missing", "video_missing"]));
    store.record_cycle(&config, &[both], 101.0, 1.0).unwrap();
    assert_eq!(conditions(&store), set(&["audio_silence", "video_freeze"]));
}
