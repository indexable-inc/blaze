//! Atomics run on this Mac, with the LSE forms and with the exclusive load/store loops: every
//! `atomic_rmw` kind at every size and ordering against Rust's own arithmetic, `cmpxchg`
//! succeeding and failing, ordered loads and stores, fences, and two threads racing
//! `fetch_add`s to an exact total.

#![cfg(target_arch = "aarch64")]

use std::alloc::Global;
use std::arch::is_aarch64_feature_detected;

use blaze_lir::{
    AtomicRmw, Builder, Flags, Full, IntCond, Op, Ordering, Ty, Value,
    builder::{MemAttrs, Param, Variadic},
    func::{BlockKind, Conv, Func},
};

use crate::{
    Isa, Os,
    tests::{emitted_for, mask, run},
};

const SIZES: [Ty; 4] = [Ty::I8, Ty::I16, Ty::I32, Ty::I64];

fn isa(lse: bool) -> Isa {
    Isa { lse, os: crate::tests::HOST_OS }
}

/// Whether the host AArch64 CPU implements FEAT_LSE (`cas`/`ldadd`/`swp`/etc.). Emitting LSE
/// code is always tested, but executing it is skipped on ARMv8.0 hosts lacking LSE to avoid
/// `SIGILL`.
fn host_lse() -> bool {
    is_aarch64_feature_detected!("lse")
}

/// `f(params) = body(params)`, returning the values `body` gives, of types `rets`.
fn function(
    params: &[Ty],
    rets: &[Ty],
    body: impl FnOnce(&mut Builder<Global, Full>, &[Value]) -> Vec<Value>,
) -> Func<Global> {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig_params: Vec<Param> = params.iter().map(|&ty| Param::of(ty)).collect();
    let sig_rets: Vec<Param> = rets.iter().map(|&ty| Param::of(ty)).collect();
    let sig = b.sig(Conv::C, Variadic::No, &sig_params, &sig_rets);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let values: Vec<Value> = params.iter().map(|&ty| b.param(ty)).collect();
    let out = body(&mut b, &values);
    b.ret(&out);
    b.finish_checked().expect("a hand-built function builds")
}

fn attrs(ty: Ty) -> MemAttrs {
    MemAttrs::aligned((ty.bits(64) / 8).trailing_zeros() as u8)
}

/// What `rmw` stores, of `old` and `v` at `width` bits (both zero-extended).
fn apply(rmw: AtomicRmw, width: u32, old: u64, v: u64) -> u64 {
    let signed = |x: u64| ((x << (64 - width)) as i64) >> (64 - width);
    let out = match rmw {
        AtomicRmw::Xchg => v,
        AtomicRmw::Add => old.wrapping_add(v),
        AtomicRmw::Sub => old.wrapping_sub(v),
        AtomicRmw::And => old & v,
        AtomicRmw::Nand => !(old & v),
        AtomicRmw::Or => old | v,
        AtomicRmw::Xor => old ^ v,
        AtomicRmw::Max => signed(old).max(signed(v)) as u64,
        AtomicRmw::Min => signed(old).min(signed(v)) as u64,
        AtomicRmw::Umax => old.max(v),
        AtomicRmw::Umin => old.min(v),
    };
    out & mask(width)
}

/// Every kind, size and ordering, LSE and not: the old value is returned, the new one stored,
/// and the bytes past the access are untouched.
#[test]
fn every_rmw_kind_stores_what_rust_computes() {
    for lse in [true, false] {
        for ty in SIZES {
            let width = ty.bits(64);
            let values = crate::tests::edges(width);
            for rmw in AtomicRmw::ALL {
                for ordering in Ordering::ALL {
                    let func = function(&[Ty::PTR, ty], &[ty], |b, v| {
                        vec![b.atomic_rmw(rmw, Flags::NONE.with_ordering(ordering), v[0], v[1], attrs(ty))]
                    });
                    let code = emitted_for(func.view(), &[], isa(lse)).expect("emits");
                    if lse && !host_lse() {
                        continue;
                    }
                    let exec = run::place(&code, |_| unreachable!());
                    for &old in &values {
                        for &v in &values {
                            let (old, v) = (old as u64, v as u64);
                            let sentinel = 0xa5a5_a5a5_a5a5_a5a5 & !mask(width);
                            let mut cell = Box::new(old | sentinel);
                            let at = &mut *cell as *mut u64 as u64;
                            let got = run::call(&exec, [at, v, 0, 0]) & mask(width);
                            let what = format!("{rmw:?} {ordering:?} i{width} lse {lse}: old {old:#x}, operand {v:#x}");
                            assert_eq!(got, old, "{what}: the returned old value");
                            assert_eq!(*cell & mask(width), apply(rmw, width, old, v), "{what}: the stored value");
                            assert_eq!(*cell & !mask(width), sentinel, "{what}: the bytes past the access");
                        }
                    }
                }
            }
        }
    }
}

/// `cmpxchg` returns `(old, old == expected)` and stores only on success.
#[test]
fn cmpxchg_succeeds_and_fails() {
    let pairs = [
        (Ordering::SeqCst, Ordering::SeqCst),
        (Ordering::Relaxed, Ordering::Relaxed),
        (Ordering::Acquire, Ordering::Acquire),
        (Ordering::Release, Ordering::Relaxed),
        (Ordering::AcqRel, Ordering::Acquire),
        (Ordering::Relaxed, Ordering::SeqCst),
    ];
    for lse in [true, false] {
        for ty in SIZES {
            let width = ty.bits(64);
            for (success, failure) in pairs {
                for weak in [false, true] {
                    let flags = Flags::NONE.with_ordering(success).with_failure(failure);
                    let flags = match weak {
                        true => flags.union(Flags::WEAK),
                        false => flags,
                    };
                    let func = function(&[Ty::PTR, ty, ty], &[ty, Ty::I1], |b, v| {
                        let pair = b.cmpxchg(flags, v[0], v[1], v[2], attrs(ty));
                        vec![b.project(pair, 0), b.project(pair, 1)]
                    });
                    let code = emitted_for(func.view(), &[], isa(lse)).expect("emits");
                    if lse && !host_lse() {
                        continue;
                    }
                    let exec = run::place(&code, |_| unreachable!());
                    // SAFETY: an emitted C function of a pointer and two integers returning two
                    // integers, which AAPCS64 returns in x0 and x1 as it does this struct.
                    let f = unsafe {
                        core::mem::transmute::<usize, extern "C" fn(u64, u64, u64) -> Two>(exec.code_address() as usize)
                    };
                    let top = mask(width);
                    for (old, expected, new) in [(5u64, 5u64, 9u64), (5, 6, 9), (top, top, 0), (top, top - 1, 1), (0, 0, top)] {
                        let sentinel = 0x5a5a_5a5a_5a5a_5a5a & !top;
                        let mut cell = Box::new(old | sentinel);
                        let at = &mut *cell as *mut u64 as u64;
                        let Two(got, ok) = f(at, expected, new);
                        let what = format!("{success:?}/{failure:?} weak {weak} i{width} lse {lse}: {old:#x} {expected:#x} {new:#x}");
                        assert_eq!(got & top, old, "{what}: the old value");
                        assert_eq!(ok & 1 == 1, old == expected, "{what}: the flag");
                        let want = match old == expected {
                            true => new,
                            false => old,
                        };
                        assert_eq!(*cell, want | sentinel, "{what}: the cell");
                    }
                }
            }
        }
    }
}

/// A `cmpxchg` whose cell is past `sp + 32 KiB` (the 2048th value on) returns its old value:
/// before pass.rs `FAR` the far address was made in `x12`, the register the old value is held
/// in, and `st` wrote the cell's address as the old value. Padded by unused constants on each
/// side of the boundary; the control (rule 23): with `FAR` back at `X12` the cases from 2048
/// on fail on "the old value" and those below pass.
#[test]
fn far_cells_keep_cmpxchg_old_values() {
    let flags = Flags::NONE.with_ordering(Ordering::SeqCst).with_failure(Ordering::SeqCst);
    for lse in [true, false] {
        for pad in [2040u32, 2047, 2048, 4200] {
            let func = function(&[Ty::PTR, Ty::I64, Ty::I64], &[Ty::I64, Ty::I1], |b, v| {
                for n in 0..pad {
                    let _ = b.iconst(Ty::I64, u128::from(n));
                }
                let pair = b.cmpxchg(flags, v[0], v[1], v[2], attrs(Ty::I64));
                vec![b.project(pair, 0), b.project(pair, 1)]
            });
            let code = emitted_for(func.view(), &[], isa(lse)).expect("emits");
            if lse && !host_lse() {
                continue;
            }
            let exec = run::place(&code, |_| unreachable!());
            // SAFETY: an emitted C function of a pointer and two integers returning two
            // integers, which AAPCS64 returns in x0 and x1 as it does this struct.
            let f = unsafe { std::mem::transmute::<usize, extern "C" fn(u64, u64, u64) -> Two>(exec.code_address() as usize) };
            for (old, expected, new) in [(5u64, 5u64, 9u64), (5, 6, 9)] {
                let mut cell = Box::new(old);
                let at = &mut *cell as *mut u64 as u64;
                let Two(got, ok) = f(at, expected, new);
                let what = format!("lse {lse}, after {pad} values: {old:#x} {expected:#x} {new:#x}");
                assert_eq!(got, old, "{what}: the old value");
                assert_eq!(ok & 1 == 1, old == expected, "{what}: the flag");
                assert_eq!(*cell, if old == expected { new } else { old }, "{what}: the cell");
            }
        }
    }
}

#[repr(C)]
struct Two(u64, u64);

/// `atomic_load` and `atomic_store` at every size and ordering they may take, and every fence.
#[test]
fn ordered_loads_stores_and_fences_run() {
    let lse = host_lse();
    for ty in SIZES {
        let width = ty.bits(64);
        for ordering in [Ordering::Relaxed, Ordering::Acquire, Ordering::SeqCst] {
            let load = function(&[Ty::PTR], &[ty], |b, v| {
                vec![b.atomic_load(ty, Flags::NONE.with_ordering(ordering), v[0], attrs(ty))]
            });
            let exec = run::place(&emitted_for(load.view(), &[], isa(lse)).expect("emits"), |_| unreachable!());
            let cell = Box::new(0x0102_0304_0506_0708u64);
            let got = run::call(&exec, [&*cell as *const u64 as u64, 0, 0, 0]) & mask(width);
            assert_eq!(got, *cell & mask(width), "a {ordering:?} load of i{width}");
        }
        for ordering in [Ordering::Relaxed, Ordering::Release, Ordering::SeqCst] {
            let store = function(&[Ty::PTR, ty], &[Ty::I64], |b, v| {
                b.atomic_store(Flags::NONE.with_ordering(ordering), v[1], v[0], attrs(ty));
                vec![b.iconst(Ty::I64, 0)]
            });
            let exec = run::place(&emitted_for(store.view(), &[], isa(lse)).expect("emits"), |_| unreachable!());
            let mut cell = Box::new(u64::MAX);
            run::call(&exec, [&mut *cell as *mut u64 as u64, 0x1122_3344_5566_7788, 0, 0]);
            assert_eq!(*cell, (0x1122_3344_5566_7788 & mask(width)) | !mask(width), "a {ordering:?} store of i{width}");
        }
    }
    for single in [false, true] {
        for ordering in [Ordering::SeqCst, Ordering::Acquire, Ordering::Release, Ordering::AcqRel] {
            let flags = Flags::NONE.with_ordering(ordering);
            let flags = match single {
                true => flags.union(Flags::SINGLE_THREAD),
                false => flags,
            };
            let fence = function(&[Ty::I64], &[Ty::I64], |b, v| {
                b.fence(flags);
                vec![v[0]]
            });
            let exec = run::place(&emitted_for(fence.view(), &[], isa(lse)).expect("emits"), |_| unreachable!());
            assert_eq!(run::call(&exec, [7, 0, 0, 0]), 7, "a {ordering:?} fence (single thread {single})");
        }
    }
}

/// `f(ptr, n)`: `n` relaxed `fetch_add(1)`s of `ty` at `ptr`, in a loop.
fn adder(ty: Ty, ordering: Ordering) -> Func<Global> {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR), Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
    b.set_signature(sig);
    let (entry, head, body, exit) = (
        b.create_block(BlockKind::Normal),
        b.create_block(BlockKind::Normal),
        b.create_block(BlockKind::Normal),
        b.create_block(BlockKind::Normal),
    );
    b.begin(entry);
    let (ptr, n) = (b.param(Ty::PTR), b.param(Ty::I64));
    let zero = b.iconst(Ty::I64, 0);
    b.jump((head, &[zero]));
    b.begin(head);
    let i = b.param(Ty::I64);
    let done = b.icmp(IntCond::Eq, i, n);
    b.br(Flags::NONE, done, (exit, &[]), (body, &[]));
    b.begin(body);
    let one = b.iconst(ty, 1);
    b.atomic_rmw(AtomicRmw::Add, Flags::NONE.with_ordering(ordering), ptr, one, attrs(ty));
    let one = b.iconst(Ty::I64, 1);
    let next = b.binary(Op::Add, Flags::NONE, i, one);
    b.jump((head, &[next]));
    b.begin(exit);
    let zero = b.iconst(Ty::I64, 0);
    b.ret(&[zero]);
    b.finish_checked().expect("builds")
}

/// Two threads each add 1 N times to one counter: it ends at exactly 2N, with LSE and with the
/// exclusive loop.
#[test]
fn two_threads_racing_fetch_adds_reach_two_n() {
    const N: u64 = 20_000;
    for lse in [true, false] {
        for (ty, ordering) in [(Ty::I64, Ordering::Relaxed), (Ty::I64, Ordering::SeqCst), (Ty::I16, Ordering::AcqRel)] {
            let func = adder(ty, ordering);
            let code = emitted_for(func.view(), &[], isa(lse)).expect("emits");
            if lse && !host_lse() {
                continue;
            }
            let exec = run::place(&code, |_| unreachable!());
            let entry = exec.code_address() as usize;
            let counter = std::sync::atomic::AtomicU64::new(0);
            let at = counter.as_ptr() as usize;
            let gate = std::sync::Barrier::new(2);
            std::thread::scope(|s| {
                for _ in 0..2 {
                    s.spawn(|| {
                        // SAFETY: an emitted C function of a pointer and a count; the pointer is
                        // `counter`, alive until the scope ends, written only atomically.
                        let f = unsafe { std::mem::transmute::<usize, extern "C" fn(usize, u64) -> u64>(entry) };
                        gate.wait();
                        f(at, N);
                    });
                }
            });
            assert_eq!(counter.into_inner(), 2 * N, "i{} {ordering:?} lse {lse}", ty.bits(64));
        }
    }
}
