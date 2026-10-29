//! The image of a patch onto an empty layout: what is reached is placed, every relocation is
//! applied, and the headers the loader reads are written around it.
//!
//! The steps, each of which a later patch onto a kept layout does again for what changed:
//!
//! 1. What is reached from `_start`, the start-up tables and what must be kept, and with it
//!    the frames of `.eh_frame` of what is reached (a frame keeps its LSDA and personality).
//! 2. Every relocation of what is reached is read once to know the slots, stubs, imports and
//!    load-time relocations the image needs, before anything has an address.
//! 3. Each piece is placed by [`Layout::place`] in its output section, and the output sections
//!    in four segments: read-only, code, read-only after relocation (with the thread-locals),
//!    and writable.
//! 4. The pieces are copied in and their relocations applied, with the relocation code of the
//!    splice ([`Field::fill`], [`Machine::stub_through`], and the x86-64 thread-local rewrite).

use anyhow::{Context, bail, ensure};
use object::elf;
use rustc_hash::{FxHashMap, FxHashSet};
use sha2::Digest;

use super::{Def, Layout, Link, Reloc, Request, To};
use crate::{Field, Fixup, Machine, UnwindTable, relative};

pub(super) struct Written {
    pub(super) bytes: Vec<u8>,
    pub(super) placed: usize,
    pub(super) imports: usize,
}

/// What a relocation asks for, by machine and type.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    None,
    /// The address, in 8 bytes, which the loader relocates.
    Abs64,
    /// Where the target is from the place.
    Rel(Field),
    /// As [`Kind::Rel`], to a stub when the target is imported.
    Call(Field),
    /// Where the slot holding the target's address is.
    Got(Field),
    /// Where the slot holding the target's offset from the thread pointer is.
    GotTp(Field),
    /// The offset from the thread pointer, in 4 bytes (x86-64).
    Tp32,
    /// Of an AArch64 `add`: the high or low 12 bits of the offset from the thread pointer.
    TpHi12,
    TpLo12,
    /// x86-64's general-dynamic sequence, rewritten to add to the thread pointer.
    TlsGdX86,
    /// AArch64's general-dynamic pair of slots, for `__tls_get_addr`.
    TlsGdPair(Field),
    /// AArch64's TLS descriptor sequence, rewritten to its offset from the thread pointer:
    /// which instruction of the four.
    TlsDesc(u8),
}

fn kind(machine: Machine, r_type: u32) -> Option<Kind> {
    Some(match machine {
        Machine::X86_64 => match r_type {
            elf::R_X86_64_NONE => Kind::None,
            elf::R_X86_64_64 => Kind::Abs64,
            elf::R_X86_64_PC32 => Kind::Rel(Field::Distance),
            elf::R_X86_64_PC64 => Kind::Rel(Field::FarDistance),
            elf::R_X86_64_PLT32 => Kind::Call(Field::Distance),
            elf::R_X86_64_GOTPCREL | elf::R_X86_64_GOTPCRELX | elf::R_X86_64_REX_GOTPCRELX => {
                Kind::Got(Field::Distance)
            }
            elf::R_X86_64_GOTTPOFF => Kind::GotTp(Field::Distance),
            elf::R_X86_64_TPOFF32 => Kind::Tp32,
            elf::R_X86_64_TLSGD => Kind::TlsGdX86,
            _ => return None,
        },
        Machine::Aarch64 => match r_type {
            elf::R_AARCH64_NONE => Kind::None,
            elf::R_AARCH64_ABS64 => Kind::Abs64,
            elf::R_AARCH64_PREL32 => Kind::Rel(Field::Distance),
            elf::R_AARCH64_PREL64 => Kind::Rel(Field::FarDistance),
            elf::R_AARCH64_CALL26 | elf::R_AARCH64_JUMP26 => Kind::Call(Field::Branch),
            elf::R_AARCH64_ADR_PREL_PG_HI21 | elf::R_AARCH64_ADR_PREL_PG_HI21_NC => {
                Kind::Rel(Field::Page)
            }
            elf::R_AARCH64_ADD_ABS_LO12_NC | elf::R_AARCH64_LDST8_ABS_LO12_NC => {
                Kind::Rel(Field::InPage(0))
            }
            elf::R_AARCH64_LDST16_ABS_LO12_NC => Kind::Rel(Field::InPage(1)),
            elf::R_AARCH64_LDST32_ABS_LO12_NC => Kind::Rel(Field::InPage(2)),
            elf::R_AARCH64_LDST64_ABS_LO12_NC => Kind::Rel(Field::InPage(3)),
            elf::R_AARCH64_LDST128_ABS_LO12_NC => Kind::Rel(Field::InPage(4)),
            elf::R_AARCH64_ADR_GOT_PAGE => Kind::Got(Field::Page),
            elf::R_AARCH64_LD64_GOT_LO12_NC => Kind::Got(Field::InPage(3)),
            elf::R_AARCH64_TLSIE_ADR_GOTTPREL_PAGE21 => Kind::GotTp(Field::Page),
            elf::R_AARCH64_TLSIE_LD64_GOTTPREL_LO12_NC => Kind::GotTp(Field::InPage(3)),
            elf::R_AARCH64_TLSLE_ADD_TPREL_HI12 => Kind::TpHi12,
            elf::R_AARCH64_TLSLE_ADD_TPREL_LO12 | elf::R_AARCH64_TLSLE_ADD_TPREL_LO12_NC => {
                Kind::TpLo12
            }
            elf::R_AARCH64_TLSGD_ADR_PAGE21 => Kind::TlsGdPair(Field::Page),
            elf::R_AARCH64_TLSGD_ADD_LO12_NC => Kind::TlsGdPair(Field::InPage(0)),
            elf::R_AARCH64_TLSDESC_ADR_PAGE21 => Kind::TlsDesc(0),
            elf::R_AARCH64_TLSDESC_LD64_LO12 => Kind::TlsDesc(1),
            elf::R_AARCH64_TLSDESC_ADD_LO12 => Kind::TlsDesc(2),
            elf::R_AARCH64_TLSDESC_CALL => Kind::TlsDesc(3),
            _ => return None,
        },
    })
}

fn type_name(machine: Machine, r_type: u32) -> String {
    let names: &[(u32, &str)] = match machine {
        Machine::X86_64 => &[
            (elf::R_X86_64_32, "R_X86_64_32"),
            (elf::R_X86_64_32S, "R_X86_64_32S"),
            (elf::R_X86_64_GOTPC32, "R_X86_64_GOTPC32"),
            (elf::R_X86_64_GOTPC32_TLSDESC, "R_X86_64_GOTPC32_TLSDESC"),
            (elf::R_X86_64_TLSDESC_CALL, "R_X86_64_TLSDESC_CALL"),
            (elf::R_X86_64_TLSLD, "R_X86_64_TLSLD"),
            (elf::R_X86_64_DTPOFF32, "R_X86_64_DTPOFF32"),
            (elf::R_X86_64_DTPOFF64, "R_X86_64_DTPOFF64"),
            (elf::R_X86_64_SIZE32, "R_X86_64_SIZE32"),
            (elf::R_X86_64_SIZE64, "R_X86_64_SIZE64"),
        ],
        Machine::Aarch64 => &[
            (elf::R_AARCH64_ADR_PREL_LO21, "R_AARCH64_ADR_PREL_LO21"),
            (elf::R_AARCH64_MOVW_UABS_G0, "R_AARCH64_MOVW_UABS_G0"),
            (elf::R_AARCH64_CONDBR19, "R_AARCH64_CONDBR19"),
            (elf::R_AARCH64_TSTBR14, "R_AARCH64_TSTBR14"),
            (elf::R_AARCH64_LD_PREL_LO19, "R_AARCH64_LD_PREL_LO19"),
        ],
    };
    match names.iter().find(|it| it.0 == r_type) {
        Some((_, name)) => (*name).to_owned(),
        None => format!("of type {r_type}"),
    }
}

/// What a relocation's target is once names are resolved, before addresses.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Res {
    /// In the image: a piece of an object, or a place the image defines (a global).
    Sec { object: u32, section: u32, offset: u64 },
    Synthetic(u32),
    Abs(u64),
    /// From a shared library, or a weak reference nothing defines: through the loader.
    Import(u32),
}

/// A slot of `.got`.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Slot {
    Address(Res),
    TpOffset(Res),
    /// The pair `__tls_get_addr` takes: the module, then the offset in its block.
    Module(Res),
    Offset(Res),
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Segment {
    Read,
    Code,
    Relro,
    Data,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Synth {
    Interp,
    BuildId,
    Dynsym,
    Version,
    VersionNeeded,
    Hash,
    Dynstr,
    RelaDyn,
    EhFrameHdr,
    EhFrame,
    Plt,
    Dynamic,
    Got,
}

/// An output section.
struct Out {
    name: String,
    segment: Segment,
    rank: u32,
    sh_type: u32,
    flags: u64,
    align: u64,
    size: u64,
    address: u64,
    /// Thread-local data that takes no room in its segment (`.tbss`).
    tls_nobits: bool,
    synth: Option<Synth>,
    /// The pieces in it, in order: (object, section).
    pieces: Vec<(u32, u32)>,
}

/// A record of an input's `.eh_frame`.
struct Frame {
    object: u32,
    section: u32,
    start: u64,
    len: u64,
    /// The common entry it refers to, by its index in the frames; `None` for one that is.
    cie: Option<usize>,
    kept: bool,
    /// Where it is in the image's `.eh_frame`.
    at: u64,
}

struct Plan<'l, 'a> {
    link: &'l mut Link<'a>,
    outs: Vec<Out>,
    frames: Vec<Frame>,
    slots: Vec<Slot>,
    slot_of: FxHashMap<Slot, u32>,
    stubs: Vec<u32>,
    stub_of: FxHashMap<u32, u32>,
    imports: Vec<u32>,
    import_of: FxHashMap<u32, u32>,
    load_relocations: usize,
    /// The relocations the scan counted a load-time relocation for, by where they are.
    load_from: FxHashSet<(u32, u32, u64)>,
    needed: Vec<u32>,
    /// (library, its version) to the index `.gnu.version` gives it.
    version_of: FxHashMap<(u32, u32), u16>,
    dynstr: Vec<u8>,
    dynstr_of: FxHashMap<String, u32>,
    started: FxHashSet<String>,
    tls: Option<(u64, u64, u64)>,
}

const PHDR: u64 = 56;
const EHDR: u64 = 64;
const BUILD_ID: usize = 16;

pub(super) fn write(
    link: &mut Link<'_>,
    layout: &mut Layout,
    request: &Request,
    entry: &str,
) -> anyhow::Result<Written> {
    let machine = link.machine;
    let mut plan = Plan {
        link,
        outs: Vec::new(),
        frames: Vec::new(),
        slots: Vec::new(),
        slot_of: FxHashMap::default(),
        stubs: Vec::new(),
        stub_of: FxHashMap::default(),
        imports: Vec::new(),
        import_of: FxHashMap::default(),
        load_relocations: 0,
        load_from: FxHashSet::default(),
        needed: Vec::new(),
        version_of: FxHashMap::default(),
        dynstr: vec![0],
        dynstr_of: FxHashMap::default(),
        started: FxHashSet::default(),
        tls: None,
    };
    let entry_place = plan.reach(entry)?;
    plan.scan()?;
    plan.libraries();
    plan.assign(layout, request)?;
    let (mut bytes, phdrs, placed) = plan.place(machine, request)?;
    plan.fill(&mut bytes, machine, request, entry_place, phdrs)?;
    Ok(Written { bytes, placed, imports: plan.imports.len() })
}

/// The output section an input section goes in, its segment and its rank there.
fn output_of(name: &str, sh_type: u32, flags: u64) -> (String, Segment, u32) {
    let has = |flag: u32| flags & u64::from(flag) != 0;
    let prefix = |of: &str| name == of || name.starts_with(&format!("{of}."));
    if sh_type == elf::SHT_NOTE {
        return (name.to_owned(), Segment::Read, 1);
    }
    if has(elf::SHF_TLS) {
        return match sh_type == elf::SHT_NOBITS {
            true => (".tbss".to_owned(), Segment::Relro, 1),
            false => (".tdata".to_owned(), Segment::Relro, 0),
        };
    }
    if has(elf::SHF_EXECINSTR) {
        return match name {
            ".init" => (".init".to_owned(), Segment::Code, 0),
            ".fini" => (".fini".to_owned(), Segment::Code, 3),
            _ if prefix(".text") => (".text".to_owned(), Segment::Code, 1),
            _ => (name.to_owned(), Segment::Code, 2),
        };
    }
    if prefix(".preinit_array") || sh_type == elf::SHT_PREINIT_ARRAY {
        return (".preinit_array".to_owned(), Segment::Relro, 2);
    }
    if prefix(".init_array") || sh_type == elf::SHT_INIT_ARRAY {
        return (".init_array".to_owned(), Segment::Relro, 3);
    }
    if prefix(".fini_array") || sh_type == elf::SHT_FINI_ARRAY {
        return (".fini_array".to_owned(), Segment::Relro, 4);
    }
    if has(elf::SHF_WRITE) {
        if prefix(".data.rel.ro") {
            return (".data.rel.ro".to_owned(), Segment::Relro, 5);
        }
        if sh_type == elf::SHT_NOBITS {
            let out = if prefix(".bss") { ".bss" } else { name };
            return (out.to_owned(), Segment::Data, 9);
        }
        if prefix(".data") {
            return (".data".to_owned(), Segment::Data, 0);
        }
        return (name.to_owned(), Segment::Data, 1);
    }
    if prefix(".rodata") {
        return (".rodata".to_owned(), Segment::Read, 10);
    }
    if prefix(".gcc_except_table") {
        return (".gcc_except_table".to_owned(), Segment::Read, 11);
    }
    (name.to_owned(), Segment::Read, 12)
}

/// The relocations of `relocs` (in the order of their offsets) at `start..end`.
fn within(relocs: &[Reloc], start: u64, end: u64) -> &[Reloc] {
    let from = relocs.partition_point(|it| it.offset < start);
    let to = relocs.partition_point(|it| it.offset < end);
    &relocs[from..to]
}

/// The order `.init_array.N` and `.fini_array.N` run in: by `N`, those without one last.
fn priority(name: &str) -> u32 {
    name.rsplit_once('.').and_then(|(_, it)| it.parse().ok()).unwrap_or(65536)
}

impl<'a> Plan<'_, 'a> {
    fn where_is(&self, object: u32, section: u32, offset: u64) -> String {
        let obj = &self.link.objects[object as usize];
        format!("{} ({}+{offset:#x})", obj.name, obj.sections[section as usize].name)
    }

    /// Marks what the program reaches; the place `entry` is at.
    fn reach(&mut self, entry: &str) -> anyhow::Result<Res> {
        let id = *self.link.names.get(entry).with_context(|| format!("no input defines `{entry}`"))?;
        let Def::At { object, section, offset } = self.link.globals[id as usize].def else {
            bail!("`{entry}`, which the program starts at, is not defined in an object")
        };
        let mut work = vec![(object, section)];
        for (o, obj) in self.link.objects.iter().enumerate() {
            for (s, sec) in obj.sections.iter().enumerate() {
                if !sec.kept || sec.name == ".eh_frame" {
                    continue;
                }
                let root = sec.sh_type == elf::SHT_NOTE
                    || matches!(
                        sec.sh_type,
                        elf::SHT_INIT_ARRAY | elf::SHT_FINI_ARRAY | elf::SHT_PREINIT_ARRAY
                    )
                    || [".init", ".fini"].contains(&sec.name)
                    || [".init_array", ".fini_array", ".preinit_array"]
                        .iter()
                        .any(|it| sec.name == *it || sec.name.starts_with(&format!("{it}.")))
                    || sec.flags & u64::from(elf::SHF_GNU_RETAIN) != 0;
                if [".ctors", ".dtors"].iter().any(|it| sec.name.starts_with(it)) {
                    bail!("{}: {} is a table blaze's patching does not run yet", obj.name, sec.name);
                }
                if root {
                    work.push((o as u32, s as u32));
                }
            }
        }
        // lld keeps `.note.gnu.property` only when every input has one, which blaze's
        // objects do not; an image whose inputs all had it would differ, so it stops.
        if self.link.objects.iter().all(|it| it.has_property_note) {
            bail!("every input has .note.gnu.property, which blaze's patching does not merge yet");
        }
        self.read_frames()?;
        loop {
            while let Some((object, section)) = work.pop() {
                let sec = &mut self.link.objects[object as usize].sections[section as usize];
                if !sec.kept || sec.live || sec.name == ".eh_frame" {
                    continue;
                }
                sec.live = true;
                let targets: Vec<To> = sec.relocs.iter().map(|it| it.to).collect();
                for to in targets {
                    self.reached(to, &mut work);
                }
            }
            // A frame of what is reached keeps its LSDA, and its common entry's personality.
            for f in 0..self.frames.len() {
                let frame = &self.frames[f];
                if frame.kept || frame.cie.is_none() {
                    continue;
                }
                let (object, section, start) = (frame.object, frame.section, frame.start);
                let sec = &self.link.objects[object as usize].sections[section as usize];
                let Some(first) = within(&sec.relocs, start + 8, start + 9).first() else {
                    continue;
                };
                if !self.is_live(first.to) {
                    continue;
                }
                self.frames[f].kept = true;
                let cie = self.frames[f].cie.unwrap_or(f);
                self.frames[cie].kept = true;
                let mut targets: Vec<To> = Vec::new();
                for which in [f, cie] {
                    let Frame { start, len, .. } = self.frames[which];
                    targets.extend(within(&sec.relocs, start, start + len).iter().map(|it| it.to));
                }
                for to in targets {
                    self.reached(to, &mut work);
                }
            }
            if work.is_empty() {
                break;
            }
        }
        Ok(Res::Sec { object, section, offset })
    }

    fn reached(&mut self, to: To, work: &mut Vec<(u32, u32)>) {
        match to {
            To::At { object, section, .. } => work.push((object, section)),
            To::Abs(_) => {}
            To::Global(id) => match self.link.globals[id as usize].def {
                Def::At { object, section, .. } => work.push((object, section)),
                Def::Synthetic => {
                    let name = self.link.globals[id as usize].name;
                    let Some(of) = name.strip_prefix("__start_").or(name.strip_prefix("__stop_"))
                    else {
                        return;
                    };
                    if !self.started.insert(of.to_owned()) {
                        return;
                    }
                    for (o, obj) in self.link.objects.iter().enumerate() {
                        for (s, sec) in obj.sections.iter().enumerate() {
                            if sec.kept && sec.name == of {
                                work.push((o as u32, s as u32));
                            }
                        }
                    }
                }
                _ => {}
            },
        }
    }

    fn is_live(&self, to: To) -> bool {
        let at = match to {
            To::At { object, section, .. } => (object, section),
            To::Global(id) => match self.link.globals[id as usize].def {
                Def::At { object, section, .. } => (object, section),
                _ => return false,
            },
            To::Abs(_) => return false,
        };
        let sec = &self.link.objects[at.0 as usize].sections[at.1 as usize];
        sec.kept && sec.live
    }

    /// The records of every input's `.eh_frame`.
    fn read_frames(&mut self) -> anyhow::Result<()> {
        for (o, obj) in self.link.objects.iter().enumerate() {
            for (s, sec) in obj.sections.iter().enumerate() {
                if !sec.kept || sec.name != ".eh_frame" {
                    continue;
                }
                let data = sec.data;
                let mut at = 0usize;
                let mut cies: FxHashMap<u64, usize> = FxHashMap::default();
                while at + 4 <= data.len() {
                    let len = u32::from_le_bytes(data[at..at + 4].try_into()?) as u64;
                    if len == 0 {
                        break;
                    }
                    ensure!(
                        len != 0xffff_ffff,
                        "{}: 64-bit DWARF in .eh_frame at {at:#x}, which blaze's patching does not read",
                        obj.name
                    );
                    let end = at + 4 + len as usize;
                    ensure!(end <= data.len(), "{}: an .eh_frame record past its end at {at:#x}", obj.name);
                    let id = u32::from_le_bytes(data[at + 4..at + 8].try_into()?) as u64;
                    let cie = if id == 0 {
                        cies.insert(at as u64, self.frames.len());
                        None
                    } else {
                        let points = (at as u64 + 4).checked_sub(id).with_context(|| {
                            format!("{}: an FDE at {at:#x} whose CIE is before the section", obj.name)
                        })?;
                        Some(*cies.get(&points).with_context(|| {
                            format!("{}: an FDE at {at:#x} whose CIE is not one", obj.name)
                        })?)
                    };
                    self.frames.push(Frame {
                        object: o as u32,
                        section: s as u32,
                        start: at as u64,
                        len: 4 + len,
                        cie,
                        kept: false,
                        at: 0,
                    });
                    at = end;
                }
            }
        }
        Ok(())
    }

    fn resolve(&self, to: To, from: (u32, u32, u64)) -> anyhow::Result<Res> {
        Ok(match to {
            To::At { object, section, offset } => {
                let sec = &self.link.objects[object as usize].sections[section as usize];
                ensure!(
                    sec.kept,
                    "{}: refers to {} of {}, which another object's copy of its group replaced",
                    self.where_is(from.0, from.1, from.2),
                    sec.name,
                    self.link.objects[object as usize].name
                );
                Res::Sec { object, section, offset }
            }
            To::Abs(value) => Res::Abs(value),
            To::Global(id) => {
                let global = &self.link.globals[id as usize];
                match global.def {
                    Def::At { object, section, offset } => Res::Sec { object, section, offset },
                    Def::Abs(value) => Res::Abs(value),
                    Def::Shared { .. } => Res::Import(id),
                    Def::Synthetic => Res::Synthetic(id),
                    Def::None if global.strong_ref => bail!(
                        "undefined symbol `{}`, referred to by {}",
                        global.name,
                        self.where_is(from.0, from.1, from.2)
                    ),
                    Def::None => Res::Import(id),
                }
            }
        })
    }

    fn import(&mut self, id: u32) {
        if !self.import_of.contains_key(&id) {
            self.import_of.insert(id, self.imports.len() as u32 + 1);
            self.imports.push(id);
        }
    }

    fn slot(&mut self, slot: Slot) {
        if self.slot_of.contains_key(&slot) {
            return;
        }
        self.slot_of.insert(slot, self.slots.len() as u32);
        self.slots.push(slot);
        match slot {
            Slot::Address(Res::Import(id)) => {
                self.import(id);
                self.load_relocations += 1;
            }
            Slot::Address(Res::Sec { .. } | Res::Synthetic(_)) | Slot::Module(_) => {
                self.load_relocations += 1;
            }
            _ => {}
        }
    }

    fn tls_target(&self, res: Res, from: (u32, u32, u64)) -> anyhow::Result<()> {
        match res {
            Res::Sec { object, section, .. }
                if self.link.objects[object as usize].sections[section as usize].flags
                    & u64::from(elf::SHF_TLS)
                    != 0 =>
            {
                Ok(())
            }
            _ => bail!(
                "{}: a thread-local relocation to what is not a thread-local of the program \
                 (one of a shared library comes in stage 2, docs/patching.md)",
                self.where_is(from.0, from.1, from.2)
            ),
        }
    }

    /// Reads every relocation of what is placed for what the image needs besides the pieces.
    fn scan(&mut self) -> anyhow::Result<()> {
        let machine = self.link.machine;
        for o in 0..self.link.objects.len() {
            for s in 0..self.link.objects[o].sections.len() {
                let sec = &self.link.objects[o].sections[s];
                let frame = sec.name == ".eh_frame";
                if !sec.kept || !(sec.live || frame) {
                    continue;
                }
                let writable = sec.flags & u64::from(elf::SHF_WRITE) != 0;
                let nobits = sec.sh_type == elf::SHT_NOBITS;
                ensure!(
                    !nobits || sec.flags & u64::from(elf::SHF_TLS) == 0 || sec.relocs.is_empty(),
                    "{}: relocations in a thread-local section without bytes",
                    self.where_is(o as u32, s as u32, 0)
                );
                // Of `.eh_frame`, the relocations of the records that are kept.
                let relocs: Vec<Reloc> = match frame {
                    true => self
                        .frames
                        .iter()
                        .filter(|it| it.object == o as u32 && it.section == s as u32 && it.kept)
                        .flat_map(|it| within(&sec.relocs, it.start, it.start + it.len).iter().copied())
                        .collect(),
                    false => sec.relocs.clone(),
                };
                let mut skip = None;
                for reloc in relocs {
                    let from = (o as u32, s as u32, reloc.offset);
                    if skip == Some(reloc.offset) {
                        skip = None;
                        continue;
                    }
                    let kind = kind(machine, reloc.r_type).with_context(|| {
                        format!(
                            "{}: relocation {} is not supported by blaze's patching (docs/patching.md)",
                            self.where_is(from.0, from.1, from.2),
                            type_name(machine, reloc.r_type)
                        )
                    })?;
                    if kind == Kind::None {
                        continue;
                    }
                    let res = self.resolve(reloc.to, from)?;
                    // Zeroed memory holds only what the loader writes there: an address. lld
                    // takes it so, and blaze's codegen puts a constant whose bytes are all
                    // relocated (`.Lblaze.const.*`, the address of a static) in `.bss`.
                    ensure!(
                        !nobits || kind == Kind::Abs64 && !matches!(res, Res::Abs(_)),
                        "{}: relocation {} in a section without bytes, which only the address the loader \
                         writes can be in (docs/patching.md)",
                        self.where_is(from.0, from.1, from.2),
                        type_name(machine, reloc.r_type)
                    );
                    match kind {
                        Kind::None => {}
                        Kind::Abs64 => match res {
                            Res::Abs(_) => {}
                            _ => {
                                ensure!(
                                    writable,
                                    "{}: an address the loader has to write, in read-only {}",
                                    self.where_is(from.0, from.1, from.2),
                                    self.link.objects[o].sections[s].name
                                );
                                if let Res::Import(id) = res {
                                    self.import(id);
                                }
                                self.load_relocations += 1;
                                self.load_from.insert(from);
                            }
                        },
                        Kind::Rel(_) => {
                            if let Res::Import(id) = res {
                                bail!(
                                    "{}: refers to `{}` of a shared library other than through a \
                                     slot or a call, which needs a copy relocation (stage 2, docs/patching.md)",
                                    self.where_is(from.0, from.1, from.2),
                                    self.link.globals[id as usize].name
                                );
                            }
                            if let Res::Abs(_) = res {
                                bail!(
                                    "{}: a distance to an absolute address, which a position-independent image has not",
                                    self.where_is(from.0, from.1, from.2)
                                );
                            }
                        }
                        Kind::Call(_) => {
                            if let Res::Import(id) = res {
                                self.import(id);
                                self.slot(Slot::Address(res));
                                if !self.stub_of.contains_key(&id) {
                                    self.stub_of.insert(id, self.stubs.len() as u32);
                                    self.stubs.push(id);
                                }
                            }
                            ensure!(
                                !matches!(res, Res::Abs(_)),
                                "{}: a call to an absolute address",
                                self.where_is(from.0, from.1, from.2)
                            );
                        }
                        Kind::Got(_) => self.slot(Slot::Address(res)),
                        Kind::GotTp(_) => {
                            self.tls_target(res, from)?;
                            self.slot(Slot::TpOffset(res));
                        }
                        Kind::Tp32 | Kind::TpHi12 | Kind::TpLo12 | Kind::TlsDesc(_) => {
                            self.tls_target(res, from)?;
                        }
                        Kind::TlsGdX86 => {
                            self.tls_target(res, from)?;
                            skip = Some(reloc.offset + 8);
                        }
                        Kind::TlsGdPair(_) => {
                            self.tls_target(res, from)?;
                            self.slot(Slot::Module(res));
                            self.slot(Slot::Offset(res));
                        }
                    }
                }
                ensure!(
                    skip.is_none(),
                    "{}: a general-dynamic thread-local without its call of __tls_get_addr",
                    self.where_is(o as u32, s as u32, 0)
                );
            }
        }
        Ok(())
    }

    fn string(&mut self, text: &str) -> u32 {
        if let Some(&at) = self.dynstr_of.get(text) {
            return at;
        }
        let at = self.dynstr.len() as u32;
        self.dynstr.extend_from_slice(text.as_bytes());
        self.dynstr.push(0);
        self.dynstr_of.insert(text.to_owned(), at);
        at
    }

    /// Which libraries are needed, and the versions of theirs the imports bind.
    fn libraries(&mut self) {
        let mut used = vec![false; self.link.libs.len()];
        for &id in &self.imports {
            let global = &self.link.globals[id as usize];
            if let Def::Shared { lib, .. } = global.def
                && global.strong_ref
            {
                used[lib as usize] = true;
            }
        }
        for (lib, it) in self.link.libs.iter().enumerate() {
            if (!it.as_needed || used[lib]) && !self.needed.contains(&(lib as u32)) {
                self.needed.push(lib as u32);
            }
        }
        let mut next = 2u16;
        let needed = self.needed.clone();
        for &lib in &needed {
            let soname = self.link.libs[lib as usize].soname;
            self.string(soname);
        }
        for &lib in &needed {
            for i in 0..self.imports.len() {
                let id = self.imports[i];
                if let Def::Shared { lib: of, version: Some(version) } =
                    self.link.globals[id as usize].def
                    && of == lib
                    && !self.version_of.contains_key(&(lib, version))
                {
                    self.version_of.insert((lib, version), next);
                    next += 1;
                    let name = self.link.libs[lib as usize].versions[version as usize].0;
                    self.string(name);
                }
            }
        }
        for i in 0..self.imports.len() {
            let name = self.link.globals[self.imports[i] as usize].name;
            self.string(name);
        }
    }

    fn version_needs(&self) -> Vec<(u32, Vec<(u32, u16)>)> {
        let mut needs = Vec::new();
        for &lib in &self.needed {
            let mut versions: Vec<(u32, u16)> = self
                .version_of
                .iter()
                .filter(|((of, _), _)| *of == lib)
                .map(|((_, version), index)| (*version, *index))
                .collect();
            versions.sort_by_key(|it| it.1);
            if !versions.is_empty() {
                needs.push((lib, versions));
            }
        }
        needs
    }

    fn dynamic_entries(&self) -> usize {
        let arrays = [".preinit_array", ".init_array", ".fini_array"]
            .iter()
            .filter(|it| self.outs.iter().any(|out| out.name == **it))
            .count();
        let inits = ["_init", "_fini"]
            .iter()
            .filter(|it| self.defined_at(it).is_some())
            .count();
        let versions = if self.version_of.is_empty() { 0 } else { 3 };
        // NEEDED..., GNU_HASH, STRTAB, SYMTAB, STRSZ, SYMENT, DEBUG, RELA, RELASZ, RELAENT,
        // RELACOUNT, FLAGS, FLAGS_1, NULL.
        self.needed.len() + 13 + arrays * 2 + inits + versions
    }

    fn defined_at(&self, name: &str) -> Option<(u32, u32, u64)> {
        let id = *self.link.names.get(name)?;
        match self.link.globals[id as usize].def {
            Def::At { object, section, offset } => Some((object, section, offset)),
            _ => None,
        }
    }

    /// Puts each piece in its output section, and adds the image's own sections.
    fn assign(&mut self, layout: &mut Layout, request: &Request) -> anyhow::Result<()> {
        let mut out_of: FxHashMap<String, usize> = FxHashMap::default();
        let mut outs: Vec<Out> = Vec::new();
        let synth = |name: &str, segment, rank, sh_type, flags: u32, align, synth| Out {
            name: name.to_owned(),
            segment,
            rank,
            sh_type,
            flags: u64::from(flags),
            align,
            size: 0,
            address: 0,
            tls_nobits: false,
            synth: Some(synth),
            pieces: Vec::new(),
        };
        let a = elf::SHF_ALLOC;
        outs.push(synth(".interp", Segment::Read, 0, elf::SHT_PROGBITS, a, 1, Synth::Interp));
        outs.push(synth(".note.gnu.build-id", Segment::Read, 1, elf::SHT_NOTE, a, 4, Synth::BuildId));
        outs.push(synth(".dynsym", Segment::Read, 2, elf::SHT_DYNSYM, a, 8, Synth::Dynsym));
        if !self.version_of.is_empty() {
            outs.push(synth(".gnu.version", Segment::Read, 3, elf::SHT_GNU_VERSYM, a, 2, Synth::Version));
            outs.push(synth(
                ".gnu.version_r",
                Segment::Read,
                4,
                elf::SHT_GNU_VERNEED,
                a,
                4,
                Synth::VersionNeeded,
            ));
        }
        outs.push(synth(".gnu.hash", Segment::Read, 5, elf::SHT_GNU_HASH, a, 8, Synth::Hash));
        outs.push(synth(".dynstr", Segment::Read, 6, elf::SHT_STRTAB, a, 1, Synth::Dynstr));
        outs.push(synth(".rela.dyn", Segment::Read, 7, elf::SHT_RELA, a, 8, Synth::RelaDyn));
        outs.push(synth(".eh_frame_hdr", Segment::Read, 20, elf::SHT_PROGBITS, a, 4, Synth::EhFrameHdr));
        let unwind = match self.link.machine {
            Machine::X86_64 => elf::SHT_X86_64_UNWIND,
            Machine::Aarch64 => elf::SHT_PROGBITS,
        };
        outs.push(synth(".eh_frame", Segment::Read, 21, unwind, a, 8, Synth::EhFrame));
        if !self.stubs.is_empty() {
            outs.push(synth(".plt", Segment::Code, 4, elf::SHT_PROGBITS, a | elf::SHF_EXECINSTR, 16, Synth::Plt));
        }
        let rw = a | elf::SHF_WRITE;
        outs.push(synth(".dynamic", Segment::Relro, 6, elf::SHT_DYNAMIC, rw, 8, Synth::Dynamic));
        outs.push(synth(".got", Segment::Relro, 7, elf::SHT_PROGBITS, rw, 8, Synth::Got));
        for (at, out) in outs.iter().enumerate() {
            out_of.insert(out.name.clone(), at);
        }
        let mut arrays: Vec<(usize, u32, u32, u32)> = Vec::new();
        for (o, obj) in self.link.objects.iter().enumerate() {
            for (s, sec) in obj.sections.iter().enumerate() {
                if !sec.kept || !sec.live {
                    continue;
                }
                let (name, segment, rank) = output_of(sec.name, sec.sh_type, sec.flags);
                let at = *out_of.entry(name.clone()).or_insert_with(|| {
                    outs.push(Out {
                        name: name.clone(),
                        segment,
                        rank,
                        sh_type: sec.sh_type,
                        flags: sec.flags
                            & u64::from(elf::SHF_ALLOC | elf::SHF_WRITE | elf::SHF_EXECINSTR | elf::SHF_TLS),
                        align: 1,
                        size: 0,
                        address: 0,
                        tls_nobits: name == ".tbss",
                        synth: None,
                        pieces: Vec::new(),
                    });
                    outs.len() - 1
                });
                ensure!(
                    outs[at].segment == segment,
                    "{}: {} would go in {}, which another input's is in another segment",
                    obj.name,
                    sec.name,
                    name
                );
                if name.ends_with("_array") {
                    arrays.push((at, priority(sec.name), o as u32, s as u32));
                } else {
                    outs[at].pieces.push((o as u32, s as u32));
                }
            }
        }
        // The start-up tables run in the order of their priorities, then the inputs'.
        arrays.sort_by_key(|it| (it.0, it.1));
        for (at, _, o, s) in arrays {
            outs[at].pieces.push((o, s));
        }
        outs.retain(|it| it.synth.is_some() || !it.pieces.is_empty());
        outs.sort_by_key(|it| (it.segment, it.rank));
        for (index, out) in outs.iter_mut().enumerate() {
            for &(o, s) in &out.pieces {
                let sec = &mut self.link.objects[o as usize].sections[s as usize];
                out.align = out.align.max(sec.align);
                sec.out = index as u32;
                sec.at = layout.place(&out.name, sec.size, sec.align);
            }
            out.size = layout.placed.get(&out.name).copied().unwrap_or(0);
        }
        self.outs = outs;
        // The sizes of the image's own sections.
        let mut frames = 0u64;
        let mut fdes = 0u64;
        for frame in &mut self.frames {
            if frame.kept {
                frame.at = frames;
                frames += frame.len;
                fdes += u64::from(frame.cie.is_some());
            }
        }
        let needs = self.version_needs();
        let dynamic = self.dynamic_entries() as u64;
        let imports = self.imports.len() as u64;
        let (stub, _) = self.link.machine.stub();
        let sizes = [
            (Synth::Interp, request.interpreter.len() as u64 + 1),
            (Synth::BuildId, 16 + BUILD_ID as u64),
            (Synth::Dynsym, (imports + 1) * 24),
            (Synth::Version, (imports + 1) * 2),
            (
                Synth::VersionNeeded,
                needs.iter().map(|it| 16 + 16 * it.1.len() as u64).sum(),
            ),
            (Synth::Hash, 16 + 8 + 4),
            (Synth::Dynstr, self.dynstr.len() as u64),
            (Synth::RelaDyn, self.load_relocations as u64 * 24),
            (Synth::EhFrameHdr, 12 + 8 * fdes),
            // And the zero length that ends it.
            (Synth::EhFrame, frames + 4),
            (Synth::Plt, self.stubs.len() as u64 * stub as u64),
            (Synth::Dynamic, dynamic * 16),
            (Synth::Got, self.slots.len() as u64 * 8),
        ];
        for out in &mut self.outs {
            if let Some(synth) = out.synth {
                out.size = sizes.iter().find(|it| it.0 == synth).map_or(0, |it| it.1);
            }
        }
        Ok(())
    }

    fn out(&self, synth: Synth) -> Option<&Out> {
        self.outs.iter().find(|it| it.synth == Some(synth))
    }

    fn named(&self, name: &str) -> Option<&Out> {
        self.outs.iter().find(|it| it.name == name)
    }

    /// Gives every output section its address, and the image its program headers.
    fn place(
        &mut self,
        machine: Machine,
        _request: &Request,
    ) -> anyhow::Result<(Vec<u8>, Vec<[u64; 7]>, usize)> {
        let page: u64 = match machine {
            Machine::X86_64 => 0x1000,
            // lld's maximum page size on AArch64, so kernels of 64 KB pages load it.
            Machine::Aarch64 => 0x10000,
        };
        let has_tls = self.outs.iter().any(|it| it.flags & u64::from(elf::SHF_TLS) != 0);
        let segments = [Segment::Read, Segment::Code, Segment::Relro, Segment::Data];
        let loads = segments.iter().filter(|s| self.outs.iter().any(|it| it.segment == **s)).count();
        // PHDR, INTERP, the loads, DYNAMIC, NOTE, TLS, GNU_EH_FRAME, GNU_STACK, GNU_RELRO.
        let phnum = 2 + loads + 1 + 1 + usize::from(has_tls) + 3;
        let mut cursor = EHDR + PHDR * phnum as u64;
        let mut file_end = 0;
        let mut loads_at: Vec<(Segment, u64, u64, u64)> = Vec::new();
        for segment in segments {
            if !self.outs.iter().any(|it| it.segment == segment) {
                continue;
            }
            if segment != Segment::Read {
                cursor = cursor.next_multiple_of(page);
            }
            let start = if segment == Segment::Read { 0 } else { cursor };
            let mut filesz_end = cursor;
            let mut tls: Option<(u64, u64, u64)> = None;
            for out in self.outs.iter_mut().filter(|it| it.segment == segment) {
                let address = cursor.next_multiple_of(out.align);
                out.address = address;
                if out.flags & u64::from(elf::SHF_TLS) != 0 {
                    let (start, end, align) = tls.get_or_insert((address, address, out.align));
                    *end = address + out.size;
                    *align = (*align).max(out.align);
                    let _ = start;
                }
                if out.tls_nobits {
                    continue;
                }
                cursor = address + out.size;
                if out.sh_type != elf::SHT_NOBITS {
                    filesz_end = cursor;
                }
            }
            if let Some(tls) = tls {
                self.tls = Some(tls);
            }
            file_end = file_end.max(filesz_end);
            loads_at.push((segment, start, filesz_end - start, cursor - start));
        }
        let mut phdrs: Vec<[u64; 7]> = Vec::new();
        // [type, flags, offset/vaddr, filesz, memsz, align] as p_type | p_flags << 32 in [0].
        let mut push = |p_type: u32, flags: u32, at: u64, filesz: u64, memsz: u64, align: u64| {
            phdrs.push([u64::from(p_type), u64::from(flags), at, filesz, memsz, align, 0]);
        };
        let header = EHDR + PHDR * phnum as u64;
        push(elf::PT_PHDR, elf::PF_R, EHDR, PHDR * phnum as u64, PHDR * phnum as u64, 8);
        let interp = self.out(Synth::Interp).context("no .interp")?;
        push(elf::PT_INTERP, elf::PF_R, interp.address, interp.size, interp.size, 1);
        for &(segment, start, filesz, memsz) in &loads_at {
            let flags = match segment {
                Segment::Read => elf::PF_R,
                Segment::Code => elf::PF_R | elf::PF_X,
                Segment::Relro | Segment::Data => elf::PF_R | elf::PF_W,
            };
            push(elf::PT_LOAD, flags, start, filesz, memsz, page);
        }
        let dynamic = self.out(Synth::Dynamic).context("no .dynamic")?;
        push(elf::PT_DYNAMIC, elf::PF_R | elf::PF_W, dynamic.address, dynamic.size, dynamic.size, 8);
        let notes: Vec<&Out> = self.outs.iter().filter(|it| it.sh_type == elf::SHT_NOTE).collect();
        let (first, last) = (notes.first().context("no notes")?, notes.last().context("no notes")?);
        ensure!(
            notes.windows(2).all(|it| it[0].address + it[0].size == it[1].address),
            "the image's notes are not next to each other"
        );
        let size = last.address + last.size - first.address;
        push(elf::PT_NOTE, elf::PF_R, first.address, size, size, 4);
        if let Some((start, end, align)) = self.tls {
            let filesz = self
                .outs
                .iter()
                .filter(|it| it.flags & u64::from(elf::SHF_TLS) != 0 && !it.tls_nobits)
                .map(|it| it.address + it.size - start)
                .max()
                .unwrap_or(0);
            push(elf::PT_TLS, elf::PF_R, start, filesz, end - start, align);
        }
        let hdr = self.out(Synth::EhFrameHdr).context("no .eh_frame_hdr")?;
        push(elf::PT_GNU_EH_FRAME, elf::PF_R, hdr.address, hdr.size, hdr.size, 4);
        push(elf::PT_GNU_STACK, elf::PF_R | elf::PF_W, 0, 0, 0, 16);
        let relro = loads_at.iter().find(|it| it.0 == Segment::Relro).context("no relro segment")?;
        push(elf::PT_GNU_RELRO, elf::PF_R, relro.1, relro.2, relro.3, 1);
        ensure!(phdrs.len() == phnum, "{} program headers, not {phnum}", phdrs.len());
        ensure!(
            self.outs.iter().filter(|it| it.segment == Segment::Read).all(|it| it.address >= header),
            "the program headers overlap a section"
        );
        let mut placed = 0;
        for o in 0..self.link.objects.len() {
            for sec in &self.link.objects[o].sections {
                placed += usize::from(sec.live && sec.kept && sec.name != ".eh_frame");
            }
        }
        Ok((vec![0; file_end as usize], phdrs, placed))
    }

    fn address_of(&self, res: Res) -> anyhow::Result<u64> {
        Ok(match res {
            Res::Sec { object, section, offset } => {
                let sec = &self.link.objects[object as usize].sections[section as usize];
                ensure!(sec.live, "a reference to {} of {}, which is not placed", sec.name, self.link.objects[object as usize].name);
                self.outs[sec.out as usize].address + sec.at + offset
            }
            Res::Abs(value) => value,
            Res::Synthetic(id) => self.synthetic(id)?,
            Res::Import(_) => bail!("the address of an import"),
        })
    }

    fn synthetic(&self, id: u32) -> anyhow::Result<u64> {
        let name = self.link.globals[id as usize].name;
        let start = |of: &str| self.named(of).map(|it| it.address);
        let end = |of: &str| self.named(of).map(|it| it.address + it.size);
        let segment_end = |segment: Segment, file: bool| {
            self.outs
                .iter()
                .filter(|it| it.segment == segment && !it.tls_nobits)
                .filter(|it| !file || it.sh_type != elf::SHT_NOBITS)
                .map(|it| it.address + it.size)
                .max()
        };
        let relro = self.named(".dynamic").map(|it| it.address);
        let found = match name {
            "__ehdr_start" | "__executable_start" => Some(0),
            "_DYNAMIC" => start(".dynamic"),
            "_GLOBAL_OFFSET_TABLE_" => start(".got"),
            "__GNU_EH_FRAME_HDR" => start(".eh_frame_hdr"),
            "__rela_iplt_start" | "__rela_iplt_end" => end(".rela.dyn"),
            "__bss_start" | "_edata" | "edata" => {
                segment_end(Segment::Data, true).or(segment_end(Segment::Relro, true))
            }
            "_end" | "end" => segment_end(Segment::Data, false).or(segment_end(Segment::Relro, false)),
            "_etext" | "etext" | "__etext" => segment_end(Segment::Code, false),
            _ => {
                if let Some(array) = name
                    .strip_prefix("__")
                    .and_then(|it| it.strip_suffix("_start").or(it.strip_suffix("_end")))
                    .filter(|it| ["init_array", "fini_array", "preinit_array"].contains(it))
                {
                    let of = format!(".{array}");
                    match name.ends_with("_start") {
                        true => start(&of).or(relro),
                        false => end(&of).or(relro),
                    }
                } else if let Some(of) = name.strip_prefix("__start_") {
                    start(of)
                } else if let Some(of) = name.strip_prefix("__stop_") {
                    end(of)
                } else {
                    None
                }
            }
        };
        match found {
            Some(it) => Ok(it),
            None if !self.link.globals[id as usize].strong_ref => Ok(0),
            None => bail!("`{name}` is referred to, and the image has no section it names"),
        }
    }

    fn slot_address(&self, slot: Slot) -> anyhow::Result<u64> {
        let got = self.out(Synth::Got).context("no .got")?;
        let index = self.slot_of.get(&slot).context("a slot that was not counted")?;
        Ok(got.address + u64::from(*index) * 8)
    }

    fn stub_address(&self, id: u32) -> anyhow::Result<u64> {
        let plt = self.out(Synth::Plt).context("no .plt")?;
        let index = self.stub_of.get(&id).context("a stub that was not counted")?;
        Ok(plt.address + u64::from(*index) * self.link.machine.stub().0 as u64)
    }

    /// The offset of a thread-local at `address` from the thread pointer.
    fn tp_offset(&self, address: u64) -> anyhow::Result<i64> {
        let (start, end, align) = self.tls.context("a thread-local, and the image has none")?;
        let within = (address - start) as i64;
        Ok(match self.link.machine {
            // The block ends at the thread pointer.
            Machine::X86_64 => within - (end - start).next_multiple_of(align) as i64,
            // The block starts 16 bytes past it, aligned.
            Machine::Aarch64 => within + 16u64.next_multiple_of(align) as i64,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn apply(
        &self,
        bytes: &mut [u8],
        reloc: &Reloc,
        kind: Kind,
        place: u64,
        res: Res,
        load: &mut Vec<(u64, u32, u32, i64)>,
        from: (u32, u32, u64),
    ) -> anyhow::Result<()> {
        let machine = self.link.machine;
        let at = place as usize;
        let len = bytes.len();
        let field = |kind: Field| -> anyhow::Result<std::ops::Range<usize>> {
            let range = at..at + kind.len();
            ensure!(range.end <= len, "a relocation past the image");
            Ok(range)
        };
        let fill = |bytes: &mut [u8], f: Field, to: u64| -> anyhow::Result<()> {
            let range = field(f)?;
            f.fill(&mut bytes[range], to, place).with_context(|| self.where_is(from.0, from.1, from.2))
        };
        let addend = reloc.addend;
        match kind {
            Kind::None => {}
            Kind::Abs64 => {
                let range = field(Field::Address)?;
                match res {
                    Res::Abs(value) => {
                        bytes[range].copy_from_slice(&value.wrapping_add_signed(addend).to_le_bytes())
                    }
                    Res::Import(id) => {
                        let symbol = self.import_of[&id];
                        let r_type = match machine {
                            Machine::X86_64 => elf::R_X86_64_64,
                            Machine::Aarch64 => elf::R_AARCH64_ABS64,
                        };
                        load.push((place, r_type, symbol, addend));
                    }
                    _ => {
                        let value = self.address_of(res)?.wrapping_add_signed(addend);
                        bytes[range].copy_from_slice(&value.to_le_bytes());
                        load.push((place, machine.relative(), 0, value as i64));
                    }
                }
            }
            Kind::Rel(f) => fill(bytes, f, self.address_of(res)?.wrapping_add_signed(addend))?,
            Kind::Call(f) => {
                let to = match res {
                    Res::Import(id) => self.stub_address(id)?,
                    _ => self.address_of(res)?,
                };
                fill(bytes, f, to.wrapping_add_signed(addend))?;
            }
            Kind::Got(f) => {
                fill(bytes, f, self.slot_address(Slot::Address(res))?.wrapping_add_signed(addend))?
            }
            Kind::GotTp(f) => {
                fill(bytes, f, self.slot_address(Slot::TpOffset(res))?.wrapping_add_signed(addend))?
            }
            Kind::TlsGdPair(f) => {
                fill(bytes, f, self.slot_address(Slot::Module(res))?.wrapping_add_signed(addend))?
            }
            Kind::Tp32 => {
                let offset = self.tp_offset(self.address_of(res)?)? + addend;
                let offset = i32::try_from(offset).context("a thread-local too far from the thread pointer")?;
                let range = field(Field::Distance)?;
                bytes[range].copy_from_slice(&offset.to_le_bytes());
            }
            Kind::TpHi12 | Kind::TpLo12 => {
                let offset = self.tp_offset(self.address_of(res)?)? + addend;
                ensure!(
                    (0..1 << 24).contains(&offset),
                    "{}: a thread-local past 16 MB of the thread pointer",
                    self.where_is(from.0, from.1, from.2)
                );
                let bits = if kind == Kind::TpHi12 { offset >> 12 } else { offset } as u32 & 0xfff;
                let range = field(Field::Distance)?;
                let old = u32::from_le_bytes(bytes[range.clone()].try_into()?);
                bytes[range].copy_from_slice(&(old & !(0xfff << 10) | bits << 10).to_le_bytes());
            }
            Kind::TlsDesc(which) => {
                let offset = self.tp_offset(self.address_of(res)?)? + addend;
                let offset = u32::try_from(offset).context("a thread-local past 4 GB of the thread pointer")?;
                // As lld rewrites the sequence in an executable: `movz x0, #hi, lsl #16`,
                // `movk x0, #lo`, and no `add` or call.
                let code: u32 = match which {
                    0 => 0xd2a0_0000 | (offset >> 16) << 5,
                    1 => 0xf280_0000 | (offset & 0xffff) << 5,
                    _ => 0xd503_201f,
                };
                let range = field(Field::Distance)?;
                bytes[range].copy_from_slice(&code.to_le_bytes());
            }
            Kind::TlsGdX86 => {
                let offset = self.tp_offset(self.address_of(res)?)?;
                let offset = i32::try_from(offset).context("a thread-local too far from the thread pointer")?;
                let start = at.checked_sub(4).context("a thread-local at the start of a section")?;
                let code = bytes.get_mut(start..start + 16).context("a relocation past the image")?;
                let (lea, call) = Fixup::GENERAL;
                ensure!(
                    code[..4] == lea && code[8..12] == call,
                    "{}: a general-dynamic thread-local reached by code of another shape",
                    self.where_is(from.0, from.1, from.2)
                );
                code.copy_from_slice(&Fixup::local(offset));
            }
        }
        Ok(())
    }

    /// Copies the pieces in, applies their relocations, and writes the image's own sections,
    /// the headers and the build ID.
    fn fill(
        &mut self,
        bytes: &mut Vec<u8>,
        machine: Machine,
        request: &Request,
        entry: Res,
        phdrs: Vec<[u64; 7]>,
    ) -> anyhow::Result<()> {
        let mut load: Vec<(u64, u32, u32, i64)> = Vec::new();
        let mut load_from: FxHashSet<(u32, u32, u64)> = FxHashSet::default();
        // The pieces.
        for out in &self.outs {
            if out.sh_type == elf::SHT_NOBITS || out.synth.is_some() {
                continue;
            }
            for &(o, s) in &out.pieces {
                let sec = &self.link.objects[o as usize].sections[s as usize];
                let at = (out.address + sec.at) as usize;
                bytes[at..at + sec.data.len()].copy_from_slice(sec.data);
            }
        }
        for out_index in 0..self.outs.len() {
            if self.outs[out_index].synth.is_some() {
                continue;
            }
            for piece in 0..self.outs[out_index].pieces.len() {
                let (o, s) = self.outs[out_index].pieces[piece];
                let sec = &self.link.objects[o as usize].sections[s as usize];
                let base = self.outs[out_index].address + sec.at;
                let mut skip = None;
                for reloc in &sec.relocs {
                    if skip == Some(reloc.offset) {
                        skip = None;
                        continue;
                    }
                    let Some(kind) = kind(machine, reloc.r_type) else { bail!("a relocation the scan did not see") };
                    if kind == Kind::None {
                        continue;
                    }
                    if kind == Kind::TlsGdX86 {
                        skip = Some(reloc.offset + 8);
                    }
                    let from = (o, s, reloc.offset);
                    let res = self.resolve(reloc.to, from)?;
                    let before = load.len();
                    if sec.sh_type == elf::SHT_NOBITS {
                        // Only the loader writes there; the scan let in nothing else.
                        let place = base + reloc.offset;
                        match res {
                            Res::Import(id) => {
                                let r_type = match machine {
                                    Machine::X86_64 => elf::R_X86_64_64,
                                    Machine::Aarch64 => elf::R_AARCH64_ABS64,
                                };
                                load.push((place, r_type, self.import_of[&id], reloc.addend));
                            }
                            _ => {
                                let value = self.address_of(res)?.wrapping_add_signed(reloc.addend);
                                load.push((place, machine.relative(), 0, value as i64));
                            }
                        }
                        load_from.insert(from);
                        continue;
                    }
                    self.apply(bytes, reloc, kind, base + reloc.offset, res, &mut load, from)?;
                    if load.len() > before {
                        load_from.insert(from);
                    }
                }
            }
        }
        // `.eh_frame`: the kept records, each common entry's place fixed in its frames.
        let eh = self.out(Synth::EhFrame).context("no .eh_frame")?.address;
        let mut table: Vec<(u64, u64)> = Vec::new();
        for f in 0..self.frames.len() {
            let frame = &self.frames[f];
            if !frame.kept {
                continue;
            }
            let sec = &self.link.objects[frame.object as usize].sections[frame.section as usize];
            let to = (eh + frame.at) as usize;
            let data = &sec.data[frame.start as usize..(frame.start + frame.len) as usize];
            bytes[to..to + data.len()].copy_from_slice(data);
            if let Some(cie) = frame.cie {
                let pointer = (frame.at + 4) - self.frames[cie].at;
                bytes[to + 4..to + 8].copy_from_slice(&u32::try_from(pointer)?.to_le_bytes());
            }
            for reloc in within(&sec.relocs, frame.start, frame.start + frame.len) {
                let kind = kind(machine, reloc.r_type).context("a relocation the scan did not see")?;
                let place = eh + frame.at + (reloc.offset - frame.start);
                let from = (frame.object, frame.section, reloc.offset);
                let res = self.resolve(reloc.to, from)?;
                let before = load.len();
                self.apply(bytes, reloc, kind, place, res, &mut load, from)?;
                if load.len() > before {
                    load_from.insert(from);
                }
                if frame.cie.is_some() && reloc.offset == frame.start + 8 {
                    table.push((self.address_of(res)?.wrapping_add_signed(reloc.addend), eh + frame.at));
                }
            }
        }
        // The slots.
        let got = self.out(Synth::Got).context("no .got")?.address;
        for (index, &slot) in self.slots.iter().enumerate() {
            let place = got + index as u64 * 8;
            let value: u64 = match slot {
                Slot::Address(Res::Import(id)) => {
                    let r_type = match machine {
                        Machine::X86_64 => elf::R_X86_64_GLOB_DAT,
                        Machine::Aarch64 => elf::R_AARCH64_GLOB_DAT,
                    };
                    load.push((place, r_type, self.import_of[&id], 0));
                    0
                }
                Slot::Address(Res::Abs(value)) => value,
                Slot::Address(res) => {
                    let value = self.address_of(res)?;
                    load.push((place, machine.relative(), 0, value as i64));
                    value
                }
                Slot::TpOffset(res) => self.tp_offset(self.address_of(res)?)? as u64,
                Slot::Module(_) => {
                    let r_type = match machine {
                        Machine::X86_64 => elf::R_X86_64_DTPMOD64,
                        Machine::Aarch64 => elf::R_AARCH64_TLS_DTPMOD,
                    };
                    load.push((place, r_type, 0, 0));
                    0
                }
                Slot::Offset(res) => {
                    let (start, _, _) = self.tls.context("a thread-local, and the image has none")?;
                    self.address_of(res)? - start
                }
            };
            bytes[place as usize..place as usize + 8].copy_from_slice(&value.to_le_bytes());
        }
        // The stubs.
        if let Some(plt) = self.out(Synth::Plt) {
            let (size, _) = machine.stub();
            for (index, &id) in self.stubs.iter().enumerate() {
                let stub = plt.address + (index * size) as u64;
                let code = machine.stub_through(self.slot_address(Slot::Address(Res::Import(id)))?, stub)?;
                bytes[stub as usize..stub as usize + code.len()].copy_from_slice(&code);
            }
        }
        if load.len() != self.load_relocations {
            let name = |it: &FxHashSet<(u32, u32, u64)>, of: &FxHashSet<(u32, u32, u64)>| {
                let mut at: Vec<_> = it.difference(of).copied().collect();
                at.sort_unstable();
                at.iter().take(8).map(|&(o, s, offset)| self.where_is(o, s, offset)).collect::<Vec<_>>().join(", ")
            };
            bail!(
                "{} load-time relocations, where {} were counted; counted and not written: [{}]; \
                 written and not counted: [{}]",
                load.len(),
                self.load_relocations,
                name(&self.load_from, &load_from),
                name(&load_from, &self.load_from)
            );
        }
        // `.rela.dyn`: the relative ones first, which `DT_RELACOUNT` counts.
        let relative_type = machine.relative();
        load.sort_by_key(|it| (it.1 != relative_type, it.0));
        let relative_count = load.iter().filter(|it| it.1 == relative_type).count();
        let rela = self.out(Synth::RelaDyn).context("no .rela.dyn")?.address as usize;
        for (index, &(offset, r_type, symbol, addend)) in load.iter().enumerate() {
            let at = rela + index * 24;
            bytes[at..at + 8].copy_from_slice(&offset.to_le_bytes());
            let info = (u64::from(symbol) << 32) | u64::from(r_type);
            bytes[at + 8..at + 16].copy_from_slice(&info.to_le_bytes());
            bytes[at + 16..at + 24].copy_from_slice(&addend.to_le_bytes());
        }
        // `.eh_frame_hdr`.
        let hdr = self.out(Synth::EhFrameHdr).context("no .eh_frame_hdr")?.address;
        table.sort_unstable();
        let mut head = UnwindTable::ENCODINGS.to_vec();
        head.extend_from_slice(&relative(eh, hdr + 4)?.to_le_bytes());
        head.extend_from_slice(&u32::try_from(table.len())?.to_le_bytes());
        for (code, frame) in &table {
            head.extend_from_slice(&relative(*code, hdr)?.to_le_bytes());
            head.extend_from_slice(&relative(*frame, hdr)?.to_le_bytes());
        }
        bytes[hdr as usize..hdr as usize + head.len()].copy_from_slice(&head);
        // `.interp`, `.dynstr`, `.dynsym`, the versions and `.gnu.hash`.
        let put = |bytes: &mut Vec<u8>, at: u64, data: &[u8]| {
            bytes[at as usize..at as usize + data.len()].copy_from_slice(data);
        };
        let interp = self.out(Synth::Interp).context("no .interp")?.address;
        put(bytes, interp, request.interpreter.as_bytes());
        let dynstr = self.out(Synth::Dynstr).context("no .dynstr")?.address;
        put(bytes, dynstr, &self.dynstr.clone());
        let dynsym = self.out(Synth::Dynsym).context("no .dynsym")?.address;
        let mut versym = vec![0u8, 0];
        for (index, &id) in self.imports.iter().enumerate() {
            let global = &self.link.globals[id as usize];
            let bind = if global.strong_ref { elf::STB_GLOBAL } else { elf::STB_WEAK };
            let mut symbol = [0u8; 24];
            symbol[..4].copy_from_slice(&self.dynstr_of[global.name].to_le_bytes());
            symbol[4] = (bind << 4) | (global.shared_type & 0xf);
            put(bytes, dynsym + (index as u64 + 1) * 24, &symbol);
            let version = match global.def {
                Def::Shared { lib, version: Some(version) } => {
                    self.version_of.get(&(lib, version)).copied().unwrap_or(1)
                }
                _ => 1,
            };
            versym.extend_from_slice(&version.to_le_bytes());
        }
        let needs = self.version_needs();
        if let Some(out) = self.out(Synth::Version) {
            put(bytes, out.address, &versym);
        }
        if let Some(out) = self.out(Synth::VersionNeeded) {
            let mut table = Vec::new();
            for (n, (lib, versions)) in needs.iter().enumerate() {
                let next = if n + 1 == needs.len() { 0 } else { 16 + 16 * versions.len() as u32 };
                table.extend_from_slice(&1u16.to_le_bytes());
                table.extend_from_slice(&(versions.len() as u16).to_le_bytes());
                table.extend_from_slice(&self.dynstr_of[self.link.libs[*lib as usize].soname].to_le_bytes());
                table.extend_from_slice(&16u32.to_le_bytes());
                table.extend_from_slice(&next.to_le_bytes());
                for (v, &(version, index)) in versions.iter().enumerate() {
                    let (name, hash) = self.link.libs[*lib as usize].versions[version as usize];
                    table.extend_from_slice(&hash.to_le_bytes());
                    table.extend_from_slice(&0u16.to_le_bytes());
                    table.extend_from_slice(&index.to_le_bytes());
                    table.extend_from_slice(&self.dynstr_of[name].to_le_bytes());
                    let next: u32 = if v + 1 == versions.len() { 0 } else { 16 };
                    table.extend_from_slice(&next.to_le_bytes());
                }
            }
            ensure!(table.len() as u64 == out.size, "the size of .gnu.version_r");
            put(bytes, out.address, &table);
        }
        let hash = self.out(Synth::Hash).context("no .gnu.hash")?.address;
        // No symbol is exported: one bucket, empty, and a bloom filter that matches nothing.
        let mut gnu_hash = Vec::new();
        gnu_hash.extend_from_slice(&1u32.to_le_bytes());
        gnu_hash.extend_from_slice(&(self.imports.len() as u32 + 1).to_le_bytes());
        gnu_hash.extend_from_slice(&1u32.to_le_bytes());
        gnu_hash.extend_from_slice(&6u32.to_le_bytes());
        gnu_hash.extend_from_slice(&0u64.to_le_bytes());
        gnu_hash.extend_from_slice(&0u32.to_le_bytes());
        put(bytes, hash, &gnu_hash);
        // `.dynamic`.
        let address = |synth| self.out(synth).map(|it| it.address);
        let mut entries: Vec<(u32, u64)> = Vec::new();
        for &lib in &self.needed {
            entries.push((elf::DT_NEEDED, u64::from(self.dynstr_of[self.link.libs[lib as usize].soname])));
        }
        entries.push((elf::DT_GNU_HASH, hash));
        entries.push((elf::DT_STRTAB, dynstr));
        entries.push((elf::DT_SYMTAB, dynsym));
        entries.push((elf::DT_STRSZ, self.dynstr.len() as u64));
        entries.push((elf::DT_SYMENT, 24));
        entries.push((elf::DT_DEBUG, 0));
        entries.push((elf::DT_RELA, rela as u64));
        entries.push((elf::DT_RELASZ, load.len() as u64 * 24));
        entries.push((elf::DT_RELAENT, 24));
        entries.push((elf::DT_RELACOUNT, relative_count as u64));
        for (name, tag) in [("_init", elf::DT_INIT), ("_fini", elf::DT_FINI)] {
            if let Some((o, s, offset)) = self.defined_at(name) {
                entries.push((tag, self.address_of(Res::Sec { object: o, section: s, offset })?));
            }
        }
        for (name, tag, size) in [
            (".preinit_array", elf::DT_PREINIT_ARRAY, elf::DT_PREINIT_ARRAYSZ),
            (".init_array", elf::DT_INIT_ARRAY, elf::DT_INIT_ARRAYSZ),
            (".fini_array", elf::DT_FINI_ARRAY, elf::DT_FINI_ARRAYSZ),
        ] {
            if let Some(out) = self.named(name) {
                entries.push((tag, out.address));
                entries.push((size, out.size));
            }
        }
        if !self.version_of.is_empty() {
            entries.push((elf::DT_VERSYM, address(Synth::Version).context("no .gnu.version")?));
            entries.push((elf::DT_VERNEED, address(Synth::VersionNeeded).context("no .gnu.version_r")?));
            entries.push((elf::DT_VERNEEDNUM, needs.len() as u64));
        }
        entries.push((elf::DT_FLAGS, u64::from(elf::DF_BIND_NOW)));
        entries.push((elf::DT_FLAGS_1, u64::from(elf::DF_1_NOW | elf::DF_1_PIE)));
        entries.push((elf::DT_NULL, 0));
        let dynamic = self.out(Synth::Dynamic).context("no .dynamic")?;
        ensure!(entries.len() as u64 * 16 == dynamic.size, "the size of .dynamic");
        let dynamic = dynamic.address;
        for (index, (tag, value)) in entries.iter().enumerate() {
            let at = dynamic + index as u64 * 16;
            put(bytes, at, &u64::from(*tag).to_le_bytes());
            put(bytes, at + 8, &value.to_le_bytes());
        }
        // The ELF header and the program headers.
        let entry = self.address_of(entry)?;
        let mut ehdr = Vec::with_capacity(EHDR as usize);
        ehdr.extend_from_slice(&[0x7f, b'E', b'L', b'F', elf::ELFCLASS64, elf::ELFDATA2LSB, elf::EV_CURRENT, elf::ELFOSABI_NONE]);
        ehdr.extend_from_slice(&[0; 8]);
        ehdr.extend_from_slice(&elf::ET_DYN.to_le_bytes());
        let e_machine = match machine {
            Machine::X86_64 => elf::EM_X86_64,
            Machine::Aarch64 => elf::EM_AARCH64,
        };
        ehdr.extend_from_slice(&e_machine.to_le_bytes());
        ehdr.extend_from_slice(&u32::from(elf::EV_CURRENT).to_le_bytes());
        ehdr.extend_from_slice(&entry.to_le_bytes());
        ehdr.extend_from_slice(&EHDR.to_le_bytes());
        // e_shoff, filled in with the section headers.
        ehdr.extend_from_slice(&0u64.to_le_bytes());
        ehdr.extend_from_slice(&0u32.to_le_bytes());
        ehdr.extend_from_slice(&(EHDR as u16).to_le_bytes());
        ehdr.extend_from_slice(&(PHDR as u16).to_le_bytes());
        ehdr.extend_from_slice(&(phdrs.len() as u16).to_le_bytes());
        ehdr.extend_from_slice(&64u16.to_le_bytes());
        // e_shnum and e_shstrndx, filled in with the section headers.
        ehdr.extend_from_slice(&[0; 4]);
        put(bytes, 0, &ehdr);
        for (index, p) in phdrs.iter().enumerate() {
            let mut header = Vec::with_capacity(PHDR as usize);
            header.extend_from_slice(&(p[0] as u32).to_le_bytes());
            header.extend_from_slice(&(p[1] as u32).to_le_bytes());
            header.extend_from_slice(&p[2].to_le_bytes());
            header.extend_from_slice(&p[2].to_le_bytes());
            header.extend_from_slice(&p[2].to_le_bytes());
            header.extend_from_slice(&p[3].to_le_bytes());
            header.extend_from_slice(&p[4].to_le_bytes());
            header.extend_from_slice(&p[5].to_le_bytes());
            put(bytes, EHDR + index as u64 * PHDR, &header);
        }
        // The build ID: a hash of the image with the ID still zero.
        let note = self.out(Synth::BuildId).context("no build ID")?.address;
        let mut head = Vec::new();
        head.extend_from_slice(&4u32.to_le_bytes());
        head.extend_from_slice(&(BUILD_ID as u32).to_le_bytes());
        head.extend_from_slice(&elf::NT_GNU_BUILD_ID.to_le_bytes());
        head.extend_from_slice(b"GNU\0");
        put(bytes, note, &head);
        self.section_headers(bytes)?;
        let id = sha2::Sha256::digest(&bytes[..]);
        put(bytes, note + 16, &id[..BUILD_ID]);
        Ok(())
    }

    /// The section headers, with a symbol table of what is placed, behind everything loaded.
    fn section_headers(&self, bytes: &mut Vec<u8>) -> anyhow::Result<()> {
        let mut strtab = vec![0u8];
        let mut locals = Vec::new();
        let mut globals = Vec::new();
        for (o, obj) in self.link.objects.iter().enumerate() {
            for &(name, section, value, size, info) in &obj.symbols {
                let sec = &obj.sections[section as usize];
                if !sec.live || !sec.kept || sec.name == ".eh_frame" {
                    continue;
                }
                let bind = info >> 4;
                if bind != elf::STB_LOCAL {
                    let id = self.link.names[name];
                    let here = Def::At { object: o as u32, section, offset: value };
                    if self.link.globals[id as usize].def != here {
                        continue;
                    }
                }
                let mut address = self.outs[sec.out as usize].address + sec.at + value;
                if info & 0xf == elf::STT_TLS {
                    address -= self.tls.map_or(0, |it| it.0);
                }
                let mut symbol = [0u8; 24];
                symbol[..4].copy_from_slice(&(strtab.len() as u32).to_le_bytes());
                strtab.extend_from_slice(name.as_bytes());
                strtab.push(0);
                symbol[4] = info;
                // Hidden: nothing outside the image binds to its symbols.
                symbol[5] = if bind == elf::STB_LOCAL { 0 } else { elf::STV_HIDDEN };
                symbol[6..8].copy_from_slice(&((sec.out + 1) as u16).to_le_bytes());
                symbol[8..16].copy_from_slice(&address.to_le_bytes());
                symbol[16..24].copy_from_slice(&size.to_le_bytes());
                if bind == elf::STB_LOCAL { locals.push(symbol) } else { globals.push(symbol) }
            }
        }
        let first_global = locals.len() + 1;
        let mut symtab = vec![0u8; 24];
        for symbol in locals.iter().chain(&globals) {
            symtab.extend_from_slice(symbol);
        }
        let mut shstrtab = vec![0u8];
        let mut name = |text: &str| {
            let at = shstrtab.len() as u32;
            shstrtab.extend_from_slice(text.as_bytes());
            shstrtab.push(0);
            at
        };
        // [name, type, flags, address, offset, size, link, info, align, entsize]
        let mut headers: Vec<[u64; 10]> = vec![[0; 10]];
        let index_of = |synth: Synth| {
            self.outs.iter().position(|it| it.synth == Some(synth)).map_or(0, |it| it as u64 + 1)
        };
        for out in &self.outs {
            let (link, info, entsize) = match out.synth {
                Some(Synth::Dynsym) => (index_of(Synth::Dynstr), 1, 24),
                Some(Synth::Version) => (index_of(Synth::Dynsym), 0, 2),
                Some(Synth::VersionNeeded) => (index_of(Synth::Dynstr), self.version_needs().len() as u64, 0),
                Some(Synth::Hash) => (index_of(Synth::Dynsym), 0, 0),
                Some(Synth::RelaDyn) => (index_of(Synth::Dynsym), 0, 24),
                Some(Synth::Dynamic) => (index_of(Synth::Dynstr), 0, 16),
                Some(Synth::Got) => (0, 0, 8),
                _ => (0, 0, 0),
            };
            let offset = if out.sh_type == elf::SHT_NOBITS { out.address.min(bytes.len() as u64) } else { out.address };
            headers.push([
                u64::from(name(&out.name)),
                u64::from(out.sh_type),
                out.flags,
                out.address,
                offset,
                out.size,
                link,
                info,
                out.align,
                entsize,
            ]);
        }
        let symtab_at = bytes.len().next_multiple_of(8) as u64;
        let strtab_at = symtab_at + symtab.len() as u64;
        let shstrtab_name = name(".shstrtab");
        let symtab_index = headers.len() as u64;
        headers.push([u64::from(name(".symtab")), u64::from(elf::SHT_SYMTAB), 0, 0, symtab_at, symtab.len() as u64, symtab_index + 1, first_global as u64, 8, 24]);
        headers.push([u64::from(name(".strtab")), u64::from(elf::SHT_STRTAB), 0, 0, strtab_at, strtab.len() as u64, 0, 0, 1, 0]);
        let shstrtab_at = strtab_at + strtab.len() as u64;
        headers.push([u64::from(shstrtab_name), u64::from(elf::SHT_STRTAB), 0, 0, shstrtab_at, shstrtab.len() as u64, 0, 0, 1, 0]);
        let shoff = (shstrtab_at + shstrtab.len() as u64).next_multiple_of(8);
        bytes.resize(symtab_at as usize, 0);
        bytes.extend_from_slice(&symtab);
        bytes.extend_from_slice(&strtab);
        bytes.extend_from_slice(&shstrtab);
        bytes.resize(shoff as usize, 0);
        for h in &headers {
            for (i, field) in h.iter().enumerate() {
                // sh_name, sh_type, sh_link and sh_info are 32 bits.
                if matches!(i, 0 | 1 | 6 | 7) {
                    bytes.extend_from_slice(&(*field as u32).to_le_bytes());
                } else {
                    bytes.extend_from_slice(&field.to_le_bytes());
                }
            }
        }
        bytes[0x28..0x30].copy_from_slice(&shoff.to_le_bytes());
        bytes[0x3c..0x3e].copy_from_slice(&(headers.len() as u16).to_le_bytes());
        bytes[0x3e..0x40].copy_from_slice(&((headers.len() - 1) as u16).to_le_bytes());
        Ok(())
    }
}
