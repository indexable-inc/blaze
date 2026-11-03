//! ELF relocation types for SuperH (`R_SH_*`).
//!
//! Every number here was read from `sh-elf-readelf -r` on an object that
//! `sh-elf-as` (binutils 2.47) produced, not taken from a header.
//!
//! Instruction fields get no relocation. GNU as resolves every PC-relative
//! instruction field at assembly time, and reports a branch to another
//! section or to an undefined symbol as an overflow rather than emitting
//! `R_SH_DIR8WPN` / `R_SH_IND12W`; those appear only under `--relax`, which
//! this backend does not model. The one exception GNU as makes, an
//! `R_SH_DIR8WPL` for `mov.l undefined,rn` at offset 0, depends on where the
//! instruction sits and is not reproduced.

use crate::arch::FlatModifier;

pub const DIR32: u32 = 1;
pub const REL32: u32 = 2;
pub const DIR16: u32 = 33;
pub const DIR8: u32 = 34;
pub const TLS_GD_32: u32 = 144;
pub const TLS_LD_32: u32 = 145;
pub const TLS_LDO_32: u32 = 146;
pub const TLS_IE_32: u32 = 147;
pub const TLS_LE_32: u32 = 148;
pub const GOT32: u32 = 160;
pub const PLT32: u32 = 161;
pub const GOTOFF: u32 = 166;
pub const GOTPLT32: u32 = 168;
pub const GOTFUNCDESC: u32 = 203;
pub const GOTOFFFUNCDESC: u32 = 205;
pub const FUNCDESC: u32 = 207;

/// The relocation for an `size`-byte data reference.
///
/// Only a 32-bit field can be PC-relative: GNU as rejects `.word sym - .`
/// against an undefined `sym`, because the one 16-bit PC-relative SH
/// relocation means something else.
pub fn data(size: u8, pcrel: bool) -> Option<u32> {
    Some(match (size, pcrel) {
        (4, false) => DIR32,
        (4, true) => REL32,
        (2, false) => DIR16,
        (1, false) => DIR8,
        _ => return None,
    })
}

/// The relocation a `sym@NAME` modifier selects, for the 32-bit fields GNU
/// as allows them in.
pub fn modifier(name: &str, size: u8) -> Option<u32> {
    if size != 4 {
        return None;
    }
    Some(match name.to_ascii_lowercase().as_str() {
        "got" => GOT32,
        "plt" => PLT32,
        "gotoff" => GOTOFF,
        "gotplt" => GOTPLT32,
        "tlsgd" => TLS_GD_32,
        "tlsldm" => TLS_LD_32,
        "dtpoff" => TLS_LDO_32,
        "gottpoff" => TLS_IE_32,
        "tpoff" => TLS_LE_32,
        "pcrel" => REL32,
        "gotfuncdesc" => GOTFUNCDESC,
        "gotofffuncdesc" => GOTOFFFUNCDESC,
        "funcdesc" => FUNCDESC,
        _ => return None,
    })
}

/// What a modifier makes of a value in a flat binary. `R_SH_PLT32` and
/// `R_SH_REL32` are both PC-relative, and a static image's PLT entry is the
/// function itself; everything else names a GOT, a TLS block or a function
/// descriptor, which only a linker makes.
pub fn flat_modifier(name: &str) -> FlatModifier {
    match name.to_ascii_lowercase().as_str() {
        "plt" | "pcrel" => FlatModifier::PcRelative,
        _ => FlatModifier::LinkerOnly,
    }
}
