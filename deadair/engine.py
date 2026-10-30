"""Stdlib-first, bounded media observations and command orchestration."""

from __future__ import annotations

import argparse
import email.message
import email.utils
import hashlib
import http.client
import ipaddress
import json
import math
import os
import re
import signal
import shutil
import smtplib
import sqlite3
import ssl
import socket
import subprocess
import sys
import tempfile
import threading
import time
import tomllib
import urllib.error
import urllib.parse
import urllib.request
import uuid
from dataclasses import asdict, dataclass, replace
from datetime import datetime, timezone
from fractions import Fraction
from pathlib import Path, PureWindowsPath
from typing import Any, Callable, Iterator, Mapping, Sequence

from .constants import *
from .models import ConfigError, PolicyError, PersistenceError, WatchConfig, ToolsConfig, NetworkConfig, StreamConfig, AppConfig, DestinationConfig, ChildResult, Interval, DimensionEvidence, CoverageEvidence, Observation, ProbeEvidence, ProgressEvidence, MediaInput, ResolvedMedia, HttpResult, ThumbnailBaseline
from .config import _finite, _integer, _boolean, _table, _string, _safe_hostport, _static_url, validate_config, load_config, _env_name, _email_address, _parse_destinations



_ID = re.compile(r"[a-z0-9][a-z0-9_-]{0,63}\Z")
_LABEL_CONTROL = re.compile(r"[\x00-\x1f\x7f]")
_FREEZE_START = re.compile(r"lavfi\.freezedetect\.freeze_start:\s*(-?(?:\d+(?:\.\d*)?|\.\d+))")
_FREEZE_END = re.compile(r"lavfi\.freezedetect\.freeze_end:\s*(-?(?:\d+(?:\.\d*)?|\.\d+))")
_SILENCE_START = re.compile(r"\bsilence_start:\s*(-?(?:\d+(?:\.\d*)?|\.\d+))")
_SILENCE_END = re.compile(r"\bsilence_end:\s*(-?(?:\d+(?:\.\d*)?|\.\d+))")


def build_argv(executable: str | os.PathLike[str], *args: str | os.PathLike[str]) -> list[str]:
    values = [os.fspath(executable), *(os.fspath(arg) for arg in args)]
    if not values[0].strip() or any("\0" in value for value in values):
        raise ValueError("argv contains an empty executable or NUL")
    return values


def child_environment() -> dict[str, str]:
    """Pass only runtime essentials; omit proxy, credential, and user config variables."""
    allowed = ("PATH", "SYSTEMROOT", "WINDIR", "TEMP", "TMP")
    env = {key: os.environ[key] for key in allowed if os.environ.get(key)}
    env.update({"LC_ALL": "C", "LANG": "C"})
    return env


def _terminate_and_reap(child: subprocess.Popen[bytes]) -> bool:
    try:
        if os.name == "posix":
            os.killpg(child.pid, signal.SIGTERM)
        else:
            child.terminate()
    except OSError:
        pass
    try:
        child.wait(timeout=0.2)
        return True
    except subprocess.TimeoutExpired:
        pass
    try:
        if os.name == "posix":
            os.killpg(child.pid, signal.SIGKILL)
        else:
            child.kill()
    except OSError:
        try:
            child.kill()
        except OSError:
            pass
    try:
        child.wait(timeout=1.0)
        return True
    except subprocess.TimeoutExpired:
        return False


def run_bounded(
    argv: Sequence[str],
    timeout_seconds: float,
    *,
    max_output_bytes: int = 65536,
    file_limits: Sequence[tuple[Path, int]] = (),
) -> ChildResult:
    """Run with capped pipes and finite cleanup; Windows descendants are not job-object contained."""
    if not argv:
        raise ValueError("argv cannot be empty")
    command = build_argv(argv[0], *argv[1:])
    if not math.isfinite(timeout_seconds) or timeout_seconds <= 0:
        raise ValueError("timeout_seconds must be positive and finite")
    if isinstance(max_output_bytes, bool) or not isinstance(max_output_bytes, int) or max_output_bytes < 128:
        raise ValueError("max_output_bytes must be at least 128")
    try:
        child = subprocess.Popen(
            command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            shell=False, close_fds=True, env=child_environment(), bufsize=0,
            start_new_session=(os.name == "posix"),
            creationflags=(getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0) if os.name == "nt" else 0),
        )
    except OSError as exc:
        return ChildResult("", "", None, launch_error=type(exc).__name__)

    buffers = {"stdout": bytearray(), "stderr": bytearray()}
    overflow = threading.Event()
    drained_to_eof = {"stdout": threading.Event(), "stderr": threading.Event()}
    drain_truncated = {"stdout": threading.Event(), "stderr": threading.Event()}

    def drain(name: str, pipe: Any) -> None:
        buf = buffers[name]
        while True:
            try:
                chunk = pipe.read(4096)
            except (OSError, ValueError):
                return
            if not chunk:
                drained_to_eof[name].set()
                return
            remaining = max_output_bytes + 1 - len(buf)
            if remaining > 0:
                buf.extend(chunk[:remaining])
            if len(buf) > max_output_bytes:
                drain_truncated[name].set()
                overflow.set()
                return

    threads = [
        threading.Thread(target=drain, args=("stdout", child.stdout), daemon=True),
        threading.Thread(target=drain, args=("stderr", child.stderr), daemon=True),
    ]
    deadline = time.monotonic() + timeout_seconds
    stop_reason: str | None = None
    cleanup_complete = True
    try:
        for thread in threads:
            thread.start()
        while child.poll() is None:
            if overflow.is_set():
                stop_reason = "output"
                break
            exceeded = False
            for path, limit in file_limits:
                try:
                    if path.stat().st_size > limit:
                        exceeded = True
                        break
                except FileNotFoundError:
                    pass
                except OSError:
                    exceeded = True
                    break
            if exceeded:
                stop_reason = "file"
                break
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                stop_reason = "timeout"
                break
            time.sleep(min(0.02, remaining))
        if overflow.is_set() and stop_reason is None:
            stop_reason = "output"
    except BaseException:
        # Also signal a POSIX process group if its direct child already exited.
        _terminate_and_reap(child)
        raise
    finally:
        # Ponytail: one owner path cleans every started child, including interrupts.
        if child.poll() is None:
            cleanup_complete = _terminate_and_reap(child)
        if os.name == "posix":
            # A resolver may exit while a helper still owns its stdout/stderr.
            # The new session belongs to this invocation, including those helpers.
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            except OSError:
                cleanup_complete = False
        for thread in threads:
            if thread.ident is not None:
                thread.join(timeout=0.5)
                if thread.is_alive():
                    cleanup_complete = False
        for name, pipe in (("stdout", child.stdout), ("stderr", child.stderr)):
            if pipe is not None:
                if not drained_to_eof[name].is_set() and not drain_truncated[name].is_set():
                    cleanup_complete = False
                try:
                    pipe.close()
                except (OSError, ValueError):
                    pass
        for thread in threads:
            if thread.ident is not None:
                thread.join(timeout=0.1)
                if thread.is_alive():
                    cleanup_complete = False
    if overflow.is_set() and stop_reason is None:
        stop_reason = "output"

    return ChildResult(
        stdout=bytes(buffers["stdout"][:max_output_bytes]).decode("utf-8", errors="replace"),
        stderr=bytes(buffers["stderr"][:max_output_bytes]).decode("utf-8", errors="replace"),
        returncode=child.returncode,
        timed_out=stop_reason == "timeout",
        output_limited=stop_reason == "output",
        file_limited=stop_reason == "file",
        cleanup_complete=cleanup_complete,
    )


def _hostport_key(host: str, port: int) -> str:
    try:
        normalized = host.encode("idna").decode("ascii").lower().rstrip(".")
    except UnicodeError:
        raise PolicyError("DNS_NAME_INVALID") from None
    if ":" in normalized:
        normalized = f"[{normalized}]"
    return f"{normalized}:{port}"


def _resolve_addresses(host: str, port: int, *, timeout_s: float = 5.0) -> tuple[str, ...]:
    try:
        literal = ipaddress.ip_address(host)
        return (str(literal),)
    except ValueError:
        pass
    if timeout_s <= 0:
        raise PolicyError("DNS_TIMEOUT")
    code = ("import socket,json,sys; "
            "a=socket.getaddrinfo(sys.argv[1],int(sys.argv[2]),type=socket.SOCK_STREAM); "
            "print(json.dumps(list(dict.fromkeys(x[4][0].split('%',1)[0] for x in a))))")
    answer = run_bounded([sys.executable, "-c", code, host, str(port)], timeout_s, max_output_bytes=8192)
    if answer.timed_out:
        raise PolicyError("DNS_TIMEOUT")
    if answer.returncode != 0 or answer.output_limited or not answer.cleanup_complete:
        raise PolicyError("DNS_UNAVAILABLE") from None
    try:
        values = json.loads(answer.stdout)
        if not isinstance(values, list) or len(values) > 64:
            raise ValueError
        result = tuple(str(ipaddress.ip_address(value)) for value in values)
    except (ValueError, TypeError):
        raise PolicyError("DNS_UNAVAILABLE") from None
    if not result:
        raise PolicyError("DNS_EMPTY")
    return result


def validate_network_target(
    url: str,
    source: str,
    network: NetworkConfig,
    allow_private: bool,
    address_resolver: Callable[[str, int], Sequence[str]] = _resolve_addresses,
    *,
    thumbnail: bool = False,
    deadline: float | None = None,
) -> tuple[str, ...]:
    try:
        host, port = _static_url(url, source, thumbnail=thumbnail)
    except ConfigError as exc:
        raise PolicyError("URL_POLICY_REJECTED") from exc
    try:
        if address_resolver is _resolve_addresses:
            timeout = _remaining(5.0, deadline)
            if timeout is None:
                raise PolicyError("DNS_TIMEOUT")
            addresses = _resolve_addresses(host, port, timeout_s=timeout)
        else:
            addresses = tuple(address_resolver(host, port))
        parsed = [ipaddress.ip_address(address) for address in addresses]
    except (ValueError, OSError, PolicyError):
        raise PolicyError("DNS_UNAVAILABLE") from None
    if not parsed:
        raise PolicyError("DNS_EMPTY")
    key = _hostport_key(host, port)
    explicit = allow_private and key in network.private_allowlist
    for address in parsed:
        if str(address) in CLOUD_METADATA_IPS:
            raise PolicyError("CLOUD_METADATA_DENIED")
        if not address.is_global and not explicit:
            raise PolicyError("PRIVATE_NETWORK_DENIED")
    return tuple(str(address) for address in parsed)


def build_ytdlp_argv(executable: str, url: str) -> list[str]:
    """Fixed metadata-only resolver command; no config, plugins, cookies, or download."""
    return build_argv(executable, *YTDLP_FLAGS, "--", url)


def _not_none(value: Any) -> bool:
    return value not in (None, "", "none", "None")


def _format_rank(item: Mapping[str, Any]) -> tuple[float, float]:
    try:
        height = float(item.get("height") or 0)
    except (TypeError, ValueError):
        height = 0.0
    try:
        bitrate = float(item.get("tbr") or item.get("abr") or 0)
    except (TypeError, ValueError):
        bitrate = 0.0
    return (height if height <= 720 else 0.0, bitrate)


def _within_video_limit(item: Mapping[str, Any]) -> bool:
    value = item.get("height")
    if value in (None, "", 0, "0"):
        return True
    try:
        return float(value) <= 720
    except (TypeError, ValueError):
        return False


def select_resolved_media(
    payload: Any,
    stream: StreamConfig,
    network: NetworkConfig,
    address_resolver: Callable[[str, int], Sequence[str]] = _resolve_addresses,
    *, deadline: float | None = None,
) -> tuple[ResolvedMedia | None, str | None, str | None]:
    """Return media, explicit off-air reason, or a non-sensitive UNKNOWN reason."""
    if not isinstance(payload, dict):
        return None, None, "RESOLVER_JSON_INVALID"
    live_status = payload.get("live_status")
    if stream.expected_live and live_status in OFF_AIR_LIVE_STATUS:
        return None, "RESOLVER_NOT_LIVE", None
    if stream.expected_live and live_status in UPCOMING_LIVE_STATUS:
        return None, "RESOLVER_UPCOMING", None

    formats = payload.get("formats")
    candidates = [item for item in formats if isinstance(item, dict)] if isinstance(formats, list) else []
    top_url = payload.get("url")
    if isinstance(top_url, str):
        candidates.append({
            "url": top_url, "vcodec": payload.get("vcodec"), "acodec": payload.get("acodec"),
            "height": payload.get("height"), "tbr": payload.get("tbr"),
        })
    candidates = [item for item in candidates if isinstance(item.get("url"), str)]
    video = [item for item in candidates if _not_none(item.get("vcodec")) and _within_video_limit(item)]
    audio = [item for item in candidates if _not_none(item.get("acodec"))]
    muxed = [item for item in candidates if _not_none(item.get("vcodec")) and _not_none(item.get("acodec")) and _within_video_limit(item)]

    chosen: list[MediaInput] = []
    if stream.expect_video and stream.expect_audio and muxed:
        item = max(muxed, key=_format_rank)
        chosen.append(MediaInput("muxed", item["url"]))
    else:
        video_only = [item for item in video if not _not_none(item.get("acodec"))]
        audio_only = [item for item in audio if not _not_none(item.get("vcodec"))]
        if stream.expect_video and video:
            chosen.append(MediaInput("video", max(video_only or video, key=_format_rank)["url"]))
        if stream.expect_audio and audio:
            chosen.append(MediaInput("audio", max(audio_only or audio, key=_format_rank)["url"]))
    if not chosen or len(chosen) > 2:
        return None, None, "RESOLVER_MEDIA_MISSING"
    for media in chosen:
        try:
            validate_network_target(media.url, "direct", network, stream.allow_private, address_resolver,
                                    deadline=deadline)
        except PolicyError:
            return None, None, "RESOLVER_MEDIA_URL_REJECTED"
    return ResolvedMedia(tuple(chosen)), None, None


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req: Any, fp: Any, code: int, msg: str, headers: Any, newurl: str) -> None:
        return None


def _connect_pinned(connection: Any, addresses: Sequence[str]) -> None:
    """Connect to already checked numeric addresses without a second DNS lookup."""
    deadline = time.monotonic() + float(connection.timeout)
    for address in addresses:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            break
        sock = socket.socket(socket.AF_INET6 if ":" in address else socket.AF_INET, socket.SOCK_STREAM)
        try:
            sock.settimeout(remaining)
            sock.connect((address, connection.port))
            connection.sock = sock
            return
        except OSError:
            sock.close()
    raise OSError("HTTP_CONNECT_FAILED")


def _pinned_handlers(addresses: Sequence[str]) -> tuple[Any, Any]:
    class HTTPConnection(http.client.HTTPConnection):
        def connect(self) -> None:
            _connect_pinned(self, addresses)

    class HTTPSConnection(http.client.HTTPSConnection):
        def connect(self) -> None:
            _connect_pinned(self, addresses)
            try:
                self.sock = self._context.wrap_socket(self.sock, server_hostname=self.host)
            except BaseException:
                self.sock.close()
                raise

    class HTTPHandler(urllib.request.HTTPHandler):
        def http_open(self, request: Any) -> Any:
            return self.do_open(HTTPConnection, request)

    class HTTPSHandler(urllib.request.HTTPSHandler):
        def https_open(self, request: Any) -> Any:
            return self.do_open(HTTPSConnection, request, context=self._context)

    return HTTPHandler(), HTTPSHandler()


def _read_http_body(response: Any, max_bytes: int, deadline: float) -> tuple[bytes, bool, bool]:
    body = bytearray()
    while len(body) <= max_bytes:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            return bytes(body[:max_bytes]), True, True
        raw = getattr(getattr(getattr(response, "fp", None), "raw", None), "_sock", None)
        if raw is not None:
            try:
                raw.settimeout(remaining)
            except OSError:
                pass
        read = getattr(response, "read1", response.read)
        try:
            chunk = read(min(4096, max_bytes + 1 - len(body)))
        except (TimeoutError, socket.timeout):
            return bytes(body[:max_bytes]), True, True
        if not chunk:
            return bytes(body), False, False
        body.extend(chunk)
        if len(body) > max_bytes:
            return bytes(body[:max_bytes]), True, False
    return bytes(body[:max_bytes]), True, False


def bounded_http_get(
    url: str,
    *,
    source: str,
    network: NetworkConfig,
    allow_private: bool,
    timeout_s: float,
    max_bytes: int,
    address_resolver: Callable[[str, int], Sequence[str]] = _resolve_addresses,
    range_request: bool = True,
    deadline: float | None = None,
) -> HttpResult:
    """Proxy-free bounded GET with each redirect independently revalidated."""
    current = url
    operation_deadline = time.monotonic() + timeout_s
    if deadline is not None:
        operation_deadline = min(operation_deadline, deadline)
    for redirect_count in range(network.max_redirects + 1):
        try:
            addresses = validate_network_target(current, source, network, allow_private, address_resolver,
                                                deadline=operation_deadline)
        except PolicyError as exc:
            return HttpResult(None, current, "", b"", error_code=str(exc))
        hop_timeout = operation_deadline - time.monotonic()
        if hop_timeout <= 0:
            return HttpResult(None, current, "", b"", error_code="HTTP_TIMEOUT")
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), _NoRedirect(),
                                            *_pinned_handlers(addresses or ()))
        headers = {"User-Agent": f"Dead-Air/{VERSION}", "Accept": "*/*"}
        if range_request:
            headers["Range"] = f"bytes=0-{max_bytes - 1}"
        request = urllib.request.Request(current, headers=headers, method="GET")
        try:
            response = opener.open(request, timeout=hop_timeout)
        except urllib.error.HTTPError as exc:
            response = exc
        except (OSError, urllib.error.URLError, ValueError):
            return HttpResult(None, current, "", b"", error_code="HTTP_REQUEST_FAILED")
        try:
            status = int(getattr(response, "status", getattr(response, "code", 0)) or 0)
            location = response.headers.get("Location") if getattr(response, "headers", None) else None
            if 300 <= status < 400 and location:
                response.close()
                if redirect_count >= network.max_redirects:
                    return HttpResult(status, current, "", b"", error_code="REDIRECT_LIMIT")
                current = urllib.parse.urljoin(current, location)
                continue
            content_type = ""
            if getattr(response, "headers", None):
                content_type = response.headers.get_content_type().lower()
            body, truncated, timed_out = _read_http_body(response, max_bytes, operation_deadline)
            return HttpResult(
                status=status, final_url=current, content_type=content_type,
                body=body, truncated=truncated,
                error_code="HTTP_TIMEOUT" if timed_out else None,
            )
        except (OSError, ValueError):
            return HttpResult(None, current, "", b"", error_code="HTTP_READ_FAILED")
        finally:
            try:
                response.close()
            except OSError:
                pass
    return HttpResult(None, current, "", b"", error_code="REDIRECT_LIMIT")


def _off_air_from_direct(result: HttpResult, stream: StreamConfig) -> str | None:
    if not stream.expected_live:
        return None
    if result.status in stream.offline_http_statuses:
        return "DIRECT_OFFLINE_STATUS"
    path = urllib.parse.urlsplit(result.final_url).path.lower()
    is_hls = path.endswith(".m3u8") or "mpegurl" in result.content_type
    if (is_hls and result.error_code is None and result.status is not None
            and 200 <= result.status < 300 and not result.truncated):
        lines = result.body.splitlines()
        if lines and lines[0].lstrip(b"\xef\xbb\xbf").strip() == b"#EXTM3U" \
                and any(line.strip() == b"#EXT-X-ENDLIST" for line in lines[1:]):
            return "DIRECT_HLS_ENDLIST"
    return None


def parse_probe_json(text: str) -> ProbeEvidence:
    try:
        payload = json.loads(text)
    except (TypeError, json.JSONDecodeError):
        raise ValueError("PROBE_JSON_INVALID") from None
    if not isinstance(payload, dict) or not isinstance(payload.get("streams"), list):
        raise ValueError("PROBE_STREAMS_MISSING")
    audio = 0
    video = 0
    audio_index = video_index = None
    audio_time_base = video_time_base = None
    audio_sample_rate = None
    for item in payload["streams"]:
        if not isinstance(item, dict):
            raise ValueError("PROBE_STREAM_INVALID")
        kind = item.get("codec_type")
        index = _probe_integer(item.get("index"))
        time_base = _probe_time_base(item.get("time_base"))
        if kind == "audio":
            audio += 1
            if audio == 1:
                audio_index = index
                audio_time_base = time_base
                audio_sample_rate = _probe_integer(item.get("sample_rate"))
        elif kind == "video":
            video += 1
            if video == 1:
                video_index = index
                video_time_base = time_base
    duration: float | None = None
    start: float | None = None
    fmt = payload.get("format")
    if isinstance(fmt, dict):
        if fmt.get("duration") not in (None, "N/A", ""):
            duration = _probe_float(fmt["duration"])
            if duration is None or duration < 0:
                raise ValueError("PROBE_DURATION_INVALID")
        if fmt.get("start_time") not in (None, "N/A", ""):
            start = _probe_float(fmt["start_time"])
            if start is None:
                raise ValueError("PROBE_START_INVALID")
    raw_frames = payload.get("frames", [])
    if not isinstance(raw_frames, list):
        raise ValueError("PROBE_FRAMES_INVALID")
    selected = {value for value in (audio_index, video_index) if value is not None}
    frames = tuple(
        item for item in raw_frames
        if isinstance(item, dict) and _probe_integer(item.get("stream_index")) in selected
    )
    if any(not isinstance(item, dict) for item in raw_frames):
        raise ValueError("PROBE_FRAME_INVALID")
    return ProbeEvidence(
        audio, video, duration, start, audio_index, audio_time_base, audio_sample_rate,
        video_index, video_time_base, frames,
    )


def _probe_float(value: Any) -> float | None:
    if isinstance(value, bool) or value in (None, "", "N/A"):
        return None
    try:
        result = float(value)
    except (TypeError, ValueError, OverflowError):
        return None
    return result if math.isfinite(result) and abs(result) <= 86400 else None


def _probe_integer(value: Any) -> int | None:
    if isinstance(value, bool) or value in (None, "", "N/A"):
        return None
    try:
        result = int(value)
    except (TypeError, ValueError, OverflowError):
        return None
    if isinstance(value, float) and value != result:
        return None
    return result if result >= 0 else None


def _probe_time_base(value: Any) -> float | None:
    if not isinstance(value, str):
        return None
    try:
        result = float(Fraction(value))
    except (ValueError, ZeroDivisionError, OverflowError):
        return None
    return result if math.isfinite(result) and result > 0 else None


def _validated_window_offset(value: Any) -> float:
    if (isinstance(value, bool) or not isinstance(value, (int, float))
            or not math.isfinite(value) or value < 0 or value > CAPTURE_STARTUP_S):
        raise ValueError("WINDOW_OFFSET_INVALID")
    return float(value)


def _validated_window_bounds(value: tuple[float, float]) -> tuple[float, float]:
    if not isinstance(value, tuple) or len(value) != 2:
        raise ValueError("WINDOW_BOUNDS_INVALID")
    start, end = value
    if any(isinstance(item, bool) or not isinstance(item, (int, float))
           or not math.isfinite(item) or abs(item) > 172800 for item in value):
        raise ValueError("WINDOW_BOUNDS_INVALID")
    if end <= start:
        raise ValueError("WINDOW_BOUNDS_INVALID")
    return float(start), float(end)


def _decoded_coverage(
    probe: ProbeEvidence, name: str, sample_s: float, *,
    window_offset_s: float = 0.0, window_bounds: tuple[float, float] | None = None,
) -> CoverageEvidence:
    """Measure selected-stream decoded intervals; never fill timestamp gaps."""
    window_offset_s = _validated_window_offset(window_offset_s)
    audio = name == "AUDIO"
    stream_index = probe.audio_index if audio else probe.video_index
    tick = probe.audio_time_base_s if audio else probe.video_time_base_s
    valid_tick = tick is not None and 0 < tick <= 0.005
    if window_bounds is not None:
        origin, window_end = _validated_window_bounds(window_bounds)
    else:
        origin = probe.start_s + window_offset_s if probe.start_s is not None else None
        window_end = origin + sample_s if origin is not None else None
    intervals: list[tuple[float, float]] = []
    decoded_amount_s = 0.0
    sample_count = 0
    invalid = origin is None or stream_index is None or not valid_tick
    rate = probe.audio_sample_rate if audio else None
    if audio and (rate is None or rate <= 0 or rate > 768000):
        invalid = True

    for frame in probe.frames:
        if _probe_integer(frame.get("stream_index")) != stream_index:
            continue
        if frame.get("media_type") not in (None, "audio" if audio else "video"):
            invalid = True
            continue
        pts = _probe_float(frame.get("pts_time"))
        if pts is None:
            pts = _probe_float(frame.get("best_effort_timestamp_time"))
        if pts is None:
            invalid = True
            continue
        if audio:
            samples = _probe_integer(frame.get("nb_samples"))
            if samples is None or samples <= 0 or rate is None or not 0 < rate <= 768000:
                invalid = True
                continue
            duration = samples / rate
        else:
            duration = _probe_float(frame.get("duration_time"))
            if duration is None or duration <= 0:
                duration = _probe_float(frame.get("pkt_duration_time"))
            if duration is None or duration <= 0:
                invalid = True
                continue
        if not math.isfinite(duration) or duration <= 0 or duration > 86400:
            invalid = True
            continue
        end = pts + duration
        if not math.isfinite(end) or abs(end) > 172800:
            invalid = True
            continue
        if origin is None or window_end is None:
            continue
        start_clip, end_clip = max(pts, origin), min(end, window_end)
        if end_clip <= start_clip:
            continue
        start_rel, end_rel = start_clip - origin, end_clip - origin
        intervals.append((start_rel, end_rel))
        decoded_amount_s += end_rel - start_rel
        if audio:
            if start_clip == pts and end_clip == end:
                included_samples = samples
            else:
                included_samples = min(
                    samples,
                    int(math.floor(samples * (end_clip - start_clip) / duration)),
                )
            sample_count += included_samples

    intervals.sort()
    merged: list[list[float]] = []
    for start, end in intervals:
        if merged and start <= merged[-1][1]:
            merged[-1][1] = max(merged[-1][1], end)
        else:
            merged.append([start, end])
    covered_s = sum(end - start for start, end in merged)
    gaps: list[float] = []
    if merged:
        gaps.append(merged[0][0])
        gaps.extend(max(0.0, right[0] - left[1]) for left, right in zip(merged, merged[1:]))
        gaps.append(max(0.0, sample_s - merged[-1][1]))
    else:
        gaps.append(sample_s)
    largest_gap_s = max(gaps, default=sample_s)
    frame_count = len(intervals)
    # A timestamp tick can move each frame boundary, but cannot hide a long gap.
    quantization_budget = (tick or 0.0) * (frame_count + 1)
    complete = (
        not invalid and frame_count >= (1 if audio else 2)
        and largest_gap_s <= (tick or 0.0)
        and covered_s + quantization_budget >= sample_s
        and (not audio or sample_count / rate + (tick or 0.0) >= sample_s)
    )
    return CoverageEvidence(
        covered_s, decoded_amount_s, frame_count, sample_count, rate,
        merged[0][0] if merged else None,
        merged[-1][1] if merged else None,
        largest_gap_s, complete, window_offset_s,
    )


def parse_progress(text: str) -> ProgressEvidence:
    fields: dict[str, str] = {}
    for line in text.splitlines():
        if "=" in line:
            key, value = line.split("=", 1)
            fields[key.strip()] = value.strip()
    decoded: float | None = None
    raw_us = fields.get("out_time_us") or fields.get("out_time_ms")
    if raw_us and raw_us not in {"N/A", "-1"}:
        try:
            decoded = int(raw_us) / 1_000_000
        except ValueError:
            raise ValueError("PROGRESS_TIME_INVALID") from None
    elif fields.get("out_time"):
        parts = fields["out_time"].split(":")
        try:
            if len(parts) == 3:
                decoded = float(parts[0]) * 3600 + float(parts[1]) * 60 + float(parts[2])
        except ValueError:
            raise ValueError("PROGRESS_TIME_INVALID") from None
    try:
        frames = int(fields.get("frame", "0").strip())
    except ValueError:
        frames = 0
    if decoded is not None and (not math.isfinite(decoded) or decoded < 0):
        raise ValueError("PROGRESS_TIME_INVALID")
    return ProgressEvidence(fields.get("progress") == "end", decoded, max(0, frames))


def _parse_intervals(log: str, decoded_s: float) -> tuple[tuple[Interval, ...], tuple[Interval, ...]]:
    pending: dict[str, float | None] = {"freeze": None, "silence": None}
    patterns = {
        "freeze_start": ("freeze", _FREEZE_START, True),
        "freeze_end": ("freeze", _FREEZE_END, False),
        "silence_start": ("silence", _SILENCE_START, True),
        "silence_end": ("silence", _SILENCE_END, False),
    }
    found: dict[str, list[Interval]] = {"freeze": [], "silence": []}
    for line in log.splitlines():
        for marker, (kind, pattern, is_start) in patterns.items():
            if marker not in line:
                continue
            match = pattern.search(line)
            if match is None:
                raise ValueError("DETECTOR_INTERVAL_MALFORMED")
            value = float(match.group(1))
            if not math.isfinite(value) or value < 0:
                raise ValueError("DETECTOR_INTERVAL_INVALID")
            if is_start:
                if pending[kind] is not None:
                    raise ValueError("DETECTOR_INTERVAL_OVERLAP")
                pending[kind] = value
            else:
                start = pending[kind]
                if start is None or value < start:
                    raise ValueError("DETECTOR_INTERVAL_UNPAIRED")
                found[kind].append(Interval(start, value, False))
                pending[kind] = None
            break
    for kind, start in pending.items():
        if start is not None:
            if decoded_s < start:
                raise ValueError("DETECTOR_INTERVAL_AFTER_DECODE_END")
            found[kind].append(Interval(start, decoded_s, True))
    return tuple(found["freeze"]), tuple(found["silence"])


def _dimension(
    *,
    name: str,
    expected: bool,
    present: bool,
    coverage_ok: bool,
    decoded_s: float | None,
    sampled_s: float,
    intervals: tuple[Interval, ...],
    window_s: float,
    coverage: CoverageEvidence | None = None,
    detector_enabled: bool = True,
    decode_error: str | None = None,
) -> DimensionEvidence:
    if not expected:
        return DimensionEvidence("NOT_APPLICABLE", (f"{name}_NOT_EXPECTED",), sampled_s=sampled_s, decoded_s=decoded_s, coverage=coverage)
    if not present:
        return DimensionEvidence("UNKNOWN", (f"{name}_MISSING",), sampled_s=sampled_s, decoded_s=decoded_s, coverage=coverage)
    if decode_error:
        return DimensionEvidence("UNKNOWN", (decode_error,), sampled_s=sampled_s, decoded_s=decoded_s, coverage=coverage)
    if not detector_enabled:
        return DimensionEvidence("UNKNOWN", (f"{name}_DETECTOR_DISABLED",), sampled_s=sampled_s, decoded_s=decoded_s, coverage=coverage)
    if not coverage_ok:
        return DimensionEvidence("UNKNOWN", (f"{name}_COVERAGE_INCOMPLETE",), sampled_s=sampled_s, decoded_s=decoded_s, coverage=coverage)
    if any((item.end_s - item.start_s) + 0.1 >= window_s for item in intervals):
        reason = "VIDEO_FREEZE" if name == "VIDEO" else "AUDIO_SILENCE"
        return DimensionEvidence("UNHEALTHY", (reason,), intervals, sampled_s, decoded_s, coverage)
    return DimensionEvidence("HEALTHY", (f"{name}_DECODED_NO_EVENT",), intervals, sampled_s, decoded_s, coverage)


def _overall(audio: DimensionEvidence, video: DimensionEvidence) -> tuple[str, tuple[str, ...]]:
    dimensions = (audio, video)
    bad = tuple(reason for item in dimensions if item.status == "UNHEALTHY" for reason in item.reason_codes)
    if bad:
        return "UNHEALTHY", bad
    unknown = tuple(reason for item in dimensions if item.status == "UNKNOWN" for reason in item.reason_codes)
    if unknown:
        return "UNKNOWN", unknown
    return "HEALTHY", tuple(reason for item in dimensions for reason in item.reason_codes)


def _unknown_observation(stream: StreamConfig, reason: str, observed_at: str | None = None) -> Observation:
    unknown_audio = DimensionEvidence("UNKNOWN", (reason,)) if stream.expect_audio else DimensionEvidence("NOT_APPLICABLE", ("AUDIO_NOT_EXPECTED",))
    unknown_video = DimensionEvidence("UNKNOWN", (reason,)) if stream.expect_video else DimensionEvidence("NOT_APPLICABLE", ("VIDEO_NOT_EXPECTED",))
    return Observation(stream.id, observed_at or utc_now(), "UNKNOWN", (reason,), unknown_audio, unknown_video)


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z")


def _remaining(configured_s: float, deadline: float | None) -> float | None:
    if deadline is None:
        return configured_s
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        return None
    return min(configured_s, remaining)


def _local_sample_path(source: str | os.PathLike[str]) -> Path:
    raw = os.fspath(source)
    if "://" in raw or raw.lower().startswith(("http:", "https:", "rtmp:", "rtsp:", "udp:", "tcp:", "lavfi:")):
        raise ValueError("SOURCE_NOT_LOCAL")
    if raw.startswith(("\\\\", "//")) or PureWindowsPath(raw).anchor.startswith("\\\\"):
        raise ValueError("SOURCE_NETWORK_PATH")
    candidate = Path(raw).expanduser()
    if candidate.is_symlink():
        raise ValueError("SOURCE_SYMLINK")
    path = candidate.resolve(strict=True)
    resolved = os.fspath(path)
    if resolved.startswith("//") or PureWindowsPath(resolved).anchor.startswith("\\\\"):
        raise ValueError("SOURCE_NETWORK_PATH")
    if not path.is_file():
        raise ValueError("SOURCE_NOT_FILE")
    return path


def build_decode_argv(
    path: Path, stream: StreamConfig, config: AppConfig,
    present_audio: bool, present_video: bool, track: str,
    *, window_bounds: tuple[float, float] | None = None,
) -> list[str]:
    bounds = _validated_window_bounds(window_bounds) if window_bounds is not None else None
    args: list[str] = [
        "-nostdin", "-hide_banner", "-loglevel", "info", "-protocol_whitelist", "file",
        "-threads", "1", "-filter_threads", "1",
    ]
    if bounds is not None:
        args.append("-copyts")
    args.extend(["-i", str(path)])
    filter_start = format(bounds[0], ".9f") if bounds is not None else None
    filter_end = format(bounds[1], ".9f") if bounds is not None else None
    use_video = track == "video" and stream.expect_video and present_video
    use_audio = track == "audio" and stream.expect_audio and present_audio
    if not (use_video or use_audio):
        raise ValueError("DECODE_TRACK_UNAVAILABLE")
    if use_video:
        video_filter = f"freezedetect=n={stream.freeze_noise}:d={stream.freeze_window_s}"
        if bounds is not None:
            video_filter = (
                f"trim=start={filter_start}:end={filter_end},"
                f"setpts=PTS-({filter_start})/TB,{video_filter}"
            )
        args.extend(["-map", "0:v:0", "-vf", video_filter])
    if use_audio:
        audio_filter = f"silencedetect=n={stream.silence_db}dB:d={stream.silence_window_s}"
        if bounds is not None:
            audio_filter = (
                f"atrim=start={filter_start}:end={filter_end},"
                f"asetpts=PTS-({filter_start})/TB,{audio_filter}"
            )
        args.extend(["-map", "0:a:0", "-af", audio_filter])
    args.extend([
        "-t", str(config.watch.sample_s), "-progress", "pipe:1", "-nostats", "-f", "null", "-",
    ])
    return build_argv(config.tools.ffmpeg, *args)


def inspect_sample(
    source: str | os.PathLike[str],
    stream: StreamConfig,
    config: AppConfig,
    *,
    runner: Callable[..., ChildResult] = run_bounded,
    deadline: float | None = None,
    window_offset_s: float = 0.0,
) -> Observation:
    """Inspect one bounded local file. Freeze and silence are separate dimensions."""
    observed_at = utc_now()
    try:
        window_offset_s = _validated_window_offset(window_offset_s)
    except ValueError:
        return _unknown_observation(stream, "SAMPLE_WINDOW_INVALID", observed_at)
    try:
        path = _local_sample_path(source)
        if path.stat().st_size <= 0 or path.stat().st_size > config.watch.max_sample_bytes:
            return _unknown_observation(stream, "SAMPLE_SIZE_INVALID", observed_at)
    except (OSError, ValueError):
        return _unknown_observation(stream, "SAMPLE_UNAVAILABLE_OR_NOT_LOCAL", observed_at)

    probe_argv = build_argv(
        config.tools.ffprobe,
        "-protocol_whitelist", "file", "-v", "error", "-threads", "1", "-show_frames",
        "-read_intervals", f"%+{config.watch.sample_s + window_offset_s}",
        "-show_entries",
        "format=start_time,duration:stream=index,codec_type,sample_rate,time_base:"
        "frame=stream_index,media_type,pts_time,best_effort_timestamp_time,duration_time,pkt_duration_time,nb_samples",
        "-of", "json", str(path),
    )
    timeout = _remaining(config.watch.analysis_timeout_s, deadline)
    if timeout is None:
        return _unknown_observation(stream, "CYCLE_BUDGET_EXHAUSTED", observed_at)
    probe_result = runner(
        probe_argv, timeout,
        max_output_bytes=config.watch.max_output_bytes,
    )
    if probe_result.timed_out or probe_result.output_limited or probe_result.file_limited or not probe_result.cleanup_complete:
        return _unknown_observation(stream, "PROBE_BOUNDED_FAILURE", observed_at)
    if probe_result.launch_error or probe_result.returncode != 0:
        return _unknown_observation(stream, "PROBE_FAILED", observed_at)
    try:
        probe = parse_probe_json(probe_result.stdout)
    except ValueError as exc:
        return _unknown_observation(stream, str(exc), observed_at)

    if probe.start_s is None and window_offset_s > 0:
        return _unknown_observation(stream, "PROBE_START_MISSING", observed_at)
    window_bounds = None
    if probe.start_s is not None:
        window_start_s = probe.start_s + window_offset_s
        window_end_s = window_start_s + config.watch.sample_s
        try:
            window_bounds = _validated_window_bounds((window_start_s, window_end_s))
        except ValueError:
            return _unknown_observation(stream, "PROBE_WINDOW_INVALID", observed_at)

    present_audio = probe.audio_streams > 0
    present_video = probe.video_streams > 0
    def decode_dimension(name: str, present: bool) -> DimensionEvidence:
        expected = stream.expect_audio if name == "AUDIO" else stream.expect_video
        window = stream.silence_window_s if name == "AUDIO" else stream.freeze_window_s
        enabled = True if name == "AUDIO" else stream.detect_freeze
        if not expected or not present:
            return _dimension(
                name=name, expected=expected, present=present, coverage_ok=False,
                decoded_s=None, sampled_s=config.watch.sample_s, intervals=(),
                window_s=window, detector_enabled=enabled,
            )
        timeout = _remaining(config.watch.analysis_timeout_s, deadline)
        if timeout is None:
            return _dimension(
                name=name, expected=True, present=True, coverage_ok=False, decoded_s=None,
                sampled_s=config.watch.sample_s, intervals=(), window_s=window,
                detector_enabled=enabled, decode_error="CYCLE_BUDGET_EXHAUSTED",
            )
        try:
            decoded = runner(
                build_decode_argv(
                    path, stream, config, present_audio, present_video, name.lower(),
                    window_bounds=window_bounds if window_offset_s > 0 else None,
                ),
                timeout, max_output_bytes=config.watch.max_output_bytes,
            )
        except (OSError, ValueError):
            return _dimension(
                name=name, expected=True, present=True, coverage_ok=False, decoded_s=None,
                sampled_s=config.watch.sample_s, intervals=(), window_s=window,
                detector_enabled=enabled, decode_error="DECODE_FAILED",
            )
        if decoded.timed_out or decoded.output_limited or decoded.file_limited or not decoded.cleanup_complete:
            error = "DECODE_BOUNDED_FAILURE"
            return _dimension(name=name, expected=True, present=True, coverage_ok=False,
                              decoded_s=None, sampled_s=config.watch.sample_s, intervals=(),
                              window_s=window, detector_enabled=enabled, decode_error=error)
        if decoded.launch_error or decoded.returncode != 0:
            return _dimension(name=name, expected=True, present=True, coverage_ok=False,
                              decoded_s=None, sampled_s=config.watch.sample_s, intervals=(),
                              window_s=window, detector_enabled=enabled, decode_error="DECODE_FAILED")
        try:
            progress = parse_progress(decoded.stdout)
            if not progress.complete:
                raise ValueError("DECODE_COVERAGE_MISSING")
        except ValueError as exc:
            return _dimension(name=name, expected=True, present=True, coverage_ok=False,
                              decoded_s=None, sampled_s=config.watch.sample_s, intervals=(),
                              window_s=window, detector_enabled=enabled, decode_error=str(exc))
        coverage = _decoded_coverage(
            probe, name, config.watch.sample_s, window_offset_s=window_offset_s,
            window_bounds=window_bounds,
        )
        decoded_s = coverage.covered_s
        detector_end_s = coverage.last_s or 0.0
        try:
            freezes, silences = _parse_intervals(decoded.stderr, detector_end_s)
        except ValueError as exc:
            return _dimension(name=name, expected=True, present=True, coverage_ok=False,
                              decoded_s=decoded_s, sampled_s=config.watch.sample_s, intervals=(),
                              window_s=window, coverage=coverage,
                              detector_enabled=enabled, decode_error=str(exc))
        intervals = silences if name == "AUDIO" else freezes
        return _dimension(
            name=name, expected=True, present=True, coverage_ok=coverage.complete,
            decoded_s=decoded_s, sampled_s=config.watch.sample_s,
            intervals=intervals, window_s=window, coverage=coverage,
            detector_enabled=enabled,
        )

    audio = decode_dimension("AUDIO", present_audio)
    video = decode_dimension("VIDEO", present_video)
    status, reasons = _overall(audio, video)
    return Observation(stream.id, observed_at, status, reasons, audio, video)


def build_capture_argv(media: ResolvedMedia, stream: StreamConfig, config: AppConfig, output: Path) -> list[str]:
    args = [
        "-nostdin", "-hide_banner", "-loglevel", "error", "-threads", "1",
        "-filter_threads", "1",
    ]
    for item in media.inputs:
        args.extend(["-protocol_whitelist", INPUT_PROTOCOLS])
        # Older FFmpeg builds reject unused TLS options on plain HTTP inputs.
        if urllib.parse.urlsplit(item.url).scheme.lower() == "https":
            args.extend(["-tls_verify", "1"])
        args.extend(["-rw_timeout", "10000000", "-i", item.url])
    maps: list[str] = []
    if stream.expect_video:
        video_index = next((i for i, item in enumerate(media.inputs) if item.kind in {"video", "muxed"}), None)
        if video_index is not None:
            maps.extend(["-map", f"{video_index}:v:0?"])
    if stream.expect_audio:
        audio_index = next((i for i, item in enumerate(media.inputs) if item.kind in {"audio", "muxed"}), None)
        if audio_index is not None:
            maps.extend(["-map", f"{audio_index}:a:0?"])
    if not maps:
        raise ValueError("RESOLVED_MEDIA_HAS_NO_EXPECTED_TRACK")
    args.extend([
        "-t", str(config.watch.sample_s + CAPTURE_STARTUP_S), *maps,
        "-c", "copy", "-fs", str(config.watch.max_sample_bytes),
        "-f", "matroska", str(output),
    ])
    return build_argv(config.tools.ffmpeg, *args)


def capture_and_inspect(
    media: ResolvedMedia,
    stream: StreamConfig,
    config: AppConfig,
    *,
    runner: Callable[..., ChildResult] = run_bounded,
    deadline: float | None = None,
) -> Observation:
    """Capture a short local Matroska sample, inspect it, and remove it on exit."""
    try:
        with tempfile.TemporaryDirectory(prefix="dead-air-") as directory:
            output = Path(directory) / "sample.mkv"
            argv = build_capture_argv(media, stream, config, output)
            timeout = _remaining(config.watch.capture_timeout_s, deadline)
            if timeout is None:
                return _unknown_observation(stream, "CYCLE_BUDGET_EXHAUSTED")
            result = runner(
                argv, timeout,
                max_output_bytes=config.watch.max_output_bytes,
                file_limits=((output, config.watch.max_sample_bytes),),
            )
            if result.timed_out or result.output_limited or result.file_limited or not result.cleanup_complete:
                return _unknown_observation(stream, "CAPTURE_BOUNDED_FAILURE")
            if result.launch_error or result.returncode != 0:
                return _unknown_observation(stream, "CAPTURE_FAILED")
            try:
                size = output.stat().st_size
            except OSError:
                return _unknown_observation(stream, "CAPTURE_FILE_MISSING")
            if size <= 0 or size >= config.watch.max_sample_bytes:
                return _unknown_observation(stream, "CAPTURE_SIZE_INVALID")
            return inspect_sample(
                output, stream, config, runner=runner, deadline=deadline,
                window_offset_s=CAPTURE_STARTUP_S,
            )
    except (OSError, ValueError):
        return _unknown_observation(stream, "CAPTURE_SETUP_FAILED")


def _parse_resolver(stdout: str, stream: StreamConfig, config: AppConfig, address_resolver: Callable[[str, int], Sequence[str]], *, deadline: float | None = None) -> tuple[ResolvedMedia | None, str | None, str | None]:
    try:
        payload = json.loads(stdout)
    except (TypeError, json.JSONDecodeError):
        return None, None, "RESOLVER_JSON_INVALID"
    return select_resolved_media(payload, stream, config.network, address_resolver, deadline=deadline)


def _youtube_thumbnail_url(url: str) -> str | None:
    try:
        parsed = urllib.parse.urlsplit(url)
        host = (parsed.hostname or "").lower()
        if host == "youtu.be":
            video_id = parsed.path.strip("/").split("/", 1)[0]
        elif parsed.path.rstrip("/") == "/watch":
            video_id = urllib.parse.parse_qs(parsed.query).get("v", [""])[0]
        elif parsed.path.startswith("/live/"):
            video_id = parsed.path.removeprefix("/live/").split("/", 1)[0]
        else:
            return None
    except ValueError:
        return None
    if host not in YOUTUBE_HOSTS or not re.fullmatch(r"[A-Za-z0-9_-]{11}", video_id):
        return None
    return f"https://i.ytimg.com/vi/{video_id}/hqdefault.jpg"


def _fallback_observation(
    original: Observation,
    stream: StreamConfig,
    config: AppConfig,
    history: dict[str, ThumbnailBaseline],
    *,
    address_resolver: Callable[[str, int], Sequence[str]],
    now: float,
    deadline: float | None = None,
    transport: Callable[..., HttpResult] = bounded_http_get,
) -> Observation:
    thumbnail_url = stream.thumbnail_url or (
        _youtube_thumbnail_url(stream.url) if stream.source == "youtube" else None
    )
    if not thumbnail_url:
        return original
    try:
        validate_network_target(thumbnail_url, "direct", config.network, stream.allow_private,
                               address_resolver, thumbnail=True, deadline=deadline)
    except PolicyError:
        return original
    timeout = _remaining(config.watch.analysis_timeout_s, deadline)
    if timeout is None:
        return original
    fetched = transport(
        thumbnail_url, source="direct", network=config.network,
        allow_private=stream.allow_private, timeout_s=timeout,
        max_bytes=config.network.thumbnail_max_bytes, address_resolver=address_resolver,
        range_request=False, deadline=deadline,
    )
    if fetched.error_code or fetched.status is None or not 200 <= fetched.status < 300 or not fetched.content_type.startswith("image/") or fetched.truncated or not fetched.body:
        return original
    digest = hashlib.sha256(fetched.body).hexdigest()
    previous = history.get(stream.id)
    if previous is None:
        history[stream.id] = ThumbnailBaseline(digest, now)
        reason = "THUMBNAIL_BASELINE_ONLY"
    elif digest != previous.digest:
        history[stream.id] = ThumbnailBaseline(digest, now)
        reason = "THUMBNAIL_VISUAL_ACTIVITY_ONLY"
    elif now - previous.observed_monotonic >= stream.thumbnail_stale_after_s:
        reason = "THUMBNAIL_SUSPECT_STATIC"
    else:
        reason = "THUMBNAIL_UNCHANGED_BELOW_STALE_WINDOW"
    audio = DimensionEvidence("UNKNOWN", ("AUDIO_UNVERIFIED_BY_THUMBNAIL",))
    video = DimensionEvidence("DEGRADED", (reason,))
    return Observation(stream.id, original.observed_at, "DEGRADED", (reason, "FULL_MEDIA_UNVERIFIED"), audio, video)


def observe_stream(
    config: AppConfig,
    stream: StreamConfig,
    *,
    runner: Callable[..., ChildResult] = run_bounded,
    address_resolver: Callable[[str, int], Sequence[str]] = _resolve_addresses,
    thumbnail_history: dict[str, ThumbnailBaseline] | None = None,
    deadline: float | None = None,
    transport: Callable[..., HttpResult] = bounded_http_get,
) -> Observation:
    """Resolve one configured source and perform a bounded observation."""
    try:
        validate_network_target(stream.url, stream.source, config.network, stream.allow_private, address_resolver,
                                deadline=deadline)
    except PolicyError as exc:
        return _unknown_observation(stream, str(exc))

    def fallback_unknown(original: Observation) -> Observation:
        if original.status != "UNKNOWN" or not stream.thumbnail_enabled or thumbnail_history is None:
            return original
        return _fallback_observation(
            original, stream, config, thumbnail_history,
            address_resolver=address_resolver, now=time.monotonic(), deadline=deadline,
            transport=transport,
        )

    media: ResolvedMedia | None = None
    if stream.source == "direct":
        timeout = _remaining(config.watch.resolver_timeout_s, deadline)
        if timeout is None:
            return _unknown_observation(stream, "CYCLE_BUDGET_EXHAUSTED")
        result = transport(
            stream.url, source="direct", network=config.network, allow_private=stream.allow_private,
            timeout_s=timeout, max_bytes=65536,
            address_resolver=address_resolver, deadline=deadline,
        )
        off_air = _off_air_from_direct(result, stream)
        if off_air:
            audio = DimensionEvidence("UNKNOWN", ("NO_MEDIA_SAMPLE_AFTER_OFF_AIR",)) if stream.expect_audio else DimensionEvidence("NOT_APPLICABLE", ("AUDIO_NOT_EXPECTED",))
            video = DimensionEvidence("UNKNOWN", ("NO_MEDIA_SAMPLE_AFTER_OFF_AIR",)) if stream.expect_video else DimensionEvidence("NOT_APPLICABLE", ("VIDEO_NOT_EXPECTED",))
            return Observation(stream.id, utc_now(), "OFF_AIR", (off_air,), audio, video)
        if result.status is None or not 200 <= result.status < 300:
            return _unknown_observation(stream, result.error_code or "DIRECT_PROBE_UNKNOWN")
        try:
            validate_network_target(result.final_url, "direct", config.network, stream.allow_private, address_resolver,
                                    deadline=deadline)
        except PolicyError:
            return _unknown_observation(stream, "DIRECT_REDIRECT_REJECTED")
        media = ResolvedMedia((MediaInput("muxed", result.final_url),))
    else:
        argv = build_ytdlp_argv(config.tools.yt_dlp, stream.url)
        timeout = _remaining(config.watch.resolver_timeout_s, deadline)
        if timeout is None:
            return _unknown_observation(stream, "CYCLE_BUDGET_EXHAUSTED")
        result = runner(
            argv, timeout,
            max_output_bytes=config.watch.max_output_bytes,
        )
        if result.timed_out or result.output_limited or not result.cleanup_complete:
            return fallback_unknown(_unknown_observation(stream, "RESOLVER_BOUNDED_FAILURE"))
        if result.launch_error or result.returncode != 0:
            return fallback_unknown(_unknown_observation(stream, "RESOLVER_FAILED"))
        media, off_air, error = _parse_resolver(result.stdout, stream, config, address_resolver, deadline=deadline)
        if off_air:
            audio = DimensionEvidence("UNKNOWN", ("NO_MEDIA_SAMPLE_AFTER_OFF_AIR",)) if stream.expect_audio else DimensionEvidence("NOT_APPLICABLE", ("AUDIO_NOT_EXPECTED",))
            video = DimensionEvidence("UNKNOWN", ("NO_MEDIA_SAMPLE_AFTER_OFF_AIR",)) if stream.expect_video else DimensionEvidence("NOT_APPLICABLE", ("VIDEO_NOT_EXPECTED",))
            return Observation(stream.id, utc_now(), "OFF_AIR", (off_air,), audio, video)
        if error or media is None:
            original = _unknown_observation(stream, error or "RESOLVER_MEDIA_MISSING")
            if error == "RESOLVER_MEDIA_URL_REJECTED":
                return original
            return fallback_unknown(original)

    observation = capture_and_inspect(media, stream, config, runner=runner, deadline=deadline)
    return fallback_unknown(observation)


def check_cycle(
    config: AppConfig,
    *,
    observer: Callable[[AppConfig, StreamConfig], Observation] | None = None,
    clock: Callable[[], float] = time.monotonic,
    thumbnail_history: dict[str, ThumbnailBaseline] | None = None,
) -> tuple[Observation, ...]:
    """Run each stream at most once, stopping new work when the cycle budget is spent."""
    started = clock()
    history = thumbnail_history if thumbnail_history is not None else {}
    results: list[Observation] = []
    for index, stream in enumerate(config.streams):
        now = clock()
        remaining = started + config.watch.cycle_budget_s - now
        if remaining <= 0:
            results.append(_unknown_observation(stream, "CYCLE_BUDGET_EXHAUSTED"))
            continue
        try:
            if observer is None:
                # Unused time is available to later streams; one slow source cannot
                # consume every subsequent source's entire observation window.
                stream_deadline = now + remaining / (len(config.streams) - index)
                result = observe_stream(config, stream, thumbnail_history=history,
                                       deadline=stream_deadline)
            else:
                result = observer(config, stream)
        except Exception:
            result = _unknown_observation(stream, "OBSERVATION_INTERNAL_ERROR")
        results.append(result)
    return tuple(results)


def _validate_watch_mode(max_cycles: int | None, continuous: bool) -> None:
    if continuous and max_cycles is not None:
        raise ValueError("max_cycles cannot be set with continuous mode")
    if not continuous and (
        isinstance(max_cycles, bool) or not isinstance(max_cycles, int)
        or not 1 <= max_cycles <= MAX_CYCLES
    ):
        raise ValueError(f"max_cycles must be between 1 and {MAX_CYCLES}")


def watch_cycles(
    config: AppConfig,
    max_cycles: int | None = None,
    *,
    continuous: bool = False,
    cycle: Callable[[AppConfig], tuple[Observation, ...]] = check_cycle,
    sleeper: Callable[[float], None] = time.sleep,
    store: "IncidentStore | None" = None,
    clock: Callable[[], float] | None = None,
) -> Iterator[tuple[Observation, ...]]:
    _validate_watch_mode(max_cycles, continuous)
    remaining = None if continuous else max_cycles
    thumbnail_history: dict[str, ThumbnailBaseline] = {}
    clock = clock or time.monotonic
    while remaining is None or remaining > 0:
        started = clock()
        if cycle is check_cycle:
            batch = check_cycle(config, thumbnail_history=thumbnail_history)
        else:
            batch = cycle(config)
        if store is not None:
            store.record_cycle(config, batch, time.time(), time.monotonic())
            dispatch_due(store, config.destinations, config.network)
        yield batch
        if remaining is not None:
            remaining -= 1
            if remaining == 0:
                break
        sleeper(max(0.0, config.watch.interval_s - (clock() - started)))


def _bounded_number(value: Any, *, allow_none: bool = False) -> float | None:
    if value is None and allow_none:
        return None
    if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value):
        return None
    return round(min(86400.0, max(0.0, float(value))), 3)


def _bounded_coverage(value: CoverageEvidence | None) -> dict[str, Any] | None:
    if value is None:
        return None
    return {
        "covered_s": _bounded_number(value.covered_s) or 0.0,
        "decoded_amount_s": _bounded_number(value.decoded_amount_s) or 0.0,
        "frame_count": min(100000, max(0, int(value.frame_count))),
        "sample_count": min(100000000000, max(0, int(value.sample_count))),
        "sample_rate_hz": value.sample_rate_hz,
        "first_s": _bounded_number(value.first_s, allow_none=True),
        "last_s": _bounded_number(value.last_s, allow_none=True),
        "largest_gap_s": _bounded_number(value.largest_gap_s) or 0.0,
        "complete": bool(value.complete),
        "window_offset_s": _bounded_number(value.window_offset_s) or 0.0,
    }


def _bounded_dimension_evidence(value: DimensionEvidence) -> dict[str, Any]:
    status = value.status if value.status in {"HEALTHY", "UNHEALTHY", "UNKNOWN", "DEGRADED", "NOT_APPLICABLE"} else "UNKNOWN"
    reasons = [reason for reason in value.reason_codes[:16]
               if isinstance(reason, str) and re.fullmatch(r"[A-Z0-9_]{1,64}", reason)]
    intervals = []
    for item in value.intervals[:32]:
        start, end = _bounded_number(item.start_s), _bounded_number(item.end_s)
        if start is None or end is None or end < start:
            continue
        intervals.append({"start_s": start, "end_s": end,
                          "open_at_sample_end": bool(item.open_at_sample_end)})
    return {
        "status": status,
        "reason_codes": reasons,
        "intervals": intervals,
        "sampled_s": _bounded_number(value.sampled_s) or 0.0,
        "decoded_s": _bounded_number(value.decoded_s, allow_none=True),
        "coverage": _bounded_coverage(value.coverage),
    }


def _transition_sample(condition: str, observation: Observation) -> dict[str, Any]:
    if condition.startswith("audio_"):
        dimensions = {"audio": _bounded_dimension_evidence(observation.audio)}
    elif condition.startswith("video_"):
        dimensions = {"video": _bounded_dimension_evidence(observation.video)}
    else:
        dimensions = {
            "audio": _bounded_dimension_evidence(observation.audio),
            "video": _bounded_dimension_evidence(observation.video),
        }
    observed_at = observation.observed_at[:64] if isinstance(observation.observed_at, str) else ""
    status = observation.status if observation.status in {"HEALTHY", "UNHEALTHY", "UNKNOWN", "DEGRADED", "OFF_AIR"} else "UNKNOWN"
    return {"observed_at": observed_at, "status": status, "dimensions": dimensions}


def _transition_evidence(condition: str, observation: Observation, *,
                         threshold: Mapping[str, Any],
                         samples: Sequence[Mapping[str, Any]] = ()) -> dict[str, Any]:
    return {
        "affected_dimensions": _transition_sample(condition, observation)["dimensions"],
        "threshold": dict(threshold),
        "samples": [dict(sample) for sample in samples[-100:]],
    }


from .persistence import IncidentStore


def stream_fingerprint(stream: StreamConfig) -> str:
    """Invalidate stored health when a configured source or detector changes."""
    return hashlib.sha256(json.dumps(asdict(stream), sort_keys=True).encode()).hexdigest()


def _incident_signals(observation: Observation) -> dict[str, str]:
    status = observation.status
    known = status in {"HEALTHY", "UNHEALTHY", "OFF_AIR"}
    unknown = status == "UNKNOWN" or (status not in {"OFF_AIR", "DEGRADED"} and
                                      (observation.audio.status == "UNKNOWN" or observation.video.status == "UNKNOWN"))
    def has_fresh_valid_media(value: DimensionEvidence) -> bool:
        invalid_reason = any(
            reason.endswith(("_DETECTOR_DISABLED", "_MISSING"))
            or "STALE" in reason or "THUMBNAIL" in reason
            for reason in value.reason_codes
        )
        sampled = _bounded_number(value.sampled_s) or 0.0
        decoded = _bounded_number(value.decoded_s, allow_none=True)
        return (value.status in {"HEALTHY", "UNHEALTHY"} and not invalid_reason
                and sampled > 0 and decoded is not None and decoded > 0)

    def is_healthy_for_this_condition(value: DimensionEvidence) -> bool:
        return value.status == "HEALTHY" and has_fresh_valid_media(value)

    audio_good = is_healthy_for_this_condition(observation.audio)
    video_good = is_healthy_for_this_condition(observation.video)
    media_good = has_fresh_valid_media(observation.audio) or has_fresh_valid_media(observation.video)
    return {
        "off_air": "good" if media_good else ("bad" if status == "OFF_AIR" else "unknown"),
        "video_freeze": "bad" if "VIDEO_FREEZE" in observation.video.reason_codes else
                        ("good" if video_good else "unknown"),
        "video_missing": "bad" if "VIDEO_MISSING" in observation.video.reason_codes else
                         ("good" if has_fresh_valid_media(observation.video) else "unknown"),
        "audio_silence": "bad" if "AUDIO_SILENCE" in observation.audio.reason_codes else
                         ("good" if audio_good else "unknown"),
        "audio_missing": "bad" if "AUDIO_MISSING" in observation.audio.reason_codes else
                         ("good" if has_fresh_valid_media(observation.audio) else "unknown"),
        "unknown": "bad" if unknown else ("good" if known else "unknown"),
        "degraded": "bad" if status == "DEGRADED" else
                    ("good" if status in {"HEALTHY", "UNHEALTHY", "OFF_AIR"} else "unknown"),
    }


def _notification_summary(event: Mapping[str, Any]) -> str:
    return f"Dead Air {event['transition']}: {event['stream_id']} {event['condition']} ({event['event_id']})"


def _retry_after_seconds(value: str | None, now: float | None = None) -> float | None:
    if not value:
        return None
    try:
        delay = float(value.strip())
    except ValueError:
        try:
            parsed = email.utils.parsedate_to_datetime(value)
            delay = parsed.timestamp() - (time.time() if now is None else now)
        except (AttributeError, TypeError, ValueError, OverflowError):
            return None
    if not math.isfinite(delay):
        return None
    return min(120.0, max(0.0, delay))


def bounded_http_post(url: str, body: bytes, headers: Mapping[str, str], timeout_s: float,
                      network: NetworkConfig, address_resolver: Callable[[str, int], Sequence[str]] = _resolve_addresses,
                      *, allow_private: bool = False) -> tuple[int | None, str | None, float | None]:
    try:
        _static_url(url, "direct")
        if urllib.parse.urlsplit(url).scheme != "https":
            return None, "TLS_REQUIRED", None
        addresses = validate_network_target(url, "direct", network, allow_private, address_resolver,
                                            deadline=time.monotonic() + timeout_s)
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), _NoRedirect(),
                                            *_pinned_handlers(addresses or ()))
        request = urllib.request.Request(url, data=body, headers=dict(headers), method="POST")
        try:
            response = opener.open(request, timeout=timeout_s)
        except urllib.error.HTTPError as exc:
            retry_after = _retry_after_seconds(exc.headers.get("Retry-After") if exc.headers else None)
            try:
                exc.close()
            except OSError:
                pass
            return int(exc.code), "HTTP_STATUS", retry_after
        try:
            status = int(getattr(response, "status", getattr(response, "code", 0)) or 0)
            retry_after = _retry_after_seconds(response.headers.get("Retry-After") if response.headers else None)
            return status, None if 200 <= status < 300 else "HTTP_STATUS", retry_after
        finally:
            response.close()
    except PolicyError as exc:
        return None, str(exc), None
    except (OSError, urllib.error.URLError, ValueError):
        return None, "HTTP_REQUEST_FAILED", None


class _AttemptDeadlineExceeded(Exception):
    pass


def send_destination(destination: DestinationConfig, event: Mapping[str, Any], network: NetworkConfig,
                     *, environment: Mapping[str, str] | None = None,
                     http_transport: Callable[..., tuple[int | None, str | None]] | None = None,
                     address_resolver: Callable[[str, int], Sequence[str]] = _resolve_addresses,
                     smtp_ssl_factory: Callable[..., Any] | None = None,
                     smtp_factory: Callable[..., Any] | None = None,
                     deadline: float | None = None,
                     monotonic: Callable[[], float] = time.monotonic) -> tuple[bool, str | None, int | None, float | None]:
    env = os.environ if environment is None else environment
    summary = _notification_summary(event)

    def remaining_timeout() -> float:
        if deadline is None:
            return 5.0
        remaining = deadline - monotonic()
        if remaining <= 0:
            raise _AttemptDeadlineExceeded
        return min(5.0, remaining)

    if destination.kind == "smtp":
        host = destination.host or ""
        port = destination.port or 0
        url_host = f"[{host}]" if ":" in host and not host.startswith("[") else host
        client = None
        try:
            validate_network_target(f"https://{url_host}:{port}/", "direct", network,
                                    destination.allow_private, address_resolver)
            remaining_timeout()
            message = email.message.EmailMessage()
            message["From"] = _email_address(destination.sender, "sender")
            message["To"] = ", ".join(_email_address(x, "recipient") for x in destination.recipients)
            message["Subject"] = summary
            message.set_content(summary + "\n" + json.dumps(dict(event), sort_keys=True))
            context = ssl.create_default_context()
            if destination.tls == "ssl":
                constructor = smtp_ssl_factory or smtplib.SMTP_SSL
                client = constructor(host, port, timeout=remaining_timeout(), context=context)
            else:
                constructor = smtp_factory or smtplib.SMTP
                client = constructor(host, port, timeout=remaining_timeout())
                remaining_timeout()
                client.ehlo()
                remaining_timeout()
                client.starttls(context=context)
                remaining_timeout()
                client.ehlo()
            remaining_timeout()
            if destination.username_env:
                username = env.get(destination.username_env, "")
                password = env.get(destination.password_env or "", "")
                if (not username or not password or len(username) > 4096 or len(password) > 4096
                        or _LABEL_CONTROL.search(username + password)):
                    return False, "SMTP_CREDENTIALS_MISSING", None, None
                remaining_timeout()
                client.login(username, password)
                remaining_timeout()
            refused = client.send_message(message)
            remaining_timeout()
            if refused:
                return False, "SMTP_PARTIAL_FAILURE", None, None
            return True, None, None, None
        except _AttemptDeadlineExceeded:
            return False, "DELIVERY_TIMEOUT", None, None
        except (OSError, smtplib.SMTPException, PolicyError, ValueError, TypeError):
            return False, "SMTP_DELIVERY_FAILED", None, None
        finally:
            if client is not None:
                if deadline is not None and monotonic() >= deadline:
                    try:
                        client.close()
                    except Exception:
                        pass
                else:
                    try:
                        client.quit()
                    except Exception:
                        try:
                            client.close()
                        except Exception:
                            pass

    endpoint = env.get(destination.endpoint_env or "", "")
    if not endpoint or len(endpoint) > 4096 or _LABEL_CONTROL.search(endpoint):
        return False, "ENDPOINT_UNAVAILABLE", None, None
    headers = {"User-Agent": f"Dead-Air/{VERSION}"}
    if destination.kind == "ntfy":
        body = summary.encode("utf-8")
        headers["Content-Type"] = "text/plain; charset=utf-8"
        headers["Title"] = "Dead Air incident"
    elif destination.kind == "discord":
        body = json.dumps({"content": summary, "allowed_mentions": {"parse": []}}, separators=(",", ":")).encode()
        headers["Content-Type"] = "application/json"
    else:
        body = json.dumps(dict(event), sort_keys=True, separators=(",", ":")).encode()
        headers["Content-Type"] = "application/json"
    if destination.token_env:
        token = env.get(destination.token_env, "")
        if not token or len(token) > 4096 or _LABEL_CONTROL.search(token):
            return False, "TOKEN_UNAVAILABLE", None, None
        headers["Authorization"] = "Bearer " + token
    transport = http_transport or (lambda url, data, hdrs, timeout: bounded_http_post(
        url, data, hdrs, timeout, network, address_resolver, allow_private=destination.allow_private))
    try:
        timeout = remaining_timeout()
        response = transport(endpoint, body, headers, timeout)
        remaining_timeout()
        status, error = response[:2]
        retry_after = response[2] if len(response) > 2 else None
        return ((True, None, status, None) if status is not None and 200 <= status < 300 and error is None
                else (False, error or "HTTP_STATUS", status, retry_after))
    except _AttemptDeadlineExceeded:
        return False, "DELIVERY_TIMEOUT", None, None
    except Exception:
        return False, "HTTP_REQUEST_FAILED", None, None


def _notification_worker() -> int:
    """Private child entrypoint; input contains no credential values or endpoint secrets."""
    try:
        raw = sys.stdin.buffer.read(65537)
        if len(raw) > 65536:
            raise ValueError
        data = json.loads(raw)
        destination_data = data["destination"]
        destination_data["recipients"] = tuple(destination_data.get("recipients", ()))
        destination = DestinationConfig(**destination_data)
        network_data = data["network"]
        network = NetworkConfig(tuple(network_data["private_allowlist"]),
                                int(network_data["max_redirects"]),
                                int(network_data["thumbnail_max_bytes"]))
        timeout_s = float(data["timeout_s"])
        if not math.isfinite(timeout_s) or timeout_s <= 0:
            raise ValueError
        result = send_destination(
            destination, data["event"], network, deadline=time.monotonic() + timeout_s,
        )
        print(json.dumps({"result": list(result)}, separators=(",", ":")))
        return 0
    except Exception:
        print('{"result":[false,"DELIVERY_INTERNAL_ERROR",null,null]}')
        return 0


def _stop_notification_child(child: subprocess.Popen[bytes], deadline: float,
                             monotonic: Callable[[], float]) -> bool:
    """Stop and reap the one owned sender child within the reserved cleanup window."""
    try:
        if child.stdin is not None:
            child.stdin.close()
    except OSError:
        pass
    if child.poll() is None:
        try:
            child.terminate()
        except OSError:
            pass
        remaining = deadline - monotonic()
        if remaining > 0:
            try:
                child.wait(timeout=min(0.2, remaining))
            except subprocess.TimeoutExpired:
                pass
        if child.poll() is None:
            try:
                child.kill()
            except OSError:
                pass
        remaining = deadline - monotonic()
        if remaining > 0 and child.poll() is None:
            try:
                child.wait(timeout=remaining)
            except subprocess.TimeoutExpired:
                pass
    stopped = child.poll() is not None
    for pipe in (child.stdout, child.stderr):
        if pipe is not None:
            try:
                pipe.close()
            except OSError:
                pass
    return stopped


def _send_destination_bounded(destination: DestinationConfig, event: Mapping[str, Any],
                              network: NetworkConfig, *, timeout_s: float,
                              environment: Mapping[str, str] | None = None,
                              monotonic: Callable[[], float] = time.monotonic,
                              popen_factory: Callable[..., subprocess.Popen[bytes]] = subprocess.Popen
                              ) -> tuple[bool, str | None, int | None, float | None]:
    if not math.isfinite(timeout_s) or timeout_s <= NOTIFICATION_CLEANUP_RESERVE_S:
        return False, "DELIVERY_TIMEOUT", None, None
    started = monotonic()
    deadline = started + timeout_s
    work_deadline = deadline - NOTIFICATION_CLEANUP_RESERVE_S
    env_source = os.environ if environment is None else environment
    child_env = child_environment()
    for name in (destination.endpoint_env, destination.token_env,
                 destination.username_env, destination.password_env):
        if name and name in env_source:
            child_env[name] = env_source[name]
    data = {
        "destination": {
            "id": destination.id, "kind": destination.kind,
            "endpoint_env": destination.endpoint_env, "token_env": destination.token_env,
            "host": destination.host, "port": destination.port, "tls": destination.tls,
            "sender": destination.sender, "recipients": list(destination.recipients),
            "username_env": destination.username_env, "password_env": destination.password_env,
            "allow_private": destination.allow_private,
        },
        "event": dict(event),
        "network": {"private_allowlist": list(network.private_allowlist),
                    "max_redirects": network.max_redirects,
                    "thumbnail_max_bytes": network.thumbnail_max_bytes},
        "timeout_s": max(0.01, work_deadline - monotonic()),
    }
    payload = json.dumps(data, sort_keys=True, separators=(",", ":")).encode("utf-8")
    if len(payload) > 65536:
        return False, "DELIVERY_PAYLOAD_LIMIT", None, None
    child: subprocess.Popen[bytes] | None = None
    result: tuple[bool, str | None, int | None, float | None] = (
        False, "DELIVERY_WORKER_FAILED", None, None,
    )
    cleanup_confirmed = True
    try:
        child = popen_factory(
            [sys.executable, "-c", "from dead_air import _notification_worker; raise SystemExit(_notification_worker())"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
            cwd=str(Path(__file__).resolve().parent.parent), env=child_env, shell=False, close_fds=True,
            creationflags=(getattr(subprocess, "CREATE_NO_WINDOW", 0) if os.name == "nt" else 0),
        )
        remaining = work_deadline - monotonic()
        if remaining <= 0:
            result = (False, "DELIVERY_TIMEOUT", None, None)
        else:
            try:
                output, _ = child.communicate(input=payload, timeout=remaining)
            except subprocess.TimeoutExpired:
                result = (False, "DELIVERY_TIMEOUT", None, None)
            else:
                if monotonic() > work_deadline:
                    result = (False, "DELIVERY_TIMEOUT", None, None)
                elif child.returncode != 0 or len(output) > 4096:
                    result = (False, "DELIVERY_WORKER_FAILED", None, None)
                else:
                    decoded = json.loads(output.decode("utf-8"))
                    worker_result = decoded.get("result")
                    if isinstance(worker_result, list) and len(worker_result) == 4:
                        result = (bool(worker_result[0]), worker_result[1],
                                  worker_result[2], worker_result[3])
    except (OSError, ValueError, TypeError):
        result = (False, "DELIVERY_WORKER_FAILED", None, None)
    finally:
        if child is not None:
            cleanup_confirmed = _stop_notification_child(child, deadline, monotonic)
    if not cleanup_confirmed:
        return False, "DELIVERY_CLEANUP_FAILED", None, None
    return result


def dispatch_due(store: IncidentStore, destinations: Sequence[DestinationConfig],
                 network: NetworkConfig, *, now_wall: float | None = None,
                 sender: Callable[..., tuple[Any, ...]] | None = None,
                 monotonic: Callable[[], float] = time.monotonic,
                 dispatch_budget_s: float = NOTIFICATION_DISPATCH_BUDGET_S,
                 attempt_budget_s: float = NOTIFICATION_ATTEMPT_BUDGET_S,
                 environment: Mapping[str, str] | None = None) -> None:
    """Dispatch active destinations only; sender is retained as a synchronous test seam."""
    when = time.time() if now_wall is None else now_wall
    by_id = {item.id: item for item in destinations}
    if not by_id or not math.isfinite(dispatch_budget_s) or dispatch_budget_s <= 0:
        return
    deadline = monotonic() + dispatch_budget_s
    for event_id, destination_id, attempts, created, payload_json in store.due_deliveries(
            when, limit=8, destination_ids=tuple(by_id)):
        remaining = deadline - monotonic()
        if remaining <= NOTIFICATION_CLEANUP_RESERVE_S:
            break
        destination = by_id.get(destination_id)
        if destination is None:
            continue
        try:
            event = json.loads(payload_json)
            if sender is None:
                response = _send_destination_bounded(
                    destination, event, network, timeout_s=min(attempt_budget_s, remaining),
                    environment=environment, monotonic=monotonic,
                )
            else:
                response = sender(destination, event, network)
            success, reason, status = response[:3]
            retry_after = response[3] if len(response) > 3 else None
        except Exception:
            success, reason, status, retry_after = False, "DELIVERY_INTERNAL_ERROR", None, None
        store.record_delivery(event_id, destination_id, success, reason, status,
                             when, created, attempts, retry_after)
        if reason == "DELIVERY_CLEANUP_FAILED":
            raise PersistenceError("DELIVERY_CLEANUP_FAILED")
        if monotonic() >= deadline:
            break


def check_cycle_stateful(config: AppConfig, store: IncidentStore,
                         *, observer: Callable[[AppConfig, StreamConfig], Observation] | None = None) -> tuple[Observation, ...]:
    observations = check_cycle(config, observer=observer)
    store.record_cycle(config, observations, time.time(), time.monotonic())
    dispatch_due(store, config.destinations, config.network)
    return observations


def _interval_json(items: tuple[Interval, ...]) -> list[dict[str, Any]]:
    return [{"start_s": item.start_s, "end_s": item.end_s,
             "open_at_sample_end": item.open_at_sample_end} for item in items]


def observation_json(item: Observation) -> dict[str, Any]:
    def dimension(value: DimensionEvidence) -> dict[str, Any]:
        return {"status": value.status, "reason_codes": list(value.reason_codes),
                "intervals": _interval_json(value.intervals), "sampled_s": value.sampled_s,
                "decoded_s": value.decoded_s, "coverage": _bounded_coverage(value.coverage)}
    return {"stream_id": item.stream_id, "observed_at": item.observed_at,
            "status": item.status, "reason_codes": list(item.reason_codes),
            "audio": dimension(item.audio), "video": dimension(item.video)}


def _exit_code(observations: Sequence[Observation]) -> int:
    if any(item.status == "OFF_AIR" or item.status == "UNHEALTHY" for item in observations):
        return 1
    if any(item.status in {"UNKNOWN", "DEGRADED"} for item in observations):
        return 2
    return 0


def _print_observations(observations: Sequence[Observation], diagnostics: Mapping[str, Any], *, json_mode: bool) -> None:
    if json_mode:
        print(json.dumps({"observations": [observation_json(item) for item in observations],
                          **diagnostics}, sort_keys=True), flush=True)
        return
    lines = []
    for item in observations:
        reasons = ", ".join(item.reason_codes)
        lines.append(f"{item.observed_at}  {item.stream_id}: {item.status} | audio={item.audio.status} video={item.video.status} | {reasons}")
    counts = diagnostics["deliveries"]
    lines.append(f"Incidents: {len(diagnostics['open_incidents'])} open | Alerts: {counts['pending']} pending, {counts['failed']} failed, {counts['sent']} sent")
    print("\n".join(lines), flush=True)


def doctor(config: AppConfig, *, runner: Callable[..., ChildResult] = run_bounded) -> dict[str, Any]:
    tools: dict[str, Any] = {}
    resolver_flags_ok = False
    resolver_required = any(stream.source == "youtube" for stream in config.streams)
    for name, executable, args in (
        ("yt_dlp", config.tools.yt_dlp, ("--version",)),
        ("ffmpeg", config.tools.ffmpeg, ("-version",)),
        ("ffprobe", config.tools.ffprobe, ("-version",)),
    ):
        path = shutil.which(executable)
        required = name != "yt_dlp" or resolver_required
        if path is None:
            tools[name] = {"available": False, "required": required}
            continue
        result = runner(build_argv(path, *args), 5, max_output_bytes=32768)
        first_line = result.stdout.splitlines()[0][:160] if result.stdout else ""
        tools[name] = {"available": result.returncode == 0 and not result.timed_out and not result.output_limited and result.cleanup_complete,
                       "version": first_line or None, "required": required}
        if name == "yt_dlp" and tools[name]["available"]:
            help_result = runner(build_argv(path, "--help"), 5, max_output_bytes=262144)
            help_text = help_result.stdout + help_result.stderr
            expected_flags = [flag for flag in YTDLP_FLAGS if flag.startswith("--")]
            resolver_flags_ok = (
                help_result.returncode == 0 and not help_result.timed_out
                and not help_result.output_limited and help_result.cleanup_complete
                and all(flag in help_text for flag in expected_flags)
            )
    ffmpeg_path = shutil.which(config.tools.ffmpeg)
    filters_ok = False
    if ffmpeg_path:
        filters = runner(build_argv(ffmpeg_path, "-hide_banner", "-filters"), 5, max_output_bytes=65536)
        text = filters.stdout + filters.stderr
        filters_ok = (filters.returncode == 0 and not filters.timed_out
                      and not filters.output_limited and filters.cleanup_complete
                      and "freezedetect" in text and "silencedetect" in text)
    tools["detector_filters_available"] = filters_ok
    tools["resolver_isolation_flags_available"] = resolver_flags_ok
    warnings = []
    if config.watch.sample_s + CAPTURE_STARTUP_S > config.watch.capture_timeout_s:
        warnings.append("CAPTURE_TIMEOUT_SHORTER_THAN_SAMPLE")
    if any(stream.max_gap_s < config.watch.interval_s for stream in config.streams):
        warnings.append("MAX_GAP_SHORTER_THAN_INTERVAL")
    if not config.destinations:
        warnings.append("NO_NOTIFICATION_DESTINATIONS")
    if len(config.streams) > 1 and config.watch.cycle_budget_s / len(config.streams) < config.watch.sample_s + CAPTURE_STARTUP_S:
        warnings.append("CYCLE_BUDGET_TOO_SMALL_FOR_ALL_SAMPLES")
    return {"ok": all(item["available"] for item in tools.values()
                       if isinstance(item, dict) and item["required"])
            and filters_ok and (resolver_flags_ok or not resolver_required), "warnings": warnings,
            "tools": tools, "network_contacted": False}


def _main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Dead Air stream observations")
    parser.add_argument("--version", action="version", version=f"dead-air {VERSION}")
    commands = parser.add_subparsers(dest="command", required=True)
    init_parser = commands.add_parser("init", help="create a private starter configuration without overwriting files")
    init_parser.add_argument("--config", default=DEFAULT_CONFIG)
    validate_parser = commands.add_parser("validate", help="validate TOML without network or state writes")
    validate_parser.add_argument("--config", default="dead-air.example.toml")
    doctor_parser = commands.add_parser("doctor", help="check local binary versions and detector filters")
    doctor_parser.add_argument("--config", default=DEFAULT_CONFIG)
    status_parser = commands.add_parser("status", help="read persisted health without network access or state writes")
    status_parser.add_argument("--config", default=DEFAULT_CONFIG)
    status_parser.add_argument("--json", action="store_true")
    check_parser = commands.add_parser("check", help="perform one bounded observation cycle")
    check_parser.add_argument("--config", default=DEFAULT_CONFIG)
    check_parser.add_argument("--json", action="store_true")
    watch_parser = commands.add_parser("watch", help="run serial cycles")
    watch_parser.add_argument("--config", default=DEFAULT_CONFIG)
    watch_mode = watch_parser.add_mutually_exclusive_group(required=True)
    watch_mode.add_argument("--max-cycles", type=int)
    watch_mode.add_argument("--continuous", action="store_true",
                            help="run in the foreground until interrupted or a fatal error")
    watch_parser.add_argument("--json", action="store_true")
    gui_parser = commands.add_parser("gui", help="serve the local page on 127.0.0.1")
    gui_parser.add_argument("--config", default=DEFAULT_CONFIG)
    gui_parser.add_argument("--port", type=int, default=47824)
    gui_parser.add_argument("--no-browser", action="store_true")
    gui_parser.add_argument("--watch", action="store_true", help="also start watching")
    args = parser.parse_args(argv)
    if args.command == "gui":
        from dead_air_gui import serve
        return serve(args.config, args.port, not args.no_browser, args.watch)
    if args.command == "init":
        from importlib.resources import files
        try:
            template = files("deadair").joinpath("example.toml").read_text(encoding="utf-8")
            descriptor = os.open(Path(args.config).expanduser(), os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            with os.fdopen(descriptor, "w", encoding="utf-8", newline="\n") as output:
                output.write(template)
            print(json.dumps({"ok": True, "created": True, "next": "Edit the source URL, then run validate and doctor."}), flush=True)
            return 0
        except OSError as exc:
            print(json.dumps({"ok": False, "error": "CONFIG_EXISTS" if isinstance(exc, FileExistsError) else "CONFIG_CREATE_FAILED"}), flush=True)
            return 3
    if args.command == "watch":
        try:
            _validate_watch_mode(args.max_cycles, args.continuous)
        except ValueError as exc:
            print(json.dumps({"ok": False, "error": str(exc)}, sort_keys=True), flush=True)
            return 3
    try:
        config = load_config(args.config)
    except ConfigError as exc:
        print(json.dumps({"ok": False, "error": str(exc)}, sort_keys=True))
        return 3
    if args.command == "validate":
        print(json.dumps({"ok": True, "stream_count": len(config.streams)}, sort_keys=True))
        return 0
    if args.command == "doctor":
        result = doctor(config)
        print(json.dumps(result, sort_keys=True))
        return 0 if result["ok"] else 3
    if args.command == "status":
        from .status import format_status, read_status, status_exit_code
        try:
            result = read_status(config)
        except PersistenceError as exc:
            print(json.dumps({"ok": False, "error": str(exc)}, sort_keys=True), flush=True)
            return 3
        print(json.dumps(result, sort_keys=True) if args.json else format_status(result), flush=True)
        return status_exit_code(result)
    if args.command == "check":
        try:
            with IncidentStore(config.watch.state_path, config.retention_days) as store:
                observations = check_cycle_stateful(config, store)
                diagnostics = store.diagnostics()
        except KeyboardInterrupt:
            return 130
        except PersistenceError as exc:
            print(json.dumps({"ok": False, "error": str(exc)}, sort_keys=True), flush=True)
            return 3
        _print_observations(observations, diagnostics, json_mode=args.json)
        return _exit_code(observations)
    aggregate = 0
    try:
        with IncidentStore(config.watch.state_path, config.retention_days) as store:
            for batch in watch_cycles(
                config, args.max_cycles, continuous=args.continuous, store=store,
            ):
                _print_observations(batch, store.diagnostics(), json_mode=args.json)
                code = _exit_code(batch)
                if code == 1:
                    aggregate = 1
                elif code == 2 and aggregate == 0:
                    aggregate = 2
    except KeyboardInterrupt:
        return 130
    except (ValueError, PersistenceError) as exc:
        print(json.dumps({"ok": False, "error": str(exc)}, sort_keys=True), flush=True)
        return 3
    return aggregate


def main(argv: Sequence[str] | None = None) -> int:
    """Unwind owned resources on service termination as well as Ctrl+C."""
    received_signal = 0
    previous_handler = None
    can_handle = threading.current_thread() is threading.main_thread()

    def stop(signum: int, _frame: Any) -> None:
        nonlocal received_signal
        received_signal = signum
        raise KeyboardInterrupt

    if can_handle:
        previous_handler = signal.signal(signal.SIGTERM, stop)
    try:
        result = _main(argv)
        return 128 + received_signal if received_signal else result
    except KeyboardInterrupt:
        return 128 + received_signal if received_signal else 130
    except BrokenPipeError:
        # A downstream consumer closed the pipe. Context managers and owned child
        # cleanup have already unwound; leave without a public traceback.
        return 141
    finally:
        if can_handle:
            signal.signal(signal.SIGTERM, previous_handler)


if __name__ == "__main__":
    sys.exit(main())
