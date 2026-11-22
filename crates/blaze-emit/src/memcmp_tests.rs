//! `memcmp` (pass/bulk.rs) run on this Mac: inline for a constant length up to
//! [`crate::BULK_INLINE_MOST`], a call of the system's `memcmp` past it or for a length only
//! known at run time. Each result's sign is checked against C's contract (the first differing
//! byte pair compared as unsigned, 0 for no bytes): equal, the first byte differing, the last
//! byte differing, each both ways and with a byte above 0x7f (a signed compare would invert it),
//! from unaligned starts, and `n = 0`.

#![cfg(target_arch = "aarch64")]

use std::alloc::Global;

use blaze_lir::{
    Builder, Full, Ty,
    builder::{MemAttrs, Param, Variadic},
    func::{BlockKind, Conv, Func},
};

use crate::{
    BULK_INLINE_MOST, RelocTo, Routine,
    tests::{emitted, run},
};

unsafe extern "C" {
    fn memcmp(a: *const u8, b: *const u8, n: usize) -> i32;
}

/// `f(a, b, len) = memcmp(a, b, n)`, `n` the constant `known` or else the parameter `len`.
fn compare(known: Option<u64>) -> Func<Global> {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let params = [Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::I64)];
    let sig = b.sig(Conv::C, Variadic::No, &params, &[Param::of(Ty::I32)]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let (x, y, len) = (b.param(Ty::PTR), b.param(Ty::PTR), b.param(Ty::I64));
    let len = match known {
        Some(n) => b.iconst(Ty::I64, u128::from(n)),
        None => len,
    };
    let got = b.memcmp(x, y, len, MemAttrs::aligned(0));
    b.ret(&[got]);
    b.finish_checked().expect("builds")
}

/// Whether the emitted code calls `memcmp`, and the placed code.
fn placed(func: &Func<Global>) -> (bool, blaze_inproc::exec::Executable) {
    let code = emitted(func.view(), &[]).unwrap_or_else(|why| panic!("{why}\n{}", blaze_lir::print(func.view())));
    let calls = code.relocs.iter().any(|it| it.to == RelocTo::Routine(Routine::Memcmp));
    let exec = run::place_all(
        &code,
        |_| unreachable!("no literal words"),
        |to| match to {
            RelocTo::Routine(Routine::Memcmp) => memcmp as usize as u64,
            other => panic!("an unexpected relocation {other:?}"),
        },
    );
    (calls, exec)
}

/// The sign of `memcmp(a, b, n)` by C's contract, from the bytes.
fn sign(a: &[u8], b: &[u8]) -> i32 {
    match a.iter().zip(b).find(|(x, y)| x != y) {
        None => 0,
        Some((x, y)) if x > y => 1,
        Some(_) => -1,
    }
}

/// Every case for length `n` at start offsets `(oa, ob)`: `(a, b)` with `n` bytes from each
/// offset compared.
fn cases(n: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
    let base: Vec<u8> = (0..n).map(|i| (i * 37 + 11) as u8).collect();
    let mut out = vec![(base.clone(), base.clone())];
    for at in [0, n / 2, n.saturating_sub(1)] {
        if at >= n {
            continue;
        }
        for (x, y) in [(0x01u8, 0xffu8), (0xff, 0x01), (0x7f, 0x80), (0x80, 0x7f)] {
            let (mut a, mut b) = (base.clone(), base.clone());
            a[at] = x;
            b[at] = y;
            // A later difference the other way: only the first one counts.
            if at + 1 < n {
                a[at + 1] = 0;
                b[at + 1] = 0xff;
            }
            out.push((a, b));
        }
    }
    out
}

/// Runs `exec` (of length `known`, or the length passed) on every case of `n` bytes, from
/// starts 0..3 bytes past an 8-aligned address in each buffer.
fn check(exec: &blaze_inproc::exec::Executable, n: usize) {
    for (a, b) in cases(n) {
        for (oa, ob) in [(0, 0), (1, 0), (0, 3), (3, 1), (7, 5)] {
            // 8-aligned buffers with room for the offset; bytes outside `n` differ, so a read
            // past `n` would show.
            let mut ba = vec![0u64; n / 8 + 3];
            let mut bb = vec![u64::MAX; n / 8 + 3];
            // SAFETY: each `u64` buffer viewed as its bytes, alive for the call below.
            let (sa, sb) = unsafe {
                (
                    std::slice::from_raw_parts_mut(ba.as_mut_ptr().cast::<u8>(), ba.len() * 8),
                    std::slice::from_raw_parts_mut(bb.as_mut_ptr().cast::<u8>(), bb.len() * 8),
                )
            };
            sa[oa..oa + n].copy_from_slice(&a);
            sb[ob..ob + n].copy_from_slice(&b);
            let (pa, pb) = (sa[oa..].as_ptr() as u64, sb[ob..].as_ptr() as u64);
            let got = run::call(exec, [pa, pb, n as u64, 0]) as u32 as i32;
            assert_eq!(got.signum(), sign(&a, &b), "n {n} starts ({oa}, {ob}) a {a:x?} b {b:x?}");
        }
    }
}

/// Inline lengths around each piece size, the bound itself, and calls past it.
///
/// Expected: `test memcmp_tests::memcmp_of_a_constant_length_follows_c ... ok`.
#[test]
fn memcmp_of_a_constant_length_follows_c() {
    let most = BULK_INLINE_MOST as usize;
    for n in [0, 1, 2, 3, 4, 5, 7, 8, 9, 15, 16, 17, 31, 33, most, most + 1, 300] {
        let (calls, exec) = placed(&compare(Some(n as u64)));
        assert_eq!(calls, n > most, "n {n}: inline up to the bound, a call past it");
        check(&exec, n);
    }
}

/// A length known only at run time calls `memcmp`, `n = 0` included.
///
/// Expected: `test memcmp_tests::memcmp_of_a_run_time_length_calls_the_routine ... ok`.
#[test]
fn memcmp_of_a_run_time_length_calls_the_routine() {
    let (calls, exec) = placed(&compare(None));
    assert!(calls, "a run-time length calls `memcmp`");
    for n in [0, 1, 8, 13, 64, 200] {
        check(&exec, n);
    }
}
