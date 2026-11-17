//! The readers against objects built byte by byte here (tests/support/obj.rs). Each
//! expectation is what the format's own specification says the bytes mean.

#[path = "support/obj.rs"]
mod obj;

use blaze_native::Platform;
use blaze_native::ar::{self, ArError};
use blaze_native::object::{self, Bind, Def, Point, ReadError, Reloc, RelocKind, RelocTo, SectionKind, UnwindRow};
use blaze_own::Alloc;
use obj::{Ar, ElfSection, ElfSymbol, MachSection, MachSymbol, elf, elf_group, elf_rela, elf_symtab, macho, reloc_word};

fn words(insns: &[u32]) -> obj::Bytes {
    let mut v = obj::bytes();
    for w in insns {
        v.extend_from_slice(&w.to_le_bytes());
    }
    v
}

/// ar(5): "If the file name is longer than 16 bytes [BSD] ... the file name is stored
/// immediately after the header, and the name field holds `#1/` followed by its length"; GNU:
/// "a name ending in `/`", and `/<n>` "refers to the long name at offset n of the `//`
/// member", each long name ended by "/\n"; members start on an even byte boundary ("a newline
/// is inserted between files if necessary"). Symbol indexes (`__.SYMDEF SORTED`, `/`) are not
/// members. Expected: the two object members of each archive, named and sliced as written.
#[test]
fn ar_bsd_and_gnu_members() {
    let alloc = Alloc::global();

    let mut bsd = Ar::start();
    bsd.member(b"#1/20", b"__.SYMDEF SORTED\0\0\0\0\x08\0\0\0\0\0\0\0");
    bsd.member(b"#1/12", b"long_name.o\0ABCD");
    bsd.member(b"short.o", b"xyz");
    let archive = ar::read(&bsd.bytes, alloc).expect("the BSD archive reads");
    assert_eq!(archive.members.len(), 2);
    assert_eq!(archive.members[0].name, &b"long_name.o"[..]);
    assert_eq!(archive.members[0].bytes, &b"ABCD"[..]);
    assert_eq!(archive.members[0].offset, bsd.offsets[1]);
    assert_eq!(archive.members[1].name, &b"short.o"[..]);
    assert_eq!(archive.members[1].bytes, &b"xyz"[..]);
    assert_eq!(archive.members[1].offset, bsd.offsets[2]);

    let mut gnu = Ar::start();
    gnu.member(b"/", b"\0\0\0\0\0\0\0\0");
    gnu.member(b"//", b"a_very_long_member_name.o/\n");
    gnu.member(b"/0", b"1234");
    gnu.member(b"b.o/", b"q");
    let archive = ar::read(&gnu.bytes, alloc).expect("the GNU archive reads");
    assert_eq!(archive.members.len(), 2);
    assert_eq!(archive.members[0].name, &b"a_very_long_member_name.o"[..]);
    assert_eq!(archive.members[0].bytes, &b"1234"[..]);
    assert_eq!(archive.members[0].offset, gnu.offsets[2]);
    assert_eq!(archive.members[1].name, &b"b.o"[..]);
    assert_eq!(archive.members[1].bytes, &b"q"[..]);
}

/// The four arm64 objects' forms clang writes, read as `<mach-o/arm64/reloc.h>` and ld64 define
/// them: ADRP+ADD (`ARM64_RELOC_PAGE21`, `ARM64_RELOC_PAGEOFF12`, the ADD's scale 0); an
/// `LDR x1, [x1]` PAGEOFF12 (scale 3, its size field); `ARM64_RELOC_ADDEND` ("must be followed
/// by PAGE21 or PAGEOFF12" or BRANCH26: its `r_symbolnum` is the next one's addend);
/// `ARM64_RELOC_SUBTRACTOR` ("must be followed by ARM64_RELOC_UNSIGNED": the pair is
/// `unsigned - subtractor + addend`, the addend the bytes at the place); a non-extern
/// `ARM64_RELOC_UNSIGNED` whose bytes hold the target address, here `__cstring`'s address 16
/// plus 1, so section 1 plus 1. A `compact_unwind_entry` (function, length, encoding,
/// personality, lsda) read through its relocations. `n_value` of an N_SECT symbol is an address,
/// so `_weak_hidden` at address 8 in `__text` is offset 8; `N_PEXT` is hidden; `N_WEAK_DEF` weak.
#[test]
fn macho_reads_sections_symbols_relocs() {
    let alloc = Alloc::global();
    // adrp x0, l_.str@PAGE; add x0, x0, l_.str@PAGEOFF; bl _callee+8; ldr x1, [x1, _data_sym@PAGEOFF]
    let text = words(&[0x9000_0000, 0x9100_0000, 0x9400_0000, 0xf940_0021]);
    let mut unwind = obj::bytes();
    unwind.extend_from_slice(&0u64.to_le_bytes()); // function (relocated)
    unwind.extend_from_slice(&16u32.to_le_bytes()); // length
    unwind.extend_from_slice(&0x0400_0000u32.to_le_bytes()); // encoding: UNWIND_ARM64_MODE_FRAME
    unwind.extend_from_slice(&0u64.to_le_bytes()); // personality (relocated)
    unwind.extend_from_slice(&0u64.to_le_bytes()); // lsda: none
    let mut data = obj::bytes();
    data.extend_from_slice(&4u64.to_le_bytes()); // the SUBTRACTOR pair's addend
    data.extend_from_slice(&17u64.to_le_bytes()); // __cstring's address (16) + 1
    let (unsigned, subtractor, branch26, page21, pageoff12, addend) = (0, 1, 2, 3, 4, 10);
    let text_relocs = [
        (12, reloc_word(1, false, 2, true, pageoff12)),
        (8, reloc_word(8, false, 2, false, addend)),
        (8, reloc_word(3, true, 2, true, branch26)),
        (4, reloc_word(0, false, 2, true, pageoff12)),
        (0, reloc_word(0, true, 2, true, page21)),
    ];
    let unwind_relocs = [(16, reloc_word(4, false, 3, true, unsigned)), (0, reloc_word(2, false, 3, true, unsigned))];
    let data_relocs = [
        (8, reloc_word(2, false, 3, false, unsigned)),
        (0, reloc_word(1, false, 3, true, subtractor)),
        (0, reloc_word(2, false, 3, true, unsigned)),
    ];
    let sections = [
        MachSection { segment: b"__TEXT", name: b"__text", flags: 0x8000_0400, align_log2: 2, bytes: &text, relocs: &text_relocs },
        MachSection { segment: b"__TEXT", name: b"__cstring", flags: 0x2, align_log2: 0, bytes: b"hi\0", relocs: &[] },
        MachSection { segment: b"__LD", name: b"__compact_unwind", flags: 0x0200_0000, align_log2: 3, bytes: &unwind, relocs: &unwind_relocs },
        MachSection { segment: b"__DATA", name: b"__data", flags: 0, align_log2: 3, bytes: &data, relocs: &data_relocs },
    ];
    let symbols = [
        MachSymbol { name: b"l_.str", n_type: 0x0e, n_sect: 2, n_desc: 0, value: 0 },
        MachSymbol { name: b"_data_sym", n_type: 0x0f, n_sect: 4, n_desc: 0, value: 0 },
        MachSymbol { name: b"_main", n_type: 0x0f, n_sect: 1, n_desc: 0, value: 0 },
        MachSymbol { name: b"_callee", n_type: 0x01, n_sect: 0, n_desc: 0, value: 0 },
        MachSymbol { name: b"___gxx_personality_v0", n_type: 0x01, n_sect: 0, n_desc: 0, value: 0 },
        MachSymbol { name: b"_weak_hidden", n_type: 0x1f, n_sect: 1, n_desc: 0x0080, value: 8 },
    ];
    assert_eq!(obj::macho_addresses(&sections)[1], 16, "the builder lays __cstring at 16");
    let file = macho(&sections, &symbols);
    let object = object::read(Platform::MacosArm64, &file, alloc).expect("the Mach-O object reads");

    let kinds: std::vec::Vec<SectionKind> = object.sections.iter().map(|s| s.kind).collect();
    assert_eq!(kinds, [SectionKind::Code, SectionKind::CStrings, SectionKind::CompactUnwind, SectionKind::Data]);
    assert_eq!(object.sections[0].name, &b"__text"[..]);
    assert_eq!(object.sections[0].segment, &b"__TEXT"[..]);
    assert_eq!(object.sections[2].align_log2, 3);
    let ranges: std::vec::Vec<_> = object.sections.iter().map(|s| s.relocs.clone()).collect();
    assert_eq!(ranges, [0..4, 4..4, 4..6, 6..8]);

    let r = |section, offset, kind, to, addend| Reloc { section, offset, kind, to, addend };
    let want = [
        r(0, 0, RelocKind::Page21, RelocTo::Symbol(0), 0),
        r(0, 4, RelocKind::PageOff12 { scale_log2: 0 }, RelocTo::Symbol(0), 0),
        r(0, 8, RelocKind::Branch26, RelocTo::Symbol(3), 8),
        r(0, 12, RelocKind::PageOff12 { scale_log2: 3 }, RelocTo::Symbol(1), 0),
        r(2, 0, RelocKind::Abs64, RelocTo::Symbol(2), 0),
        r(2, 16, RelocKind::Abs64, RelocTo::Symbol(4), 0),
        r(3, 0, RelocKind::Delta64 { minus: RelocTo::Symbol(1) }, RelocTo::Symbol(2), 4),
        r(3, 8, RelocKind::Abs64, RelocTo::Section(1), 1),
    ];
    assert_eq!(&object.relocs[..], &want[..]);

    assert_eq!(object.symbols.len(), 6);
    assert_eq!((object.symbols[0].def, object.symbols[0].bind), (Def::In { section: 1, offset: 0 }, Bind::Local));
    assert_eq!((object.symbols[1].def, object.symbols[1].bind), (Def::In { section: 3, offset: 0 }, Bind::Global));
    assert_eq!(object.symbols[2].name, &b"_main"[..]);
    assert_eq!((object.symbols[3].def, object.symbols[3].bind), (Def::Undefined, Bind::Global));
    let weak = &object.symbols[5];
    assert_eq!((weak.def, weak.bind, weak.hidden), (Def::In { section: 0, offset: 8 }, Bind::Weak, true));
    assert!(!object.symbols[2].hidden);

    assert_eq!(
        &object.unwind[..],
        &[UnwindRow { function: Point { section: 0, offset: 0 }, length: 16, encoding: 0x0400_0000, personality: Some(RelocTo::Symbol(4)), lsda: None }][..]
    );
    assert!(object.groups.is_empty());
}

/// gABI: an `SHT_GROUP` section's "sh_info ... the symbol table index of an entry ... the name
/// of that symbol table entry provides a signature for the section group", its contents a
/// `GRP_COMDAT` flag word then member indexes; "a relocatable file ... st_value holds a section
/// offset for a defined symbol"; `SHN_COMMON`'s "st_value holds alignment constraints".
/// AAELF64: `R_AARCH64_CALL26` (283), `ADR_PREL_PG_HI21` (275), `LDST64_ABS_LO12_NC` (286,
/// "Set the LD/ST immediate value to bits [11:3] of X"), `ADR_GOT_PAGE` (311), `PREL32` (261),
/// `ABS64` (257). A relocation against an `STT_SECTION` symbol is that section plus the addend.
/// Relocations come out sorted by (section, offset) whatever the file's order.
#[test]
fn elf_reads_rela_and_group() {
    let alloc = Alloc::global();
    let symbols = [
        ElfSymbol { name: b"", info: 0x03, other: 0, shndx: 1, value: 0, size: 0 },
        ElfSymbol { name: b"f", info: 0x22, other: 0, shndx: 4, value: 0, size: 4 },
        ElfSymbol { name: b"g", info: 0x12, other: 2, shndx: 1, value: 4, size: 4 },
        ElfSymbol { name: b"ext", info: 0x10, other: 0, shndx: 0, value: 0, size: 0 },
        ElfSymbol { name: b"wext", info: 0x20, other: 0, shndx: 0, value: 0, size: 0 },
        ElfSymbol { name: b"common", info: 0x11, other: 0, shndx: 0xfff2, value: 16, size: 8 },
    ];
    let (symtab, strtab) = elf_symtab(&symbols);
    let rela_text = elf_rela(&[(0, 4, 283, 0), (4, 1, 275, 8), (8, 1, 286, 8), (12, 5, 311, 0)]);
    let rela_data = elf_rela(&[(8, 3, 257, 0), (0, 2, 261, -4)]);
    let group = elf_group(1, &[4]);
    let sections = [
        ElfSection { name: b".text", kind: 1, flags: 0x6, align: 4, bytes: &[0u8; 16], link: 0, info: 0, entsize: 0 },
        ElfSection { name: b".rela.text", kind: 4, flags: 0x40, align: 8, bytes: &rela_text, link: 7, info: 1, entsize: 24 },
        ElfSection { name: b".group", kind: 17, flags: 0, align: 4, bytes: &group, link: 7, info: 2, entsize: 4 },
        ElfSection { name: b".text.f", kind: 1, flags: 0x206, align: 4, bytes: &[0u8; 4], link: 0, info: 0, entsize: 0 },
        ElfSection { name: b".data", kind: 1, flags: 0x3, align: 8, bytes: &[0u8; 16], link: 0, info: 0, entsize: 0 },
        ElfSection { name: b".rela.data", kind: 4, flags: 0x40, align: 8, bytes: &rela_data, link: 7, info: 5, entsize: 24 },
        ElfSection { name: b".symtab", kind: 2, flags: 0, align: 8, bytes: &symtab, link: 8, info: 2, entsize: 24 },
        ElfSection { name: b".strtab", kind: 3, flags: 0, align: 1, bytes: &strtab, link: 0, info: 0, entsize: 0 },
        ElfSection { name: b".note.GNU-stack", kind: 1, flags: 0, align: 1, bytes: &[], link: 0, info: 0, entsize: 0 },
    ];
    let file = elf(&sections);
    let object = object::read(Platform::LinuxArm64, &file, alloc).expect("the ELF object reads");

    let kinds: std::vec::Vec<SectionKind> = object.sections.iter().map(|s| s.kind).collect();
    use SectionKind::{Code, Data, Skip};
    assert_eq!(kinds, [Skip, Code, Skip, Skip, Code, Data, Skip, Skip, Skip, Skip, Skip]);
    assert_eq!(object.sections[4].name, &b".text.f"[..]);
    assert_eq!(object.sections[1].align_log2, 2);
    assert_eq!(object.sections[1].relocs, 0..4);
    assert_eq!(object.sections[4].relocs, 4..4);
    assert_eq!(object.sections[5].relocs, 4..6);

    let r = |section, offset, kind, to, addend| Reloc { section, offset, kind, to, addend };
    let want = [
        r(1, 0, RelocKind::Branch26, RelocTo::Symbol(4), 0),
        r(1, 4, RelocKind::Page21, RelocTo::Section(1), 8),
        r(1, 8, RelocKind::PageOff12 { scale_log2: 3 }, RelocTo::Section(1), 8),
        r(1, 12, RelocKind::GotPage21, RelocTo::Symbol(5), 0),
        r(5, 0, RelocKind::Rel32, RelocTo::Symbol(2), -4),
        r(5, 8, RelocKind::Abs64, RelocTo::Symbol(3), 0),
    ];
    assert_eq!(&object.relocs[..], &want[..]);

    assert_eq!(object.symbols.len(), 7);
    assert_eq!((object.symbols[2].def, object.symbols[2].bind), (Def::In { section: 4, offset: 0 }, Bind::Weak));
    let g = &object.symbols[3];
    assert_eq!((g.def, g.bind, g.hidden), (Def::In { section: 1, offset: 4 }, Bind::Global, true));
    assert_eq!((object.symbols[4].def, object.symbols[4].bind), (Def::Undefined, Bind::Global));
    assert_eq!((object.symbols[5].def, object.symbols[5].bind), (Def::Undefined, Bind::WeakUndefined));
    assert_eq!(object.symbols[6].def, Def::Common { size: 8, align_log2: 4 });

    assert_eq!(object.groups.len(), 1);
    assert_eq!(object.groups[0].signature, &b"f"[..]);
    assert_eq!(&object.groups[0].sections[..], &[4u32][..]);
    assert!(object.unwind.is_empty());
}

/// `<mach-o/arm64/reloc.h>` defines `r_type` 0 to 11 (`ARM64_RELOC_UNSIGNED` ..
/// `ARM64_RELOC_AUTHENTICATED_POINTER`); 15 is none of them. A form the reader does not know
/// is refused by name with its value, never guessed. Expected:
/// `Unsupported { what: "mach-o reloc", value: 15, .. }`.
#[test]
fn refuses_unknown_reloc() {
    let alloc = Alloc::global();
    let text = words(&[0xd503_201f]); // nop
    let relocs = [(0, reloc_word(0, false, 2, true, 15))];
    let sections = [MachSection { segment: b"__TEXT", name: b"__text", flags: 0x8000_0400, align_log2: 2, bytes: &text, relocs: &relocs }];
    let symbols = [MachSymbol { name: b"_x", n_type: 0x01, n_sect: 0, n_desc: 0, value: 0 }];
    let file = macho(&sections, &symbols);
    let refused = object::read(Platform::MacosArm64, &file, alloc).err().expect("type 15 is refused");
    println!("{refused:?}");
    assert!(matches!(refused, ReadError::Unsupported { what: "mach-o reloc", value: 15, .. }), "{refused:?}");
}

/// ar(5)'s thin form (`!<thin>\n`) keeps members in other files by path; a record must hold its
/// bytes, so it is refused by name. Expected: `Thin`.
#[test]
fn refuses_thin() {
    let alloc = Alloc::global();
    let refused = ar::read(b"!<thin>\n", alloc).err().expect("a thin archive is refused");
    assert_eq!(refused, ArError::Thin);
}
