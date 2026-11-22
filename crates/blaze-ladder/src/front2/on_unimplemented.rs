//! A trait's `#[diagnostic::on_unimplemented(..)]`, read from its text, and its words written
//! out for an unmet bound (D104: `fn main(cx: &Cx, io: Io)` is refused in the words bstd's
//! `Grant` gives, which name what to write instead).
//!
//! The Reference, "The `diagnostic` tool attribute namespace", "The
//! `diagnostic::on_unimplemented` attribute": on a trait, it takes `message = "..."`,
//! `label = "..."` and any number of `note = "..."`; each is a format string where `{Self}` is
//! the type the bound failed for and `{This}` the trait's name, with a generic parameter of the
//! trait by its name. An option given twice: the first is taken (rustc's rule). An option the
//! attribute does not know is the definition's warning in rustc, not the use's: it is passed
//! over here, by name ([`Options::each`] yields it; [`Words::of`] takes only the three).
//!
//! Sans-IO and allocation-free: the options borrow the trait's text, and their words are written
//! into the caller's sink with the escapes of a string literal applied.

use core::fmt;

/// A string literal as written: its body between the quotes, and whether it is raw (no escapes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lit<'t> {
    pub body: &'t str,
    pub raw: bool,
}

/// Why a trait's `#[diagnostic::on_unimplemented]` does not read, at a byte of the item's text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadError {
    /// An attribute, a string or a comment is not closed before the item's text ends.
    Unclosed { at: u32, what: &'static str },
    /// An option is not `name = "string"`.
    NotAnOption { at: u32 },
    /// A `\` escape the Reference does not have ("Tokens", "Character escapes").
    Escape { at: u32 },
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            ReadError::Unclosed { at, what } => write!(f, "its {what} at byte {at} is not closed"),
            ReadError::NotAnOption { at } => write!(f, "at byte {at}: an option is `message`, `label` or `note` = \"a string\""),
            ReadError::Escape { at } => write!(f, "at byte {at}: an escape a string literal does not have"),
        }
    }
}

impl core::error::Error for ReadError {}

/// The options of one `#[diagnostic::on_unimplemented(..)]`: the text inside its parentheses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options<'t> {
    args: &'t str,
    /// `args`' first byte in the item's text, for the errors' places.
    from: u32,
}

impl<'t> Options<'t> {
    /// Each option, `(name, value)`, in written order; the first malformed one is the error.
    pub fn each(&self, mut f: impl FnMut(&'t str, Lit<'t>)) -> Result<(), ReadError> {
        let b = self.args.as_bytes();
        let mut i = 0;
        loop {
            i = skip_ws(b, i);
            if i == b.len() {
                return Ok(());
            }
            let at = self.from + i as u32;
            let name_end = ident_end(b, i).ok_or(ReadError::NotAnOption { at })?;
            let name = &self.args[i..name_end];
            i = skip_ws(b, name_end);
            if b.get(i) != Some(&b'=') {
                return Err(ReadError::NotAnOption { at });
            }
            i = skip_ws(b, i + 1);
            let (lit, end) = string_at(self.args, i, self.from)?.ok_or(ReadError::NotAnOption { at })?;
            f(name, lit);
            i = skip_ws(b, end);
            match b.get(i) {
                None => return Ok(()),
                Some(b',') => i += 1,
                Some(_) => return Err(ReadError::NotAnOption { at: self.from + i as u32 }),
            }
        }
    }
}

/// The three options a refusal shows: the first `message`, the first `label`, and the notes.
#[derive(Clone, Copy, Debug)]
pub struct Words<'t> {
    pub message: Option<Lit<'t>>,
    pub label: Option<Lit<'t>>,
    pub options: Options<'t>,
}

impl<'t> Words<'t> {
    /// The words of `options`.
    pub fn of(options: Options<'t>) -> Result<Words<'t>, ReadError> {
        let (mut message, mut label) = (None, None);
        options.each(|name, lit| match name {
            "message" if message.is_none() => message = Some(lit),
            "label" if label.is_none() => label = Some(lit),
            _ => {}
        })?;
        Ok(Words { message, label, options })
    }

    /// Each `note`, in written order.
    pub fn notes(&self, mut f: impl FnMut(Lit<'t>)) -> Result<(), ReadError> {
        self.options.each(|name, lit| {
            if name == "note" {
                f(lit);
            }
        })
    }
}

/// The `#[diagnostic::on_unimplemented(..)]` among the outer attributes `item` (an item's whole
/// text, attributes first) starts with; `None` when it has none. Comments, doc comments and
/// other attributes before it are passed over; the scan stops at the first token that is not
/// an outer attribute.
pub fn find(item: &str) -> Result<Option<Options<'_>>, ReadError> {
    let b = item.as_bytes();
    let mut i = 0;
    loop {
        i = skip_trivia(b, i)?;
        if b.get(i) != Some(&b'#') {
            return Ok(None);
        }
        let open = skip_ws(b, i + 1);
        if b.get(open) != Some(&b'[') {
            // `#!` (an inner attribute) or anything else: not an outer attribute.
            return Ok(None);
        }
        let close = closing(item, open)?;
        let content = &item[open + 1..close];
        if let Some((args, from)) = on_unimplemented_args(content) {
            return Ok(Some(Options { args, from: (open + 1 + from) as u32 }));
        }
        i = close + 1;
    }
}

/// `value` written into `out` with its escapes applied and its placeholders filled: `{{` and
/// `}}` are braces, `{name}` is `fill(name)`'s answer, or the placeholder as written when
/// `fill` has none.
pub fn write_value<'f>(value: Lit<'_>, at: u32, fill: &dyn Fn(&str) -> Option<&'f str>, out: &mut dyn fmt::Write) -> Result<Result<(), ReadError>, fmt::Error> {
    // A placeholder's name, kept until its `}`: a name longer than this is written as is.
    let mut name = [0u8; 64];
    let mut held: Option<usize> = None;
    let mut brace_close = false;
    let mut each = |c: char| -> fmt::Result {
        if brace_close {
            brace_close = false;
            if c == '}' {
                return out.write_char('}');
            }
            out.write_char('}')?;
        }
        match held {
            Some(len) => {
                if c == '{' && len == 0 {
                    held = None;
                    return out.write_char('{');
                }
                if c == '}' {
                    held = None;
                    let written = core::str::from_utf8(&name[..len]).map_err(|_| fmt::Error)?;
                    return match fill(written) {
                        Some(filled) => out.write_str(filled),
                        None => write!(out, "{{{written}}}"),
                    };
                }
                let mut buf = [0u8; 4];
                let bytes = c.encode_utf8(&mut buf).as_bytes();
                if len + bytes.len() > name.len() {
                    held = None;
                    out.write_char('{')?;
                    out.write_str(core::str::from_utf8(&name[..len]).map_err(|_| fmt::Error)?)?;
                    return out.write_char(c);
                }
                name[len..len + bytes.len()].copy_from_slice(bytes);
                held = Some(len + bytes.len());
                Ok(())
            }
            None if c == '{' => {
                held = Some(0);
                Ok(())
            }
            None if c == '}' => {
                brace_close = true;
                Ok(())
            }
            None => out.write_char(c),
        }
    };
    let read = if value.raw { value.body.chars().try_for_each(&mut each).map(|()| Ok(())) } else { unescape(value.body, at, &mut each) };
    let read = read?;
    // A `{` never closed, or a lone `}` last, is written as it stands.
    if let Some(len) = held {
        out.write_char('{')?;
        out.write_str(core::str::from_utf8(&name[..len]).map_err(|_| fmt::Error)?)?;
    }
    if brace_close {
        out.write_char('}')?;
    }
    Ok(read)
}

/// The body of a non-raw string literal, each character after its escape, into `each`.
fn unescape(body: &str, at: u32, each: &mut dyn FnMut(char) -> fmt::Result) -> Result<Result<(), ReadError>, fmt::Error> {
    let mut chars = body.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c != '\\' {
            each(c)?;
            continue;
        }
        let bad = ReadError::Escape { at: at + i as u32 };
        let Some((_, e)) = chars.next() else { return Ok(Err(bad)) };
        let made = match e {
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            '0' => '\0',
            '\\' => '\\',
            '\'' => '\'',
            '"' => '"',
            '\n' => {
                // A line continuation: the newline and the whitespace after it are skipped.
                while chars.next_if(|&(_, w)| matches!(w, ' ' | '\t' | '\n' | '\r')).is_some() {}
                continue;
            }
            'x' => {
                let hi = chars.next().and_then(|(_, d)| d.to_digit(8));
                let lo = chars.next().and_then(|(_, d)| d.to_digit(16));
                match (hi, lo) {
                    (Some(hi), Some(lo)) => char::from((hi * 16 + lo) as u8),
                    _ => return Ok(Err(bad)),
                }
            }
            'u' => {
                if chars.next().map(|(_, b)| b) != Some('{') {
                    return Ok(Err(bad));
                }
                let mut v: u32 = 0;
                let mut digits = 0;
                loop {
                    match chars.next() {
                        Some((_, '}')) if digits > 0 => break,
                        Some((_, '_')) => {}
                        Some((_, d)) if digits < 6 && d.is_ascii_hexdigit() => {
                            v = v * 16 + d.to_digit(16).unwrap_or(0);
                            digits += 1;
                        }
                        _ => return Ok(Err(bad)),
                    }
                }
                match char::from_u32(v) {
                    Some(c) => c,
                    None => return Ok(Err(bad)),
                }
            }
            _ => return Ok(Err(bad)),
        };
        each(made)?;
    }
    Ok(Ok(()))
}

/// The arguments of `diagnostic::on_unimplemented(..)` when `content` (an attribute's content)
/// is that attribute, with their first byte in `content`. Whitespace may stand between tokens.
fn on_unimplemented_args(content: &str) -> Option<(&str, usize)> {
    let mut rest = content.trim_start();
    for token in ["diagnostic", "::", "on_unimplemented"] {
        rest = rest.strip_prefix(token)?.trim_start();
    }
    let rest = rest.strip_prefix('(')?;
    let inner = rest.trim_end().strip_suffix(')')?;
    Some((inner, content.len() - rest.len()))
}

/// The index of the `]` closing the `[` at `open`, over nested brackets, strings and comments.
fn closing(text: &str, open: usize) -> Result<usize, ReadError> {
    let b = text.as_bytes();
    let mut depth = 0u32;
    let mut i = open;
    while i < b.len() {
        match b[i] {
            b'[' | b'(' | b'{' => depth += 1,
            b']' | b')' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(i);
                }
            }
            b'"' | b'r' => {
                if let Some((_, end)) = string_at(text, i, 0)? {
                    i = end;
                    continue;
                }
            }
            b'/' if matches!(b.get(i + 1), Some(b'/' | b'*')) => {
                i = skip_trivia(b, i)?;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    Err(ReadError::Unclosed { at: open as u32, what: "attribute" })
}

/// The string literal starting at `i` (`"..."`, `r"..."` or `r#"..."#`) and the index after
/// it; `None` when no string starts there (an `r` that begins a name).
fn string_at(text: &str, i: usize, from: u32) -> Result<Option<(Lit<'_>, usize)>, ReadError> {
    let b = text.as_bytes();
    let at = from + i as u32;
    if b.get(i) == Some(&b'"') {
        let mut j = i + 1;
        while j < b.len() {
            match b[j] {
                b'\\' => j += 2,
                b'"' => return Ok(Some((Lit { body: &text[i + 1..j], raw: false }, j + 1))),
                _ => j += 1,
            }
        }
        return Err(ReadError::Unclosed { at, what: "string" });
    }
    if b.get(i) != Some(&b'r') || (i > 0 && is_ident(b[i - 1])) {
        return Ok(None);
    }
    let mut j = i + 1;
    while b.get(j) == Some(&b'#') {
        j += 1;
    }
    let hashes = j - i - 1;
    if b.get(j) != Some(&b'"') {
        return Ok(None);
    }
    let start = j + 1;
    let mut k = start;
    while k < b.len() {
        if b[k] == b'"' && b.len() >= k + 1 + hashes && b[k + 1..k + 1 + hashes].iter().all(|&h| h == b'#') {
            return Ok(Some((Lit { body: &text[start..k], raw: true }, k + 1 + hashes)));
        }
        k += 1;
    }
    Err(ReadError::Unclosed { at, what: "raw string" })
}

/// Past whitespace and comments (line, doc and nested block) from `i`.
fn skip_trivia(b: &[u8], mut i: usize) -> Result<usize, ReadError> {
    loop {
        i = skip_ws(b, i);
        match (b.get(i), b.get(i + 1)) {
            (Some(b'/'), Some(b'/')) => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            (Some(b'/'), Some(b'*')) => {
                let open = i;
                let mut depth = 0u32;
                loop {
                    match (b.get(i), b.get(i + 1)) {
                        (Some(b'/'), Some(b'*')) => {
                            depth += 1;
                            i += 2;
                        }
                        (Some(b'*'), Some(b'/')) => {
                            depth -= 1;
                            i += 2;
                            if depth == 0 {
                                break;
                            }
                        }
                        (Some(_), _) => i += 1,
                        (None, _) => return Err(ReadError::Unclosed { at: open as u32, what: "block comment" }),
                    }
                }
            }
            _ => return Ok(i),
        }
    }
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

const fn is_ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// The end of the name starting at `i`, or `None` when none starts there.
fn ident_end(b: &[u8], i: usize) -> Option<usize> {
    if !b.get(i).is_some_and(|&c| c.is_ascii_alphabetic() || c == b'_') {
        return None;
    }
    let mut j = i;
    while j < b.len() && is_ident(b[j]) {
        j += 1;
    }
    Some(j)
}

#[cfg(test)]
mod tests {
    use blaze_own::fixed::FixedString;

    use super::{Lit, ReadError, Words, find, write_value};

    /// bstd's `Grant`, as its text stands after D104 (shortened note): doc comments before the
    /// attribute, a `\` line continuation inside the note.
    const GRANT: &str = "/// A type `main` may take after `cx`.\n/// `Io` is not one.\n#[diagnostic::on_unimplemented(\n    message = \"`main` cannot take a `{Self}`\",\n    label = \"not a capability `main` may take\",\n    note = \"a whole world (`Io`) cannot be asked for. Write \\\n            `out: Out`, `args: Args` ({{as named}})\"\n)]\npub trait Grant: Sized {}\n";

    fn shown(lit: Lit<'_>) -> FixedString<256> {
        let mut out = FixedString::new();
        let fill = |name: &str| (name == "Self").then_some("Io");
        write_value(lit, 0, &fill, &mut out).expect("256 bytes hold the words").expect("the literal reads");
        out
    }

    #[test]
    fn bstd_grants_words_are_read_and_filled() {
        let options = find(GRANT).expect("it reads").expect("the attribute is found past the doc comments");
        let words = Words::of(options).expect("its options read");
        assert_eq!(shown(words.message.expect("a message")).as_str(), "`main` cannot take a `Io`");
        assert_eq!(shown(words.label.expect("a label")).as_str(), "not a capability `main` may take");
        let mut notes = 0;
        words
            .notes(|n| {
                notes += 1;
                // A planted unescape that kept the line continuation's newline and indent fails here.
                assert_eq!(shown(n).as_str(), "a whole world (`Io`) cannot be asked for. Write `out: Out`, `args: Args` ({as named})");
            })
            .expect("the notes read");
        assert_eq!(notes, 1);
    }

    #[test]
    fn a_trait_without_it_has_none_and_the_scan_stops_at_the_item() {
        assert_eq!(find("#[doc = \"x\"]\n/* a ] in a comment */ pub trait T {}\n#[diagnostic::on_unimplemented(message = \"no\")]"), Ok(None));
        assert_eq!(find("#[diagnostic::on_unimplemented(message = r#\"raw \"{Self}\"\"#)] trait T {}").map(|o| o.map(|o| Words::of(o).map(|w| w.message))), Ok(Some(Ok(Some(Lit { body: "raw \"{Self}\"", raw: true })))));
    }

    #[test]
    fn a_malformed_option_is_named_not_passed_over() {
        let options = find("#[diagnostic::on_unimplemented(message)] trait T {}").expect("the attribute closes").expect("found");
        assert!(matches!(Words::of(options), Err(ReadError::NotAnOption { .. })));
        assert!(matches!(find("#[diagnostic::on_unimplemented(message = \"open)] trait T {}"), Err(ReadError::Unclosed { .. })));
    }
}
