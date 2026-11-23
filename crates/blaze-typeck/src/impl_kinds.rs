//! The kind of each of an impl's generics, read from its header: what the variable that
//! instantiates it at a use must be (walk list-AJ, P1).
//!
//! The rule: the Reference, "Generic parameters": a const parameter's argument is a const
//! ("Const parameters may be used anywhere a const item may be used ... as a const argument"),
//! never a type; "Array types": `[T; N]` takes a const `N`. And "Implementations", "Generic
//! Implementations": "Generic parameters constrain an implementation if the parameter appears
//! at least once in one of: the implemented trait, if it has one; the implementing type", or
//! as an associated type's value when the trait's projection names it. So the header (self
//! type, trait arguments, associated type values) names every constrained parameter, and a
//! const one appears there as `PNode::ConstParam(i)`. A const parameter named only in the
//! where-clauses is unconstrained (E0207) and gets a type variable here: its uses in the
//! where-clauses then fail by the sort check, never silently.
//!
//! This mirrors the solver's `coherence::fresh_for` (blaze-solve), which works over solver
//! terms; the checker's instance path (`instance::unify_header`) needs the same over its
//! portable table.

use blaze_own::{Alloc, Vec};

use crate::error::{CheckError, Limit, Malformed};
use crate::portable::{PNode, PTy};
use crate::ty::VarKind;
use crate::world::ImplHeader;

/// Most nodes walked in one header (rule 18): a header past it is refused by name.
const MAX_STEPS: u32 = 1 << 16;

/// The kind of the variable for each of `h`'s generics, in order: `VarKind::Const` for one
/// the header names as `PNode::ConstParam(i)`, `VarKind::Ty` for the rest.
///
/// A work list over the header's nodes (no recursion): every node with children pushes them,
/// leaves stop. A `ConstParam(i)` with `i` past `h.generics` is malformed: `Malformed::Sort`
/// on that node.
pub(crate) fn impl_var_kinds<'a>(h: &ImplHeader<'_>, alloc: Alloc<'a>) -> Result<Vec<VarKind, Alloc<'a>>, CheckError> {
    let mut kinds: Vec<VarKind, Alloc<'a>> = Vec::with_capacity_in(h.generics as usize, alloc);
    for _ in 0..h.generics {
        kinds.push(VarKind::Ty);
    }
    let mut work: Vec<PTy, Alloc<'a>> = Vec::new_in(alloc);
    work.push(h.self_ty);
    work.extend_from_slice(h.types.list(h.trait_args.list())?);
    // The self type and trait arguments first, then the associated types' values: a value is
    // over the impl's generics THEN the associated type's own (a generic associated type, or
    // a trait fn's `impl Trait` whose fn has its own const parameter), so there a
    // `ConstParam(i)` past the impl's generics is that own parameter, no impl variable.
    let mut from_assoc = false;
    let mut steps: u32 = 0;
    loop {
        let Some(t) = work.pop() else {
            if from_assoc {
                break;
            }
            from_assoc = true;
            for &(_, t) in h.assoc_types {
                work.push(t);
            }
            continue;
        };
        steps += 1;
        if steps > MAX_STEPS {
            return Err(CheckError::Limit(Limit::ImplVarKindSteps { node: t, steps, limit: MAX_STEPS }));
        }
        match h.types.node(t)? {
            PNode::ConstParam(i) => match kinds.get_mut(i as usize) {
                Some(k) => *k = VarKind::Const,
                None if from_assoc => {}
                None => return Err(CheckError::Malformed(Malformed::Sort { node: t.0 })),
            },
            PNode::Tuple(l)
            | PNode::Record(_, l)
            | PNode::Union(l)
            | PNode::Adt(_, l)
            | PNode::FnDef(_, l)
            | PNode::FnPtr(l, _)
            | PNode::Opaque(_, l)
            | PNode::Dynamic(l)
            | PNode::ExTrait(_, l)
            | PNode::Alias { args: l, .. }
            | PNode::Closure { args: l, .. }
            | PNode::Coroutine { args: l, .. } => work.extend_from_slice(h.types.list(l)?),
            PNode::Ref(_, p) | PNode::Ptr(_, p) | PNode::Slice(p) | PNode::ConstValue { ty: p, .. } => work.push(p),
            PNode::Array(e, n) => {
                work.push(e);
                work.push(n);
            }
            PNode::Pat(b, l) => {
                work.push(b);
                work.extend_from_slice(h.types.list(l)?);
            }
            PNode::Future(_, output, interior) => {
                work.push(output);
                work.push(interior);
            }
            PNode::ExProj { args, term, .. } => {
                work.extend_from_slice(h.types.list(args)?);
                work.push(term);
            }
            PNode::Bool
            | PNode::Char
            | PNode::Int(_)
            | PNode::Uint(_)
            | PNode::Float(_)
            | PNode::Str
            | PNode::Never
            | PNode::Param(_)
            | PNode::Error
            | PNode::ExAuto(_)
            | PNode::Foreign(_)
            | PNode::Infer => {}
        }
    }
    Ok(kinds)
}
