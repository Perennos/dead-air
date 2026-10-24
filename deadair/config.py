"""Strict TOML parsing and static input validation; no network or writes."""
from __future__ import annotations
import email.utils
import ipaddress
import math
import os
import re
import tomllib
import urllib.parse
from dataclasses import replace
from pathlib import Path
from typing import Any
from .models import AppConfig, ConfigError, DestinationConfig, NetworkConfig, StreamConfig, ToolsConfig, WatchConfig
from .constants import MAX_STREAMS, YOUTUBE_HOSTS

_ID = re.compile(r"[a-z0-9][a-z0-9_-]{0,63}\Z")
_LABEL_CONTROL = re.compile(r"[\x00-\x1f\x7f]")

def _finite(value: Any, name: str, low: float, high: float) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise ConfigError(f"{name} must be numeric")
    result = float(value)
    if not math.isfinite(result) or not low <= result <= high:
        raise ConfigError(f"{name} is outside its permitted range")
    return result


def _integer(value: Any, name: str, low: int, high: int) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or not low <= value <= high:
        raise ConfigError(f"{name} must be an integer in range")
    return value


def _boolean(value: Any, name: str) -> bool:
    if not isinstance(value, bool):
        raise ConfigError(f"{name} must be a boolean")
    return value


def _table(value: Any, name: str, expected: set[str]) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise ConfigError(f"{name} must be a TOML table")
    if set(value) != expected:
        missing = sorted(expected - set(value))
        unknown = sorted(set(value) - expected)
        parts = []
        if missing:
            parts.append("missing " + ", ".join(missing))
        if unknown:
            parts.append("unknown " + ", ".join(unknown))
        raise ConfigError(f"{name}: " + "; ".join(parts))
    return value


def _string(value: Any, name: str, max_length: int = 4096) -> str:
    if not isinstance(value, str) or not value.strip() or len(value) > max_length or "\0" in value:
        raise ConfigError(f"{name} must be a non-empty bounded string")
    return value


def _safe_hostport(value: Any) -> str:
    if not isinstance(value, str) or len(value) > 260 or "\0" in value:
        raise ConfigError("private_allowlist entries must be host:port strings")
    try:
        if value.startswith("["):
            close = value.index("]")
            host, port_text = value[1:close], value[close + 2:]
            if value[close + 1] != ":":
                raise ValueError
        else:
            host, port_text = value.rsplit(":", 1)
        port = int(port_text)
        if not host or not 1 <= port <= 65535:
            raise ValueError
        try:
            ipaddress.ip_address(host)
        except ValueError:
            if not re.fullmatch(r"(?i)[a-z0-9](?:[a-z0-9.-]{0,251}[a-z0-9])?", host):
                raise ValueError
    except (ValueError, IndexError):
        raise ConfigError("private_allowlist entry is malformed") from None
    normalized = host.lower()
    if ":" in normalized:
        normalized = f"[{normalized}]"
    return f"{normalized}:{port}"


def _static_url(url: str, source: str, *, thumbnail: bool = False) -> tuple[str, int]:
    if any(ord(char) < 32 or ord(char) == 127 for char in url) or "#" in url:
        raise ConfigError("URL contains a control character or fragment")
    try:
        parsed = urllib.parse.urlsplit(url)
        host = parsed.hostname
        port = parsed.port if parsed.port is not None else (443 if parsed.scheme.lower() == "https" else 80)
    except ValueError:
        raise ConfigError("URL host or port is malformed") from None
    if parsed.username is not None or parsed.password is not None:
        raise ConfigError("URL userinfo is prohibited")
    scheme = parsed.scheme.lower()
    if scheme not in ({"https"} if thumbnail else {"http", "https"}) or not host:
        raise ConfigError("URL must be an absolute permitted HTTP(S) URL")
    try:
        normalized_host = host.encode("idna").decode("ascii").lower().rstrip(".")
    except UnicodeError:
        raise ConfigError("URL hostname is malformed") from None
    if not normalized_host or not 1 <= port <= 65535:
        raise ConfigError("URL host or port is malformed")
    if source == "youtube":
        if scheme != "https" or normalized_host not in YOUTUBE_HOSTS:
            raise ConfigError("YouTube source must use an approved HTTPS host")
        path = parsed.path.rstrip("/") or "/"
        query = urllib.parse.parse_qs(parsed.query, keep_blank_values=True)
        valid_watch = path == "/watch" and bool(query.get("v", [""])[0])
        valid_live = path.startswith("/live/") and bool(path.removeprefix("/live/"))
        valid_short = normalized_host == "youtu.be" and bool(path.strip("/"))
        if not (valid_watch or valid_live or valid_short):
            raise ConfigError("YouTube URL must identify one watch or live item")
    return normalized_host, port


def validate_config(data: dict[str, Any]) -> AppConfig:
    if not isinstance(data, dict):
        raise ConfigError("config must be a TOML table")
    required_top = {"watch", "tools", "network", "streams"}
    allowed_top = required_top | {"storage", "destinations"}
    if required_top - set(data) or set(data) - allowed_top:
        raise ConfigError("config has missing or unknown top-level keys")
    top = data
    watch_data = _table(top["watch"], "[watch]", {
        "interval_s", "sample_s", "resolver_timeout_s", "capture_timeout_s",
        "analysis_timeout_s", "max_sample_bytes", "max_output_bytes", "cycle_budget_s", "state_path",
    })
    watch = WatchConfig(
        interval_s=_finite(watch_data["interval_s"], "watch.interval_s", 1, 86400),
        sample_s=_finite(watch_data["sample_s"], "watch.sample_s", 1, 300),
        resolver_timeout_s=_finite(watch_data["resolver_timeout_s"], "watch.resolver_timeout_s", 0.1, 120),
        capture_timeout_s=_finite(watch_data["capture_timeout_s"], "watch.capture_timeout_s", 0.1, 120),
        analysis_timeout_s=_finite(watch_data["analysis_timeout_s"], "watch.analysis_timeout_s", 0.1, 120),
        max_sample_bytes=_integer(watch_data["max_sample_bytes"], "watch.max_sample_bytes", 1024, 25_165_824),
        max_output_bytes=_integer(watch_data["max_output_bytes"], "watch.max_output_bytes", 1024, 1_048_576),
        cycle_budget_s=_finite(watch_data["cycle_budget_s"], "watch.cycle_budget_s", 1, 600),
        state_path=_string(watch_data["state_path"], "watch.state_path", 512),
    )
    if watch.cycle_budget_s < max(watch.resolver_timeout_s, watch.capture_timeout_s, watch.analysis_timeout_s):
        raise ConfigError("watch.cycle_budget_s must cover at least one child deadline")

    tools_data = _table(top["tools"], "[tools]", {"yt_dlp", "ffmpeg", "ffprobe"})
    tools_values = []
    for key in ("yt_dlp", "ffmpeg", "ffprobe"):
        value = _string(tools_data[key], f"tools.{key}", 1024)
        if any(ord(char) < 32 for char in value):
            raise ConfigError(f"tools.{key} contains a control character")
        tools_values.append(value)
    tools = ToolsConfig(*tools_values)

    network_data = _table(top["network"], "[network]", {
        "private_allowlist", "max_redirects", "thumbnail_max_bytes",
    })
    raw_allowlist = network_data["private_allowlist"]
    if not isinstance(raw_allowlist, list):
        raise ConfigError("network.private_allowlist must be an array")
    allowlist = tuple(_safe_hostport(item) for item in raw_allowlist)
    if len(set(allowlist)) != len(allowlist):
        raise ConfigError("network.private_allowlist contains duplicates")
    network = NetworkConfig(
        private_allowlist=allowlist,
        max_redirects=_integer(network_data["max_redirects"], "network.max_redirects", 0, 3),
        thumbnail_max_bytes=_integer(network_data["thumbnail_max_bytes"], "network.thumbnail_max_bytes", 1024, 2_097_152),
    )

    stream_fields = {
        "id", "label", "url", "source", "expected_live", "expect_audio", "expect_video",
        "detect_freeze", "freeze_noise", "freeze_window_s", "silence_db", "silence_window_s",
        "unhealthy_after_s", "min_bad_samples", "recovery_samples", "max_gap_s",
        "unknown_notice_after_s", "offline_http_statuses", "thumbnail_enabled", "thumbnail_url",
        "thumbnail_stale_after_s", "allow_private",
    }
    raw_streams = top["streams"]
    if not isinstance(raw_streams, list) or not 1 <= len(raw_streams) <= MAX_STREAMS:
        raise ConfigError(f"streams must contain 1 to {MAX_STREAMS} entries")
    streams: list[StreamConfig] = []
    seen_ids: set[str] = set()
    for index, raw in enumerate(raw_streams):
        item = _table(raw, f"streams[{index}]", stream_fields)
        sid = _string(item["id"], "stream.id", 64)
        if not _ID.fullmatch(sid) or sid in seen_ids:
            raise ConfigError("stream ids must be safe, unique slugs")
        seen_ids.add(sid)
        label = _string(item["label"], "stream.label", 80)
        if _LABEL_CONTROL.search(label):
            raise ConfigError("stream.label contains control characters")
        source = _string(item["source"], "stream.source", 16).lower()
        if source not in {"youtube", "direct"}:
            raise ConfigError("stream.source must be youtube or direct")
        url = _string(item["url"], "stream.url", 4096)
        _static_url(url, source)
        expected_live = _boolean(item["expected_live"], "stream.expected_live")
        expect_audio = _boolean(item["expect_audio"], "stream.expect_audio")
        expect_video = _boolean(item["expect_video"], "stream.expect_video")
        detect_freeze = _boolean(item["detect_freeze"], "stream.detect_freeze")
        if not (expect_audio or expect_video) or (detect_freeze and not expect_video):
            raise ConfigError("each stream needs an expected media dimension")
        freeze_noise = _finite(item["freeze_noise"], "stream.freeze_noise", 0.000001, 1.0)
        freeze_window = _finite(item["freeze_window_s"], "stream.freeze_window_s", 0.1, watch.sample_s)
        silence_db = _finite(item["silence_db"], "stream.silence_db", -120, 0)
        silence_window = _finite(item["silence_window_s"], "stream.silence_window_s", 0.1, watch.sample_s)
        offline_raw = item["offline_http_statuses"]
        if not isinstance(offline_raw, list):
            raise ConfigError("stream.offline_http_statuses must be an array")
        offline = tuple(_integer(v, "stream.offline_http_statuses", 400, 599) for v in offline_raw)
        if len(set(offline)) != len(offline):
            raise ConfigError("stream.offline_http_statuses contains duplicates")
        thumbnail_enabled = _boolean(item["thumbnail_enabled"], "stream.thumbnail_enabled")
        thumbnail_url = item["thumbnail_url"]
        if not isinstance(thumbnail_url, str):
            raise ConfigError("stream.thumbnail_url must be a string")
        if thumbnail_enabled:
            if source not in {"direct", "youtube"} or not expect_video:
                raise ConfigError("thumbnail fallback requires a direct or YouTube video source")
            if thumbnail_url:
                _static_url(_string(thumbnail_url, "stream.thumbnail_url", 4096), "direct", thumbnail=True)
            elif source == "direct":
                raise ConfigError("direct thumbnail fallback requires thumbnail_url")
        elif thumbnail_url:
            _static_url(_string(thumbnail_url, "stream.thumbnail_url", 4096), "direct", thumbnail=True)
        streams.append(StreamConfig(
            id=sid, label=label, url=url, source=source, expected_live=expected_live,
            expect_audio=expect_audio, expect_video=expect_video, detect_freeze=detect_freeze,
            freeze_noise=freeze_noise, freeze_window_s=freeze_window, silence_db=silence_db,
            silence_window_s=silence_window,
            unhealthy_after_s=_finite(item["unhealthy_after_s"], "stream.unhealthy_after_s", 0, 86400),
            min_bad_samples=_integer(item["min_bad_samples"], "stream.min_bad_samples", 1, 100),
            recovery_samples=_integer(item["recovery_samples"], "stream.recovery_samples", 1, 100),
            max_gap_s=_finite(item["max_gap_s"], "stream.max_gap_s", 1, 86400),
            unknown_notice_after_s=_finite(item["unknown_notice_after_s"], "stream.unknown_notice_after_s", 0, 604800),
            offline_http_statuses=offline, thumbnail_enabled=thumbnail_enabled,
            thumbnail_url=thumbnail_url,
            thumbnail_stale_after_s=_finite(item["thumbnail_stale_after_s"], "stream.thumbnail_stale_after_s", 60, 86400),
            allow_private=_boolean(item["allow_private"], "stream.allow_private"),
        ))
    storage = _table(top.get("storage", {"retention_days": 90}), "[storage]", {"retention_days"})
    retention_days = _integer(storage["retention_days"], "storage.retention_days", 1, 3650)
    destinations = _parse_destinations(top.get("destinations", []))
    return AppConfig(watch, tools, network, tuple(streams), retention_days, destinations)


def load_config(path: str | os.PathLike[str]) -> AppConfig:
    try:
        config_path = Path(path).expanduser().absolute()
        with config_path.open("rb") as handle:
            data = tomllib.load(handle)
    except (OSError, UnicodeError, tomllib.TOMLDecodeError) as exc:
        raise ConfigError(f"cannot read config ({type(exc).__name__})") from None
    config = validate_config(data)
    parent = config_path.parent
    state = Path(config.watch.state_path).expanduser()
    if not state.is_absolute():
        state = parent / state
    tool_paths = {}
    for name in ("ffmpeg", "ffprobe", "yt_dlp"):
        value = getattr(config.tools, name)
        if "/" in value or "\\" in value:
            candidate = Path(value).expanduser()
            value = str(candidate if candidate.is_absolute() else parent / candidate)
        tool_paths[name] = value
    return replace(config, watch=replace(config.watch, state_path=str(state)),
                   tools=ToolsConfig(**tool_paths))


def _env_name(value: Any, name: str) -> str:
    result = _string(value, name, 64)
    if not re.fullmatch(r"[A-Z_][A-Z0-9_]{0,63}", result):
        raise ConfigError(f"{name} must name an uppercase environment variable")
    return result


def _email_address(value: Any, name: str) -> str:
    result = _string(value, name, 254)
    if _LABEL_CONTROL.search(result) or "\r" in result or "\n" in result:
        raise ConfigError(f"{name} contains a header control character")
    display, address = email.utils.parseaddr(result)
    if display or address != result or not re.fullmatch(r"[^\s@<>]+@[^\s@<>]+\.[^\s@<>]+", address):
        raise ConfigError(f"{name} must be a single email address")
    return result


def _parse_destinations(raw: Any) -> tuple[DestinationConfig, ...]:
    if not isinstance(raw, list) or len(raw) > 8:
        raise ConfigError("destinations must be an array with at most 8 entries")
    result: list[DestinationConfig] = []
    seen: set[str] = set()
    for index, item in enumerate(raw):
        if not isinstance(item, dict):
            raise ConfigError(f"destinations[{index}] must be a table")
        kind = _string(item.get("type"), f"destinations[{index}].type", 16)
        if kind in {"ntfy", "discord", "webhook"}:
            required = {"id", "type", "endpoint_env"}
            optional = ({"token_env"} if kind in {"ntfy", "webhook"} else set()) | {"allow_private"}
            if set(item) - required - optional or required - set(item):
                raise ConfigError(f"destinations[{index}] has invalid keys")
            endpoint_env = _env_name(item["endpoint_env"], "destination.endpoint_env")
            token_env = _env_name(item["token_env"], "destination.token_env") if "token_env" in item else None
            dest = DestinationConfig(
                "", kind, endpoint_env=endpoint_env, token_env=token_env,
                allow_private=_boolean(item["allow_private"], "destination.allow_private") if "allow_private" in item else False,
            )
        elif kind == "smtp":
            required = {"id", "type", "host", "port", "tls", "sender", "recipients"}
            optional = {"username_env", "password_env", "allow_private"}
            if set(item) - required - optional or required - set(item):
                raise ConfigError(f"destinations[{index}] has invalid keys")
            if ("username_env" in item) != ("password_env" in item):
                raise ConfigError("SMTP username_env and password_env must be supplied together")
            host = _string(item["host"], "destination.host", 253)
            try:
                ipaddress.ip_address(host)
            except ValueError:
                if not re.fullmatch(r"(?i)[a-z0-9](?:[a-z0-9.-]{0,251}[a-z0-9])?", host):
                    raise ConfigError("destination.host is malformed")
            tls_mode = _string(item["tls"], "destination.tls", 16).lower()
            if tls_mode not in {"ssl", "starttls"}:
                raise ConfigError("SMTP requires tls='ssl' or tls='starttls'")
            recipients = item["recipients"]
            if not isinstance(recipients, list) or not 1 <= len(recipients) <= 20:
                raise ConfigError("destination.recipients must contain 1 to 20 addresses")
            dest = DestinationConfig(
                "", kind, host=host,
                port=_integer(item["port"], "destination.port", 1, 65535), tls=tls_mode,
                sender=_email_address(item["sender"], "destination.sender"),
                recipients=tuple(_email_address(x, "destination.recipient") for x in recipients),
                username_env=_env_name(item["username_env"], "destination.username_env") if "username_env" in item else None,
                password_env=_env_name(item["password_env"], "destination.password_env") if "password_env" in item else None,
                allow_private=_boolean(item["allow_private"], "destination.allow_private") if "allow_private" in item else False,
            )
        else:
            raise ConfigError(f"destinations[{index}].type is unsupported")
        dest_id = _string(item.get("id"), "destination.id", 64)
        if not _ID.fullmatch(dest_id) or dest_id in seen:
            raise ConfigError("destination ids must be safe, unique slugs")
        seen.add(dest_id)
        result.append(replace(dest, id=dest_id))
    return tuple(result)


