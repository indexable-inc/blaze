//! This module defines a `DynMap` -- a container for heterogeneous maps.
//!
//! This means that `DynMap` stores a bunch of hash maps inside, and those maps
//! can be of different types: one per key in [`keys`], each a field of [`keys::Maps`].
//!
//! It is used like this:
//!
//! ```ignore
//! # use hir_def::dyn_map::{DynMap, keys};
//! let mut map = DynMap::default();
//!
//! // To access a specific map, index the `DynMap` by a key:
//! map[keys::FUNCTION].insert(ptr, function_id);
//! let value = map[keys::CONST].get(&const_ptr);
//! ```
//!
//! This is a work of fiction. Any similarities to Kotlin's `BindingContext` are
//! a coincidence.

pub mod keys {
    use std::marker::PhantomData;

    use either::Either;
    use hir_expand::{MacroCallId, attrs::AttrId};
    use syntax::{AstNode, AstPtr, ast};

    use crate::{
        BlockId, BuiltinDeriveImplId, ConstId, EnumId, EnumVariantId, ExternBlockId, ExternCrateId,
        FieldId, FunctionId, ImplId, LifetimeParamId, Macro2Id, MacroRulesId, ProcMacroId,
        StaticId, StructId, TraitId, TypeAliasId, TypeOrConstParamId, UnionId, UseId,
        dyn_map::{DynMap, KeyMap, Policy},
    };

    pub type Key<K, V> = crate::dyn_map::Key<AstPtr<K>, V, AstPtrPolicy<K, V>>;

    /// The derive attribute's id, the `derive()` call and the actual derive macros.
    pub type Derives =
        (AttrId, MacroCallId, Box<[Option<Either<MacroCallId, BuiltinDeriveImplId>>]>);

    /// Each key with its own map, a field of [`Maps`]: the keys are a closed set, so a lookup
    /// is a field access.
    macro_rules! keys {
        ($($key:ident, $field:ident: $ast:ty => $id:ty;)*) => {
            $(pub const $key: Key<$ast, $id> = Key::new();)*

            /// One map per key.
            #[derive(Default)]
            pub struct Maps {
                $($field: KeyMap<AstPtr<$ast>, $id>,)*
            }

            $(impl Slot<$ast, $id> for AstPtrPolicy<$ast, $id> {
                fn slot(maps: &Maps) -> &KeyMap<AstPtr<$ast>, $id> {
                    &maps.$field
                }
                fn slot_mut(maps: &mut Maps) -> &mut KeyMap<AstPtr<$ast>, $id> {
                    &mut maps.$field
                }
            })*
        };
    }

    keys! {
        BLOCK, block: ast::BlockExpr => BlockId;
        FUNCTION, function: ast::Fn => FunctionId;
        CONST, konst: ast::Const => ConstId;
        STATIC, static_: ast::Static => StaticId;
        TYPE_ALIAS, type_alias: ast::TypeAlias => TypeAliasId;
        IMPL, impl_: ast::Impl => ImplId;
        EXTERN_BLOCK, extern_block: ast::ExternBlock => ExternBlockId;
        TRAIT, trait_: ast::Trait => TraitId;
        STRUCT, struct_: ast::Struct => StructId;
        UNION, union_: ast::Union => UnionId;
        ENUM, enum_: ast::Enum => EnumId;
        EXTERN_CRATE, extern_crate: ast::ExternCrate => ExternCrateId;
        USE, use_: ast::Use => UseId;

        ENUM_VARIANT, enum_variant: ast::Variant => EnumVariantId;
        TUPLE_FIELD, tuple_field: ast::TupleField => FieldId;
        RECORD_FIELD, record_field: ast::RecordField => FieldId;
        TYPE_PARAM, type_param: ast::TypeParam => TypeOrConstParamId;
        CONST_PARAM, const_param: ast::ConstParam => TypeOrConstParamId;
        LIFETIME_PARAM, lifetime_param: ast::LifetimeParam => LifetimeParamId;

        MACRO_RULES, macro_rules_: ast::MacroRules => MacroRulesId;
        MACRO2, macro2: ast::MacroDef => Macro2Id;
        PROC_MACRO, proc_macro: ast::Fn => ProcMacroId;
        MACRO_CALL, macro_call: ast::MacroCall => MacroCallId;
        ATTR_MACRO_CALL, attr_macro_call: ast::Item => MacroCallId;
        DERIVE_MACRO_CALL, derive_macro_call: ast::Meta => Derives;
    }

    /// The field of [`Maps`] that a key's policy reads.
    pub trait Slot<AST: AstNode, ID> {
        fn slot(maps: &Maps) -> &KeyMap<AstPtr<AST>, ID>;
        fn slot_mut(maps: &mut Maps) -> &mut KeyMap<AstPtr<AST>, ID>;
    }

    /// XXX: AST Nodes and SyntaxNodes have identity equality semantics: nodes are
    /// equal if they point to exactly the same object.
    ///
    /// In general, we do not guarantee that we have exactly one instance of a
    /// syntax tree for each file. We probably should add such guarantee, but, for
    /// the time being, we will use identity-less AstPtr comparison.
    pub struct AstPtrPolicy<AST, ID> {
        _phantom: PhantomData<(AST, ID)>,
    }

    impl<AST: AstNode + 'static, ID: 'static> Policy for AstPtrPolicy<AST, ID>
    where
        Self: Slot<AST, ID>,
    {
        type K = AstPtr<AST>;
        type V = ID;
        fn map(map: &DynMap) -> &KeyMap<AstPtr<AST>, ID> {
            <Self as Slot<AST, ID>>::slot(&map.maps)
        }
        fn map_mut(map: &mut DynMap) -> &mut KeyMap<AstPtr<AST>, ID> {
            <Self as Slot<AST, ID>>::slot_mut(&mut map.maps)
        }
    }
}

use std::{
    hash::Hash,
    marker::PhantomData,
    ops::{Index, IndexMut},
};

use blaze_hash::HashMap;

pub struct Key<K, V, P> {
    _phantom: PhantomData<(K, V, P)>,
}

impl<K, V, P> Key<K, V, P> {
    pub(crate) const fn new() -> Key<K, V, P> {
        Key { _phantom: PhantomData }
    }
}

impl<K, V, P> Copy for Key<K, V, P> {}

impl<K, V, P> Clone for Key<K, V, P> {
    fn clone(&self) -> Key<K, V, P> {
        *self
    }
}

pub trait Policy {
    type K;
    type V;

    /// The key's own map, a field of [`keys::Maps`].
    fn map(map: &DynMap) -> &KeyMap<Self::K, Self::V>;
    fn map_mut(map: &mut DynMap) -> &mut KeyMap<Self::K, Self::V>;
}

#[derive(Default)]
pub struct DynMap {
    pub(crate) maps: keys::Maps,
}

/// The map of one key: indexing a [`DynMap`] by the key is a field access.
pub struct KeyMap<K, V> {
    map: HashMap<K, V>,
}

impl<K, V> Default for KeyMap<K, V> {
    fn default() -> Self {
        KeyMap { map: HashMap::default() }
    }
}

impl<K: Hash + Eq, V> KeyMap<K, V> {
    pub fn insert(&mut self, key: K, value: V) {
        self.map.insert(key, value);
    }
    pub fn get(&self, key: &K) -> Option<&V> {
        self.map.get(key)
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

impl<P: Policy> Index<Key<P::K, P::V, P>> for DynMap {
    type Output = KeyMap<P::K, P::V>;
    fn index(&self, _key: Key<P::K, P::V, P>) -> &Self::Output {
        P::map(self)
    }
}

impl<P: Policy> IndexMut<Key<P::K, P::V, P>> for DynMap {
    fn index_mut(&mut self, _key: Key<P::K, P::V, P>) -> &mut Self::Output {
        P::map_mut(self)
    }
}

#[cfg(test)]
mod tests {
    use syntax::{AstPtr, ast};
    use hir_expand::fixture;

    use super::{DynMap, keys};
    use crate::{
        Lookup,
        src::HasSource,
        test_db::TestDBExt,
        test_items,
    };

    #[test]
    fn each_key_has_its_own_map() {
        let db = fixture::db("//- /main.rs\nfn f() {}\nconst C: u8 = 0;\n").into_db();
        let krate = db.fetch_test_crate();
        let f = test_items::function(&db, krate, "f");
        let c = test_items::constant(&db, krate, "C");
        let f_ptr = AstPtr::<ast::Fn>::of(f.lookup(&db).source_view(&db).value);
        let c_ptr = AstPtr::<ast::Const>::of(c.lookup(&db).source_view(&db).value);

        let mut map = DynMap::default();
        assert!(map[keys::FUNCTION].is_empty());
        map[keys::FUNCTION].insert(f_ptr, f);
        map[keys::CONST].insert(c_ptr, c);

        assert!(!map[keys::FUNCTION].is_empty());
        assert_eq!(map[keys::FUNCTION].get(&f_ptr), Some(&f));
        assert_eq!(map[keys::CONST].get(&c_ptr), Some(&c));
        // `PROC_MACRO` keys the same syntax as `FUNCTION`, in a map of its own.
        assert!(map[keys::PROC_MACRO].is_empty());
        assert_eq!(map[keys::PROC_MACRO].get(&f_ptr), None);
        assert!(map[keys::STATIC].is_empty());
    }
}
