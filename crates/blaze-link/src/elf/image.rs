//! The file around the placed regions: the ELF header and program headers at its start, and
//! after the last region the symbol table, the debug sections and the section headers.
//!
//! [`file`] reads the synthetic regions' bytes from [`Placed::contents`], where the core puts
//! what [`crate::model::Writer::synthesize`] returned, so no table is built twice. An
//! incremental output keeps the tail past the debug sections and, where [`Placed::delta`]
//! spliced functions without changing any name's length, rewrites only their local symbols
//! and names in it; the rest of the tail is small and written whole.

use blaze_format::cursor::Put;
use blaze_graph::par;
use object::elf;

use super::dynamic::{CHUNK, Strings, Sym, info, numbered, shndx, value};
use super::{
    BUILD_ID, DYNAMIC, DYNSTR, DYNSYM, EH_FRAME_HDR, EHDR, Elf, GNU_HASH, INTERP, PHDR, RELA_DYN, Segment, VERNEED,
    VERSYM, error, in_file,
};
use crate::cow::CowBytes;
use crate::model::{Class, File, Function, Placed, Plan, Region, Reserve, Splice, SymbolKind};
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
        Class::Stubs => ".blaze.stubs",
        Class::Plt => ".plt",
        Class::Veneers => ".blaze.veneers",
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
}

/// The tail past the debug sections as [`file`] made it, kept to patch the next.
#[derive(Default)]
pub(crate) struct KeptSymtab {
    synthesis: u64,
    layout: Layout,
    /// By function, in [`Placed::functions`] order.
    locals: Vec<Local>,
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
    /// Whether each function `splices` put in has a name where the one it replaced had, of
    /// the length that one's had: then every local keeps its index and its name's place.
    fn same_locals(&self, functions: &[Function], splices: &[Splice]) -> bool {
        self.locals.len() == functions.len()
            && splices.iter().all(|splice| {
                let (start, end) = (splice.new.start as usize, splice.new.end as usize);
                match (functions.get(start..end), self.locals.get(start..end)) {
                    (Some(functions), Some(locals)) => {
                        functions.iter().zip(locals).all(|(function, local)| match function.name.as_deref() {
                            None => local.sym == 0,
                            Some(name) => local.sym != 0 && local.stored as usize == stored(name),
                        })
                    }
                    _ => false,
                }
            })
    }

    /// Rewrites the local symbol and name of each function `splices` put in, once
    /// [`KeptSymtab::same_locals`] held; `symtab` and `strtab` are where the tables are in the
    /// kept bytes.
    fn patch_locals(&mut self, regions: &[Region], functions: &[Function], splices: &[Splice], symtab: usize, strtab: usize) {
        for splice in splices {
            for at in splice.new.start as usize..splice.new.end as usize {
                let (function, local) = (&functions[at], self.locals[at]);
                let Some(name) = function.name.as_deref() else { continue };
                let symbol = local_symbol(regions, function, local.name);
                self.bytes.update(symtab + SYM * local.sym as usize, &symbol.bytes());
                self.bytes.update(strtab + local.name as usize, name.as_bytes());
            }
        }
    }
}

/// By chunk of [`CHUNK`] functions: how many are named and the bytes their names take.
fn name_counts(functions: &[Function]) -> Vec<(usize, usize)> {
    par::map(par::chunks(functions, CHUNK), |chunk| {
        (chunk.iter().filter_map(|it| it.name.as_deref())).fold((0, 0), |(count, len), name| (count + 1, len + stored(name)))
    }) // alloc: once per image, one count per chunk of functions
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
    (layout.symtab() - layout.len + SYM * (1 + layout.named), layout.strtab() - layout.len + 1 + layout.function_names)
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
        let symbol = Sym {
            name: strings.add(&export.name),
            info: info(elf::STB_GLOBAL, export.kind),
            shndx: shndx(regions, export.addr),
            value: value(placed, &export.name, export.addr, export.kind)?,
            size: export.size,
        };
        out.next().expect("a symbol counted").copy_from_slice(&symbol.bytes());
    }
    for import in placed.imports {
        let bind = if import.weak { elf::STB_WEAK } else { elf::STB_GLOBAL };
        let symbol = Sym { name: strings.add(&import.name), info: info(bind, import.kind), ..Sym::default() };
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
            Err(error(format!("a dev run's {:?} segment at {start:#x}..{end:#x} {why}", load.segment)))
        };
        match load.segment {
            Segment::Code => {
                if !start.is_multiple_of(page) || !(start - plan.base).is_multiple_of(page) {
                    return refuse(&format!("does not start on a {page:#x} page in the file and in memory")); // alloc: error path
                }
                if load.segment.flags() & elf::PF_W.0 != 0 {
                    return refuse("is writable");
                }
                if load.zeroed || load.file_end != end {
                    return refuse("is not all file bytes");
                }
                let before = at.checked_sub(1).map(|it| &loads[it]).filter(|it| it.end.next_multiple_of(page) > start);
                let after = loads.get(at + 1).filter(|it| it.vaddr < end.next_multiple_of(page));
                if let Some(other) = before.or(after) {
                    // alloc: error path
                    return refuse(&format!("shares a {page:#x} page with the {:?} segment at {:#x}", other.segment, other.vaddr));
                }
            }
            Segment::Reserve(reserve) => {
                if last_reserve.is_some_and(|it| it >= reserve) {
                    return refuse("is out of the arena, cells, fixed order");
                }
                last_reserve = Some(reserve);
                if end == start || !start.is_multiple_of(page) || !(end - start).is_multiple_of(page) {
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
    let entry = match &writer.output.kind {
        OutputKind::Executable { entry } => {
            Some(placed.entry.ok_or_else(|| error(format!("the executable's entry `{entry}` is not placed")))?) // alloc: error path
        }
        OutputKind::Shared { .. } => placed.entry,
    };

    // At most one per segment: a second run of one is an error below.
    let mut loads: Vec<Load> = Vec::with_capacity((4 + super::RESERVES) as usize);
    for region in regions {
        let segment = Segment::of(&region.class);
        let end = region.addr + region.capacity.max(region.size);
        let file = in_file(&region.class);
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
                    return Err(error(format!("{:?} at {:#x} splits the {segment:?} segment", region.class, region.addr)));
                }
                let vaddr = if loads.is_empty() { plan.base } else { region.addr };
                if loads.is_empty() && segment != Segment::Read {
                    // alloc: error path
                    return Err(error(format!("the image starts with {:?}, not the headers' segment", region.class)));
                }
                if vaddr % plan.page != 0 {
                    // alloc: error path
                    return Err(error(format!(
                        "the {segment:?} segment starts at {vaddr:#x}, not on a {:#x} page",
                        plan.page
                    )));
                }
                loads.push(Load { segment, vaddr, file_end: if file { end } else { vaddr }, end, zeroed: !file });
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
                return Err(error(format!("{:?} at {:#x} in an image that is not a dev run", region.class, region.addr)));
            }
        }
    }

    // Each synthetic region's address and how many bytes `synthesize` gave it.
    let synthetic = |name: &'static str| {
        placed.region(&Class::Synthetic(name)).map(|(index, it)| (it.addr, placed.len(index) as u64))
    };
    let room = writer.phdrs();
    let mut phdrs = Vec::with_capacity(room as usize);
    phdrs.push(Phdr { p_type: elf::PT_PHDR, flags: 4, vaddr: plan.base + EHDR, filesz: 0, memsz: 0, align: 8 });
    if let Some((addr, size)) = synthetic(INTERP) {
        phdrs.push(Phdr { p_type: elf::PT_INTERP, flags: 4, vaddr: addr, filesz: size, memsz: size, align: 1 });
    }
    for load in &loads {
        phdrs.push(Phdr {
            p_type: elf::PT_LOAD,
            flags: load.segment.flags(),
            vaddr: load.vaddr,
            filesz: load.file_end - load.vaddr,
            memsz: load.end - load.vaddr,
            align: plan.page,
        });
    }
    if let Some((addr, size)) = synthetic(DYNAMIC) {
        phdrs.push(Phdr { p_type: elf::PT_DYNAMIC, flags: 6, vaddr: addr, filesz: size, memsz: size, align: 8 });
    }
    if let Some(at) = loads.iter().position(|it| it.segment == Segment::Relro) {
        let load = &loads[at];
        // The loader protects whole pages: round up to the next segment's page so the last
        // one is protected too, but never past the mapping when nothing follows.
        let end = if at + 1 < loads.len() { load.end.next_multiple_of(plan.page) } else { load.end };
        let size = end - load.vaddr;
        phdrs.push(Phdr { p_type: elf::PT_GNU_RELRO, flags: 4, vaddr: load.vaddr, filesz: size, memsz: size, align: 1 });
    }
    if let (Some(_), Some((addr, size))) = (placed.region(&Class::EhFrame), synthetic(EH_FRAME_HDR)) {
        phdrs.push(Phdr { p_type: elf::PT_GNU_EH_FRAME, flags: 4, vaddr: addr, filesz: size, memsz: size, align: 4 });
    }
    phdrs.push(Phdr { p_type: elf::PT_GNU_STACK, flags: 6, vaddr: 0, filesz: 0, memsz: 0, align: 0 });
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
        phdrs.push(Phdr { p_type: elf::PT_NOTE, flags: 4, vaddr: addr, filesz: size, memsz: size, align: 4 });
    }
    if phdrs.len() as u64 > room {
        return Err(error(format!("{} program headers, more than the {room} the headers have room for", phdrs.len()))); // alloc: error path
    }
    let phnum = phdrs.len() as u64;
    phdrs[0].filesz = phnum * PHDR;
    phdrs[0].memsz = phnum * PHDR;

    // Section headers: the null one, one per region in address order (`shndx` counts on it),
    // then those outside the address space.
    numbered(regions)?;
    let index_of = |class: &Class| regions.iter().position(|it| &it.class == class).map_or(0, |it| it as u32 + 1);
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
                (shdr.sh_type, shdr.flags, shdr.size) = (elf::SHT_NOBITS, 0, region.capacity.max(region.size))
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
                    GNU_HASH => (shdr.sh_type, shdr.link) = (elf::SHT_GNU_HASH, index_of(&Class::Synthetic(DYNSYM))),
                    VERSYM => {
                        (shdr.sh_type, shdr.entsize) = (elf::SHT_GNU_VERSYM, 2);
                        shdr.link = index_of(&Class::Synthetic(DYNSYM));
                    }
                    VERNEED => {
                        shdr.sh_type = elf::SHT_GNU_VERNEED;
                        (shdr.link, shdr.info) = (index_of(&Class::Synthetic(DYNSTR)), verneeds(placed.bytes(index)));
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

    // `.symtab`: the functions by name (locals, for backtraces and debuggers), then the
    // exports and the imports. Each name has its own place in `.strtab`, unshared as mold
    // writes it, so both tables are laid out by prefix sums over chunks of functions and
    // filled in parallel, straight into the file's tail. An incremental output keeps the
    // tail and, when only functions' symbols changed and no name's length did, rewrites
    // those in place.
    let mut last = writer.last();
    let mut kept = last.as_deref_mut().map(|it| &mut it.symtab);
    let from = kept.as_deref_mut().map_or(0, |it| std::mem::replace(&mut it.synthesis, 0));
    let patch = match (kept.as_deref(), placed.delta) {
        (Some(kept), Some(delta)) => {
            from != 0 && delta.patches(from, &[delta.functions]) && kept.same_locals(placed.functions, delta.functions)
        }
        _ => false,
    };
    let (mut chunks, named, function_names) = match kept.as_deref() {
        // alloc: none, an empty Vec holds no buffer
        Some(kept) if patch => (Vec::new(), kept.layout.named, kept.layout.function_names),
        _ => {
            let chunks = name_counts(placed.functions);
            let named: usize = chunks.iter().map(|it| it.0).sum();
            let function_names: usize = chunks.iter().map(|it| it.1).sum();
            (chunks, named, function_names)
        }
    };
    let global_names = placed.exports.iter().map(|it| stored(&it.name)).sum::<usize>()
        + placed.imports.iter().map(|it| stored(&it.name)).sum::<usize>();
    let symbols = 1 + named + placed.exports.len() + placed.imports.len();
    let strtab_len = 1 + function_names + global_names;
    if u32::try_from(strtab_len).is_err() {
        return Err(error(format!("{strtab_len} bytes of symbol names, more than .strtab offsets reach"))); // alloc: error path
    }
    let first_global = 1 + named as u32;

    let tail = regions
        .iter()
        .filter(|it| in_file(&it.class))
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
    let symtab_at = len.next_multiple_of(8);
    let strtab_at = symtab_at + symbols * SYM;
    let symtab_index = shdrs.len() as u32;
    shdrs.push(Shdr {
        name: names.add(".symtab"),
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
        name: names.add(".strtab"),
        sh_type: elf::SHT_STRTAB,
        offset: tail + strtab_at as u64,
        size: strtab_len as u64,
        align: 1,
        ..Shdr::default()
    });
    let shstrndx = shdrs.len();
    let shstrtab_name = names.add(".shstrtab");
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
        globals: placed.exports.len() + placed.imports.len(),
        shstrtab_len: names.bytes.len(),
        shdrs: shdrs.len(),
    };
    debug_assert_eq!((layout.symtab(), layout.strtab(), layout.shoff()), (symtab_at, strtab_at, shoff_at));

    // The tail after the debug sections: the padding to `.symtab`, the symbol tables and the
    // section headers, at `len` from `tail`. The debug sections are the core's
    // (`File::debug`), so a link whose debug info is as before writes none of it again.
    let size = shoff_at + shdrs.len() * SHDR - len;
    let (symtab, strtab) = (symtab_at - len, strtab_at - len);
    let bytes = match kept {
        Some(kept) if patch && kept.layout == layout && kept.bytes.len() == size => {
            let splices = placed.delta.map_or(&[][..], |it| it.functions);
            kept.patch_locals(regions, placed.functions, splices, symtab, strtab);
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
            if patch {
                chunks = name_counts(placed.functions);
            }
            // alloc: the output buffer, the file's tail past its debug info
            let mut bytes = vec![0u8; size];
            {
                let locals: &mut [Local] = match kept.as_deref_mut() {
                    Some(kept) => {
                        kept.locals.clear();
                        kept.locals.resize(placed.functions.len(), Local::default());
                        &mut kept.locals[..]
                    }
                    None => &mut [],
                };
                let (head, rest) = bytes.split_at_mut(strtab);
                let functions_syms = &mut head[symtab..symtab + SYM * (1 + named)];
                let function_strs = &mut rest[..1 + function_names];
                let (mut syms, mut strs, mut locals) = (&mut functions_syms[SYM..], &mut function_strs[1..], locals);
                let mut jobs = Vec::with_capacity(chunks.len());
                let (mut name_at, mut sym_at) = (1u32, 1u32);
                for (chunk, &(count, len)) in placed.functions.chunks(CHUNK).zip(&chunks) {
                    let (these_syms, rest) = std::mem::take(&mut syms).split_at_mut(SYM * count);
                    syms = rest;
                    let (these_strs, rest) = std::mem::take(&mut strs).split_at_mut(len);
                    strs = rest;
                    let kept_here = if locals.is_empty() { 0 } else { chunk.len() };
                    let (these_locals, rest) = std::mem::take(&mut locals).split_at_mut(kept_here);
                    locals = rest;
                    jobs.push((chunk, these_syms, these_strs, these_locals, name_at, sym_at));
                    name_at += len as u32;
                    sym_at += count as u32;
                }
                par::for_each(jobs, |(chunk, syms, strs, locals, name_at, sym_at)| {
                    let mut strings = Names { bytes: strs, at: 0, offset: name_at };
                    let mut out = syms.chunks_exact_mut(SYM);
                    let mut sym = sym_at;
                    for (at, function) in chunk.iter().enumerate() {
                        let Some(name) = function.name.as_deref() else { continue };
                        let offset = strings.add(name);
                        let symbol = local_symbol(regions, function, offset);
                        out.next().expect("a symbol counted").copy_from_slice(&symbol.bytes());
                        if let Some(local) = locals.get_mut(at) {
                            *local = Local { sym, name: offset, stored: stored(name) as u32 };
                        }
                        sym += 1;
                    }
                });
            }
            {
                let (syms, names_at) = globals_at(&layout);
                let (head, rest) = bytes.split_at_mut(names_at);
                globals_and_headers(placed, &layout, &mut head[syms..strtab], rest, &names.bytes, &shdrs)?;
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
