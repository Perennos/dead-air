//! Alert delivery: stable event IDs, at most three attempts at 0/30/120 s, at-least-once.
//!
//! Each attempt runs in an owned child process (`dead-air __notify-worker`) that receives
//! the event and the destination's *names* of environment variables on stdin, and only
//! those named variables in its environment. The parent stops and reaps it inside the
//! attempt deadline. No AI or other optional service sits on this path.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::config::email_address;
use crate::constants::{
    NOTIFICATION_ATTEMPT_BUDGET_S, NOTIFICATION_CLEANUP_RESERVE_S, NOTIFICATION_DISPATCH_BUDGET_S, VERSION,
};
use crate::http::{PostOutcome, bounded_http_post};
use crate::interrupt;
use crate::model::{DestinationConfig, NetworkConfig, PersistenceError};
use crate::netpolicy::{Resolver, validate_network_target};
use crate::process::child_environment;
use crate::pyjson;
use crate::smtp::{RealConnector, SmtpConnector, SmtpSession};

#[derive(Clone, Debug, PartialEq)]
pub struct SendOutcome {
    pub ok: bool,
    pub reason: Option<String>,
    pub status: Option<u16>,
    pub retry_after: Option<f64>,
}

impl SendOutcome {
    pub fn sent(status: Option<u16>) -> Self {
        SendOutcome { ok: true, reason: None, status, retry_after: None }
    }

    pub fn failed(reason: &str) -> Self {
        SendOutcome { ok: false, reason: Some(reason.into()), status: None, retry_after: None }
    }

    pub fn to_value(&self) -> Value {
        json!([self.ok, self.reason, self.status, self.retry_after.map(pyjson::float)])
    }

    pub fn from_value(value: &Value) -> Option<Self> {
        let items = value.as_array().filter(|a| a.len() == 4)?;
        Some(SendOutcome {
            ok: items[0].as_bool()?,
            reason: items[1].as_str().map(str::to_string),
            status: items[2].as_u64().and_then(|s| u16::try_from(s).ok()),
            retry_after: items[3].as_f64(),
        })
    }
}

fn text(event: &Value, key: &str) -> Option<String> {
    match event.get(key)? {
        Value::String(s) => Some(s.clone()),
        other => Some(pyjson::dumps(other)),
    }
}

/// `Dead Air <transition>: <stream> <condition> (<event id>)`.
pub fn notification_summary(event: &Value) -> Option<String> {
    Some(format!(
        "Dead Air {}: {} {} ({})",
        text(event, "transition")?,
        text(event, "stream_id")?,
        text(event, "condition")?,
        text(event, "event_id")?
    ))
}

fn has_control(text: &str) -> bool {
    text.chars().any(|c| (c as u32) < 0x20 || c as u32 == 0x7f)
}

/// The HTTP transport seam: `(url, body, headers, timeout_s) -> outcome`.
pub type HttpPost<'a> = &'a dyn Fn(&str, &[u8], &[(String, String)], f64) -> PostOutcome;

pub struct SendDeps<'a> {
    pub env: &'a dyn Fn(&str) -> Option<String>,
    pub http: Option<HttpPost<'a>>,
    pub resolver: &'a Resolver,
    pub smtp: &'a dyn SmtpConnector,
    pub deadline: Option<f64>,
    pub monotonic: &'a dyn Fn() -> f64,
}

struct DeadlineExceeded;

fn email_date(epoch: f64) -> String {
    let seconds = epoch.floor() as i64;
    let days = seconds.div_euclid(86_400);
    let rem = seconds.rem_euclid(86_400);
    let (y, m, d) = crate::timefmt::civil_from_days(days);
    let weekday = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"][days.rem_euclid(7) as usize];
    let month = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"][m as usize - 1];
    format!("{weekday}, {d:02} {month} {y} {:02}:{:02}:{:02} +0000", rem / 3600, (rem % 3600) / 60, rem % 60)
}

/// A plain-text message; 7bit when every line fits, base64 otherwise.
pub fn build_email(sender: &str, recipients: &[String], subject: &str, body: &str) -> Vec<u8> {
    let seven_bit = body.is_ascii() && body.lines().all(|l| l.len() <= 900);
    let mut message = format!(
        "From: {sender}\r\nTo: {}\r\nSubject: {subject}\r\nDate: {}\r\nMessage-ID: <{}@dead-air.invalid>\r\nMIME-Version: 1.0\r\nContent-Type: text/plain; charset=\"utf-8\"\r\n",
        recipients.join(", "),
        email_date(crate::timefmt::wall()),
        crate::random::uuid4_hex()
    );
    if seven_bit {
        message.push_str("Content-Transfer-Encoding: 7bit\r\n\r\n");
        message.push_str(&body.replace("\r\n", "\n").replace('\n', "\r\n"));
    } else {
        message.push_str("Content-Transfer-Encoding: base64\r\n\r\n");
        let encoded = crate::random::base64(body.as_bytes(), false);
        for chunk in encoded.as_bytes().chunks(76) {
            message.push_str(std::str::from_utf8(chunk).unwrap_or(""));
            message.push_str("\r\n");
        }
    }
    message.into_bytes()
}

fn send_smtp(
    destination: &DestinationConfig,
    event: &Value,
    network: &NetworkConfig,
    summary: &str,
    deps: &SendDeps<'_>,
) -> SendOutcome {
    let remaining = || -> Result<f64, DeadlineExceeded> {
        match deps.deadline {
            None => Ok(5.0),
            Some(deadline) => {
                let left = deadline - (deps.monotonic)();
                if left <= 0.0 { Err(DeadlineExceeded) } else { Ok(left.min(5.0)) }
            }
        }
    };
    let host = destination.host.clone().unwrap_or_default();
    let port = destination.port.unwrap_or(0);
    let url_host = if host.contains(':') && !host.starts_with('[') { format!("[{host}]") } else { host.clone() };
    let mut client: Option<Box<dyn SmtpSession>> = None;
    let outcome = (|| -> Result<SendOutcome, Option<DeadlineExceeded>> {
        let addresses = validate_network_target(
            &format!("https://{url_host}:{port}/"),
            "direct",
            network,
            destination.allow_private,
            deps.resolver,
            false,
            None,
        )
        .map_err(|_| None)?;
        remaining().map_err(Some)?;
        let sender = email_address(destination.sender.as_deref().unwrap_or(""), "sender").map_err(|_| None)?;
        let recipients: Vec<String> = destination
            .recipients
            .iter()
            .map(|r| email_address(r, "recipient"))
            .collect::<Result<_, _>>()
            .map_err(|_| None)?;
        let body = format!("{summary}\n{}", pyjson::dumps(event));
        let message = build_email(&sender, &recipients, summary, &body);
        if destination.tls.as_deref() == Some("ssl") {
            client =
                Some(deps.smtp.connect_ssl(&host, port, &addresses, remaining().map_err(Some)?).map_err(|_| None)?);
        } else {
            client =
                Some(deps.smtp.connect_plain(&host, port, &addresses, remaining().map_err(Some)?).map_err(|_| None)?);
            let session = client.as_mut().expect("client");
            remaining().map_err(Some)?;
            session.ehlo().map_err(|_| None)?;
            remaining().map_err(Some)?;
            session.starttls().map_err(|_| None)?;
            remaining().map_err(Some)?;
            session.ehlo().map_err(|_| None)?;
        }
        remaining().map_err(Some)?;
        let session = client.as_mut().expect("client");
        if let Some(username_env) = &destination.username_env {
            let username = (deps.env)(username_env).unwrap_or_default();
            let password = destination.password_env.as_deref().and_then(|n| (deps.env)(n)).unwrap_or_default();
            if username.is_empty()
                || password.is_empty()
                || username.len() > 4096
                || password.len() > 4096
                || has_control(&format!("{username}{password}"))
            {
                return Ok(SendOutcome::failed("SMTP_CREDENTIALS_MISSING"));
            }
            remaining().map_err(Some)?;
            session.login(&username, &password).map_err(|_| None)?;
            remaining().map_err(Some)?;
        }
        let refused = session.send_message(&sender, &recipients, &message).map_err(|_| None)?;
        remaining().map_err(Some)?;
        if !refused.is_empty() {
            return Ok(SendOutcome::failed("SMTP_PARTIAL_FAILURE"));
        }
        Ok(SendOutcome::sent(None))
    })();
    if let Some(mut session) = client {
        let spent = deps.deadline.is_some_and(|d| (deps.monotonic)() >= d);
        if spent || session.quit().is_err() {
            session.close();
        }
    }
    match outcome {
        Ok(result) => result,
        Err(Some(DeadlineExceeded)) => SendOutcome::failed("DELIVERY_TIMEOUT"),
        Err(None) => SendOutcome::failed("SMTP_DELIVERY_FAILED"),
    }
}

/// One delivery attempt from inside the worker (or a test). Never panics on bad input.
pub fn send_destination(
    destination: &DestinationConfig,
    event: &Value,
    network: &NetworkConfig,
    deps: &SendDeps<'_>,
) -> SendOutcome {
    let Some(summary) = notification_summary(event) else {
        return SendOutcome::failed("DELIVERY_INTERNAL_ERROR");
    };
    if destination.kind == "smtp" {
        return send_smtp(destination, event, network, &summary, deps);
    }
    let endpoint = destination.endpoint_env.as_deref().and_then(|n| (deps.env)(n)).unwrap_or_default();
    if endpoint.is_empty() || endpoint.len() > 4096 || has_control(&endpoint) {
        return SendOutcome::failed("ENDPOINT_UNAVAILABLE");
    }
    let mut headers = vec![("User-Agent".to_string(), format!("Dead-Air/{VERSION}"))];
    let body: Vec<u8> = match destination.kind.as_str() {
        "ntfy" => {
            headers.push(("Content-Type".into(), "text/plain; charset=utf-8".into()));
            headers.push(("Title".into(), "Dead Air incident".into()));
            summary.clone().into_bytes()
        }
        "discord" => {
            headers.push(("Content-Type".into(), "application/json".into()));
            pyjson::dumps_compact(&json!({"content": summary, "allowed_mentions": {"parse": []}})).into_bytes()
        }
        _ => {
            headers.push(("Content-Type".into(), "application/json".into()));
            pyjson::dumps_compact(event).into_bytes()
        }
    };
    if let Some(token_env) = &destination.token_env {
        let token = (deps.env)(token_env).unwrap_or_default();
        if token.is_empty() || token.len() > 4096 || has_control(&token) {
            return SendOutcome::failed("TOKEN_UNAVAILABLE");
        }
        headers.push(("Authorization".into(), format!("Bearer {token}")));
    }
    let remaining = || -> Result<f64, DeadlineExceeded> {
        match deps.deadline {
            None => Ok(5.0),
            Some(deadline) => {
                let left = deadline - (deps.monotonic)();
                if left <= 0.0 { Err(DeadlineExceeded) } else { Ok(left.min(5.0)) }
            }
        }
    };
    let Ok(timeout) = remaining() else {
        return SendOutcome::failed("DELIVERY_TIMEOUT");
    };
    let (status, error, retry_after) = match deps.http {
        Some(transport) => transport(&endpoint, &body, &headers, timeout),
        None => {
            bounded_http_post(&endpoint, &body, &headers, timeout, network, deps.resolver, destination.allow_private)
        }
    };
    if remaining().is_err() {
        return SendOutcome::failed("DELIVERY_TIMEOUT");
    }
    if status.is_some_and(|s| (200..300).contains(&s)) && error.is_none() {
        return SendOutcome::sent(status);
    }
    SendOutcome { ok: false, reason: Some(error.unwrap_or_else(|| "HTTP_STATUS".into())), status, retry_after }
}

/// Result of waiting on a worker.
pub enum Communicated {
    Output(Vec<u8>),
    TimedOut,
    Interrupted,
    Failed,
}

/// An owned sender child. A fake implements this in tests.
pub trait WorkerProcess {
    fn communicate(&mut self, input: &[u8], timeout_s: f64) -> Communicated;
    fn poll(&mut self) -> Option<i32>;
    fn terminate(&mut self);
    fn kill(&mut self);
    fn wait(&mut self, timeout_s: f64) -> Option<i32>;
    fn close_pipes(&mut self);
}

pub trait WorkerLauncher {
    fn launch(&self, argv: &[String], env: &[(String, String)]) -> std::io::Result<Box<dyn WorkerProcess>>;
    fn argv(&self) -> Vec<String>;
}

static HELPER_EXE: OnceLock<PathBuf> = OnceLock::new();

/// The binary that serves `__notify-worker` (set by `main`; defaults to the current exe).
pub fn set_helper_exe(path: PathBuf) {
    let _ = HELPER_EXE.set(path);
}

pub fn helper_exe() -> PathBuf {
    HELPER_EXE.get().cloned().or_else(|| std::env::current_exe().ok()).unwrap_or_else(|| PathBuf::from("dead-air"))
}

/// Launches the real worker; `extra_env` lets tests trust a fixture certificate.
pub struct ChildLauncher {
    pub exe: PathBuf,
    pub extra_env: Vec<(String, String)>,
}

impl ChildLauncher {
    pub fn current() -> Self {
        ChildLauncher { exe: helper_exe(), extra_env: Vec::new() }
    }
}

struct RealWorker {
    child: Child,
    output: Option<mpsc::Receiver<Vec<u8>>>,
}

impl WorkerLauncher for ChildLauncher {
    fn argv(&self) -> Vec<String> {
        vec![self.exe.to_string_lossy().into_owned(), "__notify-worker".into()]
    }

    fn launch(&self, argv: &[String], env: &[(String, String)]) -> std::io::Result<Box<dyn WorkerProcess>> {
        let mut command = Command::new(&argv[0]);
        command.args(&argv[1..]).env_clear().envs(env.iter().cloned()).envs(self.extra_env.iter().cloned());
        command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(crate::process::CREATE_NO_WINDOW);
        }
        let child = command.spawn()?;
        Ok(Box::new(RealWorker { child, output: None }))
    }
}

impl WorkerProcess for RealWorker {
    fn communicate(&mut self, input: &[u8], timeout_s: f64) -> Communicated {
        let deadline = Instant::now() + Duration::from_secs_f64(timeout_s.clamp(0.0, 600.0));
        if let Some(mut stdin) = self.child.stdin.take() {
            let data = input.to_vec();
            std::thread::spawn(move || {
                let _ = stdin.write_all(&data);
            });
        }
        if self.output.is_none() {
            if let Some(mut stdout) = self.child.stdout.take() {
                let (tx, rx) = mpsc::channel();
                std::thread::spawn(move || {
                    let mut buffer = Vec::new();
                    let _ = (&mut stdout).take(65_536).read_to_end(&mut buffer);
                    let _ = tx.send(buffer);
                });
                self.output = Some(rx);
            }
        }
        loop {
            if interrupt::requested() {
                return Communicated::Interrupted;
            }
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {}
                Err(_) => return Communicated::Failed,
            }
            if Instant::now() >= deadline {
                return Communicated::TimedOut;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let Some(receiver) = self.output.take() else {
            return Communicated::Output(Vec::new());
        };
        let left = deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(200));
        match receiver.recv_timeout(left) {
            Ok(output) => Communicated::Output(output),
            Err(_) => Communicated::TimedOut,
        }
    }

    fn poll(&mut self) -> Option<i32> {
        self.child.try_wait().ok().flatten().map(crate::process::exit_code)
    }

    fn terminate(&mut self) {
        #[cfg(unix)]
        // SAFETY: signalling our own direct child.
        unsafe {
            libc::kill(self.child.id() as libc::pid_t, libc::SIGTERM);
        }
        #[cfg(windows)]
        {
            let _ = self.child.kill();
        }
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
    }

    fn wait(&mut self, timeout_s: f64) -> Option<i32> {
        let end = Instant::now() + Duration::from_secs_f64(timeout_s.clamp(0.0, 60.0));
        loop {
            if let Some(code) = self.poll() {
                return Some(code);
            }
            if Instant::now() >= end {
                return None;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn close_pipes(&mut self) {
        self.child.stdin.take();
        self.child.stdout.take();
        self.output = None;
    }
}

/// Stop and reap the sender child within the reserved cleanup window.
fn stop_worker(child: &mut dyn WorkerProcess, deadline: f64, monotonic: &dyn Fn() -> f64) -> bool {
    if child.poll().is_none() {
        child.terminate();
        let left = deadline - monotonic();
        if left > 0.0 {
            child.wait(left.min(0.2));
        }
        if child.poll().is_none() {
            child.kill();
        }
        let left = deadline - monotonic();
        if left > 0.0 && child.poll().is_none() {
            child.wait(left);
        }
    }
    let stopped = child.poll().is_some();
    child.close_pipes();
    stopped
}

/// Environment for one worker: runtime essentials plus only this destination's named
/// variables (and certificate-store overrides, which are trust settings, not secrets).
pub fn worker_environment(
    destination: &DestinationConfig,
    env: &dyn Fn(&str) -> Option<String>,
) -> Vec<(String, String)> {
    let mut result = child_environment();
    for name in destination.env_names().into_iter().chain(["SSL_CERT_FILE", "SSL_CERT_DIR"]) {
        if let Some(value) = env(name) {
            result.push((name.to_string(), value));
        }
    }
    result
}

/// One delivery attempt in an owned child, stopped and reaped inside `timeout_s`.
pub fn send_destination_bounded(
    destination: &DestinationConfig,
    event: &Value,
    network: &NetworkConfig,
    timeout_s: f64,
    env: &dyn Fn(&str) -> Option<String>,
    monotonic: &dyn Fn() -> f64,
    launcher: &dyn WorkerLauncher,
) -> SendOutcome {
    if !timeout_s.is_finite() || timeout_s <= NOTIFICATION_CLEANUP_RESERVE_S {
        return SendOutcome::failed("DELIVERY_TIMEOUT");
    }
    let started = monotonic();
    let deadline = started + timeout_s;
    let work_deadline = deadline - NOTIFICATION_CLEANUP_RESERVE_S;
    let data = json!({
        "destination": destination.to_value(),
        "event": event,
        "network": {"private_allowlist": network.private_allowlist, "max_redirects": network.max_redirects,
                    "thumbnail_max_bytes": network.thumbnail_max_bytes},
        "timeout_s": pyjson::float((work_deadline - monotonic()).max(0.01)),
    });
    let payload = pyjson::dumps_compact(&data).into_bytes();
    if payload.len() > 65_536 {
        return SendOutcome::failed("DELIVERY_PAYLOAD_LIMIT");
    }
    let Ok(mut child) = launcher.launch(&launcher.argv(), &worker_environment(destination, env)) else {
        return SendOutcome::failed("DELIVERY_WORKER_FAILED");
    };
    let left = work_deadline - monotonic();
    let mut interrupted = false;
    let result = if left <= 0.0 {
        SendOutcome::failed("DELIVERY_TIMEOUT")
    } else {
        match child.communicate(&payload, left) {
            Communicated::TimedOut => SendOutcome::failed("DELIVERY_TIMEOUT"),
            Communicated::Interrupted => {
                interrupted = true;
                SendOutcome::failed("DELIVERY_TIMEOUT")
            }
            Communicated::Failed => SendOutcome::failed("DELIVERY_WORKER_FAILED"),
            Communicated::Output(output) => {
                if monotonic() > work_deadline {
                    SendOutcome::failed("DELIVERY_TIMEOUT")
                } else if child.poll() != Some(0) || output.len() > 4096 {
                    SendOutcome::failed("DELIVERY_WORKER_FAILED")
                } else {
                    serde_json::from_slice::<Value>(&output)
                        .ok()
                        .and_then(|v| v.get("result").and_then(SendOutcome::from_value))
                        .unwrap_or_else(|| SendOutcome::failed("DELIVERY_WORKER_FAILED"))
                }
            }
        }
    };
    let cleanup_confirmed = stop_worker(child.as_mut(), deadline, monotonic);
    if interrupted {
        std::panic::resume_unwind(Box::new(interrupt::Interrupted));
    }
    if !cleanup_confirmed {
        return SendOutcome::failed("DELIVERY_CLEANUP_FAILED");
    }
    result
}

/// The worker side of the child protocol.
pub fn notification_worker() -> i32 {
    let failure = || {
        println!("{{\"result\":[false,\"DELIVERY_INTERNAL_ERROR\",null,null]}}");
        0
    };
    let mut raw = Vec::new();
    if std::io::stdin().take(65_537).read_to_end(&mut raw).is_err() || raw.len() > 65_536 {
        return failure();
    }
    let Ok(data) = serde_json::from_slice::<Value>(&raw) else {
        return failure();
    };
    let Some(destination) = data.get("destination").and_then(DestinationConfig::from_value) else {
        return failure();
    };
    let network = data.get("network").and_then(|n| {
        Some(NetworkConfig {
            private_allowlist: n
                .get("private_allowlist")?
                .as_array()?
                .iter()
                .map(|v| v.as_str().map(str::to_string))
                .collect::<Option<Vec<_>>>()?,
            max_redirects: u32::try_from(n.get("max_redirects")?.as_u64()?).ok()?,
            thumbnail_max_bytes: usize::try_from(n.get("thumbnail_max_bytes")?.as_u64()?).ok()?,
        })
    });
    let (Some(network), Some(timeout_s), Some(event)) =
        (network, data.get("timeout_s").and_then(Value::as_f64), data.get("event"))
    else {
        return failure();
    };
    if !timeout_s.is_finite() || timeout_s <= 0.0 {
        return failure();
    }
    let env = |name: &str| std::env::var(name).ok();
    let resolver = Resolver::System;
    let deps = SendDeps {
        env: &env,
        http: None,
        resolver: &resolver,
        smtp: &RealConnector,
        deadline: Some(crate::timefmt::monotonic() + timeout_s),
        monotonic: &crate::timefmt::monotonic,
    };
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        send_destination(&destination, event, &network, &deps)
    }));
    match outcome {
        Ok(outcome) => {
            println!("{}", pyjson::dumps_compact(&json!({"result": outcome.to_value()})));
            0
        }
        Err(_) => failure(),
    }
}

/// One due delivery row.
#[derive(Clone, Debug, PartialEq)]
pub struct DueDelivery {
    pub event_id: String,
    pub destination_id: String,
    pub attempts: i64,
    pub created_wall: f64,
    pub payload_json: String,
}

/// The delivery ledger seam (the incident store; a fake in tests).
pub trait DeliveryLedger {
    fn due_deliveries(
        &mut self,
        now_wall: f64,
        limit: usize,
        destination_ids: &[String],
    ) -> Result<Vec<DueDelivery>, PersistenceError>;
    fn record_delivery(
        &mut self,
        row: &DueDelivery,
        success: bool,
        reason: Option<&str>,
        http_status: Option<u16>,
        now_wall: f64,
        retry_after_s: Option<f64>,
    ) -> Result<(), PersistenceError>;
}

/// Synchronous sender seam used by tests: `(destination, event, network) -> outcome`.
pub type SenderFn<'a> = dyn FnMut(&DestinationConfig, &Value, &NetworkConfig) -> SendOutcome + 'a;

pub struct DispatchOptions<'a> {
    pub now_wall: Option<f64>,
    /// Synchronous sender seam used by tests: `(destination, event, network) -> outcome`.
    pub sender: Option<&'a mut SenderFn<'a>>,
    pub monotonic: &'a dyn Fn() -> f64,
    pub dispatch_budget_s: f64,
    pub attempt_budget_s: f64,
    pub env: &'a dyn Fn(&str) -> Option<String>,
    pub launcher: Option<&'a dyn WorkerLauncher>,
}

fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

impl Default for DispatchOptions<'_> {
    fn default() -> Self {
        DispatchOptions {
            now_wall: None,
            sender: None,
            monotonic: &crate::timefmt::monotonic,
            dispatch_budget_s: NOTIFICATION_DISPATCH_BUDGET_S,
            attempt_budget_s: NOTIFICATION_ATTEMPT_BUDGET_S,
            env: &process_env,
            launcher: None,
        }
    }
}

/// Attempt every due delivery for the configured destinations within the dispatch budget.
/// Removed destinations keep their pending rows untouched until they return.
pub fn dispatch_due(
    store: &mut dyn DeliveryLedger,
    destinations: &[DestinationConfig],
    network: &NetworkConfig,
    mut options: DispatchOptions<'_>,
) -> Result<(), PersistenceError> {
    let when = options.now_wall.unwrap_or_else(crate::timefmt::wall);
    let ids: Vec<String> = destinations.iter().map(|d| d.id.clone()).collect();
    if ids.is_empty() || !options.dispatch_budget_s.is_finite() || options.dispatch_budget_s <= 0.0 {
        return Ok(());
    }
    let monotonic = options.monotonic;
    let deadline = monotonic() + options.dispatch_budget_s;
    let default_launcher = ChildLauncher::current();
    let launcher: &dyn WorkerLauncher = options.launcher.unwrap_or(&default_launcher);
    for row in store.due_deliveries(when, 8, &ids)? {
        let left = deadline - monotonic();
        if left <= NOTIFICATION_CLEANUP_RESERVE_S {
            break;
        }
        let Some(destination) = destinations.iter().find(|d| d.id == row.destination_id) else {
            continue;
        };
        let attempt = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            match serde_json::from_str::<Value>(&row.payload_json) {
                Err(_) => SendOutcome::failed("DELIVERY_INTERNAL_ERROR"),
                Ok(event) => match options.sender.as_mut() {
                    Some(sender) => sender(destination, &event, network),
                    None => send_destination_bounded(
                        destination,
                        &event,
                        network,
                        options.attempt_budget_s.min(left),
                        options.env,
                        monotonic,
                        launcher,
                    ),
                },
            }
        }));
        let outcome = match attempt {
            Ok(outcome) => outcome,
            Err(payload) if interrupt::is_control_payload(payload.as_ref()) => std::panic::resume_unwind(payload),
            Err(_) => SendOutcome::failed("DELIVERY_INTERNAL_ERROR"),
        };
        store.record_delivery(
            &row,
            outcome.ok,
            outcome.reason.as_deref(),
            outcome.status,
            when,
            outcome.retry_after,
        )?;
        if outcome.reason.as_deref() == Some("DELIVERY_CLEANUP_FAILED") {
            return Err(PersistenceError::new("DELIVERY_CLEANUP_FAILED"));
        }
        if monotonic() >= deadline {
            break;
        }
    }
    Ok(())
}
