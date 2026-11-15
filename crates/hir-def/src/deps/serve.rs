//! Giving a database the images a store has ([`serve`]) and writing those it lacks
//! ([`store`]).
//!
//! The store is the driver's ([`ImageStore`]): the local store, or the local store over the
//! global cache ([`ReadThrough`]), each checking a value's content hash on every read and
//! giving it in place, so the front end depends on neither.
//!
//! A crate's image is under its key ([`keys`]) when it has no [`Reads`]; else under its key,
//! the names it read (a slot's bytes) and their values ([`filed`]).

use std::{ops::Range, sync::Arc};

use base_db::{Crate, CrateKey, Db, Env, Nonce};
use blaze_hash::{HashMap, HashSet};
use blaze_ids::{ContentId, Hasher};
use hir_expand::{InFile, MacroCallId};
use std::alloc::Global;
use syntax::ast::HasName;

use crate::{
    Lookup,
    deps::{
        Image, filed,
        image::{ImageBuf, ImageRef, Part, PartBytes, Portable, write_image},
        key::{Base, keys},
        reads::Reads,
        signatures,
    },
    nameres::{
        crate_def_map,
        diagnostics::DefDiagnosticKind,
        frozen::{
            self, Frozen, Mode,
            pages::{self, PageSource, Paged},
        },
    },
};

/// The domain of the key a crate's def map pages are kept under ([`pages_key`]).
pub const PAGES_DOMAIN: &str = "blaze v2 def map pages";

/// Where the def map pages of the crate whose image is filed under `filed` are: the image's
/// key (which covers every input of the crate and what it read, [`Image::key`]), and the
/// layout and encoding of the record, so pages of another layout are another key and are
/// never asked for.
pub fn pages_key(filed: ContentId) -> ContentId {
    let mut hasher = Hasher::new(PAGES_DOMAIN);
    hasher
        .id(&filed)
        .update(&pages::LAYOUT.to_le_bytes())
        .part(frozen::ENCODING.as_bytes());
    hasher.finish()
}

/// Every key a crate's front-end records are kept under that is made from the key its image
/// is filed under (`filed`, [`Image::key`]): the image itself, then each record kind of this
/// crate keyed over it. The one list of them: what pins a crate's front end against the
/// store's collector and what a pack of a crate carries is taken from here, so a record kind
/// added here is pinned and exported, and one written by [`store`] and left out is found by
/// `tests::every_key_store_writes_is_a_front_key_or_a_filing_key` (a record written and not
/// pinned is collected). A lane that adds a record kind keyed over `filed` in this crate adds
/// its key here; one in a crate above this one adds it where that crate's keys are joined to
/// these (blaze-cargo's `serve::collect::front_keys`).
pub fn front_keys(filed: ContentId) -> impl Iterator<Item = ContentId> {
    [filed, pages_key(filed), kept_key(filed)].into_iter()
}

/// The domain of the key a crate's kept roots are kept under ([`kept_key`]).
pub const KEPT_DOMAIN: &str = "blaze v2 kept roots 1";

/// Where the kept roots of the crate whose image is filed under `filed` are: what the crate
/// keeps by name whatever reaches it (`#[no_mangle]`, `#[used]`, linked by name), written by
/// hir-ty-codegen's `kept_record` beside the crate's impl tables. As [`pages_key`], with the
/// encoding its ids are written in; the record's own layout is in [`KEPT_DOMAIN`].
pub fn kept_key(filed: ContentId) -> ContentId {
    let mut hasher = Hasher::new(KEPT_DOMAIN);
    hasher.id(&filed).part(frozen::ENCODING.as_bytes());
    hasher.finish()
}

/// Every key the read-filing records of the crate whose own key is `key` may be under: the
/// slots a crate with reads is found through ([`super::slot`]), each that may be filled. A
/// slot that holds nothing names no record.
pub fn filing_keys(key: ContentId) -> impl Iterator<Item = ContentId> {
    (0..super::SLOTS).map(move |n| super::slot(key, n))
}

/// Where images are kept, and the threads that read and write them: what the driver adapts the
/// store or the global cache to. Every method is called from those threads.
pub trait ImageStore: Sync {
    /// The value kept under `key` (an image or a crate's reads), its bytes checked against the
    /// content hash it was put with and held where the store keeps them; `Err` when the store
    /// could not be read.
    fn get(&self, key: &ContentId) -> Result<Option<ImageBuf>, String>;

    /// Keeps `value` under `key`. A second put of a key with other bytes is an error: a key
    /// names one value.
    fn put(&self, key: ContentId, value: Vec<u8>) -> Result<(), String>;

    /// The hash of the values that the names `shape` an expansion server encoded (an
    /// `ExpansionReads::Traced` shape) have now, as the server hashed them when the expansion
    /// ran: the daemon's `proc_macro_api::kept::Keeping::reads_now`. A variable is the calling crate's (`env`, what the
    /// expansion was given) before the process's. `Err` when one of them can't be read.
    fn reads_now(&self, env: &Env, shape: &[u8]) -> Result<ContentId, String>;

    /// The record kept under `key`, read a part at a time: the driver's maps it once
    /// (`blaze_cas::direct`), so a part is a slice of the kernel's page cache and only the
    /// pages a read touches are ever loaded. A store with no way to read a part (a test's, a
    /// value just fetched from the global cache) says so with [`whole_pages`].
    ///
    /// No default, nor for [`ImageStore::for_each`]: each had one that gave the same answer
    /// the slow way (the whole value read, the tasks run on one thread), so a store that can
    /// do better and forgot to say kept working, slower, and nothing said so. Each store says
    /// how it reads a part and how it runs a batch.
    // dyn: the driver's holder of one record, once per crate per session
    fn pages(&self, key: &ContentId) -> Result<Option<Arc<dyn PageSource>>, String>;

    /// Runs `task(at)` for each `at` in `0..count`, on every core the driver has: a crate's
    /// image is checked and encoded by one task, so [`serve`] and [`store`] take the larger of
    /// the work over the cores and the largest crate's. Running them in order on one thread
    /// gives the same result, which a test's store says it does.
    // dyn: one call per batch of crates, its task called once per crate
    fn for_each(&self, count: usize, task: &(dyn Fn(usize) + Sync));
}

/// [`ImageStore::pages`] of a store with no way to read a part of a record: `found`, the value
/// [`ImageStore::get`] read, held whole. The holder is in `alloc`, the caller's: a store's
/// `pages` hands `Global`, since the holder is shared with the driver past any frame.
// dyn: the holder of one record, once per crate per session
pub fn whole_pages<A: std::alloc::Allocator + Clone>(
    found: Option<ImageBuf>,
    alloc: A,
) -> Option<Arc<dyn PageSource, A>> {
    // dyn: as above
    found.map(|bytes| Arc::new_in(pages::Whole(bytes), alloc) as Arc<dyn PageSource, A>)
}

/// The local store over the global cache: a value the local store lacks is asked of the
/// global cache, and kept in the local store once found, so the next session reads it in place
/// from the local store. What is written is written to the local store only: the global cache
/// is written by blaze's builders, which publish what passes its rule for public keys.
pub struct ReadThrough<'s> {
    // dyn: the driver's stores, one call per crate per session
    pub local: &'s dyn ImageStore,
    // dyn: as above
    pub global: &'s dyn ImageStore,
}

impl ImageStore for ReadThrough<'_> {
    fn get(&self, key: &ContentId) -> Result<Option<ImageBuf>, String> {
        if let Some(found) = self.local.get(key)? {
            return Ok(Some(found));
        }
        let Some(found) = self.global.get(key)? else { return Ok(None) };
        // alloc: a value fetched from the global cache, copied once into the local store
        self.local.put(*key, found.bytes().to_vec())?;
        Ok(Some(found))
    }

    fn put(&self, key: ContentId, value: Vec<u8>) -> Result<(), String> {
        self.local.put(key, value)
    }

    fn reads_now(&self, env: &Env, shape: &[u8]) -> Result<ContentId, String> {
        self.local.reads_now(env, shape)
    }

    // dyn: as `ImageStore::pages`
    fn pages(&self, key: &ContentId) -> Result<Option<Arc<dyn PageSource>>, String> {
        if let Some(found) = self.local.pages(key)? {
            return Ok(Some(found));
        }
        // From the global cache, kept in the local store by `get`: held whole this session
        // (the local store's put is written after the build), mapped from the local store
        // from the next.
        Ok(whole_pages(self.get(key)?, std::alloc::Global))
    }

    // dyn: as `ImageStore::for_each`
    fn for_each(&self, count: usize, task: &(dyn Fn(usize) + Sync)) {
        self.local.for_each(count, task)
    }
}

/// What [`serve`] did.
///
/// The invariant: nothing in it is tied to a snapshot. Every crate in it is named by its
/// [`CrateKey`], which the world gives once and never reuses in a session, never by a
/// [`Crate`] id (an id dies at a commit that drops its arena, and a serve ends in one), so it
/// is valid across any number of commits: a driver keeps it across its serves' and builds'
/// commits and passes it to [`store`] later. `nonce` says which database it is of.
///
/// What makes it stale: a crate whose key no longer names a crate of the database's world
/// (the crate was removed by a later write), or a database other than `nonce`'s. [`store`]
/// is then the typed error [`StoreError::Gone`] naming the key, or
/// [`StoreError::OtherDatabase`]: never a panic, never the crate skipped. Its fields are
/// data; reading them asks nothing of a database.
#[derive(Debug)]
pub struct Served {
    /// The database the serve read ([`Db::nonce`]): the one whose world its keys name.
    pub nonce: Nonce,
    /// The key of each crate read from an image or that can be: the [`eligible`] crates of
    /// those asked, and their dependencies.
    ///
    /// [`eligible`]: super::eligible
    pub keys: HashMap<CrateKey, ContentId, Global>,
    /// The crates read from an image, in the order of their keys.
    pub hits: Vec<CrateKey>,
    /// The crates with reads kept under their key but no image for the values those reads
    /// have now: something they read changed, so they are resolved and imaged again.
    pub stale: Vec<CrateKey>,
    /// The blaze the keys were made by: [`store`] makes each key again with it, to check the
    /// database did not move a crate's key between the serve and the store.
    pub build: ContentId,
    /// The crates whose image under their key is of the layout keys of domain 7 filed, its
    /// def map a part of it (`ImageRef::is_layout_7`): each the named miss `records of
    /// <crate>, def map, layout 7: made again from source`, resolved and imaged again.
    pub old_layout: Vec<CrateKey>,
    /// What this serve's searches of the crates' read-filing slots came to, as `blaze_record`
    /// counted them (`filed::search`): one lookup a crate fetched that has no image under
    /// its key alone, a hit where a slot's reads have now the values an image is filed by,
    /// the slots read, and a read valued for each slot whose set of reads was worked out
    /// again. A crate found under its key alone is not among them.
    pub lookups: blaze_record::Counts,
    /// The crates a [`store`] of this serve wrote: a later store of it (the [`Which::All`] a
    /// front end makes before it is let go, after its build's end made the [`Which::Sealed`]
    /// one) writes none of them again, each written under the key this serve gave it.
    pub stored: StoredOf,
    /// The crates that held an image in this snapshot and found none under their key now:
    /// the driver gives them none in the next ([`serve`]).
    pub withdrawn: Vec<CrateKey, Global>,
}

/// The crates the stores of one [`Served`] wrote, behind a lock: [`store`] has the serve by
/// `&`, as a front end kept for later holds it. By [`CrateKey`], as [`Served`] is.
#[derive(Debug)]
pub struct StoredOf(pub(super) blaze_core::lock::Lock<HashSet<CrateKey, Global>>);

impl Default for StoredOf {
    fn default() -> StoredOf {
        StoredOf(blaze_core::lock::Lock::new("deps.served.stored", HashSet::new_in(Global)))
    }
}

/// Why [`store`] wrote no image of a crate. Each is an input the key and the reads do not
/// cover, so the image would be served where the crate resolves otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// Its def map's collection ran a proc-macro call whose output may depend on its session's
    /// state (`StateUse::Session`: its crate's state is not known, or does not settle, or the
    /// call was not shown to give the same from either state). An image is not refused for
    /// what such a call read (each expansion states that: `ExpansionReads`), but because an
    /// image's expansions are not run, so the session's state is not built for the calls of
    /// the crate a later query expands (bodies), which would then run from another state than
    /// rustc's. A stateless crate's calls, and a settled crate's shown ones, refuse nothing.
    SessionMacros,
    /// A procedural macro's expansion of it whose reads are not known, and why.
    ProcMacro { call: String, why: &'static str },
    /// A module, an `include!`, `include_str!` or `include_bytes!` of it reads a file outside
    /// the crate's source root (a `#[path]` or a path into another package, a file no source
    /// root has): the key, made before the crate is resolved, covers only the source root's
    /// files ([`refusal`], `super::late::walk`). A build script's `OUT_DIR` files are in it.
    /// `what` names the module or the macro, where it is, and the file.
    OutsideSourceRoot { what: String },
    /// A `mod foo;` of it names a file that is not there (rustc's E0583): the key covers the
    /// files that are, so it would not move when that file appears ([`refusal`]). `what` names
    /// the module, where it is declared and the files looked for.
    ModuleFileMissing { what: String },
    /// The crate's key as the database has it when its image would be written is not the one
    /// it was served under: something it is made from changed between the serve and the
    /// store, so the image would be filed under a key that does not name it (rule 21).
    KeyMoved { served: ContentId, now: Option<ContentId> },
    /// A part of it holds a path its key writes as a marker ([`Base`]): the image would not be
    /// the same in another checkout with the same key.
    MachinePath { part: Part, path: String },
    /// Its reads can't be kept.
    Reads { why: &'static str },
    /// An encoding refused a part of it.
    Encode { part: Part, why: String },
    /// Its image depends on an input that a `Surface` record's key (its unit's own text) does
    /// not cover and that no fact of a surface's shape can ask ([`image_parts`]): what a
    /// procedural macro's expansion traced reading, or a file outside the VFS. `what` names
    /// the call or the file and the crate.
    Unasked { what: String },
    /// One late read (an `env!`'s variable, an `include!`'s file) seen with two values in one
    /// walk of the crate's calls (`super::late::walk`): an input moved while the crate was
    /// resolved. `what` names the crate, the read and both values.
    LateTwice { what: String },
}

impl Refused {
    /// The refusal's kind, one word a variant: what a count of refusals groups by.
    pub fn kind(&self) -> &'static str {
        match self {
            Refused::SessionMacros => "session macros",
            Refused::ProcMacro { .. } => "proc macro reads unknown",
            Refused::OutsideSourceRoot { .. } => "outside source root",
            Refused::ModuleFileMissing { .. } => "module file missing",
            Refused::KeyMoved { .. } => "key moved",
            Refused::MachinePath { .. } => "machine path",
            Refused::Reads { .. } => "reads",
            Refused::Encode { .. } => "encode",
            Refused::Unasked { .. } => "unasked",
            Refused::LateTwice { .. } => "late read twice",
        }
    }
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::SessionMacros => f.write_str("its def map uses the session's macros"),
            Refused::ProcMacro { call, why } => {
                write!(f, "what procedural macro {call} read is not known: {why}")
            }
            Refused::OutsideSourceRoot { what } => {
                write!(f, "{what}, outside the crate's source root, which its key covers")
            }
            Refused::ModuleFileMissing { what } => {
                write!(f, "{what}, a file its key would not move for once it appears")
            }
            Refused::KeyMoved { served, now } => match now {
                Some(now) => write!(
                    f,
                    "its key moved from {served:?}, which it was served under, to {now:?} \
                     before its image was written"
                ),
                None => write!(
                    f,
                    "it had key {served:?} when served and is not eligible for an image now"
                ),
            },
            Refused::MachinePath { part, path } => {
                write!(f, "its {} hold the path {path}, which its key does not", part.name())
            }
            Refused::Reads { why } => write!(f, "its reads can't be kept: {why}"),
            Refused::Encode { part, why } => write!(f, "its {} do not encode: {why}", part.name()),
            Refused::Unasked { what } => write!(
                f,
                "{what}: an input of its image that a surface's key (its unit's own text) does \
                 not cover and no fact of its shape can ask (rule keys-cover-every-input)"
            ),
            Refused::LateTwice { what } => write!(
                f,
                "{what}: one late read with two values while the crate was resolved, an input \
                 that moved (rule keys-cover-every-input)"
            ),
        }
    }
}

/// What ends the image of a crate at `why`: `Some(why)`, which the caller returns. Under a
/// [`census`] (this crate's tests alone) `why` is recorded and `None` comes back, so the walk
/// goes on to the next refusal: a module it would refuse is read as it was resolved, an
/// include as no late read. Every refusal of [`image_parts`] and [`store`] goes through here.
pub(super) fn ends(why: Refused) -> Option<Refused> {
    #[cfg(test)]
    let why = census::record(why)?;
    Some(why)
}

/// [`ends`] as a walk's step: `Err(why)` ends it, `Ok(())` goes on (under a [`census`]).
pub(super) fn end(why: Refused) -> Result<(), Refused> {
    match ends(why) {
        Some(why) => Err(why),
        None => Ok(()),
    }
}

/// Whether a [`census`] is taken on this thread.
#[cfg(test)]
pub(super) fn census_taken() -> bool {
    census::taken()
}

/// Whether a census is taken on this thread: never, outside this crate's tests.
#[cfg(not(test))]
pub(super) fn census_taken() -> bool {
    false
}

/// The census of refusals (rule 82): one run of [`image_parts`] over a crate that lists every
/// refusal in it, where a build stops at the first and a release build finds one a run.
///
/// A census, never a way to build: it exists in this crate's test build alone (`cfg(test)`,
/// so no other crate and no build of blaze can turn it on), and what a run under it makes is
/// not an image to keep: a refused module is read as it was resolved and a refused include is
/// left out of the late reads, so the parts it makes are of a crate no key covers.
#[cfg(test)]
pub(crate) mod census {
    use std::{alloc::Global, cell::RefCell};

    use super::Refused;

    std::thread_local! {
        /// The refusals of the census this thread takes, in the order they were met; `None`
        /// when it takes none.
        static TAKEN: RefCell<Option<Vec<Refused, Global>>> = const { RefCell::new(None) };
    }

    /// Runs `run` with the census taken on this thread: what it returns, and every refusal it
    /// met. A census in a census is refused (a panic: a test's mistake).
    pub(crate) fn take<T>(run: impl FnOnce() -> T) -> (T, Vec<Refused, Global>) {
        TAKEN.with(|it| {
            let mut taken = it.borrow_mut();
            assert!(taken.is_none(), "a census taken inside another");
            *taken = Some(Vec::new_in(Global));
        });
        // Ends the census however `run` ends, so a panic in one crate's run leaves the next
        // test's thread with none taken.
        struct Ends;
        impl Drop for Ends {
            fn drop(&mut self) {
                TAKEN.with(|it| _ = it.borrow_mut().take());
            }
        }
        let ends = Ends;
        let out = run();
        let met = TAKEN.with(|it| it.borrow_mut().take()).expect("the census `take` began is still taken");
        drop(ends);
        (out, met)
    }

    /// Records `why` in the census taken on this thread (`None` back), or gives it back where
    /// none is taken.
    pub(in crate::deps) fn record(why: Refused) -> Option<Refused> {
        TAKEN.with(|it| match it.borrow_mut().as_mut() {
            Some(met) => {
                met.push(why);
                None
            }
            None => Some(why),
        })
    }

    pub(in crate::deps) fn taken() -> bool {
        TAKEN.with(|it| it.borrow().is_some())
    }
}

/// Which crates [`store`] writes of those [`serve`] found no image of.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Which {
    /// Only the [`sealed`] ones: what a build's end writes. A sealed crate's resolution does
    /// not move with an edit of the workspace, so it is written once; a member's would be paid
    /// by the next edit and thrown away by it.
    ///
    /// [`sealed`]: super::sealed
    Sealed,
    /// Every one, members and the crates above them too: what a front end writes before it is
    /// let go for idleness and at a stop, so the next process reads them from their pages.
    All,
}

/// The most crates of each kind a [`serve`]'s event names.
const NAMED: usize = 12;

/// What [`store`] did.
///
/// The invariant: as [`Served`], nothing in it is tied to a snapshot: each crate is named by
/// its [`CrateKey`], so it is valid across any number of commits. What makes it stale is the
/// same: a key that names no crate of the world any more, which a caller turning one into a
/// [`Crate`] meets as `None` from `Db::crate_at`, to answer with its own typed error. Its
/// fields are data; it has no method that asks a database.
#[derive(Debug, Default)]
pub struct Stored {
    /// The crates whose image was written.
    pub written: Vec<CrateKey>,
    /// The crates whose image was not, and why.
    pub refused: Vec<(CrateKey, Refused)>,
    /// The key each crate of `written` has its image filed under ([`Image::key`]): what a
    /// value made from the crate as this database resolved it is filed by
    /// (`blaze_diagnostics::cache`).
    pub filed: HashMap<CrateKey, ContentId, Global>,
}

/// Why [`store`] wrote nothing: an error of the store's caller or of the store, never of one
/// crate's image (that is [`Refused`], in [`Stored::refused`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// A crate the [`Served`] names by `key` is no crate of the database's world: a write
    /// after the serve removed it. The serve is stale for it (rule keys-cover-every-input:
    /// its key named a crate that is gone), so nothing is written for any.
    Gone { key: CrateKey },
    /// The [`Served`] is of another database (`served`) than the one asked to store
    /// (`asked`): its crates' keys name another world's crates.
    OtherDatabase { served: Nonce, asked: Nonce },
    /// The image store failed, or an image did not encode as a store keeps it: what and
    /// which crate.
    Store(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Gone { key } => write!(
                f,
                "crate key {} of a dependency serve names no crate of the database now: the \
                 crate was removed after the serve, so the serve is stale for it (rule \
                 keys-cover-every-input); serve again before storing",
                key.index()
            ),
            StoreError::OtherDatabase { served, asked } => write!(
                f,
                "a dependency serve of database {served:?} was stored through database \
                 {asked:?}: its crate keys name the other database's crates \
                 (rule keys-cover-every-input)"
            ),
            StoreError::Store(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for StoreError {}

/// The message, for a caller whose errors are text (`blaze_lower::LowerError::new`).
impl From<StoreError> for String {
    fn from(error: StoreError) -> String {
        error.to_string()
    }
}

/// The crates a database holds lowered: each crate a [`serve`] walked that it gave no image,
/// by key, with its id at that serve (R3: a database that asked for a crate before and holds
/// its state is not served it again). The driver's, kept with the database across its serves.
///
/// The invariant: a crate whose id is the one noted (so its arena is the one the serve left:
/// a crate's id names one arena for the database's life, base-db `id_space`) and that is
/// still served no image is *live and lowered*, and a serve does not ask the store for it:
/// it stays lowered, whatever its image key says. Only a crate with no arena noted yet, or
/// whose arena was made again since (a new id, its memo gone with the old arena), takes an
/// image. Sound because the memo answers a carried arena from its current inputs: an image
/// key that moved while the arena was carried moved for an input none of its queries read
/// (a dependency's surface, which the key covers), and a lowered crate and its image answer
/// the same for every query read through a stable id (its def map, lang items, diagnostic
/// items and signatures by item key: `tests::a_served_dependency_resolves_as_a_fresh_one`).
/// Where they differ is the in-session ids of block scopes (an image's are paged,
/// `ScopeSlot::Paged`) and [`super::is_served`] itself, which only the arena's own entries
/// read: switching a live crate to its image is exactly what would show that difference, and
/// drop the crate's arena and its dependents' for it (the first edit after a cold build:
/// 24 arenas, 3.2 s).
///
/// The floor this serves: an edit of a workspace crate after the build that lowered the
/// sealed dependencies redoes and re-reads nothing of them.
pub struct Lowered {
    /// The database the ids are of: another nonce is another id space, where none holds.
    nonce: Option<Nonce>,
    by_key: HashMap<CrateKey, Crate, Global>,
}

impl Default for Lowered {
    fn default() -> Lowered {
        Lowered { nonce: None, by_key: HashMap::new_in(Global) }
    }
}

impl Lowered {
    /// Notes each of `crates` as `db`'s snapshot has it: lowered where it is given no image,
    /// else not. A crate the snapshot no longer has is forgotten.
    pub fn note(&mut self, db: &Db, crates: impl IntoIterator<Item = CrateKey>) {
        if self.nonce != Some(db.nonce()) {
            self.by_key.clear();
            self.nonce = Some(db.nonce());
        }
        for key in crates {
            match db.crate_at(key) {
                Some(krate) if !super::is_served(db, krate) => {
                    self.by_key.insert(key, krate);
                }
                _ => {
                    self.by_key.remove(&key);
                }
            }
        }
    }

    /// Whether `krate` is live and lowered in `db`: noted unserved with this very id.
    pub fn holds(&self, db: &Db, krate: Crate) -> bool {
        self.nonce == Some(db.nonce())
            && !super::is_served(db, krate)
            && self.by_key.get(&krate.key(db)) == Some(&krate)
    }
}

/// Reads each of `crates` that is [`eligible`] (and its dependencies) from the image `images`
/// keep for it, by `build`, the blaze reading: `mode` says what is done with each image.
///
/// Reads this snapshot and writes nothing: what it found is given in the next one. Each
/// `(crate, image)` returned is given to the crate's own arena by the driver while it builds
/// that snapshot ([`super::give_image`], which gives the image, its lang items, its def map
/// pages and, under `Mode::Check` and `Mode::Verify`, its def map whole); each crate of
/// [`Served::withdrawn`] is given none there. A crate served before under the same key and mode
/// is neither: the driver gives it the same image again. One whose key or mode is another is
/// fetched again, and is withdrawn when none is found. A write that moves a served crate's
/// files withdraws its image in that write ([`withdrawn`]): it is fetched again here like a
/// crate never served.
///
/// An image that does not read under the key it is filed by, or reads that do not decode, are
/// an error naming the crate and why, never a crate resolved instead: the key covers what the
/// image was made from and the store checked the bytes, so such an image is a bug in the key or
/// an encoding.
///
/// [`eligible`]: super::eligible
pub fn serve(
    db: &Db,
    crates: &[Crate],
    build: ContentId,
    // dyn: the driver's store, one call per crate per session
    images: &dyn ImageStore,
    mode: Mode,
) -> Result<(Served, Vec<(Crate, Image), Global>), String> {
    serve_keeping(db, crates, build, images, mode, &Lowered::default())
}

/// [`serve`], asking the store for no crate `lowered` holds live and lowered
/// ([`Lowered::holds`]), whether it is one of `crates` or a dependency of one: it is neither
/// given an image nor withdrawn, and is keyed in [`Served::keys`] as any crate walked.
pub fn serve_keeping(
    db: &Db,
    crates: &[Crate],
    build: ContentId,
    // dyn: the driver's store, one call per crate per session
    images: &dyn ImageStore,
    mode: Mode,
    lowered: &Lowered,
) -> Result<(Served, Vec<(Crate, Image), Global>), String> {
    let keys = keys(db, crates, build);
    // What this snapshot gives each crate is its own slot ([`super::image_of`]): a crate kept
    // under the same key and mode is given the same image again by the driver.
    let kept = |krate: &Crate, key: &ContentId| {
        super::image_of(db, *krate).is_some_and(|it| it.base == *key && it.mode == mode)
    };
    let mut order: Vec<(Crate, ContentId)> = (keys.iter())
        .filter(|(krate, key)| !kept(krate, key) && !lowered.holds(db, **krate))
        .map(|(&krate, &key)| (krate, key))
        // alloc: setup: once per serve
        .collect();
    order.sort_unstable_by_key(|&(_, key)| key);
    // alloc: setup: one slot per crate, once per serve
    #[expect(
        clippy::disallowed_types,
        reason = "fetched image results, set once per crate in a serve: not global"
    )]
    let found: Vec<std::sync::OnceLock<Fetched>> =
        order.iter().map(|_| std::sync::OnceLock::new()).collect();
    // What the fetches' searches of the slots come to: counted from every thread of the
    // store's, taken once they have all ended.
    let lookups = blaze_record::Tally::new();
    images.for_each(order.len(), &|at| {
        let (krate, key) = order[at];
        _ = found[at].set(fetch(db, krate, key, images, &lookups, mode));
    });
    let lookups = lookups.take();
    let mut hits: Vec<Crate> =
        // alloc: setup: once per serve
        keys.iter().filter(|(krate, key)| kept(krate, key)).map(|(&krate, _)| krate).collect();
    // alloc: none until a crate is found: the images to give in the next snapshot
    let mut given: Vec<(Crate, Image), Global> = Vec::new_in(Global);
    // alloc: none until a crate that held an image finds none
    let mut withdrawn: Vec<Crate, Global> = Vec::new_in(Global);
    // alloc: none until a crate's reads are stale
    let mut stale = Vec::new();
    // alloc: none until a crate's image is of layout 7
    let mut old_layout = Vec::new();
    for (&(krate, base), found) in order.iter().zip(found) {
        let found = found.into_inner().unwrap_or_else(|| {
            panic!("`ImageStore::for_each` did not run the task of crate {}", krate.named(db))
        });
        let held = super::is_served(db, krate);
        let missed = |withdrawn: &mut Vec<Crate, Global>| {
            if held {
                withdrawn.push(krate);
            }
        };
        let hit = match found? {
            Found::Hit(hit) => hit,
            Found::Miss { stale: true } => {
                stale.push(krate);
                missed(&mut withdrawn);
                continue;
            }
            Found::Miss { stale: false } => {
                missed(&mut withdrawn);
                continue;
            }
            Found::OldLayout => {
                old_layout.push(krate);
                missed(&mut withdrawn);
                continue;
            }
        };
        given.push((krate, hit.image(base, mode)));
        hits.push(krate);
    }
    hits.sort_unstable_by_key(|krate| keys[krate]);
    if !given.is_empty() || !withdrawn.is_empty() {
        // What this serve changes of how each crate is read, named: a crate that gains an
        // image is read from it from the next snapshot on, and one that loses its image is
        // resolved again (rule 19). Bounded: the first [`NAMED`] of each, the rest counted.
        let name = |krate: &Crate| krate.named(db).to_string(); // alloc: an event's names, at most `NAMED` a list
        let list = |crates: &[Crate]| crates.iter().take(NAMED).map(name).collect::<Vec<_>>().join(", "); // alloc: as above
        // alloc: once per serve that changed what is given
        let mut gained: Vec<Crate, Global> = Vec::new_in(Global);
        gained.extend(given.iter().map(|it| it.0).filter(|krate| !super::is_served(db, *krate)));
        let again = given.len() - gained.len();
        tracing::info!(
            target: "blaze",
            asked = order.len() as u64,
            gained = gained.len() as u64,
            lost = withdrawn.len() as u64,
            again = again as u64,
            given = given.len() as u64,
            gained_first = %list(&gained),
            lost_first = %list(&withdrawn),
            "dependency images served: crates that gained an image are read from it, crates that lost theirs are resolved again"
        );
    }
    // Each crate by its key in the world, so what is served outlives the commit that gives
    // the images ([`Served`]); `given` is this snapshot's, used before that commit.
    let world = |krate: &Crate| krate.key(db);
    // alloc: setup: once per serve, as `keys`
    let keys = HashMap::from_iter_in(keys.iter().map(|(krate, &key)| (world(krate), key)), Global);
    let mut gone: Vec<CrateKey, Global> = Vec::with_capacity_in(withdrawn.len(), Global);
    gone.extend(withdrawn.iter().map(world));
    let served = Served {
        nonce: db.nonce(),
        keys,
        // alloc: setup: once per serve, as above
        hits: hits.iter().map(world).collect(),
        // alloc: none for a serve with no stale crate
        stale: stale.iter().map(world).collect(),
        // alloc: none for a serve with no image of layout 7
        old_layout: old_layout.iter().map(world).collect(),
        build,
        lookups,
        stored: StoredOf::default(),
        withdrawn: gone,
    };
    Ok((served, given))
}

/// What one task of [`serve`] found of a crate.
enum Found {
    Hit(Hit),
    /// No image; `stale` when reads were kept under its key.
    Miss { stale: bool },
    /// An image of layout 7 under its key ([`ImageRef::is_layout_7`]): a miss, said by name.
    OldLayout,
}

/// A crate's records, found: its image, where its parts are in it, the key it is filed under,
/// and the source of its def map pages, read a part at a time; under
/// `Mode::Check` and `Mode::Verify`, the def map pages whole as well.
struct Hit {
    bytes: ImageBuf,
    parts: [Range<usize>; Part::ALL.len()],
    filed: ContentId,
    pages: Paged,
    whole: Option<ImageBuf>,
}

impl Hit {
    /// The image this hit is, of a crate walked under `base`, given in `mode`.
    fn image(self, base: ContentId, mode: Mode) -> Image {
        let Hit { bytes, parts, filed, pages: def_pages, whole } = self;
        let lang_items = Frozen { bytes: PartBytes::of(&bytes, &parts, Part::LangItems), mode };
        let diagnostic_items =
            Frozen { bytes: PartBytes::of(&bytes, &parts, Part::DiagnosticItems), mode };
        // Under `Mode::Check` and `Mode::Verify` the def map's record whole, which what the
        // crate resolves to (`Check`) or what was read of its pages (`Verify`) is compared
        // with; never when serving, which holds no byte of it past a read.
        let def_map = whole.map(|whole| {
            let range = 0..whole.bytes().len();
            Frozen { bytes: PartBytes::In { buffer: whole, range }, mode }
        });
        // Read from pages in `Mode::Serve` and `Mode::Verify`; `Mode::Check` resolves the
        // crate and compares what it resolves to with its pages.
        let pages = (mode != Mode::Check).then_some(def_pages);
        Image { base, key: filed, bytes, parts, mode, lang_items, diagnostic_items, def_map, pages }
    }
}

/// The image of each of `found`, a crate and the crate's key a serve walked and stored its
/// image under (no found record holds that key since layout 2, which its own key does not
/// cover: [`super::found`]; no driver passes one now), for a start to give before any query of the
/// crate runs (docs/v2/plan/design/front-end-on-memo.md, "A start gives what it found"): so the
/// first [`serve_keeping`], whose walk keys the crate under that same key, finds it kept
/// (`image_of(..).base` is its key, the mode is its mode), asks the store nothing and gives
/// nothing, and the crate's arena is not made again for it.
///
/// No key is walked here, and no slot's reads are valued: an image is asked under the
/// record's key alone ([`filed::under_key_alone`]), the case of a crate that read nothing.
/// One filed by its reads is no hit here (the record's `filed` is not its `base`, and the
/// caller passes none such) and is left to the first serve, as a crate with no record is. A
/// crate that is not [`super::eligible`] is skipped: only an eligible one is keyed by a serve,
/// and an image no serve keys would never be checked against its key.
///
/// The record is no proof ([`super::found`], "What a record is not"): what is given here is checked
/// by the first serve that walks the crate. Its key there equal to `base`, the image is the
/// one the serve would have found under that key; another, and the serve fetches anew under
/// its own key or withdraws the image, as for any crate whose key moved, before any task
/// reads the crate (a unit's crates are walked by its serve before its task runs).
///
/// An image that does not read under its key, or pages that name files not the crate's, are
/// the same errors [`serve`] says, naming the crate.
pub fn serve_found(
    db: &Db,
    found: &[(Crate, ContentId)],
    // dyn: the driver's store, one call per crate per session
    images: &dyn ImageStore,
    mode: Mode,
) -> Result<Vec<(Crate, Image), Global>, String> {
    let _span = tracing::info_span!(target: "blaze", "found images", asked = found.len() as u64)
        .entered();
    let mut given: Vec<(Crate, Image), Global> = Vec::with_capacity_in(found.len(), Global);
    for &(krate, base) in found {
        if !super::eligible(db, krate) {
            continue;
        }
        let bytes = filed::under_key_alone(db, krate, base, |at| {
            images.get(&at).map_err(|why| {
                // alloc: error text
                format!("reading the image of crate {} (key {at:?}): {why}", krate.named(db))
            })
        })?;
        let Some(bytes) = bytes else { continue };
        match open(db, krate, base, bytes, images, mode)? {
            Found::Hit(hit) => given.push((krate, hit.image(base, mode))),
            // Said by `open`, by name; the first serve makes it from source as it would.
            Found::Miss { .. } | Found::OldLayout => {}
        }
    }
    tracing::info!(
        target: "blaze",
        asked = found.len() as u64,
        given = given.len() as u64,
        "found images: crates given their image before any query of theirs ran"
    );
    Ok(given)
}

type Fetched = Result<Found, String>;

/// The records of `krate`, whose key is `key`: its image under the key, or under the reads of
/// one of its slots and their values now ([`filed::search`], which counts its search in
/// `lookups`), then its pages ([`open`]).
fn fetch(
    db: &Db,
    krate: Crate,
    key: ContentId,
    // dyn: the driver's store, one call per crate per session
    images: &dyn ImageStore,
    lookups: &blaze_record::Tally,
    mode: Mode,
) -> Fetched {
    let found = filed::search(db, krate, key, images, lookups, |at| {
        let bytes = images.get(&at).map_err(|why| {
            // alloc: error text
            format!("reading the image of crate {} (key {at:?}): {why}", krate.named(db))
        })?;
        Ok(bytes.map(|bytes| (at, bytes)))
    })?;
    match found {
        Ok((filed, bytes)) => open(db, krate, filed, bytes, images, mode),
        Err(stale) => Ok(Found::Miss { stale }),
    }
}

/// The image `bytes`, filed under `filed`, opened, and the crate's def map pages found beside
/// it, the files the pages name checked (their header and files alone are read). An image of
/// layout 7 is the named miss [`Found::OldLayout`]. An image whose pages the store does not
/// hold is a miss, said by name: the two are written together ([`make`]) and pinned together
/// ([`front_keys`]), so a store that holds one alone lost a record, and the crate is made again
/// from source and both written again. Its items' entries are kept under their own keys
/// ([`super::facts::keep_all`]), found one at a time by whichever query asks.
fn open(
    db: &Db,
    krate: Crate,
    filed: ContentId,
    bytes: ImageBuf,
    // dyn: the driver's store, one call per crate per session
    images: &dyn ImageStore,
    mode: Mode,
) -> Fetched {
    if ImageRef::is_layout_7(bytes.bytes()) {
        tracing::info!(
            target: "blaze",
            krate = %krate.named(db),
            "records of {}, def map, layout 7: made again from source",
            krate.named(db)
        );
        return Ok(Found::OldLayout);
    }
    let image = ImageRef::open(bytes.bytes(), filed).map_err(|why| {
        // alloc: error text
        format!("the image of crate {} (key {filed:?}): {why}", krate.named(db))
    })?;
    let parts = image.ranges(bytes.bytes());
    let found = |key: ContentId, what: &str| {
        images.pages(&key).map_err(|why| {
            // alloc: error text
            format!("reading the {what} of crate {} (key {key:?}): {why}", krate.named(db))
        })
    };
    let pages_at = pages_key(filed);
    let Some(def_pages) = found(pages_at, "def map pages")? else {
        tracing::info!(
            target: "blaze",
            krate = %krate.named(db),
            "records of {}: an image with no def map pages beside it, made again from source",
            krate.named(db)
        );
        return Ok(Found::Miss { stale: false });
    };
    if let Some(why) = pages::unreadable(db, krate, &*def_pages) {
        // alloc: error text
        return Err(format!(
            "the def map pages of crate {} (key {pages_at:?}) name files that are not the \
             crate's: {why}; the key leaves out an input",
            krate.named(db)
        ));
    }
    let whole = match mode {
        Mode::Serve => None,
        Mode::Check | Mode::Verify => match images.get(&pages_at) {
            Ok(Some(whole)) => Some(whole),
            // alloc: error text
            Ok(None) => return Err(format!(
                "the def map pages of crate {} (key {pages_at:?}) were found a part at a time and not whole",
                krate.named(db)
            )),
            // alloc: error text
            Err(why) => return Err(format!(
                "reading the def map pages of crate {} (key {pages_at:?}): {why}",
                krate.named(db)
            )),
        },
    };
    Ok(Found::Hit(Hit {
        bytes,
        parts,
        filed,
        pages: Paged { key: pages_at, source: def_pages },
        whole,
    }))
}

/// The crates of `served` (those the driver gives an image in this snapshot) that a write
/// takes the image from, whose files `files` were set or added and whose source roots `roots`
/// had their file sets changed (which covers a file removed): the driver gives each of them none in the snapshot the
/// write makes, nor its frozen def map, lang items or pages ([`super::give_image`] gives the
/// four together). A crate's own files moved when a file of its source root was set, added or
/// removed: an edit, a build script's run that left other `OUT_DIR` files, a view moved. So
/// does every crate read from an image that depends on a crate whose files moved, whether that
/// crate is served or not.
///
/// The key covers every file of the crate's source root and every crate it depends on, so the
/// image no longer names the crate. Kept, it would be read against files it was not made from,
/// which the content check refuses ([`frozen::Unreadable`]). The next [`serve`] fetches it
/// under its new key. Reads this snapshot only: the driver asks it while it makes the write.
///
/// A crate whose cfg or env alone moved keeps its image until that serve. The image still
/// reads, and holds what the crate was before the write.
pub fn withdrawn(
    db: &Db,
    served: &[Crate],
    files: &[base_db::FileId],
    roots: &[base_db::SourceRootId],
) -> Vec<Crate, Global> {
    if served.is_empty() || (files.is_empty() && roots.is_empty()) {
        return Vec::new_in(Global);
    }
    let moved = moved_crates(db, files, roots);
    if moved.is_empty() {
        return Vec::new_in(Global);
    }
    // alloc: one entry per crate walked, once per write that moved a served crate's files
    let mut reaches = HashMap::new_in(Global);
    // alloc: as above
    let mut gone = Vec::new_in(Global);
    gone.extend(served.iter().copied().filter(|&krate| reaches_moved(db, krate, &moved, &mut reaches)));
    gone.sort_unstable();
    gone.dedup();
    if !gone.is_empty() {
        tracing::info!(target: "blaze", crates = gone.len(), "dependency images withdrawn: their files moved");
    }
    gone
}

/// The crates whose root is in a source root a write moved: a file of it set or added
/// (`files`), or its file set changed (`roots`, which covers a file removed).
fn moved_crates(
    db: &Db,
    files: &[base_db::FileId],
    roots: &[base_db::SourceRootId],
) -> HashSet<Crate, Global> {
    // alloc: once per write that moved a file while any crate is served
    let mut roots: Vec<base_db::SourceRootId, Global> = roots.to_vec_in(Global);
    for &file in files {
        roots.push(db.file_source_root(file).source_root_id(db));
    }
    roots.sort_unstable();
    roots.dedup();
    // alloc: as above
    roots.iter().flat_map(|&root| base_db::source_root_crates(db, root).iter().copied()).collect()
}

/// Whether `krate` is one of `moved` or depends on one, through any path: memoized in `memo`.
fn reaches_moved(
    db: &Db,
    krate: Crate,
    moved: &HashSet<Crate, Global>,
    memo: &mut HashMap<Crate, bool, Global>,
) -> bool {
    if let Some(&it) = memo.get(&krate) {
        return it;
    }
    let it = moved.contains(&krate)
        || krate
            .data(db)
            .dependencies
            .iter()
            .any(|dep| reaches_moved(db, dep.crate_id, moved, memo));
    memo.insert(krate, it);
    it
}

/// Writes to `images` the image of each crate [`serve`] found none of that `which` names,
/// resolving it in `db`, one task a crate ([`ImageStore::for_each`]). A crate whose image
/// would not cover every input is refused, with why. An image the store will not keep is an
/// error naming the crate. A crate written before under the same key writes the same bytes,
/// which the store keeps once, so a store with [`Which::All`] after one with
/// [`Which::Sealed`] writes the members and what was refused before, and keeps the rest.
///
/// `served` may be of any earlier snapshot of `db`'s database ([`Served`]'s invariant): each
/// crate it names is found in `db` by its key. One that `db` no longer has is
/// [`StoreError::Gone`] naming its key, and a serve of another database is
/// [`StoreError::OtherDatabase`]; both before anything is written. Floor: one lookup of each
/// key the serve holds (an index into the world's table), against the resolution and
/// encoding of each crate written.
// dyn: the driver's store, one call per crate per session
pub fn store(
    db: &Db,
    served: &Served,
    // dyn: as above
    images: &dyn ImageStore,
    which: Which,
) -> Result<Stored, StoreError> {
    if served.nonce != db.nonce() {
        return Err(StoreError::OtherDatabase { served: served.nonce, asked: db.nonce() });
    }
    // The crates read from an image and those an earlier store of this serve wrote: neither
    // is written again.
    // alloc: setup: once per session
    let mut hits: HashSet<CrateKey, Global> = served.hits.iter().copied().collect();
    hits.extend(served.stored.0.lock().iter().copied());
    let named = |krate: Crate| match which {
        Which::All => true,
        Which::Sealed => super::key::sealed(db, krate),
    };
    // alloc: setup: once per session
    let mut order: Vec<(Crate, ContentId)> = Vec::with_capacity(served.keys.len());
    // Every key is looked up, a hit's too: a serve that names a crate gone is stale whole.
    // The least such key is the one named, so the error is the same in every run. A crate is
    // gone when the graph no longer lists it: the world keeps a crate's key and slot for its
    // life (`Crates::set_graph` takes one the graph lacks off the list), so `crate_at` still
    // finds it, and only the list says a write removed it.
    // alloc: setup: once per store, one entry a listed crate
    let listed: HashSet<Crate, Global> = base_db::all_crates(db).iter().copied().collect();
    let mut gone: Option<CrateKey> = None;
    for (&world, &key) in &served.keys {
        let Some(krate) = db.crate_at(world).filter(|krate| listed.contains(krate)) else {
            gone = Some(gone.map_or(world, |it| it.min(world)));
            continue;
        };
        if !hits.contains(&world) && named(krate) {
            order.push((krate, key));
        }
    }
    if let Some(key) = gone {
        return Err(StoreError::Gone { key });
    }
    order.sort_unstable_by_key(|&(_, key)| key);
    // The keys as the database has them now, in one walk: a crate whose key moved since the
    // serve is not written under the old one.
    let now = {
        // alloc: setup: once per store
        let crates: Vec<Crate> = order.iter().map(|&(krate, _)| krate).collect();
        let _span = tracing::info_span!(target: "blaze", "dependency image keys at store", crates = crates.len()).entered();
        keys(db, &crates, served.build)
    };
    #[expect(clippy::disallowed_types, reason = "image creation results, set once per crate in a session: not global")]
    let made: Vec<std::sync::OnceLock<Result<Result<ContentId, Refused>, String>>> =
        // alloc: setup: one slot per crate, once per session
        order.iter().map(|_| std::sync::OnceLock::new()).collect();
    images.for_each(order.len(), &|at| {
        let (krate, key) = order[at];
        let made_now = match now.get(&krate) {
            Some(&it) if it == key => make(db, krate, key, images),
            now => Ok(Err(Refused::KeyMoved { served: key, now: now.copied() })),
        };
        _ = made[at].set(made_now);
    });
    let mut stored = Stored::default();
    for (&(krate, _), made) in order.iter().zip(made) {
        let made = made.into_inner().unwrap_or_else(|| {
            panic!("`ImageStore::for_each` did not run the task of crate {}", krate.named(db))
        });
        let world = krate.key(db);
        match made.map_err(StoreError::Store)? {
            Ok(filed) => {
                stored.written.push(world);
                stored.filed.insert(world, filed);
                served.stored.0.lock().insert(world);
            }
            Err(why) => {
                // Said, a crate each: a crate with no image is resolved again by every daemon
                // that starts (reth's first build after a restart resolved 827 crates and
                // read 291 from images, and nothing in its profile said why of the rest).
                tracing::info!(
                    target: "blaze",
                    krate = %krate.named(db),
                    kind = why.kind(),
                    why = %why,
                    "a crate's image is not written: every daemon that starts resolves the crate again"
                );
                stored.refused.push((world, why));
            }
        }
    }
    Ok(stored)
}

/// Writes the image of `krate`, whose key is `key`, or says why it is refused: one task of
/// [`store`].
fn make(
    db: &Db,
    krate: Crate,
    key: ContentId,
    // dyn: the driver's store, one call per crate per session
    images: &dyn ImageStore,
) -> Result<Result<ContentId, Refused>, String> {
    // alloc: setup: the macro calls the crate's signatures name, once per crate imaged
    let mut calls = Vec::new_in(Global);
    // The late reads and what no surface can ask are a surface's ([`image_parts`]): a store
    // files the crate by its [`Reads`].
    let parts = match checked_parts(db, krate, &mut calls, super::late::Nested::Walk) {
        Ok((parts, _)) => parts,
        Err(why) => return Ok(Err(why)),
    };
    // The crate's def map pages, made before anything is kept: a def map that does not write
    // as pages refuses the crate whole, so no image is kept whose pages never will be.
    let paged = match pages::encode(db, krate) {
        Ok(paged) => paged,
        Err(why) => return Ok(Err(Refused::Encode { part: Part::DefMap, why })),
    };
    // Every part of the image (lang items, signatures, diagnostic items: [`Part::ALL`]), then
    // the pages beside it.
    let held: [(Part, &[u8]); Part::ALL.len() + 1] = std::array::from_fn(|at| match Part::ALL.get(at) {
        Some(&part) => (part, parts[at].as_bytes()),
        None => (Part::DefMap, paged.as_slice()),
    });
    if let Some(why) = Base::of(db, krate).held_by(&held) {
        return Ok(Err(why));
    }
    let reads = match Reads::of(db, krate, &calls) {
        Ok(reads) => reads,
        Err(why) => return Ok(Err(why)),
    };
    let filed = match filed::file_by_reads(db, krate, key, &reads, images)? {
        Ok(filed) => filed,
        Err(why) => return Ok(Err(why)),
    };
    let image = write_image(filed, parts.each_ref());
    drop(parts);
    images.put(filed, image).map_err(|why| {
        // alloc: error text
        format!("keeping the image of crate {} (key {filed:?}): {why}", krate.named(db))
    })?;
    let key = pages_key(filed);
    images.put(key, paged).map_err(|why| {
        // alloc: error text
        format!("keeping the def map pages of crate {} (key {key:?}): {why}", krate.named(db))
    })?;
    // Its items' entries (rule 65: each under its own key, none under the crate's): every
    // item's flags, assoc items and facts, asked here of the database that resolved the crate
    // from source, so each is made off the parse it holds and kept through the database's
    // entry store, which the driver writes with this store. Never for a crate read from an
    // image, which [`store`] does not reach.
    super::facts::keep_all(db, krate);
    Ok(Ok(filed))
}

/// The parts of the image of `krate`, in the order of [`Part::ALL`], as a `Surface` record
/// holds them (crates/blaze-front/src/surface.rs): each to be passed as it is to
/// [`write_image`], under the record's address. The image a [`store`] would write, less what
/// filing it in the store asks (its reads' slots, its key).
///
/// A surface's key covers its unit's own text only (`blaze_keyed::kind::SurfaceInputs`:
/// tool, settings, unit, pieces), and its shape what it asked of other surfaces and of the
/// request's late reads. So every input the image has is one of those or a refusal here:
///
/// - Refused, as [`store`] refuses them, since no key made from text covers them: a def map
///   that uses the session's macros, a module whose file is outside the crate's source root
///   or is not there ([`refusal`]), an `include!` of a file of the VFS in another source root
///   or of no source root (`Refused::OutsideSourceRoot`, [`super::late::walk`]), a part that
///   holds a path of the checkout (`Refused::MachinePath`), and an expansion of a procedural
///   macro whose reads are not known (`Refused::ProcMacro`).
/// - Refused here alone ([`Refused::Unasked`]): an expansion of a procedural macro that
///   traced reading something, and an `include!` of a file outside the VFS. A store files
///   them by their values ([`Reads`]); a surface's shape has no fact that names them.
/// - Refused when it cannot happen ([`Refused::LateTwice`]): one late read with two values.
/// - Given to the caller to ask in its shape ([`super::ImageParts::late`]): every `env!`,
///   `option_env!`, `include!`, `include_str!` and `include_bytes!` of a file of its source
///   root that the image or the def map pages depend on, with the value its expansion saw,
///   nested ones included ([`super::late`]). What a body's expansions read is the body's.
/// - Left to the caller's shape as it is: what the crate read of its dependencies, which is
///   their surfaces' (each a fact of the shape), their expansions' reads included.
///
/// Floor: the crate's lang items and signatures encoded once, and the macro calls its image
/// is made from walked once, for its refusals and its late reads alike. Here: that, the def
/// map's modules walked for [`refusal`], and the parts scanned once for a path of the
/// checkout. A crate with no late read allocates nothing for [`super::ImageParts::late`].
pub fn image_parts(db: &Db, krate: Crate) -> Result<super::ImageParts, Refused> {
    image_parts_walking(db, krate, super::late::Nested::Walk)
}

/// [`image_parts`], its walk reading nested calls or not ([`super::late::Nested`]): `Skip` is
/// a test's planted control alone.
pub(super) fn image_parts_walking(
    db: &Db,
    krate: Crate,
    nested: super::late::Nested,
) -> Result<super::ImageParts, Refused> {
    // alloc: the macro calls the crate's signatures name, once per surface written
    let mut calls = Vec::new_in(Global);
    let (parts, walked) = checked_parts(db, krate, &mut calls, nested)?;
    let held: [(Part, &[u8]); Part::ALL.len()] =
        std::array::from_fn(|at| (Part::ALL[at], parts[at].as_bytes()));
    if let Some(why) = Base::of(db, krate).held_by(&held).and_then(ends) {
        return Err(why);
    }
    // Under a census every one was recorded as it was met (`late::walk`), and this is none.
    if let Some(why) = walked.unasked {
        return Err(why);
    }
    Ok(super::ImageParts { parts, late: walked.late })
}

/// The parts of the image of `krate` ([`encode`]), with in `calls` the macro calls its
/// signatures name, and the one walk of the calls the image is made from
/// ([`super::late::walk`]: its late reads and what no surface can ask); or the refusal of the
/// crate that its def map ([`refusal`]) or that walk makes. What [`store`] and
/// [`image_parts`] both check; a store ignores the late reads and the unaskable.
fn checked_parts(
    db: &Db,
    krate: Crate,
    calls: &mut Vec<MacroCallId>,
    nested: super::late::Nested,
) -> Result<([Portable; Part::ALL.len()], super::late::Walked), Refused> {
    if let Some(why) = refusal(db, krate) {
        return Err(why);
    }
    // An encoding refused ends the census too: there are no parts to go on with.
    let parts = encode(db, krate, calls)?;
    let walked = super::late::walk(db, krate, calls, nested)?;
    Ok((parts, walked))
}

/// The parts of the image of `krate`, in the order of [`Part::ALL`], and in `calls` the macro
/// calls its signatures name.
fn encode(
    db: &Db,
    krate: Crate,
    calls: &mut Vec<MacroCallId>,
) -> Result<[Portable; Part::ALL.len()], Refused> {
    let refused = |part| move |why| Refused::Encode { part, why };
    Ok([
        // `nameres::frozen` writes no id, only keys ([`frozen::Enc`]).
        Portable::written(
            frozen::encode_lang_items(db, krate).map_err(refused(Part::LangItems))?,
        ),
        signatures::encode(db, krate, calls).map_err(refused(Part::Signatures))?,
        Portable::written(
            frozen::encode_diagnostic_items(db, krate).map_err(refused(Part::DiagnosticItems))?,
        ),
    ])
}

/// Why the image of `krate` would leave out an input its key and reads do not cover, if it
/// would, as far as its def map says, before its image is encoded: a def map that uses the
/// session's macros, a module whose file is outside the crate's source root, or a module whose
/// file is not there.
///
/// The key covers every file of the crate's source root by its path and content, a file with
/// no text by its bytes ([`super::key`]), and a frozen def map's files are looked up there
/// alone when it is read. A file the crate's resolution or signatures read from anywhere else
/// is not known until the crate is resolved, so no key made before that can cover it, and an
/// image filed by it (as by [`Reads`]) would be kept as it is by a [`serve`] whose key did not
/// move after that file was edited: such a crate is refused. The `include!`s the image depends
/// on are asked once it is encoded, when its signatures' calls are known
/// (`super::late::walk`).
pub(super) fn refusal(db: &Db, krate: Crate) -> Option<Refused> {
    let def_map = crate_def_map(db, krate);
    if def_map.session_macros
        && let Some(why) = ends(Refused::SessionMacros)
    {
        return Some(why);
    }
    let root = db.file_source_root(krate.data(db).root_file_id).source_root_id(db);
    for (id, module) in def_map.modules(db) {
        if let Some(file) = module.origin.file_id() {
            let file = file.file_id(db);
            if db.file_source_root(file).source_root_id(db) != root {
                // Where the module is declared, as a person finds it (rule 19): the file and
                // line of its `mod`, or of the macro call whose expansion wrote it.
                let declared = match module.origin.declaration() {
                    Some(ast) => site(
                        db,
                        InFile::new(ast.file_id, ast.to_range(db)).original_node_file_range_rooted(db),
                    ),
                    None => "the crate root".to_owned(), // alloc: error text
                };
                // A file no source root has was read from disk by its declaration
                // (`Db::module_source`): the files the crate was given with, which its key
                // covers, did not reach that declaration.
                let (path, how) = match db.files().included_source_path(file) {
                    Some(path) => (
                        path.to_string(), // alloc: error text
                        ", read from disk: no file of the crate's source root is there, so the \
                         declaration is one the files its key covers do not reach (a `mod` a \
                         macro call wrote, as `cfg_select!` does)",
                    ),
                    None => (shown_path(db, file), ", a file of another source root"),
                };
                // alloc: why a crate is refused, once per refused crate
                let what = format!(
                    "module `{}` of crate {}, declared at {declared}, is the file {path}{how}",
                    module_path(db, id),
                    krate.named(db),
                );
                if let Some(why) = ends(Refused::OutsideSourceRoot { what }) {
                    return Some(why);
                }
            }
        }
    }
    let missing = def_map.diagnostics(db).iter().filter_map(|it| match &it.kind {
        DefDiagnosticKind::UnresolvedModule { ast, candidates } => {
            Some((it.in_module, ast, candidates))
        }
        _ => None,
    });
    for (parent, ast, candidates) in missing {
        let name = match ast.to_node(db).name() {
            Some(name) => name.text().to_string(), // alloc: error text
            None => "_".to_owned(),                // alloc: error text
        };
        let declared =
            InFile::new(ast.file_id, ast.to_range(db)).original_node_file_range_rooted(db);
        let anchor = ast.file_id.original_file_respecting_includes(db).file_id(db);
        // alloc: why a crate is refused, once per refused crate
        // In a snapshot ([`base_db::Db::sealed`]) the crate's files are those its key task
        // gathered and nothing is read from disk, so a file that is on disk but was not
        // gathered is missing here too: the gather did not spell it.
        let what = format!(
            "module `{}::{name}` of crate {}, declared at {}, has no file: none of {} (from the \
             directory of {}) is among the crate's files (the key task's tokens did not spell \
             this module's file, or it is not there)",
            module_path(db, parent),
            krate.named(db),
            site(db, declared),
            candidates.join(", "),
            shown_path(db, anchor),
        );
        if let Some(why) = ends(Refused::ModuleFileMissing { what }) {
            return Some(why);
        }
    }
    None
}

/// The path the source root has `file` at, as an error shows it.
pub(super) fn shown_path(db: &Db, file: base_db::FileId) -> String {
    match db.path_for_file(file) {
        Some(path) => path.to_string(),                 // alloc: error text
        None => format!("{file:?}, which has no path"), // alloc: error text
    }
}

/// Where the call `loc` is in a file of its crate: the file's path and the line, from 1.
pub(super) fn call_site(db: &Db, loc: &hir_expand::MacroCallLoc) -> String {
    site(db, loc.kind.original_call_range_with_input(db))
}

/// Where the item or node `ast` is written: the path of the file it is in (the file of the
/// macro call that wrote it, where it came from an expansion) and the line it starts on, from
/// 1, as the image refusals print a place. For a refusal's text, said once at the edge.
#[blaze::renders_positions]
pub fn ast_site(db: &Db, ast: hir_expand::ErasedAstId) -> String {
    let range = hir_expand::InFile::new(ast.file_id, ast.to_range(db));
    site(db, range.original_node_file_range_rooted(db))
}

/// Where `range` is: its file's path and the line it starts on, from 1.
fn site(db: &Db, range: hir_expand::FileRange) -> String {
    let file = range.file_id.file_id(db);
    let start = usize::from(range.range.start());
    let text = db.file_text(file).text(db);
    let line = text.get(..start).map_or(0, |it| it.matches('\n').count()) + 1;
    // alloc: error text
    format!("{}:{line}", shown_path(db, file))
}

/// The path of `module` in its crate, as `crate::a::b`: for a refusal's text.
pub fn module_path(db: &Db, module: crate::ModuleId) -> String {
    // alloc: error text
    let mut names = Vec::new();
    let mut at = Some(module);
    while let Some(it) = at {
        let Some(name) = it.name(db) else { break };
        names.push(name.as_str().to_owned()); // alloc: error text
        at = it.containing_module(db).map(|it| it.to_static());
    }
    names.push("crate".to_owned()); // alloc: error text
    names.reverse();
    names.join("::")
}
