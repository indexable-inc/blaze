//! Atomics of an `i128` (pass/atomic.rs `atomic_wide`), run on this Mac with the LSE `casp`
//! and with the exclusive-pair loops: loads and stores of both halves, every `atomic_rmw` kind
//! against Rust's own `u128`/`i128` arithmetic, a `cmpxchg` that succeeds and one that fails on
//! the high half only, and two threads racing `fetch_add(1)` across the 64-bit carry to an
//! exact count. The control (rule 23): with the wide path planted off (`WIDE_OFF`), the same
//! function is refused by name as before.
//!
//! Every function here is built by `tests::one` and emitted by `tests::emitted_for` (which
//! also builds it as a code record and checks it: record_tests.rs `holds`).

#![cfg(target_arch = "aarch64")]

use blaze_lir::{AtomicRmw, Flags, IntCond, Op, Ordering, Ty, builder::MemAttrs, func::BlockKind};

use crate::{
    EmitError, Isa, Os,
    pass::WIDE_OFF,
    tests::{emitted_for, one, run},
};

fn isa(lse: bool) -> Isa {
    Isa { lse, os: crate::tests::HOST_OS }
}

/// 16-byte aligned, as `AtomicU128` is (an exclusive pair access faults otherwise).
fn wide() -> MemAttrs {
    MemAttrs::aligned(4)
}

/// A 16-aligned `u128` the emitted code reaches by its address.
#[repr(C, align(16))]
struct Cell16(u128);

/// A `u128` as its two words, low first: what AAPCS64 passes in an `x` pair.
const fn split(v: u128) -> (u64, u64) {
    (v as u64, (v >> 64) as u64)
}

/// What `rmw` stores, of `old` and `v`.
fn apply(rmw: AtomicRmw, old: u128, v: u128) -> u128 {
    match rmw {
        AtomicRmw::Xchg => v,
        AtomicRmw::Add => old.wrapping_add(v),
        AtomicRmw::Sub => old.wrapping_sub(v),
        AtomicRmw::And => old & v,
        AtomicRmw::Nand => !(old & v),
        AtomicRmw::Or => old | v,
        AtomicRmw::Xor => old ^ v,
        AtomicRmw::Max => (old as i128).max(v as i128) as u128,
        AtomicRmw::Min => (old as i128).min(v as i128) as u128,
        AtomicRmw::Umax => old.max(v),
        AtomicRmw::Umin => old.min(v),
    }
}

/// Values whose halves differ, cross the carry and the sign: each a case for every kind.
const VALUES: [u128; 7] = [
    0,
    1,
    u64::MAX as u128,
    1 << 64,
    (1 << 127) | 5,
    u128::MAX,
    0x0123_4567_89ab_cdef_fedc_ba98_7654_3210,
];

/// `atomic_load` and `atomic_store` of an `i128` at each ordering they take: both halves read
/// and written, nothing past them touched. Prints each form's bytes (the floor: one exclusive
/// loop, `ld{a}xp; st{l}xp; cbnz`, three words, beside the cell moves).
///
/// Prints: `atomic_load i128 Relaxed: N bytes` (and the rest), then
/// `test atomic_wide_tests::wide_loads_and_stores_run ... ok`
#[test]
fn wide_loads_and_stores_run() {
    for ordering in [Ordering::Relaxed, Ordering::Acquire, Ordering::SeqCst] {
        let load = one(&[Ty::PTR], Ty::I128, |b, v| b.atomic_load(Ty::I128, Flags::NONE.with_ordering(ordering), v[0], wide()));
        let code = emitted_for(load.view(), &[], isa(true)).unwrap_or_else(|e| panic!("an i128 load emits: {e}"));
        println!("atomic_load i128 {ordering:?}: {} bytes", code.bytes.len());
        let exec = run::place(&code, |_| unreachable!("no literal word"));
        // SAFETY: an emitted C function of a pointer returning an `i128` in x0:x1 (AAPCS64
        // C.9), mapped while `exec` lives; the pointer is a live 16-aligned cell.
        let f = unsafe { std::mem::transmute::<usize, extern "C" fn(*mut Cell16) -> u128>(exec.code_address() as usize) };
        let mut cell = Cell16(0x1111_2222_3333_4444_5555_6666_7777_8888);
        assert_eq!(f(&mut cell), 0x1111_2222_3333_4444_5555_6666_7777_8888, "a {ordering:?} load of i128");
        assert_eq!(cell.0, 0x1111_2222_3333_4444_5555_6666_7777_8888, "a load leaves the cell as it was");
    }
    for ordering in [Ordering::Relaxed, Ordering::Release, Ordering::SeqCst] {
        let store = one(&[Ty::PTR, Ty::I128], Ty::I64, |b, v| {
            b.atomic_store(Flags::NONE.with_ordering(ordering), v[1], v[0], wide());
            b.iconst(Ty::I64, 0)
        });
        let code = emitted_for(store.view(), &[], isa(true)).unwrap_or_else(|e| panic!("an i128 store emits: {e}"));
        println!("atomic_store i128 {ordering:?}: {} bytes", code.bytes.len());
        let exec = run::place(&code, |_| unreachable!("no literal word"));
        // SAFETY: an emitted C function of a pointer (x0) and an `i128` (x2:x3, AAPCS64 C.8).
        let f = unsafe { std::mem::transmute::<usize, extern "C" fn(*mut Cell16, u128) -> u64>(exec.code_address() as usize) };
        let mut cells = [Cell16(u128::MAX), Cell16(u128::MAX), Cell16(u128::MAX)];
        f(&mut cells[1], 0xaaaa_bbbb_cccc_dddd_0000_1111_2222_3333);
        assert_eq!(cells[1].0, 0xaaaa_bbbb_cccc_dddd_0000_1111_2222_3333, "a {ordering:?} store of i128: both halves");
        assert_eq!((cells[0].0, cells[2].0), (u128::MAX, u128::MAX), "a {ordering:?} store of i128: nothing beside it");
    }
}

/// Every `atomic_rmw` kind and ordering of an `i128`, with LSE and without (both take the
/// exclusive loop: LSE has no 128-bit `ldadd`): the old value is returned, `old rmw v` stored.
///
/// Prints: `test atomic_wide_tests::every_wide_rmw_kind_stores_what_rust_computes ... ok`
#[test]
fn every_wide_rmw_kind_stores_what_rust_computes() {
    for lse in [true, false] {
        for rmw in AtomicRmw::ALL {
            for ordering in Ordering::ALL {
                let func = one(&[Ty::PTR, Ty::I128], Ty::I128, |b, v| {
                    b.atomic_rmw(rmw, Flags::NONE.with_ordering(ordering), v[0], v[1], wide())
                });
                let code = emitted_for(func.view(), &[], isa(lse)).unwrap_or_else(|e| panic!("{rmw:?} {ordering:?}: {e}"));
                let exec = run::place(&code, |_| unreachable!("no literal word"));
                // SAFETY: an emitted C function of a pointer (x0) and an `i128` (x2:x3)
                // returning an `i128` (x0:x1); the pointer is a live 16-aligned cell.
                let f = unsafe { std::mem::transmute::<usize, extern "C" fn(*mut Cell16, u128) -> u128>(exec.code_address() as usize) };
                for old in VALUES {
                    for v in VALUES {
                        let mut cell = Cell16(old);
                        let got = f(&mut cell, v);
                        let what = format!("{rmw:?} {ordering:?} lse {lse}: old {old:#x}, operand {v:#x}");
                        assert_eq!(got, old, "{what}: the returned old value");
                        assert_eq!(cell.0, apply(rmw, old, v), "{what}: the stored value");
                    }
                }
            }
        }
    }
}

/// `f(cell, flag_out, expected, new) -> old`: a `cmpxchg` of an `i128`, its flag stored to
/// `flag_out` as an `i64`.
fn exchanger(flags: Flags) -> blaze_lir::func::Func<impl core::alloc::Allocator + Clone> {
    one(&[Ty::PTR, Ty::PTR, Ty::I128, Ty::I128], Ty::I128, |b, v| {
        let pair = b.cmpxchg(flags, v[0], v[2], v[3], wide());
        let ok = b.project(pair, 1);
        let ok = b.convert(Op::Zext, Flags::NONE, Ty::I64, ok);
        b.store(Flags::NONE, ok, v[1], 0, MemAttrs::aligned(3));
        b.project(pair, 0)
    })
}

/// A `cmpxchg` of an `i128` that succeeds, one that fails on the high half only (the low
/// halves equal: a compare of the low word alone would store), and one that fails on the low
/// half only; with `casp` (LSE) and with the exclusive loop, at each ordering pair.
///
/// Prints: `cmpxchg i128 lse true: N bytes`, `cmpxchg i128 lse false: N bytes`, then
/// `test atomic_wide_tests::wide_cmpxchg_compares_both_halves ... ok`
#[test]
fn wide_cmpxchg_compares_both_halves() {
    let pairs = [
        (Ordering::SeqCst, Ordering::SeqCst),
        (Ordering::Relaxed, Ordering::Relaxed),
        (Ordering::Acquire, Ordering::Acquire),
        (Ordering::Release, Ordering::Relaxed),
        (Ordering::AcqRel, Ordering::Acquire),
    ];
    let old: u128 = (1 << 64) | 5;
    let cases: [(u128, u128, bool, &str); 4] = [
        (old, 0xdead_beef_0000_0000_0000_0000_0000_0009, true, "equal"),
        ((2 << 64) | 5, 9, false, "the high half differs only"),
        ((1 << 64) | 6, 9, false, "the low half differs only"),
        (u128::MAX, 0, false, "both differ"),
    ];
    for lse in [true, false] {
        for (i, (success, failure)) in pairs.into_iter().enumerate() {
            let flags = Flags::NONE.with_ordering(success).with_failure(failure);
            let func = exchanger(flags);
            let code = emitted_for(func.view(), &[], isa(lse)).unwrap_or_else(|e| panic!("an i128 cmpxchg emits: {e}"));
            if i == 0 {
                println!("cmpxchg i128 lse {lse}: {} bytes", code.bytes.len());
            }
            let exec = run::place(&code, |_| unreachable!("no literal word"));
            // SAFETY: an emitted C function of two pointers (x0, x1) and two `i128`s (x2:x3,
            // x4:x5) returning an `i128` (x0:x1); both pointers are live and aligned.
            let f = unsafe {
                std::mem::transmute::<usize, extern "C" fn(*mut Cell16, *mut u64, u128, u128) -> u128>(exec.code_address() as usize)
            };
            for (expected, new, ok, what) in cases {
                let mut cell = Cell16(old);
                let mut flag = 0xffu64;
                let got = f(&mut cell, &mut flag, expected, new);
                let what = format!("{success:?}/{failure:?} lse {lse}: {what}");
                assert_eq!(got, old, "{what}: the old value");
                assert_eq!(flag, u64::from(ok), "{what}: the flag");
                assert_eq!(cell.0, if ok { new } else { old }, "{what}: the cell");
            }
        }
    }
}

/// `f(ptr, n)`: `n` `fetch_add(1)`s of an `i128` at `ptr`, in a loop.
fn wide_adder(ordering: Ordering) -> blaze_lir::func::Func<impl core::alloc::Allocator + Clone> {
    one(&[Ty::PTR, Ty::I64], Ty::I64, |b, v| {
        let (head, body, exit) = (b.create_block(BlockKind::Normal), b.create_block(BlockKind::Normal), b.create_block(BlockKind::Normal));
        let zero = b.iconst(Ty::I64, 0);
        b.jump((head, &[zero]));
        b.begin(head);
        let i = b.param(Ty::I64);
        let done = b.icmp(IntCond::Eq, i, v[1]);
        b.br(Flags::NONE, done, (exit, &[]), (body, &[]));
        b.begin(body);
        let one = b.iconst(Ty::I128, 1);
        b.atomic_rmw(AtomicRmw::Add, Flags::NONE.with_ordering(ordering), v[0], one, wide());
        let step = b.iconst(Ty::I64, 1);
        let next = b.binary(Op::Add, Flags::NONE, i, step);
        b.jump((head, &[next]));
        b.begin(exit);
        b.iconst(Ty::I64, 0)
    })
}

/// Two threads each add 1 N times to one `i128` starting N below 2^64: it ends at exactly
/// 2^64 + N, so the carry out of the low word happened inside the race and was kept (a loop
/// that stored the halves apart, or added the high half without the carry, ends elsewhere).
///
/// Prints: `test atomic_wide_tests::two_threads_racing_wide_fetch_adds_cross_the_carry ... ok`
#[test]
fn two_threads_racing_wide_fetch_adds_cross_the_carry() {
    const N: u64 = 20_000;
    for lse in [true, false] {
        for ordering in [Ordering::Relaxed, Ordering::SeqCst] {
            let func = wide_adder(ordering);
            let code = emitted_for(func.view(), &[], isa(lse)).unwrap_or_else(|e| panic!("the i128 adder emits: {e}"));
            let exec = run::place(&code, |_| unreachable!("no literal word"));
            let entry = exec.code_address() as usize;
            let mut counter = Cell16((1u128 << 64) - u128::from(N));
            let at = &mut counter as *mut Cell16 as usize;
            let gate = std::sync::Barrier::new(2);
            std::thread::scope(|s| {
                for _ in 0..2 {
                    s.spawn(|| {
                        // SAFETY: an emitted C function of a pointer and a count; the pointer is
                        // `counter`, alive until the scope ends, written only by the emitted
                        // exclusive loop and read after the scope.
                        let f = unsafe { std::mem::transmute::<usize, extern "C" fn(usize, u64) -> u64>(entry) };
                        gate.wait();
                        f(at, N);
                    });
                }
            });
            let (lo, hi) = split(counter.0);
            assert_eq!((lo, hi), (N, 1), "{ordering:?} lse {lse}: 2^64 - N + 2N is 2^64 + N");
        }
    }
}

/// THE CONTROL (rule 23): the wide path planted off on this thread, an `i128` atomic load,
/// store, rmw and cmpxchg are each refused as before `atomic_wide`, naming the value.
///
/// Prints: `test atomic_wide_tests::the_wide_path_off_refuses_by_name ... ok`
#[test]
fn the_wide_path_off_refuses_by_name() {
    const OLD: &str = "a value that is not an integer or pointer of at most 64 bits (i128, a float, a vector, a compound)";
    let funcs = [
        (Op::AtomicLoad, one(&[Ty::PTR], Ty::I128, |b, v| b.atomic_load(Ty::I128, Flags::NONE.with_ordering(Ordering::SeqCst), v[0], wide()))),
        (
            Op::AtomicStore,
            one(&[Ty::PTR, Ty::I128], Ty::I64, |b, v| {
                b.atomic_store(Flags::NONE.with_ordering(Ordering::SeqCst), v[1], v[0], wide());
                b.iconst(Ty::I64, 0)
            }),
        ),
        (
            Op::AtomicRmw,
            one(&[Ty::PTR, Ty::I128], Ty::I128, |b, v| {
                b.atomic_rmw(AtomicRmw::Add, Flags::NONE.with_ordering(Ordering::SeqCst), v[0], v[1], wide())
            }),
        ),
    ];
    WIDE_OFF.with(|off| off.set(true));
    for (op, func) in funcs.iter() {
        for lse in [true, false] {
            let got = emitted_for(func.view(), &[], isa(lse)).map(|_| ());
            assert!(
                matches!(got, Err(EmitError::Unsupported { op: o, why, .. }) if o == *op && why == OLD),
                "{op:?} lse {lse} with the wide path off: {got:?}"
            );
        }
    }
    let cmpxchg = exchanger(Flags::NONE.with_ordering(Ordering::SeqCst).with_failure(Ordering::SeqCst));
    let got = emitted_for(cmpxchg.view(), &[], isa(true)).map(|_| ());
    WIDE_OFF.with(|off| off.set(false));
    assert!(matches!(got, Err(EmitError::Unsupported { op: Op::Cmpxchg, why, .. }) if why == OLD), "cmpxchg with the wide path off: {got:?}");
}
