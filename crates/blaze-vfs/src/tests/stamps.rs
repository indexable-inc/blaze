//! The column of stamps and the marks in the VFS journal (`SEEN` and `MARK` records), under the
//! simulation: a restart finds what the watcher saw without reading a file it saw, and no crash
//! leaves a stamp vouching for bytes the recovered head does not hold.
//!
//! The watched disk is a model here, not the simulation's: its clock is coarse (an mtime and a
//! ctime of one second, as HFS+ keeps) or fine (a kernel tick of 10 ms, as ext4's coarse
//! clock at `HZ=100`), so two writes of one length in one tick leave one stamp over other bytes, which the simulation's disk (its clock counts changes) never does.
//! The VFS journal's effects go to the simulation, which tears what was not synced at a crash
//! and fails effects.

use std::collections::{BTreeMap, BTreeSet};

use blaze_core::{
    Completion, Cursor, Done, Driver, Effect, FileName, IoError, Journaled, S_IFDIR,
    S_IFREG, Stamp, Tag,
    sim::{Faults, Rng, Sim},
    wait::Deadline,
};
use blaze_image::{Builder, MIN_GROUP_SHIFT};

use crate::{
    COARSE_NS, FINE_NS, MARK_MOST, MarkTooLarge, Ticket, Vfs, Watcher,
    disk::{changed_ns, racy},
    journal, window_ns,
};

const TAGS: u64 = 1 << 40;
/// A second, and the coarse model file system's clock: what its mtime or ctime can say.
const TICK: u64 = 1_000_000_000;
/// The fine model file system's clock: a kernel tick at `HZ=100`, the slowest a file system
/// with sub-second stamps ticks ([`FINE_NS`] is twice it).
const FINE_TICK: u64 = 10_000_000;

/// Which clock the model file system stamps by.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Clock {
    Coarse,
    Fine,
}

impl Clock {
    fn tick(self) -> u64 {
        match self {
            Clock::Coarse => TICK,
            Clock::Fine => FINE_TICK,
        }
    }

    /// The bound of a short step of the wall clock: about the window, so a look falls on
    /// either side of it.
    fn short(self) -> u64 {
        match self {
            Clock::Coarse => TICK / 2,
            Clock::Fine => 3 * FINE_NS / 2,
        }
    }
}
const DIR: &str = "ws";
/// The files a history writes, deletes and renames; `ws/d/keep` is never touched, so `ws/d`
/// is never empty.
const PATHS: [&str; 4] = ["ws/a", "ws/b", "ws/d/c", "ws/d/e"];

fn name(it: &str) -> FileName {
    FileName::new(it).unwrap()
}

fn journal_file() -> FileName {
    name("state/journal")
}

/// The bug a run plants, for the oracle's controls.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Plant {
    None,
    /// A stamp taken inside the racy window is saved.
    TrustsRacy,
    /// A batch's `SEEN` record is made before its head record.
    SeenBeforeHead,
    /// A start with a saved cursor trusts the journal alone and sweeps nothing.
    SweepsOnlyWithoutCursor,
}

struct File {
    bytes: Vec<u8>,
    ino: u64,
    mtime: u64,
    ctime: u64,
}

/// The watched disk: files, two directories, and every name changed in order (what a journal
/// of changes would tell; its cursor is an index into it).
struct Disk {
    files: BTreeMap<String, File>,
    /// Each directory's inode and the time its entries last changed.
    dirs: BTreeMap<String, (u64, u64)>,
    next_ino: u64,
    changed: Vec<String>,
}

fn parent(name: &str) -> &str {
    name.rsplit_once('/').map_or("", |it| it.0)
}

impl Disk {
    fn new() -> Disk {
        let mut disk = Disk {
            files: BTreeMap::new(),
            dirs: BTreeMap::from([(DIR.to_owned(), (1, 0)), (format!("{DIR}/d"), (2, 0))]),
            next_ino: 10,
            changed: Vec::new(),
        };
        // Every file there long before the first start: its stamp is steady at once.
        for (at, path) in PATHS.iter().chain(&["ws/d/keep"]).enumerate() {
            let bytes = format!("i{at:03}").into_bytes();
            let file = File { bytes, ino: 3 + at as u64, mtime: 0, ctime: 0 };
            disk.files.insert((*path).to_owned(), file);
        }
        disk
    }

    fn stamp(&self, name: &str) -> Stamp {
        if let Some(file) = self.files.get(name) {
            return Stamp {
                ino: file.ino,
                size: file.bytes.len() as u64,
                mtime_ns: file.mtime,
                ctime_ns: file.ctime,
                mode: S_IFREG | 0o644,
            };
        }
        match self.dirs.get(name) {
            Some(&(ino, mtime)) => Stamp {
                ino,
                size: self.children(name).len() as u64,
                mtime_ns: mtime,
                ctime_ns: mtime,
                mode: S_IFDIR | 0o755,
            },
            None => Stamp::GONE,
        }
    }

    fn children(&self, dir: &str) -> Vec<String> {
        let files = self.files.keys().filter(|it| parent(it) == dir);
        let dirs = self.dirs.keys().filter(|it| parent(it) == dir);
        let mut all: Vec<String> = files.chain(dirs).cloned().collect();
        all.sort_unstable();
        all
    }

    fn entries_changed(&mut self, name: &str, now: u64) {
        self.dirs.get_mut(parent(name)).expect("a file of a directory there").1 = now;
    }

    fn write(&mut self, name: &str, bytes: Vec<u8>, now: u64) {
        match self.files.get_mut(name) {
            Some(file) => (file.bytes, file.mtime, file.ctime) = (bytes, now, now),
            None => {
                let ino = self.next_ino;
                self.next_ino += 1;
                self.files.insert(name.to_owned(), File { bytes, ino, mtime: now, ctime: now });
                self.entries_changed(name, now);
            }
        }
        self.changed.push(name.to_owned());
    }

    fn delete(&mut self, name: &str, now: u64) {
        if self.files.remove(name).is_some() {
            self.entries_changed(name, now);
            self.changed.push(name.to_owned());
        }
    }

    /// A rename keeps the file's inode, length and mtime; its ctime moves.
    fn rename(&mut self, from: &str, to: &str, now: u64) {
        if from == to {
            return;
        }
        let Some(mut file) = self.files.remove(from) else { return };
        file.ctime = now;
        self.files.insert(to.to_owned(), file);
        self.entries_changed(from, now);
        self.entries_changed(to, now);
        self.changed.push(from.to_owned());
        self.changed.push(to.to_owned());
    }
}

/// What the watcher asked of the disk and is not answered yet.
enum Ask {
    Look(Effect),
    /// The journal of changes tells what changed since it last told.
    Tell,
}

/// A daemon's VFS and watcher on the model disk, its journal on the simulation, an image, the
/// marks it set, and every ticket made durable since its last start.
struct World {
    plant: Plant,
    clock: Clock,
    rng: Rng,
    wall: u64,
    sim: Sim,
    vfs: Vfs,
    watcher: Watcher,
    disk: Disk,
    effects: Vec<Effect>,
    asked: Vec<Ask>,
    journal: Option<Tag>,
    told: usize,
    /// The journal of changes keeps no cursor (Linux): every start sweeps the directory.
    sweeps: bool,
    reads: usize,
    /// Each mark's values set in order, with the ticket of each (`None`: durable before the
    /// last start).
    marks: BTreeMap<String, Vec<(Option<Vec<u8>>, Option<Ticket>)>>,
    durable: BTreeSet<Ticket>,
    /// The image file as its last rename left it, and the column it holds.
    image: Option<Vec<u8>>,
}

fn watcher(plant: Plant) -> Watcher {
    let mut watcher = Watcher::new("disk", name(DIR), TAGS).expect("its looks in flight reserved");
    watcher.trusts_racy = plant == Plant::TrustsRacy;
    watcher.seen_before_head = plant == Plant::SeenBeforeHead;
    watcher.sweeps_only_without_cursor = plant == Plant::SweepsOnlyWithoutCursor;
    watcher
}

impl World {
    fn new(seed: u64, plant: Plant, clock: Clock, faults: Faults, sweeps: bool) -> World {
        let mut world = World {
            plant,
            clock,
            rng: Rng::new(seed ^ 0x5ee7),
            wall: 100 * TICK,
            sim: Sim::new(seed, faults),
            vfs: Vfs::recover(journal_file(), 0, &[]).0,
            watcher: watcher(plant),
            disk: Disk::new(),
            effects: Vec::new(),
            asked: Vec::new(),
            journal: None,
            told: 0,
            sweeps,
            reads: 0,
            marks: BTreeMap::new(),
            durable: BTreeSet::new(),
            image: None,
        };
        world.watcher.start(&world.vfs, world.wall, &mut world.effects);
        world.route();
        world
    }

    /// The model file system's time now.
    fn now(&self) -> u64 {
        self.wall / self.clock.tick() * self.clock.tick()
    }

    fn cursor(&self) -> Cursor {
        match self.sweeps {
            true => Cursor::None,
            false => Cursor::Sim { journal: 7, next: self.disk.changed.len() as u64 },
        }
    }

    /// The VFS's effects to the simulation, the watcher's to be answered.
    fn route(&mut self) {
        let mut journal = Vec::new();
        for effect in std::mem::take(&mut self.effects) {
            match effect {
                Effect::Append { .. }
                | Effect::Sync { .. }
                | Effect::Write { .. }
                | Effect::Truncate { .. } => journal.push(effect),
                Effect::Journal { since, tag, .. } => {
                    self.journal = Some(tag);
                    self.told = match since {
                        Cursor::Sim { next, .. } => next as usize,
                        _ => self.disk.changed.len(),
                    };
                    self.asked.push(Ask::Tell);
                }
                other => self.asked.push(Ask::Look(other)),
            }
        }
        self.sim.submit(&mut journal);
    }

    /// Answers what the watcher asked at `at`, from the disk as it is now.
    fn answer(&mut self, at: usize) {
        let (tag, done) = match self.asked.remove(at) {
            Ask::Tell => {
                let Some(tag) = self.journal else { return };
                let mut files: Vec<FileName> =
                    self.disk.changed[self.told..].iter().map(|it| name(it)).collect();
                files.sort_unstable();
                files.dedup();
                self.told = self.disk.changed.len();
                let cursor = self.cursor();
                let told = Journaled { files, lost: Vec::new(), cursor, caught_up: true };
                (tag, Done::Journaled(Box::new(told)))
            }
            Ask::Look(Effect::Stamp { files, tag }) => {
                (tag, Done::Stamped(files.iter().map(|it| self.disk.stamp(it)).collect()))
            }
            Ask::Look(Effect::ReadAll { file, tag }) => {
                self.reads += 1;
                let bytes = self.disk.files.get(file.as_str()).map(|it| it.bytes.clone());
                (tag, Done::Read(bytes.unwrap_or_default()))
            }
            Ask::Look(Effect::Entries { dir, tag }) => match self.disk.dirs.contains_key(dir.as_str()) {
                true => {
                    let children = self.disk.children(dir.as_str());
                    let entries = children.iter().map(|it| (name(it), self.disk.stamp(it)));
                    (tag, Done::Entries(entries.collect()))
                }
                false => (tag, Done::Failed(IoError(format!("{dir}: not a directory")))),
            },
            Ask::Look(effect) => panic!("the watcher asked {effect:?} of the model disk"),
        };
        let completion = Completion { tag, done };
        self.watcher.complete(&mut self.vfs, completion, self.wall, &mut self.effects);
        self.route();
    }

    /// One completion of the journal's effects, if any is in flight.
    fn poll(&mut self) {
        if self.sim.in_flight() == 0 {
            return;
        }
        let mut done = Vec::new();
        self.sim.poll(&mut done, Deadline::NEVER);
        for completion in done {
            assert!(self.vfs.owns(completion.tag), "{completion:?}");
            let durable = self.vfs.complete(completion, &mut self.effects);
            self.durable.extend(durable);
        }
        self.route();
    }

    /// Everything asked answered and every effect done.
    fn settle(&mut self) {
        for _ in 0..100_000 {
            if self.asked.is_empty() && self.sim.in_flight() == 0 && self.effects.is_empty() {
                self.vfs.flush(&mut self.effects);
                if self.effects.is_empty() {
                    return;
                }
                self.route();
            }
            if !self.asked.is_empty() {
                self.answer(0);
            }
            self.poll();
        }
        panic!("the world did not settle in 100000 turns");
    }

    fn tell(&mut self) {
        if self.journal.is_some() {
            self.asked.push(Ask::Tell);
        }
    }

    fn value(&mut self) -> Vec<u8> {
        // One length for every value: two writes in one tick leave one stamp.
        format!("v{:03}", self.rng.below(1000)).into_bytes()
    }

    fn mark(&mut self) {
        let mark = format!("m{}", self.rng.below(3));
        let ticket = match self.rng.one_in(4) {
            true => self.vfs.unmark(&mark).map(|it| (None, it)),
            false => {
                let len = 1 + self.rng.below(8) as usize;
                let bytes: Vec<u8> = (0..len).map(|_| self.rng.below(256) as u8).collect();
                let ticket = self.vfs.mark(&mark, &bytes).expect("under the bound");
                Some((Some(bytes), ticket))
            }
        };
        if let Some((value, ticket)) = ticket {
            self.marks.entry(mark).or_default().push((value, Some(ticket)));
        }
        self.vfs.flush(&mut self.effects);
        self.route();
    }

    /// An image of the VFS written, its file renamed into place and made durable.
    fn image(&mut self) {
        let mut builder = Builder::with_group_shift(MIN_GROUP_SHIFT);
        let texts = crate::image::tests::texts();
        let Ok(mark) = self.vfs.write_image(&mut builder, &*texts) else { return };
        self.image = Some(builder.finish().expect("an image under the limit"));
        assert!(self.vfs.image_durable(mark.name));
        self.vfs.flush(&mut self.effects);
        self.route();
    }

    /// The daemon stops (at a crash, what was not synced is torn) and starts again from the
    /// journal's file and the image, a new watcher from the column it finds.
    fn restart(&mut self, crash: bool) -> Result<(), String> {
        if crash {
            self.sim.crash();
        } else {
            self.settle();
        }
        (self.asked.clear(), self.effects.clear());
        self.journal = None;
        let bytes = self.sim.contents(&journal_file());
        let image = self.image.as_deref().map(crate::image::tests::mapping);
        self.vfs = Vfs::restore(journal_file(), 0, &bytes, image).0;
        self.check_marks()?;
        // Tickets are numbered again from the start: each mark's history begins at what it is.
        self.durable.clear();
        let marks: Vec<String> = self.marks.keys().cloned().collect();
        for mark in marks {
            let now = self.vfs.marked(&mark).map(<[u8]>::to_vec);
            self.marks.insert(mark, vec![(now, None)]);
        }
        self.check()?;
        self.watcher = watcher(self.plant);
        self.watcher.start(&self.vfs, self.wall, &mut self.effects);
        self.route();
        Ok(())
    }

    /// The invariant of the column: a stamp other than [`Stamp::UNKNOWN`] that is the stamp
    /// the disk gives now vouches for the head: the head holds there exactly the disk's bytes.
    /// It is what a start trusts without a read.
    fn check(&self) -> Result<(), String> {
        let Some(column) = self.vfs.disks.get("disk") else { return Ok(()) };
        let root = self.vfs.head("disk").and_then(|it| self.vfs.root(it));
        let mut bad = None;
        column.under(DIR, |path, stamp| {
            if bad.is_some() || stamp.is_unknown() || stamp.is_dir() {
                return;
            }
            let Some(file) = self.disk.files.get(path) else { return };
            if self.disk.stamp(path) != stamp {
                return;
            }
            let rel = &path[DIR.len() + 1..];
            let held = root
                .and_then(|root| self.vfs.tree.entry(root, rel))
                .and_then(|(node, _)| self.vfs.tree.bytes(node).ok().flatten().map(<[u8]>::to_vec));
            if held.as_deref() != Some(&file.bytes[..]) {
                bad = Some(format!(
                    "{path}: the column's stamp {stamp:?} is the disk's, which holds {:?}, and the \
                     head holds {:?}",
                    String::from_utf8_lossy(&file.bytes),
                    held.map(|it| String::from_utf8_lossy(&it).into_owned())
                ));
            }
        });
        bad.map_or(Ok(()), Err)
    }

    /// The invariant of the marks: each recovered mark is a value that was set, the last
    /// whose ticket was durable or one set after it.
    fn check_marks(&self) -> Result<(), String> {
        for (mark, values) in &self.marks {
            let durable = values
                .iter()
                .rposition(|(_, ticket)| ticket.is_none_or(|it| self.durable.contains(&it)));
            let from = durable.unwrap_or(0);
            let mut allowed: Vec<Option<&[u8]>> =
                values[from..].iter().map(|(value, _)| value.as_deref()).collect();
            if durable.is_none() {
                allowed.push(None);
            }
            let got = self.vfs.marked(mark);
            if !allowed.contains(&got) {
                return Err(format!("the mark {mark} is {got:?}, none of {allowed:?}"));
            }
        }
        Ok(())
    }

    /// One step of a history: the disk changed, a mark set, an image written, a crash, or a
    /// name gone steady looked at; then some of what is asked answered, in a drawn order.
    fn step(&mut self) -> Result<(), String> {
        self.wall += match self.rng.one_in(4) {
            true => 3 * TICK,
            false => self.rng.below(self.clock.short()),
        };
        let path = PATHS[self.rng.below(PATHS.len() as u64) as usize];
        match self.rng.below(12) {
            0..=2 => {
                let bytes = self.value();
                self.disk.write(path, bytes, self.now());
                self.tell();
            }
            3 => {
                self.disk.delete(path, self.now());
                self.tell();
            }
            4 => {
                let to = PATHS[self.rng.below(PATHS.len() as u64) as usize];
                self.disk.rename(path, to, self.now());
                self.tell();
            }
            5 => self.mark(),
            6 => self.image(),
            7 => {
                if self.rng.one_in(2) {
                    self.restart(true)?;
                }
            }
            8 => {
                self.watcher.steady(&self.vfs, self.wall, &mut self.effects);
                self.route();
            }
            9 => {
                // A write looked at in full, steady or not, maybe written again in the same
                // tick, and maybe a crash while its records are in flight.
                let bytes = self.value();
                self.disk.write(path, bytes, self.now());
                self.tell();
                if self.rng.one_in(2) {
                    self.wall += 3 * TICK;
                }
                while !self.asked.is_empty() {
                    self.answer(0);
                }
                if self.rng.one_in(2) {
                    let bytes = self.value();
                    self.disk.write(path, bytes, self.now());
                    self.tell();
                    while !self.asked.is_empty() {
                        self.answer(0);
                    }
                }
                self.check()?;
                if self.rng.one_in(2) {
                    self.restart(true)?;
                }
            }
            _ => {}
        }
        for _ in 0..self.rng.below(5) {
            if !self.asked.is_empty() {
                let at = self.rng.below(self.asked.len() as u64) as usize;
                self.answer(at);
            }
        }
        for _ in 0..self.rng.below(4) {
            self.poll();
        }
        self.check()
    }
}

/// A history of `seed`: steps, crashes and a last stop; the first break of an invariant.
fn stamps_run(seed: u64, plant: Plant, clock: Clock) -> Result<(), String> {
    let faults = Faults { fail_one_in: 9, ..Faults::default() };
    let mut world = World::new(seed, plant, clock, faults, seed % 3 == 0);
    for step in 0..120 {
        world.step().map_err(|it| format!("seed {seed} step {step}: {it}"))?;
    }
    world.restart(true).map_err(|it| format!("seed {seed} at the last crash: {it}"))?;
    world.settle();
    world.check().map_err(|it| format!("seed {seed} settled after: {it}"))
}

/// The invariant: after any crash (unsynced appends torn, syncs failed), and at every step, a
/// column stamp other than UNKNOWN that is the disk's stamp now was taken while the disk held
/// the bytes the (recovered) head has there; a mark's bytes are a value that was set, the last
/// whose ticket completed or a later one.
/// On both clocks: the fine one's window is [`FINE_NS`], not the coarse one's.
#[test]
fn stamps_survive_a_crash_with_the_bytes_they_vouch_for() {
    for clock in [Clock::Coarse, Clock::Fine] {
        for seed in 0..256 {
            if let Err(error) = stamps_run(seed, Plant::None, clock) {
                panic!("{clock:?}: {error}");
            }
        }
    }
}

/// The control: with a stamp taken inside the racy window saved, a second write of one length
/// in the same tick leaves the column vouching for bytes the head does not hold; on the fine
/// clock too, where the window is [`FINE_NS`].
#[test]
fn the_oracle_catches_a_stamp_trusted_inside_the_racy_window() {
    for clock in [Clock::Coarse, Clock::Fine] {
        let caught = (0..256).find_map(|seed| stamps_run(seed, Plant::TrustsRacy, clock).err());
        assert!(caught.is_some(), "{clock:?}: no seed broke the invariant with racy stamps trusted");
    }
}

/// The control: with a batch's `SEEN` record before its head record, a crash that keeps the
/// one and tears the other leaves the column ahead of the head.
#[test]
fn the_oracle_catches_a_stamp_recorded_before_its_head() {
    let caught =
        (0..256).find_map(|seed| stamps_run(seed, Plant::SeenBeforeHead, Clock::Coarse).err());
    assert!(caught.is_some(), "no seed broke the invariant with SEEN before its head");
}

/// A world whose files are all looked at, steady and saved, and its journal quiet.
fn steady_world(seed: u64, sweeps: bool) -> World {
    let mut world = World::new(seed, Plant::None, Clock::Coarse, Faults::default(), sweeps);
    world.settle();
    // Written in place, racy when first looked at: no directory's entries change.
    for (at, path) in PATHS.iter().enumerate() {
        let bytes = format!("f{at:03}").into_bytes();
        world.disk.write(path, bytes, world.now());
    }
    world.tell();
    world.settle();
    world.wall += 3 * TICK;
    world.watcher.steady(&world.vfs, world.wall, &mut world.effects);
    world.route();
    world.settle();
    world
}

fn column(vfs: &Vfs) -> Vec<u8> {
    let mut bytes = Vec::new();
    vfs.disks.encode(&mut bytes);
    bytes
}

/// The invariant: a start that sweeps the directory (no cursor kept) over the column the
/// journal holds reads no file whose stamp is the one saved, and ends with the column of a
/// twin that never stopped, byte for byte; with an image, and with the journal alone.
#[test]
fn a_restart_sweep_reads_no_unchanged_file() {
    for imaged in [false, true] {
        let mut world = steady_world(3, true);
        assert!(world.watcher.idle());
        // Each file at the first sweep, each written once racy and once steady.
        assert_eq!(world.reads, 3 * PATHS.len() + 1, "{imaged}");
        if imaged {
            world.image();
            assert!(world.image.is_some(), "a quiet journal takes an image");
            world.settle();
        }
        let twin = column(&world.vfs);
        world.reads = 0;
        world.restart(false).unwrap();
        world.settle();
        assert_eq!(world.reads, 0, "a file whose stamp is the saved one is read: {imaged}");
        assert_eq!(column(&world.vfs), twin, "the column as the twin's: {imaged}");
        world.check().unwrap();
    }
}

/// The invariant: a start with a saved cursor sweeps the column too: with nothing changed it
/// reads no file, and a file changed while no daemon watched is read once even when the
/// journal's history never names it (the system dropped it). Control: a start that trusts the
/// journal alone with a cursor never reads it.
#[test]
fn a_start_with_a_cursor_sweeps_the_column_and_reads_no_unchanged_file() {
    for plant in [Plant::None, Plant::SweepsOnlyWithoutCursor] {
        let mut world = steady_world(6, false);
        assert!(world.watcher.idle());
        world.reads = 0;
        world.plant = plant;
        world.restart(false).unwrap();
        let stamped: BTreeSet<String> = world
            .asked
            .iter()
            .filter_map(|it| match it {
                Ask::Look(Effect::Stamp { files, .. }) => Some(files.iter()),
                _ => None,
            })
            .flatten()
            .map(str::to_owned)
            .collect();
        world.settle();
        assert_eq!(world.reads, 0, "a file whose stamp is the saved one is read: {plant:?}");
        match plant {
            Plant::SweepsOnlyWithoutCursor => assert!(stamped.is_empty(), "{stamped:?}"),
            _ => {
                let missing: Vec<&str> =
                    PATHS.iter().copied().filter(|it| !stamped.contains(*it)).collect();
                assert!(missing.is_empty(), "the start's sweep stamps every file: {missing:?}");
            }
        }
        // Down again; one file written in place, its change lost from the journal's history.
        world.wall += 3 * TICK;
        world.disk.write(PATHS[1], b"g001".to_vec(), world.now());
        assert_eq!(world.disk.changed.pop().as_deref(), Some(PATHS[1]));
        world.restart(false).unwrap();
        world.settle();
        match plant {
            Plant::SweepsOnlyWithoutCursor => {
                assert_eq!(world.reads, 0, "the control trusts the journal and misses it")
            }
            _ => assert_eq!(world.reads, 1, "the file changed while down is read once"),
        }
        world.check().unwrap();
    }
}

/// The invariant: a journal of the layout before `SEEN` and `MARK` (kinds 1 to 6 only) loads
/// over its image with the image's column, no stamp of its own and no mark.
#[test]
fn a_journal_of_before_stamps_loads_with_the_images_column_and_no_marks() {
    let mut world = steady_world(4, false);
    world.mark();
    world.settle();
    world.image();
    assert!(world.image.is_some(), "a quiet journal takes an image");
    world.settle();
    let imaged = column(&world.vfs);
    let bytes = world.value();
    world.disk.write(PATHS[0], bytes, world.now());
    world.tell();
    world.settle();
    let journal = world.sim.contents(&journal_file());
    let (seen, mark) = journal::STAMP_KINDS;
    let mut before = Vec::new();
    let mut at = 0;
    while at + 9 <= journal.len() {
        let len = u32::from_le_bytes(journal[at + 4..at + 8].try_into().unwrap()) as usize;
        let end = at + 9 + len + 8;
        if ![seen, mark].contains(&journal[at + 8]) {
            before.extend_from_slice(&journal[at..end]);
        }
        at = end;
    }
    assert!(before.len() < journal.len(), "the journal had stamps to strip");
    let image = world.image.as_deref().map(crate::image::tests::mapping);
    let (vfs, found) = Vfs::restore(journal_file(), 0, &before, image);
    assert_eq!((found.seen, found.marks, found.skipped), (0, 0, 0), "{found:?}");
    assert_eq!(vfs.marks("").count(), 0);
    assert_eq!(column(&vfs), imaged, "the image's column, as it was written");
    assert_eq!(vfs.head("disk"), world.vfs.head("disk"), "the head the journal moved to");
    world.vfs = vfs;
    world.check().unwrap();
}

/// The invariant: a file looked at within its window ([`COARSE_NS`] here) of its change is saved as UNKNOWN, so
/// its stamp vouches for nothing; once steady it is read again and its stamp saved, and then
/// trusted (told again, it is not read). Control: with racy stamps trusted, its stamp is saved
/// at once.
#[test]
fn a_racy_file_is_read_again_once_steady_and_then_trusted() {
    for plant in [Plant::None, Plant::TrustsRacy] {
        let mut world = World::new(5, plant, Clock::Coarse, Faults::default(), false);
        world.settle();
        let swept = world.reads;
        world.wall += TICK / 10;
        world.disk.write("ws/a", b"racy".to_vec(), world.now());
        world.tell();
        world.settle();
        assert_eq!(world.reads, swept + 1);
        let saved = world.vfs.disks.get("disk").and_then(|it| it.get("ws/a"));
        let stamp = world.disk.stamp("ws/a");
        match plant {
            Plant::TrustsRacy => {
                assert_eq!(saved, Some(stamp), "the control saves the racy stamp");
                continue;
            }
            _ => assert_eq!(saved, Some(Stamp::UNKNOWN), "a racy stamp is not saved"),
        }
        world.watcher.steady(&world.vfs, world.wall, &mut world.effects);
        assert!(world.effects.is_empty(), "nothing is steady yet: {:?}", world.effects);
        world.wall += COARSE_NS + TICK;
        world.watcher.steady(&world.vfs, world.wall, &mut world.effects);
        world.route();
        world.settle();
        assert_eq!(world.reads, swept + 2, "read again once steady");
        let saved = world.vfs.disks.get("disk").and_then(|it| it.get("ws/a"));
        assert_eq!(saved, Some(stamp), "then its stamp is saved");
        world.watcher.steady(&world.vfs, world.wall, &mut world.effects);
        assert!(world.effects.is_empty(), "nothing left racy: {:?}", world.effects);
        world.disk.changed.push("ws/a".into());
        world.tell();
        world.settle();
        assert_eq!(world.reads, swept + 2, "told again, its saved stamp is trusted");
        world.check().unwrap();
    }
}

/// Each verdict the window owes, as `(what, stamp, looked_ns, racy)`: a stamp with a
/// sub-second part looked at 25 ms after its change vouches and 15 ms after does not; a
/// whole-second one looked at 1.5 s after does not.
fn window_cases() -> [(&'static str, Stamp, u64, bool); 3] {
    const MS: u64 = 1_000_000;
    let at = 100 * TICK;
    let mode = S_IFREG | 0o644;
    let fine = Stamp { ino: 3, size: 4, mtime_ns: at + 7 * MS, ctime_ns: at + 7 * MS, mode };
    let whole = Stamp { mtime_ns: at, ctime_ns: at, ..fine };
    [
        ("a fine stamp 25 ms after", fine, changed_ns(&fine) + 25 * MS, false),
        ("a fine stamp 15 ms after", fine, changed_ns(&fine) + 15 * MS, true),
        ("a whole-second stamp 1.5 s after", whole, changed_ns(&whole) + 1_500 * MS, true),
    ]
}

/// The cases `racy` answers otherwise than owed.
fn window_wrong(racy: impl Fn(&Stamp, u64) -> bool) -> Vec<&'static str> {
    let cases = window_cases().into_iter();
    cases.filter(|(_, stamp, looked, owed)| racy(stamp, *looked) != *owed).map(|it| it.0).collect()
}

/// The invariant: the racy window is the stamp's file system's clock, [`FINE_NS`] for a
/// stamp with a sub-second part and [`COARSE_NS`] for a whole-second one, so a fine stamp
/// vouches 25 ms after its change and not 15 ms after, and a whole-second one not 1.5 s after.
/// Control: one window for both stamps, of any length, either trusts the whole-second stamp
/// at 1.5 s or refuses the fine one at 25 ms (or trusts it at 15 ms).
#[test]
fn the_racy_window_is_the_file_systems_clock() {
    assert_eq!(window_wrong(racy), Vec::<&str>::new());
    let (fine, whole) = (window_cases()[0].1, window_cases()[2].1);
    assert_eq!((window_ns(&fine), window_ns(&whole)), (FINE_NS, COARSE_NS));
    // A stamp whose ctime alone is fine is of a fine clock.
    assert_eq!(window_ns(&Stamp { ctime_ns: fine.ctime_ns, ..whole }), FINE_NS);
    const MS: u64 = 1_000_000;
    for window in [MS, 10 * MS, 16 * MS, FINE_NS, 25 * MS, 100 * MS, TICK, 1_600 * MS, COARSE_NS] {
        let one = |stamp: &Stamp, looked: u64| changed_ns(stamp).saturating_add(window) > looked;
        assert!(!window_wrong(one).is_empty(), "one window of {window} ns for both stamps");
    }
}

fn settle_vfs(sim: &mut Sim, vfs: &mut Vfs) {
    let mut effects = Vec::new();
    vfs.flush(&mut effects);
    let mut done = Vec::new();
    for _ in 0..10_000 {
        if effects.is_empty() && sim.in_flight() == 0 {
            return;
        }
        sim.submit(&mut effects);
        sim.poll(&mut done, Deadline::NEVER);
        for completion in done.drain(..) {
            vfs.complete(completion, &mut effects);
        }
    }
    panic!("the journal did not settle");
}

/// The invariant: a mark is the last value set through a rewrite of the journal, which holds
/// one record per live mark; a mark removed stays removed.
#[test]
fn marks_keep_the_last_value_through_a_rewrite() {
    let mut sim = Sim::new(6, Faults::default());
    let mut vfs = Vfs::recover(journal_file(), 0, &[]).0;
    vfs.mark("tool", b"one").unwrap();
    vfs.mark("tool", b"two").unwrap();
    vfs.mark("placed\0/out/a", b"a").unwrap();
    vfs.mark("carry\0/ws", b"c").unwrap();
    assert!(vfs.unmark("carry\0/ws").is_some());
    assert!(vfs.unmark("never set").is_none(), "nothing to remove records nothing");
    settle_vfs(&mut sim, &mut vfs);
    let (_, found) = Vfs::recover(journal_file(), 0, &sim.contents(&journal_file()));
    assert_eq!(found.marks, 5, "each mark set appended");
    vfs.collect(&[]);
    settle_vfs(&mut sim, &mut vfs);
    let (back, found) = Vfs::recover(journal_file(), 0, &sim.contents(&journal_file()));
    assert_eq!(found.marks, 2, "the rewrite holds the live marks alone");
    assert_eq!(back.marked("tool"), Some(&b"two"[..]));
    assert_eq!(back.marked("carry\0/ws"), None, "removed stays removed");
    let placed: Vec<(&str, &[u8])> = back.marks("placed\0").collect();
    assert_eq!(placed, vec![("placed\0/out/a", &b"a"[..])]);
    // Control: the planted reading of every record, not the last, would give `one`.
    assert_ne!(back.marked("tool"), Some(&b"one"[..]));
}

/// The invariant: a mark over [`MARK_MOST`] bytes is refused naming the mark and its length,
/// and nothing is recorded; one of exactly the bound is kept.
#[test]
fn a_mark_over_the_bound_is_refused_by_name() {
    let mut vfs = Vfs::recover(journal_file(), 0, &[]).0;
    let over = vec![7; MARK_MOST + 1];
    let refused = vfs.mark("placed\0/out/big", &over).unwrap_err();
    assert_eq!(refused, MarkTooLarge { name: "placed\0/out/big".into(), len: MARK_MOST + 1 });
    assert!(refused.to_string().contains("placed\0/out/big"), "{refused}");
    assert_eq!(vfs.marked("placed\0/out/big"), None);
    let mut effects = Vec::new();
    vfs.flush(&mut effects);
    assert!(effects.is_empty(), "a refused mark records nothing: {effects:?}");
    vfs.mark("placed\0/out/big", &over[..MARK_MOST]).unwrap();
    assert_eq!(vfs.marked("placed\0/out/big").map(<[u8]>::len), Some(MARK_MOST));
}
