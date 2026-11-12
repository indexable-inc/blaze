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
//! | `Forget` | tells [`Records::forget`], which takes back the range's unwind entry and name |
//! | `Decommit` | gives the pages' memory back (`sys::Code::decommit`) |
//!
//! An edit is the core's transaction ([`Driver::edit`]): staged, filled, committed; a failure
//! while filling aborts it and nothing the program can reach has changed.
//!
//! # One mapping
//!
//! ```text
//! | slot words, 8 bytes each | stub head, stubs | entry thunks | (to a page) | the code area ...
//! 0                          stubs               thunks                       area
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

use blaze_launch::image::Image;
use blaze_launch::wire;
use blaze_launch::{
    Committed, Edit, Frame, KEPT, Kept, Launch, Placed, Slot, Target, Txn, Write,
};

use crate::arch::{self, GATE_ALIGN, GATE_ENTRY, GATE_HEAD, OutOfReach, STUB, STUB_HEAD, THUNK};
use crate::sys::{self, Code, SysError};
use crate::table::{Event, Shared, Told};

/// Where a bind's code comes from: the daemon's records, to the launcher.
/// [`crate::records::Loaded`] is the one a program runs on: it keeps the records the daemon
/// sends, checks each against its id, places a record's data and statics in memory of its
/// own and writes the code against them (`blaze_launch::place`). A test of the stores alone
/// has its bytes in a table.
pub trait Records {
    /// The daemon's [`wire::Tell::Record`]: the bytes of the record `id`, kept until the
    /// next edit or definition of statics was carried out. A refusal says why.
    fn record(&mut self, id: wire::RecordId, bytes: &[u8]) -> Result<(), String>;

    /// The daemon's [`wire::Tell::Statics`]: gives each static its memory, then its initial
    /// value. `place` has no `at`: a static's value holds only absolute addresses.
    fn statics(&mut self, defs: &[wire::StaticDef<'_>], place: Place) -> Result<(), String>;

    /// Clears `into` and fills it with the code of `bind` as it must be at `place`: exactly
    /// `bind.len` bytes, its fixups applied, checked against `bind.record`. A refusal says
    /// which record and why.
    fn code(
        &mut self,
        bind: &wire::Bind<'_>,
        place: Place,
        into: &mut Vec<u8>,
    ) -> Result<(), String>;

    /// The code [`Records::code`] gave for the range that begins at address `at` is where a
    /// thread can run it: its unwind entry and its name are told to whoever asks for them.
    fn placed(&mut self, at: u64, len: u64) -> Result<(), String>;

    /// The range that begins at address `at` is freed: no thread is or will be in it. What
    /// [`Records::placed`] told of it is taken back.
    fn forget(&mut self, at: u64);

    /// The message that named the records given so far was carried out, or failed: they are
    /// let go. The daemon gives a record again before each message that names it.
    fn release(&mut self);

    /// The address of static `number`, when it is defined and is no thread-local.
    fn static_at(&self, number: u32) -> Option<usize>;

    /// The daemon's [`wire::Tell::Image`], for records that hold nothing yet: takes the
    /// image's data, statics, thread-locals and outside symbols as its own, moves the
    /// image's addresses to this process's ([`Image::rebase`]: `place` gives the code
    /// mapping's, `area` the address of the code area's first byte) and keeps what it knows
    /// of each body, as [`Records::code`] does of one. The image's code is then what the
    /// driver copies into the area. Refused by records that are a test's table.
    fn image(&mut self, image: &mut Image, place: Place, area: u64) -> Result<(), String> {
        let _ = (image, place, area);
        Err("these records take no image of placed code".to_owned()) // alloc: an error
    }
}

/// Where a bind's code goes, and the addresses its fixups name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Place {
    /// The address of the code's first byte.
    pub at: u64,
    /// The address of slot 0's word.
    pub words: u64,
    /// The address of slot 0's entry thunk.
    pub thunks: u64,
    /// How many slots are declared: a word and a thunk exist for each below it.
    pub slots: u32,
}

impl Place {
    /// The address of `slot`'s word: what a call of the function loads. `None` for a slot
    /// that is not declared.
    pub fn word(&self, slot: Slot) -> Option<u64> {
        (slot.0 < self.slots).then(|| self.words + u64::from(slot.0) * 8)
    }

    /// The address of `slot`'s entry thunk: what the function's address is. `None` for a
    /// slot that is not declared.
    pub fn thunk(&self, slot: Slot) -> Option<u64> {
        (slot.0 < self.slots).then(|| self.thunks + u64::from(slot.0) * THUNK as u64)
    }
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
    /// What the launcher of a program uses: a million slots (24 to 40 bytes each of word,
    /// stub and thunk, touched when the slot is declared) and the rest of [`REACH`] as area.
    pub const PROGRAM: Config = Config { slots: 1 << 20, area: REACH - (64 << 20) };
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
    /// An image of placed code this launcher cannot take, and why.
    Image { why: String },
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
            Error::Image { why } => write!(f, "the daemon's image of placed code: {why}"),
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
    /// The offset in the mapping of slot 0's entry thunk.
    thunks: usize,
    /// The offset in the mapping of the code area's offset 0.
    area: usize,
    shared: Arc<Shared>,
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
        let thunks = stubs + STUB_HEAD + slots * STUB;
        let area = (thunks + slots * THUNK).next_multiple_of(page);
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
        let shared = Arc::new(Shared::new(
            config.slots,
            base,
            base + stubs,
            base + thunks,
            base + area..base + whole as usize,
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
            thunks,
            area,
            shared,
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

    /// Declares `count` more slots, each on its stub: writes the stubs' and the entry
    /// thunks' code, then stores each word.
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
        // Their entry thunks, which no code names until a record that takes the function's
        // address is placed; that is after this.
        self.scratch.clear();
        let base = self.code.base();
        for slot in first.0..first.0 + count {
            let at = (base + self.thunks + slot as usize * THUNK) as u64;
            let word = (base + slot as usize * 8) as u64;
            self.scratch.extend_from_slice(&arch::thunk(at, word).map_err(Error::Reach)?);
        }
        let at = self.thunks + first.0 as usize * THUNK;
        self.code.write(at, &self.scratch)?;
        self.switch("declare", &mut Nothing)?;
        Ok(first)
    }

    /// One edit, as one transaction: stages `binds`, fills their ranges from `records`,
    /// commits. `together` as `blaze_launch::Edit::together`.
    pub fn edit(
        &mut self,
        txn: Txn,
        binds: &[wire::Bind<'_>],
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
                Ok(()) => self.switch("abort", records),
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
        self.switch("commit", records).map_err(EditError::Broken)?;
        Ok(committed)
    }

    /// A launcher's whole start from an image of placed code (`blaze_launch::image`), for a
    /// driver that has declared and placed nothing: declares the image's slots, has
    /// `records` take its data and move its addresses to this process's, checks that the
    /// core hands each body the range the image has it at, copies the code in with one
    /// write, and commits as an edit of every body does. Gives the address of the entry's
    /// thunk and of each initializer static.
    ///
    /// What is checked before a byte is used: the image is for this instruction set, system
    /// and page size; the table's and the area's offsets in the mapping are this driver's;
    /// every range is the core's. A failure leaves the process with no program to run, and
    /// says why (rule `no-fallback`): the daemon made the image for this launcher.
    pub fn image(&mut self, mut image: Image, records: &mut dyn Records) -> Result<(usize, Vec<usize>), Error> {
        let refuse = |why: String| Error::Image { why };
        let page = sys::page()?;
        if self.core.declared() != 0 {
            return Err(refuse(format!("{} slots are declared already: an image is a launcher's start", self.core.declared())));
        }
        #[cfg(target_arch = "aarch64")]
        let arch = wire::Arch::Aarch64;
        #[cfg(target_arch = "x86_64")]
        let arch = wire::Arch::X86_64;
        #[cfg(target_os = "macos")]
        let os = wire::Os::Macos;
        #[cfg(target_os = "linux")]
        let os = wire::Os::Linux;
        if (image.arch, image.os, image.page) != (arch, os, page as u64) {
            return Err(refuse(format!(
                "made for {:?} on {:?} in pages of {}, and this launcher is {arch:?} on {os:?} in pages of {page}",
                image.arch, image.os, image.page
            )));
        }
        let geometry = image.geometry;
        if (geometry.thunks, geometry.thunk, geometry.area) != (self.thunks as u64, THUNK as u64, self.area as u64) {
            return Err(refuse(format!(
                "made for a mapping whose thunks are at {} ({} bytes each) and whose area is at {}, and this                  launcher's are at {} ({THUNK} bytes each) and {}",
                geometry.thunks, geometry.thunk, geometry.area, self.thunks, self.area
            )));
        }
        self.declare(image.slots)?;
        let base = self.base() as u64;
        records.image(&mut image, self.place(), base).map_err(refuse)?;
        if !image.bodies.is_empty() {
            // The image's own transaction: the daemon numbers its edits from 1.
            let txn = Txn(0);
            self.binds.clear();
            self.binds.extend(image.bodies.iter().map(|body| blaze_launch::Bind {
                slot: body.slot,
                len: body.len,
                align: u64::from(body.align),
                tier: body.tier,
                code: body.code,
            }));
            self.out.clear();
            self.core.stage(&Edit { txn, binds: &self.binds, together: false }, &mut self.out)?;
            for write in self.out.iter() {
                let Write::Place { bind, range } = *write else {
                    return Err(Error::Write { during: "stage of an image", write: *write });
                };
                let body = &image.bodies[bind as usize];
                if (range.at, range.len) != (body.at, body.len) {
                    return Err(refuse(format!(
                        "`{}` is at {} of the area in the image ({} bytes), and this launcher's core puts it at {}                          ({} bytes)",
                        body.name, body.at, body.len, range.at, range.len
                    )));
                }
            }
            // One write of every body, none of which a word names yet, and one barrier.
            self.code.copy(self.area, &image.area)?;
            self.code.settle()?;
            for body in &image.bodies {
                records.placed(base + body.at, body.len).map_err(refuse)?;
            }
            self.out.clear();
            self.core.commit(txn, &mut self.out)?;
            self.switch("commit of an image", records)?;
        }
        let Some(entry) = self.thunk(image.entry) else {
            return Err(refuse(format!("its entry is slot {}, which is not declared", image.entry.0)));
        };
        // alloc: the addresses of the program's initializer statics, once a process
        let mut inits = Vec::with_capacity(image.inits.len());
        for number in &image.inits {
            match records.static_at(*number) {
                Some(address) => inits.push(address),
                None => return Err(refuse(format!("static {number} is an initializer and has no address here"))),
            }
        }
        Ok((entry, inits))
    }

    /// Carries out the writes of a `stage`: code and gates into ranges nothing names.
    fn fill(
        &mut self,
        txn: Txn,
        binds: &[wire::Bind<'_>],
        records: &mut dyn Records,
    ) -> Result<(), Error> {
        let base = self.base() as u64;
        let words = self.code.base() as u64;
        let thunks = (self.code.base() + self.thunks) as u64;
        let slots = self.core.declared();
        for index in 0..self.out.len() {
            match self.out[index] {
                Write::Place { bind, range } => {
                    let bind = &binds[bind as usize];
                    let slot = bind.slot;
                    let place = Place { at: base + range.at, words, thunks, slots };
                    self.scratch.clear();
                    records
                        .code(bind, place, &mut self.scratch)
                        .map_err(|why| Error::Record { txn, slot, why })?;
                    let got = self.scratch.len() as u64;
                    if got != range.len {
                        return Err(Error::Length { txn, slot, said: range.len, got });
                    }
                    self.code.copy(self.area + range.at as usize, &self.scratch)?;
                    records
                        .placed(base + range.at, range.len)
                        .map_err(|why| Error::Record { txn, slot, why })?;
                }
                // The flag, clear. The area's offsets are never used twice, so the bytes are
                // the zeroes the mapping began with; they are written all the same, so the
                // gate does not rest on that.
                Write::Gate { range, .. } => {
                    self.code.copy(self.area + range.at as usize, &[0; 8])?;
                }
                Write::GateEntry { gate, entry, old, new } => {
                    let offset = gate.at + GATE_HEAD + u64::from(entry) * GATE_ENTRY;
                    let slot = binds[entry as usize].slot;
                    let old = self.address(slot, old) as u64;
                    let code = arch::gate_entry(base + offset, base + gate.at, old, base + new.at)
                        .map_err(Error::Reach)?;
                    self.code.copy(self.area + offset as usize, &code)?;
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
        // Once for the whole edit, before the commit stores a word that names any of it.
        self.code.settle()?;
        Ok(())
    }

    /// Carries out the writes of a `declare`, a `commit`, an `abort` or a `collect`, in the
    /// order given: the stores a running thread can see, and what is freed.
    fn switch(&mut self, during: &'static str, records: &mut dyn Records) -> Result<(), Error> {
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
                // Its unwind entry and its name go: no thread is in it, so no unwinder or
                // backtrace can ask for them any more.
                Write::Forget { range } => records.forget(self.base() as u64 + range.at),
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
        self.shared.next()
    }

    /// The next event: the control thread's one wait while it has nothing to do
    /// (`Shared::wait`).
    pub fn wait(&self) -> Event {
        self.shared.wait()
    }

    /// Frees every batch every thread cleared, and gives how many. Every birth announced
    /// before this call must have been given to [`Driver::told`] first: [`Driver::settle`]
    /// and the control thread's loop both drain the queue before they collect.
    pub fn collect(&mut self, records: &mut dyn Records) -> Result<usize, Error> {
        self.out.clear();
        let freed = self.core.collect(&mut self.out);
        self.switch("collect", records)?;
        Ok(freed)
    }

    /// The addresses a record's fixups name, for what is placed outside the code area (a
    /// static's value): the words and the thunks, and no address of its own.
    pub fn place(&self) -> Place {
        let base = self.code.base();
        let slots = self.core.declared();
        Place { at: 0, words: base as u64, thunks: (base + self.thunks) as u64, slots }
    }

    /// The address of `slot`'s entry thunk: the function's address.
    pub fn thunk(&self, slot: Slot) -> Option<usize> {
        self.shared.thunk(slot)
    }

    /// For a driver with no control thread (a test that is its own): gives the core every
    /// thread event that is ready, then collects. Any other event is refused by name.
    pub fn settle(&mut self, records: &mut dyn Records) -> Result<usize, Error> {
        while let Some(event) = self.ready() {
            match event {
                Event::Thread(told) => self.told(told)?,
                Event::Need { .. } => return Err(Error::Unserved { what: "a stub's need" }),
                Event::Frame(_) => return Err(Error::Unserved { what: "a frame of the daemon's" }),
                Event::Closed(_) => return Err(Error::Unserved { what: "the socket's end" }),
                Event::Stop => return Err(Error::Unserved { what: "the word to stop" }),
            }
        }
        self.collect(records)
    }
}

/// The records of a call that places and frees no code (a declaration): every method is a
/// refusal, since the core hands such a call no write that would reach one.
struct Nothing;

impl Records for Nothing {
    fn record(&mut self, _: wire::RecordId, _: &[u8]) -> Result<(), String> {
        Err("a record given where no code is placed".to_owned()) // alloc: an error
    }

    fn statics(&mut self, _: &[wire::StaticDef<'_>], _: Place) -> Result<(), String> {
        Err("statics defined where no code is placed".to_owned()) // alloc: an error
    }

    fn code(&mut self, _: &wire::Bind<'_>, _: Place, _: &mut Vec<u8>) -> Result<(), String> {
        Err("code asked for where none is placed".to_owned()) // alloc: an error
    }

    fn placed(&mut self, _: u64, _: u64) -> Result<(), String> {
        Err("code placed by a declaration".to_owned()) // alloc: an error
    }

    fn forget(&mut self, _: u64) {}

    fn release(&mut self) {}

    fn static_at(&self, _: u32) -> Option<usize> {
        None
    }
}
