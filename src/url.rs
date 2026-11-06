//! A faithful subset of Python's `urllib.parse` for http(s) URLs.

use std::net::Ipv6Addr;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SplitUrl {
    pub scheme: String,
    pub netloc: String,
    pub path: String,
    pub query: String,
    pub fragment: String,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UrlError;

fn is_scheme_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')
}

/// `urllib.parse.urlsplit`, including its bracket validation for IPv6 hosts.
pub fn urlsplit(url: &str) -> Result<SplitUrl, UrlError> {
    // Python strips leading C0 controls and spaces and removes tab/CR/LF everywhere.
    let cleaned: String = url
        .trim_start_matches(|c: char| (c as u32) <= 0x20)
        .chars()
        .filter(|c| !matches!(c, '\t' | '\r' | '\n'))
        .collect();
    let mut rest: &str = &cleaned;
    let mut scheme = String::new();
    if let Some(index) = rest.find(':') {
        let candidate = &rest[..index];
        if index > 0
            && candidate.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
            && candidate.chars().all(is_scheme_char)
        {
            scheme = candidate.to_ascii_lowercase();
            rest = &rest[index + 1..];
        }
    }
    let mut netloc = String::new();
    if let Some(after) = rest.strip_prefix("//") {
        let end = after.find(['/', '?', '#']).unwrap_or(after.len());
        netloc = after[..end].to_string();
        rest = &after[end..];
        let open = netloc.contains('[');
        let close = netloc.contains(']');
        if open != close {
            return Err(UrlError);
        }
        if open {
            let bracketed = netloc.rsplit('@').next().unwrap_or("");
            let inner = bracketed.split_once('[').map(|(_, r)| r).and_then(|r| r.split_once(']'));
            match inner {
                Some((host, _)) if host.parse::<Ipv6Addr>().is_ok() => {}
                Some((host, _)) if host.starts_with(['v', 'V']) && host.contains('.') => {}
                _ => return Err(UrlError),
            }
        }
    }
    let (rest, fragment) = match rest.split_once('#') {
        Some((before, fragment)) => (before, fragment.to_string()),
        None => (rest, String::new()),
    };
    let (path, query) = match rest.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (rest.to_string(), String::new()),
    };
    Ok(SplitUrl { scheme, netloc, path, query, fragment })
}

impl SplitUrl {
    fn hostinfo(&self) -> &str {
        self.netloc.rsplit_once('@').map(|(_, host)| host).unwrap_or(&self.netloc)
    }

    /// True when the netloc carries any userinfo (`user@`, even empty).
    pub fn has_userinfo(&self) -> bool {
        self.netloc.contains('@')
    }

    /// Lowercase host without brackets, like Python's `hostname`.
    pub fn hostname(&self) -> Option<String> {
        let info = self.hostinfo();
        let host = if let Some(inner) = info.strip_prefix('[') {
            inner.split_once(']').map(|(h, _)| h).unwrap_or("")
        } else {
            info.split_once(':').map(|(h, _)| h).unwrap_or(info)
        };
        if host.is_empty() { None } else { Some(host.to_lowercase()) }
    }

    fn port_text(&self) -> Option<&str> {
        let info = self.hostinfo();
        if info.starts_with('[') {
            info.split_once(']').and_then(|(_, after)| after.strip_prefix(':'))
        } else {
            info.split_once(':').map(|(_, port)| port)
        }
    }

    /// Python's `port`: `Ok(None)` when absent or empty, `Err` when malformed or out of range.
    pub fn port(&self) -> Result<Option<u32>, UrlError> {
        match self.port_text() {
            None | Some("") => Ok(None),
            Some(text) if text.bytes().all(|b| b.is_ascii_digit()) => {
                let value: u64 = text.parse().map_err(|_| UrlError)?;
                if value > 65_535 { Err(UrlError) } else { Ok(Some(value as u32)) }
            }
            Some(_) => Err(UrlError),
        }
    }

    /// `urlunsplit`.
    pub fn unsplit(&self) -> String {
        let mut url = self.path.clone();
        if !self.netloc.is_empty() || (is_netloc_scheme(&self.scheme) && !url.starts_with("//")) {
            if !url.is_empty() && !url.starts_with('/') {
                url.insert(0, '/');
            }
            url = format!("//{}{}", self.netloc, url);
        }
        if !self.scheme.is_empty() {
            url = format!("{}:{}", self.scheme, url);
        }
        if !self.query.is_empty() {
            url.push('?');
            url.push_str(&self.query);
        }
        if !self.fragment.is_empty() {
            url.push('#');
            url.push_str(&self.fragment);
        }
        url
    }

    /// The request target: path plus query, `/` when empty.
    pub fn selector(&self) -> String {
        let mut selector = if self.path.is_empty() { "/".to_string() } else { self.path.clone() };
        if !self.query.is_empty() {
            selector.push('?');
            selector.push_str(&self.query);
        }
        selector
    }
}

fn is_netloc_scheme(scheme: &str) -> bool {
    matches!(scheme, "http" | "https" | "ftp" | "ws" | "wss" | "file" | "")
}

/// `urllib.parse.urljoin` for http(s) bases.
pub fn urljoin(base: &str, reference: &str) -> String {
    if base.is_empty() {
        return reference.to_string();
    }
    if reference.is_empty() {
        return base.to_string();
    }
    let (Ok(b), Ok(mut r)) = (urlsplit(base), urlsplit(reference)) else {
        return reference.to_string();
    };
    if r.scheme.is_empty() {
        r.scheme = b.scheme.clone();
    }
    if r.scheme != b.scheme || !matches!(r.scheme.as_str(), "http" | "https") {
        return reference.to_string();
    }
    if !r.netloc.is_empty() {
        return r.unsplit();
    }
    r.netloc = b.netloc.clone();
    if r.path.is_empty() {
        r.path = b.path.clone();
        if r.query.is_empty() {
            r.query = b.query.clone();
        }
        return r.unsplit();
    }
    let mut base_parts: Vec<&str> = b.path.split('/').collect();
    if base_parts.last().is_some_and(|last| !last.is_empty()) {
        base_parts.pop();
    }
    let segments: Vec<String> = if r.path.starts_with('/') {
        r.path.split('/').map(str::to_string).collect()
    } else {
        let mut joined: Vec<String> = base_parts.iter().map(|s| s.to_string()).collect();
        joined.extend(r.path.split('/').map(str::to_string));
        if joined.len() > 2 {
            let last = joined.len() - 1;
            let middle: Vec<String> = joined[1..last].iter().filter(|s| !s.is_empty()).cloned().collect();
            let mut rebuilt = vec![joined[0].clone()];
            rebuilt.extend(middle);
            rebuilt.push(joined[last].clone());
            joined = rebuilt;
        }
        joined
    };
    let mut resolved: Vec<String> = Vec::new();
    for segment in &segments {
        match segment.as_str() {
            ".." => {
                resolved.pop();
            }
            "." => {}
            _ => resolved.push(segment.clone()),
        }
    }
    if segments.last().is_some_and(|s| s == "." || s == "..") {
        resolved.push(String::new());
    }
    let path = resolved.join("/");
    r.path = if path.is_empty() { "/".to_string() } else { path };
    r.unsplit()
}

/// Percent-decoding with `+` as space (`unquote_plus`), replacing invalid UTF-8.
pub fn unquote_plus(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => out.push(b' '),
            b'%' if index + 2 < bytes.len() => {
                let hex = |b: u8| (b as char).to_digit(16);
                if let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2])) {
                    out.push((high * 16 + low) as u8);
                    index += 3;
                    continue;
                }
                out.push(b'%');
            }
            other => out.push(other),
        }
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `parse_qs(query, keep_blank_values=keep_blank).get(key, [""])[0]`.
pub fn query_first(query: &str, key: &str, keep_blank: bool) -> String {
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (name, value) = match pair.split_once('=') {
            Some((name, value)) => (name, value),
            None if keep_blank => (pair, ""),
            None => continue,
        };
        if value.is_empty() && !keep_blank {
            continue;
        }
        if unquote_plus(name) == key {
            return unquote_plus(value);
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_matches_python() {
        let url = urlsplit("HTTPS://User:pw@Example.Invalid:8443/a/b?x=1#frag").unwrap();
        assert_eq!(url.scheme, "https");
        assert_eq!(url.netloc, "User:pw@Example.Invalid:8443");
        assert_eq!(url.hostname().unwrap(), "example.invalid");
        assert_eq!(url.port(), Ok(Some(8443)));
        assert!(url.has_userinfo());
        assert_eq!((url.path.as_str(), url.query.as_str(), url.fragment.as_str()), ("/a/b", "x=1", "frag"));
        let v6 = urlsplit("http://[::1]:80/x").unwrap();
        assert_eq!(v6.hostname().unwrap(), "::1");
        assert_eq!(v6.port(), Ok(Some(80)));
        assert!(urlsplit("http://[::1/x").is_err());
        assert!(urlsplit("http://[nothost]/x").is_err());
        assert_eq!(urlsplit("https://h:0/").unwrap().port(), Ok(Some(0)));
        assert!(urlsplit("https://h:99999/").unwrap().port().is_err());
        assert!(urlsplit("https://h:x1/").unwrap().port().is_err());
        assert_eq!(urlsplit("https://h:/").unwrap().port(), Ok(None));
        assert_eq!(urlsplit("ftp://h/").unwrap().scheme, "ftp");
        assert_eq!(urlsplit("https:///nohost").unwrap().hostname(), None);
    }

    #[test]
    fn join_matches_python() {
        let base = "http://127.0.0.1:8000/dir/index.m3u8?a=1";
        assert_eq!(urljoin(base, "/small"), "http://127.0.0.1:8000/small");
        assert_eq!(urljoin(base, "seg.ts"), "http://127.0.0.1:8000/dir/seg.ts");
        assert_eq!(urljoin(base, "../up"), "http://127.0.0.1:8000/up");
        assert_eq!(urljoin(base, "http://other:1/x"), "http://other:1/x");
        assert_eq!(urljoin(base, "//other/x"), "http://other/x");
        assert_eq!(urljoin(base, "?b=2"), "http://127.0.0.1:8000/dir/index.m3u8?b=2");
        assert_eq!(urljoin("https://a/b/c", "https://d/e"), "https://d/e");
        assert_eq!(urljoin("https://a/b/c", "http://d/e"), "http://d/e");
        assert_eq!(urljoin("https://a/b/c/", "."), "https://a/b/c/");
    }

    #[test]
    fn query_values() {
        assert_eq!(query_first("v=abc&x=1", "v", true), "abc");
        assert_eq!(query_first("v=&x=1", "v", true), "");
        assert_eq!(query_first("x=1&v=a%20b+c", "v", false), "a b c");
        assert_eq!(query_first("v", "v", true), "");
        assert_eq!(query_first("", "v", true), "");
    }
}
