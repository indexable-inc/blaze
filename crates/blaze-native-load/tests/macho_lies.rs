//! The Trust rule at the loader's door: bytes are verified before they are used. A Mach-O
//! object is written here as bytes by a tiny writer (one `__TEXT,__text` section of two words,
//! two symbols, one `ARM64_RELOC_BRANCH26`), once honest and once with each of three lies, and
//! blaze-native's reader (the one object reader the loader takes its input from) must refuse
//! each lie by name. The honest one reads (the control, rule 23).

use blaze_mem::frame::Frame;
use blaze_mem::tag::Tag;
use blaze_native::object::{ReadError, RelocKind};
use blaze_own::{AVec, Alloc, Erased, Vec};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Lie {
    None,
    /// The section's size runs its bytes past the file's end.
    SectionPastEnd,
    /// The relocation names symbol 2 of a table of two.
    SymbolPastTable,
    /// The relocation's place is the section's length: four bytes past its last word.
    RelocPastSection,
}

fn u32le(b: &mut AVec<'_, u8>, x: u32) {
    b.extend_from_slice(&x.to_le_bytes());
}

fn u64le(b: &mut AVec<'_, u8>, x: u64) {
    b.extend_from_slice(&x.to_le_bytes());
}

fn name16(b: &mut AVec<'_, u8>, name: &[u8]) {
    let mut field = [0u8; 16];
    field[..name.len()].copy_from_slice(name);
    b.extend_from_slice(&field);
}

/// The layout (offsets in the file): header 0..32; LC_SEGMENT_64 32..184 (72 + one 80-byte
/// section_64); LC_SYMTAB 184..208; the section's bytes 208..216 (`bl _g; nop`); its one
/// relocation 216..224; two nlist_64 224..256; the string table 256..264 ("\0_f\0_g\0" and a pad).
fn object<'a>(lie: Lie, a: Alloc<'a>) -> &'a [u8] {
    let mut b: AVec<'a, u8> = Vec::new_in(a);
    // mach_header_64: MH_MAGIC_64, CPU_TYPE_ARM64, subtype 0, MH_OBJECT, ncmds 2, sizeofcmds 176, flags, reserved.
    for x in [0xfeed_facf, 0x0100_000c, 0, 1, 2, 152 + 24, 0, 0] {
        u32le(&mut b, x);
    }
    // segment_command_64: LC_SEGMENT_64, cmdsize 152, segname "", vmaddr 0, vmsize 8, fileoff 208,
    // filesize 8, maxprot 7, initprot 7, nsects 1, flags 0.
    u32le(&mut b, 0x19);
    u32le(&mut b, 152);
    name16(&mut b, b"");
    for x in [0u64, 8, 208, 8] {
        u64le(&mut b, x);
    }
    for x in [7, 7, 1, 0] {
        u32le(&mut b, x);
    }
    // section_64: "__text", "__TEXT", addr 0, size, offset 208, align 2, reloff 216, nreloc 1,
    // flags S_ATTR_PURE_INSTRUCTIONS | S_ATTR_SOME_INSTRUCTIONS, three reserved words.
    name16(&mut b, b"__text");
    name16(&mut b, b"__TEXT");
    u64le(&mut b, 0);
    u64le(&mut b, if lie == Lie::SectionPastEnd { 0x1000 } else { 8 });
    for x in [208, 2, 216, 1, 0x8000_0400, 0, 0, 0] {
        u32le(&mut b, x);
    }
    // symtab_command: LC_SYMTAB, 24, symoff 224, nsyms 2, stroff 256, strsize 8.
    for x in [0x2, 24, 224, 2, 256, 8] {
        u32le(&mut b, x);
    }
    assert_eq!(b.len(), 208, "the commands end where the section's bytes start");
    // bl _g (0x9400_0000, the relocation fills it); nop.
    u32le(&mut b, 0x9400_0000);
    u32le(&mut b, 0xd503_201f);
    // relocation_info: r_address, then r_symbolnum:24 r_pcrel:1 r_length:2 r_extern:1 r_type:4:
    // symbol 1 (`_g`), pcrel, length 2 (4 bytes), extern, ARM64_RELOC_BRANCH26 (2).
    let address = if lie == Lie::RelocPastSection { 8 } else { 0 };
    let symbol = if lie == Lie::SymbolPastTable { 2 } else { 1 };
    u32le(&mut b, address);
    u32le(&mut b, symbol | 1 << 24 | 2 << 25 | 1 << 27 | 2 << 28);
    // nlist_64 `_f`: n_strx 1, N_SECT | N_EXT, n_sect 1, n_desc 0, n_value 0.
    u32le(&mut b, 1);
    b.extend_from_slice(&[0x0f, 1, 0, 0]);
    u64le(&mut b, 0);
    // nlist_64 `_g`: n_strx 4, N_UNDF | N_EXT, no section.
    u32le(&mut b, 4);
    b.extend_from_slice(&[0x01, 0, 0, 0]);
    u64le(&mut b, 0);
    b.extend_from_slice(b"\0_f\0_g\0\0");
    assert_eq!(b.len(), 264);
    b.leak()
}

/// Prints "honest_object_reads: 1 section, 2 symbols, Branch26 at 0".
#[test]
fn honest_object_reads() {
    let f = Frame::with_reserve(1 << 24, Tag::Link).expect("a frame");
    let erased = Erased::new(&f);
    let a = erased.handle();
    let o = match blaze_native::macho::read(object(Lie::None, a), a) {
        Ok(o) => o,
        Err(e) => panic!("the honest object is refused: {e:?}"),
    };
    assert_eq!(o.sections.len(), 1);
    assert_eq!(o.symbols.len(), 2);
    assert_eq!(o.symbols[0].name, b"_f");
    assert_eq!(o.relocs.len(), 1);
    assert_eq!((o.relocs[0].offset, o.relocs[0].kind), (0, RelocKind::Branch26));
    println!("honest_object_reads: 1 section, 2 symbols, Branch26 at 0");
}

/// Each lie refused by name. Prints one line a lie, "lie SectionPastEnd: Truncated { at: .. }" and so on.
#[test]
fn lying_objects_refused() {
    let f = Frame::with_reserve(1 << 24, Tag::Link).expect("a frame");
    let erased = Erased::new(&f);
    let a = erased.handle();
    for lie in [Lie::SectionPastEnd, Lie::SymbolPastTable, Lie::RelocPastSection] {
        let refused = match blaze_native::macho::read(object(lie, a), a) {
            Ok(_) => panic!("{lie:?} was read"),
            Err(e) => e,
        };
        match (lie, refused) {
            (Lie::SectionPastEnd, ReadError::Truncated { .. }) => {}
            (Lie::SymbolPastTable, ReadError::Malformed { what, .. }) => assert_eq!(what, "a mach-o relocation's r_symbolnum names no symbol"),
            (Lie::RelocPastSection, ReadError::Malformed { .. }) => {}
            (lie, other) => panic!("{lie:?} refused as {other:?}"),
        }
        println!("lie {lie:?}: {refused:?}");
    }
}
