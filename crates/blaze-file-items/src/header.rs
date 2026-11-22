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
    self, COMMA, CONST_KW, ENUM_KW, FOR_KW, IDENT, IMPL_KW, L_ANGLE, L_BRACK, L_CURLY, L_PAREN,
    LIFETIME_IDENT, MINUS, POUND, R_ANGLE, R_BRACK, R_CURLY, R_PAREN, SEMICOLON, STRUCT_KW, TRAIT_KW,
    TYPE_KW, UNION_KW, UNSAFE_KW, WHERE_KW,
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
        // Each parameter's start, its own attributes stepped over ([`Params`]).
        let mut walk = Params::new();
        for i in at + 1..close {
            let tok = toks[i];
            if walk.next(tok.kind, step(&toks, i)) == ParamTok::Start {
                let name = match tok.kind {
                    LIFETIME_IDENT => None,
                    CONST_KW => toks.get(i + 1).filter(|it| it.kind == IDENT).map(word),
                    IDENT => Some(word(&tok)),
                    _ => None,
                };
                params.extend(name);
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

/// Why an item's declared lifetimes were not counted ([`declared_lifetimes`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GenericsError {
    /// The lexer refused the text.
    Lex,
    /// A `<`, `(`, `[` or `{` the text does not close.
    Unclosed,
    /// No `struct`, `enum`, `union`, `trait` or `type` (and a name after it) at the item's
    /// own depth: not an item of a kind that declares generics here.
    NoKeyword,
    /// More lifetime parameters than a `u16` counts.
    TooMany,
}

impl core::fmt::Display for GenericsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            GenericsError::Lex => "the item's text does not lex",
            GenericsError::Unclosed => "the item's text leaves a bracket open",
            GenericsError::NoKeyword => "no `struct`, `enum`, `union`, `trait` or `type` and a name in the item's text",
            GenericsError::TooMany => "an item declaring more lifetime parameters than a u16 counts",
        })
    }
}

impl core::error::Error for GenericsError {}

/// How many lifetime parameters the generics of the struct, enum, union, trait, trait alias,
/// type alias or associated type whose whole text (attributes included: an outline's
/// `Item::range`, a member's text, an expansion's tokens alike) is `text` declare (D155): the
/// `'a` of `<'a, 'b: 'a, T, const N: usize>`, two. Read by tokens, as [`impl_header`] reads an
/// impl's: outer attributes skipped, the first item keyword at depth 0 (a visibility's
/// `pub(crate)` is a bracket and skipped), its name, then each parameter of the `<..>` right
/// after the name that starts with a lifetime. A lifetime in a default (`T = Foo<'static>`),
/// a bound (`T: 'a`), a where clause or a `for<'x>` binder is no parameter: it is not at the
/// start of one. A parameter's own outer attributes are before its start
/// (`#[rustc_assert_variance(covariant)] 'a`: std's `MutexGuard`; `#[may_dangle] 'a`), so the
/// walk is still at a parameter's start after each (the one walk of a generics list,
/// [`Params`], as [`impl_header`] reads its names). No `<` after the name: zero. One pass over
/// the tokens as the lexer gives them, nothing kept (rule 11): the walk stops at the generics'
/// closing `>`.
pub fn declared_lifetimes(text: &str, edition: Edition) -> Result<u16, GenericsError> {
    /// Where the walk is.
    #[derive(Clone, Copy)]
    enum At {
        /// Before the item keyword, `depth` brackets in (an attribute's, a visibility's).
        Seek { depth: usize },
        /// After the keyword: its name next. `union`: the keyword was the contextual
        /// `union`, which an identifier after it confirms (else it was a name, and the walk
        /// goes on seeking).
        Name { union: bool },
        /// After the name: its `<`, or no generics.
        Open,
        /// In the generics: the list's own walk is `params` ([`Params`]).
        In,
    }
    let lexed = parser::LexedStr::new(parser_edition(edition), text);
    if lexed.errors().next().is_some() {
        return Err(GenericsError::Lex);
    }
    let mut state = At::Seek { depth: 0 };
    let mut params = Params::new();
    let mut count = 0u16;
    // The token before, for a `->`'s `>` (no bracket: [`step`]'s rule).
    let mut prev: Option<(SyntaxKind, usize)> = None;
    let mut offset = 0;
    for i in 0..lexed.len() {
        let (start, kind) = (offset, lexed.kind(i));
        offset += lexed.text_len(i);
        if kind.is_trivia() {
            continue;
        }
        let arrow = kind == R_ANGLE && prev == Some((MINUS, start));
        prev = Some((kind, offset));
        let opens = matches!(kind, L_ANGLE | L_PAREN | L_BRACK | L_CURLY);
        let closes = !arrow && matches!(kind, R_ANGLE | R_PAREN | R_BRACK | R_CURLY);
        let word = text.get(start..offset).unwrap_or("");
        state = match state {
            At::Name { .. } if kind == IDENT => At::Open,
            At::Name { union: false } => return Err(GenericsError::NoKeyword),
            // `union` was a name: this token is seeked over like any other.
            At::Name { union: true } | At::Seek { .. } => {
                let depth = match state {
                    At::Seek { depth } => depth,
                    _ => 0,
                };
                if depth == 0 && matches!(kind, STRUCT_KW | ENUM_KW | TRAIT_KW | TYPE_KW | UNION_KW) {
                    At::Name { union: false }
                } else if depth == 0 && kind == IDENT && word == "union" {
                    At::Name { union: true }
                } else if opens {
                    At::Seek { depth: depth + 1 }
                } else if closes {
                    At::Seek { depth: depth.checked_sub(1).ok_or(GenericsError::Unclosed)? }
                } else {
                    At::Seek { depth }
                }
            }
            At::Open if kind == L_ANGLE => At::In,
            At::Open => return Ok(0),
            At::In => {
                let step = if opens {
                    Step::Open
                } else if closes {
                    Step::Close
                } else if arrow {
                    Step::Arrow
                } else {
                    Step::Other
                };
                match params.next(kind, step) {
                    ParamTok::Start if kind == LIFETIME_IDENT => count = count.checked_add(1).ok_or(GenericsError::TooMany)?,
                    ParamTok::End => return Ok(count),
                    ParamTok::Start | ParamTok::Inside => {}
                }
                At::In
            }
        };
    }
    match state {
        At::Open => Ok(0),
        At::In => Err(GenericsError::Unclosed),
        At::Seek { .. } | At::Name { .. } => Err(GenericsError::NoKeyword),
    }
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

/// What one token of a generics list is to its parameters ([`Params::next`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ParamTok {
    /// The first token of a parameter, after its own outer attributes: a lifetime, a type
    /// parameter's name, a const parameter's `const`.
    Start,
    /// Any other token inside the list: a parameter's own attribute, its bounds, its default,
    /// a comma.
    Inside,
    /// The list's closing `>`.
    End,
}

/// THE ONE WALK of a generics list's parameters (rule 30), used by every reader of one in this
/// file ([`impl_header`]'s names, [`declared_lifetimes`]'s count): which token starts a
/// parameter. A parameter starts after the list's `<` or a comma at the list's own depth, and
/// its own outer attributes are before its start, stepped over (std's
/// `unsafe impl<#[may_dangle] T, A: Allocator> Drop for Vec<T, A>`, `MutexGuard<
/// #[rustc_assert_variance(covariant)] 'a, ..>`; several, and brackets inside one, alike).
/// Fed every non-trivia token after the `<`, with its [`Step`]; nothing kept but the depths.
struct Params {
    /// Brackets open inside the list.
    depth: usize,
    /// The next token starts a parameter (or its attributes).
    first: bool,
    /// Inside a parameter's own attribute: its `[..]` is open (`depth` counts its brackets).
    attr: bool,
}

impl Params {
    /// The walk right after the list's `<`.
    fn new() -> Params {
        Params { depth: 0, first: true, attr: false }
    }

    /// What the token of `kind`, doing `step` to the bracket depth, is to the parameters.
    fn next(&mut self, kind: SyntaxKind, step: Step) -> ParamTok {
        if self.attr {
            match step {
                Step::Open => self.depth += 1,
                Step::Close => {
                    self.depth = self.depth.saturating_sub(1);
                    if self.depth == 0 {
                        // The attribute's `]`: the parameter still starts next.
                        self.attr = false;
                    }
                }
                Step::Arrow | Step::Other => {}
            }
            return ParamTok::Inside;
        }
        if self.depth == 0 && matches!(step, Step::Close) {
            return ParamTok::End;
        }
        if self.first && self.depth == 0 && kind == POUND {
            return ParamTok::Inside;
        }
        if self.first && self.depth == 0 && kind == L_BRACK {
            self.depth = 1;
            self.attr = true;
            return ParamTok::Inside;
        }
        let start = self.first && kind != COMMA;
        self.first = false;
        match step {
            Step::Open => self.depth += 1,
            Step::Close => self.depth -= 1,
            Step::Other if kind == COMMA && self.depth == 0 => self.first = true,
            Step::Other | Step::Arrow => {}
        }
        if start { ParamTok::Start } else { ParamTok::Inside }
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
