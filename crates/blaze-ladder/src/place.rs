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

use blaze_emit::{Code, Fixup};
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

/// Places `bodies`, body `n` in slot `n`, in one edit.
pub fn place(bodies: &[Body]) -> Result<Placed, PlaceError> {
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
    let mut table = Table { bodies, missing: None };
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
        };
        me.safepoint();
        Ok(status)
    }
}
