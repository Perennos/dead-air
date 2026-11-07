//! Perennos DEAD AIR: a bounded local stream watchdog.
//!
//! The library is the whole product; `src/main.rs` only calls [`cli::main`]. Modules map to
//! the Python 0.2.0 layout: configuration, models, persistence, status, the media/network
//! engine, alerts, the GUI, plus the Rust release's Tempo client, MCP server and metrics.

pub mod alerts;
pub mod cli;
pub mod config;
pub mod constants;
pub mod doctor;
pub mod gui;
pub mod http;
pub mod interrupt;
pub mod mcp;
pub mod media;
pub mod metrics;
pub mod model;
pub mod netpolicy;
pub mod observe;
pub mod persistence;
pub mod process;
pub mod pyjson;
pub mod random;
pub mod smtp;
pub mod status;
pub mod tempo;
pub mod timefmt;
pub mod tls;
pub mod url;
pub mod watch;
