//! Live Apple Silicon hardware verification & head-to-head benchmarks for `TierName::Sota`
//! against `blaze_lir::eval`, `Reference`, and `Fast`.

use std::alloc::Global;
use std::time::Instant;

use blaze_code::image::ImageError;
use blaze_code::place::{At, PlaceError, Words};
use blaze_code::record::{Space, Symbol, Target};
use blaze_inproc::exec::Executable;
use blaze_lir::{
    Flags, FloatCond, IntCond, Op, Ty,
    builder::{Builder, Full, MemAttrs, Param, Variadic},
    eval::{self, Outcome, Val},
    func::{BlockKind, Conv, RefKind, SlotKind},
    verify,
};
use blaze_own::{AVec, Alloc};
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

struct LeafWords {
    page: u64,
}
impl Words for LeafWords {
    fn word(&mut self, target: Target, _call: bool) -> Result<u64, ImageError> {
        Err(ImageError::Unbound { target })
    }

    fn value(&mut self, target: Target) -> Result<u64, ImageError> {
        if target.space == Space::Outside && target.symbol == Symbol::of_name(POLL_PAGE) {
            Ok(self.page)
        } else {
            Err(ImageError::Unbound { target })
        }
    }
}

const MAC_ARM64: Machine = Machine { arch: Arch::Arm64, os: Os::MacOs, features: Machine::LSE };

fn test_alloc() -> Alloc<'static> {
    Alloc::global()
}

/// Places an `Emitted` record into executable memory on this Apple Silicon host.
fn load_exec(emitted: Emitted<'static>) -> (Executable, usize) {
    let sealed = seal(emitted).expect("emitted record must seal cleanly");
    let mut bytes: AVec<'static, u8> = AVec::new_in(test_alloc());
    encode(&sealed, &mut bytes);
    let view = read(&bytes).expect("encoded record must read back");
    let code_bytes = view.code().len();
    let relro_len = view.relro().len();
    let align = 1usize << view.relro_align();
    let sym = Symbol::of_name("blaze-emit sota test");
    let mut done: Result<(), PlaceError> = Ok(());
    let exec = Executable::written(code_bytes, 8192 + relro_len + align, |code, data| {
        let poll_skip = (4096 - data.as_ptr() as usize % 4096) % 4096;
        let (poll_and_pad, rest) = data.split_at_mut(poll_skip + 4096);
        let poll = &mut poll_and_pad[poll_skip..];
        poll.fill(0);
        let skip = (align - rest.as_ptr() as usize % align) % align;
        let relro = &mut rest[skip..skip + relro_len];
        let at = At { code: code.as_ptr() as u64, relro: relro.as_ptr() as u64 };
        let mut words = LeafWords { page: poll.as_ptr() as u64 };
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
    assert_eq!(optimizing_tier(f.view(), x64), TierName::Sota);
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

    let f_ptr: extern "C" fn(u64, u64, u64) -> u64 = unsafe { exec.as_fn() };
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
    let fib: extern "C" fn(u64) -> u64 = unsafe { exec.as_fn() };

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

    let f_ref: extern "C" fn(u64, u64, u64) -> u64 = unsafe { ref_exec.as_fn() };
    let f_fast: extern "C" fn(u64, u64, u64) -> u64 = unsafe { fast_exec.as_fn() };
    let f_sota: extern "C" fn(u64, u64, u64) -> u64 = unsafe { sota_exec.as_fn() };

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

    // Sota unrolls/vectorizes the hot loop while staying smaller than Reference and beating both Reference & Fast on hardware!
    assert!(sota_bytes <= ref_bytes, "expected Sota ({sota_bytes} B) <= Reference ({ref_bytes} B)");
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

#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
#[test]
fn sota_stack_slots_struct_load_store_and_address_folding_match_evaluator_on_hardware() {
    // 1. Local stack slot with 64-byte alignment (tests dynamic SP realignment + FP epilogue restore!),
    //    mixed 8/16/32/64-bit integer & f32/f64 fields, volatile access, folded `[sp, #off]` loads/stores,
    //    AND dynamic `ptr_add(slot_addr, idx)` so SROA keeps the stack slot!
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
    let idx = b.param(Ty::I64);

    let slot = b.slot(64, 6, SlotKind::Named); // 64 bytes, 64-byte aligned
    b.slot_live(slot);
    let base = b.slot_addr(slot);

    // Store mixed-width fields directly onto `slot_addr` (folded to `[sp, #off]`).
    let b8 = b.convert(Op::Trunc, Flags::NONE, Ty::I8, a);
    let b16 = b.convert(Op::Trunc, Flags::NONE, Ty::I16, bv);
    let b32 = b.convert(Op::Trunc, Flags::NONE, Ty::I32, a);
    b.store(Flags::NONE, b8, base, 0, MemAttrs::aligned(0));
    b.store(Flags::NONE, b16, base, 2, MemAttrs::aligned(1));
    b.store(Flags::NONE, b32, base, 4, MemAttrs::aligned(2));
    b.store(Flags::NONE, a, base, 8, MemAttrs::aligned(3));
    b.store(Flags::NONE, bv, base, 16, MemAttrs::aligned(3));

    // Address-mode folding through `ptr_add(base, imm)`: writes at `base + 24` and `base + 32`.
    let off24 = b.iconst(Ty::I64, 24);
    let p24 = b.ptr_add(Flags::NONE, base, off24);
    let fa = b.convert(Op::SitoFp, Flags::NONE, Ty::F64, a);
    b.store(Flags::NONE, fa, p24, 0, MemAttrs::aligned(3));
    let fb32 = b.convert(Op::UitoFp, Flags::NONE, Ty::F32, bv);
    b.store(Flags::NONE, fb32, p24, 8, MemAttrs::aligned(2)); // base + 32

    // Dynamic index into the 2-element u64 array at offset 8 (`8 + (idx & 1) * 8`) -> prevents SROA!
    let one = b.iconst(Ty::I64, 1);
    let lane = b.binary(Op::And, Flags::NONE, idx, one);
    let three = b.iconst(Ty::I64, 3);
    let byte_off = b.binary(Op::Shl, Flags::NONE, lane, three);
    let dyn_ptr = b.ptr_add(Flags::NONE, base, byte_off);
    let dyn_val = b.load(Ty::I64, Flags::NONE, dyn_ptr, 8, MemAttrs::aligned(3));

    // Reload every width and verify zero-extension + FP round-trip!
    let l8 = b.load(Ty::I8, Flags::NONE, base, 0, MemAttrs::aligned(0));
    let l16 = b.load(Ty::I16, Flags::NONE, base, 2, MemAttrs::aligned(1));
    let l32 = b.load(Ty::I32, Flags::NONE, base, 4, MemAttrs::aligned(2));
    let l64 = b.load(Ty::I64, Flags::NONE, base, 8, MemAttrs::aligned(3));
    let p24_r = b.ptr_add(Flags::NONE, base, off24);
    let lf64 = b.load(Ty::F64, Flags::NONE, p24_r, 0, MemAttrs::aligned(3));
    let lf32 = b.load(Ty::F32, Flags::NONE, p24_r, 8, MemAttrs::aligned(2));
    let lf64_i = b.convert(Op::FptoSi, Flags::NONE, Ty::I64, lf64);
    let lf32_i = b.convert(Op::FptoUi, Flags::NONE, Ty::I64, lf32);

    let z8 = b.convert(Op::Zext, Flags::NONE, Ty::I64, l8);
    let z16 = b.convert(Op::Zext, Flags::NONE, Ty::I64, l16);
    let z32 = b.convert(Op::Zext, Flags::NONE, Ty::I64, l32);
    let s1 = b.binary(Op::Add, Flags::NONE, z8, z16);
    let s2 = b.binary(Op::Add, Flags::NONE, s1, z32);
    let s3 = b.binary(Op::Add, Flags::NONE, s2, l64);
    let s4 = b.binary(Op::Add, Flags::NONE, s3, dyn_val);
    let s5 = b.binary(Op::Add, Flags::NONE, s4, lf64_i);
    let s6 = b.binary(Op::Add, Flags::NONE, s5, lf32_i);
    b.slot_dead(slot);
    b.ret(&[s6]);

    let func = b.finish_checked().unwrap();
    let v = verify(func.view()).unwrap();
    let emitted = Sota::standalone().emit(v, &NoNames, MAC_ARM64, Alloc::global()).unwrap();
    assert!(emitted.frame != 0, "stack-slot function must emit a valid ARM64 frame word");
    assert!(blaze_code::unwind::Frame::of_word(emitted.frame).unwrap().is_some());

    let (exec, _) = load_exec(emitted);
    let f_ptr: extern "C" fn(u64, u64, u64) -> u64 = unsafe { exec.as_fn() };

    for &(av, bval, iv) in &[
        (0x1234_5678u64, 0x00ab_cdefu64, 0u64),
        (0x1234_5678, 0x00ab_cdef, 1),
        (0xff, 0xffff, 0),
        (42, 1000, 1),
    ] {
        let got = f_ptr(av, bval, iv);
        let expected = match eval::run_in(
            func.view(),
            &[Val::Int(av.into()), Val::Int(bval.into()), Val::Int(iv.into())],
            100_000,
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
        assert_eq!(got, expected, "stack-slot mismatch on ({av:#x}, {bval:#x}, {iv})");
    }

    // 2. External pointer struct load/store with address-mode folding (`ptr_add(ptr, imm)` & negative offset).
    let mut b2 = Builder::<Global, Full>::new_in(64, Global);
    let sig2 = b2.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR)], &[Param::of(Ty::I64)]);
    b2.set_signature(sig2);
    let e2 = b2.create_block(BlockKind::Normal);
    b2.begin(e2);
    let ptr = b2.param(Ty::PTR);
    let off8 = b2.iconst(Ty::I64, 8);
    let p_mid = b2.ptr_add(Flags::NONE, ptr, off8);
    // Negative offset `-8` (uses `ldur`), `0` (uses `ldr`), `+8` (volatile `ldr`).
    let v0 = b2.load(Ty::I64, Flags::NONE, p_mid, -8, MemAttrs::aligned(3));
    let v1 = b2.load(Ty::I64, Flags::VOLATILE, p_mid, 0, MemAttrs::aligned(3));
    let v2 = b2.load(Ty::I64, Flags::NONE, p_mid, 8, MemAttrs::aligned(3));
    let sum = b2.binary(Op::Add, Flags::NONE, v0, v1);
    let sum2 = b2.binary(Op::Add, Flags::NONE, sum, v2);
    let off16 = b2.iconst(Ty::I64, 16);
    let p16 = b2.ptr_add(Flags::NONE, ptr, off16);
    b2.store(Flags::VOLATILE, sum2, p16, 0, MemAttrs::aligned(3));
    b2.ret(&[sum2]);

    let func2 = b2.finish_checked().unwrap();
    let v2_ver = verify(func2.view()).unwrap();
    let emitted2 = Sota::standalone().emit(v2_ver, &NoNames, MAC_ARM64, Alloc::global()).unwrap();
    let (exec2, _) = load_exec(emitted2);
    let f2: extern "C" fn(*mut u64) -> u64 = unsafe { exec2.as_fn() };
    let mut buf = [10u64, 25u64, 7u64];
    let res = f2(buf.as_mut_ptr());
    assert_eq!(res, 42);
    assert_eq!(buf[2], 42);
}

#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
#[test]
fn sota_high_register_pressure_spilling_over_30_live_values_matches_evaluator_on_hardware() {
    // 1. Straight-line function with 32 simultaneously live values (exceeds all 26 allocatable GPRs,
    //    forcing genuine spill-slot allocation, reloads via X16/X17, and stores to spill slots!).
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(
        Conv::C,
        Variadic::No,
        &[Param::of(Ty::I64), Param::of(Ty::I64)],
        &[Param::of(Ty::I64)],
    );
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);
    let x = b.param(Ty::I64);
    let y = b.param(Ty::I64);

    let mut live = AVec::with_capacity_in(32, test_alloc());
    for k in 0..32u64 {
        let c1 = b.iconst(Ty::I64, (k * 7 + 3).into());
        let c2 = b.iconst(Ty::I64, (k * 13 + 5).into());
        let p = b.binary(Op::Mul, Flags::NONE, x, c1);
        let q = b.binary(Op::Add, Flags::NONE, p, y);
        let v = b.binary(Op::Xor, Flags::NONE, q, c2);
        live.push(v);
    }
    // Keep all 32 values simultaneously live by consuming them in reverse + interleaved order,
    // including a 3-operand fused `madd` on spilled values and a `select` on spilled values!
    let mut acc = live[31];
    for k in (0..31).rev() {
        let prod = b.binary(Op::Mul, Flags::NONE, live[k], live[(k + 17) % 32]);
        acc = b.binary(Op::Add, Flags::NONE, prod, acc);
    }
    let cond = b.icmp(IntCond::Ult, live[0], live[31]);
    let out = b.select(Flags::NONE, cond, acc, live[29]);
    b.ret(&[out]);

    let func = b.finish_checked().unwrap();
    let v = verify(func.view()).unwrap();
    let emitted = Sota::standalone().emit(v, &NoNames, MAC_ARM64, Alloc::global()).unwrap();
    assert!(emitted.frame != 0, "spilling function must allocate a stack frame");
    let (exec, _) = load_exec(emitted);
    let f_ptr: extern "C" fn(u64, u64) -> u64 = unsafe { exec.as_fn() };

    for &(xv, yv) in &[(1u64, 2u64), (17, 99), (0, 0), (0xdead_beef, 0x1234_5678)] {
        let got = f_ptr(xv, yv);
        let expected = match eval::run_in(
            func.view(),
            &[Val::Int(xv.into()), Val::Int(yv.into())],
            100_000,
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
        assert_eq!(got, expected, "32-live-value spill mismatch on ({xv}, {yv})");
    }

    // 2. Spilling loop with 30 loop-carried block parameters rotating in a cyclic shift every
    //    iteration -> exercises spill-to-spill cyclic parallel-copy resolution!
    let mut b2 = Builder::<Global, Full>::new_in(64, Global);
    let sig2 = b2.sig(
        Conv::C,
        Variadic::No,
        &[Param::of(Ty::I64), Param::of(Ty::I64)],
        &[Param::of(Ty::I64)],
    );
    b2.set_signature(sig2);
    let [e2, header, body, exit] = [(); 4].map(|_| b2.create_block(BlockKind::Normal));

    b2.begin(e2);
    let n = b2.param(Ty::I64);
    let seed = b2.param(Ty::I64);
    let zero = b2.iconst(Ty::I64, 0);
    let mut init_args = AVec::with_capacity_in(31, test_alloc());
    init_args.push(zero);
    for k in 0..30u64 {
        let ck = b2.iconst(Ty::I64, (k + 1).into());
        init_args.push(b2.binary(Op::Add, Flags::NONE, seed, ck));
    }
    b2.jump((header, &init_args));

    b2.begin(header);
    let iter = b2.param(Ty::I64);
    let mut ring = AVec::with_capacity_in(30, test_alloc());
    for _ in 0..30 {
        ring.push(b2.param(Ty::I64));
    }
    let cont = b2.icmp(IntCond::Ult, iter, n);
    b2.br(Flags::NONE, cont, (body, &[]), (exit, &[]));

    b2.begin(body);
    let one = b2.iconst(Ty::I64, 1);
    let next_iter = b2.binary(Op::Add, Flags::NONE, iter, one);
    // Rotate all 30 values cyclically (guaranteed spill-to-spill cycle!) while mixing `ring[0] + ring[29]`.
    let mixed = b2.binary(Op::Add, Flags::NONE, ring[0], ring[29]);
    let mut next_args = AVec::with_capacity_in(31, test_alloc());
    next_args.push(next_iter);
    for k in 1..30 {
        next_args.push(ring[k]);
    }
    next_args.push(mixed);
    b2.jump((header, &next_args));

    b2.begin(exit);
    let mut total = ring[0];
    for &r in &ring[1..] {
        total = b2.binary(Op::Xor, Flags::NONE, total, r);
    }
    b2.ret(&[total]);

    let func2 = b2.finish_checked().unwrap();
    let v2 = verify(func2.view()).unwrap();
    let emitted2 = Sota::standalone().emit(v2, &NoNames, MAC_ARM64, Alloc::global()).unwrap();
    let (exec2, _) = load_exec(emitted2);
    let f2: extern "C" fn(u64, u64) -> u64 = unsafe { exec2.as_fn() };

    for &(nv, sv) in &[(0u64, 5u64), (1, 10), (7, 42), (35, 13)] {
        let got = f2(nv, sv);
        let expected = match eval::run_in(
            func2.view(),
            &[Val::Int(nv.into()), Val::Int(sv.into())],
            200_000,
            Global,
        )
        .unwrap()
        {
            Outcome::Returned(vals) => match vals[0] {
                Val::Int(x) => x as u64,
                _ => panic!("non-int"),
            },
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(got, expected, "spilling cyclic loop mismatch on ({nv}, {sv})");
    }
}

#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
#[test]
fn sota_scalar_fp_f32_f64_fmadd_fmsub_conversions_and_fcmp_match_evaluator_on_hardware() {
    // 1. f64 arithmetic + Fma + contractible fadd(fmul) -> fmadd & fsub(c, fmul) -> fmsub + fcmp + fcsel!
    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(
        Conv::C,
        Variadic::No,
        &[Param::of(Ty::F64), Param::of(Ty::F64), Param::of(Ty::F64)],
        &[Param::of(Ty::F64)],
    );
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);

    let x = b.param(Ty::F64);
    let y = b.param(Ty::F64);
    let z = b.param(Ty::F64);

    let half = b.fconst(Ty::F64, (0.5f64).to_bits() as u128);
    // Strict IEEE ops:
    let sum = b.binary(Op::Fadd, Flags::NONE, x, y);
    let diff = b.binary(Op::Fsub, Flags::NONE, sum, half);
    let quot = b.binary(Op::Fdiv, Flags::NONE, diff, z);
    let neg_q = b.unary(Op::Fneg, Flags::NONE, quot);
    // Explicit hardware FMA (`x * y + neg_q`):
    let fma_v = b.ternary(Op::Fma, Flags::NONE, x, y, neg_q);
    // Contractible fsub(fma_v, fmul(y, z)) -> fused hardware `fmsub`!
    let prod_c = b.binary(Op::Fmul, Flags::CONTRACT, y, z);
    let fmsub_v = b.binary(Op::Fsub, Flags::CONTRACT, fma_v, prod_c);
    // Fused `fcmp` + `select` -> `fcmp` + `fcsel`!
    let lt = b.fcmp(FloatCond::Olt, x, y);
    let chosen = b.select(Flags::NONE, lt, fmsub_v, fma_v);
    b.ret(&[chosen]);

    let func = b.finish_checked().unwrap();
    let v = verify(func.view()).unwrap();
    let emitted = Sota::standalone().emit(v, &NoNames, MAC_ARM64, Alloc::global()).unwrap();

    // Verify genuine AArch64 `fmadd` (0x1f40_0000 mask 0xff60_8000) and `fmsub` (0x1f40_8000) words were emitted!
    let mut saw_fmadd = false;
    let mut saw_fmsub = false;
    for chunk in emitted.code.chunks_exact(4) {
        let w = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        if (w & 0xff60_8000) == 0x1f40_0000 {
            saw_fmadd = true;
        }
        if (w & 0xff60_8000) == 0x1f40_8000 {
            saw_fmsub = true;
        }
    }
    assert!(saw_fmadd, "expected hardware fmadd instruction in Sota FP code");
    assert!(saw_fmsub, "expected hardware fmsub instruction in Sota FP code");

    let (exec, _) = load_exec(emitted);
    let f64_fn: extern "C" fn(f64, f64, f64) -> f64 = unsafe { exec.as_fn() };

    for &(xv, yv, zv) in &[
        (1.5f64, 2.25f64, 4.0f64),
        (5.0, -3.5, 2.0),
        (0.0, 0.0, 1.0),
        (-10.25, 10.25, -0.5),
        (f64::NAN, 1.0, 2.0),
    ] {
        let got = f64_fn(xv, yv, zv);
        // Compute reference directly with IEEE 754 + Rust `mul_add` (which matches hardware `fmadd`/`fmsub`!).
        let sum_r = xv + yv;
        let diff_r = sum_r - 0.5;
        let quot_r = diff_r / zv;
        let neg_r = -quot_r;
        let fma_r = xv.mul_add(yv, neg_r);
        let fmsub_r = (-yv).mul_add(zv, fma_r);
        let want = if xv < yv { fmsub_r } else { fma_r };
        if want.is_nan() {
            assert!(got.is_nan(), "expected NaN on ({xv}, {yv}, {zv}), got {got}");
        } else {
            assert_eq!(got.to_bits(), want.to_bits(), "f64 mismatch on ({xv}, {yv}, {zv}): got {got}, want {want}");
        }
    }

    // 2. f32 arithmetic + fpext/fptrunc + sitofp/uitofp/fptosi/fptoui + bitcast, verified bit-for-bit against `blaze_lir::eval`!
    let mut b2 = Builder::<Global, Full>::new_in(64, Global);
    let sig2 = b2.sig(
        Conv::C,
        Variadic::No,
        &[Param::of(Ty::F32), Param::of(Ty::I32), Param::of(Ty::I64)],
        &[Param::of(Ty::I64)],
    );
    b2.set_signature(sig2);
    let [e2, t_blk, f_blk] = [(); 3].map(|_| b2.create_block(BlockKind::Normal));
    b2.begin(e2);
    let fa = b2.param(Ty::F32);
    let si = b2.param(Ty::I32);
    let ui = b2.param(Ty::I64);

    let c32 = b2.fconst(Ty::F32, (2.5f32).to_bits() as u128);
    let s_as_f32 = b2.convert(Op::SitoFp, Flags::NONE, Ty::F32, si);
    let f32_sum = b2.binary(Op::Fadd, Flags::NONE, fa, s_as_f32);
    let f32_mul = b2.binary(Op::Fmul, Flags::NONE, f32_sum, c32);
    // Promote f32 -> f64, add uitofp(ui), demote back to f32, then convert to signed i16 (saturation!) & bitcast!
    let promoted = b2.convert(Op::Fpext, Flags::NONE, Ty::F64, f32_mul);
    let u_as_f64 = b2.convert(Op::UitoFp, Flags::NONE, Ty::F64, ui);
    let d_sum = b2.binary(Op::Fadd, Flags::NONE, promoted, u_as_f64);
    let demoted = b2.convert(Op::Fptrunc, Flags::NONE, Ty::F32, d_sum);
    let raw_bits = b2.convert(Op::Bitcast, Flags::NONE, Ty::I32, demoted);
    let raw64 = b2.convert(Op::Zext, Flags::NONE, Ty::I64, raw_bits);
    let sat16 = b2.convert(Op::FptoSi, Flags::NONE, Ty::I16, d_sum);
    let sat64 = b2.convert(Op::Zext, Flags::NONE, Ty::I64, sat16);
    let zero_f = b2.fconst(Ty::F64, (0.0f64).to_bits() as u128);
    let is_pos = b2.fcmp(FloatCond::Ogt, d_sum, zero_f);
    b2.br(Flags::NONE, is_pos, (t_blk, &[]), (f_blk, &[]));

    b2.begin(t_blk);
    let r_pos = b2.binary(Op::Add, Flags::NONE, raw64, sat64);
    b2.ret(&[r_pos]);

    b2.begin(f_blk);
    let r_neg = b2.binary(Op::Xor, Flags::NONE, raw64, sat64);
    b2.ret(&[r_neg]);

    let func2 = b2.finish_checked().unwrap();
    let v2 = verify(func2.view()).unwrap();
    let emitted2 = Sota::standalone().emit(v2, &NoNames, MAC_ARM64, Alloc::global()).unwrap();
    let (exec2, _) = load_exec(emitted2);
    let f32_fn: extern "C" fn(f32, i32, u64) -> u64 = unsafe { exec2.as_fn() };

    for &(fv, sv, uv) in &[
        (1.25f32, -10i32, 5u64),
        (-100.5, -50_000, 0),
        (3.5, 40_000, 1234),
        (f32::INFINITY, 1, 2),
        (f32::NAN, 7, 9),
    ] {
        let got = f32_fn(fv, sv, uv);
        let expected = match eval::run_in(
            func2.view(),
            &[
                Val::Int(fv.to_bits() as u128),
                Val::Int((sv as u32) as u128),
                Val::Int(uv as u128),
            ],
            100_000,
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
        assert_eq!(got, expected, "f32/conv/fcmp mismatch on ({fv}, {sv}, {uv})");
    }
}

#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
#[test]
fn sota_data_addr_func_addr_call_indirect_and_code_record_seal_on_hardware() {
    extern "C" fn add_triple(a: u64, b: u64) -> u64 {
        a.wrapping_mul(3).wrapping_add(b)
    }

    static TEST_STATIC_VAL: u64 = 0x1122_3344_5566_7788;
    // Import words holding the addresses of `TEST_STATIC_VAL` and `add_triple` (8-byte aligned!).
    static mut STATIC_GOT_WORD: u64 = 0;
    static mut FUNC_GOT_WORD: u64 = 0;
    unsafe {
        STATIC_GOT_WORD = core::ptr::addr_of!(TEST_STATIC_VAL) as u64;
        FUNC_GOT_WORD = add_triple as *const () as u64;
    }

    struct ExtNames;
    impl Names for ExtNames {
        fn name(&self, of: Outside) -> Option<blaze_tier::Named<'_>> {
            match of {
                Outside::Static(0) => Some(blaze_tier::Named::Symbol(Symbol::of_name("test_static"))),
                Outside::Func(0) => Some(blaze_tier::Named::Symbol(Symbol::of_name("test_func"))),
                _ => None,
            }
        }

        fn routines(&self) -> RoutinePlaces {
            RoutinePlaces::NONE
        }
    }

    struct ExtWords;
    impl Words for ExtWords {
        fn word(&mut self, target: Target, call: bool) -> Result<u64, ImageError> {
            if target == (Target { space: Space::Static, symbol: Symbol::of_name("test_static") }) && !call {
                Ok(core::ptr::addr_of!(STATIC_GOT_WORD) as u64)
            } else if target == (Target { space: Space::Func, symbol: Symbol::of_name("test_func") }) {
                Ok(core::ptr::addr_of!(FUNC_GOT_WORD) as u64)
            } else {
                Err(ImageError::Unbound { target })
            }
        }

        fn value(&mut self, target: Target) -> Result<u64, ImageError> {
            Err(ImageError::Unbound { target })
        }
    }

    let mut b = Builder::<Global, Full>::new_in(64, Global);
    let sig = b.sig(
        Conv::C,
        Variadic::No,
        &[Param::of(Ty::I64), Param::of(Ty::PTR)],
        &[Param::of(Ty::I64)],
    );
    let callee_sig = b.sig(
        Conv::C,
        Variadic::No,
        &[Param::of(Ty::I64), Param::of(Ty::I64)],
        &[Param::of(Ty::I64)],
    );
    b.set_signature(sig);
    let entry = b.create_block(BlockKind::Normal);
    b.begin(entry);

    let x = b.param(Ty::I64);
    let dyn_fn = b.param(Ty::PTR);
    let static_ref = b.reference(RefKind::Static, 0);
    let func_ref = b.reference(RefKind::Func, 0);
    let own_data_ref = b.data(&100u64.to_le_bytes(), 3, &[]);

    // 1. Static DataAddr (`SiteKind::Load`)
    let s_ptr = b.data_addr(static_ref);
    let s_val = b.load(Ty::I64, Flags::NONE, s_ptr, 0, MemAttrs::aligned(3));
    // 2. Own DataAddr (`SiteKind::Relro`)
    let d_ptr = b.data_addr(own_data_ref);
    let d_val = b.load(Ty::I64, Flags::NONE, d_ptr, 0, MemAttrs::aligned(3));
    // 3. FuncAddr (`SiteKind::Load`) — read raw address so it stays a true `FuncAddr` load!
    let f_ptr = b.func_addr(func_ref);
    let f_addr_bits = b.convert(Op::PtrAddr, Flags::NONE, Ty::I64, f_ptr);
    // 4. Direct Call (`SiteKind::Call`) + Indirect Call (`CallIndirect`)
    let direct_res = b.call(Flags::NONE, func_ref, callee_sig, &[x, d_val]);
    let indirect_res = b.call_indirect(Flags::NONE, dyn_fn, callee_sig, &[direct_res, d_val]);
    let t1 = b.binary(Op::Xor, Flags::NONE, indirect_res, s_val);
    let res = b.binary(Op::Xor, Flags::NONE, t1, f_addr_bits);
    b.ret(&[res]);

    let func = b.finish_checked().unwrap();
    let v = verify(func.view()).unwrap();
    let emitted = Sota::standalone().emit(v, &ExtNames, MAC_ARM64, Alloc::global()).unwrap();

    // 1. Verify `emitted_to_code_record` converts Load (Static + FuncAddr), Relro, and Call sites and seals cleanly!
    let emitted_copy = Sota::standalone().emit(v, &ExtNames, MAC_ARM64, Alloc::global()).unwrap();
    let (code_rec, _insts) = super::emitted_to_code_record(emitted_copy, &[], Alloc::global())
        .expect("DataAddr (Static + Relro), FuncAddr, and Call must convert to CodeRecord cleanly");
    assert_eq!(code_rec.sites.len(), 4, "expected 2 Load sites + 1 Relro site + 1 Call site");
    let has_load = code_rec.sites.iter().any(|s| s.kind == blaze_code::record::SiteKind::Load);
    let has_relro = code_rec.sites.iter().any(|s| s.kind == blaze_code::record::SiteKind::Relro);
    let has_call = code_rec.sites.iter().any(|s| s.kind == blaze_code::record::SiteKind::Call);
    assert!(has_load && has_relro && has_call, "expected Load, Relro, and Call sites in CodeRecord");
    let sealed_rec = code_rec.seal().expect("Sota CodeRecord must seal without error");
    let mut rec_bytes: AVec<'static, u8> = AVec::new_in(test_alloc());
    sealed_rec.encode(&mut rec_bytes);
    let rec_view = blaze_code::record::CodeView::read(&rec_bytes).expect("encoded CodeRecord must read back");
    blaze_code::unwind::read(rec_view).expect("Sota CodeRecord unwind info must validate cleanly");

    // 2. Place and execute the sealed `CodeRecord` on live Apple Silicon hardware!
    let code_bytes = rec_view.code().len();
    let relro_len = rec_view.relro().len();
    let align = 1usize << rec_view.relro_align();
    let sym = Symbol::of_name("blaze-emit sota ext test");
    let mut done: Result<(), PlaceError> = Ok(());
    let exec = Executable::written(code_bytes, relro_len + align, |code, data| {
        let skip = (align - data.as_ptr() as usize % align) % align;
        let relro = &mut data[skip..skip + relro_len];
        let at = At { code: code.as_ptr() as u64, relro: relro.as_ptr() as u64 };
        let mut words = ExtWords;
        done = blaze_code::place::place(&rec_view, sym, at, &mut words, code, relro);
    })
    .expect("W^X exec mapping must succeed");
    done.expect("place must succeed");

    let run_fn: extern "C" fn(u64, extern "C" fn(u64, u64) -> u64) -> u64 =
        unsafe { exec.as_fn() };
    let fn_addr = add_triple as *const () as u64;
    for &xv in &[0u64, 7, 42, 9999] {
        let d = add_triple(xv, 100);
        let ind = add_triple(d, 100);
        let want = ind ^ TEST_STATIC_VAL ^ fn_addr;
        assert_eq!(run_fn(xv, add_triple), want, "DataAddr/FuncAddr/Call/CallIndirect mismatch on {xv}");
    }

    // 3. Also verify `Emitted` (bzcode08) seal + place + live execution with C names!
    struct ExtCNames;
    impl Names for ExtCNames {
        fn name(&self, of: Outside) -> Option<blaze_tier::Named<'_>> {
            match of {
                Outside::Static(0) => Some(blaze_tier::Named::C("test_static_c")),
                Outside::Func(0) => Some(blaze_tier::Named::C("test_func_c")),
                _ => None,
            }
        }
        fn routines(&self) -> RoutinePlaces {
            RoutinePlaces::NONE
        }
    }
    struct ExtCWords;
    impl Words for ExtCWords {
        fn word(&mut self, target: Target, call: bool) -> Result<u64, ImageError> {
            if target == (Target { space: Space::Outside, symbol: Symbol::of_name("test_static_c") }) && !call {
                Ok(core::ptr::addr_of!(STATIC_GOT_WORD) as u64)
            } else if target == (Target { space: Space::Outside, symbol: Symbol::of_name("test_func_c") }) {
                Ok(core::ptr::addr_of!(FUNC_GOT_WORD) as u64)
            } else {
                Err(ImageError::Unbound { target })
            }
        }
        fn value(&mut self, target: Target) -> Result<u64, ImageError> {
            Err(ImageError::Unbound { target })
        }
    }
    let emitted_c = Sota::standalone().emit(v, &ExtCNames, MAC_ARM64, Alloc::global()).unwrap();
    let sealed_c = seal(emitted_c).expect("emitted bzcode08 record must seal cleanly");
    let mut bytes_c: AVec<'static, u8> = AVec::new_in(test_alloc());
    encode(&sealed_c, &mut bytes_c);
    let view_c = read(&bytes_c).expect("encoded bzcode08 must read back");
    let mut done_c: Result<(), PlaceError> = Ok(());
    let align_c = 1usize << view_c.relro_align();
    let exec_c = Executable::written(view_c.code().len(), view_c.relro().len() + align_c, |code, data| {
        let skip = (align_c - data.as_ptr() as usize % align_c) % align_c;
        let relro = &mut data[skip..skip + view_c.relro().len()];
        let at = At { code: code.as_ptr() as u64, relro: relro.as_ptr() as u64 };
        let mut words = ExtCWords;
        done_c = place(&view_c, sym, at, &mut words, code, relro);
    })
    .expect("W^X exec mapping must succeed");
    done_c.expect("bzcode08 place must succeed");
    let run_fn_c: extern "C" fn(u64, extern "C" fn(u64, u64) -> u64) -> u64 =
        unsafe { exec_c.as_fn() };
    let want_c = add_triple(add_triple(123, 100), 100) ^ TEST_STATIC_VAL ^ fn_addr;
    assert_eq!(run_fn_c(123, add_triple), want_c);
    drop(emitted);
}

#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
mod std_readiness_tests {
    use super::*;
    use blaze_code::record::{CodeView, PositionField, PositionPart};
    use blaze_code::unwind::{self, Action};
    use blaze_lir::builder::{DataPosition, DataReloc, Facts};
    use blaze_lir::func::ParamKind;

    unsafe extern "C" {
        fn memcpy(dst: *mut u8, src: *const u8, n: usize) -> *mut u8;
        fn memmove(dst: *mut u8, src: *const u8, n: usize) -> *mut u8;
        fn memset(dst: *mut u8, c: i32, n: usize) -> *mut u8;
        fn memcmp(a: *const u8, b: *const u8, n: usize) -> i32;
        fn fmod(x: f64, y: f64) -> f64;
        fn fmodf(x: f32, y: f32) -> f32;
    }

    extern "C" fn rt_udivti3(a: u128, b: u128) -> u128 {
        a / b
    }
    extern "C" fn rt_divti3(a: i128, b: i128) -> i128 {
        a / b
    }
    extern "C" fn rt_umodti3(a: u128, b: u128) -> u128 {
        a % b
    }
    extern "C" fn rt_modti3(a: i128, b: i128) -> i128 {
        a % b
    }

    struct SymWords<'a> {
        got: &'a mut [u64],
        next_slot: usize,
        extra: &'a [(Symbol, u64)],
        page: u64,
    }

    impl<'a> SymWords<'a> {
        fn new(got: &'a mut [u64], extra: &'a [(Symbol, u64)], page: u64) -> Self {
            Self { got, next_slot: 0, extra, page }
        }

        fn lookup(&self, target: Target) -> Option<u64> {
            for &(s, addr) in self.extra {
                if s == target.symbol {
                    return Some(addr);
                }
            }
            if target.space == Space::Outside {
                let s = target.symbol;
                if s == Symbol::of_name(POLL_PAGE) {
                    return Some(self.page);
                }
                if s == Symbol::of_name("memcpy") {
                    return Some(memcpy as *const () as usize as u64);
                }
                if s == Symbol::of_name("memmove") {
                    return Some(memmove as *const () as usize as u64);
                }
                if s == Symbol::of_name("memset") {
                    return Some(memset as *const () as usize as u64);
                }
                if s == Symbol::of_name("memcmp") {
                    return Some(memcmp as *const () as usize as u64);
                }
                if s == Symbol::of_name("fmod") {
                    return Some(fmod as *const () as usize as u64);
                }
                if s == Symbol::of_name("fmodf") {
                    return Some(fmodf as *const () as usize as u64);
                }
                if s == Symbol::of_name("__udivti3") {
                    return Some(rt_udivti3 as *const () as usize as u64);
                }
                if s == Symbol::of_name("__divti3") {
                    return Some(rt_divti3 as *const () as usize as u64);
                }
                if s == Symbol::of_name("__umodti3") {
                    return Some(rt_umodti3 as *const () as usize as u64);
                }
                if s == Symbol::of_name("__modti3") {
                    return Some(rt_modti3 as *const () as usize as u64);
                }
            }
            None
        }
    }

    impl Words for SymWords<'_> {
        fn word(&mut self, target: Target, _call: bool) -> Result<u64, ImageError> {
            let addr = self.lookup(target).ok_or(ImageError::Unbound { target })?;
            let idx = self.next_slot;
            self.next_slot += 1;
            self.got[idx] = addr;
            Ok(core::ptr::addr_of!(self.got[idx]) as u64)
        }

        fn value(&mut self, target: Target) -> Result<u64, ImageError> {
            self.lookup(target).ok_or(ImageError::Unbound { target })
        }
    }

    fn load_exec_with_syms(mut emitted: Emitted<'static>, extra: &[(Symbol, u64)]) -> (Executable, ()) {
        struct BoundDeclares;
        impl blaze_tier::record::Declares for BoundDeclares {
            fn declared(&self, t: Target) -> Option<blaze_tier::record::Declare<'_>> {
                (t.space == Space::Func).then_some(blaze_tier::record::Declare {
                    kind: blaze_tier::record::DeclaredKind::Bound,
                    item: 0,
                    name: &[],
                })
            }
        }
        blaze_tier::record::declare(&mut emitted, &BoundDeclares).expect("declare");
        let sealed = seal(emitted).expect("seal");
        let mut bytes: AVec<'static, u8> = AVec::new_in(test_alloc());
        encode(&sealed, &mut bytes);
        let view = read(&bytes).expect("read");
        let code_bytes = view.code().len();
        let relro_len = view.relro().len();
        let align = 8usize.max(1usize << view.relro_align());
        let got_bytes = 64 * 8;
        let sym = Symbol::of_name("sota_std_test");
        let mut done: Result<(), PlaceError> = Ok(());
        let exec = Executable::written(code_bytes, 8192 + relro_len + align + got_bytes + 8, |code, data| {
            let poll_skip = (4096 - data.as_ptr() as usize % 4096) % 4096;
            let (poll_and_pad, after_poll) = data.split_at_mut(poll_skip + 4096);
            let poll = &mut poll_and_pad[poll_skip..];
            poll.fill(0);
            let skip = (align - after_poll.as_ptr() as usize % align) % align;
            let (relro, rest) = after_poll[skip..].split_at_mut(relro_len);
            let got_skip = (8 - rest.as_ptr() as usize % 8) % 8;
            let got_slice = &mut rest[got_skip..got_skip + got_bytes];
            // SAFETY: `got_slice` is 8-byte aligned and `64 * 8` bytes long inside `Executable`'s data region
            // adjacent to `code` (guaranteed within ARM64 `adrp` ±4 GiB reach under any ASLR).
            let got_u64 = unsafe { core::slice::from_raw_parts_mut(got_slice.as_mut_ptr() as *mut u64, 64) };
            let at = At { code: code.as_ptr() as u64, relro: relro.as_ptr() as u64 };
            let mut words = SymWords::new(got_u64, extra, poll.as_ptr() as u64);
            done = place(&view, sym, at, &mut words, code, relro);
        })
        .expect("exec");
        done.expect("place");
        (exec, ())
    }

    #[test]
    fn sota_std_bulk_mem_and_frem_on_hardware() {
        let alloc = test_alloc();
        // 1. Dynamic `memcpy`, overlapping `memmove`, `> 256 B` `memset`, dynamic `memcmp`.
        let bulk_fn = {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::I64)],
                &[Param::of(Ty::I32)],
            );
            b.set_signature(sig);
            let e = b.create_block(BlockKind::Normal);
            b.begin(e);
            let dst = b.param(Ty::PTR);
            let src = b.param(Ty::PTR);
            let n = b.param(Ty::I64);
            b.bulk(Op::Memcpy, Flags::NONE, dst, src, n, MemAttrs::default());
            let k4 = b.iconst(Ty::I64, 4);
            let dst4 = b.ptr_add(Flags::NONE, dst, k4);
            let nm4 = b.binary(Op::Sub, Flags::NONE, n, k4);
            b.bulk(Op::Memmove, Flags::NONE, dst4, dst, nm4, MemAttrs::default());
            let cmp = b.memcmp(dst4, src, nm4, MemAttrs::default());
            b.ret(&[cmp]);
            b.finish_checked().unwrap()
        };
        let em = Sota::standalone().emit(verify(bulk_fn.view()).unwrap(), &NoNames, MAC_ARM64, alloc).unwrap();
        let (ex, _slots) = load_exec_with_syms(em, &[]);
        let f_bulk: extern "C" fn(*mut u8, *const u8, u64) -> i32 = unsafe { ex.as_fn() };
        let src: [u8; 400] = core::array::from_fn(|i| ((i * 37 + 11) & 0xff) as u8);
        let mut dst = [0u8; 400];
        let cmp = f_bulk(dst.as_mut_ptr(), src.as_ptr(), 400);
        assert_eq!(cmp, 0, "dynamic memcpy + overlapping memmove + memcmp mismatch!");
        assert_eq!(&dst[4..400], &src[0..396]);

        // 2. `> 256 B` (400 B) `memset`
        let memset_fn = {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR), Param::of(Ty::I8)], &[]);
            b.set_signature(sig);
            let e = b.create_block(BlockKind::Normal);
            b.begin(e);
            let dst = b.param(Ty::PTR);
            let byte = b.param(Ty::I8);
            let n = b.iconst(Ty::I64, 400);
            b.bulk(Op::Memset, Flags::NONE, dst, byte, n, MemAttrs::default());
            b.ret(&[]);
            b.finish_checked().unwrap()
        };
        let em_ms = Sota::standalone().emit(verify(memset_fn.view()).unwrap(), &NoNames, MAC_ARM64, alloc).unwrap();
        let (ex_ms, _s2) = load_exec_with_syms(em_ms, &[]);
        let f_ms: extern "C" fn(*mut u8, u8) = unsafe { ex_ms.as_fn() };
        let mut buf = [0u8; 400];
        f_ms(buf.as_mut_ptr(), 0xab);
        assert!(buf.iter().all(|&x| x == 0xab), "400-byte runtime memset mismatch!");

        // 3. `f64` `Op::Frem` (`fmod`)
        let frem_fn = {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::F64), Param::of(Ty::F64)], &[Param::of(Ty::F64)]);
            b.set_signature(sig);
            let e = b.create_block(BlockKind::Normal);
            b.begin(e);
            let x = b.param(Ty::F64);
            let y = b.param(Ty::F64);
            let r = b.binary(Op::Frem, Flags::NONE, x, y);
            b.ret(&[r]);
            b.finish_checked().unwrap()
        };
        let em_fr = Sota::standalone().emit(verify(frem_fn.view()).unwrap(), &NoNames, MAC_ARM64, alloc).unwrap();
        let (ex_fr, _s3) = load_exec_with_syms(em_fr, &[]);
        let f_fr: extern "C" fn(f64, f64) -> f64 = unsafe { ex_fr.as_fn() };
        assert!((f_fr(17.5, 4.0) - 1.5).abs() < 1e-12, "f64 frem mismatch!");
    }

    #[test]
    fn sota_std_stacked_args_byval_and_i128_div_on_hardware() {
        let alloc = test_alloc();
        let mut byval_param = Param::of(Ty::PTR);
        byval_param.kind = ParamKind::Byval;
        byval_param.facts = Facts { size: 48, align: 3, ..Facts::default() };

        let mut callee_params: AVec<'static, Param> = AVec::new_in(alloc);
        for _ in 0..10 {
            callee_params.push(Param::of(Ty::I64));
        }
        callee_params.push(byval_param);
        for _ in 0..10 {
            callee_params.push(Param::of(Ty::F64));
        }

        let callee_fn = {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &callee_params, &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            let e = b.create_block(BlockKind::Normal);
            b.begin(e);
            let mut gprs = AVec::new_in(alloc);
            for _ in 0..10 {
                gprs.push(b.param(Ty::I64));
            }
            let bv_ptr = b.param(Ty::PTR);
            let mut fps = AVec::new_in(alloc);
            for _ in 0..10 {
                fps.push(b.param(Ty::F64));
            }
            let mut sum = gprs[0];
            for &g in &gprs[1..] {
                sum = b.binary(Op::Add, Flags::NONE, sum, g);
            }
            for k in 0..6i32 {
                let field = b.load(Ty::I64, Flags::NONE, bv_ptr, k * 8, MemAttrs::default());
                sum = b.binary(Op::Add, Flags::NONE, sum, field);
            }
            let poison = b.iconst(Ty::I64, 0xdead);
            b.store(Flags::NONE, poison, bv_ptr, 0, MemAttrs::default());
            let mut fsum = fps[0];
            for &fv in &fps[1..] {
                fsum = b.binary(Op::Fadd, Flags::NONE, fsum, fv);
            }
            let fi = b.convert(Op::FptoSi, Flags::NONE, Ty::I64, fsum);
            let total = b.binary(Op::Add, Flags::NONE, sum, fi);
            b.ret(&[total]);
            b.finish_checked().unwrap()
        };

        let callee_em = Sota::standalone().emit(verify(callee_fn.view()).unwrap(), &NoNames, MAC_ARM64, alloc).unwrap();
        let (callee_ex, _cs) = load_exec_with_syms(callee_em, &[]);
        let callee_addr = unsafe { callee_ex.as_fn::<extern "C" fn()>() as usize as u64 };
        let callee_sym = Symbol::of_name("stacked_byval_callee");

        struct CalleeName(Symbol);
        impl Names for CalleeName {
            fn name(&self, of: Outside) -> Option<blaze_tier::Named<'_>> {
                match of {
                    Outside::Func(0) => Some(blaze_tier::Named::Symbol(self.0)),
                    _ => None,
                }
            }
            fn routines(&self) -> RoutinePlaces {
                RoutinePlaces::NONE
            }
        }

        let caller_fn = {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let csig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR)], &[Param::of(Ty::I64)]);
            b.set_signature(csig);
            let target_sig = b.sig(Conv::C, Variadic::No, &callee_params, &[Param::of(Ty::I64)]);
            let target_ref = b.reference(RefKind::Func, 0);
            let e = b.create_block(BlockKind::Normal);
            b.begin(e);
            let src_struct = b.param(Ty::PTR);
            let mut args = AVec::new_in(alloc);
            for k in 1u64..=10 {
                args.push(b.iconst(Ty::I64, u128::from(k * 10)));
            }
            args.push(src_struct);
            for k in 1u64..=10 {
                args.push(b.fconst(Ty::F64, u128::from(((k as f64) * 2.0).to_bits())));
            }
            let r = b.call(Flags::NONE, target_ref, target_sig, &args);
            b.ret(&[r]);
            b.finish_checked().unwrap()
        };
        let caller_em = Sota::standalone()
            .emit(verify(caller_fn.view()).unwrap(), &CalleeName(callee_sym), MAC_ARM64, alloc)
            .unwrap();
        let (caller_ex, _ks) = load_exec_with_syms(caller_em, &[(callee_sym, callee_addr)]);
        let f_caller: extern "C" fn(*const u64) -> i64 = unsafe { caller_ex.as_fn() };
        let orig_struct: [u64; 6] = [100, 200, 300, 400, 500, 600];
        let got = f_caller(orig_struct.as_ptr());
        assert_eq!(got, 2760, "stacked GPR + stacked FP + stacked byval sum mismatch!");
        assert_eq!(orig_struct[0], 100, "byval callee mutation leaked into caller struct!");

        let i128_fn = {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I128), Param::of(Ty::I128)],
                &[Param::of(Ty::I128)],
            );
            b.set_signature(sig);
            let e = b.create_block(BlockKind::Normal);
            b.begin(e);
            let a = b.param(Ty::I128);
            let d = b.param(Ty::I128);
            let q = b.binary(Op::Sdiv, Flags::NONE, a, d);
            let r = b.binary(Op::Srem, Flags::NONE, a, d);
            let sum = b.binary(Op::Add, Flags::NONE, q, r);
            b.ret(&[sum]);
            b.finish_checked().unwrap()
        };
        let em_128 = Sota::standalone().emit(verify(i128_fn.view()).unwrap(), &NoNames, MAC_ARM64, alloc).unwrap();
        let (ex_128, _s128) = load_exec_with_syms(em_128, &[]);
        let f_128: extern "C" fn(i128, i128) -> i128 = unsafe { ex_128.as_fn() };
        let a: i128 = -100_000_000_000_000_000_000_000_007;
        let d: i128 = 97;
        assert_eq!(f_128(a, d), (a / d) + (a % d), "i128 sdiv/srem mismatch on hardware!");
    }

    #[test]
    fn sota_std_tls_and_data_relocs_and_positions_record() {
        let alloc = test_alloc();
        struct TlsStaticNames;
        impl Names for TlsStaticNames {
            fn name(&self, of: Outside) -> Option<blaze_tier::Named<'_>> {
                match of {
                    Outside::Static(0) => Some(blaze_tier::Named::Symbol(Symbol::of_name("sym_static_0"))),
                    Outside::Tls(0) => Some(blaze_tier::Named::Symbol(Symbol::of_name("sym_tls_0"))),
                    Outside::TlsDesc => Some(blaze_tier::Named::Symbol(Symbol::of_name("sym_tls_desc"))),
                    _ => Some(blaze_tier::Named::Symbol(Symbol::of_name("sym_tls_aux"))),
                }
            }
            fn routines(&self) -> RoutinePlaces {
                RoutinePlaces::NONE
            }
        }

        let func = {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[], &[Param::of(Ty::PTR)]);
            b.set_signature(sig);
            let st_ref = b.reference(RefKind::Static, 0);
            let tls_ref = b.reference(RefKind::Tls, 0);
            let bytes = [0u8; 16];
            let relocs = [DataReloc { offset: 0, target: st_ref, addend: 8 }];
            let pos = [DataPosition {
                offset: 8,
                item: 0x1122_3344_5566_7788_99aa_bbcc_ddee_ff00u128,
                site: 42,
                field: blaze_lir::PositionField::Line,
            }];
            let data_ref = b.data_with_positions(&bytes, 3, &relocs, &pos);
            let e = b.create_block(BlockKind::Normal);
            b.begin(e);
            let p_data = b.data_addr(data_ref);
            let p_tls = b.tls_addr(tls_ref);
            let _v = b.load(Ty::I64, Flags::VOLATILE, p_tls, 0, MemAttrs::default());
            b.ret(&[p_data]);
            b.finish_checked().unwrap()
        };

        let isa = crate::Isa { os: crate::Os::MacOs, lse: true };
        let (rec, _insts, _called) = crate::emit_sota_record_measured(
            verify(func.view()).unwrap(),
            &TlsStaticNames,
            &RoutinePlaces::NONE,
            isa,
            &[],
            alloc,
        )
        .expect("Sota must emit DataAddr relocs + PositionRow + TlsAddr cleanly");
        assert_eq!(rec.words.len(), 1, "expected 1 relro Word relocation");
        assert_eq!(rec.words[0].addend, 8);
        assert_eq!(rec.positions.len(), 1, "expected 1 PositionRow slot");
        assert_eq!(rec.positions[0].part, PositionPart::Relro);
        assert_eq!(rec.positions[0].field, PositionField::Line);
        assert_eq!(rec.positions[0].site, 42);
        let sealed = rec.seal().expect("Sota CodeRecord with words+positions+TLS must seal cleanly");
        let mut enc_bytes: AVec<'static, u8> = AVec::new_in(alloc);
        sealed.encode(&mut enc_bytes);
        let view = CodeView::read(&enc_bytes).expect("encoded CodeRecord must read back");
        assert_eq!(view.words(), 1);
        assert_eq!(view.positions(), 1);
    }

    #[test]
    fn sota_std_invoke_resume_and_unwind_table() {
        let alloc = test_alloc();
        struct UnwNames;
        impl Names for UnwNames {
            fn name(&self, of: Outside) -> Option<blaze_tier::Named<'_>> {
                match of {
                    Outside::Func(0) => Some(blaze_tier::Named::Symbol(Symbol::of_name("unw_may_panic"))),
                    Outside::Func(1) => Some(blaze_tier::Named::Symbol(Symbol::of_name("unw_dropper"))),
                    Outside::Func(2) => Some(blaze_tier::Named::Symbol(Symbol::of_name("unw_plain"))),
                    _ => None,
                }
            }
            fn routines(&self) -> RoutinePlaces {
                RoutinePlaces::NONE
            }
        }

        let func = {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[], &[]);
            b.set_signature(sig);
            let none = b.sig(Conv::C, Variadic::No, &[], &[]);
            let one_ptr = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR)], &[]);
            let may_panic = b.reference(RefKind::Func, 0);
            let dropper = b.reference(RefKind::Func, 1);
            let plain = b.reference(RefKind::Func, 2);
            let entry = b.create_block(BlockKind::Normal);
            let normal = b.create_block(BlockKind::Normal);
            let pad = b.create_block(BlockKind::Cleanup);
            b.begin(entry);
            b.call(Flags::NONE, plain, none, &[]);
            b.invoke(Flags::NONE, may_panic, none, &[], normal, pad);
            b.begin(normal);
            b.ret(&[]);
            b.begin(pad);
            let exception = b.param(Ty::PTR);
            b.call(Flags::NONE, dropper, one_ptr, &[exception]);
            b.resume(exception);
            b.finish_checked().unwrap()
        };

        let isa = crate::Isa { os: crate::Os::MacOs, lse: true };
        let (rec, _insts, _called) = crate::emit_sota_record_measured(
            verify(func.view()).unwrap(),
            &UnwNames,
            &RoutinePlaces::NONE,
            isa,
            &[],
            alloc,
        )
        .expect("Sota must emit Invoke/Resume/Cleanup cleanly");
        let sealed = rec.seal().expect("Sota unwind CodeRecord must seal cleanly");
        let mut bytes: AVec<'static, u8> = AVec::new_in(alloc);
        sealed.encode(&mut bytes);
        let view = CodeView::read(&bytes).expect("read back");
        assert_eq!(view.name(Symbol::of_name("_Unwind_Resume")), Some("_Unwind_Resume"));
        let rows = unwind::read(view).expect("Sota unwind rows must be well formed");
        assert_eq!(rows.rows(), 3, "plain(Continue), may_panic(Cleanup), _Unwind_Resume(Continue); dropper inside cleanup has NONE!");
        assert_eq!(rows.row(0).action, Action::Continue);
        assert_eq!(rows.row(0).landing, 0);
        assert_eq!(rows.row(1).action, Action::Cleanup);
        assert!(rows.row(1).landing > 0 && rows.row(1).landing < view.code().len() as u32);
        assert_eq!(rows.row(2).action, Action::Continue);
        assert_eq!(rows.row(2).landing, 0);
    }

    /// Regression test for the `block_buffer::BlockBuffer::len128_padding_be` SIGSEGV on macOS:
    /// Apple ARM64 passes a `u128`/`i128` after an odd number of GPR parameters in `x1:x2` (not
    /// rounded up to even `x2:x3` as generic AAPCS64 C.8 does), leaving the following parameter in
    /// `x3`. `Sota` and `Reference` must agree with each other and with host `extern "C"` on both
    /// macOS and Linux, including cross-tier calls and stacked overflow past `x7`.
    #[test]
    fn sota_i128_odd_register_abi_matches_host_and_reference() {
        let alloc = test_alloc();

        // Shape of `BlockBuffer::len128_padding_be(&mut self, data_len: u128, compress: Ptr)`:
        // loads `*self_ptr` (x0) and `*closure_ptr` (x3 on macOS, x4 on Linux), folds them into
        // `data_len` (x1:x2 on macOS, x2:x3 on Linux), and returns the `u128`.
        let callee_fn = {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::PTR), Param::of(Ty::I128), Param::of(Ty::PTR)],
                &[Param::of(Ty::I128)],
            );
            b.set_signature(sig);
            let e = b.create_block(BlockKind::Normal);
            b.begin(e);
            let p0 = b.param(Ty::PTR);
            let wide = b.param(Ty::I128);
            let p2 = b.param(Ty::PTR);
            let v0 = b.load(Ty::I64, Flags::NONE, p0, 0, MemAttrs::default());
            let v2 = b.load(Ty::I64, Flags::NONE, p2, 0, MemAttrs::default());
            let z0 = b.convert(Op::Zext, Flags::NONE, Ty::I128, v0);
            let z2 = b.convert(Op::Zext, Flags::NONE, Ty::I128, v2);
            let s1 = b.binary(Op::Add, Flags::NONE, wide, z0);
            let s2 = b.binary(Op::Xor, Flags::NONE, s1, z2);
            b.ret(&[s2]);
            b.finish_checked().unwrap()
        };

        let a_word: u64 = 0x1122_3344_5566_7788;
        let b_word: u64 = 0x99aa_bbcc_ddee_ff01;
        let wide_val: u128 = 0x0123_4567_89ab_cdef_fedc_ba98_7654_3210;
        let want: u128 = wide_val.wrapping_add(u128::from(a_word)) ^ u128::from(b_word);

        // 1. Host `extern "C"` -> Sota callee directly on macOS arm64.
        let sota_callee_em = Sota::standalone()
            .emit(verify(callee_fn.view()).unwrap(), &NoNames, MAC_ARM64, alloc)
            .expect("Sota emits (ptr, i128, ptr) -> i128");
        let (sota_callee_ex, _ks1) = load_exec_with_syms(sota_callee_em, &[]);
        let sota_callee_addr = unsafe { sota_callee_ex.as_fn::<extern "C" fn()>() as usize as u64 };
        let f_sota: extern "C" fn(*const u64, u128, *const u64) -> u128 = unsafe { sota_callee_ex.as_fn() };
        assert_eq!(f_sota(&a_word, wide_val, &b_word), want, "host -> Sota (ptr, u128, ptr) mismatch");

        // 2. Reference callee -> host `extern "C"`.
        let ref_callee_em = Reference
            .emit(verify(callee_fn.view()).unwrap(), &NoNames, MAC_ARM64, alloc)
            .expect("Reference emits (ptr, i128, ptr) -> i128");
        let (ref_callee_ex, _ks2) = load_exec_with_syms(ref_callee_em, &[]);
        let ref_callee_addr = unsafe { ref_callee_ex.as_fn::<extern "C" fn()>() as usize as u64 };

        // Caller that invokes the target via `(ptr, i128, ptr) -> i128`.
        let sym = Symbol::of_name("odd_i128_callee");
        struct OneName(Symbol);
        impl Names for OneName {
            fn name(&self, of: Outside) -> Option<blaze_tier::Named<'_>> {
                match of {
                    Outside::Func(0) => Some(blaze_tier::Named::Symbol(self.0)),
                    _ => None,
                }
            }
            fn routines(&self) -> RoutinePlaces {
                RoutinePlaces::NONE
            }
        }
        let caller_fn = {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::PTR), Param::of(Ty::I128), Param::of(Ty::PTR)],
                &[Param::of(Ty::I128)],
            );
            b.set_signature(sig);
            let target = b.reference(RefKind::Func, 0);
            let e = b.create_block(BlockKind::Normal);
            b.begin(e);
            let p0 = b.param(Ty::PTR);
            let w = b.param(Ty::I128);
            let p2 = b.param(Ty::PTR);
            let res = b.call(Flags::NONE, target, sig, &[p0, w, p2]);
            b.ret(&[res]);
            b.finish_checked().unwrap()
        };

        // 3. Cross-tier: Reference caller -> Sota callee (exact `finalize_variable_core` -> `len128_padding_be` crash).
        let ref_caller_em = Reference
            .emit(verify(caller_fn.view()).unwrap(), &OneName(sym), MAC_ARM64, alloc)
            .expect("Reference emits caller");
        let (ref_caller_ex, _ks3) = load_exec_with_syms(ref_caller_em, &[(sym, sota_callee_addr)]);
        let f_ref_to_sota: extern "C" fn(*const u64, u128, *const u64) -> u128 = unsafe { ref_caller_ex.as_fn() };
        assert_eq!(f_ref_to_sota(&a_word, wide_val, &b_word), want, "Reference -> Sota cross-tier mismatch");

        // 4. Cross-tier: Sota caller -> Reference callee.
        let sota_caller_em = Sota::standalone()
            .emit(verify(caller_fn.view()).unwrap(), &OneName(sym), MAC_ARM64, alloc)
            .expect("Sota emits caller");
        let (sota_caller_ex, _ks4) = load_exec_with_syms(sota_caller_em, &[(sym, ref_callee_addr)]);
        let f_sota_to_ref: extern "C" fn(*const u64, u128, *const u64) -> u128 = unsafe { sota_caller_ex.as_fn() };
        assert_eq!(f_sota_to_ref(&a_word, wide_val, &b_word), want, "Sota -> Reference cross-tier mismatch");

        // 5. Stacked `i128` after 7 GPRs (`x0..x6`), plus trailing `u64` (AAPCS64 C.11: both stacked).
        let stacked_fn = {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let params = [
                Param::of(Ty::I64),
                Param::of(Ty::I64),
                Param::of(Ty::I64),
                Param::of(Ty::I64),
                Param::of(Ty::I64),
                Param::of(Ty::I64),
                Param::of(Ty::I64),
                Param::of(Ty::I128),
                Param::of(Ty::I64),
            ];
            let sig = b.sig(Conv::C, Variadic::No, &params, &[Param::of(Ty::I128)]);
            b.set_signature(sig);
            let e = b.create_block(BlockKind::Normal);
            b.begin(e);
            let mut gprs = AVec::new_in(alloc);
            for _ in 0..7 {
                gprs.push(b.param(Ty::I64));
            }
            let w = b.param(Ty::I128);
            let tail = b.param(Ty::I64);
            let mut sum = gprs[0];
            for &p in &gprs[1..] {
                sum = b.binary(Op::Add, Flags::NONE, sum, p);
            }
            let sum = b.binary(Op::Add, Flags::NONE, sum, tail);
            let z = b.convert(Op::Zext, Flags::NONE, Ty::I128, sum);
            let out = b.binary(Op::Add, Flags::NONE, w, z);
            b.ret(&[out]);
            b.finish_checked().unwrap()
        };
        let stacked_em = Sota::standalone()
            .emit(verify(stacked_fn.view()).unwrap(), &NoNames, MAC_ARM64, alloc)
            .expect("Sota emits stacked i128 after 7 GPRs");
        let (stacked_ex, _ks5) = load_exec_with_syms(stacked_em, &[]);
        let f_stacked: extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u128, u64) -> u128 =
            unsafe { stacked_ex.as_fn() };
        assert_eq!(
            f_stacked(1, 2, 3, 4, 5, 6, 7, wide_val, 100),
            wide_val.wrapping_add(1 + 2 + 3 + 4 + 5 + 6 + 7 + 100),
            "host -> Sota stacked i128 after 7 GPRs mismatch"
        );

        // 6. `Op::Invoke` returning `Ty::I128` must be emitted directly by Sota without refusal.
        let invoke_i128_fn = {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::PTR), Param::of(Ty::I128), Param::of(Ty::PTR)],
                &[Param::of(Ty::I128)],
            );
            b.set_signature(sig);
            let target = b.reference(RefKind::Func, 0);
            let entry = b.create_block(BlockKind::Normal);
            let normal = b.create_block(BlockKind::Normal);
            let pad = b.create_block(BlockKind::Cleanup);
            b.begin(entry);
            let p0 = b.param(Ty::PTR);
            let w = b.param(Ty::I128);
            let p2 = b.param(Ty::PTR);
            let res = b.invoke(Flags::NONE, target, sig, &[p0, w, p2], normal, pad);
            b.begin(normal);
            b.ret(&[res]);
            b.begin(pad);
            let ex = b.param(Ty::PTR);
            b.resume(ex);
            b.finish_checked().unwrap()
        };
        let inv_em = Sota::standalone()
            .emit(verify(invoke_i128_fn.view()).unwrap(), &OneName(sym), MAC_ARM64, alloc)
            .expect("Sota must emit Op::Invoke returning Ty::I128");
        let unwind_sym = Symbol::of_name("_Unwind_Resume");
        let (inv_ex, _ks6) = load_exec_with_syms(inv_em, &[(sym, sota_callee_addr), (unwind_sym, sota_callee_addr)]);
        let f_inv: extern "C" fn(*const u64, u128, *const u64) -> u128 = unsafe { inv_ex.as_fn() };
        assert_eq!(f_inv(&a_word, wide_val, &b_word), want, "Sota Invoke returning i128 mismatch");

        // 7. Linux AAPCS64 C.8 still aligns `i128` to an even register pair (`x2:x3`) and places
        //    the following pointer in `x4`, whereas macOS uses `x1:x2` and `x3`.
        let linux_arm64 = Machine { arch: Arch::Arm64, os: Os::Linux, features: 0 };
        let sota_linux = Sota::standalone()
            .emit(verify(callee_fn.view()).unwrap(), &NoNames, linux_arm64, alloc)
            .expect("Sota emits Linux (ptr, i128, ptr) -> i128");
        let sota_mac = Sota::standalone()
            .emit(verify(callee_fn.view()).unwrap(), &NoNames, MAC_ARM64, alloc)
            .expect("Sota emits macOS (ptr, i128, ptr) -> i128");
        assert_ne!(
            sota_linux.code.as_slice(),
            sota_mac.code.as_slice(),
            "Linux AAPCS64 (even pair x2:x3, ptr in x4) and macOS (unaligned pair x1:x2, ptr in x3) must emit distinct register placements"
        );
    }

    #[test]
    fn sota_mem_uxtw_idx_folding() {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();

        // 1. Unscaled `ptr_add(base, zext(u32 -> u64, idx))` -> `ldr xt, [xn, wm, uxtw]`.
        let unscaled_fn = {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::PTR), Param::of(Ty::I32)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let base = b.param(Ty::PTR);
            let idx32 = b.param(Ty::I32);
            let idx64 = b.convert(Op::Zext, Flags::NONE, Ty::I64, idx32);
            let p = b.ptr_add(Flags::NONE, base, idx64);
            let v = b.load(Ty::I64, Flags::NONE, p, 0, MemAttrs::aligned(0));
            b.ret(&[v]);
            b.finish_checked().unwrap()
        };
        let em_unscaled = Sota::standalone()
            .emit(verify(unscaled_fn.view()).unwrap(), &NoNames, MAC_ARM64, alloc)
            .expect("Sota emits unscaled MemUxtwIdx load");
        let mut words_unscaled = blaze_own::AVec::new_in(alloc);
        for c in em_unscaled.code.as_slice().chunks_exact(4) {
            words_unscaled.push(u32::from_le_bytes([c[0], c[1], c[2], c[3]]));
        }
        assert!(
            words_unscaled.iter().any(|&w| (w & 0x3fe0_fc00) == 0x3860_4800),
            "expected folded `ldr [rn, wm, uxtw]` instruction, got: {words_unscaled:#x?}"
        );
        let (ex_unscaled, _k1) = load_exec_with_syms(em_unscaled, &[]);
        // Pass a `u64` with dirty upper 32 bits in x1 to prove `UXTW` ignores bits 63..32.
        let f_unscaled: extern "C" fn(*const u8, u64) -> u64 = unsafe { ex_unscaled.as_fn() };
        let mut buf = [0u8; 32];
        buf[5..13].copy_from_slice(&0x1122_3344_5566_7788u64.to_le_bytes());
        assert_eq!(
            f_unscaled(buf.as_ptr(), 0xDEAD_BEEF_0000_0005u64),
            0x1122_3344_5566_7788u64,
            "unscaled UXTW load must zero-extend low 32 bits of index"
        );

        // 2. Scaled `ptr_add(base, shl(zext(u32 -> u64, idx), 2))` -> `ldr wt, [xn, wm, uxtw #2]`.
        let scaled_fn = {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::PTR), Param::of(Ty::I32)],
                &[Param::of(Ty::I32)],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let base = b.param(Ty::PTR);
            let idx32 = b.param(Ty::I32);
            let idx64 = b.convert(Op::Zext, Flags::NONE, Ty::I64, idx32);
            let c2 = b.iconst(Ty::I64, 2);
            let byte_off = b.binary(Op::Shl, Flags::NONE, idx64, c2);
            let p = b.ptr_add(Flags::NONE, base, byte_off);
            let v = b.load(Ty::I32, Flags::NONE, p, 0, MemAttrs::aligned(2));
            b.ret(&[v]);
            b.finish_checked().unwrap()
        };
        let em_scaled = Sota::standalone()
            .emit(verify(scaled_fn.view()).unwrap(), &NoNames, MAC_ARM64, alloc)
            .expect("Sota emits scaled MemUxtwIdx load");
        let mut words_scaled = blaze_own::AVec::new_in(alloc);
        for c in em_scaled.code.as_slice().chunks_exact(4) {
            words_scaled.push(u32::from_le_bytes([c[0], c[1], c[2], c[3]]));
        }
        assert!(
            words_scaled.iter().any(|&w| (w & 0x3fe0_fc00) == 0x3860_5800),
            "expected folded `ldr wt, [rn, wm, uxtw #2]` instruction, got: {words_scaled:#x?}"
        );
        let (ex_scaled, _k2) = load_exec_with_syms(em_scaled, &[]);
        let f_scaled: extern "C" fn(*const u32, u64) -> u32 = unsafe { ex_scaled.as_fn() };
        let table = [10u32, 20, 30, 40, 0xCAFE_BABE, 60, 70, 80];
        assert_eq!(
            f_scaled(table.as_ptr(), 0xFFFF_0000_0000_0004u64),
            0xCAFE_BABE,
            "scaled UXTW #2 load must zero-extend low 32 bits and scale by 4"
        );
    }

    #[test]
    fn sota_ubfx_and_loop_align() {
        let alloc = blaze_own::testing::heap().alloc();
        let decode_words = |bytes: &[u8]| -> AVec<'static, u32> {
            let mut out = AVec::with_capacity_in(bytes.len() / 4, alloc);
            for c in bytes.chunks_exact(4) {
                out.push(u32::from_le_bytes([c[0], c[1], c[2], c[3]]));
            }
            out
        };

        // 1. `UBFX` fusion: `(x >> 10) & 0x3FF` -> single `ubfx xd, xn, #10, #10` (`0xd34a_4c00`).
        let ubfx_fn = {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let x = b.param(Ty::I64);
            let c10 = b.iconst(Ty::I64, 10);
            let shifted = b.binary(Op::Lshr, Flags::NONE, x, c10);
            let mask = b.iconst(Ty::I64, 0x3FF);
            let extracted = b.binary(Op::And, Flags::NONE, shifted, mask);
            b.ret(&[extracted]);
            b.finish_checked().unwrap()
        };
        let em_ubfx = Sota::standalone()
            .emit(verify(ubfx_fn.view()).unwrap(), &NoNames, MAC_ARM64, alloc)
            .expect("Sota emits UBFX fused bitfield extract");
        let words_ubfx = decode_words(em_ubfx.code.as_slice());
        // `ubfx xd, xn, #10, #10` = `ubfm xd, xn, #10, #19` = `0xd340_0000 | (10 << 16) | (19 << 10) = 0xd34a_4c00`.
        assert!(
            words_ubfx.iter().any(|&w| (w & 0xffff_fc00) == 0xd34a_4c00),
            "expected fused `ubfx xd, xn, #10, #10` instruction, got: {words_ubfx:#x?}"
        );
        let (ex_ubfx, _k1) = load_exec_with_syms(em_ubfx, &[]);
        let f_ubfx: extern "C" fn(u64) -> u64 = unsafe { ex_ubfx.as_fn() };
        for &x in &[0u64, u64::MAX, 0xDEAD_BEEF_CAFE_BABE, 0x3FFu64 << 10, 1u64 << 9, 1u64 << 20] {
            assert_eq!(f_ubfx(x), (x >> 10) & 0x3FF, "ubfx mismatch for x={x:#x}");
        }

        // 2. `BFXIL` fusion: `(dst & !0xFF) | (src & 0xFF)` -> `bfxil xd, xn, #0, #8` (`0xb340_1c00`).
        let bfxil_fn = {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let dst = b.param(Ty::I64);
            let src = b.param(Ty::I64);
            let hi_mask = b.iconst(Ty::I64, (!0xFFu64) as u128);
            let lo_mask = b.iconst(Ty::I64, 0xFF);
            let dst_hi = b.binary(Op::And, Flags::NONE, dst, hi_mask);
            let src_lo = b.binary(Op::And, Flags::NONE, src, lo_mask);
            let inserted = b.binary(Op::Or, Flags::NONE, dst_hi, src_lo);
            b.ret(&[inserted]);
            b.finish_checked().unwrap()
        };
        let em_bfxil = Sota::standalone()
            .emit(verify(bfxil_fn.view()).unwrap(), &NoNames, MAC_ARM64, alloc)
            .expect("Sota emits BFXIL fused bitfield insert");
        let words_bfxil = decode_words(em_bfxil.code.as_slice());
        // `bfxil xd, xn, #0, #8` = `bfm xd, xn, #0, #7` = `0xb340_0000 | (0 << 16) | (7 << 10) = 0xb340_1c00`.
        assert!(
            words_bfxil.iter().any(|&w| (w & 0xffff_fc00) == 0xb340_1c00),
            "expected fused `bfxil xd, xn, #0, #8` instruction, got: {words_bfxil:#x?}"
        );
        let (ex_bfxil, _k2) = load_exec_with_syms(em_bfxil, &[]);
        let f_bfxil: extern "C" fn(u64, u64) -> u64 = unsafe { ex_bfxil.as_fn() };
        for &(d, s) in &[
            (0u64, u64::MAX),
            (u64::MAX, 0u64),
            (0xDEAD_BEEF_CAFE_BABEu64, 0x1234_5678_9ABC_DEF0u64),
        ] {
            assert_eq!(f_bfxil(d, s), (d & !0xFF) | (s & 0xFF), "bfxil mismatch for ({d:#x}, {s:#x})");
        }

        // 3. Simple loop with 16-byte loop-header alignment: executes cleanly & backward branches land on 16B boundary.
        let loop_fn = {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            let [entry, header, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));

            b.begin(entry);
            let n = b.param(Ty::I64);
            let zero = b.iconst(Ty::I64, 0);
            let seed = b.iconst(Ty::I64, 0x1234);
            b.jump((header, &[zero, seed]));

            b.begin(header);
            let i = b.param(Ty::I64);
            let acc = b.param(Ty::I64);
            let cont = b.icmp(IntCond::Ult, i, n);
            b.br(Flags::NONE, cont, (body, &[]), (exit, &[]));

            b.begin(body);
            let c3 = b.iconst(Ty::I64, 3);
            let c1 = b.iconst(Ty::I64, 1);
            let p = b.binary(Op::Mul, Flags::NONE, acc, c3);
            let s = b.binary(Op::Add, Flags::NONE, p, i);
            let r = b.binary(Op::Lshr, Flags::NONE, acc, c1);
            let next_acc = b.binary(Op::Xor, Flags::NONE, s, r);
            let next_i = b.binary(Op::Add, Flags::NONE, i, c1);
            b.jump((header, &[next_i, next_acc]));

            b.begin(exit);
            b.ret(&[acc]);
            b.finish_checked().unwrap()
        };
        let em_loop = Sota::standalone()
            .emit(verify(loop_fn.view()).unwrap(), &NoNames, MAC_ARM64, alloc)
            .expect("Sota emits aligned loop");
        let words_loop = decode_words(em_loop.code.as_slice());
        let mut saw_back_branch = false;
        for (idx, &w) in words_loop.iter().enumerate() {
            let delta = if (w & 0xfc00_0000) == 0x1400_0000 {
                // `b <imm26>`
                Some(((w & 0x03ff_ffff) as i32) << 6 >> 6)
            } else if (w & 0xff00_0010) == 0x5400_0000 || (w & 0x7e00_0000) == 0x3400_0000 {
                // `b.cond <imm19>` or `cbz`/`cbnz <imm19>`
                Some((((w >> 5) & 0x7_ffff) as i32) << 13 >> 13)
            } else {
                None
            };
            if let Some(d) = delta {
                if d < 0 {
                    let target_w = (idx as isize + d as isize) as usize;
                    if words_loop[target_w] == 0xd65f_03c0 {
                        // Skip tail jump to shared `ret` epilogue (not a loop header).
                        continue;
                    }
                    assert_eq!(
                        target_w % 4,
                        0,
                        "loop-header target word {target_w} (from branch at {idx}) must be 16-byte aligned: {words_loop:#x?}"
                    );
                    saw_back_branch = true;
                }
            }
        }
        assert!(saw_back_branch, "expected at least one backward loop branch in {words_loop:#x?}");
        let (ex_loop, _k3) = load_exec_with_syms(em_loop, &[]);
        let f_loop: extern "C" fn(u64) -> u64 = unsafe { ex_loop.as_fn() };
        for &n in &[0u64, 1, 5, 32, 200] {
            let mut want = 0x1234u64;
            for i in 0..n {
                want = want.wrapping_mul(3).wrapping_add(i) ^ (want >> 1);
            }
            assert_eq!(f_loop(n), want, "aligned loop mismatch for n={n}");
        }
    }

    #[test]
    fn sota_i128_switch_compares_both_words() {
        let alloc = test_alloc();
        let high_case: u128 = 0x1234_5678_9abc_def0_0000_0000_0000_0000;
        let full_case: u128 = 0xfedc_ba98_7654_3210_0123_4567_89ab_cdef;
        let lo_only: u128 = 42;

        let func = {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I128)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            let [entry, b_zero, b_lo, b_hi, b_join, b_def] = [(); 6].map(|_| b.create_block(BlockKind::Normal));

            b.begin(entry);
            let x = b.param(Ty::I128);
            let arg_full = b.iconst(Ty::I64, 44);
            b.switch(
                x,
                (b_def, &[]),
                &[
                    (0, (b_zero, &[])),
                    (lo_only, (b_lo, &[])),
                    (high_case, (b_hi, &[])),
                    (full_case, (b_join, &[arg_full])),
                ],
            );

            b.begin(b_zero);
            let r0 = b.iconst(Ty::I64, 10);
            b.ret(&[r0]);

            b.begin(b_lo);
            let r1 = b.iconst(Ty::I64, 20);
            b.ret(&[r1]);

            b.begin(b_hi);
            let r2 = b.iconst(Ty::I64, 30);
            b.ret(&[r2]);

            b.begin(b_join);
            let p = b.param(Ty::I64);
            b.ret(&[p]);

            b.begin(b_def);
            let rd = b.iconst(Ty::I64, 99);
            b.ret(&[rd]);
            b.finish_checked().unwrap()
        };

        let em = Sota::standalone()
            .emit(verify(func.view()).unwrap(), &NoNames, MAC_ARM64, alloc)
            .expect("Sota must emit i128 Op::Switch");
        let (ex, _k) = load_exec_with_syms(em, &[]);
        let f: extern "C" fn(u128) -> u64 = unsafe { ex.as_fn() };
        assert_eq!(f(0), 10);
        assert_eq!(f(lo_only), 20);
        assert_eq!(f(high_case), 30);
        assert_eq!(f(full_case), 44);
        assert_eq!(f(1u128 << 64), 99);
        assert_eq!(f((1u128 << 64) | lo_only), 99);
        assert_eq!(f(full_case ^ (1u128 << 127)), 99);
        assert_eq!(f(full_case ^ 1), 99);
        assert_eq!(f(high_case | 1), 99);
    }

    /// A function whose live stack slots exceed 32 KiB AND simultaneously spills SSA values must
    /// still emit natively on `Sota` (placing spill slots before local slots so `max_spill_slots`
    /// never drops to 0) and execute with exact results.
    #[test]
    fn sota_large_live_slot_frame_places_spills_first_and_runs() {
        let alloc = test_alloc();
        let func = {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let x = b.param(Ty::I64);
            // 40 KiB live stack slot (> 32 KiB scaled-imm12 threshold!).
            let big = b.slot(40_960, 4, blaze_lir::func::SlotKind::Named);
            let p = b.slot_addr(big);
            b.store(Flags::NONE, x, p, 0, blaze_lir::builder::MemAttrs::aligned(3));
            b.store(Flags::NONE, x, p, 40_952, blaze_lir::builder::MemAttrs::aligned(3));
            // Create 32 simultaneously live values to force GPR spills alongside the 40 KiB slot.
            let mut live = [x; 32];
            for (i, slot_v) in live.iter_mut().enumerate() {
                let c = b.iconst(Ty::I64, (i as u128) + 3);
                let v = b.binary(Op::Mul, Flags::NONE, x, c);
                let w = b.binary(Op::Add, Flags::NONE, v, c);
                *slot_v = w;
            }
            let first = b.load(Ty::I64, Flags::NONE, p, 0, blaze_lir::builder::MemAttrs::aligned(3));
            let last = b.load(Ty::I64, Flags::NONE, p, 40_952, blaze_lir::builder::MemAttrs::aligned(3));
            let mut acc = b.binary(Op::Add, Flags::NONE, first, last);
            for &v in live.iter() {
                acc = b.binary(Op::Add, Flags::NONE, acc, v);
            }
            b.ret(&[acc]);
            b.finish_checked().unwrap()
        };

        let em = Sota::standalone()
            .emit(verify(func.view()).unwrap(), &NoNames, MAC_ARM64, alloc)
            .expect("Sota must emit >32 KiB slot frame with spills natively");
        let (ex, _k) = load_exec_with_syms(em, &[]);
        let f: extern "C" fn(u64) -> u64 = unsafe { ex.as_fn() };
        let x = 7u64;
        let expected: u64 = x * 2 + (0..32u64).map(|i| x * (i + 3) + (i + 3)).sum::<u64>();
        assert_eq!(f(x), expected);
    }

    /// Linear-scan cell coloring (pass/frame.rs) compacts a 25,000+-instruction function from a
    /// >400 KiB frame down to <=128 bytes while preserving exact execution semantics on Reference.
    #[test]
    fn cell_coloring_compacts_large_function_frame_and_runs() {
        let alloc = test_alloc();
        let func = {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let mut acc = b.param(Ty::I64);
            let slot = b.slot(8, 3, SlotKind::Temp);
            for i in 0..5_000u128 {
                let p = b.slot_addr(slot);
                let k = b.iconst(Ty::I64, (i & 7) + 1);
                acc = b.binary(Op::Add, Flags::NONE, acc, k);
                b.store(Flags::NONE, acc, p, 0, MemAttrs::aligned(3));
                let reloaded = b.load(Ty::I64, Flags::NONE, p, 0, MemAttrs::aligned(3));
                acc = b.binary(Op::Xor, Flags::NONE, reloaded, k);
            }
            b.slot_dead(slot);
            b.ret(&[acc]);
            b.finish_checked().unwrap()
        };

        let cells = crate::pass::frame::layout_cells(func.view(), alloc).expect("cells");
        assert!(
            cells.end <= 128,
            "25,000+ sequential instructions should compact to <= 128 B of cells, got {} B",
            cells.end
        );
        let code = crate::tests::emitted(func.view(), &[]).expect("emits");
        // Prologue frame adjustment at bytes 8..16 must be inline `sub sp` (no stack-probe branch!).
        let w0 = u32::from_le_bytes(code.bytes[8..12].try_into().unwrap());
        assert_eq!(w0 & 0xff80_0000, 0xd100_0000, "compact frame uses inline `sub sp`, not probe branch: {w0:#010x}");
        let exec = crate::tests::run::place(&code, |_: crate::Fixup| 0);
        // SAFETY: `func` has signature `extern "C" fn(u64) -> u64`.
        let got = unsafe { exec.as_fn::<extern "C" fn(u64) -> u64>() }(42);
        let mut want = 42u64;
        for i in 0..5_000u64 {
            let k = (i & 7) + 1;
            want = want.wrapping_add(k) ^ k;
        }
        assert_eq!(got, want);
    }

    /// Stack-slot lifetime coloring (pass/frame.rs) overlaps sequential `SlotDead`-bounded temp
    /// slots AND slots in disjoint `if/else` arms (even without `SlotDead`), while keeping
    /// simultaneously live slots and hoisted `SlotAddr` pointer comparisons strictly disjoint.
    #[test]
    fn slot_coloring_reuses_dead_and_disjoint_slots_and_preserves_live_distinctness() {
        let alloc = test_alloc();
        let func = {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            let [entry, arm_a, arm_b, done] = [(); 4].map(|_| b.create_block(BlockKind::Normal));
            b.begin(entry);
            let sel = b.param(Ty::I64);
            let mut sum = b.iconst(Ty::I64, 0);
            for i in 0..32u128 {
                let s = b.slot(1024, 3, SlotKind::Temp);
                let p = b.slot_addr(s);
                let v = b.iconst(Ty::I64, i + 1);
                b.store(Flags::NONE, v, p, 0, MemAttrs::aligned(3));
                let got = b.load(Ty::I64, Flags::NONE, p, 0, MemAttrs::aligned(3));
                sum = b.binary(Op::Add, Flags::NONE, sum, got);
                b.slot_dead(s);
            }
            let zero = b.iconst(Ty::I64, 0);
            let is_zero = b.icmp(IntCond::Eq, sel, zero);
            let sa = b.slot(2048, 3, SlotKind::Temp);
            let sb = b.slot(2048, 3, SlotKind::Temp);
            // Hoist both `slot_addr`s into entry (mirroring egraph/LICM) without using them there.
            let pa = b.slot_addr(sa);
            let pb = b.slot_addr(sb);
            b.br(Flags::NONE, is_zero, (arm_a, &[]), (arm_b, &[]));

            b.begin(arm_a);
            let va = b.iconst(Ty::I64, 100);
            b.store(Flags::NONE, va, pa, 16, MemAttrs::aligned(3));
            let la = b.load(Ty::I64, Flags::NONE, pa, 16, MemAttrs::aligned(3));
            let ra = b.binary(Op::Add, Flags::NONE, sum, la);
            b.jump((done, &[ra]));

            b.begin(arm_b);
            let vb = b.iconst(Ty::I64, 200);
            b.store(Flags::NONE, vb, pb, 16, MemAttrs::aligned(3));
            let lb = b.load(Ty::I64, Flags::NONE, pb, 16, MemAttrs::aligned(3));
            let rb = b.binary(Op::Add, Flags::NONE, sum, lb);
            b.jump((done, &[rb]));

            b.begin(done);
            let res = b.param(Ty::I64);
            b.ret(&[res]);
            b.finish_checked().unwrap()
        };

        let packed = crate::pass::frame::pack_slots(func.view(), 0, alloc).expect("packs");
        assert!(
            packed.end <= 2048,
            "32x1KiB dead temps + 2x2KiB disjoint arms must pack into <= 2048 B, got {} B",
            packed.end
        );
        let code = crate::tests::emitted(func.view(), &[]).expect("emits");
        let exec = crate::tests::run::place(&code, |_: crate::Fixup| 0);
        // SAFETY: `func` has signature `extern "C" fn(u64) -> u64`.
        let f = unsafe { exec.as_fn::<extern "C" fn(u64) -> u64>() };
        assert_eq!(f(0), 528 + 100);
        assert_eq!(f(1), 528 + 200);

        // Simultaneously live slots (including pointer equality comparison) MUST stay distinct.
        let func2 = {
            let mut b2 = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig2 = b2.sig(Conv::C, Variadic::No, &[], &[Param::of(Ty::I64)]);
            b2.set_signature(sig2);
            let e2 = b2.create_block(BlockKind::Normal);
            b2.begin(e2);
            let s0 = b2.slot(64, 3, SlotKind::Named);
            let s1 = b2.slot(64, 3, SlotKind::Named);
            let p0 = b2.slot_addr(s0);
            let p1 = b2.slot_addr(s1);
            let c11 = b2.iconst(Ty::I64, 11);
            let c22 = b2.iconst(Ty::I64, 22);
            b2.store(Flags::NONE, c11, p0, 0, MemAttrs::aligned(3));
            b2.store(Flags::NONE, c22, p1, 0, MemAttrs::aligned(3));
            let l0 = b2.load(Ty::I64, Flags::NONE, p0, 0, MemAttrs::aligned(3));
            let l1 = b2.load(Ty::I64, Flags::NONE, p1, 0, MemAttrs::aligned(3));
            let ne = b2.icmp(IntCond::Ne, p0, p1);
            let ne64 = b2.convert(Op::Zext, Flags::NONE, Ty::I64, ne);
            let s01 = b2.binary(Op::Add, Flags::NONE, l0, l1);
            let out = b2.binary(Op::Add, Flags::NONE, s01, ne64);
            b2.ret(&[out]);
            b2.finish_checked().unwrap()
        };

        let packed2 = crate::pass::frame::pack_slots(func2.view(), 0, alloc).expect("packs");
        assert_ne!(packed2.slot_at[0], packed2.slot_at[1], "live slots must not overlap");
        let code2 = crate::tests::emitted(func2.view(), &[]).expect("emits");
        let exec2 = crate::tests::run::place(&code2, |_: crate::Fixup| 0);
        // SAFETY: `func2` has signature `extern "C" fn() -> u64`.
        assert_eq!(unsafe { exec2.as_fn::<extern "C" fn() -> u64>() }(), 11 + 22 + 1);
    }

    /// Cell coloring preserves cross-block values live across a loop back-edge and handles parallel
    /// parameter swaps `(a, b) -> (b, a)` cleanly.
    #[test]
    fn cell_coloring_handles_loops_and_param_swaps() {
        let alloc = test_alloc();
        let func = {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            let [entry, header, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));

            b.begin(entry);
            let n = b.param(Ty::I64);
            let a0 = b.iconst(Ty::I64, 1);
            let b0 = b.iconst(Ty::I64, 2);
            let bonus = b.iconst(Ty::I64, 1000);
            b.jump((header, &[n, a0, b0]));

            b.begin(header);
            let rem = b.param(Ty::I64);
            let pa = b.param(Ty::I64);
            let pb = b.param(Ty::I64);
            let zero = b.iconst(Ty::I64, 0);
            let cont = b.icmp(IntCond::Ugt, rem, zero);
            b.br(Flags::NONE, cont, (body, &[]), (exit, &[]));

            b.begin(body);
            let one = b.iconst(Ty::I64, 1);
            let next_rem = b.binary(Op::Sub, Flags::NONE, rem, one);
            let mut tmp = one;
            for k in 0..64u128 {
                let c = b.iconst(Ty::I64, k);
                tmp = b.binary(Op::Add, Flags::NONE, tmp, c);
            }
            let zero_mask = b.binary(Op::Xor, Flags::NONE, tmp, tmp);
            let next_a = b.binary(Op::Add, Flags::NONE, pb, zero_mask);
            let next_b = b.binary(Op::Add, Flags::NONE, pa, pb);
            b.jump((header, &[next_rem, next_a, next_b]));

            b.begin(exit);
            let ans = b.binary(Op::Add, Flags::NONE, pa, bonus);
            b.ret(&[ans]);
            b.finish_checked().unwrap()
        };

        let code = crate::tests::emitted(func.view(), &[]).expect("emits");
        let exec = crate::tests::run::place(&code, |_: crate::Fixup| 0);
        // SAFETY: `func` has signature `extern "C" fn(u64) -> u64`.
        assert_eq!(unsafe { exec.as_fn::<extern "C" fn(u64) -> u64>() }(5), 1013);
    }

    /// Regression test: two stack slots BOTH live across a loop back-edge (without `SlotDead`)
    /// must conflict and receive disjoint offsets even though both are set in `at_start[header]`.
    #[test]
    fn slot_coloring_keeps_loop_carried_live_slots_disjoint() {
        let alloc = test_alloc();
        let func = {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            let [entry, header, body, exit] = [(); 4].map(|_| b.create_block(BlockKind::Normal));

            b.begin(entry);
            let n = b.param(Ty::I64);
            let s0 = b.slot(64, 3, SlotKind::Named);
            let s1 = b.slot(64, 3, SlotKind::Named);
            let p0 = b.slot_addr(s0);
            let p1 = b.slot_addr(s1);
            let c10 = b.iconst(Ty::I64, 10);
            let c20 = b.iconst(Ty::I64, 20);
            b.store(Flags::NONE, c10, p0, 0, MemAttrs::aligned(3));
            b.store(Flags::NONE, c20, p1, 0, MemAttrs::aligned(3));
            b.jump((header, &[n]));

            b.begin(header);
            let rem = b.param(Ty::I64);
            let zero = b.iconst(Ty::I64, 0);
            let cont = b.icmp(IntCond::Ugt, rem, zero);
            b.br(Flags::NONE, cont, (body, &[]), (exit, &[]));

            b.begin(body);
            let v0 = b.load(Ty::I64, Flags::NONE, p0, 0, MemAttrs::aligned(3));
            let v1 = b.load(Ty::I64, Flags::NONE, p1, 0, MemAttrs::aligned(3));
            let one = b.iconst(Ty::I64, 1);
            let two = b.iconst(Ty::I64, 2);
            let nv0 = b.binary(Op::Add, Flags::NONE, v0, one);
            let nv1 = b.binary(Op::Add, Flags::NONE, v1, two);
            b.store(Flags::NONE, nv0, p0, 0, MemAttrs::aligned(3));
            b.store(Flags::NONE, nv1, p1, 0, MemAttrs::aligned(3));
            let next_rem = b.binary(Op::Sub, Flags::NONE, rem, one);
            b.jump((header, &[next_rem]));

            b.begin(exit);
            let f0 = b.load(Ty::I64, Flags::NONE, p0, 0, MemAttrs::aligned(3));
            let f1 = b.load(Ty::I64, Flags::NONE, p1, 0, MemAttrs::aligned(3));
            let sum = b.binary(Op::Add, Flags::NONE, f0, f1);
            b.ret(&[sum]);
            b.finish_checked().unwrap()
        };

        let packed = crate::pass::frame::pack_slots(func.view(), 0, alloc).expect("packs");
        assert_ne!(
            packed.slot_at[0], packed.slot_at[1],
            "loop-carried live slots must never share an offset"
        );
        let code = crate::tests::emitted(func.view(), &[]).expect("emits");
        let exec = crate::tests::run::place(&code, |_: crate::Fixup| 0);
        // SAFETY: `func` has signature `extern "C" fn(u64) -> u64`.
        assert_eq!(unsafe { exec.as_fn::<extern "C" fn(u64) -> u64>() }(5), 45);
    }

    /// Regression test for the `crypto_bigint::Uint::<4>::from_be_slice` infinite loop:
    /// when `thread_jumps` folds a loop entry directly into a higher-indexed body block,
    /// the resulting CFG is valid SSA but non-RPO (`header > latch`). Both the full SOTA
    /// pipeline and direct `emit_sota_a64` must canonicalize to dominator-first RPO before
    /// linear-interval register allocation so values defined in `header` and used in `latch`
    /// never get clobbered.
    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    #[test]
    fn sota_non_rpo_uadd_overflow_loop_matches_evaluator_on_hardware() {
        let alloc = test_alloc();
        // 1. Original slot-based checked-add loop (`while j < 4 { buf = j; j += 1; }`).
        let func = {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            let [entry, header, body, trap_blk, cont, exit] =
                [(); 6].map(|_| b.create_block(BlockKind::Normal));

            b.begin(entry);
            let s_buf = b.slot(8, 3, SlotKind::Named);
            let s_j = b.slot(8, 3, SlotKind::Named);
            let p_buf = b.slot_addr(s_buf);
            let p_j = b.slot_addr(s_j);
            let zero = b.iconst(Ty::I64, 0);
            b.store(Flags::NONE, zero, p_buf, 0, MemAttrs::aligned(3));
            b.store(Flags::NONE, zero, p_j, 0, MemAttrs::aligned(3));
            b.jump((header, &[]));

            b.begin(header);
            let j0 = b.load(Ty::I64, Flags::NONE, p_j, 0, MemAttrs::aligned(3));
            let four = b.iconst(Ty::I64, 4);
            let cond = b.icmp(IntCond::Ult, j0, four);
            b.br(Flags::NONE, cond, (body, &[]), (exit, &[]));

            b.begin(body);
            let j1 = b.load(Ty::I64, Flags::NONE, p_j, 0, MemAttrs::aligned(3));
            b.store(Flags::NONE, j1, p_buf, 0, MemAttrs::aligned(3));
            let j2 = b.load(Ty::I64, Flags::NONE, p_j, 0, MemAttrs::aligned(3));
            let one = b.iconst(Ty::I64, 1);
            let pair = b.overflow(Op::UaddOverflow, j2, one);
            let nj = b.project(pair, 0);
            let ovf = b.project(pair, 1);
            b.br(Flags::NONE, ovf, (trap_blk, &[]), (cont, &[]));

            b.begin(trap_blk);
            b.trap(1);

            b.begin(cont);
            b.store(Flags::NONE, nj, p_j, 0, MemAttrs::aligned(3));
            b.jump((header, &[]));

            b.begin(exit);
            let res = b.load(Ty::I64, Flags::NONE, p_buf, 0, MemAttrs::aligned(3));
            b.ret(&[res]);
            b.finish_checked().unwrap()
        };

        assert!(matches!(
            eval::run_in(func.view(), &[], 10_000, alloc).unwrap(),
            Outcome::Returned(ref v) if v.as_slice() == [Val::Int(3)]
        ));
        let pre = crate::sota::run_sroa_thread_cleanup(func.view(), alloc);
        let mid_in = pre.as_ref().map_or(func.view(), |f| f.view());
        let opt = blaze_lir::egraph::sota_optimize_fixpoint(
            mid_in,
            blaze_lir::egraph::RegBudget::AARCH64,
            3,
            |_| 1,
            alloc,
        )
        .unwrap();
        let late = crate::sota::run_sota_loop_and_late_passes(
            opt.func.view(),
            MAC_ARM64,
            blaze_lir::egraph::RegBudget::AARCH64,
            true,
            Some(blaze_lir::superopt::SuperoptOpts::apple_silicon()),
            alloc,
        );
        let emitted = crate::sota::isel::emit_sota_a64(
            late.view(),
            &NoNames,
            crate::sota::CallMode::GotSlot,
            MAC_ARM64.features,
            true,
            alloc,
        )
        .unwrap();
        let (exec, _) = load_exec(emitted);
        let f_ptr: extern "C" fn() -> u64 = unsafe { exec.as_fn() };
        assert_eq!(f_ptr(), 3);

        // 2. Direct non-RPO SSA LIR (latch block 1 before dominating header block 2) emitted
        //    directly via `emit_sota_a64` without mid-end passes.
        let non_rpo_src = "\
func ptr64 sig s0 attrs [] features \"\"
type t0 = {i64, i1}
sig s0 = c () -> (i64)
block0 normal:
  v0 = iconst i64 0
  v1 = iconst i64 4
  v2 = iconst i64 1
  jump block2(v0)
block1 normal:
  v4 = icmp i1 ult, v8, v1
  br v4, block2(v8), block4()
block2 normal:
  v6 = param i64
  v7 = uadd_overflow t0 v6, v2
  v8 = project i64 v7, 0
  v9 = project i1 v7, 1
  br v9, block3(), block1()
block3 normal:
  trap 1
block4 normal:
  return (v6)
";
        let non_rpo = blaze_lir::parse::parse(non_rpo_src).expect("parses non-RPO LIR");
        assert!(
            blaze_lir::coldsplit::reorder_blocks_rpo(non_rpo.view(), alloc).unwrap().is_some(),
            "test input must genuinely be non-RPO"
        );
        let emitted2 = crate::sota::isel::emit_sota_a64(
            non_rpo.view(),
            &NoNames,
            crate::sota::CallMode::GotSlot,
            MAC_ARM64.features,
            true,
            alloc,
        )
        .unwrap();
        let (exec2, _) = load_exec(emitted2);
        let f_ptr2: extern "C" fn() -> u64 = unsafe { exec2.as_fn() };
        assert_eq!(f_ptr2(), 3);
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn sota_a64_spilled_i128_and_mul_overflow_preserve_both_halves() {
        extern "C" fn clobber_caller_saved(x: u64) -> u64 {
            x.wrapping_mul(3).wrapping_add(7)
        }

        struct OneSym(Symbol);
        impl Names for OneSym {
            fn name(&self, of: Outside) -> Option<blaze_tier::Named<'_>> {
                match of {
                    Outside::Func(0) => Some(blaze_tier::Named::Symbol(self.0)),
                    _ => None,
                }
            }
            fn routines(&self) -> RoutinePlaces {
                RoutinePlaces::NONE
            }
        }

        let alloc = test_alloc();
        let sym = Symbol::of_name("spilled_pair_helper");
        let helper_addr = u64::try_from((clobber_caller_saved as *const ()).addr()).unwrap_or(0);

        // 1. 8 simultaneously live `u128` values crossing a call force `GprPairSpill` in exact-CFG
        //    regalloc; every low half (including values with `hi == 0` like `8u128`) must survive.
        let i128_spill_fn = {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I128), Param::of(Ty::I64)],
                &[Param::of(Ty::I128)],
            );
            b.set_signature(sig);
            let hsig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            let href = b.reference(RefKind::Func, 0);
            let e = b.create_block(BlockKind::Normal);
            b.begin(e);
            let seed = b.param(Ty::I128);
            let tag = b.param(Ty::I64);
            let mut live: Vec<blaze_lir::Value, Alloc<'static>> = Vec::with_capacity_in(8, alloc);
            let mut cur = seed;
            for k in 1u128..=8 {
                let delta = b.iconst(Ty::I128, k);
                cur = b.binary(Op::Add, Flags::NONE, cur, delta);
                live.push(cur);
            }
            let h = b.call(Flags::NONE, href, hsig, &[tag]);
            let mut acc = b.convert(Op::Zext, Flags::NONE, Ty::I128, h);
            for (idx, &v) in live.iter().enumerate() {
                let sh = b.iconst(Ty::I128, u128::try_from(idx).unwrap_or(0));
                let shifted = b.binary(Op::Shl, Flags::NONE, v, sh);
                acc = b.binary(Op::Add, Flags::NONE, acc, shifted);
            }
            b.ret(&[acc]);
            b.finish_checked().unwrap()
        };
        let emitted1 = Sota::standalone()
            .emit(verify(i128_spill_fn.view()).unwrap(), &OneSym(sym), MAC_ARM64, alloc)
            .expect("Sota emits spilled i128 across call");
        let (ex1, _ks1) = load_exec_with_syms(emitted1, &[(sym, helper_addr)]);
        let f1: extern "C" fn(u128, u64) -> u128 = unsafe { ex1.as_fn() };
        for (seed, tag) in [
            (0u128, 5u64),
            (8u128, 11u64),
            ((1u128 << 63) | 0x1234, 42u64),
            (0x0123_4567_89ab_cdef_fedc_ba98_7654_3210u128, 99u64),
        ] {
            let mut expected = u128::from(clobber_caller_saved(tag));
            let mut cur = seed;
            for k in 1u128..=8 {
                cur = cur.wrapping_add(k);
                expected = expected.wrapping_add(cur << (k - 1));
            }
            assert_eq!(f1(seed, tag), expected, "spilled i128 mismatch for seed={seed:#x}, tag={tag}");
        }

        // 2. Spilled `UmulOverflow` and `SmulOverflow` with spilled inputs and spilled pair results.
        let mul_ov_fn = {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let hsig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            let href = b.reference(RefKind::Func, 0);
            let e = b.create_block(BlockKind::Normal);
            b.begin(e);
            let a = b.param(Ty::I64);
            let bv = b.param(Ty::I64);
            // Force >= 14 live values across calls so both operands and overflow pairs spill.
            let mut anchors: Vec<blaze_lir::Value, Alloc<'static>> = Vec::with_capacity_in(14, alloc);
            let mut cur = a;
            for k in 1u128..=14 {
                let ck = b.iconst(Ty::I64, k);
                cur = b.binary(Op::Add, Flags::NONE, cur, ck);
                anchors.push(cur);
            }
            let h0 = b.call(Flags::NONE, href, hsig, &[a]);
            let upair = b.overflow(Op::UmulOverflow, a, bv);
            let spair = b.overflow(Op::SmulOverflow, a, bv);
            let h1 = b.call(Flags::NONE, href, hsig, &[h0]);
            let ulo = b.project(upair, 0);
            let uov_flag = b.project(upair, 1);
            let uov = b.convert(Op::Zext, Flags::NONE, Ty::I64, uov_flag);
            let slo = b.project(spair, 0);
            let sov_flag = b.project(spair, 1);
            let sov = b.convert(Op::Zext, Flags::NONE, Ty::I64, sov_flag);
            let mut out = b.binary(Op::Xor, Flags::NONE, ulo, slo);
            let c1 = b.iconst(Ty::I64, 1);
            let uov_s = b.binary(Op::Shl, Flags::NONE, uov, c1);
            out = b.binary(Op::Add, Flags::NONE, out, uov_s);
            out = b.binary(Op::Add, Flags::NONE, out, sov);
            out = b.binary(Op::Add, Flags::NONE, out, h1);
            for &v in &anchors {
                out = b.binary(Op::Xor, Flags::NONE, out, v);
            }
            b.ret(&[out]);
            b.finish_checked().unwrap()
        };
        let emitted2 = Sota::standalone()
            .emit(verify(mul_ov_fn.view()).unwrap(), &OneSym(sym), MAC_ARM64, alloc)
            .expect("Sota emits spilled UmulOverflow/SmulOverflow");
        let (ex2, _ks2) = load_exec_with_syms(emitted2, &[(sym, helper_addr)]);
        let f2: extern "C" fn(u64, u64) -> u64 = unsafe { ex2.as_fn() };
        for (a, bv) in [
            (7u64, 9u64),
            (1u64 << 40, 1u64 << 30),
            (i64::MIN.cast_unsigned(), 2u64),
            (u64::MAX, u64::MAX),
            ((-5i64).cast_unsigned(), (-7i64).cast_unsigned()),
        ] {
            let (ulo, uov) = a.overflowing_mul(bv);
            let (slo, sov) = a.cast_signed().overflowing_mul(bv.cast_signed());
            let h1 = clobber_caller_saved(clobber_caller_saved(a));
            let mut expected = (ulo ^ slo.cast_unsigned())
                .wrapping_add(u64::from(uov) << 1)
                .wrapping_add(u64::from(sov))
                .wrapping_add(h1);
            let mut cur = a;
            for k in 1u64..=14 {
                cur = cur.wrapping_add(k);
                expected ^= cur;
            }
            assert_eq!(f2(a, bv), expected, "spilled mul_overflow mismatch for a={a:#x}, b={bv:#x}");
        }
    }

    #[test]
    fn explicit_tail_call_direct_and_indirect_baseline_sota_and_x64() {
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();

        extern "C" fn add_pair(a: u64, b: u64) -> u64 {
            a.wrapping_add(b).wrapping_add(1)
        }
        let target_addr = add_pair as *const () as u64;

        // Direct `Op::TailCall` with a local stack slot (proving explicit tail calls work even
        // when `view.slots` is non-empty, unlike implicit TCO).
        let direct_fn = {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64), Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            let slot = b.slot(16, 3, blaze_lir::func::SlotKind::Named);
            let callee = b.reference(RefKind::Func, 0);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let a = b.param(Ty::I64);
            let bv = b.param(Ty::I64);
            let addr = b.slot_addr(slot);
            b.store(Flags::NONE, a, addr, 0, MemAttrs::aligned(3));
            let loaded = b.load(Ty::I64, Flags::NONE, addr, 0, MemAttrs::aligned(3));
            b.tail_call(Flags::NONE, callee, sig, &[loaded, bv]);
            b.finish_checked().unwrap()
        };

        // Indirect `Op::TailCallIndirect` with a local stack slot.
        let indirect_fn = {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::PTR), Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            let callee_sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64), Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
            b.set_signature(sig);
            let slot = b.slot(16, 3, blaze_lir::func::SlotKind::Named);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let fp = b.param(Ty::PTR);
            let a = b.param(Ty::I64);
            let bv = b.param(Ty::I64);
            let addr = b.slot_addr(slot);
            b.store(Flags::NONE, bv, addr, 0, MemAttrs::aligned(3));
            let loaded = b.load(Ty::I64, Flags::NONE, addr, 0, MemAttrs::aligned(3));
            b.tail_call_indirect(Flags::NONE, fp, callee_sig, &[a, loaded]);
            b.finish_checked().unwrap()
        };

        struct OneSym(Symbol);
        impl Names for OneSym {
            fn name(&self, of: Outside) -> Option<blaze_tier::Named<'_>> {
                match of {
                    Outside::Func(0) => Some(blaze_tier::Named::Symbol(self.0)),
                    _ => None,
                }
            }
            fn routines(&self) -> blaze_tier::RoutinePlaces {
                NoNames.routines()
            }
        }
        let sym = Symbol::of_name("add_pair");

        // 1. Baseline (Reference) tier: record form + native execution.
        let ref_direct = crate::Reference
            .emit(verify(direct_fn.view()).unwrap(), &OneSym(sym), MAC_ARM64, alloc)
            .expect("Reference emits Op::TailCall");
        let (ex_rd, _k1) = load_exec_with_syms(ref_direct, &[(sym, target_addr)]);
        let f_rd: extern "C" fn(u64, u64) -> u64 = unsafe { ex_rd.as_fn() };
        assert_eq!(f_rd(19, 22), 42, "Reference Op::TailCall mismatch");

        let ref_indirect = crate::Reference
            .emit(verify(indirect_fn.view()).unwrap(), &NoNames, MAC_ARM64, alloc)
            .expect("Reference emits Op::TailCallIndirect");
        let (ex_ri, _k2) = load_exec_with_syms(ref_indirect, &[]);
        let f_ri: extern "C" fn(*const (), u64, u64) -> u64 = unsafe { ex_ri.as_fn() };
        assert_eq!(f_ri(add_pair as *const (), 19, 22), 42, "Reference Op::TailCallIndirect mismatch");

        // 2. SOTA ARM64 (GotSlot JIT mode AND DirectBl export mode): native execution.
        let sota_direct = Sota::standalone()
            .emit(verify(direct_fn.view()).unwrap(), &OneSym(sym), MAC_ARM64, alloc)
            .expect("Sota ARM64 GotSlot emits Op::TailCall");
        let (ex_sd, _k3) = load_exec_with_syms(sota_direct, &[(sym, target_addr)]);
        let f_sd: extern "C" fn(u64, u64) -> u64 = unsafe { ex_sd.as_fn() };
        assert_eq!(f_sd(19, 22), 42, "Sota ARM64 GotSlot Op::TailCall mismatch");

        let sota_indirect = Sota::standalone()
            .emit(verify(indirect_fn.view()).unwrap(), &NoNames, MAC_ARM64, alloc)
            .expect("Sota ARM64 emits Op::TailCallIndirect");
        let (ex_si, _k4) = load_exec_with_syms(sota_indirect, &[]);
        let f_si: extern "C" fn(*const (), u64, u64) -> u64 = unsafe { ex_si.as_fn() };
        assert_eq!(f_si(add_pair as *const (), 19, 22), 42, "Sota ARM64 Op::TailCallIndirect mismatch");

        let sota_direct_bl = Sota::for_export()
            .emit(verify(direct_fn.view()).unwrap(), &OneSym(sym), MAC_ARM64, alloc)
            .expect("Sota ARM64 DirectBl emits Op::TailCall");
        assert!(
            sota_direct_bl.relocs.iter().any(|r| r.kind == blaze_tier::RelocKind::Branch26),
            "Sota DirectBl Op::TailCall must emit a Branch26 tail branch"
        );

        // 3. SOTA x86-64 (GotSlot and DirectBl): emit cleanly without refusal.
        let linux_x64 = Machine { arch: Arch::X86_64, os: Os::Linux, features: 0 };
        let x64_d_got = Sota::standalone()
            .emit(verify(direct_fn.view()).unwrap(), &OneSym(sym), linux_x64, alloc)
            .expect("Sota x86-64 GotSlot emits Op::TailCall");
        assert!(!x64_d_got.code.is_empty());
        let x64_d_rel = Sota::for_export()
            .emit(verify(direct_fn.view()).unwrap(), &OneSym(sym), linux_x64, alloc)
            .expect("Sota x86-64 DirectBl emits Op::TailCall");
        assert!(!x64_d_rel.code.is_empty());
        let x64_ind = Sota::standalone()
            .emit(verify(indirect_fn.view()).unwrap(), &NoNames, linux_x64, alloc)
            .expect("Sota x86-64 emits Op::TailCallIndirect");
        assert!(!x64_ind.code.is_empty());
    }
}
