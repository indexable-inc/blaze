//! Writing a relocation's value into code and data (docs/v2/plan/lanes/link/notes.md, "Relocations").
//!
//! Every [`Apply`] but [`Apply::Native`] is computed and written here, for x86-64 and
//! AArch64; a value that does not fit its field is an error naming the field, never a
//! truncation. [`apply`] is one `match` on the kind with no allocation: a failure is a
//! [`Bad`] of a few words, made into words only when the link reports it.

use std::fmt;

use blaze_codegen::{Arch, RelocKind};
use blaze_mem::align::align_up_u64;

use crate::{
    Format,
    model::{Apply, Tls},
};

/// Why a relocation can't be written. `Copy` and 24 bytes: the success path builds nothing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Bad {
    PastEnd { apply: Apply, need: u8 },
    Signed32(i64),
    Unsigned32(u64),
    NoTls,
    OutsideTls,
    SecRelNotTls,
    Section16,
    TlsOffsetIn(Format, Arch),
    A64Literal,
    Branch { offset: i64, bits: u8 },
    Adrp(i64),
    Adr(i64),
    Lo12Insn(u32),
    Lo12Align { low: u64, shift: u8 },
    TpRange(i64),
    Native,
    WasmTls,
}

const _: () = assert!(size_of::<Bad>() <= 24);

impl fmt::Display for Bad {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Bad::PastEnd { apply, need } => {
                write!(f, "a {apply:?} relocation {need} bytes wide past the end of its piece")
            }
            Bad::Signed32(value) => write!(f, "{value:#x} does not fit a signed 32-bit field"),
            Bad::Unsigned32(value) => write!(f, "{value:#x} does not fit an unsigned 32-bit field"),
            Bad::NoTls => f.write_str("a thread-local relocation in an image with no thread-locals"),
            Bad::OutsideTls => f.write_str("a TLS offset outside TLS"),
            Bad::SecRelNotTls => f.write_str(
                "a section-relative relocation to what is not thread-local: only `.tls`'s are kept in the image",
            ),
            Bad::Section16 => f.write_str("a section index relocation outside debug info, which is dropped"),
            Bad::TlsOffsetIn(format, arch) => write!(
                f,
                "a thread-local offset in {format:?} {arch:?} code: this target reaches thread-locals \
                 through descriptors (TLV relocations)"
            ),
            Bad::A64Literal => f.write_str("an AArch64 thread-local offset is a 64-bit literal"),
            Bad::Branch { offset, bits } => write!(
                f,
                "an AArch64 branch of {offset:#x} bytes, past its {bits}-bit field's reach of {:#x}",
                1i64 << (bits + 1)
            ),
            Bad::Adrp(pages) => write!(f, "an ADRP of {pages:#x} pages, past its 4 GiB reach"),
            Bad::Adr(offset) => write!(f, "an ADR of {offset:#x} bytes, past its 1 MiB reach"),
            Bad::Lo12Insn(insn) => {
                write!(f, "a low-12-bit relocation in {insn:#010x}, neither an ADD nor a load or store")
            }
            Bad::Lo12Align { low, shift } => write!(
                f,
                "a low-12-bit offset {low:#x} not aligned to the {}-byte access it is in",
                1u32 << shift
            ),
            Bad::TpRange(off) => write!(f, "a thread-pointer offset {off:#x} past 16 MiB"),
            Bad::Native => f.write_str("a native relocation is the format writer's to apply"),
            Bad::WasmTls => f.write_str("thread-locals of wasm are the wasm writer's"),
        }
    }
}

/// What a relocation's value is made of, resolved by the caller.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Ctx {
    pub arch: Arch,
    pub format: Format,
    /// The target's address (its PLT stub's for a branch to an import; 0 for a weak symbol
    /// no one defines).
    pub s: u64,
    pub a: i64,
    /// The place's address.
    pub p: u64,
    /// The target's GOT cell, for the kinds that go through one.
    pub g: u64,
    /// [`crate::model::Fixup::minus`]'s address.
    pub m: u64,
    pub base: u64,
    pub tls: Option<Tls>,
    /// The target's thread-local offset when the image's thread-locals are at a place fixed
    /// beside another image's ([`crate::state::State::tls_at`]): every thread-local kind then
    /// writes `tp + A`, whatever the format's TLS model would compute from `tls`.
    pub tp: Option<i64>,
}

/// The [`Apply`] a codegen relocation is written as, by the kind of target it has and
/// whether it is in a function's code (`in_code`) or in data. A thread-local kind is valid
/// only on the format and arch whose TLS model it belongs to.
pub(crate) fn of_kind(kind: RelocKind, format: Format, arch: Arch, in_code: bool) -> Result<Apply, String> {
    if arch == Arch::Wasm32 {
        return crate::wasm::codegen_apply(kind, in_code);
    }
    use {Arch::Aarch64 as A64, Arch::X86_64 as X64, Format::*};
    Ok(match (kind, format, arch) {
        (RelocKind::Abs64, _, _) => Apply::Abs64,
        (RelocKind::PcRel32, _, X64) => Apply::X86Branch32,
        (RelocKind::GotPcRel32, _, X64) => Apply::X86GotPcRel32,
        (RelocKind::Call26, _, A64) => Apply::A64Call26,
        (RelocKind::AdrpPage21, _, A64) => Apply::A64Page21,
        // The shift is read from the instruction the field is in: see `a64_lo12_shift`.
        (RelocKind::PageOffset12, _, A64) => Apply::A64Lo12 { shift: u8::MAX },
        (RelocKind::GotPage21, _, A64) => Apply::A64GotPage21,
        (RelocKind::GotPageOffset12, _, A64) => Apply::A64GotLo12,
        (RelocKind::TlsOffset, _, _) => Apply::TlsOffset,
        (RelocKind::TlsGd32, Elf, X64) => Apply::X86TlsGd32,
        (RelocKind::TlsGotTpOff32, Elf, X64) => Apply::X86GotTpOff32,
        (RelocKind::TlvRel32, MachO, X64) => Apply::TlvRel32,
        (RelocKind::TlvPage21, MachO, A64) => Apply::TlvPage21,
        (RelocKind::TlvPageOffset12, MachO, A64) => Apply::TlvLo12,
        (RelocKind::TlsDescPage21, Elf, A64) => Apply::A64TlsDescPage21,
        (RelocKind::TlsDescLd64Lo12, Elf, A64) => Apply::A64TlsDescLo12,
        (RelocKind::TlsDescAddLo12, Elf, A64) => Apply::A64TlsDescAdd12,
        (RelocKind::TlsDescCall, Elf, A64) => Apply::A64TlsDescCall,
        (RelocKind::TlsIePage21, Elf, A64) => Apply::A64TlsIePage21,
        (RelocKind::TlsIeLo12, Elf, A64) => Apply::A64TlsIeLo12,
        (RelocKind::TpHi12, Elf, A64) => Apply::A64TpHi12,
        (RelocKind::TpLo12, Elf, A64) => Apply::A64TpLo12,
        (RelocKind::SectionRel32, Pe, _) => Apply::SecRel32,
        // alloc: error path
        (kind, format, arch) => return Err(format!("a {kind:?} relocation in {format:?} {arch:?} code")),
    })
}

/// The wasm relocation types whose value the core computes before the writer encodes it:
/// a function's index (`R_WASM_FUNCTION_INDEX_LEB`, `_I32`) or its table slot, the index
/// + 1 (`R_WASM_TABLE_INDEX_SLEB`, `_I32`).
pub(crate) const WASM_FUNCTION_INDEX_LEB: u32 = 0;
pub(crate) const WASM_TABLE_INDEX_SLEB: u32 = 1;
pub(crate) const WASM_TABLE_INDEX_I32: u32 = 2;
pub(crate) const WASM_FUNCTION_INDEX_I32: u32 = 26;

/// How many bytes from the place an [`Apply`] writes.
pub(crate) fn width(apply: Apply) -> usize {
    match apply {
        Apply::Abs64 | Apply::Rel64 | Apply::Delta64 | Apply::DtpOff64 | Apply::TpOff64 => 8,
        Apply::Section16 => 2,
        Apply::A64TlsDescCall | Apply::Native(_) => 0,
        Apply::TlsOffset => 4,
        _ => 4,
    }
}

/// Whether the kind reaches its target through a GOT cell, and which.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub(crate) enum GotKind {
    Address,
    TpOff,
    TlsGd,
    TlsDesc,
}

impl GotKind {
    pub fn cells(self) -> u64 {
        match self {
            GotKind::Address | GotKind::TpOff => 1,
            GotKind::TlsGd | GotKind::TlsDesc => 2,
        }
    }
}

pub(crate) fn got_kind(apply: Apply) -> Option<GotKind> {
    match apply {
        Apply::X86GotPcRel32
        | Apply::A64GotPage21
        | Apply::A64GotLo12
        | Apply::GotRel32
        | Apply::TlvPage21
        | Apply::TlvLo12
        | Apply::TlvRel32 => Some(GotKind::Address),
        Apply::X86GotTpOff32 | Apply::A64TlsIePage21 | Apply::A64TlsIeLo12 => Some(GotKind::TpOff),
        Apply::X86TlsGd32 => Some(GotKind::TlsGd),
        Apply::A64TlsDescPage21 | Apply::A64TlsDescLo12 | Apply::A64TlsDescAdd12 => {
            Some(GotKind::TlsDesc)
        }
        _ => None,
    }
}

/// Whether the kind is a branch, which reaches an imported function through its PLT stub.
pub(crate) fn is_branch(apply: Apply) -> bool {
    matches!(apply, Apply::X86Branch32 | Apply::A64Call26 | Apply::A64Jump26)
}

/// Whether the kind reads its target's thread-local offset, which changes when the TLS
/// template's layout does.
pub(crate) fn is_tls(apply: Apply) -> bool {
    matches!(
        apply,
        Apply::X86TpOff32
            | Apply::DtpOff32
            | Apply::DtpOff64
            | Apply::A64TpHi12
            | Apply::A64TpLo12
            | Apply::TlsOffset
            | Apply::TpOff64
            | Apply::SecRel32
    )
}

/// The offset of the thread-local at `s` from the thread pointer: ELF variant II on
/// x86-64 (the block ends at the pointer), variant I on AArch64 (a 16-byte TCB, then the
/// block at its alignment).
pub(crate) fn tp_offset(arch: Arch, tls: Tls, s: u64) -> Result<i64, Bad> {
    let at = s.checked_sub(tls.addr).ok_or(Bad::OutsideTls)?;
    let align = tls.align.max(1);
    Ok(match arch {
        Arch::X86_64 => at as i64 - align_up_u64(tls.size, align) as i64,
        Arch::Aarch64 => (align_up_u64(16, align) + at) as i64,
        Arch::Wasm32 => return Err(Bad::WasmTls),
    })
}

/// Writes `apply`'s value at the start of `field`.
pub(crate) fn apply(apply: Apply, ctx: &Ctx, field: &mut [u8]) -> Result<(), Bad> {
    let need = width(apply);
    if field.len() < need {
        return Err(Bad::PastEnd { apply, need: need as u8 });
    }
    let Ctx { s, a, p, g, m, base, .. } = *ctx;
    let sa = s.wrapping_add(a as u64);
    let ga = g.wrapping_add(a as u64);
    let pc = |to: u64| to.wrapping_sub(p) as i64;
    let tls = || ctx.tls.ok_or(Bad::NoTls);
    // The offset from the thread pointer, and from the start of the image's block.
    let tp = || match ctx.tp {
        Some(tp) => Ok(tp),
        None => tp_offset(ctx.arch, tls()?, s),
    };
    let dtp = || match ctx.tp {
        Some(tp) => Ok(tp),
        None => Ok(s.checked_sub(tls()?.addr).ok_or(Bad::OutsideTls)? as i64),
    };
    match apply {
        Apply::Abs64 => put(field, sa.to_le_bytes()),
        Apply::Abs32 => put(field, fits_u32(sa)?.to_le_bytes()),
        Apply::Abs32S => put(field, fits_i32(sa as i64)?.to_le_bytes()),
        Apply::Rel64 => put(field, pc(sa).to_le_bytes()),
        Apply::Rel32 | Apply::X86Branch32 => put(field, fits_i32(pc(sa))?.to_le_bytes()),
        Apply::Delta64 => put(field, sa.wrapping_sub(m).to_le_bytes()),
        Apply::Delta32 => put(field, fits_i32(sa.wrapping_sub(m) as i64)?.to_le_bytes()),
        Apply::ImageRel32 => put(field, fits_u32(sa.wrapping_sub(base))?.to_le_bytes()),
        Apply::SecRel32 => {
            let at = match ctx.tp {
                Some(tp) => u64::try_from(tp + a).map_err(|_| Bad::SecRelNotTls)?,
                None => {
                    let tls = tls()?;
                    sa.checked_sub(tls.addr).filter(|it| *it <= tls.size).ok_or(Bad::SecRelNotTls)?
                }
            };
            put(field, fits_u32(at)?.to_le_bytes())
        }
        Apply::Section16 => return Err(Bad::Section16),
        Apply::X86GotPcRel32
        | Apply::X86GotTpOff32
        | Apply::X86TlsGd32
        | Apply::GotRel32
        | Apply::TlvRel32 => put(field, fits_i32(pc(ga))?.to_le_bytes()),
        Apply::X86TpOff32 => {
            let off = tp()? + a;
            put(field, fits_i32(off)?.to_le_bytes())
        }
        Apply::DtpOff32 => {
            let off = dtp()? + a;
            put(field, fits_i32(off)?.to_le_bytes())
        }
        Apply::DtpOff64 => {
            let off = dtp()? + a;
            put(field, off.to_le_bytes())
        }
        Apply::TpOff64 => put(field, (tp()? + a).to_le_bytes()),
        Apply::TlsOffset => match (ctx.format, ctx.arch) {
            (Format::Elf, Arch::X86_64) => {
                let off = tp()? + a;
                put(field, fits_i32(off)?.to_le_bytes())
            }
            (Format::Elf, Arch::Aarch64) => {
                if field.len() < 8 {
                    return Err(Bad::A64Literal);
                }
                let off = tp()? + a;
                put(field, off.to_le_bytes())
            }
            (Format::Pe, _) => {
                let at = dtp()?;
                put(field, fits_u32(at.wrapping_add(a) as u64)?.to_le_bytes())
            }
            (format, arch) => return Err(Bad::TlsOffsetIn(format, arch)),
        },
        Apply::A64Call26 | Apply::A64Jump26 => a64_branch(field, pc(sa), 26)?,
        Apply::A64CondBr19 => a64_branch(field, pc(sa), 19)?,
        Apply::A64TstBr14 => a64_branch(field, pc(sa), 14)?,
        Apply::A64Page21 => a64_adrp(field, sa, p)?,
        Apply::A64Adr21 => a64_adr(field, pc(sa))?,
        Apply::A64Lo12 { shift } => a64_lo12(field, sa, shift)?,
        Apply::A64MovW { group } => {
            let insn = read32(field);
            let imm = ((sa >> (16 * u64::from(group))) & 0xffff) as u32;
            write32(field, (insn & !(0xffff << 5)) | (imm << 5));
        }
        Apply::A64GotPage21 | Apply::A64TlsIePage21 | Apply::A64TlsDescPage21 | Apply::TlvPage21 => {
            a64_adrp(field, ga, p)?
        }
        Apply::A64GotLo12 | Apply::A64TlsIeLo12 | Apply::A64TlsDescLo12 | Apply::TlvLo12 => {
            a64_lo12(field, ga, 3)?
        }
        Apply::A64TlsDescAdd12 => a64_lo12(field, ga, 0)?,
        Apply::A64TlsDescCall => {}
        Apply::A64TpHi12 | Apply::A64TpLo12 => {
            let off = tp()? + a;
            if !(0..1 << 24).contains(&off) {
                return Err(Bad::TpRange(off));
            }
            let imm = if apply == Apply::A64TpHi12 { off >> 12 } else { off } as u32 & 0xfff;
            let insn = read32(field);
            write32(field, (insn & !(0xfff << 10)) | (imm << 10));
        }
        Apply::Native(_) => return Err(Bad::Native),
    }
    Ok(())
}

#[inline(always)]
fn put<const N: usize>(field: &mut [u8], bytes: [u8; N]) {
    field[..N].copy_from_slice(&bytes);
}

#[inline(always)]
fn fits_i32(value: i64) -> Result<i32, Bad> {
    i32::try_from(value).map_err(|_| Bad::Signed32(value))
}

#[inline(always)]
fn fits_u32(value: u64) -> Result<u32, Bad> {
    u32::try_from(value).map_err(|_| Bad::Unsigned32(value))
}

fn read32(field: &[u8]) -> u32 {
    u32::from_le_bytes(field[..4].try_into().expect("four bytes"))
}

fn write32(field: &mut [u8], insn: u32) {
    field[..4].copy_from_slice(&insn.to_le_bytes());
}

/// A `B`/`BL` (26 bits), `B.cond`/`CBZ`/`LDR literal` (19) or `TBZ` (14) offset.
fn a64_branch(field: &mut [u8], offset: i64, bits: u32) -> Result<(), Bad> {
    let reach = 1i64 << (bits + 1);
    if offset & 3 != 0 || !(-reach..reach).contains(&offset) {
        return Err(Bad::Branch { offset, bits: bits as u8 });
    }
    let insn = read32(field);
    let imm = ((offset >> 2) as u32) & ((1 << bits) - 1);
    let insn = match bits {
        26 => (insn & !0x03ff_ffff) | imm,
        19 => (insn & !(0x7_ffff << 5)) | (imm << 5),
        14 => (insn & !(0x3fff << 5)) | (imm << 5),
        _ => unreachable!("a branch field of {bits} bits"),
    };
    write32(field, insn);
    Ok(())
}

fn a64_adr_fields(insn: u32, imm: i64) -> u32 {
    let imm = imm as u32 & 0x1f_ffff;
    (insn & !((3 << 29) | (0x7_ffff << 5))) | ((imm & 3) << 29) | ((imm >> 2) << 5)
}

fn a64_adrp(field: &mut [u8], to: u64, p: u64) -> Result<(), Bad> {
    let pages = ((to & !0xfff) as i64).wrapping_sub((p & !0xfff) as i64) >> 12;
    if !(-(1 << 20)..1 << 20).contains(&pages) {
        return Err(Bad::Adrp(pages));
    }
    write32(field, a64_adr_fields(read32(field), pages));
    Ok(())
}

fn a64_adr(field: &mut [u8], offset: i64) -> Result<(), Bad> {
    if !(-(1 << 20)..1 << 20).contains(&offset) {
        return Err(Bad::Adr(offset));
    }
    write32(field, a64_adr_fields(read32(field), offset));
    Ok(())
}

/// The shift of a 12-bit low-offset field, from the instruction it is in: 0 for `ADD`, the
/// access size's log for `LDR`/`STR` (immediate, unsigned offset), 4 for a 128-bit one.
pub(crate) fn a64_lo12_shift(insn: u32) -> Result<u8, Bad> {
    if insn & 0x1f00_0000 == 0x1100_0000 {
        return Ok(0);
    }
    if insn & 0x3b00_0000 == 0x3900_0000 {
        let size = (insn >> 30) as u8;
        let vector = insn & (1 << 26) != 0;
        let opc = (insn >> 22) & 3;
        return Ok(if vector && size == 0 && opc >= 2 { 4 } else { size });
    }
    Err(Bad::Lo12Insn(insn))
}

fn a64_lo12(field: &mut [u8], to: u64, shift: u8) -> Result<(), Bad> {
    let insn = read32(field);
    let shift = if shift == u8::MAX { a64_lo12_shift(insn)? } else { shift };
    let low = to & 0xfff;
    if low & ((1 << shift) - 1) != 0 {
        return Err(Bad::Lo12Align { low, shift });
    }
    let imm = (low >> shift) as u32;
    write32(field, (insn & !(0xfff << 10)) | (imm << 10));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(arch: Arch, s: u64, a: i64, p: u64) -> Ctx {
        Ctx { arch, format: Format::Elf, s, a, p, g: 0, m: 0, base: 0, tls: None, tp: None }
    }

    #[test]
    fn thread_local_kinds_are_valid_only_on_their_format() {
        use Format::*;
        let ok = [
            (RelocKind::TlsGd32, Elf, Arch::X86_64, Apply::X86TlsGd32),
            (RelocKind::TlsGotTpOff32, Elf, Arch::X86_64, Apply::X86GotTpOff32),
            (RelocKind::TlvRel32, MachO, Arch::X86_64, Apply::TlvRel32),
            (RelocKind::TlvPage21, MachO, Arch::Aarch64, Apply::TlvPage21),
            (RelocKind::TlvPageOffset12, MachO, Arch::Aarch64, Apply::TlvLo12),
            (RelocKind::TlsDescPage21, Elf, Arch::Aarch64, Apply::A64TlsDescPage21),
            (RelocKind::TlsDescLd64Lo12, Elf, Arch::Aarch64, Apply::A64TlsDescLo12),
            (RelocKind::TlsDescAddLo12, Elf, Arch::Aarch64, Apply::A64TlsDescAdd12),
            (RelocKind::TlsDescCall, Elf, Arch::Aarch64, Apply::A64TlsDescCall),
            (RelocKind::TlsIePage21, Elf, Arch::Aarch64, Apply::A64TlsIePage21),
            (RelocKind::TlsIeLo12, Elf, Arch::Aarch64, Apply::A64TlsIeLo12),
            (RelocKind::TpHi12, Elf, Arch::Aarch64, Apply::A64TpHi12),
            (RelocKind::TpLo12, Elf, Arch::Aarch64, Apply::A64TpLo12),
            (RelocKind::SectionRel32, Pe, Arch::X86_64, Apply::SecRel32),
            (RelocKind::SectionRel32, Pe, Arch::Aarch64, Apply::SecRel32),
        ];
        for (kind, format, arch, apply) in ok {
            assert_eq!(of_kind(kind, format, arch, true), Ok(apply), "{kind:?}");
            for other in [Elf, MachO, Pe].into_iter().filter(|it| *it != format) {
                assert!(of_kind(kind, other, arch, true).is_err(), "{kind:?} on {other:?}");
            }
        }
        assert!(of_kind(RelocKind::TlsGd32, Elf, Arch::Aarch64, true).is_err());
        assert!(of_kind(RelocKind::TlvPage21, MachO, Arch::X86_64, true).is_err());
    }

    #[test]
    fn x86_call_is_relative_to_the_end_of_its_field() {
        let mut code = [0xe8, 0, 0, 0, 0];
        apply(Apply::X86Branch32, &ctx(Arch::X86_64, 0x2000, -4, 0x1001), &mut code[1..]).unwrap();
        assert_eq!(i32::from_le_bytes(code[1..].try_into().unwrap()), 0x2000 - 4 - 0x1001);
        let far = apply(Apply::X86Branch32, &ctx(Arch::X86_64, 1 << 40, 0, 0), &mut code[1..]);
        assert_eq!(far, Err(Bad::Signed32(1i64 << 40)));
        assert!(far.unwrap_err().to_string().contains("signed 32-bit"));
    }

    #[test]
    fn aarch64_bl_adrp_and_ldr_keep_their_opcodes() {
        let mut bl = 0x9400_0000u32.to_le_bytes();
        apply(Apply::A64Call26, &ctx(Arch::Aarch64, 0x1_0000, 0, 0x8000), &mut bl).unwrap();
        assert_eq!(u32::from_le_bytes(bl), 0x9400_0000 | (0x8000 >> 2));
        let mut back = 0x9400_0000u32.to_le_bytes();
        apply(Apply::A64Call26, &ctx(Arch::Aarch64, 0x8000, 0, 0x1_0000), &mut back).unwrap();
        assert_eq!(u32::from_le_bytes(back), 0x9400_0000 | (((-0x8000i32 >> 2) as u32) & 0x3ff_ffff));
        let mut too_far = 0x9400_0000u32.to_le_bytes();
        assert!(apply(Apply::A64Call26, &ctx(Arch::Aarch64, 1 << 28, 0, 0), &mut too_far).is_err());

        // adrp x16, page of 0x12345678 from 0x1000.
        let mut adrp = 0x9000_0010u32.to_le_bytes();
        apply(Apply::A64Page21, &ctx(Arch::Aarch64, 0x1234_5678, 0, 0x1000), &mut adrp).unwrap();
        let insn = u32::from_le_bytes(adrp);
        let imm = (((insn >> 5) & 0x7_ffff) << 2) | ((insn >> 29) & 3);
        assert_eq!(imm, (0x1234_5000 - 0x1000) >> 12);
        assert_eq!(insn & 0x9f00_001f, 0x9000_0010);

        // ldr x0, [x1, #lo12]: an 8-byte load, so the offset is scaled by 8.
        let mut ldr = 0xf940_0020u32.to_le_bytes();
        let lo12 = Apply::A64Lo12 { shift: u8::MAX };
        apply(lo12, &ctx(Arch::Aarch64, 0x5678, 0, 0), &mut ldr).unwrap();
        assert_eq!((u32::from_le_bytes(ldr) >> 10) & 0xfff, 0x678 >> 3);
        let mut misaligned = 0xf940_0020u32.to_le_bytes();
        assert!(apply(lo12, &ctx(Arch::Aarch64, 0x5674, 0, 0), &mut misaligned).is_err());
        // add x0, x0, #lo12 is unscaled.
        let mut add = 0x9100_0000u32.to_le_bytes();
        apply(lo12, &ctx(Arch::Aarch64, 0x5675, 0, 0), &mut add).unwrap();
        assert_eq!((u32::from_le_bytes(add) >> 10) & 0xfff, 0x675);
    }

    /// What `as` and `ld` write for the same instructions at the same addresses (v1's splice
    /// checked its jumps and stubs against these words).
    #[test]
    fn aarch64_fields_are_the_words_as_and_ld_write() {
        let word = |insn: u32, apply_as: Apply, s: u64, p: u64| {
            let mut field = insn.to_le_bytes();
            apply(apply_as, &ctx(Arch::Aarch64, s, 0, p), &mut field).unwrap();
            u32::from_le_bytes(field)
        };
        let lo12 = Apply::A64Lo12 { shift: u8::MAX };
        // A far jump to 0x12355678 from 0x10000: adrp x16; add x16, x16, #lo12.
        assert_eq!(word(0x9000_0010, Apply::A64Page21, 0x1235_5678, 0x10000), 0xb009_1a30);
        assert_eq!(word(0x9100_0210, lo12, 0x1235_5678, 0x10004), 0x9119_e210);
        // A stub through the slot at 0x7ff8 from 0x1000c: adrp x16; ldr x16, [x16, #lo12].
        assert_eq!(word(0x9000_0010, Apply::A64Page21, 0x7ff8, 0x1000c), 0xf0ff_ffb0);
        assert_eq!(word(0xf940_0210, lo12, 0x7ff8, 0x10010), 0xf947_fe10);
        // bl 0x400 from 0x1001c, and ldrh w1, [x0, #lo12] of 0x7ff8.
        assert_eq!(word(0x9400_0000, Apply::A64Call26, 0x400, 0x1001c), 0x97ff_c0f9);
        assert_eq!(word(0x7940_0001, lo12, 0x7ff8, 0x10024), 0x795f_f001);
        let mut odd = 0x7940_0001u32.to_le_bytes();
        assert!(apply(lo12, &ctx(Arch::Aarch64, 0x7ffd, 0, 0), &mut odd).is_err());
    }

    /// A `b` or `bl` reaches 128 MiB less 4 forward and 128 MiB back, and no further.
    #[test]
    fn an_aarch64_branch_reaches_128_mib() {
        let reaches = |to: u64, from: u64| {
            let mut bl = 0x9400_0000u32.to_le_bytes();
            apply(Apply::A64Call26, &ctx(Arch::Aarch64, to, 0, from), &mut bl).is_ok()
        };
        let (old, last) = (0x10000u64, 0x10000 + (128 << 20) - 4);
        assert!(reaches(last, old) && !reaches(last + 4, old));
        assert!(reaches(0, 128 << 20) && !reaches(0, (128 << 20) + 4));
        assert!(!reaches(old + 2, old), "a branch to an address not 4-aligned");
    }

    #[test]
    fn lo12_shifts_follow_the_access_size() {
        assert_eq!(a64_lo12_shift(0x3940_0000).unwrap(), 0); // ldrb
        assert_eq!(a64_lo12_shift(0x7940_0000).unwrap(), 1); // ldrh
        assert_eq!(a64_lo12_shift(0xb940_0000).unwrap(), 2); // ldr w
        assert_eq!(a64_lo12_shift(0xf940_0000).unwrap(), 3); // ldr x
        assert_eq!(a64_lo12_shift(0x3dc0_0000).unwrap(), 4); // ldr q
        assert_eq!(a64_lo12_shift(0x9100_0000).unwrap(), 0); // add
        assert!(a64_lo12_shift(0xd503_201f).is_err()); // nop
    }

    #[test]
    fn thread_pointer_offsets_follow_each_variant() {
        let tls = Tls { addr: 0x1000, init: 8, size: 24, align: 16 };
        // x86-64: the block ends at the thread pointer, its size rounded to its alignment.
        assert_eq!(tp_offset(Arch::X86_64, tls, 0x1008).unwrap(), 8 - 32);
        // AArch64: a 16-byte control block, then the block.
        assert_eq!(tp_offset(Arch::Aarch64, tls, 0x1008).unwrap(), 16 + 8);
        let big = Tls { align: 64, ..tls };
        assert_eq!(tp_offset(Arch::Aarch64, big, 0x1000).unwrap(), 64);
    }

    #[test]
    fn deltas_and_got_relative_values() {
        let mut field = [0u8; 4];
        let delta = Ctx { m: 0x100, ..ctx(Arch::X86_64, 0x180, 4, 0) };
        apply(Apply::Delta32, &delta, &mut field).unwrap();
        assert_eq!(i32::from_le_bytes(field), 0x84);
        let got = Ctx { g: 0x3000, ..ctx(Arch::X86_64, 0, -4, 0x1004) };
        apply(Apply::X86GotPcRel32, &got, &mut field).unwrap();
        assert_eq!(i32::from_le_bytes(field), 0x3000 - 4 - 0x1004);
    }

    #[test]
    fn failures_name_the_field_once_reported() {
        let mut short = [0u8; 2];
        let past = apply(Apply::Abs64, &ctx(Arch::X86_64, 0, 0, 0), &mut short).unwrap_err();
        assert_eq!(past, Bad::PastEnd { apply: Apply::Abs64, need: 8 });
        assert!(past.to_string().contains("8 bytes wide"));
        let mut bl = 0x9400_0000u32.to_le_bytes();
        let far = apply(Apply::A64Call26, &ctx(Arch::Aarch64, 1 << 28, 0, 0), &mut bl).unwrap_err();
        assert_eq!(far, Bad::Branch { offset: 1 << 28, bits: 26 });
        assert!(far.to_string().contains("reach of 0x8000000"));
        let mut ldr = 0xf940_0020u32.to_le_bytes();
        let odd = apply(Apply::A64Lo12 { shift: u8::MAX }, &ctx(Arch::Aarch64, 0x5674, 0, 0), &mut ldr);
        assert!(odd.unwrap_err().to_string().contains("8-byte access"));
        let mut field = [0u8; 4];
        let no_tls = apply(Apply::X86TpOff32, &ctx(Arch::X86_64, 0, 0, 0), &mut field);
        assert_eq!(no_tls, Err(Bad::NoTls));
    }
}
