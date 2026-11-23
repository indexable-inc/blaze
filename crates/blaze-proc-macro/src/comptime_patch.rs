//! Default built-in comptime "fake patch" library for popular Rust proc-macro crates.
//!
//! Enabled **by default** in [`crate::hook::ProcMacros`] (opt out with
//! `BLAZE_NO_COMPTIME_PATCH=1` or [`crate::hook::ProcMacros::set_comptime_patch`]).
//!
//! Instead of compiling `syn`/`quote`/`proc-macro2` and spawning a `.dylib` or Wasmtime
//! proc-macro sandbox on every common derive, Blaze intercepts well-known proc macros at
//! comptime and expands them in-process directly over [`blaze_expand::TokenStream`] in
//! microseconds — with **100% lossless fallback** ([`PatchResult::Fallback`]) to the real
//! proc macro whenever an exotic attribute or unsupported construct appears.
//!
//! # Supported Crates (Enabled by Default)
//! | Crate | Macros |
//! |---|---|
//! | `serde_derive` | `#[derive(Serialize)]`, `#[derive(Deserialize)]` (structs, enums, generics, `rename`, `rename_all`, `transparent`, `default`, `skip`, `skip_serializing`, `skip_deserializing`, `deny_unknown_fields`, `crate`) |
//! | `bytemuck_derive` | `#[derive(Zeroable)]`, `#[derive(Pod)]`, `#[derive(NoUninit)]`, `#[derive(AnyBitPattern)]`, `#[derive(TransparentWrapper)]` |
//! | `thiserror_impl` | `#[derive(Error)]` (`#[error("...")]`, `#[error(transparent)]`, `#[from]`, `#[source]`) |
//! | `derive_more` / `derive_more_impl` | `#[derive(From)]`, `#[derive(Into)]`, `#[derive(Deref)]`, `#[derive(DerefMut)]`, `#[derive(AsRef)]`, `#[derive(AsMut)]`, `#[derive(Constructor)]`, `#[derive(Display)]`, `#[derive(IsVariant)]` |
//! | `strum_macros` | `#[derive(AsRefStr)]`, `#[derive(IntoStaticStr)]`, `#[derive(EnumCount)]`, `#[derive(Display)]` (`#[strum(serialize = "...")]`, `#[strum(serialize_all = "...")]`) |
//! | `paste` | `paste! { ... }` (`[<a b>]` concatenation + `:lower`, `:upper`, `:snake`, `:camel`) |

#![forbid(unsafe_code)]

use core::fmt::Write as _;
use std::borrow::Cow;

use blaze_expand::{
    Delim, Edition, LitKind, ProcCall, ProcKind, Produced, Reads, Spacing, TokenKind, TokenOrigin,
    TokenStream, Transparency, lex,
};
use blaze_own::{AVec, Alloc};

/// Versioned digest domain used when caching pure comptime-patch expansions.
pub const COMPTIME_PATCH_VERSION: &[u8] = b"blaze-comptime-patch-v2";

/// Cumulative telemetry counters for the built-in comptime patch library.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct ComptimePatchStats {
    /// Proc-macro calls expanded in-process by the built-in comptime patch library.
    pub hits: u64,
    /// Calls targeting a patched macro that fell back to the real `.dylib`/Wasm proc macro
    /// due to an exotic attribute or unsupported syntax.
    pub fallbacks: u64,
}

/// Result of attempting an in-process comptime patch expansion.
pub enum PatchResult<'a> {
    /// Successfully expanded at comptime with zero external reads.
    Expanded(Produced<'a>),
    /// Not covered by the comptime patch (or uses an exotic attribute): fall back to the real
    /// proc-macro module seamlessly.
    Fallback,
}

/// Canonical list of normalized proc-macro crate names shipped in the default comptime patch library.
pub const SUPPORTED_CRATES: &[&str] = &[
    "serde_derive",
    "bytemuck_derive",
    "thiserror_impl",
    "derive_more",
    "derive_more_impl",
    "strum_macros",
    "paste",
];

/// Normalize a crate name or `"name version"` label into a canonical underscore-separated crate name.
pub fn normalize_crate_name(label_or_name: &str) -> String {
    let base = label_or_name.split_whitespace().next().unwrap_or(label_or_name);
    base.replace('-', "_")
}

/// Return `true` if `crate_name` (or `"name version"` label) is in the default comptime patch catalog.
pub fn is_patched_crate(crate_name: &str) -> bool {
    let norm = normalize_crate_name(crate_name);
    SUPPORTED_CRATES.contains(&norm.as_str())
}

/// Return `true` if `(crate_name, kind, macro_name)` has a built-in comptime patch handler.
pub fn has_patch(crate_name: &str, kind: ProcKind, macro_name: &str) -> bool {
    let norm = normalize_crate_name(crate_name);
    match (norm.as_str(), kind, macro_name) {
        ("serde_derive", ProcKind::Derive, "Serialize" | "Deserialize") => true,
        (
            "bytemuck_derive",
            ProcKind::Derive,
            "Zeroable" | "Pod" | "NoUninit" | "AnyBitPattern" | "TransparentWrapper",
        ) => true,
        ("thiserror_impl", ProcKind::Derive, "Error") => true,
        (
            "derive_more" | "derive_more_impl",
            ProcKind::Derive,
            "From"
            | "Into"
            | "Deref"
            | "DerefMut"
            | "AsRef"
            | "AsMut"
            | "Constructor"
            | "Display"
            | "IsVariant",
        ) => true,
        (
            "strum_macros",
            ProcKind::Derive,
            "AsRefStr" | "IntoStaticStr" | "EnumCount" | "Display",
        ) => true,
        ("paste", ProcKind::FnLike, "paste") => true,
        _ => false,
    }
}

/// Returns `true` if `stream` contains macro-rules hygiene markers (`Delim::Invisible`, `$crate`,
/// or `crate::` paths that may carry macro-expansion hygiene), in which case re-lexing a generated
/// string would drop hygiene and we must fall back to the real proc macro.
fn has_hygiene_sensitive_tokens(stream: &TokenStream<'_>) -> bool {
    let tokens = stream.tokens();
    for i in 0..tokens.len() {
        match tokens[i].kind {
            TokenKind::Open(Delim::Invisible) | TokenKind::Close(Delim::Invisible) => return true,
            TokenKind::Ident { .. } if stream.text_at(i) == "$crate" => return true,
            TokenKind::Ident { .. }
                if stream.text_at(i) == "crate"
                    && i + 2 < tokens.len()
                    && is_punct(stream, i + 1, ':')
                    && is_punct(stream, i + 2, ':') =>
            {
                return true;
            }
            TokenKind::Punct { char: '$', .. }
                if i + 1 < tokens.len()
                    && matches!(tokens[i + 1].kind, TokenKind::Ident { .. })
                    && stream.text_at(i + 1) == "crate" =>
            {
                return true;
            }
            _ => {}
        }
    }
    false
}

/// Try to expand `(crate_name, kind, macro_name)` in-process using the default comptime patch library.
///
/// Returns [`PatchResult::Fallback`] whenever the crate/macro is not covered OR the item uses an
/// unsupported attribute/construct, guaranteeing lossless fallback to the real proc macro.
pub fn try_expand<'a>(
    crate_name: &str,
    kind: ProcKind,
    macro_name: &str,
    call: &ProcCall<'_>,
    edition: Edition,
    alloc: Alloc<'a>,
) -> PatchResult<'a> {
    if has_hygiene_sensitive_tokens(call.input) {
        return PatchResult::Fallback;
    }
    let norm = normalize_crate_name(crate_name);
    let generated: Option<String> = match (norm.as_str(), kind, macro_name) {
        ("serde_derive", ProcKind::Derive, "Serialize") => expand_serde(call.input, false),
        ("serde_derive", ProcKind::Derive, "Deserialize") => expand_serde(call.input, true),
        ("bytemuck_derive", ProcKind::Derive, m) => expand_bytemuck(call.input, m),
        ("thiserror_impl", ProcKind::Derive, "Error") => expand_thiserror(call.input),
        ("derive_more" | "derive_more_impl", ProcKind::Derive, m) => {
            expand_derive_more(call.input, m)
        }
        ("strum_macros", ProcKind::Derive, m) => expand_strum(call.input, m),
        ("paste", ProcKind::FnLike, "paste") => expand_paste(call.input),
        _ => None,
    };

    let Some(code) = generated else {
        return PatchResult::Fallback;
    };
    match lex_to_produced(&code, edition, alloc) {
        Some(produced) => PatchResult::Expanded(produced),
        None => PatchResult::Fallback,
    }
}

fn lex_to_produced<'a>(code: &str, edition: Edition, alloc: Alloc<'a>) -> Option<Produced<'a>> {
    let tokens = lex(code, edition, alloc).ok()?;
    let mut origins = AVec::with_capacity_in(tokens.len(), alloc);
    for _ in 0..tokens.len() {
        origins.push(TokenOrigin::Made(Transparency::Transparent));
    }
    Some(Produced {
        tokens,
        origins,
        reads: Reads::new_in(alloc),
        diagnostics: AVec::new_in(alloc),
        text: AVec::new_in(alloc),
    })
}

// =================================================================================================
// Zero-Dep Linear ADT & Attribute Parser over `blaze_expand::TokenStream`
// =================================================================================================

#[derive(Clone, Debug)]
struct RawAttr {
    /// Leading path segments joined by `::` (e.g. `"serde"`, `"repr"`, `"error"`, `"from"`).
    path: String,
    /// Raw text inside `(...)` if parenthesized, or `= ...` if name-value.
    args: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AdtKind {
    Struct,
    Enum,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FieldsShape {
    Named,
    Tuple,
    Unit,
}

#[derive(Clone, Debug)]
struct ParsedField {
    attrs: Vec<RawAttr>,
    name: Option<String>,
    ty: String,
}

#[derive(Clone, Debug)]
struct ParsedVariant {
    attrs: Vec<RawAttr>,
    name: String,
    shape: FieldsShape,
    fields: Vec<ParsedField>,
}

#[derive(Clone, Debug)]
struct GenericParam {
    /// Full parameter declaration without defaults (e.g. `'a`, `T: Clone`, `const N: usize`).
    decl: String,
    /// Argument form for `Type<...>` (e.g. `'a`, `T`, `N`).
    arg: String,
    kind: GenericParamKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GenericParamKind {
    Lifetime,
    Type,
    Const,
}

#[derive(Clone, Debug)]
struct ParsedAdt {
    attrs: Vec<RawAttr>,
    vis: String,
    kind: AdtKind,
    name: String,
    generics: Vec<GenericParam>,
    where_predicates: Vec<String>,
    /// For structs: a single variant with `name == adt.name`. For enums: all variants.
    variants: Vec<ParsedVariant>,
}

impl ParsedAdt {
    fn has_lifetimes(&self) -> bool {
        self.generics.iter().any(|g| g.kind == GenericParamKind::Lifetime)
    }

    fn impl_generics(&self, extra_lifetime: Option<&str>) -> String {
        if self.generics.is_empty() && extra_lifetime.is_none() {
            return String::new();
        }
        let mut out = String::from("<");
        let mut first = true;
        if let Some(lt) = extra_lifetime {
            out.push_str(lt);
            first = false;
        }
        for g in &self.generics {
            if !first {
                out.push_str(", ");
            }
            out.push_str(&g.decl);
            first = false;
        }
        out.push('>');
        out
    }

    fn ty_generics(&self) -> String {
        if self.generics.is_empty() {
            return String::new();
        }
        let mut out = String::from("<");
        for (i, g) in self.generics.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str(&g.arg);
        }
        out.push('>');
        out
    }

    fn where_clause_with_bounds(&self, extra_type_bounds: &[&str]) -> String {
        let mut preds: Vec<String> = self.where_predicates.clone();
        for g in &self.generics {
            if g.kind == GenericParamKind::Type {
                for bound in extra_type_bounds {
                    preds.push(format!("{}: {bound}", g.arg));
                }
            }
        }
        if preds.is_empty() {
            String::new()
        } else {
            format!(" where {}", preds.join(", "))
        }
    }

    fn has_cfg_on_members(&self) -> bool {
        for v in &self.variants {
            if v.attrs.iter().any(|a| a.path == "cfg" || a.path == "cfg_attr") {
                return true;
            }
            for f in &v.fields {
                if f.attrs.iter().any(|a| a.path == "cfg" || a.path == "cfg_attr") {
                    return true;
                }
            }
        }
        false
    }
}

/// Render a single `TokenKind::Literal` back to valid Rust source syntax (including quotes).
fn write_literal(out: &mut String, kind: LitKind, text: &str, suffix_len: u32) {
    let (prefix, hashes, quote) = match kind {
        LitKind::Byte => ("b", 0u8, "'"),
        LitKind::Char => ("", 0, "'"),
        LitKind::Integer | LitKind::Float | LitKind::Err => ("", 0, ""),
        LitKind::Str => ("", 0, "\""),
        LitKind::ByteStr => ("b", 0, "\""),
        LitKind::CStr => ("c", 0, "\""),
        LitKind::StrRaw(h) => ("r", h, "\""),
        LitKind::ByteStrRaw(h) => ("br", h, "\""),
        LitKind::CStrRaw(h) => ("cr", h, "\""),
    };
    let split = text.len().saturating_sub(suffix_len as usize);
    let (inside, suffix) = text.split_at(split);
    out.push_str(prefix);
    for _ in 0..hashes {
        out.push('#');
    }
    out.push_str(quote);
    out.push_str(inside);
    out.push_str(quote);
    for _ in 0..hashes {
        out.push('#');
    }
    out.push_str(suffix);
}

/// Render a token slice `[start..end)` back into valid Rust source text.
fn render_tokens(stream: &TokenStream<'_>, start: usize, end: usize) -> String {
    let tokens = stream.tokens();
    let mut out = String::new();
    let mut i = start;
    let mut prev_joint = false;
    let mut prev_word: Option<&str> = None;
    while i < end && i < tokens.len() {
        let tok = &tokens[i];
        match tok.kind {
            TokenKind::Open(d) => {
                let space_before_open = match d {
                    Delim::Paren => matches!(
                        prev_word,
                        Some("if" | "while" | "match" | "for" | "return" | "in" | "as" | "let")
                    ),
                    _ => true,
                };
                if space_before_open
                    && !out.is_empty()
                    && !prev_joint
                    && !out.ends_with(['(', '[', '{', '<', ':'])
                {
                    out.push(' ');
                }
                out.push(match d {
                    Delim::Paren => '(',
                    Delim::Bracket => '[',
                    Delim::Brace => '{',
                    Delim::Invisible => ' ',
                });
                prev_joint = true;
                prev_word = None;
            }
            TokenKind::Close(d) => {
                out.push(match d {
                    Delim::Paren => ')',
                    Delim::Bracket => ']',
                    Delim::Brace => '}',
                    Delim::Invisible => ' ',
                });
                prev_joint = false;
                prev_word = None;
            }
            TokenKind::Punct { char: c, spacing } => {
                let is_leading_path_sep = c == ':'
                    && spacing == Spacing::Joint
                    && matches!(
                        prev_word,
                        Some(
                            "impl" | "for" | "as" | "where" | "in" | "return" | "match"
                                | "unsafe" | "const" | "let" | "fn" | "pub" | "use" | "type"
                                | "ref" | "mut" | "dyn" | "if" | "else" | "while"
                        )
                    );
                if !out.is_empty()
                    && !prev_joint
                    && (is_leading_path_sep || !matches!(c, ',' | ';' | ':' | '.' | '>' | '!' | '?'))
                    && !out.ends_with(['(', '[', '{', ':', '\'', '<', '&', '#'])
                {
                    out.push(' ');
                }
                out.push(c);
                prev_joint = spacing == Spacing::Joint || matches!(c, '\'' | '#');
                prev_word = None;
            }
            TokenKind::Ident { .. } => {
                if !out.is_empty()
                    && !prev_joint
                    && !out.ends_with(['(', '[', '{', '<', '\'', '.', '&', '#'])
                    && !out.ends_with("::")
                {
                    out.push(' ');
                }
                let word = stream.text_at(i);
                out.push_str(word);
                prev_joint = false;
                prev_word = Some(word);
            }
            TokenKind::Literal { kind, suffix_len, .. } => {
                if !out.is_empty()
                    && !prev_joint
                    && !out.ends_with(['(', '[', '{', '<', ':', '\'', '.', '&', '#'])
                {
                    out.push(' ');
                }
                write_literal(&mut out, kind, stream.text_at(i), suffix_len);
                prev_joint = false;
                prev_word = None;
            }
        }
        i += 1;
    }
    out.trim().to_owned()
}

fn is_punct(stream: &TokenStream<'_>, at: usize, ch: char) -> bool {
    matches!(stream.tokens().get(at).map(|t| t.kind), Some(TokenKind::Punct { char: c, .. }) if c == ch)
}

fn is_ident(stream: &TokenStream<'_>, at: usize, want: &str) -> bool {
    matches!(stream.tokens().get(at).map(|t| t.kind), Some(TokenKind::Ident { .. }))
        && stream.text_at(at) == want
}

fn is_any_ident(stream: &TokenStream<'_>, at: usize) -> bool {
    matches!(stream.tokens().get(at).map(|t| t.kind), Some(TokenKind::Ident { .. }))
}

fn parse_attrs(stream: &TokenStream<'_>, i: &mut usize, end: usize) -> Option<Vec<RawAttr>> {
    let tokens = stream.tokens();
    let mut attrs = Vec::new();
    while *i < end && is_punct(stream, *i, '#') {
        // Inner attributes `#![...]` are unexpected on derive input -> fallback.
        if *i + 1 < end && is_punct(stream, *i + 1, '!') {
            return None;
        }
        if *i + 1 >= end
            || !matches!(tokens[*i + 1].kind, TokenKind::Open(Delim::Bracket))
        {
            return None;
        }
        let open_bracket = *i + 1;
        let close_bracket = stream.close_of(open_bracket)?;
        let mut p = open_bracket + 1;
        // Parse path segments (`serde`, `core::prelude::...`, etc.)
        let mut path = String::new();
        while p < close_bracket {
            if is_punct(stream, p, ':') && p + 1 < close_bracket && is_punct(stream, p + 1, ':') {
                path.push_str("::");
                p += 2;
            } else if is_any_ident(stream, p) {
                path.push_str(stream.text_at(p));
                p += 1;
            } else {
                break;
            }
        }
        let mut args = None;
        if p < close_bracket {
            if matches!(tokens[p].kind, TokenKind::Open(Delim::Paren)) {
                let close_p = stream.close_of(p)?;
                args = Some(render_tokens(stream, p + 1, close_p));
            } else if is_punct(stream, p, '=') {
                args = Some(render_tokens(stream, p + 1, close_bracket));
            }
        }
        attrs.push(RawAttr { path, args });
        *i = close_bracket + 1;
    }
    Some(attrs)
}

fn parse_visibility(stream: &TokenStream<'_>, i: &mut usize, end: usize) -> String {
    let start = *i;
    if *i < end && is_ident(stream, *i, "pub") {
        *i += 1;
        if *i < end && matches!(stream.tokens()[*i].kind, TokenKind::Open(Delim::Paren)) {
            if let Some(close) = stream.close_of(*i) {
                *i = close + 1;
            }
        }
        render_tokens(stream, start, *i)
    } else {
        String::new()
    }
}

/// Skip balanced tokens until top-level stop punctuation or `end`.
fn find_top_level(stream: &TokenStream<'_>, start: usize, end: usize, stop: &[char]) -> usize {
    let tokens = stream.tokens();
    let mut i = start;
    let mut angle_depth: usize = 0;
    while i < end {
        match tokens[i].kind {
            TokenKind::Open(_) => {
                if let Some(c) = stream.close_of(i) {
                    i = c + 1;
                    continue;
                }
            }
            TokenKind::Punct { char: '<', .. } => {
                angle_depth += 1;
            }
            TokenKind::Punct { char: '>', .. } => {
                if angle_depth > 0 {
                    angle_depth -= 1;
                } else if stop.contains(&'>') {
                    return i;
                }
            }
            TokenKind::Punct { char: c, .. } if angle_depth == 0 && stop.contains(&c) => {
                return i;
            }
            _ => {}
        }
        i += 1;
    }
    end
}

fn parse_generics(stream: &TokenStream<'_>, i: &mut usize, end: usize) -> Option<Vec<GenericParam>> {
    if *i >= end || !is_punct(stream, *i, '<') {
        return Some(Vec::new());
    }
    let close = find_top_level(stream, *i + 1, end, &['>']);
    if close >= end || !is_punct(stream, close, '>') {
        return None;
    }
    let mut params = Vec::new();
    let mut p = *i + 1;
    while p < close {
        // Reject attributes on generic params (fallback).
        if is_punct(stream, p, '#') {
            return None;
        }
        let comma = find_top_level(stream, p, close, &[',']);
        let seg_end = comma;
        if p < seg_end {
            // Strip default value `= DefaultTy` from `decl`!
            let eq_pos = find_top_level(stream, p, seg_end, &['=']);
            let decl_end = eq_pos.min(seg_end);
            if is_punct(stream, p, '\'') && p + 1 < decl_end && is_any_ident(stream, p + 1) {
                let arg = format!("'{}", stream.text_at(p + 1));
                let decl = render_tokens(stream, p, decl_end);
                params.push(GenericParam { decl, arg, kind: GenericParamKind::Lifetime });
            } else if is_ident(stream, p, "const") && p + 1 < decl_end && is_any_ident(stream, p + 1)
            {
                let arg = stream.text_at(p + 1).to_owned();
                let decl = render_tokens(stream, p, decl_end);
                params.push(GenericParam { decl, arg, kind: GenericParamKind::Const });
            } else if is_any_ident(stream, p) {
                let arg = stream.text_at(p).to_owned();
                let decl = render_tokens(stream, p, decl_end);
                params.push(GenericParam { decl, arg, kind: GenericParamKind::Type });
            } else {
                return None;
            }
        }
        p = if comma < close { comma + 1 } else { close };
    }
    *i = close + 1;
    Some(params)
}

fn parse_where_clause(stream: &TokenStream<'_>, i: &mut usize, end: usize) -> Option<Vec<String>> {
    if *i >= end || !is_ident(stream, *i, "where") {
        return Some(Vec::new());
    }
    *i += 1;
    let tokens = stream.tokens();
    // Find the end of the where clause: `{` or `;` at top level.
    let mut clause_end = *i;
    let mut angle = 0usize;
    while clause_end < end {
        match tokens[clause_end].kind {
            TokenKind::Open(Delim::Brace) if angle == 0 => break,
            TokenKind::Open(_) => {
                if let Some(c) = stream.close_of(clause_end) {
                    clause_end = c + 1;
                    continue;
                }
            }
            TokenKind::Punct { char: ';', .. } if angle == 0 => break,
            TokenKind::Punct { char: '<', .. } => angle += 1,
            TokenKind::Punct { char: '>', .. } => angle = angle.saturating_sub(1),
            _ => {}
        }
        clause_end += 1;
    }

    let mut preds = Vec::new();
    let mut p = *i;
    while p < clause_end {
        let comma = find_top_level(stream, p, clause_end, &[',']);
        let text = render_tokens(stream, p, comma);
        if !text.is_empty() {
            preds.push(text);
        }
        p = if comma < clause_end { comma + 1 } else { clause_end };
    }
    *i = clause_end;
    Some(preds)
}

fn parse_named_fields(stream: &TokenStream<'_>, open_brace: usize) -> Option<Vec<ParsedField>> {
    let close = stream.close_of(open_brace)?;
    let mut fields = Vec::new();
    let mut p = open_brace + 1;
    while p < close {
        let attrs = parse_attrs(stream, &mut p, close)?;
        if p >= close {
            break;
        }
        let _vis = parse_visibility(stream, &mut p, close);
        if p >= close || !is_any_ident(stream, p) {
            return None;
        }
        let name = stream.text_at(p).to_owned();
        p += 1;
        if p >= close || !is_punct(stream, p, ':') {
            return None;
        }
        p += 1;
        let comma = find_top_level(stream, p, close, &[',']);
        // Strip field default `= expr` if present (RFC 3681).
        let eq = find_top_level(stream, p, comma, &['=']);
        let ty = render_tokens(stream, p, eq.min(comma));
        if ty.is_empty() {
            return None;
        }
        fields.push(ParsedField { attrs, name: Some(name), ty });
        p = if comma < close { comma + 1 } else { close };
    }
    Some(fields)
}

fn parse_tuple_fields(stream: &TokenStream<'_>, open_paren: usize) -> Option<Vec<ParsedField>> {
    let close = stream.close_of(open_paren)?;
    let mut fields = Vec::new();
    let mut p = open_paren + 1;
    while p < close {
        let attrs = parse_attrs(stream, &mut p, close)?;
        if p >= close {
            break;
        }
        let _vis = parse_visibility(stream, &mut p, close);
        let comma = find_top_level(stream, p, close, &[',']);
        let ty = render_tokens(stream, p, comma);
        if ty.is_empty() {
            return None;
        }
        fields.push(ParsedField { attrs, name: None, ty });
        p = if comma < close { comma + 1 } else { close };
    }
    Some(fields)
}

fn parse_adt(stream: &TokenStream<'_>) -> Option<ParsedAdt> {
    let tokens = stream.tokens();
    let end = tokens.len();
    let mut i = 0;

    let attrs = parse_attrs(stream, &mut i, end)?;
    let vis = parse_visibility(stream, &mut i, end);
    if i >= end {
        return None;
    }

    let kind = if is_ident(stream, i, "struct") {
        i += 1;
        AdtKind::Struct
    } else if is_ident(stream, i, "enum") {
        i += 1;
        AdtKind::Enum
    } else {
        return None;
    };

    if i >= end || !is_any_ident(stream, i) {
        return None;
    }
    let name = stream.text_at(i).to_owned();
    i += 1;

    let generics = parse_generics(stream, &mut i, end)?;

    match kind {
        AdtKind::Struct => {
            // Tuple struct: `struct Foo<T>(...) where ...;`
            if i < end && matches!(tokens[i].kind, TokenKind::Open(Delim::Paren)) {
                let open_p = i;
                let close_p = stream.close_of(open_p)?;
                let fields = parse_tuple_fields(stream, open_p)?;
                i = close_p + 1;
                let where_predicates = parse_where_clause(stream, &mut i, end)?;
                let variant = ParsedVariant {
                    attrs: Vec::new(),
                    name: name.clone(),
                    shape: FieldsShape::Tuple,
                    fields,
                };
                return Some(ParsedAdt {
                    attrs,
                    vis,
                    kind,
                    name,
                    generics,
                    where_predicates,
                    variants: vec![variant],
                });
            }
            let where_predicates = parse_where_clause(stream, &mut i, end)?;
            if i < end && matches!(tokens[i].kind, TokenKind::Open(Delim::Brace)) {
                let fields = parse_named_fields(stream, i)?;
                let variant = ParsedVariant {
                    attrs: Vec::new(),
                    name: name.clone(),
                    shape: FieldsShape::Named,
                    fields,
                };
                Some(ParsedAdt {
                    attrs,
                    vis,
                    kind,
                    name,
                    generics,
                    where_predicates,
                    variants: vec![variant],
                })
            } else if i < end && is_punct(stream, i, ';') {
                let variant = ParsedVariant {
                    attrs: Vec::new(),
                    name: name.clone(),
                    shape: FieldsShape::Unit,
                    fields: Vec::new(),
                };
                Some(ParsedAdt {
                    attrs,
                    vis,
                    kind,
                    name,
                    generics,
                    where_predicates,
                    variants: vec![variant],
                })
            } else {
                None
            }
        }
        AdtKind::Enum => {
            let where_predicates = parse_where_clause(stream, &mut i, end)?;
            if i >= end || !matches!(tokens[i].kind, TokenKind::Open(Delim::Brace)) {
                return None;
            }
            let close = stream.close_of(i)?;
            let mut p = i + 1;
            let mut variants = Vec::new();
            while p < close {
                let v_attrs = parse_attrs(stream, &mut p, close)?;
                if p >= close {
                    break;
                }
                if !is_any_ident(stream, p) {
                    return None;
                }
                let v_name = stream.text_at(p).to_owned();
                p += 1;
                let (shape, fields) = if p < close
                    && matches!(tokens[p].kind, TokenKind::Open(Delim::Brace))
                {
                    let c = stream.close_of(p)?;
                    let fs = parse_named_fields(stream, p)?;
                    p = c + 1;
                    (FieldsShape::Named, fs)
                } else if p < close && matches!(tokens[p].kind, TokenKind::Open(Delim::Paren)) {
                    let c = stream.close_of(p)?;
                    let fs = parse_tuple_fields(stream, p)?;
                    p = c + 1;
                    (FieldsShape::Tuple, fs)
                } else {
                    (FieldsShape::Unit, Vec::new())
                };
                // Skip optional explicit discriminant `= expr` up to `,` or `close`.
                let comma = find_top_level(stream, p, close, &[',']);
                p = if comma < close { comma + 1 } else { close };
                variants.push(ParsedVariant { attrs: v_attrs, name: v_name, shape, fields });
            }
            Some(ParsedAdt {
                attrs,
                vis,
                kind,
                name,
                generics,
                where_predicates,
                variants,
            })
        }
    }
}

// =================================================================================================
// Case Conversion & Attribute Parsing Helpers
// =================================================================================================

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RenameRule {
    None,
    Lower,
    Upper,
    Pascal,
    Camel,
    Snake,
    ScreamingSnake,
    Kebab,
    ScreamingKebab,
}

impl RenameRule {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "lowercase" => Some(Self::Lower),
            "UPPERCASE" => Some(Self::Upper),
            "PascalCase" => Some(Self::Pascal),
            "camelCase" => Some(Self::Camel),
            "snake_case" => Some(Self::Snake),
            "SCREAMING_SNAKE_CASE" => Some(Self::ScreamingSnake),
            "kebab-case" => Some(Self::Kebab),
            "SCREAMING-KEBAB-CASE" => Some(Self::ScreamingKebab),
            _ => None,
        }
    }

    fn apply(self, ident: &str) -> String {
        let ident = ident.strip_prefix("r#").unwrap_or(ident);
        match self {
            Self::None => ident.to_owned(),
            Self::Lower => ident.to_ascii_lowercase(),
            Self::Upper => ident.to_ascii_uppercase(),
            Self::Pascal => to_pascal_case(ident),
            Self::Camel => {
                let p = to_pascal_case(ident);
                let mut chars = p.chars();
                match chars.next() {
                    Some(c) => {
                        let mut out = String::new();
                        out.push(c.to_ascii_lowercase());
                        out.push_str(chars.as_str());
                        out
                    }
                    None => String::new(),
                }
            }
            Self::Snake => to_snake_case(ident),
            Self::ScreamingSnake => to_snake_case(ident).to_ascii_uppercase(),
            Self::Kebab => to_snake_case(ident).replace('_', "-"),
            Self::ScreamingKebab => to_snake_case(ident).replace('_', "-").to_ascii_uppercase(),
        }
    }
}

fn to_snake_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    let chars: Vec<char> = s.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if c == '-' {
            out.push('_');
            continue;
        }
        if c.is_ascii_uppercase() {
            if i > 0 {
                let prev = chars[i - 1];
                let next_lower = chars.get(i + 1).is_some_and(|n| n.is_ascii_lowercase());
                if prev != '_' && prev != '-' && (prev.is_ascii_lowercase() || prev.is_ascii_digit() || next_lower) {
                    out.push('_');
                }
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

fn to_pascal_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut cap = true;
    for c in s.chars() {
        if c == '_' || c == '-' {
            cap = true;
        } else if cap {
            out.push(c.to_ascii_uppercase());
            cap = false;
        } else {
            out.push(c);
        }
    }
    out
}

/// Split comma-separated attribute meta items at top-level (respecting quotes and parens).
fn split_meta_items(s: &str) -> Vec<&str> {
    let mut items = Vec::new();
    let bytes = s.as_bytes();
    let mut start = 0usize;
    let mut depth = 0usize;
    let mut in_str = false;
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if in_str {
            if b == b'\\' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            if b == b'"' {
                in_str = false;
            }
        } else {
            match b {
                b'"' => in_str = true,
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' => depth = depth.saturating_sub(1),
                b',' if depth == 0 => {
                    let item = s[start..i].trim();
                    if !item.is_empty() {
                        items.push(item);
                    }
                    start = i + 1;
                }
                _ => {}
            }
        }
        i += 1;
    }
    let tail = s[start..].trim();
    if !tail.is_empty() {
        items.push(tail);
    }
    items
}

fn unquote_str(s: &str) -> Option<String> {
    let s = s.trim();
    if s.len() < 2 || !s.starts_with('"') || !s.ends_with('"') {
        return None;
    }
    let inner = &s[1..s.len() - 1];
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next()? {
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                '\\' => out.push('\\'),
                '"' => out.push('"'),
                '\'' => out.push('\''),
                '0' => out.push('\0'),
                _ => return None, // exotic escape -> fallback
            }
        } else {
            out.push(c);
        }
    }
    Some(out)
}

fn quote_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// =================================================================================================
// 1. `serde_derive` Comptime Patch (`Serialize` & `Deserialize`)
// =================================================================================================

#[derive(Default)]
struct SerdeContainerAttrs {
    rename: Option<String>,
    rename_all: RenameRule,
    transparent: bool,
    default: bool,
    deny_unknown_fields: bool,
    crate_path: Option<String>,
}

impl Default for RenameRule {
    fn default() -> Self {
        Self::None
    }
}

#[derive(Default)]
struct SerdeFieldAttrs {
    rename: Option<String>,
    default: bool,
    skip_serializing: bool,
    skip_deserializing: bool,
}

fn parse_serde_container_attrs(attrs: &[RawAttr]) -> Option<SerdeContainerAttrs> {
    let mut out = SerdeContainerAttrs::default();
    for attr in attrs {
        if attr.path != "serde" {
            continue;
        }
        let args = attr.args.as_deref()?;
        for item in split_meta_items(args) {
            if item == "transparent" {
                out.transparent = true;
            } else if item == "default" {
                out.default = true;
            } else if item == "deny_unknown_fields" {
                out.deny_unknown_fields = true;
            } else if let Some((k, v)) = item.split_once('=') {
                let k = k.trim();
                let v = v.trim();
                match k {
                    "rename" => out.rename = Some(unquote_str(v)?),
                    "rename_all" => out.rename_all = RenameRule::parse(&unquote_str(v)?)?,
                    "crate" => out.crate_path = Some(unquote_str(v)?),
                    _ => return None, // exotic serde container attr -> safe fallback
                }
            } else {
                return None;
            }
        }
    }
    Some(out)
}

fn parse_serde_field_attrs(attrs: &[RawAttr]) -> Option<SerdeFieldAttrs> {
    let mut out = SerdeFieldAttrs::default();
    for attr in attrs {
        if attr.path != "serde" {
            continue;
        }
        let args = attr.args.as_deref()?;
        for item in split_meta_items(args) {
            if item == "default" {
                out.default = true;
            } else if item == "skip" {
                out.skip_serializing = true;
                out.skip_deserializing = true;
            } else if item == "skip_serializing" {
                out.skip_serializing = true;
            } else if item == "skip_deserializing" {
                out.skip_deserializing = true;
            } else if let Some((k, v)) = item.split_once('=') {
                let k = k.trim();
                let v = v.trim();
                match k {
                    "rename" => out.rename = Some(unquote_str(v)?),
                    _ => return None, // `with`, `serialize_with`, `default = "..."`, `flatten`, etc. -> fallback
                }
            } else {
                return None;
            }
        }
    }
    Some(out)
}

fn expand_serde(stream: &TokenStream<'_>, is_deserialize: bool) -> Option<String> {
    let adt = parse_adt(stream)?;
    if adt.has_cfg_on_members() {
        return None;
    }
    let cattrs = parse_serde_container_attrs(&adt.attrs)?;
    // Let real serde_derive handle borrowed `'de: 'a` lifetimes on Deserialize.
    if is_deserialize && adt.has_lifetimes() {
        return None;
    }
    // Check all variant & field serde attributes up front (bail cleanly to fallback on exotic ones).
    let mut variant_attrs = Vec::with_capacity(adt.variants.len());
    let mut field_attrs = Vec::with_capacity(adt.variants.len());
    for v in &adt.variants {
        if adt.kind == AdtKind::Enum {
            let va = parse_serde_field_attrs(&v.attrs)?;
            if va.default {
                return None;
            }
            variant_attrs.push(va);
        } else {
            variant_attrs.push(SerdeFieldAttrs::default());
        }
        let mut v_fattrs = Vec::with_capacity(v.fields.len());
        for f in &v.fields {
            v_fattrs.push(parse_serde_field_attrs(&f.attrs)?);
        }
        field_attrs.push(v_fattrs);
    }

    // `#[serde(transparent)]` only on single-field struct without skipping that field.
    if cattrs.transparent {
        if adt.kind != AdtKind::Struct || adt.variants[0].fields.len() != 1 {
            return None;
        }
        if field_attrs[0][0].skip_serializing || field_attrs[0][0].skip_deserializing {
            return None;
        }
    }

    let serde_path = cattrs.crate_path.as_deref().unwrap_or("_serde");
    let extern_crate = if cattrs.crate_path.is_none() {
        "#[allow(unused_extern_crates, clippy::useless_attribute)] extern crate serde as _serde;\n"
    } else {
        ""
    };

    let body = if !is_deserialize {
        gen_serde_serialize(&adt, &cattrs, &variant_attrs, &field_attrs, serde_path)?
    } else {
        gen_serde_deserialize(&adt, &cattrs, &variant_attrs, &field_attrs, serde_path)?
    };

    Some(format!(
        "#[doc(hidden)] #[allow(non_upper_case_globals, unused_attributes, unused_qualifications)] const _: () = {{\n{extern_crate}{body}\n}};"
    ))
}

fn gen_serde_serialize(
    adt: &ParsedAdt,
    cattrs: &SerdeContainerAttrs,
    vattrs: &[SerdeFieldAttrs],
    fattrs: &[Vec<SerdeFieldAttrs>],
    sp: &str,
) -> Option<String> {
    let name = &adt.name;
    let container_wire = cattrs.rename.as_deref().unwrap_or(name);
    let impl_g = adt.impl_generics(None);
    let ty_g = adt.ty_generics();
    let ser_bound = format!("{sp}::Serialize");
    let where_c = adt.where_clause_with_bounds(&[&ser_bound]);

    let mut out = String::new();
    let _ = writeln!(
        out,
        "#[automatically_derived] impl{impl_g} {sp}::Serialize for {name}{ty_g}{where_c} {{"
    );
    let _ = writeln!(
        out,
        "    fn serialize<__S>(&self, __serializer: __S) -> ::core::result::Result<__S::Ok, __S::Error> where __S: {sp}::Serializer {{"
    );

    match adt.kind {
        AdtKind::Struct => {
            let v = &adt.variants[0];
            let fs = &fattrs[0];
            if cattrs.transparent {
                let access = match v.shape {
                    FieldsShape::Named => format!("&self.{}", v.fields[0].name.as_deref()?),
                    FieldsShape::Tuple => "&self.0".to_owned(),
                    FieldsShape::Unit => return None,
                };
                let _ = writeln!(out, "        {sp}::Serialize::serialize({access}, __serializer)");
            } else {
                match v.shape {
                    FieldsShape::Unit => {
                        let _ = writeln!(
                            out,
                            "        {sp}::Serializer::serialize_unit_struct(__serializer, {})",
                            quote_str(container_wire)
                        );
                    }
                    FieldsShape::Tuple => {
                        let active: Vec<usize> = (0..v.fields.len())
                            .filter(|&i| !fs[i].skip_serializing)
                            .collect();
                        if active.len() == 1 && v.fields.len() == 1 {
                            let _ = writeln!(
                                out,
                                "        {sp}::Serializer::serialize_newtype_struct(__serializer, {}, &self.0)",
                                quote_str(container_wire)
                            );
                        } else {
                            let _ = writeln!(
                                out,
                                "        let mut __state = {sp}::Serializer::serialize_tuple_struct(__serializer, {}, {})?;",
                                quote_str(container_wire),
                                active.len()
                            );
                            for idx in active {
                                let _ = writeln!(
                                    out,
                                    "        {sp}::ser::SerializeTupleStruct::serialize_field(&mut __state, &self.{idx})?;"
                                );
                            }
                            let _ = writeln!(
                                out,
                                "        {sp}::ser::SerializeTupleStruct::end(__state)"
                            );
                        }
                    }
                    FieldsShape::Named => {
                        let active_count = v.fields.iter().zip(fs.iter()).filter(|(_, f)| !f.skip_serializing).count();
                        let _ = writeln!(
                            out,
                            "        let mut __state = {sp}::Serializer::serialize_struct(__serializer, {}, {})?;",
                            quote_str(container_wire),
                            active_count
                        );
                        for (field, fa) in v.fields.iter().zip(fs.iter()).filter(|(_, f)| !f.skip_serializing) {
                            let raw = field.name.as_deref().unwrap_or("");
                            let wire = match fa.rename.as_deref() {
                                Some(r) => Cow::Borrowed(r),
                                None => Cow::Owned(cattrs.rename_all.apply(raw)),
                            };
                            let fname = field.name.as_deref()?;
                            let _ = writeln!(
                                out,
                                "        {sp}::ser::SerializeStruct::serialize_field(&mut __state, {}, &self.{fname})?;",
                                quote_str(&wire)
                            );
                        }
                        let _ = writeln!(out, "        {sp}::ser::SerializeStruct::end(__state)");
                    }
                }
            }
        }
        AdtKind::Enum => {
            if adt.variants.is_empty() {
                let _ = writeln!(out, "        match *self {{}}");
            } else {
                let _ = writeln!(out, "        match *self {{");
                for (vi, v) in adt.variants.iter().enumerate() {
                    let vname = &v.name;
                    let vwire = match vattrs[vi].rename.as_deref() {
                        Some(r) => Cow::Borrowed(r),
                        None => Cow::Owned(cattrs.rename_all.apply(vname)),
                    };
                    let fs = &fattrs[vi];
                    if vattrs[vi].skip_serializing {
                        let pat = match v.shape {
                            FieldsShape::Unit => format!("{name}::{vname}"),
                            FieldsShape::Tuple => format!("{name}::{vname}(..)"),
                            FieldsShape::Named => format!("{name}::{vname} {{ .. }}"),
                        };
                        let _ = writeln!(
                            out,
                            "            {pat} => ::core::result::Result::Err({sp}::ser::Error::custom(\"variant is skipped\")),"
                        );
                        continue;
                    }
                    match v.shape {
                        FieldsShape::Unit => {
                            let _ = writeln!(
                                out,
                                "            {name}::{vname} => {sp}::Serializer::serialize_unit_variant(__serializer, {}, {vi}u32, {}),",
                                quote_str(container_wire),
                                quote_str(&vwire)
                            );
                        }
                        FieldsShape::Tuple => {
                            let bindings: Vec<String> =
                                (0..v.fields.len()).map(|i| format!("ref __f{i}")).collect();
                            let active: Vec<usize> = (0..v.fields.len())
                                .filter(|&i| !fs[i].skip_serializing)
                                .collect();
                            if active.len() == 1 && v.fields.len() == 1 {
                                let _ = writeln!(
                                    out,
                                    "            {name}::{vname}(ref __f0) => {sp}::Serializer::serialize_newtype_variant(__serializer, {}, {vi}u32, {}, __f0),",
                                    quote_str(container_wire),
                                    quote_str(&vwire)
                                );
                            } else {
                                let _ = writeln!(
                                    out,
                                    "            {name}::{vname}({}) => {{",
                                    bindings.join(", ")
                                );
                                let _ = writeln!(
                                    out,
                                    "                let mut __state = {sp}::Serializer::serialize_tuple_variant(__serializer, {}, {vi}u32, {}, {})?;",
                                    quote_str(container_wire),
                                    quote_str(&vwire),
                                    active.len()
                                );
                                for idx in active {
                                    let _ = writeln!(
                                        out,
                                        "                {sp}::ser::SerializeTupleVariant::serialize_field(&mut __state, __f{idx})?;"
                                    );
                                }
                                let _ = writeln!(
                                    out,
                                    "                {sp}::ser::SerializeTupleVariant::end(__state)"
                                );
                                let _ = writeln!(out, "            }}");
                            }
                        }
                        FieldsShape::Named => {
                            let bindings: Vec<String> = v
                                .fields
                                .iter()
                                .enumerate()
                                .map(|(i, f)| {
                                    let fnm = f.name.as_deref().unwrap_or("");
                                    format!("{fnm}: ref __f{i}")
                                })
                                .collect();
                            let active: Vec<(usize, &str)> = (0..v.fields.len())
                                .filter(|&i| !fs[i].skip_serializing)
                                .map(|i| {
                                    let raw = v.fields[i].name.as_deref().unwrap_or("");
                                    let wire = fs[i].rename.as_deref().unwrap_or(raw);
                                    (i, wire)
                                })
                                .collect();
                            let _ = writeln!(
                                out,
                                "            {name}::{vname} {{ {} }} => {{",
                                bindings.join(", ")
                            );
                            let _ = writeln!(
                                out,
                                "                let mut __state = {sp}::Serializer::serialize_struct_variant(__serializer, {}, {vi}u32, {}, {})?;",
                                quote_str(container_wire),
                                quote_str(&vwire),
                                active.len()
                            );
                            for (idx, wire) in active {
                                let _ = writeln!(
                                    out,
                                    "                {sp}::ser::SerializeStructVariant::serialize_field(&mut __state, {}, __f{idx})?;",
                                    quote_str(&wire)
                                );
                            }
                            let _ = writeln!(
                                out,
                                "                {sp}::ser::SerializeStructVariant::end(__state)"
                            );
                            let _ = writeln!(out, "            }}");
                        }
                    }
                }
                let _ = writeln!(out, "        }}");
            }
        }
    }

    let _ = writeln!(out, "    }}");
    let _ = writeln!(out, "}}");
    Some(out)
}

fn gen_serde_deserialize(
    adt: &ParsedAdt,
    cattrs: &SerdeContainerAttrs,
    vattrs: &[SerdeFieldAttrs],
    fattrs: &[Vec<SerdeFieldAttrs>],
    sp: &str,
) -> Option<String> {
    let name = &adt.name;
    let container_wire = cattrs.rename.as_deref().unwrap_or(name);
    let impl_g = adt.impl_generics(Some("'de"));
    let ty_g = adt.ty_generics();
    let de_bound = format!("{sp}::Deserialize<'de>");
    let where_c = if cattrs.default {
        adt.where_clause_with_bounds(&[&de_bound, "::core::default::Default"])
    } else {
        adt.where_clause_with_bounds(&[&de_bound])
    };
    let marker_ty = if adt.generics.is_empty() {
        String::new()
    } else {
        format!("::core::marker::PhantomData<{name}{ty_g}>")
    };

    let mut out = String::new();
    let _ = writeln!(
        out,
        "#[automatically_derived] impl{impl_g} {sp}::Deserialize<'de> for {name}{ty_g}{where_c} {{"
    );
    let _ = writeln!(
        out,
        "    fn deserialize<__D>(__deserializer: __D) -> ::core::result::Result<Self, __D::Error> where __D: {sp}::Deserializer<'de> {{"
    );

    match adt.kind {
        AdtKind::Struct => {
            let v = &adt.variants[0];
            let fs = &fattrs[0];
            if cattrs.transparent {
                let f = &v.fields[0];
                let _ = writeln!(
                    out,
                    "        let __v = <{} as {sp}::Deserialize<'de>>::deserialize(__deserializer)?;",
                    f.ty
                );
                match v.shape {
                    FieldsShape::Named => {
                        let fname = f.name.as_deref()?;
                        let _ = writeln!(out, "        ::core::result::Result::Ok({name} {{ {fname}: __v }})");
                    }
                    FieldsShape::Tuple => {
                        let _ = writeln!(out, "        ::core::result::Result::Ok({name}(__v))");
                    }
                    FieldsShape::Unit => return None,
                }
            } else {
                match v.shape {
                    FieldsShape::Unit => {
                        let _ = writeln!(out, "        struct __Visitor;");
                        let _ = writeln!(
                            out,
                            "        impl<'de> {sp}::de::Visitor<'de> for __Visitor {{"
                        );
                        let _ = writeln!(out, "            type Value = {name};");
                        let _ = writeln!(
                            out,
                            "            fn expecting(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {{ ::core::fmt::Formatter::write_str(f, {}) }}",
                            quote_str(&format!("unit struct {name}"))
                        );
                        let _ = writeln!(
                            out,
                            "            fn visit_unit<__E>(self) -> ::core::result::Result<Self::Value, __E> where __E: {sp}::de::Error {{ ::core::result::Result::Ok({name}) }}"
                        );
                        let _ = writeln!(out, "        }}");
                        let _ = writeln!(
                            out,
                            "        {sp}::Deserializer::deserialize_unit_struct(__deserializer, {}, __Visitor)",
                            quote_str(container_wire)
                        );
                    }
                    FieldsShape::Tuple => {
                        let active: Vec<usize> = (0..v.fields.len())
                            .filter(|&i| !fs[i].skip_deserializing)
                            .collect();
                        if active.len() == 1 && v.fields.len() == 1 && !fs[0].default && !cattrs.default {
                            if marker_ty.is_empty() {
                                let _ = writeln!(out, "        struct __Visitor;");
                            } else {
                                let _ = writeln!(out, "        struct __Visitor{ty_g}({marker_ty});");
                            }
                            let _ = writeln!(
                                out,
                                "        impl{impl_g} {sp}::de::Visitor<'de> for __Visitor{ty_g}{where_c} {{"
                            );
                            let _ = writeln!(out, "            type Value = {name}{ty_g};");
                            let _ = writeln!(
                                out,
                                "            fn expecting(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {{ ::core::fmt::Formatter::write_str(f, {}) }}",
                                quote_str(&format!("tuple struct {name}"))
                            );
                            let _ = writeln!(
                                out,
                                "            fn visit_newtype_struct<__E>(self, __e: __E) -> ::core::result::Result<Self::Value, __E::Error> where __E: {sp}::Deserializer<'de> {{ ::core::result::Result::Ok({name}(<{} as {sp}::Deserialize<'de>>::deserialize(__e)?)) }}",
                                v.fields[0].ty
                            );
                            let _ = writeln!(
                                out,
                                "            fn visit_seq<__A>(self, mut __seq: __A) -> ::core::result::Result<Self::Value, __A::Error> where __A: {sp}::de::SeqAccess<'de> {{ let __f0 = match {sp}::de::SeqAccess::next_element::<{}>(&mut __seq)? {{ ::core::option::Option::Some(v) => v, ::core::option::Option::None => return ::core::result::Result::Err({sp}::de::Error::invalid_length(0usize, &\"tuple struct {name} with 1 element\")), }}; ::core::result::Result::Ok({name}(__f0)) }}",
                                v.fields[0].ty
                            );
                            let _ = writeln!(out, "        }}");
                            let vinit = if marker_ty.is_empty() {
                                "__Visitor".to_owned()
                            } else {
                                "__Visitor(::core::marker::PhantomData)".to_owned()
                            };
                            let _ = writeln!(
                                out,
                                "        {sp}::Deserializer::deserialize_newtype_struct(__deserializer, {}, {vinit})",
                                quote_str(container_wire)
                            );
                        } else {
                            if marker_ty.is_empty() {
                                let _ = writeln!(out, "        struct __Visitor;");
                            } else {
                                let _ = writeln!(out, "        struct __Visitor{ty_g}({marker_ty});");
                            }
                            let _ = writeln!(
                                out,
                                "        impl{impl_g} {sp}::de::Visitor<'de> for __Visitor{ty_g}{where_c} {{"
                            );
                            let _ = writeln!(out, "            type Value = {name}{ty_g};");
                            let _ = writeln!(
                                out,
                                "            fn expecting(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {{ ::core::fmt::Formatter::write_str(f, {}) }}",
                                quote_str(&format!("tuple struct {name}"))
                            );
                            let _ = writeln!(
                                out,
                                "            fn visit_seq<__A>(self, mut __seq: __A) -> ::core::result::Result<Self::Value, __A::Error> where __A: {sp}::de::SeqAccess<'de> {{"
                            );
                            let mut elem_idx = 0usize;
                            for (i, f) in v.fields.iter().enumerate() {
                                if fs[i].skip_deserializing {
                                    let _ = writeln!(
                                        out,
                                        "                let __f{i}: {} = ::core::default::Default::default();",
                                        f.ty
                                    );
                                } else if fs[i].default || cattrs.default {
                                    let _ = writeln!(
                                        out,
                                        "                let __f{i}: {} = match {sp}::de::SeqAccess::next_element(&mut __seq)? {{ ::core::option::Option::Some(v) => v, ::core::option::Option::None => ::core::default::Default::default() }};",
                                        f.ty
                                    );
                                    elem_idx += 1;
                                } else {
                                    let _ = writeln!(
                                        out,
                                        "                let __f{i}: {} = match {sp}::de::SeqAccess::next_element(&mut __seq)? {{ ::core::option::Option::Some(v) => v, ::core::option::Option::None => return ::core::result::Result::Err({sp}::de::Error::invalid_length({elem_idx}usize, &\"tuple struct {name}\")) }};",
                                        f.ty
                                    );
                                    elem_idx += 1;
                                }
                            }
                            let args: Vec<String> =
                                (0..v.fields.len()).map(|i| format!("__f{i}")).collect();
                            let _ = writeln!(
                                out,
                                "                ::core::result::Result::Ok({name}({}))",
                                args.join(", ")
                            );
                            let _ = writeln!(out, "            }}");
                            let _ = writeln!(out, "        }}");
                            let vinit = if marker_ty.is_empty() {
                                "__Visitor".to_owned()
                            } else {
                                "__Visitor(::core::marker::PhantomData)".to_owned()
                            };
                            let _ = writeln!(
                                out,
                                "        {sp}::Deserializer::deserialize_tuple_struct(__deserializer, {}, {}, {vinit})",
                                quote_str(container_wire),
                                active.len()
                            );
                        }
                    }
                    FieldsShape::Named => {
                        gen_serde_struct_visitor(
                            &mut out,
                            adt,
                            cattrs,
                            v,
                            fs,
                            container_wire,
                            &impl_g,
                            &ty_g,
                            &where_c,
                            &marker_ty,
                            sp,
                        )?;
                    }
                }
            }
        }
        AdtKind::Enum => {
            if adt.variants.is_empty() {
                return None;
            }
            gen_serde_enum_deserialize(
                &mut out,
                adt,
                cattrs,
                vattrs,
                fattrs,
                container_wire,
                &impl_g,
                &ty_g,
                &where_c,
                &marker_ty,
                sp,
            )?;
        }
    }

    let _ = writeln!(out, "    }}");
    let _ = writeln!(out, "}}");
    Some(out)
}

#[allow(clippy::too_many_arguments)]
fn gen_serde_struct_visitor(
    out: &mut String,
    adt: &ParsedAdt,
    cattrs: &SerdeContainerAttrs,
    v: &ParsedVariant,
    fs: &[SerdeFieldAttrs],
    container_wire: &str,
    impl_g: &str,
    ty_g: &str,
    where_c: &str,
    marker_ty: &str,
    sp: &str,
) -> Option<()> {
    let name = &adt.name;
    let active: Vec<(usize, &str, Cow<'_, str>)> = (0..v.fields.len())
        .filter(|&i| !fs[i].skip_deserializing)
        .map(|i| {
            let raw = v.fields[i].name.as_deref().unwrap_or("");
            let wire = match fs[i].rename.as_deref() {
                Some(r) => Cow::Borrowed(r),
                None => Cow::Owned(cattrs.rename_all.apply(raw)),
            };
            (i, raw, wire)
        })
        .collect();

    // Field enum
    let _ = writeln!(out, "        #[allow(non_camel_case_types)] enum __Field {{");
    for (idx, _, _) in &active {
        let _ = writeln!(out, "            __f{idx},");
    }
    if !cattrs.deny_unknown_fields {
        let _ = writeln!(out, "            __ignore,");
    }
    let _ = writeln!(out, "        }}");

    let field_names_list: Vec<String> = active.iter().map(|(_, _, w)| quote_str(w)).collect();
    let _ = writeln!(
        out,
        "        const __FIELDS: &'static [&'static str] = &[{}];",
        field_names_list.join(", ")
    );

    // __Field Deserialize
    let _ = writeln!(
        out,
        "        impl<'de> {sp}::Deserialize<'de> for __Field {{ fn deserialize<__D>(__d: __D) -> ::core::result::Result<Self, __D::Error> where __D: {sp}::Deserializer<'de> {{"
    );
    let _ = writeln!(out, "            struct __FieldVisitor;");
    let _ = writeln!(
        out,
        "            impl<'de> {sp}::de::Visitor<'de> for __FieldVisitor {{ type Value = __Field; fn expecting(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {{ ::core::fmt::Formatter::write_str(f, \"field identifier\") }}"
    );
    let _ = writeln!(
        out,
        "                fn visit_str<__E>(self, __v: &str) -> ::core::result::Result<Self::Value, __E> where __E: {sp}::de::Error {{ match __v {{"
    );
    for (idx, _, wire) in &active {
        let _ = writeln!(
            out,
            "                    {} => ::core::result::Result::Ok(__Field::__f{idx}),",
            quote_str(wire)
        );
    }
    if cattrs.deny_unknown_fields {
        let _ = writeln!(
            out,
            "                    _ => ::core::result::Result::Err({sp}::de::Error::unknown_field(__v, __FIELDS)),"
        );
    } else {
        let _ = writeln!(out, "                    _ => ::core::result::Result::Ok(__Field::__ignore),");
    }
    let _ = writeln!(out, "                }} }}");
    let _ = writeln!(out, "            }}");
    let _ = writeln!(
        out,
        "            {sp}::Deserializer::deserialize_identifier(__d, __FieldVisitor)"
    );
    let _ = writeln!(out, "        }} }}");

    // Struct Visitor
    if marker_ty.is_empty() {
        let _ = writeln!(out, "        struct __Visitor;");
    } else {
        let _ = writeln!(out, "        struct __Visitor{ty_g}({marker_ty});");
    }
    let _ = writeln!(
        out,
        "        impl{impl_g} {sp}::de::Visitor<'de> for __Visitor{ty_g}{where_c} {{"
    );
    let _ = writeln!(out, "            type Value = {name}{ty_g};");
    let _ = writeln!(
        out,
        "            fn expecting(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {{ ::core::fmt::Formatter::write_str(f, {}) }}",
        quote_str(&format!("struct {name}"))
    );

    // visit_seq
    let _ = writeln!(
        out,
        "            fn visit_seq<__A>(self, mut __seq: __A) -> ::core::result::Result<Self::Value, __A::Error> where __A: {sp}::de::SeqAccess<'de> {{"
    );
    let mut seq_i = 0usize;
    for (i, f) in v.fields.iter().enumerate() {
        if fs[i].skip_deserializing {
            let _ = writeln!(
                out,
                "                let __f{i}: {} = ::core::default::Default::default();",
                f.ty
            );
        } else if fs[i].default || cattrs.default {
            let _ = writeln!(
                out,
                "                let __f{i}: {} = match {sp}::de::SeqAccess::next_element(&mut __seq)? {{ ::core::option::Option::Some(v) => v, ::core::option::Option::None => ::core::default::Default::default() }};",
                f.ty
            );
            seq_i += 1;
        } else {
            let _ = writeln!(
                out,
                "                let __f{i}: {} = match {sp}::de::SeqAccess::next_element(&mut __seq)? {{ ::core::option::Option::Some(v) => v, ::core::option::Option::None => return ::core::result::Result::Err({sp}::de::Error::invalid_length({seq_i}usize, &\"struct {name}\")) }};",
                f.ty
            );
            seq_i += 1;
        }
    }
    let ctor_fields: Vec<String> = v
        .fields
        .iter()
        .enumerate()
        .map(|(i, f)| format!("{}: __f{i}", f.name.as_deref().unwrap_or("")))
        .collect();
    let _ = writeln!(
        out,
        "                ::core::result::Result::Ok({name} {{ {} }})",
        ctor_fields.join(", ")
    );
    let _ = writeln!(out, "            }}");

    // visit_map
    let _ = writeln!(
        out,
        "            fn visit_map<__A>(self, mut __map: __A) -> ::core::result::Result<Self::Value, __A::Error> where __A: {sp}::de::MapAccess<'de> {{"
    );
    for (idx, _, _) in &active {
        let _ = writeln!(
            out,
            "                let mut __f{idx}: ::core::option::Option<{}> = ::core::option::Option::None;",
            v.fields[*idx].ty
        );
    }
    let _ = writeln!(
        out,
        "                while let ::core::option::Option::Some(__key) = {sp}::de::MapAccess::next_key::<__Field>(&mut __map)? {{"
    );
    let _ = writeln!(out, "                    match __key {{");
    for (idx, _, wire) in &active {
        let _ = writeln!(
            out,
            "                        __Field::__f{idx} => {{ if ::core::option::Option::is_some(&__f{idx}) {{ return ::core::result::Result::Err({sp}::de::Error::duplicate_field({})); }} __f{idx} = ::core::option::Option::Some({sp}::de::MapAccess::next_value(&mut __map)?); }}",
            quote_str(wire)
        );
    }
    if !cattrs.deny_unknown_fields {
        let _ = writeln!(
            out,
            "                        __Field::__ignore => {{ let _ = {sp}::de::MapAccess::next_value::<{sp}::de::IgnoredAny>(&mut __map)?; }}"
        );
    }
    let _ = writeln!(out, "                    }}");
    let _ = writeln!(out, "                }}");
    if cattrs.default {
        let _ = writeln!(
            out,
            "                let __default: Self::Value = ::core::default::Default::default();"
        );
    }
    for (i, f) in v.fields.iter().enumerate() {
        let raw = f.name.as_deref()?;
        if fs[i].skip_deserializing {
            let _ = writeln!(
                out,
                "                let __f{i}: {} = ::core::default::Default::default();",
                f.ty
            );
        } else if fs[i].default {
            let _ = writeln!(
                out,
                "                let __f{i} = match __f{i} {{ ::core::option::Option::Some(v) => v, ::core::option::Option::None => ::core::default::Default::default() }};"
            );
        } else if cattrs.default {
            let _ = writeln!(
                out,
                "                let __f{i} = match __f{i} {{ ::core::option::Option::Some(v) => v, ::core::option::Option::None => __default.{raw} }};"
            );
        } else {
            let wire = match fs[i].rename.as_deref() {
                Some(r) => Cow::Borrowed(r),
                None => Cow::Owned(cattrs.rename_all.apply(raw)),
            };
            let _ = writeln!(
                out,
                "                let __f{i} = match __f{i} {{ ::core::option::Option::Some(v) => v, ::core::option::Option::None => return ::core::result::Result::Err({sp}::de::Error::missing_field({})) }};",
                quote_str(&wire)
            );
        }
    }
    let _ = writeln!(
        out,
        "                ::core::result::Result::Ok({name} {{ {} }})",
        ctor_fields.join(", ")
    );
    let _ = writeln!(out, "            }}");
    let _ = writeln!(out, "        }}");

    let vinit = if marker_ty.is_empty() {
        "__Visitor".to_owned()
    } else {
        "__Visitor(::core::marker::PhantomData)".to_owned()
    };
    let _ = writeln!(
        out,
        "        {sp}::Deserializer::deserialize_struct(__deserializer, {}, __FIELDS, {vinit})",
        quote_str(container_wire)
    );
    Some(())
}

#[allow(clippy::too_many_arguments)]
fn gen_serde_enum_deserialize(
    out: &mut String,
    adt: &ParsedAdt,
    cattrs: &SerdeContainerAttrs,
    vattrs: &[SerdeFieldAttrs],
    fattrs: &[Vec<SerdeFieldAttrs>],
    container_wire: &str,
    impl_g: &str,
    ty_g: &str,
    where_c: &str,
    marker_ty: &str,
    sp: &str,
) -> Option<()> {
    // Only handle externally tagged enums without skipped/defaulted struct-variant complexity on first pass.
    if cattrs.default {
        return None;
    }
    let name = &adt.name;
    let active_vars: Vec<(usize, Cow<'_, str>)> = (0..adt.variants.len())
        .filter(|&i| !vattrs[i].skip_deserializing)
        .map(|i| {
            let wire = match vattrs[i].rename.as_deref() {
                Some(r) => Cow::Borrowed(r),
                None => Cow::Owned(cattrs.rename_all.apply(&adt.variants[i].name)),
            };
            (i, wire)
        })
        .collect();

    // Only support unit and single-field newtype variants for Deserialize in fast-path; complex
    // multi-field enum variants fall back to real serde_derive cleanly.
    for &(vi, _) in &active_vars {
        let v = &adt.variants[vi];
        match v.shape {
            FieldsShape::Unit => {}
            FieldsShape::Tuple if v.fields.len() == 1 && !fattrs[vi][0].skip_deserializing => {}
            _ => return None,
        }
    }

    let var_names: Vec<String> = active_vars.iter().map(|(_, w)| quote_str(w)).collect();
    let _ = writeln!(
        out,
        "        const __VARIANTS: &'static [&'static str] = &[{}];",
        var_names.join(", ")
    );
    let _ = writeln!(out, "        #[allow(non_camel_case_types)] enum __Field {{");
    for (vi, _) in &active_vars {
        let _ = writeln!(out, "            __v{vi},");
    }
    let _ = writeln!(out, "        }}");
    let _ = writeln!(
        out,
        "        impl<'de> {sp}::Deserialize<'de> for __Field {{ fn deserialize<__D>(__d: __D) -> ::core::result::Result<Self, __D::Error> where __D: {sp}::Deserializer<'de> {{"
    );
    let _ = writeln!(out, "            struct __FieldVisitor;");
    let _ = writeln!(
        out,
        "            impl<'de> {sp}::de::Visitor<'de> for __FieldVisitor {{ type Value = __Field; fn expecting(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {{ ::core::fmt::Formatter::write_str(f, \"variant identifier\") }}"
    );
    let _ = writeln!(
        out,
        "                fn visit_str<__E>(self, __v: &str) -> ::core::result::Result<Self::Value, __E> where __E: {sp}::de::Error {{ match __v {{"
    );
    for (vi, wire) in &active_vars {
        let _ = writeln!(
            out,
            "                    {} => ::core::result::Result::Ok(__Field::__v{vi}),",
            quote_str(wire)
        );
    }
    let _ = writeln!(
        out,
        "                    _ => ::core::result::Result::Err({sp}::de::Error::unknown_variant(__v, __VARIANTS)),"
    );
    let _ = writeln!(out, "                }} }}");
    let _ = writeln!(out, "            }}");
    let _ = writeln!(
        out,
        "            {sp}::Deserializer::deserialize_identifier(__d, __FieldVisitor)"
    );
    let _ = writeln!(out, "        }} }}");

    if marker_ty.is_empty() {
        let _ = writeln!(out, "        struct __Visitor;");
    } else {
        let _ = writeln!(out, "        struct __Visitor{ty_g}({marker_ty});");
    }
    let _ = writeln!(
        out,
        "        impl{impl_g} {sp}::de::Visitor<'de> for __Visitor{ty_g}{where_c} {{"
    );
    let _ = writeln!(out, "            type Value = {name}{ty_g};");
    let _ = writeln!(
        out,
        "            fn expecting(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {{ ::core::fmt::Formatter::write_str(f, {}) }}",
        quote_str(&format!("enum {name}"))
    );
    let _ = writeln!(
        out,
        "            fn visit_enum<__A>(self, __data: __A) -> ::core::result::Result<Self::Value, __A::Error> where __A: {sp}::de::EnumAccess<'de> {{"
    );
    let _ = writeln!(
        out,
        "                match {sp}::de::EnumAccess::variant::<__Field>(__data)? {{"
    );
    for &(vi, _) in &active_vars {
        let v = &adt.variants[vi];
        let vname = &v.name;
        match v.shape {
            FieldsShape::Unit => {
                let _ = writeln!(
                    out,
                    "                    (__Field::__v{vi}, __variant) => {{ {sp}::de::VariantAccess::unit_variant(__variant)?; ::core::result::Result::Ok({name}::{vname}) }}"
                );
            }
            FieldsShape::Tuple => {
                let _ = writeln!(
                    out,
                    "                    (__Field::__v{vi}, __variant) => ::core::result::Result::Ok({name}::{vname}({sp}::de::VariantAccess::newtype_variant::<{}>(__variant)?)),",
                    v.fields[0].ty
                );
            }
            FieldsShape::Named => return None,
        }
    }
    let _ = writeln!(out, "                }}");
    let _ = writeln!(out, "            }}");
    let _ = writeln!(out, "        }}");
    let vinit = if marker_ty.is_empty() {
        "__Visitor".to_owned()
    } else {
        "__Visitor(::core::marker::PhantomData)".to_owned()
    };
    let _ = writeln!(
        out,
        "        {sp}::Deserializer::deserialize_enum(__deserializer, {}, __VARIANTS, {vinit})",
        quote_str(container_wire)
    );
    Some(())
}

// =================================================================================================
// 2. `bytemuck_derive` Comptime Patch (`Zeroable`, `Pod`, `NoUninit`, `AnyBitPattern`, `TransparentWrapper`)
// =================================================================================================

fn expand_bytemuck(stream: &TokenStream<'_>, macro_name: &str) -> Option<String> {
    let adt = parse_adt(stream)?;
    if adt.kind != AdtKind::Struct || adt.has_cfg_on_members() {
        return None;
    }
    // Any custom `#[bytemuck(...)]` attribute -> fallback to real bytemuck_derive.
    if adt.attrs.iter().any(|a| a.path == "bytemuck")
        || adt.variants[0].fields.iter().any(|f| f.attrs.iter().any(|a| a.path == "bytemuck"))
    {
        return None;
    }
    let name = &adt.name;
    let fields = &adt.variants[0].fields;
    let impl_g = adt.impl_generics(None);
    let ty_g = adt.ty_generics();

    match macro_name {
        "Zeroable" => {
            let where_c = adt.where_clause_with_bounds(&["::bytemuck::Zeroable"]);
            let mut checks = String::new();
            for f in fields {
                let _ = writeln!(
                    checks,
                    "        fn __assert_z<T: ::bytemuck::Zeroable>() {{}} __assert_z::<{}>();",
                    f.ty
                );
            }
            Some(format!(
                "unsafe impl{impl_g} ::bytemuck::Zeroable for {name}{ty_g}{where_c} {{}}\nconst _: () = {{ fn __check{impl_g}(){where_c} {{\n{checks}    }} }};"
            ))
        }
        "Pod" | "NoUninit" | "AnyBitPattern" => {
            // Require `#[repr(C)]` or `#[repr(transparent)]` (non-packed check for Pod/NoUninit).
            let has_repr = adt.attrs.iter().any(|a| {
                a.path == "repr"
                    && a.args
                        .as_deref()
                        .is_some_and(|s| s.contains('C') || s.contains("transparent"))
            });
            if !has_repr {
                return None;
            }
            let trait_path = format!("::bytemuck::{macro_name}");
            let where_c = adt.where_clause_with_bounds(&[&trait_path]);
            let mut field_sum = String::from("0usize");
            let mut checks = String::new();
            for f in fields {
                let _ = write!(field_sum, " + ::core::mem::size_of::<{}>()", f.ty);
                let _ = writeln!(
                    checks,
                    "        fn __assert_t<T: {trait_path}>() {{}} __assert_t::<{}>();",
                    f.ty
                );
            }
            let pad_assert = if macro_name == "Pod" || macro_name == "NoUninit" {
                if adt.generics.is_empty() {
                    format!(
                        "const _: () = assert!(::core::mem::size_of::<{name}>() == ({field_sum}), \"bytemuck: struct has padding\");\n"
                    )
                } else {
                    String::new()
                }
            } else {
                String::new()
            };
            Some(format!(
                "{pad_assert}unsafe impl{impl_g} {trait_path} for {name}{ty_g}{where_c} {{}}\nconst _: () = {{ fn __check{impl_g}(){where_c} {{\n{checks}    }} }};"
            ))
        }
        "TransparentWrapper" => {
            if fields.len() != 1 {
                return None;
            }
            let has_transparent = adt.attrs.iter().any(|a| {
                a.path == "repr" && a.args.as_deref().is_some_and(|s| s.contains("transparent"))
            });
            if !has_transparent {
                return None;
            }
            let inner = &fields[0].ty;
            let where_c = adt.where_clause_with_bounds(&[]);
            Some(format!(
                "unsafe impl{impl_g} ::bytemuck::TransparentWrapper<{inner}> for {name}{ty_g}{where_c} {{}}"
            ))
        }
        _ => None,
    }
}

// =================================================================================================
// 3. `thiserror_impl` Comptime Patch (`#[derive(Error)]`)
// =================================================================================================

fn expand_thiserror(stream: &TokenStream<'_>) -> Option<String> {
    let adt = parse_adt(stream)?;
    if adt.has_cfg_on_members() {
        return None;
    }
    // Generic error ADTs (e.g. `UnbuiltTransactionError<N: Network>`) do not necessarily bound all
    // type parameters with `Debug + Display`; fall back to real `thiserror-impl` for exact bounds.
    if adt.generics.iter().any(|g| matches!(g.kind, GenericParamKind::Type)) {
        return None;
    }
    // `thiserror` v2 auto-calls `.display()` on `Path`/`PathBuf` fields and uses `AsDynError`
    // autoref specialization for non-`Error` report/boxed-dyn wrappers (`eyre::Report`,
    // `anyhow::Error`, `Box<dyn Error>`); fall back to real `thiserror-impl` on those.
    for v in &adt.variants {
        if v.fields.iter().any(|f| {
            f.ty.contains("Path")
                || f.ty.contains("Report")
                || f.ty.contains("eyre")
                || f.ty.contains("anyhow")
                || f.ty.contains("dyn")
        }) {
            return None;
        }
    }
    let name = &adt.name;
    let impl_g = adt.impl_generics(None);
    let ty_g = adt.ty_generics();
    let where_c = adt.where_clause_with_bounds(&[]);

    // Reject `#[backtrace]` or exotic `#[error(fmt = ...)]` -> safe fallback.
    let has_exotic = |attrs: &[RawAttr]| {
        attrs.iter().any(|a| a.path == "backtrace")
    };
    if has_exotic(&adt.attrs) {
        return None;
    }

    let mut display_arms = String::new();
    let mut source_arms = String::new();
    let mut from_impls = String::new();

    match adt.kind {
        AdtKind::Struct => {
            let v = &adt.variants[0];
            let err_attr = adt.attrs.iter().find(|a| a.path == "error")?;
            let (disp_expr, transparent) = parse_thiserror_attr(err_attr, v, true)?;
            let _ = writeln!(display_arms, "        {disp_expr}");

            // Check `#[from]` / `#[source]` fields.
            let mut source_field: Option<(usize, &ParsedField)> = None;
            for (i, f) in v.fields.iter().enumerate() {
                if has_exotic(&f.attrs) {
                    return None;
                }
                let is_from = f.attrs.iter().any(|a| a.path == "from");
                let is_source = f.attrs.iter().any(|a| a.path == "source")
                    || f.name.as_deref() == Some("source");
                if is_from {
                    if v.fields.len() != 1 {
                        return None;
                    }
                    let ctor = match v.shape {
                        FieldsShape::Named => {
                            format!("{name} {{ {}: __source }}", f.name.as_deref()?)
                        }
                        FieldsShape::Tuple => format!("{name}(__source)"),
                        FieldsShape::Unit => return None,
                    };
                    let _ = writeln!(
                        from_impls,
                        "#[automatically_derived] impl{impl_g} ::core::convert::From<{}> for {name}{ty_g}{where_c} {{ fn from(__source: {}) -> Self {{ {ctor} }} }}",
                        f.ty, f.ty
                    );
                    source_field = Some((i, f));
                } else if is_source {
                    source_field = Some((i, f));
                }
            }
            if transparent && v.fields.len() == 1 {
                let acc = match v.shape {
                    FieldsShape::Named => format!("&self.{}", v.fields[0].name.as_deref()?),
                    FieldsShape::Tuple => "&self.0".to_owned(),
                    FieldsShape::Unit => return None,
                };
                let _ = writeln!(
                    source_arms,
                    "        ::core::error::Error::source({acc})"
                );
            } else if let Some((idx, f)) = source_field {
                let acc = match v.shape {
                    FieldsShape::Named => format!("&self.{}", f.name.as_deref()?),
                    FieldsShape::Tuple => format!("&self.{idx}"),
                    FieldsShape::Unit => return None,
                };
                let _ = writeln!(
                    source_arms,
                    "        ::core::option::Option::Some({acc} as &(dyn ::core::error::Error + 'static))"
                );
            } else {
                let _ = writeln!(source_arms, "        ::core::option::Option::None");
            }
        }
        AdtKind::Enum => {
            if adt.variants.is_empty() {
                return None;
            }
            let _ = writeln!(display_arms, "        match self {{");
            let _ = writeln!(source_arms, "        match self {{");
            for v in &adt.variants {
                if has_exotic(&v.attrs) {
                    return None;
                }
                let vname = &v.name;
                let err_attr = v.attrs.iter().find(|a| a.path == "error")?;
                let (disp_body, transparent) = parse_thiserror_attr(err_attr, v, false)?;
                let pat = variant_ref_pattern(name, v);
                let _ = writeln!(display_arms, "            {pat} => {disp_body},");

                let mut source_idx: Option<usize> = None;
                for (i, f) in v.fields.iter().enumerate() {
                    if has_exotic(&f.attrs) {
                        return None;
                    }
                    let is_from = f.attrs.iter().any(|a| a.path == "from");
                    let is_source = f.attrs.iter().any(|a| a.path == "source")
                        || f.name.as_deref() == Some("source");
                    if is_from {
                        if v.fields.len() != 1 {
                            return None;
                        }
                        let ctor = match v.shape {
                            FieldsShape::Named => {
                                format!("{name}::{vname} {{ {}: __source }}", f.name.as_deref()?)
                            }
                            FieldsShape::Tuple => format!("{name}::{vname}(__source)"),
                            FieldsShape::Unit => return None,
                        };
                        let _ = writeln!(
                            from_impls,
                            "#[automatically_derived] impl{impl_g} ::core::convert::From<{}> for {name}{ty_g}{where_c} {{ fn from(__source: {}) -> Self {{ {ctor} }} }}",
                            f.ty, f.ty
                        );
                        source_idx = Some(i);
                    } else if is_source {
                        source_idx = Some(i);
                    }
                }
                if transparent && v.fields.len() == 1 {
                    let _ = writeln!(
                        source_arms,
                        "            {pat} => ::core::error::Error::source(__f0),"
                    );
                } else if let Some(idx) = source_idx {
                    let _ = writeln!(
                        source_arms,
                        "            {pat} => ::core::option::Option::Some(__f{idx} as &(dyn ::core::error::Error + 'static)),"
                    );
                } else {
                    let _ = writeln!(
                        source_arms,
                        "            {pat} => ::core::option::Option::None,"
                    );
                }
            }
            let _ = writeln!(display_arms, "        }}");
            let _ = writeln!(source_arms, "        }}");
        }
    }

    let err_where = adt.where_clause_with_bounds(&["::core::fmt::Debug", "::core::fmt::Display"]);
    let disp_where = adt.where_clause_with_bounds(&["::core::fmt::Display"]);
    Some(format!(
        "#[automatically_derived] impl{impl_g} ::core::fmt::Display for {name}{ty_g}{disp_where} {{\n    fn fmt(&self, __f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {{\n{display_arms}    }}\n}}\n#[automatically_derived] impl{impl_g} ::core::error::Error for {name}{ty_g}{err_where} {{\n    fn source(&self) -> ::core::option::Option<&(dyn ::core::error::Error + 'static)> {{\n{source_arms}    }}\n}}\n{from_impls}"
    ))
}

fn variant_ref_pattern(enum_name: &str, v: &ParsedVariant) -> String {
    let vname = &v.name;
    match v.shape {
        FieldsShape::Unit => format!("{enum_name}::{vname}"),
        FieldsShape::Tuple => {
            let binds: Vec<String> = (0..v.fields.len()).map(|i| format!("__f{i}")).collect();
            format!("{enum_name}::{vname}({})", binds.join(", "))
        }
        FieldsShape::Named => {
            let binds: Vec<String> = v
                .fields
                .iter()
                .enumerate()
                .map(|(i, f)| format!("{}: __f{i}", f.name.as_deref().unwrap_or("")))
                .collect();
            format!("{enum_name}::{vname} {{ {} }}", binds.join(", "))
        }
    }
}

/// Parse `#[error("...")]` or `#[error(transparent)]` into a Rust Display expression.
/// Falls back on extra format args (`#[error("...", x)]`) or format specs (`{0:?}`).
fn parse_thiserror_attr(
    attr: &RawAttr,
    v: &ParsedVariant,
    is_struct: bool,
) -> Option<(String, bool)> {
    let args = attr.args.as_deref()?.trim();
    if args == "transparent" {
        if v.fields.len() != 1 {
            return None;
        }
        let target = if is_struct {
            match v.shape {
                FieldsShape::Named => format!("&self.{}", v.fields[0].name.as_deref()?),
                FieldsShape::Tuple => "&self.0".to_owned(),
                FieldsShape::Unit => return None,
            }
        } else {
            "__f0".to_owned()
        };
        return Some((format!("::core::fmt::Display::fmt({target}, __f)"), true));
    }
    // Single string literal with optional `{0}` or `{field_name}` placeholders (no extra args).
    let fmt_str = unquote_str(args)?;
    let mut new_fmt = String::new();
    let mut fmt_args: Vec<String> = Vec::new();
    let bytes = fmt_str.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            if i + 1 < bytes.len() && bytes[i + 1] == b'{' {
                new_fmt.push_str("{{");
                i += 2;
                continue;
            }
            let close = fmt_str[i + 1..].find('}')? + i + 1;
            let key = &fmt_str[i + 1..close];
            // Any format spec (`:` like `{0:?}` or `{x:#}`) -> safe fallback!
            if key.is_empty() || key.contains(':') || key.contains('.') {
                return None;
            }
            let expr = if let Ok(idx) = key.parse::<usize>() {
                if idx >= v.fields.len() {
                    return None;
                }
                if is_struct { format!("self.{idx}") } else { format!("__f{idx}") }
            } else {
                let pos = v.fields.iter().position(|f| f.name.as_deref() == Some(key))?;
                if is_struct { format!("self.{key}") } else { format!("__f{pos}") }
            };
            new_fmt.push_str("{}");
            fmt_args.push(expr);
            i = close + 1;
        } else if bytes[i] == b'}' {
            if i + 1 < bytes.len() && bytes[i + 1] == b'}' {
                new_fmt.push_str("}}");
                i += 2;
                continue;
            }
            return None;
        } else {
            let ch = fmt_str[i..].chars().next()?;
            new_fmt.push(ch);
            i += ch.len_utf8();
        }
    }
    if fmt_args.is_empty() {
        Some((
            format!("::core::fmt::Formatter::write_str(__f, {})", quote_str(&new_fmt)),
            false,
        ))
    } else {
        Some((
            format!(
                "::core::write!(__f, {}, {})",
                quote_str(&new_fmt),
                fmt_args.join(", ")
            ),
            false,
        ))
    }
}

// =================================================================================================
// 4. `derive_more` / `derive_more_impl` Comptime Patch
// =================================================================================================

fn has_derive_more_attr(attrs: &[RawAttr]) -> bool {
    attrs.iter().any(|a| {
        matches!(
            a.path.as_str(),
            "from" | "into" | "deref" | "deref_mut" | "as_ref" | "as_mut" | "display" | "is_variant" | "constructor"
        )
    })
}

fn expand_derive_more(stream: &TokenStream<'_>, macro_name: &str) -> Option<String> {
    let adt = parse_adt(stream)?;
    if adt.has_cfg_on_members() || has_derive_more_attr(&adt.attrs) {
        return None;
    }
    for v in &adt.variants {
        if has_derive_more_attr(&v.attrs)
            || v.fields.iter().any(|f| has_derive_more_attr(&f.attrs))
        {
            return None;
        }
    }
    let name = &adt.name;
    let impl_g = adt.impl_generics(None);
    let ty_g = adt.ty_generics();
    let where_c = adt.where_clause_with_bounds(&[]);

    match macro_name {
        "From" => match adt.kind {
            AdtKind::Struct => {
                let v = &adt.variants[0];
                if v.fields.len() != 1 {
                    return None;
                }
                let f = &v.fields[0];
                let body = match v.shape {
                    FieldsShape::Tuple => format!("{name}(__v)"),
                    FieldsShape::Named => format!("{name} {{ {}: __v }}", f.name.as_deref()?),
                    FieldsShape::Unit => return None,
                };
                Some(format!(
                    "#[automatically_derived] impl{impl_g} ::core::convert::From<{}> for {name}{ty_g}{where_c} {{ #[inline] fn from(__v: {}) -> Self {{ {body} }} }}",
                    f.ty, f.ty
                ))
            }
            AdtKind::Enum => {
                // Only handle enums where every variant has 1 distinct field type.
                let mut seen_tys: Vec<&str> = Vec::new();
                let mut out = String::new();
                for v in &adt.variants {
                    if v.fields.len() != 1 {
                        return None;
                    }
                    let ty = v.fields[0].ty.as_str();
                    if seen_tys.contains(&ty) {
                        return None;
                    }
                    seen_tys.push(ty);
                    let vname = &v.name;
                    let body = match v.shape {
                        FieldsShape::Tuple => format!("{name}::{vname}(__v)"),
                        FieldsShape::Named => {
                            format!("{name}::{vname} {{ {}: __v }}", v.fields[0].name.as_deref()?)
                        }
                        FieldsShape::Unit => return None,
                    };
                    let _ = writeln!(
                        out,
                        "#[automatically_derived] impl{impl_g} ::core::convert::From<{ty}> for {name}{ty_g}{where_c} {{ #[inline] fn from(__v: {ty}) -> Self {{ {body} }} }}"
                    );
                }
                Some(out)
            }
        },
        "Into" => {
            if adt.kind != AdtKind::Struct || adt.variants[0].fields.len() != 1 {
                return None;
            }
            let v = &adt.variants[0];
            let f = &v.fields[0];
            let acc = match v.shape {
                FieldsShape::Tuple => "__v.0".to_owned(),
                FieldsShape::Named => format!("__v.{}", f.name.as_deref()?),
                FieldsShape::Unit => return None,
            };
            Some(format!(
                "#[automatically_derived] impl{impl_g} ::core::convert::From<{name}{ty_g}> for {}{where_c} {{ #[inline] fn from(__v: {name}{ty_g}) -> Self {{ {acc} }} }}",
                f.ty
            ))
        }
        "Deref" => {
            if adt.kind != AdtKind::Struct || adt.variants[0].fields.len() != 1 {
                return None;
            }
            let v = &adt.variants[0];
            let f = &v.fields[0];
            let acc = match v.shape {
                FieldsShape::Tuple => "&self.0".to_owned(),
                FieldsShape::Named => format!("&self.{}", f.name.as_deref()?),
                FieldsShape::Unit => return None,
            };
            Some(format!(
                "#[automatically_derived] impl{impl_g} ::core::ops::Deref for {name}{ty_g}{where_c} {{ type Target = {}; #[inline] fn deref(&self) -> &Self::Target {{ {acc} }} }}",
                f.ty
            ))
        }
        "DerefMut" => {
            if adt.kind != AdtKind::Struct || adt.variants[0].fields.len() != 1 {
                return None;
            }
            let v = &adt.variants[0];
            let acc = match v.shape {
                FieldsShape::Tuple => "&mut self.0".to_owned(),
                FieldsShape::Named => format!("&mut self.{}", v.fields[0].name.as_deref()?),
                FieldsShape::Unit => return None,
            };
            Some(format!(
                "#[automatically_derived] impl{impl_g} ::core::ops::DerefMut for {name}{ty_g}{where_c} {{ #[inline] fn deref_mut(&mut self) -> &mut Self::Target {{ {acc} }} }}"
            ))
        }
        "AsRef" => {
            if adt.kind != AdtKind::Struct || adt.variants[0].fields.len() != 1 {
                return None;
            }
            let v = &adt.variants[0];
            let f = &v.fields[0];
            let acc = match v.shape {
                FieldsShape::Tuple => "&self.0".to_owned(),
                FieldsShape::Named => format!("&self.{}", f.name.as_deref()?),
                FieldsShape::Unit => return None,
            };
            Some(format!(
                "#[automatically_derived] impl{impl_g} ::core::convert::AsRef<{}> for {name}{ty_g}{where_c} {{ #[inline] fn as_ref(&self) -> &{} {{ {acc} }} }}",
                f.ty, f.ty
            ))
        }
        "AsMut" => {
            if adt.kind != AdtKind::Struct || adt.variants[0].fields.len() != 1 {
                return None;
            }
            let v = &adt.variants[0];
            let f = &v.fields[0];
            let acc = match v.shape {
                FieldsShape::Tuple => "&mut self.0".to_owned(),
                FieldsShape::Named => format!("&mut self.{}", f.name.as_deref()?),
                FieldsShape::Unit => return None,
            };
            Some(format!(
                "#[automatically_derived] impl{impl_g} ::core::convert::AsMut<{}> for {name}{ty_g}{where_c} {{ #[inline] fn as_mut(&mut self) -> &mut {} {{ {acc} }} }}",
                f.ty, f.ty
            ))
        }
        "Constructor" => {
            if adt.kind != AdtKind::Struct {
                return None;
            }
            let v = &adt.variants[0];
            let vis = if adt.vis.is_empty() { "" } else { &adt.vis };
            let params: Vec<String> = v
                .fields
                .iter()
                .enumerate()
                .map(|(i, f)| match f.name.as_deref() {
                    Some(pname) => format!("{pname}: {}", f.ty),
                    None => format!("__f{i}: {}", f.ty),
                })
                .collect();
            let body: Cow<'_, str> = match v.shape {
                FieldsShape::Unit => Cow::Borrowed(name.as_str()),
                FieldsShape::Tuple => {
                    let mut body = format!("{name}(");
                    for i in 0..v.fields.len() {
                        if i > 0 {
                            body.push_str(", ");
                        }
                        let _ = write!(body, "__f{i}");
                    }
                    body.push(')');
                    Cow::Owned(body)
                }
                FieldsShape::Named => {
                    let mut body = format!("{name} {{ ");
                    for (i, f) in v.fields.iter().enumerate() {
                        if i > 0 {
                            body.push_str(", ");
                        }
                        body.push_str(f.name.as_deref().unwrap_or(""));
                    }
                    body.push_str(" }");
                    Cow::Owned(body)
                }
            };
            Some(format!(
                "#[automatically_derived] impl{impl_g} {name}{ty_g}{where_c} {{ #[inline] {vis} const fn new({}) -> Self {{ {body} }} }}",
                params.join(", ")
            ))
        }
        "Display" => {
            let disp_where = adt.where_clause_with_bounds(&["::core::fmt::Display"]);
            match adt.kind {
                AdtKind::Struct if adt.variants[0].fields.len() == 1 => {
                    let v = &adt.variants[0];
                    let acc = match v.shape {
                        FieldsShape::Tuple => "&self.0".to_owned(),
                        FieldsShape::Named => format!("&self.{}", v.fields[0].name.as_deref()?),
                        FieldsShape::Unit => return None,
                    };
                    Some(format!(
                        "#[automatically_derived] impl{impl_g} ::core::fmt::Display for {name}{ty_g}{disp_where} {{ fn fmt(&self, __f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {{ ::core::fmt::Display::fmt({acc}, __f) }} }}"
                    ))
                }
                AdtKind::Enum if adt.variants.iter().all(|v| v.shape == FieldsShape::Unit) => {
                    let mut arms = String::new();
                    for v in &adt.variants {
                        let _ = writeln!(
                            arms,
                            "            {name}::{} => ::core::fmt::Formatter::write_str(__f, {}),",
                            v.name,
                            quote_str(&v.name)
                        );
                    }
                    Some(format!(
                        "#[automatically_derived] impl{impl_g} ::core::fmt::Display for {name}{ty_g}{where_c} {{ fn fmt(&self, __f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {{ match self {{\n{arms}        }} }} }}"
                    ))
                }
                _ => None,
            }
        }
        "IsVariant" => {
            if adt.kind != AdtKind::Enum {
                return None;
            }
            let vis = if adt.vis.is_empty() { "" } else { &adt.vis };
            let mut methods = String::new();
            for v in &adt.variants {
                let fn_name = format!("is_{}", to_snake_case(&v.name));
                let pat = match v.shape {
                    FieldsShape::Unit => format!("{name}::{}", v.name),
                    FieldsShape::Tuple => format!("{name}::{}(..)", v.name),
                    FieldsShape::Named => format!("{name}::{} {{ .. }}", v.name),
                };
                let _ = writeln!(
                    methods,
                    "    #[inline] {vis} const fn {fn_name}(&self) -> bool {{ ::core::matches!(self, {pat}) }}"
                );
            }
            Some(format!(
                "#[automatically_derived] impl{impl_g} {name}{ty_g}{where_c} {{\n{methods}}}"
            ))
        }
        _ => None,
    }
}

// =================================================================================================
// 5. `strum_macros` Comptime Patch (`AsRefStr`, `IntoStaticStr`, `EnumCount`, `Display`)
// =================================================================================================

fn parse_strum_container_rule(attrs: &[RawAttr]) -> Option<RenameRule> {
    let mut rule = RenameRule::None;
    for a in attrs {
        if a.path != "strum" {
            continue;
        }
        let args = a.args.as_deref()?;
        for item in split_meta_items(args) {
            if let Some((k, v)) = item.split_once('=') {
                if k.trim() == "serialize_all" {
                    rule = RenameRule::parse(&unquote_str(v)?)?;
                } else {
                    return None;
                }
            } else {
                return None;
            }
        }
    }
    Some(rule)
}

fn parse_strum_variant_name(attrs: &[RawAttr], fallback_name: &str, rule: RenameRule) -> Option<String> {
    let mut custom = None;
    for a in attrs {
        if a.path != "strum" {
            continue;
        }
        let args = a.args.as_deref()?;
        for item in split_meta_items(args) {
            if let Some((k, v)) = item.split_once('=') {
                if k.trim() == "serialize" || k.trim() == "to_string" {
                    custom = Some(unquote_str(v)?);
                } else {
                    return None;
                }
            } else {
                return None;
            }
        }
    }
    Some(custom.unwrap_or_else(|| rule.apply(fallback_name)))
}

fn expand_strum(stream: &TokenStream<'_>, macro_name: &str) -> Option<String> {
    let adt = parse_adt(stream)?;
    if adt.kind != AdtKind::Enum || adt.has_cfg_on_members() {
        return None;
    }
    let rule = parse_strum_container_rule(&adt.attrs)?;
    let name = &adt.name;
    let impl_g = adt.impl_generics(None);
    let ty_g = adt.ty_generics();
    let where_c = adt.where_clause_with_bounds(&[]);

    if macro_name == "EnumCount" {
        let count = adt.variants.len();
        return Some(format!(
            "#[automatically_derived] impl{impl_g} ::strum::EnumCount for {name}{ty_g}{where_c} {{ const COUNT: usize = {count}usize; }}"
        ));
    }

    let mut arms = String::new();
    for v in &adt.variants {
        let s = parse_strum_variant_name(&v.attrs, &v.name, rule)?;
        let pat = match v.shape {
            FieldsShape::Unit => format!("{name}::{}", v.name),
            FieldsShape::Tuple => format!("{name}::{}(..)", v.name),
            FieldsShape::Named => format!("{name}::{} {{ .. }}", v.name),
        };
        let _ = writeln!(arms, "            {pat} => {},", quote_str(&s));
    }

    match macro_name {
        "AsRefStr" => Some(format!(
            "#[automatically_derived] impl{impl_g} ::core::convert::AsRef<str> for {name}{ty_g}{where_c} {{ fn as_ref(&self) -> &'static str {{ match self {{\n{arms}        }} }} }}"
        )),
        "IntoStaticStr" => Some(format!(
            "#[automatically_derived] impl{impl_g} ::core::convert::From<{name}{ty_g}> for &'static str{where_c} {{ fn from(__v: {name}{ty_g}) -> &'static str {{ match __v {{\n{arms}        }} }} }}\n#[automatically_derived] impl{impl_g} ::core::convert::From<&{name}{ty_g}> for &'static str{where_c} {{ fn from(__v: &{name}{ty_g}) -> &'static str {{ match __v {{\n{arms}        }} }} }}"
        )),
        "Display" => Some(format!(
            "#[automatically_derived] impl{impl_g} ::core::fmt::Display for {name}{ty_g}{where_c} {{ fn fmt(&self, __f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {{ ::core::fmt::Formatter::write_str(__f, match self {{\n{arms}        }}) }} }}"
        )),
        _ => None,
    }
}

// =================================================================================================
// 6. `paste` Comptime Patch (`paste! { ... }`)
// =================================================================================================

fn expand_paste(stream: &TokenStream<'_>) -> Option<String> {
    let tokens = stream.tokens();
    // Bail to the real `paste` proc macro if the input contains `$crate` or invisible
    // macro_rules! fragment delimiters, where preserving macro hygiene spans is required.
    for i in 0..tokens.len() {
        match tokens[i].kind {
            TokenKind::Open(Delim::Invisible) | TokenKind::Close(Delim::Invisible) => return None,
            TokenKind::Ident { .. } if stream.text_at(i) == "$crate" => return None,
            TokenKind::Punct { char: '$', .. }
                if i + 1 < tokens.len()
                    && matches!(tokens[i + 1].kind, TokenKind::Ident { .. })
                    && stream.text_at(i + 1) == "crate" =>
            {
                return None;
            }
            _ => {}
        }
    }
    let mut out = String::new();
    expand_paste_range(stream, 0, tokens.len(), &mut out)?;
    Some(out)
}

fn expand_paste_range(stream: &TokenStream<'_>, start: usize, end: usize, out: &mut String) -> Option<()> {
    let tokens = stream.tokens();
    let mut i = start;
    let mut prev_joint = false;
    while i < end {
        match tokens[i].kind {
            TokenKind::Open(Delim::Bracket) => {
                let close = stream.close_of(i)?;
                // Check for paste group `[< ... >]`.
                if i + 2 < close && is_punct(stream, i + 1, '<') && is_punct(stream, close - 1, '>') {
                    let pasted = eval_paste_group(stream, i + 2, close - 1)?;
                    if !out.is_empty()
                        && !prev_joint
                        && !out.ends_with(['(', '[', '{', '<', ':', '\'', '.', '&', '#', ' '])
                    {
                        out.push(' ');
                    }
                    out.push_str(&pasted);
                    prev_joint = false;
                    i = close + 1;
                    continue;
                }
                out.push('[');
                expand_paste_range(stream, i + 1, close, out)?;
                out.push(']');
                prev_joint = false;
                i = close + 1;
            }
            TokenKind::Open(d) => {
                let close = stream.close_of(i)?;
                let (o, c) = match d {
                    Delim::Paren => ('(', ')'),
                    Delim::Brace => ('{', '}'),
                    Delim::Bracket => ('[', ']'),
                    Delim::Invisible => return None,
                };
                out.push(o);
                expand_paste_range(stream, i + 1, close, out)?;
                out.push(c);
                prev_joint = false;
                i = close + 1;
            }
            TokenKind::Close(_) => {
                prev_joint = false;
                i += 1;
            }
            TokenKind::Punct { char: ch, spacing } => {
                if !out.is_empty()
                    && !prev_joint
                    && !matches!(ch, ',' | ';' | ':' | '.' | '>' | '!' | '?')
                    && !out.ends_with(['(', '[', '{', ':', '\'', '<', '&', '#', ' '])
                {
                    out.push(' ');
                }
                out.push(ch);
                if spacing == Spacing::Alone && matches!(ch, ',' | ';') {
                    out.push(' ');
                }
                prev_joint = spacing == Spacing::Joint || matches!(ch, '\'' | '#');
                i += 1;
            }
            TokenKind::Ident { .. } => {
                if !out.is_empty()
                    && !prev_joint
                    && !out.ends_with(['(', '[', '{', '<', ':', '\'', '.', '&', '#', ' '])
                {
                    out.push(' ');
                }
                out.push_str(stream.text_at(i));
                prev_joint = false;
                i += 1;
            }
            TokenKind::Literal { kind, suffix_len, .. } => {
                if !out.is_empty()
                    && !prev_joint
                    && !out.ends_with(['(', '[', '{', '<', ':', '\'', '.', '&', '#', ' '])
                {
                    out.push(' ');
                }
                write_literal(out, kind, stream.text_at(i), suffix_len);
                prev_joint = false;
                i += 1;
            }
        }
    }
    Some(())
}

fn eval_paste_group(stream: &TokenStream<'_>, start: usize, end: usize) -> Option<String> {
    let tokens = stream.tokens();
    let mut out = String::new();
    let mut i = start;
    while i < end {
        let mut part = match tokens[i].kind {
            TokenKind::Ident { .. } => stream.text_at(i).strip_prefix("r#").unwrap_or(stream.text_at(i)).to_owned(),
            TokenKind::Literal { kind, .. } => {
                let t = stream.text_at(i);
                match kind {
                    LitKind::Str | LitKind::StrRaw(_) => t.to_owned(),
                    LitKind::Integer => t.to_owned(),
                    _ => return None,
                }
            }
            _ => return None, // exotic paste syntax (like nested groups) -> fallback
        };
        i += 1;
        // Apply `:modifier` chain (`:lower`, `:upper`, `:snake`, `:camel`).
        while i + 1 < end && is_punct(stream, i, ':') && is_any_ident(stream, i + 1) {
            match stream.text_at(i + 1) {
                "lower" => part = part.to_ascii_lowercase(),
                "upper" => part = part.to_ascii_uppercase(),
                "snake" => part = to_snake_case(&part),
                "camel" => part = to_pascal_case(&part),
                _ => return None,
            }
            i += 2;
        }
        out.push_str(&part);
    }
    if out.is_empty() {
        return None;
    }
    Some(out)
}

// =================================================================================================
// Unit Tests (Including Live `rustc` Compilation & Execution Proof!)
// =================================================================================================

#[cfg(test)]
mod tests {
    use blaze_expand::{Edition, ProcCall, ProcKind, lex};
    use blaze_own::testing::heap;

    use super::*;

    fn expand_code(krate: &str, kind: ProcKind, mac: &str, input: &str) -> Option<String> {
        let alloc = heap().alloc();
        let stream = lex(input, Edition::E2021, alloc).expect("test input lexes");
        let call = ProcCall {
            kind,
            input: &stream,
            attr_args: None,
            file: Ok("/workspace/src/lib.rs"),
        };
        match try_expand(krate, kind, mac, &call, Edition::E2021, alloc) {
            PatchResult::Expanded(prod) => {
                Some(render_tokens(&prod.tokens, 0, prod.tokens.len()))
            }
            PatchResult::Fallback => None,
        }
    }

    #[test]
    fn serde_derive_structs_enums_attrs_and_exotic_fallback() {
        // 1. Struct with rename_all + default + skip + generic
        let item = r#"
            #[serde(rename_all = "camelCase", deny_unknown_fields)]
            pub struct User<T> {
                pub user_id: u64,
                #[serde(rename = "fullName")]
                pub name: String,
                #[serde(default)]
                pub score: T,
                #[serde(skip)]
                pub cached: bool,
            }
        "#;
        let ser = expand_code("serde_derive", ProcKind::Derive, "Serialize", item)
            .expect("Serialize expands at comptime");
        assert!(ser.contains("serialize_struct"));
        assert!(ser.contains("\"userId\""));
        assert!(ser.contains("\"fullName\""));
        assert!(!ser.contains("\"cached\""));

        let de = expand_code("serde_derive", ProcKind::Derive, "Deserialize", item)
            .expect("Deserialize expands at comptime");
        assert!(de.contains("deserialize_struct"));
        assert!(de.contains("unknown_field"));
        assert!(de.contains("\"userId\""));
        assert!(de.contains("\"fullName\""));

        // 2. Transparent newtype struct
        let trans = "#[serde(transparent)] pub struct Wrapper(pub u32);";
        let ser_t = expand_code("serde_derive", ProcKind::Derive, "Serialize", trans).unwrap();
        let de_t = expand_code("serde_derive", ProcKind::Derive, "Deserialize", trans).unwrap();
        assert!(ser_t.contains("&self.0"));
        assert!(de_t.contains("Wrapper(__v)"));

        // 3. Externally tagged enum
        let en = r#"
            #[serde(rename_all = "snake_case")]
            pub enum Event {
                Ping,
                Payload(u64),
            }
        "#;
        let ser_e = expand_code("serde_derive", ProcKind::Derive, "Serialize", en).unwrap();
        let de_e = expand_code("serde_derive", ProcKind::Derive, "Deserialize", en).unwrap();
        assert!(ser_e.contains("\"ping\""));
        assert!(ser_e.contains("\"payload\""));
        assert!(de_e.contains("deserialize_enum"));

        // 4. Exotic serde attribute MUST cleanly fall back!
        let exotic = r#"
            #[serde(tag = "type")]
            pub enum Tagged { A { x: u32 } }
        "#;
        assert!(expand_code("serde_derive", ProcKind::Derive, "Serialize", exotic).is_none());
        assert!(expand_code("serde_derive", ProcKind::Derive, "Deserialize", exotic).is_none());

        // 5. Lifetimes on Deserialize cleanly fall back to serde_derive!
        let borrowed = "pub struct Borrowed<'a> { pub s: &'a str }";
        assert!(expand_code("serde_derive", ProcKind::Derive, "Serialize", borrowed).is_some());
        assert!(expand_code("serde_derive", ProcKind::Derive, "Deserialize", borrowed).is_none());
    }

    #[test]
    fn bytemuck_thiserror_derive_more_strum_and_paste_expand_cleanly() {
        // bytemuck_derive
        let pod_item = "#[repr(C)] #[derive(Clone, Copy)] pub struct Header { pub a: u32, pub b: u32 }";
        let pod = expand_code("bytemuck_derive", ProcKind::Derive, "Pod", pod_item).unwrap();
        assert!(pod.contains("unsafe impl ::bytemuck::Pod for Header"));
        assert!(pod.contains("size_of::<Header>()"));

        // thiserror_impl
        let err_item = r#"
            #[derive(Debug)]
            pub enum MyErr {
                #[error("missing item {0}")]
                Missing(u32),
                #[error("io failed")]
                Io(#[from] std::io::Error),
                #[error(transparent)]
                Other(std::fmt::Error),
            }
        "#;
        let err_exp = expand_code("thiserror-impl", ProcKind::Derive, "Error", err_item).unwrap();
        assert!(err_exp.contains("impl ::core::fmt::Display for MyErr"));
        assert!(err_exp.contains("impl ::core::error::Error for MyErr"));
        assert!(err_exp.contains("::core::convert::From") && err_exp.contains("std::io::Error") && err_exp.contains("for MyErr"));

        // derive_more
        let dm_item = "pub struct UserId(pub u64);";
        let dm_from = expand_code("derive_more", ProcKind::Derive, "From", dm_item).unwrap();
        assert!(dm_from.contains("::core::convert::From") && dm_from.contains("u64"));
        assert!(expand_code("derive_more", ProcKind::Derive, "Deref", dm_item).unwrap().contains("type Target = u64"));
        assert!(expand_code("derive_more", ProcKind::Derive, "Constructor", dm_item).unwrap().contains("pub const fn new"));

        let dm_enum = "pub enum State { Idle, ActiveNow(u32) }";
        let is_v = expand_code("derive_more_impl", ProcKind::Derive, "IsVariant", dm_enum).unwrap();
        assert!(is_v.contains("pub const fn is_idle"));
        assert!(is_v.contains("pub const fn is_active_now"));

        // strum_macros
        let strum_item = r#"
            #[strum(serialize_all = "kebab-case")]
            pub enum Color {
                DarkRed,
                #[strum(serialize = "ocean")]
                DeepBlue,
            }
        "#;
        let s_ref = expand_code("strum_macros", ProcKind::Derive, "AsRefStr", strum_item).unwrap();
        assert!(s_ref.contains("\"dark-red\""));
        assert!(s_ref.contains("\"ocean\""));
        let s_cnt = expand_code("strum_macros", ProcKind::Derive, "EnumCount", strum_item).unwrap();
        assert!(s_cnt.contains("const COUNT: usize = 2usize"));

        // paste!
        let p = expand_code("paste", ProcKind::FnLike, "paste", "fn [<get_ FooBar:snake>]() -> u32 { 42 }").unwrap();
        assert!(p.contains("fn get_foo_bar"), "got: {p}");
    }

    #[test]
    fn generated_expansions_typecheck_and_run_with_rustc() {
        let alloc = heap().alloc();
        let expand_raw = |krate: &str, kind: ProcKind, mac: &str, src: &str| -> String {
            let stream = lex(src, Edition::E2021, alloc).unwrap();
            let call = ProcCall {
                kind,
                input: &stream,
                attr_args: None,
                file: Ok("/workspace/src/lib.rs"),
            };
            let PatchResult::Expanded(prod) = try_expand(krate, kind, mac, &call, Edition::E2021, alloc) else {
                panic!("expected comptime patch expansion for {krate}::{mac}");
            };
            render_tokens(&prod.tokens, 0, prod.tokens.len())
        };

        let dm_struct = "pub struct Port(pub u16);";
        let dm_from = expand_raw("derive_more", ProcKind::Derive, "From", dm_struct);
        let dm_deref = expand_raw("derive_more", ProcKind::Derive, "Deref", dm_struct);
        let dm_ctor = expand_raw("derive_more", ProcKind::Derive, "Constructor", dm_struct);
        let dm_disp = expand_raw("derive_more", ProcKind::Derive, "Display", dm_struct);

        let err_enum_def = "
            #[derive(Debug)]
            pub enum AppErr {
                Missing(u32),
                BadKey { key: &'static str },
            }
        ";
        let err_enum_annotated = r#"
            #[derive(Debug)]
            pub enum AppErr {
                #[error("missing {0}")]
                Missing(u32),
                #[error("bad key {key}")]
                BadKey { key: &'static str },
            }
        "#;
        let err_impl = expand_raw("thiserror_impl", ProcKind::Derive, "Error", err_enum_annotated);

        let strum_def = "pub enum Kind { FastPath, SlowFallback }";
        let strum_annotated = r#"
            #[strum(serialize_all = "kebab-case")]
            pub enum Kind { FastPath, SlowFallback }
        "#;
        let strum_as_ref = expand_raw("strum_macros", ProcKind::Derive, "AsRefStr", strum_annotated);
        let strum_into_str = expand_raw("strum_macros", ProcKind::Derive, "IntoStaticStr", strum_annotated);
        let is_var = expand_raw("derive_more", ProcKind::Derive, "IsVariant", strum_def);

        let pasted = expand_raw("paste", ProcKind::FnLike, "paste", "fn [<compute_ Answer:snake>]() -> u32 { 42 }");

        let program = format!(
            r#"
{dm_struct}
{dm_from}
{dm_deref}
{dm_ctor}
{dm_disp}
{err_enum_def}
{err_impl}
{strum_def}
{strum_as_ref}
{strum_into_str}
{is_var}
{pasted}

fn main() {{
    let p = Port::new(8080);
    let p2: Port = 8080u16.into();
    assert_eq!(*p, 8080);
    assert_eq!(*p2, 8080);
    assert_eq!(format!("{{p}}"), "8080");

    let e1 = AppErr::Missing(7);
    let e2 = AppErr::BadKey {{ key: "foo" }};
    assert_eq!(format!("{{e1}}"), "missing 7");
    assert_eq!(format!("{{e2}}"), "bad key foo");
    assert!(std::error::Error::source(&e1).is_none());

    let k = Kind::FastPath;
    assert_eq!(k.as_ref(), "fast-path");
    let s: &'static str = k.into();
    assert_eq!(s, "fast-path");
    assert!(Kind::FastPath.is_fast_path());
    assert!(!Kind::FastPath.is_slow_fallback());
    assert_eq!(compute_answer(), 42);
}}
"#
        );

        let dir = std::env::temp_dir().join(format!("blaze-comptime-patch-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let rs = dir.join("main.rs");
        let bin = dir.join("main_bin");
        std::fs::write(&rs, &program).unwrap();

        let compile = std::process::Command::new("rustc")
            .arg("--edition=2021")
            .arg(&rs)
            .arg("-o")
            .arg(&bin)
            .output()
            .expect("rustc available in test env");
        assert!(
            compile.status.success(),
            "generated code failed rustc:\n{}\n--- SOURCE ---\n{program}",
            String::from_utf8_lossy(&compile.stderr)
        );
        let run = std::process::Command::new(&bin).output().expect("run compiled test");
        assert!(run.status.success(), "compiled test panicked: {}", String::from_utf8_lossy(&run.stderr));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
