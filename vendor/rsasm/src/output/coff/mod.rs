//! PE/COFF relocatable object output, for x86-64 (AMD64), i386 and ARM64.
//!
//! Only the object form is written — no PE image, no optional header: rsasm
//! is an assembler, and `link.exe`, `lld-link` or mingw's `ld` does the rest.
//!
//! The shape of the file follows llvm-mc's, which is the reference all three
//! machines are checked against (`tools/coff-diff`). Two conventions are
//! worth naming, because they are what makes COFF different from ELF rather
//! than merely differently spelled:
//!
//! - **The addend lives in the bytes.** A COFF relocation is ten bytes with
//!   nowhere to put one, so `call foo+4` writes the 4 into the displacement
//!   field, as a `REL` psABI would. What "here" means differs per relocation
//!   as well, which is what `reloc::pc_base` carries.
//! - **Sections carry their own symbol.** Every section has a symbol with an
//!   auxiliary record giving its length, relocation count and a checksum of
//!   its bytes, which is also where a COMDAT's selection is recorded.
//!
//! `.text`, `.data` and `.bss` are always present, in that order, even when
//! empty, as llvm-mc writes them; anything else follows in the order the
//! source named it.

pub(crate) mod reloc;

use super::OutputError;
use crate::assembler::Assembler;
use crate::coff::{self, Comdat};
use crate::section::{SectionFlags, SectionId, SectionKind};
use crate::symbol::{Binding, SymbolId, SymbolValue};
use std::collections::HashMap;

pub(crate) const MACHINE_I386: u16 = 0x14c;
pub(crate) const MACHINE_AMD64: u16 = 0x8664;
pub(crate) const MACHINE_ARM64: u16 = 0xaa64;

// Section characteristics (`IMAGE_SCN_*`).
pub(crate) const SCN_CNT_CODE: u32 = 0x0000_0020;
pub(crate) const SCN_CNT_INITIALIZED_DATA: u32 = 0x0000_0040;
pub(crate) const SCN_CNT_UNINITIALIZED_DATA: u32 = 0x0000_0080;
pub(crate) const SCN_LNK_INFO: u32 = 0x0000_0200;
pub(crate) const SCN_LNK_REMOVE: u32 = 0x0000_0800;
pub(crate) const SCN_LNK_COMDAT: u32 = 0x0000_1000;
pub(crate) const SCN_ALIGN_MASK: u32 = 0x00f0_0000;
pub(crate) const SCN_MEM_DISCARDABLE: u32 = 0x0200_0000;
pub(crate) const SCN_MEM_SHARED: u32 = 0x1000_0000;
pub(crate) const SCN_MEM_EXECUTE: u32 = 0x2000_0000;
pub(crate) const SCN_MEM_READ: u32 = 0x4000_0000;
pub(crate) const SCN_MEM_WRITE: u32 = 0x8000_0000;

// Storage classes (`IMAGE_SYM_CLASS_*`).
pub(crate) const SYM_CLASS_EXTERNAL: u8 = 2;
pub(crate) const SYM_CLASS_STATIC: u8 = 3;
pub(crate) const SYM_CLASS_FILE: u8 = 103;
pub(crate) const SYM_CLASS_WEAK_EXTERNAL: u8 = 105;

/// `IMAGE_SYM_DTYPE_FUNCTION` in the high half of a symbol's type, which is
/// what `.def foo; .type 32; .endef` records.
pub(crate) const SYM_TYPE_FUNCTION: u16 = 0x20;

const SYM_UNDEFINED: i16 = 0;
const SYM_ABSOLUTE: i16 = -1;
const SYM_DEBUG: i16 = -2;

/// `IMAGE_COMDAT_SELECT_ASSOCIATIVE`: the section is kept exactly when the
/// one its auxiliary record names is.
pub(crate) const SELECT_ASSOCIATIVE: u8 = 5;

/// `IMAGE_WEAK_EXTERN_SEARCH_ALIAS`: the linker uses the aliased symbol if
/// nothing else defines the name.
const WEAK_SEARCH_ALIAS: u32 = 3;

const HEADER_SIZE: u64 = 20;
const SECTION_HEADER_SIZE: u64 = 40;

/// The COFF machine an architecture's objects are for, or `None` for a target
/// Windows has never run on, which has no machine number to write.
pub(crate) fn machine(arch: &dyn crate::arch::Architecture) -> Option<u16> {
    match arch.elf_machine() {
        3 => Some(MACHINE_I386),
        62 => Some(MACHINE_AMD64),
        183 => Some(MACHINE_ARM64),
        _ => None,
    }
}

/// Alignment padding in x86 code as llvm-mc writes it, which is the reference
/// for COFF objects, where the backend's own no-ops follow GNU as for ELF.
/// `None` where the two agree or llvm-mc has nothing different to say.
///
/// For x86-64, llvm-mc takes the longest no-op it can, up to fifteen bytes:
/// the multi-byte `nopw` forms up to ten, and `0x66` prefixes on the ten-byte
/// one beyond that. For i386 its default Windows CPU has no `nopl`, so the
/// padding is all one-byte `nop`s. Sixteen-bit code keeps the backend's.
pub(crate) fn nop_fill(
    arch: &dyn crate::arch::Architecture,
    state: &crate::arch::ArchState,
    len: usize,
) -> Option<Vec<u8>> {
    #[rustfmt::skip]
    const NOPS: [&[u8]; 10] = [
        &[0x90],
        &[0x66, 0x90],
        &[0x0f, 0x1f, 0x00],
        &[0x0f, 0x1f, 0x40, 0x00],
        &[0x0f, 0x1f, 0x44, 0x00, 0x00],
        &[0x66, 0x0f, 0x1f, 0x44, 0x00, 0x00],
        &[0x0f, 0x1f, 0x80, 0x00, 0x00, 0x00, 0x00],
        &[0x0f, 0x1f, 0x84, 0x00, 0x00, 0x00, 0x00, 0x00],
        &[0x66, 0x0f, 0x1f, 0x84, 0x00, 0x00, 0x00, 0x00, 0x00],
        &[0x66, 0x2e, 0x0f, 0x1f, 0x84, 0x00, 0x00, 0x00, 0x00, 0x00],
    ];
    if state.bits < 32 {
        return None;
    }
    match machine(arch)? {
        MACHINE_I386 => Some(vec![0x90; len]),
        MACHINE_AMD64 => {
            let mut out = Vec::with_capacity(len);
            let mut left = len;
            while left > 0 {
                let n = left.min(15);
                let prefixes = n.saturating_sub(10);
                out.resize(out.len() + prefixes, 0x66);
                out.extend_from_slice(NOPS[n - prefixes - 1]);
                left -= n;
            }
            Some(out)
        }
        _ => None,
    }
}

/// The alignment a section starts with, which llvm-mc gives by name: the
/// three it creates itself are four-byte aligned, and a section the source
/// names gets no alignment of its own until something in it asks.
pub(crate) fn default_align(name: &str) -> u64 {
    match name {
        ".text" | ".data" | ".bss" => 4,
        _ => 1,
    }
}

/// The characteristics of the sections llvm-mc knows by name, which it
/// keeps whatever flags `.section` gives them: `.section .rdata,"dw"` is
/// still read-only, and `.drectve` is always linker information.
///
/// These are the sections LLVM's object file description creates up front,
/// for its own code generation; each was checked by naming it with flags
/// that would otherwise give different characteristics.
pub(crate) fn preset_characteristics(name: &str) -> Option<u32> {
    const RDATA: u32 = SCN_CNT_INITIALIZED_DATA | SCN_MEM_READ;
    const DEBUG: u32 = SCN_CNT_INITIALIZED_DATA | SCN_MEM_DISCARDABLE | SCN_MEM_READ;
    Some(match name {
        ".text" => SCN_CNT_CODE | SCN_MEM_EXECUTE | SCN_MEM_READ,
        ".data" | ".tls$" => SCN_CNT_INITIALIZED_DATA | SCN_MEM_READ | SCN_MEM_WRITE,
        ".bss" => SCN_CNT_UNINITIALIZED_DATA | SCN_MEM_READ | SCN_MEM_WRITE,
        ".rdata" | ".xdata" | ".pdata" | ".eh_frame" | ".llvm_stackmaps" | ".gfids$y"
        | ".gljmp$y" | ".giats$y" | ".gehcont$y" => RDATA,
        ".drectve" => SCN_LNK_INFO | SCN_LNK_REMOVE,
        ".sxdata" => SCN_LNK_INFO,
        ".debug_abbrev"
        | ".debug_info"
        | ".debug_line"
        | ".debug_line_str"
        | ".debug_str"
        | ".debug_str_offsets"
        | ".debug_frame"
        | ".debug_loc"
        | ".debug_loclists"
        | ".debug_ranges"
        | ".debug_rnglists"
        | ".debug_aranges"
        | ".debug_addr"
        | ".debug_macinfo"
        | ".debug_macro"
        | ".debug_names"
        | ".debug_pubnames"
        | ".debug_pubtypes"
        | ".debug_gnu_pubnames"
        | ".debug_gnu_pubtypes"
        | ".debug_cu_index"
        | ".debug_tu_index"
        | ".debug_abbrev.dwo"
        | ".debug_info.dwo"
        | ".debug$S"
        | ".debug$T"
        | ".debug$H"
        | ".apple_names"
        | ".pseudo_probe"
        | ".pseudo_probe_desc" => DEBUG,
        _ => return None,
    })
}

/// The characteristics of a section the source never described with COFF
/// flags: the three standard ones, whatever a dialect's own section
/// directive made, and the DWARF sections.
pub(crate) fn default_characteristics(name: &str, kind: SectionKind, flags: &SectionFlags) -> u32 {
    if let Some(v) = preset_characteristics(name) {
        return v;
    }
    let mut v = match () {
        _ if name == ".text" || flags.exec => SCN_CNT_CODE | SCN_MEM_EXECUTE | SCN_MEM_READ,
        _ if kind == SectionKind::Nobits => {
            SCN_CNT_UNINITIALIZED_DATA | SCN_MEM_READ | SCN_MEM_WRITE
        }
        _ if flags.write => SCN_CNT_INITIALIZED_DATA | SCN_MEM_READ | SCN_MEM_WRITE,
        _ => SCN_CNT_INITIALIZED_DATA | SCN_MEM_READ,
    };
    if name.starts_with(".debug") {
        v |= SCN_MEM_DISCARDABLE;
    }
    v
}

/// The characteristics `.section name,"flags"` asks for, as llvm-mc's COFF
/// parser reads the letters.
///
/// The letters do not map one to one onto bits: `x` implies code, execute and
/// read but not write, a later `w` puts write back, and `y` takes read away
/// for good. So they are collected as intentions first and turned into bits
/// once, the way llvm-mc does it.
pub(crate) fn parse_flags(name: &str, letters: &str) -> Result<u32, char> {
    #[derive(Default)]
    struct Want {
        alloc: bool,
        code: bool,
        load: bool,
        init: bool,
        shared: bool,
        noload: bool,
        noread: bool,
        nowrite: bool,
        discardable: bool,
        info: bool,
        any: bool,
    }
    let mut w = Want::default();
    // Whether a `w` has been seen since the last `r`, which keeps a later
    // `x` from making the section read-only again.
    let mut writable = false;
    for c in letters.chars() {
        match c {
            // ELF's "allocated", which COFF says with the content flags.
            'a' => continue,
            'b' => {
                w.alloc = true;
                w.load = false;
            }
            'd' => {
                w.init = true;
                w.nowrite = false;
                w.load |= !w.noload;
            }
            'n' => {
                w.noload = true;
                w.load = false;
            }
            'D' => w.discardable = true,
            'r' => {
                writable = false;
                w.nowrite = true;
                w.init |= !w.code;
                w.load |= !w.noload;
            }
            's' => {
                w.shared = true;
                w.init = true;
                w.nowrite = false;
                w.load |= !w.noload;
            }
            'w' => {
                w.nowrite = false;
                writable = true;
            }
            'x' => {
                w.code = true;
                w.load |= !w.noload;
                w.nowrite |= !writable;
            }
            'y' => {
                w.noread = true;
                w.nowrite = true;
            }
            'i' => w.info = true,
            other => return Err(other),
        }
        w.any = true;
    }
    if !w.any {
        w.init = true;
    }
    let mut v = 0;
    if w.code {
        v |= SCN_CNT_CODE | SCN_MEM_EXECUTE;
    }
    if w.init {
        v |= SCN_CNT_INITIALIZED_DATA;
    }
    if w.alloc && !w.load {
        v |= SCN_CNT_UNINITIALIZED_DATA;
    }
    if w.noload {
        v |= SCN_LNK_REMOVE;
    }
    if w.discardable || name.starts_with(".debug") {
        v |= SCN_MEM_DISCARDABLE;
    }
    if !w.noread {
        v |= SCN_MEM_READ;
    }
    if !w.nowrite {
        v |= SCN_MEM_WRITE;
    }
    if w.shared {
        v |= SCN_MEM_SHARED;
    }
    if w.info {
        v |= SCN_LNK_INFO;
    }
    Ok(v)
}

/// The alignment bits of a section's characteristics: `IMAGE_SCN_ALIGN_n` is
/// `log2(n) + 1` in bits 20 to 23, and 8192 is the most COFF can say.
fn align_bits(align: u64) -> u32 {
    let n = align.clamp(1, 8192).next_power_of_two().trailing_zeros();
    (n + 1) << 20
}

/// The checksum an auxiliary section record carries: a CRC-32 of the
/// section's bytes with the initial value zero and no final inversion, which
/// is what LLVM calls `JamCRC`. An uninitialized section has none.
fn jam_crc(data: &[u8]) -> u32 {
    let mut crc = 0u32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let bit = crc & 1;
            crc >>= 1;
            if bit != 0 {
                crc ^= 0xedb8_8320;
            }
        }
    }
    crc
}

/// The string table: names longer than eight bytes live here and the symbol
/// or section header points at them.
struct StrTab {
    bytes: Vec<u8>,
    seen: HashMap<String, u32>,
}

impl StrTab {
    fn new() -> StrTab {
        // The first four bytes are the table's own size.
        StrTab {
            bytes: vec![0, 0, 0, 0],
            seen: HashMap::new(),
        }
    }

    fn add(&mut self, s: &str) -> u32 {
        if let Some(&o) = self.seen.get(s) {
            return o;
        }
        let off = self.bytes.len() as u32;
        self.bytes.extend_from_slice(s.as_bytes());
        self.bytes.push(0);
        self.seen.insert(s.to_string(), off);
        off
    }

    fn finish(&mut self) {
        let size = self.bytes.len() as u32;
        self.bytes[..4].copy_from_slice(&size.to_le_bytes());
    }
}

/// One symbol table entry, plus whatever auxiliary records follow it.
struct OutSym {
    name: String,
    value: u32,
    section: i16,
    ty: u16,
    class: u8,
    aux: Vec<Aux>,
}

enum Aux {
    /// A section definition, on a section's own symbol.
    Section {
        length: u32,
        relocs: u16,
        checksum: u32,
        number: u16,
        selection: u8,
    },
    /// The definition a weak external falls back on.
    Weak { tag: u32 },
    /// A `.file` name, which may take several records.
    File(String),
}

impl Aux {
    fn count(&self) -> usize {
        match self {
            Aux::File(name) => name.len().div_ceil(18).max(1),
            _ => 1,
        }
    }
}

/// A little-endian byte sink. Every COFF field is little-endian whatever the
/// machine, unlike ELF, whose headers follow the target's byte order.
#[derive(Default)]
struct Buf {
    out: Vec<u8>,
}

impl Buf {
    fn u8(&mut self, v: u8) {
        self.out.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.out.extend_from_slice(&v.to_le_bytes());
    }
    fn i16(&mut self, v: i16) {
        self.out.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.out.extend_from_slice(&v.to_le_bytes());
    }
    fn zeros(&mut self, n: usize) {
        self.out.resize(self.out.len() + n, 0);
    }
    fn len(&self) -> u64 {
        self.out.len() as u64
    }
}

/// A section as the file will hold it.
struct OutSec {
    id: SectionId,
    name: String,
    characteristics: u32,
    data: Vec<u8>,
    relocs: Vec<crate::assembler::Relocation>,
    /// Index in the section table, counting from one.
    number: u16,
    comdat: Option<Comdat>,
}

pub fn build(asm: &Assembler) -> Result<Vec<u8>, OutputError> {
    let target = asm.target();
    let machine = machine(target).ok_or_else(|| {
        OutputError::Unsupported(format!(
            "COFF output has no machine number for `{}`; use `-f elf`",
            target.name()
        ))
    })?;

    // ---- sections ---------------------------------------------------------
    let mut order: Vec<SectionId> = Vec::new();
    let mut standard: Vec<(&str, Option<SectionId>)> =
        vec![(".text", None), (".data", None), (".bss", None)];
    for s in &asm.sections {
        let name = asm.interner.get(s.name);
        match standard.iter_mut().find(|(n, _)| *n == name) {
            Some((_, slot)) => *slot = Some(s.id),
            None => order.push(s.id),
        }
    }
    let mut secs: Vec<OutSec> = Vec::new();
    let mut number = 0u16;
    // NASM writes the sections the source named, in that order, and the
    // default `.text` only if something went into it.
    let nasm = asm.options.dialect == crate::lexer::Dialect::Nasm;
    if nasm {
        standard.clear();
        order = asm
            .sections
            .iter()
            .filter(|s| s.size > 0 || asm.coff.sections.contains_key(&s.id))
            .map(|s| s.id)
            .collect();
    }
    // The three llvm-mc always writes come first, made up where the source
    // never used them, then everything else in the order it was named.
    for (name, id) in &standard {
        number += 1;
        secs.push(match id {
            Some(id) => out_section(asm, *id, number),
            None => OutSec {
                id: SectionId(u32::MAX),
                name: (*name).to_string(),
                characteristics: default_characteristics(
                    name,
                    if *name == ".bss" {
                        SectionKind::Nobits
                    } else {
                        SectionKind::Progbits
                    },
                    &match *name {
                        ".text" => SectionFlags::text(),
                        ".data" => SectionFlags::data(),
                        _ => SectionFlags::bss(),
                    },
                ) | align_bits(default_align(name)),
                data: Vec::new(),
                relocs: Vec::new(),
                number,
                comdat: None,
            },
        });
    }
    // An associative COMDAT is numbered after every section that is not
    // one, so that it never points forward at the section it goes with
    // (`WinCOFFWriter::assignSectionNumbers`, which says MSVC's linker cannot
    // read a forward reference).
    order.sort_by_key(|id| {
        asm.coff
            .sections
            .get(id)
            .and_then(|i| i.comdat)
            .is_some_and(|c| c.selection == SELECT_ASSOCIATIVE)
    });
    for id in order {
        number += 1;
        secs.push(out_section(asm, id, number));
    }
    let sec_number: HashMap<SectionId, i16> =
        secs.iter().map(|s| (s.id, s.number as i16)).collect();

    // ---- symbols ----------------------------------------------------------
    let (syms, sym_index) = collect_symbols(asm, &secs, &sec_number, nasm);

    // ---- lay the file out -------------------------------------------------
    let mut buf = Buf::default();
    buf.zeros((HEADER_SIZE + SECTION_HEADER_SIZE * secs.len() as u64) as usize);
    // Each section's bytes are followed by its own relocations, as llvm-mc
    // writes them, with no padding anywhere.
    let mut placed: Vec<(u32, u32)> = Vec::new();
    for s in &secs {
        let uninitialized = s.characteristics & SCN_CNT_UNINITIALIZED_DATA != 0;
        // An empty section still points at where its bytes would be, as
        // llvm-mc writes it; an uninitialized one has no bytes to point at.
        let data_at = if uninitialized {
            0
        } else {
            let at = buf.len() as u32;
            buf.out.extend_from_slice(&s.data);
            at
        };
        let reloc_at = if s.relocs.is_empty() {
            0
        } else {
            let at = buf.len() as u32;
            for r in &s.relocs {
                buf.u32(r.offset as u32);
                buf.u32(
                    r.symbol
                        .and_then(|s| sym_index.get(&s).copied())
                        .unwrap_or(0),
                );
                buf.u16(r.kind as u16);
            }
            at
        };
        placed.push((data_at, reloc_at));
    }

    let symbol_table_at = buf.len() as u32;
    let mut strtab = StrTab::new();
    let mut nsyms = 0u32;
    for s in &syms {
        nsyms += 1 + s.aux.iter().map(|a| a.count() as u32).sum::<u32>();
        write_symbol(&mut buf, s, &mut strtab);
    }
    // A section name longer than eight bytes lives in the string table too,
    // and the header points at it by offset, so they go in before the table
    // is closed.
    for s in &secs {
        if s.name.len() > 8 {
            strtab.add(&s.name);
        }
    }
    strtab.finish();
    buf.out.extend_from_slice(&strtab.bytes);

    // ---- headers ----------------------------------------------------------
    let mut hdr = Buf::default();
    hdr.u16(machine);
    hdr.u16(secs.len() as u16);
    hdr.u32(0); // TimeDateStamp: zero, so the same source gives the same object
    hdr.u32(symbol_table_at);
    hdr.u32(nsyms);
    hdr.u16(0); // no optional header in an object
    hdr.u16(0); // Characteristics
    for (s, (data_at, reloc_at)) in secs.iter().zip(&placed) {
        let mut name = [0u8; 8];
        if s.name.len() <= 8 {
            name[..s.name.len()].copy_from_slice(s.name.as_bytes());
        } else {
            // A long section name is `/` and its decimal offset in the
            // string table, which is only written once the table is built.
            let off = strtab.seen[&s.name];
            let text = format!("/{off}");
            name[..text.len()].copy_from_slice(text.as_bytes());
        }
        hdr.out.extend_from_slice(&name);
        hdr.u32(0); // VirtualSize: zero in an object
        hdr.u32(0); // VirtualAddress
        hdr.u32(s.data.len() as u32);
        hdr.u32(*data_at);
        hdr.u32(*reloc_at);
        hdr.u32(0); // PointerToLineNumbers
        if s.relocs.len() > u16::MAX as usize {
            return Err(OutputError::Unsupported(format!(
                "section `{}` has {} relocations, more than a COFF section header can count",
                s.name,
                s.relocs.len()
            )));
        }
        hdr.u16(s.relocs.len() as u16);
        hdr.u16(0); // NumberOfLinenumbers
        hdr.u32(s.characteristics);
    }
    buf.out[..hdr.out.len()].copy_from_slice(&hdr.out);
    Ok(buf.out)
}

/// A long name is written as four zero bytes and an offset into the string
/// table; a short one sits in the eight bytes themselves.
fn write_symbol(buf: &mut Buf, s: &OutSym, strtab: &mut StrTab) {
    if s.name.len() <= 8 {
        let mut name = [0u8; 8];
        name[..s.name.len()].copy_from_slice(s.name.as_bytes());
        buf.out.extend_from_slice(&name);
    } else {
        let off = strtab.add(&s.name);
        buf.u32(0);
        buf.u32(off);
    }
    buf.u32(s.value);
    buf.i16(s.section);
    buf.u16(s.ty);
    buf.u8(s.class);
    buf.u8(s.aux.iter().map(|a| a.count() as u8).sum());
    for a in &s.aux {
        match a {
            Aux::Section {
                length,
                relocs,
                checksum,
                number,
                selection,
            } => {
                buf.u32(*length);
                buf.u16(*relocs);
                buf.u16(0); // NumberOfLinenumbers
                buf.u32(*checksum);
                buf.u16(*number);
                buf.u8(*selection);
                buf.zeros(3);
            }
            Aux::Weak { tag } => {
                buf.u32(*tag);
                buf.u32(WEAK_SEARCH_ALIAS);
                buf.zeros(10);
            }
            Aux::File(name) => {
                let n = a.count() * 18;
                let bytes = name.as_bytes();
                buf.out.extend_from_slice(bytes);
                buf.zeros(n - bytes.len());
            }
        }
    }
}

fn out_section(asm: &Assembler, id: SectionId, number: u16) -> OutSec {
    let s = asm.section(id);
    let name = coff::section_name(asm.interner.get(s.name)).to_string();
    let info = asm.coff.sections.get(&id);
    let base = match info {
        Some(i) => i.characteristics,
        None => default_characteristics(&name, s.kind, &s.flags),
    };
    let comdat = info.and_then(|i| i.comdat);
    let mut characteristics = base | align_bits(s.align);
    if comdat.is_some() {
        characteristics |= SCN_LNK_COMDAT;
    }
    let uninitialized = characteristics & SCN_CNT_UNINITIALIZED_DATA != 0;
    let data = if uninitialized {
        // An uninitialized section still records its size; it just has no
        // bytes in the file.
        vec![0; s.size as usize]
    } else {
        asm.section_bytes(id)
    };
    let relocs = asm
        .relocs
        .iter()
        .filter(|r| r.section == id)
        .cloned()
        .collect();
    OutSec {
        id,
        name,
        characteristics,
        data,
        relocs,
        number,
        comdat,
    }
}

/// Builds the symbol table and the index every relocation names.
///
/// The order is llvm-mc's: each section's own symbol, in section order, with
/// a COMDAT's symbol directly after the section it selects, then the symbols
/// of the source in the order it first mentioned them, and the `.file` names
/// last.
fn collect_symbols(
    asm: &Assembler,
    secs: &[OutSec],
    sec_number: &HashMap<SectionId, i16>,
    nasm: bool,
) -> (Vec<OutSym>, HashMap<SymbolId, u32>) {
    let mut out: Vec<OutSym> = Vec::new();
    let mut index: HashMap<SymbolId, u32> = HashMap::new();
    // Auxiliary records take symbol table slots of their own, so an entry's
    // index is not its position in the list.
    fn push(out: &mut Vec<OutSym>, next: &mut u32, s: OutSym) -> u32 {
        let at = *next;
        *next += 1 + s.aux.iter().map(|a| a.count() as u32).sum::<u32>();
        out.push(s);
        at
    }
    let mut next = 0u32;

    let mut placed: Vec<SymbolId> = Vec::new();
    for s in secs {
        let uninitialized = s.characteristics & SCN_CNT_UNINITIALIZED_DATA != 0;
        // A relocation against a local label names the section's symbol,
        // which the assembler made when it needed one.
        if let Some(sym) = asm.sections.get(s.id.0 as usize).and_then(|x| x.sym) {
            index.insert(sym, next);
        }
        push(
            &mut out,
            &mut next,
            OutSym {
                name: s.name.clone(),
                value: 0,
                section: s.number as i16,
                ty: 0,
                class: SYM_CLASS_STATIC,
                aux: vec![Aux::Section {
                    length: s.data.len() as u32,
                    relocs: s.relocs.len().min(u16::MAX as usize) as u16,
                    // NASM leaves the checksum out.
                    checksum: if uninitialized || nasm {
                        0
                    } else {
                        jam_crc(&s.data)
                    },
                    number: associated(asm, s, sec_number).unwrap_or(s.number),
                    selection: s.comdat.map_or(0, |c| c.selection),
                }],
            },
        );
        // An associative section's symbol belongs to the section it goes
        // with, and is placed there.
        if let Some(id) = s
            .comdat
            .filter(|c| c.selection != SELECT_ASSOCIATIVE)
            .and_then(|c| c.symbol)
            && !placed.contains(&id)
        {
            let Some(sym) = out_symbol(asm, id, sec_number) else {
                continue;
            };
            index.insert(id, push(&mut out, &mut next, sym));
            placed.push(id);
        }
    }

    // The name a weak external's definition hides behind, which llvm-mc
    // builds from the first defined global in the object.
    let first_global = asm
        .symbols
        .iter()
        .find(|(_, s)| s.binding == Binding::Global && s.is_defined())
        .map(|(_, s)| asm.interner.get(s.name).to_string());

    for (id, sym) in asm.symbols.iter() {
        if placed.contains(&id) || !coff::keeps_symbol(asm, id) {
            continue;
        }
        if sym.binding == Binding::Weak {
            // A weak symbol is an undefined external with an auxiliary
            // record naming the definition to fall back on, which is where
            // the value goes. The alias itself is a plain external under a
            // name no source can collide with.
            let name = asm.interner.get(sym.name).to_string();
            let alias = match &first_global {
                Some(g) => format!(".weak.{name}.default.{g}"),
                None => format!(".weak.{name}.default"),
            };
            let at = next;
            index.insert(id, at);
            push(
                &mut out,
                &mut next,
                OutSym {
                    name,
                    value: 0,
                    section: SYM_UNDEFINED,
                    ty: coff::symbol_type(asm, id),
                    class: SYM_CLASS_WEAK_EXTERNAL,
                    aux: vec![Aux::Weak { tag: at + 2 }],
                },
            );
            let (section, value) = match out_symbol(asm, id, sec_number) {
                Some(s) if s.section > 0 => (s.section, s.value),
                _ => (SYM_ABSOLUTE, 0),
            };
            push(
                &mut out,
                &mut next,
                OutSym {
                    name: alias,
                    value,
                    section,
                    ty: 0,
                    class: SYM_CLASS_EXTERNAL,
                    aux: Vec::new(),
                },
            );
            continue;
        }
        let Some(s) = out_symbol(asm, id, sec_number) else {
            continue;
        };
        index.insert(id, push(&mut out, &mut next, s));
    }

    // NASM names the source file whether or not the source did, and adds
    // two absolute symbols of its own: `.absolut`, and in an i386 object
    // `@feat.00`, whose 1 tells the linker the object is safe for SAFESEH.
    let mut files = asm.coff.files.clone();
    if nasm {
        if files.is_empty()
            && let Some(f) = asm
                .sm
                .files()
                .iter()
                .find(|f| !f.name.to_string_lossy().starts_with('<'))
        {
            files.push(f.name.to_string_lossy().into_owned());
        }
        let mut extra = vec![(".absolut", 0)];
        if machine(asm.target()) == Some(MACHINE_I386) {
            extra.push(("@feat.00", 1));
        }
        for (name, value) in extra {
            push(
                &mut out,
                &mut next,
                OutSym {
                    name: name.to_string(),
                    value,
                    section: SYM_ABSOLUTE,
                    ty: 0,
                    class: SYM_CLASS_STATIC,
                    aux: Vec::new(),
                },
            );
        }
    }
    for name in &files {
        push(
            &mut out,
            &mut next,
            OutSym {
                name: ".file".to_string(),
                value: 0,
                section: SYM_DEBUG,
                ty: 0,
                class: SYM_CLASS_FILE,
                aux: vec![Aux::File(name.clone())],
            },
        );
    }
    (out, index)
}

/// For an associative COMDAT, the number of the section it goes with: the one
/// its symbol is defined in, which is what the auxiliary record's section
/// number holds instead of the section's own.
fn associated(asm: &Assembler, s: &OutSec, sec_number: &HashMap<SectionId, i16>) -> Option<u16> {
    let c = s.comdat.filter(|c| c.selection == SELECT_ASSOCIATIVE)?;
    let section = asm.symbol_section(c.symbol?)?;
    sec_number.get(&section).map(|n| *n as u16)
}

/// One ordinary symbol, or `None` if it belongs to a section that was not
/// written.
fn out_symbol(
    asm: &Assembler,
    id: SymbolId,
    sec_number: &HashMap<SectionId, i16>,
) -> Option<OutSym> {
    let sym = asm.symbols.get(id);
    let name = asm.interner.get(sym.name).to_string();
    let (section, value) = match &sym.value {
        SymbolValue::Label { section, .. } => {
            (*sec_number.get(section)?, asm.symbol_addr(id).unwrap_or(0))
        }
        // A common block is an undefined external whose value is its size,
        // which is all COFF records; the alignment is the linker's choice.
        SymbolValue::Common { size, .. } => (SYM_UNDEFINED, *size as i64),
        SymbolValue::Expr(_) => match asm.symbol_target_section(id) {
            Some((section, off)) => (*sec_number.get(&section)?, off as i64),
            None => (SYM_ABSOLUTE, asm.symbol_number(id).unwrap_or(0)),
        },
        SymbolValue::Undefined => (SYM_UNDEFINED, 0),
    };
    let class = coff::storage_class(asm, id);
    Some(OutSym {
        name,
        value: value as u32,
        section,
        ty: coff::symbol_type(asm, id),
        class,
        aux: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_checksum_is_llvms_jamcrc() {
        // Taken from an llvm-mc object: the auxiliary record for a `.text` of
        // these eleven bytes carries 0xf2f0fe1d.
        let data = [0xb8, 1, 0, 0, 0, 0xe8, 0, 0, 0, 0, 0xc3];
        assert_eq!(jam_crc(&data), 0xf2f0_fe1d);
        assert_eq!(jam_crc(&[]), 0);
    }

    #[test]
    fn alignment_is_log2_plus_one_in_bits_20_to_23() {
        assert_eq!(align_bits(1), 0x0010_0000);
        assert_eq!(align_bits(4), 0x0030_0000);
        assert_eq!(align_bits(16), 0x0050_0000);
        // Not a power of two, and past what COFF can say.
        assert_eq!(align_bits(3), 0x0030_0000);
        assert_eq!(align_bits(1 << 20), align_bits(8192));
    }

    #[test]
    fn section_flag_letters_follow_llvm_mc() {
        // Checked against llvm-mc 22 for x86_64-windows-msvc.
        let f = |s: &str| parse_flags(".s", s).unwrap();
        assert_eq!(f("d"), 0xc000_0040);
        assert_eq!(f("r"), 0x4000_0040);
        assert_eq!(f("x"), 0x6000_0020);
        assert_eq!(f("xw"), 0xe000_0020);
        assert_eq!(f("b"), 0xc000_0080);
        assert_eq!(f("n"), 0xc000_0800);
        assert_eq!(f("y"), 0);
        assert_eq!(f("yd"), 0x8000_0040);
        assert_eq!(f(""), 0xc000_0040);
        assert_eq!(f("xr"), 0x6000_0020);
        assert_eq!(f("dr"), 0x4000_0040);
        assert_eq!(f("nd"), 0xc000_0840);
        assert_eq!(f("xsw"), 0xf000_0060);
        assert_eq!(parse_flags(".debug_foo", "d").unwrap(), 0xc200_0040);
        assert_eq!(parse_flags(".s", "q"), Err('q'));
        // `.drectve` is one of the sections whose flags are fixed by name.
        assert_eq!(preset_characteristics(".drectve"), Some(0x0000_0a00));
    }
}
