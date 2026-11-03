//! ELF relocatable object output, 32- and 64-bit.
//!
//! Only `ET_REL` is produced: rsasm is an assembler, so linking is somebody
//! else's job.
//!
//! The class follows the target's pointer width, and it changes more than the
//! field widths: `Elf32_Sym` orders its members differently from `Elf64_Sym`,
//! and `r_info` packs the symbol index into 24 bits rather than 32. The
//! relocation *format* also varies by architecture rather than by class —
//! see [`uses_rela`].

use super::OutputError;
use crate::assembler::Assembler;
use crate::section::{SectionId, SectionKind};
use crate::symbol::{Binding, SymType, SymbolId, SymbolValue, Visibility};
use std::collections::HashMap;

const EI_NIDENT: usize = 16;
const ELFCLASS32: u8 = 1;
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const ELFDATA2MSB: u8 = 2;
const EV_CURRENT: u8 = 1;
const ET_REL: u16 = 1;

const SHT_NULL: u32 = 0;
const SHT_PROGBITS: u32 = 1;
const SHT_SYMTAB: u32 = 2;
const SHT_STRTAB: u32 = 3;
const SHT_RELA: u32 = 4;
const SHT_REL: u32 = 9;
const SHT_NOBITS: u32 = 8;
const SHT_NOTE: u32 = 7;
const SHT_MIPS_DWARF: u32 = 0x7000_001e;
const EM_MIPS: u16 = 8;

const SHF_WRITE: u64 = 0x1;
const SHF_ALLOC: u64 = 0x2;
const SHF_EXECINSTR: u64 = 0x4;
const SHF_MERGE: u64 = 0x10;
const SHF_STRINGS: u64 = 0x20;
const SHF_TLS: u64 = 0x400;

const SHN_UNDEF: u16 = 0;
const SHN_ABS: u16 = 0xfff1;
const SHN_COMMON: u16 = 0xfff2;

const STB_LOCAL: u8 = 0;
const STB_GLOBAL: u8 = 1;
const STB_WEAK: u8 = 2;

const STT_NOTYPE: u8 = 0;
const STT_OBJECT: u8 = 1;
const STT_FUNC: u8 = 2;
const STT_SECTION: u8 = 3;
const STT_FILE: u8 = 4;
const STT_TLS: u8 = 6;

/// Which ELF class is being written. Everything whose width or layout differs
/// between the two goes through here rather than through a scattering of
/// `if is_64` tests.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Class {
    Elf32,
    Elf64,
}

impl Class {
    fn ehdr_size(self) -> u64 {
        match self {
            Class::Elf32 => 52,
            Class::Elf64 => 64,
        }
    }

    fn shdr_size(self) -> u64 {
        match self {
            Class::Elf32 => 40,
            Class::Elf64 => 64,
        }
    }

    fn sym_size(self) -> u64 {
        match self {
            Class::Elf32 => 16,
            Class::Elf64 => 24,
        }
    }

    fn rel_size(self, rela: bool) -> u64 {
        match (self, rela) {
            (Class::Elf32, false) => 8,
            (Class::Elf32, true) => 12,
            (Class::Elf64, false) => 16,
            (Class::Elf64, true) => 24,
        }
    }

    /// Alignment the tables want. 32-bit ELF only needs word alignment.
    fn table_align(self) -> u64 {
        match self {
            Class::Elf32 => 4,
            Class::Elf64 => 8,
        }
    }
}

/// Whether this machine's psABI carries relocation addends in the relocation
/// entry (`RELA`) or in the field being relocated (`REL`).
///
/// Mostly a property of the machine, not of the ELF class: i386 and ARM use
/// `REL` while PowerPC, SPARC, RISC-V, AArch64 and x86-64 use `RELA`. MIPS is
/// the exception that needs the class: o32 is `REL` and n64 `RELA`.
/// Not API.
#[doc(hidden)]
pub fn uses_rela(machine: u16, elf64: bool) -> bool {
    match machine {
        3 | 40 => false, // EM_386, EM_ARM
        8 => elf64,      // EM_MIPS
        _ => true,
    }
}

/// Whether an object for `arch` is ELF64, which is decided by the target's
/// initial mode rather than wherever the source leaves it.
/// Not API.
#[doc(hidden)]
pub fn is_elf64(arch: &dyn crate::arch::Architecture) -> bool {
    arch.pointer_bytes(&arch.initial_state()) == 8
}

/// A growable string table with deduplication of whole strings.
#[derive(Default)]
struct StrTab {
    bytes: Vec<u8>,
    seen: HashMap<String, u32>,
}

impl StrTab {
    fn new() -> StrTab {
        StrTab {
            bytes: vec![0],
            seen: HashMap::new(),
        }
    }

    fn add(&mut self, s: &str) -> u32 {
        if s.is_empty() {
            return 0;
        }
        if let Some(&o) = self.seen.get(s) {
            return o;
        }
        let off = self.bytes.len() as u32;
        self.bytes.extend_from_slice(s.as_bytes());
        self.bytes.push(0);
        self.seen.insert(s.to_string(), off);
        off
    }
}

/// Little-endian byte sink. ELF is written in the target's byte order, which
/// for every architecture here is the same order as the data it describes.
struct Buf {
    out: Vec<u8>,
    big_endian: bool,
    class: Class,
}

impl Buf {
    fn u8(&mut self, v: u8) {
        self.out.push(v);
    }
    fn u16(&mut self, v: u16) {
        if self.big_endian {
            self.out.extend_from_slice(&v.to_be_bytes());
        } else {
            self.out.extend_from_slice(&v.to_le_bytes());
        }
    }
    fn u32(&mut self, v: u32) {
        if self.big_endian {
            self.out.extend_from_slice(&v.to_be_bytes());
        } else {
            self.out.extend_from_slice(&v.to_le_bytes());
        }
    }
    fn u64(&mut self, v: u64) {
        if self.big_endian {
            self.out.extend_from_slice(&v.to_be_bytes());
        } else {
            self.out.extend_from_slice(&v.to_le_bytes());
        }
    }
    /// An address, offset or size: 32 bits wide in ELF32, 64 in ELF64.
    fn addr(&mut self, v: u64) {
        match self.class {
            Class::Elf32 => self.u32(v as u32),
            Class::Elf64 => self.u64(v),
        }
    }

    fn saddr(&mut self, v: i64) {
        match self.class {
            Class::Elf32 => self.u32(v as u32),
            Class::Elf64 => self.u64(v as u64),
        }
    }
    fn pad_to(&mut self, align: u64) {
        while !(self.out.len() as u64).is_multiple_of(align) {
            self.out.push(0);
        }
    }
    fn len(&self) -> u64 {
        self.out.len() as u64
    }
}

/// One section header, filled in as the layout is decided.
#[derive(Default, Clone)]
struct Shdr {
    name: u32,
    ty: u32,
    flags: u64,
    addr: u64,
    offset: u64,
    size: u64,
    link: u32,
    info: u32,
    addralign: u64,
    entsize: u64,
}

/// An assembler symbol that made it into the ELF symbol table.
struct OutSym {
    /// `None` for a mapping symbol, which no relocation names.
    id: Option<SymbolId>,
    name: u32,
    info: u8,
    other: u8,
    shndx: u16,
    value: u64,
    size: u64,
}

pub fn build(asm: &Assembler) -> Result<Vec<u8>, OutputError> {
    // The class belongs to the object, not to whatever mode the file ended
    // in: an x86-64 file whose last stretch is `.code32` is still an ELF64
    // object, and writing ELF32 with EM_X86_64 would instead claim the x32
    // ABI. A target's initial state is its object class. Nor does `.arch`
    // change it: the object is for the target the assembler started with.
    let class = match asm.target().pointer_bytes(&asm.target().initial_state()) {
        8 => Class::Elf64,
        4 => Class::Elf32,
        n => {
            return Err(OutputError::Unsupported(format!(
                "ELF has no class for a {}-bit target; use `-f bin` instead",
                n as u32 * 8
            )));
        }
    };
    let rela = uses_rela(asm.target().elf_machine(), class == Class::Elf64);
    let rel_size = class.rel_size(rela);

    let mut shstrtab = StrTab::new();
    let mut strtab = StrTab::new();

    // ---- decide which ELF sections exist ---------------------------------
    // Index 0 is the null header; then one per assembler section that has
    // content, then a .rela for each of those with relocations, then the
    // symbol and string tables.
    let mut shdrs: Vec<Shdr> = vec![Shdr {
        ty: SHT_NULL,
        ..Shdr::default()
    }];
    let mut sec_index: HashMap<SectionId, u16> = HashMap::new();
    let mut emitted: Vec<SectionId> = Vec::new();

    // NASM writes every section a `section` directive named, empty or not,
    // and the initial `.text` its standard macros select only when something
    // went into it: a data-only NASM program has no `.text` in its object.
    let nasm_named =
        |id| asm.options.dialect == crate::lexer::Dialect::Nasm && asm.nasm.opened.contains(&id);
    // The sections the target writes of its own accord keep the type and
    // flags their psABI gives them, which the section model has no room for:
    // see `Architecture::elf_attributes`.
    let attributes: HashMap<&'static str, (u32, u64)> = asm
        .attribute_sections()
        .iter()
        .map(|a| (a.name, (a.sh_type, a.sh_flags)))
        .collect();
    // A section with nothing in it but a label is written too, as both
    // references write it: the label needs a section to be in, and a
    // relocation against it one to name.
    let labelled: std::collections::HashSet<SectionId> = asm
        .symbols
        .iter()
        .filter_map(|(_, sym)| match sym.value {
            SymbolValue::Label { section, .. } => Some(section),
            _ => None,
        })
        .collect();
    for s in &asm.sections {
        let keep_empty = nasm_named(s.id) || labelled.contains(&s.id);
        if s.size == 0 && s.frags.is_empty() && !keep_empty {
            continue;
        }
        let idx = shdrs.len() as u16;
        sec_index.insert(s.id, idx);
        emitted.push(s.id);
        let name = asm.interner.get(s.name).to_string();
        shdrs.push(Shdr {
            name: shstrtab.add(&name),
            ty: match s.kind {
                SectionKind::Nobits => SHT_NOBITS,
                SectionKind::Note => SHT_NOTE,
                // MIPS gives DWARF sections a type of their own, in both GNU
                // as and llvm-mc.
                SectionKind::Progbits
                    if asm.target().elf_machine() == EM_MIPS && name.starts_with(".debug_") =>
                {
                    SHT_MIPS_DWARF
                }
                SectionKind::Progbits if attributes.contains_key(name.as_str()) => {
                    attributes[name.as_str()].0
                }
                SectionKind::Progbits => SHT_PROGBITS,
            },
            flags: match attributes.get(name.as_str()) {
                Some(&(_, sh_flags)) => sh_flags,
                None => elf_flags(&s.flags),
            },
            addr: 0,
            offset: 0,
            size: s.size,
            link: 0,
            info: 0,
            addralign: s.align.max(1),
            entsize: s.entsize,
        });
    }

    // ---- build the symbol table ------------------------------------------
    let (syms, first_global) = collect_symbols(asm, &sec_index, &mut strtab);
    let sym_index: HashMap<SymbolId, u32> = syms
        .iter()
        .enumerate()
        // +1 for the reserved null entry.
        .filter_map(|(i, s)| Some((s.id?, i as u32 + 1)))
        .collect();

    // Relocation sections, one per section that needs them.
    let mut relocs_by_section: HashMap<SectionId, Vec<&crate::assembler::Relocation>> =
        HashMap::new();
    for r in &asm.relocs {
        relocs_by_section.entry(r.section).or_default().push(r);
    }
    let mut rela_for: Vec<(SectionId, u16)> = Vec::new();
    for &sid in &emitted {
        let Some(list) = relocs_by_section.get(&sid) else {
            continue;
        };
        if list.is_empty() {
            continue;
        }
        let target = sec_index[&sid];
        let name = format!(
            "{}{}",
            if rela { ".rela" } else { ".rel" },
            asm.interner.get(asm.section(sid).name)
        );
        let idx = shdrs.len() as u16;
        rela_for.push((sid, idx));
        shdrs.push(Shdr {
            name: shstrtab.add(&name),
            ty: if rela { SHT_RELA } else { SHT_REL },
            flags: 0,
            addr: 0,
            offset: 0,
            size: list.len() as u64 * rel_size,
            // Patched below once the symtab index is known.
            link: 0,
            info: target as u32,
            addralign: class.table_align(),
            entsize: rel_size,
        });
    }

    let symtab_idx = shdrs.len() as u16;
    shdrs.push(Shdr {
        name: shstrtab.add(".symtab"),
        ty: SHT_SYMTAB,
        flags: 0,
        addr: 0,
        offset: 0,
        size: (syms.len() as u64 + 1) * class.sym_size(),
        link: 0, // patched: .strtab
        info: first_global + 1,
        addralign: class.table_align(),
        entsize: class.sym_size(),
    });
    let strtab_idx = shdrs.len() as u16;
    shdrs.push(Shdr {
        name: shstrtab.add(".strtab"),
        ty: SHT_STRTAB,
        flags: 0,
        addr: 0,
        offset: 0,
        size: 0, // patched
        link: 0,
        info: 0,
        addralign: 1,
        entsize: 0,
    });
    let shstrtab_idx = shdrs.len() as u16;
    shdrs.push(Shdr {
        name: shstrtab.add(".shstrtab"),
        ty: SHT_STRTAB,
        flags: 0,
        addr: 0,
        offset: 0,
        size: 0, // patched
        link: 0,
        info: 0,
        addralign: 1,
        entsize: 0,
    });

    shdrs[symtab_idx as usize].link = strtab_idx as u32;
    for (_, idx) in &rela_for {
        shdrs[*idx as usize].link = symtab_idx as u32;
    }

    // ---- lay the file out -------------------------------------------------
    let big_endian = asm.target().endian() == crate::arch::Endian::Big;
    let mut buf = Buf {
        out: Vec::new(),
        big_endian,
        class,
    };
    buf.out.resize(class.ehdr_size() as usize, 0);

    for &sid in &emitted {
        let i = sec_index[&sid] as usize;
        let s = asm.section(sid);
        if s.kind == SectionKind::Nobits {
            // No file space, but the offset still has to look plausible.
            shdrs[i].offset = buf.len();
            continue;
        }
        buf.pad_to(shdrs[i].addralign.max(1));
        shdrs[i].offset = buf.len();
        let bytes = asm.section_bytes(sid);
        debug_assert_eq!(
            bytes.len() as u64,
            s.size,
            "section bytes disagree with layout"
        );
        buf.out.extend_from_slice(&bytes);
    }

    let mips64el = asm.target().elf_machine() == 8 && class == Class::Elf64 && !big_endian;
    for (sid, idx) in &rela_for {
        buf.pad_to(class.table_align());
        shdrs[*idx as usize].offset = buf.len();
        for r in &relocs_by_section[sid] {
            let sym = r
                .symbol
                .and_then(|s| sym_index.get(&s).copied())
                .unwrap_or(0);
            buf.addr(r.offset);
            match class {
                // ELF32 packs the symbol index into the top 24 bits and the
                // type into the low 8, not the 32/32 split ELF64 uses.
                Class::Elf32 => buf.u32((sym << 8) | (r.kind & 0xff)),
                // MIPS n64 splits the 64-bit info into a 32-bit symbol and
                // four one-byte fields — the special symbol, the third type,
                // the second type and the primary type — which is why the
                // backend packs up to three types into `kind`, one per byte.
                // Big-endian that is bit-for-bit the standard packing;
                // little-endian it is not.
                Class::Elf64 if mips64el => {
                    buf.out.extend_from_slice(&sym.to_le_bytes());
                    let (t2, t3) = ((r.kind >> 8) as u8, (r.kind >> 16) as u8);
                    buf.out.extend_from_slice(&[0, t3, t2, r.kind as u8]);
                }
                Class::Elf64 => buf.u64(((sym as u64) << 32) | r.kind as u64),
            }
            // Under REL the addend lives in the field instead; the layout pass
            // has already written it there.
            if rela {
                buf.saddr(r.addend);
            }
        }
    }

    buf.pad_to(class.table_align());
    shdrs[symtab_idx as usize].offset = buf.len();
    // The reserved null symbol.
    for _ in 0..class.sym_size() {
        buf.u8(0);
    }
    for s in &syms {
        // Elf32_Sym and Elf64_Sym do not merely differ in width: the value and
        // size move ahead of info/other/shndx in the 32-bit form.
        match class {
            Class::Elf32 => {
                buf.u32(s.name);
                buf.u32(s.value as u32);
                buf.u32(s.size as u32);
                buf.u8(s.info);
                buf.u8(s.other);
                buf.u16(s.shndx);
            }
            Class::Elf64 => {
                buf.u32(s.name);
                buf.u8(s.info);
                buf.u8(s.other);
                buf.u16(s.shndx);
                buf.u64(s.value);
                buf.u64(s.size);
            }
        }
    }

    shdrs[strtab_idx as usize].offset = buf.len();
    shdrs[strtab_idx as usize].size = strtab.bytes.len() as u64;
    buf.out.extend_from_slice(&strtab.bytes);

    shdrs[shstrtab_idx as usize].offset = buf.len();
    shdrs[shstrtab_idx as usize].size = shstrtab.bytes.len() as u64;
    buf.out.extend_from_slice(&shstrtab.bytes);

    buf.pad_to(class.table_align());
    let shoff = buf.len();
    for sh in &shdrs {
        buf.u32(sh.name);
        buf.u32(sh.ty);
        buf.addr(sh.flags);
        buf.addr(sh.addr);
        buf.addr(sh.offset);
        buf.addr(sh.size);
        buf.u32(sh.link);
        buf.u32(sh.info);
        buf.addr(sh.addralign);
        buf.addr(sh.entsize);
    }

    // ---- header -----------------------------------------------------------
    let mut hdr = Buf {
        out: Vec::new(),
        big_endian,
        class,
    };
    let mut ident = [0u8; EI_NIDENT];
    ident[0..4].copy_from_slice(b"\x7fELF");
    ident[4] = match class {
        Class::Elf32 => ELFCLASS32,
        Class::Elf64 => ELFCLASS64,
    };
    ident[5] = if big_endian { ELFDATA2MSB } else { ELFDATA2LSB };
    ident[6] = EV_CURRENT;
    ident[7] = asm.target().elf_osabi();
    hdr.out.extend_from_slice(&ident);
    hdr.u16(ET_REL);
    hdr.u16(asm.target().elf_machine());
    hdr.u32(EV_CURRENT as u32);
    hdr.addr(0); // e_entry
    hdr.addr(0); // e_phoff
    hdr.addr(shoff);
    let (arch, state) = asm.target_state();
    hdr.u32(arch.elf_flags(state));
    hdr.u16(class.ehdr_size() as u16);
    hdr.u16(0); // e_phentsize
    hdr.u16(0); // e_phnum
    hdr.u16(class.shdr_size() as u16);
    hdr.u16(shdrs.len() as u16);
    hdr.u16(shstrtab_idx);
    buf.out[..class.ehdr_size() as usize].copy_from_slice(&hdr.out);

    Ok(buf.out)
}

fn elf_flags(f: &crate::section::SectionFlags) -> u64 {
    let mut v = 0;
    if f.alloc {
        v |= SHF_ALLOC;
    }
    if f.write {
        v |= SHF_WRITE;
    }
    if f.exec {
        v |= SHF_EXECINSTR;
    }
    if f.merge {
        v |= SHF_MERGE;
    }
    if f.strings {
        v |= SHF_STRINGS;
    }
    if f.tls {
        v |= SHF_TLS;
    }
    v
}

/// Collects the symbols worth writing, locals first as ELF requires.
///
/// Returns the symbols and the index of the last local, which becomes the
/// symbol table's `sh_info`.
fn collect_symbols(
    asm: &Assembler,
    sec_index: &HashMap<SectionId, u16>,
    strtab: &mut StrTab,
) -> (Vec<OutSym>, u32) {
    let mut locals = Vec::new();
    let mut globals = Vec::new();
    // Relocations normally name a local label's section instead, but some
    // have to name the label itself (see `RelocSymbol`), and those labels
    // have to be written even when the assembler made them up.
    let named: std::collections::HashSet<SymbolId> =
        asm.relocs.iter().filter_map(|r| r.symbol).collect();
    let mut temps = 0;

    for (id, sym) in asm.symbols.iter() {
        let raw = asm.interner.get(sym.name);
        let is_synthetic = raw.contains('\u{0}') && sym.ty != SymType::Section;
        // Numeric local labels, the anonymous labels standing in for `.` and
        // the ones behind a RISC-V `la` are assembler bookkeeping; they reach
        // the object file only when a relocation names them, under a name in
        // llvm-mc's style.
        let temp_name;
        let raw = if sym.local_number.is_some() || is_synthetic {
            if !named.contains(&id) {
                continue;
            }
            temp_name = format!(".Ltmp{temps}");
            temps += 1;
            temp_name.as_str()
        } else {
            raw
        };
        // A symbol declared global and never defined is written even if
        // nothing refers to it, as both references write it: it makes the
        // linker pull in whatever defines it, which is what the
        // `.globl __do_copy_data` avr-gcc and Clang emit is for. GNU as
        // writes one that `.type`, `.size`, `.local` or a visibility named
        // too, as a global undefined symbol; `.weak` alone it leaves out, as
        // it does any weak symbol nothing refers to, and a `.L` name, which
        // is the assembly's own. llvm-mc splits from it three ways: it keeps
        // the weak one, leaves out the one only `.size` named, and makes the
        // one `.local` named a local undefined symbol. NASM has no spelling
        // for any of this — an undefined symbol there was declared `extern`
        // — and leaves one nothing refers to out of the object, so the NASM
        // dialect does the same.
        let keep_undefined = asm.options.dialect != crate::lexer::Dialect::Nasm
            && (sym.binding == Binding::Global
                || (sym.declared && sym.binding != Binding::Weak && !raw.starts_with(".L")));
        if !sym.is_defined() && !sym.used && !keep_undefined {
            continue;
        }
        // A `.L` label is local to the assembly, by the ELF convention GNU as
        // and llvm-mc both follow, unless a relocation has to name it.
        if raw.starts_with(".L")
            && sym.binding == Binding::Local
            && sym.is_defined()
            && !named.contains(&id)
        {
            continue;
        }

        let (shndx, value) = match &sym.value {
            SymbolValue::Label { section, .. } => {
                let Some(&idx) = sec_index.get(section) else {
                    continue;
                };
                (idx, asm.symbol_addr(id).unwrap_or(0) as u64)
            }
            SymbolValue::Common { align, .. } => (SHN_COMMON, *align),
            // An `.equ` naming a label belongs to that label's section, the
            // way GNU as places it; anything else is absolute.
            SymbolValue::Expr(_) => match asm.symbol_target_section(id) {
                Some((section, off)) => match sec_index.get(&section) {
                    Some(&idx) => (idx, off),
                    None => continue,
                },
                None => (SHN_ABS, asm.symbol_number(id).unwrap_or(0) as u64),
            },
            SymbolValue::Undefined => (SHN_UNDEF, 0),
        };

        let bind = match sym.binding {
            // An undefined symbol is the linker's to find, so it is global
            // whether or not the source said `.globl`, as in both references;
            // a linker refuses a local one.
            Binding::Local if !sym.is_defined() => STB_GLOBAL,
            Binding::Local => STB_LOCAL,
            Binding::Global => STB_GLOBAL,
            Binding::Weak => STB_WEAK,
        };
        // A Thumb function is marked by its type and low bit; the target
        // decides that from what it recorded on the label.
        let (sym_ty, value) =
            asm.target()
                .elf_symbol(sym.target_flags, sym.ty, sym.is_defined(), value);
        // A label in a thread-local section is thread-local whatever `.type`
        // said, as in GNU as; the section symbol standing for the section as
        // a whole is not, since it names the section rather than a variable
        // the linker copies into each thread's block.
        let sym_ty = if sym_ty != SymType::Section && asm.is_thread_local(id) {
            SymType::Tls
        } else {
            sym_ty
        };
        let ty = match sym_ty {
            SymType::NoType => STT_NOTYPE,
            SymType::Object => STT_OBJECT,
            SymType::Func => STT_FUNC,
            SymType::Section => STT_SECTION,
            SymType::File => STT_FILE,
            SymType::Tls => STT_TLS,
        };
        let other = match sym.visibility {
            Visibility::Default => 0,
            Visibility::Internal => 1,
            Visibility::Hidden => 2,
            Visibility::Protected => 3,
        };
        let size = asm.symbol_size(id).unwrap_or(0) as u64;

        // A section symbol is named by its section, not by a string of its own.
        let name = if sym.ty == SymType::Section {
            0
        } else {
            strtab.add(raw)
        };

        let out = OutSym {
            id: Some(id),
            name,
            info: (bind << 4) | ty,
            other,
            shndx,
            value,
            size,
        };
        if bind == STB_LOCAL {
            locals.push(out);
        } else {
            globals.push(out);
        }
    }

    // Mapping symbols are untyped locals, one per change between code and
    // data; see `crate::mapping`. One in a thread-local section is
    // thread-local as well, since GNU as decides that from the section
    // whatever the symbol stands for.
    for m in &asm.mapping_symbols {
        let Some(&shndx) = sec_index.get(&m.section) else {
            continue;
        };
        let ty = if asm.section(m.section).flags.tls {
            STT_TLS
        } else {
            STT_NOTYPE
        };
        locals.push(OutSym {
            id: None,
            name: strtab.add(m.name),
            info: (STB_LOCAL << 4) | ty,
            other: 0,
            shndx,
            value: m.offset,
            size: 0,
        });
    }

    // Section symbols sort first among locals, which is what linkers expect
    // and what makes `.rela` entries against them readable.
    locals.sort_by_key(|s| (s.info & 0xf) != STT_SECTION);

    let first_global = locals.len() as u32;
    locals.extend(globals);
    (locals, first_global)
}
