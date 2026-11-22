//! x0-intr-01-isb: the first target intrinsic on every program's path. The pinned library's
//! `core::hint::spin_loop` (library/core/src/hint.rs) on aarch64 is
//! `unsafe { crate::arch::aarch64::__isb(crate::arch::aarch64::SY) }`, and `__isb` calls
//! `isb(15)`, declared (library/stdarch/crates/core_arch/src/arm_shared/barrier/mod.rs):
//! `unsafe extern "llvm-intrinsic" { #[cfg_attr(any(target_arch = "aarch64", target_arch =
//! "arm64ec"), link_name = "llvm.aarch64.isb")] #[cfg_attr(target_arch = "arm", link_name =
//! "llvm.arm.isb")] fn isb(_: i32); }`.
//!
//! The rule: library/core/src/hint.rs, `spin_loop`'s doc: "Emits a machine instruction to
//! signal the processor that it is running in a busy-wait spin-loop", and "`spin_loop` does
//! not interact with the operating system": the signal changes no value and no output of the
//! program, so the right answer is that the program exits 0 with nothing printed.
//! A front end that reads the block's ABI as no ABI refuses `isb` ("ABI Rust: neither Rust nor
//! C"); one that links `isb` as a C symbol fails to link `llvm.aarch64.isb`; one that drops
//! the call is not seen here (the instruction has no observable effect), which is why the
//! builder's own test prints the `target_intrinsic "llvm.aarch64.isb"` op.
//! Until the builder builds target intrinsics (blaze-build `Intrinsic::Target`) and the back
//! end prints them, a run stops naming "a target intrinsic".

fn main() {
    for _ in 0..3 {
        std::hint::spin_loop();
    }
    std::process::exit(0);
}
