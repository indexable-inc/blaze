//! [`Snapshot`]: a `Send + Sync`, read-only view of the committed segments of one generation,
//! for the engine's pool workers to read records by key themselves (runtime.md, end state 5:
//! nothing shared and mutable on the path of an item; a stage's tables are immutable while it
//! runs). [`Lent`] is the command's owner of every byte and is not `Sync` (a `RefCell` over the
//! store, with an arena of puts not yet committed), so it cannot be read from a pool; a snapshot
//! taken from it after a commit can.
//!
//! WHAT IT HOLDS. The generation's local segments that are mapped and proved, in the order a
//! [`Plain::get`] probes them (each namespace read, in [`Plain::reads`]' order; within one, the
//! generation's probe order, largest first, ties newest first: probe.rs `order_view`), each as
//! its manifest row and a CLONE of its [`Mapping`]. A `Mapping` is shared ownership of the bytes
//! (an `Arc`), so the snapshot owns what it reads: it borrows nothing of the `Lent`, and a
//! commit, a compaction or another put may run while it lives. Nothing else: no arena, no held
//! batch, no lock, no cell.
//!
//! WHAT MAKES IT SOUND, each a stated assumption with its test (no `unsafe` here: every byte is
//! borrowed from a `Mapping` the snapshot owns, through safe slicing):
//!
//! - (a) The bytes outlive every borrow of them. A record's bytes are borrowed from the
//!   snapshot (`&self`), and the snapshot holds a clone of each mapping it lists. blaze-core
//!   lib.rs, [`Mapping`]: "its bytes, which stay as they are for as long as any clone of it
//!   lives". A compaction that replaces a listed segment retires its file and unlinks it
//!   [`super::GRACE_NS`] later; a mapping outlives an unlink (plain.rs, "Compaction"), and the
//!   store itself never unmaps a segment while it is open (core.rs `Plain::maps`). Evidence:
//!   [`tests::a_snapshot_reads_what_was_committed_before_it_through_commits_compactions_and_crashes`]
//!   (compactions merge the segments a live snapshot lists, and its reads stay exact).
//! - (b) A sealed segment's bytes never change: plain.rs, "On disk": `seg-<id>`, "written once,
//!   never changed"; blaze-core lib.rs, `Mapping::map_file`: "the bytes below the length it had
//!   when mapped are never written again and it is never truncated". The snapshot lists only
//!   segments proved once ([`super::verified::Verified`], decision 18), so a read checks nothing.
//! - (c) A key put after the snapshot was taken is ABSENT through it, by design: the list is
//!   fixed at the take. Under `Visibility::AfterCommit` (arena.rs), a key put before it and not
//!   yet committed is absent too: a private arena put is in no segment. Evidence: the same test
//!   (its oracle), and its controls
//!   [`tests::the_oracle_catches_a_snapshot_that_sees_an_uncommitted_put`]. Under
//!   `Visibility::InArena`, a key put before the take reads its exact bytes through it from the
//!   shared arena (the take seals the puts since the last into a run; the view holds the arena
//!   and its runs' and blocks' watermarks at the take, so a later put is absent): evidence in
//!   arena_tests.rs, its controls `SnapshotPlant::SeesLater` and `ShortWatermark`.
//!
//! A snapshot is taken from a WHOLE store ([`Lent::snapshot_in`] maps every segment first,
//! [`Lent::map_all`]): a lazy store has rows not mapped yet, and a snapshot over it would answer
//! "absent" for a committed key (control:
//! [`tests::the_oracle_catches_a_snapshot_over_a_lazy_store`]). It never maps, fetches or falls
//! back to the `Lent` (rule 3): absent through a snapshot is absent from what it lists.
//!
//! THE ENGINE'S USE. The phase loop commits its puts (`CommandStore::commit_puts`), takes one
//! snapshot, hands `&Snapshot` (a `Copy` reference) to the stage's workers, and drops it at the
//! stage's end. Records the stage puts are not visible through it (c); the next stage's
//! snapshot, taken after the next commit, sees them. Holding a snapshot over a commit is
//! allowed (it keeps reading its own generation), but a stage that needs a newer record takes a
//! newer snapshot; nothing refreshes one.
//!
//! COST OF ONE `get` (as read here, not measured): for each listed segment, in order, until one
//! holds the key: the `Mapping`'s bytes (one pointer through its `Arc` to the mapped extent,
//! the same line every get), then `format::find` over the sorted 48-byte entries: up to three
//! interpolation steps on the key's first eight bytes, then bisection (at most
//! `3 + log2 n` probes, each one 32-byte key compare, one or two cache lines; typically two or
//! three probes on uniform keys). A hit then reads the entry's place and slices the payload: no
//! copy, no checksum, no allocation, no syscall. A NEGATIVE lookup probes every listed segment:
//! there is no filter for absent keys yet (store-next.md, "Left, and why": "A per-segment fuse
//! filter ... Planned, not written here"), so its cost is the segment count (compaction keeps
//! it under [`super::SEGMENTS_MOST`], 276 for a namespace under 256 GiB, about 45 at 100 GB,
//! once its cleanup has run) times one
//! index search.

use core::fmt;
use std::alloc::Allocator;

use blaze_core::{Driver, Mapping};
use blaze_own::Share;
use blaze_ids::ContentId;
use blaze_keyed::kind::{Namespace, ReservedTag};
use blaze_keyed::store::{ENTRY, Generation};

use super::arena::ArenaView;
use super::bloom::{BloomMapped, BloomRef};
use super::core::Plain;
use super::format::{self, Entry, Segment};
use super::lent::{Lent, LentError};
use super::verified::Proof;
use crate::Key;

/// At most this many segments in one snapshot (rule 18): a table cap with a typed refusal, as
/// a multiple of the one bound ([`super::SEGMENTS_MOST`], 276 a namespace under 256 GiB after
/// a cleanup). A
/// snapshot is taken mid-command too, after the command's own commits (one each phase and each
/// 256 MiB) and before its cleanup merges them, and after a crashed command whose commits no
/// cleanup merged: so this is not the at-rest bound (a store at it would refuse every command
/// before the cleanup that restores it), but 21 times it (5796), room for a command's commits
/// past a namespace at its bound and for every namespace a home holds.
pub const SNAPSHOT_SEGMENTS_MOST: usize = 21 * super::SEGMENTS_MOST;

const _: () = assert!(SNAPSHOT_SEGMENTS_MOST >= 1024, "no store the old cap admitted is refused");

/// Why a snapshot could not be taken or read, by name (rule 19).
#[derive(Clone, Debug)]
pub enum SnapshotError {
    /// The store's owner refused while making the store whole for the snapshot (`doing`).
    Lent { doing: &'static str, why: LentError },
    /// The store is not whole (a row not mapped): a snapshot over it would miss committed keys.
    NotWhole { generation: u64 },
    /// The generation names more mapped segments than a snapshot holds.
    TooManySegments { generation: u64, segments: usize, most: usize },
    /// The allocator refused the list of this many segments.
    Alloc { segments: usize },
    /// A pointer met where a record was asked: a pointer is read by `Lent::pointed` alone (its
    /// newest value is a name in the newest generation, not a probe).
    PointerTag { key: Key },
    /// A record of the shared arena's runs this snapshot sees, whose bytes are not below its
    /// block watermark: its index was published before its bytes (arena.rs: never, by
    /// construction; a planted control's word).
    Unpublished { key: Key },
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SnapshotError::Lent { doing, why } => write!(f, "a store snapshot: the store's owner refused while {doing}: {why}"),
            SnapshotError::NotWhole { generation } => {
                write!(f, "a store snapshot of generation {generation}: the store is not whole (a segment not mapped yet), so the snapshot would miss committed records; Lent::map_all first")
            }
            SnapshotError::TooManySegments { generation, segments, most } => {
                write!(f, "a store snapshot of generation {generation}: {segments} segments mapped, past the {most} a snapshot holds: compaction is behind")
            }
            SnapshotError::Alloc { segments } => write!(f, "a store snapshot: the allocator refused its list of {segments} segments"),
            SnapshotError::PointerTag { key } => write!(f, "a store snapshot read of {:?}: a pointer met as a record (a pointer is read by Lent::pointed alone)", key.0),
            SnapshotError::Unpublished { key } => write!(f, "a store snapshot read of {:?}: the shared arena's run names it, but its bytes were not published at the take (an index published before its bytes)", key.0),
        }
    }
}

/// The planted bugs of `snapshot_tests`; outside tests, none.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SnapshotPlant {
    None,
    /// Taking a snapshot commits the arena first, "so it is complete": a key put and not yet
    /// committed when the snapshot was asked is read through it.
    CommitsFirst,
    /// The lookup compares only the key's first eight bytes (the interpolation's prefix): on a
    /// collision of the prefix it returns another key's bytes.
    ShortPrefix,
    /// Taken without making the store whole: a row not mapped yet (a lazy open's) is not listed,
    /// and its committed keys read as absent.
    SkipsMapAll,
    /// `Visibility::InArena`: the view reads every run and block of the shared arena published
    /// by the time of the READ, not of the take: a record put (and sealed by a later take) after
    /// this snapshot was taken is seen through it (seq N sees N + 1).
    SeesLater,
    /// `Visibility::InArena`: the take's block watermark one block short, as a take that
    /// published a run before its last record's bytes would leave it.
    ShortWatermark,
}

/// One listed segment: its manifest row (where its index lies), its mapping, shared, the
/// proof its [`super::verified::Verified`] gave when the snapshot was taken, and its optional
/// verified per-segment Bloom filter for O(1) negative rejection.
struct Listed {
    seg: Segment,
    map: Mapping,
    proof: Proof,
    bloom: Option<BloomRef>,
}

/// A record read through a snapshot with the store's proof beside its bytes: the bytes lie in a
/// segment proved once this boot (its manifest row, its seal, or its one full check;
/// verified.rs), so they are the bytes put under the key, whole, and no byte is hashed for it
/// (rule 74). `proof` is a [`Proof`], which only a `Verified` makes (`Verified::proof`; its
/// fields are private to verified.rs): a `Proved` cannot be built over bytes nothing proved
/// (rule 76). It proves the bytes, not the form of the record they hold.
#[derive(Clone, Copy, Debug)]
pub struct Proved<'s> {
    pub bytes: &'s [u8],
    pub proof: Proof,
}

/// A read-only snapshot of the committed segments of one generation: see the module's docs.
///
/// `Send + Sync` by construction (plain data and [`Mapping`]s, which blaze-core makes `Send +
/// Sync`; checked below at compile time), so `&Snapshot` is handed to every worker of a pool.
/// Reads take `&self`, touch nothing shared that is written, and allocate nothing.
///
/// # Guarantees
///
/// - Every key committed before the take reads its exact bytes for as long as the snapshot
///   lives; every other key is absent. Evidence:
///   [`tests::a_snapshot_reads_what_was_committed_before_it_through_commits_compactions_and_crashes`].
/// - Read from many threads at once, it answers as from one. Evidence:
///   [`tests::a_snapshot_is_read_from_eight_threads_at_once`].
pub struct Snapshot<A: Allocator> {
    /// In probe order: the namespaces read, in order; within one, the generation's probe order.
    listed: Vec<Listed, A>,
    /// Shared mapped `bloom` sidecar and lazy per-slot atomic verification states (one handle per
    /// snapshot, zero per-segment `Mapping` clones).
    bloom_mapped: Option<BloomMapped>,
    generation: u64,
    plant: SnapshotPlant,
    /// `Visibility::InArena`: what the take saw of the shared arena of puts not yet committed
    /// (arena.rs); read before the segments. `None` under `AfterCommit`, or with no such put.
    arena: Option<ArenaView>,
}

// `Snapshot` is `Send + Sync` (the pool's workers share `&Snapshot`): a field that is not breaks
// the build here, not a caller. Checked over blaze-mem's `Kept`, a `Send + Sync` allocator of
// the kind a pool's caller hands it.
const _: fn() = || {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<Snapshot<blaze_mem::Kept>>();
};

impl<A: Allocator> Snapshot<A> {
    /// The snapshot of `plain`'s generation as it stands: every mapped, proved local segment of
    /// each namespace it reads, in probe order, each mapping cloned (no byte copied). `plain`
    /// must be whole ([`Plain::whole`]); the plant [`SnapshotPlant::SkipsMapAll`] takes it anyway.
    pub(crate) fn of<P: Allocator + Clone>(plain: &Plain<P>, alloc: A, plant: SnapshotPlant) -> Result<Snapshot<A>, SnapshotError> {
        let generation = plain.generation();
        if !plain.whole() && plant != SnapshotPlant::SkipsMapAll {
            return Err(SnapshotError::NotWhole { generation });
        }
        let reads = plain.reads();
        let segments: usize = reads.iter().map(|&space| in_space(plain, space).count()).sum();
        if segments > SNAPSHOT_SEGMENTS_MOST {
            return Err(SnapshotError::TooManySegments { generation, segments, most: SNAPSHOT_SEGMENTS_MOST });
        }
        let mut listed = Vec::new_in(alloc);
        listed.try_reserve_exact(segments).map_err(|_| SnapshotError::Alloc { segments })?;
        let branded = Generation(generation);
        let bloom_mapped = plain.bloom_shared_mapped();
        for &space in reads {
            for at in in_space(plain, space) {
                let verified = plain.mapped_at(at);
                let seg = *verified.seg();
                let bloom = plain.bloom_ref_for(&seg);
                // Within the reserve: no reallocation.
                listed.push(Listed { seg, map: verified.mapping().share(), proof: verified.proof(branded), bloom });
            }
        }
        Ok(Snapshot { listed, bloom_mapped, generation, plant, arena: None })
    }

    /// This snapshot with `view` of the shared arena beside its segments.
    fn with_arena(self, view: Option<ArenaView>) -> Snapshot<A> {
        Snapshot { arena: view, ..self }
    }

    /// `(runs, blocks)` of the shared arena this snapshot sees: `(0, 0)` with none.
    pub fn arena_seen(&self) -> (u32, u32) {
        self.arena.as_ref().map_or((0, 0), ArenaView::seen)
    }

    /// The bytes committed under `key` in the first listed segment that holds it, in place from
    /// its mapping, for the life of this borrow of the snapshot. `None`: absent from every
    /// listed segment (a key committed after the take included). A pure read: no lock, no
    /// interior mutability, no allocation, no syscall.
    ///
    /// The bytes start where the segment's payload does, at a multiple of `ALIGN` (16) in a
    /// mapping that begins on a page (or, for a copied mapping, a sixteen-byte boundary): where
    /// the `Aligned16` of lent.rs lands, it wraps this return (`Aligned16::new(bytes)`).
    ///
    /// The bytes alone: [`Snapshot::get_proved`]'s bytes, its proof dropped. A caller that hashes
    /// every byte itself (the `--verify` path: blaze-engine pages.rs `Sealed::hashed`) reads
    /// through this; one that takes the store's proof reads through `get_proved`.
    pub fn get(&self, key: &Key) -> Result<Option<&[u8]>, SnapshotError> {
        Ok(self.get_proved(key)?.map(|proved| proved.bytes))
    }

    /// [`Snapshot::get`] with the store's proof beside the bytes: the [`Proof`] of the listed
    /// segment that holds the key (made once, at the take, from its `Verified`; nothing checked
    /// here). The same pure read: no lock, no allocation, no syscall.
    pub fn get_proved(&self, key: &Key) -> Result<Option<Proved<'_>>, SnapshotError> {
        if let Some(view) = &self.arena {
            // A key is put once (a put of a held key is `Same`), so where it lies the order of
            // the probe does not change: the arena's runs first, a few bisections.
            let unbounded = self.plant == SnapshotPlant::SeesLater;
            if let Some(entry) = view.find(&key.0.0, unbounded) {
                if ReservedTag::Pointer.is(entry.tag) {
                    return Err(SnapshotError::PointerTag { key: *key });
                }
                let Some(bytes) = view.bytes(&entry, unbounded) else { return Err(SnapshotError::Unpublished { key: *key }) };
                // This process's own put, copied by this tool (blaze-cli plain_place.rs
                // `Checked`, way 2): the proof of a record not yet in any segment.
                return Ok(Some(Proved { bytes, proof: Proof::own(Generation(self.generation)) }));
            }
        }
        for listed in self.listed.iter() {
            if listed.seg.count == 0 {
                continue;
            }
            if let Some(bloom) = &listed.bloom
                && !bloom.may_contain(self.bloom_mapped.as_ref(), &key.0.0)
            {
                continue;
            }
            let file = listed.map.bytes();
            let run = listed.seg.run(file);
            let Some(i) = self.find(run, &key.0) else { continue };
            let entry = Entry::read(&run[i * ENTRY..(i + 1) * ENTRY]);
            if ReservedTag::Pointer.is(entry.tag) {
                // Found in probe order, which may be an older value of the name.
                return Err(SnapshotError::PointerTag { key: *key });
            }
            // A proved segment's entry places its payload inside the file (its layout was
            // checked once with every byte, or its row proves it): no check here.
            let bytes = &file[entry.at as usize..(entry.at + u64::from(entry.len)) as usize];
            return Ok(Some(Proved { bytes, proof: listed.proof }));
        }
        Ok(None)
    }

    /// The generation the snapshot lists.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// How many segments it lists: what a negative lookup probes.
    pub fn segments(&self) -> usize {
        self.listed.len()
    }

    /// The position of `key` in `run`: the store's one lookup (`format::find`), or the planted
    /// prefix-only search.
    fn find(&self, run: &[u8], key: &ContentId) -> Option<usize> {
        match self.plant {
            SnapshotPlant::ShortPrefix => find_by_prefix(run, key),
            SnapshotPlant::None | SnapshotPlant::CommitsFirst | SnapshotPlant::SkipsMapAll | SnapshotPlant::SeesLater | SnapshotPlant::ShortWatermark => format::find(run, key),
        }
    }
}

/// The indexes (in `plain`'s maps) of the generation's mapped, proved local segments of `space`,
/// in its probe order: the rows `Plain::find_in` probes for that namespace, in its order.
fn in_space<P: Allocator + Clone>(plain: &Plain<P>, space: Namespace) -> impl Iterator<Item = usize> + '_ {
    plain
        .view
        .iter()
        .filter(move |&&row| plain.row_namespace(row) == Some(space))
        .filter_map(move |&row| plain.mapped_row(row))
}

/// The planted bug [`SnapshotPlant::ShortPrefix`]: a bisection that compares the first eight
/// bytes of each key alone and takes the first entry whose prefix equals the key's.
fn find_by_prefix(run: &[u8], key: &ContentId) -> Option<usize> {
    let want = &key.0[..8];
    let (mut low, mut high) = (0usize, run.len() / ENTRY);
    while low < high {
        let mid = low + (high - low) / 2;
        match run[mid * ENTRY..mid * ENTRY + 8].cmp(want) {
            core::cmp::Ordering::Less => low = mid + 1,
            core::cmp::Ordering::Greater => high = mid,
            core::cmp::Ordering::Equal => return Some(mid),
        }
    }
    None
}

impl<D: Driver, A: Allocator + Clone> Lent<D, A> {
    /// A [`Snapshot`] of what is committed now, its list in `alloc`: the store made whole first
    /// ([`Lent::map_all`]; nothing when it is whole already), then every mapped segment of the
    /// generation listed. This command's puts not yet committed are not in it: commit first
    /// ([`Lent::commit`]; the engine commits after each phase's puts, then takes one).
    pub fn snapshot_in<B: Allocator>(&self, alloc: B) -> Result<Snapshot<B>, SnapshotError> {
        self.snapshot_planted(alloc, SnapshotPlant::None)
    }

    pub(crate) fn snapshot_planted<B: Allocator>(&self, alloc: B, plant: SnapshotPlant) -> Result<Snapshot<B>, SnapshotError> {
        if plant == SnapshotPlant::CommitsFirst {
            self.commit().map_err(|why| SnapshotError::Lent { doing: "committing the arena before the snapshot (planted)", why })?;
        }
        if plant != SnapshotPlant::SkipsMapAll {
            self.map_all().map_err(|why| SnapshotError::Lent { doing: "mapping every segment named", why })?;
        }
        // `Visibility::InArena`: the puts since the last take sealed, and what is published now
        // seen (nothing under `AfterCommit`, which has no shared arena).
        let view = self.seal_run().map_err(|why| SnapshotError::Lent { doing: "sealing the shared arena's run", why })?;
        let view = if plant == SnapshotPlant::ShortWatermark { view.map(ArenaView::short) } else { view };
        Ok(self.store_with(|store| Snapshot::of(store.core(), alloc, plant))?.with_arena(view))
    }
}

#[cfg(test)]
#[path = "snapshot_tests.rs"]
mod tests;
