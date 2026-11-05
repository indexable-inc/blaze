//! ELF relocation types for MIPS (`R_MIPS_*`).
//!
//! The numbers come from the MIPS psABI. Only the handful this backend can
//! actually produce are listed.
//!
//! # Composite types
//!
//! n64 is the one ABI whose `r_info` holds three relocation types rather
//! than one, applied innermost first, and the nested operators are what it
//! exists for: `%hi(%neg(%gp_rel(x)))` is `GPREL16` negated by `SUB` and
//! then narrowed to a high half by `HI16`. [`compose`] packs such a chain
//! into the single number a fixup carries, one type per byte, and the ELF
//! writer unpacks it into the `r_type`, `r_type2` and `r_type3` of an n64
//! entry; an o32 entry has room for the first alone, which is why GNU as
//! reads only one operator there.

pub const R16: u32 = 1;
pub const R32: u32 = 2;
/// The 26-bit field of `j` / `jal`, holding a word index into the current
/// 256 MB region rather than a displacement.
pub const R26: u32 = 4;
/// High half of a 32-bit address, biased so that a sign-extended `LO16`
/// added to it lands on the right value.
pub const HI16: u32 = 5;
pub const LO16: u32 = 6;
/// The offset of `x` from `$gp`, which only the linker knows.
pub const GPREL16: u32 = 7;
/// The offset from `$gp` of the GOT entry holding `x`. Against a local
/// symbol it names the entry for the symbol's *page*, which a `%lo`
/// completes; against a global one, the entry for the symbol itself.
pub const GOT16: u32 = 9;
/// The 16-bit word displacement of a conditional branch.
pub const PC16: u32 = 10;
/// The same as [`GOT16`], for an entry a call goes through, which the
/// linker may point at a stub.
pub const CALL16: u32 = 11;
/// A 32-bit offset from `$gp`, which `.gpword` writes.
pub const GPREL32: u32 = 12;
pub const R64: u32 = 18;
/// The offset of the GOT entry holding `x`, in an ABI whose GOT entry is a
/// whole address rather than a page: n32 and n64's `%got_disp`.
pub const GOT_DISP: u32 = 19;
/// The entry for the page `x` is in, and `x`'s offset within that page, for
/// a local symbol under the new ABIs.
pub const GOT_PAGE: u32 = 20;
pub const GOT_OFST: u32 = 21;
/// The halves of a GOT offset too large for one field, which `%got_hi` and
/// `%got_lo` split an address of the GOT itself into.
pub const GOT_HI16: u32 = 22;
pub const GOT_LO16: u32 = 23;
/// Negation, which only ever composes another type: GNU as stops with an
/// internal error on a `%neg` that wraps nothing else.
pub const SUB: u32 = 24;
pub const HIGHER: u32 = 28;
pub const HIGHEST: u32 = 29;
/// The halves of a call's GOT offset, as [`GOT_HI16`] and [`GOT_LO16`] are
/// of an ordinary one.
pub const CALL_HI16: u32 = 30;
pub const CALL_LO16: u32 = 31;
pub const PC32: u32 = 248;

/// The thread-local access models, which are the same numbers in all three
/// ABIs: n64 packs three relocation types into one `r_info`, but every
/// thread-local operator fills only the first and leaves the other two
/// `R_MIPS_NONE`, as both references do.
///
/// `TLS_GD` and `TLS_LDM` name the two-word GOT entry `__tls_get_addr` is
/// given, for general dynamic and for local dynamic; `TLS_GOTTPREL` the
/// one-word entry initial exec reads. The `DTPREL` halves are a variable's
/// offset within its module's block, which local dynamic adds to what the
/// call returned, and the `TPREL` halves its offset from the thread pointer,
/// which only local exec knows without asking.
pub const TLS_GD: u32 = 42;
pub const TLS_LDM: u32 = 43;
pub const TLS_DTPREL_HI16: u32 = 44;
pub const TLS_DTPREL_LO16: u32 = 45;
pub const TLS_GOTTPREL: u32 = 46;
pub const TLS_TPREL_HI16: u32 = 49;
pub const TLS_TPREL_LO16: u32 = 50;

/// The offsets `.dtprelword`, `.dtpreldword` and `.tprelword` write: a
/// variable's place within its module's block, and its place relative to
/// the thread pointer, as a whole word rather than as halves of an
/// instruction field. `R_MIPS_TLS_TPREL64`, the fourth of the set, has no
/// directive here: GNU as 2.47 aborts on `.tpreldword`, so there is nothing
/// to check one against.
pub const TLS_DTPREL32: u32 = 39;
pub const TLS_DTPREL64: u32 = 41;
pub const TLS_TPREL32: u32 = 47;

/// Packs a chain of relocation types, innermost first, into the one number
/// a fixup carries; see the module note. Only n64 has room for more than
/// the first.
pub fn compose(types: &[u32]) -> u32 {
    let mut packed = 0;
    for (i, t) in types.iter().enumerate().take(3) {
        packed |= t << (8 * i);
    }
    packed
}

/// The absolute relocation for an `n`-byte data reference.
pub fn abs(n: u8) -> Option<u32> {
    Some(match n {
        2 => R16,
        4 => R32,
        8 => R64,
        _ => return None,
    })
}

/// The PC-relative relocation for an `n`-byte data reference. MIPS has no
/// PC-relative data relocation narrower than 32 bits.
pub fn pcrel(n: u8) -> Option<u32> {
    match n {
        4 => Some(PC32),
        _ => None,
    }
}
