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
}

impl core::fmt::Display for PlaceError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            PlaceError::Launcher(error) => write!(f, "the launcher: {error}"),
            PlaceError::Edit(error) => write!(f, "the launcher's edit: {error}"),
            PlaceError::NoSymbol { name } => {
                write!(f, "the outside symbol `{name}` is not in this process (dlsym found nothing)")
            }
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
    /// The first outside symbol that was not found, said after the edit fails.
    missing: Option<Box<str, Global>>,
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
                    let name = match routine {
                        Routine::Memcpy => "memcpy",
                        Routine::Memmove => "memmove",
                        Routine::Memset => "memset",
                    };
                    match symbol(name) {
                        Some(at) => at,
                        None => {
                            self.missing = Some(name.into());
                            return Err(format!("the system routine `{name}` is not in this process"));
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
                // The code builder's `Built::vtables` names the (type, principal) record at each
                // place; the ladder writes no vtable records yet (codegen.md, "Trait objects").
                RelocTo::Vtable(p) => return Err(format!("slot {slot}: vtable reference {p}: the ladder places no vtables yet")),
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
pub fn place(bodies: &[Body], image: Option<(&StaticImage<Global>, u64)>, tls: Option<Tls>) -> Result<Placed, PlaceError> {
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
    let mut table = Table { bodies, image, tls, missing: None };
    // Not `together`: nothing runs before the commit, so no gate is needed.
    if let Err(error) = driver.edit(Txn(1), &binds, false, &mut table) {
        return Err(match table.missing {
            Some(name) => PlaceError::NoSymbol { name },
            None => PlaceError::Edit(error),
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
