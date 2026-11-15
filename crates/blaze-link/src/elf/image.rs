//! The file around the placed regions: the ELF header and program headers at its start, and
//! after the last region the symbol table, the debug sections and the section headers.
//!
//! [`file`] reads the synthetic regions' bytes from [`Placed::contents`], where the core puts
//! what [`crate::model::Writer::synthesize`] returned, so no table is built twice. An
//! incremental output keeps the tail past the debug sections and patches it: each function's
//! local symbol and name keep their places, and a function come or gone takes or frees one
//! ([`KeptSymtab`]); the globals' run and the section headers are small and written whole.
//!
//! **The locals' order.** A tail made whole lists the functions' local symbols by a fixed hash
//! of their names, never by address ([`crate::model::local_order`], which says why); a patched
//! one keeps each where it was placed.
//!
//! **Floor and cost.** The floor of a patch is the entries and names of the functions that
//! changed: 24 bytes and a name each. A patch writes those, and the globals' symbols, names and
//! section headers whole into the kept bytes (`O(exports + imports + sections)`), which the
//! commit compares with the last file's so only changed words reach the file
//! (`crate::file`'s `narrowed`). Making the tail whole (a full link, an open whose state kept no
//! places, or the room used up) is `O(functions)`, once per eighth of the locals added.

use std::alloc::Global;

use blaze_format::cursor::Put;
use blaze_graph::par;
use object::elf;

use super::dynamic::{CHUNK, Strings, Sym, info, numbered, shndx, value};
use super::{
    BUILD_ID, DYNAMIC, DYNSTR, DYNSYM, EH_FRAME_HDR, EHDR, Elf, GNU_HASH, INTERP, PHDR, RELA_DYN,
    Segment, VERNEED, VERSYM, error,
};
use crate::cow::CowBytes;
use crate::model::{
    Class, File, Function, Placed, Plan, Region, Reserve, Splice, SymbolKind, zeroed,
};
use crate::{LinkError, OutputKind};

const SYM: usize = 24;
const SHDR: usize = 64;

/// The bytes a symbol name takes in `.strtab`: none for an empty one (offset 0).
fn stored(name: &str) -> usize {
    if name.is_empty() { 0 } else { name.len() + 1 }
}

/// A run of `.strtab` laid out beforehand: names written in order, each at the next offset,
/// its terminator already 0.
struct Names<'b> {
    bytes: &'b mut [u8],
    at: usize,
    /// The `.strtab` offset of `bytes[0]`.
    offset: u32,
}

impl Names<'_> {
    fn add(&mut self, name: &str) -> u32 {
        if name.is_empty() {
            return 0;
        }
        self.bytes[self.at..self.at + name.len()].copy_from_slice(name.as_bytes());
        let offset = self.offset + self.at as u32;
        self.at += name.len() + 1;
        offset
    }
}

/// One `PT_LOAD`: from `vaddr`, its bytes in the file to `file_end`, in memory to `end`.
struct Load {
    segment: Segment,
    vaddr: u64,
    file_end: u64,
    end: u64,
    /// A region not in the file was placed: what follows can't be either.
    zeroed: bool,
}

struct Phdr {
    p_type: elf::ProgramType,
    flags: u32,
    vaddr: u64,
    filesz: u64,
    memsz: u64,
    align: u64,
}

#[derive(Default)]
struct Shdr {
    name: u32,
    sh_type: elf::SectionType,
    flags: u64,
    addr: u64,
    offset: u64,
    size: u64,
    link: u32,
    info: u32,
    align: u64,
    entsize: u64,
}

impl Shdr {
    /// Its `Elf64_Shdr`.
    fn bytes(&self) -> [u8; SHDR] {
        let mut out = [0; SHDR];
        out[0..4].copy_from_slice(&self.name.to_le_bytes());
        out[4..8].copy_from_slice(&self.sh_type.0.to_le_bytes());
        out[8..16].copy_from_slice(&self.flags.to_le_bytes());
        out[16..24].copy_from_slice(&self.addr.to_le_bytes());
        out[24..32].copy_from_slice(&self.offset.to_le_bytes());
        out[32..40].copy_from_slice(&self.size.to_le_bytes());
        out[40..44].copy_from_slice(&self.link.to_le_bytes());
        out[44..48].copy_from_slice(&self.info.to_le_bytes());
        out[48..56].copy_from_slice(&self.align.to_le_bytes());
        out[56..64].copy_from_slice(&self.entsize.to_le_bytes());
        out
    }
}

pub(crate) fn section_name(class: &Class) -> &str {
    match class {
        Class::Text => ".text",
        Class::Patch => ".blaze.patch",
        Class::Stubs => blaze_live::patch::STUBS_SECTION_ELF,
        Class::Plt => ".plt",
        Class::Rodata => ".rodata",
        Class::RelRo => ".data.rel.ro",
        Class::ThreadVars => ".blaze.thread_vars",
        Class::EhFrame => ".eh_frame",
        Class::Data => ".data",
        Class::Table => ".blaze.table",
        Class::Got => ".got",
        Class::PreinitArray => super::PREINIT_ARRAY,
        Class::InitArray => ".init_array",
        Class::FiniArray => ".fini_array",
        Class::Reserve(Reserve::Arena) => ".blaze.reserve.arena",
        Class::Reserve(Reserve::Cells) => ".blaze.reserve.cells",
        Class::Reserve(Reserve::Fixed) => ".blaze.reserve.fixed",
        Class::PatchData => ".blaze.patch_data",
        Class::Named { name, .. } => name.as_str(),
        Class::Tdata => ".tdata",
        Class::Tbss => ".tbss",
        Class::Bss => ".bss",
        Class::Synthetic(name) => *name,
    }
}

/// The `Elf64_Verneed` entries of `.gnu.version_r`, its section's `sh_info`: the `vn_next` chain.
fn verneeds(bytes: &[u8]) -> u32 {
    let (mut count, mut at) = (0, 0usize);
    while let Some(next) = bytes.get(at + 12..at + 16) {
        count += 1;
        let next = u32::from_le_bytes(next.try_into().expect("4 bytes"));
        if next == 0 {
            break;
        }
        at += next as usize;
    }
    count
}

/// Where the tail's tables are, from the tail's start (`tail`) past `len` bytes of debug
/// sections: two tails of one layout put every symbol and name at the same offset.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
struct Layout {
    tail: u64,
    len: usize,
    /// The named functions and the bytes their names take in `.strtab`.
    named: usize,
    function_names: usize,
    /// The bytes the exports' and imports' names take, and how many there are.
    global_names: usize,
    globals: usize,
    shstrtab_len: usize,
    shdrs: usize,
}

impl Layout {
    fn symtab(&self) -> usize {
        self.len.next_multiple_of(8)
    }

    fn strtab(&self) -> usize {
        self.symtab() + (1 + self.named + self.globals) * SYM
    }

    fn strtab_len(&self) -> usize {
        1 + self.function_names + self.global_names
    }

    fn shstrtab(&self) -> usize {
        self.strtab() + self.strtab_len()
    }

    fn shoff(&self) -> usize {
        (self.shstrtab() + self.shstrtab_len).next_multiple_of(8)
    }

    /// The tail's bytes from `len`: what [`file`] makes of it.
    fn size(&self) -> usize {
        self.shoff() + self.shdrs * SHDR - self.len
    }
}

/// The empty room an incremental output's `.symtab` keeps past its live locals (`least` 16
/// entries) and `.strtab` past their names (`least` 1 KiB): an eighth of what is live and
/// `least`, so a patch that adds a function finds a place for it, and the tables are made whole
/// again (with new room) only once an eighth of them came since.
fn spare_room(live: usize, least: usize) -> usize {
    live / 8 + least
}

/// The tail past the debug sections as [`file`] made it, kept to patch the next.
///
/// **Places.** An incremental output's locals keep their places for their lives: a function's
/// local symbol keeps its `.symtab` index and its name its run of `.strtab` from the link that
/// placed it until the tail is made whole. A function gone leaves its entry zeroed (the null
/// symbol's bytes: `STT_NOTYPE`, `SHN_UNDEF`, no name, which the `backtrace` crate's symboliser
/// skips, as ix-prof's does, reading only text symbols at an address) and its name's run free;
/// a function come takes the lowest free entry and the smallest free run its name fits
/// ([`KeptSymtab::place_locals`]). The names' runs freed are not merged, so `.strtab` holds
/// dead bytes: at most the room it was made with, since a name that fits no run makes the tail
/// whole. So a patched tail is history, not a function of the image: it is kept with the state
/// ([`KeptSymtab::keep`]), and a tail made whole lists the locals in name order
/// ([`crate::model::local_order`]) with the room after them, the canonical form a full link
/// writes. A patched file is a full link's but for where each local is: the same symbols.
#[derive(Default)]
pub(crate) struct KeptSymtab {
    synthesis: u64,
    layout: Layout,
    /// By function, in [`Placed::functions`] order.
    locals: Vec<Local>,
    /// The `.symtab` indexes of the locals' entries that are empty.
    free: std::collections::BTreeSet<u32>,
    /// The runs of `.strtab`'s locals' names that no name holds, as (length, offset).
    holes: std::collections::BTreeSet<(u32, u32)>,
    /// What the last link's file holds too.
    bytes: CowBytes,
    /// The globals' symbols and the run from their names to the section headers, as a patch
    /// writes them, reused.
    scratch: Vec<u8>,
    /// How many builds were patches.
    #[cfg(test)]
    pub(crate) patched: u32,
}

/// A function's local symbol: its index in `.symtab` (0 for an unnamed function, which has
/// none), its name's offset in `.strtab` and the bytes the name takes there.
#[derive(Clone, Copy, Default, Debug)]
struct Local {
    sym: u32,
    name: u32,
    stored: u32,
}

impl KeptSymtab {
    /// Places the locals of the functions `splices` put in, in the kept tail of the last link:
    /// a function named as one the splice took out takes that one's entry and name (rewritten
    /// with its value, size and section); a local taken out and not put back is zeroed and
    /// freed; a function named anew takes the lowest free entry and the smallest free run of
    /// `.strtab` its name fits, its name written there. The frees come before the places, so a
    /// rename takes the entry the old name left. False when the splices are not of the kept
    /// list or the room is used up; the tail is then made whole, which resets every place, so
    /// what this changed before it gave up is never read.
    fn place_locals(&mut self, regions: &[Region], functions: &[Function], splices: &[Splice]) -> bool {
        let (symtab, strtab) = (self.layout.symtab() - self.layout.len, self.layout.strtab() - self.layout.len);
        let (mut old_end, mut len) = (0, self.locals.len());
        for splice in splices {
            let (old, new) = (&splice.old, &splice.new);
            if old.start < old_end
                || old.end < old.start
                || old.end as usize > self.locals.len()
                || new.end < new.start
                || new.end as usize > functions.len()
            {
                return false;
            }
            old_end = old.end;
            len = len - old.len() + new.len();
        }
        if len != functions.len() {
            return false;
        }
        const UNSET: u32 = u32::MAX;
        let (mut taken, mut gone) = (Vec::new_in(Global), Vec::new_in(Global));
        for splice in splices {
            let old = &self.locals[splice.old.start as usize..splice.old.end as usize];
            let new = &functions[splice.new.start as usize..splice.new.end as usize];
            let bytes = &self.bytes;
            // The splice's old locals by name, read where they are in the kept `.strtab`.
            let mut by_name = Vec::with_capacity_in(old.len(), Global);
            by_name.extend((0..old.len()).filter(|at| old[*at].sym != 0).map(|at| {
                let local = old[at];
                (bytes.read(strtab + local.name as usize, (local.stored as usize).saturating_sub(1)), at)
            }));
            by_name.sort_unstable_by(|a, b| a.0.cmp(&b.0));
            let mut used = Vec::with_capacity_in(old.len(), Global);
            used.resize(old.len(), false);
            for function in new {
                let Some(name) = function.name.as_deref() else {
                    taken.push(Local::default());
                    continue;
                };
                let first = by_name.partition_point(|it| &*it.0 < name.as_bytes());
                let found = (by_name[first..].iter())
                    .take_while(|it| &*it.0 == name.as_bytes())
                    .find(|it| !used[it.1])
                    .map(|it| it.1);
                match found {
                    Some(at) => {
                        used[at] = true;
                        taken.push(old[at]);
                    }
                    None => taken.push(Local { sym: UNSET, name: 0, stored: stored(name) as u32 }),
                }
            }
            gone.extend((0..old.len()).filter(|at| old[*at].sym != 0 && !used[*at]).map(|at| old[at]));
        }
        for local in &gone {
            self.free.insert(local.sym);
            if local.stored != 0 {
                self.holes.insert((local.stored, local.name));
            }
            self.bytes.update(symtab + SYM * local.sym as usize, &[0; SYM]);
        }
        let mut at = 0;
        for splice in splices {
            for function in &functions[splice.new.start as usize..splice.new.end as usize] {
                let local = &mut taken[at];
                at += 1;
                if local.sym != UNSET {
                    continue;
                }
                let Some(sym) = self.free.pop_first() else { return false };
                local.sym = sym;
                if local.stored == 0 {
                    continue;
                }
                let Some(&(run, offset)) = self.holes.range((local.stored, 0)..).next() else { return false };
                self.holes.remove(&(run, offset));
                if run > local.stored {
                    self.holes.insert((run - local.stored, offset + local.stored));
                }
                local.name = offset;
                let name = function.name.as_deref().unwrap_or_default();
                self.bytes.update(strtab + offset as usize, name.as_bytes());
                self.bytes.update(strtab + offset as usize + name.len(), &[0]);
            }
        }
        // Back to front, so each splice's place in the kept list is still the last link's.
        let mut end = taken.len();
        for splice in splices.iter().rev() {
            let start = end - splice.new.len();
            self.locals.splice(splice.old.start as usize..splice.old.end as usize, taken[start..end].iter().copied());
            end = start;
        }
        for splice in splices {
            for at in splice.new.start as usize..splice.new.end as usize {
                let (function, local) = (&functions[at], self.locals[at]);
                if local.sym != 0 {
                    let symbol = local_symbol(regions, function, local.name);
                    self.bytes.update(symtab + SYM * local.sym as usize, &symbol.bytes());
                }
            }
        }
        true
    }

    /// Whether it holds no tail.
    pub(crate) fn is_empty(&self) -> bool {
        self.synthesis == 0
    }

    /// The tail's places for a kept state, when they are of the state's synthesis `of`: its
    /// layout, each function's local, the free entries and the free runs of names. The bytes
    /// are the state's part, not kept twice.
    pub(crate) fn keep<A: std::alloc::Allocator>(&self, of: u64, out: &mut crate::keep::Out<A>) {
        let held = self.synthesis != 0 && self.synthesis == of;
        out.bool(held);
        if !held {
            return;
        }
        let layout = &self.layout;
        out.u64(layout.tail);
        for it in [
            layout.len,
            layout.named,
            layout.function_names,
            layout.global_names,
            layout.globals,
            layout.shstrtab_len,
            layout.shdrs,
        ] {
            out.len(it);
        }
        out.len(self.locals.len());
        for local in &self.locals {
            out.u32(local.sym);
            out.u32(local.name);
            out.u32(local.stored);
        }
        out.len(self.free.len());
        for sym in &self.free {
            out.u32(*sym);
        }
        out.len(self.holes.len());
        for &(run, offset) in &self.holes {
            out.u32(run);
            out.u32(offset);
        }
    }

    /// The places [`KeptSymtab::keep`] wrote, as of the opened state's synthesis `synthesis`,
    /// over the opened state's tail part among `parts` (shared with it until a patch writes a
    /// page). Every entry is held by one local or free, every name and free run is within the
    /// locals' names, and the part is the layout's length: else the state is refused.
    pub(crate) fn open(
        input: &mut crate::keep::In<'_>,
        synthesis: u64,
        parts: &[(u64, CowBytes)],
    ) -> Result<KeptSymtab, crate::keep::Bad> {
        use crate::keep::Bad;
        if !input.bool()? {
            return Ok(KeptSymtab::default());
        }
        let tail = input.u64()?;
        let mut sizes = [0usize; 7];
        for it in &mut sizes {
            *it = input.len()?;
        }
        let [len, named, function_names, global_names, globals, shstrtab_len, shdrs] = sizes;
        let layout = Layout { tail, len, named, function_names, global_names, globals, shstrtab_len, shdrs };
        let count = input.count(12)?;
        let mut locals = Vec::with_capacity_in(count, Global);
        for _ in 0..count {
            locals.push(Local { sym: input.u32()?, name: input.u32()?, stored: input.u32()? });
        }
        let mut free = std::collections::BTreeSet::new_in(Global);
        for _ in 0..input.count(4)? {
            free.insert(input.u32()?);
        }
        let mut holes = std::collections::BTreeSet::new_in(Global);
        for _ in 0..input.count(8)? {
            holes.insert((input.u32()?, input.u32()?));
        }
        let Some((_, bytes)) = parts.iter().find(|it| it.0 == tail + len as u64) else {
            return Err(Bad::What("`.symtab` places kept for a state with no symbol table part"));
        };
        if bytes.len() != layout.size() {
            return Err(Bad::What("a symbol table part not of its kept layout's length"));
        }
        // Every entry of the locals' run held once, by a local or as free.
        let names_end = 1 + function_names as u64;
        let mut held = std::collections::BTreeSet::new_in(Global);
        for local in locals.iter().filter(|it| it.sym != 0) {
            if local.sym as usize > named
                || free.contains(&local.sym)
                || !held.insert(local.sym)
                || u64::from(local.name) + u64::from(local.stored) > names_end
            {
                return Err(Bad::What("a local's `.symtab` entry past the locals, held twice, or its name past theirs"));
            }
        }
        if free.first() == Some(&0) || free.last().is_some_and(|it| *it as usize > named) || held.len() + free.len() != named {
            return Err(Bad::What("`.symtab`'s free entries not the locals' entries no local holds"));
        }
        if holes.iter().any(|&(run, offset)| offset == 0 || u64::from(offset) + u64::from(run) > names_end) {
            return Err(Bad::What("a free run of `.strtab` past the locals' names"));
        }
        // clone: two reference counts, the tail shared with the opened state's part
        Ok(KeptSymtab { synthesis, layout, locals, free, holes, bytes: bytes.clone(), ..KeptSymtab::default() })
    }
}

fn local_symbol(regions: &[Region], function: &Function, name: u32) -> Sym {
    Sym {
        name,
        info: info(elf::STB_LOCAL, SymbolKind::Function),
        shndx: shndx(regions, function.addr),
        value: function.addr,
        size: function.size,
    }
}

/// Where the exports' and imports' symbols start in `.symtab`, and where their names start in
/// `.strtab`, from `layout.len`.
fn globals_at(layout: &Layout) -> (usize, usize) {
    (
        layout.symtab() - layout.len + SYM * (1 + layout.named),
        layout.strtab() - layout.len + 1 + layout.function_names,
    )
}

/// The cheap part of the tail, written whole on every link: the exports' and imports'
/// symbols into `syms`, which runs from the first of them to `.strtab`, and into `rest`,
/// which runs from their first name to the tail's end, their names, `.shstrtab` and the
/// section headers. `rest`'s other bytes stay as they are, zeros.
fn globals_and_headers(
    placed: &Placed<'_>,
    layout: &Layout,
    syms: &mut [u8],
    rest: &mut [u8],
    shstrtab: &[u8],
    shdrs: &[Shdr],
) -> Result<(), LinkError> {
    let regions = placed.regions;
    let base = layout.strtab() + 1 + layout.function_names;
    let strs = &mut rest[..layout.strtab_len() - 1 - layout.function_names];
    strs.fill(0);
    let mut strings = Names { bytes: strs, at: 0, offset: (1 + layout.function_names) as u32 };
    let mut out = syms.chunks_exact_mut(SYM);
    for export in placed.exports {
        let bind = if export.weak { elf::STB_WEAK } else { elf::STB_GLOBAL };
        let symbol = Sym {
            name: strings.add(&export.name),
            info: info(bind, export.kind),
            shndx: shndx(regions, export.addr),
            value: value(placed, &export.name, export.addr, export.kind)?,
            size: export.size,
        };
        out.next().expect("a symbol counted").copy_from_slice(&symbol.bytes());
    }
    for import in placed.imports {
        let bind = if import.weak { elf::STB_WEAK } else { elf::STB_GLOBAL };
        let symbol = Sym {
            name: strings.add(&import.name),
            info: info(bind, import.kind),
            ..Sym::default()
        };
        out.next().expect("a symbol counted").copy_from_slice(&symbol.bytes());
    }
    rest[layout.shstrtab() - base..][..shstrtab.len()].copy_from_slice(shstrtab);
    for (shdr, out) in shdrs.iter().zip(rest[layout.shoff() - base..].chunks_exact_mut(SHDR)) {
        out.copy_from_slice(&shdr.bytes());
    }
    Ok(())
}

/// What a dev run's start hook needs of the `PT_LOAD`s (`blaze_inproc::image` reads them, and
/// a run overlays whole pages of code): each code segment starts on a page in the file and
/// in memory, shares no page with another segment (the headers' included), is never writable
/// and is all file bytes; each reservation is whole pages with no file bytes, after every
/// other segment and in [`Reserve`] order. The layout makes all of it, so an error here
/// names a layout bug; the image is never written otherwise.
fn dev_run(loads: &[Load], plan: &Plan) -> Result<(), LinkError> {
    let page = plan.page;
    let mut last_reserve: Option<Reserve> = None;
    for (at, load) in loads.iter().enumerate() {
        let (start, end) = (load.vaddr, load.end);
        let refuse = |why: &str| -> Result<(), LinkError> {
            // alloc: error path
            Err(error(format!(
                "a dev run's {:?} segment at {start:#x}..{end:#x} {why}",
                load.segment
            )))
        };
        match load.segment {
            Segment::Code => {
                if !start.is_multiple_of(page) || !(start - plan.base).is_multiple_of(page) {
                    return refuse(&format!(
                        "does not start on a {page:#x} page in the file and in memory"
                    )); // alloc: error path
                }
                if load.segment.flags() & elf::PF_W.0 != 0 {
                    return refuse("is writable");
                }
                if load.zeroed || load.file_end != end {
                    return refuse("is not all file bytes");
                }
                let before = at
                    .checked_sub(1)
                    .map(|it| &loads[it])
                    .filter(|it| it.end.next_multiple_of(page) > start);
                let after = loads.get(at + 1).filter(|it| it.vaddr < end.next_multiple_of(page));
                if let Some(other) = before.or(after) {
                    // alloc: error path
                    return refuse(&format!(
                        "shares a {page:#x} page with the {:?} segment at {:#x}",
                        other.segment, other.vaddr
                    ));
                }
            }
            Segment::Reserve(reserve) => {
                if last_reserve.is_some_and(|it| it >= reserve) {
                    return refuse("is out of the arena, cells, fixed order");
                }
                last_reserve = Some(reserve);
                if end == start
                    || !start.is_multiple_of(page)
                    || !(end - start).is_multiple_of(page)
                {
                    return refuse(&format!("is not whole {page:#x} pages")); // alloc: error path
                }
                if load.file_end != start {
                    return refuse("has file bytes");
                }
            }
            _ if last_reserve.is_some() => return refuse("follows a reservation"),
            Segment::Read | Segment::Relro | Segment::Write => {}
        }
    }
    Ok(())
}

pub(crate) fn file(writer: &Elf, placed: &Placed<'_>) -> Result<File, LinkError> {
    let plan = placed.plan;
    let regions = placed.regions;
    let entry =
        match &writer.output.kind {
            OutputKind::Executable { entry } => {
                Some(placed.entry.ok_or_else(|| {
                    error(format!("the executable's entry `{entry}` is not placed"))
                })?) // alloc: error path
            }
            OutputKind::Shared { .. } => placed.entry,
        };

    // At most one per segment: a second run of one is an error below.
    let mut loads: Vec<Load> = Vec::with_capacity((4 + super::RESERVES) as usize);
    for region in regions {
        let segment = Segment::of(&region.class);
        let end = region.addr + region.capacity.max(region.size);
        let file = !zeroed(&region.class);
        match loads.last_mut() {
            Some(load) if load.segment == segment => {
                if file && load.zeroed {
                    // alloc: error path
                    return Err(error(format!(
                        "{:?} at {:#x} is in the file but follows zeroed memory in its segment",
                        region.class, region.addr
                    )));
                }
                if file {
                    load.file_end = end;
                }
                load.zeroed |= !file;
                load.end = end;
            }
            _ => {
                if loads.iter().any(|it| it.segment == segment) {
                    // alloc: error path
                    return Err(error(format!(
                        "{:?} at {:#x} splits the {segment:?} segment",
                        region.class, region.addr
                    )));
                }
                let vaddr = if loads.is_empty() { plan.base } else { region.addr };
                if loads.is_empty() && segment != Segment::Read {
                    // alloc: error path
                    return Err(error(format!(
                        "the image starts with {:?}, not the headers' segment",
                        region.class
                    )));
                }
                if vaddr % plan.page != 0 {
                    // alloc: error path
                    return Err(error(format!(
                        "the {segment:?} segment starts at {vaddr:#x}, not on a {:#x} page",
                        plan.page
                    )));
                }
                loads.push(Load {
                    segment,
                    vaddr,
                    file_end: if file { end } else { vaddr },
                    end,
                    zeroed: !file,
                });
            }
        }
    }
    if loads.is_empty() {
        return Err(error("an image with no regions".into()));
    }
    match writer.output.dev_run {
        Some(_) => dev_run(&loads, plan)?,
        None => {
            if let Some(region) = regions.iter().find(|it| matches!(it.class, Class::Reserve(_))) {
                // alloc: error path
                return Err(error(format!(
                    "{:?} at {:#x} in an image that is not a dev run",
                    region.class, region.addr
                )));
            }
        }
    }

    // Each synthetic region's address and how many bytes `synthesize` gave it.
    let synthetic = |name: &'static str| {
        placed
            .region(&Class::Synthetic(name))
            .map(|(index, it)| (it.addr, placed.len(index) as u64))
    };
    let room = writer.phdrs();
    let mut phdrs = Vec::with_capacity(room as usize);
    phdrs.push(Phdr {
        p_type: elf::PT_PHDR,
        flags: 4,
        vaddr: plan.base + EHDR,
        filesz: 0,
        memsz: 0,
        align: 8,
    });
    if let Some((addr, size)) = synthetic(INTERP) {
        phdrs.push(Phdr {
            p_type: elf::PT_INTERP,
            flags: 4,
            vaddr: addr,
            filesz: size,
            memsz: size,
            align: 1,
        });
    }
    // The loader protects relro in whole pages of the running kernel's size, rounding its end
    // down (glibc's `_dl_protect_relro`), and `mprotect` of a page the image doesn't map fails
    // the start with ENOMEM. So relro ends on the next segment's page, a multiple of every page
    // size the architecture runs with (64 KiB on AArch64, whose kernels run 4, 16 or 64 KiB
    // pages), and its `PT_LOAD` maps zeroed memory up to there (lld's `.relro_padding`): on a
    // 4 KiB kernel the pages between its last byte and that boundary are in no mapping
    // otherwise. With nothing after it, it ends at its last byte.
    let relro = loads.iter().position(|it| it.segment == Segment::Relro).map(|at| {
        let load = &loads[at];
        let end =
            if at + 1 < loads.len() { load.end.next_multiple_of(plan.page) } else { load.end };
        (at, end)
    });
    for (at, load) in loads.iter().enumerate() {
        let end = match relro {
            Some((relro, end)) if relro == at => end,
            _ => load.end,
        };
        phdrs.push(Phdr {
            p_type: elf::PT_LOAD,
            flags: load.segment.flags(),
            vaddr: load.vaddr,
            filesz: load.file_end - load.vaddr,
            memsz: end - load.vaddr,
            align: plan.page,
        });
    }
    if let Some((addr, size)) = synthetic(DYNAMIC) {
        phdrs.push(Phdr {
            p_type: elf::PT_DYNAMIC,
            flags: 6,
            vaddr: addr,
            filesz: size,
            memsz: size,
            align: 8,
        });
    }
    if let Some((at, end)) = relro {
        let load = &loads[at];
        let (filesz, memsz) = (load.file_end - load.vaddr, end - load.vaddr);
        phdrs.push(Phdr {
            p_type: elf::PT_GNU_RELRO,
            flags: 4,
            vaddr: load.vaddr,
            filesz,
            memsz,
            align: 1,
        });
    }
    if let (Some(_), Some((addr, size))) = (placed.region(&Class::EhFrame), synthetic(EH_FRAME_HDR))
    {
        phdrs.push(Phdr {
            p_type: elf::PT_GNU_EH_FRAME,
            flags: 4,
            vaddr: addr,
            filesz: size,
            memsz: size,
            align: 4,
        });
    }
    phdrs.push(Phdr {
        p_type: elf::PT_GNU_STACK,
        flags: 6,
        vaddr: 0,
        filesz: 0,
        memsz: 0,
        align: 0,
    });
    if let Some(tls) = placed.tls {
        phdrs.push(Phdr {
            p_type: elf::PT_TLS,
            flags: 4,
            vaddr: tls.addr,
            filesz: tls.init,
            memsz: tls.size,
            align: tls.align.max(1),
        });
    }
    if let Some((addr, size)) = synthetic(BUILD_ID) {
        phdrs.push(Phdr {
            p_type: elf::PT_NOTE,
            flags: 4,
            vaddr: addr,
            filesz: size,
            memsz: size,
            align: 4,
        });
    }
    if phdrs.len() as u64 > room {
        return Err(error(format!(
            "{} program headers, more than the {room} the headers have room for",
            phdrs.len()
        ))); // alloc: error path
    }
    let phnum = phdrs.len() as u64;
    phdrs[0].filesz = phnum * PHDR;
    phdrs[0].memsz = phnum * PHDR;

    // Section headers: the null one, one per region in address order (`shndx` counts on it),
    // then those outside the address space.
    numbered(regions)?;
    let index_of = |class: &Class| {
        regions.iter().position(|it| &it.class == class).map_or(0, |it| it as u32 + 1)
    };
    let shdr_count = 1 + regions.len() + placed.debug.len() + 3;
    let mut names = Strings::with_capacity(
        regions.iter().map(|it| section_name(&it.class).len() + 1).sum::<usize>()
            + placed.debug.iter().map(|(name, _)| name.len() + 1).sum::<usize>()
            + ".symtab\0.strtab\0.shstrtab\0".len(),
        shdr_count,
    );
    let mut shdrs = Vec::with_capacity(shdr_count);
    shdrs.push(Shdr::default());
    for (index, region) in regions.iter().enumerate() {
        let class = &region.class;
        let segment = Segment::of(class);
        let mut flags = elf::SHF_ALLOC.0;
        if segment.loader_writes() {
            flags |= elf::SHF_WRITE.0;
        }
        if segment == Segment::Code {
            flags |= elf::SHF_EXECINSTR.0;
        }
        if matches!(class, Class::Tdata | Class::Tbss) {
            flags |= elf::SHF_TLS.0;
        }
        let mut shdr = Shdr {
            name: names.add(section_name(class)),
            sh_type: elf::SHT_PROGBITS,
            flags,
            addr: region.addr,
            offset: region.addr - plan.base,
            size: region.size,
            align: region.align.max(1),
            ..Shdr::default()
        };
        match class {
            Class::Tbss | Class::Bss => shdr.sh_type = elf::SHT_NOBITS,
            // No access, so not allocated: nothing of the image is there until the start hook
            // maps it.
            Class::Reserve(_) => {
                (shdr.sh_type, shdr.flags, shdr.size) =
                    (elf::SHT_NOBITS, 0, region.capacity.max(region.size))
            }
            Class::PreinitArray => (shdr.sh_type, shdr.entsize) = (elf::SHT_PREINIT_ARRAY, 8),
            Class::InitArray => (shdr.sh_type, shdr.entsize) = (elf::SHT_INIT_ARRAY, 8),
            Class::FiniArray => (shdr.sh_type, shdr.entsize) = (elf::SHT_FINI_ARRAY, 8),
            Class::Got => shdr.entsize = 8,
            Class::EhFrame if writer.output.arch == blaze_codegen::Arch::X86_64 => {
                shdr.sh_type = elf::SHT_X86_64_UNWIND
            }
            Class::Synthetic(name) => {
                shdr.size = placed.len(index) as u64;
                match *name {
                    BUILD_ID => shdr.sh_type = elf::SHT_NOTE,
                    DYNSYM => {
                        (shdr.sh_type, shdr.entsize) = (elf::SHT_DYNSYM, 24);
                        (shdr.link, shdr.info) = (index_of(&Class::Synthetic(DYNSTR)), 1);
                    }
                    DYNSTR => shdr.sh_type = elf::SHT_STRTAB,
                    GNU_HASH => {
                        (shdr.sh_type, shdr.link) =
                            (elf::SHT_GNU_HASH, index_of(&Class::Synthetic(DYNSYM)))
                    }
                    VERSYM => {
                        (shdr.sh_type, shdr.entsize) = (elf::SHT_GNU_VERSYM, 2);
                        shdr.link = index_of(&Class::Synthetic(DYNSYM));
                    }
                    VERNEED => {
                        shdr.sh_type = elf::SHT_GNU_VERNEED;
                        (shdr.link, shdr.info) =
                            (index_of(&Class::Synthetic(DYNSTR)), verneeds(placed.bytes(index)));
                    }
                    RELA_DYN => {
                        (shdr.sh_type, shdr.entsize) = (elf::SHT_RELA, 24);
                        shdr.link = index_of(&Class::Synthetic(DYNSYM));
                    }
                    DYNAMIC => {
                        (shdr.sh_type, shdr.entsize) = (elf::SHT_DYNAMIC, 16);
                        shdr.link = index_of(&Class::Synthetic(DYNSTR));
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        shdrs.push(shdr);
    }

    // `.symtab`: the functions' locals (for backtraces and debuggers), then the exports and
    // the imports. Each name has its own place in `.strtab`, unshared as mold writes it. A
    // tail made whole lists the locals by their names' hash (the module doc's order), laid out
    // by prefix sums over chunks of functions and filled in parallel, straight into the file's
    // tail, an incremental output's with room after them ([`room`]). An incremental output
    // keeps the tail and patches it while its layout holds: each local keeps its place, and a
    // function come or gone takes or frees one ([`KeptSymtab::place_locals`]).
    let global_names = placed.exports.iter().map(|it| stored(&it.name)).sum::<usize>()
        + placed.imports.iter().map(|it| stored(&it.name)).sum::<usize>();
    let globals = placed.exports.len() + placed.imports.len();
    let tail = regions
        .iter()
        .filter(|it| !zeroed(&it.class))
        .map(|it| it.addr - plan.base + it.capacity.max(it.size))
        .fold(plan.headers, u64::max)
        .next_multiple_of(8);
    let mut len = 0usize;
    for (name, data) in placed.debug {
        shdrs.push(Shdr {
            name: names.add(name),
            sh_type: elf::SHT_PROGBITS,
            offset: tail + len as u64,
            size: data.len() as u64,
            align: 1,
            ..Shdr::default()
        });
        len += data.len();
    }
    // The last three sections' names, added now so the layout is known before the locals are
    // placed: nothing is added to `.shstrtab` after them.
    let (symtab_name, strtab_name, shstrtab_name) =
        (names.add(".symtab"), names.add(".strtab"), names.add(".shstrtab"));
    let shdr_count = shdrs.len() + 3;

    let mut last = writer.last();
    let mut kept = last.as_deref_mut().map(|it| &mut it.symtab);
    let from = kept.as_deref_mut().map_or(0, |it| std::mem::replace(&mut it.synthesis, 0));
    let splices = placed.delta.map_or(&[][..], |it| it.functions);
    let patched = match (kept.as_deref_mut(), placed.delta) {
        (Some(kept), Some(delta)) => {
            let held = &kept.layout;
            from != 0
                && delta.from == from
                && delta.same_places
                && (held.tail, held.len, held.global_names, held.globals, held.shstrtab_len, held.shdrs)
                    == (tail, len, global_names, globals, names.bytes.len(), shdr_count)
                && kept.bytes.len() == held.size()
                && kept.place_locals(regions, placed.functions, splices)
        }
        _ => false,
    };
    let (order, chunks, named, function_names) = match kept.as_deref() {
        Some(kept) if patched => (Vec::new_in(Global), Vec::new_in(Global), kept.layout.named, kept.layout.function_names),
        _ => {
            // Every named function's index, in `.symtab`'s order of locals
            // ([`crate::model::local_order`]); by chunk of [`CHUNK`] of them, how many there are
            // and the bytes their names take.
            let order = crate::model::local_order(placed.functions, |_| true, Global);
            let chunks = par::map(par::chunks(&order, CHUNK), |chunk| {
                let len: usize =
                    (chunk.iter().filter_map(|at| placed.functions[*at as usize].name.as_deref())).map(stored).sum();
                (chunk.len(), len)
            });
            let live: usize = chunks.iter().map(|it| it.0).sum();
            let live_names: usize = chunks.iter().map(|it| it.1).sum();
            let (named, function_names) = match writer.output.incremental {
                true => (live + spare_room(live, 16), live_names + spare_room(live_names, 1024)),
                false => (live, live_names),
            };
            (order, chunks, named, function_names)
        }
    };
    let symbols = 1 + named + globals;
    let strtab_len = 1 + function_names + global_names;
    if u32::try_from(strtab_len).is_err() {
        return Err(error(format!(
            "{strtab_len} bytes of symbol names, more than .strtab offsets reach"
        )));
    }
    let first_global = 1 + named as u32;

    let symtab_at = len.next_multiple_of(8);
    let strtab_at = symtab_at + symbols * SYM;
    let symtab_index = shdrs.len() as u32;
    shdrs.push(Shdr {
        name: symtab_name,
        sh_type: elf::SHT_SYMTAB,
        offset: tail + symtab_at as u64,
        size: (symbols * SYM) as u64,
        link: symtab_index + 1,
        info: first_global,
        align: 8,
        entsize: SYM as u64,
        ..Shdr::default()
    });
    shdrs.push(Shdr {
        name: strtab_name,
        sh_type: elf::SHT_STRTAB,
        offset: tail + strtab_at as u64,
        size: strtab_len as u64,
        align: 1,
        ..Shdr::default()
    });
    let shstrndx = shdrs.len();
    let shstrtab_at = strtab_at + strtab_len;
    shdrs.push(Shdr {
        name: shstrtab_name,
        sh_type: elf::SHT_STRTAB,
        offset: tail + shstrtab_at as u64,
        size: names.bytes.len() as u64,
        align: 1,
        ..Shdr::default()
    });
    if shdrs.len() >= usize::from(elf::SHN_LORESERVE) {
        return Err(error(format!("{} sections, more than ELF numbers", shdrs.len()))); // alloc: error path
    }
    let shoff_at = (shstrtab_at + names.bytes.len()).next_multiple_of(8);
    let shoff = tail + shoff_at as u64;

    let layout = Layout {
        tail,
        len,
        named,
        function_names,
        global_names,
        globals,
        shstrtab_len: names.bytes.len(),
        shdrs: shdrs.len(),
    };
    debug_assert_eq!(
        (layout.symtab(), layout.strtab(), layout.shoff(), layout.shdrs),
        (symtab_at, strtab_at, shoff_at, shdr_count)
    );

    // The tail after the debug sections: the padding to `.symtab`, the symbol tables and the
    // section headers, at `len` from `tail`. The debug sections are the core's
    // (`File::debug`), so a link whose debug info is as before writes none of it again.
    let size = shoff_at + shdrs.len() * SHDR - len;
    let (symtab, strtab) = (symtab_at - len, strtab_at - len);
    let bytes = match kept {
        Some(kept) if patched => {
            debug_assert!(kept.layout == layout && kept.bytes.len() == size, "a patched tail keeps its layout");
            let (syms, names_at) = globals_at(&layout);
            let scratch = &mut kept.scratch;
            scratch.clear();
            scratch.resize(strtab - syms + size - names_at, 0);
            let (globals, rest) = scratch.split_at_mut(strtab - syms);
            globals_and_headers(placed, &layout, globals, rest, &names.bytes, &shdrs)?;
            kept.bytes.update(syms, &scratch[..strtab - syms]);
            kept.bytes.update(names_at, &scratch[strtab - syms..]);
            kept.synthesis = placed.synthesis;
            #[cfg(test)]
            {
                kept.patched += 1;
            }
            // clone: two reference counts, the table shared with the state that holds it
            kept.bytes.clone()
        }
        mut kept => {
            // alloc: the output buffer, the file's tail past its debug info
            let mut bytes = vec![0u8; size];
            {
                let functions = placed.functions;
                let (head, rest) = bytes.split_at_mut(strtab);
                let functions_syms = &mut head[symtab..symtab + SYM * (1 + named)];
                let function_strs = &mut rest[..1 + function_names];
                let (mut syms, mut strs) = (&mut functions_syms[SYM..], &mut function_strs[1..]);
                let mut jobs = Vec::with_capacity_in(chunks.len(), Global);
                let mut name_at = 1u32;
                for (chunk, &(count, len)) in order.chunks(CHUNK).zip(&chunks) {
                    let (these_syms, rest) = std::mem::take(&mut syms).split_at_mut(SYM * count);
                    syms = rest;
                    let (these_strs, rest) = std::mem::take(&mut strs).split_at_mut(len);
                    strs = rest;
                    jobs.push((chunk, these_syms, these_strs, name_at));
                    name_at += len as u32;
                }
                par::for_each(jobs, |(chunk, syms, strs, name_at)| {
                    let mut strings = Names { bytes: strs, at: 0, offset: name_at };
                    for (at, out) in chunk.iter().zip(syms.chunks_exact_mut(SYM)) {
                        let function = &functions[*at as usize];
                        let name = function.name.as_deref().expect("`local_order` gives only named functions");
                        let offset = strings.add(name);
                        out.copy_from_slice(&local_symbol(regions, function, offset).bytes());
                    }
                });
                // Each function's local by its index in `functions`, for the next link's
                // patch: one pass over the order, as the fill above laid the names out.
                if let Some(kept) = kept.as_deref_mut() {
                    kept.locals.clear();
                    kept.locals.resize(functions.len(), Local::default());
                    let mut name_at = 1u32;
                    for (nth, at) in order.iter().enumerate() {
                        let name = functions[*at as usize].name.as_deref().unwrap_or_default();
                        let stored = stored(name) as u32;
                        let name = if stored == 0 { 0 } else { name_at };
                        kept.locals[*at as usize] = Local { sym: 1 + nth as u32, name, stored };
                        name_at += stored;
                    }
                    // The room: the entries after the live ones, and the names' run after
                    // theirs, which starts at `name_at`.
                    kept.free.clear();
                    kept.free.extend(1 + order.len() as u32..1 + named as u32);
                    kept.holes.clear();
                    let names_end = 1 + function_names as u32;
                    if names_end > name_at {
                        kept.holes.insert((names_end - name_at, name_at));
                    }
                }
            }
            {
                let (syms, names_at) = globals_at(&layout);
                let (head, rest) = bytes.split_at_mut(names_at);
                globals_and_headers(
                    placed,
                    &layout,
                    &mut head[syms..strtab],
                    rest,
                    &names.bytes,
                    &shdrs,
                )?;
            }
            let bytes = CowBytes::from_vec(bytes);
            if let Some(kept) = kept {
                // clone: two reference counts, the table shared with the state that holds it
                kept.bytes = bytes.clone();
                kept.layout = layout;
                kept.synthesis = placed.synthesis;
            }
            bytes
        }
    };
    drop(last);

    let mut head = Vec::with_capacity((EHDR + PHDR * phnum) as usize);
    head.extend_from_slice(&elf::ELFMAG);
    head.put_u8(elf::ELFCLASS64.0);
    head.put_u8(elf::ELFDATA2LSB.0);
    head.put_u8(elf::EV_CURRENT.0);
    head.put_u8(elf::ELFOSABI_NONE.0);
    head.resize(16, 0);
    head.put_u16(elf::ET_DYN.0);
    head.put_u16(writer.machine());
    head.put_u32(u32::from(elf::EV_CURRENT.0));
    head.put_u64(entry.unwrap_or(0));
    head.put_u64(EHDR);
    head.put_u64(shoff);
    head.put_u32(0);
    head.put_u16(EHDR as u16);
    head.put_u16(PHDR as u16);
    head.put_u16(phnum as u16);
    head.put_u16(64);
    head.put_u16(shdrs.len() as u16);
    head.put_u16(shstrndx as u16);
    for phdr in &phdrs {
        head.put_u32(phdr.p_type.0);
        head.put_u32(phdr.flags);
        head.put_u64(phdr.vaddr.saturating_sub(plan.base));
        head.put_u64(phdr.vaddr);
        head.put_u64(phdr.vaddr);
        head.put_u64(phdr.filesz);
        head.put_u64(phdr.memsz);
        head.put_u64(phdr.align);
    }
    debug_assert!(head.len() as u64 <= plan.headers);

    // alloc: once per image, what `Writer::file` returns, and the headers' `Arc`
    let mut parts = Vec::with_capacity(2);
    parts.push((0, CowBytes::from_vec(head)));
    // The debug sections are the core's: laid at their offsets, never copied here.
    // alloc: once per image, an entry per debug section
    let mut debug = Vec::with_capacity(placed.debug.len());
    let mut at = tail;
    for (nth, (_, data)) in placed.debug.iter().enumerate() {
        if !data.is_empty() {
            debug.push((at, nth));
        }
        at += data.len() as u64;
    }
    let file_len = at + bytes.len() as u64;
    parts.push((at, bytes));
    Ok(File { len: file_len, parts, debug })
}
