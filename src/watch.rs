//! Serial watch cycles, stateful checks, and observation output.

use std::collections::HashMap;

use serde_json::{Value, json};

use crate::alerts::{DispatchOptions, dispatch_due};
use crate::constants::MAX_CYCLES;
use crate::interrupt;
use crate::model::{AppConfig, Observation, PersistenceError, ThumbnailBaseline, observation_json};
use crate::persistence::IncidentStore;
use crate::pyjson;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WatchMode {
    Cycles(i64),
    Continuous,
}

pub fn validate_watch_mode(max_cycles: Option<i64>, continuous: bool) -> Result<WatchMode, String> {
    if continuous {
        if max_cycles.is_some() {
            return Err("max_cycles cannot be set with continuous mode".into());
        }
        return Ok(WatchMode::Continuous);
    }
    match max_cycles {
        Some(n) if (1..=MAX_CYCLES).contains(&n) => Ok(WatchMode::Cycles(n)),
        _ => Err(format!("max_cycles must be between 1 and {MAX_CYCLES}")),
    }
}

/// The cycle seam: one serial pass over every stream.
pub type Cycle<'a> = dyn FnMut(&AppConfig, &mut HashMap<String, ThumbnailBaseline>) -> Vec<Observation> + 'a;
/// The dispatch seam: run due deliveries after a cycle is recorded.
pub type Dispatch<'a> = dyn FnMut(&mut IncidentStore, &AppConfig) -> Result<(), PersistenceError> + 'a;

pub struct WatchHooks<'a> {
    pub cycle: &'a mut Cycle<'a>,
    pub sleeper: &'a mut dyn FnMut(f64),
    pub clock: &'a dyn Fn() -> f64,
    pub wall: &'a dyn Fn() -> f64,
    pub record_mono: &'a dyn Fn() -> f64,
    pub dispatch: &'a mut Dispatch<'a>,
}

/// Production dispatch: real bounded sender children.
pub fn default_dispatch(store: &mut IncidentStore, config: &AppConfig) -> Result<(), PersistenceError> {
    dispatch_due(store, &config.destinations, &config.network, DispatchOptions::default())
}

/// What a consumer sees after each completed cycle.
pub struct CycleReport<'a> {
    pub observations: &'a [Observation],
    pub store: Option<&'a IncidentStore>,
    pub duration_s: f64,
    pub completed_wall: f64,
}

/// Run serial cycles; intervals are measured start to start and overruns never queue.
pub fn watch_cycles(
    config: &AppConfig,
    mode: WatchMode,
    mut store: Option<&mut IncidentStore>,
    hooks: WatchHooks<'_>,
    on_cycle: &mut dyn FnMut(CycleReport<'_>) -> Result<(), PersistenceError>,
) -> Result<(), PersistenceError> {
    let mut remaining = match mode {
        WatchMode::Cycles(n) => Some(n),
        WatchMode::Continuous => None,
    };
    let mut history: HashMap<String, ThumbnailBaseline> = HashMap::new();
    loop {
        interrupt::check();
        let started = (hooks.clock)();
        let batch = (hooks.cycle)(config, &mut history);
        if let Some(store) = store.as_deref_mut() {
            store.record_cycle(config, &batch, (hooks.wall)(), (hooks.record_mono)())?;
            (hooks.dispatch)(store, config)?;
        }
        let duration_s = ((hooks.clock)() - started).max(0.0);
        on_cycle(CycleReport {
            observations: &batch,
            store: store.as_deref(),
            duration_s,
            completed_wall: (hooks.wall)(),
        })?;
        if let Some(left) = remaining.as_mut() {
            *left -= 1;
            if *left == 0 {
                return Ok(());
            }
        }
        (hooks.sleeper)((config.watch.interval_s - ((hooks.clock)() - started)).max(0.0));
    }
}

/// One stateful check: observe, persist, then dispatch.
pub fn check_cycle_stateful(
    config: &AppConfig,
    store: &mut IncidentStore,
    cycle: &mut Cycle<'_>,
    dispatch: &mut Dispatch<'_>,
) -> Result<Vec<Observation>, PersistenceError> {
    let mut history = HashMap::new();
    let observations = cycle(config, &mut history);
    store.record_cycle(config, &observations, crate::timefmt::wall(), crate::timefmt::monotonic())?;
    dispatch(store, config)?;
    Ok(observations)
}

/// Fold one cycle's exit code into a finite watch's result: unhealthy outranks unknown.
pub fn aggregate(previous: i32, code: i32) -> i32 {
    match code {
        1 => 1,
        2 if previous == 0 => 2,
        _ => previous,
    }
}

pub fn exit_code(observations: &[Observation]) -> i32 {
    if observations.iter().any(|o| o.status == "OFF_AIR" || o.status == "UNHEALTHY") {
        return 1;
    }
    if observations.iter().any(|o| o.status == "UNKNOWN" || o.status == "DEGRADED") {
        return 2;
    }
    0
}

/// The text of one cycle: JSON (`observations` plus diagnostics) or a readable summary.
pub fn format_observations(observations: &[Observation], diagnostics: &Value, json_mode: bool) -> String {
    if json_mode {
        let mut object = diagnostics.as_object().cloned().unwrap_or_default();
        object.insert("observations".into(), Value::Array(observations.iter().map(observation_json).collect()));
        return pyjson::dumps(&Value::Object(object));
    }
    let mut lines: Vec<String> = observations
        .iter()
        .map(|o| {
            format!(
                "{}  {}: {} | audio={} video={} | {}",
                o.observed_at,
                o.stream_id,
                o.status,
                o.audio.status,
                o.video.status,
                o.reason_codes.join(", ")
            )
        })
        .collect();
    let counts = &diagnostics["deliveries"];
    lines.push(format!(
        "Incidents: {} open | Alerts: {} pending, {} failed, {} sent",
        diagnostics["open_incidents"].as_array().map_or(0, Vec::len),
        counts["pending"],
        counts["failed"],
        counts["sent"]
    ));
    lines.join("\n")
}

/// Print one line to stdout and flush; a closed pipe unwinds with [`interrupt::BrokenPipe`].
pub fn emit(line: &str) {
    use std::io::Write;
    let mut stdout = std::io::stdout().lock();
    let written = writeln!(stdout, "{line}").and_then(|_| stdout.flush());
    if let Err(error) = written {
        if error.kind() == std::io::ErrorKind::BrokenPipe {
            std::panic::resume_unwind(Box::new(interrupt::BrokenPipe));
        }
    }
}

pub fn error_line(message: &str) -> String {
    pyjson::dumps(&json!({"ok": false, "error": message}))
}
