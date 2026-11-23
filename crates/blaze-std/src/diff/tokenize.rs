//! Lossless text tokenizers for line, word, and character diffs.
//!
//! Every tokenizer returns borrowed [`Token`] slices with explicit byte spans `(start, end)` into
//! the input text so coalescing requires zero string copies, zero pointer casts, and zero
//! panicking slice indexes.

use core::alloc::Allocator;
use core::hash::{Hash, Hasher};

use blaze_own::Vec;

use super::DiffError;

/// A borrowed token slice together with its byte span `[start..end)` in the source string.
/// Equality and hashing compare only `text`, so identical tokens at different offsets match.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Token<'a> {
    pub(crate) text: &'a str,
    pub(crate) start: usize,
    pub(crate) end: usize,
}

impl PartialEq for Token<'_> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.text == other.text
    }
}

impl Eq for Token<'_> {}

impl Hash for Token<'_> {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.text.hash(state);
    }
}

/// Split `text` into line slices where every line keeps its trailing `\n` (if present).
pub(crate) fn split_lines<'a, A: Allocator>(
    text: &'a str,
    alloc: A,
) -> Result<Vec<&'a str, A>, DiffError> {
    let mut out = Vec::new_in(alloc);
    for line in text.split_inclusive('\n') {
        out.try_push(line).map_err(|r| DiffError::Alloc(r.why))?;
    }
    Ok(out)
}

/// Split `text` into line [`Token`]s with byte spans.
pub(crate) fn split_line_tokens<'a, A: Allocator>(
    text: &'a str,
    alloc: A,
) -> Result<Vec<Token<'a>, A>, DiffError> {
    let mut out = Vec::new_in(alloc);
    let mut offset = 0usize;
    for line in text.split_inclusive('\n') {
        let end = offset + line.len();
        out.try_push(Token { text: line, start: offset, end })
            .map_err(|r| DiffError::Alloc(r.why))?;
        offset = end;
    }
    Ok(out)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WordKind {
    Word,
    Space,
    Punct,
}

fn char_kind(ch: char) -> WordKind {
    if ch.is_alphanumeric() || ch == '_' {
        WordKind::Word
    } else if ch.is_whitespace() {
        WordKind::Space
    } else {
        WordKind::Punct
    }
}

/// Split `text` into lossless word/whitespace/punctuation [`Token`]s with byte spans.
pub(crate) fn split_word_tokens<'a, A: Allocator>(
    text: &'a str,
    alloc: A,
) -> Result<Vec<Token<'a>, A>, DiffError> {
    let mut out = Vec::new_in(alloc);
    let mut iter = text.char_indices();
    let Some((mut start, first)) = iter.next() else {
        return Ok(out);
    };
    let mut kind = char_kind(first);
    let mut prev_nl = first == '\n';

    for (idx, ch) in iter {
        let next_kind = char_kind(ch);
        let split = match (kind, next_kind) {
            (WordKind::Punct, _) | (_, WordKind::Punct) => true,
            (WordKind::Word, WordKind::Word) => false,
            (WordKind::Space, WordKind::Space) => prev_nl || ch == '\n',
            _ => true,
        };
        if split {
            if let Some(slice) = text.get(start..idx) {
                out.try_push(Token { text: slice, start, end: idx })
                    .map_err(|r| DiffError::Alloc(r.why))?;
            }
            start = idx;
            kind = next_kind;
        }
        prev_nl = ch == '\n';
    }
    if let Some(slice) = text.get(start..) {
        out.try_push(Token { text: slice, start, end: text.len() })
            .map_err(|r| DiffError::Alloc(r.why))?;
    }
    Ok(out)
}

/// Split `text` into single Unicode scalar [`Token`]s with byte spans.
pub(crate) fn split_char_tokens<'a, A: Allocator>(
    text: &'a str,
    alloc: A,
) -> Result<Vec<Token<'a>, A>, DiffError> {
    let mut out = Vec::new_in(alloc);
    for (idx, ch) in text.char_indices() {
        let end = idx + ch.len_utf8();
        if let Some(slice) = text.get(idx..end) {
            out.try_push(Token { text: slice, start: idx, end })
                .map_err(|r| DiffError::Alloc(r.why))?;
        }
    }
    Ok(out)
}

/// Strip a single trailing `\n` from a line slice, returning `(body, had_lf)`.
#[inline]
pub(crate) fn strip_lf(line: &str) -> (&str, bool) {
    match line.strip_suffix('\n') {
        Some(body) => (body, true),
        None => (line, false),
    }
}
