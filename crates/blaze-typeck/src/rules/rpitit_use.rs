//! A trait fn's `impl Trait` (an `async fn`'s future in a trait) at a call whose `Self` is
//! concrete: the type the impl's fn returns, as Rust gives it.
//!
//! The rule (RFC 3425, "Return position `impl Trait` in traits"): each return-position `impl
//! Trait` of a trait fn is an associated type of the trait, `<Self as Tr>::{synthetic}`, whose
//! value in an impl is the impl fn's return type at that place. A call through a generic `T:
//! Tr` sees the projection rigid (the trait's bounds only); a call on a concrete type sees the
//! impl's type. rustc documents it with the lint `refining_impl_trait_reachable` ("Callers of
//! methods for types where the implementation is known are able to observe the types written in
//! the impl signature"), whose example compiles `let _x: &str = "".as_display();` for `fn
//! as_display(&self) -> impl Display` implemented as `fn as_display(&self) -> Self` on `&str`.
//!
//! Here a trait fn's signature keeps the opaque `Opaque(o, ident)` (front2 rpitit.rs: the world's
//! hidden type of `o` is the projection, which auto traits read). At a call that instantiates
//! the signature at a concrete `Self`, [`Machine::rpitit_rows`] names each such opaque with its
//! trait and synthetic name (the trait's `TraitDef::assoc_types` row whose item is `o`), and
//! `lower` makes it the projection (machine.rs `lowered_projection`): a fresh variable that the
//! impl's associated-type row normalizes. Before, the opaque stayed rigid and `let a: [u8; 3] =
//! S.k::<3>()` was E0308 (tests/front2_reveal_trait_fn.rs `const_own`).
//!
//! `Self` is concrete when it is not a parameter, an alias, an opaque, a trait object, an error
//! or an inference variable. A generic receiver keeps the opaque, whose own bounds answer as
//! before (blaze-solve structural.rs `assemble_auto`). NOT YET: a `Self` still an inference
//! variable when the signature is instantiated (`K::k(&s)`, `Self` found from the argument
//! later) keeps the opaque too: rustc sees the impl's type there; here it is the opaque, so a
//! use that needs the impl's type is E0308 (a deferred obligation that projects once `Self` is
//! known would close it).

use blaze_own::{Alloc, Vec};

use crate::error::Stop;
use crate::ids::{ItemKey, Name};
use crate::machine::Machine;
use crate::portable::{PNode, PTy, PTys};
use crate::ty::{TyList, TyView, TypeCtx};
use crate::world::World;

/// The most type nodes one walk of a signature's output visits (rule 18): a signature's
/// output is far smaller.
const MOST_NODES: usize = 1 << 16;

impl<'a, W: World, C: TypeCtx> Machine<'_, '_, '_, 'a, W, C> {
    /// The rows [`Machine::rpitit`] holds while trait fn `k`'s output `output` (of `types`) is
    /// lowered at `args`: (opaque, trait, synthetic name) per `impl Trait` of the output that
    /// is one of the trait's synthetic associated types, when `Self` (`args[0]`) is concrete.
    /// Empty (asking the world nothing) when the output names no opaque or `Self` is not
    /// concrete; empty when `k` is no trait's fn.
    pub(crate) fn rpitit_rows(&mut self, k: ItemKey, types: PTys<'_>, output: PTy, args: TyList) -> Result<Vec<(ItemKey, ItemKey, Name), Alloc<'a>>, Stop> {
        let mut rows: Vec<(ItemKey, ItemKey, Name), Alloc<'a>> = Vec::new_in(self.alloc);
        let mut opaques: Vec<ItemKey, Alloc<'a>> = Vec::new_in(self.alloc);
        let mut work: Vec<PTy, Alloc<'a>> = Vec::new_in(self.alloc);
        work.push(output);
        let mut seen = 0usize;
        while let Some(t) = work.pop() {
            seen += 1;
            if seen > MOST_NODES {
                let limit = crate::error::Limit::LowerSteps { owner: self.body.owner, node: output, steps: seen, limit: MOST_NODES };
                return Err(crate::error::CheckError::Limit(limit).into());
            }
            let node = types.node(t)?;
            if let PNode::Opaque(o, _) = node
                && !opaques.contains(&o)
            {
                opaques.push(o);
            }
            types.children_of(node, &mut work)?;
        }
        if opaques.is_empty() || self.ty.list_len(args) == 0 {
            return Ok(rows);
        }
        let self_ty = self.ty.list_get(args, 0);
        let self_ty = self.ty.shallow_resolve(self_ty);
        let concrete = !matches!(
            self.ty.view(self_ty),
            TyView::Param(_)
                | TyView::Alias { .. }
                | TyView::Opaque(..)
                | TyView::Infer(_)
                | TyView::Dynamic(_)
                | TyView::ExTrait(..)
                | TyView::ExProj { .. }
                | TyView::ExAuto(_)
                | TyView::Error
        );
        if !concrete {
            return Ok(rows);
        }
        let Some(parent) = self.reads.assoc_parent(k)? else { return Ok(rows) };
        let Some(def) = self.reads.trait_def(parent.container)? else { return Ok(rows) };
        for a in def.assoc_types.iter().filter(|a| opaques.contains(&a.item)) {
            rows.push((a.item, parent.container, a.name));
        }
        Ok(rows)
    }
}
