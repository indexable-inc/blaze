//! `unstable_name_collisions` (typeck): a method call, a field access with no field that names
//! a method, or a type-relative path whose probe passed over an unstable item of the same name
//! for a stable one, which the item's stabilization makes resolve to it: "{a} {kind} with this
//! name may be added to the standard library in the future", how to keep the item picked, and
//! the `#![feature]` that enables each passed over.
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/method/probe.rs`
//! (`Pick::maybe_emit_unstable_name_collision_hint`), its callers in `method/mod.rs`
//! (`lookup_probe`: at the method's name; `method_exists_for_diagnostic`: at the field's;
//! `resolve_fully_qualified_call`: at the path) and
//! `compiler/rustc_middle/src/ty/context.rs` (`disabled_nightly_features`,
//! `crate_level_attribute_injection_span`) (MIT or Apache-2.0). The items passed over are type
//! checking's (`InferenceResult::unstable_candidate_nodes`).

use hir::{InFile, ModuleDef};
use hir_def::{
    ItemContainerId, Lookup as _,
    hir::{Expr, ExprId, ExprOrPatId, Pat, PatId},
};
use hir_ty::{method_resolution::CandidateId, next_solver::GenericArgs};
use intern::Symbol;
use rustc_type_ir::inherent::GenericArgs as _;
use syntax::{
    SyntaxNodePtr,
    view::{self as ast, AstNode as _},
};

use super::BodyCx;
use crate::{
    lower::{Found, Phase, unresolved_method_call::inject_use_span},
    model::{Applicability, Diag, Span},
};

/// The lints this module gives: it runs only when one may fire.
pub(super) const LINTS: super::LintSet = super::lint_set(["unstable_name_collisions"]);

pub(super) fn lints(b: &BodyCx<'_, '_, '_>) -> Vec<Found> {
    // alloc: the lints found, empty until one is
    let mut found: Vec<(u32, Found)> = Vec::new();
    let store = b.store();
    for (id, candidates) in b.infer.unstable_candidate_nodes() {
        let node = match id {
            ExprOrPatId::ExprId(expr) => match store.expr(expr) {
                Expr::MethodCall { .. } | Expr::Field { .. } => Node::MethodCall(expr),
                Expr::Path(_) => Node::PathExpr(expr),
                _ => continue,
            },
            ExprOrPatId::PatId(pat) => match store.pat(pat) {
                Pat::Path(_) => Node::PathPat(pat),
                _ => continue,
            },
        };
        if let Some(it) = lint(b, node, candidates) {
            found.push(it);
        }
    }
    // Type checking probes each in the order its name is written: a receiver's calls before
    // the call, the call before its arguments.
    if found.len() > 1 {
        found.sort_by_key(|(at, _)| *at);
    }
    // alloc: the lints found, none on a body without a collision
    found.into_iter().map(|(_, it)| it).collect()
}

/// A node whose probe passed over unstable items.
#[derive(Clone, Copy)]
enum Node {
    /// A method call, or a field access with no field whose name a method has (rustc's
    /// `method_exists_for_diagnostic`).
    MethodCall(ExprId),
    PathExpr(ExprId),
    PathPat(PatId),
}

/// The lint of `node`, with the position of the span it is at, for the order of the body's.
fn lint(b: &BodyCx<'_, '_, '_>, node: Node, candidates: &[(CandidateId, Symbol)]) -> Option<(u32, Found)> {
    let db = b.db();
    let (item, args, at) = match node {
        Node::MethodCall(expr) => {
            let (function, args) = b.infer.method_resolution(expr)?;
            (CandidateId::FunctionId(function), args, b.expr_node(expr)?)
        }
        Node::PathExpr(expr) => {
            let (item, args) = b.infer.assoc_resolutions_for_expr(expr)?;
            (item, args, b.expr_node(expr)?)
        }
        Node::PathPat(pat) => {
            let (item, args) = b.infer.assoc_resolutions_for_pat(pat)?;
            (item, args, b.pat_node(pat)?)
        }
    };
    let span = match node {
        Node::MethodCall(_) => method_name(b, at)?,
        Node::PathExpr(_) | Node::PathPat(_) => b.cx.span_of(at),
    };
    let (article, kind) = match item {
        CandidateId::FunctionId(it) if hir::Function::from(it).has_self_param(db) => ("a", "method"),
        CandidateId::FunctionId(_) => ("an", "associated function"),
        CandidateId::ConstId(_) => ("an", "associated constant"),
    };
    // alloc: the lint's text, for a collision found
    let message = format!("{article} {kind} with this name may be added to the standard library in the future");
    // clone: the lint's span, for a collision found
    let mut diag = Diag::lint("unstable_name_collisions", message, span.clone());
    match item {
        CandidateId::FunctionId(it) => {
            let path = b.cx.def_path(ModuleDef::Function(it.into()));
            // alloc: the lint's text, for a collision found
            diag.help(format!("call with fully qualified syntax `{path}(...)` to keep using the current method"));
        }
        CandidateId::ConstId(it) => {
            if let ItemContainerId::TraitId(trait_) = it.lookup(db).container {
                let name = hir::Const::from(it).name(db).expect("a trait's associated constant is named");
                let trait_path = b.cx.def_path(ModuleDef::Trait(trait_.into()));
                let self_ty = b.ty(self_ty(args));
                diag.suggestion(
                    // clone: the lint's span, for a collision found
                    span.clone(),
                    "use the fully qualified path to the associated const",
                    // alloc: the lint's text, for a collision found
                    format!("<{self_ty} as {trait_path}>::{}", name.as_str()),
                    Applicability::MachineApplicable,
                );
            }
        }
    }
    disabled_nightly_features(b, &mut diag, candidates);
    Some((span.lo, b.lint(diag, Phase::Bodies, at)))
}

/// The self type of a trait's item, the first of its arguments.
fn self_ty<'db>(args: GenericArgs<'db>) -> hir_ty::next_solver::Ty<'db> {
    args.type_at(0)
}

/// rustc's `disabled_nightly_features`: on a nightly, for each item passed over, the
/// suggestion to enable its feature at the top of the crate.
fn disabled_nightly_features(b: &BodyCx<'_, '_, '_>, diag: &mut Diag, candidates: &[(CandidateId, Symbol)]) {
    // `UnstableFeatures::from_environment(None).is_nightly_build()` of blaze's nightly, as
    // `checks::features` reads it: `RUSTC_BOOTSTRAP=-1` makes it a stable compiler.
    let bootstrap = b.cx.krate.base().env(b.db()).get("RUSTC_BOOTSTRAP");
    if bootstrap.as_deref() == Some("-1") {
        return;
    }
    let root = b.cx.krate.root_module(b.db());
    let span = inject_use_span(b.cx, root).expect("the crate root is a file, whose items have a start");
    for &(candidate, ref feature) in candidates {
        let def = match candidate {
            CandidateId::FunctionId(it) => ModuleDef::Function(it.into()),
            CandidateId::ConstId(it) => ModuleDef::Const(it.into()),
        };
        let path = b.cx.def_path(def);
        let feature = feature.as_str();
        diag.suggestion_verbose(
            // clone: the lint's span, for a collision found
            span.clone(),
            // alloc: the lint's text, for a collision found
            format!("add `#![feature({feature})]` to the crate attributes to enable `{path}`"),
            // alloc: the lint's text, for a collision found
            format!("#![feature({feature})]\n"),
            Applicability::MaybeIncorrect,
        );
    }
}

/// The span of the name the method call or field access at `at` names (rustc's
/// `segment.ident.span`, `field.span`).
fn method_name(b: &BodyCx<'_, '_, '_>, at: InFile<SyntaxNodePtr>) -> Option<Span> {
    let node = b.cx.syntax_view(at);
    let name = match ast::Expr::cast(node.value)? {
        ast::Expr::MethodCallExpr(it) => it.name_ref()?,
        ast::Expr::FieldExpr(it) => it.name_ref()?,
        _ => return None,
    };
    Some(b.cx.span(node.with_value(name.syntax().text_range())))
}
