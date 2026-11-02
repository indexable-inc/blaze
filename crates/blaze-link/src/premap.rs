//! Dependency images (`v2/requests/fastcompile.md`, fastcompile-premap 1 and 2): a run maps a
//! prelinked image of the workspace's dependencies at a fixed address and links the
//! workspace's own code beside it, so the two behave as one whole link.
//!
//! - [`prelink_placed`] links the image whole, at the request's base, never slid: its segments,
//!   where every function, static and global name is, the cells the run binds, its libraries,
//!   init and fini pieces, thread-local template, call frame information and Mach-O
//!   thread-local descriptors. Its thread-locals are at `tls_bias` from offset 0 of the run's
//!   area ([`Request::tls_bias`]).
//! - [`crate::Image::beside`] makes the workspace's image against an [`Outside`], the image
//!   mapped beside it: first byte at the outside's end rounded up to the page, thread-locals
//!   after the outside's at their alignment. Its stages ([`crate::Image::stage_beside`])
//!   resolve as a whole link of both would: the change's own definitions first, then the
//!   outside's (a strong definition beats a weak one, of two weak ones the first on the line
//!   wins, two strong ones are the duplicate symbol error), then archive members, pulled only
//!   for what is still undefined. [`layout`] says what the run's start needs of it.
//!
//! blaze-premap's prelink and table use these types as they are (it depends on blaze-link, so
//! blaze-link names none of its types); its `Prelinked` adds the segments' bytes, and
//! [`WorkspaceLayout`] is its `start::WorkspaceLink`, owned.
//!
//! Every thread-local reference of such an image is a fixed offset from the thread pointer
//! ([`crate::state::State::tls_at`]): the initial-exec and local-exec models. A dynamic one
//! (a `__tls_get_addr` pair, a TLS descriptor) is an error, never a slower path.

use blaze_codegen::{Arch, CompiledFunction, InstanceKey, Static, Target};
use blaze_core::FileName;
use blaze_graph::par;
use blaze_hash::HashSet;
use blaze_ids::ContentId;
use blaze_items::ItemKey;
use blaze_mem::align::checked_align_up_u64;

use crate::arena::{FrameVec, collect_in};
use crate::model::{self, Apply, Class, Fixup, Holds, Plan, Sym};
use crate::stage::{Content, Stage, error, refused};
use crate::state::{self, State, What};
use crate::symbols::{IdNo, SymKey, Symbols};
use crate::{Change, Format, Image, LinkError, Native, Output, OutputKind, Refusal};

/// What the prelink links.
pub struct Request<'a> {
    /// The output it links as: a shared image, never incremental, live or a dev run.
    pub output: &'a Output,
    /// The address of the image's first byte, a multiple of [`page`]: it is never slid.
    pub base: u64,
    /// Where the run's loader keeps the program's thread-locals: the image's are at
    /// `tls_bias + offset`, from offset 0 (from the thread pointer on ELF, from the start of
    /// the module's block on PE; 0 on Mach-O, whose descriptors hold the offset into the
    /// template).
    pub tls_bias: i64,
    pub functions: &'a [&'a CompiledFunction],
    pub statics: &'a [&'a Static],
    pub names: &'a [&'a (String, Target)],
    pub debug_names: &'a [&'a (InstanceKey, String)],
    pub natives: &'a [Native<'a>],
    pub spans: &'a [(ContentId, &'a [u8])],
}

/// A run of the address space with one protection.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Prot {
    /// Read and execute, shared by every run.
    Code,
    /// Read only, shared.
    ReadOnly,
    /// Read and write: each run gets its own copy of the page on its first write.
    Data,
}

/// Where a static is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StaticAt {
    Addr(u64),
    /// A thread-local: its offset from the thread pointer.
    ThreadLocal(u64),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PlacedStatic {
    pub item: ItemKey,
    pub instance: ContentId,
    pub at: StaticAt,
    pub size: u64,
}

/// What a symbol's value is.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Kind {
    Function,
    /// A static or a native datum: `value` is its address.
    Data,
    /// A thread-local: `value` is its offset from the thread pointer, as the image's code
    /// reaches it (blaze-premap's `start::TlsBlock`: the image's block is the first).
    ThreadLocal,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Strength {
    Strong,
    /// A weak definition (or a common): a strong one elsewhere wins; of two weak ones the
    /// first on the link line ([`Def::order`]).
    Weak,
}

/// A symbol's definition.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Def {
    pub value: u64,
    pub size: u64,
    pub kind: Kind,
    pub strength: Strength,
    /// Its input's place on the link line: `u32::MAX` for Rust's own pieces, which sort after
    /// every native.
    pub order: u32,
}

/// A global name the image defines: Rust's (`#[no_mangle]`) and every global of a native
/// object in it, so a workspace reference finds it here and never pulls a second copy.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: blaze-premap's `Prelinked` derives it, once per image made
pub struct PlacedName {
    pub name: String,
    pub def: Def,
}

/// How a bound cell is written.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum BindKind {
    /// The address of the symbol plus the addend, 64 bits.
    Addr,
    /// The symbol's offset from the thread pointer plus the addend, 64 bits.
    TpOff,
}

/// An 8-byte cell (a GOT entry, a pointer in data) of a writable segment the run writes
/// before anything runs: a name the image does not define, or defines only weakly (so a
/// strong definition in the workspace wins, as in a whole link).
#[derive(Clone, PartialEq, Eq, Debug)] // clone: blaze-premap's `Prelinked` derives it, once per image made
pub struct Bind {
    pub place: u64,
    pub name: String,
    pub addend: i64,
    pub kind: BindKind,
    /// Referred to weakly: 0 when nothing defines it.
    pub weak: bool,
}

/// A shared library the image imports from.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: blaze-premap's `Prelinked` derives it, once per image made
pub struct Library {
    /// What the loader finds it by (`DT_SONAME`, install name, DLL name).
    pub name: String,
    /// Its place on the link line.
    pub order: u32,
    /// Left out when none of its symbols is used.
    pub as_needed: bool,
}

/// One piece of an init or fini array: `count` 8-byte function pointers at `at`. A whole
/// link orders pieces by `rank` (`.init_array.NNNNN`'s priority; 65536 for none), then
/// by `order` (the input's place on the line, its member and its section; `u32::MAX` each
/// for Rust's pieces, which go after natives), then by `tie` (Rust's: the static's item and
/// instance); within a piece the pointers run in address order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct InitEntry {
    pub rank: u32,
    pub order: [u32; 3],
    pub tie: [u8; 64],
    pub at: u64,
    pub count: u32,
}

impl InitEntry {
    pub fn sort_key(&self) -> (u32, [u32; 3], [u8; 64]) {
        (self.rank, self.order, self.tie)
    }

    /// The tie of a Rust static's piece: its item's id, then its instance's.
    pub fn tie_of(item: &ItemKey, instance: &ContentId) -> [u8; 64] {
        let mut tie = [0; 64];
        tie[..32].copy_from_slice(&item.0.0);
        tie[32..].copy_from_slice(&instance.0);
        tie
    }
}

/// The thread-local template: initialized bytes at `template`, `init` long, then zeroes to
/// `size`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Tls {
    pub template: u64,
    pub init: u64,
    pub size: u64,
    pub align: u64,
}

/// What the prelink gives back.
#[derive(Default, PartialEq, Eq, Debug)]
pub struct Prelinked {
    /// Each function asked for: its key, address and size.
    pub functions: Vec<(InstanceKey, u64, u64)>,
    pub statics: Vec<PlacedStatic>,
    pub names: Vec<PlacedName>,
    /// In place order.
    pub binds: Vec<Bind>,
    pub libraries: Vec<Library>,
    pub init: Vec<InitEntry>,
    pub fini: Vec<InitEntry>,
    pub tls: Option<Tls>,
    /// Call frame information (`.eh_frame`, `__eh_frame`) to register, as address and length.
    pub unwind: Vec<(u64, u64)>,
    /// Mach-O: the thread-local descriptors (`__thread_vars`) the run's loader fills.
    pub thread_vars: Vec<(u64, u64)>,
    /// The names it defines weakly that its own code reaches some way but a bind (a
    /// PC-relative call or address, a difference): a definition beside it can't take those
    /// references over, so one that would win is an error ([`Outside::reached_directly`]).
    /// Sorted, each once.
    pub reached_directly: Vec<String>,
}

/// What the workspace link made, as the run's start needs it: `WorkspaceLink`'s fields,
/// owned.
#[derive(Default, PartialEq, Eq, Debug)]
pub struct WorkspaceLayout {
    /// The global names it defines, sorted by name, each once.
    pub names: Vec<(String, Def)>,
    pub init: Vec<InitEntry>,
    pub fini: Vec<InitEntry>,
    pub tls: Option<Tls>,
    /// Where its link put its thread-locals in the run's area: the offset from `tls_bias`.
    pub tls_offset: Option<u64>,
    /// Mach-O: its thread-local descriptors (`__thread_vars`), as address and length.
    pub thread_vars: Vec<(u64, u64)>,
    pub libraries: Vec<Library>,
    pub unwind: Vec<(u64, u64)>,
}

/// The image an image is linked beside: what it defines, and where it ends. Fixed for the
/// life of the image linked beside it. Stages are generic over it, so the image with none
/// ([`NoOutside`]) pays nothing.
pub trait Outside {
    /// Whether it can define anything: `false` only for [`NoOutside`], whose lookups every
    /// stage then skips.
    const ANY: bool = true;

    /// Where a function the image placed is.
    fn function(&self, key: &InstanceKey) -> Option<u64>;

    /// Where a static the image placed is.
    fn static_at(&self, item: &ItemKey, instance: &ContentId) -> Option<StaticAt>;

    /// A global name the image defines, and how strongly.
    fn name(&self, name: &str) -> Option<Def>;

    /// Whether `name` is one the image defines weakly and its own code reaches some way but a
    /// bind ([`Prelinked::reached_directly`]): a definition linked beside it that a whole link
    /// would pick is then an error, as the image's code keeps reaching its own.
    fn reached_directly(&self, name: &str) -> bool;

    /// The end of its thread-locals in the run's area, from `tls_bias`: 0 with none.
    fn tls_end(&self) -> u64;

    /// The address past its last segment.
    fn end(&self) -> u64;
}

/// No image beside: every link but a workspace's beside a dependency image.
#[derive(Clone, Copy, Default, Debug)]
pub struct NoOutside;

impl Outside for NoOutside {
    const ANY: bool = false;

    #[inline(always)]
    fn function(&self, _: &InstanceKey) -> Option<u64> {
        None
    }

    #[inline(always)]
    fn static_at(&self, _: &ItemKey, _: &ContentId) -> Option<StaticAt> {
        None
    }

    #[inline(always)]
    fn name(&self, _: &str) -> Option<Def> {
        None
    }

    #[inline(always)]
    fn reached_directly(&self, _: &str) -> bool {
        false
    }

    fn tls_end(&self) -> u64 {
        0
    }

    fn end(&self) -> u64 {
        0
    }
}

/// Where an image of a run with a dependency image is fixed: its base and page, and where its
/// thread-locals start in the run's area.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Place {
    pub base: u64,
    pub page: u64,
    pub tls_bias: i64,
    /// Its thread-locals start here from `tls_bias`, rounded up to their alignment.
    pub tls_from: u64,
    /// The outside's end ([`Outside::end`]); 0 for a dependency image.
    pub end: u64,
    /// Linked beside an outside, rather than a dependency image itself.
    pub beside: bool,
}

/// The granule every segment is aligned to: the largest a system of the target maps a memory
/// object by (16 KiB pages on Apple silicon, 64 KiB pages on AArch64 Linux kernels built so,
/// Windows' 64 KiB allocation granularity for a view's address and offset).
pub fn page(format: Format, arch: Arch) -> u64 {
    match (format, arch) {
        (Format::MachO, Arch::Aarch64) => 16 << 10,
        (Format::Pe, _) | (_, Arch::Aarch64) => 64 << 10,
        _ => 4 << 10,
    }
}

/// The writer's plan, fixed at `place`.
pub(crate) fn fix(plan: &mut Plan, place: &Place) -> Result<(), String> {
    if plan.indexed_code {
        return Err("wasm has no dependency image: code is reached by index, not mapped".into());
    }
    if !place.page.is_power_of_two() || place.base % place.page != 0 {
        // alloc: error path
        return Err(format!("an image at {:#x}, not on a page of {:#x} bytes", place.base, place.page));
    }
    plan.base = place.base;
    plan.page = plan.page.max(place.page);
    Ok(())
}

/// What the image beside defines a name as, in this image's terms.
pub(crate) fn outside_def(def: Def) -> state::Def {
    match def.kind {
        Kind::ThreadLocal => state::Def::TpOff(def.value as i64),
        Kind::Function | Kind::Data => state::Def::Absolute(def.value),
    }
}

/// Where a segment is: its first `len` bytes are the image's, then zeroes to `size`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SegmentPlace {
    pub addr: u64,
    pub size: u64,
    pub prot: Prot,
    pub len: u64,
}

/// A prelink placed and relocated, its segments' bytes not yet copied out: the caller sizes
/// its image from [`Placed::segments`] and has them written into it ([`Placed::write_into`]),
/// so a cold prelink writes the image's bytes once.
pub struct Placed {
    /// In address order.
    pub segments: Vec<SegmentPlace>,
    /// Everything else the prelink says.
    pub linked: Prelinked,
    state: State,
    /// Of each segment, its regions' range of [`State::spaces`].
    groups: Vec<(usize, usize)>,
    page: u64,
}

/// What one copy job writes: of the region's bytes from `at`, as many as `out` holds.
type CopyJob<'s, 'o> = (&'s state::Space, usize, &'o mut [u8]);

/// The bytes a copy job writes at most: a large region is copied by every core.
const COPY_CHUNK: usize = 1 << 20;

impl Placed {
    /// Writes each segment's `len` bytes at its offset of `out`, in parallel: `offsets` has
    /// one per segment, in increasing order, each a multiple of the page and its segment
    /// within `out`. Gaps inside a segment are written as zeroes; nothing else of `out` is.
    pub fn write_into(&self, out: &mut [u8], offsets: &[usize]) -> Result<(), LinkError> {
        if offsets.len() != self.segments.len() {
            // alloc: error path
            return Err(error(None, format!("{} offsets for {} segments", offsets.len(), self.segments.len())));
        }
        // alloc: the copy jobs, once per prelink
        let mut jobs: Vec<CopyJob<'_, '_>> = Vec::new();
        let (mut rest, mut consumed) = (out, 0usize);
        for (at, &offset) in offsets.iter().enumerate() {
            let len = self.segments[at].len as usize;
            let fits = offset >= consumed && offset.checked_add(len).is_some_and(|end| end - consumed <= rest.len());
            if offset as u64 % self.page != 0 || !fits {
                return Err(error(
                    None,
                    // alloc: error path
                    format!(
                        "segment {at} of {len:#x} bytes at offset {offset:#x}: not on a page of {:#x}, overlapping \
                         the one before, or past the buffer's end",
                        self.page
                    ),
                ));
            }
            let (_, tail) = std::mem::take(&mut rest).split_at_mut(offset - consumed);
            let (segment, tail) = tail.split_at_mut(len);
            rest = tail;
            consumed = offset + len;
            self.carve(at, segment, &mut jobs)?;
        }
        par::for_each(jobs, |(space, at, out)| space.bytes.copy_to(at, out));
        Ok(())
    }

    /// Segment `at`'s copy jobs into `out` (its `len` bytes), its gaps zeroed.
    fn carve<'s, 'o>(&'s self, at: usize, mut out: &'o mut [u8], jobs: &mut Vec<CopyJob<'s, 'o>>) -> Result<(), LinkError> {
        let mut addr = self.segments[at].addr;
        for space in self.state.spaces[self.groups[at].0..self.groups[at].1].iter().filter(|it| !it.zero && !it.bytes.is_empty()) {
            let len = space.bytes.len().min(space.region.size as usize);
            let from = space.region.addr.checked_sub(addr).map(|it| it as usize).filter(|it| it + len <= out.len());
            let Some(from) = from else {
                // alloc: error path
                return Err(error(None, format!("the region at {:#x} is out of its segment's order", space.region.addr)));
            };
            let (gap, tail) = std::mem::take(&mut out).split_at_mut(from);
            gap.fill(0);
            let (bytes, tail) = tail.split_at_mut(len);
            out = tail;
            addr = space.region.addr + len as u64;
            jobs.extend(bytes.chunks_mut(COPY_CHUNK).enumerate().map(|(n, chunk)| (space, n * COPY_CHUNK, chunk)));
        }
        out.fill(0);
        Ok(())
    }
}

/// Links `request` whole at its base, and says where everything is: its segments' bytes are
/// written where the caller says ([`Placed::write_into`]).
pub fn prelink_placed(request: &Request<'_>) -> Result<Placed, LinkError> {
    let output = request.output;
    let OutputKind::Shared { name, .. } = &output.kind else {
        return Err(error(None, "a dependency image links as a shared image"));
    };
    if output.incremental || output.live.is_some() || output.dev_run.is_some() {
        return Err(error(None, "a dependency image is linked whole: its output is neither incremental, live nor a dev run"));
    }
    // alloc: error path
    let to = FileName::new(name).ok_or_else(|| error(None, format!("`{name}` is no file name for a dependency image")))?;
    let page = page(output.format, output.arch);
    let place = Place { base: request.base, page, tls_bias: request.tls_bias, tls_from: 0, end: 0, beside: false };
    // clone: the image owns its output, once per image
    let mut image = Image::placed(to, output.clone(), Some(place))?;
    let change = Change {
        added: request.functions.to_vec(), // alloc: the change's lists, once per image
        statics: request.statics.to_vec(), // alloc: once per image
        names: request.names.iter().map(|it| (*it).clone()).collect(), // clone: the change owns its names, once per image
        debug_names: request.debug_names.iter().map(|it| (*it).clone()).collect(), // clone: as above
        natives: request.natives.to_vec(), // alloc: once per image
        spans: request.spans.to_vec(), // alloc: once per image
        ..Change::default()
    };
    let owned = image.stage(change)?.state;
    let state = &owned;
    let symbols = &image.symbols;
    let (binds, reached_directly) = binds(state, symbols)?;
    let thread_vars = regions_of(state, |it| *it == Class::ThreadVars);
    // alloc: each list of the answer, once per image
    let mut functions = Vec::with_capacity(request.functions.len());
    for function in request.functions {
        let piece = symbols.find_body(&function.key).and_then(|it| state.home(it)).and_then(|it| state.piece(it.0));
        let piece = piece.ok_or_else(|| error(Some(function.key), "the prelink placed no code for it"))?;
        functions.push((function.key, piece.addr, piece.size));
    }
    let mut statics = Vec::with_capacity(request.statics.len());
    for it in request.statics {
        let sym = match (symbols.find_id(&it.item.0), symbols.find_id(&it.instance)) {
            (Some(item), Some(instance)) => symbols.find(SymKey::Static(item, instance)),
            _ => None,
        };
        let piece = sym.and_then(|it| state.home(it)).and_then(|it| state.piece(it.0));
        // alloc: error path
        let piece = piece.ok_or_else(|| error(None, format!("the prelink placed no static {:?}", it.item)))?;
        let at = if it.thread_local {
            StaticAt::ThreadLocal(thread_local(state, piece.addr).ok_or_else(|| {
                // alloc: error path
                error(None, format!("the thread-local static {:?} is outside the thread-local template", it.item))
            })?)
        } else {
            StaticAt::Addr(piece.addr)
        };
        statics.push(PlacedStatic { item: it.item, instance: it.instance, at, size: piece.size });
    }
    let (segments, groups) = segments(state, page, &binds, &thread_vars);
    let linked = Prelinked {
        // alloc: none: an empty `Vec` allocates nothing
        functions,
        statics,
        // alloc: the answer's names, once per prelink
        names: names(state, symbols).into_iter().map(|(name, def)| PlacedName { name, def }).collect(),
        binds,
        libraries: libraries(state),
        init: init_entries(state, symbols, Class::InitArray),
        fini: init_entries(state, symbols, Class::FiniArray),
        tls: tls(state),
        unwind: regions_of(state, |it| *it == Class::EhFrame),
        thread_vars,
        reached_directly,
    };
    Ok(Placed { segments, linked, state: owned, groups, page })
}

/// What the run's start needs of an image linked beside a dependency image, as committed.
pub fn layout(image: &Image) -> WorkspaceLayout {
    let state = &image.state;
    let bias = image.place.map_or(0, |it| it.tls_bias);
    WorkspaceLayout {
        names: names(state, &image.symbols),
        init: init_entries(state, &image.symbols, Class::InitArray),
        fini: init_entries(state, &image.symbols, Class::FiniArray),
        tls: tls(state),
        tls_offset: state.tls.and(state.tls_at).map(|at| at.wrapping_sub(bias) as u64),
        thread_vars: regions_of(state, |it| *it == Class::ThreadVars),
        libraries: libraries(state),
        unwind: regions_of(state, |it| *it == Class::EhFrame),
    }
}

/// The offset from the thread pointer of the thread-local at `addr` of the image's template.
fn thread_local(state: &State, addr: u64) -> Option<u64> {
    let (tls, at) = (state.tls?, state.tls_at?);
    let offset = addr.checked_sub(tls.addr).filter(|it| *it <= tls.size)?;
    Some(at.wrapping_add(offset as i64) as u64)
}

/// A symbol's value, kind and size through its aliases; `None` for what the image does not
/// define (an import, the linker's).
fn value_of(state: &State, mut sym: Sym) -> Option<(u64, Kind, u64)> {
    for _ in 0..64 {
        match state.def(sym) {
            state::Def::Piece { piece, offset } => {
                let piece = state.piece(piece)?;
                let region = &state.spaces.get(piece.region as usize)?.region;
                let (addr, size) = (piece.addr + offset, if offset == 0 { piece.size } else { 0 });
                return Some(match region.class {
                    Class::Tdata | Class::Tbss => (thread_local(state, addr)?, Kind::ThreadLocal, size),
                    _ if region.prot.exec => (addr, Kind::Function, size),
                    _ => (addr, Kind::Data, size),
                });
            }
            state::Def::Absolute(value) => return Some((value, Kind::Data, 0)),
            state::Def::TlsOffset(offset) => {
                return Some((state.tls_at?.wrapping_add(offset as i64) as u64, Kind::ThreadLocal, 0));
            }
            state::Def::TpOff(offset) => return Some((offset as u64, Kind::ThreadLocal, 0)),
            state::Def::Alias(to) => sym = to,
            state::Def::Import(_) | state::Def::Special(_) | state::Def::None => return None,
        }
    }
    None
}

/// Every global name the image defines, sorted by name: Rust's, strong and after every
/// native, then the natives' as resolution chose them.
fn names(state: &State, symbols: &Symbols) -> Vec<(String, Def)> {
    let natives = state.resolved.native_defs(&state.inputs);
    // alloc: the answer's names, once per prelink or layout
    let mut out: Vec<(String, Def)> = Vec::with_capacity(state.names.len());
    for (name, target) in state.names.iter() {
        let Some((value, kind, size)) = value_of(state, *target) else { continue };
        let def = Def { value, size, kind, strength: Strength::Strong, order: u32::MAX };
        // alloc: the answer owns its names, once per name
        out.push((symbols.str(*name).to_owned(), def));
    }
    for it in natives {
        let (Some(name), Some((value, kind, size))) = (symbols.name_of(it.sym), value_of(state, it.sym)) else { continue };
        let strength = if it.weak { Strength::Weak } else { Strength::Strong };
        let def = Def { value, size: size.max(it.size), kind, strength, order: it.order };
        // alloc: the answer owns its names, once per name
        out.push((name.to_owned(), def));
    }
    out.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    out.dedup_by(|a, b| a.0 == b.0);
    out
}

/// The cells the run binds: those to imports (GOT cells, pointers in data), and those to a
/// name the image defines only weakly, which a strong definition beside wins. With them, the
/// weakly defined names the image reaches some other way ([`Prelinked::reached_directly`]).
fn binds(state: &State, symbols: &Symbols) -> Result<(Vec<Bind>, Vec<String>), LinkError> {
    let imports = &state.resolved.imports;
    let import = |at: u32| imports.get(at as usize).ok_or_else(|| error(None, "a cell bound to no import"));
    // alloc: the answer's binds, once per prelink
    let mut out: Vec<Bind> = Vec::new();
    // alloc: the answer's names reached directly, once per prelink
    let mut direct: Vec<String> = Vec::new();
    for entry in state.lists.got.items.iter() {
        let (at, kind) = match entry.holds {
            Holds::Import(at) => (at, BindKind::Addr),
            Holds::ImportTpOff(at) => (at, BindKind::TpOff),
            Holds::TlsGd { import: Some(_), .. } | Holds::TlsDesc { import: Some(_), .. } => {
                return Err(error(None, "a dynamic thread-local access to an import in a dependency image"));
            }
            _ => continue,
        };
        let import = import(at)?;
        // clone: the answer owns its names, once per bind
        out.push(Bind { place: entry.addr, name: import.name.clone(), addend: 0, kind, weak: import.weak });
    }
    for bind in state.lists.binds.items.iter() {
        let import = import(bind.import)?;
        // clone: the answer owns its names, once per bind
        let name = import.name.clone();
        out.push(Bind { place: bind.addr, name, addend: bind.addend, kind: BindKind::Addr, weak: import.weak });
    }
    // alloc: the weakly defined natives, once per prelink
    let weak: HashSet<Sym> = state.resolved.native_defs(&state.inputs).filter(|it| it.weak).map(|it| it.sym).collect();
    if !weak.is_empty() {
        for piece in state.live_pieces() {
            let name = |sym: Sym| symbols.name_of(sym).map(str::to_owned);
            if let What::Got { kind, target } = piece.what() {
                if !weak.contains(&target) {
                    continue;
                }
                let kind = match kind {
                    crate::reloc::GotKind::Address => BindKind::Addr,
                    crate::reloc::GotKind::TpOff => BindKind::TpOff,
                    _ => return Err(error(None, "a dynamic thread-local access in a dependency image")),
                };
                let Some(name) = name(target) else { continue };
                out.push(Bind { place: piece.addr, name, addend: 0, kind, weak: false });
                continue;
            }
            let writable = state.spaces.get(piece.region as usize).is_some_and(|it| it.region.prot.write);
            for fixup in piece.fixups() {
                if let Some(minus) = fixup.minus.filter(|it| weak.contains(it)) {
                    direct.extend(name(minus));
                }
                if !weak.contains(&fixup.target) || crate::reloc::got_kind(fixup.apply).is_some() {
                    continue;
                }
                let Some(name) = name(fixup.target) else { continue };
                if fixup.apply != Apply::Abs64 || !writable {
                    direct.push(name);
                    continue;
                }
                out.push(Bind { place: piece.addr + fixup.at, name, addend: fixup.addend, kind: BindKind::Addr, weak: false });
            }
        }
    }
    out.sort_unstable_by_key(|it| it.place);
    direct.sort_unstable();
    direct.dedup();
    Ok((out, direct))
}

/// The image's segments: each run of regions of one protection, from its first page to its
/// last region's end, its bytes to the end of the last that has any. A run with a cell the
/// run writes is [`Prot::Data`], whatever the format's own protection (ELF's RELRO). With
/// each, its regions' range of [`State::spaces`].
fn segments(
    state: &State,
    page: u64,
    binds: &[Bind],
    thread_vars: &[(u64, u64)],
) -> (Vec<SegmentPlace>, Vec<(usize, usize)>) {
    // alloc: the answer's segments, once per prelink
    let mut out: Vec<SegmentPlace> = Vec::new();
    // alloc: once per prelink, as above
    let mut groups: Vec<(usize, usize)> = Vec::new();
    let spaces = &state.spaces;
    let mut at = 0;
    while at < spaces.len() {
        let prot = spaces[at].region.prot;
        let run = spaces[at..].iter().take_while(|it| it.region.prot == prot).count();
        let range = (at, at + run);
        let group = &spaces[at..at + run];
        at += run;
        if prot == model::Prot::NONE {
            continue;
        }
        let start = group[0].region.addr / page * page;
        let end = group.iter().map(|it| it.region.addr + it.region.capacity.max(it.region.size)).max().unwrap_or(start);
        if end <= start {
            continue;
        }
        let filled = group
            .iter()
            .filter(|it| !it.zero && !it.bytes.is_empty())
            .map(|it| it.region.addr + it.bytes.len().min(it.region.size as usize) as u64)
            .max()
            .unwrap_or(start);
        let inside = |addr: u64| (start..end).contains(&addr);
        let prot = if prot.exec {
            Prot::Code
        } else if prot.write || binds.iter().any(|it| inside(it.place)) || thread_vars.iter().any(|it| inside(it.0)) {
            Prot::Data
        } else {
            Prot::ReadOnly
        };
        out.push(SegmentPlace { addr: start, size: end - start, prot, len: filled - start });
        groups.push(range);
    }
    (out, groups)
}

/// The address and size of each non-empty region of the classes `of` takes.
fn regions_of(state: &State, of: impl Fn(&Class) -> bool) -> Vec<(u64, u64)> {
    state
        .spaces
        .iter()
        .filter(|it| of(&it.region.class) && it.region.size > 0)
        .map(|it| (it.region.addr, it.region.size))
        // alloc: the answer's ranges, once per prelink or layout
        .collect()
}

fn tls(state: &State) -> Option<Tls> {
    state.tls.map(|it| Tls { template: it.addr, init: it.init, size: it.size, align: it.align })
}

fn libraries(state: &State) -> Vec<Library> {
    let resolved = &state.resolved;
    resolved
        .libraries
        .iter()
        .zip(resolved.library_orders.iter())
        // clone: the answer owns its names, once per library
        .map(|(it, order)| Library { name: it.name.clone(), order: *order, as_needed: it.as_needed })
        // alloc: the answer's libraries, once per prelink or layout
        .collect()
}

/// The pieces of the init or fini arrays (`class`), in address order.
fn init_entries(state: &State, symbols: &Symbols, class: Class) -> Vec<InitEntry> {
    let mut out: Vec<InitEntry> = state
        .live_pieces()
        .filter(|it| it.size >= 8 && state.spaces.get(it.region as usize).is_some_and(|space| space.region.class == class))
        .map(|piece| {
            let tie = match (piece.what(), symbols.key(piece.sym)) {
                (What::Static { .. }, SymKey::Static(item, instance)) => tie_of(symbols, item, instance),
                _ => [0; 64],
            };
            InitEntry { rank: piece.rank(), order: piece.order(), tie, at: piece.addr, count: (piece.size / 8) as u32 }
        })
        // alloc: the answer's pieces, once per prelink or layout
        .collect();
    out.sort_unstable_by_key(|it| it.at);
    out
}

/// The tie of a Rust static's piece: its item's id, then its instance's.
fn tie_of(symbols: &Symbols, item: IdNo, instance: IdNo) -> [u8; 64] {
    let mut tie = [0; 64];
    tie[..32].copy_from_slice(&symbols.content(item).0);
    tie[32..].copy_from_slice(&symbols.content(instance).0);
    tie
}

impl<'a> Stage<'a> {
    /// What the change's pieces refer to and do not define, found in the image beside: its
    /// functions' addresses and its statics' places, fixed for this image's life. Names are
    /// resolution's ([`Stage::resolve`]).
    pub(crate) fn beside_targets<O: Outside>(&mut self, outside: &O) -> Result<(), LinkError> {
        if !O::ANY {
            return Ok(());
        }
        let symbols: &Symbols = &*self.symbols;
        let rust = |sym: &Sym| matches!(symbols.key(*sym), SymKey::Body(_) | SymKey::Slot(_) | SymKey::Static(..));
        let mut wanted: FrameVec<'a, Sym> = Vec::new_in(self.arena);
        for id in self.fresh.iter() {
            let piece = self.new.piece(id).expect("a fresh piece is live");
            wanted.extend(piece.fixups().iter().map(|it| it.target).filter(rust));
        }
        wanted.extend(self.new.names.values().copied().filter(rust));
        wanted.sort_unstable_by_key(|it| it.0);
        wanted.dedup();
        for sym in wanted {
            if self.new.def(sym) != state::Def::None {
                continue;
            }
            let def = match self.symbols.key(sym) {
                SymKey::Body(id) | SymKey::Slot(id) => outside.function(&self.symbols.instance(id)).map(state::Def::Absolute),
                SymKey::Static(item, instance) => {
                    match outside.static_at(&self.symbols.item(item), self.symbols.content(instance)) {
                        Some(StaticAt::Addr(addr)) => Some(state::Def::Absolute(addr)),
                        Some(StaticAt::ThreadLocal(offset)) => {
                            if self.plan.tls_descriptors {
                                self.outside_tlv(item, instance, sym);
                            }
                            Some(state::Def::TpOff(offset as i64))
                        }
                        None => None,
                    }
                }
                _ => None,
            };
            if let Some(def) = def {
                self.define(sym, def);
            }
        }
        Ok(())
    }

    /// Mach-O: this image's descriptor of a thread-local of the image beside, which its code
    /// reaches the static through, as [`Stage::add_static`]'s of its own.
    fn outside_tlv(&mut self, item: IdNo, instance: IdNo, sym: Sym) {
        let tlv = self.sym(SymKey::Tlv(item, instance));
        if self.new.home(tlv).is_some() {
            return;
        }
        let bootstrap = self.sym_name("_tlv_bootstrap");
        let fixups: &'a [Fixup] = collect_in(
            [
                Fixup { at: 0, apply: Apply::Abs64, target: bootstrap, addend: 0, minus: None },
                Fixup { at: 16, apply: Apply::DtpOff64, target: sym, addend: 0, minus: None },
            ],
            self.arena,
        )
        .leak();
        let piece = self.piece(tlv, &Class::ThreadVars, 8, 24, fixups, What::Tlv);
        self.make(&piece, Content::Zero);
    }

    /// Fixes where the image's thread-locals are in the run's area once they are placed: at
    /// `tls_bias` plus `place.tls_from` rounded up to their alignment. An incremental stage
    /// cannot move them: every thread-local reference would change.
    pub(crate) fn fix_tls(&mut self, place: &Place) -> Result<(), LinkError> {
        let at = match self.new.tls {
            None => None,
            Some(tls) => {
                let from = checked_align_up_u64(place.tls_from, tls.align.max(1))
                    .and_then(|it| i64::try_from(it).ok())
                    .and_then(|it| place.tls_bias.checked_add(it));
                // alloc: error path
                Some(from.ok_or_else(|| error(None, format!("thread-locals past {:#x} in the run's area", place.tls_from)))?)
            }
        };
        if !self.full && self.old.laid_out && self.old.tls_at != at {
            return Err(refused(
                None,
                Refusal::TlsMoved,
                "the image's thread-locals moved in the run's area (a thread-local of a larger alignment, or the \
                 first): link it from scratch",
            ));
        }
        self.new.tls_at = at;
        Ok(())
    }
}
