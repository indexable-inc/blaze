//! `doc_names_nothing`: a doc comment that names, in backticks, a path that resolves to
//! nothing from the item it documents. The doc describes code that was renamed, moved or
//! deleted, and a reader (an agent above all, which takes the doc's word and searches for the
//! name) goes looking for what is not there.
//!
//! What counts as a path: the text of a single-backtick span that is a `::`-separated list of
//! identifiers (a trailing `()` and generic arguments dropped), or one CamelCase identifier.
//! Single lowercase words (`len`, `true`), macros (`foo!`), generic parameters (`T::Item`),
//! primitives (`u32::MAX`), explicit links (`` [`x`](..) ``), code blocks and spans of two or
//! more backticks are left alone. Each path is resolved the way rustdoc resolves an
//! intra-doc link, by `hir::resolve_doc_path_on` in the item's own scope (a module's inner doc
//! in the module's), never by matching text.
//!
//! A path that does not resolve is reported when it names nothing at all, or when its first
//! segment resolves and the rest does not (`crate::gone`, `Self::removed`). A single name that
//! is not in scope is reported when no dependency exports it (it names nothing anywhere) or
//! when exactly one item of the crate has it, where the fix (`MaybeIncorrect`: the doc may mean
//! another) writes that item's full path, `crate::module::Name`. A single name in scope
//! nowhere here but exported by a dependency (`HashMap` in prose) is left alone.

use syntax::{
    SyntaxKind, TextRange, TextSize,
    view::{self as ast, AstNode as _},
};

use super::super::{
    Lint,
    std_mistakes::{Fix, report},
    test_code,
};
use crate::{check::Cx, diag::Applicability};

const LINT: Lint = Lint::DocNamesNothing;

/// The primitive types, whose paths (`u32::MAX`, `str::len`) are no item's.
const PRIMITIVES: &[&str] = &[
    "bool", "char", "str", "u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16", "i32", "i64",
    "i128", "isize", "f16", "f32", "f64", "f128",
];

/// What a doc comment is on, the scope its paths resolve in.
#[derive(Clone, Copy)]
enum Owner {
    Def(hir::ModuleDef),
    Impl(hir::Impl),
}

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if node.kind() != SyntaxKind::DOC_COMMENT || !cx.on_blaze(LINT) {
        return;
    }
    let Some(doc) = ast::DocComment::cast(node) else { return };
    let inner = doc.inner_doc_comment_token().is_some();
    let Some(parent) = node.parent() else { return };
    // The owner's doc comments are read once, from the first, for the code blocks across them.
    let same = |it: &ast::Node<'t>| {
        ast::DocComment::cast(*it).is_some_and(|it| it.inner_doc_comment_token().is_some() == inner)
    };
    if parent.children().find(same) != Some(node) {
        return;
    }
    let Some(owner) = owner_of(cx, node, parent, inner) else { return };
    let is_inner = if inner { hir::IsInnerDoc::Yes } else { hir::IsInnerDoc::No };
    // alloc: the findings of one item's doc, reported after the walk of it
    let mut found = Vec::new();
    let mut in_code = false;
    for comment in parent.children().filter(same) {
        let Some(range) = cx.range(comment) else { continue };
        let text = cx.snippet(range);
        let Some(body) = text.strip_prefix("///").or_else(|| text.strip_prefix("//!")) else {
            continue;
        };
        let line = body.strip_prefix(' ').unwrap_or(body);
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_code = !in_code;
            continue;
        }
        if in_code || line.starts_with("    ") {
            continue;
        }
        let base = range.start() + TextSize::of(&text[..text.len() - line.len()]);
        for (start, span) in spans(line) {
            let Some(path) = path_like(span) else { continue };
            let Some(finding) = judge(cx, owner, path, is_inner) else { continue };
            let at = TextRange::at(base + TextSize::of(&line[..start]), TextSize::of(path));
            found.push((comment, at, path.to_owned(), finding));
        }
    }
    let what = owner_name(cx, owner);
    for (comment, at, path, fix) in found {
        let message = format!(
            "this doc names `{path}`, which resolves to nothing from {what}: whoever looks for it \
             finds nothing"
        );
        match fix {
            Some(full) => {
                let note = format!("the one item of that name in the crate is `{full}`");
                let fix = Fix {
                    message: "write the item's full path",
                    // alloc: the fix's one part
                    parts: vec![(at, full)],
                    applicability: Applicability::MaybeIncorrect,
                };
                report(cx, LINT, comment, at, message, &note, Some(fix));
            }
            None => {
                let note = "write the path of the item it means, or say in words what is gone";
                report(cx, LINT, comment, at, message, note, None);
            }
        }
    }
}

/// What the doc comment `node`, a child of `parent`, documents.
fn owner_of<'t>(cx: &Cx<'_, 't>, node: ast::Node<'t>, parent: ast::Node<'t>, inner: bool) -> Option<Owner> {
    if inner {
        return test_code::module_of(cx, node).map(|it| Owner::Def(hir::ModuleDef::Module(it)));
    }
    if parent == cx.root()
        && let Some(analysis) = cx.analysis()
    {
        return match analysis.def() {
            hir::DefWithBody::Function(it) => Some(Owner::Def(hir::ModuleDef::Function(it))),
            hir::DefWithBody::Const(it) => Some(Owner::Def(hir::ModuleDef::Const(it))),
            hir::DefWithBody::Static(it) => Some(Owner::Def(hir::ModuleDef::Static(it))),
            _ => None,
        };
    }
    for at in parent.ancestors() {
        if let Some(imp) = ast::Impl::cast(at) {
            return cx.impl_def(imp).map(Owner::Impl);
        }
        if ast::Item::cast(at).is_some()
            && let Some(def) = cx.def(at)
        {
            return Some(Owner::Def(def));
        }
    }
    None
}

/// The item a message names: `` `name` `` or "this impl".
fn owner_name(cx: &Cx<'_, '_>, owner: Owner) -> String {
    let db = cx.db();
    match owner {
        Owner::Def(def) => match def.name(db) {
            Some(name) => format!("`{}`", name.as_str()),
            None => "the crate root".to_owned(),
        },
        Owner::Impl(_) => "this impl".to_owned(),
    }
}

/// Whether `link` resolves from `owner`'s scope as rustdoc would resolve it.
fn resolves(cx: &Cx<'_, '_>, owner: Owner, link: &str, inner: hir::IsInnerDoc) -> bool {
    let db = cx.db();
    match owner {
        Owner::Def(def) => hir::resolve_doc_path_on(db, def, link, None, inner).is_some(),
        Owner::Impl(imp) => hir::resolve_doc_path_on(db, imp, link, None, inner).is_some(),
    }
}

/// Whether `path` is reported: `None` when it is not, `Some(fix)` when it is, with the one item
/// of the crate by its last segment when there is exactly one.
fn judge(cx: &Cx<'_, '_>, owner: Owner, path: &str, inner: hir::IsInnerDoc) -> Option<Option<String>> {
    if resolves(cx, owner, path, inner) {
        return None;
    }
    // alloc: one path's segments, a few
    let segments: Vec<&str> = path.split("::").collect();
    let (first, last) = (segments[0], segments[segments.len() - 1]);
    let in_crate = crate_items(cx, last);
    let fix = (in_crate.len() == 1).then(|| in_crate[0].clone());
    if segments.len() == 1 {
        return match in_crate.len() {
            1 => Some(fix),
            0 if !exported_by_a_dependency(cx, last) => Some(None),
            _ => None,
        };
    }
    let first_resolves = matches!(first, "crate" | "self" | "super" | "Self")
        || resolves(cx, owner, first, inner);
    if !first_resolves
        && (!crate_items(cx, first).is_empty() || exported_by_a_dependency(cx, first))
    {
        // Prose about an item not imported here: `Vec::push` where `Vec` is not in scope.
        return None;
    }
    Some(fix)
}

/// The full paths, `crate::a::b::Name`, of the items of the crate linted named `name`.
fn crate_items(cx: &Cx<'_, '_>, name: &str) -> Vec<String> {
    let db = cx.db();
    // alloc: the few items of one name
    let mut found = Vec::new();
    for module in cx.krate().modules(db) {
        for def in module.declarations(db) {
            if def.name(db).is_some_and(|it| it.as_str() == name)
                && let Some(path) = full_path(db, module, name)
            {
                found.push(path);
            }
        }
    }
    found.sort();
    found.dedup();
    found
}

/// `crate::a::b::name` for an item `name` declared in `module`, when every module to the root
/// has a name (none is a block's).
fn full_path(db: &base_db::Db, module: hir::Module, name: &str) -> Option<String> {
    let chain = module.path_to_root(db);
    let mut path = String::from("crate");
    for it in chain.iter().rev().skip(1) {
        path.push_str("::");
        path.push_str(it.name(db)?.as_str());
    }
    path.push_str("::");
    path.push_str(name);
    Some(path)
}

/// Whether a dependency of the crate linted exports an item named exactly `name`.
fn exported_by_a_dependency(cx: &Cx<'_, '_>, name: &str) -> bool {
    let query = hir::import_map::Query::new(name.to_owned()).exact().case_sensitive();
    cx.krate().query_external_importables(cx.db(), &query).next().is_some()
}

/// The single-backtick spans of `line`, each with the byte offset of its text: a span right
/// before `](` or `][` is a link's text, whose target is elsewhere, and is left out.
fn spans(line: &str) -> Vec<(usize, &str)> {
    let bytes = line.as_bytes();
    // alloc: one line's spans, a few
    let mut found = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] != b'`' {
            at += 1;
            continue;
        }
        let run = bytes[at..].iter().take_while(|it| **it == b'`').count();
        let start = at + run;
        // The closing run of the same length.
        let mut end = start;
        let mut close = None;
        while end < bytes.len() {
            if bytes[end] == b'`' {
                let len = bytes[end..].iter().take_while(|it| **it == b'`').count();
                if len == run {
                    close = Some(end);
                    break;
                }
                end += len;
                continue;
            }
            end += 1;
        }
        let Some(close) = close else { break };
        let after = &line[close + run..];
        if run == 1 && !after.starts_with("](") && !after.starts_with("][") {
            found.push((start, &line[start..close]));
        }
        at = close + run;
    }
    found
}

/// The path `text` is, when it is one this lint resolves: its identifiers, without a trailing
/// `()` or generic arguments.
fn path_like(text: &str) -> Option<&str> {
    let text = text.strip_suffix("()").unwrap_or(text);
    let text = text.split('<').next()?;
    if text.is_empty() || text.ends_with('!') {
        return None;
    }
    let mut segments = text.split("::");
    let first = segments.clone().next()?;
    if !segments.all(is_ident) || PRIMITIVES.contains(&first) {
        return None;
    }
    let single = !text.contains("::");
    // A generic parameter (`T`, `T::Item`) is the item's own, no path.
    if first.len() == 1 || (single && !camel(first)) {
        return None;
    }
    Some(text)
}

fn is_ident(text: &str) -> bool {
    let mut chars = text.chars();
    chars.next().is_some_and(|it| it.is_ascii_alphabetic() || it == '_')
        && chars.all(|it| it.is_ascii_alphanumeric() || it == '_')
}

/// Whether `text` is CamelCase: an uppercase first letter and a lowercase letter after it.
fn camel(text: &str) -> bool {
    text.starts_with(|it: char| it.is_ascii_uppercase()) && text.chars().any(|it| it.is_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::{path_like, spans};

    #[test]
    fn a_doc_line_gives_its_paths() {
        let line = "calls `crate::a::run()`, `Thing`, `len`, ``Two``, `T::Item`, `u32::MAX`, [`Link`](x)";
        let paths: Vec<_> = spans(line).into_iter().filter_map(|(_, it)| path_like(it)).collect();
        assert_eq!(paths, ["crate::a::run", "Thing"]);
    }
}
