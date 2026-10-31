"""SQLite incident ledger and OS-owned writer lifecycle."""
from __future__ import annotations
import json
import math
import os
import sqlite3
import tempfile
import uuid
from pathlib import Path
from datetime import datetime
from typing import Any, Mapping, Sequence
from .models import AppConfig, DestinationConfig, Observation, PersistenceError, StreamConfig

class IncidentStore:
    SCHEMA = 2
    LEGACY_GUARD = b"DEAD_AIR_OS_LOCK_V2\n"

    def __init__(self, path: str | os.PathLike[str], retention_days: int = 90):
        self.path = Path(path).absolute()
        self.directory = self.path.parent
        self.retention_days = retention_days
        self._db: sqlite3.Connection | None = None
        self._lock_path = self.path.with_name(self.path.name + ".lock")
        self._lock_fd: int | None = None
        try:
            self.directory.mkdir(parents=True, exist_ok=True, mode=0o700)
            if self.directory.is_symlink() or self.path.is_symlink():
                raise PersistenceError("STATE_PATH_UNSAFE")
            self._guard_legacy_writers()
            self._acquire_lock()
            existed = self.path.exists()
            if existed and self.path.stat().st_size == 0:
                raise PersistenceError("STATE_CORRUPT")
            self._db = sqlite3.connect(str(self.path), timeout=5, isolation_level=None)
            if not existed:
                os.chmod(self.path, 0o600)
            self._db.execute("PRAGMA foreign_keys=ON")
            self._db.execute("PRAGMA busy_timeout=5000")
            version = int(self._db.execute("PRAGMA user_version").fetchone()[0])
            if not existed:
                self._create_schema()
            elif version == 1:
                self._migrate_schema_1()
            elif version != self.SCHEMA:
                raise PersistenceError("STATE_SCHEMA_UNSUPPORTED")
            if self._db.execute("PRAGMA quick_check").fetchone()[0] != "ok":
                raise PersistenceError("STATE_CORRUPT")
            self._db.execute("BEGIN IMMEDIATE")
            for stream_id, raw_state in self._db.execute("SELECT stream_id,state_json FROM stream_state").fetchall():
                state = json.loads(raw_state)
                # Monotonic counters cannot cross process lifetimes. Keep the last
                # observation for readers, but never use it as new transition evidence.
                saved = {key: state[key] for key in ("latest", "last_observed_wall", "config_fingerprint") if key in state}
                saved.update({"version": self.SCHEMA, "pending": {}, "recoveries": {}})
                self._db.execute("UPDATE stream_state SET state_json=? WHERE stream_id=?",
                                 (json.dumps(saved, separators=(",", ":")), stream_id))
            self._db.execute("COMMIT")
        except FileExistsError:
            self._close_db()
            raise PersistenceError("STATE_LOCKED") from None
        except PersistenceError:
            self._close_db()
            self._release_lock()
            raise
        except sqlite3.DatabaseError:
            self._close_db()
            self._release_lock()
            raise PersistenceError("STATE_CORRUPT") from None
        except (OSError, ValueError, TypeError):
            self._close_db()
            self._release_lock()
            raise PersistenceError("STATE_UNAVAILABLE") from None

    def _create_schema(self) -> None:
        assert self._db is not None
        self._db.executescript("""
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
        """)

    def _migrate_schema_1(self) -> None:
        """Add delivery acknowledgments without deleting or rewriting delivery rows."""
        assert self._db is not None
        try:
            self._db.execute("BEGIN EXCLUSIVE")
            self._db.execute("ALTER TABLE deliveries ADD COLUMN acknowledged_at TEXT")
            self._db.execute("ALTER TABLE deliveries ADD COLUMN acknowledged_wall REAL")
            self._db.execute("ALTER TABLE deliveries ADD COLUMN acknowledged_by TEXT")
            self._db.execute("PRAGMA user_version=2")
            self._db.execute("COMMIT")
        except sqlite3.DatabaseError:
            try:
                self._db.execute("ROLLBACK")
            except sqlite3.DatabaseError:
                pass
            raise PersistenceError("STATE_MIGRATION_FAILED") from None

    def _close_db(self) -> None:
        if self._db is not None:
            try:
                self._db.close()
            except sqlite3.Error:
                pass
            self._db = None

    def _acquire_lock(self) -> None:
        if self._lock_path.is_symlink():
            raise PersistenceError("STATE_PATH_UNSAFE")
        flags = os.O_CREAT | os.O_RDWR | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_BINARY", 0)
        descriptor = os.open(self._lock_path, flags, 0o600)
        try:
            if os.name == "nt":
                import msvcrt
                if os.fstat(descriptor).st_size == 0:
                    os.write(descriptor, b"\0")
                os.lseek(descriptor, 0, os.SEEK_SET)
                msvcrt.locking(descriptor, msvcrt.LK_NBLCK, 1)
            else:
                import fcntl
                fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError:
            os.close(descriptor)
            raise PersistenceError("STATE_LOCKED") from None
        self._lock_fd = descriptor

    def _guard_legacy_writers(self) -> None:
        """Old releases use O_EXCL on this directory sentinel; keep them out too."""
        path = self.directory / ".dead-air.lock"
        def verify() -> None:
            if path.is_symlink():
                raise PersistenceError("STATE_PATH_UNSAFE")
            with path.open("rb") as handle:
                if handle.read(len(self.LEGACY_GUARD) + 1) != self.LEGACY_GUARD:
                    raise PersistenceError("STATE_LOCKED")
        if path.exists() or path.is_symlink():
            verify()
            return
        descriptor, temporary = tempfile.mkstemp(prefix=".dead-air-guard-", dir=self.directory)
        try:
            with os.fdopen(descriptor, "wb") as handle:
                handle.write(self.LEGACY_GUARD)
                handle.flush()
                os.fsync(handle.fileno())
            try:
                # Publish complete contents without replacing any legacy owner's
                # sentinel. A crash before this link cannot block future startup.
                os.link(temporary, path)
            except FileExistsError:
                verify()
        finally:
            os.unlink(temporary)

    def _release_lock(self) -> None:
        if self._lock_fd is not None:
            descriptor, self._lock_fd = self._lock_fd, None
            try:
                if os.name == "nt":
                    import msvcrt
                    os.lseek(descriptor, 0, os.SEEK_SET)
                    msvcrt.locking(descriptor, msvcrt.LK_UNLCK, 1)
                else:
                    import fcntl
                    fcntl.flock(descriptor, fcntl.LOCK_UN)
            finally:
                os.close(descriptor)
        # Keep the inode: unlinking a lock file lets competing processes lock
        # different files at the same path and both believe they own the database.

    def close(self) -> None:
        self._close_db()
        self._release_lock()

    def __enter__(self) -> "IncidentStore":
        return self

    def __exit__(self, *_: Any) -> None:
        self.close()

    def _insert_event(self, stream: StreamConfig, incident_id: str, condition: str,
                      transition: str, observation: Observation, now_wall: float,
                      destinations: Sequence[DestinationConfig],
                      evidence: Mapping[str, Any]) -> None:
        assert self._db is not None
        event_id = uuid.uuid4().hex
        payload = {
            "event_id": event_id, "incident_id": incident_id, "stream_id": stream.id,
            "condition": condition, "transition": transition,
            "observed_at": observation.observed_at,
            "status": observation.status, "reason_codes": list(observation.reason_codes),
            "evidence": dict(evidence),
        }
        self._db.execute(
            "INSERT INTO events VALUES(?,?,?,?,?,?,?)",
            (event_id, incident_id, stream.id, condition, transition, now_wall,
             json.dumps(payload, sort_keys=True, separators=(",", ":"))),
        )
        for destination in destinations:
            self._db.execute(
                "INSERT INTO deliveries(event_id,destination_id,status,attempts,created_wall,next_attempt_wall,last_error,last_http_status,updated_wall) "
                "VALUES(?,?,'pending',0,?,?,NULL,NULL,?)",
                (event_id, destination.id, now_wall, now_wall, now_wall),
            )

    def record_cycle(self, config: AppConfig, observations: Sequence[Observation],
                     now_wall: float, now_mono: float) -> None:
        from . import engine as core
        if self._db is None:
            raise PersistenceError("STATE_CLOSED")
        streams = {stream.id: stream for stream in config.streams}
        try:
            self._db.execute("BEGIN IMMEDIATE")
            for observation in observations:
                stream = streams.get(observation.stream_id)
                if stream is None:
                    raise PersistenceError("OBSERVATION_STREAM_UNKNOWN")
                row = self._db.execute("SELECT state_json FROM stream_state WHERE stream_id=?", (stream.id,)).fetchone()
                state = json.loads(row[0]) if row else {"version": self.SCHEMA, "pending": {}, "recoveries": {}}
                if state.get("version") != self.SCHEMA:
                    raise PersistenceError("STATE_ROW_SCHEMA_UNSUPPORTED")
                pending = state.setdefault("pending", {})
                recoveries = state.setdefault("recoveries", {})
                previous_wall, previous_mono = state.get("last_wall"), state.get("last_mono")
                if previous_wall is not None:
                    wall_delta, mono_delta = now_wall - previous_wall, now_mono - previous_mono
                    if wall_delta < 0 or mono_delta < 0 or abs(wall_delta - mono_delta) > 5 or mono_delta > stream.max_gap_s:
                        pending.clear()
                        recoveries.clear()
                signals = core._incident_signals(observation)
                for condition, signal_value in signals.items():
                    incident = self._db.execute(
                        "SELECT incident_id FROM incidents WHERE stream_id=? AND condition=? AND closed_wall IS NULL",
                        (stream.id, condition),
                    ).fetchone()
                    if incident:
                        if signal_value == "good":
                            prior_recovery = recoveries.get(condition)
                            if not isinstance(prior_recovery, dict):
                                prior_recovery = {"count": 0, "first_mono": now_mono, "samples": []}
                            prior_recovery["count"] += 1
                            prior_recovery["samples"].append(core._transition_sample(condition, observation))
                            recoveries[condition] = prior_recovery
                            if prior_recovery["count"] >= stream.recovery_samples:
                                at = observation.observed_at
                                self._db.execute(
                                    "UPDATE incidents SET closed_at=?,closed_wall=?,status='resolved' WHERE incident_id=?",
                                    (at, now_wall, incident[0]),
                                )
                                evidence = core._transition_evidence(
                                    condition, observation,
                                    threshold={
                                        "sample_count": prior_recovery["count"],
                                        "required_samples": stream.recovery_samples,
                                        "span_s": max(0.0, now_mono - prior_recovery["first_mono"]),
                                        "required_span_s": 0.0,
                                    },
                                    samples=prior_recovery["samples"],
                                )
                                self._insert_event(stream, incident[0], condition, "recovery", observation,
                                                   now_wall, config.destinations, evidence)
                                recoveries.pop(condition, None)
                        else:
                            recoveries.pop(condition, None)
                        continue
                    if signal_value == "good":
                        pending.pop(condition, None)
                    elif signal_value == "bad":
                        prior = pending.get(condition)
                        if prior and now_mono - prior["last_mono"] > stream.max_gap_s:
                            prior = None
                        if prior is None:
                            prior = {"count": 0, "first_wall": now_wall, "first_mono": now_mono}
                        prior["count"] += 1
                        prior["last_wall"] = now_wall
                        prior["last_mono"] = now_mono
                        pending[condition] = prior
                        if condition in {"unknown", "degraded"}:
                            threshold = stream.unknown_notice_after_s
                            required = 1 if threshold == 0 else 2
                        else:
                            threshold = stream.unhealthy_after_s
                            required = stream.min_bad_samples
                        if prior["count"] >= required and now_mono - prior["first_mono"] >= threshold:
                            incident_id = uuid.uuid4().hex
                            self._db.execute(
                                "INSERT INTO incidents VALUES(?,?,?,?,?,NULL,NULL,'open')",
                                (incident_id, stream.id, condition, observation.observed_at, now_wall),
                            )
                            evidence = core._transition_evidence(
                                condition, observation,
                                threshold={
                                    "sample_count": prior["count"],
                                    "required_samples": required,
                                    "span_s": max(0.0, now_mono - prior["first_mono"]),
                                    "required_span_s": threshold,
                                },
                            )
                            self._insert_event(stream, incident_id, condition, "open", observation,
                                               now_wall, config.destinations, evidence)
                            pending.pop(condition, None)
                    # Unknown evidence neither advances nor resolves another condition.
                state["last_wall"], state["last_mono"] = now_wall, now_mono
                state["latest"] = core.observation_json(observation)
                try:
                    observed = datetime.fromisoformat(observation.observed_at.replace("Z", "+00:00"))
                    state["last_observed_wall"] = observed.timestamp() if observed.tzinfo is not None else None
                except (ValueError, OverflowError, OSError):
                    state["last_observed_wall"] = None
                state["config_fingerprint"] = core.stream_fingerprint(stream)
                raw_state = json.dumps(state, sort_keys=True, separators=(",", ":"))
                self._db.execute(
                    "INSERT INTO stream_state VALUES(?,?) ON CONFLICT(stream_id) DO UPDATE SET state_json=excluded.state_json",
                    (stream.id, raw_state),
                )
            self._prune(now_wall)
            self._db.execute("COMMIT")
        except PersistenceError:
            try:
                self._db.execute("ROLLBACK")
            except sqlite3.DatabaseError:
                pass
            raise
        except (sqlite3.DatabaseError, ValueError, TypeError, KeyError):
            try:
                self._db.execute("ROLLBACK")
            except sqlite3.DatabaseError:
                pass
            raise PersistenceError("PERSISTENCE_WRITE_FAILED") from None

    def _prune(self, now_wall: float) -> None:
        assert self._db is not None
        cutoff = now_wall - self.retention_days * 86400
        self._db.execute("""
            DELETE FROM events WHERE created_wall < ?
            AND incident_id IN (SELECT incident_id FROM incidents WHERE closed_wall IS NOT NULL)
            AND NOT EXISTS (SELECT 1 FROM deliveries d WHERE d.event_id=events.event_id
                            AND d.status!='sent' AND d.acknowledged_at IS NULL)
        """, (cutoff,))
        self._db.execute("""
            DELETE FROM incidents WHERE closed_wall IS NOT NULL AND closed_wall < ?
            AND NOT EXISTS (SELECT 1 FROM events e WHERE e.incident_id=incidents.incident_id)
        """, (cutoff,))

    def due_deliveries(self, now_wall: float, limit: int = 8,
                       destination_ids: Sequence[str] | None = None) -> list[tuple[Any, ...]]:
        assert self._db is not None
        if destination_ids is not None and not destination_ids:
            return []
        filter_sql = ""
        parameters: tuple[Any, ...] = (now_wall,)
        if destination_ids is not None:
            filter_sql = " AND d.destination_id IN (" + ",".join("?" for _ in destination_ids) + ")"
            parameters += tuple(destination_ids)
        parameters += (limit,)
        return self._db.execute("""
            SELECT d.event_id,d.destination_id,d.attempts,d.created_wall,e.payload_json
            FROM deliveries d JOIN events e ON e.event_id=d.event_id
            WHERE d.status='pending' AND d.attempts<3 AND d.next_attempt_wall<=?
        """ + filter_sql + """
            ORDER BY d.next_attempt_wall,d.event_id,d.destination_id LIMIT ?
        """, parameters).fetchall()

    def record_delivery(self, event_id: str, destination_id: str, success: bool,
                        reason: str | None, http_status: int | None, now_wall: float,
                        created_wall: float, attempts: int, retry_after_s: float | None = None) -> None:
        assert self._db is not None
        count = attempts + 1
        status = "sent" if success else ("failed" if count >= 3 else "pending")
        base_delay = 30 if count == 1 else 120
        server_delay = min(120.0, max(0.0, retry_after_s)) if retry_after_s is not None and math.isfinite(retry_after_s) else 0.0
        next_due = None if success or count >= 3 else max(created_wall + base_delay, now_wall + server_delay)
        try:
            self._db.execute("""
                UPDATE deliveries SET status=?,attempts=?,next_attempt_wall=?,last_error=?,
                last_http_status=?,updated_wall=? WHERE event_id=? AND destination_id=?
            """, (status, count, next_due, None if success else reason, http_status, now_wall,
                  event_id, destination_id))
        except sqlite3.DatabaseError:
            raise PersistenceError("PERSISTENCE_WRITE_FAILED") from None

    def acknowledge_failed_delivery(self, event_id: str, destination_id: str,
                                    operator: str, acknowledged_at: str,
                                    now_wall: float) -> str:
        """Record who acknowledged a terminal failure while retaining its delivery row."""
        assert self._db is not None
        if (not isinstance(event_id, str) or not event_id or len(event_id) > 64
                or not event_id.isascii() or not event_id.isalnum()):
            raise ValueError("event_id must be a bounded ASCII identifier")
        if (not isinstance(destination_id, str) or not destination_id
                or len(destination_id) > 64
                or any(char not in "abcdefghijklmnopqrstuvwxyz0123456789_-" for char in destination_id)):
            raise ValueError("destination_id must be a bounded lowercase identifier")
        if (not isinstance(operator, str) or not operator.strip()
                or len(operator) > 80 or not operator.isprintable()):
            raise ValueError("operator must be a printable label of 1 to 80 characters")
        if not isinstance(acknowledged_at, str) or len(acknowledged_at) > 40:
            raise ValueError("acknowledged_at must be an ISO timestamp")
        try:
            parsed_at = datetime.fromisoformat(acknowledged_at.replace("Z", "+00:00"))
        except ValueError:
            raise ValueError("acknowledged_at must be an ISO timestamp") from None
        try:
            finite_now = (isinstance(now_wall, (int, float)) and not isinstance(now_wall, bool)
                          and math.isfinite(now_wall))
        except OverflowError:
            finite_now = False
        if parsed_at.tzinfo is None or not finite_now:
            raise ValueError("acknowledgment time must include a timezone and finite epoch")
        try:
            self._db.execute("BEGIN IMMEDIATE")
            row = self._db.execute(
                "SELECT status,acknowledged_at FROM deliveries WHERE event_id=? AND destination_id=?",
                (event_id, destination_id),
            ).fetchone()
            if row is None:
                self._db.execute("COMMIT")
                return "not_found"
            if row[0] != "failed":
                self._db.execute("COMMIT")
                return "not_failed"
            if row[1] is not None:
                self._db.execute("COMMIT")
                return "already_acknowledged"
            self._db.execute(
                "UPDATE deliveries SET acknowledged_at=?,acknowledged_wall=?,acknowledged_by=? "
                "WHERE event_id=? AND destination_id=? AND status='failed' AND acknowledged_at IS NULL",
                (acknowledged_at, now_wall, operator.strip(), event_id, destination_id),
            )
            if self._db.execute("SELECT changes()").fetchone()[0] != 1:
                raise sqlite3.DatabaseError("delivery changed during acknowledgment")
            self._db.execute("COMMIT")
            return "acknowledged"
        except sqlite3.DatabaseError:
            try:
                self._db.execute("ROLLBACK")
            except sqlite3.DatabaseError:
                pass
            raise PersistenceError("PERSISTENCE_WRITE_FAILED") from None

    def open_incidents(self) -> list[tuple[Any, ...]]:
        assert self._db is not None
        return self._db.execute("SELECT stream_id,condition,opened_at FROM incidents WHERE closed_wall IS NULL ORDER BY condition").fetchall()

    def events(self) -> list[tuple[Any, ...]]:
        assert self._db is not None
        return self._db.execute("SELECT event_id,condition,transition,payload_json FROM events ORDER BY created_wall,event_id").fetchall()

    def deliveries(self) -> list[tuple[Any, ...]]:
        assert self._db is not None
        return self._db.execute("SELECT event_id,destination_id,status,attempts,last_error,last_http_status FROM deliveries ORDER BY event_id,destination_id").fetchall()

    def diagnostics(self) -> dict[str, Any]:
        assert self._db is not None
        try:
            counts = {"pending": 0, "sent": 0, "failed": 0}
            counts.update(dict(self._db.execute("SELECT status,COUNT(*) FROM deliveries GROUP BY status")))
            return {"open_incidents": [{"stream_id": sid, "condition": condition, "opened_at": at}
                                       for sid, condition, at in self.open_incidents()],
                    "deliveries": counts}
        except sqlite3.DatabaseError:
            raise PersistenceError("PERSISTENCE_READ_FAILED") from None


