//! Every path any source root has named, across source roots: where `Db::resolve_path` looks
//! when the anchor's own source root lacks a path. rustc reads a `#[path]` module or an `include!` anywhere on disk, while v2 gives each
//! package its own source root, so a member's `#[path = "../shared/m.rs"]` names a file of
//! another root.
//!
//! A lookup is tracked as reads of inputs, never a quiet miss and never a look at the disk:
//! - the [`PathBucket`] of the path, whose content ID is of the set of paths named in it, so a
//!   lookup that found nothing runs again once a source root names the path;
//! - on a hit, the file's [`FileSourceRootInput`](crate::FileSourceRootInput) and its source
//!   root's file set, so an edit of that root (the file taken out, or given another path)
//!   reruns the reader; the file's text is read by the reader itself.
//!
//! The table behind it is [`Table`]: a lookup is acquire loads, with no lock and nothing
//! written (docs/v2/plan/design/concurrency.md). Only the setters of [`Files`](crate::Files)
//! write it, in an edit.
//!
//! The inputs (the singleton and its 64 buckets) are the session arena's, made with it
//! ([`Named::make`]); a setter whose paths moved a bucket marks the session
//! ([`Named::publish`]), whose arena is then made again. Cost (rule 67): a path named is a
//! table insert and an `O(1)` sum; a bucket that moved drops the session arena, where the floor
//! is its readers alone, said plainly: a new path is rare next to an edit of a file's text,
//! which moves no bucket.
//!
//! The table is shared by every snapshot and written in place: a reader of an older snapshot
//! that looks a path up sees the file the newest setter gave it, and the bucket its snapshot
//! has. The bucket a reader reads is its snapshot's, so a lookup that found nothing still
//! reruns at the next snapshot whose bucket moved.

use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
};

use crate::{FileId, VfsPath};
use blaze_ids::{ContentId, Hasher};
use blaze_memo::{Fact, Inputs};
use blaze_mem::{Frame, scratch};
use std::alloc::Global;

use crate::{Db, InputId, SourceRoot, content, table::Table, world::facts};

/// The domain of a bucket's fact id: of its number.
const BUCKET_FACT_DOMAIN: &str = "blaze v2 named paths bucket fact 1";

pub(crate) const BUCKETS: usize = 64;

/// A bucket's content ID is of the paths named in it: how many (8 bytes, little-endian), the
/// lane-wise wrapping sum of their [`SourceRoot::PATH_DOMAIN`] IDs as four little-endian
/// `u64`s, then how many times one of them was given another file (8 bytes). A function of the
/// set alone, in any order it was named, while no path moves, kept in `O(1)` a path; a move
/// changes it, so a lookup that found the path's old file out of its root runs again.
pub const NAMED_PATHS_DOMAIN: &str = "blaze v2 named paths bucket 2";

/// The index's handles, made with the session arena: what it holds is in its buckets' content
/// IDs.
#[blaze_memo::input(singleton, debug)]
pub(crate) struct NamedPaths {
    #[returns(ref)]
    table: PathTable,
    #[returns(ref)]
    buckets: Box<[PathBucket]>,
}

/// The paths of one of [`BUCKETS`], by [`bucket_of`]: see [`NAMED_PATHS_DOMAIN`].
#[blaze_memo::input(debug)]
pub(crate) struct PathBucket {
    #[returns(copy)]
    content: InputId,
}

#[derive(Clone)]
pub(crate) struct PathTable(Arc<Table<NamedPath>>);

/// The same table, by its handle: what an input's setter compares to write nothing for an
/// equal value. What the table holds is not compared (it is in its buckets' content IDs).
impl PartialEq for PathTable {
    fn eq(&self, other: &PathTable) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl fmt::Debug for PathTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PathTable").finish_non_exhaustive()
    }
}

struct NamedPath {
    path: VfsPath,
    /// The file the path names, [`FileId::index`]: a path given another file keeps its entry.
    file: AtomicU32,
}

/// The bucket of `path`: by foldhash's fixed hasher, not by the table's keyed hash, as a
/// bucket's content is an input (the same paths must make the same buckets in every process).
/// A source that picks paths with one fixed hash only fills one bucket, which costs a sum
/// each, never a probe.
fn bucket_of(path: &VfsPath) -> usize {
    use std::hash::BuildHasher;
    (blaze_hash::FixedState.hash_one(path) >> 52) as usize % BUCKETS
}

/// The setters' side of the index, in [`Writer`](crate::files::Writer).
pub(crate) struct Named {
    table: Arc<Table<NamedPath>>,
    counts: [u64; BUCKETS],
    sums: [[u64; 4]; BUCKETS],
    moves: [u64; BUCKETS],
    /// Buckets a path of moved in since the last [`Named::publish`], a bit each.
    moved: u64,
    /// Paths named since the last [`Named::publish`], as their [`bucket_of`] and place in the
    /// table: hashed for the buckets in one batch there.
    pending: Vec<(usize, u32), Global>,
    /// The sums as the open edit found them ([`Named::begin`]): put back by
    /// [`Named::rollback`].
    begun: Sums,
    /// The open edit's writes of the table: a row it made, or a file it gave a row (with the
    /// file the row had).
    undo: Vec<NamedUndo, Global>,
    /// Rows made by an edit that was dropped: the table only grows, so they stay, counted in
    /// no bucket. A setter that names one's path again counts it then, as a new path
    /// ([`Named::note`]). A reader that finds one keeps its file only if its root names the
    /// file there, which no committed root does.
    ghosts: blaze_hash::HashSet<u32, Global>,
}

/// What the buckets' content IDs are made of.
#[derive(Clone, Copy)]
struct Sums {
    counts: [u64; BUCKETS],
    sums: [[u64; 4]; BUCKETS],
    moves: [u64; BUCKETS],
    moved: u64,
}

/// A write of [`Named`]'s table: see [`Named::rollback`].
enum NamedUndo {
    /// The row was made, or a ghost's path named again.
    Made(u32),
    /// The row was given another file: the one it had.
    Given(u32, u32),
}

impl Default for Named {
    fn default() -> Named {
        Named {
            // Once per database, when its `Files` is made.
            table: Arc::new_in(Table::new(1 << 22), Global),
            counts: [0; BUCKETS],
            sums: [[0; 4]; BUCKETS],
            moves: [0; BUCKETS],
            moved: 0,
            // Empty until a setter names a path; reused after.
            pending: Vec::new_in(Global),
            begun: Sums { counts: [0; BUCKETS], sums: [[0; 4]; BUCKETS], moves: [0; BUCKETS], moved: 0 },
            undo: Vec::new_in(Global),
            ghosts: blaze_hash::HashSet::new_in(Global),
        }
    }
}

impl Named {
    /// A source root names `file` at `path`: a new path, or one given another file.
    pub(crate) fn note(&mut self, path: &VfsPath, file: FileId) {
        let hash = self.table.hash(path);
        // clone: the table's one copy of a path, made once when a source root first names it.
        let make = || NamedPath { path: path.clone(), file: AtomicU32::new(file.index()) };
        let (at, inserted) = self.table.insert(hash, |it| it.path == *path, make);
        if inserted || self.ghosts.remove(&at) {
            if !inserted {
                let was = self.table.get(at).file.swap(file.index(), Ordering::Release);
                self.undo.push(NamedUndo::Given(at, was));
            }
            self.undo.push(NamedUndo::Made(at));
            self.pending.push((bucket_of(path), at));
            return;
        }
        let was = self.table.get(at).file.swap(file.index(), Ordering::Release);
        if was != file.index() {
            self.undo.push(NamedUndo::Given(at, was));
            let bucket = bucket_of(path);
            self.moves[bucket] += 1;
            self.moved |= 1 << bucket;
        }
    }

    /// Begins an edit: the sums kept for [`Named::rollback`]. A copy of 3 KiB.
    pub(crate) fn begin(&mut self) {
        self.begun = Sums { counts: self.counts, sums: self.sums, moves: self.moves, moved: self.moved };
    }

    /// Ends the open edit, its writes kept.
    pub(crate) fn forget(&mut self) {
        self.undo.clear();
    }

    /// Puts back what the open edit wrote: each row it gave a file has the one it had, each
    /// row it made is a ghost, and the sums are as it found them. O(the edit's paths).
    pub(crate) fn rollback(&mut self) {
        while let Some(undo) = self.undo.pop() {
            match undo {
                NamedUndo::Made(at) => _ = self.ghosts.insert(at),
                NamedUndo::Given(at, was) => self.table.get(at).file.store(was, Ordering::Release),
            }
        }
        let Sums { counts, sums, moves, moved } = self.begun;
        (self.counts, self.sums, self.moves, self.moved) = (counts, sums, moves, moved);
        self.pending.clear();
    }

    fn content(&self, bucket: usize) -> ContentId {
        let mut hasher = Hasher::new(NAMED_PATHS_DOMAIN);
        hasher.update(&self.counts[bucket].to_le_bytes());
        for lane in self.sums[bucket] {
            hasher.update(&lane.to_le_bytes());
        }
        hasher.update(&self.moves[bucket].to_le_bytes());
        hasher.finish()
    }

    /// Brings the content IDs of the buckets that gained paths or had one move up to date, and
    /// says whether any did: then the session's inputs are to be made again ([`Named::make`]).
    /// Called by every setter that sets a source root's files, after it has.
    pub(crate) fn publish(&mut self, frame: &Frame) -> bool {
        if self.pending.is_empty() && self.moved == 0 {
            return false;
        }
        let table = &self.table;
        let mut bytes = scratch::lend_bytes();
        let mut ranges: Vec<std::ops::Range<usize>, &Frame> =
            Vec::with_capacity_in(self.pending.len(), frame);
        for &(_, at) in &self.pending {
            let start = bytes.len();
            content::path_key_into(&table.get(at).path, &mut bytes);
            ranges.push(start..bytes.len());
        }
        let ids = content::hash_ranges(SourceRoot::PATH_DOMAIN, &bytes, &ranges, frame);
        let mut dirty = std::mem::take(&mut self.moved);
        for (&(bucket, _), id) in self.pending.iter().zip(ids) {
            self.counts[bucket] += 1;
            for (lane, sum) in self.sums[bucket].iter_mut().enumerate() {
                let word: [u8; 8] = id.0[lane * 8..lane * 8 + 8].try_into().expect("8 bytes");
                *sum = sum.wrapping_add(u64::from_le_bytes(word));
            }
            dirty |= 1 << bucket;
        }
        self.pending.clear();
        dirty != 0
    }

    /// Makes the index's inputs in the session arena being made: each bucket with its content
    /// ID, and the singleton that names them and the table.
    pub(crate) fn make(&self, inputs: &Inputs<'_>) {
        let mut buckets = Vec::with_capacity_in(BUCKETS, Global);
        buckets.extend((0..BUCKETS).map(|bucket| {
            let content = self.content(bucket);
            let fact = Fact {
                id: ContentId::of(BUCKET_FACT_DOMAIN, &(bucket as u32).to_le_bytes()),
                digest: content,
            };
            PathBucket::new(inputs, fact, InputId(content))
        }));
        // The handles hold nothing a reader keys on but through the buckets: the singleton's
        // digest is of its domain alone.
        let fact = facts::singleton(NAMED_PATHS_DOMAIN, InputId(ContentId::of(NAMED_PATHS_DOMAIN, b"")));
        let table = PathTable(Arc::clone(&self.table));
        NamedPaths::set(inputs, fact, table, buckets.into_boxed_slice());
    }
}

/// The file a source root last named at `path`, found through the index: see the module. The
/// caller reads that file's source root and keeps the file only if the root still names it
/// there, which is the read a hit reruns on.
pub(crate) fn lookup(db: &Db, path: &VfsPath) -> Option<FileId> {
    let named = NamedPaths::try_get(db)?;
    // The read a miss reruns on.
    let _ = named.buckets(db)[bucket_of(path)].content(db);
    let table = &named.table(db).0;
    let at = table.find(table.hash(path), |it| it.path == *path)?;
    Some(FileId::from_raw(table.get(at).file.load(Ordering::Acquire)))
}
