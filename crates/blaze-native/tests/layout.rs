//! `blaze_native::layout::lay` and `blaze_native::unwind` (NATIVE.md W3). Objects are built as
//! values here (no bytes, no tool): what a reader (W1) gives and a plan (W2) resolves.
//! Without the piece: "unresolved import `blaze_native::layout`".

use blaze_native::image::{FixTo, Fixup, Image, Import, Region};
use blaze_native::layout::{Layout, LayoutError, lay};
use blaze_native::object::{Bind, Def, Object, Reloc, RelocKind, RelocTo, Section, SectionKind, Symbol};
use blaze_native::resolve::{Input, InputKind, Origin, Plan, Pulled, Resolution};
use blaze_native::unwind::{Row, unwind_info};
use blaze_own::{AVec, Alloc, Vec};

const ORIGIN: Origin<'static> = Origin { package: "t@0.1.0", file: "libt.a" };

fn a() -> Alloc<'static> {
    Alloc::global()
}

fn le(words: &[u32]) -> &'static [u8] {
    let mut b: AVec<'static, u8> = Vec::new_in(a());
    for w in words {
        b.extend_from_slice(&w.to_le_bytes());
    }
    b.leak_static()
}

fn zeros(n: usize) -> &'static [u8] {
    let mut b: AVec<'static, u8> = Vec::new_in(a());
    b.resize(n, 0);
    b.leak_static()
}

fn section(segment: &'static [u8], name: &'static [u8], kind: SectionKind, align_log2: u8, bytes: &'static [u8], relocs: core::ops::Range<u32>) -> Section<'static> {
    Section { segment, name, kind, align_log2, bytes, size: bytes.len() as u64, relocs }
}

fn sym(name: &'static [u8], def: Def, bind: Bind) -> Symbol<'static> {
    Symbol { name, def, bind, hidden: false }
}

fn reloc(section: u32, offset: u32, kind: RelocKind, symbol: u32) -> Reloc {
    Reloc { section, offset, kind, to: RelocTo::Symbol(symbol), addend: 0 }
}

fn object<const S: usize, const Y: usize, const R: usize>(sections: [Section<'static>; S], symbols: [Symbol<'static>; Y], relocs: [Reloc; R]) -> Object<'static> {
    Object {
        sections: Vec::from_array_in(sections, a()),
        symbols: Vec::from_array_in(symbols, a()),
        relocs: Vec::from_array_in(relocs, a()),
        unwind: Vec::new_in(a()),
        groups: Vec::new_in(a()),
    }
}

fn plan<const N: usize, const W: usize, const I: usize>(object: Object<'static>, names: [(&'static [u8], Resolution); N], words: [&'static [u8]; W], imports: [&'static [u8]; I]) -> Plan<'static> {
    Plan {
        objects: Vec::from_array_in([Pulled { input: 0, member: 0, name: b"one.o", object }], a()),
        names: Vec::from_array_in(names, a()),
        words: Vec::from_array_in(words, a()),
        imports: Vec::from_array_in(imports, a()),
        dropped: Vec::new_in(a()),
        libraries: Vec::new_in(a()),
    }
}

fn inputs() -> [Input<'static>; 1] {
    [Input { from: ORIGIN, bytes: &[], kind: InputKind::Archive { whole: false } }]
}

fn laid(plan: &Plan<'static>) -> Image<'static> {
    match lay(plan, &inputs(), Layout::MACOS_ARM64, a()) {
        Ok(image) => image,
        Err(e) => panic!("lay refused: {e:?}"),
    }
}

fn at(image: &Image<'_>, region: Region) -> (u64, u64) {
    let p = image.region(region).expect("every region is listed");
    (p.at, p.len)
}

fn word(image: &Image<'_>, addr: u64) -> u32 {
    let i = (addr - image.file_start) as usize;
    u32::from_le_bytes(image.bytes[i..i + 4].try_into().expect("four bytes"))
}

fn dword(image: &Image<'_>, addr: u64) -> u64 {
    let i = (addr - image.file_start) as usize;
    u64::from_le_bytes(image.bytes[i..i + 8].try_into().expect("eight bytes"))
}

fn sext(value: u32, bits: u32) -> i64 {
    let shift = 64 - bits;
    ((u64::from(value) << shift) as i64) >> shift
}

/// The page an ADRP at `place` names: A64 "ADRP": `imm = SignExtend(immhi:immlo:Zeros(12), 64);
/// X[d] = (PC AND NOT(0xFFF)) + imm`.
fn adrp_page(insn: u32, place: u64) -> u64 {
    let imm = ((insn >> 5) & 0x7ffff) << 2 | ((insn >> 29) & 3);
    (place & !0xfff).wrapping_add((sext(imm, 21) << 12) as u64)
}

/// imm12 of an ADD (immediate) or an LDR (immediate, unsigned offset), unscaled.
fn imm12(insn: u32) -> u64 {
    u64::from((insn >> 10) & 0xfff)
}

/// A64 "BL": `offset = SignExtend(imm26:'00', 64); BranchTo(PC + offset)`.
fn bl_dest(insn: u32, place: u64) -> u64 {
    place.wrapping_add((sext(insn & 0x03ff_ffff, 26) << 2) as u64)
}

/// ADRP x0 + ADD x0, x0 to a local symbol in data: the Arm ARM's ADRP rule ("the address of
/// the 4KB page ... PC-relative, page offset in imm") with the ADD's imm12 the low 12 bits,
/// so together they make the symbol's exact image address.
/// Prints "adrp_add_resolved: target 0x.... page delta 0x...." with a nonzero delta.
#[test]
fn adrp_add_resolved() {
    let code = le(&[0x9000_0000, 0x9100_0000]);
    let p = plan(
        object(
            [section(b"__TEXT", b"__text", SectionKind::Code, 2, code, 0..2), section(b"__DATA", b"__data", SectionKind::Data, 3, zeros(0x20), 2..2)],
            [sym(b"ltmp_k", Def::In { section: 1, offset: 0x10 }, Bind::Local)],
            [reloc(0, 0, RelocKind::Page21, 0), reloc(0, 4, RelocKind::PageOff12 { scale_log2: 0 }, 0)],
        ),
        [],
        [],
        [],
    );
    let image = laid(&p);
    let (text, _) = at(&image, Region::Text);
    let (data, _) = at(&image, Region::Data);
    // The descriptor (48 bytes, BNL2) is Data's first item; the section follows at 8-byte alignment.
    let target = data + 48 + 0x10;
    assert_eq!(adrp_page(word(&image, text), text), target & !0xfff);
    assert_eq!(imm12(word(&image, text + 4)), target & 0xfff);
    assert_ne!(target & !0xfff, text & !0xfff, "the test wants a nonzero page delta");
    assert!(image.fixups.is_empty(), "a pc-relative pair needs no fixup");
    println!("adrp_add_resolved: target {target:#x} page delta {:#x}", (target & !0xfff) - (text & !0xfff));
}

/// The same `adrp; add` pair, its name now an import (no object of the image defines it, as a
/// hidden name whose definer is not in the link): refused naming the member, section, offset,
/// symbol and why (rule 19), not as a bare kind and offset. Twin: `adrp_add_resolved`.
#[test]
fn adrp_to_an_import_names_the_member_and_symbol() {
    use blaze_native::layout::{Far, Reach};
    let code = le(&[0xd503_201f, 0x9000_0000, 0x9100_0000]);
    let p = plan(
        object(
            [section(b"__TEXT", b"__text", SectionKind::Code, 2, code, 0..2)],
            [sym(b"_hidden_elsewhere", Def::Undefined, Bind::Global)],
            [reloc(0, 4, RelocKind::Page21, 0), reloc(0, 8, RelocKind::PageOff12 { scale_log2: 0 }, 0)],
        ),
        [(&b"_hidden_elsewhere"[..], Resolution::Import { index: 0, weak: false })],
        [],
        [&b"_hidden_elsewhere"[..]],
    );
    let refused = match lay(&p, &inputs(), Layout::MACOS_ARM64, a()) {
        Err(refused) => refused,
        Ok(_) => panic!("a direct page reference to an import was laid"),
    };
    let want = Far {
        kind: RelocKind::Page21,
        from: ORIGIN,
        member: b"one.o",
        segment: b"__TEXT",
        section: b"__text",
        offset: 4,
        symbol: b"_hidden_elsewhere",
        reach: Reach::Import,
    };
    assert_eq!(refused, LayoutError::OutOfRange(want));
    let words = std::format!("{refused:?}");
    assert!(words.contains("`one.o`") && words.contains("`_hidden_elsewhere`") && words.contains("__TEXT,__text"), "{words}");
    println!("adrp_to_an_import_names_the_member_and_symbol: {words}");
}

/// aws-lc's bcm.o shape: `__TEXT,__StaticInit` takes `___dso_handle` by `adrp; add` to hand it
/// to `__cxa_atexit`. No object defines the name: the linker does (synthetic.rs), as the
/// image's Mach-O header, which the writer puts at address 0 of the image (image.rs:
/// `file_start` = `header_reserve`, the bytes before it the writer's header). So the pair makes
/// address 0, with no fixup (pc-relative: the header moves with the code), and a pointer to it
/// is 0 rebased to the image's load address. Twin: `adrp_to_an_import_names_the_member_and_symbol`
/// (the same pair to a name nobody defines is refused).
/// Prints "dso_handle_is_the_header: adrp page 0x0 add 0x0, pointer 0x0 rebased".
#[test]
fn dso_handle_is_the_header() {
    let code = le(&[0xd503_201f, 0x9000_0000, 0x9100_0000]);
    let p = plan(
        object(
            [section(b"__TEXT", b"__StaticInit", SectionKind::Code, 2, code, 0..2), section(b"__DATA", b"__data", SectionKind::Data, 3, zeros(8), 2..3)],
            [sym(b"___dso_handle", Def::Undefined, Bind::Global)],
            [reloc(0, 4, RelocKind::Page21, 0), reloc(0, 8, RelocKind::PageOff12 { scale_log2: 0 }, 0), reloc(1, 0, RelocKind::Abs64, 0)],
        ),
        [(&b"___dso_handle"[..], Resolution::Linker)],
        [],
        [],
    );
    let image = laid(&p);
    assert!(image.file_start > 0, "the header is the writer's, before the file bytes");
    let (text, _) = at(&image, Region::Text);
    let (data, _) = at(&image, Region::Data);
    let header = 0u64;
    let page = adrp_page(word(&image, text + 4), text + 4);
    let low = imm12(word(&image, text + 8));
    assert_eq!(page + low, header, "adrp; add make the header's address");
    let pointer = data + 48;
    assert_eq!(dword(&image, pointer), header);
    assert!(image.fixups.contains(&Fixup { at: pointer, to: FixTo::rebase(header) }));
    assert_eq!(image.fixups.len(), 1, "the pc-relative pair needs no fixup");
    println!("dso_handle_is_the_header: adrp page {page:#x} add {low:#x}, pointer {:#x} rebased", dword(&image, pointer));
}

/// ld64's boundary names: `section$start$__DATA$__data` is the first byte of the laid
/// `__DATA,__data`, `section$end$...` the byte past its last (`__DATA,__data` starts after the
/// 48-byte BNL2 descriptor, as in `adrp_add_resolved`). Both are pointers, rebased.
#[test]
fn section_bounds_are_the_laid_section() {
    let p = plan(
        object(
            [section(b"__DATA", b"__data", SectionKind::Data, 3, zeros(0x20), 0..2)],
            [sym(b"section$start$__DATA$__data", Def::Undefined, Bind::Global), sym(b"section$end$__DATA$__data", Def::Undefined, Bind::Global)],
            [reloc(0, 0, RelocKind::Abs64, 0), reloc(0, 8, RelocKind::Abs64, 1)],
        ),
        [(&b"section$end$__DATA$__data"[..], Resolution::Linker), (&b"section$start$__DATA$__data"[..], Resolution::Linker)],
        [],
        [],
    );
    let image = laid(&p);
    let (data, _) = at(&image, Region::Data);
    let start = data + 48;
    assert_eq!(dword(&image, start), start);
    assert_eq!(dword(&image, start + 8), start + 0x20);
    assert!(image.fixups.contains(&Fixup { at: start + 8, to: FixTo::rebase(start + 0x20) }));
}

/// A pointer to a linker name plus an addend (stop 2 of tempo's `run`, aws-lc's shape): the
/// header's `___dso_handle + 0x10` and `section$start$__DATA$__data + 8` are image places,
/// rebased to them. Apple arm64's non-unique RTTI (tempo's rocksdb: a typeinfo's name field,
/// `.quad __ZTS1H + 0x8000000000000000`) is a tag over an image place: laid as a rebase whose
/// target keeps the top byte (the writer's `high8`). A target that is no image place below its
/// top byte is refused naming the member, section, offset, name and the in-place addend (rule
/// 19), never handed on to fail as a bare value at the chain encoding: `___dso_handle - 8`
/// (below the image, wrapped) and bit 55 (`+ 0x0080000000000000`, no tag: rule 3).
#[test]
fn a_pointer_to_a_linker_name_with_an_addend_is_rebased_or_refused_by_name() {
    use blaze_native::image::Addr56;
    use blaze_native::layout::{Far, Reach};
    let with = |offset: u32, symbol: u32, addend: i64| Reloc { section: 0, offset, kind: RelocKind::Abs64, to: RelocTo::Symbol(symbol), addend };
    let names = || [(&b"___dso_handle"[..], Resolution::Linker), (&b"section$start$__DATA$__data"[..], Resolution::Linker)];
    let symbols = || {
        [
            sym(b"___dso_handle", Def::Undefined, Bind::Global),
            sym(b"section$start$__DATA$__data", Def::Undefined, Bind::Global),
            sym(b"__ZTS1H", Def::In { section: 0, offset: 0 }, Bind::Local),
        ]
    };
    let p = plan(object([section(b"__DATA", b"__data", SectionKind::Data, 3, zeros(0x18), 0..3)], symbols(), [with(0, 0, 0x10), with(8, 1, 8), with(0x10, 2, i64::MIN)]), names(), [], []);
    let image = laid(&p);
    let (data, _) = at(&image, Region::Data);
    let start = data + 48;
    let rebase = |address: u64, high8: u8| FixTo::Rebase { address: Addr56::new(address).expect("an image address"), high8 };
    assert!(image.fixups.contains(&Fixup { at: start, to: rebase(0x10, 0) }), "the header plus 0x10");
    assert!(image.fixups.contains(&Fixup { at: start + 8, to: rebase(start + 8, 0) }), "control: the section's start plus 8, untagged");
    assert!(image.fixups.contains(&Fixup { at: start + 0x10, to: rebase(start, 0x80) }), "the RTTI name pointer keeps bit 63 as its high8");
    for (symbol, addend, name) in [(0u32, -8i64, &b"___dso_handle"[..]), (2, 0x0080_0000_0000_0000, &b"__ZTS1H"[..])] {
        let far = plan(object([section(b"__DATA", b"__data", SectionKind::Data, 3, zeros(8), 0..1)], symbols(), [with(0, symbol, addend)]), names(), [], []);
        let refused = match lay(&far, &inputs(), Layout::MACOS_ARM64, a()) {
            Err(refused) => refused,
            Ok(_) => panic!("a pointer {addend:#x} off `{}` was laid", core::str::from_utf8(name).unwrap_or("?")),
        };
        let &LayoutError::OutOfRange(Far { reach: Reach::Rebase { to, addend: said, end }, symbol: named, member, section, offset, .. }) = &refused else {
            panic!("not refused as a rebase past the image: {refused:?}")
        };
        assert_eq!((named, member, section, offset, said), (name, &b"one.o"[..], &b"__data"[..], 0, addend));
        assert!(to & Addr56::MASK > end, "{to:#x} is past {end:#x} below its top byte");
        println!("a_pointer_to_a_linker_name_with_an_addend_is_rebased_or_refused_by_name: {refused:?}");
    }
}

/// A linker name this image has no address for is refused by its own words (rule 3: lld would
/// make an empty section; rule 19: the name, the input, why): a boundary of a section that is
/// not laid, and an executable's header in a dylib image.
#[test]
fn a_linker_name_with_no_address_is_refused_by_name() {
    use blaze_native::layout::{NoAddress, Unmade};
    let refused = |name: &'static [u8]| {
        let p = plan(
            object([section(b"__DATA", b"__data", SectionKind::Data, 3, zeros(8), 0..1)], [sym(name, Def::Undefined, Bind::Global)], [reloc(0, 0, RelocKind::Abs64, 0)]),
            [(name, Resolution::Linker)],
            [],
            [],
        );
        match lay(&p, &inputs(), Layout::MACOS_ARM64, a()) {
            Err(e) => e,
            Ok(_) => panic!("`{}` was laid", core::str::from_utf8(name).unwrap_or("?")),
        }
    };
    let none = refused(b"section$start$__DATA$__mod_init_func");
    assert_eq!(none, LayoutError::Linker(Unmade { name: b"section$start$__DATA$__mod_init_func", from: ORIGIN, why: NoAddress::NoSection }));
    let words = std::format!("{none:?}");
    assert!(words.contains("section$start$__DATA$__mod_init_func") && words.contains("no section"), "{words}");
    let exe = refused(b"__mh_execute_header");
    assert_eq!(exe, LayoutError::Linker(Unmade { name: b"__mh_execute_header", from: ORIGIN, why: NoAddress::OtherKind { kind: "an executable" } }));
    println!("a_linker_name_with_no_address_is_refused_by_name: {words}");
}

/// NATIVE.md 3.4: a branch from C to a Rust export goes through a 12-byte stub `adrp x16,
/// word@PAGE; ldr x16, [x16, word@PAGEOFF]; br x16`, and the word is the launcher's to fill.
/// Prints "branch_to_rust_goes_through_word_stub: bl -> stub 0x...., stub -> word 0x....".
#[test]
fn branch_to_rust_goes_through_word_stub() {
    let p = plan(
        object(
            [section(b"__TEXT", b"__text", SectionKind::Code, 2, le(&[0x9400_0000]), 0..1)],
            [sym(b"_rust_cb", Def::Undefined, Bind::Global)],
            [reloc(0, 0, RelocKind::Branch26, 0)],
        ),
        [(&b"_rust_cb"[..], Resolution::Rust { word: 0 })],
        [&b"_rust_cb"[..]],
        [],
    );
    let image = laid(&p);
    let (text, text_len) = at(&image, Region::Text);
    let (words, words_len) = at(&image, Region::Words);
    let stub = bl_dest(word(&image, text), text);
    assert!(stub > text && stub + 12 <= text + text_len, "the stub is in Text, after the code");
    let ldr = word(&image, stub + 4);
    assert_eq!(adrp_page(word(&image, stub), stub) + imm12(ldr) * 8, words);
    assert_eq!(ldr & 0xffc0_03ff, 0xf940_0210, "ldr x16, [x16, #imm]");
    assert_eq!(word(&image, stub + 8), 0xd61f_0200, "br x16");
    assert_eq!(words_len, 8);
    assert_eq!(image.words.as_slice(), &[&b"_rust_cb"[..]]);
    assert!(image.fixups.is_empty(), "a word is filled by the launcher through the descriptor, not by a fixup");
    println!("branch_to_rust_goes_through_word_stub: bl -> stub {stub:#x}, stub -> word {words:#x}");
}

/// NATIVE.md 3.4: initializers (`__mod_init_func`) are not emitted as such; they go to
/// `__blaze_inits` in link order, a rebased pointer each, and the descriptor (magic "BNL1",
/// words, inits, pad, word_at, init_at) counts them and says where they are.
/// Prints "init_pointers_land_in_inits_region: 1 init at 0x...., descriptor at 0x....".
#[test]
fn init_pointers_land_in_inits_region() {
    let p = plan(
        object(
            [
                section(b"__TEXT", b"__text", SectionKind::Code, 2, le(&[0xd65f_03c0]), 0..0),
                section(b"__DATA", b"__mod_init_func", SectionKind::InitPointers, 3, zeros(8), 0..1),
            ],
            [sym(b"ltmp_init", Def::In { section: 0, offset: 0 }, Bind::Local)],
            [reloc(1, 0, RelocKind::Abs64, 0)],
        ),
        [],
        [],
        [],
    );
    let image = laid(&p);
    let (text, _) = at(&image, Region::Text);
    let (inits, inits_len) = at(&image, Region::Inits);
    let (data, _) = at(&image, Region::Data);
    assert_eq!((inits_len, image.inits), (8, 1));
    assert!(image.fixups.contains(&Fixup { at: inits, to: FixTo::rebase(text) }));
    assert_eq!(image.descriptor_at, data, "the descriptor is Data's first item");
    let d = image.descriptor_at;
    // BNL2 (descriptor.rs): tag, words, inits, pointer rows.
    assert_eq!((word(&image, d), word(&image, d + 4), word(&image, d + 8), word(&image, d + 12)), (0x424e_4c32, 0, 1, 0));
    assert_eq!(dword(&image, d + 24), inits);
    assert!(image.exports.iter().any(|e| *e == (&b"___blaze_native_v1"[..], d)));
    println!("init_pointers_land_in_inits_region: 1 init at {inits:#x}, descriptor at {d:#x}");
}

/// NATIVE.md 3.6 and dyld's `struct TLVDescriptor { thunk, key, offset }`: the thunk word
/// binds to `__tlv_bootstrap` (an import), the key is dyld's, and the offset is the variable's
/// offset in the TLV template (`__thread_data` then `__thread_bss`), a number, not a pointer.
/// Prints "tlv_thunk_imports_tlv_bootstrap: Import { index: 0, addend: 0 } at 0x...., offset 0".
#[test]
fn tlv_thunk_imports_tlv_bootstrap() {
    let p = plan(
        object(
            [
                section(b"__DATA", b"__thread_vars", SectionKind::ThreadVars, 3, zeros(24), 0..2),
                section(b"__DATA", b"__thread_data", SectionKind::ThreadData, 3, le(&[7, 0]), 2..2),
            ],
            [sym(b"__tlv_bootstrap", Def::Undefined, Bind::Global), sym(b"_v$tlv$init", Def::In { section: 1, offset: 0 }, Bind::Local)],
            [reloc(0, 0, RelocKind::Abs64, 0), reloc(0, 16, RelocKind::Abs64, 1)],
        ),
        [(&b"__tlv_bootstrap"[..], Resolution::Import { index: 0, weak: false })],
        [],
        [&b"__tlv_bootstrap"[..]],
    );
    let image = laid(&p);
    let (vars, _) = at(&image, Region::ThreadVars);
    let thunk = image.fixups.iter().find(|f| f.at == vars).expect("a fixup at the thunk word");
    assert_eq!(thunk.to, FixTo::Import { index: 0, addend: 0 });
    assert!(!image.fixups.iter().any(|f| f.at == vars + 16), "the offset word is a number");
    assert_eq!(dword(&image, vars + 16), 0, "the variable is the template's first byte");
    assert_eq!(image.imports.as_slice(), &[Import { name: &b"__tlv_bootstrap"[..], weak: false }]);
    println!("tlv_thunk_imports_tlv_bootstrap: {:?} at {vars:#x}, offset 0", thunk.to);
}

/// `<mach-o/compact_unwind_encoding.h>`: version 1; personalities are image offsets of pointer
/// cells, their 1-based index in `UNWIND_PERSONALITY_MASK` (0x30000000); `UNWIND_HAS_LSDA`
/// (0x40000000) marks a row with an LSDA, found in the LSDA index; a regular second-level page
/// (kind 2) holds (functionOffset, encoding) pairs; the last first-level entry's
/// functionOffset is the end of the last function. A DWARF-mode row (0x03000000) keeps its FDE
/// offset in the low 24 bits. Read back here by a reader of the format, not by the writer.
/// Prints "unwind_info_round_trips: 2 entries, personality 0x8000, lsda (0x4000, 0x5000), end 0x4030".
#[test]
fn unwind_info_round_trips() {
    let rows = [
        Row { start: 0x4000, length: 0x20, encoding: 0x0400_0000, personality: Some(0x8000), lsda: Some(0x5000) },
        Row { start: 0x4020, length: 0x10, encoding: 0x0300_0040, personality: None, lsda: None },
    ];
    let mut out: AVec<'static, u8> = Vec::new_in(a());
    unwind_info(&rows, &mut out).expect("two sorted rows");
    let u = |at: usize| u32::from_le_bytes(out[at..at + 4].try_into().expect("in the section"));
    let h = |at: usize| u16::from_le_bytes(out[at..at + 2].try_into().expect("in the section"));
    assert_eq!(u(0), 1, "version");
    let (personalities, personality_count) = (u(12) as usize, u(16));
    let (index, index_count) = (u(20) as usize, u(24) as usize);
    assert_eq!(personality_count, 1);
    assert_eq!(u(personalities), 0x8000);
    assert_eq!(index_count, 2, "one page and the end entry");
    assert_eq!(u(index), 0x4000);
    let (page, lsdas) = (u(index + 4) as usize, u(index + 8) as usize);
    let end = index + 12;
    assert_eq!(u(end), 0x4030, "the end of the last function");
    let lsdas_end = u(end + 8) as usize;
    assert_eq!((lsdas_end - lsdas) / 8, 1);
    assert_eq!((u(lsdas), u(lsdas + 4)), (0x4000, 0x5000));
    assert_eq!(u(page), 2, "UNWIND_SECOND_LEVEL_REGULAR");
    let (entries, count) = (page + h(page + 4) as usize, h(page + 6));
    assert_eq!(count, 2);
    assert_eq!((u(entries), u(entries + 4)), (0x4000, 0x0400_0000 | 0x1000_0000 | 0x4000_0000));
    assert_eq!((u(entries + 8), u(entries + 12)), (0x4020, 0x0300_0040));
    assert_eq!(out.len(), page + 8 + 16, "nothing after the page");
    println!("unwind_info_round_trips: 2 entries, personality {:#x}, lsda ({:#x}, {:#x}), end {:#x}", u(personalities), u(lsdas), u(lsdas + 4), u(end));
}

/// NATIVE.md 3.4: `__mod_term_func`/`.fini_array` are refused `TermFunctions` (C++ registers
/// destructors with `__cxa_atexit`, which works in a loaded image).
/// Prints "term_functions_refused: TermFunctions { from: Origin { package: "t@0.1.0", file: "libt.a" } }".
#[test]
fn term_functions_refused() {
    let p = plan(object([section(b"__DATA", b"__mod_term_func", SectionKind::TermPointers, 3, zeros(8), 0..0)], [], []), [], [], []);
    let err = match lay(&p, &inputs(), Layout::MACOS_ARM64, a()) {
        Ok(_) => panic!("a term function section was laid"),
        Err(e) => e,
    };
    assert_eq!(err, LayoutError::TermFunctions { from: ORIGIN });
    println!("term_functions_refused: {err:?}");
}
