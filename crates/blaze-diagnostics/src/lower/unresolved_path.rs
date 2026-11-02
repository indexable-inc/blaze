//! A path of a body that is not one identifier and resolves to nothing (`m::f()`,
//! `m::S { .. }`, `m::V(x)` in a pattern), at the segment hir found failing: a segment before
//! the last is E0433, as `checks::names::failure` gives it for a signature; the last is E0425
//! "cannot find value", E0422 "cannot find struct, variant or union type" or E0531 "cannot
//! find tuple struct or tuple variant", in the module the prefix names, with that module's
//! name of a similar spelling.
//!
//! Follows rustc's `compiler/rustc_resolve/src/late/diagnostics.rs`
//! (`smart_resolve_report_errors`, `make_base_error`, `lookup_typo_candidate`),
//! `compiler/rustc_resolve/src/late.rs` (`PathSource::descr_expected`, `error_code`) and
//! `compiler/rustc_resolve/src/diagnostics/impls.rs` (`report_path_resolution_error`) (MIT or
//! Apache-2.0).

use either::Either;
use hir::{InFile, ModuleDef, PathResolution};
use smallvec::SmallVec;
use span::Edition;
use syntax::{
    SyntaxNodePtr,
    view::{self as ast, AstNode as _},
};

use crate::{
    checks::names::{
        failure::{self, At},
        paths::{is_named, module_of, names_to_string, segments},
    },
    cx::Cx,
    lints::code_fact,
    lower::{
        Found, Phase,
        unresolved_ident::{Names, Ns, PathSource, def_descr, typo},
        unresolved_import::{ROOT, Segment},
    },
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::UnresolvedPath) -> Vec<Found> {
    let file = d.node.file_id;
    let node = cx.node_view(d.node);
    let ptr = InFile::new(file, d.node.value.syntax_node_ptr());
    let Some(path) = path_of(node.value) else {
        return refused(cx, ptr, "hir found it in a node that is not a path expression or pattern, or a struct literal");
    };
    // alloc: the path's prefixes, for an error hir found
    let mut prefixes: SmallVec<[ast::Path<'_>; 4]> = std::iter::successors(Some(path), |it| it.qualifier()).collect();
    prefixes.reverse();
    let Some(first) = prefixes.first().and_then(|it| it.segment()) else {
        return refused(cx, ptr, "its path has no first segment");
    };
    if !prefixes.iter().all(|it| it.segment().is_some_and(is_named)) {
        return refused(cx, ptr, "its path has a segment that is a type");
    }
    let segments = segments(first, &prefixes);
    let offset = usize::from(first.coloncolon_token().is_some());
    let lead = segments.len() - failure::kind_of(&segments).1.len();
    let at = lead + d.segment as usize;
    if at >= segments.len() {
        return refused(cx, ptr, "hir's failing segment is past the path's last");
    }
    // What the segments before the failing one name: the last of them (a prefix before it
    // resolved, or hir would have stopped there).
    let module = match at.checked_sub(offset + 1).map(|it| cx.vsema.resolve_path(prefixes[it])) {
        Some(Some(PathResolution::Def(def @ (ModuleDef::Module(_) | ModuleDef::Adt(hir::Adt::Enum(_)))))) => Some(def),
        _ => None,
    };
    let current = cx.vsema.scope(path.syntax()).map(|it| it.module());
    let name = &segments[at].name;

    let mut diag = if at + 1 < segments.len() {
        failure::diag(cx, &At { file, node: path.syntax() }, &segments, at, module)
    } else {
        last(cx, file, node.value, &segments, module)
    };
    if let Some(module) = module_of(module).or(current) {
        // alloc: the error's text, for an error hir found
        diag.fact(cx.resolves_fact(module, name), format!("what `{name}` names in its scope"));
    }
    code_fact(cx, &mut diag, ptr);
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Resolve)]
}

/// The path of a node hir reports an unresolved path of.
fn path_of<'t>(node: Either<ast::Expr<'t>, ast::Pat<'t>>) -> Option<ast::Path<'t>> {
    match node {
        Either::Left(ast::Expr::PathExpr(it)) => it.path(),
        Either::Left(ast::Expr::RecordExpr(it)) => it.path(),
        Either::Right(ast::Pat::PathPat(it)) => it.path(),
        Either::Right(ast::Pat::TupleStructPat(it)) => it.path(),
        Either::Right(ast::Pat::RecordPat(it)) => it.path(),
        _ => None,
    }
}

/// `smart_resolve_report_errors` of a path whose last segment names nothing in the module its
/// prefix names: "cannot find {expected} `name` in {module}", labelled "not found in
/// {module}" unless a name of that module is suggested.
fn last(
    cx: &Cx<'_, '_>,
    file: hir::HirFileId,
    node: Either<ast::Expr<'_>, ast::Pat<'_>>,
    segments: &[Segment],
    module: Option<ModuleDef>,
) -> Diag {
    let last = &segments[segments.len() - 1];
    let source = PathSource::of(node, false, &last.name);
    let mod_path = &segments[..segments.len() - 1];
    let (tick, mod_prefix, mod_str, in_module) = match mod_path {
        [first] if first.name == ROOT && cx.edition > Edition::Edition2015 => {
            // alloc: the error's text, for an error hir found
            ("", String::new(), "the list of imported crates".to_owned(), None)
        }
        [first] if first.name == ROOT || first.name == "crate" => {
            // alloc: the error's text, for an error hir found
            ("", String::new(), "the crate root".to_owned(), Some(cx.krate.root_module(cx.db)))
        }
        _ => {
            // `Res::descr` of a crate's root module is "crate".
            let prefix = match module {
                // alloc: the error's text, for an error hir found
                Some(ModuleDef::Module(it)) if it.is_crate_root(cx.db) => "crate ".to_owned(),
                // alloc: the error's text, for an error hir found
                Some(def) => format!("{} ", def_descr(cx, Ns::Type, def).unwrap_or("module")),
                // alloc: the error's text, for an error hir found
                None => String::new(),
            };
            ("`", prefix, names_to_string(mod_path), module_of(module))
        }
    };
    let at = cx.span(InFile::new(file, last.range));
    // alloc: the error's text, for an error hir found
    let message = format!("cannot find {} `{}` in {mod_prefix}{tick}{mod_str}{tick}", source.expected(), last.name);
    // clone: on the error path, for an error hir found
    let mut diag = Diag::error(Some(source.code()), message, at.clone());
    let suggested = in_module.is_some_and(|module| typo(cx, &mut diag, Names::Module(module), source, &last.name, &at, &[]));
    if !suggested {
        // alloc: the error's text, for an error hir found
        diag.span_label(at, format!("not found in {tick}{mod_str}{tick}"));
    }
    diag
}

/// An unresolved path blaze can't report as rustc does: an error naming why, never nothing.
fn refused(cx: &Cx<'_, '_>, ptr: InFile<SyntaxNodePtr>, why: &str) -> Vec<Found> {
    // alloc: the error's text, for an error hir found
    let message = format!("blaze can't report this unresolved path as rustc does: {why}");
    let mut diag = Diag::error(None, message, cx.span_of(ptr));
    code_fact(cx, &mut diag, ptr);
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Resolve)]
}
