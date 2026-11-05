//! In rust, it is possible to have a value, a type and a macro with the same
//! name without conflicts.
//!
//! `PerNs` (per namespace) captures this.

use crate::{
    MacroId, ModuleDefId,
    item_scope::{ImportId, ImportOrExternCrate, ImportOrGlob, ItemInNs},
    visibility::Visibility,
};

#[derive(PartialEq, Eq, Hash, Copy, Clone, Debug)]
pub enum Namespace {
    Types,
    Values,
    Macros,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Item<Def, Import = ImportId> {
    pub def: Def,
    pub vis: Visibility,
    pub import: Option<Import>,
}

impl<Def: PartialEq, Import: Copy + PartialEq> Item<Def, Import> {
    /// An explicit import can shadow a glob with another definition or with a different
    /// visibility for the same definition. Re-exports that saw the glob must follow it.
    pub(crate) fn is_reresolved_by(&self, item: &Self, import: Option<Import>) -> bool {
        import.is_some() && self.import == import && (self.def != item.def || self.vis != item.vis)
    }
}

pub type TypesItem = Item<ModuleDefId, ImportOrExternCrate>;
pub type ValuesItem = Item<ModuleDefId, ImportOrGlob>;
// May be Externcrate for `[macro_use]`'d macros
pub type MacrosItem = Item<MacroId, ImportOrExternCrate>;

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct PerNs {
    pub types: Option<TypesItem>,
    pub values: Option<ValuesItem>,
    pub macros: Option<MacrosItem>,
}

// Ids are 8 bytes (index and generation), a `Visibility` a tag and a module id, a def a tag
// and an id; each `Option` lives in a niche of the def's tag, so a `PerNs` is three items and
// no more. `item_scope` stores none of these: it packs an entry into 40 bytes.
const _: () = {
    assert!(size_of::<TypesItem>() <= 44);
    assert!(size_of::<ValuesItem>() <= 44);
    assert!(size_of::<MacrosItem>() <= 40);
    assert!(size_of::<Item<()>>() <= 24);
    assert!(size_of::<Option<TypesItem>>() == size_of::<TypesItem>());
    assert!(size_of::<PerNs>() <= 128);
};

impl PerNs {
    pub fn none() -> PerNs {
        PerNs { types: None, values: None, macros: None }
    }

    pub fn values(def: ModuleDefId, vis: Visibility, import: Option<ImportOrGlob>) -> PerNs {
        PerNs { types: None, values: Some(Item { def, vis, import }), macros: None }
    }

    pub fn types(def: ModuleDefId, vis: Visibility, import: Option<ImportOrExternCrate>) -> PerNs {
        PerNs { types: Some(Item { def, vis, import }), values: None, macros: None }
    }

    pub fn both(
        types: ModuleDefId,
        values: ModuleDefId,
        vis: Visibility,
        import: Option<ImportOrExternCrate>,
    ) -> PerNs {
        PerNs {
            types: Some(Item { def: types, vis, import }),
            values: Some(Item {
                def: values,
                vis,
                import: import.and_then(ImportOrExternCrate::import_or_glob),
            }),
            macros: None,
        }
    }

    pub fn macros(def: MacroId, vis: Visibility, import: Option<ImportOrExternCrate>) -> PerNs {
        PerNs { types: None, values: None, macros: Some(Item { def, vis, import }) }
    }

    pub fn is_none(&self) -> bool {
        self.types.is_none() && self.values.is_none() && self.macros.is_none()
    }

    pub fn is_full(&self) -> bool {
        self.types.is_some() && self.values.is_some() && self.macros.is_some()
    }

    pub fn take_types(self) -> Option<ModuleDefId> {
        self.types.map(|it| it.def)
    }

    pub fn take_types_full(self) -> Option<TypesItem> {
        self.types
    }

    pub fn take_values(self) -> Option<ModuleDefId> {
        self.values.map(|it| it.def)
    }

    pub fn take_values_full(self) -> Option<ValuesItem> {
        self.values
    }

    pub fn take_macros(self) -> Option<MacroId> {
        self.macros.map(|it| it.def)
    }

    pub fn take_macros_full(self) -> Option<MacrosItem> {
        self.macros
    }

    pub fn filter_visibility(self, mut f: impl FnMut(Visibility) -> bool) -> PerNs {
        PerNs {
            types: self.types.filter(|def| f(def.vis)),
            values: self.values.filter(|def| f(def.vis)),
            macros: self.macros.filter(|def| f(def.vis)),
        }
    }

    pub fn with_visibility(self, vis: Visibility) -> PerNs {
        PerNs {
            types: self.types.map(|def| Item { vis, ..def }),
            values: self.values.map(|def| Item { vis, ..def }),
            macros: self.macros.map(|def| Item { vis, ..def }),
        }
    }

    pub fn or(self, other: PerNs) -> PerNs {
        PerNs {
            types: self.types.or(other.types),
            values: self.values.or(other.values),
            macros: self.macros.or(other.macros),
        }
    }

    pub fn or_else(self, f: impl FnOnce() -> PerNs) -> PerNs {
        if self.is_full() { self } else { self.or(f()) }
    }

    pub fn iter_items(self) -> impl Iterator<Item = (ItemInNs, Option<ImportOrExternCrate>)> {
        self.types
            .map(|it| (ItemInNs::Types(it.def), it.import))
            .into_iter()
            .chain(
                self.values
                    .map(|it| (ItemInNs::Values(it.def), it.import.map(ImportOrExternCrate::from))),
            )
            .chain(self.macros.map(|it| (ItemInNs::Macros(it.def), it.import)))
    }
}
