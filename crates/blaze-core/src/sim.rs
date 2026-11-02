//! A deterministic [`Driver`]: one thread, a virtual clock and a seeded scheduler, so a state
//! machine's run under it is a function of the seed. It delays completions, delivers the ones
//! that are ready in a shuffled order, fails effects at a chosen rate, and [`Sim::crash`]
//! loses what was not synced, keeping a random prefix of it (a torn append), as a power cut
//! does. A failing seed replays exactly.
//!
//! Processes are the test's: [`Sim::program`] says what a program does when it is spawned, how
//! long it takes and what it ends with; a memory limit it passes kills it, as a cgroup does.
//! Sockets are in memory: a connect to an address listened on in the simulation makes both
//! ends, and [`Sim::dial`] is a peer from outside (an agent's client, a webhook) that the test
//! plays with [`Sim::peer_send`], [`Sim::peer_received`] and [`Sim::peer_hangup`].
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

mod journal;

use std::{mem, path::PathBuf, time::Duration};

use blaze_hash::HashMap;
use blaze_mem::{Slab, Tag as MemTag};

pub use self::journal::Journal;
use crate::{
    Access, Address, Command, Completion, Conn, Done, Driver, Effect, Exit, FileName, IoError,
    Meta, RECV_MOST, Tag,
    threads::tagged::{Id, Tagged},
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
// dyn: a test's program, given once per path and called once per spawn of it.
pub type Program = Box<dyn FnMut(&Command, &mut Rng) -> Ran>;

/// The signal a kill or a passed memory limit ends a process with.
pub const KILLED: i32 = 9;

/// The most ends of connections at once: their slab's reservation, committed as used.
const MAX_ENDS: u32 = 1 << 20;

/// The room reserved at the start for what is in flight at once, so a warm run does not grow.
const RESERVE: usize = 256;

/// A change made under a watch that is ending is still told once in this many, as a system
/// that queued it before it saw the unwatch: when it is due after the end, it is stale.
const RACE_ONE_IN: u64 = 3;

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
}

/// The simulation's random numbers: blaze's one seeded SplitMix64.
pub use blaze_rng::SplitMix64 as Rng;

/// How unkind the simulated machine is.
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
}

impl Default for Faults {
    fn default() -> Faults {
        Faults {
            fail_one_in: 0,
            max_latency: Duration::from_micros(200),
            no_clone: false,
            lose_change_one_in: 0,
        }
    }
}

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
}

impl Default for SimFile {
    fn default() -> SimFile {
        // alloc: none, two empty vectors.
        SimFile { durable: Vec::new(), ops: Vec::new(), mode: 0o644, link: false }
    }
}

impl SimFile {
    fn whole(bytes: Vec<u8>) -> SimFile {
        SimFile { durable: bytes, ..SimFile::default() }
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
        self.ops.iter().fold(self.durable.len() as u64, |len, op| match op {
            Op::Append(more) => len + more.len() as u64,
            Op::At(at, more) => len.max(at + more.len() as u64),
            Op::Cut(to) => *to,
        })
    }

    /// The first `covers` of its changes made durable.
    fn sync(&mut self, covers: usize) {
        let SimFile { durable, ops, .. } = self;
        for op in ops.drain(..covers.min(ops.len())) {
            op.apply(durable);
        }
    }
}

/// A change to a file not yet synced.
#[derive(Debug)]
enum Op {
    Append(Vec<u8>),
    At(u64, Vec<u8>),
    Cut(u64),
}

impl Op {
    fn apply(&self, bytes: &mut Vec<u8>) {
        match self {
            Op::Append(more) => bytes.extend_from_slice(more),
            Op::At(at, more) => {
                let at = *at as usize;
                if bytes.len() < at + more.len() {
                    bytes.resize(at + more.len(), 0);
                }
                bytes[at..at + more.len()].copy_from_slice(more);
            }
            Op::Cut(len) => bytes.resize(*len as usize, 0),
        }
    }

    /// The change cut short, as a power cut leaves a write it was in the middle of.
    fn torn(&self, rng: &mut Rng) -> Option<Op> {
        // alloc: a crash, not an op: the part of the change that landed.
        let part = |more: &[u8], rng: &mut Rng| more[..rng.below(more.len() as u64) as usize].to_vec();
        match self {
            Op::Append(more) => Some(Op::Append(part(more, rng))),
            Op::At(at, more) => Some(Op::At(*at, part(more, rng))),
            Op::Cut(_) => None,
        }
    }
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
    dir: FileName,
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

pub struct Sim {
    rng: Rng,
    faults: Faults,
    now: Duration,
    files: HashMap<FileName, SimFile>,
    /// Whose a file or directory is and who else can write it, where a test said it is not
    /// the user's alone ([`Sim::set_access`]).
    access: HashMap<FileName, Access>,
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
    programs: HashMap<PathBuf, Program>,
    listens: Tagged<Address>,
    ends: Slab<End>,
    /// Receives waiting for bytes or a hang-up.
    receiving: Vec<Receiving>,
    /// A file's bytes with its unsynced changes applied, made for a read and kept for the next.
    scratch: Vec<u8>,
    /// Completions told for an op that had ended when they were due.
    stale: u64,
}

/// A delay drawn up to the simulation's latency.
fn latency(rng: &mut Rng, faults: &Faults) -> Duration {
    Duration::from_nanos(rng.below(faults.max_latency.as_nanos() as u64 + 1))
}

/// `at`, or later: after everything told of `a` or `b`.
fn after_told(told: &[Told], at: Duration, a: Tag, b: Tag) -> Duration {
    told.iter()
        .filter(|it| it.completion.tag == a || it.completion.tag == b || matches!(it.of, Of::Flush(watch) if watch == a))
        .map(|it| it.at)
        .fold(at, Duration::max)
}

/// The file `name` of the simulated disk, made empty if it is not there.
fn file_mut<'a>(files: &'a mut HashMap<FileName, SimFile>, name: &FileName) -> &'a mut SimFile {
    if !files.contains_key(name) {
        // clone: once per file new to the simulated disk, its key.
        files.insert(name.clone(), SimFile::default());
    }
    files.get_mut(name).expect("there now")
}

impl Sim {
    pub fn new(seed: u64, faults: Faults) -> Sim {
        Sim {
            rng: Rng::new(seed),
            faults,
            now: Duration::ZERO,
            files: HashMap::default(),
            access: HashMap::default(),
            pending: Wheel::new(),
            spawns: Tagged::new(RESERVE),
            ready: Vec::with_capacity(RESERVE),
            watches: Tagged::new(RESERVE),
            ending: 0,
            disk: journal::Disk::default(),
            told: Vec::with_capacity(RESERVE),
            later: Vec::with_capacity(RESERVE),
            programs: HashMap::default(),
            listens: Tagged::new(RESERVE),
            ends: match Slab::new(MAX_ENDS, MemTag::Io) {
                Ok(ends) => ends,
                Err(error) => panic!("{error}"),
            },
            receiving: Vec::with_capacity(RESERVE),
            // alloc: none until the first read of a file with unsynced changes.
            scratch: Vec::new(),
            stale: 0,
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
        program: impl FnMut(&Command, &mut Rng) -> Ran + 'static,
    ) {
        // alloc: once per program the test gives.
        self.programs.insert(path.into(), Box::new(program));
    }

    /// A peer outside the simulation connects to `at`: the listen there accepts the end this
    /// returns, which the test plays the other side of.
    pub fn dial(&mut self, at: &Address) -> Result<Conn, IoError> {
        let Some(listen) = self.listening_at(at) else {
            // alloc: an error's message.
            return Err(IoError(format!("{at:?}: nothing listens there: connection refused")));
        };
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
    /// a random prefix of the changes after (appends, writes at an offset, truncations), the
    /// last of which may be torn. A whole-file write or rename that completed is kept whole;
    /// one in flight is not there.
    pub fn crash(&mut self) {
        self.pending.drain(|_, _| {});
        self.spawns.clear();
        self.watches.clear();
        self.ending = 0;
        self.told.clear();
        self.listens.clear();
        self.ends.clear();
        self.receiving.clear();
        // By name, so the seed's numbers go to the same files whatever the table's order.
        // alloc: a crash, not an op.
        let mut files: Vec<(&FileName, &mut SimFile)> = self.files.iter_mut().collect();
        files.sort_unstable_by(|a, b| a.0.cmp(b.0));
        for (_, file) in files {
            let kept = self.rng.below(file.ops.len() as u64 + 1) as usize;
            let torn = file.ops.get(kept).and_then(|op| op.torn(&mut self.rng));
            file.sync(kept);
            if let Some(torn) = torn {
                torn.apply(&mut file.durable);
            }
            file.ops.clear();
        }
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
        // alloc: the world changed by the test, not an op.
        *file_mut(&mut self.files, file) = SimFile::whole(bytes.to_vec());
        self.changed(file);
    }

    /// `file` made a symbolic link to `target`, durably, by something outside the state
    /// machine (a checkout): watches of it hear of it.
    pub fn symlink(&mut self, file: &FileName, target: &str) {
        // alloc: the world changed by the test, not an op.
        *file_mut(&mut self.files, file) = SimFile { mode: 0o777, link: true, ..SimFile::whole(target.into()) };
        self.changed(file);
    }

    /// `to` made with `from`'s contents and mode, durably: the simulation does not model
    /// shared blocks or a link's shared inode.
    fn copy(&mut self, from: &FileName, to: &FileName) {
        let SimFile { mode, link, .. } = self.files[from];
        let bytes = self.contents(from);
        *file_mut(&mut self.files, to) = SimFile { mode, link, ..SimFile::whole(bytes) };
        self.changed(to);
    }

    /// Where the symbolic link `file` leads, followed as the system opens a file (`None` when
    /// `file` is no link): an error when it ends at a directory, outside the simulated root or
    /// in a loop. Only the last name is followed; a link among a path's directories is not
    /// modelled.
    fn followed(&self, file: &FileName) -> Result<Option<FileName>, IoError> {
        if !self.files.get(file).is_some_and(|it| it.link) {
            return Ok(None);
        }
        // clone: the name of the file a read ends at.
        let mut at = file.clone();
        for _ in 0..40 {
            let Some(sim) = self.files.get(&at).filter(|it| it.link) else {
                if !self.files.contains_key(&at) && self.files.keys().any(|it| it.is_under(&at)) {
                    // alloc: an error's message.
                    return Err(IoError(format!("{file}: Is a directory (os error 21)")));
                }
                return Ok(Some(at));
            };
            let target = String::from_utf8_lossy(&sim.contents()).into_owned();
            // alloc: the name a link leads to.
            let mut parts: Vec<&str> = at.as_str().rsplit_once('/').map_or(vec![], |(dir, _)| dir.split('/').collect());
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
                            return Err(IoError(format!("{file}: a link out of the simulated root")));
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
            None => Done::Failed(IoError(format!("{file}: No such file or directory (os error 2)"))),
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
    fn list(&self, dir: &FileName) -> Done {
        if self.files.contains_key(dir) {
            // alloc: an error's message.
            return Done::Failed(IoError(format!("{dir}: not a directory")));
        }
        let start = dir.as_str().len() + 1;
        // alloc: the listing, which the completion carries.
        let mut listed: Vec<(FileName, Meta)> = Vec::new();
        for (file, sim) in self.files.iter().filter(|(it, _)| it.is_under(dir)) {
            for (at, _) in file.as_str()[start..].match_indices('/') {
                let inner = FileName::new(&file.as_str()[..start + at]).expect("a part of a name");
                let modified_ns = self.disk.mtime(&inner).unwrap_or(0);
                listed.push((inner, Meta { len: 0, dir: true, modified_ns }));
            }
            let modified_ns = self.disk.mtime(file).unwrap_or(0);
            // clone: a name of the listing.
            listed.push((file.clone(), Meta { len: sim.len(), dir: false, modified_ns }));
        }
        if listed.is_empty() {
            // alloc: an error's message.
            return Done::Failed(IoError(format!("{dir}: no such directory")));
        }
        listed.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.dir.cmp(&b.1.dir)));
        listed.dedup_by(|later, first| later.0 == first.0);
        Done::Listed(listed)
    }

    /// [`Effect::Private`] on the simulated disk, whose directories are made 0o700: the first,
    /// by name, of `dir` and what is under it that [`Sim::set_access`] took from the user or
    /// whose mode lets others write it.
    fn private(&self, dir: &FileName) -> Done {
        if let Some(&access) = self.access.get(dir) {
            // clone: the refusal names the directory
            return Done::Private(Some((dir.clone(), access)));
        }
        // What a test set of a file comes before what its mode says.
        let given = self.access.iter().filter(|(it, _)| it.is_under(dir)).map(|(it, &access)| (it, 0, access));
        let moded = self
            .files
            .iter()
            .filter(|(it, sim)| sim.mode & 0o002 != 0 && it.is_under(dir))
            .map(|(it, _)| (it, 1, Access::OthersWrite));
        let first = given.chain(moded).min_by_key(|&(file, rank, _)| (file, rank));
        // clone: the refusal names the file
        Done::Private(first.map(|(file, _, access)| (file.clone(), access)))
    }

    /// The file removed by something outside the state machine.
    pub fn delete(&mut self, file: &FileName) {
        self.files.remove(file);
        self.changed(file);
    }

    pub fn rng(&mut self) -> &mut Rng {
        &mut self.rng
    }

    fn delay(&mut self) -> Duration {
        latency(&mut self.rng, &self.faults)
    }

    /// Tells the watches of `file` it changed, in their slab's order, which is the seed's as
    /// everything else is.
    fn changed(&mut self, file: &FileName) {
        self.disk.touched(file, &self.files);
        self.tell_journals();
        for (id, tag, watch) in self.watches.iter() {
            if watch.journal || !file.is_under(&watch.dir) {
                continue;
            }
            if watch.ending && !self.rng.one_in(RACE_ONE_IN) {
                continue;
            }
            let done = match self.rng.one_in(u64::from(self.faults.lose_change_one_in)) {
                // alloc: none, an empty list.
                true => Done::Changed { files: Vec::new(), lost: true },
                // alloc: the change's payload, the one name it tells.
                false => Done::Changed { files: vec![file.clone()], lost: false },
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
                self.told.push(Told { at, completion: Completion { tag, done }, of: Of::Change(id) });
            }
        }
    }

    /// Which system's journal of changes the simulation plays from now on.
    pub fn set_journal(&mut self, journal: Journal) {
        self.disk.kind = journal;
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
        self.disk.stamp(name, &self.files)
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
            access => self.access.insert(file.clone(), access),
        };
    }

    /// `from` renamed over `to` by something outside the state machine.
    pub fn rename(&mut self, from: &FileName, to: &FileName) {
        if let Some(sim) = self.files.remove(from) {
            // clone: the world changed by the test, not an op.
            self.files.insert(to.clone(), sim);
            self.disk.renamed(from, to);
            self.changed(from);
            self.changed(to);
        }
    }

    fn fail(&mut self, tag: Tag) -> Completion {
        // alloc: an injected failure's message.
        Completion { tag, done: Done::Failed(IoError("injected by the simulation".into())) }
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

    /// Every waiting receive that can be answered now is: with what came, or with nothing once
    /// either end hung up.
    fn wake(&mut self) {
        let mut at = 0;
        while at < self.receiving.len() {
            let Receiving { end, most, .. } = self.receiving[at];
            let ready = self.ends.get(end).is_none_or(|it| !it.inbox.is_empty() || it.closed || it.peer_closed);
            if !ready {
                at += 1;
                continue;
            }
            let Receiving { tag, end, most: _, mut into } = self.receiving.remove(at);
            let kept = into.len();
            if let Some(it) = self.ends.get_mut(end) {
                let n = most.min(RECV_MOST).min(it.inbox.len());
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

    fn complete(&mut self, pending: Pending) -> Completion {
        let Pending { effect, covers, spawn } = pending;
        let tag = effect.tag();
        if !matches!(effect, Effect::Timer { .. }) && self.rng.one_in(u64::from(self.faults.fail_one_in)) {
            if let Some(spawn) = spawn {
                self.spawns.remove_id(spawn);
            }
            if let Effect::Append { file, mut bytes, .. } = effect {
                let torn = self.rng.below(bytes.len() as u64) as usize;
                bytes.truncate(torn);
                file_mut(&mut self.files, &file).ops.push(Op::Append(bytes));
                if torn > 0 {
                    self.changed(&file);
                }
            }
            return self.fail(tag);
        }
        let done = match effect {
            Effect::ReadAll { file, .. } => self.read(&file, 0, usize::MAX).map_or_else(Done::Failed, Done::Read),
            Effect::Read { file, at, len, .. } => self.read(&file, at, len).map_or_else(Done::Failed, Done::Read),
            Effect::ReadLink { file, .. } => self.read_link(&file),
            Effect::Append { file, bytes, .. } => {
                file_mut(&mut self.files, &file).ops.push(Op::Append(bytes));
                self.changed(&file);
                Done::Appended
            }
            Effect::Sync { file, .. } => {
                self.files.entry(file).or_default().sync(covers);
                Done::Synced
            }
            Effect::Truncate { file, len, .. } => {
                file_mut(&mut self.files, &file).ops.push(Op::Cut(len));
                self.changed(&file);
                Done::Truncated
            }
            Effect::Write { file, bytes, .. } => {
                *file_mut(&mut self.files, &file) = SimFile::whole(bytes);
                self.changed(&file);
                Done::Written
            }
            Effect::WriteAt { file, at, bytes, .. } => match self.files.get_mut(&file) {
                Some(sim) => {
                    sim.ops.push(Op::At(at, bytes));
                    self.changed(&file);
                    Done::Written
                }
                // alloc: an error's message.
                None => Done::Failed(IoError(format!("{file}: no such file"))),
            },
            Effect::Rename { from, to, .. } => match self.files.remove(&from) {
                Some(sim) => {
                    *file_mut(&mut self.files, &to) = sim;
                    self.disk.renamed(&from, &to);
                    self.changed(&from);
                    self.changed(&to);
                    Done::Renamed
                }
                // alloc: an error's message.
                None => Done::Failed(IoError(format!("{from}: no such file"))),
            },
            Effect::Mode { file, mode, .. } => match self.files.get_mut(&file) {
                Some(sim) => {
                    sim.mode = mode;
                    self.disk.attributes(&file);
                    self.tell_journals();
                    Done::Moded
                }
                // alloc: an error's message.
                None => Done::Failed(IoError(format!("{file}: no such file"))),
            },
            Effect::Times { file, modified_ns, .. } => match self.disk.times(&file, modified_ns, &self.files) {
                true => {
                    self.tell_journals();
                    Done::Timed
                }
                // alloc: an error's message.
                false => Done::Failed(IoError(format!("{file}: no such file or directory"))),
            },
            Effect::Clone { from, to, .. } | Effect::Link { from, to, .. }
                if !self.files.contains_key(&from) =>
            {
                let _ = to;
                // alloc: an error's message.
                Done::Failed(IoError(format!("{from}: no such file")))
            }
            Effect::Clone { .. } if self.faults.no_clone => Done::Failed(IoError(
                // alloc: an error's message.
                "the simulated file system cannot share blocks; blaze does not copy instead"
                    .into(),
            )),
            Effect::Clone { to, .. } | Effect::Link { to, .. } if self.files.contains_key(&to) => {
                // alloc: an error's message.
                Done::Failed(IoError(format!("{to}: already there")))
            }
            Effect::Clone { from, to, .. } => {
                self.copy(&from, &to);
                Done::Cloned
            }
            Effect::Link { from, to, .. } => {
                self.copy(&from, &to);
                Done::Linked
            }
            Effect::Stat { file, .. } => Done::Stat({
                let modified_ns = self.disk.mtime(&file).unwrap_or(0);
                match self.files.get(&file) {
                    Some(it) => Some(Meta { len: it.len(), dir: false, modified_ns }),
                    None if self.files.keys().any(|it| it != &file && it.is_under(&file)) => {
                        Some(Meta { len: 0, dir: true, modified_ns })
                    }
                    None => None,
                }
            }),
            Effect::List { dir, .. } => self.list(&dir),
            Effect::Private { dir, .. } => self.private(&dir),
            Effect::Stamp { files, .. } => Done::Stamped(
                files
                    .iter()
                    .map(|name| {
                        // alloc: a name of the batch as the simulated disk keys it.
                        let name = FileName::new(name).expect("a Names entry is a name");
                        self.disk.stamp(&name, &self.files)
                    })
                    // alloc: the stamps, which the completion carries.
                    .collect(),
            ),
            Effect::Entries { dir, .. } => match self.disk.entries(&dir, &self.files) {
                Some(entries) => Done::Entries(entries),
                // alloc: an error's message.
                None => Done::Failed(IoError(format!("{dir}: no such directory"))),
            },
            Effect::Remove { file, .. } => {
                if self.files.remove(&file).is_some() {
                    self.changed(&file);
                }
                Done::Removed
            }
            Effect::Timer { .. } => Done::Fired,
            Effect::Map { file, .. } => {
                // alloc: the mapping, which the completion carries.
                Done::Mapped(crate::Mapping::copy(&self.contents(&file)))
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
            | Effect::Hangup { .. } => {
                unreachable!("watches, kills and sockets are not pending effects")
            }
        };
        Completion { tag, done }
    }

    fn watch(&mut self, tag: Tag, dir: FileName, journal: bool) -> Id<Watching> {
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
            Of::Recv { end, .. } if self.ends.get(end).is_some_and(|it| !it.closed) => Some(completion),
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

impl Driver for Sim {
    fn submit(&mut self, effects: &mut Vec<Effect>) {
        for effect in effects.drain(..) {
            match effect {
                Effect::Watch { dir, tag } => {
                    self.watch(tag, dir, false);
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
                        _ => (Done::Failed(IoError(format!("nothing is watched as {tag:?}"))), Of::Op),
                    };
                    // A change told before the end is heard before it.
                    self.tell_after(tag, tag, done, of);
                    continue;
                }
                Effect::Flush { watch, tag } => {
                    let done = match self.watches.get(watch).is_some_and(|it| !it.ending) {
                        // alloc: an error path.
                        false => Done::Failed(IoError(format!("nothing is watched as {watch:?}"))),
                        true if self.rng.one_in(u64::from(self.faults.fail_one_in)) => self.fail(tag).done,
                        true => Done::Flushed,
                    };
                    // Every change of the watch told by now is heard first.
                    self.tell_after(watch, tag, done, Of::Flush(watch));
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
                Effect::Listen { at, tag } => {
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
                Effect::Connect { at, tag } => {
                    let done = match self.listening_at(&at) {
                        _ if self.rng.one_in(u64::from(self.faults.fail_one_in)) => self.fail(tag).done,
                        // alloc: an error's message.
                        None => Done::Failed(IoError(format!(
                            "{at:?}: nothing listens there: connection refused"
                        ))),
                        Some(listen) => {
                            let mine = self.new_end(None);
                            let theirs = self.new_end(Some(end_of(mine)));
                            self.ends.get_mut(end_of(mine)).expect("just made").peer = Some(end_of(theirs));
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
                        false => self.tell(tag, Done::unread(into, format_args!("no connection {conn:?}")), Of::Op),
                    }
                    continue;
                }
                Effect::Send { conn, mut bytes, tag } => {
                    let end = end_of(conn);
                    let done = match self.ends.get(end).map(|it| (it.closed, it.peer_closed, it.peer)) {
                        None | Some((true, _, _)) => Done::unsent(bytes, format_args!("no connection {conn:?}")),
                        Some((_, true, _)) => {
                            Done::unsent(bytes, format_args!("{conn:?}: the connection is hung up: broken pipe"))
                        }
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
                Effect::Spawn { ref command, tag } => match self.run(command) {
                    Ok(ran) => {
                        let after = ran.takes.saturating_add(self.delay());
                        let ready = wheel::ticks(self.now).saturating_add(wheel::ticks(after));
                        let (id, _) = self.spawns.insert(tag, Spawning { exit: ran.exit, pending: None });
                        let pending =
                            self.pending.insert(ready, Pending { effect, covers: 0, spawn: Some(id) });
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
            let delay = match &effect {
                Effect::Timer { after, .. } => *after,
                _ => self.delay(),
            };
            let covers = match &effect {
                Effect::Sync { file, .. } => self.files.get(file).map_or(0, |it| it.ops.len()),
                _ => 0,
            };
            self.pend(delay, effect, covers);
        }
    }

    /// Never waits: time jumps to the first completion when none is ready.
    fn poll(&mut self, out: &mut Vec<Completion>, _wait: Duration) -> usize {
        let first = self.pending.next_deadline().map(Duration::from_nanos);
        let Some(first) = first.into_iter().chain(self.told.iter().map(|it| it.at)).min() else {
            return 0;
        };
        self.now = self.now.max(first);
        let mut ready = mem::take(&mut self.ready);
        self.pending.advance(wheel::ticks(self.now), |_, pending| ready.push(pending));
        // In the order they were due; shuffled so it is the seed's.
        self.rng.shuffle(&mut ready);
        let before = out.len();
        for pending in ready.drain(..) {
            let completion = self.complete(pending);
            out.push(completion);
        }
        self.ready = ready;
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
        out.len() - before
    }

    fn in_flight(&self) -> usize {
        let ending = self.told.iter().filter(|it| {
            !matches!(
                it.completion.done,
                Done::Changed { .. } | Done::Journaled(_) | Done::Accepted { .. }
            )
        });
        self.pending.len()
            + (self.watches.len() - self.ending)
            + self.listens.len()
            + self.receiving.len()
            + ending.count()
    }

    fn now(&self) -> Duration {
        self.now
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

    #[test]
    fn a_crash_keeps_what_was_synced_and_a_prefix_of_the_rest() {
        for seed in 0..200 {
            let mut sim = Sim::new(seed, Faults::default());
            let file = name("log");
            let mut effects =
                vec![Effect::Append { file: file.clone(), bytes: b"synced".to_vec(), tag: Tag(0) }];
            run_until_idle(&mut sim, &mut effects, |_, _| {});
            effects.push(Effect::Sync { file: file.clone(), tag: Tag(1) });
            run_until_idle(&mut sim, &mut effects, |_, _| {});
            effects.push(Effect::Append {
                file: file.clone(),
                bytes: b"-lost".to_vec(),
                tag: Tag(2),
            });
            run_until_idle(&mut sim, &mut effects, |_, _| {});
            sim.crash();
            let left = sim.contents(&file);
            assert!(left.starts_with(b"synced") && b"synced-lost".starts_with(&left), "{left:?}");
        }
    }

    #[test]
    fn the_same_seed_is_the_same_run() {
        let run = |seed| {
            let mut sim = Sim::new(seed, Faults { fail_one_in: 4, ..Faults::default() });
            let mut effects: Vec<Effect> = (0..32)
                .map(|n| Effect::Append { file: name("f"), bytes: vec![n as u8], tag: Tag(n) })
                .collect();
            let mut order = Vec::new();
            run_until_idle(&mut sim, &mut effects, |done, _| order.push(done));
            (order, sim.contents(&name("f")))
        };
        assert_eq!(run(7), run(7));
        assert_ne!(run(7), run(8));
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
        run_until_idle(&mut sim, &mut effects, |_, _| {});
        sim.submit(&mut vec![Effect::Sync { file: file.clone(), tag: Tag(1) }]);
        // Appended after the sync was asked for, completed before it.
        sim.files.get_mut(&file).unwrap().ops.push(Op::Append(b"b".to_vec()));
        let mut out = Vec::new();
        sim.poll(&mut out, Duration::ZERO);
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
                sim.files.get_mut(&file).unwrap().ops.push(op);
                let whole = sim.contents(&file);
                assert_eq!(sim.files[&file].len(), whole.len() as u64, "seed {seed}");
                let (at, len) = (sim.rng.below(25), sim.rng.below(25) as usize);
                let from = (at as usize).min(whole.len());
                assert_eq!(sim.read(&file, at, len).unwrap(), whole[from..(from + len).min(whole.len())], "seed {seed}");
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
            let mut effects = vec![Effect::Write {
                file: file.clone(),
                bytes: b"aaaa".to_vec(),
                tag: Tag(0),
            }];
            run_until_idle(&mut sim, &mut effects, |_, _| {});
            let synced = seed % 2 == 0;
            let mut effects = vec![Effect::WriteAt {
                file: file.clone(),
                at: 2,
                bytes: b"bbbb".to_vec(),
                tag: Tag(1),
            }];
            run_until_idle(&mut sim, &mut effects, |it, _| assert_eq!(it.done, Done::Written));
            assert_eq!(sim.contents(&file), b"aabbbb");
            if synced {
                let mut effects = vec![Effect::Sync { file: file.clone(), tag: Tag(2) }];
                run_until_idle(&mut sim, &mut effects, |_, _| {});
            }
            sim.crash();
            let after = sim.contents(&file);
            if synced {
                assert_eq!(after, b"aabbbb", "seed {seed}");
            } else {
                // Some first part of the bbbb landed over aaaa, lengthening it past its end.
                let landed = after.iter().skip(2).take_while(|&&b| b == b'b').count();
                let mut want = b"aaaa".to_vec();
                Op::At(2, vec![b'b'; landed]).apply(&mut want);
                assert_eq!(after, want, "seed {seed}");
            }
        }
        let mut sim = Sim::new(1, Faults::default());
        let to = FileName::new("out/app.new").unwrap();
        let mut effects = vec![Effect::Write { file: to.clone(), bytes: b"new".to_vec(), tag: Tag(0) }];
        run_until_idle(&mut sim, &mut effects, |_, _| {});
        let mut effects = vec![Effect::Mode { file: to.clone(), mode: 0o755, tag: Tag(1) }];
        run_until_idle(&mut sim, &mut effects, |it, _| assert_eq!(it.done, Done::Moded));
        let mut effects = vec![Effect::Rename { from: to.clone(), to: file.clone(), tag: Tag(2) }];
        run_until_idle(&mut sim, &mut effects, |it, _| assert_eq!(it.done, Done::Renamed));
        sim.crash();
        assert_eq!(
            (sim.contents(&file), sim.mode(&file), sim.exists(&to)),
            (b"new".to_vec(), Some(0o755), false),
        );
        let mut effects = vec![Effect::WriteAt { file: to, at: 0, bytes: vec![1], tag: Tag(3) }];
        run_until_idle(&mut sim, &mut effects, |it, _| assert!(matches!(it.done, Done::Failed(_))));
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
            run_until_idle(sim, &mut effects, |it, _| found = Some(it.done));
            found.expect("the check completed")
        };
        let mut effects = vec![
            Effect::Write { file: a.clone(), bytes: b"a".to_vec(), tag: Tag(0) },
            Effect::Write { file: b.clone(), bytes: b"b".to_vec(), tag: Tag(1) },
            Effect::Write { file: name("other/c"), bytes: b"c".to_vec(), tag: Tag(2) },
        ];
        run_until_idle(&mut sim, &mut effects, |_, _| {});
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
            let mut effects = vec![Effect::Times { file: file.clone(), modified_ns: at, tag: Tag(1) }];
            run_until_idle(&mut sim, &mut effects, |it, _| done = Some(it.done));
            let after = sim.stamp(&file);
            assert_eq!((after.ino, after.size, after.mode), (before.ino, before.size, before.mode), "seed {seed}");
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
                Effect::List { dir: name("target"), tag: Tag(3) },
            ];
            run_until_idle(&mut sim, &mut effects, |it, _| _ = seen.insert(it.tag.0, it.done));
            assert_eq!(seen[&2], Done::Stat(Some(Meta { len: 33, dir: false, modified_ns: at })), "seed {seed}");
            let Done::Listed(listed) = &seen[&3] else { panic!("seed {seed}: {:?}", seen[&3]) };
            assert!(listed.contains(&(file.clone(), Meta { len: 33, dir: false, modified_ns: at })), "seed {seed}");
            sim.crash();
            assert_eq!(sim.stamp(&file).mtime_ns, at, "seed {seed}: the time is the inode's, which a crash keeps");
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
            });
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
            assert_eq!(sim.stamp(&file), crate::Stamp::GONE, "seed {seed}: a failed set makes nothing");
            let mut effects = vec![Effect::Write { file: moved.clone(), bytes: b"again".to_vec(), tag: Tag(7) }];
            run_until_idle(&mut sim, &mut effects, |it, _| assert_eq!(it.done, Done::Written, "seed {seed}"));
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
                sim.poll(&mut out, Duration::ZERO);
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

    #[test]
    fn a_listing_names_files_and_the_directories_they_imply() {
        let mut sim = Sim::new(1, Faults::default());
        sim.change(&name("src/a/b/c.rs"), b"12");
        sim.change(&name("src/a/d.rs"), b"1");
        sim.change(&name("src/e"), b"");
        sim.change(&name("other"), b"x");
        let Done::Listed(listed) = sim.list(&name("src")) else { panic!() };
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
        assert!(matches!(sim.list(&name("other")), Done::Failed(_)));
        assert!(matches!(sim.list(&name("none")), Done::Failed(_)));
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
            });
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
            });
            assert!(ended && sim.in_flight() == 0, "seed {seed}");
            // Changes told for after the end are not in flight: each is due, and dropped.
            let mut out = Vec::new();
            while !sim.told.is_empty() {
                sim.poll(&mut out, Duration::ZERO);
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
            run_until_idle(&mut sim, &mut effects, |done, _| heard.push(done.done));
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
            let (mut heard, mut at_flush, mut refused, mut ended) = (Vec::new(), None, false, false);
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
            });
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
        run_until_idle(&mut sim, &mut effects, |done, _| _ = seen.insert(done.tag, done.done));
        assert!(matches!(&seen[&Tag(1)], Done::Failed(why) if why.0.contains("does not copy")));
        assert_eq!(seen[&Tag(2)], Done::Linked);
        assert!(!sim.exists(&name("target/a")));
        assert_eq!(sim.contents(&name("target/b")), b"code");
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
            run_until_idle(&mut sim, &mut effects, |done, _| _ = seen.insert(done.tag, done.done));
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
                sim.poll(&mut out, Duration::ZERO);
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
        sim.poll(&mut out, Duration::ZERO);
        assert!(sim.now() < Duration::from_millis(5), "{:?}", sim.now());
        assert!(matches!(&out[..], [Completion { done: Done::Exited(it), .. }] if it.signal == Some(KILLED)));
    }

    /// A client and a server in one simulation, and a peer from outside, trade bytes and hang
    /// up; every receive ends, with bytes or with nothing once the other end hung up.
    #[test]
    fn sockets_carry_bytes_and_hang_ups_on_every_seed() {
        let at = Address::Unix(name("daemon.sock"));
        for seed in 0..500 {
            let mut sim = Sim::new(seed, Faults::default());
            let mut effects = vec![Effect::Listen { at: at.clone(), tag: Tag(1) }];
            sim.submit(&mut effects);
            effects.push(Effect::Connect { at: at.clone(), tag: Tag(2) });
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
                sim.poll(&mut out, Duration::ZERO);
                for done in out.drain(..) {
                    next += 1;
                    match done.done {
                        Done::Accepted { conn } => {
                            heard.insert(conn, (Vec::new(), false));
                            receiving.insert(Tag(next), conn);
                            effects.push(Effect::Recv { conn, most: 4, into: Vec::new(), tag: Tag(next) });
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
                            assert!(bytes.is_empty() && bytes.capacity() >= 5, "seed {seed}: {bytes:?}");
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
                                    effects.push(Effect::Recv { conn, most: 4, into: bytes, tag: Tag(next) });
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
            sim.submit(&mut vec![Effect::Listen { at: at.clone(), tag: Tag(1) }]);
            let conn = sim.dial(&at).unwrap();
            let mut seen = BTreeMap::new();
            let mut out = Vec::new();
            while !seen.contains_key(&Tag(1)) {
                sim.poll(&mut out, Duration::ZERO);
                seen.extend(out.drain(..).map(|it| (it.tag, it.done)));
            }
            sim.peer_send(conn, b"bytes");
            sim.submit(&mut vec![Effect::Recv { conn, most: 16, into: b"kept:".to_vec(), tag: Tag(2) }]);
            if sim.rng().one_in(2) {
                while !seen.contains_key(&Tag(2)) {
                    sim.poll(&mut out, Duration::ZERO);
                    seen.extend(out.drain(..).map(|it| (it.tag, it.done)));
                }
            }
            let mut effects = vec![
                Effect::Hangup { conn, tag: Tag(3) },
                Effect::Recv { conn, most: 16, into: b"again".to_vec(), tag: Tag(4) },
                Effect::Unlisten { tag: Tag(1) },
            ];
            run_until_idle(&mut sim, &mut effects, |it, _| _ = seen.insert(it.tag, it.done));
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
            assert_eq!(sim.stale(), u64::from(seen[&Tag(2)] == Done::Read(b"kept:".to_vec())), "seed {seed}");
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
                sim.poll(&mut out, Duration::ZERO);
                if let Some(at) = out.iter().position(|it| it.tag == tag) {
                    return out.swap_remove(at).done;
                }
            }
        }
        let at = Address::Unix(name("s.sock"));
        for seed in 0..100 {
            let mut sim = Sim::new(seed, Faults::default());
            sim.submit(&mut vec![Effect::Listen { at: at.clone(), tag: Tag(1) }]);
            let mut model: HashMap<u64, Old> = HashMap::default();
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
                        let done = until(&mut sim, Effect::Send { conn: conns[at], bytes: vec![step as u8], tag });
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
                        assert!(buffer.is_empty() && buffer.capacity() >= 1, "a send's buffer comes back cleared");
                    }
                    (2, Some(at)) => {
                        let done = until(&mut sim, Effect::Hangup { conn: conns[at], tag: Tag(1000 + step) });
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
                        let want = model.get_mut(&(at as u64)).map(|old| mem::take(&mut old.outbox));
                        assert_eq!(sim.peer_received(conns[at]), want.unwrap_or_default(), "seed {seed} step {step}");
                        let gone = model.get(&(at as u64)).is_none_or(|old| old.closed);
                        assert_eq!(sim.peer_hung_up_on(conns[at]), gone, "seed {seed} step {step}");
                    }
                }
                sim.poll(&mut out, Duration::ZERO);
                out.clear();
            }
            let live = model.len();
            assert_eq!(sim.ends.len(), live, "seed {seed}");
        }
    }

    #[test]
    fn a_connect_to_nothing_and_a_second_listen_fail() {
        let mut sim = Sim::new(2, Faults::default());
        let at = Address::Tcp("127.0.0.1:8080".parse().unwrap());
        let mut effects = vec![
            Effect::Connect { at: at.clone(), tag: Tag(1) },
            Effect::Listen { at: at.clone(), tag: Tag(2) },
        ];
        sim.submit(&mut effects);
        effects.push(Effect::Listen { at: at.clone(), tag: Tag(3) });
        effects.push(Effect::Unlisten { tag: Tag(2) });
        effects.push(Effect::Unlisten { tag: Tag(4) });
        let mut seen = BTreeMap::new();
        run_until_idle(&mut sim, &mut effects, |done, _| _ = seen.insert(done.tag, done.done));
        assert!(matches!(&seen[&Tag(1)], Done::Failed(why) if why.0.contains("refused")));
        assert_eq!(seen[&Tag(2)], Done::Unlistened);
        assert!(matches!(&seen[&Tag(3)], Done::Failed(_)));
        assert!(matches!(&seen[&Tag(4)], Done::Failed(_)));
        assert!(sim.dial(&at).is_err());
    }
}
