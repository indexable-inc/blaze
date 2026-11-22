//! The argument count of a projection onto a generic associated type.
//!
//! The Reference, "Associated Items", "Associated Types": "Associated types may include generic
//! parameters and where clauses; these are often referred to as generic associated types, or
//! GATs", and its example `type Pointer<T>: Deref<Target = T>;` used as `<Self as
//! PointerFamily>::Pointer<T>`. The Reference, "Paths", "Qualified paths": "Fully qualified
//! paths allow for disambiguating the path for trait implementations and for specifying
//! canonical paths"; the last segment of `<S as Tr>::Out<u16>` carries the associated type's
//! own arguments.
//!
//! A projection's portable form lists the trait's arguments, `Self` first, then the associated
//! type's own (`world::AssocGenerics`): `<S as Tr>::Out<u16>` is `Alias { trait_: Tr, name:
//! Out, args: [S, u16] }`. An impl's `type Out<T> = (T, u8);` is a type over the impl's
//! generics then `T`, so the solver instantiates it with the impl's arguments and the
//! projection's own (blaze-solve alias.rs `step_impl`): a list of the wrong length would
//! name a parameter that is not there, so the count is checked where the type is lowered,
//! and a wrong one is E0107 (`Because::ArgCount`), the type the error type.

use crate::diag::{About, At, Because, DiagCode};
use crate::error::Stop;
use crate::ids::{ItemKey, Name};
use crate::infer::assoc_def;
use crate::machine::Machine;
use crate::ty::{Cause, TypeCtx};
use crate::world::World;

impl<W: World, C: TypeCtx> Machine<'_, '_, '_, '_, W, C> {
    /// Whether the projection `<_ as trait_<..>>::name<..>`, written with `written` arguments
    /// in all (`Self`, the trait's, the associated type's own), has the count its declaration
    /// says. A wrong count is reported at `cause` as E0107 naming the associated type, with
    /// the own counts (declared, written) when the trait's part is whole, the totals when it
    /// is not; the caller then makes the error type instead of a projection goal.
    ///
    /// A world that keeps no count ([`World::assoc_generics`]'s default) is not checked.
    pub(crate) fn alias_arity(&mut self, trait_: ItemKey, name: Name, written: u32, cause: Cause) -> Result<bool, Stop> {
        let Some(g) = self.reads.assoc_generics(trait_, name)? else { return Ok(true) };
        let declared = g.parent.saturating_add(g.own);
        if written == declared {
            return Ok(true);
        }
        let at = At::of(cause);
        let (expected, got) = match written.checked_sub(g.parent) {
            Some(own) => (g.own, own),
            None => (declared, written),
        };
        self.diag(DiagCode::GenericArgCount, at, About::name(name));
        if let Some(d) = self.diags.last_mut() {
            d.because = Some(Because::ArgCount { item: ItemKey(assoc_def(trait_, name)), expected, got });
        }
        Ok(false)
    }
}
