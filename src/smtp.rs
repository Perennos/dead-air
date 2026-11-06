//! A minimal SMTP submission client: implicit TLS or mandatory STARTTLS, never plaintext.

use std::io;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use crate::http::Connection;
use crate::random::base64;

#[derive(Debug, Clone, PartialEq)]
pub struct SmtpError(pub String);

impl From<io::Error> for SmtpError {
    fn from(error: io::Error) -> Self {
        SmtpError(format!("io: {:?}", error.kind()))
    }
}

/// One SMTP conversation. Fakes implement this in tests.
pub trait SmtpSession {
    fn ehlo(&mut self) -> Result<(), SmtpError>;
    fn starttls(&mut self) -> Result<(), SmtpError>;
    fn login(&mut self, username: &str, password: &str) -> Result<(), SmtpError>;
    /// Returns the refused recipients; an error when every recipient was refused.
    fn send_message(&mut self, sender: &str, recipients: &[String], message: &[u8]) -> Result<Vec<String>, SmtpError>;
    fn quit(&mut self) -> Result<(), SmtpError>;
    fn close(&mut self);
}

/// Opens sessions to policy-checked addresses.
pub trait SmtpConnector {
    /// Implicit TLS (`tls = "ssl"`), certificate and hostname verified.
    fn connect_ssl(
        &self,
        host: &str,
        port: u16,
        addresses: &[IpAddr],
        timeout_s: f64,
    ) -> Result<Box<dyn SmtpSession>, SmtpError>;
    /// Plain TCP that must be upgraded with STARTTLS before credentials or mail.
    fn connect_plain(
        &self,
        host: &str,
        port: u16,
        addresses: &[IpAddr],
        timeout_s: f64,
    ) -> Result<Box<dyn SmtpSession>, SmtpError>;
}

pub struct RealConnector;

impl SmtpConnector for RealConnector {
    fn connect_ssl(
        &self,
        host: &str,
        port: u16,
        addresses: &[IpAddr],
        timeout_s: f64,
    ) -> Result<Box<dyn SmtpSession>, SmtpError> {
        let deadline = Instant::now() + Duration::from_secs_f64(timeout_s.clamp(0.001, 600.0));
        let connection = Connection::open(true, host, addresses, port, deadline)?;
        RealSession::start(connection, host, true)
    }

    fn connect_plain(
        &self,
        host: &str,
        port: u16,
        addresses: &[IpAddr],
        timeout_s: f64,
    ) -> Result<Box<dyn SmtpSession>, SmtpError> {
        let deadline = Instant::now() + Duration::from_secs_f64(timeout_s.clamp(0.001, 600.0));
        let connection = Connection::open(false, host, addresses, port, deadline)?;
        RealSession::start(connection, host, false)
    }
}

struct RealSession {
    connection: Option<Connection>,
    pending: Vec<u8>,
    host: String,
    tls: bool,
    features: Vec<String>,
    greeted: bool,
}

impl RealSession {
    fn start(connection: Connection, host: &str, tls: bool) -> Result<Box<dyn SmtpSession>, SmtpError> {
        let mut session = RealSession {
            connection: Some(connection),
            pending: Vec::new(),
            host: host.to_string(),
            tls,
            features: Vec::new(),
            greeted: false,
        };
        let (code, _) = session.reply()?;
        if code != 220 {
            return Err(SmtpError(format!("greeting {code}")));
        }
        Ok(Box::new(session))
    }

    fn conn(&mut self) -> Result<&mut Connection, SmtpError> {
        self.connection.as_mut().ok_or_else(|| SmtpError("closed".into()))
    }

    fn reply(&mut self) -> Result<(u16, Vec<String>), SmtpError> {
        let mut lines = Vec::new();
        loop {
            let mut pending = std::mem::take(&mut self.pending);
            let line = self.conn()?.read_line(&mut pending, 4096);
            self.pending = pending;
            let line = line?;
            if line.len() < 3 || lines.len() > 100 {
                return Err(SmtpError("malformed reply".into()));
            }
            let code: u16 = line[..3].parse().map_err(|_| SmtpError("malformed reply".into()))?;
            let more = line.as_bytes().get(3) == Some(&b'-');
            lines.push(line.get(4..).unwrap_or("").to_string());
            if !more {
                return Ok((code, lines));
            }
        }
    }

    fn command(&mut self, line: &str) -> Result<(u16, Vec<String>), SmtpError> {
        if line.contains(['\r', '\n']) {
            return Err(SmtpError("header control".into()));
        }
        self.conn()?.write_all_deadline(format!("{line}\r\n").as_bytes())?;
        self.reply()
    }

    fn expect(&mut self, line: &str, codes: &[u16]) -> Result<Vec<String>, SmtpError> {
        let (code, lines) = self.command(line)?;
        if codes.contains(&code) { Ok(lines) } else { Err(SmtpError(format!("{code}"))) }
    }

    fn helo_name(&mut self) -> String {
        match self.connection.as_ref().and_then(Connection::local_ip) {
            Some(IpAddr::V4(v4)) => format!("[{v4}]"),
            Some(IpAddr::V6(v6)) => format!("[IPv6:{v6}]"),
            None => "[127.0.0.1]".into(),
        }
    }

    fn ensure_ehlo(&mut self) -> Result<(), SmtpError> {
        if !self.greeted { self.ehlo() } else { Ok(()) }
    }

    fn has_feature(&self, name: &str) -> Option<String> {
        self.features
            .iter()
            .find(|f| f.split_whitespace().next().is_some_and(|k| k.eq_ignore_ascii_case(name)))
            .cloned()
    }
}

impl SmtpSession for RealSession {
    fn ehlo(&mut self) -> Result<(), SmtpError> {
        let name = self.helo_name();
        let lines = self.expect(&format!("EHLO {name}"), &[250])?;
        self.features = lines.into_iter().skip(1).collect();
        self.greeted = true;
        Ok(())
    }

    fn starttls(&mut self) -> Result<(), SmtpError> {
        self.ensure_ehlo()?;
        if self.has_feature("STARTTLS").is_none() {
            return Err(SmtpError("STARTTLS not offered".into()));
        }
        self.expect("STARTTLS", &[220])?;
        let connection = self.connection.take().ok_or_else(|| SmtpError("closed".into()))?;
        let host = self.host.clone();
        self.connection = Some(connection.start_tls(&host)?);
        self.pending.clear();
        self.tls = true;
        self.greeted = false;
        self.features.clear();
        Ok(())
    }

    fn login(&mut self, username: &str, password: &str) -> Result<(), SmtpError> {
        if !self.tls {
            return Err(SmtpError("refusing credentials without TLS".into()));
        }
        self.ensure_ehlo()?;
        let mechanisms =
            self.has_feature("AUTH").ok_or_else(|| SmtpError("AUTH not offered".into()))?.to_ascii_uppercase();
        if mechanisms.split_whitespace().any(|m| m == "PLAIN") {
            let token = base64(format!("\0{username}\0{password}").as_bytes(), false);
            self.expect(&format!("AUTH PLAIN {token}"), &[235])?;
            return Ok(());
        }
        if mechanisms.split_whitespace().any(|m| m == "LOGIN") {
            self.expect("AUTH LOGIN", &[334])?;
            self.expect(&base64(username.as_bytes(), false), &[334])?;
            self.expect(&base64(password.as_bytes(), false), &[235])?;
            return Ok(());
        }
        Err(SmtpError("no supported AUTH mechanism".into()))
    }

    fn send_message(&mut self, sender: &str, recipients: &[String], message: &[u8]) -> Result<Vec<String>, SmtpError> {
        if !self.tls {
            return Err(SmtpError("refusing mail without TLS".into()));
        }
        self.ensure_ehlo()?;
        self.expect(&format!("MAIL FROM:<{sender}>"), &[250])?;
        let mut refused = Vec::new();
        for recipient in recipients {
            let (code, _) = self.command(&format!("RCPT TO:<{recipient}>"))?;
            if code != 250 && code != 251 {
                refused.push(recipient.clone());
            }
        }
        if refused.len() == recipients.len() {
            let _ = self.command("RSET");
            return Err(SmtpError("all recipients refused".into()));
        }
        self.expect("DATA", &[354])?;
        let mut data = Vec::with_capacity(message.len() + 64);
        for line in message.split(|b| *b == b'\n') {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if line.first() == Some(&b'.') {
                data.push(b'.');
            }
            data.extend_from_slice(line);
            data.extend_from_slice(b"\r\n");
        }
        data.extend_from_slice(b".\r\n");
        self.conn()?.write_all_deadline(&data)?;
        let (code, _) = self.reply()?;
        if code != 250 {
            return Err(SmtpError(format!("{code}")));
        }
        Ok(refused)
    }

    fn quit(&mut self) -> Result<(), SmtpError> {
        let result = self.command("QUIT").map(|_| ());
        self.close();
        result
    }

    fn close(&mut self) {
        if let Some(connection) = self.connection.take() {
            connection.close();
        }
    }
}

impl Drop for RealSession {
    fn drop(&mut self) {
        self.close();
    }
}
