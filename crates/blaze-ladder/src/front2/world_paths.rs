//! A body path's resolution for [`BodyWorld`]: `Self` and the body's generic parameters
//! first (the body's own scope), then the names library ([`Resolver::path`]) at the body's
//! module. The library's kind of the item it found picks the checker's [`Def`]; a struct or
//! variant named as a value needs its ADT's record (its constructor kind).

use std::alloc::Global;
use std::vec::Vec;

use blaze_typeck::{Def, Fetch, Name, PNode, PathRes, Prim};

use super::heads::Want;
use super::keys::checker_of;
use super::types::SelfTy;
use super::world_items::{BodyWorld, Fill, Gap};
use crate::resolve::{Kind, Ns, ResolveError};

impl BodyWorld<'_, '_> {
    /// What `segments`, written in the body, name.
    pub(super) fn path(&self, segments: &[Name]) -> Fetch<Option<PathRes>> {
        let mut written = Vec::with_capacity_in(segments.len(), Global);
        for &s in segments {
            match self.spell.get(s) {
                Ok(t) => written.push(t),
                Err(u) => return self.gap(Gap::Unspelled(u)),
            }
        }
        let Some(&first) = written.first() else { return Ok(None) };
        if first == "Self" {
            return Ok(match self.self_ {
                SelfTy::Trait(_) => Some(PathRes { def: Def::TyParam(0), consumed: 1 }),
                SelfTy::Ty(t, _) => match self.store.tys.view().nodes.get(t.0 as usize) {
                    Some(PNode::Adt(k, _)) => Some(PathRes { def: Def::Adt(*k), consumed: 1 }),
                    _ => None,
                },
                SelfTy::None => None,
            });
        }
        if let Some(&(_, i)) = self.params.iter().find(|(n, _)| *n == segments[0]) {
            return Ok(Some(PathRes { def: Def::TyParam(i), consumed: 1 }));
        }
        // `::x`: blaze-lower writes the global root as a first segment `{{root}}`.
        let (abs, rest) = match written.split_first() {
            Some((&"{{root}}", rest)) => (true, rest),
            _ => (false, &written[..]),
        };
        let lead = (written.len() - rest.len()) as u32;
        let target = match self.resolver().path(self.module, abs, rest, Ns::Value) {
            Ok(t) => t,
            // The program names nothing: the checker's own diagnostic.
            Err(ResolveError::Path(_)) => return Ok(None),
            Err(e @ ResolveError::NoText { .. }) => return self.gap(Gap::Resolve(e)),
        };
        let consumed = target.consumed + lead;
        let all = consumed as usize == written.len();
        let k = checker_of(target.key);
        let def = match target.kind {
            Kind::Fn => Def::Fn(k),
            Kind::Const => Def::Const(k),
            Kind::Static => match self.store.statics.get(&k) {
                Some(&m) => Def::Static(k, m),
                None => return self.want(k),
            },
            Kind::Struct if all && self.names.ctor(target.key).is_some() => self.ctor(target.key, 0)?,
            Kind::Struct | Kind::Enum | Kind::Union => Def::Adt(k),
            Kind::Variant { adt, index } => self.ctor(adt, index)?,
            Kind::Trait => {
                // `Trait::f`: the trait's fn.
                if let Some(&next) = segments.get(consumed as usize) {
                    let Some(t) = self.store.maps.traits.get(&target.key) else {
                        return self.fill(Fill::Store(Want::Head(target.key)));
                    };
                    if let Some(&(_, f)) = t.fns.iter().find(|(n, _)| *n == next) {
                        return Ok(Some(PathRes { def: Def::Fn(f), consumed: consumed + 1 }));
                    }
                }
                Def::Trait(k)
            }
            Kind::TypeAlias => return self.gap(Gap::TypeAlias(target.key)),
            // A built-in type in a body path (`u8::MAX`, `str::len`): the names library's
            // answer mapped through the one table (`PRIMS`); the checker looks the next segment
            // up on it. blaze's wide ints have no checker type yet.
            Kind::Builtin(name) => match super::types::PRIMS.iter().find(|(p, _)| *p == name).map(|(_, n)| *n) {
                Some(PNode::Bool) => Def::Prim(Prim::Bool),
                Some(PNode::Char) => Def::Prim(Prim::Char),
                Some(PNode::Str) => Def::Prim(Prim::Str),
                Some(PNode::Int(i)) => Def::Prim(Prim::Int(i)),
                Some(PNode::Uint(u)) => Def::Prim(Prim::Uint(u)),
                Some(PNode::Float(f)) => Def::Prim(Prim::Float(f)),
                _ => return self.gap(Gap::BuiltinTypePath(written.join("::").into_boxed_str())),
            },
            Kind::WideInt { .. } => return self.gap(Gap::BuiltinTypePath(written.join("::").into_boxed_str())),
            Kind::Module | Kind::Macro | Kind::TraitAlias | Kind::Other(_) => return Ok(None),
        };
        Ok(Some(PathRes { def, consumed }))
    }
}
