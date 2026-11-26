//! Thin compatibility wrapper over [`super::memssa`] (Rule 30: one canonical home for MemorySSA,
//! alias partitioning, store-to-load forwarding, DSE, dead-temp bulk-copy elision, small-memop
//! scalarization, and Memory LICM).
//!
//! Preserves all existing public signatures (`optimize_memory`, `scalarize_small_memops`,
//! `eliminate_temp_bulk_copies`, `MemOptStats`, `MemOptOutput`) for downstream callers in
//! `blaze-emit` and `blaze-lir::lto`.

use core::alloc::Allocator;

use super::{SimplifyError, memssa};
use crate::{Func, View};

/// Telemetry from [`optimize_memory`].
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct MemOptStats {
    /// Loop-invariant loads physically hoisted into loop preheaders.
    pub loads_hoisted: u32,
    /// Overwritten dead stores eliminated within basic blocks.
    pub dead_stores_eliminated: u32,
    /// Dead temporary bulk-copy slots elided (`Memset -> Memcpy` / `Memcpy -> Memcpy`).
    pub temp_copies_elided: u32,
}

/// Output of [`optimize_memory`].
#[derive(Debug)]
pub struct MemOptOutput<A: Allocator> {
    pub func: Func<A>,
    pub stats: MemOptStats,
}

/// Eliminates dead temporary local stack slots used solely as single-writer bulk-copy staging
/// buffers (`Op::Memset -> Op::Memcpy/Memmove` and `Op::Memcpy/Memmove -> Op::Memcpy/Memmove`),
/// compacting eliminated stack slots and running up to 4 fixed-point passes followed by `dead`.
#[inline]
pub fn eliminate_temp_bulk_copies<A: Allocator + Copy>(
    func: Func<A>,
    alloc: A,
) -> Result<(Func<A>, u32), SimplifyError> {
    memssa::eliminate_temp_bulk_copies(func, alloc)
}

/// Elides dead temporary bulk-copy chains (`Memset -> Memcpy`, `Memcpy -> Memcpy`) and then
/// scalarizes non-volatile `Op::Memcpy` and `Op::Memmove` instructions into overlap-safe
/// `Op::Load` + `Op::Store` sequences (all loads before all stores).
#[inline]
pub fn scalarize_small_memops<A: Allocator + Copy>(
    view: View<'_>,
    alloc: A,
) -> Result<Option<Func<A>>, SimplifyError> {
    memssa::scalarize_small_memops(view, alloc)
}

/// Runs unified MemorySSA optimization (dead-temp bulk-copy elision, alias-aware Memory LICM,
/// store-to-load & uniform-`Memset` & sub-word/piece-gather forwarding, DSE, and dead stack-slot
/// compaction) on `view`.
#[inline]
pub fn optimize_memory<A: Allocator + Copy>(
    view: View<'_>,
    alloc: A,
) -> Result<MemOptOutput<A>, SimplifyError> {
    let out = memssa::optimize_memssa(view, alloc)?;
    Ok(MemOptOutput {
        func: out.func,
        stats: MemOptStats {
            loads_hoisted: out.stats.loads_hoisted,
            dead_stores_eliminated: out.stats.dead_stores_eliminated,
            temp_copies_elided: out.stats.temp_copies_elided,
        },
    })
}

#[cfg(test)]
mod tests {
    use blaze_own::Vec;
    use blaze_own::testing::TestHeap;

    use super::*;
    use crate::{
        Flags, Op, Ty,
        builder::{Builder, Full, MemAttrs, Param, Variadic},
        func::{BlockKind, Conv, SlotKind},
        verify,
    };

    #[inline]
    fn to_idx(v: u32) -> usize {
        usize::try_from(v).unwrap_or(usize::MAX)
    }

    #[inline]
    fn at_u32(s: &[u32], i: usize) -> u32 {
        s.get(i).copied().unwrap_or(0)
    }

    #[test]
    fn memory_licm_hoists_non_aliasing_invariant_load_out_of_loop() {
        // Loop writes to `[dst + 8]` while reading invariant `[dst + 0]` in header: same base, non-overlapping!
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR), Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let [entry, header, exit] = [(); 3].map(|_| b.create_block(BlockKind::Normal));
        b.begin(entry);
        let ptr = b.param(Ty::PTR);
        let n = b.param(Ty::I64);
        let zero = b.iconst(Ty::I64, 0);
        b.jump((header, &[zero, zero]));

        b.begin(header);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let scale = b.load(Ty::I64, Flags::NONE, ptr, 0, MemAttrs::aligned(8));
        let prod = b.binary(Op::Mul, Flags::NONE, i, scale);
        let acc2 = b.binary(Op::Add, Flags::NONE, acc, prod);
        b.store(Flags::NONE, acc2, ptr, 8, MemAttrs::aligned(8));
        let one = b.iconst(Ty::I64, 1);
        let i2 = b.binary(Op::Add, Flags::NONE, i, one);
        let keep = b.icmp(crate::IntCond::Ult, i2, n);
        b.br(Flags::NONE, keep, (header, &[i2, acc2]), (exit, &[acc2]));

        b.begin(exit);
        let res = b.param(Ty::I64);
        b.ret(&[res]);
        let func = b.finish_checked().unwrap();

        let out = optimize_memory(func.view(), TestHeap).unwrap();
        assert_eq!(out.stats.loads_hoisted, 1, "non-overlapping invariant load at [ptr+0] must hoist to preheader");
    }

    #[test]
    fn dead_store_elimination_removes_overwritten_store_on_non_aliasing_slot() {
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR), Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let ext = b.param(Ty::PTR);
        let x = b.param(Ty::I64);
        let s = b.slot(16, 3, SlotKind::Named);
        let sp = b.slot_addr(s);
        let k1 = b.iconst(Ty::I64, 111);
        let k2 = b.iconst(Ty::I64, 222);
        b.store(Flags::NONE, k1, sp, 0, MemAttrs::aligned(8)); // dead!
        let ext_v = b.load(Ty::I64, Flags::NONE, ext, 0, MemAttrs::aligned(8)); // cannot alias non-escaping slot `s`!
        let sum = b.binary(Op::Add, Flags::NONE, x, ext_v);
        b.store(Flags::NONE, k2, sp, 0, MemAttrs::aligned(8)); // overwrites `[sp + 0]`!
        let loaded = b.load(Ty::I64, Flags::NONE, sp, 0, MemAttrs::aligned(8));
        let res = b.binary(Op::Add, Flags::NONE, sum, loaded);
        b.ret(&[res]);
        let func = b.finish_checked().unwrap();

        let out = optimize_memory(func.view(), TestHeap).unwrap();
        // MemorySSA forwards `loaded -> k2`, making both stack stores dead and compacting `s`.
        assert_eq!(
            out.stats.dead_stores_eliminated, 2,
            "both overwritten and forwarded stack stores across non-aliasing ext load must be eliminated"
        );
        assert!(out.func.slots.is_empty(), "non-escaping slot s must be compacted out");
    }

    #[test]
    fn memory_licm_does_not_hoist_sret_slot_load_past_invoke() {
        // Exact `Iterator::fold` pattern: `SlotAddr` in entry, loop invokes `next(sret = slot)`
        // and then loads the discriminant from `slot`. `Op::Invoke` clobbers escaping `slot`, so
        // the load MUST NOT hoist above the loop!
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let callee_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR), Param::of(Ty::PTR)], &[]);
        let callee = b.reference(crate::func::RefKind::Func, 1);
        let [entry, header, after_call, exit, lpad] = [
            b.create_block(BlockKind::Normal),
            b.create_block(BlockKind::Normal),
            b.create_block(BlockKind::Normal),
            b.create_block(BlockKind::Normal),
            b.create_block(BlockKind::Cleanup),
        ];

        b.begin(entry);
        let iter_ptr = b.param(Ty::PTR);
        let ret_slot = b.slot(48, 3, SlotKind::Named);
        let ret_ptr = b.slot_addr(ret_slot);
        let zero = b.iconst(Ty::I64, 0);
        b.jump((header, &[zero]));

        b.begin(header);
        let acc = b.param(Ty::I64);
        b.invoke(Flags::NONE, callee, callee_sig, &[iter_ptr, ret_ptr], after_call, lpad);

        b.begin(after_call);
        let disc = b.load(Ty::I8, Flags::NONE, ret_ptr, 0, MemAttrs::aligned(1));
        let zero_i8 = b.iconst(Ty::I8, 0);
        let is_some = b.icmp(crate::IntCond::Ne, disc, zero_i8);
        let payload = b.load(Ty::I64, Flags::NONE, ret_ptr, 8, MemAttrs::aligned(8));
        let acc2 = b.binary(Op::Add, Flags::NONE, acc, payload);
        b.br(Flags::NONE, is_some, (header, &[acc2]), (exit, &[acc]));

        b.begin(exit);
        let out_val = b.param(Ty::I64);
        b.ret(&[out_val]);

        b.begin(lpad);
        let exc = b.param(Ty::PTR);
        b.resume(exc);

        let func = b.finish_checked().unwrap();
        let out = optimize_memory(func.view(), TestHeap).unwrap();
        assert_eq!(
            out.stats.loads_hoisted, 0,
            "sret slot load after Op::Invoke inside loop must NEVER hoist into preheader"
        );
    }

    #[test]
    fn memory_licm_does_not_hoist_slot_load_past_dynamic_ptradd_store() {
        // Non-escaping local slot read at `[s + 0]` and written via `PtrAdd(s, i)` in loop:
        // dynamic index MAY alias offset 0, so the load must NOT hoist!
        let mut b = Builder::<TestHeap, Full>::new_in(64, TestHeap);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let [entry, header, exit] = [(); 3].map(|_| b.create_block(BlockKind::Normal));

        b.begin(entry);
        let n = b.param(Ty::I64);
        let s = b.slot(64, 3, SlotKind::Named);
        let sp = b.slot_addr(s);
        let zero = b.iconst(Ty::I64, 0);
        b.store(Flags::NONE, zero, sp, 0, MemAttrs::aligned(8));
        b.jump((header, &[zero, zero]));

        b.begin(header);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let cur = b.load(Ty::I64, Flags::NONE, sp, 0, MemAttrs::aligned(8));
        let acc2 = b.binary(Op::Add, Flags::NONE, acc, cur);
        let elem_ptr = b.ptr_add(Flags::NONE, sp, i);
        b.store(Flags::NONE, acc2, elem_ptr, 0, MemAttrs::aligned(1));
        let one = b.iconst(Ty::I64, 1);
        let i2 = b.binary(Op::Add, Flags::NONE, i, one);
        let keep = b.icmp(crate::IntCond::Ult, i2, n);
        b.br(Flags::NONE, keep, (header, &[i2, acc2]), (exit, &[acc2]));

        b.begin(exit);
        let res = b.param(Ty::I64);
        b.ret(&[res]);

        let func = b.finish_checked().unwrap();
        let out = optimize_memory(func.view(), TestHeap).unwrap();
        assert_eq!(
            out.stats.loads_hoisted, 0,
            "load from [slot+0] must NOT hoist across dynamic PtrAdd(slot, i) store"
        );
    }

    #[test]
    fn scalarize_small_memops_rewrites_swap_and_enables_sroa() {
        use crate::{
            eval::{Bodies, BodyId, Outcome, Val, run_calls_in, run_in},
            func::{RefId, RefKind},
        };

        let alloc = blaze_own::testing::heap().alloc();

        // Build a `slice::swap`-style function using a local 8-byte `StackSlot` and 3
        // `Op::Memmove(..., 8)` calls (`tmp <- p`, `p <- q`, `q <- tmp`).
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let swap_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR), Param::of(Ty::PTR)], &[]);
        b.set_signature(swap_sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let p = b.param(Ty::PTR);
        let q = b.param(Ty::PTR);
        let tmp = b.slot(8, 3, SlotKind::Temp);
        let tp = b.slot_addr(tmp);
        let eight = b.iconst(Ty::I64, 8);
        let aligned8 = MemAttrs { align: 3, src_align: 3, pointee_align: 0, bits: 0, range: None };
        let dst_only = MemAttrs::aligned(3);
        b.bulk(Op::Memmove, Flags::NONE, tp, p, eight, dst_only);
        b.bulk(Op::Memmove, Flags::NONE, p, q, eight, aligned8);
        b.bulk(Op::Memmove, Flags::NONE, q, tp, eight, aligned8);
        b.ret(&[]);
        let swap_fn = b.finish_checked().expect("valid swap input");

        // Run `scalarize_small_memops` -> `scalar_replace` -> `aegraph_optimize`.
        let scalarized = scalarize_small_memops(swap_fn.view(), alloc)
            .expect("scalarize_small_memops succeeds")
            .expect("must scalarize 8-byte memmove");
        // First `Memmove` had `src_align == 0`: emitted `Load` must not over-claim alignment.
        let first_load = (0..scalarized.view().len())
            .find(|&i| scalarized.view().op(i) == Op::Load)
            .expect("must emit Load");
        assert_eq!(scalarized.view().attrs.get(first_load).copied(), Some(0));

        let after_sroa = super::super::scalar_replace(scalarized.view(), alloc).expect("sroa succeeds");
        let after_ae = super::super::egraph::aegraph_optimize(after_sroa.view(), alloc)
            .expect("aegraph succeeds")
            .func;
        verify(after_ae.view()).expect("optimized swap verifies");
        assert!(after_ae.slots.is_empty(), "swap temp stack slot must be eliminated by SROA");
        for inst in 0..after_ae.view().len() {
            let op = after_ae.view().op(inst);
            assert_ne!(op, Op::Memmove, "no Op::Memmove may remain after scalarize + sroa + aegraph");
            assert_ne!(op, Op::SlotAddr, "no Op::SlotAddr may remain after scalarize + sroa + aegraph");
        }

        // Caller `driver(a0, a1, same)` swaps `arr[0]` and `arr[same ? 0 : 1]` via `swap(p, q)`,
        // exercising both distinct (`i != j`) and overlapping (`i == j`) pointers.
        let mut db = Builder::<_, Full>::new_in(64, alloc);
        let drv_sig = db.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I1)],
            &[Param::of(Ty::I64), Param::of(Ty::I64)],
        );
        db.set_signature(drv_sig);
        let callee_sig = db.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR), Param::of(Ty::PTR)], &[]);
        let callee = db.reference(RefKind::Func, 0);
        let d_entry = db.create_block(BlockKind::Normal);
        db.begin(d_entry);
        let a0 = db.param(Ty::I64);
        let a1 = db.param(Ty::I64);
        let same = db.param(Ty::I1);
        let arr = db.slot(16, 3, SlotKind::Named);
        let base = db.slot_addr(arr);
        db.store(Flags::NONE, a0, base, 0, MemAttrs::aligned(3));
        db.store(Flags::NONE, a1, base, 8, MemAttrs::aligned(3));
        let off8 = db.iconst(Ty::I64, 8);
        let p1 = db.ptr_add(Flags::NONE, base, off8);
        let q_ptr = db.select(Flags::NONE, same, base, p1);
        let _ = db.call(Flags::NONE, callee, callee_sig, &[base, q_ptr]);
        let r0 = db.load(Ty::I64, Flags::NONE, base, 0, MemAttrs::aligned(3));
        let r1 = db.load(Ty::I64, Flags::NONE, base, 8, MemAttrs::aligned(3));
        db.ret(&[r0, r1]);
        let driver = db.finish_checked().expect("valid driver");

        struct OneCallee<'a> {
            view: View<'a>,
        }
        impl Bodies for OneCallee<'_> {
            fn callee(&self, _from: BodyId, reference: RefId) -> Option<(BodyId, View<'_>)> {
                match reference {
                    RefId(0) => Some((BodyId([2; 32]), self.view)),
                    _ => None,
                }
            }
        }

        let root = BodyId([1; 32]);
        let fuel = 10_000;
        let x: u128 = 0x1111_2222_3333_4444;
        let y: u128 = 0xAAAA_BBBB_CCCC_DDDD;
        for (same_flag, want0, want1) in [(0u128, y, x), (1u128, x, y)] {
            let args = [Val::Int(x), Val::Int(y), Val::Int(same_flag)];
            let before =
                run_calls_in(driver.view(), root, &args, fuel, &OneCallee { view: swap_fn.view() }, alloc)
                    .expect("eval unoptimized swap");
            let after =
                run_calls_in(driver.view(), root, &args, fuel, &OneCallee { view: after_ae.view() }, alloc)
                    .expect("eval optimized swap");
            assert_eq!(before, after, "swap mismatch for same={same_flag}");
            assert!(
                matches!(after, Outcome::Returned(ref vals) if vals.as_slice() == [Val::Int(want0), Val::Int(want1)]),
                "unexpected swap result for same={same_flag}: {after:?}"
            );
        }

        // Also test direct partial-overlap `Op::Memmove(buf + 4, buf + 0, 8)` via `eval::run_in`.
        let mut ob = Builder::<_, Full>::new_in(64, alloc);
        let ov_sig = ob.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        ob.set_signature(ov_sig);
        let o_entry = ob.create_block(BlockKind::Normal);
        ob.begin(o_entry);
        let val = ob.param(Ty::I64);
        let buf = ob.slot(12, 2, SlotKind::Named);
        let bp = ob.slot_addr(buf);
        ob.store(Flags::NONE, val, bp, 0, MemAttrs::aligned(2));
        let off4 = ob.iconst(Ty::I64, 4);
        let dst4 = ob.ptr_add(Flags::NONE, bp, off4);
        let len8 = ob.iconst(Ty::I64, 8);
        let al4 = MemAttrs { align: 2, src_align: 2, pointee_align: 0, bits: 0, range: None };
        ob.bulk(Op::Memmove, Flags::NONE, dst4, bp, len8, al4);
        let moved = ob.load(Ty::I64, Flags::NONE, dst4, 0, MemAttrs::aligned(2));
        ob.ret(&[moved]);
        let ov_fn = ob.finish_checked().expect("valid overlap fn");
        let ov_opt = scalarize_small_memops(ov_fn.view(), alloc)
            .expect("scalarize overlap succeeds")
            .expect("must scalarize overlap memmove");
        let ov_before = run_in(ov_fn.view(), &[Val::Int(x)], fuel, alloc).expect("eval overlap before");
        let ov_after = run_in(ov_opt.view(), &[Val::Int(x)], fuel, alloc).expect("eval overlap after");
        assert_eq!(ov_before, ov_after);
        assert!(matches!(ov_after, Outcome::Returned(ref vals) if vals.as_slice() == [Val::Int(x)]));
    }

    #[test]
    fn scalarize_small_memops_skips_volatile_and_large_sizes() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR), Param::of(Ty::PTR)], &[]);
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let dst = b.param(Ty::PTR);
        let src = b.param(Ty::PTR);
        let len8 = b.iconst(Ty::I64, 8);
        let len16 = b.iconst(Ty::I64, 16);
        let len3 = b.iconst(Ty::I64, 3);
        let attrs = MemAttrs { align: 3, src_align: 3, pointee_align: 0, bits: 0, range: None };
        b.bulk(Op::Memmove, Flags::VOLATILE, dst, src, len8, attrs);
        b.bulk(Op::Memmove, Flags::NONE, dst, src, len16, attrs);
        b.bulk(Op::Memcpy, Flags::VOLATILE, dst, src, len8, attrs);
        b.bulk(Op::Memcpy, Flags::NONE, dst, src, len3, attrs);
        b.ret(&[]);
        let func = b.finish_checked().expect("valid input");

        let out = scalarize_small_memops(func.view(), alloc).expect("pass succeeds");
        assert!(out.is_none(), "volatile 8B and non-1/2/4/8B memops must return Ok(None)");
    }

    #[test]
    fn memopt_forwards_wide_store_to_byte_loads_and_reconstructs_from_le_bytes() {
        use crate::eval::{Outcome, Val, run_in};

        let alloc = blaze_own::testing::heap().alloc();

        // Exact `SeqEntry::unpack()` pattern:
        // `Store(I64, sp, x)` -> 8x `Load(I8, sp+0..8)` (mixing direct offsets and `PtrAdd`)
        // -> 4x `Store(I8, sp2+0..4)` -> `Load(I32, sp2)`
        // + 2x `Store(I8, sp3+0..2)` -> `Load(I16, sp3)`
        // + `Load(I16, sp+6)` (wide->medium subword load)
        // + 8x `Store(I8, sp4+0..8)` (reversed bytes) -> `Load(I64, sp4)`
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64)],
            &[Param::of(Ty::I32), Param::of(Ty::I16), Param::of(Ty::I16), Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let x = b.param(Ty::I64);

        let s1 = b.slot(8, 3, SlotKind::Temp);
        let sp = b.slot_addr(s1);
        b.store(Flags::NONE, x, sp, 0, MemAttrs::aligned(3));
        let b0 = b.load(Ty::I8, Flags::NONE, sp, 0, MemAttrs::default());
        let b1 = b.load(Ty::I8, Flags::NONE, sp, 1, MemAttrs::default());
        let b2 = b.load(Ty::I8, Flags::NONE, sp, 2, MemAttrs::default());
        let b3 = b.load(Ty::I8, Flags::NONE, sp, 3, MemAttrs::default());
        let off4 = b.iconst(Ty::I64, 4);
        let sp_4 = b.ptr_add(Flags::NONE, sp, off4);
        let b4 = b.load(Ty::I8, Flags::NONE, sp_4, 0, MemAttrs::default());
        let b5 = b.load(Ty::I8, Flags::NONE, sp_4, 1, MemAttrs::default());
        let b6 = b.load(Ty::I8, Flags::NONE, sp_4, 2, MemAttrs::default());
        let b7 = b.load(Ty::I8, Flags::NONE, sp_4, 3, MemAttrs::default());
        let hi16 = b.load(Ty::I16, Flags::NONE, sp, 6, MemAttrs::aligned(1));

        let s2 = b.slot(4, 2, SlotKind::Temp);
        let sp2 = b.slot_addr(s2);
        b.store(Flags::NONE, b0, sp2, 0, MemAttrs::default());
        b.store(Flags::NONE, b1, sp2, 1, MemAttrs::default());
        b.store(Flags::NONE, b2, sp2, 2, MemAttrs::default());
        b.store(Flags::NONE, b3, sp2, 3, MemAttrs::default());
        let w32 = b.load(Ty::I32, Flags::NONE, sp2, 0, MemAttrs::aligned(2));

        let s3 = b.slot(2, 1, SlotKind::Temp);
        let sp3 = b.slot_addr(s3);
        b.store(Flags::NONE, b4, sp3, 0, MemAttrs::default());
        b.store(Flags::NONE, b5, sp3, 1, MemAttrs::default());
        let w16 = b.load(Ty::I16, Flags::NONE, sp3, 0, MemAttrs::aligned(1));

        let s4 = b.slot(8, 3, SlotKind::Temp);
        let sp4 = b.slot_addr(s4);
        b.store(Flags::NONE, b7, sp4, 0, MemAttrs::default());
        b.store(Flags::NONE, b6, sp4, 1, MemAttrs::default());
        b.store(Flags::NONE, b5, sp4, 2, MemAttrs::default());
        b.store(Flags::NONE, b4, sp4, 3, MemAttrs::default());
        b.store(Flags::NONE, b3, sp4, 4, MemAttrs::default());
        b.store(Flags::NONE, b2, sp4, 5, MemAttrs::default());
        b.store(Flags::NONE, b1, sp4, 6, MemAttrs::default());
        b.store(Flags::NONE, b0, sp4, 7, MemAttrs::default());
        let swapped64 = b.load(Ty::I64, Flags::NONE, sp4, 0, MemAttrs::aligned(3));

        b.ret(&[w32, w16, hi16, swapped64]);
        let func = b.finish_checked().expect("valid to_le_bytes/from_le_bytes input");

        let out = optimize_memory(func.view(), alloc).expect("memopt succeeds");
        assert!(verify(out.func.view()).is_ok(), "optimized function must verify");

        let opt_view = out.func.view();
        for inst in 0..opt_view.len() {
            assert_ne!(opt_view.op(inst), Op::Load, "all stack Op::Load instructions must be eliminated");
            assert_ne!(opt_view.op(inst), Op::Store, "dead stack Op::Store instructions must be eliminated");
        }

        let fuel = 10_000;
        for x_val in [0x0123_4567_89ab_cdefu64, u64::MAX, 0u64] {
            let args = [Val::Int(u128::from(x_val))];
            let before = run_in(func.view(), &args, fuel, alloc).expect("eval before memopt");
            let after = run_in(opt_view, &args, fuel, alloc).expect("eval after memopt");
            assert_eq!(before, after, "eval mismatch for x={x_val:#018x}");

            let want_w32 = u128::from(x_val & 0xffff_ffff);
            let want_w16 = u128::from((x_val >> 32) & 0xffff);
            let want_hi16 = u128::from((x_val >> 48) & 0xffff);
            let want_swapped = u128::from(x_val.swap_bytes());
            assert!(
                matches!(
                    after,
                    Outcome::Returned(ref vals)
                        if vals.as_slice()
                            == [
                                Val::Int(want_w32),
                                Val::Int(want_w16),
                                Val::Int(want_hi16),
                                Val::Int(want_swapped),
                            ]
                ),
                "unexpected result for x={x_val:#018x}: {after:?}"
            );
        }
    }

    #[test]
    fn memopt_invalidates_subword_forwarding_on_partial_overwrite() {
        use crate::eval::{Outcome, Val, run_in};

        let alloc = blaze_own::testing::heap().alloc();

        // Negative soundness test:
        // 1. `Store(I64, sp, v)` followed by partial overwrite `Store(I8, sp+2, 0xaa)`
        //    then `Load(I32, sp)`, `Load(I8, sp+2)`, and `Load(I8, sp+0)` — must never forward stale bytes!
        // 2. 4x `Store(I8, sp2+0..4)` followed by partial overwrite `Store(I16, sp2+2, 0xbeef)`
        //    then `Load(I32, sp2)` — must invalidate overwritten bytes in `byte_stores`!
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64)],
            &[Param::of(Ty::I32), Param::of(Ty::I8), Param::of(Ty::I8), Param::of(Ty::I32)],
        );
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let v = b.param(Ty::I64);

        let s1 = b.slot(8, 3, SlotKind::Temp);
        let sp = b.slot_addr(s1);
        b.store(Flags::NONE, v, sp, 0, MemAttrs::aligned(3));
        let patch_byte = b.iconst(Ty::I8, 0xaa);
        b.store(Flags::NONE, patch_byte, sp, 2, MemAttrs::default());
        let patched_i32 = b.load(Ty::I32, Flags::NONE, sp, 0, MemAttrs::aligned(2));
        let patched_b2 = b.load(Ty::I8, Flags::NONE, sp, 2, MemAttrs::default());
        let orig_b0 = b.load(Ty::I8, Flags::NONE, sp, 0, MemAttrs::default());

        let s2 = b.slot(4, 2, SlotKind::Temp);
        let sp2 = b.slot_addr(s2);
        let byte_a = b.iconst(Ty::I8, 0x11);
        let byte_b = b.iconst(Ty::I8, 0x22);
        let byte_c = b.iconst(Ty::I8, 0x33);
        let byte_d = b.iconst(Ty::I8, 0x44);
        b.store(Flags::NONE, byte_a, sp2, 0, MemAttrs::default());
        b.store(Flags::NONE, byte_b, sp2, 1, MemAttrs::default());
        b.store(Flags::NONE, byte_c, sp2, 2, MemAttrs::default());
        b.store(Flags::NONE, byte_d, sp2, 3, MemAttrs::default());
        let patch_hi = b.iconst(Ty::I16, 0xbeef);
        b.store(Flags::NONE, patch_hi, sp2, 2, MemAttrs::aligned(1));
        let patched_s2 = b.load(Ty::I32, Flags::NONE, sp2, 0, MemAttrs::aligned(2));

        b.ret(&[patched_i32, patched_b2, orig_b0, patched_s2]);
        let func = b.finish_checked().expect("valid partial-overwrite input");

        let out = optimize_memory(func.view(), alloc).expect("memopt succeeds");
        assert!(verify(out.func.view()).is_ok(), "optimized function must verify");

        let fuel = 10_000;
        for v_val in [0x0123_4567_89ab_cdefu64, u64::MAX, 0u64] {
            let args = [Val::Int(u128::from(v_val))];
            let before = run_in(func.view(), &args, fuel, alloc).expect("eval before memopt");
            let after = run_in(out.func.view(), &args, fuel, alloc).expect("eval after memopt");
            assert_eq!(before, after, "partial-overwrite mismatch for v={v_val:#018x}");

            let want_i32 = u128::from(((v_val & 0xffff_ffff) & 0xff00_ffff) | 0x00aa_0000);
            let want_b2 = 0xaau128;
            let want_b0 = u128::from(v_val & 0xff);
            let want_s2 = 0xbeef_2211u128;
            assert!(
                matches!(
                    after,
                    Outcome::Returned(ref vals)
                        if vals.as_slice()
                            == [
                                Val::Int(want_i32),
                                Val::Int(want_b2),
                                Val::Int(want_b0),
                                Val::Int(want_s2),
                            ]
                ),
                "unexpected partial-overwrite result for v={v_val:#018x}: {after:?}"
            );
        }
    }

    #[test]
    fn scalarize_slot_memcpy_12b_seqop_eliminates_slot_via_sroa() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[
                Param::of(Ty::PTR),
                Param::of(Ty::I32),
                Param::of(Ty::I32),
                Param::of(Ty::I32),
            ],
            &[],
        );
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let ext_dst = b.param(Ty::PTR);
        let lit_len = b.param(Ty::I32);
        let match_len = b.param(Ty::I32);
        let offset = b.param(Ty::I32);

        // 12-byte local slot (align = 4 = 1 << 2): exact `ptr::write(seq_dst, SeqOp { .. })` pattern.
        let slot = b.slot(12, 2, SlotKind::Temp);
        let sp = b.slot_addr(slot);
        b.store(Flags::NONE, lit_len, sp, 0, MemAttrs::aligned(2));
        b.store(Flags::NONE, match_len, sp, 4, MemAttrs::aligned(2));
        b.store(Flags::NONE, offset, sp, 8, MemAttrs::aligned(2));
        let len12 = b.iconst(Ty::I64, 12);
        let al4 = MemAttrs { align: 2, src_align: 2, pointee_align: 0, bits: 0, range: None };
        b.bulk(Op::Memcpy, Flags::NONE, ext_dst, sp, len12, al4);
        b.ret(&[]);
        let func = b.finish_checked().expect("valid 12B SeqOp input");

        let scalarized = scalarize_small_memops(func.view(), alloc)
            .expect("scalarize_small_memops succeeds")
            .expect("must scalarize 12B slot memcpy");
        let after_sroa = crate::simplify::sroa::scalar_replace(scalarized.view(), alloc)
            .expect("sroa succeeds");
        verify(after_sroa.view()).expect("after_sroa verifies");
        assert!(after_sroa.slots.is_empty(), "12B SeqOp local slot must be eliminated by SROA");

        let view = after_sroa.view();
        let mut stores: Vec<(u32, u32, i32), _> = Vec::new_in(alloc);
        for inst in 0..view.len() {
            let op = view.op(inst);
            assert_ne!(op, Op::Memcpy, "no Op::Memcpy may remain after scalarize + sroa");
            assert_ne!(op, Op::Load, "no Op::Load may remain after scalarize + sroa");
            assert_ne!(op, Op::SlotAddr, "no Op::SlotAddr may remain after scalarize + sroa");
            if op == Op::Store {
                let at = to_idx(at_u32(view.data, inst));
                let val = at_u32(view.extra, at);
                let ptr = at_u32(view.extra, at.saturating_add(1));
                let off = at_u32(view.extra, at.saturating_add(2)).cast_signed();
                stores.push((val, ptr, off));
            }
        }
        assert_eq!(
            stores.as_slice(),
            &[
                (lit_len.0, ext_dst.0, 0),
                (match_len.0, ext_dst.0, 4),
                (offset.0, ext_dst.0, 8),
            ],
            "must emit exactly 3 I32 stores directly to ext_dst at +0, +4, +8"
        );
    }

    #[test]
    fn scalarize_slot_memmove_12b_overlap_preserves_bytes() {
        use crate::eval::{Outcome, Val, run_in};

        let alloc = blaze_own::testing::heap().alloc();
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[
                Param::of(Ty::I32),
                Param::of(Ty::I32),
                Param::of(Ty::I32),
                Param::of(Ty::I32),
            ],
            &[
                Param::of(Ty::I32),
                Param::of(Ty::I32),
                Param::of(Ty::I32),
                Param::of(Ty::I32),
            ],
        );
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let a = b.param(Ty::I32);
        let c1 = b.param(Ty::I32);
        let c2 = b.param(Ty::I32);
        let d = b.param(Ty::I32);

        let buf = b.slot(16, 2, SlotKind::Named);
        let bp = b.slot_addr(buf);
        b.store(Flags::NONE, a, bp, 0, MemAttrs::aligned(2));
        b.store(Flags::NONE, c1, bp, 4, MemAttrs::aligned(2));
        b.store(Flags::NONE, c2, bp, 8, MemAttrs::aligned(2));
        b.store(Flags::NONE, d, bp, 12, MemAttrs::aligned(2));
        let off4 = b.iconst(Ty::I64, 4);
        let dst4 = b.ptr_add(Flags::NONE, bp, off4);
        let len12 = b.iconst(Ty::I64, 12);
        let al4 = MemAttrs { align: 2, src_align: 2, pointee_align: 0, bits: 0, range: None };
        b.bulk(Op::Memmove, Flags::NONE, dst4, bp, len12, al4);
        let r0 = b.load(Ty::I32, Flags::NONE, bp, 0, MemAttrs::aligned(2));
        let r1 = b.load(Ty::I32, Flags::NONE, bp, 4, MemAttrs::aligned(2));
        let r2 = b.load(Ty::I32, Flags::NONE, bp, 8, MemAttrs::aligned(2));
        let r3 = b.load(Ty::I32, Flags::NONE, bp, 12, MemAttrs::aligned(2));
        b.ret(&[r0, r1, r2, r3]);
        let func = b.finish_checked().expect("valid overlapping 12B memmove input");

        let opt = scalarize_small_memops(func.view(), alloc)
            .expect("scalarize_small_memops succeeds")
            .expect("must scalarize 12B slot memmove");
        verify(opt.view()).expect("scalarized overlap memmove verifies");
        for inst in 0..opt.view().len() {
            assert_ne!(opt.view().op(inst), Op::Memmove, "no Op::Memmove may remain");
        }

        let fuel = 10_000;
        let args = [
            Val::Int(0x1111_2222),
            Val::Int(0x3333_4444),
            Val::Int(0x5555_6666),
            Val::Int(0x7777_8888),
        ];
        let before = run_in(func.view(), &args, fuel, alloc).expect("eval before scalarize");
        let after = run_in(opt.view(), &args, fuel, alloc).expect("eval after scalarize");
        assert_eq!(before, after, "overlapping 12B memmove must preserve exact bytes");
        assert!(
            matches!(
                after,
                Outcome::Returned(ref vals)
                    if vals.as_slice()
                        == [
                            Val::Int(0x1111_2222),
                            Val::Int(0x1111_2222),
                            Val::Int(0x3333_4444),
                            Val::Int(0x5555_6666),
                        ]
            ),
            "unexpected overlapping 12B memmove result: {after:?}"
        );
    }

    #[test]
    fn scalarize_non_slot_16b_memcpy_stays_bulk() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR), Param::of(Ty::PTR)], &[]);
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let dst = b.param(Ty::PTR);
        let src = b.param(Ty::PTR);
        let len16 = b.iconst(Ty::I64, 16);
        let al8 = MemAttrs { align: 3, src_align: 3, pointee_align: 0, bits: 0, range: None };
        b.bulk(Op::Memcpy, Flags::NONE, dst, src, len16, al8);
        b.ret(&[]);
        let func = b.finish_checked().expect("valid non-slot 16B memcpy input");

        let out = scalarize_small_memops(func.view(), alloc).expect("pass succeeds");
        assert!(out.is_none(), "non-slot 16B memcpy must remain bulk for NEON ldr q; str q");
    }

    #[test]
    fn scalarize_slot_to_slot_padded_tuple_preserves_sroa() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I64), Param::of(Ty::I32)],
            &[Param::of(Ty::I64), Param::of(Ty::I32)],
        );
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let idx = b.param(Ty::I64);
        let tag = b.param(Ty::I32);

        // `(usize, u32)`: 16B slot with 4B trailing padding at +12..16.
        let s_src = b.slot(16, 3, SlotKind::Temp);
        let s_dst = b.slot(16, 3, SlotKind::Temp);
        let p_src = b.slot_addr(s_src);
        let p_dst = b.slot_addr(s_dst);
        b.store(Flags::NONE, idx, p_src, 0, MemAttrs::aligned(3));
        b.store(Flags::NONE, tag, p_src, 8, MemAttrs::aligned(2));
        let len16 = b.iconst(Ty::I64, 16);
        let al8 = MemAttrs { align: 3, src_align: 3, pointee_align: 0, bits: 0, range: None };
        b.bulk(Op::Memcpy, Flags::NONE, p_dst, p_src, len16, al8);
        let r0 = b.load(Ty::I64, Flags::NONE, p_dst, 0, MemAttrs::aligned(3));
        let r1 = b.load(Ty::I32, Flags::NONE, p_dst, 8, MemAttrs::aligned(2));
        b.ret(&[r0, r1]);
        let func = b.finish_checked().expect("valid padded tuple slot-to-slot input");

        let out = scalarize_small_memops(func.view(), alloc).expect("pass succeeds");
        assert!(
            out.is_none(),
            "slot-to-slot padded tuple copy must be left intact for sroa::scalar_replace"
        );
        let after_sroa =
            crate::simplify::sroa::scalar_replace(func.view(), alloc).expect("sroa succeeds");
        verify(after_sroa.view()).expect("after_sroa verifies");
        assert!(
            after_sroa.slots.is_empty(),
            "both padded tuple slots must be eliminated by SROA"
        );
    }

    #[test]
    fn forward_mixed_width_u16_u8_u8_stores_to_u32_load_and_dse() {
        use crate::eval::{Outcome, Val, run_in};

        let alloc = blaze_own::testing::heap().alloc();

        // Exact `FseDecEntry` pattern: out-of-order stores `u8 @ +2`, `u8 @ +3`, `u16 @ +0` ->
        // `u32 @ +0`, plus 64-bit mixed-width `u16 @ +4`, `u8 @ +7`, `u32 @ +0`, `u8 @ +6` ->
        // `u64 @ +0`.
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[
                Param::of(Ty::I16),
                Param::of(Ty::I8),
                Param::of(Ty::I8),
                Param::of(Ty::I32),
            ],
            &[Param::of(Ty::I32), Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let lo = b.param(Ty::I16);
        let b2 = b.param(Ty::I8);
        let b3 = b.param(Ty::I8);
        let lo32 = b.param(Ty::I32);

        let s1 = b.slot(4, 2, SlotKind::Temp);
        let sp1 = b.slot_addr(s1);
        b.store(Flags::NONE, b2, sp1, 2, MemAttrs::default());
        b.store(Flags::NONE, b3, sp1, 3, MemAttrs::default());
        b.store(Flags::NONE, lo, sp1, 0, MemAttrs::aligned(1));
        let w32 = b.load(Ty::I32, Flags::NONE, sp1, 0, MemAttrs::aligned(2));

        let s2 = b.slot(8, 3, SlotKind::Temp);
        let sp2 = b.slot_addr(s2);
        b.store(Flags::NONE, lo, sp2, 4, MemAttrs::aligned(1));
        b.store(Flags::NONE, b3, sp2, 7, MemAttrs::default());
        b.store(Flags::NONE, lo32, sp2, 0, MemAttrs::aligned(2));
        b.store(Flags::NONE, b2, sp2, 6, MemAttrs::default());
        let w64 = b.load(Ty::I64, Flags::NONE, sp2, 0, MemAttrs::aligned(3));

        b.ret(&[w32, w64]);
        let func = b.finish_checked().expect("valid mixed-width input");

        let out = optimize_memory(func.view(), alloc).expect("memopt succeeds");
        assert!(verify(out.func.view()).is_ok(), "optimized mixed-width function must verify");

        let opt_view = out.func.view();
        for inst in 0..opt_view.len() {
            assert_ne!(
                opt_view.op(inst),
                Op::Load,
                "mixed-width stack loads must be forwarded and eliminated"
            );
            assert_ne!(
                opt_view.op(inst),
                Op::Store,
                "dead mixed-width stack stores must be eliminated by DSE"
            );
        }

        let fuel = 10_000;
        let cases = [
            (0u16, 0u8, 0u8, 0u32),
            (u16::MAX, u8::MAX, u8::MAX, u32::MAX),
            (0x1234u16, 0x56u8, 0x78u8, 0x9abc_def0u32),
            (0x8001u16, 0x80u8, 0x01u8, 0x8000_0001u32),
        ];
        for (lo_v, b2_v, b3_v, lo32_v) in cases {
            let args = [
                Val::Int(u128::from(lo_v)),
                Val::Int(u128::from(b2_v)),
                Val::Int(u128::from(b3_v)),
                Val::Int(u128::from(lo32_v)),
            ];
            let before = run_in(func.view(), &args, fuel, alloc).expect("eval before memopt");
            let after = run_in(opt_view, &args, fuel, alloc).expect("eval after memopt");
            assert_eq!(before, after, "eval mismatch for ({lo_v:#x}, {b2_v:#x}, {b3_v:#x}, {lo32_v:#x})");

            let want32 =
                u128::from(u32::from(lo_v) | (u32::from(b2_v) << 16) | (u32::from(b3_v) << 24));
            let want64 = u128::from(
                u64::from(lo32_v)
                    | (u64::from(lo_v) << 32)
                    | (u64::from(b2_v) << 48)
                    | (u64::from(b3_v) << 56),
            );
            assert!(
                matches!(
                    after,
                    Outcome::Returned(ref vals)
                        if vals.as_slice() == [Val::Int(want32), Val::Int(want64)]
                ),
                "unexpected result for ({lo_v:#x}, {b2_v:#x}, {b3_v:#x}, {lo32_v:#x}): {after:?}"
            );
        }

        // Negative / soundness case: a newer overlapping `u8 @ +1` partially overwrites `u16 @ +0`
        // (leaving byte 0 uncovered in active_stores). Must NOT mis-forward stale byte 1!
        let mut nb = Builder::<_, Full>::new_in(64, alloc);
        let nsig = nb.sig(
            Conv::C,
            Variadic::No,
            &[
                Param::of(Ty::I16),
                Param::of(Ty::I8),
                Param::of(Ty::I8),
                Param::of(Ty::I8),
            ],
            &[Param::of(Ty::I32)],
        );
        nb.set_signature(nsig);
        let nentry = nb.create_block(BlockKind::Normal);
        nb.begin(nentry);
        let n_lo = nb.param(Ty::I16);
        let n_b2 = nb.param(Ty::I8);
        let n_b3 = nb.param(Ty::I8);
        let patch_b1 = nb.param(Ty::I8);

        let ns = nb.slot(4, 2, SlotKind::Temp);
        let nsp = nb.slot_addr(ns);
        nb.store(Flags::NONE, n_b2, nsp, 2, MemAttrs::default());
        nb.store(Flags::NONE, n_b3, nsp, 3, MemAttrs::default());
        nb.store(Flags::NONE, n_lo, nsp, 0, MemAttrs::aligned(1));
        nb.store(Flags::NONE, patch_b1, nsp, 1, MemAttrs::default());
        let nw32 = nb.load(Ty::I32, Flags::NONE, nsp, 0, MemAttrs::aligned(2));
        nb.ret(&[nw32]);
        let nfunc = nb.finish_checked().expect("valid partial-overwrite mixed-width input");

        let nout = optimize_memory(nfunc.view(), alloc).expect("memopt succeeds on partial overwrite");
        assert!(verify(nout.func.view()).is_ok(), "partial-overwrite optimized function must verify");
        let nopt_view = nout.func.view();
        let has_load = (0..nopt_view.len()).any(|i| nopt_view.op(i) == Op::Load);
        assert!(has_load, "partially overwritten u16 @ +0 by u8 @ +1 must NOT be forwarded");

        for (lo_v, b2_v, b3_v, patch_v) in [
            (0x1234u16, 0x56u8, 0x78u8, 0xaau8),
            (0xffffu16, 0x00u8, 0xffu8, 0x00u8),
            (0x00ffu16, 0x80u8, 0x01u8, 0x7fu8),
        ] {
            let args = [
                Val::Int(u128::from(lo_v)),
                Val::Int(u128::from(b2_v)),
                Val::Int(u128::from(b3_v)),
                Val::Int(u128::from(patch_v)),
            ];
            let before = run_in(nfunc.view(), &args, fuel, alloc).expect("eval partial before");
            let after = run_in(nopt_view, &args, fuel, alloc).expect("eval partial after");
            assert_eq!(before, after, "partial-overwrite mismatch");

            let want = u128::from(
                (u32::from(lo_v) & 0xff)
                    | (u32::from(patch_v) << 8)
                    | (u32::from(b2_v) << 16)
                    | (u32::from(b3_v) << 24),
            );
            assert!(
                matches!(after, Outcome::Returned(ref vals) if vals.as_slice() == [Val::Int(want)]),
                "unexpected partial-overwrite result: {after:?}"
            );
        }
    }

    #[test]
    fn elides_chained_temp_memset_and_memcpy() {
        use crate::eval::{Outcome, Val, run_in};

        let alloc = blaze_own::testing::heap().alloc();

        // Exact `[0u32; 256]` lowering chain:
        // `memset(t1, fill, 1024) -> memcpy(t2, t1, 1024) -> memcpy(out, t2, 1024)`
        // with `slot_live` / `slot_dead` markers and pre-poisoned `out`.
        let mut b = Builder::<_, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::I8)],
            &[Param::of(Ty::I32), Param::of(Ty::I32)],
        );
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let fill = b.param(Ty::I8);

        let out_slot = b.slot(1024, 2, SlotKind::Named);
        let t1 = b.slot(1024, 2, SlotKind::Temp);
        let t2 = b.slot(1024, 2, SlotKind::Temp);
        let p_out = b.slot_addr(out_slot);
        let poison = b.iconst(Ty::I32, 0xdead_beef);
        b.store(Flags::NONE, poison, p_out, 0, MemAttrs::aligned(2));
        b.store(Flags::NONE, poison, p_out, 1020, MemAttrs::aligned(2));

        let n1024 = b.iconst(Ty::I64, 1024);
        let copy_al = MemAttrs { align: 2, src_align: 2, pointee_align: 0, bits: 0, range: None };

        b.slot_live(t1);
        let p_t1 = b.slot_addr(t1);
        b.bulk(Op::Memset, Flags::NONE, p_t1, fill, n1024, MemAttrs::aligned(2));

        b.slot_live(t2);
        let p_t2 = b.slot_addr(t2);
        b.bulk(Op::Memcpy, Flags::NONE, p_t2, p_t1, n1024, copy_al);
        b.slot_dead(t1);

        b.bulk(Op::Memcpy, Flags::NONE, p_out, p_t2, n1024, copy_al);
        b.slot_dead(t2);

        let first = b.load(Ty::I32, Flags::NONE, p_out, 0, MemAttrs::aligned(2));
        let last = b.load(Ty::I32, Flags::NONE, p_out, 1020, MemAttrs::aligned(2));
        b.ret(&[first, last]);
        let func = b.finish_checked().expect("valid chained memset/memcpy input");

        let (elided_fn, elided_count) =
            eliminate_temp_bulk_copies(func.view().to_func_in(alloc), alloc)
                .expect("eliminate_temp_bulk_copies succeeds");
        assert_eq!(elided_count, 2, "must elide both temp slots t1 and t2");
        verify(elided_fn.view()).expect("elided function verifies");
        assert_eq!(elided_fn.slots.len(), 1, "both 1024B temp slots must be removed");

        let ev = elided_fn.view();
        let mut memsets = 0u32;
        let mut memcpys = 0u32;
        let mut memmoves = 0u32;
        let mut memset_inst = None;
        for inst in 0..ev.len() {
            match ev.op(inst) {
                Op::Memset => {
                    memsets += 1;
                    memset_inst = Some(inst);
                }
                Op::Memcpy => memcpys += 1,
                Op::Memmove => memmoves += 1,
                _ => {}
            }
        }
        assert_eq!(memsets, 1, "must collapse to exactly 1 Op::Memset(out, fill, 1024)");
        assert_eq!(memcpys, 0, "no Op::Memcpy may remain");
        assert_eq!(memmoves, 0, "no Op::Memmove may remain");
        let ms_idx = memset_inst.expect("memset present");
        let ms_attrs = ev.mem_attrs(ms_idx).expect("memset must keep dst align");
        assert_eq!(ms_attrs.align, 2, "dst align must be preserved");
        assert_eq!(ms_attrs.src_align, 0, "memset src_align must be 0");

        // Also verify both pipeline entry points (`scalarize_small_memops` and `optimize_memory`) hit it.
        let via_scalarize = scalarize_small_memops(func.view(), alloc)
            .expect("scalarize_small_memops succeeds")
            .expect("scalarize_small_memops must elide chained temp memset");
        verify(via_scalarize.view()).expect("scalarize output verifies");
        assert_eq!(via_scalarize.slots.len(), 1);
        assert!(
            !(0..via_scalarize.view().len()).any(|i| via_scalarize.view().op(i) == Op::Memcpy),
            "scalarize_small_memops must eliminate all chained Op::Memcpy"
        );

        let via_memopt = optimize_memory(func.view(), alloc).expect("optimize_memory succeeds");
        verify(via_memopt.func.view()).expect("optimize_memory output verifies");
        assert_eq!(via_memopt.stats.temp_copies_elided, 2);
        assert_eq!(via_memopt.func.slots.len(), 1);
        assert!(
            !(0..via_memopt.func.view().len()).any(|i| via_memopt.func.view().op(i) == Op::Memcpy),
            "optimize_memory must eliminate all chained Op::Memcpy"
        );

        let fuel = 50_000;
        for (byte_val, want_word) in [(0x00u8, 0x0000_0000u32), (0x5au8, 0x5a5a_5a5au32)] {
            let args = [Val::Int(u128::from(byte_val))];
            let before = run_in(func.view(), &args, fuel, alloc).expect("eval before elision");
            let after = run_in(ev, &args, fuel, alloc).expect("eval after elision");
            assert_eq!(before, after, "eval mismatch for fill={byte_val:#x}");
            let want = u128::from(want_word);
            assert!(
                matches!(
                    after,
                    Outcome::Returned(ref vals) if vals.as_slice() == [Val::Int(want), Val::Int(want)]
                ),
                "unexpected eval result for fill={byte_val:#x}: {after:?}"
            );
        }
    }

    #[test]
    fn elides_temp_memcpy_to_memcpy_and_respects_aliasing() {
        use crate::eval::{Outcome, Val, run_in};

        let alloc = blaze_own::testing::heap().alloc();
        let fuel = 50_000;

        // 1. Positive (provably disjoint local slots): `memcpy(t1, src, 512) -> memcpy(dst, t1, 512)`
        //    collapses to 1 `Op::Memcpy(dst, src, 512)` and drops `t1`.
        {
            let mut b = Builder::<_, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64), Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let v0 = b.param(Ty::I64);
            let v1 = b.param(Ty::I64);

            let src_slot = b.slot(512, 3, SlotKind::Named);
            let dst_slot = b.slot(512, 3, SlotKind::Named);
            let t1 = b.slot(512, 3, SlotKind::Temp);
            let p_src = b.slot_addr(src_slot);
            let p_dst = b.slot_addr(dst_slot);
            let p_t1 = b.slot_addr(t1);
            b.store(Flags::NONE, v0, p_src, 0, MemAttrs::aligned(3));
            b.store(Flags::NONE, v1, p_src, 504, MemAttrs::aligned(3));

            let n512 = b.iconst(Ty::I64, 512);
            let al8 = MemAttrs { align: 3, src_align: 3, pointee_align: 0, bits: 0, range: None };
            b.bulk(Op::Memcpy, Flags::NONE, p_t1, p_src, n512, al8);
            b.bulk(Op::Memcpy, Flags::NONE, p_dst, p_t1, n512, al8);
            b.slot_dead(t1);

            let r0 = b.load(Ty::I64, Flags::NONE, p_dst, 0, MemAttrs::aligned(3));
            let r1 = b.load(Ty::I64, Flags::NONE, p_dst, 504, MemAttrs::aligned(3));
            b.ret(&[r0, r1]);
            let func = b.finish_checked().expect("valid disjoint memcpy chain");

            let out = optimize_memory(func.view(), alloc).expect("optimize_memory succeeds");
            assert_eq!(out.stats.temp_copies_elided, 1);
            verify(out.func.view()).expect("optimized disjoint copy verifies");
            assert_eq!(out.func.slots.len(), 2, "temp slot t1 must be dropped");

            let ov = out.func.view();
            let memcpys = (0..ov.len()).filter(|&i| ov.op(i) == Op::Memcpy).count();
            let memmoves = (0..ov.len()).filter(|&i| ov.op(i) == Op::Memmove).count();
            assert_eq!(memcpys, 1, "disjoint slots must collapse to 1 Op::Memcpy");
            assert_eq!(memmoves, 0, "disjoint slots need not downgrade to Op::Memmove");

            let args = [Val::Int(0x1122_3344_5566_7788), Val::Int(0x99aa_bbcc_ddee_ff00)];
            let before = run_in(func.view(), &args, fuel, alloc).expect("eval disjoint before");
            let after = run_in(ov, &args, fuel, alloc).expect("eval disjoint after");
            assert_eq!(before, after);
            assert!(matches!(
                after,
                Outcome::Returned(ref vals)
                    if vals.as_slice()
                        == [Val::Int(0x1122_3344_5566_7788), Val::Int(0x99aa_bbcc_ddee_ff00)]
            ));
        }

        // 2. Positive (potentially aliasing external params): `memcpy(t1, src, 512) -> memcpy(dst, t1, 512)`
        //    must downgrade to overlap-safe `Op::Memmove(dst, src, 512)` with `dst_align` and `src_align`.
        {
            let mut b = Builder::<_, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR), Param::of(Ty::PTR)], &[]);
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let dst = b.param(Ty::PTR);
            let src = b.param(Ty::PTR);
            let t1 = b.slot(512, 3, SlotKind::Temp);
            let p_t1 = b.slot_addr(t1);
            let n512 = b.iconst(Ty::I64, 512);
            let w_attrs = MemAttrs { align: 3, src_align: 2, pointee_align: 0, bits: 0, range: None };
            let r_attrs = MemAttrs { align: 1, src_align: 3, pointee_align: 0, bits: 0, range: None };
            b.bulk(Op::Memcpy, Flags::NONE, p_t1, src, n512, w_attrs);
            b.bulk(Op::Memcpy, Flags::NONE, dst, p_t1, n512, r_attrs);
            b.ret(&[]);
            let func = b.finish_checked().expect("valid external param copy");

            let (opt, elided) =
                eliminate_temp_bulk_copies(func, alloc).expect("elide external param copy");
            assert_eq!(elided, 1);
            verify(opt.view()).expect("external param memmove verifies");
            assert!(opt.slots.is_empty(), "temp slot t1 must be dropped");

            let ov = opt.view();
            let memcpys = (0..ov.len()).filter(|&i| ov.op(i) == Op::Memcpy).count();
            let mm_idx = (0..ov.len()).find(|&i| ov.op(i) == Op::Memmove).expect("must emit Memmove");
            assert_eq!(memcpys, 0, "potentially aliasing external params must use Op::Memmove");
            let attrs = ov.mem_attrs(mm_idx).expect("must preserve alignments");
            assert_eq!(attrs.align, 1, "must keep dst align from reader");
            assert_eq!(attrs.src_align, 2, "must keep src align from writer");
        }

        // 3. Negative control: intervening store to `src` between `memcpy`s must block elision.
        {
            let mut b = Builder::<_, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let old_v = b.param(Ty::I64);
            let new_v = b.param(Ty::I64);
            let src_slot = b.slot(512, 3, SlotKind::Named);
            let dst_slot = b.slot(512, 3, SlotKind::Named);
            let t1 = b.slot(512, 3, SlotKind::Temp);
            let p_src = b.slot_addr(src_slot);
            let p_dst = b.slot_addr(dst_slot);
            let p_t1 = b.slot_addr(t1);
            let n512 = b.iconst(Ty::I64, 512);
            let al8 = MemAttrs { align: 3, src_align: 3, pointee_align: 0, bits: 0, range: None };
            b.store(Flags::NONE, old_v, p_src, 0, MemAttrs::aligned(3));
            b.bulk(Op::Memcpy, Flags::NONE, p_t1, p_src, n512, al8);
            b.store(Flags::NONE, new_v, p_src, 0, MemAttrs::aligned(3));
            b.bulk(Op::Memcpy, Flags::NONE, p_dst, p_t1, n512, al8);
            let out_v = b.load(Ty::I64, Flags::NONE, p_dst, 0, MemAttrs::aligned(3));
            b.ret(&[out_v]);
            let func = b.finish_checked().expect("valid intervening-store input");

            let (opt, elided) =
                eliminate_temp_bulk_copies(func.view().to_func_in(alloc), alloc)
                    .expect("pass succeeds on intervening store");
            assert_eq!(elided, 0, "intervening store between memcpys must block elision");
            let memcpys = (0..opt.view().len()).filter(|&i| opt.view().op(i) == Op::Memcpy).count();
            assert_eq!(memcpys, 2, "both Op::Memcpys must remain");

            let args = [Val::Int(0xaaaa), Val::Int(0xbbbb)];
            let after = run_in(opt.view(), &args, fuel, alloc).expect("eval intervening store");
            assert!(
                matches!(after, Outcome::Returned(ref vals) if vals.as_slice() == [Val::Int(0xaaaa)]),
                "must observe pre-store snapshot 0xaaaa, got {after:?}"
            );
        }

        // 4. Negative control: escaping `t1` passed to a call must block elision.
        {
            let mut b = Builder::<_, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR), Param::of(Ty::PTR)], &[]);
            b.set_signature(sig);
            let sink_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR)], &[]);
            let sink = b.reference(crate::func::RefKind::Func, 0);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let dst = b.param(Ty::PTR);
            let src = b.param(Ty::PTR);
            let t1 = b.slot(512, 3, SlotKind::Temp);
            let p_t1 = b.slot_addr(t1);
            let n512 = b.iconst(Ty::I64, 512);
            let al8 = MemAttrs { align: 3, src_align: 3, pointee_align: 0, bits: 0, range: None };
            b.bulk(Op::Memcpy, Flags::NONE, p_t1, src, n512, al8);
            b.bulk(Op::Memcpy, Flags::NONE, dst, p_t1, n512, al8);
            let _ = b.call(Flags::NONE, sink, sink_sig, &[p_t1]);
            b.ret(&[]);
            let func = b.finish_checked().expect("valid escaping-slot input");

            let (opt, elided) =
                eliminate_temp_bulk_copies(func, alloc).expect("pass succeeds on escaping slot");
            assert_eq!(elided, 0, "escaping temp slot must not be elided");
            let memcpys = (0..opt.view().len()).filter(|&i| opt.view().op(i) == Op::Memcpy).count();
            assert_eq!(memcpys, 2, "both Op::Memcpys must remain when slot escapes");
        }

        // 5. Negative control: volatile bulk writer or reader must block elision.
        {
            let mut b = Builder::<_, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR), Param::of(Ty::PTR)], &[]);
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let dst = b.param(Ty::PTR);
            let src = b.param(Ty::PTR);
            let t1 = b.slot(512, 3, SlotKind::Temp);
            let t2 = b.slot(512, 3, SlotKind::Temp);
            let p_t1 = b.slot_addr(t1);
            let p_t2 = b.slot_addr(t2);
            let zero = b.iconst(Ty::I8, 0);
            let n512 = b.iconst(Ty::I64, 512);
            let al8 = MemAttrs { align: 3, src_align: 3, pointee_align: 0, bits: 0, range: None };
            b.bulk(Op::Memset, Flags::VOLATILE, p_t1, zero, n512, MemAttrs::aligned(3));
            b.bulk(Op::Memcpy, Flags::NONE, dst, p_t1, n512, al8);
            b.bulk(Op::Memcpy, Flags::NONE, p_t2, src, n512, al8);
            b.bulk(Op::Memcpy, Flags::VOLATILE, dst, p_t2, n512, al8);
            b.ret(&[]);
            let func = b.finish_checked().expect("valid volatile bulk input");

            let (opt, elided) =
                eliminate_temp_bulk_copies(func, alloc).expect("pass succeeds on volatile bulk");
            assert_eq!(elided, 0, "volatile bulk ops must not be elided");
            let memsets = (0..opt.view().len()).filter(|&i| opt.view().op(i) == Op::Memset).count();
            let memcpys = (0..opt.view().len()).filter(|&i| opt.view().op(i) == Op::Memcpy).count();
            assert_eq!(memsets, 1, "volatile memset must remain");
            assert_eq!(memcpys, 3, "all three memcpys must remain");
        }
    }
}
