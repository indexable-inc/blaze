//! Layout and relocation of a resolved [`Plan`] into one [`Image`] at image-relative addresses
//! (NATIVE.md 3.4 and 3.6; M6, M7). Sans-IO: objects in, one image out, in the allocator the
//! caller names; nothing of a map's order reaches the image (every table is sorted by a key of
//! the inputs, or in pull order).
//!
//! The order of the image (see [`crate::image`] for what each region holds): Text (code, then a
//! 12-byte stub per Rust word or import a branch reaches, then far-branch veneers), Const,
//! EhFrame, UnwindInfo, then on a new page Got, Data (the descriptor first), Words, Inits,
//! ThreadVars, ThreadData, ThreadZero, Zero. Sections keep (pull order, section order) inside
//! their region; a weak or common loser keeps its bytes while its name points at the winner (as
//! lld); the sections of a losing COMDAT group are not laid.
//!
//! Every PC-relative relocation inside the image is resolved here, with the ranges of the
//! Arm Architecture Reference Manual (A64 "B", "BL": imm26, +-128 MiB; "B.cond", "CBZ": imm19,
//! +-1 MiB; "TBZ": imm14, +-32 KiB; "ADRP": imm21 pages, +-4 GiB). A `Branch26` out of range
//! goes through a veneer (`adrp x16; add x16; br x16`, one per target, as lld's thunks), any
//! other out-of-range field is refused `OutOfRange`. What only the loader can know (the image's
//! base, an import's address, a Rust word) is a [`Fixup`]; ELF thread-local GOT entries are
//! [`TlsReloc`]s.
//!
//! Precondition: `plan` is what `resolve::plan` made of the same `inputs` (its object, symbol
//! and input indices index them); a plan that breaks this is a bug of the caller's and panics
//! at the index.

use blaze_own::{AVec, Alloc, Vec};

use crate::Platform;
use crate::descriptor::{Descriptor, MAGIC as DESCRIPTOR_MAGIC, NAME_ROW_LEN, POINTER_ROW_LEN};
use crate::image::{DESCRIPTOR_LEN, FixTo, Fixup, Image, Import, Placed, Region, TlsReloc, descriptor_name};
use crate::object::{Bind, Def, Reloc, RelocKind, RelocTo, SectionKind};
use crate::resolve::{Input, Origin, Plan, Resolution};
use crate::unwind::{self, Row, UnwindInfoError};

/// The version of what `lay` produces for an unchanged plan (region order, stubs, the
/// descriptor, the unwind index's form). The program's native library record (NATIVE.md 3.1,
/// W7) folds it into its key; bump it with any change to an image's bytes.
pub const LAYOUT_VERSION: u32 = 1;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Layout { pub page: u64, pub header_reserve: u64 } // macOS: 0x4000, 0x4000; Linux: 0x10000, 0x10000

impl Layout {
    pub const MACOS_ARM64: Layout = Layout { page: 0x4000, header_reserve: 0x4000 };
    pub const LINUX_ARM64: Layout = Layout { page: 0x1_0000, header_reserve: 0x1_0000 };

    /// The platform a layout is for: the two pairs NATIVE.md W3 fixes, nothing else.
    pub fn platform(self) -> Option<Platform> {
        if self == Layout::MACOS_ARM64 {
            Some(Platform::MacosArm64)
        } else if self == Layout::LINUX_ARM64 {
            Some(Platform::LinuxArm64)
        } else {
            None
        }
    }
}

/// Why `lay` refused. Beyond NATIVE.md W3's list (kept verbatim): `Unplaced` (a reference to
/// a name the plan does not resolve, a local symbol with no place, or a section the layout did
/// not lay: a dropped COMDAT section, a skipped one), and `UnknownLayout` (a `Layout` that is
/// neither platform's pair, since `lay` takes the platform from it).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LayoutError<'a> {
    Section { from: Origin<'a>, segment: &'a [u8], name: &'a [u8] },     // SectionKind::Other
    TermFunctions { from: Origin<'a> }, TlsLocalExec { from: Origin<'a> },
    RustDirectAddress { name: &'a [u8], from: Origin<'a> },
    OutOfRange { kind: RelocKind, from: Origin<'a>, offset: u32 },
    Unwind { from: Origin<'a>, why: &'static str }, Alloc,
    Unplaced { name: &'a [u8], from: Origin<'a> },
    UnknownLayout { page: u64, header_reserve: u64 },
    /// The descriptor reserved `counted` pointer rows (the relocations whose target is a Rust
    /// word) and relocation made `found` `FixTo::Word` fixups: the count's test and `apply`'s
    /// disagree (BNL2, descriptor.rs).
    PointerRows { counted: u32, found: u32 },
}

/// An allocation the caller's allocator refused.
struct Full;

impl From<Full> for LayoutError<'_> {
    fn from(_: Full) -> Self {
        LayoutError::Alloc
    }
}

fn push<T>(v: &mut AVec<'_, T>, value: T) -> Result<(), Full> {
    v.try_push(value).map_err(|_| Full)
}

const REGIONS: [Region; 12] = [
    Region::Text, Region::Const, Region::EhFrame, Region::UnwindInfo, Region::Got, Region::Data,
    Region::Words, Region::Inits, Region::ThreadVars, Region::ThreadData, Region::ThreadZero, Region::Zero,
];

fn ri(region: Region) -> usize {
    match region {
        Region::Text => 0,
        Region::Const => 1,
        Region::EhFrame => 2,
        Region::UnwindInfo => 3,
        Region::Got => 4,
        Region::Data => 5,
        Region::Words => 6,
        Region::Inits => 7,
        Region::ThreadVars => 8,
        Region::ThreadData => 9,
        Region::ThreadZero => 10,
        Region::Zero => 11,
    }
}

fn align(at: u64, log2: u8) -> u64 {
    let a = 1u64 << log2;
    (at + a - 1) & !(a - 1)
}

/// What becomes of one section.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Class { Lay(Region), EhFrame, Skip, Dropped }

const NOT_PLACED: u64 = u64::MAX;

#[derive(Clone, Copy)]
struct Sec { class: Class, at: u64 }

/// A reference's target, before any address is known: an ordered key, so GOT cells and stubs
/// are numbered by the inputs, not by the order something met them.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Target {
    Place { object: u32, section: u32, offset: i64 },
    Common { entry: u32, offset: i64 },
    Absolute(u64),
    Rust { word: u32 },
    Import { index: u32, addend: i64 },
}

/// A GOT entry: a pointer cell, an ELF TLS descriptor pair, an ELF initial-exec offset cell.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Cell { Plain(Target), TlsDesc(Target), TlsIe(Target) }

impl Cell {
    fn len(self) -> u64 {
        match self {
            Cell::TlsDesc(_) => 16,
            Cell::Plain(_) | Cell::TlsIe(_) => 8,
        }
    }
}

/// One record of an `__eh_frame`/`.eh_frame` section in the merged region: `out` is its
/// offset in the region (a duplicate CIE's is its kept twin's; a dropped FDE's is `DROPPED`).
#[derive(Clone, Copy)]
struct Piece { object: u32, section: u32, start: u32, len: u32, out: u32, primary: bool, cie: bool }

const DROPPED: u32 = u32::MAX;

/// An FDE's function, where the FDE went, and the length it covers.
#[derive(Clone, Copy)]
struct Fde { function: Target, out: u32, range: u32 }

const STUB_LEN: u64 = 12;
const UNWIND_ARM64_MODE_MASK: u32 = 0x0F00_0000;
const UNWIND_ARM64_MODE_DWARF: u32 = 0x0300_0000;
const DWARF_OFFSET_MASK: u32 = 0x00FF_FFFF;
/// dyld's TLV thunk every Mach-O `__thread_vars` descriptor starts with.
const TLV_BOOTSTRAP: &[u8] = b"__tlv_bootstrap";
/// A Mach-O `__thread_vars` descriptor: thunk, key, offset (`struct TLVDescriptor`).
const TLV_DESCRIPTOR_LEN: u32 = 24;

const X16_ADRP: u32 = 0x9000_0010; // adrp x16, 0
const X16_LDR: u32 = 0xF940_0210; // ldr x16, [x16, #0]
const X16_ADD: u32 = 0x9100_0210; // add x16, x16, #0
const BR_X16: u32 = 0xD61F_0200; // br x16

/// ADRP's page delta into `insn` (A64 "ADRP": `Xd = (PC & ~0xFFF) + (imm21 << 12)`).
fn adrp(insn: u32, place: u64, to: u64) -> Option<u32> {
    let pages = ((to & !0xfff) as i64).wrapping_sub((place & !0xfff) as i64) >> 12;
    if !(-(1i64 << 20)..(1i64 << 20)).contains(&pages) {
        return None;
    }
    let imm = pages as u32;
    Some((insn & 0x9f00_001f) | ((imm & 3) << 29) | (((imm >> 2) & 0x7ffff) << 5))
}

/// The low 12 bits of `to`, scaled by the access size, into imm12 (bits 10..21) of an ADD or
/// LDR/STR (unsigned offset); a misaligned offset cannot be encoded.
fn lo12(insn: u32, to: u64, scale_log2: u8) -> Option<u32> {
    let low = (to & 0xfff) as u32;
    if low & ((1u32 << scale_log2) - 1) != 0 {
        return None;
    }
    Some((insn & !(0xfff << 10)) | ((low >> scale_log2) << 10))
}

/// A branch's word offset into a field of `bits` at bit `shift`.
fn branch(insn: u32, place: u64, to: u64, bits: u32, shift: u32) -> Option<u32> {
    let d = (to as i64).wrapping_sub(place as i64);
    let reach = 1i64 << (bits + 1);
    if d & 3 != 0 || !(-reach..reach).contains(&d) {
        return None;
    }
    let mask = (1u32 << bits) - 1;
    Some((insn & !(mask << shift)) | ((((d >> 2) as u32) & mask) << shift))
}

fn rd32(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn rd64(b: &[u8], at: usize) -> Option<u64> {
    let s = b.get(at..at.checked_add(8)?)?;
    let mut w = [0u8; 8];
    w.copy_from_slice(s);
    Some(u64::from_le_bytes(w))
}

fn unwind_why(e: UnwindInfoError) -> &'static str {
    match e {
        UnwindInfoError::TooManyPersonalities { .. } => "more than three personality functions (UNWIND_PERSONALITY_MASK holds a 2-bit index)",
        UnwindInfoError::Unsorted { .. } => "two unwind rows overlap or start at one address, or an address is past 4 GiB",
        UnwindInfoError::BadFde { .. } => "a record of the merged .eh_frame the .eh_frame_hdr writer cannot read",
    }
}

struct Lay<'p, 'a> {
    plan: &'p Plan<'a>,
    inputs: &'p [Input<'a>],
    platform: Platform,
    alloc: Alloc<'a>,
    first: AVec<'a, u32>,
    secs: AVec<'a, Sec>,
    eh: AVec<'a, u8>,
    pieces: AVec<'a, Piece>,
    /// The kept (first-seen) CIEs, as indices of `pieces`.
    cies: AVec<'a, u32>,
    fdes: AVec<'a, Fde>,
    stubs: AVec<'a, Target>,
    cells: AVec<'a, Cell>,
    cell_at: AVec<'a, u64>,
    veneers: AVec<'a, u64>,
    commons: AVec<'a, (u32, u64)>,
    /// (at, len, align_log2) per region, in `REGIONS` order.
    regions: [(u64, u64, u8); 12],
    stubs_at: u64,
    veneers_at: u64,
    words_at: u64,
    tls_start: u64,
    file_start: u64,
    bytes: AVec<'a, u8>,
    fixups: AVec<'a, Fixup>,
    tls: AVec<'a, TlsReloc>,
}

impl<'p, 'a> Lay<'p, 'a> {
    fn origin(&self, o: u32) -> Origin<'a> {
        self.inputs[self.plan.objects[o as usize].input as usize].from
    }

    fn sec(&self, o: u32, s: u32) -> Option<Sec> {
        let i = *self.first.get(o as usize)? as usize + s as usize;
        let end = self.first.get(o as usize + 1).map_or(self.secs.len(), |&e| e as usize);
        if i < end { Some(self.secs[i]) } else { None }
    }

    fn sec_mut(&mut self, o: u32, s: u32) -> &mut Sec {
        let i = self.first[o as usize] as usize + s as usize;
        &mut self.secs[i]
    }

    /// Each section's class (NATIVE.md W3's region order; `Other` and term functions refused).
    fn classify(&mut self) -> Result<(), LayoutError<'a>> {
        let plan = self.plan;
        for (o, pulled) in plan.objects.iter().enumerate() {
            push(&mut self.first, self.secs.len() as u32)?;
            let obj = &pulled.object;
            for sec in obj.sections.iter() {
                let relocs = &obj.relocs[sec.relocs.start as usize..sec.relocs.end as usize];
                let absolute = relocs.iter().any(|r| r.kind == RelocKind::Abs64);
                let class = match sec.kind {
                    SectionKind::Code => Class::Lay(Region::Text),
                    // Read-only data with an absolute pointer needs a fixup: a writable region
                    // (ld64's `__DATA_CONST`; no relro here).
                    SectionKind::ReadOnly | SectionKind::ExceptTable if absolute => Class::Lay(Region::Data),
                    SectionKind::ReadOnly | SectionKind::ExceptTable | SectionKind::CStrings => Class::Lay(Region::Const),
                    SectionKind::Data => Class::Lay(Region::Data),
                    SectionKind::ZeroFill => Class::Lay(Region::Zero),
                    SectionKind::ThreadData => Class::Lay(Region::ThreadData),
                    SectionKind::ThreadZero => Class::Lay(Region::ThreadZero),
                    SectionKind::ThreadVars => Class::Lay(Region::ThreadVars),
                    SectionKind::InitPointers => Class::Lay(Region::Inits),
                    SectionKind::TermPointers if sec.size == 0 => Class::Skip,
                    SectionKind::TermPointers => return Err(LayoutError::TermFunctions { from: self.origin(o as u32) }),
                    SectionKind::EhFrame => Class::EhFrame,
                    // Its rows are read from `Object::unwind`.
                    SectionKind::CompactUnwind | SectionKind::Skip => Class::Skip,
                    SectionKind::Other => {
                        return Err(LayoutError::Section { from: self.origin(o as u32), segment: sec.segment, name: sec.name });
                    }
                };
                // Initializer pointers are 8-byte cells laid back to back (Image::inits counts
                // them): a size or alignment that breaks that is refused by the section's name.
                if class == Class::Lay(Region::Inits) && (sec.size % 8 != 0 || sec.align_log2 > 3) {
                    return Err(LayoutError::Section { from: self.origin(o as u32), segment: sec.segment, name: sec.name });
                }
                push(&mut self.secs, Sec { class, at: NOT_PLACED })?;
            }
        }
        for &(o, s) in plan.dropped.iter() {
            self.sec_mut(o, s).class = Class::Dropped;
        }
        Ok(())
    }

    fn laid(&self, t: Target) -> bool {
        match t {
            Target::Place { object, section, .. } => {
                matches!(self.sec(object, section), Some(Sec { class: Class::Lay(_) | Class::EhFrame, .. }))
            }
            _ => true,
        }
    }

    fn def_target(o: u32, def: Def, addend: i64) -> Option<Target> {
        match def {
            Def::In { section, offset } => Some(Target::Place { object: o, section, offset: (offset as i64).wrapping_add(addend) }),
            Def::Absolute(v) => Some(Target::Absolute(v.wrapping_add(addend as u64))),
            Def::Undefined | Def::Common { .. } => None,
        }
    }

    /// The target `to + addend` of object `o`, and the name it goes by (for errors); a local
    /// symbol by its own definition, a global one by the plan's resolution of its name.
    fn target_raw(&self, o: u32, to: RelocTo, addend: i64) -> Result<(Target, &'a [u8]), LayoutError<'a>> {
        let plan = self.plan;
        let obj = &plan.objects[o as usize].object;
        let from = self.origin(o);
        match to {
            RelocTo::Section(s) => {
                let name = obj.sections.get(s as usize).map_or(&b""[..], |x| x.name);
                Ok((Target::Place { object: o, section: s, offset: addend }, name))
            }
            RelocTo::Symbol(i) => {
                let Some(sym) = obj.symbols.get(i as usize) else {
                    return Err(LayoutError::Unplaced { name: b"", from });
                };
                let name = sym.name;
                if sym.bind == Bind::Local {
                    return Self::def_target(o, sym.def, addend).map(|t| (t, name)).ok_or(LayoutError::Unplaced { name, from });
                }
                let Ok(k) = plan.names.binary_search_by(|(n, _)| n.cmp(&name)) else {
                    return Err(LayoutError::Unplaced { name, from });
                };
                let t = match plan.names[k].1 {
                    Resolution::Defined { object, symbol } => {
                        let def = plan.objects.get(object as usize).and_then(|p| p.object.symbols.get(symbol as usize)).map(|s| s.def);
                        def.and_then(|d| Self::def_target(object, d, addend)).ok_or(LayoutError::Unplaced { name, from })?
                    }
                    Resolution::Common { .. } => Target::Common { entry: k as u32, offset: addend },
                    Resolution::Rust { word } if addend == 0 => Target::Rust { word },
                    Resolution::Rust { .. } => return Err(LayoutError::RustDirectAddress { name, from }),
                    Resolution::Import { index, .. } => Target::Import { index, addend },
                };
                Ok((t, name))
            }
        }
    }

    /// The pointer rows BNL2 reserves: every 64-bit absolute relocation whose target is a Rust
    /// word (the case `apply` records as a `FixTo::Word` fixup), counted before any address is
    /// known, so the rows sit right after the descriptor; [`LayoutError::PointerRows`] if
    /// relocation finds another number.
    fn count_word_pointers(&self) -> Result<u32, LayoutError<'a>> {
        let mut n: u32 = 0;
        self.each_reloc(|me, o, _s, _index, r| {
            if matches!(r.kind, RelocKind::Abs64) && matches!(me.target_raw(o, r.to, r.addend), Ok((Target::Rust { .. }, _))) {
                n += 1;
            }
            Ok(())
        })?;
        Ok(n)
    }

    /// `target_raw`, refusing a place the layout did not lay.
    fn target(&self, o: u32, to: RelocTo, addend: i64) -> Result<(Target, &'a [u8]), LayoutError<'a>> {
        let (t, name) = self.target_raw(o, to, addend)?;
        if !self.laid(t) {
            return Err(LayoutError::Unplaced { name, from: self.origin(o) });
        }
        Ok((t, name))
    }

    /// A `Delta` whose subtracted symbol is in the same `__eh_frame` section as the place: the
    /// assembler's form of a pc-relative pointer there (`ltmpN` at the section's start, plus
    /// the place's distance from it in the stored addend). As ld64's `CFISection::cfiParse`
    /// and lld's `EhRelocator`, it is read as `target + addend + (place - minus) - P`, which
    /// stays true when the records move. None for any other `Delta`.
    fn eh_delta(&self, o: u32, r: &Reloc) -> Result<Option<(Target, &'a [u8])>, LayoutError<'a>> {
        let minus = match r.kind {
            RelocKind::Delta32 { minus } | RelocKind::Delta64 { minus } => minus,
            _ => return Ok(None),
        };
        let obj = &self.plan.objects[o as usize].object;
        let (section, from) = match minus {
            RelocTo::Section(s) => (s, 0u64),
            RelocTo::Symbol(i) => match obj.symbols.get(i as usize).map(|s| (s.bind, s.def)) {
                Some((Bind::Local, Def::In { section, offset })) => (section, offset),
                _ => return Ok(None),
            },
        };
        if section != r.section || !matches!(self.sec(o, section), Some(Sec { class: Class::EhFrame, .. })) {
            return Ok(None);
        }
        let shift = (r.offset as i64).wrapping_sub(from as i64);
        self.target_raw(o, r.to, r.addend.wrapping_add(shift)).map(Some)
    }

    fn relocs_of(&self, o: u32, s: u32) -> &'p [Reloc] {
        let obj = &self.plan.objects[o as usize].object;
        let sec = &obj.sections[s as usize];
        &obj.relocs[sec.relocs.start as usize..sec.relocs.end as usize]
    }

    /// Two CIEs are one when their bytes are equal and their relocations name the same targets
    /// at the same places. A CIE with a `Delta` (a position-dependent pair) is never shared.
    fn same_cie(&self, kept: Piece, o: u32, bytes: &[u8], relocs: &[Reloc], start: u32) -> Result<bool, LayoutError<'a>> {
        if self.eh.get(kept.out as usize..(kept.out + kept.len) as usize) != Some(bytes) {
            return Ok(false);
        }
        let all = self.relocs_of(kept.object, kept.section);
        let lo = all.partition_point(|r| r.offset < kept.start);
        let hi = all.partition_point(|r| r.offset < kept.start + kept.len);
        let theirs = &all[lo..hi];
        if theirs.len() != relocs.len() {
            return Ok(false);
        }
        for (a, b) in theirs.iter().zip(relocs.iter()) {
            let delta = |k: RelocKind| matches!(k, RelocKind::Delta32 { .. } | RelocKind::Delta64 { .. });
            if delta(a.kind) || delta(b.kind) || a.kind != b.kind || a.offset - kept.start != b.offset - start {
                return Ok(false);
            }
            if self.target_raw(kept.object, a.to, a.addend)?.0 != self.target_raw(o, b.to, b.addend)?.0 {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Merges every laid `__eh_frame`/`.eh_frame` by record: CIEs deduplicated, FDEs whose
    /// function is in a dropped section left out, each FDE's CIE pointer rewritten.
    fn merge_eh(&mut self) -> Result<(), LayoutError<'a>> {
        let plan = self.plan;
        for (o, pulled) in plan.objects.iter().enumerate() {
            let o = o as u32;
            for (s, sec) in pulled.object.sections.iter().enumerate() {
                let s = s as u32;
                if !matches!(self.sec(o, s), Some(Sec { class: Class::EhFrame, .. })) {
                    continue;
                }
                let from = self.origin(o);
                let fail = |why: &'static str| LayoutError::Unwind { from, why };
                let b = sec.bytes;
                let relocs = self.relocs_of(o, s);
                let mut at = 0usize;
                while at < b.len() {
                    let length = rd32(b, at).ok_or(fail("an eh_frame record runs past its section"))?;
                    if length == 0 {
                        if b[at..].iter().any(|&x| x != 0) {
                            return Err(fail("bytes after an eh_frame terminator"));
                        }
                        break;
                    }
                    if length == u32::MAX {
                        return Err(fail("a 64-bit DWARF eh_frame record"));
                    }
                    let end = at + 4 + length as usize;
                    if end > b.len() || length < 4 {
                        return Err(fail("an eh_frame record runs past its section"));
                    }
                    let id = rd32(b, at + 4).ok_or(fail("an eh_frame record runs past its section"))?;
                    let lo = relocs.partition_point(|r| (r.offset as usize) < at);
                    let hi = relocs.partition_point(|r| (r.offset as usize) < end);
                    let mine = &relocs[lo..hi];
                    let record = &b[at..end];
                    let piece = |out: u32, primary: bool, cie: bool| Piece { object: o, section: s, start: at as u32, len: (end - at) as u32, out, primary, cie };
                    if id == 0 {
                        let mut twin = None;
                        let mut k = 0;
                        while k < self.cies.len() {
                            let kept = self.pieces[self.cies[k] as usize];
                            if self.same_cie(kept, o, record, mine, at as u32)? {
                                twin = Some(kept.out);
                                break;
                            }
                            k += 1;
                        }
                        match twin {
                            Some(out) => push(&mut self.pieces, piece(out, false, true))?,
                            None => {
                                let out = self.eh.len() as u32;
                                self.eh.try_extend_from_slice(record).map_err(|_| Full)?;
                                push(&mut self.cies, self.pieces.len() as u32)?;
                                push(&mut self.pieces, piece(out, true, true))?;
                            }
                        }
                    } else {
                        let cie_in = (at + 4).checked_sub(id as usize).ok_or(fail("an FDE's CIE pointer reaches before its section"))?;
                        let k = self.pieces.partition_point(|p| (p.object, p.section, p.start as usize) < (o, s, cie_in));
                        let cie = self
                            .pieces
                            .get(k)
                            .filter(|p| p.object == o && p.section == s && p.start as usize == cie_in && p.cie)
                            .map(|p| p.out)
                            .ok_or(fail("an FDE's CIE pointer names no CIE of its section"))?;
                        let r = mine.iter().find(|r| r.offset as usize == at + 8).ok_or(fail("an FDE's pc_begin has no relocation"))?;
                        let (function, _) = match self.eh_delta(o, r)? {
                            Some(t) => t,
                            None => self.target_raw(o, r.to, r.addend)?,
                        };
                        if !self.laid(function) {
                            push(&mut self.pieces, piece(DROPPED, false, false))?;
                            at = end;
                            continue;
                        }
                        let width = match r.kind {
                            RelocKind::Delta32 { .. } | RelocKind::Rel32 => 4,
                            RelocKind::Delta64 { .. } | RelocKind::Rel64 | RelocKind::Abs64 => 8,
                            _ => return Err(fail("an FDE's pc_begin relocation is not a 32- or 64-bit pointer")),
                        };
                        let range = match width {
                            4 => rd32(b, at + 12).map(u64::from),
                            _ => rd64(b, at + 16),
                        };
                        let range = range.and_then(|v| u32::try_from(v).ok()).ok_or(fail("an FDE's pc_range is past its record or over 4 GiB"))?;
                        let out = self.eh.len() as u32;
                        self.eh.try_extend_from_slice(record).map_err(|_| Full)?;
                        let pointer = (out + 4 - cie).to_le_bytes();
                        self.eh[out as usize + 4..out as usize + 8].copy_from_slice(&pointer);
                        push(&mut self.pieces, piece(out, true, false))?;
                        push(&mut self.fdes, Fde { function, out, range })?;
                    }
                    at = end;
                }
            }
        }
        self.fdes.sort_unstable_by_key(|f| (f.function, f.out));
        Ok(())
    }

    /// The piece of eh section (o, s) holding `offset`, and the offset inside it.
    fn piece(&self, o: u32, s: u32, offset: i64) -> Option<(Piece, u64)> {
        let offset = u32::try_from(offset).ok()?;
        let after = self.pieces.partition_point(|p| (p.object, p.section, p.start) <= (o, s, offset));
        let p = *self.pieces.get(after.checked_sub(1)?)?;
        (p.object == o && p.section == s && offset < p.start + p.len && p.out != DROPPED).then_some((p, u64::from(offset - p.start)))
    }

    /// Every laid relocation, `f(object, section, reloc)`; the relocations of a duplicate CIE
    /// or a dropped FDE are not laid.
    fn each_reloc(&self, mut f: impl FnMut(&Self, u32, u32, u32, &'p Reloc) -> Result<(), LayoutError<'a>>) -> Result<(), LayoutError<'a>> {
        let plan = self.plan;
        for (o, pulled) in plan.objects.iter().enumerate() {
            let o = o as u32;
            for (s, sec) in pulled.object.sections.iter().enumerate() {
                let s = s as u32;
                let class = self.sec(o, s).map(|x| x.class);
                if !matches!(class, Some(Class::Lay(_) | Class::EhFrame)) {
                    continue;
                }
                for index in sec.relocs.clone() {
                    let r = &pulled.object.relocs[index as usize];
                    if class == Some(Class::EhFrame) && !self.piece(o, s, i64::from(r.offset)).is_some_and(|(p, _)| p.primary) {
                        continue;
                    }
                    f(self, o, s, index, r)?;
                }
            }
        }
        Ok(())
    }

    /// The GOT cells and stubs the relocations need (sorted, deduplicated).
    fn collect(&mut self) -> Result<(), LayoutError<'a>> {
        let mut stubs: AVec<'a, Target> = Vec::new_in(self.alloc);
        let mut cells: AVec<'a, Cell> = Vec::new_in(self.alloc);
        self.each_reloc(|me, o, _, _, r| {
            match r.kind {
                RelocKind::Branch26 | RelocKind::CondBranch19 | RelocKind::TestBranch14 => {
                    let (t, _) = me.target(o, r.to, r.addend)?;
                    match t {
                        Target::Rust { .. } => push(&mut stubs, t)?,
                        Target::Import { addend: 0, .. } => {
                            push(&mut stubs, t)?;
                            push(&mut cells, Cell::Plain(t))?;
                        }
                        Target::Import { .. } | Target::Absolute(_) => {
                            return Err(LayoutError::OutOfRange { kind: r.kind, from: me.origin(o), offset: r.offset });
                        }
                        Target::Place { .. } | Target::Common { .. } => {}
                    }
                }
                RelocKind::GotPage21 | RelocKind::GotPageOff12 | RelocKind::GotRel32 | RelocKind::TlvPage21 | RelocKind::TlvPageOff12 => {
                    let (t, _) = me.target(o, r.to, r.addend)?;
                    if !matches!(t, Target::Rust { .. }) {
                        push(&mut cells, Cell::Plain(t))?;
                    }
                }
                RelocKind::TlsDescPage21 | RelocKind::TlsDescLo12 | RelocKind::TlsDescAdd => {
                    let (t, _) = me.target(o, r.to, r.addend)?;
                    push(&mut cells, Cell::TlsDesc(t))?;
                }
                RelocKind::TlsIePage21 | RelocKind::TlsIeLo12 => {
                    let (t, _) = me.target(o, r.to, r.addend)?;
                    push(&mut cells, Cell::TlsIe(t))?;
                }
                RelocKind::TlsLeHi12 | RelocKind::TlsLeLo12 => return Err(LayoutError::TlsLocalExec { from: me.origin(o) }),
                _ => {}
            }
            Ok(())
        })?;
        // A compact unwind row's personality is read through a pointer cell.
        let plan = self.plan;
        for (o, pulled) in plan.objects.iter().enumerate() {
            for row in pulled.object.unwind.iter() {
                if let Some(p) = row.personality {
                    let (t, _) = self.target(o as u32, p, 0)?;
                    if !matches!(t, Target::Rust { .. }) {
                        push(&mut cells, Cell::Plain(t))?;
                    }
                }
            }
        }
        stubs.sort_unstable();
        stubs.dedup();
        cells.sort_unstable();
        cells.dedup();
        self.stubs = stubs;
        self.cells = cells;
        Ok(())
    }

    fn region_align(&self, region: Region) -> u8 {
        let plan = self.plan;
        let mut most = match region {
            Region::Text | Region::UnwindInfo => 2,
            Region::Got | Region::Data | Region::Words | Region::Inits | Region::ThreadVars => 3,
            _ => 0,
        };
        for (o, pulled) in plan.objects.iter().enumerate() {
            for (s, sec) in pulled.object.sections.iter().enumerate() {
                if self.sec(o as u32, s as u32).map(|x| x.class) == Some(Class::Lay(region)) {
                    most = most.max(sec.align_log2);
                }
            }
        }
        if region == Region::Zero {
            for (_, res) in plan.names.iter() {
                if let Resolution::Common { align_log2, .. } = res {
                    most = most.max(*align_log2);
                }
            }
        }
        most
    }

    /// Places region `region`'s sections from `at` (aligned to the region); returns its end.
    fn place(&mut self, region: Region, at: u64) -> u64 {
        let log2 = self.regions[ri(region)].2.max(self.region_align(region));
        let start = align(at, log2);
        let mut at = start;
        let plan = self.plan;
        for (o, pulled) in plan.objects.iter().enumerate() {
            for (s, sec) in pulled.object.sections.iter().enumerate() {
                let x = self.sec_mut(o as u32, s as u32);
                if x.class == Class::Lay(region) {
                    at = align(at, sec.align_log2);
                    x.at = at;
                    at += sec.size;
                }
            }
        }
        self.regions[ri(region)] = (start, at - start, log2);
        at
    }

    fn set_region(&mut self, region: Region, start: u64, len: u64, log2: u8) -> u64 {
        self.regions[ri(region)] = (start, len, log2);
        start + len
    }

    fn region_at(&self, region: Region) -> u64 {
        self.regions[ri(region)].0
    }

    /// The image address of a target that has one (a place or a common).
    fn address(&self, t: Target, o: u32, name: &'a [u8]) -> Result<u64, LayoutError<'a>> {
        let none = LayoutError::Unplaced { name, from: self.origin(o) };
        match t {
            Target::Place { object, section, offset } => {
                let sec = self.sec(object, section).ok_or(none)?;
                match sec.class {
                    Class::EhFrame => {
                        let (p, inside) = self.piece(object, section, offset).ok_or(none)?;
                        Ok(self.region_at(Region::EhFrame) + u64::from(p.out) + inside)
                    }
                    Class::Lay(_) if sec.at != NOT_PLACED => Ok(sec.at.wrapping_add(offset as u64)),
                    _ => Err(none),
                }
            }
            Target::Common { entry, offset } => {
                let k = self.commons.binary_search_by_key(&entry, |c| c.0).map_err(|_| none)?;
                Ok(self.commons[k].1.wrapping_add(offset as u64))
            }
            Target::Absolute(_) | Target::Rust { .. } | Target::Import { .. } => Err(none),
        }
    }

    fn stub(&self, t: Target) -> Option<u64> {
        self.stubs.binary_search(&t).ok().map(|i| self.stubs_at + STUB_LEN * i as u64)
    }

    fn cell(&self, c: Cell) -> Option<u64> {
        self.cells.binary_search(&c).ok().map(|i| self.cell_at[i])
    }

    /// Where a branch to `t` lands before any veneer: a stub for a word or an import, else
    /// the place (code only: a branch into data is refused).
    fn branch_dest(&self, o: u32, r: &Reloc) -> Result<u64, LayoutError<'a>> {
        let (t, name) = self.target(o, r.to, r.addend)?;
        let far = LayoutError::OutOfRange { kind: r.kind, from: self.origin(o), offset: r.offset };
        match t {
            Target::Rust { .. } | Target::Import { .. } => self.stub(t).ok_or(far),
            Target::Place { object, section, .. } if self.sec(object, section).map(|x| x.class) == Some(Class::Lay(Region::Text)) => {
                self.address(t, o, name)
            }
            _ => Err(far),
        }
    }

    fn site(&self, o: u32, s: u32, r: &Reloc) -> Result<u64, LayoutError<'a>> {
        self.address(Target::Place { object: o, section: s, offset: i64::from(r.offset) }, o, b"")
    }

    /// Text: code, stubs, then a veneer per far `Branch26` target.
    fn place_text(&mut self, at: u64) -> Result<u64, LayoutError<'a>> {
        let code_end = self.place(Region::Text, at);
        self.stubs_at = align(code_end, 2);
        self.veneers_at = self.stubs_at + STUB_LEN * self.stubs.len() as u64;
        let mut far: AVec<'a, u64> = Vec::new_in(self.alloc);
        self.each_reloc(|me, o, s, _, r| {
            if r.kind == RelocKind::Branch26 {
                let place = me.site(o, s, r)?;
                let dest = me.branch_dest(o, r)?;
                if branch(0x9400_0000, place, dest, 26, 0).is_none() {
                    push(&mut far, dest)?;
                }
            }
            Ok(())
        })?;
        far.sort_unstable();
        far.dedup();
        let end = self.veneers_at + STUB_LEN * far.len() as u64;
        self.veneers = far;
        let start = self.regions[ri(Region::Text)].0;
        let log2 = self.regions[ri(Region::Text)].2;
        Ok(self.set_region(Region::Text, start, end - start, log2))
    }

    /// The unwind rows of a Mach-O image: each member's compact unwind rows (a DWARF-mode row
    /// given its FDE's offset in the merged `__eh_frame`), and a DWARF-mode row for each FDE no
    /// compact row covers (as ld64 and lld make for assembly with only CFI). With `real`
    /// false, personalities and LSDAs are stand-ins of the same count (the size is all that is
    /// asked before the GOT and data are placed).
    fn rows(&self, real: bool) -> Result<AVec<'a, (Row, u32)>, LayoutError<'a>> {
        let plan = self.plan;
        let mut rows: AVec<'a, (Row, u32)> = Vec::new_in(self.alloc);
        let mut covered: AVec<'a, Target> = Vec::new_in(self.alloc);
        for (o, pulled) in plan.objects.iter().enumerate() {
            let o = o as u32;
            let from = self.origin(o);
            for row in pulled.object.unwind.iter() {
                let function = Target::Place { object: o, section: row.function.section, offset: row.function.offset as i64 };
                let start = self.address(function, o, b"")?;
                push(&mut covered, function)?;
                let mut encoding = row.encoding;
                if encoding & UNWIND_ARM64_MODE_MASK == UNWIND_ARM64_MODE_DWARF {
                    let k = self.fdes.partition_point(|f| f.function < function);
                    let fde = self.fdes.get(k).filter(|f| f.function == function).ok_or(LayoutError::Unwind {
                        from,
                        why: "a DWARF-mode compact unwind row whose function has no FDE",
                    })?;
                    if fde.out > DWARF_OFFSET_MASK {
                        return Err(LayoutError::Unwind { from, why: "an FDE past 16 MiB of __eh_frame (a DWARF-mode row holds 24 bits)" });
                    }
                    encoding = (encoding & !DWARF_OFFSET_MASK) | fde.out;
                }
                let personality = match row.personality {
                    None => None,
                    Some(p) => {
                        let (t, _) = self.target(o, p, 0)?;
                        let no_cell = LayoutError::Unwind { from, why: "a personality with no GOT cell" };
                        // Stand-ins: distinct per cell and under 4 GiB, as the real cells are.
                        let cell = match (t, real) {
                            (Target::Rust { word }, true) => self.words_at + 8 * u64::from(word),
                            (Target::Rust { word }, false) => (1u64 << 31) + u64::from(word),
                            (_, true) => self.cell(Cell::Plain(t)).ok_or(no_cell)?,
                            (_, false) => self.cells.binary_search(&Cell::Plain(t)).map_err(|_| no_cell)? as u64,
                        };
                        Some(cell)
                    }
                };
                let lsda = match row.lsda {
                    None => None,
                    Some(l) if real => Some(self.address(Target::Place { object: o, section: l.section, offset: l.offset as i64 }, o, b"")?),
                    Some(_) => Some(0),
                };
                push(&mut rows, (Row { start, length: row.length, encoding, personality, lsda }, o))?;
            }
        }
        covered.sort_unstable();
        for fde in self.fdes.iter() {
            if covered.binary_search(&fde.function).is_ok() {
                continue;
            }
            let Target::Place { object, .. } = fde.function else {
                return Err(LayoutError::Unwind { from: self.origin(0), why: "an FDE whose pc_begin names no place of the image" });
            };
            let start = self.address(fde.function, object, b"")?;
            if fde.out > DWARF_OFFSET_MASK {
                return Err(LayoutError::Unwind { from: self.origin(object), why: "an FDE past 16 MiB of __eh_frame (a DWARF-mode row holds 24 bits)" });
            }
            let row = Row { start, length: fde.range, encoding: UNWIND_ARM64_MODE_DWARF | fde.out, personality: None, lsda: None };
            push(&mut rows, (row, object))?;
        }
        rows.sort_unstable_by_key(|(r, o)| (r.start, r.length, *o));
        Ok(rows)
    }

    fn unwind_error(&self, rows: &[(Row, u32)], e: UnwindInfoError) -> LayoutError<'a> {
        let at = match e {
            UnwindInfoError::Unsorted { at } | UnwindInfoError::BadFde { at } => at,
            UnwindInfoError::TooManyPersonalities { .. } => 0,
        };
        let o = rows.iter().find(|(r, _)| r.start == at).map_or(0, |(_, o)| *o);
        LayoutError::Unwind { from: self.origin(o), why: unwind_why(e) }
    }

    fn row_list(rows: &[(Row, u32)], alloc: Alloc<'a>) -> Result<AVec<'a, Row>, Full> {
        let mut out: AVec<'a, Row> = Vec::new_in(alloc);
        out.try_reserve_exact(rows.len()).map_err(|_| Full)?;
        for (r, _) in rows {
            push(&mut out, *r)?;
        }
        Ok(out)
    }

    /// The size of the unwind index: `__unwind_info` from stand-in rows on Mach-O,
    /// `.eh_frame_hdr` from the merged records' count on ELF.
    fn unwind_len(&self) -> Result<u64, LayoutError<'a>> {
        match self.platform {
            Platform::MacosArm64 => {
                let rows = self.rows(false)?;
                if rows.is_empty() {
                    return Ok(0);
                }
                let list = Self::row_list(&rows, self.alloc)?;
                let len = unwind::unwind_info_len(&list).map_err(|e| self.unwind_error(&rows, e))?;
                Ok(len as u64)
            }
            Platform::LinuxArm64 => {
                if self.eh.is_empty() {
                    return Ok(0);
                }
                let len = unwind::eh_frame_hdr_len(&self.eh, 0).map_err(|e| self.unwind_error(&[], e))?;
                Ok(len as u64)
            }
        }
    }

    fn put32(&mut self, at: u64, value: u32) {
        let i = (at - self.file_start) as usize;
        self.bytes[i..i + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn put64(&mut self, at: u64, value: u64) {
        let i = (at - self.file_start) as usize;
        self.bytes[i..i + 8].copy_from_slice(&value.to_le_bytes());
    }

    fn get32(&self, at: u64) -> u32 {
        let i = (at - self.file_start) as usize;
        u32::from_le_bytes([self.bytes[i], self.bytes[i + 1], self.bytes[i + 2], self.bytes[i + 3]])
    }

    /// Writes one 12-byte `adrp x16; <second> x16; br x16` at `at` reaching `to`.
    fn write_x16(&mut self, at: u64, second: u32, to: u64, scale: u8) -> Option<()> {
        let a = adrp(X16_ADRP, at, to)?;
        let b = lo12(second, to, scale)?;
        self.put32(at, a);
        self.put32(at + 4, b);
        self.put32(at + 8, BR_X16);
        Some(())
    }

    /// Applies relocation `r` of section (o, s) to the bytes, or records its fixup.
    fn apply(&mut self, o: u32, s: u32, r: &Reloc) -> Result<(), LayoutError<'a>> {
        let from = self.origin(o);
        let far = LayoutError::OutOfRange { kind: r.kind, from, offset: r.offset };
        let place = self.site(o, s, r)?;
        let sec = self.sec(o, s).ok_or(far)?;
        let plan = self.plan;
        let obj = &plan.objects[o as usize].object;
        let section = &obj.sections[s as usize];
        let refuse_section = LayoutError::Section { from, segment: section.segment, name: section.name };
        if matches!(sec.class, Class::Lay(Region::ThreadZero | Region::Zero)) {
            return Err(refuse_section);
        }
        let insn = |me: &Self| me.get32(place);
        match r.kind {
            RelocKind::Abs64 => {
                if !matches!(sec.class, Class::Lay(Region::Data | Region::Inits | Region::ThreadVars | Region::ThreadData)) {
                    return Err(refuse_section);
                }
                let (t, name) = self.target(o, r.to, r.addend)?;
                if section.kind == SectionKind::ThreadVars && r.offset % TLV_DESCRIPTOR_LEN == 16 {
                    // The descriptor's offset word: the variable's offset in the TLV template.
                    let v = self.address(t, o, name)?.wrapping_sub(self.tls_start);
                    self.put64(place, v);
                    return Ok(());
                }
                if section.kind == SectionKind::ThreadVars && r.offset % TLV_DESCRIPTOR_LEN == 0 {
                    let bootstrap = match t {
                        Target::Import { index, addend: 0 } => plan.imports.get(index as usize).is_some_and(|n| *n == TLV_BOOTSTRAP),
                        _ => false,
                    };
                    if !bootstrap {
                        return Err(refuse_section);
                    }
                }
                match t {
                    Target::Place { .. } | Target::Common { .. } => {
                        let v = self.address(t, o, name)?;
                        self.put64(place, v);
                        push(&mut self.fixups, Fixup { at: place, to: FixTo::Rebase { target: v } })?;
                    }
                    Target::Absolute(v) => self.put64(place, v),
                    Target::Import { index, addend } => {
                        self.put64(place, 0);
                        push(&mut self.fixups, Fixup { at: place, to: FixTo::Import { index, addend } })?;
                    }
                    Target::Rust { word } => {
                        self.put64(place, 0);
                        push(&mut self.fixups, Fixup { at: place, to: FixTo::Word { index: word } })?;
                    }
                }
            }
            // A 32-bit absolute address cannot hold a position-independent image's addresses.
            RelocKind::Abs32 => return Err(far),
            RelocKind::Rel32 | RelocKind::Rel64 | RelocKind::Delta32 { .. } | RelocKind::Delta64 { .. } => {
                let v = match self.eh_delta(o, r)? {
                    Some((t, name)) => {
                        if !self.laid(t) {
                            return Err(LayoutError::Unplaced { name, from });
                        }
                        self.address(t, o, name)?.wrapping_sub(place)
                    }
                    None => {
                        let (t, name) = self.target(o, r.to, r.addend)?;
                        if let Target::Rust { .. } = t {
                            return Err(LayoutError::RustDirectAddress { name, from });
                        }
                        let to = self.address(t, o, name).map_err(|_| far)?;
                        let minus = match r.kind {
                            RelocKind::Delta32 { minus } | RelocKind::Delta64 { minus } => {
                                let (m, mname) = self.target(o, minus, 0)?;
                                self.address(m, o, mname).map_err(|_| far)?
                            }
                            _ => place,
                        };
                        to.wrapping_sub(minus)
                    }
                };
                match r.kind {
                    RelocKind::Rel32 | RelocKind::Delta32 { .. } => {
                        let v = i32::try_from(v as i64).map_err(|_| far)?;
                        self.put32(place, v as u32);
                    }
                    _ => self.put64(place, v),
                }
            }
            RelocKind::Branch26 | RelocKind::CondBranch19 | RelocKind::TestBranch14 => {
                let dest = self.branch_dest(o, r)?;
                let (bits, shift) = match r.kind {
                    RelocKind::Branch26 => (26, 0),
                    RelocKind::CondBranch19 => (19, 5),
                    _ => (14, 5),
                };
                let i = insn(self);
                let patched = match branch(i, place, dest, bits, shift) {
                    Some(p) => p,
                    None if r.kind == RelocKind::Branch26 => {
                        let k = self.veneers.binary_search(&dest).map_err(|_| far)?;
                        let veneer = self.veneers_at + STUB_LEN * k as u64;
                        branch(i, place, veneer, bits, shift).ok_or(far)?
                    }
                    None => return Err(far),
                };
                self.put32(place, patched);
            }
            RelocKind::Page21 | RelocKind::PageOff12 { .. } => {
                let (t, name) = self.target(o, r.to, r.addend)?;
                if let Target::Rust { .. } = t {
                    return Err(LayoutError::RustDirectAddress { name, from });
                }
                // An import's or an absolute symbol's address is not a place of this image.
                let to = self.address(t, o, name).map_err(|_| far)?;
                let i = insn(self);
                let patched = match r.kind {
                    RelocKind::PageOff12 { scale_log2 } => lo12(i, to, scale_log2),
                    _ => adrp(i, place, to),
                };
                self.put32(place, patched.ok_or(far)?);
            }
            RelocKind::GotPage21 | RelocKind::GotPageOff12 | RelocKind::GotRel32 | RelocKind::TlvPage21 | RelocKind::TlvPageOff12 => {
                let (t, _) = self.target(o, r.to, r.addend)?;
                let cell = match t {
                    Target::Rust { word } => self.words_at + 8 * u64::from(word),
                    _ => self.cell(Cell::Plain(t)).ok_or(far)?,
                };
                let i = insn(self);
                match r.kind {
                    RelocKind::GotPage21 | RelocKind::TlvPage21 => self.put32(place, adrp(i, place, cell).ok_or(far)?),
                    RelocKind::GotPageOff12 | RelocKind::TlvPageOff12 => self.put32(place, lo12(i, cell, 3).ok_or(far)?),
                    _ => {
                        let v = i32::try_from(cell.wrapping_sub(place) as i64).map_err(|_| far)?;
                        self.put32(place, v as u32);
                    }
                }
            }
            RelocKind::TlsDescPage21 | RelocKind::TlsDescLo12 | RelocKind::TlsDescAdd => {
                let (t, _) = self.target(o, r.to, r.addend)?;
                let pair = self.cell(Cell::TlsDesc(t)).ok_or(far)?;
                let i = insn(self);
                let patched = match r.kind {
                    RelocKind::TlsDescPage21 => adrp(i, place, pair),
                    RelocKind::TlsDescLo12 => lo12(i, pair, 3),
                    _ => lo12(i, pair, 0),
                };
                self.put32(place, patched.ok_or(far)?);
            }
            // The `blr` of a TLS descriptor sequence: nothing to patch (no relaxation).
            RelocKind::TlsDescCall => {}
            RelocKind::TlsIePage21 | RelocKind::TlsIeLo12 => {
                let (t, _) = self.target(o, r.to, r.addend)?;
                let cell = self.cell(Cell::TlsIe(t)).ok_or(far)?;
                let i = insn(self);
                let patched = match r.kind {
                    RelocKind::TlsIePage21 => adrp(i, place, cell),
                    _ => lo12(i, cell, 3),
                };
                self.put32(place, patched.ok_or(far)?);
            }
            RelocKind::TlsLeHi12 | RelocKind::TlsLeLo12 => return Err(LayoutError::TlsLocalExec { from }),
        }
        Ok(())
    }

    /// Fills the GOT: a cell's value or fixup, an ELF TLS pair's or cell's dynamic relocation.
    fn fill_got(&mut self) -> Result<(), LayoutError<'a>> {
        let mut k = 0;
        while k < self.cells.len() {
            let (c, at) = (self.cells[k], self.cell_at[k]);
            k += 1;
            match c {
                Cell::Plain(t) => match t {
                    Target::Place { object, .. } => {
                        let v = self.address(t, object, b"")?;
                        self.put64(at, v);
                        push(&mut self.fixups, Fixup { at, to: FixTo::Rebase { target: v } })?;
                    }
                    Target::Common { .. } => {
                        let v = self.address(t, 0, b"")?;
                        self.put64(at, v);
                        push(&mut self.fixups, Fixup { at, to: FixTo::Rebase { target: v } })?;
                    }
                    Target::Absolute(v) => self.put64(at, v),
                    Target::Import { index, addend } => push(&mut self.fixups, Fixup { at, to: FixTo::Import { index, addend } })?,
                    // Rust names use their word as the cell; `collect` never makes one.
                    Target::Rust { .. } => {}
                },
                Cell::TlsDesc(t) | Cell::TlsIe(t) => {
                    let Target::Place { object, .. } = t else {
                        // A thread-local another library defines (or a Rust or absolute name):
                        // `TlsReloc` holds an offset in this image's template, no symbol.
                        let kind = if let Cell::TlsDesc(_) = c { RelocKind::TlsDescPage21 } else { RelocKind::TlsIePage21 };
                        return Err(LayoutError::OutOfRange { kind, from: self.origin(0), offset: 0 });
                    };
                    let offset = self.address(t, object, b"")?.wrapping_sub(self.tls_start);
                    let tls = match c {
                        Cell::TlsDesc(_) => TlsReloc::Desc { got_at: at, offset },
                        _ => TlsReloc::TpRel { got_at: at, offset },
                    };
                    push(&mut self.tls, tls)?;
                }
            }
        }
        Ok(())
    }
}

pub fn lay<'a>(plan: &Plan<'a>, inputs: &[Input<'a>], layout: Layout, alloc: Alloc<'a>) -> Result<Image<'a>, LayoutError<'a>> {
    let platform = layout.platform().ok_or(LayoutError::UnknownLayout { page: layout.page, header_reserve: layout.header_reserve })?;
    let mut me = Lay {
        plan,
        inputs,
        platform,
        alloc,
        first: Vec::new_in(alloc),
        secs: Vec::new_in(alloc),
        eh: Vec::new_in(alloc),
        pieces: Vec::new_in(alloc),
        cies: Vec::new_in(alloc),
        fdes: Vec::new_in(alloc),
        stubs: Vec::new_in(alloc),
        cells: Vec::new_in(alloc),
        cell_at: Vec::new_in(alloc),
        veneers: Vec::new_in(alloc),
        commons: Vec::new_in(alloc),
        regions: [(0, 0, 0); 12],
        stubs_at: 0,
        veneers_at: 0,
        words_at: 0,
        tls_start: 0,
        file_start: layout.header_reserve,
        bytes: Vec::new_in(alloc),
        fixups: Vec::new_in(alloc),
        tls: Vec::new_in(alloc),
    };
    me.classify()?;
    me.merge_eh()?;
    me.collect()?;

    // The read-only segment.
    let mut at = me.place_text(layout.header_reserve)?;
    at = me.place(Region::Const, at);
    let eh_at = align(at, 3);
    at = me.set_region(Region::EhFrame, eh_at, me.eh.len() as u64, 3);
    // The unwind index's size needs only the code's addresses and the counts; its contents
    // are written once the GOT and the data are placed.
    let unwind_len = me.unwind_len()?;
    at = me.set_region(Region::UnwindInfo, align(at, 2), unwind_len, 2);

    // The writable segment, on a new page.
    let got_at = at.next_multiple_of(layout.page);
    let mut cell = got_at;
    let mut k = 0;
    while k < me.cells.len() {
        let c = me.cells[k];
        let a = if let Cell::TlsDesc(_) = c { 4 } else { 3 };
        cell = align(cell, a);
        push(&mut me.cell_at, cell)?;
        cell += c.len();
        k += 1;
    }
    let got_log2 = if me.cells.iter().any(|c| matches!(c, Cell::TlsDesc(_))) { 4 } else { 3 };
    at = me.set_region(Region::Got, got_at, cell - got_at, got_log2);
    // Data: on a new page after a non-empty Got (so a writer maps the Got alone, read-only once
    // bound: W4's `__DATA_CONST`, W5's `PT_GNU_RELRO`), the descriptor first, then the members'
    // data. An empty Got leaves `at` on its page already.
    let data_log2 = me.region_align(Region::Data);
    let descriptor_at = align(at.next_multiple_of(layout.page), data_log2.max(3));
    me.regions[ri(Region::Data)].2 = data_log2.max(3);
    // BNL2 (descriptor.rs): the descriptor, its names table (word order: rows, then the
    // bytes), its pointer rows (8-aligned), then the members' data.
    let mut names_len = NAME_ROW_LEN * plan.words.len() as u64;
    for w in plan.words.iter() {
        names_len += w.len() as u64;
    }
    let pointers = me.count_word_pointers()?;
    let name_at = descriptor_at + DESCRIPTOR_LEN;
    let pointer_at = align(name_at + names_len, 3);
    at = me.place(Region::Data, pointer_at + POINTER_ROW_LEN * u64::from(pointers));
    let data_end = at;
    me.regions[ri(Region::Data)].0 = descriptor_at;
    me.regions[ri(Region::Data)].1 = data_end - descriptor_at;
    me.words_at = align(at, 3);
    at = me.set_region(Region::Words, me.words_at, 8 * plan.words.len() as u64, 3);
    at = me.place(Region::Inits, at);
    let inits_at = me.region_at(Region::Inits);
    let inits = me.regions[ri(Region::Inits)].1 / 8;
    at = me.place(Region::ThreadVars, at);
    // The TLV template is ThreadData then ThreadZero: aligned to the larger of the two.
    let tls_log2 = me.region_align(Region::ThreadData).max(me.region_align(Region::ThreadZero));
    me.regions[ri(Region::ThreadData)].2 = tls_log2;
    at = me.place(Region::ThreadData, at);
    let init_end = at;
    at = me.place(Region::ThreadZero, at);
    me.tls_start = if me.regions[ri(Region::ThreadData)].1 > 0 { me.region_at(Region::ThreadData) } else { me.region_at(Region::ThreadZero) };
    at = me.place(Region::Zero, at);
    let zero_start = me.region_at(Region::Zero);
    // The winning commons, in name order, after the zero-fill sections.
    for (entry, (_, res)) in plan.names.iter().enumerate() {
        if let Resolution::Common { size, align_log2, .. } = *res {
            at = align(at, align_log2);
            push(&mut me.commons, (entry as u32, at))?;
            at += size;
        }
    }
    let zero_log2 = me.regions[ri(Region::Zero)].2;
    let end = me.set_region(Region::Zero, zero_start, at - zero_start, zero_log2);

    // The file bytes, from the end of the header to the end of initialized data.
    let file_len = (init_end - layout.header_reserve) as usize;
    me.bytes.try_reserve_exact(file_len).map_err(|_| Full)?;
    me.bytes.resize(file_len, 0);
    for (o, pulled) in plan.objects.iter().enumerate() {
        for (s, sec) in pulled.object.sections.iter().enumerate() {
            // Every section has a row (classify made one per section).
            let Some(x) = me.sec(o as u32, s as u32) else { continue };
            if let Class::Lay(region) = x.class
                && !matches!(region, Region::ThreadZero | Region::Zero)
            {
                let i = (x.at - layout.header_reserve) as usize;
                let n = sec.bytes.len().min(sec.size as usize);
                me.bytes[i..i + n].copy_from_slice(&sec.bytes[..n]);
            }
        }
    }
    let i = (eh_at - layout.header_reserve) as usize;
    let n = me.eh.len();
    let (bytes, eh) = (&mut me.bytes, &me.eh);
    bytes[i..i + n].copy_from_slice(eh);
    // Stubs: a word's stub loads its word; an import's loads its GOT cell.
    let mut k = 0;
    while k < me.stubs.len() {
        let t = me.stubs[k];
        let stub_at = me.stubs_at + STUB_LEN * k as u64;
        k += 1;
        let cell = match t {
            Target::Rust { word } => me.words_at + 8 * u64::from(word),
            // `collect` made a cell for every import a stub loads.
            _ => me.cell(Cell::Plain(t)).ok_or(LayoutError::Unplaced { name: b"", from: me.origin(0) })?,
        };
        me.write_x16(stub_at, X16_LDR, cell, 3).ok_or(LayoutError::OutOfRange { kind: RelocKind::Page21, from: me.origin(0), offset: 0 })?;
    }
    let mut k = 0;
    while k < me.veneers.len() {
        let dest = me.veneers[k];
        let veneer_at = me.veneers_at + STUB_LEN * k as u64;
        k += 1;
        me.write_x16(veneer_at, X16_ADD, dest, 0).ok_or(LayoutError::OutOfRange { kind: RelocKind::Page21, from: me.origin(0), offset: 0 })?;
    }
    // The descriptor (BNL2, descriptor.rs): image-relative numbers, so it needs no fixup.
    let descriptor = Descriptor {
        magic: DESCRIPTOR_MAGIC,
        words: plan.words.len() as u32,
        inits: inits as u32,
        pointers,
        word_at: me.words_at,
        init_at: inits_at,
        pointer_at,
        name_at,
    };
    let d = descriptor.to_bytes();
    let i = (descriptor_at - layout.header_reserve) as usize;
    me.bytes[i..i + d.len()].copy_from_slice(&d);
    // The names table: row k is (offset from `name_at`, length) of word k's name.
    let mut text = NAME_ROW_LEN * plan.words.len() as u64;
    for (k, w) in plan.words.iter().enumerate() {
        let row = name_at + NAME_ROW_LEN * k as u64;
        me.put32(row, text as u32);
        me.put32(row + 4, w.len() as u32);
        let j = (name_at + text - layout.header_reserve) as usize;
        me.bytes[j..j + w.len()].copy_from_slice(w);
        text += w.len() as u64;
    }

    // Every relocation, then the GOT.
    let mut sites: AVec<'a, (u32, u32, u32)> = Vec::new_in(alloc);
    me.each_reloc(|_, o, s, index, _| push(&mut sites, (o, s, index)).map_err(LayoutError::from))?;
    for &(o, s, index) in sites.iter() {
        let r = plan.objects[o as usize].object.relocs[index as usize];
        me.apply(o, s, &r)?;
    }
    me.fill_got()?;
    // BNL2's pointer rows: each `FixTo::Word` cell (C data holding a Rust function's address),
    // sorted by cell; the launcher writes each with its word's slot thunk (one address per
    // function).
    let mut rows: AVec<'a, (u64, u32)> = Vec::new_in(alloc);
    for f in me.fixups.iter() {
        if let FixTo::Word { index } = f.to {
            push(&mut rows, (f.at, index))?;
        }
    }
    if rows.len() as u64 != u64::from(pointers) {
        return Err(LayoutError::PointerRows { counted: pointers, found: rows.len() as u32 });
    }
    rows.sort_unstable();
    for (k, &(cell, word)) in rows.iter().enumerate() {
        let row = pointer_at + POINTER_ROW_LEN * k as u64;
        me.put64(row, cell);
        me.put32(row + 8, word);
        me.put32(row + 12, 0);
    }

    // The unwind index, now that every address is known.
    let unwind_at = me.region_at(Region::UnwindInfo);
    if unwind_len > 0 {
        let mut out: AVec<'a, u8> = Vec::new_in(alloc);
        out.try_reserve_exact(unwind_len as usize).map_err(|_| Full)?;
        match platform {
            Platform::MacosArm64 => {
                let rows = me.rows(true)?;
                let list = Lay::row_list(&rows, alloc)?;
                unwind::unwind_info(&list, &mut out).map_err(|e| me.unwind_error(&rows, e))?;
            }
            Platform::LinuxArm64 => {
                let i = (eh_at - layout.header_reserve) as usize;
                let eh = &me.bytes[i..i + me.eh.len()];
                unwind::eh_frame_hdr(eh, eh_at, unwind_at, &mut out).map_err(|e| me.unwind_error(&[], e))?;
            }
        }
        if out.len() as u64 != unwind_len {
            return Err(LayoutError::Unwind { from: me.origin(0), why: "the unwind index changed size between its measure and its write" });
        }
        let i = (unwind_at - layout.header_reserve) as usize;
        me.bytes[i..i + out.len()].copy_from_slice(&out);
    }

    // What the writers bind and export.
    let mut imports: AVec<'a, Import<'a>> = Vec::new_in(alloc);
    for name in plan.imports.iter() {
        push(&mut imports, Import { name: *name, weak: false })?;
    }
    for (_, res) in plan.names.iter() {
        if let Resolution::Import { index, weak } = *res
            && let Some(import) = imports.get_mut(index as usize)
        {
            import.weak = weak;
        }
    }
    let mut exports: AVec<'a, (&'a [u8], u64)> = Vec::new_in(alloc);
    for (entry, (name, res)) in plan.names.iter().enumerate() {
        let t = match *res {
            Resolution::Defined { object, symbol } => {
                let def = plan.objects[object as usize].object.symbols[symbol as usize].def;
                match Lay::def_target(object, def, 0) {
                    Some(t @ Target::Place { .. }) => t,
                    _ => continue,
                }
            }
            Resolution::Common { .. } => Target::Common { entry: entry as u32, offset: 0 },
            Resolution::Rust { .. } | Resolution::Import { .. } => continue,
        };
        let o = if let Target::Place { object, .. } = t { object } else { 0 };
        let at = me.address(t, o, *name)?;
        push(&mut exports, (*name, at))?;
    }
    push(&mut exports, (descriptor_name(platform), descriptor_at))?;
    exports.sort_unstable_by(|a, b| a.0.cmp(b.0));
    let mut words: AVec<'a, &'a [u8]> = Vec::new_in(alloc);
    for w in plan.words.iter() {
        push(&mut words, *w)?;
    }
    let mut libraries = Vec::new_in(alloc);
    for l in plan.libraries.iter() {
        push(&mut libraries, *l)?;
    }
    let mut regions: AVec<'a, Placed> = Vec::new_in(alloc);
    for region in REGIONS {
        let (at, len, align_log2) = me.regions[ri(region)];
        push(&mut regions, Placed { region, at, len, align_log2 })?;
    }
    me.fixups.sort_unstable_by_key(|f| f.at);
    me.tls.sort_unstable_by_key(|t| match *t {
        TlsReloc::Desc { got_at, .. } | TlsReloc::TpRel { got_at, .. } => got_at,
    });
    Ok(Image {
        platform,
        regions,
        bytes: me.bytes,
        file_start: layout.header_reserve,
        end,
        fixups: me.fixups,
        tls: me.tls,
        imports,
        exports,
        words,
        inits: inits as u32,
        descriptor_at,
        libraries,
    })
}
