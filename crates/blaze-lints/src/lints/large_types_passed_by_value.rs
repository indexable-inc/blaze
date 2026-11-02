//! `large_types_passed_by_value`: a function taking by value a `Copy` struct, enum, array or
//! tuple larger than `pass-by-value-size-limit` (256 bytes by default).
//!
//! Follows the `large_types_passed_by_value` part of clippy's
//! `clippy_lints/src/pass_by_ref_or_value.rs` (`PassByRefOrValue::check_poly_fn`) (MIT or
//! Apache-2.0), ported from what is known of its source at rust-lang/rust's nightly of 2026-10:
//! no reference checkout of clippy was at hand. With `avoid-breaking-exported-api` (the
//! default) a function another crate can call is left alone. A `self` taken by value is linted
//! as any parameter, its span the `self` HIR gives the implicit `Self` type.

use syntax::view::{self as ast, AstNode as _, HasAttrs as _, HasName as _};

use crate::{check::Cx, diag::Applicability};

const LINT: super::Implemented = super::Implemented::LargeTypesPassedByValue;

/// Where a function is, as clippy's `check_fn` and `check_trait_item` tell them apart.
#[derive(PartialEq, Eq)]
pub(super) enum Owner {
    /// A free function, of a module or a block.
    Item,
    /// A method of an inherent impl.
    Inherent,
    /// A method of a trait, provided or required.
    Trait,
    /// A method of a trait's impl, whose signature the trait decides.
    TraitImpl,
}

pub(super) fn owner(function: ast::Fn<'_>) -> Owner {
    let Some(list) = function.syntax().parent().and_then(ast::AssocItemList::cast) else {
        return Owner::Item;
    };
    match list.syntax().parent() {
        Some(parent) if ast::Trait::can_cast(parent.kind()) => Owner::Trait,
        Some(parent) => match ast::Impl::cast(parent) {
            Some(imp) if imp.trait_().is_some() => Owner::TraitImpl,
            _ => Owner::Inherent,
        },
        None => Owner::Item,
    }
}

/// The ABI check of clippy's `check_fn` for a free function: an `extern` ABI other than
/// `"Rust"` (`extern fn` is `"C"`).
pub(super) fn foreign_abi(function: ast::Fn<'_>) -> bool {
    function.abi().is_some_and(|abi| abi.string_token().is_none_or(|it| it.text() != "\"Rust\""))
}

/// What `PassByRefOrValue::check_fn` leaves a free function alone for: a foreign ABI, or an
/// attribute with a list that is `proc_macro_derive(..)` or `inline(always)`.
fn skipped_item_fn(function: ast::Fn<'_>) -> bool {
    foreign_abi(function)
        || function.attrs().any(|attr| {
            let Some(ast::Meta::TokenTreeMeta(meta)) = attr.meta() else { return false };
            let Some(tt) = meta.token_tree() else { return false };
            match attr.simple_name() {
                Some("proc_macro_derive") => true,
                Some("inline") => tt
                    .syntax()
                    .descendants_with_tokens()
                    .filter_map(|it| it.into_token())
                    .any(|it| it.text() == "always"),
                _ => false,
            }
        })
}

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on(LINT) {
        return;
    }
    let Some(function) = ast::Fn::cast(node) else { return };
    let Some(params) = function.param_list() else { return };
    // Each parameter's input span as HIR has it (the written type, or the `self` of a `self`
    // or `mut self`), whether it is bound by a plain name, and whether its type can be an ADT,
    // array or tuple at all: a reference, pointer, `impl Trait` or `fn` type never is.
    let mut inputs: Vec<(ast::Node<'t>, bool, bool)> = Vec::new();
    if let Some(it) = params.self_param() {
        let plain = it.mut_token().is_none();
        match (it.ty(), it.amp_token()) {
            (Some(ty), _) => inputs.push((ty.syntax(), plain, candidate(ty))),
            (None, Some(_)) => inputs.push((it.syntax(), plain, false)),
            (None, None) => {
                let Some(name) = it.name() else { return };
                inputs.push((name.syntax(), plain, true));
            }
        }
    }
    for param in params.params() {
        let Some(ty) = param.ty() else { return };
        inputs.push((ty.syntax(), param.pat().is_some_and(plain_binding), candidate(ty)));
    }
    if !inputs.iter().any(|&(_, _, candidate)| candidate) {
        return;
    }
    if !cx.is_written(node) || cx.range(node).is_none() {
        return;
    }
    let owner = owner(function);
    match owner {
        Owner::TraitImpl => return,
        Owner::Item if skipped_item_fn(function) => return,
        _ => {}
    }
    if cx.conf().bool("avoid-breaking-exported-api", true) && cx.is_exported(node) {
        return;
    }
    let Some(def) = cx.fn_def(function) else { return };
    let db = cx.db();
    let Some(signature) = def.signature(db) else { return };
    let types = signature.params(db);
    if inputs.len() != types.len() {
        return;
    }
    let limit = u64::try_from(cx.conf().int("pass-by-value-size-limit", 256)).expect("a count, checked as the configuration loads");
    // A trait's method is checked by `check_trait_item`, outside its body: its patterns are
    // not looked at.
    let in_body = owner != Owner::Trait && function.body().is_some();
    let mut found = Vec::new();
    for (&(input, plain, candidate), ty) in inputs.iter().zip(&types) {
        if !candidate {
            continue;
        }
        let aggregate =
            ty.as_adt().is_some() || ty.as_array_element(db).is_some() || !ty.tuple_fields(db).is_empty();
        if !aggregate {
            continue;
        }
        // A parameter bound by anything but a plain name (`mut x`, a pattern) is left alone.
        if in_body && !plain {
            continue;
        }
        if !ty.is_copy(db) {
            continue;
        }
        let Ok(layout) = ty.layout(db) else { continue };
        let size = layout.size();
        if size <= limit {
            continue;
        }
        let Some(range) = cx.range(input) else { continue };
        found.push((range, size));
    }
    for (range, size) in found {
        let span = cx.span(range);
        let snippet = cx.snippet(range);
        cx.lint(
            LINT,
            node,
            span.clone(),
            format!(
                "this argument ({size} byte) is passed by value, but might be more efficient if passed by reference (limit: {limit} byte)"
            ),
        )
        .suggestion(
            span,
            "consider passing by reference instead",
            format!("&{snippet}"),
            Applicability::MaybeIncorrect,
        );
    }
}

/// Whether the type written `ty` can be an ADT, an array or a tuple: a path (`Self` and
/// aliases included), an array, a tuple, or one of those in parentheses.
fn candidate(ty: ast::Type<'_>) -> bool {
    match ty {
        ast::Type::PathType(_) | ast::Type::ArrayType(_) | ast::Type::TupleType(_) => true,
        ast::Type::ParenType(it) => it.ty().is_some_and(candidate),
        _ => false,
    }
}

/// `PatKind::Binding(BindingMode::NONE, ..)`: a name bound by value, not `mut` or `ref`.
pub(super) fn plain_binding(pat: ast::Pat<'_>) -> bool {
    matches!(pat, ast::Pat::IdentPat(it) if it.mut_token().is_none() && it.ref_token().is_none())
}
