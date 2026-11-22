//! [`Registry`]: the directory of live runs, and [`RunEvent`], the stream each run writes for a
//! viewer to read.
//!
//! A run registers itself as one small file named by its process (pid and the OS's start time
//! of that process, so a reused pid is another name), written whole (a temporary file renamed)
//! and removed when the run ends. A run killed before it removes its file leaves it behind: the
//! next reader of the directory finds that no process with that pid and that start time lives,
//! leaves it out of what it lists and removes it. So a read never lists a dead run.
//!
//! Beside its entry each run appends its events to its stream (`<name>.events`): a blaze-format
//! record in columns a flush ([`crate::events`]), keys and numbers only. A viewer tails the
//! streams of the runs a read lists.

#![blaze::sans_io]

use core::fmt;

use blaze_own::{AVec, Alloc, soa};

use crate::{CheckoutKey, InstanceKey, Nanos, Plant, TestId};

/// A process id.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Pid(pub u32);

/// When the OS says a process started (Linux: `starttime` of `/proc/<pid>/stat`; macOS:
/// `proc_pidinfo`'s `pbi_start_tvsec` and `_tvusec`): with the pid, what names one process for
/// ever, since a pid is reused.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ProcessStart(pub u64);

/// A run's name in the directory: `run-<pid>-<start>`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Name {
    pub pid: Pid,
    pub start: ProcessStart,
}

soa! {
    /// One live run, as its file holds it: its name, the checkout it is of, the program's entry
    /// instance and when it began, on the run's own clock. A read lists them as [`Entries`], by
    /// columns: a viewer picks runs by checkout or program, scanning that column alone.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct Entry, Entries, EntryRef {
        pub name: Name => names / names_mut,
        pub checkout: CheckoutKey => checkouts / checkouts_mut,
        pub program: InstanceKey => programs / programs_mut,
        pub began: Nanos => begans / begans_mut,
    }
}

/// The bytes of an entry: a version word, then the fields in order, little-endian.
pub const ENTRY_LEN: usize = 4 + 4 + 8 + 32 + 32 + 8;

const ENTRY_VERSION: u32 = 1;

impl Entry {
    pub fn encode(&self) -> [u8; ENTRY_LEN] {
        let mut out = [0; ENTRY_LEN];
        let mut at = Cursor(0);
        at.put(&mut out, &ENTRY_VERSION.to_le_bytes());
        at.put(&mut out, &self.name.pid.0.to_le_bytes());
        at.put(&mut out, &self.name.start.0.to_le_bytes());
        at.put(&mut out, &self.checkout.0);
        at.put(&mut out, &self.program.0);
        at.put(&mut out, &self.began.0.to_le_bytes());
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Entry, Bad> {
        if bytes.len() != ENTRY_LEN {
            return Err(Bad::Length { len: bytes.len(), want: ENTRY_LEN });
        }
        let mut at = Cursor(0);
        let version = u32::from_le_bytes(at.take(bytes));
        if version != ENTRY_VERSION {
            return Err(Bad::Version { version });
        }
        let pid = Pid(u32::from_le_bytes(at.take(bytes)));
        let start = ProcessStart(u64::from_le_bytes(at.take(bytes)));
        let checkout = CheckoutKey(at.take(bytes));
        let program = InstanceKey(at.take(bytes));
        let began = Nanos(u64::from_le_bytes(at.take(bytes)));
        Ok(Entry { name: Name { pid, start }, checkout, program, began })
    }
}

/// A place in a fixed-layout buffer whose length was checked first.
struct Cursor(usize);

impl Cursor {
    fn put(&mut self, out: &mut [u8], bytes: &[u8]) {
        out[self.0..self.0 + bytes.len()].copy_from_slice(bytes);
        self.0 += bytes.len();
    }

    fn take<const N: usize>(&mut self, bytes: &[u8]) -> [u8; N] {
        let mut out = [0; N];
        out.copy_from_slice(&bytes[self.0..self.0 + N]);
        self.0 += N;
        out
    }
}

/// Why bytes are not an entry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Bad {
    Length { len: usize, want: usize },
    Version { version: u32 },
}

impl fmt::Display for Bad {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Bad::Length { len, want } => write!(f, "{len} bytes where {want} are wanted"),
            Bad::Version { version } => {
                write!(f, "layout {version}, where this blaze writes {ENTRY_VERSION}")
            }
        }
    }
}

/// What the registry asks its driver to do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Effect {
    /// Write the entry file `name` whole (a temporary file, renamed).
    Put { name: Name, bytes: [u8; ENTRY_LEN] },
    /// Remove the entry file `name` and its stream; one already gone is not an error (another
    /// reader removed it first).
    Remove { name: Name },
    /// One event for `name`'s stream: the driver gathers them and appends each flush as one
    /// columnar batch ([`crate::events::write`]).
    Append { name: Name, event: RunEvent },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// A run that registers twice.
    Twice { name: Name },
    NotRegistered,
    /// A file of the directory that is not an entry: named, with why (an old layout after an
    /// upgrade is reported so, and the file is the person's to remove: nothing is migrated).
    BadEntry { name: Name, bad: Bad },
    /// An entry whose bytes name another run than its file.
    Misnamed { file: Name, holds: Name },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Error::Twice { name } => {
                write!(f, "run {}-{} registered twice", name.pid.0, name.start.0)
            }
            Error::NotRegistered => write!(f, "a run removed itself without registering"),
            Error::BadEntry { name, bad } => {
                write!(f, "the live-run entry run-{}-{} is not one: {bad}", name.pid.0, name.start.0)
            }
            Error::Misnamed { file, holds } => write!(
                f,
                "the live-run entry run-{}-{} holds run {}-{}",
                file.pid.0, file.start.0, holds.pid.0, holds.start.0
            ),
        }
    }
}

impl core::error::Error for Error {}

/// What the registry needs of the process table: the start of the process that has `pid` now,
/// if one has it. The driver asks the OS; a core never does.
pub trait Processes {
    fn start_of(&self, pid: Pid) -> Option<ProcessStart>;
}

/// One run's side of the directory. See the module's documentation.
pub struct Registry {
    me: Option<Name>,
    pub(crate) plant: Plant,
}

impl Default for Registry {
    fn default() -> Registry {
        Registry::new()
    }
}

impl Registry {
    pub const fn new() -> Registry {
        Registry { me: None, plant: Plant::None }
    }

    pub fn register(
        &mut self,
        entry: &Entry,
        out: &mut AVec<'_, Effect>,
    ) -> Result<(), Error> {
        if let Some(name) = self.me {
            return Err(Error::Twice { name });
        }
        self.me = Some(entry.name);
        out.push(Effect::Put { name: entry.name, bytes: entry.encode() });
        Ok(())
    }

    pub fn unregister(&mut self, out: &mut AVec<'_, Effect>) -> Result<(), Error> {
        let name = self.me.take().ok_or(Error::NotRegistered)?;
        out.push(Effect::Remove { name });
        Ok(())
    }

    /// One event of this run, for its stream.
    pub fn say(&self, event: &RunEvent, out: &mut AVec<'_, Effect>) -> Result<(), Error> {
        let name = self.me.ok_or(Error::NotRegistered)?;
        out.push(Effect::Append { name, event: *event });
        Ok(())
    }

    /// The directory as listed: the live runs pushed to `live`, which is then sorted by name;
    /// each entry of a dead process removed.
    pub fn read<P: Processes>(
        &self,
        listing: &[(Name, &[u8])],
        processes: &P,
        live: &mut Entries<Alloc<'_>>,
        out: &mut AVec<'_, Effect>,
    ) -> Result<(), Error> {
        for (file, bytes) in listing {
            let entry = Entry::decode(bytes).map_err(|bad| Error::BadEntry { name: *file, bad })?;
            if entry.name != *file {
                return Err(Error::Misnamed { file: *file, holds: entry.name });
            }
            let now = processes.start_of(entry.name.pid);
            let alive = match self.plant {
                Plant::PidOnly => now.is_some(),
                _ => now == Some(entry.name.start),
            };
            if alive {
                live.push(entry);
            } else {
                out.push(Effect::Remove { name: entry.name });
            }
        }
        live.sort_by_key(|entry| *entry.name);
        Ok(())
    }
}

/// One thing a run says happened, for the viewer: plain data, keys and numbers only.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RunEvent {
    Began { at: Nanos, program: InstanceKey, checkout: CheckoutKey },
    /// A compile of the function started (it had no record).
    Compiling { function: InstanceKey },
    /// The function's fast code is in its slot.
    Published { function: InstanceKey },
    /// The function's optimized code is in its slot.
    Tiered { function: InstanceKey },
    /// The function has no code; its callers fail.
    Failed { function: InstanceKey },
    TestPassed { test: TestId },
    TestFailed { test: TestId },
    /// The program's footprint at a sample.
    Memory { at: Nanos, bytes: u64 },
    Ended { at: Nanos, status: i32 },
}

