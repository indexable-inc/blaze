//! E0433: a segment before the last of a path (in a signature, and of a body through
//! `lower::unresolved_path`) that names no module or type, as
//! rustc's late resolution reports it (`report_path_resolution_error` with the ribs of the
//! item, `opt_ns` the type namespace).
//!
//! Follows rustc's `compiler/rustc_resolve/src/diagnostics/impls.rs`
//! (`report_path_resolution_error`, `find_similarly_named_module_or_crate`) and
//! `compiler/rustc_span/src/source_map.rs` (`span_to_short_string`) (MIT or Apache-2.0).

use hir::{InFile, ItemInNs, ModPath, ModuleDef, Name, PathKind};
use span::Edition;
use stdx::edit_distance::find_best_match_for_name;
use syntax::{
    SyntaxKind, TextRange,
    view::{self as ast, AstNode as _},
};

pub(super) use crate::lower::unresolved_import::{ROOT, Segment};
use crate::{
    cx::Cx,
    lower::unresolved_import::{missing_crate, missing_crate_2015, value_descr},
    model::{Applicability, Diag, Span},
};

/// Where a path is written: its file and a node in its scope, of a tree `cx.vsema` gave out.
#[derive(Clone, Copy)]
pub(crate) struct At<'db> {
    pub(crate) file: hir::HirFileId,
    pub(crate) node: ast::Node<'db>,
}

/// The E0433 of segment `at` of `path`, `module` what the segments before it named.
pub(crate) fn diag<'db>(cx: &Cx<'_, 'db>, place: &At<'db>, path: &[Segment], at: usize, module: Option<ModuleDef>) -> Diag {
    let ident = &path[at];
    let name = ident.name.as_str();
    let span = |range: TextRange| cx.span(InFile::new(place.file, range));
    let scope = match path[..at].last() {
        // alloc: the error's text, on the error path
        Some(prev) if prev.name == ROOT => "the crate root".to_owned(),
        // alloc: the error's text, on the error path
        Some(prev) => format!("`{}`", prev.name),
        // alloc: the error's text, on the error path
        None => "this scope".to_owned(),
    };
    // alloc: the error's text, on the error path
    let mut message = format!("cannot find `{name}` in {scope}");
    let mut suggestion: Option<(Vec<(Span, String)>, String)> = None;
    let crate_root = cx.krate.root_module(cx.db);
    let label = if name == "super" {
        // alloc: the error's text, on the error path
        message = "too many leading `super` keywords".to_owned();
        // alloc: the error's text, on the error path
        "there are too many leading `super` keywords".to_owned()
    } else if module == Some(ModuleDef::Module(crate_root)) {
        if let Some(candidate) = module_candidate(cx, place, path, at) {
            // alloc: the error's text, on the error path
            suggestion = Some((vec![(span(ident.range), candidate)], "a similar path exists".to_owned()));
            // alloc: the error's text, on the error path
            "unresolved import".to_owned()
        } else if name == "core" {
            // alloc: the error's text, on the error path
            suggestion = Some((vec![(span(ident.range), "std".to_owned())], "try using `std` instead of `core`".to_owned()));
            // alloc: the error's text, on the error path
            "you might be missing crate `core`".to_owned()
        } else if name == "_" {
            // alloc: the error's text, on the error path
            message = "invalid crate or module name `_`".to_owned();
            // alloc: the error's text, on the error path
            "`_` is not a valid crate or module name".to_owned()
        } else if cx.edition == Edition::Edition2015 {
            // alloc: the error's text, on the error path
            message = format!("cannot find module or crate `{name}` in {scope}");
            suggestion = Some((
                // alloc: the error's text, on the error path
                vec![(crate_insert_span(cx), format!("extern crate {name};\n"))],
                missing_crate_2015(cx, name),
            ));
            // alloc: the error's text, on the error path
            format!("use of unresolved module or unlinked crate `{name}`")
        } else {
            // alloc: the error's text, on the error path
            format!("could not find `{name}` in the crate root")
        }
    } else if at > 0 {
        let parent = match path[at - 1].name.as_str() {
            // alloc: the error's text, on the error path
            ROOT if cx.edition > Edition::Edition2015 => "the list of imported crates".to_owned(),
            // alloc: the error's text, on the error path
            ROOT | "crate" => "the crate root".to_owned(),
            // alloc: the error's text, on the error path
            parent => format!("`{parent}`"),
        };
        match value(cx, place, Some(&path[..at]), name) {
            // alloc: the error's text, on the error path
            Some(def) => format!("expected type, found {} `{name}` in {parent}", value_descr(cx, def)),
            // alloc: the error's text, on the error path
            None => format!("could not find `{name}` in {parent}"),
        }
    } else if name == "Self" {
        // alloc: the error's text, on the error path
        "`Self` is only available in impls, traits, and type definitions".to_owned()
    } else if name.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
        // alloc: the error's text, on the error path
        message = format!("cannot find type `{name}` in {scope}");
        match value(cx, place, None, name).and_then(|def| Some((def, def_start(cx, def)?))) {
            Some((def, at)) => {
                let descr = value_descr(cx, def);
                let article = if descr.starts_with(['a', 'e', 'i', 'o', 'u']) { "an" } else { "a" };
                // alloc: the error's text, on the error path
                format!("`{name}` is declared as {article} {descr} at `{at}`, not a type")
            }
            // alloc: the error's text, on the error path
            None => format!("use of undeclared type `{name}`"),
        }
    } else {
        if name == "alloc" {
            // alloc: the error's text, on the error path
            suggestion = Some((Vec::new(), "add `extern crate alloc` to use the `alloc` crate".to_owned()));
        }
        if suggestion.is_none() {
            suggestion = similar_module_or_crate(cx, place, name)
                // alloc: the error's text, on the error path
                .map(|it| (vec![(span(ident.range), it)], "there is a crate or module with a similar name".to_owned()));
        }
        // alloc: the error's text, on the error path
        message = format!("cannot find module or crate `{name}` in {scope}");
        match value(cx, place, None, name) {
            // alloc: the error's text, on the error path
            Some(def) => format!("{} `{name}` is not a crate or module", value_descr(cx, def)),
            None => {
                if suggestion.is_none() {
                    // alloc: the error's text, on the error path
                    suggestion = Some((Vec::new(), missing_crate(cx, name)));
                }
                // alloc: the error's text, on the error path
                format!("use of unresolved module or unlinked crate `{name}`")
            }
        }
    };
    let at_span = span(ident.range);
    // clone: the error's span
    let mut diag = Diag::error(Some("E0433"), message, at_span.clone());
    diag.span_label(at_span, label);
    match suggestion {
        Some((parts, message)) if parts.is_empty() => {
            diag.help(message);
        }
        Some((parts, message)) => {
            diag.multipart_suggestion(message, parts, Applicability::MaybeIncorrect);
        }
        None => {}
    }
    diag
}

/// The leading keywords of `path` as hir's path kind, and the names after them.
pub(crate) fn kind_of(path: &[Segment]) -> (PathKind, &[Segment]) {
    let mut kind = PathKind::Plain;
    let mut rest = path;
    while let [first, tail @ ..] = rest {
        kind = match (first.name.as_str(), kind) {
            (ROOT, PathKind::Plain) if rest.len() == path.len() => PathKind::Abs,
            ("crate", PathKind::Plain) if rest.len() == path.len() => PathKind::Crate,
            ("self", PathKind::Plain) if rest.len() == path.len() => PathKind::Super(0),
            ("super", PathKind::Plain) if rest.len() == path.len() => PathKind::Super(1),
            ("super", PathKind::Super(n)) => PathKind::Super(n + 1),
            _ => break,
        };
        rest = tail;
    }
    (kind, rest)
}

/// What `name` names as a value in the module `scope` names, or where the path is.
fn value<'db>(cx: &Cx<'_, 'db>, place: &At<'db>, scope: Option<&[Segment]>, name: &str) -> Option<ModuleDef> {
    let mut path: Vec<Segment> = scope.map_or_else(Vec::new, <[Segment]>::to_vec);
    // alloc: a path looked up for the error's label, on the error path
    path.push(Segment { name: name.to_owned(), range: TextRange::default() });
    let (kind, rest) = kind_of(&path);
    let path = ModPath::from_segments(kind, rest.iter().map(|it| Name::new_root(&it.name)));
    let mut items = cx.vsema.resolve_mod_path(place.node, &path)?;
    items.find_map(|it| match it {
        ItemInNs::Values(def) => Some(def),
        _ => None,
    })
}

/// rustc's `span_to_short_string` of where a definition starts: `file:line:col`.
fn def_start(cx: &Cx<'_, '_>, def: ModuleDef) -> Option<String> {
    let source = match def {
        ModuleDef::Function(it) => cx.vsema.source(it)?.map(|it| it.syntax().text_range()),
        ModuleDef::Const(it) => cx.vsema.source(it)?.map(|it| it.syntax().text_range()),
        ModuleDef::Static(it) => cx.vsema.source(it)?.map(|it| it.syntax().text_range()),
        _ => return None,
    };
    let span = cx.span(source);
    let file = cx.file_of(&span)?;
    let text = cx.file_text(file);
    let before = text.get(..span.lo as usize)?;
    let line = before.matches('\n').count() + 1;
    let col = before.rsplit('\n').next().map_or(0, |it| it.chars().count()) + 1;
    // alloc: the error's text, on the error path
    Some(format!("{}:{line}:{col}", span.file))
}

/// rustc's `lookup_import_candidates` of a module named as the segment `at` of `path`, as the
/// path that names it with what it shares with `path` left out.
fn module_candidate<'db>(cx: &Cx<'_, 'db>, place: &At<'db>, path: &[Segment], at: usize) -> Option<String> {
    let db = cx.db;
    let name = path[at].name.as_str();
    let current = cx.vsema.scope(place.node)?.module();
    let mut candidates: Vec<Vec<String>> = cx
        .krate
        .modules_iter(db)
        .filter(|m| m.name(db).is_some_and(|it| it.as_str() == name))
        .filter(|m| hir::HasVisibility::is_visible_from(m, db, current))
        .map(|m| {
            // alloc: the error's suggestion, on the error path
            let mut segments: Vec<String> = Vec::new();
            if cx.edition > Edition::Edition2015 {
                // alloc: the error's suggestion, on the error path
                segments.push("crate".to_owned());
            }
            // alloc: the error's suggestion, on the error path
            segments.extend(m.path_segments(db).map(|it| it.as_str().to_owned()));
            segments
        })
        // alloc: the error's suggestion, on the error path
        .collect();
    candidates.sort_by_cached_key(|it| (it.len(), it.join("::")));
    let candidate = candidates.into_iter().next()?;
    let len = candidate.len();
    let start = (0..=at.min(len - 1)).find(|&i| path[i].name != candidate[i]).unwrap_or_default();
    Some(candidate[start..].join("::"))
}

/// rustc's `current_crate_outer_attr_insert_span`: before the first token after the crate
/// root's inner attributes.
fn crate_insert_span(cx: &Cx<'_, '_>) -> Span {
    let file = cx.krate.root_module(cx.db).definition_source_file_id(cx.db);
    cx.span(InFile::new(file, TextRange::empty(after_inner_attributes(cx.root(file)))))
}

/// Where the first token after the inner attributes of the file `source` is the root of
/// starts; its end when there is none.
fn after_inner_attributes(source: ast::Node<'_>) -> syntax::TextSize {
    // A plain comment is skipped and an outer doc comment is the first token; an inner doc
    // comment (a `DOC_COMMENT` node of an `INNER_DOC_COMMENT`) is an inner attribute.
    let is_token = |token: ast::Token<'_>| {
        !matches!(
            token.kind(),
            SyntaxKind::WHITESPACE | SyntaxKind::SHEBANG | SyntaxKind::COMMENT | SyntaxKind::INNER_DOC_COMMENT
        )
    };
    source
        .children_with_tokens()
        .find_map(|it| match it {
            syntax::NodeOrToken::Node(node) => {
                if ast::Attr::cast(node).is_some_and(|it| it.excl_token().is_some()) {
                    return None;
                }
                node.descendants_with_tokens().filter_map(syntax::NodeOrToken::into_token).find(|it| is_token(*it))
            }
            syntax::NodeOrToken::Token(token) => is_token(token).then_some(token),
        })
        .map_or(source.text_range().end(), |it| it.text_range().start())
}

/// rustc's `find_similarly_named_module_or_crate`: a crate of the extern prelude or a module
/// below the one the path is in whose name is close to `name`.
fn similar_module_or_crate<'db>(cx: &Cx<'_, 'db>, place: &At<'db>, name: &str) -> Option<String> {
    let db = cx.db;
    let current = cx.vsema.scope(place.node)?.module().nearest_non_block_module(db);
    // alloc: the error's suggestion, on the error path
    let mut candidates: Vec<Name> = cx.krate.dependencies_iter(db).map(|it| it.name).collect();
    for module in cx.krate.modules_iter(db) {
        let below = std::iter::successors(module.parent(db), |it| it.parent(db)).any(|it| it == current);
        if below && let Some(it) = module.name(db) {
            candidates.push(it);
        }
    }
    candidates.retain(|it| !it.as_str().is_empty());
    candidates.sort_unstable_by(|a, b| a.symbol().cmp_order(b.symbol()));
    candidates.dedup();
    // alloc: the error's suggestion, on the error path
    let candidates: Vec<&str> = candidates.iter().map(Name::as_str).collect();
    find_best_match_for_name(&candidates, name, None).copied().filter(|it| *it != name).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use span::Edition;
    use syntax::SourceFile;

    use super::after_inner_attributes;

    /// `extern crate` goes after the inner attributes and inner doc comments, past whitespace,
    /// a shebang and plain comments, and before an item's outer doc comment.
    #[test]
    fn the_insertion_is_after_the_inner_attributes() {
        for (text, rest) in [
            ("#![allow(dead_code)]\n//! doc\n// plain\n/* block */\nfn f() {}", "fn f() {}"),
            ("//! doc\n#![feature(x)]\n\n/// outer\nstruct S;", "/// outer\nstruct S;"),
            ("#!/usr/bin/env run\n#![allow(x)]\nuse a::b;", "use a::b;"),
            ("#![allow(x)]\n// only a comment\n", ""),
        ] {
            let parse = SourceFile::parse(text, Edition::CURRENT);
            let at = usize::from(after_inner_attributes(parse.root()));
            assert_eq!(&text[at..], rest, "in {text:?}");
        }
    }
}
