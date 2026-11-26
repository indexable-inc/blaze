//! Records by key (drive.md, "The system side", "Store"; "D114 rulings", "The store under the
//! command"): ONE store a user, blaze-cas's plain store at `<home>/.blaze/`
//! [`plain_place::STORE_DIR`] (`v2/store/f5s3`: its forms name its folder, D160), read and
//! written by every checkout and worktree, records read IN PLACE ([`plain_place`]). A get is
//! a probe of the segments the head names, each mapped at its first probe; the request's first
//! pool task maps the rest ([`Store::map_all`]); a put is copied once into the command's arena;
//! the commit, the seal and a bounded compaction come after the answer is printed
//! ([`plain_place::after_answer`], called by main.rs's outer function).
//!
//! A key missed by the store is asked of a named origin when one is named; what comes back is
//! checked by the read path against the signed head's chain of digests, put, then answered.
//! What is not fetched is a typed miss: one line on stderr says what and why for each kind, and
//! the request computes the record locally. That is not a fallback (remote-read.md, "Not
//! fetched"): the local computation is the record's definition and the origin a cache of it.
//!
//! THE OLD HOME. The stand-in's folders (the home's `.blaze/v2/shared` and `.blaze/v2/own`, a
//! checkout's `.blaze/v2/packs`) are an old form: looked for once (a root records the look),
//! reported by name ([`old_home`]), never read, migrated or deleted; their records are made
//! again (rule 26).

use blaze_drive::profile::Profile;
use blaze_drive::record::Stored;
use blaze_drive::{Key, Kind, Name};
use blaze_ids::ContentId;

use core::fmt;
use core::time::Duration;
use core::sync::atomic::{AtomicU64, Ordering};

use blaze_cas::plain::{Lent, Liveness, Settings, Snapshot};
use blaze_core::Driver;
use blaze_own::{Alloc, Arc, Share};
use blaze_remote::PublicKey;
use blaze_remote::published::set::SetCheck;
use blaze_remote::read::{Answer, BatchError, BatchOutcome, BatchRead, Ed25519Keys, HeadCheck, Limits, Namespace, Object, Origin, OriginError, Reader, SignatureWhy, Unfetched, fetch};
use bstd::Stopped;
use bstd::clock::Clock;
use bstd::cx::Cx;
use bstd::fs::{Dir, Fs, FsError, FsKind, ReadWrite};
use std::alloc::{Allocator, Global};
use bstd::mem::Bytes;
use bstd::out::Out;
use bstd::path::Path;
use bstd::Deadline;
use bstd::net::{Addr, Net};
use bstd::url::Url;
use blaze_h1::Range;
use bstd::requests::{Finished, Http, HttpKind, NO_REDIRECTS, Request, Wire};
use bstd::roots::Roots;

use crate::CliError;

/// The command's store on blaze-cas's plain store: `Lent`, `PlainPlace`, the proof.
pub(crate) mod plain_place;
pub use plain_place::{Facts, Held, PlaceError, PlainPlace, StoreAlloc};

/// Selective fast `zstd` (`BZR1`) envelope for large decoded-on-read CAS records.
pub(crate) mod record_codec;

/// What keeps a record after its command: one typed root per lifetime class.
pub(crate) mod roots;

/// The published set: the toolchain root's walk, a publish to a folder origin, and the
/// rollback floor a home keeps for each set it reads.
pub(crate) mod published;

/// A fresh home takes a published set whole, once, before the command's first phase (D138).
pub(crate) mod take;

/// `blaze store publish <folder> --signer <key file>`: a trusted builder's (D136).
pub(crate) mod publish_command;

/// Whether this blaze is a trusted builder's (CI's: blaze-cli's `publish` feature, D136): its
/// store is opened as a trusted writer's, so a record every name of which is public (the
/// toolchain's, [`Sharing`]) is put `PUBLIC`, which `blaze store publish` lays out. A
/// developer's blaze puts every record `LOCAL`, and nothing it makes is ever published.
pub(crate) const TRUSTED_BUILDER: bool = cfg!(feature = "publish");

/// Answer of [`Store::get_committed`]: a committed segment borrow (`'m`), an uncommitted put
/// currently in this command's arena (to be read through a [`SharedSnapshot`] so the arena frees
/// on commit), or absent.
#[derive(Clone, Copy)]
pub enum Committed<'m> {
    Found(Held<'m>),
    InArena,
    Absent,
}

/// The seam the request reads and writes records through. `'m` is the borrow of the command's
/// [`Lent`]: a [`Held<'m>`] outlives every later get and put (blaze-cas plain/lent.rs).
pub trait Store<'m> {
    /// The committed records visible to one request's read-only workers. `None` is a typed
    /// refusal for a test or alternate store that cannot lend the immutable view.
    fn snapshot(&self) -> Result<Option<SharedSnapshot>, CliError> {
        Ok(None)
    }

    /// The drive can hand a body inputs still in its put arena. Seal those inputs before
    /// lending the round's read-only view to the workers.
    fn commit_for_body_reads(&mut self) -> Result<SharedSnapshot, CliError> {
        Err(CliError::Text { what: "this store cannot commit the body round's expansion records" })
    }

    /// The record at `key`, in place; `None` when there is none.
    fn get(&mut self, cx: &Cx, key: Key) -> Result<Option<Held<'m>>, CliError>;
    /// Committed-only in-place lookup: borrows a committed segment for `'m` without pinning any
    /// uncommitted arena. Returns [`Committed::InArena`] when `key` is an uncommitted put.
    fn get_committed(&mut self, cx: &Cx, key: Key) -> Result<Committed<'m>, CliError> {
        match self.get(cx, key)? {
            Some(held) => Ok(Committed::Found(held)),
            None => Ok(Committed::Absent),
        }
    }
    /// Non-lending existence check (uncommitted puts or committed segments).
    fn contains(&mut self, cx: &Cx, key: Key) -> Result<bool, CliError> {
        Ok(self.get(cx, key)?.is_some())
    }
    /// Number of commits completed through this store so far (used to drop stale snapshots).
    fn commits(&self) -> u32 {
        0
    }
    /// The record at `key` copied into `into`, for a reader whose bytes must live in its own
    /// frame (a pool job's); `false` when there is none.
    fn get_into(&mut self, cx: &Cx, key: Key, into: &mut Bytes) -> Result<bool, CliError> {
        match self.get(cx, key)? {
            Some(held) => {
                into.try_extend(held.bytes).map_err(|stopped| CliError::Store(StoreFailure::Copy(stopped)))?;
                Ok(true)
            }
            None => Ok(false),
        }
    }
    /// `bytes` at `key`. A key already held with equal bytes is left as it is (rule 25); other
    /// bytes are refused (rule 21).
    fn put(&mut self, cx: &Cx, key: Key, bytes: &[u8]) -> Result<(), CliError>;
    /// [`Store::put`] of the concatenation of `parts` (a record's head and payload), never
    /// joined first: the bytes go to the store from where their writer made them.
    fn put_parts(&mut self, cx: &Cx, key: Key, parts: &[&[u8]]) -> Result<(), CliError>;
    /// A record a task made beside its own (an expansion record: `tag`
    /// [`blaze_keyed::kind::ReservedTag::Expansion`]), keyed by its bytes and put beside its owner's record
    /// (right after it on the engine's path, right before it on the machine's: work.rs
    /// `PendingExtras`), whose parts are `owner` (as [`Store::put_parts`] took them): it goes to the
    /// namespace its owner goes to. Equal bytes already held under `key` are
    /// [`blaze_keyed::store::Put::Same`], whichever namespace holds them; other bytes are
    /// refused (rule 21).
    fn put_record(&mut self, cx: &Cx, key: Key, tag: blaze_keyed::kind::Tag, owner: &[&[u8]], bytes: &[u8]) -> Result<blaze_keyed::store::Put, CliError>;
    /// The keys of the records put beside `owner` ([`Store::put_record`]) filed as one index
    /// beside it (blaze-cas `Plain::index_beside`), so a root naming `owner` through an index
    /// keeps them too ([`CommandStore::pin_through`]); a store with no roots keeps nothing.
    fn index_beside(&mut self, cx: &Cx, owner: Key, keys: &[Key]) -> Result<(), CliError>;
    /// Every segment the head names mapped, and every parked one checked: asked once, when the
    /// request's first pool task is about to start (work.rs `run_batches`); later asks do
    /// nothing.
    fn map_all(&mut self, cx: &Cx) -> Result<(), CliError>;
    /// Once the request has its answer: the store's facts into `profile` (the commit itself
    /// comes after the answer is printed, [`plain_place::after_answer`]).
    fn after_answer(&mut self, cx: &Cx, profile: &mut Profile) -> Result<(), CliError>;
    /// The `CrateScope` subjects of crates whose records hold nothing of a project (the
    /// toolchain's), told by the names step once its world is made: a trusted builder puts
    /// their records in the public namespace.
    fn public_crates(&mut self, subjects: &[ContentId]);
    /// The `CrateScope` subjects of the toolchain's crates (a subset of the public ones), told
    /// by the names step with them: their records are kept under the toolchain's root, for
    /// every checkout on the toolchain (store/roots.rs `keep_toolchain`).
    fn toolchain_crates(&mut self, subjects: &[ContentId]);
    /// A record made of the toolchain alone, read or put this command (a side's library-features
    /// listing, scripts.rs `CommandFeatures`): kept under the toolchain's root with the
    /// toolchain's crates' records, by the answer's save ([`CommandStore::keep_toolchain`]),
    /// renewed with that root. A store with no roots keeps nothing.
    fn keep_with_toolchain(&mut self, key: Key) -> Result<(), CliError>;
    /// What this store's puts cost the process so far (rules 50, 51): the copies of records it
    /// made into the heap and the bytes of them it holds now, counted where it copies.
    fn kept(&self) -> StoreKept;
}

/// A verified, immutable view of the store's committed generation. The mappings are owned by the
/// snapshot and can be read concurrently by body workers without borrowing the mutable command
/// store.
pub(crate) struct StoreSnapshot {
    inner: Snapshot<blaze_mem::Kept>,
    expansion_demands: bstd::sync::CachePadded<AtomicU64>,
}

pub(crate) type SharedSnapshot = Arc<StoreSnapshot, blaze_mem::Kept>;

impl StoreSnapshot {
    /// A regular snapshot read. It is not an expansion-text demand and therefore does not enter
    /// the Bodies expansion diagnostic.
    pub(crate) fn get(&self, key: Key) -> Result<Option<&[u8]>, blaze_cas::plain::SnapshotError> {
        self.inner.get(&blaze_cas::Key(key.0))
    }

    /// The lazy expansion loader's record source. Every demand is counted for the Bodies stage's
    /// profile after every worker has joined. The phase owns its distinct-key set.
    pub(crate) fn expansion_get(&self, key: Key) -> Result<Option<&[u8]>, blaze_cas::plain::SnapshotError> {
        self.expansion_demands.fetch_add(1, Ordering::Relaxed);
        self.inner.get(&blaze_cas::Key(key.0))
    }

    pub(crate) fn expansion_demands(&self) -> u64 {
        self.expansion_demands.load(Ordering::Relaxed)
    }

}

/// Task scratch frame for CAS parallel CPU helpers (BLAKE3 subtrees, Bloom bucketing,
/// `TwoValues` probes).
const CAS_PAR_FRAME: usize = 64 << 10;

/// Store-tagged allocator for [`CxPar`] worker scratch vectors (`Send + Sync + Copy`, zero `Global`).
const CAS_PAR_ALLOC: blaze_mem::Kept = blaze_mem::Kept(blaze_mem::Tag::Store);

/// 100% safe work-stealing [`blaze_cas::plain::Par`] executor backed by `bstd`'s scoped task
/// pool (`cx.scope().may_overlap`) and a 128B-padded atomic stealing cursor
/// (`bstd::sync::CachePadded<AtomicUsize>`).
///
/// Falls back directly to an inline serial loop with zero scope or worker overhead when
/// `chunks <= 1` or `workers <= 1` (including simulated/single-worker contexts).
pub(crate) struct CxPar<'a> {
    cx: &'a Cx,
    workers: core::num::NonZeroU32,
}

impl<'a> CxPar<'a> {
    /// Creates a [`CxPar`] sized to `cx.workers()`.
    pub(crate) fn new(cx: &'a Cx) -> CxPar<'a> {
        let workers = core::num::NonZeroU32::new(cx.workers()).unwrap_or(core::num::NonZeroU32::MIN);
        CxPar { cx, workers }
    }

    /// Creates a [`CxPar`] capped to `min(workers, cx.workers().max(1))`.
    #[allow(dead_code, reason = "explicit worker cap constructor for bounded callers and tests")]
    pub(crate) fn with_workers(cx: &'a Cx, workers: core::num::NonZeroU32) -> CxPar<'a> {
        let capped = workers.get().min(cx.workers().max(1));
        let workers = core::num::NonZeroU32::new(capped).unwrap_or(core::num::NonZeroU32::MIN);
        CxPar { cx, workers }
    }
}

impl blaze_cas::plain::Par for CxPar<'_> {
    #[inline]
    fn workers(&self) -> usize {
        usize::try_from(self.workers.get()).unwrap_or(1)
    }

    fn map_range<R: Send, A: Allocator + Copy>(
        &self,
        count: usize,
        grain: usize,
        alloc: A,
        f: impl Fn(usize, usize) -> R + Sync,
    ) -> blaze_own::Vec<R, A> {
        if count == 0 {
            return blaze_own::Vec::new_in(alloc);
        }
        let step = grain.max(1);
        let chunks = count.div_ceil(step);
        let w = self.workers();
        if chunks <= 1 || w <= 1 {
            let mut out = blaze_own::Vec::with_capacity_in(chunks, alloc);
            let mut lo = 0usize;
            while lo < count {
                let hi = lo.saturating_add(step).min(count);
                out.push(f(lo, hi));
                lo = hi;
            }
            return out;
        }

        let worker_count = w.min(chunks);
        let helpers = worker_count.saturating_sub(1);
        let cursor = bstd::sync::CachePadded::new(core::sync::atomic::AtomicUsize::new(0));
        let mut slots: blaze_own::Vec<Option<R>, A> = blaze_own::Vec::with_capacity_in(chunks, alloc);
        for _ in 0..chunks {
            slots.push(None);
        }
        {
            let cursor_ref = &cursor;
            let f_ref = &f;
            let scope = self.cx.scope();
            scope.narrow_workers(self.workers, bstd::task::Class::Foreground);
            let mut handles = blaze_own::Vec::with_capacity_in(helpers, CAS_PAR_ALLOC);
            for _ in 0..helpers {
                handles.push(scope.may_overlap(
                    "cas: par map",
                    CAS_PAR_FRAME,
                    move |_cx: &Cx| -> Result<blaze_own::Vec<(usize, R), blaze_mem::Kept>, core::convert::Infallible> {
                        let mut local: blaze_own::Vec<(usize, R), blaze_mem::Kept> = blaze_own::Vec::new_in(CAS_PAR_ALLOC);
                        loop {
                            let idx = cursor_ref.fetch_add(1, Ordering::Relaxed);
                            if idx >= chunks {
                                break;
                            }
                            let lo = idx.saturating_mul(step);
                            let hi = lo.saturating_add(step).min(count);
                            local.push((idx, f_ref(lo, hi)));
                        }
                        Ok(local)
                    },
                ));
            }
            // Calling thread participates as worker 0.
            loop {
                let idx = cursor_ref.fetch_add(1, Ordering::Relaxed);
                if idx >= chunks {
                    break;
                }
                let lo = idx.saturating_mul(step);
                let hi = lo.saturating_add(step).min(count);
                if let Some(slot) = slots.get_mut(idx) {
                    *slot = Some(f_ref(lo, hi));
                }
            }
            for handle in handles.into_iter() {
                if let Ok(local) = handle.join(self.cx) {
                    for (idx, value) in local.into_iter() {
                        if let Some(slot) = slots.get_mut(idx) {
                            *slot = Some(value);
                        }
                    }
                }
            }
            let _ = scope.join(self.cx);
        }
        let mut out = blaze_own::Vec::with_capacity_in(chunks, alloc);
        for (idx, slot) in slots.into_iter().enumerate() {
            match slot {
                Some(value) => out.push(value),
                None => {
                    let lo = idx.saturating_mul(step);
                    let hi = lo.saturating_add(step).min(count);
                    out.push(f(lo, hi));
                }
            }
        }
        out
    }

    fn for_each_mut_chunks<T: Send>(
        &self,
        items: &mut [T],
        grain: usize,
        f: impl Fn(usize, &mut [T]) + Sync,
    ) {
        if items.is_empty() {
            return;
        }
        let step = grain.max(1);
        let chunks = items.len().div_ceil(step);
        let w = self.workers();
        if chunks <= 1 || w <= 1 {
            for (idx, chunk) in items.chunks_mut(step).enumerate() {
                f(idx.saturating_mul(step), chunk);
            }
            return;
        }

        let worker_count = w.min(chunks);
        let base_chunks = chunks / worker_count;
        let extra_chunks = chunks % worker_count;
        let f_ref = &f;
        let scope = self.cx.scope();
        scope.narrow_workers(self.workers, bstd::task::Class::Foreground);
        let mut handles = blaze_own::Vec::with_capacity_in(worker_count.saturating_sub(1), CAS_PAR_ALLOC);
        let mut rest = items;
        let mut offset = 0usize;
        // Worker 0 slice runs inline on the calling thread; helpers take disjoint slices via split_at_mut
        // with zero locks and zero shared chunk-table allocation.
        let first_len = base_chunks.saturating_add(usize::from(extra_chunks > 0)).saturating_mul(step).min(rest.len());
        let (first_slice, after_first) = rest.split_at_mut(first_len);
        rest = after_first;
        offset = offset.saturating_add(first_len);
        for worker in 1..worker_count {
            let my_chunks = base_chunks.saturating_add(usize::from(worker < extra_chunks));
            let my_len = my_chunks.saturating_mul(step).min(rest.len());
            let (mine, tail) = rest.split_at_mut(my_len);
            let my_offset = offset;
            rest = tail;
            offset = offset.saturating_add(my_len);
            handles.push(scope.may_overlap("cas: par chunks", CAS_PAR_FRAME, move |_cx: &Cx| -> Result<(), core::convert::Infallible> {
                for (local_idx, chunk) in mine.chunks_mut(step).enumerate() {
                    f_ref(my_offset.saturating_add(local_idx.saturating_mul(step)), chunk);
                }
                Ok(())
            }));
        }
        for (local_idx, chunk) in first_slice.chunks_mut(step).enumerate() {
            f_ref(local_idx.saturating_mul(step), chunk);
        }
        for handle in handles.into_iter() {
            let _ = handle.join(self.cx);
        }
        let _ = scope.join(self.cx);
    }
}

/// The bytes of records a [`Store`] copied into this process's heap, and holds now (a gauge):
/// read at each phase's merge by engine_store.rs `Ledger`.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct StoreKept {
    /// Puts copied into the heap, ever.
    pub copies: u64,
    pub copy_bytes: u64,
    /// Bytes of those copies alive now (outside a mapping).
    pub held_bytes: u64,
}

/// blaze's folder in the user's home, made by the first file made in it.
pub(crate) const HOME_FOLDER: &str = ".blaze";

/// The stand-in's folders under blaze's folder in the home: an old home when either is there.
pub(crate) const OLD_HOME: [&str; 2] = ["v2/shared", "v2/own"];
/// The stand-in's forms under a checkout: its packs folder and its `last` pointer file (the
/// last answer and leaf table are now a root of the one store, drive.md ruling 3).
/// Named from clean.rs's one list of a checkout's entries, so `blaze clean` removes them.
pub(crate) const OLD_CHECKOUT: [&str; 2] = [crate::clean::CheckoutEntry::Packs.path(), crate::clean::CheckoutEntry::At.path()];
/// The roots that say the old forms were looked for: the home's once, each checkout's once
/// (its name ends in the digest of the checkout's path, drive.md ruling 3).
const OLD_HOME_LOOKED: &str = "blaze.old-home-looked";
const OLD_CHECKOUT_LOOKED: &str = "blaze.old-packs-looked.";

/// Whether compaction runs after the answer (past zero: it does): the first slice folded into
/// the answer's commit, which restores blaze-cas's tier invariant for the namespace it merges
/// however far behind it is (`plain::core::Tiers`, the tiered merge), then one more slice a
/// namespace still behind while blaze-cas `Plain::cleanup_goes_on` holds (its work budget,
/// [`plain_place::after_answer`]).
pub(crate) const AFTER_ANSWER_SLICES: u32 = 1;

/// Why the command's store stopped it, by name (rule 19).
#[derive(Debug)]
pub enum StoreFailure {
    /// The environment names no home: the store is under it, and there is no other.
    NoHome,
    /// The home could not be opened, bstd saying why (and naming it).
    Home(bstd::fs::HomeError),
    /// The plain store refused.
    Place(PlaceError),
    /// A record could not be copied into a reader's frame.
    Copy(Stopped),
    /// Looking for an old form's folder failed.
    Look { folder: &'static str, error: FsError },
    /// The store root holds more than `most` stores of other forms (D160, store/forms.rs
    /// `OTHER_FORMS_MOST`): refused by the count, never cut.
    OtherForms { most: usize },
    /// A put of other bytes under a key the store holds (rule 21): the store's refusal, and
    /// the two records opened, so the refusal names the record and where they differ.
    TwoValues { key: Key, lent: blaze_cas::plain::LentError, clash: Clash },
    /// A class root of store/roots.rs could not be set (the toolchain's, from the engine's answer).
    Root(roots::RootError),
    /// The published set (store/published.rs): its walk, its floor, or its folder's write.
    Published(published::PublishedError),
    /// A published set named by `--origin` could not be taken (store/take.rs): the command
    /// stops, and nothing is made in its place (rule 3).
    Take(take::TakeError),
}

/// A drive record's form, as [`record::open`](blaze_drive::record::open) reads it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Form {
    /// A whole record (at a task key with no reads, or at a value key).
    Whole,
    /// A shape: the read names of the records under one task key.
    Shape,
    /// Bytes that are no drive record (another writer's under a drive key).
    NotARecord,
}

impl Form {
    fn word(self) -> &'static str {
        match self {
            Form::Whole => "whole record",
            Form::Shape => "shape",
            Form::NotARecord => "bytes that are no drive record",
        }
    }
}

/// Two records under one drive key, opened (rule 19): the record's name, each side's form, the
/// first field they differ in, and the first names that differ when the field is a list of
/// reads (a shape's read names, a whole record's reads or parts), or the first differing byte
/// of the payload. `label` is the crate or file the name is of, filled by the command where it
/// knows the crate list.
#[derive(Debug)]
pub struct Clash {
    pub name: Option<Name>,
    pub held: Form,
    pub put: Form,
    /// The first field that differs: `form`, `name`, `source`, `out`, `split`, `reads`,
    /// `cells`, `parts`, `paths`, `findings` or `payload`; `none` when the records are equal
    /// (the store refused on its tag or namespace).
    pub field: &'static str,
    /// The first differing entry of a list field, held then put (`None` past a side's end).
    pub names: Option<(Option<Name>, Option<Name>)>,
    /// The first differing payload byte.
    pub payload_at: Option<usize>,
    /// When the read lists differ: the names only the held record read and only the put one
    /// did, so a read list that is not a function of the key's inputs shows which reads moved.
    pub apart: Option<Apart>,
    /// The file a whole record's own path entry names.
    pub path: Option<blaze_own::String<Global>>,
    pub label: Option<blaze_own::String<Global>>,
}

/// How many names of each side [`Apart`] keeps.
pub const APART_NAMES: usize = 16;

/// The read names only one side of a [`Clash`] holds: up to [`APART_NAMES`] a side, in name
/// order, and how many more.
#[derive(Debug)]
pub struct Apart {
    pub held: blaze_own::Vec<Name, Global>,
    pub put: blaze_own::Vec<Name, Global>,
    pub more_held: usize,
    pub more_put: usize,
}

impl Apart {
    /// `held` and `put`, each sorted by name (a record's reads are): one merge.
    fn of(held: impl Iterator<Item = Name>, put: impl Iterator<Item = Name>) -> Apart {
        let mut apart = Apart { held: blaze_own::Vec::new_in(Global), put: blaze_own::Vec::new_in(Global), more_held: 0, more_put: 0 };
        let keep = |list: &mut blaze_own::Vec<Name, Global>, more: &mut usize, name: Name| match list.len() < APART_NAMES {
            true => list.push(name),
            false => *more += 1,
        };
        let (mut held, mut put) = (held.peekable(), put.peekable());
        loop {
            match (held.peek().copied(), put.peek().copied()) {
                (None, None) => return apart,
                (Some(h), Some(p)) if h == p => {
                    held.next();
                    put.next();
                }
                (Some(h), Some(p)) if h < p => {
                    held.next();
                    keep(&mut apart.held, &mut apart.more_held, h);
                }
                (Some(h), None) => {
                    held.next();
                    keep(&mut apart.held, &mut apart.more_held, h);
                }
                (_, Some(p)) => {
                    put.next();
                    keep(&mut apart.put, &mut apart.more_put, p);
                }
            }
        }
    }
}

impl fmt::Display for Apart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let side = |f: &mut fmt::Formatter<'_>, what: &str, names: &[Name], more: usize| -> fmt::Result {
            write!(f, "; read only by the {what} record ({}):", names.len() + more)?;
            if names.is_empty() {
                f.write_str(" none")?;
            }
            for name in names.iter() {
                // A name prints as its kind and its subject's first bytes (key.rs `Display`).
                write!(f, " {name}")?;
            }
            if more > 0 {
                write!(f, " and {more} more")?;
            }
            Ok(())
        };
        side(f, "held", &self.held, self.more_held)?;
        side(f, "put", &self.put, self.more_put)
    }
}

/// The first place two lists differ: their entries there (`None` past a side's end).
fn first_apart<T: PartialEq>(mut a: impl Iterator<Item = T>, mut b: impl Iterator<Item = T>) -> Option<(Option<T>, Option<T>)> {
    loop {
        match (a.next(), b.next()) {
            (None, None) => return None,
            (x, y) if x == y => {}
            (x, y) => return Some((x, y)),
        }
    }
}

impl Clash {
    /// The records `held` (what the key holds; `None` when the store lends nothing there) and
    /// `put` (the refused bytes), opened and compared field by field.
    pub fn of(held: Option<&[u8]>, put: &[u8]) -> Clash {
        use blaze_drive::WireError;
        use blaze_drive::record::{Stored, open};
        fn form(stored: &Result<Stored<'_>, WireError>) -> Form {
            match stored {
                Ok(Stored::Whole(_)) => Form::Whole,
                Ok(Stored::Shape(_)) => Form::Shape,
                Err(_) => Form::NotARecord,
            }
        }
        fn name_of(stored: &Result<Stored<'_>, WireError>) -> Option<Name> {
            match stored {
                Ok(Stored::Whole(view)) => Some(view.name),
                Ok(Stored::Shape(view)) => Some(view.name),
                Err(_) => None,
            }
        }
        let held = held.map(open);
        let new = open(put);
        let mut clash = Clash {
            name: name_of(&new).or_else(|| held.as_ref().and_then(name_of)),
            held: held.as_ref().map_or(Form::NotARecord, form),
            put: form(&new),
            field: "form",
            names: None,
            payload_at: None,
            apart: None,
            path: None,
            label: None,
        };
        if let Ok(Stored::Whole(view)) = &new {
            if let Some(own) = view.paths().find(|path| path.name == view.name) {
                let mut path = blaze_own::String::new_in(Global);
                path.push_str(core::str::from_utf8(own.path).unwrap_or("a path not UTF-8"));
                clash.path = Some(path);
            }
        }
        match (held, new) {
            (Some(Ok(Stored::Whole(a))), Ok(Stored::Whole(b))) => {
                clash.field = if a.name != b.name {
                    "name"
                } else if a.source != b.source {
                    "source"
                } else if a.out != b.out {
                    "out"
                } else if a.split != b.split {
                    "split"
                } else if let Some((x, y)) = first_apart(a.reads(), b.reads()) {
                    clash.names = Some((x.map(|(name, _)| name), y.map(|(name, _)| name)));
                    clash.apart = Some(Apart::of(a.reads().map(|(name, _)| name), b.reads().map(|(name, _)| name)));
                    "reads"
                } else if let Some((x, y)) = first_apart(a.cells(), b.cells()) {
                    clash.names = Some((x.map(|read| read.name), y.map(|read| read.name)));
                    "cells"
                } else if let Some((x, y)) = first_apart(a.parts(), b.parts()) {
                    clash.names = Some((x.map(|part| part.name), y.map(|part| part.name)));
                    "parts"
                } else if let Some((x, y)) = first_apart(a.paths(), b.paths()) {
                    clash.names = Some((x.map(|path| path.name), y.map(|path| path.name)));
                    "paths"
                } else if first_apart(a.findings(), b.findings()).is_some() {
                    "findings"
                } else if let Some(at) = blaze_cas::plain::first_difference(a.payload, b.payload) {
                    clash.payload_at = Some(at);
                    "payload"
                } else {
                    "none"
                };
            }
            (Some(Ok(Stored::Shape(a))), Ok(Stored::Shape(b))) => {
                clash.field = if a.name != b.name {
                    "name"
                } else if let Some(apart) = first_apart(a.names(), b.names()) {
                    clash.names = Some(apart);
                    clash.apart = Some(Apart::of(a.names(), b.names()));
                    "reads"
                } else if let Some((x, y)) = first_apart(a.cells(), b.cells()) {
                    clash.names = Some((x.map(|(name, _)| name), y.map(|(name, _)| name)));
                    "cells"
                } else {
                    "none"
                };
            }
            _ => {}
        }
        clash
    }
}

impl fmt::Display for Clash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.write_subject(f)?;
        f.write_str(": ")?;
        self.write_detail(f)
    }
}

impl Clash {
    /// The record's name as the drive prints it, the crate it is of and its own file.
    fn write_subject(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.name {
            Some(name) => write!(f, "{name}")?,
            None => f.write_str("a record of no drive name")?,
        }
        if let Some(label) = &self.label {
            write!(f, " ({label})")?;
        }
        if let Some(path) = &self.path {
            write!(f, " ({path})")?;
        }
        Ok(())
    }

    /// Each side's form, the first field they differ in, and where.
    fn write_detail(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "held {}, put {}", self.held.word(), self.put.word())?;
        match self.field {
            "none" => f.write_str(", equal fields")?,
            field => write!(f, ", first differing in {field}")?,
        }
        if let Some((held, put)) = self.names {
            let side = |f: &mut fmt::Formatter<'_>, name: Option<Name>| match name {
                Some(name) => write!(f, "{name}"),
                None => f.write_str("(none)"),
            };
            f.write_str(" (held ")?;
            side(f, held)?;
            f.write_str(", put ")?;
            side(f, put)?;
            f.write_str(")")?;
        }
        if let Some(at) = self.payload_at {
            write!(f, " at payload byte {at}")?;
        }
        if let Some(apart) = &self.apart {
            write!(f, "{apart}")?;
        }
        Ok(())
    }
}

impl fmt::Display for StoreFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreFailure::NoHome => write!(f, "HOME is not set: blaze's one store is under the home's .blaze/{} (rule: store.rs)", plain_place::STORE_DIR),
            StoreFailure::OtherForms { most } => write!(
                f,
                "the home's .blaze/{} holds more than {most} stores of other forms; blaze lists at most {most} (rule 18): remove them with `blaze clean`",
                plain_place::STORE_ROOT
            ),
            StoreFailure::Home(error) => write!(f, "the home: {error}"),
            StoreFailure::Place(error) => write!(f, "{error}"),
            StoreFailure::Copy(stopped) => write!(f, "a record read from the store: {stopped}"),
            StoreFailure::Look { folder, error } => write!(f, "looking for the old store folder {folder}: {error}"),
            StoreFailure::Root(error) => write!(f, "{error}"),
            StoreFailure::Published(error) => write!(f, "{error}"),
            StoreFailure::Take(error) => write!(f, "{error}"),
            StoreFailure::TwoValues { key, lent, clash } => {
                write!(f, "the store under the home's .blaze/{}: two values under one key {key}: the record of ", plain_place::STORE_DIR)?;
                clash.write_subject(f)?;
                // Whether `blaze clean --store` lets the user go on: only when the held value is
                // a committed one (an earlier command's); two values made by this one command
                // come back after a clean.
                use blaze_keyed::store::Holder;
                let committed = match lent {
                    blaze_cas::plain::LentError::TwoValues { two, .. } => {
                        let held = match two.held {
                            Holder::ThisProcess => "put earlier by this command",
                            Holder::Committed => "committed",
                        };
                        match two.len {
                            Some(len) => write!(f, " made now ({len} bytes)")?,
                            None => f.write_str(" asked now")?,
                        }
                        write!(f, " differs from the one the store holds ({} bytes, {held})", two.held_len)?;
                        match two.differs_at {
                            Some(at) => write!(f, ", first at byte {at}")?,
                            None => f.write_str(", with equal bytes under another kind or namespace")?,
                        }
                        two.held == Holder::Committed
                    }
                    other => {
                        write!(f, " was refused: {other}")?;
                        true
                    }
                };
                f.write_str(" (")?;
                clash.write_detail(f)?;
                f.write_str(") (rule 21: its key misses an input of its value). This is a bug in blaze: please report this line.")?;
                match committed {
                    true => f.write_str(" To go on: `blaze clean --store`, then run the command again."),
                    false => f.write_str(" Both values were made by this command, so `blaze clean --store` does not get past it."),
                }
            }
        }
    }
}

impl From<PlaceError> for CliError {
    fn from(error: PlaceError) -> CliError {
        CliError::Store(StoreFailure::Place(error))
    }
}

/// blaze's folder in the home, passed in from the command's start (rule 75) and reached
/// through the effect layer as a second tree ([`Fs::home`]), wherever it is.
pub struct Home {
    dir: Dir<ReadWrite>,
    bg_gc: crate::gc::BgGc,
}

impl Home {
    /// From `HOME` as main.rs read it (absolute). No home is a typed refusal: the one store is
    /// under it.
    pub fn of(cx: &Cx, fs: &Fs, home: Option<&str>) -> Result<Home, CliError> {
        let Some(home) = home else { return Err(CliError::Store(StoreFailure::NoHome)) };
        match fs.home(cx, home, &Path::new(HOME_FOLDER)?) {
            Ok(dir) => Ok(Home { dir, bg_gc: crate::gc::BgGc::None }),
            Err(error) => Err(CliError::Store(StoreFailure::Home(error))),
        }
    }

    pub fn dir(&self) -> &Dir<ReadWrite> {
        &self.dir
    }

    /// The command's store, opened lazily on the world's driver lent to [`plain_place::STORE_DIR`]
    /// (one driver a process, drive.md ruling 5): nothing is mapped until a get probes.
    pub fn open_store(&self, settings: &Settings, facts: Facts, alloc: StoreAlloc) -> Result<Lent<bstd::io::LentDriver, StoreAlloc>, CliError> {
        let driver = self.dir.lend(&Path::new(plain_place::STORE_DIR)?);
        let dir = driver.dir().share();
        let lent = plain_place::open(driver, plain_place::config(dir, settings, facts, alloc), alloc)?;
        lent.set_commit_durability(default_local_durability(settings));
        Ok(lent)
    }

    /// Parallel twin of [`Home::open_store`]: verifies large earlier-boot segments through [`CxPar`].
    pub fn open_store_par(&self, cx: &Cx, settings: &Settings, facts: Facts, alloc: StoreAlloc) -> Result<Lent<bstd::io::LentDriver, StoreAlloc>, CliError> {
        let driver = self.dir.lend(&Path::new(plain_place::STORE_DIR)?);
        let dir = driver.dir().share();
        let par = CxPar::new(cx);
        let lent = plain_place::open_par(driver, plain_place::config(dir, settings, facts, alloc), alloc, &par)?;
        lent.set_commit_durability(default_local_durability(settings));
        Ok(lent)
    }

    /// Opens the store with an explicit [`Liveness`] policy (used by `blaze gc` to sweep
    /// unreachable records with [`Liveness::Reached`]).
    pub(crate) fn open_store_with_liveness(&self, settings: &Settings, facts: Facts, liveness: Liveness, alloc: StoreAlloc) -> Result<Lent<bstd::io::LentDriver, StoreAlloc>, CliError> {
        let driver = self.dir.lend(&Path::new(plain_place::STORE_DIR)?);
        let dir = driver.dir().share();
        let mut cfg = plain_place::config(dir, settings, facts, alloc);
        cfg.liveness = liveness;
        let lent = plain_place::open(driver, cfg, alloc)?;
        lent.set_commit_durability(default_local_durability(settings));
        Ok(lent)
    }
}

/// Default commit durability for a local rebuildable CLI store:
/// - `settings.durable == false` keeps [`blaze_cas::plain::CommitDurability::Unsynced`] (zero
///   foreground barriers/syncs; background GC seals segments asynchronously and post-crash opens
///   verify unsealed segments whole on first read).
/// - `settings.durable == true` defaults to [`blaze_cas::plain::CommitDurability::Barrier`]
///   (`F_BARRIERFSYNC` on macOS, `fdatasync` on Linux: ordered crash-consistent commits with zero
///   `F_FULLFSYNC`, proven across power cuts in `blaze-cas` DST
///   `barrier_ordered_commits_cut_full_syncs_and_survive_power_cuts_at_every_step`). Callers
///   requiring `F_FULLFSYNC` can explicitly opt in via `lent.set_commit_durability(Full)`.
const fn default_local_durability(settings: &Settings) -> blaze_cas::plain::CommitDurability {
    if settings.durable {
        blaze_cas::plain::CommitDurability::Barrier
    } else {
        blaze_cas::plain::CommitDurability::Unsynced
    }
}

/// The old forms' folders, each reported by name once (one line a folder), never read, migrated
/// or deleted. Looked for once in the home's life and once in each checkout's: the look is
/// recorded as a root, committed with this command's records after the answer.
pub fn old_home<D: Driver>(cx: &Cx, home: &Home, root: &Dir<ReadWrite>, checkout: &str, lent: &Lent<D, StoreAlloc>, err: &Out) -> Result<u32, CliError> {
    let mut checkout_root = std::string::String::with_capacity(OLD_CHECKOUT_LOOKED.len() + 16);
    checkout_root.push_str(OLD_CHECKOUT_LOOKED);
    for byte in &ContentId::of("blaze-cli old packs look", checkout.as_bytes()).0[..8] {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        checkout_root.push(HEX[usize::from(byte >> 4)] as char);
        checkout_root.push(HEX[usize::from(byte & 15)] as char);
    }
    let mut found = 0u32;
    let mut items: std::vec::Vec<std::string::String, Global> = std::vec::Vec::new_in(Global);
    if !lent.has_root(OLD_HOME_LOOKED) {
        for folder in OLD_HOME {
            if home.dir.stat(cx, &Path::new(folder)?).map_err(|error| CliError::Store(StoreFailure::Look { folder, error }))?.is_some() {
                items.push(std::format!("~/.blaze/{folder}"));
                found += 1;
            }
        }
        lent.root(OLD_HOME_LOOKED, &[]).map_err(|error| CliError::Store(StoreFailure::Place(PlaceError::Lent(error))))?;
    }
    if !lent.has_root(&checkout_root) {
        for old in OLD_CHECKOUT {
            if root.stat(cx, &Path::new(old)?).map_err(|error| CliError::Store(StoreFailure::Look { folder: old, error }))?.is_some() {
                items.push(std::string::String::from(old));
                found += 1;
            }
        }
        lent.root(&checkout_root, &[]).map_err(|error| CliError::Store(StoreFailure::Place(PlaceError::Lent(error))))?;
    }
    if !items.is_empty() {
        let noun = if items.len() == 1 { "folder" } else { "folders" };
        struct StackBuf {
            buf: [u8; 96],
            len: usize,
        }
        impl core::fmt::Write for StackBuf {
            fn write_str(&mut self, s: &str) -> core::fmt::Result {
                let bytes = s.as_bytes();
                let take = bytes.len().min(self.buf.len().saturating_sub(self.len));
                if let (Some(dst), Some(src)) = (self.buf.get_mut(self.len..self.len + take), bytes.get(..take)) {
                    dst.copy_from_slice(src);
                    self.len += take;
                }
                Ok(())
            }
        }
        let mut sb = StackBuf { buf: [0u8; 96], len: 0 };
        let _ = core::fmt::write(&mut sb, format_args!("{} old store {noun} unused by this blaze", items.len()));
        let headline = core::str::from_utf8(sb.buf.get(..sb.len).unwrap_or(&[])).unwrap_or("");
        let rows: std::vec::Vec<(&str, &str), Global> = items.iter().map(|p| (p.as_str(), "unused")).collect();
        let rendered = blaze_report::ui::Theme::PLAIN.notice_group(
            "note",
            blaze_report::palette::Token::Warning,
            headline,
            &rows,
            Some("delete when no older blaze needs them; records are rebuilt in the active store"),
        );
        for line in rendered.lines() {
            err.line(cx, format_args!("{line}"))?;
        }
    }
    Ok(found)
}

/// Where records go (drive.md ruling 2): the one seam where the choice is made (rule 78). The
/// command's is [`ToolchainCrates`].
pub trait Sharing {
    /// The crates whose records hold nothing of a project, by their `CrateScope` subjects.
    fn add_public(&mut self, subjects: &[ContentId]);
    /// Whether the record about to be put, the concatenation of `parts` (one slice, or a whole
    /// record's head and payload), holds only public names: it goes to the public namespace
    /// when this machine is a trusted builder, else to `LOCAL`.
    fn all_names_public(&self, parts: &[&[u8]]) -> bool;
}

/// A drive record's names are all public when its name and every name it reads are on a
/// POSITIVE LIST BY SUBJECT (main's ruling, D115): `add_public` takes `CrateScope` subjects and
/// `Script` subjects; the cli adds a script's subject only when its crate is public (the
/// toolchain's, a checksummed registry archive's: target.md, "Namespaces") AND it is placed on
/// the wasm path, so a native script, and every record reading it, stays LOCAL (forgetting one
/// makes it local: the safe direction). Sharing never reads how a script ran. A `Fetch` record
/// stands for the home's sources folder (fetch.rs `SOURCES`): this machine's, never public
/// (D78). Anything not a drive record (a certificate, a root, a page) is not public.
pub struct ToolchainCrates {
    /// Sorted, without repeats.
    subjects: std::vec::Vec<ContentId, Global>,
}

impl ToolchainCrates {
    pub fn new() -> ToolchainCrates {
        ToolchainCrates { subjects: std::vec::Vec::new_in(Global) }
    }
}

impl Sharing for ToolchainCrates {
    fn add_public(&mut self, subjects: &[ContentId]) {
        self.subjects.extend_from_slice(subjects);
        self.subjects.sort_unstable();
        self.subjects.dedup();
    }

    fn all_names_public(&self, parts: &[&[u8]]) -> bool {
        let public = |name: Name| matches!(name.kind, Kind::CrateScope | Kind::Script | Kind::ImplBucket | Kind::Signature) && self.subjects.binary_search(&name.subject).is_ok();
        // One record whole, or a whole record's head and payload (engine_store.rs `put_all`);
        // anything else is not a drive record.
        let opened = match parts {
            [bytes] => blaze_drive::record::open(bytes),
            [head, payload] => blaze_drive::record::open_parts(head, payload),
            _ => return false,
        };
        let (name, all_public) = match opened {
            // A cell read names its record as a whole read does, so both must be public.
            Ok(Stored::Whole(view)) => (
                view.name,
                public(view.name) && view.reads().all(|(name, _)| public(name)) && view.cells().all(|read| public(read.name)),
            ),
            Ok(Stored::Shape(view)) => (view.name, public(view.name) && view.names().all(public) && view.cells().all(|(name, _)| public(name))),
            // Not a drive record (a certificate, a root, a page): this machine's.
            Err(_) => return false,
        };
        match (name.kind, all_public) {
            // Fetched and adopted sources are this machine's (D78): never the shared namespace.
            (Kind::Fetch, _) => false,
            (_, all_public) => all_public,
        }
    }
}

/// The command's store: the plain store's handle, where records go, and a named origin.
pub struct CommandStore<'m, 'r, D: Driver, S: Sharing = ToolchainCrates> {
    place: PlainPlace<'m, D>,
    sharing: S,
    /// The toolchain's crates by their `CrateScope` subjects, sorted ([`Store::toolchain_crates`]):
    /// their records go under the toolchain's root ([`CommandStore::keep_toolchain`]).
    toolchain: std::vec::Vec<ContentId, Global>,
    /// The records of the toolchain alone this command read or put ([`Store::keep_with_toolchain`]),
    /// in the store's allocator: named by the toolchain's root at the answer's save.
    toolchain_records: std::vec::Vec<Key, StoreAlloc>,
    remote: Option<Remote<'r>>,
    mapped_all: bool,
    /// The open's time, for the profile and `--stats`.
    open_ns: u64,
}

impl<'m, 'r, D: Driver> CommandStore<'m, 'r, D, ToolchainCrates> {
    /// The handle over `lent`, opened in `open_ns`, then `origin` when one is named. Past
    /// the bound blaze-cas's tiered merge keeps after a cleanup (`Lent::named_most`: 49 a
    /// namespace) segments named, one line says compaction is behind (a crashed command's
    /// commits, or a trusted writer's namespaces waiting for their slices).
    #[expect(clippy::too_many_arguments, reason = "the store's edge: each argument one handle or fact of it")]
    pub fn new(cx: &Cx, lent: &'m Lent<D, StoreAlloc>, public_writer: bool, open_ns: u64, origin: Option<&'r NamedOrigin>, clock: Clock, err: &'r Out, alloc: Alloc<'r>) -> Result<CommandStore<'m, 'r, D>, CliError> {
        let remote = origin.map(|named| Remote {
            named,
            // The prefix on (published.md): a batch's head, manifest and summaries in one read,
            // so three waves. blaze-h1's `Response::ranged` takes the 206 that ends at the
            // object's end for an ask past it (blaze-h1 tests.rs), so HTTP reads it too.
            reader: Reader::new_in(Namespace::PUBLIC, &named.check, Limits::PUBLISHED, alloc),
            clock,
            err,
            alloc,
            spent_ns: 0,
            said: 0,
            kept: 0,
            kept_bytes: 0,
            floored: false,
        });
        let (_, named) = lent.mapped_of_named();
        let most = lent.named_most();
        if named > most {
            err.line(cx, format_args!("blaze: the store under the home's .blaze/{} names {named} segments, past {most}: its compaction is behind", plain_place::STORE_DIR))?;
        }
        // A trusted builder's blaze writes the public class `PUBLIC` whoever calls ([`TRUSTED_BUILDER`]).
        Ok(CommandStore { place: PlainPlace::new(lent, public_writer || TRUSTED_BUILDER), sharing: ToolchainCrates::new(), toolchain: std::vec::Vec::new_in(Global), toolchain_records: std::vec::Vec::new_in(*lent.alloc()), remote, mapped_all: false, open_ns })
    }
}

/// The command's store: the plain store on the world's lent driver.
pub type CommandLent = Lent<bstd::io::LentDriver, StoreAlloc>;

/// The one store as the command's function holds it, from before the request to after the
/// answer is printed.
pub struct Opened {
    pub home: Home,
    pub lent: CommandLent,
    /// The open's nanoseconds (the profile's `store open` line).
    pub open_ns: u64,
}

/// The one store a user, opened as a command opens it (drive.md, "The store under the
/// command"): the home from `HOME` (none: the typed refusal), the boot fact (none: one line,
/// and every segment of an earlier command is checked once), the nonce from the granted `Rng`;
/// the open maps nothing; the store lives in `heap`, the process's heap `main` was handed. Then
/// the old forms are reported once ([`old_home`]).
#[expect(clippy::too_many_arguments, reason = "the store's edge: each argument one handle or fact of it")]
pub fn open_for_command(
    cx: &Cx,
    fs: &bstd::fs::Fs,
    host: &bstd::host::Host,
    rng: &mut bstd::rng::Rng,
    home_str: Option<&str>,
    max_store_bytes: Option<u64>,
    checkout_root: &Dir<ReadWrite>,
    checkout: &str,
    clock: &Clock,
    err: &Out,
    heap: &'static blaze_own::Heap,
) -> Result<Opened, CliError> {
    let mut home = Home::of(cx, fs, home_str)?;
    home.bg_gc = crate::gc::BgGc::detached(cx, host, home_str, max_store_bytes, heap.alloc());
    let boot = match host.boot() {
        Ok(boot) => Some(boot_fact(&boot)),
        Err(why) => {
            err.line(cx, format_args!("blaze: no boot fact ({why}): each segment an earlier command wrote is checked once"))?;
            None
        }
    };
    let nonce = nonce(rng);
    let ns = |clock: &Clock| u64::try_from(clock.now().since_start().as_nanos()).unwrap_or(u64::MAX);
    let began = ns(clock);
    // A trusted builder's store may name the public namespace ([`TRUSTED_BUILDER`]); a
    // developer's only its own. Budget defaults to 32 GiB unless overridden via env.
    let settings = Settings { max_store_bytes, trusted_writer: TRUSTED_BUILDER, ..Settings::default() };
    let lent = home.open_store_par(cx, &settings, Facts { boot, nonce }, heap.alloc())?;
    if !cx.is_simulated() {
        lent.set_pid(host.pid());
    }
    let open_ns = ns(clock).saturating_sub(began);
    old_home(cx, &home, checkout_root, checkout, &lent, err)?;
    // Stores of other forms beside this blaze's (D160): said once, never read.
    plain_place::forms::note_once(cx, &home.dir, &lent, err)?;
    Ok(Opened { home, lent, open_ns })
}

/// The store's boot fact ([`Facts::boot`]) of the machine's boot: its first 8 bytes. A power
/// cut is a new boot, so a segment written before it is checked once after it (a torn,
/// unsynced segment is then lost by name, never trusted): a test that opens a store after a
/// simulated crash takes its boot from here too, never a constant.
pub(crate) fn boot_fact(boot: &blaze_core::Boot) -> u64 {
    let mut first = [0u8; 8];
    first.copy_from_slice(&boot.0[..8]);
    u64::from_le_bytes(first)
}

/// How [`after_answer_printed_with`] disposes of the closed store once commits and lease release
/// are complete.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StoreTeardown {
    /// Normal `Drop`: unmap read-only segments and free store tables. Always used in tests,
    /// simulated worlds, multi-check commands (`--fix`), and whenever background GC is spawned.
    Drop,
    /// On a one-shot native CLI process about to exit (after all commits/unlocks and when no
    /// background GC is pending), skip tearing down read-only segment mappings and store tables
    /// via `core::mem::forget` so the kernel reclaims them in one shot at process exit.
    ForgetForExit,
}

/// After the answer is printed, once every handle over `lent` is gone: the ONE commit of this
/// command's records and roots ([`plain_place::after_answer`]), and with `stats` its line on
/// `err`: the commit's time, and the time from the answer's last byte to the store's last
/// effect (the process exits right after). Answers the cleanup line, whole (its slice and what
/// the commit carried too): a caller that writes its profile after the commit (the engine's
/// `check`, engine_check.rs) puts it there; one that wrote its profile before put
/// [`roots::CleanupLine::before_commit`] there instead. After the commit, the sweep of stores of
/// other forms under `home` (D160, clean.rs `sweep_other_forms`): one unused past forms.rs
/// `EXPIRY_DAYS` is removed and said; this blaze's own folder never is.
pub fn after_answer_printed(cx: &Cx, lent: CommandLent, home: &Home, stats: bool, clock: &Clock, err: &Out) -> Result<roots::CleanupLine, CliError> {
    after_answer_printed_with(cx, lent, home, stats, StoreTeardown::Drop, clock, err)
}

/// [`after_answer_printed`] with an explicit [`StoreTeardown`] policy.
pub fn after_answer_printed_with(
    cx: &Cx,
    lent: CommandLent,
    home: &Home,
    stats: bool,
    teardown: StoreTeardown,
    clock: &Clock,
    err: &Out,
) -> Result<roots::CleanupLine, CliError> {
    let ns = |clock: &Clock| u64::try_from(clock.now().since_start().as_nanos()).unwrap_or(u64::MAX);
    let answered = ns(clock);
    // Queue lease release (if this process held one) before testing whether a commit is owed.
    lent.release_lease();
    let bg_detached = home.bg_gc.is_detached();
    let forget_for_exit = !cfg!(test) && !cx.is_simulated() && teardown == StoreTeardown::ForgetForExit;
    // On a native host with detached background GC, commit immediately with 0 foreground slices
    // (1 flip, 0 seal barriers, full foreground QoS) and spawn detached `blaze gc --background`
    // only when `Driven::needs_bg_gc` fires (urgent triggers immediately, or unsealed segments
    // batched past `BG_GC_UNSEALED_SEGMENTS` / `BG_GC_UNSEALED_BYTES`). In simulated worlds
    // without a spawner, run inline slices when `needs_housekeeping` is true.
    let slices = if !bg_detached && lent.needs_commit() && lent.needs_housekeeping(lent.wall_ns()) {
        AFTER_ANSWER_SLICES
    } else {
        0
    };
    let has_work = slices > 0 || lent.needs_commit();
    let (after, commit_ns, spawn_bg) = if !has_work {
        let named = lent.named();
        let before = lent.put_tally();
        let cleanup = roots::CleanupLine {
            roots: named.roots,
            keys: named.keys,
            indexed: named.indexed,
            swept_bytes: 0,
            slices: 0,
            carried: lent.carried(),
            leases: u32::try_from(lent.leases_len()).unwrap_or(u32::MAX),
            put: roots::PutLine { before_answer: before, after_answer: blaze_cas::plain::PutTally::default() },
        };
        let after = plain_place::AfterAnswer { cleanup, ..plain_place::AfterAnswer::default() };
        let commit_ns = ns(clock).saturating_sub(answered);
        let spawn_bg = if bg_detached {
            let now_ns = lent.wall_ns();
            let driven = lent.into_store();
            let due = driven.needs_bg_gc(now_ns);
            if !due && forget_for_exit {
                // Zero writes committed (no lock ever acquired) and no background GC pending on
                // this one-shot native CLI exit: let the kernel reclaim read-only segment
                // mappings and store heap tables at process exit.
                core::mem::forget(driven);
            } else {
                drop(driven);
            }
            due
        } else {
            if forget_for_exit {
                core::mem::forget(lent);
            } else {
                drop(lent);
            }
            false
        };
        (after, commit_ns, spawn_bg)
    } else {
        // `plain_place::after_answer` keeps `lent.commit()` at caller QoS (P-cores) and scopes
        // QoS lowering strictly to inline maintenance (`fold_slice`, `clean_on`, `seal`).
        let after = plain_place::after_answer(cx, &lent, slices)?;
        let commit_ns = ns(clock).saturating_sub(answered);
        let spawn_bg = if bg_detached {
            let now_ns = lent.wall_ns();
            let driven = lent.into_store();
            let due = driven.needs_bg_gc(now_ns);
            if !due && forget_for_exit {
                // `plain_place::after_answer` already committed and released `flip.lock`, and no
                // background GC is pending on this one-shot native CLI exit: skip unmapping
                // read-only segments and freeing store tables.
                core::mem::forget(driven);
            } else {
                drop(driven);
            }
            due
        } else {
            if forget_for_exit {
                core::mem::forget(lent);
            } else {
                drop(lent);
            }
            false
        };
        (after, commit_ns, spawn_bg)
    };
    if spawn_bg {
        let cx = cx.span("store after answer: spawn detached background gc");
        let _ = home.bg_gc.spawn(&cx, home.dir(), err);
    } else if !bg_detached {
        let cx = cx.span("store after answer: sweep of other forms' stores (a listing of the store root)");
        #[cfg(target_os = "macos")]
        let _sweep_qos = match plain_place::AFTER_ANSWER_QOS {
            plain_place::AfterAnswerQos::Scoped => {
                Some(blaze_core::threads::qos::Lowered::at_most(blaze_core::threads::qos::AFTER_ANSWER).map_err(PlaceError::Qos)?)
            }
            plain_place::AfterAnswerQos::AsCalled => None,
        };
        crate::clean::sweep_other_forms(&cx, &home.dir, clock.wall().as_ns(), err, crate::clean::SweepPlant::None)?;
    }
    if stats {
        err.line(
            cx,
            format_args!(
                "blaze: store after the answer: commit {} segments, sealed {}, slices {}, {:.3} ms; answer's last byte to exit {:.3} ms",
                after.segments_written,
                after.sealed,
                after.compaction_slices,
                commit_ns as f64 / 1e6,
                ns(clock).saturating_sub(answered) as f64 / 1e6,
            ),
        )?;
        // The cleanup the commit named (store/roots.rs `CleanupLine`), whole.
        err.line(cx, format_args!("{}", after.cleanup))?;
    }
    Ok(after.cleanup)
}

/// A planted bug of the nonce's test (`store/nonce_tests.rs`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum NoncePlant {
    None,
    /// Every open takes one constant: two processes would name a segment alike.
    Constant,
}

/// The store's nonce ([`Facts::nonce`]): the next draw of the granted generator (bstd `Rng`,
/// seeded once from the system's secure source on a real driver, from the world's generator in
/// the simulation; main's ruling 21:3x: no second randomness grant). Taken where the store is
/// opened, from a generator handed in (rule 75).
pub fn nonce(rng: &mut bstd::rng::Rng) -> u64 {
    draw_nonce(rng, NoncePlant::None)
}

pub(crate) fn draw_nonce(rng: &mut bstd::rng::Rng, plant: NoncePlant) -> u64 {
    match plant {
        NoncePlant::None => rng.next_u64(),
        NoncePlant::Constant => 0x6e6f_6e63_6521,
    }
}

impl<'m, D: Driver, S: Sharing> CommandStore<'m, '_, D, S> {
    /// The keys the root `name` names, in place (32 bytes each, in order): `None` when there
    /// is no root of that name (engine_answer.rs reads the engine's stored answer by it).
    pub fn named(&self, name: &str) -> Result<Option<&'m [u8]>, CliError> {
        Ok(self.place.root(name)?.map(|held| held.bytes))
    }

    /// A phase boundary (plain_place.rs `PlainPlace::checkpoint`, by the put path's seam
    /// `VISIBILITY`). Under `AfterCommit`: what is put so far written now, one segment a
    /// namespace and one flip, and every arena chunk no get lent from freed (blaze-cas
    /// `Lent::commit`), so the next phase reads these records from a segment. Under `InArena`:
    /// a commit only past `COMMIT_BYTES` or `CHECKPOINT_AGE_NS`; the next phase reads them in
    /// place from the shared arena. A crash after a commit keeps its records, valid by their
    /// keys; the answer's pointer is still written last, by the commit after the answer.
    pub fn commit_puts(&mut self) -> Result<(), CliError> {
        self.place.checkpoint()?;
        Ok(())
    }

    /// Commits any uncommitted puts immediately (even below `COMMIT_BYTES`) so the current shared
    /// arena is freed before the next stage reads from committed file-backed segment mappings.
    pub fn flush_puts(&mut self) -> Result<(), CliError> {
        self.place.flush_puts()?;
        Ok(())
    }

    /// Number of shared arenas pinned past commit on this command's `Lent`.
    #[cfg(test)]
    pub fn kept_arenas(&self) -> usize {
        self.place.kept_arenas()
    }

    /// Fetches `key` from `self.remote` (if configured), writes it into `self.place`, and
    /// optionally copies the fetched bytes into `into` without lending from the arena.
    fn fetch_remote(&mut self, cx: &Cx, key: Key, into: Option<&mut Bytes>) -> Result<bool, CliError> {
        let Some(remote) = &mut self.remote else { return Ok(false) };
        let began = remote.now_ns();
        let clock = remote.clock.clone();
        let mut now = || u64::try_from(clock.now().since_start().as_nanos()).unwrap_or(u64::MAX);
        // A batch of one key: the request asks one record at a time (blaze-drive's `Get`).
        // `BatchError` is only a batch past `keys_most` or one begun mid-batch: neither can be.
        let fetched = match &remote.named.at {
            OriginAt::Folder(folder) => fetch(&mut remote.reader, &mut FolderOrigin { cx, folder }, &mut now, core::slice::from_ref(&key.0), remote.alloc),
            OriginAt::Http(at) => fetch(&mut remote.reader, &mut HttpOrigin::new(cx, at, &clock), &mut now, core::slice::from_ref(&key.0), remote.alloc),
        };
        fetched.map_err(|error| CliError::Remote(RemoteError::Batch(error)))?;
        let found = match remote.reader.answer(0) {
            Answer::Found { tag, .. } => {
                let bytes = remote.reader.found(0).unwrap_or(&[]);
                if let Some(into) = into {
                    into.try_extend(bytes).map_err(|stopped| CliError::Store(StoreFailure::Copy(stopped)))?;
                }
                // Verified once, by the read machine, up from the signed head; then kept as this
                // home's own record (`LOCAL`, its own tag), so the next lookup is a local hit
                // that checks nothing again.
                self.place.put_record(key, false, tag, bytes)?;
                remote.kept += 1;
                remote.kept_bytes += u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                // The set's floor rises to the head just taken, once a command (the rollback
                // rule: blaze-remote published/set.rs).
                if let (Some(set), Some(head), false) = (remote.named.check.set, remote.reader.head(), remote.floored)
                    && head.generation > set.seen
                {
                    published::keep_floor(&mut self.place, &set.id, head.generation, clock.wall().as_ns())?;
                    remote.floored = true;
                }
                true
            }
            Answer::Absent | Answer::Pending => false,
            Answer::Unfetched(why) => {
                remote.say(cx, key, &why)?;
                false
            }
        };
        if let Some(remote) = &mut self.remote {
            remote.spent_ns += remote.now_ns().saturating_sub(began);
        }
        Ok(found)
    }

    /// A `Send + Sync` read-only snapshot of the records committed now (blaze-cas
    /// plain/snapshot.rs), its list of segments in `alloc`: the engine's phase loop takes one
    /// after [`CommandStore::commit_puts`], hands `&Snapshot` to the stage's pool workers (who
    /// read records by key with it, in place, with no lock), and drops it at the stage's end.
    /// Not in it: this command's puts since the last commit under `Visibility::AfterCommit`
    /// (under `InArena` they are, from the shared arena), and any record an origin would serve
    /// (a snapshot reads the local segments and the arena alone, never fetches).
    pub fn snapshot_in<B: Allocator>(&self, alloc: B) -> Result<Snapshot<B>, CliError> {
        Ok(self.place.snapshot_in(alloc)?)
    }

    /// Keeps `keys` under the root `name` from the commit after the answer on: the engine's
    /// stored answer, its sealed pointer first, then the answer's root and pages
    /// (engine_answer.rs; the order simulated in blaze-engine pages_sim.rs).
    pub fn pin(&mut self, name: &str, keys: &[Key]) -> Result<(), CliError> {
        self.place.pin(name, keys)?;
        Ok(())
    }

    /// The toolchain's records this command's answer names, `records` (std's names records and
    /// the like: their `CrateScope` subjects were told public by the names step,
    /// [`Store::public_crates`]) and those filed beside them, kept under the toolchain's root
    /// for every checkout on `toolchain` (store/roots.rs `keep_toolchain`), renewed by every
    /// command that answers with them.
    pub fn keep_toolchain(&mut self, toolchain: blaze_toolchain::ToolchainId, records: &[Key]) -> Result<(), CliError> {
        let alloc = *self.place.lent().alloc();
        let mut made = std::vec::Vec::with_capacity_in(records.len() + self.toolchain_records.len(), alloc);
        made.extend(records.iter().map(|key| blaze_cas::Key(key.0)));
        // The records of the toolchain alone this command read or put (`keep_with_toolchain`):
        // sorted and once each, so an equal set is an equal index (rule 25).
        let mut alone = std::vec::Vec::with_capacity_in(self.toolchain_records.len(), alloc);
        alone.extend(self.toolchain_records.iter().map(|key| blaze_cas::Key(key.0)));
        alone.sort_unstable();
        alone.dedup();
        made.extend(alone);
        let mut beside = std::vec::Vec::new_in(alloc);
        for record in records {
            if let Some(index) = self.place.beside(*record)? {
                beside.push(index);
            }
        }
        roots::keep_toolchain(self.place.lent(), toolchain, &made, &beside).map_err(|why| CliError::Store(StoreFailure::Root(why)))?;
        Ok(())
    }

    /// A certified answer's roots renewed: each of `answer` (the answer's root and its
    /// `.pages`, the dependency heads') for blaze-cas `ROOT_LIFE_NS`, and the toolchain's
    /// root for its own life, naming what they named (plain_place.rs `renew`): the request
    /// read through them and wrote nothing, and a root a command reaches is renewed.
    pub fn renew(&mut self, answer: &[&str], toolchain: blaze_toolchain::ToolchainId) -> Result<(), CliError> {
        for name in answer {
            self.place.renew(name, blaze_cas::plain::ROOT_LIFE_NS)?;
        }
        self.place.renew(roots::RootName::toolchain(toolchain).as_str(), roots::TOOLCHAIN_LIFE_NS)?;
        Ok(())
    }

    /// Renews a single named root for `ROOT_LIFE_NS`, returning `true` if the root was live.
    pub fn renew_one(&mut self, name: &str) -> Result<bool, CliError> {
        Ok(self.place.renew(name, blaze_cas::plain::ROOT_LIFE_NS)?)
    }

    /// Puts a record that is always `LOCAL` (answer pages, producer header shards, printed lines,
    /// sealed pointers, bases), skipping [`Sharing::all_names_public`] drive-record parsing.
    pub fn put_local(&mut self, cx: &Cx, key: Key, bytes: &[u8]) -> Result<(), CliError> {
        let par = CxPar::new(cx);
        match self.place.put_parts_par(key, false, &[bytes], &par) {
            Ok(()) => Ok(()),
            Err(PlaceError::Lent(lent @ blaze_cas::plain::LentError::TwoValues { .. })) => {
                let held = self.place.get(key)?;
                let clash = Clash::of(held.map(|held| held.bytes), bytes);
                Err(CliError::Store(StoreFailure::TwoValues { key, lent, clash }))
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Whether `name` is a toolchain crate's record ([`Store::toolchain_crates`]): a
    /// `CrateScope`, `ImplBucket`, or closed `Signature` name of one of its subjects.
    pub fn is_toolchain(&self, name: Name) -> bool {
        matches!(name.kind, Kind::CrateScope | Kind::ImplBucket | Kind::Signature) && self.toolchain.binary_search(&name.subject).is_ok()
    }
}

impl<'m, D: Driver, S: Sharing> Store<'m> for CommandStore<'m, '_, D, S> {
    fn commit_for_body_reads(&mut self) -> Result<SharedSnapshot, CliError> {
        self.commit_puts()?;
        self.snapshot()?.ok_or(CliError::Text { what: "the committed body round has no store snapshot" })
    }

    fn snapshot(&self) -> Result<Option<SharedSnapshot>, CliError> {
        let alloc = blaze_mem::Kept(blaze_mem::Tag::Store);
        let inner = self.place.snapshot_in(alloc)?;
        Ok(Some(Arc::new_in(StoreSnapshot { inner, expansion_demands: bstd::sync::CachePadded::new(AtomicU64::new(0)) }, alloc)))
    }

    fn public_crates(&mut self, subjects: &[ContentId]) {
        self.sharing.add_public(subjects);
    }

    fn toolchain_crates(&mut self, subjects: &[ContentId]) {
        self.toolchain.extend_from_slice(subjects);
        self.toolchain.sort_unstable();
        self.toolchain.dedup();
    }

    fn keep_with_toolchain(&mut self, key: Key) -> Result<(), CliError> {
        if self.toolchain_records.contains(&key) {
            return Ok(());
        }
        let had = self.toolchain_records.len();
        self.toolchain_records.try_reserve(1).map_err(|_| CliError::EngineCheck(crate::engine_check::EngineCheckError::Grow { what: "the store's records of the toolchain alone", had }))?;
        self.toolchain_records.push(key);
        Ok(())
    }

    fn kept(&self) -> StoreKept {
        let lent = self.place.counts().lent;
        // A commit frees each arena chunk no get lent from (blaze-cas plain/lent.rs).
        StoreKept { copies: lent.copied, copy_bytes: lent.arena_bytes, held_bytes: lent.held_bytes }
    }

    fn get(&mut self, cx: &Cx, key: Key) -> Result<Option<Held<'m>>, CliError> {
        if let Some(held) = self.place.get(key)? {
            return Ok(Some(held));
        }
        if self.fetch_remote(cx, key, None)? {
            return Ok(self.place.get(key)?);
        }
        Ok(None)
    }

    fn get_committed(&mut self, cx: &Cx, key: Key) -> Result<Committed<'m>, CliError> {
        match self.place.get_committed(key)? {
            Committed::Found(held) => Ok(Committed::Found(held)),
            Committed::InArena => Ok(Committed::InArena),
            Committed::Absent => {
                if self.fetch_remote(cx, key, None)? {
                    Ok(Committed::InArena)
                } else {
                    Ok(Committed::Absent)
                }
            }
        }
    }

    fn contains(&mut self, cx: &Cx, key: Key) -> Result<bool, CliError> {
        if self.place.contains(key)? {
            return Ok(true);
        }
        self.fetch_remote(cx, key, None)
    }

    fn commits(&self) -> u32 {
        self.place.commits()
    }

    fn get_into(&mut self, cx: &Cx, key: Key, into: &mut Bytes) -> Result<bool, CliError> {
        if let Some(copied) = self.place.with_bytes(key, |bytes| into.try_extend(bytes))? {
            copied.map_err(|stopped| CliError::Store(StoreFailure::Copy(stopped)))?;
            return Ok(true);
        }
        self.fetch_remote(cx, key, Some(into))
    }

    fn put(&mut self, cx: &Cx, key: Key, bytes: &[u8]) -> Result<(), CliError> {
        self.put_parts(cx, key, &[bytes])
    }

    fn put_parts(&mut self, cx: &Cx, key: Key, parts: &[&[u8]]) -> Result<(), CliError> {
        let public = self.place.is_public_writer() && self.sharing.all_names_public(parts);
        let par = CxPar::new(cx);
        match self.place.put_parts_par(key, public, parts, &par) {
            Ok(()) => Ok(()),
            Err(PlaceError::Lent(lent @ blaze_cas::plain::LentError::TwoValues { .. })) => {
                let held = self.place.get(key)?;
                // A refusal alone joins the parts, to name the first differing byte.
                let mut bytes = Vec::new_in(*self.place.lent().alloc());
                for part in parts {
                    bytes.extend_from_slice(part);
                }
                let clash = Clash::of(held.map(|held| held.bytes), &bytes);
                Err(CliError::Store(StoreFailure::TwoValues { key, lent, clash }))
            }
            Err(error) => Err(error.into()),
        }
    }

    fn put_record(&mut self, cx: &Cx, key: Key, tag: blaze_keyed::kind::Tag, owner: &[&[u8]], bytes: &[u8]) -> Result<blaze_keyed::store::Put, CliError> {
        let public = self.place.is_public_writer() && self.sharing.all_names_public(owner);
        let par = CxPar::new(cx);
        match self.place.put_record_par(key, public, tag, bytes, &par) {
            Ok(put) => Ok(put),
            Err(PlaceError::Lent(lent @ blaze_cas::plain::LentError::TwoValues { .. })) => {
                let held = self.place.get(key)?;
                let clash = Clash::of(held.map(|held| held.bytes), bytes);
                Err(CliError::Store(StoreFailure::TwoValues { key, lent, clash }))
            }
            Err(error) => Err(error.into()),
        }
    }

    fn index_beside(&mut self, _cx: &Cx, owner: Key, keys: &[Key]) -> Result<(), CliError> {
        self.place.index_beside(owner, keys)?;
        Ok(())
    }

    fn map_all(&mut self, cx: &Cx) -> Result<(), CliError> {
        if !self.mapped_all {
            let par = CxPar::new(cx);
            self.place.map_all_par(&par)?;
            self.mapped_all = true;
        }
        Ok(())
    }

    fn after_answer(&mut self, cx: &Cx, profile: &mut Profile) -> Result<(), CliError> {
        // Facts of the run, not news: the profile's rows (and `--stats`), never plain stderr.
        let counts = self.place.counts();
        profile.store_open_ns = self.open_ns;
        profile.store_named = counts.named;
        profile.store_mapped = counts.mapped;
        profile.store_probe_gets = counts.probes.gets;
        profile.store_probes = counts.probes.probes;
        profile.store_probes_most = counts.probes.most;
        if let Some(remote) = &self.remote {
            let r = remote.reader.report();
            remote.err.line(
                cx,
                format_args!(
                    "blaze: origin: {} keys asked, {} found, {} absent, {} not fetched; {} range reads (head {}, manifest {}, summary {}, index {}, payload {}), {} late; {} bytes fetched for {} used; {} records verified and kept ({} bytes); {:.3} ms fetching and verifying",
                    r.keys,
                    r.found,
                    r.absent,
                    r.unfetched,
                    r.asks,
                    r.head_asks,
                    r.manifest_asks,
                    r.summary_asks,
                    r.page_asks,
                    r.payload_asks,
                    r.late,
                    r.bytes_fetched,
                    r.bytes_used,
                    remote.kept,
                    remote.kept_bytes,
                    remote.spent_ns as f64 / 1e6,
                ),
            )?;
        }
        Ok(())
    }
}

/// An origin named on the command line (remote-read.md): `--origin <folder> --origin-key <hex>`
/// after the checkout. Nothing ambient (rule 75): with no `--origin`, nothing is read but the
/// local store. The folder is beneath the start directory; the key is the one Ed25519 public
/// key whose signed head is believed.
pub struct NamedOrigin {
    at: OriginAt,
    check: OneKey,
}

/// Where a named origin's objects are: a folder, or an `http://` / `https://` base address.
enum OriginAt {
    Folder(Dir<ReadWrite>),
    Http(HttpAt),
}

/// Pooled keep-alive HTTPS/HTTP client shared across every read of an [`HttpAt`] origin
/// (zero redundant TCP/TLS handshakes across waves and records).
#[cfg(feature = "fetch")]
struct PooledHttp {
    handle: blaze_http::driver::Handle,
    https_only: bool,
}

/// An `http://` or `https://` origin: the base it was given, the network narrowed to that one
/// address (for `http://` over bstd's `Net`), and (under `feature = "fetch"`) a shared pooled
/// keep-alive `blaze-http` client for `https://` R2/S3 origins with TLS verification and bounded
/// parallel fetches.
pub struct HttpAt {
    base: Url,
    net: Net,
    #[cfg(feature = "fetch")]
    pooled: Option<PooledHttp>,
}

/// The one trusted key of a named origin, owned, as the read path's head check; for a published
/// set, its id the head's signature is bound to and the floor this home has taken for it.
struct OneKey {
    keys: [PublicKey; 1],
    set: Option<PublishedSet>,
}

/// A published set a named origin is read as (blaze-remote published/set.rs): its id, the
/// highest generation this home has taken for it (store/published.rs `floor`), and whether the
/// command named a key for the origin (`--origin-key`; else only blaze-remote's compiled-in
/// `trust::PUBLISHED_KEYS` are believed, D137).
#[derive(Clone, Copy)]
struct PublishedSet {
    id: ContentId,
    seen: u64,
    named: bool,
}

impl OneKey {
    /// The keys the command granted the origin by name: the one, or for a published set named
    /// with no `--origin-key`, none.
    fn named_keys(&self) -> &[PublicKey] {
        match self.set {
            Some(PublishedSet { named: false, .. }) => &[],
            _ => &self.keys,
        }
    }
}

impl HeadCheck for OneKey {
    fn check(&self, namespace: Namespace, signer: &PublicKey, domain: &'static str, message: &[u8], signature: &[u8; 64]) -> Result<(), SignatureWhy> {
        match self.set {
            None => Ed25519Keys { namespace: Namespace::PUBLIC, keys: &self.keys }.check(namespace, signer, domain, message, signature),
            Some(set) => SetCheck { set: set.id, keys: self.named_keys(), seen: set.seen }.check(namespace, signer, domain, message, signature),
        }
    }
}

impl NamedOrigin {
    /// The origin the arguments name (main.rs reads them): `--origin` and `--origin-key`
    /// together or neither.
    pub fn of(cx: &Cx, fs: &Fs, net: Option<&Net>, folder: Option<&str>, key: Option<&str>) -> Result<Option<NamedOrigin>, CliError> {
        match (folder, key) {
            (None, None) => Ok(None),
            (Some(folder), Some(key)) => {
                let key = PublicKey::from_hex(key).ok_or(CliError::Usage("--origin-key is not 64 hex digits (an Ed25519 public key)"))?;
                let at = if folder.starts_with("https://") || folder.starts_with("http://") {
                    let net = net.ok_or(CliError::Usage("--origin http(s)://: this host offers no network"))?;
                    OriginAt::Http(HttpAt::of(folder, net)?)
                } else {
                    OriginAt::Folder(fs.open_dir(cx, &Path::new(folder)?)?)
                };
                Ok(Some(NamedOrigin { at, check: OneKey { keys: [key], set: None } }))
            }
            _ => Err(CliError::Usage("--origin and --origin-key are named together")),
        }
    }

    /// The published set `set` of the origin `origin`: its folder `public/<toolchain>/<target>`
    /// beneath a folder, or that path joined to an `http://` / `https://` base (each object then
    /// one ranged GET of `<base>/public/<toolchain>/<target>/<name>`). Believed when its head is
    /// signed over the set's id by a key trusted for the origin (blaze-remote trust.rs
    /// `trusted_for`: the compiled-in published keys, or `key`, the `--origin-key` named for it),
    /// at a generation no lower than the floor `lent` holds for it.
    pub fn published<D: Driver>(cx: &Cx, fs: &Fs, net: Option<&Net>, origin: &str, key: Option<&str>, set: blaze_remote::published::set::SetName<'_>, lent: &Lent<D, StoreAlloc>) -> Result<NamedOrigin, CliError> {
        let named = match key {
            Some(key) => Some(PublicKey::from_hex(key).ok_or(CliError::Usage("--origin-key is not 64 hex digits (an Ed25519 public key)"))?),
            None => None,
        };
        let at = if origin.starts_with("https://") || origin.starts_with("http://") {
            let net = net.ok_or(CliError::Usage("--origin http(s)://: this host offers no network"))?;
            let [public, toolchain, target] = set.dir();
            // Each part a checked set name (`[0-9a-z._-]`): nothing joined here leaves the base.
            let joined = bstd::mem::format_in!(cx.frame(), "{}/{}/{}/{}", origin.trim_end_matches('/'), public, toolchain, target);
            OriginAt::Http(HttpAt::of(joined.as_str(), net)?)
        } else {
            let mut dir = fs.open_dir(cx, &Path::new(origin)?)?;
            for part in set.dir() {
                dir = dir.subdir(&Path::new(part)?);
            }
            OriginAt::Folder(dir)
        };
        let id = set.id();
        let seen = published::floor(lent, &id)?;
        // With no key named, the one slot is never consulted (`OneKey::named_keys`).
        let keys = [named.unwrap_or(PublicKey([0; 32]))];
        Ok(NamedOrigin { at, check: OneKey { keys, set: Some(PublishedSet { id, seen, named: named.is_some() }) } })
    }
}

/// Why a read from a named origin stopped the command (not a miss: a miss computes locally).
#[derive(Debug)]
pub enum RemoteError {
    /// The read path refused the batch: past `keys_most`, or begun mid-batch.
    Batch(BatchError),
    /// A fetched record could not be copied into the request's buffer.
    Copy(Stopped),
    /// System TLS root certificates could not be loaded for an `https://` origin.
    Tls(blaze_own::String<blaze_mem::Kept>),
    /// Proxy settings for an `https://` origin were invalid.
    Proxy(blaze_own::String<blaze_mem::Kept>),
    /// The pooled HTTP/HTTPS driver thread could not be started.
    Driver(blaze_own::String<blaze_mem::Kept>),
}

impl core::fmt::Display for RemoteError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            RemoteError::Batch(error) => write!(f, "the origin's read path: {error}"),
            RemoteError::Copy(stopped) => write!(f, "a record fetched from the origin: {stopped}"),
            RemoteError::Tls(why) => write!(f, "the origin's HTTPS TLS roots: {why}"),
            RemoteError::Proxy(why) => write!(f, "the origin's HTTP proxy settings: {why}"),
            RemoteError::Driver(why) => write!(f, "starting the origin's pooled HTTPS driver: {why}"),
        }
    }
}

/// A folder as an origin: each ask one ranged read through bstd's files, bounded by the ask's
/// deadline. An object that is not there is `Missing`; any other failure `Unreadable`.
struct FolderOrigin<'o> {
    cx: &'o Cx,
    folder: &'o Dir<ReadWrite>,
}

impl Origin for FolderOrigin<'_> {
    fn read(&mut self, object: &Object, at: u64, into: &mut [u8], deadline_ns: u64) -> Result<usize, OriginError> {
        let failed = |error: FsError| if error.kind == FsKind::NotFound { OriginError::Missing } else { OriginError::Unreadable };
        // The name is built from a digest's hex only (`Object::name`): nothing fetched is a path.
        let name = object.name();
        let path = Path::new(name.as_str()).map_err(|_| OriginError::Unreadable)?;
        let cx = self.cx.until(Deadline::at(Duration::from_nanos(deadline_ns)));
        let file = self.folder.open_read(&cx, &path).map_err(failed)?;
        file.read_into(into, at).wait(&cx).map_err(failed)
    }
}

/// Bounded concurrent ranged requests kept in flight against a pooled HTTP/HTTPS origin (rule 18).
const ORIGIN_IN_FLIGHT: usize = 16;

/// Extra wait slack past the earliest request deadline in flight when awaiting the HTTP driver.
#[cfg(feature = "fetch")]
const ORIGIN_ANSWER_SLACK: Duration = Duration::from_secs(1);

#[cfg(feature = "fetch")]
fn own_msg(display: impl core::fmt::Display) -> blaze_own::String<blaze_mem::Kept> {
    let mut msg = blaze_own::String::new_in(blaze_mem::Kept(blaze_mem::Tag::Store));
    let _ = core::fmt::write(&mut msg, format_args!("{display}"));
    msg
}

impl HttpAt {
    /// The origin at `base` (`http://host:port[/prefix]` or `https://host[:port][/prefix]`),
    /// the network narrowed to its address. For `https://` (R2/S3/static HTTPS origins), builds a
    /// pooled keep-alive `blaze-http` client with verified system TLS roots (no fallback on TLS
    /// setup or handshake failure, rule 3).
    fn of(base: &str, net: &Net) -> Result<HttpAt, CliError> {
        let trimmed = base.trim_end_matches('/');
        let is_https = trimmed.starts_with("https://");
        let parsed = Url::parse(trimmed).map_err(|_| CliError::Usage("--origin http(s)://: not a URL"))?;
        let origin = parsed.origin().ok_or(CliError::Usage("--origin http(s)://: the URL names no host"))?;
        // clone: the origin's address, a refcount.
        let net = net.connect_only(&Addr::Tcp(origin.at().clone()));
        #[cfg(not(feature = "fetch"))]
        {
            if is_https {
                return Err(CliError::Usage("--origin https:// requires blaze built with the `fetch` (HTTPS/TLS) feature"));
            }
            Ok(HttpAt { base: parsed, net })
        }
        #[cfg(feature = "fetch")]
        {
            let pooled = if is_https {
                let places = blaze_http::roots::Places { cert_file: None, cert_dir: None };
                let tls = blaze_http::roots::system(places).map_err(|e| CliError::Remote(RemoteError::Tls(own_msg(e))))?;
                let proxies = blaze_http::proxy_env::from_env().map_err(|e| CliError::Remote(RemoteError::Proxy(own_msg(e))))?;
                let config = blaze_http::Config {
                    idle_most: ORIGIN_IN_FLIGHT,
                    proxies,
                    redirects: 0,
                    ..blaze_http::Config::new(tls, ORIGIN_WIRE.user_agent, ORIGIN_WIRE.wait_most)
                };
                let handle = blaze_http::driver::Handle::spawn(blaze_http::Client::new(config), ORIGIN_IN_FLIGHT)
                    .map_err(|e| CliError::Remote(RemoteError::Driver(own_msg(e))))?;
                Some(PooledHttp { handle, https_only: true })
            } else {
                None
            };
            Ok(HttpAt { base: parsed, net, pooled })
        }
    }
}

/// How the shared cache's origin is asked over HTTP: a reply's body is held to the ask's range
/// by bstd's `Http` itself (and to a record's most here); a redirect is refused; a host that
/// says nothing for ten seconds is `Silent`, and the ask's deadline bounds the whole.
const ORIGIN_WIRE: Wire = Wire {
    user_agent: "blaze",
    body_most: 64 << 20,
    // A 64 MiB body in 16 KiB receives, and its head.
    reads_most: (64 << 20) / (16 << 10) + 64,
    wait_most: Duration::from_secs(10),
    redirects: NO_REDIRECTS,
};

/// An `http://` or `https://` origin: each ask one ranged GET of `<base>/<object name>`.
///
/// - For `https://` (R2/S3/static HTTPS origins under `feature = "fetch"`): dispatches over the
///   origin's shared pooled keep-alive `blaze-http` client (verified TLS roots, `https_only: true`,
///   `redirects: 0`, up to [`ORIGIN_IN_FLIGHT`] concurrent ranged GETs in [`Origin::read_batch`]).
///   TLS handshake/verification failures return typed `OriginError::Unreadable` with zero fallback.
/// - For `http://` (and simulated single-lane tests via [`HttpOrigin::read_with`]): uses bstd's
///   `Http` over its `Net`.
///
/// A 404 is `Missing`, a 416 (asked past the object's end) no bytes, anything else `Unreadable`.
/// Nothing here trusts the bytes: the read machine / take verifies the signed head and digest chain.
struct HttpOrigin<'o> {
    cx: &'o Cx,
    at: &'o HttpAt,
    clock: &'o Clock,
    /// bstd's client, reaching the origin's address alone (used for `http://` and simulated tests).
    http: Http,
    /// The URL's text, reused from ask to ask.
    url: blaze_own::Vec<u8, blaze_mem::Kept>,
}

impl<'o> HttpOrigin<'o> {
    fn new(cx: &'o Cx, at: &'o HttpAt, clock: &'o Clock) -> HttpOrigin<'o> {
        let http = Http::over(&at.net, clock, &Roots::none(), ORIGIN_WIRE);
        let http = match at.base.origin() {
            Some(origin) => http.only(origin),
            None => http,
        };
        HttpOrigin { cx, at, clock, http, url: blaze_own::Vec::new_in(blaze_mem::Kept(blaze_mem::Tag::Store)) }
    }
}

#[cfg(feature = "fetch")]
fn apply_ranged_fetched(at: u64, into: &mut [u8], fetched: blaze_http::driver::Fetched) -> Result<usize, OriginError> {
    match fetched {
        blaze_http::driver::Fetched::Answered { status: 206, headers, body, .. } => {
            // Verify `Content-Range: bytes <at>-...` starts at the exact offset requested so a
            // misbehaving origin or proxy cannot silently shift the returned slice.
            let range_ok = headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("content-range"))
                .and_then(|(_, v)| v.trim().strip_prefix("bytes "))
                .and_then(|v| v.split_once('-'))
                .and_then(|(first, _)| first.parse::<u64>().ok())
                .is_some_and(|first| first == at);
            if !range_ok || body.len() > into.len() {
                return Err(OriginError::Unreadable);
            }
            let got = body.len();
            if let Some(dst) = into.get_mut(..got) {
                dst.copy_from_slice(body.as_slice());
                Ok(got)
            } else {
                Err(OriginError::Unreadable)
            }
        }
        blaze_http::driver::Fetched::Answered { status: 404, .. } => Err(OriginError::Missing),
        blaze_http::driver::Fetched::Answered { status: 416, .. } => Ok(0),
        // 200 on a ranged read, TLS verification/handshake failure, timeout, or any other status:
        // typed refusal, never silently downgraded or retried (rule 3).
        blaze_http::driver::Fetched::Answered { .. }
        | blaze_http::driver::Fetched::Failed(_)
        | blaze_http::driver::Fetched::TimedOut => Err(OriginError::Unreadable),
    }
}

impl HttpOrigin<'_> {
    fn now_ns(&self) -> u64 {
        u64::try_from(self.clock.now().since_start().as_nanos()).unwrap_or(u64::MAX)
    }

    #[cfg(feature = "fetch")]
    fn build_pooled_request<A: core::alloc::Allocator>(
        base: &Url,
        url_buf: &mut blaze_own::Vec<u8, A>,
        object: &Object,
        at: u64,
        len: usize,
        https_only: bool,
    ) -> Result<blaze_http::Request, OriginError> {
        let name = object.name();
        url_buf.clear();
        for part in [base.as_str().as_bytes(), b"/", name.as_str().as_bytes()] {
            url_buf.try_extend_from_slice(part).map_err(|_| OriginError::Unreadable)?;
        }
        let text = core::str::from_utf8(url_buf.as_slice()).map_err(|_| OriginError::Unreadable)?;
        let len_u64 = u64::try_from(len).map_err(|_| OriginError::Unreadable)?;
        let last = at.checked_add(len_u64.saturating_sub(1)).ok_or(OriginError::Unreadable)?;
        Ok(blaze_http::Request {
            method: blaze_http::Method::Get,
            url: text.into(),
            range: Some(blaze_http::Range { first: at, last: Some(last) }),
            headers: Default::default(),
            body: Default::default(),
            gzip: false,
            most: len_u64,
            https_only,
        })
    }

    #[cfg(feature = "fetch")]
    fn read_pooled(&mut self, pooled: &PooledHttp, object: &Object, at: u64, into: &mut [u8], deadline_ns: u64) -> Result<usize, OriginError> {
        if into.is_empty() {
            return Ok(0);
        }
        let req = Self::build_pooled_request(&self.at.base, &mut self.url, object, at, into.len(), pooled.https_only)?;
        let now = self.now_ns();
        let allowed = Duration::from_nanos(deadline_ns.saturating_sub(now));
        let (tx, rx) = blaze_core::chan::bounded(1);
        pooled.handle.fetch(req, allowed, 0, &tx);
        match blaze_http::mailbox::answer("the origin's HTTPS answer", &rx, allowed.saturating_add(ORIGIN_ANSWER_SLACK)) {
            Ok(Some((_, fetched))) => apply_ranged_fetched(at, into, fetched),
            Ok(None) | Err(_) => Err(OriginError::Unreadable),
        }
    }

    #[cfg(feature = "fetch")]
    fn read_batch_pooled(
        &mut self,
        pooled: &PooledHttp,
        reads: &mut [BatchRead<'_>],
        clock: &mut dyn FnMut() -> u64,
        outcomes: &mut [BatchOutcome],
    ) {
        let total = reads.len().min(outcomes.len());
        let mut offset = 0usize;
        while offset < total {
            let chunk_len = (total - offset).min(ORIGIN_IN_FLIGHT);
            let Some(req_chunk) = reads.get_mut(offset..offset + chunk_len) else { break };
            let Some(out_chunk) = outcomes.get_mut(offset..offset + chunk_len) else { break };
            let (tx, rx) = blaze_core::chan::bounded(chunk_len);
            let mut done = [false; ORIGIN_IN_FLIGHT];
            let mut in_flight = 0usize;
            let now = clock();

            for (i, req) in req_chunk.iter().enumerate() {
                if req.into.is_empty() {
                    if let Some(slot) = out_chunk.get_mut(i) {
                        *slot = BatchOutcome { result: Ok(0), at_ns: now };
                    }
                    if let Some(flag) = done.get_mut(i) {
                        *flag = true;
                    }
                    continue;
                }
                match Self::build_pooled_request(&self.at.base, &mut self.url, &req.object, req.at, req.into.len(), pooled.https_only) {
                    Ok(http_req) => {
                        let allowed = Duration::from_nanos(req.deadline_ns.saturating_sub(now));
                        let key = u64::try_from(i).unwrap_or(0);
                        pooled.handle.fetch(http_req, allowed, key, &tx);
                        in_flight += 1;
                    }
                    Err(err) => {
                        if let Some(slot) = out_chunk.get_mut(i) {
                            *slot = BatchOutcome { result: Err(err), at_ns: now };
                        }
                        if let Some(flag) = done.get_mut(i) {
                            *flag = true;
                        }
                    }
                }
            }

            while in_flight > 0 {
                let earliest = req_chunk
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| !done.get(*i).copied().unwrap_or(true))
                    .map(|(_, req)| req.deadline_ns)
                    .min()
                    .unwrap_or_else(|| clock());
                let wait = Duration::from_nanos(earliest.saturating_sub(clock())).saturating_add(ORIGIN_ANSWER_SLACK);
                match blaze_http::mailbox::answer("the origin's HTTPS batch answers", &rx, wait) {
                    Ok(Some((key, fetched))) => {
                        let at_ns = clock();
                        if let Ok(idx) = usize::try_from(key)
                            && idx < chunk_len
                            && !done.get(idx).copied().unwrap_or(true)
                        {
                            let result = match req_chunk.get_mut(idx) {
                                Some(req) => apply_ranged_fetched(req.at, req.into, fetched),
                                None => Err(OriginError::Unreadable),
                            };
                            if let Some(slot) = out_chunk.get_mut(idx) {
                                *slot = BatchOutcome { result, at_ns };
                            }
                            if let Some(flag) = done.get_mut(idx) {
                                *flag = true;
                            }
                            in_flight -= 1;
                        }
                    }
                    Ok(None) | Err(_) => {
                        let failed_at = clock();
                        for i in 0..chunk_len {
                            if !done.get(i).copied().unwrap_or(true) {
                                if let Some(slot) = out_chunk.get_mut(i) {
                                    *slot = BatchOutcome { result: Err(OriginError::Unreadable), at_ns: failed_at };
                                }
                                if let Some(flag) = done.get_mut(i) {
                                    *flag = true;
                                }
                            }
                        }
                        break;
                    }
                }
            }
            offset += chunk_len;
        }
    }
}

impl Origin for HttpOrigin<'_> {
    fn read(&mut self, object: &Object, at: u64, into: &mut [u8], deadline_ns: u64) -> Result<usize, OriginError> {
        #[cfg(feature = "fetch")]
        if let Some(pooled) = &self.at.pooled {
            return self.read_pooled(pooled, object, at, into, deadline_ns);
        }
        self.read_with(object, at, into, deadline_ns, |_| {})
    }

    fn read_batch(
        &mut self,
        reads: &mut [BatchRead<'_>],
        clock: &mut dyn FnMut() -> u64,
        outcomes: &mut [BatchOutcome],
    ) {
        #[cfg(feature = "fetch")]
        if let Some(pooled) = &self.at.pooled {
            self.read_batch_pooled(pooled, reads, clock, outcomes);
            return;
        }
        for (req, out) in reads.iter_mut().zip(outcomes.iter_mut()) {
            let result = self.read(&req.object, req.at, req.into, req.deadline_ns);
            *out = BatchOutcome { result, at_ns: clock() };
        }
    }
}

impl HttpOrigin<'_> {
    /// [`Origin::read`], with `between` run after the request is sent and before its answer is
    /// read: nothing in the command; the test's server, in the simulation (bstd's begin/finish
    /// split, which lets one lane play both ends).
    fn read_with(&mut self, object: &Object, at: u64, into: &mut [u8], deadline_ns: u64, between: impl FnOnce(&Cx)) -> Result<usize, OriginError> {
        if into.is_empty() {
            return Ok(0);
        }
        // The name is built from a digest's hex only (`Object::name`): nothing fetched is a path.
        let name = object.name();
        self.url.clear();
        let joined = [self.at.base.as_str().as_bytes(), b"/", name.as_str().as_bytes()];
        for part in joined {
            self.url.try_extend_from_slice(part).map_err(|_| OriginError::Unreadable)?;
        }
        let text = core::str::from_utf8(&self.url).map_err(|_| OriginError::Unreadable)?;
        let url = Url::parse(text).map_err(|_| OriginError::Unreadable)?;
        let range = Range { first: at, last: Some(at + into.len() as u64 - 1) };
        let request = Request { range: Some(range), ..Request::get(&url) };
        let cx = self.cx.until(Deadline::at(Duration::from_nanos(deadline_ns)));
        let mut body = cx.bytes();
        let answered = match self.http.begin(&cx, &request) {
            Ok(exchange) => {
                between(&cx);
                match exchange.finish(&cx, &mut body) {
                    Ok(Finished::Answered(response)) => Ok(response),
                    // `NO_REDIRECTS` makes a redirect an error; a hop is never begun.
                    Ok(Finished::Redirected(next)) => {
                        let _ = next.finish(&cx, &mut body);
                        return Err(OriginError::Unreadable);
                    }
                    Err(error) => Err(error),
                }
            }
            Err(error) => Err(error),
        };
        match answered {
            Ok(_) => {
                // bstd held the body to the range, `into.len()` bytes at most.
                let got = body.len().min(into.len());
                into[..got].copy_from_slice(&body.as_slice()[..got]);
                Ok(got)
            }
            Err(error) => match error.kind {
                HttpKind::Protocol(blaze_h1::Error { why: blaze_h1::Why::RangeStatus { status: 404 }, .. }) => {
                    Err(OriginError::Missing)
                }
                HttpKind::Protocol(blaze_h1::Error { why: blaze_h1::Why::RangeStatus { status: 416 }, .. }) => Ok(0),
                _ => Err(OriginError::Unreadable),
            },
        }
    }
}

struct Remote<'r> {
    named: &'r NamedOrigin,
    reader: Reader<'r>,
    clock: Clock,
    err: &'r Out,
    alloc: Alloc<'r>,
    /// Time spent reading the origin and checking what came back.
    spent_ns: u64,
    /// One bit for each kind of "not fetched" already said.
    said: u8,
    /// Records fetched, verified and kept as local records, and their bytes.
    kept: u32,
    kept_bytes: u64,
    /// Whether this command has raised its published set's floor.
    floored: bool,
}

impl Remote<'_> {
    fn now_ns(&self) -> u64 {
        u64::try_from(self.clock.now().since_start().as_nanos()).unwrap_or(u64::MAX)
    }

    /// One "not fetched" line for each kind, the first time it happens in this command.
    fn say(&mut self, cx: &Cx, key: Key, why: &Unfetched) -> Result<(), CliError> {
        let kind = match why {
            Unfetched::Origin { .. } => 0,
            Unfetched::Short { .. } => 1,
            Unfetched::Late { .. } => 2,
            Unfetched::Refused(_) => 3,
            Unfetched::Limit { .. } => 4,
        };
        if self.said & (1 << kind) != 0 {
            return Ok(());
        }
        self.said |= 1 << kind;
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut short = [0u8; 12];
        for (at, byte) in key.0.0[..6].iter().enumerate() {
            short[at * 2] = HEX[usize::from(byte >> 4)];
            short[at * 2 + 1] = HEX[usize::from(byte & 15)];
        }
        let short = core::str::from_utf8(&short).map_err(|_| CliError::Text { what: "a key's hex" })?;
        self.err.line(cx, format_args!("blaze: origin: record {short}… not fetched, computed here: {why}"))?;
        Ok(())
    }
}

/// The http origin's proof: the folder's records byte-equal over HTTP, a liar refused.
#[cfg(test)]
#[path = "store_http_tests.rs"]
mod http_tests;

/// The old home's report and the no-home refusal, on bstd's simulated world.
#[cfg(test)]
#[path = "store/old_home_tests.rs"]
mod old_home_tests;

/// The nonce: equal under one simulation seed, different under two, and different for two opens
/// of one process (control: a constant).
#[cfg(test)]
#[path = "store/nonce_tests.rs"]
mod nonce_tests;

/// A natively run build script's record and its reader stay `LOCAL` (D115; the scripts lane's).
#[cfg(test)]
#[path = "store/sharing_tests.rs"]
mod sharing_tests;

/// A snapshot taken after a phase's commit, read from eight threads, byte-equal to `Store::get`.
#[cfg(test)]
#[path = "store/snapshot_tests.rs"]
mod snapshot_tests;

/// The published set end to end: only publishable records leave home A, home B reads each once,
/// each planted fault refused by name, a publish stopped anywhere (DST), equal bytes from two homes.
#[cfg(test)]
#[path = "store/published_e2e_tests.rs"]
mod published_e2e_tests;

/// Every record a later command reads is reached from a root, under a reach sweep: two
/// checkouts' programs over one store, one checkout let expire (store/roots.rs).
#[cfg(test)]
#[path = "store/reach_tests.rs"]
mod reach_tests;

#[cfg(test)]
mod after_answer_commit_tests {
    use super::*;
    use bstd::io::{Io, Limits, SimWorld};
    use bstd::mem::TASK_FRAME;
    use bstd::test::BUDGET;

    /// Foreground zero-slice `after_answer_printed` commits in 1 flip with 0 seal barriers,
    /// and `gc::gc_lock_is_free` detects an active GC lock so `BgGc::spawn` skips redundant spawns.
    #[test]
    fn after_answer_zero_slice_commits_without_seal_barrier_and_gc_lock_check_skips_active_gc() {
        let io = Io::simulated(SimWorld::new(83, Limits::USUAL));
        let cx = io.cx(BUDGET, TASK_FRAME).expect("root cx");
        let fs = io.need_fs().expect("fs");
        let clock = io.need_clock().expect("clock");
        let err = io.need_err().expect("err");
        let heap = blaze_own::testing::heap();
        let home = Home::of(&cx, &fs, Some("/h")).expect("home");

        // 1. Put a record and run `after_answer_printed`: housekeeping is not due, so `slices == 0`
        //    -> commits in 1 flip with 0 barriers/seals!
        let lent = home.open_store(&Settings::default(), Facts { boot: Some(1), nonce: 83 }, heap.alloc()).expect("open");
        let k = Key(ContentId::of("after answer zero slice key", b"v1"));
        lent.put(&blaze_cas::Key(k.0), blaze_keyed::kind::Namespace::LOCAL, blaze_keyed::kind::ReservedTag::Drive.tag(), b"hello")
            .expect("put");
        let cleanup = after_answer_printed(&cx, lent, &home, false, &clock, &err).expect("after answer printed");
        assert_eq!(cleanup.put.after_answer.commits, 1, "one foreground commit");
        assert_eq!(cleanup.put.after_answer.disk.barriers, 0, "zero foreground seal barriers on slices == 0");
        assert_eq!(cleanup.put.after_answer.disk.syncs, 0, "zero foreground syncs on slices == 0");
        assert_eq!(cleanup.slices, 0, "zero inline compaction slices");

        // Reopen in the same boot: committed record is readable immediately.
        let lent2 = home.open_store(&Settings::default(), Facts { boot: Some(1), nonce: 84 }, heap.alloc()).expect("reopen");
        let held = lent2.get(&blaze_cas::Key(k.0)).expect("get").expect("committed record readable");
        assert_eq!(held.bytes, b"hello");
        // No uncommitted puts -> `after_answer_printed` is a 0-commit, 0-barrier no-op even though
        // the previous segment is unsealed!
        let noop = after_answer_printed(&cx, lent2, &home, false, &clock, &err).expect("noop after answer");
        assert_eq!(noop.put.after_answer.commits, 0, "no-op command does not commit or seal");
        assert_eq!(noop.put.after_answer.disk.barriers, 0, "no-op command does 0 barriers");

        // 2. `gc_lock_is_free` returns true when unlocked, false while `gc.lock` is held, and true
        //    again after release — ensuring `BgGc::spawn` never spawns duplicate GC processes.
        assert!(crate::gc::gc_lock_is_free(&cx, home.dir()).expect("probe free gc.lock"), "gc.lock starts free");
        let held_lock = crate::gc::try_acquire_gc_lock(&cx, home.dir()).expect("acquire").expect("lock acquired");
        assert!(!crate::gc::gc_lock_is_free(&cx, home.dir()).expect("probe contended gc.lock"), "gc_lock_is_free returns false while GC holds gc.lock");
        drop(held_lock);
        assert!(crate::gc::gc_lock_is_free(&cx, home.dir()).expect("probe released gc.lock"), "gc.lock free again after drop");
    }

    #[test]
    fn default_local_durability_uses_unsynced_when_off_and_barrier_when_durable() {
        use blaze_cas::plain::CommitDurability;

        let unsynced = Settings::default();
        let durable = Settings { durable: true, ..Settings::default() };
        assert_eq!(default_local_durability(&unsynced), CommitDurability::Unsynced);
        assert_eq!(default_local_durability(&durable), CommitDurability::Barrier);

        let io = Io::simulated(SimWorld::new(85, Limits::USUAL));
        let cx = io.cx(BUDGET, TASK_FRAME).expect("root cx");
        let fs = io.need_fs().expect("fs");
        let clock = io.need_clock().expect("clock");
        let err = io.need_err().expect("err");
        let heap = blaze_own::testing::heap();
        let home = Home::of(&cx, &fs, Some("/h")).expect("home");

        let lent = home.open_store(&durable, Facts { boot: Some(1), nonce: 85 }, heap.alloc()).expect("open durable");
        let k = Key(ContentId::of("barrier default durability key", b"v1"));
        lent.put(&blaze_cas::Key(k.0), blaze_keyed::kind::Namespace::LOCAL, blaze_keyed::kind::ReservedTag::Drive.tag(), b"barrier-payload")
            .expect("put");
        let cleanup = after_answer_printed(&cx, lent, &home, false, &clock, &err).expect("commit");
        assert_eq!(cleanup.put.after_answer.commits, 1);
        assert!(
            cleanup.put.after_answer.disk.barriers >= 2,
            "durable local store commits with ordered barriers (segment + manifest log), got {}",
            cleanup.put.after_answer.disk.barriers
        );
        assert_eq!(cleanup.put.after_answer.disk.syncs, 0, "Barrier mode avoids F_FULLFSYNC");

        let reopened = home.open_store(&durable, Facts { boot: Some(1), nonce: 86 }, heap.alloc()).expect("reopen");
        let held = reopened.get(&blaze_cas::Key(k.0)).expect("get").expect("record readable");
        assert_eq!(held.bytes, b"barrier-payload");
    }

    #[test]
    fn forms_note_once_uses_has_root_and_single_small_commit_does_not_trigger_bg_gc() {
        let io = Io::simulated(SimWorld::new(87, Limits::USUAL));
        let cx = io.cx(BUDGET, TASK_FRAME).expect("root cx");
        let fs = io.need_fs().expect("fs");
        let clock = io.need_clock().expect("clock");
        let err = io.need_err().expect("err");
        let heap = blaze_own::testing::heap();
        let home = Home::of(&cx, &fs, Some("/h")).expect("home");

        let lent = home.open_store(&Settings::default(), Facts { boot: Some(1), nonce: 87 }, heap.alloc()).expect("open");
        assert!(!lent.has_root(plain_place::forms::OTHER_FORMS_LOOKED));
        let noted = plain_place::forms::note_once(&cx, &home.dir, &lent, &err).expect("first note_once");
        assert_eq!(noted, 0);
        after_answer_printed(&cx, lent, &home, false, &clock, &err).expect("commit note_once");

        let lent2 = home.open_store(&Settings::default(), Facts { boot: Some(1), nonce: 88 }, heap.alloc()).expect("reopen");
        assert!(lent2.has_root(plain_place::forms::OTHER_FORMS_LOOKED), "note_once must persist OTHER_FORMS_LOOKED root");
        let noted2 = plain_place::forms::note_once(&cx, &home.dir, &lent2, &err).expect("second note_once");
        assert_eq!(noted2, 0);
        let now_ns = lent2.wall_ns();
        let driven = lent2.into_store();
        assert!(driven.needs_housekeeping(now_ns), "1 unsealed segment keeps needs_housekeeping true");
        assert!(!driven.needs_bg_gc(now_ns), "1 small unsealed segment must not trigger detached bg GC spawn");
    }

    #[test]
    fn cx_par_map_range_and_for_each_mut_chunks_match_serial_and_preserve_order() {
        use blaze_cas::plain::{Par, Serial};

        let io = Io::simulated(SimWorld::new(89, Limits::USUAL));
        let cx = io.cx(BUDGET, TASK_FRAME).expect("root cx");
        let alloc = blaze_mem::Kept(blaze_mem::Tag::Store);

        // Exercise both the inline serial fast path (`workers == 1`) and the work-stealing
        // `scope.may_overlap` + `CachePadded<AtomicUsize>` cursor path (`workers == 4`).
        let par_one = CxPar::new(&cx);
        let par_many = CxPar { cx: &cx, workers: core::num::NonZeroU32::new(4).expect("non-zero") };

        let want = Serial.map_range(137, 9, alloc, |lo, hi| (lo, hi, lo.wrapping_mul(31).wrapping_add(hi)));
        let got_one = par_one.map_range(137, 9, alloc, |lo, hi| (lo, hi, lo.wrapping_mul(31).wrapping_add(hi)));
        let got_many = par_many.map_range(137, 9, alloc, |lo, hi| (lo, hi, lo.wrapping_mul(31).wrapping_add(hi)));
        assert_eq!(got_one.as_slice(), want.as_slice());
        assert_eq!(got_many.as_slice(), want.as_slice());

        // Empty and single-chunk edge cases.
        assert!(par_many.map_range(0, 8, alloc, |lo, hi| (lo, hi)).is_empty());
        assert_eq!(par_many.map_range(5, 16, alloc, |lo, hi| (lo, hi)).as_slice(), &[(0, 5)]);

        let mut serial_buf: blaze_own::Vec<usize, _> = blaze_own::Vec::with_capacity_in(137, alloc);
        let mut one_buf: blaze_own::Vec<usize, _> = blaze_own::Vec::with_capacity_in(137, alloc);
        let mut many_buf: blaze_own::Vec<usize, _> = blaze_own::Vec::with_capacity_in(137, alloc);
        for _ in 0..137 {
            serial_buf.push(0);
            one_buf.push(0);
            many_buf.push(0);
        }
        let fill = |start: usize, chunk: &mut [usize]| {
            for (i, slot) in chunk.iter_mut().enumerate() {
                *slot = start.saturating_add(i).wrapping_mul(17).wrapping_add(3);
            }
        };
        Serial.for_each_mut_chunks(serial_buf.as_mut_slice(), 11, fill);
        par_one.for_each_mut_chunks(one_buf.as_mut_slice(), 11, fill);
        par_many.for_each_mut_chunks(many_buf.as_mut_slice(), 11, fill);
        assert_eq!(one_buf.as_slice(), serial_buf.as_slice());
        assert_eq!(many_buf.as_slice(), serial_buf.as_slice());
    }

    #[test]
    fn store_snapshot_expansion_demands_is_cache_padded() {
        assert!(
            core::mem::align_of::<bstd::sync::CachePadded<AtomicU64>>() >= 64,
            "StoreSnapshot::expansion_demands must be cache-line padded",
        );
        let io = Io::simulated(SimWorld::new(91, Limits::USUAL));
        let cx = io.cx(BUDGET, TASK_FRAME).expect("root cx");
        let fs = io.need_fs().expect("fs");
        let clock = io.need_clock().expect("clock");
        let err = io.need_err().expect("err");
        let heap = blaze_own::testing::heap();
        let home = Home::of(&cx, &fs, Some("/h")).expect("home");
        let lent = home.open_store_par(&cx, &Settings::default(), Facts { boot: Some(1), nonce: 91 }, heap.alloc()).expect("open_store_par");
        let k = Key(ContentId::of("cx_par snapshot demand key", b"v1"));
        lent.put(&blaze_cas::Key(k.0), blaze_keyed::kind::Namespace::LOCAL, blaze_keyed::kind::ReservedTag::Drive.tag(), b"payload")
            .expect("put");
        let par = CxPar { cx: &cx, workers: core::num::NonZeroU32::new(4).expect("non-zero") };
        lent.commit_par(&par).expect("commit_par");
        let store = CommandStore::new(&cx, &lent, false, 0, None, clock, &err, heap.alloc()).expect("command store");
        let snap = store.snapshot().expect("snapshot").expect("some snapshot");
        assert_eq!(snap.expansion_demands(), 0);
        let got = snap.expansion_get(k).expect("expansion_get").expect("present");
        assert_eq!(got, b"payload");
        assert_eq!(snap.expansion_demands(), 1);
    }
}
