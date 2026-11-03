//! ELF relocation numbers.
//!
//! PPC32 and PPC64 share the low end of the table — `R_PPC_ADDR32` and
//! `R_PPC64_ADDR32` are both 1, and so on up through `REL32` — which is why
//! one set of constants serves both, and why the ones that exist for only one
//! of the two say so. The numbers here were read back from objects produced
//! by `llvm-mc` and by `powerpc64-linux-gnu-as`, not from memory.

pub const ADDR32: u32 = 1;
pub const ADDR24: u32 = 2;
pub const ADDR16: u32 = 3;
pub const ADDR16_LO: u32 = 4;
pub const ADDR16_HI: u32 = 5;
pub const ADDR16_HA: u32 = 6;
pub const ADDR14: u32 = 7;
pub const REL24: u32 = 10;
pub const REL14: u32 = 11;
pub const GOT16: u32 = 14;
pub const GOT16_LO: u32 = 15;
pub const GOT16_HI: u32 = 16;
pub const GOT16_HA: u32 = 17;
pub const REL32: u32 = 26;

/// Thread-local storage. From here to 94 the two tables agree again, apart
/// from the width of the three data relocations, which is the pointer's:
/// `R_PPC_DTPMOD32` and `R_PPC64_DTPMOD64` are both 68.
///
/// `TLS` marks an instruction that uses the thread pointer and fills no
/// field in it; see [`super::encode::Encoder::tls_register`].
pub const TLS: u32 = 67;
pub const DTPMOD: u32 = 68;
pub const TPREL16: u32 = 69;
pub const TPREL16_LO: u32 = 70;
pub const TPREL16_HI: u32 = 71;
pub const TPREL16_HA: u32 = 72;
pub const TPREL: u32 = 73;
pub const DTPREL16: u32 = 74;
pub const DTPREL16_LO: u32 = 75;
pub const DTPREL16_HI: u32 = 76;
pub const DTPREL16_HA: u32 = 77;
pub const DTPREL: u32 = 78;
pub const GOT_TLSGD16: u32 = 79;
pub const GOT_TLSGD16_LO: u32 = 80;
pub const GOT_TLSGD16_HI: u32 = 81;
pub const GOT_TLSGD16_HA: u32 = 82;
pub const GOT_TLSLD16: u32 = 83;
pub const GOT_TLSLD16_LO: u32 = 84;
pub const GOT_TLSLD16_HI: u32 = 85;
pub const GOT_TLSLD16_HA: u32 = 86;
/// PowerPC64 defines these two and their `_LO` halves only as DS-form
/// relocations — 87 is `R_PPC64_GOT_TPREL16_DS` — and both references write
/// the same number for an `addi` as for an `ld`, since what the GOT entry is
/// loaded by is always a `ld`.
pub const GOT_TPREL16: u32 = 87;
pub const GOT_TPREL16_LO: u32 = 88;
pub const GOT_TPREL16_HI: u32 = 89;
pub const GOT_TPREL16_HA: u32 = 90;
pub const GOT_DTPREL16: u32 = 91;
pub const GOT_DTPREL16_LO: u32 = 92;
pub const GOT_DTPREL16_HI: u32 = 93;
pub const GOT_DTPREL16_HA: u32 = 94;
/// The marker a `(sym@tlsgd)` or `(sym@tlsld)` argument puts on the call to
/// `__tls_get_addr`, which the two tables number differently: PowerPC64 had
/// already given 95 and 96 to its DS-form thread-pointer offsets when the
/// markers were added.
pub const TLSGD_32: u32 = 95;
pub const TLSLD_32: u32 = 96;
pub const TLSGD_64: u32 = 107;
pub const TLSLD_64: u32 = 108;

/// 32-bit only: a call that the linker routes through the PLT (`@plt`), and
/// one it is told to keep direct (`@local`). PowerPC64 has neither; see
/// [`super::encode::Encoder::branch`].
pub const PLTREL24: u32 = 18;
pub const LOCAL24PC: u32 = 23;

/// 64-bit only.
pub const ADDR64: u32 = 38;
/// The halves above bit 31 of a 64-bit address, which only a 64-bit object
/// can name: `@higher` and `@highest` take bits 32:47 and 48:63, and the `a`
/// spellings pre-compensate for a sign-extended low half the way `@ha` does.
pub const ADDR16_HIGHER: u32 = 39;
pub const ADDR16_HIGHERA: u32 = 40;
pub const ADDR16_HIGHEST: u32 = 41;
pub const ADDR16_HIGHESTA: u32 = 42;
pub const REL64: u32 = 44;
/// 64-bit only: an offset into the TOC, the ELFv1 and ELFv2 register-2
/// window onto a program's addresses.
pub const TOC16: u32 = 47;
pub const TOC16_LO: u32 = 48;
pub const TOC16_HI: u32 = 49;
pub const TOC16_HA: u32 = 50;
/// The DS-form variants, whose low two bits belong to the opcode.
pub const ADDR16_DS: u32 = 56;
pub const ADDR16_LO_DS: u32 = 57;
pub const GOT16_DS: u32 = 58;
pub const GOT16_LO_DS: u32 = 59;
pub const TOC16_DS: u32 = 63;
pub const TOC16_LO_DS: u32 = 64;
/// 64-bit only: `@high` and `@higha` take the same halfword as `@h` and
/// `@ha` but are not checked for overflow, which is what a `lis`/`ori`
/// sequence building a full 64-bit address needs.
pub const ADDR16_HIGH: u32 = 110;
pub const ADDR16_HIGHA: u32 = 111;

/// 64-bit only: the DS-form and upper halves of the two thread-local
/// offsets, which mirror those of an address.
pub const TPREL16_DS: u32 = 95;
pub const TPREL16_LO_DS: u32 = 96;
pub const TPREL16_HIGHER: u32 = 97;
pub const TPREL16_HIGHERA: u32 = 98;
pub const TPREL16_HIGHEST: u32 = 99;
pub const TPREL16_HIGHESTA: u32 = 100;
pub const DTPREL16_DS: u32 = 101;
pub const DTPREL16_LO_DS: u32 = 102;
pub const DTPREL16_HIGHER: u32 = 103;
pub const DTPREL16_HIGHERA: u32 = 104;
pub const DTPREL16_HIGHEST: u32 = 105;
pub const DTPREL16_HIGHESTA: u32 = 106;
pub const TPREL16_HIGH: u32 = 112;
pub const TPREL16_HIGHA: u32 = 113;
pub const DTPREL16_HIGH: u32 = 114;
pub const DTPREL16_HIGHA: u32 = 115;

/// The 34-bit field of a POWER10 prefixed instruction: absolute, relative to
/// the instruction (`@pcrel`), and the address of a GOT entry relative to the
/// instruction (`@got@pcrel`).
pub const D34: u32 = 128;
pub const PCREL34: u32 = 132;
pub const GOT_PCREL34: u32 = 133;
/// The thread-local forms of the 34-bit field: the two offsets, which are
/// relative to nothing the instruction knows, and the GOT entries of three of
/// the models that go through one, relative to the instruction.
pub const TPREL34: u32 = 146;
pub const DTPREL34: u32 = 147;
pub const GOT_TLSGD_PCREL34: u32 = 148;
pub const GOT_TLSLD_PCREL34: u32 = 149;
pub const GOT_TPREL_PCREL34: u32 = 150;

/// The relocation a thread-local modifier on a `size`-byte data reference
/// selects: `.long sym@dtpmod` in a 32-bit object, `.quad` in a 64-bit one.
/// The three exist only at the width of a pointer, and GNU as refuses any
/// other width, as it refuses a difference.
pub fn tls_data(name: &str, size: u8, pcrel: bool, bits64: bool) -> Option<u32> {
    if pcrel || size != if bits64 { 8 } else { 4 } {
        return None;
    }
    Some(match name {
        "dtpmod" => DTPMOD,
        "tprel" => TPREL,
        "dtprel" => DTPREL,
        _ => return None,
    })
}

/// The relocation for a `size`-byte data reference, or `None` where the ABI
/// has none.
pub fn data(size: u8, pcrel: bool, bits64: bool) -> Option<u32> {
    Some(match (size, pcrel) {
        (8, false) if bits64 => ADDR64,
        (8, true) if bits64 => REL64,
        (4, false) => ADDR32,
        (4, true) => REL32,
        (2, false) => ADDR16,
        _ => return None,
    })
}
