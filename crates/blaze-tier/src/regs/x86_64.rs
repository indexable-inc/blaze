//! x86-64 under the System V AMD64 ABI (the psABI, "Function Calling Sequence": registers
//! `%rbx`, `%rsp`, `%rbp`, `%r12`..`%r15` belong to the caller; integer arguments in `%rdi`,
//! `%rsi`, `%rdx`, `%rcx`, `%r8`, `%r9`; float arguments in `%xmm0`..`%xmm7`; integer results in
//! `%rax`, `%rdx`; float results in `%xmm0`, `%xmm1`; every `%xmm` register is caller-saved).
//! Linux's and macOS's x86-64 conventions are the same for what the composer uses.
//!
//! Numbering per the parent module: a general register is its hardware number (the value the
//! ModRM, SIB and opcode fields hold with REX's extension bit as bit 3): rax 0, rcx 1, rdx 2,
//! rbx 3, rsp 4, rbp 5, rsi 6, rdi 7, r8..r15 8..15; `xmm0`..`xmm15` are 32..47.
//!
//! THE FRAME RECORD: the prologue is `push rbp; mov rbp, rsp`, so `[rbp]` holds the caller's
//! `rbp` and `[rbp + 8]` the return address: the same two words at the same offsets as arm64's
//! `stp x29, x30, [sp, #-16]!; mov x29, sp`, which is what the sampler's `[fp + 8]` read and the
//! unwinder's frame walk rely on. There is no link register: `call` pushes the return address.
//!
//! SAVES: x86-64 has no pair store; a "pair" is two pushes, one 16-byte step, so the stack stays
//! 16-byte aligned at every call when the frame below the saves is a multiple of 16 (on entry
//! `rsp` is 8 mod 16; the return address and the pushed `rbp` make it 0 mod 16). Five registers
//! are saved besides `rbp`, so the last pair is `(r15, r15)`: read as "save r15, then an 8-byte
//! pad", or naively as two pushes and two pops of r15, both give the same r15 and the same
//! 16-byte step (the naive reading is correct, never wrong).

use super::{Arch, Reg, RegDesc, RegSet};

pub const RAX: Reg = Reg(0);
pub const RCX: Reg = Reg(1);
pub const RDX: Reg = Reg(2);
pub const RBX: Reg = Reg(3);
pub const RSP: Reg = Reg(4);
pub const RBP: Reg = Reg(5);
pub const RSI: Reg = Reg(6);
pub const RDI: Reg = Reg(7);
pub const R8: Reg = Reg(8);
pub const R9: Reg = Reg(9);
pub const R10: Reg = Reg(10);
pub const R11: Reg = Reg(11);
pub const R12: Reg = Reg(12);
pub const R13: Reg = Reg(13);
pub const R14: Reg = Reg(14);
pub const R15: Reg = Reg(15);
/// `xmm0`; `xmm<n>` is `Reg(32 + n)`.
pub const XMM0: Reg = Reg(32);

const X86_64_ARGS_GPR: [Reg; 6] = [RDI, RSI, RDX, RCX, R8, R9];
const X86_64_ARGS_FPR: [Reg; 8] = [Reg(32), Reg(33), Reg(34), Reg(35), Reg(36), Reg(37), Reg(38), Reg(39)];
const X86_64_RETS_GPR: [Reg; 2] = [RAX, RDX];
const X86_64_RETS_FPR: [Reg; 2] = [Reg(32), Reg(33)];
const X86_64_SAVE_PAIRS: [(Reg, Reg); 3] = [(RBX, R12), (R13, R14), (R15, R15)];

/// x86-64 System V, the composer's view.
///
/// Allocatable: `rbx`, `r12`..`r15` (callee-saved) and `xmm8`..`xmm15` (caller-saved, no
/// argument or result role). Scratch: `r10` and `r11`, the composer's pair (never allocated, never
/// live across a template; `r11` is also what a PLT-style stub may clobber, so nothing of ours
/// lives there across a call anyway). Never in any set: `rsp` (the stack), `rbp` (the frame
/// record; in `callee_saved` only, as arm64's x29 is). No register is reserved for a run context
/// (D100 point 3).
///
/// NOT ALLOCATABLE, AGAINST THE BRIEF (WAVE-BACKEND.md piece X asks for `rsi`, `rdi`, `r8`..`r9`
/// too): piece 0's [`RegDesc::allocatable`] says it "never holds ... an argument register", and
/// the composer is written against that (arguments are only moved into at calls and returns, as
/// arm64's x0..x8 are). The four would raise the general count from 5 to 9; the mask with them
/// is `0x0000_ff00_0000_f3c8`, a one-line change once the composer treats argument registers as
/// clobbered by its call sequence's parallel move and piece 0's documentation says so.
pub const X86_64_SYSV: RegDesc = RegDesc {
    arch: Arch::X86_64,
    // rbx (bit 3), r12..r15 (bits 12..15), xmm8..xmm15 (bits 40..47).
    allocatable: RegSet(0x0000_ff00_0000_f008),
    // rbx (3), rbp (5), r12..r15 (12..15); no xmm register is callee-saved in System V.
    callee_saved: RegSet(0x0000_0000_0000_f028),
    // r10 (10), r11 (11).
    scratch: RegSet(0x0000_0000_0000_0c00),
    run_context: None,
    args_gpr: &X86_64_ARGS_GPR,
    args_fpr: &X86_64_ARGS_FPR,
    rets_gpr: &X86_64_RETS_GPR,
    rets_fpr: &X86_64_RETS_FPR,
    stack: RSP,
    frame: RBP,
    link: None,
    save_pairs: &X86_64_SAVE_PAIRS,
};

#[cfg(test)]
mod tests {
    use super::{RBP, RSP, X86_64_SYSV};
    use crate::{Arch, Reg, RegClass, RegSet};

    /// The x86-64 description's sets do not overlap where they must not, and each mask holds
    /// exactly the registers its comment names (each bit is named here, independently of the
    /// masks). Expected: passes. Fails if a mask is mistyped, or if an argument, result, scratch,
    /// stack or frame register becomes allocatable.
    #[test]
    fn x86_64_sets_are_disjoint() {
        let d = X86_64_SYSV;
        assert!(d.arch == Arch::X86_64);
        assert!(d.allocatable.0 & d.scratch.0 == 0, "allocatable and scratch overlap");
        // Not allocatable: rax, rcx, rdx, rsp, rbp, rsi, rdi, r8, r9, r10, r11, xmm0..xmm7, and
        // nothing past xmm15.
        for reg in [0u8, 1, 2, 4, 5, 6, 7, 8, 9, 10, 11, 32, 33, 34, 35, 36, 37, 38, 39, 48, 63] {
            assert!(!d.allocatable.contains(Reg(reg)), "register {reg} must not be allocatable");
        }
        for reg in [3u8, 12, 13, 14, 15, 40, 41, 47] {
            assert!(d.allocatable.contains(Reg(reg)), "register {reg} must be allocatable");
        }
        for &reg in d.args_gpr.iter().chain(d.args_fpr).chain(d.rets_gpr).chain(d.rets_fpr) {
            assert!(!d.allocatable.contains(reg), "argument or result {reg:?} is allocatable");
            assert!(!d.scratch.contains(reg), "argument or result {reg:?} is scratch");
        }
        assert!(d.scratch == RegSet::EMPTY.with(Reg(10)).with(Reg(11)));
        assert!(d.callee_saved == RegSet::EMPTY.with(Reg(3)).with(Reg(5)).with(Reg(12)).with(Reg(13)).with(Reg(14)).with(Reg(15)));
        assert!(d.callee_saved.of(RegClass::Fpr).is_empty(), "System V saves no xmm register");
        assert!(d.run_context.is_none(), "D100 point 3: no reserved register");
        assert!(d.link.is_none(), "x86-64 returns through the stack");
        assert!(d.stack == RSP && d.frame == RBP);
        assert!(!d.scratch.contains(RSP) && !d.scratch.contains(RBP));
        assert!(!d.callee_saved.contains(RSP), "rsp is restored by the frame, not saved");
        let mut saved = RegSet::EMPTY;
        for &(a, b) in d.save_pairs {
            assert!(d.callee_saved.contains(a) && d.callee_saved.contains(b), "pair {a:?} {b:?}");
            assert!(a != RBP && b != RBP, "rbp is saved by the frame record, not a pair");
            saved = saved.with(a).with(b);
        }
        assert!(saved == d.callee_saved.without(RBP), "the pairs save every callee-saved register but rbp");
        assert_eq!(d.args_gpr, &[Reg(7), Reg(6), Reg(2), Reg(1), Reg(8), Reg(9)]);
        assert_eq!(d.rets_gpr, &[Reg(0), Reg(2)]);
        assert_eq!(d.args_fpr.len(), 8);
        assert_eq!(d.rets_fpr, &[Reg(32), Reg(33)]);
        assert_eq!(d.allocatable.of(RegClass::Gpr).len(), 5);
        assert_eq!(d.allocatable.of(RegClass::Fpr).len(), 8);
    }
}
