//! Canonical JSON (docs: "Canonical JSON and the id"): keys sorted by byte order, no
//! whitespace, integers as integers, minimal escapes. Written by hand so the bytes do not
//! depend on a serializer's settings.

use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Int(i128),
    Float(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(BTreeMap<String, Json>),
}

impl Json {
    pub fn write(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(it) => out.push_str(if *it { "true" } else { "false" }),
            Json::Int(it) => out.push_str(&it.to_string()),
            Json::Float(it) if it.is_finite() => {
                if it.fract() == 0.0 && it.abs() < 1e15 {
                    out.push_str(&format!("{it:.1}"));
                } else {
                    out.push_str(&format!("{it:?}"));
                }
            }
            Json::Float(_) => out.push_str("null"),
            Json::Str(it) => string(it, out),
            Json::Arr(items) => {
                out.push('[');
                for (i, it) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    it.write(out);
                }
                out.push(']');
            }
            Json::Obj(map) => {
                out.push('{');
                for (i, (k, v)) in map.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    string(k, out);
                    out.push(':');
                    v.write(out);
                }
                out.push('}');
            }
        }
    }

    pub fn to_text(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    /// From parsed JSON (integers stay exact up to `u64`/`i64`).
    pub fn from_value(value: &serde_json::Value) -> Json {
        match value {
            serde_json::Value::Null => Json::Null,
            serde_json::Value::Bool(it) => Json::Bool(*it),
            serde_json::Value::Number(it) => match (it.as_i64(), it.as_u64()) {
                (Some(it), _) => Json::Int(i128::from(it)),
                (None, Some(it)) => Json::Int(i128::from(it)),
                _ => Json::Float(it.as_f64().unwrap_or(f64::NAN)),
            },
            serde_json::Value::String(it) => Json::Str(it.clone()),
            serde_json::Value::Array(it) => Json::Arr(it.iter().map(Json::from_value).collect()),
            serde_json::Value::Object(it) => {
                Json::Obj(it.iter().map(|(k, v)| (k.clone(), Json::from_value(v))).collect())
            }
        }
    }

    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(map) => map.get(key),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(it) => Some(it),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Json::Int(it) => i64::try_from(*it).ok(),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Json::Int(it) => u64::try_from(*it).ok(),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(it) => Some(*it),
            _ => None,
        }
    }
}

/// An object from `(key, value)` pairs.
pub fn obj<const N: usize>(pairs: [(&str, Json); N]) -> Json {
    Json::Obj(pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
}

pub fn str(text: &str) -> Json {
    Json::Str(text.to_owned())
}

pub fn opt_u64(it: Option<u64>) -> Json {
    it.map_or(Json::Null, |it| Json::Int(i128::from(it)))
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
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}
