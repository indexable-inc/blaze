//! ELF relocation types for SPARC (`R_SPARC_*`), shared by the 32- and 64-bit
//! ABIs: `elf32-sparc` and `elf64-sparc` number them the same way.

#[allow(dead_code)]
pub const NONE: u32 = 0;
pub const ABS8: u32 = 1;
pub const ABS16: u32 = 2;
pub const ABS32: u32 = 3;
pub const DISP8: u32 = 4;
pub const DISP16: u32 = 5;
pub const DISP32: u32 = 6;
/// `call`: a 30-bit field counted in instructions.
pub const WDISP30: u32 = 7;
/// `Bicc`: a 22-bit field counted in instructions.
pub const WDISP22: u32 = 8;
/// The high 22 bits of a value, as `sethi` takes them.
pub const HI22: u32 = 9;
/// A bare 22-bit field, for `sethi` given a plain expression.
pub const ABS22: u32 = 10;
/// A 13-bit signed immediate.
pub const ABS13: u32 = 13;
/// The low 10 bits of a value, as `%lo()` produces them.
pub const LO10: u32 = 12;
pub const ABS64: u32 = 32;
/// `R_SPARC_32` for a field that need not be aligned.
pub const UA32: u32 = 23;
/// `R_SPARC_64` for a field that need not be aligned.
pub const UA64: u32 = 54;
/// V9 branch on register: a 16-bit split field.
pub const WDISP16: u32 = 40;
/// V9 predicted branch: a 19-bit field.
pub const WDISP19: u32 = 41;
pub const DISP64: u32 = 46;

// ---- thread-local access models -------------------------------------------
//
// One relocation per step of each model, read out of objects
// `sparc64-elf-as` and llvm-mc wrote for the operators named in the doc
// comments; the two agree on every one. The `_ADD`, `_CALL`, `_LD` and `_LDX`
// steps fill no field: they mark the instruction they sit on for the linker
// that rewrites the sequence into a cheaper model, and so cover no bytes.

/// `%tgd_hi22()`: the high 22 bits of the general-dynamic GOT offset.
pub const TLS_GD_HI22: u32 = 56;
/// `%tgd_lo10()`: the low 10 bits of the same.
pub const TLS_GD_LO10: u32 = 57;
/// `%tgd_add()`: the `add` that builds the argument of the call below.
pub const TLS_GD_ADD: u32 = 58;
/// `%tgd_call()`: the call to `__tls_get_addr`.
pub const TLS_GD_CALL: u32 = 59;
/// `%tldm_hi22()`: the high 22 bits of the local-dynamic GOT offset.
pub const TLS_LDM_HI22: u32 = 60;
/// `%tldm_lo10()`: the low 10 bits of the same.
pub const TLS_LDM_LO10: u32 = 61;
/// `%tldm_add()`: the `add` that builds the argument of the call below.
pub const TLS_LDM_ADD: u32 = 62;
/// `%tldm_call()`: the call to `__tls_get_addr` that resolves the module.
pub const TLS_LDM_CALL: u32 = 63;
/// `%tldo_hix22()`: the high 22 bits of a variable's offset within its
/// module's block, exclusive-ored so that `%tldo_lox10()` can complete it.
pub const TLS_LDO_HIX22: u32 = 64;
/// `%tldo_lox10()`: the low 10 bits of the same.
pub const TLS_LDO_LOX10: u32 = 65;
/// `%tldo_add()`: the `add` that applies that offset to the module's block.
pub const TLS_LDO_ADD: u32 = 66;
/// `%tie_hi22()`: the high 22 bits of the initial-exec GOT offset.
pub const TLS_IE_HI22: u32 = 67;
/// `%tie_lo10()`: the low 10 bits of the same.
pub const TLS_IE_LO10: u32 = 68;
/// `%tie_ld()`: the 32-bit load of the GOT entry.
pub const TLS_IE_LD: u32 = 69;
/// `%tie_ldx()`: the 64-bit load of the same.
pub const TLS_IE_LDX: u32 = 70;
/// `%tie_add()`: the `add` that applies the loaded offset to `%g7`.
pub const TLS_IE_ADD: u32 = 71;
/// `%tle_hix22()`: the high 22 bits of a variable's offset from the thread
/// pointer, exclusive-ored so that `%tle_lox10()` can complete it.
pub const TLS_LE_HIX22: u32 = 72;
/// `%tle_lox10()`: the low 10 bits of the same.
pub const TLS_LE_LOX10: u32 = 73;

/// The absolute relocation for an `n`-byte data reference.
pub fn abs(n: u8) -> Option<u32> {
    Some(match n {
        1 => ABS8,
        2 => ABS16,
        4 => ABS32,
        8 => ABS64,
        _ => return None,
    })
}

/// The PC-relative relocation for an `n`-byte data reference.
pub fn pcrel(n: u8) -> Option<u32> {
    Some(match n {
        1 => DISP8,
        2 => DISP16,
        4 => DISP32,
        8 => DISP64,
        _ => return None,
    })
}
