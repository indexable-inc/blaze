//! Live Apple Silicon hardware verification & head-to-head benchmarks for `TierName::Sota`
//! against `blaze_lir::eval`, `Reference`, and `Fast`.

use std::alloc::Global;
use std::time::Instant;

use blaze_code::image::ImageError;
use blaze_code::place::{At, PlaceError, Words};
use blaze_code::record::{Space, Symbol, Target};
use blaze_inproc::exec::Executable;
use blaze_lir::{
    Flags, IntCond, Op, Ty,
    builder::{Builder, Full, Param, Variadic},
    eval::{self, Outcome, Val},
    func::{BlockKind, Conv},
    verify,
};
use blaze_own::{AVec, Alloc, Vec};
use blaze_tier::{
    ARM64, Arch, Emitted, Machine, Names, Os, Outside, RoutinePlaces, Table, Tier, TierName, optimizing_tier,
    record::{encode, place, read, seal},
};

use super::Sota;
use crate::compose::emit::POLL_PAGE;
use crate::table_gen::EncodedTable;
use crate::{Fast, Reference};

const TABLE: &[u8] = include_bytes!("../../table/arm64.bztab");

struct NoNames;
impl Names for NoNames {
    fn name(&self, _of: Outside) -> Option<blaze_tier::Named<'_>> {
        None
    }

    fn routines(&self) -> RoutinePlaces {
        RoutinePlaces::NONE
    }
}

#[repr(C, align(4096))]
struct PollPage([u8; 4096]);
static POLL: PollPage = PollPage([0; 4096]);

struct LeafWords;
impl Words for LeafWords {
    fn word(&mut self, target: Target, _call: bool) -> Result<u64, ImageError> {
        Err(ImageError::Unbound { target })
    }

    fn value(&mut self, target: Target) -> Result<u64, ImageError> {
        if target.space == Space::Outside && target.symbol == Symbol::of_name(POLL_PAGE) {
            Ok(core::ptr::addr_of!(POLL) as u64)
        } else {
            Err(ImageError::Unbound { target })
        }
    }
}

const MAC_ARM64: Machine = Machine { arch: Arch::Arm64, os: Os::MacOs, features: Machine::LSE };

/// Places an `Emitted` record into executable memory on this Apple Silicon host.
fn load_exec(emitted: Emitted<'static>) -> (Executable, usize) {
    let sealed = seal(emitted).expect("emitted record must seal cleanly");
    let mut bytes: AVec<'static, u8> = Vec::new_in(Alloc::global());
    encode(&sealed, &mut bytes);
    let view = read(&bytes).expect("encoded record must read back");
    let code_bytes = view.code().len();
    let relro_len = view.relro().len();
    let align = 1usize << view.relro_align();
    let sym = Symbol::of_name("blaze-emit sota test");
    let mut done: Result<(), PlaceError> = Ok(());
    let exec = Executable::written(code_bytes, relro_len + align, |code, data| {
        let skip = (align - data.as_ptr() as usize % align) % align;
        let relro = &mut data[skip..skip + relro_len];
        let at = At { code: code.as_ptr() as u64, relro: relro.as_ptr() as u64 };
        let mut words = LeafWords;
        done = place(&view, sym, at, &mut words, code, relro);
    })
    .expect("W^X exec mapping must succeed");
    done.expect("place must succeed");
    (exec, code_bytes)
}

#[test]
fn aarch64_routes_optimizing_tier_to_sota_by_default() {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let x = b.param(Ty::I64);
    b.ret(&[x]);
    let f = b.finish_checked().unwrap();

    assert_eq!(optimizing_tier(f.view(), MAC_ARM64), TierName::Sota);
    let x64 = Machine { arch: Arch::X86_64, os: Os::Linux, features: 0 };
    assert_eq!(optimizing_tier(f.view(), x64), TierName::Optimized);
}

#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
#[test]
fn sota_fused_madd_msub_shifted_alu_and_csel_match_evaluator_on_hardware() {
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(
        Conv::C,
        Variadic::No,
        &[Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64)],
        &[Param::of(Ty::I64)],
    );
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let a = b.param(Ty::I64);
    let bv = b.param(Ty::I64);
    let c = b.param(Ty::I64);
    // madd: a * b + c
    let prod = b.binary(Op::Mul, Flags::NONE, a, bv);
    let ma = b.binary(Op::Add, Flags::NONE, prod, c);
    // add_lsl: ma + (a << 3)
    let three = b.iconst(Ty::I64, 3);
    let sh = b.binary(Op::Shl, Flags::NONE, a, three);
    let sl = b.binary(Op::Add, Flags::NONE, ma, sh);
    // msub: sl - (b * c)
    let prod2 = b.binary(Op::Mul, Flags::NONE, bv, c);
    let ms = b.binary(Op::Sub, Flags::NONE, sl, prod2);
    // fused cmp + csel: if a < b { ms } else { ma }
    let lt = b.icmp(IntCond::Ult, a, bv);
    let chosen = b.select(Flags::NONE, lt, ms, ma);
    b.ret(&[chosen]);

    let func = b.finish_checked().unwrap();
    let v = verify(func.view()).unwrap();
    let emitted = Sota::standalone().emit(v, &NoNames, MAC_ARM64, Alloc::global()).unwrap();
    let (exec, code_len) = load_exec(emitted);

    // Frameless fused sequence should be tiny (<= 40 bytes = <= 10 AArch64 instructions)!
    assert!(code_len <= 44, "expected tiny frameless fused A64 code, got {code_len} bytes");

    let f_ptr: extern "C" fn(u64, u64, u64) -> u64 = unsafe { core::mem::transmute(exec.code_address()) };
    for &(av, bval, cv) in &[(3u64, 7u64, 11u64), (20, 5, 13), (0, 0, 99), (u64::MAX, 2, 7)] {
        let got = f_ptr(av, bval, cv);
        let expected = match eval::run_in(
            func.view(),
            &[Val::Int(av.into()), Val::Int(bval.into()), Val::Int(cv.into())],
            10_000,
            Global,
        )
        .unwrap()
        {
            Outcome::Returned(vals) => match vals[0] {
                Val::Int(n) => n as u64,
                _ => panic!("non-int"),
            },
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(got, expected, "Sota hardware output diverged on ({av}, {bval}, {cv})");
    }
}

#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
#[test]
fn sota_fibonacci_parallel_copy_cycle_matches_evaluator_on_hardware() {
    // Fibonacci loop: `(a, b) = (b, a + b)` — exercises cyclic parallel-copy resolution (`X16`)!
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
    b.set_signature(sig);
    let [entry, header, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));

    b.begin(entry);
    let n = b.param(Ty::I64);
    let zero = b.iconst(Ty::I64, 0);
    let one = b.iconst(Ty::I64, 1);
    b.jump((header, &[zero, zero, one]));

    b.begin(header);
    let i = b.param(Ty::I64);
    let fa = b.param(Ty::I64);
    let fb = b.param(Ty::I64);
    let cond = b.icmp(IntCond::Ult, i, n);
    b.br(Flags::NONE, cond, (body, &[]), (exit, &[]));

    b.begin(body);
    let next = b.binary(Op::Add, Flags::NONE, fa, fb);
    let inc = b.iconst(Ty::I64, 1);
    let next_i = b.binary(Op::Add, Flags::NONE, i, inc);
    b.jump((header, &[next_i, fb, next]));

    b.begin(exit);
    b.ret(&[fa]);

    let func = b.finish_checked().unwrap();
    let v = verify(func.view()).unwrap();
    let emitted = Sota::standalone().emit(v, &NoNames, MAC_ARM64, Alloc::global()).unwrap();
    let (exec, _len) = load_exec(emitted);
    let fib: extern "C" fn(u64) -> u64 = unsafe { core::mem::transmute(exec.code_address()) };

    for &nv in &[0u64, 1, 2, 5, 10, 20, 40] {
        let got = fib(nv);
        let expected = match eval::run_in(func.view(), &[Val::Int(nv.into())], 100_000, Global).unwrap() {
            Outcome::Returned(vals) => match vals[0] {
                Val::Int(x) => x as u64,
                _ => panic!("non-int"),
            },
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(got, expected, "fib({nv}) mismatch");
    }
}

#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
#[test]
fn sota_beats_reference_and_fast_on_hot_loop_hardware_benchmark() {
    // Realistic hot loop with loop-invariant math, power-of-2 udiv/urem/mul, fused madd,
    // and induction variable:
    //   acc += (a * 8) + (i / 8) + (i % 16) + (i * b)
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(
        Conv::C,
        Variadic::No,
        &[Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64)],
        &[Param::of(Ty::I64)],
    );
    b.set_signature(sig);
    let [entry, header, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));

    b.begin(entry);
    let n = b.param(Ty::I64);
    let a = b.param(Ty::I64);
    let bv = b.param(Ty::I64);
    let zero = b.iconst(Ty::I64, 0);
    b.jump((header, &[zero, zero]));

    b.begin(header);
    let i = b.param(Ty::I64);
    let acc = b.param(Ty::I64);
    let cont = b.icmp(IntCond::Ult, i, n);
    b.br(Flags::NONE, cont, (body, &[]), (exit, &[]));

    b.begin(body);
    let eight = b.iconst(Ty::I64, 8);
    let sixteen = b.iconst(Ty::I64, 16);
    let inv = b.binary(Op::Mul, Flags::NONE, a, eight); // LICM + strength-reduced to shl 3
    let q = b.binary(Op::Udiv, Flags::NONE, i, eight); // strength-reduced to lshr 3 -> add_lsr
    let r = b.binary(Op::Urem, Flags::NONE, i, sixteen); // strength-reduced to and 15 -> ubfx
    let t1 = b.binary(Op::Add, Flags::NONE, inv, q);
    let t2 = b.binary(Op::Add, Flags::NONE, t1, r);
    let ib = b.binary(Op::Mul, Flags::NONE, i, bv);
    let t3 = b.binary(Op::Add, Flags::NONE, ib, t2); // fused madd!
    let next_acc = b.binary(Op::Add, Flags::NONE, acc, t3);
    let one = b.iconst(Ty::I64, 1);
    let next_i = b.binary(Op::Add, Flags::NONE, i, one);
    b.jump((header, &[next_i, next_acc]));

    b.begin(exit);
    b.ret(&[acc]);

    let func = b.finish_checked().unwrap();
    let v = verify(func.view()).unwrap();
    let alloc = Alloc::global();

    let table = EncodedTable::read(TABLE, alloc).unwrap();
    let machine = table.head().machine;

    // Compile with all 3 tiers: Reference, Fast (stencils), and Sota (non-LLVM optimizing)!
    let t0 = Instant::now();
    let ref_em = Reference.emit(v, &NoNames, machine, alloc).unwrap();
    let ref_compile_us = t0.elapsed().as_micros();

    let fast_tier = Fast { table: &table, regs: &ARM64 };
    let t1 = Instant::now();
    let fast_em = fast_tier.emit(v, &NoNames, machine, alloc).unwrap();
    let fast_compile_us = t1.elapsed().as_micros();

    let t2 = Instant::now();
    let sota_em = Sota::standalone().emit(v, &NoNames, machine, alloc).unwrap();
    let sota_compile_us = t2.elapsed().as_micros();

    let (ref_exec, ref_bytes) = load_exec(ref_em);
    let (fast_exec, fast_bytes) = load_exec(fast_em);
    let (sota_exec, sota_bytes) = load_exec(sota_em);

    let f_ref: extern "C" fn(u64, u64, u64) -> u64 = unsafe { core::mem::transmute(ref_exec.code_address()) };
    let f_fast: extern "C" fn(u64, u64, u64) -> u64 = unsafe { core::mem::transmute(fast_exec.code_address()) };
    let f_sota: extern "C" fn(u64, u64, u64) -> u64 = unsafe { core::mem::transmute(sota_exec.code_address()) };

    // Correctness check across all 3 tiers + LIR evaluator!
    let eval_out = match eval::run_in(func.view(), &[Val::Int(50), Val::Int(7), Val::Int(3)], 200_000, Global).unwrap() {
        Outcome::Returned(vals) => match vals[0] {
            Val::Int(x) => x as u64,
            _ => panic!("non-int"),
        },
        other => panic!("unexpected {other:?}"),
    };
    assert_eq!(f_ref(50, 7, 3), eval_out);
    assert_eq!(f_fast(50, 7, 3), eval_out);
    assert_eq!(f_sota(50, 7, 3), eval_out);

    // Hardware timing on 1,000,000 iterations!
    const N: u64 = 1_000_000;
    let t_ref = Instant::now();
    let r_ref = std::hint::black_box(f_ref(N, 7, 3));
    let ref_ns = t_ref.elapsed().as_nanos();

    let t_fast = Instant::now();
    let r_fast = std::hint::black_box(f_fast(N, 7, 3));
    let fast_ns = t_fast.elapsed().as_nanos();

    let t_sota = Instant::now();
    let r_sota = std::hint::black_box(f_sota(N, 7, 3));
    let sota_ns = t_sota.elapsed().as_nanos();

    assert_eq!(r_ref, r_sota);
    assert_eq!(r_fast, r_sota);

    std::eprintln!(
        "SOTA vs Fast vs Reference (1M-iter hot loop on Apple Silicon, 1 core):\n\
         - Reference: {ref_bytes:4} B | compile {ref_compile_us:4} us | exec {:.2} ns/iter\n\
         - Fast:      {fast_bytes:4} B | compile {fast_compile_us:4} us | exec {:.2} ns/iter\n\
         - Sota:      {sota_bytes:4} B | compile {sota_compile_us:4} us | exec {:.2} ns/iter (speedup vs Ref: {:.1}x, vs Fast: {:.1}x)",
        ref_ns as f64 / N as f64,
        fast_ns as f64 / N as f64,
        sota_ns as f64 / N as f64,
        ref_ns as f64 / (sota_ns.max(1) as f64),
        fast_ns as f64 / (sota_ns.max(1) as f64),
    );

    // Sota code MUST be dramatically smaller and faster than Reference and Fast!
    assert!(sota_bytes < ref_bytes / 2, "expected Sota ({sota_bytes} B) < Reference/2 ({ref_bytes} B)");
    assert!(sota_bytes < fast_bytes, "expected Sota ({sota_bytes} B) < Fast ({fast_bytes} B)");
    assert!(sota_ns < ref_ns, "expected Sota ({sota_ns} ns) faster than Reference ({ref_ns} ns)");
    assert!(sota_ns < fast_ns, "expected Sota ({sota_ns} ns) faster than Fast ({fast_ns} ns)");
}

#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
#[test]
fn sota_matches_evaluator_across_150_random_lir_programs() {
    use blaze_lir::{Block, Value};
    use blaze_sim::{Broke, Config, Tape};

    use crate::tests::{FUEL, mask, random, run};

    let standalone_compared = core::cell::Cell::new(0u32);
    let fallback_compared = core::cell::Cell::new(0u32);

    blaze_sim::check(Config::cases(150), |tape: &Tape| {
        let (func, args) = random::program(tape).map_err(|why| Broke::Harness(format!("build: {why:?}")))?;
        let view = func.view();
        let v = verify(view).map_err(|why| Broke::Harness(format!("verify: {why:?}")))?;
        let want = match eval::run_in(view, &args, FUEL, Global) {
            Ok(Outcome::Returned(vals)) => match vals.as_slice() {
                [Val::Int(n)] => *n as u64,
                _ => return Ok(()),
            },
            _ => return Ok(()),
        };

        let garbage = tape.next_u64("garbage above arg widths");
        let mut regs = [0u64; 4];
        for (at, (inst, arg)) in view.params(Block(0)).zip(&args).enumerate() {
            let Val::Int(bits) = *arg else { return Ok(()) };
            let width = view.ty(Value(inst as u32)).bits(64);
            regs[at] = bits as u64 | (garbage & !mask(width));
        }

        // 1. Test Sota standalone (native fused A64 emitter) whenever in subset.
        if let Ok(emitted) = Sota::standalone().emit(v, &NoNames, MAC_ARM64, Alloc::global()) {
            let (exec, _) = load_exec(emitted);
            let got = run::call(&exec, regs);
            if got != want {
                return Err(Broke::Oracle(format!(
                    "Sota::standalone diverged: got {got:#x}, eval {want:#x} on {args:x?}\n{}",
                    blaze_lir::print(view)
                )));
            }
            standalone_compared.set(standalone_compared.get() + 1);
        }

        // 2. Test Sota::with_fallback(Reference) — must handle 100% of Reference's subset!
        let emitted = Sota::with_fallback(Reference)
            .emit(v, &NoNames, MAC_ARM64, Alloc::global())
            .map_err(|e| Broke::Oracle(format!("Sota::with_fallback refused: {e}")))?;
        let (exec, _) = load_exec(emitted);
        let got = run::call(&exec, regs);
        if got != want {
            return Err(Broke::Oracle(format!(
                "Sota::with_fallback diverged: got {got:#x}, eval {want:#x} on {args:x?}\n{}",
                blaze_lir::print(view)
            )));
        }
        fallback_compared.set(fallback_compared.get() + 1);
        Ok(())
    });

    let sc = standalone_compared.get();
    let fc = fallback_compared.get();
    std::eprintln!(
        "Sota fuzz tier-equality: {fc} full-coverage runs, {sc} direct fused-A64 runs — 0 divergences!"
    );
    assert!(fc >= 20, "expected >= 20 compared programs, got {fc}");
    assert!(sc >= 5, "expected >= 5 direct fused-A64 programs, got {sc}");
}
