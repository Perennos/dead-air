"""Immutable configuration and observation types."""
from __future__ import annotations
from dataclasses import dataclass
from typing import Any, Mapping

class ConfigError(ValueError):
    """Invalid, incomplete, or unknown TOML configuration."""


class PolicyError(ValueError):
    """Input violates the configured URL or private-network policy."""


class PersistenceError(RuntimeError):
    """A durable state operation failed; monitoring must stop visibly."""


@dataclass(frozen=True)
class WatchConfig:
    interval_s: float
    sample_s: float
    resolver_timeout_s: float
    capture_timeout_s: float
    analysis_timeout_s: float
    max_sample_bytes: int
    max_output_bytes: int
    cycle_budget_s: float
    state_path: str


@dataclass(frozen=True)
class ToolsConfig:
    yt_dlp: str
    ffmpeg: str
    ffprobe: str


@dataclass(frozen=True)
class NetworkConfig:
    private_allowlist: tuple[str, ...]
    max_redirects: int
    thumbnail_max_bytes: int


@dataclass(frozen=True)
class StreamConfig:
    id: str
    label: str
    url: str
    source: str
    expected_live: bool
    expect_audio: bool
    expect_video: bool
    detect_freeze: bool
    freeze_noise: float
    freeze_window_s: float
    silence_db: float
    silence_window_s: float
    unhealthy_after_s: float
    min_bad_samples: int
    recovery_samples: int
    max_gap_s: float
    unknown_notice_after_s: float
    offline_http_statuses: tuple[int, ...]
    thumbnail_enabled: bool
    thumbnail_url: str
    thumbnail_stale_after_s: float
    allow_private: bool


@dataclass(frozen=True)
class AppConfig:
    watch: WatchConfig
    tools: ToolsConfig
    network: NetworkConfig
    streams: tuple[StreamConfig, ...]
    retention_days: int = 90
    destinations: tuple["DestinationConfig", ...] = ()


@dataclass(frozen=True)
class DestinationConfig:
    id: str
    kind: str
    endpoint_env: str | None = None
    token_env: str | None = None
    host: str | None = None
    port: int | None = None
    tls: str | None = None
    sender: str | None = None
    recipients: tuple[str, ...] = ()
    username_env: str | None = None
    password_env: str | None = None
    allow_private: bool = False


@dataclass(frozen=True)
class ChildResult:
    stdout: str
    stderr: str
    returncode: int | None
    timed_out: bool = False
    output_limited: bool = False
    file_limited: bool = False
    cleanup_complete: bool = True
    launch_error: str | None = None


@dataclass(frozen=True)
class Interval:
    start_s: float
    end_s: float
    open_at_sample_end: bool = False


@dataclass(frozen=True)
class DimensionEvidence:
    status: str
    reason_codes: tuple[str, ...]
    intervals: tuple[Interval, ...] = ()
    sampled_s: float = 0.0
    decoded_s: float | None = None
    coverage: CoverageEvidence | None = None


@dataclass(frozen=True)
class CoverageEvidence:
    covered_s: float
    decoded_amount_s: float
    frame_count: int
    sample_count: int
    sample_rate_hz: int | None
    first_s: float | None
    last_s: float | None
    largest_gap_s: float
    complete: bool
    window_offset_s: float = 0.0


@dataclass(frozen=True)
class Observation:
    stream_id: str
    observed_at: str
    status: str
    reason_codes: tuple[str, ...]
    audio: DimensionEvidence
    video: DimensionEvidence


@dataclass(frozen=True)
class ProbeEvidence:
    audio_streams: int
    video_streams: int
    duration_s: float | None
    start_s: float | None = None
    audio_index: int | None = None
    audio_time_base_s: float | None = None
    audio_sample_rate: int | None = None
    video_index: int | None = None
    video_time_base_s: float | None = None
    frames: tuple[Mapping[str, Any], ...] = ()


@dataclass(frozen=True)
class ProgressEvidence:
    complete: bool
    decoded_s: float | None
    frames: int


@dataclass(frozen=True)
class MediaInput:
    kind: str
    url: str


@dataclass(frozen=True)
class ResolvedMedia:
    inputs: tuple[MediaInput, ...]


@dataclass(frozen=True)
class HttpResult:
    status: int | None
    final_url: str
    content_type: str
    body: bytes
    truncated: bool = False
    error_code: str | None = None


@dataclass(frozen=True)
class ThumbnailBaseline:
    digest: str
    observed_monotonic: float


