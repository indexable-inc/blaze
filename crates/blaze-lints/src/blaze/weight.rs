//! Code that carries no weight, abstraction with one user, and arguments in the wrong order:
//! the shapes code written by an LLM agent leaves behind most (a field nothing reads, a helper
//! only tests call, a trait with one impl, a function that only forwards, a parameter every
//! caller passes the same constant, two same-typed arguments passed in each other's place).
//!
//! Each is found by name resolution and inferred types, never by text, and the ones about how
//! an item is used read the whole workspace: the use rows of [`rows`], summed over every crate
//! that can name the item (every configuration of its own crate, its tests too, and the
//! workspace crates that depend on it). One module per lint; this one dispatches them and holds
//! what they share. What each finds and leaves alone is in its module's comment and in
//! docs/lints.md#code-that-carries-no-weight.

mod always_same_result;
mod constant_argument;
mod forwarding_fn;
mod never_used_variant;
mod only_used_in_tests;
pub(crate) mod rows;
mod single_impl_trait;
mod swappable_parameters;
mod swapped_arguments;
#[cfg(test)]
mod tests;
mod write_only_field;

use syntax::{
    SyntaxKind,
    view::{self as ast, AstNode as _, HasAttrs as _},
};

use super::{
    Lint,
    std_mistakes::{Fix, report},
    test_code,
};
use crate::check::Cx;
use rows::{Facts, Subject};

/// The lints of this module.
const LINTS: &[Lint] = &[
    Lint::SwappedArguments,
    Lint::WriteOnlyField,
    Lint::OnlyUsedInTests,
    Lint::SingleImplTrait,
    Lint::ForwardingFn,
    Lint::ConstantArgument,
    Lint::AlwaysSameResult,
    Lint::NeverConstructed,
    Lint::NeverMatched,
    Lint::SwappableParameters,
];

/// Every node of a file or a body: each lint looks at the kinds it finds. Nothing a macro made
/// is looked at.
pub(super) fn check_node<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !matches!(
        node.kind(),
        SyntaxKind::FN
            | SyntaxKind::CALL_EXPR
            | SyntaxKind::METHOD_CALL_EXPR
            | SyntaxKind::STRUCT
            | SyntaxKind::ENUM
            | SyntaxKind::TRAIT
    ) {
        return;
    }
    if !LINTS.iter().any(|it| cx.on_blaze(*it)) || cx.expansion().is_some() || !cx.is_written(node)
    {
        return;
    }
    match node.kind() {
        SyntaxKind::FN => {
            // A function is linted by its own body's query, whose tree's root it is.
            if node != cx.root() {
                return;
            }
            let Some(f) = ast::Fn::cast(node) else { return };
            only_used_in_tests::check(cx, f);
            constant_argument::check(cx, f);
            swappable_parameters::check(cx, f);
            if cx.analysis().is_some() {
                forwarding_fn::check(cx, f);
                always_same_result::check(cx, f);
            }
        }
        SyntaxKind::CALL_EXPR | SyntaxKind::METHOD_CALL_EXPR => {
            if cx.analysis().is_some() {
                swapped_arguments::check(cx, node);
            }
        }
        SyntaxKind::STRUCT => {
            if let Some(item) = ast::Struct::cast(node) {
                write_only_field::check(cx, ast::Adt::Struct(item));
            }
        }
        SyntaxKind::ENUM => {
            if let Some(item) = ast::Enum::cast(node) {
                write_only_field::check(cx, ast::Adt::Enum(item));
                never_used_variant::check(cx, item);
            }
        }
        SyntaxKind::TRAIT => {
            if let Some(item) = ast::Trait::cast(node) {
                single_impl_trait::check(cx, item);
            }
        }
        _ => {}
    }
}

/// The facts of `subject` over the workspace, from the crate linted.
fn facts(cx: &Cx<'_, '_>, subject: Subject) -> Facts {
    rows::facts(cx.db(), cx.krate(), subject)
}

/// The function `f` is, when it is one these lints judge by its uses: not a trait's item or
/// an impl of one (their callers call the trait), not test code, not `extern`, not `main`.
fn judged_fn<'t>(cx: &Cx<'_, 't>, f: ast::Fn<'t>) -> Option<hir::Function> {
    let db = cx.db();
    if f.abi().is_some() || test_code::is_test_code(cx, f.syntax()) {
        return None;
    }
    let func = cx.fn_def(f)?;
    if let Some(assoc) = hir::AsAssocItem::as_assoc_item(func, db)
        && (assoc.implemented_trait(db).is_some()
            || matches!(assoc.container(db), hir::AssocItemContainer::Trait(_)))
    {
        return None;
    }
    if func.name(db).as_str() == "main" {
        return None;
    }
    Some(func)
}

/// The subject of `func`.
fn fn_subject(cx: &Cx<'_, '_>, func: hir::Function) -> Option<Subject> {
    let id = hir_def::FunctionId::try_from(func).ok()?;
    Subject::of_fn(cx.db(), id)
}

/// The variants of `adt` with the syntax of each: a struct's one, an enum's each.
fn variants_of<'t>(
    cx: &Cx<'_, 't>,
    adt: ast::Adt<'t>,
) -> Vec<(hir_def::VariantId, ast::Node<'t>, Option<ast::FieldList<'t>>)> {
    // alloc: an ADT's variants, once per ADT linted
    let mut out = Vec::new();
    match adt {
        ast::Adt::Struct(it) => {
            if let Some(hir::Adt::Struct(def)) = cx.adt_def(adt) {
                out.push((hir_def::VariantId::StructId(def.into()), it.syntax(), it.field_list()));
            }
        }
        ast::Adt::Enum(it) => {
            for variant in it.variant_list().into_iter().flat_map(|it| it.variants()) {
                if let Some(hir::ModuleDef::EnumVariant(def)) = cx.def(variant.syntax()) {
                    let id = hir_def::EnumVariantId::from(def);
                    out.push((id.into(), variant.syntax(), variant.field_list()));
                }
            }
        }
        ast::Adt::Union(_) => {}
    }
    out
}

/// The names of the derives on `owner`, as their macros are named.
fn derives<'t>(cx: &Cx<'_, 't>, owner: ast::Node<'t>) -> Vec<String> {
    let db = cx.db();
    // alloc: an item's few derives
    let mut out = Vec::new();
    let Some(owner) = ast::AnyHasAttrs::cast(owner) else { return out };
    for attr in owner.attrs() {
        if attr.meta().and_then(|it| it.simple_name()) != Some("derive") {
            continue;
        }
        for mac in cx.derive_macros(attr).into_iter().flatten() {
            out.push(mac.name(db).as_str().to_owned());
        }
    }
    out
}

/// Whether `owner` has an attribute named `name`.
fn has_attr<'t>(owner: ast::Node<'t>, name: &str) -> bool {
    ast::AnyHasAttrs::cast(owner).is_some_and(|it| {
        it.attrs().any(|attr| attr.meta().and_then(|meta| meta.simple_name()) == Some(name))
    })
}

/// Whether `owner` has an attribute other than a doc comment: something (an `#[inline]`, a
/// `#[track_caller]`, a `cfg`, an export) the lints that ask whether an item carries weight
/// take as its reason to be.
fn has_attrs<'t>(owner: impl ast::HasAttrs<'t>) -> bool {
    owner.attrs().any(|it| it.meta().and_then(|meta| meta.simple_name()) != Some("doc"))
}
