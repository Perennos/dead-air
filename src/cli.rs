//! The `dead-air` command line.
//!
//! Exit codes: 0 healthy or success, 1 unhealthy/off-air, 2 unknown/degraded (and usage
//! errors), 3 configuration or operational error, 130 Ctrl+C, 141 closed stdout, 143 SIGTERM.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use clap::{Args, Parser, Subcommand};
use serde_json::json;

use crate::config::{EXAMPLE_TOML, load_config};
use crate::constants::{DEFAULT_CONFIG, DEFAULT_GUI_PORT, DEFAULT_VALIDATE_CONFIG};
use crate::interrupt;
use crate::model::AppConfig;
use crate::persistence::IncidentStore;
use crate::pyjson;
use crate::watch::{CycleReport, WatchHooks, emit, error_line, exit_code, format_observations, validate_watch_mode};

#[derive(Parser, Debug)]
#[command(name = "dead-air", version, about = "Dead Air stream observations: off-air, frozen picture and silence")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Args, Debug, Clone)]
pub struct ConfigArg {
    #[arg(long, default_value = DEFAULT_CONFIG)]
    pub config: String,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Create a private starter configuration without overwriting files
    Init(ConfigArg),
    /// Validate TOML without network or state writes
    Validate {
        #[arg(long, default_value = DEFAULT_VALIDATE_CONFIG)]
        config: String,
    },
    /// Check local binary versions and detector filters (and Tempo, when enabled)
    Doctor(ConfigArg),
    /// Read persisted health without network access or state writes
    Status {
        #[command(flatten)]
        config: ConfigArg,
        #[arg(long)]
        json: bool,
    },
    /// List terminal notification failures without network access
    Failures {
        #[command(flatten)]
        config: ConfigArg,
        #[arg(long, default_value_t = 50, allow_negative_numbers = true)]
        limit: i64,
        #[arg(long, default_value_t = 0, allow_negative_numbers = true)]
        offset: i64,
    },
    /// Acknowledge one terminal failed notification
    AckDelivery {
        #[command(flatten)]
        config: ConfigArg,
        #[arg(long)]
        event_id: String,
        #[arg(long)]
        destination_id: String,
        #[arg(long)]
        operator: String,
    },
    /// List recent incidents and their ids without network access or state writes
    Incidents {
        #[command(flatten)]
        config: ConfigArg,
        /// Only incidents that are still open
        #[arg(long)]
        open: bool,
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
    /// Perform one bounded observation cycle
    Check {
        #[command(flatten)]
        config: ConfigArg,
        #[arg(long)]
        json: bool,
    },
    /// Run serial cycles: --max-cycles N or --continuous
    #[command(group(clap::ArgGroup::new("mode").required(true).args(["max_cycles", "continuous"])))]
    Watch {
        #[command(flatten)]
        config: ConfigArg,
        #[arg(long, allow_negative_numbers = true)]
        max_cycles: Option<i64>,
        /// Run in the foreground until interrupted or a fatal error
        #[arg(long)]
        continuous: bool,
        #[arg(long)]
        json: bool,
    },
    /// Serve the local page on 127.0.0.1
    Gui {
        #[command(flatten)]
        config: ConfigArg,
        #[arg(long, default_value_t = DEFAULT_GUI_PORT as i64, allow_negative_numbers = true)]
        port: i64,
        #[arg(long)]
        no_browser: bool,
        /// Also start watching
        #[arg(long)]
        watch: bool,
    },
    /// Ask Perennos Tempo for an advisory brief about one incident (metadata only)
    Explain {
        /// The incident id (see `status` or `incidents`)
        incident_id: String,
        #[command(flatten)]
        config: ConfigArg,
        #[arg(long)]
        json: bool,
    },
    /// Serve read-only MCP tools over stdio
    Mcp(ConfigArg),
    /// Perennos Tempo integration helpers
    Tempo {
        #[command(subcommand)]
        action: TempoCommand,
    },
    #[command(name = "__notify-worker", hide = true)]
    NotifyWorker,
}

#[derive(Subcommand, Debug)]
pub enum TempoCommand {
    /// Enabled?, URL, health, token present/missing, model count (loopback only)
    Status(ConfigArg),
    /// Print a tool-policy fragment to merge into Tempo (no callers, no tokens)
    Policy,
    /// Print a secret-free MCP client entry with absolute paths
    McpConfig(ConfigArg),
}

fn print_error(message: &str) -> i32 {
    emit(&error_line(message));
    3
}

fn load(path: &str) -> Result<AppConfig, i32> {
    load_config(path).map_err(|error| print_error(&error.0))
}

fn init(path: &str) -> i32 {
    let target = crate::config::expand_user(path);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let outcome = options.open(&target).and_then(|mut file| {
        use std::io::Write;
        file.write_all(EXAMPLE_TOML.as_bytes())
    });
    match outcome {
        Ok(()) => {
            emit(&pyjson::dumps(
                &json!({"ok": true, "created": true, "next": "Edit the source URL, then run validate and doctor."}),
            ));
            0
        }
        Err(error) => {
            let code = if error.kind() == std::io::ErrorKind::AlreadyExists {
                "CONFIG_EXISTS"
            } else {
                "CONFIG_CREATE_FAILED"
            };
            emit(&pyjson::dumps(&json!({"ok": false, "error": code})));
            3
        }
    }
}

fn run_check(config: &AppConfig, json_mode: bool) -> i32 {
    let mut store = match IncidentStore::open(&config.watch.state_path, config.retention_days) {
        Ok(store) => store,
        Err(error) => return print_error(&error.0),
    };
    let mut observer = crate::observe::default_observer();
    let mut cycle = |config: &AppConfig, history: &mut HashMap<_, _>| {
        crate::observe::check_cycle(config, &mut observer, &crate::timefmt::monotonic, history)
    };
    let mut dispatch = crate::watch::default_dispatch;
    let observations = match crate::watch::check_cycle_stateful(config, &mut store, &mut cycle, &mut dispatch) {
        Ok(observations) => observations,
        Err(error) => return print_error(&error.0),
    };
    let diagnostics = match store.diagnostics() {
        Ok(diagnostics) => diagnostics,
        Err(error) => return print_error(&error.0),
    };
    drop(store);
    emit(&format_observations(&observations, &diagnostics, json_mode));
    exit_code(&observations)
}

/// Start the optional metrics endpoint; failure is reported on stderr and never stops watch.
fn start_metrics(
    config: &AppConfig,
) -> (Option<crate::metrics::MetricsServer>, Option<Arc<Mutex<crate::metrics::MetricsState>>>) {
    if !config.metrics.enabled {
        return (None, None);
    }
    let state = Arc::new(Mutex::new(crate::metrics::MetricsState::new(config)));
    match crate::metrics::start(config.metrics.port, Arc::clone(&state)) {
        Ok(server) => {
            eprintln!("{}", pyjson::dumps(&json!({"metrics": format!("http://127.0.0.1:{}/metrics", server.port)})));
            (Some(server), Some(state))
        }
        Err(error) => {
            eprintln!(
                "{}",
                pyjson::dumps(
                    &json!({"warning": "METRICS_UNAVAILABLE", "detail": format!("127.0.0.1:{} ({:?})", config.metrics.port, error.kind())})
                )
            );
            (None, None)
        }
    }
}

fn run_watch(config: &AppConfig, max_cycles: Option<i64>, continuous: bool, json_mode: bool) -> i32 {
    let mode = match validate_watch_mode(max_cycles, continuous) {
        Ok(mode) => mode,
        Err(message) => return print_error(&message),
    };
    let mut store = match IncidentStore::open(&config.watch.state_path, config.retention_days) {
        Ok(store) => store,
        Err(error) => return print_error(&error.0),
    };
    let (_metrics_server, metrics_state) = start_metrics(config);
    let mut observer = crate::observe::default_observer();
    let mut cycle = |config: &AppConfig, history: &mut HashMap<_, _>| {
        crate::observe::check_cycle(config, &mut observer, &crate::timefmt::monotonic, history)
    };
    let mut sleeper = interrupt::sleep;
    let mut dispatch = crate::watch::default_dispatch;
    let mut aggregate = 0;
    let mut on_cycle = |report: CycleReport<'_>| {
        let diagnostics = match report.store {
            Some(store) => store.diagnostics()?,
            None => json!({"open_incidents": [], "deliveries": {"pending": 0, "sent": 0, "failed": 0}}),
        };
        if let Some(state) = &metrics_state {
            state.lock().unwrap_or_else(|e| e.into_inner()).record(
                report.observations,
                report.duration_s,
                report.completed_wall,
                report.store,
            );
        }
        emit(&format_observations(report.observations, &diagnostics, json_mode));
        aggregate = crate::watch::aggregate(aggregate, exit_code(report.observations));
        Ok(())
    };
    let hooks = WatchHooks {
        cycle: &mut cycle,
        sleeper: &mut sleeper,
        clock: &crate::timefmt::monotonic,
        wall: &crate::timefmt::wall,
        record_mono: &crate::timefmt::monotonic,
        dispatch: &mut dispatch,
    };
    let outcome = crate::watch::watch_cycles(config, mode, Some(&mut store), hooks, &mut on_cycle);
    match outcome {
        Ok(()) => aggregate,
        Err(error) => print_error(&error.0),
    }
}

fn run_explain(config: &AppConfig, incident_id: &str, json_mode: bool) -> i32 {
    match crate::tempo::explain(config, incident_id) {
        Err((code, message)) => {
            emit(&error_line(&message));
            code
        }
        Ok(result) => {
            if json_mode {
                emit(&pyjson::dumps(&result));
            } else {
                let receipt = result["receipt_id"].as_str().unwrap_or("not provided");
                let note = if result["advisory"] == true {
                    "advisory: review before use"
                } else {
                    "Tempo marked review as not required"
                };
                emit(&format!(
                    "Perennos Tempo brief for incident {incident_id} ({note}; receipt {receipt})\n\n{}",
                    result["brief"].as_str().unwrap_or("")
                ));
                if result["stored"] != true {
                    eprintln!("{}", error_line(result["error"].as_str().unwrap_or("BRIEF_NOT_STORED")));
                }
            }
            if result["stored"] == true { 0 } else { 3 }
        }
    }
}

fn run(cli: Cli) -> i32 {
    match cli.command {
        Command::NotifyWorker => crate::alerts::notification_worker(),
        Command::Init(args) => init(&args.config),
        Command::Gui { config, port, no_browser, watch } => {
            crate::gui::serve(&config.config, port, !no_browser, watch, crate::gui::IDLE_S)
        }
        Command::Mcp(args) => crate::mcp::run(Path::new(&args.config)),
        Command::Tempo { action: TempoCommand::Policy } => {
            emit(&pyjson::dumps(&crate::tempo::policy_fragment()));
            0
        }
        Command::Tempo { action: TempoCommand::McpConfig(args) } => {
            emit(&pyjson::dumps(&crate::tempo::mcp_config(Path::new(&args.config))));
            0
        }
        Command::Watch { config, max_cycles, continuous, json } => {
            // The mode is validated before the configuration or state is touched.
            if let Err(message) = validate_watch_mode(max_cycles, continuous) {
                return print_error(&message);
            }
            match load(&config.config) {
                Ok(app) => run_watch(&app, max_cycles, continuous, json),
                Err(code) => code,
            }
        }
        command => {
            let path = match &command {
                Command::Validate { config } => config.clone(),
                Command::Doctor(a) | Command::Tempo { action: TempoCommand::Status(a) } => a.config.clone(),
                Command::Status { config, .. }
                | Command::Failures { config, .. }
                | Command::AckDelivery { config, .. }
                | Command::Incidents { config, .. }
                | Command::Check { config, .. }
                | Command::Explain { config, .. } => config.config.clone(),
                _ => DEFAULT_CONFIG.into(),
            };
            let config = match load(&path) {
                Ok(config) => config,
                Err(code) => return code,
            };
            run_with_config(command, &config)
        }
    }
}

fn run_with_config(command: Command, config: &AppConfig) -> i32 {
    match command {
        Command::Validate { .. } => {
            emit(&pyjson::dumps(&json!({"ok": true, "stream_count": config.streams.len()})));
            0
        }
        Command::Doctor(_) => {
            let runner = crate::process::run_bounded;
            let result = crate::doctor::doctor(config, &runner, &crate::doctor::which);
            emit(&pyjson::dumps(&result));
            if result["ok"] == true { 0 } else { 3 }
        }
        Command::Status { json, .. } => match crate::status::read_status(config, None) {
            Ok(result) => {
                emit(&if json { pyjson::dumps(&result) } else { crate::status::format_status(&result) });
                crate::status::status_exit_code(&result)
            }
            Err(error) => print_error(&error.0),
        },
        Command::Failures { limit, offset, .. } => match crate::status::read_failed_deliveries(config, limit, offset) {
            Ok(failures) => {
                emit(&pyjson::dumps(&json!({"ok": true, "failures": failures, "limit": limit, "offset": offset})));
                0
            }
            Err(message) => print_error(&message),
        },
        Command::AckDelivery { event_id, destination_id, operator, .. } => {
            let outcome = crate::persistence::acknowledge_failed_delivery(
                Path::new(&config.watch.state_path),
                &event_id,
                &destination_id,
                &operator,
                &crate::timefmt::utc_now_micro(),
                crate::timefmt::wall(),
                config.retention_days,
            );
            match outcome {
                Ok(result) => {
                    let ok = result == "acknowledged" || result == "already_acknowledged";
                    emit(&pyjson::dumps(
                        &json!({"ok": ok, "result": result, "event_id": event_id, "destination_id": destination_id}),
                    ));
                    if ok { 0 } else { 1 }
                }
                Err(error) => print_error(&error.to_string()),
            }
        }
        Command::Incidents { open, limit, .. } => match crate::status::list_incidents(config, open, limit) {
            Ok(result) => {
                let mut result = result;
                result["ok"] = json!(true);
                result["network_contacted"] = json!(false);
                emit(&pyjson::dumps(&result));
                0
            }
            Err(error) => print_error(&error.0),
        },
        Command::Check { json, .. } => run_check(config, json),
        Command::Explain { incident_id, json, .. } => run_explain(config, &incident_id, json),
        Command::Tempo { action: TempoCommand::Status(_) } => {
            let report = crate::tempo::status_report(config);
            emit(&pyjson::dumps(&report));
            if report["health"] == "ok" && report["token"] == "present" && report["models"].is_number() { 0 } else { 2 }
        }
        _ => 3,
    }
}

fn quiet_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if interrupt::is_control_payload(info.payload()) {
            return;
        }
        default(info);
    }));
}

/// Parse arguments, install signal handling, and map unwinds to exit codes.
pub fn main() -> i32 {
    quiet_panic_hook();
    if let Ok(exe) = std::env::current_exe() {
        crate::alerts::set_helper_exe(exe);
    }
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let _ = error.print();
            return error.exit_code();
        }
    };
    if matches!(cli.command, Command::NotifyWorker | Command::Mcp(_)) {
        // The worker and the MCP server keep the default signal behaviour: a stop ends them.
        return run(cli);
    }
    interrupt::install_handlers();
    interrupt::set_interruptible(true);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(cli)));
    let signal = interrupt::pending();
    match outcome {
        Ok(_) if signal == interrupt::SIGTERM => 128 + signal,
        Ok(code) => code,
        Err(payload) if payload.is::<interrupt::BrokenPipe>() => 141,
        Err(payload) if payload.is::<interrupt::Interrupted>() => {
            128 + if signal != 0 { signal } else { interrupt::SIGINT }
        }
        Err(_) => {
            let _ = std::panic::catch_unwind(|| emit(&error_line("INTERNAL_ERROR")));
            3
        }
    }
}
