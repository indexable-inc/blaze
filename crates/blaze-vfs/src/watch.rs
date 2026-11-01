//! The disk as one input: a [`Watcher`] follows a directory through the system's journal of
//! changes ([`Effect::Journal`]) and turns what changed into transactions on one head, by the
//! disk (docs/v2/plan/design/restore.md, docs/v2/plan/lanes/frontend/journal.md).
//!
//! - **Start**: the journal is asked for what changed since the cursor the head's [`Disk`] saved
//!   (FSEvents' event ID, NTFS's USN). With no cursor (a first start, or Linux, whose journal
//!   does not outlive a restart) the whole directory is looked at again ([`Watcher::rescan`]),
//!   which with a saved column is a sweep of stamps, not a read of every file.
//! - **Every look is stamps first**: the paths of a batch are stamped together in one
//!   [`Effect::Stamp`] (io_uring's batched `statx` on Linux); a path whose stamp is the one the
//!   column saved, and which the head has, is unchanged and never read or hashed. The rest are
//!   read ([`Effect::ReadAll`]), after their stamp was taken, so a change during the read moves
//!   the stamp and is seen again.
//! - **A rescan of a subtree** (a cold start, a Linux start, or a [`Lost`] the journal names)
//!   stamps every path the column and the head have under it, lists ([`Effect::Entries`]) only
//!   the directories whose stamp moved, and every directory new to it, and reads only files
//!   that are new or moved. Nothing outside the subtree is looked at. [`Lost::Unnamed`] (names
//!   in one directory changed, not which) lists that directory alone and diffs it.
//! - **A move is not a read**: a name new to a batch whose stamp is that of a file the batch
//!   found gone (the same inode, size, mtime and type) is that file moved, and the head's own
//!   node goes there ([`Op::Rename`] where the source goes in the same commit, else its bytes
//!   from the tree). A directory renamed while blaze was down reads none of its files.
//! - A batch commits once every look in it is in, batches in the order they were told, so a
//!   later read of a file wins; its stamps and the journal's cursor after it are saved in the
//!   head's [`Disk`] then, so a cursor is never past a change not committed.
//! - The disk is the truth for the paths it reports, but not over a transaction made since: a
//!   path whose head changed after the report (by anyone but this watcher) is not taken and is
//!   named ([`Watched::Conflict`]). A file whose bytes are those the head has already makes no op.
//! - Modes: a file the head has keeps its mode, a new one is [`Mode::Regular`] (the stamp's
//!   mode bits are saved for when the driver reports modes, v2/requests/vfs.md item 6).
//! - **Names are numbers inside**: each name is interned once ([`Paths`]) as it comes in, and
//!   slots, looks in flight, the commit's lists and what its commits left are [`PathId`]s. Text
//!   is made only where a name leaves: an effect's [`FileName`], an [`Op`]'s path, a
//!   [`Watched`]'s. Batches, the stamp buffer and every list are reused, so a warm batch of
//!   unchanged paths allocates nothing.

use std::{cmp::Ordering, collections::VecDeque, sync::Arc};

use blaze_core::{
    Completion, Cursor, Done, Effect, FileName, IoError, Lost, Names, S_IFMT, Stamp, Tag,
};
use blaze_hash::{HashMap, HashSet};
use blaze_ids::ContentId;
use blaze_mem::Slab;

use crate::{
    Commit, Mode, Op, Vfs,
    disk::Disk,
    paths::{PathId, Paths},
    tree::{NodeId, Tree, ancestors},
};

/// What the head has at a path: its node's content ID and its mode.
type Seen = Option<(ContentId, Mode)>;

fn seen(tree: &Tree, root: NodeId, path: &str) -> Seen {
    tree.entry(root, path).map(|(node, mode)| (tree.id(node), mode))
}

/// The read of a file stamped `stamp`. A symbolic link is an entry whose bytes are its target,
/// as git keeps one: its target is read, never followed, whatever it points at.
pub(crate) fn read(file: FileName, stamp: Stamp, tag: Tag) -> Effect {
    match stamp.is_link() {
        true => Effect::ReadLink { file, tag },
        false => Effect::ReadAll { file, tag },
    }
}

/// The mode a file read with [`read`] is committed with, the head having `had` there: a
/// symbolic link's; else what the head had (an executable stays one), a link that became a
/// file a regular file.
pub(crate) fn file_mode(stamp: Stamp, had: Option<Mode>) -> Mode {
    match had {
        _ if stamp.is_link() => Mode::Symlink,
        Some(Mode::Symlink) | None => Mode::Regular,
        Some(mode) => mode,
    }
}

/// How many tags a watcher uses: its journal, then one per stamp, listing or read in flight.
const TAGS: u64 = 1 << 32;
const FIRST: u64 = 1;
/// The most looks one watch has in flight at once (a rescan's reads): address space only,
/// 4 Mi slots, committed as they are used.
const MOST_IN_FLIGHT: u32 = 1 << 22;
/// Committed batches kept for reuse, and the most slots one keeps room for: a rescan of a
/// checkout's 100 000 paths gives its room back rather than holding 17 MB.
const SPARE_BATCHES: usize = 4;
const SPARE_SLOTS: usize = 1 << 12;

/// What a watcher's completion did.
#[derive(PartialEq, Eq, Debug)]
pub enum Watched {
    /// What the disk changed, committed to the head.
    Committed(Commit),
    /// The head's `path` changed since the disk's change was reported: the head keeps its own.
    Conflict { path: String },
    /// `path` changed and could not be read: the head keeps what it had. `""` is the watched
    /// directory itself, which ends the watch.
    Unreadable { path: String, error: IoError },
    /// The journal could not name the changes under `path` (`""` for all of it), for `why`: that
    /// subtree is looked at again, and only it ([`Lost::Unnamed`]: only the directory's own
    /// names); the look ends with [`Watched::Scanned`].
    Lost { path: String, why: Lost },
    /// The head holds what the directory did: a look at a subtree, or everything the journal
    /// told up to where it caught up, is committed.
    Scanned,
    /// The watch ended: stopped, or failed with the error.
    Ended(Option<IoError>),
}

/// What the disk had at a path.
enum Look {
    Gone,
    Dir,
    File(Vec<u8>),
    /// A file new to its path, with the stamp of the batch's gone slot at this index: that
    /// file moved here. Not read.
    Moved(usize),
    /// Its stamp is the one saved and the head has it: not read.
    Same,
    Unreadable,
}

struct Slot {
    /// Under the driver's root; its path in the head is what follows the watched directory's
    /// `/` (`""` for the directory itself).
    name: PathId,
    /// What the head had at the path when the change was reported.
    was: Seen,
    /// What the column saved of it, and what it is now, taken before any read.
    saved: Option<Stamp>,
    stamp: Option<Stamp>,
    look: Option<Look>,
}

struct Batch {
    /// The names stamped in one effect sharing this buffer. In a rescan's sweep its first
    /// `swept` are the column's names under its subtree, sorted, with what it saved of each in
    /// `saved`; the rest are its slots' names. They are compared in one pass, and only a swept
    /// name whose stamp moved (or that `forced` names) becomes a slot: an unchanged one costs no
    /// look in the tree. `None` for a relisting.
    names: Option<Arc<Names>>,
    swept: usize,
    saved: Vec<Stamp>,
    /// Swept names the head changed since the column held all of it, ascending.
    forced: Vec<u32>,
    slots: Vec<Slot>,
    /// Each slot by name, once a listing may add names to it (the swept names are found in
    /// `names`).
    index: HashMap<PathId, usize>,
    /// Stamps and listings in flight: it commits once none is and every slot is looked at.
    open: u32,
    /// The directory a rescan looks at the subtree of.
    rescan: Option<PathId>,
    /// The directory a [`Lost::Unnamed`] lists alone: its slots are what was in it.
    relist: Option<PathId>,
    /// Each gone file the head had, by its inode: a name new to the batch with its stamp is it.
    gone: HashMap<u64, usize>,
    /// The journal's place once this is committed, and whether it then has told everything
    /// from before the watch.
    cursor: Option<Cursor>,
    caught_up: bool,
}

impl Batch {
    fn empty() -> Batch {
        Batch {
            names: None,
            swept: 0,
            // alloc: none until a path changed; its room is kept as the batch is reused.
            saved: Vec::new(),
            // alloc: none until a path changed; its room is kept as the batch is reused.
            forced: Vec::new(),
            // alloc: none until a path changed; its room is kept as the batch is reused.
            slots: Vec::new(),
            index: HashMap::default(),
            open: 1,
            rescan: None,
            relist: None,
            gone: HashMap::default(),
            cursor: None,
            caught_up: false,
        }
    }

    /// Where `name` is among the swept names, or where it would go.
    fn swept(&self, name: &str) -> Result<usize, usize> {
        let Some(names) = &self.names else { return Err(0) };
        let (mut low, mut high) = (0, self.swept);
        while low < high {
            let mid = (low + high) / 2;
            match names.get(mid).cmp(name) {
                Ordering::Less => low = mid + 1,
                Ordering::Greater => high = mid,
                Ordering::Equal => return Ok(mid),
            }
        }
        Err(low)
    }

    /// Whether the batch looks at `name` already.
    fn known(&self, paths: &Paths, name: &str) -> bool {
        paths.find(name).is_some_and(|it| self.index.contains_key(&it)) || self.swept(name).is_ok()
    }

    fn indexed(&mut self) {
        if self.index.is_empty() {
            self.index.extend(self.slots.iter().enumerate().map(|(at, it)| (it.name, at)));
        }
    }

    /// Takes in what is gone: what the column had under each gone directory is gone too (a
    /// slot each), and each file the head had that left its path (gone, or another inode is
    /// there now) is kept by its inode for [`Batch::moved`]. `dir` is room to hold a gone
    /// directory's name in while its column is walked.
    fn went(
        &mut self,
        paths: &mut Paths,
        dir: &mut String,
        disk: Option<&Disk>,
        tree: &Tree,
        head: NodeId,
        below: usize,
    ) {
        let gone_dir = |it: &Slot| matches!(it.look, Some(Look::Gone)) && it.saved.is_some_and(|it| it.is_dir());
        if let Some(disk) = disk
            && self.slots.iter().any(gone_dir)
        {
            self.indexed();
            // The slots the walks add are not walked: they come after `count`.
            let count = self.slots.len();
            for at in 0..count {
                if !gone_dir(&self.slots[at]) {
                    continue;
                }
                dir.clear();
                dir.push_str(paths.get(self.slots[at].name));
                disk.under(dir.as_str(), |name, saved| {
                    if name == dir.as_str() || self.known(paths, name) {
                        return;
                    }
                    let id = paths.intern(name);
                    let was = seen(tree, head, paths.rel(id, below));
                    self.index.insert(id, self.slots.len());
                    let look = Some(Look::Gone);
                    self.slots.push(Slot { name: id, was, saved: Some(saved), stamp: Some(Stamp::GONE), look });
                });
            }
        }
        for (at, slot) in self.slots.iter().enumerate() {
            let (Some(saved), Some(_)) = (slot.saved, slot.was) else { continue };
            let left = match slot.look {
                Some(Look::Gone) => true,
                _ => slot.stamp.is_some_and(|now| !now.is_gone() && now.ino != saved.ino),
            };
            if left && !saved.is_dir() {
                self.gone.entry(saved.ino).or_insert(at);
            }
        }
    }

    /// The gone slot a file new to the batch, with `stamp`, moved from.
    fn moved(&self, stamp: &Stamp) -> Option<usize> {
        if stamp.is_gone() || stamp.is_unknown() || stamp.is_dir() {
            return None;
        }
        let &at = self.gone.get(&stamp.ino)?;
        let saved = self.slots[at].saved?;
        let same = saved.size == stamp.size
            && saved.mtime_ns == stamp.mtime_ns
            && saved.mode & S_IFMT == stamp.mode & S_IFMT;
        same.then_some(at)
    }

    /// Empties it for another batch, its room kept but past [`SPARE_SLOTS`].
    fn reset(&mut self) {
        self.names = None;
        self.swept = 0;
        self.saved.clear();
        self.saved.shrink_to(SPARE_SLOTS);
        self.forced.clear();
        self.forced.shrink_to(SPARE_SLOTS);
        self.slots.clear();
        self.slots.shrink_to(SPARE_SLOTS);
        self.index.clear();
        self.index.shrink_to(SPARE_SLOTS);
        self.open = 1;
        self.rescan = None;
        self.relist = None;
        self.gone.clear();
        self.gone.shrink_to(SPARE_SLOTS);
        self.cursor = None;
        self.caught_up = false;
    }
}

enum Pending {
    Stamp(u64),
    Entries(u64, PathId),
    Read(u64, usize),
}

/// The lists one completion builds, emptied and kept between them.
#[derive(Default)]
struct Work {
    /// A rescan's or relisting's subtree, relative to the watched directory.
    root: String,
    /// A name being built before it is interned, or held while its column is walked.
    name: String,
    /// What a batch's stamps ask: slots to read with their stamps, directories to list, slots
    /// that changed.
    reads: Vec<(usize, PathId, Stamp)>,
    lists: Vec<PathId>,
    changed: Vec<usize>,
    /// What a listing asks: a slot to read with its stamp (`Some`) or a directory to list, by
    /// the name the effect takes; and directories replaced, to rescan.
    asks: Vec<(Option<(usize, Stamp)>, PathId, FileName)>,
    rescans: Vec<PathId>,
    /// A commit's: each slot it deletes, deletes before and after the ones under another
    /// delete go, writes, moves, paths taken, renames and their sources.
    deleting: Vec<bool>,
    deletes: Vec<PathId>,
    kept: Vec<PathId>,
    writes: Vec<(PathId, Vec<u8>, Mode)>,
    moves: Vec<(usize, usize)>,
    taken: Vec<PathId>,
    renames: Vec<(PathId, PathId)>,
    renamed: HashSet<usize>,
    sources: Vec<PathId>,
    early: Vec<PathId>,
    late: Vec<PathId>,
}

pub struct Watcher {
    head: Box<str>,
    dir: FileName,
    /// `dir` and a `/`: what every name under it starts with.
    below: String,
    tags: u64,
    watching: bool,
    caught_up: bool,
    /// What the journal told of before the watch, held until it has told all of it.
    held: (Vec<FileName>, Vec<(FileName, Lost)>),
    /// Batches in the order their changes were told; the first is number `first`.
    batches: VecDeque<Batch>,
    first: u64,
    /// Looks in flight, by their tag past `tags + FIRST`: a slot is reused once its look is in.
    pending: Slab<Pending>,
    /// What this watcher's commits left at each name, while batches told before them wait.
    ours: HashMap<PathId, Seen>,
    /// Every name a batch, a look in flight or `ours` holds; cleared once none is.
    paths: Paths,
    /// Committed batches, emptied, and the stamp buffer the driver last gave back.
    spare: Vec<Batch>,
    spare_names: Option<Arc<Names>>,
    work: Work,
}

/// The batch numbered `number` of `batches`, whose first is `first`.
fn batch_in(batches: &mut VecDeque<Batch>, first: u64, number: u64) -> Option<&mut Batch> {
    number.checked_sub(first).and_then(|it| batches.get_mut(it as usize))
}

impl Watcher {
    /// A watcher of `dir` (under the driver's root) committing to `head`. Tags it asks with are
    /// `tags..tags + 2^32`.
    pub fn new(head: &str, dir: FileName, tags: u64) -> Watcher {
        Watcher {
            head: head.into(),
            // alloc: once per watch.
            below: format!("{dir}/"),
            dir,
            tags,
            watching: false,
            caught_up: false,
            // alloc: none unless a path changed.
            held: (Vec::new(), Vec::new()),
            batches: VecDeque::new(),
            first: 0,
            pending: Slab::new(MOST_IN_FLIGHT, blaze_mem::Tag::Vfs)
                .unwrap_or_else(|error| panic!("the looks in flight of a watch: {error}")),
            ours: HashMap::default(),
            paths: Paths::default(),
            // alloc: none until a batch commits; then at most `SPARE_BATCHES`, reused.
            spare: Vec::new(),
            spare_names: None,
            work: Work::default(),
        }
    }

    /// Starts following the directory from where `vfs` saved this head's disk, or from scratch.
    pub fn start(&mut self, vfs: &Vfs, effects: &mut Vec<Effect>) {
        assert!(!self.watching, "{} is watched already", self.dir);
        self.watching = true;
        let since = vfs
            .disks
            .get(&self.head)
            .filter(|disk| disk.dir == self.dir)
            .map_or(Cursor::None, |disk| disk.cursor);
        // clone: once per watch.
        effects.push(Effect::Journal { dir: self.dir.clone(), since, tag: Tag(self.tags) });
        if since == Cursor::None {
            let dir = self.paths.intern(self.dir.as_str());
            self.rescan_at(vfs, dir, effects);
        }
    }

    /// Stops the watch: it ends with [`Watched::Ended`]. Looks in flight still commit.
    pub fn stop(&mut self, effects: &mut Vec<Effect>) {
        if self.watching {
            self.watching = false;
            effects.push(Effect::Unwatch { tag: Tag(self.tags) });
        }
    }

    /// Whether a completion is of an effect this asked for.
    pub fn owns(&self, tag: Tag) -> bool {
        tag.0.checked_sub(self.tags).is_some_and(|it| it < TAGS)
    }

    /// Whether nothing it was told is waiting to be looked at or committed.
    pub fn idle(&self) -> bool {
        self.batches.is_empty() && self.held.0.is_empty() && self.held.1.is_empty()
    }

    /// `name` relative to the watched directory (`""` for the directory itself); `None` for a
    /// name outside it.
    fn rel<'a>(&self, name: &'a str) -> Option<&'a str> {
        match name.strip_prefix(&self.below) {
            Some(rest) => Some(rest),
            None => (name == self.dir.as_str()).then_some(""),
        }
    }

    fn head_root(&self, vfs: &Vfs) -> NodeId {
        vfs.heads.get(&*self.head).copied().unwrap_or(vfs.tree.empty())
    }

    fn disk<'a>(&self, vfs: &'a Vfs) -> Option<&'a Disk> {
        vfs.disks.get(&self.head).filter(|it| it.dir == self.dir)
    }

    fn tag(&mut self, pending: Pending) -> Tag {
        Tag(self.tags + FIRST + u64::from(self.pending.insert(pending).index()))
    }

    /// The look `tag` was asked for, out of its slot: each tag completes once.
    fn take(&mut self, tag: Tag) -> Pending {
        let index = tag.0 - self.tags - FIRST;
        let Some(handle) = u32::try_from(index).ok().and_then(|it| self.pending.handle_at(it)) else {
            unreachable!("a completion {tag:?} of nothing asked")
        };
        let Some(pending) = self.pending.remove(handle) else { unreachable!("an occupied slot") };
        pending
    }

    fn push(&mut self, batch: Batch) -> u64 {
        let number = self.first + self.batches.len() as u64;
        self.batches.push_back(batch);
        number
    }

    /// An empty batch, a committed one's room reused where there is one.
    fn fresh(&mut self, rescan: Option<PathId>, relist: Option<PathId>) -> Batch {
        let mut batch = self.spare.pop().unwrap_or_else(Batch::empty);
        (batch.rescan, batch.relist) = (rescan, relist);
        batch
    }

    /// A committed batch's room, and its stamp buffer once the driver let it go, kept.
    fn recycle(&mut self, mut batch: Batch) {
        if let Some(mut names) = batch.names.take()
            && Arc::get_mut(&mut names).map(Names::clear).is_some()
        {
            self.spare_names = Some(names);
        }
        if self.spare.len() < SPARE_BATCHES {
            batch.reset();
            self.spare.push(batch);
        }
    }

    /// An empty stamp buffer only this watcher holds.
    fn names(&mut self) -> Arc<Names> {
        // alloc: once per watch, and again only while the driver still holds the last one.
        self.spare_names.take().unwrap_or_default()
    }

    /// The name `id` as an effect takes it.
    fn file_name(&self, id: PathId) -> FileName {
        // alloc: the name an effect hands the driver, which takes it owned (blaze-core's `Effect`).
        FileName::new(self.paths.get(id)).expect("an interned name is a name")
    }

    /// Takes a completion of its own; commits what is looked at in full to the head, and
    /// flushes `vfs` after.
    pub fn complete(
        &mut self,
        vfs: &mut Vfs,
        completion: Completion,
        effects: &mut Vec<Effect>,
    ) -> Vec<Watched> {
        // alloc: none unless a path changed.
        let mut out = Vec::new();
        if completion.tag == Tag(self.tags) {
            match completion.done {
                Done::Journaled(batch) => self.journaled(vfs, *batch, effects, &mut out),
                Done::Unwatched => {
                    self.watching = false;
                    out.push(Watched::Ended(None));
                }
                Done::Failed(error) => {
                    self.watching = false;
                    out.push(Watched::Ended(Some(error)));
                }
                done => unreachable!("a journal completed with {done:?}"),
            }
            self.drain(vfs, &mut out, effects);
            return out;
        }
        let pending = self.take(completion.tag);
        let below = self.below.len();
        match (pending, completion.done) {
            (Pending::Stamp(number), Done::Stamped(stamps)) => {
                self.stamped(vfs, number, stamps, effects, &mut out);
            }
            (Pending::Entries(number, dir), Done::Entries(entries)) => {
                self.listed(vfs, number, dir, entries, effects);
            }
            (Pending::Read(number, at), done) => {
                let Watcher { batches, first, paths, .. } = self;
                if let Some(slot) = batch_in(batches, *first, number).and_then(|it| it.slots.get_mut(at)) {
                    slot.look = Some(match done {
                        Done::Read(bytes) => Look::File(bytes),
                        Done::Failed(error) => {
                            // alloc: an error path.
                            out.push(Watched::Unreadable { path: paths.rel(slot.name, below).to_owned(), error });
                            Look::Unreadable
                        }
                        done => unreachable!("a read of {} completed with {done:?}", paths.get(slot.name)),
                    });
                }
            }
            (Pending::Stamp(number), Done::Failed(error)) => {
                let Watcher { batches, first, paths, .. } = self;
                let Some(batch) = batch_in(batches, *first, number) else { unreachable!("stamps of no batch") };
                batch.open -= 1;
                // alloc: an error path.
                let path = batch.rescan.map(|it| paths.rel(it, below).to_owned()).unwrap_or_default();
                for slot in batch.slots.iter_mut().filter(|it| it.look.is_none()) {
                    slot.look = Some(Look::Unreadable);
                }
                let whole = batch.rescan.is_some_and(|it| paths.rel(it, below).is_empty());
                if whole {
                    self.stop(effects);
                }
                out.push(Watched::Unreadable { path, error });
            }
            (Pending::Entries(number, dir), Done::Failed(error)) => {
                let Watcher { batches, first, paths, .. } = self;
                let Some(batch) = batch_in(batches, *first, number) else { unreachable!("a listing of no batch") };
                batch.open -= 1;
                if batch.relist == Some(dir) {
                    for slot in batch.slots.iter_mut().filter(|it| it.look.is_none()) {
                        slot.look = Some(Look::Unreadable);
                    }
                }
                // alloc: an error path.
                let path = paths.rel(dir, below).to_owned();
                if path.is_empty() {
                    self.stop(effects);
                }
                out.push(Watched::Unreadable { path, error });
            }
            (_, done) => unreachable!("a look completed with {done:?}"),
        }
        self.drain(vfs, &mut out, effects);
        out
    }

    /// A batch of the journal's: what it names is looked at, each subtree it lost is looked at
    /// again, and its cursor is saved once they and everything told before are committed. What
    /// changed before the watch is held until the journal has told all of it, then looked at
    /// as one batch and committed as one transaction: a rename told across two of the
    /// journal's batches is still one move.
    fn journaled(
        &mut self,
        vfs: &mut Vfs,
        told: blaze_core::Journaled,
        effects: &mut Vec<Effect>,
        out: &mut Vec<Watched>,
    ) {
        let blaze_core::Journaled { mut files, mut lost, cursor, caught_up } = told;
        if !self.caught_up {
            self.held.0.append(&mut files);
            self.held.1.append(&mut lost);
            if !caught_up {
                return;
            }
            (files, lost) = std::mem::take(&mut self.held);
            files.sort_unstable();
            files.dedup();
        }
        self.told(vfs, files, effects);
        for (under, why) in lost {
            // alloc: an output's path.
            let Some(path) = self.rel(under.as_str()).map(str::to_owned) else { continue };
            out.push(Watched::Lost { path, why });
            match why {
                Lost::Unnamed => self.relist(vfs, under, effects),
                _ => self.rescan(vfs, under, effects),
            }
        }
        let was = self.caught_up;
        self.caught_up = caught_up || was;
        let caught_up = caught_up && !was;
        match self.batches.back_mut() {
            Some(last) => {
                last.cursor = Some(cursor);
                last.caught_up |= caught_up;
            }
            None => {
                vfs.disks.of(&self.head, &self.dir).cursor = cursor;
                if caught_up {
                    out.push(Watched::Scanned);
                }
            }
        }
    }

    /// The names the journal told: a batch of them, stamped together.
    fn told(&mut self, vfs: &Vfs, files: Vec<FileName>, effects: &mut Vec<Effect>) {
        let head = self.head_root(vfs);
        let disk = self.disk(vfs);
        let mut batch = self.fresh(None, None);
        for file in files {
            let Some(path) = self.rel(file.as_str()).filter(|it| !it.is_empty()) else { continue };
            let saved = disk.and_then(|it| it.get(file.as_str()));
            let was = seen(&vfs.tree, head, path);
            let name = self.paths.intern(file.as_str());
            batch.slots.push(Slot { name, was, saved, stamp: None, look: None });
        }
        if batch.slots.is_empty() {
            self.spare.push(batch);
        } else {
            self.ask_stamps(batch, effects);
        }
    }

    /// Looks at the subtree `under` again: every path the column and the head have under it is
    /// stamped, and the directories whose stamp moved are listed. The column's names are swept
    /// as they are ([`Batch::names`]); of the head, only what changed since the column held all
    /// of it is added (all of it when the column never did).
    pub fn rescan(&mut self, vfs: &Vfs, under: FileName, effects: &mut Vec<Effect>) {
        if self.rel(under.as_str()).is_none() {
            return;
        }
        let under = self.paths.intern(under.as_str());
        self.rescan_at(vfs, under, effects);
    }

    fn rescan_at(&mut self, vfs: &Vfs, under: PathId, effects: &mut Vec<Effect>) {
        let mut root = std::mem::take(&mut self.work.root);
        root.clear();
        match self.rel(self.paths.get(under)) {
            Some(rel) => root.push_str(rel),
            None => {
                self.work.root = root;
                return;
            }
        }
        let head = self.head_root(vfs);
        let disk = self.disk(vfs);
        let mut batch = self.fresh(Some(under), None);
        let mut names = self.names();
        let unshared = Arc::get_mut(&mut names).expect("a spare stamp buffer is the watcher's alone");
        if let Some(disk) = disk {
            disk.sweep(self.paths.get(under), unshared, &mut batch.saved);
        }
        batch.swept = unshared.len();
        batch.names = Some(names);
        let missing = batch.swept(self.paths.get(under)).is_err();
        {
            let Watcher { paths, dir, below, work, .. } = self;
            let name = &mut work.name;
            let tree = &vfs.tree;
            let inside = |path: &str| {
                root.is_empty()
                    || path.strip_prefix(root.as_str()).is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
            };
            let mut add = |path: &str, batch: &mut Batch| {
                name.clear();
                if path.is_empty() {
                    name.push_str(dir.as_str());
                } else {
                    name.push_str(below.as_str());
                    name.push_str(path);
                }
                match batch.swept(name.as_str()) {
                    Ok(at) => batch.forced.push(at as u32),
                    Err(_) => {
                        let id = paths.intern(name.as_str());
                        if !batch.index.contains_key(&id) {
                            let was = seen(tree, head, path);
                            batch.index.insert(id, batch.slots.len());
                            batch.slots.push(Slot { name: id, was, saved: None, stamp: None, look: None });
                        }
                    }
                }
            };
            if missing {
                add(&root, &mut batch);
            }
            match disk.and_then(|it| it.root).and_then(|it| tree.node(&it)) {
                Some(synced) => {
                    for change in tree.diff(synced, head) {
                        if inside(&change.path) {
                            add(&change.path, &mut batch);
                        }
                    }
                }
                None => {
                    if let Some(node) = tree.lookup(head, &root) {
                        for (path, ..) in tree.files(node, Mode::Regular, &root) {
                            add(&path, &mut batch);
                        }
                    }
                }
            }
            batch.forced.sort_unstable();
            batch.forced.dedup();
            let names = batch.names.as_mut().expect("set above");
            let unshared = Arc::get_mut(names).expect("not given to the driver yet");
            for slot in &batch.slots {
                unshared.push_checked(paths.get(slot.name));
            }
        }
        self.work.root = root;
        let files = Arc::clone(batch.names.as_ref().expect("set above"));
        let number = self.push(batch);
        let tag = self.tag(Pending::Stamp(number));
        effects.push(Effect::Stamp { files, tag });
    }

    /// Lists the directory `dir` alone again, for [`Lost::Unnamed`]: a name directly in it that
    /// the column or the head has and the listing has not is gone with what is under it, a
    /// name new to it is taken whole, and the rest are as they were.
    fn relist(&mut self, vfs: &Vfs, dir: FileName, effects: &mut Vec<Effect>) {
        let mut root = std::mem::take(&mut self.work.root);
        root.clear();
        match self.rel(dir.as_str()) {
            Some(rel) => root.push_str(rel),
            None => {
                self.work.root = root;
                return;
            }
        }
        let head = self.head_root(vfs);
        let disk = self.disk(vfs);
        let below = self.below.len();
        let id = self.paths.intern(dir.as_str());
        let mut batch = self.fresh(None, Some(id));
        {
            let Watcher { paths, below: below_text, work, .. } = self;
            let tree = &vfs.tree;
            if let Some(disk) = disk {
                disk.children(dir.as_str(), |name, saved| {
                    let id = paths.intern(name);
                    let was = seen(tree, head, paths.rel(id, below));
                    batch.index.insert(id, batch.slots.len());
                    batch.slots.push(Slot { name: id, was, saved: Some(saved), stamp: None, look: None });
                });
            }
            if let Some(node) = tree.lookup(head, &root)
                && let Some(entries) = tree.entries(node)
            {
                for (name, child, mode) in entries.iter() {
                    let full = &mut work.name;
                    full.clear();
                    full.push_str(below_text.as_str());
                    if !root.is_empty() {
                        full.push_str(&root);
                        full.push('/');
                    }
                    full.push_str(tree.name_str(name));
                    let id = paths.intern(full.as_str());
                    if batch.index.contains_key(&id) {
                        continue;
                    }
                    batch.index.insert(id, batch.slots.len());
                    let was = Some((tree.id(child), mode));
                    batch.slots.push(Slot { name: id, was, saved: None, stamp: None, look: None });
                }
            }
        }
        self.work.root = root;
        let number = self.push(batch);
        let tag = self.tag(Pending::Entries(number, id));
        effects.push(Effect::Entries { dir, tag });
    }

    /// A batch of slots, and the stamp of each asked for in one effect.
    fn ask_stamps(&mut self, mut batch: Batch, effects: &mut Vec<Effect>) {
        let mut names = self.names();
        let unshared = Arc::get_mut(&mut names).expect("a spare stamp buffer is the watcher's alone");
        for slot in &batch.slots {
            unshared.push_checked(self.paths.get(slot.name));
        }
        batch.names = Some(Arc::clone(&names));
        let number = self.push(batch);
        let tag = self.tag(Pending::Stamp(number));
        effects.push(Effect::Stamp { files: names, tag });
    }

    /// The stamps of a batch: what is unchanged is settled, a file new to its path (or renamed
    /// over it) with a gone file's stamp is that file moved, what else moved is read, and each
    /// directory that is new, replaced, or (in a rescan) moved is listed.
    fn stamped(
        &mut self,
        vfs: &Vfs,
        number: u64,
        stamps: Vec<Stamp>,
        effects: &mut Vec<Effect>,
        out: &mut Vec<Watched>,
    ) {
        let head = self.head_root(vfs);
        let disk = self.disk(vfs);
        let below = self.below.len();
        let mut stop = false;
        {
            let Watcher { batches, first, paths, work, .. } = self;
            let Work { name: scratch, reads, lists, changed, .. } = work;
            let Some(batch) = batch_in(batches, *first, number) else { unreachable!("stamps of no batch") };
            let swept = batch.swept;
            assert_eq!(stamps.len(), swept + batch.slots.len(), "a stamp per name");
            let rescan = batch.rescan.is_some();
            batch.open -= 1;
            let (of_swept, of_slots) = stamps.split_at(swept);
            for (slot, stamp) in batch.slots.iter_mut().zip(of_slots) {
                slot.stamp = Some(*stamp);
            }
            let mut forced = batch.forced.iter().copied().peekable();
            for (at, (saved, now)) in batch.saved.iter().zip(of_swept).enumerate() {
                let force = forced.next_if_eq(&(at as u32)).is_some();
                if saved == now && !force {
                    continue;
                }
                let name = paths.intern(batch.names.as_deref().expect("a sweep's names").get(at));
                let was = seen(&vfs.tree, head, paths.rel(name, below));
                batch.slots.push(Slot { name, was, saved: Some(*saved), stamp: Some(*now), look: None });
            }
            for (at, slot) in batch.slots.iter_mut().enumerate() {
                let stamp = slot.stamp.expect("stamped above");
                if stamp.is_gone() {
                    if paths.rel(slot.name, below).is_empty() {
                        out.push(Watched::Unreadable {
                            // alloc: an error path.
                            path: String::new(),
                            // alloc: an error path.
                            error: IoError(format!("{}: no such directory", paths.get(slot.name))),
                        });
                        slot.look = Some(Look::Unreadable);
                        stop = true;
                    } else {
                        slot.look = Some(Look::Gone);
                    }
                } else if stamp.is_dir() {
                    slot.look = Some(Look::Dir);
                    let list = match slot.saved {
                        None => true,
                        Some(saved) if rescan => saved.moved(&stamp),
                        Some(saved) => saved.ino != stamp.ino,
                    };
                    if list {
                        lists.push(slot.name);
                    }
                } else if slot.saved == Some(stamp) && slot.was.is_some() {
                    slot.look = Some(Look::Same);
                } else {
                    changed.push(at);
                }
            }
            if !lists.is_empty() {
                batch.indexed();
            }
            batch.went(paths, scratch, disk, &vfs.tree, head, below);
            for &at in changed.iter() {
                let slot = &batch.slots[at];
                let stamp = slot.stamp.expect("stamped above");
                // A new name, or one another file was renamed over (it has another inode).
                let from = match slot.saved {
                    Some(saved) if saved.ino == stamp.ino => None,
                    _ => batch.moved(&stamp),
                };
                match from {
                    Some(from) => batch.slots[at].look = Some(Look::Moved(from)),
                    None => reads.push((at, slot.name, stamp)),
                }
            }
            changed.clear();
            batch.open += lists.len() as u32;
        }
        for at in 0..self.work.reads.len() {
            let (slot, name, stamp) = self.work.reads[at];
            let tag = self.tag(Pending::Read(number, slot));
            effects.push(read(self.file_name(name), stamp, tag));
        }
        self.work.reads.clear();
        for at in 0..self.work.lists.len() {
            let dir = self.work.lists[at];
            let tag = self.tag(Pending::Entries(number, dir));
            effects.push(Effect::Entries { dir: self.file_name(dir), tag });
        }
        self.work.lists.clear();
        if stop {
            self.stop(effects);
        }
    }

    /// A listing of a directory of a batch: each name the batch has not looked at is new to it,
    /// moved there if it has a gone file's stamp, else read if a file and listed if a
    /// directory. The listing of a relisted directory first settles what was in it.
    fn listed(
        &mut self,
        vfs: &Vfs,
        number: u64,
        dir: PathId,
        entries: Vec<(FileName, Stamp)>,
        effects: &mut Vec<Effect>,
    ) {
        let head = self.head_root(vfs);
        let disk = self.disk(vfs);
        let below = self.below.len();
        {
            let Watcher { batches, first, paths, work, .. } = self;
            let Work { name: scratch, asks, rescans, .. } = work;
            let Some(batch) = batch_in(batches, *first, number) else { unreachable!("a listing of no batch") };
            batch.open -= 1;
            if batch.relist == Some(dir) {
                let there: HashMap<&str, Stamp> =
                    // alloc: once per relisting, by the directory's names.
                    entries.iter().map(|(it, stamp)| (it.as_str(), *stamp)).collect();
                for (at, slot) in batch.slots.iter_mut().enumerate().filter(|(_, it)| it.look.is_none()) {
                    let Some(&now) = there.get(paths.get(slot.name)) else {
                        slot.stamp = Some(Stamp::GONE);
                        slot.look = Some(Look::Gone);
                        continue;
                    };
                    if now.is_dir() {
                        let kept = slot.saved.is_some_and(|it| it.is_dir() && it.ino == now.ino);
                        if !kept {
                            rescans.push(slot.name);
                        }
                        slot.look = Some(Look::Same);
                    } else if slot.saved == Some(now) && slot.was.is_some() {
                        slot.look = Some(Look::Same);
                    } else {
                        slot.stamp = Some(now);
                        // alloc: the name the read hands the driver, which takes it owned.
                        let file = FileName::new(paths.get(slot.name)).expect("an interned name is a name");
                        asks.push((Some((at, now)), slot.name, file));
                    }
                }
                batch.went(paths, scratch, disk, &vfs.tree, head, below);
            }
            for (file, stamp) in entries {
                if batch.known(paths, file.as_str()) || file.as_str().len() <= below {
                    continue;
                }
                let name = paths.intern(file.as_str());
                let at = batch.slots.len();
                batch.index.insert(name, at);
                let was = seen(&vfs.tree, head, paths.rel(name, below));
                let look = if stamp.is_gone() {
                    Some(Look::Gone)
                } else if stamp.is_dir() {
                    asks.push((None, name, file));
                    batch.open += 1;
                    Some(Look::Dir)
                } else if let Some(from) = batch.moved(&stamp) {
                    Some(Look::Moved(from))
                } else {
                    asks.push((Some((at, stamp)), name, file));
                    None
                };
                batch.slots.push(Slot { name, was, saved: None, stamp: Some(stamp), look });
            }
        }
        let mut asks = std::mem::take(&mut self.work.asks);
        for (at, name, file) in asks.drain(..) {
            match at {
                Some((at, stamp)) => {
                    let tag = self.tag(Pending::Read(number, at));
                    effects.push(read(file, stamp, tag));
                }
                None => {
                    let tag = self.tag(Pending::Entries(number, name));
                    effects.push(Effect::Entries { dir: file, tag });
                }
            }
        }
        self.work.asks = asks;
        let mut rescans = std::mem::take(&mut self.work.rescans);
        for under in rescans.drain(..) {
            self.rescan_at(vfs, under, effects);
        }
        self.work.rescans = rescans;
    }

    /// Commits every batch looked at in full that waits for none told before it.
    fn drain(&mut self, vfs: &mut Vfs, out: &mut Vec<Watched>, effects: &mut Vec<Effect>) {
        while self
            .batches
            .front()
            .is_some_and(|it| it.open == 0 && it.slots.iter().all(|it| it.look.is_some()))
        {
            let mut batch = self.batches.pop_front().expect("a batch");
            self.first += 1;
            let scanned = batch.rescan.is_some() || batch.relist.is_some() || batch.caught_up;
            let again = self.commit(vfs, &mut batch, out);
            self.recycle(batch);
            if scanned {
                out.push(Watched::Scanned);
            }
            for name in again {
                vfs.disks.of(&self.head, &self.dir).set(self.paths.get(name), Stamp::GONE);
                // alloc: an output's path.
                let path = self.paths.rel(name, self.below.len()).to_owned();
                out.push(Watched::Lost { path, why: Lost::Rescan });
                self.rescan_at(vfs, name, effects);
            }
        }
        if self.batches.is_empty() {
            self.ours.clear();
            if self.pending.is_empty() {
                self.paths.clear();
            }
        }
        vfs.flush(effects);
    }

    /// Commits what a batch saw; returns the files moved from a node the tree no longer has,
    /// to be looked at again.
    fn commit(&mut self, vfs: &mut Vfs, batch: &mut Batch, out: &mut Vec<Watched>) -> Vec<PathId> {
        let Watcher { head, dir, below, paths, ours, work, .. } = self;
        let below = below.len();
        save(vfs.disks.of(head, dir), batch, paths);
        let base = vfs.heads.get(&**head).copied().unwrap_or(vfs.tree.empty());
        let tree = &vfs.tree;
        let Work { deleting, deletes, kept, writes, moves, taken, renames, renamed, sources, early, late, .. } = work;
        // The gone slots this commit deletes: a file moved from one of them is renamed.
        deleting.clear();
        deleting.resize(batch.slots.len(), false);
        (deletes.clear(), kept.clear(), writes.clear(), moves.clear(), taken.clear());
        (renames.clear(), renamed.clear(), sources.clear(), early.clear(), late.clear());
        for (at, slot) in batch.slots.iter_mut().enumerate() {
            let path = paths.rel(slot.name, below);
            if path.is_empty() || matches!(slot.look, Some(Look::Same | Look::Unreadable)) {
                continue;
            }
            let entry = tree.entry(base, path);
            let node = entry.map(|it| it.0);
            let now = entry.map(|(node, mode)| (tree.id(node), mode));
            if now != slot.was && !made(ours, paths, below, slot.name, now) {
                // alloc: an output's path.
                out.push(Watched::Conflict { path: path.to_owned() });
                continue;
            }
            let is_file = node.is_some_and(|it| !tree.is_dir(it));
            match slot.look.take().expect("a looked-at path") {
                Look::Same | Look::Unreadable => unreachable!("skipped above"),
                Look::Gone if node.is_some() => {
                    deletes.push(slot.name);
                    deleting[at] = true;
                }
                Look::Gone => {}
                Look::Dir if is_file => deletes.push(slot.name),
                Look::Dir => {}
                Look::Moved(from) => {
                    moves.push((at, from));
                    continue;
                }
                Look::File(bytes) => {
                    room(tree, base, paths, slot.name, below, deletes);
                    let had = entry.filter(|_| is_file).map(|it| it.1);
                    let mode = file_mode(slot.stamp.expect("a read path is stamped"), had);
                    // Compared in place, not hashed: the commit hashes what it writes, once. A
                    // blob of the image that fails its check compares unequal, so it is replaced.
                    let same = had == Some(mode)
                        && node.is_some_and(|it| matches!(tree.bytes(it), Ok(Some(have)) if *have == *bytes));
                    if !same {
                        writes.push((slot.name, bytes, mode));
                    }
                }
            }
            taken.push(slot.name);
        }

        // alloc: none unless a file moved from a node the tree no longer has.
        let mut again = Vec::new();
        for &(at, from) in moves.iter() {
            let (target, source) = (&batch.slots[at], &batch.slots[from]);
            let Some((id, mode)) = source.was else { unreachable!("a move is from a file the head had") };
            let entry = tree.entry(base, paths.rel(target.name, below));
            if entry.is_some_and(|(node, _)| tree.id(node) == id) {
                taken.push(target.name);
                continue;
            }
            let clear = entry.is_none()
                && !ancestors(paths.rel(target.name, below))
                    .any(|up| tree.lookup(base, up).is_some_and(|it| !tree.is_dir(it)));
            if deleting[from] && clear && renamed.insert(from) {
                renames.push((source.name, target.name));
            } else {
                match tree.node(&id).map(|node| tree.bytes(node)) {
                    Some(Ok(Some(bytes))) => {
                        room(tree, base, paths, target.name, below, deletes);
                        // alloc: per file moved from a node no op keeps, its bytes copied once.
                        writes.push((target.name, bytes.to_vec(), mode));
                    }
                    _ => {
                        again.push(target.name);
                        continue;
                    }
                }
            }
            taken.push(target.name);
        }

        let text = |it: PathId| paths.rel(it, below);
        deletes.sort_unstable_by(|a, b| text(*a).cmp(text(*b)));
        deletes.dedup();
        // A path under one deleted goes with it.
        let deleted = |path: &str| deletes.binary_search_by(|it| text(*it).cmp(path)).is_ok();
        kept.extend(deletes.iter().copied().filter(|it| !ancestors(text(*it)).any(deleted)));
        // A rename moves its source out first: a delete at or above one waits for the renames,
        // and is left out when nothing else is under it.
        sources.extend(renames.iter().map(|&(from, _)| from));
        sources.sort_unstable_by(|a, b| text(*a).cmp(text(*b)));
        let source = |path: &str| sources.binary_search_by(|it| text(*it).cmp(path)).is_ok();
        let covers = |path: &str| {
            let first_below = sources.partition_point(|it| cmp_below(text(*it), path) == Ordering::Less);
            source(path) || sources.get(first_below).is_some_and(|it| is_below(text(*it), path))
        };
        for &it in kept.iter() {
            let path = text(it);
            if !covers(path) {
                early.push(it);
            } else if !source(path)
                && tree.lookup(base, path).is_some_and(|dir| {
                    tree.files(dir, Mode::Regular, path).iter().any(|(file, ..)| !source(file.as_str()))
                })
            {
                late.push(it);
            }
        }
        let count = early.len() + renames.len() + late.len() + writes.len();
        if count == 0 {
            return again;
        }
        // alloc: the commit's ops (the API's type), each with its path owned; once per batch.
        let mut ops = Vec::with_capacity(count);
        let owned = |it: PathId| text(it).to_owned(); // alloc: an op's path, which `Op` owns
        ops.extend(early.iter().map(|&it| Op::Delete { path: owned(it) }));
        ops.extend(renames.iter().map(|&(from, to)| Op::Rename { to: owned(to), from: owned(from) }));
        ops.extend(late.iter().map(|&it| Op::Delete { path: owned(it) }));
        // The bytes read become the files' own, not copied again.
        ops.extend(writes.drain(..).map(|(it, bytes, mode)| Op::Write { path: owned(it), bytes, mode }));
        let base = vfs.tree.id(base);
        let commit = vfs.commit_owned(head, base, ops).unwrap_or_else(|error| {
            panic!("the disk's changes to {dir} do not apply to its head: {error}")
        });
        let root = vfs.root(commit.root).expect("the root just made");
        // Every write's path is in `taken`; the deletes and renames record what the commit made.
        let renamed_paths = renames.iter().flat_map(|&(from, to)| [from, to]);
        for it in taken.iter().chain(early.iter()).chain(late.iter()).copied().chain(renamed_paths) {
            ours.insert(it, seen(&vfs.tree, root, paths.rel(it, below)));
        }
        out.push(Watched::Committed(commit));
        again
    }
}

/// Whether the watcher's commits left `name` as `now`: it put that there, or `name` is gone
/// because they replaced or deleted something above it.
fn made(ours: &HashMap<PathId, Seen>, paths: &Paths, below: usize, name: PathId, now: Seen) -> bool {
    if ours.get(&name) == Some(&now) {
        return true;
    }
    let full = paths.get(name);
    now.is_none()
        && ancestors(&full[below..])
            .any(|up| paths.find(&full[..below + up.len()]).is_some_and(|it| ours.contains_key(&it)))
}

/// `name` against `dir` and a `/`, bytewise, without making that string.
fn cmp_below(name: &str, dir: &str) -> Ordering {
    let (name, dir) = (name.as_bytes(), dir.as_bytes());
    match name.get(..dir.len()) {
        Some(head) => head.cmp(dir).then_with(|| match name.get(dir.len()) {
            None => Ordering::Less,
            Some(byte) => byte.cmp(&b'/').then(name.len().cmp(&(dir.len() + 1))),
        }),
        None => name.cmp(dir).then(Ordering::Less),
    }
}

/// Whether `name` is under the directory `dir`.
fn is_below(name: &str, dir: &str) -> bool {
    name.len() > dir.len() && name.starts_with(dir) && name.as_bytes()[dir.len()] == b'/'
}

/// Makes room in `base` for a file at `name`: the files above it and a directory at it go.
fn room(tree: &Tree, base: NodeId, paths: &mut Paths, name: PathId, below: usize, deletes: &mut Vec<PathId>) {
    let mut from = 0;
    loop {
        let path = paths.rel(name, below);
        let Some(slash) = path[from..].find('/') else { break };
        let up = from + slash;
        from = up + 1;
        if tree.lookup(base, &path[..up]).is_some_and(|it| !tree.is_dir(it)) {
            deletes.push(paths.intern_prefix(name, below + up));
        }
    }
    if tree.lookup(base, paths.rel(name, below)).is_some_and(|it| tree.is_dir(it)) {
        deletes.push(name);
    }
}

/// What a committed batch saw goes into the column: each path's stamp (a conflict's too: the
/// disk was seen and the head chose), gone paths with everything under them, and the journal's
/// cursor after it. An unreadable path keeps what it had, so it is looked at again.
fn save(disk: &mut Disk, batch: &Batch, paths: &Paths) {
    for slot in &batch.slots {
        let Some(stamp) = slot.stamp else { continue };
        match slot.look {
            Some(Look::Unreadable) | None => {}
            Some(Look::Gone) => disk.remove_under(paths.get(slot.name)),
            Some(Look::Same) => {}
            Some(Look::Dir | Look::File(_) | Look::Moved(_)) => disk.set(paths.get(slot.name), stamp),
        }
    }
    if let Some(cursor) = batch.cursor {
        disk.cursor = cursor;
    }
}

#[cfg(test)]
mod reference;
#[cfg(test)]
mod tests;
