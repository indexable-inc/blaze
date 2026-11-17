//! An attribute's value written as a macro call, read to the literal it expands to.
//!
//! The Reference, "Attributes", "Meta Item Attribute Syntax" (the pinned toolchain's
//! share/doc/rust/html/reference/attributes.html): "Expressions in meta items must macro-expand
//! to literal expressions, which must not include integer or float type suffixes. Expressions
//! which are not literal expressions will be syntactically accepted (and can be passed to
//! proc-macros), but will be rejected after parsing." So `#[lang = stringify!(x)]` names the
//! lang item `x`: the call is expanded first and the attribute is read from the literal it
//! gives (library/core/src/panicking.rs declares every `panic_const_*` lang item so).
//!
//! Only the built-ins whose literal comes from their tokens alone are evaluated here:
//! `stringify!` and `concat!` (of literals, `true`/`false`, and nested `stringify!`/`concat!`).
//! Each is the expander's own code: `stringify!`'s text is [`crate::builtin::stringified`] and
//! `concat!`'s pieces [`crate::literal::concat_piece`], the two `builtin.rs` runs for a call,
//! so an attribute reads the same literal a call expands to. A macro that needs a file
//! (`include_str!`), the environment (`env!`) or name resolution (any other macro) is refused by
//! name ([`AttrLiteralError::NeedsContext`]): the attribute reader runs before names resolve.
//!
//! The reader takes the macro BY ITS WRITTEN NAME (bare, or under `core::`/`std::`): a crate
//! that shadows `stringify` or `concat` with its own `macro_rules!` and uses it in an
//! attribute's value is not told apart here (the reader runs before names resolve).
//!
//! No recursion: nested calls are a work list, each entry with its depth, bounded by
//! [`MOST_ATTR_VALUE_DEPTH`] and refused past it.

use blaze_own::{AVec, Alloc, Vec};

use crate::builtin::stringified;
use crate::literal::{Buf, concat_piece, string_value};
use crate::tokens::{Edition, LitKind, TokenKind, TokenStream, lex};

/// The most nested `concat!`/`stringify!` calls one attribute value is read through.
pub const MOST_ATTR_VALUE_DEPTH: u32 = 64;

/// Why an attribute's value did not read to a string literal.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AttrLiteralError<'a> {
    /// The value's text did not lex in the crate's edition.
    Lex,
    /// A call of `mac!`, which needs a file, the environment or name resolution to give its
    /// literal (`include_str!`, `env!`, any macro not built in): not evaluated where
    /// attributes are read.
    NeedsContext { mac: &'a str },
    /// A token where a literal, `true`/`false`, `stringify!(..)` or `concat!(..)` is wanted
    /// (the `index`th token of the value).
    NotALiteral { index: u32 },
    /// A literal the value cannot take: `message` is the expander's own (a byte string in a
    /// `concat!`, an integer where the whole value must be a string).
    Literal { message: &'static str },
    /// More than [`MOST_ATTR_VALUE_DEPTH`] nested calls.
    TooDeep,
    /// A call's `!` without a delimited group after it, or two commas with nothing between.
    Malformed { index: u32 },
}

impl core::fmt::Display for AttrLiteralError<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            AttrLiteralError::Lex => f.write_str("the value does not lex"),
            AttrLiteralError::NeedsContext { mac } => write!(
                f,
                "`{mac}!` needs a file, the environment or name resolution to give its literal; only `stringify!` and `concat!` are read where attributes are read"
            ),
            AttrLiteralError::NotALiteral { index } => {
                write!(f, "token {index} of the value is not a literal, `stringify!(..)` or `concat!(..)`")
            }
            AttrLiteralError::Literal { message } => f.write_str(message),
            AttrLiteralError::TooDeep => write!(f, "more than {MOST_ATTR_VALUE_DEPTH} nested macro calls"),
            AttrLiteralError::Malformed { index } => write!(f, "a macro call or argument list is malformed at token {index}"),
        }
    }
}

/// One piece of the value left to read: the tokens `start..end` of the lexed value, at
/// `depth` nested calls; `top` for the whole value (a literal there must be a string).
#[derive(Clone, Copy)]
struct Piece {
    start: usize,
    end: usize,
    depth: u32,
    top: bool,
}

/// The string `expr` (an attribute's value as written: `stringify!(x)`,
/// `concat!("a", stringify!(b))`, `"plain"`) expands to, lexed in `edition`, in `alloc`.
pub fn attr_literal<'a>(expr: &str, edition: Edition, alloc: Alloc<'a>) -> Result<&'a str, AttrLiteralError<'a>> {
    let stream = lex(expr, edition, alloc).map_err(|_| AttrLiteralError::Lex)?;
    let mut value = Buf::new_in(alloc);
    let mut work: AVec<'_, Piece> = Vec::new_in(alloc);
    work.push(Piece { start: 0, end: stream.len(), depth: 0, top: true });
    while let Some(piece) = work.pop() {
        if piece.depth > MOST_ATTR_VALUE_DEPTH {
            return Err(AttrLiteralError::TooDeep);
        }
        read_piece(&stream, piece, &mut value, &mut work, alloc)?;
    }
    let bytes: &'a [u8] = value.0.leak();
    Ok(core::str::from_utf8(bytes).expect("a `Buf` is pushed only whole `str`s and `char`s"))
}

/// Reads one piece: a literal or `true`/`false` is appended to `value`; a `stringify!` call's
/// text is appended; a `concat!` call's arguments are pushed on `work`, last first, so they are
/// read in the order written.
fn read_piece<'a>(
    stream: &TokenStream<'_>,
    piece: Piece,
    value: &mut Buf<'_>,
    work: &mut AVec<'_, Piece>,
    alloc: Alloc<'a>,
) -> Result<(), AttrLiteralError<'a>> {
    let tokens = stream.tokens();
    let Piece { start, end, depth, top } = piece;
    let at = |i: usize| u32::try_from(i).unwrap_or(u32::MAX);
    if start >= end {
        return Err(AttrLiteralError::Malformed { index: at(start) });
    }
    // A literal, alone or after a `-` (`concat!(-1)`).
    let (negative, lit_at) = match tokens[start].kind {
        TokenKind::Punct { char: '-', .. } if end - start == 2 => (true, start + 1),
        _ => (false, start),
    };
    if end - lit_at == 1 {
        let token = tokens[lit_at];
        match token.kind {
            TokenKind::Literal { kind, suffix_len, .. } => {
                let text = stream.text_of(&token);
                let read = if top {
                    if negative {
                        return Err(AttrLiteralError::NotALiteral { index: at(start) });
                    }
                    string_value(kind, text, suffix_len, value)
                } else {
                    if negative && !matches!(kind, LitKind::Integer | LitKind::Float) {
                        return Err(AttrLiteralError::NotALiteral { index: at(start) });
                    }
                    concat_piece(kind, text, suffix_len, negative, value)
                };
                return read.map_err(|e| AttrLiteralError::Literal { message: e.message() });
            }
            TokenKind::Ident { .. } if !top && !negative => {
                match stream.text_of(&token) {
                    "true" => value.push_str("true"),
                    "false" => value.push_str("false"),
                    _ => return Err(AttrLiteralError::NotALiteral { index: at(lit_at) }),
                }
                return Ok(());
            }
            _ => return Err(AttrLiteralError::NotALiteral { index: at(lit_at) }),
        }
    }
    // A call: a path, `!`, then one delimited group that ends the piece.
    let (name, bang, built_in) = call_path(stream, start, end).ok_or(AttrLiteralError::NotALiteral { index: at(start) })?;
    let open = bang + 1;
    if open >= end || !matches!(tokens[open].kind, TokenKind::Open(_)) || stream.close_of(open) != Some(end - 1) {
        return Err(AttrLiteralError::Malformed { index: at(bang) });
    }
    let inner = open + 1..end - 1;
    match name {
        _ if !built_in => Err(AttrLiteralError::NeedsContext { mac: leak_name(name, alloc) }),
        "stringify" => {
            let input = stream.copy_in(inner, alloc).map_err(|_| AttrLiteralError::Malformed { index: at(open) })?;
            stringified(&input, alloc, value).map_err(|_| AttrLiteralError::Malformed { index: at(open) })
        }
        "concat" => {
            // The arguments, split at the commas outside groups; a last empty one (a trailing
            // comma) is no argument.
            let first = work.len();
            let mut arg = inner.start;
            let mut i = inner.start;
            while i < inner.end {
                match tokens[i].kind {
                    TokenKind::Open(_) => {
                        i = stream.close_of(i).ok_or(AttrLiteralError::Malformed { index: at(i) })? + 1;
                        continue;
                    }
                    TokenKind::Punct { char: ',', .. } => {
                        if arg == i {
                            return Err(AttrLiteralError::Malformed { index: at(i) });
                        }
                        work.push(Piece { start: arg, end: i, depth: depth + 1, top: false });
                        arg = i + 1;
                    }
                    _ => {}
                }
                i += 1;
            }
            if arg < inner.end {
                work.push(Piece { start: arg, end: inner.end, depth: depth + 1, top: false });
            }
            work[first..].reverse();
            Ok(())
        }
        other => Err(AttrLiteralError::NeedsContext { mac: leak_name(other, alloc) }),
    }
}

/// The last name of the call path at `start`, the index of its `!`, and whether the path can
/// name a built-in (bare, or `core::`/`std::` with an optional leading `::`); `None` when
/// `start..end` does not begin with a path and `!`.
fn call_path<'s>(stream: &'s TokenStream<'_>, start: usize, end: usize) -> Option<(&'s str, usize, bool)> {
    let tokens = stream.tokens();
    let colons = |i: usize| {
        i + 1 < end
            && matches!(tokens[i].kind, TokenKind::Punct { char: ':', .. })
            && matches!(tokens[i + 1].kind, TokenKind::Punct { char: ':', .. })
    };
    let mut i = start;
    let rooted = colons(i);
    if rooted {
        i += 2;
    }
    let mut segments = 0u32;
    let mut first = "";
    let mut last = "";
    loop {
        let token = tokens.get(i).filter(|_| i < end)?;
        if !matches!(token.kind, TokenKind::Ident { .. }) {
            return None;
        }
        last = stream.text_of(token);
        if segments == 0 {
            first = last;
        }
        segments += 1;
        i += 1;
        if colons(i) {
            i += 2;
            continue;
        }
        break;
    }
    if i >= end || !matches!(tokens[i].kind, TokenKind::Punct { char: '!', .. }) {
        return None;
    }
    // `core::stringify!` and `std::concat!` name the built-ins; any longer or other path
    // (`$crate::concat!`, `my::stringify!`) names a macro only resolution can tell.
    let built_in = (segments == 1 && !rooted) || (segments == 2 && matches!(first, "core" | "std"));
    Some((last, i, built_in))
}

/// `name` copied into `alloc` for as long as it lives (an error's macro name outlives the
/// lexed stream it was read from).
fn leak_name<'a>(name: &str, alloc: Alloc<'a>) -> &'a str {
    let mut bytes: AVec<'a, u8> = Vec::new_in(alloc);
    bytes.extend_from_slice(name.as_bytes());
    let bytes: &'a [u8] = bytes.leak();
    core::str::from_utf8(bytes).expect("copied from a `str`")
}
