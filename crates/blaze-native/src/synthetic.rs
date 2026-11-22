//! The names a linker defines itself: no object of the link defines them, the linker gives each
//! an address in the image it writes (NATIVE.md 3.3). One closed table: a name is one of these
//! or it is an ordinary name (defined by a member, a Rust word, or an import).
//!
//! What each is, as lld's MachO port and ld64 make them. Read from memory of lld's source
//! (`lld/MachO/Driver.cpp` `createSyntheticSymbols`, `lld/MachO/SymbolTable.cpp`
//! `handleSectionBoundarySymbol` / `handleSegmentBoundarySymbol`, `lld/ELF/Writer.cpp`
//! `addReservedSymbols`); NOT checked against the source in this tree (none is vendored, and
//! this lane has no network): each row says so where it matters.
//!
//! | name (as the symbol table spells it) | platform | address |
//! |---|---|---|
//! | `___dso_handle` | Mach-O | the image's Mach-O header (address 0 of the image): C and C++ static initializers pass it to `__cxa_atexit`, tying a destructor to its image. lld: a private-extern header symbol in every output kind |
//! | `__mh_dylib_header` | Mach-O | the header too, defined only in a dylib, which this image is (W4 native_dylib.rs) |
//! | `__mh_execute_header` | Mach-O | an executable's header: refused by name in a dylib image |
//! | `__mh_bundle_header` | Mach-O | a bundle's header: refused by name in a dylib image |
//! | `section$start$<seg>$<sect>`, `section$end$<seg>$<sect>` | Mach-O | ld64's boundary names: the first byte, and the byte past the last, of the laid section `<seg>,<sect>` (written with no leading `_`: C names them by `__asm("section$start$...")`). lld creates an empty section when none is there; here a name whose section is not laid is refused by name (layout.rs `NoAddress`) |
//! | `segment$start$<seg>`, `segment$end$<seg>` | Mach-O | the bounds of segment `<seg>` of the written dylib: `__TEXT`, `__DATA_CONST` (the GOT, when there is one), `__DATA` (native_dylib.rs's segment table) |
//! | `__dso_handle` | ELF | the image's ELF header (address 0), hidden (lld ELF: `ElfSym`, `__dso_handle` at the ELF header). The other ELF reserved names (`__ehdr_start`, `__start_<sec>`, `__stop_<sec>`, `_etext`, `_end`) are not in the table: W5 is not built (a named gap) |
//!
//! The table is consulted only for a name that no pulled member defines and no Rust word is
//! (resolve.rs `finish`): a member defining one of these names wins, as a definition does in
//! lld (a synthetic symbol is added only where the name is still undefined).

use crate::Platform;

/// A linker-defined name, parsed: what [`of`] read from the name's bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Synthetic<'n> {
    /// `___dso_handle` (Mach-O), `__dso_handle` (ELF): the image's header.
    DsoHandle,
    /// `__mh_dylib_header`: the header of a dylib image.
    DylibHeader,
    /// The header of an image of another kind than a dylib: `kind` names it (an executable's
    /// `__mh_execute_header`, a bundle's `__mh_bundle_header`). Refused in this image.
    OtherHeader { kind: &'static str },
    /// `section$start$<segment>$<section>` (`end` false) or `section$end$...` (`end` true).
    Section { segment: &'n [u8], section: &'n [u8], end: bool },
    /// `segment$start$<segment>` or `segment$end$<segment>`.
    Segment { segment: &'n [u8], end: bool },
}

/// `name` as a linker-defined name of `platform`'s images, or `None` for an ordinary name.
pub fn of(platform: Platform, name: &[u8]) -> Option<Synthetic<'_>> {
    match platform {
        Platform::LinuxArm64 => (name == b"__dso_handle").then_some(Synthetic::DsoHandle),
        Platform::MacosArm64 => macho(name),
    }
}

fn macho(name: &[u8]) -> Option<Synthetic<'_>> {
    match name {
        b"___dso_handle" => return Some(Synthetic::DsoHandle),
        b"__mh_dylib_header" => return Some(Synthetic::DylibHeader),
        b"__mh_execute_header" => return Some(Synthetic::OtherHeader { kind: "an executable" }),
        b"__mh_bundle_header" => return Some(Synthetic::OtherHeader { kind: "a bundle" }),
        _ => {}
    }
    if let Some(rest) = name.strip_prefix(b"section$start$") {
        return section(rest, false);
    }
    if let Some(rest) = name.strip_prefix(b"section$end$") {
        return section(rest, true);
    }
    if let Some(rest) = name.strip_prefix(b"segment$start$") {
        return segment(rest, false);
    }
    if let Some(rest) = name.strip_prefix(b"segment$end$") {
        return segment(rest, true);
    }
    None
}

/// `<segment>$<section>`, both non-empty (a segment name holds no `$`).
fn section(rest: &[u8], end: bool) -> Option<Synthetic<'_>> {
    let at = rest.iter().position(|&b| b == b'$')?;
    let (segment, section) = (&rest[..at], &rest[at + 1..]);
    (!segment.is_empty() && !section.is_empty()).then_some(Synthetic::Section { segment, section, end })
}

/// `<segment>`, non-empty, with no `$`.
fn segment(rest: &[u8], end: bool) -> Option<Synthetic<'_>> {
    (!rest.is_empty() && !rest.contains(&b'$')).then_some(Synthetic::Segment { segment: rest, end })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each row of the table parses to what it is, and a near miss stays an ordinary name.
    #[test]
    fn the_table_parses_each_name_and_nothing_else() {
        let mac = |n: &'static [u8]| of(Platform::MacosArm64, n);
        assert_eq!(mac(b"___dso_handle"), Some(Synthetic::DsoHandle));
        assert_eq!(mac(b"__mh_dylib_header"), Some(Synthetic::DylibHeader));
        assert_eq!(mac(b"__mh_execute_header"), Some(Synthetic::OtherHeader { kind: "an executable" }));
        assert_eq!(mac(b"section$start$__DATA$__mod_init_func"), Some(Synthetic::Section { segment: b"__DATA", section: b"__mod_init_func", end: false }));
        assert_eq!(mac(b"section$end$__TEXT$__cstring"), Some(Synthetic::Section { segment: b"__TEXT", section: b"__cstring", end: true }));
        assert_eq!(mac(b"segment$end$__DATA"), Some(Synthetic::Segment { segment: b"__DATA", end: true }));
        // Controls: the ELF spelling on Mach-O, a boundary with no section, a plain C name.
        assert_eq!(mac(b"__dso_handle"), None);
        assert_eq!(mac(b"section$start$__DATA"), None);
        assert_eq!(mac(b"segment$start$__DATA$__data"), None);
        assert_eq!(mac(b"_dso_handle_user"), None);
        assert_eq!(of(Platform::LinuxArm64, b"__dso_handle"), Some(Synthetic::DsoHandle));
        assert_eq!(of(Platform::LinuxArm64, b"___dso_handle"), None);
    }
}
