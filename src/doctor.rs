//! Local readiness checks: tool versions, detector filters and timing warnings, plus an
//! optional loopback probe of Perennos Tempo. Sources and alert receivers are never contacted.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::constants::{CAPTURE_STARTUP_S, YTDLP_FLAGS};
use crate::model::AppConfig;
use crate::process::{RunOptions, Runner, build_argv};

/// `shutil.which`: an explicit path must be an executable file; a bare name searches PATH.
pub fn which(name: &str) -> Option<PathBuf> {
    let is_exec = |path: &Path| -> bool {
        let Ok(meta) = std::fs::metadata(path) else {
            return false;
        };
        if !meta.is_file() {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            meta.permissions().mode() & 0o111 != 0
        }
        #[cfg(not(unix))]
        {
            true
        }
    };
    let extensions: Vec<String> = if cfg!(windows) {
        let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
        let mut list = vec![String::new()];
        list.extend(pathext.split(';').filter(|e| !e.is_empty()).map(|e| e.to_ascii_lowercase()));
        list
    } else {
        vec![String::new()]
    };
    let candidates = |base: &Path| -> Option<PathBuf> {
        extensions.iter().map(|ext| PathBuf::from(format!("{}{ext}", base.display()))).find(|p| is_exec(p))
    };
    if name.contains('/') || name.contains('\\') {
        return candidates(Path::new(name));
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).filter(|dir| !dir.as_os_str().is_empty()).find_map(|dir| candidates(&dir.join(name)))
}

pub type Which<'a> = &'a dyn Fn(&str) -> Option<PathBuf>;

pub fn doctor(config: &AppConfig, runner: Runner<'_>, which: Which<'_>) -> Value {
    let mut tools = Map::new();
    let mut advice: Vec<String> = Vec::new();
    let mut resolver_flags_ok = false;
    let resolver_required = config.streams.iter().any(|s| s.source == "youtube");
    for (name, executable, flag) in [
        ("yt_dlp", config.tools.yt_dlp.as_str(), "--version"),
        ("ffmpeg", config.tools.ffmpeg.as_str(), "-version"),
        ("ffprobe", config.tools.ffprobe.as_str(), "-version"),
    ] {
        let required = name != "yt_dlp" || resolver_required;
        let Some(path) = which(executable) else {
            tools.insert(name.into(), json!({"available": false, "required": required}));
            if required {
                advice.push(format!(
                    "{executable} was not found. Install it (for example `sudo apt install ffmpeg`, `brew install ffmpeg`, or `winget install --id Gyan.FFmpeg.Shared -e`) or set tools.{name} to its full path."
                ));
            }
            continue;
        };
        let path_text = path.to_string_lossy().into_owned();
        let result = match build_argv(&path_text, &[flag]) {
            Ok(argv) => runner(&argv, 5.0, &RunOptions::output(32_768)),
            Err(_) => continue,
        };
        let first_line: String = result.stdout.lines().next().unwrap_or("").chars().take(160).collect();
        let available =
            result.returncode == Some(0) && !result.timed_out && !result.output_limited && result.cleanup_complete;
        tools.insert(
            name.into(),
            json!({"available": available, "version": if first_line.is_empty() { Value::Null } else { json!(first_line) }, "required": required}),
        );
        if !available && required {
            advice
                .push(format!("{executable} is installed but did not run cleanly; check that it is a working build."));
        }
        if name == "yt_dlp" && available {
            if let Ok(argv) = build_argv(&path_text, &["--help"]) {
                let help = runner(&argv, 5.0, &RunOptions::output(262_144));
                let text = format!("{}{}", help.stdout, help.stderr);
                resolver_flags_ok = help.returncode == Some(0)
                    && !help.timed_out
                    && !help.output_limited
                    && help.cleanup_complete
                    && YTDLP_FLAGS.iter().filter(|f| f.starts_with("--")).all(|f| text.contains(f));
            }
            if !resolver_flags_ok && resolver_required {
                advice.push("This yt-dlp build lacks the isolation flags DEAD AIR requires; update yt-dlp.".into());
            }
        }
    }
    let mut filters_ok = false;
    if let Some(ffmpeg) = which(&config.tools.ffmpeg) {
        if let Ok(argv) = build_argv(&ffmpeg.to_string_lossy(), &["-hide_banner", "-filters"]) {
            let filters = runner(&argv, 5.0, &RunOptions::output(65_536));
            let text = format!("{}{}", filters.stdout, filters.stderr);
            filters_ok = filters.returncode == Some(0)
                && !filters.timed_out
                && !filters.output_limited
                && filters.cleanup_complete
                && text.contains("freezedetect")
                && text.contains("silencedetect");
        }
        if !filters_ok {
            advice.push(
                "FFmpeg's freezedetect and silencedetect filters did not answer; use a full FFmpeg build.".into(),
            );
        }
    }
    tools.insert("detector_filters_available".into(), json!(filters_ok));
    tools.insert("resolver_isolation_flags_available".into(), json!(resolver_flags_ok));
    let mut warnings: Vec<&str> = Vec::new();
    let watch = &config.watch;
    if watch.sample_s + CAPTURE_STARTUP_S > watch.capture_timeout_s {
        warnings.push("CAPTURE_TIMEOUT_SHORTER_THAN_SAMPLE");
        advice.push(
            "watch.capture_timeout_s is shorter than one sample plus its 100 ms prefix; every capture would time out."
                .into(),
        );
    }
    if config.streams.iter().any(|s| s.max_gap_s < watch.interval_s) {
        warnings.push("MAX_GAP_SHORTER_THAN_INTERVAL");
        advice.push(
            "A stream's max_gap_s is shorter than watch.interval_s, so pending evidence resets every cycle.".into(),
        );
    }
    if config.destinations.is_empty() {
        warnings.push("NO_NOTIFICATION_DESTINATIONS");
        advice.push("No alert destinations are configured; incidents will be recorded but nobody is told.".into());
    }
    let count = config.streams.len() as f64;
    if config.streams.len() > 1 && watch.cycle_budget_s / count < watch.sample_s + CAPTURE_STARTUP_S {
        warnings.push("CYCLE_BUDGET_TOO_SMALL_FOR_ALL_SAMPLES");
        advice.push("watch.cycle_budget_s cannot fit one sample per stream; later streams will be UNKNOWN.".into());
    }
    let required_ok = tools
        .values()
        .filter_map(Value::as_object)
        .filter(|t| t["required"].as_bool() == Some(true))
        .all(|t| t["available"].as_bool() == Some(true));
    let ok = required_ok && filters_ok && (resolver_flags_ok || !resolver_required);
    let mut result = json!({"ok": ok, "warnings": warnings, "tools": tools, "network_contacted": false});
    if config.tempo.enabled {
        let report = crate::tempo::status_report(config);
        if report["health"] != "ok" {
            if let Some(warnings) = result["warnings"].as_array_mut() {
                warnings.push(json!("TEMPO_UNREACHABLE"));
            }
            advice.push(format!(
                "Perennos Tempo did not answer at {}. Briefs are unavailable; monitoring and alerts are unaffected.",
                config.tempo.url
            ));
        }
        if report["token"] != "present" {
            if let Some(warnings) = result["warnings"].as_array_mut() {
                warnings.push(json!("TEMPO_TOKEN_MISSING"));
            }
            advice.push(
                "The Perennos Tempo token is missing: set TEMPO_GATEWAY_TOKEN or [tempo] home (or TEMPO_HOME).".into(),
            );
        }
        result["tempo"] = report;
        result["loopback_tempo_contacted"] = json!(true);
    }
    if ok && advice.is_empty() {
        advice.push(
            "Local tools look ready. This does not prove a source is reachable or that alerts reach a person.".into(),
        );
    }
    result["advice"] = json!(advice);
    result
}
