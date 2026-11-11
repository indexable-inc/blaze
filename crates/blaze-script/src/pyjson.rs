//! JSON written as Python's `json.dumps` writes it: `", "` between items, `": "` after a key,
//! non-ASCII as `\uXXXX` unless told otherwise, a float as `repr` gives it. A generated file or
//! an error that quotes a value is then the bytes the Python script wrote.

use serde_json::Value;

/// `json.dumps(value)`.
pub fn dumps(value: &Value) -> String {
    let mut out = String::new();
    write(&mut out, value, true);
    out
}

/// `json.dumps(value, ensure_ascii=False)`.
pub fn dumps_unicode(value: &Value) -> String {
    let mut out = String::new();
    write(&mut out, value, false);
    out
}

fn write(out: &mut String, value: &Value, ascii: bool) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => match (n.as_i64(), n.as_u64(), n.as_f64()) {
            (Some(i), _, _) => out.push_str(&i.to_string()),
            (_, Some(u), _) => out.push_str(&u.to_string()),
            (_, _, Some(f)) => out.push_str(&float(f)),
            _ => out.push_str(&n.to_string()),
        },
        Value::String(s) => string(out, s, ascii),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write(out, item, ascii);
            }
            out.push(']');
        }
        Value::Object(fields) => {
            out.push('{');
            for (i, (key, item)) in fields.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                string(out, key, ascii);
                out.push_str(": ");
                write(out, item, ascii);
            }
            out.push('}');
        }
    }
}

/// A string as `json.dumps` quotes it.
pub fn string(out: &mut String, s: &str, ascii: bool) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c if ascii && !c.is_ascii() => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// A float as Python's `repr`: the shortest digits that read back, `1.0` for a whole number, and
/// an exponent with a sign and at least two digits (`1e-05`, `1e+16`).
pub fn float(f: f64) -> String {
    if f.is_nan() {
        return "NaN".to_owned();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity".to_owned() } else { "-Infinity".to_owned() };
    }
    let shown = format!("{f:?}");
    match shown.split_once('e') {
        None => shown,
        Some((mantissa, exponent)) => {
            let (sign, digits) = match exponent.strip_prefix('-') {
                Some(digits) => ('-', digits),
                None => ('+', exponent),
            };
            // Python writes `1e-05` where Rust writes `1.0e-5` or `1e-5`.
            let mantissa = mantissa.strip_suffix(".0").unwrap_or(mantissa);
            format!("{mantissa}e{sign}{digits:0>2}")
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_value_is_written_as_python_writes_it() {
        let value = json!({"a": [1, 2.5, "x\ty", null, true], "é": {"b": 1.0}});
        assert_eq!(dumps(&value), r#"{"a": [1, 2.5, "x\ty", null, true], "\u00e9": {"b": 1.0}}"#);
        assert_eq!(dumps_unicode(&value), "{\"a\": [1, 2.5, \"x\\ty\", null, true], \"é\": {\"b\": 1.0}}");
        assert_eq!(dumps(&json!("\u{1F600}\u{1}")), r#""\ud83d\ude00\u0001""#);
    }

    #[test]
    fn a_float_is_pythons_repr() {
        assert_eq!(float(1.0), "1.0");
        assert_eq!(float(0.1), "0.1");
        assert_eq!(float(1e-5), "1e-05");
        assert_eq!(float(1.5e-7), "1.5e-07");
        assert_eq!(float(1e16), "1e+16");
        assert_eq!(float(1e15), "1000000000000000.0");
        assert_eq!(float(0.0001), "0.0001");
    }
}
