//! The front end's [`Load`] over the store (drive.md, "End state: the whole pipeline on the
//! command", piece 6, STORE LOAD).
//!
//! front2 checks a body over `Records` (front2 source.rs), which asks a [`Load`] for a crate's
//! names section, its items section and the texts their rows point into, each the first time
//! a question reaches it. Here those come from the command's store: a crate's two sections are
//! read from its `CrateScope` record ([`crate::names::sections`]: the record names.rs `run`
//! files), found by the record's key as the request machine gave it (an [`Input`]); the texts
//! come from the world's trees (`/<tree>/<path>`: the checkout, the toolchain's packs, the
//! fetched packages' packs) through blaze-scope's [`SourceTexts`].
//!
//! - A record the store does not hold is [`LoadError::NotInStore`], naming its key: the caller
//!   asks the machine to make it (the pull). Nothing here makes, guesses or retries.
//! - A record read under one crate's name must be that crate's: both sections carry their
//!   crate's key, checked once at the read ([`crate_records`]); a mismatch is
//!   [`LoadError::OtherCrate`].
//! - Each section is copied once, 16-aligned, into the caller's allocator (the store's
//!   `get_into` reads a record into a frame; the records' readers need 16-aligned bytes). It is
//!   then read in place for the command: nothing decodes it whole and no second cache holds it.
//! - A macro call's expansion text (piece 16; runtime.md plan row B3) is read from the committed
//!   store snapshot only when `Load::expansion` asks for its directory key. A call the directory
//!   names no text for, a record the store lacks (`ExpansionRecordsError::Missing`, naming the
//!   call and key) or a damaged row is answered none and filed in [`StoreLoad::unserved`].

use core::cell::RefCell;

use blaze_drive::{Input, Key, Name};
use blaze_core::SharedSet;
use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_ladder::front2::source::{Load, LoadedExpansion};
use blaze_own::{AVec, Alloc, Bump, Vec};
use blaze_scope::items_record::{CurrentFile, ItemsRecord, ItemsRecordError};
use blaze_scope::names_record::{NamesRecordError, Verified};
use blaze_scope::package_path::PackagePath;
use blaze_scope::resolve::SourceTexts;
use bstd::cx::Cx;
use bstd::mem::Frame;

use blaze_scope::expansion_records::{ExpansionRecord, ExpansionRecordsError};

use crate::CliError;
use crate::names::expansion_texts::ExpansionTexts;
use crate::names::sections::{self, SectionsError};
use crate::store::{Store, StoreSnapshot};

/// One crate's names and items sections, read from its `CrateScope` record, each 16-aligned.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CrateRecords<'n> {
    pub key: CrateItemKey,
    /// Present checkout surfaces in the drive record's read list, sorted by
    /// subject. A directory module's surface identifies the slot that owns its bodies.
    pub file_slots: &'n [blaze_ids::ContentId],
    pub names: &'n [u8],
    /// The names section's one form check this process (rule 74): every later read of `names`
    /// opens it from here, in O(1), with no check again.
    pub names_proof: Verified<'n>,
    pub items: &'n [u8],
    /// The items section's edition, read by its one open at the load ([`checked_records`]):
    /// what a reader that needs only the edition takes, never opening the section again (a
    /// proc-macro caller's edition, run_names.rs; before 2026-11-18 every names-records build
    /// opened every crate's items section for it, re-checking its strings).
    pub edition: blaze_surface::Edition,
    /// The call directory. Text and origin columns stay in their own store records.
    pub expansions: ExpansionTexts<'n>,
}

/// Why a crate's sections could not be served (rule: blaze-cli front_load.rs).
#[derive(Debug)]
pub(crate) enum LoadError {
    /// The store holds no record at the key the machine gave for `name`: the caller has it made.
    NotInStore { name: Name, at: Key },
    /// The bytes at the key are not a whole record of this tool's form.
    NotWhole { name: Name, why: &'static str },
    /// The record's payload is not a `CrateScope` payload of this form.
    Sections { name: Name, error: SectionsError },
    /// A section does not open as its record.
    Names { name: Name, error: NamesRecordError },
    Items { name: Name, error: ItemsRecordError },
    /// The expansion texts section does not open.
    Expansions { name: Name, error: ExpansionRecordsError },
    /// The record read under crate `asked`'s name holds crate `found`'s section `which`.
    OtherCrate { name: Name, asked: CrateItemKey, found: CrateItemKey, which: &'static str },
    /// The store could not be read.
    Store(CliError),
}

impl core::fmt::Display for LoadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            LoadError::NotInStore { name, at } => write!(f, "not in the store: {name:?} at key {at:?} (rule: front_load.rs)"),
            LoadError::NotWhole { name, why } => write!(f, "the record of {name:?} is not a whole record: {why} (rule: front_load.rs)"),
            LoadError::Sections { name, error } => write!(f, "the record of {name:?}: {error}"),
            LoadError::Names { name, error } => write!(f, "the names section of {name:?} does not open: {error:?} (rule: front_load.rs)"),
            LoadError::Items { name, error } => write!(f, "the items section of {name:?} does not open: {error:?} (rule: front_load.rs)"),
            LoadError::Expansions { name, error } => write!(f, "the expansion texts of {name:?}: {error}"),
            LoadError::OtherCrate { name, asked, found, which } => {
                write!(f, "the record of {name:?} holds the {which} section of crate {found:?}, not of {asked:?} (rule: front_load.rs)")
            }
            LoadError::Store(error) => write!(f, "the store: {error}"),
        }
    }
}

/// The sections of a `CrateScope` record's `payload`, read under `name` for crate `key`, with
/// expansion text records deferred to the keyed demand seam.
pub(crate) fn crate_records<'n>(name: Name, payload: &[u8], key: CrateItemKey, bump: Bump<'n>) -> Result<CrateRecords<'n>, LoadError> {
    let parts = sections::sections(payload).map_err(|error| LoadError::Sections { name, error })?;
    let names = blaze_scope::resolve::aligned(parts.names, bump);
    let items = blaze_scope::resolve::aligned(parts.items, bump);
    let expansions = blaze_scope::resolve::aligned(parts.expansions, bump);
    checked_records(name, key, names, items, expansions, bump.alloc())
}

/// A crate's three sections, each 16-aligned where it lies (copied by [`crate_records`], or in
/// a segment's mapping, engine_records.rs), checked once as `key`'s (rule 74).
pub(crate) fn checked_records<'n>(
    name: Name,
    key: CrateItemKey,
    names: &'n [u8],
    items: &'n [u8],
    expansions: &'n [u8],
    alloc: Alloc<'n>,
) -> Result<CrateRecords<'n>, LoadError> {
    let expansions = ExpansionTexts::open(expansions).map_err(|error| LoadError::Expansions { name, error })?;
    let names_proof = Verified::check(names).map_err(|error| LoadError::Names { name, error })?;
    let found = names_proof.key();
    if found != key {
        return Err(LoadError::OtherCrate { name, asked: key, found, which: "names" });
    }
    // The section's one open this command: its key checked, its edition kept beside it. Every
    // later reader of its rows opens it once per job, in the job's names records
    // (front2 source.rs `Records::items_record`).
    let opened = ItemsRecord::open(items, alloc).map_err(|error| LoadError::Items { name, error })?;
    let (found, edition) = (opened.key(), opened.edition());
    if found != key {
        return Err(LoadError::OtherCrate { name, asked: key, found, which: "items" });
    }
    Ok(CrateRecords { key, file_slots: &[], names, names_proof, items, edition, expansions })
}

/// The crate `key`'s sections from the record `input` names, read from `store` by its key
/// (`input.at`) in place and copied into `alloc` ([`crate_records`]). Expansion text records are
/// opened later by `StoreLoad::expansion`.
pub(crate) fn read_crate<'n>(cx: &Cx, store: &mut dyn Store<'_>, input: &Input, key: CrateItemKey, bump: Bump<'n>) -> Result<CrateRecords<'n>, LoadError> {
    let cx = cx.span("front load: read a crate's records");
    let Some(held) = store.get(&cx, input.at).map_err(LoadError::Store)? else {
        return Err(LoadError::NotInStore { name: input.name, at: input.at });
    };
    match blaze_drive::record::open(held.bytes) {
        Ok(blaze_drive::record::Stored::Whole(view)) => {
            let mut slots = Vec::new_in(bump);
            slots.extend(view.reads().filter_map(|(name, out)| {
                (name.kind == blaze_drive::Kind::Surface && out != blaze_drive::key::ABSENT).then_some(name.subject)
            }));
            slots.sort_unstable();
            let mut records = crate_records(input.name, view.payload, key, bump)?;
            records.file_slots = slots.into_bump_slice();
            Ok(records)
        }
        Ok(blaze_drive::record::Stored::Shape(_)) => Err(LoadError::NotWhole { name: input.name, why: "a shape record, which holds no payload" }),
        Err(_) => Err(LoadError::NotWhole { name: input.name, why: "its bytes are not a record of this tool's form" }),
    }
}

/// One crate of a check scope whose `CrateScope` record is not opened when its scope is made: it
/// is opened at the first question about it ([`ScopeRecords`], piece C (a)). What its readers
/// need without its record is here: its names input (the record's key), its crate key, its place
/// in the crate list, its name and its package's directory.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Unopened {
    pub(crate) input: Input,
    pub(crate) key: CrateItemKey,
    pub(crate) world_at: usize,
    pub(crate) name: &'static str,
    pub(crate) package: &'static str,
}

/// THE REQUEST'S SCOPE RECORDS OPENED BY NEED (piece C (a)): each crate's `CrateScope` record
/// read in place from the committed snapshot and its sections checked once a request
/// (front_load.rs `checked_records` through engine_records.rs `open_record`, rule 74), at the
/// first question of any body job about it, and the same borrow for every later one, on any
/// worker: one slot a crate of the list, by its place. A crate no question reaches is never
/// opened. A record that does not open is kept as the words of why, so every job that asks it
/// refuses its bodies by those words ([`StoreLoad::refused`]), never answers without it (rule 3).
///
/// FLOOR: the crates the request's body checks ask a question of, each opened once; the ratio is
/// [`ByNeedCounts::opened`] over that, 1.0 by construction (a slot opens once). Before, every
/// crate of every opened scope was opened at the setup (tempo, one edited body: 773 records,
/// 823 MB checked).
pub(crate) struct ScopeRecords<'r> {
    snapshot: &'r StoreSnapshot,
    heap: &'static blaze_own::Heap,
    slots: blaze_own::Vec<std::sync::OnceLock<Result<CrateRecords<'r>, &'static str>>, blaze_own::SharedAlloc<'static>>,
    asks: core::sync::atomic::AtomicU64,
    opened: core::sync::atomic::AtomicU64,
    bytes: core::sync::atomic::AtomicU64,
}

/// What a request's [`ScopeRecords`] did: asks of it, records opened (each once) and their bytes.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct ByNeedCounts {
    pub(crate) asks: u64,
    pub(crate) opened: u64,
    pub(crate) bytes: u64,
}

impl<'r> ScopeRecords<'r> {
    /// No crate opened yet, for a crate list of `crates` crates, over the committed `snapshot`.
    pub(crate) fn new(snapshot: &'r StoreSnapshot, crates: usize, heap: &'static blaze_own::Heap) -> ScopeRecords<'r> {
        let slots = blaze_own::Vec::from_iter_in((0..crates).map(|_| std::sync::OnceLock::new()), heap.shared());
        ScopeRecords { snapshot, heap, slots, asks: core::sync::atomic::AtomicU64::new(0), opened: core::sync::atomic::AtomicU64::new(0), bytes: core::sync::atomic::AtomicU64::new(0) }
    }

    /// What it did so far.
    pub(crate) fn counts(&self) -> ByNeedCounts {
        use core::sync::atomic::Ordering::Relaxed;
        ByNeedCounts { asks: self.asks.load(Relaxed), opened: self.opened.load(Relaxed), bytes: self.bytes.load(Relaxed) }
    }

    /// Crate `one`'s records: opened at the first ask, the same after; `Err`: the words of why
    /// its record did not open, the same for every ask.
    fn records(&self, one: &Unopened) -> Result<CrateRecords<'r>, &'static str> {
        use core::sync::atomic::Ordering::Relaxed;
        self.asks.fetch_add(1, Relaxed);
        let Some(slot) = self.slots.get(one.world_at) else {
            return Err("its place is past the request's crate list (front_load.rs `ScopeRecords`)");
        };
        *slot.get_or_init(|| {
            let mut counts = crate::engine_records::ReadCounts::default();
            let read = self.read(one, &mut counts);
            self.opened.fetch_add(counts.records, Relaxed);
            self.bytes.fetch_add(counts.bytes, Relaxed);
            read.map_err(|why| crate::engine_head_notes::leak(self.heap.alloc(), format_args!("{why}")))
        })
    }

    /// The one read of crate `one`'s record: from the snapshot by its key, in place, opened.
    fn read(&self, one: &Unopened, counts: &mut crate::engine_records::ReadCounts) -> Result<CrateRecords<'r>, ByNeedError> {
        let name = one.input.name;
        let bytes = self.snapshot.get(one.input.at).map_err(|error| ByNeedError::Snapshot { name, at: one.input.at, error })?;
        let bytes = bytes.ok_or(ByNeedError::Read(LoadError::NotInStore { name, at: one.input.at }.into()))?;
        let record = blaze_cas::plain::Aligned16::new(bytes).map_err(|why| ByNeedError::Read(crate::engine_records::ReadError::NotInPlace { name, why: blaze_cas::plain::NotInPlace::Misaligned(why) }))?;
        crate::engine_records::open_record::<crate::engine_records::InPlace>(name, record, one.key, counts, self.heap.alloc()).map_err(ByNeedError::Read)
    }
}

/// Whether crate `one`'s record is in the committed `snapshot` where a reader can take it in
/// place, without opening it: what a scope made by need checks of each crate, so a cut at a
/// crate whose record is gone is made at the setup as the whole scope's read makes it
/// (engine_bodies.rs `scope_by_need`). `Err`: the snapshot's own refusal, the caller's to stop on.
pub(crate) fn present(snapshot: &StoreSnapshot, one: &Unopened) -> Result<Result<(), crate::engine_records::ReadError>, blaze_cas::plain::SnapshotError> {
    let name = one.input.name;
    Ok(match snapshot.get(one.input.at)? {
        None => Err(LoadError::NotInStore { name, at: one.input.at }.into()),
        Some(bytes) => blaze_cas::plain::Aligned16::new(bytes).map(|_| ()).map_err(|why| crate::engine_records::ReadError::NotInPlace { name, why: blaze_cas::plain::NotInPlace::Misaligned(why) }),
    })
}

/// Why a crate's record opened by need was not served ([`ScopeRecords`]).
#[derive(Debug)]
enum ByNeedError {
    /// The committed snapshot refused the read at the record's key.
    Snapshot { name: Name, at: Key, error: blaze_cas::plain::SnapshotError },
    /// Not in the store, not in place, or it does not open (engine_records.rs `open_record`).
    Read(crate::engine_records::ReadError),
}

impl core::fmt::Display for ByNeedError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ByNeedError::Snapshot { name, at, error } => write!(f, "the store's snapshot did not read the record of {name:?} at key {at:?}: {error:?} (front_load.rs `ScopeRecords`)"),
            ByNeedError::Read(error) => write!(f, "{error}"),
        }
    }
}

/// A source of scope records opened by need, each borrowed for as long as the source is: the
/// request's [`ScopeRecords`] behind a lifetime-free object, so a job's frame-lived reader and
/// every view of the run (engine_bodies.rs `BodyRunner`, `Shared`) borrow the request's slots
/// at their own shorter life (a `ScopeRecords<'r>` itself is invariant in `'r`: its slots are
/// cells).
pub(crate) trait OpenByNeed: Sync {
    fn open<'s>(&'s self, one: &Unopened) -> Result<CrateRecords<'s>, &'static str>;
}

impl OpenByNeed for ScopeRecords<'_> {
    fn open<'s>(&'s self, one: &Unopened) -> Result<CrateRecords<'s>, &'static str> {
        self.records(one)
    }
}

/// The crates a [`StoreLoad`] serves, in reader order (the own crate at 0): THE SEAM of piece C
/// (a) (rule 78). [`CratesFrom::Opened`]: every crate opened before the reader is made (the
/// Heads stage, `blaze run`, the oracle arm of the Bodies stage); [`CratesFrom::ByNeed`]: the own
/// crate opened, every other opened at its first question ([`ScopeRecords`]). Both answer every
/// question alike: tested equal by the bodies' oracle (engine_bodies.rs `Whole` against
/// `ByNeed`).
#[derive(Clone, Copy)]
pub(crate) enum CratesFrom<'n> {
    Opened(&'n [CrateRecords<'n>]),
    ByNeed { own: CrateRecords<'n>, rest: &'n [Unopened], records: &'n dyn OpenByNeed },
}

impl<'n> CratesFrom<'n> {
    pub(crate) fn len(&self) -> usize {
        match self {
            CratesFrom::Opened(crates) => crates.len(),
            CratesFrom::ByNeed { rest, .. } => rest.len() + 1,
        }
    }

    /// Reader crate `k`'s key, known without its record.
    pub(crate) fn key(&self, k: usize) -> Option<CrateItemKey> {
        match self {
            CratesFrom::Opened(crates) => crates.get(k).map(|one| one.key),
            CratesFrom::ByNeed { own, rest, .. } => match k {
                0 => Some(own.key),
                k => rest.get(k - 1).map(|one| one.key),
            },
        }
    }

    /// Reader crate `k`'s records, opened by need on [`CratesFrom::ByNeed`]: `Err` names why
    /// its record did not open.
    pub(crate) fn records(&self, k: usize) -> Option<Result<CrateRecords<'n>, &'static str>> {
        match self {
            CratesFrom::Opened(crates) => crates.get(k).copied().map(Ok),
            CratesFrom::ByNeed { own, rest, records } => match k {
                0 => Some(Ok(*own)),
                k => rest.get(k - 1).map(|one| records.open(one)),
            },
        }
    }
}

/// front2's [`Load`] over crates read from the store: `crates`' crate `i` is the reader's crate
/// `i` (the program's own at 0, as `Records::new`'s keys), `texts` the world's trees.
pub(crate) struct StoreLoad<'n> {
    crates: CratesFrom<'n>,
    /// The first crate (by its reader place) whose record a question asked and that did not
    /// open, with the words why ([`CratesFrom::ByNeed`] only): sticky, since front2 keeps a
    /// crate it was refused as none for its job (blaze-scope names_record.rs `RecordNames`), so
    /// every body of the job after it is refused by these words ([`StoreLoad::refused`]).
    refused: core::cell::Cell<Option<(usize, &'static str)>>,
    /// `packages[i]`: the directory the world placed reader crate `i`'s package at (its
    /// `CrateInput::shown_from`): a record's package path is joined onto it to read (D96).
    packages: &'n [&'n str],
    texts: &'n dyn SourceTexts<'n>,
    snapshot: Option<&'n StoreSnapshot>,
    expansion_keys: Option<&'n SharedSet<Key, blaze_own::SharedAlloc<'static>>>,
    outlines: Option<&'n [crate::engine_tasks::Outlined<'n>]>,
    /// Each (crate, call) whose expansion was asked and none was served, with why when its
    /// directory names one ([`ExpansionRecordsError::Missing`]: the store lacks its record; a
    /// damaged row); `None`: the crate made no text for the call.
    unserved: RefCell<AVec<'n, Unserved>>,
    alloc: Alloc<'n>,
}

/// One expansion a question asked that was not served ([`StoreLoad::unserved`]).
#[derive(Clone, Debug)]
pub(crate) struct Unserved {
    pub(crate) krate: usize,
    pub(crate) call: ItemKey,
    pub(crate) why: Option<ExpansionReadError>,
}

#[derive(Clone, Debug)]
pub(crate) enum ExpansionReadError {
    Record(ExpansionRecordsError),
    NoSnapshot { call: ItemKey, key: Key },
    Snapshot { call: ItemKey, key: Key, error: blaze_cas::plain::SnapshotError },
}

impl core::fmt::Display for ExpansionReadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Record(error) => write!(f, "{error}"),
            Self::NoSnapshot { call, key } => write!(f, "no committed snapshot for expansion {key:?} of call {call:?}"),
            Self::Snapshot { call, key, error } => write!(f, "reading expansion {key:?} of call {call:?}: {error}"),
        }
    }
}

impl<'n> StoreLoad<'n> {
    #[cfg(test)]
    pub(crate) fn new(crates: &'n [CrateRecords<'n>], packages: &'n [&'n str], texts: &'n dyn SourceTexts<'n>, alloc: Alloc<'n>) -> StoreLoad<'n> {
        Self::from_parts(crates, packages, texts, None, None, None, alloc)
    }

    /// Both (the merge of two lanes, 2026-11-18): the snapshot for lazy expansion texts and
    /// the outlines for current file ranges. The engine's Bodies path passes both.
    pub(crate) fn from_parts(
        crates: &'n [CrateRecords<'n>],
        packages: &'n [&'n str],
        texts: &'n dyn SourceTexts<'n>,
        snapshot: Option<&'n StoreSnapshot>,
        expansion_keys: Option<&'n SharedSet<Key, blaze_own::SharedAlloc<'static>>>,
        outlines: Option<&'n [crate::engine_tasks::Outlined<'n>]>,
        alloc: Alloc<'n>,
    ) -> Self {
        Self::from_crates(CratesFrom::Opened(crates), packages, texts, snapshot, expansion_keys, outlines, alloc)
    }

    /// [`StoreLoad::from_parts`] over either source of crates ([`CratesFrom`]).
    pub(crate) fn from_crates(
        crates: CratesFrom<'n>,
        packages: &'n [&'n str],
        texts: &'n dyn SourceTexts<'n>,
        snapshot: Option<&'n StoreSnapshot>,
        expansion_keys: Option<&'n SharedSet<Key, blaze_own::SharedAlloc<'static>>>,
        outlines: Option<&'n [crate::engine_tasks::Outlined<'n>]>,
        alloc: Alloc<'n>,
    ) -> Self {
        StoreLoad { crates, refused: core::cell::Cell::new(None), packages, texts, snapshot, expansion_keys, outlines, unserved: RefCell::new(AVec::new_in(alloc)), alloc }
    }

    /// The crates' keys in reader order: `Records::new`'s `keys`.
    pub(crate) fn keys(&self, frame: &'n Frame) -> &'n [CrateItemKey] {
        let mut keys = Vec::with_capacity_in(self.crates.len(), frame);
        keys.extend((0..self.crates.len()).filter_map(|k| self.crates.key(k)));
        keys.into_bump_slice()
    }

    /// The first crate (its reader place) a question of this job asked whose record did not
    /// open by need, and why: every body checked from the first such ask on is refused by it.
    pub(crate) fn refused(&self) -> Option<(usize, &'static str)> {
        self.refused.get()
    }

    /// Reader crate `k`'s records, opened by need: a record that did not open is kept as this
    /// job's refusal ([`StoreLoad::refused`]) and answered none.
    fn records_of(&self, k: usize) -> Option<CrateRecords<'n>> {
        match self.crates.records(k)? {
            Ok(records) => Some(records),
            Err(why) => {
                if self.refused.get().is_none() {
                    self.refused.set(Some((k, why)));
                }
                None
            }
        }
    }

    /// The macro calls whose expansion a question asked and none was served, in asked order.
    pub(crate) fn unserved(&self) -> core::cell::Ref<'_, AVec<'n, Unserved>> {
        self.unserved.borrow()
    }

    /// `answer` for the call `call` of crate `krate`: `Some` served, else filed unserved.
    fn served<T>(&self, krate: usize, call: ItemKey, answer: Result<Option<T>, ExpansionReadError>) -> Option<T> {
        let (served, why) = match answer {
            Ok(value) => (value, None),
            Err(error) => (None, Some(error)),
        };
        if served.is_none() {
            self.unserved.borrow_mut().push(Unserved { krate, call, why });
        }
        served
    }

    /// The ONE store read of `call`'s expansion record: its text and both origin parts, each
    /// borrowed where it lies (F4: the text and its marks never read the record twice).
    fn expansion_read(&self, krate: usize, call: ItemKey) -> Result<Option<LoadedExpansion<'n>>, ExpansionReadError> {
        let Some(records) = self.records_of(krate) else { return Ok(None) };
        let Some(entry) = records.expansions.entry(call).map_err(ExpansionReadError::Record)? else { return Ok(None) };
        let key = Key(entry.record);
        let snapshot = self.snapshot.ok_or(ExpansionReadError::NoSnapshot { call, key })?;
        if let Some(keys) = self.expansion_keys {
            let _ = keys.pin().insert(key);
        }
        let bytes = snapshot.expansion_get(key).map_err(|error| ExpansionReadError::Snapshot { call, key, error })?
            .ok_or(ExpansionReadError::Record(ExpansionRecordsError::Missing { call, key: entry.record }))?;
        let record = ExpansionRecord::open(bytes).map_err(ExpansionReadError::Record)?;
        let text = entry.text(&record).map_err(ExpansionReadError::Record)?;
        Ok(Some(LoadedExpansion { text, origins: Some((entry.call_part, record.column)) }))
    }
}

impl<'n> Load<'n> for StoreLoad<'n> {
    fn names(&self, krate: usize) -> Option<&'n [u8]> {
        self.records_of(krate).map(|c| c.names)
    }

    fn items(&self, krate: usize) -> Option<&'n [u8]> {
        self.records_of(krate).map(|c| c.items)
    }

    fn file(&self, krate: usize, path: PackagePath<'_>) -> Option<&'n str> {
        let at = path.in_package(self.packages.get(krate)?, self.alloc)?;
        self.texts.read(krate, &at)
    }

    fn file_items(&self, krate: usize, index: u32, path: PackagePath<'_>, surface: ContentId, edition: blaze_surface::Edition, bump: Bump<'n>) -> Result<Option<CurrentFile<'n>>, ItemsRecordError> {
        let Some(package) = self.packages.get(krate) else { return Ok(None) };
        let Some(full) = path.in_package(package, self.alloc) else { return Ok(None) };
        let Some(text) = self.texts.read(krate, &full) else { return Ok(None) };
        // The outline planner covers checkout sources. The packed libraries and the
        // text-only driver supply immutable texts from which the view is made once.
        let (Some(outlines), Some(real)) = (self.outlines, full.strip_prefix("/0/")) else {
            return CurrentFile::from_text(index, text, edition, bump).map(Some);
        };
        // A physical x/mod.rs can be a literal file or the module slot x.rs.
        let locate = |slot: Name| outlines.binary_search_by_key(&slot, |one| one.name).ok()
            .and_then(|i| outlines.get(i)).filter(|one| one.path.as_slice() == real.as_bytes());
        let one = locate(Name::file(real.as_bytes())).or_else(|| {
            let stem = real.strip_suffix("/mod.rs")?;
            let mut slot = AVec::from_slice_in(stem.as_bytes(), self.alloc);
            slot.extend_from_slice(b".rs");
            locate(Name::file(&slot))
        }).ok_or(ItemsRecordError::FileItemsUnavailable { file: index, what: "the checkout path has no planned file outline" })?;
        if one.edition != edition.byte() {
            return Err(ItemsRecordError::FileItemsUnavailable { file: index, what: "the planned outline has a different edition" });
        }
        let record = one.made.as_ref().map_err(|_| ItemsRecordError::FileItemsUnavailable { file: index, what: "the outline task failed" })?;
        let physical = crate::signature::SignatureFile::new(real.as_bytes());
        let expected = blaze_ids::Hasher::new(crate::work::FILE_SURFACE_PHYSICAL_DOMAIN).id(&physical.subject()).id(&surface).finish();
        let found = crate::engine_tasks::surface_of(record).ok_or(ItemsRecordError::FileItemsUnavailable { file: index, what: "the planned outline has no surface" })?;
        let Some(key) = self.crates.key(krate) else { return Ok(None) };
        let stale = |saw, found| ItemsRecordError::Stale { krate: key, file: Some(index), call: None, saw, found };
        if found != expected { return Err(stale(expected, found)); }
        let payload = record.payload_bytes().map_err(|_| ItemsRecordError::FileItemsUnavailable { file: index, what: "the planned outline payload is unavailable" })?;
        let view = blaze_surface::FileItemsView::read(payload).map_err(|why| ItemsRecordError::FileItemsRead { file: index, why })?;
        let digest = ContentId::of(crate::sweep::FILE_DOMAIN, text.as_bytes());
        if view.file() != digest { return Err(stale(view.file(), digest)); }
        Ok(Some(CurrentFile { text, surface, view }))
    }

    fn expansion(&self, krate: usize, call: ItemKey) -> Option<LoadedExpansion<'n>> {
        self.served(krate, call, self.expansion_read(krate, call))
    }
}

#[cfg(test)]
#[path = "names/origins_parts_tests.rs"]
mod origins_parts_tests;

#[cfg(test)]
mod tests {
    //! Piece 6's proof: crates solved in memory, their sections written as names.rs `run`
    //! writes them (`sections::write`), read back through [`StoreLoad`] under `Records`: every
    //! answer about every item equals the in-memory names' (the comparison of blaze-ladder's
    //! tests/front2_records.rs `answers`, cut to the questions a body's check asks of an item);
    //! a record the store lacks is "not in the store"; a section read under another crate's
    //! key is caught.
    //!
    //! Prints `front load: N items equal over the store`.

    use super::*;
    use blaze_drive::Kind;
    use blaze_ids::ContentId;
    use blaze_ladder::front2::source::{Log, NamesSource, Records};
    use bstd::mem::Frame;
    use blaze_scope::graph::Sysroot;
    use blaze_scope::resolve::{CrateInput, Names};
    use blaze_surface::{Edition, Namespace};
    use blaze_unit_graph::{Implicit, Prelude, StdIs};
    use bstd::io::{Io, Limits, SimWorld};

    /// `made` and `Made` are written by a macro call (piece 16): their text is the call's
    /// expansion, read through [`StoreLoad::expansion`] from the record's expansion texts.
    const LIB: &str = "#![no_core]\npub mod m {\n    pub struct Thing;\n    pub fn make() -> Thing { Thing }\n}\npub use m::Thing;\nfn hidden() {}\nmacro_rules! write_two {\n    () => {\n        pub struct Made;\n        pub fn made() -> Made { Made }\n    };\n}\nwrite_two!();\n";
    const APP: &str = "#![no_core]\nuse lib::m::make;\npub struct Own;\nfn main() { let _ = make(); }\nmod inner { pub fn g() {} }\n";

    /// The files of the two crates, at their world paths.
    pub(super) struct Files;

    impl<'a> SourceTexts<'a> for Files {
        fn read(&self, _krate: usize, path: &str) -> Option<&'a str> {
            match path {
                "/0/app/lib.rs" => Some(APP),
                "/1/lib/lib.rs" => Some(LIB),
                _ => None,
            }
        }
    }

    const RUST: Implicit = Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: false, entry: false };

    fn input(name: &'static str, root: &'static str, deps: &'static [(&'static str, usize)]) -> CrateInput<'static> {
        CrateInput {
            name,
            key: CrateItemKey(ContentId::of("blaze-cli front load test crate", name.as_bytes())),
            edition: Edition::E2021,
            implicit: RUST,
            root,
            shown_from: "/",
            features: &[],
            deps,
            cfg: &[],
            script: None,
        }
    }

    pub(super) fn solved() -> Names<'static> {
        let inputs = [input("app", "/0/app/lib.rs", &[("lib", 1)]), input("lib", "/1/lib/lib.rs", &[])];
        let sysroot = Sysroot { core: None, alloc: None, std: None, bstd: None };
        let names = blaze_scope::resolve::resolve_crates(&inputs, sysroot, &Files, &blaze_scope::task::Serial, blaze_own::testing::bump());
        names.unwrap_or_else(|e| panic!("the two crates' names: {e:?}"))
    }

    /// The same solved directory can be read against a complete generation or one with
    /// its expansion puts deliberately omitted.
    pub(super) fn kept(names: &Names<'_>, texts: bool) -> crate::store::SharedSnapshot {
        use crate::store::{CommandStore, Facts, Home};
        let io = Io::simulated(SimWorld::new(31, Limits::USUAL));
        let cx = io.cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME).expect("a root context");
        let err = io.need_err().expect("stderr");
        let home = Home::of(&cx, &io.need_fs().expect("files"), Some("/h")).expect("the test home");
        let alloc = blaze_own::testing::heap().alloc();
        let lent = home.open_store(&blaze_cas::plain::Settings::default(), Facts { boot: Some(1), nonce: 31 }, alloc).expect("the store");
        let mut store = CommandStore::new(&cx, &lent, false, 0, None, io.need_clock().expect("clock"), &err, alloc).expect("the command store");
        if texts {
            for krate in names.crates() {
                let split = blaze_scope::expansion_records::write(krate, alloc).expect("the expansion records");
                for record in &split.records {
                    store.put_record(&cx, Key(record.key), blaze_keyed::kind::ReservedTag::Expansion.tag(), &[], &record.bytes).expect("the expansion put");
                }
            }
        }
        store.commit_for_body_reads().expect("the committed test snapshot")
    }

    /// Each crate's `CrateScope` payload as names.rs `run` writes it.
    pub(super) fn payloads(names: &Names<'_>) -> std::vec::Vec<blaze_own::Vec<u8, std::alloc::Global>> {
        let g = Alloc::global();
        let mut out = std::vec::Vec::new();
        for c in names.crates().iter() {
            let n = blaze_scope::names_record::encode(c, g).unwrap_or_else(|e| panic!("names record of `{}`: {e:?}", c.name));
            let i = blaze_scope::items_record::encode(c, g).unwrap_or_else(|e| panic!("items record of `{}`: {e:?}", c.name));
            let split = blaze_scope::expansion_records::write(c, g).unwrap_or_else(|e| panic!("expansions of `{}`: {e}", c.name));
            out.push(sections::write(0, [&[], &[], &n[..], &i[..], &[], &split.directory[..], &[], &[]]));
        }
        out
    }

    pub(super) fn scope_name(key: CrateItemKey) -> Name {
        blaze_drive::key::crate_scope_name(&key)
    }

    /// Every answer about every item of every crate, as text: one function over both sources.
    fn answers<'n, S: NamesSource<'n>>(names: S) -> std::vec::Vec<std::string::String> {
        let mut out = std::vec::Vec::new();
        for krate in 0..names.crate_count() {
            let mut items = std::vec::Vec::new();
            names.each_item(krate, &mut |key, module| items.push((key, module)));
            items.sort();
            for (key, module) in items {
                let name = names.name_of(key);
                let text = names.text_of(key).map(|t| t.text);
                let binds = name.map(|n| (names.binds(module, Namespace::Types, n), names.binds(module, Namespace::Values, n)));
                out.push(format!(
                    "crate {krate} item {key:?} in {module:?}: name {name:?} kind {:?} home {:?} place {:?} text {text:?} binds {binds:?}",
                    names.kind_of_item(key),
                    names.home_of(key),
                    names.place_of(key),
                ));
            }
        }
        out
    }

    /// The answers in memory, the answers over the store load, whether the reader logged, and
    /// the calls whose texts the load could not serve.
    fn both(texts: bool) -> (std::vec::Vec<std::string::String>, std::vec::Vec<std::string::String>, bool, std::vec::Vec<Unserved>) {
        let names = solved();
        let solved_answers = answers(&names);
        let keys: std::vec::Vec<CrateItemKey> = names.crates().iter().map(|c| c.key).collect();
        let payloads = payloads(&names);
        let frame = Frame::new(bstd::mem::Tag::Other).expect("a frame");
        let erased = blaze_own::Erased::new(&frame);
        let bump = erased.bump();
        let g = bump.alloc();
        let read: std::vec::Vec<_> = keys
            .iter()
            .zip(payloads.iter())
            .map(|(key, payload)| crate_records(scope_name(*key), payload, *key, bump).unwrap_or_else(|e| panic!("{e}")))
            .collect();
        let packages = vec!["/"; read.len()];
        let snapshot = kept(&names, texts);
        // The load and the log go in the frame, as the command places them (run_names.rs
        // `names_records_with_snapshot`): the reader takes both for the one lifetime it lives
        // for, which a stack value that is dropped cannot give.
        let load: &StoreLoad<'_> = frame.alloc(StoreLoad::from_parts(&read, &packages, &Files, Some(&snapshot), None, None, g));
        let crate_names: std::vec::Vec<_> = names.crates().iter().map(|c| c.name).collect();
        let log: &Log<'_> = frame.alloc(Log::new(&crate_names, g));
        let records = frame.alloc(Records::new(load.keys(&frame), load, log, &frame, g));
        let over_store = answers(&*records);
        let unserved = load.unserved().to_vec();
        (solved_answers, over_store, log.is_empty(), unserved)
    }

    /// Piece 16, B3: an item a macro call wrote (`lib`'s `made`, `Made`) is read through the
    /// command's loader from its crate's directory and the expansion record it names, its text
    /// equal to the solve's; the loader leaves no call unserved. PLANTED BUG: the same records
    /// with the expansion records' puts skipped: the crates still open (nothing re-expands), the
    /// macro's items' answers differ (no text) and each unserved call is `Missing`, naming the
    /// record's key, while every item no macro wrote answers as before.
    /// Prints `front load: N items equal over the store, M of them written by a macro call`.
    #[test]
    fn an_item_a_macro_call_wrote_is_read_through_the_store_load() {
        let (solved_answers, over_store, quiet, unserved) = both(true);
        let by_macro = solved_answers.iter().filter(|a| a.contains("pub fn made") || a.contains("pub struct Made")).count();
        assert!(by_macro >= 2, "the macro's two items are among the answers: {solved_answers:?}");
        assert!(quiet, "the reader logged over the records with texts");
        assert!(unserved.is_empty(), "a call's text was not served: {unserved:?}");
        assert_eq!(solved_answers, over_store, "an answer differs over the store");
        let (_, without, _, unserved) = both(false);
        let differ = solved_answers.iter().zip(without.iter()).filter(|(a, b)| a != b).count();
        // The two items the call wrote (the crate's root text also holds the macro's rules, so
        // `by_macro` counts more answers than the call's own).
        assert!(differ >= 2 && !unserved.is_empty(), "CONTROL: without the expansion records the macro's items still answered ({differ} differ, {unserved:?})");
        for one in unserved.iter() {
            assert!(
                matches!(&one.why, Some(ExpansionReadError::Record(ExpansionRecordsError::Missing { call, .. })) if *call == one.call),
                "an unserved call is the record the store lacks, naming the call: {one:?}"
            );
        }
        let same = solved_answers.iter().zip(without.iter()).filter(|(a, b)| a == b).count();
        assert_eq!(same + differ, solved_answers.len(), "every answer is compared");
        assert!(same >= solved_answers.len() - by_macro, "CONTROL: an item no macro wrote answered otherwise without the expansion records ({same} of {})", solved_answers.len());
        println!("front load: {} items equal over the store, {by_macro} of them written by a macro call", over_store.len());
    }

    #[test]
    fn sections_read_through_the_store_load_answer_as_the_solved_names() {
        let (solved_answers, over_store, quiet, unserved) = both(true);
        assert!(quiet && unserved.is_empty(), "a read failed over the committed records: {unserved:?}");
        assert!(solved_answers.len() >= 8, "the two crates hold their items: {solved_answers:?}");
        for (at, (solved, stored)) in solved_answers.iter().zip(over_store.iter()).enumerate() {
            assert_eq!(solved, stored, "answer {at} differs over the store");
        }
        assert_eq!(solved_answers.len(), over_store.len());
        println!("front load: {} items equal over the store", solved_answers.len());
    }

    /// PLANTED BUG: the `lib` crate's payload read under `app`'s name and key is refused at the
    /// read, naming both crates; no reader ever answers from it.
    #[test]
    fn a_section_read_under_another_crates_key_is_caught() {
        let names = solved();
        let keys: std::vec::Vec<CrateItemKey> = names.crates().iter().map(|c| c.key).collect();
        let payloads = payloads(&names);
        match crate_records(scope_name(keys[0]), &payloads[1], keys[0], blaze_own::testing::bump()) {
            Err(LoadError::OtherCrate { asked, found, which: "names", .. }) => assert_eq!((asked, found), (keys[0], keys[1])),
            other => panic!("lib's record under app's key was {other:?}"),
        }
    }

    /// THE TWO ROUTES OPEN ONE RECORD (engine_records.rs, rule 78), AND A COPY IS COUNTED: each
    /// crate's whole `CrateScope` record laid at a multiple of sixteen, as a segment lays it, and
    /// opened in place and by the copying route: the same sections' bytes; in place, each section
    /// borrowed from inside the record and 0 bytes counted copied; copying, from outside it, every
    /// section byte counted. The control for `engine_check_reads_records_in_place`'s 0. Prints
    /// `engine records: N crates opened in place (B bytes, 0 copied) and by copying (C copied)`.
    #[test]
    fn the_copying_route_counts_every_byte_it_copies() {
        use crate::engine_records::{Copying, InPlace, ReadCounts, open_record};
        use blaze_cas::plain::Aligned16;
        let names = solved();
        let keys: std::vec::Vec<CrateItemKey> = names.crates().iter().map(|c| c.key).collect();
        let payloads = payloads(&names);
        let g = Alloc::global();
        let (mut in_place, mut copying) = (ReadCounts::default(), ReadCounts::default());
        for (key, payload) in keys.iter().zip(payloads.iter()) {
            let name = scope_name(*key);
            let head = blaze_drive::record::write_plain_head(name, ContentId::of("blaze-cli engine records test", b"source"), ContentId::of("blaze-cli engine records test", b"out"), &[], payload.len(), g);
            let mut whole = std::vec::Vec::new();
            whole.extend_from_slice(&head);
            whole.extend_from_slice(payload);
            let laid: &'static [u8] = blaze_scope::resolve::aligned(&whole, blaze_own::testing::bump());
            let record = Aligned16::new(laid).unwrap_or_else(|e| panic!("the laid record: {e}"));
            // Both routes open the same directory without reading the records it names.
            let read = open_record::<InPlace>(name, record, *key, &mut in_place, g).unwrap_or_else(|e| panic!("in place: {e}"));
            let copied = open_record::<Copying>(name, record, *key, &mut copying, g).unwrap_or_else(|e| panic!("copying: {e}"));
            assert_eq!((read.names, read.items), (copied.names, copied.items), "the two routes read other bytes");
            assert_eq!(read.expansions.len(), copied.expansions.len(), "the two routes' directories hold other rows");
            let inside = laid.as_ptr_range();
            assert!(inside.contains(&read.names.as_ptr()) && inside.contains(&read.items.as_ptr()), "in place: a section not borrowed from its record");
            assert!(!inside.contains(&copied.names.as_ptr()) && !inside.contains(&copied.items.as_ptr()), "copying: a section borrowed, not copied");
        }
        let sections: usize = payloads.iter().map(|payload| {
            let parts = sections::sections(payload).unwrap_or_else(|e| panic!("{e}"));
            parts.names.len() + parts.items.len() + parts.expansions.len()
        }).sum();
        assert_eq!(in_place.records, keys.len() as u64);
        assert_eq!(in_place.copied, 0, "in place: a byte counted copied");
        assert_eq!(copying.copied, sections as u64, "CONTROL: the copying route's copy is counted, every section byte");
        println!("engine records: {} crates opened in place ({} bytes, 0 copied) and by copying ({} copied)", keys.len(), in_place.bytes, copying.copied);
    }

    /// A store without the record: "not in the store", naming the key; nothing is made here.
    #[test]
    fn a_record_the_store_lacks_is_not_in_the_store() {
        struct Empty;
        impl<'m> Store<'m> for Empty {
            fn get(&mut self, _cx: &Cx, _key: Key) -> Result<Option<crate::store::Held<'m>>, CliError> {
                Ok(None)
            }
            fn put(&mut self, _cx: &Cx, _key: Key, _bytes: &[u8]) -> Result<(), CliError> {
                Ok(())
            }
            fn map_all(&mut self, _cx: &Cx) -> Result<(), CliError> {
                Ok(())
            }
            fn after_answer(&mut self, _cx: &Cx, _profile: &mut blaze_drive::profile::Profile) -> Result<(), CliError> {
                Ok(())
            }
            fn public_crates(&mut self, _subjects: &[ContentId]) {}
            fn toolchain_crates(&mut self, _subjects: &[ContentId]) {}
            fn keep_with_toolchain(&mut self, _key: Key) -> Result<(), CliError> {
                Ok(())
            }
            fn index_beside(&mut self, _cx: &Cx, _owner: Key, _keys: &[Key]) -> Result<(), CliError> {
                Ok(())
            }
            fn put_parts(&mut self, _cx: &Cx, _key: Key, _parts: &[&[u8]]) -> Result<(), CliError> {
                Ok(())
            }
            fn put_record(&mut self, _cx: &Cx, _key: Key, _tag: blaze_keyed::kind::Tag, _owner: &[&[u8]], _bytes: &[u8]) -> Result<blaze_keyed::store::Put, CliError> {
                Ok(blaze_keyed::store::Put::Written)
            }
            fn kept(&self) -> crate::store::StoreKept {
                crate::store::StoreKept::default()
            }
        }
        let io = Io::simulated(SimWorld::new(3, Limits::USUAL));
        let cx = io.cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME).expect("a root context");
        let key = CrateItemKey(ContentId::of("blaze-cli front load test crate", b"absent"));
        let name = scope_name(key);
        assert_eq!(name.kind, Kind::CrateScope);
        let at = Key(ContentId::of("blaze-cli front load test key", b"absent"));
        let input = Input { name, at, out: ContentId::of("blaze-cli front load test out", b"absent") };
        let frame = Frame::new(bstd::mem::Tag::Other).expect("a frame");
        let erased = blaze_own::Erased::new(&frame);
        match read_crate(&cx, &mut Empty, &input, key, erased.bump()) {
            Err(error @ LoadError::NotInStore { .. }) => assert!(error.to_string().starts_with("not in the store: "), "{error}"),
            other => panic!("an empty store gave {other:?}"),
        }
    }

    /// The prelude reached through TWO crates' globs, as hello world reaches `Result`: the
    /// program's implicit prelude is `std::prelude::rust_2021` (the Rust Reference, "Preludes":
    /// the standard library prelude is `std::prelude::rust_20xx`, glob-imported into every
    /// module), which globs `core::prelude::rust_2021`, itself a glob of `core::prelude::v1`
    /// (the pinned library's own library/std/src/prelude/mod.rs and library/core/src/prelude/
    /// mod.rs). `Result` and `Option` resolve at the program's root through the command's
    /// loader as they do in the solved names.
    const PRE_CORE: &str = "#![no_core]\npub mod result { pub enum Result<T, E> { Ok(T), Err(E) } }\npub mod option { pub enum Option<T> { None, Some(T) } }\npub mod prelude {\n    pub mod v1 {\n        pub use crate::option::Option::{self, None, Some};\n        pub use crate::result::Result::{self, Err, Ok};\n    }\n    pub mod rust_2021 { pub use super::v1::*; }\n}\n";
    const PRE_STD: &str = "#![no_core]\npub mod prelude {\n    pub mod rust_2021 { pub use core::prelude::rust_2021::*; }\n}\n";
    const PRE_APP: &str = "pub fn head() -> Result<(), ()> { loop {} }\n";

    struct PreludeFiles;

    impl<'a> SourceTexts<'a> for PreludeFiles {
        fn read(&self, _krate: usize, path: &str) -> Option<&'a str> {
            match path {
                "/0/app/lib.rs" => Some(PRE_APP),
                "/1/std/lib.rs" => Some(PRE_STD),
                "/2/core/lib.rs" => Some(PRE_CORE),
                _ => None,
            }
        }
    }

    /// Prints `front load: Result and Option through two preludes' globs, solved and over the store`.
    #[test]
    fn a_prelude_through_two_crates_globs_resolves_over_the_store() {
        let inputs = [input("app", "/0/app/lib.rs", &[]), input("std", "/1/std/lib.rs", &[("core", 2)]), input("core", "/2/core/lib.rs", &[])];
        let sysroot = Sysroot { core: Some(blaze_scope::graph::CrateId(2)), alloc: None, std: Some(blaze_scope::graph::CrateId(1)), bstd: None };
        let b = blaze_own::testing::bump();
        let names = blaze_scope::resolve::resolve_crates(&inputs, sysroot, &PreludeFiles, &blaze_scope::task::Serial, b)
            .unwrap_or_else(|e| panic!("the three crates' names: {e:?}"));
        let names: &'static Names<'static> = std::boxed::Box::leak(std::boxed::Box::new(names));
        let keys: std::vec::Vec<CrateItemKey> = names.crates().iter().map(|c| c.key).collect();
        let payloads: &'static [blaze_own::Vec<u8, std::alloc::Global>] = std::vec::Vec::leak(payloads(names));
        let g = Alloc::global();
        // The readers in the command's order (body_types.rs `prepare`): the owner, then its
        // dependencies, then the sysroot's crates; here app, core, std, not the world's order.
        let order = [0usize, 2, 1];
        let read: std::vec::Vec<CrateRecords<'static>> = order
            .iter()
            .map(|at| crate_records(scope_name(keys[*at]), &payloads[*at], keys[*at], b).unwrap_or_else(|e| panic!("{e}")))
            .collect();
        let read: &'static [CrateRecords<'static>] = std::vec::Vec::leak(read);
        let packages: &'static [&'static str] = std::vec::Vec::leak(order.iter().map(|_| "/").collect());
        let load: &'static StoreLoad<'static> = std::boxed::Box::leak(std::boxed::Box::new(StoreLoad::new(read, packages, &PreludeFiles, g)));
        let crate_names: &'static [&'static str] = std::vec::Vec::leak(order.iter().map(|at| &*std::string::String::from(names.crates()[*at].name).leak()).collect());
        let log: &'static Log<'static> = std::boxed::Box::leak(std::boxed::Box::new(Log::new(crate_names, g)));
        let frame: &'static Frame = std::boxed::Box::leak(std::boxed::Box::new(Frame::new(bstd::mem::Tag::Other).expect("a frame")));
        let records: &'static Records<'static> = std::boxed::Box::leak(std::boxed::Box::new(Records::new(load.keys(frame), load, log, frame, g)));
        let app = names.crates().iter().position(|c| c.name == "app").expect("the app crate");
        let stored_app = order.iter().position(|at| *at == app).expect("app among the readers");
        for word in ["Result", "Option"] {
            let segments = [word];
            let written = blaze_scope::names::Written { kind: blaze_surface::PathKind::Plain, segments: &segments[..] };
            let solved_root = NamesSource::root_module(names, app).expect("the solved root");
            let solved = NamesSource::walk(names, solved_root, Namespace::Types, written);
            let stored_root = NamesSource::root_module(records, stored_app).expect("the stored root");
            let stored = NamesSource::walk(records, stored_root, Namespace::Types, written);
            assert!(solved.is_ok(), "`{word}` in the solved names: {solved:?}");
            assert_eq!(format!("{solved:?}"), format!("{stored:?}"), "`{word}` over the store differs from the solved names; the reader logged {:?}", &*log.errors());
        }
        println!("front load: Result and Option through two preludes' globs, solved and over the store");
    }
}
