//! Definition of builtin derive impls.
//!
//! To save time and memory, builtin derives are not really expanded. Instead, we record them
//! and create their impls based on lowered data, see crates/hir-ty/src/builtin_derive.rs.

use base_db::Db;
use hir_expand::{AstId, InFile, builtin::BuiltinDeriveExpander, name::Name};
use intern::{Symbol, sym};
use syntax::ast;
use tt::TextRange;

use crate::{
    AdtId, BuiltinDeriveImplId, BuiltinDeriveImplLoc, FunctionId, HasModule, MacroId,
    lang_item::LangItems,
};

macro_rules! declare_enum {
    ( $( $trait:ident => [ $( $method:ident ),* ] ),* $(,)? ) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum BuiltinDeriveImplTrait {
            $( $trait, )*
        }

        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        #[allow(non_camel_case_types)]
        pub enum BuiltinDeriveImplMethod {
            $( $( $method, )* )*
        }

        impl BuiltinDeriveImplTrait {
            /// Every variant, in declaration order.
            pub const ALL: &[Self] = &[$(Self::$trait),*];

            #[inline]
            pub fn name(self) -> Symbol {
                match self {
                    $( Self::$trait => sym::$trait, )*
                }
            }

            #[inline]
            pub fn get_id(self, lang_items: &crate::lang_item::LangItems) -> Option<crate::TraitId> {
                match self {
                    $( Self::$trait => lang_items.$trait, )*
                }
            }

            #[inline]
            pub fn get_method(self, method_name: &Symbol) -> Option<BuiltinDeriveImplMethod> {
                match self {
                    $(
                        Self::$trait => {
                            match method_name {
                                $( _ if *method_name == sym::$method => Some(BuiltinDeriveImplMethod::$method), )*
                                _ => None,
                            }
                        }
                    )*
                }
            }

            #[inline]
            pub fn all_methods(self) -> &'static [BuiltinDeriveImplMethod] {
                match self {
                    $( Self::$trait => &[ $(BuiltinDeriveImplMethod::$method),* ], )*
                }
            }
        }

        impl BuiltinDeriveImplMethod {
            #[inline]
            pub fn name(self) -> Symbol {
                match self {
                    $( $( BuiltinDeriveImplMethod::$method => sym::$method, )* )*
                }
            }
        }
    };
}

declare_enum!(
    Copy => [],
    Clone => [clone],
    Default => [default],
    Debug => [fmt],
    Hash => [hash],
    Ord => [cmp],
    PartialOrd => [partial_cmp],
    Eq => [],
    PartialEq => [eq],
    CoerceUnsized => [],
    DispatchFromDyn => [],
    Reborrow => [],
);

impl BuiltinDeriveImplTrait {
    pub fn derive_macro(self, lang_items: &LangItems) -> Option<MacroId> {
        match self {
            BuiltinDeriveImplTrait::Copy => lang_items.CopyDerive,
            BuiltinDeriveImplTrait::Clone => lang_items.CloneDerive,
            BuiltinDeriveImplTrait::Default => lang_items.DefaultDerive,
            BuiltinDeriveImplTrait::Debug => lang_items.DebugDerive,
            BuiltinDeriveImplTrait::Hash => lang_items.HashDerive,
            BuiltinDeriveImplTrait::Ord => lang_items.OrdDerive,
            BuiltinDeriveImplTrait::PartialOrd => lang_items.PartialOrdDerive,
            BuiltinDeriveImplTrait::Eq => lang_items.EqDerive,
            BuiltinDeriveImplTrait::PartialEq => lang_items.PartialEqDerive,
            BuiltinDeriveImplTrait::CoerceUnsized | BuiltinDeriveImplTrait::DispatchFromDyn => {
                lang_items.CoercePointeeDerive
            }
            BuiltinDeriveImplTrait::Reborrow => lang_items.ReborrowDerive,
        }
    }
}

pub(crate) fn has_builtin_derive_impl(derive: BuiltinDeriveExpander) -> bool {
    !matches!(
        derive,
        BuiltinDeriveExpander::CoerceShared
            | BuiltinDeriveExpander::ConstParamTy
            | BuiltinDeriveExpander::From
    )
}

impl BuiltinDeriveImplMethod {
    pub fn trait_method(
        self,
        db: &Db,
        impl_: BuiltinDeriveImplId,
    ) -> Option<FunctionId> {
        let loc = impl_.loc(db);
        let lang_items = crate::lang_item::lang_items(db, loc.krate(db));
        let trait_ = loc.trait_.get_id(lang_items)?;
        trait_.trait_items(db).method_by_name(&Name::new_symbol_root(self.name()))
    }
}

pub(crate) fn with_derive_traits(
    derive: BuiltinDeriveExpander,
    mut f: impl FnMut(BuiltinDeriveImplTrait),
) {
    debug_assert!(has_builtin_derive_impl(derive));
    match derive {
        BuiltinDeriveExpander::Copy => f(BuiltinDeriveImplTrait::Copy),
        BuiltinDeriveExpander::Clone => f(BuiltinDeriveImplTrait::Clone),
        BuiltinDeriveExpander::Default => f(BuiltinDeriveImplTrait::Default),
        BuiltinDeriveExpander::Debug => f(BuiltinDeriveImplTrait::Debug),
        BuiltinDeriveExpander::Hash => f(BuiltinDeriveImplTrait::Hash),
        BuiltinDeriveExpander::Ord => f(BuiltinDeriveImplTrait::Ord),
        BuiltinDeriveExpander::PartialOrd => f(BuiltinDeriveImplTrait::PartialOrd),
        BuiltinDeriveExpander::Eq => f(BuiltinDeriveImplTrait::Eq),
        BuiltinDeriveExpander::PartialEq => f(BuiltinDeriveImplTrait::PartialEq),
        BuiltinDeriveExpander::CoercePointee => {
            f(BuiltinDeriveImplTrait::CoerceUnsized);
            f(BuiltinDeriveImplTrait::DispatchFromDyn);
        }
        BuiltinDeriveExpander::Reborrow => f(BuiltinDeriveImplTrait::Reborrow),
        BuiltinDeriveExpander::CoerceShared
        | BuiltinDeriveExpander::ConstParamTy
        | BuiltinDeriveExpander::From => {}
    }
}

impl BuiltinDeriveImplLoc {
    /// Whether the derive is listed in a `#[derive_const(..)]`, which makes the impl a
    /// `const impl`.
    pub fn is_const(&self, db: &Db) -> bool {
        let (adt_ast_id, module) = self.adt_source(db);
        let (_, meta) = self.derive_attr_id.find_attr_range_view(db, module.krate(db), adt_ast_id);
        meta.path()
            .and_then(|path| path.as_single_name_ref())
            .is_some_and(|name| name.text() == "derive_const")
    }

    pub fn source(&self, db: &Db) -> InFile<TextRange> {
        let (adt_ast_id, module) = self.adt_source(db);
        let derive_range = self.derive_attr_id.find_derive_range(
            db,
            module.krate(db),
            adt_ast_id,
            self.derive_index,
        );
        adt_ast_id.with_value(derive_range)
    }

    fn adt_source(&self, db: &Db) -> (AstId<ast::Adt>, crate::ModuleId) {
        match self.adt {
            AdtId::StructId(adt) => {
                let adt_loc = adt.loc(db);
                (adt_loc.id.upcast(), adt_loc.container)
            }
            AdtId::UnionId(adt) => {
                let adt_loc = adt.loc(db);
                (adt_loc.id.upcast(), adt_loc.container)
            }
            AdtId::EnumId(adt) => {
                let adt_loc = adt.loc(db);
                (adt_loc.id.upcast(), adt_loc.container)
            }
        }
    }
}
