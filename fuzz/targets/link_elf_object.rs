//! ELF relocatable objects (`ET_REL`), read as the linker reads a C object of the link line:
//! `blaze_link::fuzz_read(Format::Elf, arch, NativeKind::Object, bytes)`, for x86-64 and AArch64.
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - the read returns, accepting or refusing, and never panics (a read past the input is a
//!   slice index out of range in the plain test, and ASan's report in the fuzz build);
//! - the largest single allocation of a read is at most `blaze_fuzz::allowed(len)`;
//! - the bytes at an address 8 does not divide read as they do 8-aligned (the reader copies
//!   them for `object`'s structs);
//! - an accepted input is still accepted with 64 bytes after its end: every structure is named
//!   by an offset and a size, so nothing past them is read.

#![forbid(unsafe_code)]

use std::time::Duration;

use blaze_codegen::Arch;
use blaze_fuzzer::Target;
use blaze_link::{Format, NativeKind};
use object::write::{Comdat, Object as Builder, Relocation, SectionId, StandardSection, Symbol, SymbolId, SymbolSection};
use object::{
    Architecture, BinaryFormat, ComdatKind, Endianness, RelocationFlags, SectionFlags, SectionKind, SymbolFlags,
    SymbolKind, SymbolScope, elf,
};

const TARGET: Target = Target {
    name: "link_elf_object",
    check,
    seeds,
    dictionary: &[
        b"\x7fELF\x02\x01\x01",
        b"\x01\x00",
        b"\x3e\x00",
        b"\xb7\x00",
        b".text",
        b".data",
        b".bss",
        b".tdata",
        b".tbss",
        b".rodata",
        b".eh_frame",
        b".init_array",
        b".data.rel.ro",
        b".group",
        b".symtab",
        b".strtab",
        b".rela.text",
        b".debug_info",
        b".note.GNU-stack",
        b"_GLOBAL_OFFSET_TABLE_",
        b"\xff\xff",
        b"\xf1\xff",
        b"\xf2\xff",
    ],
    max_len: 1 << 16,
    budget: Duration::from_secs(600),
};

const KIND: NativeKind = NativeKind::Object;
const ARCHES: [Arch; 2] = [Arch::X86_64, Arch::Aarch64];

/// An arch and the read compared with its first (shifted, padded), one a run
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
    let (largest, read) = blaze_fuzz::largest_allocation(|| blaze_link::fuzz_read(Format::Elf, arch, KIND, aligned));
    assert!(
        largest <= bound,
        "allocation bound: a {arch:?} read of {} bytes asked for {largest} bytes at once, past {bound}",
        data.len()
    );
    if case / ARCHES.len() == 0 {
        let (shifted, at) = copy_at(data, 1, 0);
        let shifted = &shifted[at..][..data.len()];
        assert_eq!(
            blaze_link::fuzz_read(Format::Elf, arch, KIND, shifted),
            read,
            "alignment: a {arch:?} read of the bytes one past an 8-aligned address answers otherwise"
        );
    } else if read {
        let (padded, at) = copy_at(data, 0, 64);
        let padded = &padded[at..][..data.len() + 64];
        assert!(
            blaze_link::fuzz_read(Format::Elf, arch, KIND, padded),
            "inside the input: a {arch:?} read accepted the bytes and refuses them with 64 more after"
        );
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

/// Objects `object`'s writer makes with every kind of section, symbol and relocation the
/// reader distinguishes, as the reader's own tests build them.
fn seeds() -> Vec<Vec<u8>> {
    named().into_iter().map(|(_, _, bytes)| bytes).collect()
}

fn named() -> Vec<(&'static str, Arch, Vec<u8>)> {
    let x86_64 = x86_64();
    let mut stripped = x86_64.clone();
    // No section headers: `e_shoff`, `e_shnum` and `e_shstrndx` zeroed.
    stripped[40..48].fill(0);
    stripped[60..64].fill(0);
    vec![
        ("x86-64, every kind of section and relocation", Arch::X86_64, x86_64),
        ("AArch64, every kind of relocation", Arch::Aarch64, aarch64()),
        ("x86-64, one function and no relocations", Arch::X86_64, minimal(Architecture::X86_64)),
        ("AArch64, one function and no relocations", Arch::Aarch64, minimal(Architecture::Aarch64)),
        ("x86-64, no section headers", Arch::X86_64, stripped),
    ]
}

fn builder(arch: Architecture) -> Builder<'static> {
    Builder::new(BinaryFormat::Elf, arch, Endianness::Little)
}

fn define(
    b: &mut Builder<'_>,
    name: &str,
    section: SectionId,
    value: u64,
    size: u64,
    kind: SymbolKind,
    scope: SymbolScope,
    weak: bool,
) -> SymbolId {
    b.add_symbol(Symbol {
        name: name.into(),
        value,
        size,
        kind,
        scope,
        weak,
        section: SymbolSection::Section(section),
        flags: SymbolFlags::None,
    })
}

fn undefined(b: &mut Builder<'_>, name: &str, weak: bool) -> SymbolId {
    b.add_symbol(Symbol {
        name: name.into(),
        value: 0,
        size: 0,
        kind: SymbolKind::Unknown,
        scope: SymbolScope::Dynamic,
        weak,
        section: SymbolSection::Undefined,
        flags: SymbolFlags::None,
    })
}

fn tls(b: &mut Builder<'_>, name: &str) -> SymbolId {
    let tbss = b.section_id(StandardSection::UninitializedTls);
    let symbol = b.add_symbol(Symbol {
        name: name.into(),
        value: 0,
        size: 0,
        kind: SymbolKind::Tls,
        scope: SymbolScope::Dynamic,
        weak: false,
        section: SymbolSection::Undefined,
        flags: SymbolFlags::None,
    });
    b.add_symbol_bss(symbol, tbss, 8, 8);
    symbol
}

fn relocate(b: &mut Builder<'_>, section: SectionId, at: u64, symbol: SymbolId, addend: i64, r_type: elf::RelocationType) {
    b.add_relocation(section, Relocation { offset: at, symbol, addend, flags: RelocationFlags::Elf { r_type } })
        .expect("a relocation inside its section");
}

/// Sets the `sh_flags` of `section`, keeping the `sh_type` its kind gives it.
fn flags(b: &mut Builder<'_>, section: SectionId, sh_flags: elf::SectionFlags) {
    let SectionFlags::Elf { sh_flags: flags, .. } = b.section_flags_mut(section) else {
        panic!("a section with no ELF flags")
    };
    *flags = sh_flags;
}

fn x86_64() -> Vec<u8> {
    let mut b = builder(Architecture::X86_64);
    let text = b.section_id(StandardSection::Text);
    b.append_section_data(text, &[0x90; 64], 16);
    let data = b.section_id(StandardSection::Data);
    b.append_section_data(data, &[0; 16], 8);
    let inline = b.add_section(Vec::new(), b".text.inl".to_vec(), SectionKind::Text);
    b.append_section_data(inline, &[0xc3], 1);
    let keep = b.add_section(Vec::new(), b".data.keep".to_vec(), SectionKind::Data);
    b.append_section_data(keep, &[1, 2, 3, 4], 4);
    flags(&mut b, keep, elf::SHF_ALLOC | elf::SHF_WRITE | elf::SHF_GNU_RETAIN);
    let init = b.add_section(Vec::new(), b".init_array.00100".to_vec(), SectionKind::Data);
    b.append_section_data(init, &[0; 8], 8);
    b.section_mut(init).flags = SectionFlags::Elf { sh_type: elf::SHT_INIT_ARRAY, sh_flags: elf::SHF_ALLOC | elf::SHF_WRITE };
    let eh_frame = b.add_section(Vec::new(), b".eh_frame".to_vec(), SectionKind::ReadOnlyData);
    b.append_section_data(eh_frame, &[0; 24], 8);
    b.section_mut(eh_frame).flags = SectionFlags::Elf { sh_type: elf::SHT_X86_64_UNWIND, sh_flags: elf::SHF_ALLOC };
    let debug = b.add_section(Vec::new(), b".debug_info".to_vec(), SectionKind::Debug);
    b.append_section_data(debug, &[0; 16], 1);

    let f = define(&mut b, "f", text, 0, 16, SymbolKind::Text, SymbolScope::Dynamic, false);
    let g = define(&mut b, "g", text, 16, 8, SymbolKind::Text, SymbolScope::Linkage, false);
    let local = define(&mut b, "local", text, 24, 4, SymbolKind::Text, SymbolScope::Compilation, false);
    let ext = undefined(&mut b, "ext", false);
    let weak = undefined(&mut b, "wk", true);
    let got = undefined(&mut b, "_GLOBAL_OFFSET_TABLE_", false);
    let tvar = tls(&mut b, "tvar");
    let comm = b.add_common_symbol(
        Symbol {
            name: b"comm".to_vec(),
            value: 8,
            size: 0,
            kind: SymbolKind::Data,
            scope: SymbolScope::Dynamic,
            weak: false,
            section: SymbolSection::Undefined,
            flags: SymbolFlags::None,
        },
        16,
        8,
    );
    let inl = define(&mut b, "inl", inline, 0, 1, SymbolKind::Text, SymbolScope::Dynamic, false);
    b.add_comdat(Comdat { kind: ComdatKind::Any, symbol: inl, sections: vec![inline] });

    relocate(&mut b, text, 1, ext, -4, elf::R_X86_64_PLT32);
    relocate(&mut b, text, 6, ext, -4, elf::R_X86_64_REX_GOTPCRELX);
    relocate(&mut b, text, 11, g, -4, elf::R_X86_64_PC32);
    relocate(&mut b, text, 16, tvar, -4, elf::R_X86_64_TLSGD);
    relocate(&mut b, text, 21, tvar, -4, elf::R_X86_64_TLSLD);
    relocate(&mut b, text, 26, tvar, 0, elf::R_X86_64_DTPOFF32);
    relocate(&mut b, text, 30, tvar, -4, elf::R_X86_64_GOTTPOFF);
    relocate(&mut b, text, 34, tvar, 0, elf::R_X86_64_TPOFF32);
    relocate(&mut b, text, 38, got, 2, elf::R_X86_64_GOTPC32);
    relocate(&mut b, text, 42, f, 0, elf::R_X86_64_SIZE32);
    relocate(&mut b, text, 46, comm, 0, elf::R_X86_64_32S);
    relocate(&mut b, text, 50, weak, 0, elf::R_X86_64_PC16);
    relocate(&mut b, text, 52, local, 0, elf::R_X86_64_NONE);
    relocate(&mut b, text, 56, weak, -4, elf::R_X86_64_PLT32);
    relocate(&mut b, data, 0, f, 8, elf::R_X86_64_64);
    relocate(&mut b, data, 8, g, 0, elf::R_X86_64_GOTOFF64);
    relocate(&mut b, init, 0, f, 0, elf::R_X86_64_64);
    let text_symbol = b.section_symbol(text);
    relocate(&mut b, eh_frame, 8, text_symbol, 0, elf::R_X86_64_PC32);
    relocate(&mut b, debug, 0, text_symbol, 0, elf::R_X86_64_GOT64);
    b.write().expect("an ELF object")
}

fn aarch64() -> Vec<u8> {
    let mut b = builder(Architecture::Aarch64);
    let text = b.section_id(StandardSection::Text);
    b.append_section_data(text, &[0; 80], 4);
    let data = b.section_id(StandardSection::Data);
    b.append_section_data(data, &[0; 8], 8);
    let eh_frame = b.add_section(Vec::new(), b".eh_frame".to_vec(), SectionKind::ReadOnlyData);
    b.append_section_data(eh_frame, &[0; 16], 8);
    b.section_mut(eh_frame).flags = SectionFlags::Elf { sh_type: elf::SHT_PROGBITS, sh_flags: elf::SHF_ALLOC };
    let f = define(&mut b, "f", text, 0, 80, SymbolKind::Text, SymbolScope::Dynamic, false);
    let ext = undefined(&mut b, "ext", false);
    let var = define(&mut b, "var", data, 0, 8, SymbolKind::Data, SymbolScope::Dynamic, false);
    let tvar = tls(&mut b, "tvar");
    let relocations = [
        (0, ext, elf::R_AARCH64_CALL26),
        (4, ext, elf::R_AARCH64_JUMP26),
        (8, var, elf::R_AARCH64_ADR_PREL_PG_HI21),
        (12, var, elf::R_AARCH64_ADD_ABS_LO12_NC),
        (16, var, elf::R_AARCH64_LDST64_ABS_LO12_NC),
        (20, var, elf::R_AARCH64_LDST128_ABS_LO12_NC),
        (24, ext, elf::R_AARCH64_ADR_GOT_PAGE),
        (28, ext, elf::R_AARCH64_LD64_GOT_LO12_NC),
        (32, tvar, elf::R_AARCH64_TLSDESC_ADR_PAGE21),
        (36, tvar, elf::R_AARCH64_TLSDESC_LD64_LO12),
        (40, tvar, elf::R_AARCH64_TLSDESC_ADD_LO12),
        (44, tvar, elf::R_AARCH64_TLSDESC_CALL),
        (48, tvar, elf::R_AARCH64_TLSLE_ADD_TPREL_HI12),
        (52, tvar, elf::R_AARCH64_TLSLE_ADD_TPREL_LO12_NC),
        (56, var, elf::R_AARCH64_MOVW_UABS_G1_NC),
        (60, f, elf::R_AARCH64_CONDBR19),
        (64, f, elf::R_AARCH64_TSTBR14),
        (68, var, elf::R_AARCH64_LD_PREL_LO19),
        (72, var, elf::R_AARCH64_NONE),
        (76, var, elf::R_AARCH64_PREL16),
    ];
    for (at, symbol, r_type) in relocations {
        relocate(&mut b, text, at, symbol, 1, r_type);
    }
    relocate(&mut b, data, 0, f, 0, elf::R_AARCH64_ABS64);
    let text_symbol = b.section_symbol(text);
    relocate(&mut b, eh_frame, 8, text_symbol, 0, elf::R_AARCH64_PREL32);
    b.write().expect("an ELF object")
}

fn minimal(arch: Architecture) -> Vec<u8> {
    let mut b = builder(arch);
    let text = b.section_id(StandardSection::Text);
    b.append_section_data(text, &[0; 8], 4);
    define(&mut b, "main", text, 0, 8, SymbolKind::Text, SymbolScope::Dynamic, false);
    b.write().expect("an ELF object")
}

#[cfg_attr(test, allow(dead_code))]
fn main() -> std::process::ExitCode {
    blaze_fuzzer::main(&TARGET)
}

#[cfg(test)]
mod tests {
    use blaze_link::Format;

    use super::{KIND, named};

    #[test]
    fn seeds() {
        blaze_fuzzer::seeds(&super::TARGET);
    }

    #[test]
    fn every_case() {
        blaze_fuzzer::every_case(&super::TARGET, super::CASES, super::check_case);
    }

    /// The seeds are objects the linker takes, so the mutations start from accepted reads.
    #[test]
    fn seeds_read() {
        for (what, arch, bytes) in named() {
            assert!(blaze_link::fuzz_read(Format::Elf, arch, KIND, &bytes), "seed `{what}` is refused");
        }
    }
}
