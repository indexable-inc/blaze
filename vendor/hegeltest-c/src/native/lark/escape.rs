//! Literal processing: Lark's `eval_escaping` and the conversion of
//! string, regex and range literals into [`Pattern`]s.

use super::ast::{Pattern, PatternKind};
use super::{GrammarError, GrammarResult};
use alloc::format;
use alloc::string::String;

fn hex_escape(
    chars: &[char],
    i: usize,
    digits: usize,
    literal: &str,
) -> GrammarResult<(char, usize)> {
    let end = i + digits;
    if end > chars.len() || !chars[i..end].iter().all(char::is_ascii_hexdigit) {
        return Err(GrammarError::new(format!(
            "Truncated escape sequence in literal `{literal}`"
        )));
    }
    let hex: String = chars[i..end].iter().collect();
    let value = u32::from_str_radix(&hex, 16)
        .ok()
        .and_then(char::from_u32)
        .ok_or_else(|| {
            GrammarError::new(format!("Invalid escape sequence in literal `{literal}`"))
        })?;
    Ok((value, end))
}

/// Lark's `eval_escaping`: interpret `\n \t \f \r \xHH \uHHHH \UHHHHHHHH`
/// and leave every other backslash sequence (`\d`, `\.`, `\/`, `\\`, …)
/// as it is. Lark implements this by doubling the backslashes it wants to
/// keep, turning `\"` into `"`, and evaluating the result as a Python
/// string literal; this follows the same steps so the corner cases (such
/// as `\\"` collapsing to `\"`) come out the same.
pub fn eval_escaping(s: &str) -> GrammarResult<String> {
    let chars: alloc::vec::Vec<char> = s.chars().collect();
    let mut doubled = String::with_capacity(s.len() * 2);
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        i += 1;
        doubled.push(c);
        if c != '\\' {
            continue;
        }
        let Some(&n) = chars.get(i) else {
            return Err(GrammarError::new(format!(
                "Literal ended unexpectedly (bad escaping): `{s}`"
            )));
        };
        i += 1;
        if n == '\\' {
            doubled.push_str("\\\\");
        } else if !matches!(n, 'U' | 'u' | 'x' | 'n' | 'f' | 't' | 'r') {
            doubled.push('\\');
        }
        doubled.push(n);
    }
    let doubled = doubled.replace("\\\"", "\"");
    python_literal(&doubled, s)
}

/// Evaluate the escapes of a Python string literal body, as
/// `ast.literal_eval` does for the strings [`eval_escaping`] builds.
fn python_literal(body: &str, literal: &str) -> GrammarResult<String> {
    let chars: alloc::vec::Vec<char> = body.chars().collect();
    let mut out = String::with_capacity(body.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        i += 1;
        if c != '\\' {
            out.push(c);
            continue;
        }
        let n = chars[i];
        i += 1;
        match n {
            '\\' => out.push('\\'),
            '"' => out.push('"'),
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'f' => out.push('\x0c'),
            'r' => out.push('\r'),
            'x' => {
                let (ch, next) = hex_escape(&chars, i, 2, literal)?;
                out.push(ch);
                i = next;
            }
            'u' => {
                let (ch, next) = hex_escape(&chars, i, 4, literal)?;
                out.push(ch);
                i = next;
            }
            'U' => {
                let (ch, next) = hex_escape(&chars, i, 8, literal)?;
                out.push(ch);
                i = next;
            }
            _ => unreachable!("eval_escaping doubles every other backslash"),
        }
    }
    Ok(out)
}

/// Lark's `_literal_to_pattern` for a string literal: the body between the
/// quotes and the flags after them.
pub fn string_literal(body: &str, flags: &str) -> GrammarResult<Pattern> {
    if body.contains('\n') {
        return Err(GrammarError::new(
            "You cannot put newlines in string literals",
        ));
    }
    let value = eval_escaping(body)?.replace("\\\\", "\\");
    if value.is_empty() {
        return Err(GrammarError::new(format!(
            "Empty terminals are not allowed (\"{body}\")"
        )));
    }
    Ok(Pattern::str(value).with_flags(flags))
}

/// Lark's `_literal_to_pattern` for a regex literal.
pub fn regex_literal(body: &str, flags: &str) -> GrammarResult<Pattern> {
    if body.contains('\n') && !flags.contains('x') {
        return Err(GrammarError::new(
            "You can only use newlines in regular expressions with the `x` (verbose) flag",
        ));
    }
    let value = eval_escaping(body)?;
    if value.is_empty() {
        return Err(GrammarError::new(format!(
            "Empty terminals are not allowed (/{body}/)"
        )));
    }
    Ok(Pattern::re(value).with_flags(flags))
}

/// Lark's `PrepareLiterals.range`: `"a".."z"` becomes the character class
/// `[a-z]`, built from the literals' raw text so escapes like `\x00` stay
/// escapes inside the class.
pub fn range_literal(start: &str, end: &str) -> GrammarResult<Pattern> {
    for body in [start, end] {
        let value = eval_escaping(body)?.replace("\\\\", "\\");
        if value.chars().count() != 1 {
            return Err(GrammarError::new(format!(
                "Ranges must be between single characters, got \"{body}\""
            )));
        }
    }
    Ok(Pattern::re(format!("[{start}-{end}]")))
}

/// Escape a string for use in a regex, as Python's `re.escape` does: only
/// the characters in its `_special_chars_map` (those special somewhere in
/// its regex syntax, plus whitespace, `#`, `&`, `~` and `-`) are
/// backslash-escaped, and every other character is left as it is. The
/// result is matched by the engine's own regex parser, which follows
/// Python's.
pub fn re_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for c in s.chars() {
        if matches!(
            c,
            '.' | '^'
                | '$'
                | '*'
                | '+'
                | '?'
                | '{'
                | '}'
                | '['
                | ']'
                | '\\'
                | '|'
                | '('
                | ')'
                | '#'
                | '&'
                | '~'
                | '-'
                | ' '
        ) {
            out.push('\\');
            out.push(c);
        } else if c == '\t' {
            out.push_str("\\\t");
        } else if c == '\n' {
            out.push_str("\\\n");
        } else if c == '\r' {
            out.push_str("\\\r");
        } else if c == '\x0b' {
            out.push_str("\\\x0b");
        } else if c == '\x0c' {
            out.push_str("\\\x0c");
        } else {
            out.push(c);
        }
    }
    out
}

/// Lark's `Pattern.to_regexp`: the pattern as a regex, with each flag
/// applied as a scoped inline group (`(?i:...)`).
pub fn pattern_to_regexp(pattern: &Pattern) -> String {
    let mut value = match pattern.kind {
        PatternKind::Str => re_escape(&pattern.value),
        PatternKind::Re => pattern.value.clone(),
    };
    for flag in pattern.flags.chars() {
        value = format!("(?{flag}:{value})");
    }
    value
}

#[cfg(test)]
#[path = "../../../tests/embedded/native/lark/escape_tests.rs"]
mod tests;
