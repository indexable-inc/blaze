//! Reach (pass/reach.rs): functions whose branches and PC-relative references pass the short
//! fields' ±1 MiB are emitted, placed and run here, each against its expected value; the same
//! functions with islands off ([`Reach::ShortOnly`], the pass as it was) are refused by name;
//! a small function's bytes are the same either way.
//!
//! Every function here is also built as a code record and checked by `emitted`
//! (record_tests.rs `holds`): its `adr`s, far forms included, reach the data after the code.

use core::alloc::Allocator;

use blaze_lir::{
    Builder, Flags, IntCond, Op, Sink, Ty, Value, View,
    builder::{MemAttrs, Param, To, Variadic},
    func::{BlockKind, Conv, RefKind, SlotKind},
};

use crate::{
    BULK_INLINE_MOST, Code, EmitError, RelocTo, Routine, RoutinePlaces, Via, emit_reaching,
    pass::Reach,
    tests::{ISA, emitted, one, run},
};

/// What the short-only pass refuses a far function with.
const REFUSED: EmitError = EmitError::TooLarge { what: "a branch or PC-relative reference past its reach" };

/// `from + step + step + ...`, `n` adds: straight-line code, at least 12 bytes per add (each
/// value has its own stack cell: a load, the add, a store).
fn fill<A: Allocator + Clone, S: Sink>(b: &mut Builder<A, S>, from: Value, step: Value, n: usize) -> Value {
    (0..n).fold(from, |acc, _| b.binary(Op::Add, Flags::NONE, acc, step))
}

/// `view` emitted again in `like`'s allocator with the pass's `reach`: the outcome and the code
/// (whole even when refused: the refusal is `settle`'s, after every byte is written).
fn emit_with<A: Allocator + Clone>(view: View<'_>, via: &[Via], reach: Reach, like: &Code<A>) -> (Result<(), EmitError>, Code<A>) {
    let mut out = Code::new_in(like.bytes.allocator().clone());
    let got = emit_reaching(blaze_lir::verify(view).expect("the test's function is LIR"), via, &RoutinePlaces::NONE, ISA, BULK_INLINE_MOST, reach, &mut out).map(|_| ());
    (got, out)
}

/// The control: islands off, `view` is refused as before islands; the ratio of the two sizes
/// is printed (the islands' cost). Then: a second emission with islands is equal in bytes.
fn refused_short_only<A: Allocator + Clone>(name: &str, view: View<'_>, via: &[Via], far: &Code<A>) {
    let (got, short) = emit_with(view, via, Reach::ShortOnly, far);
    assert_eq!(got, Err(REFUSED), "{name}: the short-only pass refuses it");
    let (far_len, short_len) = (far.bytes.len(), short.bytes.len());
    println!(
        "{name}: {far_len} bytes with islands, {short_len} short-only (refused), ratio {:.5}",
        far_len as f64 / short_len as f64
    );
    // Deterministic (rule 25).
    let (again, twice) = emit_with(view, via, Reach::Islands, far);
    assert_eq!(again, Ok(()), "{name}: emits twice");
    assert!(twice.bytes[..] == far.bytes[..], "{name}: two emissions are equal in bytes");
    assert!(twice.fixups[..] == far.fixups[..] && twice.relocs[..] == far.relocs[..], "{name}: and in their words");
}

/// Runs `f` on a thread with room for these functions' frames (16 bytes per value: up to 5 MiB).
fn on_big_stack<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|s| {
        std::thread::Builder::new()
            .stack_size(256 << 20)
            .spawn_scoped(s, f)
            .expect("a thread for the run")
            .join()
            .expect("the run returns")
    })
}

/// Places `code` (no literal word, no relocation) and calls it with `x`, on a big stack.
macro_rules! run_plain {
    ($code:expr, $x:expr) => {{
        let (code, x) = ($code, $x);
        on_big_stack(|| {
            let exec = run::place(code, |_| unreachable!("no literal word"));
            run::call(&exec, [x, 0, 0, 0])
        })
    }};
}

/// Adds in the 2 MiB stretch.
const FAR: usize = 200_000;

/// A `cbnz` forward over 2 MiB of straight code, taken and not: the island's veneer `b`.
#[test]
fn a_cond_branch_over_2_mib_runs_both_ways() {
    let func = one(&[Ty::I64], Ty::I64, |b, v| {
        let body = b.create_block(BlockKind::Normal);
        let far = b.create_block(BlockKind::Normal);
        let zero = b.iconst(Ty::I64, 0);
        let taken = b.icmp(IntCond::Ne, v[0], zero);
        b.br(Flags::NONE, taken, (far, &[]), (body, &[]));
        b.begin(body);
        let step = b.iconst(Ty::I64, 1);
        let sum = fill(b, v[0], step, FAR);
        b.ret(&[sum]);
        b.begin(far);
        b.iconst(Ty::I64, 0xfa7)
    });
    let code = emitted(func.view(), &[]).unwrap_or_else(|e| panic!("{e}"));
    assert!(code.bytes.len() > 2 << 20, "the stretch is past 2 MiB: {} bytes", code.bytes.len());
    assert_eq!(run_plain!(&code, 0), FAR as u64, "not taken: the stretch runs");
    assert_eq!(run_plain!(&code, 5), 0xfa7, "taken: 2 MiB forward through the veneer");
    refused_short_only("cbnz over 2 MiB", func.view(), &[], &code);
}

/// A conditional back edge over 1.4 MiB: the long form at once (`cbz x9, +8; b head`).
#[test]
fn a_far_back_edge_is_long_at_once() {
    const BODY: usize = 120_000;
    let func = one(&[Ty::I64], Ty::I64, |b, v| {
        let head = b.create_block(BlockKind::Normal);
        let exit = b.create_block(BlockKind::Normal);
        let (acc, left) = (b.slot(8, 3, SlotKind::Temp), b.slot(8, 3, SlotKind::Temp));
        let (acc, left) = (b.slot_addr(acc), b.slot_addr(left));
        let (zero, step) = (b.iconst(Ty::I64, 0), b.iconst(Ty::I64, 1));
        b.store(Flags::NONE, zero, acc, 0, MemAttrs::default());
        b.store(Flags::NONE, v[0], left, 0, MemAttrs::default());
        b.jump((head, &[]));
        b.begin(head);
        let a = b.load(Ty::I64, Flags::NONE, acc, 0, MemAttrs::default());
        let a = fill(b, a, step, BODY);
        b.store(Flags::NONE, a, acc, 0, MemAttrs::default());
        let n = b.load(Ty::I64, Flags::NONE, left, 0, MemAttrs::default());
        let n = b.binary(Op::Sub, Flags::NONE, n, step);
        b.store(Flags::NONE, n, left, 0, MemAttrs::default());
        let again = b.icmp(IntCond::Ne, n, zero);
        b.br(Flags::NONE, again, (head, &[]), (exit, &[]));
        b.begin(exit);
        b.load(Ty::I64, Flags::NONE, acc, 0, MemAttrs::default())
    });
    let code = emitted(func.view(), &[]).unwrap_or_else(|e| panic!("{e}"));
    assert!(code.bytes.len() > 1 << 20, "the loop's body is past 1 MiB: {} bytes", code.bytes.len());
    assert_eq!(run_plain!(&code, 1), BODY as u64, "once round");
    assert_eq!(run_plain!(&code, 3), 3 * BODY as u64, "three times round the far back edge");
    refused_short_only("a back edge over 1.4 MiB", func.view(), &[], &code);
}

/// `data_addr` of a small row 2 MiB before the data: the island's data veneer (`b` there, the
/// far form, `b` back).
#[test]
fn an_adr_of_far_data_goes_through_a_veneer() {
    const ROW: [u8; 16] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
    let func = one(&[Ty::I64], Ty::I64, |b, v| {
        let row = b.data(&ROW, 3, &[]);
        let at = b.data_addr(row);
        let step = b.iconst(Ty::I64, 1);
        let sum = fill(b, v[0], step, FAR);
        let word = b.load(Ty::I64, Flags::NONE, at, 8, MemAttrs::default());
        b.binary(Op::Add, Flags::NONE, sum, word)
    });
    let code = emitted(func.view(), &[]).unwrap_or_else(|e| panic!("{e}"));
    let word = u64::from_le_bytes([9, 10, 11, 12, 13, 14, 15, 16]);
    assert_eq!(run_plain!(&code, 7), (7 + FAR as u64).wrapping_add(word), "the row's second word, 2 MiB on");
    refused_short_only("adr over 2 MiB", func.view(), &[], &code);
}

/// A row of 300 KiB: own data past the margin, so every use is the far form in line.
const BIG_LEN: usize = 300 << 10;
static BIG: [u8; BIG_LEN] = {
    let mut row = [0u8; BIG_LEN];
    let (first, last) = (0x0102_0304_0506_0708u64.to_le_bytes(), 0x1112_1314_1516_1718u64.to_le_bytes());
    let mut i = 0;
    while i < 8 {
        row[i] = first[i];
        row[BIG_LEN - 8 + i] = last[i];
        i += 1;
    }
    row
};

/// A row of 300 KiB read at both ends: the in-line far form (`adr; movz; movk; add`).
#[test]
fn a_large_row_is_reached_at_both_ends() {
    let func = one(&[Ty::I64], Ty::I64, |b, v| {
        let row = b.data(&BIG, 3, &[]);
        let at = b.data_addr(row);
        let first = b.load(Ty::I64, Flags::NONE, at, 0, MemAttrs::default());
        let last = b.load(Ty::I64, Flags::NONE, at, (BIG_LEN - 8) as i32, MemAttrs::default());
        let both = b.binary(Op::Xor, Flags::NONE, first, last);
        b.binary(Op::Add, Flags::NONE, both, v[0])
    });
    let code = emitted(func.view(), &[]).unwrap_or_else(|e| panic!("{e}"));
    let want = (0x0102_0304_0506_0708u64 ^ 0x1112_1314_1516_1718) + 2;
    assert_eq!(run_plain!(&code, 2), want, "the row's first and last words");
    // Small code, so the short-only pass still reaches the row's start: the far form is the
    // islands pass's choice by the data's size alone, and both run.
    let (short, code_short) = emit_with(func.view(), &[], Reach::ShortOnly, &code);
    assert_eq!(short, Ok(()), "the short-only pass emits it too (the row starts within 1 MiB)");
    assert_eq!(run_plain!(&code_short, 2), want, "and it runs the same");
}

/// A switch of 3,000 cases over 1.4 MiB of case blocks: the later cases' `b.eq` through
/// veneers placed between the cases' blocks.
#[test]
fn a_switch_of_3000_cases_reaches_every_case() {
    const CASES: usize = 3000;
    const EACH: usize = 40;
    let func = one(&[Ty::I64], Ty::I64, |b, v| {
        let blocks: [_; CASES] = core::array::from_fn(|_| b.create_block(BlockKind::Normal));
        let default = b.create_block(BlockKind::Normal);
        let step = b.iconst(Ty::I64, 1);
        let cases: [(u128, To<'_>); CASES] = core::array::from_fn(|i| (i as u128, (blocks[i], &[][..])));
        b.switch(v[0], (default, &[]), &cases);
        for (i, &block) in blocks.iter().enumerate() {
            b.begin(block);
            let base = b.iconst(Ty::I64, i as u128 * 1000);
            let got = fill(b, base, step, EACH);
            b.ret(&[got]);
        }
        b.begin(default);
        b.iconst(Ty::I64, 0xdead)
    });
    let code = emitted(func.view(), &[]).unwrap_or_else(|e| panic!("{e}"));
    assert!(code.bytes.len() > 1 << 20, "the cases' blocks are past 1 MiB: {} bytes", code.bytes.len());
    for x in [0u64, 1, 1500, 2998, 2999] {
        assert_eq!(run_plain!(&code, x), x * 1000 + EACH as u64, "case {x}");
    }
    assert_eq!(run_plain!(&code, 3000), 0xdead, "past the last case: the default");
    refused_short_only("a switch of 3,000 cases", func.view(), &[], &code);
}

/// A frame of 1 MiB written at both ends.
#[test]
fn a_1_mib_frame_is_written_at_both_ends() {
    const SIZE: u32 = 1 << 20;
    let func = one(&[Ty::I64], Ty::I64, |b, v| {
        let slot = b.slot(SIZE, 3, SlotKind::Temp);
        let at = b.slot_addr(slot);
        let thousand = b.iconst(Ty::I64, 1000);
        b.store(Flags::NONE, v[0], at, 0, MemAttrs::default());
        b.store(Flags::NONE, thousand, at, (SIZE - 8) as i32, MemAttrs::default());
        let first = b.load(Ty::I64, Flags::NONE, at, 0, MemAttrs::default());
        let last = b.load(Ty::I64, Flags::NONE, at, (SIZE - 8) as i32, MemAttrs::default());
        b.binary(Op::Add, Flags::NONE, first, last)
    });
    let code = emitted(func.view(), &[]).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(run_plain!(&code, 5), 1005, "both ends of the frame");
}

/// The shape of 64 unrolled checks, each branching to one shared failure at the end over
/// straight code: every check but the last few through an island's veneer.
#[test]
fn sixty_four_unrolled_checks_reach_their_shared_failure() {
    const CHECKS: usize = 64;
    const EACH: usize = 4000;
    let func = one(&[Ty::I64], Ty::I64, |b, v| {
        let next: [_; CHECKS] = core::array::from_fn(|_| b.create_block(BlockKind::Normal));
        let fail = b.create_block(BlockKind::Normal);
        let step = b.iconst(Ty::I64, 1);
        let mut acc = b.iconst(Ty::I64, 0);
        for (i, &block) in next.iter().enumerate() {
            let bad = b.iconst(Ty::I64, 1000 + i as u128);
            let failed = b.icmp(IntCond::Eq, v[0], bad);
            b.br(Flags::NONE, failed, (fail, &[]), (block, &[]));
            b.begin(block);
            acc = fill(b, acc, step, EACH);
        }
        b.ret(&[acc]);
        b.begin(fail);
        b.iconst(Ty::I64, 0xdead)
    });
    let code = emitted(func.view(), &[]).unwrap_or_else(|e| panic!("{e}"));
    assert!(code.bytes.len() > 2 << 20, "the checks span past 2 MiB: {} bytes", code.bytes.len());
    assert_eq!(run_plain!(&code, 0), (CHECKS * EACH) as u64, "every check passes");
    for x in [1000u64, 1001, 1031, 1062, 1063] {
        assert_eq!(run_plain!(&code, x), 0xdead, "check {} fails", x - 1000);
    }
    refused_short_only("64 unrolled checks", func.view(), &[], &code);
}

extern "C" fn plus_three(x: u64) -> u64 {
    x + 3
}

/// A call whose literal word (the pool after the code) is 2 MiB on: the island holds a copy of
/// the word, filled by placement like the pool's (one more fixup).
#[test]
fn a_far_literal_word_is_copied_into_an_island() {
    let func = one(&[Ty::I64], Ty::I64, |b, v| {
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        let callee = b.reference(RefKind::Func, 0);
        let got = b.call(Flags::NONE, callee, sig, &[v[0]]);
        let step = b.iconst(Ty::I64, 1);
        fill(b, got, step, FAR)
    });
    let code = emitted(func.view(), &[Via::Address]).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(code.fixups.len(), 2, "the pool's word and the island's copy: {:?}", &code.fixups[..]);
    assert!(code.fixups.iter().all(|it| it.place == 0), "both name the one callee");
    let got = on_big_stack(|| {
        let exec = run::place(&code, |_| plus_three as usize as u64);
        run::call(&exec, [10, 0, 0, 0])
    });
    assert_eq!(got, 13 + FAR as u64, "the call through the copied word");
    refused_short_only("a literal word 2 MiB on", func.view(), &[Via::Address], &code);
}

unsafe extern "C" {
    fn memset(dst: *mut u8, byte: i32, n: usize) -> *mut u8;
}

/// A routine's address word 2 MiB on (`memset` of a length not known): the island's copy is a
/// relocation of the routine like the pool's.
#[test]
fn a_far_routine_word_is_copied_into_an_island() {
    let func = one(&[Ty::I64], Ty::I64, |b, v| {
        let slot = b.slot(64, 3, SlotKind::Temp);
        let at = b.slot_addr(slot);
        let byte = b.iconst(Ty::I8, 0x5a);
        b.bulk(Op::Memset, Flags::NONE, at, byte, v[0], MemAttrs::default());
        let step = b.iconst(Ty::I64, 1);
        let sum = fill(b, v[0], step, FAR);
        let word = b.load(Ty::I64, Flags::NONE, at, 0, MemAttrs::default());
        b.binary(Op::Add, Flags::NONE, sum, word)
    });
    let code = emitted(func.view(), &[]).unwrap_or_else(|e| panic!("{e}"));
    let memsets = code.relocs.iter().filter(|it| it.to == RelocTo::Routine(Routine::Memset)).count();
    assert_eq!(memsets, 2, "the pool's word and the island's copy");
    let got = on_big_stack(|| {
        let exec = run::place_all(
            &code,
            |_| unreachable!("no literal word"),
            |to| match to {
                RelocTo::Routine(Routine::Memset) => memset as usize as u64,
                other => panic!("an unexpected relocation {other:?}"),
            },
        );
        run::call(&exec, [8, 0, 0, 0])
    });
    assert_eq!(got, (8 + FAR as u64).wrapping_add(0x5a5a_5a5a_5a5a_5a5a), "memset through the copied word");
    refused_short_only("a routine word 2 MiB on", func.view(), &[], &code);
}

/// Short forms are still chosen when near: a small function's bytes and words with islands on
/// are the short-only pass's, byte for byte (the oracle: the pass before islands).
#[test]
fn a_small_function_is_byte_equal() {
    const ROW: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];
    let funcs = [
        one(&[Ty::I64], Ty::I64, |b, v| {
            let body = b.create_block(BlockKind::Normal);
            let far = b.create_block(BlockKind::Normal);
            let zero = b.iconst(Ty::I64, 0);
            let taken = b.icmp(IntCond::Ne, v[0], zero);
            b.br(Flags::NONE, taken, (far, &[]), (body, &[]));
            b.begin(body);
            let step = b.iconst(Ty::I64, 1);
            let sum = fill(b, v[0], step, 1000);
            b.ret(&[sum]);
            b.begin(far);
            b.iconst(Ty::I64, 7)
        }),
        one(&[Ty::I64], Ty::I64, |b, v| {
            let blocks: [_; 8] = core::array::from_fn(|_| b.create_block(BlockKind::Normal));
            let default = b.create_block(BlockKind::Normal);
            let cases: [(u128, To<'_>); 8] = core::array::from_fn(|i| (i as u128 * 5000, (blocks[i], &[][..])));
            b.switch(v[0], (default, &[]), &cases);
            for (i, &block) in blocks.iter().enumerate() {
                b.begin(block);
                let got = b.iconst(Ty::I64, i as u128);
                b.ret(&[got]);
            }
            b.begin(default);
            b.iconst(Ty::I64, 99)
        }),
        one(&[Ty::I64], Ty::I64, |b, v| {
            let row = b.data(&ROW, 3, &[]);
            let at = b.data_addr(row);
            let word = b.load(Ty::I64, Flags::NONE, at, 0, MemAttrs::default());
            b.binary(Op::Add, Flags::NONE, word, v[0])
        }),
        one(&[Ty::I64], Ty::I64, |b, v| {
            let slot = b.slot(64, 3, SlotKind::Temp);
            let at = b.slot_addr(slot);
            let byte = b.iconst(Ty::I8, 1);
            b.bulk(Op::Memset, Flags::NONE, at, byte, v[0], MemAttrs::default());
            b.load(Ty::I64, Flags::NONE, at, 0, MemAttrs::default())
        }),
    ];
    for (i, func) in funcs.iter().enumerate() {
        let code = emitted(func.view(), &[]).unwrap_or_else(|e| panic!("{e}"));
        let (short, same) = emit_with(func.view(), &[], Reach::ShortOnly, &code);
        assert_eq!(short, Ok(()), "function {i}: the short-only pass emits it");
        assert!(same.bytes[..] == code.bytes[..], "function {i}: the same bytes with islands on and off");
        assert!(same.fixups[..] == code.fixups[..] && same.relocs[..] == code.relocs[..], "function {i}: the same words");
    }
    // And a call: one literal word, near.
    let call = one(&[Ty::I64], Ty::I64, |b, v| {
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        let callee = b.reference(RefKind::Func, 0);
        b.call(Flags::NONE, callee, sig, &[v[0]])
    });
    let code = emitted(call.view(), &[Via::Address]).unwrap_or_else(|e| panic!("{e}"));
    let (short, same) = emit_with(call.view(), &[Via::Address], Reach::ShortOnly, &code);
    assert_eq!(short, Ok(()), "the call: the short-only pass emits it");
    assert!(same.bytes[..] == code.bytes[..] && same.fixups[..] == code.fixups[..], "the call: the same bytes");
}
