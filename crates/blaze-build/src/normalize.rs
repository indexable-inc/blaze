//! Associated types at an instance: every type the build lowers at an instance passes through
//! [`crate::instance::ground_with`], which substitutes the instance's arguments and then, here,
//! replaces each projection `<A as Trait<..>>::Name` by the type the front end's solver
//! normalizes it to ([`LayoutFacts::normalize`]).
//!
//! The Reference, "Associated Items", "Associated types": "An associated type declaration
//! declares a signature for associated type definitions"; "Paths", "Qualified paths": "`<S as
//! T1>::f()` ... Fully qualified paths allow for disambiguating the path for trait
//! implementations". At an instance every argument is concrete, so a projection names exactly
//! one impl's definition: the solver answers it, and that answer may itself be a projection
//! (`type Item = <I as Iterator>::Item`), which is normalized again. That is a work list over
//! rounds of the ground table, never a recursion, bounded by [`ROUNDS`].

use blaze_own::{Alloc, Vec};
use blaze_typeck::{List, PNode, PTy, PortableTys};

use crate::BuildError;
use crate::layout::LayoutFacts;

/// How many rounds of answers a type may need before it holds no projection. A chain of
/// definitions each naming the next projection settles in one round per link; a longer chain is
/// refused (rule 18: a count, never a hang).
pub(crate) const ROUNDS: u32 = 64;

fn full<E>(_: E) -> BuildError {
    BuildError::Malformed { what: "the ground table is full" }
}

fn past<E>(_: E) -> BuildError {
    BuildError::Malformed { what: "a type index past the ground table" }
}

/// Ground type `top` with every projection it reaches normalized by `facts`. A projection the
/// solver does not answer is refused naming it ([`BuildError::Unnormalized`]); one whose
/// answers do not settle within [`ROUNDS`] is refused naming it too.
pub(crate) fn normalize_ground(
    ground: &mut PortableTys<'_>,
    top: PTy,
    facts: &dyn LayoutFacts,
    scratch: Alloc<'_>,
) -> Result<PTy, BuildError> {
    let mut top = top;
    let mut reached: Vec<bool, Alloc<'_>> = Vec::new_in(scratch);
    let mut mapped: Vec<PTy, Alloc<'_>> = Vec::new_in(scratch);
    // Whether `mapped[at]` may still hold a projection (a solver's answer, or a node over one):
    // a projection whose arguments are dirty waits for the next round, so the solver is only
    // ever asked with concrete arguments.
    let mut dirty: Vec<bool, Alloc<'_>> = Vec::new_in(scratch);
    let mut children: Vec<PTy, Alloc<'_>> = Vec::new_in(scratch);
    let mut list: Vec<PTy, Alloc<'_>> = Vec::new_in(scratch);
    let mut last = None;
    for _ in 0..ROUNDS {
        let n = top.0 as usize + 1;
        reached.clear();
        reached.resize(n, false);
        reached[n - 1] = true;
        for at in (0..n).rev() {
            if !reached[at] {
                continue;
            }
            children.clear();
            crate::instance::each_child(ground.view(), ground.view().node(PTy(at as u32)).map_err(past)?, &mut children)?;
            for c in children.iter() {
                *reached.get_mut(c.0 as usize).ok_or(BuildError::Malformed { what: "a child after its parent" })? = true;
            }
        }
        mapped.clear();
        mapped.extend((0..n as u32).map(PTy));
        dirty.clear();
        dirty.resize(n, false);
        let mut met = false;
        for at in 0..n {
            if !reached[at] {
                continue;
            }
            let node = ground.view().node(PTy(at as u32)).map_err(past)?;
            children.clear();
            crate::instance::each_child(ground.view(), node, &mut children)?;
            let changed = children.iter().any(|c| mapped[c.0 as usize] != *c);
            let child_dirty = children.iter().any(|c| dirty[c.0 as usize]);
            if let PNode::Alias { trait_, name, args } = node {
                met = true;
                last = Some((trait_, name));
                let args = if changed { remap_list(ground, args, &mapped, &mut list)? } else { args };
                if child_dirty {
                    mapped[at] = ground.intern(PNode::Alias { trait_, name, args }).map_err(full)?;
                } else {
                    list.clear();
                    list.extend_from_slice(ground.view().list(args).map_err(past)?);
                    mapped[at] = facts.normalize(trait_, name, &list, ground)?;
                }
                dirty[at] = true;
                continue;
            }
            dirty[at] = child_dirty;
            if !changed {
                continue;
            }
            let m = |t: PTy| mapped[t.0 as usize];
            let new = match node {
                PNode::Tuple(l) => PNode::Tuple(remap_list(ground, l, &mapped, &mut list)?),
                PNode::Record(s, l) => PNode::Record(s, remap_list(ground, l, &mapped, &mut list)?),
                PNode::Union(l) => PNode::Union(remap_list(ground, l, &mapped, &mut list)?),
                PNode::Adt(k, l) => PNode::Adt(k, remap_list(ground, l, &mapped, &mut list)?),
                PNode::FnDef(k, l) => PNode::FnDef(k, remap_list(ground, l, &mapped, &mut list)?),
                PNode::FnPtr(l, h) => PNode::FnPtr(remap_list(ground, l, &mapped, &mut list)?, h),
                PNode::Closure { owner, ordinal, args } => {
                    PNode::Closure { owner, ordinal, args: remap_list(ground, args, &mapped, &mut list)? }
                }
                PNode::Coroutine { owner, ordinal, args } => {
                    PNode::Coroutine { owner, ordinal, args: remap_list(ground, args, &mapped, &mut list)? }
                }
                PNode::Opaque(k, l) =>PNode::Opaque(k, remap_list(ground, l, &mapped, &mut list)?),
                PNode::Dynamic(l) => PNode::Dynamic(remap_list(ground, l, &mapped, &mut list)?),
                PNode::ExTrait(k, l) => PNode::ExTrait(k, remap_list(ground, l, &mapped, &mut list)?),
                PNode::ExProj { trait_, name, args, term } => {
                    PNode::ExProj { trait_, name, args: remap_list(ground, args, &mapped, &mut list)?, term: m(term) }
                }
                PNode::Pat(t, l) => PNode::Pat(m(t), l),
                PNode::Ref(mu, t) => PNode::Ref(mu, m(t)),
                PNode::Ptr(mu, t) => PNode::Ptr(mu, m(t)),
                PNode::Slice(t) => PNode::Slice(m(t)),
                PNode::Array(t, len) => PNode::Array(m(t), m(len)),
                PNode::ConstValue { ty, bits } => PNode::ConstValue { ty: m(ty), bits },
                _ => return Err(BuildError::UnsupportedSig { what: "a projection inside a closure's or a future's type" }),
            };
            mapped[at] = ground.intern(new).map_err(full)?;
        }
        if !met {
            return Ok(top);
        }
        top = mapped[n - 1];
    }
    let (trait_, name) = last.ok_or(BuildError::Malformed { what: "a projection round with no projection" })?;
    Err(BuildError::Projection { trait_, name })
}

/// List `l` of `ground`, each element replaced by its `mapped` form, interned.
fn remap_list(ground: &mut PortableTys<'_>, l: List, mapped: &[PTy], list: &mut Vec<PTy, Alloc<'_>>) -> Result<List, BuildError> {
    list.clear();
    for t in ground.view().list(l).map_err(past)? {
        list.push(*mapped.get(t.0 as usize).ok_or(BuildError::Malformed { what: "a child after its parent" })?);
    }
    ground.intern_list(list).map_err(full)
}
