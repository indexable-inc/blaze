//! `Bcc`, `BRA`, `BSR` and `DBcc`.
//!
//! A branch is `0110 cccc dddddddd`: the low byte of the opcode word is an
//! 8-bit displacement from the address of the next word. Two of its values are
//! not displacements at all but escape codes:
//!
//! - `$00` means a 16-bit displacement follows. So a short branch to the very
//!   next instruction cannot exist — its displacement would be 0 — and GNU as
//!   assembles `bra next` as `6000 0002`.
//! - `$FF` means, on the 68020 and later, that a 32-bit displacement follows.
//!   A short displacement of -1 would land on an odd address anyway.
//!
//! Layout picks among variants by range alone, and a range cannot have a hole
//! in the middle. So the short form is offered twice, once for backward
//! branches (`-128..=-2`) and once for forward ones (`1..=127`), each with a
//! second, *guard* fixup on the same byte. The guard writes nothing; it only
//! exists so that its range, shifted by its `adjust`, cuts off the escape
//! codes. Layout moves past a variant as soon as any of its fixups does not
//! fit, which is exactly the check needed.
//!
//! The guard has two costs, both confined to a branch written `.s`, which
//! has no longer form to fall back on. Its diagnostic can only be the core's
//! range message about the guard's own shifted value, so `bra.s` to the next
//! instruction is rejected — as GNU as and vasm both reject it — but with an
//! unhelpful number in the message. And against an undefined symbol the guard
//! has no relocation to become, so `bra.s ext` is an error where GNU as emits
//! `R_68K_PC8`; an 8-bit reach into another object file is rarely wanted.
//!
//! Which forms an unsized branch may take depends on the syntax. Motorola
//! assemblers relax `bra` to whatever reaches. GNU as does that only for its
//! `jbsr`/`jra`/`jbCC` pseudo-ops and keeps a plain `bra` at 16 bits
//! (checked: `bra` to a label two bytes back is `6000 fffc` there, but
//! `60fc` under `--mri`).

use super::Cpu;
use super::reloc;
use crate::expr::ExprRef;
use crate::section::{Fixup, FixupKind, Variant};
use crate::source::Span;

/// The size a branch was written with.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum BranchSize {
    /// `.s` or `.b`.
    Short,
    Word,
    /// `.l`: 32-bit, 68020 and later.
    Long,
    /// No size: relax to whatever reaches.
    Relax,
}

/// Leaves the instruction untouched. A guard fixup is only a range check.
fn keep(word: u64, _value: i64) -> u64 {
    word
}

fn fixup(offset: u32, expr: ExprRef, kind: FixupKind, span: Span) -> Fixup {
    Fixup {
        offset,
        expr,
        kind,
        span,
    }
}

/// The two short variants: backward, then forward.
fn short(op: u8, target: ExprRef, span: Span) -> [Variant; 2] {
    // The real field: one byte, measured from the end of the opcode word,
    // which is one byte past the field.
    let real = FixupKind::pcrel(1, 1).with_reloc(reloc::R_68K_PC8);
    // The guard sits on the same byte, so its value is `disp + 1 - adjust`.
    //
    // Backward: `disp + 65` must fit a signed 7-bit field, `-64..=63`, which
    // allows `disp` from -129 to -2. The real field cuts off -129.
    let back = FixupKind::pcrel(1, -64).with_field(7, 1).scatter(keep);
    // Forward: `disp - 65` must fit an unsigned 7-bit field, `-64..=127`,
    // which allows `disp` from 1 to 192. The real field cuts off 128 and up.
    let mut fwd = FixupKind::pcrel(1, 66).with_field(7, 1).scatter(keep);
    fwd.signed = false;
    let make = |guard: FixupKind| Variant {
        bytes: vec![op, 0],
        fixups: vec![fixup(1, target, real, span), fixup(1, target, guard, span)],
    };
    [make(back), make(fwd)]
}

fn word(op: u8, target: ExprRef, span: Span) -> Variant {
    Variant {
        bytes: vec![op, 0, 0, 0],
        fixups: vec![fixup(
            2,
            target,
            FixupKind::pcrel(2, 0).with_reloc(reloc::R_68K_PC16),
            span,
        )],
    }
}

fn long(op: u8, target: ExprRef, span: Span) -> Variant {
    Variant {
        bytes: vec![op, 0xff, 0, 0, 0, 0],
        fixups: vec![fixup(
            2,
            target,
            FixupKind::pcrel(4, 0).with_reloc(reloc::R_68K_PC32),
            span,
        )],
    }
}

/// What a 68000 does when 16 bits do not reach: an absolute `jmp` or `jsr`,
/// behind a short branch on the opposite condition for `Bcc`. This is the
/// sequence GNU as `-m68000` emits.
fn far(cond: u8, target: ExprRef, span: Span) -> Variant {
    let abs = FixupKind::data(4).with_reloc(reloc::R_68K_32);
    match cond {
        0 | 1 => Variant {
            bytes: vec![0x4e, if cond == 0 { 0xf9 } else { 0xb9 }, 0, 0, 0, 0],
            fixups: vec![fixup(2, target, abs, span)],
        },
        _ => Variant {
            bytes: vec![0x60 | (cond ^ 1), 0x06, 0x4e, 0xf9, 0, 0, 0, 0],
            fixups: vec![fixup(4, target, abs, span)],
        },
    }
}

/// `Bcc`, `BRA` (`cond` 0) or `BSR` (`cond` 1). `constant` says the target
/// is a number rather than a label.
pub(crate) fn bcc(
    cond: u8,
    size: BranchSize,
    cpu: Cpu,
    target: ExprRef,
    constant: bool,
    span: Span,
) -> Vec<Variant> {
    let op = 0x60 | cond;
    match size {
        // A number is no distance relaxation can settle, and GNU as jumps to
        // it absolutely: `jra 0x100` is `jmp 0x100`.
        BranchSize::Relax if constant => vec![far(cond, target, span)],
        BranchSize::Short => short(op, target, span).to_vec(),
        BranchSize::Word => vec![word(op, target, span)],
        BranchSize::Long => vec![long(op, target, span)],
        BranchSize::Relax => {
            let mut v = short(op, target, span).to_vec();
            v.push(word(op, target, span));
            v.push(if cpu.long_branch_for(cond) {
                long(op, target, span)
            } else {
                far(cond, target, span)
            });
            v
        }
    }
}

/// `DBcc`: a 16-bit displacement, measured from the displacement word.
///
/// A label out of that reach, or in another section, gets what GNU as
/// emulates it with: the `DBcc` skips over a short branch around a long one,
///
/// ```text
///     dbcc  dn,1f       ; 5xc8 0004
///     bra.s 2f          ; 6006
/// 1:  bra.l target      ; 60ff xxxx xxxx, or jmp target (4ef9) on a 68000
/// 2:
/// ```
///
/// A number is the displacement's word alone, since no relaxation can place
/// it. GNU as writes that word as zero and emits no relocation for it at all,
/// leaving a branch to the next word; rsasm relocates it.
pub(crate) fn dbcc(
    cond: u8,
    reg: u8,
    cpu: Cpu,
    target: ExprRef,
    constant: bool,
    span: Span,
) -> Vec<Variant> {
    let w = 0x50c8 | (cond as u16) << 8 | reg as u16;
    let mut v = word(0, target, span);
    v.bytes[..2].copy_from_slice(&w.to_be_bytes());
    if constant {
        return vec![v];
    }
    let mut bytes = w.to_be_bytes().to_vec();
    bytes.extend_from_slice(&[0x00, 0x04, 0x60, 0x06]);
    let kind = if cpu.long_branch() {
        bytes.extend_from_slice(&[0x60, 0xff]);
        FixupKind::pcrel(4, 0).with_reloc(reloc::R_68K_PC32)
    } else {
        bytes.extend_from_slice(&[0x4e, 0xf9]);
        FixupKind::data(4).with_reloc(reloc::R_68K_32)
    };
    bytes.extend_from_slice(&[0; 4]);
    let long = Variant {
        bytes,
        fixups: vec![fixup(8, target, kind, span)],
    };
    vec![v, long]
}
