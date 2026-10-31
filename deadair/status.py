"""Read-only operator health snapshots. No source or notification network access."""

from __future__ import annotations

import json
import math
import re
import sqlite3
import time
from pathlib import Path
from typing import Any

from . import engine as dead_air


_STATUSES = {"HEALTHY", "UNHEALTHY", "OFF_AIR", "UNKNOWN", "DEGRADED", "NOT_APPLICABLE"}
_CONDITIONS = {"off_air", "video_freeze", "video_missing", "audio_silence", "audio_missing", "unknown", "degraded"}


def _reasons(values: Any) -> list[str]:
    if not isinstance(values, list):
        return []
    return [value for value in values[:16] if isinstance(value, str) and re.fullmatch(r"[A-Z0-9_]{1,64}", value)]


def _stream_status(stream: dead_air.StreamConfig, state: dict[str, Any], when: float) -> dict[str, Any]:
    latest = state.get("latest", {})
    if not isinstance(latest, dict):
        latest = {}
    raw_wall = state.get("last_observed_wall")
    try:
        wall = float(raw_wall) if isinstance(raw_wall, (int, float)) and not isinstance(raw_wall, bool) else None
    except OverflowError:
        wall = None
    age = when - wall if wall is not None and math.isfinite(wall) else None
    if age is not None and not math.isfinite(age):
        age = None
    reason = None
    if not latest or age is None:
        reason = "NO_OBSERVATION"
    elif state.get("config_fingerprint") != dead_air.stream_fingerprint(stream):
        reason = "CONFIG_CHANGED"
    elif age < 0:
        reason = "CLOCK_ROLLBACK"
    elif age > stream.max_gap_s:
        reason = "OBSERVATION_STALE"
    raw_status = latest.get("status", "UNKNOWN")
    status = raw_status if isinstance(raw_status, str) and raw_status in _STATUSES - {"NOT_APPLICABLE"} else "UNKNOWN"
    at = latest.get("observed_at")
    # Do not pass arbitrary stored strings into output or terminal escape sequences.
    if not isinstance(at, str) or not re.fullmatch(r"[0-9T:.+Z-]{1,40}", at):
        at = None
    dimensions = {}
    for name in ("audio", "video"):
        dimension = latest.get(name, {})
        if not isinstance(dimension, dict):
            dimension = {}
        dim_status = dimension.get("status", "UNKNOWN")
        dimensions[name] = {"status": dim_status if isinstance(dim_status, str) and dim_status in _STATUSES else "UNKNOWN",
                            "reason_codes": _reasons(dimension.get("reason_codes"))}
    return {"stream_id": stream.id, "status": "UNKNOWN" if reason else status,
            "observation_status": status, "observed_at": at,
            "age_s": round(age, 3) if age is not None and age >= 0 else None,
            "reason_codes": [reason] if reason else _reasons(latest.get("reason_codes")),
            **dimensions}


def read_status(config: dead_air.AppConfig, *, now_wall: float | None = None) -> dict[str, Any]:
    """Read one consistent SQLite snapshot without taking the writer's application lock."""
    when = time.time() if now_wall is None else now_wall
    path = Path(config.watch.state_path).absolute()
    states: dict[str, dict[str, Any]] = {}
    incidents: list[dict[str, Any]] = []
    counts = {"pending": 0, "sent": 0, "failed": 0,
              "unacknowledged_failed": 0, "acknowledged_failed": 0}
    destinations: list[dict[str, Any]] = []
    exists = path.exists()
    if path.is_symlink() or path.parent.is_symlink():
        raise dead_air.PersistenceError("STATE_PATH_UNSAFE")
    if exists:
        connection = None
        try:
            connection = sqlite3.connect(path.as_uri() + "?mode=ro", uri=True, timeout=1, isolation_level=None)
            connection.execute("PRAGMA query_only=ON")
            connection.execute("BEGIN")
            version = connection.execute("PRAGMA user_version").fetchone()[0]
            if version not in {1, dead_air.IncidentStore.SCHEMA}:
                raise dead_air.PersistenceError("STATE_SCHEMA_UNSUPPORTED")
            for stream in config.streams:
                row = connection.execute("SELECT state_json FROM stream_state WHERE stream_id=?", (stream.id,)).fetchone()
                if row:
                    state = json.loads(row[0])
                    if not isinstance(state, dict):
                        raise ValueError
                    states[stream.id] = state
            for sid, condition, opened_at in connection.execute(
                "SELECT stream_id,condition,opened_at FROM incidents WHERE closed_wall IS NULL ORDER BY stream_id,condition LIMIT 448"
            ):
                if re.fullmatch(r"[a-z0-9][a-z0-9_-]{0,63}", sid) and condition in _CONDITIONS:
                    at = opened_at if re.fullmatch(r"[0-9T:.+Z-]{1,40}", opened_at) else None
                    incidents.append({"stream_id": sid, "condition": condition, "opened_at": at})
            if version == 1:
                delivery_rows = connection.execute(
                    "SELECT destination_id,status,COUNT(*),COUNT(*),0 FROM deliveries "
                    "GROUP BY destination_id,status ORDER BY destination_id,status"
                )
            else:
                delivery_rows = connection.execute(
                    "SELECT destination_id,status,COUNT(*),"
                    "SUM(CASE WHEN acknowledged_at IS NULL THEN 1 ELSE 0 END),"
                    "SUM(CASE WHEN acknowledged_at IS NOT NULL THEN 1 ELSE 0 END) "
                    "FROM deliveries GROUP BY destination_id,status ORDER BY destination_id,status"
                )
            for destination_id, status, count, unacknowledged, acknowledged in delivery_rows:
                if status not in {"pending", "sent", "failed"}:
                    raise ValueError
                counts[status] += count
                if status == "failed":
                    counts["unacknowledged_failed"] += unacknowledged
                    counts["acknowledged_failed"] += acknowledged
                if re.fullmatch(r"[a-z0-9][a-z0-9_-]{0,63}", destination_id):
                    item = {"destination_id": destination_id, "status": status, "count": count}
                    if status == "failed":
                        item.update({"unacknowledged": unacknowledged, "acknowledged": acknowledged})
                    destinations.append(item)
        except (sqlite3.DatabaseError, ValueError, TypeError, OverflowError):
            raise dead_air.PersistenceError("PERSISTENCE_READ_FAILED") from None
        finally:
            if connection is not None:
                connection.close()
    streams = [_stream_status(stream, states.get(stream.id, {}), when) for stream in config.streams]
    unhealthy = (any(stream["status"] in {"UNHEALTHY", "OFF_AIR"} for stream in streams)
                 or any(item["condition"] not in {"unknown", "degraded"} for item in incidents)
                 or counts["unacknowledged_failed"] > 0)
    uncertain = any(stream["status"] in {"UNKNOWN", "DEGRADED"} for stream in streams) or bool(incidents)
    status = "UNHEALTHY" if unhealthy else "UNKNOWN" if uncertain else "HEALTHY"
    return {"ok": status == "HEALTHY", "status": status, "state_available": exists,
            "streams": streams, "open_incidents": incidents, "deliveries": counts,
            "destination_deliveries": destinations, "network_contacted": False}


def status_exit_code(result: dict[str, Any]) -> int:
    return {"HEALTHY": 0, "UNHEALTHY": 1}.get(result["status"], 2)


def read_failed_deliveries(config: dead_air.AppConfig, *, limit: int = 50,
                           offset: int = 0) -> list[dict[str, Any]]:
    """List terminal failures for operators without creating or mutating state."""
    if isinstance(limit, bool) or not isinstance(limit, int) or not 1 <= limit <= 200:
        raise ValueError("limit must be between 1 and 200")
    if isinstance(offset, bool) or not isinstance(offset, int) or not 0 <= offset <= 2**31 - 1:
        raise ValueError("offset must be a nonnegative bounded integer")
    path = Path(config.watch.state_path).absolute()
    if path.is_symlink() or path.parent.is_symlink():
        raise dead_air.PersistenceError("STATE_PATH_UNSAFE")
    if not path.exists():
        return []
    connection = None
    try:
        connection = sqlite3.connect(path.as_uri() + "?mode=ro", uri=True, timeout=1, isolation_level=None)
        connection.execute("PRAGMA query_only=ON")
        connection.execute("BEGIN")
        version = connection.execute("PRAGMA user_version").fetchone()[0]
        if version == 1:
            query = ("SELECT event_id,destination_id,attempts,last_http_status,updated_wall,NULL,NULL "
                     "FROM deliveries WHERE status='failed' ORDER BY updated_wall DESC,event_id,destination_id LIMIT ? OFFSET ?")
        elif version == dead_air.IncidentStore.SCHEMA:
            query = ("SELECT event_id,destination_id,attempts,last_http_status,updated_wall,acknowledged_at,acknowledged_by "
                     "FROM deliveries WHERE status='failed' ORDER BY updated_wall DESC,event_id,destination_id LIMIT ? OFFSET ?")
        else:
            raise dead_air.PersistenceError("STATE_SCHEMA_UNSUPPORTED")
        rows = connection.execute(query, (limit, offset)).fetchall()
        result = []
        for event_id, destination_id, attempts, http_status, updated_wall, acknowledged_at, acknowledged_by in rows:
            if not (isinstance(event_id, str) and 1 <= len(event_id) <= 64 and event_id.isascii()
                    and event_id.isalnum() and isinstance(destination_id, str)
                    and re.fullmatch(r"[a-z0-9][a-z0-9_-]{0,63}", destination_id)):
                continue
            result.append({
                "event_id": event_id,
                "destination_id": destination_id,
                "attempts": attempts if isinstance(attempts, int) and 0 <= attempts <= 1000 else None,
                "last_http_status": http_status if isinstance(http_status, int) and 100 <= http_status <= 599 else None,
                "updated_wall": updated_wall if isinstance(updated_wall, (int, float)) and math.isfinite(updated_wall) else None,
                "acknowledged_at": acknowledged_at if isinstance(acknowledged_at, str)
                and re.fullmatch(r"[0-9T:.+Z-]{1,40}", acknowledged_at) else None,
                "acknowledged_by": acknowledged_by if isinstance(acknowledged_by, str)
                and 1 <= len(acknowledged_by) <= 80 and acknowledged_by.isprintable() else None,
            })
        return result
    except (sqlite3.DatabaseError, TypeError, OverflowError):
        raise dead_air.PersistenceError("PERSISTENCE_READ_FAILED") from None
    finally:
        if connection is not None:
            connection.close()


def format_status(result: dict[str, Any]) -> str:
    lines = [f"DEAD AIR  {result['status']}"]
    for stream in result["streams"]:
        age = "never sampled" if stream["age_s"] is None else f"{stream['age_s']:.0f}s ago"
        reasons = ", ".join(stream["reason_codes"]) or "no reason codes"
        lines.append(f"  {stream['stream_id']}: {stream['status']} | {age} | {reasons}")
    for incident in result["open_incidents"]:
        lines.append(f"  OPEN {incident['stream_id']}: {incident['condition']}")
    counts = result["deliveries"]
    lines.append(
        f"Alerts: {counts['pending']} pending, {counts['failed']} failed "
        f"({counts['unacknowledged_failed']} unacknowledged, {counts['acknowledged_failed']} acknowledged), "
        f"{counts['sent']} sent"
    )
    return "\n".join(lines)
