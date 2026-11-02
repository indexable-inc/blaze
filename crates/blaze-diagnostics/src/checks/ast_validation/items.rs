//! The checks of rustc's AST validation of items: `visit_item`, `visit_foreign_item` and
//! `visit_assoc_item`, in their order, before what is in the item is walked; and the errors
//! rustc's parser gives of the modifiers of an inherent impl, which blaze's parser accepts.
//!
//! Follows rustc's `compiler/rustc_ast_passes/src/ast_validation.rs` (`visit_item`,
//! `visit_foreign_item`, `visit_assoc_item`, `visibility_not_permitted`,
//! `check_defaultness`, `check_item_safety`, `check_type_no_bounds`,
//! `check_type_alias_where_clause_location`, `check_foreign_*`, `check_trait_fn_not_const`,
//! `check_impl_fn_not_const`, `check_async_fn_in_const_trait_or_impl`, `check_item_named`,
//! `check_nomangle_item_asciionly`, `check_mod_file_item_asciionly`, `ending_semi_or_hi`,
//! `deny_*`) and
//! `diagnostics.rs`, and `compiler/rustc_parse/src/parser/item.rs` (`parse_item_impl`) and its
//! `diagnostics.rs` (`TraitImplModifierInInherentImpl`) (MIT or Apache-2.0).

use hir::InFile;
use syntax::{
    SyntaxKind, TextRange,
    view::{self as ast, AstNode as _, HasGenericParams as _, HasName as _, HasTypeBounds as _, HasVisibility as _},
};

use super::{Ctxt, Out, V, const_token, ending_semi_or_hi, extend_while_whitespace, fns, unraw};
use crate::checks::defs::head_range_view as head_range;
use crate::{
    cx::item_range_view as item_range,
    lints::syntax::pretty,
    model::{Applicability, Diag, Span, SuggestionStyle},
};

/// Where the items rustc's `extern` keyword page explains.
const EXTERN_NOTE: &str = "for more information, visit https://doc.rust-lang.org/std/keyword.extern.html";

/// `VisibilityNotPermittedNote`: why a visibility is not permitted where it is.
#[derive(Clone, Copy)]
enum VisNote {
    EnumVariant,
    TraitImpl,
    IndividualImplItems,
    IndividualForeignItems,
}

impl VisNote {
    fn text(self) -> &'static str {
        match self {
            VisNote::EnumVariant => {
                "enum variants and their fields always share the visibility of the enum they are in"
            }
            VisNote::TraitImpl => "trait items always share the visibility of their trait",
            VisNote::IndividualImplItems => "place qualifiers on individual impl items instead",
            VisNote::IndividualForeignItems => "place qualifiers on individual foreign items instead",
        }
    }
}

/// E0449 `visibility_not_permitted` of `vis`, written on `node` or on what is in it.
fn visibility(v: &V<'_, '_, '_>, node: ast::Node<'_>, vis: Option<ast::Visibility<'_>>, note: VisNote, out: &mut Out) {
    let Some(vis) = vis else { return };
    let span = v.span(vis.syntax().text_range());
    // clone: a span, on the error path
    let mut diag = Diag::error(Some("E0449"), "visibility qualifiers are not permitted here", span.clone());
    diag.note(note.text());
    diag.suggestion(span, "remove the qualifier", "", Applicability::MachineApplicable);
    v.emit(out, node, diag);
}

/// The name an item declares.
fn item_name(node: ast::Node<'_>) -> Option<ast::Name<'_>> {
    node.children().find_map(ast::Name::cast)
}

/// `check_nomangle_item_asciionly` of an item (not a foreign one) or an associated item.
pub(super) fn no_mangle(v: &V<'_, '_, '_>, node: ast::Node<'_>, out: &mut Out) {
    let Some(name) = item_name(node) else { return };
    if unraw(name.text()).is_ascii() || !v.has_attr(node, "no_mangle") {
        return;
    }
    let diag = Diag::error(Some("E0754"), "`#[no_mangle]` requires ASCII identifier", v.span(head_range(node)));
    v.emit(out, node, diag);
}

/// `check_defaultness` with `default` not allowed: `ForbiddenDefault`.
fn defaultness(v: &V<'_, '_, '_>, node: ast::Node<'_>, default: Option<ast::Token<'_>>, out: &mut Out) {
    let Some(default) = default else { return };
    let mut diag =
        Diag::error(None, "`default` is only allowed on items in trait impls", v.span(head_range(node)));
    diag.span_label(v.span(default.text_range()), "`default` because of this");
    v.emit(out, node, diag);
}

/// `check_type_no_bounds`: `BoundInContext` over the bounds of `list`.
fn no_bounds(v: &V<'_, '_, '_>, node: ast::Node<'_>, list: Option<ast::TypeBoundList<'_>>, ctx: &str, out: &mut Out) {
    let Some(list) = list else { return };
    let mut bounds = list.bounds();
    let Some(first) = bounds.next() else { return };
    let first = first.syntax().text_range();
    let last = bounds.last().map_or(first, |it| it.syntax().text_range());
    let range = first.cover(last);
    // alloc: the error's message
    let diag = Diag::error(None, format!("bounds on `type`s in {ctx} have no effect"), v.span(range));
    v.emit(out, node, diag);
}

/// `check_item_safety` of an item outside an `extern` block: `safe` is `InvalidSafetyOnItem`.
pub(super) fn safe_outside_extern(v: &V<'_, '_, '_>, node: ast::Node<'_>, safe: Option<ast::Token<'_>>, out: &mut Out) {
    if safe.is_none() {
        return;
    }
    let diag = Diag::error(
        None,
        "items outside of `unsafe extern { }` cannot be declared with `safe` safety qualifier",
        v.span(item_range(node)),
    );
    v.emit(out, node, diag);
}

/// `check_item_safety` of an item of the `extern` block `block`: a safety qualifier in a
/// block not marked `unsafe` is `InvalidSafetyOnExtern`.
pub(super) fn safety_in_extern(
    v: &V<'_, '_, '_>,
    node: ast::Node<'_>,
    safety: Option<ast::Token<'_>>,
    block: InFile<ast::ExternBlock<'_>>,
    out: &mut Out,
) {
    if safety.is_none() || block.value.unsafe_token().is_some() {
        return;
    }
    let mut diag = Diag::error(
        None,
        "items in `extern` blocks without an `unsafe` qualifier cannot have safety qualifiers",
        v.span(item_range(node)),
    );
    diag.suggestion_verbose(
        block_head(v, block).shrink_to_lo(),
        "add `unsafe` to this `extern` block",
        "unsafe ",
        Applicability::MachineApplicable,
    );
    v.emit(out, node, diag);
}

/// `current_extern_span`: the head of the `extern` block an item is in.
pub(super) fn block_head(v: &V<'_, '_, '_>, block: InFile<ast::ExternBlock<'_>>) -> Span {
    v.cx.span(block.with_value(head_range(block.value.syntax())))
}

/// A function: `visit_item`'s, `visit_foreign_item`'s or `visit_assoc_item`'s checks of it,
/// then `visit_fn`'s.
pub(super) fn fn_<'db>(v: &V<'_, '_, 'db>, node: ast::Node<'db>, out: &mut Out) {
    let Some(item) = ast::Fn::cast(node) else { return };
    let ctxt = v.ctxt(node);
    match &ctxt {
        Ctxt::Free => {
            no_mangle(v, node, out);
            defaultness(v, node, item.default_token(), out);
            if item.body().is_none() && !v.has_attr(node, "rustc_intrinsic") {
                fn_without_body(v, node, item, out);
            }
        }
        Ctxt::Foreign(block) => foreign_fn(v, node, item, *block, out),
        Ctxt::Trait(_) | Ctxt::TraitImpl(_) | Ctxt::Impl(_) => {
            assoc(v, node, item.default_token(), &ctxt, out);
            if matches!(ctxt, Ctxt::TraitImpl(_) | Ctxt::Impl(_)) && item.body().is_none() {
                let span = v.span(item_range(node));
                let mut diag = Diag::error(None, "associated function in `impl` without body", span);
                diag.suggestion(
                    v.span(ending_semi_or_hi(node)),
                    "provide a definition for the function",
                    " { <body> }",
                    Applicability::HasPlaceholders,
                );
                v.emit(out, node, diag);
            }
            assoc_vis(v, node, &ctxt, out);
            assoc_fn_constness(v, node, item, &ctxt, out);
        }
    }
    fns::visit_fn(v, node, item, &ctxt, out);
}

/// `FnWithoutBody`, with the `extern` block a function with an ABI may have meant.
fn fn_without_body(v: &V<'_, '_, '_>, node: ast::Node<'_>, item: ast::Fn<'_>, out: &mut Out) {
    let span = v.span(item_range(node));
    // clone: a span, on the error path
    let mut diag = Diag::error(None, "free function without a body", span.clone());
    diag.suggestion(
        v.span(ending_semi_or_hi(node)),
        "provide a definition for the function",
        " { <body> }",
        Applicability::HasPlaceholders,
    );
    if let Some(abi) = item.abi() {
        let start = v.span(abi.syntax().text_range());
        let code = match fns::abi_string(abi) {
            // alloc: the error's suggestion
            None => "extern {".to_owned(),
            // alloc: as above
            Some(string) => fns::with_abi_text(string, |it| format!("extern \"{it}\" {{")),
        };
        diag.multipart_suggestion(
            "if you meant to declare an externally defined function, use an `extern` block",
            // alloc: as above
            vec![(start, code), (span.shrink_to_hi(), " }".to_owned())],
            Applicability::MaybeIncorrect,
        );
    }
    v.emit(out, node, diag);
}

/// `visit_assoc_item`'s first checks of an associated item: `#[no_mangle]`, `default` (only
/// in impls).
fn assoc(v: &V<'_, '_, '_>, node: ast::Node<'_>, default: Option<ast::Token<'_>>, ctxt: &Ctxt<'_>, out: &mut Out) {
    no_mangle(v, node, out);
    if matches!(ctxt, Ctxt::Trait(_)) {
        defaultness(v, node, default, out);
    }
}

/// `visit_assoc_item`: the visibility of an item of a trait or a trait impl.
fn assoc_vis(v: &V<'_, '_, '_>, node: ast::Node<'_>, ctxt: &Ctxt<'_>, out: &mut Out) {
    if matches!(ctxt, Ctxt::Trait(_) | Ctxt::TraitImpl(_)) {
        let vis = node.children().find_map(ast::Visibility::cast);
        visibility(v, node, vis, VisNote::TraitImpl, out);
    }
}

/// `check_trait_fn_not_const`, `check_impl_fn_not_const` and
/// `check_async_fn_in_const_trait_or_impl` of an associated function.
fn assoc_fn_constness(v: &V<'_, '_, '_>, node: ast::Node<'_>, item: ast::Fn<'_>, ctxt: &Ctxt<'_>, out: &mut Out) {
    let parent_const = ctxt.constness();
    let parent_const_span = parent_const.map(|it| v.cx.span(it.map(|it| it.text_range())));
    match (ctxt, item.const_token()) {
        (Ctxt::Trait(_) | Ctxt::TraitImpl(_), Some(const_)) => {
            // clone: a span, on the error path (E0379)
            trait_fn_const(v, node, const_, ctxt, parent_const_span.clone(), out);
        }
        (Ctxt::Impl(_), Some(const_)) => {
            if let Some(parent) = &parent_const_span {
                let span = v.span(extend_while_whitespace(const_));
                // clone: a span, on the error path
                let mut diag = Diag::error(None, "redundant `const` fn marker in const impl", span.clone());
                diag.suggestion(span, "remove the `const`", "", Applicability::MachineApplicable);
                // clone: as above
                diag.span_label(parent.clone(), "this declares all associated functions implicitly const");
                v.emit(out, node, diag);
            }
        }
        _ => {}
    }
    if let (Some(parent), Some(async_)) = (parent_const_span, item.async_token()) {
        let context = match ctxt {
            Ctxt::TraitImpl(_) => "trait impls",
            Ctxt::Impl(_) => "impls",
            _ => "traits",
        };
        let mut diag = Diag::error(
            None,
            // alloc: the error's message
            format!("async functions are not allowed in `const` {context}"),
            v.span(async_.text_range()),
        );
        diag.span_label(parent, "associated functions of `const` cannot be declared `async`");
        v.emit(out, node, diag);
    }
}

/// E0379 `TraitFnConst`: a `const fn` in a trait or a trait impl.
fn trait_fn_const(
    v: &V<'_, '_, '_>,
    node: ast::Node<'_>,
    const_: ast::Token<'_>,
    ctxt: &Ctxt<'_>,
    parent_const: Option<Span>,
    out: &mut Out,
) {
    let in_impl = matches!(ctxt, Ctxt::TraitImpl(_));
    let const_trait_impl = v.feature("const_trait_impl");
    let make_impl_const = match ctxt {
        Ctxt::TraitImpl(it) if const_trait_impl && it.value.const_token().is_none() && it.value.excl_token().is_none() => {
            it.value.trait_().map(|trait_| v.cx.span(it.with_value(trait_.syntax().text_range())).shrink_to_lo())
        }
        _ => None,
    };
    let make_trait_const = match ctxt {
        Ctxt::Trait(it) if const_trait_impl && const_token(it.value.syntax()).is_none() => {
            let range = item_range(it.value.syntax());
            let after = match it.value.syntax().last_token().and_then(|it| it.next_token()) {
                Some(next) if next.kind() == SyntaxKind::WHITESPACE => next.text_range().end(),
                _ => range.end(),
            };
            Some(v.cx.span(it.with_value(TextRange::empty(after))))
        }
        _ => None,
    };
    let what = if in_impl { "trait impls" } else { "traits" };
    let span = v.span(const_.text_range());
    // alloc: the error's message, and a span clone on the error path
    let mut diag = Diag::error(Some("E0379"), format!("functions in {what} cannot be declared const"), span.clone());
    // alloc: the error's label
    diag.span_label(span, format!("functions in {what} cannot be const"));
    let applicability = match &parent_const {
        Some(_) => Applicability::MachineApplicable,
        None => Applicability::MaybeIncorrect,
    };
    if let Some(parent) = parent_const {
        diag.span_label(parent, "this declares all associated functions implicitly const");
    }
    let multiple = make_impl_const.is_some() || make_trait_const.is_some();
    let message = if multiple { "remove the `const` ..." } else { "remove the `const`" };
    diag.suggestion(v.span(extend_while_whitespace(const_)), message, "", applicability);
    if let Some(at) = make_impl_const {
        diag.suggestion(at, "... and declare the impl to be const instead", "const ", Applicability::MaybeIncorrect);
    }
    if let Some(at) = make_trait_const {
        diag.suggestion(at, "... and declare the trait to be const instead", "const ", Applicability::MaybeIncorrect);
    }
    v.emit(out, node, diag);
}

/// An impl: of a trait, `visit_item`'s visibility check and E0198; inherent, the parser's
/// errors of its modifiers, then the visibility check.
pub(super) fn impl_<'db>(v: &V<'_, '_, 'db>, node: ast::Node<'db>, out: &mut Out) {
    let Some(impl_) = ast::Impl::cast(node) else { return };
    if let Some(trait_) = impl_.trait_() {
        visibility(v, node, impl_.visibility(), VisNote::TraitImpl, out);
        if let (Some(unsafe_), Some(excl)) = (impl_.unsafe_token(), impl_.excl_token()) {
            unsafe_negative(v, node, unsafe_, excl, trait_, out);
        }
        return;
    }
    if let Some(self_ty) = impl_.self_ty() {
        let self_ty = v.span(self_ty.syntax().text_range());
        let modifiers = [
            (impl_.unsafe_token(), "unsafe", "unsafe", Some("E0197")),
            (impl_.excl_token(), "!", "negative", None),
            (impl_.default_token(), "default", "default", None),
        ];
        for (token, modifier, name, code) in modifiers {
            let Some(token) = token else { continue };
            // alloc: the error's message, and a span clone on the error path
            let mut diag = Diag::error(code, format!("inherent impls cannot be {name}"), self_ty.clone());
            // alloc: the error's note
            diag.note(format!("only trait implementations may be annotated with `{modifier}`"));
            // alloc: the error's label
            diag.span_label(v.span(token.text_range()), format!("{name} because of this"));
            // clone: a span, on the error path
            diag.span_label(self_ty.clone(), "inherent impl for this type");
            v.emit_parse(out, node, diag);
        }
    }
    visibility(v, node, impl_.visibility(), VisNote::IndividualImplItems, out);
}

/// E0198 `UnsafeNegativeImpl`: from the `!` to the end of the trait's path.
fn unsafe_negative(
    v: &V<'_, '_, '_>,
    node: ast::Node<'_>,
    unsafe_: ast::Token<'_>,
    excl: ast::Token<'_>,
    trait_: ast::Type<'_>,
    out: &mut Out,
) {
    let path = match trait_ {
        ast::Type::PathType(it) => it.path().map(|it| it.syntax().text_range()),
        _ => None,
    };
    let negative = excl.text_range();
    let end = path.unwrap_or(trait_.syntax().text_range()).end().max(negative.end());
    let span = v.span(TextRange::new(negative.start(), end));
    let mut diag = Diag::error(Some("E0198"), "negative impls cannot be unsafe", span);
    diag.span_label(v.span(negative), "negative because of this");
    diag.span_label(v.span(unsafe_.text_range()), "unsafe because of this");
    v.emit(out, node, diag);
}

/// `ConstWithoutBody`, `StaticWithoutBody`, `TyAliasWithoutBody`, `AssocConstWithoutBody` and
/// `AssocTypeWithoutBody`: `message`, with the placeholder definition `code` of the `what`.
fn without_body(v: &V<'_, '_, '_>, node: ast::Node<'_>, message: &str, what: &str, code: &str, out: &mut Out) {
    let mut diag = Diag::error(None, message, v.span(item_range(node)));
    diag.suggestion(
        v.span(ending_semi_or_hi(node)),
        // alloc: the error's suggestion
        format!("provide a definition for the {what}"),
        code,
        Applicability::HasPlaceholders,
    );
    v.emit(out, node, diag);
}

/// A trait or a trait alias: `#[no_mangle]`, then what an auto trait cannot have.
pub(super) fn trait_<'db>(v: &V<'_, '_, 'db>, node: ast::Node<'db>, out: &mut Out) {
    let Some(trait_) = ast::Trait::cast(node) else { return };
    no_mangle(v, node, out);
    if trait_.auto_token().is_none() {
        return;
    }
    let Some(name) = trait_.name() else { return };
    let ident = v.span(name.syntax().text_range());
    if let Some(const_) = const_token(node) {
        let mut diag = Diag::error(None, "auto traits cannot be const", v.span(const_.text_range()));
        diag.help("remove the `const` keyword");
        v.emit(out, node, diag);
    }
    if let Some(params) = trait_.generic_param_list()
        && params.generic_params().any(|it| v.kept(it.syntax()))
    {
        let span = v.span(params.syntax().text_range());
        // clone: a span, on the error path
        let mut diag = Diag::error(Some("E0567"), "auto traits cannot have generic parameters", span.clone());
        diag.suggestion_with_style(
            // alloc: the error's suggestion
            vec![(span, String::new())],
            "remove the parameters",
            Applicability::MachineApplicable,
            SuggestionStyle::CompletelyHidden,
        );
        // clone: a span, on the error path
        diag.span_label(ident.clone(), "auto trait cannot have generic parameters");
        v.emit(out, node, diag);
    }
    if let Some(list) = trait_.type_bound_list() {
        let mut bounds = list.bounds().peekable();
        if bounds.peek().is_some() {
            let mut diag: Option<Diag> = None;
            let mut last = TextRange::empty(name.syntax().text_range().end());
            for bound in bounds {
                let range = bound.syntax().text_range();
                last = range;
                match &mut diag {
                    None => {
                        diag = Some(Diag::error(
                            Some("E0568"),
                            "auto traits cannot have super traits or lifetime bounds",
                            v.span(range),
                        ));
                    }
                    Some(diag) => {
                        diag.primary(v.span(range));
                    }
                }
            }
            if let Some(mut diag) = diag {
                let removal = TextRange::new(name.syntax().text_range().end(), last.end());
                auto_trait_bounds(v, &mut diag, removal, &ident);
                v.emit(out, node, diag);
            }
        }
    }
    if let Some(where_) = trait_.where_clause()
        && where_.predicates().any(|it| v.kept(it.syntax()))
    {
        let range = where_.syntax().text_range();
        let mut diag =
            Diag::error(Some("E0568"), "auto traits cannot have super traits or lifetime bounds", v.span(range));
        auto_trait_bounds(v, &mut diag, range, &ident);
        v.emit(out, node, diag);
    }
    if let Some(list) = trait_.assoc_item_list() {
        let items = || list.assoc_items().filter(|it| v.kept(it.syntax()));
        let names = items().filter_map(|it| item_name(it.syntax()));
        let mut diag: Option<Diag> = None;
        for name in names {
            let span = v.span(name.syntax().text_range());
            match &mut diag {
                None => diag = Some(Diag::error(Some("E0380"), "auto traits cannot have associated items", span)),
                Some(diag) => {
                    diag.primary(span);
                }
            }
        }
        if let (Some(mut diag), Some(first), Some(last)) = (diag, items().next(), items().last()) {
            let total = item_range(first.syntax()).cover(item_range(last.syntax()));
            diag.suggestion_with_style(
                // alloc: the error's suggestion
                vec![(v.span(total), String::new())],
                "remove the associated items",
                Applicability::MachineApplicable,
                SuggestionStyle::CompletelyHidden,
            );
            diag.span_label(ident, "auto traits cannot have associated items");
            v.emit(out, node, diag);
        }
    }
}

/// The suggestion and label of `AutoTraitBounds`.
fn auto_trait_bounds(v: &V<'_, '_, '_>, diag: &mut Diag, removal: TextRange, ident: &Span) {
    diag.suggestion_with_style(
        // alloc: the error's suggestion
        vec![(v.span(removal), String::new())],
        "remove the super traits or lifetime bounds",
        Applicability::MachineApplicable,
        SuggestionStyle::CompletelyHidden,
    );
    // clone: a span, on the error path
    diag.span_label(ident.clone(), "auto traits cannot have super traits or lifetime bounds");
}

/// An `extern` block: its visibility, and in Rust 2024 that it is `unsafe`.
pub(super) fn extern_block<'db>(v: &V<'_, '_, 'db>, node: ast::Node<'db>, out: &mut Out) {
    let Some(block) = ast::ExternBlock::cast(node) else { return };
    let vis = node.children().find_map(ast::Visibility::cast);
    visibility(v, node, vis, VisNote::IndividualForeignItems, out);
    let range = item_range(node);
    if block.unsafe_token().is_none() && v.edition_at(range.start()).at_least_2024() {
        v.emit(out, node, Diag::error(None, "extern blocks must be unsafe", v.span(range)));
    }
}

/// An enum: `#[no_mangle]`, the visibilities of its variants and their fields.
pub(super) fn enum_<'db>(v: &V<'_, '_, 'db>, node: ast::Node<'db>, out: &mut Out) {
    let Some(enum_) = ast::Enum::cast(node) else { return };
    no_mangle(v, node, out);
    let Some(variants) = enum_.variant_list() else { return };
    for variant in variants.variants().filter(|it| v.kept(it.syntax())) {
        visibility(v, node, variant.visibility(), VisNote::EnumVariant, out);
        match variant.field_list() {
            Some(ast::FieldList::RecordFieldList(fields)) => {
                for field in fields.fields().filter(|it| v.kept(it.syntax())) {
                    visibility(v, node, field.visibility(), VisNote::EnumVariant, out);
                }
            }
            Some(ast::FieldList::TupleFieldList(fields)) => {
                for field in fields.fields().filter(|it| v.kept(it.syntax())) {
                    visibility(v, node, field.visibility(), VisNote::EnumVariant, out);
                }
            }
            None => {}
        }
    }
}

/// A module: `#[no_mangle]`, and the name of one loaded from a file of its name.
pub(super) fn module<'db>(v: &V<'_, '_, 'db>, node: ast::Node<'db>, out: &mut Out) {
    let Some(module) = ast::Module::cast(node) else { return };
    no_mangle(v, node, out);
    let Some(name) = module.name() else { return };
    let text = unraw(name.text());
    if module.item_list().is_some() || text.is_ascii() || v.has_attr(node, "path") {
        return;
    }
    let mut diag = Diag::error(
        Some("E0754"),
        // alloc: the error's message
        format!("trying to load file for module `{text}` with non-ascii identifier name"),
        v.span(name.syntax().text_range()),
    );
    diag.help("consider using the `#[path]` attribute to specify filesystem path");
    v.emit(out, node, diag);
}

/// A struct: `#[no_mangle]`, and what a scalable vector must be.
pub(super) fn struct_<'db>(v: &V<'_, '_, 'db>, node: ast::Node<'db>, out: &mut Out) {
    let Some(struct_) = ast::Struct::cast(node) else { return };
    no_mangle(v, node, out);
    let Some(attr) = v.attr(node, "rustc_scalable_vector") else { return };
    if !matches!(struct_.field_list(), Some(ast::FieldList::TupleFieldList(_))) {
        let diag = Diag::error(None, "scalable vectors must be tuple structs", v.span(item_range(node)));
        v.emit(out, node, diag);
    }
    // `Arch::supports_scalable_vectors`.
    if !matches!(v.arch(), Some("aarch64" | "riscv32" | "riscv64")) {
        let diag = Diag::error(None, "scalable vectors are not supported on this architecture", v.span(attr));
        v.emit(out, node, diag);
    }
}

/// A union: `#[no_mangle]`, and that it has a field.
pub(super) fn union_<'db>(v: &V<'_, '_, 'db>, node: ast::Node<'db>, out: &mut Out) {
    let Some(union_) = ast::Union::cast(node) else { return };
    no_mangle(v, node, out);
    let Some(fields) = union_.record_field_list() else { return };
    if fields.fields().any(|it| v.kept(it.syntax())) {
        return;
    }
    v.emit(out, node, Diag::error(None, "unions cannot have zero fields", v.span(item_range(node))));
}

/// A `const` item, free or associated: one without a body is refused but in a trait.
pub(super) fn const_<'db>(v: &V<'_, '_, 'db>, node: ast::Node<'db>, out: &mut Out) {
    let Some(item) = ast::Const::cast(node) else { return };
    let ctxt = v.ctxt(node);
    match &ctxt {
        Ctxt::Free | Ctxt::Foreign(_) => {
            no_mangle(v, node, out);
            defaultness(v, node, item.default_token(), out);
            if matches!(ctxt, Ctxt::Free) && item.body().is_none() {
                without_body(v, node, "free constant item without body", "constant", " = <expr>;", out);
            }
        }
        Ctxt::Trait(_) | Ctxt::TraitImpl(_) | Ctxt::Impl(_) => {
            assoc(v, node, item.default_token(), &ctxt, out);
            if !matches!(ctxt, Ctxt::Trait(_)) && item.body().is_none() {
                let message = "associated constant in `impl` without body";
                without_body(v, node, message, "constant", " = <expr>;", out);
            }
            assoc_vis(v, node, &ctxt, out);
            if let Some(underscore) = item.underscore_token() {
                let span = v.span(underscore.text_range());
                // clone: a span, on the error path
                let mut diag = Diag::error(None, "`const` items in this context need a name", span.clone());
                diag.span_label(span, "`_` is not a valid name for this `const` item");
                v.emit(out, node, diag);
            }
        }
    }
}

/// A `static`: its safety and its body, or in an `extern` block what a foreign static cannot
/// have. rustc's parser makes one in a trait or impl a `const` (with its error
/// `AssociatedStaticItemNotAllowed`), checked as one.
pub(super) fn static_<'db>(v: &V<'_, '_, 'db>, node: ast::Node<'db>, out: &mut Out) {
    let Some(item) = ast::Static::cast(node) else { return };
    let ctxt = v.ctxt(node);
    match &ctxt {
        Ctxt::Foreign(block) => {
            safety_in_extern(v, node, item.safe_token().or(item.unsafe_token()), *block, out);
            if let (Some(name), Some(body)) = (item.name(), item.body()) {
                body_in_extern(v, node, name, body.syntax().text_range(), *block, "static", out);
            }
            ascii_in_extern(v, node, item.name(), *block, out);
        }
        Ctxt::Trait(_) | Ctxt::TraitImpl(_) | Ctxt::Impl(_) => {
            assoc(v, node, None, &ctxt, out);
            if !matches!(ctxt, Ctxt::Trait(_)) && item.body().is_none() {
                let message = "associated constant in `impl` without body";
                without_body(v, node, message, "constant", " = <expr>;", out);
            }
            assoc_vis(v, node, &ctxt, out);
        }
        Ctxt::Free => {
            no_mangle(v, node, out);
            safe_outside_extern(v, node, item.safe_token(), out);
            if item.unsafe_token().is_some() {
                let diag = Diag::error(
                    None,
                    "static items cannot be declared with `unsafe` safety qualifier outside of `extern` block",
                    v.span(item_range(node)),
                );
                v.emit(out, node, diag);
            }
            if item.body().is_none() {
                without_body(v, node, "free static item without body", "static", " = <expr>;", out);
            }
        }
    }
}

/// A type alias, free, associated or foreign: one without a type is refused but in a trait
/// or an `extern` block.
pub(super) fn type_alias<'db>(v: &V<'_, '_, 'db>, node: ast::Node<'db>, out: &mut Out) {
    let Some(alias) = ast::TypeAlias::cast(node) else { return };
    let ctxt = v.ctxt(node);
    match &ctxt {
        Ctxt::Free => {
            no_mangle(v, node, out);
            defaultness(v, node, alias.default_token(), out);
            if alias.ty().is_none() {
                without_body(v, node, "free type alias without body", "type", " = <type>;", out);
            }
            no_bounds(v, node, alias.type_bound_list(), "this context", out);
            where_clause_location(v, node, alias, out);
        }
        Ctxt::Foreign(block) => foreign_type(v, node, alias, *block, out),
        Ctxt::Trait(_) | Ctxt::TraitImpl(_) | Ctxt::Impl(_) => {
            assoc(v, node, alias.default_token(), &ctxt, out);
            if matches!(ctxt, Ctxt::TraitImpl(_) | Ctxt::Impl(_)) {
                if alias.ty().is_none() {
                    let message = "associated type in `impl` without body";
                    without_body(v, node, message, "type", " = <type>;", out);
                }
                no_bounds(v, node, alias.type_bound_list(), "`impl`s", out);
            }
            assoc_vis(v, node, &ctxt, out);
        }
    }
}

/// The `where` clauses of a type alias: before its type and after it.
fn where_clauses(alias: ast::TypeAlias<'_>) -> (Option<ast::WhereClause<'_>>, Option<ast::WhereClause<'_>>) {
    let eq = alias.eq_token().map(|it| it.text_range().start());
    let mut before = None;
    let mut after = None;
    for clause in alias.syntax().children().filter_map(ast::WhereClause::cast) {
        match eq {
            Some(eq) if clause.syntax().text_range().start() > eq => after = Some(clause),
            _ => before = Some(clause),
        }
    }
    (before, after)
}

/// A free type alias's `where` clauses: with `checked_type_aliases`,
/// `check_type_alias_where_clause_location`; without it, one after the type is an error.
fn where_clause_location(v: &V<'_, '_, '_>, node: ast::Node<'_>, alias: ast::TypeAlias<'_>, out: &mut Out) {
    let (before, after) = where_clauses(alias);
    if !v.feature("checked_type_aliases") {
        let Some(after) = after else { return };
        let mut diag = Diag::error(
            None,
            "where clauses are not allowed after the type for type aliases",
            v.span(after.syntax().text_range()),
        );
        diag.note("see issue #112792 <https://github.com/rust-lang/rust/issues/112792> for more information");
        diag.help("add `#![feature(checked_type_aliases)]` to the crate attributes to enable");
        v.emit(out, node, diag);
        return;
    }
    let (Some(ty), Some(before)) = (alias.ty(), before) else { return };
    let span = v.span(before.syntax().text_range());
    let message = "where clauses are not allowed before the type for type aliases";
    // clone: a span, on the error path
    let mut diag = Diag::error(None, message, span.clone());
    diag.note("see issue #89122 <https://github.com/rust-lang/rust/issues/89122> for more information");
    let after_preds = after.is_some_and(|it| it.predicates().next().is_some());
    if before.predicates().next().is_some() || after.is_none() {
        // alloc: the error's suggestion
        let mut snippet = String::new();
        let mut needs_comma = after_preds;
        if after.is_none() {
            snippet.push_str(" where ");
        } else if !needs_comma {
            snippet.push(' ');
        }
        for pred in before.predicates() {
            if needs_comma {
                snippet.push_str(", ");
            }
            needs_comma = true;
            let edition = v.edition_at(pred.syntax().text_range().start());
            pretty::where_predicate(&mut snippet, pred, edition);
        }
        // rustc's printer writes a break it ends on as nothing.
        snippet.truncate(snippet.trim_end().len());
        let right = match after {
            Some(after) => after.syntax().text_range().end(),
            None => ty.syntax().text_range().end(),
        };
        diag.multipart_suggestion(
            "move it to the end of the type declaration",
            // alloc: as above
            vec![(span, String::new()), (v.span(TextRange::empty(right)), snippet)],
            Applicability::MachineApplicable,
        );
    } else {
        diag.suggestion(span, "remove this `where`", "", Applicability::MachineApplicable);
    }
    v.emit(out, node, diag);
}

/// `visit_foreign_item` of a function, before `visit_fn`: `default`, a body, qualifiers, its
/// name, its ABI's signature, `#[track_caller]`.
fn foreign_fn<'db>(
    v: &V<'_, '_, 'db>,
    node: ast::Node<'db>,
    item: ast::Fn<'db>,
    block: InFile<ast::ExternBlock<'db>>,
    out: &mut Out,
) {
    defaultness(v, node, item.default_token(), out);
    if let (Some(name), Some(body)) = (item.name(), item.body()) {
        let ident = v.span(name.syntax().text_range());
        // clone: a span, on the error path
        let mut diag = Diag::error(None, "incorrect function inside `extern` block", ident.clone());
        diag.help(
            "you might have meant to write a function accessible through FFI, which can be done by writing `extern fn` outside of the `extern` block",
        );
        diag.note(EXTERN_NOTE);
        diag.span_label(ident, "cannot have a body");
        diag.suggestion(v.span(body.syntax().text_range()), "remove the invalid body", ";", Applicability::MaybeIncorrect);
        diag.span_label(
            block_head(v, block),
            "`extern` blocks define existing foreign functions and functions inside of them cannot have a body",
        );
        v.emit(out, node, diag);
    }
    let mut qualifier = |range: TextRange, kw: &str| {
        // alloc: the error's message
        let message = format!("functions in `extern` blocks cannot have `{kw}` qualifier");
        let mut diag = Diag::error(None, message, v.span(range));
        // alloc: the error's suggestion
        diag.suggestion(v.span(range), format!("remove the `{kw}` qualifier"), "", Applicability::MaybeIncorrect);
        diag.span_label(block_head(v, block), "in this `extern` block");
        v.emit(out, node, diag);
    };
    if let Some((range, kw)) = fns::coroutine_kind(item) {
        qualifier(range, kw);
    }
    if let Some(const_) = item.const_token() {
        qualifier(const_.text_range(), "const");
    }
    if let Some(abi) = item.abi() {
        qualifier(abi.syntax().text_range(), "extern");
    }
    ascii_in_extern(v, node, item.name(), block, out);
    // `extern_mod_abi`: the block's ABI when it names one rustc knows.
    let known = block.value.abi().and_then(fns::abi_string).and_then(fns::known_abi_of);
    fns::extern_signature(v, node, item, known.unwrap_or("C"), out);
    if let Some(attr) = v.attr(node, "track_caller")
        && known != Some("Rust")
    {
        fns::requires_rust_abi(v, node, attr, block_head(v, block), out);
    }
}

/// `visit_foreign_item` of a type: `default`, a body, bounds, generics, its name.
fn foreign_type(
    v: &V<'_, '_, '_>,
    node: ast::Node<'_>,
    alias: ast::TypeAlias<'_>,
    block: InFile<ast::ExternBlock<'_>>,
    out: &mut Out,
) {
    defaultness(v, node, alias.default_token(), out);
    if let (Some(name), Some(ty)) = (alias.name(), alias.ty()) {
        body_in_extern(v, node, name, ty.syntax().text_range(), block, "type", out);
    }
    no_bounds(v, node, alias.type_bound_list(), "`extern` blocks", out);
    let mut cannot_have = |range: TextRange, descr: &str, remove: &str| {
        let span = v.span(range);
        // alloc: the error's message, and a span clone on the error path
        let mut diag = Diag::error(None, format!("`type`s inside `extern` blocks cannot have {descr}"), span.clone());
        diag.note(EXTERN_NOTE);
        // alloc: the error's suggestion
        diag.suggestion(span, format!("remove the {remove}"), "", Applicability::MaybeIncorrect);
        diag.span_label(block_head(v, block), "`extern` block begins here");
        v.emit(out, node, diag);
    };
    if let Some(params) = alias.generic_param_list()
        && params.generic_params().any(|it| v.kept(it.syntax()))
    {
        cannot_have(params.syntax().text_range(), "generic parameters", "generic parameters");
    }
    let (before, after) = where_clauses(alias);
    for clause in [before, after].into_iter().flatten() {
        cannot_have(clause.syntax().text_range(), "`where` clauses", "`where` clause");
    }
    ascii_in_extern(v, node, alias.name(), block, out);
}

/// `BodyInExtern`: a foreign type or static with a body.
fn body_in_extern(
    v: &V<'_, '_, '_>,
    node: ast::Node<'_>,
    name: ast::Name<'_>,
    body: TextRange,
    block: InFile<ast::ExternBlock<'_>>,
    kind: &str,
    out: &mut Out,
) {
    let ident = v.span(name.syntax().text_range());
    // alloc: the error's message, and a span clone on the error path
    let mut diag = Diag::error(None, format!("incorrect `{kind}` inside `extern` block"), ident.clone());
    diag.note(EXTERN_NOTE);
    diag.span_label(ident, "cannot have a body");
    diag.span_label(v.span(body), "the invalid body");
    diag.span_label(
        block_head(v, block),
        // alloc: the error's label
        format!("`extern` blocks define existing foreign {kind}s and {kind}s inside of them cannot have a body"),
    );
    v.emit(out, node, diag);
}

/// `check_foreign_item_ascii_only`.
fn ascii_in_extern(
    v: &V<'_, '_, '_>,
    node: ast::Node<'_>,
    name: Option<ast::Name<'_>>,
    block: InFile<ast::ExternBlock<'_>>,
    out: &mut Out,
) {
    let Some(name) = name else { return };
    if unraw(name.text()).is_ascii() {
        return;
    }
    let mut diag =
        Diag::error(None, "items in `extern` blocks cannot use non-ascii identifiers", v.span(name.syntax().text_range()));
    diag.note(
        "this limitation may be lifted in the future; see issue #83942 <https://github.com/rust-lang/rust/issues/83942> for more information",
    );
    diag.span_label(block_head(v, block), "in this `extern` block");
    v.emit(out, node, diag);
}
