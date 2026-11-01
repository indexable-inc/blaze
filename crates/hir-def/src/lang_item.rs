//! Collects lang items: items marked with `#[lang = "..."]` attribute.
//!
//! This attribute to tell the compiler about semi built-in std library
//! features, such as Fn family of traits.
use base_db::Db;
use hir_expand::name::Name;
use intern::{Symbol, sym};
use stdx::impl_from;

use crate::{
    AdtId, AssocItemId, AttrDefId, ConstId, Crate, EnumId, EnumVariantId, FunctionId, ImplId,
    ItemContainerId, MacroId, ModuleDefId, StaticId, StructId, TraitId, TypeAliasId, UnionId,
    attrs::{AttrFlags, CrateAttrFlags},
    nameres::{DefMap, assoc::TraitItems, crate_def_map, crate_local_def_map},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LangItemTarget {
    EnumId(EnumId),
    FunctionId(FunctionId),
    ImplId(ImplId),
    StaticId(StaticId),
    StructId(StructId),
    UnionId(UnionId),
    TypeAliasId(TypeAliasId),
    TraitId(TraitId),
    EnumVariantId(EnumVariantId),
    ConstId(ConstId),
    MacroId(MacroId),
}

impl_from!(
    EnumId, FunctionId, ImplId, StaticId, StructId, UnionId, TypeAliasId, TraitId, EnumVariantId, ConstId, MacroId for LangItemTarget
);

/// A query. This will look for lang items in a specific crate.
#[blaze_query::tracked(returns(as_deref), mem_tag = Nameres)]
pub fn crate_lang_items(db: &Db, krate: Crate) -> Option<Box<LangItems>> {
    let _p = tracing::info_span!("crate_lang_items_query").entered();
    if let Some(frozen) = crate::nameres::frozen::served_lang_items(db, krate) {
        return frozen;
    }

    let mut lang_items = LangItems::default();

    let crate_def_map = crate_def_map(db, krate);

    if !crate_def_map.features().lang_items {
        return None;
    }

    // Each item's flags off its file's column, not its own query: the pass is often the first
    // reader of a crate's flags, so the columns are built first, on every core.
    blaze_mem::frame::current(|frame| {
        let mut files = Vec::new_in(frame);
        files.extend(crate_def_map.modules(db).filter_map(|(_, module)| module.origin.file_id()));
        crate::attrs::build_columns(db, krate, &files);
    });
    let mut flags = CrateAttrFlags::new(db);
    for (_, module_data) in crate_def_map.modules(db) {
        for impl_def in module_data.scope(db).inherent_impls() {
            lang_items.collect_lang_item(&mut flags, impl_def);
            for &(_, assoc) in impl_def.impl_items(db).items.iter() {
                match assoc {
                    AssocItemId::FunctionId(f) => lang_items.collect_lang_item(&mut flags, f),
                    AssocItemId::TypeAliasId(t) => lang_items.collect_lang_item(&mut flags, t),
                    AssocItemId::ConstId(c) => lang_items.collect_lang_item(&mut flags, c),
                }
            }
        }

        for def in module_data.scope(db).declarations() {
            match def {
                ModuleDefId::TraitId(trait_) => {
                    lang_items.collect_lang_item(&mut flags, trait_);
                    TraitItems::query(db, trait_).items.iter().for_each(|&(_, assoc_id)| {
                        match assoc_id {
                            AssocItemId::FunctionId(f) => {
                                lang_items.collect_lang_item(&mut flags, f);
                            }
                            AssocItemId::TypeAliasId(alias) => {
                                lang_items.collect_lang_item(&mut flags, alias)
                            }
                            AssocItemId::ConstId(c) => lang_items.collect_lang_item(&mut flags, c),
                        }
                    });
                }
                ModuleDefId::AdtId(AdtId::EnumId(e)) => {
                    lang_items.collect_lang_item(&mut flags, e);
                    e.enum_variants(db).variants.values().for_each(|&(id, _)| {
                        lang_items.collect_lang_item(&mut flags, id);
                    });
                }
                ModuleDefId::AdtId(AdtId::StructId(s)) => {
                    lang_items.collect_lang_item(&mut flags, s);
                }
                ModuleDefId::AdtId(AdtId::UnionId(u)) => {
                    lang_items.collect_lang_item(&mut flags, u);
                }
                ModuleDefId::FunctionId(f) => {
                    lang_items.collect_lang_item(&mut flags, f);
                }
                ModuleDefId::StaticId(s) => {
                    lang_items.collect_lang_item(&mut flags, s);
                }
                ModuleDefId::TypeAliasId(t) => {
                    lang_items.collect_lang_item(&mut flags, t);
                }
                ModuleDefId::ConstId(c) => lang_items.collect_lang_item(&mut flags, c),
                _ => {}
            }
        }
    }

    if matches!(krate.data(db).origin, base_db::CrateOrigin::Lang(base_db::LangCrateOrigin::Core)) {
        lang_items.fill_non_lang_core_items(db, crate_def_map);
    }

    lang_items.resolve_manually(db);

    // alloc: the memo's value, once per crate
    if lang_items.is_empty() { None } else { Some(Box::new(lang_items)) }
}

/// A query. Look for a lang items, starting from the specified crate and recursively
/// traversing its dependencies.
#[blaze_query::tracked(returns(ref), mem_tag = Nameres)]
pub fn lang_items(db: &Db, start_crate: Crate) -> LangItems {
    let _p = tracing::info_span!("lang_items_query").entered();

    let mut result = crate_lang_items(db, start_crate).cloned().unwrap_or_default();

    // Our `CrateGraph` eagerly inserts sysroot dependencies like `core` or `std` into dependencies
    // even if the target crate has `#![no_std]`, `#![no_core]` or shadowed sysroot dependencies
    // like `dependencies.std.path = ".."`. So we use `extern_prelude()` instead of
    // `CrateData.dependencies` here, which has already come through such sysroot complexities
    // while nameres.
    //
    // See https://github.com/rust-lang/rust-analyzer/pull/20475 for details.
    for (_, (module, _)) in crate_local_def_map(db, start_crate).local(db).extern_prelude() {
        // Some crates declares themselves as extern crate like `extern crate self as core`.
        // Ignore these to prevent cycles.
        let krate = module.krate(db);
        if krate != start_crate {
            result.merge_prefer_self(lang_items(db, krate));
        }
    }

    result
}

impl LangItems {
    fn collect_lang_item<T>(&mut self, flags: &mut CrateAttrFlags<'_>, item: T)
    where
        T: Into<AttrDefId> + Into<LangItemTarget> + Copy,
    {
        if let Some(lang_item) = flags.lang_item(item.into()) {
            self.assign_lang_item(lang_item, item.into());
        }
    }
}

fn resolve_core_trait(
    db: &Db,
    core_def_map: &DefMap,
    modules: &[Symbol],
    name: Symbol,
) -> Option<TraitId> {
    let mut current = core_def_map.module(db, core_def_map.root);
    for module in modules {
        let Some((ModuleDefId::ModuleId(cur), _)) =
            current.scope(db).type_(&Name::new_symbol_root(*module))
        else {
            return None;
        };
        if cur.krate(db) != core_def_map.krate() || cur.block(db) != core_def_map.block_id() {
            return None;
        }
        current = core_def_map.module(db, cur);
    }
    let Some((ModuleDefId::TraitId(trait_), _)) =
        current.scope(db).type_(&Name::new_symbol_root(name))
    else {
        return None;
    };
    Some(trait_)
}

fn resolve_core_macro(
    db: &Db,
    core_def_map: &DefMap,
    modules: &[Symbol],
    name: Symbol,
) -> Option<MacroId> {
    let mut current = core_def_map.module(db, core_def_map.root);
    for module in modules {
        let Some((ModuleDefId::ModuleId(cur), _)) =
            current.scope(db).type_(&Name::new_symbol_root(*module))
        else {
            return None;
        };
        if cur.krate(db) != core_def_map.krate() || cur.block(db) != core_def_map.block_id() {
            return None;
        }
        current = core_def_map.module(db, cur);
    }
    current.scope(db).makro(&Name::new_symbol_root(name))
}

impl LangItems {
    fn resolve_manually(&mut self, db: &Db) {
        let parent_trait =
            |lang_item: &mut Option<TraitId>, def: Option<FunctionId>| match def?.loc(db).container
            {
                ItemContainerId::TraitId(trait_) => {
                    *lang_item = Some(trait_);
                    Some(trait_)
                }
                _ => None,
            };
        let assoc_types =
            |trait_: TraitId, assoc_types: &mut [(&mut Option<TypeAliasId>, Symbol)]| {
                let trait_items = trait_.trait_items(db);
                for (assoc_type, name) in assoc_types {
                    **assoc_type =
                        trait_items.associated_type_by_name(&Name::new_symbol_root(*name));
                }
            };
        let methods = |trait_: TraitId, assoc_types: &mut [(&mut Option<FunctionId>, Symbol)]| {
            let trait_items = trait_.trait_items(db);
            for (assoc_type, name) in assoc_types {
                **assoc_type = trait_items.method_by_name(&Name::new_symbol_root(*name));
            }
        };
        (|| {
            let into_future = parent_trait(&mut self.IntoFuture, self.IntoFutureIntoFuture)?;
            assoc_types(into_future, &mut [(&mut self.IntoFutureOutput, sym::Output)]);
            Some(())
        })();

        (|| {
            let into_iterator = parent_trait(&mut self.IntoIterator, self.IntoIterIntoIter)?;
            assoc_types(
                into_iterator,
                &mut [
                    (&mut self.IntoIteratorItem, sym::Item),
                    (&mut self.IntoIterIntoIterType, sym::IntoIter),
                ],
            );
            Some(())
        })();

        (|| {
            assoc_types(self.Iterator?, &mut [(&mut self.IteratorItem, sym::Item)]);
            Some(())
        })();

        (|| {
            assoc_types(self.AsyncIterator?, &mut [(&mut self.AsyncIteratorItem, sym::Item)]);
            Some(())
        })();

        for (op_trait, op_method, op_method_name) in [
            (self.Fn, &mut self.Fn_call, sym::call),
            (self.FnMut, &mut self.FnMut_call_mut, sym::call_mut),
            (self.FnOnce, &mut self.FnOnce_call_once, sym::call_once),
            (self.AsyncFn, &mut self.AsyncFn_async_call, sym::async_call),
            (self.AsyncFnMut, &mut self.AsyncFnMut_async_call_mut, sym::async_call_mut),
            (self.AsyncFnOnce, &mut self.AsyncFnOnce_async_call_once, sym::async_call_once),
            (self.Not, &mut self.Not_not, sym::not),
            (self.Neg, &mut self.Neg_neg, sym::neg),
            (self.Add, &mut self.Add_add, sym::add),
            (self.Mul, &mut self.Mul_mul, sym::mul),
            (self.Sub, &mut self.Sub_sub, sym::sub),
            (self.Div, &mut self.Div_div, sym::div),
            (self.Rem, &mut self.Rem_rem, sym::rem),
            (self.Shl, &mut self.Shl_shl, sym::shl),
            (self.Shr, &mut self.Shr_shr, sym::shr),
            (self.BitXor, &mut self.BitXor_bitxor, sym::bitxor),
            (self.BitOr, &mut self.BitOr_bitor, sym::bitor),
            (self.BitAnd, &mut self.BitAnd_bitand, sym::bitand),
            (self.AddAssign, &mut self.AddAssign_add_assign, sym::add_assign),
            (self.MulAssign, &mut self.MulAssign_mul_assign, sym::mul_assign),
            (self.SubAssign, &mut self.SubAssign_sub_assign, sym::sub_assign),
            (self.DivAssign, &mut self.DivAssign_div_assign, sym::div_assign),
            (self.RemAssign, &mut self.RemAssign_rem_assign, sym::rem_assign),
            (self.ShlAssign, &mut self.ShlAssign_shl_assign, sym::shl_assign),
            (self.ShrAssign, &mut self.ShrAssign_shr_assign, sym::shr_assign),
            (self.BitXorAssign, &mut self.BitXorAssign_bitxor_assign, sym::bitxor_assign),
            (self.BitOrAssign, &mut self.BitOrAssign_bitor_assign, sym::bitor_assign),
            (self.BitAndAssign, &mut self.BitAndAssign_bitand_assign, sym::bitand_assign),
            (self.Debug, &mut self.Debug_fmt, sym::fmt),
            (self.Deref, &mut self.Deref_deref, sym::deref),
            (self.DerefMut, &mut self.DerefMut_deref_mut, sym::deref_mut),
            (self.Index, &mut self.Index_index, sym::index),
            (self.IndexMut, &mut self.IndexMut_index_mut, sym::index_mut),
        ] {
            (|| {
                methods(op_trait?, &mut [(op_method, op_method_name)]);
                Some(())
            })();
        }
        (|| {
            methods(
                self.PartialEq?,
                &mut [(&mut self.PartialEq_eq, sym::eq), (&mut self.PartialEq_ne, sym::ne)],
            );
            Some(())
        })();
        (|| {
            methods(
                self.PartialOrd?,
                &mut [
                    (&mut self.PartialOrd_le, sym::le),
                    (&mut self.PartialOrd_lt, sym::lt),
                    (&mut self.PartialOrd_ge, sym::ge),
                    (&mut self.PartialOrd_gt, sym::gt),
                ],
            );
            Some(())
        })();
        (|| {
            methods(
                self.Drop?,
                &mut [(&mut self.Drop_drop, sym::drop), (&mut self.Drop_pin_drop, sym::pin_drop)],
            );
            Some(())
        })();
    }
}

#[blaze_query::tracked(returns(as_deref), mem_tag = Nameres)]
pub fn crate_notable_traits(db: &Db, krate: Crate) -> Option<Box<[TraitId]>> {
    blaze_mem::frame::current(|frame| {
        let mut traits = Vec::new_in(frame);
        let crate_def_map = crate_def_map(db, krate);
        for (_, module_data) in crate_def_map.modules(db) {
            for def in module_data.scope(db).declarations() {
                if let ModuleDefId::TraitId(trait_) = def
                    && AttrFlags::query(db, trait_.into())
                        .contains(AttrFlags::IS_DOC_NOTABLE_TRAIT)
                {
                    traits.push(trait_);
                }
            }
        }
        // alloc: the memo's traits, exact
        if traits.is_empty() { None } else { Some(Box::from(&traits[..])) }
    })
}

macro_rules! language_item_table {
    (
        $LangItems:ident =>
        $( $(#[$attr:meta])* $lang_item:ident, $module:ident :: $name:ident, $target:ident; )*

        @non_lang_core_traits:

        $( core::$($non_lang_trait_module:ident)::*, $non_lang_trait:ident; )*

        @non_lang_core_macros:

        $( core::$($non_lang_macro_module:ident)::*, $non_lang_macro:ident, $non_lang_macro_field:ident; )*

        @resolve_manually:

        $( $resolve_manually:ident, $resolve_manually_type:ident; )*
    ) => {
        #[allow(non_snake_case)] // FIXME: Should we remove this?
        #[derive(Debug, Default, Clone, PartialEq, Eq, Hash)]
        pub struct $LangItems {
            $(
                $(#[$attr])*
                pub $lang_item: Option<$target>,
            )*
            $(
                pub $non_lang_trait: Option<TraitId>,
            )*
            $(
                pub $non_lang_macro_field: Option<MacroId>,
            )*
            $(
                pub $resolve_manually: Option<$resolve_manually_type>,
            )*
        }

        impl LangItems {
            fn is_empty(&self) -> bool {
                $( self.$lang_item.is_none() )&&*
            }

            /// Merges `self` with `other`, with preference to `self` items.
            fn merge_prefer_self(&mut self, other: &Self) {
                $( self.$lang_item = self.$lang_item.or(other.$lang_item); )*
                $( self.$non_lang_trait = self.$non_lang_trait.or(other.$non_lang_trait); )*
                $( self.$non_lang_macro_field = self.$non_lang_macro_field.or(other.$non_lang_macro_field); )*
                $( self.$resolve_manually = self.$resolve_manually.or(other.$resolve_manually); )*
            }

            /// Sets the lang item `name` names to `target`, unless `target` is not of its kind.
            fn assign_lang_item(&mut self, name: Symbol, target: LangItemTarget) {
                if let Some(which) = LangItemEnum::from_symbol(&name) {
                    self.set(which, target);
                }
            }

            /// Sets `which` to `target`, or is `false` and leaves it when `target` is not of
            /// its kind.
            pub fn set(&mut self, which: LangItemEnum, target: LangItemTarget) -> bool {
                match (which, target) {
                    $( (LangItemEnum::$lang_item, LangItemTarget::$target(it)) => self.$lang_item = Some(it), )*
                    $( (LangItemEnum::$non_lang_trait, LangItemTarget::TraitId(it)) => self.$non_lang_trait = Some(it), )*
                    $( (LangItemEnum::$non_lang_macro_field, LangItemTarget::MacroId(it)) => self.$non_lang_macro_field = Some(it), )*
                    $( (LangItemEnum::$resolve_manually, LangItemTarget::$resolve_manually_type(it)) => self.$resolve_manually = Some(it), )*
                    _ => return false,
                }
                true
            }

            fn fill_non_lang_core_items(&mut self, db: &Db, core_def_map: &DefMap) {
                $( self.$non_lang_trait = resolve_core_trait(db, core_def_map, &[ $(sym::$non_lang_trait_module),* ], sym::$non_lang_trait); )*
                $( self.$non_lang_macro_field = resolve_core_macro(db, core_def_map, &[ $(sym::$non_lang_macro_module),* ], sym::$non_lang_macro); )*
            }
        }

        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        #[allow(non_camel_case_types)]
        #[repr(u16)]
        pub enum LangItemEnum {
            $(
                $(#[$attr])*
                $lang_item,
            )*
            $( $non_lang_trait, )*
            $( $non_lang_macro_field, )*
            $( $resolve_manually, )*
        }

        impl LangItemEnum {
            /// Every lang item, in the order of the fields of `LangItems`.
            pub const ALL: &[LangItemEnum] = &[
                $( LangItemEnum::$lang_item, )*
                $( LangItemEnum::$non_lang_trait, )*
                $( LangItemEnum::$non_lang_macro_field, )*
                $( LangItemEnum::$resolve_manually, )*
            ];

            #[inline]
            pub fn from_lang_items(self, lang_items: &LangItems) -> Option<LangItemTarget> {
                match self {
                    $( LangItemEnum::$lang_item => lang_items.$lang_item.map(Into::into), )*
                    $( LangItemEnum::$non_lang_trait => lang_items.$non_lang_trait.map(Into::into), )*
                    $( LangItemEnum::$non_lang_macro_field => lang_items.$non_lang_macro_field.map(Into::into), )*
                    $( LangItemEnum::$resolve_manually => lang_items.$resolve_manually.map(Into::into), )*
                }
            }

            /// The lang item `#[lang = "symbol"]` declares: the symbol's declared index and one
            /// load, no hash. A symbol named twice keeps its first lang item.
            #[inline]
            pub fn from_symbol(symbol: &Symbol) -> Option<Self> {
                // Member `i` is the `i`th `#[lang]` item, which leads `ALL` and whose
                // discriminant is `i`.
                static BY_SYMBOL: intern::KnownNames =
                    intern::KnownNames::new(&[ $( $module::Declared::$name, )* ]);
                BY_SYMBOL.get(symbol).map(|at| Self::ALL[usize::from(at)])
            }

            /// Each lang item with the symbol of its `#[lang = ".."]`, in declaration order.
            #[cfg(test)]
            const SYMBOLS: &[(Symbol, LangItemEnum)] = &[ $( ($module::$name, Self::$lang_item), )* ];

            /// [`Self::from_symbol`] as a chain of compares, the reference its table is checked
            /// against.
            #[cfg(test)]
            fn from_symbol_by_compare(symbol: &Symbol) -> Option<Self> {
                match symbol {
                    $( _ if *symbol == $module::$name => Some(Self::$lang_item), )*
                    _ => None,
                }
            }
        }
    }
}

language_item_table! { LangItems =>
//  Variant name,            Name,                     Target;
    Sized,                   sym::sized,               TraitId;
    MetaSized,               sym::meta_sized,          TraitId;
    PointeeSized,            sym::pointee_sized,       TraitId;
    Unsize,                  sym::unsize,              TraitId;
    /// Trait injected by `#[derive(PartialEq)]`, (i.e. "Partial EQ").
    StructuralPeq,           sym::structural_peq,      TraitId;
    /// Trait injected by `#[derive(Eq)]`, (i.e. "Total EQ"; no, I will not apologize).
    StructuralTeq,           sym::structural_teq,      TraitId;
    Copy,                    sym::copy,                TraitId;
    UseCloned,               sym::use_cloned,          TraitId;
    Clone,                   sym::clone,               TraitId;
    TrivialClone,            sym::trivial_clone,       TraitId;
    TryAsDyn,                sym::try_as_dyn,          TraitId;
    Sync,                    sym::sync,                TraitId;
    DiscriminantKind,        sym::discriminant_kind,   TraitId;
    /// The associated item of the `DiscriminantKind` trait.
    Discriminant,            sym::discriminant_type,   TypeAliasId;

    PointeeTrait,            sym::pointee_trait,       TraitId;
    Metadata,                sym::metadata_type,       TypeAliasId;
    DynMetadata,             sym::dyn_metadata,        StructId;

    Freeze,                  sym::freeze,              TraitId;

    NonNull,                 sym::non_null,            StructId;

    FnPtrTrait,              sym::fn_ptr_trait,        TraitId;
    FnPtrAsPtr,              sym::fn_ptr_as_ptr,       FunctionId;
    FnPtrFromPtr,            sym::fn_ptr_from_ptr,     FunctionId;

    Drop,                    sym::drop,                TraitId;
    Destruct,                sym::destruct,            TraitId;
    BikeshedGuaranteedNoDrop,sym::bikeshed_guaranteed_no_drop, TraitId;

    CoerceUnsized,           sym::coerce_unsized,      TraitId;
    DispatchFromDyn,         sym::dispatch_from_dyn,   TraitId;
    Reborrow,                sym::reborrow,            TraitId;
    CoerceShared,            sym::coerce_shared,       TraitId;

    // language items relating to transmutability
    TransmuteOpts,           sym::transmute_opts,      StructId;
    TransmuteTrait,          sym::transmute_trait,     TraitId;

    Add,                     sym::add,                 TraitId;
    Sub,                     sym::sub,                 TraitId;
    Mul,                     sym::mul,                 TraitId;
    Div,                     sym::div,                 TraitId;
    Rem,                     sym::rem,                 TraitId;
    Neg,                     sym::neg,                 TraitId;
    Not,                     sym::not,                 TraitId;
    BitXor,                  sym::bitxor,              TraitId;
    BitAnd,                  sym::bitand,              TraitId;
    BitOr,                   sym::bitor,               TraitId;
    Shl,                     sym::shl,                 TraitId;
    Shr,                     sym::shr,                 TraitId;
    AddAssign,               sym::add_assign,          TraitId;
    SubAssign,               sym::sub_assign,          TraitId;
    MulAssign,               sym::mul_assign,          TraitId;
    DivAssign,               sym::div_assign,          TraitId;
    RemAssign,               sym::rem_assign,          TraitId;
    BitXorAssign,            sym::bitxor_assign,       TraitId;
    BitAndAssign,            sym::bitand_assign,       TraitId;
    BitOrAssign,             sym::bitor_assign,        TraitId;
    ShlAssign,               sym::shl_assign,          TraitId;
    ShrAssign,               sym::shr_assign,          TraitId;
    Index,                   sym::index,               TraitId;
    IndexMut,                sym::index_mut,           TraitId;

    UnsafeCell,              sym::unsafe_cell,         StructId;
    UnsafePinned,            sym::unsafe_pinned,       StructId;
    VaList,                  sym::va_list,             StructId;
    F16B,                    sym::f16b,                StructId;
    PpcF128,                 sym::ppcf128,             StructId;

    Deref,                   sym::deref,               TraitId;
    DerefMut,                sym::deref_mut,           TraitId;
    DerefPure,               sym::deref_pure,          TraitId;
    DerefTarget,             sym::deref_target,        TypeAliasId;
    Receiver,                sym::receiver,            TraitId;
    ReceiverTarget,          sym::receiver_target,     TypeAliasId;

    Fn,                      sym::fn_,                 TraitId;
    FnMut,                   sym::fn_mut,              TraitId;
    FnOnce,                  sym::fn_once,             TraitId;
    AsyncFn,                 sym::async_fn,            TraitId;
    AsyncFnMut,              sym::async_fn_mut,        TraitId;
    AsyncFnOnce,             sym::async_fn_once,       TraitId;
    AsyncFnKindHelper,       sym::async_fn_kind_helper,TraitId;
    AsyncFnKindUpvars,       sym::async_fn_kind_upvars,TypeAliasId;

    CallRefFuture,           sym::call_ref_future,     TypeAliasId;
    CallOnceFuture,          sym::call_once_future,    TypeAliasId;
    AsyncFnOnceOutput,       sym::async_fn_once_output, TypeAliasId;

    FnOnceOutput,            sym::fn_once_output,      TypeAliasId;

    Future,                  sym::future_trait,        TraitId;
    AsyncIterator,           sym::async_iterator,      TraitId;
    CoroutineState,          sym::coroutine_state,     EnumId;
    Coroutine,               sym::coroutine,           TraitId;
    CoroutineReturn,         sym::coroutine_return,    TypeAliasId;
    CoroutineYield,          sym::coroutine_yield,     TypeAliasId;
    Unpin,                   sym::unpin,               TraitId;
    UnsafeUnpin,             sym::unsafe_unpin,        TraitId;
    GlobalAlloc,             sym::global_alloc_ty,     StructId;
    MaybeDangling,           sym::maybe_dangling,      StructId;
    Pin,                     sym::pin,                 StructId;

    PartialEq,               sym::eq,                  TraitId;
    PartialOrd,              sym::partial_ord,         TraitId;
    CVoid,                   sym::c_void,              EnumId;

    // A number of panic-related lang items. The `panic` item corresponds to divide-by-zero and
    // various panic cases with `match`. The `panic_bounds_check` item is for indexing arrays.
    //
    // The `begin_unwind` lang item has a predefined symbol name and is sort of a "weak lang item"
    // in the sense that a crate is not required to have it defined to use it, but a final product
    // is required to define it somewhere. Additionally, there are restrictions on crates that use
    // a weak lang item, but do not have it defined.
    Panic,                   sym::panic,               FunctionId;
    PanicNounwind,           sym::panic_nounwind,      FunctionId;
    PanicFmt,                sym::panic_fmt,           FunctionId;
    PanicDisplay,            sym::panic_display,       FunctionId;
    ConstPanicFmt,           sym::const_panic_fmt,     FunctionId;
    PanicBoundsCheck,        sym::panic_bounds_check,  FunctionId;
    PanicMisalignedPointerDereference, sym::panic_misaligned_pointer_dereference, FunctionId;
    PanicConstAddOverflow, sym::panic_const_add_overflow, FunctionId;
    PanicConstSubOverflow, sym::panic_const_sub_overflow, FunctionId;
    PanicConstMulOverflow, sym::panic_const_mul_overflow, FunctionId;
    PanicConstDivOverflow, sym::panic_const_div_overflow, FunctionId;
    PanicConstRemOverflow, sym::panic_const_rem_overflow, FunctionId;
    PanicConstNegOverflow, sym::panic_const_neg_overflow, FunctionId;
    PanicConstShrOverflow, sym::panic_const_shr_overflow, FunctionId;
    PanicConstShlOverflow, sym::panic_const_shl_overflow, FunctionId;
    PanicConstDivByZero, sym::panic_const_div_by_zero, FunctionId;
    PanicConstRemByZero, sym::panic_const_rem_by_zero, FunctionId;
    PanicConstCoroutineResumed, sym::panic_const_coroutine_resumed, FunctionId;
    PanicConstAsyncFnResumed, sym::panic_const_async_fn_resumed, FunctionId;
    PanicConstAsyncGenFnResumed, sym::panic_const_async_gen_fn_resumed, FunctionId;
    PanicConstGenFnNone, sym::panic_const_gen_fn_none, FunctionId;
    PanicConstCoroutineResumedPanic, sym::panic_const_coroutine_resumed_panic, FunctionId;
    PanicConstAsyncFnResumedPanic, sym::panic_const_async_fn_resumed_panic, FunctionId;
    PanicConstAsyncGenFnResumedPanic, sym::panic_const_async_gen_fn_resumed_panic, FunctionId;
    PanicConstGenFnNonePanic, sym::panic_const_gen_fn_none_panic, FunctionId;
    PanicConstCoroutineResumedDrop, sym::panic_const_coroutine_resumed_drop, FunctionId;
    PanicConstAsyncFnResumedDrop, sym::panic_const_async_fn_resumed_drop, FunctionId;
    PanicConstAsyncGenFnResumedDrop, sym::panic_const_async_gen_fn_resumed_drop, FunctionId;
    PanicConstGenFnNoneDrop, sym::panic_const_gen_fn_none_drop, FunctionId;
    PanicInfo,               sym::panic_info,          StructId;
    PanicLocation,           sym::panic_location,      StructId;
    PanicImpl,               sym::panic_impl,          FunctionId;
    PanicCannotUnwind,       sym::panic_cannot_unwind, FunctionId;
    PanicNullPointerDereference, sym::panic_null_pointer_dereference, FunctionId;
    PanicNullReferenceConstructed, sym::panic_null_reference_constructed, FunctionId;
    PanicInvalidEnumConstruction, sym::panic_invalid_enum_construction, FunctionId;
    /// libstd panic entry point. Necessary for const eval to be able to catch it
    BeginPanic,              sym::begin_panic,         FunctionId;

    // Lang items needed for `format_args!()`.
    FormatAlignment,         sym::format_alignment,    EnumId;
    FormatArgument,          sym::format_argument,     StructId;
    FormatArguments,         sym::format_arguments,    StructId;
    FormatCount,             sym::format_count,        EnumId;
    FormatPlaceholder,       sym::format_placeholder,  StructId;
    FormatUnsafeArg,         sym::format_unsafe_arg,   StructId;

    ExchangeMalloc,          sym::exchange_malloc,     FunctionId;
    BoxFree,                 sym::box_free,            FunctionId;
    DropInPlace,             sym::drop_in_place,       FunctionId;
    DropGlue,                sym::drop_glue,           FunctionId;
    AllocLayout,             sym::alloc_layout,        StructId;

    Start,                   sym::start,               FunctionId;

    EhPersonality,           sym::eh_personality,      FunctionId;
    EhCatchTypeinfo,         sym::eh_catch_typeinfo,   StaticId;

    OwnedBox,                sym::owned_box,           StructId;

    PhantomData,             sym::phantom_data,        StructId;

    ManuallyDrop,            sym::manually_drop,       StructId;

    MaybeUninit,             sym::maybe_uninit,        UnionId;

    /// Align offset for stride != 1; must not panic.
    AlignOffset,             sym::align_offset,        FunctionId;

    Termination,             sym::termination,         TraitId;

    Try,                     sym::Try,                 TraitId;

    Tuple,                   sym::tuple_trait,         TraitId;

    SliceLen,                sym::slice_len_fn,        FunctionId;

    // Language items from AST lowering
    TryTraitFromResidual,    sym::from_residual,       FunctionId;
    TryTraitFromOutput,      sym::from_output,         FunctionId;
    TryTraitBranch,          sym::branch,              FunctionId;
    TryTraitFromYeet,        sym::from_yeet,           FunctionId;
    ResidualIntoTryType,     sym::into_try_type,       FunctionId;

    PointerLike,             sym::pointer_like,        TraitId;

    ConstParamTy,            sym::const_param_ty,      TraitId;

    Poll,                    sym::Poll,                EnumId;
    PollReady,               sym::Ready,               EnumVariantId;
    PollPending,             sym::Pending,             EnumVariantId;

    // FIXME(swatinem): the following lang items are used for async lowering and
    // should become obsolete eventually.
    ResumeTy,                sym::ResumeTy,            StructId;
    GetContext,              sym::get_context,         FunctionId;

    Context,                 sym::Context,             StructId;
    FuturePoll,              sym::poll,                FunctionId;
    FutureOutput,            sym::future_output,       TypeAliasId;

    TypeId,                  sym::type_id,             StructId;
    /// `core::mem::type_info::Type`, what reflection's `type_id_type_of` makes.
    TypeInfo,                sym::type_info,           StructId;
    /// `core::mem::type_info::Generic`, an element of what `type_id_generics` makes.
    TypeInfoGeneric,         sym::type_info_generic,   EnumId;
    /// `core::mem::type_info::FnPtr`, what `type_id_function_ptr` makes.
    TypeInfoFnPtr,           sym::FnPtr,               StructId;

    Option,                  sym::Option,              EnumId;
    OptionSome,              sym::Some,                EnumVariantId;
    OptionNone,              sym::None,                EnumVariantId;

    ResultOk,                sym::Ok,                  EnumVariantId;
    ResultErr,               sym::Err,                 EnumVariantId;

    ControlFlowContinue,     sym::Continue,            EnumVariantId;
    ControlFlowBreak,        sym::Break,               EnumVariantId;

    IntoFutureIntoFuture,    sym::into_future,         FunctionId;
    IntoIterIntoIter,        sym::into_iter,           FunctionId;
    IteratorNext,            sym::next,                FunctionId;
    Iterator,                sym::iterator,            TraitId;
    FusedIterator,           sym::fused_iterator,      TraitId;

    PinNewUnchecked,         sym::new_unchecked,       FunctionId;

    RangeFrom,               sym::RangeFrom,           StructId;
    RangeFull,               sym::RangeFull,           StructId;
    RangeInclusiveStruct,    sym::RangeInclusive,      StructId;
    RangeInclusiveNew,       sym::range_inclusive_new, FunctionId;
    Range,                   sym::Range,               StructId;
    RangeToInclusive,        sym::RangeToInclusive,    StructId;
    RangeTo,                 sym::RangeTo,             StructId;

    RangeFromCopy,           sym::RangeFromCopy,           StructId;
    RangeInclusiveCopy,      sym::RangeInclusiveCopy,      StructId;
    RangeCopy,               sym::RangeCopy,               StructId;
    RangeToInclusiveCopy,    sym::RangeToInclusiveCopy,    StructId;

    String,                  sym::String,              StructId;
    CStr,                    sym::CStr,                StructId;
    Ordering,                sym::Ordering,            EnumId;

    Field,                   sym::field,               TraitId;
    FieldBase,               sym::field_base,          TypeAliasId;
    FieldType,               sym::field_type,          TypeAliasId;

    RangeMin,                sym::RangeMin,            ConstId;
    /// `SizedTypeProperties::SIZE` and `::ALIGN`.
    SizeOf,                  sym::mem_size_const,      ConstId;
    AlignOf,                 sym::mem_align_const,     ConstId;
    RangeMax,                sym::RangeMax,            ConstId;
    RangeSub,                sym::RangeSub,            FunctionId;

    @non_lang_core_traits:
    core::default, Default;
    core::fmt, Debug;
    core::hash, Hash;
    core::cmp, Ord;
    core::cmp, Eq;

    @non_lang_core_macros:
    core::default, Default, DefaultDerive;
    core::fmt, Debug, DebugDerive;
    core::hash, Hash, HashDerive;
    core::cmp, PartialOrd, PartialOrdDerive;
    core::cmp, Ord, OrdDerive;
    core::cmp, PartialEq, PartialEqDerive;
    core::cmp, Eq, EqDerive;
    core::marker, CoercePointee, CoercePointeeDerive;
    core::marker, CoerceShared, CoerceSharedDerive;
    core::marker, Copy, CopyDerive;
    core::marker, Reborrow, ReborrowDerive;
    core::clone, Clone, CloneDerive;

    @resolve_manually:

    IntoFuture,                    TraitId;
    IntoFutureOutput,              TypeAliasId;
    IntoIterator,                  TraitId;
    IntoIteratorItem,              TypeAliasId;
    IntoIterIntoIterType,          TypeAliasId;
    IteratorItem,                  TypeAliasId;
    AsyncIteratorItem,             TypeAliasId;

    Fn_call,                       FunctionId;
    FnMut_call_mut,                FunctionId;
    FnOnce_call_once,              FunctionId;
    AsyncFn_async_call,            FunctionId;
    AsyncFnMut_async_call_mut,     FunctionId;
    AsyncFnOnce_async_call_once,   FunctionId;
    Not_not,                       FunctionId;
    Neg_neg,                       FunctionId;
    Add_add,                       FunctionId;
    Mul_mul,                       FunctionId;
    Sub_sub,                       FunctionId;
    Div_div,                       FunctionId;
    Rem_rem,                       FunctionId;
    Shl_shl,                       FunctionId;
    Shr_shr,                       FunctionId;
    BitXor_bitxor,                 FunctionId;
    BitOr_bitor,                   FunctionId;
    BitAnd_bitand,                 FunctionId;
    AddAssign_add_assign,          FunctionId;
    MulAssign_mul_assign,          FunctionId;
    SubAssign_sub_assign,          FunctionId;
    DivAssign_div_assign,          FunctionId;
    RemAssign_rem_assign,          FunctionId;
    ShlAssign_shl_assign,          FunctionId;
    ShrAssign_shr_assign,          FunctionId;
    BitXorAssign_bitxor_assign,    FunctionId;
    BitOrAssign_bitor_assign,      FunctionId;
    BitAndAssign_bitand_assign,    FunctionId;
    PartialEq_eq,                  FunctionId;
    PartialEq_ne,                  FunctionId;
    PartialOrd_le,                 FunctionId;
    PartialOrd_lt,                 FunctionId;
    PartialOrd_ge,                 FunctionId;
    PartialOrd_gt,                 FunctionId;
    Drop_drop,                     FunctionId;
    Drop_pin_drop,                     FunctionId;
    Debug_fmt,                     FunctionId;
    Deref_deref,                   FunctionId;
    DerefMut_deref_mut,            FunctionId;
    Index_index,                   FunctionId;
    IndexMut_index_mut,            FunctionId;
}

#[cfg(test)]
mod tests {
    use intern::{Symbol, sym};
    use blaze_query::db::FromId;
    use hir_expand::fixture;

    use super::{LangItemEnum, LangItemTarget, LangItems, crate_lang_items};
    use crate::{
        AttrDefId, ConstId, FunctionId, StructId, TraitId,
        attrs::AttrFlags,
        signatures::{ConstSignature, FunctionSignature, StructSignature, TraitSignature},
        test_db::TestDBExt,
    };

    #[test]
    fn symbol_table_matches_compares() {
        let mut symbols: Vec<Symbol> = LangItemEnum::SYMBOLS.iter().map(|&(it, _)| it).collect();
        // The same texts interned at run time, and near misses of each.
        let texts: Vec<String> = symbols.iter().map(|it| it.as_str().to_owned()).collect();
        for text in &texts {
            symbols.push(Symbol::intern(text));
            symbols.push(Symbol::intern(&format!("{text}_")));
            symbols.push(Symbol::intern(&text.to_uppercase()));
            symbols.push(Symbol::intern(&text[..text.len() - 1]));
        }
        symbols.extend(
            ["sized", "copy", "drop", "Default", "Debug", "", "not a lang item"].map(Symbol::intern),
        );
        symbols.extend([sym::Default, sym::Output, sym::Item, sym::call, sym::fmt, sym::self_]);
        symbols.extend([sym::Integer::get(0), sym::Integer::get(300)]);
        for symbol in &symbols {
            assert_eq!(
                LangItemEnum::from_symbol(symbol),
                LangItemEnum::from_symbol_by_compare(symbol),
                "{symbol:?}"
            );
        }
        assert_eq!(LangItemEnum::from_symbol(&sym::sized), Some(LangItemEnum::Sized));
        assert_eq!(LangItemEnum::from_symbol(&Symbol::intern("sized")), Some(LangItemEnum::Sized));
        assert_eq!(LangItemEnum::from_symbol(&sym::fn_), Some(LangItemEnum::Fn));
        assert_eq!(LangItemEnum::from_symbol(&Symbol::intern("not a lang item")), None);
        // Items that are no `#[lang]` are never named by one.
        assert_eq!(LangItemEnum::from_symbol(&sym::Default), None);
    }

    #[test]
    fn discriminant_is_index_in_all() {
        for (i, &which) in LangItemEnum::ALL.iter().enumerate() {
            assert_eq!(which as u16 as usize, i, "{which:?}");
        }
        for (i, &(_, which)) in LangItemEnum::SYMBOLS.iter().enumerate() {
            assert_eq!(LangItemEnum::ALL[i], which);
        }
    }

    #[test]
    fn assign_keeps_to_the_kind() {
        let trait_ = TraitId::from_id(blaze_query::db::Id::from_u32(5));
        let struct_ = StructId::from_id(blaze_query::db::Id::from_u32(6));
        let mut items = LangItems::default();
        items.assign_lang_item(sym::sized, struct_.into());
        assert_eq!(items, LangItems::default());
        items.assign_lang_item(sym::sized, trait_.into());
        items.assign_lang_item(Symbol::intern("not a lang item"), trait_.into());
        items.assign_lang_item(sym::phantom_data, struct_.into());
        let mut expected = LangItems::default();
        expected.Sized = Some(trait_);
        expected.PhantomData = Some(struct_);
        assert_eq!(items, expected);
    }

    /// The lang items of rustc's UB checks, `SizedTypeProperties`' consts and the UB checker's
    /// three types, as the pinned nightly's `library/` declares them.
    #[test]
    fn check_and_layout_lang_items_are_found() {
        let db = fixture::db(
            r#"
#![feature(lang_items)]
#[lang = "panic_null_reference_constructed"]
fn null_reference() -> ! { loop {} }
#[lang = "panic_invalid_enum_construction"]
fn invalid_enum(source: u128) -> ! { loop {} }
pub trait SizedTypeProperties: Sized {
    #[lang = "mem_size_const"]
    const SIZE: usize = 0;
    #[lang = "mem_align_const"]
    const ALIGN: usize = 1;
}
#[lang = "unsafe_unpin"]
pub unsafe auto trait UnsafeUnpin {}
#[lang = "global_alloc_ty"]
pub struct Global;
#[lang = "maybe_dangling"]
pub struct MaybeDangling<P: ?Sized>(P);
"#,
        )
        .into_db();
        let items = crate_lang_items(&db, db.fetch_test_crate()).unwrap();
        let function =
            |id: Option<FunctionId>| FunctionSignature::of(&db, id.unwrap()).name.clone();
        let constant = |id: Option<ConstId>| ConstSignature::of(&db, id.unwrap()).name.clone();
        let structure = |id: Option<StructId>| StructSignature::of(&db, id.unwrap()).name.clone();
        let names = [
            function(items.PanicNullReferenceConstructed),
            function(items.PanicInvalidEnumConstruction),
            constant(items.SizeOf).unwrap(),
            constant(items.AlignOf).unwrap(),
            TraitSignature::of(&db, items.UnsafeUnpin.unwrap()).name.clone(),
            structure(items.GlobalAlloc),
            structure(items.MaybeDangling),
        ];
        let names: Vec<&str> = names.iter().map(|it| it.as_str()).collect();
        assert_eq!(
            names,
            [
                "null_reference",
                "invalid_enum",
                "SIZE",
                "ALIGN",
                "UnsafeUnpin",
                "Global",
                "MaybeDangling"
            ]
        );
    }

    /// The lang items read off the files' flag columns are found wherever an item is (a trait's
    /// type, an inherent impl's fn, a variant, an inline and a file module, a `cfg_attr`), and
    /// a macro's item, which no column holds, is read off its own query; each is what the
    /// item's own `AttrFlags::lang_item` says.
    #[test]
    fn lang_items_off_the_columns_are_each_items_own() {
        let db = fixture::db(
            r#"
//- /lib.rs crate:test
#![feature(lang_items)]
#[lang = "sized"]
pub trait Sized {}
#[lang = "owned_box"]
pub struct Box<T>(T);
pub enum Option<T> { None, #[lang = "Some"] Some(T) }
pub trait Deref { #[lang = "deref_target"] type Target; }
pub struct S;
impl S { #[lang = "begin_panic"] pub fn begin() {} }
pub mod inline { #[lang = "drop_in_place"] pub fn drop_in_place() {} }
mod file;
macro_rules! written { () => { #[lang = "panic"] pub fn panic() {} } }
written!();
#[cfg_attr(all(), lang = "String")]
pub struct String;
#[cfg_attr(any(), lang = "RangeFull")]
pub struct RangeFull;
//- /file.rs
#[lang = "panic_fmt"]
pub fn panic_fmt() {}
"#,
        )
        .into_db();
        let items = crate_lang_items(&db, db.fetch_test_crate()).unwrap();
        let own = |target: LangItemTarget, name: &str| {
            let owner = match target {
                LangItemTarget::TraitId(it) => AttrDefId::from(it),
                LangItemTarget::StructId(it) => it.into(),
                LangItemTarget::EnumVariantId(it) => it.into(),
                LangItemTarget::TypeAliasId(it) => it.into(),
                LangItemTarget::FunctionId(it) => it.into(),
                other => panic!("no {name} expected of {other:?}"),
            };
            assert_eq!(AttrFlags::lang_item(&db, owner), Some(Symbol::intern(name)), "{name}");
        };
        own(items.Sized.unwrap().into(), "sized");
        own(items.OwnedBox.unwrap().into(), "owned_box");
        own(items.OptionSome.unwrap().into(), "Some");
        own(items.DerefTarget.unwrap().into(), "deref_target");
        own(items.BeginPanic.unwrap().into(), "begin_panic");
        own(items.DropInPlace.unwrap().into(), "drop_in_place");
        own(items.PanicFmt.unwrap().into(), "panic_fmt");
        own(items.Panic.unwrap().into(), "panic");
        own(items.String.unwrap().into(), "String");
        assert_eq!(items.RangeFull, None);
    }

    /// A lang-item lookup is a memo read and a field load.
    #[test]
    fn lang_item_lookup_allocates_nothing() {
        let db = fixture::db(
            r#"
#![feature(lang_items)]
#[lang = "sized"]
pub trait Sized {}
#[lang = "global_alloc_ty"]
pub struct Global;
"#,
        )
        .into_db();
        let krate = db.fetch_test_crate();
        let _ = super::lang_items(&db, krate);
        let (sized, global) = blaze_alloc_count::assert_at_most(0, "a warm lang-item lookup", || {
            let items = super::lang_items(&db, krate);
            (items.Sized, LangItemEnum::GlobalAlloc.from_lang_items(items))
        });
        assert!(sized.is_some() && global.is_some());
    }

    /// A miss of `crate_notable_traits`, its inputs warm, allocates its box and nothing else.
    /// The query's table, made once a database on its first call, is warmed by a call for
    /// another crate.
    #[test]
    fn notable_traits_miss_is_its_result() {
        let db = fixture::db(
            r#"
//- /other.rs crate:other
pub trait Elsewhere {}
//- /main.rs crate:main deps:other
#[doc(notable_trait)]
pub trait Read {}
pub trait Other {}
"#,
        )
        .into_db();
        let named = |name: &str| {
            *base_db::all_crates(&db)
                .iter()
                .find(|it| it.extra_data(&db).display_name.as_ref().is_some_and(|it| it.to_string() == name))
                .unwrap_or_else(|| panic!("no crate {name}"))
        };
        let (krate, other) = (named("main"), named("other"));
        assert_eq!(super::crate_notable_traits(&db, other), None);
        let def_map = crate::nameres::crate_def_map(&db, krate);
        for (_, module) in def_map.modules(&db) {
            for def in module.scope(&db).declarations() {
                if let crate::ModuleDefId::TraitId(it) = def {
                    let _ = crate::attrs::AttrFlags::query(&db, it.into());
                }
            }
        }
        let traits = blaze_alloc_count::assert_at_most(1, "a notable-traits miss", || {
            super::crate_notable_traits(&db, krate).map(<[TraitId]>::len)
        });
        assert_eq!(traits, Some(1));
    }
}
