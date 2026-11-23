//! Pure `#![no_std]` `.env` parser with zero ambient environment access.
//!
//! - Zero-copy [`DotenvValue::Borrowed`] for unquoted, single-quoted, and escape-free
//!   double-quoted values; allocates [`DotenvValue::Owned`] in the caller's allocator only when
//!   double-quoted escape sequences are expanded.
//! - Supports optional `export ` prefixes, strict POSIX key validation, single-quoted literal
//!   (including multiline) values, double-quoted values with `\n`, `\r`, `\t`, `\"`, `\\`, `\$`,
//!   trailing whitespace trimming and inline ` # comment` stripping on unquoted values, CRLF/LF,
//!   and `#` comments.
//! - Zero `unsafe`, zero integer `as` casts, zero `.clone()`, zero panicking slice indexing.

use core::alloc::Allocator;
use core::fmt;

use blaze_own::{AllocFailure, String, Vec};

/// Bounds enforced while parsing untrusted `.env` text.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DotenvLimits {
    /// Maximum input length in bytes.
    pub most_bytes: usize,
    /// Maximum number of parsed entries.
    pub most_entries: usize,
}

impl DotenvLimits {
    /// Generous default bounds (4 MiB, 65,536 entries).
    pub const USUAL: Self = Self {
        most_bytes: 4 << 20,
        most_entries: 65_536,
    };

    /// Alias matching `blaze_json::Limits::SHIPPED`.
    pub const SHIPPED: Self = Self::USUAL;
}

/// A parsed `.env` value: zero-copy when no escape expansion was needed.
pub enum DotenvValue<'a, A: Allocator> {
    /// Borrowed directly from the input text.
    Borrowed(&'a str),
    /// Unescaped into the caller's allocator.
    Owned(String<A>),
}

impl<A: Allocator> fmt::Debug for DotenvValue<'_, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DotenvValue::Borrowed(s) => f.debug_tuple("Borrowed").field(s).finish(),
            DotenvValue::Owned(s) => f.debug_tuple("Owned").field(&s.as_str()).finish(),
        }
    }
}

impl<A: Allocator> PartialEq for DotenvValue<'_, A> {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (DotenvValue::Borrowed(a), DotenvValue::Borrowed(b)) => a == b,
            (DotenvValue::Owned(a), DotenvValue::Owned(b)) => a.as_str() == b.as_str(),
            _ => false,
        }
    }
}

impl<A: Allocator> Eq for DotenvValue<'_, A> {}

impl<A: Allocator> DotenvValue<'_, A> {
    /// View the parsed value as a string slice.
    #[inline]
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            DotenvValue::Borrowed(s) => s,
            DotenvValue::Owned(s) => s.as_str(),
        }
    }
}

/// A single key/value assignment from a `.env` file.
pub struct Entry<'a, A: Allocator> {
    /// Validated POSIX environment variable name.
    pub key: &'a str,
    /// Parsed value.
    pub value: DotenvValue<'a, A>,
    /// 1-based line number where the assignment began.
    pub line: usize,
}

impl<A: Allocator> fmt::Debug for Entry<'_, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Entry")
            .field("key", &self.key)
            .field("value", &self.value)
            .field("line", &self.line)
            .finish()
    }
}

impl<A: Allocator> PartialEq for Entry<'_, A> {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key && self.value == other.value && self.line == other.line
    }
}

impl<A: Allocator> Eq for Entry<'_, A> {}

/// Why parsing a `.env` file failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DotenvError {
    /// 1-based line number of the error.
    pub line: usize,
    /// Byte offset in the input text.
    pub offset: usize,
    /// Specific failure reason.
    pub why: DotenvWhy,
}

/// Failure category for [`DotenvError`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DotenvWhy {
    InputTooLarge { bytes: usize, limit: usize },
    TooManyEntries { limit: usize },
    MissingEquals,
    InvalidKey,
    UnclosedSingleQuote,
    UnclosedDoubleQuote,
    InvalidEscape { byte: u8 },
    TrailingContent { byte: u8 },
    Alloc(AllocFailure),
}

impl fmt::Display for DotenvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bstd::dotenv: line {} (byte {}): ", self.line, self.offset)?;
        match self.why {
            DotenvWhy::InputTooLarge { bytes, limit } => {
                write!(f, "input size {bytes} bytes exceeds limit {limit}")
            }
            DotenvWhy::TooManyEntries { limit } => {
                write!(f, "entry count exceeds limit {limit}")
            }
            DotenvWhy::MissingEquals => f.write_str("expected `=` after variable name"),
            DotenvWhy::InvalidKey => f.write_str("invalid POSIX environment variable name"),
            DotenvWhy::UnclosedSingleQuote => f.write_str("unterminated single-quoted value"),
            DotenvWhy::UnclosedDoubleQuote => f.write_str("unterminated double-quoted value"),
            DotenvWhy::InvalidEscape { byte } => {
                write!(f, "unsupported escape sequence `\\` followed by byte 0x{byte:02x}")
            }
            DotenvWhy::TrailingContent { byte } => {
                write!(f, "unexpected byte 0x{byte:02x} after closing quote")
            }
            DotenvWhy::Alloc(err) => write!(f, "allocation failed ({err})"),
        }
    }
}

impl core::error::Error for DotenvError {}

/// Parse `.env` text into entries allocated in `alloc`.
pub fn parse_in<'a, A: Allocator + Copy>(
    input: &'a str,
    limits: DotenvLimits,
    alloc: A,
) -> Result<Vec<Entry<'a, A>, A>, DotenvError> {
    parse_in_planted(input, limits, alloc, false)
}

fn parse_in_planted<'a, A: Allocator + Copy>(
    input: &'a str,
    limits: DotenvLimits,
    alloc: A,
    plant_skip_escape: bool,
) -> Result<Vec<Entry<'a, A>, A>, DotenvError> {
    if input.len() > limits.most_bytes {
        return Err(DotenvError {
            line: 1,
            offset: 0,
            why: DotenvWhy::InputTooLarge { bytes: input.len(), limit: limits.most_bytes },
        });
    }

    let bytes = input.as_bytes();
    let mut i = if input.starts_with('\u{FEFF}') { 3usize } else { 0usize };
    let mut line = 1usize;
    let mut entries = Vec::new_in(alloc);

    while i < bytes.len() {
        skip_hspace(bytes, &mut i);
        match bytes.get(i) {
            None => break,
            Some(&b'\n') => {
                i += 1;
                line += 1;
                continue;
            }
            Some(&b'\r') => {
                i += 1;
                if bytes.get(i) == Some(&b'\n') {
                    i += 1;
                }
                line += 1;
                continue;
            }
            Some(&b'#') => {
                skip_to_eol(bytes, &mut i, &mut line);
                continue;
            }
            Some(_) => {}
        }

        let entry_line = line;

        // Optional `export ` prefix (only when followed by horizontal whitespace).
        if input.get(i..).is_some_and(|rest| rest.starts_with("export"))
            && matches!(bytes.get(i + 6), Some(&b' ' | &b'\t'))
        {
            i += 6;
            skip_hspace(bytes, &mut i);
        }

        let key_start = i;
        while let Some(&b) = bytes.get(i) {
            if matches!(b, b'=' | b'\n' | b'\r' | b' ' | b'\t') {
                break;
            }
            i += 1;
        }
        let key_end = i;
        let key = input.get(key_start..key_end).unwrap_or("");
        if !is_valid_posix_key(key) {
            return Err(DotenvError {
                line: entry_line,
                offset: key_start,
                why: DotenvWhy::InvalidKey,
            });
        }

        skip_hspace(bytes, &mut i);
        if bytes.get(i) != Some(&b'=') {
            return Err(DotenvError {
                line: entry_line,
                offset: i,
                why: DotenvWhy::MissingEquals,
            });
        }
        i += 1; // consume '='

        let before_ws = i;
        skip_hspace(bytes, &mut i);
        let saw_space_after_eq = i > before_ws;

        if entries.len() >= limits.most_entries {
            return Err(DotenvError {
                line: entry_line,
                offset: key_start,
                why: DotenvWhy::TooManyEntries { limit: limits.most_entries },
            });
        }

        let value = match bytes.get(i) {
            Some(&b'\'') => parse_single_quoted(input, bytes, &mut i, &mut line)?,
            Some(&b'"') => {
                parse_double_quoted(input, bytes, &mut i, &mut line, alloc, plant_skip_escape)?
            }
            _ => parse_unquoted(input, bytes, &mut i, &mut line, saw_space_after_eq),
        };

        entries
            .try_push(Entry { key, value, line: entry_line })
            .map_err(|r| DotenvError {
                line: entry_line,
                offset: key_start,
                why: DotenvWhy::Alloc(r.why),
            })?;
    }

    Ok(entries)
}

#[inline]
fn is_valid_posix_key(key: &str) -> bool {
    let mut bs = key.as_bytes().iter();
    let Some(&first) = bs.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return false;
    }
    bs.all(|&b| b.is_ascii_alphanumeric() || b == b'_')
}

#[inline]
fn skip_hspace(bytes: &[u8], i: &mut usize) {
    while matches!(bytes.get(*i), Some(&b' ' | &b'\t')) {
        *i += 1;
    }
}

fn skip_to_eol(bytes: &[u8], i: &mut usize, line: &mut usize) {
    while let Some(&b) = bytes.get(*i) {
        *i += 1;
        if b == b'\n' {
            *line += 1;
            return;
        }
        if b == b'\r' {
            if bytes.get(*i) == Some(&b'\n') {
                *i += 1;
            }
            *line += 1;
            return;
        }
    }
}

fn consume_post_quote_tail(
    bytes: &[u8],
    i: &mut usize,
    line: &mut usize,
) -> Result<(), DotenvError> {
    skip_hspace(bytes, i);
    match bytes.get(*i) {
        None => Ok(()),
        Some(&b'\n') => {
            *i += 1;
            *line += 1;
            Ok(())
        }
        Some(&b'\r') => {
            *i += 1;
            if bytes.get(*i) == Some(&b'\n') {
                *i += 1;
            }
            *line += 1;
            Ok(())
        }
        Some(&b'#') => {
            skip_to_eol(bytes, i, line);
            Ok(())
        }
        Some(&byte) => Err(DotenvError {
            line: *line,
            offset: *i,
            why: DotenvWhy::TrailingContent { byte },
        }),
    }
}

fn parse_single_quoted<'a, A: Allocator>(
    input: &'a str,
    bytes: &[u8],
    i: &mut usize,
    line: &mut usize,
) -> Result<DotenvValue<'a, A>, DotenvError> {
    let open_line = *line;
    let open_offset = *i;
    *i += 1; // skip opening '\''
    let start = *i;
    while let Some(&b) = bytes.get(*i) {
        if b == b'\'' {
            let end = *i;
            *i += 1; // skip closing '\''
            consume_post_quote_tail(bytes, i, line)?;
            return Ok(DotenvValue::Borrowed(input.get(start..end).unwrap_or("")));
        }
        if b == b'\n' {
            *line += 1;
        }
        *i += 1;
    }
    Err(DotenvError {
        line: open_line,
        offset: open_offset,
        why: DotenvWhy::UnclosedSingleQuote,
    })
}

fn parse_double_quoted<'a, A: Allocator + Copy>(
    input: &'a str,
    bytes: &[u8],
    i: &mut usize,
    line: &mut usize,
    alloc: A,
    plant_skip_escape: bool,
) -> Result<DotenvValue<'a, A>, DotenvError> {
    let open_line = *line;
    let open_offset = *i;
    *i += 1; // skip opening '"'
    let start = *i;

    // Fast path scan to detect whether any backslash escape is present.
    let mut scan = *i;
    let mut has_escape = false;
    let mut close_pos: Option<usize> = None;
    let mut scan_line = *line;
    while let Some(&b) = bytes.get(scan) {
        if b == b'\\' {
            has_escape = true;
            scan += 1;
            match bytes.get(scan) {
                Some(&b'\n') => {
                    scan_line += 1;
                    scan += 1;
                }
                Some(_) => {
                    scan += 1;
                }
                None => break,
            }
            continue;
        }
        if b == b'"' {
            close_pos = Some(scan);
            break;
        }
        if b == b'\n' {
            scan_line += 1;
        }
        scan += 1;
    }

    let Some(end) = close_pos else {
        return Err(DotenvError {
            line: open_line,
            offset: open_offset,
            why: DotenvWhy::UnclosedDoubleQuote,
        });
    };

    if !has_escape || plant_skip_escape {
        *i = end + 1;
        *line = scan_line;
        consume_post_quote_tail(bytes, i, line)?;
        return Ok(DotenvValue::Borrowed(input.get(start..end).unwrap_or("")));
    }

    let mut out = String::try_with_capacity_in(end - start, alloc).map_err(|e| DotenvError {
        line: open_line,
        offset: open_offset,
        why: DotenvWhy::Alloc(e),
    })?;

    let mut seg_start = start;
    let mut cur = start;
    while cur < end {
        let Some(&b) = bytes.get(cur) else {
            break;
        };
        if b == b'\\' {
            if let Some(seg) = input.get(seg_start..cur) {
                out.try_push_str(seg).map_err(|e| DotenvError {
                    line: *line,
                    offset: cur,
                    why: DotenvWhy::Alloc(e),
                })?;
            }
            let esc_offset = cur;
            cur += 1;
            let Some(&esc) = bytes.get(cur) else {
                return Err(DotenvError {
                    line: open_line,
                    offset: open_offset,
                    why: DotenvWhy::UnclosedDoubleQuote,
                });
            };
            let decoded = match esc {
                b'n' => '\n',
                b'r' => '\r',
                b't' => '\t',
                b'"' => '"',
                b'\\' => '\\',
                b'$' => '$',
                other => {
                    return Err(DotenvError {
                        line: *line,
                        offset: esc_offset,
                        why: DotenvWhy::InvalidEscape { byte: other },
                    });
                }
            };
            out.try_push(decoded).map_err(|e| DotenvError {
                line: *line,
                offset: esc_offset,
                why: DotenvWhy::Alloc(e),
            })?;
            cur += 1;
            seg_start = cur;
            continue;
        }
        if b == b'\n' {
            *line += 1;
        }
        cur += 1;
    }

    if let Some(tail) = input.get(seg_start..end) {
        out.try_push_str(tail).map_err(|e| DotenvError {
            line: *line,
            offset: end,
            why: DotenvWhy::Alloc(e),
        })?;
    }

    *i = end + 1; // skip closing '"'
    consume_post_quote_tail(bytes, i, line)?;
    Ok(DotenvValue::Owned(out))
}

fn parse_unquoted<'a, A: Allocator>(
    input: &'a str,
    bytes: &[u8],
    i: &mut usize,
    line: &mut usize,
    saw_space_after_eq: bool,
) -> DotenvValue<'a, A> {
    let start = *i;
    if saw_space_after_eq && bytes.get(*i) == Some(&b'#') {
        skip_to_eol(bytes, i, line);
        return DotenvValue::Borrowed("");
    }
    let mut end = start;
    let mut prev_ws = saw_space_after_eq;
    while let Some(&b) = bytes.get(*i) {
        if b == b'\n' {
            *i += 1;
            *line += 1;
            break;
        }
        if b == b'\r' {
            *i += 1;
            if bytes.get(*i) == Some(&b'\n') {
                *i += 1;
            }
            *line += 1;
            break;
        }
        if b == b'#' && prev_ws {
            skip_to_eol(bytes, i, line);
            break;
        }
        prev_ws = b == b' ' || b == b'\t';
        *i += 1;
        end = *i;
    }
    let raw = input.get(start..end).unwrap_or("");
    let trimmed = raw.trim_end_matches([' ', '\t']);
    DotenvValue::Borrowed(trimmed)
}

#[cfg(test)]
mod tests {
    use core::alloc::Allocator;
    use core::fmt::Write;

    use blaze_own::{String as OwnString, Vec as OwnVec, format_in, testing::heap};
    use blaze_sim::{Broke, Config};

    use super::{DotenvError, DotenvLimits, DotenvValue, DotenvWhy, parse_in, parse_in_planted};

    #[test]
    fn dotenv_basics_quotes_exports_and_comments() {
        let h = heap();
        let alloc = h.alloc();
        let src = concat!(
            "\u{FEFF}# Database config\r\n",
            "\r\n",
            "export DATABASE_URL=postgres://localhost/app # inline comment\r\n",
            "COLOR=#ff00aa\n",
            "EMPTY=\n",
            "EMPTY_COMMENT = # nothing\n",
            "SINGLE='hello\\nworld\nline2'\n",
            "DOUBLE=\"a\\nb\\rc\\td\\\"e\\\\f\\$g\"\n",
            "export=keyword_as_key\n",
        );

        let entries = parse_in(src, DotenvLimits::SHIPPED, alloc).expect("parse dotenv");
        assert_eq!(entries.len(), 7);

        assert_eq!(entries[0].key, "DATABASE_URL");
        assert_eq!(entries[0].value, DotenvValue::Borrowed("postgres://localhost/app"));
        assert_eq!(entries[0].line, 3);

        assert_eq!(entries[1].key, "COLOR");
        assert_eq!(entries[1].value, DotenvValue::Borrowed("#ff00aa"));

        assert_eq!(entries[2].key, "EMPTY");
        assert_eq!(entries[2].value, DotenvValue::Borrowed(""));

        assert_eq!(entries[3].key, "EMPTY_COMMENT");
        assert_eq!(entries[3].value, DotenvValue::Borrowed(""));

        assert_eq!(entries[4].key, "SINGLE");
        assert_eq!(entries[4].value, DotenvValue::Borrowed("hello\\nworld\nline2"));

        assert_eq!(entries[5].key, "DOUBLE");
        assert_eq!(entries[5].value.as_str(), "a\nb\rc\td\"e\\f$g");
        assert!(matches!(entries[5].value, DotenvValue::Owned(_)));

        assert_eq!(entries[6].key, "export");
        assert_eq!(entries[6].value, DotenvValue::Borrowed("keyword_as_key"));
    }

    #[test]
    fn dotenv_errors_and_limits() {
        let h = heap();
        let alloc = h.alloc();

        assert!(matches!(
            parse_in("1BAD=val\n", DotenvLimits::SHIPPED, alloc),
            Err(DotenvError { line: 1, why: DotenvWhy::InvalidKey, .. })
        ));
        assert!(matches!(
            parse_in("NO_EQUALS\n", DotenvLimits::SHIPPED, alloc),
            Err(DotenvError { line: 1, why: DotenvWhy::MissingEquals, .. })
        ));
        assert!(matches!(
            parse_in("A='unclosed\n", DotenvLimits::SHIPPED, alloc),
            Err(DotenvError { line: 1, why: DotenvWhy::UnclosedSingleQuote, .. })
        ));
        assert!(matches!(
            parse_in("A=\"bad\\q\"\n", DotenvLimits::SHIPPED, alloc),
            Err(DotenvError { line: 1, why: DotenvWhy::InvalidEscape { byte: b'q' }, .. })
        ));
        assert!(matches!(
            parse_in("A=\"ok\" extra\n", DotenvLimits::SHIPPED, alloc),
            Err(DotenvError { line: 1, why: DotenvWhy::TrailingContent { byte: b'e' }, .. })
        ));
        assert!(matches!(
            parse_in(
                "A=1\nB=2\n",
                DotenvLimits { most_bytes: 100, most_entries: 1 },
                alloc,
            ),
            Err(DotenvError { line: 2, why: DotenvWhy::TooManyEntries { limit: 1 }, .. })
        ));
    }

    fn format_dotenv_double<A: Allocator>(
        key: &str,
        val: &str,
        with_export: bool,
        with_comment: bool,
        crlf: bool,
        out: &mut OwnString<A>,
    ) {
        if with_export {
            out.push_str("export ");
        }
        out.push_str(key);
        out.push_str("=\"");
        for ch in val.chars() {
            match ch {
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '$' => out.push_str("\\$"),
                other => out.push(other),
            }
        }
        out.push('"');
        if with_comment {
            out.push_str(" # note");
        }
        if crlf {
            out.push_str("\r\n");
        } else {
            out.push('\n');
        }
    }

    fn draw_pairs<A: Allocator + Copy>(
        tape: &blaze_sim::Tape,
        alloc: A,
    ) -> OwnVec<(OwnString<A>, OwnString<A>), A> {
        const KEYS: &[&str] = &["PORT", "HOST_NAME", "_SECRET_1", "APP_ENV", "TOKEN_V2"];
        const PIECES: &[&str] = &["", "abc", " ", "\n", "\r", "\t", "\"", "\\", "$VAR", "#hash", "café"];
        let count = tape.int_in(0usize..=8, "entries");
        let mut pairs = OwnVec::new_in(alloc);
        for i in 0..count {
            let mut key = OwnString::from_str_in(KEYS[tape.index(KEYS.len(), "key")], alloc);
            write!(&mut key, "_{i}").expect("write");
            let parts = tape.int_in(0usize..=4, "parts");
            let mut val = OwnString::new_in(alloc);
            for _ in 0..parts {
                val.push_str(PIECES[tape.index(PIECES.len(), "piece")]);
            }
            pairs.push((key, val));
        }
        pairs
    }

    fn check_dotenv_round_trip<A: Allocator + Copy>(
        tape: &blaze_sim::Tape,
        pairs: &[(OwnString<A>, OwnString<A>)],
        plant_skip_escape: bool,
        alloc: A,
    ) -> Result<(), Broke> {
        let mut text = OwnString::new_in(alloc);
        for (k, v) in pairs {
            let with_export = tape.coin("export");
            let with_comment = tape.coin("comment");
            let crlf = tape.coin("crlf");
            format_dotenv_double(k.as_str(), v.as_str(), with_export, with_comment, crlf, &mut text);
        }
        let parsed = parse_in_planted(text.as_str(), DotenvLimits::SHIPPED, alloc, plant_skip_escape)
            .map_err(|e| Broke::Oracle(format_in!(alloc, "parse failed: {e}").as_str().into()))?;
        if parsed.len() != pairs.len() {
            return Err(Broke::Oracle(
                format_in!(alloc, "count mismatch: expected {}, got {}", pairs.len(), parsed.len())
                    .as_str()
                    .into(),
            ));
        }
        for (idx, (got, (ek, ev))) in parsed.iter().zip(pairs.iter()).enumerate() {
            if got.key != ek.as_str() || got.value.as_str() != ev.as_str() {
                return Err(Broke::Oracle(
                    format_in!(
                        alloc,
                        "entry {idx} mismatch: expected ({:?}, {:?}), got ({:?}, {:?})",
                        ek.as_str(),
                        ev.as_str(),
                        got.key,
                        got.value.as_str()
                    )
                    .as_str()
                    .into(),
                ));
            }
        }
        Ok(())
    }

    /// The invariant: dotenv formatting and parsing round-trip arbitrary keys and escaped values without loss.
    #[test]
    fn property_dotenv_round_trip() {
        let h = heap();
        blaze_sim::check(Config::cases(256), |tape| {
            let pairs = draw_pairs(tape, h.alloc());
            check_dotenv_round_trip(tape, pairs.as_slice(), false, h.alloc())
        });
    }

    #[test]
    fn the_oracle_catches_skipped_escapes() {
        let h = heap();
        blaze_sim::control(Config::cases(128), false, true, "", |tape, plant| {
            let pairs = draw_pairs(tape, h.alloc());
            check_dotenv_round_trip(tape, pairs.as_slice(), plant, h.alloc())
        });
    }
}
