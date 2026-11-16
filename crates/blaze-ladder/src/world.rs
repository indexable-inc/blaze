//! The checker's `World` over a lowered program ([`crate::lower::Program`]): what blaze-typeck
//! asks while checking one body, answered from the program. Stands in for the item layer's
//! `World` over its records.
//!
//! Paths resolve in the body's module: an item of that module, a `mod` path, `crate::`,
//! `self::` and `super::` ([`Program::resolve`]). A fn is `Def::Fn`, a struct `Def::Adt` (the
//! checker takes it for a struct literal), a trait `Def::Trait`; a module alone is no value.
//! Lang items are the traits marked `#[lang = ".."]`. The subset has no impls, generics,
//! consts or opaque types, so those questions answer empty, as the toy world's do.

use blaze_typeck::{
    Adt, AssocItem, Def, Fetch, FnSig, Hidden, ImplHeader, ItemKey, LangItem, Name, PClause, PTys, PathIdx, PathRes,
    Predicates, ScopeId, Seal, Shape, TypeAttrs, World,
};

use crate::lower::{ItemRef, Program};

/// The world one body of `program` is checked in: the body's module's.
pub struct BodyWorld<'p> {
    pub program: &'p Program,
    /// The body's module, by index in [`Program::modules`].
    pub module: u32,
}

impl World for BodyWorld<'_> {
    fn fn_sig(&self, _: Seal, item: ItemKey) -> Fetch<Option<FnSig<'_>>> {
        Ok(self.program.index_of(item).map(|i| self.program.sig(i)))
    }

    fn predicates(&self, _: Seal, _: ItemKey) -> Fetch<Predicates<'_>> {
        Ok(Predicates { clauses: &[] as &[PClause], types: PTys::EMPTY })
    }

    fn adt(&self, _: Seal, item: ItemKey) -> Fetch<Option<Adt<'_>>> {
        Ok(self.program.adt(item))
    }

    fn impls_of(&self, _: Seal, _: ItemKey, _: Shape) -> Fetch<&[ItemKey]> {
        Ok(&[])
    }

    fn impl_header(&self, _: Seal, _: ItemKey) -> Fetch<Option<ImplHeader<'_>>> {
        Ok(None)
    }

    fn inherent(&self, _: Seal, _: Shape, _: Name) -> Fetch<&[AssocItem]> {
        Ok(&[])
    }

    fn assoc_item(&self, _: Seal, _: ItemKey, _: Name) -> Fetch<Option<AssocItem>> {
        Ok(None)
    }

    fn traits_in_scope(&self, _: Seal, _: ScopeId) -> Fetch<&[ItemKey]> {
        Ok(&[])
    }

    fn resolve_path(&self, _: Seal, _: ScopeId, _: PathIdx, segments: &[Name]) -> Fetch<Option<PathRes>> {
        let Some((item, consumed)) = self.program.resolve(self.module, segments) else { return Ok(None) };
        let Some(key) = self.program.key_of(item) else { return Ok(None) };
        let def = match item {
            ItemRef::Fn(_) => Def::Fn(key),
            ItemRef::Struct(_) => Def::Adt(key),
            ItemRef::Trait(_) => Def::Trait(key),
            ItemRef::Mod(_) => return Ok(None),
        };
        Ok(Some(PathRes { def, consumed }))
    }

    fn lang_item(&self, _: Seal, item: LangItem) -> Fetch<Option<ItemKey>> {
        Ok(self.program.lang.iter().find(|(l, _)| *l == item).map(|&(_, k)| k))
    }

    fn const_value(&self, _: Seal, _: ItemKey) -> Fetch<Option<u64>> {
        Ok(None)
    }

    fn opaque_hidden(&self, _: Seal, _: ItemKey) -> Fetch<Option<Hidden<'_>>> {
        Ok(None)
    }

    fn attrs(&self, _: Seal, item: ItemKey) -> Fetch<TypeAttrs> {
        let _ = self.program.item(item);
        Ok(TypeAttrs(0))
    }
}
