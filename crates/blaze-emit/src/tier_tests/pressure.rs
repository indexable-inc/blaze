//! Register pressure in the Fast tier (D100 point 5, "homes and eviction"): more values live at
//! once than there are registers of the class, run as machine code and compared with LIR's
//! evaluator and with the value LIR's semantics give (crates/blaze-lir/src/arith.rs, `binary`:
//! `add`, `mul` wrap modulo 2^64, `xor` is bitwise; "`Op::Shl => (a << amount) & m`" with
//! "`amount = b % width`", 0 at width 1). A child of tier_tests.rs, so it uses that harness's
//! placer, evaluator call and names.
//!
//! - One block with 24 block-local values live at once: x9 to x15 are the only registers a local
//!   may take in a function whose homes save no callee-saved register (emit.rs, `take`), so the
//!   emission pass must evict to cells and reload.
//! - Twenty-one values live across a block boundary AND eight block-local ones in the later
//!   block: before compose/reserve.rs the analysis gave homes all 17 general registers, and the
//!   later block's first instruction was refused with "no register of the class is free: each
//!   holds a value live across blocks or an operand of this instruction" (the tier equality
//!   harness counted 226 such programs of 512 at 18:00 on 2026-11-16). Now x11 to x15 are never
//!   a home.
//! - Shifts, divisions and `freeze` of 1 bit (table_gen/arm64_one_bit.rs), against the evaluator.

use blaze_lir::builder::{Param, Variadic};
use blaze_lir::eval::Val;
use blaze_lir::func::{BlockKind, Conv, Func};
use blaze_lir::{Builder, Flags, Full, Op, Ty, Value};
use blaze_own::Alloc;
use blaze_tier::{ARM64, RegClass, RegSet, Table, TierName};

use super::{Nameless, evaluated, fast, placed, table};
use crate::compose::analysis::{self, Home};
use crate::compose::reserve;
use crate::tests::run;

/// The arguments every test here runs on.
const ARGS: [u64; 5] = [0, 1, 7, 0xdead_beef, u64::MAX];

fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

/// A builder of `f(p: i64) -> i64` with its entry block begun; returns the builder and `p`.
fn begin() -> (Builder<Alloc<'static>, Full>, Value) {
    let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc());
    let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let p = b.param(Ty::I64);
    (b, p)
}

/// `func` through the Fast tier, placed, and called on each of [`ARGS`]: equal to `want(p)` and
/// to the evaluator's answer, or the test fails naming the argument.
fn agrees(what: &str, func: &Func<Alloc<'static>>, want: &dyn Fn(u64) -> u64) {
    let view = func.view();
    if let Err(why) = blaze_lir::verify(view) {
        panic!("{what}: the function does not verify: {why}");
    }
    let machine = table().head().machine;
    let emitted = match fast(view, &Nameless, machine, alloc()) {
        Ok(emitted) => emitted,
        Err(error) => panic!("{what}: the Fast tier refused it: {error}"),
    };
    let exec = match placed(emitted, TierName::Fast) {
        Ok(exec) => exec,
        Err(broke) => panic!("{what}: {broke}"),
    };
    for p in ARGS {
        let right = want(p);
        match evaluated(view, &[Val::Int(u128::from(p))]) {
            Ok(it) => assert_eq!(it, right, "{what}: the evaluator's f({p:#x}) is not LIR's semantics"),
            Err(why) => panic!("{what}: the evaluator did not answer f({p:#x}): {why}"),
        }
        assert_eq!(run::call(&exec, [p, 0, 0, 0]), right, "{what}: the Fast tier's f({p:#x})");
    }
}

/// `f(p) = (p+1) * (p+2) + (p+3) + .. + (p+24)`, every `p + k` defined before the first is
/// used: 24 locals live at once in one block, more than the seven registers (x9 to x15) a local
/// may take here, so the emission pass evicts the one whose next use is farthest and reloads it.
/// Expected: passes, printing "pressure: 24 locals in one block agree on 5 arguments". Fails if
/// an evicted local is not stored, is reloaded from the wrong cell, or is refused.
#[test]
fn a_block_with_more_live_locals_than_registers_agrees() {
    let (mut b, p) = begin();
    let mut v = std::vec::Vec::new();
    for k in 1..=24u128 {
        let c = b.iconst(Ty::I64, k);
        v.push(b.binary(Op::Add, Flags::NONE, p, c));
    }
    let mut acc = b.binary(Op::Mul, Flags::NONE, v[0], v[1]);
    for &x in &v[2..] {
        acc = b.binary(Op::Add, Flags::NONE, acc, x);
    }
    b.ret(&[acc]);
    let func = match b.finish_checked() {
        Ok(func) => func,
        Err(why) => panic!("the function does not build: {why:?}"),
    };
    let want = |p: u64| {
        let mut acc = p.wrapping_add(1).wrapping_mul(p.wrapping_add(2));
        for k in 3..=24u64 {
            acc = acc.wrapping_add(p.wrapping_add(k));
        }
        acc
    };
    agrees("24 locals in one block", &func, &want);
    std::println!("pressure: 24 locals in one block agree on {} arguments", ARGS.len());
}

/// `p` and `h_k = p + k` (k = 1..=20) are defined in the entry block and read in the next, so
/// they are live across a block boundary: 21 homes wanted, more than the 17 general registers.
/// In the next block `l_j = h_j * h_(21-j)` (j = 1..=8) are block-local and all live before the
/// sum `l_1 + .. + l_8`, then `xor` with every `h_k`, then `+ p`, reads them. The analysis must
/// leave x11 to x15 (compose/reserve.rs) to the locals and every operand loaded from a cell.
/// Expected: passes, printing "pressure: 21 homes and 8 locals agree on 5 arguments". Without
/// reserve.rs (homes over all 17 registers) it fails with the Fast tier's refusal "no register
/// of the class is free: each holds a value live across blocks or an operand of this
/// instruction".
#[test]
fn locals_find_registers_beside_more_homes_than_registers() {
    let (mut b, p) = begin();
    let mut h = std::vec::Vec::new();
    for k in 1..=20u128 {
        let c = b.iconst(Ty::I64, k);
        h.push(b.binary(Op::Add, Flags::NONE, p, c));
    }
    let next = b.create_block(BlockKind::Normal);
    b.jump((next, &[]));
    b.begin(next);
    let mut l = std::vec::Vec::new();
    for j in 0..8usize {
        l.push(b.binary(Op::Mul, Flags::NONE, h[j], h[19 - j]));
    }
    let mut acc = l[0];
    for &x in &l[1..] {
        acc = b.binary(Op::Add, Flags::NONE, acc, x);
    }
    for &x in &h {
        acc = b.binary(Op::Xor, Flags::NONE, acc, x);
    }
    acc = b.binary(Op::Add, Flags::NONE, acc, p);
    b.ret(&[acc]);
    let func = match b.finish_checked() {
        Ok(func) => func,
        Err(why) => panic!("the function does not build: {why:?}"),
    };
    // No home is in the kept set, which is five caller-saved registers of each class: x11 to
    // x15 and v27 to v31 (AAPCS64: x9 to x15 and v16 to v31 are not preserved across a call).
    let gpr = reserve::kept(&ARM64, RegClass::Gpr);
    let fpr = reserve::kept(&ARM64, RegClass::Fpr);
    let mut want_gpr = RegSet::EMPTY;
    let mut want_fpr = RegSet::EMPTY;
    for k in 11..=15u8 {
        want_gpr = want_gpr.with(blaze_tier::Reg(k));
    }
    for k in 27..=31u8 {
        want_fpr = want_fpr.with(blaze_tier::Reg(32 + k));
    }
    assert!(gpr == want_gpr, "the kept general registers are x11 to x15: {gpr:?}");
    assert!(fpr == want_fpr, "the kept float registers are v27 to v31: {fpr:?}");
    assert!(reserve::kept_callee_saved(&ARM64) == RegSet::EMPTY, "arm64 keeps no callee-saved register");
    let an = match analysis::analyse(func.view(), &ARM64, alloc()) {
        Ok(an) => an,
        Err(error) => panic!("the analysis refused it: {error}"),
    };
    let mut homes = 0;
    for home in an.home.iter() {
        if let Home::Reg(reg) = *home {
            assert!(!gpr.contains(reg) && !fpr.contains(reg), "a home took kept register {reg:?}");
            homes += 1;
        }
    }
    assert_eq!(homes, 12, "the 12 general registers outside the kept five are homes; the other 9 values are cells");
    let want = |p: u64| {
        let h: std::vec::Vec<u64> = (1..=20u64).map(|k| p.wrapping_add(k)).collect();
        let mut acc = 0u64;
        for j in 0..8 {
            acc = acc.wrapping_add(h[j].wrapping_mul(h[19 - j]));
        }
        for &x in &h {
            acc ^= x;
        }
        acc.wrapping_add(p)
    };
    agrees("21 homes and 8 locals", &func, &want);
    std::println!("pressure: 21 homes and 8 locals agree on {} arguments", ARGS.len());
}

/// `f(p) = zext(op(trunc p, trunc (p >> 1)))` for each 1-bit shift, and `freeze` of 1 bit:
/// LIR's semantics give `trunc p` for every shift (the amount is `b % 1 = 0`) and for `freeze`.
/// Divisions of 1 bit by 1: `udiv` gives `a`, `urem` and `srem` 0, `sdiv` traps for `a = 1`
/// (`-1 / -1` does not fit) and gives 0 for `a = 0`, so it is run on even `p` only. A trap would
/// stop this process; a divisor of 0 is never made here.
/// Expected: passes, printing "one bit: 3 shifts, 4 divisions and freeze agree". Fails with the
/// table's `Missing` if the table was not generated again after arm64_one_bit.rs was wired.
#[test]
fn one_bit_shifts_divisions_and_freeze_agree() {
    let shifts = [Op::Shl, Op::Lshr, Op::Ashr];
    for op in shifts {
        let (mut b, p) = begin();
        let one = b.iconst(Ty::I64, 1);
        let q = b.binary(Op::Lshr, Flags::NONE, p, one);
        let a = b.convert(Op::Trunc, Flags::NONE, Ty::I1, p);
        let s = b.convert(Op::Trunc, Flags::NONE, Ty::I1, q);
        let r = b.binary(op, Flags::NONE, a, s);
        let out = b.convert(Op::Zext, Flags::NONE, Ty::I64, r);
        b.ret(&[out]);
        let func = match b.finish_checked() {
            Ok(func) => func,
            Err(why) => panic!("the function does not build: {why:?}"),
        };
        agrees(op.name(), &func, &|p| p & 1);
    }
    let divisions: [(Op, fn(u64) -> u64); 4] =
        [(Op::Udiv, |p| p & 1), (Op::Urem, |_| 0), (Op::Srem, |_| 0), (Op::Sdiv, |_| 0)];
    for (op, want) in divisions {
        let (mut b, p) = begin();
        // `sdiv`: `p & ~1`, so `a = 0` and no overflow trap; the others take `p`'s low bit.
        let p = match op {
            Op::Sdiv => {
                let even = b.iconst(Ty::I64, u128::from(u64::MAX - 1));
                b.binary(Op::And, Flags::NONE, p, even)
            }
            _ => p,
        };
        let a = b.convert(Op::Trunc, Flags::NONE, Ty::I1, p);
        let d = b.iconst(Ty::I1, 1);
        let r = b.binary(op, Flags::NONE, a, d);
        let out = b.convert(Op::Zext, Flags::NONE, Ty::I64, r);
        b.ret(&[out]);
        let func = match b.finish_checked() {
            Ok(func) => func,
            Err(why) => panic!("the function does not build: {why:?}"),
        };
        agrees(op.name(), &func, &want);
    }
    let (mut b, p) = begin();
    let a = b.convert(Op::Trunc, Flags::NONE, Ty::I1, p);
    let r = b.unary(Op::Freeze, Flags::NONE, a);
    let out = b.convert(Op::Zext, Flags::NONE, Ty::I64, r);
    b.ret(&[out]);
    let func = match b.finish_checked() {
        Ok(func) => func,
        Err(why) => panic!("the function does not build: {why:?}"),
    };
    agrees("freeze", &func, &|p| p & 1);
    std::println!("one bit: {} shifts, {} divisions and freeze agree", shifts.len(), divisions.len());
}
