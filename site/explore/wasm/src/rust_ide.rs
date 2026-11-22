//! Compiler-backed Rust Post-IDE engine for Blaze Explore.
//!
//! Directly reuses Blaze's own compiler crates with zero duplicated lexing or item parsing:
//!   - Lexing & token classification: `blaze_lex::tokenize` (exact `rustc_lexer` token kinds).
//!   - File item / struct field / enum variant / impl / `use` / `mod` graph: ingested directly from
//!     Blaze's `blaze-file-items` + `blaze-surface` (`outline` + `impl_header`).

use std::collections::HashMap;

use blaze_lex::{Cpu, FrontmatterAllowed, TokenKind, tokenize};

#[derive(Clone, Debug)]
pub struct FieldDef {
    pub name: String,
    pub ty: String,
    pub doc: String,
    pub line: u32,
}

#[derive(Clone, Debug)]
pub struct ImplSpan {
    pub self_ty: String,
    pub start_line: u32,
    pub end_line: u32,
    pub line: u32,
}

#[derive(Clone, Debug)]
pub struct SymDef {
    pub kind: String, // "fn", "method", "struct", "enum", "field", "variant", "const", "param", "local", "self", "alloc", "type"
    pub name: String,
    pub sig: String,
    pub doc: String,
    pub file: String,
    pub line: u32,
    pub start_line: u32,
    pub end_line: u32,
    pub owner: Option<String>,
    pub target_type: Option<String>,
    pub fields: Vec<FieldDef>,
    pub snippet: Vec<(u32, String)>,
}

#[derive(Clone, Debug)]
pub struct ImportTarget {
    pub path: String,
    pub orig_name: String,
}

#[derive(Clone, Debug, Default)]
pub struct FileGraph {
    pub defs: Vec<SymDef>,
    pub impls: Vec<ImplSpan>,
    pub imports: HashMap<String, ImportTarget>,
    pub submods: Vec<String>,
}

/// Raw item entry emitted by Blaze's `blaze-file-items` (`explore_graph`).
pub struct CompilerItemRow {
    pub kind: String,
    pub name: String,
    pub owner: String,
    pub sig: String,
    pub ty: String,
    pub doc: String,
    pub line: u32,
    pub start_line: u32,
    pub end_line: u32,
}

impl FileGraph {
    /// Builds `FileGraph` directly from Blaze's `blaze-file-items` + `blaze-surface` output.
    pub fn from_compiler(
        file: &str,
        lines: &[String],
        rows: Vec<CompilerItemRow>,
        uses: Vec<String>,
        submods: Vec<String>,
    ) -> Self {
        let mut graph = FileGraph {
            defs: Vec::with_capacity(rows.len()),
            impls: Vec::new(),
            imports: HashMap::new(),
            submods,
        };
        for u in &uses {
            parse_use_tree(u, &mut graph.imports);
        }

        // First pass: collect top-level items & impl spans
        let mut field_rows = Vec::new();
        for r in rows {
            if r.kind == "impl" {
                graph.impls.push(ImplSpan {
                    self_ty: r.name,
                    start_line: r.start_line,
                    end_line: r.end_line,
                    line: r.line,
                });
                continue;
            }
            if r.kind == "field" || r.kind == "variant" {
                field_rows.push(r);
                continue;
            }
            let snippet = snippet_around(lines, r.line, 5);
            graph.defs.push(SymDef {
                kind: r.kind,
                name: r.name,
                sig: r.sig,
                doc: r.doc,
                file: file.to_string(),
                line: r.line,
                start_line: r.start_line,
                end_line: r.end_line,
                owner: if r.owner.is_empty() { None } else { Some(r.owner) },
                target_type: None,
                fields: Vec::new(),
                snippet,
            });
        }

        // Second pass: attach compiler-parsed `Field` & `Variant` children to their parent `Struct` / `Enum`
        for f in field_rows {
            if f.owner.is_empty() {
                continue;
            }
            if let Some(parent) = graph
                .defs
                .iter_mut()
                .find(|d| (d.kind == "struct" || d.kind == "enum") && d.name == f.owner)
            {
                parent.fields.push(FieldDef {
                    name: f.name,
                    ty: f.ty,
                    doc: f.doc,
                    line: f.line,
                });
            }
        }
        graph
    }
}

#[derive(Clone, Debug)]
pub struct IdentHit {
    pub name: String,
    pub col_start: usize,
    pub col_end: usize,
    pub is_call: bool,
    pub receiver: Option<String>,
    pub path_prefix: Option<String>,
    pub is_doc_link: bool,
}

#[derive(Clone, Debug)]
pub struct CrossFileQuery {
    pub ident: String,
    pub prefix: Option<String>,
    pub import_path: Option<String>,
    pub want_kind: &'static str, // "any", "fn", "type", "member"
}

pub enum ResolveOutcome {
    Resolved(SymDef),
    NeedCrossFile(CrossFileQuery),
    None,
}

/// Tokenizes the line using Blaze's own `blaze_lex::tokenize` and returns the identifier under `char_col`.
/// Comments (except explicit Rustdoc ``[`...`]`` intra-doc links), strings, literals, lifetimes, and
/// `#[...]` attributes are skipped using exact `blaze_lex::TokenKind` classification.
pub fn ident_at_col(lines: &[String], line_idx: usize, char_col: usize) -> Option<IdentHit> {
    let line = lines.get(line_idx)?;
    let chars: Vec<(usize, char)> = line.char_indices().collect();
    if char_col >= chars.len() {
        return None;
    }
    let target_byte = chars[char_col].0;

    // Lex the line with Blaze's compiler lexer (`blaze_lex::tokenize`)!
    let mut off = 0usize;
    let mut attr_depth = 0i32;
    let mut toks = tokenize(line, FrontmatterAllowed::No, Cpu::SCALAR).peekable();
    while let Some(tok) = toks.next() {
        let start = off;
        let end = (off + tok.len as usize).min(line.len());
        off = end;

        // Track `#[...]` / `#![...]` attributes via exact `TokenKind`
        match tok.kind {
            TokenKind::Pound => {
                if matches!(toks.peek().map(|t| t.kind), Some(TokenKind::OpenBracket | TokenKind::Bang)) {
                    attr_depth = attr_depth.max(1);
                }
            }
            TokenKind::OpenBracket if attr_depth > 0 => attr_depth += 1,
            TokenKind::CloseBracket if attr_depth > 0 => {
                attr_depth -= 1;
                if attr_depth == 1 {
                    attr_depth = 0;
                }
            }
            _ => {}
        }

        if target_byte < start || target_byte >= end {
            continue;
        }

        match tok.kind {
            // Doc comment (`///` or `//!`): ONLY allow explicit Rustdoc intra-doc links ``[`...`]``!
            TokenKind::LineComment { doc_style: Some(_) } => {
                return extract_doc_link_ident(line, &chars, char_col);
            }
            // Real code identifier (outside `#[...]` attributes)
            TokenKind::Ident | TokenKind::RawIdent if attr_depth == 0 => {
                let raw = &line[start..end];
                let name = raw.strip_prefix("r#").unwrap_or(raw);
                if name.is_empty() || is_rust_keyword(name) {
                    return None;
                }
                let col_start = byte_to_char_col(&chars, start);
                let col_end = byte_to_char_col(&chars, end);
                let before = line[..start].trim_end();
                let mut receiver = None;
                let mut path_prefix = None;
                if let Some(prev) = before.strip_suffix("::") {
                    path_prefix = take_trailing_ident(prev.trim_end());
                } else if let Some(prev) = before.strip_suffix('.') {
                    if !prev.ends_with('.') {
                        receiver = take_trailing_ident(prev.trim_end());
                    }
                }
                let rest = line[end..].trim_start();
                let is_call = rest.starts_with('(') || rest.starts_with('!') || rest.starts_with("::<");
                return Some(IdentHit {
                    name: name.to_string(),
                    col_start,
                    col_end,
                    is_call,
                    receiver,
                    path_prefix,
                    is_doc_link: false,
                });
            }
            // Plain comments, block comments, strings, raw strings, char literals, lifetimes, attributes -> skip!
            _ => return None,
        }
    }
    None
}

fn extract_doc_link_ident(line: &str, chars: &[(usize, char)], char_col: usize) -> Option<IdentHit> {
    let mut l = char_col;
    let mut found_open = false;
    while l > 0 {
        if chars[l].1 == '`' && chars[l - 1].1 == '[' {
            found_open = true;
            break;
        }
        if chars[l].1 == ']' || chars[l].1 == ' ' {
            break;
        }
        l -= 1;
    }
    let mut r = char_col;
    let mut found_close = false;
    while r + 1 < chars.len() {
        if chars[r].1 == '`' && chars[r + 1].1 == ']' {
            found_close = true;
            break;
        }
        if chars[r].1 == '[' || chars[r].1 == ' ' {
            break;
        }
        r += 1;
    }
    if !(found_open && found_close) {
        return None;
    }
    let ch = chars[char_col].1;
    if !(ch.is_ascii_alphanumeric() || ch == '_') {
        return None;
    }
    let mut s = char_col;
    while s > 0 && (chars[s - 1].1.is_ascii_alphanumeric() || chars[s - 1].1 == '_') {
        s -= 1;
    }
    let mut e = char_col + 1;
    while e < chars.len() && (chars[e].1.is_ascii_alphanumeric() || chars[e].1 == '_') {
        e += 1;
    }
    let b0 = chars[s].0;
    let b1 = if e < chars.len() { chars[e].0 } else { line.len() };
    let name = &line[b0..b1];
    if name.is_empty() || is_rust_keyword(name) {
        return None;
    }
    let before = line[..b0].trim_end();
    let path_prefix = before
        .strip_suffix("::")
        .and_then(|p| take_trailing_ident(p.trim_end()));
    Some(IdentHit {
        name: name.to_string(),
        col_start: s,
        col_end: e,
        is_call: false,
        receiver: None,
        path_prefix,
        is_doc_link: true,
    })
}

fn byte_to_char_col(chars: &[(usize, char)], byte_off: usize) -> usize {
    chars
        .binary_search_by_key(&byte_off, |(b, _)| *b)
        .unwrap_or(chars.len())
}

/// Detects common Rust heap allocation calls on a source line for subtle inline inlay hints.
pub fn find_alloc_hint(line: &str) -> Option<&'static str> {
    let trimmed = line.trim();
    if trimmed.starts_with("//") {
        return None;
    }
    if trimmed.contains("Vec::with_capacity") || trimmed.contains("Vec::new") || trimmed.contains("vec![") {
        Some("heap alloc · Vec")
    } else if trimmed.contains("Box::new") {
        Some("heap alloc · Box")
    } else if trimmed.contains("Arc::new") {
        Some("heap alloc · Arc")
    } else if trimmed.contains("String::with_capacity")
        || trimmed.contains("String::from")
        || trimmed.contains(".to_string()")
        || trimmed.contains("format!(")
    {
        Some("heap alloc · String")
    } else if trimmed.contains("HashMap::new")
        || trimmed.contains("HashMap::with_capacity")
        || trimmed.contains("BTreeMap::new")
    {
        Some("heap alloc · Map")
    } else if trimmed.contains(".to_vec()") || trimmed.contains(".to_owned()") {
        Some("heap alloc · clone")
    } else {
        None
    }
}

/// Strictly scoped identifier resolution backed directly by Blaze's `FileGraph` (`blaze-file-items`).
pub fn resolve_hit(
    file: &str,
    lines: &[String],
    graph: &FileGraph,
    line_no: u32,
    hit: &IdentHit,
) -> ResolveOutcome {
    let idx = (line_no as usize).saturating_sub(1).min(lines.len().saturating_sub(1));
    let defs = &graph.defs;

    // 1. Receiver member / method access (`recv.field` or `recv.method()`)
    if let Some(ref recv) = hit.receiver {
        if let Some(recv_ty) = resolve_expr_type_name(lines, graph, line_no, idx, recv) {
            let base_ty = extract_base_type(&recv_ty);
            if hit.is_call {
                if let Some(m) = defs
                    .iter()
                    .find(|d| d.name == hit.name && d.owner.as_deref() == Some(base_ty))
                {
                    return ResolveOutcome::Resolved(m.clone());
                }
                if let Some(imp) = graph.imports.get(base_ty) {
                    return ResolveOutcome::NeedCrossFile(CrossFileQuery {
                        ident: hit.name.clone(),
                        prefix: Some(imp.orig_name.clone()),
                        import_path: Some(imp.path.clone()),
                        want_kind: "fn",
                    });
                }
                if let Some(b) = builtin_std_hint(&hit.name) {
                    return ResolveOutcome::Resolved(b);
                }
                return ResolveOutcome::None;
            } else {
                if let Some(td) = defs.iter().find(|d| d.kind == "struct" && d.name == base_ty) {
                    if let Some(fld) = td.fields.iter().find(|f| f.name == hit.name) {
                        return ResolveOutcome::Resolved(SymDef {
                            kind: "field".into(),
                            name: format!("{base_ty}::{}", fld.name),
                            sig: format!("{}: {}", fld.name, fld.ty),
                            doc: fld.doc.clone(),
                            file: file.into(),
                            line: fld.line,
                            start_line: fld.line,
                            end_line: fld.line,
                            owner: Some(base_ty.into()),
                            target_type: Some(extract_base_type(&fld.ty).to_string()),
                            fields: Vec::new(),
                            snippet: snippet_around(lines, fld.line, 5),
                        });
                    }
                }
                if let Some(imp) = graph.imports.get(base_ty) {
                    return ResolveOutcome::NeedCrossFile(CrossFileQuery {
                        ident: hit.name.clone(),
                        prefix: Some(imp.orig_name.clone()),
                        import_path: Some(imp.path.clone()),
                        want_kind: "member",
                    });
                }
                return ResolveOutcome::None;
            }
        }
        if let Some(b) = builtin_std_hint(&hit.name) {
            return ResolveOutcome::Resolved(b);
        }
        return ResolveOutcome::None;
    }

    // 2. Qualified path access (`Prefix::name`, e.g. `Event::cpu`, `RingPlant::None`, `Vec::with_capacity`)
    if let Some(ref pref) = hit.path_prefix {
        let base_pref = if pref == "Self" {
            enclosing_impl(graph, line_no)
                .map(|im| im.self_ty.clone())
                .unwrap_or_else(|| pref.clone())
        } else {
            pref.clone()
        };
        if let Some(m) = defs
            .iter()
            .find(|d| d.name == hit.name && d.owner.as_deref() == Some(base_pref.as_str()))
        {
            return ResolveOutcome::Resolved(m.clone());
        }
        if let Some(td) = defs
            .iter()
            .find(|d| (d.kind == "struct" || d.kind == "enum") && d.name == base_pref)
        {
            if let Some(fld) = td.fields.iter().find(|f| f.name == hit.name) {
                let kind = if td.kind == "enum" { "variant" } else { "field" };
                let sig = if td.kind == "enum" {
                    format!("{base_pref}::{}", fld.name)
                } else {
                    format!("{}: {}", fld.name, fld.ty)
                };
                return ResolveOutcome::Resolved(SymDef {
                    kind: kind.into(),
                    name: format!("{base_pref}::{}", fld.name),
                    sig,
                    doc: fld.doc.clone(),
                    file: file.into(),
                    line: fld.line,
                    start_line: fld.line,
                    end_line: fld.line,
                    owner: Some(base_pref),
                    target_type: None,
                    fields: Vec::new(),
                    snippet: snippet_around(lines, fld.line, 5),
                });
            }
        }
        if let Some(imp) = graph.imports.get(&base_pref) {
            return ResolveOutcome::NeedCrossFile(CrossFileQuery {
                ident: hit.name.clone(),
                prefix: Some(imp.orig_name.clone()),
                import_path: Some(imp.path.clone()),
                want_kind: "member",
            });
        }
        if let Some(b) = builtin_std_hint(&hit.name) {
            return ResolveOutcome::Resolved(b);
        }
        return ResolveOutcome::NeedCrossFile(CrossFileQuery {
            ident: hit.name.clone(),
            prefix: Some(base_pref),
            import_path: None,
            want_kind: if hit.is_call { "fn" } else { "any" },
        });
    }

    // 3. `self` / `Self` -> directly from Blaze's `impl_header` span!
    if hit.name == "self" || hit.name == "Self" {
        if let Some(im) = enclosing_impl(graph, line_no) {
            let base = im.self_ty.as_str();
            let td = defs
                .iter()
                .find(|d| (d.kind == "struct" || d.kind == "enum") && d.name == base);
            let receiver = enclosing_fn(graph, line_no)
                .and_then(|f| fn_receiver(&f.sig))
                .unwrap_or("&self");
            let sig = if hit.name == "Self" {
                format!("type Self = {base}")
            } else if receiver.contains("mut") {
                format!("self: &mut {base}")
            } else if receiver.contains('&') {
                format!("self: &{base}")
            } else {
                format!("self: {base}")
            };
            return ResolveOutcome::Resolved(SymDef {
                kind: "self".into(),
                name: hit.name.clone(),
                sig,
                doc: td.map(|d| d.doc.clone()).unwrap_or_default(),
                file: file.into(),
                line: td.map(|d| d.line).unwrap_or(im.line),
                start_line: td.map(|d| d.start_line).unwrap_or(im.start_line),
                end_line: td.map(|d| d.end_line).unwrap_or(im.end_line),
                owner: None,
                target_type: Some(base.to_string()),
                fields: Vec::new(),
                snippet: td.map(|d| d.snippet.clone()).unwrap_or_default(),
            });
        }
        return ResolveOutcome::None;
    }

    // 4. Function parameter (from Blaze's parsed `fn` signature!) or local `let` binding
    if !hit.is_call && !hit.is_doc_link {
        if let Some(efn) = enclosing_fn(graph, line_no) {
            if let Some(ty) = find_param_type(&efn.sig, &hit.name) {
                let base = extract_base_type(&ty);
                let td = defs
                    .iter()
                    .find(|d| (d.kind == "struct" || d.kind == "enum" || d.kind == "type") && d.name == base);
                return ResolveOutcome::Resolved(SymDef {
                    kind: "param".into(),
                    name: hit.name.clone(),
                    sig: format!("{}: {ty}", hit.name),
                    doc: td.map(|d| d.doc.clone()).unwrap_or_default(),
                    file: file.into(),
                    line: td.map(|d| d.line).unwrap_or(efn.line),
                    start_line: efn.start_line,
                    end_line: efn.end_line,
                    owner: None,
                    target_type: if base.is_empty() { None } else { Some(base.to_string()) },
                    fields: Vec::new(),
                    snippet: td.map(|d| d.snippet.clone()).unwrap_or_default(),
                });
            }
            let fn_start = (efn.line as usize).saturating_sub(1);
            if let Some((let_line, ty_inferred, expr)) = find_local_let(lines, fn_start, idx, &hit.name, defs) {
                let base = extract_base_type(&ty_inferred);
                let td = defs
                    .iter()
                    .find(|d| (d.kind == "struct" || d.kind == "enum" || d.kind == "type") && d.name == base);
                return ResolveOutcome::Resolved(SymDef {
                    kind: "local".into(),
                    name: hit.name.clone(),
                    sig: if ty_inferred.is_empty() {
                        format!("let {} = {expr}", hit.name)
                    } else {
                        format!("let {}: {ty_inferred}", hit.name)
                    },
                    doc: td.map(|d| d.doc.clone()).unwrap_or_default(),
                    file: file.into(),
                    line: td.map(|d| d.line).unwrap_or((let_line + 1) as u32),
                    start_line: (let_line + 1) as u32,
                    end_line: (let_line + 1) as u32,
                    owner: None,
                    target_type: if base.is_empty() { None } else { Some(base.to_string()) },
                    fields: Vec::new(),
                    snippet: td.map(|d| d.snippet.clone()).unwrap_or_default(),
                });
            }
        }
    }

    // 5. Same-file definitions from Blaze's `FileItems` outline
    if hit.is_call {
        if let Some(d) = defs
            .iter()
            .find(|d| (d.kind == "fn" || d.kind == "method") && d.name == hit.name)
        {
            return ResolveOutcome::Resolved(d.clone());
        }
    }
    if let Some(d) = defs.iter().find(|d| d.name == hit.name && d.owner.is_none()) {
        return ResolveOutcome::Resolved(d.clone());
    }
    if let Some(d) = defs.iter().find(|d| d.name == hit.name) {
        return ResolveOutcome::Resolved(d.clone());
    }

    // 6. Explicit `use` import (from Blaze's `ItemDetail::Use`)
    if let Some(imp) = graph.imports.get(&hit.name) {
        return ResolveOutcome::NeedCrossFile(CrossFileQuery {
            ident: imp.orig_name.clone(),
            prefix: None,
            import_path: Some(imp.path.clone()),
            want_kind: if hit.is_call { "fn" } else { "any" },
        });
    }

    // 7. Built-in Rust std / primitive hints
    if let Some(b) = builtin_std_hint(&hit.name) {
        return ResolveOutcome::Resolved(b);
    }

    // 8. Scoped crate-local / Cargo.toml dep lookup ONLY for real calls, doc links, or PascalCase/CONST items
    let first_ch = hit.name.chars().next().unwrap_or('a');
    let is_const_or_type = first_ch.is_ascii_uppercase();
    if hit.is_call || hit.is_doc_link || is_const_or_type {
        return ResolveOutcome::NeedCrossFile(CrossFileQuery {
            ident: hit.name.clone(),
            prefix: None,
            import_path: None,
            want_kind: if hit.is_call {
                "fn"
            } else if is_const_or_type {
                "type"
            } else {
                "any"
            },
        });
    }

    ResolveOutcome::None
}

fn enclosing_impl(graph: &FileGraph, line_no: u32) -> Option<&ImplSpan> {
    graph
        .impls
        .iter()
        .find(|im| line_no >= im.start_line && line_no <= im.end_line)
}

fn enclosing_fn(graph: &FileGraph, line_no: u32) -> Option<&SymDef> {
    graph
        .defs
        .iter()
        .filter(|d| (d.kind == "fn" || d.kind == "method") && line_no >= d.start_line && line_no <= d.end_line)
        .min_by_key(|d| d.end_line.saturating_sub(d.start_line))
}

fn resolve_expr_type_name(
    lines: &[String],
    graph: &FileGraph,
    line_no: u32,
    idx: usize,
    expr: &str,
) -> Option<String> {
    if expr == "self" {
        return enclosing_impl(graph, line_no).map(|im| im.self_ty.clone());
    }
    if let Some(efn) = enclosing_fn(graph, line_no) {
        if let Some(ty) = find_param_type(&efn.sig, expr) {
            return Some(ty);
        }
        let fn_start = (efn.line as usize).saturating_sub(1);
        if let Some((_, ty, _)) = find_local_let(lines, fn_start, idx, expr, &graph.defs) {
            if !ty.is_empty() {
                return Some(ty);
            }
        }
    }
    None
}

fn snippet_around(lines: &[String], line_no: u32, count: usize) -> Vec<(u32, String)> {
    let start = (line_no as usize).saturating_sub(2).min(lines.len().saturating_sub(1));
    let end = (start + count).min(lines.len());
    let mut out = Vec::with_capacity(end - start);
    for i in start..end {
        out.push(((i + 1) as u32, lines[i].clone()));
    }
    out
}

fn parse_use_tree(tree: &str, imports: &mut HashMap<String, ImportTarget>) {
    let rest = tree.trim().trim_end_matches(';').trim();
    if let Some((prefix, group)) = rest.split_once("::{") {
        let inner = group.trim_end_matches('}').trim();
        for part in inner.split(',') {
            let p = part.trim();
            if p.is_empty() || p == "self" || p == "*" {
                continue;
            }
            if let Some((orig, alias)) = p.split_once(" as ") {
                imports.insert(
                    alias.trim().to_string(),
                    ImportTarget {
                        path: prefix.trim().to_string(),
                        orig_name: orig.trim().to_string(),
                    },
                );
            } else {
                imports.insert(
                    p.to_string(),
                    ImportTarget {
                        path: prefix.trim().to_string(),
                        orig_name: p.to_string(),
                    },
                );
            }
        }
    } else if let Some((full, alias)) = rest.split_once(" as ") {
        if let Some((prefix, orig)) = full.trim().rsplit_once("::") {
            imports.insert(
                alias.trim().to_string(),
                ImportTarget {
                    path: prefix.trim().to_string(),
                    orig_name: orig.trim().to_string(),
                },
            );
        }
    } else if let Some((prefix, item)) = rest.rsplit_once("::") {
        let item = item.trim();
        if !item.is_empty() && item != "*" && item != "self" {
            imports.insert(
                item.to_string(),
                ImportTarget {
                    path: prefix.trim().to_string(),
                    orig_name: item.to_string(),
                },
            );
        }
    }
}

fn take_trailing_ident(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut i = bytes.len();
    while i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_') {
        i -= 1;
    }
    if i == bytes.len() {
        None
    } else {
        Some(s[i..].to_string())
    }
}

fn take_ident(s: &str) -> Option<String> {
    let s = s.trim_start();
    let end = s
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(s.len());
    if end == 0 { None } else { Some(s[..end].to_string()) }
}

fn is_rust_keyword(name: &str) -> bool {
    matches!(
        name,
        "fn" | "pub"
            | "let"
            | "mut"
            | "const"
            | "static"
            | "struct"
            | "enum"
            | "impl"
            | "trait"
            | "for"
            | "in"
            | "while"
            | "loop"
            | "if"
            | "else"
            | "match"
            | "return"
            | "break"
            | "continue"
            | "use"
            | "mod"
            | "crate"
            | "super"
            | "where"
            | "as"
            | "unsafe"
            | "async"
            | "await"
            | "ref"
            | "move"
            | "type"
            | "true"
            | "false"
    )
}

fn fn_receiver(sig: &str) -> Option<&'static str> {
    let p0 = sig.find('(')?;
    let p1 = sig.find(')')?;
    let params = &sig[p0 + 1..p1];
    if params.contains("&mut self") {
        Some("&mut self")
    } else if params.contains("&self") {
        Some("&self")
    } else if params.contains("self") {
        Some("self")
    } else {
        None
    }
}

fn find_param_type(fn_sig: &str, ident: &str) -> Option<String> {
    let p0 = fn_sig.find('(')?;
    let p1 = fn_sig.rfind(')')?;
    if p1 <= p0 {
        return None;
    }
    let params = &fn_sig[p0 + 1..p1];
    for part in split_top_level(params, ',') {
        let p = part.trim();
        let p = p.strip_prefix("mut ").unwrap_or(p).trim();
        if let Some((lhs, rhs)) = p.split_once(':') {
            if lhs.trim() == ident {
                return Some(rhs.trim().to_string());
            }
        }
    }
    None
}

fn find_local_let(
    lines: &[String],
    fn_start: usize,
    cur_idx: usize,
    ident: &str,
    defs: &[SymDef],
) -> Option<(usize, String, String)> {
    let mut i = cur_idx;
    while i > fn_start {
        let t = lines[i].trim();
        if let Some(rest) = t.strip_prefix("let ") {
            let rest = rest.trim_start();
            let rest = rest.strip_prefix("mut ").unwrap_or(rest).trim_start();
            if let Some(after) = rest.strip_prefix(ident) {
                let first = after.chars().next().unwrap_or(' ');
                if !(first.is_ascii_alphanumeric() || first == '_') {
                    let after = after.trim_start();
                    if let Some(t_rest) = after.strip_prefix(':') {
                        let ty = t_rest.split('=').next().unwrap_or("").trim().trim_end_matches(';');
                        return Some((i, ty.to_string(), String::new()));
                    }
                    if let Some(eq_rest) = after.strip_prefix('=') {
                        let expr = eq_rest.trim().trim_end_matches(';').trim();
                        let inferred = infer_expr_type(expr, defs);
                        return Some((i, inferred, expr.to_string()));
                    }
                }
            }
        }
        i -= 1;
    }
    None
}

fn infer_expr_type(expr: &str, defs: &[SymDef]) -> String {
    if expr.starts_with("Vec::") || expr.starts_with("vec![") {
        return "Vec<_>".into();
    }
    if expr.starts_with("String::") || expr.starts_with("format!(") || expr.ends_with(".to_string()") {
        return "String".into();
    }
    if expr.starts_with("Box::new") {
        return "Box<_>".into();
    }
    if expr.starts_with("HashMap::") {
        return "HashMap<_, _>".into();
    }
    if expr.ends_with(".len()") {
        return "usize".into();
    }
    if let Some(rest) = expr.strip_prefix("self.") {
        if let Some(mname) = take_ident(rest) {
            if let Some(fd) = defs
                .iter()
                .find(|d| (d.kind == "fn" || d.kind == "method") && d.name == mname)
            {
                if let Some((_, ret)) = fd.sig.split_once("->") {
                    return ret.trim().to_string();
                }
            }
        }
    }
    if let Some((_, rhs)) = expr.rsplit_once(" as ") {
        return rhs.trim().to_string();
    }
    String::new()
}

fn split_top_level(s: &str, delim: char) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (i, ch) in s.char_indices() {
        match ch {
            '<' | '(' | '[' => depth += 1,
            '>' | ')' | ']' => depth -= 1,
            c if c == delim && depth == 0 => {
                out.push(&s[start..i]);
                start = i + ch.len_utf8();
            }
            _ => {}
        }
    }
    if start <= s.len() {
        out.push(&s[start..]);
    }
    out
}

fn extract_base_type(ty: &str) -> &str {
    let t = ty
        .trim()
        .trim_start_matches('&')
        .trim()
        .trim_start_matches("mut ")
        .trim();
    let t = t.split('<').next().unwrap_or(t).trim();
    let t = t.rsplit("::").next().unwrap_or(t).trim();
    t.trim_matches(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
}

fn builtin_std_hint(ident: &str) -> Option<SymDef> {
    let (kind, sig, doc) = match ident {
        "Vec" => ("struct", "pub struct Vec<T, A = Global>", "Heap-allocated contiguous growable array."),
        "with_capacity" => ("alloc", "pub fn with_capacity(capacity: usize) -> Self", "Allocates a heap buffer upfront for `capacity` elements."),
        "String" => ("struct", "pub struct String", "UTF-8 encoded, heap-allocated growable string."),
        "Box" => ("struct", "pub struct Box<T: ?Sized>", "Single-owner heap allocation pointer."),
        "Arc" => ("struct", "pub struct Arc<T: ?Sized>", "Thread-safe atomically reference-counted heap pointer."),
        "Mutex" => ("struct", "pub struct Mutex<T: ?Sized>", "Mutual exclusion lock; contention blocks waiter threads."),
        "Ordering" => ("enum", "pub enum Ordering { Relaxed, Release, Acquire, AcqRel, SeqCst }", "Atomic memory ordering constraint."),
        "Option" => ("enum", "pub enum Option<T> { None, Some(T) }", "Optional value."),
        "Result" => ("enum", "pub enum Result<T, E> { Ok(T), Err(E) }", "Recoverable error or success result."),
        "TcpStream" => ("struct", "pub struct TcpStream", "OS TCP socket stream (fd-backed)."),
        "TcpListener" => ("struct", "pub struct TcpListener", "Bound TCP listening socket (accepts inbound connections)."),
        "AtomicU64" | "AtomicU32" | "AtomicBool" | "AtomicUsize" => {
            return Some(SymDef {
                kind: "struct".into(),
                name: ident.into(),
                sig: format!("pub struct {ident}"),
                doc: "Lock-free atomic primitive.".into(),
                file: String::new(),
                line: 0,
                start_line: 0,
                end_line: 0,
                owner: None,
                target_type: None,
                fields: Vec::new(),
                snippet: Vec::new(),
            });
        }
        "u8" | "u16" | "u32" | "u64" | "usize" | "i8" | "i16" | "i32" | "i64" | "isize" | "f32" | "f64" | "bool" => {
            return Some(SymDef {
                kind: "type".into(),
                name: ident.into(),
                sig: format!("primitive {ident}"),
                doc: "Stack value primitive (no heap allocation).".into(),
                file: String::new(),
                line: 0,
                start_line: 0,
                end_line: 0,
                owner: None,
                target_type: None,
                fields: Vec::new(),
                snippet: Vec::new(),
            });
        }
        _ => return None,
    };
    Some(SymDef {
        kind: kind.into(),
        name: ident.into(),
        sig: sig.into(),
        doc: doc.into(),
        file: String::new(),
        line: 0,
        start_line: 0,
        end_line: 0,
        owner: None,
        target_type: None,
        fields: Vec::new(),
        snippet: Vec::new(),
    })
}
