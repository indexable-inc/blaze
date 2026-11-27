//! A deterministic [`Driver`]: one thread, a virtual clock and a seeded scheduler, so a state
//! machine's run under it is a function of the seed. It delays completions, delivers the ones
//! that are ready in a shuffled order, fails effects at a chosen rate, and [`Sim::crash`]
//! loses what was not synced in any of the ways a power cut may: each unsynced [`SECTOR`] on
//! its own, so an append may lose its middle and keep its end. A failing seed replays exactly.
//!
//! Processes are the test's: [`Sim::program`] says what a program does when it is spawned, how
//! long it takes and what it ends with; a memory limit it passes kills it, as a cgroup does.
//! Sockets are in memory: a connect to an address listened on in the simulation makes both
//! ends, and [`Sim::dial`] is a peer from outside (an agent's client, a webhook) that the test
//! plays with [`Sim::peer_send`], [`Sim::peer_received`] and [`Sim::peer_hangup`].
//!
//! File locks ([`Effect::Lock`]) are a table of who holds each file's lock, by `flock`'s
//! rules: shared locks share, an exclusive one is alone, and a lock another excludes is
//! [`Done::Contended`], never waited for. [`Sim::hold_lock`] is another process holding one. A
//! lock follows its file as a rename moves it; a file removed or replaced takes its locks with
//! it, so a lock of the name made again is another's. A crash lets every lock go.
//!
//! Reads into memory of the asker's ([`Effect::ReadInto`]) and writes from it
//! ([`Effect::WriteFrom`]) are carried out against the registered memory itself
//! ([`crate::spans`]), as a kernel's are: a read scribbles its span with [`SCRIBBLE`] as it is
//! submitted and fills it as it completes, so an asker that reads a span in flight sees the
//! scribble; a write takes its bytes from memory as it completes, so memory changed while it was
//! in flight is what lands. A failed write, and one in flight at a crash, lands any of its
//! [`SECTOR`]s, and a crash tears a write from memory that was not synced the same way: each
//! sector kept or not, as a drive writes a request's blocks in any order. [`Sim::hold_reads`],
//! [`Sim::fail_reads`], [`Sim::short_reads`] and [`Sim::rot`] aim faults at the reads of the
//! store's pool.
//!
//! A name is durable only once its directory is synced, as on a real disk: a file made (an
//! append, a cut, a lock, a clone, a copy, a link), written whole in place of another without a
//! sync ([`Effect::WriteUnsynced`]), renamed without one ([`Effect::RenameUnsynced`]) or
//! removed is a change of a name a crash may undo (the name holds what it held before, a
//! renamed file is back under its old name), until [`Effect::SyncDir`] of its directory (for a
//! rename, the new name's). A file's [`Effect::Sync`] makes its bytes durable, never its name.
//! A directory made ([`Effect::MakeDir`]) or removed ([`Effect::RemoveDir`]) is a change of a
//! name of its parent's the same way (`sim/host/dir_names.rs`): undone, a directory made goes
//! with everything under it and one removed is back; a directory's own sync makes the names in
//! it durable, never its own name.
//! [`Effect::Write`] and [`Effect::Rename`] sync their directories before they complete, as
//! the native drivers do. A crash tears the bytes of an unsynced write by [`SECTOR`]s whether
//! or not it undoes it. A clone holds
//! its source's bytes as they are when it completes, whole and durable: a file system that
//! shares blocks writes its source's dirty pages out before it shares them. That write is the
//! clone's cost, and the simulation charges it: the bytes written to a file since its last
//! write-back ([`Effect::WriteBack`]) or sync are forced out by a clone of it and counted in
//! [`Sim::bytes_forced`], so a test of a path that must wait for no disk asserts that it forced
//! none. A write-back makes nothing durable (a crash tears what it covered as before); it and
//! a sync leave the file clean.
//! [`Sim::bytes_written`] and [`Sim::bytes_read`] count the bytes effects wrote and read, for a
//! test that an operation touches only what it must.
//!
//! Targeted faults, each until a crash: [`Sim::fail_syncs`] (a disk gone bad),
//! [`Sim::hold_syncs`] (a disk that stopped answering), [`Sim::slow_writes`],
//! [`Sim::hold_lists`] and [`Sim::fail_lists`], [`Sim::hold_sends`], [`Sim::fill`] (a disk
//! out of room: what does not fit fails with ENOSPC, an append keeping the part that fits), and
//! [`Sim::stall_holders`] (a lock holder descheduled or paging mid critical section: a test's
//! holder runs its section through [`Sim::critical_section`], or [`Sim::hold_for`] when other
//! actors step while it holds, and once in a while it stalls on the virtual clock).
//!
//! A waker, asked for ([`Sim::give_wakers`]): other threads' wakes are entries of the timeline
//! ([`Sim::wake_at`]) that land before a poll looks, between its look and its park, or while it
//! is parked, as the seed and the times place them; [`Sim::lose_wakes_while_parked`] plants a
//! lost wake; [`Sim::other_process`] is a second process on the machine, such as a test's
//! server beside a loop that polls on its own (`sim/wake.rs`).
//!
//! UDP sockets, the standard streams, the wall clock, random bytes and directories made empty
//! are the host's (`sim/host.rs`): datagrams lost, doubled and reordered at [`Sim::datagrams`]'
//! rates, the input the test's ([`Sim::stdin`]) and the output kept for it ([`Sim::output`]).
//!
//! What is in flight sits where a real driver keeps it (docs/v2/plan/design/memory.md,
//! "Generational handles"): effects in a [`Wheel`], spawns, watches and listens in [`Tagged`]
//! slabs named by the asker's tag, and the ends of connections in a [`Slab`] whose handle bits
//! are the [`Conn`], all tagged [`MemTag::Io`]. A warm cycle allocates nothing but what the
//! completions carry. A completion told for an op that has ended by the time it is due (a
//! change of a watch whose end came first, a receive on an end hung up meanwhile) names a stale
//! handle: it is detected when due, the change dropped and the receive told with nothing
//! appended to its buffer, and counted ([`Sim::stale`]). The scheduler makes both races happen
//! on some seeds: a watch that is ending is still told some of the changes made meanwhile, as a
//! system that queued them before it saw the unwatch.

pub mod block;
mod every_cut;
mod host;
mod journal;
mod kept;
#[cfg(test)]
mod pages;
mod wake;
#[cfg(test)]
mod world;
pub mod seeds;

use std::{cell::Cell, mem, path::PathBuf, time::Duration};

use blaze_hash::{FixedState, Map, MapOpts};
use blaze_mem::{Slab, Tag as MemTag};
use blaze_own::Share;
use std::alloc::Global;

pub use self::{host::Datagrams, journal::Journal, wake::Landed};
pub use self::every_cut::{
    AfterCrash, CrashPoint, FileCut, Inject, KINDS, Kind, Left, Refused, Steps, Sweep, Swept, Tear,
    UnknownKind, every_cut,
};
use crate::{
    Access, Address, Cache, Command, Completion, Conn, Done, Driver, Effect, Exit, FileName,
    IoError, LockMode, Meta, RECV_MOST, Span, Tag, spans,
    threads::tagged::{Id, Tagged},
    wait::{self, Clock, Deadline, Once},
    wheel::{self, Wheel},
};

/// What a simulated program does when spawned: how it ends, and how long it runs.
#[derive(Clone, Debug)]
pub struct Ran {
    pub exit: Exit,
    pub takes: Duration,
}

/// A simulated program: given the command it was spawned with and the simulation's random
/// numbers, what its run is.
// dyn: a test's program, given once per path and called once per spawn of it. `Send`: a
// simulated world is an `Io` like a native one, and an `Io` crosses to bstd's pool workers.
pub type Program = Box<dyn FnMut(&Command, &mut Rng) -> Ran + Send>;

/// The signal a kill or a passed memory limit ends a process with.
pub const KILLED: i32 = 9;

/// The simulated process's user: the one uid its Unix listens answer ([`Sim::dial_as`]).
pub const SIM_USER: u32 = 501;

/// The most ends of connections at once: their slab's reservation, committed as used.
const MAX_ENDS: u32 = 1 << 20;

/// The room reserved at the start for what is in flight at once, so a warm run does not grow.
const RESERVE: usize = 256;

/// A change made under a watch that is ending is still told once in this many, as a system
/// that queued it before it saw the unwatch: when it is due after the end, it is stale.
const RACE_ONE_IN: u64 = 3;

/// What a read into memory writes over its span as it is submitted: the bytes an asker sees
/// that reads a span while the read is in flight (the kernel may be writing it), and what a
/// failed read leaves.
pub const SCRIBBLE: u8 = 0x5A;

/// What a write from memory lands, or is torn, in: a drive's sector, each written whole or
/// not at all, in any order, when the power goes or the write fails. The disk world's, which
/// states the rule this simulation is held to.
pub use crate::world::disk::SECTOR;

type EndId = blaze_mem::Handle<End>;

fn end_of(conn: Conn) -> EndId {
    EndId::from_bits(conn.0)
}

/// One end of a connection.
#[derive(Debug, Default)]
struct End {
    /// The other end, or `None` for a peer outside the simulation the test plays.
    peer: Option<EndId>,
    /// Sent to this end, not yet received.
    inbox: Vec<u8>,
    /// Sent by this end to a peer outside, not yet taken by the test.
    outbox: Vec<u8>,
    /// This end hung up: kept only while a peer outside may still take what it was sent, and
    /// named by no effect.
    closed: bool,
    /// The other end hung up.
    peer_closed: bool,
    /// Its sends are held, in flight and never done, until it hangs up ([`Sim::hold_sends`]).
    holds_sends: bool,
}

/// The simulation's random numbers: blaze's one seeded SplitMix64.
pub use blaze_rng::SplitMix64 as Rng;

/// How unkind the simulated machine is, at random. The faults a test aims at one place
/// ([`Sim::fail_syncs`], [`Sim::fill`] and the rest in the module's docs) are the [`Sim`]'s.
#[derive(Clone, Debug)]
pub struct Faults {
    /// An effect fails once in this many (0: never). A failed append writes a random part of
    /// its bytes, as a short write does; a failed sync makes nothing durable; a failed write
    /// leaves the file as it was.
    pub fail_one_in: u32,
    /// Each completion is ready after a delay drawn up to this.
    pub max_latency: Duration,
    /// The file system cannot share blocks: every clone fails, as on ext4.
    pub no_clone: bool,
    /// A watch's change is reported as lost (the queue overflowed) once in this many.
    pub lose_change_one_in: u32,
    /// A poll's wait is interrupted once in this many calls (0: never), after a drawn part of
    /// it, as a signal ends a blocking system call: the poll waits again, through
    /// [`wait::until`], for what is left of its deadline.
    pub interrupt_one_in: u32,
}

impl Default for Faults {
    fn default() -> Faults {
        Faults {
            fail_one_in: 0,
            max_latency: Duration::from_micros(200),
            no_clone: false,
            lose_change_one_in: 0,
            interrupt_one_in: 0,
        }
    }
}

/// The bit of a mode that lets its owner write the file.
const OWNER_WRITES: u32 = 0o200;
/// What the system says of an open for writing it refuses (`EACCES`), as `io::Error` prints it.
const PERMISSION_DENIED: &str = "Permission denied (os error 13)";
/// What a failure the simulation injected says ([`Faults::fail_one_in`]).
const FAULT: &str = "injected by the simulation";

#[derive(Debug)]
struct SimFile {
    /// What survives a crash.
    durable: Vec<u8>,
    /// What changed it since the last sync that covered it, in order; read as part of the
    /// file, lost (in part) by a crash.
    ops: Vec<Op>,
    mode: u32,
    /// A symbolic link, its bytes its target.
    link: bool,
    /// How many of `ops`, from the first, were written back to the device
    /// ([`Effect::WriteBack`], or a clone that forced them out): not durable, but no longer
    /// only in memory. The ops after them are the file's dirty data.
    back: usize,
    /// The epoch each of `ops` was made in, as far as they are stamped
    /// ([`Sim::stamp_epochs`]): the count of barriers that had completed by then.
    stamps: Vec<u64>,
    /// Each barrier of the file ([`Effect::Barrier`]) no sync has covered yet: how many of
    /// `ops` it covers, and the epoch it completed in. A crash that keeps a change of a later
    /// epoch, of any file's bytes or of a name ([`NameChange`]), keeps the changes it covers.
    barriers: Vec<(usize, u64)>,
    /// The sectors of it that hold blocks ([`SECTOR`] each, by index): what a write covered,
    /// less what a cut dropped. A cut that grows the file, and every byte no write covered,
    /// is a hole, which reads as zeros and takes no block ([`Sim::allocated`]). A copy of
    /// its data ([`Effect::CopyData`]) and a clone keep its holes; a whole-file write has
    /// none. What a crash tears is not taken back out: it says what was ever written.
    blocks: std::collections::BTreeSet<u64>,
}

/// A change of a name no sync of its directory has covered yet ([`Sim::names`]): a crash may
/// undo it, and what it holds is what the undo puts back. The disk world's
/// [`crate::world::disk::Named`], by name.
#[derive(Debug)]
enum Named {
    /// A file put under `file`: made where none was (`was` none), or written whole in place
    /// of the file that was. Undone, the file goes and `was` is back as far as it was durable.
    Put { file: FileName, was: Option<SimFile> },
    /// The file under `file` removed. Undone, it is back as far as it was durable.
    Gone { file: FileName, was: SimFile },
    /// The file under `from` renamed to `to`, in place of `was`. Undone, the file is back
    /// under `from` and `was` under `to`.
    Moved { from: FileName, to: FileName, was: Option<SimFile> },
    /// The directory `dir` made where none was ([`Effect::MakeDir`]): a change of a name of its
    /// parent's. Undone, it goes, with every file and directory under it
    /// (`sim/host/dir_names.rs`).
    MadeDir { dir: FileName },
    /// The empty directory `dir` removed ([`Effect::RemoveDir`]). Undone, it is back, holding
    /// the names whose removal the same crash undoes.
    RemovedDir { dir: FileName },
}

impl Named {
    /// Whether it changes the name `name`.
    fn touches(&self, name: &FileName) -> bool {
        match self {
            Named::Put { file, .. } | Named::Gone { file, .. } => file == name,
            Named::Moved { from, to, .. } => from == name || to == name,
            Named::MadeDir { dir } | Named::RemovedDir { dir } => dir == name,
        }
    }

    /// Whether it and `other` change a name in common, or one is a directory's change and the
    /// other changes a name at or under that directory ([`Named::holds`]): undoing the earlier
    /// of the two undoes the later.
    fn shares(&self, other: &Named) -> bool {
        let common = match self {
            Named::Put { file, .. } | Named::Gone { file, .. } => other.touches(file),
            Named::Moved { from, to, .. } => other.touches(from) || other.touches(to),
            Named::MadeDir { dir } | Named::RemovedDir { dir } => other.touches(dir),
        };
        common || self.holds(other) || other.holds(self)
    }

    /// The directory whose sync makes it durable: the file's, or for a rename the new name's
    /// (`fs.rs`'s `rename` and the ring's syncs that one first, and the old name's directory
    /// too where it is another), or for a directory made or removed its parent.
    fn dir(&self) -> &str {
        match self {
            Named::Put { file, .. } | Named::Gone { file, .. } => parent_of(file),
            Named::Moved { to, .. } => parent_of(to),
            Named::MadeDir { dir } | Named::RemovedDir { dir } => parent_of(dir),
        }
    }
}

/// A change of a name in [`Sim::names`], and the epoch it was made in: how many barriers had
/// completed then, as a file's change is stamped ([`SimFile::stamps`]). A barrier orders what
/// it covers before every write asked for after it, a name's change included (the contract of
/// [`Effect::Barrier`]: "the bytes ordered before the name"), so a crash that keeps this
/// change keeps whole what every barrier of an earlier epoch covers ([`keep_ordered`]).
#[derive(Debug)]
struct NameChange {
    named: Named,
    epoch: u64,
}

/// The barriers' promise on one crash's cut, the one home of the rule that [`Sim::crash_cut`]
/// draws by and `sim/every_cut.rs` enumerates by: a change kept whole or in part, of any file
/// or name, of a later epoch than a barrier keeps whole what the barrier covers, which may in
/// turn be of an epoch later than another's; in rounds, until nothing moves.
///
/// `files` is each file's stamps (every change stamped, [`Sim::stamp_epochs`]) and barriers,
/// in the order of `cut`, which is each file's (changes kept whole, changes landed up to). A
/// change that may have landed in part is on the disk in part: it counts as kept, whatever its
/// sectors draw. `names` is the newest epoch of the changes of names the crash does not undo,
/// none where it undoes them all or there are none. `planted`
/// ([`host::Plant::BarriersNotKept`]): nothing is kept for a barrier.
fn keep_ordered(files: &[(&[u64], &[(usize, u64)])], names: Option<u64>, planted: bool, cut: &mut [(usize, usize)]) {
    if planted {
        return;
    }
    // Bounded: each round that moves raises some file's whole changes to what one of its
    // barriers covers, which no later round lowers.
    let rounds: usize = files.iter().map(|(_, barriers)| barriers.len()).sum();
    for _ in 0..=rounds {
        let mut newest = names;
        for ((stamps, _), (kept, upto)) in files.iter().zip(cut.iter()) {
            let reach = (*upto).max(*kept).min(stamps.len());
            if let Some(most) = stamps[..reach].iter().max().copied() {
                newest = Some(newest.map_or(most, |it| it.max(most)));
            }
        }
        let Some(newest) = newest else { break };
        let mut forced = false;
        for ((_, barriers), (kept, upto)) in files.iter().zip(cut.iter_mut()) {
            let covered =
                barriers.iter().filter(|(_, epoch)| *epoch < newest).map(|(covers, _)| *covers).max().unwrap_or(0);
            if covered > *kept {
                *kept = covered;
                // What a barrier covers is whole; what came after it may still be torn.
                *upto = (*upto).max(covered);
                forced = true;
            }
        }
        if !forced {
            break;
        }
    }
}

/// The directory `name` is directly in: everything before its last `/`, or the root's empty
/// name.
fn parent_of(name: &FileName) -> &str {
    match name.as_str().rsplit_once('/') {
        Some((dir, _)) => dir,
        None => "",
    }
}

/// What a crash keeps of one file's changes not synced ([`Sim::crash_cut`]).
#[derive(Debug)]
struct Cut {
    name: FileName,
    /// How many of them, from the first, are kept whole.
    kept: usize,
    /// How many of them, from the first, may have landed in part: at least `kept`. The tear
    /// is drawn after the barriers have had their say ([`Sim::crash_cut`]).
    upto: usize,
    /// What landed of the changes after those kept whole: the disk world's own value.
    torn: crate::world::disk::Torn,
}

/// What a crash does ([`Sim::crash_cut`]): what it keeps of each file, by name, and of each
/// change of a name not synced ([`Sim::names`], in its order) whether it is undone, each later
/// change that shares a name with one undone undone too.
#[derive(Debug)]
struct Cuts {
    files: Vec<Cut>,
    undone: Vec<bool>,
}

impl Default for SimFile {
    fn default() -> SimFile {
        // alloc: none, two empty vectors.
        SimFile {
            durable: Vec::new(),
            ops: Vec::new(),
            mode: 0o644,
            link: false,
            back: 0,
            // alloc: none, two empty vectors.
            stamps: Vec::new(),
            barriers: Vec::new(),
            // alloc: none, an empty set.
            blocks: std::collections::BTreeSet::new(),
        }
    }
}

impl SimFile {
    fn whole(bytes: Vec<u8>) -> SimFile {
        let mut file = SimFile::default();
        file.allocate(0, bytes.len() as u64);
        SimFile { durable: bytes, ..file }
    }

    /// The sectors from `at` for `len` bytes given blocks.
    fn allocate(&mut self, at: u64, len: u64) {
        if len == 0 {
            return;
        }
        let sector = SECTOR as u64;
        // alloc: a sector's entry, once per sector a simulated write covers.
        self.blocks.extend(at / sector..(at + len).div_ceil(sector));
    }

    /// A change made: the blocks it gives or drops, then the change kept.
    fn push_op(&mut self, op: Op) {
        match &op {
            Op::Append(more) => self.allocate(self.len(), more.len() as u64),
            Op::At(at, more) | Op::Sectors(at, more) => self.allocate(*at, more.len() as u64),
            // The sectors wholly past the new end go; the one it ends in keeps its block.
            Op::Cut(to) => {
                let keep = to.div_ceil(SECTOR as u64);
                self.blocks.retain(|it| *it < keep);
            }
        }
        self.ops.push(op);
    }

    /// The whole file, for the test's look at it or a copy of it.
    fn contents(&self) -> Vec<u8> {
        // clone: a copy of the file's bytes, which is what is asked for.
        let mut bytes = self.durable.clone();
        for op in &self.ops {
            op.apply(&mut bytes);
        }
        bytes
    }

    /// Its length with every change applied, without making its bytes.
    fn len(&self) -> u64 {
        self.len_after(self.ops.len())
    }

    /// Its durable bytes with its first `count` changes made: what a crash tears between.
    fn after(&self, count: usize) -> Vec<u8> {
        // clone: a crash, not an op: a copy of the bytes a tear mixes.
        let mut bytes = self.durable.clone();
        for op in &self.ops[..count.min(self.ops.len())] {
            op.apply(&mut bytes);
        }
        bytes
    }

    /// Its length with its first `count` changes applied, without making its bytes.
    fn len_after(&self, count: usize) -> u64 {
        self.ops[..count.min(self.ops.len())].iter().fold(self.durable.len() as u64, |len, op| match op {
            Op::Append(more) => len + more.len() as u64,
            Op::At(at, more) | Op::Sectors(at, more) => len.max(at + more.len() as u64),
            Op::Cut(to) => *to,
        })
    }

    /// The bytes written to it that are only in memory: what a clone of it forces out.
    fn dirty(&self) -> u64 {
        self.ops[self.back.min(self.ops.len())..]
            .iter()
            .map(|op| match op {
                Op::Append(more) | Op::At(_, more) | Op::Sectors(_, more) => more.len() as u64,
                Op::Cut(_) => 0,
            })
            .sum()
    }

    /// Its dirty data written to the device, none of it durable.
    fn write_back(&mut self) {
        self.back = self.ops.len();
    }

    /// The drive's cache flushed: what its barriers cover is durable.
    fn flushed(&mut self) {
        if let Some(covered) = self.barriers.iter().map(|(covers, _)| *covers).max() {
            self.sync(covered);
        }
    }

    /// The first `covers` of its changes made durable.
    fn sync(&mut self, covers: usize) {
        let SimFile { durable, ops, back, stamps, barriers, .. } = self;
        let covered = covers.min(ops.len());
        for op in ops.drain(..covered) {
            op.apply(durable);
        }
        stamps.drain(..covered.min(stamps.len()));
        // A barrier of what is durable now says nothing more; one of more covers the rest.
        barriers.retain_mut(|(covers, _)| {
            *covers = covers.saturating_sub(covered);
            *covers > 0
        });
        // What a sync covers it writes out; what came after it is as dirty as it was. Its name
        // is its directory's to make durable ([`Sim::sync_names`]), never this.
        *back = back.saturating_sub(covered);
    }

    /// As it is on the disk after a crash that keeps none of its changes not synced: what a
    /// crash puts back when it undoes the change of a name that replaced or removed it.
    fn durable_only(mut self, keep_barriers: bool) -> SimFile {
        self.ops.clear();
        self.back = 0;
        if !keep_barriers {
            self.stamps.clear();
            self.barriers.clear();
        }
        self
    }
}

/// A change to a file not yet synced.
#[derive(Debug)]
enum Op {
    Append(Vec<u8>),
    At(u64, Vec<u8>),
    /// A write from memory ([`Effect::WriteFrom`]): as [`Op::At`], torn by [`SECTOR`]s.
    Sectors(u64, Vec<u8>),
    Cut(u64),
}

impl Op {
    fn apply(&self, bytes: &mut Vec<u8>) {
        match self {
            Op::Append(more) => bytes.extend_from_slice(more),
            Op::At(at, more) | Op::Sectors(at, more) => {
                let at = *at as usize;
                if bytes.len() < at + more.len() {
                    bytes.resize(at + more.len(), 0);
                }
                bytes[at..at + more.len()].copy_from_slice(more);
            }
            Op::Cut(len) => bytes.resize(*len as usize, 0),
        }
    }

}

/// Whether `after`, a file read after a crash, is one the crash may leave of a file whose bytes
/// `durable` were synced and to which `appended` was then appended, unsynced, in one or more
/// appends: a test's oracle. It begins with all of `durable` and is no longer than both; each
/// [`SECTOR`] of it past `durable` holds `appended`'s bytes in their places up to some point
/// (an append kept whole may end inside a sector), then zeros (a hole) — a later sector may be
/// whole while an earlier one is zeros.
pub fn appended_and_torn(durable: &[u8], appended: &[u8], after: &[u8]) -> bool {
    if !after.starts_with(durable) || after.len() > durable.len() + appended.len() {
        return false;
    }
    let mut from = durable.len();
    while from < after.len() {
        let to = ((from / SECTOR + 1) * SECTOR).min(after.len());
        let new = &appended[from - durable.len()..to - durable.len()];
        let landed = after[from..to].iter().zip(new).take_while(|(a, b)| a == b).count();
        if after[from + landed..to].iter().any(|it| *it != 0) {
            return false;
        }
        from = to;
    }
    true
}

/// Any of the [`SECTOR`]s of `bytes` at `at`, each kept or not, as writes at their offsets: what
/// a write from memory leaves that failed or was cut by a crash. One kept past the file's end
/// leaves zeros before it, as a hole reads.
fn some_sectors(at: u64, bytes: &[u8], rng: &mut Rng) -> Vec<Op> {
    bytes
        .chunks(SECTOR)
        .enumerate()
        .filter(|_| rng.one_in(2))
        // alloc: a failure or a crash, not an op: the sectors that landed.
        .map(|(n, it)| Op::At(at + (n * SECTOR) as u64, it.to_vec()))
        .collect()
}

#[derive(Debug)]
struct Pending {
    effect: Effect,
    /// For a sync: how many changes the file had not synced when it was submitted.
    covers: usize,
    /// For a spawn: its run.
    spawn: Option<Id<Spawning>>,
}

/// A spawn in flight: its run, decided when it was spawned, and where its end waits.
struct Spawning {
    exit: Exit,
    pending: Option<wheel::Handle>,
}

struct Watching {
    /// A watch's named directory, or a journal's directory: named, or the root.
    dir: crate::DirName,
    /// Told as [`Done::Journaled`] from the disk's journal of changes.
    journal: bool,
    /// Unwatched: its end is told, and it is no longer in flight as a watch.
    ending: bool,
}

/// What a told completion names that can end before it is due.
#[derive(Clone, Copy)]
enum Of {
    /// Nothing that ends first.
    Op,
    /// A flush of the watch of this tag: its end is told after it.
    Flush(Tag),
    /// A change or journal batch of the watch: dropped if the watch has ended by then.
    Change(Id<Watching>),
    /// The watch's end: it ends as this is delivered.
    End(Id<Watching>),
    /// What a receive on the end got, after the `kept` bytes its buffer held: told with
    /// nothing appended if the end has hung up by then.
    Recv { end: EndId, kept: usize },
}

struct Told {
    at: Duration,
    completion: Completion,
    of: Of,
}

struct Receiving {
    tag: Tag,
    end: EndId,
    most: usize,
    /// The asker's buffer, appended to.
    into: Vec<u8>,
}

/// One folder above a simulation's root ([`Sim::set_above`]).
#[derive(Clone, Debug, Default)] // clone: a test's world, copied into each run
pub struct SimAbove {
    /// The name in it of the folder on the way down to the root.
    pub toward: std::sync::Arc<str>,
    /// Its other files, by names relative to it, with their bytes.
    pub files: Vec<(FileName, Vec<u8>)>,
}

pub struct Sim {
    rng: Rng,
    faults: Faults,
    now: Duration,
    /// The simulated machine's boot ([`Driver::boot`]): drawn from the seed, and drawn anew at
    /// every power cut ([`Sim::crash_with`]).
    boot: crate::Boot,
    /// The generator boots are drawn from: the seed's own, apart from `rng`, so that drawing a
    /// boot moves no other draw of a seed's run.
    boots: Rng,
    files: Map<FileName, SimFile, FixedState, Global>,
    /// The changes of names no sync of their directory has covered, oldest first: a crash may
    /// undo any of them, with every later one that shares a name ([`Sim::crash`]). A name is
    /// durable only once its directory is synced; a file's sync makes its bytes durable,
    /// never its name. A directory made ([`Effect::MakeDir`]) or removed
    /// ([`Effect::RemoveDir`]) is in it as a change of a name of its parent's, durable once the
    /// parent is synced; a directory's own sync never makes its own name durable
    /// (`sim/host/dir_names.rs`). Each is stamped with the epoch it was made in: a crash that
    /// keeps it keeps what the barriers before it cover ([`NameChange`]).
    names: Vec<NameChange>,
    /// The folders above the root, nearest first ([`Sim::set_above`]): read-only, so they are
    /// plain bytes, outside the disk's journal and its crashes.
    above: Vec<SimAbove>,
    /// Whose a file or directory is and who else can write it, where a test said it is not
    /// the user's alone ([`Sim::set_access`]).
    access: Map<FileName, Access, FixedState, Global>,
    /// Effects in flight by the tick of the virtual clock they are ready at. The clock ends
    /// some 584 years in ([`wheel::ticks`]): one due later is due at its end.
    pending: Wheel<Pending>,
    /// Each spawn in flight, for a kill to bring its end forward.
    spawns: Tagged<Spawning>,
    /// What was ready at the last poll, kept for the next's.
    ready: Vec<Pending>,
    watches: Tagged<Watching>,
    /// Of `watches`, those ending.
    ending: usize,
    /// Inodes, change times and the journal of changes, which a crash keeps.
    disk: journal::Disk,
    /// Changes and ends of watches, accepts and ends of listens, and what sockets did, each
    /// ready at its time, in the order they were told.
    told: Vec<Told>,
    /// What a poll left in `told`, swapped with it: both keep their room.
    later: Vec<Told>,
    programs: Map<PathBuf, Program, FixedState, Global>,
    listens: Tagged<Address>,
    ends: Slab<End>,
    /// Receives waiting for bytes or a hang-up.
    receiving: Vec<Receiving>,
    /// A file's bytes with its unsynced changes applied, made for a read and kept for the next.
    scratch: Vec<u8>,
    /// Completions told for an op that had ended when they were due.
    stale: u64,
    /// Syncs of files under this directory never complete ([`Sim::hold_syncs`]): a disk that
    /// stopped answering, as a hung NFS mount or a dying drive's queue does.
    hold_syncs: Option<FileName>,
    /// The syncs it holds, never told, each with the writes it covers as it was held; a crash
    /// drops them as it does every op in flight.
    held: Vec<(Effect, usize)>,
    /// The sends [`Sim::hold_sends`] holds: in flight, never done until their end hangs up.
    held_sends: Vec<(Conn, Tag, Vec<u8>)>,
    /// Syncs of files under this directory fail ([`Sim::fail_syncs`]): a disk that went bad.
    fail_syncs: Option<FileName>,
    /// Reads into memory of files under this directory are taken and not done until
    /// [`Sim::release_reads`] ([`Sim::hold_reads`]).
    hold_reads: Option<FileName>,
    /// The reads it holds, their spans scribbled.
    held_reads: Vec<Effect>,
    /// Reads into memory of files under this directory fail once in `.1` ([`Sim::fail_reads`]).
    fail_reads: Option<(FileName, u32)>,
    /// Reads into memory of files under this directory end short once in `.1`
    /// ([`Sim::short_reads`]).
    short_reads: Option<(FileName, u32)>,
    /// Lists of this directory wait until [`Sim::release_lists`] ([`Sim::hold_lists`]).
    hold_lists: Option<FileName>,
    /// The lists it holds, each listed as the directory is when released.
    held_lists: Vec<(crate::DirName, Tag)>,
    /// The next this many lists of this directory fail ([`Sim::fail_lists`]).
    fail_lists: Option<(FileName, u32)>,
    /// Writes, truncates and syncs of files under this directory each take this long
    /// ([`Sim::slow_writes`]): a disk that is slow but moving.
    slow: Option<(FileName, Duration)>,
    /// Completions of writes and syncs of files under these directories, in the order told,
    /// with the clock then ([`Sim::record`]).
    recording: Vec<FileName>,
    recorded: Vec<(Duration, FileName, bool)>,
    /// Appends and writes of files under this directory take their bytes from what is left of
    /// its room ([`Sim::fill`]): a disk filling up.
    room: Option<(FileName, u64)>,
    /// Who holds each file's lock ([`Effect::Lock`]), by the tag that took it, in the order
    /// taken; a file no one holds has no entry.
    locks: Map<FileName, Vec<(Tag, LockMode)>, FixedState, Global>,
    /// Locks held of files removed or replaced since: still held, so an unlock lets them go,
    /// but of a file no name leads to, so they exclude no one.
    unlinked: Vec<Tag>,
    /// The locks another process holds ([`Sim::hold_lock`]).
    outside: Map<FileName, LockMode, FixedState, Global>,
    /// The tags of each [`Effect::Pressure`] in flight, in the order asked: each is told of
    /// the memory pressure a test makes ([`Sim::press`]).
    pressures: Vec<Tag>,
    /// A critical section stalls once in `.0` (never 0), for a drawn time up to `.1`
    /// ([`Sim::stall_holders`]).
    stall_holders: Option<(u32, Duration)>,
    /// Bytes the effects that completed wrote to files ([`Sim::bytes_written`]) and read from
    /// them ([`Sim::bytes_read`]).
    bytes_written: u64,
    bytes_read: u64,
    /// Files mapped for writing ([`Effect::MapWrite`]) whose mapping may still be held: folded
    /// into their file by [`Sim::fold_drawn`] once it is dropped; forgotten at a crash.
    drawn: Vec<(FileName, Drawn)>,
    /// Data bytes kernel copies copied ([`Sim::bytes_copied`]).
    bytes_copied: u64,
    /// UDP sockets, the standard streams, the wall clock and directories made (`sim/host.rs`).
    host: host::Host,
    /// Bytes clones forced out of their sources ([`Sim::bytes_forced`]).
    bytes_forced: u64,
    /// How many barriers ([`Effect::Barrier`]) have completed: the epoch changes are stamped
    /// with ([`Sim::stamp_epochs`]).
    epoch: u64,
    /// What a test under seeds armed it with ([`Sim::arm`]): its power cuts, trace and counts.
    armed: Option<Box<seeds::Armed>>,
    /// The descriptors a real driver keeps for reads into memory, modelled
    /// (`crate::descriptors`): a file opened once, the least recently read closed past the
    /// bound, an unlinked file read as the bytes it held.
    descriptors: kept::Descriptors,
    /// The waker it hands out, the other threads' acts due and the other processes beside the
    /// poller (`sim/wake.rs`), once a test asked for them ([`Sim::give_wakers`]).
    wakes: Option<Box<wake::Wakes>>,
    /// The opt-in fault kinds a test switched on ([`Sim::inject`], `sim/every_cut.rs`): none
    /// unless asked, and a kind off draws no number.
    inject: every_cut::Inject,
}

/// A delay drawn up to the simulation's latency.
fn latency(rng: &mut Rng, faults: &Faults) -> Duration {
    Duration::from_nanos(rng.below(faults.max_latency.as_nanos() as u64 + 1))
}

/// `at`, or later: after everything told of `a` or `b`.
fn after_told(told: &[Told], at: Duration, a: Tag, b: Tag) -> Duration {
    told.iter()
        .filter(|it| {
            it.completion.tag == a
                || it.completion.tag == b
                || matches!(it.of, Of::Flush(watch) if watch == a)
        })
        .map(|it| it.at)
        .fold(at, Duration::max)
}

/// The slot a simulated [`Effect::MapWrite`]'s bytes come back through: empty while the task's
/// [`crate::MappingWrite`] holds them, filled when it is dropped. The simulator drains it at
/// its next effect ([`Sim::fold_drawn`]) and folds the bytes into the file as unsynced writes
/// from memory ([`Op::Sectors`]), so a crash before a sync tears them by sectors as any other.
pub(crate) type Drawn = std::sync::Arc<crate::lock::Lock<Option<Vec<u8>>>>;

/// The file `name` of the simulated disk, made empty if it is not there.
fn file_mut<'a>(
    files: &'a mut Map<FileName, SimFile, FixedState, Global>,
    name: &FileName,
) -> &'a mut SimFile {
    if !files.contains_key(name) {
        // clone: once per file new to the simulated disk, its key.
        files.insert(name.share(), SimFile::default());
    }
    files.get_mut(name).expect("there now")
}

impl Sim {
    pub fn new(seed: u64, faults: Faults) -> Sim {
        let mut sim = Sim {
            rng: Rng::new(seed),
            faults,
            now: Duration::ZERO,
            // Drawn below, once the boots' generator is there.
            boot: crate::Boot([0; 16]),
            boots: Rng::new(seed ^ 0xB007_B007_B007_B007),
            files: Map::new(MapOpts { hasher: FixedState, alloc: Global, capacity: None }),
            names: Vec::new_in(Global),
            // alloc: none until a test sets folders above the root.
            above: Vec::new(),
            access: Map::new(MapOpts { hasher: FixedState, alloc: Global, capacity: None }),
            pending: Wheel::new(),
            spawns: Tagged::new(RESERVE),
            ready: Vec::with_capacity(RESERVE),
            watches: Tagged::new(RESERVE),
            ending: 0,
            disk: journal::Disk::default(),
            told: Vec::with_capacity(RESERVE),
            later: Vec::with_capacity(RESERVE),
            programs: Map::new(MapOpts { hasher: FixedState, alloc: Global, capacity: None }),
            listens: Tagged::new(RESERVE),
            ends: match Slab::new(MAX_ENDS, MemTag::Io) {
                Ok(ends) => ends,
                Err(error) => panic!("{error}"),
            },
            receiving: Vec::with_capacity(RESERVE),
            // alloc: none until the first read of a file with unsynced changes.
            scratch: Vec::new(),
            stale: 0,
            hold_syncs: None,
            // alloc: none until a sync is held.
            held: Vec::new(),
            // alloc: none until a send is held.
            held_sends: Vec::new(),
            fail_syncs: None,
            hold_reads: None,
            // alloc: none until a read is held.
            held_reads: Vec::new(),
            fail_reads: None,
            short_reads: None,
            hold_lists: None,
            held_lists: Vec::new(), // alloc: empty, allocates nothing
            fail_lists: None,
            slow: None,
            recording: Vec::new(),
            recorded: Vec::new(),
            room: None,
            locks: Map::new(MapOpts { hasher: FixedState, alloc: Global, capacity: None }),
            // alloc: none until a locked file is removed.
            unlinked: Vec::new(),
            outside: Map::new(MapOpts { hasher: FixedState, alloc: Global, capacity: None }),
            // alloc: none until memory pressure is watched.
            pressures: Vec::new(),
            stall_holders: None,
            bytes_written: 0,
            bytes_read: 0,
            // alloc: none until a file is mapped for writing.
            drawn: Vec::new(),
            bytes_copied: 0,
            host: host::Host::new(seed),
            bytes_forced: 0,
            epoch: 0,
            armed: None,
            descriptors: kept::Descriptors::new(crate::descriptors::MOST),
            wakes: None,
            inject: every_cut::Inject::NONE,
        };
        // The disk's clock is the wall clock from the start, so its first stamps are of then.
        sim.disk.at(sim.wall_ns());
        sim.boot = sim.draw_boot();
        sim
    }

    /// A new boot of the simulated machine: 16 bytes from the boots' generator.
    fn draw_boot(&mut self) -> crate::Boot {
        let (high, low) = (self.boots.next_u64(), self.boots.next_u64());
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&high.to_le_bytes());
        bytes[8..].copy_from_slice(&low.to_le_bytes());
        crate::Boot(bytes)
    }

    /// How many files reads into memory opened, and how many descriptors closed, as a real
    /// driver's table counts them ([`crate::Threads::opens`]).
    pub fn opens(&self) -> crate::Opens {
        self.descriptors.opens()
    }

    /// How many descriptors the modelled table keeps open now.
    pub fn descriptors_kept(&self) -> usize {
        self.descriptors.len()
    }

    /// The table of descriptors keeps at most `bound` from now on (a real driver's is
    /// `blaze_core`'s 1024, or a quarter of `RLIMIT_NOFILE`): for a test of what happens at
    /// the bound. Past it the least recently read closes; a read is never refused.
    pub fn bound_descriptors(&mut self, bound: usize) {
        self.descriptors.bound(bound);
    }

    /// The bytes every effect that completed wrote into files, ever: appends, whole-file
    /// writes, writes at an offset and from memory. A clone, a rename and a truncate write
    /// none. For a test that an operation writes only what it must.
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }

    /// The bytes clones forced out of their sources, ever: a clone of a file holding bytes
    /// written since its last write-back ([`Effect::WriteBack`]) or sync makes the file system
    /// write those bytes to the device before it shares the blocks, and waits for them. For a
    /// test that a path clones only files whose write-back completed (0 here), with no real
    /// disk: on one, that forced write is what the path would have waited for.
    pub fn bytes_forced(&self) -> u64 {
        self.bytes_forced
    }

    /// The bytes every effect that completed read from files, ever: reads, reads into memory
    /// and whole files mapped (whose asker reads them). A clone reads none.
    pub fn bytes_read(&self) -> u64 {
        self.bytes_read
    }

    /// The system says memory is short: each [`Effect::Pressure`] in flight is told
    /// [`Done::Pressed`], `critical` as given, after a delay drawn as any completion's.
    pub fn press(&mut self, critical: bool) {
        for at in 0..self.pressures.len() {
            let tag = self.pressures[at];
            self.tell(tag, Done::Pressed { critical }, Of::Op);
        }
    }

    /// `effect` ready `after` from now.
    fn pend(&mut self, after: Duration, effect: Effect, covers: usize) {
        let ready = wheel::ticks(self.now).saturating_add(wheel::ticks(after));
        self.pending.insert(ready, Pending { effect, covers, spawn: None });
    }

    /// What a spawn of `path` does. A spawn of a program not given fails, as one not on disk.
    pub fn program(
        &mut self,
        path: impl Into<PathBuf>,
        program: impl FnMut(&Command, &mut Rng) -> Ran + Send + 'static,
    ) {
        // alloc: once per program the test gives.
        self.programs.insert(path.into(), Box::new(program));
    }

    /// A peer outside the simulation connects to `at`: the listen there accepts the end this
    /// returns, which the test plays the other side of.
    pub fn dial(&mut self, at: &Address) -> Result<Conn, IoError> {
        self.dial_as(at, SIM_USER)
    }

    /// [`Sim::dial`] by a peer of user `uid`. A Unix listen answers [`SIM_USER`] alone, as a
    /// real one answers the uid that made it (net.rs `peer_is`): a dial of another uid is
    /// refused at its accept, closed unread, and the listen is told nothing; the real accept
    /// loop goes on past it the same way (kqueue.rs, threads.rs: `ConnectionAborted`). A TCP
    /// peer has no uid to check.
    pub fn dial_as(&mut self, at: &Address, uid: u32) -> Result<Conn, IoError> {
        let Some(listen) = self.listening_at(at) else {
            // alloc: an error's message.
            return Err(IoError(format!("{at:?}: nothing listens there: connection refused")));
        };
        if !matches!(at, Address::Tcp(_)) && uid != SIM_USER {
            // alloc: an error's message.
            return Err(IoError(format!(
                "{at:?}: a connection of uid {uid} refused unread: this socket answers uid {SIM_USER} alone"
            )));
        }
        let conn = self.new_end(None);
        self.tell(listen, Done::Accepted { conn }, Of::Op);
        Ok(conn)
    }

    /// The peer outside sends `bytes` to the end `conn` of the simulation's.
    pub fn peer_send(&mut self, conn: Conn, bytes: &[u8]) {
        if let Some(end) = self.ends.get_mut(end_of(conn))
            && end.peer.is_none()
            && !end.peer_closed
        {
            end.inbox.extend_from_slice(bytes);
            self.wake();
        }
    }

    /// What the end `conn` sent to the peer outside since the last time the test asked.
    pub fn peer_received(&mut self, conn: Conn) -> Vec<u8> {
        self.ends.get_mut(end_of(conn)).map(|end| mem::take(&mut end.outbox)).unwrap_or_default()
    }

    /// Whether the end `conn` has hung up on the peer outside.
    pub fn peer_hung_up_on(&self, conn: Conn) -> bool {
        self.ends.get(end_of(conn)).is_none_or(|end| end.closed)
    }

    /// The peer outside hangs up: a receive of `conn` then gets nothing.
    pub fn peer_hangup(&mut self, conn: Conn) {
        let end = end_of(conn);
        if let Some(it) = self.ends.get_mut(end)
            && it.peer.is_none()
        {
            it.peer_closed = true;
            // Both sides hung up: nothing names it any more.
            if it.closed {
                self.ends.remove(end);
            }
            self.let_go_sends(conn);
            self.wake();
        }
    }

    /// Completions told for an op that had ended by the time they were due, and so detected
    /// as stale: changes of a watch whose end came first (dropped), and bytes a receive got
    /// on an end that hung up before they were told (told with nothing appended).
    pub fn stale(&self) -> u64 {
        self.stale
    }

    /// The machine loses power: every effect in flight is gone without a completion, every
    /// watch, process, listen and connection with it, and each file keeps what was synced plus
    /// a random prefix of the changes after (appends, writes at an offset, writes from memory,
    /// truncations) whole; of the changes after those, up to any of them, each [`SECTOR`] of
    /// the file on its own is the new bytes or the old (zeros past the old end), and the
    /// length is anywhere between ([`crate::world::disk::Torn`]): a later sector may survive
    /// an earlier one, as unsynced data on a real disk may. A write from memory in flight
    /// may have landed any of its sectors first. A whole-file write or rename that completed is
    /// kept whole; one in flight is not there. Each change of a name no sync of its directory
    /// covered ([`Sim::names`]: a file made, written whole or renamed without a sync, removed)
    /// may be undone, with every later change that shares a name with it (the name holds what
    /// it held before, as far as that was durable; a renamed file is back under the name it
    /// had), and an unsynced write's bytes are torn by [`SECTOR`]s as any change not synced is.
    /// What a barrier covers is kept whole by any change kept of a later epoch, of a file's
    /// bytes or of a name ([`keep_ordered`]).
    ///
    /// Every lock goes with the processes that held them, another process's too
    /// ([`Sim::hold_lock`]), and a disk filled up ([`Sim::fill`]) has room again.
    pub fn crash(&mut self) {
        // A mapping for writing let go wrote the page cache: its bytes are unsynced writes the
        // cut tears. One still held is taken to have landed none (one of the outcomes its
        // contract allows).
        self.fold_drawn();
        self.drawn.clear();
        let cut = self.crash_cut();
        self.crash_with(cut);
    }

    /// The simulation's processes died and the machine stayed up: every lock they held goes,
    /// as `flock`'s do at a process's exit, and nothing else changes (the page cache keeps every
    /// write; another process's locks, [`Sim::hold_lock`], stay). For a test that cuts a process
    /// (blaze-cas phase_commit_tests.rs) and then has a new one commit, which takes the lock.
    pub fn processes_died(&mut self) {
        self.locks.clear();
    }

    /// The first half of a crash: everything in flight and everything a process holds is
    /// gone, and what each file keeps is drawn, the files in the order of their names.
    /// [`Sim::crash_with`] makes the disk of it. The two are apart so that the disk world's
    /// refinement test (`sim/world.rs`) can ask the world whether it allows the cut drawn.
    fn crash_cut(&mut self) -> Cuts {
        seeds::crashed(self);
        // A write from memory in flight may have landed any of its sectors, unsynced: the cut
        // below keeps or tears them as it does every change since the last sync.
        // alloc: a crash, not an op.
        let mut flying = Vec::new();
        self.pending.drain(|_, pending| flying.push(pending.effect));
        for effect in flying {
            if let Effect::WriteFrom { file, at, from, cache, .. } = effect {
                self.land_some(&file, at, &from, cache);
            }
        }
        self.held.clear();
        self.held_reads.clear();
        // The process ended: every descriptor it kept is closed.
        self.descriptors.crash();
        self.hold_reads = None;
        self.fail_reads = None;
        self.short_reads = None;
        self.held_sends.clear();
        self.hold_syncs = None;
        self.fail_syncs = None;
        self.room = None;
        self.locks.clear();
        self.unlinked.clear();
        self.outside.clear();
        self.hold_lists = None;
        self.held_lists.clear();
        self.fail_lists = None;
        self.slow = None;
        self.stall_holders = None;
        self.spawns.clear();
        self.watches.clear();
        self.ending = 0;
        self.pressures.clear();
        self.told.clear();
        self.listens.clear();
        self.ends.clear();
        self.receiving.clear();
        self.host_crash();
        self.crash_wakes();
        self.stamp_epochs();
        // By name, so the seed's numbers go to the same files whatever the table's order.
        let mut files: Vec<(&FileName, &SimFile)> = Vec::with_capacity_in(self.files.len(), Global);
        files.extend(self.files.iter());
        files.sort_unstable_by(|a, b| a.0.cmp(b.0));
        // What each file keeps of its changes not synced, drawn file by file: how many changes
        // whole, then (only where some are not) how many more may have landed in part. A file
        // with nothing unsynced draws one number, as before tears by sectors.
        let mut drawn: Vec<(usize, usize)> = Vec::with_capacity_in(files.len(), Global);
        for (_, file) in &files {
            let all = file.ops.len();
            let kept = self.rng.below(all as u64 + 1) as usize;
            let upto = match kept < all {
                true => kept + self.rng.below((all - kept) as u64 + 1) as usize,
                false => kept,
            };
            drawn.push((kept, upto));
        }
        // Which changes of names not synced are undone: any of them, each later one that
        // shares a name with one undone undone with it (a name's changes are kept in order).
        let mut undone: Vec<bool> = Vec::with_capacity_in(self.names.len(), Global);
        for (i, change) in self.names.iter().enumerate() {
            let forced = (0..i).any(|earlier| undone[earlier] && self.names[earlier].named.shares(&change.named));
            undone.push(forced || self.rng.one_in(3));
        }
        // A barrier's promise: a change made after it completed, of a file's bytes or of a
        // name, is on the disk only if what it covers is.
        let names = self.names.iter().zip(&undone).filter(|(_, undone)| !**undone).map(|(change, _)| change.epoch).max();
        let mut ordered: Vec<(&[u64], &[(usize, u64)])> = Vec::with_capacity_in(files.len(), Global);
        ordered.extend(files.iter().map(|(_, file)| (&file.stamps[..], &file.barriers[..])));
        keep_ordered(&ordered, names, self.planted(host::Plant::BarriersNotKept), &mut drawn);
        let mut cuts: Vec<Cut> = Vec::with_capacity_in(files.len(), Global);
        for ((name, _), (kept, upto)) in files.iter().zip(&drawn) {
            // clone: a crash, not an op: the name the cut is of.
            cuts.push(Cut { name: (*name).share(), kept: *kept, upto: *upto, torn: crate::world::disk::Torn::Nothing });
        }
        // The tears, drawn last, file by file in the order of the names: of the changes after
        // those kept whole, up to `upto`, the file's length between the two files they leave
        // (the longer, the shorter or any between, one in three each), then whether every sector under it landed (a clean cut, one in three), else each
        // sector on its own.
        for ((_, file), cut) in files.iter().zip(&mut cuts) {
            if cut.upto <= cut.kept {
                continue;
            }
            let old = file.len_after(cut.kept);
            let new = file.len_after(cut.upto);
            let (low, high) = (old.min(new), old.max(new));
            // The ends weighed as heavily as everything between: a file whose length grew
            // whole (its end there, its middle any) is the case a reader must check for.
            let len = match self.rng.below(3) {
                0 => high,
                1 => low,
                _ => low + self.rng.below(high - low + 1),
            } as usize;
            let count = crate::world::disk::sectors(len);
            // alloc: a crash, not an op: one bit for each sector of the file kept.
            let mut landed: Vec<bool> = Vec::with_capacity_in(count, Global);
            match self.rng.one_in(3) {
                true => landed.resize(count, true),
                false => landed.extend((0..count).map(|_| self.rng.one_in(2))),
            }
            cut.torn = crate::world::disk::Torn::Sectors { upto: cut.upto, len, landed };
        }
        Cuts { files: cuts, undone }
    }

    /// The second half of a crash: the disk a power cut that made `cuts` leaves. Each file
    /// keeps what its cut says, all of it durable; then each change of a name undone is
    /// undone, the newest first, and what is left of the names is durable.
    fn crash_with(&mut self, cuts: Cuts) {
        // The machine comes back as another boot.
        self.boot = self.draw_boot();
        for Cut { name, kept, torn, .. } in cuts.files {
            let file = self.files.get_mut(&name).expect("a cut is of the files there are");
            match torn {
                crate::world::disk::Torn::Nothing => file.sync(kept),
                crate::world::disk::Torn::Sectors { upto, len, landed } => {
                    let new = file.after(upto);
                    file.sync(kept);
                    // The world's own function: what the refinement test checks is what runs.
                    file.durable = crate::world::disk::mixed(&file.durable, &new, len, &landed);
                }
            }
            file.ops.clear();
            file.back = 0;
            file.stamps.clear();
            file.barriers.clear();
        }
        let keep_barriers = self.planted(host::Plant::PutBackWithBarriers);
        let mut names = mem::take(&mut self.names);
        while let Some(NameChange { named, .. }) = names.pop() {
            if !cuts.undone.get(names.len()).copied().unwrap_or(false) {
                continue;
            }
            // Every later change that shares a name with this one is undone already, so the
            // names it puts back are free.
            match named {
                Named::Put { file, was } => {
                    self.files.remove(&file);
                    if let Some(was) = was {
                        self.files.insert(file, was.durable_only(keep_barriers));
                    }
                }
                Named::Gone { file, was } => {
                    self.files.insert(file, was.durable_only(keep_barriers));
                }
                Named::Moved { from, to, was } => {
                    if let Some(moved) = self.files.remove(&to) {
                        self.files.insert(from, moved);
                    }
                    if let Some(was) = was {
                        self.files.insert(to, was.durable_only(keep_barriers));
                    }
                }
                Named::MadeDir { dir } => self.undo_made_dir(&dir),
                Named::RemovedDir { dir } => self.undo_removed_dir(dir),
            }
        }
    }

    /// A sync of the directory `dir` (`""`, the root) completed: every change of a name in it
    /// is durable, and every earlier change that shares a name with one of those (undoing it
    /// would undo the later one), except a directory's change under which the later one is: a
    /// directory's sync makes the names in it durable, never its own ([`Named::kept_by`]).
    fn sync_names(&mut self, dir: &str) {
        let mut synced: Vec<bool> = Vec::with_capacity_in(self.names.len(), Global);
        synced.extend(self.names.iter().map(|change| change.named.dir() == dir));
        for i in (0..self.names.len()).rev() {
            if synced[i] {
                for earlier in 0..i {
                    if self.names[earlier].named.kept_by(&self.names[i].named) {
                        synced[earlier] = true;
                    }
                }
            }
        }
        let mut at = 0;
        self.names.retain(|_| {
            at += 1;
            !synced[at - 1]
        });
    }

    /// The change of a name `named` made, in the epoch that is now: durable once its directory
    /// is synced.
    fn named(&mut self, named: Named) {
        let epoch = self.epoch;
        self.names.push(NameChange { named, epoch });
    }

    /// `file` made where no file is: a change of its name, after the making of each directory
    /// above it that is not there ([`Sim::make_parents`]), as a driver makes them. How many
    /// directories were made.
    fn made(&mut self, file: &FileName) -> usize {
        if self.files.contains_key(file) {
            return 0;
        }
        let dirs = self.make_parents(file, host::Maker::Effect);
        // clone: the log's own name of the file, a refcount.
        self.named(Named::Put { file: file.share(), was: None });
        dirs
    }

    /// A write-back ([`Effect::WriteBack`]) of a file under `dir` is held and released as a
    /// sync is, and failed by [`Sim::fail_syncs`] as one is: both are the device's.
    ///
    /// From now on, a sync of any file under `dir` is taken and never completes, until a crash:
    /// what the sync covers stays unsynced. For a test of a store whose commit hangs.
    pub fn hold_syncs(&mut self, dir: FileName) {
        self.hold_syncs = Some(dir);
    }

    /// From now on, each send on the simulation's end `conn` is taken and stays in flight,
    /// never done, until that end hangs up, when it fails: a peer that stopped reading with
    /// its socket's buffer full. For a test of what a send that never ends holds up.
    pub fn hold_sends(&mut self, conn: Conn) {
        if let Some(end) = self.ends.get_mut(end_of(conn)) {
            end.holds_sends = true;
        }
    }

    /// The sends held so far and still in flight ([`Sim::hold_sends`]).
    pub fn held_sends(&self) -> usize {
        self.held_sends.len()
    }

    /// From now on, a list of `dir` is taken and waits, not in flight, until
    /// [`Sim::release_lists`]: a slow walk of a large directory, for a test of what may happen
    /// while it runs.
    pub fn hold_lists(&mut self, dir: FileName) {
        self.hold_lists = Some(dir);
    }

    /// The lists held so far ([`Sim::hold_lists`]).
    pub fn lists_held(&self) -> usize {
        self.held_lists.len()
    }

    /// Each held list answered with the directory as it is now, and lists no longer held.
    pub fn release_lists(&mut self) {
        self.hold_lists = None;
        for (dir, tag) in mem::take(&mut self.held_lists) {
            let done = self.list(&dir);
            self.tell(tag, done, Of::Op);
        }
    }

    /// The next `times` lists of `dir` fail, as a directory the user may not read does, until a
    /// crash. For a test of a look that is asked again, and of one that never succeeds.
    pub fn fail_lists(&mut self, dir: FileName, times: u32) {
        self.fail_lists = Some((dir, times));
    }

    /// From now on, a sync of any file under `dir` fails, until a crash: what it covers stays
    /// unsynced. For a test of a store that breaks.
    pub fn fail_syncs(&mut self, dir: FileName) {
        self.fail_syncs = Some(dir);
    }

    /// From now on, appends, whole-file writes and writes at an offset of files under `dir`
    /// take their bytes from `room`, until a crash: a disk filling up. One that does not fit
    /// fails with ENOSPC, naming the file: an append keeps the part that fits, unsynced (a
    /// short write), as does a write at an offset, and a whole-file write leaves the file as it
    /// was. For a test of a store or a journal on a full disk.
    pub fn fill(&mut self, dir: FileName, room: u64) {
        self.room = Some((dir, room));
    }

    /// From now on, a read into memory ([`Effect::ReadInto`]) of a file under `dir` is taken,
    /// its span scribbled, and not done until [`Sim::release_reads`] or a crash: a load the
    /// test keeps in flight while other actors step (a pool's page `Loading` while a reader and
    /// the evictor look at it). As held syncs are, a held read is not counted in flight.
    pub fn hold_reads(&mut self, dir: FileName) {
        self.hold_reads = Some(dir);
    }

    /// The reads held so far ([`Sim::hold_reads`]).
    pub fn reads_held(&self) -> usize {
        self.held_reads.len()
    }

    /// Lets the reads held so far go, each done `after` from now against the file as it is
    /// then, and holds no more.
    pub fn release_reads(&mut self, after: Duration) {
        self.hold_reads = None;
        for effect in mem::take(&mut self.held_reads) {
            self.pend(after, effect, 0);
        }
    }

    /// From now on, once in `one_in` (at least 1), a read into memory of a file under `dir`
    /// fails with EIO, its span left scribbled, until a crash: a drive that cannot read a
    /// block. For a test that a failed load is an error for its reader and leaves nothing kept.
    pub fn fail_reads(&mut self, dir: FileName, one_in: u32) {
        self.fail_reads = Some((dir, one_in.max(1)));
    }

    /// From now on, once in `one_in` (at least 1), a read into memory of a file under `dir`
    /// ends short of what the file holds, a drawn part of it read and the rest of its span left
    /// scribbled, until a crash: a file cut by something other than blaze, as its reader sees
    /// it. For a test that a short read inside what was committed is refused as corrupt.
    pub fn short_reads(&mut self, dir: FileName, one_in: u32) {
        self.short_reads = Some((dir, one_in.max(1)));
    }

    /// `bytes` at `at` of `file` changed on the disk, durably and silently, by nothing the
    /// simulation runs: a block that rotted, or a file another program wrote. No watch hears of
    /// it, and what was not synced stays on top of it. A file not there is left so.
    pub fn rot(&mut self, file: &FileName, at: u64, bytes: &[u8]) {
        if let Some(sim) = self.files.get_mut(file) {
            Op::At(at, bytes.to_vec()).apply(&mut sim.durable); // alloc: the world changed by the test, not an op.
        }
    }

    /// Another process holds `file`'s lock in `mode`, until [`Sim::release_lock`] or a crash:
    /// a lock asked of it that `mode` excludes is [`Done::Contended`]. Whatever the simulation's
    /// own locks of it are, as a test plays another process that got there first.
    pub fn hold_lock(&mut self, file: &FileName, mode: LockMode) {
        // clone: the world changed by the test, not an op.
        self.outside.insert(file.share(), mode);
    }

    /// Another process lets go of `file`'s lock ([`Sim::hold_lock`]).
    pub fn release_lock(&mut self, file: &FileName) {
        self.outside.remove(file);
    }

    /// From now on, each write, truncate and sync of a file under `dir` completes `by` after it
    /// is submitted, until a crash: a slow disk that still moves. For a test that a store's
    /// commit taking long, step by step, is waited for.
    pub fn slow_writes(&mut self, dir: FileName, by: Duration) {
        self.slow = Some((dir, by));
    }

    /// Lets the syncs held so far go, each completing `after` from now, and holds no more: a
    /// disk that stopped and came back.
    pub fn release_held(&mut self, after: Duration) {
        self.hold_syncs = None;
        for (effect, covers) in mem::take(&mut self.held) {
            // What it covers is what it covered as it was held, never the writes after it.
            self.pend(after, effect, covers);
        }
    }

    /// From now on, records each completion of a write or a sync of a file under one of
    /// `dirs`, in the order it is told: the clock then, the file, and whether it was a sync.
    /// For a test that two completions of one tick came in an order it means to test.
    pub fn record(&mut self, dirs: Vec<FileName>) {
        self.recording = dirs;
    }

    /// What [`Sim::record`] recorded so far.
    pub fn recorded(&self) -> &[(Duration, FileName, bool)] {
        &self.recorded
    }

    /// The syncs held so far ([`Sim::hold_syncs`]).
    pub fn held_syncs(&self) -> usize {
        self.held.len()
    }

    /// From now on, once in `one_in` critical sections ([`Sim::critical_section`],
    /// [`Sim::hold_for`]), chosen by the seed, the holder stalls for a time drawn up to `most`
    /// on top of its work, until a crash or a call with `one_in` 0: a lock holder descheduled,
    /// paging or preempted by a signal handler while it holds the lock. For a test that a slow
    /// holder never holds up what must not wait on it (a turn loop), and that its waiters say
    /// who they waited behind.
    pub fn stall_holders(&mut self, one_in: u32, most: Duration) {
        self.stall_holders = (one_in > 0).then_some((one_in, most));
    }

    /// How long a lock holder's critical section of `work` takes, a stall drawn by
    /// [`Sim::stall_holders`] included, without moving the clock: for a test that steps other
    /// actors while the holder holds, and lets it go at the virtual time this returns past its
    /// start. Draws from the seed only while stalls are on, so a test that never turns them on
    /// sees the same numbers it did before.
    pub fn hold_for(&mut self, work: Duration) -> Duration {
        let Some((one_in, most)) = self.stall_holders else { return work };
        if self.rng.below(u64::from(one_in)) != 0 {
            return work;
        }
        let most = u64::try_from(most.as_nanos()).unwrap_or(u64::MAX);
        let stall = Duration::from_nanos(self.rng.below(most.saturating_add(1)));
        work.saturating_add(stall)
    }

    /// A lock holder's critical section of `work`, run now: the clock moves on by
    /// [`Sim::hold_for`]`(work)` (a stall included, once in a while), which it returns. The
    /// holder lets go after it, at the clock's new now.
    pub fn critical_section(&mut self, work: Duration) -> Duration {
        let took = self.hold_for(work);
        self.advance(took);
        took
    }

    /// The virtual clock moved on by `by`, with nothing done: time a test lets pass while
    /// nothing completes (a poll with nothing in flight never moves it). Whatever falls due
    /// by then is told at the next poll.
    pub fn advance(&mut self, by: Duration) {
        self.now = self.now.saturating_add(by);
        self.disk.at(self.wall_ns());
    }

    /// The wall clock now, in nanoseconds since the epoch: the seed's start
    /// ([`Sim::wall_start`]) plus the virtual clock, what [`Driver::wall_ns`] says and what the
    /// disk's change times are of.
    pub fn wall_ns(&self) -> u64 {
        u64::try_from(self.wall_now().as_nanos()).unwrap_or(u64::MAX)
    }

    /// What a file holds now, synced or not.
    pub fn contents(&self, file: &FileName) -> Vec<u8> {
        self.files.get(file).map(SimFile::contents).unwrap_or_default()
    }

    /// The file's permission bits, if it is there.
    pub fn mode(&self, file: &FileName) -> Option<u32> {
        self.files.get(file).map(|it| it.mode)
    }

    /// Whether the file is there.
    pub fn exists(&self, file: &FileName) -> bool {
        self.files.contains_key(file)
    }

    /// The file made to hold `bytes`, durably, by something outside the state machine (a
    /// person's editor): watches of it hear of it.
    pub fn change(&mut self, file: &FileName, bytes: &[u8]) {
        self.make_parents(file, host::Maker::Outside);
        // alloc: the world changed by the test, not an op.
        *file_mut(&mut self.files, file) = SimFile::whole(bytes.to_vec());
        self.outside_named(file);
        self.changed(file);
    }

    /// `file` grown or cut to `len` bytes (a hole past its end), by something outside the state
    /// machine: its stamp says `len` and watches of it hear of it, and nothing is allocated
    /// until something reads it. Not durable: a crash takes the change back. For a file larger
    /// than a test can hold (one past the 4 GiB a VFS file may be).
    pub fn grow(&mut self, file: &FileName, len: u64) {
        self.make_parents(file, host::Maker::Outside);
        file_mut(&mut self.files, file).push_op(Op::Cut(len));
        self.changed(file);
    }

    /// `file` made a symbolic link to `target`, durably, by something outside the state
    /// machine (a checkout): watches of it hear of it.
    pub fn symlink(&mut self, file: &FileName, target: &str) {
        self.make_parents(file, host::Maker::Outside);
        // alloc: the world changed by the test, not an op.
        *file_mut(&mut self.files, file) =
            SimFile { mode: 0o777, link: true, ..SimFile::whole(target.into()) };
        self.outside_named(file);
        self.changed(file);
    }

    /// `to` made with `from`'s contents, mode and blocks (its holes kept holes), durably: the
    /// simulation does not model shared blocks or a link's shared inode.
    fn copy(&mut self, from: &FileName, to: &FileName) {
        let SimFile { mode, link, .. } = self.files[from];
        // clone: the copy's own record of which sectors hold blocks
        let blocks = self.files[from].blocks.clone();
        let bytes = self.contents(from);
        self.made(to);
        *file_mut(&mut self.files, to) = SimFile { mode, link, blocks, ..SimFile::whole(bytes) };
        self.changed(to);
    }

    /// The bytes of `file` that hold blocks: its sectors a write covered, less what a cut
    /// dropped; a hole (a cut that grew it, a run no write covered) holds none. 0 for a file
    /// that is not there. For a test that a place keeps a file's holes holes.
    pub fn allocated(&self, file: &FileName) -> u64 {
        self.files.get(file).map_or(0, |it| it.blocks.len() as u64 * SECTOR as u64)
    }

    /// The data bytes copies made by the kernel copied, ever ([`Effect::CopyData`]): not in
    /// [`Sim::bytes_written`], as no byte of them passed through the asker.
    pub fn bytes_copied(&self) -> u64 {
        self.bytes_copied
    }

    /// Where the symbolic link `file` leads, followed as the system opens a file (`None` when
    /// `file` is no link): an error when it ends at a directory, outside the simulated root or
    /// in a loop. Only the last name is followed; a link among a path's directories is not
    /// modelled.
    fn followed(&self, file: &FileName) -> Result<Option<FileName>, IoError> {
        let led = self.led_to(file)?;
        if let Some(at) = &led
            && self.is_dir(at)
        {
            // alloc: an error's message.
            return Err(IoError(format!("{file}: Is a directory (os error 21)")));
        }
        Ok(led)
    }

    /// Where the symbolic link `file` leads, as [`Sim::followed`] says, a directory there
    /// too: what a stat that follows links ([`Effect::Stat`], `stat(2)`) describes.
    fn led_to(&self, file: &FileName) -> Result<Option<FileName>, IoError> {
        if !self.files.get(file).is_some_and(|it| it.link) {
            return Ok(None);
        }
        // clone: the name of the file a read ends at.
        let mut at = file.share();
        for _ in 0..40 {
            let Some(sim) = self.files.get(&at).filter(|it| it.link) else {
                return Ok(Some(at));
            };
            let target = String::from_utf8_lossy(&sim.contents()).into_owned();
            // alloc: the name a link leads to.
            let mut parts: Vec<&str> =
                at.as_str().rsplit_once('/').map_or(vec![], |(dir, _)| dir.split('/').collect());
            for part in target.split('/') {
                match part {
                    "" if parts.is_empty() && target.starts_with('/') => {
                        // alloc: an error's message.
                        return Err(IoError(format!("{file}: a link out of the simulated root")));
                    }
                    "" | "." => {}
                    ".." => {
                        if parts.pop().is_none() {
                            // alloc: an error's message.
                            return Err(IoError(format!(
                                "{file}: a link out of the simulated root"
                            )));
                        }
                    }
                    part => parts.push(part),
                }
            }
            let Some(next) = FileName::new(&parts.join("/")) else {
                // alloc: an error's message.
                return Err(IoError(format!("{file}: a link to the simulated root")));
            };
            at = next;
        }
        // alloc: an error's message.
        Err(IoError(format!("{file}: Too many levels of symbolic links (os error 40)")))
    }

    /// [`Effect::ReadLink`] of `file`.
    fn read_link(&self, file: &FileName) -> Done {
        match self.files.get(file) {
            Some(sim) if sim.link => Done::Read(sim.contents()),
            // alloc: an error's message.
            Some(_) => Done::Failed(IoError(format!("{file}: Invalid argument (os error 22)"))),
            // alloc: an error's message.
            None => {
                Done::Failed(IoError(format!("{file}: No such file or directory (os error 2)")))
            }
        }
    }

    /// The bytes `at..at + len` of the file, or fewer at its end: none of a file not there.
    /// A symbolic link is followed.
    fn read(&mut self, file: &FileName, at: u64, len: usize) -> Result<Vec<u8>, IoError> {
        let followed = self.followed(file)?;
        let file = followed.as_ref().unwrap_or(file);
        let Some(sim) = self.files.get(file) else {
            // alloc: none, an empty read.
            return Ok(Vec::new());
        };
        let bytes: &[u8] = match sim.ops.is_empty() {
            true => &sim.durable,
            false => {
                self.scratch.clear();
                self.scratch.extend_from_slice(&sim.durable);
                for op in &sim.ops {
                    op.apply(&mut self.scratch);
                }
                &self.scratch
            }
        };
        let at = usize::try_from(at).unwrap_or(usize::MAX).min(bytes.len());
        // alloc: the read's payload, which the completion carries.
        Ok(bytes[at..at.saturating_add(len).min(bytes.len())].to_vec())
    }

    /// The files under `dir` and the directories they imply, as a disk would list them. As for
    /// a stat, a directory is there while some file is under it; a name that is a file and has
    /// files under it is listed as the file.
    /// The root ([`crate::DirName::Root`]) is always there, and lists every name.
    /// The folders above the root, nearest first, each the name in it of the folder on the way
    /// down and its other files (names relative to it, which must not start with that name:
    /// that is the root's side). Read by [`Effect::Ancestors`] and [`Effect::Above`].
    pub fn set_above(&mut self, levels: Vec<SimAbove>) {
        for level in &levels {
            assert!(
                level.files.iter().all(|(name, _)| name.as_str().split('/').next() != Some(&*level.toward)),
                "a folder above the root holds a file under {:?}, its way down to the root",
                level.toward
            );
        }
        self.above = levels;
    }

    /// [`Effect::Above`]: `op` on `name` in the folders [`Sim::set_above`] gave.
    fn read_above(&self, op: crate::AboveOp, name: &crate::Above) -> Done {
        let missing = || {
            // alloc: an error's message.
            Done::Failed(IoError(format!("{name}: no such file or directory")))
        };
        let Some(level) = self.above.get(usize::from(name.up().get()) - 1) else { return missing() };
        let meta = |len: u64, dir: bool| Meta { len, dir, modified_ns: 0, changed_ns: 0, ino: 0, dev: 0 };
        let file = name.rest().and_then(|rest| level.files.iter().find(|(it, _)| it == rest));
        let is_dir = |dir: Option<&FileName>| match dir {
            None => true,
            Some(dir) => level.files.iter().any(|(it, _)| it != dir && it.is_under(dir)),
        };
        match op {
            crate::AboveOp::Stat => Done::Stat(match file {
                Some((_, bytes)) => Some(meta(bytes.len() as u64, false)),
                None if is_dir(name.rest()) => Some(meta(0, true)),
                None => None,
            }),
            crate::AboveOp::Read => match file {
                // clone: the read's payload.
                Some((_, bytes)) => Done::Read(bytes.clone()),
                None => missing(),
            },
            crate::AboveOp::List => {
                if file.is_some() || !is_dir(name.rest()) {
                    return missing();
                }
                let start = name.rest().map_or(0, |it| it.as_str().len() + 1);
                // alloc: the listing's payload.
                let mut names: Vec<(FileName, Meta)> = Vec::new();
                if name.rest().is_none() {
                    // The way down to the root is a folder in it.
                    names.push((FileName::new(&level.toward).expect("a folder's name"), meta(0, true)));
                }
                for (it, bytes) in &level.files {
                    if name.rest().is_some_and(|dir| !it.is_under(dir) || it == dir) {
                        continue;
                    }
                    let inner = &it.as_str()[start..];
                    let entry = match inner.split_once('/') {
                        Some((first, _)) => (FileName::new(first).expect("a part of a name"), meta(0, true)),
                        None => (FileName::new(inner).expect("a name"), meta(bytes.len() as u64, false)),
                    };
                    if !names.iter().any(|(had, _)| *had == entry.0) {
                        names.push(entry);
                    }
                }
                names.sort_unstable_by(|a, b| a.0.cmp(&b.0));
                Done::Listed(names)
            }
        }
    }

    fn list(&mut self, dir: &crate::DirName) -> Done {
        if dir.name().is_some_and(|name| self.files.contains_key(name)) {
            // alloc: an error's message.
            return Done::Failed(IoError(format!("{dir}: not a directory")));
        }
        let start = dir.start();
        // Each name with its length and whether it is a directory, then its meta.
        // alloc: the names gathered, freed when the listing is made.
        let mut names: Vec<(FileName, u64, bool)> = Vec::new();
        for (file, sim) in self.files.iter().filter(|(it, _)| dir.holds(it)) {
            for (at, _) in file.as_str()[start..].match_indices('/') {
                let inner = FileName::new(&file.as_str()[..start + at]).expect("a part of a name");
                names.push((inner, 0, true));
            }
            // clone: a name of the listing.
            names.push((file.share(), sim.len(), false));
        }
        // A directory made is listed as a file's directories are, with those it is under.
        for made in self.made_under(dir) {
            for (at, _) in made.as_str()[start..].match_indices('/') {
                let inner = FileName::new(&made.as_str()[..start + at]).expect("a part of a name");
                names.push((inner, 0, true));
            }
            // clone: a name of the listing.
            names.push((made.share(), 0, true));
        }
        // Every directory is in `host.made` (`sim/host.rs`): one not there fails, empty or not.
        if !dir.name().is_none_or(|name| self.is_dir(name)) {
            // alloc: an error's message.
            return Done::Failed(IoError(format!("{dir}: no such directory")));
        }
        names.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.2.cmp(&b.2)));
        names.dedup_by(|later, first| later.0 == first.0);
        // alloc: the listing, which the completion carries.
        let mut listed: Vec<(FileName, Meta)> = Vec::with_capacity(names.len());
        for (name, len, is_dir) in names {
            let meta = self.disk.meta(&name, len, is_dir);
            listed.push((name, meta));
        }
        Done::Listed(listed)
    }

    /// A stat's [`Meta`] of `file`, a file there or (`dir`) a directory.
    fn meta(&mut self, file: &FileName, dir: bool) -> Meta {
        let len = if dir { 0 } else { self.files.get(file).map_or(0, SimFile::len) };
        self.disk.meta(file, len, dir)
    }

    /// [`Effect::Private`] on the simulated disk, whose directories are made 0o700: the first,
    /// by name, of `dir` and what is under it that [`Sim::set_access`] took from the user or
    /// whose mode lets others write it.
    fn private(&self, dir: &FileName) -> Done {
        if let Some(&access) = self.access.get(dir) {
            // clone: the refusal names the directory
            return Done::Private(Some((dir.share(), access)));
        }
        // What a test set of a file comes before what its mode says.
        let given = self
            .access
            .iter()
            .filter(|(it, _)| it.is_under(dir))
            .map(|(it, &access)| (it, 0, access));
        let moded = self
            .files
            .iter()
            .filter(|(it, sim)| sim.mode & 0o002 != 0 && it.is_under(dir))
            .map(|(it, _)| (it, 1, Access::OthersWrite));
        let first = given.chain(moded).min_by_key(|&(file, rank, _)| (file, rank));
        // clone: the refusal names the file
        Done::Private(first.map(|(file, _, access)| (file.share(), access)))
    }

    /// The file removed by something outside the state machine.
    pub fn delete(&mut self, file: &FileName) {
        // A descriptor kept of it holds its inode, and reads the bytes it held.
        self.orphan_kept(file);
        self.files.remove(file);
        self.outside_named(file);
        self.unlink_locks(file);
        self.changed(file);
    }

    /// `file`'s name changed durably by something outside the state machine: no crash undoes
    /// a change of it made before.
    fn outside_named(&mut self, file: &FileName) {
        self.names.retain(|change| !change.named.touches(file));
    }

    pub fn rng(&mut self) -> &mut Rng {
        &mut self.rng
    }

    fn delay(&mut self) -> Duration {
        latency(&mut self.rng, &self.faults)
    }

    /// Tells the watches of `file` it changed, in their slab's order, which is the seed's as
    /// everything else is.
    /// Every change not stamped yet is of the epoch that is now: made since the last barrier
    /// completed. Stamped as a barrier completes and at a crash, so no site that makes a
    /// change has to.
    fn stamp_epochs(&mut self) {
        let epoch = self.epoch;
        for file in self.files.values_mut() {
            let missing = file.ops.len().saturating_sub(file.stamps.len());
            file.stamps.extend(std::iter::repeat_n(epoch, missing));
        }
    }

    /// A sync that waits for the drive completed ([`Effect::Sync`], [`Effect::SyncDir`], a
    /// whole-file [`Effect::Write`], an [`Effect::Rename`]): the drive's cache was flushed,
    /// and what every barrier before it covers went with it. Durable now.
    fn flushed(&mut self) {
        for file in self.files.values_mut() {
            file.flushed();
        }
    }

    fn changed(&mut self, file: &FileName) {
        self.disk.touched(file, journal::Tree { files: &self.files, dirs: &self.host.made });
        self.tell_journals();
        for (id, tag, watch) in self.watches.iter() {
            if watch.journal || !watch.dir.holds(file) {
                continue;
            }
            if watch.ending && !self.rng.one_in(RACE_ONE_IN) {
                continue;
            }
            let done = match self.rng.one_in(u64::from(self.faults.lose_change_one_in)) {
                // alloc: none, an empty list.
                true => Done::Changed { files: Vec::new(), lost: true },
                // alloc: the change's payload, the one name it tells.
                false => Done::Changed { files: vec![file.share()], lost: false },
            };
            let at = self.now + latency(&mut self.rng, &self.faults);
            self.told.push(Told { at, completion: Completion { tag, done }, of: Of::Change(id) });
        }
    }

    /// Tells each journal watch the change its journal just recorded, if under its directory;
    /// one in `lose_change_one_in` is told as dropped.
    fn tell_journals(&mut self) {
        for (id, tag, watch) in self.watches.iter() {
            if !watch.journal || watch.ending {
                continue;
            }
            let dropped = self.rng.one_in(u64::from(self.faults.lose_change_one_in));
            if let Some(batch) = self.disk.live(&watch.dir, dropped) {
                let at = self.now + latency(&mut self.rng, &self.faults);
                let at = after_told(&self.told, at, tag, tag);
                // alloc: the batch's payload.
                let done = Done::Journaled(Box::new(batch));
                self.told.push(Told {
                    at,
                    completion: Completion { tag, done },
                    of: Of::Change(id),
                });
            }
        }
    }

    /// Which system's journal of changes the simulation plays from now on.
    pub fn set_journal(&mut self, journal: Journal) {
        self.disk.kind = journal;
    }

    /// Which system's journal of changes the simulation plays: [`Journal::None`] keeps none
    /// across a restart, as Linux.
    pub fn journal(&self) -> Journal {
        self.disk.kind
    }

    /// The journal of changes made again, with a new ID: a cursor into the old one is
    /// [`crate::Lost::Reset`].
    pub fn reset_journal(&mut self) {
        self.disk.reset();
    }

    /// The journal of changes keeps nothing from before now: a cursor before now is
    /// [`crate::Lost::Truncated`].
    pub fn truncate_journal(&mut self) {
        self.disk.truncate();
    }

    /// The system loses the changes under `dir`, as FSEvents' `MustScanSubDirs` says: journal
    /// watches of it, and journals read from before now, are told so.
    pub fn lose_changes(&mut self, dir: &FileName, why: crate::Lost) {
        self.disk.lose(dir, why);
        self.tell_journals();
    }

    /// What `name` is on the simulated disk now.
    pub fn stamp(&mut self, name: &FileName) -> crate::Stamp {
        self.disk.stamp(name, journal::Tree { files: &self.files, dirs: &self.host.made })
    }

    /// The file's permission bits set by something outside the state machine: its change time
    /// moves, its bytes and mtime do not.
    pub fn chmod(&mut self, file: &FileName, mode: u32) {
        if let Some(sim) = self.files.get_mut(file) {
            sim.mode = mode;
            self.disk.attributes(file);
            self.tell_journals();
        }
    }

    /// The file or directory made another user's, or writable by others, by something outside
    /// the state machine, for [`Effect::Private`] to find; [`Access::Alone`] makes it the
    /// user's again.
    pub fn set_access(&mut self, file: &FileName, access: Access) {
        match access {
            Access::Alone => self.access.remove(file),
            // clone: the world changed by the test, not an op.
            access => self.access.insert(file.share(), access),
        };
    }

    /// `from` renamed over `to` by something outside the state machine.
    pub fn rename(&mut self, from: &FileName, to: &FileName) {
        // A descriptor kept of `to` holds the inode renamed over, which it reads as it was; one
        // of `from` holds the inode that moved, whose bytes are those it held.
        if self.files.contains_key(from) {
            self.orphan_kept(to);
            self.orphan_kept(from);
        }
        if let Some(sim) = self.files.remove(from) {
            self.make_parents(to, host::Maker::Outside);
            // clone: the world changed by the test, not an op.
            self.files.insert(to.share(), sim);
            self.outside_named(from);
            self.outside_named(to);
            self.disk.renamed(from, to);
            self.move_locks(from, to);
            self.changed(from);
            self.changed(to);
        }
    }

    /// Whether `tag` holds a lock, of a file there or of one gone.
    fn holds_lock(&self, tag: Tag) -> bool {
        self.unlinked.contains(&tag)
            || self.locks.values().any(|holders| holders.iter().any(|&(it, _)| it == tag))
    }

    /// [`Effect::Lock`] of `file` in `mode` under `tag`, decided as it completes.
    fn lock(&mut self, file: &FileName, mode: LockMode, tag: Tag) -> Done {
        if self.holds_lock(tag) {
            // alloc: an error's message.
            return Done::Failed(IoError(format!(
                "{file}: lock {} asked under a tag that holds a lock already",
                tag.0
            )));
        }
        // Opened to read and write, made if missing, whether or not the lock is taken, as a
        // real driver opens it.
        if !self.files.contains_key(file) {
            self.made(file);
            file_mut(&mut self.files, file);
            self.changed(file);
        }
        let holders = self.locks.get(file).map_or(&[][..], Vec::as_slice);
        let outside = self.outside.get(file).copied();
        let free = match mode {
            LockMode::Exclusive => holders.is_empty() && outside.is_none(),
            LockMode::Shared => {
                holders.iter().all(|&(_, it)| it == LockMode::Shared)
                    && outside != Some(LockMode::Exclusive)
            }
        };
        if !free {
            return Done::Contended;
        }
        match self.locks.get_mut(file) {
            Some(holders) => holders.push((tag, mode)),
            None => {
                // clone: once per file locked while no one held it, its key.
                // alloc: the file's holders, kept while any hold it.
                self.locks.insert(file.share(), vec![(tag, mode)]);
            }
        }
        Done::Locked
    }

    /// The lock `lock` took let go of: whether it held one.
    fn unlock(&mut self, lock: Tag) -> bool {
        if let Some(at) = self.unlinked.iter().position(|&it| it == lock) {
            self.unlinked.swap_remove(at);
            return true;
        }
        let Some(file) = self
            .locks
            .iter()
            .find(|(_, holders)| holders.iter().any(|&(it, _)| it == lock))
            // clone: the name of the file let go of, a refcount.
            .map(|(file, _)| file.share())
        else {
            return false;
        };
        let holders = self.locks.get_mut(&file).expect("found above");
        holders.retain(|&(it, _)| it != lock);
        if holders.is_empty() {
            self.locks.remove(&file);
        }
        true
    }

    /// The locks of `file` stay held but exclude no one: their file is removed or replaced, so
    /// the name, made again, is another file.
    fn unlink_locks(&mut self, file: &FileName) {
        if let Some(holders) = self.locks.remove(file) {
            self.unlinked.extend(holders.into_iter().map(|(tag, _)| tag));
        }
    }

    /// The locks of `from` follow their file to `to`, as a rename moves it; `to`'s own go with
    /// the file the rename replaced.
    fn move_locks(&mut self, from: &FileName, to: &FileName) {
        self.unlink_locks(to);
        if let Some(holders) = self.locks.remove(from) {
            // clone: the name the locked file has now, a refcount.
            self.locks.insert(to.share(), holders);
        }
    }

    /// The failure of `effect` where it opens for writing a file that is there whose mode
    /// gives its owner no write bit (a file sealed read-only, [`Effect::Seal`]), as the kernel
    /// refuses a user who is not root with `EACCES`: an append, a write at an offset or from
    /// memory, a cut. A sync, a barrier and a write-back open the file read-only and are done;
    /// a whole-file write puts a new file in its place by a rename, as a clone, a copy and a
    /// link make a new name, so none of them is refused. A symbolic link's own bits are not
    /// the file's, and this simulation follows no link for a write.
    fn refused_write(&self, effect: &Effect) -> Option<Done> {
        let file = match effect {
            Effect::Append { file, .. }
            | Effect::WriteAt { file, .. }
            | Effect::WriteFrom { file, .. }
            | Effect::Truncate { file, .. } => file,
            _ => return None,
        };
        let sim = self.files.get(file)?;
        if sim.link || sim.mode & OWNER_WRITES != 0 {
            return None;
        }
        // alloc: an error's message.
        Some(Done::Failed(IoError(format!("{file}: {PERMISSION_DENIED}"))))
    }

    /// `effect` on a disk filling up ([`Sim::fill`]): handed back to be done, its bytes taken
    /// from the room, when it fits; else what it ends with, an append or a write at an offset
    /// having kept the part that fit.
    fn take_room(&mut self, effect: Effect) -> Result<Effect, Done> {
        let Some((dir, room)) = &mut self.room else {
            return Ok(effect);
        };
        let (file, len) = match &effect {
            Effect::Append { file, bytes, .. }
            | Effect::Write { file, bytes, .. }
            | Effect::WriteUnsynced { file, bytes, .. } => (file, bytes.len() as u64),
            // One of a file not there fails as it would have, taking no room.
            Effect::WriteAt { file, bytes, .. } if self.files.contains_key(file) => {
                (file, bytes.len() as u64)
            }
            Effect::WriteFrom { file, from, .. } if self.files.contains_key(file) => {
                (file, u64::from(from.len))
            }
            _ => return Ok(effect),
        };
        if !file.is_under(dir) {
            return Ok(effect);
        }
        if len <= *room {
            *room -= len;
            return Ok(effect);
        }
        let fits = *room as usize;
        *room = 0;
        // alloc: an error's message.
        let full = Done::Failed(IoError(format!("{file}: No space left on device (os error 28)")));
        match effect {
            Effect::Append { file, mut bytes, .. } => {
                bytes.truncate(fits);
                self.made(&file);
                file_mut(&mut self.files, &file).push_op(Op::Append(bytes));
                if fits > 0 {
                    self.changed(&file);
                }
            }
            Effect::WriteAt { file, at, mut bytes, .. } if fits > 0 => {
                bytes.truncate(fits);
                let sim = self.files.get_mut(&file).expect("looked up above");
                sim.push_op(Op::At(at, bytes));
                self.changed(&file);
            }
            // The sectors that fit, as a drive out of room takes the first of a request's.
            Effect::WriteFrom { file, at, from, cache, .. } if fits >= SECTOR => {
                if let Ok(mut bytes) = Sim::bytes_of(&from, &file, at, cache) {
                    bytes.truncate(fits / SECTOR * SECTOR);
                    let sim = self.files.get_mut(&file).expect("looked up above");
                    sim.push_op(Op::Sectors(at, bytes));
                    self.changed(&file);
                }
            }
            // A whole-file write leaves the file as it was.
            _ => {}
        }
        Err(full)
    }

    /// `span` of the read or write `what` of `file` at `at` reached, or why not.
    fn resolve(
        span: &Span,
        file: &FileName,
        at: u64,
        cache: Cache,
        what: &str,
    ) -> Result<spans::Resolved, IoError> {
        spans::resolve(span, file, at, cache, what)
    }

    /// The span of a read into memory just submitted written over with [`SCRIBBLE`]: what the
    /// asker would see of it while the kernel writes there.
    fn scribble(effect: &Effect) {
        if let Effect::ReadInto { file, at, into, cache, .. } = effect
            && let Ok(span) = Sim::resolve(into, file, *at, *cache, "a read into memory of")
        {
            span.with_mut(|bytes| bytes.fill(SCRIBBLE));
        }
        if let Effect::ReadMany { parts, cache, .. } = effect {
            for part in parts {
                let what = "a batched read into memory of";
                if let Ok(span) = Sim::resolve(&part.into, &part.file, part.at, *cache, what) {
                    span.with_mut(|bytes| bytes.fill(SCRIBBLE));
                }
            }
        }
    }

    /// Whether `file` is under the directory of a fault drawn once in its count.
    fn drawn(rng: &mut Rng, fault: &Option<(FileName, u32)>, file: &FileName) -> bool {
        fault.as_ref().is_some_and(|(dir, one_in)| {
            file.is_under(dir) && rng.below(u64::from(*one_in)) == 0
        })
    }

    /// The descriptors kept of `file` and what is under it read the bytes each file holds now,
    /// from now on: another process unlinks it or renames over it.
    fn orphan_kept(&mut self, file: &FileName) {
        for name in self.descriptors.live_under(file) {
            let bytes = self.contents(&name);
            self.descriptors.orphan(&name, &bytes);
        }
    }

    /// [`Effect::ReadInto`] of `into` from `at` of `file`, done now through its kept
    /// descriptor: the file at the name, opened at its first read, or the bytes the file held
    /// when another process unlinked it.
    fn read_into(&mut self, file: &FileName, at: u64, into: &Span, cache: Cache) -> Done {
        let span = match Sim::resolve(into, file, at, cache, "a read into memory of") {
            Ok(span) => span,
            Err(why) => return Done::Failed(why),
        };
        let followed = match self.followed(file) {
            Ok(followed) => followed,
            Err(why) => return Done::Failed(why),
        };
        let asked = file;
        let file = followed.as_ref().unwrap_or(file);
        let orphan = match self.descriptors.read(asked, cache) {
            Some(orphan) => orphan,
            None => {
                if !self.files.contains_key(file) {
                    // alloc: an error's message.
                    return Done::Failed(IoError(format!(
                        "{file}: No such file or directory (os error 2)"
                    )));
                }
                // Opened and kept; the least recently read closes past the bound.
                self.descriptors.open(asked, cache);
                None
            }
        };
        if Sim::drawn(&mut self.rng, &self.fail_reads, file) {
            // alloc: an error's message.
            return Done::Failed(IoError(format!("{file}: Input/output error (os error 5)")));
        }
        let bytes = match orphan {
            Some(held) => {
                let from = usize::try_from(at).unwrap_or(usize::MAX).min(held.len());
                // alloc: the read's bytes, as `Sim::read` makes them.
                held[from..from.saturating_add(span.len()).min(held.len())].to_vec()
            }
            None => match self.read(file, at, span.len()) {
                Ok(bytes) => bytes,
                Err(why) => return Done::Failed(why),
            },
        };
        let mut len = bytes.len();
        if len > 0 && Sim::drawn(&mut self.rng, &self.short_reads, file) {
            len = self.rng.below(len as u64) as usize;
        }
        span.with_mut(|into| into[..len].copy_from_slice(&bytes[..len]));
        Done::ReadInto { len: len as u32 }
    }

    /// The bytes of `from` as they are now, for a write from memory to `file` at `at`.
    fn bytes_of(from: &Span, file: &FileName, at: u64, cache: Cache) -> Result<Vec<u8>, IoError> {
        let span = Sim::resolve(from, file, at, cache, "a write from memory to")?;
        // alloc: the write's bytes, taken from memory as the kernel would as it writes them.
        Ok(span.with(<[u8]>::to_vec))
    }

    /// Any of the sectors of a write from memory landed on `file`, unsynced: what one that
    /// failed, or was in flight at a crash, leaves.
    fn land_some(&mut self, file: &FileName, at: u64, from: &Span, cache: Cache) {
        let Ok(bytes) = Sim::bytes_of(from, file, at, cache) else { return };
        if !self.files.contains_key(file) {
            return;
        }
        let landed = some_sectors(at, &bytes, &mut self.rng);
        if landed.is_empty() {
            return;
        }
        let sim = self.files.get_mut(file).expect("looked up above");
        // Each landed sector waits for a sync as a write from memory does: a crash before
        // one keeps it whole or not at all (`Op::Sectors`), never a prefix of it, which an
        // `Op::At` cut short would leave.
        sim.ops.extend(landed.into_iter().map(|op| match op {
            Op::At(at, sector) => Op::Sectors(at, sector),
            op => op,
        }));
        self.changed(file);
    }

    fn fail(&mut self, tag: Tag) -> Completion {
        seeds::injected(self);
        // alloc: an injected failure's message.
        Completion { tag, done: Done::Failed(IoError(FAULT.into())) }
    }

    /// `done` of `tag`, ready after a delay, and after everything told of the same tag before.
    fn tell(&mut self, tag: Tag, done: Done, of: Of) {
        let at = self.now + self.delay();
        let at = after_told(&self.told, at, tag, tag);
        self.told.push(Told { at, completion: Completion { tag, done }, of });
    }

    /// `done` of `tag`, ready after a delay, and after everything told of `tag` or of `after`
    /// before: a watch's end or flush after the watch's changes.
    fn tell_after(&mut self, after: Tag, tag: Tag, done: Done, of: Of) {
        let at = self.now + self.delay();
        let at = after_told(&self.told, at, after, tag);
        self.told.push(Told { at, completion: Completion { tag, done }, of });
    }

    fn listening_at(&self, at: &Address) -> Option<Tag> {
        self.listens.iter().find(|(_, _, it)| *it == at).map(|(_, tag, _)| tag)
    }

    fn new_end(&mut self, peer: Option<EndId>) -> Conn {
        Conn(self.ends.insert(End { peer, ..End::default() }).to_bits())
    }

    /// The sends held on `conn` ([`Sim::hold_sends`]) end, failed: its end hung up.
    fn let_go_sends(&mut self, conn: Conn) {
        let mut at = 0;
        while at < self.held_sends.len() {
            if self.held_sends[at].0 != conn {
                at += 1;
                continue;
            }
            let (_, tag, bytes) = self.held_sends.remove(at);
            let done =
                Done::unsent(bytes, format_args!("{conn:?}: hung up with the send in flight"));
            self.tell(tag, done, Of::Op);
        }
    }

    /// Every waiting receive that can be answered now is: with what came, or with nothing once
    /// either end hung up.
    fn wake(&mut self) {
        let mut at = 0;
        while at < self.receiving.len() {
            let Receiving { end, most, .. } = self.receiving[at];
            let ready = self
                .ends
                .get(end)
                .is_none_or(|it| !it.inbox.is_empty() || it.closed || it.peer_closed);
            if !ready {
                at += 1;
                continue;
            }
            let Receiving { tag, end, most: _, mut into } = self.receiving.remove(at);
            let kept = into.len();
            if let Some(it) = self.ends.get_mut(end) {
                // All that came, or with `short-receive` on a drawn part (`sim/every_cut.rs`).
                let n = every_cut::short_receive(self.inject, &mut self.rng, most.min(RECV_MOST).min(it.inbox.len()));
                // alloc: none once the asker's buffer has room for what came.
                into.extend(it.inbox.drain(..n));
            }
            self.tell(tag, Done::Read(into), Of::Recv { end, kept });
        }
    }

    /// What a spawn of `command` ends with, decided now from its program.
    fn run(&mut self, command: &Command) -> Result<Ran, IoError> {
        let Some(program) = self.programs.get_mut(command.program.as_path()) else {
            // alloc: an error's message.
            return Err(IoError(format!(
                "starting {}: no such program in the simulation",
                command.program.display()
            )));
        };
        let mut ran = program(command, &mut self.rng);
        if let Some(limit) = command.memory
            && ran.exit.peak_memory > limit
        {
            // Killed as it passed its limit: by then it had run a part of its time.
            ran.takes = Duration::from_nanos(self.rng.below(ran.takes.as_nanos() as u64 + 1));
            ran.exit = Exit {
                code: None,
                signal: Some(KILLED),
                // alloc: none, empty output.
                stdout: Vec::new(),
                // alloc: none, empty output.
                stderr: Vec::new(),
                traced: None,
                peak_memory: limit,
            };
        }
        Ok(ran)
    }

    /// Every mapping for writing that its task has dropped, folded into its file: each
    /// [`SECTOR`] that differs from the file as an unsynced write from memory, in order.
    fn fold_drawn(&mut self) {
        let mut at = 0;
        while at < self.drawn.len() {
            let Some(bytes) = self.drawn[at].1.lock().take() else {
                at += 1;
                continue;
            };
            // In the order drawn: two mappings of one file fold as they were let go.
            let (file, _) = self.drawn.remove(at);
            let Some(sim) = self.files.get_mut(&file) else { continue };
            let was = sim.contents();
            for (index, sector) in bytes.chunks(SECTOR).enumerate() {
                let from = index * SECTOR;
                if was.get(from..from + sector.len()) != Some(sector) {
                    // alloc: the sector's bytes, as a write from memory carries them.
                    sim.push_op(Op::Sectors(from as u64, sector.to_vec()));
                    self.bytes_written += sector.len() as u64;
                }
            }
            self.changed(&file);
        }
    }

    fn complete(&mut self, pending: Pending) -> Completion {
        self.fold_drawn();
        let Pending { effect, covers, spawn } = pending;
        let tag = effect.tag();
        // Before any fault: a torn append never lands in a file the kernel would not open.
        if let Some(done) = self.refused_write(&effect) {
            return Completion { tag, done };
        }
        // A file's name made under a file, or where a directory is: refused by the kernel
        // before anything is opened, so before any fault too.
        if let Some(done) = self.cannot_make(&effect) {
            return Completion { tag, done };
        }
        // A batched read's faults are drawn a part at a time, each failing its part alone.
        if !matches!(effect, Effect::Timer { .. } | Effect::ReadMany { .. })
            && self.rng.one_in(u64::from(self.faults.fail_one_in))
        {
            if let Some(spawn) = spawn {
                self.spawns.remove_id(spawn);
            }
            if let Effect::Append { file, mut bytes, .. } = effect {
                let torn = self.rng.below(bytes.len() as u64) as usize;
                bytes.truncate(torn);
                // Opened, and so made, before the write failed.
                self.made(&file);
                file_mut(&mut self.files, &file).push_op(Op::Append(bytes));
                if torn > 0 {
                    self.changed(&file);
                }
            } else if let Effect::WriteFrom { file, at, from, cache, .. } = effect {
                self.land_some(&file, at, &from, cache);
            }
            return self.fail(tag);
        }
        let effect = match self.take_room(effect) {
            Ok(effect) => effect,
            Err(done) => return Completion { tag, done },
        };
        let changed = crate::threads::changes(&effect);
        let done = match effect {
            Effect::ReadAll { file, .. } => match self.read(&file, 0, usize::MAX) {
                Ok(bytes) => {
                    self.bytes_read += bytes.len() as u64;
                    Done::Read(bytes)
                }
                Err(why) => Done::Failed(why),
            },
            Effect::Read { file, at, len, .. } => match self.read(&file, at, len) {
                Ok(mut bytes) => {
                    if bytes.len() > 1 && Sim::drawn(&mut self.rng, &self.short_reads, &file) {
                        let span = u64::try_from(bytes.len() - 1).unwrap_or(1);
                        let short = usize::try_from(self.rng.below(span)).unwrap_or(0) + 1;
                        bytes.truncate(short);
                    }
                    self.bytes_read += bytes.len() as u64;
                    Done::Read(bytes)
                }
                Err(why) => Done::Failed(why),
            },
            Effect::ReadLink { file, .. } => self.read_link(&file),
            Effect::ReadInto { file, at, into, cache, .. } => {
                let done = self.read_into(&file, at, &into, cache);
                if let Done::ReadInto { len } = &done {
                    self.bytes_read += u64::from(*len);
                }
                done
            }
            Effect::ReadMany { mut parts, cache, .. } => {
                for part in &mut parts {
                    part.got = match self.rng.one_in(u64::from(self.faults.fail_one_in)) {
                        // alloc: an error's message.
                        true => crate::Got::Failed(IoError(format!(
                            "{} at {} ({} bytes): {}",
                            part.file, part.at, part.into.len, FAULT
                        ))),
                        false => match self.read_into(&part.file, part.at, &part.into, cache) {
                            Done::ReadInto { len } => {
                                self.bytes_read += u64::from(len);
                                crate::Got::Read(len)
                            }
                            // alloc: an error's message, naming the part's offset.
                            Done::Failed(why) => crate::Got::Failed(IoError(format!(
                                "{} at {} ({} bytes): {}",
                                part.file, part.at, part.into.len, why.0
                            ))),
                            other => unreachable!("a read into memory ends as one: {other:?}"),
                        },
                    };
                }
                Done::ReadMany(parts)
            }
            Effect::WriteFrom { file, at, from, cache, .. } => {
                match Sim::bytes_of(&from, &file, at, cache) {
                    Err(why) => Done::Failed(why),
                    Ok(bytes) => match self.files.get_mut(&file) {
                        Some(sim) => {
                            self.bytes_written += bytes.len() as u64;
                            sim.push_op(Op::Sectors(at, bytes));
                            self.changed(&file);
                            Done::Written
                        }
                        // alloc: an error's message.
                        None => Done::Failed(IoError(format!("{file}: no such file"))),
                    },
                }
            }
            Effect::Append { file, bytes, .. } => {
                self.bytes_written += bytes.len() as u64;
                self.made(&file);
                file_mut(&mut self.files, &file).push_op(Op::Append(bytes));
                self.changed(&file);
                Done::Appended
            }
            // The file's bytes made durable (`fsync`; `F_FULLFSYNC` on Apple's systems), never
            // its name: that is its directory's sync.
            Effect::Sync { file, .. } => {
                let less = self.planted(host::Plant::SyncCoversLess);
                // The planted bug: the loose rule, a file's sync keeping its name too.
                if self.planted(host::Plant::FileSyncKeepsName) {
                    self.names.retain(|change| !change.named.touches(&file));
                }
                self.made(&file);
                let sim = self.files.entry(file).or_default();
                // The planted bug: one change fewer made durable than the sync covered.
                let covers = match less && covers.min(sim.ops.len()) > 0 {
                    true => covers.min(sim.ops.len()) - 1,
                    false => covers,
                };
                sim.sync(covers);
                self.flushed();
                Done::Synced
            }
            // Ordered, not durable: what it covers is written before whatever is written
            // from now on, so a crash that keeps any of that keeps this (`Sim::crash`), and a
            // sync of anything that completes after this makes it durable (`Sim::flushed`).
            // It covers what was written when it was asked, as a sync does.
            Effect::Barrier { file, .. } => {
                self.stamp_epochs();
                let epoch = self.epoch;
                self.made(&file);
                let sim = self.files.entry(file).or_default();
                let covered = covers.min(sim.ops.len());
                if covered > 0 {
                    sim.barriers.push((covered, epoch));
                }
                // Written out, as a write-back's: the file is clean as far as it covers.
                sim.back = sim.back.max(covered);
                self.epoch += 1;
                Done::Ordered
            }
            // Written out, not durable: the file is clean, and a crash tears it as before.
            // It covers what was written when it was asked, as a sync does.
            Effect::WriteBack { file, .. } => match self.files.get_mut(&file) {
                Some(sim) => {
                    sim.back = sim.back.max(covers.min(sim.ops.len()));
                    Done::WrittenBack
                }
                // alloc: an error's message.
                None => Done::Failed(IoError(format!("{file}: no such file"))),
            },
            // The directory's names made durable (`fsync` of the directory; `F_FULLFSYNC` on
            // Apple's systems, which also flushes the drive's cache).
            Effect::SyncDir { dir, .. } => {
                self.sync_names(dir.name().map_or("", FileName::as_str));
                self.flushed();
                Done::Synced
            }
            Effect::Truncate { file, len, .. } => {
                self.made(&file);
                file_mut(&mut self.files, &file).push_op(Op::Cut(len));
                self.changed(&file);
                Done::Truncated
            }
            // Cut to nothing and grown to `len` zeros (a hole), unsynced; the bytes are
            // drawn into a buffer the task writes and folded in once it lets go.
            Effect::MapWrite { file, len, .. } => {
                self.made(&file);
                let sim = file_mut(&mut self.files, &file);
                sim.push_op(Op::Cut(0));
                sim.push_op(Op::Cut(len));
                self.changed(&file);
                // alloc: the slot the bytes come back through.
                let drawn: Drawn = std::sync::Arc::new(crate::lock::Lock::new("sim.drawn", None));
                // clone: the simulator's hold on the slot, to fold the bytes in later.
                self.drawn.push((file, drawn.clone()));
                // alloc: the mapping's bytes, which the task writes.
                Done::MappedWrite(crate::MappingWrite::sim(vec![0; len as usize], drawn))
            }
            // Made beside it and renamed over it: a lock of the file it replaced is of that one.
            // Its directory synced after the rename (`fs.rs`'s `write_atomically`): the new
            // name is durable, with every change of a name in that directory.
            // The directories it is made in are made too, durably, as every effect makes
            // them ([`Sim::make_parents`]): the write is durable from the root down.
            Effect::Write { file, bytes, .. } => {
                self.bytes_written += bytes.len() as u64;
                self.make_parents(&file, host::Maker::Effect);
                *file_mut(&mut self.files, &file) = SimFile::whole(bytes);
                self.sync_names(parent_of(&file));
                self.flushed();
                self.unlink_locks(&file);
                self.changed(&file);
                Done::Written
            }
            // As a write, but nothing of it is on the disk: its bytes are a change not synced,
            // torn by sectors at a crash, and the name may go back to what it held.
            Effect::WriteUnsynced { file, bytes, .. } => {
                self.bytes_written += bytes.len() as u64;
                let was = self.files.remove(&file);
                let new = SimFile {
                    // alloc: none, an empty vector.
                    durable: Vec::new(),
                    // alloc: the file's one change not synced.
                    ops: vec![Op::Sectors(0, bytes)],
                    mode: SimFile::default().mode,
                    link: false,
                    back: 0,
                    // alloc: none, two empty vectors.
                    stamps: Vec::new(),
                    barriers: Vec::new(),
                    // alloc: none, an empty set, filled below.
                    blocks: std::collections::BTreeSet::new(),
                };
                let mut new = new;
                new.allocate(0, new.len());
                // Its directories made with it, durably as every effect makes them; the file's
                // own name and bytes are not synced.
                self.make_parents(&file, host::Maker::Effect);
                // clone: the log's own name of the file, a refcount.
                self.named(Named::Put { file: file.share(), was });
                *file_mut(&mut self.files, &file) = new;
                self.unlink_locks(&file);
                self.changed(&file);
                Done::Written
            }
            Effect::WriteAt { file, at, bytes, .. } => match self.files.get_mut(&file) {
                Some(sim) => {
                    self.bytes_written += bytes.len() as u64;
                    sim.push_op(Op::At(at, bytes));
                    self.changed(&file);
                    Done::Written
                }
                // alloc: an error's message.
                None => Done::Failed(IoError(format!("{file}: no such file"))),
            },
            // As a rename, but the new name is not on the disk: a crash may put the file back
            // under `from` and what `to` held back under `to`.
            Effect::RenameUnsynced { from, to, .. } => match self.files.remove(&from) {
                Some(sim) => {
                    // The new name's directories made with it, durably as every effect makes
                    // them; the new name itself is not synced.
                    self.make_parents(&to, host::Maker::Effect);
                    let was = self.files.remove(&to);
                    // clone: the log's own names of the files, refcounts.
                    self.named(Named::Moved { from: from.share(), to: to.share(), was });
                    *file_mut(&mut self.files, &to) = sim;
                    self.disk.renamed(&from, &to);
                    self.move_locks(&from, &to);
                    self.changed(&from);
                    self.changed(&to);
                    Done::Renamed
                }
                // alloc: an error's message.
                None => Done::Failed(IoError(format!("{from}: no such file"))),
            },
            Effect::Rename { from, to, .. } => match self.files.remove(&from) {
                Some(mut sim) => {
                    self.flushed();
                    // The file being renamed is out of the table here, and the flush is of
                    // the whole drive: what its own barriers cover went too.
                    if !self.planted(host::Plant::RenameFlushesOthers) {
                        sim.flushed();
                    }
                    self.make_parents(&to, host::Maker::Effect);
                    *file_mut(&mut self.files, &to) = sim;
                    // Both directories synced after it (`fs.rs`'s `rename`, the ring's chain):
                    // the new name is durable, and the old one's going; the directories it made
                    // for the new name are durable as every effect makes them.
                    self.sync_names(parent_of(&to));
                    if parent_of(&from) != parent_of(&to) {
                        self.sync_names(parent_of(&from));
                    }
                    self.disk.renamed(&from, &to);
                    self.move_locks(&from, &to);
                    self.changed(&from);
                    self.changed(&to);
                    Done::Renamed
                }
                // alloc: an error's message.
                None => Done::Failed(IoError(format!("{from}: no such file"))),
            },
            Effect::Seal { file, .. } if self.files.get(&file).is_some_and(|it| it.link) => {
                // alloc: an error's message.
                Done::Failed(IoError(format!("{file} {}", crate::SEAL_THROUGH_A_LINK)))
            }
            Effect::Mode { file, mode, .. } | Effect::Seal { file, mode, .. } => {
                match self.files.get_mut(&file) {
                    Some(sim) => {
                        sim.mode = mode;
                        self.disk.attributes(&file);
                        self.tell_journals();
                        Done::Moded
                    }
                    // alloc: an error's message.
                    None => Done::Failed(IoError(format!("{file}: no such file"))),
                }
            }
            Effect::Times { file, modified_ns, .. } => {
                match self.disk.times(&file, modified_ns, journal::Tree { files: &self.files, dirs: &self.host.made }) {
                    true => {
                        self.tell_journals();
                        Done::Timed
                    }
                    // alloc: an error's message.
                    false => Done::Failed(IoError(format!("{file}: no such file or directory"))),
                }
            }
            // A hint to the page cache, which the simulation has none of: the file to read
            // ahead in is there (one it is like that is not holds nothing), and nothing of
            // either changes. What the order of it is to the rename
            // that follows is the asker's to hold (`blaze_cas::publish::Writing`).
            Effect::Prewarm { like, file, .. } => {
                let _ = like;
                match self.files.contains_key(&file) {
                    true => Done::Prewarmed { bytes: 0 },
                    // alloc: an error's message.
                    false => Done::Failed(IoError(format!("{file}: no such file"))),
                }
            }
            Effect::CopyData { from, .. } if !self.files.contains_key(&from) => {
                // alloc: an error's message.
                Done::Failed(IoError(format!("{from}: no such file")))
            }
            Effect::CopyData { from, .. } if self.files[&from].link => {
                // alloc: an error's message.
                Done::Failed(IoError(format!("{from} {}", crate::CLONE_THROUGH_A_LINK)))
            }
            Effect::CopyData { to, .. } if self.files.contains_key(&to) => {
                // alloc: an error's message.
                Done::Failed(IoError(format!("{to}: already there")))
            }
            // The kernel's copy of each extent of data: read from what the source holds, its
            // dirty pages included, so nothing is forced out (a clone forces them); its holes
            // stay holes, and the bytes copied are its data's, not its length.
            Effect::CopyData { from, to, .. } => {
                let source = &self.files[&from];
                let (len, sector) = (source.len(), SECTOR as u64);
                let bytes: u64 =
                    source.blocks.iter().map(|it| len.saturating_sub(it * sector).min(sector)).sum();
                self.bytes_copied += bytes;
                self.copy(&from, &to);
                // A new file's mode whatever its source's, as a clone's: `Effect::CopyData`.
                file_mut(&mut self.files, &to).mode = SimFile::default().mode;
                Done::Copied { bytes }
            }
            Effect::Clone { from, to, .. } | Effect::Link { from, to, .. }
                if !self.files.contains_key(&from) =>
            {
                let _ = to;
                // alloc: an error's message.
                Done::Failed(IoError(format!("{from}: no such file")))
            }
            Effect::Clone { from, .. } if self.files[&from].link => {
                // alloc: an error's message.
                Done::Failed(IoError(format!("{from} {}", crate::CLONE_THROUGH_A_LINK)))
            }
            Effect::Clone { .. } if self.faults.no_clone => Done::Failed(IoError(
                // alloc: an error's message.
                "the simulated file system cannot share blocks; blaze does not copy instead".into(),
            )),
            Effect::Clone { to, .. } | Effect::Link { to, .. } if self.files.contains_key(&to) => {
                // alloc: an error's message.
                Done::Failed(IoError(format!("{to}: already there")))
            }
            Effect::Clone { from, to, .. } => {
                // The file system writes the source's dirty data out before it shares the
                // blocks: charged to the clone, and the source is clean after.
                if let Some(source) = self.files.get_mut(&from) {
                    self.bytes_forced += source.dirty();
                    source.write_back();
                }
                self.copy(&from, &to);
                // A clone is a new file's mode whatever its source's: `Effect::Clone`.
                file_mut(&mut self.files, &to).mode = SimFile::default().mode;
                Done::Cloned
            }
            // A second name of one file: one inode ([`journal::Disk::linked`]), so both
            // names' looks give the same `ino`, times and change time. The bytes and mode are
            // kept per name (a copy): no place writes or chmods a linked file in place before
            // its looks, so a test of the store sees no difference.
            Effect::Link { from, to, .. } => {
                let SimFile { mode, link, .. } = self.files[&from];
                // clone: the second name's own record of which sectors hold blocks
                let blocks = self.files[&from].blocks.clone();
                let bytes = self.contents(&from);
                self.made(&to);
                *file_mut(&mut self.files, &to) = SimFile { mode, link, blocks, ..SimFile::whole(bytes) };
                self.disk.linked(&from, &to, journal::Tree { files: &self.files, dirs: &self.host.made });
                self.changed(&to);
                Done::Linked
            }
            // A stat follows a link as `stat(2)` does: what it leads to, or not there where it
            // dangles (the suite's `a_link_is_followed_by_stat_alone`, `conform/fs.rs`; until
            // 2026-11-20 the simulated disk followed none, so a stat of a link saw the link's
            // own length where every real disk sees its target's).
            Effect::Stat { file, .. } if self.files.get(&file).is_some_and(|it| it.link) => {
                match self.led_to(&file) {
                    Ok(Some(at)) if self.files.contains_key(&at) => {
                        Done::Stat(Some(self.meta(&at, false)))
                    }
                    Ok(Some(at)) if self.is_dir(&at) => Done::Stat(Some(self.meta(&at, true))),
                    Ok(_) => Done::Stat(None),
                    Err(error) => Done::Failed(error),
                }
            }
            // A name under a file: `ENOTDIR` from `stat(2)` and `statx`, a failure on every
            // native driver, never "not there" (the suite's `a_name_under_a_file`; until
            // 2026-11-20 the simulated disk answered `Stat(None)`, more than the real disk).
            Effect::Stat { file, .. } | Effect::Lstat { file, .. }
                if self.file_above(&file).is_some() =>
            {
                let above = self.file_above(&file).expect("looked up in the guard");
                // alloc: an error's message.
                Done::Failed(IoError(format!("{file}: {above} {}", host::NOT_A_DIRECTORY)))
            }
            // A link is a file of its own length to an lstat: it follows none.
            Effect::Stat { file, .. } | Effect::Lstat { file, .. } => Done::Stat({
                match self.files.contains_key(&file) {
                    true => Some(self.meta(&file, false)),
                    false if self.is_dir(&file) => Some(self.meta(&file, true)),
                    false => None,
                }
            }),
            Effect::List { dir, .. } => self.list(&dir),
            Effect::Private { dir, .. } => self.private(&dir),
            Effect::Stamp { files, .. } if files.len() > crate::STAMP_MOST => {
                let refused = crate::journal::stamp_rows(&files).expect_err("past the cap");
                // alloc: the refusal's text, once.
                Done::Failed(crate::IoError(refused.to_string()))
            }
            Effect::Stamp { files, .. } => Done::Stamped(
                files
                    .iter()
                    .map(|name| {
                        // alloc: a name of the batch as the simulated disk keys it.
                        let name = FileName::new(name).expect("a Names entry is a name");
                        self.disk.stamp(&name, journal::Tree { files: &self.files, dirs: &self.host.made })
                    })
                    // alloc: the stamps, which the completion carries.
                    .collect(),
            ),
            Effect::Entries { dir, .. } => match self.disk.entries(&dir, journal::Tree { files: &self.files, dirs: &self.host.made }) {
                Some(entries) => Done::Entries(Box::new(entries)),
                // alloc: an error's message.
                None => Done::Failed(IoError(format!("{dir}: no such directory"))),
            },
            Effect::Remove { file, .. } if self.is_dir(&file) => self.remove_made(&file),
            Effect::Remove { file, .. } => {
                if let Some(was) = self.files.remove(&file) {
                    // clone: the log's own name of the file, a refcount.
                    self.named(Named::Gone { file: file.share(), was });
                    self.unlink_locks(&file);
                    self.changed(&file);
                }
                Done::Removed
            }
            Effect::Lock { file, mode, tag } => self.lock(&file, mode, tag),
            Effect::MakeDir { dir, .. } => self.make_dir(dir),
            Effect::RemoveDir { dir, .. } => self.remove_dir(&dir),
            Effect::Ancestors { most, .. } => Done::Ancestors(crate::Ancestors::answered(
                // clone: the answer's names, refcounts.
                self.above.iter().map(|it| it.toward.clone()).collect(),
                most,
            )),
            Effect::Above { op, name, .. } => self.read_above(op, &name),
            Effect::Timer { .. } => Done::Fired,
            Effect::Map { file, extent, .. } => {
                let bytes = self.contents(&file);
                let range = match extent {
                    crate::Extent::Whole => Ok(&bytes[..]),
                    // As a driver maps a range: inside the file, or refused naming it.
                    crate::Extent::Range { at, len } => at
                        .checked_add(len)
                        .filter(|end| *end <= bytes.len() as u64)
                        .map(|end| &bytes[at as usize..end as usize])
                        .ok_or_else(|| {
                            // alloc: an error's message.
                            IoError(format!(
                                "{file}: a mapping of {len} bytes at {at} runs past the file's \
                                 end at {}: a range is mapped only inside the file",
                                bytes.len()
                            ))
                        }),
                };
                match range {
                    Ok(range) => {
                        self.bytes_read += range.len() as u64;
                        // alloc: the mapping, which the completion carries.
                        Done::Mapped(crate::Mapping::copy(range))
                    }
                    Err(why) => Done::Failed(why),
                }
            }
            Effect::Spawn { .. } => match spawn.and_then(|id| self.spawns.remove_id(id)) {
                // alloc: the exit, which the completion carries.
                Some((_, spawning)) => Done::Exited(Box::new(spawning.exit)),
                None => unreachable!("a spawn in flight has its run"),
            },
            Effect::Watch { .. }
            | Effect::Journal { .. }
            | Effect::Unwatch { .. }
            | Effect::Flush { .. }
            | Effect::Kill { .. }
            | Effect::Listen { .. }
            | Effect::Unlisten { .. }
            | Effect::Connect { .. }
            | Effect::Recv { .. }
            | Effect::Send { .. }
            | Effect::Hangup { .. }
            | Effect::Pressure { .. }
            | Effect::Unlock { .. }
            | Effect::Bind { .. }
            | Effect::SendTo { .. }
            | Effect::RecvFrom { .. }
            | Effect::Unbind { .. }
            | Effect::Resolve { .. }
            | Effect::ReadStdin { .. }
            | Effect::WriteOut { .. }
            | Effect::Now { .. }
            | Effect::Random { .. }
            | Effect::Roots { .. } => {
                unreachable!(
                    "watches, kills, sockets, unlocks and the host's effects are not pending \
                     effects"
                )
            }
        };
        // The names this effect changed: their kept descriptors close, as the real drivers'
        // do once the effect is done (`threads::run`).
        for name in changed.iter().flatten() {
            self.descriptors.forget(name);
        }
        Completion { tag, done }
    }

    fn watch(&mut self, tag: Tag, dir: crate::DirName, journal: bool) -> Id<Watching> {
        let (id, old) = self.watches.insert(tag, Watching { dir, journal, ending: false });
        if old.is_some_and(|it| it.ending) {
            self.ending -= 1;
        }
        id
    }

    /// The told completion as it is delivered: `None` for a change of a watch that ended
    /// first, a receive's bytes cut from its buffer when its end hung up first.
    fn deliver(&mut self, told: Told) -> Option<Completion> {
        let Told { completion, of, .. } = told;
        match of {
            Of::Op | Of::Flush(_) => Some(completion),
            Of::Change(watch) if self.watches.by_id(watch).is_some() => Some(completion),
            Of::Change(_) => {
                self.stale += 1;
                None
            }
            Of::End(watch) => {
                if self.watches.remove_id(watch).is_some() {
                    self.ending -= 1;
                }
                Some(completion)
            }
            Of::Recv { end, .. } if self.ends.get(end).is_some_and(|it| !it.closed) => {
                Some(completion)
            }
            Of::Recv { kept, .. } => match completion.done {
                Done::Read(mut bytes) if bytes.len() > kept => {
                    self.stale += 1;
                    bytes.truncate(kept);
                    Some(Completion { tag: completion.tag, done: Done::Read(bytes) })
                }
                done => Some(Completion { tag: completion.tag, done }),
            },
        }
    }
}

/// The simulation's clock, as a wait reads it.
struct Virtual<'a>(&'a Cell<Duration>);

impl Clock for Virtual<'_> {
    fn now(&self) -> Duration {
        self.0.get()
    }
}

impl Sim {
    /// Waits from now for a completion due at `first`, until `until`: the clock where the wait
    /// ended, `first` or the deadline, never past either. Each call of the wait blocks to the
    /// sooner of the two, or, interrupted, ends after a drawn part of that as EINTR or a
    /// spurious wakeup.
    fn wait(&mut self, first: Duration, until: Deadline) -> Duration {
        let now = Cell::new(self.now);
        let (rng, one_in) = (&mut self.rng, u64::from(self.faults.interrupt_one_in));
        let what = "a completion of the simulation";
        let waited = wait::until(what, &Virtual(&now), until.finite(), |left| {
            let at = now.get();
            let wake = left.map_or(first, |left| first.min(at.saturating_add(left)));
            // An interrupt comes while the call blocks, and the call took some time: the clock
            // moves at least a nanosecond, so every rate of interrupts still ends the wait.
            if one_in > 0 && wake > at && rng.below(one_in) == 0 {
                let span = wake.saturating_sub(at).as_nanos() as u64;
                now.set(at + Duration::from_nanos(1 + rng.below(span)));
                return Ok(match rng.below(2) {
                    0 => Once::Interrupted,
                    _ => Once::Spurious,
                });
            }
            now.set(wake);
            Ok(match wake >= first {
                true => Once::Ready(()),
                false => Once::TimedOut,
            })
        });
        if let Err(error) = waited {
            panic!("the simulation's clock: {error}");
        }
        now.get()
    }
}

impl Driver for Sim {
    fn submit(&mut self, effects: &mut Vec<Effect>) {
        // An armed simulation traces them, and takes none while its power is cut.
        if seeds::submitted(self, effects) {
            return;
        }
        for effect in effects.drain(..) {
            match effect {
                Effect::Watch { dir, tag } => {
                    self.watch(tag, crate::DirName::Named(dir), false);
                    continue;
                }
                Effect::Journal { dir, since, tag } => {
                    let parts = 1 + self.rng.below(3);
                    let batches = self.disk.history(&dir, &since, parts);
                    let id = self.watch(tag, dir, true);
                    for batch in batches {
                        // alloc: the batch's payload.
                        self.tell(tag, Done::Journaled(Box::new(batch)), Of::Change(id));
                    }
                    continue;
                }
                Effect::Pressure { tag } => {
                    match self.pressures.contains(&tag) {
                        // alloc: an error's message.
                        true => self.tell(
                            tag,
                            Done::Failed(IoError(format!(
                                "memory pressure is watched as {tag:?} already"
                            ))),
                            Of::Op,
                        ),
                        // alloc: none past the most watched at once.
                        false => self.pressures.push(tag),
                    }
                    continue;
                }
                // A watch of memory pressure ends after every pressure told it.
                Effect::Unwatch { tag } if self.pressures.contains(&tag) => {
                    self.pressures.retain(|&it| it != tag);
                    self.tell_after(tag, tag, Done::Unwatched, Of::Op);
                    continue;
                }
                Effect::Unwatch { tag } => {
                    let ended = match self.watches.get_mut(tag) {
                        Some(watch) if !watch.ending => {
                            watch.ending = true;
                            true
                        }
                        _ => false,
                    };
                    let (done, of) = match self.watches.id(tag) {
                        Some(id) if ended => {
                            self.ending += 1;
                            (Done::Unwatched, Of::End(id))
                        }
                        // alloc: an error's message.
                        _ => (
                            Done::Failed(IoError(format!("nothing is watched as {tag:?}"))),
                            Of::Op,
                        ),
                    };
                    // A change told before the end is heard before it.
                    self.tell_after(tag, tag, done, of);
                    continue;
                }
                Effect::Flush { watch, tag } => {
                    let done = match self.watches.get(watch).is_some_and(|it| !it.ending) {
                        // alloc: an error path.
                        false => Done::Failed(IoError(format!("nothing is watched as {watch:?}"))),
                        true if self.rng.one_in(u64::from(self.faults.fail_one_in)) => {
                            self.fail(tag).done
                        }
                        true => Done::Flushed,
                    };
                    // Every change of the watch told by now is heard first.
                    self.tell_after(watch, tag, done, Of::Flush(watch));
                    continue;
                }
                // A real unlock cannot fail, so none is injected to.
                Effect::Unlock { lock, tag } => {
                    let done = match self.unlock(lock) {
                        true => Done::Unlocked,
                        // alloc: an error's message.
                        false => Done::Failed(IoError(format!(
                            "unlock of lock {}: it holds no lock",
                            lock.0
                        ))),
                    };
                    self.tell(tag, done, Of::Op);
                    continue;
                }
                Effect::Kill { tag } => {
                    if let Some(spawning) = self.spawns.get_mut(tag)
                        && let Some(handle) = spawning.pending
                        && let Some(ready) = self.pending.deadline(handle)
                    {
                        let delay = latency(&mut self.rng, &self.faults);
                        spawning.exit.code = None;
                        spawning.exit.signal = Some(KILLED);
                        let sooner = wheel::ticks(self.now).saturating_add(wheel::ticks(delay));
                        if sooner < ready {
                            let pending = self.pending.cancel(handle).expect("in the wheel");
                            spawning.pending = Some(self.pending.insert(sooner, pending));
                        }
                    }
                    continue;
                }
                Effect::Listen { at, options, tag } => {
                    let side = crate::sockopt::Side::Listen;
                    if let Err(refused) =
                        crate::sockopt::check_tcp(options.get(), side, crate::sockopt::Platform::Simulated)
                    {
                        // alloc: an error's message.
                        self.tell(tag, Done::Failed(IoError(format!("listening at {at:?}: {refused}"))), Of::Op);
                        continue;
                    }
                    let at = match at {
                        Address::Inherited { at, .. } => Address::Unix(at),
                        at => at,
                    };
                    match self.listening_at(&at) {
                        Some(_) => self.tell(
                            tag,
                            // alloc: an error's message.
                            Done::Failed(IoError(format!("{at:?}: another listen has it"))),
                            Of::Op,
                        ),
                        None if self.rng.one_in(u64::from(self.faults.fail_one_in)) => {
                            let done = self.fail(tag).done;
                            self.tell(tag, done, Of::Op);
                        }
                        None => _ = self.listens.insert(tag, at),
                    }
                    continue;
                }
                Effect::Unlisten { tag } => {
                    let done = match self.listens.remove(tag) {
                        Some(_) => Done::Unlistened,
                        // alloc: an error's message.
                        None => Done::Failed(IoError(format!("nothing is listened on as {tag:?}"))),
                    };
                    self.tell(tag, done, Of::Op);
                    continue;
                }
                Effect::Connect { at, options, tag } => {
                    let side = crate::sockopt::Side::Connect;
                    if let Err(refused) =
                        crate::sockopt::check_tcp(options.get(), side, crate::sockopt::Platform::Simulated)
                    {
                        // alloc: an error's message.
                        self.tell(tag, Done::Failed(IoError(format!("connecting to {at:?}: {refused}"))), Of::Op);
                        continue;
                    }
                    let done = match self.listening_at(&at) {
                        _ if self.rng.one_in(u64::from(self.faults.fail_one_in)) => {
                            self.fail(tag).done
                        }
                        // alloc: an error's message.
                        None => Done::Failed(IoError(format!(
                            "{at:?}: nothing listens there: connection refused"
                        ))),
                        Some(listen) => {
                            let mine = self.new_end(None);
                            let theirs = self.new_end(Some(end_of(mine)));
                            self.ends.get_mut(end_of(mine)).expect("just made").peer =
                                Some(end_of(theirs));
                            self.tell(listen, Done::Accepted { conn: theirs }, Of::Op);
                            Done::Connected { conn: mine }
                        }
                    };
                    self.tell(tag, done, Of::Op);
                    continue;
                }
                Effect::Recv { conn, most, into, tag } => {
                    let end = end_of(conn);
                    match self.ends.get(end).is_some_and(|it| !it.closed) {
                        true => {
                            self.receiving.push(Receiving { tag, end, most, into });
                            self.wake();
                        }
                        false => self.tell(
                            tag,
                            Done::unread(into, format_args!("no connection {conn:?}")),
                            Of::Op,
                        ),
                    }
                    continue;
                }
                Effect::Send { conn, mut bytes, tag } => {
                    let end = end_of(conn);
                    if self
                        .ends
                        .get(end)
                        .is_some_and(|it| it.holds_sends && !it.closed && !it.peer_closed)
                    {
                        self.held_sends.push((conn, tag, bytes));
                        continue;
                    }
                    let done =
                        match self.ends.get(end).map(|it| (it.closed, it.peer_closed, it.peer)) {
                            None | Some((true, _, _)) => {
                                Done::unsent(bytes, format_args!("no connection {conn:?}"))
                            }
                            Some((_, true, _)) => Done::unsent(
                                bytes,
                                format_args!("{conn:?}: the connection is hung up: broken pipe"),
                            ),
                            Some((_, _, None)) => {
                                let it = self.ends.get_mut(end).expect("looked up");
                                it.outbox.extend_from_slice(&bytes);
                                bytes.clear();
                                Done::Sent(bytes)
                            }
                            Some((_, _, Some(peer))) => {
                                if let Some(peer) = self.ends.get_mut(peer) {
                                    peer.inbox.extend_from_slice(&bytes);
                                }
                                bytes.clear();
                                Done::Sent(bytes)
                            }
                        };
                    self.tell(tag, done, Of::Op);
                    self.wake();
                    continue;
                }
                Effect::Hangup { conn, tag } => {
                    let end = end_of(conn);
                    let hung = match self.ends.get_mut(end) {
                        Some(it) if !it.closed => {
                            it.closed = true;
                            Some((it.peer, it.peer_closed))
                        }
                        _ => None,
                    };
                    self.let_go_sends(conn);
                    let done = match hung {
                        Some((peer, peer_closed)) => {
                            if let Some(it) = peer.and_then(|peer| self.ends.get_mut(peer)) {
                                it.peer_closed = true;
                            }
                            // An end of the simulation's own, or one the peer outside hung up
                            // too, is named by nothing now; one the peer outside may still
                            // read from is kept for it.
                            if peer.is_some() || peer_closed {
                                self.ends.remove(end);
                            }
                            Done::HungUp
                        }
                        // alloc: an error's message.
                        None => Done::Failed(IoError(format!("no connection {conn:?}"))),
                    };
                    self.wake();
                    self.tell(tag, done, Of::Op);
                    continue;
                }
                effect @ (Effect::Bind { .. }
                | Effect::SendTo { .. }
                | Effect::RecvFrom { .. }
                | Effect::Unbind { .. }
                | Effect::Resolve { .. }
                | Effect::ReadStdin { .. }
                | Effect::WriteOut { .. }
                | Effect::Now { .. }
                | Effect::Random { .. }
                | Effect::Roots { .. }) => {
                    self.hosted(effect);
                    continue;
                }
                Effect::Spawn { ref command, tag } => match self.run(command) {
                    Ok(ran) => {
                        let after = ran.takes.saturating_add(self.delay());
                        let ready = wheel::ticks(self.now).saturating_add(wheel::ticks(after));
                        let (id, _) =
                            self.spawns.insert(tag, Spawning { exit: ran.exit, pending: None });
                        let pending = self
                            .pending
                            .insert(ready, Pending { effect, covers: 0, spawn: Some(id) });
                        if let Some((_, spawning)) = self.spawns.by_id_mut(id) {
                            spawning.pending = Some(pending);
                        }
                        continue;
                    }
                    Err(why) => {
                        self.tell(tag, Done::Failed(why), Of::Op);
                        continue;
                    }
                },
                _ => {}
            }
            // The kernel may write a read's span from now until it completes.
            Sim::scribble(&effect);
            let held = match (&effect, &self.hold_reads) {
                (Effect::ReadInto { file, .. }, Some(dir)) => file.is_under(dir),
                (Effect::ReadMany { parts, .. }, Some(dir)) => {
                    parts.iter().any(|part| part.file.is_under(dir))
                }
                _ => false,
            };
            if held {
                self.held_reads.push(effect);
                continue;
            }
            let delay = match (&effect, &self.slow) {
                (Effect::Timer { after, .. }, _) => *after,
                (
                    Effect::Sync { file, .. }
                    | Effect::Barrier { file, .. }
                    | Effect::WriteBack { file, .. }
                    | Effect::Write { file, .. }
                    | Effect::WriteUnsynced { file, .. }
                    | Effect::WriteAt { file, .. }
                    | Effect::WriteFrom { file, .. }
                    | Effect::Truncate { file, .. },
                    Some((dir, by)),
                ) if file.is_under(dir) => *by,
                _ => self.delay(),
            };
            if let (
                Effect::Sync { file, .. }
                | Effect::Barrier { file, .. }
                | Effect::WriteBack { file, .. },
                Some(dir),
            ) = (&effect, &self.hold_syncs)
                && file.is_under(dir)
            {
                let covers = self.files.get(file).map_or(0, |it| it.ops.len());
                self.held.push((effect, covers));
                continue;
            }
            if let (Effect::List { dir, tag }, Some(held)) = (&effect, &self.hold_lists)
                && dir.name() == Some(held)
            {
                // clone: the held list keeps its directory's name
                self.held_lists.push((dir.share(), *tag));
                continue;
            }
            // A directory the user may not read fails both listings: names alone, and names
            // with stamps (bstd's `Dir::facts_of` lists folders by the second).
            if let (Effect::List { dir, tag } | Effect::Entries { dir, tag }, Some((failing, left))) =
                (&effect, &mut self.fail_lists)
                && dir.name() == Some(&*failing)
                && *left > 0
            {
                *left -= 1;
                // alloc: an error's message.
                let done = Done::Failed(IoError(format!("{dir}: Permission denied (os error 13)")));
                let tag = *tag;
                self.tell(tag, done, Of::Op);
                continue;
            }
            if let (
                Effect::Sync { file, tag }
                | Effect::Barrier { file, tag }
                | Effect::WriteBack { file, tag },
                Some(dir),
            ) = (&effect, &self.fail_syncs)
                && file.is_under(dir)
            {
                // alloc: an error's message.
                let done =
                    Done::Failed(IoError(format!("{file}: Input/output error (os error 5)")));
                let tag = *tag;
                self.tell(tag, done, Of::Op);
                continue;
            }
            let covers = match &effect {
                Effect::Sync { file, .. }
                | Effect::Barrier { file, .. }
                | Effect::WriteBack { file, .. } => {
                    self.files.get(file).map_or(0, |it| it.ops.len())
                }
                _ => 0,
            };
            self.pend(delay, effect, covers);
        }
    }

    /// Waits in virtual time: the clock moves to the first completion when none is ready, or
    /// to `until` where that comes first (then nothing is moved). The wait goes through
    /// [`wait::until`], interrupted at [`Faults::interrupt_one_in`]'s rate as a kernel's is.
    /// With nothing in flight that will complete, it returns at once, unless its waker was
    /// handed out ([`Sim::give_wakers`]): then it keeps the rule on [`Driver::waker`], and
    /// other threads' wakes ([`Sim::wake_at`]) land before it looks, between the look and the
    /// park, or while it is parked (`sim/wake.rs`).
    fn poll(&mut self, out: &mut Vec<Completion>, until: Deadline) -> usize {
        if seeds::off(self) {
            return 0;
        }
        // With `clock-jump` on, the machine may have slept since the last poll (`sim/every_cut.rs`).
        every_cut::jump_clock(self);
        let before = out.len();
        match self.wakes.is_some() {
            true => self.poll_with_wakes(out, before, until),
            false => self.poll_without_wakes(out, until),
        }
        seeds::polled(self, &out[before..]);
        out.len() - before
    }

    fn in_flight(&self) -> usize {
        let ending = self.told.iter().filter(|it| {
            !matches!(
                it.completion.done,
                Done::Changed { .. }
                    | Done::Journaled(_)
                    | Done::Accepted { .. }
                    | Done::Pressed { .. }
            )
        });
        self.pending.len()
            + self.held_sends.len()
            + (self.watches.len() - self.ending)
            + self.listens.len()
            + self.receiving.len()
            + self.pressures.len()
            + self.host_in_flight()
            + ending.count()
    }

    fn now(&self) -> Duration {
        self.now
    }

    /// The seed's wall clock start plus the virtual clock: the clock the disk stamps change
    /// times by ([`journal::Disk::at`]), so a look just after a write sees a racy stamp.
    fn wall_ns(&self) -> u64 {
        self.wall_ns()
    }

    fn boot(&self) -> Result<crate::Boot, crate::NoBoot> {
        Ok(self.boot)
    }

    /// The simulation's waker, once a test asked for one ([`Sim::give_wakers`]); from this
    /// call on a poll keeps the rule a native driver keeps.
    fn waker(&self) -> Option<crate::Waker> {
        self.wakes().map(wake::Wakes::waker)
    }
}

/// The most turns one poll takes inside the simulation before it hands its caller anything:
/// each turn moves the clock to an event, runs an act or hands another process its
/// completions, so a poll past this is two other processes answering each other at one
/// instant forever (rule bounded-progress).
const POLL_TURNS_MOST: u32 = 1 << 20;

impl Sim {
    /// When the first completion is due, of what is in flight. (`&mut`: the timers' next
    /// deadline settles their heap as it is read.)
    fn first_due(&mut self) -> Option<Duration> {
        let first = self.pending.next_deadline().map(Duration::from_nanos);
        first.into_iter().chain(self.first_landing()).chain(self.told.iter().map(|it| it.at)).min()
    }

    /// The poll of a simulation that keeps no waker: advances until at least one completion is
    /// delivered, nothing more is due, or `until` is reached (a datagram landing at `first`
    /// queues a `RecvFrom` completion at `first + delay`, which the next turn collects).
    fn poll_without_wakes(&mut self, out: &mut Vec<Completion>, until: Deadline) {
        let before = out.len();
        for _ in 0..POLL_TURNS_MOST {
            let Some(first) = self.first_due() else { return };
            if first > self.now {
                self.now = self.wait(first, until);
                self.disk.at(self.wall_ns());
                if self.now < first {
                    return;
                }
            }
            self.collect(out);
            if out.len() > before {
                return;
            }
        }
    }

    /// The poll of a simulation whose waker may be handed out: the look, the park and the
    /// wait, with other threads' acts landing between them (`sim/wake.rs`), and other
    /// processes' completions handed to them, until the caller has a completion, a wake or
    /// its deadline.
    fn poll_with_wakes(&mut self, out: &mut Vec<Completion>, before: usize, until: Deadline) {
        for _ in 0..POLL_TURNS_MOST {
            // Acts due by now: each before the look or after it, as the seed draws.
            self.run_posts(wake::Lands::Drawn);
            let woke = self.wakes().is_some_and(wake::Wakes::take);
            self.collect(out);
            self.hand_to_others(out, before);
            if out.len() > before || woke {
                return;
            }
            // The look saw nothing; what is due now and was not before it lands before the
            // park, and the park sees it.
            self.run_posts(wake::Lands::BeforePark);
            if self.wakes().is_some_and(wake::Wakes::take) {
                return;
            }
            let posted = self.wakes().and_then(wake::Wakes::next_due);
            let target = match self.first_due().into_iter().chain(posted).min() {
                Some(target) => target,
                // No waker handed out: nothing will end a wait, as with none.
                None if !self.wakes().is_some_and(wake::Wakes::given) => return,
                None if until.finite().is_none() => panic!(
                    "the simulation: a poll parked with its waker handed out, no deadline, nothing in \
                     flight that completes and no wake scheduled (Sim::wake_at) at {:?}: a native \
                     driver's never returns (rule bounded-progress)",
                    self.now
                ),
                // Parked to the deadline, which the wait reaches first.
                None => Duration::MAX,
            };
            if target > self.now {
                self.now = self.wait(target, until);
                self.disk.at(self.wall_ns());
                if self.now < target {
                    // The deadline.
                    return;
                }
            }
            if posted.is_some_and(|at| at <= self.now) {
                self.run_posts(wake::Lands::Parked);
                if self.wakes().is_some_and(wake::Wakes::take) {
                    // What completed at the same instant comes with it.
                    self.collect(out);
                    self.hand_to_others(out, before);
                    return;
                }
            }
        }
        panic!(
            "the simulation: a poll turned {POLL_TURNS_MOST} times at {:?} with nothing for its caller: \
             other processes' effects answer each other at one instant (rule bounded-progress)",
            self.now
        );
    }

    /// Moves what is due by now into `out`: effects ready, datagrams landed, what was told.
    fn collect(&mut self, out: &mut Vec<Completion>) {
        let mut ready = mem::take(&mut self.ready);
        self.pending.advance(wheel::ticks(self.now), |_, pending| ready.push(pending));
        // In the order they were due; shuffled so it is the seed's.
        self.rng.shuffle(&mut ready);
        for pending in ready.drain(..) {
            if !self.recording.is_empty() {
                let file = match &pending.effect {
                    // A barrier is what a commit waits on the device for where it once
                    // waited on a sync: recorded as one.
                    Effect::Sync { file, .. } | Effect::Barrier { file, .. } => Some((file, true)),
                    Effect::Write { file, .. }
                    | Effect::WriteUnsynced { file, .. }
                    | Effect::WriteAt { file, .. }
                    | Effect::WriteFrom { file, .. } => Some((file, false)),
                    _ => None,
                };
                if let Some((file, sync)) = file
                    && self.recording.iter().any(|dir| file.is_under(dir))
                {
                    // clone: a test's record, once a completion recorded
                    self.recorded.push((self.now, file.share(), sync));
                }
            }
            let completion = self.complete(pending);
            out.push(completion);
        }
        self.ready = ready;
        // Datagrams due by now arrive, and the receives waiting for them are told.
        self.land();
        // What watches were told, in the order they were told: a watch's end comes last.
        let mut told = mem::take(&mut self.told);
        let mut later = mem::take(&mut self.later);
        for it in told.drain(..) {
            if it.at > self.now {
                later.push(it);
            } else if let Some(completion) = self.deliver(it) {
                out.push(completion);
            }
        }
        self.told = later;
        self.later = told;
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::run_until_idle;

    fn name(it: &str) -> FileName {
        FileName::new(it).unwrap()
    }

    /// What a crash leaves of a file with 1000 synced bytes and two unsynced appends after
    /// them (1000 and 1000 more, never zero): over many seeds, the synced bytes are equal in
    /// every outcome; each [`SECTOR`] past them is wholly the new bytes or wholly zeros; the
    /// length is between the two; and every class a real disk may leave occurs: the whole
    /// of it kept, the whole of it lost, a clean prefix, a zeroed sector at the end under a
    /// grown length, and a hole in the middle with the end intact. A seed replays its crash.
    #[test]
    fn a_crash_keeps_what_was_synced_and_any_sectors_of_the_rest() {
        let synced: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8 + 1).collect();
        let more: Vec<u8> = (0..2000u32).map(|i| (i % 241) as u8 + 3).collect();
        let mut all = synced.clone();
        all.extend_from_slice(&more);
        let crashed = |seed: u64| {
            let mut sim = Sim::new(seed, Faults::default());
            let file = name("log");
            let mut effects = vec![Effect::Append { file: file.clone(), bytes: synced.clone(), tag: Tag(0) }];
            run_until_idle(&mut sim, &mut effects, |_, _| {}).expect("the effects end");
            effects.push(Effect::Sync { file: file.clone(), tag: Tag(1) });
            // The file's sync keeps its bytes; its name is kept by its directory's sync.
            effects.push(Effect::SyncDir { dir: crate::DirName::Root, tag: Tag(3) });
            run_until_idle(&mut sim, &mut effects, |_, _| {}).expect("the effects end");
            effects.push(Effect::Append { file: file.clone(), bytes: more[..1000].to_vec(), tag: Tag(2) });
            run_until_idle(&mut sim, &mut effects, |_, _| {}).expect("the effects end");
            effects.push(Effect::Append { file: file.clone(), bytes: more[1000..].to_vec(), tag: Tag(4) });
            run_until_idle(&mut sim, &mut effects, |_, _| {}).expect("the effects end");
            sim.crash();
            sim.contents(&file)
        };
        let (mut whole, mut lost, mut prefix, mut zeroed_end, mut hole) = (0, 0, 0, 0, 0);
        for seed in 0..2000 {
            let left = crashed(seed);
            assert_eq!(left[..synced.len().min(left.len())], synced[..synced.len().min(left.len())], "seed {seed}");
            assert!(left.len() >= synced.len() && left.len() <= all.len(), "seed {seed}: {} bytes", left.len());
            // Each sector past the synced bytes: all new, or as it was after a change kept
            // whole (the new bytes up to the end of the synced part or of the first append),
            // then zeros past that end. All written bytes are non-zero.
            let mut from = synced.len();
            let mut kinds = Vec::new();
            while from < left.len() {
                let to = ((from / SECTOR + 1) * SECTOR).min(left.len());
                let written = from + left[from..to].iter().zip(&all[from..to]).take_while(|(a, b)| a == b).count();
                let ends_a_change = written == from || written == synced.len() || written == synced.len() + 1000;
                assert!(
                    left[written..to].iter().all(|it| *it == 0) && (written == to || ends_a_change),
                    "seed {seed}: the sector at {from} is new to {written}, then not zeros or not at a change's end"
                );
                kinds.push(written == to);
                from = to;
            }
            if left == all {
                whole += 1;
            } else if left.len() == synced.len() {
                lost += 1;
            } else if kinds.iter().all(|it| *it) {
                prefix += 1;
            }
            if kinds.last() == Some(&false) {
                zeroed_end += 1;
            }
            if left.len() == all.len() && kinds.contains(&false) && kinds.last() == Some(&true) {
                hole += 1;
            }
        }
        eprintln!(
            "of 2000 crashes: {whole} kept whole, {lost} lost whole, {prefix} a clean prefix, \
             {zeroed_end} a zeroed sector at the end, {hole} a hole in the middle with the end intact"
        );
        assert!(whole > 0 && lost > 0 && prefix > 0 && zeroed_end > 0 && hole > 0);
        assert_eq!(crashed(7), crashed(7), "a seed replays its crash");
        assert!((0..20).any(|seed| crashed(seed) != crashed(7)), "the seeds draw different crashes");
    }

    /// The boot is one fact until a power cut, and another after each one; a seed draws the
    /// same boots each time it runs.
    #[test]
    fn a_power_cut_is_a_new_boot() {
        let boots = |seed| {
            let mut sim = Sim::new(seed, Faults::default());
            let first = Driver::boot(&sim).expect("the simulation has a boot");
            ran(&mut sim, vec![Effect::Append { file: name("f"), bytes: b"x".to_vec(), tag: Tag(1) }]);
            assert_eq!(Driver::boot(&sim), Ok(first), "seed {seed}: the boot moved without a cut");
            sim.crash();
            let second = Driver::boot(&sim).expect("a boot after the cut");
            sim.crash();
            let third = Driver::boot(&sim).expect("a boot after the second cut");
            assert!(first != second && second != third && first != third, "seed {seed}");
            [first, second, third]
        };
        assert_eq!(boots(3), boots(3));
        assert_ne!(boots(3), boots(4));
    }

    #[test]
    fn the_same_seed_is_the_same_run() {
        let run = |seed| {
            let mut sim = Sim::new(seed, Faults { fail_one_in: 4, ..Faults::default() });
            let mut effects: Vec<Effect> = (0..32)
                .map(|n| Effect::Append { file: name("f"), bytes: vec![n as u8], tag: Tag(n) })
                .collect();
            let mut order = Vec::new();
            run_until_idle(&mut sim, &mut effects, |done, _| order.push(done))
            .expect("the effects end");
            (order, sim.contents(&name("f")))
        };
        assert_eq!(run(7), run(7));
        assert_ne!(run(7), run(8));
    }

    /// A log committed by ordering alone: each round appends a record to `data`, asks for a
    /// barrier of it (or, `plant`ed, for none), and only once that completed appends the
    /// record's end to `commits`, which no one syncs. Gives what a crash at a drawn point
    /// left of the two files.
    fn barrier_log(seed: u64, plant: bool) -> (Vec<u8>, Vec<u8>) {
        let mut sim = Sim::new(seed, Faults::default());
        let (data, commits) = (name("log/data"), name("log/commits"));
        let rounds = 1 + sim.rng.below(6);
        let crash_after = sim.rng.below(rounds * 3 + 1);
        // Both files made and their names synced before the log starts: what is tested is
        // the order of their bytes, not whether a crash keeps their names. The folder `log` is
        // durable as the first truncate makes it ([`Sim::make_parents`]); the sync of `log`
        // keeps the names in it.
        ran(&mut sim, vec![Effect::Truncate { file: data.clone(), len: 0, tag: Tag(4) }]);
        ran(&mut sim, vec![Effect::Truncate { file: commits.clone(), len: 0, tag: Tag(5) }]);
        ran(&mut sim, vec![Effect::SyncDir { dir: name("log").into(), tag: Tag(6) }]);
        let mut steps = 0;
        let mut step = |sim: &mut Sim, effect: Effect| {
            if steps == crash_after {
                return false;
            }
            steps += 1;
            ran(sim, vec![effect]);
            true
        };
        'rounds: for round in 0..rounds {
            let record = vec![round as u8 + 1; 700];
            let end = ((round + 1) * 700) as u32;
            let effects = [
                Effect::Append { file: data.clone(), bytes: record, tag: Tag(1) },
                match plant {
                    false => Effect::Barrier { file: data.clone(), tag: Tag(2) },
                    // The planted bug: the bytes written back and nothing ordered.
                    true => Effect::WriteBack { file: data.clone(), tag: Tag(2) },
                },
                Effect::Append { file: commits.clone(), bytes: end.to_le_bytes().to_vec(), tag: Tag(3) },
            ];
            for effect in effects {
                if !step(&mut sim, effect) {
                    break 'rounds;
                }
            }
        }
        sim.crash();
        let read = |sim: &Sim, file: &FileName| match sim.exists(file) {
            true => sim.contents(file),
            false => Vec::new(),
        };
        (read(&sim, &data), read(&sim, &commits))
    }

    /// What the log's reader holds: every whole commit names bytes of `data` that are there
    /// and are the records written, in order. `Err` says which commit names what is not.
    fn barrier_log_holds((data, commits): &(Vec<u8>, Vec<u8>)) -> Result<(), String> {
        for (n, end) in commits.chunks_exact(4).enumerate() {
            let end = u32::from_le_bytes(end.try_into().unwrap()) as usize;
            let want: Vec<u8> = (0..end).map(|at| (at / 700) as u8 + 1).collect();
            if data.get(..end) != Some(&want[..]) {
                return Err(format!(
                    "commit {n} names {end} bytes of data, which holds {} and not those",
                    data.len()
                ));
            }
        }
        Ok(())
    }

    /// The invariant: what a barrier covers is ordered before every write asked for after it
    /// completed. A commit record appended only after its data's barrier never survives a
    /// crash that lost or tore the data it names, though nothing was synced: on every seed,
    /// at every crash point.
    #[test]
    fn a_crash_keeps_no_write_made_after_a_barrier_without_what_the_barrier_covers() {
        let mut committed = 0;
        for seed in 0..4000 {
            let left = barrier_log(seed, false);
            if let Err(why) = barrier_log_holds(&left) {
                panic!("seed {seed}: {why}");
            }
            committed += left.1.len() / 4;
        }
        assert!(committed > 2000, "the crashes kept {committed} commits: too few to judge by");
    }

    /// The planted-bug control: with a write-back where the barrier was (the bytes on their
    /// way to the device and nothing ordered), some crash keeps a commit and loses the data it
    /// names, and the oracle says so.
    #[test]
    fn the_oracle_catches_a_commit_ordered_by_a_write_back() {
        let caught = (0..4000).find_map(|seed| barrier_log_holds(&barrier_log(seed, true)).err());
        let why = caught.expect("the oracle missed a commit whose data no barrier ordered");
        assert!(why.contains("names") && why.contains("bytes of data"), "{why}");
    }

    /// A barrier is not durable by itself, and is once any sync that waits for the drive
    /// completes after it: that sync flushed the drive's cache, where the barrier's bytes
    /// were.
    #[test]
    fn what_a_barrier_covers_is_durable_once_a_later_sync_of_any_file_completes() {
        let (data, other) = (name("log/data"), name("log/other"));
        let mut lost = false;
        // The file made and its name synced first (its folder durable as the truncate makes
        // it): what is tested is its bytes.
        let made = |sim: &mut Sim| {
            ran(sim, vec![Effect::Truncate { file: data.clone(), len: 0, tag: Tag(4) }]);
            ran(sim, vec![Effect::SyncDir { dir: name("log").into(), tag: Tag(5) }]);
        };
        for seed in 0..200 {
            let mut sim = Sim::new(seed, Faults::default());
            made(&mut sim);
            ran(&mut sim, vec![Effect::Append { file: data.clone(), bytes: b"ordered".to_vec(), tag: Tag(1) }]);
            let done = ran(&mut sim, vec![Effect::Barrier { file: data.clone(), tag: Tag(2) }]);
            assert_eq!(done[&Tag(2)], Done::Ordered);
            let mut after = Sim::new(seed, Faults::default());
            made(&mut after);
            ran(&mut after, vec![Effect::Append { file: data.clone(), bytes: b"ordered".to_vec(), tag: Tag(1) }]);
            ran(&mut after, vec![Effect::Barrier { file: data.clone(), tag: Tag(2) }]);
            ran(&mut after, vec![Effect::Sync { file: other.clone(), tag: Tag(3) }]);
            sim.crash();
            after.crash();
            lost |= sim.contents(&data) != b"ordered";
            assert_eq!(after.contents(&data), b"ordered", "seed {seed}");
        }
        assert!(lost, "a barrier alone never lost its bytes to a crash: it is modelled as a sync");
    }

    /// A table committed by its name, as blaze-solo commits its table: the old table durable
    /// under `st/table`, the new one written beside it (`st/table.part`) with no sync, a
    /// barrier of the part, and the part renamed over the table with no sync either; then a
    /// crash. `planted`: the simulation's barriers keep nothing
    /// ([`host::Plant::BarriersNotKept`]). What the table's name holds after the crash, then
    /// the old table and the new.
    fn barrier_then_rename(seed: u64, planted: bool) -> (Option<Vec<u8>>, Vec<u8>, Vec<u8>) {
        let mut sim = Sim::new(seed, Faults::default());
        if planted {
            sim.plant(host::Plant::BarriersNotKept);
        }
        let (table, part) = (name("st/table"), name("st/table.part"));
        let old = vec![0xEE; 900];
        // Three sectors, none of them zero: a tear is told from the whole table.
        let new: Vec<u8> = (0..1500u32).map(|at| (at % 250) as u8 + 1).collect();
        ran(&mut sim, vec![Effect::Write { file: table.clone(), bytes: old.clone(), tag: Tag(1) }]);
        ran(&mut sim, vec![Effect::SyncDir { dir: name("st").into(), tag: Tag(2) }]);
        ran(&mut sim, vec![Effect::WriteUnsynced { file: part.clone(), bytes: new.clone(), tag: Tag(3) }]);
        let done = ran(&mut sim, vec![Effect::Barrier { file: part.clone(), tag: Tag(4) }]);
        assert_eq!(done[&Tag(4)], Done::Ordered, "seed {seed}");
        let done = ran(&mut sim, vec![Effect::RenameUnsynced { from: part, to: table.clone(), tag: Tag(5) }]);
        assert_eq!(done[&Tag(5)], Done::Renamed, "seed {seed}");
        sim.crash();
        let left = sim.exists(&table).then(|| sim.contents(&table));
        (left, old, new)
    }

    /// What the table's name held, said for a failure.
    fn held(left: &Option<Vec<u8>>) -> String {
        match left {
            Some(bytes) => format!("{} bytes", bytes.len()),
            None => "no file".to_string(),
        }
    }

    /// The invariant: a barrier orders what it covers before every write asked for after it
    /// completed, a change of a name included ([`Effect::Barrier`]: "the bytes ordered before
    /// the name"). So a table written beside its name, barriered and renamed over it, none of
    /// it synced, is after any crash the old table or the whole new one under its name, never a
    /// tear: on every seed. Both outcomes are reached, or the test judged nothing. Prints
    /// `test sim::tests::a_rename_after_a_barrier_keeps_the_bytes_it_orders ... ok`.
    #[test]
    fn a_rename_after_a_barrier_keeps_the_bytes_it_orders() {
        let (mut olds, mut news) = (0, 0);
        for seed in 0..2000 {
            let (left, old, new) = barrier_then_rename(seed, false);
            match &left {
                Some(bytes) if *bytes == old => olds += 1,
                Some(bytes) if *bytes == new => news += 1,
                _ => panic!(
                    "seed {seed}: the table's name holds {}, neither the old table ({} bytes) nor the whole new one ({} bytes)",
                    held(&left),
                    old.len(),
                    new.len()
                ),
            }
        }
        assert!(olds > 100 && news > 100, "the crashes left the old table {olds} times and the new {news}: too few to judge by");
    }

    /// The planted-bug control: with the barriers keeping nothing, some crash keeps the rename
    /// and tears (or loses) the bytes the barrier ordered before it, and the oracle names it.
    /// Prints `test sim::tests::the_oracle_catches_a_rename_kept_without_the_bytes_its_barrier_ordered ... ok`.
    #[test]
    fn the_oracle_catches_a_rename_kept_without_the_bytes_its_barrier_ordered() {
        let torn = (0..2000).find_map(|seed| {
            let (left, old, new) = barrier_then_rename(seed, true);
            (left.as_ref() != Some(&old) && left.as_ref() != Some(&new)).then(|| (seed, held(&left)))
        });
        let (seed, held) = torn.expect("with the barriers keeping nothing no crash tore the table under its name: the oracle sees nothing");
        assert!(held != "no file", "seed {seed}: the table's name lost its file, which no barrier is about");
    }

    #[test]
    fn a_sync_covers_only_what_was_appended_before_it() {
        let mut sim = Sim::new(
            1,
            Faults { fail_one_in: 0, max_latency: Duration::ZERO, ..Faults::default() },
        );
        let file = name("log");
        let mut effects =
            vec![Effect::Append { file: file.clone(), bytes: b"a".to_vec(), tag: Tag(0) }];
        run_until_idle(&mut sim, &mut effects, |_, _| {}).expect("the effects end");
        sim.submit(&mut vec![Effect::Sync { file: file.clone(), tag: Tag(1) }]);
        // Appended after the sync was asked for, completed before it.
        sim.files.get_mut(&file).unwrap().push_op(Op::Append(b"b".to_vec()));
        let mut out = Vec::new();
        sim.poll(&mut out, Deadline::NEVER);
        let file = &sim.files[&file];
        assert_eq!((file.durable.as_slice(), file.contents().as_slice()), (&b"a"[..], &b"ab"[..]));
    }

    /// The length of a file with unsynced changes, worked out without its bytes, is the length
    /// of its bytes, and a read of a range of it is that range, on every seed.
    #[test]
    fn a_file_s_length_and_reads_are_its_bytes_on_every_seed() {
        for seed in 0..300 {
            let mut sim = Sim::new(seed, Faults::default());
            let file = name("f");
            sim.change(&file, b"0123456789");
            for _ in 0..8 {
                let op = match sim.rng.below(3) {
                    0 => Op::Append(vec![b'a'; sim.rng.below(5) as usize]),
                    1 => Op::At(sim.rng.below(20), vec![b'b'; sim.rng.below(5) as usize]),
                    _ => Op::Cut(sim.rng.below(20)),
                };
                sim.files.get_mut(&file).unwrap().push_op(op);
                let whole = sim.contents(&file);
                assert_eq!(sim.files[&file].len(), whole.len() as u64, "seed {seed}");
                let (at, len) = (sim.rng.below(25), sim.rng.below(25) as usize);
                let from = (at as usize).min(whole.len());
                assert_eq!(
                    sim.read(&file, at, len).unwrap(),
                    whole[from..(from + len).min(whole.len())],
                    "seed {seed}"
                );
            }
        }
    }

    /// A write at an offset over synced bytes is lost or kept, maybe torn, by a crash, never
    /// anything else; once synced it is kept. Rename and mode take effect at once.
    #[test]
    fn a_write_at_an_offset_is_durable_after_its_sync_on_every_seed() {
        let file = FileName::new("out/app").unwrap();
        for seed in 0..300 {
            let mut sim = Sim::new(seed, Faults::default());
            let mut effects =
                vec![Effect::Write { file: file.clone(), bytes: b"aaaa".to_vec(), tag: Tag(0) }];
            run_until_idle(&mut sim, &mut effects, |_, _| {}).expect("the effects end");
            let synced = seed % 2 == 0;
            let mut effects = vec![Effect::WriteAt {
                file: file.clone(),
                at: 2,
                bytes: b"bbbb".to_vec(),
                tag: Tag(1),
            }];
            run_until_idle(&mut sim, &mut effects, |it, _| assert_eq!(it.done, Done::Written))
            .expect("the effects end");
            assert_eq!(sim.contents(&file), b"aabbbb");
            if synced {
                let mut effects = vec![Effect::Sync { file: file.clone(), tag: Tag(2) }];
                run_until_idle(&mut sim, &mut effects, |_, _| {}).expect("the effects end");
            }
            sim.crash();
            let after = sim.contents(&file);
            if synced {
                assert_eq!(after, b"aabbbb", "seed {seed}");
            } else {
                // Its one sector landed or not, the length anywhere from the old to the new:
                // the new bytes to that length, or the old with zeros past their end.
                let len = after.len();
                assert!((4..=6).contains(&len), "seed {seed}: {after:?}");
                let mut old = b"aaaa".to_vec();
                old.resize(len, 0);
                assert!(after == b"aabbbb"[..len] || after == old, "seed {seed}: {after:?}");
            }
        }
        let mut sim = Sim::new(1, Faults::default());
        let to = FileName::new("out/app.new").unwrap();
        let mut effects =
            vec![Effect::Write { file: to.clone(), bytes: b"new".to_vec(), tag: Tag(0) }];
        run_until_idle(&mut sim, &mut effects, |_, _| {}).expect("the effects end");
        let mut effects = vec![Effect::Mode { file: to.clone(), mode: 0o755, tag: Tag(1) }];
        run_until_idle(&mut sim, &mut effects, |it, _| assert_eq!(it.done, Done::Moded))
        .expect("the effects end");
        let mut effects = vec![Effect::Rename { from: to.clone(), to: file.clone(), tag: Tag(2) }];
        run_until_idle(&mut sim, &mut effects, |it, _| assert_eq!(it.done, Done::Renamed))
        .expect("the effects end");
        sim.crash();
        assert_eq!(
            (sim.contents(&file), sim.mode(&file), sim.exists(&to)),
            (b"new".to_vec(), Some(0o755), false),
        );
        let mut effects = vec![Effect::WriteAt { file: to, at: 0, bytes: vec![1], tag: Tag(3) }];
        run_until_idle(&mut sim, &mut effects, |it, _| assert!(matches!(it.done, Done::Failed(_))))
        .expect("the effects end");
    }

    /// What a test took from the user, or a mode that lets others write, is found by a check of
    /// the directory, the first by name, and what was set comes before the mode; anything
    /// outside the directory is not looked at.
    #[test]
    fn a_private_directory_names_the_first_that_is_not_the_users_alone() {
        let mut sim = Sim::new(1, Faults::default());
        let name = |it: &str| FileName::new(it).unwrap();
        let (store, a, b) = (name("store"), name("store/a"), name("store/b"));
        let check = |sim: &mut Sim| {
            let mut found = None;
            let mut effects = vec![Effect::Private { dir: store.clone(), tag: Tag(9) }];
            run_until_idle(sim, &mut effects, |it, _| found = Some(it.done))
            .expect("the effects end");
            found.expect("the check completed")
        };
        let mut effects = vec![
            Effect::Write { file: a.clone(), bytes: b"a".to_vec(), tag: Tag(0) },
            Effect::Write { file: b.clone(), bytes: b"b".to_vec(), tag: Tag(1) },
            Effect::Write { file: name("other/c"), bytes: b"c".to_vec(), tag: Tag(2) },
        ];
        run_until_idle(&mut sim, &mut effects, |_, _| {}).expect("the effects end");
        assert_eq!(check(&mut sim), Done::Private(None));
        sim.chmod(&name("other/c"), 0o666);
        sim.set_access(&name("other"), Access::OtherUser);
        assert_eq!(check(&mut sim), Done::Private(None));
        sim.chmod(&b, 0o646);
        assert_eq!(check(&mut sim), Done::Private(Some((b.clone(), Access::OthersWrite))));
        sim.set_access(&b, Access::Unknown);
        assert_eq!(check(&mut sim), Done::Private(Some((b.clone(), Access::Unknown))));
        sim.set_access(&a, Access::OtherUser);
        assert_eq!(check(&mut sim), Done::Private(Some((a.clone(), Access::OtherUser))));
        sim.set_access(&store, Access::OthersWrite);
        assert_eq!(check(&mut sim), Done::Private(Some((store.clone(), Access::OthersWrite))));
        sim.set_access(&store, Access::Alone);
        sim.set_access(&a, Access::Alone);
        assert_eq!(check(&mut sim), Done::Private(Some((b, Access::Unknown))));
    }

    /// A file's times set: its stamp's mtime is the time given and its change time moves, its
    /// inode, bytes and mode do not; a stat and a listing read the time, a crash keeps it, and
    /// a later write moves it on. A name not there, never made or renamed away, fails naming
    /// it, and changes nothing. On every seed, with failures injected: one that fails leaves
    /// the time as it was.
    #[test]
    fn times_set_the_mtime_a_stat_reads_on_every_seed() {
        let (file, moved) = (name("target/build/output"), name("target/moved"));
        for seed in 0..200 {
            let mut sim = Sim::new(seed, Faults { fail_one_in: 4, ..Faults::default() });
            sim.change(&file, b"cargo::rerun-if-changed=build.rs\n");
            let before = sim.stamp(&file);
            let at = 1_700_000_000_123_456_789 + seed;
            let mut done = None;
            let mut effects =
                vec![Effect::Times { file: file.clone(), modified_ns: at, tag: Tag(1) }];
            run_until_idle(&mut sim, &mut effects, |it, _| done = Some(it.done))
            .expect("the effects end");
            let after = sim.stamp(&file);
            assert_eq!(
                (after.ino, after.size, after.mode),
                (before.ino, before.size, before.mode),
                "seed {seed}"
            );
            match done.expect("completed") {
                Done::Timed => {
                    assert_eq!(after.mtime_ns, at, "seed {seed}");
                    assert!(after.ctime_ns > before.ctime_ns, "seed {seed}");
                }
                Done::Failed(_) => {
                    assert_eq!(after, before, "seed {seed}: a failed set changed the stamp");
                    continue;
                }
                other => panic!("seed {seed}: {other:?}"),
            }
            // From here what fails is what the disk says, which names the file.
            sim.faults.fail_one_in = 0;
            assert_eq!(sim.contents(&file), b"cargo::rerun-if-changed=build.rs\n", "seed {seed}");
            let mut seen = BTreeMap::new();
            let mut effects = vec![
                Effect::Stat { file: file.clone(), tag: Tag(2) },
                Effect::List { dir: name("target").into(), tag: Tag(3) },
            ];
            run_until_idle(&mut sim, &mut effects, |it, _| _ = seen.insert(it.tag.0, it.done))
            .expect("the effects end");
            let Done::Stat(Some(stat)) = seen[&2] else { panic!("seed {seed}: {:?}", seen[&2]) };
            assert_eq!((stat.len, stat.dir, stat.modified_ns), (33, false, at), "seed {seed}");
            let Done::Listed(listed) = &seen[&3] else { panic!("seed {seed}: {:?}", seen[&3]) };
            // A listing and a stat of one file say the same of it, the inode and device too.
            assert!(listed.contains(&(file.clone(), stat)), "seed {seed}");
            sim.crash();
            assert_eq!(
                sim.stamp(&file).mtime_ns,
                at,
                "seed {seed}: the time is the inode's, which a crash keeps"
            );
            let mut seen = BTreeMap::new();
            let mut effects = vec![
                Effect::Times { file: name("target/never-written"), modified_ns: at, tag: Tag(4) },
                Effect::Rename { from: file.clone(), to: moved.clone(), tag: Tag(5) },
            ];
            run_until_idle(&mut sim, &mut effects, |it, effects| {
                if it.tag == Tag(5) {
                    effects.push(Effect::Times { file: file.clone(), modified_ns: 1, tag: Tag(6) });
                }
                seen.insert(it.tag.0, it.done);
            }).expect("the effects end");
            assert!(
                matches!(&seen[&4], Done::Failed(why) if why.0.contains("target/never-written")),
                "seed {seed}: {:?}",
                seen[&4]
            );
            assert_eq!(seen[&5], Done::Renamed, "seed {seed}");
            assert!(
                matches!(&seen[&6], Done::Failed(why) if why.0.contains("target/build/output")),
                "seed {seed}: {:?}",
                seen[&6]
            );
            assert_eq!(sim.stamp(&moved).mtime_ns, at, "seed {seed}: a rename keeps the time");
            assert_eq!(
                sim.stamp(&file),
                crate::Stamp::GONE,
                "seed {seed}: a failed set makes nothing"
            );
            let mut effects =
                vec![Effect::Write { file: moved.clone(), bytes: b"again".to_vec(), tag: Tag(7) }];
            run_until_idle(&mut sim, &mut effects, |it, _| {
                assert_eq!(it.done, Done::Written, "seed {seed}")
            }).expect("the effects end");
            assert_ne!(sim.stamp(&moved).mtime_ns, at, "seed {seed}: a write moves the mtime on");
        }
    }

    /// A whole-file write is all or nothing over a crash at any point, on every seed.
    #[test]
    fn a_write_is_whole_or_not_there_after_a_crash() {
        for seed in 0..2_000 {
            let mut sim = Sim::new(seed, Faults { fail_one_in: 5, ..Faults::default() });
            let file = name("store/index");
            sim.change(&file, b"old contents");
            sim.submit(&mut vec![Effect::Write {
                file: file.clone(),
                bytes: b"new".to_vec(),
                tag: Tag(1),
            }]);
            let mut out = Vec::new();
            if sim.rng().one_in(2) {
                sim.poll(&mut out, Deadline::NEVER);
            }
            let acknowledged = out.iter().any(|it| it.done == Done::Written);
            sim.crash();
            let left = sim.contents(&file);
            match acknowledged {
                true => assert_eq!(left, b"new", "seed {seed}"),
                false => assert!(left == b"new" || left == b"old contents", "seed {seed}"),
            }
        }
    }

    /// A whole-file write that waited for no sync, completed, is after a crash the old file,
    /// the new one whole, or the new one torn by sectors (each sector the new bytes or a hole);
    /// never anything else, and on some seed each of the three. Synced after, it is the new.
    #[test]
    fn an_unsynced_write_is_old_new_or_torn_after_a_crash() {
        let new: Vec<u8> = (0..4 * SECTOR).map(|n| (n / SECTOR) as u8 + 1).collect();
        let (mut olds, mut news, mut torns, mut named_lost) = (0, 0, 0, 0);
        for seed in 0..2_000 {
            let mut sim = Sim::new(seed, Faults::default());
            let file = name("store/placed/value");
            sim.change(&file, b"old contents");
            let write = Effect::WriteUnsynced {
                file: file.clone(),
                bytes: new.clone(), // clone: a test's bytes, once a seed.
                tag: Tag(1),
            };
            assert_eq!(ran(&mut sim, vec![write])[&Tag(1)], Done::Written, "seed {seed}");
            assert_eq!(sim.contents(&file), new, "seed {seed}: read whole before a crash");
            // Synced: its bytes by the file's sync, its name by its directory's. A file's
            // sync alone keeps the bytes and not the name (the strict rule).
            let synced = seed % 4 == 0;
            let bytes_only = seed % 4 == 1;
            if synced || bytes_only {
                ran(&mut sim, vec![Effect::Sync { file: file.clone(), tag: Tag(2) }]);
            }
            if synced {
                let sync = Effect::SyncDir { dir: name("store/placed").into(), tag: Tag(3) };
                ran(&mut sim, vec![sync]);
            }
            sim.crash();
            let left = sim.contents(&file);
            if left == new {
                news += 1;
            } else if left == b"old contents" {
                assert!(!synced, "seed {seed}: a synced write was undone");
                if bytes_only {
                    named_lost += 1;
                }
                olds += 1;
            } else {
                assert!(!synced && !bytes_only, "seed {seed}: a synced write was torn");
                assert!(left.len() <= new.len(), "seed {seed}");
                for (n, sector) in left.chunks(SECTOR).enumerate() {
                    let kept = &new[n * SECTOR..n * SECTOR + sector.len()];
                    assert!(
                        sector == kept || sector.iter().all(|it| *it == 0),
                        "seed {seed}: sector {n} is neither the new bytes nor a hole"
                    );
                }
                torns += 1;
            }
        }
        assert!(olds > 0 && news > 0 && torns > 0, "{olds} old, {news} new, {torns} torn");
        assert!(named_lost > 0, "a file's sync without its directory's never lost the name");
    }

    /// A clone of a file with bytes written since its last write-back is charged those bytes
    /// as forced out, once: the source is clean after, so a second clone forces none. A
    /// write-back completed first, or a sync, makes the clone free; a write after either is
    /// dirty again. Nothing a write-back covers is durable: a crash still tears it.
    #[test]
    fn a_clone_of_a_dirty_file_is_charged_its_forced_write_back() {
        let mut sim = Sim::new(9, Faults::default());
        let (file, bytes) = (name("store/placed/value"), vec![7u8; 3 * SECTOR]);
        // clone: a test's bytes.
        let write = Effect::WriteUnsynced { file: file.clone(), bytes: bytes.clone(), tag: Tag(1) };
        assert_eq!(ran(&mut sim, vec![write])[&Tag(1)], Done::Written);
        let clone = |to: &str, tag| Effect::Clone { from: file.clone(), to: name(to), tag: Tag(tag) };
        assert_eq!(ran(&mut sim, vec![clone("out/a", 2)])[&Tag(2)], Done::Cloned);
        assert_eq!(sim.bytes_forced(), 3 * SECTOR as u64, "the clone forced the whole write out");
        assert_eq!(ran(&mut sim, vec![clone("out/b", 3)])[&Tag(3)], Done::Cloned);
        assert_eq!(sim.bytes_forced(), 3 * SECTOR as u64, "a second clone of it forces nothing");
        // Written again, then written back: clean, so the clone after is free.
        let at = Effect::WriteAt { file: file.clone(), at: 0, bytes: vec![1; 100], tag: Tag(4) };
        ran(&mut sim, vec![at]);
        let back = Effect::WriteBack { file: file.clone(), tag: Tag(5) };
        assert_eq!(ran(&mut sim, vec![back])[&Tag(5)], Done::WrittenBack);
        assert_eq!(ran(&mut sim, vec![clone("out/c", 6)])[&Tag(6)], Done::Cloned);
        assert_eq!(sim.bytes_forced(), 3 * SECTOR as u64);
        // A write after the write-back is dirty, and only it is charged.
        let at = Effect::WriteAt { file: file.clone(), at: 0, bytes: vec![2; 40], tag: Tag(7) };
        ran(&mut sim, vec![at]);
        assert_eq!(ran(&mut sim, vec![clone("out/d", 8)])[&Tag(8)], Done::Cloned);
        assert_eq!(sim.bytes_forced(), 3 * SECTOR as u64 + 40);
        // A write-back of a file that is not there fails, naming it.
        let missing = Effect::WriteBack { file: name("store/placed/none"), tag: Tag(9) };
        assert!(matches!(&ran(&mut sim, vec![missing])[&Tag(9)], Done::Failed(why) if why.0.contains("none")));
    }

    /// A rename that waited for no sync is, after a crash, done (the file under the new name,
    /// the old name gone) or undone (the file back under its old name, the new name holding
    /// what it held), on some seed each. A synced rename is always done.
    #[test]
    fn an_unsynced_rename_is_done_or_undone_after_a_crash() {
        let (mut done, mut undone) = (0, 0);
        for seed in 0..500 {
            let mut sim = Sim::new(seed, Faults::default());
            let (from, to) = (name("out/.staged"), name("out/program"));
            sim.change(&from, b"new");
            sim.change(&to, b"old");
            let synced = seed % 4 == 0;
            let rename = match synced {
                true => Effect::Rename { from: from.clone(), to: to.clone(), tag: Tag(1) },
                false => Effect::RenameUnsynced { from: from.clone(), to: to.clone(), tag: Tag(1) },
            };
            assert_eq!(ran(&mut sim, vec![rename])[&Tag(1)], Done::Renamed, "seed {seed}");
            assert_eq!((sim.contents(&to), sim.exists(&from)), (b"new".to_vec(), false));
            sim.crash();
            match (sim.contents(&to).as_slice(), sim.exists(&from)) {
                (b"new", false) => done += 1,
                (b"old", true) => {
                    assert!(!synced, "seed {seed}: a synced rename was undone");
                    assert_eq!(sim.contents(&from), b"new", "seed {seed}");
                    undone += 1;
                }
                other => panic!("seed {seed}: a rename left {other:?}"),
            }
        }
        assert!(done > 0 && undone > 0, "{done} done, {undone} undone");
    }

    /// A publish that renames a synced file into place without waiting, then syncs the new
    /// name's directory (`fsync` of it; `F_FULLFSYNC` on Apple's systems), is kept by every
    /// crash: the directory's sync makes the rename durable.
    #[test]
    fn a_rename_kept_by_its_directorys_sync_survives_a_crash() {
        for seed in 0..500 {
            let mut sim = Sim::new(seed, Faults::default());
            let (from, to) = (name("out/.staged"), name("out/program"));
            sim.change(&from, b"new");
            sim.change(&to, b"old");
            let rename = Effect::RenameUnsynced { from: from.clone(), to: to.clone(), tag: Tag(1) };
            assert_eq!(ran(&mut sim, vec![rename])[&Tag(1)], Done::Renamed, "seed {seed}");
            let sync = Effect::SyncDir { dir: name("out").into(), tag: Tag(2) };
            assert_eq!(ran(&mut sim, vec![sync])[&Tag(2)], Done::Synced, "seed {seed}");
            sim.crash();
            assert_eq!(
                (sim.contents(&to).as_slice(), sim.exists(&from)),
                (&b"new"[..], false),
                "seed {seed}: an unsynced rename was undone after its directory was synced"
            );
        }
    }

    /// One publish of `new` to `out/program`: written to a staged name, renamed into place
    /// without a wait, its bytes synced under the new name, and its directory synced unless
    /// `dir_sync` is false (the planted bug: the publish that leaves the directory's sync
    /// out). After a crash, whether the program is the new one.
    fn publish(sim: &mut Sim, dir_sync: bool) -> bool {
        let (staged, program) = (name("out/.staged"), name("out/program"));
        sim.change(&program, b"old");
        let mut asked = vec![
            Effect::Append { file: staged.clone(), bytes: b"new".to_vec(), tag: Tag(1) },
            Effect::RenameUnsynced { from: staged.clone(), to: program.clone(), tag: Tag(2) },
            Effect::Sync { file: program.clone(), tag: Tag(3) },
        ];
        if dir_sync {
            asked.push(Effect::SyncDir { dir: name("out").into(), tag: Tag(4) });
        }
        // One at a time, each after the last completed, as a publish asks them.
        for effect in asked {
            let tag = effect.tag();
            assert!(!matches!(ran(sim, vec![effect])[&tag], Done::Failed(_)));
        }
        sim.crash();
        sim.contents(&program) == b"new"
    }

    /// The strict rule's control: a publish that syncs the file and never the directory is
    /// caught by a crash (the old program is back on some seed), and the same publish with
    /// the directory's sync is kept on every seed. The loose rule planted (a file's sync keeps
    /// its name) hides the missing sync: the control is what tells the two apart.
    #[test]
    fn a_publish_without_its_directorys_sync_is_caught_by_a_crash() {
        let (mut lost, mut lost_loose) = (0, 0);
        for seed in 0..500 {
            assert!(publish(&mut Sim::new(seed, Faults::default()), true), "seed {seed}: a synced publish was lost");
            if !publish(&mut Sim::new(seed, Faults::default()), false) {
                lost += 1;
            }
            let mut loose = Sim::new(seed, Faults::default());
            loose.plant(host::Plant::FileSyncKeepsName);
            if !publish(&mut loose, false) {
                lost_loose += 1;
            }
        }
        assert!(lost > 0, "no crash lost a publish whose directory was never synced");
        assert_eq!(lost_loose, 0, "under the loose rule the missing directory sync is hidden");
    }

    #[test]
    fn a_listing_names_files_and_the_directories_they_imply() {
        let mut sim = Sim::new(1, Faults::default());
        sim.change(&name("src/a/b/c.rs"), b"12");
        sim.change(&name("src/a/d.rs"), b"1");
        sim.change(&name("src/e"), b"");
        sim.change(&name("other"), b"x");
        let Done::Listed(listed) = sim.list(&name("src").into()) else { panic!() };
        let shown: Vec<(&str, bool, u64)> =
            listed.iter().map(|(file, meta)| (file.as_str(), meta.dir, meta.len)).collect();
        assert_eq!(
            shown,
            [
                ("src/a", true, 0),
                ("src/a/b", true, 0),
                ("src/a/b/c.rs", false, 2),
                ("src/a/d.rs", false, 1),
                ("src/e", false, 0)
            ]
        );
        assert!(matches!(sim.list(&name("other").into()), Done::Failed(_)));
        assert!(matches!(sim.list(&name("none").into()), Done::Failed(_)));
        // The root is always there and lists every name.
        let Done::Listed(all) = sim.list(&crate::DirName::Root) else { panic!() };
        assert_eq!(all.iter().filter(|(_, meta)| !meta.dir).count(), 4);
    }

    #[test]
    fn watches_hear_changes_under_their_directory_then_their_end() {
        for seed in 0..500 {
            let mut sim = Sim::new(seed, Faults { lose_change_one_in: 7, ..Faults::default() });
            let mut effects = vec![Effect::Watch { dir: name("src"), tag: Tag(9) }];
            sim.submit(&mut effects);
            sim.change(&name("src/lib.rs"), b"fn main() {}");
            sim.change(&name("target/out"), b"not watched");
            effects.push(Effect::Write { file: name("src/a/b.rs"), bytes: vec![1], tag: Tag(1) });
            effects.push(Effect::Unwatch { tag: Tag(9) });
            let mut heard = Vec::new();
            let mut lost = false;
            let mut ended = false;
            run_until_idle(&mut sim, &mut effects, |done, _| match done.done {
                Done::Changed { files, lost: was_lost } => {
                    assert!(!ended, "seed {seed}: a change after the end");
                    lost |= was_lost;
                    heard.extend(files);
                }
                Done::Unwatched => ended = true,
                _ => {}
            }).expect("the effects end");
            assert!(ended, "seed {seed}");
            assert!(!heard.contains(&name("target/out")), "seed {seed}");
            if !lost {
                assert!(heard.contains(&name("src/lib.rs")), "seed {seed}: {heard:?}");
            }
        }
    }

    /// Changes made while a watch is ending are told on some seeds, as a system that queued
    /// them before it saw the unwatch does: one due after the end names a watch gone, and is
    /// dropped, never delivered after the end. The race happens on some seeds and not others.
    #[test]
    fn a_change_racing_an_unwatch_is_stale_and_dropped() {
        let (mut raced, mut clean) = (0, 0);
        for seed in 0..500 {
            let mut sim = Sim::new(seed, Faults::default());
            sim.submit(&mut vec![Effect::Watch { dir: name("src"), tag: Tag(9) }]);
            sim.change(&name("src/before"), b"1");
            sim.submit(&mut vec![Effect::Unwatch { tag: Tag(9) }]);
            for n in 0..8 {
                sim.change(&name(&format!("src/during-{n}")), b"2");
            }
            let mut ended = false;
            let mut effects = Vec::new();
            run_until_idle(&mut sim, &mut effects, |done, _| match done.done {
                Done::Changed { .. } => assert!(!ended, "seed {seed}: a change after the end"),
                Done::Unwatched => ended = true,
                other => panic!("seed {seed}: {other:?}"),
            }).expect("the effects end");
            assert!(ended && sim.in_flight() == 0, "seed {seed}");
            // Changes told for after the end are not in flight: each is due, and dropped.
            let mut out = Vec::new();
            while !sim.told.is_empty() {
                sim.poll(&mut out, Deadline::NEVER);
                assert!(out.is_empty(), "seed {seed}: {out:?} after the end");
            }
            match sim.stale() {
                0 => clean += 1,
                _ => raced += 1,
            }
            // A watch of the same tag again is a new one: the old end's handle names nothing.
            sim.submit(&mut vec![Effect::Watch { dir: name("src"), tag: Tag(9) }]);
            sim.change(&name("src/again"), b"3");
            let mut heard = Vec::new();
            sim.submit(&mut vec![Effect::Unwatch { tag: Tag(9) }]);
            run_until_idle(&mut sim, &mut effects, |done, _| heard.push(done.done))
            .expect("the effects end");
            assert_eq!(heard.last(), Some(&Done::Unwatched), "seed {seed}");
        }
        assert!(raced > 0 && clean > 0, "raced on {raced} seeds, clean on {clean}");
    }

    #[test]
    fn a_flush_is_told_after_every_change_made_before_it() {
        for seed in 0..500 {
            let mut sim = Sim::new(seed, Faults::default());
            let mut effects = vec![Effect::Watch { dir: name("src"), tag: Tag(9) }];
            sim.submit(&mut effects);
            sim.change(&name("src/a"), b"1");
            sim.change(&name("src/b"), b"2");
            effects.push(Effect::Flush { watch: Tag(9), tag: Tag(10) });
            effects.push(Effect::Flush { watch: Tag(8), tag: Tag(11) });
            effects.push(Effect::Unwatch { tag: Tag(9) });
            let (mut heard, mut at_flush, mut refused, mut ended) =
                (Vec::new(), None, false, false);
            run_until_idle(&mut sim, &mut effects, |done, _| match (done.tag.0, done.done) {
                (9, Done::Changed { files, .. }) => {
                    assert!(!ended, "seed {seed}: a change after the end");
                    heard.extend(files);
                }
                (9, Done::Unwatched) => ended = true,
                (10, Done::Flushed) => {
                    assert!(!ended, "seed {seed}: the flush after the end");
                    let mut seen = heard.clone();
                    seen.sort();
                    at_flush = Some(seen);
                }
                (11, Done::Failed(_)) => refused = true,
                other => panic!("seed {seed}: {other:?}"),
            }).expect("the effects end");
            assert_eq!(at_flush, Some(vec![name("src/a"), name("src/b")]), "seed {seed}");
            assert!(refused && ended, "seed {seed}");
            assert_eq!(sim.in_flight(), 0);
        }
    }

    #[test]
    fn a_file_system_that_cannot_share_blocks_refuses_a_clone() {
        let mut sim = Sim::new(3, Faults { no_clone: true, ..Faults::default() });
        sim.change(&name("store/v"), b"code");
        let mut effects = vec![
            Effect::Clone { from: name("store/v"), to: name("target/a"), tag: Tag(1) },
            Effect::Link { from: name("store/v"), to: name("target/b"), tag: Tag(2) },
        ];
        let mut seen = BTreeMap::new();
        run_until_idle(&mut sim, &mut effects, |done, _| _ = seen.insert(done.tag, done.done))
        .expect("the effects end");
        assert!(matches!(&seen[&Tag(1)], Done::Failed(why) if why.0.contains("does not copy")));
        assert_eq!(seen[&Tag(2)], Done::Linked);
        assert!(!sim.exists(&name("target/a")));
        assert_eq!(sim.contents(&name("target/b")), b"code");
    }

    /// The invariant: a hard link is a second name of one inode: both names' looks give the
    /// same `ino`, mtime and change time, the link moved the change time and not the mtime,
    /// and a rename of either name moves the change time both looks see. The control: a
    /// clone of the same file is an inode of its own.
    #[test]
    fn a_link_is_a_second_name_of_one_inode() {
        let mut sim = Sim::new(5, Faults::default());
        sim.change(&name("store/v"), b"code");
        let before = sim.stamp(&name("store/v"));
        let mut effects = vec![
            Effect::Link { from: name("store/v"), to: name("target/b"), tag: Tag(1) },
            Effect::Clone { from: name("store/v"), to: name("target/c"), tag: Tag(2) },
        ];
        run_until_idle(&mut sim, &mut effects, |_, _| {}).expect("the effects end");
        let (from, to) = (sim.stamp(&name("store/v")), sim.stamp(&name("target/b")));
        assert_eq!((from.ino, from.mtime_ns, from.ctime_ns), (to.ino, to.mtime_ns, to.ctime_ns));
        assert_eq!(from.mtime_ns, before.mtime_ns, "a link moves no mtime");
        assert!(from.ctime_ns > before.ctime_ns, "a link moves the change time");
        assert_ne!(sim.stamp(&name("target/c")).ino, from.ino, "the control: a clone is its own inode");
        sim.rename(&name("target/b"), &name("target/d"));
        let (from, to) = (sim.stamp(&name("store/v")), sim.stamp(&name("target/d")));
        assert_eq!((from.ino, from.ctime_ns), (to.ino, to.ctime_ns), "the rename's change time is both names'");
        assert!(to.ctime_ns > before.ctime_ns);
    }

    fn command(program: &str, memory: Option<u64>) -> Box<Command> {
        Box::new(Command {
            program: program.into(),
            args: vec!["x".into()],
            env: Vec::new(),
            dir: "/".into(),
            stdin: b"input".to_vec(),
            sandbox: crate::Sandbox::On {
                read: Vec::new(),
                write: Vec::new(),
                network: false,
                trace: true,
                services: Vec::new(),
            },
            memory,
        })
    }

    fn echo(sim: &mut Sim) {
        sim.program("/bin/echo", |command: &Command, rng: &mut Rng| Ran {
            exit: Exit {
                code: Some(0),
                signal: None,
                stdout: command.stdin.clone(),
                stderr: Vec::new(),
                traced: Some(crate::Traced::default()),
                peak_memory: 1000 + rng.below(1000),
            },
            takes: Duration::from_millis(5),
        });
    }

    #[test]
    fn a_spawn_ends_as_its_program_says_or_is_killed() {
        for seed in 0..300 {
            let mut sim = Sim::new(seed, Faults::default());
            echo(&mut sim);
            let mut effects = vec![
                Effect::Spawn { command: command("/bin/echo", None), tag: Tag(1) },
                Effect::Spawn { command: command("/bin/echo", Some(500)), tag: Tag(2) },
                Effect::Spawn { command: command("/bin/missing", None), tag: Tag(3) },
                Effect::Spawn { command: command("/bin/echo", None), tag: Tag(4) },
                Effect::Kill { tag: Tag(4) },
                Effect::Kill { tag: Tag(99) },
            ];
            let mut seen = BTreeMap::new();
            run_until_idle(&mut sim, &mut effects, |done, _| _ = seen.insert(done.tag, done.done))
            .expect("the effects end");
            assert_eq!(sim.in_flight(), 0);
            let Done::Exited(echoed) = &seen[&Tag(1)] else { panic!("seed {seed}: {seen:?}") };
            assert_eq!((echoed.code, echoed.stdout.as_slice()), (Some(0), &b"input"[..]));
            assert!(echoed.traced.is_some());
            let Done::Exited(over) = &seen[&Tag(2)] else { panic!("seed {seed}: {seen:?}") };
            assert_eq!((over.code, over.signal, over.peak_memory), (None, Some(KILLED), 500));
            assert!(matches!(&seen[&Tag(3)], Done::Failed(why) if why.0.contains("no such")));
            let Done::Exited(killed) = &seen[&Tag(4)] else { panic!("seed {seed}: {seen:?}") };
            assert_eq!(killed.signal, Some(KILLED));
            assert!(!seen.contains_key(&Tag(99)));
            // A kill once the spawn has ended names nothing: nothing more is told.
            sim.submit(&mut vec![Effect::Kill { tag: Tag(1) }]);
            assert_eq!(sim.in_flight(), 0);
        }
    }

    /// Each timer fires at exactly its virtual time, the clock jumping to it, whatever else is
    /// in flight; one far past the clock's end fires at its end.
    #[test]
    fn timers_fire_at_their_virtual_time() {
        for seed in 0..100 {
            let mut sim = Sim::new(seed, Faults::default());
            let mut at = Vec::new();
            sim.submit(&mut vec![
                Effect::Timer { after: Duration::from_micros(7_000), tag: Tag(0) },
                Effect::Timer { after: Duration::from_micros(3_000), tag: Tag(1) },
                Effect::Timer { after: Duration::from_micros(3_000), tag: Tag(2) },
                Effect::Timer { after: Duration::from_micros(12), tag: Tag(3) },
                Effect::Timer { after: Duration::MAX, tag: Tag(4) },
                Effect::Append { file: name("f"), bytes: vec![1], tag: Tag(5) },
            ]);
            let mut out = Vec::new();
            while sim.in_flight() > 0 {
                sim.poll(&mut out, Deadline::NEVER);
                for done in out.drain(..) {
                    if done.done == Done::Fired {
                        at.push((sim.now(), done.tag.0));
                    }
                }
            }
            at.sort_unstable();
            let micros = |it| Duration::from_micros(it);
            assert_eq!(
                at,
                [
                    (micros(12), 3),
                    (micros(3_000), 1),
                    (micros(3_000), 2),
                    (micros(7_000), 0),
                    (Duration::from_nanos(u64::MAX), 4),
                ],
                "seed {seed}"
            );
        }
    }

    #[test]
    fn a_kill_comes_before_the_end_it_cuts_short() {
        let mut sim = Sim::new(5, Faults::default());
        echo(&mut sim);
        sim.submit(&mut vec![Effect::Spawn { command: command("/bin/echo", None), tag: Tag(1) }]);
        sim.submit(&mut vec![Effect::Kill { tag: Tag(1) }]);
        let mut out = Vec::new();
        sim.poll(&mut out, Deadline::NEVER);
        assert!(sim.now() < Duration::from_millis(5), "{:?}", sim.now());
        assert!(
            matches!(&out[..], [Completion { done: Done::Exited(it), .. }] if it.signal == Some(KILLED))
        );
    }

    /// A client and a server in one simulation, and a peer from outside, trade bytes and hang
    /// up; every receive ends, with bytes or with nothing once the other end hung up.
    #[test]
    fn sockets_carry_bytes_and_hang_ups_on_every_seed() {
        let at = Address::Unix(name("daemon.sock"));
        for seed in 0..500 {
            let mut sim = Sim::new(seed, Faults::default());
            let mut effects = vec![Effect::Listen { at: at.clone(), options: crate::sockopt::Tcp::DEFAULT, tag: Tag(1) }];
            sim.submit(&mut effects);
            effects.push(Effect::Connect { at: at.clone(), options: crate::sockopt::Tcp::DEFAULT, tag: Tag(2) });
            let outside = sim.dial(&at).unwrap();
            sim.peer_send(outside, b"from outside");
            let mut client = None;
            // What each server end received, and whether it has heard the hang-up.
            let mut heard: BTreeMap<Conn, (Vec<u8>, bool)> = BTreeMap::new();
            let mut receiving: BTreeMap<Tag, Conn> = BTreeMap::new();
            let (mut next, mut outside_hung_up, mut unlistened) = (100, false, false);
            let mut out = Vec::new();
            while !effects.is_empty() || sim.in_flight() > 0 {
                sim.submit(&mut effects);
                sim.poll(&mut out, Deadline::NEVER);
                for done in out.drain(..) {
                    next += 1;
                    match done.done {
                        Done::Accepted { conn } => {
                            heard.insert(conn, (Vec::new(), false));
                            receiving.insert(Tag(next), conn);
                            effects.push(Effect::Recv {
                                conn,
                                most: 4,
                                into: Vec::new(),
                                tag: Tag(next),
                            });
                            if conn == outside {
                                let bytes = b"reply".to_vec();
                                effects.push(Effect::Send { conn, bytes, tag: Tag(5) });
                            }
                        }
                        Done::Connected { conn } => {
                            client = Some(conn);
                            let bytes = b"hello".to_vec();
                            effects.push(Effect::Send { conn, bytes, tag: Tag(3) });
                        }
                        Done::Sent(bytes) if done.tag == Tag(3) => {
                            assert!(
                                bytes.is_empty() && bytes.capacity() >= 5,
                                "seed {seed}: {bytes:?}"
                            );
                            let conn = client.expect("connected");
                            effects.push(Effect::Hangup { conn, tag: Tag(4) });
                        }
                        Done::Read(mut bytes) => {
                            let conn = receiving.remove(&done.tag).expect("a receive's");
                            let (got, ended) = heard.get_mut(&conn).expect("accepted");
                            match bytes.is_empty() {
                                true => *ended = true,
                                false => {
                                    assert!(bytes.len() <= 4, "seed {seed}");
                                    got.extend_from_slice(&bytes);
                                    // The buffer handed back is the next receive's.
                                    bytes.clear();
                                    receiving.insert(Tag(next), conn);
                                    effects.push(Effect::Recv {
                                        conn,
                                        most: 4,
                                        into: bytes,
                                        tag: Tag(next),
                                    });
                                }
                            }
                        }
                        Done::Unlistened => unlistened = true,
                        Done::Sent(_) | Done::HungUp => {}
                        other => panic!("seed {seed}: {other:?}"),
                    }
                }
                if !outside_hung_up && heard.get(&outside).is_some_and(|it| it.0.len() == 12) {
                    outside_hung_up = true;
                    sim.peer_hangup(outside);
                }
                if heard.len() == 2 && heard.values().all(|it| it.1) && !unlistened {
                    unlistened = true;
                    effects.push(Effect::Unlisten { tag: Tag(1) });
                }
            }
            assert_eq!(heard[&outside].0, b"from outside", "seed {seed}");
            let inside = heard.keys().find(|&&it| it != outside).expect("the client's end");
            assert_eq!(heard[inside].0, b"hello", "seed {seed}");
            assert_eq!(sim.peer_received(outside), b"reply", "seed {seed}");
            assert_eq!(sim.in_flight(), 0, "seed {seed}");
        }
    }

    /// The bytes a receive got, told after a delay, on an end the state machine hung up before
    /// they were due name an end gone: the receive is told with nothing appended to what its
    /// buffer kept, as a receive in flight at a hang-up is. A receive after the hang-up names
    /// nothing and fails, its buffer back. On some seeds the bytes are due first and arrive
    /// whole, after what the buffer kept.
    #[test]
    fn a_hang_up_racing_a_receive_tells_it_empty() {
        let at = Address::Unix(name("s.sock"));
        let (mut raced, mut clean) = (0, 0);
        for seed in 0..300 {
            let mut sim = Sim::new(seed, Faults::default());
            sim.submit(&mut vec![Effect::Listen { at: at.clone(), options: crate::sockopt::Tcp::DEFAULT, tag: Tag(1) }]);
            let conn = sim.dial(&at).unwrap();
            let mut seen = BTreeMap::new();
            let mut out = Vec::new();
            while !seen.contains_key(&Tag(1)) {
                sim.poll(&mut out, Deadline::NEVER);
                seen.extend(out.drain(..).map(|it| (it.tag, it.done)));
            }
            sim.peer_send(conn, b"bytes");
            sim.submit(&mut vec![Effect::Recv {
                conn,
                most: 16,
                into: b"kept:".to_vec(),
                tag: Tag(2),
            }]);
            if sim.rng().one_in(2) {
                while !seen.contains_key(&Tag(2)) {
                    sim.poll(&mut out, Deadline::NEVER);
                    seen.extend(out.drain(..).map(|it| (it.tag, it.done)));
                }
            }
            let mut effects = vec![
                Effect::Hangup { conn, tag: Tag(3) },
                Effect::Recv { conn, most: 16, into: b"again".to_vec(), tag: Tag(4) },
                Effect::Unlisten { tag: Tag(1) },
            ];
            run_until_idle(&mut sim, &mut effects, |it, _| _ = seen.insert(it.tag, it.done))
            .expect("the effects end");
            assert_eq!(seen[&Tag(3)], Done::HungUp, "seed {seed}");
            assert!(
                matches!(&seen[&Tag(4)], Done::Broken { buffer, error } if buffer == b"again" && error.0.contains("no connection")),
                "seed {seed}: {:?}",
                seen[&Tag(4)]
            );
            match &seen[&Tag(2)] {
                Done::Read(bytes) if bytes == b"kept:bytes" => clean += 1,
                Done::Read(bytes) if bytes == b"kept:" => raced += 1,
                other => panic!("seed {seed}: {other:?}"),
            }
            assert_eq!(
                sim.stale(),
                u64::from(seen[&Tag(2)] == Done::Read(b"kept:".to_vec())),
                "seed {seed}"
            );
            assert!(sim.peer_hung_up_on(conn));
            sim.peer_hangup(conn);
            assert_eq!(sim.in_flight(), 0, "seed {seed}");
        }
        assert!(raced > 0 && clean > 0, "raced on {raced} seeds, clean on {clean}");
    }

    /// The ends of connections in their slab, named by handle bits, against the map of
    /// connections they replace (an `HashMap<Conn, End>` keyed by a counter, an end gone once
    /// hung up and the peer outside hung up too): the same answers to the same random sends,
    /// hang-ups, peer hang-ups and takes on every seed, stale conns included.
    #[test]
    fn ends_on_a_slab_answer_as_the_map_of_conns_did() {
        #[derive(Default)]
        struct Old {
            closed: bool,
            peer_closed: bool,
            outbox: Vec<u8>,
        }
        /// What `effect` completed with, polling past what else is told.
        fn until(sim: &mut Sim, effect: Effect) -> Done {
            let tag = effect.tag();
            sim.submit(&mut vec![effect]);
            let mut out = Vec::new();
            loop {
                sim.poll(&mut out, Deadline::NEVER);
                if let Some(at) = out.iter().position(|it| it.tag == tag) {
                    return out.swap_remove(at).done;
                }
            }
        }
        let at = Address::Unix(name("s.sock"));
        for seed in 0..100 {
            let mut sim = Sim::new(seed, Faults::default());
            sim.submit(&mut vec![Effect::Listen { at: at.clone(), options: crate::sockopt::Tcp::DEFAULT, tag: Tag(1) }]);
            let mut model: Map<u64, Old, FixedState, Global> = Map::new(MapOpts { hasher: FixedState, alloc: Global, capacity: None });
            let mut conns: Vec<Conn> = Vec::new();
            let mut rng = Rng::new(seed ^ 0x5eed);
            let mut out = Vec::new();
            for step in 0..400u64 {
                let pick = (!conns.is_empty()).then(|| rng.below(conns.len() as u64) as usize);
                match (rng.below(5), pick) {
                    (0, _) | (_, None) => {
                        conns.push(sim.dial(&at).unwrap());
                        model.insert(conns.len() as u64 - 1, Old::default());
                    }
                    (1, Some(at)) => {
                        let tag = Tag(1000 + step);
                        let done = until(
                            &mut sim,
                            Effect::Send { conn: conns[at], bytes: vec![step as u8], tag },
                        );
                        let want = match model.get_mut(&(at as u64)) {
                            Some(old) if !old.closed && !old.peer_closed => {
                                old.outbox.push(step as u8);
                                true
                            }
                            _ => false,
                        };
                        assert_eq!(matches!(done, Done::Sent(_)), want, "seed {seed} step {step}");
                        let (Done::Sent(buffer) | Done::Broken { buffer, .. }) = &done else {
                            panic!("seed {seed} step {step}: {done:?}")
                        };
                        assert!(
                            buffer.is_empty() && buffer.capacity() >= 1,
                            "a send's buffer comes back cleared"
                        );
                    }
                    (2, Some(at)) => {
                        let done = until(
                            &mut sim,
                            Effect::Hangup { conn: conns[at], tag: Tag(1000 + step) },
                        );
                        let want = match model.get_mut(&(at as u64)) {
                            Some(old) if !old.closed => {
                                old.closed = true;
                                if old.peer_closed {
                                    model.remove(&(at as u64));
                                }
                                true
                            }
                            _ => false,
                        };
                        assert_eq!(done == Done::HungUp, want, "seed {seed} step {step}");
                    }
                    (3, Some(at)) => {
                        sim.peer_hangup(conns[at]);
                        if let Some(old) = model.get_mut(&(at as u64)) {
                            old.peer_closed = true;
                            if old.closed {
                                model.remove(&(at as u64));
                            }
                        }
                    }
                    (_, Some(at)) => {
                        let want =
                            model.get_mut(&(at as u64)).map(|old| mem::take(&mut old.outbox));
                        assert_eq!(
                            sim.peer_received(conns[at]),
                            want.unwrap_or_default(),
                            "seed {seed} step {step}"
                        );
                        let gone = model.get(&(at as u64)).is_none_or(|old| old.closed);
                        assert_eq!(sim.peer_hung_up_on(conns[at]), gone, "seed {seed} step {step}");
                    }
                }
                sim.poll(&mut out, Deadline::NEVER);
                out.clear();
            }
            let live = model.len();
            assert_eq!(sim.ends.len(), live, "seed {seed}");
        }
    }

    /// A dial of another user at a Unix listen is refused before any byte of it is read and
    /// the listen is told nothing; the listen goes on, and a dial of its own user next is
    /// accepted and read. The control: the same foreign dial at a TCP listen, which has no uid
    /// to check, is accepted (the check is the Unix socket's, as net.rs `peer_is` is).
    #[test]
    fn a_dial_of_another_user_is_refused_unread_and_the_listen_goes_on() {
        let at = Address::Unix(name("helper.sock"));
        for seed in 0..64 {
            let mut sim = Sim::new(seed, Faults::default());
            sim.submit(&mut vec![Effect::Listen { at: at.clone(), options: crate::sockopt::Tcp::DEFAULT, tag: Tag(1) }]);
            let refused = sim.dial_as(&at, SIM_USER + 1).expect_err("a foreign dial was taken");
            assert!(refused.0.contains(&format!("uid {}", SIM_USER + 1)) && refused.0.contains("unread"), "seed {seed}: {}", refused.0);
            let ours = sim.dial(&at).expect("the listen goes on after a refusal");
            sim.peer_send(ours, b"ask");
            let mut accepted = Vec::new();
            let mut out = Vec::new();
            for _ in 0..64 {
                sim.poll(&mut out, Deadline::NEVER);
                accepted.extend(out.drain(..).filter_map(|it| match it.done {
                    Done::Accepted { conn } => Some(conn),
                    _ => None,
                }));
                if !accepted.is_empty() {
                    break;
                }
            }
            assert_eq!(accepted, vec![ours], "seed {seed}: only the own user's dial reaches the listen");
            let mut seen = BTreeMap::new();
            // The receive alone first, polled to its answer while the listen stays open: a
            // hang-up in the same batch may race it (`a_hang_up_racing_a_receive_tells_it_empty`).
            sim.submit(&mut vec![Effect::Recv { conn: ours, most: 16, into: Vec::new(), tag: Tag(2) }]);
            for _ in 0..64 {
                if seen.contains_key(&Tag(2)) {
                    break;
                }
                sim.poll(&mut out, Deadline::NEVER);
                seen.extend(out.drain(..).map(|it| (it.tag, it.done)));
            }
            assert_eq!(seen.get(&Tag(2)), Some(&Done::Read(b"ask".to_vec())), "seed {seed}");
            let mut effects = vec![Effect::Hangup { conn: ours, tag: Tag(3) }, Effect::Unlisten { tag: Tag(1) }];
            run_until_idle(&mut sim, &mut effects, |it, _| _ = seen.insert(it.tag, it.done)).expect("the effects end");
            sim.peer_hangup(ours);
        }
        let tcp = Address::Tcp("127.0.0.1:8081".parse().unwrap());
        let mut sim = Sim::new(0, Faults::default());
        sim.submit(&mut vec![Effect::Listen { at: tcp.clone(), options: crate::sockopt::Tcp::DEFAULT, tag: Tag(1) }]);
        assert!(sim.dial_as(&tcp, SIM_USER + 1).is_ok(), "a TCP peer has no uid to refuse");
    }

    #[test]
    fn a_connect_to_nothing_and_a_second_listen_fail() {
        let mut sim = Sim::new(2, Faults::default());
        let at = Address::Tcp("127.0.0.1:8080".parse().unwrap());
        let mut effects = vec![
            Effect::Connect { at: at.clone(), options: crate::sockopt::Tcp::DEFAULT, tag: Tag(1) },
            Effect::Listen { at: at.clone(), options: crate::sockopt::Tcp::DEFAULT, tag: Tag(2) },
        ];
        sim.submit(&mut effects);
        effects.push(Effect::Listen { at: at.clone(), options: crate::sockopt::Tcp::DEFAULT, tag: Tag(3) });
        effects.push(Effect::Unlisten { tag: Tag(2) });
        effects.push(Effect::Unlisten { tag: Tag(4) });
        let mut seen = BTreeMap::new();
        run_until_idle(&mut sim, &mut effects, |done, _| _ = seen.insert(done.tag, done.done))
        .expect("the effects end");
        assert!(matches!(&seen[&Tag(1)], Done::Failed(why) if why.0.contains("refused")));
        assert_eq!(seen[&Tag(2)], Done::Unlistened);
        assert!(matches!(&seen[&Tag(3)], Done::Failed(_)));
        assert!(matches!(&seen[&Tag(4)], Done::Failed(_)));
        assert!(sim.dial(&at).is_err());
    }

    /// The invariant: a sync, a barrier and a write-back of a file sealed read-only succeed
    /// in the simulation, as on the real file system (`fs::assert_syncs_a_sealed_file`), and
    /// a sync of a missing file makes it; what opens the sealed file for writing (an append,
    /// a write at an offset, a cut) fails with permission denied, as the kernel refuses a user
    /// who is not root, and changes nothing; a whole-file write, a new file renamed over it,
    /// does not. The control, the cause removed: the same file made writable again takes the
    /// same append. On every seed.
    #[test]
    fn a_sync_of_a_sealed_file_succeeds_and_a_write_of_it_is_refused() {
        let (file, missing) = (name("store/placed/out"), name("store/placed/new"));
        for seed in 0..64 {
            let mut sim = Sim::new(seed, Faults::default());
            sim.change(&file, b"placed");
            let seal = Effect::Seal { file: file.clone(), mode: 0o444, tag: Tag(1) };
            assert_eq!(ran(&mut sim, vec![seal])[&Tag(1)], Done::Moded);
            let seen = ran(
                &mut sim,
                vec![
                    Effect::Sync { file: file.clone(), tag: Tag(2) },
                    Effect::Barrier { file: file.clone(), tag: Tag(3) },
                    Effect::WriteBack { file: file.clone(), tag: Tag(4) },
                    Effect::Sync { file: missing.clone(), tag: Tag(5) },
                ],
            );
            assert_eq!(
                (&seen[&Tag(2)], &seen[&Tag(3)], &seen[&Tag(4)], &seen[&Tag(5)]),
                (&Done::Synced, &Done::Ordered, &Done::WrittenBack, &Done::Synced),
                "seed {seed}"
            );
            assert!(sim.exists(&missing), "seed {seed}: a sync makes a missing file");
            let seen = ran(
                &mut sim,
                vec![
                    Effect::Append { file: file.clone(), bytes: b"!".to_vec(), tag: Tag(6) },
                    Effect::WriteAt { file: file.clone(), at: 0, bytes: b"P".to_vec(), tag: Tag(7) },
                    Effect::Truncate { file: file.clone(), len: 0, tag: Tag(8) },
                ],
            );
            for tag in [6, 7, 8] {
                assert!(
                    matches!(&seen[&Tag(tag)], Done::Failed(IoError(why))
                        if why == &format!("{file}: {PERMISSION_DENIED}")),
                    "seed {seed}: tag {tag}: {:?}",
                    seen[&Tag(tag)]
                );
            }
            assert_eq!(sim.contents(&file), b"placed", "seed {seed}");
            assert_eq!(sim.mode(&file), Some(0o444));
            // The control: the cause, the missing write bit, removed.
            let unseal = Effect::Mode { file: file.clone(), mode: 0o644, tag: Tag(9) };
            assert_eq!(ran(&mut sim, vec![unseal])[&Tag(9)], Done::Moded);
            let append = Effect::Append { file: file.clone(), bytes: b"!".to_vec(), tag: Tag(10) };
            assert_eq!(ran(&mut sim, vec![append])[&Tag(10)], Done::Appended, "seed {seed}");
            assert_eq!(sim.contents(&file), b"placed!", "seed {seed}");
        }
    }

    /// Runs `effects` on `sim` until idle: each one's end by its tag.
    fn ran(sim: &mut Sim, mut effects: Vec<Effect>) -> BTreeMap<Tag, Done> {
        let mut seen = BTreeMap::new();
        run_until_idle(sim, &mut effects, |done, _| _ = seen.insert(done.tag, done.done))
        .expect("the effects end");
        seen
    }

    /// Two exclusive locks of a file contend and an unlock lets the next take it; shared locks
    /// share and exclude an exclusive one, as another process's lock does; a lock asked under a
    /// tag that holds one, and an unlock of a tag that holds none, fail; a crash lets every lock
    /// go, another process's too.
    #[test]
    fn locks_exclude_as_flock_does_until_unlocked_or_a_crash() {
        use LockMode::{Exclusive, Shared};
        let mut sim = Sim::new(3, Faults::default());
        let lock = name("history/lock");
        let take = |mode, tag| Effect::Lock { file: lock.clone(), mode, tag: Tag(tag) };
        let free = |lock, tag| Effect::Unlock { lock: Tag(lock), tag: Tag(tag) };
        assert_eq!(ran(&mut sim, vec![take(Exclusive, 1)])[&Tag(1)], Done::Locked);
        assert!(sim.exists(&lock), "a lock makes its file");
        assert_eq!(ran(&mut sim, vec![take(Exclusive, 2)])[&Tag(2)], Done::Contended);
        assert_eq!(ran(&mut sim, vec![take(Shared, 3)])[&Tag(3)], Done::Contended);
        assert!(matches!(&ran(&mut sim, vec![take(Shared, 1)])[&Tag(1)], Done::Failed(_)));
        assert_eq!(ran(&mut sim, vec![free(1, 4)])[&Tag(4)], Done::Unlocked);
        assert!(matches!(&ran(&mut sim, vec![free(1, 5)])[&Tag(5)], Done::Failed(_)));
        assert_eq!(ran(&mut sim, vec![take(Exclusive, 2)])[&Tag(2)], Done::Locked);
        assert_eq!(ran(&mut sim, vec![free(2, 6)])[&Tag(6)], Done::Unlocked);
        let seen = ran(&mut sim, vec![take(Shared, 7), take(Shared, 8)]);
        assert_eq!((&seen[&Tag(7)], &seen[&Tag(8)]), (&Done::Locked, &Done::Locked));
        assert_eq!(ran(&mut sim, vec![take(Exclusive, 9)])[&Tag(9)], Done::Contended);
        sim.crash();
        sim.hold_lock(&lock, Exclusive);
        assert_eq!(ran(&mut sim, vec![take(Shared, 9)])[&Tag(9)], Done::Contended);
        sim.release_lock(&lock);
        assert_eq!(ran(&mut sim, vec![take(Exclusive, 9)])[&Tag(9)], Done::Locked);
        sim.hold_lock(&lock, Shared);
        sim.crash();
        assert_eq!(ran(&mut sim, vec![take(Exclusive, 10)])[&Tag(10)], Done::Locked);
    }

    /// On a disk filled up, an append past the room keeps the part that fits, unsynced, and
    /// fails naming ENOSPC; a whole-file write that does not fit leaves the file as it was; a
    /// file outside the directory has room, and a crash gives the room back.
    #[test]
    fn a_full_disk_cuts_an_append_short_and_leaves_a_write_as_it_was() {
        let mut sim = Sim::new(4, Faults::default());
        let (log, head) = (name("history/log"), name("history/head"));
        let write = |bytes: &[u8], tag| Effect::Write {
            file: head.clone(),
            bytes: bytes.to_vec(),
            tag: Tag(tag),
        };
        let append = |bytes: &[u8], tag| Effect::Append {
            file: log.clone(),
            bytes: bytes.to_vec(),
            tag: Tag(tag),
        };
        assert_eq!(ran(&mut sim, vec![write(b"old", 0)])[&Tag(0)], Done::Written);
        sim.fill(name("history"), 5);
        assert_eq!(ran(&mut sim, vec![append(b"abc", 1)])[&Tag(1)], Done::Appended);
        let full = ran(&mut sim, vec![append(b"defg", 2)]);
        assert!(
            matches!(&full[&Tag(2)], Done::Failed(why)
                if why.0 == "history/log: No space left on device (os error 28)"),
            "{full:?}"
        );
        assert_eq!(sim.contents(&log), b"abcde");
        assert!(matches!(&ran(&mut sim, vec![write(b"new", 3)])[&Tag(3)], Done::Failed(_)));
        assert_eq!(sim.contents(&head), b"old");
        let elsewhere =
            Effect::Append { file: name("elsewhere"), bytes: b"fits".to_vec(), tag: Tag(4) };
        assert_eq!(ran(&mut sim, vec![elsewhere])[&Tag(4)], Done::Appended);
        sim.crash();
        assert_eq!(ran(&mut sim, vec![write(b"new", 5)])[&Tag(5)], Done::Written);
        assert_eq!(sim.contents(&head), b"new");
    }

    /// Memory of the simulation's tests, registered for reads into and writes from it.
    #[allow(unsafe_code, reason = "test memory: under `Sim`, which touches it only inside poll and crash")]
    fn memory(len: usize) -> crate::Memory {
        // SAFETY: `Memory::new`'s contract holds under `Sim` on the test's one thread.
        unsafe { crate::Memory::new("the simulation's tests", len) }.expect("a free region")
    }

    fn read_into(file: &FileName, at: u64, into: crate::Span, tag: u64) -> Effect {
        Effect::ReadInto {
            file: file.clone(),
            at,
            into,
            cache: Cache::Kept,
            priority: crate::Priority::Demand,
            tag: Tag(tag),
        }
    }

    fn write_from(file: &FileName, at: u64, from: crate::Span, tag: u64) -> Effect {
        Effect::WriteFrom { file: file.clone(), at, from, cache: Cache::Kept, tag: Tag(tag) }
    }

    /// A read into memory scribbles its span as it is submitted, so an asker that looks at it
    /// in flight sees the scribble, and fills it as it completes; a write from memory takes the
    /// bytes memory holds as it completes, not as it was asked.
    #[test]
    fn memory_is_the_kernel_s_while_an_effect_on_it_is_in_flight() {
        let mut sim = Sim::new(3, Faults::default());
        let mut memory = memory(4096);
        let file = name("pool/pack-1");
        sim.change(&file, b"0123456789");
        sim.submit(&mut vec![read_into(&file, 2, memory.span(0, 4), 1)]);
        let mut held = [0; 4];
        memory.read(0, &mut held);
        assert_eq!(held, [SCRIBBLE; 4]);
        let mut out = Vec::new();
        while out.is_empty() {
            sim.poll(&mut out, Deadline::NEVER);
        }
        assert_eq!(out[0].done, Done::ReadInto { len: 4 });
        memory.read(0, &mut held);
        assert_eq!(&held, b"2345");
        memory.write(100, b"asked");
        sim.submit(&mut vec![write_from(&file, 0, memory.span(100, 5), 2)]);
        memory.write(100, b"later");
        out.clear();
        while out.is_empty() {
            sim.poll(&mut out, Deadline::NEVER);
        }
        assert_eq!(out[0].done, Done::Written);
        assert_eq!(sim.contents(&file), b"later56789");
    }

    /// A crash keeps each sector of a write from memory that was not synced whole or not at
    /// all, never a part of one, on every seed; across seeds it keeps none, all, and some.
    ///
    /// A crash keeps each of the write's eight sectors or not by a coin of the seed's, so one
    /// that keeps none of them is one seed in a few hundred: over 200 seeds whether one
    /// turned up hung on which numbers the simulation had drawn before the crash, and any
    /// change to what it draws (a completion shuffled, a span scribbled) failed this test
    /// with nothing wrong. Over 4000 seeds none fails to turn up once in a hundred thousand
    /// such changes.
    #[test]
    fn a_crash_tears_a_write_from_memory_by_sectors() {
        let file = name("pool/pack-1");
        let (mut none, mut all, mut some) = (false, false, false);
        for seed in 0..4000 {
            let mut sim = Sim::new(seed, Faults::default());
            let mut memory = memory(4096);
            memory.fill(0, 4096, b'x');
            sim.change(&file, &[0; 4096]);
            let mut effects = vec![write_from(&file, 0, memory.span(0, 4096), 1)];
            // On odd seeds the write is still in flight at the crash.
            match seed % 2 {
                0 => run_until_idle(&mut sim, &mut effects, |_, _| {}).expect("the effects end"),
                _ => sim.submit(&mut effects),
            }
            sim.crash();
            let after = sim.contents(&file);
            assert_eq!(after.len(), 4096, "seed {seed}");
            let kept: Vec<bool> = after
                .chunks(SECTOR)
                .map(|it| match it {
                    it if it.iter().all(|&b| b == b'x') => true,
                    it if it.iter().all(|&b| b == 0) => false,
                    it => panic!("seed {seed}: a torn sector {it:?}"),
                })
                .collect();
            none |= kept.iter().all(|it| !it);
            all |= kept.iter().all(|it| *it);
            some |= kept.iter().any(|it| *it) && !kept.iter().all(|it| *it);
        }
        assert!(none && all && some, "none {none}, all {all}, some {some}");
    }

    /// Held reads wait, scribbled, until released; failed and short reads say so; rot changes
    /// the disk under what was synced, and a read sees it.
    #[test]
    fn reads_into_memory_are_held_failed_cut_short_and_see_rot() {
        let file = name("pool/pack-1");
        let mut sim = Sim::new(9, Faults::default());
        let memory = memory(4096);
        sim.change(&file, b"abcdefgh");
        sim.hold_reads(name("pool"));
        sim.submit(&mut vec![read_into(&file, 0, memory.span(0, 8), 1)]);
        let mut out = Vec::new();
        sim.advance(Duration::from_secs(1));
        sim.poll(&mut out, Deadline::PASSED);
        assert!(out.is_empty() && sim.reads_held() == 1);
        sim.rot(&file, 2, b"XY");
        sim.release_reads(Duration::ZERO);
        while out.is_empty() {
            sim.poll(&mut out, Deadline::NEVER);
        }
        assert_eq!(out[0].done, Done::ReadInto { len: 8 });
        let mut held = [0; 8];
        memory.read(0, &mut held);
        assert_eq!(&held, b"abXYefgh");
        sim.fail_reads(name("pool"), 1);
        let mut failed = None;
        run_until_idle(&mut sim, &mut vec![read_into(&file, 0, memory.span(0, 8), 2)], |it, _| {
            failed = Some(it.done)
        })
        .expect("the effects end");
        assert!(matches!(failed, Some(Done::Failed(_))), "{failed:?}");
        sim.crash();
        sim.short_reads(name("pool"), 1);
        let mut cut = None;
        run_until_idle(&mut sim, &mut vec![read_into(&file, 0, memory.span(0, 8), 3)], |it, _| {
            cut = Some(it.done)
        })
        .expect("the effects end");
        assert!(matches!(cut, Some(Done::ReadInto { len }) if len < 8), "{cut:?}");
    }
}
