//! Shared resource bounds and resolver policy.

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const PRODUCT: &str = "deadair";
pub const DEFAULT_CONFIG: &str = "dead-air.toml";
pub const DEFAULT_VALIDATE_CONFIG: &str = "dead-air.example.toml";
pub const DEFAULT_GUI_PORT: u16 = 47824;
pub const DEFAULT_METRICS_PORT: u16 = 47825;
pub const NOTIFICATION_ATTEMPT_BUDGET_S: f64 = 5.0;
pub const NOTIFICATION_CLEANUP_RESERVE_S: f64 = 1.0;
pub const NOTIFICATION_DISPATCH_BUDGET_S: f64 = 8.0;
pub const CAPTURE_STARTUP_S: f64 = 0.100;
pub const MAX_STREAMS: usize = 64;
pub const MAX_CYCLES: i64 = 10_000;
pub const YTDLP_FLAGS: &[&str] = &[
    "--ignore-config",
    "--no-plugin-dirs",
    "--no-remote-components",
    "--no-playlist",
    "--skip-download",
    "--dump-single-json",
    "--socket-timeout",
    "8",
    "--retries",
    "0",
    "--extractor-retries",
    "0",
];
pub const YOUTUBE_HOSTS: &[&str] = &["youtube.com", "www.youtube.com", "m.youtube.com", "youtu.be"];
pub const OFF_AIR_LIVE_STATUS: &[&str] = &["not_live", "was_live", "post_live", "ended"];
pub const UPCOMING_LIVE_STATUS: &[&str] = &["is_upcoming", "upcoming"];
/// Cloud instance metadata services; denied even when a private target is allowlisted.
pub const CLOUD_METADATA_IPS: &[&str] = &["169.254.169.254", "100.100.100.200", "fd00:ec2::254"];
pub const INPUT_PROTOCOLS: &str = "http,https,tcp,tls,crypto";
/// The persistent legacy guard written beside every state database.
pub const LEGACY_GUARD: &[u8] = b"DEAD_AIR_OS_LOCK_V2\n";
pub const SCHEMA: i64 = 2;
pub const CONDITIONS: &[&str] =
    &["off_air", "video_freeze", "video_missing", "audio_silence", "audio_missing", "unknown", "degraded"];
pub const TEMPO_TASK_CLASS: &str = "deadair-incident-brief";
