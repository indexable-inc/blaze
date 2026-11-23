//! [`Arena`]: one generation of this command's own puts, not yet committed, shared with the
//! pool's readers in place (the `InArena` side of [`Visibility`], the put path's seam; rule 78).
//!
//! WHY. Under [`Visibility::AfterCommit`] (today's), a record a phase puts is read by the next
//! phase's workers only from a segment: every phase commits (a batch copy, a layout copy, a
//! digest of every byte, a flip, a map) on the command thread before the next phase may start.
//! Under [`Visibility::InArena`] the one copy a put makes (the worker's bytes into this arena)
//! is the copy every reader reads, in place, before any commit: a [`super::Snapshot`] taken for a
//! phase sees the records put before its take, and the commit moves to after the answer (and to
//! a checkpoint by bytes or age; blaze-cli store/plain_place.rs).
//!
//! WHAT IT IS. Three blaze-mem [`AppendVec`]s (the tree's one structure for one writer and
//! lock-free readers below a published length, rule 12), each a reservation that never moves:
//!
//! - `blocks`: the records' bytes, each record from a block boundary (16 bytes: a segment's
//!   payload alignment, so a reader that reads in place reads the same alignment either way),
//!   appended with ONE publish per record ([`Arena::append`]), so a reader that sees the length
//!   sees every byte of every record below it (publish after complete, by construction);
//! - `entries`: each SEALED run's entries, sorted by key; a run is sealed at each snapshot take
//!   ([`Arena::seal`]: the puts since the last take, sorted, appended with one publish);
//! - `runs`: each run's `(start, len)` in `entries`, appended after its entries.
//!
//! A snapshot holds an [`ArenaView`]: the arena shared (`blaze_own::Arc` in the caller's
//! `Sync` allocator, `blaze_mem::Kept` of the store's tag) and two watermarks taken at the take,
//! the runs and the blocks published then. It reads runs below its watermark only, and bytes
//! below its block watermark only, so a record put after the take is absent through it (the
//! oracle's word "a snapshot taken at seq N saw a record of seq N+1": arena_tests.rs).
//!
//! WHAT MAKES IT SOUND: no `unsafe` here. Every byte is borrowed from an `AppendVec` the view (or
//! the `Lent`) owns a share of, through `AppendVec::as_slice` (its published prefix, its
//! soundness argued in blaze-mem append.rs); a published slot is never written again; the
//! reservation is freed only with the last `Arc`. Nothing reuses a block, an entry or a run
//! while any view or lend holds the arena: a commit starts a NEW arena (the type says so: there
//! is no `&mut` to a shared `Arena`, and nothing takes one back but its last owner's drop).
//!
//! COST. A put: its bytes copied once into committed pages (the one copy), one release store.
//! A take: the puts since the last take sorted by key (a scratch of 56-byte entries in the
//! `Lent`'s allocator, then one copy into `entries`), two release stores. A read: per run below
//! the watermark, newest first, a binary search of its entries (32-byte key compares); a hit
//! slices the blocks: no copy, no check, no allocation, no syscall.

use blaze_keyed::kind::{Namespace, Tag};
use blaze_mem::{AppendVec, Kept, Tag as MemTag};
use blaze_own::{Arc, Share};

use super::lent::LentError;

/// The put path's seam (rule 78): where a reader finds a record this command put and has not
/// committed. Chosen once, when the store's owner is made (blaze-cas `Lent::with_visibility`,
/// blaze-cli store/plain_place.rs `VISIBILITY`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Visibility {
    /// Today's: an own put is read through the `Lent` alone and is not in place
    /// (`NotInPlace::Arena`); a snapshot lists committed segments alone, so each phase commits
    /// before the next reads its records.
    AfterCommit,
    /// The one copy is the read: own puts lie in a shared [`Arena`], in place for the `Lent`'s
    /// readers and seen by every snapshot taken after them; `shared` is the `Sync` allocator
    /// the arena's shared owner is made in (the store's tag, as the snapshot's own list is:
    /// blaze-cli store.rs).
    InArena { shared: Kept },
}

/// At most this many blocks of one generation (one arena): a segment's bound
/// ([`blaze_keyed::store::SEGMENT_MOST`], 4 GiB), since what one arena holds is laid out as one
/// segment a namespace. Reserved, not committed: pages are committed as records arrive.
pub(super) const ARENA_BLOCKS: usize = (blaze_keyed::store::SEGMENT_MOST / 16) as usize;
/// At most this many records of one arena (rule 18: a table has a cap): 16 Mi, a record of
/// at least one block each fills `ARENA_BLOCKS` at 256 bytes a record on average.
pub(super) const ARENA_RECORDS: usize = 1 << 24;
/// At most this many runs of one arena: one each snapshot take with puts before it.
pub(super) const ARENA_RUNS: usize = 1 << 16;

/// One block: sixteen bytes, at sixteen-byte alignment in the column (a page-aligned base and
/// a size of sixteen).
pub(super) type Block16 = [u8; 16];

/// A sealed record's place: its key, its first block, its length, its kind and namespace.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct RunEntry {
    pub key: [u8; 32],
    pub block: u32,
    pub len: u32,
    pub tag: Tag,
    pub namespace: Namespace,
}

/// One sealed run's place in `entries`.
#[derive(Clone, Copy, Debug)]
struct RunAt {
    start: u32,
    len: u32,
}

/// One generation of own puts: see the module's docs.
pub(super) struct Arena {
    blocks: AppendVec<Block16>,
    entries: AppendVec<RunEntry>,
    runs: AppendVec<RunAt>,
}

/// The cursor over a put's parts, one block at a time (a block may take the end of one part and
/// the start of the next), zero-padded at the end.
struct Cursor<'a, 'b> {
    parts: &'a [&'b [u8]],
    part: usize,
    at: usize,
}

impl Cursor<'_, '_> {
    fn next_block(&mut self) -> Block16 {
        let mut out = [0u8; 16];
        let mut fill = 0usize;
        while fill < 16 && self.part < self.parts.len() {
            let whole = self.parts[self.part];
            let rest = &whole[self.at..];
            let take = (16 - fill).min(rest.len());
            out[fill..fill + take].copy_from_slice(&rest[..take]);
            fill += take;
            self.at += take;
            if self.at == whole.len() {
                self.part += 1;
                self.at = 0;
            }
        }
        out
    }
}

fn reserve(what: &'static str, wanted: usize) -> impl FnOnce(blaze_mem::MemError) -> LentError {
    move |_| LentError::ArenaReserve { what, wanted }
}

impl Arena {
    /// An empty arena: three reservations, nothing committed.
    pub(super) fn new() -> Result<Arena, LentError> {
        Ok(Arena {
            blocks: AppendVec::new(ARENA_BLOCKS, MemTag::Store).map_err(reserve("blocks reserved", ARENA_BLOCKS))?,
            entries: AppendVec::new(ARENA_RECORDS, MemTag::Store).map_err(reserve("record entries reserved", ARENA_RECORDS))?,
            runs: AppendVec::new(ARENA_RUNS, MemTag::Store).map_err(reserve("runs reserved", ARENA_RUNS))?,
        })
    }

    /// Blocks published.
    pub(super) fn blocks(&self) -> u32 {
        self.blocks.len()
    }

    /// Runs published.
    pub(super) fn runs(&self) -> u32 {
        self.runs.len()
    }

    /// The concatenation of `parts` (`total` bytes) copied in, whole, with one publish: its
    /// first block. The one writer is the `Lent`'s thread (`AppendVec::writer` stops a second).
    pub(super) fn append(&self, parts: &[&[u8]], total: usize) -> Result<u32, LentError> {
        let need = total.div_ceil(16).max(1);
        let mut cursor = Cursor { parts, part: 0, at: 0 };
        self.blocks.writer().try_extend_with(need, |_| cursor.next_block()).map_err(reserve("bytes past the blocks reserved", need * 16))
    }

    /// The `len` bytes from `block`, when every block of them is below `upto` (a view's
    /// watermark; `u32::MAX` for the owner, which reads all it published).
    pub(super) fn bytes(&self, upto: u32, block: u32, len: u32) -> Option<&[u8]> {
        let all = self.blocks.as_slice();
        let seen = &all[..(upto as usize).min(all.len())];
        let need = (len as usize).div_ceil(16).max(1);
        let start = block as usize;
        seen.get(start..start.checked_add(need)?)?.as_flattened().get(..len as usize)
    }

    /// `sorted` (entries sorted by key, of records whose blocks are published) sealed as one
    /// run: its entries published, then the run. The runs published after.
    pub(super) fn seal(&self, sorted: &[RunEntry]) -> Result<u32, LentError> {
        if sorted.is_empty() {
            return Ok(self.runs.len());
        }
        let len = u32::try_from(sorted.len()).map_err(|_| LentError::ArenaReserve { what: "record entries in one run", wanted: sorted.len() })?;
        let start = self.entries.try_extend_from_slice(sorted).map_err(reserve("record entries past those reserved", sorted.len()))?;
        // After its entries: a reader that sees the run sees them (each publish is a release).
        self.runs.try_append(RunAt { start, len }).map_err(reserve("runs past those reserved", 1))?;
        Ok(self.runs.len())
    }

    /// The entry of `key` in the runs below `upto`, newest run first.
    pub(super) fn find(&self, upto: u32, key: &[u8; 32]) -> Option<RunEntry> {
        // The runs first: their entries were published before them.
        let runs = self.runs.as_slice();
        let runs = &runs[..(upto as usize).min(runs.len())];
        let entries = self.entries.as_slice();
        for run in runs.iter().rev() {
            let one = entries.get(run.start as usize..run.start as usize + run.len as usize)?;
            if let Ok(at) = one.binary_search_by(|entry| entry.key.cmp(key)) {
                return Some(one[at]);
            }
        }
        None
    }
}

/// What a snapshot sees of an arena: the arena shared, and what was published at its take.
pub(super) struct ArenaView {
    arena: Arc<Arena, Kept>,
    runs: u32,
    blocks: u32,
}

impl ArenaView {
    /// The view of `arena` as it stands: every run and every block published now.
    pub(super) fn of(arena: &Arc<Arena, Kept>) -> ArenaView {
        // The runs first: a run's records' blocks were published before it was sealed.
        let runs = arena.runs();
        let blocks = arena.blocks();
        ArenaView { arena: arena.share(), runs, blocks }
    }

    /// The planted bug `SnapshotPlant::ShortWatermark`: the block watermark one block short, as
    /// a take that published its index before the last record's bytes would leave it.
    pub(super) fn short(self) -> ArenaView {
        ArenaView { blocks: self.blocks.saturating_sub(1), ..self }
    }

    /// The entry of `key` in the runs this view sees (every run published by now when
    /// `unbounded`: the planted bug `SnapshotPlant::SeesLater`).
    pub(super) fn find(&self, key: &[u8; 32], unbounded: bool) -> Option<RunEntry> {
        self.arena.find(if unbounded { u32::MAX } else { self.runs }, key)
    }

    /// The bytes of `entry`, when they are below this view's block watermark (or anywhere
    /// published, `unbounded`).
    pub(super) fn bytes(&self, entry: &RunEntry, unbounded: bool) -> Option<&[u8]> {
        self.arena.bytes(if unbounded { u32::MAX } else { self.blocks }, entry.block, entry.len)
    }

    /// Runs and blocks this view sees.
    pub(super) fn seen(&self) -> (u32, u32) {
        (self.runs, self.blocks)
    }
}

// `ArenaView` crosses to the pool's workers inside a `Snapshot`: a field that is not `Send +
// Sync` breaks the build here.
const _: fn() = || {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<ArenaView>();
};
