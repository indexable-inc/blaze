//! Branches, and how GNU as relaxes them.
//!
//! RX branch displacements are measured from the first byte of the branch
//! instruction, not from its end, so a field one byte into an instruction has
//! `adjust = -1`.
//!
//! Each branch offers every size GNU as's relaxation can pick, smallest first:
//!
//! | branch       | sizes                                                  |
//! |--------------|--------------------------------------------------------|
//! | `bra`        | `.s` (1), `.b` (2), `.w` (3), `.a` (4)                 |
//! | `bsr`        | `.w` (3), `.a` (4)                                     |
//! | `beq`, `bne` | `.s` (1), `.b` (2), `.w` (3), inverted `.s` + `bra.a` (5) |
//! | other `bCnd` | `.b` (2), inverted `.b` + `bra.w` (5), inverted `.b` + `bra.a` (6) |
//!
//! The pairs are GNU as's own invention, not instructions: RX has no 16- or
//! 24-bit conditional branch other than `beq.w`/`bne.w`, so a far `bgt`
//! becomes `ble .+5; bra.w target`. The `.s` forms reach 3 to 10 bytes
//! forward only.
//!
//! One difference remains for a target that is not resolved in this file.
//! GNU as then gives `bra`/`bsr` their 24-bit form — as layout does, since it
//! takes the widest candidate — but keeps `beq`/`bne` at 16 bits and other
//! conditions at 8, trusting the linker to reach. Here those get the synthetic
//! pair instead: larger, but correct at any distance.

use super::encode::Enc;
use super::reloc;
use crate::arch::AsmCtx;
use crate::expr::ExprRef;
use crate::section::{FixupKind, Variant};
use crate::source::Span;

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Kind {
    Bra,
    Bsr,
    /// `bCnd` with its condition code; 0 is `beq` and 1 is `bne`.
    Cond(u8),
}

/// The size a branch was written with.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Width {
    Auto,
    S,
    B,
    W,
    A,
}

impl Width {
    pub fn from_suffix(s: &str) -> Option<Width> {
        Some(match s {
            ".s" => Width::S,
            ".b" => Width::B,
            ".w" => Width::W,
            ".a" => Width::A,
            _ => return None,
        })
    }
}

/// Writes a 3-bit short displacement into the opcode byte.
///
/// The field holds 3 to 10, with 8, 9 and 10 wrapping to 0, 1 and 2. The
/// fixup is resolved seven bytes past the instruction so that range becomes
/// the signed 3-bit -4..=3 a fixup can check; this puts the seven back.
fn disp3(word: u64, v: i64) -> u64 {
    (word & !7) | (((v + 7) as u64) & 7)
}

// Relocation addends.
//
// Both RX linkers' PC-relative relocations measure from the instruction, not
// from the relocated field: for the 8/16/24-bit types the linker adds one to
// get from the field back to its opcode, and `R_RX_DIR3U_PCREL` sits in the
// opcode byte already. So GNU as's addend is the plain `A`, never biased by
// the field's offset, and every fixup here is `unbiased_reloc`. The `adjust`
// still applies when a branch resolves within the file.

/// A one-byte `.s` branch.
///
/// The fixup is resolved seven bytes past the opcode, turning the 3..=10 the
/// field holds into the signed -4..=3 a fixup can range-check; [`disp3`] puts
/// the seven back.
fn short(op: u8, target: ExprRef, span: Span) -> Variant {
    let kind = FixupKind::pcrel(1, 7)
        .with_field(3, 1)
        .scatter(disp3)
        .with_reloc(reloc::DIR3U_PCREL)
        .unbiased_reloc();
    let mut e = Enc::new(&[op]);
    e.fixup_at(0, target, kind, span);
    e.variant()
}

/// A one-byte opcode followed by an `n`-byte displacement.
fn plain(op: u8, n: u8, target: ExprRef, span: Span) -> Variant {
    long(&[op], n, target, span)
}

/// `prefix` followed by an `n`-byte displacement, measured from the prefix's
/// last byte, which is the opcode the displacement belongs to.
fn long(prefix: &[u8], n: u8, target: ExprRef, span: Span) -> Variant {
    let r = match n {
        1 => reloc::DIR8S_PCREL,
        2 => reloc::DIR16S_PCREL,
        _ => reloc::DIR24S_PCREL,
    };
    let kind = FixupKind::pcrel(n, -1).with_reloc(r).unbiased_reloc();
    let mut e = Enc::new(prefix);
    e.fixup(target, kind, span);
    e.variant()
}

/// Assembles a branch to `target`.
pub fn assemble(
    cx: &mut AsmCtx<'_>,
    kind: Kind,
    width: Width,
    target: ExprRef,
    m: &str,
    span: Span,
) -> Option<Vec<Variant>> {
    let t = target;
    let v = match (kind, width) {
        (Kind::Bra, Width::Auto) => vec![
            short(0x08, t, span),
            plain(0x2e, 1, t, span),
            plain(0x38, 2, t, span),
            plain(0x04, 3, t, span),
        ],
        (Kind::Bra, Width::S) => vec![short(0x08, t, span)],
        (Kind::Bra, Width::B) => vec![plain(0x2e, 1, t, span)],
        (Kind::Bra, Width::W) => vec![plain(0x38, 2, t, span)],
        (Kind::Bra, Width::A) => vec![plain(0x04, 3, t, span)],

        (Kind::Bsr, Width::Auto) => vec![plain(0x39, 2, t, span), plain(0x05, 3, t, span)],
        (Kind::Bsr, Width::W) => vec![plain(0x39, 2, t, span)],
        (Kind::Bsr, Width::A) => vec![plain(0x05, 3, t, span)],

        (Kind::Cond(c @ (0 | 1)), Width::Auto) => {
            // The synthetic form skips over a `bra.a` with the opposite
            // condition: `bne.s .+5` is 0x1d, `beq.s .+5` is 0x15.
            let skip = if c == 0 { 0x1d } else { 0x15 };
            vec![
                short(0x10 | (c << 3), t, span),
                plain(0x20 | c, 1, t, span),
                plain(0x3a | c, 2, t, span),
                long(&[skip, 0x04], 3, t, span),
            ]
        }
        (Kind::Cond(c @ (0 | 1)), Width::S) => vec![short(0x10 | (c << 3), t, span)],
        (Kind::Cond(c @ (0 | 1)), Width::W) => vec![plain(0x3a | c, 2, t, span)],
        (Kind::Cond(c), Width::B) => vec![plain(0x20 | c, 1, t, span)],
        (Kind::Cond(c), Width::Auto) => {
            let inv = 0x20 | (c ^ 1);
            // `rx_relax_frag` checks the `bra.w` pair's reach from the
            // conditional's opcode, two bytes before the `bra.w`, so GNU as
            // stops using it at a forward displacement of 32765. Its matching
            // lower check lets -32769 and -32770 through, where the field
            // then silently wraps; that half is not copied.
            let mut pair = long(&[inv, 0x05, 0x38], 2, t, span);
            for f in &mut pair.fixups {
                f.kind = f.kind.with_limits(-0x8000, 0x7ffd);
            }
            vec![
                plain(0x20 | c, 1, t, span),
                pair,
                long(&[inv, 0x06, 0x04], 3, t, span),
            ]
        }
        (Kind::Cond(_), Width::S | Width::W) => {
            cx.error(
                span,
                format!("`{m}` has no such size; only `beq` and `bne` have `.s` and `.w`"),
            );
            return None;
        }
        (Kind::Cond(_), Width::A) => {
            cx.error(
                span,
                format!("`{m}` has no `.a` size; conditional branches reach at most 16 bits"),
            );
            return None;
        }
        (Kind::Bsr, Width::S | Width::B) => {
            cx.error(
                span,
                format!("`{m}` has no such size; `bsr` is `.w` or `.a`"),
            );
            return None;
        }
    };
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_displacements_wrap_eight_to_ten() {
        for (disp, field) in [(3, 3), (7, 7), (8, 0), (9, 1), (10, 2)] {
            assert_eq!(disp3(0x08, disp - 7), 0x08 | field);
        }
    }
}
