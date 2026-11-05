//! Byte emission: the instruction builder, and the address checks that decide
//! between encodings before any byte is written.

use super::operand::{Expr, Operand};
use super::reloc;
use crate::arch::AsmCtx;
use crate::section::{Fixup, FixupKind, Variant};

/// The `ES:` prefix byte. It is a real prefix — it comes before the whole
/// opcode, including a `61`/`71`/`31` second-page byte — and it changes
/// nothing about the rest of the encoding, which is why every `es:` form is
/// the plain form with `11` in front.
pub const PREFIX_ES: u8 = 0x11;

/// The short direct address range, `saddr`: one byte of address, with
/// `0x20`–`0xFF` meaning `0xFFE20`–`0xFFEFF` and `0x00`–`0x1F` meaning
/// `0xFFF00`–`0xFFF1F` (see `saddr()` in `opcodes/rl78-decode.opc`).
pub const SADDR: (i64, i64) = (0xffe20, 0xfff1f);
/// The SFR range: one byte of address, the low byte of `0xFFFxx`.
pub const SFR: (i64, i64) = (0xfff00, 0xfffff);

/// One instruction being built.
pub struct Enc {
    pub bytes: Vec<u8>,
    pub fixups: Vec<Fixup>,
}

impl Enc {
    /// Starts an instruction with its opcode bytes, preceded by the `ES:`
    /// prefix if the operand that selects the address was written with one.
    pub fn new(es: bool, opcode: &[u8]) -> Enc {
        let mut bytes = Vec::with_capacity(6);
        if es {
            bytes.push(PREFIX_ES);
        }
        bytes.extend_from_slice(opcode);
        Enc {
            bytes,
            fixups: Vec::new(),
        }
    }

    pub fn byte(&mut self, b: u8) {
        self.bytes.push(b);
    }

    fn field(&mut self, x: Expr, kind: FixupKind) {
        self.fixups.push(Fixup {
            offset: self.bytes.len() as u32,
            expr: x.e,
            kind,
            span: x.span,
        });
        self.bytes
            .extend(std::iter::repeat_n(0u8, kind.size as usize));
    }

    /// An 8-bit immediate or displacement. Always a fixup, even for a
    /// constant, so the core does the range check and names the limit.
    pub fn imm8(&mut self, x: Expr) {
        self.field(x, reloc::imm8());
    }

    pub fn imm16(&mut self, x: Expr) {
        self.field(x, reloc::imm16());
    }

    pub fn addr20(&mut self, x: Expr) {
        self.field(x, reloc::addr20());
    }

    pub fn rel8(&mut self, x: Expr) {
        self.field(x, reloc::rel8());
    }

    pub fn rel16(&mut self, x: Expr) {
        self.field(x, reloc::rel16());
    }

    /// A 16-bit absolute address, `!addr`.
    pub fn addr16(&mut self, cx: &mut AsmCtx<'_>, x: Expr) -> Option<()> {
        match cx.constant(x.e) {
            Some(v) => {
                // The reference silently keeps the low 16 bits of anything.
                // Only the two ranges a 16-bit address can actually name are
                // accepted here: the low 64 KiB, and its alias at the top of
                // the address space where RAM and the SFRs are.
                if !((0..=0xffff).contains(&v) || (0xf0000..=0xfffff).contains(&v)) {
                    cx.error(
                        x.span,
                        format!(
                            "address {v:#x} is out of range for `!addr` \
                             (0 to 0xffff, or 0xf0000 to 0xfffff)"
                        ),
                    );
                    return None;
                }
                self.bytes.push(v as u8);
                self.bytes.push((v >> 8) as u8);
            }
            None => self.field(x, reloc::addr16()),
        }
        Some(())
    }

    /// The one address byte of a short direct or SFR operand.
    pub fn direct(&mut self, d: Direct) {
        match d.value {
            Some(v) => self.bytes.push(v as u8),
            // Only a short direct address can be symbolic; see [`Direct`].
            None => self.field(d.x, reloc::saddr()),
        }
    }

    pub fn variant(self) -> Variant {
        Variant {
            bytes: self.bytes,
            fixups: self.fixups,
        }
    }

    /// The usual result: exactly one candidate encoding.
    pub fn done(self) -> Option<Vec<Variant>> {
        Some(vec![self.variant()])
    }
}

/// Which of the two one-byte address spaces an operand ended up in.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Area {
    Saddr,
    Sfr,
}

/// A bare address resolved to one of the one-byte address spaces.
#[derive(Copy, Clone, Debug)]
pub struct Direct {
    pub area: Area,
    /// The address, if it is a constant now.
    pub value: Option<i64>,
    pub x: Expr,
}

/// The order an instruction tries the address spaces in.
///
/// `0xFFF00`–`0xFFF1F` is both a short direct address and an SFR, so for an
/// address there the order decides the encoding. It is not consistent across
/// the instruction set: the reference's grammar tests `saddr` first for
/// `mov a, addr`, `mov1` and every `movw` form, and `sfr` first for
/// `mov addr, a`, `mov addr, #imm`, `xch`, `set1`/`clr1`, `and1`/`or1`/`xor1`
/// and `bt`/`bf`/`btclr`. Each call site names the order it uses.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Order {
    SaddrFirst,
    SfrFirst,
    /// The form has no SFR variant at all.
    SaddrOnly,
}

fn within(v: i64, range: (i64, i64)) -> bool {
    (range.0..=range.1).contains(&v)
}

/// Classifies a bare address for an instruction that tries the spaces in
/// `order`.
///
/// A symbol whose value is not known yet is always a short direct address,
/// which is also the reference's rule (`expr_is_saddr` accepts any
/// non-constant, `expr_is_sfr` none): SFRs are fixed hardware addresses that
/// a program names with constants, while `saddr` RAM is where linked data
/// goes.
pub fn classify(cx: &mut AsmCtx<'_>, x: Expr, order: Order) -> Option<Direct> {
    let value = cx.constant(x.e);
    let saddr = value.is_none_or(|v| within(v, SADDR));
    let sfr = value.is_some_and(|v| within(v, SFR));
    let area = match order {
        Order::SaddrFirst | Order::SaddrOnly if saddr => Area::Saddr,
        Order::SfrFirst if sfr => Area::Sfr,
        Order::SfrFirst if saddr => Area::Saddr,
        Order::SaddrFirst if sfr => Area::Sfr,
        _ => {
            let v = value.unwrap_or(0);
            let msg = if order == Order::SaddrOnly {
                format!(
                    "address {v:#x} is not a short direct address ({:#x} to {:#x})",
                    SADDR.0, SADDR.1
                )
            } else {
                format!(
                    "address {v:#x} is neither a short direct address ({:#x} to {:#x}) \
                     nor an SFR ({:#x} to {:#x})",
                    SADDR.0, SADDR.1, SFR.0, SFR.1
                )
            };
            cx.error(x.span, msg);
            return None;
        }
    };
    Some(Direct { area, value, x })
}

/// A 16-bit value read from or written to an address has to start on an even
/// one. Only constants can be checked; so does the reference.
pub fn word_aligned(cx: &mut AsmCtx<'_>, x: Expr) -> Option<()> {
    match cx.constant(x.e) {
        Some(v) if v & 1 != 0 => {
            cx.error(
                x.span,
                format!("a 16-bit operand needs an even address or offset, not {v:#x}"),
            );
            None
        }
        _ => Some(()),
    }
}

/// A constant operand that must fall in `lo..=hi`, such as a shift count.
pub fn small_constant(
    cx: &mut AsmCtx<'_>,
    op: &Operand,
    what: &str,
    lo: i64,
    hi: i64,
) -> Option<u8> {
    let super::operand::Kind::Direct(x) = op.kind else {
        cx.error(op.span, format!("expected {what} as a plain number"));
        return None;
    };
    let Some(v) = cx.constant(x.e) else {
        cx.error(x.span, format!("{what} must be a constant"));
        return None;
    };
    if !(lo..=hi).contains(&v) {
        let range = if lo == hi {
            format!("must be {lo}")
        } else {
            format!("is out of range ({lo} to {hi})")
        };
        cx.error(x.span, format!("{what} {v} {range}"));
        return None;
    }
    Some(v as u8)
}
