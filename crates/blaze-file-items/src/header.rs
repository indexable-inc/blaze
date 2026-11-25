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
            match walk.next(tok.kind, tok_delim(&toks, i)) {
                ParamTok::Start => {
                    let name = match tok.kind {
                        LIFETIME_IDENT => None,
                        CONST_KW => toks.get(i + 1).filter(|it| it.kind == IDENT).map(word),
                        IDENT => Some(word(&tok)),
                        _ => None,
                    };
                    params.extend(name);
                }
                ParamTok::Unclosed => return Err(HeaderError::Unclosed),
                ParamTok::Inside | ParamTok::End => {}
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
    let mut depth = Brackets::default();
    let mut split = None;
    let mut end = toks.len();
    for i in at..toks.len() {
        let kind = toks[i].kind;
        if depth.is_top() {
            if kind == FOR_KW && split.is_none() && i > at {
                split = Some(i);
            } else if matches!(kind, WHERE_KW | L_CURLY | SEMICOLON) {
                end = i;
                break;
            }
        }
        if depth.step(tok_delim(&toks, i)).is_err() {
            return Err(HeaderError::Unclosed);
        }
    }
    if !depth.is_top() {
        return Err(HeaderError::Unclosed);
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
        /// Before the item keyword, `group` token-tree brackets in (an attribute's, a visibility's).
        /// Angle brackets `<` / `>` never count while seeking the item keyword.
        Seek { group: usize },
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
    let mut state = At::Seek { group: 0 };
    let mut params = Params::new();
    let mut count = 0u16;
    // The token before, for a `->`'s `>` (no bracket: [`delim`]'s rule).
    let mut prev: Option<(SyntaxKind, usize)> = None;
    let mut offset = 0;
    for i in 0..lexed.len() {
        let (start, kind) = (offset, lexed.kind(i));
        offset += lexed.text_len(i);
        if kind.is_trivia() {
            continue;
        }
        let d = delim(kind, prev == Some((MINUS, start)));
        prev = Some((kind, offset));
        let word = text.get(start..offset).unwrap_or("");
        state = match state {
            At::Name { .. } if kind == IDENT => At::Open,
            At::Name { union: false } => return Err(GenericsError::NoKeyword),
            // `union` was a name: this token is seeked over like any other.
            At::Name { union: true } | At::Seek { .. } => {
                let mut brackets = Brackets {
                    group: match state {
                        At::Seek { group } => group,
                        _ => 0,
                    },
                    angle: 0,
                };
                if brackets.group == 0 && matches!(kind, STRUCT_KW | ENUM_KW | TRAIT_KW | TYPE_KW | UNION_KW) {
                    At::Name { union: false }
                } else if brackets.group == 0 && kind == IDENT && word == "union" {
                    At::Name { union: true }
                } else {
                    brackets.step_group(d).map_err(|()| GenericsError::Unclosed)?;
                    At::Seek { group: brackets.group }
                }
            }
            At::Open if kind == L_ANGLE => At::In,
            At::Open => return Ok(0),
            At::In => {
                match params.next(kind, d) {
                    ParamTok::Start if kind == LIFETIME_IDENT => count = count.checked_add(1).ok_or(GenericsError::TooMany)?,
                    ParamTok::End => return Ok(count),
                    ParamTok::Unclosed => return Err(GenericsError::Unclosed),
                    ParamTok::Start | ParamTok::Inside => {}
                }
                At::In
            }
        };
    }
    match state {
        At::Open => Ok(0),
        At::In => Err(GenericsError::Unclosed),
        At::Seek { group } if group > 0 => Err(GenericsError::Unclosed),
        At::Seek { .. } | At::Name { .. } => Err(GenericsError::NoKeyword),
    }
}

/// Bitmask of an `impl` item's type/const generic parameter indices (`0..64`, lifetimes omitted,
/// matching `impl_header`'s `params` / `PNode::Param(p)` numbering) marked `#[may_dangle]`
/// (RFC 1327, e.g. `unsafe impl<#[may_dangle] T: ?Sized, A: Allocator> Drop for Arc<T, A>` ->
/// bit 0 set). One pass over the tokens, nothing kept (rule 11).
pub fn impl_may_dangle_params(text: &str, edition: Edition) -> Result<u64, GenericsError> {
    #[derive(Clone, Copy)]
    enum At {
        Seek { group: usize },
        Open,
        In,
    }
    let lexed = parser::LexedStr::new(parser_edition(edition), text);
    if lexed.errors().next().is_some() {
        return Err(GenericsError::Lex);
    }
    let mut state = At::Seek { group: 0 };
    let mut params = Params::new();
    let mut mask = 0u64;
    let mut param_idx = 0u32;
    let mut has_may_dangle = false;
    let mut prev: Option<(SyntaxKind, usize)> = None;
    let mut offset = 0;
    for i in 0..lexed.len() {
        let (start, kind) = (offset, lexed.kind(i));
        offset += lexed.text_len(i);
        if kind.is_trivia() {
            continue;
        }
        let d = delim(kind, prev == Some((MINUS, start)));
        prev = Some((kind, offset));
        let word = text.get(start..offset).unwrap_or("");
        state = match state {
            At::Seek { group } => {
                if group == 0 && kind == IMPL_KW {
                    At::Open
                } else {
                    let mut brackets = Brackets { group, angle: 0 };
                    brackets.step_group(d).map_err(|()| GenericsError::Unclosed)?;
                    At::Seek { group: brackets.group }
                }
            }
            At::Open => {
                if kind == L_ANGLE {
                    At::In
                } else {
                    return Ok(0);
                }
            }
            At::In => {
                if params.attr && params.depth.group == 1 && kind == IDENT && word == "may_dangle" {
                    has_may_dangle = true;
                }
                match params.next(kind, d) {
                    ParamTok::End => return Ok(mask),
                    ParamTok::Unclosed => return Err(GenericsError::Unclosed),
                    ParamTok::Start => {
                        match kind {
                            IDENT => {
                                if has_may_dangle && param_idx < 64 {
                                    mask |= 1u64 << param_idx;
                                }
                                param_idx = param_idx.saturating_add(1);
                            }
                            CONST_KW => {
                                param_idx = param_idx.saturating_add(1);
                            }
                            _ => {}
                        }
                        has_may_dangle = false;
                        At::In
                    }
                    ParamTok::Inside => At::In,
                }
            }
        };
    }
    match state {
        At::Seek { group } if group > 0 => Err(GenericsError::Unclosed),
        At::Seek { .. } => Err(GenericsError::NoKeyword),
        At::Open => Ok(0),
        At::In => Err(GenericsError::Unclosed),
    }
}

/// Balanced delimiter depth (rule 30: the one counter every reader in this file shares).
///
/// `group` counts Rust token-tree delimiters `( ) [ ] { }`, which are always balanced — inside
/// outer attributes (`#[debug("..", match x { A => 1 })]`), visibilities (`pub(...)`), const
/// generic expressions (`{ if a > b { 1 } else { 0 } }`), and parenthesized types alike.
/// `angle` counts `< >` strictly at `group == 0`: comparisons, shifts, fat arrows (`=>`), and
/// nested generics inside any `( ) [ ] { }` group never open or close an outer angle bracket.
#[derive(Clone, Copy, Default)]
struct Brackets {
    group: usize,
    angle: usize,
}

impl Brackets {
    fn is_top(&self) -> bool {
        self.group == 0 && self.angle == 0
    }

    /// Updates only token-tree group depth (`( ) [ ] { }`), ignoring `<`, `>`, and `->`.
    fn step_group(&mut self, d: Delim) -> Result<(), ()> {
        match d {
            Delim::OpenGroup => self.group += 1,
            Delim::CloseGroup => self.group = self.group.checked_sub(1).ok_or(())?,
            Delim::OpenAngle | Delim::CloseAngle | Delim::Arrow | Delim::Other => {}
        }
        Ok(())
    }

    /// Updates group depth everywhere and angle depth strictly when `group == 0`.
    fn step(&mut self, d: Delim) -> Result<(), ()> {
        match d {
            Delim::OpenGroup => self.group += 1,
            Delim::CloseGroup => self.group = self.group.checked_sub(1).ok_or(())?,
            Delim::OpenAngle if self.group == 0 => self.angle += 1,
            Delim::CloseAngle if self.group == 0 => self.angle = self.angle.checked_sub(1).ok_or(())?,
            Delim::OpenAngle | Delim::CloseAngle | Delim::Arrow | Delim::Other => {}
        }
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Delim {
    OpenGroup,
    CloseGroup,
    OpenAngle,
    CloseAngle,
    /// The `>` of a `->`: no bracket.
    Arrow,
    Other,
}

fn delim(kind: SyntaxKind, prev_minus_adjacent: bool) -> Delim {
    match kind {
        L_PAREN | L_BRACK | L_CURLY => Delim::OpenGroup,
        R_PAREN | R_BRACK | R_CURLY => Delim::CloseGroup,
        L_ANGLE => Delim::OpenAngle,
        R_ANGLE if prev_minus_adjacent => Delim::Arrow,
        R_ANGLE => Delim::CloseAngle,
        _ => Delim::Other,
    }
}

/// What token `i` does to the delimiter depth: `(` `[` `{` open groups, `<` opens an angle at
/// `group == 0`, their pairs close, and a `>` right after a `-` is an arrow.
fn tok_delim(toks: &[Tok], i: usize) -> Delim {
    let prev_minus_adjacent = i > 0 && toks[i - 1].kind == MINUS && toks[i - 1].end == toks[i].start;
    delim(toks[i].kind, prev_minus_adjacent)
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
    /// A closing group delimiter with no matching opener inside the list.
    Unclosed,
}

/// THE ONE WALK of a generics list's parameters (rule 30), used by every reader of one in this
/// file ([`impl_header`]'s names, [`declared_lifetimes`]'s count, [`impl_may_dangle_params`]):
/// which token starts a parameter. A parameter starts after the list's `<` or a comma at the
/// list's own depth, and its own outer attributes are before its start, stepped over (std's
/// `unsafe impl<#[may_dangle] T, A: Allocator> Drop for Vec<T, A>`, `MutexGuard<
/// #[rustc_assert_variance(covariant)] 'a, ..>`; several, and brackets or `=>` inside one,
/// alike). Fed every non-trivia token after the `<`, with its [`Delim`]; nothing kept but the
/// depths.
struct Params {
    /// Delimiters open inside the list (`group` for `() [] {}`, `angle` for `< >` at `group == 0`).
    depth: Brackets,
    /// The next token starts a parameter (or its attributes).
    first: bool,
    /// Saw `#` at a parameter's start; an immediate `[` enters an outer attribute.
    pound: bool,
    /// Inside a parameter's own attribute: its `[..]` is open (`depth.group` counts its brackets).
    attr: bool,
}

impl Params {
    /// The walk right after the list's `<`.
    fn new() -> Params {
        Params { depth: Brackets::default(), first: true, pound: false, attr: false }
    }

    /// What the token of `kind`, doing `d` to the delimiter depth, is to the parameters.
    fn next(&mut self, kind: SyntaxKind, d: Delim) -> ParamTok {
        if self.attr {
            if self.depth.step_group(d).is_err() {
                return ParamTok::Unclosed;
            }
            if self.depth.group == 0 {
                // The attribute's `]`: the parameter still starts next.
                self.attr = false;
            }
            return ParamTok::Inside;
        }
        if self.depth.is_top() && d == Delim::CloseAngle {
            return ParamTok::End;
        }
        if self.first && self.depth.is_top() && kind == POUND {
            self.pound = true;
            return ParamTok::Inside;
        }
        if self.first && self.pound && self.depth.is_top() && kind == L_BRACK {
            self.pound = false;
            self.depth.group = 1;
            self.attr = true;
            return ParamTok::Inside;
        }
        self.pound = false;
        let start = self.first && kind != COMMA;
        self.first = false;
        if self.depth.step(d).is_err() {
            return ParamTok::Unclosed;
        }
        if kind == COMMA && self.depth.is_top() {
            self.first = true;
        }
        if start { ParamTok::Start } else { ParamTok::Inside }
    }
}

/// The token closing the `<` opened at `open`.
fn closing(toks: &[Tok], open: usize) -> Result<usize, HeaderError> {
    let mut depth = Brackets::default();
    for i in open..toks.len() {
        depth.step(tok_delim(toks, i)).map_err(|()| HeaderError::Unclosed)?;
        if depth.is_top() {
            return Ok(i);
        }
    }
    Err(HeaderError::Unclosed)
}
