//! Python's `str` methods whose rules differ from Rust's, so a port reads text as the Python
//! script read it: `str.splitlines` (eleven line boundaries where `str::lines` has two) and
//! `str.isspace`, which `strip` and friends use (it counts `\x1c`..`\x1f`, which
//! `char::is_whitespace` does not).

/// Python's `str.isspace` of one character.
pub fn is_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// Python's `\w` in a str pattern: a letter or digit of any script, or `_` (close to Python's
/// `str.isalnum`, which also counts the numeric characters that are not digits).
pub fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// `s.strip()`.
pub fn strip(s: &str) -> &str {
    s.trim_matches(is_space)
}

/// `s.lstrip()`.
pub fn lstrip(s: &str) -> &str {
    s.trim_start_matches(is_space)
}

/// `s.rstrip()`.
pub fn rstrip(s: &str) -> &str {
    s.trim_end_matches(is_space)
}

/// Whether `c` ends a line for `str.splitlines`.
fn is_boundary(c: char) -> bool {
    matches!(
        c,
        '\n' | '\r' | '\u{b}' | '\u{c}' | '\u{1c}' | '\u{1d}' | '\u{1e}' | '\u{85}' | '\u{2028}' | '\u{2029}'
    )
}

/// `s.splitlines()`: the lines without their ends; `\r\n` is one end, and a last end makes no
/// empty line after it.
pub fn splitlines(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut chars = s.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        if !is_boundary(c) {
            continue;
        }
        out.push(&s[start..at]);
        start = at + c.len_utf8();
        if c == '\r' && chars.peek().is_some_and(|(_, next)| *next == '\n') {
            chars.next();
            start += 1;
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// `s.split()`: the runs of characters between Python's whitespace.
pub fn split_whitespace(s: &str) -> Vec<&str> {
    s.split(is_space).filter(|it| !it.is_empty()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Each expectation is what Python 3.13 gives for the same string.
    #[test]
    fn word_characters_are_pythons() {
        assert!(is_word('a') && is_word('_') && is_word('9') && is_word('é') && is_word('名'));
        assert!(!is_word('.') && !is_word('\'') && !is_word(' ') && !is_word('-'));
    }

    #[test]
    fn lines_and_spaces_are_pythons() {
        assert_eq!(splitlines("a\nb\r\nc\rd\u{b}e\u{85}f\u{2028}g\n"), ["a", "b", "c", "d", "e", "f", "g"]);
        assert_eq!(splitlines(""), Vec::<&str>::new());
        assert_eq!(splitlines("\n"), [""]);
        assert_eq!(splitlines("a\n\nb"), ["a", "", "b"]);
        assert_eq!(splitlines("a\r"), ["a"]);
        assert_eq!(strip("\u{1f} a \u{a0}\n"), "a");
        assert_eq!(lstrip("  #[x] "), "#[x] ");
        assert_eq!(rstrip("x \t"), "x");
        assert_eq!(split_whitespace(" a  b\u{1c}c\n"), ["a", "b", "c"]);
    }
}
