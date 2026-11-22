//! Calls in the reference evaluator (docs/v2/plan/design/backend-next.md, "Evidence, gap and
//! pieces", C1): a direct `call` whose callee [`Bodies`] gives is run, so a function with a
//! callee inlined into it can be compared with itself before the inlining.
//!
//! The run keeps a stack of the callers waiting on a call, never Rust's own stack: a call
//! saves the caller's view, values and slot base in a [`Frame`], opens the callee's stack
//! slots at fresh ids of the run's one memory (eval/memory.rs) and starts the callee's entry
//! block; the callee's `return` closes its slots, puts the caller back and hands its values
//! to the call: one as it is, several as a `Val::Tuple` that `project` reads. A pointer passes
//! into and out of a call as any value does; one into a returned callee's slot is undefined
//! behaviour at its next access. Fuel is shared by every frame. At most [`MAX_DEPTH`] frames
//! run at once; a deeper call is [`EvalError::Depth`]. A trap, undefined behaviour or running
//! out of fuel in a callee ends the whole run with that outcome (an `Undefined` names the
//! callee's instruction).
//!
//! A `call_indirect` through a function's pointer enters its callee here too (`enter`;
//! eval/data.rs). Refused by name, in this version: `invoke`, the tail calls (the step's
//! "this operation"); a `call` whose callee `bodies` does not give (an outside function), or
//! that names no function.

use alloc::vec::Vec;
use core::alloc::Allocator;

use super::{EvalError, Memory, Outcome, Run, Val};
use crate::{
    Block, Op, View,
    func::{RefId, RefKind},
};

/// Moved to [`crate::body`] (one home: the inliner runs in a build, the evaluator only in tests).
pub use crate::body::{Bodies, BodyId};

/// The bodies of a run that follows no call: [`super::run_in`]'s.
pub(super) struct NoBodies;

impl Bodies for NoBodies {
    fn callee(&self, _from: BodyId, _reference: RefId) -> Option<(BodyId, View<'_>)> {
        None
    }
}

/// The most frames a run holds at once, the root's included.
pub(super) const MAX_DEPTH: usize = 64;

/// A caller waiting on its call: what the run puts back when the callee returns.
pub(super) struct Frame<'a, A: Allocator> {
    body: BodyId,
    view: View<'a>,
    values: Vec<Option<Val>, A>,
    /// The id of its slot 0 in the run's memory.
    base: u32,
    /// The `call` instruction it waits at; the run goes on at the next.
    call: usize,
}

/// Where a run goes when a body's terminator ends it.
pub(super) enum Resume<A: Allocator> {
    /// A caller's instruction: the one after its call.
    At(usize),
    /// The run is over.
    Done(Outcome<A>),
}

/// Runs `view`, the body `root`, from its entry block with `args` for its parameters, at most
/// `fuel` instructions over every frame, following each direct `call` whose callee `bodies`
/// gives, every buffer in `alloc`. Each body must be verified (`crate::verify`).
pub fn run_calls_in<A: Allocator + Clone>(
    view: View<'_>,
    root: BodyId,
    args: &[Val],
    fuel: u64,
    bodies: &dyn Bodies,
    alloc: A,
) -> Result<Outcome<A>, EvalError> {
    let values = fresh(view, &alloc);
    let mut memory = Memory::new_in(alloc.clone());
    let Some(base) = memory.open(view) else {
        return Err(EvalError::Unsupported { inst: 0, op: view.op(0), what: "more stack slots than a run numbers" });
    };
    let frames = Vec::with_capacity_in(0, alloc.clone());
    let tuples = Vec::new_in(alloc.clone());
    let mut run = Run { view, values, memory, base, tuples, alloc, body: root, frames, bodies };
    let entry = view.params(Block(0));
    if entry.len() != args.len() {
        return Err(EvalError::Args { want: entry.len(), given: args.len() });
    }
    for (inst, &arg) in entry.zip(args) {
        run.values[inst] = Some(arg);
    }
    run.go(Block(0), fuel)
}

/// A body's values, none set. Its stack slots are opened in the run's memory.
fn fresh<A: Allocator + Clone>(view: View<'_>, alloc: &A) -> Vec<Option<Val>, A> {
    let mut values = Vec::with_capacity_in(view.len(), alloc.clone());
    values.resize(view.len(), None);
    values
}

impl<'a, A: Allocator + Clone> Run<'a, A> {
    /// The direct call `inst`: the caller waits in a frame and the callee's entry block takes
    /// the arguments. Returns the callee's first instruction.
    pub(super) fn call(&mut self, inst: usize) -> Result<usize, EvalError> {
        let op = Op::Call;
        let mut o = self.view.operands(inst);
        let reference = o.reference();
        let _sig = o.sig();
        let args = o.values();
        if self.view.reference(reference).kind != RefKind::Func as u32 {
            return Err(EvalError::Unsupported { inst, op, what: "a call of a reference that is not a function" });
        }
        let bodies = self.bodies;
        let Some((callee, view)) = bodies.callee(self.body, reference) else {
            return Err(EvalError::Unsupported { inst, op, what: "a call whose callee the run's bodies do not give" });
        };
        self.enter_call(inst, op, callee, view, args)
    }

    /// Call `inst` (`op`, direct or indirect) of `view`, the body `callee`, with `args`: the
    /// caller waits in a frame and the callee's entry block takes the arguments. Returns the
    /// callee's first instruction.
    pub(super) fn enter_call(
        &mut self,
        inst: usize,
        op: Op,
        callee: BodyId,
        view: View<'a>,
        args: crate::op::Values<'_>,
    ) -> Result<usize, EvalError> {
        // The running frame is one more than those waiting; the callee would be one more again.
        if self.frames.len() + 2 > MAX_DEPTH {
            return Err(EvalError::Depth { inst });
        }
        let entry = view.params(Block(0));
        let first = entry.end;
        if entry.len() != args.len() {
            return Err(EvalError::Unsupported {
                inst,
                op,
                what: "a call passing another number of arguments than its callee's entry takes",
            });
        }
        let mut values = fresh(view, &self.alloc);
        for (param, value) in entry.zip(args.iter()) {
            values[param] = Some(self.get(inst, value)?);
        }
        let Some(base) = self.memory.open(view) else {
            return Err(EvalError::Unsupported { inst, op, what: "more stack slots over one run than it numbers" });
        };
        let caller = Frame {
            body: self.body,
            view: self.view,
            values: core::mem::replace(&mut self.values, values),
            base: core::mem::replace(&mut self.base, base),
            call: inst,
        };
        self.frames.push(caller);
        self.body = callee;
        self.view = view;
        Ok(first)
    }

    /// The running body ended with `outcome`: a return goes back to the caller waiting, if
    /// any, with its value; anything else, or the root's return, ends the run.
    pub(super) fn resume(&mut self, outcome: Outcome<A>) -> Result<Resume<A>, EvalError> {
        let returned = match outcome {
            Outcome::Returned(returned) => returned,
            other => return Ok(Resume::Done(other)),
        };
        let Some(caller) = self.frames.pop() else { return Ok(Resume::Done(Outcome::Returned(returned))) };
        let Frame { body, view, values, base, call } = caller;
        self.memory.close(self.base);
        self.body = body;
        self.view = view;
        self.values = values;
        self.base = base;
        let op = self.view.op(call);
        let mut o = self.view.operands(call);
        // A direct call's callee is a reference; an indirect call's a value (eval/data.rs).
        match op {
            Op::CallIndirect => {
                let _callee = o.value();
            }
            _ => {
                let _reference = o.reference();
            }
        }
        let want = self.view.sig(o.sig()).rets.len();
        if returned.len() != want {
            return Err(EvalError::Unsupported {
                inst: call,
                op,
                what: "a callee returning another number of values than its call's signature",
            });
        }
        match returned[..] {
            [] => {}
            [value] => self.values[call] = Some(value),
            _ => {
                let Ok(at) = u32::try_from(self.tuples.len()) else {
                    return Err(EvalError::Unsupported { inst: call, op, what: "more call values over one run than it numbers" });
                };
                self.tuples.extend_from_slice(&returned);
                self.values[call] = Some(Val::Tuple { at, len: returned.len() as u32 });
            }
        }
        Ok(Resume::At(call + 1))
    }
}
