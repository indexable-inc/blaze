//! THE ONE CALL RULE (end-state.md D156): every call, of every [`Callee`] kind, is
//!
//! 1. the callee's signature instantiated at the call ([`instantiate`]: early-bound lifetimes
//!    from the call's generic argument slots, late-bound ones and the instantiated binder's
//!    fresh, existential origins of the caller);
//! 2. each argument related to its input, covariantly (`arg <: input`);
//! 3. the output related to the destination, covariantly (`output <: dest`);
//! 4. the callee's clauses turned into outlives (relate.rs `clauses`);
//! 5. each instantiated input and the output well-formed at the call (relate.rs
//!    `well_formed`: the instance of `&'a &'b T` makes `'b: 'a` hold, the implied bound the
//!    callee was promised).
//!
//! Each alias meets normalization through the selected impl during 2 and 3. Nothing here
//! knows what kind of callee it has beyond what [`instantiate`] reads: there are no per-kind
//! paths (today's nine in blaze-flow-lower lib.rs `call_done` are what this replaces).
#![blaze::sans_io]

use core::alloc::Allocator;

use crate::inst::{Arg, Callee, GenericArgs, Side, instantiate};
use crate::relate::{Cx, Kind, clauses, relate_sides, well_formed};
use crate::{Bad, Refused, Variance};

/// The call of `callee` with `generics`, the values `args` (in input order: a method's
/// receiver first) and the destination `dest`; its constraints go to `cx.out`. The caller has
/// pushed the call's statement to the flow first (the constraints hold at its Mid).
pub fn call<'c, 'k: 'c, A: Allocator + Copy>(
    cx: &mut Cx<'k, '_>,
    callee: Callee<'c>,
    generics: GenericArgs<'_>,
    args: &[Arg],
    dest: Arg,
    alloc: A,
) -> Result<(), Refused> {
    let item = callee.item();
    let mut inst = {
        let out = &mut *cx.out;
        instantiate(callee, generics, &mut |_| out.fresh(Kind::Existential), alloc)?
    };
    if args.len() != inst.inputs.len() {
        let want = u32::try_from(inst.inputs.len()).unwrap_or(u32::MAX);
        let got = u32::try_from(args.len()).unwrap_or(u32::MAX);
        return Err(Refused::Malformed { item, node: dest.ty, what: Bad::ValueArgs { want, got } });
    }
    for (a, &input) in args.iter().zip(inst.inputs.iter()) {
        relate_sides(cx, &mut inst.envs, item, Side::caller(*a), input, Variance::Co)?;
    }
    let output = inst.output;
    relate_sides(cx, &mut inst.envs, item, output, Side::caller(dest), Variance::Co)?;
    clauses(cx, &mut inst)?;
    well_formed(cx, &mut inst)
}
