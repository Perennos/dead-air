"""Shared resource bounds and resolver policy."""
VERSION = "0.2.0"
DEFAULT_CONFIG = "dead-air.toml"
NOTIFICATION_ATTEMPT_BUDGET_S = 5.0
NOTIFICATION_CLEANUP_RESERVE_S = 1.0
NOTIFICATION_DISPATCH_BUDGET_S = 8.0
CAPTURE_STARTUP_S = 0.100
MAX_STREAMS = 64
MAX_CYCLES = 10000
YTDLP_FLAGS = (
    "--ignore-config", "--no-plugin-dirs", "--no-remote-components",
    "--no-playlist", "--skip-download", "--dump-single-json",
    "--socket-timeout", "8", "--retries", "0", "--extractor-retries", "0",
)
YOUTUBE_HOSTS = frozenset({"youtube.com", "www.youtube.com", "m.youtube.com", "youtu.be"})
OFF_AIR_LIVE_STATUS = frozenset({"not_live", "was_live", "post_live", "ended"})
UPCOMING_LIVE_STATUS = frozenset({"is_upcoming", "upcoming"})
CLOUD_METADATA_IPS = frozenset({"169.254.169.254", "100.100.100.200"})
INPUT_PROTOCOLS = "http,https,tcp,tls,crypto"


