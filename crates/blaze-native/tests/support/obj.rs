//! In-test byte builders: a minimal Mach-O arm64 `MH_OBJECT`, a minimal ELF64 AArch64 `ET_REL`
//! and an `ar` archive, written field by field from the formats' structures (no external tool).
//! The layouts follow the same headers the readers cite: `<mach-o/loader.h>`, `<mach-o/nlist.h>`,
//! `<mach-o/reloc.h>`; the gABI's `Elf64_Ehdr`/`Elf64_Shdr`/`Elf64_Sym`/`Elf64_Rela`; ar(5).

use blaze_own::{Alloc, Vec};

pub type Bytes = Vec<u8, Alloc<'static>>;

pub fn bytes() -> Bytes {
    Vec::new_in(Alloc::global())
}

fn put16(v: &mut Bytes, x: u16) {
    v.extend_from_slice(&x.to_le_bytes());
}

fn put32(v: &mut Bytes, x: u32) {
    v.extend_from_slice(&x.to_le_bytes());
}

fn put64(v: &mut Bytes, x: u64) {
    v.extend_from_slice(&x.to_le_bytes());
}

fn pad_to(v: &mut Bytes, at: usize) {
    while v.len() < at {
        v.push(0);
    }
}

fn align(x: usize, to: usize) -> usize {
    x.div_ceil(to) * to
}

fn name16(v: &mut Bytes, name: &[u8]) {
    let mut field = [0u8; 16];
    field[..name.len()].copy_from_slice(name);
    v.extend_from_slice(&field);
}

// ---- Mach-O ----

pub struct MachSection<'t> {
    pub segment: &'t [u8],
    pub name: &'t [u8],
    pub flags: u32,
    pub align_log2: u32,
    pub bytes: &'t [u8],
    /// `(r_address, packed word)` pairs, in file order.
    pub relocs: &'t [(u32, u32)],
}

/// `value` is section-relative for an `N_SECT` symbol (the builder adds the section's address),
/// the raw `n_value` otherwise.
pub struct MachSymbol<'t> {
    pub name: &'t [u8],
    pub n_type: u8,
    pub n_sect: u8,
    pub n_desc: u16,
    pub value: u64,
}

/// `relocation_info`'s second word: `r_symbolnum:24 r_pcrel:1 r_length:2 r_extern:1 r_type:4`.
pub fn reloc_word(symbolnum: u32, pcrel: bool, length: u32, external: bool, kind: u32) -> u32 {
    (symbolnum & 0x00ff_ffff) | (u32::from(pcrel) << 24) | (length << 25) | (u32::from(external) << 27) | (kind << 28)
}

/// The address each section gets: in order from 0, each aligned to its alignment.
pub fn macho_addresses(sections: &[MachSection<'_>]) -> Vec<u64, Alloc<'static>> {
    let mut out = Vec::new_in(Alloc::global());
    let mut addr = 0usize;
    for s in sections {
        addr = align(addr, 1 << s.align_log2);
        out.push(addr as u64);
        addr += s.bytes.len();
    }
    out
}

pub fn macho(sections: &[MachSection<'_>], symbols: &[MachSymbol<'_>]) -> Bytes {
    let n = sections.len();
    let sizeofcmds = 72 + 80 * n + 24;
    let addresses = macho_addresses(sections);

    let mut at = align(32 + sizeofcmds, 8);
    let data_start = at;
    let mut data_at = Vec::new_in(Alloc::global());
    for s in sections {
        data_at.push(at);
        at = align(at + s.bytes.len(), 8);
    }
    let data_end = at;
    let mut reloc_at = Vec::new_in(Alloc::global());
    for s in sections {
        reloc_at.push(at);
        at += 8 * s.relocs.len();
    }
    let symoff = align(at, 8);
    let stroff = symoff + 16 * symbols.len();
    let mut strtab = bytes();
    strtab.push(0);
    let mut strx = Vec::new_in(Alloc::global());
    for sym in symbols {
        strx.push(strtab.len() as u32);
        strtab.extend_from_slice(sym.name);
        strtab.push(0);
    }

    let mut v = bytes();
    // mach_header_64
    put32(&mut v, 0xfeed_facf);
    put32(&mut v, 0x0100_000c);
    put32(&mut v, 0);
    put32(&mut v, 1);
    put32(&mut v, 2);
    put32(&mut v, sizeofcmds as u32);
    put32(&mut v, 0);
    put32(&mut v, 0);
    // LC_SEGMENT_64
    put32(&mut v, 0x19);
    put32(&mut v, (72 + 80 * n) as u32);
    name16(&mut v, b"");
    put64(&mut v, 0);
    put64(&mut v, addresses.last().copied().unwrap_or(0) + sections.last().map_or(0, |s| s.bytes.len() as u64));
    put64(&mut v, data_start as u64);
    put64(&mut v, (data_end - data_start) as u64);
    put32(&mut v, 7);
    put32(&mut v, 7);
    put32(&mut v, n as u32);
    put32(&mut v, 0);
    for (i, s) in sections.iter().enumerate() {
        name16(&mut v, s.name);
        name16(&mut v, s.segment);
        put64(&mut v, addresses[i]);
        put64(&mut v, s.bytes.len() as u64);
        put32(&mut v, data_at[i] as u32);
        put32(&mut v, s.align_log2);
        put32(&mut v, if s.relocs.is_empty() { 0 } else { reloc_at[i] as u32 });
        put32(&mut v, s.relocs.len() as u32);
        put32(&mut v, s.flags);
        put32(&mut v, 0);
        put32(&mut v, 0);
        put32(&mut v, 0);
    }
    // LC_SYMTAB
    put32(&mut v, 0x2);
    put32(&mut v, 24);
    put32(&mut v, symoff as u32);
    put32(&mut v, symbols.len() as u32);
    put32(&mut v, stroff as u32);
    put32(&mut v, strtab.len() as u32);
    // section contents
    for (i, s) in sections.iter().enumerate() {
        pad_to(&mut v, data_at[i]);
        v.extend_from_slice(s.bytes);
    }
    pad_to(&mut v, data_end);
    // relocation tables
    for s in sections {
        for &(address, word) in s.relocs {
            put32(&mut v, address);
            put32(&mut v, word);
        }
    }
    // nlist_64 entries
    pad_to(&mut v, symoff);
    for (i, sym) in symbols.iter().enumerate() {
        let n_value = if sym.n_type & 0x0e == 0x0e { addresses[usize::from(sym.n_sect) - 1] + sym.value } else { sym.value };
        put32(&mut v, strx[i]);
        v.push(sym.n_type);
        v.push(sym.n_sect);
        put16(&mut v, sym.n_desc);
        put64(&mut v, n_value);
    }
    v.extend_from_slice(&strtab);
    v
}

// ---- ELF ----

pub struct ElfSection<'t> {
    pub name: &'t [u8],
    pub kind: u32,
    pub flags: u64,
    pub align: u64,
    pub bytes: &'t [u8],
    pub link: u32,
    pub info: u32,
    pub entsize: u64,
}

pub struct ElfSymbol<'t> {
    pub name: &'t [u8],
    pub info: u8,
    pub other: u8,
    pub shndx: u16,
    pub value: u64,
    pub size: u64,
}

/// `.symtab`'s and `.strtab`'s contents for `symbols` (the null symbol 0 is added first).
pub fn elf_symtab(symbols: &[ElfSymbol<'_>]) -> (Bytes, Bytes) {
    let mut table = bytes();
    let mut strings = bytes();
    strings.push(0);
    table.extend_from_slice(&[0u8; 24]);
    for s in symbols {
        let st_name = if s.name.is_empty() { 0 } else { strings.len() as u32 };
        if !s.name.is_empty() {
            strings.extend_from_slice(s.name);
            strings.push(0);
        }
        put32(&mut table, st_name);
        table.push(s.info);
        table.push(s.other);
        put16(&mut table, s.shndx);
        put64(&mut table, s.value);
        put64(&mut table, s.size);
    }
    (table, strings)
}

/// `Elf64_Rela` entries: `(r_offset, symbol, type, r_addend)`.
pub fn elf_rela(entries: &[(u64, u32, u32, i64)]) -> Bytes {
    let mut v = bytes();
    for &(offset, symbol, kind, addend) in entries {
        put64(&mut v, offset);
        put64(&mut v, (u64::from(symbol) << 32) | u64::from(kind));
        put64(&mut v, addend as u64);
    }
    v
}

/// `SHT_GROUP` contents: the flag word, then the member section indexes.
pub fn elf_group(flags: u32, members: &[u32]) -> Bytes {
    let mut v = bytes();
    put32(&mut v, flags);
    for &m in members {
        put32(&mut v, m);
    }
    v
}

/// An ELF64 AArch64 `ET_REL`: section 0 is the null header, `sections` are 1.., and
/// `.shstrtab` is appended as the last section.
pub fn elf(sections: &[ElfSection<'_>]) -> Bytes {
    let mut shstrtab = bytes();
    shstrtab.push(0);
    let mut names = Vec::new_in(Alloc::global());
    for s in sections {
        names.push(shstrtab.len() as u32);
        shstrtab.extend_from_slice(s.name);
        shstrtab.push(0);
    }
    let shstrtab_name = shstrtab.len() as u32;
    shstrtab.extend_from_slice(b".shstrtab\0");

    let mut at = 64usize;
    let mut data_at = Vec::new_in(Alloc::global());
    for s in sections {
        at = align(at, 8);
        data_at.push(at);
        if s.kind != 8 {
            at += s.bytes.len();
        }
    }
    let shstrtab_at = align(at, 8);
    let shoff = align(shstrtab_at + shstrtab.len(), 8);
    let shnum = sections.len() + 2;

    let mut v = bytes();
    v.extend_from_slice(&[0x7f, b'E', b'L', b'F', 2, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    put16(&mut v, 1); // ET_REL
    put16(&mut v, 183); // EM_AARCH64
    put32(&mut v, 1);
    put64(&mut v, 0); // e_entry
    put64(&mut v, 0); // e_phoff
    put64(&mut v, shoff as u64);
    put32(&mut v, 0); // e_flags
    put16(&mut v, 64); // e_ehsize
    put16(&mut v, 0);
    put16(&mut v, 0);
    put16(&mut v, 64); // e_shentsize
    put16(&mut v, shnum as u16);
    put16(&mut v, (shnum - 1) as u16);
    for (i, s) in sections.iter().enumerate() {
        if s.kind != 8 {
            pad_to(&mut v, data_at[i]);
            v.extend_from_slice(s.bytes);
        }
    }
    pad_to(&mut v, shstrtab_at);
    v.extend_from_slice(&shstrtab);
    pad_to(&mut v, shoff);
    // the null header
    v.extend_from_slice(&[0u8; 64]);
    let header = |v: &mut Bytes, name: u32, kind: u32, flags: u64, offset: u64, size: u64, link: u32, info: u32, align: u64, entsize: u64| {
        put32(v, name);
        put32(v, kind);
        put64(v, flags);
        put64(v, 0);
        put64(v, offset);
        put64(v, size);
        put32(v, link);
        put32(v, info);
        put64(v, align);
        put64(v, entsize);
    };
    for (i, s) in sections.iter().enumerate() {
        header(&mut v, names[i], s.kind, s.flags, data_at[i] as u64, s.bytes.len() as u64, s.link, s.info, s.align, s.entsize);
    }
    header(&mut v, shstrtab_name, 3, 0, shstrtab_at as u64, shstrtab.len() as u64, 0, 0, 1, 0);
    v
}

// ---- ar ----

pub struct Ar {
    pub bytes: Bytes,
    /// Each member header's offset, in the order written.
    pub offsets: Vec<u32, Alloc<'static>>,
}

impl Ar {
    pub fn start() -> Ar {
        let mut bytes = bytes();
        bytes.extend_from_slice(b"!<arch>\n");
        Ar { bytes, offsets: Vec::new_in(Alloc::global()) }
    }

    /// One member: `name` as it stands in `ar_name` (`#1/12`, `foo.o/`, `/0`, `//`), and its
    /// data (a BSD long name's bytes included by the caller).
    pub fn member(&mut self, name: &[u8], data: &[u8]) {
        self.offsets.push(self.bytes.len() as u32);
        field(&mut self.bytes, name, 16);
        field(&mut self.bytes, b"0", 12);
        field(&mut self.bytes, b"0", 6);
        field(&mut self.bytes, b"0", 6);
        field(&mut self.bytes, b"644", 8);
        let mut digits = [0u8; 10];
        let mut len = 0;
        let mut size = data.len();
        loop {
            digits[len] = b'0' + (size % 10) as u8;
            len += 1;
            size /= 10;
            if size == 0 {
                break;
            }
        }
        digits[..len].reverse();
        field(&mut self.bytes, &digits[..len], 10);
        self.bytes.extend_from_slice(b"`\n");
        self.bytes.extend_from_slice(data);
        if data.len() % 2 == 1 {
            self.bytes.push(b'\n');
        }
    }
}

/// `text` padded with spaces to `width`.
fn field(v: &mut Bytes, text: &[u8], width: usize) {
    v.extend_from_slice(text);
    for _ in text.len()..width {
        v.push(b' ');
    }
}
