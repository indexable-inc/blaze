//! [`Driver`]: blaze-launch's core with real memory under it. It carries out each
//! `blaze_launch::Write` the core hands back, in order, and gives the core what threads say.
//!
//! One thread owns a `Driver` (every method that changes anything takes `&mut self`): the
//! control thread. That is the whole of the write side's synchronisation, and what
//! `sys::Code`'s invariant rests on. What each write is here:
//!
//! | `Write` | what the driver does |
//! |---|---|
//! | `Place` | asks [`Records`] for the bind's code as it must be at its address, checks its length, copies it into the area (`sys::Code::write`, which makes it runnable) |
//! | `Gate`, `GateEntry` | writes the flag, clear, and each entry's code (`arch::gate_entry`) |
//! | `Slot` | one atomic store of an address to the slot's word (and, for a slot being declared, its stub's code first) |
//! | `Commit` | one atomic store of 1 to the gate's flag |
//! | `Epoch` | one atomic store to the published epoch |
//! | `Forget` | nothing yet: no unwind or symbol entry is kept until step 3 of launcher.md |
//! | `Decommit` | gives the pages' memory back (`sys::Code::decommit`) |
//!
//! An edit is the core's transaction ([`Driver::edit`]): staged, filled, committed; a failure
//! while filling aborts it and nothing the program can reach has changed.
//!
//! # One mapping
//!
//! ```text
//! | slot words, 8 bytes each | stub head, stubs | (to a page) | the code area ...
//! 0                          stubs               area
//! ```
//!
//! A call loads its word relative to itself (`call [rip + word]`; `adrp`, `ldr`), so every
//! body must be within reach of the table: 2 GiB on x86-64, 4 GiB on AArch64. The table is
//! therefore in the mapping the code is in, before it, and the whole is at most [`REACH`]:
//! any body reaches any word, with no second copy of the table. (On macOS this also has to
//! be so: a `MAP_JIT` mapping cannot be placed at a chosen address beside another.) The
//! words are data that happen to lie in the code's mapping; a store to one goes through
//! `sys::Code::store`, as a gate's flag does.

use core::sync::atomic::Ordering;
use std::sync::Arc;

use blaze_launch::wire;
use blaze_launch::{
    Committed, Edit, Frame, KEPT, Kept, Launch, Placed, Slot, Target, Txn, Write,
};
use crossbeam_channel::Receiver;

use crate::arch::{self, GATE_ALIGN, GATE_ENTRY, GATE_HEAD, OutOfReach, STUB, STUB_HEAD};
use crate::sys::{self, Code, SysError};
use crate::table::{Event, Shared, Told};

/// Where a bind's code comes from: the store's records, to the launcher. Step 3 of
/// launcher.md makes the one that reads the store's directory, applies a record's fixups and
/// checks its content id; a test has its bytes in a table.
pub trait Records {
    /// The daemon's [`wire::Tell::Welcome`]: where the store is and how long the statics'
    /// initial image is.
    fn open(&mut self, store: &str, statics: u64) -> Result<(), String>;

    /// Clears `into` and fills it with the code of `bind` as it must be at address `at`:
    /// exactly `bind.len` bytes, its fixups applied, checked against `bind.record`. A refusal
    /// says which record and why.
    fn code(&mut self, bind: &wire::Bind, at: u64, into: &mut Vec<u8>) -> Result<(), String>;
}

/// What a launcher is made for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Config {
    /// The most slots the table holds.
    pub slots: u32,
    /// The code area's length: a whole number of pages. Address space only, until written.
    pub area: u64,
}

impl Config {
    /// What the launcher of a program uses: a million slots (8 MiB of words and 8 or 16 MiB
    /// of stubs, each touched when its slot is declared) and the rest of [`REACH`] as area.
    pub const PROGRAM: Config = Config { slots: 1 << 20, area: REACH - (32 << 20) };
}

/// The most bytes a launcher's mapping has: what a call on x86-64 reaches from its end to
/// the first word (a signed 32-bit displacement). AArch64 reaches twice as far; one bound
/// for both keeps a program's limits the same on every target.
pub const REACH: u64 = 2 << 30;

/// Why the driver could not do what it was asked.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Error {
    /// The core refused; its state is as it was.
    Core(blaze_launch::Error),
    Sys(SysError),
    Reach(OutOfReach),
    /// [`Records`] refused the code of a bind.
    Record { txn: Txn, slot: Slot, why: String },
    /// [`Records`] gave code of another length than the bind says.
    Length { txn: Txn, slot: Slot, said: u64, got: u64 },
    /// The core handed back a write that the call it answered never makes.
    Write { during: &'static str, write: Write },
    /// An event only a control thread serves arrived where there is none.
    Unserved { what: &'static str },
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::Core(why) => write!(f, "{why}"),
            Error::Sys(why) => write!(f, "the launcher's memory: {why}"),
            Error::Reach(why) => write!(f, "the launcher's code: {why}"),
            Error::Record { txn, slot, why } => {
                write!(f, "edit {}, slot {}: {why}", txn.0, slot.0)
            }
            Error::Length { txn, slot, said, got } => write!(
                f,
                "edit {}, slot {}: the bind says {said} bytes of code and its record gave {got}",
                txn.0, slot.0
            ),
            Error::Write { during, write } => {
                write!(f, "the launcher's core handed back {write:?} during {during}")
            }
            Error::Unserved { what } => {
                write!(f, "{what} arrived and no control thread is running to serve it")
            }
        }
    }
}

impl core::error::Error for Error {}

impl From<blaze_launch::Error> for Error {
    fn from(why: blaze_launch::Error) -> Error {
        Error::Core(why)
    }
}

impl From<SysError> for Error {
    fn from(why: SysError) -> Error {
        Error::Sys(why)
    }
}

/// How an edit ended when it was not committed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum EditError {
    /// Nothing of the edit is reachable and the table is as it was: the daemon is told
    /// `Failed` and the program runs on.
    Failed(Error),
    /// A store of the commit itself could not be made. The core cannot make this happen (it
    /// names only words and gates it handed out); if it does, the table is in no state the
    /// core knows and the launcher must stop.
    Broken(Error),
}

impl core::fmt::Display for EditError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            EditError::Failed(why) => write!(f, "{why}"),
            EditError::Broken(why) => write!(f, "the commit was cut short: {why}"),
        }
    }
}

impl core::error::Error for EditError {}

/// The launcher's core and its memory. See the module's documentation.
///
/// Dropping a `Driver` frees its tables and unmaps nothing (`sys::Code` is never unmapped):
/// code a thread is in, and the words it loads, outlive it.
pub struct Driver {
    core: Launch,
    /// The one mapping: words, stubs, area.
    code: Code,
    /// The offset in the mapping of the stub region's head.
    stubs: usize,
    /// The offset in the mapping of the code area's offset 0.
    area: usize,
    shared: Arc<Shared>,
    inbox: Receiver<Event>,
    /// The writes of the core's last call.
    out: Vec<Write, Kept>,
    /// The staged edit's binds, as the core takes them.
    binds: Vec<blaze_launch::Bind>,
    /// One bind's code on its way into the area.
    scratch: Vec<u8>,
    /// The bytes given back to the system so far.
    decommitted: u64,
}

impl Driver {
    /// Maps the words, the stubs and the code area, and writes the stubs' head. No slot is
    /// declared: every word is zero, and nothing may call through one until it is.
    pub fn new(config: Config) -> Result<Driver, Error> {
        let page = sys::page()?;
        let core = Launch::new(
            blaze_launch::Config {
                slots: config.slots,
                area: config.area,
                page: page as u64,
                gate_head: GATE_HEAD,
                gate_entry: GATE_ENTRY,
                gate_align: GATE_ALIGN,
            },
            KEPT,
        )?;
        let slots = config.slots as usize;
        let stubs = (slots * 8).next_multiple_of(16);
        let area = (stubs + STUB_HEAD + slots * STUB).next_multiple_of(page);
        let whole = area as u64 + config.area;
        if whole > REACH {
            return Err(Error::Reach(OutOfReach {
                what: "the launcher's mapping is longer than a call reaches its slot's word",
                from: whole,
                to: REACH,
            }));
        }
        let mut code = Code::map(whole as usize)?;
        let base = code.base();
        let (send, inbox) = crossbeam_channel::unbounded();
        let shared = Arc::new(Shared::new(
            config.slots,
            base,
            base + stubs,
            base + area..base + whole as usize,
            send,
        ));
        // The stubs hand this pointer to `table::resolve` for as long as the process runs
        // code: one count of the `Arc` is leaked for it, so it never dangles, whatever
        // happens to this `Driver`.
        let context = Arc::into_raw(Arc::clone(&shared)) as u64;
        let entry = arch::resolver as usize as u64;
        code.write(stubs, &arch::stub_head(context, entry))?;
        Ok(Driver {
            core,
            code,
            stubs,
            area,
            shared,
            inbox,
            out: Vec::new_in(KEPT),
            binds: Vec::new(),
            scratch: Vec::new(),
            decommitted: 0,
        })
    }

    /// What program threads hold.
    pub fn shared(&self) -> Arc<Shared> {
        Arc::clone(&self.shared)
    }

    /// The address of the code area's offset 0.
    pub fn base(&self) -> usize {
        self.code.base() + self.area
    }

    /// How many batches wait for a thread to clear them.
    pub fn waiting(&self) -> usize {
        self.core.waiting()
    }

    /// The bytes of the area given back to the system so far.
    pub fn decommitted(&self) -> u64 {
        self.decommitted
    }

    /// What `slot`'s word names.
    pub fn target(&self, slot: Slot) -> Result<Target, blaze_launch::Error> {
        self.core.target(slot)
    }

    /// The body a frame at an offset of the area is in, when a thread can be in it.
    pub fn placed(&self, frame: Frame) -> Option<Placed> {
        self.core.placed(frame)
    }

    /// The address a word holds for `to`. `slot` is the word's slot: a stub is its slot's.
    fn address(&self, slot: Slot, to: Target) -> usize {
        match to {
            Target::Stub => self.shared.stub(slot),
            Target::Body(body) => self.base() + body.at as usize,
            Target::Gate { gate, entry } => {
                self.base() + (gate.at + GATE_HEAD + u64::from(entry) * GATE_ENTRY) as usize
            }
        }
    }

    /// Declares `count` more slots, each on its stub: writes the stubs' code, then stores
    /// each word.
    pub fn declare(&mut self, count: u32) -> Result<Slot, Error> {
        self.out.clear();
        let first = self.core.declare(count, &mut self.out)?;
        // The stubs of the new slots, which nothing names until the words below are stored.
        self.scratch.clear();
        for slot in first.0..first.0 + count {
            self.scratch.extend_from_slice(&arch::stub(slot).map_err(Error::Reach)?);
        }
        let at = self.stubs + STUB_HEAD + first.0 as usize * STUB;
        self.code.write(at, &self.scratch)?;
        self.switch("declare")?;
        Ok(first)
    }

    /// One edit, as one transaction: stages `binds`, fills their ranges from `records`,
    /// commits. `together` as `blaze_launch::Edit::together`.
    pub fn edit(
        &mut self,
        txn: Txn,
        binds: &[wire::Bind],
        together: bool,
        records: &mut dyn Records,
    ) -> Result<Committed, EditError> {
        self.binds.clear();
        self.binds.extend(binds.iter().map(|bind| blaze_launch::Bind {
            slot: bind.slot,
            len: bind.len,
            align: u64::from(bind.align),
            tier: bind.tier,
            code: bind.code,
        }));
        self.out.clear();
        self.core
            .stage(&Edit { txn, binds: &self.binds, together }, &mut self.out)
            .map_err(|why| EditError::Failed(Error::Core(why)))?;
        if let Err(why) = self.fill(txn, binds, records) {
            // Nothing names what was staged: give it up, and free what was handed out.
            self.out.clear();
            let undone = match self.core.abort(txn, &mut self.out) {
                Ok(()) => self.switch("abort"),
                Err(core) => Err(Error::Core(core)),
            };
            return Err(match undone {
                Ok(()) => EditError::Failed(why),
                Err(broken) => EditError::Broken(broken),
            });
        }
        self.out.clear();
        let committed = match self.core.commit(txn, &mut self.out) {
            Ok(committed) => committed,
            Err(core) => return Err(EditError::Broken(Error::Core(core))),
        };
        self.switch("commit").map_err(EditError::Broken)?;
        Ok(committed)
    }

    /// Carries out the writes of a `stage`: code and gates into ranges nothing names.
    fn fill(
        &mut self,
        txn: Txn,
        binds: &[wire::Bind],
        records: &mut dyn Records,
    ) -> Result<(), Error> {
        let base = self.base() as u64;
        for index in 0..self.out.len() {
            match self.out[index] {
                Write::Place { bind, range } => {
                    let bind = &binds[bind as usize];
                    let slot = bind.slot;
                    self.scratch.clear();
                    records
                        .code(bind, base + range.at, &mut self.scratch)
                        .map_err(|why| Error::Record { txn, slot, why })?;
                    let got = self.scratch.len() as u64;
                    if got != range.len {
                        return Err(Error::Length { txn, slot, said: range.len, got });
                    }
                    self.code.write(self.area + range.at as usize, &self.scratch)?;
                }
                // The flag, clear. The area's offsets are never used twice, so the bytes are
                // the zeroes the mapping began with; they are written all the same, so the
                // gate does not rest on that.
                Write::Gate { range, .. } => {
                    self.code.write(self.area + range.at as usize, &[0; 8])?;
                }
                Write::GateEntry { gate, entry, old, new } => {
                    let offset = gate.at + GATE_HEAD + u64::from(entry) * GATE_ENTRY;
                    let slot = binds[entry as usize].slot;
                    let old = self.address(slot, old) as u64;
                    let code = arch::gate_entry(base + offset, base + gate.at, old, base + new.at)
                        .map_err(Error::Reach)?;
                    self.code.write(self.area + offset as usize, &code)?;
                }
                write @ (Write::Slot { .. }
                | Write::Commit { .. }
                | Write::Epoch { .. }
                | Write::Forget { .. }
                | Write::Decommit { .. }) => {
                    return Err(Error::Write { during: "stage", write });
                }
            }
        }
        Ok(())
    }

    /// Carries out the writes of a `declare`, a `commit`, an `abort` or a `collect`, in the
    /// order given: the stores a running thread can see, and what is freed.
    fn switch(&mut self, during: &'static str) -> Result<(), Error> {
        for index in 0..self.out.len() {
            match self.out[index] {
                // The word is 8 bytes times the slot into the mapping; a slot past the table
                // is past the words, which `store` cannot tell from the stubs after them.
                Write::Slot { slot, to } => {
                    if self.shared.word(slot).is_none() {
                        return Err(Error::Write { during, write: self.out[index] });
                    }
                    let address = self.address(slot, to) as u64;
                    self.code.store(slot.0 as usize * 8, address)?;
                }
                Write::Commit { gate } => self.code.store(self.area + gate.at as usize, 1)?,
                Write::Epoch { epoch } => self.shared.epoch.store(epoch, Ordering::SeqCst),
                // No unwind or symbol entry is kept yet (launcher.md, step 3): nothing to
                // take back.
                Write::Forget { .. } => {}
                Write::Decommit { range } => {
                    self.code.decommit(self.area + range.at as usize, range.len as usize)?;
                    self.decommitted += range.len;
                }
                write @ (Write::Place { .. } | Write::Gate { .. } | Write::GateEntry { .. }) => {
                    return Err(Error::Write { during, write });
                }
            }
        }
        self.out.clear();
        self.shared.waiting.store(self.core.waiting() as u64, Ordering::SeqCst);
        Ok(())
    }

    /// Gives the core what a thread said.
    pub fn told(&mut self, told: Told) -> Result<(), Error> {
        match told {
            Told::Began { thread, epoch } => self.core.thread_began(thread, epoch)?,
            Told::Ended { thread } => self.core.thread_ended(thread)?,
            Told::Report { thread, epoch, frames, complete } => {
                self.core.report(thread, epoch, &frames, complete)?;
            }
        }
        Ok(())
    }

    /// The next event, when one is there.
    pub fn ready(&self) -> Option<Event> {
        self.inbox.try_recv().ok()
    }

    /// The next event: the control thread's one wait while it has nothing to do, for a
    /// thread's report, a stub's need, a frame from the daemon or the word to stop. It has
    /// no bound: an idle launcher waits as long as its program runs. The queue cannot
    /// disconnect while `self` lives (its `Shared` holds a sender); were it to, that is the
    /// word to stop.
    pub fn wait(&self) -> Event {
        self.inbox.recv().unwrap_or(Event::Stop)
    }

    /// Frees every batch every thread cleared, and gives how many. Every birth announced
    /// before this call must have been given to [`Driver::told`] first: [`Driver::settle`]
    /// and the control thread's loop both drain the queue before they collect.
    pub fn collect(&mut self) -> Result<usize, Error> {
        self.out.clear();
        let freed = self.core.collect(&mut self.out);
        self.switch("collect")?;
        Ok(freed)
    }

    /// For a driver with no control thread (a test that is its own): gives the core every
    /// thread event that is ready, then collects. Any other event is refused by name.
    pub fn settle(&mut self) -> Result<usize, Error> {
        while let Some(event) = self.ready() {
            match event {
                Event::Thread(told) => self.told(told)?,
                Event::Need { .. } => return Err(Error::Unserved { what: "a stub's need" }),
                Event::Frame(_) => return Err(Error::Unserved { what: "a frame of the daemon's" }),
                Event::Closed(_) => return Err(Error::Unserved { what: "the socket's end" }),
                Event::Stop => return Err(Error::Unserved { what: "the word to stop" }),
            }
        }
        self.collect()
    }
}
