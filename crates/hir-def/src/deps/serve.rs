//! Giving a database the images a store has ([`serve`]) and writing those it lacks
//! ([`store`]).
//!
//! The store is the driver's ([`ImageStore`]): the local store, or the local store over the
//! global cache ([`ReadThrough`]), each checking a value's content hash on every read and
//! giving it in place, so the front end depends on neither.
//!
//! A crate's image is under its key ([`keys`]) when it has no [`Reads`]; else under its key,
//! the names it read (a slot's bytes) and their values ([`filed`]).

use std::{
    ops::Range,
    sync::{Arc, OnceLock},
};

use base_db::{Crate, Db, Env};
use blaze_hash::{HashMap, HashSet};
use blaze_ids::ContentId;
use hir_expand::{MacroCallId, MacroCallKind, MacroDefKind, builtin::include_input_to_file_id};

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
    /// ran: `proc_macro_api::kept::reads_now`. A variable is the calling crate's (`env`, what the
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
    pub keys: HashMap<Crate, ContentId>,
    /// The crates read from an image, in the order of their keys.
    pub hits: Vec<Crate>,
    /// The crates with reads kept under their key but no image for the values those reads
    /// have now: something they read changed, so they are resolved and imaged again.
    pub stale: Vec<Crate>,
}

/// Why [`store`] wrote no image of a crate. Each is an input the key and the reads do not
/// cover, so the image would be served where the crate resolves otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// Its def map depends on the macros of the session (`#[macro_export]` from a proc macro
    /// crate of the session): what they expand to is not in the key.
    SessionMacros,
    /// A procedural macro's expansion of it whose reads are not known, and why.
    ProcMacro { call: String, why: &'static str },
    /// A module or an `include!` of it is a file outside the crate's source root, whose text
    /// the key does not cover (`OUT_DIR`, a `#[path]` out of the crate).
    OutsideSourceRoot { what: String },
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
                write!(f, "{what} is outside the crate's source root")
            }
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
    pub filed: HashMap<Crate, ContentId>,
}

/// The name of `krate` in an error.
pub fn crate_name(db: &Db, krate: Crate) -> String {
    match &krate.extra_data(db).display_name {
        // alloc: error text
        Some(name) => name.to_string(),
        // alloc: error text
        None => format!("of root file {:?}", krate.data(db).root_file_id),
    }
}

/// Reads each of `crates` that is [`eligible`] (and its dependencies) from the image `images`
/// keep for it, by `build`, the blaze reading: `mode` says what is done with each image.
///
/// Adds to what earlier calls served, so a driver serves crates as their inputs become final.
/// A crate served before under the same key and mode keeps its image as it is and is not
/// fetched again. One whose key or mode is another is fetched again, and loses its image when
/// none is found. The frozen def maps and lang items `db` was given for other crates are kept.
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
    let found: Vec<OnceLock<Fetched>> = order.iter().map(|_| OnceLock::new()).collect();
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
        return Ok(Served { keys, hits, stale: Vec::new() });
    }
    // clone: what was served before, once per serve that fetches a crate, to add to
    let mut served = (*before).clone();
    let mut maps = copied(FrozenDefMaps::try_get(&*db).map(|it| it.maps(&*db)));
    let mut lang_items = copied(FrozenLangItems::try_get(&*db).map(|it| it.maps(&*db)));
    // alloc: setup, as above
    let mut stale = Vec::new();
    for (&(krate, base), found) in order.iter().zip(found) {
        let found = found.into_inner().unwrap_or_else(|| {
            panic!("`ImageStore::for_each` did not run the task of crate {}", crate_name(db, krate))
        });
        served.remove(&krate);
        maps.remove(&krate);
        lang_items.remove(&krate);
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
    }
    hits.sort_unstable_by_key(|krate| keys[krate]);
    frozen::set(db, maps);
    frozen::set_lang_items(db, lang_items);
    set_images(db, served);
    // A served crate's modules are the decoded map's, not the ones its dependents were
    // maintained against: the maintainers run again over the crates now served.
    db.note_crates_changed();
    Ok(Served { keys, hits, stale })
}

/// The frozen bytes a database was given, to add to.
fn copied(maps: Option<&Arc<HashMap<Crate, Frozen>>>) -> HashMap<Crate, Frozen> {
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
            format!("reading the image of crate {} (key {at:?}): {why}", crate_name(db, krate))
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
        format!("the image of crate {} (key {filed:?}): {why}", crate_name(db, krate))
    })?;
    if let Some(why) = frozen::unreadable(db, krate, image.part(Part::DefMap)) {
        // alloc: error text
        return Err(format!(
            "the image of crate {} (key {filed:?}) names files that are not the crate's: \
             {why}; the key leaves out an input",
            crate_name(db, krate)
        ));
    }
    let parts = image.ranges(bytes.bytes());
    Ok(Found::Hit { bytes, parts, filed })
}

fn set_images(db: &mut Db, images: HashMap<Crate, Image>) {
    use blaze_query::db::{Durability, Setter};
    // alloc: setup: the one input value, once per session
    let images = Arc::new(images);
    match DepImages::try_get(&*db) {
        Some(it) => {
            it.set_images(db).with_durability(Durability::HIGH).to(images);
        }
        None => {
            _ = DepImages::builder(images).durability(Durability::HIGH).new(db);
        }
    }
}

/// Writes to `images` the image of each crate [`serve`] found none of, resolving it in `db`,
/// one task a crate ([`ImageStore::for_each`]). A crate whose image would not cover every
/// input is refused, with why. An image the store will not keep is an error naming the crate.
// dyn: the driver's store, one call per crate per session
pub fn store(db: &Db, served: &Served, images: &dyn ImageStore) -> Result<Stored, String> {
    // alloc: setup: once per session
    let hits: HashSet<Crate> = served.hits.iter().copied().collect();
    // alloc: setup: once per session
    let mut order: Vec<(Crate, ContentId)> = (served.keys.iter())
        .filter(|(krate, _)| !hits.contains(krate))
        .map(|(&krate, &key)| (krate, key))
        // alloc: setup: once per session
        .collect();
    order.sort_unstable_by_key(|&(_, key)| key);
    let made: Vec<OnceLock<Result<Result<ContentId, Refused>, String>>> =
        // alloc: setup: one slot per crate, once per session
        order.iter().map(|_| OnceLock::new()).collect();
    images.for_each(order.len(), &|at| {
        let (krate, key) = order[at];
        _ = made[at].set(make(db, krate, key, images));
    });
    let mut stored = Stored::default();
    for (&(krate, _), made) in order.iter().zip(made) {
        let made = made.into_inner().unwrap_or_else(|| {
            panic!("`ImageStore::for_each` did not run the task of crate {}", crate_name(db, krate))
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
        format!("keeping the image of crate {} (key {filed:?}): {why}", crate_name(db, krate))
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

/// Why the image of `krate` would leave out an input its reads do not cover, if it would.
pub(super) fn refusal(db: &Db, krate: Crate) -> Option<Refused> {
    let def_map = crate_def_map(db, krate);
    if def_map.session_macros {
        return Some(Refused::SessionMacros);
    }
    let root = db.file_source_root(krate.data(db).root_file_id).source_root_id(db);
    for (_, module) in def_map.modules(db) {
        if let Some(file) = module.origin.file_id() {
            let file = file.file_id(db);
            if db.file_source_root(file).source_root_id(db) != root {
                // alloc: why a crate is refused, once per refused crate
                return Some(Refused::OutsideSourceRoot { what: format!("module file {file:?}") });
            }
        }
        for call in module.scope(db).all_macro_calls() {
            let loc = call.lookup(db);
            if let MacroDefKind::BuiltInEager(_, expander) = &loc.def.kind
                && expander.is_include_like()
                && include_outside(db, call, root)
            {
                // alloc: why a crate is refused, once per refused crate
                return Some(Refused::OutsideSourceRoot { what: format!("include {call:?}") });
            }
        }
    }
    None
}

/// Whether the file the eager `include!`, `include_str!` or `include_bytes!` `call` reads is
/// outside the source root `root`, or not known: its eager argument, expanded, is the path.
fn include_outside(db: &Db, call: MacroCallId, root: base_db::SourceRootId) -> bool {
    let MacroCallKind::FnLike { eager: Some(eager), .. } = &call.lookup(db).kind else {
        return true;
    };
    match include_input_to_file_id(db, call, &eager.arg) {
        Ok(file) => db.file_source_root(file.file_id(db)).source_root_id(db) != root,
        Err(_) => true,
    }
}
