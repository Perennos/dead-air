"""Local page for Dead Air: config and state on 127.0.0.1, stdlib only.

It reads the SQLite state read-only (opening IncidentStore would take the writer lock and
reset a running watcher's counters), adds a stream through the same config loader, and sends
a test alert only when the user clicks the button. It does not run checks; `watch` does.
"""

from __future__ import annotations

import errno
import hmac
import json
import os
import re
import secrets
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
import tomllib
import urllib.parse
import urllib.request
import webbrowser
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any

import dead_air


HERE = Path(__file__).resolve().parent
MAX_BODY = 64 * 1024
LOCAL_NAMES = ("127.0.0.1", "localhost")
PAGES = {
    "/": ("index.html", "text/html; charset=utf-8"),
    "/app.css": ("app.css", "text/css; charset=utf-8"),
    "/app.js": ("app.js", "text/javascript; charset=utf-8"),
}
BRAND = frozenset({"mark.svg", "mark-dead.svg", "favicon.svg"})
DEFAULT_PORT = 47824
IDLE_S = 30 * 60
SECURITY_HEADERS = (
    ("Content-Security-Policy",
     "default-src 'self'; img-src 'self' data:; frame-ancestors 'none'; form-action 'self'; base-uri 'none'"),
    ("X-Frame-Options", "DENY"),
    ("X-Content-Type-Options", "nosniff"),
    ("Referrer-Policy", "no-referrer"),
    ("Cache-Control", "no-store"),
)
# Incident conditions in the order that decides a stream's status word. The first five are signal red.
CONDITIONS = {"off_air": "off-air", "video_freeze": "frozen", "video_missing": "no picture",
              "audio_silence": "silent", "audio_missing": "no audio", "degraded": "degraded",
              "unknown": "unknown"}
RED = tuple(CONDITIONS)[:5]
_WRITE = threading.Lock()
_WATCH = threading.Lock()


class Server(ThreadingHTTPServer):
    allow_reuse_address = os.name != "nt"  # on Windows SO_REUSEADDR would let a second server share the port
    child = None  # the one watcher process this server owns
    started_at = log_path = lock_path = None


def watch_argv(config_path: Path) -> list[str]:
    return [sys.executable, str(HERE / "dead_air.py"), "watch", "--config", str(config_path), "--continuous", "--json"]


def _running(server: Server) -> bool:
    return server.child is not None and server.child.poll() is None


def start_watch(server: Server) -> tuple[int, dict[str, Any]]:
    with _WATCH:
        if _running(server):
            return 409, {"error": "Already watching."}
        try:
            config = dead_air.load_config(server.config_path)
        except dead_air.ConfigError as exc:
            return 400, {"error": str(exc)}
        state = Path(config.watch.state_path).absolute()
        lock = state.parent / ".dead-air.lock"
        if lock.exists():
            return 409, {"error": "Already watched by another process."}
        try:
            state.parent.mkdir(parents=True, exist_ok=True)
            with (state.parent / "dead-air-watch.log").open("wb") as log:  # truncated on each start
                server.child = subprocess.Popen(
                    watch_argv(server.config_path), stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT,
                    start_new_session=os.name == "posix", creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0))
        except OSError:
            return 500, {"error": "could not start the watcher"}
        server.log_path, server.lock_path, server.started_at = state.parent / "dead-air-watch.log", lock, dead_air.utc_now()
    return 200, {"ok": True}


def stop_watch(server: Server) -> tuple[int, dict[str, Any]]:
    with _WATCH:
        child = server.child
        if child is None or child.poll() is not None:
            return 200, {"ok": True}
        if not dead_air._terminate_and_reap(child):  # the core's own helper for owned children
            return 500, {"error": "The watcher did not stop."}
        try:  # a killed watcher cannot release its lock; remove it only if it is the one we started
            if f"pid={child.pid}" in server.lock_path.read_text(encoding="ascii"):
                server.lock_path.unlink()
        except (OSError, UnicodeError, AttributeError):
            pass
    return 200, {"ok": True}


def watcher_info(server: Server) -> dict[str, Any]:
    running, last = _running(server), None
    if server.log_path:
        try:
            with server.log_path.open("rb") as log:
                log.seek(0, os.SEEK_END)
                log.seek(max(0, log.tell() - 262144))
                lines = log.read().decode("utf-8", "replace").splitlines()
            for line in reversed(lines):
                try:
                    parsed = json.loads(line)
                except ValueError:
                    continue
                if isinstance(parsed, dict):
                    last = parsed
                    break
        except OSError:
            pass
    return {"running": running, "pid": server.child.pid if running else None,
            "started_at": server.started_at if running else None, "last_line": last}


def _idle_loop(server: Server) -> None:
    """Stop the server after idle_s without a request, but never while the watcher child is running."""
    while not server.done.wait(min(1.0, server.idle_s / 4)):
        if not _running(server) and time.monotonic() - server.last_request > server.idle_s:
            server.shutdown()
            return


def _iso(timestamp: Any) -> str | None:
    if isinstance(timestamp, bool) or not isinstance(timestamp, (int, float)):
        return None
    try:
        return datetime.fromtimestamp(timestamp, timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z")
    except (OverflowError, OSError, ValueError):
        return None


def read_state(path: Path) -> tuple[dict[str, dict[str, Any]], str | None]:
    """Per stream: last cycle time, bad conditions still being confirmed, open incidents, newest delivery."""
    rows: dict[str, dict[str, Any]] = {}

    def row(stream_id: str) -> dict[str, Any]:
        return rows.setdefault(stream_id, {"last": None, "pending": set(), "open": [], "alert": None})

    if not path.exists():
        return rows, None
    try:
        db = sqlite3.connect(path.absolute().as_uri() + "?mode=ro", uri=True, timeout=2)
        try:
            if db.execute("PRAGMA user_version").fetchone()[0] != dead_air.IncidentStore.SCHEMA:
                return rows, "STATE_SCHEMA_UNSUPPORTED"
            for stream_id, raw in db.execute("SELECT stream_id,state_json FROM stream_state"):
                state = json.loads(raw)
                row(stream_id).update(last=state.get("last_wall"), pending=set(state.get("pending", {})))
            for stream_id, condition, opened in db.execute(
                    "SELECT stream_id,condition,opened_at FROM incidents WHERE closed_wall IS NULL ORDER BY opened_wall"):
                row(stream_id)["open"].append((condition, opened))
            for stream_id, destination, status, error, when in db.execute(
                    "SELECT e.stream_id,d.destination_id,d.status,d.last_error,MAX(d.updated_wall) "
                    "FROM deliveries d JOIN events e ON e.event_id=d.event_id GROUP BY e.stream_id"):
                row(stream_id)["alert"] = {"destination": destination, "outcome": status,
                                           "detail": error, "at": _iso(when)}
        finally:
            db.close()
    except (sqlite3.Error, ValueError, TypeError, AttributeError):
        return rows, "STATE_UNREADABLE"
    return rows, None


def snapshot(config_path: Path) -> dict[str, Any]:
    """What /api/state returns. Hosts only, destination ids and types only: no URLs, endpoints or secrets."""
    out: dict[str, Any] = {
        "config_path": str(config_path), "config_missing": not config_path.exists(), "config_error": None,
        "state_error": None, "stream_count": 0, "streams": [], "destinations": [],
        "last_cycle": None, "watching": False,
    }
    if out["config_missing"]:
        return out
    try:
        config = dead_air.load_config(config_path)
    except dead_air.ConfigError as exc:
        out["config_error"] = str(exc)
        return out
    rows, out["state_error"] = read_state(Path(config.watch.state_path))
    blank = {"last": None, "pending": set(), "open": [], "alert": None}
    for stream in config.streams:
        row = rows.get(stream.id, blank)
        opened = {condition for condition, _ in row["open"]}
        seen = opened | row["pending"]
        top = next((condition for condition in CONDITIONS if condition in seen), None)
        if top:
            status, severity = CONDITIONS[top].upper(), ("incident" if top in RED else "uncertain")
        elif row["last"] is None:
            status, severity = "UNKNOWN", "uncertain"  # never checked, or the state could not be read
        else:
            status, severity = "HEALTHY", "ok"
        out["streams"].append({
            "id": stream.id, "name": stream.label, "host": urllib.parse.urlsplit(stream.url).hostname,
            "source": stream.source, "status": status, "severity": severity,
            "confirming": top is not None and top not in opened,
            "incidents": [{"kind": CONDITIONS.get(condition, condition), "since": since}
                          for condition, since in row["open"]],
            "last_alert": row["alert"], "last_checked": _iso(row["last"]),
        })
    out["stream_count"] = len(config.streams)
    out["destinations"] = [{"id": item.id, "type": item.kind} for item in config.destinations]
    cycles = [r["last"] for r in rows.values() if isinstance(r["last"], (int, float))]
    if cycles:
        out["last_cycle"] = _iso(max(cycles))
        out["watching"] = time.time() - max(cycles) <= 2 * (config.watch.interval_s + config.watch.cycle_budget_s)
    return out


def _toml(value: Any) -> str:
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, (int, float)):
        return repr(value)
    if isinstance(value, list):
        return "[" + ", ".join(_toml(item) for item in value) + "]"
    return json.dumps(value, ensure_ascii=False)  # a JSON string is a valid TOML basic string


def add_stream(config_path: Path, name: Any, url: Any, source: Any) -> tuple[int, dict[str, Any]]:
    """Append one [[streams]] block to a candidate file, check it with the real loader, then replace the file."""
    if not all(isinstance(value, str) for value in (name, url, source)):
        return 400, {"error": "name, url and source must be text"}
    name, url = name.strip(), url.strip()
    slug = re.sub(r"[^a-z0-9]+", "-", name.lower()).strip("-")[:64].strip("-") or "stream-" + secrets.token_hex(3)
    example = (HERE / "dead-air.example.toml").read_text(encoding="utf-8")
    with _WRITE:
        try:
            if config_path.exists():
                current, streams = config_path.read_text(encoding="utf-8"), dead_air.load_config(config_path).streams
            else:  # first stream: start from the example without its sample stream
                current = example[:example.index("[[streams]]")] + example[example.index("# Notifications are disabled"):]
                streams = ()
                # The venv's yt-dlp, so the config works without the venv on PATH.
                tool = next((p for p in (HERE / ".venv" / "Scripts" / "yt-dlp.exe", HERE / ".venv" / "bin" / "yt-dlp")
                             if p.exists()), None)
                if tool:
                    current = current.replace('yt_dlp = "yt-dlp"', f"yt_dlp = {_toml(str(tool))}")
        except dead_air.ConfigError as exc:
            return 400, {"error": str(exc)}
        except (OSError, UnicodeError):
            return 400, {"error": "cannot read the config file"}
        if any(s.url == url or s.label.casefold() == name.casefold() or s.id == slug for s in streams):
            return 409, {"error": "A stream with that name or URL is already in the config."}
        fields = dict(tomllib.loads(example)["streams"][0], id=slug, label=name, url=url, source=source)
        candidate = current.rstrip("\n") + "\n\n[[streams]]\n" + "".join(f"{k} = {_toml(v)}\n" for k, v in fields.items())
        try:
            config_path.parent.mkdir(parents=True, exist_ok=True)
            handle, temp = tempfile.mkstemp(dir=config_path.parent, prefix=".dead-air-", suffix=".tmp")
            try:
                with os.fdopen(handle, "w", encoding="utf-8", newline="\n") as out:
                    out.write(candidate)
                try:
                    dead_air.load_config(temp)
                except dead_air.ConfigError as exc:
                    return 400, {"error": str(exc)}
                if config_path.exists():
                    shutil.copymode(config_path, temp)
                os.replace(temp, config_path)
            finally:
                if os.path.exists(temp):
                    os.unlink(temp)
        except UnicodeError:
            return 400, {"error": "that text cannot be saved"}
        except OSError:
            return 500, {"error": "could not write the config file"}
    return 200, {"ok": True, "id": slug}


def send_test_alerts(config_path: Path) -> tuple[int, dict[str, Any]]:
    """One labelled test event through each configured destination: a single bounded attempt each."""
    if not config_path.exists():
        return 200, {"results": []}
    try:
        config = dead_air.load_config(config_path)
    except dead_air.ConfigError as exc:
        return 400, {"error": str(exc)}
    now = dead_air.utc_now()
    # The sender builds its one-line summary from these fields: "Dead Air test: alert from the GUI (<time>)".
    event = {"event_id": now, "incident_id": "gui-test", "stream_id": "alert", "condition": "from the GUI",
             "transition": "test", "observed_at": now, "status": "TEST", "reason_codes": [], "evidence": {},
             "message": f"DEAD AIR test alert from the GUI, {now}"}
    results = []
    for destination in config.destinations:
        ok, reason, status = dead_air._send_destination_bounded(
            destination, event, config.network, timeout_s=dead_air.NOTIFICATION_ATTEMPT_BUDGET_S)[:3]
        detail = "sent" if ok else (reason or "failed") + (f" ({status})" if status else "")
        results.append({"id": destination.id, "type": destination.kind, "ok": bool(ok), "detail": detail})
    return 200, {"results": results}


class Handler(BaseHTTPRequestHandler):
    server_version = "DeadAirGUI"
    sys_version = ""
    timeout = 15

    def log_message(self, *args: Any) -> None:  # pythonw has no stderr, and requests are not worth a log
        pass

    def end_headers(self) -> None:
        for name, value in SECURITY_HEADERS:
            self.send_header(name, value)
        super().end_headers()

    def _send(self, status: int, content_type: str, body: bytes) -> None:
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _json(self, status: int, payload: dict[str, Any]) -> None:
        self._send(status, "application/json; charset=utf-8", json.dumps(payload).encode("utf-8"))

    def _host_ok(self) -> bool:
        name, colon, port = (self.headers.get("Host") or "").lower().partition(":")
        return name in LOCAL_NAMES and (not colon or port == str(self.server.server_port))

    def _origin_ok(self) -> bool:
        origin = self.headers.get("Origin")
        return origin is None or origin.lower() in {f"http://{n}:{self.server.server_port}" for n in LOCAL_NAMES}

    def do_GET(self) -> None:
        self.server.last_request = time.monotonic()
        if not self._host_ok():
            return self._json(421, {"error": "unexpected Host"})
        path = urllib.parse.urlsplit(self.path).path
        if path == "/api/ping":
            return self._json(200, {"app": "dead-air"})
        if path == "/api/state":
            return self._json(200, dict(snapshot(self.server.config_path), watcher=watcher_info(self.server)))
        if path in PAGES:
            filename, content_type = PAGES[path]
            file = HERE / "gui" / filename
        elif path.startswith("/brand/") and path[len("/brand/"):] in BRAND:
            file, content_type = HERE / "brand" / path[len("/brand/"):], "image/svg+xml"
        else:
            return self._json(404, {"error": "not found"})
        try:
            body = file.read_bytes()
        except OSError:
            return self._json(404, {"error": "not found"})
        if path == "/":
            body = body.replace(b"__TOKEN__", self.server.token.encode("ascii"))
        self._send(200, content_type, body)

    def do_POST(self) -> None:
        self.server.last_request = time.monotonic()
        self.close_connection = True
        try:
            length = int(self.headers.get("Content-Length", ""))
            if length < 0:
                raise ValueError
        except ValueError:
            return self._json(411, {"error": "Content-Length required"})
        try:
            body = self.rfile.read(min(length, MAX_BODY))
            extra = min(length - MAX_BODY, 1 << 20)  # read an oversize body (to 1 MiB) so the 413 can be delivered
            while 0 < extra and (chunk := self.rfile.read(min(extra, 65536))):
                extra -= len(chunk)
        except OSError:
            return
        if not self._host_ok():
            return self._json(421, {"error": "unexpected Host"})
        supplied = (self.headers.get("X-Token") or "").encode("utf-8", "replace")
        if not self._origin_ok() or not hmac.compare_digest(supplied, self.server.token.encode("ascii")):
            return self._json(403, {"error": "forbidden"})
        if length > MAX_BODY:
            return self._json(413, {"error": "request body too large"})
        path = urllib.parse.urlsplit(self.path).path
        if path not in ("/api/streams", "/api/test-alerts", "/api/watch", "/api/quit"):
            return self._json(404, {"error": "not found"})
        try:
            if path == "/api/test-alerts":
                status, payload = send_test_alerts(self.server.config_path)
            elif path == "/api/quit":
                status, payload = stop_watch(self.server)
                if status == 200:
                    threading.Thread(target=self.server.shutdown, daemon=True).start()
            elif path == "/api/watch":
                try:
                    data = json.loads(body.decode("utf-8"))
                except ValueError:
                    data = None
                action = data.get("action") if isinstance(data, dict) else None
                if action not in ("start", "stop"):
                    return self._json(400, {"error": "action must be start or stop"})
                status, payload = (start_watch if action == "start" else stop_watch)(self.server)
            else:
                try:
                    data = json.loads(body.decode("utf-8"))
                except ValueError:  # bad JSON or bad UTF-8
                    data = None
                if not isinstance(data, dict):
                    return self._json(400, {"error": "expected a JSON object"})
                status, payload = add_stream(self.server.config_path, data.get("name"), data.get("url"), data.get("source"))
        except Exception:
            return self._json(500, {"error": "internal error"})
        self._json(status, payload)


def make_server(config_path: str | os.PathLike[str], port: int = DEFAULT_PORT, idle_s: float = IDLE_S) -> Server:
    server = Server(("127.0.0.1", port), Handler)
    server.token = secrets.token_urlsafe(32)
    server.config_path = Path(config_path).absolute()
    server.idle_s, server.last_request, server.done = idle_s, time.monotonic(), threading.Event()
    return server


def _already_running(port: int, open_browser: bool) -> int:
    """The port is taken: if it is another Dead Air, open its page; otherwise say so."""
    url = f"http://127.0.0.1:{port}/"
    try:
        with urllib.request.build_opener(urllib.request.ProxyHandler({})).open(url + "api/ping", timeout=2) as reply:
            ours = json.loads(reply.read(1024)).get("app") == "dead-air"
    except (OSError, ValueError, AttributeError):
        ours = False
    if not ours:
        print(json.dumps({"ok": False, "error": f"port {port} is in use by another program; use --port N"}))
        return 2
    print(f"DEAD AIR GUI: {url}", flush=True)
    if open_browser:
        webbrowser.open(url)
    return 0


def serve(config_path: str | os.PathLike[str], port: int = DEFAULT_PORT, open_browser: bool = True,
          watch: bool = False, idle_s: float = IDLE_S) -> int:
    try:
        server = make_server(config_path, port, idle_s)
    except (OSError, OverflowError) as exc:
        if isinstance(exc, OSError) and exc.errno in (errno.EADDRINUSE, 10048):
            return _already_running(port, open_browser)
        print(json.dumps({"ok": False, "error": f"cannot listen on 127.0.0.1:{port} ({type(exc).__name__})"}))
        return 3
    url = f"http://127.0.0.1:{server.server_port}/"
    print(f"DEAD AIR GUI: {url}", flush=True)
    if watch:
        status, payload = start_watch(server)
        if status != 200:
            print(json.dumps({"ok": False, "error": payload["error"]}), flush=True)
    threading.Thread(target=_idle_loop, args=(server,), daemon=True).start()
    if open_browser:
        webbrowser.open(url)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        stop_watch(server)
        server.done.set()
        server.server_close()
    return 0
