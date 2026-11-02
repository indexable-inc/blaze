//! What a public opaque macro makes reachable: the front end's half of rustc's
//! `EmbargoVisitor::update_reachability_from_macro` (`compiler/rustc_privacy/src/lib.rs`, MIT or
//! Apache-2.0).
//!
//! rustc doesn't read what the macro's body names. Each name of the macro's module that is
//! accessible from that module is reached at the macro's effective visibility, and so are the
//! names of the modules among them that are accessible from it, in turn, and the types of the
//! accessible fields of the structs and unions among them. Each later pass of rustc's fixpoint
//! that finds the macro's module already walked walks its parent module instead, still asking
//! what is accessible from the macro's module. The fixpoint and the effective visibilities are
//! the caller's: [`macro_reach_module`] says whether a macro takes part, and
//! [`MacroReachable::update_from_macro`] is one pass's visit of it.

use base_db::Db;
use blaze_hash::FixedState;
use blaze_mem::frame::Frame;
use hir_expand::MacroDefKind;
use span::Transparency;

use crate::{
    AdtId, AttrDefId, FieldId, HasModule, MacroId, ModuleDefId, ModuleId, VariantId,
    attrs::symbol_attr, signatures::VariantFields,
};

#[cfg(test)]
mod tests;

/// The module a macro makes names reachable in, once it is public at some level: rustc's
/// `update_reachability_from_macro` before its effective visibility check. `None` for a macro
/// that isn't opaque (a `macro_rules!` unless `#[rustc_macro_transparency = "opaque"]`, a
/// `macro` with another transparency), a procedural macro, and one whose parent is no module (a
/// block's).
pub fn macro_reach_module(db: &Db, id: MacroId) -> Option<ModuleId> {
    let def = id.definition(db);
    let transparency = match def.kind {
        MacroDefKind::Declarative(ast_id, _) => ast_id.decl_macro_expander(db, def.krate).transparency,
        MacroDefKind::ProcMacro(..) => return None,
        _ => {
            let written = symbol_attr(db, AttrDefId::MacroId(id), "rustc_macro_transparency");
            match written.as_ref().map(|it| it.as_str()) {
                Some("transparent") => Transparency::Transparent,
                Some("semiopaque" | "semitransparent") => Transparency::SemiOpaque,
                Some("opaque") => Transparency::Opaque,
                _ => match id {
                    MacroId::MacroRulesId(_) => Transparency::SemiOpaque,
                    MacroId::Macro2Id(_) | MacroId::ProcMacroId(_) => Transparency::Opaque,
                },
            }
        }
    };
    if transparency != Transparency::Opaque {
        return None;
    }
    let module = id.module(db);
    if module.is_block_module(db) {
        return None;
    }
    Some(module)
}

/// What a walk reaches, each at the macro's effective visibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Reached {
    /// rustc's `update(def, macro_ev, Level::Reachable)`: the definition is reachable as far as
    /// its own visibility lets it be.
    Def(ModuleDefId),
    /// rustc's `reach(field, macro_ev).ty()`: what the field's type names is reachable; the
    /// field itself is not reached.
    FieldType(FieldId),
}

/// rustc's `macro_reachable`: the pairs of a module walked and the macro's module it was walked
/// for, kept across the macros and the passes of one fixpoint, in the frame of the run that
/// holds the fixpoint.
#[derive(Debug)]
pub struct MacroReachable<'f> {
    walked: hashbrown::HashSet<(ModuleId, ModuleId), FixedState, &'f Frame>,
    stack: Vec<ModuleId, &'f Frame>,
}

impl<'f> MacroReachable<'f> {
    pub fn new_in(frame: &'f Frame) -> MacroReachable<'f> {
        MacroReachable {
            walked: hashbrown::HashSet::with_hasher_in(FixedState, frame),
            stack: Vec::new_in(frame),
        }
    }

    /// One pass's visit of a macro public at some level, made from `defining`, the module
    /// [`macro_reach_module`] gave: walks `defining` the first time, and the first ancestor not
    /// yet walked for it in each later pass, up to the crate root.
    pub fn update_from_macro(
        &mut self,
        db: &Db,
        defining: ModuleId,
        mut each: impl FnMut(Reached),
    ) {
        let mut module = defining;
        loop {
            if self.update_macro_reachable(db, module, defining, &mut each) {
                break;
            }
            match module.def_map(db).module(db, module).parent {
                Some(parent) if !parent.is_block_module(db) => module = parent,
                _ => break,
            }
        }
    }

    /// rustc's `update_macro_reachable`: when `module` hasn't been walked for `defining`, each
    /// name of it accessible from `defining` that is of this crate, and, through the modules
    /// among them that are accessible from `defining` by their own visibility, theirs. Whether
    /// `module` was new.
    pub fn update_macro_reachable(
        &mut self,
        db: &Db,
        module: ModuleId,
        defining: ModuleId,
        each: &mut impl FnMut(Reached),
    ) -> bool {
        if !self.walked.insert((module, defining)) {
            return false;
        }
        let krate = defining.krate(db);
        debug_assert!(self.stack.is_empty());
        self.stack.push(module);
        while let Some(module) = self.stack.pop() {
            let scope = module.def_map(db).module(db, module).scope(db);
            let names = scope.entries().flat_map(|(_, per_ns)| {
                let types = per_ns.types.map(|it| (it.def, it.vis));
                let values = per_ns.values.map(|it| (it.def, it.vis));
                let macros = per_ns.macros.map(|it| (ModuleDefId::MacroId(it.def), it.vis));
                types.into_iter().chain(values).chain(macros)
            });
            let unnamed = scope
                .unnamed_trait_imports
                .iter()
                .map(|&(trait_, item)| (ModuleDefId::TraitId(trait_), item.vis));
            for (def, vis) in names.chain(unnamed) {
                if !vis.is_visible_from(db, defining) {
                    continue;
                }
                match def.module(db) {
                    Some(home) if home.krate(db) == krate => {}
                    _ => continue,
                }
                each(Reached::Def(def));
                match def {
                    ModuleDefId::ModuleId(child) => {
                        let own = child.def_map(db).module(db, child).visibility;
                        if own.is_visible_from(db, defining) && self.walked.insert((child, defining))
                        {
                            self.stack.push(child);
                        }
                    }
                    ModuleDefId::AdtId(AdtId::StructId(it)) => {
                        fields(db, VariantId::StructId(it), defining, each)
                    }
                    ModuleDefId::AdtId(AdtId::UnionId(it)) => {
                        fields(db, VariantId::UnionId(it), defining, each)
                    }
                    _ => {}
                }
            }
        }
        true
    }
}

/// The fields of a struct or union accessible from `defining`, whose types rustc reaches.
fn fields(
    db: &Db,
    variant: VariantId,
    defining: ModuleId,
    each: &mut impl FnMut(Reached),
) {
    for (local_id, vis) in VariantFields::field_visibilities(db, variant).iter() {
        if vis.is_visible_from(db, defining) {
            each(Reached::FieldType(FieldId { parent: variant, local_id }));
        }
    }
}
