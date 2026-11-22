//! The `gpu` crate's macros (docs/future/gpu/shaders.md), implemented in pure `proc_macro` with
//! zero external dependencies.
//!
//! Each macro only lists what it is given — the parameters' names and types, or the fields' names
//! and types; what a type means (a uniform, a built-in, a flat varying) is its trait constant in
//! `gpu`, and the rules (slots, locations, what a stage may take) are `const fn`s there.
//!
//! - `#[gpu::vertex]`, `#[gpu::fragment]`, `#[gpu::kernel(threads = ...)]`: the function
//!   forwarded verbatim, and beside it a module of its name holding `ENTRY: gpu::EntryDesc`,
//!   `PARAMS`, and one `gpu::Slot` per parameter, named like it.
//! - `#[derive(gpu::Varyings)]`: `gpu::Varyings` (the fields and their locations), and `gpu::Param`
//!   and `gpu::Output`, so the struct is what a vertex stage returns and a fragment stage takes.
//! - `#[derive(gpu::Pod)]`: `gpu::Pod` for a `#[repr(C)]` or `#[repr(transparent)]` struct whose
//!   fields are all `Pod` and which has no padding, both checked at compile time.

#![forbid(unsafe_code)]

use proc_macro::{Delimiter, Group, Ident, Literal, Punct, Spacing, Span, TokenStream, TokenTree};

#[proc_macro_attribute]
pub fn vertex(attr: TokenStream, item: TokenStream) -> TokenStream {
    entry(attr, item, "Vertex", "vertex")
}

#[proc_macro_attribute]
pub fn fragment(attr: TokenStream, item: TokenStream) -> TokenStream {
    entry(attr, item, "Fragment", "fragment")
}

/// A compute kernel: `#[gpu::kernel(threads = 64)]`, `(64,)`, `(8, 8)` or `(4, 4, 4)`, the
/// workgroup's size on each axis (Slang's `numthreads`); an axis left out is 1.
#[proc_macro_attribute]
pub fn kernel(attr: TokenStream, item: TokenStream) -> TokenStream {
    let original = item.clone();
    let parsed = match parse_fn(item, "kernel") {
        Ok(f) => f,
        Err(e) => return e.into_tokens(),
    };
    let what = format!("#[gpu::kernel] fn {}", parsed.name);
    let [x, y, z] = match parse_threads(attr, &what, parsed.name.span()) {
        Ok(dims) => dims,
        Err(e) => return e.into_tokens(),
    };
    let span = parsed.name.span();
    let mut stage = TokenStream::new();
    push_path(&mut stage, &["gpu", "Stage", "Compute"], span);
    let mut fields = TokenStream::new();
    push_ident(&mut fields, "x", span);
    push_punct(&mut fields, ':', Spacing::Alone, span);
    push_lit_u32(&mut fields, x, span);
    push_punct(&mut fields, ',', Spacing::Alone, span);
    push_ident(&mut fields, "y", span);
    push_punct(&mut fields, ':', Spacing::Alone, span);
    push_lit_u32(&mut fields, y, span);
    push_punct(&mut fields, ',', Spacing::Alone, span);
    push_ident(&mut fields, "z", span);
    push_punct(&mut fields, ':', Spacing::Alone, span);
    push_lit_u32(&mut fields, z, span);
    push_group(&mut stage, Delimiter::Brace, fields, span);

    emit_entry(original, &parsed, stage)
}

#[proc_macro_derive(Varyings)]
pub fn derive_varyings(input: TokenStream) -> TokenStream {
    match expand_varyings(input) {
        Ok(ts) => ts,
        Err(e) => e.into_tokens(),
    }
}

#[proc_macro_derive(Pod)]
pub fn derive_pod(input: TokenStream) -> TokenStream {
    match expand_pod(input) {
        Ok(ts) => ts,
        Err(e) => e.into_tokens(),
    }
}

// ---------------------------------------------------------------------------
// Error & TokenStream construction helpers
// ---------------------------------------------------------------------------

struct Error {
    span: Span,
    message: String,
}

impl Error {
    fn new(span: Span, message: impl Into<String>) -> Self {
        Self { span, message: message.into() }
    }

    fn into_tokens(self) -> TokenStream {
        let mut out = TokenStream::new();
        push_path(&mut out, &["core", "compile_error"], self.span);
        push_punct(&mut out, '!', Spacing::Alone, self.span);
        let mut inner = TokenStream::new();
        push_lit_str(&mut inner, &self.message, self.span);
        push_group(&mut out, Delimiter::Parenthesis, inner, self.span);
        push_punct(&mut out, ';', Spacing::Alone, self.span);
        out
    }
}

fn push_ident(out: &mut TokenStream, name: &str, span: Span) {
    out.extend([TokenTree::Ident(Ident::new(name, span))]);
}

fn push_punct(out: &mut TokenStream, ch: char, spacing: Spacing, span: Span) {
    let mut p = Punct::new(ch, spacing);
    p.set_span(span);
    out.extend([TokenTree::Punct(p)]);
}

fn push_colon2(out: &mut TokenStream, span: Span) {
    push_punct(out, ':', Spacing::Joint, span);
    push_punct(out, ':', Spacing::Alone, span);
}

fn push_path(out: &mut TokenStream, segments: &[&str], span: Span) {
    for seg in segments {
        push_colon2(out, span);
        push_ident(out, seg, span);
    }
}

fn push_lit_str(out: &mut TokenStream, s: &str, span: Span) {
    let mut lit = Literal::string(s);
    lit.set_span(span);
    out.extend([TokenTree::Literal(lit)]);
}

fn push_lit_u32(out: &mut TokenStream, v: u32, span: Span) {
    let mut lit = Literal::u32_unsuffixed(v);
    lit.set_span(span);
    out.extend([TokenTree::Literal(lit)]);
}

fn push_lit_usize(out: &mut TokenStream, v: usize, span: Span) {
    let mut lit = Literal::usize_unsuffixed(v);
    lit.set_span(span);
    out.extend([TokenTree::Literal(lit)]);
}

fn push_group(out: &mut TokenStream, delim: Delimiter, inner: TokenStream, span: Span) {
    let mut g = Group::new(delim, inner);
    g.set_span(span);
    out.extend([TokenTree::Group(g)]);
}

fn push_doc_attr(out: &mut TokenStream, doc: &str, span: Span) {
    push_punct(out, '#', Spacing::Alone, span);
    let mut inner = TokenStream::new();
    push_ident(&mut inner, "doc", span);
    push_punct(&mut inner, '=', Spacing::Alone, span);
    push_lit_str(&mut inner, doc, span);
    push_group(out, Delimiter::Bracket, inner, span);
}

fn push_tokens(out: &mut TokenStream, tokens: &[TokenTree]) {
    out.extend(tokens.iter().cloned());
}

fn is_punct(tt: &TokenTree, ch: char) -> bool {
    matches!(tt, TokenTree::Punct(p) if p.as_char() == ch)
}

fn is_joint_punct(tt: &TokenTree, ch: char) -> bool {
    matches!(tt, TokenTree::Punct(p) if p.as_char() == ch && p.spacing() == Spacing::Joint)
}

fn is_ident(tt: &TokenTree, name: &str) -> bool {
    matches!(tt, TokenTree::Ident(id) if id.to_string() == name)
}

fn is_ellipsis(tokens: &[TokenTree]) -> bool {
    tokens.len() >= 3
        && is_joint_punct(&tokens[0], '.')
        && is_joint_punct(&tokens[1], '.')
        && is_punct(&tokens[2], '.')
}

/// Skip zero or more outer attributes (`#[...]`), returning the skipped attribute bodies and
/// advancing `pos`.
fn skip_outer_attrs<'a>(tokens: &'a [TokenTree], pos: &mut usize) -> Vec<Group> {
    let mut attrs = Vec::new();
    while *pos + 1 < tokens.len() && is_punct(&tokens[*pos], '#') {
        if let TokenTree::Group(g) = &tokens[*pos + 1]
            && g.delimiter() == Delimiter::Bracket
        {
            attrs.push(g.clone());
            *pos += 2;
            continue;
        }
        break;
    }
    attrs
}

/// Parse optional visibility (`pub` or `pub(...)`), returning the visibility tokens and advancing
/// `pos`.
fn parse_visibility(tokens: &[TokenTree], pos: &mut usize) -> Vec<TokenTree> {
    let mut vis = Vec::new();
    if *pos < tokens.len() && is_ident(&tokens[*pos], "pub") {
        vis.push(tokens[*pos].clone());
        *pos += 1;
        if *pos < tokens.len()
            && let TokenTree::Group(g) = &tokens[*pos]
            && g.delimiter() == Delimiter::Parenthesis
        {
            vis.push(tokens[*pos].clone());
            *pos += 1;
        }
    }
    vis
}

/// Split `tokens` on top-level `,` separators, respecting `<...>` generic brackets (and ignoring
/// `->`). Trailing commas produce no trailing empty slice.
fn split_top_level_commas(tokens: &[TokenTree]) -> Vec<&[TokenTree]> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut angle_depth = 0usize;
    let mut i = 0usize;
    while i < tokens.len() {
        // Step over `->` so `>` does not affect `angle_depth`.
        if is_joint_punct(&tokens[i], '-') && i + 1 < tokens.len() && is_punct(&tokens[i + 1], '>') {
            i += 2;
            continue;
        }
        if is_punct(&tokens[i], '<') {
            angle_depth += 1;
        } else if is_punct(&tokens[i], '>') {
            angle_depth = angle_depth.saturating_sub(1);
        } else if angle_depth == 0 && is_punct(&tokens[i], ',') {
            parts.push(&tokens[start..i]);
            start = i + 1;
        }
        i += 1;
    }
    if start < tokens.len() {
        parts.push(&tokens[start..]);
    }
    parts
}

/// Find the index of the first top-level single `:` (not `::`), respecting `<...>`.
fn find_top_level_colon(tokens: &[TokenTree]) -> Option<usize> {
    let mut angle_depth = 0usize;
    let mut i = 0usize;
    while i < tokens.len() {
        if is_joint_punct(&tokens[i], '-') && i + 1 < tokens.len() && is_punct(&tokens[i + 1], '>') {
            i += 2;
            continue;
        }
        if is_punct(&tokens[i], '<') {
            angle_depth += 1;
        } else if is_punct(&tokens[i], '>') {
            angle_depth = angle_depth.saturating_sub(1);
        } else if angle_depth == 0 && is_punct(&tokens[i], ':') {
            if is_joint_punct(&tokens[i], ':') && i + 1 < tokens.len() && is_punct(&tokens[i + 1], ':') {
                i += 2;
                continue;
            }
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Parse a base-10 `u32` integer literal (allowing `_` separators and an optional `u32` suffix).
fn parse_u32_literal(raw: &str) -> Option<u32> {
    let s = raw.strip_suffix("u32").unwrap_or(raw);
    if s.is_empty() {
        return None;
    }
    // Reject non-decimal prefixes (`0x`, `0b`, `0o`) and non-digit characters.
    let mut value: u32 = 0;
    let mut saw_digit = false;
    for b in s.bytes() {
        if b == b'_' {
            continue;
        }
        if !b.is_ascii_digit() {
            return None;
        }
        saw_digit = true;
        value = value.checked_mul(10)?.checked_add(u32::from(b - b'0'))?;
    }
    saw_digit.then_some(value)
}

// ---------------------------------------------------------------------------
// Entry points: #[gpu::vertex], #[gpu::fragment], #[gpu::kernel]
// ---------------------------------------------------------------------------

struct ParsedParam {
    ident: Ident,
    name: String,
    ty: Vec<TokenTree>,
}

struct ParsedFn {
    vis: Vec<TokenTree>,
    name: Ident,
    params: Vec<ParsedParam>,
    ret_ty: Vec<TokenTree>,
}

fn entry(attr: TokenStream, item: TokenStream, stage_variant: &str, attr_name: &str) -> TokenStream {
    let attr_tokens: Vec<TokenTree> = attr.into_iter().collect();
    if let Some(first) = attr_tokens.first() {
        return Error::new(first.span(), format!("#[gpu::{attr_name}] takes no arguments")).into_tokens();
    }
    let original = item.clone();
    let parsed = match parse_fn(item, attr_name) {
        Ok(f) => f,
        Err(e) => return e.into_tokens(),
    };
    let span = parsed.name.span();
    let mut stage = TokenStream::new();
    push_path(&mut stage, &["gpu", "Stage", stage_variant], span);
    emit_entry(original, &parsed, stage)
}

fn parse_threads(attr: TokenStream, what: &str, fallback_span: Span) -> Result<[u32; 3], Error> {
    let usage = || format!("{what}: write `#[gpu::kernel(threads = 64)]`, `(8, 8)` or `(4, 4, 4)`");
    let tokens: Vec<TokenTree> = attr.into_iter().collect();
    if tokens.is_empty() {
        return Err(Error::new(fallback_span, usage()));
    }
    let parts = split_top_level_commas(&tokens);
    let mut found: Option<&[TokenTree]> = None;
    for part in parts {
        if part.is_empty() {
            return Err(Error::new(tokens[0].span(), usage()));
        }
        let Some(TokenTree::Ident(key)) = part.first() else {
            return Err(Error::new(part[0].span(), usage()));
        };
        if part.len() < 3 || !is_punct(&part[1], '=') {
            return Err(Error::new(part[0].span(), usage()));
        }
        if key.to_string() != "threads" {
            return Err(Error::new(key.span(), format!("{what}: the only argument is `threads`")));
        }
        if found.is_some() {
            return Err(Error::new(key.span(), format!("{what}: `threads` is given once")));
        }
        found = Some(&part[2..]);
    }
    let Some(value) = found else {
        return Err(Error::new(tokens[0].span(), usage()));
    };
    if value.len() != 1 {
        return Err(Error::new(value[0].span(), usage()));
    }
    let parse_axis = |tt: &TokenTree| -> Result<u32, Error> {
        match tt {
            TokenTree::Literal(lit) => parse_u32_literal(&lit.to_string()).ok_or_else(|| Error::new(lit.span(), usage())),
            other => Err(Error::new(other.span(), usage())),
        }
    };
    let mut size = [1u32; 3];
    match &value[0] {
        TokenTree::Literal(_) => {
            size[0] = parse_axis(&value[0])?;
        }
        TokenTree::Group(g) if g.delimiter() == Delimiter::Parenthesis => {
            let inner: Vec<TokenTree> = g.stream().into_iter().collect();
            let elems = split_top_level_commas(&inner);
            if elems.is_empty() || elems.len() > 3 {
                return Err(Error::new(g.span(), usage()));
            }
            for (i, elem) in elems.iter().enumerate() {
                if elem.len() != 1 {
                    let sp = elem.first().map_or_else(|| g.span(), TokenTree::span);
                    return Err(Error::new(sp, usage()));
                }
                size[i] = parse_axis(&elem[0])?;
            }
        }
        other => return Err(Error::new(other.span(), usage())),
    }
    Ok(size)
}

fn parse_fn(item: TokenStream, attr: &str) -> Result<ParsedFn, Error> {
    let tokens: Vec<TokenTree> = item.into_iter().collect();
    if tokens.is_empty() {
        return Err(Error::new(Span::call_site(), format!("#[gpu::{attr}] expects a function")));
    }
    let mut pos = 0usize;
    let _attrs = skip_outer_attrs(&tokens, &mut pos);
    let vis = parse_visibility(&tokens, &mut pos);

    // Function qualifiers before `fn`: `default`, `const`, `async`, `unsafe`, `extern ["ABI"]`.
    let mut async_span: Option<Span> = None;
    while pos < tokens.len() {
        if is_ident(&tokens[pos], "const") || is_ident(&tokens[pos], "unsafe") || is_ident(&tokens[pos], "default") {
            pos += 1;
        } else if is_ident(&tokens[pos], "async") {
            async_span = Some(tokens[pos].span());
            pos += 1;
        } else if is_ident(&tokens[pos], "extern") {
            pos += 1;
            if pos < tokens.len() && matches!(&tokens[pos], TokenTree::Literal(_)) {
                pos += 1;
            }
        } else {
            break;
        }
    }

    if pos >= tokens.len() || !is_ident(&tokens[pos], "fn") {
        let sp = tokens.get(pos).map_or_else(Span::call_site, TokenTree::span);
        return Err(Error::new(sp, format!("#[gpu::{attr}] expects a function")));
    }
    pos += 1;

    let Some(TokenTree::Ident(name)) = tokens.get(pos).cloned() else {
        let sp = tokens.get(pos).map_or_else(Span::call_site, TokenTree::span);
        return Err(Error::new(sp, format!("#[gpu::{attr}] expects a function name")));
    };
    pos += 1;

    let what = format!("#[gpu::{attr}] fn {name}");

    if let Some(a_span) = async_span {
        return Err(Error::new(a_span, format!("{what}: an entry point is not async")));
    }

    // Reject generics `<...>` (non-empty). Empty `<>` is allowed (matching `syn`).
    if pos < tokens.len() && is_punct(&tokens[pos], '<') {
        let lt_span = tokens[pos].span();
        if pos + 1 < tokens.len() && is_punct(&tokens[pos + 1], '>') {
            pos += 2;
        } else {
            return Err(Error::new(lt_span, format!("{what}: an entry point is not generic")));
        }
    }

    let Some(TokenTree::Group(param_group)) = tokens.get(pos) else {
        let sp = tokens.get(pos).map_or_else(|| name.span(), TokenTree::span);
        return Err(Error::new(sp, format!("{what}: expected parameter list")));
    };
    if param_group.delimiter() != Delimiter::Parenthesis {
        return Err(Error::new(param_group.span(), format!("{what}: expected parameter list")));
    }
    pos += 1;

    let param_tokens: Vec<TokenTree> = param_group.stream().into_iter().collect();
    let mut params = Vec::new();
    for raw_arg in split_top_level_commas(&param_tokens) {
        if raw_arg.is_empty() {
            continue;
        }
        let mut ap = 0usize;
        let _param_attrs = skip_outer_attrs(raw_arg, &mut ap);
        let arg = &raw_arg[ap..];
        if arg.is_empty() {
            continue;
        }
        if is_ellipsis(arg) {
            return Err(Error::new(arg[0].span(), format!("{what}: an entry point takes no variadic arguments")));
        }
        if is_self_receiver(arg) {
            return Err(Error::new(arg[0].span(), format!("{what}: an entry point takes no `self`")));
        }
        let Some(colon) = find_top_level_colon(arg) else {
            return Err(Error::new(
                arg[0].span(),
                format!("{what}: each parameter is a plain name, which the binding table is keyed by"),
            ));
        };
        let pat = &arg[..colon];
        let ty = &arg[colon + 1..];
        if is_ellipsis(ty) {
            return Err(Error::new(ty[0].span(), format!("{what}: an entry point takes no variadic arguments")));
        }
        if ty.is_empty() {
            return Err(Error::new(arg[colon].span(), format!("{what}: expected parameter type")));
        }
        let ident = extract_plain_param_ident(pat, &what)?;
        params.push(ParsedParam { name: ident.to_string(), ident, ty: ty.to_vec() });
    }

    // Parse optional return type `-> RetTy` and check for `where`.
    let mut ret_ty = Vec::new();
    if pos + 1 < tokens.len() && is_joint_punct(&tokens[pos], '-') && is_punct(&tokens[pos + 1], '>') {
        pos += 2;
        let ret_start = pos;
        while pos < tokens.len() {
            if is_ident(&tokens[pos], "where") {
                return Err(Error::new(tokens[pos].span(), format!("{what}: an entry point is not generic")));
            }
            if matches!(&tokens[pos], TokenTree::Group(g) if g.delimiter() == Delimiter::Brace) {
                break;
            }
            pos += 1;
        }
        if pos == ret_start {
            return Err(Error::new(name.span(), format!("{what}: expected return type after `->`")));
        }
        ret_ty.extend_from_slice(&tokens[ret_start..pos]);
    } else {
        if pos < tokens.len() && is_ident(&tokens[pos], "where") {
            return Err(Error::new(tokens[pos].span(), format!("{what}: an entry point is not generic")));
        }
        if attr == "kernel" {
            let mut g = Group::new(Delimiter::Parenthesis, TokenStream::new());
            g.set_span(name.span());
            ret_ty.push(TokenTree::Group(g));
        } else {
            return Err(Error::new(
                name.span(),
                format!("{what}: an entry point returns its varyings (vertex) or its colour (fragment)"),
            ));
        }
    }

    // Ensure there is a function body `{ ... }`.
    if !matches!(tokens.get(pos), Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Brace) {
        let sp = tokens.get(pos).map_or_else(|| name.span(), TokenTree::span);
        return Err(Error::new(sp, format!("{what}: expected function body")));
    }

    Ok(ParsedFn { vis, name, params, ret_ty })
}

fn is_self_receiver(arg: &[TokenTree]) -> bool {
    let head = match find_top_level_colon(arg) {
        Some(c) => &arg[..c],
        None => arg,
    };
    head.last().is_some_and(|tt| is_ident(tt, "self"))
}

fn extract_plain_param_ident(pat: &[TokenTree], what: &str) -> Result<Ident, Error> {
    let err = |sp: Span| {
        Error::new(sp, format!("{what}: each parameter is a plain name, which the binding table is keyed by"))
    };
    let id = match pat {
        [TokenTree::Ident(id)] => id.clone(),
        [TokenTree::Ident(kw), TokenTree::Ident(id)] if kw.to_string() == "mut" => id.clone(),
        _ => {
            let sp = pat.first().map_or_else(Span::call_site, TokenTree::span);
            return Err(err(sp));
        }
    };
    if id.to_string() == "_" {
        return Err(err(id.span()));
    }
    Ok(id)
}

fn emit_entry(original: TokenStream, f: &ParsedFn, stage: TokenStream) -> TokenStream {
    let mut out = original;
    let span = f.name.span();
    let name_s = f.name.to_string();
    let n = f.params.len();

    // #[doc = "..."]
    let doc = format!("`{name_s}`'s binding table, from its signature (`gpu::layout`).");
    push_doc_attr(&mut out, &doc, span);

    // #[allow(non_upper_case_globals, non_snake_case, unused_imports, unreachable_pub, reason = "...")]
    push_punct(&mut out, '#', Spacing::Alone, span);
    let mut allow_inner = TokenStream::new();
    push_ident(&mut allow_inner, "allow", span);
    let mut allow_args = TokenStream::new();
    push_ident(&mut allow_args, "non_upper_case_globals", span);
    push_punct(&mut allow_args, ',', Spacing::Alone, span);
    push_ident(&mut allow_args, "non_snake_case", span);
    push_punct(&mut allow_args, ',', Spacing::Alone, span);
    push_ident(&mut allow_args, "unused_imports", span);
    push_punct(&mut allow_args, ',', Spacing::Alone, span);
    push_ident(&mut allow_args, "unreachable_pub", span);
    push_punct(&mut allow_args, ',', Spacing::Alone, span);
    push_ident(&mut allow_args, "reason", span);
    push_punct(&mut allow_args, '=', Spacing::Alone, span);
    push_lit_str(&mut allow_args, "generated GPU entry descriptor module", span);
    push_group(&mut allow_inner, Delimiter::Parenthesis, allow_args, span);
    push_group(&mut out, Delimiter::Bracket, allow_inner, span);

    // <vis> mod <name> { ... }
    push_tokens(&mut out, &f.vis);
    push_ident(&mut out, "mod", span);
    out.extend([TokenTree::Ident(f.name.clone())]);

    let mut body = TokenStream::new();

    // use super::*;
    push_ident(&mut body, "use", span);
    push_ident(&mut body, "super", span);
    push_colon2(&mut body, span);
    push_punct(&mut body, '*', Spacing::Alone, span);
    push_punct(&mut body, ';', Spacing::Alone, span);

    // const NAMES: [&str; N] = ["a", "b", ...];
    push_ident(&mut body, "const", span);
    push_ident(&mut body, "NAMES", span);
    push_punct(&mut body, ':', Spacing::Alone, span);
    let mut names_ty = TokenStream::new();
    push_punct(&mut names_ty, '&', Spacing::Alone, span);
    push_ident(&mut names_ty, "str", span);
    push_punct(&mut names_ty, ';', Spacing::Alone, span);
    push_lit_usize(&mut names_ty, n, span);
    push_group(&mut body, Delimiter::Bracket, names_ty, span);
    push_punct(&mut body, '=', Spacing::Alone, span);
    let mut names_arr = TokenStream::new();
    for p in &f.params {
        push_lit_str(&mut names_arr, &p.name, p.ident.span());
        push_punct(&mut names_arr, ',', Spacing::Alone, p.ident.span());
    }
    push_group(&mut body, Delimiter::Bracket, names_arr, span);
    push_punct(&mut body, ';', Spacing::Alone, span);

    // const KINDS: [::gpu::Kind; N] = [<T0 as ::gpu::Param>::KIND, ...];
    push_ident(&mut body, "const", span);
    push_ident(&mut body, "KINDS", span);
    push_punct(&mut body, ':', Spacing::Alone, span);
    let mut kinds_ty = TokenStream::new();
    push_path(&mut kinds_ty, &["gpu", "Kind"], span);
    push_punct(&mut kinds_ty, ';', Spacing::Alone, span);
    push_lit_usize(&mut kinds_ty, n, span);
    push_group(&mut body, Delimiter::Bracket, kinds_ty, span);
    push_punct(&mut body, '=', Spacing::Alone, span);
    let mut kinds_arr = TokenStream::new();
    for p in &f.params {
        let pspan = p.ident.span();
        push_punct(&mut kinds_arr, '<', Spacing::Alone, pspan);
        push_tokens(&mut kinds_arr, &p.ty);
        push_ident(&mut kinds_arr, "as", pspan);
        push_path(&mut kinds_arr, &["gpu", "Param"], pspan);
        push_punct(&mut kinds_arr, '>', Spacing::Alone, pspan);
        push_colon2(&mut kinds_arr, pspan);
        push_ident(&mut kinds_arr, "KIND", pspan);
        push_punct(&mut kinds_arr, ',', Spacing::Alone, pspan);
    }
    push_group(&mut body, Delimiter::Bracket, kinds_arr, span);
    push_punct(&mut body, ';', Spacing::Alone, span);

    // pub const PARAMS: [::gpu::ParamDesc; N] = ::gpu::layout(NAMES, KINDS);
    push_doc_attr(&mut body, "Each parameter's name, kind and slot.", span);
    push_ident(&mut body, "pub", span);
    push_ident(&mut body, "const", span);
    push_ident(&mut body, "PARAMS", span);
    push_punct(&mut body, ':', Spacing::Alone, span);
    let mut params_ty = TokenStream::new();
    push_path(&mut params_ty, &["gpu", "ParamDesc"], span);
    push_punct(&mut params_ty, ';', Spacing::Alone, span);
    push_lit_usize(&mut params_ty, n, span);
    push_group(&mut body, Delimiter::Bracket, params_ty, span);
    push_punct(&mut body, '=', Spacing::Alone, span);
    push_path(&mut body, &["gpu", "layout"], span);
    let mut layout_args = TokenStream::new();
    push_ident(&mut layout_args, "NAMES", span);
    push_punct(&mut layout_args, ',', Spacing::Alone, span);
    push_ident(&mut layout_args, "KINDS", span);
    push_group(&mut body, Delimiter::Parenthesis, layout_args, span);
    push_punct(&mut body, ';', Spacing::Alone, span);

    // pub const ENTRY: ::gpu::EntryDesc = ::gpu::EntryDesc { ... };
    push_doc_attr(&mut body, "The entry point as its signature describes it.", span);
    push_ident(&mut body, "pub", span);
    push_ident(&mut body, "const", span);
    push_ident(&mut body, "ENTRY", span);
    push_punct(&mut body, ':', Spacing::Alone, span);
    push_path(&mut body, &["gpu", "EntryDesc"], span);
    push_punct(&mut body, '=', Spacing::Alone, span);
    push_path(&mut body, &["gpu", "EntryDesc"], span);
    let mut entry_fields = TokenStream::new();
    // name: #name_s,
    push_ident(&mut entry_fields, "name", span);
    push_punct(&mut entry_fields, ':', Spacing::Alone, span);
    push_lit_str(&mut entry_fields, &name_s, span);
    push_punct(&mut entry_fields, ',', Spacing::Alone, span);
    // stage: #stage,
    push_ident(&mut entry_fields, "stage", span);
    push_punct(&mut entry_fields, ':', Spacing::Alone, span);
    entry_fields.extend(stage);
    push_punct(&mut entry_fields, ',', Spacing::Alone, span);
    // params: &PARAMS,
    push_ident(&mut entry_fields, "params", span);
    push_punct(&mut entry_fields, ':', Spacing::Alone, span);
    push_punct(&mut entry_fields, '&', Spacing::Alone, span);
    push_ident(&mut entry_fields, "PARAMS", span);
    push_punct(&mut entry_fields, ',', Spacing::Alone, span);
    // out: <#ret as ::gpu::Output>::OUT,
    push_ident(&mut entry_fields, "out", span);
    push_punct(&mut entry_fields, ':', Spacing::Alone, span);
    push_punct(&mut entry_fields, '<', Spacing::Alone, span);
    push_tokens(&mut entry_fields, &f.ret_ty);
    push_ident(&mut entry_fields, "as", span);
    push_path(&mut entry_fields, &["gpu", "Output"], span);
    push_punct(&mut entry_fields, '>', Spacing::Alone, span);
    push_colon2(&mut entry_fields, span);
    push_ident(&mut entry_fields, "OUT", span);
    push_punct(&mut entry_fields, ',', Spacing::Alone, span);
    // sizes: ::gpu::sizes_slot(&PARAMS),
    push_ident(&mut entry_fields, "sizes", span);
    push_punct(&mut entry_fields, ':', Spacing::Alone, span);
    push_path(&mut entry_fields, &["gpu", "sizes_slot"], span);
    let mut sizes_arg = TokenStream::new();
    push_punct(&mut sizes_arg, '&', Spacing::Alone, span);
    push_ident(&mut sizes_arg, "PARAMS", span);
    push_group(&mut entry_fields, Delimiter::Parenthesis, sizes_arg, span);
    push_punct(&mut entry_fields, ',', Spacing::Alone, span);

    push_group(&mut body, Delimiter::Brace, entry_fields, span);
    push_punct(&mut body, ';', Spacing::Alone, span);

    // const _: () = ::gpu::check(&ENTRY);
    push_ident(&mut body, "const", span);
    push_ident(&mut body, "_", span);
    push_punct(&mut body, ':', Spacing::Alone, span);
    push_group(&mut body, Delimiter::Parenthesis, TokenStream::new(), span);
    push_punct(&mut body, '=', Spacing::Alone, span);
    push_path(&mut body, &["gpu", "check"], span);
    let mut check_arg = TokenStream::new();
    push_punct(&mut check_arg, '&', Spacing::Alone, span);
    push_ident(&mut check_arg, "ENTRY", span);
    push_group(&mut body, Delimiter::Parenthesis, check_arg, span);
    push_punct(&mut body, ';', Spacing::Alone, span);

    // Per-parameter `pub const <id>: ::gpu::Slot = PARAMS[i].slot;`
    for (i, p) in f.params.iter().enumerate() {
        let pspan = p.ident.span();
        let pdoc = format!("Where `{name_s}`'s parameter `{}` is bound.", p.name);
        push_doc_attr(&mut body, &pdoc, pspan);
        push_ident(&mut body, "pub", pspan);
        push_ident(&mut body, "const", pspan);
        body.extend([TokenTree::Ident(p.ident.clone())]);
        push_punct(&mut body, ':', Spacing::Alone, pspan);
        push_path(&mut body, &["gpu", "Slot"], pspan);
        push_punct(&mut body, '=', Spacing::Alone, pspan);
        push_ident(&mut body, "PARAMS", pspan);
        let mut idx = TokenStream::new();
        push_lit_usize(&mut idx, i, pspan);
        push_group(&mut body, Delimiter::Bracket, idx, pspan);
        push_punct(&mut body, '.', Spacing::Alone, pspan);
        push_ident(&mut body, "slot", pspan);
        push_punct(&mut body, ';', Spacing::Alone, pspan);
    }

    push_group(&mut out, Delimiter::Brace, body, span);
    out
}

// ---------------------------------------------------------------------------
// Derive input parser & #[derive(gpu::Varyings)] / #[derive(gpu::Pod)]
// ---------------------------------------------------------------------------

struct ParsedField {
    ident: Option<Ident>,
    ty: Vec<TokenTree>,
}

enum StructFields {
    Named(Vec<ParsedField>),
    Unnamed(Vec<ParsedField>),
    Unit,
}

struct ParsedStruct {
    attrs: Vec<Group>,
    is_struct: bool,
    name: Ident,
    generic_span: Option<Span>,
    fields: StructFields,
}

fn parse_derive_input(input: TokenStream, derive_name: &str) -> Result<ParsedStruct, Error> {
    let tokens: Vec<TokenTree> = input.into_iter().collect();
    if tokens.is_empty() {
        return Err(Error::new(Span::call_site(), format!("#[derive(gpu::{derive_name})] expects a type definition")));
    }
    let mut pos = 0usize;
    let attrs = skip_outer_attrs(&tokens, &mut pos);
    let _vis = parse_visibility(&tokens, &mut pos);

    let is_struct = if pos < tokens.len() && is_ident(&tokens[pos], "struct") {
        pos += 1;
        true
    } else if pos < tokens.len() && (is_ident(&tokens[pos], "enum") || is_ident(&tokens[pos], "union")) {
        pos += 1;
        false
    } else {
        let sp = tokens.get(pos).map_or_else(Span::call_site, TokenTree::span);
        return Err(Error::new(sp, format!("#[derive(gpu::{derive_name})] expects a struct")));
    };

    let Some(TokenTree::Ident(name)) = tokens.get(pos).cloned() else {
        let sp = tokens.get(pos).map_or_else(Span::call_site, TokenTree::span);
        return Err(Error::new(sp, format!("#[derive(gpu::{derive_name})] expects a type name")));
    };
    pos += 1;

    let mut generic_span = None;
    if pos < tokens.len() && is_punct(&tokens[pos], '<') {
        if pos + 1 < tokens.len() && is_punct(&tokens[pos + 1], '>') {
            pos += 2;
        } else {
            generic_span = Some(tokens[pos].span());
            // Skip balanced `<...>` so we can still inspect subsequent tokens if needed.
            let mut depth = 0usize;
            while pos < tokens.len() {
                if is_punct(&tokens[pos], '<') {
                    depth += 1;
                } else if is_punct(&tokens[pos], '>') {
                    depth -= 1;
                    if depth == 0 {
                        pos += 1;
                        break;
                    }
                }
                pos += 1;
            }
        }
    }

    if !is_struct {
        return Ok(ParsedStruct { attrs, is_struct: false, name, generic_span, fields: StructFields::Unit });
    }

    // Check for `where` before `{` or `;`.
    if pos < tokens.len() && is_ident(&tokens[pos], "where") {
        generic_span.get_or_insert(tokens[pos].span());
        while pos < tokens.len() {
            if matches!(&tokens[pos], TokenTree::Group(g) if g.delimiter() == Delimiter::Brace)
                || is_punct(&tokens[pos], ';')
            {
                break;
            }
            pos += 1;
        }
    }

    let fields = match tokens.get(pos) {
        Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Brace => {
            let inner: Vec<TokenTree> = g.stream().into_iter().collect();
            let mut parsed = Vec::new();
            for raw in split_top_level_commas(&inner) {
                if raw.is_empty() {
                    continue;
                }
                let mut fp = 0usize;
                let _fattrs = skip_outer_attrs(raw, &mut fp);
                let _fvis = parse_visibility(raw, &mut fp);
                let Some(TokenTree::Ident(fid)) = raw.get(fp).cloned() else {
                    let sp = raw.get(fp).map_or_else(|| g.span(), TokenTree::span);
                    return Err(Error::new(sp, format!("#[derive(gpu::{derive_name})] {name}: expected field name")));
                };
                fp += 1;
                if fp >= raw.len() || !is_punct(&raw[fp], ':') {
                    let sp = raw.get(fp).map_or_else(|| fid.span(), TokenTree::span);
                    return Err(Error::new(sp, format!("#[derive(gpu::{derive_name})] {name}: expected `:` after field `{fid}`")));
                }
                fp += 1;
                if fp >= raw.len() {
                    return Err(Error::new(fid.span(), format!("#[derive(gpu::{derive_name})] {name}: expected type for field `{fid}`")));
                }
                parsed.push(ParsedField { ident: Some(fid), ty: raw[fp..].to_vec() });
            }
            StructFields::Named(parsed)
        }
        Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Parenthesis => {
            let inner: Vec<TokenTree> = g.stream().into_iter().collect();
            let mut parsed = Vec::new();
            for raw in split_top_level_commas(&inner) {
                if raw.is_empty() {
                    continue;
                }
                let mut fp = 0usize;
                let _fattrs = skip_outer_attrs(raw, &mut fp);
                let _fvis = parse_visibility(raw, &mut fp);
                if fp >= raw.len() {
                    return Err(Error::new(g.span(), format!("#[derive(gpu::{derive_name})] {name}: expected field type")));
                }
                parsed.push(ParsedField { ident: None, ty: raw[fp..].to_vec() });
            }
            // Check for `where` after tuple fields.
            if pos + 1 < tokens.len() && is_ident(&tokens[pos + 1], "where") {
                generic_span.get_or_insert(tokens[pos + 1].span());
            }
            StructFields::Unnamed(parsed)
        }
        Some(tt) if is_punct(tt, ';') => StructFields::Unit,
        other => {
            let sp = other.map_or_else(|| name.span(), TokenTree::span);
            return Err(Error::new(sp, format!("#[derive(gpu::{derive_name})] {name}: expected struct fields")));
        }
    };

    Ok(ParsedStruct { attrs, is_struct: true, name, generic_span, fields })
}

fn expand_varyings(input: TokenStream) -> Result<TokenStream, Error> {
    let s = parse_derive_input(input, "Varyings")?;
    let name = &s.name;
    let span = name.span();
    if let Some(gspan) = s.generic_span {
        return Err(Error::new(gspan, format!("#[derive(gpu::Varyings)] {name}: not generic")));
    }
    if !s.is_struct {
        return Err(Error::new(span, format!("#[derive(gpu::Varyings)] {name}: a struct with named fields")));
    }
    let StructFields::Named(fields) = &s.fields else {
        return Err(Error::new(span, format!("#[derive(gpu::Varyings)] {name}: a struct with named fields")));
    };
    let n = fields.len();
    let mut out = TokenStream::new();

    // impl ::gpu::Varyings for #name {
    //     const FIELDS: &'static [::gpu::Field] =
    //         &::gpu::layout_fields::<#n>([#(#names),*], [#(<#tys as ::gpu::Varying>::KIND),*]);
    // }
    push_ident(&mut out, "impl", span);
    push_path(&mut out, &["gpu", "Varyings"], span);
    push_ident(&mut out, "for", span);
    out.extend([TokenTree::Ident(name.clone())]);
    let mut vbody = TokenStream::new();
    push_ident(&mut vbody, "const", span);
    push_ident(&mut vbody, "FIELDS", span);
    push_punct(&mut vbody, ':', Spacing::Alone, span);
    push_punct(&mut vbody, '&', Spacing::Alone, span);
    push_punct(&mut vbody, '\'', Spacing::Joint, span);
    push_ident(&mut vbody, "static", span);
    let mut field_slice = TokenStream::new();
    push_path(&mut field_slice, &["gpu", "Field"], span);
    push_group(&mut vbody, Delimiter::Bracket, field_slice, span);
    push_punct(&mut vbody, '=', Spacing::Alone, span);
    push_punct(&mut vbody, '&', Spacing::Alone, span);
    push_path(&mut vbody, &["gpu", "layout_fields"], span);
    push_colon2(&mut vbody, span);
    push_punct(&mut vbody, '<', Spacing::Alone, span);
    push_lit_usize(&mut vbody, n, span);
    push_punct(&mut vbody, '>', Spacing::Alone, span);
    let mut call_args = TokenStream::new();
    let mut names_arr = TokenStream::new();
    let mut kinds_arr = TokenStream::new();
    for f in fields {
        let fid = f.ident.as_ref().expect("named field has ident");
        let fspan = fid.span();
        push_lit_str(&mut names_arr, &fid.to_string(), fspan);
        push_punct(&mut names_arr, ',', Spacing::Alone, fspan);

        push_punct(&mut kinds_arr, '<', Spacing::Alone, fspan);
        push_tokens(&mut kinds_arr, &f.ty);
        push_ident(&mut kinds_arr, "as", fspan);
        push_path(&mut kinds_arr, &["gpu", "Varying"], fspan);
        push_punct(&mut kinds_arr, '>', Spacing::Alone, fspan);
        push_colon2(&mut kinds_arr, fspan);
        push_ident(&mut kinds_arr, "KIND", fspan);
        push_punct(&mut kinds_arr, ',', Spacing::Alone, fspan);
    }
    push_group(&mut call_args, Delimiter::Bracket, names_arr, span);
    push_punct(&mut call_args, ',', Spacing::Alone, span);
    push_group(&mut call_args, Delimiter::Bracket, kinds_arr, span);
    push_group(&mut vbody, Delimiter::Parenthesis, call_args, span);
    push_punct(&mut vbody, ';', Spacing::Alone, span);
    push_group(&mut out, Delimiter::Brace, vbody, span);

    // impl ::gpu::Param for #name {
    //     const KIND: ::gpu::Kind = ::gpu::Kind::Varyings(<#name as ::gpu::Varyings>::FIELDS);
    // }
    push_ident(&mut out, "impl", span);
    push_path(&mut out, &["gpu", "Param"], span);
    push_ident(&mut out, "for", span);
    out.extend([TokenTree::Ident(name.clone())]);
    let mut pbody = TokenStream::new();
    push_ident(&mut pbody, "const", span);
    push_ident(&mut pbody, "KIND", span);
    push_punct(&mut pbody, ':', Spacing::Alone, span);
    push_path(&mut pbody, &["gpu", "Kind"], span);
    push_punct(&mut pbody, '=', Spacing::Alone, span);
    push_path(&mut pbody, &["gpu", "Kind", "Varyings"], span);
    let mut vfields = TokenStream::new();
    push_varyings_fields_expr(&mut vfields, name, span);
    push_group(&mut pbody, Delimiter::Parenthesis, vfields, span);
    push_punct(&mut pbody, ';', Spacing::Alone, span);
    push_group(&mut out, Delimiter::Brace, pbody, span);

    // impl ::gpu::Output for #name {
    //     const OUT: ::gpu::Out = ::gpu::Out::Varyings(<#name as ::gpu::Varyings>::FIELDS);
    // }
    push_ident(&mut out, "impl", span);
    push_path(&mut out, &["gpu", "Output"], span);
    push_ident(&mut out, "for", span);
    out.extend([TokenTree::Ident(name.clone())]);
    let mut obody = TokenStream::new();
    push_ident(&mut obody, "const", span);
    push_ident(&mut obody, "OUT", span);
    push_punct(&mut obody, ':', Spacing::Alone, span);
    push_path(&mut obody, &["gpu", "Out"], span);
    push_punct(&mut obody, '=', Spacing::Alone, span);
    push_path(&mut obody, &["gpu", "Out", "Varyings"], span);
    let mut ofields = TokenStream::new();
    push_varyings_fields_expr(&mut ofields, name, span);
    push_group(&mut obody, Delimiter::Parenthesis, ofields, span);
    push_punct(&mut obody, ';', Spacing::Alone, span);
    push_group(&mut out, Delimiter::Brace, obody, span);

    Ok(out)
}

fn push_varyings_fields_expr(out: &mut TokenStream, name: &Ident, span: Span) {
    push_punct(out, '<', Spacing::Alone, span);
    out.extend([TokenTree::Ident(name.clone())]);
    push_ident(out, "as", span);
    push_path(out, &["gpu", "Varyings"], span);
    push_punct(out, '>', Spacing::Alone, span);
    push_colon2(out, span);
    push_ident(out, "FIELDS", span);
}

fn has_repr_c_or_transparent(attrs: &[Group]) -> bool {
    for attr in attrs {
        let tokens: Vec<TokenTree> = attr.stream().into_iter().collect();
        if tokens.len() == 2
            && is_ident(&tokens[0], "repr")
            && let TokenTree::Group(args) = &tokens[1]
            && args.delimiter() == Delimiter::Parenthesis
        {
            let inner: Vec<TokenTree> = args.stream().into_iter().collect();
            for part in split_top_level_commas(&inner) {
                if let Some(first) = part.first()
                    && (is_ident(first, "C") || is_ident(first, "transparent"))
                {
                    return true;
                }
            }
        }
    }
    false
}

fn expand_pod(input: TokenStream) -> Result<TokenStream, Error> {
    let s = parse_derive_input(input, "Pod")?;
    let name = &s.name;
    let span = name.span();
    if let Some(gspan) = s.generic_span {
        return Err(Error::new(gspan, format!("#[derive(gpu::Pod)] {name}: not generic")));
    }
    if !has_repr_c_or_transparent(&s.attrs) {
        return Err(Error::new(
            span,
            format!("#[derive(gpu::Pod)] {name}: needs #[repr(C)], so host and device lay it out the same"),
        ));
    }
    if !s.is_struct {
        return Err(Error::new(span, format!("#[derive(gpu::Pod)] {name}: a struct")));
    }
    let field_tys: Vec<&[TokenTree]> = match &s.fields {
        StructFields::Named(fs) | StructFields::Unnamed(fs) => fs.iter().map(|f| f.ty.as_slice()).collect(),
        StructFields::Unit => Vec::new(),
    };
    let padding = format!("#[derive(gpu::Pod)] {name}: has padding, which the host would write as garbage");

    let mut out = TokenStream::new();

    // const _: () = {
    //     const fn pod<T: ::gpu::Pod>() {}
    //     pod::<T0>(); ...
    //     ::core::assert!(::core::mem::size_of::<#name>() == 0 + ::core::mem::size_of::<T0>() + ..., #padding);
    // };
    push_ident(&mut out, "const", span);
    push_ident(&mut out, "_", span);
    push_punct(&mut out, ':', Spacing::Alone, span);
    push_group(&mut out, Delimiter::Parenthesis, TokenStream::new(), span);
    push_punct(&mut out, '=', Spacing::Alone, span);

    let mut block = TokenStream::new();
    // const fn pod<T: ::gpu::Pod>() {}
    push_ident(&mut block, "const", span);
    push_ident(&mut block, "fn", span);
    push_ident(&mut block, "pod", span);
    push_punct(&mut block, '<', Spacing::Alone, span);
    push_ident(&mut block, "T", span);
    push_punct(&mut block, ':', Spacing::Alone, span);
    push_path(&mut block, &["gpu", "Pod"], span);
    push_punct(&mut block, '>', Spacing::Alone, span);
    push_group(&mut block, Delimiter::Parenthesis, TokenStream::new(), span);
    push_group(&mut block, Delimiter::Brace, TokenStream::new(), span);

    // pod::<#ty>();
    for ty in &field_tys {
        push_ident(&mut block, "pod", span);
        push_colon2(&mut block, span);
        push_punct(&mut block, '<', Spacing::Alone, span);
        push_tokens(&mut block, ty);
        push_punct(&mut block, '>', Spacing::Alone, span);
        push_group(&mut block, Delimiter::Parenthesis, TokenStream::new(), span);
        push_punct(&mut block, ';', Spacing::Alone, span);
    }

    // ::core::assert!(::core::mem::size_of::<#name>() == 0 + ..., #padding);
    push_path(&mut block, &["core", "assert"], span);
    push_punct(&mut block, '!', Spacing::Alone, span);
    let mut assert_args = TokenStream::new();
    push_path(&mut assert_args, &["core", "mem", "size_of"], span);
    push_colon2(&mut assert_args, span);
    push_punct(&mut assert_args, '<', Spacing::Alone, span);
    assert_args.extend([TokenTree::Ident(name.clone())]);
    push_punct(&mut assert_args, '>', Spacing::Alone, span);
    push_group(&mut assert_args, Delimiter::Parenthesis, TokenStream::new(), span);
    push_punct(&mut assert_args, '=', Spacing::Joint, span);
    push_punct(&mut assert_args, '=', Spacing::Alone, span);
    push_lit_usize(&mut assert_args, 0, span);
    for ty in &field_tys {
        push_punct(&mut assert_args, '+', Spacing::Alone, span);
        push_path(&mut assert_args, &["core", "mem", "size_of"], span);
        push_colon2(&mut assert_args, span);
        push_punct(&mut assert_args, '<', Spacing::Alone, span);
        push_tokens(&mut assert_args, ty);
        push_punct(&mut assert_args, '>', Spacing::Alone, span);
        push_group(&mut assert_args, Delimiter::Parenthesis, TokenStream::new(), span);
    }
    push_punct(&mut assert_args, ',', Spacing::Alone, span);
    push_lit_str(&mut assert_args, &padding, span);
    push_group(&mut block, Delimiter::Parenthesis, assert_args, span);
    push_punct(&mut block, ';', Spacing::Alone, span);

    push_group(&mut out, Delimiter::Brace, block, span);
    push_punct(&mut out, ';', Spacing::Alone, span);

    // unsafe impl ::gpu::Pod for #name {}
    push_ident(&mut out, "unsafe", span);
    push_ident(&mut out, "impl", span);
    push_path(&mut out, &["gpu", "Pod"], span);
    push_ident(&mut out, "for", span);
    out.extend([TokenTree::Ident(name.clone())]);
    push_group(&mut out, Delimiter::Brace, TokenStream::new(), span);

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::parse_u32_literal;

    #[test]
    fn parses_decimal_u32_literals() {
        assert_eq!(parse_u32_literal("64"), Some(64));
        assert_eq!(parse_u32_literal("1_024"), Some(1024));
        assert_eq!(parse_u32_literal("8u32"), Some(8));
        assert_eq!(parse_u32_literal("0"), Some(0));
        assert_eq!(parse_u32_literal("4294967295"), Some(u32::MAX));
        assert_eq!(parse_u32_literal("4294967296"), None);
        assert_eq!(parse_u32_literal("0x10"), None);
        assert_eq!(parse_u32_literal("8i32"), None);
        assert_eq!(parse_u32_literal("1.5"), None);
        assert_eq!(parse_u32_literal("_"), None);
        assert_eq!(parse_u32_literal(""), None);
    }
}
