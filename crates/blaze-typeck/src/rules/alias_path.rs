//! A body path through a type alias whose aliased type no plain `Def` names ([`Def::Alias`]).
//!
//! The Reference, "Type aliases": "A type alias defines a new name for an existing type". So
//! `A::new()`, `A::CONST`, `A::Variant`, `A { .. }` with `type A<..> = X;` mean what they mean
//! with `X` written, the alias's parameters replaced by the arguments on the alias's segment.
//! rustc lowers the alias's segment as a type (`lower_path_segment` on the alias's `DefKind`,
//! then `type_of(alias).instantiate(args)`) and resolves the next segment on that type; the
//! same here: [`Machine::alias_path_ty`] is the aliased type at the segment's arguments,
//! normalized at its top (`type Scalar<C> = <C as CurveArithmetic>::Scalar` is the projection,
//! which `C`'s bounds or an impl name), and the callers in lit_path.rs (`value_path`) and
//! adt.rs (`struct_path`) read the next segment on it as on any written type.
//!
//! Arguments. None written: one fresh variable per parameter, as for any value path that
//! writes none (rustc's `infer_args` for a path in an expression or a pattern). All written:
//! each checked against its parameter as an ADT's are ([`Machine::explicit_args`]: a count
//! other than the alias's is E0107). Some written and the rest defaulted (`type R<T, E =
//! Error>` written `R::<u8>::f()`): each default over the arguments before it (the Reference,
//! "Generic parameters": "a default ... may refer to earlier parameters").

use blaze_own::{Alloc, Vec};

use crate::body::Segment;
use crate::diag::{About, At, DiagCode};
use crate::error::Stop;
use crate::ids::ItemKey;
use crate::machine::{Lower, Machine};
use crate::ty::{Cause, Ty, TyList, TypeCtx};
use crate::world::{AliasDef, World};

impl<'a, W: World, C: TypeCtx> Machine<'_, '_, '_, 'a, W, C> {
    /// The type the alias `k`, named by `seg` in a body path, stands for at `seg`'s arguments
    /// (module docs), resolved at its top; `None` after E0425 naming `seg` when the world holds
    /// no alias of that key.
    pub(crate) fn alias_path_ty(&mut self, at: At, cause: Cause, k: ItemKey, seg: Segment) -> Result<Option<Ty>, Stop> {
        let Some(def) = self.reads.type_alias(k)? else {
            self.diag(DiagCode::UnresolvedPath, at, About::path(Some(seg.name)));
            return Ok(None);
        };
        let args = self.alias_args(at, cause, k, seg, &def)?;
        let t = self.lower(def.types, def.ty, Lower::Args(args), cause)?;
        Ok(Some(self.try_structurally_resolve(t)?))
    }

    /// The alias's arguments as `seg` writes them (module docs, "Arguments").
    fn alias_args(&mut self, at: At, cause: Cause, k: ItemKey, seg: Segment, def: &AliasDef<'_>) -> Result<TyList, Stop> {
        let n = def.generics;
        if !seg.has_args {
            return self.fresh_generic_args(k, 0, n);
        }
        let written = self.body.type_list(seg.generic_args)?.len();
        let w = u32::try_from(written).unwrap_or(u32::MAX);
        let defaulted = w < n && (w..n).all(|i| def.defaults.get(i as usize).is_some_and(|d| d.is_some()));
        if !defaulted {
            // Exactly as many as the alias declares, or a wrong count (E0107 there).
            return self.explicit_args(at, Some(seg), k, 0, n);
        }
        let given = self.explicit_args(at, Some(seg), k, 0, w)?;
        let mut v: Vec<Ty, Alloc<'a>> = self.list(given);
        for i in w..n {
            let Some(&Some(d)) = def.defaults.get(i as usize) else {
                return self.explicit_args(at, Some(seg), k, 0, n);
            };
            let before = self.mk_list(&v)?;
            v.push(self.lower(def.types, d, Lower::Args(before), cause)?);
        }
        self.mk_list(&v)
    }
}
