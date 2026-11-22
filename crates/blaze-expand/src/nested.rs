//! The macro calls in a stream, found without parsing it: `path!(..)` (and `[..]`, `{..}`),
//! `macro_rules! name {..}` and `macro name(..) {..}`. A call's arguments and a definition's
//! body are skipped, not scanned: they are the call's to expand, or a template.
//!
//! Attribute macros and derives are not found here: which attributes are macros is a name
//! resolution, and their items' extents a parse, so the caller's parse of the expansion (the
//! same one a file's items come from) finds them.

use blaze_own::{AVec, Alloc, Vec};

use crate::{
    error::StreamError,
    expansion::{NestedCall, NestedKind, PathScope, Range},
    tokens::{Edition, Raw, TokenKind, TokenStream, index},
};

/// The macro calls and definitions of `stream`, in order.
pub fn find_calls<'a>(
    stream: &TokenStream<'_>,
    alloc: Alloc<'a>,
) -> Result<AVec<'a, NestedCall>, StreamError> {
    let mut found = Vec::new_in(alloc);
    let tokens = stream.tokens();
    let mut at = 0;
    while at < tokens.len() {
        if let Some((call, next)) = definition_at(stream, at).or_else(|| call_at(stream, at)) {
            found.push(call);
            at = next;
        } else {
            at += 1;
        }
    }
    Ok(found)
}

/// `path ! group` at `at`: the call and the index after its group.
fn call_at(stream: &TokenStream<'_>, at: usize) -> Option<(NestedCall, usize)> {
    // The shape first, by kinds alone, then the names: most identifiers are no call's.
    let shape = path_shape_at(stream, at)?;
    let tokens = stream.tokens();
    if !is_punct(stream, shape, '!') || !matches!(tokens.get(shape + 1)?.kind, TokenKind::Open(_)) {
        return None;
    }
    // `path_at` ends at the shape's end, or earlier, before a `::`, so at no call.
    let end = path_at(stream, at)?;
    if end != shape {
        return None;
    }
    let close = stream.close_of(end + 1)?;
    let call = NestedCall {
        kind: NestedKind::FnLike,
        path: Range { start: index(at), end: index(end) },
        name: None,
        args: Range { start: index(end + 1), end: index(close + 1) },
        params: None,
        scope: PathScope::AsWritten,
    };
    Some((call, close + 1))
}

/// `macro_rules ! name group` or `macro name (params)? {body}` at `at`.
fn definition_at(stream: &TokenStream<'_>, at: usize) -> Option<(NestedCall, usize)> {
    let tokens = stream.tokens();
    let keyword = plain_ident(stream, at)?;
    let path = Range { start: index(at), end: index(at + 1) };
    match keyword {
        b"macro_rules" => {
            if !is_punct(stream, at + 1, '!') || plain_or_raw_ident(stream, at + 2).is_none() {
                return None;
            }
            let open = at + 3;
            if !matches!(tokens.get(open)?.kind, TokenKind::Open(_)) {
                return None;
            }
            let close = stream.close_of(open)?;
            let call = NestedCall {
                kind: NestedKind::MacroRules,
                path,
                name: Some(index(at + 2)),
                args: Range { start: index(open), end: index(close + 1) },
                params: None,
                scope: PathScope::AsWritten,
            };
            Some((call, close + 1))
        }
        b"macro" => {
            plain_or_raw_ident(stream, at + 1)?;
            let mut open = at + 2;
            let mut params = None;
            if matches!(tokens.get(open)?.kind, TokenKind::Open(crate::tokens::Delim::Paren)) {
                let close = stream.close_of(open)?;
                params = Some(Range { start: index(open), end: index(close + 1) });
                open = close + 1;
            }
            if !matches!(tokens.get(open)?.kind, TokenKind::Open(crate::tokens::Delim::Brace)) {
                return None;
            }
            let close = stream.close_of(open)?;
            let call = NestedCall {
                kind: NestedKind::Macro2,
                path,
                name: Some(index(at + 1)),
                args: Range { start: index(open), end: index(close + 1) },
                params,
                scope: PathScope::AsWritten,
            };
            Some((call, close + 1))
        }
        _ => None,
    }
}

/// The end of a macro path that starts at `at`: `::`? segment (`::` segment)*. The last
/// segment, the macro's name, is no keyword; one before it may be `crate`, `self`, `super`,
/// `Self` or `$crate`.
fn path_at(stream: &TokenStream<'_>, at: usize) -> Option<usize> {
    let mut next = at;
    if is_path_sep(stream, next) {
        next += 2;
    }
    let mut last_is_name = segment(stream, next)?;
    next += 1;
    while is_path_sep(stream, next) && segment(stream, next + 2).is_some() {
        last_is_name = segment(stream, next + 2)?;
        next += 3;
    }
    last_is_name.then_some(next)
}

/// The end of `::`? ident (`::` ident)* at `at`, by the tokens' kinds alone: where
/// [`path_at`] can end at the furthest.
fn path_shape_at(stream: &TokenStream<'_>, at: usize) -> Option<usize> {
    let tokens = stream.tokens();
    let ident = |at: usize| matches!(tokens.get(at).map(|it| it.kind), Some(TokenKind::Ident { .. }));
    let mut next = at;
    if is_path_sep(stream, next) {
        next += 2;
    }
    if !ident(next) {
        return None;
    }
    next += 1;
    while is_path_sep(stream, next) && ident(next + 2) {
        next += 3;
    }
    Some(next)
}

/// Whether the token at `at` can be a path segment, and if so whether it can be a macro's
/// name (not a path keyword).
fn segment(stream: &TokenStream<'_>, at: usize) -> Option<bool> {
    let token = stream.tokens().get(at)?;
    let TokenKind::Ident { raw, .. } = token.kind else { return None };
    if raw == Raw::Yes {
        return Some(true);
    }
    let text = stream.text_of(token);
    if matches!(text, "crate" | "self" | "super" | "Self" | "$crate") {
        return Some(false);
    }
    (!is_keyword(text, stream.edition_of(token.ctx))).then_some(true)
}

fn is_path_sep(stream: &TokenStream<'_>, at: usize) -> bool {
    matches!(
        stream.tokens().get(at).map(|it| it.kind),
        Some(TokenKind::Punct { char: ':', spacing: crate::tokens::Spacing::Joint })
    ) && is_punct(stream, at + 1, ':')
}

fn is_punct(stream: &TokenStream<'_>, at: usize, want: char) -> bool {
    matches!(stream.tokens().get(at).map(|it| it.kind), Some(TokenKind::Punct { char, .. }) if char == want)
}

/// The text of a plain (not raw) identifier at `at`.
fn plain_ident<'s>(stream: &'s TokenStream<'_>, at: usize) -> Option<&'s [u8]> {
    let token = stream.tokens().get(at)?;
    matches!(token.kind, TokenKind::Ident { raw: Raw::No, .. }).then(|| stream.bytes_at(at))
}

fn plain_or_raw_ident(stream: &TokenStream<'_>, at: usize) -> Option<()> {
    matches!(stream.tokens().get(at)?.kind, TokenKind::Ident { .. }).then_some(())
}

/// Whether `text` is a strict or reserved keyword in `edition`: no macro's name.
fn is_keyword(text: &str, edition: Edition) -> bool {
    let all = matches!(
        text,
        "as" | "break"
            | "const"
            | "continue"
            | "crate"
            | "else"
            | "enum"
            | "extern"
            | "false"
            | "fn"
            | "for"
            | "if"
            | "impl"
            | "in"
            | "let"
            | "loop"
            | "match"
            | "mod"
            | "move"
            | "mut"
            | "pub"
            | "ref"
            | "return"
            | "self"
            | "Self"
            | "static"
            | "struct"
            | "super"
            | "trait"
            | "true"
            | "type"
            | "unsafe"
            | "use"
            | "where"
            | "while"
            | "abstract"
            | "become"
            | "box"
            | "do"
            | "final"
            | "macro"
            | "override"
            | "priv"
            | "typeof"
            | "unsized"
            | "virtual"
            | "yield"
    );
    let since_2018 = edition >= Edition::E2018 && matches!(text, "async" | "await" | "dyn" | "try");
    let since_2024 = edition >= Edition::E2024 && text == "gen";
    all || since_2018 || since_2024
}
