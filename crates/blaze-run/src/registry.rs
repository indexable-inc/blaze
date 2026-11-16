//! [`Registry`]: the directory of live runs, and [`RunEvent`], the stream each run writes for a
//! viewer to read.
//!
//! A run registers itself as one small file named by its process (pid and the OS's start time
//! of that process, so a reused pid is another name), written whole (a temporary file renamed)
//! and removed when the run ends. A run killed before it removes its file leaves it behind: the
//! next reader of the directory finds that no process with that pid and that start time lives,
//! leaves it out of what it lists and removes it. So a read never lists a dead run.
//!
//! Beside its entry each run appends its events to its stream (`<name>.events`): each one a
//! length byte and [`RunEvent::encode`]'s bytes, little-endian, nothing of the machine but the
//! pid. A viewer tails the streams of the runs a read lists.

#![blaze::sans_io]

use alloc::vec::Vec;
use core::alloc::Allocator;
use core::fmt;

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

/// One live run, as its file holds it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Entry {
    pub name: Name,
    pub checkout: CheckoutKey,
    /// The program's entry instance.
    pub program: InstanceKey,
    /// When it began, on the run's own clock.
    pub began: Nanos,
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

/// Why bytes are not an entry or an event.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Bad {
    Length { len: usize, want: usize },
    Version { version: u32 },
    Kind { kind: u8 },
    Tier { tier: u8 },
}

impl fmt::Display for Bad {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Bad::Length { len, want } => write!(f, "{len} bytes where {want} are wanted"),
            Bad::Version { version } => {
                write!(f, "layout {version}, where this blaze writes {ENTRY_VERSION}")
            }
            Bad::Kind { kind } => write!(f, "an event of unknown kind {kind}"),
            Bad::Tier { tier } => write!(f, "a tier of unknown number {tier}"),
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
    /// Append one event to `name`'s stream.
    Append { name: Name, event: Encoded },
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

    pub fn register<A: Allocator>(
        &mut self,
        entry: &Entry,
        out: &mut Vec<Effect, A>,
    ) -> Result<(), Error> {
        if let Some(name) = self.me {
            return Err(Error::Twice { name });
        }
        self.me = Some(entry.name);
        out.push(Effect::Put { name: entry.name, bytes: entry.encode() });
        Ok(())
    }

    pub fn unregister<A: Allocator>(&mut self, out: &mut Vec<Effect, A>) -> Result<(), Error> {
        let name = self.me.take().ok_or(Error::NotRegistered)?;
        out.push(Effect::Remove { name });
        Ok(())
    }

    /// One event of this run, for its stream.
    pub fn say<A: Allocator>(&self, event: &RunEvent, out: &mut Vec<Effect, A>) -> Result<(), Error> {
        let name = self.me.ok_or(Error::NotRegistered)?;
        out.push(Effect::Append { name, event: event.encode() });
        Ok(())
    }

    /// The directory as listed: the live runs into `live`, sorted by name; each entry of a dead
    /// process removed.
    pub fn read<A: Allocator, P: Processes>(
        &self,
        listing: &[(Name, &[u8])],
        processes: &P,
        live: &mut Vec<Entry, A>,
        out: &mut Vec<Effect, A>,
    ) -> Result<(), Error> {
        let first = live.len();
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
        live[first..].sort_unstable_by_key(|entry| entry.name);
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

/// The longest event: a kind byte, a time and two keys.
pub const MOST_EVENT: usize = 1 + 8 + 32 + 32;

/// One event's bytes, held without an allocator.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Encoded {
    bytes: [u8; MOST_EVENT],
    len: u8,
}

impl Encoded {
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }
}

impl RunEvent {
    pub fn encode(&self) -> Encoded {
        let mut bytes = [0; MOST_EVENT];
        let mut at = Cursor(0);
        match self {
            RunEvent::Began { at: time, program, checkout } => {
                at.put(&mut bytes, &[0]);
                at.put(&mut bytes, &time.0.to_le_bytes());
                at.put(&mut bytes, &program.0);
                at.put(&mut bytes, &checkout.0);
            }
            RunEvent::Compiling { function: key }
            | RunEvent::Published { function: key }
            | RunEvent::Tiered { function: key }
            | RunEvent::Failed { function: key } => {
                let kind = match self {
                    RunEvent::Compiling { .. } => 1,
                    RunEvent::Published { .. } => 2,
                    RunEvent::Tiered { .. } => 3,
                    _ => 4,
                };
                at.put(&mut bytes, &[kind]);
                at.put(&mut bytes, &key.0);
            }
            RunEvent::TestPassed { test } => {
                at.put(&mut bytes, &[5]);
                at.put(&mut bytes, &test.0);
            }
            RunEvent::TestFailed { test } => {
                at.put(&mut bytes, &[6]);
                at.put(&mut bytes, &test.0);
            }
            RunEvent::Memory { at: time, bytes: value } => {
                at.put(&mut bytes, &[7]);
                at.put(&mut bytes, &time.0.to_le_bytes());
                at.put(&mut bytes, &value.to_le_bytes());
            }
            RunEvent::Ended { at: time, status } => {
                at.put(&mut bytes, &[8]);
                at.put(&mut bytes, &time.0.to_le_bytes());
                at.put(&mut bytes, &status.to_le_bytes());
            }
        }
        Encoded { bytes, len: at.0 as u8 }
    }

    pub fn decode(bytes: &[u8]) -> Result<RunEvent, Bad> {
        let Some(&kind) = bytes.first() else {
            return Err(Bad::Length { len: 0, want: 1 });
        };
        let want = match kind {
            0 => 1 + 8 + 64,
            1..=6 => 1 + 32,
            7 => 1 + 8 + 8,
            8 => 1 + 8 + 4,
            _ => return Err(Bad::Kind { kind }),
        };
        if bytes.len() != want {
            return Err(Bad::Length { len: bytes.len(), want });
        }
        let mut at = Cursor(1);
        Ok(match kind {
            0 => {
                let time = Nanos(u64::from_le_bytes(at.take(bytes)));
                let program = InstanceKey(at.take(bytes));
                RunEvent::Began { at: time, program, checkout: CheckoutKey(at.take(bytes)) }
            }
            1 => RunEvent::Compiling { function: InstanceKey(at.take(bytes)) },
            2 => RunEvent::Published { function: InstanceKey(at.take(bytes)) },
            3 => RunEvent::Tiered { function: InstanceKey(at.take(bytes)) },
            4 => RunEvent::Failed { function: InstanceKey(at.take(bytes)) },
            5 => RunEvent::TestPassed { test: TestId(at.take(bytes)) },
            6 => RunEvent::TestFailed { test: TestId(at.take(bytes)) },
            7 => {
                let time = Nanos(u64::from_le_bytes(at.take(bytes)));
                RunEvent::Memory { at: time, bytes: u64::from_le_bytes(at.take(bytes)) }
            }
            _ => {
                let time = Nanos(u64::from_le_bytes(at.take(bytes)));
                RunEvent::Ended { at: time, status: i32::from_le_bytes(at.take(bytes)) }
            }
        })
    }
}
