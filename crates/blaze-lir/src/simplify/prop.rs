//! Copy and constant propagation with folding (docs/v2/plan/design/backend-next.md, D100
//! point 1: the third pass of the linear simplify stage).
//!
//! Facts, each a true invariant of every run, found over the reached blocks until none is new
//! (each value gains at most one fact, so the passes are at most one more than the values):
//!
//! - a value STANDS FOR another: a block parameter whose every argument, on the edges from
//!   reached blocks, is one value or the parameter itself (the value dominates the block, so
//!   every use may read it); a `select` on a known condition, or with one value on both arms;
//! - a value is a KNOWN integer: `iconst` and `iconst_wide`; an integer operation without
//!   flags whose operands are known, folded by the evaluator's own arithmetic
//!   ([`crate::arith`]: `binary`, `unary`, `overflow`, `compare`, `convert`), so the constant is
//!   what the evaluator gives for every operation and width; a `project` of a known overflow
//!   pair; a `freeze` of a known integer; a block parameter whose every argument is one known
//!   integer.
//!
//! What is not folded, by rule: an operation that traps on its operands (division by zero,
//! `sdiv` of `MIN` by -1) stays, so its trap stays; an instruction with any flags stays (a
//! promise such as `nsw` or `exact` could make the folded value undefined, and the evaluator
//! models no flag); floats, vectors and pointers are not folded.
//!
//! The output ([`super::dce::rebuild`]): every use reads the value its value stands for; an
//! instruction with a known integer, not a parameter, is that `iconst` (or `iconst_wide`) in
//! place. Nothing is removed: what this leaves unused, [`super::dead`] removes.

use core::alloc::Allocator;

use blaze_own::Vec;

use super::{
    SimplifyError,
    dce::{Edit, NONE, filled, incoming, rebuild, reachable},
};
use crate::{
    Block, Flags, Func, Op, Ty, View,
    arith::{binary, compare, convert, iconst_value, overflow, unary, wide_value},
};

/// A planted wrong rewrite, for the pass's control test (rule 53); [`Plant::None`] in every
/// build. Read only under `cfg!(test)`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Plant {
    None,
    /// `sub` of two known integers folded as their sum.
    FoldSubAsAdd,
}

/// What is known of a value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Known {
    No,
    /// A scalar integer, its bits above its width zero.
    Int(u128),
    /// An overflow operation's `{T, i1}`.
    Pair(u128, bool),
}

/// A fact found for one instruction.
enum Fact {
    None,
    Stands(u32),
    Known(Known),
}

/// The function `view` with its copies and constants propagated and its integer operations
/// on constants folded, in `alloc`, verified.
pub fn propagate<A: Allocator + Clone>(view: View<'_>, alloc: A) -> Result<Func<A>, SimplifyError> {
    propagate_planted(view, Plant::None, alloc)
}

/// [`propagate`] with `plant` (a test's control; ignored outside tests).
pub fn propagate_planted<A: Allocator + Clone>(view: View<'_>, plant: Plant, alloc: A) -> Result<Func<A>, SimplifyError> {
    if view.blocks() == 0 {
        return Err(SimplifyError::Refused { inst: 0, what: "a function with no blocks" });
    }
    let len = view.len();
    let no_jumps = filled(len, NONE, &alloc);
    let reached = reachable(&view, &no_jumps, &alloc);
    let into = incoming(&view, &reached, &no_jumps, &alloc);
    let mut facts = Facts {
        view,
        plant: cfg!(test) && plant == Plant::FoldSubAsAdd,
        stands: Vec::with_capacity_in(len, alloc.clone()),
        known: filled(len, Known::No, &alloc),
    };
    for value in 0..len as u32 {
        facts.stands.push(value);
    }
    let mut passes = 0usize;
    loop {
        let mut changed = false;
        for b in (0..view.blocks()).filter(|&b| reached[b]) {
            let block = Block(b as u32);
            let params = view.params(block);
            if into.targets_only[b] {
                for (k, param) in params.clone().enumerate() {
                    if facts.decided(param) {
                        continue;
                    }
                    let fact = facts.param(param, k, into.edges_into(b));
                    changed |= facts.take(param, fact);
                }
            }
            for inst in params.end..view.block(block).end {
                if facts.decided(inst) {
                    continue;
                }
                let fact = facts.inst(inst);
                changed |= facts.take(inst, fact);
            }
        }
        if !changed {
            break;
        }
        passes += 1;
        if passes > len + 1 {
            // Each pass that goes on decides one more value: past this, the analysis is wrong.
            return Err(SimplifyError::Refused { inst: 0, what: "propagation that did not settle within one pass per value" });
        }
    }
    let mut stands = Vec::with_capacity_in(len, alloc.clone());
    let mut constant = filled(len, None, &alloc);
    for value in 0..len {
        stands.push(facts.resolve(value as u32));
        if let (Known::Int(it), true) = (facts.known[value], view.op(value) != Op::Param) {
            constant[value] = Some(it);
        }
    }
    let edit = Edit {
        block_kept: filled(view.blocks(), true, &alloc),
        inst_kept: filled(len, true, &alloc),
        stands,
        constant,
        jump: no_jumps,
    };
    rebuild(view, &edit, alloc)
}

/// The facts found so far.
struct Facts<'v, A: Allocator> {
    view: View<'v>,
    /// `Plant::FoldSubAsAdd`, in a test.
    plant: bool,
    /// Per value: the value it stands for, itself if none; set once, always to a value that
    /// stands for none, so the chains end.
    stands: Vec<u32, A>,
    known: Vec<Known, A>,
}

impl<A: Allocator> Facts<'_, A> {
    fn decided(&self, value: usize) -> bool {
        self.stands[value] != value as u32 || self.known[value] != Known::No
    }

    /// The value `value` stands for at the end of its chain.
    fn resolve(&self, mut value: u32) -> u32 {
        // A chain is acyclic and at most one link per value long.
        for _ in 0..=self.stands.len() {
            let next = self.stands[value as usize];
            if next == value {
                return value;
            }
            value = next;
        }
        unreachable!("a chain of `stands` longer than the values: a cycle, which `take` never makes")
    }

    /// The known integer `value` reads, when its type is a scalar integer.
    fn int(&self, value: u32) -> Option<(u128, u32)> {
        let ty = Ty(self.view.tys[value as usize]);
        if !ty.is_scalar() || !ty.is_int() {
            return None;
        }
        match self.known[self.resolve(value) as usize] {
            Known::Int(it) => Some((it, ty.bits(self.view.head.ptr_bits))),
            Known::No | Known::Pair(..) => None,
        }
    }

    /// Records `fact` for `value`; whether it is new.
    fn take(&mut self, value: usize, fact: Fact) -> bool {
        match fact {
            Fact::None => false,
            Fact::Stands(other) => {
                let other = self.resolve(other);
                if other == value as u32 {
                    return false;
                }
                self.stands[value] = other;
                true
            }
            Fact::Known(known) => {
                self.known[value] = known;
                true
            }
        }
    }

    /// Block parameter `param`, the `k`th of its block, whose block's edges have their
    /// arguments at `edges` in `extra`.
    fn param(&self, param: usize, k: usize, edges: &[u32]) -> Fact {
        let mut same = None;
        let mut all_same = true;
        let mut constant = None;
        let mut all_constant = true;
        for &args in edges {
            let arg = self.resolve(self.view.extra[args as usize + k]);
            if arg == param as u32 {
                continue;
            }
            match same {
                None => same = Some(arg),
                Some(it) if it != arg => all_same = false,
                Some(_) => {}
            }
            match (self.int(arg), constant) {
                (Some((it, _)), None) => constant = Some(it),
                (Some((it, _)), Some(was)) if it != was => all_constant = false,
                (Some(_), Some(_)) => {}
                (None, _) => all_constant = false,
            }
        }
        let ty = Ty(self.view.tys[param]);
        match (same, all_same, constant, all_constant) {
            (Some(value), true, _, _) => Fact::Stands(value),
            (Some(_), false, Some(it), true) if ty.is_scalar() && ty.is_int() => Fact::Known(Known::Int(it)),
            _ => Fact::None,
        }
    }

    /// Instruction `inst`, not a parameter.
    fn inst(&self, inst: usize) -> Fact {
        let view = self.view;
        if view.flags(inst) != Flags::NONE {
            return Fact::None;
        }
        let op = view.op(inst);
        let ty = Ty(view.tys[inst]);
        let Some(width) = (ty.is_scalar() && ty.is_int()).then(|| ty.bits(view.head.ptr_bits)) else {
            return match op {
                Op::UaddOverflow | Op::SaddOverflow | Op::UsubOverflow | Op::SsubOverflow | Op::UmulOverflow | Op::SmulOverflow => {
                    let mut o = view.operands(inst);
                    match (self.int(o.value().0), self.int(o.value().0)) {
                        (Some((a, width)), Some((b, _))) => {
                            let (value, flag) = overflow(op, a, b, width);
                            Fact::Known(Known::Pair(value, flag))
                        }
                        _ => Fact::None,
                    }
                }
                Op::Select => self.select(inst),
                _ => Fact::None,
            };
        };
        let mut o = view.operands(inst);
        match op {
            Op::Iconst => Fact::Known(Known::Int(iconst_value(o.imm(), width))),
            Op::IconstWide => Fact::Known(Known::Int(wide_value(o.constant(), width))),
            Op::Add | Op::Sub | Op::Mul | Op::Udiv | Op::Sdiv | Op::Urem | Op::Srem | Op::And
            | Op::Or | Op::Xor | Op::Shl | Op::Lshr | Op::Ashr | Op::Rotl | Op::Rotr
            | Op::Umulhi | Op::Smulhi | Op::UaddSat | Op::SaddSat | Op::UsubSat | Op::SsubSat
            | Op::Umin | Op::Umax | Op::Smin | Op::Smax => {
                let (Some((a, _)), Some((b, _))) = (self.int(o.value().0), self.int(o.value().0)) else {
                    return Fact::None;
                };
                // Planted (test builds only): `sub` folded as `add`.
                let op = match self.plant && op == Op::Sub {
                    true => Op::Add,
                    false => op,
                };
                match binary(op, a, b, width) {
                    Ok(it) => Fact::Known(Known::Int(it)),
                    // A trap is the instruction's to keep.
                    Err(_) => Fact::None,
                }
            }
            Op::Neg | Op::Not | Op::Clz | Op::Ctz | Op::Popcnt | Op::Bswap | Op::Bitrev => match self.int(o.value().0) {
                Some((a, _)) => Fact::Known(Known::Int(unary(op, a, width))),
                None => Fact::None,
            },
            Op::Project => {
                let pair = self.resolve(o.value().0);
                match (self.known[pair as usize], o.imm()) {
                    (Known::Pair(value, _), 0) => Fact::Known(Known::Int(value)),
                    (Known::Pair(_, flag), 1) => Fact::Known(Known::Int(u128::from(flag))),
                    _ => Fact::None,
                }
            }
            Op::Icmp => {
                let cond = o.int_cond();
                match (self.int(o.value().0), self.int(o.value().0)) {
                    (Some((a, from)), Some((b, _))) => Fact::Known(Known::Int(u128::from(compare(cond, a, b, from)))),
                    _ => Fact::None,
                }
            }
            Op::Select => self.select(inst),
            Op::Zext | Op::Sext | Op::Trunc => match self.int(o.value().0) {
                Some((a, from)) => Fact::Known(Known::Int(convert(op, a, from, width))),
                None => Fact::None,
            },
            Op::Freeze => match self.int(o.value().0) {
                Some((a, _)) => Fact::Known(Known::Int(a)),
                None => Fact::None,
            },
            _ => Fact::None,
        }
    }

    /// A `select`: the arm a known condition picks, the one value of both arms, or the one
    /// known integer of both.
    fn select(&self, inst: usize) -> Fact {
        let mut o = self.view.operands(inst);
        let cond = o.value().0;
        let (a, b) = (self.resolve(o.value().0), self.resolve(o.value().0));
        if let Some((cond, _)) = self.int(cond) {
            return Fact::Stands(if cond != 0 { a } else { b });
        }
        if a == b {
            return Fact::Stands(a);
        }
        let ty = Ty(self.view.tys[inst]);
        match (self.int(a), self.int(b)) {
            (Some((x, _)), Some((y, _))) if x == y && ty.is_scalar() && ty.is_int() => Fact::Known(Known::Int(x)),
            _ => Fact::None,
        }
    }
}
