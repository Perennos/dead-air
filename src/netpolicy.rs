//! URL policy, address classification and bounded DNS.
//!
//! Public targets are the default. A private, loopback or otherwise non-global address is
//! accepted only when the target opted in (`allow_private = true`) *and* its exact
//! `host:port` is in `network.private_allowlist`. Cloud metadata addresses are always denied.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use crate::constants::{CLOUD_METADATA_IPS, YOUTUBE_HOSTS};
use crate::model::{ConfigError, NetworkConfig, PolicyError};
use crate::url::{query_first, urlsplit};

fn v4_in(address: Ipv4Addr, network: [u8; 4], prefix: u32) -> bool {
    let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
    (u32::from(address) & mask) == (u32::from(Ipv4Addr::from(network)) & mask)
}

fn v6_in(address: Ipv6Addr, network: &str, prefix: u32) -> bool {
    let network: Ipv6Addr = network.parse().expect("static network");
    let mask = if prefix == 0 { 0 } else { u128::MAX << (128 - prefix) };
    (u128::from(address) & mask) == (u128::from(network) & mask)
}

/// Python `ipaddress` `is_global` (IANA special-purpose registries), plus multicast and
/// site-local, which this monitor never needs to reach.
pub fn is_global(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => {
            const PRIVATE: &[([u8; 4], u32)] = &[
                ([0, 0, 0, 0], 8),
                ([10, 0, 0, 0], 8),
                ([127, 0, 0, 0], 8),
                ([169, 254, 0, 0], 16),
                ([172, 16, 0, 0], 12),
                ([192, 0, 2, 0], 24),
                ([192, 168, 0, 0], 16),
                ([198, 18, 0, 0], 15),
                ([198, 51, 100, 0], 24),
                ([203, 0, 113, 0], 24),
                ([240, 0, 0, 0], 4),
                ([100, 64, 0, 0], 10),
                ([224, 0, 0, 0], 4),
            ];
            if PRIVATE.iter().any(|(net, prefix)| v4_in(v4, *net, *prefix)) {
                return false;
            }
            if v4_in(v4, [192, 0, 0, 0], 24) {
                // 192.0.0.9 and 192.0.0.10 are globally reachable exceptions.
                return matches!(v4.octets()[3], 9 | 10);
            }
            true
        }
        IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                let _ = mapped;
                return false; // ::ffff:0:0/96 is never global
            }
            const EXCEPTIONS: &[(&str, u32)] = &[
                ("2001:1::1", 128),
                ("2001:1::2", 128),
                ("2001:3::", 32),
                ("2001:4:112::", 48),
                ("2001:20::", 28),
                ("2001:30::", 28),
            ];
            if v6_in(v6, "2001::", 23) {
                return EXCEPTIONS.iter().any(|(net, prefix)| v6_in(v6, net, *prefix));
            }
            const PRIVATE: &[(&str, u32)] = &[
                ("::1", 128),
                ("::", 128),
                ("64:ff9b:1::", 48),
                ("100::", 64),
                ("2001:db8::", 32),
                ("2002::", 16),
                ("fc00::", 7),
                ("fe80::", 10),
                ("fec0::", 10),
                ("ff00::", 8),
            ];
            !PRIVATE.iter().any(|(net, prefix)| v6_in(v6, net, *prefix))
        }
    }
}

/// Parse an address the way Python's `ipaddress.ip_address` does after stripping a zone id.
pub fn parse_ip(text: &str) -> Option<IpAddr> {
    let bare = text.split('%').next().unwrap_or(text);
    bare.parse::<IpAddr>().ok()
}

fn valid_hostname(host: &str) -> bool {
    let bytes = host.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 253
        && bytes[0].is_ascii_alphanumeric()
        && bytes[bytes.len() - 1].is_ascii_alphanumeric()
        && bytes.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'.' || *b == b'-')
}

/// URL hosts may also carry underscores (some CDN names do); nothing that could break a
/// request line or header is accepted.
fn valid_url_host(host: &str) -> bool {
    !host.is_empty() && host.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

/// Python's IDNA codec on an ASCII name: inner labels 1..63 characters, last label < 64.
fn idna_ascii(host: &str) -> Option<String> {
    if !host.is_ascii() {
        return None;
    }
    let labels: Vec<&str> = host.split('.').collect();
    let (last, inner) = labels.split_last()?;
    if inner.iter().any(|label| label.is_empty() || label.len() > 63) || last.len() >= 64 {
        return None;
    }
    Some(host.to_ascii_lowercase())
}

/// Normalise and validate a `private_allowlist` entry (`host:port` or `[v6]:port`).
pub fn safe_hostport(value: &str) -> Result<String, ConfigError> {
    let malformed = || ConfigError::new("private_allowlist entry is malformed");
    if value.chars().count() > 260 || value.contains('\0') {
        return Err(ConfigError::new("private_allowlist entries must be host:port strings"));
    }
    let (host, port_text) = if let Some(inner) = value.strip_prefix('[') {
        let close = inner.find(']').ok_or_else(malformed)?;
        let after = &inner[close + 1..];
        let port = after.strip_prefix(':').ok_or_else(malformed)?;
        (&inner[..close], port)
    } else {
        value.rsplit_once(':').ok_or_else(malformed)?
    };
    let port: u32 = port_text.trim().parse().map_err(|_| malformed())?;
    if host.is_empty() || !(1..=65_535).contains(&port) {
        return Err(malformed());
    }
    if parse_ip(host).is_none() && !valid_hostname(host) {
        return Err(malformed());
    }
    let mut normalized = host.to_lowercase();
    if normalized.contains(':') {
        normalized = format!("[{normalized}]");
    }
    Ok(format!("{normalized}:{port}"))
}

/// The allowlist key for a resolved target.
pub fn hostport_key(host: &str, port: u16) -> String {
    let mut normalized = host.to_ascii_lowercase().trim_end_matches('.').to_string();
    if normalized.contains(':') {
        normalized = format!("[{normalized}]");
    }
    format!("{normalized}:{port}")
}

/// Static URL policy (no network): returns the normalised host and effective port.
pub fn static_url(url: &str, source: &str, thumbnail: bool) -> Result<(String, u16), ConfigError> {
    if url.chars().any(|c| (c as u32) < 32 || c as u32 == 127) || url.contains('#') {
        return Err(ConfigError::new("URL contains a control character or fragment"));
    }
    let parsed = urlsplit(url).map_err(|_| ConfigError::new("URL host or port is malformed"))?;
    let port = parsed.port().map_err(|_| ConfigError::new("URL host or port is malformed"))?;
    if parsed.has_userinfo() {
        return Err(ConfigError::new("URL userinfo is prohibited"));
    }
    let scheme = parsed.scheme.as_str();
    let scheme_ok = if thumbnail { scheme == "https" } else { scheme == "http" || scheme == "https" };
    let Some(host) = parsed.hostname() else {
        return Err(ConfigError::new("URL must be an absolute permitted HTTP(S) URL"));
    };
    if !scheme_ok {
        return Err(ConfigError::new("URL must be an absolute permitted HTTP(S) URL"));
    }
    let port = port.unwrap_or(if scheme == "https" { 443 } else { 80 });
    let normalized = if parse_ip(&host).is_some() {
        host.clone()
    } else {
        // Non-ASCII names must be supplied in their xn-- form; the monitor does not apply
        // IDNA mapping itself.
        let ascii = idna_ascii(&host).ok_or_else(|| ConfigError::new("URL hostname is malformed"))?;
        let trimmed = ascii.trim_end_matches('.').to_string();
        if !valid_url_host(&trimmed) {
            return Err(ConfigError::new("URL hostname is malformed"));
        }
        trimmed
    };
    if normalized.is_empty() || !(1..=65_535).contains(&port) {
        return Err(ConfigError::new("URL host or port is malformed"));
    }
    if source == "youtube" {
        if scheme != "https" || !YOUTUBE_HOSTS.contains(&normalized.as_str()) {
            return Err(ConfigError::new("YouTube source must use an approved HTTPS host"));
        }
        let trimmed = parsed.path.trim_end_matches('/');
        let path = if trimmed.is_empty() { "/" } else { trimmed };
        let valid_watch = path == "/watch" && !query_first(&parsed.query, "v", true).is_empty();
        let valid_live = parsed.path.starts_with("/live/") && parsed.path.len() > "/live/".len();
        let valid_short = normalized == "youtu.be" && !parsed.path.trim_matches('/').is_empty();
        if !(valid_watch || valid_live || valid_short) {
            return Err(ConfigError::new("YouTube URL must identify one watch or live item"));
        }
    }
    Ok((normalized, port as u16))
}

/// A resolver that returns textual addresses (test seam) or the bounded system resolver.
#[derive(Clone)]
pub enum Resolver {
    System,
    Fixed(Arc<dyn Fn(&str, u16) -> Result<Vec<String>, String> + Send + Sync>),
}

impl Resolver {
    pub fn fixed<F>(f: F) -> Self
    where
        F: Fn(&str, u16) -> Result<Vec<String>, String> + Send + Sync + 'static,
    {
        Resolver::Fixed(Arc::new(f))
    }

    /// A resolver that answers every name with the same addresses.
    pub fn always(addresses: &[&str]) -> Self {
        let owned: Vec<String> = addresses.iter().map(|a| a.to_string()).collect();
        Resolver::fixed(move |_, _| Ok(owned.clone()))
    }
}

static OUTSTANDING_LOOKUPS: AtomicUsize = AtomicUsize::new(0);
const MAX_OUTSTANDING_LOOKUPS: usize = 8;

/// System DNS on a bounded worker thread. A lookup that outlives its deadline is abandoned;
/// at most eight abandoned lookups may be outstanding before new lookups are refused.
pub fn system_resolve(host: &str, port: u16, timeout_s: f64) -> Result<Vec<IpAddr>, PolicyError> {
    if let Some(literal) = parse_ip(host) {
        return Ok(vec![literal]);
    }
    if !(timeout_s > 0.0) {
        return Err(PolicyError::new("DNS_TIMEOUT"));
    }
    if OUTSTANDING_LOOKUPS.fetch_add(1, Ordering::SeqCst) >= MAX_OUTSTANDING_LOOKUPS {
        OUTSTANDING_LOOKUPS.fetch_sub(1, Ordering::SeqCst);
        return Err(PolicyError::new("DNS_UNAVAILABLE"));
    }
    let (sender, receiver) = mpsc::channel();
    let name = host.to_string();
    let spawned = std::thread::Builder::new().name("dead-air-dns".into()).spawn(move || {
        let answer = (name.as_str(), port).to_socket_addrs().map(|iter| iter.map(|a| a.ip()).collect::<Vec<_>>());
        OUTSTANDING_LOOKUPS.fetch_sub(1, Ordering::SeqCst);
        let _ = sender.send(answer);
    });
    if spawned.is_err() {
        OUTSTANDING_LOOKUPS.fetch_sub(1, Ordering::SeqCst);
        return Err(PolicyError::new("DNS_UNAVAILABLE"));
    }
    let deadline = std::time::Instant::now() + Duration::from_secs_f64(timeout_s.min(120.0));
    loop {
        crate::interrupt::check();
        let now = std::time::Instant::now();
        if now >= deadline {
            return Err(PolicyError::new("DNS_TIMEOUT"));
        }
        match receiver.recv_timeout((deadline - now).min(Duration::from_millis(50))) {
            Ok(Ok(addresses)) => {
                let mut unique: Vec<IpAddr> = Vec::new();
                for address in addresses {
                    if !unique.contains(&address) {
                        unique.push(address);
                    }
                }
                if unique.len() > 64 {
                    return Err(PolicyError::new("DNS_UNAVAILABLE"));
                }
                if unique.is_empty() {
                    return Err(PolicyError::new("DNS_EMPTY"));
                }
                return Ok(unique);
            }
            Ok(Err(_)) | Err(mpsc::RecvTimeoutError::Disconnected) => return Err(PolicyError::new("DNS_UNAVAILABLE")),
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
        }
    }
}

/// Remaining seconds of `configured_s` before a monotonic deadline; `None` when spent.
pub fn remaining(configured_s: f64, deadline: Option<f64>, now: f64) -> Option<f64> {
    match deadline {
        None => Some(configured_s),
        Some(deadline) => {
            let left = deadline - now;
            if left <= 0.0 { None } else { Some(configured_s.min(left)) }
        }
    }
}

/// Validate a URL against the static policy, resolve it and check every address.
pub fn validate_network_target(
    url: &str,
    source: &str,
    network: &NetworkConfig,
    allow_private: bool,
    resolver: &Resolver,
    thumbnail: bool,
    deadline: Option<f64>,
) -> Result<Vec<IpAddr>, PolicyError> {
    let (host, port) = static_url(url, source, thumbnail).map_err(|_| PolicyError::new("URL_POLICY_REJECTED"))?;
    let addresses: Vec<IpAddr> = match resolver {
        Resolver::System => {
            let timeout = remaining(5.0, deadline, crate::timefmt::monotonic())
                .ok_or_else(|| PolicyError::new("DNS_UNAVAILABLE"))?;
            system_resolve(&host, port, timeout).map_err(|_| PolicyError::new("DNS_UNAVAILABLE"))?
        }
        Resolver::Fixed(answer) => {
            let raw = answer(&host, port).map_err(|_| PolicyError::new("DNS_UNAVAILABLE"))?;
            let mut parsed = Vec::new();
            for text in raw {
                parsed.push(parse_ip(&text).ok_or_else(|| PolicyError::new("DNS_UNAVAILABLE"))?);
            }
            parsed
        }
    };
    if addresses.is_empty() {
        return Err(PolicyError::new("DNS_EMPTY"));
    }
    let key = hostport_key(&host, port);
    let explicit = allow_private && network.private_allowlist.iter().any(|entry| entry == &key);
    for address in &addresses {
        if CLOUD_METADATA_IPS.iter().any(|ip| parse_ip(ip) == Some(*address)) {
            return Err(PolicyError::new("CLOUD_METADATA_DENIED"));
        }
        if !is_global(*address) && !explicit {
            return Err(PolicyError::new("PRIVATE_NETWORK_DENIED"));
        }
    }
    Ok(addresses)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn global_classification_follows_the_python_registry() {
        for public in ["93.184.216.34", "8.8.8.8", "192.0.0.9", "2606:4700::1111", "2001:4:112::1"] {
            assert!(is_global(ip(public)), "{public}");
        }
        for private in [
            "10.0.0.8",
            "127.0.0.1",
            "169.254.169.254",
            "172.16.0.1",
            "192.168.1.1",
            "100.64.0.1",
            "0.0.0.0",
            "192.0.2.1",
            "198.18.0.1",
            "240.0.0.1",
            "255.255.255.255",
            "224.0.0.1",
            "::1",
            "::",
            "fe80::1",
            "fd00::1",
            "2001:db8::1",
            "::ffff:10.0.0.1",
            "2002::1",
            "ff02::1",
        ] {
            assert!(!is_global(ip(private)), "{private}");
        }
    }

    #[test]
    fn static_policy_rejects_bad_urls_without_network() {
        assert_eq!(
            static_url("https://Example.Invalid./live", "direct", false).unwrap(),
            ("example.invalid".into(), 443)
        );
        assert_eq!(static_url("http://127.0.0.1:8080/x", "direct", false).unwrap(), ("127.0.0.1".into(), 8080));
        assert_eq!(static_url("http://[::1]:80/x", "direct", false).unwrap(), ("::1".into(), 80));
        for bad in [
            "https://example.invalid:0/live",
            "ftp://example.invalid/x",
            "https://user:pw@example.invalid/x",
            "https://@example.invalid/x",
            "https://example.invalid/x#frag",
            "https://exa\nmple.invalid/x",
            "https://a..b/x",
            "https:///x",
            "https://d\u{ed}a.example/x",
        ] {
            assert!(static_url(bad, "direct", false).is_err(), "{bad}");
        }
        assert!(static_url("http://example.invalid/x.png", "direct", true).is_err());
        assert!(static_url("https://www.youtube.com/watch?v=abc", "youtube", false).is_ok());
        assert!(static_url("https://www.youtube.com/live/abc", "youtube", false).is_ok());
        assert!(static_url("https://youtu.be/AbCdEfGhijk", "youtube", false).is_ok());
        assert!(static_url("https://example.invalid/watch?v=demo", "youtube", false).is_err());
        assert!(static_url("https://www.youtube.com/watch?v=", "youtube", false).is_err());
        assert!(static_url("http://www.youtube.com/watch?v=a", "youtube", false).is_err());
    }

    #[test]
    fn allowlist_entries_are_normalised() {
        assert_eq!(safe_hostport("Example.Invalid:443").unwrap(), "example.invalid:443");
        assert_eq!(safe_hostport("[::1]:8443").unwrap(), "[::1]:8443");
        assert_eq!(safe_hostport("10.0.0.8:465").unwrap(), "10.0.0.8:465");
        for bad in ["example.invalid", "host:0", "host:70000", ":80", "[::1]8443", "bad host:1"] {
            assert!(safe_hostport(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn private_answers_need_opt_in_and_exact_allowlist() {
        let mut network = NetworkConfig { private_allowlist: vec![], max_redirects: 3, thumbnail_max_bytes: 1024 };
        let private = Resolver::always(&["10.0.0.8"]);
        let url = "https://example.invalid/live/sample";
        assert_eq!(
            validate_network_target(url, "direct", &network, false, &private, false, None).unwrap_err().0,
            "PRIVATE_NETWORK_DENIED"
        );
        network.private_allowlist = vec!["example.invalid:443".into()];
        assert!(validate_network_target(url, "direct", &network, false, &private, false, None).is_err());
        assert_eq!(
            validate_network_target(url, "direct", &network, true, &private, false, None).unwrap(),
            vec![ip("10.0.0.8")]
        );
        let metadata = Resolver::always(&["169.254.169.254"]);
        assert_eq!(
            validate_network_target(url, "direct", &network, true, &metadata, false, None).unwrap_err().0,
            "CLOUD_METADATA_DENIED"
        );
        let empty = Resolver::always(&[]);
        assert_eq!(
            validate_network_target(url, "direct", &network, true, &empty, false, None).unwrap_err().0,
            "DNS_EMPTY"
        );
        let broken = Resolver::fixed(|_, _| Err("boom".into()));
        assert_eq!(
            validate_network_target(url, "direct", &network, true, &broken, false, None).unwrap_err().0,
            "DNS_UNAVAILABLE"
        );
        let public = Resolver::always(&["93.184.216.34"]);
        assert!(validate_network_target(url, "direct", &network, false, &public, false, None).is_ok());
    }

    #[test]
    fn literal_addresses_skip_dns_and_spent_deadline_is_unavailable() {
        assert_eq!(system_resolve("127.0.0.1", 80, 0.0).unwrap(), vec![ip("127.0.0.1")]);
        assert_eq!(system_resolve("example.invalid", 80, 0.0).unwrap_err().0, "DNS_TIMEOUT");
        let network = NetworkConfig { private_allowlist: vec![], max_redirects: 3, thumbnail_max_bytes: 1024 };
        let spent = Some(crate::timefmt::monotonic() - 1.0);
        assert_eq!(
            validate_network_target(
                "https://example.invalid/",
                "direct",
                &network,
                false,
                &Resolver::System,
                false,
                spent
            )
            .unwrap_err()
            .0,
            "DNS_UNAVAILABLE"
        );
    }
}
