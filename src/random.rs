//! Random identifiers and small encodings.

/// Fill a buffer from the operating system's CSPRNG.
pub fn bytes<const N: usize>() -> [u8; N] {
    let mut buffer = [0u8; N];
    if getrandom::fill(&mut buffer).is_err() {
        // The OS generator failing is not recoverable for identifiers or tokens.
        panic!("operating system random generator unavailable");
    }
    buffer
}

/// A random version-4 UUID as 32 lowercase hex characters (Python `uuid4().hex`).
pub fn uuid4_hex() -> String {
    let mut raw = bytes::<16>();
    raw[6] = (raw[6] & 0x0f) | 0x40;
    raw[8] = (raw[8] & 0x3f) | 0x80;
    hex(&raw)
}

pub fn hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}

/// Python `secrets.token_urlsafe(32)`: 43 URL-safe base64 characters.
pub fn token_urlsafe() -> String {
    base64(&bytes::<32>(), true)
}

const STANDARD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const URLSAFE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Base64 encoding; the URL-safe form omits padding.
pub fn base64(data: &[u8], urlsafe: bool) -> String {
    let table = if urlsafe { URLSAFE } else { STANDARD };
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(table[(n >> 18) as usize & 63] as char);
        out.push(table[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(table[(n >> 6) as usize & 63] as char);
        } else if !urlsafe {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(table[n as usize & 63] as char);
        } else if !urlsafe {
            out.push('=');
        }
    }
    out
}

/// Constant-time byte comparison for tokens.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_and_tokens_have_python_shapes() {
        let id = uuid4_hex();
        assert_eq!(id.len(), 32);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_eq!(&id[12..13], "4");
        let token = token_urlsafe();
        assert_eq!(token.len(), 43);
        assert!(token.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn base64_vectors() {
        assert_eq!(base64(b"", false), "");
        assert_eq!(base64(b"f", false), "Zg==");
        assert_eq!(base64(b"fo", false), "Zm8=");
        assert_eq!(base64(b"foo", false), "Zm9v");
        assert_eq!(base64(b"\0user\0pass", false), "AHVzZXIAcGFzcw==");
        assert_eq!(base64(&[0xfb, 0xff], true), "-_8");
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
