//! What user code read outside every view (the toolchain, the C compiler's, `/usr/include`, a
//! `path` dependency beside the checkout, an output placed), remembered by stamp: no directory
//! outside a checkout is watched. Each read is a leaf, its path with the [`Stamp`] it had when
//! it was looked at and the instant of the look; a later check asks one `lstat` (or `stat`, for
//! a read through links) of the leaf and holds the answer when [`holds`] says so.
//!
//! - **The stamp** is [`blaze_core::Stamp`], the one definition (inode, size, mtime, ctime and
//!   mode, nanoseconds), the stricter of the two this replaced: blaze-usercode's own (length,
//!   mtime, ctime, inode, device) had no mode, so a chmod with nothing else moved held, and
//!   blaze-host's `Leaf` already used this one. The device is in neither driver's stamp; a file
//!   moved to another device with the same inode, size and both times is the one change it
//!   misses.
//! - **The racy rule** is [`crate::racy`] (git's): a stamp whose change is within its window of
//!   the look ([`crate::window_ns`]: 2 s for whole-second stamps, 20 ms else) vouches for
//!   nothing, so a second write in the same tick of the file system's clock is never missed.
//! - **An absent path** has [`Stamp::GONE`], asked of the whole path: a `stat` resolves every
//!   part each time, so a path missing a part before its last (`/p/src/lib.rs` with no `src`)
//!   is one stat too, and its making is seen whichever part is made. This is the rule
//!   blaze-usercode's `names` kept for its watches (the first missing part is the name that
//!   would be made) without a parent's stamp: a full-path stat already answers it.
//! - **A link**: a `stat` stamp is the target's (a link pointed elsewhere has another inode);
//!   an `lstat` stamp is the link's own (a link is never written in place, only replaced). A
//!   caller that reads through a link with an `lstat` stamp in hand must not hold it; blaze-host
//!   refuses a link's stamp for a file's content for that reason.
//!
//! The floor of a check: one stat per leaf, nothing read, no allocation; a file is read again
//! only when its stamp moved or was racy. Every check here costs exactly that (ratio 1.0); a
//! watch-based check costs zero syscalls per leaf but a walk of the watched tree to set up
//! (256,216 directories under `/root` on the build VM, on the daemon's loop), which is what
//! this module replaced. A directory of leaves is never walked.

use std::path::{Path, PathBuf};

use blaze_core::Stamp;
use blaze_ids::ContentId;

use crate::sync::Lock;

/// The stamp of `meta`, as every driver's `Effect::Stamp` makes one: nanoseconds since the
/// epoch, the inode, size and `st_mode`.
#[cfg(unix)]
pub fn stamp_of(meta: &std::fs::Metadata) -> Stamp {
    use std::os::unix::fs::MetadataExt as _;
    let ns = |secs: i64, nsec: i64| {
        u64::try_from(secs).unwrap_or(0).saturating_mul(1_000_000_000).saturating_add(u64::try_from(nsec).unwrap_or(0))
    };
    Stamp {
        ino: meta.ino(),
        size: meta.size(),
        mtime_ns: ns(meta.mtime(), meta.mtime_nsec()),
        ctime_ns: ns(meta.ctime(), meta.ctime_nsec()),
        mode: meta.mode(),
    }
}

/// What a stat's result is as a stamp: [`Stamp::GONE`] for nothing there (the name, or a part
/// above it, missing or not a directory), [`Stamp::UNKNOWN`] for any other error, which no
/// remembered stamp equals, so the asker looks and meets the error itself.
#[cfg(unix)]
fn stamp_or(meta: std::io::Result<std::fs::Metadata>) -> Stamp {
    match meta {
        Ok(meta) => stamp_of(&meta),
        Err(error) if matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => Stamp::GONE,
        Err(_) => Stamp::UNKNOWN,
    }
}

/// The stamp of `path` itself, a link not followed. A driver's call: one `lstat`.
#[cfg(unix)]
pub fn lstat(path: &Path) -> Stamp {
    stamp_or(std::fs::symlink_metadata(path))
}

/// The stamp of what `path` leads to, links followed. A driver's call: one `stat`.
#[cfg(unix)]
pub fn stat(path: &Path) -> Stamp {
    stamp_or(std::fs::metadata(path))
}

/// Whether a leaf stamped `was` by a look at `looked_ns` is as it was, its stamp being `now`:
/// the one rule of "this path is as it was". `now` must be known, equal to `was`, and `was`
/// not racy at the look ([`crate::racy`]). [`Stamp::GONE`] holds against itself, at any
/// instant: a path still absent is as it was (a gone stamp has no change time, so it is never
/// racy), which is what a probe of a missing file rests on. A fact about the bytes at a path
/// asks [`holds_bytes`], which a gone path never satisfies. `looked_ns` is the wall clock
/// ([`blaze_core::system_wall_ns`], the clock a kernel stamps a write with) read before the
/// look's first stat, a lower bound of the look, so the rule errs toward racy.
///
/// The invariant: a leaf whose file was written, replaced, removed, made, chmodded or renamed
/// over since the look does not hold, and neither does one written in the tick it was looked
/// at; one absent at the look and absent now holds.
pub fn holds(was: &Stamp, looked_ns: u64, now: &Stamp) -> bool {
    !now.is_unknown() && was == now && !crate::racy(was, looked_ns)
}

/// [`holds`], for a fact about the bytes at the path (a hash, a placed output's content, a
/// digest file's mark): a path that is gone holds no bytes, so it vouches for none, even
/// against a [`Stamp::GONE`] it was stamped with.
pub fn holds_bytes(was: &Stamp, looked_ns: u64, now: &Stamp) -> bool {
    !now.is_gone() && holds(was, looked_ns, now)
}

/// One remembered look: a file's path, its stamp, and the id of the bytes read under it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Leaf<'l> {
    pub path: &'l [u8],
    pub stamp: Stamp,
    pub id: ContentId,
}

/// A leaf memory: rows sorted by path, each once, looked at no earlier than `instant_ns` (the
/// wall clock before that look's first stat, a lower bound of every look).
#[derive(Clone, Copy, Debug)]
pub struct Leaves<'l> {
    pub instant_ns: u64,
    pub rows: &'l [Leaf<'l>],
}

impl Leaves<'_> {
    pub const NONE: Leaves<'static> = Leaves { instant_ns: 0, rows: &[] };

    /// The row of `path`, by binary search.
    pub fn find(&self, path: &[u8]) -> Option<&Leaf<'_>> {
        self.rows.binary_search_by(|row| row.path.cmp(path)).ok().map(|at| &self.rows[at])
    }

    /// The row of `path` when its stamp, `now`, holds ([`holds`]).
    pub fn held(&self, path: &[u8], now: &Stamp) -> Option<&Leaf<'_>> {
        self.find(path).filter(|leaf| holds(&leaf.stamp, self.instant_ns, now))
    }
}

/// What a look at the disk was of ([`Look`]).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum LookKind {
    /// A file's content, links followed: the answer is its content's hash; the stamp a `stat`.
    File,
    /// The path itself, a link not followed: the answer is a hash of what it is (a
    /// directory, or a file's or a link's id and mode); the stamp an `lstat`.
    Entry,
    /// The names in a directory: the answer is the hash of its listing; the stamp the
    /// directory's `lstat`, which moves when a name is made, removed or renamed in it.
    Listing,
}

impl LookKind {
    /// The stamp a sweep asks of a look of this kind at `path`: links followed for a file's
    /// content, not for an entry or a listing.
    #[cfg(unix)]
    pub fn stamp_now(self, path: &Path) -> Stamp {
        match self {
            LookKind::File => stat(path),
            LookKind::Entry | LookKind::Listing => lstat(path),
        }
    }

    /// Whether a sweep's `Effect::Stamp` (an `lstat`) answers a look of this kind: not a
    /// file's content, whose stamp is its target's.
    pub fn lstat_answers(self) -> bool {
        !matches!(self, LookKind::File)
    }
}

impl std::fmt::Display for LookKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            LookKind::File => "the file",
            LookKind::Entry => "the entry",
            LookKind::Listing => "the names in",
        })
    }
}

/// One look at the disk for what `path`, outside every view, is now (a read, a `stat`, a
/// listing): the path, which question was asked of it, what the disk said (`None`: nothing is
/// there), the stamp it had and the instant of the look ([`Stamp::UNKNOWN`] when the looker
/// had none: such a look is checked by its answer, never by stamp), and the caller that looked.
/// Made by each place that checks such a path (blaze-usercode's `inputs` and `roots`, base-db's
/// `IncludedFile::read_now`, blaze-lints' `profile::load`, the daemon's pinned native layouts)
/// and kept by the build it was made for ([`Looks`]).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Look {
    pub path: PathBuf,
    pub kind: LookKind,
    pub answer: Option<[u8; 32]>,
    pub stamp: Stamp,
    pub looked_ns: u64,
    pub by: &'static std::panic::Location<'static>,
}

impl Look {
    /// Whether this look still says what it said, its path's stamp being `now`
    /// ([`LookKind::stamp_now`]): never for a look with no stamp.
    pub fn holds(&self, now: &Stamp) -> bool {
        !self.stamp.is_unknown() && holds(&self.stamp, self.looked_ns, now)
    }
}

/// The looks one build made, owned by that build: every place that looks at the disk is
/// handed the `Looks` of the build it looks for and pushes its look there, so no look is kept
/// by the process and none is another build's. A clone is the same list. No cap: a first
/// build after a daemon start looks at every native input and traced path once, and so does
/// every build since: a look answered from a stamp memory is pushed as one made.
///
/// The invariant: a look is in the list of the build it was made for and of no other, each
/// distinct (path, kind, answer) once, in an order that depends on no thread's timing.
#[derive(Clone)]
pub struct Looks(Option<std::sync::Arc<Lock<Kept>>>);

/// What is kept of a look beside its key: its stamp, the instant of the look and its caller.
#[derive(Clone, Copy)]
struct Seen {
    stamp: Stamp,
    looked_ns: u64,
    by: &'static std::panic::Location<'static>,
}

/// A build's looks by (path, kind, answer), sorted: the same set from any interleaving.
type Kept = std::collections::BTreeMap<(PathBuf, LookKind, Option<[u8; 32]>), Seen>;

impl Looks {
    /// An empty list, for one build.
    pub fn new() -> Looks {
        Looks(Some(std::sync::Arc::new(Lock::new("vfs.outside.looks", Kept::new()))))
    }

    /// A handle that keeps nothing, for a caller with no build (a check at a certificate
    /// hit, a test): such a caller uses the answer of its look itself.
    pub const fn none() -> Looks {
        Looks(None)
    }

    /// Whether this keeps what it is told ([`Looks::new`]).
    pub fn keeps(&self) -> bool {
        self.0.is_some()
    }

    /// Keeps a look of `kind` at `path` that said `answer`, made with no stamp in hand: it is
    /// checked again by its answer ([`Look::stamp`] is [`Stamp::UNKNOWN`]).
    #[track_caller]
    pub fn push(&self, path: &Path, kind: LookKind, answer: Option<[u8; 32]>) {
        self.keep(path, kind, answer, Stamp::UNKNOWN, 0, std::panic::Location::caller());
    }

    /// Keeps a look of `kind` at `path` that said `answer` under `stamp`, taken by a look at
    /// `looked_ns` ([`LookKind::stamp_now`] before the read): checked again by stamp
    /// ([`Look::holds`]). A second look that said the same is the same row, the first stamp
    /// kept; one that said something else is a second row ([`Looks::twice`]).
    #[track_caller]
    pub fn push_stamped(&self, path: &Path, kind: LookKind, answer: Option<[u8; 32]>, stamp: Stamp, looked_ns: u64) {
        self.keep(path, kind, answer, stamp, looked_ns, std::panic::Location::caller());
    }

    fn keep(
        &self,
        path: &Path,
        kind: LookKind,
        answer: Option<[u8; 32]>,
        stamp: Stamp,
        looked_ns: u64,
        by: &'static std::panic::Location<'static>,
    ) {
        let Some(kept) = &self.0 else { return };
        kept.lock().entry((path.to_path_buf(), kind, answer)).or_insert(Seen { stamp, looked_ns, by });
    }

    /// Keeps each of `looks` too: a build that used a task another build ran holds that
    /// task's looks as its own.
    pub fn extend(&self, looks: &[Look]) {
        let Some(kept) = &self.0 else { return };
        let mut kept = kept.lock();
        for look in looks {
            let seen = Seen { stamp: look.stamp, looked_ns: look.looked_ns, by: look.by };
            kept.entry((look.path.clone(), look.kind, look.answer)).or_insert(seen);
        }
    }

    /// How many looks are kept.
    pub fn len(&self) -> usize {
        self.0.as_ref().map_or(0, |kept| kept.lock().len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every look kept, sorted by path, kind and answer.
    pub fn all(&self) -> Vec<Look> {
        let Some(kept) = &self.0 else { return Vec::new() };
        kept.lock()
            .iter()
            .map(|((path, kind, answer), seen)| Look {
                path: path.clone(),
                kind: *kind,
                answer: *answer,
                stamp: seen.stamp,
                looked_ns: seen.looked_ns,
                by: seen.by,
            })
            .collect()
    }

    /// Each pair of looks in `looks` (sorted, as [`Looks::all`] gives them) of one path and
    /// kind that said two things: what such a build cannot be checked again by.
    pub fn twice(looks: &[Look]) -> impl Iterator<Item = (&Look, &Look)> {
        looks.windows(2).filter(|it| it[0].path == it[1].path && it[0].kind == it[1].kind).map(|it| (&it[0], &it[1]))
    }
}

impl Default for Looks {
    fn default() -> Looks {
        Looks::new()
    }
}

impl std::fmt::Debug for Looks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.all().iter()).finish()
    }
}

#[cfg(test)]
mod tests;
