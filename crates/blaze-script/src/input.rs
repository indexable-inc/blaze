//! A script's one typed input (AGENTS.md rule scripts-take-typed-input): a single JSON object,
//! given inline as the one argument, as the path of a file, or as `-` for standard input, read
//! into a struct whose every field is named and typed. A field the struct does not have, one it
//! needs and was not given, and a value of another type are refused naming the field, in the
//! words `tools/typed-input` (Python) used, so a caller sees the same line from either.
//!
//! ```ignore
//! struct Run { suite: String, timeout_minutes: i64 }
//!
//! impl Input for Run {
//!     const FIELDS: &'static [&'static str] = &["suite", "timeout_minutes"];
//!     fn parse(f: &Fields<'_>) -> Result<Run, InputError> {
//!         Ok(Run {
//!             suite: f.one_of("suite", &["ruff-and-ty", "uv"], None)?,
//!             timeout_minutes: f.int("timeout_minutes", Some(300))?,
//!         })
//!     }
//! }
//!
//! let run: Run = input::parse(&argument, "downstream");
//! ```
//!
//! `FIELDS` is in the struct's order: the fields are read, and so refused, in that order.
// when bstd::data is released: derive `Input` by reflection (docs/v2/plan/design/scripts.md,
// "The typed input"); the hand-written `parse` of each script goes.

use std::io::Read;

use serde_json::{Map, Value};

use crate::pyjson::dumps;

/// What is wrong with the input, naming the field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputError(pub String);

/// A struct read from a JSON object.
pub trait Input: Sized {
    /// Every field's name, in the struct's order.
    const FIELDS: &'static [&'static str];

    /// The struct from the object's fields, each read with one of [`Fields`]' methods.
    fn parse(fields: &Fields<'_>) -> Result<Self, InputError>;
}

/// The JSON value of `text`: inline, the path of a file, or `-` for standard input.
pub fn read(text: &str) -> Result<Value, InputError> {
    let held;
    let text = if text == "-" {
        let mut all = String::new();
        std::io::stdin().read_to_string(&mut all).map_err(|e| InputError(e.to_string()))?;
        held = all;
        held.as_str()
    } else if !text.trim_start().starts_with(['{', '[']) {
        held = std::fs::read_to_string(text).map_err(|e| InputError(os_error(&e, text)))?;
        held.as_str()
    } else {
        text
    };
    serde_json::from_str(text).map_err(|e| InputError(format!("the input is not JSON: {e}")))
}

/// An error opening `path` as Python's `OSError` prints: `[Errno 2] No such file or directory: 'x'`.
fn os_error(e: &std::io::Error, path: &str) -> String {
    let said = e.to_string();
    let said = said.split(" (os error").next().unwrap_or(&said);
    match e.raw_os_error() {
        Some(code) => format!("[Errno {code}] {said}: '{path}'"),
        None => format!("{said}: '{path}'"),
    }
}

/// `T` read from the JSON `text` (inline, a path, or `-`); ends the script naming what is wrong.
pub fn parse<T: Input>(text: &str, script: &str) -> T {
    match read(text).and_then(|value| of::<T>(&value, "")) {
        Ok(it) => it,
        Err(InputError(message)) => crate::fail(script, &message),
    }
}

/// `T` from `value`, the object at `where_` (empty for the input itself).
pub fn of<T: Input>(value: &Value, where_: &str) -> Result<T, InputError> {
    let Value::Object(given) = value else {
        let what = if where_.is_empty() { "the input" } else { where_ };
        return Err(InputError(format!("{what} is a JSON object, not {}", dumps(value))));
    };
    let prefix = if where_.is_empty() { String::new() } else { format!("{where_}.") };
    let mut unknown: Vec<&str> =
        given.keys().map(String::as_str).filter(|key| !T::FIELDS.contains(key)).collect();
    if !unknown.is_empty() {
        unknown.sort_unstable();
        let names: Vec<String> = unknown.iter().map(|it| format!("{prefix}{it}")).collect();
        let mut fields = T::FIELDS.to_vec();
        fields.sort_unstable();
        return Err(InputError(format!(
            "no field {} (the fields are {})",
            names.join(", "),
            fields.join(", ")
        )));
    }
    T::parse(&Fields { given, prefix })
}

/// The fields of one JSON object, read by name.
pub struct Fields<'a> {
    given: &'a Map<String, Value>,
    prefix: String,
}

impl Fields<'_> {
    /// The field's value when it was given.
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.given.get(name)
    }

    /// The field's name with the path to it: `engine.blaze`.
    pub fn at(&self, name: &str) -> String {
        format!("{}{name}", self.prefix)
    }

    fn required<T>(&self, name: &str, default: Option<T>) -> Result<T, InputError> {
        default.ok_or_else(|| InputError(format!("field {} is required", self.at(name))))
    }

    fn not_a(&self, name: &str, kind: &str, value: &Value) -> InputError {
        InputError(format!("{} is a {kind}, not {}", self.at(name), dumps(value)))
    }

    /// A `str` field; `default: None` makes it required.
    pub fn str(&self, name: &str, default: Option<&str>) -> Result<String, InputError> {
        match self.get(name) {
            None => self.required(name, default.map(str::to_owned)),
            Some(Value::String(it)) => Ok(it.clone()),
            Some(other) => Err(self.not_a(name, "str", other)),
        }
    }

    /// An `int` field. A bool is not an int, and neither is `1.0`.
    pub fn int(&self, name: &str, default: Option<i64>) -> Result<i64, InputError> {
        match self.get(name) {
            None => self.required(name, default),
            Some(Value::Number(n)) if n.is_i64() => Ok(n.as_i64().unwrap_or(0)),
            Some(other) => Err(self.not_a(name, "int", other)),
        }
    }

    /// A `float` field. JSON's `1` is not a float's `1.0`.
    pub fn float(&self, name: &str, default: Option<f64>) -> Result<f64, InputError> {
        match self.get(name) {
            None => self.required(name, default),
            Some(Value::Number(n)) if n.is_f64() => Ok(n.as_f64().unwrap_or(0.0)),
            Some(other) => Err(self.not_a(name, "float", other)),
        }
    }

    /// A `bool` field.
    pub fn bool(&self, name: &str, default: Option<bool>) -> Result<bool, InputError> {
        match self.get(name) {
            None => self.required(name, default),
            Some(Value::Bool(it)) => Ok(*it),
            Some(other) => Err(self.not_a(name, "bool", other)),
        }
    }

    /// A `Literal[...]` of strings: one of `allowed`.
    pub fn one_of(&self, name: &str, allowed: &[&str], default: Option<&str>) -> Result<String, InputError> {
        match self.get(name) {
            None => self.required(name, default.map(str::to_owned)),
            Some(Value::String(it)) if allowed.contains(&it.as_str()) => Ok(it.clone()),
            Some(other) => Err(self.not_one_of(name, allowed, other)),
        }
    }

    fn not_one_of(&self, name: &str, allowed: &[&str], value: &Value) -> InputError {
        let listed: Vec<String> = allowed.iter().map(|it| dumps(&Value::String((*it).to_owned()))).collect();
        InputError(format!("{} is one of {}, not {}", self.at(name), listed.join(", "), dumps(value)))
    }

    /// A `Literal[...] | None` of strings whose default is `None`.
    pub fn one_of_or_null(&self, name: &str, allowed: &[&str]) -> Result<Option<String>, InputError> {
        match self.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(it)) if allowed.contains(&it.as_str()) => Ok(Some(it.clone())),
            Some(other) => Err(InputError(format!(
                "{} or {} is null, not {}",
                self.not_one_of(name, allowed, other).0,
                self.at(name),
                dumps(other)
            ))),
        }
    }

    /// A `str | None` field whose default is `None`.
    pub fn str_or_null(&self, name: &str) -> Result<Option<String>, InputError> {
        match self.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(it)) => Ok(Some(it.clone())),
            Some(other) => Err(InputError(format!(
                "{} or {} is null, not {}",
                self.not_a(name, "str", other).0,
                self.at(name),
                dumps(other)
            ))),
        }
    }

    /// A `tuple[str, ...]` field, empty when it was not given.
    pub fn strs(&self, name: &str) -> Result<Vec<String>, InputError> {
        let Some(value) = self.get(name) else { return Ok(Vec::new()) };
        let Value::Array(items) = value else { return Err(self.not_a(name, "list", value)) };
        items
            .iter()
            .enumerate()
            .map(|(i, item)| match item {
                Value::String(it) => Ok(it.clone()),
                other => Err(InputError(format!("{}[{i}] is a str, not {}", self.at(name), dumps(other)))),
            })
            .collect()
    }

    /// A required `tuple[str, ...]` field.
    pub fn strs_required(&self, name: &str) -> Result<Vec<String>, InputError> {
        match self.get(name) {
            None => self.required(name, None),
            Some(_) => self.strs(name),
        }
    }

    /// A `tuple[Literal[...], ...]` field of strings, `default` when it was not given.
    pub fn ones_of(&self, name: &str, allowed: &[&str], default: &[&str]) -> Result<Vec<String>, InputError> {
        let Some(value) = self.get(name) else { return Ok(default.iter().map(|it| (*it).to_owned()).collect()) };
        let Value::Array(items) = value else { return Err(self.not_a(name, "list", value)) };
        items
            .iter()
            .enumerate()
            .map(|(i, item)| match item {
                Value::String(it) if allowed.contains(&it.as_str()) => Ok(it.clone()),
                other => Err(self.not_one_of(&format!("{name}[{i}]"), allowed, other)),
            })
            .collect()
    }

    /// A `tuple[T, ...]` field of objects, each read into `T`; `required` refuses its absence,
    /// and otherwise it is empty when not given.
    pub fn objects<T: Input>(&self, name: &str, required: bool) -> Result<Vec<T>, InputError> {
        let Some(value) = self.get(name) else {
            return if required { self.required(name, None) } else { Ok(Vec::new()) };
        };
        let Value::Array(items) = value else { return Err(self.not_a(name, "list", value)) };
        items.iter().enumerate().map(|(i, item)| of::<T>(item, &format!("{}[{i}]", self.at(name)))).collect()
    }

    /// An `Any` field: the value as given, null when it was not.
    pub fn any(&self, name: &str) -> Value {
        self.get(name).cloned().unwrap_or(Value::Null)
    }

    /// A field that is itself an object read into `T`, required: its absence and a null are
    /// refused.
    pub fn object_required<T: Input>(&self, name: &str) -> Result<T, InputError> {
        match self.get(name) {
            None => self.required(name, None),
            Some(value) => of::<T>(value, &self.at(name)),
        }
    }

    /// A field that is itself an object read into `T`, `None` when it was not given or is null
    /// (`T | None = None`); `required` refuses its absence.
    pub fn object<T: Input>(&self, name: &str, required: bool) -> Result<Option<T>, InputError> {
        match self.get(name) {
            None if required => Err(InputError(format!("field {} is required", self.at(name)))),
            None => Ok(None),
            Some(Value::Null) if !required => Ok(None),
            Some(value) if required => of::<T>(value, &self.at(name)).map(Some),
            // `T | None`: Python tries each member and joins what each said.
            Some(value) => of::<T>(value, &self.at(name)).map(Some).map_err(|InputError(said)| {
                InputError(format!("{said} or {} is null, not {}", self.at(name), dumps(value)))
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[derive(Debug, PartialEq)]
    struct Inner {
        blaze: String,
    }

    impl Input for Inner {
        const FIELDS: &'static [&'static str] = &["blaze"];

        fn parse(f: &Fields<'_>) -> Result<Inner, InputError> {
            Ok(Inner { blaze: f.str("blaze", None)? })
        }
    }

    #[derive(Debug, PartialEq)]
    struct Run {
        suite: String,
        timeout_minutes: i64,
        ids: Vec<String>,
        engine: Option<Inner>,
        only: Option<String>,
    }

    impl Input for Run {
        const FIELDS: &'static [&'static str] = &["suite", "timeout_minutes", "ids", "engine", "only"];

        fn parse(f: &Fields<'_>) -> Result<Run, InputError> {
            Ok(Run {
                suite: f.one_of("suite", &["ruff-and-ty", "uv"], None)?,
                timeout_minutes: f.int("timeout_minutes", Some(300))?,
                ids: f.strs("ids")?,
                engine: f.object("engine", false)?,
                only: f.str_or_null("only")?,
            })
        }
    }

    fn refused(value: &Value) -> String {
        of::<Run>(value, "").unwrap_err().0
    }

    // Each expected line is what tools/typed-input printed for the same value (2026-11-11).
    #[test]
    fn an_input_is_read_or_refused_naming_the_field() {
        let run = of::<Run>(&json!({"suite": "uv", "ids": ["a"], "engine": {"blaze": "b"}}), "").unwrap();
        assert_eq!(
            run,
            Run {
                suite: "uv".to_owned(),
                timeout_minutes: 300,
                ids: vec!["a".to_owned()],
                engine: Some(Inner { blaze: "b".to_owned() }),
                only: None
            }
        );
        assert_eq!(refused(&json!([1])), "the input is a JSON object, not [1]");
        assert_eq!(
            refused(&json!({"suite": "uv", "zeta": 1, "alpha": 2})),
            "no field alpha, zeta (the fields are engine, ids, only, suite, timeout_minutes)"
        );
        assert_eq!(refused(&json!({})), "field suite is required");
        assert_eq!(refused(&json!({"suite": "pip"})), r#"suite is one of "ruff-and-ty", "uv", not "pip""#);
        assert_eq!(refused(&json!({"suite": "uv", "timeout_minutes": true})), "timeout_minutes is a int, not true");
        assert_eq!(refused(&json!({"suite": "uv", "timeout_minutes": 1.5})), "timeout_minutes is a int, not 1.5");
        assert_eq!(refused(&json!({"suite": "uv", "ids": "a"})), r#"ids is a list, not "a""#);
        assert_eq!(refused(&json!({"suite": "uv", "ids": ["a", 2]})), "ids[1] is a str, not 2");
        assert_eq!(
            refused(&json!({"suite": "uv", "engine": {"blaze": 1}})),
            r#"engine.blaze is a str, not 1 or engine is null, not {"blaze": 1}"#
        );
        assert_eq!(
            refused(&json!({"suite": "uv", "engine": {"x": 1}})),
            r#"no field engine.x (the fields are blaze) or engine is null, not {"x": 1}"#
        );
        assert_eq!(refused(&json!({"suite": "uv", "only": 3})), "only is a str, not 3 or only is null, not 3");
    }
}
