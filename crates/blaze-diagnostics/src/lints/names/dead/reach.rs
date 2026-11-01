//! The definitions public at rustc's `Level::Reachable` (`is_reachable`): what another crate
//! can name or reach. Read once from the crate's effective visibilities (`names::effective`)
//! into a bitset over the numbering, so that each question the pass asks is a bit test.

use blaze_hash::HashMap;
use hir::{InFile, ModuleDef};
use hir_def::{Lookup as _, ModuleId};

use super::defs::{DefIdx, DefSet, Defs, Id};
use crate::{
    cx::Cx,
    lints::names::effective::{Def, Effective},
};

pub(super) struct Reach {
    public: DefSet,
}

impl Reach {
    pub(super) fn of(cx: &Cx<'_, '_>, defs: &Defs, effective: &Effective) -> Reach {
        let db = cx.db;
        // The `use`s of the crate's modules by their nodes; one in a body has no effective
        // visibility.
        let mut uses = HashMap::default(); // alloc: setup, once per crate
        for &module in &defs.module_ids {
            let id: ModuleId = module.into();
            for use_ in id.def_map(db).module(db, id).scope(db).use_decls() {
                let ast = use_.lookup(db).id;
                uses.insert(InFile::new(ast.file_id, ast.erase().to_ptr(db)), use_);
            }
        }
        let mut public = DefSet::new(defs.len());
        for def in defs.all() {
            let key = match defs[def].id {
                Id::Fn(it) => Def::Item(ModuleDef::Function(it.into())),
                Id::Const(it) => Def::Item(ModuleDef::Const(it.into())),
                Id::Static(it) => Def::Item(ModuleDef::Static(it.into())),
                Id::Adt(it) => Def::Item(ModuleDef::Adt(it.into())),
                Id::Variant(it) => Def::Item(ModuleDef::EnumVariant(it.into())),
                Id::Field(it) => Def::Field(it.into()),
                Id::Trait(it) => Def::Item(ModuleDef::Trait(it.into())),
                Id::Alias(it) => Def::Item(ModuleDef::TypeAlias(it.into())),
                Id::Impl(it) => Def::Impl(it.into()),
                // A trait impl's items have no visibility of their own: they are as reachable as
                // the impl.
                Id::Derive(it) | Id::DeriveMethod(it) => Def::Impl(it.into()),
                Id::Use(node) => match uses.get(&node) {
                    Some(&it) => Def::Use(it),
                    None => continue,
                },
            };
            if effective.is_reachable(key) {
                public.insert(def);
            }
        }
        Reach { public }
    }

    /// rustc's `is_public_at_level(def, Level::Reachable)`, and `is_reachable`.
    pub(super) fn is_reachable(&self, def: DefIdx) -> bool {
        self.public.contains(def)
    }
}
