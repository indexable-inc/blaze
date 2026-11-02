//! E0116, E0390, E0118 and E0785: an inherent impl of a type defined outside the crate, of a
//! primitive type, of a parameter, projection or opaque type, or of a `dyn` of auto traits only
//! (hir's `IncoherentImpl`, from
//! `is_inherent_impl_coherent`); and what the impl kinds share: the impl a diagnostic is of and
//! the spans rustc gives its parts.
//!
//! Follows rustc's `compiler/rustc_hir_analysis/src/coherence/inherent_impls.rs`
//! (`InherentCollect::check_item`, `check_def_id`, `check_primitive_impl`) and the structs
//! `InherentTyOutsideNew`, `InherentTyOutside`, `InherentTyOutsideRelevant`,
//! `InherentTyOutsidePrimitive`, `InherentPrimitiveTy`, `InherentNominal` and `InherentDyn` of
//! `compiler/rustc_hir_analysis/src/diagnostics.rs` (MIT or Apache-2.0).

use blaze_items::{Fact, ItemId, ItemKey};
use hir::{HirFileId, InFile};
use hir_def::{
    AdtId, ImplId, TraitId,
    signatures::{
        ConstFlags, ConstSignature, EnumFlags, EnumSignature, FnFlags, FunctionSignature,
        StructFlags, StructSignature, TraitFlags, TraitSignature, TypeAliasFlags,
        TypeAliasSignature, UnionSignature,
    },
    unstable_features::UnstableFeatures,
};
use intern::sym;
use syntax::{
    AstPtr, TextRange,
    view::{self as ast, AstNode, HasGenericParams as _, HasName as _, HasTypeBounds as _},
};

use crate::{
    cx::{Cx, def_range_view as def_range, item_range_view as item_range, signature_range_view as signature_range},
    lower::{Found, Phase},
    model::{Diag, Span},
};

/// An impl a diagnostic is of: hir's item and its syntax, a view of a tree `cx.vsema` gave out.
pub(crate) struct ImplAt<'db> {
    pub(crate) impl_: hir::Impl,
    pub(crate) id: ImplId,
    pub(crate) node: InFile<ast::Impl<'db>>,
}

impl<'db> ImplAt<'db> {
    /// The impl `ptr` in `file_id` points to, as hir's diagnostics name an impl.
    pub(crate) fn of_ptr(
        cx: &Cx<'_, 'db>,
        file_id: HirFileId,
        ptr: AstPtr<syntax::ast::Impl>,
    ) -> Option<ImplAt<'db>> {
        let node = cx.node_view(InFile::new(file_id, ptr));
        let impl_ = cx.vsema.to_def(node.value)?;
        let id = ImplId::try_from(impl_).ok()?;
        Some(ImplAt { impl_, id, node })
    }

    /// `impl_`, when it is written in source (a builtin derive's is not).
    pub(crate) fn of(cx: &Cx<'_, 'db>, impl_: hir::Impl) -> Option<ImplAt<'db>> {
        let id = ImplId::try_from(impl_).ok()?;
        let node = cx.vsema.source(impl_)?;
        Some(ImplAt { impl_, id, node })
    }

    /// rustc's `def_span` of the impl: from `impl` (or `unsafe`) to the end of its header,
    /// where clause included.
    pub(crate) fn span(&self, cx: &Cx<'_, '_>) -> Span {
        cx.span(self.node.map(|it| def_range(it.syntax())))
    }

    /// The span of a node of the impl.
    pub(crate) fn span_of(&self, cx: &Cx<'_, '_>, node: impl AstNode<'db>) -> Span {
        cx.span(self.node.with_value(node.syntax().text_range()))
    }

    /// The type the impl is for, as written.
    pub(crate) fn self_ty_span(&self, cx: &Cx<'_, '_>) -> Span {
        match self.node.value.self_ty() {
            Some(ty) => self.span_of(cx, ty),
            None => self.span(cx),
        }
    }

    /// The path of the trait it implements, as written.
    pub(crate) fn trait_path(&self) -> Option<ast::Path<'db>> {
        match self.node.value.trait_()? {
            ast::Type::PathType(it) => it.path(),
            _ => None,
        }
    }

    /// The span of the trait's path, its generic arguments included.
    pub(crate) fn trait_span(&self, cx: &Cx<'_, '_>) -> Span {
        match self.node.value.trait_() {
            Some(trait_) => self.span_of(cx, trait_),
            None => self.span(cx),
        }
    }

    pub(crate) fn key(&self, cx: &Cx<'_, '_>) -> ItemKey {
        cx.key(ItemId::Impl(self.id))
    }

    /// `Fact::Coherence` of the impl.
    pub(crate) fn coherence(&self, cx: &Cx<'_, '_>) -> Fact {
        Fact::Coherence(self.key(cx))
    }
}

/// blaze's own error where it cannot find the impl hir gave an error of: the program is
/// refused all the same (no-fallback).
pub(crate) fn unresolved(cx: &Cx<'_, '_>, file_id: HirFileId, ptr: AstPtr<syntax::ast::Impl>, what: &str) -> Vec<Found> {
    let span = cx.span(InFile::new(file_id, ptr.text_range()));
    // alloc: the error's text, for an error hir found
    let diag = Diag::error(None, format!("blaze cannot find the impl {what} is of"), span);
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Items)]
}

/// rustc's `def_span` of a type alias: from its start to the end of its name and generics.
pub(crate) fn alias_def_range(alias: ast::TypeAlias<'_>) -> TextRange {
    let range = item_range(alias.syntax());
    let end = match (alias.generic_param_list(), alias.name()) {
        (Some(generics), _) => generics.syntax().text_range().end(),
        (None, Some(name)) => name.syntax().text_range().end(),
        (None, None) => range.end(),
    };
    TextRange::new(range.start(), end.max(range.start()))
}

/// Whether the type is one rustc's `check_primitive_impl` takes: not an ADT, a `dyn` or a
/// parameter.
pub(crate) fn is_primitive(ty: &hir::Type<'_>) -> bool {
    ty.as_builtin().is_some()
        || ty.is_reference()
        || ty.is_raw_ptr()
        || ty.is_slice()
        || ty.is_array()
        || ty.is_tuple()
        || ty.is_never()
        || ty.is_str()
        || ty.is_fn()
}

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::IncoherentImpl) -> Vec<Found> {
    let Some(at) = ImplAt::of_ptr(cx, d.file_id, d.impl_) else {
        return unresolved(cx, d.file_id, d.impl_, "an incoherent inherent impl error");
    };
    let self_ty = at.impl_.self_ty(cx.db);
    let span = at.span(cx);
    let rustc_attrs =
        UnstableFeatures::query(cx.db, cx.krate.base()).is_enabled(&sym::rustc_attrs);
    let first_unallowed = || first_item_without_allow(cx, at.id);
    let mut diag = if is_primitive(&self_ty) {
        if rustc_attrs {
            // With every item allowed rustc takes the impl, as hir does not when it has none.
            // alloc: empty, allocates nothing
            let Some(item) = first_unallowed() else { return Vec::new() };
            let mut diag = Diag::error(
                Some("E0390"),
                "cannot define inherent `impl` for primitive types outside of `core`",
                span,
            );
            diag.help("consider moving this inherent impl into `core` if possible");
            diag.span_help(item, "alternatively add `#[rustc_allow_incoherent_impl]` to the relevant impl items");
            diag
        } else {
            let mut diag = Diag::error(Some("E0390"), "cannot define inherent `impl` for primitive types", span);
            diag.help("consider using an extension trait instead");
            if let Some((inner, _)) = self_ty.as_reference() {
                // alloc: the error's text, for an error hir found
                diag.note(format!(
                    "you could also try moving the reference to uses of `{}` (such as `self`) within the implementation",
                    cx.ty(&inner)
                ));
            }
            diag
        }
    } else if let Some(owner) = nominal(&self_ty) {
        if rustc_attrs {
            if !has_incoherent_impls(cx, owner) {
                let mut diag = Diag::error(
                    Some("E0390"),
                    "cannot define inherent `impl` for a type outside of the crate where the type is defined",
                    // clone: on the error path, for an error hir found
                    span.clone(),
                );
                diag.help("consider moving this inherent impl into the crate defining the type if possible");
                diag.span_help(
                    span,
                    "alternatively add `#[rustc_has_incoherent_inherent_impls]` to the type and `#[rustc_allow_incoherent_impl]` to the relevant impl items",
                );
                diag
            } else {
                // alloc: empty, allocates nothing
                let Some(item) = first_unallowed() else { return Vec::new() };
                let mut diag = Diag::error(
                    Some("E0390"),
                    "cannot define inherent `impl` for a type outside of the crate where the type is defined",
                    span,
                );
                diag.help("consider moving this inherent impl into the crate defining the type if possible");
                diag.span_help(item, "alternatively add `#[rustc_allow_incoherent_impl]` to the relevant impl items");
                diag
            }
        } else {
            outside_new(cx, &at, span)
        }
    } else if self_ty.as_type_param(cx.db).is_some()
        || self_ty.as_associated_type_parent_trait(cx.db).is_some()
        || self_ty.as_impl_traits(cx.db).is_some()
    {
        // clone: on the error path, for an error hir found
        let mut diag = Diag::error(Some("E0118"), "no nominal type found for inherent implementation", span.clone());
        diag.span_label(span, "impl requires a nominal type");
        diag.note("either implement a trait on it or create a newtype to wrap it instead");
        diag
    } else {
        // clone: on the error path, for an error hir found
        let mut diag = Diag::error(Some("E0785"), "cannot define inherent `impl` for a dyn auto trait", span.clone());
        diag.span_label(span, "impl requires at least one non-auto trait");
        diag.note("define and implement a new trait or type instead");
        diag
    };
    let ty = cx.ty(&self_ty);
    // alloc: the error's text, for an error hir found
    diag.fact(at.coherence(cx), format!("the coherence of the inherent impl of `{ty}`"));
    match nominal(&self_ty) {
        Some(Nominal::Adt(adt)) => {
            // alloc: the error's text, for an error hir found
            diag.fact(Fact::Signature(cx.key(ItemId::Adt(adt.into()))), format!("where `{ty}` is defined"));
        }
        Some(Nominal::Trait(trait_)) => {
            // alloc: the error's text, for an error hir found
            diag.fact(Fact::Signature(cx.key(ItemId::Trait(trait_.into()))), format!("where `{ty}` is defined"));
        }
        None => {}
    }
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Items)]
}

/// What an inherent impl's self type is defined by, when it is not a primitive.
#[derive(Clone, Copy)]
enum Nominal {
    Adt(hir::Adt),
    Trait(hir::Trait),
}

fn nominal(ty: &hir::Type<'_>) -> Option<Nominal> {
    if let Some(adt) = ty.as_adt() {
        return Some(Nominal::Adt(adt));
    }
    ty.as_dyn_trait().map(Nominal::Trait)
}

/// `InherentTyOutsideNew`: E0116, with the note on an alias when the impl names one.
fn outside_new<'db>(cx: &Cx<'_, 'db>, at: &ImplAt<'db>, span: Span) -> Diag {
    let mut diag = Diag::error(
        Some("E0116"),
        "cannot define inherent `impl` for a type outside of the crate where the type is defined",
        // clone: on the error path, for an error hir found
        span.clone(),
    );
    diag.span_label(span, "impl for type defined outside of crate");
    diag.help("consider defining a trait and implementing it for the type or using a newtype wrapper like `struct MyType(ExternalType);` and implement it");
    diag.note("for more details about the orphan rules, see <https://doc.rust-lang.org/reference/items/implementations.html?highlight=orphan#orphan-rules>");
    if let Some(alias) = self_ty_alias(cx, at)
        && let Some(source) = cx.vsema.source(alias)
    {
        let alias_span = cx.span(source.map(alias_def_range));
        let ty_name = cx.def_path(hir::ModuleDef::TypeAlias(alias));
        let alias_ty = cx.ty(&alias.ty(cx.db));
        diag.span_note(
            alias_span,
            // alloc: the error's text, for an error hir found
            format!("`{ty_name}` does not define a new type, only an alias of `{alias_ty}` defined here"),
        );
        // alloc: the error's text, for an error hir found
        diag.fact(Fact::Signature(cx.key(ItemId::Alias(alias.into()))), format!("what `{ty_name}` is an alias of"));
    }
    diag
}

/// The type alias the impl's self type is written as, when it is a path to one.
fn self_ty_alias<'db>(cx: &Cx<'_, 'db>, at: &ImplAt<'db>) -> Option<hir::TypeAlias> {
    let ast::Type::PathType(ty) = at.node.value.self_ty()? else { return None };
    match cx.vsema.resolve_path(ty.path()?)? {
        hir::PathResolution::Def(hir::ModuleDef::TypeAlias(alias)) => Some(alias),
        _ => None,
    }
}

/// Whether the type is marked `#[rustc_has_incoherent_inherent_impls]`.
fn has_incoherent_impls(cx: &Cx<'_, '_>, owner: Nominal) -> bool {
    let db = cx.db;
    match owner {
        Nominal::Adt(adt) => match AdtId::from(adt) {
            AdtId::StructId(id) => {
                StructSignature::of(db, id).flags.contains(StructFlags::RUSTC_HAS_INCOHERENT_INHERENT_IMPLS)
            }
            AdtId::UnionId(id) => {
                UnionSignature::of(db, id).flags.contains(StructFlags::RUSTC_HAS_INCOHERENT_INHERENT_IMPLS)
            }
            AdtId::EnumId(id) => {
                EnumSignature::of(db, id).flags.contains(EnumFlags::RUSTC_HAS_INCOHERENT_INHERENT_IMPLS)
            }
        },
        Nominal::Trait(trait_) => TraitSignature::of(db, TraitId::from(trait_))
            .flags
            .contains(TraitFlags::RUSTC_HAS_INCOHERENT_INHERENT_IMPLS),
    }
}

/// rustc's `def_span` of the first item of the impl not marked `#[rustc_allow_incoherent_impl]`.
fn first_item_without_allow(cx: &Cx<'_, '_>, id: ImplId) -> Option<Span> {
    let db = cx.db;
    let items = id.impl_items(db);
    let item = items.items.iter().find_map(|&(_, item)| {
        let allowed = match item {
            hir_def::AssocItemId::FunctionId(it) => {
                FunctionSignature::of(db, it).flags.contains(FnFlags::RUSTC_ALLOW_INCOHERENT_IMPL)
            }
            hir_def::AssocItemId::ConstId(it) => {
                ConstSignature::of(db, it).flags.contains(ConstFlags::RUSTC_ALLOW_INCOHERENT_IMPL)
            }
            hir_def::AssocItemId::TypeAliasId(it) => {
                TypeAliasSignature::of(db, it).flags.contains(TypeAliasFlags::RUSTC_ALLOW_INCOHERENT_IMPL)
            }
        };
        (!allowed).then_some(hir::AssocItem::from(item))
    })?;
    assoc_def_span(cx, item)
}

/// rustc's `def_span` of an associated item: a function's signature, a const up to its type,
/// a type up to its last bound (or its name and generics).
pub(crate) fn assoc_def_span(cx: &Cx<'_, '_>, item: hir::AssocItem) -> Option<Span> {
    match item {
        hir::AssocItem::Function(it) => {
            let source = cx.vsema.source(it)?;
            Some(cx.span(source.map(signature_range)))
        }
        hir::AssocItem::Const(it) => {
            let source = cx.vsema.source(it)?;
            Some(cx.span(source.map(|it| {
                let range = item_range(it.syntax());
                let end = it.ty().map_or(range.end(), |ty| ty.syntax().text_range().end());
                TextRange::new(range.start(), end)
            })))
        }
        hir::AssocItem::TypeAlias(it) => {
            let source = cx.vsema.source(it)?;
            Some(cx.span(source.map(|it| {
                let range = alias_def_range(it);
                let end = it
                    .type_bound_list()
                    .and_then(|bounds| bounds.bounds().last())
                    .map_or(range.end(), |bound| bound.syntax().text_range().end());
                TextRange::new(range.start(), end.max(range.end()))
            })))
        }
    }
}

/// The span rustc's resolver gives an associated item (the AST's `item.span`): all of it, its
/// body included, from after its attributes.
pub(crate) fn assoc_item_span(cx: &Cx<'_, '_>, item: hir::AssocItem) -> Option<Span> {
    let range = match item {
        hir::AssocItem::Function(it) => cx.vsema.source(it)?.map(|it| item_range(it.syntax())),
        hir::AssocItem::Const(it) => cx.vsema.source(it)?.map(|it| item_range(it.syntax())),
        hir::AssocItem::TypeAlias(it) => cx.vsema.source(it)?.map(|it| item_range(it.syntax())),
    };
    Some(cx.span(range))
}

/// The identifier of an associated item, as written.
pub(crate) fn assoc_name_span(cx: &Cx<'_, '_>, item: hir::AssocItem) -> Option<Span> {
    let name = match item {
        hir::AssocItem::Function(it) => cx.vsema.source(it)?.map(|it| it.name()),
        hir::AssocItem::Const(it) => cx.vsema.source(it)?.map(|it| it.name()),
        hir::AssocItem::TypeAlias(it) => cx.vsema.source(it)?.map(|it| it.name()),
    };
    let range = name.value?.syntax().text_range();
    Some(cx.span(InFile::new(name.file_id, range)))
}
