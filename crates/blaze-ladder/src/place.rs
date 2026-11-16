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
}

/// One function to place: its code and, by literal place, what each word is filled with.
pub struct Body {
    pub code: Code<Global>,
    pub targets: Vec<Target, Global>,
    /// By `Static` reference place: the static, found in the placed static image.
    pub statics: Vec<ItemKey, Global>,
    /// By `Tls` reference place: the thread-local's offset in a thread's block.
    pub tls: Vec<u32, Global>,
    /// By `Vtable` reference place: the program's table ([`Vtables`]) the word is the address of.
    pub vtables: Vec<u32, Global>,
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
    /// The program has thread-locals and its static image holds no descriptor for them.
    NoDescriptor,
    /// A vtable's word names a table the program does not have.
    NoVtable { table: u32 },
    /// A vtable's address word is not an aligned word inside it.
    VtableWord { table: u32, offset: u32 },
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
            PlaceError::Statics(DataError::NoFn { .. }) => {
                write!(f, "a static holds a fn pointer, which the ladder does not place yet (statics are written before the code)")
            }
            PlaceError::Statics(error) => write!(f, "the static image: {error:?}"),
            PlaceError::NoMemory { bytes } => write!(f, "no memory for the {bytes}-byte static image"),
            PlaceError::TlsKey { errno } => write!(f, "`pthread_key_create` for the thread-locals' key failed: errno {errno}"),
            PlaceError::NoDescriptor => write!(f, "the program has thread-locals and its static image holds no descriptor"),
            PlaceError::NoVtable { table } => write!(f, "a vtable's word names table {table}, which the program does not have"),
            PlaceError::VtableWord { table, offset } => write!(f, "vtable {table}: the address word at byte {offset} is not an aligned word inside it"),
        }
    }
}

impl core::error::Error for PlaceError {}

/// A placed program: the table and code stay mapped for the life of the process.
pub struct Placed {
    _driver: Driver,
    shared: Arc<Shared>,
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
        Routine::Memcpy | Routine::Memmove | Routine::Memset => "libc.so.6",
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
        | Routine::Fixunssfti => "libgcc_s.so.1",
    }
}

/// The address of `name` in this process.
fn symbol(name: &str) -> Option<u64> {
    let name = CString::new(name).ok()?;
    // SAFETY: `dlsym` reads a live NUL-terminated name and returns an address or null; the
    // handle `RTLD_DEFAULT` searches every image loaded, as a static link of the program
    // against the system's libraries would bind it.
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
                RelocTo::Static(p) => {
                    let found = body.statics.get(p as usize).and_then(|&key| {
                        let (image, base) = self.image?;
                        Some(base + u64::from(image.static_at(key)?))
                    });
                    match found {
                        Some(address) => address,
                        None => return Err(format!("slot {slot}: static reference {p} is in no placed static image")),
                    }
                }
                // The code builder's `Built::vtables` names the (type, principal) table at each
                // place; run.rs made each program table once (`Body::vtables`).
                RelocTo::Vtable(p) => match body.vtables.get(p as usize) {
                    Some(&t) => self.vtables.address(t, place)?,
                    None => return Err(format!("slot {slot}: vtable reference {p} is not in the body's table")),
                },
                RelocTo::TlsOffset(p) => match body.tls.get(p as usize) {
                    Some(&offset) => u64::from(offset),
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

/// Writes `image` into memory of its own and gives its address. The memory is never freed: the
/// statics live as long as the placed code does, the whole process. Done before the code is
/// placed, so a static holding a fn pointer is refused ([`PlaceError::Statics`]).
pub fn load_statics(image: &StaticImage<Global>) -> Result<u64, PlaceError> {
    use core::alloc::{Allocator, Layout};
    let bytes = image.bytes.len().max(1);
    let layout = Layout::from_size_align(bytes, 1usize << image.align).map_err(|_| PlaceError::NoMemory { bytes })?;
    let memory = Global.allocate_zeroed(layout).map_err(|_| PlaceError::NoMemory { bytes })?;
    let base = memory.as_ptr().cast::<u8>() as u64;
    // SAFETY: `memory` is a fresh allocation of `bytes` bytes, owned here and never freed or
    // aliased by another Rust reference; `fill` writes exactly `image.bytes.len()` of them.
    let out = unsafe { core::slice::from_raw_parts_mut(memory.as_ptr().cast::<u8>(), image.bytes.len()) };
    image.fill(base, out, |_| None).map_err(PlaceError::Statics)?;
    Ok(base)
}

/// Creates the program's thread-local key in the descriptor static at `desc` (its first
/// word), with `free` as the destructor of each thread's block (crates/blaze-build/src/tls.rs).
pub fn create_tls_key(desc: u64) -> Result<(), PlaceError> {
    // SAFETY: `desc` is the descriptor static in the image `load_statics` wrote, 8-aligned, its
    // first word the key (`tls::DESC_KEY`); `pthread_key_t` fits in it. Each block was made by
    // the getter's `aligned_alloc`, so `free` is its destructor, run once at its thread's exit.
    let errno = unsafe { libc::pthread_key_create(desc as *mut libc::pthread_key_t, Some(libc::free)) };
    if errno != 0 {
        return Err(PlaceError::TlsKey { errno });
    }
    Ok(())
}

/// Places `bodies`, body `n` in slot `n`, in one edit. `image` is the program's statics as
/// written at an address ([`load_statics`]); `tls` is what thread-local words name.
pub fn place(bodies: &[Body], vtables: &[VtableImage], image: Option<(&StaticImage<Global>, u64)>, tls: Option<Tls>) -> Result<Placed, PlaceError> {
    let vtables = Vtables::write(vtables)?;
    let page = blaze_launch_rt::sys::page().map_err(|e| PlaceError::Launcher(e.into()))? as u64;
    let code_len: u64 = bodies.iter().map(|b| (b.code.bytes.len() as u64).next_multiple_of(16)).sum();
    let area = (code_len + page).next_multiple_of(page).max(16 * page);
    let slots = u32::try_from(bodies.len()).unwrap_or(u32::MAX).max(1);
    let mut driver = Driver::new(Config { slots, area }).map_err(PlaceError::Launcher)?;
    if !bodies.is_empty() {
        driver.declare(slots).map_err(PlaceError::Launcher)?;
    }
    let mut binds = Vec::with_capacity_in(bodies.len(), Global);
    for (n, body) in bodies.iter().enumerate() {
        let id = blaze_ids::ContentId::of("blaze-ladder code 1", &body.code.bytes).0;
        binds.push(wire::Bind {
            slot: Slot(n as u32),
            record: RecordId(id),
            code: CodeId(id),
            tier: Tier::Zero,
            len: body.code.bytes.len() as u64,
            align: 16,
            refs: List::of(&[]),
        });
    }
    let mut table = Table { bodies, image, tls, vtables, missing: None, missing_routine: None };
    // Not `together`: nothing runs before the commit, so no gate is needed.
    if let Err(error) = driver.edit(Txn(1), &binds, false, &mut table) {
        return Err(match (table.missing_routine, table.missing) {
            (Some(routine), _) => PlaceError::NoRoutine { routine, library: routine_library(routine) },
            (None, Some(name)) => PlaceError::NoSymbol { name },
            (None, None) => PlaceError::Edit(error),
        });
    }
    let shared = driver.shared();
    Ok(Placed { _driver: driver, shared })
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
}

impl Placed {
    /// Calls the function in `slot` on this thread and gives the process status it ends with.
    pub fn call(&self, slot: u32, entry: Entry) -> Result<u8, PlaceError> {
        let at = match self.shared.target(Slot(slot)) {
            Some(at) if at != 0 => at,
            _ => return Err(PlaceError::NoEntry { slot }),
        };
        let me = self.shared.birth().begin();
        let status = match entry {
            Entry::Unit => {
                // SAFETY: `at` is the slot's word: the address of a body this module placed
                // (`place`), emitted by blaze-emit for a function of no parameters and no
                // result in the C convention, in a mapping never unmapped (`sys::Code`).
                let function: extern "C" fn() = unsafe { core::mem::transmute::<usize, extern "C" fn()>(at) };
                function();
                0
            }
            Entry::I32 => {
                // SAFETY: as above, for a function of no parameters returning an `i32` in `w0`.
                let function: extern "C" fn() -> i32 =
                    unsafe { core::mem::transmute::<usize, extern "C" fn() -> i32>(at) };
                function() as u8
            }
            Entry::Hosted(table) => {
                // SAFETY: as above, for a function of one pointer parameter in `x0` returning an
                // `i32` in `w0`; `table` is the host's table, alive for this call (the caller
                // holds the `Hosting` it points into).
                let function: extern "C" fn(*const core::ffi::c_void) -> i32 =
                    unsafe { core::mem::transmute::<usize, extern "C" fn(*const core::ffi::c_void) -> i32>(at) };
                function(table.as_ptr()) as u8
            }
        };
        me.safepoint();
        Ok(status)
    }
}
