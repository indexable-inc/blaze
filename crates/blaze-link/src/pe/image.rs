//! The tables a PE loader reads, made from the placed image, and the headers around it.
//!
//! The import directory (descriptors, lookup tables, the hint/name table, DLL names; the
//! import address table is the core's GOT, laid out per DLL), the export directory, the debug
//! directory with its CodeView entry, the TLS directory when the CRT defines none, the
//! exception directory (`.pdata`: codegen's functions and the objects' entries, sorted), the
//! base relocations; then the DOS header and stub, the PE and optional headers and the section
//! table.
//!
//! Cost of a build: `.reloc` is one pass over `Placed::rebase` (sorted by the core; sorted
//! here only if it isn't), 2 bytes per address, with a region cursor rather than a search per
//! address, cut at page boundaries into about four pieces a thread, written in parallel and
//! copied in parallel into one zeroed buffer; `.pdata` is one pass over `Placed::functions` in
//! pieces of 4096 on every thread, each piece then merged with the objects' entries that sort
//! before the next piece into its place in the output, 12 bytes each. The two run in parallel
//! with each other. The import and export tables are linear in imports and exports, plus a
//! sort of each; each import's name is parsed once and borrowed, and nothing in them allocates
//! per entry. `file` doesn't rebuild any table: it reads each directory's size from the bytes
//! `synthesize` produced, which the core put in the regions and sized them to, so it costs the
//! number of regions plus GOT cells.
//!
//! An incremental output keeps its last `.pdata`, `.reloc`, import and export directories
//! ([`Last`]) and, for a [`Placed::delta`] from them with no region moved, patches them: the
//! entries of the spliced functions and rebased addresses, each checked to sort strictly
//! between its neighbours (so a build would make the same bytes), and the export directory's
//! time stamp. The import directory is the same bytes when the imports, libraries and GOT are.
//! Anything a patch can't show to be the build's bytes is built.

use std::borrow::Cow;
use std::ops::Range;

use blaze_bytes::le::{u32 as u32_of, u64 as u64_of};
use blaze_format::cursor::Put;
use blaze_graph::par;
use blaze_ids::radix;
use object::pe;
use smallvec::SmallVec;

use super::coff::ImportName;
use super::{
    DEBUG, EDATA, GRANULE, IDATA, OBJ_PDATA, PAGE, PDATA, Pe, RELOC, RSRC, START_HOOK, Segment, TLS_DIR, TLS_INDEX,
    error, place, zeroed,
};
use crate::cow::CowBytes;
use crate::model::{
    Class, Counts, Delta, Export, File, Function, Holds, Pages, Placed, Prot, Region, SymbolKind, View, WindowsUnwind,
};
use crate::{LinkError, OutputKind};

/// Room for the image's file name, which the export directory and the CodeView entry hold
/// and which is not known when the synthetic regions are sized.
const NAME_ROOM: u64 = 260;
const DESCRIPTOR: u64 = 20;
const EXPORT_DIRECTORY: u64 = 40;
const TLS_DIRECTORY: u64 = 40;
const DEBUG_DIRECTORY: u64 = 28;
/// `RSDS`, the GUID and the age, before the PDB's name.
const CODEVIEW: u64 = 24;
const RUNTIME_FUNCTION: usize = 12;
/// Where the PE signature is: after the DOS header and its program.
const NT: usize = 0x80;
const FILE_HEADER: usize = 20;
const OPTIONAL: usize = 240;
const SECTION_HEADER: usize = 40;
const DIRECTORIES: usize = 16;
/// link.exe's defaults: `/STACK` and `/HEAP` reserve 1 MiB and commit a page.
const STACK_RESERVE: u64 = 0x10_0000;
const HEAP_RESERVE: u64 = 0x10_0000;
const COMMIT: u64 = 0x1000;
/// link.exe 14's version, and the Windows version its x64 images ask for (Vista, 6.0).
const LINKER_VERSION: u8 = 14;
const WINDOWS_VERSION: u16 = 6;
/// `IMAGE_LOAD_CONFIG_DIRECTORY64`'s `GuardCFFunctionTable`, `GuardCFFunctionCount` and
/// `GuardFlags`, and the flag of an image built with `/guard:cf`.
const GUARD_TABLE: usize = 0x80;
const GUARD_COUNT: usize = 0x88;
const GUARD_FLAGS: usize = 0x90;
const GUARD_CF_INSTRUMENTED: u32 = 0x100;

/// The MS-DOS program link.exe puts after the DOS header.
const DOS_PROGRAM: &[u8] =
    b"\x0e\x1f\xba\x0e\x00\xb4\x09\xcd\x21\xb8\x01\x4c\xcd\x21This program cannot be run in DOS mode.\r\r\n$";

/// A little-endian byte writer over one buffer sized up front.
#[derive(Default)]
pub(super) struct Buf(pub(super) Vec<u8>);

impl Buf {
    pub(super) fn with_capacity(capacity: usize) -> Buf {
        Buf(Vec::with_capacity(capacity))
    }

    /// The bytes and a NUL.
    pub(super) fn string(&mut self, text: &str) {
        self.0.put_bytes(text.as_bytes());
        self.0.put_u8(0);
    }

    pub(super) fn len(&self) -> u64 {
        self.0.len() as u64
    }
}

impl std::ops::Deref for Buf {
    type Target = Vec<u8>;
    fn deref(&self) -> &Vec<u8> {
        &self.0
    }
}

impl std::ops::DerefMut for Buf {
    fn deref_mut(&mut self) -> &mut Vec<u8> {
        &mut self.0
    }
}

fn rva_of(placed: &Placed<'_>, addr: u64) -> Option<u32> {
    addr.checked_sub(placed.plan.base).and_then(|it| u32::try_from(it).ok())
}

fn rva(placed: &Placed<'_>, addr: u64, what: &str) -> Result<u32, LinkError> {
    Image::of(placed).rva(addr, what)
}

/// What the tables made on other threads read of the placed image: `Placed` itself is not
/// `Sync` (its `symbol` is a `dyn Fn`).
#[derive(Clone, Copy)]
pub(super) struct Image<'a> {
    base: u64,
    regions: &'a [Region],
    contents: &'a [&'a [u8]],
    paged: &'a [Option<Pages<'a>>],
    rebase: &'a [u64],
    functions: &'a [Function],
    /// A dev run's: no base relocation may land in code.
    dev_run: bool,
    synthesis: u64,
}

impl<'a> Image<'a> {
    pub(super) fn of(placed: &Placed<'a>) -> Image<'a> {
        Image {
            base: placed.plan.base,
            regions: placed.regions,
            contents: placed.contents,
            paged: placed.paged,
            rebase: placed.rebase,
            functions: placed.functions,
            dev_run: placed.output.dev_run.is_some(),
            synthesis: placed.synthesis,
        }
    }

    /// A function's `RUNTIME_FUNCTION`.
    fn entry(self, function: &Function, windows: WindowsUnwind) -> Result<Entry, LinkError> {
        let entry = (|| {
            Some((self.rva_of(function.addr)?, self.rva_of(function.addr + function.size)?, self.rva_of(windows.info)?))
        })();
        entry.ok_or_else(|| {
            let name = function.name.as_deref().unwrap_or("a function");
            error(format_args!("{name} at {:#x} or its unwind info is outside the image", function.addr))
        })
    }

    /// The region a rebased address at `addr` is checked against: the first reaching past its
    /// 8 bytes (the regions are in address order, so their ends are too).
    fn region_for(self, addr: u64) -> Option<&'a Region> {
        let at = self.regions.partition_point(|it| it.addr + it.capacity.max(it.size) < addr + 8);
        self.regions.get(at)
    }

    /// That the address at `addr`, in `region` ([`Image::region_for`]), may be rebased: it is
    /// in the file, and not in a dev run's code.
    fn rebased(self, region: Option<&Region>, addr: u64) -> Result<(), LinkError> {
        match region {
            Some(it) if it.addr <= addr && zeroed(&it.class) => {
                Err(error(format_args!("a rebased address at {addr:#x} in {:?}, which is not in the file", it.class)))
            }
            Some(it) if it.addr <= addr && it.prot.exec && self.dev_run => {
                let what = match self.code_at(addr) {
                    // alloc: error path
                    Some((name, offset)) => format!("in `{name}` (+{offset:#x})"),
                    // alloc: error path
                    None => format!("in {:?}", it.class),
                };
                Err(error(format_args!(
                    "an absolute address at {addr:#x} {what}, which needs a base relocation into code: a dev run's \
                     code is the same bytes at every load address, so code reaches addresses RIP-relative only \
                     (an `IMAGE_REL_AMD64_ADDR64` belongs in data)"
                )))
            }
            Some(it) if it.addr <= addr => Ok(()),
            _ => Err(error(format_args!("a rebased address at {addr:#x} in no region"))),
        }
    }

    /// Region `index`'s bytes ([`Placed::view`]).
    fn view(self, index: usize) -> View<'a> {
        View::of(self.contents, self.paged, index)
    }

    /// What errors call the code at `addr`: the function it is in, when the core placed one there.
    fn code_at(self, addr: u64) -> Option<(&'a str, u64)> {
        let index = self.functions.partition_point(|it| it.addr <= addr).checked_sub(1)?;
        let function = &self.functions[index];
        (addr < function.addr + function.size.max(1))
            .then(|| (function.name.as_deref().unwrap_or("a function with no name"), addr - function.addr))
    }

    fn rva_of(self, addr: u64) -> Option<u32> {
        addr.checked_sub(self.base).and_then(|it| u32::try_from(it).ok())
    }

    fn rva(self, addr: u64, what: &str) -> Result<u32, LinkError> {
        self.rva_of(addr).ok_or_else(|| error(format_args!("{what} at {addr:#x} is outside the image")))
    }
}

/// The time stamp of the headers and directories: from the image's ID, so the same image is
/// the same bytes (link.exe `/Brepro`).
fn stamp(placed: &Placed<'_>) -> u32 {
    let id = placed.id;
    u32::from_le_bytes([id[0], id[1], id[2], id[3]])
}

/// The `N` bytes of the image at `addr`, across a page of its region or not.
fn bytes_at<const N: usize>(placed: &Placed<'_>, addr: u64, what: &str) -> Result<[u8; N], LinkError> {
    let index = placed.regions.partition_point(|it| it.addr <= addr);
    index
        .checked_sub(1)
        .and_then(|index| placed.view(index).array(usize::try_from(addr - placed.regions[index].addr).ok()?))
        .ok_or_else(|| error(format_args!("{what} at {addr:#x} is not in the image's bytes")))
}

/// What the synthetic regions may need, from the counts alone.
pub(super) fn synthetic(counts: &Counts, dev_run: bool) -> Vec<(Class, u64)> {
    let mut regions = Vec::with_capacity(7);
    if counts.libraries > 0 {
        let descriptors = ((counts.libraries + 1) * DESCRIPTOR).next_multiple_of(8);
        let lookup = (counts.imports + counts.libraries) * 8;
        // A hint, the name (no longer than its `"<hint> <name>"` version), a NUL and a pad.
        let hint_names = counts.imports * 4 + counts.import_names;
        let dll_names = counts.library_names + counts.libraries;
        regions.push((Class::Synthetic(IDATA), descriptors + lookup + hint_names + dll_names));
    }
    if counts.exports > 0 {
        let tables = counts.exports * 10;
        let names = counts.export_names + counts.exports + NAME_ROOM + 1;
        regions.push((Class::Synthetic(EDATA), EXPORT_DIRECTORY + tables + names));
    }
    regions.push((Class::Synthetic(DEBUG), DEBUG_DIRECTORY + CODEVIEW + NAME_ROOM + 1));
    // The directory and a callback list holding only its terminator; a dev run's holds the
    // start hook first when there is no CRT's.
    regions.push((Class::Synthetic(TLS_DIR), TLS_DIRECTORY + if dev_run { 16 } else { 8 }));
    regions.push((Class::Synthetic(TLS_INDEX), 8));
    if counts.functions + counts.unwind > 0 {
        regions.push((Class::Synthetic(PDATA), counts.functions * RUNTIME_FUNCTION as u64 + counts.unwind));
    }
    // Every rebased address and the four of a TLS directory the writer makes (five with a dev
    // run's start hook); a block (its header and a pad) per page that has one.
    let entries = counts.absolute + if dev_run { 5 } else { 4 };
    let blocks = entries.min(counts.image / PAGE + 64);
    regions.push((Class::Synthetic(RELOC), entries * 2 + blocks * 10));
    regions
}

pub(super) fn synthesize(pe: &Pe, placed: &Placed<'_>) -> Result<Vec<(Class, CowBytes)>, LinkError> {
    Ok(Tables::of(pe, placed)?.regions)
}

/// The tables an incremental output's writer made last, each with the [`Placed::synthesis`]
/// it was made in (0 when it holds none: never made, or a build of it failed).
#[derive(Default)]
pub(crate) struct Last {
    pub(super) pdata: KeptPdata,
    pub(super) reloc: KeptReloc,
    pub(super) idata: KeptTable,
    pub(super) edata: KeptTable,
}

/// A table kept whole: the same bytes when what it is made of is the same.
#[derive(Default)]
pub(super) struct KeptTable {
    synthesis: u64,
    /// Its region's address: the table holds RVAs into itself, and a synthetic region may
    /// move when no other does.
    addr: u64,
    bytes: CowBytes,
    /// How many syntheses took it as it was.
    #[cfg(test)]
    pub(super) reused: u32,
}

impl KeptTable {
    /// The kept table, when it is at `region` still and `same` says this synthesis would make
    /// its bytes (`patch` then brings it up to date in place); else `build`'s, kept.
    fn of(
        &mut self,
        placed: &Placed<'_>,
        region: &Region,
        same: impl FnOnce(&Delta<'_>, &CowBytes) -> bool,
        patch: impl FnOnce(&mut CowBytes),
        build: impl FnOnce() -> Result<Vec<u8>, LinkError>,
    ) -> Result<CowBytes, LinkError> {
        let last = std::mem::replace(&mut self.synthesis, 0);
        if let Some(delta) = placed.delta
            && last != 0
            && self.addr == region.addr
            && delta.patches(last, &[])
            && same(delta, &self.bytes)
        {
            patch(&mut self.bytes);
            self.synthesis = placed.synthesis;
            #[cfg(test)]
            {
                self.reused += 1;
            }
            // clone: two reference counts, the table shared with the state that holds it
            return Ok(self.bytes.clone());
        }
        let bytes = CowBytes::from_vec(build()?);
        // clone: two reference counts, the table shared with the state that holds it
        self.bytes = bytes.clone();
        (self.synthesis, self.addr) = (placed.synthesis, region.addr);
        Ok(bytes)
    }
}

/// Whether the import directory is the last one's bytes: the same imports and libraries, and
/// no GOT cell spliced (so each DLL's IAT is where it was).
fn same_imports(delta: &Delta<'_>) -> bool {
    delta.same_names && delta.got.is_empty()
}

/// The synthetic regions' bytes, each handed to the core as a [`CowBytes`]: a table built
/// whole ([`CowBytes::from_vec`]), or an incremental output's kept one, patched.
pub(super) struct Tables {
    pub(super) regions: Vec<(Class, CowBytes)>,
}

impl Tables {
    pub(super) fn of(pe: &Pe, placed: &Placed<'_>) -> Result<Tables, LinkError> {
        let mut tables = Tables { regions: Vec::with_capacity(7) };
        let mut last = pe.last();
        // The GOT, imports and binds the last synthesis checked, unchanged: checked again only
        // when one may differ.
        let checked = match (last.as_deref(), placed.delta) {
            (Some(last), Some(delta)) => {
                last.idata.synthesis != 0 && delta.from == last.idata.synthesis && same_imports(delta)
            }
            _ => false,
        };
        if let Some(bind) = placed.binds.first() {
            let name = placed.imports.get(bind.import as usize).map_or("?", |it| it.name.as_str());
            return Err(error(format_args!(
                "a bind of `{name}` at {:#x}: a PE loader fills only the import address table, \
                 so an import is reached through its cell or its thunk",
                bind.addr
            )));
        }
        if !checked
            && let Some(cell) = placed.got.iter().find(|it| !matches!(it.holds, Holds::Local(_) | Holds::Import(_)))
        {
            return Err(error(format_args!(
                "the GOT cell at {:#x} holds {:?}: Windows reaches thread-locals through \
                 `_tls_index` and `SECREL`, never a GOT cell",
                cell.addr, cell.holds
            )));
        }
        for import in placed.imports.iter().filter(|_| !checked) {
            if import.library.is_none() {
                return Err(error(format_args!(
                    "`{}` is imported from no DLL: PE has no weak or unresolved imports \
                     (link.exe: LNK2019)",
                    import.name
                )));
            }
            if import.kind == SymbolKind::Tls {
                return Err(error(format_args!(
                    "the thread-local `{}` is imported: a DLL exports no thread-locals",
                    import.name
                )));
            }
        }

        match placed.region(&Class::Synthetic(IDATA)) {
            Some((_, region)) => {
                let bytes = match last.as_deref_mut() {
                    Some(last) => {
                        last.idata.of(placed, region, |it, _| same_imports(it), |_| {}, || imports(placed, region))?
                    }
                    None => CowBytes::from_vec(imports(placed, region)?),
                };
                tables.push_cow(placed, IDATA, bytes)?;
            }
            None if !checked && placed.got.iter().any(|it| matches!(it.holds, Holds::Import(_))) => {
                return Err(error("an image with imports and no import directory region"));
            }
            None => {}
        }
        match placed.region(&Class::Synthetic(EDATA)) {
            Some((_, region)) => {
                let bytes = match last.as_deref_mut() {
                    Some(last) => last.edata.of(
                        placed,
                        region,
                        |it, kept| it.same_names && same_export_name(placed, region, kept),
                        |kept| kept.update(4, &stamp(placed).to_le_bytes()),
                        || exports(placed, region),
                    )?,
                    None => CowBytes::from_vec(exports(placed, region)?),
                };
                tables.push_cow(placed, EDATA, bytes)?;
            }
            None if !placed.exports.is_empty() => {
                return Err(error("an image with exports and no export directory region"));
            }
            None => {}
        }
        if let Some((_, region)) = placed.region(&Class::Synthetic(DEBUG)) {
            tables.push(placed, DEBUG, debug(placed, region)?)?;
        }

        let mut extra = SmallVec::<[u64; 5]>::new();
        let tls = match tls_directory(placed)? {
            TlsDirectory::Ours(addr) => own_tls(placed, addr, &mut extra)?,
            TlsDirectory::Crt(_) | TlsDirectory::None => Vec::default(),
        };
        if placed.region(&Class::Synthetic(TLS_DIR)).is_some() {
            tables.push(placed, TLS_DIR, tls)?;
        }
        if placed.region(&Class::Synthetic(TLS_INDEX)).is_some() {
            // alloc: once per link: the `_tls_index` region's bytes, `synthesize`'s output
            tables.push(placed, TLS_INDEX, vec![0; 8])?;
        }

        let (image, delta) = (Image::of(placed), placed.delta);
        let (pdata, relocations) = match last.as_deref_mut() {
            Some(Last { pdata: kept_pdata, reloc: kept_reloc, .. }) => {
                par::join(|| kept_pdata.of(image, delta), || kept_reloc.of(image, delta, &extra))
            }
            None => par::join(
                || pdata(image, None).map(CowBytes::from_vec),
                || relocations(image, &extra, reloc_piece(image.rebase.len())).map(CowBytes::from_vec),
            ),
        };
        drop(last);
        match placed.region(&Class::Synthetic(PDATA)) {
            Some(_) => tables.push_cow(placed, PDATA, pdata?)?,
            None if placed.functions.iter().any(|it| it.windows.is_some()) => {
                return Err(error("an image with unwind info and no `.pdata` region"));
            }
            None => {}
        }
        match placed.region(&Class::Synthetic(RELOC)) {
            Some(_) => tables.push_cow(placed, RELOC, relocations?)?,
            None if !placed.rebase.is_empty() || !extra.is_empty() => {
                return Err(error("an image with rebased addresses and no `.reloc` region"));
            }
            None => {}
        }
        Ok(tables)
    }

    fn push(&mut self, placed: &Placed<'_>, name: &'static str, bytes: Vec<u8>) -> Result<(), LinkError> {
        self.push_cow(placed, name, CowBytes::from_vec(bytes))
    }

    fn push_cow(&mut self, placed: &Placed<'_>, name: &'static str, bytes: CowBytes) -> Result<(), LinkError> {
        let class = Class::Synthetic(name);
        let (_, region) =
            placed.region(&class).ok_or_else(|| error(format_args!("the {name} region was not placed")))?;
        if !region.addr.is_multiple_of(8) {
            return Err(error(format_args!("the {name} region at {:#x} is not 8-aligned", region.addr)));
        }
        let room = region.capacity.max(region.size);
        if bytes.len() as u64 > room {
            return Err(error(format_args!(
                "{name} needs {} bytes, more than the {room} of its region at {:#x}",
                bytes.len(),
                region.addr
            )));
        }
        self.regions.push((class, bytes));
        Ok(())
    }
}

/// Whether the kept export directory names the image as `placed` does: its name's RVA, which
/// the directory states, is where the name is in its last bytes. With the same exports (and
/// the region where it was), every byte but the time stamp is the build's.
fn same_export_name(placed: &Placed<'_>, region: &Region, kept: &CowBytes) -> bool {
    let name = placed.name.as_bytes();
    let Some(at) = kept.len().checked_sub(name.len() + 1) else { return false };
    if kept.len() < 16 {
        return false;
    }
    let (mut stated, mut tail) = ([0; 4], [0; 1]);
    kept.copy_to(12, &mut stated);
    kept.copy_to(kept.len() - 1, &mut tail);
    rva_of(placed, region.addr + at as u64) == Some(u32::from_le_bytes(stated))
        && tail == [0]
        && *kept.read(at, name.len()) == *name
}

/// Which TLS directory the image has.
enum TlsDirectory {
    None,
    /// The CRT's `_tls_used`, at this address.
    Crt(u64),
    /// The writer's own, in its region at this address.
    Ours(u64),
}

/// The CRT's `_tls_used` when it defines one (the template must be the range it states), else
/// the writer's own for an image with thread-locals, one that names `_tls_used` or a dev run
/// (whose start hook is a TLS callback).
fn tls_directory(placed: &Placed<'_>) -> Result<TlsDirectory, LinkError> {
    let ours = placed.region(&Class::Synthetic(TLS_DIR)).map(|(_, it)| it.addr);
    let used = (placed.symbol)("_tls_used");
    match used {
        Some(addr) if Some(addr) != ours => {
            let directory: [u8; TLS_DIRECTORY as usize] =
                bytes_at(placed, addr, "the CRT's TLS directory `_tls_used`")?;
            let start = u64_of(&directory, 0);
            let end = u64_of(&directory, 8);
            // The CRT's range runs from `_tls_start` (`.tls`, first) to `_tls_end` (`.tls$ZZZ`,
            // whose own byte is past it); every other thread-local ranks between the two. The
            // template's growth room and that byte are past `end`.
            let placed_end = placed
                .regions
                .iter()
                .filter(|it| matches!(it.class, Class::Tdata | Class::Tbss))
                .map(|it| it.addr + it.size)
                .max();
            if let (Some(tls), Some(placed_end)) = (placed.tls, placed_end)
                && (tls.addr != start || end < start || end > placed_end)
            {
                return Err(error(format_args!(
                    "the thread-locals placed at {:#x}..{placed_end:#x} don't hold the range \
                     {start:#x}..{end:#x} the CRT's `_tls_used` states (between `_tls_start` and \
                     `_tls_end`)",
                    tls.addr
                )));
            }
            Ok(TlsDirectory::Crt(addr))
        }
        _ if used.is_some() || placed.tls.is_some() || placed.output.dev_run.is_some() => ours
            .map(TlsDirectory::Ours)
            .ok_or_else(|| error("an image with thread-locals or a start hook and no TLS directory region")),
        _ => Ok(TlsDirectory::None),
    }
}

/// The writer's TLS directory at `addr`, and its four addresses the loader rebases. Its
/// callbacks are the CRT's run after `__xl_a` when there is one; with none, a dev run's list
/// is the start hook and its terminator (a fifth address to rebase), and any other's is empty.
fn own_tls(placed: &Placed<'_>, addr: u64, extra: &mut SmallVec<[u64; 5]>) -> Result<Vec<u8>, LinkError> {
    let index = (placed.symbol)("_tls_index")
        .or_else(|| placed.region(&Class::Synthetic(TLS_INDEX)).map(|(_, it)| it.addr))
        .ok_or_else(|| error("an image with thread-locals and no `_tls_index`"))?;
    let crt = (placed.symbol)("__xl_a");
    let hook = match (crt, placed.output.dev_run) {
        (None, Some(_)) => Some(start_hook(placed)?),
        _ => None,
    };
    let callbacks = crt.map_or(addr + TLS_DIRECTORY, |it| it + 8);
    let (start, end, zero_fill, align) = match placed.tls {
        Some(tls) => (tls.addr, tls.addr + tls.init, tls.size - tls.init, tls.align),
        None => (0, 0, 0, 1),
    };
    let zero_fill =
        u32::try_from(zero_fill).map_err(|_| error(format_args!("{zero_fill:#x} bytes of zeroed thread-locals")))?;
    let mut bytes = Buf::with_capacity((TLS_DIRECTORY + 16) as usize);
    bytes.put_u64(start);
    bytes.put_u64(end);
    bytes.put_u64(index);
    bytes.put_u64(callbacks);
    bytes.put_u32(zero_fill);
    bytes.put_u32(alignment(align).map_err(error)?);
    if let Some(hook) = hook {
        bytes.put_u64(hook);
    }
    bytes.put_u64(0);
    if start != 0 {
        extra.extend([addr, addr + 8]);
    }
    extra.extend([addr + 16, addr + 24]);
    if hook.is_some() {
        extra.push(addr + TLS_DIRECTORY);
    }
    Ok(bytes.0)
}

/// The address of a dev run's start hook.
fn start_hook(placed: &Placed<'_>) -> Result<u64, LinkError> {
    (placed.symbol)(START_HOOK).ok_or_else(|| {
        error(format_args!(
            "a dev run's executable defines no `{START_HOOK}`: its first TLS callback is the start hook \
             (`blaze_inproc::runtime`), which the dev runtime must link in"
        ))
    })
}

/// That the first callback the loader calls through the TLS directory at `directory` is a dev
/// run's start hook: the CRT's `__xl_a` is 0 and the hook's pointer (`.CRT$XLAB`) is the next
/// cell, or the writer's own list holds it.
fn check_start_hook(placed: &Placed<'_>, directory: u64) -> Result<(), LinkError> {
    let hook = start_hook(placed)?;
    let callbacks = u64_of(&bytes_at::<{ TLS_DIRECTORY as usize }>(placed, directory, "the TLS directory")?, 24);
    let first = u64_of(&bytes_at::<8>(placed, callbacks, "the TLS callback list")?, 0);
    if first != hook {
        return Err(error(format_args!(
            "the first TLS callback (at {callbacks:#x}) is {first:#x}, not the start hook `{START_HOOK}` at \
             {hook:#x}: a dev run's start hook must run before every other callback and initializer, so its \
             pointer goes in `.CRT$XLAB`, right after the CRT's `__xl_a`"
        )));
    }
    Ok(())
}

/// That a dev run's load configuration asks for no CFG: the loader's CFG bitmap would describe
/// the image section, not the code memory's views over it.
fn check_no_guard(placed: &Placed<'_>) -> Result<(), LinkError> {
    let Some(addr) = (placed.symbol)("_load_config_used") else { return Ok(()) };
    let what = "the load configuration `_load_config_used`";
    let size = u32_of(&bytes_at::<4>(placed, addr, what)?, 0) as usize;
    if size < GUARD_FLAGS + 4 {
        return Ok(());
    }
    let config = bytes_at::<{ GUARD_FLAGS + 4 }>(placed, addr, what)?;
    let (table, count, flags) = (u64_of(&config, GUARD_TABLE), u64_of(&config, GUARD_COUNT), u32_of(&config, GUARD_FLAGS));
    if table != 0 || count != 0 || flags & GUARD_CF_INSTRUMENTED != 0 {
        return Err(error(format_args!(
            "{what} at {addr:#x} asks for control-flow guard (a table of {count} functions at {table:#x}, flags \
             {flags:#x}): a dev run has no CFG, since the loader's bitmap would not cover the code memory's views"
        )));
    }
    Ok(())
}

/// `IMAGE_SCN_ALIGN_*` of an alignment.
pub(super) fn alignment(align: u64) -> Result<u32, String> {
    let align = align.max(1);
    if !align.is_power_of_two() || align > 8192 {
        // alloc: error path
        return Err(format!("an alignment of {align}, which a PE section can't state"));
    }
    Ok((align.trailing_zeros() + 1) << 20)
}

/// An import cell, `(library, cell, import)`: 16 bytes per import, sorted once.
type Cell = (u32, u64, u32);
const _: () = assert!(size_of::<Cell>() == 16);
/// A `RUNTIME_FUNCTION`, `(begin, end, info)` RVAs: 12 bytes per function, as in the file.
type Entry = (u32, u32, u32);
const _: () = assert!(size_of::<Entry>() == RUNTIME_FUNCTION);

/// An entry's place in the order of the tuple.
fn entry_key(&(begin, end, info): &Entry) -> u128 {
    (u128::from(begin) << 64) | (u128::from(end) << 32) | u128::from(info)
}

/// The GOT's import cells sorted by library then cell: each DLL's IAT.
fn iat(placed: &Placed<'_>) -> Result<Vec<Cell>, LinkError> {
    let mut cells = Vec::with_capacity(placed.imports.len());
    for cell in placed.got {
        let Holds::Import(import) = cell.holds else { continue };
        let it = placed.imports.get(import as usize).ok_or_else(|| {
            error(format_args!("the GOT cell at {:#x} holds import {import}, which there isn't", cell.addr))
        })?;
        let library = it.library.ok_or_else(|| error(format_args!("`{}` is imported from no DLL", it.name)))?;
        if placed.libraries.get(library as usize).is_none() {
            return Err(error(format_args!("`{}` is imported from library {library}, which there isn't", it.name)));
        }
        cells.push((library, cell.addr, import));
    }
    radix::sort_copy_by_key(&mut cells, |&(library, addr, import): &Cell| {
        (u128::from(library) << 96) | (u128::from(addr) << 32) | u128::from(import)
    });
    Ok(cells)
}

/// The import directory, the lookup tables, the hint/name table and the DLL names. Each DLL's
/// IAT cells are the core's GOT cells of its imports, consecutive and followed by a zero cell.
fn imports(placed: &Placed<'_>, region: &Region) -> Result<Vec<u8>, LinkError> {
    let cells = iat(placed)?;
    if cells.is_empty() {
        return Ok(Vec::default());
    }
    let mut taken = Vec::with_capacity(placed.got.len());
    taken.extend(placed.got.iter().map(|it| it.addr));
    radix::sort(&mut taken);
    // Each DLL's run of `cells` (and of `names`, which is parallel to it).
    let mut groups: Vec<Range<usize>> = Vec::with_capacity(placed.libraries.len());
    for group in cells.chunk_by(|a, b| a.0 == b.0) {
        let start = groups.last().map_or(0, |it| it.end);
        groups.push(start..start + group.len());
    }
    let mut names = Vec::with_capacity(cells.len());
    for &(_, _, import) in &cells {
        let it = &placed.imports[import as usize];
        names.push(ImportName::parse(&it.name, it.version.as_deref()).map_err(error)?);
    }
    for group in &groups {
        let group = &cells[group.start..group.end];
        let dll = &placed.libraries[group[0].0 as usize].name;
        for pair in group.windows(2) {
            if pair[1].1 != pair[0].1 + 8 {
                return Err(error(format_args!(
                    "the import address table of {dll} is not consecutive: cells at {:#x} and {:#x}",
                    pair[0].1, pair[1].1
                )));
            }
        }
        let after = group[group.len() - 1].1 + 8;
        if taken.binary_search(&after).is_ok() {
            return Err(error(format_args!(
                "the import address table of {dll} has no zero cell after it: {after:#x} is a GOT cell"
            )));
        }
    }

    let base = region.addr;
    let descriptors = ((groups.len() as u64 + 1) * DESCRIPTOR).next_multiple_of(8);
    let lookup = (cells.len() as u64 + groups.len() as u64) * 8;
    let hint_start = descriptors + lookup;
    let mut out = Buf::with_capacity(region.capacity.max(region.size) as usize);
    let mut lookup_at = descriptors;
    // The hint/name entries and DLL names are written after the lookup tables, so their
    // offsets are computed first: the names' bytes are known from the imports.
    let hint_names: u64 = names
        .iter()
        .map(|it| match it {
            ImportName::Name { name, .. } => (2 + name.len() as u64 + 1).next_multiple_of(2),
            ImportName::Ordinal(_) => 0,
        })
        .sum();
    let mut name_at = hint_start + hint_names;
    for group in &groups {
        let first = cells[group.start];
        let dll = &placed.libraries[first.0 as usize].name;
        out.put_u32(rva(placed, base + lookup_at, "an import lookup table")?);
        out.put_u32(0);
        out.put_u32(0);
        out.put_u32(rva(placed, base + name_at, "a DLL name")?);
        out.put_u32(rva(placed, first.1, "an import address table")?);
        lookup_at += (group.len() as u64 + 1) * 8;
        name_at += dll.len() as u64 + 1;
    }
    out.put_bytes(&[0; DESCRIPTOR as usize]);
    out.pad_to(8);
    let mut hint_at = hint_start;
    for group in &groups {
        for name in &names[group.start..group.end] {
            match *name {
                ImportName::Ordinal(ordinal) => out.put_u64(pe::IMAGE_ORDINAL_FLAG64 | u64::from(ordinal)),
                ImportName::Name { name, .. } => {
                    out.put_u64(u64::from(rva(placed, base + hint_at, "a hint/name entry")?));
                    hint_at += (2 + name.len() as u64 + 1).next_multiple_of(2);
                }
            }
        }
        out.put_u64(0);
    }
    for name in &names {
        if let ImportName::Name { hint, name } = *name {
            out.put_u16(hint);
            out.string(name);
            out.pad_to(2);
        }
    }
    for group in &groups {
        out.string(&placed.libraries[cells[group.start].0 as usize].name);
    }
    Ok(out.0)
}

/// The export directory: every export by name, the names sorted (the loader searches them
/// by bisection), ordinals from 1 in that order.
fn exports(placed: &Placed<'_>, region: &Region) -> Result<Vec<u8>, LinkError> {
    let mut exports: Vec<&Export> = Vec::with_capacity(placed.exports.len());
    exports.extend(placed.exports.iter());
    blaze_ids::names::sort_by_name(&mut exports, |it| it.name.as_bytes());
    for pair in exports.windows(2) {
        if pair[0].name == pair[1].name {
            return Err(error(format_args!("`{}` is exported twice", pair[0].name)));
        }
    }
    let count = u16::try_from(exports.len())
        .map_err(|_| error(format_args!("{} exports, more than a PE export table's 65535", exports.len())))?;
    let n = u64::from(count);
    let base = region.addr;
    let functions = EXPORT_DIRECTORY;
    let name_pointers = functions + 4 * n;
    let ordinals = name_pointers + 4 * n;
    let strings = ordinals + 2 * n;
    let names: u64 = exports.iter().map(|it| it.name.len() as u64 + 1).sum();
    let image_name = strings + names;

    let mut out = Buf::with_capacity((image_name + placed.name.len() as u64 + 1) as usize);
    out.put_u32(0);
    out.put_u32(stamp(placed));
    out.put_u16(0);
    out.put_u16(0);
    out.put_u32(rva(placed, base + image_name, "the export directory's name")?);
    out.put_u32(1);
    out.put_u32(count.into());
    out.put_u32(count.into());
    out.put_u32(rva(placed, base + functions, "the export address table")?);
    out.put_u32(rva(placed, base + name_pointers, "the export name table")?);
    out.put_u32(rva(placed, base + ordinals, "the export ordinal table")?);
    for export in &exports {
        if export.kind == SymbolKind::Tls {
            return Err(error(format_args!(
                "the thread-local `{}` is exported: a DLL exports no thread-locals",
                export.name
            )));
        }
        let at = rva_of(placed, export.addr).ok_or_else(|| {
            error(format_args!("the export `{}` at {:#x} is outside the image", export.name, export.addr))
        })?;
        out.put_u32(at);
    }
    let mut at = strings;
    for export in &exports {
        if export.name.is_empty() {
            return Err(error(format_args!("an export with no name at {:#x}", export.addr)));
        }
        out.put_u32(rva(placed, base + at, "an export name")?);
        at += export.name.len() as u64 + 1;
    }
    for ordinal in 0..count {
        out.put_u16(ordinal);
    }
    for export in &exports {
        out.string(&export.name);
    }
    out.string(placed.name);
    Ok(out.0)
}

/// The PDB a debugger looks for is the image's name with `.pdb` for its extension: this stem
/// and [`PDB`].
pub(super) fn pdb_stem(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => name,
    }
}

const PDB: &str = ".pdb";

/// The debug directory: one CodeView entry naming the PDB by the image's ID. No PDB is
/// written.
fn debug(placed: &Placed<'_>, region: &Region) -> Result<Vec<u8>, LinkError> {
    let stem = pdb_stem(placed.name);
    let data = region.addr + DEBUG_DIRECTORY;
    let size = CODEVIEW as u32 + (stem.len() + PDB.len()) as u32 + 1;
    let at = rva(placed, data, "the CodeView entry")?;
    let mut out = Buf::with_capacity((DEBUG_DIRECTORY + u64::from(size)) as usize);
    out.put_u32(0);
    out.put_u32(stamp(placed));
    out.put_u16(0);
    out.put_u16(0);
    out.put_u32(pe::IMAGE_DEBUG_TYPE_CODEVIEW.0);
    out.put_u32(size);
    out.put_u32(at);
    // A region's file offset is its RVA.
    out.put_u32(at);
    out.put_bytes(b"RSDS");
    out.put_bytes(&placed.id);
    out.put_u32(1);
    out.put_bytes(stem.as_bytes());
    out.string(PDB);
    Ok(out.0)
}

/// A `RUNTIME_FUNCTION`'s bytes, as in the file.
fn entry_bytes((begin, end, info): Entry) -> [u8; RUNTIME_FUNCTION] {
    let mut out = [0; RUNTIME_FUNCTION];
    out[..4].copy_from_slice(&begin.to_le_bytes());
    out[4..8].copy_from_slice(&end.to_le_bytes());
    out[8..].copy_from_slice(&info.to_le_bytes());
    out
}

fn read_entry(bytes: &[u8]) -> Entry {
    (u32_of(bytes, 0), u32_of(bytes, 4), u32_of(bytes, 8))
}

/// An entry index or a byte offset that is none: a function with no unwind info, a rebased
/// address repeating the one before.
const NONE: u32 = u32::MAX;

/// Functions per piece of `.pdata` staged on one thread.
const PDATA_PIECE: usize = 1 << 12;

/// A piece of the functions' entries, staged: how many, whether in order, the first and last.
#[derive(Clone, Copy)]
struct Staged {
    count: u32,
    sorted: bool,
    first: Entry,
    last: Entry,
}

/// The entries of `functions` with unwind info, one after another from the start of `out`;
/// with `slots`, each function's index among them ([`NONE`] for one with none).
fn stage(
    image: Image<'_>,
    functions: &[Function],
    out: &mut [u8],
    mut slots: Option<&mut [u32]>,
) -> Result<Staged, LinkError> {
    let mut staged = Staged { count: 0, sorted: true, first: (0, 0, 0), last: (0, 0, 0) };
    for (at, function) in functions.iter().enumerate() {
        let slot = match function.windows {
            None => NONE,
            Some(windows) => {
                let entry = image.entry(function, windows)?;
                if staged.count == 0 {
                    staged.first = entry;
                } else if entry < staged.last {
                    staged.sorted = false;
                }
                out[staged.count as usize * RUNTIME_FUNCTION..][..RUNTIME_FUNCTION].copy_from_slice(&entry_bytes(entry));
                staged.last = entry;
                staged.count += 1;
                staged.count - 1
            }
        };
        if let Some(slots) = slots.as_deref_mut() {
            slots[at] = slot;
        }
    }
    Ok(staged)
}

/// One piece's staged entries merged with the objects' that sort before the next piece's, into
/// `out`, whose first entry is the table's `first`; with `slots` (the piece's, as [`stage`]
/// left them), each function's entry index in the table.
fn merge(staged: &[u8], native: &[Entry], out: &mut [u8], first: u32, slots: Option<&mut [u32]>) {
    let mut ours = staged.chunks_exact(RUNTIME_FUNCTION).map(read_entry).peekable();
    let mut native = native.iter().copied().peekable();
    let mut slots = slots.map(|it| it.iter_mut().filter(|it| **it != NONE));
    for (at, to) in (first..).zip(out.chunks_exact_mut(RUNTIME_FUNCTION)) {
        let entry = match (ours.peek(), native.peek()) {
            (Some(x), Some(y)) if y < x => native.next(),
            (Some(_), _) => {
                if let Some(slot) = slots.as_mut().and_then(Iterator::next) {
                    *slot = at;
                }
                ours.next()
            }
            (None, _) => native.next(),
        };
        let Some(entry) = entry else { break };
        to.copy_from_slice(&entry_bytes(entry));
    }
}

/// `slots` cut into `count` runs of `size`, one per piece (each none when `slots` is).
fn chunks(slots: Option<&mut [u32]>, size: usize, count: usize) -> Vec<Option<&mut [u32]>> {
    let mut out = Vec::with_capacity(count);
    match slots {
        Some(slots) => out.extend(slots.chunks_mut(size).map(Some)),
        None => out.resize_with(count, || None),
    }
    out
}

/// `RUNTIME_FUNCTION`s: one per function codegen gave Windows unwind info (in address order, as
/// the core gives them), merged with the native objects' `.pdata` entries (those of dropped
/// functions read 0), sorted by start; an overlap is an error. Staged in pieces in parallel,
/// then each piece merged into its place in parallel when the pieces are in order (they are
/// when the functions are); else sorted and merged on one thread. With `slots` (a kept
/// table's), each function's entry index in the table, when the pieces were in order; else
/// `slots` is left empty.
fn pdata(image: Image<'_>, mut slots: Option<&mut Vec<u32>>) -> Result<Vec<u8>, LinkError> {
    let functions = image.functions;
    let pieces = functions.len().div_ceil(PDATA_PIECE);
    if let Some(slots) = slots.as_deref_mut() {
        slots.resize(functions.len(), NONE);
    }
    // alloc: once per build, zeroed: each piece's pages are first touched by the thread staging it
    let mut staged = vec![0u8; functions.len() * RUNTIME_FUNCTION];
    let mut done: Vec<Result<Staged, LinkError>> = Vec::with_capacity(pieces);
    par::extend(
        &mut done,
        par::zip(
            par::zip(par::chunks_mut(&mut staged, PDATA_PIECE * RUNTIME_FUNCTION), par::chunks(functions, PDATA_PIECE)),
            chunks(slots.as_deref_mut().map(Vec::as_mut_slice), PDATA_PIECE, pieces),
        ),
        |((out, functions), slots)| stage(image, functions, out, slots),
    );
    let done = done.into_iter().collect::<Result<Vec<Staged>, LinkError>>()?;
    let native = native_pdata(image)?;
    let count: usize = done.iter().map(|it| it.count as usize).sum();
    let mut sorted = true;
    let mut before: Option<Entry> = None;
    for piece in done.iter().filter(|it| it.count > 0) {
        sorted &= piece.sorted && before.is_none_or(|it| it <= piece.first);
        before = Some(piece.last);
    }
    let staged_of = |at: usize, piece: &Staged| {
        &staged[at * PDATA_PIECE * RUNTIME_FUNCTION..][..piece.count as usize * RUNTIME_FUNCTION]
    };
    if !sorted {
        if let Some(slots) = slots {
            slots.clear();
        }
        let mut ours: Vec<Entry> = Vec::with_capacity(count);
        for (at, piece) in done.iter().enumerate() {
            ours.extend(staged_of(at, piece).chunks_exact(RUNTIME_FUNCTION).map(read_entry));
        }
        radix::sort_copy_by_key(&mut ours, entry_key);
        return merge_serial(&ours, &native);
    }
    let Some(first) = done.iter().position(|it| it.count > 0) else {
        return merge_serial(&[], &native);
    };
    // Each piece's run of the objects' entries: those that sort before the next piece's first
    // entry and not before its own; the first piece's from the first.
    let mut ranges = Vec::with_capacity(done.len());
    let mut end = native.len();
    for piece in done.iter().rev() {
        let start = if piece.count == 0 { end } else { native.partition_point(|it| *it < piece.first) };
        ranges.push(start..end);
        end = start;
    }
    ranges.reverse();
    ranges[first].start = 0;
    let total = count + native.len();
    // alloc: the output buffer, `.pdata`'s bytes, zeroed: its pages are first touched by the merging threads
    let mut out = vec![0u8; total * RUNTIME_FUNCTION];
    {
        let mut jobs = Vec::with_capacity(done.len());
        let mut rest = &mut out[..];
        let mut at = 0u32;
        let slots = chunks(slots.as_deref_mut().map(Vec::as_mut_slice), PDATA_PIECE, done.len());
        for (index, ((piece, range), slots)) in done.iter().zip(&ranges).zip(slots).enumerate() {
            let len = piece.count as usize + range.len();
            if len == 0 {
                continue;
            }
            let (to, tail) = std::mem::take(&mut rest).split_at_mut(len * RUNTIME_FUNCTION);
            rest = tail;
            // clone: a `Range` of two `usize`s
            jobs.push((to, staged_of(index, piece), &native[range.clone()], at, slots));
            at += len as u32;
        }
        par::for_each(jobs, |(to, staged, native, first, slots)| merge(staged, native, to, first, slots));
    }
    let read = |at: usize| read_entry(&out[at * RUNTIME_FUNCTION..][..RUNTIME_FUNCTION]);
    if let Some(at) = par::find_map_first(1..total, |at| (read(at).0 < read(at - 1).1).then_some(at)) {
        let ((before, before_end, _), (begin, end, _)) = (read(at - 1), read(at));
        return Err(error(format_args!(
            "the unwind entries of RVAs {before:#x}..{before_end:#x} and {begin:#x}..{end:#x} overlap"
        )));
    }
    Ok(out)
}

/// `.pdata` as [`pdata`] builds it, kept to patch the next.
#[derive(Default)]
pub(super) struct KeptPdata {
    synthesis: u64,
    bytes: CowBytes,
    /// By function, its entry's index in `bytes` ([`NONE`] for one with no unwind info): kept
    /// only when the functions' entries came in order.
    slots: Vec<u32>,
    /// The entries a patch wrote, reused.
    touched: Vec<u32>,
    /// How many syntheses were patches.
    #[cfg(test)]
    pub(super) patched: u32,
}

impl KeptPdata {
    /// The table for `image`: the kept one patched, when `delta` is from it and every functions
    /// splice keeps its length; else built (and kept when its entries came in order).
    fn of(&mut self, image: Image<'_>, delta: Option<&Delta<'_>>) -> Result<CowBytes, LinkError> {
        let last = std::mem::replace(&mut self.synthesis, 0);
        if let Some(delta) = delta
            && last != 0
            && delta.patches(last, &[delta.functions])
            && self.patch(image, delta)
        {
            self.synthesis = image.synthesis;
            #[cfg(test)]
            {
                self.patched += 1;
            }
            // clone: two reference counts, the table shared with the state that holds it
            return Ok(self.bytes.clone());
        }
        let bytes = CowBytes::from_vec(pdata(image, Some(&mut self.slots))?);
        if self.slots.len() == image.functions.len() {
            // clone: two reference counts, the table shared with the state that holds it
            self.bytes = bytes.clone();
            self.synthesis = image.synthesis;
        }
        Ok(bytes)
    }

    /// Writes the entries of the spliced functions into the kept table. False, with the table
    /// left half patched, when that is not the table a build makes: a function that gained or
    /// lost unwind info, an entry that no longer sorts strictly between its neighbours or
    /// overlaps one, the objects' entries written, or one a build refuses (it gives the error).
    fn patch(&mut self, image: Image<'_>, delta: &Delta<'_>) -> bool {
        let functions = image.functions;
        if self.slots.len() != functions.len() {
            return false;
        }
        let written = |region: &Region| {
            let end = region.addr + region.capacity.max(region.size);
            delta.written.iter().any(|&(addr, len)| addr < end && region.addr < addr + len)
        };
        let mut objects =
            image.regions.iter().filter(|it| matches!(&it.class, Class::Named { name, .. } if name == OBJ_PDATA));
        if objects.any(written) {
            return false;
        }
        let entries = self.bytes.len() / RUNTIME_FUNCTION;
        self.touched.clear();
        for splice in delta.functions {
            let range = splice.new.start as usize..splice.new.end as usize;
            // clone: a `Range` of two `usize`s
            let (Some(functions), Some(slots)) = (functions.get(range.clone()), self.slots.get(range)) else {
                return false;
            };
            for (function, &slot) in functions.iter().zip(slots) {
                match (function.windows, slot) {
                    (None, NONE) => {}
                    (Some(windows), slot) if slot != NONE && (slot as usize) < entries => {
                        let Ok(entry) = image.entry(function, windows) else { return false };
                        self.bytes.update(slot as usize * RUNTIME_FUNCTION, &entry_bytes(entry));
                        self.touched.push(slot);
                    }
                    _ => return false,
                }
            }
        }
        let bytes = &self.bytes;
        let read = |at: usize| {
            let mut entry = [0; RUNTIME_FUNCTION];
            bytes.copy_to(at * RUNTIME_FUNCTION, &mut entry);
            read_entry(&entry)
        };
        let after = |before: Entry, it: Entry| before < it && before.1 <= it.0;
        self.touched.iter().all(|&slot| {
            let (slot, this) = (slot as usize, read(slot as usize));
            (slot == 0 || after(read(slot - 1), this)) && (slot + 1 >= entries || after(this, read(slot + 1)))
        })
    }
}

/// The objects' `.pdata` entries but those of dropped functions (which read 0), sorted.
fn native_pdata(image: Image<'_>) -> Result<Vec<Entry>, LinkError> {
    let objects = || {
        (image.regions.iter().enumerate())
            .filter(|(_, region)| matches!(&region.class, Class::Named { name, .. } if name == OBJ_PDATA))
            .map(|(index, region)| (region, image.view(index)))
    };
    let mut native: Vec<Entry> =
        Vec::with_capacity(objects().map(|(_, bytes)| bytes.len() / RUNTIME_FUNCTION).sum());
    for (region, bytes) in objects() {
        if !bytes.len().is_multiple_of(RUNTIME_FUNCTION) {
            return Err(error(format_args!(
                "the objects' .pdata at {:#x} is {} bytes, not whole RUNTIME_FUNCTIONs",
                region.addr,
                bytes.len()
            )));
        }
        for entry in bytes.arrays::<RUNTIME_FUNCTION>(0, bytes.len()) {
            let begin = u32_of(&entry, 0);
            if begin != 0 {
                native.push((begin, u32_of(&entry, 4), u32_of(&entry, 8)));
            }
        }
    }
    if !native.is_sorted() {
        radix::sort_copy_by_key(&mut native, entry_key);
    }
    Ok(native)
}

/// `ours` and `native`, each sorted, merged on one thread; an overlap is an error.
fn merge_serial(ours: &[Entry], native: &[Entry]) -> Result<Vec<u8>, LinkError> {
    let mut out = Vec::with_capacity((ours.len() + native.len()) * RUNTIME_FUNCTION);
    let (mut a, mut b) = (ours.iter().peekable(), native.iter().peekable());
    let mut last: Option<Entry> = None;
    loop {
        let entry = match (a.peek(), b.peek()) {
            (Some(x), Some(y)) if y < x => b.next(),
            (Some(_), _) => a.next(),
            (None, _) => b.next(),
        };
        let Some(&(begin, end, info)) = entry else { break };
        if let Some((before, before_end, _)) = last
            && begin < before_end
        {
            return Err(error(format_args!(
                "the unwind entries of RVAs {before:#x}..{before_end:#x} and {begin:#x}..{end:#x} overlap"
            )));
        }
        last = Some((begin, end, info));
        out.extend_from_slice(&begin.to_le_bytes());
        out.extend_from_slice(&end.to_le_bytes());
        out.extend_from_slice(&info.to_le_bytes());
    }
    Ok(out)
}

/// Rebased addresses per piece of `.reloc` written on one thread at least: below this a thread
/// costs more than the 2 bytes per address it would write.
const RELOC_PIECE: usize = 1 << 13;

/// The rebased addresses per piece of `.reloc` for `addresses` of them: about four pieces a
/// thread, so a slow thread holds the rest up by a quarter of its share at most.
fn reloc_piece(addresses: usize) -> usize {
    addresses.div_ceil(4 * par::threads()).max(RELOC_PIECE)
}

fn page_of(addr: u64) -> u64 {
    blaze_mem::align::align_down_u64(addr, PAGE)
}

/// A base relocation's entry: `DIR64` at the address's offset in its page.
fn dir64(addr: u64) -> u16 {
    (pe::IMAGE_REL_BASED_DIR64.0 << 12) | (addr & (PAGE - 1)) as u16
}

/// What a kept `.reloc` is patched by: by rebased address, its entry's byte offset ([`NONE`]
/// for a repeat of the address before), and each block's byte offset, in order.
#[derive(Default)]
struct RelocIndex {
    slots: Vec<u32>,
    blocks: Vec<u32>,
}

/// The base relocations: a block per 4 KiB page, a `DIR64` entry per rebased address in it,
/// padded to 4 bytes. The sorted addresses are cut at page boundaries into pieces of about
/// `piece` addresses, each written on its own thread and copied in parallel into its place: a
/// block never spans two pieces, so the bytes are the same whatever the number of threads or
/// the piece size. One piece's worth allocates only its bytes (and a sorted copy when the
/// core's addresses aren't sorted).
pub(super) fn relocations(image: Image<'_>, extra: &[u64], piece: usize) -> Result<Vec<u8>, LinkError> {
    build_relocations(image, extra, piece, None)
}

/// [`relocations`], and with `index` (a kept table's) what patches it, when the core's
/// addresses came sorted; else `index` is left empty.
fn build_relocations(
    image: Image<'_>,
    extra: &[u64],
    piece: usize,
    index: Option<&mut RelocIndex>,
) -> Result<Vec<u8>, LinkError> {
    let rebase: Cow<'_, [u64]> = if image.rebase.is_sorted() {
        Cow::Borrowed(image.rebase)
    } else {
        // alloc: once per link, only when the core's rebased addresses aren't sorted
        let mut sorted = image.rebase.to_vec();
        radix::sort(&mut sorted);
        Cow::Owned(sorted)
    };
    let (mut slots, mut starts) = match index {
        Some(RelocIndex { slots, blocks }) => {
            blocks.clear();
            match &rebase {
                Cow::Borrowed(_) => {
                    slots.resize(rebase.len(), NONE);
                    (Some(&mut slots[..]), Some(blocks))
                }
                Cow::Owned(_) => {
                    slots.clear();
                    (None, None)
                }
            }
        }
        None => (None, None),
    };
    let mut extra = SmallVec::<[u64; 5]>::from_slice(extra);
    extra.sort_unstable();
    let piece = piece.max(1);
    // alloc: none for one piece: an empty `Vec` holds no buffer
    let cuts = if rebase.len() > piece { page_cuts(&rebase, piece) } else { Vec::new() };
    if cuts.is_empty() {
        let mut out = Vec::default();
        blocks(image, &rebase, &extra, &mut out, slots, starts)?;
        return Ok(out);
    }
    // Each piece's addresses and the extra ones in its pages.
    let mut bounds: Vec<(Range<usize>, Range<usize>)> = Vec::with_capacity(cuts.len() + 1);
    let (mut from, mut extra_from) = (0, 0);
    for &cut in &cuts {
        let page = page_of(rebase[cut]);
        let extra_to = extra.partition_point(|it| page_of(*it) < page);
        bounds.push((from..cut, extra_from..extra_to));
        (from, extra_from) = (cut, extra_to);
    }
    bounds.push((from..rebase.len(), extra_from..extra.len()));
    let keep = starts.is_some();
    let mut written: Vec<Result<(Vec<u8>, Vec<u32>), LinkError>> = Vec::with_capacity(bounds.len());
    par::extend(&mut written, par::zip(&bounds, split(slots.as_deref_mut(), &bounds)), |((addresses, extras), slots)| {
        // alloc: one buffer a piece (and its block starts for a kept table)
        let (mut out, mut piece_starts) = (Vec::default(), Vec::new());
        let starts = keep.then_some(&mut piece_starts);
        // clone: two `Range`s of two `usize`s
        blocks(image, &rebase[addresses.clone()], &extra[extras.clone()], &mut out, slots, starts)
            .map(|()| (out, piece_starts))
    });
    let mut parts = Vec::with_capacity(written.len());
    for piece in written {
        parts.push(piece?);
    }
    let total: usize = parts.iter().map(|it| it.0.len()).sum();
    // alloc: the output buffer, `.reloc`'s bytes, zeroed: its pages are first touched by the copying threads
    let mut out = vec![0u8; total];
    if let Some(starts) = starts.as_deref_mut() {
        starts.resize(parts.iter().map(|it| it.1.len()).sum(), 0);
    }
    {
        let mut jobs = Vec::with_capacity(parts.len());
        let (mut rest, mut starts_rest) = (&mut out[..], starts.as_deref_mut().map(Vec::as_mut_slice));
        let mut base = 0usize;
        for ((bytes, piece_starts), slots) in parts.iter().zip(split(slots.as_deref_mut(), &bounds)) {
            let (to, tail) = std::mem::take(&mut rest).split_at_mut(bytes.len());
            rest = tail;
            let starts_to = starts_rest.take().map(|it| {
                let (head, tail) = it.split_at_mut(piece_starts.len());
                starts_rest = Some(tail);
                head
            });
            jobs.push((to, &bytes[..], slots, starts_to, &piece_starts[..], base as u32));
            base += bytes.len();
        }
        par::for_each(jobs, |(to, from, slots, starts_to, starts_from, base)| {
            to.copy_from_slice(from);
            for slot in slots.into_iter().flatten().filter(|it| **it != NONE) {
                *slot += base;
            }
            for (to, from) in starts_to.into_iter().flatten().zip(starts_from) {
                *to = from + base;
            }
        });
    }
    Ok(out)
}

/// The first address of each piece but the first, about `piece` apart: each the first of its
/// page, so no block spans two pieces.
fn page_cuts(rebase: &[u64], piece: usize) -> Vec<usize> {
    let mut cuts = Vec::with_capacity(rebase.len() / piece + 1);
    let mut at = piece;
    while at < rebase.len() {
        let page = page_of(rebase[at]);
        let mut cut = rebase.partition_point(|it| page_of(*it) < page);
        if cut <= cuts.last().copied().unwrap_or(0) {
            cut = rebase.partition_point(|it| page_of(*it) <= page);
        }
        if cut >= rebase.len() {
            break;
        }
        cuts.push(cut);
        at = cut + piece;
    }
    cuts
}

/// `slots` cut into the runs of `bounds`' addresses, one per piece (each none when `slots` is).
fn split<'s>(
    mut slots: Option<&'s mut [u32]>,
    bounds: &[(Range<usize>, Range<usize>)],
) -> Vec<Option<&'s mut [u32]>> {
    let mut out = Vec::with_capacity(bounds.len());
    for (addresses, _) in bounds {
        out.push(slots.take().map(|it| {
            let (head, tail) = it.split_at_mut(addresses.len());
            slots = Some(tail);
            head
        }));
    }
    out
}

/// Each synthetic region's address and room, in order.
fn synthetic_layout(regions: &[Region]) -> SmallVec<[(u64, u64); 8]> {
    regions
        .iter()
        .filter(|it| matches!(it.class, Class::Synthetic(_)))
        .map(|it| (it.addr, it.capacity.max(it.size)))
        // alloc: none: the writer's seven synthetic regions fit inline
        .collect()
}

/// `.reloc` as [`relocations`] builds it, kept to patch the next.
#[derive(Default)]
pub(super) struct KeptReloc {
    synthesis: u64,
    bytes: CowBytes,
    index: RelocIndex,
    /// The writer's own addresses it holds (its TLS directory's), as `synthesize` gave them.
    extra: SmallVec<[u64; 5]>,
    /// The synthetic regions' places ([`synthetic_layout`]), which the addresses were checked
    /// against and which may move when no other region does.
    layout: SmallVec<[(u64, u64); 8]>,
    /// (block, entry offset) of each entry a patch wrote, reused.
    touched: Vec<(u32, u32)>,
    /// How many syntheses were patches.
    #[cfg(test)]
    pub(super) patched: u32,
}

impl KeptReloc {
    /// The table for `image`: the kept one patched, when `delta` is from it, every rebase
    /// splice keeps its length and the writer's own addresses are the same; else built (and
    /// kept when the core's addresses came sorted).
    fn of(&mut self, image: Image<'_>, delta: Option<&Delta<'_>>, extra: &[u64]) -> Result<CowBytes, LinkError> {
        let last = std::mem::replace(&mut self.synthesis, 0);
        let layout = synthetic_layout(image.regions);
        if let Some(delta) = delta
            && last != 0
            && self.extra[..] == *extra
            && self.layout == layout
            && delta.patches(last, &[delta.rebase])
            && self.patch(image, delta)
        {
            self.synthesis = image.synthesis;
            #[cfg(test)]
            {
                self.patched += 1;
            }
            // clone: two reference counts, the table shared with the state that holds it
            return Ok(self.bytes.clone());
        }
        let built = build_relocations(image, extra, reloc_piece(image.rebase.len()), Some(&mut self.index))?;
        let bytes = CowBytes::from_vec(built);
        if self.index.slots.len() == image.rebase.len() {
            // clone: two reference counts, the table shared with the state that holds it
            self.bytes = bytes.clone();
            (self.extra, self.layout) = (SmallVec::from_slice(extra), layout);
            self.synthesis = image.synthesis;
        }
        Ok(bytes)
    }

    /// Writes the entries of the spliced rebased addresses into the kept table. False, with
    /// the table left half patched, when that is not the table a build makes: an address that
    /// left its page or its place in the order, a repeat, or one a build refuses (it gives the
    /// error).
    fn patch(&mut self, image: Image<'_>, delta: &Delta<'_>) -> bool {
        let rebase = image.rebase;
        let RelocIndex { slots, blocks } = &self.index;
        if slots.len() != rebase.len() {
            return false;
        }
        self.touched.clear();
        for splice in delta.rebase {
            let (start, end) = (splice.new.start as usize, splice.new.end as usize);
            // The address after a splice repeating the one before has no entry of its own,
            // which it needs when that one changed.
            if end > rebase.len()
                || !rebase[start.saturating_sub(1)..(end + 1).min(rebase.len())].is_sorted()
                || slots.get(end) == Some(&NONE)
            {
                return false;
            }
            for at in start..end {
                let (addr, slot) = (rebase[at], slots[at]);
                if slot == NONE || image.rebased(image.region_for(addr), addr).is_err() {
                    return false;
                }
                let Some(block) = blocks.partition_point(|&it| it <= slot).checked_sub(1) else { return false };
                let mut page = [0; 4];
                self.bytes.copy_to(blocks[block] as usize, &mut page);
                if image.rva_of(page_of(addr)) != Some(u32::from_le_bytes(page)) {
                    return false;
                }
                self.bytes.update(slot as usize, &dir64(addr).to_le_bytes());
                self.touched.push((block as u32, slot));
            }
        }
        // Each written entry is strictly between its neighbours in its block, as a build sorts
        // and dedups them; a pad (type 0) ends a block.
        let bytes = &self.bytes;
        let entry = |at: usize| {
            let mut it = [0; 2];
            bytes.copy_to(at, &mut it);
            u16::from_le_bytes(it)
        };
        self.touched.iter().all(|&(block, slot)| {
            let (start, slot) = (blocks[block as usize] as usize, slot as usize);
            let mut size = [0; 4];
            bytes.copy_to(start + 4, &mut size);
            let end = start + u32::from_le_bytes(size) as usize;
            let this = entry(slot) & 0xfff;
            let next = (slot + 2 < end).then(|| entry(slot + 2)).filter(|it| (it >> 12) == pe::IMAGE_REL_BASED_DIR64.0);
            (slot < start + 10 || (entry(slot - 2) & 0xfff) < this) && next.is_none_or(|it| this < (it & 0xfff))
        })
    }
}

/// The blocks of sorted rebased addresses merged with sorted `extra`, appended to `out`: one
/// pass, with a cursor over the regions checking each lies in a region with bytes in the file.
/// With `slots`, each rebased address's entry offset in `out` ([`NONE`] for a repeat); with
/// `starts`, each block's.
fn blocks(
    image: Image<'_>,
    rebase: &[u64],
    extra: &[u64],
    out: &mut Vec<u8>,
    mut slots: Option<&mut [u32]>,
    mut starts: Option<&mut Vec<u32>>,
) -> Result<(), LinkError> {
    let regions = image.regions;
    let count = rebase.len() + extra.len();
    let lowest = rebase.first().into_iter().chain(extra.first()).min();
    let highest = rebase.last().into_iter().chain(extra.last()).max();
    let (Some(&lowest), Some(&highest)) = (lowest, highest) else {
        return Ok(());
    };
    let pages = ((highest - lowest) / PAGE + 1) as usize;
    out.reserve(count * 2 + pages.min(count) * 12);
    let (mut i, mut j) = (0, 0);
    let mut region = 0;
    let mut last = None;
    let mut block: Option<(u64, usize)> = None;
    loop {
        // The address and, for one of `rebase`, its index there.
        let (addr, from) = match (rebase.get(i), extra.get(j)) {
            (Some(&a), Some(&b)) if b < a => {
                j += 1;
                (b, usize::MAX)
            }
            (Some(&a), _) => {
                i += 1;
                (a, i - 1)
            }
            (None, Some(&b)) => {
                j += 1;
                (b, usize::MAX)
            }
            (None, None) => break,
        };
        if last == Some(addr) {
            if let Some(slot) = slots.as_deref_mut().and_then(|it| it.get_mut(from)) {
                *slot = NONE;
            }
            continue;
        }
        last = Some(addr);
        while region < regions.len() && regions[region].addr + regions[region].capacity.max(regions[region].size) < addr + 8 {
            region += 1;
        }
        image.rebased(regions.get(region), addr)?;
        let page = page_of(addr);
        if block.map(|it| it.0) != Some(page) {
            if let Some((_, start)) = block {
                close(out, start);
            }
            block = Some((page, out.len()));
            if let Some(starts) = starts.as_deref_mut() {
                starts.push(out.len() as u32);
            }
            out.extend_from_slice(&image.rva(page, "a rebased page")?.to_le_bytes());
            out.extend_from_slice(&[0; 4]);
        }
        if let Some(slot) = slots.as_deref_mut().and_then(|it| it.get_mut(from)) {
            *slot = out.len() as u32;
        }
        out.extend_from_slice(&dir64(addr).to_le_bytes());
    }
    if let Some((_, start)) = block {
        close(out, start);
    }
    Ok(())
}

/// Ends the block at `start`: a pad entry to 4 bytes, and its size.
fn close(out: &mut Vec<u8>, start: usize) {
    if !out.len().is_multiple_of(4) {
        out.extend_from_slice(&[0; 2]);
    }
    let size = (out.len() - start) as u32;
    out[start + 4..start + 8].copy_from_slice(&size.to_le_bytes());
}

/// The data directories, RVA and size by `IMAGE_DIRECTORY_ENTRY_*`, read from the tables
/// `synthesize` made (the core sized each synthetic region to its bytes) rather than made again.
fn directories(placed: &Placed<'_>) -> Result<[(u32, u32); DIRECTORIES], LinkError> {
    let mut directories = [(0, 0); DIRECTORIES];
    let filled = |name: &'static str| -> Result<Option<(usize, u32, u32)>, LinkError> {
        match placed.region(&Class::Synthetic(name)) {
            Some((index, region)) if region.size > 0 => {
                let size = u32::try_from(region.size).map_err(|_| error(format_args!("a {name} of {:#x} bytes", region.size)))?;
                Ok(Some((index, rva(placed, region.addr, name)?, size)))
            }
            _ => Ok(None),
        }
    };
    if let Some((index, at, _)) = filled(IDATA)? {
        let view = placed.view(index);
        let whole = view.len() - view.len() % DESCRIPTOR as usize;
        let count = view
            .arrays::<{ DESCRIPTOR as usize }>(0, whole)
            .position(|it| it.iter().all(|byte| *byte == 0))
            .ok_or_else(|| error("the import directory has no null descriptor: its bytes are not in place"))?;
        directories[pe::IMAGE_DIRECTORY_ENTRY_IMPORT] = (at, (count as u32 + 1) * DESCRIPTOR as u32);
        let bounds = placed
            .got
            .iter()
            .filter(|it| matches!(it.holds, Holds::Import(_)))
            .fold(None, |bounds: Option<(u64, u64)>, it| match bounds {
                Some((first, last)) => Some((first.min(it.addr), last.max(it.addr))),
                None => Some((it.addr, it.addr)),
            });
        if let Some((first, last)) = bounds {
            directories[pe::IMAGE_DIRECTORY_ENTRY_IAT] =
                (rva(placed, first, "the import address table")?, (last + 16 - first) as u32);
        }
    }
    if let Some((_, at, size)) = filled(EDATA)? {
        directories[pe::IMAGE_DIRECTORY_ENTRY_EXPORT] = (at, size);
    }
    if let Some((index, at, _)) = filled(DEBUG)? {
        if placed.view(index).u32(12) != Some(pe::IMAGE_DEBUG_TYPE_CODEVIEW.0) {
            return Err(error("the debug directory's bytes are not in place: `file` before `synthesize`"));
        }
        directories[pe::IMAGE_DIRECTORY_ENTRY_DEBUG] = (at, DEBUG_DIRECTORY as u32);
    }
    match tls_directory(placed)? {
        TlsDirectory::Crt(addr) | TlsDirectory::Ours(addr) => {
            directories[pe::IMAGE_DIRECTORY_ENTRY_TLS] = (rva(placed, addr, "the TLS directory")?, TLS_DIRECTORY as u32);
        }
        TlsDirectory::None => {}
    }
    if let Some((_, at, size)) = filled(PDATA)? {
        directories[pe::IMAGE_DIRECTORY_ENTRY_EXCEPTION] = (at, size);
    }
    if let Some((_, at, size)) = filled(RELOC)? {
        directories[pe::IMAGE_DIRECTORY_ENTRY_BASERELOC] = (at, size);
    }
    if let Some(addr) = (placed.symbol)("_load_config_used") {
        let size = u32_of(&bytes_at::<4>(placed, addr, "the load configuration `_load_config_used`")?, 0);
        directories[pe::IMAGE_DIRECTORY_ENTRY_LOAD_CONFIG] = (rva(placed, addr, "the load configuration")?, size);
    }
    let resources = || placed.regions.iter().filter(|it| matches!(&it.class, Class::Named { name, .. } if name == RSRC));
    if let (Some(start), Some(end)) =
        (resources().map(|it| it.addr).min(), resources().map(|it| it.addr + it.size).max())
    {
        directories[pe::IMAGE_DIRECTORY_ENTRY_RESOURCE] =
            (rva(placed, start, "the resource directory")?, (end - start) as u32);
    }
    Ok(directories)
}

/// One section of the image: a run of regions of one protection.
pub(super) struct Out {
    pub(super) name: [u8; 8],
    pub(super) start: u64,
    pub(super) end: u64,
    /// The end of its bytes in the file: zeroed regions at its end are not.
    pub(super) file_end: u64,
    pub(super) prot: Prot,
    pub(super) discardable: bool,
    /// A dev run's reservation: no access, no raw data, a section of its own.
    pub(super) reserve: bool,
}

impl Out {
    fn raw(&self) -> u64 {
        if self.file_end > self.start { (self.file_end - self.start).next_multiple_of(PAGE) } else { 0 }
    }

    fn characteristics(&self) -> pe::SectionFlags {
        let mut flags = if self.prot.exec {
            pe::IMAGE_SCN_CNT_CODE | pe::IMAGE_SCN_MEM_EXECUTE
        } else if self.raw() > 0 {
            pe::IMAGE_SCN_CNT_INITIALIZED_DATA
        } else {
            pe::IMAGE_SCN_CNT_UNINITIALIZED_DATA
        };
        if self.prot.read {
            flags |= pe::IMAGE_SCN_MEM_READ;
        }
        if self.prot.write {
            flags |= pe::IMAGE_SCN_MEM_WRITE;
        }
        if self.discardable {
            flags |= pe::IMAGE_SCN_MEM_DISCARDABLE;
        }
        flags
    }
}

fn default_prot(segment: Segment) -> Prot {
    match segment {
        Segment::Text => Prot::RX,
        Segment::Data | Segment::Tls => Prot::RW,
        Segment::Rdata | Segment::Pdata | Segment::Rsrc | Segment::Reloc => Prot::R,
        Segment::Arena | Segment::Cells | Segment::Fixed => Prot::NONE,
    }
}

fn section_name(class: &Class, segment: Segment, prot: Prot) -> [u8; 8] {
    let name = match class {
        Class::Named { name, .. } if prot != default_prot(segment) => name.as_bytes(),
        _ => segment.name(),
    };
    let mut out = [0; 8];
    for (to, byte) in out.iter_mut().zip(name) {
        *to = *byte;
    }
    out
}

/// The image's sections: a new one at each page-aligned change of section or protection. A
/// section whose regions the core placed right after another's of the same protection (no
/// page boundary between them, as when `.tls` is absent and `.reloc` follows `.pdata`)
/// joins it: the loader maps both alike, and the data directories find the tables in it.
pub(super) fn sections(placed: &Placed<'_>) -> Result<Vec<Out>, LinkError> {
    let mut sections: Vec<Out> = Vec::with_capacity(8);
    // The segment of the last section's first region.
    let mut last_segment = None;
    for region in placed.regions {
        if region.class == Class::ThreadVars {
            return Err(error(format_args!(
                "Mach-O thread-local descriptors at {:#x} in a PE image",
                region.addr
            )));
        }
        if region.class == Class::PreinitArray {
            return Err(error(format_args!(
                "ELF's `.preinit_array` at {:#x} in a PE image: a PE image's start hook is a TLS callback \
                 (`.CRT$XLAB`)",
                region.addr
            )));
        }
        let end = region.addr + region.capacity.max(region.size);
        if end == region.addr {
            continue;
        }
        let segment = place(&region.class).0;
        let in_file = !zeroed(&region.class);
        let aligned = region.addr.is_multiple_of(PAGE);
        let extend = match sections.last() {
            Some(last) if region.addr < last.end => {
                return Err(error(format_args!(
                    "{:?} at {:#x} overlaps the section before it, which ends at {:#x}",
                    region.class, region.addr, last.end
                )));
            }
            Some(_) if segment.reserve() => false,
            Some(last) => last.prot == region.prot && (last_segment == Some(segment) || !aligned),
            None => false,
        };
        if let Some(last) = sections.last_mut().filter(|_| extend) {
            last.end = end;
            if in_file {
                last.file_end = end;
            }
            continue;
        }
        if !aligned {
            return Err(error(format_args!(
                "{:?} at {:#x} begins a section off a {PAGE:#x} page boundary",
                region.class, region.addr
            )));
        }
        last_segment = Some(segment);
        sections.push(Out {
            name: section_name(&region.class, segment, region.prot),
            start: region.addr,
            end,
            file_end: if in_file { end } else { region.addr },
            prot: region.prot,
            discardable: segment == Segment::Reloc,
            reserve: segment.reserve(),
        });
    }
    match sections.first() {
        None => return Err(error("an image with no regions")),
        Some(first) if first.start < placed.plan.base + placed.plan.headers => {
            return Err(error(format_args!("the first section at {:#x} is inside the headers", first.start)));
        }
        Some(_) => {}
    }
    if placed.output.dev_run.is_some() {
        dev_run_sections(placed.plan.base, &mut sections)?;
    }
    Ok(sections)
}

fn name_of(section: &Out) -> &str {
    let len = section.name.iter().position(|it| *it == 0).unwrap_or(section.name.len());
    std::str::from_utf8(&section.name[..len]).unwrap_or("?")
}

/// A dev run's sections, as the start hook overlays them ([`super`]'s docs): the code is one run
/// of sections from a granule, never writable, before every other section; the first section
/// after it starts on the next granule; the reservations come last, each whole granules. Each
/// section then reaches the next (the NT loader wants them adjacent), the code's raw data with
/// it, so a code view's every byte is in the file. A layout that breaks one is an error: the
/// core placed it, and nothing here moves a region.
fn dev_run_sections(base: u64, sections: &mut [Out]) -> Result<(), LinkError> {
    let code = sections.iter().take_while(|it| it.prot.exec).count();
    if code == 0 {
        return Err(error("a dev run's executable has no code section first: its code must start the image"));
    }
    for (index, section) in sections.iter().enumerate() {
        let rva = section.start - base;
        let name = name_of(section);
        if section.prot.exec && section.prot.write {
            return Err(error(format_args!(
                "the section {name} at RVA {rva:#x} is writable and executable: a dev run's code is a shared view \
                 that is never writable"
            )));
        }
        if index >= code && section.prot.exec {
            return Err(error(format_args!(
                "the code section {name} at RVA {rva:#x} comes after a section that is not code: a dev run's code \
                 is one run of sections before all others"
            )));
        }
        if (index == 0 || index == code) && !rva.is_multiple_of(GRANULE) {
            let what = if index == 0 { "the code" } else { "the first section after the code" };
            return Err(error(format_args!(
                "{what}, {name}, starts at RVA {rva:#x}, off a {GRANULE:#x}-byte granule: a dev run's code is mapped \
                 as views, which start on granules"
            )));
        }
        if index > 0 && sections[index - 1].reserve && !section.reserve {
            return Err(error(format_args!(
                "the section {name} at RVA {rva:#x} comes after a reservation: a dev run's reservations are last"
            )));
        }
        if section.reserve && !(rva.is_multiple_of(GRANULE) && (section.end - base).is_multiple_of(GRANULE)) {
            return Err(error(format_args!(
                "the reservation {name} at RVA {rva:#x}..{:#x} is not whole {GRANULE:#x}-byte granules: a dev run's \
                 reservation sizes are multiples of 64 KiB",
                section.end - base
            )));
        }
    }
    for index in 0..sections.len() {
        let next = sections.get(index + 1).map(|it| it.start);
        let section = &mut sections[index];
        section.end = match next {
            Some(next) => next,
            None if section.prot.exec => section.end.next_multiple_of(GRANULE),
            None => section.end,
        };
        if section.prot.exec {
            section.file_end = section.end;
            if section.end - section.start > section.raw() {
                return Err(error(format_args!(
                    "the code section {} at RVA {:#x} is {:#x} bytes with {:#x} of raw data: a dev run's code is all \
                     in the file",
                    name_of(section),
                    section.start - base,
                    section.end - section.start,
                    section.raw()
                )));
            }
        }
    }
    Ok(())
}

/// The headers: the DOS header and program, the PE header, the optional header with the data
/// directories, the section table.
pub(super) fn file(pe: &Pe, placed: &Placed<'_>) -> Result<File, LinkError> {
    let directories = directories(placed)?;
    let sections = sections(placed)?;
    if pe.dev_run() {
        match tls_directory(placed)? {
            TlsDirectory::Crt(addr) | TlsDirectory::Ours(addr) => check_start_hook(placed, addr)?,
            TlsDirectory::None => return Err(error("a dev run's executable has no TLS directory for its start hook")),
        }
        check_no_guard(placed)?;
    }
    let base = placed.plan.base;
    let size_of_headers = placed.plan.headers;
    let headers = NT + 4 + FILE_HEADER + OPTIONAL + SECTION_HEADER * sections.len();
    if headers as u64 > PAGE {
        return Err(error(format_args!("{} sections, more than the headers' page has room for", sections.len())));
    }
    let last = sections.last().map_or(base + PAGE, |it| it.end);
    let size_of_image = u32::try_from((last - base).next_multiple_of(PAGE))
        .map_err(|_| error(format_args!("an image of {:#x} bytes, more than PE's 4 GiB", last - base)))?;
    let entry = match &pe.output.kind {
        OutputKind::Executable { entry } => {
            let addr = placed.entry.ok_or_else(|| error(format_args!("the executable's entry `{entry}` is not placed")))?;
            rva(placed, addr, entry)?
        }
        OutputKind::Shared { .. } => match placed.entry {
            Some(addr) => rva(placed, addr, "the DLL's entry")?,
            None => 0,
        },
    };
    let mut code = 0u64;
    let mut initialized = 0u64;
    let mut uninitialized = 0u64;
    for section in sections.iter().filter(|it| !it.reserve) {
        match (section.prot.exec, section.raw()) {
            (true, raw) => code += raw,
            (false, 0) => uninitialized += (section.end - section.start).next_multiple_of(PAGE),
            (false, raw) => initialized += raw,
        }
    }
    let size = |value: u64, what: &str| {
        u32::try_from(value).map_err(|_| error(format_args!("{value:#x} bytes of {what}, more than PE's 4 GiB")))
    };
    let base_of_code = match sections.iter().find(|it| it.prot.exec) {
        Some(section) => rva(placed, section.start, "the code")?,
        None => 0,
    };
    let dll = pe.dll();

    let mut out = Buf::with_capacity(headers);
    out.put_u16(pe::IMAGE_DOS_SIGNATURE);
    for field in [0x90, 3, 0, 4, 0, 0xffff, 0, 0xb8, 0, 0, 0, 0x40, 0] {
        out.put_u16(field);
    }
    out.put_bytes(&[0; 32]);
    out.put_u32(NT as u32);
    out.put_bytes(DOS_PROGRAM);
    out.0.resize(NT, 0);

    out.put_u32(pe::IMAGE_NT_SIGNATURE);
    out.put_u16(pe::IMAGE_FILE_MACHINE_AMD64.0);
    out.put_u16(sections.len() as u16);
    out.put_u32(stamp(placed));
    out.put_u32(0);
    out.put_u32(0);
    out.put_u16(OPTIONAL as u16);
    let mut characteristics = pe::IMAGE_FILE_EXECUTABLE_IMAGE | pe::IMAGE_FILE_LARGE_ADDRESS_AWARE;
    if dll {
        characteristics |= pe::IMAGE_FILE_DLL;
    }
    out.put_u16(characteristics.0);

    out.put_u16(pe::IMAGE_NT_OPTIONAL_HDR64_MAGIC);
    out.put_u8(LINKER_VERSION);
    out.put_u8(0);
    out.put_u32(size(code, "code")?);
    out.put_u32(size(initialized, "data")?);
    out.put_u32(size(uninitialized, "zeroed data")?);
    out.put_u32(entry);
    out.put_u32(base_of_code);
    out.put_u64(base);
    out.put_u32(PAGE as u32);
    // A region's file offset is its RVA, so the file is aligned as memory is.
    out.put_u32(PAGE as u32);
    out.put_u16(WINDOWS_VERSION);
    out.put_u16(0);
    out.put_u16(0);
    out.put_u16(0);
    out.put_u16(WINDOWS_VERSION);
    out.put_u16(0);
    out.put_u32(0);
    out.put_u32(size_of_image);
    out.put_u32(size(size_of_headers, "headers")?);
    out.put_u32(0);
    out.put_u16(if pe.output.windows_subsystem {
        pe::IMAGE_SUBSYSTEM_WINDOWS_GUI.0
    } else {
        pe::IMAGE_SUBSYSTEM_WINDOWS_CUI.0
    });
    let mut dll_characteristics = pe::IMAGE_DLLCHARACTERISTICS_HIGH_ENTROPY_VA
        | pe::IMAGE_DLLCHARACTERISTICS_DYNAMIC_BASE
        | pe::IMAGE_DLLCHARACTERISTICS_NX_COMPAT;
    if !dll {
        dll_characteristics |= pe::IMAGE_DLLCHARACTERISTICS_TERMINAL_SERVER_AWARE;
    }
    out.put_u16(dll_characteristics.0);
    out.put_u64(pe.output.stack.unwrap_or(STACK_RESERVE));
    out.put_u64(COMMIT);
    out.put_u64(HEAP_RESERVE);
    out.put_u64(COMMIT);
    out.put_u32(0);
    out.put_u32(DIRECTORIES as u32);
    for (at, len) in directories {
        out.put_u32(at);
        out.put_u32(len);
    }

    let mut len = size_of_headers;
    for section in &sections {
        let start = rva(placed, section.start, "a section")?;
        let raw = section.raw();
        out.put_bytes(&section.name);
        out.put_u32(size(section.end - section.start, "a section")?);
        out.put_u32(start);
        out.put_u32(size(raw, "a section")?);
        out.put_u32(if raw > 0 { start } else { 0 });
        out.put_u32(0);
        out.put_u32(0);
        out.put_u16(0);
        out.put_u16(0);
        out.put_u32(section.characteristics().0);
        if raw > 0 {
            len = len.max(u64::from(start) + raw);
        }
    }
    // alloc: once per link: the one part `File` holds, the headers, and its `Arc`
    Ok(File { len, parts: vec![(0, CowBytes::from_vec(out.0))], debug: Vec::new() })
}
