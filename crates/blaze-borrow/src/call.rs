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

use blaze_own::Vec;
use blaze_typeck::ItemKey;

use crate::inst::{Arg, Callee, Clause, EnvId, EnvPools, Envs, GenericArgs, Side, SmallBuf, instantiate_into_buf};
use crate::relate::{Cx, Kind, run_call};
use crate::{Bad, Refused};

/// Lifetime-free reusable scratch buffers for [`call_in`], retaining pool capacities across calls
/// so repeated calls in one body do not re-allocate environments, region slots, late-bound
/// regions, or seen-impl sets in a bump frame.
pub struct CallScratch<A: Allocator> {
    pools: Option<EnvPools<A>>,
    impl_seen: Vec<(ItemKey, EnvId), A>,
    alloc: A,
}

impl<A: Allocator + Copy> CallScratch<A> {
    pub fn new_in(alloc: A) -> CallScratch<A> {
        CallScratch {
            pools: Some(EnvPools::new_in(alloc)),
            impl_seen: Vec::new_in(alloc),
            alloc,
        }
    }

    pub fn clear(&mut self) {
        if let Some(pools) = &mut self.pools {
            pools.clear();
        }
        self.impl_seen.clear();
    }
}

/// [`call`], reusing `scratch`'s allocations across calls.
pub fn call_in<'c, 'k: 'c, A: Allocator + Copy>(
    cx: &mut Cx<'k, '_>,
    callee: Callee<'c>,
    generics: GenericArgs<'_>,
    args: &[Arg],
    dest: Arg,
    scratch: &mut CallScratch<A>,
) -> Result<(), Refused> {
    let item = callee.item();
    let alloc = scratch.alloc;
    let pools = scratch.pools.take().unwrap_or_else(|| EnvPools::new_in(alloc));
    let mut envs = Envs::from_pools(pools, alloc);
    let mut inputs: SmallBuf<Side<'c>, 8, A> = SmallBuf::new_in(alloc);
    let mut clauses: SmallBuf<Clause<'c>, 8, A> = SmallBuf::new_in(alloc);
    let res = (|| -> Result<(), Refused> {
        let output = {
            let out = &mut *cx.out;
            instantiate_into_buf(
                callee,
                generics,
                &mut |_| out.fresh(Kind::Existential),
                &mut envs,
                &mut inputs,
                &mut clauses,
            )?
        };
        if args.len() != inputs.len() {
            let want = u32::try_from(inputs.len()).unwrap_or(u32::MAX);
            let got = u32::try_from(args.len()).unwrap_or(u32::MAX);
            return Err(Refused::Malformed { item, node: dest.ty, what: Bad::ValueArgs { want, got } });
        }
        run_call(
            cx,
            item,
            args,
            inputs.as_slice(),
            output,
            dest,
            clauses.as_slice(),
            &mut envs,
            &mut scratch.impl_seen,
        )
    })();
    scratch.pools = Some(envs.into_pools());
    res
}

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
    let mut scratch = CallScratch::new_in(alloc);
    call_in(cx, callee, generics, args, dest, &mut scratch)
}
