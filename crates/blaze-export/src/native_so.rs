//! The program's native library record as an ELF shared object (`ET_DYN`) for arm64 Linux
//! (NATIVE.md 3.1, 3.4, 3.5, 3.6, 3.10; piece W5): headers, the dynamic section and its tables
//! placed around a [`blaze_native::image::Image`] that `blaze_native::layout::lay` already laid
//! out at image-relative addresses with [`Layout::LINUX_ARM64`]. Nothing here moves a byte of the
//! image: every region keeps its address, and file offset equals address up to the end of
//! initialized data (`Image::file_start + Image::bytes.len()`).
//!
//! PUBLIC SIGNATURE (W7 makes the record with it, W8 loads it):
//!
//! ```text
//! pub enum SoError<'a> {
//!     NotLinux,                                           // image.platform is not LinuxArm64
//!     Library { name: &'a str },                          // a library resolved for another platform
//!     Alloc,                                              // the output could not be reserved
//!     Soname { len: usize },                              // empty, or holds a NUL byte
//!     TooLarge { what: &'static str, len: u64 },          // a field of the format too narrow
//!     Shape { what: &'static str, at: u64 },              // the image breaks a promise of image.rs
//!     Remeasured { what: &'static str, measured: u64, written: u64 }, // a size the writer named came out other
//! }
//! pub fn write<'a>(image: &Image<'a>, soname: &str, out: &mut AVec<'_, u8>, alloc: Alloc<'_>)
//!     -> Result<(), SoError<'a>>;
//! ```
//!
//! Structures: the System V gABI (`Elf64_Ehdr`, `Elf64_Phdr`, `Elf64_Shdr`, `Elf64_Sym`,
//! `Elf64_Rela`, `Elf64_Dyn`), its AArch64 supplement (`R_AARCH64_ABS64` 257, `GLOB_DAT` 1025,
//! `RELATIVE` 1027, `TLS_TPREL64` 1030, `TLSDESC` 1031) and glibc's `<elf.h>` (`PT_TLS` 7,
//! `PT_GNU_*`, `DT_GNU_HASH`, `DT_FLAGS_1`, `DF_STATIC_TLS`, `SHT_GNU_HASH`). The file, in order:
//!
//! | load | holds | protection |
//! |---|---|---|
//! | 1 | the ELF header and program headers (the image's first 0x10000 bytes), Text, Const, EhFrame, UnwindInfo (`.eh_frame_hdr`) | r-x |
//! | 2 | Got (`PT_GNU_RELRO` over it), Data (the descriptor `__blaze_native_v1` first), Words, Inits, ThreadData, ThreadZero, Zero | rw- |
//! | 3 | `.rela.dyn`, `.gnu.hash`, `.dynsym`, `.dynstr` | r-- |
//! | 4 | `.dynamic` | rw- (glibc before 2.35 adjusts its pointers in place) |
//!
//! Then `.shstrtab` and the section headers (no load): tools and debuggers read them, the loader
//! does not. Loads 3 and 4 follow the image in the file with no padding: each takes the next free
//! page of address space at the address congruent to its file offset (`p_offset % p_align ==
//! p_vaddr % p_align`), as lld places them.
//!
//! Program headers: the four `PT_LOAD`s, `PT_DYNAMIC`, `PT_TLS` (the template ThreadData then
//! ThreadZero, when non-empty), `PT_GNU_EH_FRAME` (the image's UnwindInfo, when non-empty),
//! `PT_GNU_RELRO` (the Got, when non-empty: the image puts Data on the page after it), and
//! `PT_GNU_STACK` (rw-). Dynamic entries: `DT_NEEDED` per distinct soname (`libc.so.6` first,
//! then the image's libraries in order), `DT_SONAME`, `DT_GNU_HASH`, `DT_STRTAB`, `DT_SYMTAB`,
//! `DT_STRSZ`, `DT_SYMENT`, `DT_RELA`, `DT_RELASZ`, `DT_RELAENT`, `DT_RELACOUNT`, `DT_FLAGS`
//! (`DF_BIND_NOW`, and `DF_STATIC_TLS` when any `TlsReloc::TpRel` exists), `DT_FLAGS_1`
//! (`DF_1_NOW`), `DT_NULL`. No `DT_INIT_ARRAY`: the members' initializers are
//! `.data.blaze_inits`, which the launcher runs after it has filled the words (NATIVE.md 3.4).
//!
//! Relocations (`.rela.dyn`, every cell's file bytes 0, the value in the addend):
//! - `FixTo::Rebase` -> `R_AARCH64_RELATIVE`, first, sorted by address (`DT_RELACOUNT` of them).
//! - `FixTo::Import` -> `R_AARCH64_GLOB_DAT` for a Got cell with addend 0, `R_AARCH64_ABS64`
//!   otherwise (a data pointer, or an addend); the symbol is the import's, `STB_WEAK` when
//!   `Import::weak` (glibc then binds 0 where no library defines it).
//! - `TlsReloc::Desc` -> `R_AARCH64_TLSDESC` on the Got pair, `TlsReloc::TpRel` ->
//!   `R_AARCH64_TLS_TPREL64` on the Got cell; both with symbol 0 (the image's own module) and the
//!   variable's offset in the template as the addend, as lld emits them for a local variable.
//! - `FixTo::Word` -> none: the descriptor's pointer rows list the cell (BNL2, blaze-native
//!   descriptor.rs) and the launcher writes it with the function's one address, its slot's thunk.
//!
//! Symbols (`.dynsym`): the null symbol, then the imports in the image's order (undefined), then
//! the exports in `.gnu.hash` bucket order (the table requires the hashed symbols last and grouped
//! by bucket; within a bucket the image's name order): `STT_FUNC` in Text, `STT_TLS` in the
//! template (value: offset in the template), `STT_OBJECT` elsewhere; each with its section's
//! index. Imports are bound by the default lookup among the libraries the image needs and the
//! global scope (NATIVE.md 3.5: no symbol versions, so the bytes depend on no machine's libraries).
//!
//! Determinism (NATIVE.md 3.10): the bytes are a function of the image and the soname: no path
//! but the sonames, every table in a fixed order, no time, no build id of the machine.

use blaze_native::Platform;
use blaze_native::image::{FixTo, Image, Placed, Region, TlsReloc};
use blaze_native::layout::Layout;
use blaze_own::{AString, AVec, Alloc, String, Vec};

use crate::bytes;
use crate::elf::{
    self, DF_BIND_NOW, DT_FLAGS, DT_FLAGS_1, DT_NEEDED, DT_NULL, DT_RELA, DT_RELACOUNT, DT_RELAENT, DT_RELASZ, DT_STRSZ, DT_STRTAB, DT_SYMENT, DT_SYMTAB, EHDR, PF_R,
    PF_W, PF_X, PHDR, PT_DYNAMIC, PT_GNU_EH_FRAME, PT_GNU_RELRO, PT_GNU_STACK, PT_LOAD, R_AARCH64_ABS64, R_AARCH64_GLOB_DAT, R_AARCH64_RELATIVE,
};

/// Why the image could not be written as a shared object; each names the thing and the rule.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SoError<'a> {
    /// The image is not for arm64 Linux (NATIVE.md 3.1: ELF on Linux; macOS is W4's).
    NotLinux,
    /// A library of the image resolved for another platform than Linux: its install name is no
    /// `DT_NEEDED` soname (NATIVE.md 3.5).
    Library { name: &'a str },
    /// The output's room could not be reserved.
    Alloc,
    /// The soname is empty or holds a NUL byte: `DT_SONAME` is a C string (gABI).
    Soname { len: usize },
    /// A value does not fit the format's field `what` (32-bit string offsets, 16-bit header
    /// counts, the header room of `Layout::header_reserve`).
    TooLarge { what: &'static str, len: u64 },
    /// The image breaks a promise image.rs makes (`what`), at image address `at`.
    Shape { what: &'static str, at: u64 },
    /// `what` came out at `written` where the writer had measured `measured` and already named
    /// it in a header or `.dynamic`: a bug of this file's, refused in every build.
    Remeasured { what: &'static str, measured: u64, written: u64 },
}

const SHDR: u64 = 64;
const SYM: u64 = 24;
const RELA: u64 = 24;
const DYN: u64 = 16;

const ET_DYN: u16 = 3;
const EM_AARCH64: u16 = 183;
const EV_CURRENT: u32 = 1;
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;

const PT_TLS: u32 = 7;

const DT_SONAME: u64 = 14;
const DT_GNU_HASH: u64 = 0x6FFF_FEF5;
/// The image holds initial-exec TLS: glibc serves it from its static surplus, or fails the load
/// naming the library (NATIVE.md 3.6).
const DF_STATIC_TLS: u64 = 0x10;
const DF_1_NOW: u64 = 0x1;

const R_AARCH64_TLS_TPREL64: u64 = 1030;
const R_AARCH64_TLSDESC: u64 = 1031;

const SHT_PROGBITS: u32 = 1;
const SHT_STRTAB: u32 = 3;
const SHT_RELA: u32 = 4;
const SHT_DYNAMIC: u32 = 6;
const SHT_NOBITS: u32 = 8;
const SHT_DYNSYM: u32 = 11;
const SHT_GNU_HASH: u32 = 0x6FFF_FFF6;
const SHF_WRITE: u64 = 0x1;
const SHF_ALLOC: u64 = 0x2;
const SHF_EXECINSTR: u64 = 0x4;
const SHF_TLS: u64 = 0x400;

const STB_GLOBAL: u8 = 1;
const STB_WEAK: u8 = 2;
const STT_NOTYPE: u8 = 0;
const STT_OBJECT: u8 = 1;
const STT_FUNC: u8 = 2;
const STT_TLS: u8 = 6;

/// `.gnu.hash`'s second bloom shift, lld's for ELF64.
const BLOOM_SHIFT: u32 = 26;

/// The image's regions as sections: name, type, flags, kept when empty.
const REGION_SECTIONS: [(Region, &str, u32, u64, bool); 11] = [
    (Region::Text, ".text", SHT_PROGBITS, SHF_ALLOC | SHF_EXECINSTR, true),
    (Region::Const, ".rodata", SHT_PROGBITS, SHF_ALLOC, false),
    (Region::EhFrame, ".eh_frame", SHT_PROGBITS, SHF_ALLOC, false),
    (Region::UnwindInfo, ".eh_frame_hdr", SHT_PROGBITS, SHF_ALLOC, false),
    (Region::Got, ".got", SHT_PROGBITS, SHF_ALLOC | SHF_WRITE, false),
    (Region::Data, ".data", SHT_PROGBITS, SHF_ALLOC | SHF_WRITE, true),
    (Region::Words, ".data.blaze_words", SHT_PROGBITS, SHF_ALLOC | SHF_WRITE, false),
    (Region::Inits, ".data.blaze_inits", SHT_PROGBITS, SHF_ALLOC | SHF_WRITE, false),
    (Region::ThreadData, ".tdata", SHT_PROGBITS, SHF_ALLOC | SHF_WRITE | SHF_TLS, false),
    (Region::ThreadZero, ".tbss", SHT_NOBITS, SHF_ALLOC | SHF_WRITE | SHF_TLS, false),
    (Region::Zero, ".bss", SHT_NOBITS, SHF_ALLOC | SHF_WRITE, false),
];

fn up(n: u64, to: u64) -> u64 {
    n.next_multiple_of(to)
}

fn shape<'a>(what: &'static str, at: u64) -> SoError<'a> {
    SoError::Shape { what, at }
}

fn end_of(p: &Placed) -> u64 {
    p.at + p.len
}

/// One `Elf64_Rela`: `info` is `sym << 32 | type`.
#[derive(Clone, Copy)]
struct Rela {
    at: u64,
    info: u64,
    addend: i64,
}

/// One `Elf64_Shdr`; `region` is the image region it covers (none for the tables).
#[derive(Clone, Copy)]
struct Shdr {
    region: Option<Region>,
    name: u32,
    kind: u32,
    flags: u64,
    addr: u64,
    offset: u64,
    size: u64,
    link: u32,
    info: u32,
    align: u64,
    entsize: u64,
}

/// Appends `name` and its NUL to a string table; its offset there.
fn intern<'a>(table: &mut AVec<'_, u8>, name: &[u8]) -> Result<u32, SoError<'a>> {
    let at = table.len() as u64;
    let at = u32::try_from(at).map_err(|_| SoError::TooLarge { what: "a string table's 32-bit offset", len: at })?;
    table.extend_from_slice(name);
    table.push(0);
    Ok(at)
}

/// The GNU hash of a symbol name (`dl_new_hash`: h = h * 33 + c from 5381, 32-bit).
fn gnu_hash(name: &[u8]) -> u32 {
    let mut h: u32 = 5381;
    for &c in name {
        h = h.wrapping_mul(33).wrapping_add(u32::from(c));
    }
    h
}

fn sym(out: &mut AVec<'_, u8>, name: u32, info: u8, shndx: u16, value: u64) {
    bytes::u32(out, name);
    bytes::u8(out, info);
    bytes::u8(out, 0); // st_other: STV_DEFAULT
    bytes::u16(out, shndx);
    bytes::u64(out, value);
    bytes::u64(out, 0); // st_size: not known per name; no loader reads it
}

fn dynamic(out: &mut AVec<'_, u8>, tag: u64, value: u64) {
    bytes::u64(out, tag);
    bytes::u64(out, value);
}

fn shdr(out: &mut AVec<'_, u8>, s: &Shdr) {
    bytes::u32(out, s.name);
    bytes::u32(out, s.kind);
    bytes::u64(out, s.flags);
    bytes::u64(out, s.addr);
    bytes::u64(out, s.offset);
    bytes::u64(out, s.size);
    bytes::u32(out, s.link);
    bytes::u32(out, s.info);
    bytes::u64(out, s.align);
    bytes::u64(out, s.entsize);
}

/// Writes `image` as an ELF shared object for arm64 Linux whose `DT_SONAME` is `soname` (the
/// record's hex id), into `out` (its contents replaced). The libraries are `image.libraries`,
/// after `libc.so.6`.
pub fn write<'a, 'b>(image: &Image<'a>, soname: &str, out: &mut AVec<'_, u8>, alloc: Alloc<'b>) -> Result<(), SoError<'a>> {
    if image.platform != Platform::LinuxArm64 {
        return Err(SoError::NotLinux);
    }
    if soname.is_empty() || soname.bytes().any(|b| b == 0) {
        return Err(SoError::Soname { len: soname.len() });
    }
    let page = Layout::LINUX_ARM64.page;
    if image.file_start != Layout::LINUX_ARM64.header_reserve {
        return Err(shape("the image's header room is not Linux's 0x10000 (Layout::LINUX_ARM64)", image.file_start));
    }
    let region = |r: Region| image.region(r).ok_or(SoError::Shape { what: "a region the image does not list", at: 0 });
    let text = region(Region::Text)?;
    let unwind = region(Region::UnwindInfo)?;
    let got = region(Region::Got)?;
    let data = region(Region::Data)?;
    let thread_vars = region(Region::ThreadVars)?;
    let tdata = region(Region::ThreadData)?;
    let tbss = region(Region::ThreadZero)?;
    if text.at < image.file_start {
        return Err(shape("Text starts inside the header room", text.at));
    }
    if thread_vars.len != 0 {
        return Err(shape("Mach-O __thread_vars descriptors in an ELF image", thread_vars.at));
    }

    // The loads' bounds (image.rs: the writable segment starts on a new page; Data on the page
    // after a non-empty Got).
    let init_end = image.file_start + image.bytes.len() as u64;
    let text_end = end_of(unwind);
    if got.at != up(text_end, page) {
        return Err(shape("the Got does not start the page after UnwindInfo", got.at));
    }
    let got_end = end_of(got);
    let data_at = if got.len > 0 { up(got_end, page) } else { got.at };
    if data.at != data_at {
        return Err(shape("Data does not start the page after the Got", data.at));
    }
    if init_end <= data_at || image.end < init_end {
        return Err(shape("the image's file bytes end outside its writable regions", init_end));
    }
    // The TLS template: ThreadData then ThreadZero (layout.rs's `tls_start`, the base of every
    // `TlsReloc` offset).
    let tls_at = if tdata.len > 0 { tdata.at } else { tbss.at };
    let tls_end = if tbss.len > 0 { end_of(tbss) } else { end_of(tdata) };
    let tls_len = tls_end.saturating_sub(tls_at);
    if !image.tls.is_empty() && tls_len == 0 {
        return Err(shape("TLS relocations with no TLS template", tls_at));
    }

    // The relocations.
    let n_imports = image.imports.len();
    let mut relative: AVec<'_, Rela> = Vec::new_in(alloc);
    let mut other: AVec<'_, Rela> = Vec::new_in(alloc);
    let mut launcher_cells: AVec<'_, u64> = Vec::new_in(alloc);
    let mut last: Option<u64> = None;
    for f in image.fixups.iter() {
        if last.is_some_and(|l| f.at <= l) {
            return Err(shape("fixups not sorted by address, or two at one address", f.at));
        }
        last = Some(f.at);
        if !f.at.is_multiple_of(8) || f.at < got.at || f.at + 8 > init_end {
            return Err(shape("a fixup outside the writable file bytes, or not 8-aligned", f.at));
        }
        match f.to {
            FixTo::Rebase { address, high8 } => {
                // RELATIVE's addend is the whole pointer, its tag included.
                let target = address.tagged(high8);
                let addend = i64::try_from(target).map_err(|_| SoError::TooLarge { what: "a rebase target in a 64-bit signed addend", len: target })?;
                relative.push(Rela { at: f.at, info: R_AARCH64_RELATIVE, addend });
            }
            FixTo::Import { index, addend } => {
                if index as usize >= n_imports {
                    return Err(shape("a fixup's import index past the image's imports", f.at));
                }
                let kind = if addend == 0 && f.at < got_end { R_AARCH64_GLOB_DAT } else { R_AARCH64_ABS64 };
                other.push(Rela { at: f.at, info: ((u64::from(index) + 1) << 32) | kind, addend });
            }
            // The launcher's cell (the descriptor's pointer rows): no dynamic relocation.
            FixTo::Word { .. } => launcher_cells.push(f.at),
        }
    }
    let mut static_tls = false;
    for t in image.tls.iter() {
        let (at, offset, kind, len) = match *t {
            TlsReloc::Desc { got_at, offset } => (got_at, offset, R_AARCH64_TLSDESC, 16),
            TlsReloc::TpRel { got_at, offset } => {
                static_tls = true;
                (got_at, offset, R_AARCH64_TLS_TPREL64, 8)
            }
        };
        if !at.is_multiple_of(8) || at < got.at || at + len > got_end {
            return Err(shape("a TLS relocation's cell outside the Got, or not 8-aligned", at));
        }
        if offset >= tls_len {
            return Err(shape("a TLS relocation's offset past the TLS template", at));
        }
        other.push(Rela { at, info: kind, addend: offset as i64 });
    }
    other.sort_unstable_by_key(|r| (r.at, r.info));
    for pair in other.windows(2) {
        if pair[0].at == pair[1].at {
            return Err(shape("two relocations at one cell", pair[1].at));
        }
    }

    // The needed libraries: libc first, then each distinct soname in the image's order.
    let mut needed: AVec<'_, AString<'b>> = Vec::new_in(alloc);
    let mut libc: AString<'b> = String::new_in(alloc);
    libc.push_str(elf::LIBC);
    needed.push(libc);
    for library in image.libraries.iter() {
        if library.platform() != Platform::LinuxArm64 {
            return Err(SoError::Library { name: library.library().name });
        }
        let soname = library.install();
        if !needed.iter().any(|n| n.as_str() == soname) {
            let mut name: AString<'b> = String::new_in(alloc);
            name.push_str(soname);
            needed.push(name);
        }
    }

    // The image's regions as sections (index 0 is the null section).
    let mut shstr: AVec<'_, u8> = Vec::new_in(alloc);
    shstr.push(0);
    let mut sections: AVec<'_, Shdr> = Vec::new_in(alloc);
    sections.push(Shdr { region: None, name: 0, kind: 0, flags: 0, addr: 0, offset: 0, size: 0, link: 0, info: 0, align: 0, entsize: 0 });
    for &(r, name, kind, flags, keep_empty) in REGION_SECTIONS.iter() {
        let p = region(r)?;
        if p.len == 0 && !keep_empty {
            continue;
        }
        let offset = if kind == SHT_NOBITS { init_end } else { p.at };
        let name = intern(&mut shstr, name.as_bytes())?;
        sections.push(Shdr { region: Some(r), name, kind, flags, addr: p.at, offset, size: p.len, link: 0, info: 0, align: 1 << p.align_log2, entsize: 0 });
    }
    // The tables' indices: `.rela.dyn`, `.gnu.hash`, `.dynsym`, `.dynstr`, `.dynamic` follow.
    let dynsym_index = sections.len() as u32 + 2;
    let dynstr_index = dynsym_index + 1;

    // `.dynstr` and `.dynsym`: imports, then exports in bucket order.
    let mut dynstr: AVec<'_, u8> = Vec::new_in(alloc);
    dynstr.push(0);
    let soname_at = intern(&mut dynstr, soname.as_bytes())?;
    let mut needed_at: AVec<'_, u32> = Vec::with_capacity_in(needed.len(), alloc);
    for n in needed.iter() {
        needed_at.push(intern(&mut dynstr, n.as_str().as_bytes())?);
    }
    let mut dynsym: AVec<'_, u8> = Vec::new_in(alloc);
    dynsym.extend_from_slice(&[0u8; SYM as usize]);
    for import in image.imports.iter() {
        let name = intern(&mut dynstr, import.name)?;
        let bind = if import.weak { STB_WEAK } else { STB_GLOBAL };
        sym(&mut dynsym, name, (bind << 4) | STT_NOTYPE, 0, 0);
    }
    let n_exports = image.exports.len();
    if n_exports == 0 {
        return Err(shape("an image that exports nothing (the descriptor is always exported)", data.at));
    }
    let n_exports32 = u32::try_from(n_exports).map_err(|_| SoError::TooLarge { what: "the export count in 32 bits", len: n_exports as u64 })?;
    let symoffset = u32::try_from(1 + n_imports).map_err(|_| SoError::TooLarge { what: "the import count in 32 bits", len: n_imports as u64 })?;
    let nbuckets = (n_exports32 / 4).max(1);
    let mask_words = n_exports32.div_ceil(8).next_power_of_two();
    // (bucket, index in `image.exports`, hash): the hashed symbols' order.
    let mut order: AVec<'_, (u32, u32, u32)> = Vec::with_capacity_in(n_exports, alloc);
    for (k, &(name, _)) in image.exports.iter().enumerate() {
        let h = gnu_hash(name);
        order.push((h % nbuckets, k as u32, h));
    }
    order.sort_unstable();
    for &(_, k, _) in order.iter() {
        let (name, at) = image.exports[k as usize];
        let found = sections.iter().position(|s| s.region.is_some() && at >= s.addr && at < s.addr + s.size);
        let found = found.or_else(|| sections.iter().position(|s| s.region.is_some() && at == s.addr + s.size));
        let index = found.ok_or(shape("an export outside every region", at))?;
        let shndx = u16::try_from(index).map_err(|_| shape("an export in a section past 16-bit st_shndx", at))?;
        let (kind, value) = match sections[index].region {
            Some(Region::Text) => (STT_FUNC, at),
            Some(Region::ThreadData | Region::ThreadZero) => (STT_TLS, at - tls_at),
            _ => (STT_OBJECT, at),
        };
        let name = intern(&mut dynstr, name)?;
        sym(&mut dynsym, name, (STB_GLOBAL << 4) | kind, shndx, value);
    }

    // `.gnu.hash`: header, bloom words, buckets, chains (glibc's `_dl_setup_hash` reads it so).
    let mut bloom: AVec<'_, u64> = Vec::new_in(alloc);
    bloom.resize(mask_words as usize, 0);
    let mut buckets: AVec<'_, u32> = Vec::new_in(alloc);
    buckets.resize(nbuckets as usize, 0);
    let mut chains: AVec<'_, u32> = Vec::with_capacity_in(n_exports, alloc);
    for (i, &(b, _, h)) in order.iter().enumerate() {
        let word = ((h / 64) % mask_words) as usize;
        bloom[word] |= (1u64 << (h % 64)) | (1u64 << ((h >> BLOOM_SHIFT) % 64));
        if buckets[b as usize] == 0 {
            buckets[b as usize] = symoffset + i as u32;
        }
        let last_in_bucket = order.get(i + 1).is_none_or(|n| n.0 != b);
        chains.push((h & !1) | u32::from(last_in_bucket));
    }
    let mut hash: AVec<'_, u8> = Vec::new_in(alloc);
    bytes::u32(&mut hash, nbuckets);
    bytes::u32(&mut hash, symoffset);
    bytes::u32(&mut hash, mask_words);
    bytes::u32(&mut hash, BLOOM_SHIFT);
    for &w in bloom.iter() {
        bytes::u64(&mut hash, w);
    }
    for &b in buckets.iter() {
        bytes::u32(&mut hash, b);
    }
    for &c in chains.iter() {
        bytes::u32(&mut hash, c);
    }

    // Load 3 (`.rela.dyn`, `.gnu.hash`, `.dynsym`, `.dynstr`) right after the file bytes, at the
    // next free page of address space with its offset's congruence.
    let n_rela = relative.len() + other.len();
    let mut meta: AVec<'_, u8> = Vec::new_in(alloc);
    for r in relative.iter().chain(other.iter()) {
        bytes::u64(&mut meta, r.at);
        bytes::u64(&mut meta, r.info);
        bytes::u64(&mut meta, r.addend as u64);
    }
    let rela_len = meta.len() as u64;
    bytes::pad(&mut meta, 8);
    let hash_at = meta.len() as u64;
    meta.extend_from_slice(&hash);
    bytes::pad(&mut meta, 8);
    let dynsym_at = meta.len() as u64;
    meta.extend_from_slice(&dynsym);
    let dynstr_at = meta.len() as u64;
    meta.extend_from_slice(&dynstr);
    let meta_len = meta.len() as u64;
    let meta_off = up(init_end, 8);
    let meta_vaddr = up(image.end, page) + meta_off % page;

    // Load 4: `.dynamic`.
    let mut flags = DF_BIND_NOW;
    if static_tls {
        flags |= DF_STATIC_TLS;
    }
    let mut dyn_bytes: AVec<'_, u8> = Vec::new_in(alloc);
    for &at in needed_at.iter() {
        dynamic(&mut dyn_bytes, DT_NEEDED, u64::from(at));
    }
    dynamic(&mut dyn_bytes, DT_SONAME, u64::from(soname_at));
    dynamic(&mut dyn_bytes, DT_GNU_HASH, meta_vaddr + hash_at);
    dynamic(&mut dyn_bytes, DT_STRTAB, meta_vaddr + dynstr_at);
    dynamic(&mut dyn_bytes, DT_SYMTAB, meta_vaddr + dynsym_at);
    dynamic(&mut dyn_bytes, DT_STRSZ, dynstr.len() as u64);
    dynamic(&mut dyn_bytes, DT_SYMENT, SYM);
    dynamic(&mut dyn_bytes, DT_RELA, meta_vaddr);
    dynamic(&mut dyn_bytes, DT_RELASZ, rela_len);
    dynamic(&mut dyn_bytes, DT_RELAENT, RELA);
    dynamic(&mut dyn_bytes, DT_RELACOUNT, relative.len() as u64);
    dynamic(&mut dyn_bytes, DT_FLAGS, flags);
    dynamic(&mut dyn_bytes, DT_FLAGS_1, DF_1_NOW);
    dynamic(&mut dyn_bytes, DT_NULL, 0);
    let dyn_len = dyn_bytes.len() as u64;
    let dyn_off = up(meta_off + meta_len, 8);
    let dyn_vaddr = up(meta_vaddr + meta_len, page) + dyn_off % page;

    // The tables' sections, then `.shstrtab` and the section headers after everything loaded.
    let tables: [(&str, u32, u64, u64, u64, u64, u32, u32, u64, u64); 5] = [
        (".rela.dyn", SHT_RELA, SHF_ALLOC, meta_vaddr, meta_off, rela_len, dynsym_index, 0, 8, RELA),
        (".gnu.hash", SHT_GNU_HASH, SHF_ALLOC, meta_vaddr + hash_at, meta_off + hash_at, hash.len() as u64, dynsym_index, 0, 8, 0),
        // `sh_info`: the first non-local symbol, after the null one.
        (".dynsym", SHT_DYNSYM, SHF_ALLOC, meta_vaddr + dynsym_at, meta_off + dynsym_at, dynsym.len() as u64, dynstr_index, 1, 8, SYM),
        (".dynstr", SHT_STRTAB, SHF_ALLOC, meta_vaddr + dynstr_at, meta_off + dynstr_at, dynstr.len() as u64, 0, 0, 1, 0),
        (".dynamic", SHT_DYNAMIC, SHF_ALLOC | SHF_WRITE, dyn_vaddr, dyn_off, dyn_len, dynstr_index, 0, 8, DYN),
    ];
    for &(name, kind, flags, addr, offset, size, link, info, align, entsize) in tables.iter() {
        let name = intern(&mut shstr, name.as_bytes())?;
        sections.push(Shdr { region: None, name, kind, flags, addr, offset, size, link, info, align, entsize });
    }
    // `.dynstr`'s index was named (in `.dynsym`'s and `.dynamic`'s `sh_link`) before it was pushed.
    if sections.len() as u64 != u64::from(dynstr_index) + 2 {
        return Err(SoError::Remeasured { what: "the section count before .shstrtab", measured: u64::from(dynstr_index) + 2, written: sections.len() as u64 });
    }
    let shstr_index = sections.len();
    let shstr_name = intern(&mut shstr, b".shstrtab")?;
    let shstr_off = dyn_off + dyn_len;
    sections.push(Shdr { region: None, name: shstr_name, kind: SHT_STRTAB, flags: 0, addr: 0, offset: shstr_off, size: shstr.len() as u64, link: 0, info: 0, align: 1, entsize: 0 });
    let shoff = up(shstr_off + shstr.len() as u64, 8);
    let shnum = u16::try_from(sections.len()).map_err(|_| SoError::TooLarge { what: "e_shnum's 16 bits", len: sections.len() as u64 })?;
    let shstrndx = u16::try_from(shstr_index).map_err(|_| SoError::TooLarge { what: "e_shstrndx's 16 bits", len: shstr_index as u64 })?;

    // The program headers.
    let mut ph: AVec<'_, u8> = Vec::new_in(alloc);
    let rx = PF_R | PF_X;
    let rw = PF_R | PF_W;
    elf::phdr(&mut ph, PT_LOAD, rx, 0, 0, text_end, text_end, page);
    elf::phdr(&mut ph, PT_LOAD, rw, got.at, got.at, init_end - got.at, image.end - got.at, page);
    elf::phdr(&mut ph, PT_LOAD, PF_R, meta_off, meta_vaddr, meta_len, meta_len, page);
    elf::phdr(&mut ph, PT_LOAD, rw, dyn_off, dyn_vaddr, dyn_len, dyn_len, page);
    elf::phdr(&mut ph, PT_DYNAMIC, rw, dyn_off, dyn_vaddr, dyn_len, dyn_len, 8);
    if tls_len > 0 {
        let tls_align = 1u64 << tdata.align_log2.max(tbss.align_log2);
        elf::phdr(&mut ph, PT_TLS, PF_R, tls_at.min(init_end), tls_at, tdata.len, tls_len, tls_align);
    }
    if unwind.len > 0 {
        elf::phdr(&mut ph, PT_GNU_EH_FRAME, PF_R, unwind.at, unwind.at, unwind.len, unwind.len, 4);
    }
    if got.len > 0 {
        // Up to Data's page, so glibc's rounding down of the end keeps every Got page.
        elf::phdr(&mut ph, PT_GNU_RELRO, PF_R, got.at, got.at, data_at - got.at, data_at - got.at, 1);
    }
    elf::phdr(&mut ph, PT_GNU_STACK, rw, 0, 0, 0, 0, 16);
    let phnum = (ph.len() as u64 / PHDR) as u16;
    let header_end = EHDR + ph.len() as u64;
    if header_end > text.at {
        return Err(SoError::TooLarge { what: "the ELF header and program headers past the image's header room", len: header_end });
    }

    // The file.
    let total = shoff + SHDR * u64::from(shnum);
    out.clear();
    out.try_reserve(total as usize).map_err(|_| SoError::Alloc)?;
    out.extend_from_slice(&[0x7F, b'E', b'L', b'F', ELFCLASS64, ELFDATA2LSB, EV_CURRENT as u8, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    bytes::u16(out, ET_DYN);
    bytes::u16(out, EM_AARCH64);
    bytes::u32(out, EV_CURRENT);
    bytes::u64(out, 0); // e_entry: none, a library
    bytes::u64(out, EHDR); // e_phoff
    bytes::u64(out, shoff);
    bytes::u32(out, 0); // e_flags
    bytes::u16(out, EHDR as u16);
    bytes::u16(out, PHDR as u16);
    bytes::u16(out, phnum);
    bytes::u16(out, SHDR as u16);
    bytes::u16(out, shnum);
    bytes::u16(out, shstrndx);
    out.extend_from_slice(&ph);
    out.resize(image.file_start as usize, 0);
    out.extend_from_slice(&image.bytes);
    // RELA: each relocated cell's bytes are 0 (the value is the addend), and the launcher's
    // cells stay 0 until it writes them.
    for r in relative.iter().chain(other.iter()) {
        bytes::set64(out, r.at as usize, 0);
        if r.info == R_AARCH64_TLSDESC {
            bytes::set64(out, r.at as usize + 8, 0);
        }
    }
    for &at in launcher_cells.iter() {
        bytes::set64(out, at as usize, 0);
    }
    out.resize(meta_off as usize, 0);
    out.extend_from_slice(&meta);
    out.resize(dyn_off as usize, 0);
    out.extend_from_slice(&dyn_bytes);
    out.extend_from_slice(&shstr);
    out.resize(shoff as usize, 0);
    for s in sections.iter() {
        shdr(out, s);
    }
    if out.len() as u64 != total {
        return Err(SoError::Remeasured { what: "the shared object's bytes", measured: total, written: out.len() as u64 });
    }
    if n_rela as u64 * RELA != rela_len {
        return Err(SoError::Remeasured { what: ".rela.dyn", measured: n_rela as u64 * RELA, written: rela_len });
    }
    Ok(())
}
