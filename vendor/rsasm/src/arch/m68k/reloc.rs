//! ELF relocation numbers for `EM_68K`, as `m68k-elf-objdump -r` names them.

pub const R_68K_32: u32 = 1;
pub const R_68K_16: u32 = 2;
pub const R_68K_8: u32 = 3;
pub const R_68K_PC32: u32 = 4;
pub const R_68K_PC16: u32 = 5;
pub const R_68K_PC8: u32 = 6;

pub const R_68K_GOT32: u32 = 7;
pub const R_68K_GOT16: u32 = 8;
pub const R_68K_GOT8: u32 = 9;
pub const R_68K_GOT32O: u32 = 10;
pub const R_68K_GOT16O: u32 = 11;
pub const R_68K_GOT8O: u32 = 12;
pub const R_68K_PLT32: u32 = 13;
pub const R_68K_PLT16: u32 = 14;
pub const R_68K_PLT8: u32 = 15;
pub const R_68K_PLT32O: u32 = 16;
pub const R_68K_PLT16O: u32 = 17;
pub const R_68K_PLT8O: u32 = 18;

pub const R_68K_TLS_GD32: u32 = 25;
pub const R_68K_TLS_GD16: u32 = 26;
pub const R_68K_TLS_GD8: u32 = 27;
pub const R_68K_TLS_LDM32: u32 = 28;
pub const R_68K_TLS_LDM16: u32 = 29;
pub const R_68K_TLS_LDM8: u32 = 30;
pub const R_68K_TLS_LDO32: u32 = 31;
pub const R_68K_TLS_LDO16: u32 = 32;
pub const R_68K_TLS_LDO8: u32 = 33;
pub const R_68K_TLS_IE32: u32 = 34;
pub const R_68K_TLS_IE16: u32 = 35;
pub const R_68K_TLS_IE8: u32 = 36;
pub const R_68K_TLS_LE32: u32 = 37;
pub const R_68K_TLS_LE16: u32 = 38;
pub const R_68K_TLS_LE8: u32 = 39;

/// The relocation for a `size`-byte reference.
pub fn data(size: u8, pcrel: bool) -> Option<u32> {
    Some(match (size, pcrel) {
        (4, false) => R_68K_32,
        (2, false) => R_68K_16,
        (1, false) => R_68K_8,
        (4, true) => R_68K_PC32,
        (2, true) => R_68K_PC16,
        (1, true) => R_68K_PC8,
        _ => return None,
    })
}

/// Every `@` suffix GNU as reads on an m68k operand — its `enum
/// pic_relocation` in `m68k-parse.h` — spelled as it is written, with the
/// relocation it takes in a four-, two- and one-byte field.
///
/// The four position-independent ones come in pairs that differ only in where
/// the offset is measured from: `@GOT` and `@PLT` are offsets into the table,
/// which is what an operand relative to the register holding the table's
/// address needs, while `@GOTPC` and `@PLTPC` are offsets to the entry
/// itself. Which of a pair a field belongs to is the suffix's to say, not the
/// addressing mode's: `movel #x@GOTPC,%d0` and `jsr x@GOTPC` both take
/// `R_68K_GOT32`.
///
/// Every suffix has a relocation in all three widths, which is the whole of
/// GNU as's `get_reloc_code`: nothing here depends on the value, and the
/// width alone picks the number.
const SUFFIXES: [(&str, [u32; 3]); 9] = [
    ("got", [R_68K_GOT32O, R_68K_GOT16O, R_68K_GOT8O]),
    ("gotpc", [R_68K_GOT32, R_68K_GOT16, R_68K_GOT8]),
    ("plt", [R_68K_PLT32O, R_68K_PLT16O, R_68K_PLT8O]),
    ("pltpc", [R_68K_PLT32, R_68K_PLT16, R_68K_PLT8]),
    ("tlsgd", [R_68K_TLS_GD32, R_68K_TLS_GD16, R_68K_TLS_GD8]),
    ("tlsldm", [R_68K_TLS_LDM32, R_68K_TLS_LDM16, R_68K_TLS_LDM8]),
    ("tlsldo", [R_68K_TLS_LDO32, R_68K_TLS_LDO16, R_68K_TLS_LDO8]),
    ("tlsie", [R_68K_TLS_IE32, R_68K_TLS_IE16, R_68K_TLS_IE8]),
    ("tlsle", [R_68K_TLS_LE32, R_68K_TLS_LE16, R_68K_TLS_LE8]),
];

/// Whether `name` is one of the nine suffixes.
pub fn is_suffix(name: &str) -> bool {
    SUFFIXES.iter().any(|&(s, _)| s == name)
}

/// Whether `name` is one of the five thread-local access models, which name a
/// variable the linker places rather than an entry in a table.
pub fn is_tls(name: &str) -> bool {
    matches!(name, "tlsgd" | "tlsldm" | "tlsldo" | "tlsie" | "tlsle")
}

/// The relocation the suffix `name` takes in a `size`-byte field.
///
/// A suffix has no PC-relative relocation of its own: what the linker
/// computes is the same either way, and GNU as writes the same number for a
/// field it measures from the instruction.
pub fn suffix(name: &str, size: u8) -> Option<u32> {
    let i = match size {
        4 => 0,
        2 => 1,
        1 => 2,
        _ => return None,
    };
    SUFFIXES
        .iter()
        .find(|&&(s, _)| s == name)
        .map(|&(_, r)| r[i])
}
