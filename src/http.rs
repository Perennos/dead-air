//! A small bounded HTTP/1.1 client that connects only to already checked addresses.
//!
//! No proxy, no automatic redirects, no compression, no connection reuse. Every socket
//! operation respects one operation deadline, reads are sliced so Ctrl+C is honoured, and
//! HTTPS always verifies the certificate chain and the URL's hostname.

use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use rustls::{ClientConnection, StreamOwned};

use crate::constants::VERSION;
use crate::interrupt;
use crate::model::{HttpResult, NetworkConfig};
use crate::netpolicy::{Resolver, static_url, validate_network_target};
use crate::url::{urljoin, urlsplit};

const MAX_HEADER_BYTES: usize = 64 * 1024;
const READ_SLICE: Duration = Duration::from_millis(100);

enum Stream {
    Plain(TcpStream),
    Tls(Box<StreamOwned<ClientConnection, TcpStream>>),
}

impl Stream {
    fn socket(&self) -> &TcpStream {
        match self {
            Stream::Plain(s) => s,
            Stream::Tls(s) => &s.sock,
        }
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.read(buf),
            Stream::Tls(s) => match s.read(buf) {
                // Like Python's suppress_ragged_eofs: a close without close_notify is EOF.
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(0),
                other => other,
            },
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.write(buf),
            Stream::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Stream::Plain(s) => s.flush(),
            Stream::Tls(s) => s.flush(),
        }
    }
}

fn retryable(error: &io::Error) -> bool {
    matches!(error.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted)
}

/// A connection with one deadline for every operation.
pub struct Connection {
    stream: Stream,
    deadline: Instant,
}

impl Connection {
    /// Connect to the first reachable checked address, then (for TLS) verify `host`.
    pub fn open(tls: bool, host: &str, addresses: &[IpAddr], port: u16, deadline: Instant) -> io::Result<Self> {
        let mut last = io::Error::new(io::ErrorKind::NotConnected, "HTTP_CONNECT_FAILED");
        let mut socket = None;
        for address in addresses {
            interrupt::check();
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            match TcpStream::connect_timeout(&SocketAddr::new(*address, port), left) {
                Ok(stream) => {
                    socket = Some(stream);
                    break;
                }
                Err(error) => last = error,
            }
        }
        let socket = socket.ok_or(last)?;
        let _ = socket.set_nodelay(true);
        let stream = if tls {
            let name = crate::tls::server_name(host)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "TLS server name invalid"))?;
            let connection = ClientConnection::new(crate::tls::client_config(), name).map_err(io::Error::other)?;
            Stream::Tls(Box::new(StreamOwned::new(connection, socket)))
        } else {
            Stream::Plain(socket)
        };
        let mut connection = Connection { stream, deadline };
        if tls {
            connection.handshake()?;
        }
        Ok(connection)
    }

    /// Wrap an existing plain socket (SMTP before STARTTLS).
    pub fn plain(socket: TcpStream, deadline: Instant) -> Self {
        Connection { stream: Stream::Plain(socket), deadline }
    }

    pub fn set_deadline(&mut self, deadline: Instant) {
        self.deadline = deadline;
    }

    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    pub fn local_ip(&self) -> Option<IpAddr> {
        self.stream.socket().local_addr().ok().map(|a| a.ip())
    }

    fn arm(&self) -> io::Result<()> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "deadline"));
        }
        let slice = left.min(READ_SLICE).max(Duration::from_millis(1));
        self.stream.socket().set_read_timeout(Some(slice))?;
        self.stream.socket().set_write_timeout(Some(left.max(Duration::from_millis(1))))?;
        Ok(())
    }

    fn handshake(&mut self) -> io::Result<()> {
        if let Stream::Tls(tls) = &mut self.stream {
            while tls.conn.is_handshaking() {
                let left = self.deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "deadline"));
                }
                tls.sock.set_read_timeout(Some(left.min(READ_SLICE)))?;
                tls.sock.set_write_timeout(Some(left))?;
                match tls.conn.complete_io(&mut tls.sock) {
                    Ok(_) => {}
                    Err(error) if retryable(&error) => interrupt::check(),
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(())
    }

    /// Upgrade a plain connection to verified TLS (SMTP STARTTLS).
    pub fn start_tls(self, host: &str) -> io::Result<Self> {
        let Stream::Plain(socket) = self.stream else {
            return Err(io::Error::other("already TLS"));
        };
        let name = crate::tls::server_name(host)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "TLS server name invalid"))?;
        let connection = ClientConnection::new(crate::tls::client_config(), name).map_err(io::Error::other)?;
        let mut upgraded =
            Connection { stream: Stream::Tls(Box::new(StreamOwned::new(connection, socket))), deadline: self.deadline };
        upgraded.handshake()?;
        Ok(upgraded)
    }

    /// Read some bytes before the deadline. `Ok(0)` is end of stream.
    pub fn read_some(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            self.arm()?;
            match self.stream.read(buf) {
                Ok(n) => return Ok(n),
                Err(error) if retryable(&error) => interrupt::check(),
                Err(error) => return Err(error),
            }
        }
    }

    pub fn write_all_deadline(&mut self, mut data: &[u8]) -> io::Result<()> {
        while !data.is_empty() {
            self.arm()?;
            match self.stream.write(data) {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "closed")),
                Ok(n) => data = &data[n..],
                Err(error) if retryable(&error) => interrupt::check(),
                Err(error) => return Err(error),
            }
        }
        loop {
            self.arm()?;
            match self.stream.flush() {
                Ok(()) => return Ok(()),
                Err(error) if retryable(&error) => interrupt::check(),
                Err(error) => return Err(error),
            }
        }
    }

    /// Read one CRLF-terminated line (SMTP replies), bounded.
    pub fn read_line(&mut self, pending: &mut Vec<u8>, max: usize) -> io::Result<String> {
        loop {
            if let Some(index) = pending.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = pending.drain(..=index).collect();
                return Ok(String::from_utf8_lossy(&line).trim_end_matches(['\r', '\n']).to_string());
            }
            if pending.len() > max {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
            }
            let mut chunk = [0u8; 4096];
            let n = self.read_some(&mut chunk)?;
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "closed"));
            }
            pending.extend_from_slice(&chunk[..n]);
        }
    }

    /// Best-effort TLS close_notify.
    pub fn close(mut self) {
        if let Stream::Tls(tls) = &mut self.stream {
            tls.conn.send_close_notify();
            let _ = tls.sock.set_write_timeout(Some(Duration::from_millis(200)));
            let _ = tls.conn.write_tls(&mut tls.sock);
        }
        let _ = self.stream.socket().shutdown(std::net::Shutdown::Both);
    }
}

/// A parsed response. `read_error` and `timed_out` describe the body phase only.
#[derive(Debug, Default)]
pub struct RawResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub truncated: bool,
    pub timed_out: bool,
    pub read_error: bool,
}

impl RawResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    /// Python `Message.get_content_type()`: lowercased main type, `text/plain` by default.
    pub fn content_type(&self) -> String {
        let Some(value) = self.header("content-type") else {
            return "text/plain".into();
        };
        let main = value.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
        if main.matches('/').count() == 1 { main } else { "text/plain".into() }
    }
}

/// Failure before any response arrived. `sent` says whether the request reached the socket.
#[derive(Debug)]
pub struct ExchangeError {
    pub sent: bool,
    pub timed_out: bool,
    pub refused: bool,
}

fn classify(error: &io::Error, sent: bool) -> ExchangeError {
    ExchangeError {
        sent,
        timed_out: matches!(error.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock),
        refused: error.kind() == io::ErrorKind::ConnectionRefused,
    }
}

pub struct Request<'a> {
    pub method: &'a str,
    pub tls: bool,
    pub host: &'a str,
    pub port: u16,
    pub addresses: &'a [IpAddr],
    pub host_header: String,
    pub selector: String,
    pub headers: Vec<(String, String)>,
    pub body: &'a [u8],
    pub max_body: usize,
    pub read_body: bool,
}

/// The first blank line ends the head, whether the server uses CRLF or bare LF.
fn find_header_end(data: &[u8]) -> Option<(usize, usize)> {
    let crlf = data.windows(4).position(|w| w == b"\r\n\r\n").map(|i| (i, i + 4));
    let lf = data.windows(2).position(|w| w == b"\n\n").map(|i| (i, i + 2));
    match (crlf, lf) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (a, b) => a.or(b),
    }
}

/// Send one request and read one response before `deadline`.
pub fn exchange(request: &Request<'_>, deadline: Instant) -> Result<RawResponse, ExchangeError> {
    if request.selector.bytes().any(|b| b <= 0x20 || b >= 0x7f) || !request.host_header.is_ascii() {
        return Err(ExchangeError { sent: false, timed_out: false, refused: false });
    }
    let mut connection = Connection::open(request.tls, request.host, request.addresses, request.port, deadline)
        .map_err(|e| classify(&e, false))?;
    let mut head = format!("{} {} HTTP/1.1\r\nHost: {}\r\n", request.method, request.selector, request.host_header);
    for (name, value) in &request.headers {
        if value.contains(['\r', '\n']) || name.contains(['\r', '\n', ':']) {
            return Err(ExchangeError { sent: false, timed_out: false, refused: false });
        }
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if !request.body.is_empty() || request.method == "POST" {
        head.push_str(&format!("Content-Length: {}\r\n", request.body.len()));
    }
    head.push_str("Connection: close\r\n\r\n");
    let mut payload = head.into_bytes();
    payload.extend_from_slice(request.body);
    connection.write_all_deadline(&payload).map_err(|e| classify(&e, true))?;

    let mut data = Vec::new();
    let mut chunk = [0u8; 8192];
    let (status, headers, mut rest) = loop {
        let (end, body_start) = loop {
            if let Some(found) = find_header_end(&data) {
                break found;
            }
            if data.len() > MAX_HEADER_BYTES {
                return Err(ExchangeError { sent: true, timed_out: false, refused: false });
            }
            let n = connection.read_some(&mut chunk).map_err(|e| classify(&e, true))?;
            if n == 0 {
                return Err(ExchangeError { sent: true, timed_out: false, refused: false });
            }
            data.extend_from_slice(&chunk[..n]);
        };
        let text = String::from_utf8_lossy(&data[..end]).into_owned();
        let mut lines = text.split('\n').map(|l| l.trim_end_matches('\r'));
        let status_line = lines.next().unwrap_or("");
        let mut parts = status_line.splitn(3, ' ');
        let version = parts.next().unwrap_or("");
        let status: u16 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        if !version.starts_with("HTTP/") || !(100..=999).contains(&status) {
            return Err(ExchangeError { sent: true, timed_out: false, refused: false });
        }
        let headers: Vec<(String, String)> = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
            .collect();
        let rest = data[body_start..].to_vec();
        if (100..200).contains(&status) && status != 101 {
            data = rest;
            continue;
        }
        break (status, headers, rest);
    };
    let mut response = RawResponse { status, headers, ..RawResponse::default() };
    if !request.read_body || request.method == "HEAD" || status == 204 || status == 304 {
        connection.close();
        return Ok(response);
    }
    let chunked = response.header("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));
    let length: Option<usize> =
        if chunked { None } else { response.header("content-length").and_then(|v| v.parse().ok()) };
    let cap = request.max_body + 1;
    let mut body: Vec<u8> = Vec::new();
    let mut raw = std::mem::take(&mut rest);
    let mut eof = false;
    // Pull more raw bytes; returns false on deadline/error (flags set on the response).
    let mut pull = |raw: &mut Vec<u8>, response: &mut RawResponse, eof: &mut bool| -> bool {
        match connection.read_some(&mut chunk) {
            Ok(0) => {
                *eof = true;
                true
            }
            Ok(n) => {
                raw.extend_from_slice(&chunk[..n]);
                true
            }
            Err(error) if matches!(error.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock) => {
                response.timed_out = true;
                false
            }
            Err(_) => {
                response.read_error = true;
                false
            }
        }
    };
    if chunked {
        'chunks: loop {
            // Chunk size line.
            let line_end = loop {
                if let Some(i) = raw.windows(2).position(|w| w == b"\r\n") {
                    break i;
                }
                if raw.len() > 4096 || eof {
                    response.read_error = true;
                    break 'chunks;
                }
                if !pull(&mut raw, &mut response, &mut eof) {
                    break 'chunks;
                }
            };
            let size_text = String::from_utf8_lossy(&raw[..line_end]).into_owned();
            let size_hex = size_text.split(';').next().unwrap_or("").trim().to_string();
            raw.drain(..line_end + 2);
            let Ok(size) = usize::from_str_radix(&size_hex, 16) else {
                response.read_error = true;
                break;
            };
            if size == 0 {
                break;
            }
            let mut remaining = size;
            while remaining > 0 {
                if raw.is_empty() {
                    if eof {
                        response.read_error = true;
                        break 'chunks;
                    }
                    if !pull(&mut raw, &mut response, &mut eof) {
                        break 'chunks;
                    }
                    continue;
                }
                let take = remaining.min(raw.len());
                let room = cap.saturating_sub(body.len());
                body.extend_from_slice(&raw[..take.min(room)]);
                raw.drain(..take);
                remaining -= take;
                if body.len() >= cap {
                    break 'chunks;
                }
            }
            while raw.len() < 2 && !eof {
                if !pull(&mut raw, &mut response, &mut eof) {
                    break 'chunks;
                }
            }
            if raw.len() >= 2 {
                raw.drain(..2);
            }
        }
    } else {
        let target = length.map(|l| l.min(cap)).unwrap_or(cap);
        loop {
            let room = target.saturating_sub(body.len());
            if !raw.is_empty() {
                let take = room.min(raw.len());
                body.extend_from_slice(&raw[..take]);
                raw.drain(..take);
            }
            if body.len() >= target {
                break;
            }
            if eof {
                if length.is_some() {
                    response.read_error = true;
                }
                break;
            }
            if !pull(&mut raw, &mut response, &mut eof) {
                break;
            }
        }
    }
    if body.len() > request.max_body {
        body.truncate(request.max_body);
        response.truncated = true;
    }
    if response.timed_out {
        response.truncated = true;
    }
    response.body = body;
    connection.close();
    Ok(response)
}

/// `Host` header value: the URL's host (bracketed for IPv6) plus an explicit port.
fn host_header(netloc: &str) -> String {
    netloc.rsplit_once('@').map(|(_, h)| h).unwrap_or(netloc).to_string()
}

fn monotonic_deadline(deadline: Option<f64>, timeout_s: f64) -> f64 {
    let now = crate::timefmt::monotonic();
    let own = now + timeout_s;
    deadline.map_or(own, |d| d.min(own))
}

fn instant_at(monotonic_deadline: f64) -> Instant {
    let left = monotonic_deadline - crate::timefmt::monotonic();
    Instant::now() + Duration::from_secs_f64(left.clamp(0.0, 86_400.0))
}

/// Options for [`bounded_http_get`] (the transport seam takes the same values).
#[derive(Clone)]
pub struct GetOptions<'a> {
    pub source: &'a str,
    pub network: &'a NetworkConfig,
    pub allow_private: bool,
    pub timeout_s: f64,
    pub max_bytes: usize,
    pub resolver: &'a Resolver,
    pub range_request: bool,
    pub deadline: Option<f64>,
}

/// The transport seam: `(url, options) -> HttpResult`.
pub type Transport<'a> = &'a dyn Fn(&str, &GetOptions<'_>) -> HttpResult;

/// Proxy-free bounded GET; every redirect is revalidated against the policy.
pub fn bounded_http_get(url: &str, options: &GetOptions<'_>) -> HttpResult {
    let mut current = url.to_string();
    let operation_deadline = monotonic_deadline(options.deadline, options.timeout_s);
    for redirect_count in 0..=options.network.max_redirects {
        let addresses = match validate_network_target(
            &current,
            options.source,
            options.network,
            options.allow_private,
            options.resolver,
            false,
            Some(operation_deadline),
        ) {
            Ok(addresses) => addresses,
            Err(error) => return HttpResult::failed(&current, &error.0),
        };
        if operation_deadline - crate::timefmt::monotonic() <= 0.0 {
            return HttpResult::failed(&current, "HTTP_TIMEOUT");
        }
        let Ok(parsed) = urlsplit(&current) else {
            return HttpResult::failed(&current, "HTTP_REQUEST_FAILED");
        };
        let Ok((host, port)) = static_url(&current, "direct", false) else {
            return HttpResult::failed(&current, "HTTP_REQUEST_FAILED");
        };
        let mut headers = vec![
            ("User-Agent".to_string(), format!("Dead-Air/{VERSION}")),
            ("Accept".to_string(), "*/*".to_string()),
            ("Accept-Encoding".to_string(), "identity".to_string()),
        ];
        if options.range_request {
            headers.push(("Range".into(), format!("bytes=0-{}", options.max_bytes.saturating_sub(1))));
        }
        let request = Request {
            method: "GET",
            tls: parsed.scheme == "https",
            host: &host,
            port,
            addresses: &addresses,
            host_header: host_header(&parsed.netloc),
            selector: parsed.selector(),
            headers,
            body: b"",
            max_body: options.max_bytes,
            read_body: true,
        };
        let response = match exchange(&request, instant_at(operation_deadline)) {
            Ok(response) => response,
            Err(_) => return HttpResult::failed(&current, "HTTP_REQUEST_FAILED"),
        };
        let status = response.status;
        if (300..400).contains(&status) {
            if let Some(location) = response.header("location").filter(|l| !l.is_empty()) {
                if redirect_count >= options.network.max_redirects {
                    return HttpResult { status: Some(status), ..HttpResult::failed(&current, "REDIRECT_LIMIT") };
                }
                current = urljoin(&current, location);
                continue;
            }
        }
        let error_code = if response.read_error {
            return HttpResult { status: Some(status), ..HttpResult::failed(&current, "HTTP_READ_FAILED") };
        } else if response.timed_out {
            Some("HTTP_TIMEOUT".to_string())
        } else {
            None
        };
        return HttpResult {
            status: Some(status),
            final_url: current,
            content_type: response.content_type(),
            truncated: response.truncated,
            body: response.body,
            error_code,
        };
    }
    HttpResult::failed(&current, "REDIRECT_LIMIT")
}

/// `Retry-After` as seconds from `now`, clamped to 0..120.
pub fn retry_after_seconds(value: Option<&str>, now: f64) -> Option<f64> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }
    let delay = match value.parse::<f64>() {
        Ok(seconds) => seconds,
        Err(_) => crate::timefmt::parse_http_date(value)? - now,
    };
    if !delay.is_finite() {
        return None;
    }
    Some(delay.clamp(0.0, 120.0))
}

/// Outcome of a notification POST: `(status, error_code, retry_after_s)`.
pub type PostOutcome = (Option<u16>, Option<String>, Option<f64>);

/// HTTPS-only bounded POST to a policy-checked endpoint; redirects are not followed.
pub fn bounded_http_post(
    url: &str,
    body: &[u8],
    headers: &[(String, String)],
    timeout_s: f64,
    network: &NetworkConfig,
    resolver: &Resolver,
    allow_private: bool,
) -> PostOutcome {
    let failed = || (None, Some("HTTP_REQUEST_FAILED".to_string()), None);
    let Ok((host, port)) = static_url(url, "direct", false) else {
        return failed();
    };
    let Ok(parsed) = urlsplit(url) else {
        return failed();
    };
    if parsed.scheme != "https" {
        return (None, Some("TLS_REQUIRED".into()), None);
    }
    let deadline = crate::timefmt::monotonic() + timeout_s;
    let addresses =
        match validate_network_target(url, "direct", network, allow_private, resolver, false, Some(deadline)) {
            Ok(addresses) => addresses,
            Err(error) => return (None, Some(error.0), None),
        };
    let request = Request {
        method: "POST",
        tls: true,
        host: &host,
        port,
        addresses: &addresses,
        host_header: host_header(&parsed.netloc),
        selector: parsed.selector(),
        headers: headers.to_vec(),
        body,
        max_body: 4096,
        read_body: false,
    };
    match exchange(&request, instant_at(deadline)) {
        Ok(response) => {
            let retry_after = retry_after_seconds(response.header("retry-after"), crate::timefmt::wall());
            let error = if (200..300).contains(&response.status) { None } else { Some("HTTP_STATUS".to_string()) };
            (Some(response.status), error, retry_after)
        }
        Err(_) => failed(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_is_bounded() {
        assert_eq!(retry_after_seconds(Some("300"), 0.0), Some(120.0));
        assert_eq!(retry_after_seconds(Some(" 90 "), 0.0), Some(90.0));
        assert_eq!(retry_after_seconds(Some("-5"), 0.0), Some(0.0));
        assert_eq!(retry_after_seconds(Some("Thu, 01 Jan 1970 00:01:00 GMT"), 30.0), Some(30.0));
        assert_eq!(retry_after_seconds(Some("later"), 0.0), None);
        assert_eq!(retry_after_seconds(None, 0.0), None);
    }

    #[test]
    fn plain_http_post_is_refused_before_any_network() {
        let network = NetworkConfig { private_allowlist: vec![], max_redirects: 3, thumbnail_max_bytes: 1024 };
        let outcome =
            bounded_http_post("http://hooks.invalid/hook", b"{}", &[], 5.0, &network, &Resolver::System, false);
        assert_eq!(outcome, (None, Some("TLS_REQUIRED".into()), None));
    }

    fn serve_once(reply: &'static [u8]) -> u16 {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut buffer = [0u8; 4096];
            let _ = socket.read(&mut buffer);
            let _ = socket.write_all(reply);
        });
        port
    }

    fn get(port: u16, max_body: usize) -> RawResponse {
        let addresses = ["127.0.0.1".parse().unwrap()];
        let request = Request {
            method: "GET",
            tls: false,
            host: "127.0.0.1",
            port,
            addresses: &addresses,
            host_header: format!("127.0.0.1:{port}"),
            selector: "/".into(),
            headers: vec![],
            body: b"",
            max_body,
            read_body: true,
        };
        exchange(&request, Instant::now() + Duration::from_secs(5)).unwrap()
    }

    #[test]
    fn chunked_bodies_continue_responses_and_bare_lf_heads_are_parsed() {
        let port = serve_once(b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: application/vnd.apple.mpegurl\r\n\r\n8\r\n#EXTM3U\n\r\n10;ext=1\r\n#EXT-X-ENDLIST\n\n\r\n0\r\n\r\n");
        let response = get(port, 1024);
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"#EXTM3U\n#EXT-X-ENDLIST\n\n");
        assert!(!response.truncated && !response.read_error);
        let port = serve_once(b"HTTP/1.0 200 OK\nContent-Type: text/plain\n\nline one\r\n\r\nline two");
        let response = get(port, 1024);
        assert_eq!(response.body, b"line one\r\n\r\nline two", "a bare-LF head ends at its own blank line");
        let port = serve_once(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n0123456789");
        let response = get(port, 4);
        assert_eq!((response.body.as_slice(), response.truncated), (b"0123".as_slice(), true));
        let port = serve_once(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n0123");
        assert!(get(port, 1024).read_error, "a short Content-Length body is a read failure");
    }

    #[test]
    fn content_type_defaults_like_python() {
        let mut response = RawResponse::default();
        assert_eq!(response.content_type(), "text/plain");
        response.headers.push(("content-type".into(), "Application/VND.Apple.MPEGURL; charset=utf-8".into()));
        assert_eq!(response.content_type(), "application/vnd.apple.mpegurl");
        response.headers[0].1 = "garbage".into();
        assert_eq!(response.content_type(), "text/plain");
    }
}
