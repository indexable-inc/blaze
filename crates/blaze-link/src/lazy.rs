//! Lazy images ([`blaze_lazy`]): a dev run whose functions are compiled on their first call,
//! and the placer that puts each one into the run's code memory.
//!
//! An image staged with [`Image::stage_lazy`] knows its session's lazy functions. Its first
//! link places the session's stub region ([`blaze_lazy::stub`]) as one piece of
//! [`crate::model::Class::Stubs`] after the slot stubs ([`SymKey::LazyStubs`]), which no later
//! link moves, so it is in the code memory's executable part. Each lazy function gets a
//! jump-table slot, in plan order before every other function's, whose cell holds its entry in
//! the region ([`SymKey::LazyEntry`]): what [`Session::slots`] gives for the region's address
//! ([`Image::lazy_stubs`]). Its body is an alias of its slot stub, so every reference to it,
//! [`Target::Direct`] as well as [`Target::Function`], a function pointer, a vtable entry or
//! a static's, is the slot stub: the address a whole live build gives it. A later
//! [`crate::Change`] never adds, removes or puts another function in a lazy function's slot:
//! [`LazyPlacer`] places it and its tier-ups.
//!
//! The region's hook cell ([`stub::CELL`]) is linked as a reference to `blaze_lazy_resolve`
//! ([`hook::SYMBOL`]; `_blaze_lazy_resolve` on Mach-O), its offset from the cell, which keeps
//! the hook resolved and its section linked in; an image that doesn't define it is an error
//! naming it. [`LazyPlacer::new`] stores the hook's address there before the run starts.
//!
//! # Addresses
//!
//! What the placer writes into the arena, and the slot targets and the cell, are words every
//! run process of the code memory shares. Code reaches the image PC-relative, which every slide
//! keeps, but an absolute word (the cell, `Abs64` data, a `Location`'s file name, the address
//! a slot is published at) holds one process's address. So a lazy run's processes share one
//! slide: [`LazyPlacer::new`] takes it (`blaze_inproc::host::Session::slide` of the first) and
//! every absolute word it writes is the linked address plus it. A process whose slide differs
//! must be refused before it runs ([`LazyPlacer::slide`]); the launcher starts lazy runs with
//! one slide (v2/requests/requests.md, "lazy runs: the image's runtime and one slide").
//!
//! # History
//!
//! What the placer publishes is outside the image's history: a commit or a rollback
//! ([`Image::commit_into`], [`Image::roll_back_into`]) never names a lazy slot. A lazy slot's
//! cell is its entry in every generation of the image (made at the first link and never put
//! again), so no patch message lists it, and blaze-live's switch (`slots/switch.rs`) stores
//! only the slots whose cell differs between the two generations' tables. A switch to a patch
//! and back keeps every placed slot's published target, in the loader's table and in each pool
//! table, where blaze-live's `slots::exec::publish` stored it ([`Slots`]). The counter pairs
//! the placer gives are carried into a commit staged before them ([`Image::commit`]).

#![forbid(unsafe_code)]

use std::sync::Arc;

use blaze_codegen::{Arch, CompiledFunction, InstanceKey, RelocKind, Relocs, Target};
use blaze_hash::HashMap;
use blaze_inproc::{CodeMemory, CodeRange};
use blaze_lazy::runner::Placer;
use blaze_lazy::stub::{self, Layout, Stubs};
use blaze_lazy::{Session, hook};
use blaze_mem::align::align_up_u64;
use parking_lot::Mutex;

use crate::model::{Apply, Sym};
use crate::position::{self, LOCATION_ALIGN, LOCATION_SIZE};
use crate::reloc::{self, Ctx, GotKind};
use crate::state::Def;
use crate::symbols::{IdNo, SymKey};
use crate::{Format, Image, LinkError, Output, Slot};

/// The stub region's alignment: its resolver starts a cache line from its start.
pub(crate) const REGION_ALIGN: u64 = 64;

/// What a lazy image knows of its session: its lazy functions in plan order ([`LazyId`] `n`
/// is `keys[n]`) and the stub region's bytes and layout. Once per image.
///
/// [`LazyId`]: blaze_lazy::LazyId
pub(crate) struct Lazy {
    pub keys: Box<[InstanceKey]>,
    pub bytes: Box<[u8]>,
    pub layout: Layout,
}

impl Lazy {
    /// The lazy image `session` and its region `stubs` describe, linked as `output` says
    /// (`table`: with a jump table). A lazy image is a dev run with a jump table.
    pub fn new(output: &Output, table: bool, session: &Session, stubs: &Stubs) -> Result<Lazy, LinkError> {
        let fail = |what: String| LinkError { function: None, what, refusal: None };
        if output.dev_run.is_none() {
            return Err(fail(
                "a lazy image without a dev run (`Output::dev_run`): lazy code is placed in a dev executable's code \
                 memory, which only a dev run has"
                    .into(),
            ));
        }
        if !table {
            return Err(fail(
                "a lazy image without a jump table: its lazy functions are reached through their slots, which only an \
                 incremental image has"
                    .into(),
            ));
        }
        let layout = *session.layout();
        let keys = session.plan().lazy();
        if stubs.layout != layout || stubs.bytes.len() != layout.len as usize || layout.count as usize != keys.len() {
            // alloc: error path
            return Err(fail(format!(
                "the stub region ({} bytes, {:?}) is not the session's ({:?}, {} lazy functions)",
                stubs.bytes.len(),
                stubs.layout,
                layout,
                keys.len()
            )));
        }
        if (layout.len as usize) < stub::CELL as usize + 8 {
            // alloc: error path
            return Err(fail(format!("a stub region of {} bytes, with no room for its hook cell", layout.len)));
        }
        Ok(Lazy {
            // alloc: once per image, its session's keys and region
            keys: keys.into(),
            bytes: stubs.bytes.as_slice().into(),
            layout,
        })
    }

    /// Whether this is what `session` and `stubs` describe.
    pub fn is(&self, session: &Session, stubs: &Stubs) -> bool {
        self.layout == *session.layout() && *self.keys == *session.plan().lazy() && *self.bytes == *stubs.bytes
    }
}

/// The image's hook as its format's symbol table names it, for errors.
pub(crate) fn hook_name(format: Format) -> &'static str {
    match format {
        Format::MachO => "_blaze_lazy_resolve",
        _ => hook::SYMBOL,
    }
}

/// The error of a lazy image that doesn't define its hook itself.
pub(crate) fn no_hook(format: Format) -> LinkError {
    LinkError {
        function: None,
        // alloc: error path
        what: format!(
            "a lazy image without `{}`, the hook its lazy stubs call: link blaze-lazy's `hook` into it",
            hook_name(format)
        ),
        refusal: None,
    }
}

/// The address of the name `name` in the image, when the image defines it.
fn name_addr(image: &Image, name: &str) -> Option<u64> {
    let sym = image.symbols.find(SymKey::Name(image.symbols.find_name(name)?))?;
    image.state.addr_of(sym)
}

/// Where a placement publishes a slot's code: every run process of the code memory stores
/// `target` in `slot` by blaze-live's `slots::exec::publish(slot, target)` in the run process,
/// which owns its tables: the loader's and each table of the switch's pool, each by one
/// release store, never the execution log's table, so an armed log's stub stays in place and
/// the function's first call is still logged (fastcompile-ident-select 9). A process that
/// starts later stores it before it runs. Returns once every running process has stored it:
/// then no thread loads the slot's old target any more, and the code it named can be retired.
pub trait Slots: Send + 'static {
    fn publish(&mut self, slot: Slot, target: u64) -> Result<(), String>;
}

/// The daemon's [`Placer`] for a lazy image committed into a code memory
/// ([`Image::commit_into`]): places each function in the memory's arena, resolved against the
/// image, and publishes it in its slot. The image and the memory are shared with whatever
/// stages and commits them; a placement locks the image, then the memory, and holds neither
/// while [`Slots::publish`] waits on the run processes.
pub struct LazyPlacer<S: Slots> {
    image: Arc<Mutex<Image>>,
    memory: Arc<Mutex<CodeMemory>>,
    slots: S,
    slide: u64,
    /// The arena code each slot runs, by the key the slot is named for: what a replacement
    /// retires.
    // HashMap: one entry per placed slot, probed once per placement.
    placed: HashMap<InstanceKey, CodeRange>,
    scratch: Scratch,
}

/// Buffers reused across placements: a placement allocates only when a function needs more
/// room than every one before it.
#[derive(Default)]
struct Scratch {
    /// The range's bytes: the code, each datum, then location records and file names, then
    /// veneers.
    bytes: Vec<u8>,
    len: u64,
    /// Where each datum starts in the range.
    data_at: Vec<u64>,
    /// Each table's targets resolved (the code's, then each datum's), and where each table's
    /// start in it.
    resolved: Vec<Resolved>,
    starts: Vec<usize>,
    /// The `Location` records the placement writes.
    records: Vec<Record>,
    /// AArch64: the veneer of each of the code's targets a `bl` reaches in the image, by
    /// target index; empty elsewhere.
    veneers: Vec<Veneer>,
}

/// Room in the range for a range-extension veneer, `adrp x16; add x16, x16, lo12; br x16`:
/// what a `bl` of placed code goes through to a target in the image past its 128 MiB reach
/// (an arena is after the image's patch regions, further than that from its code).
#[derive(Clone, Copy, Debug)]
struct Veneer {
    /// The offset in the range, or [`Veneer::NONE`] for a target no `bl` reaches.
    at: u64,
    /// Where it jumps, once a branch out of reach went through it.
    to: Option<u64>,
}

impl Veneer {
    const NONE: u64 = u64::MAX;
    /// `adrp x16, 0; add x16, x16, 0; br x16`.
    const CODE: [u32; 3] = [0x9000_0010, 0x9100_0210, 0xd61f_0200];
    const LEN: u64 = 12;
}

/// How far an AArch64 `b` or `bl` reaches either way.
const A64_BRANCH_REACH: i64 = 1 << 27;

/// A relocation target, resolved once per target of a table.
#[derive(Clone, Copy, Debug)]
enum Resolved {
    /// A symbol of the image.
    Sym(Sym),
    /// An offset of the range being placed: the code (0), a datum, a location record.
    Own(u64),
    /// A weak name nothing defines.
    Zero,
    /// The linked address of the function's counter pair.
    Counter(u64),
}

/// A span's `Location` record, written at offset `at` of the range: its file name is in the
/// image, or at an offset of the range.
#[derive(Clone, Copy, Debug)]
struct Record {
    at: u64,
    anchor: IdNo,
    index: u32,
    name: Name,
}

#[derive(Clone, Copy, Debug)]
enum Name {
    Image(u64),
    Own(u64),
}

/// A symbol's value in the image.
#[derive(Clone, Copy)]
enum Value {
    Image(u64),
    Absolute(u64),
    /// An offset in the image's thread-local block.
    Tls(u64),
    Import,
}

/// What a relocation writes: the target's address, its GOT cell, its thread-pointer offset
/// where the image fixes one, and whether the address slides with the image.
type Parts = (u64, u64, Option<i64>, bool);

impl<S: Slots> LazyPlacer<S> {
    /// The placer of the lazy image `image`, committed into `memory` and laid out over it
    /// ([`CodeMemory::lay_out`]), whose every run process has the image at `slide` from its
    /// linked addresses. Stores the hook's address in the region's cell, before any process
    /// runs.
    pub fn new(image: Arc<Mutex<Image>>, memory: Arc<Mutex<CodeMemory>>, slots: S, slide: u64) -> Result<LazyPlacer<S>, LinkError> {
        let fail = |what: String| LinkError { function: None, what, refusal: None };
        {
            let image = image.lock();
            let mut memory = memory.lock();
            let Some(region) = image.lazy_stubs() else {
                // alloc: error path
                return Err(fail(format!("{}: not a lazy image, or not linked yet (`Image::stage_lazy`)", image.to)));
            };
            if image.cells.is_none() {
                // alloc: error path
                return Err(fail(format!("{}: not committed into a code memory yet (`Image::commit_into`)", image.to)));
            }
            let Some(hook) = name_addr(&image, hook::SYMBOL) else {
                let name = hook_name(image.output.format);
                // alloc: error path
                return Err(fail(format!("{}: `{name}`, the hook its lazy stubs call, is not in the image", image.to)));
            };
            let cell = region + u64::from(stub::CELL);
            let Some(at) = memory.offset_of(cell) else {
                // alloc: error path
                return Err(fail(format!(
                    "{}: the stub region's cell at {cell:#x} is not in the code memory's code: lay the memory out over \
                     the image first (`CodeMemory::lay_out`)",
                    image.to
                )));
            };
            memory.store_word(at, hook.wrapping_add(slide)).map_err(|it| fail(it.what))?;
        }
        // alloc: once per run, the placer
        Ok(LazyPlacer { image, memory, slots, slide, placed: HashMap::default(), scratch: Scratch::default() })
    }

    /// The slide every run process of the memory has: one whose image is elsewhere must be
    /// refused before it runs.
    pub fn slide(&self) -> u64 {
        self.slide
    }

    /// Places `function` and publishes it in the slot named for `slot`: the address it runs
    /// at. The arena code the slot ran before is retired once every process stored the new.
    fn put(&mut self, slot: InstanceKey, function: &CompiledFunction) -> Result<u64, LinkError> {
        let key = function.key;
        let fail = |what: String| LinkError { function: Some(key), what, refusal: None };
        let LazyPlacer { image, memory, slots, slide, placed, scratch } = self;
        let slide = *slide;
        let (index, range, target) = {
            let mut image = image.lock();
            let image = &mut *image;
            let mut memory = memory.lock();
            if function.arch != image.output.arch {
                // alloc: error path
                return Err(fail(format!("compiled for {:?}, placed in an image for {:?}", function.arch, image.output.arch)));
            }
            if function.unwind.is_some() {
                return Err(fail(
                    "it has call frame information, which unwinding through placed code needs registered in each run \
                     process: the placer registers none"
                        .into(),
                ));
            }
            if function.profile.as_ref().is_some_and(|it| it.counted().is_some()) {
                return Err(fail("instrumented for PGO counters (`-Cprofile-generate`), which lazy code never is".into()));
            }
            let Some(index) = image.slot(&slot) else {
                // alloc: error path
                return Err(fail(format!("placed in the slot of {slot:?}, which the image has none of")));
            };
            let align = scratch.lay_out(image, function)?;
            let range = memory.reserve(scratch.len, align).map_err(|it| fail(it.what))?;
            match scratch.write(image, &mut memory, function, range, slide) {
                Ok(linked) => (index, range, linked.wrapping_add(slide)),
                Err(error) => {
                    memory.unreserve(range).map_err(|it| fail(it.what))?;
                    return Err(error);
                }
            }
        };
        if let Err(why) = slots.publish(index, target) {
            // Some process may have stored it: the range waits behind the fence.
            memory.lock().retire(range).map_err(|it| fail(it.what))?;
            // alloc: error path
            return Err(fail(format!("publishing it in slot {}: {why}", index.0)));
        }
        // alloc: once per slot placed, its entry
        if let Some(old) = placed.insert(slot, range) {
            memory.lock().retire(old).map_err(|it| fail(it.what))?;
        }
        Ok(target)
    }
}

impl<S: Slots> Placer for LazyPlacer<S> {
    fn place(&mut self, function: &CompiledFunction) -> Result<u64, String> {
        // alloc: error path, the runner's error type
        self.put(function.key, function).map_err(|it| it.to_string())
    }

    fn cells(&self, key: &InstanceKey) -> Option<u64> {
        // A poisoned lock still holds every pair given: a pair is set before the count moves.
        let image = self.image.lock();
        image.counter(key)
    }

    fn replace(&mut self, slot: InstanceKey, function: &CompiledFunction) -> Result<u64, String> {
        // alloc: error path, the runner's error type
        self.put(slot, function).map_err(|it| it.to_string())
    }
}

impl Scratch {
    /// Lays the range out (the code, each datum at its alignment, then a `Location` record and
    /// a file name for each position the image has none for, then on AArch64 a veneer for each
    /// target in the image a `bl` reaches) and resolves every target: the range's alignment,
    /// its length in [`Scratch::len`].
    fn lay_out(&mut self, image: &mut Image, function: &CompiledFunction) -> Result<u64, LinkError> {
        self.data_at.clear();
        self.resolved.clear();
        self.starts.clear();
        self.records.clear();
        let mut align = u64::from(function.align).max(1);
        let mut end = function.code.len() as u64;
        for data in &function.data {
            let data_align = u64::from(data.align).max(1);
            align = align.max(data_align);
            let at = align_up_u64(end, data_align);
            // alloc: reused across placements; grows only for a function with more data
            self.data_at.push(at);
            end = at + data.bytes.len() as u64;
        }
        let tables = std::iter::once((&function.relocs, true)).chain(function.data.iter().map(|it| (&it.relocs, false)));
        for (relocs, in_code) in tables {
            // alloc: reused across placements, as `data_at`
            self.starts.push(self.resolved.len());
            for target in relocs.targets() {
                let resolved = self.resolve(image, function, target, in_code, &mut end)?;
                // alloc: reused across placements; grows only for a function with more targets
                self.resolved.push(resolved);
            }
        }
        if !self.records.is_empty() {
            align = align.max(LOCATION_ALIGN);
        }
        self.veneers.clear();
        if image.output.arch == Arch::Aarch64 {
            let code = &self.resolved[..function.relocs.targets().len()];
            // alloc: reused across placements; grows only for a function with more targets
            self.veneers.resize(code.len(), Veneer { at: Veneer::NONE, to: None });
            for row in function.relocs.rows() {
                let veneer = &mut self.veneers[row.target as usize];
                let in_image = matches!(code[row.target as usize], Resolved::Sym(_));
                if row.kind == RelocKind::Call26 && in_image && veneer.at == Veneer::NONE {
                    veneer.at = align_up_u64(end, 4);
                    end = veneer.at + Veneer::LEN;
                    align = align.max(4);
                }
            }
        }
        if end == 0 {
            return Err(LinkError { function: Some(function.key), what: "no code to place".into(), refusal: None });
        }
        self.len = end;
        Ok(align)
    }

    fn resolve(
        &mut self,
        image: &mut Image,
        function: &CompiledFunction,
        target: &Target,
        in_code: bool,
        end: &mut u64,
    ) -> Result<Resolved, LinkError> {
        let fail = |what: String| LinkError { function: Some(function.key), what, refusal: None };
        if let Target::Counter(of) = target {
            return counter(image, function, of, in_code);
        }
        let (symbols, state) = (&image.symbols, &image.state);
        let defined = |sym: Option<Sym>| sym.filter(|it| !matches!(state.def(*it), Def::None));
        Ok(match target {
            Target::Function(key) => match defined(symbols.find_slot(key)) {
                Some(sym) => Resolved::Sym(sym),
                None if *key == function.key => Resolved::Own(0),
                // alloc: error path
                None => return Err(fail(format!("calls {key:?}, which has no slot in the image: neither linked nor lazy"))),
            },
            Target::Direct(key) if *key == function.key => Resolved::Own(0),
            Target::Direct(key) => match defined(symbols.find_body(key)) {
                Some(sym) => Resolved::Sym(sym),
                // alloc: error path
                None => return Err(fail(format!("calls {key:?} directly, which is neither linked nor lazy"))),
            },
            Target::Static { item, instance } => {
                let sym = symbols
                    .find_id(&item.0)
                    .zip(symbols.find_id(instance))
                    .and_then(|(item, instance)| symbols.find(SymKey::Static(item, instance)));
                match defined(sym) {
                    Some(sym) => Resolved::Sym(sym),
                    // alloc: error path
                    None => return Err(fail(format!("refers to the static {item:?} ({instance:?}), which the image has not"))),
                }
            }
            Target::Data(id) => match function.data.iter().position(|it| it.id == *id) {
                Some(at) => Resolved::Own(self.data_at[at]),
                None => match defined(symbols.find_id(id).and_then(|it| symbols.find(SymKey::Data(it)))) {
                    Some(sym) => Resolved::Sym(sym),
                    // alloc: error path
                    None => return Err(fail(format!("refers to the data {id:?}, neither its own nor the image's"))),
                },
            },
            Target::Extern(name) | Target::Weak(name) => {
                match defined(symbols.find_name(name).and_then(|it| symbols.find(SymKey::Name(it)))) {
                    Some(sym) => Resolved::Sym(sym),
                    None if matches!(target, Target::Weak(_)) => Resolved::Zero,
                    // alloc: error path
                    None => return Err(fail(format!("refers to `{name}`, which the image does not define"))),
                }
            }
            Target::Position(span) => {
                let anchor = symbols.find_id(&span.anchor);
                let table = anchor.and_then(|it| state.span_table(it));
                let (Some(anchor), Some(table)) = (anchor, table) else {
                    // alloc: error path
                    return Err(fail(format!("a position in {:?}, whose span table the image was not given", span.anchor)));
                };
                if let Some(sym) = symbols.find(SymKey::Position(anchor, span.index)).filter(|it| state.home(*it).is_some()) {
                    return Ok(Resolved::Sym(sym));
                }
                let Some(at) = table.span(span.index) else {
                    // alloc: error path
                    return Err(fail(format!("span {} of {:?}, whose table has {}", span.index, span.anchor, table.len())));
                };
                let file = table.file(at.file);
                let record = align_up_u64(*end, LOCATION_ALIGN);
                *end = record + LOCATION_SIZE;
                let in_image =
                    symbols.find_name(file).and_then(|it| symbols.find(SymKey::FileName(it))).and_then(|it| state.addr_of(it));
                let name = match in_image {
                    Some(addr) => Name::Image(addr),
                    None => {
                        let at = *end;
                        *end += file.len() as u64 + 1;
                        Name::Own(at)
                    }
                };
                // alloc: reused across placements; grows only for a function with more positions
                self.records.push(Record { at: record, anchor, index: span.index, name });
                Resolved::Own(record)
            }
            Target::Coverage(_) => {
                return Err(fail("SanitizerCoverage arrays in lazily placed code, which the image places none of".into()));
            }
            Target::ProfileCounters => {
                return Err(fail("instrumented for PGO counters (`-Cprofile-generate`), which lazy code never is".into()));
            }
            Target::Counter(_) => unreachable!("resolved above"),
        })
    }

    /// Writes the range at `range` of `memory` (`function`'s code and data, every relocation
    /// applied, the location records) and publishes it: its linked address. The processes have
    /// it at that plus `slide`.
    fn write(
        &mut self,
        image: &Image,
        memory: &mut CodeMemory,
        function: &CompiledFunction,
        range: CodeRange,
        slide: u64,
    ) -> Result<u64, LinkError> {
        let fail = |what: String| LinkError { function: Some(function.key), what, refusal: None };
        let Some(linked) = memory.linked(range.offset) else {
            // alloc: error path
            return Err(fail(format!("the arena range at {:#x} has no linked address: the code memory is not laid out", range.offset)));
        };
        self.bytes.clear();
        // alloc: reused across placements; grows only for a larger function
        self.bytes.resize(self.len as usize, 0);
        self.bytes[..function.code.len()].copy_from_slice(&function.code);
        for (data, at) in function.data.iter().zip(&self.data_at) {
            self.bytes[*at as usize..][..data.bytes.len()].copy_from_slice(&data.bytes);
        }
        for record in &self.records {
            let table = image.state.span_table(record.anchor).expect("resolved when laid out");
            let at = table.span(record.index).expect("resolved when laid out");
            let file = table.file(at.file);
            let mut bytes = position::location(file.len(), at);
            let name = match record.name {
                Name::Image(addr) => addr,
                Name::Own(offset) => {
                    // The NUL after it is the zero the buffer was filled with.
                    self.bytes[offset as usize..][..file.len()].copy_from_slice(file.as_bytes());
                    linked + offset
                }
            };
            bytes[..8].copy_from_slice(&name.wrapping_add(slide).to_le_bytes());
            self.bytes[record.at as usize..][..bytes.len()].copy_from_slice(&bytes);
        }
        {
            let resolved = &self.resolved[..function.relocs.targets().len()];
            let (code, rest) = self.bytes.split_at_mut(function.code.len());
            let mut veneers = Veneers { of: &mut self.veneers, bytes: rest, at: function.code.len() as u64 };
            relocate(image, &function.relocs, resolved, code, linked, 0, Some(&mut veneers), slide).map_err(fail)?;
        }
        for (n, (data, at)) in function.data.iter().zip(&self.data_at).enumerate() {
            let resolved = &self.resolved[self.starts[n + 1]..][..data.relocs.targets().len()];
            let field = &mut self.bytes[*at as usize..][..data.bytes.len()];
            relocate(image, &data.relocs, resolved, field, linked, *at, None, slide).map_err(fail)?;
        }
        memory.write(range.offset, &self.bytes).map_err(|it| fail(it.what))?;
        memory.publish(range).map_err(|it| fail(it.what))?;
        Ok(linked)
    }
}

/// `function`'s own counter pair ([`crate::counter::placed_pair`]), at its linked address.
fn counter(image: &mut Image, function: &CompiledFunction, of: &InstanceKey, in_code: bool) -> Result<Resolved, LinkError> {
    let fail = |what: String| LinkError { function: Some(function.key), what, refusal: None };
    if !in_code || *of != function.key {
        return Err(fail(
            // alloc: error path
            format!(
                "the tier-up counter cells of {of:?} named in {}: only a function's own code names its cells",
                if in_code { "another function's code" } else { "data" }
            ),
        ));
    }
    let Some(cells) = image.symbols.find(SymKey::Counters).and_then(|it| image.state.addr_of(it)) else {
        return Err(fail("its code counts in tier-up counter cells, and the image has no cells reservation".into()));
    };
    let id = image.symbols.id(&of.0);
    let had = image.state.pairs.of(id).is_some();
    let pair = crate::counter::placed_pair(&image.output, &mut image.state, id, of)?;
    if !had {
        // alloc: once per key the placer gives a pair, kept until the next commit
        image.placed_pairs.push((id, pair));
    }
    Ok(Resolved::Counter(cells + crate::counter::offset(pair)))
}

/// The code's veneers ([`Scratch::veneers`]) and the bytes of the range they are in, from its
/// offset `at`.
struct Veneers<'b> {
    of: &'b mut [Veneer],
    bytes: &'b mut [u8],
    at: u64,
}

impl Veneers<'_> {
    /// The offset in the range at `linked` of the veneer a `bl` by `ctx` to the code's target
    /// `target` goes through, written to jump to its target; `None` for one in reach.
    fn through(&mut self, target: u32, ctx: &Ctx, linked: u64) -> Result<Option<u64>, String> {
        let Some(veneer) = self.of.get_mut(target as usize).filter(|it| it.at != Veneer::NONE) else {
            return Ok(None);
        };
        let to = ctx.s.wrapping_add(ctx.a as u64);
        if (-A64_BRANCH_REACH..A64_BRANCH_REACH).contains(&(to.wrapping_sub(ctx.p) as i64)) {
            return Ok(None);
        }
        match veneer.to {
            Some(it) if it != to => {
                // alloc: error path
                return Err(format!("{to:#x} is past a `bl`'s reach, and the veneer of its target jumps to {it:#x}"));
            }
            Some(_) => {}
            None => {
                let bytes = &mut self.bytes[(veneer.at - self.at) as usize..][..Veneer::LEN as usize];
                for (word, insn) in bytes.chunks_exact_mut(4).zip(Veneer::CODE) {
                    word.copy_from_slice(&insn.to_le_bytes());
                }
                let addr = linked + veneer.at;
                let at = |p: u64| Ctx { s: to, a: 0, p, g: 0, tp: None, ..*ctx };
                // alloc: error path
                let fail = |it: reloc::Bad| format!("the veneer at {addr:#x} to {to:#x}: {it}");
                reloc::apply(Apply::A64Page21, &at(addr), &mut bytes[..4]).map_err(fail)?;
                reloc::apply(Apply::A64Lo12 { shift: 0 }, &at(addr + 4), &mut bytes[4..8]).map_err(fail)?;
                veneer.to = Some(to);
            }
        }
        Ok(Some(veneer.at))
    }
}

/// Applies `relocs` (`resolved` being its targets by index) to `field`, a table's bytes at
/// offset `start` of the range at `linked`: the code's when `veneers` is given, a datum's
/// otherwise.
#[allow(clippy::too_many_arguments)]
fn relocate(
    image: &Image,
    relocs: &Relocs,
    resolved: &[Resolved],
    field: &mut [u8],
    linked: u64,
    start: u64,
    mut veneers: Option<&mut Veneers<'_>>,
    slide: u64,
) -> Result<(), String> {
    let output = &image.output;
    let in_code = veneers.is_some();
    for (row, reloc) in relocs.rows().iter().zip(relocs.iter()) {
        // alloc: error path
        let at_fail = |what: String| format!("{:?} at {:#x} to {:?}: {what}", reloc.kind, reloc.at, reloc.target);
        let apply = reloc::of_kind(reloc.kind, output.format, output.arch, in_code).map_err(at_fail)?;
        let (s, g, tp, slid) = match resolved[row.target as usize] {
            Resolved::Own(offset) if reloc::got_kind(apply).is_none() => (linked + offset, 0, None, true),
            Resolved::Own(_) => return Err(at_fail("its own code or data reached through a GOT cell, which only the image's symbols have".into())),
            Resolved::Zero if reloc::got_kind(apply).is_none() => (0, 0, None, false),
            Resolved::Zero => return Err(at_fail("a weak name nothing defines, reached through a GOT cell, which only the image's symbols have".into())),
            Resolved::Counter(_) if !crate::counter::reaches(apply) => {
                return Err(at_fail(
                    "tier-up counter cells are reached rip-relative (x86-64) or by ADRP and a page offset (AArch64)".into(),
                ));
            }
            Resolved::Counter(addr) => (addr, 0, None, true),
            Resolved::Sym(sym) => sym_value(image, sym, apply).map_err(at_fail)?,
        };
        let s = if apply == Apply::Abs64 && slid { s.wrapping_add(slide) } else { s };
        let mut ctx = Ctx {
            arch: output.arch,
            format: output.format,
            s,
            a: reloc.addend,
            p: linked + start + u64::from(reloc.at),
            g,
            m: 0,
            base: image.plan.base,
            tls: image.state.tls,
            tp,
        };
        if let Some(veneers) = veneers.as_deref_mut().filter(|_| apply == Apply::A64Call26) {
            if let Some(at) = veneers.through(row.target, &ctx, linked).map_err(at_fail)? {
                (ctx.s, ctx.a) = (linked + at, 0);
            }
        }
        let Some(place) = field.get_mut(reloc.at as usize..) else {
            return Err(at_fail("past the end of its code or data".into()));
        };
        // alloc: error path
        reloc::apply(apply, &ctx, place).map_err(|it| at_fail(it.to_string()))?;
    }
    Ok(())
}

/// What a relocation of kind `apply` to the image's `sym` writes: its PLT stub's address for
/// a branch to an import, its GOT cell for a kind that goes through one.
fn sym_value(image: &Image, sym: Sym, apply: Apply) -> Result<Parts, String> {
    let (symbols, state) = (&image.symbols, &image.state);
    let got = reloc::got_kind(apply);
    let g = match got {
        Some(kind) => got_cell(image, sym, apply, kind)?,
        None => 0,
    };
    let tls_base = state.tls.map_or(0, |it| it.addr);
    let tp = |s: u64| -> Result<Option<i64>, String> {
        match state.tls_at {
            Some(at) if reloc::is_tls(apply) => match s.checked_sub(tls_base).filter(|_| state.tls.is_some()) {
                Some(offset) => Ok(Some(at + offset as i64)),
                None => Err("a thread-local relocation to what is not thread-local".into()),
            },
            _ => Ok(None),
        }
    };
    match def_value(image, sym)? {
        Value::Image(s) => Ok((s, g, tp(s)?, true)),
        Value::Tls(offset) => {
            let s = tls_base + offset;
            Ok((s, g, tp(s)?, false))
        }
        Value::Absolute(s) => Ok((s, g, None, false)),
        Value::Import if reloc::is_branch(apply) => match symbols.find(SymKey::Plt(sym)).and_then(|it| state.addr_of(it)) {
            Some(stub) => Ok((stub, g, None, true)),
            // alloc: error path
            None => Err(format!("{} is imported, and the image has no PLT stub of it", symbols.describe(sym))),
        },
        Value::Import if got.is_some() => Ok((0, g, None, false)),
        // alloc: error path
        Value::Import => Err(format!(
            "{} is imported: placed code reaches an import only by a call (its PLT stub) or through its GOT cell",
            symbols.describe(sym)
        )),
    }
}

/// The image's GOT cell of `kind` for `sym` (a Mach-O thread-local's is its TLV descriptor's).
fn got_cell(image: &Image, sym: Sym, apply: Apply, kind: GotKind) -> Result<u64, String> {
    let (symbols, state) = (&image.symbols, &image.state);
    let target = match (apply, symbols.key(sym)) {
        (Apply::TlvPage21 | Apply::TlvLo12 | Apply::TlvRel32, SymKey::Static(item, instance)) => {
            symbols.find(SymKey::Tlv(item, instance)).unwrap_or(sym)
        }
        _ => sym,
    };
    match symbols.find(SymKey::Got(kind, target)).and_then(|it| state.addr_of(it)) {
        Some(cell) => Ok(cell),
        // alloc: error path
        None => Err(format!("no GOT cell of {} in the image", symbols.describe(target))),
    }
}

/// `sym`'s value in the image, through its aliases.
fn def_value(image: &Image, mut sym: Sym) -> Result<Value, String> {
    let (symbols, state) = (&image.symbols, &image.state);
    for _ in 0..64 {
        return Ok(match state.def(sym) {
            Def::Piece { piece, offset } => match state.piece(piece) {
                Some(it) => Value::Image(it.addr + offset),
                // alloc: error path
                None => return Err(format!("{} was removed from the image", symbols.describe(sym))),
            },
            Def::Import(_) => Value::Import,
            Def::Absolute(value) => Value::Absolute(value),
            Def::TlsOffset(value) => Value::Tls(value),
            Def::Alias(to) => {
                sym = to;
                continue;
            }
            // alloc: error path
            Def::TpOff(_) => return Err(format!("{} is a thread-local of another image", symbols.describe(sym))),
            // alloc: error path
            Def::Special(_) => return Err(format!("{} is made by the linker, which placed code does not reach", symbols.describe(sym))),
            // alloc: error path
            Def::None => return Err(format!("{} is not defined in the image", symbols.describe(sym))),
        });
    }
    // alloc: error path
    Err(format!("{} is an alias of an alias 64 deep", symbols.describe(sym)))
}
