//! `R_ARM_*` relocation numbers, from the ARM ELF ABI.

pub const ABS32: u32 = 2;
pub const REL32: u32 = 3;
pub const ABS16: u32 = 5;
pub const ABS8: u32 = 8;
pub const PREL31: u32 = 42;
pub const THM_CALL: u32 = 10;
pub const CALL: u32 = 28;
pub const JUMP24: u32 = 29;
pub const THM_JUMP24: u32 = 30;
pub const THM_JUMP19: u32 = 51;

/// `sym(GOTOFF)`: the symbol's distance from the GOT, `S + A - GOT_ORG`.
/// `readelf` prints it as `R_ARM_GOTOFF32`.
pub const GOTOFF32: u32 = 24;
/// A reference to `_GLOBAL_OFFSET_TABLE_`: the distance from the field to the
/// GOT, `B(S) + A - P`. `readelf` prints it as `R_ARM_GOTPC`.
pub const BASE_PREL: u32 = 25;
/// `sym(GOT)`: the offset of the symbol's GOT entry from the GOT,
/// `GOT(S) + A - GOT_ORG`. `readelf` prints it as `R_ARM_GOT32`.
pub const GOT_BREL: u32 = 26;
/// `sym(GOT_PREL)`: the symbol's GOT entry measured from the field,
/// `GOT(S) + A - P`, which reaches a GOT entry without first loading the
/// GOT's own address.
pub const GOT_PREL: u32 = 96;

/// `sym(SBREL)`: the symbol's offset from the static base, `S + A - B(S)`,
/// which is how position-independent code with a dedicated base register
/// (`-msingle-pic-base`) reaches a datum. `readelf` prints it as
/// `R_ARM_SBREL32`.
pub const SBREL32: u32 = 9;
/// `sym(TARGET1)`: a word the platform ABI may link as either `R_ARM_ABS32`
/// or `R_ARM_REL32`, which the C++ runtime's `.init_array` entries use so
/// that one source serves both choices. `readelf` prints it as
/// `R_ARM_TARGET1`.
pub const TARGET1: u32 = 38;
/// `sym(TARGET2)`: the platform ABI's second such word, which on GNU
/// systems is `R_ARM_GOT_PREL` and which the exception tables' personality
/// routine references use. `readelf` prints it as `R_ARM_TARGET2`.
pub const TARGET2: u32 = 41;

// The thread-local relocations. Each names the variable itself, never its
// section, and GNU as marks the variable `STT_TLS` for any of them.

/// `sym(TLSDESC)`: the offset of the variable's TLS descriptor from the
/// field, which the `ldr`/`add` pair ahead of a descriptor call reads.
/// `readelf` prints it as `R_ARM_TLS_GOTDESC`.
pub const TLS_GOTDESC: u32 = 90;
/// `bl sym(tlscall)` in ARM code, and `.word sym(TLSCALL)`: the call to the
/// descriptor's resolver, which a linker may turn into a load or a `nop`.
pub const TLS_CALL: u32 = 91;
/// `.tlsdescseq sym` in ARM code, and `.word sym(TLSDESCSEQ)`: marks an
/// instruction of a descriptor sequence a linker may rewrite.
pub const TLS_DESCSEQ: u32 = 92;
/// `bl sym(tlscall)` and `blx sym(tlscall)` in Thumb code.
pub const THM_TLS_CALL: u32 = 93;
/// `sym(TLSGD)`: the offset of the variable's pair of GOT entries, for a
/// general-dynamic `__tls_get_addr` call.
pub const TLS_GD32: u32 = 104;
/// `sym(TLSLDM)`: the offset of the module's GOT entry, for a local-dynamic
/// call.
pub const TLS_LDM32: u32 = 105;
/// `sym(TLSLDO)`: the variable's offset within its module's block.
pub const TLS_LDO32: u32 = 106;
/// `sym(GOTTPOFF)`: the offset of the GOT entry holding the variable's offset
/// from the thread pointer, for initial exec.
pub const TLS_IE32: u32 = 107;
/// `sym(TPOFF)`: the variable's offset from the thread pointer, for local
/// exec.
pub const TLS_LE32: u32 = 108;
/// `.tlsdescseq sym` in Thumb code, whichever width the instruction after it
/// has: GNU as writes the 16-bit number for both.
pub const THM_TLS_DESCSEQ: u32 = 129;

/// `movw rd, #:lower16:sym`.
pub const MOVW_ABS_NC: u32 = 43;
/// `movt rd, #:upper16:sym`.
pub const MOVT_ABS: u32 = 44;
/// `movw rd, #:lower16:(sym - label)`.
pub const MOVW_PREL_NC: u32 = 45;
/// `movt rd, #:upper16:(sym - label)`.
pub const MOVT_PREL: u32 = 46;
/// The Thumb encodings of the same four.
pub const THM_MOVW_ABS_NC: u32 = 47;
pub const THM_MOVT_ABS: u32 = 48;
pub const THM_MOVW_PREL_NC: u32 = 49;
pub const THM_MOVT_PREL: u32 = 50;

/// Relocation for a data reference of `size` bytes.
pub fn data(size: u8, pcrel: bool) -> Option<u32> {
    Some(match (size, pcrel) {
        (4, false) => ABS32,
        (4, true) => REL32,
        (2, false) => ABS16,
        (1, false) => ABS8,
        _ => return None,
    })
}

/// The relocation a `sym(NAME)` suffix selects in a `size`-byte data field.
///
/// GNU as reads the suffix in `s_arm_elf_cons`, which only `.word` and
/// `.long` go through, so every one of these is a four-byte relocation.
/// `(PLT)` is the exception: there the suffix names a branch target and
/// `s_arm_elf_cons` emits the symbol itself, so `.word sym(PLT)` is
/// `.word sym`. `(TLSCALL)` and `(TLSDESCSEQ)` give their ARM relocations
/// even in Thumb code, since `s_arm_elf_cons` reads the table without asking
/// which instruction set it is in.
pub fn modifier(name: &str, size: u8, pcrel: bool) -> Option<u32> {
    if size != 4 || pcrel {
        return None;
    }
    Some(match name {
        "got" => GOT_BREL,
        "got_prel" => GOT_PREL,
        "gotoff" => GOTOFF32,
        "sbrel" => SBREL32,
        "target1" => TARGET1,
        "target2" => TARGET2,
        "plt" => ABS32,
        "tlsgd" => TLS_GD32,
        "tlsldm" => TLS_LDM32,
        "tlsldo" => TLS_LDO32,
        "gottpoff" => TLS_IE32,
        "tpoff" => TLS_LE32,
        "tlsdesc" => TLS_GOTDESC,
        "tlscall" => TLS_CALL,
        "tlsdescseq" => TLS_DESCSEQ,
        _ => return None,
    })
}

/// Whether a suffix names a thread-local relocation; see
/// [`Architecture::modifier_symbols`](crate::arch::Architecture::modifier_symbols).
pub fn is_tls_modifier(name: &str) -> bool {
    matches!(
        name,
        "tlsgd" | "tlsldm" | "tlsldo" | "gottpoff" | "tpoff" | "tlsdesc" | "tlscall" | "tlsdescseq"
    )
}

/// The relocations that carry no addend. Their BFD howtos are not
/// `partial_inplace`, so GNU as leaves a branch with a displacement of zero
/// and a data word zero: `bl sym(tlscall)` is `eb000000`, a branch to its own
/// address plus eight, where a plain `bl sym` holds the usual `-8`.
pub fn has_no_addend(reloc: u32) -> bool {
    matches!(
        reloc,
        TLS_CALL | THM_TLS_CALL | TLS_DESCSEQ | THM_TLS_DESCSEQ
    )
}

/// The PC-relative counterpart of a `movw`/`movt` half, which is what
/// `:lower16:(sym - label)` asks for; see
/// [`Architecture::pcrel_reloc`](crate::arch::Architecture::pcrel_reloc).
pub fn half_pcrel(reloc: u32) -> Option<u32> {
    Some(match reloc {
        MOVW_ABS_NC => MOVW_PREL_NC,
        MOVT_ABS => MOVT_PREL,
        THM_MOVW_ABS_NC => THM_MOVW_PREL_NC,
        THM_MOVT_ABS => THM_MOVT_PREL,
        _ => return None,
    })
}
