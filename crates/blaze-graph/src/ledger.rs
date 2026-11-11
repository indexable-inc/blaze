//! The machine's memory ledger (docs/memory-budget.md, section 7): one fixed record for each
//! blaze daemon of the user on the host, so every daemon admits against one machine budget
//! rather than each against its own.
//!
//! A daemon joins at start, rewrites its charge (`max(held, granted)` and its lease) at each
//! turn, and leaves at exit; whoever holds the file's lock next prunes a record whose process
//! is gone or whose start time differs (its pid reused). A daemon's [`Allowance`] is the least
//! of its ceiling and the machine budget less every other live daemon's charge, and its plane
//! admits against that ([`crate::plane::PlaneInput::Allowance`]).
//!
//! The lease is the headroom a daemon with work takes at its turn and may grant with no turn
//! ([`crate::plane::Plane::lease`]): it is part of the daemon's charge, so every other daemon's
//! allowance is less by it, and what the daemons may grant between their turns never sums past
//! the machine budget.
//!
//! This is the core: the file's bytes in, the file's bytes out, and the arithmetic. It owns no
//! file, lock or clock. The driver (blaze-cargo's `serve::machine`) opens
//! `/tmp/blaze-<uid>/machine`, takes its `flock`, reads it whole, applies one op here
//! ([`Ledger::join`], [`Ledger::turn`], [`Ledger::leave`]), writes it whole back in place and
//! lets go of the lock, so each op is one atomic read-modify-write of the file. What the core
//! needs of the processes (whether pid P runs, and since when) comes through [`Alive`]; what it
//! needs of the plane, through [`Admits`].
//!
//! The file is [`LEN`] bytes, always written whole: a 32-byte header (magic, format, record
//! size, record count, an FNV-1a checksum of the rest) and [`MOST`] records of [`RECORD`]
//! bytes, little-endian, a free slot all zeros. An empty file is an empty ledger (a new one);
//! anything else that does not decode is [`Why`]'s corrupt kinds, an error naming the file,
//! never a ledger started empty (`persisted-state-round-trips`): starting empty would drop
//! every other daemon's charge.
//!
//! A record (offsets in bytes):
//!
//! | at  | what                                                                 |
//! |-----|----------------------------------------------------------------------|
//! | 0   | pid (`u32`, 0 only in a free slot)                                   |
//! | 4   | flags (`u8`: bit 0 the version was cut, bit 1 the home was cut)      |
//! | 5   | bytes of the version kept (`u8`, at most 24)                         |
//! | 6   | bytes of the home kept (`u8`, at most 24)                            |
//! | 7   | 0                                                                    |
//! | 8   | process start time (`u64`, the driver's unit)                        |
//! | 16  | the machine budget this daemon brought                               |
//! | 24  | its ceiling                                                          |
//! | 32  | held, as its plane last measured                                     |
//! | 40  | granted to its running tasks                                         |
//! | 48  | when it was written (the writer's wall clock; no decision reads it)  |
//! | 56  | the version's hash                                                   |
//! | 64  | the home's hash                                                      |
//! | 72  | the version's first 24 bytes                                         |
//! | 96  | its lease: what it may grant past `max(held, granted)` with no turn  |
//! | 104 | the home's last 24 bytes                                             |
//!
//! The machine budget is the least that any live daemon brought: each computes its own at
//! start (or is told it, `BLAZE_MACHINE_MEMORY_GB`), and the least is the one that holds for
//! all, so no daemon admits past a bound another was given.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![forbid(unsafe_code)]

use std::{
    fmt,
    path::{Path, PathBuf},
};

#[cfg(test)]
mod tests;

/// The file's first 8 bytes.
pub const MAGIC: [u8; 8] = *b"blzledgr";
/// The layout this module reads and writes; a file of another is refused, naming both.
/// Layout 2 added the lease, in 8 of the bytes layout 1 kept of the home.
pub const FORMAT: u32 = 2;
/// The most daemons one machine budget is shared by: joining past it is refused.
pub const MOST: usize = 64;
/// The bytes of one record.
pub const RECORD: usize = 128;
/// The bytes of the header.
pub const HEADER: usize = 32;
/// The bytes of the whole file.
pub const LEN: usize = HEADER + MOST * RECORD;

const VERSION: usize = 24;
const HOME: usize = 24;
/// Where a record's lease is, and where its home's bytes start.
const LEASE_AT: usize = 96;
const HOME_AT: usize = 104;
const _: () = assert!(HOME_AT + HOME == RECORD && LEASE_AT + 8 == HOME_AT);
const VERSION_CUT: u8 = 1;
const HOME_CUT: u8 = 2;

/// A process, as the ledger tells one from another: its pid and when it started, so a pid the
/// system gave again is not taken for the process that had it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Process {
    pub pid: u32,
    /// In the driver's unit: clock ticks since boot on Linux (`/proc/<pid>/stat`), microseconds
    /// since the epoch on macOS (`proc_pidinfo`).
    pub start: u64,
}

/// A text a record names (a version, a home), kept in `N` bytes: its hash (the driver's: the
/// build's id, an FNV-1a of a path) and as much of it as fits, its head or its tail.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Label<const N: usize> {
    hash: u64,
    kept: u8,
    cut: bool,
    bytes: [u8; N],
}

impl<const N: usize> Label<N> {
    pub const EMPTY: Label<N> = Label { hash: 0, kept: 0, cut: false, bytes: [0; N] };

    /// `text` by its first `N` bytes.
    pub fn head(text: &[u8], hash: u64) -> Label<N> {
        let kept = text.len().min(N);
        Label::of(&text[..kept], text.len() > kept, hash)
    }

    /// `text` by its last `N` bytes: a path, whose end says the most.
    pub fn tail(text: &[u8], hash: u64) -> Label<N> {
        let kept = text.len().min(N);
        Label::of(&text[text.len() - kept..], text.len() > kept, hash)
    }

    fn of(kept: &[u8], cut: bool, hash: u64) -> Label<N> {
        let mut bytes = [0; N];
        bytes[..kept.len()].copy_from_slice(kept);
        Label { hash, kept: kept.len() as u8, cut, bytes }
    }

    pub fn hash(&self) -> u64 {
        self.hash
    }

    /// The bytes kept of the text.
    pub fn kept(&self) -> &[u8] {
        &self.bytes[..usize::from(self.kept)]
    }

    /// Whether the text was longer than what is kept.
    pub fn cut(&self) -> bool {
        self.cut
    }
}

/// One daemon's record.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Record {
    pub process: Process,
    /// The machine budget this daemon computed, or was told, at start.
    pub budget: u64,
    /// The most it may ever grant (`BLAZE_DAEMON_MEMORY_GB`, never above `budget`).
    pub ceiling: u64,
    pub held: u64,
    pub granted: u64,
    /// The headroom it took at its last turn: what it may grant past `max(held, granted)`
    /// until its next ([`crate::plane::Plane::lease`]); 0 for a daemon with no work.
    pub lease: u64,
    /// When it was last written, by the writer's wall clock: for a person reading the file.
    pub at_ns: u64,
    pub version: Label<VERSION>,
    pub home: Label<HOME>,
}

impl Record {
    /// What the ledger counts against the machine for it: what it holds, or what it granted
    /// if more (a task granted memory may not have taken it yet), and its lease, which it may
    /// grant before any other daemon reads the ledger again.
    pub fn charge(&self) -> u64 {
        self.held.max(self.granted).saturating_add(self.lease)
    }
}

/// What a daemon may grant now, and what that came from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Allowance {
    /// The least of its ceiling and `machine` less `others`.
    pub bytes: u64,
    /// The machine budget: the least any live daemon brought.
    pub machine: u64,
    /// Every other live daemon's charge, summed.
    pub others: u64,
    /// The other live daemons.
    pub daemons: u32,
    pub ceiling: u64,
}

/// Which processes run, as the driver found them just before the op: `Some(start)` when a
/// process has `pid` now, started at `start`, in [`Process::start`]'s unit.
pub trait Alive {
    fn started(&self, pid: u32) -> Option<u64>;
}

/// The plane a [`Ledger::turn`] admits against.
pub trait Admits {
    /// Takes `allowance` as the most it may grant: it admits what now fits, and until the next
    /// call grants no more than what it then holds and its lease
    /// ([`crate::plane::PlaneInput::Allowance`]).
    fn allow(&mut self, allowance: &Allowance);
    /// The headroom past `max(held, granted)` it may grant until the next
    /// [`Admits::allow`], as the last one left it ([`crate::plane::Plane::lease`]). The ledger
    /// records it in this daemon's charge: one recorded smaller is room two daemons grant.
    fn lease(&self) -> u64;
    /// What the process holds, as last measured.
    fn held(&self) -> u64;
    /// What its running tasks were granted.
    fn granted(&self) -> u64;
}

/// What a [`Ledger::turn`] did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Turn {
    pub allowance: Allowance,
    /// The records pruned: processes gone, or pids now another process's.
    pub pruned: u32,
    /// The charge recorded for this daemon after its plane admitted, its lease in it.
    pub charge: u64,
    /// The lease in `charge`.
    pub lease: u64,
}

/// What went wrong, and in which ledger file.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LedgerError {
    pub path: PathBuf,
    pub why: Why,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Why {
    /// The file is neither empty nor [`LEN`] bytes.
    Length { len: usize },
    Magic,
    /// Written by a blaze whose ledger has another layout.
    Format { found: u32 },
    /// The header says records of another size, or another count.
    Shape { record: u32, most: u32 },
    Reserved,
    Checksum { stored: u64, computed: u64 },
    /// A slot with pid 0 that is not all zeros.
    FreeSlot { slot: usize },
    /// A record with bits set no layout has, or kept bytes past what a label holds.
    Fields { slot: usize },
    /// Two records of one process.
    Twice { slot: usize, pid: u32, start: u64 },
    /// A daemon joined that has a record already.
    Joined { pid: u32, start: u64 },
    /// Every slot holds a live daemon.
    Full,
    /// An op of a daemon with no record (pruned, or never joined).
    NotJoined { pid: u32, start: u64 },
    /// A record with pid 0, which marks a free slot.
    NoPid,
}

impl Why {
    /// Whether the file itself is bad, rather than the op asked of it.
    pub fn corrupt(&self) -> bool {
        matches!(
            self,
            Why::Length { .. }
                | Why::Magic
                | Why::Format { .. }
                | Why::Shape { .. }
                | Why::Reserved
                | Why::Checksum { .. }
                | Why::FreeSlot { .. }
                | Why::Fields { .. }
                | Why::Twice { .. }
        )
    }
}

impl fmt::Display for LedgerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path = self.path.display();
        let what = match self.why {
            Why::Length { len } => {
                return write!(
                    f,
                    "the machine's memory ledger {path} is corrupt: {len} bytes, where a ledger is \
                     empty or {LEN}; {}",
                    REMOVE
                );
            }
            Why::Magic => "its first 8 bytes are not `blzledgr`",
            Why::Format { found } => {
                return write!(
                    f,
                    "the machine's memory ledger {path} is of layout {found}, and this blaze reads \
                     layout {FORMAT}: a daemon of another blaze version runs, or ran; stop every \
                     blaze daemon of this user, then remove {path}"
                );
            }
            Why::Shape { record, most } => {
                return write!(
                    f,
                    "the machine's memory ledger {path} is corrupt: its header says {most} \
                     records of {record} bytes, where layout {FORMAT} has {MOST} of {RECORD}; {}",
                    REMOVE
                );
            }
            Why::Reserved => "a reserved field of its header is not 0",
            Why::Checksum { stored, computed } => {
                return write!(
                    f,
                    "the machine's memory ledger {path} is corrupt: its checksum is {stored:#018x} \
                     and its bytes sum to {computed:#018x} (a write torn, or another program's); {}",
                    REMOVE
                );
            }
            Why::FreeSlot { slot } => {
                return write!(
                    f,
                    "the machine's memory ledger {path} is corrupt: slot {slot} has pid 0 and is \
                     not all zeros; {REMOVE}"
                );
            }
            Why::Fields { slot } => {
                return write!(
                    f,
                    "the machine's memory ledger {path} is corrupt: the record in slot {slot} has \
                     flags or label lengths no layout {FORMAT} record has; {REMOVE}"
                );
            }
            Why::Twice { slot, pid, start } => {
                return write!(
                    f,
                    "the machine's memory ledger {path} is corrupt: slot {slot} holds pid {pid} \
                     (started at {start}) a second time; {REMOVE}"
                );
            }
            Why::Joined { pid, start } => {
                return write!(
                    f,
                    "pid {pid} (started at {start}) joined the machine's memory ledger {path} a \
                     second time: a daemon joins once, at start"
                );
            }
            Why::Full => {
                return write!(
                    f,
                    "the machine's memory ledger {path} has a live daemon in each of its {MOST} \
                     slots, the most that share one machine budget: stop a blaze daemon of this \
                     user (`blaze stop` in its home) and start this one again"
                );
            }
            Why::NotJoined { pid, start } => {
                return write!(
                    f,
                    "the machine's memory ledger {path} has no record of this daemon, pid {pid} \
                     (started at {start}): it never joined, or another daemon pruned it as gone"
                );
            }
            Why::NoPid => {
                return write!(
                    f,
                    "a record of pid 0 was to join the machine's memory ledger {path}, where pid 0 \
                     marks a free slot"
                );
            }
        };
        write!(f, "the machine's memory ledger {path} is corrupt: {what}; {REMOVE}")
    }
}

const REMOVE: &str = "no blaze daemon starts while it is: once no blaze daemon of this user runs, \
                      remove it";

impl std::error::Error for LedgerError {}

/// A planted bug the ledger's DST must catch (`testing` skill): each is caught by the oracle in
/// one of the seeded plans its control searches.
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    None,
    /// The driver reads the ledger, lets other daemons run their turns, then writes what it
    /// read back with its own change: a read-modify-write with no lock (the simulation's).
    UnlockedRmw,
    /// [`Ledger::prune`] keeps a record whose pid runs, whatever its start time: a dead
    /// daemon's charge kept by a process that reused its pid.
    PruneIgnoresStart,
    /// [`Ledger::allowance`] leaves the first other live daemon's charge out.
    AllowanceSkipsOne,
    /// [`Ledger::turn`] records no lease, whatever room the plane was given: the room past
    /// its charge is what another daemon is told is free.
    LeaseUnrecorded,
}

/// The ledger, decoded: one slot a daemon, `None` free.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Ledger<'p> {
    path: &'p Path,
    slots: [Option<Record>; MOST],
    #[cfg(test)]
    pub(crate) plant: Plant,
}

impl<'p> Ledger<'p> {
    /// The ledger with no record: a file that was empty.
    pub fn empty(path: &'p Path) -> Ledger<'p> {
        Ledger {
            path,
            slots: [None; MOST],
            #[cfg(test)]
            plant: Plant::None,
        }
    }

    /// The ledger the file `path` holds, `bytes` all of it.
    pub fn decode(path: &'p Path, bytes: &[u8]) -> Result<Ledger<'p>, LedgerError> {
        let mut ledger = Ledger::empty(path);
        if bytes.is_empty() {
            return Ok(ledger);
        }
        if bytes.len() != LEN {
            return Err(ledger.error(Why::Length { len: bytes.len() }));
        }
        if bytes[..8] != MAGIC {
            return Err(ledger.error(Why::Magic));
        }
        let found = u32_at(bytes, 8);
        if found != FORMAT {
            return Err(ledger.error(Why::Format { found }));
        }
        let (record, most) = (u32_at(bytes, 12), u32_at(bytes, 16));
        if record as usize != RECORD || most as usize != MOST {
            return Err(ledger.error(Why::Shape { record, most }));
        }
        if u32_at(bytes, 20) != 0 {
            return Err(ledger.error(Why::Reserved));
        }
        let (stored, computed) = (u64_at(bytes, 24), checksum(bytes));
        if stored != computed {
            return Err(ledger.error(Why::Checksum { stored, computed }));
        }
        for slot in 0..MOST {
            let at = HEADER + slot * RECORD;
            let row = &bytes[at..at + RECORD];
            let pid = u32_at(row, 0);
            if pid == 0 {
                if row.iter().any(|&it| it != 0) {
                    return Err(ledger.error(Why::FreeSlot { slot }));
                }
                continue;
            }
            let (flags, version_kept, home_kept) = (row[4], row[5], row[6]);
            let fields = flags & !(VERSION_CUT | HOME_CUT) == 0
                && row[7] == 0
                && usize::from(version_kept) <= VERSION
                && usize::from(home_kept) <= HOME
                && row[72 + usize::from(version_kept)..LEASE_AT].iter().all(|&it| it == 0)
                && row[HOME_AT + usize::from(home_kept)..RECORD].iter().all(|&it| it == 0);
            if !fields {
                return Err(ledger.error(Why::Fields { slot }));
            }
            let process = Process { pid, start: u64_at(row, 8) };
            if ledger.find(process).is_some() {
                return Err(ledger.error(Why::Twice { slot, pid, start: process.start }));
            }
            let mut version = [0; VERSION];
            version.copy_from_slice(&row[72..LEASE_AT]);
            let mut home = [0; HOME];
            home.copy_from_slice(&row[HOME_AT..RECORD]);
            ledger.slots[slot] = Some(Record {
                process,
                budget: u64_at(row, 16),
                ceiling: u64_at(row, 24),
                held: u64_at(row, 32),
                granted: u64_at(row, 40),
                lease: u64_at(row, LEASE_AT),
                at_ns: u64_at(row, 48),
                version: Label {
                    hash: u64_at(row, 56),
                    kept: version_kept,
                    cut: flags & VERSION_CUT != 0,
                    bytes: version,
                },
                home: Label {
                    hash: u64_at(row, 64),
                    kept: home_kept,
                    cut: flags & HOME_CUT != 0,
                    bytes: home,
                },
            });
        }
        Ok(ledger)
    }

    /// The file's bytes: what [`Ledger::decode`] reads back as this ledger, byte for byte.
    pub fn encode(&self, out: &mut [u8; LEN]) {
        out.fill(0);
        out[..8].copy_from_slice(&MAGIC);
        put_u32(out, 8, FORMAT);
        put_u32(out, 12, RECORD as u32);
        put_u32(out, 16, MOST as u32);
        for (slot, record) in self.slots.iter().enumerate() {
            let Some(record) = record else { continue };
            let row = &mut out[HEADER + slot * RECORD..HEADER + (slot + 1) * RECORD];
            put_u32(row, 0, record.process.pid);
            row[4] = if record.version.cut { VERSION_CUT } else { 0 }
                | if record.home.cut { HOME_CUT } else { 0 };
            row[5] = record.version.kept;
            row[6] = record.home.kept;
            put_u64(row, 8, record.process.start);
            put_u64(row, 16, record.budget);
            put_u64(row, 24, record.ceiling);
            put_u64(row, 32, record.held);
            put_u64(row, 40, record.granted);
            put_u64(row, 48, record.at_ns);
            put_u64(row, 56, record.version.hash);
            put_u64(row, 64, record.home.hash);
            row[72..LEASE_AT].copy_from_slice(&record.version.bytes);
            put_u64(row, LEASE_AT, record.lease);
            row[HOME_AT..RECORD].copy_from_slice(&record.home.bytes);
        }
        let sum = checksum(&out[..]);
        put_u64(out, 24, sum);
    }

    pub fn path(&self) -> &'p Path {
        self.path
    }

    /// The records, in slot order.
    pub fn records(&self) -> impl Iterator<Item = &Record> {
        self.slots.iter().flatten()
    }

    pub fn record(&self, me: Process) -> Option<&Record> {
        self.find(me).and_then(|slot| self.slots[slot].as_ref())
    }

    /// The machine budget: the least any recorded daemon brought; `None` with no record.
    pub fn machine(&self) -> Option<u64> {
        self.records().map(|it| it.budget).min()
    }

    /// Records `record` in the first free slot. Prune first ([`Ledger::prune`]): a dead
    /// daemon's slot is free only once pruned.
    pub fn join(&mut self, record: Record) -> Result<(), LedgerError> {
        let Process { pid, start } = record.process;
        if pid == 0 {
            return Err(self.error(Why::NoPid));
        }
        if self.find(record.process).is_some() {
            return Err(self.error(Why::Joined { pid, start }));
        }
        let Some(free) = self.slots.iter().position(Option::is_none) else {
            return Err(self.error(Why::Full));
        };
        self.slots[free] = Some(record);
        Ok(())
    }

    /// Takes `me`'s record out: a clean exit.
    pub fn leave(&mut self, me: Process) -> Result<Record, LedgerError> {
        let slot = self.joined(me)?;
        Ok(self.slots[slot].take().expect("a joined slot holds a record"))
    }

    /// Rewrites what `me` holds, was granted and leases, at `at_ns` by the writer's clock.
    pub fn charge(
        &mut self,
        me: Process,
        held: u64,
        granted: u64,
        lease: u64,
        at_ns: u64,
    ) -> Result<(), LedgerError> {
        let slot = self.joined(me)?;
        let record = self.slots[slot].as_mut().expect("a joined slot holds a record");
        record.held = held;
        record.granted = granted;
        record.lease = lease;
        record.at_ns = at_ns;
        Ok(())
    }

    /// Frees the slot of each record whose process is gone, or whose pid another process has
    /// now (its start time differs): how many.
    pub fn prune(&mut self, alive: &impl Alive) -> u32 {
        let mut pruned = 0;
        for slot in &mut self.slots {
            let Some(record) = slot else { continue };
            let started = alive.started(record.process.pid);
            #[cfg(test)]
            let lives = match self.plant {
                Plant::PruneIgnoresStart => started.is_some(),
                _ => started == Some(record.process.start),
            };
            #[cfg(not(test))]
            let lives = started == Some(record.process.start);
            if !lives {
                *slot = None;
                pruned += 1;
            }
        }
        pruned
    }

    /// What `me` may grant now: the least of its ceiling and the machine budget less every
    /// other recorded daemon's charge. Prune first: a dead daemon's charge counts until it is.
    pub fn allowance(&self, me: Process) -> Result<Allowance, LedgerError> {
        let mine = self.slots[self.joined(me)?].expect("a joined slot holds a record");
        let machine = self.machine().expect("a ledger with a record has a machine budget");
        let (mut others, mut daemons) = (0u64, 0u32);
        #[cfg(test)]
        let mut skip = self.plant == Plant::AllowanceSkipsOne;
        for record in self.records().filter(|it| it.process != me) {
            #[cfg(test)]
            if std::mem::take(&mut skip) {
                continue;
            }
            others = others.saturating_add(record.charge());
            daemons += 1;
        }
        Ok(Allowance {
            bytes: mine.ceiling.min(machine.saturating_sub(others)),
            machine,
            others,
            daemons,
            ceiling: mine.ceiling,
        })
    }

    /// One turn of `me` under the lock: prune, tell `plane` its allowance (it admits what
    /// fits), and record the charge it is left with: what it holds or granted, and the lease
    /// it took of the allowance, all it may grant until its next turn.
    pub fn turn(
        &mut self,
        me: Process,
        at_ns: u64,
        alive: &impl Alive,
        plane: &mut impl Admits,
    ) -> Result<Turn, LedgerError> {
        let pruned = self.prune(alive);
        let allowance = self.allowance(me)?;
        plane.allow(&allowance);
        let (held, granted, lease) = (plane.held(), plane.granted(), plane.lease());
        #[cfg(test)]
        let lease = if self.plant == Plant::LeaseUnrecorded { 0 } else { lease };
        self.charge(me, held, granted, lease, at_ns)?;
        Ok(Turn { allowance, pruned, charge: held.max(granted).saturating_add(lease), lease })
    }

    fn find(&self, me: Process) -> Option<usize> {
        self.slots.iter().position(|it| it.is_some_and(|it| it.process == me))
    }

    fn joined(&self, me: Process) -> Result<usize, LedgerError> {
        self.find(me).ok_or_else(|| self.error(Why::NotJoined { pid: me.pid, start: me.start }))
    }

    fn error(&self, why: Why) -> LedgerError {
        // alloc: the path an error names, once an error
        LedgerError { path: self.path.to_path_buf(), why }
    }
}

/// FNV-1a, 64 bits: the home's hash a driver records, and the file's checksum.
pub const fn fnv1a(bytes: &[u8]) -> u64 {
    fnv1a_from(0xcbf2_9ce4_8422_2325, bytes)
}

const fn fnv1a_from(mut hash: u64, bytes: &[u8]) -> u64 {
    let mut at = 0;
    while at < bytes.len() {
        hash ^= bytes[at] as u64;
        hash = hash.wrapping_mul(0x0100_0000_01b3);
        at += 1;
    }
    hash
}

/// The checksum of a whole file's `bytes`: FNV-1a of everything but the checksum's own field.
fn checksum(bytes: &[u8]) -> u64 {
    fnv1a_from(fnv1a(&bytes[..24]), &bytes[HEADER..])
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("4 bytes"))
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"))
}

fn put_u32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut [u8], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
}
