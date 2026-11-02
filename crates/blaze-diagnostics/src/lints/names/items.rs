//! The lints of items rustc gives, each found as its pass finds it:
//!
//! - before the abort, `rustc_hir_analysis`'s of collecting and checking items
//!   (`invalid_type_param_default`, `uninhabited_static`: `statics`) and resolve's late pass
//!   over lifetimes (`unused_lifetimes`, `single_use_lifetimes`: `lifetimes`);
//! - after it, `rustc_privacy`'s `check_private_in_public` (`private_interfaces`,
//!   `private_bounds`, E0446: `privacy`), `rustc_lint`'s late module pass over each item
//!   (`externs`' `improper_ctypes`, `variants`, `naming`, `derives`, `bounds`, `mangle`, `publicity`,
//!   `track_caller`, `docs`, `traits`, `non_local`, `runtime_symbols`, `lifetime_syntax`, `c_void`), and `clashing_extern_declarations` (`externs`).
//!
//! `walk` gives the items in the order rustc's passes visit them; `visibility` the defs their
//! effective visibilities (`super::effective`) are kept by.
//!
//! Follows `compiler/rustc_interface/src/passes.rs` (`run_required_analyses`, `analysis`,
//! `misc_checking_3`), `compiler/rustc_lint/src/late.rs` (`late_lint_mod_inner`: the crate's
//! `check_crate` before the root module's items) and `compiler/rustc_lint/src/lib.rs`
//! (`BuiltinCombinedLateLintModPass`: the order of the passes at each item) (MIT or
//! Apache-2.0).

mod bounds;
mod c_void;
mod derives;
mod docs;
mod externs;
mod lifetime_syntax;
mod lifetimes;
mod mangle;
mod naming;
mod non_local;
mod privacy;
mod publicity;
mod runtime_symbols;
mod statics;
mod track_caller;
mod traits;
mod variants;
mod visibility;
mod walk;

use std::cell::OnceCell;

use blaze_items::ItemId;
use hir_def::{AdtId, FunctionId, lang_item::lang_items, unstable_features::UnstableFeatures};
use intern::Symbol;
use syntax::{
    SyntaxKind, TextRange, TextSize,
    view::{self as ast, AstNode, HasName},
};

use self::walk::Item;
use super::effective::Effective;
use crate::{
    cx::{Cx, def_range_view, item_range_view, signature_range_view},
    lints::registry::LintSet,
    lower::Found,
};

/// The lints each pass gives, all of them lints: a pass none of whose lints may fire in the
/// crate ([`Cx::any_may_fire`], rustc's `lints_that_dont_need_to_run`) is not run.
mod gives {
    use crate::lints::registry::{LintId, LintSet};

    pub(super) const LIFETIMES: LintSet =
        LintSet::of(&[LintId::named("single_use_lifetimes"), LintId::named("unused_lifetimes")]);
    pub(super) const STATICS: LintSet =
        LintSet::of(&[LintId::named("invalid_type_param_default"), LintId::named("uninhabited_static")]);
    pub(super) const CTYPES: LintSet = LintSet::of(&[
        LintId::named("improper_ctypes"),
        LintId::named("improper_ctypes_definitions"),
        LintId::named("uses_power_alignment"),
    ]);
    pub(super) const VARIANT_SIZES: LintSet = LintSet::of(&[LintId::named("variant_size_differences")]);
    pub(super) const NAMING: LintSet =
        LintSet::of(&[LintId::named("non_snake_case"), LintId::named("non_upper_case_globals")]);
    pub(super) const COPY: LintSet = LintSet::of(&[LintId::named("missing_copy_implementations")]);
    pub(super) const TYPE_ALIAS_BOUNDS: LintSet = LintSet::of(&[LintId::named("type_alias_bounds")]);
    pub(super) const MANGLE: LintSet =
        LintSet::of(&[LintId::named("no_mangle_const_items"), LintId::named("no_mangle_generic_items")]);
    pub(super) const PUBLICITY: LintSet = LintSet::of(&[LintId::named("unreachable_pub")]);
    pub(super) const TRACK_CALLER: LintSet = LintSet::of(&[LintId::named("ungated_async_fn_track_caller")]);
    pub(super) const DROP: LintSet = LintSet::of(&[LintId::named("drop_bounds"), LintId::named("dyn_drop")]);
    pub(super) const DEBUG: LintSet = LintSet::of(&[LintId::named("missing_debug_implementations")]);
    pub(super) const DOCS: LintSet = LintSet::of(&[LintId::named("missing_docs")]);
    pub(super) const ASYNC_FN: LintSet = LintSet::of(&[LintId::named("async_fn_in_trait")]);
    pub(super) const NON_LOCAL: LintSet = LintSet::of(&[LintId::named("non_local_definitions")]);
    pub(super) const RUNTIME_SYMBOLS: LintSet = LintSet::of(&[
        LintId::named("invalid_runtime_symbol_definitions"),
        LintId::named("suspicious_runtime_symbol_definitions"),
    ]);
    pub(super) const LIFETIME_SYNTAX: LintSet = LintSet::of(&[LintId::named("mismatched_lifetime_syntaxes")]);
    pub(super) const C_VOID: LintSet = LintSet::of(&[LintId::named("c_void_returns")]);
    pub(super) const CLASHING: LintSet = LintSet::of(&[LintId::named("clashing_extern_declarations")]);
}

/// What `rustc_hir_analysis` and resolve's late pass lint of items, given whatever else failed.
pub(super) fn before_abort(cx: &Cx<'_, '_>) -> Vec<Found> {
    let (lifetimes, statics) = (cx.any_may_fire(&gives::LIFETIMES), cx.any_may_fire(&gives::STATICS));
    if !lifetimes && !statics {
        return Vec::new(); // alloc: empty, allocates nothing
    }
    let items = &cx.items().items;
    let mut found = Vec::new(); // alloc: empty until a lint fires; holds only what fires
    if lifetimes {
        found = lifetimes::lints(cx, items);
    }
    if statics {
        for item in items {
            found.extend(statics::item(cx, item));
        }
    }
    found
}

/// The crate's items in the order rustc visits them, walked once per check ([`Cx::items`]) for
/// every pass before the abort and after it.
pub(crate) struct Walk<'db> {
    items: Vec<Item<'db>>,
}

impl<'db> Walk<'db> {
    pub(crate) fn of(cx: &Cx<'_, 'db>) -> Walk<'db> {
        Walk { items: walk::items(cx) }
    }
}

/// `check_private_in_public`: `private_interfaces` and `private_bounds`.
pub(super) fn private_in_public<'db>(cx: &Cx<'_, 'db>, walk: &Walk<'db>, effective: &Effective) -> Vec<Found> {
    privacy::lints(cx, &walk.items, effective)
}

/// rustc's late module pass over each item, in the order of `BuiltinCombinedLateLintModPass`.
pub(super) fn after_abort<'db>(cx: &Cx<'_, 'db>, walk: &Walk<'db>, effective: &Effective) -> Vec<Found> {
    let items = &walk.items;
    let may = |lints: &LintSet| cx.any_may_fire(lints);
    let (ctypes, naming, copy, type_alias, mangle, publicity) = (
        may(&gives::CTYPES),
        may(&gives::NAMING),
        may(&gives::COPY),
        may(&gives::TYPE_ALIAS_BOUNDS),
        may(&gives::MANGLE),
        may(&gives::PUBLICITY),
    );
    let (track_caller, drop, debug, docs, async_fn, non_local) = (
        may(&gives::TRACK_CALLER),
        may(&gives::DROP),
        may(&gives::DEBUG),
        may(&gives::DOCS),
        may(&gives::ASYNC_FN),
        may(&gives::NON_LOCAL),
    );
    let (lifetime_syntax, c_void, variant_sizes) =
        (may(&gives::LIFETIME_SYNTAX), may(&gives::C_VOID), may(&gives::VARIANT_SIZES));
    let canonical = may(&gives::RUNTIME_SYMBOLS).then(|| runtime_symbols::Canonical::of(cx, items));
    let krate = Krate::of(cx);
    let mut found = Vec::new(); // alloc: empty until a lint fires; holds only what fires
    if docs {
        found.extend(docs::krate(cx));
    }
    if naming {
        found.extend(naming::krate(cx));
    }
    for (index, item) in items.iter().enumerate() {
        if ctypes {
            found.extend(externs::ctypes_item(cx, item, &krate));
        }
        if variant_sizes {
            found.extend(variants::item(cx, item));
        }
        if naming {
            found.extend(naming::item(cx, item));
        }
        if copy {
            found.extend(derives::copy(cx, item, effective, &krate));
        }
        if type_alias {
            found.extend(bounds::type_alias(cx, item, &krate));
        }
        if mangle {
            found.extend(mangle::item(cx, item));
        }
        if publicity {
            found.extend(publicity::item(cx, item, effective));
        }
        if track_caller {
            found.extend(track_caller::item(cx, item, krate.async_fn_track_caller));
        }
        if drop {
            found.extend(bounds::drop(cx, item, &krate));
        }
        if debug {
            found.extend(derives::debug(cx, item, effective, &krate));
        }
        if docs {
            found.extend(docs::item(cx, item, effective));
        }
        if async_fn {
            found.extend(traits::async_fn(cx, item, effective, &krate));
        }
        if non_local {
            found.extend(non_local::item(cx, item));
        }
        if let Some(canonical) = &canonical {
            found.extend(runtime_symbols::item(cx, item, item.held(items, index), canonical));
        }
        if ctypes {
            found.extend(externs::ctypes_nested(cx, item));
        }
        if lifetime_syntax {
            found.extend(lifetime_syntax::item(cx, item, krate.anonymous_lifetime_in_impl_trait));
        }
        if c_void {
            found.extend(c_void::item(cx, item, &krate));
        }
    }
    found
}

/// What the late passes read of the crate itself, read once for all of its items.
pub(super) struct Krate {
    pub(super) anonymous_lifetime_in_impl_trait: bool,
    pub(super) async_fn_track_caller: bool,
    pub(super) checked_type_aliases: bool,
    pub(super) return_type_notation: bool,
    /// The target is AIX, the one `uses_power_alignment` applies to.
    pub(super) aix: bool,
    pub(super) c_void: Option<hir::Enum>,
    pub(super) drop: Option<hir_def::TraitId>,
    pub(super) copy: Option<hir_def::TraitId>,
    pub(super) debug: Option<hir_def::TraitId>,
    /// `core::mem::needs_drop` as rustc prints it, found when a `Drop` bound first needs it.
    needs_drop: OnceCell<Option<String>>,
}

impl Krate {
    fn of(cx: &Cx<'_, '_>) -> Krate {
        let features = UnstableFeatures::query(cx.db, cx.krate.base());
        let enabled = |name: &str| features.is_enabled(&Symbol::intern(name));
        let lang = lang_items(cx.db, cx.krate.into());
        Krate {
            anonymous_lifetime_in_impl_trait: enabled("anonymous_lifetime_in_impl_trait"),
            async_fn_track_caller: enabled("async_fn_track_caller"),
            checked_type_aliases: enabled("checked_type_aliases"),
            return_type_notation: enabled("return_type_notation"),
            aix: cx.krate.cfg(cx.db).get_cfg_values("target_os").any(|it| it.as_str() == "aix"),
            c_void: lang.CVoid.map(hir::Enum::from),
            drop: lang.Drop,
            copy: lang.Copy,
            debug: lang.Debug,
            needs_drop: OnceCell::new(),
        }
    }

    pub(super) fn needs_drop(&self, cx: &Cx<'_, '_>) -> Option<&str> {
        self.needs_drop.get_or_init(|| core_path(cx, "mem", "needs_drop")).as_deref()
    }
}

/// `clashing_extern_declarations`, its own query after the late lint passes.
pub(super) fn clashing_extern_declarations<'db>(cx: &Cx<'_, 'db>, walk: &Walk<'db>) -> Vec<Found> {
    if !cx.any_may_fire(&gives::CLASHING) {
        return Vec::new(); // alloc: empty, allocates nothing
    }
    externs::lints(cx, &walk.items)
}

/// rustc's `def_span` of an item, an associated or foreign item, a variant or a field: a
/// function's signature; an impl up to its `where` clause; a const or a static up to its type;
/// a trait or an associated type up to its last bound; a `use` its tree; a field all of it;
/// else up to its name and generics.
pub(crate) fn def_span<'t>(node: ast::Node<'t>) -> TextRange {
    fn child<'t, N: AstNode<'t>>(node: ast::Node<'t>) -> Option<N> {
        node.children().find_map(N::cast)
    }
    let range = item_range_view(node);
    let until = |end: Option<TextSize>| TextRange::new(range.start(), end.unwrap_or(range.end()).max(range.start()));
    let end_of = |it: Option<ast::Node<'t>>| it.map(|it| it.text_range().end());
    let named = |name: Option<ast::Name<'t>>, generics: Option<ast::GenericParamList<'t>>| {
        let name = end_of(name.map(|it| it.syntax()));
        until(end_of(generics.map(|it| it.syntax())).or(name))
    };
    match node.kind() {
        SyntaxKind::FN => ast::Fn::cast(node).map_or(range, signature_range_view),
        SyntaxKind::IMPL => def_range_view(node),
        SyntaxKind::CONST | SyntaxKind::STATIC => until(end_of(child::<ast::Type<'t>>(node).map(|it| it.syntax()))),
        SyntaxKind::RECORD_FIELD | SyntaxKind::TUPLE_FIELD | SyntaxKind::EXTERN_BLOCK => range,
        SyntaxKind::USE_TREE => node.text_range(),
        SyntaxKind::TRAIT | SyntaxKind::TYPE_ALIAS => match child::<ast::TypeBoundList<'t>>(node).and_then(|it| it.bounds().last()) {
            Some(bound) => until(end_of(Some(bound.syntax()))),
            None => named(child(node), child(node)),
        },
        SyntaxKind::EXTERN_CRATE => {
            let rename = child::<ast::Rename<'t>>(node).and_then(|it| it.name());
            let name_ref = child::<ast::NameRef<'t>>(node).map(|it| it.syntax().text_range().end());
            until(end_of(rename.map(|it| it.syntax())).or(name_ref))
        }
        _ => match child::<ast::Name<'t>>(node) {
            Some(name) => named(Some(name), child(node)),
            None => range,
        },
    }
}

/// Whether the crate enables the feature `name`: its `#![feature(..)]`s, `cfg_attr`s expanded,
/// as its def map collected them (one interned-symbol lookup).
pub(super) fn feature(cx: &Cx<'_, '_>, name: &str) -> bool {
    UnstableFeatures::query(cx.db, cx.krate.base()).is_enabled(&Symbol::intern(name))
}

/// The Rust path of `core`'s `name` in `module` as rustc prints it: through `std` when the
/// crate has it.
fn core_path(cx: &Cx<'_, '_>, module: &str, name: &str) -> Option<String> {
    let mut core = None;
    let mut std = false;
    for dep in cx.krate.dependencies_iter(cx.db) {
        match dep.name.symbol().as_str() {
            "core" => core = Some(dep.krate),
            "std" => std = true,
            _ => {}
        }
    }
    let core = core?;
    let root = core.root_module(cx.db);
    let module = root.children(cx.db).find(|it| it.name(cx.db).is_some_and(|it| it.as_str() == module))?;
    module.declarations_iter(cx.db).find(|it| it.name(cx.db).is_some_and(|it| it.as_str() == name))?;
    let krate = if std { "std" } else { "core" };
    // alloc: once per check, when the first `Drop` bound is found and its lint fires
    Some(format!("{krate}::{}::{name}", module.name(cx.db)?.as_str()))
}

/// The item `item` defines, and its name, for the facts of its lints.
fn item_id<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>) -> Option<(ItemId, String)> {
    let node = item.node;
    fn name<'t>(it: impl HasName<'t>) -> String {
        // alloc: the name in a firing lint's fact; every caller asks only once its lint fires
        it.name().map_or_else(|| "_".to_owned(), |it| it.text().to_owned())
    }
    let sema = &cx.vsema;
    let id = match node.value {
        ast::Item::Fn(it) => ItemId::Function(FunctionId::try_from(sema.to_def2(node.with_value(it))?).ok()?),
        ast::Item::Struct(it) => ItemId::Adt(AdtId::StructId(sema.to_def2(node.with_value(it))?.into())),
        ast::Item::Enum(it) => ItemId::Adt(AdtId::EnumId(sema.to_def2(node.with_value(it))?.into())),
        ast::Item::Union(it) => ItemId::Adt(AdtId::UnionId(sema.to_def2(node.with_value(it))?.into())),
        ast::Item::Trait(it) => ItemId::Trait(sema.to_def2(node.with_value(it))?.into()),
        ast::Item::Const(it) => ItemId::Const(sema.to_def2(node.with_value(it))?.into()),
        ast::Item::Static(it) => ItemId::Static(sema.to_def2(node.with_value(it))?.into()),
        ast::Item::TypeAlias(it) => ItemId::Alias(sema.to_def2(node.with_value(it))?.into()),
        ast::Item::Impl(it) => {
            let id = hir_def::ImplId::try_from(sema.to_def2(node.with_value(it))?).ok()?;
            return Some((ItemId::Impl(id), "impl".to_owned())); // alloc: a firing lint's fact, as above
        }
        _ => return None,
    };
    let name = match node.value {
        ast::Item::Fn(it) => name(it),
        ast::Item::Struct(it) => name(it),
        ast::Item::Enum(it) => name(it),
        ast::Item::Union(it) => name(it),
        ast::Item::Trait(it) => name(it),
        ast::Item::Const(it) => name(it),
        ast::Item::Static(it) => name(it),
        ast::Item::TypeAlias(it) => name(it),
        _ => return None,
    };
    Some((id, name))
}
