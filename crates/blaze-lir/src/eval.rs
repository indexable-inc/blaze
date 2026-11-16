//! A reference evaluator of LIR's integer core, for tests: what every backend's code for a
//! function must do, by LIR's stated semantics (the crate's "Semantics": every operation
//! without flags is defined as Cranelift's are, a shift masks its amount, division by zero and
//! `sdiv` of `MIN` by -1 trap). Backends are compared against it on random programs
//! (docs/v2/plan/design/backend-next.md, (e)); it is never const eval and never runs in a
//! build.
//!
//! It models scalar integers `i1` to `i128`, their arithmetic, bitwise, shift, rotate,
//! min/max, saturating and overflow operations, `project` of an overflow pair, `icmp`,
//! `select`, the conversions and `freeze`; stack slots with `slot_addr`, `ptr_add`, `load`,
//! `store`, `slot_live` and `slot_dead`; and `jump`, `br`, `switch`, `return`, `trap` and
//! `unreachable`. Anything else (a call, a float, a vector, an instruction with flags, a memory
//! promise) is refused with [`EvalError::Unsupported`], never guessed. Undefined behaviour is
//! an outcome ([`Outcome::Undefined`]): a comparison of two backends says nothing of a case
//! with one. Fuel bounds every run.

use std::alloc::Allocator;

use crate::{
    Block, Flags, IntCond, Op, Value, View,
    func::SlotId,
    op::Target,
};

/// A value as the evaluator holds it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Val {
    /// An integer of its type's width, its bits above the width zero.
    Int(u128),
    /// A pointer into stack slot `slot`, `offset` bytes from its start.
    Ptr { slot: u32, offset: i128 },
    /// An overflow operation's `{T, i1}`.
    Pair(u128, bool),
}

/// Why a run stopped by a trap.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Trap {
    /// `trap` with its code.
    Code(u32),
    /// A division or remainder by zero.
    DivByZero,
    /// `sdiv` of the type's minimum by -1.
    DivOverflow,
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
}

impl std::fmt::Display for EvalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
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
        }
    }
}

impl std::error::Error for EvalError {}

const fn mask(width: u32) -> u128 {
    match width {
        128.. => u128::MAX,
        width => (1u128 << width) - 1,
    }
}

const fn sext(value: u128, width: u32) -> i128 {
    let shift = 128 - width;
    ((value << shift) as i128) >> shift
}

const fn smin(width: u32) -> i128 {
    match width {
        128.. => i128::MIN,
        width => -(1i128 << (width - 1)),
    }
}

const fn smax(width: u32) -> i128 {
    match width {
        128.. => i128::MAX,
        width => (1i128 << (width - 1)) - 1,
    }
}

/// The 256-bit product of `a` and `b`: its high and low halves.
const fn wide_mul(a: u128, b: u128) -> (u128, u128) {
    let (a1, a0) = (a >> 64, a & (u64::MAX as u128));
    let (b1, b0) = (b >> 64, b & (u64::MAX as u128));
    let (p00, p01, p10, p11) = (a0 * b0, a0 * b1, a1 * b0, a1 * b1);
    let mid = (p00 >> 64) + (p01 & (u64::MAX as u128)) + (p10 & (u64::MAX as u128));
    let low = (p00 & (u64::MAX as u128)) | (mid << 64);
    let high = p11 + (p01 >> 64) + (p10 >> 64) + (mid >> 64);
    (high, low)
}

/// The high half of the `2 * width`-bit product.
fn mulhi(a: u128, b: u128, width: u32, signed: bool) -> u128 {
    if width < 128 {
        let product = match signed {
            true => (sext(a, width) * sext(b, width) >> width) as u128,
            false => (a * b) >> width,
        };
        return product & mask(width);
    }
    let (high, _) = wide_mul(a, b);
    match signed {
        // The signed high half from the unsigned: less `b` where `a` is negative, and `a`
        // where `b` is.
        true => {
            let high = if (a as i128) < 0 { high.wrapping_sub(b) } else { high };
            if (b as i128) < 0 { high.wrapping_sub(a) } else { high }
        }
        false => high,
    }
}

/// A two-operand integer operation of `width` bits; `Err` for a trap.
fn binary(op: Op, a: u128, b: u128, width: u32) -> Result<u128, Trap> {
    let m = mask(width);
    let (sa, sb) = (sext(a, width), sext(b, width));
    let clamp = |value: i128| value.clamp(smin(width), smax(width)) as u128 & m;
    let amount = (b % u128::from(width)) as u32;
    Ok(match op {
        Op::Add => a.wrapping_add(b) & m,
        Op::Sub => a.wrapping_sub(b) & m,
        Op::Mul => a.wrapping_mul(b) & m,
        Op::Udiv | Op::Urem | Op::Sdiv | Op::Srem if b == 0 => return Err(Trap::DivByZero),
        Op::Udiv => a / b,
        Op::Urem => a % b,
        Op::Sdiv if sa == smin(width) && sb == -1 => return Err(Trap::DivOverflow),
        Op::Sdiv => sa.wrapping_div(sb) as u128 & m,
        // `MIN % -1` is 0, as Cranelift's `srem` gives.
        Op::Srem => sa.wrapping_rem(sb) as u128 & m,
        Op::And => a & b,
        Op::Or => a | b,
        Op::Xor => a ^ b,
        Op::Shl => (a << amount) & m,
        Op::Lshr => a >> amount,
        Op::Ashr => (sa >> amount) as u128 & m,
        Op::Rotl | Op::Rotr if amount == 0 => a,
        Op::Rotl => ((a << amount) | (a >> (width - amount))) & m,
        Op::Rotr => ((a >> amount) | (a << (width - amount))) & m,
        Op::Umulhi => mulhi(a, b, width, false),
        Op::Smulhi => mulhi(a, b, width, true),
        Op::Umin => a.min(b),
        Op::Umax => a.max(b),
        Op::Smin => sa.min(sb) as u128 & m,
        Op::Smax => sa.max(sb) as u128 & m,
        Op::UaddSat => match a.checked_add(b) {
            Some(sum) if sum <= m => sum,
            _ => m,
        },
        Op::UsubSat => a.saturating_sub(b),
        Op::SaddSat if width == 128 => sa.saturating_add(sb) as u128,
        Op::SsubSat if width == 128 => sa.saturating_sub(sb) as u128,
        Op::SaddSat => clamp(sa + sb),
        Op::SsubSat => clamp(sa - sb),
        _ => unreachable!("`binary` is called for the operations above"),
    })
}

/// An overflow operation's value and whether it overflowed.
fn overflow(op: Op, a: u128, b: u128, width: u32) -> (u128, bool) {
    let m = mask(width);
    let (sa, sb) = (sext(a, width), sext(b, width));
    let signed = |exact: Option<i128>| match exact {
        Some(it) => it < smin(width) || it > smax(width),
        None => true,
    };
    match op {
        Op::UaddOverflow => {
            let (sum, carry) = a.overflowing_add(b);
            (sum & m, carry || sum > m)
        }
        Op::UsubOverflow => (a.wrapping_sub(b) & m, a < b),
        Op::UmulOverflow => match a.checked_mul(b) {
            Some(product) => (product & m, product > m),
            None => (a.wrapping_mul(b) & m, true),
        },
        Op::SaddOverflow => (a.wrapping_add(b) & m, signed(sa.checked_add(sb))),
        Op::SsubOverflow => (a.wrapping_sub(b) & m, signed(sa.checked_sub(sb))),
        Op::SmulOverflow => (a.wrapping_mul(b) & m, signed(sa.checked_mul(sb))),
        _ => unreachable!("`overflow` is called for the overflow operations"),
    }
}

fn unary(op: Op, a: u128, width: u32) -> u128 {
    let m = mask(width);
    let spare = 128 - width;
    match op {
        Op::Neg => a.wrapping_neg() & m,
        Op::Not => !a & m,
        Op::Clz => u128::from(a.leading_zeros() - spare),
        Op::Ctz if a == 0 => u128::from(width),
        Op::Ctz => u128::from(a.trailing_zeros()),
        Op::Popcnt => u128::from(a.count_ones()),
        Op::Bswap if width < 16 => a,
        Op::Bswap => a.swap_bytes() >> spare,
        Op::Bitrev => a.reverse_bits() >> spare,
        _ => unreachable!("`unary` is called for the operations above"),
    }
}

fn compare(cond: IntCond, a: u128, b: u128, width: u32) -> bool {
    let (sa, sb) = (sext(a, width), sext(b, width));
    match cond {
        IntCond::Eq => a == b,
        IntCond::Ne => a != b,
        IntCond::Ult => a < b,
        IntCond::Ule => a <= b,
        IntCond::Ugt => a > b,
        IntCond::Uge => a >= b,
        IntCond::Slt => sa < sb,
        IntCond::Sle => sa <= sb,
        IntCond::Sgt => sa > sb,
        IntCond::Sge => sa >= sb,
    }
}

/// The stack slots of a run: their bytes, which are initialized, and which slots are live.
struct Memory<A: Allocator> {
    starts: Vec<usize, A>,
    bytes: Vec<u8, A>,
    init: Vec<bool, A>,
    live: Vec<bool, A>,
}

/// One run of a function.
struct Run<'a, A: Allocator + Clone> {
    view: View<'a>,
    values: Vec<Option<Val>, A>,
    memory: Memory<A>,
    alloc: A,
}

/// Runs `view` from its entry block with `args` for its parameters, at most `fuel`
/// instructions, every buffer in `alloc`. `view` must be verified (`crate::verify`).
pub fn run_in<A: Allocator + Clone>(
    view: View<'_>,
    args: &[Val],
    fuel: u64,
    alloc: A,
) -> Result<Outcome<A>, EvalError> {
    let mut starts = Vec::with_capacity_in(view.slots.len(), alloc.clone());
    let mut size = 0;
    for slot in view.slots {
        starts.push(size);
        size += slot.size as usize;
    }
    let mut bytes = Vec::with_capacity_in(size, alloc.clone());
    bytes.resize(size, 0);
    let mut init = Vec::with_capacity_in(size, alloc.clone());
    init.resize(size, false);
    let mut live = Vec::with_capacity_in(view.slots.len(), alloc.clone());
    live.resize(view.slots.len(), true);
    let mut values = Vec::with_capacity_in(view.len(), alloc.clone());
    values.resize(view.len(), None);
    let memory = Memory { starts, bytes, init, live };
    let mut run = Run { view, values, memory, alloc };
    let entry = view.params(Block(0));
    if entry.len() != args.len() {
        return Err(EvalError::Args { want: entry.len(), given: args.len() });
    }
    for (inst, &arg) in entry.zip(args) {
        run.values[inst] = Some(arg);
    }
    run.go(Block(0), fuel)
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

    fn go(&mut self, mut block: Block, mut fuel: u64) -> Result<Outcome<A>, EvalError> {
        loop {
            let body = self.view.params(block).end..self.view.block(block).end;
            for inst in body {
                let Some(left) = fuel.checked_sub(1) else { return Ok(Outcome::OutOfFuel) };
                fuel = left;
                let op = self.view.op(inst);
                if self.view.flags(inst) != Flags::NONE {
                    return Err(EvalError::Unsupported { inst, op, what: "an instruction with flags" });
                }
                if op.is_terminator() {
                    match self.end(inst, op)? {
                        Next::To(next) => {
                            block = next;
                            break;
                        }
                        Next::Done(outcome) => return Ok(outcome),
                    }
                }
                match self.step(inst, op)? {
                    Ok(Some(value)) => self.values[inst] = Some(value),
                    Ok(None) => {}
                    Err(outcome) => return Ok(outcome),
                }
            }
        }
    }

    /// Instruction `inst`, not a terminator: its value, if it has one, or how the run ends.
    fn step(&mut self, inst: usize, op: Op) -> Result<Result<Option<Val>, Outcome<A>>, EvalError> {
        let mut o = self.view.operands(inst);
        let undefined = |why| Ok(Err(Outcome::Undefined { inst, why }));
        let value = match op {
            Op::Iconst => {
                let width = self.width(inst)?;
                let word = o.imm();
                let value = match width {
                    ..=32 => u128::from(word),
                    _ => (word as i32 as i128) as u128,
                };
                Val::Int(value & mask(width))
            }
            Op::IconstWide => {
                let width = self.width(inst)?;
                let words = o.constant();
                let value =
                    words.iter().rev().fold(0u128, |it, &word| (it << 32) | u128::from(word));
                Val::Int(value & mask(width))
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
                _ => {
                    return Err(EvalError::Unsupported { inst, op, what: "a project of other than an overflow pair" });
                }
            },
            Op::Icmp => {
                let cond = o.int_cond();
                let ((a, width), (b, _)) = (self.int(inst, o.value())?, self.int(inst, o.value())?);
                Val::Int(u128::from(compare(cond, a, b, width)))
            }
            Op::Select => {
                let (cond, _) = self.int(inst, o.value())?;
                let (a, b) = (self.get(inst, o.value())?, self.get(inst, o.value())?);
                if cond != 0 { a } else { b }
            }
            Op::Zext | Op::Sext | Op::Trunc => {
                let width = self.width(inst)?;
                let (a, from) = self.int(inst, o.value())?;
                Val::Int(match op {
                    Op::Sext => sext(a, from) as u128 & mask(width),
                    _ => a & mask(width),
                })
            }
            Op::Freeze => self.get(inst, o.value())?,
            Op::SlotAddr => Val::Ptr { slot: o.slot().0, offset: 0 },
            Op::SlotLive | Op::SlotDead => {
                let slot = o.slot();
                let (start, end) = self.span(slot);
                self.memory.init[start..end].fill(false);
                self.memory.live[slot.index()] = op == Op::SlotLive;
                return Ok(Ok(None));
            }
            Op::PtrAdd => {
                let Val::Ptr { slot, offset } = self.get(inst, o.value())? else {
                    return Err(EvalError::Unsupported { inst, op, what: "a pointer not into a slot" });
                };
                let (by, width) = self.int(inst, o.value())?;
                Val::Ptr { slot, offset: offset.wrapping_add(sext(by, width)) }
            }
            Op::Load => {
                let width = self.width(inst)?;
                let ptr = self.get(inst, o.value())?;
                let at = i128::from(o.offset());
                let range = match self.access(inst, op, ptr, at, width)? {
                    Ok(it) => it,
                    Err(why) => return undefined(why),
                };
                if !self.memory.init[range.clone()].iter().all(|&it| it) {
                    return undefined("a load of uninitialized bytes");
                }
                let bytes = &self.memory.bytes[range];
                Val::Int(bytes.iter().rev().fold(0u128, |it, &byte| (it << 8) | u128::from(byte)))
            }
            Op::Store => {
                let stored = o.value();
                let (value, width) = self.int(inst, stored)?;
                let ptr = self.get(inst, o.value())?;
                let at = i128::from(o.offset());
                let range = match self.access(inst, op, ptr, at, width)? {
                    Ok(it) => it,
                    Err(why) => return undefined(why),
                };
                for (i, at) in range.enumerate() {
                    self.memory.bytes[at] = (value >> (8 * i)) as u8;
                    self.memory.init[at] = true;
                }
                return Ok(Ok(None));
            }
            _ => return Err(EvalError::Unsupported { inst, op, what: "this operation" }),
        };
        Ok(Ok(Some(value)))
    }

    /// The bytes of slot `slot` in the run's memory.
    fn span(&self, slot: SlotId) -> (usize, usize) {
        let start = self.memory.starts[slot.index()];
        (start, start + self.view.slots[slot.index()].size as usize)
    }

    /// The bytes an access of `width` bits at `ptr` plus `at` touches, or why it is undefined.
    fn access(
        &self,
        inst: usize,
        op: Op,
        ptr: Val,
        at: i128,
        width: u32,
    ) -> Result<Result<std::ops::Range<usize>, &'static str>, EvalError> {
        let Val::Ptr { slot, offset } = ptr else {
            return Err(EvalError::Unsupported { inst, op, what: "an access not through a slot's pointer" });
        };
        if width % 8 != 0 {
            return Err(EvalError::Unsupported { inst, op, what: "an access of a width not of whole bytes" });
        }
        let align = match self.view.mem_attrs(inst) {
            None => 0,
            Some(attrs) if attrs.range == 0 && attrs.bits == 0 && attrs.pointee_align == 0 => attrs.align,
            Some(_) => return Err(EvalError::Unsupported { inst, op, what: "a memory promise past alignment" }),
        };
        let slot = SlotId(slot);
        if !self.memory.live[slot.index()] {
            return Ok(Err("an access to a dead slot"));
        }
        let size = i128::from(width / 8);
        let offset = offset.wrapping_add(at);
        let rec = self.view.slots[slot.index()];
        if offset < 0 || offset + size > i128::from(rec.size) {
            return Ok(Err("an access out of its slot"));
        }
        if align > rec.align || offset % (1i128 << align) != 0 {
            return Ok(Err("an access less aligned than it says"));
        }
        let (start, _) = self.span(slot);
        let first = start + offset as usize;
        Ok(Ok(first..first + size as usize))
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
        let mut args = Vec::with_capacity_in(to.args.len(), self.alloc.clone());
        for value in to.args.iter() {
            args.push(self.get(inst, value)?);
        }
        for (param, arg) in params.zip(args) {
            self.values[param] = Some(arg);
        }
        Ok(())
    }
}
