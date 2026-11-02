//! The arguments of a builtin macro as `rustc_builtin_macros/src/util.rs` reads them, a list of
//! expressions each parsed as rustc's `parse_expr` parses one, and their literals as
//! `rustc_ast`'s `LitKind::from_token_lit` reads them, with the errors of both in rustc's words
//! (`create_lit_error` in `rustc_session/src/diagnostics.rs`).

use std::borrow::Cow;

use base_db::Db;
use intern::{Symbol, sym};
use itertools::Itertools;
use mbe::expect_fragment;
use smallvec::SmallVec;
use span::Span;
use syntax::unescape::{unescape_byte, unescape_byte_str, unescape_char, unescape_str};

use crate::{
    BuiltinError, ExpandError, ExpandResult,
    tt::{self, DelimSpan, TtElement, TtIter},
};

/// One argument: an expression.
#[derive(Clone, Copy)]
pub(super) struct Arg<'t> {
    /// Its tokens, inside the invisible groups a fragment that a `macro_rules!` macro passes on
    /// is in, and inside the parentheses the transcriber puts in one of several tokens: the
    /// expression the fragment is, as rustc's parser takes an interpolated one.
    pub(super) tokens: tt::TokenTreesView<'t>,
    /// From its first token to its last.
    pub(super) span: Span,
}

impl<'t> Arg<'t> {
    fn new(tokens: tt::TokenTreesView<'t>, at: Span) -> Self {
        let tokens = unwrap_fragment(tokens);
        let span = match (tokens.first_span(), tokens.last_span()) {
            (Some(first), Some(last)) => first.cover(last),
            _ => at,
        };
        Arg { tokens, span }
    }

    /// The literal the argument is.
    pub(super) fn literal(&self) -> Option<tt::Literal> {
        match self.tokens.iter().collect_array() {
            Some([TtElement::Leaf(tt::Leaf::Literal(lit))]) => Some(lit),
            _ => None,
        }
    }

    /// `true` or `false`: a literal to rustc, an identifier in a token tree.
    pub(super) fn bool(&self) -> Option<bool> {
        match self.tokens.iter().collect_array() {
            Some([TtElement::Leaf(tt::Leaf::Ident(ident))]) if ident.is_raw.no() => {
                if ident.sym == sym::true_ {
                    Some(true)
                } else if ident.sym == sym::false_ {
                    Some(false)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// The literal of `-` and a literal, rustc's `ExprKind::Unary(UnOp::Neg, ..)` of one.
    pub(super) fn negated_literal(&self) -> Option<tt::Literal> {
        match self.tokens.iter().collect_array() {
            Some([
                TtElement::Leaf(tt::Leaf::Punct(tt::Punct { char: '-', .. })),
                TtElement::Leaf(tt::Leaf::Literal(lit)),
            ]) => Some(lit),
            _ => None,
        }
    }

    /// The trees inside the brackets, when the argument is an array or a repeat expression.
    fn bracketed(&self) -> Option<(tt::Subtree, TtIter<'t>)> {
        match self.tokens.iter().collect_array() {
            Some([TtElement::Subtree(subtree, inner)])
                if subtree.delimiter.kind == tt::DelimiterKind::Bracket =>
            {
                Some((subtree, inner))
            }
            _ => None,
        }
    }
}

/// `tokens` without the invisible groups around them, and the parentheses inside one that the
/// transcriber gave the same span as the group.
fn unwrap_fragment(mut tokens: tt::TokenTreesView<'_>) -> tt::TokenTreesView<'_> {
    loop {
        let Some(group) = tokens.try_into_subtree() else { return tokens };
        let delimiter = group.top_subtree().delimiter;
        if delimiter.kind != tt::DelimiterKind::Invisible {
            return tokens;
        }
        let inner = group.token_trees();
        tokens = match inner.try_into_subtree() {
            Some(parens)
                if parens.top_subtree().delimiter.kind == tt::DelimiterKind::Parenthesis
                    && parens.top_subtree().delimiter.open == delimiter.open =>
            {
                parens.token_trees()
            }
            _ => inner,
        };
    }
}

fn is_comma(element: &TtElement<'_>) -> bool {
    matches!(element, TtElement::Leaf(tt::Leaf::Punct(tt::Punct { char: ',', .. })))
}

/// The expression `iter` starts with, as rustc's `parse_expr` takes it.
fn expr<'t>(
    db: &Db,
    iter: &mut TtIter<'t>,
    delim_span: DelimSpan,
) -> Result<Arg<'t>, ExpandError> {
    let at = iter.next_span().unwrap_or(delim_span.close);
    let mbe::ValueResult { value, err } =
        expect_fragment(db, iter, parser::PrefixEntryPoint::Expr, delim_span);
    if let Some(err) = err {
        return Err(err.into());
    }
    if value.is_empty() {
        // The parser takes at least one token of an expression or says why it can't.
        return Err(ExpandError::other(at, "expected Expr"));
    }
    Ok(Arg::new(value, at))
}

/// rustc's `get_exprs_from_tts`: the expressions `tt` has, each followed by a `,` or the end.
pub(super) fn exprs<'t>(
    db: &Db,
    tt: &'t tt::TopSubtree,
) -> Result<SmallVec<[Arg<'t>; 4]>, ExpandError> {
    let delim_span = tt.top_subtree().delimiter.delim_span();
    let mut iter = tt.iter();
    let mut exprs = SmallVec::new();
    while !iter.is_empty() {
        exprs.push(expr(db, &mut iter, delim_span)?);
        match iter.peek() {
            None => break,
            Some(it) if is_comma(&it) => {
                iter.next();
            }
            Some(it) => return Err(BuiltinError::new("expected token: `,`").at(it.first_span())),
        }
    }
    Ok(exprs)
}

/// `tt`'s arguments, each its trees up to the next top-level `,`: what [`exprs`] gives for
/// arguments that are one literal or `true`/`false`, with no parse and so no database.
#[cfg(test)]
pub(super) fn comma_separated(tt: &tt::TopSubtree) -> SmallVec<[Arg<'_>; 4]> {
    let close = tt.top_subtree().delimiter.close;
    let mut iter = tt.iter();
    let mut args = SmallVec::new();
    while !iter.is_empty() {
        let at = iter.next_span().unwrap_or(close);
        let start = iter.savepoint();
        while iter.peek().is_some_and(|it| !is_comma(&it)) {
            iter.next();
        }
        args.push(Arg::new(iter.from_savepoint(start), at));
        iter.next();
    }
    args
}

/// rustc's `get_single_expr_from_tts`: the one expression `tt` has, and a `,` after it if there
/// is one. No expression is `{name} takes 1 argument` at the call, with none read; more is the
/// same error, with the first read all the same.
pub(super) fn single_expr<'t>(
    db: &Db,
    tt: &'t tt::TopSubtree,
    name: &str,
    call: Span,
) -> ExpandResult<Option<Arg<'t>>> {
    // alloc: an error's message, on the error path
    let takes_one = || BuiltinError::new(format!("{name} takes 1 argument")).at(call);
    let mut iter = tt.iter();
    if iter.is_empty() {
        return ExpandResult::new(None, takes_one());
    }
    let arg = match expr(db, &mut iter, tt.top_subtree().delimiter.delim_span()) {
        Ok(it) => it,
        Err(err) => return ExpandResult::new(None, err),
    };
    if iter.peek().is_some_and(|it| is_comma(&it)) {
        iter.next();
    }
    if iter.is_empty() {
        ExpandResult::ok(Some(arg))
    } else {
        ExpandResult::new(Some(arg), takes_one())
    }
}

/// rustc's `expr_to_spanned_string`: the text of `arg`, a string literal, else `err_msg` at it.
pub(super) fn string(arg: &Arg<'_>, err_msg: &'static str) -> Result<Symbol, ExpandError> {
    let not_a_string = || BuiltinError::new(err_msg).at(arg.span);
    let lit = arg.literal().ok_or_else(not_a_string)?;
    match lit_value(&lit) {
        Ok(Lit::Str(text)) => Ok(Symbol::intern(&text)),
        Ok(_) => Err(not_a_string()),
        Err(err) => Err(err.at(lit.span)),
    }
}

/// rustc's `get_single_str_spanned_from_tts`: the text and span of the string literal that is
/// the one argument of `tt`. The first error rustc gives is `err`.
pub(super) fn single_string(
    db: &Db,
    tt: &tt::TopSubtree,
    name: &str,
    call: Span,
) -> ExpandResult<Option<(Symbol, Span)>> {
    let ExpandResult { value, err } = single_expr(db, tt, name, call);
    let Some(arg) = value else { return ExpandResult { value: None, err } };
    match string(&arg, "argument must be a string literal") {
        Ok(text) => ExpandResult { value: Some((text, arg.span)), err },
        Err(string_err) => ExpandResult { value: None, err: err.or(Some(string_err)) },
    }
}

/// An array argument, as rustc's `parse_expr_array_or_repeat` reads it.
pub(super) enum Array<'t> {
    /// `[a, b, ..]`.
    List(SmallVec<[Arg<'t>; 8]>),
    /// `[element; count]`.
    Repeat(Arg<'t>, Arg<'t>),
}

/// The array `arg` is, if it is one.
pub(super) fn array<'t>(
    db: &Db,
    arg: &Arg<'t>,
) -> Option<Result<Array<'t>, ExpandError>> {
    let (subtree, mut iter) = arg.bracketed()?;
    let delim_span = subtree.delimiter.delim_span();
    let mut elements = SmallVec::new();
    let read = (|| {
        while !iter.is_empty() {
            let element = expr(db, &mut iter, delim_span)?;
            match iter.next() {
                None => {
                    elements.push(element);
                    break;
                }
                Some(it) if is_comma(&it) => elements.push(element),
                Some(TtElement::Leaf(tt::Leaf::Punct(tt::Punct { char: ';', .. })))
                    if elements.is_empty() =>
                {
                    let count = expr(db, &mut iter, delim_span)?;
                    if let Some(it) = iter.next() {
                        return Err(ExpandError::other(it.first_span(), "unexpected token"));
                    }
                    return Ok(Array::Repeat(element, count));
                }
                Some(it) => return Err(ExpandError::other(it.first_span(), "unexpected token")),
            }
        }
        Ok(Array::List(std::mem::take(&mut elements)))
    })();
    Some(read)
}

/// A literal's value, rustc's `LitKind`.
pub(super) enum Lit<'a> {
    Str(Cow<'a, str>),
    Char(char),
    Int(u128, IntSuffix),
    /// The text without its underscores.
    Float(Cow<'a, str>),
    Byte(u8),
    ByteStr(Cow<'a, [u8]>),
    CStr,
    /// A literal the lexer refused and reported.
    Err,
}

/// What an integer literal's suffix makes it, as far as `concat_bytes!` asks.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum IntSuffix {
    None,
    U8,
    Other,
}

/// rustc's `token::LitKind::descr`.
fn descr(kind: tt::LitKind) -> &'static str {
    match kind {
        tt::LitKind::Byte => "byte",
        tt::LitKind::Char => "char",
        tt::LitKind::Integer => "integer",
        tt::LitKind::Float => "float",
        tt::LitKind::Str | tt::LitKind::StrRaw(_) => "string",
        tt::LitKind::ByteStr | tt::LitKind::ByteStrRaw(_) => "byte string",
        tt::LitKind::CStr | tt::LitKind::CStrRaw(_) => "C string",
        tt::LitKind::Err(_) => "error",
    }
}

/// rustc's `LitKind::from_token_lit` of `lit`, its error as `create_lit_error` words it.
pub(super) fn lit_value(lit: &tt::Literal) -> Result<Lit<'_>, BuiltinError> {
    let (text, suffix) = lit.text_and_suffix();
    let suffix = (!suffix.is_empty()).then_some(suffix);
    if suffix.is_some()
        && !matches!(lit.kind, tt::LitKind::Integer | tt::LitKind::Float | tt::LitKind::Err(_))
    {
        // alloc: an error's message, on the error path
        return Err(BuiltinError::new(format!(
            "suffixes on {} literals are invalid",
            descr(lit.kind)
        )));
    }
    Ok(match lit.kind {
        tt::LitKind::Byte => unescape_byte(text).map_or(Lit::Err, Lit::Byte),
        tt::LitKind::Char => unescape_char(text).map_or(Lit::Err, Lit::Char),
        tt::LitKind::Integer => return integer(text, suffix),
        tt::LitKind::Float => return float(strip_underscores(text), suffix, 10),
        tt::LitKind::Str if text.contains('\\') => {
            // alloc: the text of an escaped string literal, once per argument
            let mut out = String::with_capacity(text.len());
            unescape_str(text, |_, c| {
                if let Ok(c) = c {
                    out.push(c)
                }
            });
            Lit::Str(Cow::Owned(out))
        }
        tt::LitKind::Str | tt::LitKind::StrRaw(_) => Lit::Str(Cow::Borrowed(text)),
        tt::LitKind::ByteStr => {
            // alloc: the bytes of a byte string literal, once per argument
            let mut out = Vec::with_capacity(text.len());
            unescape_byte_str(text, |_, b| {
                if let Ok(b) = b {
                    out.push(b)
                }
            });
            Lit::ByteStr(Cow::Owned(out))
        }
        tt::LitKind::ByteStrRaw(_) => Lit::ByteStr(Cow::Borrowed(text.as_bytes())),
        tt::LitKind::CStr | tt::LitKind::CStrRaw(_) => Lit::CStr,
        tt::LitKind::Err(_) => Lit::Err,
    })
}

fn strip_underscores(text: &str) -> Cow<'_, str> {
    if text.contains('_') {
        // alloc: a literal written with underscores, once per argument
        Cow::Owned(text.chars().filter(|&c| c != '_').collect())
    } else {
        Cow::Borrowed(text)
    }
}

/// rustc's `integer_lit`.
fn integer<'a>(text: &'a str, suffix: Option<&str>) -> Result<Lit<'a>, BuiltinError> {
    let digits = strip_underscores(text);
    let base = match digits.as_bytes() {
        [b'0', b'x', ..] => 16,
        [b'0', b'o', ..] => 8,
        [b'0', b'b', ..] => 2,
        _ => 10,
    };
    let suffix = match suffix {
        None => IntSuffix::None,
        Some("u8") => IntSuffix::U8,
        Some(
            "isize" | "i8" | "i16" | "i32" | "i64" | "i128" | "usize" | "u16" | "u32" | "u64"
            | "u128",
        ) => IntSuffix::Other,
        Some(suffix) if suffix.starts_with('f') => return float(digits, Some(suffix), base),
        Some(suffix) => return Err(invalid_int_suffix(text, suffix)),
    };
    let unprefixed = &digits[if base != 10 { 2 } else { 0 }..];
    match u128::from_str_radix(unprefixed, base) {
        Ok(value) => Ok(Lit::Int(value, suffix)),
        Err(_) => {
            let limit = match base {
                // alloc: an error's message, on the error path
                2 => format!("{:#b}", u128::MAX),
                // alloc: an error's message, on the error path
                8 => format!("{:#o}", u128::MAX),
                // alloc: an error's message, on the error path
                16 => format!("{:#x}", u128::MAX),
                // alloc: an error's message, on the error path
                _ => format!("{}", u128::MAX),
            };
            Err(BuiltinError::new("integer literal is too large")
                // alloc: an error's message, on the error path
                .note(format!("value exceeds limit of `{limit}`")))
        }
    }
}

/// rustc's `filtered_float_lit`.
fn float<'a>(
    digits: Cow<'a, str>,
    suffix: Option<&str>,
    base: u32,
) -> Result<Lit<'a>, BuiltinError> {
    match base {
        16 => return Err(BuiltinError::new("hexadecimal float literal is not supported")),
        8 => return Err(BuiltinError::new("octal float literal is not supported")),
        2 => return Err(BuiltinError::new("binary float literal is not supported")),
        _ => {}
    }
    match suffix {
        None | Some("f16" | "f32" | "f64" | "f128") => Ok(Lit::Float(digits)),
        Some(suffix) if looks_like_width_suffix(&['f'], suffix) => {
            // alloc: an error's message, on the error path
            Err(BuiltinError::new(format!("invalid width `{}` for float literal", &suffix[1..]))
                .help("valid widths are 32 and 64"))
        }
        // alloc: an error's message, on the error path
        Some(suffix) => Err(BuiltinError::new(format!(
            "invalid suffix `{suffix}` for float literal"
        ))
        .help("valid suffixes are `f32` and `f64`")),
    }
}

/// rustc's `LitError::InvalidIntSuffix` arm of `create_lit_error`.
fn invalid_int_suffix(text: &str, suffix: &str) -> BuiltinError {
    if looks_like_width_suffix(&['i', 'u'], suffix) {
        // alloc: an error's message, on the error path
        BuiltinError::new(format!("invalid width `{}` for integer literal", &suffix[1..]))
            .help("valid widths are 8, 16, 32, 64 and 128")
    } else if fixes_base_capitalisation(text, suffix) {
        BuiltinError::new("invalid base prefix for number literal")
            .note("base prefixes (`0xff`, `0b1010`, `0o755`) are lowercase")
    } else {
        // alloc: an error's message, on the error path
        BuiltinError::new(format!("invalid suffix `{suffix}` for number literal"))
            .help("the suffix must be one of the numeric types (`u32`, `isize`, `f32`, etc.)")
    }
}

fn looks_like_width_suffix(first_chars: &[char], s: &str) -> bool {
    s.len() > 1 && s.starts_with(first_chars) && s[1..].chars().all(|c| c.is_ascii_digit())
}

/// Whether rustc's `fix_base_capitalisation` has a fix: `0X1F`, lexed as `0` with the suffix
/// `X1F`.
fn fixes_base_capitalisation(prefix: &str, suffix: &str) -> bool {
    let mut chars = suffix.chars();
    let base = match chars.next() {
        Some('B') => 2,
        Some('O') => 8,
        Some('X') => 16,
        _ => return false,
    };
    prefix == "0"
        && chars
            .filter(|&c| c != '_')
            .take_while(|&c| c != 'i' && c != 'u')
            .all(|c| c.to_digit(base).is_some())
}
