//! The prelink: the image's pieces placed and relocated by blaze-link at a fixed base, as a
//! shared image whose undefined and weakly defined names are bound by the run
//! ([`Prelinked::binds`]), with every function, static and global name in a table.
//!
//! [`Prelink`] is the hook into blaze-link (`v2/requests/fastcompile.md`, fastcompile-premap
//! 1): blaze-link's stage runs it, in parallel as any full link is. [`build`] asks for it,
//! checks what came back against what was asked (every piece placed, segments page-aligned
//! and apart, every bind and init cell inside a writable segment) and encodes the image.

use blaze_codegen::{Arch, InstanceKey};
use blaze_graph::par;
use blaze_ids::ContentId;
use blaze_link::premap as link;
pub use blaze_link::premap::{
    Bind, BindKind, InitEntry, Library, PlacedName, PlacedStatic, Prot, StaticAt, Tls, page,
};
use blaze_link::{Format, Output, OutputKind};

use crate::PremapError;
use crate::image::{self, Meta, Pieces};
use crate::key::ImageKey;
use crate::table::{self, Def, Kind, Strength, SymbolId, TableView};

/// What the prelink links: blaze-link's request (its output is [`output`]: a shared image,
/// never incremental or live; its `tls_bias` is where the workspace executable's own
/// thread-local block holds the image's part, [`crate::key::GraphInputs::tls_bias`]), and
/// codegen's digests of its `functions`, `statics` and `debug_names`, in their order: kept
/// in the image so a build checks its pieces against it with no hash of its own.
pub struct Request<'a> {
    pub link: link::Request<'a>,
    pub function_digests: &'a [ContentId],
    pub static_digests: &'a [ContentId],
    pub debug_digests: &'a [ContentId],
}

/// blaze-link, linking a dependency image.
pub trait Prelink: Sync {
    type Placed: Placed;
    fn prelink(&self, request: &Request<'_>) -> Result<Self::Placed, PremapError>;
}

/// A prelink placed and relocated, its segments' bytes written only into the image's value,
/// so a cold prelink writes them once.
pub trait Placed {
    /// What the prelink says; its segments' `bytes` may be empty, [`Placed::len`] says how
    /// many it has.
    fn linked(&self) -> &Prelinked;
    /// How many bytes segment `at` has from its start; zeroes follow to its size.
    fn len(&self, at: usize) -> u64;
    /// Writes each segment's bytes at its offset of `out`: one offset per segment, increasing,
    /// each a multiple of the page and its segment inside `out`.
    fn write_into(&self, out: &mut [u8], offsets: &[usize]) -> Result<(), PremapError>;
}

/// A prelink whose segments hold their bytes: written by a copy of each.
impl Placed for Prelinked {
    fn linked(&self) -> &Prelinked {
        self
    }

    fn len(&self, at: usize) -> u64 {
        self.segments[at].bytes.len() as u64
    }

    fn write_into(&self, out: &mut [u8], offsets: &[usize]) -> Result<(), PremapError> {
        if offsets.len() != self.segments.len() {
            return Err(PremapError::of(format_args!("{} offsets for {} segments", offsets.len(), self.segments.len())));
        }
        let mut rest = out;
        let mut consumed = 0;
        for (segment, offset) in self.segments.iter().zip(offsets) {
            let len = segment.bytes.len();
            let fits = *offset >= consumed && offset.checked_add(len).is_some_and(|end| end - consumed <= rest.len());
            if !fits {
                return Err(PremapError::of(format_args!(
                    "the segment at {:#x} does not fit at offset {offset} of the image",
                    segment.addr
                )));
            }
            let (_, from) = std::mem::take(&mut rest).split_at_mut(offset - consumed);
            let (into, after) = from.split_at_mut(len);
            par::for_each(par::zip(par::chunks_mut(into, COPY_PIECE), par::chunks(&segment.bytes, COPY_PIECE)), |(into, from)| {
                into.copy_from_slice(from);
            });
            rest = after;
            consumed = offset + len;
        }
        Ok(())
    }
}

/// Segment bytes are copied in pieces of this size, in parallel.
const COPY_PIECE: usize = 1 << 20;

/// blaze-link's prelink ([`blaze_link::premap::prelink_placed`]), whose types mirror these
/// field for field.
#[derive(Clone, Copy, Default, Debug)]
pub struct Link;

/// [`Link`]'s answer: blaze-link's placed prelink, which writes the segments itself.
pub struct LinkPlaced {
    placed: link::Placed,
    linked: Prelinked,
}

impl Placed for LinkPlaced {
    fn linked(&self) -> &Prelinked {
        &self.linked
    }

    fn len(&self, at: usize) -> u64 {
        self.placed.segments[at].len
    }

    fn write_into(&self, out: &mut [u8], offsets: &[usize]) -> Result<(), PremapError> {
        self.placed
            .write_into(out, offsets)
            .map_err(|error| PremapError::of(format_args!("writing the dependency image's segments: {error}")))
    }
}

impl Prelink for Link {
    type Placed = LinkPlaced;

    fn prelink(&self, request: &Request<'_>) -> Result<LinkPlaced, PremapError> {
        let mut placed = link::prelink_placed(&request.link)
            .map_err(|error| PremapError::of(format_args!("the prelink of a dependency image: {error}")))?;
        let mut linked = from_link(std::mem::take(&mut placed.linked));
        let place = |it: &link::SegmentPlace| Segment {
            addr: it.addr,
            size: it.size,
            prot: it.prot,
            bytes: Vec::new(), // alloc: none: the bytes are written by `write_into`, so the list stays empty
        };
        linked.segments = placed.segments.iter().map(place).collect(); // alloc: once per image made: its segments' places
        Ok(LinkPlaced { placed, linked })
    }
}

/// What blaze-link gave back, with no segment bytes: `Placed::write_into` writes them.
fn from_link(linked: link::Prelinked) -> Prelinked {
    Prelinked {
        segments: Vec::new(), // alloc: none: the segments come from `Placed::segments`
        functions: linked.functions,
        statics: linked.statics,
        names: linked.names,
        binds: linked.binds,
        libraries: linked.libraries,
        init: linked.init,
        fini: linked.fini,
        tls: linked.tls,
        unwind: linked.unwind,
        thread_vars: linked.thread_vars,
        reached_directly: linked.reached_directly,
    }
}

/// A segment of the image: `bytes` from `addr`, then zeroes to `size`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Segment {
    pub addr: u64,
    pub size: u64,
    pub prot: Prot,
    pub bytes: Vec<u8>,
}

/// What blaze-link gives back.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct Prelinked {
    /// In address order.
    pub segments: Vec<Segment>,
    pub functions: Vec<(InstanceKey, u64, u64)>,
    pub statics: Vec<PlacedStatic>,
    pub names: Vec<PlacedName>,
    pub binds: Vec<Bind>,
    pub libraries: Vec<Library>,
    pub init: Vec<InitEntry>,
    pub fini: Vec<InitEntry>,
    pub tls: Option<Tls>,
    /// Call frame information (`.eh_frame`, `__eh_frame`) to register, as address and length.
    pub unwind: Vec<(u64, u64)>,
    /// Mach-O: the thread-local descriptors (`__thread_vars`) the run's loader fills.
    pub thread_vars: Vec<(u64, u64)>,
    /// The names the image defines weakly and its own code reaches other than through a
    /// bind, so no definition beside the image may win over them. Sorted, each once.
    pub reached_directly: Vec<String>,
}

/// The top of a 39-bit address space, the smallest AArch64 Linux configures: no segment of a
/// dependency image reaches past it.
pub const END: u64 = 1 << 39;

/// The linked address every dependency image of an output starts at, one per format and
/// never per workspace, so every workspace build of a graph shares its image. It is the start
/// of the workspace executable's reservation for fixed maps (blaze-inproc's `FixedMap`), and
/// the executable's own segments follow the image's end ([`blaze_link::Image::beside`]), so
/// calls between the two stay within reach. The image slides with the executable. Each is a
/// multiple of 64 KiB, every target's mapping granule:
///
/// - ELF: 4 MiB, where executables customarily start; the executable's headers go in its own
///   first segment, after the image;
/// - Mach-O: 4 GiB, just past `__PAGEZERO`;
/// - PE: one granule past the executable's base `0x1_4000_0000`, where its headers are.
pub fn base(format: Format, arch: Arch) -> Result<u64, PremapError> {
    match (format, arch) {
        (Format::Wasm, _) | (_, Arch::Wasm32) => {
            Err(PremapError::of(format_args!("wasm has no dependency image: code is reached by index, not mapped")))
        }
        (Format::Elf, _) => Ok(0x40_0000),
        (Format::MachO, _) => Ok(0x1_0000_0000),
        (Format::Pe, _) => Ok(0x1_4001_0000),
    }
}

/// The output a dependency image links as, from the dev run's: a shared image under the
/// key's name, of the same format, target, platform and debug info, with no room to grow and
/// no live patching (an image is replaced, never patched).
pub fn output(dev: &Output, key_name: &str) -> Output {
    Output {
        format: dev.format,
        arch: dev.arch,
        kind: OutputKind::Shared { name: key_name.to_owned(), exports: Vec::new() }, // alloc: setup, once per image made
        interpreter: None,
        platform: dev.platform,
        incremental: false,
        debug: dev.debug,
        live: None,
        tags: dev.tags,
        stack: dev.stack,
        windows_subsystem: dev.windows_subsystem,
        dev_run: None,
        triple: dev.triple.clone(), // clone: setup, once per image made: the output owns it
        exec_log: false,
        sanitize: dev.sanitize,
        rpaths: dev.rpaths.clone(), // clone: setup, once per image made: the output owns them
    }
}

/// The image of `request`, through `linker`, checked and encoded under `key`.
pub fn build(linker: &impl Prelink, request: &Request<'_>, key: ImageKey) -> Result<Vec<u8>, PremapError> {
    if request.link.output.incremental || request.link.output.live.is_some() {
        return Err(PremapError::of(format_args!(
            "a dependency image is linked whole: its output is neither incremental nor live"
        )));
    }
    let page = page(request.link.output.format, request.link.output.arch);
    let from = base(request.link.output.format, request.link.output.arch)?;
    if request.link.base % page != 0 || !(from..END).contains(&request.link.base) {
        return Err(PremapError::of(format_args!(
            "the image's base {:#x} is not page-aligned in {from:#x}..{END:#x}",
            request.link.base
        )));
    }
    let placed = linker.prelink(request)?;
    check(request, &placed, page)?;
    let linked = placed.linked();

    let mut entries: Vec<(SymbolId, Def)> =
        Vec::with_capacity(linked.functions.len() + linked.statics.len() + linked.names.len());
    for (key, addr, size) in &linked.functions {
        let def = Def { value: *addr, size: *size, kind: Kind::Function, strength: Strength::Strong, order: u32::MAX };
        entries.push((SymbolId::function(key), def));
    }
    for placed in &linked.statics {
        let (value, kind) = match placed.at {
            StaticAt::Addr(addr) => (addr, Kind::Data),
            StaticAt::ThreadLocal(offset) => (offset, Kind::ThreadLocal),
        };
        let def = Def { value, size: placed.size, kind, strength: Strength::Strong, order: u32::MAX };
        entries.push((SymbolId::of_static(&placed.item, &placed.instance), def));
    }
    for name in &linked.names {
        entries.push((SymbolId::name(&name.name), name.def));
    }
    let mut table = table::build(&entries)?;
    let fail = |what: std::fmt::Arguments<'_>| Err(PremapError::of(format_args!("the prelink of the dependency image {what}")));
    let in_table = |table: &Vec<u8>, id: &SymbolId| TableView::trusted(table).get(id).is_some();

    if request.function_digests.len() != request.link.functions.len()
        || request.static_digests.len() != request.link.statics.len()
        || request.debug_digests.len() != request.link.debug_names.len()
    {
        return Err(PremapError::of(format_args!(
            "the dependency image was asked with digests not one per function, static and debug name"
        )));
    }
    let mut contents = Vec::with_capacity(request.link.functions.len() + request.link.statics.len());
    for (function, digest) in request.link.functions.iter().zip(request.function_digests) {
        let id = SymbolId::function(&function.key);
        if !in_table(&table, &id) {
            return fail(format_args!("placed no {:?}", function.key));
        }
        contents.push(*digest);
        table::set_content(&mut table, &id, contents.len() as u32)?;
    }
    let mut statics = Vec::with_capacity(request.link.statics.len());
    for (value, definition) in request.link.statics.iter().zip(request.static_digests) {
        let id = SymbolId::of_static(&value.item, &value.instance);
        if !in_table(&table, &id) {
            return fail(format_args!("placed no static {:?}", value.item));
        }
        contents.push(*definition);
        table::set_content(&mut table, &id, contents.len() as u32)?;
        statics.push(((value.item, value.instance), *definition));
    }
    statics.sort_unstable_by_key(|it| it.0);
    for (name, _) in request.link.names {
        if !in_table(&table, &SymbolId::name(name)) {
            return fail(format_args!("defined no `{name}`"));
        }
    }
    for (at, (function, _)) in request.link.debug_names.iter().enumerate() {
        table::set_debug(&mut table, &SymbolId::function(function), at as u32 + 1)?;
    }
    for name in &linked.reached_directly {
        table::set_reached_directly(&mut table, &SymbolId::name(name))?;
    }
    let meta = Meta { key, format: request.link.output.format, arch: request.link.output.arch, base: request.link.base, page };
    let pieces = Pieces {
        table: &table,
        contents: &contents,
        debug: request.debug_digests,
        statics: &statics,
        names: request.link.names.len() as u64,
    };
    image::encode(&meta, &placed, &pieces)
}

/// What came back is what was asked, and maps.
fn check(request: &Request<'_>, placed: &impl Placed, page: u64) -> Result<(), PremapError> {
    let fail = |what: std::fmt::Arguments<'_>| Err(PremapError::of(format_args!("the prelink of the dependency image {what}")));
    let linked = placed.linked();
    let mut end = request.link.base;
    for (at, segment) in linked.segments.iter().enumerate() {
        if segment.addr % page != 0 || segment.addr < end {
            return fail(format_args!("placed a segment at {:#x}, not page-aligned after {end:#x}", segment.addr));
        }
        if placed.len(at) > segment.size || segment.size == 0 {
            return fail(format_args!("made a segment at {:#x} of {} bytes in {}", segment.addr, placed.len(at), segment.size));
        }
        end = match segment.addr.checked_add(segment.size).filter(|end| *end <= END) {
            Some(end) => end,
            None => {
                return fail(format_args!(
                    "made a segment at {:#x} reaching past {END:#x}, the top of the smallest address space",
                    segment.addr
                ));
            }
        };
    }
    let inside = |addr: u64, len: u64, writable: bool| {
        linked.segments.iter().any(|it| {
            addr >= it.addr && addr.saturating_add(len) <= it.addr + it.size && (!writable || it.prot == Prot::Data)
        })
    };

    for (key, addr, size) in &linked.functions {
        if !inside(*addr, (*size).max(1), false) {
            return fail(format_args!("placed {key:?} at {addr:#x}, outside its segments"));
        }
    }
    for bind in &linked.binds {
        if bind.place % 8 != 0 || !inside(bind.place, 8, true) {
            return fail(format_args!("binds `{}` at {:#x}, not an aligned cell of a writable segment", bind.name, bind.place));
        }
    }
    for entry in linked.init.iter().chain(&linked.fini) {
        if entry.at % 8 != 0 || !inside(entry.at, u64::from(entry.count) * 8, false) {
            return fail(format_args!("made an init or fini piece at {:#x} outside its segments", entry.at));
        }
    }
    if let Some(tls) = linked.tls {
        if tls.init > tls.size || !tls.align.is_power_of_two() || (tls.init > 0 && !inside(tls.template, tls.init, false)) {
            return fail(format_args!("made a thread-local template at {:#x} that is not one", tls.template));
        }
    }
    for (addr, len) in &linked.unwind {
        if !inside(*addr, *len, false) {
            return fail(format_args!("made call frame information at {addr:#x} outside its segments"));
        }
    }
    for (addr, len) in &linked.thread_vars {
        if addr % 8 != 0 || len % 24 != 0 || !inside(*addr, *len, true) {
            return fail(format_args!("made thread-local descriptors at {addr:#x} outside its writable segments"));
        }
    }
    Ok(())
}
