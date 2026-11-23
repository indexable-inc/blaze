//! A reference evaluator of LIR's integer core, for tests: what every backend's code for a
//! function must do, by LIR's stated semantics (the crate's "Semantics": every operation
//! without flags is defined as Cranelift's are, a shift masks its amount, division by zero and
//! `sdiv` of `MIN` by -1 trap). Backends are compared against it on random programs
//! (docs/v2/plan/design/backend-next.md, (e)); it is never const eval and never runs in a
//! build.
//!
//! It models scalar integers `i1` to `i128`, their arithmetic, bitwise, shift, rotate,
//! min/max, saturating and overflow operations, `project` of an overflow pair or of a call's
//! values, `icmp` (of pointers where their slots decide it), `select`, the conversions and
//! `freeze`; `f32` and `f64` (eval/floats.rs), `f16` and `f128` arithmetic, comparisons and
//! float conversions by soft IEEE (eval/float.rs); fixed vectors of at most 128 bits, lane-wise
//! and their lane operations (eval/vector.rs); `null`, `uninit`, `assume` and the pointer
//! operations; memory as one run-wide store of bytes with provenance holding every frame's
//! stack slots (eval/memory.rs): `slot_addr`, `ptr_add`, `load` and `store` of any scalar,
//! `memcpy`, `memmove`, `memset`, `memcmp`, `slot_live` and `slot_dead`; direct calls
//! (eval/calls.rs), pointers passed and returned; and `jump`, `br`, `switch`, `return`,
//! `trap` and `unreachable`; of the flags, `ptr_add inbounds`, its promise checked
//! (eval/promises.rs); the program's data and functions (eval/data.rs): `data_addr`,
//! `tls_addr` and `func_addr`, placed once per run, and `call_indirect`. Anything else (a
//! vector wider than 128 bits, an atomic, an outside function, an instruction with other flags)
//! is refused with
//! [`EvalError::Unsupported`], never guessed. Undefined behaviour is
//! an outcome ([`Outcome::Undefined`]): a comparison of two backends says nothing of a case
//! with one. Fuel bounds every run.

use alloc::vec::Vec;
use alloc::alloc::Allocator;

pub use crate::arith::Trap;
use crate::{
    Block, Flags, Op, Value, View,
    arith::{binary, compare, convert, iconst_value, overflow, sext, unary, wide_value},
    op::Target,
};

mod calls;
pub use calls::{Bodies, BodyId, run_calls_in};
#[cfg(test)]
mod calls_tests;
mod promises;
#[cfg(test)]
mod promises_tests;
mod floats;
pub use floats::agree;
#[cfg(test)]
mod floats_tests;
mod float;
#[cfg(test)]
mod float_tests;
mod vector;
mod memory;
use memory::Memory;
mod target;
#[cfg(test)]
mod memory_tests;
mod data;
pub use crate::body::{Datum, DatumId, DatumReloc};
#[cfg(test)]
mod data_tests;

/// A value as the evaluator holds it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Val {
    /// An integer of its type's width, its bits above the width zero; or a float's bits.
    Int(u128),
    /// A pointer `offset` bytes from the start of stack slot `slot`: a run-wide id, the slot's
    /// index in its frame plus the frame's base (eval/memory.rs), so a pointer means the same
    /// in every frame (the root frame's base is 0). [`Val::NO_SLOT`] for a pointer of no
    /// provenance, whose `offset` is its address.
    Ptr { slot: u32, offset: i128 },
    /// An overflow operation's `{T, i1}`.
    Pair(u128, bool),
    /// `uninit`: bits LIR leaves open. Stored, it leaves the bytes uninitialized; any use but
    /// passing it on is refused.
    Uninit,
    /// A call's several values: `len` of the run's `tuples` from `at`, which `project` reads.
    Tuple { at: u32, len: u32 },
}

/// How a run ended.
#[derive(Debug)]
pub enum Outcome<A: Allocator> {
    Returned(Vec<Val, A>),
    Trapped(Trap),
    /// Undefined behaviour at instruction `inst`: what any backend may do anything for.
    Undefined { inst: usize, why: &'static str },
    /// The run took more than its fuel of instructions.
    OutOfFuel,
}

impl<A: Allocator, B: Allocator> PartialEq<Outcome<B>> for Outcome<A> {
    fn eq(&self, other: &Outcome<B>) -> bool {
        match (self, other) {
            (Outcome::Returned(a), Outcome::Returned(b)) => a[..] == b[..],
            (Outcome::Trapped(a), Outcome::Trapped(b)) => a == b,
            (Outcome::Undefined { inst: a, why: x }, Outcome::Undefined { inst: b, why: y }) => {
                a == b && x == y
            }
            (Outcome::OutOfFuel, Outcome::OutOfFuel) => true,
            _ => false,
        }
    }
}

/// What the evaluator does not model, or a function it cannot run as given.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EvalError {
    /// Instruction `inst`'s operation, or this use of it, is outside the modeled core.
    Unsupported { inst: usize, op: Op, what: &'static str },
    /// The entry block takes `want` parameters and the run was given `given`.
    Args { want: usize, given: usize },
    /// A value read before anything set it: a function the verifier would refuse.
    Unset { inst: usize, value: Value },
    /// Call instruction `inst` would open more frames than a run holds (`calls::MAX_DEPTH`).
    Depth { inst: usize },
}

impl core::fmt::Display for EvalError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            EvalError::Unsupported { inst, op, what } => {
                write!(f, "instruction {inst} (`{}`): the evaluator does not model {what}", op.name())
            }
            EvalError::Args { want, given } => {
                write!(f, "the entry block takes {want} parameters and the run gave {given}")
            }
            EvalError::Unset { inst, value } => {
                write!(f, "instruction {inst} reads {value}, which nothing set: an unverified function")
            }
            EvalError::Depth { inst } => {
                write!(f, "instruction {inst} calls past the evaluator's bound of {} frames", calls::MAX_DEPTH)
            }
        }
    }
}

impl core::error::Error for EvalError {}

/// One run of a function and the functions it calls: the running body's view and values,
/// every frame's slots, and the callers waiting on a call.
struct Run<'a, A: Allocator + Clone> {
    view: View<'a>,
    values: Vec<Option<Val>, A>,
    /// Every open frame's stack slots (eval/memory.rs).
    memory: Memory<A>,
    /// The id of the running frame's slot 0 in `memory`.
    base: u32,
    /// The values of the calls that returned several, which a `Val::Tuple` names; kept for
    /// the run, bounded by its fuel.
    tuples: Vec<Val, A>,
    alloc: A,
    /// The running body, as `bodies` names it.
    body: BodyId,
    /// The callers waiting on a call, the innermost last: fewer than `calls::MAX_DEPTH`.
    frames: Vec<calls::Frame<'a, A>, A>,
    /// What a direct `call` runs.
    bodies: &'a dyn Bodies,
}

/// Runs `view` from its entry block with `args` for its parameters, at most `fuel`
/// instructions, every buffer in `alloc`. `view` must be verified (`crate::verify`). It
/// follows no call: [`run_calls_in`] with bodies that give none.
pub fn run_in<A: Allocator + Clone>(
    view: View<'_>,
    args: &[Val],
    fuel: u64,
    alloc: A,
) -> Result<Outcome<A>, EvalError> {
    run_calls_in(view, BodyId([0; 32]), args, fuel, &calls::NoBodies, alloc)
}

/// Where a block's terminator sends the run.
enum Next<A: Allocator> {
    To(Block),
    Done(Outcome<A>),
}

impl<A: Allocator + Clone> Run<'_, A> {
    fn get(&self, inst: usize, value: Value) -> Result<Val, EvalError> {
        self.values[value.index()].ok_or(EvalError::Unset { inst, value })
    }

    fn int(&self, inst: usize, value: Value) -> Result<(u128, u32), EvalError> {
        let op = self.view.op(inst);
        let ty = self.view.ty(value);
        if !ty.is_scalar() || !ty.is_int() {
            return Err(EvalError::Unsupported { inst, op, what: "an operand that is not a scalar integer" });
        }
        match self.get(inst, value)? {
            Val::Int(it) => Ok((it, ty.bits(self.view.head.ptr_bits))),
            _ => Err(EvalError::Unsupported { inst, op, what: "an integer operand that holds no integer" }),
        }
    }

    /// The width of instruction `inst`'s result, a scalar integer.
    fn width(&self, inst: usize) -> Result<u32, EvalError> {
        let ty = self.view.ty(Value(inst as u32));
        match ty.is_scalar() && ty.is_int() {
            true => Ok(ty.bits(self.view.head.ptr_bits)),
            false => Err(EvalError::Unsupported {
                inst,
                op: self.view.op(inst),
                what: "a result that is not a scalar integer",
            }),
        }
    }

    /// Runs from `block` of the running body until the root returns or the run ends. One
    /// cursor, `inst`, moves through the running body: a direct `call` moves it into the
    /// callee (its caller waits in `frames`), a callee's `return` back after the call.
    fn go(&mut self, block: Block, mut fuel: u64) -> Result<Outcome<A>, EvalError> {
        let mut inst = self.view.params(block).end;
        loop {
            let Some(left) = fuel.checked_sub(1) else { return Ok(Outcome::OutOfFuel) };
            fuel = left;
            let op = self.view.op(inst);
            // A promise it can check is modelled (eval/promises.rs); any other flag is refused.
            if let Some(what) = promises::unmodelled(op, self.view.flags(inst)) {
                return Err(EvalError::Unsupported { inst, op, what });
            }
            if op.is_terminator() {
                inst = match self.end(inst, op)? {
                    Next::To(next) => self.view.params(next).end,
                    Next::Done(outcome) => match self.resume(outcome)? {
                        calls::Resume::At(next) => next,
                        calls::Resume::Done(outcome) => return Ok(outcome),
                    },
                };
                continue;
            }
            if op == Op::Call {
                inst = self.call(inst)?;
                continue;
            }
            if op == Op::CallIndirect {
                match self.call_indirect(inst)? {
                    Ok(next) => inst = next,
                    Err(outcome) => return Ok(outcome),
                }
                continue;
            }
            match self.step(inst, op)? {
                Ok(Some(value)) => self.values[inst] = Some(value),
                Ok(None) => {}
                Err(outcome) => return Ok(outcome),
            }
            inst += 1;
        }
    }

    /// Instruction `inst`, not a terminator: its value, if it has one, or how the run ends.
    fn step(&mut self, inst: usize, op: Op) -> Result<Result<Option<Val>, Outcome<A>>, EvalError> {
        let mut o = self.view.operands(inst);
        let undefined = |why| Ok(Err(Outcome::Undefined { inst, why }));
        // A vector of at most 128 bits, or a lane-wise operation on one (eval/vector.rs); every
        // other instruction goes on below.
        if let Some(value) = self.vector(inst, op)? {
            return Ok(Ok(Some(value)));
        }
        let value = match op {
            Op::Iconst => {
                let width = self.width(inst)?;
                Val::Int(iconst_value(o.imm(), width))
            }
            Op::IconstWide => {
                let width = self.width(inst)?;
                Val::Int(wide_value(o.constant(), width))
            }
            Op::Add | Op::Sub | Op::Mul | Op::Udiv | Op::Sdiv | Op::Urem | Op::Srem | Op::And
            | Op::Or | Op::Xor | Op::Shl | Op::Lshr | Op::Ashr | Op::Rotl | Op::Rotr
            | Op::Umulhi | Op::Smulhi | Op::UaddSat | Op::SaddSat | Op::UsubSat | Op::SsubSat
            | Op::Umin | Op::Umax | Op::Smin | Op::Smax => {
                let width = self.width(inst)?;
                let ((a, _), (b, _)) = (self.int(inst, o.value())?, self.int(inst, o.value())?);
                match binary(op, a, b, width) {
                    Ok(it) => Val::Int(it),
                    Err(trap) => return Ok(Err(Outcome::Trapped(trap))),
                }
            }
            Op::Neg | Op::Not | Op::Clz | Op::Ctz | Op::Popcnt | Op::Bswap | Op::Bitrev => {
                let width = self.width(inst)?;
                Val::Int(unary(op, self.int(inst, o.value())?.0, width))
            }
            Op::UaddOverflow | Op::SaddOverflow | Op::UsubOverflow | Op::SsubOverflow
            | Op::UmulOverflow | Op::SmulOverflow => {
                let ((a, width), (b, _)) = (self.int(inst, o.value())?, self.int(inst, o.value())?);
                let (value, flag) = overflow(op, a, b, width);
                Val::Pair(value, flag)
            }
            Op::Project => match (self.get(inst, o.value())?, o.imm()) {
                (Val::Pair(value, _), 0) => Val::Int(value),
                (Val::Pair(_, flag), 1) => Val::Int(u128::from(flag)),
                (Val::Tuple { at, len }, k) if k < len => self.tuples[at as usize + k as usize],
                _ => {
                    return Err(EvalError::Unsupported {
                        inst,
                        op,
                        what: "a project of other than an overflow pair or a call's values",
                    });
                }
            },
            Op::Icmp => {
                let cond = o.int_cond();
                let (a, b) = (o.value(), o.value());
                match self.view.ty(a).is_ptr() {
                    true => self.ptr_compare(inst, cond, a, b)?,
                    false => {
                        let ((a, width), (b, _)) = (self.int(inst, a)?, self.int(inst, b)?);
                        Val::Int(u128::from(compare(cond, a, b, width)))
                    }
                }
            }
            Op::Select => {
                let (cond, _) = self.int(inst, o.value())?;
                let (a, b) = (self.get(inst, o.value())?, self.get(inst, o.value())?);
                if cond != 0 { a } else { b }
            }
            Op::Zext | Op::Sext | Op::Trunc => {
                let width = self.width(inst)?;
                let (a, from) = self.int(inst, o.value())?;
                Val::Int(convert(op, a, from, width))
            }
            Op::Freeze => match self.get(inst, o.value())? {
                Val::Uninit => {
                    return Err(EvalError::Unsupported { inst, op, what: "a `freeze` of `uninit`, whose bits LIR leaves open" });
                }
                it => it,
            },
            Op::SlotAddr => Val::Ptr { slot: self.base + o.slot().0, offset: 0 },
            Op::SlotLive | Op::SlotDead => {
                self.restart(inst, o.slot(), op == Op::SlotLive)?;
                return Ok(Ok(None));
            }
            Op::PtrAdd => {
                let Val::Ptr { slot, offset } = self.get(inst, o.value())? else {
                    return Err(EvalError::Unsupported { inst, op, what: "a pointer not into a slot" });
                };
                let (by, width) = self.int(inst, o.value())?;
                let by = sext(by, width);
                if !self.view.flags(inst).contains(Flags::INBOUNDS) {
                    Val::Ptr { slot, offset: offset.wrapping_add(by) }
                } else {
                    match self.inbounds(slot, offset, by) {
                        Ok(offset) => Val::Ptr { slot, offset },
                        Err(why) => return undefined(why),
                    }
                }
            }
            Op::Load => match self.load(inst)? {
                Ok(it) => it,
                Err(why) => return undefined(why),
            },
            Op::Store => {
                if let Err(why) = self.store(inst)? {
                    return undefined(why);
                }
                return Ok(Ok(None));
            }
            _ => return self.more(inst, op),
        };
        Ok(Ok(Some(value)))
    }

    /// Block terminator `inst`: where the run goes, or how it ends.
    fn end(&mut self, inst: usize, op: Op) -> Result<Next<A>, EvalError> {
        let mut o = self.view.operands(inst);
        let next = match op {
            Op::Jump => o.target(),
            Op::Br => {
                let (cond, _) = self.int(inst, o.value())?;
                let (then, otherwise) = (o.target(), o.target());
                if cond != 0 { then } else { otherwise }
            }
            Op::Switch => {
                let scrutinee = o.value();
                let (value, _) = self.int(inst, scrutinee)?;
                let default = o.target();
                o.cases(scrutinee)
                    .find(|(words, _)| {
                        words.iter().rev().fold(0u128, |it, &word| (it << 32) | u128::from(word))
                            == value
                    })
                    .map_or(default, |(_, to)| to)
            }
            Op::Return => {
                let values = o.values();
                let mut out = Vec::with_capacity_in(values.len(), self.alloc.clone());
                for value in values.iter() {
                    out.push(self.get(inst, value)?);
                }
                return Ok(Next::Done(Outcome::Returned(out)));
            }
            Op::Trap => return Ok(Next::Done(Outcome::Trapped(Trap::Code(o.imm())))),
            Op::Unreachable => {
                return Ok(Next::Done(Outcome::Undefined { inst, why: "`unreachable` reached" }));
            }
            _ => return Err(EvalError::Unsupported { inst, op, what: "this terminator" }),
        };
        self.enter(inst, next)?;
        Ok(Next::To(next.block))
    }

    /// The target's parameters take its arguments, all read before any is written.
    fn enter(&mut self, inst: usize, to: Target<'_>) -> Result<(), EvalError> {
        let params = self.view.params(to.block);
        if params.len() != to.args.len() {
            return Err(EvalError::Unsupported {
                inst,
                op: self.view.op(inst),
                what: "a target given another number of arguments than its parameters",
            });
        }
        let mut args = Vec::with_capacity_in(to.args.len(), &self.alloc);
        for value in to.args.iter() {
            args.push(self.get(inst, value)?);
        }
        for (param, arg) in params.zip(args) {
            self.values[param] = Some(arg);
        }
        Ok(())
    }
}
