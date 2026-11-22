//! Golden objects written byte by byte (a tiny Mach-O arm64 and ELF64 AArch64 writer below, laid
//! out from `<mach-o/loader.h>`/`<mach-o/nlist.h>`/`<mach-o/reloc.h>` and the gABI as
//! blaze-native's own test support writes them), one per accepted relocation and section kind and
//! one per refusal; a round trip of a lowered record through blaze-code's placement; the keys; the
//! symbol index; and two simulations with their planted-bug controls.

use core::fmt;
use std::{format, string::String as StdString, vec, vec::Vec as SVec};

use blaze_code::a64::{adrp_ldr_target, bl_target, read32};
use blaze_code::datum::{DatumView, Place as DatumPlace};
use blaze_code::image::ImageError;
use blaze_code::place::{At, Words, place};
use blaze_code::record::{CodeView, SiteKind, Space, Symbol, Target};
use blaze_native::Platform;
use blaze_own::planted::NoPlant;
use blaze_own::{AVec, Alloc, Vec};
use blaze_sim::{Broken, Config, Planted, Tape, check, controls};

use crate::index::{Def, DefKind, IndexError, IndexPlant, IndexView, UnitDefs, build};
use crate::key::{HeaderRead, KeyError, Lang, Root, UnitKey, UnitKeyParts, atom_key, local_symbol, unit_key};
use crate::lower::{LowerError, Lowered, lower};
use crate::split::{AtomKind, AtomName, NO_PRIORITY, RefKind, RefTo, Split, SplitError, SplitOpts, SplitPlant, Strength, split};

/// The test's allocator (D102): the process heap's handle, never `Global`.
fn heap() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

// ---- instructions ----

const RET: u32 = 0xD65F_03C0;
const BL0: u32 = 0x9400_0000;
const NOP: u32 = 0xD503_201F;
/// `adrp xN, 0`.
fn adrp_x(n: u32) -> u32 {
    0x9000_0000 | n
}
/// `ldr xT, [xN]`.
fn ldr_x(t: u32, n: u32) -> u32 {
    0xF940_0000 | (n << 5) | t
}
/// `add xD, xN, #0`.
fn add_x(d: u32, n: u32) -> u32 {
    0x9100_0000 | (n << 5) | d
}
/// `ldr wT, [xN]`.
fn ldr_w(t: u32, n: u32) -> u32 {
    0xB940_0000 | (n << 5) | t
}

fn code(insts: &[u32]) -> SVec<u8> {
    insts.iter().flat_map(|i| i.to_le_bytes()).collect()
}

// ---- a Mach-O MH_OBJECT writer ----

const S_ATTR_PURE: u32 = 0x8000_0400;
const S_ZEROFILL: u32 = 0x1;
const S_CSTRING: u32 = 0x2;
const S_MOD_INIT: u32 = 0x9;
const SUBSECTIONS: u32 = 0x2000;

const R_UNSIGNED: u32 = 0;
const R_BRANCH26: u32 = 2;
const R_PAGE21: u32 = 3;
const R_PAGEOFF12: u32 = 4;
const R_GOT_PAGE21: u32 = 5;
const R_GOT_PAGEOFF12: u32 = 6;

struct MachSection {
    segment: &'static [u8],
    name: &'static [u8],
    flags: u32,
    align_log2: u32,
    bytes: SVec<u8>,
    /// A zero-fill section's size (its bytes are empty).
    zero: u64,
    /// `(r_address, packed word)`, in file order.
    relocs: SVec<(u32, u32)>,
}

impl MachSection {
    fn size(&self) -> u64 {
        if self.flags & 0xff == S_ZEROFILL { self.zero } else { self.bytes.len() as u64 }
    }
}

/// An `nlist_64`: `value` section-relative for an `N_SECT` symbol.
struct MachSym {
    name: &'static [u8],
    n_type: u8,
    n_sect: u8,
    n_desc: u16,
    value: u64,
}

const N_EXT_SECT: u8 = 0x0f;
const N_SECT: u8 = 0x0e;
const N_UNDF_EXT: u8 = 0x01;
const N_WEAK_DEF: u16 = 0x0080;

fn text(bytes: SVec<u8>, relocs: SVec<(u32, u32)>) -> MachSection {
    MachSection { segment: b"__TEXT", name: b"__text", flags: S_ATTR_PURE, align_log2: 2, bytes, zero: 0, relocs }
}

/// `relocation_info`'s second word.
fn rword(symbolnum: u32, pcrel: bool, length: u32, external: bool, kind: u32) -> u32 {
    (symbolnum & 0x00ff_ffff) | (u32::from(pcrel) << 24) | (length << 25) | (u32::from(external) << 27) | (kind << 28)
}

fn branch(at: u32, sym: u32) -> (u32, u32) {
    (at, rword(sym, true, 2, true, R_BRANCH26))
}

fn put32(v: &mut SVec<u8>, x: u32) {
    v.extend_from_slice(&x.to_le_bytes());
}

fn put64(v: &mut SVec<u8>, x: u64) {
    v.extend_from_slice(&x.to_le_bytes());
}

fn up(x: usize, to: usize) -> usize {
    x.div_ceil(to) * to
}

fn name16(v: &mut SVec<u8>, name: &[u8]) {
    let mut field = [0u8; 16];
    field[..name.len()].copy_from_slice(name);
    v.extend_from_slice(&field);
}

fn macho(flags: u32, sections: &[MachSection], symbols: &[MachSym]) -> SVec<u8> {
    let n = sections.len();
    let sizeofcmds = 72 + 80 * n + 24;
    let mut addresses = SVec::new();
    let mut addr = 0u64;
    for s in sections {
        addr = up(addr as usize, 1 << s.align_log2) as u64;
        addresses.push(addr);
        addr += s.size();
    }
    let mut at = up(32 + sizeofcmds, 8);
    let data_start = at;
    let mut data_at = SVec::new();
    for s in sections {
        data_at.push(at);
        at = up(at + s.bytes.len(), 8);
    }
    let data_end = at;
    let mut reloc_at = SVec::new();
    for s in sections {
        reloc_at.push(at);
        at += 8 * s.relocs.len();
    }
    let symoff = up(at, 8);
    let stroff = symoff + 16 * symbols.len();
    let mut strtab = vec![0u8];
    let mut strx = SVec::new();
    for sym in symbols {
        strx.push(strtab.len() as u32);
        strtab.extend_from_slice(sym.name);
        strtab.push(0);
    }
    let mut v = SVec::new();
    for word in [0xfeed_facf, 0x0100_000c, 0, 1, 2, sizeofcmds as u32, flags, 0] {
        put32(&mut v, word);
    }
    put32(&mut v, 0x19);
    put32(&mut v, (72 + 80 * n) as u32);
    name16(&mut v, b"");
    put64(&mut v, 0);
    put64(&mut v, addr);
    put64(&mut v, data_start as u64);
    put64(&mut v, (data_end - data_start) as u64);
    for word in [7, 7, n as u32, 0] {
        put32(&mut v, word);
    }
    for (i, s) in sections.iter().enumerate() {
        name16(&mut v, s.name);
        name16(&mut v, s.segment);
        put64(&mut v, addresses[i]);
        put64(&mut v, s.size());
        put32(&mut v, if s.bytes.is_empty() { 0 } else { data_at[i] as u32 });
        put32(&mut v, s.align_log2);
        put32(&mut v, if s.relocs.is_empty() { 0 } else { reloc_at[i] as u32 });
        put32(&mut v, s.relocs.len() as u32);
        for word in [s.flags, 0, 0, 0] {
            put32(&mut v, word);
        }
    }
    for word in [0x2, 24, symoff as u32, symbols.len() as u32, stroff as u32, strtab.len() as u32] {
        put32(&mut v, word);
    }
    for (i, s) in sections.iter().enumerate() {
        v.resize(data_at[i], 0);
        v.extend_from_slice(&s.bytes);
    }
    v.resize(data_end, 0);
    for s in sections {
        for &(address, word) in &s.relocs {
            put32(&mut v, address);
            put32(&mut v, word);
        }
    }
    v.resize(symoff, 0);
    for (i, sym) in symbols.iter().enumerate() {
        let n_value = if sym.n_type & 0x0e == 0x0e { addresses[usize::from(sym.n_sect) - 1] + sym.value } else { sym.value };
        put32(&mut v, strx[i]);
        v.push(sym.n_type);
        v.push(sym.n_sect);
        v.extend_from_slice(&sym.n_desc.to_le_bytes());
        put64(&mut v, n_value);
    }
    v.extend_from_slice(&strtab);
    v
}

fn global(name: &'static [u8], n_sect: u8, value: u64) -> MachSym {
    MachSym { name, n_type: N_EXT_SECT, n_sect, n_desc: 0, value }
}

fn local(name: &'static [u8], n_sect: u8, value: u64) -> MachSym {
    MachSym { name, n_type: N_SECT, n_sect, n_desc: 0, value }
}

fn undefined(name: &'static [u8]) -> MachSym {
    MachSym { name, n_type: N_UNDF_EXT, n_sect: 0, n_desc: 0, value: 0 }
}

// ---- an ELF64 AArch64 ET_REL writer ----

struct ElfSection {
    name: &'static [u8],
    kind: u32,
    flags: u64,
    align: u64,
    bytes: SVec<u8>,
    link: u32,
    info: u32,
    entsize: u64,
}

struct ElfSym {
    name: &'static [u8],
    info: u8,
    shndx: u16,
    value: u64,
}

/// `.symtab`'s and `.strtab`'s contents (the null symbol first).
fn elf_symtab(symbols: &[ElfSym]) -> (SVec<u8>, SVec<u8>) {
    let mut table = vec![0u8; 24];
    let mut strings = vec![0u8];
    for s in symbols {
        let st_name = if s.name.is_empty() { 0 } else { strings.len() as u32 };
        if !s.name.is_empty() {
            strings.extend_from_slice(s.name);
            strings.push(0);
        }
        put32(&mut table, st_name);
        table.push(s.info);
        table.push(0);
        table.extend_from_slice(&s.shndx.to_le_bytes());
        put64(&mut table, s.value);
        put64(&mut table, 0);
    }
    (table, strings)
}

/// `Elf64_Rela`s: `(r_offset, symbol, type, r_addend)`.
fn elf_rela(entries: &[(u64, u32, u32, i64)]) -> SVec<u8> {
    let mut v = SVec::new();
    for &(offset, symbol, kind, addend) in entries {
        put64(&mut v, offset);
        put64(&mut v, (u64::from(symbol) << 32) | u64::from(kind));
        put64(&mut v, addend as u64);
    }
    v
}

/// Section 0 is the null header, `sections` are 1.., `.shstrtab` is last.
fn elf(sections: &[ElfSection]) -> SVec<u8> {
    let mut shstrtab = vec![0u8];
    let mut names = SVec::new();
    for s in sections {
        names.push(shstrtab.len() as u32);
        shstrtab.extend_from_slice(s.name);
        shstrtab.push(0);
    }
    let shstrtab_name = shstrtab.len() as u32;
    shstrtab.extend_from_slice(b".shstrtab\0");
    let mut at = 64usize;
    let mut data_at = SVec::new();
    for s in sections {
        at = up(at, 8);
        data_at.push(at);
        if s.kind != 8 {
            at += s.bytes.len();
        }
    }
    let shstrtab_at = up(at, 8);
    let shoff = up(shstrtab_at + shstrtab.len(), 8);
    let shnum = sections.len() + 2;
    let mut v = vec![0x7f, b'E', b'L', b'F', 2, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&183u16.to_le_bytes());
    put32(&mut v, 1);
    put64(&mut v, 0);
    put64(&mut v, 0);
    put64(&mut v, shoff as u64);
    put32(&mut v, 0);
    for half in [64u16, 0, 0, 64, shnum as u16, (shnum - 1) as u16] {
        v.extend_from_slice(&half.to_le_bytes());
    }
    for (i, s) in sections.iter().enumerate() {
        if s.kind != 8 {
            v.resize(data_at[i], 0);
            v.extend_from_slice(&s.bytes);
        }
    }
    v.resize(shstrtab_at, 0);
    v.extend_from_slice(&shstrtab);
    v.resize(shoff, 0);
    v.extend_from_slice(&[0u8; 64]);
    let header = |v: &mut SVec<u8>, name: u32, kind: u32, flags: u64, offset: u64, size: u64, link: u32, info: u32, align: u64, entsize: u64| {
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

const SHT_PROGBITS: u32 = 1;
const SHT_SYMTAB: u32 = 2;
const SHT_STRTAB: u32 = 3;
const SHT_RELA: u32 = 4;
const SHT_GROUP: u32 = 17;
const SHF_ALLOC_EXEC: u64 = 0x6;
const SHF_INFO_LINK: u64 = 0x40;
const SHF_GROUP: u64 = 0x200;
const R_AARCH64_CALL26: u32 = 283;

/// An ELF object of `texts` (each `.text.<name>` holding `insts`, with `calls` CALL26 at the
/// given offsets to the given symbol numbers), the symbols `symbols` (numbered from 1), and a
/// COMDAT group over the first text section when `comdat` names its signature symbol.
fn elf_object(texts: &[(&'static [u8], SVec<u32>, SVec<(u64, u32)>)], symbols: &[ElfSym], comdat: Option<u32>) -> SVec<u8> {
    let (symtab, strtab) = elf_symtab(symbols);
    let mut sections = SVec::new();
    let first_text = if comdat.is_some() { 2u32 } else { 1 };
    if let Some(signature) = comdat {
        let mut group = SVec::new();
        put32(&mut group, 1);
        put32(&mut group, first_text);
        // The symtab is after every text and rela section.
        let symtab_index = first_text + texts.iter().map(|t| if t.2.is_empty() { 1 } else { 2 }).sum::<u32>();
        sections.push(ElfSection { name: b".group", kind: SHT_GROUP, flags: 0, align: 4, bytes: group, link: symtab_index, info: signature, entsize: 4 });
    }
    // A text section's index, and its rela section's after it when it has calls.
    let mut indexes = SVec::new();
    let mut next = first_text;
    for (_, _, calls) in texts {
        indexes.push(next);
        next += if calls.is_empty() { 1 } else { 2 };
    }
    let symtab_index = next;
    for (i, (name, insts, calls)) in texts.iter().enumerate() {
        let flags = SHF_ALLOC_EXEC | if comdat.is_some() && i == 0 { SHF_GROUP } else { 0 };
        sections.push(ElfSection { name, kind: SHT_PROGBITS, flags, align: 4, bytes: code(insts), link: 0, info: 0, entsize: 0 });
        if !calls.is_empty() {
            let rela: SVec<(u64, u32, u32, i64)> = calls.iter().map(|&(at, sym)| (at, sym, R_AARCH64_CALL26, 0)).collect();
            sections.push(ElfSection { name: b".rela.text", kind: SHT_RELA, flags: SHF_INFO_LINK, align: 8, bytes: elf_rela(&rela), link: symtab_index, info: indexes[i], entsize: 24 });
        }
    }
    let locals = 1 + symbols.iter().take_while(|s| s.info >> 4 == 0).count() as u32;
    sections.push(ElfSection { name: b".symtab", kind: SHT_SYMTAB, flags: 0, align: 8, bytes: symtab, link: symtab_index + 1, info: locals, entsize: 24 });
    sections.push(ElfSection { name: b".strtab", kind: SHT_STRTAB, flags: 0, align: 1, bytes: strtab, link: 0, info: 0, entsize: 0 });
    elf(&sections)
}

// ---- helpers ----

const UNIT: UnitKey = UnitKey([7; 32]);

fn split_mac(bytes: &[u8]) -> Result<Split<'_>, SplitError<'_>> {
    split(bytes, SplitOpts { platform: Platform::MacosArm64, alloc: heap() }, NoPlant)
}

fn split_elf(bytes: &[u8]) -> Result<Split<'_>, SplitError<'_>> {
    split(bytes, SplitOpts { platform: Platform::LinuxArm64, alloc: heap() }, NoPlant)
}

fn find(split: &Split<'_>, name: &[u8]) -> usize {
    split
        .atoms
        .iter()
        .position(|a| match a.name {
            AtomName::Global(n) | AtomName::Local { name: n, .. } => n == name,
            AtomName::Anonymous { .. } => false,
        })
        .unwrap_or_else(|| panic!("no atom named {:?}", StdString::from_utf8_lossy(name)))
}

fn code_of<'r, 'a>(lowered: &'r Lowered<'a>) -> (Symbol, &'r blaze_code::record::Sealed<'a>) {
    match lowered {
        Lowered::Code { symbol, record } => (*symbol, record),
        Lowered::Data { .. } => panic!("a datum where a code record was wanted"),
    }
}

/// The run's table, faked: each target's word at `WORDS + 8 * k`, k in first-asked order.
struct Table {
    seen: SVec<Target>,
}

const WORDS: u64 = 0x4000_0000;
const CODE_AT: u64 = 0x1000_0000;

impl Table {
    fn word_of(&mut self, target: Target) -> u64 {
        let k = match self.seen.iter().position(|t| *t == target) {
            Some(k) => k,
            None => {
                self.seen.push(target);
                self.seen.len() - 1
            }
        };
        WORDS + 8 * k as u64
    }
}

impl Words for Table {
    fn word(&mut self, target: Target, _call: bool) -> Result<u64, ImageError> {
        Ok(self.word_of(target))
    }
    fn value(&mut self, target: Target) -> Result<u64, ImageError> {
        Ok(self.word_of(target) + 0x100_0000)
    }
}

/// Encodes, reads back and places a sealed record at [`CODE_AT`]; the placed code and the table.
fn placed(symbol: Symbol, record: &blaze_code::record::Sealed<'_>) -> (SVec<u8>, Table) {
    let mut bytes: AVec<'_, u8> = Vec::new_in(heap());
    record.encode(&mut bytes);
    let view = CodeView::read(&bytes).unwrap_or_else(|why| panic!("the encoded record does not read: {why:?}"));
    let mut table = Table { seen: SVec::new() };
    let mut code = vec![0u8; view.code().len()];
    let mut relro = vec![0u8; view.relro().len()];
    let at = At { code: CODE_AT, relro: CODE_AT + 0x10_0000 };
    place(&view, symbol, at, &mut table, &mut code, &mut relro).unwrap_or_else(|why| panic!("placing refused: {why}"));
    (code, table)
}

fn outside(name: &str) -> Target {
    Target { space: Space::Outside, symbol: Symbol::of_name(name) }
}

// ---- golden objects: what is accepted ----

/// `_f` calls `_g` (defined here) and `_ext` (undefined); `_g` returns. Each call becomes a
/// branch to a veneer whose `Load` site names the callee BY NAME; placed, the branch reaches the
/// veneer and the veneer's pair loads the callee's word.
#[test]
fn a_call_is_a_branch_to_a_veneer_that_loads_the_callee_by_name() {
    let insts = code(&[BL0, BL0, RET, RET]);
    let relocs = vec![branch(0, 1), branch(4, 2)];
    let bytes = macho(SUBSECTIONS, &[text(insts, relocs)], &[global(b"_f", 1, 0), global(b"_g", 1, 12), undefined(b"_ext")]);
    let split = split_mac(&bytes).unwrap_or_else(|why| panic!("{why}"));
    assert_eq!(split.atoms.len(), 2);
    let f = find(&split, b"f");
    assert_eq!(split.atoms[f].kind, AtomKind::Code);
    assert_eq!(split.atoms[f].strength, Strength::Strong);
    assert_eq!(split.atoms[f].size, 12);
    let refs = split.refs_of(f);
    assert_eq!(refs.len(), 2);
    assert_eq!(refs[0].to, RefTo::Name { name: b"g", weak: false });
    assert_eq!(refs[1].to, RefTo::Name { name: b"ext", weak: false });
    assert_eq!(refs[0].kind, RefKind::Branch26);

    let lowered = lower(&split, UNIT, f, heap()).unwrap_or_else(|why| panic!("{why}"));
    let (symbol, record) = code_of(&lowered);
    assert_eq!(symbol, Symbol::of_name("f"));
    let r = record.record();
    assert_eq!(r.code.len(), 12 + 2 * 12, "two veneers after the 12 bytes of code");
    assert_eq!(r.sites.len(), 2);
    assert!(r.sites.iter().all(|s| s.kind == SiteKind::Load));
    let mut targets: SVec<Target> = r.targets.iter().copied().collect();
    targets.sort();
    let mut want = vec![outside("g"), outside("ext")];
    want.sort();
    assert_eq!(targets, want);

    let (placed, mut table) = placed(symbol, record);
    for (call, veneer, callee) in [(0u64, 12u64, "g"), (4, 24, "ext")] {
        assert_eq!(bl_target(read32(&placed, call), CODE_AT + call), Some(CODE_AT + veneer), "the call at {call} reaches its veneer");
        let loads = adrp_ldr_target(read32(&placed, veneer), read32(&placed, veneer + 4), CODE_AT + veneer);
        assert_eq!(loads, Some(table.word_of(outside(callee))), "the veneer at {veneer} loads {callee}'s word");
        assert_eq!(read32(&placed, veneer + 8), 0xD61F_0200, "then branches to it");
    }
}

/// `adrp x8, _ext@GOTPAGE; ldr x8, [x8, _ext@GOTPAGEOFF]` is a `Load` site in place.
#[test]
fn a_got_pair_is_a_load_site() {
    let insts = code(&[adrp_x(8), ldr_x(8, 8), RET]);
    let relocs = vec![(4, rword(1, false, 2, true, R_GOT_PAGEOFF12)), (0, rword(1, true, 2, true, R_GOT_PAGE21))];
    let bytes = macho(SUBSECTIONS, &[text(insts, relocs)], &[global(b"_f", 1, 0), undefined(b"_ext")]);
    let split = split_mac(&bytes).unwrap_or_else(|why| panic!("{why}"));
    let lowered = lower(&split, UNIT, 0, heap()).unwrap_or_else(|why| panic!("{why}"));
    let (symbol, record) = code_of(&lowered);
    let r = record.record();
    assert_eq!(r.code.len(), 12, "no veneer");
    assert_eq!(r.sites.len(), 1);
    assert_eq!((r.sites[0].at, r.sites[0].kind), (0, SiteKind::Load));
    let (placed, mut table) = placed(symbol, record);
    assert_eq!(adrp_ldr_target(read32(&placed, 0), read32(&placed, 4), CODE_AT), Some(table.word_of(outside("ext"))));
}

/// `adrp x0, l_.str@PAGE; add x0, x0, l_.str@PAGEOFF` (the address of a string literal of the
/// unit) becomes `adrp; ldr` of the literal's word; the literal is a read-only datum under the
/// unit's local symbol.
#[test]
fn an_address_pair_becomes_a_load_of_the_targets_word() {
    let insts = code(&[adrp_x(0), add_x(0, 0), RET]);
    // Symbol 1 is `l_.str`.
    let relocs = vec![(4, rword(1, false, 2, true, R_PAGEOFF12)), (0, rword(1, true, 2, true, R_PAGE21))];
    let strings = MachSection { segment: b"__TEXT", name: b"__cstring", flags: S_CSTRING, align_log2: 0, bytes: b"hi\0".to_vec(), zero: 0, relocs: SVec::new() };
    let bytes = macho(SUBSECTIONS, &[text(insts, relocs), strings], &[global(b"_f", 1, 0), local(b"l_.str", 2, 0)]);
    let split = split_mac(&bytes).unwrap_or_else(|why| panic!("{why}"));
    let s = find(&split, b"l_.str");
    assert_eq!(split.atoms[s].kind, AtomKind::CStrings);
    assert_eq!(split.atoms[s].name, AtomName::Local { name: b"l_.str", nth: 0 });
    let f = find(&split, b"f");
    assert_eq!(split.refs_of(f)[0].to, RefTo::Atom(s as u32));

    let lowered = lower(&split, UNIT, f, heap()).unwrap_or_else(|why| panic!("{why}"));
    let (symbol, record) = code_of(&lowered);
    let literal = Target { space: Space::Static, symbol: local_symbol(UNIT, &split.atoms[s].name) };
    assert_eq!(&record.record().targets[..], &[literal]);
    let (placed, mut table) = placed(symbol, record);
    assert_eq!(read32(&placed, 4) & 0xFFC0_0000, 0xF940_0000, "the add is now a 64-bit ldr");
    assert_eq!(adrp_ldr_target(read32(&placed, 0), read32(&placed, 4), CODE_AT), Some(table.word_of(literal)));

    match lower(&split, UNIT, s, heap()).unwrap_or_else(|why| panic!("{why}")) {
        Lowered::Data { symbol, datum } => {
            assert_eq!(symbol, local_symbol(UNIT, &split.atoms[s].name));
            let mut bytes: AVec<'_, u8> = Vec::new_in(heap());
            datum.encode(&mut bytes);
            let view = DatumView::read(&bytes).unwrap_or_else(|why| panic!("{why}"));
            assert_eq!(view.place(), DatumPlace::ReadOnly);
            assert_eq!(view.bytes(), b"hi\0");
        }
        Lowered::Code { .. } => panic!("a string literal lowered to code"),
    }
}

/// An 8-byte pointer in `__DATA,__data` to a local function is a datum word naming the function
/// by its unit-local symbol; `__bss` is a zeroed datum of its size.
#[test]
fn data_words_and_zero_fill_are_datums() {
    let insts = code(&[RET]);
    let data = MachSection { segment: b"__DATA", name: b"__data", flags: 0, align_log2: 3, bytes: vec![0u8; 16], zero: 0, relocs: vec![(8, rword(0, false, 3, true, R_UNSIGNED))] };
    let bss = MachSection { segment: b"__DATA", name: b"__bss", flags: S_ZEROFILL, align_log2: 4, bytes: SVec::new(), zero: 64, relocs: SVec::new() };
    let bytes = macho(SUBSECTIONS, &[text(insts, SVec::new()), data, bss], &[local(b"_helper", 1, 0), global(b"_table", 2, 0), global(b"_zeros", 3, 0)]);
    let split = split_mac(&bytes).unwrap_or_else(|why| panic!("{why}"));
    let helper = find(&split, b"_helper");
    let table = find(&split, b"table");
    let zeros = find(&split, b"zeros");
    assert_eq!(split.atoms[table].kind, AtomKind::Data);
    assert_eq!(split.atoms[zeros].kind, AtomKind::Zero);
    assert_eq!(split.atoms[zeros].size, 64);
    match lower(&split, UNIT, table, heap()).unwrap_or_else(|why| panic!("{why}")) {
        Lowered::Data { datum, .. } => {
            let d = datum.datum();
            assert_eq!(d.place, DatumPlace::Writable);
            assert_eq!(&d.targets[..], &[Target { space: Space::Func, symbol: local_symbol(UNIT, &split.atoms[helper].name) }]);
            assert_eq!((d.words[0].at, d.words[0].addend), (8, 0));
        }
        Lowered::Code { .. } => panic!("data lowered to code"),
    }
    match lower(&split, UNIT, zeros, heap()).unwrap_or_else(|why| panic!("{why}")) {
        Lowered::Data { datum, .. } => assert_eq!(datum.datum().place, DatumPlace::Zeroed { len: 64 }),
        Lowered::Code { .. } => panic!("zero fill lowered to code"),
    }
}

/// A call within one atom (a function calling a label of itself, via its own global symbol
/// plus an addend is not made by clang; via the atom it is) is completed in place, no veneer.
#[test]
fn a_branch_within_its_own_atom_is_completed_in_place() {
    let insts = code(&[NOP, BL0, RET]);
    // Against the local `ltmp0` at the atom's first byte, plus nothing: a branch back to 0.
    let relocs = vec![branch(4, 0)];
    let bytes = macho(SUBSECTIONS, &[text(insts, relocs)], &[local(b"ltmp0", 1, 0)]);
    let split = split_mac(&bytes).unwrap_or_else(|why| panic!("{why}"));
    let lowered = lower(&split, UNIT, 0, heap()).unwrap_or_else(|why| panic!("{why}"));
    let (_, record) = code_of(&lowered);
    let r = record.record();
    assert!(r.sites.is_empty());
    assert_eq!(bl_target(read32(&r.code, 4), 4), Some(0));
}

/// `__mod_init_func` is the unit's init rows; a weak definition is `Strength::Weak`.
#[test]
fn init_pointers_are_init_rows_and_weak_is_weak() {
    let insts = code(&[RET, RET]);
    let init = MachSection { segment: b"__DATA", name: b"__mod_init_func", flags: S_MOD_INIT, align_log2: 3, bytes: vec![0u8; 8], zero: 0, relocs: vec![(0, rword(0, false, 3, true, R_UNSIGNED))] };
    let weak = MachSym { name: b"_w", n_type: N_EXT_SECT, n_sect: 1, n_desc: N_WEAK_DEF, value: 4 };
    let bytes = macho(SUBSECTIONS, &[text(insts, SVec::new()), init], &[local(b"_ctor", 1, 0), weak]);
    let split = split_mac(&bytes).unwrap_or_else(|why| panic!("{why}"));
    assert_eq!(split.inits.len(), 1);
    let ctor = find(&split, b"_ctor");
    assert_eq!((split.inits[0].fini, split.inits[0].priority, split.inits[0].to), (false, NO_PRIORITY, RefTo::Atom(ctor as u32)));
    assert_eq!(split.atoms[find(&split, b"w")].strength, Strength::Weak);
}

/// ELF with one function a section: a CALL26 to an undefined global is a reference by name;
/// a section in a COMDAT group is `Strength::Comdat`.
#[test]
fn elf_function_sections_split_one_atom_each_and_comdat_is_comdat() {
    // Symbols: 1 `.text.f`'s section symbol (local), 2 `f` (global, in the group), 3 `g`
    // (global), 4 `ext` (undefined).
    let symbols = [
        ElfSym { name: b"", info: 0x03, shndx: 2, value: 0 },
        ElfSym { name: b"f", info: 0x12, shndx: 2, value: 0 },
        ElfSym { name: b"g", info: 0x12, shndx: 4, value: 0 },
        ElfSym { name: b"ext", info: 0x10, shndx: 0, value: 0 },
    ];
    let texts = [(&b".text.f"[..], vec![BL0, RET], vec![(0u64, 4u32)]), (&b".text.g"[..], vec![RET], SVec::new())];
    let bytes = elf_object(&texts, &symbols, Some(2));
    let split = split_elf(&bytes).unwrap_or_else(|why| panic!("{why}"));
    assert_eq!(split.atoms.len(), 2);
    let f = find(&split, b"f");
    assert_eq!(split.atoms[f].strength, Strength::Comdat);
    assert_eq!(split.atoms[find(&split, b"g")].strength, Strength::Strong);
    assert_eq!(split.refs_of(f)[0].to, RefTo::Name { name: b"ext", weak: false });
}

// ---- golden objects: what is refused, by name ----

#[test]
fn a_macho_object_without_subsections_is_refused() {
    let bytes = macho(0, &[text(code(&[RET]), SVec::new())], &[global(b"_f", 1, 0)]);
    assert!(matches!(split_mac(&bytes), Err(SplitError::NotSubsections)));
}

#[test]
fn an_elf_section_with_two_functions_is_refused_naming_it() {
    let symbols = [ElfSym { name: b"f", info: 0x12, shndx: 1, value: 0 }, ElfSym { name: b"g", info: 0x12, shndx: 1, value: 4 }];
    let bytes = elf_object(&[(&b".text"[..], vec![RET, RET], SVec::new())], &symbols, None);
    match split_elf(&bytes) {
        Err(SplitError::SharedSection { section, at }) => assert_eq!((section.0, at), (&b".text"[..], 4)),
        other => panic!("not refused as a shared section: {:?}", other.err()),
    }
}

#[test]
fn a_common_symbol_is_refused_naming_it() {
    let common = MachSym { name: b"_c", n_type: N_UNDF_EXT, n_sect: 0, n_desc: 0x0300, value: 8 };
    let bytes = macho(SUBSECTIONS, &[text(code(&[RET]), SVec::new())], &[global(b"_f", 1, 0), common]);
    match split_mac(&bytes) {
        Err(SplitError::Common { symbol }) => assert_eq!(symbol.0, b"_c"),
        other => panic!("not refused as common: {:?}", other.err()),
    }
}

/// What the record cannot hold yet is refused naming the atom, the offset and what is lacking.
#[test]
fn what_the_record_does_not_hold_yet_is_refused_by_name() {
    // A GOT pair with an instruction between its halves.
    let insts = code(&[adrp_x(8), NOP, ldr_x(8, 8), RET]);
    let relocs = vec![(8, rword(1, false, 2, true, R_GOT_PAGEOFF12)), (0, rword(1, true, 2, true, R_GOT_PAGE21))];
    let bytes = macho(SUBSECTIONS, &[text(insts, relocs)], &[global(b"_f", 1, 0), undefined(b"_ext")]);
    let split = split_mac(&bytes).unwrap_or_else(|why| panic!("{why}"));
    match lower(&split, UNIT, 0, heap()) {
        Err(LowerError::NotAccepted { at: 0, what, .. }) => assert!(what.contains("two-offset site"), "{what}"),
        Err(other) => panic!("refused for another reason: {other}"),
        Ok(_) => panic!("a split pair was accepted"),
    }
    // A load through a direct page offset: `ldr w0, [x8, _g@PAGEOFF]`.
    let insts = code(&[adrp_x(8), ldr_w(0, 8), RET]);
    let relocs = vec![(4, rword(1, false, 2, true, R_PAGEOFF12)), (0, rword(1, true, 2, true, R_PAGE21))];
    let bytes = macho(SUBSECTIONS, &[text(insts, relocs)], &[global(b"_f", 1, 0), undefined(b"_g")]);
    let split = split_mac(&bytes).unwrap_or_else(|why| panic!("{why}"));
    match lower(&split, UNIT, 0, heap()) {
        Err(LowerError::NotAccepted { at: 4, what, .. }) => assert!(what.contains("direct page offset"), "{what}"),
        Err(other) => panic!("refused for another reason: {other}"),
        Ok(_) => panic!("a load through a page offset was accepted"),
    }
}

// ---- keys ----

fn parts<'a>(headers: &'a [HeaderRead<'a>], flags: &'a [&'a str]) -> UnitKeyParts<'a> {
    UnitKeyParts { lang: Lang::C, path: "src/a.c", source: [1; 32], headers, flags, target: Platform::MacosArm64, toolchain: [2; 32] }
}

/// Every part moves the key; a read list out of order or twice is refused naming the header.
#[test]
fn every_input_moves_the_unit_key_and_a_bad_read_list_is_refused() {
    let headers = [HeaderRead { root: Root::Include(0), path: "a.h", digest: [3; 32] }, HeaderRead { root: Root::Shelf, path: "stdio.h", digest: [4; 32] }];
    let flags = ["-DX=1", "-O2"];
    let swapped = ["-O2", "-DX=1"];
    let edited = [HeaderRead { digest: [5; 32], ..headers[0] }, headers[1]];
    let base = unit_key(&parts(&headers, &flags)).unwrap_or_else(|why| panic!("{why}"));
    let mut moved = SVec::new();
    moved.push(unit_key(&UnitKeyParts { lang: Lang::Cxx, ..parts(&headers, &flags) }));
    moved.push(unit_key(&UnitKeyParts { path: "src/b.c", ..parts(&headers, &flags) }));
    moved.push(unit_key(&UnitKeyParts { source: [9; 32], ..parts(&headers, &flags) }));
    moved.push(unit_key(&UnitKeyParts { target: Platform::LinuxArm64, ..parts(&headers, &flags) }));
    moved.push(unit_key(&UnitKeyParts { toolchain: [9; 32], ..parts(&headers, &flags) }));
    moved.push(unit_key(&parts(&headers[..1], &flags)));
    moved.push(unit_key(&parts(&headers, &swapped)));
    moved.push(unit_key(&parts(&edited, &flags)));
    for (n, key) in moved.into_iter().enumerate() {
        let key = key.unwrap_or_else(|why| panic!("{why}"));
        assert_ne!(key, base, "input {n} did not move the key");
    }
    let reversed = [headers[1], headers[0]];
    assert_eq!(unit_key(&parts(&reversed, &flags)), Err(KeyError::HeadersUnsorted { root: Root::Include(0), path: "a.h" }));
    let twice = [headers[0], headers[0]];
    assert_eq!(unit_key(&parts(&twice, &flags)), Err(KeyError::HeaderTwice { root: Root::Include(0), path: "a.h" }));
    assert_eq!(unit_key(&UnitKeyParts { path: "/abs/a.c", ..parts(&headers, &flags) }), Err(KeyError::Path { of: None, path: "/abs/a.c" }));
    let other = UnitKey([8; 32]);
    let name = AtomName::Local { name: b"x", nth: 0 };
    assert_ne!(atom_key(base, &name), atom_key(other, &name), "an atom's key is its unit's");
    assert_ne!(local_symbol(base, &name), local_symbol(other, &name), "a local symbol is its unit's");
}

// ---- the symbol index ----

fn unit(n: u8) -> UnitKey {
    UnitKey([n; 32])
}

fn def(name: &'static [u8], strength: Strength) -> Def<'static> {
    Def { name, strength, kind: DefKind::Func }
}

fn index_of<'a>(units: &[UnitDefs<'a>], plant: impl Planted<IndexPlant>) -> Result<SVec<u8>, IndexError<'a>> {
    let mut out: AVec<'_, u8> = Vec::new_in(heap());
    build(units, &mut out, heap(), plant)?;
    Ok(out.as_slice().to_vec())
}

#[test]
fn two_strong_definitions_are_refused_naming_both_units() {
    let a = [def(b"f", Strength::Strong)];
    let b = [def(b"f", Strength::Strong)];
    let units = [UnitDefs { unit: unit(5), defs: &a }, UnitDefs { unit: unit(3), defs: &b }];
    match index_of(&units, NoPlant) {
        Err(IndexError::Twice { name, first, second }) => assert_eq!((name.0, first, second), (&b"f"[..], unit(3), unit(5))),
        other => panic!("not refused: {:?}", other.err()),
    }
}

#[test]
fn strong_wins_over_weak_and_weak_is_the_least_unit_key() {
    let a = [def(b"f", Strength::Weak), def(b"g", Strength::Comdat)];
    let b = [def(b"f", Strength::Strong), def(b"g", Strength::Comdat)];
    let c = [def(b"g", Strength::Comdat), Def { name: b"v", strength: Strength::Strong, kind: DefKind::Data }];
    let units = [UnitDefs { unit: unit(1), defs: &a }, UnitDefs { unit: unit(9), defs: &b }, UnitDefs { unit: unit(4), defs: &c }];
    let bytes = index_of(&units, NoPlant).unwrap_or_else(|why| panic!("{why}"));
    let view = IndexView::read(&bytes).unwrap_or_else(|why| panic!("{why}"));
    assert_eq!(view.len(), 3);
    assert_eq!(view.find(b"f").map(|r| (r.unit, r.strength)), Some((unit(9), Strength::Strong)));
    assert_eq!(view.find(b"g").map(|r| r.unit), Some(unit(1)));
    assert_eq!(view.find(b"v").map(|r| r.kind), Some(DefKind::Data));
    assert!(view.find(b"h").is_none());
    let kinds = [Def { name: b"f", strength: Strength::Weak, kind: DefKind::Data }];
    let both = [UnitDefs { unit: unit(1), defs: &a }, UnitDefs { unit: unit(2), defs: &kinds }];
    assert!(matches!(index_of(&both, NoPlant), Err(IndexError::Kinds { .. })));
}

// ---- simulations ----

/// What the split's oracle saw broken.
#[derive(Debug)]
enum SplitBroken {
    /// The atoms are not the functions drawn.
    Atoms { want: usize, got: usize },
    /// Function `atom` has `got` references where it makes `want` calls.
    LostRef { atom: usize, want: usize, got: usize },
    /// Call `call` of function `atom` names another symbol.
    WrongTarget { atom: usize, call: usize },
    /// A lowered call does not reach its callee's word once placed.
    Placed { atom: usize, call: usize },
}

impl fmt::Display for SplitBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

blaze_sim::plants! {
    impl SplitPlant breaks SplitBroken {
        None,
        DropsRef => SplitBroken::LostRef { .. },
    }
}

const NAMES: [&[u8]; 6] = [b"_f0", b"_f1", b"_f2", b"_f3", b"_f4", b"_f5"];
const OUTSIDE: [&[u8]; 2] = [b"_x0", b"_x1"];

/// A drawn `__text` of functions, each a run of calls then `ret`, to drawn callees (functions of
/// the unit or two outside names). Every call is a reference by name to the callee drawn, and
/// once lowered and placed, every call reaches a veneer that loads the callee's word.
fn split_round_trip(tape: &Tape, plant: impl Planted<SplitPlant>) -> Result<(), Broken<SplitBroken>> {
    let functions = tape.int_in(1usize..=NAMES.len(), "functions");
    let mut calls: SVec<SVec<usize>> = SVec::new();
    for _ in 0..functions {
        let n = tape.int_in(0usize..=3, "calls");
        calls.push((0..n).map(|_| tape.int_in(0usize..=functions + OUTSIDE.len() - 1, "callee")).collect());
    }
    let mut insts = SVec::new();
    let mut relocs = SVec::new();
    let mut symbols = SVec::new();
    for (i, callees) in calls.iter().enumerate() {
        symbols.push(global(NAMES[i], 1, 4 * insts.len() as u64));
        for &callee in callees {
            relocs.push(branch(4 * insts.len() as u32, callee as u32));
            insts.push(BL0);
        }
        insts.push(RET);
    }
    // Symbol numbers: the functions, then the outside names.
    let mut table = symbols;
    for name in OUTSIDE {
        table.push(undefined(name));
    }
    let bytes = macho(SUBSECTIONS, &[text(code(&insts), relocs)], &table);
    let split = split(&bytes, SplitOpts { platform: Platform::MacosArm64, alloc: heap() }, plant).map_err(|why| Broken::Harness(format!("refused: {why}")))?;
    if split.atoms.len() != functions {
        return Err(Broken::Oracle(SplitBroken::Atoms { want: functions, got: split.atoms.len() }));
    }
    for (atom, callees) in calls.iter().enumerate() {
        let refs = split.refs_of(atom);
        if refs.len() != callees.len() {
            return Err(Broken::Oracle(SplitBroken::LostRef { atom, want: callees.len(), got: refs.len() }));
        }
        for (call, (r, &callee)) in refs.iter().zip(callees).enumerate() {
            let name = if callee < functions { NAMES[callee] } else { OUTSIDE[callee - functions] };
            if r.to != (RefTo::Name { name: &name[1..], weak: false }) || r.at != 4 * call as u32 {
                return Err(Broken::Oracle(SplitBroken::WrongTarget { atom, call }));
            }
        }
        let lowered = lower(&split, UNIT, atom, heap()).map_err(|why| Broken::Harness(format!("not lowered: {why}")))?;
        let (symbol, record) = code_of(&lowered);
        let (placed, mut words) = placed(symbol, record);
        for (call, &callee) in callees.iter().enumerate() {
            let name = if callee < functions { NAMES[callee] } else { OUTSIDE[callee - functions] };
            let at = 4 * call as u64;
            let reached = bl_target(read32(&placed, at), CODE_AT + at).and_then(|v| {
                let off = v - CODE_AT;
                adrp_ldr_target(read32(&placed, off), read32(&placed, off + 4), v)
            });
            let want = words.word_of(outside(core::str::from_utf8(&name[1..]).unwrap_or("")));
            if reached != Some(want) {
                return Err(Broken::Oracle(SplitBroken::Placed { atom, call }));
            }
        }
    }
    Ok(())
}

#[test]
fn every_call_survives_the_split_and_the_placement() {
    check(Config::cases(300), |tape| split_round_trip(tape, NoPlant));
}

#[test]
fn a_dropped_relocation_is_caught() {
    controls(Config::cases(300), |tape, plant: SplitPlant| split_round_trip(tape, plant));
}

/// What the index's oracle saw broken.
#[derive(Debug)]
enum IndexBroken {
    /// The record depends on the order the units were given in.
    Order,
    /// Name `name` chose unit `got`, not the least key `want` among its weak definitions.
    NotLeast { name: usize, want: u8, got: u8 },
}

impl fmt::Display for IndexBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

blaze_sim::plants! {
    impl IndexPlant breaks IndexBroken {
        None,
        FirstGiven => IndexBroken::Order | IndexBroken::NotLeast { .. },
    }
}

const DEF_NAMES: [&[u8]; 4] = [b"a", b"b", b"c", b"d"];

/// Units with drawn weak and COMDAT definitions, given in two drawn orders: the records are
/// equal, and each name's unit is the least key defining it.
fn index_choice(tape: &Tape, plant: impl Planted<IndexPlant>) -> Result<(), Broken<IndexBroken>> {
    let count = tape.int_in(1usize..=5, "units");
    let mut defs: SVec<SVec<Def<'static>>> = SVec::new();
    for _ in 0..count {
        let mut these = SVec::new();
        for name in DEF_NAMES {
            if tape.coin("defines") {
                let strength = if tape.coin("comdat") { Strength::Comdat } else { Strength::Weak };
                these.push(def(name, strength));
            }
        }
        defs.push(these);
    }
    let keys: SVec<u8> = (0..count).map(|i| 10 + i as u8).collect();
    let mut order: SVec<usize> = (0..count).collect();
    tape.shuffle(&mut order, "order");
    let given = |order: &[usize]| order.iter().map(|&i| UnitDefs { unit: unit(keys[i]), defs: &defs[i][..] }).collect::<SVec<_>>();
    let first = index_of(&given(&order), plant).map_err(|why| Broken::Harness(format!("refused: {why}")))?;
    order.reverse();
    let second = index_of(&given(&order), plant).map_err(|why| Broken::Harness(format!("refused: {why}")))?;
    if first != second {
        return Err(Broken::Oracle(IndexBroken::Order));
    }
    let view = IndexView::read(&first).map_err(|why| Broken::Harness(format!("does not read: {why}")))?;
    for (name, text) in DEF_NAMES.iter().enumerate() {
        let want = (0..count).filter(|&i| defs[i].iter().any(|d| d.name == *text)).map(|i| keys[i]).min();
        let got = view.find(text).map(|r| r.unit.0[0]);
        if want != got {
            return Err(Broken::Oracle(IndexBroken::NotLeast { name, want: want.unwrap_or(0), got: got.unwrap_or(0) }));
        }
    }
    Ok(())
}

#[test]
fn the_index_is_a_function_of_the_units_alone() {
    check(Config::cases(300), |tape| index_choice(tape, NoPlant));
}

#[test]
fn a_weak_symbol_chosen_by_given_order_is_caught() {
    controls(Config::cases(300), |tape, plant: IndexPlant| index_choice(tape, plant));
}
