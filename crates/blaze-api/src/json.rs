//! A [`Value`] as JSON, for people: what `--json` prints and what HTTP answers with
//! `Accept: application/json`. Byte strings are hex, and map keys that are not text are
//! written as their rendering. It is never read back.

use std::fmt::Write as _;

use crate::Value;

/// `value` as one line of JSON.
pub fn render(value: &Value) -> String {
    let mut out = String::new();
    write(value, &mut out);
    out
}

fn write(value: &Value, out: &mut String) {
    match value {
        Value::Uint(n) => _ = write!(out, "{n}"),
        Value::Nint(n) => _ = write!(out, "{}", -1 - i128::from(*n)),
        Value::Bytes(bytes) => {
            out.push('"');
            bytes.iter().for_each(|it| _ = write!(out, "{it:02x}"));
            out.push('"');
        }
        Value::Text(text) => string(text, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write(item, out);
            }
            out.push(']');
        }
        Value::Map(entries) => {
            out.push('{');
            for (i, (key, value)) in entries.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                match key {
                    Value::Text(text) => string(text, out),
                    other => string(&render(other), out),
                }
                out.push(':');
                write(value, out);
            }
            out.push('}');
        }
        Value::Bool(v) => out.push_str(if *v { "true" } else { "false" }),
        Value::Null => out.push_str("null"),
    }
}

fn string(text: &str, out: &mut String) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => _ = write!(out, "\\u{:04x}", c as u32),
            c => out.push(c),
        }
    }
    out.push('"');
}
