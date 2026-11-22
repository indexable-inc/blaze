//! `memcpy`, `memmove` and `memset` run on this Mac, inline and called, and the measurement
//! behind [`crate::BULK_INLINE_MOST`].

#![cfg(target_arch = "aarch64")]

use std::alloc::Global;

use blaze_lir::{
    Builder, Flags, Full, IntCond, Op, Ty,
    builder::{MemAttrs, Param, Variadic},
    func::{BlockKind, Conv, Func},
};

use crate::{
    BULK_INLINE_MOST, Code, RelocTo, Routine, emit_inline_up_to,
    tests::{ISA, run},
};

unsafe extern "C" {
    fn memcpy(dst: *mut u8, src: *const u8, n: usize) -> *mut u8;
    fn memmove(dst: *mut u8, src: *const u8, n: usize) -> *mut u8;
    fn memset(dst: *mut u8, byte: i32, n: usize) -> *mut u8;
}

fn routine(to: RelocTo) -> u64 {
    match to {
        RelocTo::Routine(Routine::Memcpy) => memcpy as usize as u64,
        RelocTo::Routine(Routine::Memmove) => memmove as usize as u64,
        RelocTo::Routine(Routine::Memset) => memset as usize as u64,
        other => panic!("an unexpected relocation {other:?}"),
    }
}

fn emitted_up_to(func: &Func<Global>, most: u64) -> Code<Global> {
    let mut code = Code::new_in(Global);
    emit_inline_up_to(blaze_lir::verify(func.view()).expect("the test's function is LIR"), &[], &crate::RoutinePlaces::NONE, ISA, most, &mut code).expect("emits");
    code
}

fn calls_a_routine(code: &Code<Global>) -> bool {
    code.relocs.iter().any(|r| matches!(r.to, RelocTo::Routine(_)))
}

/// `f(dst, src) = op(dst, src, len)`, `len` a constant (or, for `None`, `f(dst, src, len)`).
pub(crate) fn bulk(op: Op, len: Option<u64>) -> Func<Global> {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let src_ty = match op {
        Op::Memset => Ty::I8,
        _ => Ty::PTR,
    };
    let sig = b.sig(
        Conv::C,
        Variadic::No,
        &[Param::of(Ty::PTR), Param::of(src_ty), Param::of(Ty::I64)],
        &[Param::of(Ty::I64)],
    );
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let (dst, src, given) = (b.param(Ty::PTR), b.param(src_ty), b.param(Ty::I64));
    let n = match len {
        Some(n) => b.iconst(Ty::I64, u128::from(n)),
        None => given,
    };
    b.bulk(op, Flags::NONE, dst, src, n, MemAttrs::default());
    let zero = b.iconst(Ty::I64, 0);
    b.ret(&[zero]);
    b.finish_checked().expect("builds")
}

const LENS: [u64; 22] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 11, 15, 16, 17, 31, 33, 63, 64, 65, 127, 128, 129];

/// Each length inline and called: exactly the bytes asked are written, overlapping moves in
/// both directions included.
#[test]
fn bulk_operations_write_exactly_their_bytes() {
    for op in [Op::Memcpy, Op::Memmove, Op::Memset] {
        for &n in &LENS {
            for (len, most) in [(Some(n), BULK_INLINE_MOST), (Some(n), 0), (None, BULK_INLINE_MOST)] {
                let func = bulk(op, len);
                let code = emitted_up_to(&func, most);
                let inline = len.is_some_and(|n| n <= most && (op != Op::Memmove || n <= crate::MOVE_INLINE_MOST));
                assert_eq!(!calls_a_routine(&code), inline, "{op:?} {len:?} up to {most}");
                let exec = run::place_all(&code, |_| unreachable!(), routine);
                // Distinct bytes, a guard on each side of the written stretch.
                let fresh = || (0..512u32).map(|i| (i * 7 + 3) as u8).collect::<Vec<u8>>();
                let cases: &[(usize, usize)] = match op {
                    // (dst, src) offsets in one buffer: apart, and overlapping either way.
                    Op::Memmove => &[(300, 100), (100, 103), (103, 100), (64, 64 + 8), (72, 64)],
                    _ => &[(300, 100)],
                };
                for &(d, s) in cases {
                    let mut buf = fresh();
                    let mut want = fresh();
                    let base = buf.as_mut_ptr() as u64;
                    let n_ = n as usize;
                    let src_arg = match op {
                        Op::Memset => {
                            want[d..d + n_].fill(0xc3);
                            0xc3
                        }
                        _ => {
                            want.copy_within(s..s + n_, d);
                            base + s as u64
                        }
                    };
                    run::call(&exec, [base + d as u64, src_arg, n, 0]);
                    assert_eq!(buf, want, "{op:?} of {n} bytes to +{d} from +{s}, {len:?} up to {most}");
                }
            }
        }
    }
}

/// Volatile bulk memory stays refused by name.
#[test]
fn volatile_bulk_is_refused() {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR), Param::of(Ty::PTR)], &[Param::of(Ty::I64)]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let (dst, src) = (b.param(Ty::PTR), b.param(Ty::PTR));
    let n = b.iconst(Ty::I64, 8);
    b.bulk(Op::Memcpy, Flags::VOLATILE, dst, src, n, MemAttrs::default());
    let zero = b.iconst(Ty::I64, 0);
    b.ret(&[zero]);
    let func = b.finish_checked().expect("builds");
    let mut code = Code::new_in(Global);
    let refused = crate::emit(blaze_lir::verify(func.view()).expect("the test's function is LIR"), &[], ISA, &mut code).err();
    assert!(matches!(refused, Some(crate::EmitError::Unsupported { op: Op::Memcpy, .. })), "{refused:?}");
}

/// `f(dst, src, count)`: `count` copies of `n` bytes, `src` to `dst`.
fn copy_loop(n: u64) -> Func<Global> {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(
        Conv::C,
        Variadic::No,
        &[Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::I64)],
        &[Param::of(Ty::I64)],
    );
    b.set_signature(sig);
    let (entry, head, body, exit) = (
        b.create_block(BlockKind::Normal),
        b.create_block(BlockKind::Normal),
        b.create_block(BlockKind::Normal),
        b.create_block(BlockKind::Normal),
    );
    b.begin(entry);
    let (dst, src, count) = (b.param(Ty::PTR), b.param(Ty::PTR), b.param(Ty::I64));
    let zero = b.iconst(Ty::I64, 0);
    b.jump((head, &[zero]));
    b.begin(head);
    let i = b.param(Ty::I64);
    let done = b.icmp(IntCond::Eq, i, count);
    b.br(Flags::NONE, done, (exit, &[]), (body, &[]));
    b.begin(body);
    let len = b.iconst(Ty::I64, u128::from(n));
    b.bulk(Op::Memcpy, Flags::NONE, dst, src, len, MemAttrs::default());
    let one = b.iconst(Ty::I64, 1);
    let next = b.binary(Op::Add, Flags::NONE, i, one);
    b.jump((head, &[next]));
    b.begin(exit);
    let zero = b.iconst(Ty::I64, 0);
    b.ret(&[zero]);
    b.finish_checked().expect("builds")
}

/// The measurement behind [`BULK_INLINE_MOST`]: 10^7 copies of each size from 8 to 256 bytes,
/// inline and through `memcpy`, in this tier's code (each value through its stack cell). Run
/// with `--ignored --nocapture inline_or_call`.
#[test]
#[ignore = "a measurement, printed"]
fn inline_or_call() {
    const COUNT: u64 = 10_000_000;
    let src = vec![0x5au8; 256];
    let mut dst = vec![0u8; 256];
    println!("bytes  inline_ns_per_copy  call_ns_per_copy");
    for n in (8..=256).step_by(8) {
        let func = copy_loop(n);
        let mut times = [0f64; 2];
        for (k, most) in [n, 0].into_iter().enumerate() {
            let code = emitted_up_to(&func, most);
            assert_eq!(calls_a_routine(&code), most == 0);
            let exec = run::place_all(&code, |_| unreachable!(), routine);
            let args = [dst.as_mut_ptr() as u64, src.as_ptr() as u64, COUNT, 0];
            run::call(&exec, [args[0], args[1], 1000, 0]);
            let start = std::time::Instant::now();
            run::call(&exec, args);
            times[k] = start.elapsed().as_secs_f64() * 1e9 / COUNT as f64;
        }
        println!("{n:5}  {:18.2}  {:16.2}", times[0], times[1]);
    }
    assert_eq!(dst, src);
}
