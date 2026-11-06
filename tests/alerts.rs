//! Alert payloads, SMTP rules, attempt bounds and the delivery ledger (Python `test_alerts.py`).

mod common;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::net::IpAddr;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use common::*;
use dead_air::alerts::*;
use dead_air::model::*;
use dead_air::netpolicy::Resolver;
use dead_air::persistence::IncidentStore;
use dead_air::smtp::{SmtpConnector, SmtpError, SmtpSession};
use serde_json::{Value, json};

fn event() -> Value {
    json!({"event_id": "evt-123", "incident_id": "inc-123", "stream_id": "sample-channel",
           "condition": "audio_silence", "transition": "open", "observed_at": "2026-10-23T00:00:00Z",
           "status": "UNHEALTHY", "reason_codes": ["AUDIO_SILENCE"]})
}

fn network() -> NetworkConfig {
    test_config(2.0).network
}

fn smtp(port: u16, tls: &str) -> DestinationConfig {
    DestinationConfig {
        id: "mail".into(),
        kind: "smtp".into(),
        host: Some("mail.example.invalid".into()),
        port: Some(port),
        tls: Some(tls.into()),
        sender: Some("dead-air@example.invalid".into()),
        recipients: vec!["ops@example.invalid".into()],
        ..Default::default()
    }
}

#[derive(Default)]
struct SessionLog {
    ehlo: Cell<u32>,
    tls_started: Cell<bool>,
    logged_in: Cell<bool>,
    closed: Cell<bool>,
    quit: Cell<bool>,
    message: RefCell<Option<String>>,
    refused: RefCell<Vec<String>>,
    fail_starttls: Cell<bool>,
    clock: Option<Rc<Cell<f64>>>,
}

struct FakeSession(Rc<SessionLog>);

impl SmtpSession for FakeSession {
    fn ehlo(&mut self) -> Result<(), SmtpError> {
        self.0.ehlo.set(self.0.ehlo.get() + 1);
        if let Some(clock) = &self.0.clock {
            clock.set(clock.get() + 0.3);
        }
        Ok(())
    }
    fn starttls(&mut self) -> Result<(), SmtpError> {
        if let Some(clock) = &self.0.clock {
            clock.set(clock.get() + 0.8);
        }
        if self.0.fail_starttls.get() {
            return Err(SmtpError("private detail".into()));
        }
        self.0.tls_started.set(true);
        Ok(())
    }
    fn login(&mut self, _u: &str, _p: &str) -> Result<(), SmtpError> {
        self.0.logged_in.set(true);
        Ok(())
    }
    fn send_message(&mut self, _s: &str, _r: &[String], message: &[u8]) -> Result<Vec<String>, SmtpError> {
        *self.0.message.borrow_mut() = Some(String::from_utf8_lossy(message).into_owned());
        Ok(self.0.refused.borrow().clone())
    }
    fn quit(&mut self) -> Result<(), SmtpError> {
        self.0.quit.set(true);
        Ok(())
    }
    fn close(&mut self) {
        self.0.closed.set(true);
    }
}

/// Records which kind of connection was requested; never opens a socket.
struct FakeConnector {
    log: Rc<SessionLog>,
    calls: RefCell<Vec<(String, Vec<IpAddr>)>>,
}

impl FakeConnector {
    fn new(log: Rc<SessionLog>) -> Self {
        FakeConnector { log, calls: RefCell::new(Vec::new()) }
    }
}

impl SmtpConnector for FakeConnector {
    fn connect_ssl(&self, _h: &str, _p: u16, addresses: &[IpAddr], _t: f64) -> Result<Box<dyn SmtpSession>, SmtpError> {
        self.calls.borrow_mut().push(("ssl".into(), addresses.to_vec()));
        Ok(Box::new(FakeSession(Rc::clone(&self.log))))
    }
    fn connect_plain(
        &self,
        _h: &str,
        _p: u16,
        addresses: &[IpAddr],
        _t: f64,
    ) -> Result<Box<dyn SmtpSession>, SmtpError> {
        self.calls.borrow_mut().push(("plain".into(), addresses.to_vec()));
        Ok(Box::new(FakeSession(Rc::clone(&self.log))))
    }
}

fn no_env(_: &str) -> Option<String> {
    None
}

fn deps<'a>(
    env: &'a dyn Fn(&str) -> Option<String>,
    resolver: &'a Resolver,
    smtp: &'a dyn SmtpConnector,
) -> SendDeps<'a> {
    SendDeps { env, http: None, resolver, smtp, deadline: None, monotonic: &now_mono }
}

#[test]
fn ntfy_discord_and_webhook_payloads_are_stable_and_redacted() {
    type Seen = Vec<(String, Vec<u8>, Vec<(String, String)>, f64)>;
    let seen: RefCell<Seen> = RefCell::new(Vec::new());
    let http = |url: &str, body: &[u8], headers: &[(String, String)], timeout: f64| {
        seen.borrow_mut().push((url.to_string(), body.to_vec(), headers.to_vec(), timeout));
        (Some(204), None, None)
    };
    let resolver = Resolver::always(&["8.8.8.8"]);
    let connector = FakeConnector::new(Rc::default());
    let cases: Vec<(DestinationConfig, HashMap<&str, &str>)> = vec![
        (
            DestinationConfig {
                token_env: Some("NTFY_TOKEN".into()),
                ..DestinationConfig::webhook("n", "ntfy", "NTFY_URL")
            },
            HashMap::from([("NTFY_URL", "https://ntfy.invalid/topic/private"), ("NTFY_TOKEN", "secret-token")]),
        ),
        (
            DestinationConfig::webhook("d", "discord", "DISCORD_URL"),
            HashMap::from([("DISCORD_URL", "https://discord.invalid/api/webhooks/private/token")]),
        ),
        (
            DestinationConfig {
                token_env: Some("HOOK_TOKEN".into()),
                ..DestinationConfig::webhook("w", "webhook", "WEBHOOK_URL")
            },
            HashMap::from([("WEBHOOK_URL", "https://hooks.invalid/secret"), ("HOOK_TOKEN", "private")]),
        ),
    ];
    for (destination, env) in &cases {
        let lookup = |name: &str| env.get(name).map(|v| v.to_string());
        let mut d = deps(&lookup, &resolver, &connector);
        d.http = Some(&http);
        let outcome = send_destination(destination, &event(), &network(), &d);
        assert!(outcome.ok);
        assert_eq!(outcome.status, Some(204));
    }
    let seen = seen.borrow();
    assert_eq!(seen.iter().map(|s| s.3).collect::<Vec<_>>(), vec![5.0, 5.0, 5.0]);
    assert!(seen[0].2.contains(&("Authorization".into(), "Bearer secret-token".into())));
    assert!(seen[0].2.contains(&("Title".into(), "Dead Air incident".into())));
    let discord: Value = serde_json::from_slice(&seen[1].1).unwrap();
    assert_eq!(discord["allowed_mentions"], json!({"parse": []}));
    assert!(discord["content"].as_str().unwrap().contains("evt-123"));
    let webhook: Value = serde_json::from_slice(&seen[2].1).unwrap();
    assert_eq!(webhook["event_id"], "evt-123");
    assert_eq!(String::from_utf8_lossy(&seen[0].1), "Dead Air open: sample-channel audio_silence (evt-123)");
    let headers: String = seen[0].2.iter().map(|(k, v)| format!("{k}{v}")).collect();
    assert!(!headers.contains("https://") && !String::from_utf8_lossy(&seen[0].1).contains("https://"));
}

#[test]
fn missing_endpoint_or_token_fails_before_any_transport() {
    let http = |_u: &str, _b: &[u8], _h: &[(String, String)], _t: f64| -> dead_air::http::PostOutcome {
        panic!("no transport")
    };
    let resolver = Resolver::always(&["8.8.8.8"]);
    let connector = FakeConnector::new(Rc::default());
    let mut d = deps(&no_env, &resolver, &connector);
    d.http = Some(&http);
    let hook = DestinationConfig::webhook("w", "webhook", "UNSET_URL");
    assert_eq!(send_destination(&hook, &event(), &network(), &d).reason.as_deref(), Some("ENDPOINT_UNAVAILABLE"));
    let env = |name: &str| (name == "URL").then(|| "https://hooks.invalid/x".to_string());
    let mut d = deps(&env, &resolver, &connector);
    d.http = Some(&http);
    let tokened = DestinationConfig {
        token_env: Some("UNSET_TOKEN".into()),
        ..DestinationConfig::webhook("w", "webhook", "URL")
    };
    assert_eq!(send_destination(&tokened, &event(), &network(), &d).reason.as_deref(), Some("TOKEN_UNAVAILABLE"));
    let control = |_: &str| Some("https://hooks.invalid/x\r\nX: y".to_string());
    let mut d = deps(&control, &resolver, &connector);
    d.http = Some(&http);
    assert_eq!(send_destination(&hook, &event(), &network(), &d).reason.as_deref(), Some("ENDPOINT_UNAVAILABLE"));
}

#[test]
fn smtp_starttls_is_required_and_message_has_no_header_injection() {
    let log = Rc::new(SessionLog::default());
    let connector = FakeConnector::new(Rc::clone(&log));
    let resolver = Resolver::always(&["8.8.8.8"]);
    let outcome = send_destination(&smtp(587, "starttls"), &event(), &network(), &deps(&no_env, &resolver, &connector));
    assert!(outcome.ok, "{outcome:?}");
    assert!(log.tls_started.get());
    assert_eq!(log.ehlo.get(), 2, "EHLO again after STARTTLS");
    let message = log.message.borrow().clone().unwrap();
    assert!(message.contains("evt-123"));
    assert!(message.contains("\r\nSubject: Dead Air open: sample-channel audio_silence (evt-123)\r\n"));
    assert!(!log.logged_in.get());
    assert!(log.quit.get());
    assert_eq!(
        connector.calls.borrow()[0],
        ("plain".into(), vec!["8.8.8.8".parse().unwrap()]),
        "connects to the checked address"
    );
}

#[test]
fn smtp_credentials_come_from_named_variables_only() {
    let log = Rc::new(SessionLog::default());
    let connector = FakeConnector::new(Rc::clone(&log));
    let resolver = Resolver::always(&["8.8.8.8"]);
    let mut destination = smtp(465, "ssl");
    destination.username_env = Some("SMTP_USER".into());
    destination.password_env = Some("SMTP_PASSWORD".into());
    let outcome = send_destination(&destination, &event(), &network(), &deps(&no_env, &resolver, &connector));
    assert_eq!(outcome.reason.as_deref(), Some("SMTP_CREDENTIALS_MISSING"));
    let env = |name: &str| match name {
        "SMTP_USER" => Some("user".to_string()),
        "SMTP_PASSWORD" => Some("pass".to_string()),
        _ => None,
    };
    let outcome = send_destination(&destination, &event(), &network(), &deps(&env, &resolver, &connector));
    assert!(outcome.ok);
    assert!(log.logged_in.get());
}

#[test]
fn private_destination_requires_opt_in_exact_allowlist_and_tls() {
    let private_network = NetworkConfig {
        private_allowlist: vec!["10.0.0.8:465".into()],
        max_redirects: 3,
        thumbnail_max_bytes: 1_048_576,
    };
    let mut mail = smtp(465, "ssl");
    mail.host = Some("10.0.0.8".into());
    mail.allow_private = true;
    let resolver = Resolver::always(&["10.0.0.8"]);
    let connector = FakeConnector::new(Rc::default());
    assert!(send_destination(&mail, &event(), &private_network, &deps(&no_env, &resolver, &connector)).ok);
    let denied = DestinationConfig { allow_private: false, ..mail.clone() };
    assert_eq!(
        send_destination(&denied, &event(), &private_network, &deps(&no_env, &resolver, &connector)),
        SendOutcome::failed("SMTP_DELIVERY_FAILED")
    );
    let wrong_port = NetworkConfig { private_allowlist: vec!["10.0.0.8:587".into()], ..private_network.clone() };
    assert_eq!(
        send_destination(&mail, &event(), &wrong_port, &deps(&no_env, &resolver, &connector)),
        SendOutcome::failed("SMTP_DELIVERY_FAILED")
    );

    let webhook =
        DestinationConfig { allow_private: true, ..DestinationConfig::webhook("local-hook", "webhook", "URL") };
    let private_https = NetworkConfig { private_allowlist: vec!["10.0.0.8:443".into()], ..private_network };
    let env = |_: &str| Some("https://10.0.0.8/notify".to_string());
    let denied_hook = DestinationConfig { allow_private: false, ..webhook };
    let outcome = send_destination(&denied_hook, &event(), &private_https, &deps(&env, &resolver, &connector));
    assert_eq!(outcome, SendOutcome::failed("PRIVATE_NETWORK_DENIED"));
    let plain = |_: &str| Some("http://hooks.invalid/notify".to_string());
    let hook = DestinationConfig::webhook("hook", "webhook", "URL");
    assert_eq!(
        send_destination(&hook, &event(), &network(), &deps(&plain, &resolver, &connector)).reason.as_deref(),
        Some("TLS_REQUIRED")
    );
}

#[test]
fn smtp_uses_implicit_tls_and_never_falls_back_after_starttls_failure() {
    let log = Rc::new(SessionLog::default());
    let connector = FakeConnector::new(Rc::clone(&log));
    let resolver = Resolver::always(&["8.8.8.8"]);
    assert!(send_destination(&smtp(465, "ssl"), &event(), &network(), &deps(&no_env, &resolver, &connector)).ok);
    assert_eq!(connector.calls.borrow()[0].0, "ssl");
    let failing = Rc::new(SessionLog { fail_starttls: Cell::new(true), ..SessionLog::default() });
    let connector = FakeConnector::new(Rc::clone(&failing));
    let outcome = send_destination(&smtp(587, "starttls"), &event(), &network(), &deps(&no_env, &resolver, &connector));
    assert_eq!(outcome, SendOutcome::failed("SMTP_DELIVERY_FAILED"));
    assert_eq!(
        connector.calls.borrow().iter().map(|c| c.0.as_str()).collect::<Vec<_>>(),
        vec!["plain"],
        "no SSL fallback"
    );
    assert!(failing.message.borrow().is_none(), "nothing is sent without TLS");
}

#[test]
fn attempt_deadline_covers_smtp_stages_and_cleanup() {
    let clock = Rc::new(Cell::new(0.0));
    let log = Rc::new(SessionLog { clock: Some(Rc::clone(&clock)), ..SessionLog::default() });
    let connector = FakeConnector::new(Rc::clone(&log));
    let resolver = Resolver::always(&["8.8.8.8"]);
    let now = || clock.get();
    let d = SendDeps {
        env: &no_env,
        http: None,
        resolver: &resolver,
        smtp: &connector,
        deadline: Some(1.0),
        monotonic: &now,
    };
    assert_eq!(
        send_destination(&smtp(587, "starttls"), &event(), &network(), &d),
        SendOutcome::failed("DELIVERY_TIMEOUT")
    );
    assert!(log.closed.get());
    assert!(!log.quit.get(), "a spent deadline closes instead of sending QUIT");
    assert!((clock.get() - 1.1).abs() < 1e-9);

    let clock = Rc::new(Cell::new(0.0));
    let now = || clock.get();
    let slow = |_u: &str, _b: &[u8], _h: &[(String, String)], _t: f64| {
        clock.set(clock.get() + 1.1);
        (Some(204), None, None)
    };
    let env = |_: &str| Some("https://hooks.invalid/private".to_string());
    let d = SendDeps {
        env: &env,
        http: Some(&slow),
        resolver: &resolver,
        smtp: &connector,
        deadline: Some(1.0),
        monotonic: &now,
    };
    assert_eq!(
        send_destination(&DestinationConfig::webhook("hook", "webhook", "URL"), &event(), &network(), &d),
        SendOutcome::failed("DELIVERY_TIMEOUT")
    );
}

/// A fake worker driven by a shared fake clock.
struct FakeWorker {
    clock: Rc<Cell<f64>>,
    interrupt: bool,
    returncode: Rc<Cell<Option<i32>>>,
    terminated: Rc<Cell<bool>>,
    waits: Rc<Cell<u32>>,
    pipes_closed: Rc<Cell<bool>>,
    input: Rc<RefCell<Vec<u8>>>,
}

impl WorkerProcess for FakeWorker {
    fn communicate(&mut self, input: &[u8], timeout_s: f64) -> Communicated {
        *self.input.borrow_mut() = input.to_vec();
        if self.interrupt {
            return Communicated::Interrupted;
        }
        self.clock.set(self.clock.get() + timeout_s);
        Communicated::TimedOut
    }
    fn poll(&mut self) -> Option<i32> {
        self.returncode.get()
    }
    fn terminate(&mut self) {
        self.terminated.set(true);
        if !self.interrupt {
            self.returncode.set(Some(-15));
        }
    }
    fn kill(&mut self) {
        self.returncode.set(Some(-9));
    }
    fn wait(&mut self, _t: f64) -> Option<i32> {
        self.waits.set(self.waits.get() + 1);
        self.clock.set(self.clock.get() + 0.05);
        if self.interrupt {
            self.returncode.set(Some(-15));
        }
        self.returncode.get()
    }
    fn close_pipes(&mut self) {
        self.pipes_closed.set(true);
    }
}

struct FakeLauncher {
    worker: RefCell<Option<FakeWorker>>,
    env: RefCell<Vec<(String, String)>>,
}

impl WorkerLauncher for FakeLauncher {
    fn launch(&self, _argv: &[String], env: &[(String, String)]) -> std::io::Result<Box<dyn WorkerProcess>> {
        *self.env.borrow_mut() = env.to_vec();
        Ok(Box::new(self.worker.borrow_mut().take().expect("one launch")))
    }
    fn argv(&self) -> Vec<String> {
        vec!["fake-worker".into()]
    }
}

fn fake_worker(clock: &Rc<Cell<f64>>, interrupt: bool) -> FakeWorker {
    FakeWorker {
        clock: Rc::clone(clock),
        interrupt,
        returncode: Rc::default(),
        terminated: Rc::default(),
        waits: Rc::default(),
        pipes_closed: Rc::default(),
        input: Rc::default(),
    }
}

#[test]
fn owned_sender_process_is_cancelled_inside_total_deadline() {
    let clock = Rc::new(Cell::new(1.0));
    let worker = fake_worker(&clock, false);
    let (terminated, returncode, closed, input) = (
        Rc::clone(&worker.terminated),
        Rc::clone(&worker.returncode),
        Rc::clone(&worker.pipes_closed),
        Rc::clone(&worker.input),
    );
    let launcher = FakeLauncher { worker: RefCell::new(Some(worker)), env: RefCell::default() };
    let destination =
        DestinationConfig { token_env: Some("TOKEN".into()), ..DestinationConfig::webhook("hook", "webhook", "URL") };
    let env = |name: &str| match name {
        "URL" => Some("https://hooks.invalid/private".to_string()),
        "TOKEN" => Some("private-token".to_string()),
        "UNRELATED_SECRET" => Some("do-not-copy".to_string()),
        _ => None,
    };
    let now = || clock.get();
    let outcome = send_destination_bounded(&destination, &event(), &network(), 5.0, &env, &now, &launcher);
    assert_eq!(outcome, SendOutcome::failed("DELIVERY_TIMEOUT"));
    assert!(terminated.get());
    assert!(returncode.get().is_some());
    assert!(closed.get());
    assert!(clock.get() <= 6.0);
    let child_env = launcher.env.borrow();
    assert!(child_env.contains(&("TOKEN".into(), "private-token".into())));
    assert!(!child_env.iter().any(|(k, _)| k == "UNRELATED_SECRET"));
    let input = String::from_utf8_lossy(&input.borrow()).into_owned();
    assert!(!input.contains("private-token"));
    assert!(!input.contains("https://hooks.invalid/private"));
    assert!(input.contains("\"endpoint_env\":\"URL\""));
}

#[test]
fn owned_sender_child_is_cleaned_before_interruption_propagates() {
    let clock = Rc::new(Cell::new(1.0));
    let worker = fake_worker(&clock, true);
    let (terminated, returncode, closed, waits) = (
        Rc::clone(&worker.terminated),
        Rc::clone(&worker.returncode),
        Rc::clone(&worker.pipes_closed),
        Rc::clone(&worker.waits),
    );
    let launcher = FakeLauncher { worker: RefCell::new(Some(worker)), env: RefCell::default() };
    let env = |_: &str| Some("https://hooks.invalid/private".to_string());
    let now = || clock.get();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        send_destination_bounded(
            &DestinationConfig::webhook("hook", "webhook", "URL"),
            &event(),
            &network(),
            5.0,
            &env,
            &now,
            &launcher,
        )
    }));
    let payload = outcome.unwrap_err();
    assert!(payload.is::<dead_air::interrupt::Interrupted>());
    assert!(terminated.get());
    assert_eq!(waits.get(), 1);
    assert!(returncode.get().is_some());
    assert!(closed.get());
}

#[test]
fn owned_sender_worker_returns_before_network_when_endpoint_is_unset() {
    let launcher = ChildLauncher { exe: binary(), extra_env: vec![] };
    let outcome = send_destination_bounded(
        &DestinationConfig::webhook("hook", "webhook", "MISSING_URL"),
        &event(),
        &network(),
        5.0,
        &no_env,
        &now_mono,
        &launcher,
    );
    assert_eq!(outcome, SendOutcome::failed("ENDPOINT_UNAVAILABLE"));
    let too_short = send_destination_bounded(
        &DestinationConfig::webhook("h", "webhook", "U"),
        &event(),
        &network(),
        1.0,
        &no_env,
        &now_mono,
        &launcher,
    );
    assert_eq!(too_short, SendOutcome::failed("DELIVERY_TIMEOUT"));
}

fn store_with_incident(temp: &Temp, destinations: Vec<DestinationConfig>) -> (IncidentStore, AppConfig) {
    let mut config = incident_config(1, 0.0, 2, 180.0, 600.0);
    config.destinations = destinations;
    let mut store = IncidentStore::open(temp.join("state.db"), 1).unwrap();
    store.record_cycle(&config, &[bad_silence()], 1000.0, 0.0).unwrap();
    (store, config)
}

fn dispatch_with(
    store: &mut IncidentStore,
    destinations: &[DestinationConfig],
    now_wall: f64,
    sender: &mut dyn FnMut(&DestinationConfig, &Value, &NetworkConfig) -> SendOutcome,
) {
    let options = DispatchOptions { now_wall: Some(now_wall), sender: Some(sender), ..DispatchOptions::default() };
    dispatch_due(store, destinations, &network(), options).unwrap();
}

#[test]
fn attempts_are_finite_destinations_are_isolated_and_success_survives_restart() {
    let destinations = vec![
        DestinationConfig::webhook("a-fails", "webhook", "A"),
        DestinationConfig::webhook("b-succeeds", "ntfy", "B"),
    ];
    let temp = Temp::new();
    let (mut store, mut config) = store_with_incident(&temp, destinations.clone());
    let calls: RefCell<Vec<String>> = RefCell::default();
    let mut sender = |d: &DestinationConfig, _e: &Value, _n: &NetworkConfig| {
        calls.borrow_mut().push(d.id.clone());
        if d.id == "b-succeeds" {
            SendOutcome::sent(Some(204))
        } else {
            SendOutcome { status: Some(503), ..SendOutcome::failed("HTTP_STATUS") }
        }
    };
    dispatch_with(&mut store, &destinations, 1000.0, &mut sender);
    assert_eq!(calls.borrow().len(), 2);
    dispatch_with(&mut store, &destinations, 1029.0, &mut sender);
    assert_eq!(calls.borrow().iter().filter(|c| *c == "a-fails").count(), 1, "the 30 s retry is not yet due");
    dispatch_with(&mut store, &destinations, 1030.0, &mut sender);
    dispatch_with(&mut store, &destinations, 1120.0, &mut sender);
    let rows: HashMap<String, (String, i64)> =
        store.deliveries().unwrap().into_iter().map(|r| (r.1, (r.2, r.3))).collect();
    assert_eq!(rows["a-fails"], ("failed".to_string(), 3));
    assert_eq!(rows["b-succeeds"], ("sent".to_string(), 1));
    drop(store);
    let mut reopened = IncidentStore::open(temp.join("state.db"), 1).unwrap();
    let before = calls.borrow().iter().filter(|c| *c == "b-succeeds").count();
    dispatch_with(&mut reopened, &destinations, 100_000.0, &mut sender);
    assert_eq!(calls.borrow().iter().filter(|c| *c == "b-succeeds").count(), before);
    config.streams[0].recovery_samples = 2;
    let good = healthy("2026-10-23T00:01:00Z");
    reopened.record_cycle(&config, std::slice::from_ref(&good), 1200.0, 1.0).unwrap();
    reopened.record_cycle(&config, std::slice::from_ref(&good), 1201.0, 2.0).unwrap();
    reopened.record_cycle(&config, &[good], 90_000.0, 3.0).unwrap(); // retention cannot erase a failed delivery
    assert!(reopened.deliveries().unwrap().iter().any(|r| r.2 == "failed"));
}

#[test]
fn removed_destination_does_not_consume_due_quota_and_can_be_reintroduced() {
    let mut config = incident_config(1, 0.0, 1, 180.0, 600.0);
    let template = config.streams[0].clone();
    config.streams = (0..8).map(|i| StreamConfig { id: format!("channel-{i}"), ..template.clone() }).collect();
    let old = DestinationConfig::webhook("old-target", "webhook", "OLD_URL");
    let replacement = DestinationConfig::webhook("replacement", "webhook", "NEW_URL");
    let mut old_config = config.clone();
    old_config.destinations = vec![old.clone()];
    let mut new_config = config.clone();
    new_config.destinations = vec![replacement.clone()];
    let bad: Vec<Observation> = config
        .streams
        .iter()
        .enumerate()
        .map(|(i, s)| Observation {
            stream_id: s.id.clone(),
            observed_at: format!("2026-10-23T00:00:0{i}Z"),
            ..bad_silence()
        })
        .collect();
    let temp = Temp::new();
    let mut store = IncidentStore::open(temp.join("state.db"), 90).unwrap();
    store.record_cycle(&old_config, &bad, 100.0, 0.0).unwrap();
    assert_eq!(store.due_deliveries(100.0, 8, &["old-target".to_string()]).unwrap().len(), 8);
    let good = Observation { stream_id: "channel-0".into(), ..healthy("2026-10-23T00:00:10Z") };
    store.record_cycle(&new_config, &[good], 101.0, 1.0).unwrap();
    store.record_cycle(&new_config, &[bad[0].clone()], 102.0, 2.0).unwrap();
    let calls: RefCell<Vec<String>> = RefCell::default();
    let mut sender = |d: &DestinationConfig, _e: &Value, _n: &NetworkConfig| {
        calls.borrow_mut().push(d.id.clone());
        SendOutcome::sent(Some(204))
    };
    dispatch_with(&mut store, std::slice::from_ref(&replacement), 102.0, &mut sender);
    assert_eq!(*calls.borrow(), vec!["replacement", "replacement"]);
    let old_rows: Vec<_> = store.deliveries().unwrap().into_iter().filter(|r| r.1 == "old-target").collect();
    assert_eq!(old_rows.len(), 8);
    assert!(old_rows.iter().all(|r| r.2 == "pending" && r.3 == 0));
    calls.borrow_mut().clear();
    dispatch_with(&mut store, std::slice::from_ref(&old), 102.0, &mut sender);
    assert_eq!(calls.borrow().len(), 8);
    assert!(store.deliveries().unwrap().iter().filter(|r| r.1 == "old-target").all(|r| r.2 == "sent" && r.3 == 1));
}

#[test]
fn partial_smtp_refusal_is_retryable_redacted_at_least_once() {
    let mut destination = smtp(465, "ssl");
    destination.recipients = vec!["accepted@example.invalid".into(), "rejected@example.invalid".into()];
    let temp = Temp::new();
    let (mut store, _config) = store_with_incident(&temp, vec![destination.clone()]);
    let partial =
        Rc::new(SessionLog { refused: RefCell::new(vec!["rejected@example.invalid".into()]), ..SessionLog::default() });
    let accepted = Rc::new(SessionLog::default());
    let resolver = Resolver::always(&["8.8.8.8"]);
    let mut sender = |d: &DestinationConfig, e: &Value, n: &NetworkConfig| {
        let log = if partial.message.borrow().is_none() { Rc::clone(&partial) } else { Rc::clone(&accepted) };
        let connector = FakeConnector::new(log);
        send_destination(d, e, n, &deps(&no_env, &resolver, &connector))
    };
    dispatch_with(&mut store, std::slice::from_ref(&destination), 1000.0, &mut sender);
    let first = store.deliveries().unwrap()[0].clone();
    assert_eq!((first.2.as_str(), first.3, first.4.as_deref()), ("pending", 1, Some("SMTP_PARTIAL_FAILURE")));
    dispatch_with(&mut store, std::slice::from_ref(&destination), 1029.0, &mut sender);
    assert!(accepted.message.borrow().is_none());
    dispatch_with(&mut store, std::slice::from_ref(&destination), 1030.0, &mut sender);
    let last = store.deliveries().unwrap()[0].clone();
    assert_eq!((last.2.as_str(), last.3, last.4.clone()), ("sent", 2, None));
    assert!(accepted.message.borrow().as_ref().unwrap().contains(&last.0), "a retry keeps the stable event id");
}

#[test]
fn dispatch_stops_after_sender_cleanup_failure() {
    let destinations = vec![
        DestinationConfig::webhook("first", "webhook", "FIRST_URL"),
        DestinationConfig::webhook("second", "webhook", "SECOND_URL"),
    ];
    let temp = Temp::new();
    let (mut store, _config) = store_with_incident(&temp, destinations.clone());
    let calls = Cell::new(0);
    let mut sender = |_d: &DestinationConfig, _e: &Value, _n: &NetworkConfig| {
        calls.set(calls.get() + 1);
        SendOutcome::failed("DELIVERY_CLEANUP_FAILED")
    };
    let options = DispatchOptions { now_wall: Some(1000.0), sender: Some(&mut sender), ..DispatchOptions::default() };
    assert_eq!(dispatch_due(&mut store, &destinations, &network(), options).unwrap_err().0, "DELIVERY_CLEANUP_FAILED");
    assert_eq!(calls.get(), 1);
    let rows = store.deliveries().unwrap();
    assert_eq!(rows.iter().filter(|r| r.3 == 1).count(), 1);
    assert_eq!(rows.iter().filter(|r| r.3 == 0).count(), 1);
    let failed = rows.iter().find(|r| r.4.as_deref() == Some("DELIVERY_CLEANUP_FAILED")).unwrap();
    assert_eq!((failed.2.as_str(), failed.3), ("pending", 1));
}

#[test]
fn dispatch_monotonic_budget_stops_before_next_due_row() {
    struct Ledger {
        selected: Option<(f64, usize, Vec<String>)>,
        recorded: usize,
    }
    impl DeliveryLedger for Ledger {
        fn due_deliveries(
            &mut self,
            now_wall: f64,
            limit: usize,
            ids: &[String],
        ) -> Result<Vec<DueDelivery>, PersistenceError> {
            self.selected = Some((now_wall, limit, ids.to_vec()));
            Ok((0..3)
                .map(|i| DueDelivery {
                    event_id: format!("event-{i}"),
                    destination_id: "active".into(),
                    attempts: 0,
                    created_wall: now_wall,
                    payload_json: event().to_string(),
                })
                .collect())
        }
        fn record_delivery(
            &mut self,
            _r: &DueDelivery,
            _s: bool,
            _e: Option<&str>,
            _h: Option<u16>,
            _n: f64,
            _a: Option<f64>,
        ) -> Result<(), PersistenceError> {
            self.recorded += 1;
            Ok(())
        }
    }
    let clock = Cell::new(0.0);
    let now = || clock.get();
    let calls = Cell::new(0);
    let mut sender = |_d: &DestinationConfig, _e: &Value, _n: &NetworkConfig| {
        calls.set(calls.get() + 1);
        clock.set(clock.get() + 4.0);
        SendOutcome::failed("HTTP_REQUEST_FAILED")
    };
    let mut ledger = Ledger { selected: None, recorded: 0 };
    let options = DispatchOptions {
        now_wall: Some(100.0),
        sender: Some(&mut sender),
        monotonic: &now,
        dispatch_budget_s: 5.0,
        ..DispatchOptions::default()
    };
    dispatch_due(&mut ledger, &[DestinationConfig::webhook("active", "webhook", "URL")], &network(), options).unwrap();
    assert_eq!(calls.get(), 1);
    assert_eq!(ledger.recorded, 1);
    assert_eq!(ledger.selected, Some((100.0, 8, vec!["active".to_string()])));
}

#[test]
fn retry_after_is_honored_with_a_120_second_cap() {
    let destination = DestinationConfig::webhook("retry", "webhook", "URL");
    let mut config = incident_config(1, 0.0, 2, 180.0, 600.0);
    config.destinations = vec![destination.clone()];
    let temp = Temp::new();
    let mut store = IncidentStore::open(temp.join("state.db"), 90).unwrap();
    store.record_cycle(&config, &[bad_silence()], 500.0, 0.0).unwrap();
    let responses = RefCell::new(vec![(429, 999.0), (429, 90.0)]);
    let mut sender = |_d: &DestinationConfig, _e: &Value, _n: &NetworkConfig| {
        let (status, retry) = responses.borrow_mut().pop().unwrap();
        SendOutcome { ok: false, reason: Some("HTTP_STATUS".into()), status: Some(status), retry_after: Some(retry) }
    };
    let ids = vec!["retry".to_string()];
    dispatch_with(&mut store, std::slice::from_ref(&destination), 500.0, &mut sender);
    assert!(store.due_deliveries(589.0, 8, &ids).unwrap().is_empty());
    assert_eq!(store.due_deliveries(590.0, 8, &ids).unwrap().len(), 1);
    dispatch_with(&mut store, std::slice::from_ref(&destination), 590.0, &mut sender);
    assert!(store.due_deliveries(709.0, 8, &ids).unwrap().is_empty());
    assert_eq!(store.due_deliveries(710.0, 8, &ids).unwrap().len(), 1);
}

#[test]
fn worker_protocol_round_trips_outcomes() {
    let outcome =
        SendOutcome { ok: false, reason: Some("HTTP_STATUS".into()), status: Some(503), retry_after: Some(30.0) };
    assert_eq!(SendOutcome::from_value(&outcome.to_value()), Some(outcome));
    assert_eq!(notification_summary(&json!({"event_id": "x"})), None);
    let env = worker_environment(
        &DestinationConfig { token_env: Some("TOKEN".into()), ..DestinationConfig::webhook("h", "webhook", "URL") },
        &|name| (name != "OTHER").then(|| format!("value-of-{name}")),
    );
    assert!(env.contains(&("URL".into(), "value-of-URL".into())));
    assert!(env.contains(&("TOKEN".into(), "value-of-TOKEN".into())));
    assert!(!env.iter().any(|(k, _)| k == "HTTPS_PROXY" || k == "HOME"));
    let _ = Arc::new(Mutex::new(()));
}
