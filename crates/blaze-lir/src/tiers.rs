//! Tier fuzzing (docs/v2/plan/design/backend-next.md, (e)): random LIR programs run through
//! every tier, each outcome compared with the reference evaluator's ([`crate::eval`]) on the
//! built function. The tiers today are the evaluator on the function's record read in place
//! and on its text form parsed back; a backend reading LIR (the stencils, LLVM) joins as a
//! [`Tier`]. A planted tier that miscompiles `sub` as `add` must be caught, or the oracle is
//! not trusted (rule `dst-not-review`). The evaluator's own semantics are pinned by cases
//! worked by hand first.
//!
//! The generator draws everything from the tape, bounded: up to 6 blocks of up to 12
//! operations, forward edges only besides counted self-loops (a counter in a stack slot, run at
//! most 6 times), so every program ends. Each block mixes what it computed into an
//! accumulator slot, which the last block returns, so a wrong value anywhere shows. Division
//! by zero traps and every tier must trap alike.

use alloc::{format, string::String, string::ToString, vec, vec::Vec};
use alloc::alloc::Global;

use blaze_format::Aligned;
use blaze_sim::{Broke, Config, Tape};

use crate::{
    View,
    builder::{BuildError, Builder, Full, MemAttrs, Param, Variadic},
    eval::{self, EvalError, Outcome, Trap, Val},
    flags::{Flags, IntCond},
    func::{Block, BlockKind, Conv, Func, SlotKind, Value},
    op::Op,
    parse, persist, print,
    ty::Ty,
};

/// Instructions a run may take: far past what a generated program runs, so only a broken
/// loop (a miscompiled counter) runs out.
const FUEL: u64 = 100_000;

const INTS: [Ty; 6] = [Ty::I1, Ty::I8, Ty::I16, Ty::I32, Ty::I64, Ty::I128];
const STORED: [Ty; 5] = [Ty::I8, Ty::I16, Ty::I32, Ty::I64, Ty::I128];
const BINARY: [Op; 26] = [
    Op::Add,
    Op::Sub,
    Op::Mul,
    Op::Udiv,
    Op::Sdiv,
    Op::Urem,
    Op::Srem,
    Op::And,
    Op::Or,
    Op::Xor,
    Op::Shl,
    Op::Lshr,
    Op::Ashr,
    Op::Rotl,
    Op::Rotr,
    Op::Umulhi,
    Op::Smulhi,
    Op::UaddSat,
    Op::SaddSat,
    Op::UsubSat,
    Op::SsubSat,
    Op::Umin,
    Op::Umax,
    Op::Smin,
    Op::Smax,
    Op::Add,
];
const UNARY: [Op; 7] = [Op::Neg, Op::Not, Op::Clz, Op::Ctz, Op::Popcnt, Op::Bswap, Op::Bitrev];
const OVERFLOW: [Op; 6] = [
    Op::UaddOverflow,
    Op::SaddOverflow,
    Op::UsubOverflow,
    Op::SsubOverflow,
    Op::UmulOverflow,
    Op::SmulOverflow,
];
const DATA: u32 = 32;
/// The accumulator's multiplier: odd, so a mix loses no bits of what it takes.
const MIX: u128 = 0x9E37_79B9_7F4A_7C15;

/// One way to run a function: what a backend is, to the harness.
trait Tier {
    fn name(&self) -> &'static str;
    fn run(&self, view: View<'_>, args: &[Val]) -> Result<Outcome<Global>, String>;
}

fn evaluated(view: View<'_>, args: &[Val]) -> Result<Outcome<Global>, String> {
    eval::run_in(view, args, FUEL, Global).map_err(|why| why.to_string())
}

/// The function's record, written and read back in place.
struct Record;

impl Tier for Record {
    fn name(&self) -> &'static str {
        "the record read in place"
    }

    fn run(&self, view: View<'_>, args: &[Val]) -> Result<Outcome<Global>, String> {
        let bytes = persist::to_bytes(&view);
        let aligned = Aligned::copy_from(&bytes);
        let read = persist::read(&aligned).map_err(|why| format!("{why:?}"))?;
        evaluated(read, args)
    }
}

/// The function's text form, parsed back.
struct Text;

impl Tier for Text {
    fn name(&self) -> &'static str {
        "the text form parsed back"
    }

    fn run(&self, view: View<'_>, args: &[Val]) -> Result<Outcome<Global>, String> {
        let func = parse(&print(view)).map_err(|why| format!("{why:?}"))?;
        evaluated(func.view(), args)
    }
}

/// The planted miscompile: a backend that emits the function's first `sub` as `add`.
struct SubAsAdd;

impl Tier for SubAsAdd {
    fn name(&self) -> &'static str {
        "a backend that emits the first `sub` as `add`"
    }

    fn run(&self, view: View<'_>, args: &[Val]) -> Result<Outcome<Global>, String> {
        let mut func = view.to_func_in(Global);
        if let Some(at) = func.ops.iter().position(|&it| it == Op::Sub as u8) {
            func.ops[at] = Op::Add as u8;
        }
        evaluated(func.view(), args)
    }
}

#[derive(Clone, Copy, Debug)]
enum Plant {
    None,
    SubAsAdd,
}

/// Builds one random program.
struct Gen {
    b: Builder<Global, Full>,
    blocks: Vec<Block>,
    block_params: Vec<Vec<Ty>>,
    /// Which blocks loop on themselves, by index.
    loops: Vec<bool>,
    /// The entry block's integers, which dominate every block.
    entry: Vec<(Value, Ty)>,
    /// The current block's integers: the entry's, its parameters, what it made.
    pool: Vec<(Value, Ty)>,
    /// What the current block made, mixed into the accumulator before it ends. Cleared as each
    /// block opens: what a terminator's arguments made after the mix is the old block's, and
    /// may not dominate the next one (the first finding, kept as a regression test below).
    made: Vec<Value>,
    /// Which blocks an edge of a reached block goes to.
    reached: Vec<bool>,
    data: Value,
    acc: Value,
    counters: Value,
}

fn ty(tape: &Tape, label: &'static str) -> Ty {
    *tape.one_of(&INTS, label)
}

fn log2(bytes: u32) -> u8 {
    bytes.trailing_zeros() as u8
}

impl Gen {
    fn keep(&mut self, value: Value) -> Value {
        let ty = self.b.ty(value);
        self.pool.push((value, ty));
        self.made.push(value);
        value
    }

    fn constant(&mut self, tape: &Tape, ty: Ty) -> Value {
        let value = tape.u128("constant");
        let value = self.b.iconst(ty, value);
        self.keep(value)
    }

    /// A value of `ty` from the pool, or a new constant.
    fn of(&mut self, tape: &Tape, ty: Ty) -> Value {
        let found: Vec<Value> =
            self.pool.iter().filter(|it| it.1 == ty).map(|it| it.0).collect();
        match found.is_empty() || tape.below(8, "a new constant") == 0 {
            true => self.constant(tape, ty),
            false => found[tape.below(found.len() as u64, "which value") as usize],
        }
    }

    /// Any integer of the pool, or a new constant.
    fn any(&mut self, tape: &Tape) -> Value {
        match self.pool.is_empty() {
            true => {
                let ty = ty(tape, "type");
                self.constant(tape, ty)
            }
            false => self.pool[tape.below(self.pool.len() as u64, "which value") as usize].0,
        }
    }

    fn cond(&mut self, tape: &Tape) -> Value {
        let a = self.any(tape);
        let b = self.of(tape, self.b.ty(a));
        let cond = *tape.one_of(&IntCond::ALL, "condition");
        let cond = self.b.icmp(cond, a, b);
        self.keep(cond)
    }

    /// One operation of the block's body.
    fn op(&mut self, tape: &Tape) {
        match tape.below(10, "operation") {
            0 => {
                let ty = ty(tape, "constant type");
                self.constant(tape, ty);
            }
            1 => {
                let op = *tape.one_of(&BINARY, "binary");
                let a = self.any(tape);
                let ty = self.b.ty(a);
                let mut b = self.of(tape, ty);
                let divides = matches!(op, Op::Udiv | Op::Sdiv | Op::Urem | Op::Srem);
                if divides && tape.below(8, "a zero divisor may stay") != 0 {
                    let one = self.b.iconst(ty, 1);
                    b = self.b.binary(Op::Or, Flags::NONE, b, one);
                }
                let value = self.b.binary(op, Flags::NONE, a, b);
                self.keep(value);
            }
            2 => {
                let op = *tape.one_of(&UNARY, "unary");
                let a = self.any(tape);
                let value = self.b.unary(op, Flags::NONE, a);
                self.keep(value);
            }
            3 => {
                let op = *tape.one_of(&OVERFLOW, "overflow");
                let a = self.any(tape);
                let b = self.of(tape, self.b.ty(a));
                let pair = self.b.overflow(op, a, b);
                let (value, flag) = (self.b.project(pair, 0), self.b.project(pair, 1));
                self.keep(value);
                self.keep(flag);
            }
            4 => {
                self.cond(tape);
            }
            5 => {
                let cond = self.of(tape, Ty::I1);
                let a = self.any(tape);
                let b = self.of(tape, self.b.ty(a));
                let value = self.b.select(Flags::NONE, cond, a, b);
                self.keep(value);
            }
            6 => {
                let a = self.any(tape);
                let (from, to) = (self.b.ty(a), ty(tape, "conversion type"));
                let op = match (to.bits(64), from.bits(64)) {
                    (to, from) if to == from => return,
                    (to, from) if to < from => Op::Trunc,
                    _ if tape.bool("sign") => Op::Sext,
                    _ => Op::Zext,
                };
                let value = self.b.convert(op, Flags::NONE, to, a);
                self.keep(value);
            }
            7 => {
                let a = self.any(tape);
                let value = self.b.unary(Op::Freeze, Flags::NONE, a);
                self.keep(value);
            }
            8 => {
                let ty = *tape.one_of(&STORED, "load type");
                let size = ty.bits(64) / 8;
                let at = size * tape.below(u64::from(DATA / size), "load offset") as u32;
                let value =
                    self.b.load(ty, Flags::NONE, self.data, at as i32, MemAttrs::aligned(log2(size)));
                self.keep(value);
            }
            _ => {
                let ty = *tape.one_of(&STORED, "store type");
                let size = ty.bits(64) / 8;
                let value = self.of(tape, ty);
                let at = size * tape.below(u64::from(DATA / size), "store offset") as u32;
                let attrs = MemAttrs::aligned(log2(size));
                self.b.store(Flags::NONE, value, self.data, at as i32, attrs);
            }
        }
    }

    /// Mixes what the block made into the accumulator: `acc = (acc ^ v) * MIX` for each.
    fn mix(&mut self) -> Value {
        let attrs = MemAttrs::aligned(3);
        let mut acc = self.b.load(Ty::I64, Flags::NONE, self.acc, 0, attrs);
        let mix = self.b.iconst(Ty::I64, MIX);
        for value in core::mem::take(&mut self.made) {
            let bits = self.b.ty(value).bits(64);
            let wide = match bits {
                64 => value,
                ..64 => self.b.convert(Op::Zext, Flags::NONE, Ty::I64, value),
                _ => self.b.convert(Op::Trunc, Flags::NONE, Ty::I64, value),
            };
            let xored = self.b.binary(Op::Xor, Flags::NONE, acc, wide);
            acc = self.b.binary(Op::Mul, Flags::NONE, xored, mix);
            if bits == 128 {
                // The high half too.
                let shift = self.b.iconst(Ty::I128, 64);
                let high = self.b.binary(Op::Lshr, Flags::NONE, value, shift);
                let high = self.b.convert(Op::Trunc, Flags::NONE, Ty::I64, high);
                let xored = self.b.binary(Op::Xor, Flags::NONE, acc, high);
                acc = self.b.binary(Op::Mul, Flags::NONE, xored, mix);
            }
        }
        self.b.store(Flags::NONE, acc, self.acc, 0, attrs);
        acc
    }

    /// The arguments block `to` takes, from the pool.
    fn args(&mut self, tape: &Tape, to: usize) -> Vec<Value> {
        let tys = self.block_params[to].clone();
        tys.into_iter().map(|ty| self.of(tape, ty)).collect()
    }

    /// A forward target of block `from`. The first edge of a block whose next block nothing
    /// reaches yet goes there, so every block has a predecessor that is itself reached (by
    /// induction from the entry): no block is unreachable.
    fn forward(&mut self, tape: &Tape, from: usize, first: bool) -> usize {
        let to = match first && !self.reached[from + 1] {
            true => from + 1,
            false => from + 1 + tape.below((self.blocks.len() - from - 1) as u64, "target") as usize,
        };
        self.reached[to] = true;
        to
    }

    /// Block `at`, after its parameters and counter: its body and its end.
    fn block(&mut self, tape: &Tape, at: usize) {
        let last = at + 1 == self.blocks.len();
        let counted = match self.loops[at] {
            true => {
                let attrs = MemAttrs::aligned(3);
                let offset = 8 * at as i32;
                let count = self.b.load(Ty::I64, Flags::NONE, self.counters, offset, attrs);
                let zero = self.b.iconst(Ty::I64, 0);
                let again = self.b.icmp(IntCond::Ne, count, zero);
                let one = self.b.iconst(Ty::I64, 1);
                let less = self.b.binary(Op::Sub, Flags::NONE, count, one);
                self.b.store(Flags::NONE, less, self.counters, offset, attrs);
                Some(again)
            }
            false => None,
        };
        let ops = tape.int_in(0usize..=12, "operations");
        for _ in 0..ops {
            self.op(tape);
        }
        let acc = self.mix();
        if last {
            self.b.ret(&[acc]);
            return;
        }
        if let Some(again) = counted {
            let (back, on) = (self.args(tape, at), self.forward(tape, at, true));
            let forth = self.args(tape, on);
            let (this, next) = (self.blocks[at], self.blocks[on]);
            self.b.br(Flags::NONE, again, (this, &back), (next, &forth));
            return;
        }
        // A trap only where the next block has a predecessor already.
        match tape.below(8, "terminator") {
            0 if self.reached[at + 1] => self.b.trap(tape.int_in(0u32..=3, "trap code")),
            0..=3 => {
                let on = self.forward(tape, at, true);
                let args = self.args(tape, on);
                self.b.jump((self.blocks[on], &args));
            }
            4..=5 => {
                let cond = self.of(tape, Ty::I1);
                let (a, b) = (self.forward(tape, at, true), self.forward(tape, at, false));
                let (then, otherwise) = (self.args(tape, a), self.args(tape, b));
                let to = (self.blocks[a], then.as_slice());
                self.b.br(Flags::NONE, cond, to, (self.blocks[b], &otherwise));
            }
            _ => {
                let ty = *tape.one_of(&STORED, "scrutinee type");
                let scrutinee = self.of(tape, ty);
                let default = self.forward(tape, at, true);
                let default_args = self.args(tape, default);
                let cases = tape.int_in(0usize..=3, "cases");
                let mut targets = Vec::new();
                for case in 0..cases {
                    let on = self.forward(tape, at, false);
                    // Small, distinct case values, so drawn scrutinees meet them.
                    targets.push((case as u128 * 3 + tape.below(3, "case") as u128, on, self.args(tape, on)));
                }
                let cases: Vec<(u128, (Block, &[Value]))> = targets
                    .iter()
                    .map(|(value, on, args)| (*value, (self.blocks[*on], args.as_slice())))
                    .collect();
                self.b.switch(scrutinee, (self.blocks[default], &default_args), &cases);
            }
        }
    }
}

/// A random program and the arguments of its run; valid by construction, so a refusal is the
/// generator's bug.
fn program(tape: &Tape) -> Result<(Func<Global>, Vec<Val>), BuildError> {
    let params: Vec<Ty> = tape.vec(0..=3, "parameters", |tape| ty(tape, "parameter"));
    let n = tape.int_in(1usize..=6, "blocks");
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig_params: Vec<Param> = params.iter().map(|&ty| Param::of(ty)).collect();
    let sig = b.sig(Conv::C, Variadic::No, &sig_params, &[Param::of(Ty::I64)]);
    b.set_signature(sig);
    let data_slot = b.slot(DATA, 4, SlotKind::Temp);
    let acc_slot = b.slot(8, 3, SlotKind::Temp);
    let counter_slot = b.slot(8 * n as u32, 3, SlotKind::Temp);
    let blocks: Vec<Block> = (0..n).map(|_| b.create_block(BlockKind::Normal)).collect();
    let mut block_params = vec![params.clone()];
    block_params.extend(
        (1..n).map(|_| tape.vec(0..=3, "block parameters", |tape| ty(tape, "block parameter"))),
    );
    let loops: Vec<bool> = (0..n).map(|at| at > 0 && at + 1 < n && tape.bool("loops")).collect();
    let args: Vec<Val> = params
        .iter()
        .map(|ty| {
            let bits = ty.bits(64);
            let value = tape.u128("argument");
            Val::Int(if bits == 128 { value } else { value & ((1 << bits) - 1) })
        })
        .collect();
    // The entry block: its parameters, the slots' addresses, the slots' first bytes.
    b.begin(blocks[0]);
    let entry: Vec<(Value, Ty)> = params.iter().map(|&ty| (b.param(ty), ty)).collect();
    let (data, acc, counters) =
        (b.slot_addr(data_slot), b.slot_addr(acc_slot), b.slot_addr(counter_slot));
    let zero = b.iconst(Ty::I64, 0);
    for at in 0..DATA / 8 {
        b.store(Flags::NONE, zero, data, 8 * at as i32, MemAttrs::aligned(3));
    }
    b.store(Flags::NONE, zero, acc, 0, MemAttrs::aligned(3));
    for at in 0..n {
        let count = b.iconst(Ty::I64, u128::from(tape.int_in(0u64..=5, "loop count")));
        b.store(Flags::NONE, count, counters, 8 * at as i32, MemAttrs::aligned(3));
    }
    let mut g = Gen {
        b,
        blocks,
        block_params,
        loops,
        entry: entry.clone(),
        pool: entry,
        made: Vec::new(),
        reached: (0..n).map(|at| at == 0).collect(),
        data,
        acc,
        counters,
    };
    g.made.extend(g.pool.iter().map(|it| it.0));
    g.block(tape, 0);
    // What the entry block made dominates every block after it.
    g.entry = g.pool.clone();
    for at in 1..n {
        g.b.begin(g.blocks[at]);
        g.pool = g.entry.clone();
        g.made.clear();
        let params = g.block_params[at].clone();
        for ty in params {
            let value = g.b.param(ty);
            g.pool.push((value, ty));
            g.made.push(value);
        }
        g.block(tape, at);
    }
    debug_assert!(g.reached.iter().all(|&it| it), "a block no edge reaches");
    Ok((g.b.finish_checked()?, args))
}

/// The generator's own contract: every program it makes builds and verifies.
fn verifies(tape: &Tape) -> Result<(), Broke> {
    match program(tape) {
        Ok(_) => Ok(()),
        Err(why) => Err(Broke::Oracle(format!("a generated program does not verify: {why:?}"))),
    }
}

#[test]
fn every_generated_program_verifies() {
    blaze_sim::check(Config::cases(512), verifies);
}

/// The first finding (seed 0, case 1): a constant a terminator's arguments made after the mix
/// was mixed again by the next block, which it does not dominate.
#[test]
fn a_value_made_by_a_terminator_stays_in_its_block() {
    let mut values = vec![1, 0, 1, 0, 1, 0, 5, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 1];
    values.extend([0; 25]);
    values.push(4);
    values.extend([0; 7]);
    values.push(4);
    let tape = Tape::replay(&values);
    if let Err(Broke::Oracle(why) | Broke::Harness(why)) = verifies(&tape) {
        panic!("{why}");
    }
}

/// Every tier runs a random program as the evaluator runs the built function.
fn tiers_agree(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let (func, args) = program(tape)
        .map_err(|why| Broke::Harness(format!("a generated program does not build: {why:?}")))?;
    let view = func.view();
    let reference = eval::run_in(view, &args, FUEL, Global)
        .map_err(|why: EvalError| Broke::Harness(format!("the evaluator refused a generated program: {why}")))?;
    if let Outcome::Undefined { .. } = reference {
        return Err(Broke::Harness(format!("a generated program is undefined: {reference:?}")));
    }
    let planted: &[&dyn Tier] = match plant {
        Plant::None => &[],
        Plant::SubAsAdd => &[&SubAsAdd],
    };
    for tier in [&Record as &dyn Tier, &Text].iter().chain(planted) {
        let outcome = tier.run(view, &args).map_err(|why| Broke::Harness(format!("{}: {why}", tier.name())))?;
        if outcome != reference {
            return Err(Broke::Oracle(format!(
                "{} differs from the reference: {outcome:?} against {reference:?}\n{}",
                tier.name(),
                print(view)
            )));
        }
    }
    Ok(())
}

/// The refinement: every LIR tier produces the exact same outcome as the reference LIR evaluator on generated well-formed programs.
#[test]
fn every_tier_agrees_with_the_evaluator() {
    blaze_sim::check(Config::cases(512), |tape| tiers_agree(tape, Plant::None));
}

#[test]
fn the_oracle_catches_a_miscompiled_sub() {
    blaze_sim::control(Config::cases(512), Plant::None, Plant::SubAsAdd, "differs from the reference", tiers_agree);
}

/// `f(params) = body(params)`, returning the one value `body` gives, of type `ret`.
fn one(params: &[Ty], ret: Ty, body: impl FnOnce(&mut Builder<Global, Full>, &[Value]) -> Value) -> Func<Global> {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig_params: Vec<Param> = params.iter().map(|&ty| Param::of(ty)).collect();
    let sig = b.sig(Conv::C, Variadic::No, &sig_params, &[Param::of(ret)]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let values: Vec<Value> = params.iter().map(|&ty| b.param(ty)).collect();
    let value = body(&mut b, &values);
    b.ret(&[value]);
    b.finish_checked().expect("a hand-built function builds")
}

fn ran(func: &Func<Global>, args: &[u128]) -> Outcome<Global> {
    let args: Vec<Val> = args.iter().map(|&it| Val::Int(it)).collect();
    eval::run_in(func.view(), &args, FUEL, Global).expect("the evaluator runs it")
}

fn returned(value: u128) -> Outcome<Global> {
    Outcome::Returned(vec![Val::Int(value)])
}

/// `a op b` of type `ty`.
fn binary(ty: Ty, op: Op, a: u128, b: u128) -> Outcome<Global> {
    let func = one(&[ty, ty], ty, |b, v| b.binary(op, Flags::NONE, v[0], v[1]));
    ran(&func, &[a, b])
}

#[test]
fn the_evaluator_keeps_lirs_stated_semantics() {
    // Division by zero and `MIN / -1` trap; `MIN % -1` is 0.
    assert_eq!(binary(Ty::I32, Op::Udiv, 7, 0), Outcome::<Global>::Trapped(Trap::DivByZero));
    assert_eq!(binary(Ty::I8, Op::Sdiv, 0x80, 0xff), Outcome::<Global>::Trapped(Trap::DivOverflow));
    assert_eq!(binary(Ty::I8, Op::Srem, 0x80, 0xff), returned(0));
    assert_eq!(binary(Ty::I8, Op::Sdiv, 0xf9, 2), returned(0xfd), "-7 / 2 is -3");
    // A shift masks its amount.
    assert_eq!(binary(Ty::I32, Op::Shl, 1, 33), returned(2));
    assert_eq!(binary(Ty::I8, Op::Ashr, 0x80, 9), returned(0xc0));
    assert_eq!(binary(Ty::I16, Op::Rotl, 0x8001, 17), returned(0x0003));
    // The high halves, at 128 bits too.
    assert_eq!(binary(Ty::I128, Op::Umulhi, u128::MAX, u128::MAX), returned(u128::MAX - 1));
    assert_eq!(binary(Ty::I128, Op::Smulhi, u128::MAX, u128::MAX), returned(0), "-1 * -1");
    assert_eq!(binary(Ty::I64, Op::Smulhi, 1 << 63, 1 << 63), returned(1 << 62));
    assert_eq!(binary(Ty::I8, Op::SaddSat, 100, 100), returned(127));
    assert_eq!(binary(Ty::I8, Op::SsubSat, 0x80, 1), returned(0x80));
    assert_eq!(binary(Ty::I8, Op::UaddSat, 200, 100), returned(255));
    let unary = |ty: Ty, op: Op, a: u128| ran(&one(&[ty], ty, |b, v| b.unary(op, Flags::NONE, v[0])), &[a]);
    assert_eq!(unary(Ty::I32, Op::Clz, 0), returned(32));
    assert_eq!(unary(Ty::I16, Op::Ctz, 0), returned(16));
    assert_eq!(unary(Ty::I32, Op::Bswap, 0x1122_3344), returned(0x4433_2211));
    assert_eq!(unary(Ty::I8, Op::Bitrev, 1), returned(0x80));
    // `sadd_overflow` of `127 + 1` in i8: -128, overflowed.
    let flag = one(&[Ty::I8, Ty::I8], Ty::I1, |b, v| {
        let pair = b.overflow(Op::SaddOverflow, v[0], v[1]);
        b.project(pair, 1)
    });
    assert_eq!(ran(&flag, &[127, 1]), returned(1));
    assert_eq!(ran(&flag, &[127, 0x80]), returned(0));
    let wide = one(&[Ty::I128, Ty::I128], Ty::I1, |b, v| {
        let pair = b.overflow(Op::UmulOverflow, v[0], v[1]);
        b.project(pair, 1)
    });
    assert_eq!(ran(&wide, &[1 << 64, 1 << 64]), returned(1));
    assert_eq!(ran(&wide, &[1 << 63, 2]), returned(0));
    // A load of bytes nothing stored is undefined.
    let uninit = one(&[], Ty::I32, |b, _| {
        let slot = b.slot(4, 2, SlotKind::Temp);
        let at = b.slot_addr(slot);
        b.load(Ty::I32, Flags::NONE, at, 0, MemAttrs::aligned(2))
    });
    assert!(matches!(ran(&uninit, &[]), Outcome::Undefined { .. }));
}

#[test]
fn the_evaluator_follows_switches_and_counted_loops() {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I32)], &[Param::of(Ty::I32)]);
    b.set_signature(sig);
    let [entry, looped, done, other] = [(); 4].map(|_| b.create_block(BlockKind::Normal));
    b.begin(entry);
    let n = b.param(Ty::I32);
    let zero = b.iconst(Ty::I32, 0);
    let two = b.iconst(Ty::I32, 2);
    let (start, ended) = ([zero, n], [two]);
    b.switch(n, (other, &[]), &[(3, (looped, &start[..])), (5, (done, &ended[..]))]);
    // looped(sum, k): sum += k, k -= 1, again while k != 0.
    b.begin(looped);
    let (sum, k) = (b.param(Ty::I32), b.param(Ty::I32));
    let sum = b.binary(Op::Add, Flags::NONE, sum, k);
    let one = b.iconst(Ty::I32, 1);
    let k = b.binary(Op::Sub, Flags::NONE, k, one);
    let zero = b.iconst(Ty::I32, 0);
    let again = b.icmp(IntCond::Ne, k, zero);
    b.br(Flags::NONE, again, (looped, &[sum, k]), (done, &[sum]));
    b.begin(done);
    let result = b.param(Ty::I32);
    b.ret(&[result]);
    b.begin(other);
    b.trap(7);
    let func = b.finish_checked().expect("a hand-built function builds");
    assert_eq!(ran(&func, &[3]), returned(6), "3 + 2 + 1");
    assert_eq!(ran(&func, &[5]), returned(2));
    assert_eq!(ran(&func, &[4]), Outcome::<Global>::Trapped(Trap::Code(7)));
    let few = eval::run_in(func.view(), &[Val::Int(3)], 4, Global).expect("the evaluator runs it");
    assert_eq!(few, Outcome::<Global>::OutOfFuel);
}
