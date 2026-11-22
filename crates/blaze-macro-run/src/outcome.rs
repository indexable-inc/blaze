//! THE OUTCOME: what the macro's process gives back, as bytes, and the host's check of them.
//!
//! The server writes it (server.rs `outcome`) and the session returns it in blaze-comptime's
//! `Return`, so it is the run's `Done::output` and the equality harness compares it byte for byte
//! (piece 5). The host reads it with [`decode`] into blaze-expand's [`Produced`], the outcome
//! `ExpandCx::run_proc_macro` already returns (rule 30), and takes nothing unchecked:
//!
//! - every origin is one this call handed out: an input token's or an attribute argument's
//!   index under the call's lengths, or a site's transparency (`Made`). `Def` and `Params` are
//!   refused: no proc macro has a definition body, and an origin of one is a forged mark. No
//!   origin names a crate or a macro, so none can claim another's `$crate` or definition site;
//!   blaze-expand gives each token its real mark from this call.
//! - every identifier and literal is a token blaze's lexer makes of its printed form, in the
//!   call's edition: all of them printed apart into one text, lexed once, and compared token for
//!   token (kind, raw-ness, literal kind, suffix length and text). A token copied whole from the
//!   call's input (its origin's token, with the same kind and text) is the host's own and is not
//!   lexed again: that is how `$crate`, which no text lexes as one identifier, passes through.
//! - every `Punct` is one of rustc's punctuation characters; the delimiters pair.
//! - diagnostics are text only (their level, their first span's origin, their message), each
//!   message and all of them together under [`OutcomeLimits`].
//!
//! # Bytes
//!
//! `FORMAT`, then a tag: expanded (0), panicked (1) or stopped (2). An expanded or panicked
//! outcome is a list of tokens (kind, origin, text for an identifier or a literal), a list of
//! diagnostics (level, an optional origin, message), and for a panic its message. A stopped one is
//! the server's [`ServerError`]. Lists are a `usize` count then their items, every number
//! little-endian, as bridge.rs writes them.

use core::fmt;

use blaze_expand::{
    Ctx, Delim, Edition, LexError, LitKind, ProcDiagnostic, ProcLevel, Produced, Reads, StreamError, Text, TokenKind, TokenOrigin,
    TokenStream, lex,
};
use blaze_own::{AVec, Alloc, Vec};

use crate::bridge::{Field, Malformed, Reader, Writer, index, wide};
use crate::call::{FORMAT, has_text, put_kind, put_origin, read_kind, read_origin};
use crate::server::{ServerError, is_punct, level_of, write_literal};

const EXPANDED: u8 = 0;
const PANICKED: u8 = 1;
const STOPPED: u8 = 2;

/// Writes an outcome: [`OutcomeWriter::token`] for each token, then
/// [`OutcomeWriter::diagnostic`] for each diagnostic, then [`OutcomeWriter::end`]. The server is
/// its writer; a test writes a hostile one with it.
pub struct OutcomeWriter<'w, 'a> {
    out: &'w mut AVec<'a, u8>,
    panicked: bool,
    /// Where the open list's count is.
    at: usize,
    count: u64,
    in_tokens: bool,
}

impl<'w, 'a> OutcomeWriter<'w, 'a> {
    /// An outcome onto `out` (cleared first): a panic's when `panicked`.
    pub fn start(out: &'w mut AVec<'a, u8>, panicked: bool) -> OutcomeWriter<'w, 'a> {
        out.clear();
        out.push(FORMAT);
        out.push(if panicked { PANICKED } else { EXPANDED });
        let at = out.len();
        Writer { out: &mut *out }.usize(0);
        OutcomeWriter { out, panicked, at, count: 0, in_tokens: true }
    }

    /// A token; an identifier's or a literal's text is `text`'s parts end to end.
    pub fn token(&mut self, kind: TokenKind, origin: TokenOrigin, text: &[&str]) {
        let mut w = Writer { out: &mut *self.out };
        put_kind(&mut w, kind);
        put_origin(&mut w, origin);
        if has_text(kind) {
            w.str_parts(text);
        }
        self.count += 1;
    }

    fn close_list(&mut self) {
        let (at, count) = (self.at, self.count);
        Writer { out: &mut *self.out }.set_usize(at, count);
    }

    /// Ends the tokens; the diagnostics follow.
    pub fn diagnostics(&mut self) {
        if self.in_tokens {
            self.close_list();
            self.in_tokens = false;
            self.at = self.out.len();
            self.count = 0;
            Writer { out: &mut *self.out }.usize(0);
        }
    }

    pub fn diagnostic(&mut self, level: ProcLevel, at: Option<TokenOrigin>, message: &str) {
        self.diagnostics();
        let mut w = Writer { out: &mut *self.out };
        w.u8(level_tag(level));
        w.some(at.is_some());
        if let Some(origin) = at {
            put_origin(&mut w, origin);
        }
        w.str(message);
        self.count += 1;
    }

    /// Ends the outcome; a panic's gets `message`.
    pub fn end(mut self, message: Option<&str>) {
        self.diagnostics();
        self.close_list();
        if self.panicked {
            Writer { out: &mut *self.out }.str(message.unwrap_or(""));
        }
    }
}

/// The stopped outcome of `error`, onto `out` (cleared first).
pub fn write_stopped(out: &mut AVec<'_, u8>, error: ServerError) {
    out.clear();
    out.push(FORMAT);
    out.push(STOPPED);
    error.write(&mut Writer { out });
}

fn level_tag(level: ProcLevel) -> u8 {
    match level {
        ProcLevel::Error => 0,
        ProcLevel::Warning => 1,
        ProcLevel::Note => 2,
        ProcLevel::Help => 3,
    }
}

/// The host's caps on what an outcome holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct OutcomeLimits {
    /// Tokens (the server's own cap, server.rs `ServerLimits`).
    pub most_tokens: u32,
    /// Bytes of identifier and literal text, all together.
    pub most_text: u32,
    pub most_diagnostics: u32,
    /// Bytes of one diagnostic's (or the panic's) message.
    pub most_message: u32,
    /// Bytes of all the messages together.
    pub most_diagnostic_text: u32,
}

impl OutcomeLimits {
    pub const SHIPPED: OutcomeLimits =
        OutcomeLimits { most_tokens: 1 << 24, most_text: 1 << 28, most_diagnostics: 4096, most_message: 64 << 10, most_diagnostic_text: 1 << 20 };
}

/// What an [`OutcomeError::TooMany`] counted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Counted {
    Tokens,
    Text,
    Diagnostics,
    Message,
    DiagnosticText,
}

/// Where in an outcome a refused mark is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OutcomeAt {
    Token(u32),
    Diagnostic(u32),
}

/// Why the host refused an outcome.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OutcomeError {
    /// Bytes that are not the format's.
    Malformed(Malformed),
    /// An origin this call did not hand out: a definition's token, or an index past the call's
    /// input or attribute arguments.
    ForgedMark { at: OutcomeAt, origin: TokenOrigin },
    /// An identifier or literal that is not the token blaze's lexer makes of its text.
    NotAToken { at: u32 },
    /// A `Punct` that is not one of rustc's punctuation characters.
    BadPunct { at: u32, char: char },
    /// A closing delimiter that does not close the open group, or a group left open.
    Unbalanced { at: u32 },
    TooMany { what: Counted, most: u32 },
    /// The tokens do not fit a stream.
    Stream(StreamError),
}

impl fmt::Display for OutcomeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OutcomeError::Malformed(what) => write!(f, "gave an outcome with {what}"),
            OutcomeError::ForgedMark { at, origin } => {
                let (place, n) = match at {
                    OutcomeAt::Token(n) => ("token", n),
                    OutcomeAt::Diagnostic(n) => ("diagnostic", n),
                };
                write!(f, "gave its {place} {n} the origin {origin:?}, which this call did not hand out: a forged hygiene mark")
            }
            OutcomeError::NotAToken { at } => write!(f, "gave as its token {at} text blaze's lexer does not make that token of"),
            OutcomeError::BadPunct { at, char } => write!(f, "gave as its token {at} the punctuation {char:?}, which is none of rustc's"),
            OutcomeError::Unbalanced { at } => write!(f, "gave tokens whose delimiters do not pair at its token {at}"),
            OutcomeError::TooMany { what, most } => write!(f, "gave more {what:?} than the {most} an outcome may hold"),
            OutcomeError::Stream(error) => write!(f, "gave tokens that do not fit a stream: {error}"),
        }
    }
}

/// What the host checks an outcome against: the call it answered.
#[derive(Clone, Copy)]
pub(crate) struct Checking<'i, 's> {
    pub(crate) input: &'i TokenStream<'s>,
    pub(crate) attr: Option<&'i TokenStream<'s>>,
    pub(crate) edition: Edition,
}

impl Checking<'_, '_> {
    fn allowed(&self, origin: TokenOrigin) -> bool {
        match origin {
            TokenOrigin::Input(at) => wide(at) < self.input.tokens().len(),
            TokenOrigin::AttrArgs(at) => self.attr.is_some_and(|attr| wide(at) < attr.tokens().len()),
            TokenOrigin::Made(_) => true,
            TokenOrigin::Def(_) | TokenOrigin::Params(_) => false,
        }
    }

    /// Whether the token is its origin's input token, copied whole.
    fn copied(&self, kind: TokenKind, text: &str, origin: TokenOrigin) -> bool {
        let (stream, at) = match origin {
            TokenOrigin::Input(at) => (self.input, at),
            TokenOrigin::AttrArgs(at) => match self.attr {
                Some(attr) => (attr, at),
                None => return false,
            },
            _ => return false,
        };
        stream.tokens().get(wide(at)).is_some_and(|token| same_kind(token.kind, kind) && stream.text_of(token) == text)
    }
}

/// An outcome, read and checked.
pub(crate) enum Decoded<'a> {
    Produced(Produced<'a>),
    /// The server's own account of why it stopped.
    Stopped(ServerError),
}

/// `bytes` read as an outcome of the call `check`, every token, mark and diagnostic checked;
/// what is kept is in `alloc`, what the check needs only for itself in `scratch`.
pub(crate) fn decode<'a>(
    bytes: &[u8],
    check: &Checking<'_, '_>,
    limits: OutcomeLimits,
    alloc: Alloc<'a>,
    scratch: Alloc<'_>,
) -> Result<Decoded<'a>, OutcomeError> {
    let m = OutcomeError::Malformed;
    let mut r = Reader::new(bytes);
    let version = r.tag(Field::Version).map_err(m)?;
    if version != FORMAT {
        return Err(m(Malformed::Tag(Field::Version, version)));
    }
    let panicked = match r.tag(Field::OutcomeTag).map_err(m)? {
        EXPANDED => false,
        PANICKED => true,
        STOPPED => {
            let error = ServerError::read(&mut r).map_err(m)?;
            r.end().map_err(m)?;
            return Ok(Decoded::Stopped(error));
        }
        other => return Err(m(Malformed::Tag(Field::OutcomeTag, other))),
    };
    let count = r.count().map_err(m)?;
    if count > wide(limits.most_tokens) {
        return Err(OutcomeError::TooMany { what: Counted::Tokens, most: limits.most_tokens });
    }
    let ctx = Ctx::root(check.edition);
    let mut tokens = TokenStream::new_in(alloc);
    let mut origins: AVec<'a, TokenOrigin> = Vec::with_capacity_in(count, alloc);
    let mut open: AVec<'_, Delim> = Vec::new_in(scratch);
    // The identifiers and literals to lex, printed apart, and their indexes.
    let mut printed: AVec<'_, u8> = Vec::new_in(scratch);
    let mut checks: AVec<'_, u32> = Vec::new_in(scratch);
    let mut text_total = 0usize;
    for at in 0..count {
        let at = index(at);
        let kind = read_kind(&mut r).map_err(m)?;
        let origin = read_origin(&mut r).map_err(m)?;
        let text = if has_text(kind) { r.str().map_err(m)? } else { "" };
        if !check.allowed(origin) {
            return Err(OutcomeError::ForgedMark { at: OutcomeAt::Token(at), origin });
        }
        text_total += text.len();
        if text_total > wide(limits.most_text) {
            return Err(OutcomeError::TooMany { what: Counted::Text, most: limits.most_text });
        }
        match kind {
            TokenKind::Open(delim) => open.push(delim),
            TokenKind::Close(delim) => {
                if open.pop() != Some(delim) {
                    return Err(OutcomeError::Unbalanced { at });
                }
            }
            TokenKind::Punct { char, .. } => {
                if !u8::try_from(char).is_ok_and(is_punct) {
                    return Err(OutcomeError::BadPunct { at, char });
                }
            }
            TokenKind::Ident { .. } | TokenKind::Literal { .. } => {
                if !check.copied(kind, text, origin) {
                    if !printed.is_empty() {
                        printed.push(b' ');
                    }
                    print_token(&mut printed, kind, text).ok_or(OutcomeError::NotAToken { at })?;
                    checks.push(at);
                }
            }
        }
        tokens.push(kind, text, ctx).map_err(OutcomeError::Stream)?;
        origins.push(origin);
    }
    if !open.is_empty() {
        return Err(OutcomeError::Unbalanced { at: index(count) });
    }
    relex(&printed[..], &checks[..], &tokens, check.edition, scratch)?;

    let count = r.count().map_err(m)?;
    if count > wide(limits.most_diagnostics) {
        return Err(OutcomeError::TooMany { what: Counted::Diagnostics, most: limits.most_diagnostics });
    }
    let mut text: AVec<'a, u8> = Vec::new_in(alloc);
    let mut diagnostics: AVec<'a, ProcDiagnostic> = Vec::with_capacity_in(count + usize::from(panicked), alloc);
    for at in 0..count {
        let level = level_of(r.tag(Field::Level).map_err(m)?).map_err(m)?;
        let origin = if r.some().map_err(m)? {
            let origin = read_origin(&mut r).map_err(m)?;
            if !check.allowed(origin) {
                return Err(OutcomeError::ForgedMark { at: OutcomeAt::Diagnostic(index(at)), origin });
            }
            Some(origin)
        } else {
            None
        };
        let message = push_message(&mut text, r.str().map_err(m)?, limits)?;
        diagnostics.push(ProcDiagnostic::Emitted { level, message, at: origin });
    }
    if panicked {
        let message = push_message(&mut text, r.str().map_err(m)?, limits)?;
        diagnostics.push(ProcDiagnostic::Panicked { message });
    }
    r.end().map_err(m)?;
    Ok(Decoded::Produced(Produced { tokens, origins, reads: Reads::new_in(alloc), diagnostics, text }))
}

fn push_message(text: &mut AVec<'_, u8>, message: &str, limits: OutcomeLimits) -> Result<Text, OutcomeError> {
    if message.len() > wide(limits.most_message) {
        return Err(OutcomeError::TooMany { what: Counted::Message, most: limits.most_message });
    }
    if text.len() + message.len() > wide(limits.most_diagnostic_text) {
        return Err(OutcomeError::TooMany { what: Counted::DiagnosticText, most: limits.most_diagnostic_text });
    }
    let at = index(text.len());
    text.extend_from_slice(message.as_bytes());
    Ok(Text { at, len: index(message.len()) })
}

/// Two tokens of one kind, spacing aside: an identifier's raw-ness, a literal's kind and suffix.
fn same_kind(a: TokenKind, b: TokenKind) -> bool {
    match (a, b) {
        (TokenKind::Ident { raw: ra, .. }, TokenKind::Ident { raw: rb, .. }) => ra == rb,
        (TokenKind::Literal { kind: ka, suffix_len: sa, .. }, TokenKind::Literal { kind: kb, suffix_len: sb, .. }) => {
            ka == kb && sa == sb && ka != LitKind::Err
        }
        _ => false,
    }
}

/// An identifier's or literal's source form, onto `out`; `None` for a literal whose suffix does
/// not fit its text or whose kind is the lexer's error kind.
fn print_token(out: &mut AVec<'_, u8>, kind: TokenKind, text: &str) -> Option<()> {
    match kind {
        TokenKind::Ident { raw, .. } => {
            if raw == blaze_expand::Raw::Yes {
                out.extend_from_slice(b"r#");
            }
            out.extend_from_slice(text.as_bytes());
        }
        TokenKind::Literal { kind: LitKind::Err, .. } => return None,
        TokenKind::Literal { kind, suffix_len, .. } => {
            let split = text.len().checked_sub(wide(suffix_len))?;
            let (inside, suffix) = text.split_at_checked(split)?;
            write_literal(out, kind, inside, suffix);
        }
        TokenKind::Punct { .. } | TokenKind::Open(_) | TokenKind::Close(_) => return None,
    }
    Some(())
}

/// The printed identifiers and literals lexed once, each lexed token held to the outcome's.
fn relex(printed: &[u8], checks: &[u32], tokens: &TokenStream<'_>, edition: Edition, scratch: Alloc<'_>) -> Result<(), OutcomeError> {
    if checks.is_empty() {
        return Ok(());
    }
    let text = core::str::from_utf8(printed).map_err(|_| OutcomeError::Malformed(Malformed::NotUtf8))?;
    let lexed = match lex(text, edition, scratch) {
        Ok(lexed) => lexed,
        Err(LexError::Stream(error)) => return Err(OutcomeError::Stream(error)),
        // The lexer refuses the text: name the first token it refuses alone.
        Err(LexError::Refused { .. }) => return Err(OutcomeError::NotAToken { at: first_alone(checks, tokens, edition, scratch) }),
    };
    let got = lexed.tokens();
    for (k, at) in checks.iter().enumerate() {
        let Some(want) = tokens.tokens().get(wide(*at)) else { return Err(OutcomeError::NotAToken { at: *at }) };
        match got.get(k) {
            Some(token) if same_kind(token.kind, want.kind) && lexed.text_of(token) == tokens.text_of(want) => {}
            _ => return Err(OutcomeError::NotAToken { at: *at }),
        }
    }
    if got.len() != checks.len() {
        return Err(OutcomeError::NotAToken { at: checks.last().copied().unwrap_or(0) });
    }
    Ok(())
}

/// The first checked token that does not lex alone as itself.
fn first_alone(checks: &[u32], tokens: &TokenStream<'_>, edition: Edition, scratch: Alloc<'_>) -> u32 {
    let mut one: AVec<'_, u8> = Vec::new_in(scratch);
    for at in checks {
        let Some(want) = tokens.tokens().get(wide(*at)) else { return *at };
        one.clear();
        let text = tokens.text_of(want);
        if print_token(&mut one, want.kind, text).is_none() {
            return *at;
        }
        let alone = core::str::from_utf8(&one[..]).ok().and_then(|t| lex(t, edition, scratch).ok());
        let same = alone.is_some_and(|lexed| match lexed.tokens() {
            [token] => same_kind(token.kind, want.kind) && lexed.text_of(token) == text,
            _ => false,
        });
        if !same {
            return *at;
        }
    }
    checks.first().copied().unwrap_or(0)
}
