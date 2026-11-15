//! The leaf memory: what the command knows of the checkout's files between runs, and what it
//! hands the host. blaze-solo's leaf table (`blaze_solo::table`: sorted rows of path, stamp,
//! content id or not hashed, kind) is the one kept, and its sans-IO sweep
//! (`blaze_solo::sweep::Sweep`) is how it is brought up to date; this module is the thin
//! driver of that sweep on the disk (stats only, no watcher) and what is made of its rows:
//!
//! - the request's reads: every file row the front end may read ([`is_read`]: a `.rs` file),
//!   in path order, each with the content id it has now: [`Reads`];
//! - the host's leaf memory ([`blaze_host::outside::Leaves`]): the same rows with their stamps,
//!   so the host's answerer vouches for a file by one `lstat` and reads none;
//! - after the request, the files the host hashed ([`blaze_host::outside::Looked`]) folded
//!   back in ([`absorb`]) and the table written for the next command ([`save`]).
//!
//! The invariant: a read's answer in [`Reads::then`] is the content id of the file's bytes as
//! they were at a moment no earlier than the sweep's instant; a row's id is trusted only under
//! the racy rule (`blaze_vfs::racy`, through the sweep), and a row whose stamp moved is hashed
//! again before it is a read. A table that does not decode is refused by name, never taken as
//! empty; only a table that is not there is the first run's empty one.
//!
//! Floors (rule 67). A no-op's floor is one `lstat` per file the request read, plus a listing
//! of each directory whose stamp moved, plus the certificate's lookup. This costs one `lstat`
//! per row of the table (every file and directory of the checkout's view, the sweep) and then
//! one more per read (the host's answerer): about `(rows + reads) / reads` times the floor's
//! stats, 2x for a checkout of source files only, more for one with many files no crate
//! reads. No file is read on a no-op. An edit costs, beyond the floor, nothing: the edited
//! file is hashed once here (the host then vouches for it by its stamp). Writing the table
//! back costs one file write per command whose rows changed, none on a no-op whose rows held.
//! What would reach the floor: the host vouching from the sweep's stamps without its own
//! `lstat` (a leaf memory stamped in this process), or a sweep of directories alone.

use std::{
    alloc::{Allocator, Global},
    fmt,
    io::Read as _,
    os::unix::ffi::OsStrExt as _,
    path::{Path, PathBuf},
};

use blaze_core::Stamp;
use blaze_host::{
    disk::Disk,
    outside::{Ask, Files, Leaf, Leaves, Looked, file_answer},
};
use blaze_ids::ContentId;
use blaze_solo::{
    sweep::{Sweep, Swept, Walk},
    table::{Kind, Row, Table},
};

use crate::CommandError;

/// The most looks (stamps of a listing's entries, listings, probes) one sweep may take before
/// it is refused: far past any checkout's view, so it is reached only by a loop.
pub const MOST_LOOKS: u64 = 1 << 26;

/// Whether the front end may read the file at `path` (relative to the checkout): a Rust source
/// file. A crate's other reads (`include_str!` and the like) are late reads, keyed through
/// the unit's late table, not through these.
pub fn is_read(path: &str) -> bool {
    path.ends_with(".rs")
}

/// What the sweep's driver asks of the disk: stats, listings, the first bytes of a tag, a
/// file's bytes and the wall clock. The real one is [`DiskLook`]; a test may give its own.
pub trait Look {
    /// The stamp of `path` (relative, `""` for the checkout), by `lstat`; [`Stamp::GONE`] for
    /// nothing there.
    fn stamp(&self, path: &str) -> Result<Stamp, CommandError>;
    /// Each entry's name and stamp, of the directory at `path`, given to `each`.
    fn list(&self, path: &str, each: &mut dyn FnMut(&str, Stamp)) -> Result<(), CommandError>;
    /// The bytes of the file at `path`, at most `most` when given: lent from the look's one
    /// buffer, which the next read fills again.
    fn read(&mut self, path: &str, most: Option<usize>) -> Result<&[u8], CommandError>;
    /// The wall clock, in nanoseconds since the epoch: the table's instant.
    fn now_ns(&self) -> u64;
}

/// The checkout on disk.
pub struct DiskLook {
    root: PathBuf,
    disk: Disk,
    /// The one buffer every read fills: no allocation once it has grown.
    bytes: Vec<u8, Global>,
}

impl DiskLook {
    pub fn new(root: &Path) -> DiskLook {
        DiskLook { root: root.to_path_buf(), disk: Disk::new(root), bytes: Vec::new_in(Global) }
    }

    fn path(&self, path: &str) -> PathBuf {
        if path.is_empty() { self.root.clone() } else { self.root.join(path) }
    }
}

fn file_error(path: &Path, what: impl fmt::Display) -> CommandError {
    CommandError::File { path: path.display().to_string().into_boxed_str(), what: what.to_string().into_boxed_str() }
}

impl Look for DiskLook {
    fn stamp(&self, path: &str) -> Result<Stamp, CommandError> {
        self.disk
            .stamp(path.as_bytes())
            .map_err(|error| file_error(&self.path(path), format_args!("{} failed: errno {}", error.call, error.errno)))
    }

    fn list(&self, path: &str, each: &mut dyn FnMut(&str, Stamp)) -> Result<(), CommandError> {
        let dir = self.path(path);
        let entries = std::fs::read_dir(&dir).map_err(|error| file_error(&dir, error.to_string()))?;
        for entry in entries {
            let entry = entry.map_err(|error| file_error(&dir, error.to_string()))?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                return Err(file_error(&entry.path(), "a name that is not UTF-8, which a leaf table cannot hold"));
            };
            let below = if path.is_empty() { name.to_owned() } else { format!("{path}/{name}") };
            each(name, self.stamp(&below)?);
        }
        Ok(())
    }

    fn read(&mut self, path: &str, most: Option<usize>) -> Result<&[u8], CommandError> {
        match most {
            None => self
                .disk
                .read(path.as_bytes(), &mut self.bytes)
                .map_err(|error| file_error(&self.path(path), format_args!("{} failed: errno {}", error.call, error.errno)))?,
            Some(most) => {
                self.bytes.clear();
                let at = self.path(path);
                let file = std::fs::File::open(&at).map_err(|error| file_error(&at, error.to_string()))?;
                file.take(most as u64).read_to_end(&mut self.bytes).map_err(|error| file_error(&at, error.to_string()))?;
            }
        }
        Ok(&self.bytes)
    }

    #[expect(clippy::disallowed_methods, reason = "the leaf table's instant is the wall clock, the clock file stamps are on")]
    fn now_ns(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |it| u64::try_from(it.as_nanos()).unwrap_or(u64::MAX))
    }
}

/// What one sweep cost.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Cost {
    pub stated: u64,
    pub listed: u64,
    pub probed: u64,
    pub racy: u64,
    /// Files hashed: new, moved or never hashed reads.
    pub hashed: u64,
}

/// A sweep's end: the rows now, the rows as the last command left them, and the cost.
pub struct Fresh {
    pub table: Table,
    pub old: Vec<Row>,
    pub cost: Cost,
}

impl Fresh {
    /// Whether the table must be written again: its rows moved (the instant alone moving is
    /// not a reason, unless a racy row needs a later instant to be trusted next time).
    pub fn moved(&self) -> bool {
        self.cost.racy > 0 || self.table.rows != self.old
    }
}

/// The rows now: `old` swept against the disk, every read hashed, with what it cost. The
/// rows' instant is the clock before the first stat.
pub fn sweep<L: Look>(look: &mut L, old: Table) -> Result<Fresh, CommandError> {
    let instant_ns = look.now_ns();
    let mut sweep = Sweep::new(old.rows, old.instant_ns);
    let mut stamps = Vec::with_capacity_in(sweep.name_count(), Global);
    for name in sweep.names() {
        stamps.push(look.stamp(name)?);
    }
    let first = sweep
        .stamped(&stamps)
        .map_err(|error| CommandError::File { path: "the checkout".into(), what: format!("its sweep: {error:?}").into_boxed_str() })?;
    let mut lists: Vec<String, Global> = Vec::new_in(Global);
    let mut probes: Vec<String, Global> = Vec::new_in(Global);
    let push = |walk: Walk, lists: &mut Vec<String, Global>, probes: &mut Vec<String, Global>| {
        lists.extend(walk.lists);
        probes.extend(walk.probes);
    };
    push(first, &mut lists, &mut probes);
    let mut entries: Vec<(String, Stamp), Global> = Vec::new_in(Global);
    let mut looks = 0u64;
    loop {
        looks += 1;
        if looks > MOST_LOOKS {
            return Err(CommandError::File { path: "the checkout".into(), what: format!("its sweep took more than {MOST_LOOKS} looks").into_boxed_str() });
        }
        if let Some(probe) = probes.pop() {
            let bytes = look.read(&probe, Some(blaze_solo::sweep::SIGNATURE.len()))?;
            let walk = sweep.probed(&probe, bytes);
            push(walk, &mut lists, &mut probes);
        } else if let Some(dir) = lists.pop() {
            entries.clear();
            look.list(&dir, &mut |name, stamp| entries.push((name.to_owned(), stamp)))?;
            let walk = sweep.listed(&dir, &entries);
            push(walk, &mut lists, &mut probes);
        } else {
            break;
        }
    }
    let Swept { rows, dirty, counts, old } = sweep.finish();
    let mut table = Table { instant_ns, rows };
    let mut cost = Cost { stated: counts.stated, listed: counts.listed, probed: counts.probed, racy: counts.racy, hashed: 0 };
    for row in table.rows.iter_mut() {
        if row.kind != Kind::File || !is_read(&row.path) {
            continue;
        }
        if row.id.is_none() || dirty.binary_search(&row.path).is_ok() {
            row.id = Some(file_answer(look.read(&row.path, None)?));
            cost.hashed += 1;
        }
    }
    Ok(Fresh { table, old, cost })
}

/// The request's reads, their answers when it began, and the host's leaf memory, all of the
/// table's read rows in path order.
pub struct Reads<'t, A: Allocator + Clone> {
    pub asks: Vec<Ask<'t>, A>,
    pub then: Vec<ContentId, A>,
    pub leaves: Vec<Leaf<'t>, A>,
    pub instant_ns: u64,
}

impl<'t, A: Allocator + Clone> Reads<'t, A> {
    /// Of `table`, swept: every read row is hashed ([`sweep`]); one that is not is refused.
    pub fn of(table: &'t Table, alloc: A) -> Result<Reads<'t, A>, CommandError> {
        let mut asks = Vec::new_in(alloc.clone());
        let mut then = Vec::new_in(alloc.clone());
        let mut leaves = Vec::new_in(alloc);
        for row in table.rows.iter().filter(|row| row.kind == Kind::File && is_read(&row.path)) {
            let id = row.id.ok_or_else(|| CommandError::File {
                path: row.path.clone().into_boxed_str(),
                what: "a read the sweep did not hash (rule: every read's answer is known before the request)".into(),
            })?;
            asks.push(Ask::File(row.path.as_bytes()));
            then.push(id);
            leaves.push(Leaf { path: row.path.as_bytes(), stamp: row.stamp, id });
        }
        Ok(Reads { asks, then, leaves, instant_ns: table.instant_ns })
    }

    pub fn leaves(&self) -> Leaves<'_> {
        Leaves { instant_ns: self.instant_ns, rows: &self.leaves }
    }
}

/// What the host hashed, folded into the rows: a look under a newer stamp replaces the row's.
/// A path the table has no row of (the host reads only the request's reads) is left out.
pub fn absorb(table: &mut Table, looked: &[Looked]) {
    for look in looked {
        let Ok(path) = std::str::from_utf8(&look.path) else { continue };
        if let Ok(at) = table.rows.binary_search_by(|row| row.path.as_str().cmp(path)) {
            let row = &mut table.rows[at];
            row.stamp = look.stamp;
            row.id = Some(look.id);
        }
    }
}

/// The table kept at `path`: none there is the first run's empty table; one that does not
/// decode is refused by name.
pub fn load(path: &Path) -> Result<Table, CommandError> {
    match std::fs::read(path) {
        Ok(bytes) => Table::decode(&bytes).map_err(|why| CommandError::Memory {
            path: path.display().to_string().into_boxed_str(),
            what: format!("not a leaf table: {why}; remove it and the next command sweeps the checkout anew").into_boxed_str(),
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Table::default()),
        Err(error) => Err(file_error(path, error.to_string())),
    }
}

/// `table` kept at `path`, whole or not at all: written beside it and renamed over it.
pub fn save(path: &Path, table: &Table) -> Result<(), CommandError> {
    write_whole(path, &table.encode())
}

/// `bytes` at `path` by a write beside it and a rename: a reader sees the old file or the new.
pub(crate) fn write_whole(path: &Path, bytes: &[u8]) -> Result<(), CommandError> {
    let dir = path.parent().ok_or_else(|| file_error(path, "names no directory"))?;
    std::fs::create_dir_all(dir).map_err(|error| file_error(dir, error.to_string()))?;
    let mut name = path.file_name().map(|it| it.as_bytes().to_vec()).unwrap_or_default();
    name.extend_from_slice(format!(".{}.new", std::process::id()).as_bytes());
    let beside = dir.join(std::ffi::OsStr::from_bytes(&name));
    std::fs::write(&beside, bytes).map_err(|error| file_error(&beside, error.to_string()))?;
    std::fs::rename(&beside, path).map_err(|error| file_error(path, error.to_string()))
}

