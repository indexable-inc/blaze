//! The NEON integer intrinsics (target/neon.rs): their names as the one set `target::named`
//! asks, and their build, legalized as the Reference and Fast tiers take it (blaze-lir
//! `legalize`), then run by blaze-lir's reference evaluator.
//!
//! The rules are the Arm A64 instructions' (target/neon.rs's module doc): `TBL` gives 0 for an
//! index past the 16-byte table; `USHL`/`SSHL` shift by the signed low byte of each lane, right
//! for a negative amount, and an amount past the lane's width shifts every bit out.
//!
//! `every_neon_name_round_trips` prints "42 NEON names, each its own; llvm.aarch64.neon.tbl1.v4i16
//! refused"; `table_lookup_and_register_shifts` prints "tbl1.v16i8 = [10, 25, 0, 0, 13, 11, 12, 0,
//! 17, 17, 17, 17, 17, 17, 17, 25]; ushl.v2i64 by -1 and 4 = 0x4000000000000000 0xff000; by 64 and
//! -64 = 0 0; sshl.v2i64 by -1 and -100 = 0xc000000000000000 0xffffffffffffffff".

use std::alloc::Global;

use blaze_lir::builder::{MemAttrs, Param, Variadic};
use blaze_lir::eval::{Outcome, Val, run_in};
use blaze_lir::func::{BlockKind, Conv, Func, RefId, SlotKind};
use blaze_lir::legalize::{QuadRoutines, SoftFloat, legalize};
use blaze_lir::{Builder, Flags, Full, Op, Ty};

use super::neon::{IntShape, NeonCall, NeonFn, all};
use super::{Target, named};

/// No `f128` routine is bound: these functions have no `f128` operation.
struct NoRoutines;

impl QuadRoutines for NoRoutines {
    fn routine(&self, _: SoftFloat) -> Option<RefId> {
        None
    }
}

/// The call named `name`, which must be in the set.
fn call(name: &str) -> NeonCall {
    match named(name) {
        Ok(Target::Neon(call)) => call,
        other => panic!("{name} is not a NEON call of the set: {other:?}"),
    }
}

/// The result of `call` of the 16 bytes `a` and `x` (an 8-byte operand reads their first 8),
/// as its 16 bytes (an 8-byte result: its first 8, the rest 0).
fn run(call: NeonCall, a: [u8; 16], x: [u8; 16]) -> [u8; 16] {
    let mut out = [0u8; 16];
    let words = (call.shape().bytes() / 8) as usize;
    for word in 0..words {
        let bits = word_of(call, a, x, (word * 8) as i32);
        out[word * 8..word * 8 + 8].copy_from_slice(&bits.to_le_bytes());
    }
    out
}

/// The 8 bytes at `at` of the result of `call` of `a` and `x`.
fn word_of(call: NeonCall, a: [u8; 16], x: [u8; 16], at: i32) -> u64 {
    let heap = blaze_own::testing::heap();
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[], &[Param::of(Ty::I64)]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let mut operands = [None; 2];
    for (k, (bytes, shape)) in [a, x].iter().zip(call.operands()).enumerate() {
        // 16 bytes at a 16-byte alignment (log2 4): every shape's load is aligned.
        let slot = b.slot(16, 4, SlotKind::Temp);
        let addr = b.slot_addr(slot);
        for (i, &byte) in bytes.iter().enumerate() {
            let v = b.iconst(Ty::I8, u128::from(byte));
            b.store(Flags::NONE, v, addr, i as i32, MemAttrs::aligned(0));
        }
        operands[k] = Some(b.load(shape.ty(), Flags::NONE, addr, 0, MemAttrs::aligned(4)));
    }
    let [Some(va), Some(vx)] = operands else { panic!("two operands") };
    let r = super::neon::build(&mut b, call, va, vx);
    let slot = b.slot(16, 4, SlotKind::Temp);
    let addr = b.slot_addr(slot);
    b.store(Flags::NONE, r, addr, 0, MemAttrs::aligned(4));
    let w = b.load(Ty::I64, Flags::NONE, addr, at, MemAttrs::aligned(3));
    b.ret(&[w]);
    let f: Func<Global> = b.finish_checked().unwrap_or_else(|e| panic!("{}: the built function verifies: {e:?}", call.link_name()));

    let alloc = heap.alloc();
    let mut legal = f.view().to_func_in(alloc);
    legalize(&mut legal, &NoRoutines, alloc).unwrap_or_else(|e| panic!("{}: the vector operations legalize: {e:?}", call.link_name()));
    match run_in(legal.view(), &[], 1_000_000, Global).expect("the evaluator models it") {
        Outcome::Returned(v) => match v.first() {
            Some(Val::Int(i)) => u64::try_from(*i).expect("an i64's bits"),
            other => panic!("a non-integer result {other:?}"),
        },
        other => panic!("the run did not return: {other:?}"),
    }
}

/// Two 64-bit lanes as 16 bytes, lane 0 first.
fn lanes64(lo: u64, hi: u64) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&lo.to_le_bytes());
    out[8..].copy_from_slice(&hi.to_le_bytes());
    out
}

/// The two 64-bit lanes of 16 bytes.
fn words(bytes: [u8; 16]) -> (u64, u64) {
    let (lo, hi) = bytes.split_at(8);
    (u64::from_le_bytes(lo.try_into().expect("8 bytes")), u64::from_le_bytes(hi.try_into().expect("8 bytes")))
}

/// Every row of the set is found by its own link name and no other; a shape the library does
/// not declare `tbl1` at is refused by name. Without `named` asking target/neon.rs, the first
/// name is refused ("`llvm.aarch64.neon.tbl1.v8i8` is not a target intrinsic blaze knows").
#[test]
fn every_neon_name_round_trips() {
    let mut seen = 0;
    for c in all() {
        let name = c.link_name();
        assert_eq!(named(name), Ok(Target::Neon(c)), "{name}");
        assert_eq!(Target::Neon(c).link_name(), name);
        let [a, x] = c.operands();
        assert!(a.bytes() == 8 || a.bytes() == 16, "{name}");
        assert!(x.bytes() == 8 || x.bytes() == 16, "{name}");
        seen += 1;
    }
    assert_eq!(seen, 42);
    let tbl = call("llvm.aarch64.neon.tbl1.v16i8");
    assert_eq!((tbl.func(), tbl.shape()), (NeonFn::Tbl1, IntShape::V16I8));
    let ushl = call("llvm.aarch64.neon.ushl.v2i64");
    assert_eq!((ushl.func(), ushl.shape(), ushl.shape().lanes(), ushl.shape().width()), (NeonFn::Ushl, IntShape::V2I64, 2, 64));
    let refused = named("llvm.aarch64.neon.tbl1.v4i16").expect_err("tbl1 is declared at v8i8 and v16i8 only");
    assert_eq!(refused.to_string(), "`llvm.aarch64.neon.tbl1.v4i16` is not a target intrinsic blaze knows");
    assert!(named("llvm.aarch64.neon.tbl2.v16i8").is_err(), "a two-register table is not in the set");
    println!("{seen} NEON names, each its own; llvm.aarch64.neon.tbl1.v4i16 refused");
}

/// `tbl1` gives 0 for indices 16, 255 and 128 (a build that masks the index to the table, as
/// a shift masks its amount, gives 10 for 16); `ushl` by -1 is a logical right shift (an
/// arithmetic one gives 0xc000..), by a lane of 0x104 shifts by its low byte 4 (one reading the
/// whole lane shifts 260 bits out and gives 0), by 64 and by -64 gives 0 (one masking the
/// amount gives 1 1); `sshl` by -100 fills with the sign.
#[test]
fn table_lookup_and_register_shifts() {
    let table: [u8; 16] = core::array::from_fn(|i| 10 + i as u8);
    let index: [u8; 16] = [0, 15, 16, 255, 3, 1, 2, 128, 7, 7, 7, 7, 7, 7, 7, 15];
    let looked = run(call("llvm.aarch64.neon.tbl1.v16i8"), table, index);
    assert_eq!(looked, [10, 25, 0, 0, 13, 11, 12, 0, 17, 17, 17, 17, 17, 17, 17, 25]);

    // Eight indices: the same lookup, lanes 8.. of the result 0 (the result is 8 bytes).
    let eight = run(call("llvm.aarch64.neon.tbl1.v8i8"), table, index);
    assert_eq!(eight, [10, 25, 0, 0, 13, 11, 12, 0, 0, 0, 0, 0, 0, 0, 0, 0]);

    let ushl = call("llvm.aarch64.neon.ushl.v2i64");
    let by_low_byte = words(run(ushl, lanes64(0x8000_0000_0000_0001, 0xff00), lanes64(u64::MAX, 0x0104)));
    assert_eq!(by_low_byte, (0x4000_0000_0000_0000, 0xff000));
    let past = words(run(ushl, lanes64(1, 1), lanes64(64, (-64i64) as u64)));
    assert_eq!(past, (0, 0));

    let sshl = call("llvm.aarch64.neon.sshl.v2i64");
    let signed = words(run(sshl, lanes64(1 << 63, 1 << 63), lanes64(u64::MAX, (-100i64) as u64)));
    assert_eq!(signed, (0xc000_0000_0000_0000, u64::MAX));

    // Byte lanes: an amount of -128 is past the width (its negation wraps in an `i8`), 7 is not.
    let bytes = run(call("llvm.aarch64.neon.ushl.v16i8"), [0x80; 16], {
        let mut by = [7u8; 16];
        by[0] = 0x80;
        by[1] = 8;
        by[2] = 0xf9; // -7
        by
    });
    assert_eq!(&bytes[..4], &[0, 0, 0x01, 0]);
    assert_eq!(bytes[4], 0, "0x80 << 7 is truncated to the lane");

    // The op count: `tbl1.v16i8` is one `swizzle`.
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[], &[]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let slot = b.slot(16, 4, SlotKind::Temp);
    let addr = b.slot_addr(slot);
    let v = b.load(IntShape::V16I8.ty(), Flags::NONE, addr, 0, MemAttrs::aligned(4));
    let r = super::neon::build(&mut b, call("llvm.aarch64.neon.tbl1.v16i8"), v, v);
    b.store(Flags::NONE, r, addr, 0, MemAttrs::aligned(4));
    b.ret(&[]);
    let f: Func<Global> = b.finish_checked().expect("the built function verifies");
    let view = f.view();
    assert_eq!((0..view.len()).filter(|&i| view.op(i) == Op::Swizzle).count(), 1);

    println!(
        "tbl1.v16i8 = {looked:?}; ushl.v2i64 by -1 and 4 = {:#x} {:#x}; by 64 and -64 = {} {}; sshl.v2i64 by -1 and -100 = {:#x} {:#x}",
        by_low_byte.0, by_low_byte.1, past.0, past.1, signed.0, signed.1
    );
}

/// The pairwise maximum and minimum against Arm's rule computed here: lane `e` of the result is
/// `max`/`min` of lanes `2e` and `2e + 1` of `a`'s lanes then `x`'s, unsigned or signed by the
/// name. 0x80 is the unsigned larger and the signed smaller of 0x80 and 0x7f, so a build that
/// compares with the other signedness fails; one taking `x`'s pairs first fails at lane 0.
#[test]
fn pairwise_maximum_and_minimum() {
    let a: [u8; 16] = [0x80, 0x7f, 1, 2, 0xff, 0, 9, 9, 3, 200, 100, 50, 0x81, 0x01, 7, 8];
    let x: [u8; 16] = core::array::from_fn(|i| (i as u8).wrapping_mul(37));
    for (prefix, signed, max) in [("umaxp", false, true), ("uminp", false, false), ("smaxp", true, true), ("sminp", true, false)] {
        for shape in ["v16i8", "v8i8", "v8i16", "v4i32", "v2i32"] {
            let name = format!("llvm.aarch64.neon.{prefix}.{shape}");
            let c = call(&name);
            let (width, lanes) = ((c.shape().width() / 8) as usize, c.shape().lanes() as usize);
            let lane = |bytes: &[u8], k: usize| -> i128 {
                let mut v = 0u128;
                for i in 0..width {
                    v |= u128::from(bytes[k * width + i]) << (8 * i);
                }
                let bits = 8 * width as u32;
                if signed && v >> (bits - 1) == 1 { v as i128 - (1i128 << bits) } else { v as i128 }
            };
            let concat = |k: usize| if k < lanes { lane(&a, k) } else { lane(&x, k - lanes) };
            let got = run(c, a, x);
            for e in 0..lanes {
                let (p, q) = (concat(2 * e), concat(2 * e + 1));
                let want = if max { p.max(q) } else { p.min(q) };
                assert_eq!(lane(&got, e), want, "{name} lane {e}");
            }
        }
    }
    let u = run(call("llvm.aarch64.neon.umaxp.v16i8"), a, x);
    let s = run(call("llvm.aarch64.neon.sminp.v16i8"), a, x);
    assert_eq!((u[0], s[0], u[8]), (0x80, 0x80, 37));
    println!("umaxp.v16i8 = {u:?}; sminp.v16i8 = {s:?}");
}
