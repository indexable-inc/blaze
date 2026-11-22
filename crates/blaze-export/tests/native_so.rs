//! W5 (NATIVE.md 3.1, 3.4, 3.5, 3.6): the native library record written as an ELF shared object
//! around a hand-laid `blaze_native::image::Image`, read back by a reader here (no dlopen: the host
//! test is macOS's; W11 loads it on Linux).
//!
//! Formats quoted from the System V gABI and glibc's `<elf.h>`: `Elf64_Ehdr` (e_ident
//! "\x7fELF", class 2, data 1; e_type at 16 `ET_DYN` 3, e_machine at 18 `EM_AARCH64` 183, e_phoff
//! at 32, e_phnum at 56), `Elf64_Phdr` (56 bytes: p_type, p_flags, p_offset, p_vaddr, p_paddr,
//! p_filesz, p_memsz, p_align; `PT_LOAD` 1, `PT_DYNAMIC` 2, `PT_TLS` 7), `Elf64_Dyn` (16 bytes:
//! d_tag, d_val; `DT_NEEDED` 1, `DT_STRTAB` 5, `DT_SYMTAB` 6, `DT_RELA` 7, `DT_RELASZ` 8,
//! `DT_SONAME` 14, `DT_FLAGS` 30, `DT_RELACOUNT` 0x6ffffff9, `DT_GNU_HASH` 0x6ffffef5;
//! `DF_BIND_NOW` 0x8, `DF_STATIC_TLS` 0x10), `Elf64_Sym` (24 bytes: st_name, st_info, st_other,
//! st_shndx, st_value, st_size; `STB_WEAK` 2), `Elf64_Rela` (24 bytes: r_offset, r_info = sym << 32
//! | type, r_addend); the AArch64 ELF ABI's `R_AARCH64_ABS64` 257, `GLOB_DAT` 1025, `RELATIVE`
//! 1027, `TLS_TPREL64` 1030, `TLSDESC` 1031; the GNU hash table as glibc's `_dl_setup_hash` and
//! `do_lookup_x` read it (nbuckets, symoffset, bloom_size, bloom_shift, bloom words, buckets,
//! chains; `dl_new_hash`).
//!
//! Without the piece: "unresolved import `blaze_export::native_so`".

use blaze_export::native_so::{SoError, write};
use blaze_native::Platform;
use blaze_native::image::{FixTo, Fixup, Image, Import, Placed, Region, TlsReloc};
use blaze_native::libraries::{Library, LibraryKind, Linked, Source, source};
use blaze_own::{AVec, Alloc, Vec};

const SONAME: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const START: u64 = 0x1_0000;
const RET: u32 = 0xd65f_03c0;

const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PT_TLS: u32 = 7;
const DT_NEEDED: u64 = 1;
const DT_STRTAB: u64 = 5;
const DT_SYMTAB: u64 = 6;
const DT_RELA: u64 = 7;
const DT_RELASZ: u64 = 8;
const DT_SONAME: u64 = 14;
const DT_FLAGS: u64 = 30;
const DT_RELACOUNT: u64 = 0x6FFF_FFF9;
const DT_GNU_HASH: u64 = 0x6FFF_FEF5;
const DF_STATIC_TLS: u64 = 0x10;

fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn put32(bytes: &mut [u8], at: u64, value: u32) {
    let i = (at - START) as usize;
    bytes[i..i + 4].copy_from_slice(&value.to_le_bytes());
}

fn put64(bytes: &mut [u8], at: u64, value: u64) {
    let i = (at - START) as usize;
    bytes[i..i + 8].copy_from_slice(&value.to_le_bytes());
}

/// Two C functions (`c_entry` at 0x10000, `c_entry2` at 0x10004); a Got of the cell bound to
/// `puts` (0x20000), an initial-exec TLS cell (0x20008, only when `tp_rel`) and a TLS descriptor
/// pair (0x20010); Data at 0x30000: the BNL2 descriptor (one word, one init, one pointer row), its
/// names table (`rust_six`), its pointer row, then a pointer to `c_entry` (0x30050), a pointer to
/// `weak_hook + 8` (0x30058) and C data holding Rust word 0 (0x30060, the launcher's); one word
/// cell (0x30068); one initializer (`c_entry2`, 0x30070); an 8-byte `.tdata` variable `c_tls`
/// (0x30080, value 7) and an 8-byte `.tbss` one (0x30088); 16 bytes of zero fill; libraries c, m,
/// pthread.
fn image(tp_rel: bool) -> Image<'static> {
    let a = alloc();
    let mut regions: AVec<'static, Placed> = Vec::new_in(a);
    let rows: [(Region, u64, u64, u8); 12] = [
        (Region::Text, 0x1_0000, 8, 2),
        (Region::Const, 0x1_0008, 0, 0),
        (Region::EhFrame, 0x1_0008, 0, 3),
        (Region::UnwindInfo, 0x1_0008, 0, 2),
        (Region::Got, 0x2_0000, 0x20, 4),
        (Region::Data, 0x3_0000, 0x68, 3),
        (Region::Words, 0x3_0068, 8, 3),
        (Region::Inits, 0x3_0070, 8, 3),
        (Region::ThreadVars, 0x3_0078, 0, 3),
        (Region::ThreadData, 0x3_0080, 8, 3),
        (Region::ThreadZero, 0x3_0088, 8, 3),
        (Region::Zero, 0x3_0090, 16, 3),
    ];
    for (region, at, len, align_log2) in rows {
        regions.push(Placed { region, at, len, align_log2 });
    }
    let mut bytes: AVec<'static, u8> = Vec::new_in(a);
    bytes.resize((0x3_0088 - START) as usize, 0);
    put32(&mut bytes, 0x1_0000, RET);
    put32(&mut bytes, 0x1_0004, RET);
    // The descriptor (BNL2, blaze-native descriptor.rs): tag, words, inits, pointers, word_at,
    // init_at, pointer_at, name_at; the names table; one pointer row (cell 0x30060, word 0).
    put32(&mut bytes, 0x3_0000, 0x424e_4c32);
    put32(&mut bytes, 0x3_0004, 1);
    put32(&mut bytes, 0x3_0008, 1);
    put32(&mut bytes, 0x3_000C, 1);
    put64(&mut bytes, 0x3_0010, 0x3_0068);
    put64(&mut bytes, 0x3_0018, 0x3_0070);
    put64(&mut bytes, 0x3_0020, 0x3_0040);
    put64(&mut bytes, 0x3_0028, 0x3_0030);
    put32(&mut bytes, 0x3_0030, 8);
    put32(&mut bytes, 0x3_0034, 8);
    let i = (0x3_0038 - START) as usize;
    bytes[i..i + 8].copy_from_slice(b"rust_six");
    put64(&mut bytes, 0x3_0040, 0x3_0060);
    put64(&mut bytes, 0x3_0080, 7);
    // Planted values in the relocated cells: the writer must leave every one 0 (RELA).
    put64(&mut bytes, 0x3_0050, 0xDEAD);
    put64(&mut bytes, 0x3_0060, 0xBEEF);
    let mut fixups: AVec<'static, Fixup> = Vec::new_in(a);
    fixups.push(Fixup { at: 0x2_0000, to: FixTo::Import { index: 0, addend: 0 } });
    fixups.push(Fixup { at: 0x3_0050, to: FixTo::rebase(0x1_0000) });
    fixups.push(Fixup { at: 0x3_0058, to: FixTo::Import { index: 1, addend: 8 } });
    fixups.push(Fixup { at: 0x3_0060, to: FixTo::Word { index: 0 } });
    fixups.push(Fixup { at: 0x3_0070, to: FixTo::rebase(0x1_0004) });
    let mut tls: AVec<'static, TlsReloc> = Vec::new_in(a);
    if tp_rel {
        tls.push(TlsReloc::TpRel { got_at: 0x2_0008, offset: 0 });
    }
    tls.push(TlsReloc::Desc { got_at: 0x2_0010, offset: 8 });
    let mut imports: AVec<'static, Import<'static>> = Vec::new_in(a);
    imports.push(Import { name: b"puts", weak: false });
    imports.push(Import { name: b"weak_hook", weak: true });
    let mut exports: AVec<'static, (&'static [u8], u64)> = Vec::new_in(a);
    exports.push((b"__blaze_native_v1", 0x3_0000));
    exports.push((b"c_entry", 0x1_0000));
    exports.push((b"c_entry2", 0x1_0004));
    exports.push((b"c_tls", 0x3_0080));
    let mut words: AVec<'static, &'static [u8]> = Vec::new_in(a);
    words.push(b"rust_six");
    let mut libraries: AVec<'static, Linked<'static>> = Vec::new_in(a);
    for name in ["c", "m", "pthread"] {
        match source(Platform::LinuxArm64, Library { kind: LibraryKind::Dylib, name }) {
            Ok(Source::Linked(linked)) => libraries.push(linked),
            other => panic!("`{name}` is glibc's: {other:?}"),
        }
    }
    Image {
        platform: Platform::LinuxArm64,
        regions,
        bytes,
        file_start: START,
        end: 0x3_00A0,
        fixups,
        tls,
        imports,
        exports,
        words,
        inits: 1,
        descriptor_at: 0x3_0000,
        libraries,
        unwind: Vec::new_in(a),
        frames: Vec::new_in(a),
        eh_pointers: Vec::new_in(a),
    }
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("4 bytes"))
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("8 bytes"))
}

fn cstr(b: &[u8], at: usize) -> &[u8] {
    let len = b[at..].iter().position(|&c| c == 0).expect("a NUL-terminated string");
    &b[at..at + len]
}

/// One program header: (type, flags, offset, vaddr, filesz, memsz).
type Phdr = (u32, u32, u64, u64, u64, u64);

fn phdrs(file: &[u8]) -> AVec<'static, Phdr> {
    let phoff = u64_at(file, 32) as usize;
    let phnum = u16_at(file, 56) as usize;
    let mut out: AVec<'static, Phdr> = Vec::new_in(alloc());
    for k in 0..phnum {
        let p = phoff + 56 * k;
        out.push((u32_at(file, p), u32_at(file, p + 4), u64_at(file, p + 8), u64_at(file, p + 16), u64_at(file, p + 32), u64_at(file, p + 40)));
    }
    out
}

/// The file offset of address `v`, through the `PT_LOAD`s as a loader maps them.
fn offset_of(ph: &[Phdr], v: u64) -> usize {
    let p = ph.iter().find(|p| p.0 == PT_LOAD && v >= p.3 && v < p.3 + p.4).expect("an address in a load's file bytes");
    (p.2 + (v - p.3)) as usize
}

/// The dynamic section's entries.
fn dynamic(file: &[u8], ph: &[Phdr]) -> AVec<'static, (u64, u64)> {
    let d = ph.iter().find(|p| p.0 == PT_DYNAMIC).expect("PT_DYNAMIC");
    let mut at = d.2 as usize;
    let mut out: AVec<'static, (u64, u64)> = Vec::new_in(alloc());
    loop {
        let (tag, val) = (u64_at(file, at), u64_at(file, at + 8));
        if tag == 0 {
            return out;
        }
        out.push((tag, val));
        at += 16;
    }
}

fn tag(dynamic: &[(u64, u64)], t: u64) -> u64 {
    dynamic.iter().find(|d| d.0 == t).map(|d| d.1).expect("a dynamic tag")
}

fn written(image: &Image<'static>) -> AVec<'static, u8> {
    let mut out: AVec<'static, u8> = Vec::new_in(alloc());
    write(image, SONAME, &mut out, alloc()).expect("the image is written as a shared object");
    out
}

/// glibc's `do_lookup_x` over `.gnu.hash`: the bloom filter, the bucket, the chain; the symbol's
/// value, or none.
fn gnu_lookup(file: &[u8], ph: &[Phdr], dynamic: &[(u64, u64)], name: &[u8]) -> Option<u64> {
    let h0 = offset_of(ph, tag(dynamic, DT_GNU_HASH));
    let strtab = offset_of(ph, tag(dynamic, DT_STRTAB));
    let symtab = offset_of(ph, tag(dynamic, DT_SYMTAB));
    let (nbuckets, symoffset, bloom_size, shift) = (u32_at(file, h0), u32_at(file, h0 + 4), u32_at(file, h0 + 8), u32_at(file, h0 + 12));
    let mut hash: u32 = 5381;
    for &c in name {
        hash = hash.wrapping_mul(33).wrapping_add(u32::from(c));
    }
    let bloom = h0 + 16;
    let word = u64_at(file, bloom + 8 * ((hash / 64) % bloom_size) as usize);
    if (word >> (hash % 64)) & (word >> ((hash >> shift) % 64)) & 1 == 0 {
        return None;
    }
    let buckets = bloom + 8 * bloom_size as usize;
    let chains = buckets + 4 * nbuckets as usize;
    let mut index = u32_at(file, buckets + 4 * (hash % nbuckets) as usize);
    if index == 0 {
        return None;
    }
    loop {
        let chain = u32_at(file, chains + 4 * (index - symoffset) as usize);
        let s = symtab + 24 * index as usize;
        if (chain | 1) == (hash | 1) && cstr(file, strtab + u32_at(file, s) as usize) == name {
            return Some(u64_at(file, s + 8));
        }
        if chain & 1 == 1 {
            return None;
        }
        index += 1;
    }
}

/// The written shared object read back: ELF64 AArch64 `ET_DYN`; `DT_NEEDED` libc.so.6 then
/// libm.so.6 (c and pthread are both libc.so.6: NATIVE.md 3.5); `DT_SONAME` the record id; a
/// `PT_TLS` of the 8-byte `.tdata` and 8-byte `.tbss` at 0x30080; the relocations: two
/// `R_AARCH64_RELATIVE` first (`DT_RELACOUNT` 2), `GLOB_DAT` of `puts` on the Got cell, the
/// TPREL64 and TLSDESC with symbol 0 and the template offset as the addend, `ABS64` of the weak
/// `weak_hook` + 8, and none at the launcher's cell 0x30060, whose file bytes (like every relocated
/// cell's) are 0; `__blaze_native_v1` and `c_tls` found through `.gnu.hash`.
/// Prints: `native_so: DT_NEEDED [libc.so.6, libm.so.6], PT_TLS 8+8 at 0x30080, 6 relocations (2 relative), __blaze_native_v1 at 0x30000 by DT_GNU_HASH`
#[test]
fn so_shape() {
    let file = written(&image(true));
    assert_eq!(&file[..7], &[0x7F, b'E', b'L', b'F', 2, 1, 1], "ELF64 little-endian, version 1");
    assert_eq!((u16_at(&file, 16), u16_at(&file, 18)), (3, 183), "ET_DYN for EM_AARCH64");
    let ph = phdrs(&file);
    let d = dynamic(&file, &ph);
    let strtab = offset_of(&ph, tag(&d, DT_STRTAB));
    let mut needed: AVec<'static, &[u8]> = Vec::new_in(alloc());
    for e in d.iter().filter(|e| e.0 == DT_NEEDED) {
        needed.push(cstr(&file, strtab + e.1 as usize));
    }
    assert_eq!(needed[..], [&b"libc.so.6"[..], &b"libm.so.6"[..]]);
    assert_eq!(cstr(&file, strtab + tag(&d, DT_SONAME) as usize), SONAME.as_bytes());
    let tls = ph.iter().find(|p| p.0 == PT_TLS).expect("PT_TLS when ThreadData is non-empty");
    assert_eq!((tls.3, tls.4, tls.5), (0x3_0080, 8, 16), "the template: .tdata then .tbss");
    assert_eq!(u64_at(&file, tls.2 as usize), 7, "the template's file bytes hold .tdata's value");

    let rela = offset_of(&ph, tag(&d, DT_RELA));
    let n = (tag(&d, DT_RELASZ) / 24) as usize;
    let mut rows: AVec<'static, (u64, u64, u64, i64)> = Vec::new_in(alloc());
    for k in 0..n {
        let r = rela + 24 * k;
        let info = u64_at(&file, r + 8);
        rows.push((u64_at(&file, r), info >> 32, info & 0xFFFF_FFFF, u64_at(&file, r + 16) as i64));
    }
    let want: [(u64, u64, u64, i64); 6] = [
        (0x3_0050, 0, 1027, 0x1_0000),
        (0x3_0070, 0, 1027, 0x1_0004),
        (0x2_0000, 1, 1025, 0),
        (0x2_0008, 0, 1030, 0),
        (0x2_0010, 0, 1031, 8),
        (0x3_0058, 2, 257, 8),
    ];
    assert_eq!(rows[..], want);
    assert_eq!(tag(&d, DT_RELACOUNT), 2);
    for &(at, ..) in want.iter().chain([(0x3_0060, 0, 0, 0)].iter()) {
        assert_eq!(u64_at(&file, offset_of(&ph, at)), 0, "the cell at {at:#x} holds 0 in the file");
    }
    let symtab = offset_of(&ph, tag(&d, DT_SYMTAB));
    let weak = symtab + 24 * 2;
    assert_eq!(cstr(&file, strtab + u32_at(&file, weak) as usize), b"weak_hook");
    assert_eq!((file[weak + 4] >> 4, u16_at(&file, weak + 6)), (2, 0), "weak_hook: STB_WEAK, undefined");

    assert_eq!(gnu_lookup(&file, &ph, &d, b"__blaze_native_v1"), Some(0x3_0000));
    assert_eq!(gnu_lookup(&file, &ph, &d, b"c_entry2"), Some(0x1_0004));
    assert_eq!(gnu_lookup(&file, &ph, &d, b"c_tls"), Some(0), "a TLS export's value is its template offset");
    assert_eq!(gnu_lookup(&file, &ph, &d, b"puts"), None, "an import is not a definition");
    println!(
        "native_so: DT_NEEDED [libc.so.6, libm.so.6], PT_TLS {}+{} at {:#x}, {} relocations ({} relative), __blaze_native_v1 at 0x30000 by DT_GNU_HASH",
        tls.4,
        tls.5 - tls.4,
        tls.3,
        rows.len(),
        tag(&d, DT_RELACOUNT)
    );
}

/// `DF_STATIC_TLS` is set exactly when an initial-exec relocation (`TlsReloc::TpRel`) exists
/// (NATIVE.md 3.6): glibc then serves it from its static surplus or fails the load by name.
/// Prints: `native_so: DT_FLAGS 0x8 without a TpRel, 0x18 with one`
#[test]
fn static_tls_flag_follows_tp_rel() {
    let flags = |tp_rel: bool| {
        let file = written(&image(tp_rel));
        let ph = phdrs(&file);
        tag(&dynamic(&file, &ph), DT_FLAGS)
    };
    let (without, with) = (flags(false), flags(true));
    assert_eq!(without & DF_STATIC_TLS, 0);
    assert_eq!(with & DF_STATIC_TLS, DF_STATIC_TLS);
    println!("native_so: DT_FLAGS {without:#x} without a TpRel, {with:#x} with one");
}

/// The bytes are a function of the image and the soname (NATIVE.md 3.10).
/// Prints: `native_so: two writes, N equal bytes`
#[test]
fn same_bytes_twice() {
    let one = written(&image(true));
    let two = written(&image(true));
    assert_eq!(one[..], two[..]);
    println!("native_so: two writes, {} equal bytes", one.len());
}

/// A macOS image is W4's, refused by name; so is an empty soname.
/// Prints: `native_so: NotLinux, Soname { len: 0 }`
#[test]
fn mac_image_and_empty_soname_refused() {
    let mut mac = image(false);
    mac.platform = Platform::MacosArm64;
    let mut out: AVec<'static, u8> = Vec::new_in(alloc());
    assert_eq!(write(&mac, SONAME, &mut out, alloc()), Err(SoError::NotLinux));
    assert_eq!(write(&image(false), "", &mut out, alloc()), Err(SoError::Soname { len: 0 }));
    println!("native_so: NotLinux, Soname {{ len: 0 }}");
}
