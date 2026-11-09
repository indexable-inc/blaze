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

use base_db::{Crate, Db, Env};
use blaze_hash::{HashMap, HashSet};
use blaze_ids::ContentId;
use hir_expand::{
    InFile, MacroCallId, MacroCallKind, MacroDefKind, builtin::include_input_to_file_id,
};
use std::alloc::Global;
use syntax::ast::HasName;

use crate::{
    Lookup,
    deps::{
        DepImages, Image, filed,
        image::{ImageBuf, ImageRef, Part, PartBytes, write_image},
        key::{Base, keys},
        reads::Reads,
        signatures,
    },
    nameres::{
        crate_def_map,
        diagnostics::DefDiagnosticKind,
        frozen::{self, Frozen, FrozenDefMaps, FrozenLangItems, Mode},
    },
};

/// Where images are kept, and the threads that read and write them: what the driver adapts the
/// store or the global cache and its pool to. Every method is called from those threads.
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

    /// Runs `task(at)` for each `at` in `0..count`, on every core the driver has: a crate's
    /// image is checked and encoded by one task, so [`serve`] and [`store`] take the larger of
    /// the work over the cores and the largest crate's. Running them in order on one thread
    /// gives the same result.
    // dyn: one call per batch of crates, its task called once per crate
    fn for_each(&self, count: usize, task: &(dyn Fn(usize) + Sync)) {
        (0..count).for_each(task);
    }
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

    // dyn: as `ImageStore::for_each`
    fn for_each(&self, count: usize, task: &(dyn Fn(usize) + Sync)) {
        self.local.for_each(count, task)
    }
}

/// What [`serve`] did.
#[derive(Debug, Default)]
pub struct Served {
    /// The key of each crate read from an image or that can be: the [`eligible`] crates of
    /// those asked, and their dependencies.
    ///
    /// [`eligible`]: super::eligible
    pub keys: HashMap<Crate, ContentId, Global>,
    /// The crates read from an image, in the order of their keys.
    pub hits: Vec<Crate>,
    /// The crates with reads kept under their key but no image for the values those reads
    /// have now: something they read changed, so they are resolved and imaged again.
    pub stale: Vec<Crate>,
    /// The blaze the keys were made by: [`store`] makes each key again with it, to check the
    /// database did not move a crate's key between the serve and the store.
    pub build: ContentId,
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
    /// files ([`refusal`], [`include_refusal`]). A build script's `OUT_DIR` files are in it.
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
        }
    }
}

/// What [`store`] did.
#[derive(Debug, Default)]
pub struct Stored {
    /// The crates whose image was written.
    pub written: Vec<Crate>,
    /// The crates whose image was not, and why.
    pub refused: Vec<(Crate, Refused)>,
    /// The key each crate of `written` has its image filed under ([`Image::key`]): what a
    /// value made from the crate as this database resolved it is filed by
    /// (`blaze_diagnostics::cache`).
    pub filed: HashMap<Crate, ContentId, Global>,
}

/// Reads each of `crates` that is [`eligible`] (and its dependencies) from the image `images`
/// keep for it, by `build`, the blaze reading: `mode` says what is done with each image.
///
/// Adds to what earlier calls served, so a driver serves crates as their inputs become final.
/// A crate served before under the same key and mode keeps its image as it is and is not
/// fetched again. One whose key or mode is another is fetched again, and loses its image when
/// none is found. The frozen def maps and lang items `db` was given for other crates are kept.
/// A write that moves a served crate's files has already taken its image away, in that write
/// (`withdraw_moved`): it is fetched again here like a crate never served.
///
/// An image that does not read under the key it is filed by, or reads that do not decode, are
/// an error naming the crate and why, never a crate resolved instead: the key covers what the
/// image was made from and the store checked the bytes, so such an image is a bug in the key or
/// an encoding.
///
/// [`eligible`]: super::eligible
pub fn serve(
    db: &mut Db,
    crates: &[Crate],
    build: ContentId,
    // dyn: the driver's store, one call per crate per session
    images: &dyn ImageStore,
    mode: Mode,
) -> Result<Served, String> {
    let keys = keys(&*db, crates, build);
    // clone: the handle of the images served before, once per serve
    let before = DepImages::try_get(&*db).map(|it| it.images(&*db).clone()).unwrap_or_default();
    let kept = |krate: &Crate, key: &ContentId| {
        before.get(krate).is_some_and(|it| it.base == *key && it.mode == mode)
    };
    let mut order: Vec<(Crate, ContentId)> = (keys.iter())
        .filter(|(krate, key)| !kept(krate, key))
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
    {
        let db = &*db;
        images.for_each(order.len(), &|at| {
            let (krate, key) = order[at];
            _ = found[at].set(fetch(db, krate, key, images));
        });
    }
    let mut hits: Vec<Crate> =
        // alloc: setup: once per serve
        keys.iter().filter(|(krate, key)| kept(krate, key)).map(|(&krate, _)| krate).collect();
    if order.is_empty() {
        hits.sort_unstable_by_key(|krate| keys[krate]);
        // alloc: none: an empty `Vec` holds no buffer
        return Ok(Served { keys, hits, stale: Vec::new(), build });
    }
    // clone: what was served before, once per serve that fetches a crate, to add to
    let mut served = (*before).clone();
    let mut maps = copied(FrozenDefMaps::try_get(&*db).map(|it| it.maps(&*db)));
    let mut lang_items = copied(FrozenLangItems::try_get(&*db).map(|it| it.maps(&*db)));
    // alloc: setup, as above
    let mut stale = Vec::new();
    // Whether what the database was given changes: an image served, or one served before
    // taken away. A serve that found nothing new (a cold store) writes nothing, so it costs
    // no maintenance pass.
    let mut changed = false;
    for (&(krate, base), found) in order.iter().zip(found) {
        let found = found.into_inner().unwrap_or_else(|| {
            panic!("`ImageStore::for_each` did not run the task of crate {}", krate.named(db))
        });
        changed |= served.remove(&krate).is_some();
        changed |= maps.remove(&krate).is_some();
        changed |= lang_items.remove(&krate).is_some();
        let (bytes, parts, filed) = match found? {
            Found::Hit { bytes, parts, filed } => (bytes, parts, filed),
            Found::Miss { stale: true } => {
                stale.push(krate);
                continue;
            }
            Found::Miss { stale: false } => continue,
        };
        let part = |part| PartBytes::of(&bytes, &parts, part);
        maps.insert(krate, Frozen { bytes: part(Part::DefMap), mode });
        lang_items.insert(krate, Frozen { bytes: part(Part::LangItems), mode });
        served.insert(krate, Image { base, key: filed, bytes, parts, mode });
        hits.push(krate);
        changed = true;
    }
    hits.sort_unstable_by_key(|krate| keys[krate]);
    if !changed {
        return Ok(Served { keys, hits, stale, build });
    }
    frozen::set(db, maps);
    frozen::set_lang_items(db, lang_items);
    set_images(db, served);
    // A served crate's modules are the decoded map's, not the ones its dependents were
    // maintained against: the maintainers run again over the crates now served.
    db.note_crates_changed();
    Ok(Served { keys, hits, stale, build })
}

/// The frozen bytes a database was given, to add to.
fn copied(maps: Option<&Arc<HashMap<Crate, Frozen, Global>>>) -> HashMap<Crate, Frozen, Global> {
    // clone: what was served before, once per serve that fetches a crate, to add to
    maps.map(|it| (**it).clone()).unwrap_or_default()
}

/// What one task of [`serve`] found of a crate.
enum Found {
    /// Its image, where its parts are in it, and the key it is filed under.
    Hit { bytes: ImageBuf, parts: [Range<usize>; Part::ALL.len()], filed: ContentId },
    /// No image; `stale` when reads were kept under its key.
    Miss { stale: bool },
}

type Fetched = Result<Found, String>;

/// The image of `krate`, whose key is `key`: under the key, or under the reads of one of its
/// slots and their values now ([`filed::search`]); opened and its def map's files checked.
// dyn: the driver's store, one call per crate per session
fn fetch(db: &Db, krate: Crate, key: ContentId, images: &dyn ImageStore) -> Fetched {
    let found = filed::search(db, krate, key, images, |at| {
        let bytes = images.get(&at).map_err(|why| {
            // alloc: error text
            format!("reading the image of crate {} (key {at:?}): {why}", krate.named(db))
        })?;
        Ok(bytes.map(|bytes| (at, bytes)))
    })?;
    match found {
        Ok((filed, bytes)) => open(db, krate, filed, bytes),
        Err(stale) => Ok(Found::Miss { stale }),
    }
}

/// The image `bytes`, filed under `filed`, opened and its def map's files checked.
fn open(db: &Db, krate: Crate, filed: ContentId, bytes: ImageBuf) -> Fetched {
    let image = ImageRef::open(bytes.bytes(), filed).map_err(|why| {
        // alloc: error text
        format!("the image of crate {} (key {filed:?}): {why}", krate.named(db))
    })?;
    if let Some(why) = frozen::unreadable(db, krate, image.part(Part::DefMap)) {
        // alloc: error text
        return Err(format!(
            "the image of crate {} (key {filed:?}) names files that are not the crate's: \
             {why}; the key leaves out an input",
            krate.named(db)
        ));
    }
    let parts = image.ranges(bytes.bytes());
    Ok(Found::Hit { bytes, parts, filed })
}

inventory::submit! {
    base_db::Implied { name: "dependency images withdrawn", write: withdraw_moved }
}

/// The write implied by a change to the files of crates read from images. A crate whose own
/// files moved loses its image, frozen def map and lang items in the same write. Its own files
/// moved when a file of its source root was set, added or removed: an edit, a build script's
/// run that left other `OUT_DIR` files, a view moved. So does every crate read from an image
/// that depends on a crate whose files moved, whether that crate is served or not. The
/// maintainers then go over the crate graph again, as after a [`serve`] that takes an image
/// away.
///
/// The key covers every file of the crate's source root and every crate it depends on, so the
/// image no longer names the crate. Kept, it would be read against files it was not made from,
/// which the content check refuses ([`frozen::Unreadable`]). The next [`serve`] fetches it
/// under its new key. A serve in the same batch as a write of its crates' files is withdrawn
/// by it: drivers serve in a batch of its own, after the writes.
///
/// A crate whose cfg or env alone moved keeps its image until that serve. The image still
/// reads, and holds what the crate was before the write.
fn withdraw_moved(db: &mut Db, written: &base_db::Written<'_>) {
    let gone = moved_served(db, written);
    if gone.is_empty() {
        return;
    }
    tracing::info!(target: "blaze", crates = gone.len(), "dependency images withdrawn: their files moved");
    withdraw(db, &gone);
}

/// The crates given an image or frozen bytes that `written` moved the files of, or of a crate
/// they depend on: what [`withdraw_moved`] takes away.
fn moved_served(db: &Db, written: &base_db::Written<'_>) -> Vec<Crate> {
    if written.set.is_empty()
        && written.added.is_empty()
        && written.removed.is_empty()
        && written.roots.is_empty()
    {
        // alloc: none, an empty list
        return Vec::new();
    }
    let served = served_crates(db);
    if served.is_empty() {
        return served;
    }
    let moved = moved_crates(db, written);
    if moved.is_empty() {
        // alloc: none, an empty list
        return Vec::new();
    }
    // alloc: one entry per crate walked, once per write that moved a served crate's files
    let mut reaches = HashMap::new_in(Global);
    // alloc: as above
    served.into_iter().filter(|&krate| reaches_moved(db, krate, &moved, &mut reaches)).collect()
}

/// Every crate given an image, a frozen def map or frozen lang items, sorted.
fn served_crates(db: &Db) -> Vec<Crate> {
    // alloc: once per write that moved a file while any crate is served
    let mut crates = Vec::new();
    if let Some(it) = DepImages::try_get(db) {
        crates.extend(it.images(db).keys().copied());
    }
    if let Some(it) = FrozenDefMaps::try_get(db) {
        crates.extend(it.maps(db).keys().copied());
    }
    if let Some(it) = FrozenLangItems::try_get(db) {
        crates.extend(it.maps(db).keys().copied());
    }
    crates.sort_unstable();
    crates.dedup();
    crates
}

/// The crates whose root is in a source root `written` moved: a file of it set or added, or
/// its file set changed (which covers a file removed).
fn moved_crates(db: &Db, written: &base_db::Written<'_>) -> HashSet<Crate, Global> {
    // alloc: once per write that moved a file while any crate is served
    let mut roots: Vec<base_db::SourceRootId> = written.roots.to_vec();
    for &file in written.set.iter().chain(written.added) {
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

/// Takes the image, frozen def map and frozen lang items of each of `gone` away, and has the
/// maintainers go over the crate graph again: from here on each resolves as a fresh crate.
fn withdraw(db: &mut Db, gone: &[Crate]) {
    // clone: what was served, once per write that withdraws an image
    let mut images =
        DepImages::try_get(&*db).map(|it| (**it.images(&*db)).clone()).unwrap_or_default();
    let mut maps = copied(FrozenDefMaps::try_get(&*db).map(|it| it.maps(&*db)));
    let mut lang_items = copied(FrozenLangItems::try_get(&*db).map(|it| it.maps(&*db)));
    for krate in gone {
        images.remove(krate);
        maps.remove(krate);
        lang_items.remove(krate);
    }
    frozen::set(db, maps);
    frozen::set_lang_items(db, lang_items);
    set_images(db, images);
    db.note_crates_changed();
}

fn set_images(db: &mut Db, images: HashMap<Crate, Image, Global>) {
    use blaze_query::db::{Durability, Setter};
    // alloc: setup: the one input value, once per session
    let images = Arc::new(images);
    match DepImages::try_get(&*db) {
        Some(it) => {
            it.set_images(db).with_durability(Durability::HIGH).to(images);
        }
        None => {
            _ = DepImages::new(db, Durability::HIGH, images);
        }
    }
}

/// Writes to `images` the image of each crate [`serve`] found none of, resolving it in `db`,
/// one task a crate ([`ImageStore::for_each`]). A crate whose image would not cover every
/// input is refused, with why. An image the store will not keep is an error naming the crate.
// dyn: the driver's store, one call per crate per session
pub fn store(db: &Db, served: &Served, images: &dyn ImageStore) -> Result<Stored, String> {
    // alloc: setup: once per session
    let hits: HashSet<Crate, Global> = served.hits.iter().copied().collect();
    // alloc: setup: once per session
    let mut order: Vec<(Crate, ContentId)> = (served.keys.iter())
        .filter(|(krate, _)| !hits.contains(*krate))
        .map(|(&krate, &key)| (krate, key))
        // alloc: setup: once per session
        .collect();
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
        match made? {
            Ok(filed) => {
                stored.written.push(krate);
                stored.filed.insert(krate, filed);
            }
            Err(why) => stored.refused.push((krate, why)),
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
    if let Some(why) = refusal(db, krate) {
        return Ok(Err(why));
    }
    // alloc: setup: the macro calls the crate's signatures name, once per crate imaged
    let mut calls = Vec::new();
    let parts = match encode(db, krate, &mut calls) {
        Ok(parts) => parts,
        Err(why) => return Ok(Err(why)),
    };
    if let Some(why) = include_refusal(db, krate, &calls) {
        return Ok(Err(why));
    }
    if let Some(why) = Base::of(db, krate).held_by(&parts) {
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
    let image = write_image(filed, [&parts[0], &parts[1], &parts[2]]);
    drop(parts);
    images.put(filed, image).map_err(|why| {
        // alloc: error text
        format!("keeping the image of crate {} (key {filed:?}): {why}", krate.named(db))
    })?;
    Ok(Ok(filed))
}

/// The parts of the image of `krate`, in the order of [`Part::ALL`], and in `calls` the macro
/// calls its signatures name.
fn encode(db: &Db, krate: Crate, calls: &mut Vec<MacroCallId>) -> Result<[Vec<u8>; 3], Refused> {
    let refused = |part| move |why| Refused::Encode { part, why };
    Ok([
        frozen::encode(db, krate).map_err(refused(Part::DefMap))?,
        frozen::encode_lang_items(db, krate).map_err(refused(Part::LangItems))?,
        signatures::encode(db, krate, calls).map_err(refused(Part::Signatures))?,
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
/// ([`include_refusal`]).
pub(super) fn refusal(db: &Db, krate: Crate) -> Option<Refused> {
    let def_map = crate_def_map(db, krate);
    if def_map.session_macros {
        return Some(Refused::SessionMacros);
    }
    let root = db.file_source_root(krate.data(db).root_file_id).source_root_id(db);
    for (id, module) in def_map.modules(db) {
        if let Some(file) = module.origin.file_id() {
            let file = file.file_id(db);
            if db.file_source_root(file).source_root_id(db) != root {
                // alloc: why a crate is refused, once per refused crate
                let what = format!(
                    "module `{}` of crate {} is the file {}",
                    module_path(db, id),
                    krate.named(db),
                    shown_path(db, file),
                );
                return Some(Refused::OutsideSourceRoot { what });
            }
        }
    }
    let missing = def_map.diagnostics(db).iter().find_map(|it| match &it.kind {
        DefDiagnosticKind::UnresolvedModule { ast, candidates } => {
            Some((it.in_module, ast, candidates))
        }
        _ => None,
    });
    if let Some((parent, ast, candidates)) = missing {
        let name = match ast.to_node(db).name() {
            Some(name) => name.text().to_string(), // alloc: error text
            None => "_".to_owned(),                // alloc: error text
        };
        let declared =
            InFile::new(ast.file_id, ast.to_range(db)).original_node_file_range_rooted(db);
        let anchor = ast.file_id.original_file_respecting_includes(db).file_id(db);
        // alloc: why a crate is refused, once per refused crate
        let what = format!(
            "module `{}::{name}` of crate {}, declared at {}, has no file: none of {} (from the \
             directory of {}) is there",
            module_path(db, parent),
            krate.named(db),
            site(db, declared),
            candidates.join(", "),
            shown_path(db, anchor),
        );
        return Some(Refused::ModuleFileMissing { what });
    }
    None
}

/// Why the image of `krate` would leave out a file an `include!`, `include_str!` or
/// `include_bytes!` it depends on reads, if it would: one of `calls` (those its signatures
/// name) or of its modules' scopes, or one any of them is expanded in, whose file is outside
/// the crate's source root or not known ([`refusal`]). The calls walked are those its reads
/// are worked out from ([`super::reads::image_calls`]).
pub(super) fn include_refusal(db: &Db, krate: Crate, calls: &[MacroCallId]) -> Option<Refused> {
    let root = db.file_source_root(krate.data(db).root_file_id).source_root_id(db);
    let todo = super::reads::image_calls(db, krate, calls);
    let refused = super::reads::each_call(db, krate, todo, |call, loc| {
        let MacroDefKind::BuiltInEager(_, expander) = &loc.def.kind else { return Ok(()) };
        if !expander.is_include_like() {
            return Ok(());
        }
        // The argument of an eager macro, expanded as a call of its own (`eager: None`): the
        // parent of the calls in it (`concat!`, `env!`). The include it is the argument of is a
        // call of the scope or store too, and is asked there.
        let MacroCallKind::FnLike { eager: Some(eager), .. } = &loc.kind else { return Ok(()) };
        let read = match include_input_to_file_id(db, call, &eager.arg) {
            Ok(file) => {
                let file = file.file_id(db);
                if db.file_source_root(file).source_root_id(db) == root {
                    return Ok(());
                }
                // alloc: why a crate is refused, once per refused crate
                format!("reads {}", shown_path(db, file))
            }
            // alloc: as above
            Err(why) => format!(
                "names a file no source root has ({})",
                why.kind().render_to_string(db).message
            ),
        };
        let name = match expander {
            hir_expand::builtin::EagerExpander::Include => "include!",
            hir_expand::builtin::EagerExpander::IncludeStr => "include_str!",
            _ => "include_bytes!",
        };
        // alloc: as above
        let what =
            format!("`{name}` of crate {} at {} {read}", krate.named(db), call_site(db, loc));
        Err(Refused::OutsideSourceRoot { what })
    });
    refused.err()
}

/// The path the source root has `file` at, as an error shows it.
fn shown_path(db: &Db, file: base_db::FileId) -> String {
    match db.path_for_file(file) {
        Some(path) => path.to_string(),                 // alloc: error text
        None => format!("{file:?}, which has no path"), // alloc: error text
    }
}

/// Where the call `loc` is in a file of its crate: the file's path and the line, from 1.
fn call_site(db: &Db, loc: &hir_expand::MacroCallLoc) -> String {
    site(db, loc.kind.original_call_range_with_input(db))
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

/// The path of `module` in its crate, as `crate::a::b`.
fn module_path(db: &Db, module: crate::ModuleId) -> String {
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
