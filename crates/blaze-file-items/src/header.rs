//! [`impl_header`]: an impl's header read from its tokens, the one reader of it.
//!
//! The header is the text of an `impl` item from its first modifier to the end of its
//! signature, the body not included (an outline's `Item::signature`, its outer attributes
//! left out). It is read by tokens, not by text, so a line break where a space was (`for`,
//! `where` on their own lines) and the forms of the pinned nightly read as the plain ones:
//! `const impl<T> Trait for T` and `impl<T> const Trait for T` are both a const impl.

use blaze_own::{AVec, Alloc};
use blaze_surface::{Edition, ImplFlags};
use parser::SyntaxKind::{
    self, COMMA, CONST_KW, FOR_KW, IDENT, IMPL_KW, L_ANGLE, L_BRACK, L_CURLY, L_PAREN,
    LIFETIME_IDENT, MINUS, R_ANGLE, R_BRACK, R_CURLY, R_PAREN, SEMICOLON, UNSAFE_KW, WHERE_KW,
};

use crate::parser_edition;

/// An impl's header: its flags, its type and const parameters' names, the trait it implements
/// (none for an inherent impl) and its self type, each a slice of the header's text, trimmed.
#[derive(Debug)]
pub struct ImplHeader<'t, 'a> {
    pub flags: ImplFlags,
    /// The generic parameters' names, in order; lifetimes left out.
    pub params: AVec<'a, &'t str>,
    /// The trait's path, without a negative impl's `!`.
    pub trait_: Option<&'t str>,
    pub self_ty: &'t str,
}

/// Why a header did not read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HeaderError {
    /// The lexer refused the text.
    Lex,
    /// No `impl` keyword after the modifiers.
    NoImpl,
    /// A `<`, `(`, `[` or `{` the text does not close.
    Unclosed,
    /// Nothing after `impl` (and `for`) for the self type.
    NoSelfType,
}

impl core::fmt::Display for HeaderError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            HeaderError::Lex => "the impl header does not lex",
            HeaderError::NoImpl => "no `impl` after the header's modifiers",
            HeaderError::Unclosed => "the impl header leaves a bracket open",
            HeaderError::NoSelfType => "the impl header names no self type",
        })
    }
}

impl core::error::Error for HeaderError {}

/// A token of the header: its kind and where its text is.
#[derive(Clone, Copy)]
struct Tok {
    kind: SyntaxKind,
    start: usize,
    end: usize,
}

/// The header `text` of an impl item read in `edition`, its parameter names in `alloc`.
pub fn impl_header<'t, 'a>(
    text: &'t str,
    edition: Edition,
    alloc: Alloc<'a>,
) -> Result<ImplHeader<'t, 'a>, HeaderError> {
    let lexed = parser::LexedStr::new(parser_edition(edition), text);
    if lexed.errors().next().is_some() {
        return Err(HeaderError::Lex);
    }
    let mut toks: AVec<'a, Tok> = AVec::with_capacity_in(lexed.len(), alloc);
    let mut offset = 0;
    for i in 0..lexed.len() {
        let start = offset;
        offset += lexed.text_len(i);
        let kind = lexed.kind(i);
        if !kind.is_trivia() {
            toks.push(Tok { kind, start, end: offset });
        }
    }
    let word = |tok: &Tok| text.get(tok.start..tok.end).unwrap_or("");
    let mut flags = ImplFlags::NONE;
    let mut at = 0;
    // `default`, `unsafe` and `const` before `impl`, in any order rustc's parser takes.
    loop {
        match toks.get(at) {
            Some(tok) if tok.kind == UNSAFE_KW => flags = flags.union(ImplFlags::UNSAFE),
            Some(tok) if tok.kind == CONST_KW => flags = flags.union(ImplFlags::CONST),
            Some(tok) if tok.kind == IDENT && word(tok) == "default" => {
                flags = flags.union(ImplFlags::DEFAULT);
            }
            _ => break,
        }
        at += 1;
    }
    if toks.get(at).map(|it| it.kind) != Some(IMPL_KW) {
        return Err(HeaderError::NoImpl);
    }
    at += 1;
    let mut params: AVec<'a, &'t str> = AVec::new_in(alloc);
    if toks.get(at).map(|it| it.kind) == Some(L_ANGLE) {
        let close = closing(&toks, at)?;
        // Each parameter starts after `<` or a comma at the generics' own depth.
        let mut depth = 0usize;
        let mut first = true;
        for i in at + 1..close {
            let tok = toks[i];
            if first {
                let name = match tok.kind {
                    LIFETIME_IDENT => None,
                    CONST_KW => toks.get(i + 1).filter(|it| it.kind == IDENT).map(word),
                    IDENT => Some(word(&tok)),
                    _ => None,
                };
                params.extend(name);
                first = false;
            }
            match step(&toks, i) {
                Step::Open => depth += 1,
                Step::Close => depth = depth.saturating_sub(1),
                Step::Other if tok.kind == COMMA && depth == 0 => first = true,
                Step::Other | Step::Arrow => {}
            }
        }
        at = close + 1;
    }
    if toks.get(at).map(|it| it.kind) == Some(CONST_KW) {
        flags = flags.union(ImplFlags::CONST);
        at += 1;
    }
    let negative = toks.get(at).is_some_and(|it| it.kind == SyntaxKind::BANG);
    if negative {
        flags = flags.union(ImplFlags::NEGATIVE);
        at += 1;
    }
    // The trait ends at the first `for` at depth 0; the self type at `where`, `{` or `;`.
    let mut depth = 0usize;
    let mut split = None;
    let mut end = toks.len();
    for i in at..toks.len() {
        let kind = toks[i].kind;
        if depth == 0 {
            if kind == FOR_KW && split.is_none() && i > at {
                split = Some(i);
            } else if matches!(kind, WHERE_KW | L_CURLY | SEMICOLON) {
                end = i;
                break;
            }
        }
        match step(&toks, i) {
            Step::Open => depth += 1,
            Step::Close => depth = depth.checked_sub(1).ok_or(HeaderError::Unclosed)?,
            Step::Other | Step::Arrow => {}
        }
    }
    let span = |from: usize, to: usize| -> Option<&'t str> {
        (from < to).then(|| text.get(toks[from].start..toks[to - 1].end).map(str::trim)).flatten()
    };
    let (trait_, self_ty) = match split {
        Some(split) => (span(at, split), span(split + 1, end)),
        None => (None, span(at, end)),
    };
    let self_ty = self_ty.ok_or(HeaderError::NoSelfType)?;
    Ok(ImplHeader { flags, params, trait_, self_ty })
}

enum Step {
    Open,
    Close,
    /// The `>` of a `->`: no bracket.
    Arrow,
    Other,
}

/// What token `i` does to the bracket depth: `<` `(` `[` `{` open, their pairs close, and a
/// `>` right after a `-` is an arrow.
fn step(toks: &[Tok], i: usize) -> Step {
    match toks[i].kind {
        L_ANGLE | L_PAREN | L_BRACK | L_CURLY => Step::Open,
        R_ANGLE if i > 0 && toks[i - 1].kind == MINUS && toks[i - 1].end == toks[i].start => {
            Step::Arrow
        }
        R_ANGLE | R_PAREN | R_BRACK | R_CURLY => Step::Close,
        _ => Step::Other,
    }
}

/// The token closing the bracket opened at `open`.
fn closing(toks: &[Tok], open: usize) -> Result<usize, HeaderError> {
    let mut depth = 0usize;
    for i in open..toks.len() {
        match step(toks, i) {
            Step::Open => depth += 1,
            Step::Close => {
                depth = depth.checked_sub(1).ok_or(HeaderError::Unclosed)?;
                if depth == 0 {
                    return Ok(i);
                }
            }
            Step::Arrow | Step::Other => {}
        }
    }
    Err(HeaderError::Unclosed)
}
