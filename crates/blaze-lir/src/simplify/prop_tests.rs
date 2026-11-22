//! Copy and constant propagation, dead code removal and the whole simplify stage, validated as
//! a tier is (docs/v2/plan/design/backend-next.md, D100 point 1): on generated programs the
//! reference evaluator gives the same outcome before and after each pass and after the stage
//! (C1's `run_calls_in` the stage's oracle, the callee followed before and spliced after), the
//! output passes `verify` (each pass verifies what it returns), and a planted wrong fold
//! (`Plant::FoldSubAsAdd`) is caught.
//!
//! The rules the expectations come from are LIR's own (lib.rs, "Semantics", and `Op`'s docs):
//! "Every operation without flags is defined, as Cranelift's are: a shift masks its amount,
//! division by zero traps"; a run whose input outcome is undefined or out of fuel says nothing
//! of its output and is passed over.

use std::alloc::Global;

use blaze_sim::{Broke, Config, Tape, check, control};

use super::{Plant, SimplifyError, dead, propagate, propagate_planted, simplify};
use crate::{
    View,
    arith::mask,
    body::{Bodies, BodyId},
    builder::{BuildError, Builder, Full, MemAttrs, Param, Variadic},
    eval::{EvalError, Outcome, Trap, Val, run_calls_in, run_in},
    flags::{Flags, IntCond},
    func::{BlockKind, Conv, Func, RefId, RefKind, SigId, SlotId, SlotKind, Value},
    inline::TRIVIAL,
    op::Op,
    print,
    ty::Ty,
};

/// Far past what a generated run takes (each loop runs at most four times).
const FUEL: u64 = 100_000;
const ROOT: BodyId = BodyId([1; 32]);
const G: BodyId = BodyId([2; 32]);

const BINARY: [Op; 27] = [
    Op::Add, Op::Sub, Op::Sub, Op::Sub, Op::Mul, Op::Udiv, Op::Sdiv, Op::Urem, Op::Srem, Op::And,
    Op::Or, Op::Xor, Op::Shl, Op::Lshr, Op::Ashr, Op::Rotl, Op::Rotr, Op::Umulhi, Op::Smulhi,
    Op::UaddSat, Op::SaddSat, Op::UsubSat, Op::SsubSat, Op::Umin, Op::Umax, Op::Smin, Op::Smax,
];
const UNARY: [Op; 7] = [Op::Neg, Op::Not, Op::Clz, Op::Ctz, Op::Popcnt, Op::Bitrev, Op::Bswap];
const OVERFLOW: [Op; 6] =
    [Op::UaddOverflow, Op::SaddOverflow, Op::UsubOverflow, Op::SsubOverflow, Op::UmulOverflow, Op::SmulOverflow];
const WIDTHS: [Ty; 4] = [Ty::I8, Ty::I32, Ty::I64, Ty::I128];

/// A generated caller, its callee `g(x, y) = (x - y) + 3`, and how many calls it makes.
struct Made {
    func: Func<Global>,
    callee: Func<Global>,
    width: u32,
    calls: usize,
}

/// The program being generated: scalar integers of one width, constants, every integer
/// operation the evaluator models, `select`, overflow pairs, diamonds joining at a parameter
/// (now and then one value on both arms, or a constant condition), counted loops carrying an
/// invariant parameter, a slot stored and loaded, and calls to `g`.
struct Gen<'t> {
    t: &'t Tape,
    b: Builder<Global, Full>,
    ty: Ty,
    width: u32,
    /// The values every later point of the main path may use.
    ints: Vec<Value>,
    slot: SlotId,
    /// `g`'s reference and signature, when the program may call it.
    callee: Option<(RefId, SigId)>,
    calls: usize,
}

impl Gen<'_> {
    fn pick(&self) -> Value {
        self.ints[self.t.below(self.ints.len() as u64, "a value") as usize]
    }

    fn constant(&mut self) -> Value {
        let value = match self.t.below(6, "a constant's kind") {
            0 => 0,
            1 => 1,
            2 => u128::MAX,
            3 => u128::from(self.t.below(10, "a small constant")),
            4 => u128::from(self.t.int::<u64>("a word")),
            _ => (u128::from(self.t.int::<u64>("a high word")) << 64) | u128::from(self.t.int::<u64>("a low word")),
        };
        self.b.iconst(self.ty, value)
    }

    /// A value from `a` and `b` by a binary operation, or a constant; not kept.
    fn compute(&mut self, a: Value, b: Value) -> Value {
        match self.t.below(4, "computed or constant") {
            0 => self.constant(),
            _ => {
                let op = BINARY[self.t.below(BINARY.len() as u64, "a binary operation") as usize];
                self.b.binary(op, Flags::NONE, a, b)
            }
        }
    }

    /// An `i1`: a comparison of two values, or now and then a constant.
    fn cond(&mut self) -> Value {
        if self.t.below(4, "a constant condition") == 0 {
            let value = self.t.below(2, "the condition");
            return self.b.iconst(Ty::I1, u128::from(value));
        }
        let cond = IntCond::ALL[self.t.below(IntCond::ALL.len() as u64, "a comparison") as usize];
        let (a, b) = (self.pick(), self.pick());
        self.b.icmp(cond, a, b)
    }

    fn statement(&mut self) {
        let ty = self.ty;
        match self.t.below(11, "a statement") {
            0 | 1 => {
                let value = self.constant();
                self.ints.push(value);
            }
            2 | 3 => {
                let (a, b) = (self.pick(), self.pick());
                let op = BINARY[self.t.below(BINARY.len() as u64, "a binary operation") as usize];
                let value = self.b.binary(op, Flags::NONE, a, b);
                self.ints.push(value);
            }
            4 => {
                let op = UNARY[self.t.below(UNARY.len() as u64, "a unary operation") as usize];
                // `bswap` is of whole bytes: of at least 16 bits.
                let op = if op == Op::Bswap && self.width < 16 { Op::Not } else { op };
                let a = self.pick();
                let value = self.b.unary(op, Flags::NONE, a);
                self.ints.push(value);
            }
            5 => {
                let cond = self.cond();
                let a = self.pick();
                let b = if self.t.below(3, "one value on both arms") == 0 { a } else { self.pick() };
                let value = self.b.select(Flags::NONE, cond, a, b);
                self.ints.push(value);
            }
            6 => {
                let op = OVERFLOW[self.t.below(OVERFLOW.len() as u64, "an overflow operation") as usize];
                let (a, b) = (self.pick(), self.pick());
                let pair = self.b.overflow(op, a, b);
                let value = self.b.project(pair, 0);
                let flag = self.b.project(pair, 1);
                let flag = self.b.convert(Op::Zext, Flags::NONE, ty, flag);
                self.ints.push(value);
                self.ints.push(flag);
            }
            7 => self.diamond(),
            8 => self.counted_loop(),
            9 => {
                let value = self.pick();
                let addr = self.b.slot_addr(self.slot);
                self.b.store(Flags::NONE, value, addr, 0, MemAttrs::default());
                let addr = self.b.slot_addr(self.slot);
                let loaded = self.b.load(ty, Flags::NONE, addr, 0, MemAttrs::default());
                self.ints.push(loaded);
            }
            _ => match self.callee {
                Some((g, sig)) if self.calls < 4 => {
                    let (a, b) = (self.pick(), self.pick());
                    let value = self.b.call(Flags::NONE, g, sig, &[a, b]);
                    self.calls += 1;
                    self.ints.push(value);
                }
                _ => {
                    let (a, b) = (self.pick(), self.pick());
                    let value = self.b.binary(Op::Sub, Flags::NONE, a, b);
                    self.ints.push(value);
                }
            },
        }
    }

    /// `br c, then, other`; each passes one value to `join`'s parameter: the same value on
    /// both now and then (a parameter that is a copy).
    fn diamond(&mut self) {
        let cond = self.cond();
        let [then, other, join] = [(); 3].map(|_| self.b.create_block(BlockKind::Normal));
        let shared = (self.t.below(3, "one value on both edges") == 0).then(|| self.pick());
        self.b.br(Flags::NONE, cond, (then, &[]), (other, &[]));
        for block in [then, other] {
            self.b.begin(block);
            let value = match shared {
                Some(value) => value,
                None => {
                    let (a, b) = (self.pick(), self.pick());
                    self.compute(a, b)
                }
            };
            self.b.jump((join, &[value]));
        }
        self.b.begin(join);
        let joined = self.b.param(self.ty);
        self.ints.push(joined);
    }

    /// `for i in 0..n { acc = acc op inv }`, `n` below 5: the header's `inv` takes one value
    /// from the entry and itself from the back edge (a parameter that is a copy).
    fn counted_loop(&mut self) {
        let ty = self.ty;
        let n = self.t.below(5, "the trip count");
        let n = self.b.iconst(ty, u128::from(n));
        let zero = self.b.iconst(ty, 0);
        let one = self.b.iconst(ty, 1);
        let (start, invariant) = (self.pick(), self.pick());
        let [header, body, exit] = [(); 3].map(|_| self.b.create_block(BlockKind::Normal));
        self.b.jump((header, &[zero, start, invariant]));
        self.b.begin(header);
        let (i, acc, inv) = (self.b.param(ty), self.b.param(ty), self.b.param(ty));
        let more = self.b.icmp(IntCond::Ult, i, n);
        self.b.br(Flags::NONE, more, (body, &[]), (exit, &[]));
        self.b.begin(body);
        let next = self.compute(acc, inv);
        let step = self.b.binary(Op::Add, Flags::NONE, i, one);
        self.b.jump((header, &[step, next, inv]));
        self.b.begin(exit);
        self.ints.extend([i, acc, inv]);
    }
}

/// `g(x, y) = (x - y) + 3` of `ty`.
fn callee(ty: Ty) -> Result<Func<Global>, BuildError> {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[Param::of(ty), Param::of(ty)], &[Param::of(ty)]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let (x, y) = (b.param(ty), b.param(ty));
    let difference = b.binary(Op::Sub, Flags::NONE, x, y);
    let three = b.iconst(ty, 3);
    let sum = b.binary(Op::Add, Flags::NONE, difference, three);
    b.ret(&[sum]);
    b.finish_checked()
}

/// A random `f(a, b)` of one width: its statements, then the `xor` of every value it kept,
/// returned, so a wrong value anywhere shows.
fn random(t: &Tape, calls: bool) -> Result<Made, BuildError> {
    let ty = WIDTHS[t.below(WIDTHS.len() as u64, "the width") as usize];
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[Param::of(ty), Param::of(ty)], &[Param::of(ty)]);
    b.set_signature(sig);
    let callee_ref = calls.then(|| (b.reference(RefKind::Func, 0), sig));
    let slot = b.slot(16, 4, SlotKind::Named);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let (a, c) = (b.param(ty), b.param(ty));
    b.slot_live(slot);
    let width = ty.bits(64);
    let mut g = Gen { t, b, ty, width, ints: vec![a, c], slot, callee: callee_ref, calls: 0 };
    let first = g.constant();
    g.ints.push(first);
    for _ in 0..t.below(24, "statements") {
        g.statement();
    }
    let mut result = g.ints[0];
    for k in 1..g.ints.len() {
        let value = g.ints[k];
        result = g.b.binary(Op::Xor, Flags::NONE, result, value);
    }
    g.b.ret(&[result]);
    let Gen { b, calls, .. } = g;
    Ok(Made { func: b.finish_checked()?, callee: callee(ty)?, width, calls })
}

/// `f` (the root) calls `g` by its reference 0.
struct One<'g> {
    g: View<'g>,
}

impl Bodies for One<'_> {
    fn callee(&self, from: BodyId, reference: RefId) -> Option<(BodyId, View<'_>)> {
        match (from, reference) {
            (ROOT, RefId(0)) => Some((G, self.g)),
            _ => None,
        }
    }
}

fn arguments(t: &Tape, width: u32) -> [Val; 2] {
    let draw = || {
        let high = u128::from(t.int::<u64>("an argument's high word"));
        let low = u128::from(t.int::<u64>("an argument's low word"));
        Val::Int(((high << 64) | low) & mask(width))
    };
    [draw(), draw()]
}

/// The input's outcome, or `None` when it says nothing of the output (undefined, out of fuel).
fn defined(outcome: Result<Outcome<Global>, EvalError>, view: View<'_>) -> Result<Option<Outcome<Global>>, Broke> {
    match outcome {
        Ok(Outcome::Undefined { .. } | Outcome::OutOfFuel) => Ok(None),
        Ok(it) => Ok(Some(it)),
        Err(e) => Err(Broke::Harness(format!("the evaluator refused the input: {e}\n{}", print(view)))),
    }
}

/// The output of `pass` ran to `after`: the same outcome as the input's `before`.
fn same(
    pass: &str,
    before: &Outcome<Global>,
    after: Result<Outcome<Global>, EvalError>,
    input: View<'_>,
    output: View<'_>,
) -> Result<(), Broke> {
    match after {
        Ok(after) if after == *before => Ok(()),
        Ok(after) => Err(Broke::Oracle(format!(
            "differs after {pass}: {before:?} became {after:?}\ninput:\n{}\noutput:\n{}",
            print(input),
            print(output)
        ))),
        Err(e) => Err(Broke::Oracle(format!(
            "differs after {pass}: the output does not run ({e})\ninput:\n{}\noutput:\n{}",
            print(input),
            print(output)
        ))),
    }
}

fn refused(pass: &str, error: SimplifyError, input: View<'_>) -> Broke {
    Broke::Oracle(format!("{pass} refused a generated program: {error}\n{}", print(input)))
}

/// Each pass alone and both in order: the evaluator's outcome is kept.
fn passes_agree(t: &Tape, plant: Plant) -> Result<(), Broke> {
    let made = random(t, false).map_err(|e| Broke::Harness(format!("the generator: {e}")))?;
    let view = made.func.view();
    let args = arguments(t, made.width);
    let Some(before) = defined(run_in(view, &args, FUEL, Global), view)? else { return Ok(()) };
    let propagated = propagate_planted(view, plant, Global).map_err(|e| refused("propagation", e, view))?;
    same("propagation", &before, run_in(propagated.view(), &args, FUEL, Global), view, propagated.view())?;
    let cleaned = dead(view, Global).map_err(|e| refused("dead code removal", e, view))?;
    same("dead code removal", &before, run_in(cleaned.view(), &args, FUEL, Global), view, cleaned.view())?;
    let both = dead(propagated.view(), Global).map_err(|e| refused("dead code removal", e, propagated.view()))?;
    same("propagation then dead code removal", &before, run_in(both.view(), &args, FUEL, Global), view, both.view())
}

/// The stage on a caller of `g`: the caller run with `g` followed equals the output run (with
/// `g` followed where a call stayed); the read list is `[G]` exactly when a call was spliced.
fn stage_agrees(t: &Tape) -> Result<(), Broke> {
    let made = random(t, true).map_err(|e| Broke::Harness(format!("the generator: {e}")))?;
    let view = made.func.view();
    let bodies = One { g: made.callee.view() };
    let args = arguments(t, made.width);
    let before = run_calls_in(view, ROOT, &args, FUEL, &bodies, Global);
    let Some(before) = defined(before, view)? else { return Ok(()) };
    let (out, read) = simplify(view, ROOT, &bodies, TRIVIAL, Global).map_err(|e| refused("the simplify stage", e, view))?;
    let after = run_calls_in(out.view(), ROOT, &args, FUEL, &bodies, Global);
    same("the simplify stage", &before, after, view, out.view())?;
    let calls = |v: View<'_>| (0..v.len()).filter(|&i| v.op(i) == Op::Call).count();
    let spliced = calls(out.view()) < made.calls;
    let want: &[BodyId] = if spliced { &[G] } else { &[] };
    if read[..] != *want {
        return Err(Broke::Oracle(format!(
            "the stage's read list is {:?}, not {want:?} ({} calls before, {} after)\n{}",
            &read[..],
            made.calls,
            calls(out.view()),
            print(view)
        )));
    }
    Ok(())
}

/// D100 point 1: "copy and constant propagation with folding; dead code and trivial branch
/// removal ... validated like a tier (evaluator equality on generated programs)".
/// Expected: `test simplify::prop_tests::propagation_and_dead_code_keep_what_the_function_computes ... ok`.
#[test]
fn propagation_and_dead_code_keep_what_the_function_computes() {
    check(Config::cases(512), |t| passes_agree(t, Plant::None));
}

/// Control: `sub` of two constants folded as their sum.
/// Expected: `test simplify::prop_tests::control_a_sub_folded_as_an_add ... ok`, and on
/// stderr "plant FoldSubAsAdd: caught at case k of 512".
#[test]
fn control_a_sub_folded_as_an_add() {
    control(Config::cases(512), Plant::None, Plant::FoldSubAsAdd, "differs after propagation", passes_agree);
}

/// The stage, inliner first, against C1's evaluator following the call it splices.
/// Expected: `test simplify::prop_tests::the_stage_keeps_what_the_caller_computes ... ok`.
#[test]
fn the_stage_keeps_what_the_caller_computes() {
    check(Config::cases(512), stage_agrees);
}

/// A one-block function of `ty` with no parameters returning what `body` makes.
fn one_block(ty: Ty, body: impl FnOnce(&mut Builder<Global, Full>) -> Value) -> Func<Global> {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[], &[Param::of(ty)]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let value = body(&mut b);
    b.ret(&[value]);
    b.finish_checked().expect("the function builds")
}

fn ops(view: View<'_>) -> Vec<Op> {
    (0..view.len()).map(|i| view.op(i)).collect()
}

/// `5 - 7` of `i8` is `254` by the evaluator's wrapping `sub` (2^8 - 2): the stage's last two
/// passes leave one `iconst` and the `return`.
/// Expected: `test simplify::prop_tests::a_sub_of_constants_folds_to_the_evaluators_value ... ok`.
#[test]
fn a_sub_of_constants_folds_to_the_evaluators_value() {
    let func = one_block(Ty::I8, |b| {
        let (five, seven) = (b.iconst(Ty::I8, 5), b.iconst(Ty::I8, 7));
        b.binary(Op::Sub, Flags::NONE, five, seven)
    });
    let out = dead(propagate(func.view(), Global).expect("propagates").view(), Global).expect("cleans");
    assert_eq!(ops(out.view()), [Op::Iconst, Op::Return], "{}", print(out.view()));
    let ran = run_in(out.view(), &[], FUEL, Global).expect("runs");
    assert_eq!(ran, Outcome::<Global>::Returned(vec![Val::Int(254)]));
}

/// "Division by zero traps" (lib.rs, Semantics): `7 / 0` is not folded, and still traps.
/// Expected: `test simplify::prop_tests::a_division_by_zero_stays_and_traps ... ok`.
#[test]
fn a_division_by_zero_stays_and_traps() {
    let func = one_block(Ty::I32, |b| {
        let (seven, zero) = (b.iconst(Ty::I32, 7), b.iconst(Ty::I32, 0));
        b.binary(Op::Udiv, Flags::NONE, seven, zero)
    });
    let out = dead(propagate(func.view(), Global).expect("propagates").view(), Global).expect("cleans");
    assert!(ops(out.view()).contains(&Op::Udiv), "{}", print(out.view()));
    let ran = run_in(out.view(), &[], FUEL, Global).expect("runs");
    assert_eq!(ran, Outcome::<Global>::Trapped(Trap::DivByZero));
}

/// `add nsw` of `100 + 100` in `i8` overflows: a promise broken, undefined; the flag keeps the
/// instruction, so nothing is made of an undefined value.
/// Expected: `test simplify::prop_tests::a_flagged_add_is_not_folded ... ok`.
#[test]
fn a_flagged_add_is_not_folded() {
    let func = one_block(Ty::I8, |b| {
        let hundred = b.iconst(Ty::I8, 100);
        b.binary(Op::Add, Flags::NSW, hundred, hundred)
    });
    let out = propagate(func.view(), Global).expect("propagates");
    assert!(ops(out.view()).contains(&Op::Add), "{}", print(out.view()));
}

/// `br` on `true` becomes `jump`, and the arm it never takes goes.
/// Expected: `test simplify::prop_tests::a_br_on_a_constant_becomes_a_jump ... ok`.
#[test]
fn a_br_on_a_constant_becomes_a_jump() {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[], &[Param::of(Ty::I32)]);
    b.set_signature(sig);
    let [entry, yes, no] = [(); 3].map(|_| b.create_block(BlockKind::Normal));
    b.begin(entry);
    let on = b.iconst(Ty::I1, 1);
    b.br(Flags::NONE, on, (yes, &[]), (no, &[]));
    b.begin(yes);
    let one = b.iconst(Ty::I32, 1);
    b.ret(&[one]);
    b.begin(no);
    let two = b.iconst(Ty::I32, 2);
    b.ret(&[two]);
    let func = b.finish_checked().expect("builds");
    let out = dead(func.view(), Global).expect("cleans");
    assert_eq!(out.view().blocks(), 2, "{}", print(out.view()));
    assert!(!ops(out.view()).contains(&Op::Br), "{}", print(out.view()));
    let ran = run_in(out.view(), &[], FUEL, Global).expect("runs");
    assert_eq!(ran, Outcome::<Global>::Returned(vec![Val::Int(1)]));
}

/// A loop header's parameter given `x` on entry and itself on the back edge is `x`: after the
/// two passes the header keeps only its counter (two parameters in all, `x` and `i`).
/// Expected: `test simplify::prop_tests::an_invariant_loop_parameter_is_a_copy ... ok`.
#[test]
fn an_invariant_loop_parameter_is_a_copy() {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I32)], &[Param::of(Ty::I32)]);
    b.set_signature(sig);
    let [entry, header, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));
    b.begin(entry);
    let x = b.param(Ty::I32);
    let zero = b.iconst(Ty::I32, 0);
    b.jump((header, &[zero, x]));
    b.begin(header);
    let (i, inv) = (b.param(Ty::I32), b.param(Ty::I32));
    let three = b.iconst(Ty::I32, 3);
    let more = b.icmp(IntCond::Ult, i, three);
    b.br(Flags::NONE, more, (body, &[]), (exit, &[]));
    b.begin(body);
    let one = b.iconst(Ty::I32, 1);
    let next = b.binary(Op::Add, Flags::NONE, i, one);
    b.jump((header, &[next, inv]));
    b.begin(exit);
    let sum = b.binary(Op::Add, Flags::NONE, i, inv);
    b.ret(&[sum]);
    let func = b.finish_checked().expect("builds");
    let out = dead(propagate(func.view(), Global).expect("propagates").view(), Global).expect("cleans");
    let params = ops(out.view()).iter().filter(|&&op| op == Op::Param).count();
    assert_eq!(params, 2, "{}", print(out.view()));
    let args = [Val::Int(40)];
    let before = run_in(func.view(), &args, FUEL, Global).expect("runs");
    let after = run_in(out.view(), &args, FUEL, Global).expect("runs");
    assert_eq!(before, after);
    assert_eq!(after, Outcome::<Global>::Returned(vec![Val::Int(43)]));
}

/// A value that stands for a block parameter whose arguments fold to a constant propagates that
/// constant to its own users (tests the CSR worklist's `stands` relay).
#[test]
fn a_stands_relay_chain_folds_dependent_instructions() {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I1)], &[Param::of(Ty::I32)]);
    b.set_signature(sig);
    let [entry, then, other, join] = [(); 4].map(|_| b.create_block(BlockKind::Normal));
    b.begin(entry);
    let cond = b.param(Ty::I1);
    b.br(Flags::NONE, cond, (then, &[]), (other, &[]));
    b.begin(then);
    let two = b.iconst(Ty::I32, 2);
    let five_a = b.binary(Op::Add, Flags::NONE, two, two);
    b.jump((join, &[five_a]));
    b.begin(other);
    let one = b.iconst(Ty::I32, 1);
    let three = b.iconst(Ty::I32, 3);
    let five_b = b.binary(Op::Add, Flags::NONE, one, three);
    b.jump((join, &[five_b]));
    b.begin(join);
    // `joined` folds to `Known::Int(4)` only after both arms' `add`s fold; `sel` immediately
    // stands for `joined` (`a == b`), so `plus_one` must be woken through `sel` once `joined`
    // becomes known.
    let joined = b.param(Ty::I32);
    let sel = b.select(Flags::NONE, cond, joined, joined);
    let k = b.iconst(Ty::I32, 1);
    let plus_one = b.binary(Op::Add, Flags::NONE, sel, k);
    b.ret(&[plus_one]);
    let func = b.finish_checked().expect("builds");
    let out = dead(propagate(func.view(), Global).expect("propagates").view(), Global).expect("cleans");
    assert_eq!(
        ops(out.view()),
        [Op::Param, Op::Br, Op::Jump, Op::Jump, Op::Iconst, Op::Return],
        "{}",
        print(out.view())
    );
    for c in [0u128, 1] {
        let ran = run_in(out.view(), &[Val::Int(c)], FUEL, Global).expect("runs");
        assert_eq!(ran, Outcome::<Global>::Returned(vec![Val::Int(5)]));
    }
}

/// No-op fast paths for `propagate` and `dead` on a function with nothing to fold or remove.
#[test]
fn noop_function_preserves_ir_and_semantics() {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I32), Param::of(Ty::I32)], &[Param::of(Ty::I32)]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let (x, y) = (b.param(Ty::I32), b.param(Ty::I32));
    let sum = b.binary(Op::Add, Flags::NONE, x, y);
    b.ret(&[sum]);
    let func = b.finish_checked().expect("builds");
    let prop = propagate(func.view(), Global).expect("propagates");
    let cleaned = dead(prop.view(), Global).expect("cleans");
    assert_eq!(ops(cleaned.view()), ops(func.view()));
    let args = [Val::Int(17), Val::Int(25)];
    assert_eq!(
        run_in(cleaned.view(), &args, FUEL, Global).expect("runs"),
        Outcome::<Global>::Returned(vec![Val::Int(42)])
    );
}

/// A long chain of constant additions folds completely in linear time.
#[test]
fn a_long_constant_chain_folds_completely() {
    let func = one_block(Ty::I32, |b| {
        let one = b.iconst(Ty::I32, 1);
        let mut acc = b.iconst(Ty::I32, 0);
        for _ in 0..512 {
            acc = b.binary(Op::Add, Flags::NONE, acc, one);
        }
        acc
    });
    let out = dead(propagate(func.view(), Global).expect("propagates").view(), Global).expect("cleans");
    assert_eq!(ops(out.view()), [Op::Iconst, Op::Return], "{}", print(out.view()));
    let ran = run_in(out.view(), &[], FUEL, Global).expect("runs");
    assert_eq!(ran, Outcome::<Global>::Returned(vec![Val::Int(512)]));
}
