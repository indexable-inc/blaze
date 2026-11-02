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
//! write it, with the database's `&mut`, so no query runs while it changes.

use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
};

use blaze_ids::{ContentId, Hasher};
use blaze_mem::{Frame, scratch};
use blaze_query::db::{Durability, Setter};
use crate::{FileId, VfsPath};

use crate::{
    Db, InputId, SourceRoot, content,
    table::{Table, hash_of},
};

pub(crate) const BUCKETS: usize = 64;

/// A bucket's content ID is of the paths named in it: how many (8 bytes, little-endian), the
/// lane-wise wrapping sum of their [`SourceRoot::PATH_DOMAIN`] IDs as four little-endian
/// `u64`s, then how many times one of them was given another file (8 bytes). A function of the
/// set alone, in any order it was named, while no path moves, kept in `O(1)` a path; a move
/// changes it, so a lookup that found the path's old file out of its root runs again.
pub const NAMED_PATHS_DOMAIN: &str = "blaze v2 named paths bucket 2";

/// The index's handles, set once when the first source root is set and never again: what it
/// holds is in its buckets' content IDs.
#[blaze_query::input(singleton, debug)]
pub(crate) struct NamedPaths {
    #[returns(ref)]
    table: PathTable,
    #[returns(ref)]
    buckets: Box<[PathBucket]>,
}

/// The paths of one of [`BUCKETS`], by their [`hash_of`]: see [`NAMED_PATHS_DOMAIN`].
#[blaze_query::input(debug)]
pub(crate) struct PathBucket {
    #[returns(copy)]
    content: InputId,
}

#[derive(Clone)]
pub(crate) struct PathTable(Arc<Table<NamedPath>>);

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

fn bucket_of(hash: u64) -> usize {
    // `Table` picks a shard by the top bits and a place by the low ones.
    (hash >> 52) as usize % BUCKETS
}

/// The setters' side of the index, in [`Writer`](crate::files::Writer).
pub(crate) struct Named {
    table: Arc<Table<NamedPath>>,
    input: Option<NamedPaths>,
    counts: [u64; BUCKETS],
    sums: [[u64; 4]; BUCKETS],
    moves: [u64; BUCKETS],
    /// Buckets a path of moved in since the last [`Named::publish`], a bit each.
    moved: u64,
    /// Paths named since the last [`Named::publish`], as their [`hash_of`] and place in the
    /// table: hashed for the buckets in one batch there.
    pending: Vec<(u64, u32)>,
}

impl Default for Named {
    fn default() -> Named {
        Named {
            // alloc: once per database, when its `Files` is made.
            table: Arc::new(Table::new(1 << 22)),
            input: None,
            counts: [0; BUCKETS],
            sums: [[0; 4]; BUCKETS],
            moves: [0; BUCKETS],
            moved: 0,
            pending: Vec::new(), // alloc: empty until a setter names a path; reused after.
        }
    }
}

impl Named {
    /// A source root names `file` at `path`: a new path, or one given another file.
    pub(crate) fn note(&mut self, path: &VfsPath, file: FileId) {
        let hash = hash_of(path);
        // clone: the table's one copy of a path, made once when a source root first names it.
        let make = || NamedPath { path: path.clone(), file: AtomicU32::new(file.index()) };
        let (at, inserted) = self.table.insert(hash, |it| it.path == *path, make);
        if inserted {
            self.pending.push((hash, at));
        } else if self.table.get(at).file.swap(file.index(), Ordering::Release) != file.index() {
            let bucket = bucket_of(hash);
            self.moves[bucket] += 1;
            self.moved |= 1 << bucket;
        }
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

    /// Writes the content IDs of the buckets that gained paths or had one move, making the
    /// inputs the first time. Called by every setter that sets a source root's files, after
    /// it has.
    pub(crate) fn publish(&mut self, db: &mut Db, frame: &Frame) {
        if self.input.is_some() && self.pending.is_empty() && self.moved == 0 {
            return;
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
        for ((hash, _), id) in self.pending.iter().zip(ids) {
            let bucket = bucket_of(*hash);
            self.counts[bucket] += 1;
            for (lane, sum) in self.sums[bucket].iter_mut().enumerate() {
                let word: [u8; 8] = id.0[lane * 8..lane * 8 + 8].try_into().expect("8 bytes");
                *sum = sum.wrapping_add(u64::from_le_bytes(word));
            }
            dirty |= 1 << bucket;
        }
        self.pending.clear();
        let Some(input) = self.input else {
            // alloc: the 64 bucket inputs, made once per database.
            let buckets: Box<[PathBucket]> = (0..BUCKETS)
                .map(|bucket| {
                    PathBucket::builder(InputId(self.content(bucket)))
                        .durability(Durability::MEDIUM)
                        .new(&*db)
                })
                .collect(); // alloc: as above.
            let table = PathTable(self.table.clone()); // clone: a refcount, once per database.
            self.input =
                Some(NamedPaths::builder(table, buckets).durability(Durability::HIGH).new(db));
            return;
        };
        while dirty != 0 {
            let bucket = dirty.trailing_zeros() as usize;
            dirty &= dirty - 1;
            let content = InputId(self.content(bucket));
            let input = input.buckets(&*db)[bucket];
            if input.content(&*db) != content {
                input.set_content(db).with_durability(Durability::MEDIUM).to(content);
            }
        }
    }
}

/// The file a source root last named at `path`, found through the index: see the module. The
/// caller reads that file's source root and keeps the file only if the root still names it
/// there, which is the read a hit reruns on.
pub(crate) fn lookup(db: &Db, path: &VfsPath) -> Option<FileId> {
    let named = NamedPaths::try_get(db)?;
    let hash = hash_of(path);
    // The read a miss reruns on.
    let _ = named.buckets(db)[bucket_of(hash)].content(db);
    let table = &named.table(db).0;
    let at = table.find(hash, |it| it.path == *path)?;
    Some(FileId::from_raw(table.get(at).file.load(Ordering::Acquire)))
}
