//! Placing a program's code behind the launcher's slot table in this process, and calling its
//! entry (docs/v2/plan/design/run.md: one slot word a function; a call loads the word and
//! jumps). The table, the code area and the commit are blaze-launch-rt's [`Driver`]; this
//! module is the [`Records`] it asks for each body's bytes, which fills each body's literal
//! words ([`blaze_emit::Fixup`]): a program function's slot word address, or an outside
//! symbol's address by `dlsym`.
//!
//! Shortcuts (crates/blaze-cli/tests/ladder/README.md): the program runs on the `blaze`
//! process's own thread, not in a child launched by blaze-run's `Parent`; every function is
//! compiled and bound before the entry runs (no lazy stubs); the records are this module's
//! table, not the store's code records.

use std::alloc::Global;
use std::ffi::CString;
use std::sync::Arc;

use blaze_build::InstanceKey;
use blaze_build::data::{DataError, StaticImage};
use blaze_emit::{Code, Fixup, Reloc, RelocTo, Routine};
use blaze_typeck::ItemKey;
use blaze_launch::wire::{self, List, RecordId};
use blaze_launch::{CodeId, Slot, Tier, Txn};
use blaze_launch_rt::{Config, Driver, Place, Records, Shared};

/// What a body's literal word `place` is filled with.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Target {
    /// A program function: the address of its slot's word.
    Slot(u32),
    /// An outside symbol, by name: its address in this process.
    Symbol(Box<str, Global>),
    /// A stub's first word (crate::stub): the address of the placed program's `Stops`.
    Stops,
    /// A stub's second word: the address of `stub::stopped`.
    Stop,
}

/// Where one `Static` reference place of a body is.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum StaticAt {
    /// A Rust static: found in the placed static image, by key.
    Image(ItemKey),
    /// A static an `extern` block declares (the seam's `foreign_static`): the address of the
    /// outside symbol, by name, in this process, as an outside function's is.
    Symbol(Box<str, Global>),
}

/// One function to place: its code and, by literal place, what each word is filled with.
pub struct Body {
    pub code: Code<Global>,
    pub targets: Vec<Target, Global>,
    /// By `Static` reference place: the static ([`StaticAt`]).
    pub statics: Vec<StaticAt, Global>,
    /// By `Tls` reference place: the thread-local's index in the run's thread-local table
    /// (crate::tls_index).
    pub tls: Vec<u32, Global>,
    /// By `Vtable` reference place: the program's table ([`Vtables`]) the word is the address of.
    pub vtables: Vec<u32, Global>,
    /// Per row of `code.positions`, in order: the line or the column written there, resolved
    /// from the front end when the body was readied ([`resolve_positions`]). The code's bytes
    /// hold zero there, so its record id does not move with a line (rule 71).
    pub positions: Vec<u32, Global>,
}

/// Why a body's source position could not be placed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PositionError {
    /// The front end gave no line and column for byte `site` of item `item`.
    Unplaced { item: u128, site: u32, gap: crate::seam::PositionGap },
    /// A body's resolved values do not match its code's position rows one for one.
    Count { slot: u32, rows: usize, values: usize },
    /// A position field past the code's end.
    Past { slot: u32, at: u32 },
}

impl core::fmt::Display for PositionError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            PositionError::Unplaced { item, site, gap } => {
                write!(f, "the `#[track_caller]` location at byte {site} of item {item:#x}: {gap}")
            }
            PositionError::Count { slot, rows, values } => {
                write!(f, "slot {slot}: {rows} source position fields and {values} resolved values (place.rs `resolve_positions`)")
            }
            PositionError::Past { slot, at } => write!(f, "slot {slot}: a source position field at {at} is past the code's end"),
        }
    }
}

impl core::error::Error for PositionError {}

/// The value of each of `code`'s source position fields, in order, from `position` (the front
/// end's `source_position`): the line or the column it gives.
pub fn resolve_positions(
    code: &Code<Global>,
    position: &dyn Fn(u128, u32) -> Result<(u32, u32), crate::seam::PositionGap>,
) -> Result<Vec<u32, Global>, PositionError> {
    let mut out = Vec::with_capacity_in(code.positions.len(), Global);
    for p in code.positions.iter() {
        let (line, col) = position(p.item, p.site).map_err(|gap| PositionError::Unplaced { item: p.item, site: p.site, gap })?;
        out.push(match p.field {
            blaze_lir::PositionField::Line => line,
            blaze_lir::PositionField::Col => col,
        });
    }
    Ok(out)
}

/// What one word of a vtable becomes: a slot's entry thunk (a method, the drop glue) or another
/// table's address (a supertrait's off the first chain).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VtableWord {
    Slot(u32),
    Table(u32),
}

/// One vtable of the program: its bytes as the code builder laid them out
/// (`blaze_build::vtable::Vtable::build`: drop glue, size, align, then the methods in the object
/// trait's order) and, by byte offset, what each address word is filled with.
pub struct VtableImage {
    pub bytes: Vec<u8, Global>,
    pub words: Vec<(u32, VtableWord), Global>,
}

/// The program's vtables written into memory of their own (never freed: they live as long as
/// the placed code does). Words naming another table are filled when written; words naming a
/// slot once the slots are declared, on the first code that takes a table's address.
struct Vtables<'b> {
    images: &'b [VtableImage],
    at: Vec<u64, Global>,
    slots_filled: bool,
}

impl Vtables<'_> {
    fn write(images: &[VtableImage]) -> Result<Vtables<'_>, PlaceError> {
        use core::alloc::{Allocator, Layout};
        let mut at = Vec::with_capacity_in(images.len(), Global);
        for image in images {
            let bytes = image.bytes.len().max(8);
            let layout = Layout::from_size_align(bytes, 8).map_err(|_| PlaceError::NoMemory { bytes })?;
            let memory = Global.allocate_zeroed(layout).map_err(|_| PlaceError::NoMemory { bytes })?;
            let base = memory.as_ptr().cast::<u8>();
            // SAFETY: `memory` is a fresh allocation of at least `image.bytes.len()` bytes, owned
            // here and never freed; nothing else refers to it yet.
            unsafe { core::ptr::copy_nonoverlapping(image.bytes.as_ptr(), base, image.bytes.len()) };
            at.push(base as u64);
        }
        for (n, image) in images.iter().enumerate() {
            for &(offset, word) in image.words.iter() {
                if let VtableWord::Table(t) = word {
                    let Some(&target) = at.get(t as usize) else {
                        return Err(PlaceError::NoVtable { table: t });
                    };
                    Self::store(&at, images, n, offset, target)?;
                }
            }
        }
        Ok(Vtables { images, at, slots_filled: false })
    }

    /// Writes `value` into table `n`'s word at byte `offset`.
    fn store(at: &[u64], images: &[VtableImage], n: usize, offset: u32, value: u64) -> Result<(), PlaceError> {
        let len = images[n].bytes.len();
        if offset as usize % 8 != 0 || offset as usize + 8 > len {
            return Err(PlaceError::VtableWord { table: n as u32, offset });
        }
        // SAFETY: table `n` was allocated by `write` with `len` bytes, 8-aligned, and is never
        // freed; `offset` is an 8-aligned word inside it (checked above). The tables are written
        // only here, before any placed code runs.
        unsafe { ((at[n] + u64::from(offset)) as *mut u64).write(value) };
        Ok(())
    }

    /// Table `t`'s address, its slot words filled first (all tables at once, the first time).
    fn address(&mut self, t: u32, place: Place) -> Result<u64, String> {
        if !self.slots_filled {
            for (n, image) in self.images.iter().enumerate() {
                for &(offset, word) in image.words.iter() {
                    if let VtableWord::Slot(s) = word {
                        let Some(thunk) = place.thunk(Slot(s)) else {
                            return Err(format!("vtable {n}: the word at {offset} names slot {s}, which is not declared"));
                        };
                        Self::store(&self.at, self.images, n, offset, thunk).map_err(|e| e.to_string())?;
                    }
                }
            }
            self.slots_filled = true;
        }
        self.at.get(t as usize).copied().ok_or_else(|| format!("vtable {t} is not one of the program's {}", self.at.len()))
    }
}

/// What every body's thread-local words name: the program's one descriptor static and the
/// slot of its getter (crates/blaze-build/src/tls.rs). `None` for a program with no
/// thread-locals.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Tls {
    pub desc: u64,
    pub getter: u32,
}

/// Why the program could not be placed or entered.
#[derive(Debug)]
pub enum PlaceError {
    /// The launcher refused the table or the edit.
    Launcher(blaze_launch_rt::Error),
    Edit(blaze_launch_rt::EditError),
    /// An outside symbol the program calls is not in this process.
    NoSymbol { name: Box<str, Global> },
    /// A routine of the C library or the compiler runtime the code calls is not loaded in this
    /// process: the routine and the library that exports it ([`routine_library`]).
    NoRoutine { routine: Routine, library: &'static str },
    /// A literal word names a place the body's target table does not have.
    NoTarget { slot: u32, place: u32 },
    /// The entry's slot holds no code after the commit.
    NoEntry { slot: u32 },
    /// The static image could not be written: a fn pointer in a static (no slot address is
    /// known before the code is placed), or a malformed image.
    Statics(DataError),
    /// No memory for the static image.
    NoMemory { bytes: usize },
    /// `pthread_key_create` refused the program's thread-local key: its error number.
    TlsKey { errno: i32 },
    /// The thread-locals' ender (the key's destructor) has no entry thunk: its slot is not
    /// declared, or its address is not a function's (crate::tls_index::create_key).
    NoEnder,
    /// The descriptor's `root` offset uses bit 63, which the access sequence clears to read it
    /// (THREADS.md D2: bit 63 is the Linux flag).
    TlsRoot { offset: u64 },
    /// The program has thread-locals and its static image holds no descriptor for them.
    NoDescriptor,
    /// A vtable's word names a table the program does not have.
    NoVtable { table: u32 },
    /// A vtable's address word is not an aligned word inside it.
    VtableWord { table: u32, offset: u32 },
    /// The placed code is AArch64 and this process is not: its entry is never called.
    NotThisMachine,
    /// The entry is a Rust program's C entry shim (`main(argc, argv)`), which runs only in the
    /// program's own process: the in-process call passes one register, the shim reads two.
    ShimInProcess,
    /// The run could not be watched for traps, or a trap could not be placed (crate::trap).
    Trap(crate::trap::TrapError),
    /// A `#[track_caller]` location's line or column could not be placed.
    Position(PositionError),
}

impl core::fmt::Display for PlaceError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            PlaceError::Launcher(error) => write!(f, "the launcher: {error}"),
            PlaceError::Edit(error) => write!(f, "the launcher's edit: {error}"),
            PlaceError::NoSymbol { name } => {
                write!(f, "the outside symbol `{name}` is not in this process (dlsym found nothing)")
            }
            PlaceError::NoRoutine { routine, library } => write!(
                f,
                "the routine `{}` is not in this process: it comes from {library}, which the blaze process has not loaded (dlsym found nothing)",
                routine.symbol()
            ),
            PlaceError::NoTarget { slot, place } => {
                write!(f, "slot {slot}: a literal word names reference {place}, which the body's table lacks")
            }
            PlaceError::NoEntry { slot } => write!(f, "the entry's slot {slot} holds no code after the commit"),
            PlaceError::Statics(DataError::NoFn { key }) => {
                write!(f, "a static points to fn instance {:?}, which the build gave no slot (run.rs `static_fn_slots`)", key.0)
            }
            PlaceError::Statics(error) => write!(f, "the static image: {error:?}"),
            PlaceError::NoMemory { bytes } => write!(f, "no memory for the {bytes}-byte static image"),
            PlaceError::TlsKey { errno } => write!(f, "`pthread_key_create` for the thread-locals' key failed: errno {errno}"),
            PlaceError::NoEnder => write!(f, "the thread-locals' ender has no entry thunk: its slot is not declared (the key would have no destructor)"),
            PlaceError::TlsRoot { offset } => {
                write!(f, "the thread-locals' root offset {offset:#x} uses bit 63, the flag the access sequence clears")
            }
            PlaceError::NoDescriptor => write!(f, "the program has thread-locals and its static image holds no descriptor"),
            PlaceError::NoVtable { table } => write!(f, "a vtable's word names table {table}, which the program does not have"),
            PlaceError::VtableWord { table, offset } => write!(f, "vtable {table}: the address word at byte {offset} is not an aligned word inside it"),
            PlaceError::NotThisMachine => write!(f, "the program's code is AArch64 and this machine is not: the skeleton runs a program only where it was emitted for"),
            PlaceError::ShimInProcess => write!(f, "the C entry shim `main(argc, argv)` reads two registers and the in-process call sets one: a Rust-edition program runs only in its own process (run.md piece 6)"),
            PlaceError::Trap(error) => write!(f, "the trap watch: {error}"),
            PlaceError::Position(error) => write!(f, "{error}"),
        }
    }
}

impl core::error::Error for PlaceError {}

/// A placed program: the table and code stay mapped for the life of the process.
pub struct Placed {
    _driver: Driver,
    shared: Arc<Shared>,
    /// Where a called stub leaves the program (crate::stub); the stubs' code holds its
    /// address, so it is boxed and lives as long as the code does.
    stops: Box<crate::stub::Stops>,
    /// The code area's addresses: a trap with its pc here is the program's (crate::trap).
    code: core::ops::Range<usize>,
}

/// How a placed program's entry ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ended {
    /// It returned: the process status.
    Status(u8),
    /// It called stub `n`, a function that is not built, and was stopped there.
    Stub(u32),
    /// It trapped (a `brk`) at `offset` in the body of `slot`, called from `callers` (each at
    /// its call; read from the frame-pointer chain), and was stopped there (crate::trap).
    Trap { code: crate::trap::TrapCode, slot: u32, offset: u64, callers: crate::trap::Callers },
}

/// The records of one placement: each slot's body, by slot.
struct Table<'b> {
    bodies: &'b [Body],
    /// The program's statics and the address they were written at.
    image: Option<(&'b StaticImage<Global>, u64)>,
    tls: Option<Tls>,
    vtables: Vtables<'b>,
    /// The first outside symbol that was not found, said after the edit fails.
    missing: Option<Box<str, Global>>,
    /// The first routine that was not found, said after the edit fails.
    missing_routine: Option<Routine>,
    /// The address of the placed program's `Stops`: every stub's first word.
    stops: u64,
}

/// The library that exports `routine` on this host, named in [`PlaceError::NoRoutine`].
/// macOS: libSystem re-exports all of them (checked with `dyld_info`). Linux: `memcpy` and
/// kin are libc's, `fmod`/`fmodf` libm's, the `__*ti*` family libgcc_s's: the latter two are
/// only found by `RTLD_DEFAULT` if the blaze process itself links them.
fn routine_library(routine: Routine) -> &'static str {
    if cfg!(target_os = "macos") {
        return "libSystem.B.dylib";
    }
    match routine {
        Routine::Memcpy | Routine::Memmove | Routine::Memset | Routine::Memcmp => "libc.so.6",
        Routine::Fmod | Routine::Fmodf => "libm.so.6",
        Routine::Udivti3
        | Routine::Umodti3
        | Routine::Divti3
        | Routine::Modti3
        | Routine::Floattidf
        | Routine::Floatuntidf
        | Routine::Floattisf
        | Routine::Floatuntisf
        | Routine::Fixdfti
        | Routine::Fixunsdfti
        | Routine::Fixsfti
        | Routine::Fixunssfti
        | Routine::UnwindResume => "libgcc_s.so.1",
    }
}

/// The address of `name` in this process.
fn symbol(name: &str) -> Option<u64> {
    let name = CString::new(name).ok()?;
    // SAFETY: `dlsym` reads a live NUL-terminated name and returns an address or null; the
    // handle `RTLD_DEFAULT` searches every image loaded, as a static link of the program
    // against the system's libraries would bind it.
    #[cfg(target_os = "macos")]
    let at = unsafe { blaze_sys::posix::thread::dlsym(blaze_sys::posix::thread::RTLD_DEFAULT, name.as_ptr()) };
    #[cfg(not(target_os = "macos"))]
    let at = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
    (!at.is_null()).then_some(at as u64)
}

impl Records for Table<'_> {
    fn record(&mut self, _id: RecordId, _bytes: &[u8]) -> Result<(), String> {
        Err("the skeleton's table is given no records over the wire".to_owned())
    }

    fn statics(&mut self, _defs: &[wire::StaticDef<'_>], _place: Place) -> Result<(), String> {
        Err("statics are not placed by the skeleton yet".to_owned())
    }

    fn code(&mut self, bind: &wire::Bind<'_>, place: Place, into: &mut Vec<u8>) -> Result<(), String> {
        into.clear();
        let slot = bind.slot.0;
        let Some(body) = self.bodies.get(slot as usize) else {
            return Err(format!("slot {slot} has no body in the skeleton's table"));
        };
        into.extend_from_slice(&body.code.bytes);
        for &Fixup { at, place: p } in body.code.fixups.iter() {
            let value = match body.targets.get(p as usize) {
                Some(Target::Slot(callee)) => match place.word(Slot(*callee)) {
                    Some(word) => word,
                    None => return Err(format!("slot {slot}: callee slot {callee} is not declared")),
                },
                Some(Target::Symbol(name)) => match symbol(name) {
                    Some(at) => at,
                    None => {
                        self.missing = Some(name.clone());
                        return Err(format!("the outside symbol `{name}` is not in this process"));
                    }
                },
                Some(Target::Stops) => self.stops,
                Some(Target::Stop) => crate::stub::stopped as usize as u64,
                None => return Err(format!("slot {slot}: reference {p} is not in the body's table")),
            };
            let at = at as usize;
            match into.get_mut(at..at + 8) {
                Some(word) => word.copy_from_slice(&value.to_le_bytes()),
                None => return Err(format!("slot {slot}: a literal word at {at} is past the code's end")),
            }
        }
        for &Reloc { at, to, addend } in body.code.relocs.iter() {
            let base = match to {
                RelocTo::Code(offset) => place.at + u64::from(offset),
                // A function's address as a value: its slot's entry thunk, which stays the same
                // however often the slot word is patched; an outside function's own address.
                RelocTo::SlotStub(p) | RelocTo::Func(p) => match body.targets.get(p as usize) {
                    Some(Target::Slot(callee)) => match place.thunk(Slot(*callee)) {
                        Some(thunk) => thunk,
                        None => return Err(format!("slot {slot}: slot {callee} is not declared")),
                    },
                    Some(Target::Symbol(name)) => match symbol(name) {
                        Some(at) => at,
                        None => {
                            self.missing = Some(name.clone());
                            return Err(format!("the outside symbol `{name}` is not in this process"));
                        }
                    },
                    // A stub's words are literal words (fixups): no code takes their address.
                    Some(Target::Stops | Target::Stop) => return Err(format!("slot {slot}: reference {p} is a stub's word, which no relocation names")),
                    None => return Err(format!("slot {slot}: reference {p} is not in the body's table")),
                },
                RelocTo::Routine(routine) => {
                    // The emitter's one table of routine names (`Routine::symbol`).
                    let name = routine.symbol();
                    match symbol(name) {
                        Some(at) => at,
                        None => {
                            self.missing_routine = Some(routine);
                            let library = routine_library(routine);
                            return Err(format!("the system routine `{name}` ({library}) is not in this process"));
                        }
                    }
                }
                RelocTo::Static(p) => match body.statics.get(p as usize) {
                    Some(StaticAt::Image(key)) => {
                        let found = self.image.and_then(|(image, base)| Some(base + u64::from(image.static_at(*key)?)));
                        match found {
                            Some(address) => address,
                            None => return Err(format!("slot {slot}: static reference {p} is in no placed static image")),
                        }
                    }
                    Some(StaticAt::Symbol(name)) => match symbol(name) {
                        Some(at) => at,
                        None => {
                            self.missing = Some(name.clone());
                            return Err(format!("the outside static `{name}` is not in this process"));
                        }
                    },
                    None => return Err(format!("slot {slot}: static reference {p} is not in the body's table")),
                },
                // The code builder's `Built::vtables` names the (type, principal) table at each
                // place; run.rs made each program table once (`Body::vtables`).
                RelocTo::Vtable(p) => match body.vtables.get(p as usize) {
                    Some(&t) => self.vtables.address(t, place)?,
                    None => return Err(format!("slot {slot}: vtable reference {p} is not in the body's table")),
                },
                RelocTo::TlsIndex(p) => match body.tls.get(p as usize) {
                    Some(&index) => u64::from(index),
                    None => return Err(format!("slot {slot}: thread-local reference {p} is not in the body's table")),
                },
                RelocTo::TlsDesc | RelocTo::TlsGetter => {
                    let Some(tls) = self.tls else {
                        return Err(format!("slot {slot}: a word at {at} names {to:?} in a program with no thread-locals"));
                    };
                    match to {
                        RelocTo::TlsDesc => tls.desc,
                        _ => match place.thunk(Slot(tls.getter)) {
                            Some(thunk) => thunk,
                            None => return Err(format!("slot {slot}: the getter's slot {} is not declared", tls.getter)),
                        },
                    }
                }
            };
            let value = base.wrapping_add_signed(addend);
            let at = at as usize;
            match into.get_mut(at..at + 8) {
                Some(word) => word.copy_from_slice(&value.to_le_bytes()),
                None => return Err(format!("slot {slot}: an address word at {at} is past the code's end")),
            }
        }
        if body.positions.len() != body.code.positions.len() {
            return Err(PositionError::Count { slot, rows: body.code.positions.len(), values: body.positions.len() }.to_string());
        }
        for (p, &value) in body.code.positions.iter().zip(body.positions.iter()) {
            let at = p.at as usize;
            match into.get_mut(at..at + 4) {
                Some(field) => field.copy_from_slice(&value.to_le_bytes()),
                None => return Err(PositionError::Past { slot, at: p.at }.to_string()),
            }
        }
        Ok(())
    }

    fn placed(&mut self, _at: u64, _len: u64) -> Result<(), String> {
        Ok(())
    }

    fn forget(&mut self, _at: u64) {}

    fn release(&mut self) {}

    fn static_at(&self, _number: u32) -> Option<usize> {
        None
    }
}

/// Memory of its own for `image`, zeroed, and its address: [`place`] writes the image into it
/// once the slots are declared, when every fn a static points at has an address (its slot's
/// entry thunk). The memory is never freed: the statics live as long as the placed code does,
/// the whole process. The code's address words name it before it is written, which is sound:
/// nothing runs before the placement's commit.
pub fn load_statics(image: &StaticImage<Global>) -> Result<u64, PlaceError> {
    use core::alloc::{Allocator, Layout};
    let bytes = image.bytes.len().max(1);
    let layout = Layout::from_size_align(bytes, 1usize << image.align).map_err(|_| PlaceError::NoMemory { bytes })?;
    let memory = Global.allocate_zeroed(layout).map_err(|_| PlaceError::NoMemory { bytes })?;
    Ok(memory.as_ptr().cast::<u8>() as u64)
}

/// Writes `image` at `base` (from [`load_statics`]), each fn word the entry thunk of the slot
/// `fns` gives its instance (`fns` sorted by key), from `place`'s declared slots.
fn write_statics(image: &StaticImage<Global>, base: u64, fns: &[(InstanceKey, u32)], place: Place) -> Result<(), PlaceError> {
    // SAFETY: `base` is `load_statics`' allocation of at least `image.bytes.len()` bytes for
    // this image, never freed, and no Rust reference to it is alive (the placed code has not
    // run: the commit comes after this); `fill` writes exactly `image.bytes.len()` of them.
    let out = unsafe { core::slice::from_raw_parts_mut(base as *mut u8, image.bytes.len()) };
    let fn_at = |key: InstanceKey| {
        let at = fns.binary_search_by_key(&key, |f| f.0).ok()?;
        place.thunk(Slot(fns[at].1))
    };
    image.fill(base, out, fn_at).map_err(PlaceError::Statics)
}


/// The record id `code` is bound under: its bytes' content id, so a body whose bytes did not
/// change keeps its record (its source position fields are zero in the bytes).
pub fn record_id(code: &Code<Global>) -> RecordId {
    RecordId(blaze_ids::ContentId::of("blaze-ladder code 1", &code.bytes).0)
}

/// Places `bodies`, body `n` in slot `n`, in one edit. `image` is the program's statics and the
/// memory reserved for them ([`load_statics`]), written here once the slots are declared, each
/// fn pointer in it the thunk of the slot `static_fns` gives its instance (sorted by key);
/// `tls` is what thread-local words name. `said` is each stub's refusal in words, by stub
/// number (crate::stub; empty for a program with none).
pub fn place(
    bodies: &[Body],
    vtables: &[VtableImage],
    image: Option<(&StaticImage<Global>, u64)>,
    static_fns: &[(InstanceKey, u32)],
    tls: Option<Tls>,
    said: Box<[Box<str>]>,
) -> Result<Placed, PlaceError> {
    let stops = Box::new(crate::stub::Stops::new(said));
    let vtables = Vtables::write(vtables)?;
    let page = blaze_launch_rt::sys::page().map_err(|e| PlaceError::Launcher(e.into()))? as u64;
    // Each body on its own alignment (blaze-emit `Code::align`, log2: 16 bytes unless a datum of
    // it asks more).
    let code_len: u64 = bodies.iter().map(|b| (b.code.bytes.len() as u64).next_multiple_of(1u64 << b.code.align)).sum();
    let area = (code_len + page).next_multiple_of(page).max(16 * page);
    let slots = u32::try_from(bodies.len()).unwrap_or(u32::MAX).max(1);
    let mut driver = Driver::new(Config { slots, area }).map_err(PlaceError::Launcher)?;
    if !bodies.is_empty() {
        driver.declare(slots).map_err(PlaceError::Launcher)?;
    }
    // The statics now: every slot has a word and a thunk, so a fn pointer has its address.
    if let Some((image, base)) = image {
        write_statics(image, base, static_fns, driver.place())?;
    }
    let mut binds = Vec::with_capacity_in(bodies.len(), Global);
    for (n, body) in bodies.iter().enumerate() {
        let RecordId(id) = record_id(&body.code);
        binds.push(wire::Bind {
            slot: Slot(n as u32),
            record: RecordId(id),
            code: CodeId(id),
            tier: Tier::Zero,
            len: body.code.bytes.len() as u64,
            align: 1 << body.code.align,
            refs: List::of(&[]),
        });
    }
    let mut table = Table { bodies, image, tls, vtables, missing: None, missing_routine: None, stops: (&raw const *stops) as u64 };
    // Not `together`: nothing runs before the commit, so no gate is needed.
    if let Err(error) = driver.edit(Txn(1), &binds, false, &mut table) {
        return Err(match (table.missing_routine, table.missing) {
            (Some(routine), _) => PlaceError::NoRoutine { routine, library: routine_library(routine) },
            (None, Some(name)) => PlaceError::NoSymbol { name },
            (None, None) => PlaceError::Edit(error),
        });
    }
    let shared = driver.shared();
    // The code area's addresses (`area` bytes from its base): where a trap is the program's.
    let base = driver.base();
    let code = base..base + area as usize;
    Ok(Placed { _driver: driver, shared, stops, code })
}

/// How the entry is called, by its signature.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Entry {
    /// `fn()`: the status is 0 when it returns.
    Unit,
    /// `extern "C" fn() -> i32`: the status is its low 8 bits, as the C runtime's `exit` takes it.
    I32,
    /// bstd's hosted entry, `extern "C" fn(*const HostTable) -> i32`: called with the host's
    /// table (crates/blaze-std/src/wire.rs); the status is its low 8 bits.
    Hosted(core::ptr::NonNull<core::ffi::c_void>),
    /// A Rust program's C entry shim, `extern "C" fn(argc: i32, argv: *const *const c_char) ->
    /// i32` calling `lang_start::<T>` (blaze-build start.rs `c_main_shim`): run only in the
    /// program's own process (run.md piece 6); [`Placed::call`] refuses it.
    CMainShim,
    /// A C `main`, `extern "C" fn(argc: i32, argv: *const *const c_char) -> i32` (C17
    /// 5.1.2.2.1): called with the run's argument vector (crate::argv `Argv::c`, whose `Argv`
    /// the caller keeps alive across the call); the status is its low 8 bits.
    Main(crate::argv::CArgs),
}

impl Placed {
    /// The address of `slot`'s entry thunk: what the function's address is. `None` for a slot
    /// not declared.
    pub fn thunk(&self, slot: u32) -> Option<u64> {
        self._driver.thunk(Slot(slot)).map(|at| at as u64)
    }

    /// Calls the function in `slot` on this thread: the process status it ends with, or the
    /// stub it called (a function that is not built: the program is left there, crate::stub).
    pub fn call(&self, slot: u32, entry: Entry) -> Result<Ended, PlaceError> {
        // A Rust program's start runs only in its own process (run.md piece 6, D103): its
        // `process::exit` would end this one. Refused before anything starts.
        if let Entry::CMainShim = entry {
            return Err(PlaceError::ShimInProcess);
        }
        let at = match self.shared.target(Slot(slot)) {
            Some(at) if at != 0 => at,
            _ => return Err(PlaceError::NoEntry { slot }),
        };
        let me = self.shared.birth().begin();
        // The entry's words, in `x0` and `x1` (stub.rs `enter`): the host's table for a hosted
        // entry; `argc` and `argv` for a C `main` (`argc` zero-extended: never negative,
        // crate::argv invariant 4); zero where the entry takes nothing (it reads neither).
        let (first, second) = match entry {
            Entry::Unit | Entry::I32 => (0, 0),
            Entry::Hosted(table) => (table.as_ptr() as usize, 0),
            Entry::Main(args) => (args.argc as u32 as usize, args.argv.as_ptr() as usize),
            // Refused at the top of this function.
            Entry::CMainShim => return Err(PlaceError::ShimInProcess),
        };
        // A trap in the placed code on this thread ends the run at stub.rs `leave` (crate::trap's
        // handler resumes there), caught by this watch, armed for the run only.
        let watch = crate::trap::Watch::new(self.code.start..self.code.end, self.stops.jump()).map_err(PlaceError::Trap)?;
        let armed = watch.arm().map_err(PlaceError::Trap)?;
        // SAFETY: `at` is the slot's word: the address of a body this module placed (`place`),
        // emitted by blaze-emit in the C convention for a function of no parameters (`Unit`,
        // `I32`), one pointer parameter in `x0` (`Hosted`: the host's table, alive for this
        // call: the caller holds the `Hosting` it points into) or a C `main`'s two (`Main`:
        // `argc` in `w0`, `argv` in `x1`, alive for this call: the caller holds the `Argv` it
        // points into; the program may write its strings and array, as C17 5.1.2.2.1 allows,
        // and nothing reads them during the call), in a mapping never unmapped (`sys::Code`);
        // `run_built`'s `entry_shape` matched the entry's signature to the variant; the
        // program's stubs were placed with `self.stops`' address, and `&self` is one run at a
        // time on this thread.
        let left = unsafe { self.stops.run(at, first, second) }.ok_or(PlaceError::NotThisMachine)?;
        let caught = armed.disarm();
        me.safepoint();
        if let Some(caught) = caught {
            let (code, slot, offset, callers) = crate::trap::locate(&self._driver, &caught).map_err(PlaceError::Trap)?;
            return Ok(Ended::Trap { code, slot, offset, callers });
        }
        if left.stopped != 0 {
            return Ok(Ended::Stub(left.value as u32));
        }
        Ok(Ended::Status(match entry {
            // `fn()`: nothing is returned.
            Entry::Unit => 0,
            // An `i32` in `w0`: its low 8 bits, as the C runtime's `exit` takes it.
            Entry::I32 | Entry::Hosted(_) | Entry::Main(_) => left.value as u8,
            // Never called here (refused at the top); a C `main`'s `i32` would be read the same.
            Entry::CMainShim => left.value as u8,
        }))
    }
}
