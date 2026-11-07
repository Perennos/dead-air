//! SQLite incident ledger and OS-owned writer lifecycle.
//!
//! The on-disk format is the Python 0.2.0 schema 2, unchanged: `stream_state`, `incidents`,
//! `events` and `deliveries`, plus an optional `tempo_briefs` table that older releases
//! ignore. Each database has one writer, enforced by an OS lock on `<state>.lock`
//! (`flock` on POSIX, `LockFileEx` on byte 0 on Windows, compatible with the Python
//! release), and the directory's persistent `.dead-air.lock` guard keeps 0.1.0 writers out.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde_json::{Map, Value, json};

use crate::alerts::{DeliveryLedger, DueDelivery};
use crate::constants::{LEGACY_GUARD, SCHEMA};
use crate::model::{AppConfig, DestinationConfig, Observation, PersistenceError, StreamConfig, observation_json};
use crate::observe::incident_signals;
use crate::pyjson::{self, float};

type Result<T> = std::result::Result<T, PersistenceError>;

fn fail(code: &str) -> PersistenceError {
    PersistenceError::new(code)
}

fn write_failed<E>(_: E) -> PersistenceError {
    fail("PERSISTENCE_WRITE_FAILED")
}

fn is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path).map(|m| m.file_type().is_symlink()).unwrap_or(false)
}

/// An exclusive, crash-released OS lock on `<state>.lock`. The file itself is never deleted.
pub struct StateLock {
    file: File,
}

impl StateLock {
    fn open_file(path: &Path, create: bool) -> std::io::Result<File> {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(create);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        options.open(path)
    }

    #[cfg(unix)]
    fn try_lock(file: &File) -> bool {
        use std::os::unix::io::AsRawFd;
        // SAFETY: flock on a descriptor we own.
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
    }

    #[cfg(windows)]
    fn try_lock(file: &File) -> bool {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx};
        use windows_sys::Win32::System::IO::OVERLAPPED;
        // SAFETY: an exclusive, non-blocking lock on byte 0 of a handle we own, the same
        // range Python's msvcrt.locking(LK_NBLCK, 1) locks.
        unsafe {
            let mut overlapped: OVERLAPPED = std::mem::zeroed();
            LockFileEx(
                file.as_raw_handle() as _,
                LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
                0,
                1,
                0,
                &mut overlapped,
            ) != 0
        }
    }

    #[cfg(unix)]
    fn unlock(file: &File) {
        use std::os::unix::io::AsRawFd;
        // SAFETY: unlocking a descriptor we own.
        unsafe {
            libc::flock(file.as_raw_fd(), libc::LOCK_UN);
        }
    }

    #[cfg(windows)]
    fn unlock(file: &File) {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::UnlockFileEx;
        use windows_sys::Win32::System::IO::OVERLAPPED;
        // SAFETY: releasing the byte-0 lock taken above.
        unsafe {
            let mut overlapped: OVERLAPPED = std::mem::zeroed();
            UnlockFileEx(file.as_raw_handle() as _, 0, 1, 0, &mut overlapped);
        }
    }

    /// Take the writer lock for a state database.
    pub fn acquire(state_path: &Path) -> Result<Self> {
        let path = lock_path(state_path);
        if is_symlink(&path) {
            return Err(fail("STATE_PATH_UNSAFE"));
        }
        let mut file = Self::open_file(&path, true).map_err(|_| fail("STATE_UNAVAILABLE"))?;
        #[cfg(windows)]
        {
            if file.metadata().map(|m| m.len() == 0).unwrap_or(false) {
                let _ = file.write_all(b"\0");
            }
        }
        let _ = &mut file;
        if !Self::try_lock(&file) {
            return Err(fail("STATE_LOCKED"));
        }
        Ok(StateLock { file })
    }

    /// True when another process holds the writer lock. Never creates the lock file.
    pub fn is_held(state_path: &Path) -> bool {
        let path = lock_path(state_path);
        let Ok(file) = Self::open_file(&path, false) else {
            return false;
        };
        if Self::try_lock(&file) {
            Self::unlock(&file);
            false
        } else {
            true
        }
    }
}

impl Drop for StateLock {
    fn drop(&mut self) {
        Self::unlock(&self.file);
        // Keep the inode: unlinking a lock file lets competing processes lock different
        // files at the same path and both believe they own the database.
    }
}

pub fn lock_path(state_path: &Path) -> PathBuf {
    let mut name = state_path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".lock");
    state_path.with_file_name(name)
}

pub fn guard_path(state_path: &Path) -> PathBuf {
    state_path.parent().unwrap_or(Path::new(".")).join(".dead-air.lock")
}

#[cfg(test)]
thread_local! {
    pub static FAIL_GUARD_LINK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn verify_guard(path: &Path) -> Result<()> {
    if is_symlink(path) {
        return Err(fail("STATE_PATH_UNSAFE"));
    }
    let mut contents = Vec::new();
    File::open(path)
        .and_then(|f| f.take(LEGACY_GUARD.len() as u64 + 1).read_to_end(&mut contents))
        .map_err(|_| fail("STATE_UNAVAILABLE"))?;
    if contents != LEGACY_GUARD { Err(fail("STATE_LOCKED")) } else { Ok(()) }
}

/// Publish the persistent compatibility guard with an fsynced temporary inode and an
/// atomic hard link, so a crash can never leave a partial guard behind.
fn guard_legacy_writers(directory: &Path) -> Result<()> {
    let path = directory.join(".dead-air.lock");
    if path.exists() || is_symlink(&path) {
        return verify_guard(&path);
    }
    let temporary = directory.join(format!(".dead-air-guard-{}", &crate::random::uuid4_hex()[..12]));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let outcome = (|| -> Result<()> {
        let mut file = options.open(&temporary).map_err(|_| fail("STATE_UNAVAILABLE"))?;
        file.write_all(LEGACY_GUARD).and_then(|_| file.sync_all()).map_err(|_| fail("STATE_UNAVAILABLE"))?;
        drop(file);
        #[cfg(test)]
        if FAIL_GUARD_LINK.with(|f| f.get()) {
            return Err(fail("STATE_UNAVAILABLE"));
        }
        match std::fs::hard_link(&temporary, &path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => verify_guard(&path),
            Err(_) => Err(fail("STATE_UNAVAILABLE")),
        }
    })();
    let _ = std::fs::remove_file(&temporary);
    outcome
}

const CREATE_SCHEMA: &str = "
    BEGIN EXCLUSIVE;
    CREATE TABLE stream_state(stream_id TEXT PRIMARY KEY, state_json TEXT NOT NULL);
    CREATE TABLE incidents(
        incident_id TEXT PRIMARY KEY, stream_id TEXT NOT NULL, condition TEXT NOT NULL,
        opened_at TEXT NOT NULL, opened_wall REAL NOT NULL, closed_at TEXT,
        closed_wall REAL, status TEXT NOT NULL
    );
    CREATE UNIQUE INDEX one_open_incident ON incidents(stream_id, condition) WHERE closed_wall IS NULL;
    CREATE TABLE events(
        event_id TEXT PRIMARY KEY, incident_id TEXT NOT NULL REFERENCES incidents(incident_id),
        stream_id TEXT NOT NULL, condition TEXT NOT NULL, transition TEXT NOT NULL,
        created_wall REAL NOT NULL, payload_json TEXT NOT NULL
    );
    CREATE TABLE deliveries(
        event_id TEXT NOT NULL REFERENCES events(event_id) ON DELETE CASCADE,
        destination_id TEXT NOT NULL, status TEXT NOT NULL, attempts INTEGER NOT NULL,
        created_wall REAL NOT NULL, next_attempt_wall REAL, last_error TEXT,
        last_http_status INTEGER, updated_wall REAL NOT NULL,
        acknowledged_at TEXT, acknowledged_wall REAL, acknowledged_by TEXT,
        PRIMARY KEY(event_id, destination_id)
    );
    PRAGMA user_version=2;
    COMMIT;
";

/// The optional brief table (Perennos Tempo). Older releases ignore it.
pub const CREATE_BRIEFS: &str = "CREATE TABLE IF NOT EXISTS tempo_briefs(
    incident_id TEXT PRIMARY KEY, created_wall REAL NOT NULL, created_at TEXT NOT NULL,
    model TEXT NOT NULL, task_class TEXT NOT NULL, receipt_id TEXT, job_id TEXT,
    review_required INTEGER, brief TEXT NOT NULL)";

pub fn table_exists(db: &Connection, name: &str) -> rusqlite::Result<bool> {
    db.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?", [name], |r| r.get::<_, i64>(0))
        .map(|n| n > 0)
}

pub struct IncidentStore {
    pub path: PathBuf,
    retention_days: i64,
    db: Option<Connection>,
    _lock: Option<StateLock>,
}

/// `(stream_id, condition, opened_at)`.
pub type OpenIncidentRow = (String, String, String);
/// `(event_id, condition, transition, payload_json)`.
pub type EventRow = (String, String, String, String);
/// `(event_id, destination_id, status, attempts, last_error, last_http_status)`.
pub type DeliveryRow = (String, String, String, i64, Option<String>, Option<i64>);

impl IncidentStore {
    /// Open (or create) the writer's database, taking its lock and resetting pending counters.
    pub fn open(path: impl AsRef<Path>, retention_days: i64) -> Result<Self> {
        let path = crate::config::absolute(path.as_ref());
        let directory = path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
        create_private_dir(&directory).map_err(|_| fail("STATE_UNAVAILABLE"))?;
        if is_symlink(&directory) || is_symlink(&path) {
            return Err(fail("STATE_PATH_UNSAFE"));
        }
        guard_legacy_writers(&directory)?;
        let lock = StateLock::acquire(&path)?;
        let existed = path.exists();
        if existed && std::fs::metadata(&path).map(|m| m.len() == 0).unwrap_or(false) {
            return Err(fail("STATE_CORRUPT"));
        }
        let db = Connection::open(&path).map_err(|_| fail("STATE_CORRUPT"))?;
        if !existed {
            restrict_file(&path);
        }
        let mut store = IncidentStore { path, retention_days, db: Some(db), _lock: Some(lock) };
        store.initialise(existed)?;
        Ok(store)
    }

    fn initialise(&mut self, existed: bool) -> Result<()> {
        let db = self.db.as_mut().expect("open");
        let corrupt = |_| fail("STATE_CORRUPT");
        db.execute_batch("PRAGMA foreign_keys=ON").map_err(corrupt)?;
        db.busy_timeout(std::time::Duration::from_secs(5)).map_err(corrupt)?;
        let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0)).map_err(corrupt)?;
        if !existed {
            db.execute_batch(CREATE_SCHEMA).map_err(corrupt)?;
        } else if version == 1 {
            migrate_schema_1(db)?;
        } else if version != SCHEMA {
            return Err(fail("STATE_SCHEMA_UNSUPPORTED"));
        }
        let check: String = db.query_row("PRAGMA quick_check", [], |r| r.get(0)).map_err(corrupt)?;
        if check != "ok" {
            return Err(fail("STATE_CORRUPT"));
        }
        // Monotonic counters cannot cross process lifetimes. Keep the last observation for
        // readers, but never use it as new transition evidence.
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(corrupt)?;
        let rows: Vec<(String, String)> = {
            let mut statement = tx.prepare("SELECT stream_id,state_json FROM stream_state").map_err(corrupt)?;
            statement
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(corrupt)?
                .collect::<rusqlite::Result<_>>()
                .map_err(corrupt)?
        };
        for (stream_id, raw) in rows {
            let state: Value = serde_json::from_str(&raw).map_err(|_| fail("STATE_UNAVAILABLE"))?;
            let Value::Object(state) = state else {
                return Err(fail("STATE_UNAVAILABLE"));
            };
            let mut saved = Map::new();
            for key in ["latest", "last_observed_wall", "config_fingerprint"] {
                if let Some(value) = state.get(key) {
                    saved.insert(key.into(), value.clone());
                }
            }
            saved.insert("version".into(), json!(SCHEMA));
            saved.insert("pending".into(), json!({}));
            saved.insert("recoveries".into(), json!({}));
            tx.execute(
                "UPDATE stream_state SET state_json=? WHERE stream_id=?",
                params![pyjson::dumps_compact(&Value::Object(saved)), stream_id],
            )
            .map_err(corrupt)?;
        }
        tx.commit().map_err(corrupt)?;
        Ok(())
    }

    pub fn db(&self) -> &Connection {
        self.db.as_ref().expect("store open")
    }

    pub fn retention_days(&self) -> i64 {
        self.retention_days
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_event(
        tx: &rusqlite::Transaction<'_>,
        stream: &StreamConfig,
        incident_id: &str,
        condition: &str,
        transition: &str,
        observation: &Observation,
        now_wall: f64,
        destinations: &[DestinationConfig],
        evidence: Value,
    ) -> Result<()> {
        let event_id = crate::random::uuid4_hex();
        let payload = json!({
            "event_id": event_id, "incident_id": incident_id, "stream_id": stream.id,
            "condition": condition, "transition": transition,
            "observed_at": observation.observed_at, "status": observation.status,
            "reason_codes": observation.reason_codes, "evidence": evidence,
        });
        tx.execute(
            "INSERT INTO events VALUES(?,?,?,?,?,?,?)",
            params![event_id, incident_id, stream.id, condition, transition, now_wall, pyjson::dumps_compact(&payload)],
        )
        .map_err(write_failed)?;
        for destination in destinations {
            tx.execute(
                "INSERT INTO deliveries(event_id,destination_id,status,attempts,created_wall,next_attempt_wall,last_error,last_http_status,updated_wall) \
                 VALUES(?,?,'pending',0,?,?,NULL,NULL,?)",
                params![event_id, destination.id, now_wall, now_wall, now_wall],
            )
            .map_err(write_failed)?;
        }
        Ok(())
    }

    /// Record one cycle's observations in a single transaction: condition transitions,
    /// events, pending deliveries, the latest snapshot and retention pruning.
    pub fn record_cycle(
        &mut self,
        config: &AppConfig,
        observations: &[Observation],
        now_wall: f64,
        now_mono: f64,
    ) -> Result<()> {
        let retention = self.retention_days;
        let db = self.db.as_mut().ok_or_else(|| fail("STATE_CLOSED"))?;
        let streams: HashMap<&str, &StreamConfig> = config.streams.iter().map(|s| (s.id.as_str(), s)).collect();
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(write_failed)?;
        for observation in observations {
            let stream =
                *streams.get(observation.stream_id.as_str()).ok_or_else(|| fail("OBSERVATION_STREAM_UNKNOWN"))?;
            let raw: Option<String> = tx
                .query_row("SELECT state_json FROM stream_state WHERE stream_id=?", [&stream.id], |r| r.get(0))
                .optional()
                .map_err(write_failed)?;
            let mut state: Map<String, Value> = match raw {
                Some(raw) => match serde_json::from_str(&raw).map_err(write_failed)? {
                    Value::Object(map) => map,
                    _ => return Err(fail("PERSISTENCE_WRITE_FAILED")),
                },
                None => {
                    let mut map = Map::new();
                    map.insert("version".into(), json!(SCHEMA));
                    map.insert("pending".into(), json!({}));
                    map.insert("recoveries".into(), json!({}));
                    map
                }
            };
            if state.get("version").and_then(Value::as_i64) != Some(SCHEMA) {
                return Err(fail("STATE_ROW_SCHEMA_UNSUPPORTED"));
            }
            let mut pending = match state.remove("pending") {
                Some(Value::Object(map)) => map,
                None => Map::new(),
                Some(_) => return Err(fail("PERSISTENCE_WRITE_FAILED")),
            };
            let mut recoveries = match state.remove("recoveries") {
                Some(Value::Object(map)) => map,
                None => Map::new(),
                Some(_) => return Err(fail("PERSISTENCE_WRITE_FAILED")),
            };
            if let Some(previous_wall) = state.get("last_wall").and_then(Value::as_f64) {
                let previous_mono =
                    state.get("last_mono").and_then(Value::as_f64).ok_or_else(|| fail("PERSISTENCE_WRITE_FAILED"))?;
                let wall_delta = now_wall - previous_wall;
                let mono_delta = now_mono - previous_mono;
                if wall_delta < 0.0
                    || mono_delta < 0.0
                    || (wall_delta - mono_delta).abs() > 5.0
                    || mono_delta > stream.max_gap_s
                {
                    pending.clear();
                    recoveries.clear();
                }
            }
            for (condition, signal) in incident_signals(observation) {
                let incident: Option<String> = tx
                    .query_row(
                        "SELECT incident_id FROM incidents WHERE stream_id=? AND condition=? AND closed_wall IS NULL",
                        params![stream.id, condition],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(write_failed)?;
                if let Some(incident_id) = incident {
                    if signal == "good" {
                        let mut prior = match recoveries.get(condition) {
                            Some(Value::Object(map)) => map.clone(),
                            _ => {
                                let mut map = Map::new();
                                map.insert("count".into(), json!(0));
                                map.insert("first_mono".into(), float(now_mono));
                                map.insert("samples".into(), json!([]));
                                map
                            }
                        };
                        let count = prior.get("count").and_then(Value::as_i64).unwrap_or(0) + 1;
                        prior.insert("count".into(), json!(count));
                        let sample = crate::model::transition_sample(condition, observation);
                        match prior.get_mut("samples") {
                            Some(Value::Array(samples)) => samples.push(sample),
                            _ => {
                                prior.insert("samples".into(), json!([sample]));
                            }
                        }
                        if count >= stream.recovery_samples {
                            tx.execute(
                                "UPDATE incidents SET closed_at=?,closed_wall=?,status='resolved' WHERE incident_id=?",
                                params![observation.observed_at, now_wall, incident_id],
                            )
                            .map_err(write_failed)?;
                            let first_mono = prior.get("first_mono").and_then(Value::as_f64).unwrap_or(now_mono);
                            let samples: Vec<Value> =
                                prior.get("samples").and_then(Value::as_array).cloned().unwrap_or_default();
                            let evidence = crate::model::transition_evidence(
                                condition,
                                observation,
                                json!({
                                    "sample_count": count,
                                    "required_samples": stream.recovery_samples,
                                    "span_s": float((now_mono - first_mono).max(0.0)),
                                    "required_span_s": float(0.0),
                                }),
                                &samples,
                            );
                            Self::insert_event(
                                &tx,
                                stream,
                                &incident_id,
                                condition,
                                "recovery",
                                observation,
                                now_wall,
                                &config.destinations,
                                evidence,
                            )?;
                            recoveries.remove(condition);
                        } else {
                            recoveries.insert(condition.into(), Value::Object(prior));
                        }
                    } else {
                        recoveries.remove(condition);
                    }
                    continue;
                }
                if signal == "good" {
                    pending.remove(condition);
                } else if signal == "bad" {
                    let mut prior = match pending.get(condition) {
                        Some(Value::Object(map)) => Some(map.clone()),
                        _ => None,
                    };
                    if let Some(map) = &prior {
                        let last_mono = map.get("last_mono").and_then(Value::as_f64).unwrap_or(now_mono);
                        if now_mono - last_mono > stream.max_gap_s {
                            prior = None;
                        }
                    }
                    let mut prior = prior.unwrap_or_else(|| {
                        let mut map = Map::new();
                        map.insert("count".into(), json!(0));
                        map.insert("first_wall".into(), float(now_wall));
                        map.insert("first_mono".into(), float(now_mono));
                        map
                    });
                    let count = prior.get("count").and_then(Value::as_i64).unwrap_or(0) + 1;
                    prior.insert("count".into(), json!(count));
                    prior.insert("last_wall".into(), float(now_wall));
                    prior.insert("last_mono".into(), float(now_mono));
                    let first_mono = prior.get("first_mono").and_then(Value::as_f64).unwrap_or(now_mono);
                    let (threshold, required) = if condition == "unknown" || condition == "degraded" {
                        let threshold = stream.unknown_notice_after_s;
                        (threshold, if threshold == 0.0 { 1 } else { 2 })
                    } else {
                        (stream.unhealthy_after_s, stream.min_bad_samples)
                    };
                    if count >= required && now_mono - first_mono >= threshold {
                        let incident_id = crate::random::uuid4_hex();
                        tx.execute(
                            "INSERT INTO incidents VALUES(?,?,?,?,?,NULL,NULL,'open')",
                            params![incident_id, stream.id, condition, observation.observed_at, now_wall],
                        )
                        .map_err(write_failed)?;
                        let evidence = crate::model::transition_evidence(
                            condition,
                            observation,
                            json!({
                                "sample_count": count,
                                "required_samples": required,
                                "span_s": float((now_mono - first_mono).max(0.0)),
                                "required_span_s": float(threshold),
                            }),
                            &[],
                        );
                        Self::insert_event(
                            &tx,
                            stream,
                            &incident_id,
                            condition,
                            "open",
                            observation,
                            now_wall,
                            &config.destinations,
                            evidence,
                        )?;
                        pending.remove(condition);
                    } else {
                        pending.insert(condition.into(), Value::Object(prior));
                    }
                }
                // Unknown evidence neither advances nor resolves another condition.
            }
            state.insert("pending".into(), Value::Object(pending));
            state.insert("recoveries".into(), Value::Object(recoveries));
            state.insert("last_wall".into(), float(now_wall));
            state.insert("last_mono".into(), float(now_mono));
            state.insert("latest".into(), observation_json(observation));
            let observed = crate::timefmt::parse_iso(&observation.observed_at).flatten();
            state.insert("last_observed_wall".into(), observed.map(float).unwrap_or(Value::Null));
            state.insert("config_fingerprint".into(), json!(stream.fingerprint()));
            tx.execute(
                "INSERT INTO stream_state VALUES(?,?) ON CONFLICT(stream_id) DO UPDATE SET state_json=excluded.state_json",
                params![stream.id, pyjson::dumps_compact(&Value::Object(state))],
            )
            .map_err(write_failed)?;
        }
        prune(&tx, now_wall, retention).map_err(write_failed)?;
        tx.commit().map_err(write_failed)
    }

    /// Retention: resolved history only after all delivery rows are sent or acknowledged.
    pub fn prune(&self, now_wall: f64) -> Result<()> {
        prune(self.db(), now_wall, self.retention_days).map_err(write_failed)
    }

    pub fn open_incidents(&self) -> Result<Vec<OpenIncidentRow>> {
        let mut statement = self
            .db()
            .prepare("SELECT stream_id,condition,opened_at FROM incidents WHERE closed_wall IS NULL ORDER BY condition")
            .map_err(|_| fail("PERSISTENCE_READ_FAILED"))?;
        statement
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .and_then(|rows| rows.collect())
            .map_err(|_| fail("PERSISTENCE_READ_FAILED"))
    }

    pub fn events(&self) -> Result<Vec<EventRow>> {
        let mut statement = self
            .db()
            .prepare("SELECT event_id,condition,transition,payload_json FROM events ORDER BY created_wall,event_id")
            .map_err(|_| fail("PERSISTENCE_READ_FAILED"))?;
        statement
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .and_then(|rows| rows.collect())
            .map_err(|_| fail("PERSISTENCE_READ_FAILED"))
    }

    pub fn deliveries(&self) -> Result<Vec<DeliveryRow>> {
        let mut statement = self
            .db()
            .prepare(
                "SELECT event_id,destination_id,status,attempts,last_error,last_http_status FROM deliveries ORDER BY event_id,destination_id",
            )
            .map_err(|_| fail("PERSISTENCE_READ_FAILED"))?;
        statement
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))
            .and_then(|rows| rows.collect())
            .map_err(|_| fail("PERSISTENCE_READ_FAILED"))
    }

    /// The `open_incidents` and `deliveries` block of `check --json`.
    pub fn diagnostics(&self) -> Result<Value> {
        let incidents: Vec<Value> = self
            .open_incidents()?
            .into_iter()
            .map(|(sid, condition, at)| json!({"stream_id": sid, "condition": condition, "opened_at": at}))
            .collect();
        let mut counts = Map::new();
        for key in ["pending", "sent", "failed"] {
            counts.insert(key.into(), json!(0));
        }
        let mut statement = self
            .db()
            .prepare("SELECT status,COUNT(*) FROM deliveries GROUP BY status")
            .map_err(|_| fail("PERSISTENCE_READ_FAILED"))?;
        let rows: Vec<(String, i64)> = statement
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .and_then(|rows| rows.collect())
            .map_err(|_| fail("PERSISTENCE_READ_FAILED"))?;
        for (status, count) in rows {
            counts.insert(status, json!(count));
        }
        Ok(json!({"open_incidents": incidents, "deliveries": counts}))
    }

    pub fn acknowledge_failed_delivery(
        &self,
        event_id: &str,
        destination_id: &str,
        operator: &str,
        acknowledged_at: &str,
        now_wall: f64,
    ) -> std::result::Result<&'static str, AckError> {
        acknowledge_on(self.db(), event_id, destination_id, operator, acknowledged_at, now_wall)
    }

    pub fn close(mut self) {
        self.db = None;
        self._lock = None;
    }
}

impl DeliveryLedger for IncidentStore {
    fn due_deliveries(&mut self, now_wall: f64, limit: usize, destination_ids: &[String]) -> Result<Vec<DueDelivery>> {
        if destination_ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; destination_ids.len()].join(",");
        let sql = format!(
            "SELECT d.event_id,d.destination_id,d.attempts,d.created_wall,e.payload_json \
             FROM deliveries d JOIN events e ON e.event_id=d.event_id \
             WHERE d.status='pending' AND d.attempts<3 AND d.next_attempt_wall<=? \
             AND d.destination_id IN ({placeholders}) \
             ORDER BY d.next_attempt_wall,d.event_id,d.destination_id LIMIT ?"
        );
        let mut values: Vec<rusqlite::types::Value> = vec![now_wall.into()];
        values.extend(destination_ids.iter().map(|id| rusqlite::types::Value::Text(id.clone())));
        values.push((limit as i64).into());
        let mut statement = self.db().prepare(&sql).map_err(|_| fail("PERSISTENCE_READ_FAILED"))?;
        statement
            .query_map(rusqlite::params_from_iter(values), |r| {
                Ok(DueDelivery {
                    event_id: r.get(0)?,
                    destination_id: r.get(1)?,
                    attempts: r.get(2)?,
                    created_wall: r.get(3)?,
                    payload_json: r.get(4)?,
                })
            })
            .and_then(|rows| rows.collect())
            .map_err(|_| fail("PERSISTENCE_READ_FAILED"))
    }

    fn record_delivery(
        &mut self,
        row: &DueDelivery,
        success: bool,
        reason: Option<&str>,
        http_status: Option<u16>,
        now_wall: f64,
        retry_after_s: Option<f64>,
    ) -> Result<()> {
        let count = row.attempts + 1;
        let status = if success {
            "sent"
        } else if count >= 3 {
            "failed"
        } else {
            "pending"
        };
        let base_delay = if count == 1 { 30.0 } else { 120.0 };
        let server_delay = retry_after_s.filter(|v| v.is_finite()).map(|v| v.clamp(0.0, 120.0)).unwrap_or(0.0);
        let next_due: Option<f64> = if success || count >= 3 {
            None
        } else {
            Some((row.created_wall + base_delay).max(now_wall + server_delay))
        };
        self.db()
            .execute(
                "UPDATE deliveries SET status=?,attempts=?,next_attempt_wall=?,last_error=?,last_http_status=?,updated_wall=? \
                 WHERE event_id=? AND destination_id=?",
                params![
                    status,
                    count,
                    next_due,
                    if success { None } else { reason },
                    http_status,
                    now_wall,
                    row.event_id,
                    row.destination_id
                ],
            )
            .map(|_| ())
            .map_err(write_failed)
    }
}

fn prune(db: &Connection, now_wall: f64, retention_days: i64) -> rusqlite::Result<()> {
    let cutoff = now_wall - retention_days as f64 * 86_400.0;
    db.execute(
        "DELETE FROM events WHERE created_wall < ? \
         AND incident_id IN (SELECT incident_id FROM incidents WHERE closed_wall IS NOT NULL) \
         AND NOT EXISTS (SELECT 1 FROM deliveries d WHERE d.event_id=events.event_id \
                         AND d.status!='sent' AND d.acknowledged_at IS NULL)",
        [cutoff],
    )?;
    db.execute(
        "DELETE FROM incidents WHERE closed_wall IS NOT NULL AND closed_wall < ? \
         AND NOT EXISTS (SELECT 1 FROM events e WHERE e.incident_id=incidents.incident_id)",
        [cutoff],
    )?;
    if table_exists(db, "tempo_briefs")? {
        db.execute("DELETE FROM tempo_briefs WHERE incident_id NOT IN (SELECT incident_id FROM incidents)", [])?;
    }
    Ok(())
}

fn migrate_schema_1(db: &mut Connection) -> Result<()> {
    let outcome = (|| -> rusqlite::Result<()> {
        let tx = db.transaction_with_behavior(TransactionBehavior::Exclusive)?;
        tx.execute_batch(
            "ALTER TABLE deliveries ADD COLUMN acknowledged_at TEXT;
             ALTER TABLE deliveries ADD COLUMN acknowledged_wall REAL;
             ALTER TABLE deliveries ADD COLUMN acknowledged_by TEXT;
             PRAGMA user_version=2;",
        )?;
        tx.commit()
    })();
    outcome.map_err(|_| fail("STATE_MIGRATION_FAILED"))
}

fn create_private_dir(directory: &Path) -> std::io::Result<()> {
    if directory.is_dir() {
        return Ok(());
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(directory)
}

fn restrict_file(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(windows)]
    {
        let _ = path;
    }
}

/// Acknowledgment input errors (shown as `{"ok": false, "error": ...}`) or persistence errors.
#[derive(Debug, Clone, PartialEq)]
pub enum AckError {
    Invalid(String),
    Persistence(PersistenceError),
}

impl std::fmt::Display for AckError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AckError::Invalid(message) => f.write_str(message),
            AckError::Persistence(error) => f.write_str(&error.0),
        }
    }
}

fn is_printable(text: &str) -> bool {
    text.chars().all(|c| !c.is_control() && !matches!(c, '\u{2028}' | '\u{2029}' | '\u{85}' | '\u{a0}' | '\u{ad}'))
}

fn acknowledge_on(
    db: &Connection,
    event_id: &str,
    destination_id: &str,
    operator: &str,
    acknowledged_at: &str,
    now_wall: f64,
) -> std::result::Result<&'static str, AckError> {
    let invalid = |m: &str| Err(AckError::Invalid(m.into()));
    if event_id.is_empty() || event_id.len() > 64 || !event_id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return invalid("event_id must be a bounded ASCII identifier");
    }
    if destination_id.is_empty()
        || destination_id.len() > 64
        || !destination_id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
    {
        return invalid("destination_id must be a bounded lowercase identifier");
    }
    if operator.trim().is_empty() || operator.chars().count() > 80 || !is_printable(operator) {
        return invalid("operator must be a printable label of 1 to 80 characters");
    }
    if acknowledged_at.chars().count() > 40 {
        return invalid("acknowledged_at must be an ISO timestamp");
    }
    let parsed = crate::timefmt::parse_iso(acknowledged_at);
    let Some(parsed) = parsed else {
        return invalid("acknowledged_at must be an ISO timestamp");
    };
    if parsed.is_none() || !now_wall.is_finite() {
        return invalid("acknowledgment time must include a timezone and finite epoch");
    }
    let outcome = (|| -> rusqlite::Result<&'static str> {
        db.execute_batch("BEGIN IMMEDIATE")?;
        let row: Option<(String, Option<String>)> = db
            .query_row(
                "SELECT status,acknowledged_at FROM deliveries WHERE event_id=? AND destination_id=?",
                params![event_id, destination_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let result = match row {
            None => "not_found",
            Some((status, _)) if status != "failed" => "not_failed",
            Some((_, Some(_))) => "already_acknowledged",
            Some(_) => {
                let changed = db.execute(
                    "UPDATE deliveries SET acknowledged_at=?,acknowledged_wall=?,acknowledged_by=? \
                     WHERE event_id=? AND destination_id=? AND status='failed' AND acknowledged_at IS NULL",
                    params![acknowledged_at, now_wall, operator.trim(), event_id, destination_id],
                )?;
                if changed != 1 {
                    return Err(rusqlite::Error::StatementChangedRows(changed));
                }
                "acknowledged"
            }
        };
        db.execute_batch("COMMIT")?;
        Ok(result)
    })();
    outcome.map_err(|_| {
        let _ = db.execute_batch("ROLLBACK");
        AckError::Persistence(fail("PERSISTENCE_WRITE_FAILED"))
    })
}

/// Acknowledge one terminal failure without resetting a running watcher's counters.
///
/// Schema 2 uses a short SQLite transaction that touches only delivery metadata, so it can
/// run beside an active watcher. Schema 1 migrates through the normal exclusive writer path.
pub fn acknowledge_failed_delivery(
    path: &Path,
    event_id: &str,
    destination_id: &str,
    operator: &str,
    acknowledged_at: &str,
    now_wall: f64,
    retention_days: i64,
) -> std::result::Result<&'static str, AckError> {
    let state_path = crate::config::absolute(path);
    let unsafe_path = state_path.parent().map(is_symlink).unwrap_or(false) || is_symlink(&state_path);
    if unsafe_path {
        return Err(AckError::Persistence(fail("STATE_PATH_UNSAFE")));
    }
    let size = match std::fs::metadata(&state_path) {
        Ok(meta) => meta.len(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok("not_found"),
        Err(_) => return Err(AckError::Persistence(fail("STATE_UNAVAILABLE"))),
    };
    if size == 0 {
        return Err(AckError::Persistence(fail("STATE_CORRUPT")));
    }
    let read_failed = |_| AckError::Persistence(fail("PERSISTENCE_READ_FAILED"));
    let db =
        Connection::open_with_flags(&state_path, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX)
            .map_err(read_failed)?;
    db.busy_timeout(std::time::Duration::from_secs(5)).map_err(read_failed)?;
    let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0)).map_err(read_failed)?;
    if version == 1 {
        drop(db);
        // An old active writer cannot safely see the new columns. The writer lock makes the
        // migration wait until it stops.
        let store = IncidentStore::open(&state_path, retention_days).map_err(AckError::Persistence)?;
        return store.acknowledge_failed_delivery(event_id, destination_id, operator, acknowledged_at, now_wall);
    }
    if version != SCHEMA {
        return Err(AckError::Persistence(fail("STATE_SCHEMA_UNSUPPORTED")));
    }
    acknowledge_on(&db, event_id, destination_id, operator, acknowledged_at, now_wall)
}

/// One stored Tempo brief.
#[derive(Clone, Debug, PartialEq)]
pub struct BriefRecord {
    pub incident_id: String,
    pub created_wall: f64,
    pub created_at: String,
    pub model: String,
    pub task_class: String,
    pub receipt_id: Option<String>,
    pub job_id: Option<String>,
    pub review_required: Option<bool>,
    pub brief: String,
}

/// Store (or replace) a brief in a short transaction beside a running watcher. The incident
/// must exist; nothing else in the database is touched.
pub fn save_brief(path: &Path, record: &BriefRecord) -> Result<()> {
    let state_path = crate::config::absolute(path);
    if is_symlink(&state_path) || state_path.parent().map(is_symlink).unwrap_or(false) {
        return Err(fail("STATE_PATH_UNSAFE"));
    }
    if !state_path.is_file() {
        return Err(fail("STATE_UNAVAILABLE"));
    }
    let db =
        Connection::open_with_flags(&state_path, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX)
            .map_err(write_failed)?;
    db.busy_timeout(std::time::Duration::from_secs(5)).map_err(write_failed)?;
    let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0)).map_err(write_failed)?;
    if version != SCHEMA && version != 1 {
        return Err(fail("STATE_SCHEMA_UNSUPPORTED"));
    }
    let outcome = (|| -> rusqlite::Result<bool> {
        db.execute_batch("BEGIN IMMEDIATE")?;
        db.execute_batch(CREATE_BRIEFS)?;
        let exists: i64 =
            db.query_row("SELECT COUNT(*) FROM incidents WHERE incident_id=?", [&record.incident_id], |r| r.get(0))?;
        if exists == 0 {
            db.execute_batch("ROLLBACK")?;
            return Ok(false);
        }
        db.execute(
            "INSERT OR REPLACE INTO tempo_briefs VALUES(?,?,?,?,?,?,?,?,?)",
            params![
                record.incident_id,
                record.created_wall,
                record.created_at,
                record.model,
                record.task_class,
                record.receipt_id,
                record.job_id,
                record.review_required.map(i64::from),
                record.brief
            ],
        )?;
        db.execute_batch("COMMIT")?;
        Ok(true)
    })();
    match outcome {
        Ok(true) => Ok(()),
        Ok(false) => Err(fail("INCIDENT_NOT_FOUND")),
        Err(_) => {
            let _ = db.execute_batch("ROLLBACK");
            Err(fail("PERSISTENCE_WRITE_FAILED"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let path = std::env::temp_dir().join(format!("dead-air-persist-{}", crate::random::uuid4_hex()));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn failed_guard_publication_leaves_no_partial_lock_and_can_retry() {
        let directory = temp_dir();
        let path = directory.join("state.db");
        FAIL_GUARD_LINK.with(|f| f.set(true));
        assert!(IncidentStore::open(&path, 90).is_err());
        FAIL_GUARD_LINK.with(|f| f.set(false));
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 0);
        let store = IncidentStore::open(&path, 90).unwrap();
        assert_eq!(std::fs::read(guard_path(&path)).unwrap(), LEGACY_GUARD);
        drop(store);
        std::fs::remove_dir_all(&directory).unwrap();
    }
}
