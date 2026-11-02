//! `rust_2021_prelude_collisions`, `rust_2024_prelude_collisions`, `array_into_iter` and
//! `boxed_slice_into_iter` of a method call that a later edition's prelude or `IntoIterator`
//! impl resolves to another method: "trait method `{name}` will become ambiguous in Rust
//! {edition}", with the receiver adjusted by hand (an inherent or trait object pick) or the
//! call written through the trait's path.
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/method/prelude_edition_lints.rs`
//! (`AmbiguousTraitMethodCall`, `AmbiguousTraitMethod`, `lint_edition_dependent_dot_call`,
//! `trait_path_or_bare_name`, `trait_path`, `adjust_expr`) and
//! `compiler/rustc_ast/src/ast.rs` (`join_path_idents`) (MIT or Apache-2.0).

use hir::{AsAssocItem as _, AssocItemContainer, EditionLint, InFile, ReceiverAdjustment};
use hir_def::{Lookup as _, item_scope::{ImportId, ImportOrExternCrate}};
use hir_ty::next_solver::Mutability;
use intern::sym;
use smallvec::SmallVec;
use syntax::view::{self, AstNode as _, HasArgList as _, HasGenericArgs as _};

use crate::{
    cx::Cx,
    lints::{code_fact, registry::LintId},
    lower::{Found, Phase},
    model::{Applicability, Diag, Span},
};

const RUST_2021_PRELUDE_COLLISIONS: LintId = LintId::named("rust_2021_prelude_collisions");
const RUST_2024_PRELUDE_COLLISIONS: LintId = LintId::named("rust_2024_prelude_collisions");
const ARRAY_INTO_ITER: LintId = LintId::named("array_into_iter");
const BOXED_SLICE_INTO_ITER: LintId = LintId::named("boxed_slice_into_iter");

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::EditionDependentMethodCall) -> Vec<Found> {
    let (lint, name) = match d.lint {
        EditionLint::Rust2021PreludeCollisions => (RUST_2021_PRELUDE_COLLISIONS, "rust_2021_prelude_collisions"),
        EditionLint::Rust2024PreludeCollisions => (RUST_2024_PRELUDE_COLLISIONS, "rust_2024_prelude_collisions"),
        EditionLint::ArrayIntoIter => (ARRAY_INTO_ITER, "array_into_iter"),
        EditionLint::BoxedSliceIntoIter => (BOXED_SLICE_INTO_ITER, "boxed_slice_into_iter"),
    };
    if !cx.lint_may_fire(lint) {
        // alloc: empty, allocates nothing
        return Vec::new();
    }
    let edition = match d.lint.edition() {
        span::Edition::Edition2021 => "2021",
        span::Edition::Edition2024 => "2024",
        other => unreachable!("an edition lint of {other:?}, which no lint is of"),
    };
    let db = cx.db;
    let method = d.method.name(db);
    // alloc: the lint's text, for a call hir found
    let message = format!("trait method `{}` will become ambiguous in Rust {edition}", method.as_str());
    let derefs = "*".repeat(d.autoderefs as usize);
    let autoref = match d.autoref {
        Some(ReceiverAdjustment::Autoref(Mutability::Mut)) => "&mut ",
        Some(ReceiverAdjustment::Autoref(Mutability::Not)) => "&",
        Some(ReceiverAdjustment::ToConstPtr) | None => "",
    };
    let to_const_ptr = matches!(d.autoref, Some(ReceiverAdjustment::ToConstPtr));
    let adjusted = |expr: &str| match to_const_ptr {
        // alloc: the lint's text, for a call hir found
        true => format!("{derefs}{expr} as *const _"),
        // alloc: the lint's text, for a call hir found
        false => format!("{autoref}{derefs}{expr}"),
    };
    let receiver_at = d.receiver.map(|it| it.syntax_node_ptr());
    let receiver = cx.span_of(receiver_at);

    if d.inherent {
        // clone: the lint's span, for a call hir found
        let mut diag = Diag::lint(name, message, receiver.clone());
        match cx.snippet(&receiver) {
            Some(expr) => {
                // alloc: the lint's text, for a call hir found
                let code = format!("({})", adjusted(expr));
                diag.suggestion(receiver, "disambiguate the method call", code, Applicability::MachineApplicable);
            }
            None => {
                let placeholder = match to_const_ptr {
                    // alloc: the lint's text, for a call hir found
                    true => format!("{derefs}(...) as *const _"),
                    // alloc: the lint's text, for a call hir found
                    false => format!("{autoref}{derefs}..."),
                };
                // alloc: the lint's text, for a call hir found
                diag.span_help(receiver, format!("disambiguate the method call with `({placeholder})`"));
            }
        }
        code_fact(cx, &mut diag, receiver_at);
        // alloc: the lint, for a call hir found
        return vec![Found::lint(diag, Phase::Bodies, receiver_at)];
    }

    let call_at = d.call_expr.map(|it| it.syntax_node_ptr());
    let call = cx.node_view(d.call_expr);
    let sp = cx.span_of(call_at);
    let trait_ = match d.method.as_assoc_item(db).map(|it| it.container(db)) {
        Some(AssocItemContainer::Trait(it)) => it,
        Some(AssocItemContainer::Impl(imp)) => {
            imp.trait_(db).expect("a method call hir lints as not inherent is of a trait or a trait's impl")
        }
        None => unreachable!("a method call hir lints is of an associated function"),
    };
    let trait_name = trait_path_or_bare_name(cx, call.value.syntax(), trait_);
    // clone: the lint's span, for a call hir found
    let mut diag = Diag::lint(name, message, sp.clone());
    let self_expr = ancestor_inside(&receiver, &sp).and_then(|it| cx.snippet(&it));
    match self_expr {
        Some(expr) => {
            // alloc: the lint's suggestion, for a call hir found
            let mut args = String::new();
            for arg in call.value.arg_list().into_iter().flat_map(|it| it.args()) {
                let span = cx.span(InFile::new(call.file_id, arg.syntax().text_range()));
                let text = ancestor_inside(&span, &sp).and_then(|it| cx.snippet(&it)).unwrap_or("");
                args.push_str(", ");
                args.push_str(text);
            }
            // Keep the turbofish.
            let turbofish = call
                .value
                .generic_arg_list()
                .and_then(|list| {
                    let start = list.l_angle_token()?.text_range().start();
                    let range = syntax::TextRange::new(start, list.syntax().text_range().end());
                    cx.snippet(&cx.span(InFile::new(call.file_id, range)))
                })
                .map(|it| {
                    // alloc: the lint's text, for a call hir found
                    format!("::{it}")
                })
                .unwrap_or_default();
            // alloc: the lint's text, for a call hir found
            let code = format!("{trait_name}::{}{turbofish}({}{args})", method.as_str(), adjusted(expr));
            // clone: the lint's span, for a call hir found
            diag.suggestion(sp.clone(), "disambiguate the associated function", code, Applicability::MachineApplicable);
        }
        None => {
            // alloc: the lint's text, for a call hir found
            let help = format!("disambiguate the associated function with `{trait_name}::{}(...)`", method.as_str());
            // clone: the lint's span, for a call hir found
            diag.span_help(sp.clone(), help);
        }
    }
    code_fact(cx, &mut diag, call_at);
    // alloc: the lint, for a call hir found
    vec![Found::lint(diag, Phase::Bodies, call_at)]
}

/// rustc's `Span::find_ancestor_inside`: `span`, or the call of the macro it came from, and of
/// the macro that call came from, .., the first that is inside `outer`.
pub(crate) fn ancestor_inside(span: &Span, outer: &Span) -> Option<Span> {
    // clone: a lint's span, asked only for a lint's suggestion
    let mut at = span.clone();
    loop {
        if at.file == outer.file && outer.lo <= at.lo && at.hi <= outer.hi {
            return Some(at);
        }
        at = at.expansion?.call_site;
    }
}

/// rustc's `trait_path_or_bare_name`: how `trait_` is named where `at` is ([`trait_path`]),
/// else its name.
pub(crate) fn trait_path_or_bare_name<'db>(cx: &Cx<'_, 'db>, at: view::Node<'db>, trait_: hir::Trait) -> String {
    trait_path(cx, at, trait_).unwrap_or_else(|| {
        // alloc: the lint's text, for a call hir found
        trait_.name(cx.db).as_str().to_owned()
    })
}

/// rustc's `trait_path`: the name the import that has `trait_` in scope at `at` gives it, or
/// the whole path of a `use .. as _` (rustc's `join_path_idents`); `None` where no import
/// brings it (it is declared in the module, is the impl's own trait, comes from the prelude) or
/// a glob does.
fn trait_path<'db>(cx: &Cx<'_, 'db>, at: view::Node<'db>, trait_: hir::Trait) -> Option<String> {
    let db = cx.db;
    let scope = cx.vsema.scope(at)?;
    // rustc's first candidate is the trait of the impl the code is in, which no import brings.
    if let Some(AssocItemContainer::Impl(imp)) =
        scope.containing_function().and_then(|it| it.as_assoc_item(db)).map(|it| it.container(db))
        && imp.trait_(db) == Some(trait_)
    {
        return None;
    }
    let id = hir_def::TraitId::from(trait_);
    let mut module: Option<hir_def::ModuleId> = Some(scope.module().into());
    let (name, import) = loop {
        let at = module?;
        let def_map = at.def_map(db);
        if let Some(binding) = def_map.module(db, at).scope(db).trait_binding(id) {
            break binding;
        }
        module = if at.is_block_module(db) { def_map.containing_module(db, at) } else { None };
    };
    match import {
        Some(ImportOrExternCrate::Import(import)) if *name.symbol() == sym::underscore => use_path(cx, import),
        // alloc: the lint's text, for a call hir found
        Some(ImportOrExternCrate::Import(_)) => Some(name.as_str().to_owned()),
        _ => None,
    }
}

/// The path the use tree `import` imports, as rustc's `join_path_idents` writes it: each
/// segment as written, `::` between them, and a leading `::` for a path from the root.
fn use_path(cx: &Cx<'_, '_>, import: ImportId) -> Option<String> {
    let loc = import.use_.lookup(cx.db);
    let mut tree = hir_def::src::use_tree_to_view(cx.db, loc.id, import.idx);
    // The trees from `import`'s out to the `use`'s own.
    let mut trees: SmallVec<[view::UseTree<'_>; 4]> = SmallVec::new();
    loop {
        trees.push(tree);
        match tree.parent_use_tree_list() {
            Some(list) => tree = list.parent_use_tree(),
            None => break,
        }
    }
    // alloc: the lint's trait path, for a call hir found
    let mut out = String::new();
    for tree in trees.iter().rev() {
        let Some(path) = tree.path() else {
            if tree.coloncolon_token().is_some() && out.is_empty() {
                out.push_str("::");
            }
            continue;
        };
        if out.is_empty() && path.first_qualifier_or_self().coloncolon_token().is_some() {
            out.push_str("::");
        }
        for segment in path.segments() {
            let text = match segment.kind()? {
                view::PathSegmentKind::Name(name) => name.text(),
                view::PathSegmentKind::SelfKw => "self",
                view::PathSegmentKind::SuperKw => "super",
                view::PathSegmentKind::CrateKw => "crate",
                view::PathSegmentKind::SelfTypeKw => "Self",
                view::PathSegmentKind::Type { .. } => return None,
            };
            if !out.is_empty() && !out.ends_with("::") {
                out.push_str("::");
            }
            out.push_str(text);
        }
    }
    Some(out)
}
