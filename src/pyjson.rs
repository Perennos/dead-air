//! JSON text that matches the Python 0.2.0 release byte for byte.
//!
//! The Python CLI printed `json.dumps(value, sort_keys=True)`: `", "` and `": "` separators,
//! ASCII-only output (`ensure_ascii`) and `float.__repr__` numbers. State rows and event
//! payloads used the compact `(",", ":")` separators. `serde_json::Value` objects are sorted
//! maps here (no `preserve_order`), so key order matches `sort_keys=True` as well.

use serde_json::Value;

/// `json.dumps(value, sort_keys=True)`.
pub fn dumps(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value, ", ", ": ", true);
    out
}

/// `json.dumps(value, sort_keys=True, separators=(",", ":"))`.
pub fn dumps_compact(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value, ",", ":", true);
    out
}

fn write_value(out: &mut String, value: &Value, item_sep: &str, key_sep: &str, ascii: bool) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => {
            if let Some(i) = number.as_i64() {
                out.push_str(&i.to_string());
            } else if let Some(u) = number.as_u64() {
                out.push_str(&u.to_string());
            } else {
                out.push_str(&float_repr(number.as_f64().unwrap_or(0.0)));
            }
        }
        Value::String(text) => write_string(out, text, ascii),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(item_sep);
                }
                write_value(out, item, item_sep, key_sep, ascii);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (index, (key, item)) in map.iter().enumerate() {
                if index > 0 {
                    out.push_str(item_sep);
                }
                write_string(out, key, ascii);
                out.push_str(key_sep);
                write_value(out, item, item_sep, key_sep, ascii);
            }
            out.push('}');
        }
    }
}

/// A JSON string literal; with `ascii` every character outside space..tilde is escaped.
pub fn write_string(out: &mut String, text: &str, ascii: bool) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c if ascii && (c as u32) > 0x7e => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{:04x}", unit));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Python's `repr(float)`: shortest round-trip digits, fixed notation for exponents -4..15.
pub fn float_repr(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    if value == 0.0 {
        return if value.is_sign_negative() { "-0.0".into() } else { "0.0".into() };
    }
    // Rust's `{:e}` is the shortest representation that round-trips, like Python's repr.
    let sci = format!("{:e}", value);
    let (mantissa, exponent) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let negative = mantissa.starts_with('-');
    let digits: String = mantissa.chars().filter(|c| c.is_ascii_digit()).collect();
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    if (-4..16).contains(&exponent) {
        if exponent >= 0 {
            let point = exponent as usize + 1;
            if digits.len() <= point {
                out.push_str(&digits);
                out.push_str(&"0".repeat(point - digits.len()));
                out.push_str(".0");
            } else {
                out.push_str(&digits[..point]);
                out.push('.');
                out.push_str(&digits[point..]);
            }
        } else {
            out.push_str("0.");
            out.push_str(&"0".repeat((-exponent - 1) as usize));
            out.push_str(&digits);
        }
    } else {
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        out.push(if exponent < 0 { '-' } else { '+' });
        out.push_str(&format!("{:02}", exponent.abs()));
    }
    out
}

/// Python's `round(value, 3)` (round half to even on the exact binary value).
pub fn round3(value: f64) -> f64 {
    format!("{:.3}", value).parse().unwrap_or(value)
}

/// A JSON number that always prints as a float (`2.0`, not `2`).
pub fn float(value: f64) -> Value {
    serde_json::Number::from_f64(value).map(Value::Number).unwrap_or(Value::Null)
}

/// A TOML basic string, escaping every control character TOML forbids.
pub fn toml_string(text: &str) -> String {
    let mut out = String::from("\"");
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn float_repr_matches_python() {
        let cases = [
            (0.1, "0.1"),
            (1e-06, "1e-06"),
            (1e16, "1e+16"),
            (1e-4, "0.0001"),
            (1e-5, "1e-05"),
            (123456789.0, "123456789.0"),
            (2.0, "2.0"),
            (-0.0, "-0.0"),
            (0.001, "0.001"),
            (25165824.0, "25165824.0"),
            (1.5e300, "1.5e+300"),
            (1.0 / 3.0, "0.3333333333333333"),
            (86400.0, "86400.0"),
            (1e15, "1000000000000000.0"),
            (9999999999999998.0, "9999999999999998.0"),
            (-50.0, "-50.0"),
            (12.1, "12.1"),
        ];
        for (value, expected) in cases {
            assert_eq!(float_repr(value), expected, "{value}");
        }
    }

    #[test]
    fn dumps_matches_python_sort_keys_and_ascii() {
        let value = json!({"ok": true, "b": [1, 2.0, null], "a": "D\u{ed}a \u{1F600} \"q\" \\"});
        assert_eq!(dumps(&value), r#"{"a": "D\u00eda \ud83d\ude00 \"q\" \\", "b": [1, 2.0, null], "ok": true}"#);
        assert_eq!(dumps_compact(&json!({"z": 1, "a": {"y": 0.5}})), r#"{"a":{"y":0.5},"z":1}"#);
    }

    #[test]
    fn round3_is_half_even_like_python() {
        assert_eq!(round3(1.0005), 1.0); // 1.0005 is slightly below the tie in binary
        assert_eq!(round3(2.0005), 2.001);
        assert_eq!(round3(1.23456), 1.235);
        assert_eq!(round3(60.0), 60.0);
    }

    #[test]
    fn toml_strings_round_trip_through_the_parser() {
        let text = "Say \"hi\" \\ D\u{ed}a de Muertos\u{7f}\n";
        let doc = format!("x = {}\n", toml_string(text));
        let parsed: toml::Table = doc.parse().unwrap();
        assert_eq!(parsed["x"].as_str().unwrap(), text);
    }
}
