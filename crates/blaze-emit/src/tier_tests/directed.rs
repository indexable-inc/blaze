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
//! - The scratch pool's control (rule 23): a cell store that writes x16 first. Declared in the
//!   template's clobbers, the Fast tier refuses it by name (scratch.rs, `Clobbered`); with the
//!   declaration removed, the same bytes run and the answer is wrong.

use blaze_lir::builder::{MemAttrs, Param, Variadic};
use blaze_lir::eval::Val;
use blaze_lir::func::{BlockKind, Conv, Func, SlotKind};
use blaze_lir::{Builder, Flags, Full, IntCond, Op, Ty, Value};
use blaze_own::{AVec, Alloc, Vec};
use blaze_tier::{ARM64, Field, Pattern, Reg, RegClass, Table, TableHead, Template, Tier, TierError, TierName};

use super::{Nameless, evaluated, fast, placed, reference, table};
use crate::Fast;
use crate::compose::emit::glue;
use crate::compose::scratch::ScratchError;
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
