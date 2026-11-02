//! Lexing `&str` into a sequence of Rust tokens.
//!
//! Note that strictly speaking the parser in this crate is not required to work
//! on tokens which originated from text. Macros, eg, can synthesize tokens out
//! of thin air. So, ideally, lexer should be an orthogonal crate. It is however
//! convenient to include a text-based lexer here!
//!
//! Note that these tokens, unlike the tokens we feed into the parser, do
//! include info about comments and whitespace.

#[allow(unsafe_code)]
mod classify;
#[cfg(test)]
mod differential;
mod scan;

use std::{borrow::Cow, ops};

use blaze_mem::scratch::{self, Lent};

use rustc_literal_escaper::{
    EscapeError, Mode, unescape_byte, unescape_byte_str, unescape_c_str, unescape_char,
    unescape_str,
};

use crate::{
    Edition,
    SyntaxKind::{self, *},
    T,
};

/// The tokens of a text, the last an `EOF` of no length.
///
/// A token is one `u32`: its kind in the low [`KIND_BITS`] bits and its length in bytes in the
/// rest. A length of [`LONG`] (4 MiB) or more, a huge string literal or comment, is written as
/// [`LONG`] and kept in `long`, a side table sorted by token. Starts are arithmetic, as in the
/// flat syntax tree: `checkpoints` holds the start of every [`CHECKPOINT`]th token, and a start
/// is its checkpoint plus the lengths of the tokens between. So a token costs 4 bytes and a
/// sixteenth of a checkpoint's 4, 4.25 bytes, against the 6 of a kind (`u16`) and a start
/// (`u32`) each; finding a start reads at most 15 lengths, and walking the tokens in order
/// (`to_input`, `intersperse_trivia`) keeps a running offset instead.
///
/// Both columns are the worker's `u32` scratch ([`scratch::lend_u32s`]), back on the shelf of
/// the thread that drops the `LexedStr`, so a warm lex allocates nothing. A shelf holds
/// [`scratch::DEPTH`] buffers: more than four `LexedStr`s alive at once on a thread allocate
/// the extra columns.
pub struct LexedStr<'a> {
    text: &'a str,
    // Fields drop in declaration order: `checkpoints`, lent after `tokens`, goes back first,
    // so the next lex takes each buffer back for the same column.
    checkpoints: Lent<Vec<u32>>,
    tokens: Lent<Vec<Token>>,
    /// With [`LexedStr::new_hashed`], the [`blaze_phf::name_hash`] of each token
    /// [`LexedStr::has_name_hash`] is true of, in order: the worker's `u64` scratch.
    names: Option<Lent<Vec<u64>>>,
    long: Vec<(u32, u32)>,
    error: Vec<LexError>,
}

/// A packed token: kind and length.
type Token = u32;

const _: () = assert!(size_of::<Token>() == 4, "a token is 4 bytes (docs/v2/plan/design/data.md)");

const KIND_BITS: u32 = 10;
const KIND_MASK: u32 = (1 << KIND_BITS) - 1;
const LONG: u32 = (1 << (32 - KIND_BITS)) - 1;
const CHECKPOINT: usize = 16;
const _: () = assert!((SyntaxKind::__LAST as u32) <= KIND_MASK, "a kind fits in KIND_BITS");

/// The messages are mostly fixed text: only an unterminated literal's, an unknown prefix's and
/// a bad raw name's are formatted, so a valid token allocates nothing.
struct LexError {
    msg: Cow<'static, str>,
    token: u32,
}

impl<'a> LexedStr<'a> {
    /// The byte-class kernel this build lexes with, chosen by `target_feature` when the crate
    /// is built: `avx512vbmi`, `avx2`, `sse2`, `neon` or `scalar`.
    pub const KERNEL: &'static str = classify::KERNEL;

    pub fn new(edition: Edition, text: &'a str) -> LexedStr<'a> {
        Self::lex::<true>(edition, text)
    }

    /// [`LexedStr::new`] that also hashes each identifier's name for the interner while the
    /// scan has its bytes in a register ([`LexedStr::name_hashes`]): for a caller that interns
    /// every identifier it lexes (`syntax_bridge`'s text to token trees), which then hands
    /// each hash to `intern::Symbol::intern_hashed` and never hashes the name again. The
    /// tokens are [`LexedStr::new`]'s.
    pub fn new_hashed(edition: Edition, text: &'a str) -> LexedStr<'a> {
        Self::lex_with::<true, true>(edition, text)
    }

    /// Whether a token of `kind` has a hash in [`LexedStr::name_hashes`]: the kinds the lexer
    /// gives an identifier, `IDENT`, `_` and the strict and reserved keywords. A raw
    /// identifier's is of its name without `r#`.
    #[inline]
    pub fn has_name_hash(kind: SyntaxKind) -> bool {
        kind == IDENT || kind == UNDERSCORE || crate::syntax_kind::keywords::is_strict_kind(kind)
    }

    /// The [`blaze_phf::name_hash`] of each token [`LexedStr::has_name_hash`] is true of, in
    /// token order; empty unless lexed by [`LexedStr::new_hashed`].
    pub fn name_hashes(&self) -> &[u64] {
        self.names.as_deref().map_or(&[], Vec::as_slice)
    }

    /// Whether the fast path and `rustc_lexer` alone lex `text` alike: kinds, lengths,
    /// checkpoints, long tokens and errors (`fast_and_rustc_lexer_lex_alike`), for the fuzz
    /// target `syntax_parse`.
    #[cfg(feature = "fuzz")]
    pub fn lexes_alike(edition: Edition, text: &'a str) -> bool {
        let (fast, slow) = (Self::lex::<true>(edition, text), Self::lex::<false>(edition, text));
        *fast.tokens == *slow.tokens
            && *fast.checkpoints == *slow.checkpoints
            && *fast.long == *slow.long
            && fast.errors().eq(slow.errors())
    }

    /// With `FAST`, the tokens [`scan`] decides from the byte classes ([`classify`]) are lexed
    /// there and the rest by `rustc_lexer`; without, all by `rustc_lexer`. Both give the same
    /// tokens (`fast_and_rustc_lexer_lex_alike`, `every_file_of_the_corpus_lexes_alike`).
    fn lex<const FAST: bool>(edition: Edition, text: &'a str) -> LexedStr<'a> {
        Self::lex_with::<FAST, false>(edition, text)
    }

    /// [`LexedStr::lex`], with `HASH` each identifier's name hashed too.
    fn lex_with<const FAST: bool, const HASH: bool>(edition: Edition, text: &'a str) -> LexedStr<'a> {
        let _p = tracing::info_span!("LexedStr::new").entered();
        assert!(text.len() <= u32::MAX as usize, "a text of 4 GiB or more");
        let mut conv = Converter::new(edition, text);
        if HASH {
            let mut names = scratch::lend_u64s();
            // About one token in three is an identifier.
            names.reserve(text.len() / 9);
            conv.res.names = Some(names);
        }
        if let Ok(script) = crate::frontmatter::ScriptSource::parse(text) {
            if let Some(shebang) = script.shebang_span() {
                conv.push(SHEBANG, shebang.end - shebang.start);
            }
            if script.frontmatter().is_some() {
                conv.push(FRONTMATTER, script.content_span().start - conv.offset);
            }
        } else if let Some(shebang_len) = rustc_lexer::strip_shebang(text) {
            // Leave error reporting to `rustc_lexer`
            conv.push(SHEBANG, shebang_len);
        }

        if FAST {
            // The classes go in the worker's scratch, so a warm lex allocates nothing for them.
            blaze_mem::scratch::with_u64s(|words| {
                let classes = classify::Classes::new(text.as_bytes(), words);
                conv.lex_rest::<HASH>(Some(&classes));
            });
        } else {
            conv.lex_rest::<HASH>(None);
        }
        conv.finalize_with_eof()
    }

    pub fn single_token(edition: Edition, text: &'a str) -> Option<(SyntaxKind, Option<String>)> {
        if text.is_empty() {
            return None;
        }

        let token = rustc_lexer::tokenize(text, rustc_lexer::FrontmatterAllowed::No).next()?;
        if token.len as usize != text.len() {
            return None;
        }

        let mut conv = Converter::new(edition, text);
        conv.extend_token(&token.kind, text);
        match conv.res.tokens.len() {
            // An edition's split (`c"x"`, `'r#a` before 2021, `#"x"` before 2024) leaves text.
            1 if conv.offset == text.len() => Some((conv.res.raw_kind(0), conv.res.error.pop().map(|it| it.msg.into_owned()))),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &str {
        self.text
    }

    pub fn len(&self) -> usize {
        self.tokens.len() - 1
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn kind(&self, i: usize) -> SyntaxKind {
        assert!(i < self.len());
        self.raw_kind(i)
    }

    pub fn text(&self, i: usize) -> &str {
        self.range_text(i..i + 1)
    }

    pub fn range_text(&self, r: ops::Range<usize>) -> &str {
        assert!(r.start < r.end && r.end <= self.len());
        let lo = self.text_start(r.start);
        let hi = lo + (r.start..r.end).map(|i| self.raw_len(i)).sum::<usize>();
        &self.text[lo..hi]
    }

    // Naming is hard.
    pub fn text_range(&self, i: usize) -> ops::Range<usize> {
        assert!(i < self.len());
        let lo = self.text_start(i);
        lo..lo + self.raw_len(i)
    }
    pub fn text_start(&self, i: usize) -> usize {
        assert!(i <= self.len());
        let checkpoint = i / CHECKPOINT;
        let from = checkpoint * CHECKPOINT;
        self.checkpoints[checkpoint] as usize + (from..i).map(|i| self.raw_len(i)).sum::<usize>()
    }
    pub fn text_len(&self, i: usize) -> usize {
        assert!(i < self.len());
        self.raw_len(i)
    }

    /// The length of token `i`, for walks that keep their own offset.
    #[inline]
    pub(crate) fn token_len(&self, i: usize) -> usize {
        debug_assert!(i < self.len());
        self.raw_len(i)
    }

    /// Each token's kind and length, but the closing `EOF`, in order: one pass over the
    /// token column, for walks that keep their own offset, with no index checked a token.
    #[inline]
    pub(crate) fn kinds_and_lens(&self) -> impl Iterator<Item = (SyntaxKind, usize)> + '_ {
        self.tokens[..self.len()].iter().enumerate().map(|(i, &word)| {
            let len = word >> KIND_BITS;
            let len = if len < LONG { len as usize } else { self.raw_len(i) };
            (SyntaxKind::from((word & KIND_MASK) as u16), len)
        })
    }

    #[inline]
    fn raw_kind(&self, i: usize) -> SyntaxKind {
        SyntaxKind::from((self.tokens[i] & KIND_MASK) as u16)
    }

    #[inline]
    fn raw_len(&self, i: usize) -> usize {
        let len = self.tokens[i] >> KIND_BITS;
        if len < LONG {
            return len as usize;
        }
        let at = self
            .long
            .binary_search_by_key(&(i as u32), |&(token, _)| token)
            .expect("a long token's length is in the side table");
        self.long[at].1 as usize
    }

    pub fn error(&self, i: usize) -> Option<&str> {
        assert!(i < self.len());
        let err = self.error.binary_search_by_key(&(i as u32), |i| i.token).ok()?;
        Some(&*self.error[err].msg)
    }

    pub fn errors(&self) -> impl Iterator<Item = (usize, &str)> + '_ {
        self.error.iter().map(|it| (it.token as usize, &*it.msg))
    }

    fn push(&mut self, kind: SyntaxKind, start: usize, len: usize) {
        if let Some(names) = self.names.as_deref_mut()
            && LexedStr::has_name_hash(kind)
        {
            // An identifier `scan::simple` did not lex: raw, non-ASCII, or split by an edition.
            let text = &self.text[start..start + len];
            let name = text.strip_prefix("r#").unwrap_or(text);
            names.push(blaze_phf::name_hash(name.as_bytes()));
        }
        let at = self.tokens.len();
        if at.is_multiple_of(CHECKPOINT) {
            self.checkpoints.push(start as u32);
        }
        let kind = kind as u16 as u32;
        match u32::try_from(len) {
            Ok(len) if len < LONG => self.tokens.push(kind | (len << KIND_BITS)),
            _ => {
                self.tokens.push(kind | (LONG << KIND_BITS));
                self.long.push((at as u32, len as u32));
            }
        }
    }
}

struct Converter<'a> {
    res: LexedStr<'a>,
    offset: usize,
    edition: Edition,
}

impl<'a> Converter<'a> {
    fn new(edition: Edition, text: &'a str) -> Self {
        let mut tokens = scratch::lend_u32s();
        tokens.reserve(text.len() / 3);
        let mut checkpoints = scratch::lend_u32s();
        checkpoints.reserve(text.len() / 3 / CHECKPOINT + 1);
        Self {
            res: LexedStr {
                text,
                checkpoints,
                tokens,
                names: None,
                // alloc: none until a token is 4 MiB or longer.
                long: Vec::new(),
                // alloc: none until the first error.
                error: Vec::new(),
            },
            offset: 0,
            edition,
        }
    }

    /// Lexes the rest of the text: with classes, the simple tokens a block at a time
    /// ([`scan::simple`]), each special one by [`scan::special`], and what neither decides by
    /// `rustc_lexer`; without, all by `rustc_lexer`. Lexes from scratch at every token
    /// `rustc_lexer` lexes, because some of its tokens are split as rustc splits them in older
    /// editions: `GuardedStrPrefix` before 2024, a C string and a raw lifetime before 2021.
    fn lex_rest<const HASH: bool>(&mut self, classes: Option<&classify::Classes<'_>>) {
        let text = self.res.text;
        while self.offset < text.len() {
            if let Some(classes) = classes {
                if classify::is_simple_first(text.as_bytes()[self.offset]) {
                    self.offset = scan::simple::<HASH>(&mut self.res, classes, self.edition, self.offset);
                    if self.offset == text.len() {
                        break;
                    }
                }
                if let Some((kind, len)) = scan::special(text, self.offset, classes) {
                    self.push(kind, len);
                    continue;
                }
            }
            let rest = &text[self.offset..];
            let token = rustc_lexer::tokenize(rest, rustc_lexer::FrontmatterAllowed::No)
                .next()
                .expect("a token where text is left");
            self.extend_token(&token.kind, &rest[..token.len as usize]);
        }
    }

    fn finalize_with_eof(mut self) -> LexedStr<'a> {
        self.res.push(EOF, self.offset, 0);
        self.res
    }

    fn push(&mut self, kind: SyntaxKind, len: usize) {
        self.res.push(kind, self.offset, len);
        self.offset += len;
    }

    /// An error of the token pushed next.
    fn error(&mut self, msg: impl Into<Cow<'static, str>>) {
        let msg = msg.into();
        if !msg.is_empty() {
            self.res.error.push(LexError { msg, token: self.res.tokens.len() as u32 });
        }
    }

    fn comment_kind(doc_style: Option<rustc_lexer::DocStyle>) -> SyntaxKind {
        match doc_style {
            Some(rustc_lexer::DocStyle::Outer) => OUTER_DOC_COMMENT,
            Some(rustc_lexer::DocStyle::Inner) => INNER_DOC_COMMENT,
            None => COMMENT,
        }
    }

    /// `token_text` is the converter's text from its offset on, so a token's start is
    /// `self.offset` in `self.res.text`.
    fn extend_token(&mut self, kind: &rustc_lexer::TokenKind, mut token_text: &str) {
        debug_assert_eq!(token_text.as_ptr(), self.res.text[self.offset..].as_ptr());
        // A note on an intended tradeoff:
        // We drop some useful information here (see patterns with double dots `..`)
        // Storing that info in `SyntaxKind` is not possible due to its layout requirements of
        // being `u16` that come from `syntax_tree::SyntaxKind`.
        let syntax_kind = {
            match kind {
                rustc_lexer::TokenKind::LineComment { doc_style } => Self::comment_kind(*doc_style),
                rustc_lexer::TokenKind::BlockComment { doc_style, terminated } => {
                    if !terminated {
                        self.error("Missing trailing `*/` symbols to terminate the block comment");
                    }
                    Self::comment_kind(*doc_style)
                }

                rustc_lexer::TokenKind::Frontmatter {
                    has_invalid_preceding_whitespace,
                    invalid_infostring,
                } => {
                    if *has_invalid_preceding_whitespace {
                        self.error("invalid preceding whitespace for frontmatter opening");
                    } else if *invalid_infostring {
                        self.error("invalid infostring for frontmatter");
                    }
                    FRONTMATTER
                }

                rustc_lexer::TokenKind::Whitespace => WHITESPACE,

                rustc_lexer::TokenKind::Ident => {
                    ident_kind(self.res.text, self.offset, token_text.len(), self.edition)
                }
                rustc_lexer::TokenKind::InvalidIdent => {
                    self.error("Ident contains invalid characters");
                    IDENT
                }

                rustc_lexer::TokenKind::RawIdent => {
                    let name = &token_text[2..];
                    if !can_be_raw(name) {
                        // alloc: the error path.
                        self.error(format!("`{name}` cannot be a raw identifier"));
                    }
                    IDENT
                }
                // `k#ident` is the identifier `k` and then `#ident`, as before 2021; from 2021
                // rustc lexes a forced keyword, gated by `forced_keywords`, which this tree has
                // no token for.
                rustc_lexer::TokenKind::ForcedKeywordIdent => {
                    if self.edition.at_least_2021() {
                        self.error("forced keywords are experimental");
                    }
                    token_text = &token_text[..1];
                    IDENT
                }

                rustc_lexer::TokenKind::GuardedStrPrefix if self.edition.at_least_2024() => {
                    // FIXME: rustc does something better for recovery.
                    self.error("Invalid string literal (reserved syntax)");
                    ERROR
                }
                rustc_lexer::TokenKind::GuardedStrPrefix => {
                    // The token is `#"` or `##`, split it into two.
                    token_text = &token_text[1..];
                    POUND
                }

                // Before 2021 `c"…"` is the identifier `c` and then the string, `cr#"…"#` the
                // identifier `cr` and then `#"…"#`.
                rustc_lexer::TokenKind::Literal {
                    kind: rustc_lexer::LiteralKind::CStr { .. },
                    ..
                } if !self.edition.at_least_2021() => {
                    token_text = &token_text[..1];
                    IDENT
                }
                rustc_lexer::TokenKind::Literal {
                    kind: rustc_lexer::LiteralKind::RawCStr { .. },
                    ..
                } if !self.edition.at_least_2021() => {
                    token_text = &token_text[..2];
                    IDENT
                }
                rustc_lexer::TokenKind::Literal { kind, .. } => {
                    self.extend_literal(token_text.len(), kind);
                    return;
                }

                rustc_lexer::TokenKind::Lifetime { starts_with_number } => {
                    if *starts_with_number {
                        self.error("Lifetime name cannot start with a number");
                    }
                    LIFETIME_IDENT
                }
                rustc_lexer::TokenKind::UnknownPrefixLifetime => {
                    if self.edition.at_least_2021() {
                        // alloc: the error path.
                        self.error(format!("prefix `{token_text}` is unknown"));
                    }
                    LIFETIME_IDENT
                }
                // Before 2021 `'r#a` is `'r`, `#` and `a`.
                rustc_lexer::TokenKind::RawLifetime if !self.edition.at_least_2021() => {
                    token_text = &token_text[..2];
                    LIFETIME_IDENT
                }
                rustc_lexer::TokenKind::RawLifetime => {
                    let end = self.offset + token_text.len();
                    if self.res.text.as_bytes().get(end) == Some(&b'\'') {
                        self.error("character literal may only contain one codepoint");
                    }
                    let name = &token_text[3..];
                    if !can_be_raw(name) {
                        // alloc: the error path.
                        self.error(format!("`{name}` cannot be a raw lifetime"));
                    }
                    LIFETIME_IDENT
                }

                rustc_lexer::TokenKind::Semi => T![;],
                rustc_lexer::TokenKind::Comma => T![,],
                rustc_lexer::TokenKind::Dot => T![.],
                rustc_lexer::TokenKind::OpenParen => T!['('],
                rustc_lexer::TokenKind::CloseParen => T![')'],
                rustc_lexer::TokenKind::OpenBrace => T!['{'],
                rustc_lexer::TokenKind::CloseBrace => T!['}'],
                rustc_lexer::TokenKind::OpenBracket => T!['['],
                rustc_lexer::TokenKind::CloseBracket => T![']'],
                rustc_lexer::TokenKind::At => T![@],
                rustc_lexer::TokenKind::Pound => T![#],
                rustc_lexer::TokenKind::Tilde => T![~],
                rustc_lexer::TokenKind::Question => T![?],
                rustc_lexer::TokenKind::Colon => T![:],
                rustc_lexer::TokenKind::Dollar => T![$],
                rustc_lexer::TokenKind::Eq => T![=],
                rustc_lexer::TokenKind::Bang => T![!],
                rustc_lexer::TokenKind::Lt => T![<],
                rustc_lexer::TokenKind::Gt => T![>],
                rustc_lexer::TokenKind::Minus => T![-],
                rustc_lexer::TokenKind::And => T![&],
                rustc_lexer::TokenKind::Or => T![|],
                rustc_lexer::TokenKind::Plus => T![+],
                rustc_lexer::TokenKind::Star => T![*],
                rustc_lexer::TokenKind::Slash => T![/],
                rustc_lexer::TokenKind::Caret => T![^],
                rustc_lexer::TokenKind::Percent => T![%],
                rustc_lexer::TokenKind::Unknown => ERROR,
                rustc_lexer::TokenKind::UnknownPrefix if token_text == "builtin" => IDENT,
                // The prefix is the identifier it spells, keyword or not, in every edition.
                rustc_lexer::TokenKind::UnknownPrefix => {
                    if self.edition.at_least_2021() {
                        // alloc: the error path.
                        self.error(format!("prefix `{token_text}` is unknown"));
                    }
                    ident_kind(self.res.text, self.offset, token_text.len(), self.edition)
                }
                rustc_lexer::TokenKind::Eof => EOF,
            }
        };

        self.push(syntax_kind, token_text.len());
    }

    fn extend_literal(&mut self, len: usize, kind: &rustc_lexer::LiteralKind) {
        const INVALID_RAW: &str = "Invalid raw string literal";
        let source: &'a str = self.res.text;
        let no_end_quote = |this: &mut Self, c: char, kind: &str| {
            // alloc: the error path (an unterminated literal).
            this.error(format!("Missing trailing `{c}` symbol to terminate the {kind} literal"));
        };

        let syntax_kind = match *kind {
            rustc_lexer::LiteralKind::Int { empty_int, base: _ } => {
                if empty_int {
                    self.error("Missing digits after the integer base prefix");
                }
                INT_NUMBER
            }
            rustc_lexer::LiteralKind::Float { empty_exponent, base: _ } => {
                if empty_exponent {
                    self.error("Missing digits after the exponent symbol");
                }
                FLOAT_NUMBER
            }
            rustc_lexer::LiteralKind::Char { terminated } => {
                if !terminated {
                    no_end_quote(self, '\'', "character");
                } else {
                    let text = &source[self.offset + 1..][..len - 1];
                    let text = &text[..text.rfind('\'').unwrap()];
                    if let Err(e) = unescape_char(text) {
                        self.error(err_to_msg(e, Mode::Char));
                    }
                }
                CHAR
            }
            rustc_lexer::LiteralKind::Byte { terminated } => {
                if !terminated {
                    no_end_quote(self, '\'', "byte");
                } else {
                    let text = &source[self.offset + 2..][..len - 2];
                    let text = &text[..text.rfind('\'').unwrap()];
                    if let Err(e) = unescape_byte(text) {
                        self.error(err_to_msg(e, Mode::Byte));
                    }
                }
                BYTE
            }
            rustc_lexer::LiteralKind::Str { terminated } => {
                if !terminated {
                    no_end_quote(self, '"', "string");
                } else {
                    let text = &source[self.offset + 1..][..len - 1];
                    let text = &text[..text.rfind('"').unwrap()];
                    unescape_str(text, |_, res| {
                        if let Err(e) = res {
                            self.error(err_to_msg(e, Mode::Str));
                        }
                    });
                }
                STRING
            }
            rustc_lexer::LiteralKind::ByteStr { terminated } => {
                if !terminated {
                    no_end_quote(self, '"', "byte string");
                } else {
                    let text = &source[self.offset + 2..][..len - 2];
                    let text = &text[..text.rfind('"').unwrap()];
                    unescape_byte_str(text, |_, res| {
                        if let Err(e) = res {
                            self.error(err_to_msg(e, Mode::ByteStr));
                        }
                    });
                }
                BYTE_STRING
            }
            rustc_lexer::LiteralKind::CStr { terminated } => {
                if !terminated {
                    no_end_quote(self, '"', "C string")
                } else {
                    let text = &source[self.offset + 2..][..len - 2];
                    let text = &text[..text.rfind('"').unwrap()];
                    unescape_c_str(text, |_, res| {
                        if let Err(e) = res {
                            self.error(err_to_msg(e, Mode::CStr));
                        }
                    });
                }
                C_STRING
            }
            rustc_lexer::LiteralKind::RawStr { n_hashes } => {
                if n_hashes.is_none() {
                    self.error(INVALID_RAW);
                }
                STRING
            }
            rustc_lexer::LiteralKind::RawByteStr { n_hashes } => {
                if n_hashes.is_none() {
                    self.error(INVALID_RAW);
                }
                BYTE_STRING
            }
            rustc_lexer::LiteralKind::RawCStr { n_hashes } => {
                if n_hashes.is_none() {
                    self.error(INVALID_RAW);
                }
                C_STRING
            }
        };

        self.push(syntax_kind, len);
    }
}

/// The kind of the identifier `text[start..start + len]`: `_`, a keyword of `edition`, or
/// `IDENT`. It reads the file's text in place, so the keyword probe can load 8 bytes at once.
#[inline]
fn ident_kind(text: &str, start: usize, len: usize, edition: Edition) -> SyntaxKind {
    let bytes = text.as_bytes();
    if len == 1 && bytes[start] == b'_' {
        UNDERSCORE
    } else {
        SyntaxKind::from_keyword_in(bytes, start, len, edition).unwrap_or(IDENT)
    }
}

/// rustc's `Symbol::can_be_raw`: not `_` and not a path segment keyword.
fn can_be_raw(name: &str) -> bool {
    !matches!(name, "" | "_" | "crate" | "self" | "Self" | "super")
}

fn err_to_msg(error: EscapeError, mode: Mode) -> &'static str {
    match error {
        EscapeError::ZeroChars => "empty character literal",
        EscapeError::MoreThanOneChar => "character literal may only contain one codepoint",
        EscapeError::LoneSlash => "",
        EscapeError::InvalidEscape if mode == Mode::Byte || mode == Mode::ByteStr => {
            "unknown byte escape"
        }
        EscapeError::InvalidEscape => "unknown character escape",
        EscapeError::BareCarriageReturn => "",
        EscapeError::BareCarriageReturnInRawString => "",
        EscapeError::EscapeOnlyChar if mode == Mode::Byte => "byte constant must be escaped",
        EscapeError::EscapeOnlyChar => "character constant must be escaped",
        EscapeError::TooShortHexEscape => "numeric character escape is too short",
        EscapeError::InvalidCharInHexEscape => "invalid character in numeric character escape",
        EscapeError::OutOfRangeHexEscape => "out of range hex escape",
        EscapeError::NoBraceInUnicodeEscape => "incorrect unicode escape sequence",
        EscapeError::InvalidCharInUnicodeEscape => "invalid character in unicode escape",
        EscapeError::EmptyUnicodeEscape => "empty unicode escape",
        EscapeError::UnclosedUnicodeEscape => "unterminated unicode escape",
        EscapeError::LeadingUnderscoreUnicodeEscape => "invalid start of unicode escape",
        EscapeError::OverlongUnicodeEscape => "overlong unicode escape",
        EscapeError::LoneSurrogateUnicodeEscape => "invalid unicode character escape",
        EscapeError::OutOfRangeUnicodeEscape => "invalid unicode character escape",
        EscapeError::UnicodeEscapeInByte => "unicode escape in byte string",
        EscapeError::NonAsciiCharInByte if mode == Mode::Byte => {
            "non-ASCII character in byte literal"
        }
        EscapeError::NonAsciiCharInByte if mode == Mode::ByteStr => {
            "non-ASCII character in byte string literal"
        }
        EscapeError::NonAsciiCharInByte => "non-ASCII character in raw byte string literal",
        EscapeError::NulInCStr => "null character in C string literal",
        EscapeError::UnskippedWhitespaceWarning => "",
        EscapeError::MultipleSkippedLinesWarning => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every start, range and text found by arithmetic is the one found by walking the tokens,
    /// across checkpoints and for tokens longer than a packed length holds.
    #[test]
    fn packed_tokens_answer_as_a_walk() {
        let long_string = format!("\"{}\"", "x".repeat(LONG as usize + 7));
        let long_comment = format!("/*{}*/", "y".repeat(LONG as usize * 2));
        let mut text = String::new();
        for i in 0..200 {
            text.push_str(&format!("fn f{i}(a: u32) -> u32 {{ a + {i} }} // {i}\n"));
            if i == 37 {
                text.push_str(&long_string);
            }
            if i == 150 {
                text.push_str(&long_comment);
                text.push('\n');
            }
        }
        let lexed = LexedStr::new(Edition::CURRENT, &text);
        assert!(lexed.long.len() == 2, "both long tokens are in the side table");
        let mut start = 0;
        for i in 0..lexed.len() {
            let len = lexed.text_len(i);
            assert_eq!(lexed.text_start(i), start);
            assert_eq!(lexed.text_range(i), start..start + len);
            assert_eq!(lexed.text(i), &text[start..start + len]);
            start += len;
        }
        assert_eq!(start, text.len());
        assert_eq!(lexed.text_start(lexed.len()), text.len());
        assert_eq!(lexed.range_text(0..lexed.len()), text);
        let kinds: Vec<_> = (0..lexed.len()).map(|i| lexed.kind(i)).collect();
        assert!(kinds.contains(&STRING) && kinds.contains(&COMMENT) && kinds.contains(&FN_KW));
        assert_eq!(LexedStr::single_token(Edition::CURRENT, &long_string).map(|it| it.0), Some(STRING));
    }

    fn tokens(edition: Edition, text: &str) -> Vec<(SyntaxKind, &str, Option<String>)> {
        let lexed = LexedStr::new(edition, text);
        let mut start = 0;
        (0..lexed.len())
            .map(|i| {
                let len = lexed.text_len(i);
                let error = lexed.error(i).map(str::to_owned);
                let token = (lexed.kind(i), &text[start..start + len], error);
                start += len;
                token
            })
            .collect()
    }

    /// rustc's `report_unknown_prefix`: an error from 2021, before it only the lint, and the
    /// prefix is the identifier or lifetime it spells in every edition.
    #[test]
    fn unknown_prefixes_are_errors_from_2021() {
        for edition in Edition::iter() {
            let error = |prefix: &str| {
                edition.at_least_2021().then(|| format!("prefix `{prefix}` is unknown"))
            };
            let got = tokens(edition, "z\"hey\" hey#1 'foo#x match\"x\"");
            let kinds: Vec<_> = got.iter().map(|it| (it.0, it.1)).collect();
            assert_eq!(
                kinds,
                [
                    (IDENT, "z"),
                    (STRING, "\"hey\""),
                    (WHITESPACE, " "),
                    (IDENT, "hey"),
                    (POUND, "#"),
                    (INT_NUMBER, "1"),
                    (WHITESPACE, " "),
                    (LIFETIME_IDENT, "'foo"),
                    (POUND, "#"),
                    (IDENT, "x"),
                    (WHITESPACE, " "),
                    (MATCH_KW, "match"),
                    (STRING, "\"x\""),
                ],
                "{edition:?}"
            );
            for (at, prefix) in [(0, "z"), (3, "hey"), (7, "'foo"), (11, "match")] {
                assert_eq!(got[at].2, error(prefix), "{edition:?} {prefix}");
            }
            assert_eq!(tokens(edition, "builtin#x")[0], (IDENT, "builtin", None));
        }
    }

    /// rustc's `next_token_from_cursor` before 2021: `c"…"` and `cr"…"` are the identifier and
    /// then the rest, lexed again; from 2021 one C string.
    #[test]
    fn c_strings_are_split_before_2021() {
        for edition in Edition::iter() {
            let got = tokens(edition, "c\"x\" cr\"y\" cr#\"z\"#");
            if edition.at_least_2021() {
                assert_eq!(
                    got,
                    [
                        (C_STRING, "c\"x\"", None),
                        (WHITESPACE, " ", None),
                        (C_STRING, "cr\"y\"", None),
                        (WHITESPACE, " ", None),
                        (C_STRING, "cr#\"z\"#", None),
                    ]
                );
                assert_eq!(LexedStr::single_token(edition, "c\"x\""), Some((C_STRING, None)));
            } else {
                assert_eq!(
                    got,
                    [
                        (IDENT, "c", None),
                        (STRING, "\"x\"", None),
                        (WHITESPACE, " ", None),
                        (IDENT, "cr", None),
                        (STRING, "\"y\"", None),
                        (WHITESPACE, " ", None),
                        (IDENT, "cr", None),
                        (POUND, "#", None),
                        (STRING, "\"z\"", None),
                        (POUND, "#", None),
                    ],
                    "{edition:?}"
                );
                assert_eq!(LexedStr::single_token(edition, "c\"x\""), None, "{edition:?}");
            }
        }
    }

    /// rustc's `RawLifetime` arm: before 2021 `'r`, `#` and the name; from 2021 one lifetime,
    /// with rustc's errors for a quote after it and for a name that can't be raw.
    #[test]
    fn raw_lifetimes_are_split_before_2021() {
        for edition in [Edition::Edition2015, Edition::Edition2018] {
            assert_eq!(
                tokens(edition, "'r#lt"),
                [(LIFETIME_IDENT, "'r", None), (POUND, "#", None), (IDENT, "lt", None)]
            );
            assert_eq!(tokens(edition, "'r#_")[0], (LIFETIME_IDENT, "'r", None));
            assert_eq!(LexedStr::single_token(edition, "'r#lt"), None);
        }
        for edition in [Edition::Edition2021, Edition::Edition2024] {
            assert_eq!(tokens(edition, "'r#lt"), [(LIFETIME_IDENT, "'r#lt", None)]);
            assert_eq!(
                LexedStr::single_token(edition, "'r#lt"),
                Some((LIFETIME_IDENT, None))
            );
            for name in ["_", "self", "Self", "super", "crate"] {
                let text = format!("'r#{name}");
                let message = format!("`{name}` cannot be a raw lifetime");
                assert_eq!(
                    tokens(edition, &text),
                    [(LIFETIME_IDENT, &*text, Some(message))],
                    "{edition:?}"
                );
            }
            let got = tokens(edition, "'r#a'");
            assert_eq!(
                got[0],
                (
                    LIFETIME_IDENT,
                    "'r#a",
                    Some("character literal may only contain one codepoint".to_owned())
                )
            );
        }
    }

    /// rustc's `CannotBeRawIdent`, in every edition.
    #[test]
    fn path_keywords_cannot_be_raw_identifiers() {
        for edition in Edition::iter() {
            for name in ["_", "self", "Self", "super", "crate"] {
                let text = format!("r#{name}");
                let message = format!("`{name}` cannot be a raw identifier");
                assert_eq!(tokens(edition, &text), [(IDENT, &*text, Some(message))]);
            }
            assert_eq!(tokens(edition, "r#fn r#async"), [
                (IDENT, "r#fn", None),
                (WHITESPACE, " ", None),
                (IDENT, "r#async", None),
            ]);
        }
    }

    /// `k#fn` is `k`, `#` and `fn` in every edition, and from 2021 the error of rustc's
    /// `forced_keywords` gate on the `k`.
    #[test]
    fn forced_keywords_split_and_are_gated_from_2021() {
        for edition in Edition::iter() {
            let error = edition.at_least_2021().then(|| "forced keywords are experimental".to_owned());
            assert_eq!(
                tokens(edition, "k#fn"),
                [(IDENT, "k", error), (POUND, "#", None), (FN_KW, "fn", None)],
                "{edition:?}"
            );
            assert_eq!(LexedStr::single_token(edition, "k#fn"), None);
        }
    }

    /// Two lexings' input, one after the other, is the input of their texts lexed as one, split
    /// at any token boundary.
    #[test]
    fn input_followed_by_is_the_joined_input() {
        let text = "fn f() -> u8 { x.0.1 + 1. ; union::default; a::<'b>>=c /* d */ }";
        let edition = Edition::CURRENT;
        let whole = LexedStr::new(edition, text);
        let input = |it: &crate::Input| {
            (0..it.len())
                .map(|i| (it.kind(i), it.contextual_kind(i), it.is_joint(i)))
                .collect::<Vec<_>>()
        };
        let expected = input(&whole.to_input(edition));
        for split in 0..=whole.len() {
            let at = whole.text_start(split);
            let (head, tail) = (LexedStr::new(edition, &text[..at]), LexedStr::new(edition, &text[at..]));
            if head.len() + tail.len() != whole.len() {
                continue;
            }
            assert_eq!(input(&head.to_input_followed_by(&tail, edition)), expected, "split at {at}");
        }
    }

    /// `to_input` from the column in one pass is the input token by token, `kind`, `text` and
    /// all: trivia, contextual keywords in every edition, floats with and without a last `.`,
    /// a token past the packed length, and the parser's own sources.
    #[test]
    fn the_input_in_one_pass_is_the_input_token_by_token() {
        let mut texts = super::differential::parser_files();
        texts.push("fn f() { x.0.1 + 1. + 1.5 + 1e3; union::default; dyn auto macro_rules! /* c */ }".to_owned());
        texts.push(format!("fn f() {{ \"{}\" ; 2. }}", "s".repeat((1 << 22) + 3)));
        for text in &texts {
            for edition in Edition::iter() {
                let lexed = LexedStr::new(edition, text);
                let mut expected = crate::Input::with_capacity(lexed.len());
                let mut was_joint = false;
                for i in 0..lexed.len() {
                    let (kind, text) = (lexed.kind(i), lexed.text(i));
                    if kind.is_trivia() {
                        was_joint = false;
                    } else if kind == IDENT {
                        expected.push_ident(SyntaxKind::from_contextual_keyword(text, edition).unwrap_or(IDENT), edition);
                    } else {
                        if was_joint {
                            expected.was_joint();
                        }
                        expected.push(kind, edition);
                        if kind != FLOAT_NUMBER {
                            was_joint = true;
                        } else if !text.ends_with('.') {
                            expected.was_joint();
                        } else {
                            was_joint = false;
                        }
                    }
                }
                let got = lexed.to_input(edition);
                assert_eq!(got.len(), expected.len(), "{edition:?}");
                for i in 0..got.len() {
                    let at = |it: &crate::Input| (it.kind(i), it.contextual_kind(i), it.is_joint(i));
                    assert_eq!(at(&got), at(&expected), "token {i} in {edition:?}");
                }
            }
        }
    }
}
