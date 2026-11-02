//! Mach-O relocatable objects (`MH_OBJECT`, thin or in a universal file), read as the linker
//! reads one on its link line: `blaze_link::fuzz_read(Format::MachO, arch, NativeKind::Object,
//! bytes)`, for x86-64 and AArch64.
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - the read returns, accepting or refusing, and never panics (a read past the input is a
//!   slice index out of range in the plain test, and ASan's report in the fuzz build);
//! - the largest single allocation of a read is at most `blaze_fuzz::allowed(len)`;
//! - the bytes at an address 8 does not divide read as they do 8-aligned (the reader copies
//!   them to read `object`'s structures in place);
//! - an accepted object is still accepted with 64 bytes after its end: its sections, symbols,
//!   strings and relocations are all named by an offset and a size inside the input.

#![forbid(unsafe_code)]

use std::time::Duration;

use blaze_codegen::Arch;
use blaze_fuzzer::Target;
use blaze_link::{Format, NativeKind};
use object::write::{Object as Builder, Relocation, StandardSection, Symbol, SymbolId, SymbolSection};
use object::{
    Architecture, BinaryFormat, Endianness, RelocationFlags, SectionFlags, SectionKind, SymbolFlags, SymbolKind,
    SymbolScope, macho,
};

const TARGET: Target = Target {
    name: "link_macho_object",
    check,
    seeds,
    dictionary: &[
        b"\xcf\xfa\xed\xfe",
        b"\xca\xfe\xba\xbe",
        b"\xca\xfe\xba\xbf",
        b"\x07\x00\x00\x01",
        b"\x0c\x00\x00\x01",
        b"\x01\x00\x00\x00",
        b"\x19\x00\x00\x00",
        b"\x02\x00\x00\x00",
        b"\x2d\x00\x00\x00",
        b"\x00\x20\x00\x00",
        b"__TEXT\0\0\0\0\0\0\0\0\0\0",
        b"__DATA\0\0\0\0\0\0\0\0\0\0",
        b"__DATA_CONST\0\0\0\0",
        b"__LD\0\0\0\0\0\0\0\0\0\0\0\0",
        b"__text\0\0\0\0\0\0\0\0\0\0",
        b"__eh_frame\0\0\0\0\0\0",
        b"__compact_unwind",
        b"__mod_init_func\0",
        b"__thread_vars\0\0\0",
        b"__cstring\0\0\0\0\0\0\0",
        b"-framework\0",
        b"ltmp0",
        b"_main\0",
    ],
    max_len: 1 << 16,
    budget: Duration::from_secs(600),
};

const OBJECT: NativeKind = NativeKind::Object;
const ARCHES: [Arch; 2] = [Arch::X86_64, Arch::Aarch64];

/// An arch and the read compared with the first (shifted, padded), one a run
/// (`blaze_fuzzer::case`); the plain test takes every one of each seed.
const CASES: usize = ARCHES.len() * 2;

fn check(data: &[u8]) {
    check_case(data, blaze_fuzzer::case(data, CASES));
}

fn check_case(data: &[u8], case: usize) {
    let arch = ARCHES[case % ARCHES.len()];
    let (aligned, at) = copy_at(data, 0, 0);
    let aligned = &aligned[at..][..data.len()];
    let bound = blaze_fuzz::allowed(data.len());
    let (largest, read) =
        blaze_fuzz::largest_allocation(|| blaze_link::fuzz_read(Format::MachO, arch, OBJECT, aligned));
    assert!(
        largest <= bound,
        "allocation bound: a {arch:?} read of {} bytes asked for {largest} bytes at once, past {bound}",
        data.len()
    );
    match case / ARCHES.len() {
        0 => {
            let (shifted, at) = copy_at(data, 1, 0);
            let shifted = &shifted[at..][..data.len()];
            assert_eq!(
                blaze_link::fuzz_read(Format::MachO, arch, OBJECT, shifted),
                read,
                "alignment: a {arch:?} read of the bytes one past an 8-aligned address answers otherwise"
            );
        }
        _ => {
            if read {
                let (padded, at) = copy_at(data, 0, 64);
                let padded = &padded[at..][..data.len() + 64];
                assert!(
                    blaze_link::fuzz_read(Format::MachO, arch, OBJECT, padded),
                    "inside the input: a {arch:?} read accepted the bytes and refuses them with 64 more after"
                );
            }
        }
    }
}

/// `data` copied to `shift` past an 8-aligned address of a buffer with `extra` bytes of `0xff`
/// after it, and where it starts in the buffer.
fn copy_at(data: &[u8], shift: usize, extra: usize) -> (Vec<u8>, usize) {
    let mut buffer = vec![0xff; data.len() + extra + 16];
    let start = (8 - buffer.as_ptr() as usize % 8) % 8 + shift;
    buffer[start..start + data.len()].copy_from_slice(data);
    (buffer, start)
}

fn seeds() -> Vec<Vec<u8>> {
    named().into_iter().map(|(_, _, bytes)| bytes).collect()
}

fn named() -> Vec<(&'static str, Arch, Vec<u8>)> {
    let x86 = object(Architecture::X86_64, true);
    let arm = object(Architecture::Aarch64, true);
    let both = fat(&[(macho::CPU_TYPE_X86_64.0, 3, &x86[..]), (macho::CPU_TYPE_ARM64.0, 0, &arm[..])]);
    vec![
        ("an x86-64 object of every section and relocation read", Arch::X86_64, x86),
        ("an AArch64 object of every section and relocation read", Arch::Aarch64, arm),
        ("an x86-64 object without subsections", Arch::X86_64, object(Architecture::X86_64, false)),
        ("an AArch64 object without subsections", Arch::Aarch64, object(Architecture::Aarch64, false)),
        ("a universal file of both, its x86-64 slice", Arch::X86_64, both.clone()),
        ("a universal file of both, its AArch64 slice", Arch::Aarch64, both),
        ("an x86-64 object of linker options only", Arch::X86_64, options(macho::CPU_TYPE_X86_64.0)),
        ("an AArch64 object of linker options only", Arch::Aarch64, options(macho::CPU_TYPE_ARM64.0)),
    ]
}

const NOP: u32 = 0xd503_201f;
const EH_FRAME: macho::SectionFlags = macho::S_COALESCED
    .to_flags()
    .with(macho::S_ATTR_NO_TOC)
    .with(macho::S_ATTR_STRIP_STATIC_SYMS)
    .with(macho::S_ATTR_LIVE_SUPPORT);

/// `_f` at 0 and `_g` at 32 of `__text`; `_f` calls `_g`, takes `_v`'s address and loads
/// `_ext`'s from the GOT. `__data` holds `&_f` and `_g - _f`; `__bss`, `__cstring`, a
/// `__compact_unwind` entry for `_f` and (x86-64) an `__eh_frame` FDE for `_g` whose
/// `pc_begin` has no relocation.
fn object(arch: Architecture, subsections: bool) -> Vec<u8> {
    let x86 = arch == Architecture::X86_64;
    let mut b = Builder::new(BinaryFormat::MachO, arch, Endianness::Little);
    if subsections {
        b.set_subsections_via_symbols();
    }
    let text = b.section_id(StandardSection::Text);
    let code: Vec<u8> = if x86 {
        let mut code = vec![0xe8, 0, 0, 0, 0, 0x48, 0x8d, 0x05, 0, 0, 0, 0, 0x48, 0x8b, 0x05, 0, 0, 0, 0, 0xc3];
        code.resize(32, 0x90);
        code.push(0xc3);
        code.resize(48, 0x90);
        code
    } else {
        let words = [
            0x9400_0000u32,
            0x9000_0000,
            0x9100_0000,
            0x9000_0001,
            0xf940_0021,
            0xd65f_03c0,
            NOP,
            NOP,
            0xd65f_03c0,
            NOP,
            NOP,
            NOP,
        ];
        words.iter().flat_map(|it| it.to_le_bytes()).collect()
    };
    b.append_section_data(text, &code, 16);
    if x86 {
        let frame = b.add_section(b"__TEXT".to_vec(), b"__eh_frame".to_vec(), SectionKind::ReadOnlyData);
        b.section_mut(frame).flags = SectionFlags::MachO { flags: EH_FRAME, reserved2: 0 };
        let mut eh = Vec::new();
        eh.extend_from_slice(&16u32.to_le_bytes());
        eh.extend_from_slice(&0u32.to_le_bytes());
        eh.extend_from_slice(&[1, b'z', b'R', 0, 1, 0x78, 0x10, 1, 0x10, 0, 0, 0]);
        // The FDE's pc_begin is at 28 of `__eh_frame`, which is at 48: `_g`, at 32.
        eh.extend_from_slice(&24u32.to_le_bytes());
        eh.extend_from_slice(&24u32.to_le_bytes());
        eh.extend_from_slice(&(32i64 - 76).to_le_bytes());
        eh.extend_from_slice(&16u64.to_le_bytes());
        eh.extend_from_slice(&[0; 4]);
        b.append_section_data(frame, &eh, 8);
    }
    let data = b.section_id(StandardSection::Data);
    b.append_section_data(data, &[0; 16], 8);
    let bss = b.section_id(StandardSection::UninitializedData);
    b.append_section_bss(bss, 16, 8);
    let strings = b.section_id(StandardSection::ReadOnlyString);
    b.append_section_data(strings, b"hello\0", 1);
    let unwind = b.add_section(b"__LD".to_vec(), b"__compact_unwind".to_vec(), SectionKind::Metadata);
    b.section_mut(unwind).flags = SectionFlags::MachO { flags: macho::S_ATTR_DEBUG, reserved2: 0 };
    let mut entry = vec![0; 8];
    entry.extend_from_slice(&20u32.to_le_bytes());
    entry.extend_from_slice(&0x0200_0000u32.to_le_bytes());
    entry.resize(32, 0);
    b.append_section_data(unwind, &entry, 8);

    let mut define = |name: &str, section, value, size, kind| {
        b.add_symbol(Symbol {
            name: name.into(),
            value,
            size,
            kind,
            scope: SymbolScope::Linkage,
            weak: false,
            section: SymbolSection::Section(section),
            flags: SymbolFlags::None,
        })
    };
    let f = define("f", text, 0, 32, SymbolKind::Text);
    let g = define("g", text, 32, 16, SymbolKind::Text);
    let v = define("v", data, 0, 8, SymbolKind::Data);
    define("z", bss, 0, 16, SymbolKind::Data);
    let ext = b.add_symbol(Symbol {
        name: b"ext".to_vec(),
        value: 0,
        size: 0,
        kind: SymbolKind::Data,
        scope: SymbolScope::Linkage,
        weak: false,
        section: SymbolSection::Undefined,
        flags: SymbolFlags::None,
    });

    let mut relocate = |section, offset, symbol: SymbolId, r_type, r_pcrel, r_length, addend| {
        let flags = RelocationFlags::MachO { r_type, r_pcrel, r_length };
        b.add_relocation(section, Relocation { offset, symbol, addend, flags }).expect("a Mach-O relocation");
    };
    if x86 {
        relocate(text, 1, g, macho::X86_64_RELOC_BRANCH, true, 2, -4);
        relocate(text, 8, v, macho::X86_64_RELOC_SIGNED, true, 2, -4);
        relocate(text, 15, ext, macho::X86_64_RELOC_GOT_LOAD, true, 2, -4);
    } else {
        relocate(text, 0, g, macho::ARM64_RELOC_BRANCH26, true, 2, 0);
        relocate(text, 4, v, macho::ARM64_RELOC_PAGE21, true, 2, 0);
        relocate(text, 8, v, macho::ARM64_RELOC_PAGEOFF12, false, 2, 0);
        relocate(text, 12, ext, macho::ARM64_RELOC_GOT_LOAD_PAGE21, true, 2, 0);
        relocate(text, 16, ext, macho::ARM64_RELOC_GOT_LOAD_PAGEOFF12, false, 2, 0);
    }
    let (subtractor, unsigned) = if x86 {
        (macho::X86_64_RELOC_SUBTRACTOR, macho::X86_64_RELOC_UNSIGNED)
    } else {
        (macho::ARM64_RELOC_SUBTRACTOR, macho::ARM64_RELOC_UNSIGNED)
    };
    // `object` writes a section's relocations reversed when the first is at a lower offset than
    // the last: these, from the highest, are written as given, the subtractor before its pair.
    relocate(data, 8, f, subtractor, false, 3, 0);
    relocate(data, 8, g, unsigned, false, 3, 0);
    relocate(data, 0, f, unsigned, false, 3, 0);
    relocate(unwind, 0, f, unsigned, false, 3, 0);
    b.write().expect("a Mach-O object")
}

/// A universal (`FAT_MAGIC`) file of `slices` (CPU type, subtype, thin file), each 8-aligned.
fn fat(slices: &[(u32, u32, &[u8])]) -> Vec<u8> {
    let mut at = (8 + 20 * slices.len()).next_multiple_of(8);
    let mut out = Vec::new();
    out.extend_from_slice(&macho::FAT_MAGIC.to_be_bytes());
    out.extend_from_slice(&(slices.len() as u32).to_be_bytes());
    for (cpu, subtype, bytes) in slices {
        for value in [*cpu, *subtype, at as u32, bytes.len() as u32, 3] {
            out.extend_from_slice(&value.to_be_bytes());
        }
        at = (at + bytes.len()).next_multiple_of(8);
    }
    for (_, _, bytes) in slices {
        out.resize(out.len().next_multiple_of(8), 0);
        out.extend_from_slice(bytes);
    }
    out
}

/// An object of no section or symbol, only `LC_LINKER_OPTION`s (`-lz`, `-framework
/// CoreFoundation`) and an empty `LC_SYMTAB`.
fn options(cpu: u32) -> Vec<u8> {
    let mut commands = Vec::new();
    for words in [&["-lz"][..], &["-framework", "CoreFoundation"]] {
        let strings: Vec<u8> = words.iter().flat_map(|it| it.bytes().chain([0])).collect();
        let size = (12 + strings.len()).next_multiple_of(8);
        let start = commands.len();
        for value in [macho::LC_LINKER_OPTION.0, size as u32, words.len() as u32] {
            commands.extend_from_slice(&value.to_le_bytes());
        }
        commands.extend_from_slice(&strings);
        commands.resize(start + size, 0);
    }
    commands.extend_from_slice(&macho::LC_SYMTAB.0.to_le_bytes());
    commands.extend_from_slice(&24u32.to_le_bytes());
    commands.extend_from_slice(&[0; 16]);
    let mut out = Vec::new();
    for value in [macho::MH_MAGIC_64, cpu, 0, macho::MH_OBJECT.0, 3, commands.len() as u32, 0, 0] {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out.extend_from_slice(&commands);
    out
}

#[cfg_attr(test, allow(dead_code))]
fn main() -> std::process::ExitCode {
    blaze_fuzzer::main(&TARGET)
}

#[cfg(test)]
mod tests {
    use blaze_link::Format;

    use super::{OBJECT, named};

    #[test]
    fn seeds() {
        blaze_fuzzer::seeds(&super::TARGET);
    }

    #[test]
    fn every_case() {
        blaze_fuzzer::every_case(&super::TARGET, super::CASES, super::check_case);
    }

    #[test]
    fn seeds_read() {
        for (what, arch, bytes) in named() {
            assert!(blaze_link::fuzz_read(Format::MachO, arch, OBJECT, &bytes), "seed `{what}` is refused");
        }
    }
}
