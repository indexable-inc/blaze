//! The front end's [`Load`] over the store (end-state.md D157, STORE LOAD).
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
use std::sync::OnceLock;

use blaze_drive::{Input, Key, Name};
use blaze_core::SharedSet;
use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_ladder::front2::source::{Load, LoadedExpansion};
use blaze_own::{AVec, Alloc, Bump, SharedAlloc, Vec};
use blaze_scope::items_record::{CurrentFile, ItemsRecordError, VerifiedItems};
use blaze_scope::names_record::{NamesRecordError, Verified, VerifiedModules};
use blaze_scope::package_path::PackagePath;
use blaze_scope::resolve::SourceTexts;
use bstd::cx::Cx;
use bstd::mem::Frame;
use bstd::sync::CachePadded;

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
    /// The items section's one form and UTF-8 check this process (rule 74): every later job or
    /// stretch opens `ItemsRecord` from here in O(1), with no re-check of columns or strings.
    pub items_proof: VerifiedItems<'n>,
    /// The items section's edition, read by its one check at the load ([`checked_records`]):
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
    /// The exports section does not open as this build's `scope_record::VERSION` (catches stale
    /// `CrateScope` records whose expansion directory points to an older `RECORD_FORM`).
    Exports { name: Name, error: blaze_scope::crate_scope::RecordRead },
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
            LoadError::Exports { name, error } => write!(f, "the exports section of {name:?} does not open: {error:?} (rule: front_load.rs)"),
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
    let exports = blaze_scope::resolve::aligned(parts.exports, bump);
    let names = blaze_scope::resolve::aligned(parts.names, bump);
    let items = blaze_scope::resolve::aligned(parts.items, bump);
    let expansions = blaze_scope::resolve::aligned(parts.expansions, bump);
    checked_records(name, key, exports, names, items, expansions, bump.alloc())
}

/// A crate's sections, each 16-aligned where it lies (copied by [`crate_records`], or in
/// a segment's mapping, engine_records.rs), checked once as `key`'s (rule 74).
pub(crate) fn checked_records<'n>(
    name: Name,
    key: CrateItemKey,
    exports: &'n [u8],
    names: &'n [u8],
    items: &'n [u8],
    expansions: &'n [u8],
    alloc: Alloc<'n>,
) -> Result<CrateRecords<'n>, LoadError> {
    let exports_proof = blaze_scope::scope_record::Verified::check_header(exports).map_err(|error| LoadError::Exports { name, error })?;
    let expansions = ExpansionTexts::open(expansions).map_err(|error| LoadError::Expansions { name, error })?;
    let names_proof = Verified::check(names).map_err(|error| LoadError::Names { name, error })?;
    let found = names_proof.key();
    if found != key {
        return Err(LoadError::OtherCrate { name, asked: key, found, which: "names" });
    }
    // The section's one form + UTF-8 check this command (rule 74): its key checked, its proof
    // and edition kept beside it so every job and stretch opens `ItemsRecord` in O(1).
    let _ = alloc;
    let items_proof = VerifiedItems::check(items).map_err(|error| LoadError::Items { name, error })?;
    let (found, edition) = (items_proof.key(), items_proof.edition());
    if found != key {
        return Err(LoadError::OtherCrate { name, asked: key, found, which: "items" });
    }
    let exports_found = exports_proof.key();
    if exports_found != key {
        return Err(LoadError::OtherCrate { name, asked: key, found: exports_found, which: "exports" });
    }
    Ok(CrateRecords { key, file_slots: &[], names, names_proof, items, items_proof, edition, expansions })
}

/// O(1) lazy-validated variant of [`checked_records`] for in-place local CAS records
/// (`engine_records::InPlace`): verifies headers, alignment, all section bounds, column headers,
/// crate keys, edition, `CrateRoot`, `scope_record::VERSION` on `exports`, and expansion
/// directory upfront, deferring module scope `ScopeView::read`, variant UTF-8, and `ItemsRecord`
/// string UTF-8 checks until accessed.
pub(crate) fn checked_records_lazy<'n>(
    name: Name,
    key: CrateItemKey,
    exports: &'n [u8],
    names: &'n [u8],
    items: &'n [u8],
    expansions: &'n [u8],
) -> Result<CrateRecords<'n>, LoadError> {
    let exports_proof = blaze_scope::scope_record::Verified::check_header(exports).map_err(|error| LoadError::Exports { name, error })?;
    let expansions = ExpansionTexts::open(expansions).map_err(|error| LoadError::Expansions { name, error })?;
    let names_proof = Verified::check_header(names).map_err(|error| LoadError::Names { name, error })?;
    let found = names_proof.key();
    if found != key {
        return Err(LoadError::OtherCrate { name, asked: key, found, which: "names" });
    }
    let items_proof = VerifiedItems::check_header(items).map_err(|error| LoadError::Items { name, error })?;
    let (found, edition) = (items_proof.key(), items_proof.edition());
    if found != key {
        return Err(LoadError::OtherCrate { name, asked: key, found, which: "items" });
    }
    let exports_found = exports_proof.key();
    if exports_found != key {
        return Err(LoadError::OtherCrate { name, asked: key, found: exports_found, which: "exports" });
    }
    Ok(CrateRecords { key, file_slots: &[], names, names_proof, items, items_proof, edition, expansions })
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

/// Toolchain crate names that may declare `#[lang]` items or incoherent inherent impls
/// (`#![rustc_coherence_is_core]` / `#[rustc_allow_incoherent_impl]`), matching
/// `blaze-ladder/src/front2/source.rs` (`Records::name_lang_crates`), `personality.rs`, and
/// `engine_bodies.rs` (`is_lang_crate_name`).
pub(crate) fn is_lang_crate_name(name: &str) -> bool {
    matches!(name, "core" | "alloc" | "std" | "bstd" | "blaze_std" | "panic_unwind" | "panic_abort")
}

/// Selects the actual lang-item crates from `checked.lang_places()` for `run`/`test` programs:
/// always keeps the owner (place 0), every sysroot slot of `list`, and crates matching
/// [`is_lang_crate_name`]. Excludes pure-dependency toolchain crates (`libc`, `gimli`, `object`,
/// `addr2line`, `hashbrown`, `miniz_oxide`, `adler2`, `memchr`, `rustc_demangle`, `cfg_if`,
/// `unwind`, `std_detect`, `rustc_std_workspace_*`, `compiler_builtins`, `test`) so
/// `Records::name_lang_crates` and `Index::from_source` never eagerly open ~15 unused toolchain
/// `CrateScope` records (~29 MB) on every edit.
pub(crate) fn lang_places_of<A: core::alloc::Allocator>(
    checked: &blaze_world::check_scope::CheckScope<'_>,
    list: &blaze_world::crate_list::CrateList<'static>,
    into: &mut Vec<usize, A>,
) {
    use blaze_world::crate_list::CrateGraph;
    let sysroot = CrateGraph::sysroot(list);
    let readers = checked.readers();
    for place in checked.lang_places() {
        let keep = place == 0
            || readers.get(place).is_some_and(|&(world_at, _)| {
                sysroot.contains(&Some(world_at)) || is_lang_crate_name(CrateGraph::name(list, world_at))
            });
        if keep {
            into.push(place);
        }
    }
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
/// Content-addressed CAS key for a pack/dependency file's encoded `(surface, FileItems)` record.
/// Built purely from registered `blaze_drive::key` primitives (`FILE_SOURCE_DOMAIN`,
/// `CONTENT_DOMAIN`, `VALUE_DOMAIN`, `TASK_DOMAIN`) and covering all four `blaze-file-items`
/// outliner domains, the dedicated compile-time pack-`FileItems` producer closure
/// (`PACK_FILE_ITEMS_SOURCE`, covering the `blaze-file-items` + `blaze-drive` crate closures and
/// the `blaze-cli` pack-`FileItems` encode/decode/domain files `engine_bodies.rs`,
/// `front_load.rs`, and `sweep.rs`, while excluding unrelated codegen/CLI files so codegen edits
/// stay warm), the file edition, and the file text's `FILE_DOMAIN` digest (rule 21, rule 52,
/// rule 54).
pub(crate) fn pack_file_items_key(edition: blaze_surface::Edition, text_digest: ContentId) -> Key {
    pack_file_items_key_with_producer(edition, text_digest, crate::producers::PACK_FILE_ITEMS_SOURCE)
}

pub(crate) fn pack_file_items_key_with_producer(
    edition: blaze_surface::Edition,
    text_digest: ContentId,
    producer_digest: [u8; 32],
) -> Key {
    let source = blaze_drive::key::file_source(text_digest, edition.byte());
    let name = Name::new(blaze_drive::Kind::FileItems, text_digest);
    let base = blaze_drive::key::content_key(b"pack-file-items-v2");
    let reads = [
        (Name::new(blaze_drive::Kind::FileItems, blaze_drive::key::content_key(blaze_file_items::SURFACE_DOMAIN.as_bytes()).0), blaze_drive::key::ABSENT),
        (Name::new(blaze_drive::Kind::FileItems, blaze_drive::key::content_key(blaze_file_items::ITEM_DOMAIN.as_bytes()).0), blaze_drive::key::ABSENT),
        (Name::new(blaze_drive::Kind::FileItems, blaze_drive::key::content_key(blaze_file_items::SIGNATURE_DOMAIN.as_bytes()).0), blaze_drive::key::ABSENT),
        (Name::new(blaze_drive::Kind::FileItems, blaze_drive::key::content_key(blaze_file_items::NAMES_DOMAIN.as_bytes()).0), blaze_drive::key::ABSENT),
        (Name::new(blaze_drive::Kind::FileItems, ContentId(producer_digest)), blaze_drive::key::ABSENT),
    ];
    let tool = blaze_drive::Tool(blaze_drive::key::value_key(base, &reads).0);
    blaze_drive::key::task_key(tool, name, &source)
}

/// Encodes the 16-aligned `blaze_drive::record` header (`PAYLOAD_ALIGN = 16`) for a cached
/// pack/dependency `FileItems` CAS record, so callers can place `[head | file_items]` into one
/// 16-aligned buffer and committed CAS reads open `FileItemsView` zero-copy.
pub(crate) fn pack_file_items_head<'a>(
    key: Key,
    text_digest: ContentId,
    surface: ContentId,
    file_items_len: usize,
    head_alloc: Alloc<'a>,
) -> AVec<'a, u8> {
    let name = Name::new(blaze_drive::Kind::FileItems, key.0);
    blaze_drive::record::write_plain_head(name, text_digest, surface, &[], file_items_len, head_alloc)
}

/// Opens a persisted pack/dependency `FileItems` CAS record in place (zero-copy), validating the
/// drive envelope, matching `text_digest`, `expected_surface`, and `FileItemsView`
/// columns/alignment (rule 3, 21).
pub(crate) fn open_pack_file_items<'n>(
    bytes: &'n [u8],
    file: u32,
    text_digest: ContentId,
    expected_surface: ContentId,
) -> Result<(ContentId, blaze_surface::FileItemsView<'n>), ItemsRecordError> {
    let view = match blaze_drive::record::open(bytes) {
        Ok(blaze_drive::record::Stored::Whole(view)) => view,
        Ok(blaze_drive::record::Stored::Shape(_)) => {
            return Err(ItemsRecordError::FileItemsUnavailable {
                file,
                what: "cached pack FileItems record is a shape record without payload (front_load.rs)",
            });
        }
        Err(_) => {
            return Err(ItemsRecordError::FileItemsUnavailable {
                file,
                what: "cached pack FileItems record bytes are malformed (front_load.rs)",
            });
        }
    };
    if view.source != text_digest {
        return Err(ItemsRecordError::FileItemsUnavailable {
            file,
            what: "cached pack FileItems record source digest disagrees with file text (front_load.rs)",
        });
    }
    let fi = blaze_surface::FileItemsView::read(view.payload)
        .map_err(|why| ItemsRecordError::FileItemsRead { file, why })?;
    if fi.file() != text_digest {
        return Err(ItemsRecordError::FileItemsUnavailable {
            file,
            what: "cached pack FileItems view digest disagrees with file text (front_load.rs)",
        });
    }
    if view.out != expected_surface {
        return Err(ItemsRecordError::FileItemsSurfaceConflict {
            file,
            expected: expected_surface,
            found: view.out,
        });
    }
    Ok((view.out, fi))
}

/// One immutable pack file's cached `FileItems` entry: either a zero-copy CAS snapshot hit
/// (no heap buffer copied) or an in-memory 16-aligned buffer built on a CAS miss.
enum CachedPackFileItems {
    Snapshot {
        edition: u8,
        key: Key,
        text_digest: ContentId,
        surface: ContentId,
    },
    Owned {
        edition: u8,
        text_digest: ContentId,
        surface: ContentId,
        /// Backing allocation sized `len + 15` (never reallocated), storing the 16-aligned `FileItems` bytes at `offset`.
        buf: Vec<u8, SharedAlloc<'static>>,
        offset: usize,
        len: usize,
    },
}

impl CachedPackFileItems {
    fn edition(&self) -> u8 {
        match self {
            CachedPackFileItems::Snapshot { edition, .. } | CachedPackFileItems::Owned { edition, .. } => *edition,
        }
    }

    fn text_digest(&self) -> ContentId {
        match self {
            CachedPackFileItems::Snapshot { text_digest, .. } | CachedPackFileItems::Owned { text_digest, .. } => *text_digest,
        }
    }

    fn surface(&self) -> ContentId {
        match self {
            CachedPackFileItems::Snapshot { surface, .. } | CachedPackFileItems::Owned { surface, .. } => *surface,
        }
    }

    fn open<'s>(&'s self, snapshot: &'s StoreSnapshot, file: u32, text: &'s str, expected_surface: ContentId) -> Option<Result<CurrentFile<'s>, ItemsRecordError>> {
        match self {
            CachedPackFileItems::Snapshot { key, text_digest, .. } => {
                let bytes = match snapshot.get(*key) {
                    Ok(Some(bytes)) => bytes,
                    Ok(None) => {
                        return Some(Err(ItemsRecordError::FileItemsUnavailable {
                            file,
                            what: "cached pack FileItems record disappeared from committed snapshot (front_load.rs)",
                        }));
                    }
                    Err(_) => {
                        return Some(Err(ItemsRecordError::FileItemsUnavailable {
                            file,
                            what: "reading cached pack FileItems record from committed snapshot failed (front_load.rs)",
                        }));
                    }
                };
                match open_pack_file_items(bytes, file, *text_digest, expected_surface) {
                    Ok((surface, view)) => Some(Ok(CurrentFile { text, surface, view })),
                    Err(err) => Some(Err(err)),
                }
            }
            CachedPackFileItems::Owned { surface, buf, offset, len, .. } => {
                if *surface != expected_surface {
                    return Some(Err(ItemsRecordError::FileItemsSurfaceConflict {
                        file,
                        expected: expected_surface,
                        found: *surface,
                    }));
                }
                let end = offset.checked_add(*len)?;
                let bytes = buf.get(*offset..end)?;
                let view = match blaze_surface::FileItemsView::read(bytes) {
                    Ok(v) => v,
                    Err(why) => return Some(Err(ItemsRecordError::FileItemsRead { file, why })),
                };
                Some(Ok(CurrentFile { text, surface: *surface, view }))
            }
        }
    }
}

struct OpenedCrate<'r> {
    records: CrateRecords<'r>,
    /// Shared lock-free per-module verification bitmap for lazy `names_proof`, populated once per
    /// accessed module across all workers of the request.
    module_cache: VerifiedModules<'r, SharedAlloc<'static>>,
}

pub(crate) struct ScopeRecords<'r> {
    snapshot: &'r StoreSnapshot,
    heap: &'static blaze_own::Heap,
    crates: usize,
    /// Allocated lazily on first `records()` ask so unused `ScopeRecords` (e.g. `groups` setup
    /// when no dependency crate is opened) perform zero heap allocations.
    slots: OnceLock<blaze_own::Vec<OnceLock<Result<OpenedCrate<'r>, &'static str>>, blaze_own::SharedAlloc<'static>>>,
    /// Shared per-`(world_at, file)` `FileItems` outline cache across all workers and epochs,
    /// covering both packed dependency/toolchain crates and checkout crates when planned file
    /// outlines are absent (e.g. `blaze build` / `blaze test` codegen workers). Allocated lazily.
    file_caches: OnceLock<blaze_own::Vec<OnceLock<blaze_own::Vec<OnceLock<CachedPackFileItems>, blaze_own::SharedAlloc<'static>>>, blaze_own::SharedAlloc<'static>>>,
    /// Hot counters isolated onto separate 128-byte cache lines ([`CachePadded`], rule 86) so
    /// 128 workers opening scope crates never false-share `asks`/`opened`/`bytes` with `slots`.
    asks: CachePadded<core::sync::atomic::AtomicU64>,
    opened: CachePadded<core::sync::atomic::AtomicU64>,
    bytes: CachePadded<core::sync::atomic::AtomicU64>,
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
        ScopeRecords {
            snapshot,
            heap,
            crates,
            slots: OnceLock::new(),
            file_caches: OnceLock::new(),
            asks: CachePadded::new(core::sync::atomic::AtomicU64::new(0)),
            opened: CachePadded::new(core::sync::atomic::AtomicU64::new(0)),
            bytes: CachePadded::new(core::sync::atomic::AtomicU64::new(0)),
        }
    }

    /// Build or fetch cached `CurrentFile` for file `(world_at, file)` (sized by `nfiles`).
    /// Checks the committed CAS snapshot first (zero-copy `FileItemsView` without re-outlining)
    /// before outlining once in a task frame and sharing across all workers and epochs.
    /// Validates the cached entry's `surface` against `expected_surface` (failing loudly with
    /// [`ItemsRecordError::FileItemsSurfaceConflict`] on a conflicting CAS/cached entry instead
    /// of silently re-outlining) and, when `verify_digest` is `true` (checkout `/0/` files),
    /// validates `text_digest` against `text` so a concurrent edit never reuses a stale outline.
    /// On a cold CAS miss whose fresh outline disagrees with `expected_surface` (stale
    /// `ItemsRecord`), returns the already-built uncached [`CurrentFile`] in `bump` without
    /// populating the shared cell or retrying a second outline.
    #[expect(clippy::too_many_arguments, reason = "pack file items lookup: crate slot, file, expected surface, edition, text, verify flag, bump")]
    pub(crate) fn cached_pack_file_items<'s>(
        &'s self,
        world_at: usize,
        nfiles: usize,
        file: u32,
        expected_surface: ContentId,
        edition: blaze_surface::Edition,
        text: &'s str,
        verify_digest: bool,
        bump: Bump<'s>,
    ) -> Option<Result<CurrentFile<'s>, ItemsRecordError>> {
        let caches = self
            .file_caches
            .get_or_init(|| blaze_own::Vec::from_iter_in((0..self.crates).map(|_| OnceLock::new()), self.heap.shared()));
        let crate_files = caches
            .get(world_at)?
            .get_or_init(|| blaze_own::Vec::from_iter_in((0..nfiles).map(|_| OnceLock::new()), self.heap.shared()));
        let cell = crate_files.get(usize::try_from(file).ok()?)?;
        if let Some(entry) = cell.get() {
            if entry.edition() != edition.byte() {
                return None;
            }
            if verify_digest && entry.text_digest() != ContentId::of(crate::sweep::FILE_DOMAIN, text.as_bytes()) {
                return None;
            }
            if entry.surface() != expected_surface {
                return Some(Err(ItemsRecordError::FileItemsSurfaceConflict {
                    file,
                    expected: expected_surface,
                    found: entry.surface(),
                }));
            }
            return entry.open(self.snapshot, file, text, expected_surface);
        }
        let digest = ContentId::of(crate::sweep::FILE_DOMAIN, text.as_bytes());
        let key = pack_file_items_key(edition, digest);
        match self.snapshot.get(key) {
            Ok(Some(bytes)) => {
                let (surface, view) = match open_pack_file_items(bytes, file, digest, expected_surface) {
                    Ok(opened) => opened,
                    Err(err) => return Some(Err(err)),
                };
                let _ = cell.set(CachedPackFileItems::Snapshot { edition: edition.byte(), key, text_digest: digest, surface });
                return Some(Ok(CurrentFile { text, surface, view }));
            }
            Ok(None) => {}
            Err(_) => {
                return Some(Err(ItemsRecordError::FileItemsUnavailable {
                    file,
                    what: "reading cached pack FileItems record from committed snapshot failed (front_load.rs)",
                }));
            }
        }
        enum Built<'s> {
            Cached {
                surface: ContentId,
                buf: Vec<u8, blaze_own::SharedAlloc<'static>>,
                offset: usize,
                len: usize,
            },
            Uncached(CurrentFile<'s>),
        }
        let built = match blaze_mem::frame::task(blaze_mem::Tag::Parse, |scratch| {
            let erased = blaze_own::Erased::new(scratch);
            let alloc = erased.handle();
            let outline = blaze_file_items::outline(text, edition, alloc);
            let encoded = match outline.to_file_items(text, digest, alloc) {
                Ok(e) => e,
                Err(why) => return Err(ItemsRecordError::FileItemsBuild { file, why }),
            };
            let surface = outline.surface();
            if surface != expected_surface {
                // Fresh outline disagrees with `ItemsRecord`: do not poison the shared cell and do
                // not return `None` (which would re-outline the file a second time via
                // `CurrentFile::from_text`); return the already-built uncached view directly so
                // `ItemsRecord::check_file` reports `ItemsRecordError::Stale` in one pass.
                let view = match blaze_surface::FileItemsView::read(blaze_scope::resolve::aligned(&encoded, bump)) {
                    Ok(v) => v,
                    Err(why) => return Err(ItemsRecordError::FileItemsRead { file, why }),
                };
                return Ok(Built::Uncached(CurrentFile { text, surface, view }));
            }
            let len = encoded.len();
            let mut buf = Vec::with_capacity_in(len.saturating_add(15), self.heap.shared());
            buf.resize(len.saturating_add(15), 0);
            let offset = (16 - (buf.as_ptr().addr() & 15)) & 15;
            let Some(dst) = offset.checked_add(len).and_then(|end| buf.get_mut(offset..end)) else {
                return Err(ItemsRecordError::FileItemsUnavailable {
                    file,
                    what: "cached pack FileItems buffer bounds overflowed (front_load.rs)",
                });
            };
            dst.copy_from_slice(&encoded);
            Ok(Built::Cached { surface, buf, offset, len })
        }) {
            Ok(v) => v,
            Err(why) => return Some(Err(why)),
        };
        let (surface, buf, offset, len) = match built {
            Built::Uncached(current) => return Some(Ok(current)),
            Built::Cached { surface, buf, offset, len } => (surface, buf, offset, len),
        };
        let Some(check_slice) = offset.checked_add(len).and_then(|end| buf.get(offset..end)) else {
            return Some(Err(ItemsRecordError::FileItemsUnavailable {
                file,
                what: "cached pack FileItems slice bounds invalid (front_load.rs)",
            }));
        };
        if let Err(why) = blaze_surface::FileItemsView::read(check_slice) {
            return Some(Err(ItemsRecordError::FileItemsRead { file, why }));
        }
        let _ = cell.set(CachedPackFileItems::Owned { edition: edition.byte(), text_digest: digest, surface, buf, offset, len });
        let entry = cell.get()?;
        if entry.edition() != edition.byte() {
            return None;
        }
        if verify_digest && entry.text_digest() != digest {
            return None;
        }
        entry.open(self.snapshot, file, text, expected_surface)
    }

    /// What it did so far.
    pub(crate) fn counts(&self) -> ByNeedCounts {
        use core::sync::atomic::Ordering::Relaxed;
        ByNeedCounts { asks: self.asks.load(Relaxed), opened: self.opened.load(Relaxed), bytes: self.bytes.load(Relaxed) }
    }

    /// Crate `one`'s records: opened at the first ask, the same after; `Err`: the words of why
    /// its record did not open, the same for every ask.
    fn records<'s>(&'s self, one: &Unopened) -> Result<CrateRecords<'s>, &'static str> {
        use core::sync::atomic::Ordering::Relaxed;
        self.asks.fetch_add(1, Relaxed);
        let slots = self
            .slots
            .get_or_init(|| blaze_own::Vec::from_iter_in((0..self.crates).map(|_| OnceLock::new()), self.heap.shared()));
        let Some(slot) = slots.get(one.world_at) else {
            return Err("its place is past the request's crate list (front_load.rs `ScopeRecords`)");
        };
        let opened = slot.get_or_init(|| {
            let mut counts = crate::engine_records::ReadCounts::default();
            let read = self.read(one, &mut counts);
            self.opened.fetch_add(counts.records, Relaxed);
            self.bytes.fetch_add(counts.bytes, Relaxed);
            read.map(|records| {
                let module_cache = records.names_proof.new_module_cache(self.heap.shared());
                OpenedCrate { records, module_cache }
            })
            .map_err(|why| crate::engine_head_notes::leak(self.heap.alloc(), format_args!("{why}")))
        });
        opened
            .as_ref()
            .map(|o| CrateRecords { names_proof: o.records.names_proof.bind_cache(&o.module_cache), ..o.records })
            .map_err(|&e| e)
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

    /// Cached `FileItems` view keyed by `(world_at, file)` (sized by `nfiles`), validated
    /// against `expected_surface`. When `verify_digest` is `true`, also validates the cached
    /// entry's `text_digest` against `text`. Returns `None` if no matching cache slot is available.
    #[expect(clippy::too_many_arguments, reason = "pack file items lookup: crate slot, file, expected surface, edition, text, verify flag, bump")]
    fn pack_file_items<'s>(
        &'s self,
        _world_at: usize,
        _nfiles: usize,
        _file: u32,
        _expected_surface: ContentId,
        _edition: blaze_surface::Edition,
        _text: &'s str,
        _verify_digest: bool,
        _bump: Bump<'s>,
    ) -> Option<Result<CurrentFile<'s>, ItemsRecordError>> {
        None
    }
}

impl OpenByNeed for ScopeRecords<'_> {
    fn open<'s>(&'s self, one: &Unopened) -> Result<CrateRecords<'s>, &'static str> {
        self.records(one)
    }

    fn pack_file_items<'s>(
        &'s self,
        world_at: usize,
        nfiles: usize,
        file: u32,
        expected_surface: ContentId,
        edition: blaze_surface::Edition,
        text: &'s str,
        verify_digest: bool,
        bump: Bump<'s>,
    ) -> Option<Result<CurrentFile<'s>, ItemsRecordError>> {
        self.cached_pack_file_items(world_at, nfiles, file, expected_surface, edition, text, verify_digest, bump)
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
    ByNeed { own: CrateRecords<'n>, own_world_at: usize, rest: &'n [Unopened], records: &'n dyn OpenByNeed },
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
            CratesFrom::ByNeed { own, rest, records, .. } => match k {
                0 => Some(Ok(*own)),
                k => rest.get(k - 1).map(|one| records.open(one)),
            },
        }
    }
}

/// Optional lookup of precomputed checkout file digests (`crate::sweep::FILE_DOMAIN`),
/// implemented by [`crate::job_texts::JobTexts`] when backed by [`crate::body_types::SyncLazyTrees`].
pub(crate) trait DigestLookup {
    fn checkout_digest(&self, inner: &str) -> Option<ContentId>;
}

impl DigestLookup for crate::job_texts::JobTexts<'_> {
    fn checkout_digest(&self, inner: &str) -> Option<ContentId> {
        crate::job_texts::JobTexts::checkout_digest(self, inner)
    }
}

/// Source texts accepted by [`StoreLoad::from_crates`]: either a plain `&dyn SourceTexts` or a
/// [`crate::job_texts::JobTexts`] that also exposes cached `FILE_DOMAIN` checkout digests.
pub(crate) trait StoreTexts<'n> {
    fn split(self) -> (&'n dyn SourceTexts<'n>, Option<&'n dyn DigestLookup>);
}

impl<'a: 'n, 'n> StoreTexts<'n> for &'a (dyn SourceTexts<'n> + 'n) {
    fn split(self) -> (&'n dyn SourceTexts<'n>, Option<&'n dyn DigestLookup>) {
        (self, None)
    }
}

impl<'a: 'n, 'n> StoreTexts<'n> for &'a crate::job_texts::JobTexts<'n> {
    fn split(self) -> (&'n dyn SourceTexts<'n>, Option<&'n dyn DigestLookup>) {
        (self, Some(self))
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
    digests: Option<&'n dyn DigestLookup>,
    snapshot: Option<&'n StoreSnapshot>,
    expansion_keys: Option<&'n SharedSet<Key, blaze_own::SharedAlloc<'static>>>,
    outlines: Option<&'n [crate::engine_tasks::Outlined<'n>]>,
    /// Each (crate, call) whose expansion was asked and none was served, with why when its
    /// directory names one ([`ExpansionRecordsError::Missing`]: the store lacks its record; a
    /// damaged row); `None`: the crate made no text for the call.
    unserved: RefCell<AVec<'n, Unserved>>,
    alloc: Alloc<'n>,
    bump: Bump<'n>,
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
    pub(crate) fn new(crates: &'n [CrateRecords<'n>], packages: &'n [&'n str], texts: &'n dyn SourceTexts<'n>, bump: Bump<'n>, alloc: Alloc<'n>) -> StoreLoad<'n> {
        Self::from_parts(crates, packages, texts, None, None, None, bump, alloc)
    }

    /// Both (the merge of two lanes, 2026-11-18): the snapshot for lazy expansion texts and
    /// the outlines for current file ranges. The engine's Bodies path passes both.
    #[expect(clippy::too_many_arguments, reason = "test and driver loader inputs, each one handle")]
    pub(crate) fn from_parts(
        crates: &'n [CrateRecords<'n>],
        packages: &'n [&'n str],
        texts: &'n dyn SourceTexts<'n>,
        snapshot: Option<&'n StoreSnapshot>,
        expansion_keys: Option<&'n SharedSet<Key, blaze_own::SharedAlloc<'static>>>,
        outlines: Option<&'n [crate::engine_tasks::Outlined<'n>]>,
        bump: Bump<'n>,
        alloc: Alloc<'n>,
    ) -> Self {
        Self::from_crates(CratesFrom::Opened(crates), packages, texts, snapshot, expansion_keys, outlines, bump, alloc)
    }

    /// [`StoreLoad::from_parts`] over either source of crates ([`CratesFrom`]).
    #[expect(clippy::too_many_arguments, reason = "loader inputs, each one handle")]
    pub(crate) fn from_crates(
        crates: CratesFrom<'n>,
        packages: &'n [&'n str],
        texts: impl StoreTexts<'n>,
        snapshot: Option<&'n StoreSnapshot>,
        expansion_keys: Option<&'n SharedSet<Key, blaze_own::SharedAlloc<'static>>>,
        outlines: Option<&'n [crate::engine_tasks::Outlined<'n>]>,
        bump: Bump<'n>,
        alloc: Alloc<'n>,
    ) -> Self {
        let (texts, digests) = texts.split();
        let crates = match crates {
            CratesFrom::ByNeed { mut own, own_world_at, rest, records } => {
                if own.names_proof.needs_module_cache() {
                    own.names_proof = own.names_proof.bind_bump_cache(bump);
                }
                CratesFrom::ByNeed { own, own_world_at, rest, records }
            }
            CratesFrom::Opened(list) if list.iter().any(|one| one.names_proof.needs_module_cache()) => {
                let mut bound = Vec::with_capacity_in(list.len(), bump);
                for &one in list.iter() {
                    bound.push(CrateRecords { names_proof: one.names_proof.bind_bump_cache(bump), ..one });
                }
                CratesFrom::Opened(bound.into_bump_slice())
            }
            CratesFrom::Opened(list) => CratesFrom::Opened(list),
        };
        StoreLoad { crates, refused: core::cell::Cell::new(None), packages, texts, digests, snapshot, expansion_keys, outlines, unserved: RefCell::new(AVec::new_in(alloc)), alloc, bump }
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
    #[cfg_attr(not(test), expect(dead_code, reason = "inspected by front_load tests; engine paths use `unserved_len` / `with_first_unserved_error_since`"))]
    pub(crate) fn unserved(&self) -> core::cell::Ref<'_, AVec<'n, Unserved>> {
        self.unserved.borrow()
    }

    /// Number of unserved expansion lookups recorded so far on this loader.
    pub(crate) fn unserved_len(&self) -> usize {
        self.unserved.borrow().len()
    }

    /// Passes the first [`ExpansionReadError`] recorded at or after index `since` to `f`
    /// without cloning or allocating, returning `None` if no expansion read errored since `since`.
    pub(crate) fn with_first_unserved_error_since<R>(&self, since: usize, f: impl FnOnce(&ExpansionReadError) -> R) -> Option<R> {
        let list = self.unserved.borrow();
        let err = list.get(since..)?.iter().find_map(|entry| entry.why.as_ref())?;
        Some(f(err))
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
        let record = ExpansionRecord::open_in(bytes, self.bump).map_err(ExpansionReadError::Record)?;
        let text = entry.text(&record).map_err(ExpansionReadError::Record)?;
        Ok(Some(LoadedExpansion { text, origins: Some((entry.call_part, record.column)) }))
    }
}

impl<'n> Load<'n> for StoreLoad<'n> {
    fn names(&self, krate: usize) -> Option<&'n [u8]> {
        self.records_of(krate).map(|c| c.names)
    }

    fn names_verified(&self, krate: usize) -> Option<Verified<'n>> {
        self.records_of(krate).map(|c| c.names_proof)
    }

    fn items(&self, krate: usize) -> Option<&'n [u8]> {
        self.records_of(krate).map(|c| c.items)
    }

    fn items_verified(&self, krate: usize) -> Option<VerifiedItems<'n>> {
        self.records_of(krate).map(|c| c.items_proof)
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
            if let CratesFrom::ByNeed { own, own_world_at, rest, records } = self.crates {
                let target = if krate == 0 {
                    Some((own_world_at, own.items_proof.files()))
                } else {
                    krate.checked_sub(1).and_then(|i| rest.get(i)).and_then(|one| {
                        let nfiles = self.crates.records(krate)?.ok()?.items_proof.files();
                        Some((one.world_at, nfiles))
                    })
                };
                if let Some((world_at, nfiles)) = target
                    && let Some(res) = records.pack_file_items(world_at, nfiles, index, surface, edition, text, full.starts_with("/0/"), bump)
                {
                    return res.map(Some);
                }
            }
            return CurrentFile::from_text(index, text, edition, bump).map(Some);
        };
        // A physical x/mod.rs can be a literal file or the module slot x.rs. Ignore DATA_FILE
        // outlines (e.g. `include_str!("../src/foo/mod.rs")`) when looking up a Rust module's
        // outline, and prefer the slot matching the crate's edition.
        let locate = |slot: Name| outlines.binary_search_by_key(&slot, |one| one.name).ok()
            .and_then(|i| outlines.get(i))
            .filter(|one| one.path.as_slice() == real.as_bytes() && !crate::data_include::is_data(one.edition));
        let mod_slot = || {
            let stem = real.strip_suffix("/mod.rs")?;
            let mut slot = AVec::from_slice_in(stem.as_bytes(), self.alloc);
            slot.extend_from_slice(b".rs");
            locate(Name::file(&slot))
        };
        let literal = locate(Name::file(real.as_bytes()));
        let one = literal.filter(|o| o.edition == edition.byte())
            .or_else(|| mod_slot().filter(|o| o.edition == edition.byte()))
            .or(literal)
            .or_else(mod_slot)
            .ok_or(ItemsRecordError::FileItemsUnavailable { file: index, what: "the checkout path has no planned file outline" })?;
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
        let digest = self.digests.and_then(|d| d.checkout_digest(real)).unwrap_or_else(|| ContentId::of(crate::sweep::FILE_DOMAIN, text.as_bytes()));
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
        let sysroot = Sysroot::default();
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
            let e = blaze_scope::crate_scope::error_record(c.key, c.name, c.edition, g).unwrap_or_else(|e| panic!("exports record of `{}`: {e:?}", c.name));
            let n = blaze_scope::names_record::encode(c, g).unwrap_or_else(|e| panic!("names record of `{}`: {e:?}", c.name));
            let i = blaze_scope::items_record::encode(c, g).unwrap_or_else(|e| panic!("items record of `{}`: {e:?}", c.name));
            let split = blaze_scope::expansion_records::write(c, g).unwrap_or_else(|e| panic!("expansions of `{}`: {e}", c.name));
            out.push(sections::write(0, [&e[..], &[], &n[..], &i[..], &[], &split.directory[..], &[], &[]]));
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
        let load: &StoreLoad<'_> = frame.alloc(StoreLoad::from_parts(&read, &packages, &Files, Some(&snapshot), None, None, bump, g));
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
        let sysroot = Sysroot { core: Some(blaze_scope::graph::CrateId(2)), std: Some(blaze_scope::graph::CrateId(1)), ..Sysroot::default() };
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
        let load: &'static StoreLoad<'static> = std::boxed::Box::leak(std::boxed::Box::new(StoreLoad::new(read, packages, &PreludeFiles, b, g)));
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

    #[test]
    fn pack_file_items_cas_round_trips_zero_copy_and_checks_every_input() {
        use crate::store::{CommandStore, Facts, Home};
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();
        let shared = heap.shared();
        let text = "pub fn add(a: u64, b: u64) -> u64 { a.wrapping_add(b) }\npub struct Pair(pub u64, pub u64);\n";
        let other_text = "pub fn add(a: u64, b: u64) -> u64 { a.wrapping_sub(b) }\n";
        let digest = ContentId::of(crate::sweep::FILE_DOMAIN, text.as_bytes());
        let other_digest = ContentId::of(crate::sweep::FILE_DOMAIN, other_text.as_bytes());
        let k2021 = pack_file_items_key(Edition::E2021, digest);
        let k2024 = pack_file_items_key(Edition::E2024, digest);
        let k_other = pack_file_items_key(Edition::E2021, other_digest);
        let k_old_producer = pack_file_items_key_with_producer(Edition::E2021, digest, [0x11; 32]);
        let k_new_producer = pack_file_items_key_with_producer(Edition::E2021, digest, [0x22; 32]);
        assert_ne!(k2021, k2024, "pack FileItems CAS key must distinguish editions (rule 21)");
        assert_ne!(k2021, k_other, "pack FileItems CAS key must distinguish file digests (rule 21)");
        assert_ne!(
            k_old_producer, k_new_producer,
            "pack FileItems CAS key must distinguish compiler-crate producer digests (rule 21, rule 54)"
        );
        assert_eq!(
            k2021,
            pack_file_items_key_with_producer(Edition::E2021, digest, crate::producers::PACK_FILE_ITEMS_SOURCE),
            "pack_file_items_key must bind PACK_FILE_ITEMS_SOURCE"
        );

        let outline = blaze_file_items::outline(text, Edition::E2021, alloc);
        let raw = outline.to_file_items(text, digest, alloc).expect("to_file_items");
        let surface = outline.surface();
        let encode = |k| {
            let head = pack_file_items_head(k, digest, surface, raw.len(), alloc);
            assert_eq!(head.len() % blaze_drive::record::PAYLOAD_ALIGN, 0, "record head must be 16-aligned");
            let mut buf = Vec::with_capacity_in(head.len() + raw.len(), shared);
            buf.extend_from_slice(&head);
            buf.extend_from_slice(&raw);
            buf
        };
        let encoded_a = encode(k2021);
        let encoded_b = encode(k2021);
        assert_eq!(encoded_a.as_slice(), encoded_b.as_slice(), "encoding must be deterministic (rule 25)");

        let io = Io::simulated(SimWorld::new(71, Limits::USUAL));
        let cx = io.cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME).expect("root context");
        let err = io.need_err().expect("stderr");
        let home = Home::of(&cx, &io.need_fs().expect("files"), Some("/h")).expect("test home");
        let lent = home.open_store(&blaze_cas::plain::Settings::default(), Facts { boot: Some(1), nonce: 71 }, alloc).expect("store");
        let mut store = CommandStore::new(&cx, &lent, false, 0, None, io.need_clock().expect("clock"), &err, alloc).expect("command store");
        store.put(&cx, k2021, &encoded_a).expect("put cached pack FileItems");
        let snapshot = store.commit_for_body_reads().expect("committed snapshot");
        let stored_bytes = snapshot.get(k2021).expect("snapshot get").expect("present in snapshot");

        let (got_surface, view) = open_pack_file_items(stored_bytes, 0, digest, surface).expect("zero-copy open");
        assert_eq!(got_surface, surface);
        assert_eq!(view.file(), digest);
        assert_eq!(view.len(), outline.items().len());

        // Stale / mismatched digest, conflicting surface, or corrupt record must be refused (rule 3, rule 21).
        assert!(open_pack_file_items(stored_bytes, 0, other_digest, surface).is_err(), "mismatched digest must be refused");
        assert!(open_pack_file_items(&stored_bytes[..8], 0, digest, surface).is_err(), "truncated bytes must be refused");
        let wrong_surface = ContentId([0xEE; 32]);
        match open_pack_file_items(stored_bytes, 0, digest, wrong_surface) {
            Err(ItemsRecordError::FileItemsSurfaceConflict { file: 0, expected, found }) => {
                assert_eq!((expected, found), (wrong_surface, surface));
            }
            Ok(_) => panic!("conflicting surface must return typed FileItemsSurfaceConflict"),
            Err(other) => panic!("expected FileItemsSurfaceConflict, got {other:?}"),
        }

        // Lazy `ScopeRecords::slots` stays unallocated until first crate ask.
        let scope_records = ScopeRecords::new(&snapshot, 16, heap);
        assert!(scope_records.slots.get().is_none(), "unused ScopeRecords must not allocate slots");
    }

    /// Regression test for the #2861 (`lowered_one_by_one` on `Trait`/`Impl`) stale pack
    /// `FileItems` CAS bug (`ItemsRecordError::Stale` -> `HeadError::NoText` on `alloc::string::String`
    /// and `core::clone::Clone`):
    /// - Positive: a stale record with an old `surface` digest stored under a previous producer's
    ///   key is ignored by `ScopeRecords::cached_pack_file_items`, which misses the stale entry,
    ///   re-outlines the file fresh, and returns the current `surface`.
    /// - Negative (Rule 23 causal control): planting the same stale `surface` record under the
    ///   CURRENT key returns the stale `surface` (mismatching the fresh outline's `surface`),
    ///   reproducing the exact `ItemsRecordError::Stale` mismatch that caused `HeadError::NoText`.
    #[test]
    fn stale_pack_file_items_under_prior_producer_key_never_poisons_current_surface() {
        use crate::store::{CommandStore, Facts, Home};

        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();
        let shared = heap.shared();
        // Mirrors `library/alloc/src/string.rs` and `library/core/src/clone.rs`: a type/trait
        // whose `impl` block has an associated method carrying `#[cfg_attr(...)]`.
        let text = "#![no_core]\npub struct String;\nimpl String {\n    #[cfg_attr(debug_assertions, track_caller)]\n    pub fn new() -> Self { String }\n}\n";
        let digest = ContentId::of(crate::sweep::FILE_DOMAIN, text.as_bytes());
        let outline = blaze_file_items::outline(text, Edition::E2024, alloc);
        let raw = outline.to_file_items(text, digest, alloc).expect("to_file_items");
        let fresh_surface = outline.surface();
        let stale_surface = ContentId([0xaa; 32]);
        assert_ne!(stale_surface, fresh_surface, "control stale surface must differ from current surface");

        let old_producer = [0x01; 32];
        assert_ne!(old_producer, crate::producers::PACK_FILE_ITEMS_SOURCE);
        let stale_key = pack_file_items_key_with_producer(Edition::E2024, digest, old_producer);
        let current_key = pack_file_items_key(Edition::E2024, digest);
        assert_ne!(stale_key, current_key, "prior producer key must not collide with current key");

        let encode_with_surface = |key: Key, surface: ContentId| {
            let head = pack_file_items_head(key, digest, surface, raw.len(), alloc);
            let mut buf = Vec::with_capacity_in(head.len() + raw.len(), shared);
            buf.extend_from_slice(&head);
            buf.extend_from_slice(&raw);
            buf
        };

        // Positive case: stale surface record sits in CAS under the old producer's key.
        // `ScopeRecords::cached_pack_file_items` must miss it and compute `fresh_surface`.
        let io = Io::simulated(SimWorld::new(72, Limits::USUAL));
        let cx = io.cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME).expect("root context");
        let err = io.need_err().expect("stderr");
        let home = Home::of(&cx, &io.need_fs().expect("files"), Some("/h-pos")).expect("test home");
        let lent = home.open_store(&blaze_cas::plain::Settings::default(), Facts { boot: Some(1), nonce: 72 }, alloc).expect("store");
        let mut store = CommandStore::new(&cx, &lent, false, 0, None, io.need_clock().expect("clock"), &err, alloc).expect("command store");
        let stale_old_bytes = encode_with_surface(stale_key, stale_surface);
        store.put(&cx, stale_key, &stale_old_bytes).expect("put stale record under old producer key");
        let snap_pos = store.commit_for_body_reads().expect("commit positive snapshot");
        let scope_pos = ScopeRecords::new(&snap_pos, 4, heap);
        let bump_pos = blaze_own::testing::bump();
        let opened_pos = scope_pos
            .cached_pack_file_items(1, 2, 0, fresh_surface, Edition::E2024, text, false, bump_pos)
            .expect("cached_pack_file_items must return Some")
            .expect("positive path must succeed");
        assert_eq!(
            opened_pos.surface, fresh_surface,
            "stale pack FileItems under prior producer key must not poison current surface"
        );

        // Negative causal control (Rule 23): planting a conflicting surface under `current_key`
        // must fail loudly with typed `FileItemsSurfaceConflict` instead of returning a stale
        // surface or silently re-outlining on every command.
        let io_neg = Io::simulated(SimWorld::new(73, Limits::USUAL));
        let cx_neg = io_neg.cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME).expect("root context");
        let err_neg = io_neg.need_err().expect("stderr");
        let home_neg = Home::of(&cx_neg, &io_neg.need_fs().expect("files"), Some("/h-neg")).expect("test home");
        let lent_neg = home_neg.open_store(&blaze_cas::plain::Settings::default(), Facts { boot: Some(1), nonce: 73 }, alloc).expect("store");
        let mut store_neg = CommandStore::new(&cx_neg, &lent_neg, false, 0, None, io_neg.need_clock().expect("clock"), &err_neg, alloc).expect("command store");
        let stale_cur_bytes = encode_with_surface(current_key, stale_surface);
        store_neg.put(&cx_neg, current_key, &stale_cur_bytes).expect("put stale record under current key");
        let snap_neg = store_neg.commit_for_body_reads().expect("commit negative snapshot");
        let scope_neg = ScopeRecords::new(&snap_neg, 4, heap);
        let bump_neg = blaze_own::testing::bump();
        match scope_neg
            .cached_pack_file_items(1, 2, 0, fresh_surface, Edition::E2024, text, false, bump_neg)
            .expect("cached_pack_file_items must return Some")
        {
            Err(ItemsRecordError::FileItemsSurfaceConflict { file: 0, expected, found }) => {
                assert_eq!((expected, found), (fresh_surface, stale_surface));
            }
            Ok(_) => panic!("conflicting CAS surface under current_key must return typed error"),
            Err(other) => panic!("expected FileItemsSurfaceConflict, got {other:?}"),
        }
    }

    #[test]
    fn lang_crate_filter_keeps_sysroot_and_skips_pure_toolchain_deps() {
        for keep in ["core", "alloc", "std", "bstd", "blaze_std", "panic_unwind", "panic_abort"] {
            assert!(is_lang_crate_name(keep), "`{keep}` must be kept as a lang/incoherent crate");
        }
        for skip in [
            "libc",
            "gimli",
            "object",
            "addr2line",
            "hashbrown",
            "miniz_oxide",
            "adler2",
            "memchr",
            "rustc_demangle",
            "cfg_if",
            "unwind",
            "std_detect",
            "rustc_std_workspace_core",
            "rustc_std_workspace_alloc",
            "rustc_std_workspace_std",
            "compiler_builtins",
            "test",
            "proc_macro",
        ] {
            assert!(!is_lang_crate_name(skip), "pure toolchain dep `{skip}` must be skipped");
        }
    }

    #[test]
    fn crate_records_and_lazy_reject_stale_exports_version_and_accept_current() {
        use crate::engine_records::{InPlace, ReadCounts, ReadError, open_record};
        use blaze_cas::plain::Aligned16;
        use blaze_scope::crate_scope::RecordRead;

        let names = solved();
        let c = &names.crates()[0];
        let g = blaze_own::testing::heap().alloc();
        let bump = blaze_own::testing::bump();
        let mut exports = blaze_scope::crate_scope::error_record(c.key, c.name, c.edition, g).expect("exports");
        let names_sec = blaze_scope::names_record::encode(c, g).expect("names");
        let items_sec = blaze_scope::items_record::encode(c, g).expect("items");
        let split = blaze_scope::expansion_records::write(c, g).expect("expansions");

        let wrap_record = |payload: &[u8]| -> &'static [u8] {
            let name = scope_name(c.key);
            let head = blaze_drive::record::write_plain_head(
                name,
                ContentId::of("blaze-cli front load exports version test", b"source"),
                ContentId::of("blaze-cli front load exports version test", b"out"),
                &[],
                payload.len(),
                g,
            );
            let mut whole = Vec::with_capacity_in(head.len() + payload.len(), g);
            whole.extend_from_slice(&head);
            whole.extend_from_slice(payload);
            blaze_scope::resolve::aligned(&whole, bump)
        };

        // Positive control: current `scope_record::VERSION` opens cleanly on both eager and lazy paths.
        let current_payload = sections::write(0, [&exports[..], &[], &names_sec[..], &items_sec[..], &[], &split.directory[..], &[], &[]]);
        assert!(crate_records(scope_name(c.key), &current_payload, c.key, bump).is_ok(), "current exports VERSION must open");
        let mut counts = ReadCounts::default();
        let current_aligned = Aligned16::new(wrap_record(&current_payload)).expect("aligned");
        assert!(open_record::<InPlace>(scope_name(c.key), current_aligned, c.key, &mut counts, g).is_ok(), "lazy InPlace must open current exports VERSION");

        // Negative control: downgrade `scope_record::VERSION` from 10 -> 9 (pre-`083ffadb3` stale CrateScope).
        let current_ver = u32::from_le_bytes([exports[4], exports[5], exports[6], exports[7]]);
        let stale_ver = current_ver - 1;
        exports[4..8].copy_from_slice(&stale_ver.to_le_bytes());
        let stale_payload = sections::write(0, [&exports[..], &[], &names_sec[..], &items_sec[..], &[], &split.directory[..], &[], &[]]);
        match crate_records(scope_name(c.key), &stale_payload, c.key, bump) {
            Err(LoadError::Exports { error: RecordRead::NotThisRecord, .. }) => {}
            other => panic!("stale exports VERSION {stale_ver} must be rejected as LoadError::Exports(NotThisRecord), got {other:?}"),
        }
        let stale_aligned = Aligned16::new(wrap_record(&stale_payload)).expect("aligned");
        match open_record::<InPlace>(scope_name(c.key), stale_aligned, c.key, &mut counts, g) {
            Err(ReadError::Load(LoadError::Exports { error: RecordRead::NotThisRecord, .. })) => {}
            other => panic!("lazy InPlace must reject stale exports VERSION {stale_ver}, got {other:?}"),
        }
    }

    #[test]
    fn cached_pack_file_items_validates_surface_and_rejects_conflicting_cas_entry() {
        use crate::store::{CommandStore, Facts, Home};
        let heap = blaze_own::testing::heap();
        let alloc = heap.alloc();
        let text = "pub fn surface_guard(x: u32) -> u32 { x + 7 }\n";
        let ed = Edition::E2021;
        let digest = ContentId::of(crate::sweep::FILE_DOMAIN, text.as_bytes());
        let cas_key = pack_file_items_key(ed, digest);
        let outline = blaze_file_items::outline(text, ed, alloc);
        let real_surface = outline.surface();
        let stale_surface = ContentId([0x5C; 32]);
        assert_ne!(real_surface, stale_surface);
        let raw = outline.to_file_items(text, digest, alloc).expect("to_file_items");

        let make_record = |surface: ContentId| {
            let head = pack_file_items_head(cas_key, digest, surface, raw.len(), alloc);
            let mut buf = Vec::with_capacity_in(head.len() + raw.len(), alloc);
            buf.extend_from_slice(&head);
            buf.extend_from_slice(&raw);
            buf
        };

        let io = Io::simulated(SimWorld::new(79, Limits::USUAL));
        let cx = io.cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME).expect("root context");
        let err = io.need_err().expect("stderr");
        let home = Home::of(&cx, &io.need_fs().expect("files"), Some("/h")).expect("test home");
        let lent = home.open_store(&blaze_cas::plain::Settings::default(), Facts { boot: Some(1), nonce: 79 }, alloc).expect("store");
        let mut store = CommandStore::new(&cx, &lent, false, 0, None, io.need_clock().expect("clock"), &err, alloc).expect("command store");

        // Negative control: CAS holds a conflicting `FileItems` record with `stale_surface != real_surface`
        // under the current `pack_file_items_key`: must fail loudly with typed `FileItemsSurfaceConflict`
        // instead of silently re-outlining.
        store.put(&cx, cas_key, &make_record(stale_surface)).expect("put stale record");
        let stale_snap = store.commit_for_body_reads().expect("stale snapshot");
        let bump = blaze_own::testing::bump();
        let scope_records = ScopeRecords::new(&stale_snap, 4, heap);
        match scope_records
            .cached_pack_file_items(0, 2, 0, real_surface, ed, text, false, bump)
            .expect("must return Some(Err)")
        {
            Err(ItemsRecordError::FileItemsSurfaceConflict { file: 0, expected, found }) => {
                assert_eq!((expected, found), (real_surface, stale_surface));
            }
            Ok(_) => panic!("conflicting CAS surface must fail loudly"),
            Err(other) => panic!("expected FileItemsSurfaceConflict, got {other:?}"),
        }

        // Cold CAS miss where `ItemsRecord` expects a stale surface (`stale_surface != real_surface`):
        // must return `Some(Ok(CurrentFile { surface: real_surface, .. }))` directly (never `None`,
        // avoiding a second outline via `CurrentFile::from_text`), while leaving the shared cell
        // empty so a matching caller still populates the cell cleanly.
        let home_cold = Home::of(&cx, &io.need_fs().expect("files"), Some("/h-cold")).expect("test home cold");
        let lent_cold = home_cold.open_store(&blaze_cas::plain::Settings::default(), Facts { boot: Some(1), nonce: 81 }, alloc).expect("store cold");
        let mut store_cold = CommandStore::new(&cx, &lent_cold, false, 0, None, io.need_clock().expect("clock"), &err, alloc).expect("command store cold");
        let empty_snap = store_cold.commit_for_body_reads().expect("empty snapshot");
        let cold_records = ScopeRecords::new(&empty_snap, 4, heap);
        let uncached_mismatch = cold_records
            .cached_pack_file_items(0, 2, 0, stale_surface, ed, text, false, bump)
            .expect("cold mismatched surface must return Some(Ok(CurrentFile)) directly without falling back to None")
            .expect("valid uncached CurrentFile");
        assert_eq!(uncached_mismatch.surface, real_surface);
        assert_eq!(uncached_mismatch.view.len(), outline.items().len());
        let cold_cell_empty = cold_records
            .file_caches
            .get()
            .and_then(|crates| crates.get(0))
            .and_then(|files| files.get())
            .and_then(|files| files.get(0))
            .is_some_and(|cell| cell.get().is_none());
        assert!(cold_cell_empty, "cold mismatched outline must not populate the shared cell");
        let cold_match = cold_records
            .cached_pack_file_items(0, 2, 0, real_surface, ed, text, false, bump)
            .expect("matching cold outline must succeed")
            .expect("valid CurrentFile");
        assert_eq!(cold_match.surface, real_surface);

        // Positive control: a fresh store holds a matching `real_surface` record and serves it directly.
        let home2 = Home::of(&cx, &io.need_fs().expect("files"), Some("/h2")).expect("test home 2");
        let lent2 = home2.open_store(&blaze_cas::plain::Settings::default(), Facts { boot: Some(1), nonce: 80 }, alloc).expect("store 2");
        let mut store2 = CommandStore::new(&cx, &lent2, false, 0, None, io.need_clock().expect("clock"), &err, alloc).expect("command store 2");
        store2.put(&cx, cas_key, &make_record(real_surface)).expect("put fresh record");
        let fresh_snap = store2.commit_for_body_reads().expect("fresh snapshot");
        let fresh_records = ScopeRecords::new(&fresh_snap, 4, heap);
        let hit = fresh_records
            .cached_pack_file_items(0, 2, 0, real_surface, ed, text, false, bump)
            .expect("matching CAS record")
            .expect("valid CurrentFile");
        assert_eq!(hit.surface, real_surface);
        assert_eq!(hit.view.len(), outline.items().len());

        // Negative control (in-memory cell): querying the populated slot with a mismatched surface
        // must fail loudly with typed `FileItemsSurfaceConflict`.
        match fresh_records
            .cached_pack_file_items(0, 2, 0, stale_surface, ed, text, false, bump)
            .expect("must return Some(Err)")
        {
            Err(ItemsRecordError::FileItemsSurfaceConflict { file: 0, expected, found }) => {
                assert_eq!((expected, found), (stale_surface, real_surface));
            }
            Ok(_) => panic!("mismatched in-memory surface must fail loudly"),
            Err(other) => panic!("expected FileItemsSurfaceConflict, got {other:?}"),
        }
    }
}
