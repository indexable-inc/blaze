//! A struct literal's or pattern's path that resolves, but not to what it needs:
//! - an item of another kind: E0574 for a struct literal or record pattern, E0532 for a tuple
//!   struct or path pattern, "expected {expected}, found {descr} `{path}`", labelled "not a
//!   {expected}";
//! - a static that a one-identifier path pattern would bind: E0530 "{match|let|function
//!   parameter} bindings cannot shadow statics", labelled "cannot be named the same as a
//!   static", with "the static `ST` is defined here" at the static;
//! - a const parameter in a path pattern: E0158 "constant parameters cannot be referenced in
//!   patterns", labelled "can't be used in patterns", with "constant defined here" at it;
//! - a type of another kind (through an alias or `Self`) or a type parameter: E0071 "expected
//!   struct, variant or union type, found {sort string}", labelled "not a struct";
//! - a segment naming no variant of an enum: E0599 "no variant named `C` found for enum `E`",
//!   labelled "variant not found in `E`";
//! - a segment that would be an associated type: E0223 "ambiguous associated type", as
//!   `ambiguous_assoc_type` reports it, at the path up to that segment;
//! - a segment after a variant: E0433 "failed to resolve: `A` is a variant, not a module";
//! - a pattern's path that type checking resolves to an item of another kind (an associated
//!   function or constant after a type, a `Self` that is no unit struct): E0164 "expected
//!   tuple struct or tuple variant, found {descr} `{path}`" in a tuple struct pattern, E0533
//!   "expected unit struct, unit variant or constant, found ..." in a path pattern ("expected
//!   unit struct" for `Self`), at the pattern, labelled "not a {expected}", or for a function
//!   in a tuple struct pattern "`fn` calls are not allowed in patterns" with rustc's help.
//!
//! Follows rustc's `compiler/rustc_resolve/src/late/diagnostics.rs`
//! (`smart_resolve_report_errors`), `compiler/rustc_resolve/src/late.rs` (`error_code`),
//! `compiler/rustc_resolve/src/diagnostics.rs` (`BindingShadowsSomethingUnacceptable`),
//! `compiler/rustc_mir_build/src/errors.rs` (`ConstParamInPattern`),
//! `compiler/rustc_hir_typeck/src/lib.rs`/`fn_ctxt/mod.rs` (`check_struct_path`) and
//! `compiler/rustc_hir_analysis/src/hir_ty_lowering/mod.rs` (`lower_assoc_path`),
//! `compiler/rustc_hir_typeck/src/pat.rs` (`check_pat_path`, `check_pat_tuple_struct`) and
//! `compiler/rustc_hir_typeck/src/lib.rs` (`report_unexpected_variant_res`) (MIT or
//! Apache-2.0).

use either::Either;
use hir::{InFile, StructPathDef, StructPathFound};
use syntax::view::{self as ast, AstNode as _};

use crate::{
    checks::names::{
        failure::{self, At},
        paths::{names_to_string, segments},
    },
    cx::Cx,
    lints::code_fact,
    lower::{
        Found, Phase, ambiguous_assoc_type, invalid_lhs_of_assignment,
        unresolved_ident::{Ns, PathSource, def_descr, def_span},
    },
    model::Diag,
    print::sort_string,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::NotAStructPath<'_>) -> Vec<Found> {
    let file = d.node.file_id;
    let node = cx.node_view(d.node);
    let ptr = InFile::new(file, d.node.value.syntax_node_ptr());
    // rustc's `extract_tuple_struct_path`/`extract_unit_struct_path`: in a destructuring
    // assignment, a call or path that resolves to no constructor is an expression, not a
    // pattern, so the assignment's left-hand side is not a place.
    if matches!(d.found, StructPathFound::Def(_))
        && matches!(node.value, Either::Left(ast::Expr::CallExpr(_) | ast::Expr::PathExpr(_)))
    {
        let lhs = hir::InvalidLhsOfAssignment { lhs: d.node, assign: d.node, op: None };
        return invalid_lhs_of_assignment::lower(cx, &lhs);
    }
    let Some(path) = path_of(node.value) else {
        return refused(cx, ptr, "hir found it in a node that is not a struct literal or a pattern");
    };
    // alloc: the path's prefixes, for an error hir found
    let mut prefixes: Vec<ast::Path<'_>> = std::iter::successors(Some(path), |it| it.qualifier()).collect();
    prefixes.reverse();
    let Some(first) = prefixes.first().and_then(|it| it.segment()) else {
        return refused(cx, ptr, "its path has no first segment");
    };
    let segments = segments(first, &prefixes);
    let lead = segments.len() - failure::kind_of(&segments).1.len();
    let path_str = names_to_string(&segments);
    let path_span = cx.span(InFile::new(file, path.syntax().text_range()));
    let last = segments.last().map(|it| it.name.as_str()).unwrap_or_default();
    let source = PathSource::of(node.value, false, last);
    let segment_at = |segment: u32| segments.get(lead + segment as usize);

    let diag = match d.found {
        StructPathFound::Def(StructPathDef::Static) if source == PathSource::Pat && segments.len() == 1 => {
            let what = binding_kind(node.value);
            // alloc: the error's text, for an error hir found
            let mut diag = Diag::error(Some("E0530"), format!("{what} bindings cannot shadow statics"), path_span.clone());
            diag.span_label(path_span, "cannot be named the same as a static");
            if let Some(hir::PathResolution::Def(def @ hir::ModuleDef::Static(_))) = cx.vsema.resolve_path(path)
                && let Some(at) = def_span(cx, Ns::Value, def)
            {
                // alloc: the error's text, for an error hir found
                diag.span_label(at, format!("the static `{path_str}` is defined here"));
            }
            diag
        }
        StructPathFound::Def(StructPathDef::ConstParam) if source == PathSource::Pat => {
            let mut diag =
                Diag::error(Some("E0158"), "constant parameters cannot be referenced in patterns", path_span.clone());
            diag.span_label(path_span, "can't be used in patterns");
            if let Some(hir::PathResolution::ConstParam(it)) = cx.vsema.resolve_path(path)
                && let Some(source) = cx.vsema.source(it.merge())
            {
                diag.span_label(cx.span(source.map(|it| it.syntax().text_range())), "constant defined here");
            }
            diag
        }
        StructPathFound::Def(StructPathDef::TypeParam) => {
            // alloc: the error's text, for an error hir found
            let message = format!("expected struct, variant or union type, found type parameter `{path_str}`");
            // clone: a handle, on the error path
            let mut diag = Diag::error(Some("E0071"), message, path_span.clone());
            diag.span_label(path_span, "not a struct");
            diag
        }
        StructPathFound::Def(def) => {
            let code = match source {
                PathSource::Struct => "E0574",
                _ => "E0532",
            };
            let expected = source.expected();
            // alloc: the error's text, for an error hir found
            let message = format!("expected {expected}, found {} `{path_str}`", descr(cx, def, path));
            // clone: a handle, on the error path
            let mut diag = Diag::error(Some(code), message, path_span.clone());
            // alloc: the error's text, for an error hir found
            diag.span_label(path_span, format!("not a {expected}"));
            diag
        }
        StructPathFound::Ty => {
            let Some(ty) = &d.ty else {
                return refused(cx, ptr, "hir found a type of another kind but gave no type");
            };
            // alloc: the error's text, for an error hir found
            let message = format!("expected struct, variant or union type, found {}", sort_string(cx, ty));
            // clone: a handle, on the error path
            let mut diag = Diag::error(Some("E0071"), message, path_span.clone());
            diag.span_label(path_span, "not a struct");
            diag
        }
        StructPathFound::NoVariant { segment } => {
            let Some(name) = segment_at(segment) else {
                return refused(cx, ptr, "hir's segment is past the path's last");
            };
            let enum_str = names_to_string(&segments[..lead + segment as usize]);
            let at = cx.span(InFile::new(file, name.range));
            // alloc: the error's text, for an error hir found
            let message = format!("no variant named `{}` found for enum `{enum_str}`", name.name);
            // clone: a handle, on the error path
            let mut diag = Diag::error(Some("E0599"), message, at.clone());
            // alloc: the error's text, for an error hir found
            diag.span_label(at, format!("variant not found in `{enum_str}`"));
            diag
        }
        StructPathFound::AssocType { segment } => {
            let Some(named) = segment_at(segment) else {
                return refused(cx, ptr, "hir's segment is past the path's last");
            };
            let up_to = prefixes.iter().find(|it| {
                it.segment().is_some_and(|it| it.syntax().text_range().contains_range(named.range))
            });
            let Some(up_to) = up_to else {
                return refused(cx, ptr, "hir's segment is in none of the path's prefixes");
            };
            ambiguous_assoc_type::diag(cx.span(InFile::new(file, up_to.syntax().text_range())))
        }
        StructPathFound::AfterVariant { segment } => {
            let at = lead + segment as usize;
            if at >= segments.len() {
                return refused(cx, ptr, "hir's segment is past the path's last");
            }
            failure::diag(cx, &At { file, node: path.syntax() }, &segments, at, None)
        }
        StructPathFound::TypeRelative(def) => {
            let (code, expected) = match (source, def) {
                (PathSource::TupleStruct, _) => ("E0164", "tuple struct or tuple variant"),
                (_, StructPathDef::SelfCtor) => ("E0533", "unit struct"),
                _ => ("E0533", "unit struct, unit variant or constant"),
            };
            // rustc prints the qualified path (`qpath_to_string`): `S::new`, `<S>::new`, `Self`.
            // alloc: the error's text, for an error hir found
            let qpath: String = path.syntax().text().split_whitespace().collect();
            let at = cx.span_of(ptr);
            // alloc: the error's text, for an error hir found
            let message = format!("expected {expected}, found {} `{qpath}`", descr(cx, def, path));
            // clone: a handle, on the error path
            let mut diag = Diag::error(Some(code), message, at.clone());
            match def {
                StructPathDef::AssocFn if code == "E0164" => {
                    diag.span_label(at, "`fn` calls are not allowed in patterns");
                    diag.help("for more information, visit https://doc.rust-lang.org/book/ch19-00-patterns.html");
                }
                _ => {
                    // alloc: the error's text, for an error hir found
                    diag.span_label(at, format!("not a {expected}"));
                }
            }
            diag
        }
    };
    let mut diag = diag;
    code_fact(cx, &mut diag, ptr);
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Bodies)]
}

/// The path of a node hir reports a struct path of.
fn path_of<'t>(node: Either<ast::Expr<'t>, ast::Pat<'t>>) -> Option<ast::Path<'t>> {
    match node {
        Either::Left(ast::Expr::RecordExpr(it)) => it.path(),
        Either::Left(ast::Expr::CallExpr(it)) => match it.expr()? {
            ast::Expr::PathExpr(it) => it.path(),
            _ => None,
        },
        Either::Left(ast::Expr::PathExpr(it)) => it.path(),
        Either::Right(ast::Pat::PathPat(it)) => it.path(),
        Either::Right(ast::Pat::TupleStructPat(it)) => it.path(),
        Either::Right(ast::Pat::RecordPat(it)) => it.path(),
        _ => None,
    }
}

/// `Res::descr` of what hir found: a struct, variant or alias as the path resolves it
/// (`def_descr` words a struct by its kind: "tuple struct", "unit struct", "struct").
fn descr<'db>(cx: &Cx<'_, 'db>, def: StructPathDef, path: ast::Path<'db>) -> &'static str {
    match def {
        StructPathDef::Struct | StructPathDef::Variant | StructPathDef::Union => {
            match cx.vsema.resolve_path(path) {
                Some(hir::PathResolution::Def(it)) => def_descr(cx, Ns::Value, it).unwrap_or("struct"),
                _ => "struct",
            }
        }
        StructPathDef::Enum => "enum",
        StructPathDef::TypeAlias => "type alias",
        StructPathDef::BuiltinType => "builtin type",
        StructPathDef::Module => "module",
        StructPathDef::Trait => "trait",
        StructPathDef::TypeParam => "type parameter",
        StructPathDef::Function => "function",
        StructPathDef::Const => "constant",
        StructPathDef::Static => "static",
        StructPathDef::ConstParam => "const parameter",
        StructPathDef::Local => "local variable",
        StructPathDef::AssocFn => "associated function",
        StructPathDef::AssocConst => "associated constant",
        StructPathDef::SelfCtor => "self constructor",
    }
}

/// rustc's `BindingShadowsSomethingUnacceptable` word for where a pattern binds.
fn binding_kind(node: Either<ast::Expr<'_>, ast::Pat<'_>>) -> &'static str {
    let Either::Right(pat) = node else { return "match" };
    let mut at = pat.syntax().parent();
    while let Some(parent) = at {
        if ast::LetStmt::can_cast(parent.kind()) || ast::LetExpr::can_cast(parent.kind()) {
            return "let";
        }
        if ast::Param::can_cast(parent.kind()) {
            return "function parameter";
        }
        if ast::MatchArm::can_cast(parent.kind()) {
            return "match";
        }
        at = parent.parent();
    }
    "match"
}

/// A struct path blaze can't report as rustc does: an error naming why, never nothing.
fn refused(cx: &Cx<'_, '_>, ptr: InFile<syntax::SyntaxNodePtr>, why: &str) -> Vec<Found> {
    // alloc: the error's text, for an error hir found
    let message = format!("blaze can't report this struct path as rustc does: {why}");
    let mut diag = Diag::error(None, message, cx.span_of(ptr));
    code_fact(cx, &mut diag, ptr);
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Bodies)]
}
