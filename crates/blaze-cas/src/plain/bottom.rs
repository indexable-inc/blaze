//! The plain store's bottom as a seam (D182, rule 78 `strategy-is-a-type-parameter`): the
//! driver the store's core is run on ([`super::Driven`]'s `D`) is a [`Bottom`], and what is
//! under the store's file effects is the bottom's choice.
//!
//! - [`OverDir`]: today's behaviour, every effect handed to the driver as it is: a file per
//!   segment, head, log and tag in the store's folder. A pure refactor: `Driven<OverDir<D>>`
//!   makes the calls `Driven<D>` makes. The default stays `Driven<Native>`, unwrapped.
//! - [`OverBlock`]: every file of the store an extent of ONE device file (a block device or a
//!   file standing in for one), the names, lengths and places in a table written as one of
//!   two alternating superblocks at the device's start. Not the default: the switch is the
//!   coordinator's, after a measurement.
//!
//! # OverBlock's layout
//!
//! Two slots of [`Limits::super_bytes`] at 0 and at `super_bytes`, then data from the first
//! [`Limits::align`] past them. A slot holds a generation, the live extents (each file's name,
//! place, capacity, length and change time), the folders, and a digest of the slot
//! (`ContentId` over every byte before it). Generation `g` is written to slot `g % 2`, so the
//! slot of `g - 1` stands while `g` is written: a torn write of `g` is read as `g - 1`. A slot
//! all zeros is generation 0, empty (a new device); a slot whose digest fails is not read. An
//! open reads both and takes the newest whole one; neither whole is refused by name
//! ([`BottomError::NoHead`]), never read as empty.
//!
//! # The free map
//!
//! Derived at open from the head's live extents, never stored: what no live, reserved or
//! dropped extent covers is free. An extent a name drops (a remove, a rename over it, a file
//! replaced or moved) is DROPPED, not free: it stays out of every allocation until the flip of
//! the first generation that does not name it is durable, since until then a power cut opens a
//! head that still names it. An extent this process mapped is not freed in this process at
//! all: a mapping reads the device's bytes under it, and another file's write there would
//! change what a reader holds. The next open (another command: no daemon) derives it free.
//!
//! # A flip
//!
//! Every `Sync` of a file, `SyncDir` and durable `Write`/`Rename` is a flip: the table taken
//! as the flip starts, then the device synced (the data before the head that names it), the
//! slot written, the device synced again; only then is the generation durable and what it
//! dropped free. Flips run one at a time in order; one not yet started takes every later
//! request, so a burst of syncs is one flip. Unsynced changes (`WriteUnsynced`,
//! `RenameUnsynced`, `Remove`, a `Truncate`) live in the table until the next flip, and a
//! power cut before it loses all of them together: within what a file system may do (each
//! may be lost), and a strict subset of its outcomes.
//!
//! # Writes
//!
//! Append and write in place within an extent; a write past an extent's capacity moves the
//! file whole to a new extent of twice the capacity (copy on grow: one read and one write),
//! the old dropped. A file grown by a truncate or a write past its end is written zeros there,
//! so a reused extent never shows another file's bytes. Every effect on one name runs after
//! the one before it on that name (a name's effects park while one is in flight); effects
//! on different names run at once.
//!
//! # One process
//!
//! The table is the process's: an open takes the device's exclusive lock and holds it for the
//! bottom's life, and an open that finds it held is refused ([`BottomError::Held`]). The
//! store's two-writer mode stays on [`OverDir`].
//!
//! # The syscalls saved, per operation (against [`OverDir`] on the native driver)
//!
//! | store operation | OverDir | OverBlock |
//! | --- | --- | --- |
//! | list the store's folder | `getdirentries` + a `stat` a file | none (the table) |
//! | rename (unsynced) | `rename` | none |
//! | remove | `unlink` | none |
//! | sync a folder | `open` + `fsync` + `close` of the folder | folded into the flip |
//! | durable whole write | `open` temp + `write` + `fsync` + `rename` + folder `fsync` | `pwrite` + the flip |
//! | a flip (any number of syncs merged) | one `fsync` a file + one a folder | `fdatasync` + `pwrite` + `fdatasync` |
//! | truncate | `ftruncate` | none (shrink), one `pwrite` of zeros (grow) |
//! | read, map | `open` + `pread`/`mmap` + `close` | the same on the device |
//!
//! The last row is today's native driver: on macOS (blaze-core `kqueue`, its opening
//! `threads::opening::Kept`, 2026-11-21) `ReadInto`, `ReadMany`, `Map`, `WriteBack` and a
//! queued `Read` go through the kept read-only descriptors (blaze-core `descriptors`);
//! `WriteAt`, `WriteFrom` and `Truncate` through the kept writable one; `Sync` and `Barrier`
//! of a file already there through the read-only one, and of a missing file by its name, which
//! makes it. `Append` opens its named file (`O_APPEND`; a positional descriptor would lose one
//! of two appends in flight). So a segment written in pieces, cut and sealed opens twice in a
//! driver's life, once each way, not once an effect. The names' calls (rows 1 to 5) are saved
//! by the table alone.
//!
//! # Proof
//!
//! `bottom_tests.rs`: one crash simulation run on both bottoms with one oracle (an
//! acknowledged record reads back with equal bytes; nothing half-visible; the head after a
//! cut one of the last two generations), a differential check of the two without cuts, and
//! three plants ([`BottomPlant`]).

use core::alloc::Allocator;
use core::fmt;
use core::time::Duration;

use blaze_core::{
    Boot, Completion, DirName, Done, Driver, Effect, Extent, FileName, IoError, LockMode, Meta, NoBoot, Tag, Waker,
    planted::{Planted, Plants},
    threads::inline::Inline,
    wait::Deadline,
};
use blaze_ids::ContentId;
use blaze_own::Share;

/// The store's driver: [`OverDir`] or [`OverBlock`], over the driver beneath.
pub trait Bottom: Driver + Sized {
    type Inner: Driver;
    /// The driver beneath, the bottom let go of (a simulation's power cut, then a reopen).
    fn into_inner(self) -> Self::Inner;
}

/// Today's bottom: every effect to the driver as it is.
pub struct OverDir<D: Driver> {
    inner: D,
}

impl<D: Driver> OverDir<D> {
    pub fn new(inner: D) -> OverDir<D> {
        OverDir { inner }
    }
}

impl<D: Driver> Driver for OverDir<D> {
    fn submit(&mut self, effects: &mut Vec<Effect>) {
        self.inner.submit(effects);
    }

    fn poll(&mut self, out: &mut Vec<Completion>, until: Deadline) -> usize {
        self.inner.poll(out, until)
    }

    fn in_flight(&self) -> usize {
        self.inner.in_flight()
    }

    fn now(&self) -> Duration {
        self.inner.now()
    }

    fn wall_ns(&self) -> u64 {
        self.inner.wall_ns()
    }

    fn boot(&self) -> Result<Boot, NoBoot> {
        self.inner.boot()
    }

    fn waker(&self) -> Option<Waker> {
        self.inner.waker()
    }

    fn inline_files(&self) -> Option<Inline> {
        self.inner.inline_files()
    }
}

impl<D: Driver> Bottom for OverDir<D> {
    type Inner = D;

    fn into_inner(self) -> D {
        self.inner
    }
}

/// The bounds of an [`OverBlock`], each a typed refusal when reached.
///
/// Sized by what the simulation draws (D165): `bottom_tests.rs` prints the high-water marks
/// ([`OverBlock::high_water`]) of every case; [`Limits::SIMULATED`] is set from them with
/// room, and [`Limits::store`] by the store's own bounds (UNSURE until a real store's marks
/// are read: the final report says so).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    /// The device's length.
    pub device_bytes: u64,
    /// A slot's length: the table of the most files and folders, and its digest.
    pub super_bytes: u32,
    /// Every extent starts at a multiple of this: 64 KiB natively (a page on every host, so a
    /// range of the device maps at its own start: 16 KiB on arm64 macOS; `blaze_format`'s
    /// 16-byte alignment with room), 4 KiB in the simulation.
    pub align: u64,
    /// The least capacity an extent is made with.
    pub extent_floor: u64,
    /// The most files the table holds.
    pub files: u32,
}

impl Limits {
    /// The simulation's: a device of 2 MiB, slots of 8 KiB. The drawn cases' store makes at
    /// most a head, a folder tag, a log and three segments a commit, compacted: under 64
    /// files, each under 64 KiB (values under 700 bytes).
    pub const SIMULATED: Limits =
        Limits { device_bytes: 2 << 20, super_bytes: 8 << 10, align: 4 << 10, extent_floor: 4 << 10, files: 64 };

    /// A real store's, its device `device_bytes` long: slots of 256 KiB hold 2048 files of
    /// names up to 64 bytes (98 bytes an entry) with room.
    pub const fn store(device_bytes: u64) -> Limits {
        Limits { device_bytes, super_bytes: 256 << 10, align: 64 << 10, extent_floor: 64 << 10, files: 2048 }
    }

    fn data_start(&self) -> DeviceAt {
        DeviceAt((2 * u64::from(self.super_bytes)).next_multiple_of(self.align))
    }

    fn device_end(&self) -> DeviceAt {
        DeviceAt(self.device_bytes)
    }
}

/// The bugs [`OverBlock`]'s controls plant.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BottomPlant {
    None,
    /// A flip writes its generation to both slots in one write and syncs once: a power cut in
    /// it can tear both, and an open reads an older generation than the last two, or none.
    BothSlotsInOneStep,
    /// A dropped extent is free at once, before the flip that stops naming it is durable: a
    /// file written there overwrites what the durable head still names.
    FreedBeforeFlip,
    /// The free map is stored in the slot and trusted at open; it is the map as of the flip
    /// before (as a stored map lags the table it describes), so the extents of the last
    /// generation's new files are handed out again.
    FreeMapTrusted,
}

impl Plants for BottomPlant {}

/// Why an [`OverBlock`] could not be opened or could not do what was asked. Each names the
/// device, and the file where there is one.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum BottomError {
    /// Another holder has the device's lock.
    Held { device: FileName },
    /// Neither slot is whole: digest or form.
    NoHead { device: FileName },
    /// The device is shorter than its two slots and not empty.
    Short { device: FileName, len: u64 },
    /// No run of the device is free for `bytes`.
    Full { device: FileName, file: FileName, bytes: u64 },
    /// The table outgrew a slot.
    TableFull { device: FileName, bytes: u64, super_bytes: u32 },
    /// The table holds [`Limits::files`] files.
    Files { device: FileName, file: FileName, most: u32 },
    /// The driver beneath failed `what` on the device.
    Inner { device: FileName, what: &'static str, why: IoError },
    /// The driver beneath went quiet for [`STALL`] during an open.
    Stalled { device: FileName, what: &'static str },
}

impl fmt::Display for BottomError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BottomError::Held { device } => write!(f, "{device}: the device's lock is held by another process (D182: one process a device)"),
            BottomError::NoHead { device } => write!(f, "{device}: neither superblock is whole: no generation to open (D182)"),
            BottomError::Short { device, len } => write!(f, "{device}: {len} bytes, shorter than its two superblocks and not empty"),
            BottomError::Full { device, file, bytes } => write!(f, "{device}: no free run of {bytes} bytes for {file}"),
            BottomError::TableFull { device, bytes, super_bytes } => {
                write!(f, "{device}: the table's {bytes} bytes do not fit a superblock of {super_bytes} (Limits::super_bytes)")
            }
            BottomError::Files { device, file, most } => write!(f, "{device}: {file} would be file {} of at most {most} (Limits::files)", most + 1),
            BottomError::Inner { device, what, why } => write!(f, "{device}: {what}: {why}"),
            BottomError::Stalled { device, what } => write!(f, "{device}: {what}: no completion for {STALL:?}"),
        }
    }
}

/// How long an open waits for one completion of its own.
pub const STALL: Duration = Duration::from_secs(60);

/// The bits of a tag the bottom's own effects carry: the store's are below 2^62 (its tags
/// are `n * TAGS` plus an op's 40 bits).
const OWN: u64 = 1 << 62;

const MAGIC: [u8; 8] = *b"BLZBOTM1";
const DIGEST: usize = 32;
const DIGEST_DOMAIN: &str = "blaze-cas bottom superblock";

/// A generation of the table: how many flips made it, 0 a new device's empty table. Its slot
/// is its parity, so the slot of the one before stands while it is written.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Debug)]
pub struct Generation(pub u64);

impl Generation {
    pub const EMPTY: Generation = Generation(0);

    pub fn next(self) -> Generation {
        Generation(self.0 + 1)
    }

    /// The slot it is written to: 0 or 1.
    fn slot(self) -> u64 {
        self.0 % 2
    }
}

impl fmt::Display for Generation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "generation {}", self.0)
    }
}

/// A byte offset on the device, never one within a file: an offset within a file reaches
/// the device only through [`Run::within`], which adds its extent's start. Every `Effect` on
/// the device takes [`DeviceAt::get`], and nothing else makes one from a file's offset.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
struct DeviceAt(u64);

impl DeviceAt {
    fn get(self) -> u64 {
        self.0
    }

    fn plus(self, bytes: u64) -> DeviceAt {
        DeviceAt(self.0 + bytes)
    }

    /// The bytes from `from` to here; `from` is at or before it.
    fn since(self, from: DeviceAt) -> u64 {
        self.0 - from.0
    }
}

/// A run of the device's bytes: an extent, `cap` long.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Run {
    at: DeviceAt,
    cap: u64,
}

impl Run {
    fn end(self) -> DeviceAt {
        self.at.plus(self.cap)
    }

    /// Byte `at` of the file in this extent, on the device. Every caller has bounded `at` by
    /// the file's length or the extent's capacity.
    fn within(self, at: u64) -> DeviceAt {
        debug_assert!(at <= self.cap, "an offset {at} past an extent of {}", self.cap);
        self.at.plus(at)
    }
}

/// A file of the table.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Placed {
    name: FileName,
    run: Run,
    len: u64,
    modified_ns: u64,
    /// This process mapped bytes of it: its run is never freed in this process.
    mapped: bool,
}

/// An extent no name holds, not yet free.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Dropped {
    run: Run,
    /// Free once this generation is durable (`None`: never in this process, it was mapped).
    after: Option<Generation>,
}

/// How a write changes its file's entry when it lands.
#[derive(Debug)]
enum Change {
    /// In place: the length at least `len`.
    Grew { len: u64 },
    /// The file's bytes are now `run`, `len` long; its old run is dropped.
    Placed { run: Run, len: u64 },
}

/// What a move does to the bytes it read before they are written to the new run.
#[derive(Debug)]
enum Edit {
    At { at: u64, bytes: Vec<u8> },
    Len(u64),
}

#[derive(Debug)]
enum Then {
    /// The inner's done is the outer's as it is (a read, a map, a barrier).
    Same,
    /// A write for `file` landed: the table changes, then `done` (after a flip, if `flip`).
    Landed { file: FileName, change: Change, done: Done, flip: bool },
    /// `file`'s bytes read to move them whole to `to`, `edit` applied.
    Moving { file: FileName, to: Run, edit: Edit, done: Done, flip: bool },
    /// The flip in flight's step.
    Flip,
}

struct Op {
    own: u64,
    outer: Tag,
    /// The name it holds busy.
    file: Option<FileName>,
    then: Then,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stage {
    Queued,
    SyncData,
    Slot,
    SyncSlot,
}

struct Flip<A: Allocator + Copy> {
    generation: Generation,
    stage: Stage,
    waiters: Vec<(Tag, Done), A>,
}

/// The most an [`OverBlock`] held at once ([`OverBlock::high_water`]).
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct HighWater {
    pub files: u32,
    pub end: u64,
    pub table_bytes: u64,
    pub generation: Generation,
}

/// The store's files as extents of one device file (the module's docs).
pub struct OverBlock<D: Driver, A: Allocator + Copy, P: Planted<BottomPlant>> {
    inner: D,
    alloc: A,
    plant: P,
    limits: Limits,
    device: FileName,
    /// The table, sorted by name.
    files: Vec<Placed, A>,
    /// The folders, sorted.
    dirs: Vec<FileName, A>,
    /// Runs taken by a write in flight that has not landed.
    reserved: Vec<Run, A>,
    dropped: Vec<Dropped, A>,
    /// The newest durable generation, and the next a flip takes.
    durable: Generation,
    next: Generation,
    /// The plant's stored map: written by each flip (the map as of the flip before), and the
    /// one read at open when it is trusted.
    stale_free: Vec<Run, A>,
    trusted_free: Option<Vec<Run, A>>,
    ops: Vec<Op, A>,
    flips: Vec<Flip<A>, A>,
    busy: Vec<FileName, A>,
    /// blaze-core's own queues ([`Driver::submit`], [`Driver::poll`]), as `Driven`'s are.
    parked: Vec<Effect>,
    out: Vec<Effect>,
    done: Vec<Completion>,
    ready: Vec<Completion>,
    /// Effects taken and not yet handed back complete.
    pending: usize,
    tags: u64,
    /// The tag the device's lock is held under.
    lock: Tag,
    high: HighWater,
}

impl<D: Driver, A: Allocator + Copy, P: Planted<BottomPlant>> OverBlock<D, A, P> {
    /// The bottom over `device` (a file of `inner`'s, made at `limits.device_bytes` if it is
    /// not there), its lock taken, its head read; what it keeps in `alloc`.
    pub fn open(inner: D, device: FileName, limits: Limits, alloc: A, plant: P) -> Result<OverBlock<D, A, P>, BottomError> {
        let mut it = OverBlock {
            inner,
            plant,
            limits,
            device,
            files: Vec::new_in(alloc),
            dirs: Vec::new_in(alloc),
            reserved: Vec::new_in(alloc),
            dropped: Vec::new_in(alloc),
            durable: Generation::EMPTY,
            next: Generation::EMPTY.next(),
            stale_free: Vec::new_in(alloc),
            trusted_free: None,
            ops: Vec::new_in(alloc),
            flips: Vec::new_in(alloc),
            busy: Vec::new_in(alloc),
            parked: Vec::new(),
            out: Vec::new(),
            done: Vec::new(),
            ready: Vec::new(),
            pending: 0,
            tags: 0,
            lock: Tag(OWN),
            high: HighWater::default(),
            alloc,
        };
        // clone: a name, a refcount.
        let device = it.device.share();
        it.lock = it.tag();
        let lock = it.lock;
        match it.own(|file, _| Effect::Lock { file, mode: LockMode::Exclusive, tag: lock }, lock, "taking the device's lock")? {
            Done::Locked => {}
            Done::Contended => return Err(BottomError::Held { device }),
            other => return Err(it.inner_error("taking the device's lock", &other)),
        }
        let both = 2 * it.limits.super_bytes as usize;
        let tag = it.tag();
        let slots = match it.own(|file, tag| Effect::Read { file, at: 0, len: both, tag }, tag, "reading the superblocks")? {
            Done::Read(bytes) => bytes,
            other => return Err(it.inner_error("reading the superblocks", &other)),
        };
        if slots.len() < both {
            if slots.iter().any(|b| *b != 0) {
                return Err(BottomError::Short { device, len: slots.len() as u64 });
            }
            it.make()?;
            it.stale_free = it.free_runs();
            return Ok(it);
        }
        let half = it.limits.super_bytes as usize;
        let read = [it.decode(&slots[..half]), it.decode(&slots[half..])];
        let newest = match read {
            [Some(a), Some(b)] => Some(if a.generation >= b.generation { a } else { b }),
            [Some(a), None] | [None, Some(a)] => Some(a),
            [None, None] => None,
        };
        let Some(head) = newest else { return Err(BottomError::NoHead { device }) };
        it.durable = head.generation;
        it.next = head.generation.next();
        it.files = head.files;
        it.dirs = head.dirs;
        if it.plant.has(BottomPlant::FreeMapTrusted) {
            it.trusted_free = Some(head.free);
            it.stale_free = it.free_runs();
        }
        it.mark();
        Ok(it)
    }

    /// The newest durable generation.
    pub fn generation(&self) -> Generation {
        self.durable
    }

    /// The newest generation a flip has started (its slot may be on the device after a cut).
    pub fn started(&self) -> Generation {
        // `next` is past the durable one from the open on: never generation 0.
        Generation(self.next.0 - 1)
    }

    pub fn high_water(&self) -> HighWater {
        self.high
    }

    pub fn inner(&mut self) -> &mut D {
        &mut self.inner
    }

    /// The bottom let go of in order: nothing in flight, the device's lock released; the
    /// driver beneath back.
    pub fn close(mut self) -> Result<D, BottomError> {
        let lock = self.lock;
        let tag = self.tag();
        let done = self.own(|_, tag| Effect::Unlock { lock, tag }, tag, "releasing the device's lock")?;
        match done {
            Done::Unlocked => Ok(self.inner),
            other => Err(self.inner_error("releasing the device's lock", &other)),
        }
    }

    /// A new device: its length set, synced, and its name made durable.
    fn make(&mut self) -> Result<(), BottomError> {
        let len = self.limits.device_bytes;
        self.expect(|file, tag| Effect::Truncate { file, len, tag }, "making the device", |d| matches!(d, Done::Truncated))?;
        self.expect(|file, tag| Effect::Sync { file, tag }, "syncing the new device", |d| matches!(d, Done::Synced))?;
        let dir = match self.device.as_str().rsplit_once('/').and_then(|(parent, _)| FileName::new(parent)) {
            Some(parent) => DirName::Named(parent),
            None => DirName::Root,
        };
        self.expect(|_, tag| Effect::SyncDir { dir, tag }, "syncing the new device's name", |d| matches!(d, Done::Synced))
    }

    fn expect(&mut self, make: impl FnOnce(FileName, Tag) -> Effect, what: &'static str, ok: impl Fn(&Done) -> bool) -> Result<(), BottomError> {
        let tag = self.tag();
        let done = self.own(make, tag, what)?;
        if ok(&done) { Ok(()) } else { Err(self.inner_error(what, &done)) }
    }

    fn inner_error(&self, what: &'static str, done: &Done) -> BottomError {
        let why = match done {
            Done::Failed(why) => why.clone(),
            // alloc: an error's message.
            other => IoError(format!("an unexpected completion: {other:?}")),
        };
        // clone: a name, a refcount.
        BottomError::Inner { device: self.device.share(), what, why }
    }

    /// One effect of the bottom's own on the device, under `tag`, run to its completion: an
    /// open's and a close's, when nothing of the store's is in flight.
    fn own(&mut self, make: impl FnOnce(FileName, Tag) -> Effect, tag: Tag, what: &'static str) -> Result<Done, BottomError> {
        // clone: a name, a refcount.
        self.out.push(make(self.device.share(), tag));
        self.inner.submit(&mut self.out);
        let mut since = self.inner.now();
        loop {
            let stall = Deadline::after(since, STALL);
            if self.inner.poll(&mut self.done, stall) > 0 {
                since = self.inner.now();
            } else if stall.passed(self.inner.now()) {
                // clone: a name, a refcount.
                return Err(BottomError::Stalled { device: self.device.share(), what });
            }
            if let Some(at) = self.done.iter().position(|c| c.tag == tag) {
                return Ok(self.done.swap_remove(at).done);
            }
        }
    }

    fn tag(&mut self) -> Tag {
        self.tags += 1;
        Tag(OWN | self.tags)
    }

    // ---- the table ----

    fn find(&self, name: &FileName) -> Result<usize, usize> {
        self.files.binary_search_by(|it| it.name.as_str().cmp(name.as_str()))
    }

    fn placed(&self, name: &FileName) -> Option<&Placed> {
        self.find(name).ok().map(|at| &self.files[at])
    }

    fn add_dirs(&mut self, name: &FileName) {
        let spelled = name.as_str();
        for (at, _) in spelled.match_indices('/') {
            let Some(dir) = FileName::new(&spelled[..at]) else { continue };
            if let Err(slot) = self.dirs.binary_search_by(|it| it.as_str().cmp(dir.as_str())) {
                self.dirs.insert(slot, dir);
            }
        }
    }

    fn drop_run(&mut self, run: Run, mapped: bool) {
        if self.plant.has(BottomPlant::FreedBeforeFlip) && !mapped {
            return;
        }
        // The first generation that does not name it is the next a flip takes.
        self.dropped.push(Dropped { run, after: if mapped { None } else { Some(self.next) } });
    }

    /// The file's entry set to `run`, `len` long; the run it had dropped.
    fn place(&mut self, file: FileName, run: Run, len: u64) {
        self.reserved.retain(|it| *it != run);
        let modified_ns = self.inner.wall_ns();
        match self.find(&file) {
            Ok(at) => {
                let old = core::mem::replace(&mut self.files[at], Placed { name: file, run, len, modified_ns, mapped: false });
                self.drop_run(old.run, old.mapped);
            }
            Err(at) => {
                self.add_dirs(&file);
                self.files.insert(at, Placed { name: file, run, len, modified_ns, mapped: false });
            }
        }
        self.mark();
    }

    fn mark(&mut self) {
        self.high.files = self.high.files.max(self.files.len() as u32);
        let end = self.files.iter().map(|it| it.run.end()).chain(self.reserved.iter().map(|it| it.end())).max();
        self.high.end = self.high.end.max(end.map_or(0, DeviceAt::get));
        self.high.generation = self.high.generation.max(self.started());
    }

    /// A free run of at least `bytes` for `file`, reserved.
    fn alloc(&mut self, file: &FileName, bytes: u64) -> Result<Run, BottomError> {
        let cap = bytes.max(self.limits.extent_floor).next_multiple_of(self.limits.align);
        if self.placed(file).is_none() && self.files.len() as u32 >= self.limits.files {
            // clone: names, refcounts.
            return Err(BottomError::Files { device: self.device.share(), file: file.share(), most: self.limits.files });
        }
        let found = match &mut self.trusted_free {
            // The plant: the stored map trusted, nothing checked against the table.
            Some(free) => match free.iter().position(|it| it.cap >= cap) {
                Some(at) => {
                    let run = Run { at: free[at].at, cap };
                    free[at] = Run { at: run.end(), cap: free[at].cap - cap };
                    Some(run)
                }
                None => None,
            },
            None => self.first_fit(cap),
        };
        match found {
            Some(run) => {
                self.reserved.push(run);
                self.mark();
                Ok(run)
            }
            // clone: names, refcounts.
            None => Err(BottomError::Full { device: self.device.share(), file: file.share(), bytes: cap }),
        }
    }

    /// Every run no live, reserved or dropped extent covers, in order.
    fn free_runs(&self) -> Vec<Run, A> {
        let mut used = Vec::with_capacity_in(self.files.len() + self.reserved.len() + self.dropped.len(), self.alloc);
        used.extend(self.files.iter().map(|it| it.run));
        used.extend(self.reserved.iter().copied());
        used.extend(self.dropped.iter().map(|it| it.run));
        used.sort_unstable_by_key(|it| it.at);
        let mut free = Vec::new_in(self.alloc);
        let mut at = self.limits.data_start();
        for run in used {
            if run.at > at {
                free.push(Run { at, cap: run.at.since(at) });
            }
            at = at.max(run.end());
        }
        let end = self.limits.device_end();
        if end > at {
            free.push(Run { at, cap: end.since(at) });
        }
        free
    }

    fn first_fit(&self, cap: u64) -> Option<Run> {
        self.free_runs().into_iter().find(|it| it.cap >= cap).map(|it| Run { at: it.at, cap })
    }

    // ---- the superblock ----

    fn slot_at(&self, generation: Generation) -> u64 {
        generation.slot() * u64::from(self.limits.super_bytes)
    }

    /// The table as `generation`'s slot.
    fn encode(&mut self, generation: Generation) -> Result<Vec<u8>, BottomError> {
        let size = self.limits.super_bytes as usize;
        // Effect::WriteAt's bytes are blaze-core's process-heap vector.
        let mut slot = Vec::with_capacity(size);
        slot.extend_from_slice(&MAGIC);
        slot.extend_from_slice(&generation.0.to_le_bytes());
        let free = match self.plant.has(BottomPlant::FreeMapTrusted) {
            true => {
                let now = self.free_runs();
                core::mem::replace(&mut self.stale_free, now)
            }
            false => Vec::new_in(self.alloc),
        };
        for n in [self.files.len(), self.dirs.len(), free.len(), 0] {
            slot.extend_from_slice(&(n as u32).to_le_bytes());
        }
        for it in &self.files {
            put_name(&mut slot, &it.name);
            for n in [it.run.at.get(), it.run.cap, it.len, it.modified_ns] {
                slot.extend_from_slice(&n.to_le_bytes());
            }
        }
        for dir in &self.dirs {
            put_name(&mut slot, dir);
        }
        for run in &free {
            slot.extend_from_slice(&run.at.get().to_le_bytes());
            slot.extend_from_slice(&run.cap.to_le_bytes());
        }
        self.high.table_bytes = self.high.table_bytes.max(slot.len() as u64);
        if slot.len() + DIGEST > size {
            // clone: a name, a refcount.
            return Err(BottomError::TableFull { device: self.device.share(), bytes: slot.len() as u64, super_bytes: self.limits.super_bytes });
        }
        slot.resize(size - DIGEST, 0);
        let digest = ContentId::of(DIGEST_DOMAIN, &slot);
        slot.extend_from_slice(&digest.0);
        Ok(slot)
    }

    /// A slot read back: `None` when it is not whole.
    fn decode(&self, slot: &[u8]) -> Option<Head<A>> {
        if slot.iter().all(|b| *b == 0) {
            return Some(Head { generation: Generation::EMPTY, files: Vec::new_in(self.alloc), dirs: Vec::new_in(self.alloc), free: Vec::new_in(self.alloc) });
        }
        let body = slot.len().checked_sub(DIGEST)?;
        if ContentId::of(DIGEST_DOMAIN, &slot[..body]).0[..] != slot[body..] || slot[..8] != MAGIC {
            return None;
        }
        let mut r = Reader { bytes: &slot[..body], at: 8 };
        let generation = Generation(r.u64()?);
        let (files, dirs, free) = (r.u32()?, r.u32()?, r.u32()?);
        r.u32()?;
        let mut head = Head {
            generation,
            files: Vec::with_capacity_in(files as usize, self.alloc),
            dirs: Vec::with_capacity_in(dirs as usize, self.alloc),
            free: Vec::with_capacity_in(free as usize, self.alloc),
        };
        for _ in 0..files {
            let name = r.name()?;
            let (at, cap, len, modified_ns) = (r.u64()?, r.u64()?, r.u64()?, r.u64()?);
            head.files.push(Placed { name, run: Run { at: DeviceAt(at), cap }, len, modified_ns, mapped: false });
        }
        for _ in 0..dirs {
            head.dirs.push(r.name()?);
        }
        for _ in 0..free {
            head.free.push(Run { at: DeviceAt(r.u64()?), cap: r.u64()? });
        }
        Some(head)
    }

    // ---- effects ----

    /// The names `effect` acts on.
    fn names(effect: &Effect) -> [Option<&FileName>; 2] {
        match effect {
            Effect::ReadAll { file, .. }
            | Effect::Read { file, .. }
            | Effect::Append { file, .. }
            | Effect::Sync { file, .. }
            | Effect::Barrier { file, .. }
            | Effect::Truncate { file, .. }
            | Effect::Write { file, .. }
            | Effect::WriteUnsynced { file, .. }
            | Effect::WriteAt { file, .. }
            | Effect::Remove { file, .. }
            | Effect::Map { file, .. } => [Some(file), None],
            Effect::Rename { from, to, .. } | Effect::RenameUnsynced { from, to, .. } => [Some(from), Some(to)],
            _ => [None, None],
        }
    }

    fn is_busy(&self, effect: &Effect) -> bool {
        Self::names(effect).into_iter().flatten().any(|name| self.busy.iter().any(|it| it == name))
    }

    fn start(&mut self, effect: Effect) {
        if self.is_busy(&effect) {
            self.parked.push(effect);
            return;
        }
        match effect {
            Effect::Lock { .. } | Effect::Unlock { .. } | Effect::Timer { .. } => self.out.push(effect),
            Effect::ReadAll { file, tag } => match self.placed(&file) {
                None => self.answer(tag, Done::Read(Vec::new())),
                Some(p) => {
                    let (at, len) = (p.run.within(0), p.len as usize);
                    self.inner_op(tag, Some(file), Then::Same, |device, own| Effect::Read { file: device, at: at.get(), len, tag: own });
                }
            },
            Effect::Read { file, at, len, tag } => match self.placed(&file) {
                None => self.answer(tag, Done::Read(Vec::new())),
                Some(p) if at >= p.len => self.answer(tag, Done::Read(Vec::new())),
                Some(p) => {
                    let (from, len) = (p.run.within(at), len.min((p.len - at) as usize));
                    self.inner_op(tag, Some(file), Then::Same, |device, own| Effect::Read { file: device, at: from.get(), len, tag: own });
                }
            },
            Effect::Map { file, extent, tag } => {
                let Ok(n) = self.find(&file) else {
                    // alloc: an error's message.
                    return self.answer(tag, Done::Failed(IoError(format!("{file}: no such file to map (OverBlock, D182)"))));
                };
                let p = &mut self.files[n];
                let (at, len) = match extent {
                    Extent::Whole => (0, p.len),
                    Extent::Range { at, len } => (at, len),
                };
                if at.checked_add(len).is_none_or(|end| end > p.len) {
                    let have = p.len;
                    // alloc: an error's message.
                    let why = format!("{file}: a mapping of {len} bytes at {at} runs past the file's end at {have}: a range is mapped only inside the file");
                    return self.answer(tag, Done::Failed(IoError(why)));
                }
                p.mapped = true;
                let from = p.run.within(at);
                self.inner_op(tag, Some(file), Then::Same, |device, own| Effect::Map { file: device, extent: Extent::Range { at: from.get(), len }, tag: own });
            }
            Effect::Barrier { file, tag } => {
                self.inner_op(tag, Some(file), Then::Same, |device, own| Effect::Barrier { file: device, tag: own });
            }
            Effect::WriteAt { file, at, bytes, tag } => self.write_at(file, at, bytes, Done::Written, tag),
            Effect::Append { file, bytes, tag } => {
                let at = self.placed(&file).map_or(0, |p| p.len);
                self.write_at(file, at, bytes, Done::Appended, tag);
            }
            Effect::Truncate { file, len, tag } => self.truncate(file, len, tag),
            Effect::Write { file, bytes, tag } => self.write_whole(file, bytes, true, tag),
            Effect::WriteUnsynced { file, bytes, tag } => self.write_whole(file, bytes, false, tag),
            Effect::Rename { from, to, tag } => self.rename(from, to, true, tag),
            Effect::RenameUnsynced { from, to, tag } => self.rename(from, to, false, tag),
            Effect::Remove { file, tag } => {
                match self.find(&file) {
                    Ok(at) => {
                        let old = self.files.remove(at);
                        self.drop_run(old.run, old.mapped);
                    }
                    Err(_) => {
                        // A folder with nothing under it goes; one with files stays, as a
                        // file system refuses to remove it (the store removes files).
                        let prefix = file.as_str();
                        let under = self.files.iter().any(|it| it.name.as_str().strip_prefix(prefix).is_some_and(|rest| rest.starts_with('/')));
                        if !under {
                            self.dirs.retain(|it| it != &file);
                        }
                    }
                }
                self.answer(tag, Done::Removed);
            }
            Effect::Sync { tag, .. } | Effect::SyncDir { tag, .. } => self.flip(tag, Done::Synced),
            Effect::List { dir, tag } => self.list(dir, tag),
            other => {
                let tag = tag_of(&other);
                // alloc: an error's message. No fallback: an effect the table does not carry
                // is refused by name, never handed to the folder beneath.
                let why = format!("OverBlock carries no effect of this kind (D182): {}", kind_of(&other));
                self.answer(tag, Done::Failed(IoError(why)));
            }
        }
    }

    fn answer(&mut self, tag: Tag, done: Done) {
        self.ready.push(Completion { tag, done });
    }

    /// One effect on the device for the outer `tag`, `file` held busy until it ends.
    fn inner_op(&mut self, outer: Tag, file: Option<FileName>, then: Then, make: impl FnOnce(FileName, Tag) -> Effect) {
        let own = self.tag();
        if let Some(file) = &file {
            // clone: a name, a refcount.
            self.busy.push(file.share());
        }
        // clone: a name, a refcount.
        self.out.push(make(self.device.share(), own));
        self.ops.push(Op { own: own.0, outer, file, then });
    }

    fn write_at(&mut self, file: FileName, at: u64, bytes: Vec<u8>, done: Done, tag: Tag) {
        let need = at + bytes.len() as u64;
        let found = self.placed(&file).map(|p| (p.run, p.len));
        match found {
            None => match self.alloc(&file, need) {
                Ok(run) => {
                    let mut whole = vec![0u8; at as usize];
                    whole.extend_from_slice(&bytes);
                    let change = Change::Placed { run, len: need };
                    // clone: a name, a refcount.
                    self.inner_op(tag, Some(file.share()), Then::Landed { file, change, done, flip: false }, |device, own| {
                        Effect::WriteAt { file: device, at: run.within(0).get(), bytes: whole, tag: own }
                    });
                }
                Err(why) => self.refuse(tag, why),
            },
            Some((run, len)) if need <= run.cap => {
                // Past the end: the gap written zeros, so a reused run shows none of its last
                // file's bytes.
                let (from, bytes) = match at > len {
                    true => {
                        let mut padded = vec![0u8; (at - len) as usize];
                        padded.extend_from_slice(&bytes);
                        (len, padded)
                    }
                    false => (at, bytes),
                };
                let change = Change::Grew { len: need.max(len) };
                // clone: a name, a refcount.
                self.inner_op(tag, Some(file.share()), Then::Landed { file, change, done, flip: false }, |device, own| {
                    Effect::WriteAt { file: device, at: run.within(from).get(), bytes, tag: own }
                });
            }
            Some(had) => self.move_file(file, had, Edit::At { at, bytes }, done, tag),
        }
    }

    fn truncate(&mut self, file: FileName, len: u64, tag: Tag) {
        let found = self.placed(&file).map(|p| (p.run, p.len));
        match found {
            None => match self.alloc(&file, len) {
                Ok(run) if len == 0 => {
                    self.place(file, run, 0);
                    self.answer(tag, Done::Truncated);
                }
                Ok(run) => {
                    let change = Change::Placed { run, len };
                    // clone: a name, a refcount.
                    self.inner_op(tag, Some(file.share()), Then::Landed { file, change, done: Done::Truncated, flip: false }, |device, own| {
                        Effect::WriteAt { file: device, at: run.within(0).get(), bytes: vec![0u8; len as usize], tag: own }
                    });
                }
                Err(why) => self.refuse(tag, why),
            },
            Some((_, had)) if len <= had => {
                if let Ok(at) = self.find(&file) {
                    self.files[at].len = len;
                }
                self.answer(tag, Done::Truncated);
            }
            Some((run, had)) if len <= run.cap => {
                let change = Change::Grew { len };
                // clone: a name, a refcount.
                self.inner_op(tag, Some(file.share()), Then::Landed { file, change, done: Done::Truncated, flip: false }, |device, own| {
                    Effect::WriteAt { file: device, at: run.within(had).get(), bytes: vec![0u8; (len - had) as usize], tag: own }
                });
            }
            Some(had) => self.move_file(file, had, Edit::Len(len), Done::Truncated, tag),
        }
    }

    fn write_whole(&mut self, file: FileName, bytes: Vec<u8>, durable: bool, tag: Tag) {
        let len = bytes.len() as u64;
        match self.alloc(&file, len) {
            Ok(run) => {
                let change = Change::Placed { run, len };
                // clone: a name, a refcount.
                self.inner_op(tag, Some(file.share()), Then::Landed { file, change, done: Done::Written, flip: durable }, |device, own| {
                    Effect::WriteAt { file: device, at: run.within(0).get(), bytes, tag: own }
                });
            }
            Err(why) => self.refuse(tag, why),
        }
    }

    /// `file` (at `run`, `len` long) moved whole to a new run of twice its capacity (or what
    /// `edit` needs, if more), `edit` applied.
    fn move_file(&mut self, file: FileName, (run, len): (Run, u64), edit: Edit, done: Done, tag: Tag) {
        let need = match &edit {
            Edit::At { at, bytes } => at + bytes.len() as u64,
            Edit::Len(len) => *len,
        };
        match self.alloc(&file, need.max(2 * run.cap)) {
            Ok(to) => {
                // clone: a name, a refcount.
                self.inner_op(tag, Some(file.share()), Then::Moving { file, to, edit, done, flip: false }, |device, own| {
                    Effect::Read { file: device, at: run.within(0).get(), len: len as usize, tag: own }
                });
            }
            Err(why) => self.refuse(tag, why),
        }
    }

    fn rename(&mut self, from: FileName, to: FileName, durable: bool, tag: Tag) {
        let Ok(at) = self.find(&from) else {
            // alloc: an error's message.
            return self.answer(tag, Done::Failed(IoError(format!("{from}: no such file to rename to {to} (OverBlock, D182)"))));
        };
        let mut moved = self.files.remove(at);
        if let Ok(over) = self.find(&to) {
            let old = self.files.remove(over);
            self.drop_run(old.run, old.mapped);
        }
        self.add_dirs(&to);
        moved.name = to;
        moved.modified_ns = self.inner.wall_ns();
        if let Err(slot) = self.find(&moved.name) {
            self.files.insert(slot, moved);
        }
        match durable {
            true => self.flip(tag, Done::Renamed),
            false => self.answer(tag, Done::Renamed),
        }
    }

    fn list(&mut self, dir: DirName, tag: Tag) {
        let prefix = dir.name().map(|it| it.as_str());
        let under = |name: &str| match prefix {
            None => true,
            Some(prefix) => name.strip_prefix(prefix).is_some_and(|rest| rest.starts_with('/')),
        };
        if let Some(prefix) = prefix
            && !self.dirs.iter().any(|it| it.as_str() == prefix)
        {
            // alloc: an error's message.
            return self.answer(tag, Done::Failed(IoError(format!("{prefix}: no such folder to list (OverBlock, D182)"))));
        }
        // Done::Listed's vector is blaze-core's process-heap one.
        let mut listed: Vec<(FileName, Meta)> = Vec::new();
        for it in self.dirs.iter().filter(|it| under(it.as_str())) {
            // clone: a name, a refcount.
            listed.push((it.share(), Meta { len: 0, dir: true, modified_ns: 0, changed_ns: 0, ino: 0, dev: 0 }));
        }
        for it in self.files.iter().filter(|it| under(it.name.as_str())) {
            let ino = it.run.at.get() / self.limits.align + 1;
            let meta = Meta { len: it.len, dir: false, modified_ns: it.modified_ns, changed_ns: it.modified_ns, ino, dev: 0 };
            // clone: a name, a refcount.
            listed.push((it.name.share(), meta));
        }
        listed.sort_unstable_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
        self.answer(tag, Done::Listed(listed));
    }

    fn refuse(&mut self, tag: Tag, why: BottomError) {
        // alloc: an error's message.
        self.answer(tag, Done::Failed(IoError(why.to_string())));
    }

    // ---- flips ----

    /// `done` for `tag` once a flip that starts after now is durable.
    fn flip(&mut self, tag: Tag, done: Done) {
        if let Some(last) = self.flips.last_mut()
            && last.stage == Stage::Queued
        {
            last.waiters.push((tag, done));
            return;
        }
        let mut waiters = Vec::new_in(self.alloc);
        waiters.push((tag, done));
        // Its generation is the next one free when it starts, not now.
        self.flips.push(Flip { generation: Generation::EMPTY, stage: Stage::Queued, waiters });
        if self.flips.len() == 1 {
            self.start_flip();
        }
    }

    fn start_flip(&mut self) {
        let Some(flip) = self.flips.first_mut() else { return };
        flip.generation = self.next;
        flip.stage = Stage::SyncData;
        self.next = self.next.next();
        self.mark();
        let own = self.tag();
        // clone: a name, a refcount.
        self.out.push(Effect::Sync { file: self.device.share(), tag: own });
        self.ops.push(Op { own: own.0, outer: Tag(0), file: None, then: Then::Flip });
    }

    fn flip_step(&mut self, done: Done) {
        let Some(flip) = self.flips.first() else { return };
        let (generation, stage) = (flip.generation, flip.stage);
        let ok = match (stage, &done) {
            (Stage::SyncData | Stage::SyncSlot, Done::Synced) | (Stage::Slot, Done::Written) => true,
            _ => false,
        };
        if !ok {
            let why = match done {
                Done::Failed(why) => why,
                // alloc: an error's message.
                other => IoError(format!("{}: a flip's step ended {other:?}", self.device)),
            };
            return self.end_flip(Err(why));
        }
        match stage {
            Stage::SyncData => match self.encode(generation) {
                Ok(slot) => {
                    let (at, bytes) = match self.plant.has(BottomPlant::BothSlotsInOneStep) {
                        true => {
                            // clone: the plant's second copy of the slot.
                            let mut both = slot.clone();
                            both.extend_from_slice(&slot);
                            (0, both)
                        }
                        false => (self.slot_at(generation), slot),
                    };
                    self.set_stage(Stage::Slot);
                    let own = self.tag();
                    // clone: a name, a refcount.
                    self.out.push(Effect::WriteAt { file: self.device.share(), at, bytes, tag: own });
                    self.ops.push(Op { own: own.0, outer: Tag(0), file: None, then: Then::Flip });
                }
                // alloc: an error's message.
                Err(why) => self.end_flip(Err(IoError(why.to_string()))),
            },
            // Under the plant too: its one write held both slots, and one sync follows it.
            Stage::Slot => {
                self.set_stage(Stage::SyncSlot);
                self.flip_sync();
            }
            Stage::SyncSlot => {
                self.durable = generation;
                let durable = self.durable;
                self.dropped.retain(|it| it.after.is_none_or(|after| after > durable));
                self.end_flip(Ok(()));
            }
            Stage::Queued => {}
        }
    }

    fn set_stage(&mut self, stage: Stage) {
        if let Some(flip) = self.flips.first_mut() {
            flip.stage = stage;
        }
    }

    fn flip_sync(&mut self) {
        let own = self.tag();
        // clone: a name, a refcount.
        self.out.push(Effect::Sync { file: self.device.share(), tag: own });
        self.ops.push(Op { own: own.0, outer: Tag(0), file: None, then: Then::Flip });
    }

    fn end_flip(&mut self, how: Result<(), IoError>) {
        let flip = self.flips.remove(0);
        for (tag, done) in flip.waiters {
            let done = match &how {
                Ok(()) => done,
                Err(why) => Done::Failed(why.clone()),
            };
            self.ready.push(Completion { tag, done });
        }
        if !self.flips.is_empty() {
            self.start_flip();
        }
    }

    // ---- completions ----

    fn landed(&mut self, completion: Completion) {
        if completion.tag.0 & OWN == 0 {
            // A lock's, an unlock's or a timer's: the store's own tag, handed back as it is.
            self.ready.push(completion);
            return;
        }
        let Some(at) = self.ops.iter().position(|it| it.own == completion.tag.0) else {
            // The open's own (none in flight after it) or a stray: nothing waits on it.
            return;
        };
        let op = self.ops.swap_remove(at);
        let done = completion.done;
        if let Some(file) = &op.file {
            if let Some(at) = self.busy.iter().position(|it| it == file) {
                self.busy.swap_remove(at);
            }
        }
        match op.then {
            Then::Same => self.ready.push(Completion { tag: op.outer, done }),
            Then::Flip => self.flip_step(done),
            Then::Landed { file, change, done: then, flip } => match done {
                Done::Written => {
                    match change {
                        Change::Placed { run, len } => self.place(file, run, len),
                        Change::Grew { len } => {
                            let modified_ns = self.inner.wall_ns();
                            if let Ok(at) = self.find(&file) {
                                let p = &mut self.files[at];
                                p.len = p.len.max(len);
                                p.modified_ns = modified_ns;
                            }
                        }
                    }
                    match flip {
                        true => self.flip(op.outer, then),
                        false => self.ready.push(Completion { tag: op.outer, done: then }),
                    }
                }
                failed => {
                    if let Change::Placed { run, .. } = change {
                        self.reserved.retain(|it| *it != run);
                    }
                    self.ready.push(Completion { tag: op.outer, done: failed });
                }
            },
            Then::Moving { file, to, edit, done: then, flip } => match done {
                Done::Read(mut bytes) => {
                    let len = match edit {
                        Edit::At { at, bytes: new } => {
                            let end = at as usize + new.len();
                            if bytes.len() < end {
                                bytes.resize(end, 0);
                            }
                            bytes[at as usize..end].copy_from_slice(&new);
                            bytes.len() as u64
                        }
                        Edit::Len(len) => {
                            bytes.resize(len as usize, 0);
                            len
                        }
                    };
                    let change = Change::Placed { run: to, len };
                    // clone: a name, a refcount.
                    self.inner_op(op.outer, Some(file.share()), Then::Landed { file, change, done: then, flip }, |device, own| {
                        Effect::WriteAt { file: device, at: to.within(0).get(), bytes, tag: own }
                    });
                }
                failed => {
                    self.reserved.retain(|it| *it != to);
                    self.ready.push(Completion { tag: op.outer, done: failed });
                }
            },
        }
    }

    /// Parked effects whose names are free now, started in their order.
    fn unpark(&mut self) {
        if self.parked.is_empty() {
            return;
        }
        let parked = core::mem::take(&mut self.parked);
        for effect in parked {
            self.start(effect);
        }
    }
}

impl<D: Driver, A: Allocator + Copy, P: Planted<BottomPlant>> Driver for OverBlock<D, A, P> {
    fn submit(&mut self, effects: &mut Vec<Effect>) {
        for effect in effects.drain(..) {
            self.pending += 1;
            self.start(effect);
        }
        self.inner.submit(&mut self.out);
    }

    fn poll(&mut self, out: &mut Vec<Completion>, until: Deadline) -> usize {
        // Bounded: each turn hands back what is ready or takes at least one completion from
        // beneath, and every effect's chain of steps is at most four long.
        loop {
            if !self.ready.is_empty() || self.inner.in_flight() == 0 {
                let n = self.ready.len();
                self.pending -= n;
                out.append(&mut self.ready);
                return n;
            }
            let got = self.inner.poll(&mut self.done, until);
            let mut done = core::mem::take(&mut self.done);
            for completion in done.drain(..) {
                self.landed(completion);
            }
            self.done = done;
            self.unpark();
            self.inner.submit(&mut self.out);
            if got == 0 && self.ready.is_empty() {
                return 0;
            }
        }
    }

    fn in_flight(&self) -> usize {
        self.pending
    }

    fn now(&self) -> Duration {
        self.inner.now()
    }

    fn wall_ns(&self) -> u64 {
        self.inner.wall_ns()
    }

    fn boot(&self) -> Result<Boot, NoBoot> {
        self.inner.boot()
    }

    fn waker(&self) -> Option<Waker> {
        self.inner.waker()
    }

    /// None: an inline file call would name the store's files to the folder beneath, around
    /// the table.
    fn inline_files(&self) -> Option<Inline> {
        None
    }
}

impl<D: Driver, A: Allocator + Copy, P: Planted<BottomPlant>> Bottom for OverBlock<D, A, P> {
    type Inner = D;

    fn into_inner(self) -> D {
        self.inner
    }
}

struct Head<A: Allocator + Copy> {
    generation: Generation,
    files: Vec<Placed, A>,
    dirs: Vec<FileName, A>,
    free: Vec<Run, A>,
}

struct Reader<'b> {
    bytes: &'b [u8],
    at: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let end = self.at.checked_add(n)?;
        let got = self.bytes.get(self.at..end)?;
        self.at = end;
        Some(got)
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn name(&mut self) -> Option<FileName> {
        let len = u16::from_le_bytes(self.take(2)?.try_into().ok()?) as usize;
        FileName::new(core::str::from_utf8(self.take(len)?).ok()?)
    }
}

fn put_name(slot: &mut Vec<u8>, name: &FileName) {
    let spelled = name.as_str().as_bytes();
    // A name is a path under the driver's root: far under 64 KiB.
    slot.extend_from_slice(&(spelled.len() as u16).to_le_bytes());
    slot.extend_from_slice(spelled);
}

/// The tag of an effect the table does not carry, to refuse it by.
fn tag_of(effect: &Effect) -> Tag {
    match effect {
        Effect::ReadInto { tag, .. }
        | Effect::ReadMany { tag, .. }
        | Effect::WriteFrom { tag, .. }
        | Effect::ReadLink { tag, .. }
        | Effect::WriteBack { tag, .. }
        | Effect::Mode { tag, .. }
        | Effect::Seal { tag, .. }
        | Effect::Private { tag, .. }
        | Effect::Times { tag, .. }
        | Effect::Clone { tag, .. }
        | Effect::CopyData { tag, .. }
        | Effect::Prewarm { tag, .. }
        | Effect::Link { tag, .. }
        | Effect::Stat { tag, .. }
        | Effect::Lstat { tag, .. }
        | Effect::Watch { tag, .. }
        | Effect::Journal { tag, .. }
        | Effect::Unwatch { tag, .. }
        | Effect::Stamp { tag, .. }
        | Effect::Entries { tag, .. }
        | Effect::MapWrite { tag, .. } => *tag,
        // The store asks none of the rest; one asked anyway is answered under no tag it owns,
        // and its run stops on the stray completion (the store's `owns`).
        _ => Tag(OWN),
    }
}

fn kind_of(effect: &Effect) -> &'static str {
    match effect {
        Effect::ReadInto { .. } => "ReadInto",
        Effect::ReadMany { .. } => "ReadMany",
        Effect::WriteFrom { .. } => "WriteFrom",
        Effect::ReadLink { .. } => "ReadLink",
        Effect::WriteBack { .. } => "WriteBack",
        Effect::Mode { .. } => "Mode",
        Effect::Seal { .. } => "Seal",
        Effect::Private { .. } => "Private",
        Effect::Times { .. } => "Times",
        Effect::Clone { .. } => "Clone",
        Effect::CopyData { .. } => "CopyData",
        Effect::Prewarm { .. } => "Prewarm",
        Effect::Link { .. } => "Link",
        Effect::Stat { .. } => "Stat",
        Effect::Lstat { .. } => "Lstat",
        Effect::Watch { .. } => "Watch",
        Effect::Journal { .. } => "Journal",
        Effect::Stamp { .. } => "Stamp",
        Effect::Entries { .. } => "Entries",
        Effect::MapWrite { .. } => "MapWrite",
        _ => "another",
    }
}
