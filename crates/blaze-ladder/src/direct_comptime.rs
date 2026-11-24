//! Direct LIR Native Execution for Comptime (Tier 0 — Zero Wasm, Zero Subprocess, Zero `libc`).
//!
//! Compiles a pure Blaze LIR function (`blaze_lir::View`) straight through Blaze's own native
//! optimizing tier (`blaze_emit::Sota`: SROA + Aegraph + Strength Reduction + LICM + DCE
//! + frameless AArch64 codegen), seals and places it with `blaze_tier::record::{seal, place}`
//! into Blaze's own dual-mapped W^X [`blaze_inproc::exec::Executable`] (never RWX, never raw
//! `libc::mmap`, never `MAP_JIT`), and executes it directly in-process at native CPU speed.
//!
//! Together with `bstd::procs::hermetic` (Tier 1: macOS Seatbelt / Linux Landlock + seccomp for
//! heavy ecosystem proc macros & C build scripts), this eliminates WebAssembly completely from
//! Blaze's long-term compile-time execution architecture.

use std::time::{Duration, Instant};

use blaze_code::image::ImageError;
use blaze_code::place::{At, PlaceError, Words};
use blaze_code::record::{Symbol, Target};
use blaze_emit::Sota;
use blaze_inproc::exec::Executable;
use blaze_lir::{func::View, verify};
use blaze_own::{AVec, Alloc, Vec};
use blaze_tier::{
    Arch, Machine, Names, Os, Outside, RoutinePlaces, Tier,
    record::{encode, place, read, seal},
};

/// Result of compiling and directly executing a pure LIR comptime function natively.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirectExecResult {
    /// Raw 64-bit return value in `x0`.
    pub value: u64,
    /// Native code size emitted by `Sota` (bytes).
    pub code_bytes: usize,
    /// Wall time spent optimizing + emitting + placing native machine code.
    pub compile_time: Duration,
    /// Wall time spent executing the native machine code.
    pub exec_time: Duration,
}

/// Errors from direct native LIR comptime execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DirectExecError {
    /// Current host architecture does not yet support direct in-process `Sota` execution.
    UnsupportedHost,
    /// The LIR function uses features outside the direct pure-comptime leaf/loop subset
    /// (e.g., external symbols / syscalls — route to Tier 1 `bstd::procs::hermetic` instead).
    RequiresTier1(String),
    /// Blaze's W^X `Executable` allocator or placer failed to seal the code page.
    ExecSealFailed(String),
}

impl core::fmt::Display for DirectExecError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnsupportedHost => write!(f, "direct LIR JIT comptime requires an aarch64 host"),
            Self::RequiresTier1(why) => write!(f, "LIR function requires Tier 1 hermetic execution: {why}"),
            Self::ExecSealFailed(why) => write!(f, "blaze-inproc Executable failed: {why}"),
        }
    }
}

impl std::error::Error for DirectExecError {}

struct PureNoNames;
impl Names for PureNoNames {
    fn name(&self, _of: Outside) -> Option<blaze_tier::Named<'_>> {
        None
    }

    fn routines(&self) -> RoutinePlaces {
        RoutinePlaces::NONE
    }
}

struct PureLeafWords;
impl Words for PureLeafWords {
    fn word(&mut self, target: Target, _call: bool) -> Result<u64, ImageError> {
        Err(ImageError::Unbound { target })
    }

    fn value(&mut self, target: Target) -> Result<u64, ImageError> {
        let _ = target;
        Err(ImageError::Unbound { target })
    }
}

/// A compiled, executable native LIR comptime kernel backed by [`blaze_inproc::exec::Executable`]
/// (strict W^X sealed RX memory, zero `libc`).
#[cfg(all(target_arch = "aarch64", unix))]
pub struct CompiledComptime {
    exec: Executable,
    code_bytes: usize,
    compile_time: Duration,
}

#[cfg(all(target_arch = "aarch64", unix))]
impl CompiledComptime {
    /// Compiles and places a pure `blaze_lir::View` through `Sota` into a sealed W^X [`Executable`].
    pub fn compile(view: View<'_>) -> Result<Self, DirectExecError> {
        let start = Instant::now();
        let alloc = Alloc::global();
        let os = if cfg!(target_os = "macos") { Os::MacOs } else { Os::Linux };
        let machine = Machine { arch: Arch::Arm64, os, features: Machine::LSE };
        let verified = verify(view)
            .map_err(|e| DirectExecError::RequiresTier1(format!("LIR verify failed: {e:?}")))?;
        let emitted = Sota::standalone()
            .emit(verified, &PureNoNames, machine, alloc)
            .map_err(|e| DirectExecError::RequiresTier1(format!("{e:?}")))?;

        let sealed = seal(emitted)
            .map_err(|e| DirectExecError::RequiresTier1(format!("seal failed: {e:?}")))?;
        let mut bytes: AVec<'static, u8> = Vec::new_in(alloc);
        encode(&sealed, &mut bytes);
        let rec_view = read(&bytes)
            .map_err(|e| DirectExecError::ExecSealFailed(format!("record read failed: {e:?}")))?;
        let code_bytes = rec_view.code().len();
        let relro_len = rec_view.relro().len();
        let align = 1usize << rec_view.relro_align();
        let sym = Symbol::of_name("blaze direct comptime");

        let mut place_res: Result<(), PlaceError> = Ok(());
        let exec = Executable::written(code_bytes, relro_len + align, |code, data| {
            let skip = (align - data.as_ptr() as usize % align) % align;
            let relro = &mut data[skip..skip + relro_len];
            let at = At { code: code.as_ptr() as u64, relro: relro.as_ptr() as u64 };
            let mut words = PureLeafWords;
            place_res = place(&rec_view, sym, at, &mut words, code, relro);
        })
        .map_err(|e| DirectExecError::ExecSealFailed(format!("{e:?}")))?;

        place_res.map_err(|e| DirectExecError::RequiresTier1(format!("place failed: {e:?}")))?;

        Ok(Self {
            exec,
            code_bytes,
            compile_time: start.elapsed(),
        })
    }

    pub fn code_bytes(&self) -> usize {
        self.code_bytes
    }

    pub fn compile_time(&self) -> Duration {
        self.compile_time
    }

    /// Invokes a 0-argument `() -> u64` pure comptime LIR function.
    pub fn call0(&self) -> u64 {
        // SAFETY: `self.exec` holds sealed AArch64 code verified by `Sota::standalone()`.
        let f: extern "C" fn() -> u64 = unsafe { self.exec.as_fn() };
        f()
    }

    /// Invokes a 1-argument `(u64) -> u64` pure comptime LIR function.
    pub fn call1(&self, a0: u64) -> u64 {
        // SAFETY: `self.exec` holds sealed AArch64 code verified by `Sota::standalone()`.
        let f: extern "C" fn(u64) -> u64 = unsafe { self.exec.as_fn() };
        f(a0)
    }

    /// Invokes a 2-argument `(u64, u64) -> u64` pure comptime LIR function.
    pub fn call2(&self, a0: u64, a1: u64) -> u64 {
        // SAFETY: `self.exec` holds sealed AArch64 code verified by `Sota::standalone()`.
        let f: extern "C" fn(u64, u64) -> u64 = unsafe { self.exec.as_fn() };
        f(a0, a1)
    }
}

/// Compiles and executes a 1-argument pure LIR comptime kernel in one shot (zero Wasm, zero `libc`!).
pub fn exec_lir_u64_1(view: View<'_>, arg0: u64) -> Result<DirectExecResult, DirectExecError> {
    #[cfg(all(target_arch = "aarch64", unix))]
    {
        let compiled = CompiledComptime::compile(view)?;
        let exec_start = Instant::now();
        let value = compiled.call1(arg0);
        let exec_time = exec_start.elapsed();
        Ok(DirectExecResult {
            value,
            code_bytes: compiled.code_bytes(),
            compile_time: compiled.compile_time(),
            exec_time,
        })
    }
    #[cfg(not(all(target_arch = "aarch64", unix)))]
    {
        let _ = (view, arg0);
        Err(DirectExecError::UnsupportedHost)
    }
}

#[cfg(test)]
mod tests {
    use std::alloc::Global;

    use super::*;
    use blaze_lir::{
        Flags, IntCond, Op, Ty,
        builder::{Builder, Full, Param, Variadic},
        func::{BlockKind, Conv},
    };

    #[test]
    #[cfg(all(target_arch = "aarch64", unix))]
    fn direct_lir_comptime_executes_loop_natively_without_wasm() {
        // Pure comptime kernel: sum(i * 8 + (n * 3)) for i in 0..n
        // Exercises SROA + Aegraph strength reduction (`* 8 -> shl 3`) + LICM (`n * 3` hoisted!)
        // + frameless AArch64 execution through `blaze_inproc::exec::Executable` (zero Wasm, zero libc!).
        let mut b = Builder::<Global, Full>::new_in(64, Global);
        let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I64)]);
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        let hdr = b.create_block(BlockKind::Normal);
        let body = b.create_block(BlockKind::Normal);
        let exit = b.create_block(BlockKind::Normal);

        b.begin(entry);
        let n = b.param(Ty::I64);
        let z = b.iconst(Ty::I64, 0);
        b.jump((hdr, &[z, z]));

        b.begin(hdr);
        let i = b.param(Ty::I64);
        let acc = b.param(Ty::I64);
        let cond = b.icmp(IntCond::Ult, i, n);
        b.br(Flags::NONE, cond, (body, &[]), (exit, &[acc]));

        b.begin(body);
        let c8 = b.iconst(Ty::I64, 8);
        let c3 = b.iconst(Ty::I64, 3);
        let one = b.iconst(Ty::I64, 1);
        let scaled = b.binary(Op::Mul, Flags::NONE, i, c8);
        let inv = b.binary(Op::Mul, Flags::NONE, n, c3);
        let term = b.binary(Op::Add, Flags::NONE, scaled, inv);
        let next_acc = b.binary(Op::Add, Flags::NONE, acc, term);
        let next_i = b.binary(Op::Add, Flags::NONE, i, one);
        b.jump((hdr, &[next_i, next_acc]));

        b.begin(exit);
        let ret_val = b.param(Ty::I64);
        b.ret(&[ret_val]);

        let func = b.finish_checked().expect("valid LIR");
        let view = func.view();

        let n_val: u64 = 10_000;
        let expected: u64 = (0..n_val).map(|k| k * 8 + n_val * 3).sum();
        let res = exec_lir_u64_1(view, n_val).expect("direct LIR Sota execution");
        assert_eq!(res.value, expected);
        assert!(
            res.code_bytes <= 256,
            "expected compact frameless AArch64 (scalar or vectorized/unrolled), got {} B",
            res.code_bytes
        );
        eprintln!(
            "direct LIR Sota comptime (10k iters, zero Wasm, zero libc): value={} code={}B compile={:?} exec={:?}",
            res.value, res.code_bytes, res.compile_time, res.exec_time
        );
    }
}
