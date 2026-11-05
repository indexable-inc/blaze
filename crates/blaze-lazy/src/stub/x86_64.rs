//! The x86-64 resolver and entries, as bytes.
//!
//! The resolver's frame, from the caller's stack pointer `s` at the entry (`s` points at the
//! return address):
//!
//! | at | what |
//! |---|---|
//! | `s - 8` | the id the entry pushed; the target once the hook returns |
//! | `s - 16` (`rbp`) | the caller's `rbp` |
//! | `rbp - 64 ..` | `rax`, `rcx`, `rdx`, `rsi`, `rdi`, `r8`, `r9`, `r10` |
//! | below, 64-aligned | the `XSAVE` (or `FXSAVE`) area |

use super::{Abi, CELL, ENTRY, PROBE, RESOLVER, StubError, X86Save};

/// `endbr64; push imm32 (id); jmp rel32 (resolver); int3; int3`: an indirect jump lands on
/// `endbr64` where CET's indirect branch tracking is on.
pub(super) const ENTRY_TEMPLATE: [u8; ENTRY as usize] =
    [0xf3, 0x0f, 0x1e, 0xfa, 0x68, 0, 0, 0, 0, 0xe9, 0, 0, 0, 0, 0xcc, 0xcc];

/// Fills the id and the branch of the entry at `at`.
pub(super) fn fill_entry(entry: &mut [u8], at: u32, id: u32) {
    entry[5..9].copy_from_slice(&id.to_le_bytes());
    let rel = RESOLVER as i64 - i64::from(at + 14);
    entry[10..14].copy_from_slice(&(rel as i32).to_le_bytes());
}

/// The registers saved by push, in order; popped in reverse.
const PUSHED: [&[u8]; 8] = [
    &[0x50],       // rax
    &[0x51],       // rcx
    &[0x52],       // rdx
    &[0x56],       // rsi
    &[0x57],       // rdi
    &[0x41, 0x50], // r8
    &[0x41, 0x51], // r9
    &[0x41, 0x52], // r10
];
const POPPED: [&[u8]; 8] = [
    &[0x41, 0x5a], // r10
    &[0x41, 0x59], // r9
    &[0x41, 0x58], // r8
    &[0x5f],       // rdi
    &[0x5e],       // rsi
    &[0x5a],       // rdx
    &[0x59],       // rcx
    &[0x58],       // rax
];

/// The XSAVE header, zeroed before a save: `XSAVE` leaves the bits of components outside its
/// mask as they were, and `XRSTOR` faults on reserved bits.
const HEADER: u32 = 512;

/// Appends the resolver at `out.len()`, which is [`RESOLVER`].
pub(super) fn resolver(out: &mut Vec<u8>, abi: Abi, save: X86Save) -> Result<(), StubError> {
    debug_assert_eq!(out.len(), RESOLVER as usize);
    let size = save.size();
    if size > 1 << 20 {
        return Err(StubError::Save(size));
    }
    out.push(0x55); // push rbp
    out.extend_from_slice(&[0x48, 0x89, 0xe5]); // mov rbp, rsp
    for push in PUSHED {
        out.extend_from_slice(push);
    }
    out.extend_from_slice(&[0x48, 0x83, 0xe4, 0xc0]); // and rsp, -64
    // sub rsp, size, at most PROBE at a time, touching each step: the guard page is never
    // stepped over.
    let mut left = size;
    while left > 0 {
        let step = left.min(PROBE);
        out.extend_from_slice(&[0x48, 0x81, 0xec]); // sub rsp, imm32
        out.extend_from_slice(&step.to_le_bytes());
        out.extend_from_slice(&[0x48, 0xc7, 0x04, 0x24, 0, 0, 0, 0]); // mov qword [rsp], 0
        left -= step;
    }
    match save {
        X86Save::Fx => out.extend_from_slice(&[0x48, 0x0f, 0xae, 0x04, 0x24]), // fxsave64 [rsp]
        X86Save::Xsave { mask, .. } | X86Save::Xsavec { mask, .. } => {
            for word in 0..8u32 {
                // mov qword [rsp + disp32], 0
                out.extend_from_slice(&[0x48, 0xc7, 0x84, 0x24]);
                out.extend_from_slice(&(HEADER + 8 * word).to_le_bytes());
                out.extend_from_slice(&[0, 0, 0, 0]);
            }
            mask_registers(out, mask);
            match save {
                X86Save::Xsavec { .. } => out.extend_from_slice(&[0x48, 0x0f, 0xc7, 0x24, 0x24]), // xsavec64 [rsp]
                _ => out.extend_from_slice(&[0x48, 0x0f, 0xae, 0x24, 0x24]), // xsave64 [rsp]
            }
        }
    }
    match abi {
        Abi::SysV => out.extend_from_slice(&[0x8b, 0x7d, 0x08]), // mov edi, [rbp + 8]
        Abi::Win64 => {
            out.extend_from_slice(&[0x8b, 0x4d, 0x08]); // mov ecx, [rbp + 8]
            out.extend_from_slice(&[0x48, 0x83, 0xec, 0x20]); // sub rsp, 32
        }
    }
    // mov rax, [rip + cell]
    out.extend_from_slice(&[0x48, 0x8b, 0x05]);
    let next = out.len() as i64 + 4;
    out.extend_from_slice(&((i64::from(CELL) - next) as i32).to_le_bytes());
    out.extend_from_slice(&[0xff, 0xd0]); // call rax
    out.extend_from_slice(&[0x48, 0x85, 0xc0]); // test rax, rax
    out.extend_from_slice(&[0x74, 0]); // jz trap
    let jz = out.len();
    out.extend_from_slice(&[0x48, 0x89, 0x45, 0x08]); // mov [rbp + 8], rax
    if abi == Abi::Win64 {
        out.extend_from_slice(&[0x48, 0x83, 0xc4, 0x20]); // add rsp, 32
    }
    match save {
        X86Save::Fx => out.extend_from_slice(&[0x48, 0x0f, 0xae, 0x0c, 0x24]), // fxrstor64 [rsp]
        X86Save::Xsave { mask, .. } | X86Save::Xsavec { mask, .. } => {
            mask_registers(out, mask);
            // xrstor64 [rsp]: reads either format, by XCOMP_BV's top bit.
            out.extend_from_slice(&[0x48, 0x0f, 0xae, 0x2c, 0x24]);
        }
    }
    out.extend_from_slice(&[0x48, 0x8d, 0x65, 0xc0]); // lea rsp, [rbp - 64]
    for pop in POPPED {
        out.extend_from_slice(pop);
    }
    out.push(0x5d); // pop rbp
    out.extend_from_slice(&[0x41, 0x5b]); // pop r11: the target
    out.extend_from_slice(&[0x41, 0xff, 0xe3]); // jmp r11
    let trap = out.len();
    out.extend_from_slice(&[0x0f, 0x0b]); // ud2
    out[jz - 1] = u8::try_from(trap - jz).expect("the resolver's tail is short");
    Ok(())
}

/// `mov eax, low; mov edx, high`: the requested-feature bitmap of `XSAVE` and `XRSTOR`.
fn mask_registers(out: &mut Vec<u8>, mask: u64) {
    out.push(0xb8);
    out.extend_from_slice(&(mask as u32).to_le_bytes());
    out.push(0xba);
    out.extend_from_slice(&((mask >> 32) as u32).to_le_bytes());
}
