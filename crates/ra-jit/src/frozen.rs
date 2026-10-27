//! Frozen dependency artifacts (`docs/glancer.md`, item 1; #40): the def map, the impls and
//! the lang items of each crate whose source cannot change, written to the store once and read
//! back instead of resolving and finding them again.
//!
//! A crate is frozen when its source is the toolchain's (the standard library) or a
//! registry's or git checkout's, and every crate it depends on is frozen too. Its key is a
//! Merkle hash: where the source is below the toolchain, registry or checkout, its edition,
//! kind, crate attributes, cfg, environment, and the names and keys of its dependencies; the store adds the toolchain
//! and ra-jit's own build. `OUT_DIR` counts by its place below the target directory and by what
//! the build script wrote there, so the same crate in another checkout has the same key.
//!
//! `RA_JIT_FROZEN` says what is done with what the store has: by default it is served;
//! `check` resolves and finds as well and fails the build on any difference, `verify` writes
//! what was read and fails on any difference, `off` neither reads nor writes.

use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering::Relaxed},
};

use ide_db::base_db::Crate;
use rustc_hash::FxHashMap;
use sha2::{Digest, Sha256};

use hir_def::nameres::frozen::{self as hir_frozen, Frozen, Mode};
use hir_ty::method_resolution::frozen as impls_frozen;

/// What a build did with what is frozen of the crates it resolved.
#[derive(Default)]
pub(crate) struct Served {
    /// The key of each crate that can be frozen.
    keys: FxHashMap<Crate, [u8; 32]>,
    /// The crates each [`Part`] of which came from the store, in the order of [`Part::ALL`].
    served: [Vec<Crate>; 3],
    /// The parts the store has but which name a file that is not loaded.
    unreadable: usize,
    mode: Option<Mode>,
}

/// The crates whose source is not all the toolchain's, registry's or checkout's: a file of
/// each has a text of blaze's (`--run-tool`'s std), which its key has.
static OVERLAID: Mutex<Vec<(Crate, &'static str)>> = Mutex::new(Vec::new());

/// Says that a file of `krate` has `text` in place of the one where its source is.
pub(crate) fn overlaid(krate: Crate, text: &'static str) {
    crate::lock(&OVERLAID).push((krate, text));
}

fn mode() -> Option<Mode> {
    match std::env::var("RA_JIT_FROZEN").as_deref() {
        Ok("off" | "0") => None,
        Ok("check") => Some(Mode::Check),
        Ok("verify") => Some(Mode::Verify),
        _ => Some(Mode::Serve),
    }
}

/// Where a crate's source is when it is one that cannot change: the part of the path from
/// the toolchain's source, the registry's or a git checkout's on.
fn immutable(path: &str) -> Option<&str> {
    ["/lib/rustlib/src/rust/", "/registry/src/", "/git/checkouts/"]
        .iter()
        .find_map(|marker| path.find(marker).map(|at| &path[at + marker.len()..]))
}

/// What stands for the directory a build script wrote to in a frozen crate's key: the part of
/// it from cargo's `build` directory on, whose `<package>-<hash>` cargo names by the package,
/// its features, profile and target, and not by where the checkout is.
fn build_output(out: &str) -> String {
    let path = out.replace('\\', "/");
    match path.rfind("/build/") {
        Some(at) => format!("<target>{}", &path[at..]),
        None => "<out>".to_owned(),
    }
}

/// The key of `krate`, when it and every crate it depends on cannot change.
fn key(
    db: &ide_db::RootDatabase,
    krate: Crate,
    keys: &mut FxHashMap<Crate, Option<[u8; 32]>>,
) -> Option<[u8; 32]> {
    if let Some(&it) = keys.get(&krate) {
        return it;
    }
    // A cycle, which only dev-dependencies make, is not frozen.
    keys.insert(krate, None);
    let found = key_of(db, krate, keys);
    keys.insert(krate, found);
    found
}

fn key_of(
    db: &ide_db::RootDatabase,
    krate: Crate,
    keys: &mut FxHashMap<Crate, Option<[u8; 32]>>,
) -> Option<[u8; 32]> {
    let root = crate::crate_root_file(db, krate.into())?;
    let root = root.to_str()?;
    let source = immutable(root)?;
    // The directory the crate's paths are under, which its environment names.
    let package = &root[..root.len() - source.len()];
    let data = krate.data(db);
    let mut hasher = Sha256::new();
    let mut field = |bytes: &[u8]| {
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    };
    field(b"ra-jit frozen def map 6");
    field(source.as_bytes());
    field(format!("{:?} {:?} {:?}", data.edition, data.target_kind, data.crate_attrs).as_bytes());
    let mut cfgs: Vec<String> =
        krate.cfg_options(db).into_iter().map(|it| it.to_string()).collect();
    cfgs.sort_unstable();
    field(cfgs.join("\n").as_bytes());
    for (_, text) in crate::lock(&OVERLAID).iter().filter(|it| it.0 == krate) {
        field(b"overlaid");
        field(text.as_bytes());
    }
    // A build script writes below the target directory of the checkout that ran it, so what it
    // wrote stands for where: two checkouts share the crate when its script wrote the same.
    let env = krate.env(db);
    let out = env.get("OUT_DIR");
    let out_stands_for = out.as_deref().map(build_output);
    // `CARGO` is wherever the cargo found on `PATH` is, which differs between a build script's
    // compile and the daemon's, and no crate's names are resolved by it.
    let mut vars: Vec<String> = env
        .iter()
        .filter(|(name, _)| *name != "CARGO")
        .map(|(name, value)| {
            let value = match (&out, &out_stands_for) {
                (Some(out), Some(stands_for)) => value.replace(out.as_str(), stands_for),
                _ => value.to_owned(),
            };
            format!("{name}={}", value.replace(package, "<source>/"))
        })
        .collect();
    vars.sort_unstable();
    field(vars.join("\n").as_bytes());
    if let Some(out) = &out {
        for (path, hash) in crate::content::written_to(db, krate, out) {
            field(path.as_bytes());
            field(&hash);
        }
    }
    let deps: Vec<_> =
        data.dependencies.iter().map(|it| (it.crate_id, it.name.to_string())).collect();
    for (dep, name) in deps {
        let dep_key = key(db, dep, keys)?;
        field(name.as_bytes());
        field(&dep_key);
    }
    Some(hasher.finalize().into())
}

/// What is frozen of a crate, each under its own key: the def map's is the crate's key.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Part {
    DefMap,
    Impls,
    LangItems,
}

impl Part {
    const ALL: [Part; 3] = [Part::DefMap, Part::Impls, Part::LangItems];

    fn name(self) -> &'static str {
        match self {
            Part::DefMap => "def maps",
            Part::Impls => "impls",
            Part::LangItems => "lang items",
        }
    }

    /// The key of this part of the crate whose key is `key`.
    fn key(self, key: &[u8; 32]) -> [u8; 32] {
        let tag: &[u8] = match self {
            Part::DefMap => return *key,
            Part::Impls => b"ra-jit frozen impls 1",
            Part::LangItems => b"ra-jit frozen lang items 1",
        };
        let mut hasher = Sha256::new();
        hasher.update(tag);
        hasher.update(key);
        hasher.finalize().into()
    }

    /// Why `bytes` cannot be read in this database: a file they name is not loaded.
    fn unreadable(self, db: &ide_db::RootDatabase, krate: Crate, bytes: &[u8]) -> Option<String> {
        if self != Part::Impls {
            return hir_frozen::unreadable(db, krate, bytes);
        }
        // The impls are two parts, each naming its files.
        let len = bytes.get(..4).and_then(|it| <[u8; 4]>::try_from(it).ok());
        let Some(len) = len else { return Some("no length".to_owned()) };
        let first = 4 + u32::from_le_bytes(len) as usize;
        let (Some(traits), Some(inherent)) = (bytes.get(4..first), bytes.get(first..)) else {
            return Some("a part past the end".to_owned());
        };
        [traits, inherent].into_iter().find_map(|part| hir_frozen::unreadable(db, krate, part))
    }

    fn set(self, db: &mut ide_db::RootDatabase, maps: FxHashMap<Crate, Frozen>) {
        match self {
            Part::DefMap => hir_frozen::set(db, maps),
            Part::Impls => impls_frozen::set(db, maps),
            Part::LangItems => hir_frozen::set_lang_items(db, maps),
        }
    }

    fn encode(self, db: &ide_db::RootDatabase, krate: Crate) -> Result<Vec<u8>, String> {
        hir::attach_db(db, || match self {
            Part::DefMap => hir_frozen::encode(db, krate),
            Part::Impls => impls_frozen::encode(db, krate),
            Part::LangItems => hir_frozen::encode_lang_items(db, krate),
        })
    }

    /// Records a difference between what was read and what is live; the def map's is
    /// checked as it is resolved.
    fn check(self, db: &ide_db::RootDatabase, krate: Crate) {
        hir::attach_db(db, || match self {
            Part::DefMap => {}
            Part::Impls => impls_frozen::check(db, krate),
            Part::LangItems => hir_frozen::check_lang_items(db, krate),
        })
    }
}

/// Has name resolution, finding impls and finding lang items read what the store has of
/// `crates` and their dependencies.
pub(crate) fn serve(db: &mut ide_db::RootDatabase, crates: &[Crate]) -> Served {
    let Some(mode) = mode() else { return Served::default() };
    let mut all: FxHashMap<Crate, Option<[u8; 32]>> = FxHashMap::default();
    for &krate in crates {
        for dep in krate.transitive_deps(db) {
            key(db, dep, &mut all);
        }
    }
    let keys: FxHashMap<Crate, [u8; 32]> =
        all.into_iter().filter_map(|(krate, key)| Some((krate, key?))).collect();
    let mut served = Served { keys, mode: Some(mode), ..Served::default() };
    let Some(store) = ra_jit_store::Store::global() else { return served };
    for (index, part) in Part::ALL.into_iter().enumerate() {
        let mut maps = FxHashMap::default();
        for (&krate, key) in &served.keys {
            // A part is read only with the def map it was found from.
            if part != Part::DefMap && !served.served[0].contains(&krate) {
                continue;
            }
            let Some(bytes) = store.get(&part.key(key)) else { continue };
            if part.unreadable(db, krate, &bytes).is_some() {
                served.unreadable += 1;
                continue;
            }
            maps.insert(krate, Frozen { bytes: bytes.into(), mode });
            served.served[index].push(krate);
        }
        part.set(db, maps);
    }
    served
}

/// Writes to the store what can be frozen of the crates that were resolved, and fails on a
/// difference between what was frozen and what is live.
pub(crate) fn store(db: &ide_db::RootDatabase, served: &Served) -> anyhow::Result<()> {
    if served.mode != Some(Mode::Serve) {
        for (index, part) in Part::ALL.into_iter().enumerate() {
            served.served[index].iter().for_each(|&krate| part.check(db, krate));
        }
    }
    let differences = hir_frozen::differences();
    if !differences.is_empty() {
        anyhow::bail!("what was frozen differs:\n{}", differences.join("\n"));
    }
    let Some(store) = served.mode.and(ra_jit_store::Store::global()) else { return Ok(()) };
    let start = std::time::Instant::now();
    let mut unstored = Vec::new();
    for (index, part) in Part::ALL.into_iter().enumerate() {
        for (&krate, key) in &served.keys {
            let key = part.key(key);
            if !served.served[index].contains(&krate) && !store.has(&key) {
                unstored.push((part, krate, key));
            }
        }
    }
    if !unstored.is_empty() {
        store.settled();
    }
    // On every core: a cold build has a thousand to write, and waits for them.
    let (stored, refused) = (AtomicUsize::new(0), Mutex::new(FxHashMap::default()));
    let next = AtomicUsize::new(0);
    let threads = std::thread::available_parallelism().map_or(1, |it| it.get());
    std::thread::scope(|scope| {
        for _ in 0..threads.min(unstored.len()) {
            let (db, unstored, next) = (db.clone(), &unstored, &next);
            let (stored, refused) = (&stored, &refused);
            scope.spawn(move || {
                while let Some(&(part, krate, key)) = unstored.get(next.fetch_add(1, Relaxed)) {
                    match part.encode(&db, krate) {
                        Ok(bytes) => {
                            store.insert(&key, &bytes);
                            stored.fetch_add(1, Relaxed);
                        }
                        Err(why) => {
                            let mut refused = refused.lock().unwrap_or_else(|e| e.into_inner());
                            *refused.entry(format!("{} with {why}", part.name())).or_default() += 1;
                        }
                    }
                }
            });
        }
    });
    let (stored, refused): (usize, FxHashMap<String, usize>) =
        (stored.into_inner(), refused.into_inner().unwrap_or_else(|e| e.into_inner()));
    if stored > 0 {
        store.flush();
    }
    let from: Vec<String> = Part::ALL
        .iter()
        .zip(&served.served)
        .map(|(part, crates)| format!("{} of {}", part.name(), crates.len()))
        .collect();
    let mut said = format!(
        "frozen, of {} crates: {} from the store, {stored} written in {:.1?}",
        served.keys.len(),
        from.join(", "),
        start.elapsed()
    );
    if served.unreadable > 0 {
        said += &format!(", {} naming a file not loaded", served.unreadable);
    }
    if !refused.is_empty() {
        let mut refused: Vec<_> = refused.into_iter().collect();
        refused.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let why: Vec<String> = refused.iter().map(|(why, n)| format!("{n} {why}")).collect();
        said += &format!(", not frozen: {}", why.join(", "));
    }
    eprintln!("{said}");
    Ok(())
}
