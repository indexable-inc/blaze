//! Macros: `.macro`, `.rept`, `.irp` and `.irpc`.
//!
//! Expansion is **textual**, and deliberately so. A token-level substitution
//! cannot express what macro bodies routinely rely on: `.L\@_loop:` has to
//! paste the invocation counter into the middle of an identifier, and there is
//! no token that means "join these". Substituting into the source text and
//! re-lexing gives that for free, and it is what GNU as does, so bodies
//! written for it behave the same way here.
//!
//! The cost is that the expansion is a new entry in the source map. That turns
//! out to be a feature: a diagnostic inside a macro points at the expanded
//! text, with the file name saying which macro it came from.

use crate::intern::Name;
use crate::source::Span;

#[derive(Clone, Debug)]
pub struct MacroParam {
    pub name: String,
    /// Text substituted when the caller omits this argument.
    pub default: Option<String>,
    /// `:req` — omitting it is an error.
    pub required: bool,
    /// `:vararg` — absorbs every remaining argument, commas included.
    pub vararg: bool,
}

#[derive(Clone, Debug)]
pub struct MacroDef {
    pub name: Name,
    pub params: Vec<MacroParam>,
    /// The body as it was written, ready to be substituted into.
    pub body: String,
    pub def_span: Span,
}

impl MacroDef {
    pub fn param(&self, name: &str) -> Option<&MacroParam> {
        self.params.iter().find(|p| p.name == name)
    }
}

/// Substitutes `\param` references in a macro body.
///
/// `bindings` maps parameter names to the text to put in their place. `\@` is
/// replaced with `counter`, `\()` with nothing (it exists purely to end a
/// parameter name that would otherwise run into the following text), and `\\`
/// with a single backslash.
///
/// A `\x` whose name is not a parameter is left exactly as written. That
/// matters: macro bodies contain string literals, and `"a\nb"` must survive
/// unless the caller really did name a parameter `n`.
pub fn substitute(body: &str, bindings: &[(String, String)], counter: u64) -> String {
    substitute_with(body, bindings, counter, false)
}

/// [`substitute`], optionally also replacing the positional `\1` to `\9`
/// that Motorola and Renesas macros use for their arguments.
///
/// Positional references are off for GNU as macros, where `\1` inside a
/// string is an octal escape for byte 1 and must survive expansion untouched.
/// A positional reference to an argument that was not passed expands to
/// nothing, as it does in Devpac.
pub fn substitute_with(
    body: &str,
    bindings: &[(String, String)],
    counter: u64,
    positional: bool,
) -> String {
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(i) = rest.find('\\') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let mut chars = after.chars();
        match chars.next() {
            None => {
                // A trailing backslash: nothing to escape.
                out.push('\\');
                return out;
            }
            Some('\\') => {
                out.push('\\');
                rest = &after[1..];
            }
            Some('@') => {
                out.push_str(&counter.to_string());
                rest = &after[1..];
            }
            Some(d) if positional && d.is_ascii_digit() && d != '0' => {
                let name = d.to_string();
                if let Some((_, value)) = bindings.iter().find(|(p, _)| *p == name) {
                    out.push_str(value);
                }
                rest = &after[1..];
            }
            Some('(') if after.starts_with("()") => {
                // The empty paste: it separates a parameter name from what
                // follows and contributes nothing itself.
                rest = &after[2..];
            }
            Some(c) if is_param_start(c) => {
                let end = after
                    .find(|c: char| !is_param_cont(c))
                    .unwrap_or(after.len());
                let name = &after[..end];
                match bindings.iter().find(|(p, _)| p == name) {
                    Some((_, value)) => {
                        out.push_str(value);
                        rest = &after[end..];
                    }
                    None => {
                        // Not a parameter, so not ours to touch. The name is
                        // copied out too, so the next search does not stop on
                        // this same backslash forever.
                        out.push('\\');
                        out.push_str(name);
                        rest = &after[end..];
                    }
                }
            }
            Some(_) => {
                out.push('\\');
                rest = after;
                // Emit the escaped character too, so a `\"` inside a string
                // literal is not re-examined.
                let n = rest.chars().next().map_or(0, char::len_utf8);
                out.push_str(&rest[..n]);
                rest = &rest[n..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Substitutes a CC-RL or CC-RH macro body, whose parameters are plain words.
///
/// A word is a run of the characters a symbol is made of — letters, digits,
/// `@`, `_`, `.` and, after the first, `$` (CC-RL §5.1.2 (3)(b), page 428;
/// CC-RH §5.1.12, page 423) — and one that names a parameter or a local
/// symbol is replaced whole.
/// `concat` joins two words and disappears, so `LAB?PAR` with `PAR` bound to
/// `1` becomes `LAB1` (CC-RL §5.4.5, page 557; CC-RH §5.4.3, page 489). Both
/// manuals leave string literals and comments alone, and so does this. A
/// `.LOCAL` line has done its job once its names are bound, and is dropped.
///
/// With `quoted`, a single-quoted literal that is exactly a parameter name
/// becomes the argument in single quotes, as CC-RX does (R20UT3248EJ0115
/// page 487, "When a parameter in the body is enclosed within
/// single-quotes").
pub fn substitute_words(
    body: &str,
    bindings: &[(String, String)],
    concat: char,
    quoted: bool,
) -> String {
    // CC-RX's concatenation symbol, `@`, is a name character in CC-RL.
    let is_word =
        |c: char| c != concat && (c.is_ascii_alphanumeric() || matches!(c, '@' | '_' | '.' | '$'));
    let mut out = String::with_capacity(body.len());
    for (n, line) in body.split('\n').enumerate() {
        if n > 0 {
            out.push('\n');
        }
        if local_names(line).is_some() {
            continue;
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            out.push_str(line);
            continue;
        }
        let mut chars = line.char_indices().peekable();
        while let Some((i, c)) = chars.next() {
            // A parameter named inside single quotes is still substituted.
            let quoted_param = if c == '\'' && quoted {
                line[i + 1..].find('\'').and_then(|close| {
                    bindings
                        .iter()
                        .find(|(p, _)| *p == line[i + 1..i + 1 + close])
                        .map(|(_, value)| (close, value))
                })
            } else {
                None
            };
            if let Some((close, value)) = quoted_param {
                out.push('\'');
                out.push_str(value);
                out.push('\'');
                // Skip past the closing quote.
                for (j, _) in chars.by_ref() {
                    if j == i + 1 + close {
                        break;
                    }
                }
                continue;
            }
            match c {
                ';' => {
                    out.push_str(&line[i..]);
                    break;
                }
                '"' | '\'' => {
                    // Copy the literal through its closing quote, skipping
                    // escaped characters.
                    out.push(c);
                    while let Some((_, d)) = chars.next() {
                        out.push(d);
                        if d == '\\' {
                            if let Some((_, e)) = chars.next() {
                                out.push(e);
                            }
                        } else if d == c {
                            break;
                        }
                    }
                }
                _ if c == concat => {}
                // A symbol cannot start with `$`, which is the relative
                // addressing sigil of `BR $label`.
                _ if is_word(c) && c != '$' => {
                    let mut end = i + c.len_utf8();
                    while let Some(&(j, d)) = chars.peek() {
                        if !is_word(d) {
                            break;
                        }
                        end = j + d.len_utf8();
                        chars.next();
                    }
                    let word = &line[i..end];
                    match bindings.iter().find(|(p, _)| p == word) {
                        Some((_, value)) => out.push_str(value),
                        None => out.push_str(word),
                    }
                }
                _ => out.push(c),
            }
        }
    }
    out
}

/// The names every `.LOCAL` line of a CC-RL or CC-RH body declares.
pub fn cc_locals(body: &str) -> Vec<String> {
    body.lines().filter_map(local_names).flatten().collect()
}

/// The names on a `.LOCAL name, name` line, if that is what `line` is. A
/// label may come first (CC-RL page 528).
fn local_names(line: &str) -> Option<Vec<String>> {
    let code = line.split(';').next().unwrap_or("");
    let mut rest = code.trim_start();
    if let Some(colon) = rest.find(':')
        && rest[..colon]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "@_.$".contains(c))
    {
        rest = rest[colon + 1..].trim_start();
    }
    let (word, names) = rest.split_at(rest.find(char::is_whitespace).unwrap_or(rest.len()));
    if !word.eq_ignore_ascii_case(".local") {
        return None;
    }
    Some(
        names
            .split(',')
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
            .collect(),
    )
}

fn is_param_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_param_cont(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '$'
}

/// Parses the parameter list of a `.macro` directive from its source text.
///
/// Accepts `a`, `a=default`, `a:req` and `a:vararg`, separated by commas or
/// whitespace, which is the range GNU as takes.
pub fn parse_params(text: &str) -> Result<Vec<MacroParam>, String> {
    let mut params: Vec<MacroParam> = Vec::new();
    for piece in split_params(text) {
        let piece = piece.trim();
        if piece.is_empty() {
            continue;
        }
        let (name, rest) = split_name(piece);
        if name.is_empty() {
            return Err(format!("`{piece}` is not a valid parameter name"));
        }
        if params.iter().any(|p| p.name == name) {
            return Err(format!("duplicate parameter `{name}`"));
        }
        let mut p = MacroParam {
            name: name.to_string(),
            default: None,
            required: false,
            vararg: false,
        };
        let rest = rest.trim();
        if let Some(d) = rest.strip_prefix('=') {
            p.default = Some(d.trim().to_string());
        } else if let Some(q) = rest.strip_prefix(':') {
            match q.trim() {
                "req" => p.required = true,
                "vararg" => p.vararg = true,
                other => return Err(format!("unknown parameter qualifier `:{other}`")),
            }
        } else if !rest.is_empty() {
            return Err(format!("unexpected `{rest}` after parameter `{name}`"));
        }
        if params.last().is_some_and(|prev| prev.vararg) {
            return Err(format!("`{name}` follows a `:vararg` parameter"));
        }
        params.push(p);
    }
    Ok(params)
}

fn split_name(s: &str) -> (&str, &str) {
    let end = s.find(|c: char| !is_param_cont(c)).unwrap_or(s.len());
    s.split_at(end)
}

/// Splits a parameter list on commas and whitespace, but not inside a default
/// value's brackets or quotes.
fn split_params(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut quote = None::<char>;
    let mut start = 0usize;
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i] as char;
        match quote {
            Some(q) => {
                if c == '\\' {
                    i += 1;
                } else if c == q {
                    quote = None;
                }
            }
            None => match c {
                '"' | '\'' => quote = Some(c),
                '(' | '[' | '<' => depth += 1,
                ')' | ']' | '>' => depth -= 1,
                ',' if depth <= 0 => {
                    out.push(&text[start..i]);
                    start = i + 1;
                }
                _ => {}
            },
        }
        i += 1;
    }
    out.push(&text[start..]);
    out
}

/// Splits an argument list the same way, used for both macro calls and `.irp`.
pub fn split_args(text: &str) -> Vec<&str> {
    split_params(text)
        .into_iter()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}

/// Splits `name rest` or `name, rest` into the leading identifier and what
/// follows. Used for both `.macro NAME params` and `.irp VAR, values`.
pub fn split_macro_header(text: &str) -> (&str, &str) {
    let text = text.trim_start();
    let (name, rest) = split_name(text);
    (name, rest.trim_start().trim_start_matches(',').trim_start())
}

/// Recognises a `name=value` argument, which lets a caller pass arguments out
/// of order. The `=` has to be at the top level: `f(a=1)` is one positional
/// argument, not a named one.
pub fn split_named_arg(arg: &str) -> Option<(&str, &str)> {
    let (name, rest) = split_name(arg.trim_start());
    if name.is_empty() {
        return None;
    }
    let rest = rest.trim_start();
    let value = rest.strip_prefix('=')?;
    // `==` is a comparison in the argument, not an assignment.
    if value.starts_with('=') {
        return None;
    }
    Some((name, value.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(a, v)| (a.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn substitutes_named_parameters() {
        let s = substitute(
            "mov \\dst, \\src",
            &b(&[("dst", "%rax"), ("src", "%rbx")]),
            0,
        );
        assert_eq!(s, "mov %rax, %rbx");
    }

    #[test]
    fn pastes_into_the_middle_of_an_identifier() {
        // The reason expansion is textual: there is no token that means
        // "join these two".
        let s = substitute(".L\\@_loop:", &[], 7);
        assert_eq!(s, ".L7_loop:");
        let s = substitute("\\name\\()_end:", &b(&[("name", "foo")]), 0);
        assert_eq!(s, "foo_end:");
    }

    #[test]
    fn leaves_unknown_escapes_alone() {
        // A macro body full of string literals must survive expansion.
        let s = substitute(r#".ascii "a\nb\tc""#, &b(&[("x", "1")]), 0);
        assert_eq!(s, r#".ascii "a\nb\tc""#);
        // The name scan is greedy, so `\nb` looks for a parameter called
        // `nb`, not `n`. That is why `\()` exists.
        let s = substitute(r#".ascii "a\nb""#, &b(&[("n", "Z")]), 0);
        assert_eq!(s, r#".ascii "a\nb""#);
        let s = substitute(r#".ascii "a\n\()b""#, &b(&[("n", "Z")]), 0);
        assert_eq!(s, r#".ascii "aZb""#);
    }

    #[test]
    fn a_parameter_name_can_still_capture_a_string_escape() {
        // GNU as has this wart and bodies are written around it, so matching
        // it is more useful than being clever: with a parameter called `n`,
        // a `\n` that ends a string really is substituted.
        let s = substitute(r#".ascii "a\n""#, &b(&[("n", "Z")]), 0);
        assert_eq!(s, r#".ascii "aZ""#);
    }

    #[test]
    fn positional_arguments_are_opt_in() {
        let args = b(&[("1", "d0"), ("2", "d1")]);
        assert_eq!(
            substitute_with(r" move.l \1,\2", &args, 0, true),
            " move.l d0,d1"
        );
        // A missing argument expands to nothing.
        assert_eq!(substitute_with(r" dc.b \3", &args, 0, true), " dc.b ");
        // Off for GNU as, where `\1` in a string is an octal escape.
        assert_eq!(substitute(r#".ascii "\1""#, &args, 0), r#".ascii "\1""#);
    }

    #[test]
    fn escaped_backslash_and_trailing_backslash() {
        assert_eq!(substitute(r"a\\b", &[], 0), r"a\b");
        assert_eq!(substitute(r"a\", &[], 0), r"a\");
    }

    #[test]
    fn parses_parameter_forms() {
        let p = parse_params("a, b=2, c:req, rest:vararg").unwrap();
        assert_eq!(p.len(), 4);
        assert_eq!(p[0].name, "a");
        assert_eq!(p[1].default.as_deref(), Some("2"));
        assert!(p[2].required);
        assert!(p[3].vararg);
    }

    #[test]
    fn rejects_bad_parameter_lists() {
        assert!(parse_params("a, a").unwrap_err().contains("duplicate"));
        assert!(parse_params("a:nope").unwrap_err().contains("qualifier"));
        assert!(parse_params("v:vararg, a").unwrap_err().contains("vararg"));
    }

    #[test]
    fn splits_headers_and_named_arguments() {
        assert_eq!(split_macro_header("foo a, b"), ("foo", "a, b"));
        assert_eq!(split_macro_header("foo, a"), ("foo", "a"));
        assert_eq!(split_macro_header("  foo  "), ("foo", ""));
        assert_eq!(split_named_arg("dst=%rax"), Some(("dst", "%rax")));
        assert_eq!(split_named_arg(" n = 4 "), Some(("n", "4")));
        assert_eq!(split_named_arg("%rax"), None);
        // A comparison is not an assignment.
        assert_eq!(split_named_arg("a==b"), None);
    }

    #[test]
    fn splits_arguments_without_breaking_nesting() {
        assert_eq!(split_args("1, 2, 3"), vec!["1", "2", "3"]);
        assert_eq!(split_args("(1, 2), 3"), vec!["(1, 2)", "3"]);
        assert_eq!(split_args(r#""a,b", c"#), vec![r#""a,b""#, "c"]);
        assert_eq!(split_args("  "), Vec::<&str>::new());
    }
}
