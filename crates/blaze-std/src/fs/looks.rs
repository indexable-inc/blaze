//! Many paths beneath a directory looked at in ONE effect: the batched stat a repeat's sweep
//! asks for (docs/v2/plan/design/std.md, "End state: efficiency", piece 4).
//!
//! A [`Dir::stat`] is one trip through the effect layer per path: the world's lock taken
//! several times, and on macOS a hop to a kqueue file thread and back. On tempo the sweep's
//! stats took 8.8 ms on one worker and 11.9 to 12.3 ms on 18 (blaze-cli sweep.rs,
//! `Follow::follow`, 2026-11-16): the cost is the trip, not the system call. [`Dir::looks`]
//! asks blaze-core's [`Effect::Stamp`] once for the whole column. The native drivers
//! `fstatat` each name relative to the root's descriptor on a file thread (0.77 us a name
//! on APFS, warm, blaze-core `examples/stat_cost.rs`, 870 names); io_uring stamps them as
//! `statx`es; the simulation stamps them from its disk, as its `Stat` does.
//!
//! A [`Look`] says all a [`Meta`] does: the kind, the length, both times, the inode and the
//! device (blaze-core's [`blaze_core::Stamp::dev`] since 2026-11-21, each driver's as its
//! `Meta`'s), so [`Look::same_as`] proves exactly what two stats would.
//!
//! **Many folders, sorted paths: [`Dir::facts_of`].** The same looks, one row a path, as
//! columns in the caller's frame, for paths sorted by their bytes; each FOLDER is either
//! listed whole ([`Effect::Entries`]: one `getattrlistbulk` a buffer on macOS) with its asked
//! names picked out by a merge, or has its paths stamped by name in the one [`Effect::Stamp`],
//! chosen per folder by a count ([`Strategy`]; the system calls of each are in
//! [`Dir::facts_of_by`]'s doc). A folder whose listing fails while it is there gives its rows
//! as [`EntryKind::Unknown`] with the error ([`Facts::failure`]); the rest are answered.

use core::{cmp::Ordering, fmt};

use blaze_core::{DirName, Done, Effect, FileName, Listing, Names, STAMP_MOST, Stamp as Stamped, Tag};
use blaze_mem::Frame;
use blaze_own::Vec as OwnVec;

use super::{Arc, Dir, EntryKind, FsError, FsKind, FsOp, Meta, Rights, Vec, call, unexpected};
use crate::{Stopped, cx::Cx, io::Ends, mem::Bytes, path::Path};

/// What one path is now, from a batched look ([`Dir::looks`]).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Look {
    /// [`EntryKind::Gone`] when nothing is there. [`EntryKind::Unknown`] when the system
    /// would say nothing of it (no permission, an I/O error): the asker reads it itself and
    /// meets the error by name.
    pub kind: EntryKind,
    pub len: u64,
    /// The content's last change, in ns since the epoch on the file system's clock.
    pub modified_ns: u64,
    /// The inode's last change (a chmod, a rename, a link).
    pub changed_ns: u64,
    /// The inode and the device it is on, as [`super::Stamp`]'s.
    pub ino: u64,
    pub dev: u64,
}

impl Look {
    /// The look a stamp says: also how [`Facts`] reads its rows.
    fn of(stamp: &Stamped) -> Look {
        Look {
            kind: EntryKind::of(stamp),
            len: stamp.size,
            modified_ns: stamp.mtime_ns,
            changed_ns: stamp.ctime_ns,
            ino: stamp.ino,
            dev: stamp.dev,
        }
    }

    /// Whether this look and `meta` (from a [`Dir::stat`] of the same path) describe one
    /// version of one file or directory: the kind (file or directory), the length, both
    /// times, the inode and the device are equal. A gone or unknown look is the same as no
    /// meta.
    pub fn same_as(&self, meta: &Meta) -> bool {
        let dir = match self.kind {
            EntryKind::File => false,
            EntryKind::Dir => true,
            EntryKind::Link | EntryKind::Other | EntryKind::Gone | EntryKind::Unknown => return false,
        };
        dir == meta.dir
            && self.len == meta.len
            && self.modified_ns == meta.stamp.modified_ns
            && self.changed_ns == meta.stamp.changed_ns
            && self.ino == meta.stamp.ino
            && self.dev == meta.stamp.dev
    }
}

/// The looks of a column of paths, in the order they were given: a column of the driver's,
/// moved here whole (as [`super::Entries`] holds its listing); nothing is copied into rows.
#[derive(Debug)]
pub struct Looks {
    // alloc: the completion's column, which blaze-core's `Done::Stamped` owns.
    stamps: Vec<Stamped>,
}

impl Looks {
    pub fn len(&self) -> usize {
        self.stamps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.stamps.is_empty()
    }

    /// The look of the `i`th path given.
    pub fn get(&self, i: usize) -> Option<Look> {
        self.stamps.get(i).map(Look::of)
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = Look> + '_ {
        self.stamps.iter().map(Look::of)
    }
}

impl<P: Rights> Dir<P> {
    /// What each of `paths` beneath this directory is now, in the order given, from ONE
    /// effect ([`Effect::Stamp`]) whatever their number: a path with nothing there is
    /// [`EntryKind::Gone`], a directory [`EntryKind::Dir`], a symbolic link is looked at
    /// itself, never followed. A directory that is not there makes every path gone; nothing
    /// is asked of it first. No path is given: no effect. More than blaze-core's
    /// `STAMP_MOST` (2^20) paths are refused as a whole by the driver, before any is looked
    /// at, naming the cap. A read: any rights may ask it.
    pub fn looks<'p>(&self, cx: &Cx, paths: impl IntoIterator<Item = &'p Path>) -> Result<Looks, FsError> {
        let mut names = Names::default();
        match &self.base {
            DirName::Root => {
                for path in paths {
                    names.push(path.name());
                }
            }
            DirName::Named(base) => {
                // The joined name of each path, in one buffer of the task's frame reused for
                // every path: no `format!` and no `FileName` made a path, as `beneath` makes.
                let mut joined = Bytes::new_in(cx.frame(), "bstd::fs looks: a name joined beneath its directory");
                let stopped = |stopped| FsError::new(FsOp::Stat, &self.base, FsKind::Stopped(stopped));
                for path in paths {
                    joined.truncate(0);
                    for part in [base.as_str().as_bytes(), b"/".as_slice(), path.as_str().as_bytes()] {
                        joined.try_extend(part).map_err(stopped)?;
                    }
                    // Two names' texts joined by `/`: UTF-8, and a name (as `beneath` takes it).
                    let text = core::str::from_utf8(joined.as_slice()).expect("two names joined by a slash are UTF-8");
                    names.push_checked(text);
                }
            }
        }
        let asked = names.len();
        if asked == 0 {
            return Ok(Looks { stamps: Vec::new() });
        }
        // alloc: the column, shared with the effect that carries it to the driver.
        let files = Arc::new(names);
        match call(&self.io, cx, FsOp::Stat, &self.base, |tag| Effect::Stamp { files, tag })? {
            Done::Stamped(stamps) if stamps.len() == asked => Ok(Looks { stamps }),
            done => Err(unexpected(FsOp::Stat, &self.base, done)),
        }
    }
}

/// What a column of [`Facts`] says it is when the caller's frame has no room for it.
const COLUMNS: &str = "bstd::fs facts_of: a column in the caller's frame";
/// What the buffer a name is joined in says when the caller's frame has no room for it.
const JOINED: &str = "bstd::fs facts_of: a name joined beneath its directory";
/// A batch entry that is a listed folder's own name, not an asked path.
const FOLDER: u32 = u32::MAX;

/// How [`Dir::facts_of_by`] picks, per folder, between listing it whole and stamping its asked
/// paths by name: the one seam (rule 78), each instantiation tested equal (tests/looks.rs).
pub trait Strategy {
    /// A folder with at least this many paths asked is listed whole; `usize::MAX`: never.
    const BULK_AT: usize;
    /// The planted bug of the merge test: the last asked name of a listed folder is skipped.
    #[cfg(test)]
    const SKIPS_LAST: bool = false;
}

/// The count of this platform ([`Dir::facts_of_by`]'s table): 8 on macOS, never elsewhere.
pub enum ByCount {}

/// On macOS a listing of a folder of up to 500 entries is 5 calls (`open`, one
/// `getattrlistbulk`, the empty one that ends it, `close`, and the folder's own `fstatat` in
/// the stamp batch), no more than the stamps of 5 names; 8 leaves room for decoding entries
/// nobody asked about and keeps a folder of up to 1,500 entries (7 calls) no worse.
#[cfg(target_os = "macos")]
const BULK_AT_BY_COUNT: usize = 8;
/// Elsewhere a listing stamps every entry one call each (`statx` an entry), so it never takes
/// fewer calls than stamping the asked names: never listed.
#[cfg(not(target_os = "macos"))]
const BULK_AT_BY_COUNT: usize = usize::MAX;

impl Strategy for ByCount {
    const BULK_AT: usize = BULK_AT_BY_COUNT;
}

/// Every folder listed whole, whatever is asked of it.
pub enum Whole {}

impl Strategy for Whole {
    const BULK_AT: usize = 1;
}

/// Every path stamped by name; no folder listed.
pub enum EachPath {}

impl Strategy for EachPath {
    const BULK_AT: usize = usize::MAX;
}

/// [`Whole`] with the planted bug: the merge skips the last asked name of each folder.
#[cfg(test)]
pub(crate) enum SkipsLast {}

#[cfg(test)]
impl Strategy for SkipsLast {
    const BULK_AT: usize = 1;
    const SKIPS_LAST: bool = true;
}

#[cfg(test)]
fn skips_last<S: Strategy>() -> bool {
    S::SKIPS_LAST
}

#[cfg(not(test))]
fn skips_last<S: Strategy>() -> bool {
    false
}

/// Why [`Dir::facts_of`] answered nothing.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub enum FactsError {
    /// Path `at` is not after path `at - 1` by bytes: the paths are asked sorted, each once.
    /// The caller sorts; nothing is reordered here.
    Unsorted { at: usize, before: Path, after: Path },
    /// More paths than one stamp batch takes (blaze-core's `STAMP_MOST`).
    TooMany { asked: usize, most: usize },
    /// A column in the caller's frame had no room: [`Stopped::OutOfMemory`] naming it.
    OutOfMemory(Stopped),
    /// The driver failed the stamp batch, answered with another kind, or a wait stopped.
    Fs(FsError),
}

impl fmt::Display for FactsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FactsError::Unsorted { at, before, after } => write!(
                f,
                "bstd::fs facts_of: path {at} `{after}` is not after path {} `{before}` by bytes: the paths are asked sorted and each once (the caller sorts; nothing is reordered)",
                at - 1
            ),
            FactsError::TooMany { asked, most } => {
                write!(f, "bstd::fs facts_of of {asked} paths: at most {most} (blaze-core's STAMP_MOST) in one ask; split the column")
            }
            FactsError::OutOfMemory(stopped) => write!(f, "bstd::fs facts_of: {stopped}"),
            FactsError::Fs(error) => write!(f, "bstd::fs facts_of: {error}"),
        }
    }
}

impl core::error::Error for FactsError {}

/// The looks of the paths asked, one row a path in the order asked, as columns in the
/// caller's frame ([`Dir::facts_of`]).
pub struct Facts<'f> {
    stamps: OwnVec<Stamped, &'f Frame>,
    /// Each failed row and the index of its folder's error in `errors`, sorted by row.
    failed: OwnVec<(u32, u32), &'f Frame>,
    errors: OwnVec<FsError, &'f Frame>,
    listed: u32,
    stamped: u32,
}

impl Facts<'_> {
    pub fn len(&self) -> usize {
        self.stamps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.stamps.is_empty()
    }

    /// The look of the `i`th path asked.
    pub fn get(&self, i: usize) -> Option<Look> {
        self.stamps.get(i).map(Look::of)
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = Look> + '_ {
        self.stamps.iter().map(Look::of)
    }

    /// Why the `i`th path's look is [`EntryKind::Unknown`] when its folder's listing failed:
    /// the error, naming the folder; `None` for every answered row.
    pub fn failure(&self, i: usize) -> Option<&FsError> {
        let row = u32::try_from(i).ok()?;
        let at = self.failed.binary_search_by_key(&row, |(row, _)| *row).ok()?;
        self.errors.get(self.failed[at].1 as usize)
    }

    /// Folders listed whole by this call: one [`Effect::Entries`] each.
    pub fn listed(&self) -> u32 {
        self.listed
    }

    /// Names in this call's one [`Effect::Stamp`] (the paths stamped, and each listed folder's
    /// own name): one stat-like system call each.
    pub fn stamped(&self) -> u32 {
        self.stamped
    }
}

impl fmt::Debug for Facts<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Facts")
            .field("rows", &self.stamps.len())
            .field("failed", &self.failed.len())
            .field("listed", &self.listed)
            .field("stamped", &self.stamped)
            .finish()
    }
}

/// One folder's paths: `from..to` of the rows ordered by folder.
struct Group {
    from: u32,
    to: u32,
    /// The batch index of the folder's own name, or [`FOLDER`] when it is not stamped (the
    /// directory asked of itself, or a folder not listed).
    folder_at: u32,
    /// The folder to list, when it is listed.
    dir: Option<DirName>,
    tag: Option<Tag>,
}

impl<P: Rights> Dir<P> {
    /// [`Dir::looks`] of `paths`, sorted by their bytes and each once, one row a path in the
    /// order given, as columns in `frame`, each folder listed or stamped by this platform's
    /// count ([`ByCount`]). The first pair out of order is [`FactsError::Unsorted`]: nothing is
    /// reordered. A read: any rights may ask it.
    pub fn facts_of<'f>(&self, cx: &Cx, paths: &[Path], frame: &'f Frame) -> Result<Facts<'f>, FactsError> {
        self.facts_of_by::<ByCount>(cx, paths, frame)
    }

    /// [`Dir::facts_of`] with the strategy `S`: every `S` gives equal rows of one world.
    ///
    /// The paths are grouped by folder (the text before the last `/`). A folder with at least
    /// `S::BULK_AT` paths asked is listed whole and its asked names picked out by a merge of
    /// two sorted lists; every other path goes into ONE [`Effect::Stamp`] for the whole call,
    /// with each listed folder's own name (a listing that fails of a folder that is gone, or
    /// is a file, leaves its rows gone, as a stat of each says). Every effect is handed to the
    /// driver before the first is waited for.
    ///
    /// | strategy | macOS (kqueue driver) | Linux (io_uring driver) |
    /// |---|---|---|
    /// | listed, per folder | `open` + one `getattrlistbulk` a 64 KiB buffer (about 500 entries) + the empty one that ends it + `close`, and the folder's own `fstatat` in the batch: 5 calls up to 500 entries, 7 for 1,000 | `open` + `getdents64` a buffer + one `statx` an ENTRY + `close` |
    /// | stamped, per path | one `fstatat` relative to the root's descriptor, no `open` | one `IORING_OP_STATX`, the column in one submission |
    ///
    /// A fraction of a folder's entries cannot be the rule: how many entries a folder has is
    /// known only once it is listed, and no record keeps it. Each wait is [`Cx::wait`], the
    /// one wait (the task's deadline and span); a stop forgets every effect not yet waited
    /// for, so nothing of this call lands after it returns.
    pub fn facts_of_by<'f, S: Strategy>(&self, cx: &Cx, paths: &[Path], frame: &'f Frame) -> Result<Facts<'f>, FactsError> {
        if paths.len() > STAMP_MOST {
            return Err(FactsError::TooMany { asked: paths.len(), most: STAMP_MOST });
        }
        for (at, pair) in paths.windows(2).enumerate() {
            if pair[0].as_str().as_bytes() >= pair[1].as_str().as_bytes() {
                // clone: the error's paths, refcounts.
                return Err(FactsError::Unsorted { at: at + 1, before: pair[0].clone(), after: pair[1].clone() });
            }
        }
        let n = paths.len();
        let mut facts = Facts {
            stamps: OwnVec::new_in(frame),
            failed: OwnVec::new_in(frame),
            errors: OwnVec::new_in(frame),
            listed: 0,
            stamped: 0,
        };
        room(&mut facts.stamps, n)?;
        for _ in 0..n {
            facts.stamps.push(Stamped::GONE);
        }
        if n == 0 {
            return Ok(facts);
        }

        // The rows by folder, each folder's in the order asked (so by their names' bytes: one
        // folder's paths share the text before the name). At most STAMP_MOST rows: a `u32`.
        let mut order: OwnVec<u32, &Frame> = OwnVec::new_in(frame);
        room(&mut order, n)?;
        for row in 0..n {
            order.push(row as u32);
        }
        order.sort_unstable_by(|a, b| {
            folder_of(&paths[*a as usize]).cmp(folder_of(&paths[*b as usize])).then(a.cmp(b))
        });
        let mut groups: OwnVec<Group, &Frame> = OwnVec::new_in(frame);
        let mut from = 0;
        while from < n {
            let folder = folder_of(&paths[order[from] as usize]);
            let mut to = from + 1;
            while to < n && folder_of(&paths[order[to] as usize]) == folder {
                to += 1;
            }
            room(&mut groups, 1)?;
            groups.push(Group { from: from as u32, to: to as u32, folder_at: FOLDER, dir: None, tag: None });
            from = to;
        }

        // The one stamp batch: each stamped path, and each listed folder's own name (whether it
        // is there decides what a failed listing means). `batch[k]` is the row of name `k`.
        let mut names = Names::default();
        let mut batch: OwnVec<u32, &Frame> = OwnVec::new_in(frame);
        let mut joined = Bytes::new_in(frame, JOINED);
        for group in groups.iter_mut() {
            let rows = &order[group.from as usize..group.to as usize];
            let folder = folder_of(&paths[rows[0] as usize]);
            if rows.len() >= S::BULK_AT {
                if folder.is_empty() {
                    // clone: the directory's own name, a refcount.
                    group.dir = Some(self.base.clone());
                    continue;
                }
                let text = beneath(&self.base, &mut joined, folder).map_err(FactsError::OutOfMemory)?;
                names.push_checked(text);
                let name = FileName::new(text).expect("a directory's name and a path's folder joined are a name");
                group.dir = Some(DirName::Named(name));
                room(&mut batch, 1)?;
                group.folder_at = batch.len() as u32;
                batch.push(FOLDER);
                continue;
            }
            room(&mut batch, rows.len())?;
            for &row in rows {
                names.push_checked(beneath(&self.base, &mut joined, paths[row as usize].as_str()).map_err(FactsError::OutOfMemory)?);
                batch.push(row);
            }
        }

        // Every effect handed to the driver, then each waited for.
        let stop = |op, stopped| FactsError::Fs(FsError::new(op, &self.base, FsKind::Stopped(stopped)));
        cx.stopped_by(self.io.now()).map_err(|stopped| stop(FsOp::Stat, stopped))?;
        let asked = batch.len();
        facts.stamped = asked as u32;
        let stamp_tag = match asked {
            0 => None,
            _ => {
                // alloc: the column, shared with the effect that carries it to the driver.
                let files = Arc::new(names);
                Some(self.io.submit(|tag| Effect::Stamp { files, tag }))
            }
        };
        for group in groups.iter_mut() {
            if let Some(dir) = &group.dir {
                // clone: the effect's folder, a refcount.
                let dir = dir.clone();
                group.tag = Some(self.io.submit(|tag| Effect::Entries { dir, tag }));
                facts.listed += 1;
            }
        }

        let mut failed: Option<FactsError> = None;
        let stamps = match stamp_tag {
            None => None,
            Some(tag) => match cx.wait(&self.io, tag, Ends::Once) {
                Ok(Done::Stamped(stamps)) if stamps.len() == asked => Some(stamps),
                Ok(done) => {
                    failed = Some(FactsError::Fs(unexpected(FsOp::Stat, &self.base, done)));
                    None
                }
                Err(stopped) => {
                    failed = Some(stop(FsOp::Stat, stopped));
                    None
                }
            },
        };
        if let Some(stamps) = &stamps {
            for (at, &row) in batch.iter().enumerate() {
                if row != FOLDER {
                    facts.stamps[row as usize] = stamps[at];
                }
            }
        }
        for group in groups.iter() {
            let (Some(tag), Some(dir)) = (group.tag, &group.dir) else { continue };
            if failed.is_some() {
                self.io.forget(tag, Ends::Once);
                continue;
            }
            let rows = &order[group.from as usize..group.to as usize];
            match cx.wait(&self.io, tag, Ends::Once) {
                Ok(Done::Entries(listing)) if listing.dir() == dir => {
                    merge::<S>(&mut facts.stamps, paths, rows, &listing);
                }
                Ok(Done::Failed(error)) => {
                    let own = match (group.folder_at, &stamps) {
                        (FOLDER, _) | (_, None) => None,
                        (at, Some(stamps)) => Some(stamps[at as usize]),
                    };
                    // The folder is not there (or a file is): nothing beneath it is, so its
                    // rows stay gone, as a stat of each says.
                    let gone = own.is_some_and(|own| matches!(EntryKind::of(&own), EntryKind::Gone | EntryKind::File));
                    if !gone {
                        let error = FsError::new(FsOp::Entries, dir, FsKind::Driver(error.0));
                        if let Err(error) = fail_rows(&mut facts, rows, error) {
                            failed = Some(error);
                        }
                    }
                }
                Ok(done) => failed = Some(FactsError::Fs(unexpected(FsOp::Entries, dir, done))),
                Err(stopped) => failed = Some(FactsError::Fs(FsError::new(FsOp::Entries, dir, FsKind::Stopped(stopped)))),
            }
        }
        if let Some(failed) = failed {
            return Err(failed);
        }
        facts.failed.sort_unstable_by_key(|(row, _)| *row);
        Ok(facts)
    }
}

/// Room for `more` values in `column`, or [`FactsError::OutOfMemory`] naming the column.
fn room<T>(column: &mut OwnVec<T, &Frame>, more: usize) -> Result<(), FactsError> {
    column
        .try_reserve(more)
        .map_err(|_| FactsError::OutOfMemory(Stopped::OutOfMemory { arena: COLUMNS, asked: more.saturating_mul(size_of::<T>()) }))
}

/// The text of `path` beneath `base`, joined in `joined` (the task's frame, reused for every
/// name), as [`Dir::looks`] joins.
fn beneath<'b>(base: &DirName, joined: &'b mut Bytes<'_>, path: &'b str) -> Result<&'b str, Stopped> {
    match base {
        DirName::Root => Ok(path),
        DirName::Named(base) => {
            joined.truncate(0);
            for part in [base.as_str().as_bytes(), b"/".as_slice(), path.as_bytes()] {
                joined.try_extend(part)?;
            }
            // Two names' texts joined by `/`: UTF-8, and a name.
            Ok(core::str::from_utf8(joined.as_slice()).expect("two names joined by a slash are UTF-8"))
        }
    }
}

/// The folder of `path`: the text before its last `/`, or empty for a name directly here.
fn folder_of(path: &Path) -> &str {
    path.as_str().rsplit_once('/').map_or("", |(folder, _)| folder)
}

/// The name of `path` in its folder: the text after its last `/`.
fn leaf_of(path: &Path) -> &str {
    path.as_str().rsplit_once('/').map_or(path.as_str(), |(_, leaf)| leaf)
}

/// The asked `rows` of one folder (in their names' order) given their stamps from `listing`
/// of that folder (sorted by the names' bytes): one pass over both. A name not listed keeps
/// its gone stamp.
fn merge<S: Strategy>(stamps: &mut [Stamped], paths: &[Path], rows: &[u32], listing: &Listing) {
    let last = rows.len().saturating_sub(1);
    let (mut i, mut j) = (0, 0);
    while i < rows.len() && j < listing.len() {
        let row = rows[i] as usize;
        match leaf_of(&paths[row]).as_bytes().cmp(listing.leaf(j).as_str().as_bytes()) {
            Ordering::Less => i += 1,
            Ordering::Greater => j += 1,
            Ordering::Equal => {
                if !(skips_last::<S>() && i == last) {
                    stamps[row] = listing.stamp(j);
                }
                i += 1;
                j += 1;
            }
        }
    }
}

/// `rows` failed with their folder's `error`: each [`EntryKind::Unknown`], naming it.
fn fail_rows(facts: &mut Facts<'_>, rows: &[u32], error: FsError) -> Result<(), FactsError> {
    room(&mut facts.errors, 1)?;
    room(&mut facts.failed, rows.len())?;
    let index = facts.errors.len() as u32;
    facts.errors.push(error);
    for &row in rows {
        facts.stamps[row as usize] = Stamped::UNKNOWN;
        facts.failed.push((row, index));
    }
    Ok(())
}
