//! Strings and keys in the TOML spelling cargo 1.99 prints them with (`toml_edit`'s default
//! representation, which is `toml_writer`'s `TomlStringBuilder::as_default` and
//! `TomlKeyBuilder::as_default`): a basic string when it needs no escape, else a literal one,
//! else a multi-line form, else an escaped basic string. `cargo config get` and a key's
//! display print through these, and `tests/config_get.rs` pins the bytes.
//!
//! blaze-toml's writer always writes a basic string; when it gains these choices, this module
//! goes and its callers use `blaze_toml::write_inline` / `write_key`.

use std::fmt::Write as _;

/// What a string holds that decides its form.
struct Metrics {
    most_single_quotes: usize,
    most_double_quotes: usize,
    /// A control character other than tab and newline, which only an escape can say.
    escape_codes: bool,
    /// A backslash.
    escape: bool,
    newline: bool,
}

impl Metrics {
    fn of(s: &str) -> Metrics {
        let mut m = Metrics {
            most_single_quotes: 0,
            most_double_quotes: 0,
            escape_codes: false,
            escape: false,
            newline: false,
        };
        let (mut single, mut double) = (0usize, 0usize);
        for &b in s.as_bytes() {
            single = if b == b'\'' { single + 1 } else { 0 };
            double = if b == b'"' { double + 1 } else { 0 };
            m.most_single_quotes = m.most_single_quotes.max(single);
            m.most_double_quotes = m.most_double_quotes.max(double);
            match b {
                b'\\' => m.escape = true,
                b'\t' => {}
                b'\n' => m.newline = true,
                c if c <= 0x1f || c == 0x7f => m.escape_codes = true,
                _ => {}
            }
        }
        m
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Form {
    Literal,
    Basic,
    MlLiteral,
    MlBasic,
}

/// `s` as a TOML string value, appended to `out`.
pub(crate) fn write_string(s: &str, out: &mut String) {
    let m = Metrics::of(s);
    let form = if !m.escape_codes && !m.escape && m.most_double_quotes == 0 && !m.newline {
        Form::Basic
    } else if !m.escape_codes && m.most_single_quotes == 0 && !m.newline {
        Form::Literal
    } else if !m.escape_codes && !m.escape && m.most_double_quotes <= 2 {
        Form::MlBasic
    } else if !m.escape_codes && m.most_single_quotes <= 2 {
        Form::MlLiteral
    } else if m.newline {
        Form::MlBasic
    } else {
        Form::Basic
    };
    write_form(s, form, m.newline, out);
}

/// `s` as a TOML key, appended to `out`: bare when it can be.
pub(crate) fn write_key(s: &str, out: &mut String) {
    let bare_byte = |b: u8| b.is_ascii_alphanumeric() || b == b'-' || b == b'_';
    if !s.is_empty() && s.bytes().all(bare_byte) {
        out.push_str(s);
        return;
    }
    let (mut single, mut double, mut escape, mut codes) = (false, false, false, false);
    for &b in s.as_bytes() {
        match b {
            b'\'' => single = true,
            b'"' => double = true,
            b'\\' => escape = true,
            b'\t' => {}
            c if c <= 0x1f || c == 0x7f => codes = true,
            _ => {}
        }
    }
    let form = if !codes && !escape && !double {
        Form::Basic
    } else if !codes && !single {
        Form::Literal
    } else {
        Form::Basic
    };
    write_form(s, form, false, out);
}

fn write_form(s: &str, form: Form, newline: bool, out: &mut String) {
    let delimiter = match form {
        Form::Literal => "'",
        Form::Basic => "\"",
        Form::MlLiteral => "'''",
        Form::MlBasic => "\"\"\"",
    };
    let multi_line = matches!(form, Form::MlLiteral | Form::MlBasic);
    out.push_str(delimiter);
    if newline && multi_line {
        out.push('\n');
    }
    match form {
        Form::Literal | Form::MlLiteral => out.push_str(s),
        Form::Basic | Form::MlBasic => escape(s, multi_line, out),
    }
    out.push_str(delimiter);
}

/// `s` in a basic string: `"` escaped past the run a multi-line string allows, the short
/// escapes, `\uXXXX` for the other control characters, a newline as it is in a multi-line one.
fn escape(s: &str, multi_line: bool, out: &mut String) {
    let most_quotes = if multi_line { 2 } else { 0 };
    let mut quotes = 0;
    for ch in s.chars() {
        if ch == '"' {
            quotes += 1;
            if quotes > most_quotes {
                out.push_str("\\\"");
                // An escaped quote ends the run, as toml_writer counts it.
                quotes = 0;
            } else {
                out.push('"');
            }
            continue;
        }
        quotes = 0;
        match ch {
            '\u{8}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' if !multi_line => out.push_str("\\n"),
            '\u{c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            '\\' => out.push_str("\\\\"),
            c if c != '\n' && (c <= '\u{1f}' || c == '\u{7f}') => {
                let _ = write!(out, "\\u{:04X}", u32::from(c));
            }
            c => out.push(c),
        }
    }
}
