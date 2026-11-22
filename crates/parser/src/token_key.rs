//! An item's keys from its tokens, not its bytes (docs/v2/plan/design/token-keys.md, plan row
//! X4): re-formatting an item, or adding, removing or editing a plain comment in it, moves no
//! [`token_digest`]; what reads spacing (`stringify!`, `assert!`'s message, a proc macro
//! printing tokens) reads [`adjacency_digest`] as well, and what reads lines and columns
//! (`line!()`, `column!()`, a line table) [`line_map_digest`]; and a place kept in a record is a [`TokenPlace`], turned
//! into bytes only from today's text of the item, when it is printed.
//!
//! The tokens are the parser's ([`LexedStr`] in the item's edition): whitespace and plain
//! comments (`//`, `/* */`) are not tokens; a doc comment (`///`, `//!`, `/** */`, `/*! */`) is
//! one, as its text, since it is an attribute with meaning. Sans-IO and allocation-free: one lex
//! into the thread's scratch columns and one pass over them.

use core::{fmt, ops::Range};

use blaze_ids::{ContentId, Domain, Hasher};

use crate::{Edition, LexedStr, SyntaxKind};

/// The domain of [`token_digest`]: a change of its encoding is a new domain.
pub const TOKEN_DIGEST_DOMAIN: &str = "blaze v2 item tokens 1";
/// The domain of [`line_map_digest`].
pub const LINE_MAP_DOMAIN: &str = "blaze v2 item line map 1";

/// The domain of [`adjacency_digest`].
pub const ADJACENCY_DOMAIN: &str = "blaze v2 item adjacency 1";

static TOKENS: Domain = Domain::new(TOKEN_DIGEST_DOMAIN);
static LINES: Domain = Domain::new(LINE_MAP_DOMAIN);
static ADJACENCY: Domain = Domain::new(ADJACENCY_DOMAIN);

/// Why a text has no token key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenKeyError {
    /// The text is longer than a token place can count (`u32::MAX` bytes).
    TooLong { bytes: usize },
}

impl fmt::Display for TokenKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TokenKeyError::TooLong { bytes } => {
                write!(f, "an item's text of {bytes} bytes is longer than token places count ({} bytes)", u32::MAX)
            }
        }
    }
}

/// The digest of `text`'s tokens in `edition`, in [`TOKEN_DIGEST_DOMAIN`]: the edition as one
/// byte; then per token in order `1`, its kind (`SyntaxKind` as a little-endian u16), its
/// lexeme's length (little-endian u32), its lexeme's bytes, and one byte for whether it is
/// joined to the next token; then `0`. Whitespace and plain comments are no tokens; a doc
/// comment is one (kind `INNER_DOC_COMMENT` or `OUTER_DOC_COMMENT`, its whole text). A token
/// is joined when it is punctuation
/// other than a delimiter and the next token is written right against it and is punctuation
/// other than a delimiter, or a lifetime (which starts with `'`): `proc_macro`'s
/// `Spacing::Joint`, what tells `>>` from `> >` and `&'a` from `& 'a`.
pub fn token_digest(text: &str, edition: Edition) -> Result<ContentId, TokenKeyError> {
    fits(text)?;
    let mut h = Hasher::of(&TOKENS);
    h.update(&[edition as u8]);
    key_tokens(text, edition, |token| {
        let mut head = [0u8; 7];
        head[0] = 1;
        head[1..3].copy_from_slice(&(token.kind as u16).to_le_bytes());
        head[3..7].copy_from_slice(&narrow(token.lexeme.len()).to_le_bytes());
        h.update(&head).update(token.lexeme.as_bytes()).update(&[u8::from(token.joined)]);
    });
    h.update(&[0]);
    Ok(h.finish())
}

/// The digest of where `text`'s tokens ([`token_digest`]'s) start, in [`LINE_MAP_DOMAIN`]: the
/// edition as one byte; then per token in order `1`, its line less the first token's line
/// (little-endian u32) and its column, the characters between the start of its line and the
/// token (little-endian u32, as `column!()` counts less one); then `0`. A line ends at `\n`
/// only, so `\r\n` and `\n` give one map. What moves it: a line break added or removed between
/// two tokens, or a token's column changed (indentation, spacing); what does not: anything
/// before the first token on lines above it (a blank line added above the item).
pub fn line_map_digest(text: &str, edition: Edition) -> Result<ContentId, TokenKeyError> {
    line_map(text, edition)
}

/// The digest of which of `text`'s tokens ([`token_digest`]'s) trivia separates from the next,
/// in [`ADJACENCY_DOMAIN`]: the edition as one byte; then one bit per token in order, packed
/// eight to a byte from the low bit, 1 when whitespace or a plain comment comes between it and
/// the next token (0 for the last token, which has no next in the item); then the token count
/// (little-endian u32), which makes the packed bits decodable. What a reader of spacing reads
/// (`stringify!`, `assert!`'s message, a proc macro printing tokens): `f(x)` and `f (x)` differ
/// here and not in the token digest; a line break between tokens a space already separated
/// moves the line map and not this.
pub fn adjacency_digest(text: &str, edition: Edition) -> Result<ContentId, TokenKeyError> {
    fits(text)?;
    let mut h = Hasher::of(&ADJACENCY);
    h.update(&[edition as u8]);
    let (mut byte, mut bits, mut count) = (0u8, 0u32, 0u32);
    // A token's bit is put once the next token is seen, so the last token's is 0.
    let mut pending: Option<bool> = None;
    let mut put = |bit: bool, h: &mut Hasher| {
        byte |= u8::from(bit) << (bits % 8);
        bits += 1;
        if bits % 8 == 0 {
            h.update(&[byte]);
            byte = 0;
        }
    };
    key_tokens(text, edition, |token| {
        if let Some(bit) = pending {
            put(bit, &mut h);
        }
        pending = Some(token.separated);
        count += 1;
    });
    if pending.is_some() {
        put(false, &mut h);
    }
    if bits % 8 != 0 {
        h.update(&[byte]);
    }
    h.update(&count.to_le_bytes());
    Ok(h.finish())
}

/// [`line_map_digest`]'s walk.
fn line_map(text: &str, edition: Edition) -> Result<ContentId, TokenKeyError> {
    fits(text)?;
    let lexed = LexedStr::new(edition, text);
    let mut h = Hasher::of(&LINES);
    h.update(&[edition as u8]);
    let (mut at, mut line, mut column) = (0usize, 0u32, 0u32);
    let mut first_line = None;
    for (kind, len) in lexed.kinds_and_lens() {
        let lexeme = &text[at..at + len];
        at += len;
        if counts(kind, lexeme) {
            let base = *first_line.get_or_insert(line);
            let mut row = [0u8; 9];
            row[0] = 1;
            row[1..5].copy_from_slice(&(line - base).to_le_bytes());
            row[5..9].copy_from_slice(&column.to_le_bytes());
            h.update(&row);
        }
        match lexeme.rfind('\n') {
            Some(last) => {
                line += narrow(lexeme.bytes().filter(|&byte| byte == b'\n').count());
                column = narrow(lexeme[last + 1..].chars().count());
            }
            None => column += narrow(lexeme.chars().count()),
        }
    }
    h.update(&[0]);
    Ok(h.finish())
}

/// A place in an item, by its tokens ([`token_digest`]'s, counted from 0): tokens `first` to
/// `last`, both included; with `inside`, the bytes `inside.0..inside.1` of the one token's
/// lexeme (a format string's argument, a literal's suffix). What a record keeps for a finding
/// or a span (rule 71): it survives a re-format, which moves no key; [`TokenPlace::to_bytes`]
/// makes it bytes of today's text when it is printed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TokenPlace {
    pub first: u32,
    pub last: u32,
    pub inside: Option<(u32, u32)>,
}

/// Why a [`TokenPlace`] and a byte range of a text do not map to each other. Never clamped
/// (rule 3): each names the counts it was refused by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaceError {
    /// The text is longer than a token place can count.
    TooLong { bytes: usize },
    /// The place's last token is before its first.
    Backwards { first: u32, last: u32 },
    /// The place's last token is past the text's tokens.
    PastEnd { last: u32, tokens: u32 },
    /// A place inside a token names more than one token.
    InsideSpansTokens { first: u32, last: u32 },
    /// A place inside a token is reversed or ends past its lexeme.
    InsidePastLexeme { token: u32, inside: (u32, u32), lexeme: u32 },
    /// A place inside a token does not fall on a character boundary of its lexeme.
    NotCharBoundary { token: u32, at: u32 },
    /// The byte range is reversed or ends past the text.
    RangeOutside { start: usize, end: usize, text: usize },
    /// The byte range starts or ends in whitespace, a plain comment, or past the last token.
    InTrivia { at: usize },
    /// The byte range covers more than one token and starts or ends inside one.
    NotTokenAligned { at: usize },
}

impl fmt::Display for PlaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlaceError::TooLong { bytes } => {
                write!(f, "an item's text of {bytes} bytes is longer than token places count ({} bytes)", u32::MAX)
            }
            PlaceError::Backwards { first, last } => write!(f, "a token place from token {first} back to token {last}"),
            PlaceError::PastEnd { last, tokens } => {
                write!(f, "a token place ends at token {last} of an item of {tokens} tokens")
            }
            PlaceError::InsideSpansTokens { first, last } => {
                write!(f, "a place inside one token names tokens {first} to {last}")
            }
            PlaceError::InsidePastLexeme { token, inside, lexeme } => write!(
                f,
                "a place inside token {token} at bytes {}..{} of its lexeme of {lexeme} bytes",
                inside.0, inside.1
            ),
            PlaceError::NotCharBoundary { token, at } => {
                write!(f, "a place inside token {token} at byte {at} of its lexeme, inside a character")
            }
            PlaceError::RangeOutside { start, end, text } => {
                write!(f, "the byte range {start}..{end} is not a range of a text of {text} bytes")
            }
            PlaceError::InTrivia { at } => {
                write!(f, "the byte range starts or ends at byte {at}, which is in no token")
            }
            PlaceError::NotTokenAligned { at } => {
                write!(f, "the byte range covers more than one token and cuts a token at byte {at}")
            }
        }
    }
}

impl TokenPlace {
    /// The bytes of `text` (the item's text today, in `edition`) this place covers.
    pub fn to_bytes(self, text: &str, edition: Edition) -> Result<Range<usize>, PlaceError> {
        fits(text).map_err(|TokenKeyError::TooLong { bytes }| PlaceError::TooLong { bytes })?;
        let TokenPlace { first, last, inside } = self;
        if last < first {
            return Err(PlaceError::Backwards { first, last });
        }
        if inside.is_some() && first != last {
            return Err(PlaceError::InsideSpansTokens { first, last });
        }
        let (mut start, mut end, mut count) = (None, None, 0u32);
        key_tokens(text, edition, |token| {
            if token.index == first {
                start = Some(token.start);
            }
            if token.index == last {
                end = Some((token.start, token.lexeme.len()));
            }
            count += 1;
        });
        self.resolved(start, end, count, text)
    }

    /// [`TokenPlace::to_bytes`] from a table of the item's file made once ([`file_tokens`],
    /// [`FileTokens::item`]): the same range of `text` (the item's text today, from its start),
    /// with no lex. A file's places are then one lex of the file, however many it holds.
    pub fn to_bytes_in(self, tokens: ItemTokens<'_>, text: &str) -> Result<Range<usize>, PlaceError> {
        fits(text).map_err(|TokenKeyError::TooLong { bytes }| PlaceError::TooLong { bytes })?;
        let TokenPlace { first, last, inside } = self;
        if last < first {
            return Err(PlaceError::Backwards { first, last });
        }
        if inside.is_some() && first != last {
            return Err(PlaceError::InsideSpansTokens { first, last });
        }
        // Every key of an `ItemTokens` starts at or after its item (`FileTokens::item`).
        let from_item = |(start, len): (u32, u32)| (wide(start - tokens.item), wide(len));
        let start = tokens.keys.get(wide(first)).map(|&key| from_item(key).0);
        let end = tokens.keys.get(wide(last)).map(|&key| from_item(key));
        let count = u32::try_from(tokens.keys.len()).unwrap_or(u32::MAX);
        self.resolved(start, end, count, text)
    }

    /// The range of a place whose first token starts at `start` and whose last token starts at
    /// `end.0` with a lexeme of `end.1` bytes, in an item of `count` tokens: [`TokenPlace::to_bytes`]'s
    /// and [`TokenPlace::to_bytes_in`]'s one rule.
    fn resolved(self, start: Option<usize>, end: Option<(usize, usize)>, count: u32, text: &str) -> Result<Range<usize>, PlaceError> {
        let TokenPlace { last, inside, .. } = self;
        let (Some(start), Some((last_start, last_len))) = (start, end) else {
            return Err(PlaceError::PastEnd { last, tokens: count });
        };
        match inside {
            None => Ok(start..last_start + last_len),
            Some((lo, hi)) => {
                let lexeme = narrow(last_len);
                if hi < lo || hi > lexeme {
                    return Err(PlaceError::InsidePastLexeme { token: last, inside: (lo, hi), lexeme });
                }
                let (lo_at, hi_at) = (start + wide(lo), start + wide(hi));
                for (offset, byte) in [(lo, lo_at), (hi, hi_at)] {
                    if !text.is_char_boundary(byte) {
                        return Err(PlaceError::NotCharBoundary { token: last, at: offset });
                    }
                }
                Ok(lo_at..hi_at)
            }
        }
    }

    /// The place of the bytes `start..end` of `text` (an item's text in `edition`): whole
    /// tokens, or a stretch inside one token (an empty range too: at a token's start, inside
    /// it, or at its end). A range that starts or ends in trivia, or that covers more than one
    /// token and cuts one, is refused.
    pub fn from_bytes(text: &str, edition: Edition, Range { start, end }: Range<usize>) -> Result<TokenPlace, PlaceError> {
        fits(text).map_err(|TokenKeyError::TooLong { bytes }| PlaceError::TooLong { bytes })?;
        if end < start || end > text.len() {
            return Err(PlaceError::RangeOutside { start, end, text: text.len() });
        }
        // Per end of the range: the token it falls in and that token's start and end.
        let mut from: Option<(u32, usize, usize)> = None;
        let mut to: Option<(u32, usize, usize)> = None;
        // For an empty range: a token that ends where it is, if none holds it.
        let mut ending_here: Option<(u32, usize, usize)> = None;
        key_tokens(text, edition, |token| {
            let (lo, hi) = (token.start, token.start + token.lexeme.len());
            let at = (token.index, lo, hi);
            if lo <= start && start < hi {
                from = Some(at);
            }
            if lo < end && end <= hi {
                to = Some(at);
            }
            if start == end && hi == start {
                ending_here = Some(at);
            }
        });
        if start == end {
            let Some((index, lo, _)) = from.or(ending_here) else {
                return Err(PlaceError::InTrivia { at: start });
            };
            let offset = narrow(start - lo);
            return Ok(TokenPlace { first: index, last: index, inside: Some((offset, offset)) });
        }
        let Some((first, first_lo, _)) = from else {
            return Err(PlaceError::InTrivia { at: start });
        };
        let Some((last, last_lo, last_hi)) = to else {
            return Err(PlaceError::InTrivia { at: end });
        };
        if first == last {
            let whole = start == first_lo && end == last_hi;
            let inside = (!whole).then(|| (narrow(start - first_lo), narrow(end - first_lo)));
            return Ok(TokenPlace { first, last, inside });
        }
        if start != first_lo {
            return Err(PlaceError::NotTokenAligned { at: start });
        }
        if end != last_hi {
            return Err(PlaceError::NotTokenAligned { at: end });
        }
        debug_assert!(last_lo < end, "the last token holds the range's end");
        Ok(TokenPlace { first, last, inside: None })
    }
}

/// Each raw token of a file's `text` in `edition` that is not whitespace, in order, with its start
/// and its lexeme's length in bytes from the file's start: a key token ([`token_digest`]'s) to
/// `key`, a plain comment to `comment`. One lex of the file: the columns a caller keeps of it
/// ([`FileTokens`]) place every [`TokenPlace`] of the file's items ([`TokenPlace::to_bytes_in`]),
/// so a file is lexed once however many places it holds. The parser keeps nothing of it: the
/// caller holds the columns, in its own allocator.
pub fn file_tokens(text: &str, edition: Edition, mut key: impl FnMut(u32, u32), mut comment: impl FnMut(u32, u32)) -> Result<(), TokenKeyError> {
    fits(text)?;
    let lexed = LexedStr::new(edition, text);
    let mut at = 0usize;
    for (kind, len) in lexed.kinds_and_lens() {
        let start = at;
        at += len;
        if kind == SyntaxKind::WHITESPACE {
            continue;
        }
        if counts(kind, &text[start..at]) {
            key(narrow(start), narrow(len));
        } else {
            comment(narrow(start), narrow(len));
        }
    }
    Ok(())
}

/// The columns of one lex of a file ([`file_tokens`]): each key token's and each plain comment's
/// (start, length), in order, from the file's start.
#[derive(Clone, Copy, Debug)]
pub struct FileTokens<'t> {
    pub keys: &'t [(u32, u32)],
    pub comments: &'t [(u32, u32)],
}

/// The key tokens of one item of a file ([`FileTokens::item`]): the file's from the item's start
/// to its end, each starting at or after `item`, the item's start in the file.
#[derive(Clone, Copy, Debug)]
pub struct ItemTokens<'t> {
    keys: &'t [(u32, u32)],
    item: u32,
}

impl<'t> FileTokens<'t> {
    /// The key tokens a lex of the file's text from `item` on gives (what
    /// [`TokenPlace::to_bytes`] counts over the item's text), when `item` is where a raw token of
    /// the file starts or lies in whitespace: there a lex from `item` sees the file's tokens from
    /// there on, one by one (a token's lexing reads no byte before its start). None when `item`
    /// is inside a key token or a plain comment, where a lex from `item` would make other tokens.
    pub fn item(&self, item: u32) -> Option<ItemTokens<'t>> {
        let inside = |column: &[(u32, u32)]| {
            let before = column.partition_point(|&(start, _)| start < item);
            before > 0 && {
                let (start, len) = column[before - 1];
                item < start.saturating_add(len)
            }
        };
        if inside(self.keys) || inside(self.comments) {
            return None;
        }
        let first = self.keys.partition_point(|&(start, _)| start < item);
        Some(ItemTokens { keys: &self.keys[first..], item })
    }
}

/// One token that counts ([`counts`]): its index among them, where it starts in the text, its
/// lexeme, kind, and whether it is joined to the next ([`token_digest`]).
#[derive(Clone, Copy, Debug)]
struct KeyToken<'t> {
    index: u32,
    start: usize,
    lexeme: &'t str,
    kind: SyntaxKind,
    joined: bool,
    /// Whether trivia (whitespace, a plain comment) comes right after it ([`adjacency_digest`]).
    separated: bool,
}

/// Each token of `text` in `edition` that counts, in order. The caller has checked `text` fits
/// a u32 ([`fits`]), so every index and offset does.
fn key_tokens<'t>(text: &'t str, edition: Edition, mut each: impl FnMut(KeyToken<'t>)) {
    let lexed = LexedStr::new(edition, text);
    let mut raw = lexed.kinds_and_lens().peekable();
    let (mut at, mut index) = (0usize, 0u32);
    while let Some((kind, len)) = raw.next() {
        let start = at;
        at += len;
        let lexeme = &text[start..at];
        if !counts(kind, lexeme) {
            continue;
        }
        let next = raw.peek().copied();
        let joined = next.is_some_and(|(next, _)| joined(kind, next));
        let separated = next.is_some_and(|(next, len)| !counts(next, &text[at..at + len]));
        each(KeyToken { index, start, lexeme, kind, joined, separated });
        index += 1;
    }
}

/// Whether a token of `kind` written `lexeme` is one of an item's tokens: everything but
/// whitespace and a plain comment. A doc comment counts: the lexer gives it its own kind
/// (`INNER_DOC_COMMENT`, `OUTER_DOC_COMMENT`); a `COMMENT` is read again by rustc_lexer's rule
/// too, so no doc comment can be dropped as plain whatever kind it came with.
fn counts(kind: SyntaxKind, lexeme: &str) -> bool {
    match kind {
        SyntaxKind::WHITESPACE => false,
        SyntaxKind::COMMENT => is_doc_comment(lexeme),
        _ => true,
    }
}

/// Whether the comment `lexeme` is a doc comment, as `rustc_lexer` says: `//!`, or `///` not
/// followed by `/`; `/*!`, or `/**` not followed by `*` or `/` (`/**/` and `/***` are plain).
fn is_doc_comment(lexeme: &str) -> bool {
    let bytes = lexeme.as_bytes();
    match bytes {
        [b'/', b'/', b'!', ..] => true,
        [b'/', b'/', b'/', rest @ ..] => rest.first() != Some(&b'/'),
        [b'/', b'*', b'!', ..] => true,
        [b'/', b'*', b'*', rest @ ..] => !matches!(rest.first(), Some(b'*' | b'/')),
        _ => false,
    }
}

/// Whether a token of `kind` can be joined to the next: punctuation, not a delimiter.
fn joins_left(kind: SyntaxKind) -> bool {
    kind.is_punct()
        && !matches!(
            kind,
            SyntaxKind::L_PAREN
                | SyntaxKind::R_PAREN
                | SyntaxKind::L_CURLY
                | SyntaxKind::R_CURLY
                | SyntaxKind::L_BRACK
                | SyntaxKind::R_BRACK
        )
}

/// Whether a token of `kind` makes the token before it joined when written right against it:
/// punctuation that is not a delimiter, or a lifetime, whose `'` is punctuation to a macro.
fn joins_right(kind: SyntaxKind) -> bool {
    joins_left(kind) || kind == SyntaxKind::LIFETIME_IDENT
}

/// Whether a token of `kind` written right against a token of `next` is joined to it: the one
/// rule a token key's joined bit follows, for [`key_tokens`] and for a digest made from kinds
/// (blaze-file-items digest.rs).
pub fn joined(kind: SyntaxKind, next: SyntaxKind) -> bool {
    joins_left(kind) && joins_right(next)
}

/// `text` fits the u32 offsets and counts of a token place.
fn fits(text: &str) -> Result<(), TokenKeyError> {
    match u32::try_from(text.len()) {
        Ok(_) => Ok(()),
        Err(_) => Err(TokenKeyError::TooLong { bytes: text.len() }),
    }
}

/// A count or offset within a text [`fits`] checked.
fn narrow(n: usize) -> u32 {
    u32::try_from(n).expect("a count within a text of at most u32::MAX bytes (checked by `fits`)")
}

fn wide(n: u32) -> usize {
    n as usize
}

#[cfg(test)]
#[path = "token_key_tests.rs"]
mod tests;
