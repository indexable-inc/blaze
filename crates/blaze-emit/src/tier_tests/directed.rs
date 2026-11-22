//! The Fast tier's directed cases (D195 item 4), each run as machine code by every tier and
//! compared with the value LIR's semantics give and, where the program calls nothing native, with
//! LIR's evaluator. A child of tier_tests.rs: its placer, evaluator call and tier functions.
//!
//! - Spills: pressure.rs's 24 live locals, here with a 40 KiB slot under them so every evicted
//!   local's cell is past the near loads' reach (the far templates, `glue::FAR`).
//! - A loop whose four block parameters swap two of them on the back edge (a cycle of the
//!   parallel copy, broken through the spare cell), with an entry value live across it, at no
//!   slot, a 40 KiB slot and a 200 KiB slot (far cells, the stack probe).
//! - Calls through a pointer to a native function that trashes every caller-saved register
//!   (x9 to x17, v16 to v31) with values live across them, at low and at high pressure.
//! - A far frame (> 32 KiB, the shape of the Reference tier's x12 bug of 2026-11-21) with a flag
//!   live across stores into two slots, one of them past the near `add x, sp, #imm`.
//! - A checked-arithmetic flag (`sadd_overflow`) live across a far store: every tier that emits
//!   it must agree; a tier's refusal is printed by name (the Fast tier holds no compound value).
//! - The function's own data: a code-part row, a relro-part row, and a row holding another
//!   row's address (a relro word naming the record's own code part).
//! - The access grid: a store and a load of every width and register class at every address
//!   form the composer makes (scaled, unscaled, a register offset, a fused `ptr_add`).
//! - The scratch pool's control (rule 23): a cell store that writes x16 first. Declared in the
//!   template's clobbers, the Fast tier refuses it by name (scratch.rs, `Clobbered`); with the
//!   declaration removed, the same bytes run and the answer is wrong.

use blaze_lir::builder::{DataReloc, MemAttrs, Param, Variadic};
use blaze_lir::eval::Val;
use blaze_lir::func::{BlockKind, Conv, Func, SlotKind};
use blaze_lir::{Builder, Flags, Full, IntCond, Op, Ty, Value};
use blaze_own::{AVec, Alloc, Vec};
use blaze_tier::{ARM64, Field, Pattern, Reg, RegClass, Table, TableHead, Template, Tier, TierError, TierName};

use super::{Nameless, evaluated, fast, placed, reference, table};
use crate::Fast;
use crate::compose::emit::glue;
use crate::scratch::ScratchError;
use crate::table_gen::EncodedTable;
use crate::tests::run;

/// The arguments every test here runs on.
const ARGS: [u64; 6] = [0, 1, 7, 99, 100, 0xdead_beef];

fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn built(b: Builder<Alloc<'static>, Full>) -> Func<Alloc<'static>> {
    match b.finish_checked() {
        Ok(func) => func,
        Err(why) => panic!("the function does not build: {why:?}"),
    }
}

/// A builder of `f(params..) -> i64` with its entry block begun, and the parameters.
fn begin(params: &[Ty]) -> (Builder<Alloc<'static>, Full>, std::vec::Vec<Value>) {
    let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc());
    let sig_params: std::vec::Vec<Param> = params.iter().map(|&ty| Param::of(ty)).collect();
    let sig = b.sig(Conv::C, Variadic::No, &sig_params, &[Param::of(Ty::I64)]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let values = params.iter().map(|&ty| b.param(ty)).collect();
    (b, values)
}

/// What a directed case asks of the tiers.
struct Case<'c> {
    what: &'c str,
    /// The tiers that must emit it (a refusal fails the test); the others' refusals are printed.
    must: &'c [TierName],
    /// Whether the evaluator can run it (no native call) and must give `want` too.
    evaluate: bool,
    /// The second argument register, the same for every run (a native function's address).
    second: u64,
}

/// `func` through every tier, placed and called on each of [`ARGS`] (as the first argument,
/// `case.second` as the second): equal to `want(p)`, and to the evaluator's answer when it runs.
/// Returns each tier's code (or its refusal) for the caller to look into.
fn agrees(case: &Case<'_>, func: &Func<Alloc<'static>>, want: &dyn Fn(u64) -> u64) -> std::vec::Vec<(TierName, Result<std::vec::Vec<u8>, TierError>)> {
    let what = case.what;
    let view = func.view();
    if let Err(why) = blaze_lir::verify(view) {
        panic!("{what}: the function does not verify: {why}");
    }
    if case.evaluate {
        for p in ARGS {
            match evaluated(view, &[Val::Int(u128::from(p))]) {
                Ok(it) => assert_eq!(it, want(p), "{what}: the evaluator's f({p:#x}) is not LIR's semantics"),
                Err(why) => panic!("{what}: the evaluator did not answer f({p:#x}): {why}"),
            }
        }
    }
    let machine = table().head().machine;
    let mut out = std::vec::Vec::new();
    let tiers: [(TierName, super::TierFn); 2] = [(TierName::Reference, reference), (TierName::Fast, fast)];
    for (name, emit) in tiers {
        let emitted = match emit(view, &Nameless, machine, alloc()) {
            Ok(emitted) => emitted,
            Err(error) => {
                assert!(!case.must.contains(&name), "{what}: the {} tier refused it: {error}", name.as_str());
                std::println!("{what}: the {} tier refuses it: {error}", name.as_str());
                out.push((name, Err(error)));
                continue;
            }
        };
        let code = emitted.code.to_vec();
        let exec = match placed(emitted, name) {
            Ok(exec) => exec,
            Err(broke) => panic!("{what}: {broke}"),
        };
        for p in ARGS {
            assert_eq!(run::call(&exec, [p, case.second, 0, 0]), want(p), "{what}: the {} tier's f({p:#x})", name.as_str());
        }
        out.push((name, Ok(code)));
    }
    out
}

const BOTH: &[TierName] = &[TierName::Reference, TierName::Fast];
const REFERENCE: &[TierName] = &[TierName::Reference];

/// The code's words.
#[cfg(target_arch = "aarch64")]
fn words(code: &[u8]) -> std::vec::Vec<u32> {
    code.chunks_exact(4).map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]])).collect()
}

/// pressure.rs's 24 locals live at once, under a 40 KiB slot: the evicted locals' cells are
/// past 32 KiB, so each spill and reload is a far template (its address in a lent scratch
/// register for a store) and the frame is probed.
/// Expected: passes, printing "directed: 24 locals over far cells agree".
#[test]
fn spills_into_far_cells_agree() {
    let (mut b, v) = begin(&[Ty::I64]);
    let p = v[0];
    let _ = b.slot(40 << 10, 3, SlotKind::Temp);
    let mut xs = std::vec::Vec::new();
    for k in 1..=24u128 {
        let c = b.iconst(Ty::I64, k);
        xs.push(b.binary(Op::Add, Flags::NONE, p, c));
    }
    let mut acc = b.binary(Op::Mul, Flags::NONE, xs[0], xs[1]);
    for &x in &xs[2..] {
        acc = b.binary(Op::Add, Flags::NONE, acc, x);
    }
    b.ret(&[acc]);
    let func = built(b);
    let want = |p: u64| {
        let mut acc = p.wrapping_add(1).wrapping_mul(p.wrapping_add(2));
        for k in 3..=24u64 {
            acc = acc.wrapping_add(p.wrapping_add(k));
        }
        acc
    };
    agrees(&Case { what: "24 locals over far cells", must: BOTH, evaluate: true, second: 0 }, &func, &want);
    std::println!("directed: 24 locals over far cells agree");
}

/// `f(p)`: a loop of ten turns over `(i, acc, x, y)`, `acc += ((x ^ i) * y) + p`, whose back edge
/// passes `(i + 1, acc, y, x)`: `x` and `y` swap, a cycle of the parallel copy, broken through
/// the spare cell; `p` is live across the back edge. The exit returns `acc ^ x`.
fn swap_loop(slot: u32) -> Func<Alloc<'static>> {
    let (mut b, v) = begin(&[Ty::I64]);
    let p = v[0];
    if slot > 0 {
        let _ = b.slot(slot, 3, SlotKind::Temp);
    }
    let head = b.create_block(BlockKind::Normal);
    let exit = b.create_block(BlockKind::Normal);
    let zero = b.iconst(Ty::I64, 0);
    let one = b.iconst(Ty::I64, 1);
    let y0 = b.binary(Op::Add, Flags::NONE, p, one);
    b.jump((head, &[zero, zero, p, y0]));
    b.begin(head);
    let i = b.param(Ty::I64);
    let acc = b.param(Ty::I64);
    let x = b.param(Ty::I64);
    let y = b.param(Ty::I64);
    let t = b.binary(Op::Xor, Flags::NONE, x, i);
    let t = b.binary(Op::Mul, Flags::NONE, t, y);
    let acc = b.binary(Op::Add, Flags::NONE, acc, t);
    let acc = b.binary(Op::Add, Flags::NONE, acc, p);
    let step = b.iconst(Ty::I64, 1);
    let i = b.binary(Op::Add, Flags::NONE, i, step);
    let ten = b.iconst(Ty::I64, 10);
    let more = b.icmp(IntCond::Ult, i, ten);
    b.br(Flags::NONE, more, (head, &[i, acc, y, x]), (exit, &[acc, x]));
    b.begin(exit);
    let r = b.param(Ty::I64);
    let xe = b.param(Ty::I64);
    let out = b.binary(Op::Xor, Flags::NONE, r, xe);
    b.ret(&[out]);
    built(b)
}

/// [`swap_loop`] at no slot, a 40 KiB slot and a 200 KiB slot, against Rust's same loop.
/// Expected: passes, printing "directed: the swapping loop agrees at 3 frames". Fails if the
/// cycle is broken through a register a template also uses, if the spare cell overlaps a slot or
/// a cell, or if a far cell's two fields are filled from one half of the offset.
#[test]
fn a_loop_swapping_values_across_its_back_edge_agrees() {
    let want = |p: u64| {
        let (mut i, mut acc, mut x, mut y) = (0u64, 0u64, p, p.wrapping_add(1));
        loop {
            acc = acc.wrapping_add((x ^ i).wrapping_mul(y)).wrapping_add(p);
            i += 1;
            if i < 10 {
                (x, y) = (y, x);
                continue;
            }
            return acc ^ x;
        }
    };
    let frames = [0u32, 40 << 10, 200 << 10];
    for slot in frames {
        let what = std::format!("the swapping loop under a {slot}-byte slot");
        agrees(&Case { what: &what, must: BOTH, evaluate: true, second: 0 }, &swap_loop(slot), &want);
    }
    std::println!("directed: the swapping loop agrees at {} frames", frames.len());
}

/// Writes every register the AAPCS64 lets a callee write, except x18 (Apple's) and the argument
/// and result registers it uses: x9 to x17 and v16 to v31 all ones. Returns `a * 3`.
#[cfg(target_arch = "aarch64")]
extern "C" fn trash(a: u64) -> u64 {
    // SAFETY: the block writes only the registers it names as outputs, all caller-saved; it
    // reads and writes no memory and touches no stack.
    unsafe {
        core::arch::asm!(
            "mov x9, #-1", "mov x10, #-1", "mov x11, #-1", "mov x12, #-1", "mov x13, #-1",
            "mov x14, #-1", "mov x15, #-1", "mov x16, #-1", "mov x17, #-1",
            "movi v16.2d, #0xffffffffffffffff", "movi v17.2d, #0xffffffffffffffff",
            "movi v18.2d, #0xffffffffffffffff", "movi v19.2d, #0xffffffffffffffff",
            "movi v20.2d, #0xffffffffffffffff", "movi v21.2d, #0xffffffffffffffff",
            "movi v22.2d, #0xffffffffffffffff", "movi v23.2d, #0xffffffffffffffff",
            "movi v24.2d, #0xffffffffffffffff", "movi v25.2d, #0xffffffffffffffff",
            "movi v26.2d, #0xffffffffffffffff", "movi v27.2d, #0xffffffffffffffff",
            "movi v28.2d, #0xffffffffffffffff", "movi v29.2d, #0xffffffffffffffff",
            "movi v30.2d, #0xffffffffffffffff", "movi v31.2d, #0xffffffffffffffff",
            out("x9") _, out("x10") _, out("x11") _, out("x12") _, out("x13") _, out("x14") _,
            out("x15") _, out("x16") _, out("x17") _,
            out("v16") _, out("v17") _, out("v18") _, out("v19") _, out("v20") _, out("v21") _,
            out("v22") _, out("v23") _, out("v24") _, out("v25") _, out("v26") _, out("v27") _,
            out("v28") _, out("v29") _, out("v30") _, out("v31") _,
            options(nomem, nostack, preserves_flags),
        );
    }
    a.wrapping_mul(3)
}

/// `f(p, g)`: `n` values `h_k = p + k` made first (k = 1..=n), then, in a second block when
/// `across` (so each is a home, a register or a cell), `r = g(g(p))` through the pointer, and
/// `r + sum((h_k ^ k))`: every `h_k` is live across both calls.
fn calls(n: u64, across: bool) -> Func<Alloc<'static>> {
    let (mut b, v) = begin(&[Ty::I64, Ty::PTR]);
    let (p, g) = (v[0], v[1]);
    let callee = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
    let mut h = std::vec::Vec::new();
    for k in 1..=n {
        let c = b.iconst(Ty::I64, u128::from(k));
        h.push(b.binary(Op::Add, Flags::NONE, p, c));
    }
    if across {
        let next = b.create_block(BlockKind::Normal);
        b.jump((next, &[]));
        b.begin(next);
    }
    let r = b.call_indirect(Flags::NONE, g, callee, &[p]);
    let mut acc = b.call_indirect(Flags::NONE, g, callee, &[r]);
    for (k, &x) in h.iter().enumerate() {
        let c = b.iconst(Ty::I64, k as u128 + 1);
        let t = b.binary(Op::Xor, Flags::NONE, x, c);
        acc = b.binary(Op::Add, Flags::NONE, acc, t);
    }
    b.ret(&[acc]);
    built(b)
}

/// [`calls`] with 10 block-local values (in x9 to x15 and cells: the call's spills and reloads)
/// and with 22 values live across a block boundary (homes in callee-saved registers and cells;
/// the callee `g` may come from a cell, through a held scratch register the call spends), each
/// calling [`trash`]. The evaluator runs no native function, so the answer is Rust's.
/// Expected: passes, printing "directed: calls keep live values at 2 pressures" and, per
/// variant, whether the Fast tier's callee came through x16. Fails if a caller-saved value is
/// not spilled across the call, or a held callee overlaps a spill's address.
#[cfg(target_arch = "aarch64")]
#[test]
fn calls_keep_values_live_in_caller_saved_registers() {
    let g = (trash as extern "C" fn(u64) -> u64) as usize as u64;
    let blr_x16 = 0xd63f_0200u32;
    for (n, across) in [(10u64, false), (22, true)] {
        let want = move |p: u64| {
            let mut acc = p.wrapping_mul(9);
            for k in 1..=n {
                acc = acc.wrapping_add(p.wrapping_add(k) ^ k);
            }
            acc
        };
        let what = std::format!("two calls with {n} values live (across a block: {across})");
        let tiers = agrees(&Case { what: &what, must: BOTH, evaluate: false, second: g }, &calls(n, across), &want);
        for (name, code) in tiers {
            if let (TierName::Fast, Ok(code)) = (name, code) {
                std::println!("{what}: the Fast tier's callee through x16: {}", words(&code).contains(&blr_x16));
            }
        }
    }
    std::println!("directed: calls keep live values at 2 pressures");
}

/// `f(p)`: a flag `p < 100` and `h_k = p + k` (k = 1..=20) made in the entry block; `h_1..h_8`
/// stored 4 KiB apart into a slot of `big` bytes and `h_9..h_16` into an 8-word slot after it
/// (past the near `add x, sp, #imm` when `big` > 4 KiB); then, in a second block, every word
/// loaded back and summed, `xor` every `h_k`, and the flag chooses the result or it `^ 0xff`.
/// The flag and the `h_k` are live across the stores and the block boundary (homes and cells,
/// all cells past 32 KiB when `big` is).
fn flag_across_stores(big: u32) -> Func<Alloc<'static>> {
    let (mut b, v) = begin(&[Ty::I64]);
    let p = v[0];
    let far = b.slot(big, 3, SlotKind::Temp);
    let near = b.slot(64, 3, SlotKind::Temp);
    let hundred = b.iconst(Ty::I64, 100);
    let flag = b.icmp(IntCond::Ult, p, hundred);
    let mut h = std::vec::Vec::new();
    for k in 1..=20u128 {
        let c = b.iconst(Ty::I64, k);
        h.push(b.binary(Op::Add, Flags::NONE, p, c));
    }
    let a = b.slot_addr(far);
    let c = b.slot_addr(near);
    for k in 0..8usize {
        b.store(Flags::NONE, h[k], a, (k as i32) * 4096, MemAttrs::aligned(3));
        b.store(Flags::NONE, h[k + 8], c, (k as i32) * 8, MemAttrs::aligned(3));
    }
    let mid = b.create_block(BlockKind::Normal);
    let yes = b.create_block(BlockKind::Normal);
    let no = b.create_block(BlockKind::Normal);
    b.jump((mid, &[]));
    b.begin(mid);
    let mut sum = b.iconst(Ty::I64, 0);
    for k in 0..8i32 {
        let x = b.load(Ty::I64, Flags::NONE, a, k * 4096, MemAttrs::aligned(3));
        sum = b.binary(Op::Add, Flags::NONE, sum, x);
        let y = b.load(Ty::I64, Flags::NONE, c, k * 8, MemAttrs::aligned(3));
        sum = b.binary(Op::Add, Flags::NONE, sum, y);
    }
    for &x in &h {
        sum = b.binary(Op::Xor, Flags::NONE, sum, x);
    }
    b.br(Flags::NONE, flag, (yes, &[sum]), (no, &[sum]));
    b.begin(yes);
    let r = b.param(Ty::I64);
    b.ret(&[r]);
    b.begin(no);
    let r = b.param(Ty::I64);
    let mask = b.iconst(Ty::I64, 0xff);
    let r = b.binary(Op::Xor, Flags::NONE, r, mask);
    b.ret(&[r]);
    built(b)
}

/// [`flag_across_stores`] with a 40 KiB and a 200 KiB slot: the frame past 32 KiB, every cell
/// far, the second slot's address far, the frame probed.
/// Expected: passes, printing "directed: a flag across far stores agrees at 2 frames". Fails if
/// a far store's address temporary is a register holding the flag or a value (the x12 class).
#[test]
fn a_far_frame_with_a_flag_live_across_stores_agrees() {
    let want = |p: u64| {
        let h: std::vec::Vec<u64> = (1..=20u64).map(|k| p.wrapping_add(k)).collect();
        let mut sum = h[..16].iter().fold(0u64, |s, &x| s.wrapping_add(x));
        for &x in &h {
            sum ^= x;
        }
        if p < 100 { sum } else { sum ^ 0xff }
    };
    let frames = [40u32 << 10, 200 << 10];
    for big in frames {
        let what = std::format!("a flag across stores under a {big}-byte slot");
        agrees(&Case { what: &what, must: BOTH, evaluate: true, second: 0 }, &flag_across_stores(big), &want);
    }
    std::println!("directed: a flag across far stores agrees at {} frames", frames.len());
}

/// `f(p)`: `(s, o) = sadd_overflow(p, p)`, `s` stored at byte 28672 of a 40 KiB slot (the far
/// frame), loaded back, and `loaded + 1000 * zext(o)` returned: the overflow flag is live across
/// the far store (int_tests.rs `far_cells_keep_overflow_flags` is the Reference tier's own).
/// Expected: passes, printing the Fast tier's refusal by name if it refuses (no compound value
/// in the Fast tier), and "directed: a checked flag across a far store agrees". Fails if a tier
/// that emits it returns another value than LIR's.
#[test]
fn a_checked_flag_live_across_a_far_store_agrees() {
    let (mut b, v) = begin(&[Ty::I64]);
    let p = v[0];
    let s = b.slot(40 << 10, 3, SlotKind::Temp);
    let o = b.overflow(Op::SaddOverflow, p, p);
    let sum = b.project(o, 0);
    let flag = b.project(o, 1);
    let a = b.slot_addr(s);
    b.store(Flags::NONE, sum, a, 28672, MemAttrs::aligned(3));
    let back = b.load(Ty::I64, Flags::NONE, a, 28672, MemAttrs::aligned(3));
    let wide = b.convert(Op::Zext, Flags::NONE, Ty::I64, flag);
    let k = b.iconst(Ty::I64, 1000);
    let scaled = b.binary(Op::Mul, Flags::NONE, wide, k);
    let out = b.binary(Op::Add, Flags::NONE, back, scaled);
    b.ret(&[out]);
    let func = built(b);
    let want = |p: u64| {
        let (s, o) = (p as i64).overflowing_add(p as i64);
        (s as u64).wrapping_add(1000 * u64::from(o))
    };
    agrees(&Case { what: "a checked flag across a far store", must: REFERENCE, evaluate: true, second: 0 }, &func, &want);
    std::println!("directed: a checked flag across a far store agrees");
}

const OVERFLOWS: [Op; 6] = [Op::UaddOverflow, Op::SaddOverflow, Op::UsubOverflow, Op::SsubOverflow, Op::UmulOverflow, Op::SmulOverflow];

/// Rust's `overflowing_*` of `op` at `w` bits over `a` and `b` truncated to `w`: the wrapped
/// value zero-extended, and whether it overflowed.
fn overflowing(op: Op, w: u32, a: u64, b: u64) -> (u64, bool) {
    macro_rules! at {
        ($u:ty, $s:ty) => {
            match op {
                Op::UaddOverflow => {
                    let (v, o) = (a as $u).overflowing_add(b as $u);
                    (v as u64, o)
                }
                Op::UsubOverflow => {
                    let (v, o) = (a as $u).overflowing_sub(b as $u);
                    (v as u64, o)
                }
                Op::UmulOverflow => {
                    let (v, o) = (a as $u).overflowing_mul(b as $u);
                    (v as u64, o)
                }
                Op::SaddOverflow => {
                    let (v, o) = (a as $s).overflowing_add(b as $s);
                    (v as $u as u64, o)
                }
                Op::SsubOverflow => {
                    let (v, o) = (a as $s).overflowing_sub(b as $s);
                    (v as $u as u64, o)
                }
                _ => {
                    let (v, o) = (a as $s).overflowing_mul(b as $s);
                    (v as $u as u64, o)
                }
            }
        };
    }
    match w {
        8 => at!(u8, i8),
        16 => at!(u16, i16),
        32 => at!(u32, i32),
        _ => at!(u64, i64),
    }
}

/// The second operand of [`overflow_pair`]: `p ^ SECOND_XOR`, so the six arguments give sums,
/// differences and products both in range and past it at every width.
const SECOND_XOR: u64 = 0x7fff_ff80_7f80_ff85;

/// `f(p) = v + 1000 * flag` where `(v, flag) = op(trunc_w(p), trunc_w(p ^ SECOND_XOR))`, both
/// members read by `project` (the Fast tier's pair in its cell, emit.rs `overflow`).
fn overflow_pair(op: Op, w: u32) -> Func<Alloc<'static>> {
    let (mut b, v) = begin(&[Ty::I64]);
    let p = v[0];
    let k = b.iconst(Ty::I64, u128::from(SECOND_XOR));
    let q = b.binary(Op::Xor, Flags::NONE, p, k);
    let ty = Ty::int(w).expect("a width LIR has");
    let (x, y) = match w {
        64 => (p, q),
        _ => (b.convert(Op::Trunc, Flags::NONE, ty, p), b.convert(Op::Trunc, Flags::NONE, ty, q)),
    };
    let pair = b.overflow(op, x, y);
    let value = b.project(pair, 0);
    let flag = b.project(pair, 1);
    let value = match w {
        64 => value,
        _ => b.convert(Op::Zext, Flags::NONE, Ty::I64, value),
    };
    let flag = b.convert(Op::Zext, Flags::NONE, Ty::I64, flag);
    let thousand = b.iconst(Ty::I64, 1000);
    let flag = b.binary(Op::Mul, Flags::NONE, flag, thousand);
    let out = b.binary(Op::Add, Flags::NONE, value, flag);
    b.ret(&[out]);
    built(b)
}

/// Every overflow operation at 8, 16, 32 and 64 bits ([`overflow_pair`]) against Rust's
/// `overflowing_*`, by both tiers and the evaluator (D197 root 6 (d), the `project` and
/// overflow class). The Fast tier must emit each whose flag template the committed table holds
/// (table_gen arm64.rs `overflows`); before the table is regenerated with them it refuses
/// naming the missing pattern, printed.
/// Expected: passes, printing "directed: 24 overflow pairs agree (F at the Fast tier)" with F
/// 24 once the table holds the flag templates. Fails if a flag is wrong at a width (a narrow
/// operand read past its bits), or a member is read from the wrong half of the cell.
#[test]
fn overflow_pairs_agree() {
    let gpr = blaze_tier::Form::Reg(RegClass::Gpr);
    let none = blaze_tier::Form::None;
    let mut fast_ran = 0;
    let mut cases = 0;
    for op in OVERFLOWS {
        for w in [8u32, 16, 32, 64] {
            let flag = Pattern { op, width: w as u8, variant: 0, result: gpr, operands: [gpr, gpr, none, none], inner: None };
            let tabled = table().find(&flag).is_some();
            let must = if tabled { BOTH } else { REFERENCE };
            let what = std::format!("{} at {w} bits", op.name());
            let want = |p: u64| {
                let (v, o) = overflowing(op, w, p, p ^ SECOND_XOR);
                v.wrapping_add(1000 * u64::from(o))
            };
            let out = agrees(&Case { what: &what, must, evaluate: true, second: 0 }, &overflow_pair(op, w), &want);
            fast_ran += out.iter().filter(|(name, made)| *name == TierName::Fast && made.is_ok()).count();
            cases += 1;
        }
    }
    std::println!("directed: {cases} overflow pairs agree ({fast_ran} at the Fast tier)");
}

/// `movz x16, #0`.
const MOVZ_X16_0: u32 = 0xd280_0010;

/// The control's table: the general cell store (`glue::cell_store(Gpr)`, `str x, [sp, #off]`)
/// served as `movz x16, #0; str x, [sp, #off]`, its fields moved one word on, and x16 in its
/// clobbers when `declared`. Every other pattern is `inner`'s.
struct ClobbersX16<'t> {
    inner: &'t EncodedTable<'static>,
    declared: bool,
    bytes: AVec<'static, u8>,
    fields: AVec<'static, Field>,
}

impl<'t> ClobbersX16<'t> {
    fn over(inner: &'t EncodedTable<'static>, declared: bool) -> ClobbersX16<'t> {
        let Some(t) = inner.find(&glue::cell_store(RegClass::Gpr)) else {
            panic!("the committed table has no general cell store");
        };
        let mut bytes: AVec<'static, u8> = Vec::new_in(alloc());
        bytes.extend_from_slice(&MOVZ_X16_0.to_le_bytes());
        bytes.extend_from_slice(t.bytes);
        let mut fields: AVec<'static, Field> = Vec::new_in(alloc());
        for field in t.fields {
            assert!(!matches!(field.kind, blaze_tier::FieldKind::Branch { .. } | blaze_tier::FieldKind::Site(_)), "a cell store with a branch or a site");
            fields.push(Field { at: field.at + 32, ..*field });
        }
        ClobbersX16 { inner, declared, bytes, fields }
    }
}

impl Table for ClobbersX16<'_> {
    fn head(&self) -> TableHead {
        self.inner.head()
    }

    fn find(&self, pattern: &Pattern) -> Option<Template<'_>> {
        let template = self.inner.find(pattern)?;
        if *pattern != glue::cell_store(RegClass::Gpr) {
            return Some(template);
        }
        let clobbers = match self.declared {
            true => template.clobbers.with(Reg(16)),
            false => template.clobbers,
        };
        Some(Template { bytes: &self.bytes[..], fields: &self.fields[..], clobbers, ..template })
    }
}

/// `f(p) = small[p & 7] + wide[p & 1]` over two rows of the function's own data: `small` (8
/// bytes, byte-aligned) the Fast tier places in the code part after the instructions, `wide`
/// (two words, 8-aligned) in the relro part (emit.rs `place_rows`).
fn own_rows() -> Func<Alloc<'static>> {
    let (mut b, v) = begin(&[Ty::I64]);
    let p = v[0];
    let small = b.data(&SMALL, 0, &[]);
    let mut wide_bytes = [0u8; 16];
    wide_bytes[..8].copy_from_slice(&WIDE[0].to_le_bytes());
    wide_bytes[8..].copy_from_slice(&WIDE[1].to_le_bytes());
    let wide = b.data(&wide_bytes, 3, &[]);
    let seven = b.iconst(Ty::I64, 7);
    let at = b.binary(Op::And, Flags::NONE, p, seven);
    let base = b.data_addr(small);
    let byte_at = b.ptr_add(Flags::NONE, base, at);
    let byte = b.load(Ty::I8, Flags::NONE, byte_at, 0, MemAttrs::aligned(0));
    let byte = b.convert(Op::Zext, Flags::NONE, Ty::I64, byte);
    let one = b.iconst(Ty::I64, 1);
    let odd = b.binary(Op::And, Flags::NONE, p, one);
    let three = b.iconst(Ty::I64, 3);
    let word_off = b.binary(Op::Shl, Flags::NONE, odd, three);
    let words = b.data_addr(wide);
    let word_at = b.ptr_add(Flags::NONE, words, word_off);
    let word = b.load(Ty::I64, Flags::NONE, word_at, 0, MemAttrs::aligned(3));
    let out = b.binary(Op::Add, Flags::NONE, byte, word);
    b.ret(&[out]);
    built(b)
}

const SMALL: [u8; 8] = [3, 1, 4, 1, 5, 9, 2, 6];
const WIDE: [u64; 2] = [0x1111, 0x2222_0000];
const TARGET: [u8; 8] = [10, 20, 30, 40, 50, 60, 70, 80];

/// `f(p) = (*pointer)[p & 3]`: `pointer` is an 8-aligned row holding `&target + 2`, `target` a
/// byte row; the Fast tier writes the address as a relro word naming the record's own code part
/// (`OWN_CODE`, addend the row's offset plus 2), the placer fills it.
fn own_address_in_data() -> Func<Alloc<'static>> {
    let (mut b, v) = begin(&[Ty::I64]);
    let p = v[0];
    let target = b.data(&TARGET, 0, &[]);
    let pointer = b.data(&[0u8; 8], 3, &[DataReloc { offset: 0, target, addend: 2 }]);
    let at_pointer = b.data_addr(pointer);
    let base = b.load(Ty::PTR, Flags::NONE, at_pointer, 0, MemAttrs::aligned(3));
    let three = b.iconst(Ty::I64, 3);
    let at = b.binary(Op::And, Flags::NONE, p, three);
    let byte_at = b.ptr_add(Flags::NONE, base, at);
    let byte = b.load(Ty::I8, Flags::NONE, byte_at, 0, MemAttrs::aligned(0));
    let out = b.convert(Op::Zext, Flags::NONE, Ty::I64, byte);
    b.ret(&[out]);
    built(b)
}

/// The function's own data at the Fast tier (D197 root 6 (d), T1's own `data_addr`): a code-part
/// row, a relro-part row and a row holding another row's address, each run by both tiers and the
/// evaluator.
/// Expected: passes, printing "directed: own data agrees at 2 functions". Fails if a site's
/// relocation names the wrong part or offset, if a row is placed misaligned, or if the word's
/// addend misses the target row's offset.
#[test]
fn own_data_agrees() {
    let want_rows = |p: u64| u64::from(SMALL[(p & 7) as usize]) + WIDE[(p & 1) as usize];
    agrees(&Case { what: "two rows of own data", must: BOTH, evaluate: true, second: 0 }, &own_rows(), &want_rows);
    let want_inner = |p: u64| u64::from(TARGET[2 + (p & 3) as usize]);
    agrees(&Case { what: "an own row's address in own data", must: BOTH, evaluate: true, second: 0 }, &own_address_in_data(), &want_inner);
    std::println!("directed: own data agrees at 2 functions");
}

/// What [`memory_grid`] returns for `p`: each value it reads back, folded `acc * 31 + x`.
fn memory_grid_want(p: u64) -> u64 {
    let bits32 = u64::from((p as u32) ^ 0x4228_0000);
    let reads = [
        p & 0xff,
        p.wrapping_add(3) & 0xff,
        p.wrapping_add(5) & 0xffff,
        p.wrapping_add(9) & 0xffff_ffff,
        p.wrapping_add(11),
        p.wrapping_add(13) & 0xffff,
        bits32,
        bits32,
        p ^ 0x4045_0000_0000_0000,
        p.wrapping_add(11),
    ];
    reads.iter().fold(0u64, |acc, &x| acc.wrapping_mul(31).wrapping_add(x))
}

/// `f(p)`: one store and one load of every row of the access grid (table_gen/arm64.rs
/// `ACCESSES` times its address forms), each value read back and folded `acc * 31 + x`:
/// - an `i8` at an unscaled negative offset (no `i1`: memory holds whole bytes, and every tier
///   and the evaluator refuse an `i1` access by that rule);
/// - an `i8`, `i16`, `i32` and `i64` at offsets past both immediate ranges of an 8 KiB slot
///   (the offset in a register);
/// - an `i16` stored and loaded through a fused `ptr_add` with a 64-bit index (`[rn, rm]`);
/// - an `f32`: its bits stored as an `i32`, loaded as an `f32` (scaled), stored at a register
///   offset, loaded back as an `i32`; loaded again as an `f32` at the register offset, stored
///   at a scaled one and read as an `i32`;
/// - an `f64`: its bits loaded as an `f64` (scaled), stored at an unscaled negative offset, read
///   as an `i64`; and the `i64` above loaded as an `f64` at a register offset, stored scaled.
///
/// No float value is a NaN (each exponent is fixed or the bits are small), so the bits survive.
fn memory_grid() -> Func<Alloc<'static>> {
    let (mut b, v) = begin(&[Ty::I64]);
    let p = v[0];
    let small = b.slot(96, 3, SlotKind::Temp);
    let big = b.slot(8 << 10, 3, SlotKind::Temp);
    let any = MemAttrs::aligned(0);
    let s = b.slot_addr(small);
    let g = b.slot_addr(big);
    let k64 = b.iconst(Ty::I64, 64);
    let mid = b.ptr_add(Flags::NONE, s, k64);
    let k40 = b.iconst(Ty::I64, 40);
    let base = b.ptr_add(Flags::NONE, s, k40);
    let seven = b.iconst(Ty::I64, 7);
    let idx = b.binary(Op::And, Flags::NONE, p, seven);
    let plus = |b: &mut Builder<Alloc<'static>, Full>, k: u128, ty: Ty| {
        let c = b.iconst(Ty::I64, k);
        let x = b.binary(Op::Add, Flags::NONE, p, c);
        match ty == Ty::I64 {
            true => x,
            false => b.convert(Op::Trunc, Flags::NONE, ty, x),
        }
    };

    let byte = b.convert(Op::Trunc, Flags::NONE, Ty::I8, p);
    b.store(Flags::NONE, byte, mid, -3, any);
    let c8 = plus(&mut b, 3, Ty::I8);
    b.store(Flags::NONE, c8, g, 5001, any);
    let h16 = plus(&mut b, 5, Ty::I16);
    b.store(Flags::NONE, h16, g, 6003, any);
    let w32 = plus(&mut b, 9, Ty::I32);
    b.store(Flags::NONE, w32, g, 7001, any);
    let x64 = plus(&mut b, 11, Ty::I64);
    b.store(Flags::NONE, x64, g, 7501, any);
    let h2 = plus(&mut b, 13, Ty::I16);
    let at = b.ptr_add(Flags::NONE, base, idx);
    b.store(Flags::NONE, h2, at, 0, any);

    let low = b.convert(Op::Trunc, Flags::NONE, Ty::I32, p);
    let exp32 = b.iconst(Ty::I32, 0x4228_0000);
    let bits32 = b.binary(Op::Xor, Flags::NONE, low, exp32);
    b.store(Flags::NONE, bits32, s, 8, any);
    let exp64 = b.iconst(Ty::I64, 0x4045_0000_0000_0000);
    let bits64 = b.binary(Op::Xor, Flags::NONE, p, exp64);
    b.store(Flags::NONE, bits64, s, 16, any);
    let f = b.load(Ty::F32, Flags::NONE, s, 8, any);
    b.store(Flags::NONE, f, g, 4001, any);
    let f_again = b.load(Ty::F32, Flags::NONE, g, 4001, any);
    b.store(Flags::NONE, f_again, s, 12, any);
    let d = b.load(Ty::F64, Flags::NONE, s, 16, any);
    b.store(Flags::NONE, d, mid, -40, any);
    let d_wide = b.load(Ty::F64, Flags::NONE, g, 7501, any);
    b.store(Flags::NONE, d_wide, s, 80, any);

    let zext = |b: &mut Builder<Alloc<'static>, Full>, x: Value| b.convert(Op::Zext, Flags::NONE, Ty::I64, x);
    let mut reads = std::vec::Vec::new();
    let x = b.load(Ty::I8, Flags::NONE, mid, -3, any);
    reads.push(zext(&mut b, x));
    let x = b.load(Ty::I8, Flags::NONE, g, 5001, any);
    reads.push(zext(&mut b, x));
    let x = b.load(Ty::I16, Flags::NONE, g, 6003, any);
    reads.push(zext(&mut b, x));
    let x = b.load(Ty::I32, Flags::NONE, g, 7001, any);
    reads.push(zext(&mut b, x));
    reads.push(b.load(Ty::I64, Flags::NONE, g, 7501, any));
    let at_again = b.ptr_add(Flags::NONE, base, idx);
    let x = b.load(Ty::I16, Flags::NONE, at_again, 0, any);
    reads.push(zext(&mut b, x));
    let x = b.load(Ty::I32, Flags::NONE, g, 4001, any);
    reads.push(zext(&mut b, x));
    let x = b.load(Ty::I32, Flags::NONE, s, 12, any);
    reads.push(zext(&mut b, x));
    reads.push(b.load(Ty::I64, Flags::NONE, s, 24, any));
    reads.push(b.load(Ty::I64, Flags::NONE, s, 80, any));

    let mut acc = b.iconst(Ty::I64, 0);
    let k31 = b.iconst(Ty::I64, 31);
    for x in reads {
        let m = b.binary(Op::Mul, Flags::NONE, acc, k31);
        acc = b.binary(Op::Add, Flags::NONE, m, x);
    }
    b.ret(&[acc]);
    built(b)
}

/// [`memory_grid`] through both tiers and the evaluator: every load and store template of the
/// grid (each width and class at each address form) runs at least once.
/// Expected: passes, printing "directed: the access grid agrees". Fails with the Fast tier's
/// "no template for `load`/`store`" naming the width if a row of the grid is missing from the
/// committed table, or with a wrong value if a template's size, class or address is wrong.
#[test]
fn the_access_grid_agrees() {
    agrees(&Case { what: "the access grid", must: BOTH, evaluate: true, second: 0 }, &memory_grid(), &memory_grid_want);
    std::println!("directed: the access grid agrees");
}

const BIT_OPS: [Op; 5] = [Op::Clz, Op::Ctz, Op::Popcnt, Op::Bswap, Op::Bitrev];
const BIT_WIDTHS: [u32; 5] = [1, 8, 16, 32, 64];

/// LIR's `op` (blaze-lir arith.rs `unary`) of `p`'s low `w` bits.
fn bit_op(op: Op, w: u32, p: u64) -> u64 {
    let x = match w {
        64 => p,
        _ => p & ((1u64 << w) - 1),
    };
    let spare = 64 - w;
    match op {
        Op::Clz => u64::from(x.leading_zeros() - spare),
        Op::Ctz if x == 0 => u64::from(w),
        Op::Ctz => u64::from(x.trailing_zeros()),
        Op::Popcnt => u64::from(x.count_ones()),
        Op::Bswap if w < 16 => x,
        Op::Bswap => x.swap_bytes() >> spare,
        _ => x.reverse_bits() >> spare,
    }
}

/// `f(p) = zext(op(trunc_w(p ^ SECOND_XOR)))`: the operand's bits vary over the width, so a
/// count off by the spare bits or a swap at the wrong width shows.
fn bit_function(op: Op, w: u32) -> Func<Alloc<'static>> {
    let (mut b, v) = begin(&[Ty::I64]);
    let k = b.iconst(Ty::I64, u128::from(SECOND_XOR));
    let x = b.binary(Op::Xor, Flags::NONE, v[0], k);
    let Some(ty) = Ty::int(w) else { panic!("no integer type of {w} bits") };
    let x = match w {
        64 => x,
        _ => b.convert(Op::Trunc, Flags::NONE, ty, x),
    };
    let r = b.unary(op, Flags::NONE, x);
    let r = match w {
        64 => r,
        _ => b.convert(Op::Zext, Flags::NONE, Ty::I64, r),
    };
    b.ret(&[r]);
    built(b)
}

/// `clz`, `ctz`, `popcnt`, `bswap` and `bitrev` at 1, 8, 16, 32 and 64 bits through both tiers
/// and the evaluator (`popcnt` counts in v31, declared in its template's clobbers).
/// Expected: passes, printing "directed: 25 bit operations agree". Fails with the Fast tier's
/// "no template for `clz`" (or another) if the committed table lacks one, or with a wrong count.
#[test]
fn bit_operations_agree() {
    let mut n = 0;
    for op in BIT_OPS {
        for w in BIT_WIDTHS {
            let what = std::format!("`{}` at {w} bits", op.name());
            let want = move |p: u64| bit_op(op, w, p ^ SECOND_XOR);
            agrees(&Case { what: &what, must: BOTH, evaluate: true, second: 0 }, &bit_function(op, w), &want);
            n += 1;
        }
    }
    std::println!("directed: {n} bit operations agree");
}

/// `f(p)`: a slot's address exposed and taken back (`expose`, `from_exposed`), `p` stored
/// through the pointer taken back; `p + 1` stored through `ptr_with_addr(slot, ptr_addr(slot) +
/// 8)`; both words loaded, folded `x * 31 + y`, plus `expose - ptr_addr` (0).
fn provenance_moves() -> Func<Alloc<'static>> {
    let (mut b, v) = begin(&[Ty::I64]);
    let p = v[0];
    let slot = b.slot(16, 3, SlotKind::Temp);
    let s = b.slot_addr(slot);
    let any = MemAttrs::aligned(3);
    let exposed = b.convert(Op::Expose, Flags::NONE, Ty::I64, s);
    let back = b.convert(Op::FromExposed, Flags::NONE, Ty::PTR, exposed);
    b.store(Flags::NONE, p, back, 0, any);
    let addr = b.convert(Op::PtrAddr, Flags::NONE, Ty::I64, s);
    let eight = b.iconst(Ty::I64, 8);
    let next = b.binary(Op::Add, Flags::NONE, addr, eight);
    let second = b.ptr_with_addr(s, next);
    let one = b.iconst(Ty::I64, 1);
    let p1 = b.binary(Op::Add, Flags::NONE, p, one);
    b.store(Flags::NONE, p1, second, 0, any);
    let x = b.load(Ty::I64, Flags::NONE, s, 0, any);
    let y = b.load(Ty::I64, Flags::NONE, s, 8, any);
    let k31 = b.iconst(Ty::I64, 31);
    let m = b.binary(Op::Mul, Flags::NONE, x, k31);
    let sum = b.binary(Op::Add, Flags::NONE, m, y);
    let zero = b.binary(Op::Sub, Flags::NONE, exposed, addr);
    let out = b.binary(Op::Add, Flags::NONE, sum, zero);
    b.ret(&[out]);
    built(b)
}

/// The atomics' operands, `p + c` truncated: the first store's, each `atomic_rmw`'s (in
/// `AtomicRmw::ALL`'s order) and the failing `cmpxchg`'s new value.
const ATOMIC_START: u64 = 0x5a5a_1234_9876_0f0f;
const ATOMIC_OPERANDS: [u64; 11] = [
    0x0123_4567_89ab_cdef,
    0x8000_0000_8000_8081,
    0x0f0f_0f0f_0f0f_0f0f,
    0xfedc_ba98_7654_3210,
    0x7fff_ffff_7fff_7f7f,
    0x1111_2222_4444_8888,
    0xdead_beef_cafe_f00d,
    0x8765_4321_8765_4321,
    0x0000_0000_0000_0003,
    0xffff_ffff_ffff_fff0,
    0x4000_0000_4000_4040,
];
const ATOMIC_OTHER: u64 = 0x3c3c_c3c3_3c3c_c3c3;

/// What `rmw` stores of `old` and `v`, both the low `w` bits (zero-extended).
fn rmw_stores(rmw: blaze_lir::AtomicRmw, w: u32, old: u64, v: u64) -> u64 {
    use blaze_lir::AtomicRmw as R;
    let m = if w == 64 { u64::MAX } else { (1u64 << w) - 1 };
    let signed = |x: u64| ((x << (64 - w)) as i64) >> (64 - w);
    let out = match rmw {
        R::Xchg => v,
        R::Add => old.wrapping_add(v),
        R::Sub => old.wrapping_sub(v),
        R::And => old & v,
        R::Nand => !(old & v),
        R::Or => old | v,
        R::Xor => old ^ v,
        R::Max => if signed(old) > signed(v) { old } else { v },
        R::Min => if signed(old) < signed(v) { old } else { v },
        R::Umax => old.max(v),
        R::Umin => old.min(v),
    };
    out & m
}

/// What [`atomics_at`] returns for `p`: the slot's history folded `acc * 31 + x`.
fn atomics_want(w: u32, p: u64) -> u64 {
    let m = if w == 64 { u64::MAX } else { (1u64 << w) - 1 };
    let val = |c: u64| p.wrapping_add(c) & m;
    let fold = |acc: u64, x: u64| acc.wrapping_mul(31).wrapping_add(x);
    let mut mem = val(ATOMIC_START);
    let mut acc = 0u64;
    for (k, rmw) in blaze_lir::AtomicRmw::ALL.into_iter().enumerate() {
        acc = fold(acc, mem);
        mem = rmw_stores(rmw, w, mem, val(ATOMIC_OPERANDS[k]));
    }
    // The first `cmpxchg` expects what the slot holds: it stores the start value.
    let cur = mem;
    acc = fold(fold(acc, cur), 1);
    mem = val(ATOMIC_START);
    // The second expects `cur` again: it stores only if the start value is `cur`.
    let ok = mem == cur;
    acc = fold(fold(acc, mem), u64::from(ok));
    if ok {
        mem = val(ATOMIC_OTHER);
    }
    fold(acc, mem)
}

/// `f(p)` over an 8-byte slot at `w` bits: a relaxed and a release `atomic_store`, every
/// `atomic_rmw` kind (orderings cycling through all five), three fences (seq_cst, acquire, a
/// single-thread one), an acquire `atomic_load`, a `cmpxchg` that succeeds (acq_rel/acquire)
/// and one that fails unless the values meet (relaxed/relaxed, weak), and a final relaxed
/// load; every value read folded `acc * 31 + x`.
fn atomics_at(w: u32) -> Func<Alloc<'static>> {
    use blaze_lir::{AtomicRmw, Ordering};
    let (mut b, v) = begin(&[Ty::I64]);
    let p = v[0];
    let Some(ty) = Ty::int(w) else { panic!("no integer type of {w} bits") };
    let at = MemAttrs::aligned((w / 8).trailing_zeros() as u8);
    let slot = b.slot(8, 3, SlotKind::Temp);
    let s = b.slot_addr(slot);
    let val = |b: &mut Builder<Alloc<'static>, Full>, c: u64| {
        let k = b.iconst(Ty::I64, u128::from(c));
        let x = b.binary(Op::Add, Flags::NONE, p, k);
        match w {
            64 => x,
            _ => b.convert(Op::Trunc, Flags::NONE, ty, x),
        }
    };
    let widen = |b: &mut Builder<Alloc<'static>, Full>, x: Value| match b.ty(x) == Ty::I64 {
        true => x,
        false => b.convert(Op::Zext, Flags::NONE, Ty::I64, x),
    };
    let ordered = |o: Ordering| Flags::NONE.with_ordering(o);

    let other = val(&mut b, ATOMIC_OTHER);
    b.atomic_store(ordered(Ordering::Relaxed), other, s, at);
    let start = val(&mut b, ATOMIC_START);
    b.atomic_store(ordered(Ordering::Release), start, s, at);
    let mut reads = std::vec::Vec::new();
    for (k, rmw) in AtomicRmw::ALL.into_iter().enumerate() {
        let x = val(&mut b, ATOMIC_OPERANDS[k]);
        let old = b.atomic_rmw(rmw, ordered(Ordering::ALL[k % Ordering::ALL.len()]), s, x, at);
        reads.push(widen(&mut b, old));
    }
    b.fence(ordered(Ordering::SeqCst));
    b.fence(ordered(Ordering::Acquire));
    b.fence(ordered(Ordering::SeqCst).union(Flags::SINGLE_THREAD));
    let cur = b.atomic_load(ty, ordered(Ordering::Acquire), s, at);
    let start = val(&mut b, ATOMIC_START);
    let pair = b.cmpxchg(ordered(Ordering::AcqRel).with_failure(Ordering::Acquire), s, cur, start, at);
    for k in 0..2 {
        let member = b.project(pair, k);
        reads.push(widen(&mut b, member));
    }
    let other = val(&mut b, ATOMIC_OTHER);
    let weak = ordered(Ordering::Relaxed).with_failure(Ordering::Relaxed).union(Flags::WEAK);
    let pair = b.cmpxchg(weak, s, cur, other, at);
    for k in 0..2 {
        let member = b.project(pair, k);
        reads.push(widen(&mut b, member));
    }
    let last = b.atomic_load(ty, ordered(Ordering::Relaxed), s, at);
    reads.push(widen(&mut b, last));

    let mut acc = b.iconst(Ty::I64, 0);
    let k31 = b.iconst(Ty::I64, 31);
    for x in reads {
        let m = b.binary(Op::Mul, Flags::NONE, acc, k31);
        acc = b.binary(Op::Add, Flags::NONE, m, x);
    }
    b.ret(&[acc]);
    built(b)
}

/// [`atomics_at`] at 8, 16, 32 and 64 bits through both tiers, against [`atomics_want`] (the
/// evaluator runs no atomics). Single-threaded: it checks each template's value and store, not
/// its ordering (the orderings' instructions are pass/atomic.rs's, which atomic_tests.rs pins).
/// Expected: passes, printing "directed: atomics agree at 4 widths". Fails with the Fast tier's
/// "no template for `atomic_rmw`" (or another) if the committed table lacks one, or with a wrong
/// value if a loop stores the wrong result, a signed `max`/`min` compares unextended, or a
/// `cmpxchg` flag is wrong.
#[test]
fn atomics_agree() {
    for w in [8u32, 16, 32, 64] {
        let what = std::format!("atomics at {w} bits");
        let want = move |p: u64| atomics_want(w, p);
        agrees(&Case { what: &what, must: BOTH, evaluate: false, second: 0 }, &atomics_at(w), &want);
    }
    std::println!("directed: atomics agree at 4 widths");
}

/// Names for [`two_c_names_seal`]: function reference 0 and 1 by two C names.
struct TwoC;

const C_NAMES: [&str; 2] = ["blaze_directed_first", "blaze_directed_second_longer"];

impl blaze_tier::Names for TwoC {
    fn name(&self, of: blaze_tier::Outside) -> Option<blaze_tier::Named<'_>> {
        match of {
            blaze_tier::Outside::Func(place) => C_NAMES.get(place as usize).copied().map(blaze_tier::Named::C),
            _ => None,
        }
    }

    fn routines(&self) -> blaze_tier::RoutinePlaces {
        blaze_tier::RoutinePlaces::NONE
    }
}

/// A function taking the addresses of two functions named by C names, through each tier, sealed,
/// encoded and read back: each name reads back as itself (the support/s3-extern-c rung's
/// class: the Fast tier wrote a name row's END where the row holds its length, so every name
/// but one starting at 0 was past the text).
/// Expected: passes, printing "directed: two C names seal". Fails with "a code record's name of
/// .. is past its text" if a tier's name row is not (start, length).
#[test]
fn two_c_names_seal() {
    let (mut b, _) = begin(&[Ty::I64]);
    let first = b.reference(blaze_lir::func::RefKind::Func, 0);
    let second = b.reference(blaze_lir::func::RefKind::Func, 1);
    let slot = b.slot(16, 3, SlotKind::Temp);
    let s = b.slot_addr(slot);
    let a = b.func_addr(first);
    let c = b.func_addr(second);
    b.store(Flags::NONE, a, s, 0, MemAttrs::aligned(3));
    b.store(Flags::NONE, c, s, 8, MemAttrs::aligned(3));
    let a = b.load(Ty::I64, Flags::NONE, s, 0, MemAttrs::aligned(3));
    let c = b.load(Ty::I64, Flags::NONE, s, 8, MemAttrs::aligned(3));
    let x = b.binary(Op::Xor, Flags::NONE, a, c);
    b.ret(&[x]);
    let func = built(b);
    let machine = table().head().machine;
    let tiers: [(TierName, super::TierFn); 2] = [(TierName::Reference, reference), (TierName::Fast, fast)];
    for (tier, emit) in tiers {
        let name = tier.as_str();
        let emitted = match emit(func.view(), &TwoC, machine, alloc()) {
            Ok(emitted) => emitted,
            Err(error) => panic!("the {name} tier refused two C names: {error}"),
        };
        let sealed = match blaze_tier::record::seal(emitted) {
            Ok(sealed) => sealed,
            Err(error) => panic!("the {name} tier's record does not seal: {error}"),
        };
        let mut bytes: AVec<'static, u8> = Vec::new_in(alloc());
        blaze_tier::record::encode(&sealed, &mut bytes);
        let view = match blaze_tier::record::read(&bytes) {
            Ok(view) => view,
            Err(error) => panic!("the {name} tier's record does not read back: {error}"),
        };
        for c_name in C_NAMES {
            let symbol = blaze_code::record::Symbol::of_name(c_name);
            assert_eq!(view.name(symbol), Some(c_name), "the {name} tier's record names {c_name} wrong");
        }
    }
    std::println!("directed: two C names seal");
}

/// [`provenance_moves`] through both tiers, against LIR's semantics as `want` writes them; not
/// through the evaluator, which refuses `expose` of a slot's pointer by its own rule ("a slot has
/// no address": its slots are not placed at addresses, so an address taken from one has no value
/// to give), as the atomics are tiers-only for the evaluator's lack of them.
/// Expected: passes, printing "directed: the provenance moves agree". Fails with the Fast tier's
/// "no template for `expose`" (or `from_exposed`, `ptr_addr`, `ptr_with_addr`) if the committed
/// table lacks one, or with a wrong value if a move takes the wrong operand.
#[test]
fn provenance_moves_agree() {
    let want = |p: u64| p.wrapping_mul(31).wrapping_add(p.wrapping_add(1));
    agrees(&Case { what: "the provenance moves", must: BOTH, evaluate: false, second: 0 }, &provenance_moves(), &want);
    std::println!("directed: the provenance moves agree");
}

/// The scratch pool's control (rule 23: remove the cause, watch the effect go). `f(p: i32) =
/// zext(p) + 1`: the entry narrows `p` into a held scratch register (x16) and stores it to its
/// cell. Over a table whose cell store writes x16 first:
/// - declared in the template's clobbers, the Fast tier refuses the function by name (the
///   pool's `Clobbered`), so the wrong code is never made;
/// - not declared (the check has nothing to see), the code runs and returns 1 for every `p`,
///   not `p + 1`: the overlap the check exists for;
/// - over the true table, it agrees.
/// Expected: passes, printing "directed: a cell store clobbering a held scratch is refused, and
/// wrong unrefused". Fails if the pool lets a template clobber a held register.
#[test]
fn a_template_clobbering_a_held_scratch_is_refused() {
    let (mut b, v) = begin(&[Ty::I32]);
    let wide = b.convert(Op::Zext, Flags::NONE, Ty::I64, v[0]);
    let one = b.iconst(Ty::I64, 1);
    let out = b.binary(Op::Add, Flags::NONE, wide, one);
    b.ret(&[out]);
    let func = built(b);
    let view = func.view();
    let want = |p: u64| (p & 0xffff_ffff) + 1;
    agrees(&Case { what: "zext(p) + 1", must: BOTH, evaluate: true, second: 0 }, &func, &want);
    let inner = table();
    let machine = inner.head().machine;
    let verified = || blaze_lir::verify(view).expect("the test's function is LIR");
    let declared = ClobbersX16::over(&inner, true);
    match (Fast { table: &declared, regs: &ARM64 }).emit(verified(), &Nameless, machine, alloc()) {
        Err(TierError::Refused { tier: TierName::Fast, what, .. }) => {
            assert_eq!(what, ScratchError::Clobbered(Reg(16)).what(), "refused for another reason");
        }
        Err(other) => panic!("the declared clobber is refused for another reason: {other}"),
        Ok(_) => panic!("the Fast tier placed a template clobbering the held x16"),
    }
    let hidden = ClobbersX16::over(&inner, false);
    let emitted = match (Fast { table: &hidden, regs: &ARM64 }).emit(verified(), &Nameless, machine, alloc()) {
        Ok(emitted) => emitted,
        Err(error) => panic!("the undeclared clobber is refused: {error}"),
    };
    let exec = match placed(emitted, TierName::Fast) {
        Ok(exec) => exec,
        Err(broke) => panic!("{broke}"),
    };
    for p in [7u64, 99, 0xdead_beef] {
        let got = run::call(&exec, [p, 0, 0, 0]);
        assert_ne!(got, want(p), "with the clobber hidden the held x16 is overwritten: f({p:#x}) must be wrong");
        assert_eq!(got, 1, "f({p:#x}) reads the zero the planted `movz` wrote");
    }
    std::println!("directed: a cell store clobbering a held scratch is refused, and wrong unrefused");
}
