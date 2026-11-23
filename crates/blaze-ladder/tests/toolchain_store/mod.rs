//! The std world, made ONCE per machine and read by every test binary: the toolchain's crate
//! records in a plain store under the cargo target dir, keyed by everything they are a
//! function of (lane TS1, wave 2311; the fixture-cost job, wave 1119).
//!
//! WHY: a test that calls `Toolchain::open` + `Inputs::new` + `inputs.resolve` solves every
//! toolchain crate (core, alloc, std, bstd and the ~60 crates under them) from source,
//! serially: `Inputs::resolve` (src/crates.rs) is `resolve_crates(.., &Serial, ..)`
//! (blaze-scope resolve.rs `resolve_crates_with`), whose `for wave in graph.waves()` loop runs
//! `resolve_one` over each crate, which parses and solves every file of it. That is the ~15 s
//! every test binary paid, per `#[test]` (each one solves again; threads hide it in one binary).
//!
//! THE FLOOR (rule 67): the toolchain's records are a pure function of the pinned shelf, the
//! code that makes them and the crate inputs this harness builds, so they are made once per
//! machine. A test that finds them pays: the shelf's manifest and packs mapped and verified,
//! one walk of the `names` producer row per process, the store opened, three sections per
//! toolchain crate read in place, and its OWN crate solved. The floor is that program crate's
//! solve plus the reads; today's cost is the whole toolchain's solve (~15 s). The guard
//! (tests/fixture_cost.rs) prints the construction's time beside the program solve's and their
//! ratio (target: within 1.1 of the floor, and well under a second in all).
//!
//! WHERE (read in ONE place, [`store_base`]): `$BLAZE_TEST_STORE` when set, else
//! `$CARGO_TARGET_TMPDIR/blaze-test-store`; one plain store (blaze-cas `Driven` under a `Lent`,
//! the command's store code) per world key, in `<base>/<world key hex>`. Never the user's home
//! store (`~/.blaze/v2/store`): tests do not write the home. The shelf is read from the home
//! ([`shelf_dir`]), read-only. A key no longer made leaves its directory: removing the base
//! removes them all (the target dir's own lifetime).
//!
//! THE KEY ([`world_key`], rule 21): the shelf id ([`PINNED`]), the `names` producer row
//! (blaze-producers' digest of the source closure of the crates naming `names`: blaze-scope,
//! whose `resolve_by_records`, `names_record::encode` and `items_record::encode_with` make
//! every byte stored here, and blaze-world, blaze-drive; D105), and every field of every
//! toolchain `CrateInput` this harness solves (name, key, edition, implicit, root, package
//! directory, features, dependencies, cfg: what src/crates.rs derives from the shelf, its
//! `TRIPLE` and its `PROFILE`). The program crate is not in it: the toolchain's records do not
//! read it (they are solved without it, [`toolchain_only`]). Each section's key adds the
//! crate's key and the section's name; an expansion's adds the call's key. A `done` key, put
//! in the same commit, says every crate of the world was written.
//!
//! THE RACE: two processes (or two threads) that find no `done` both solve and both put. Each
//! record is a function of its key's inputs (rule 25), so the second put meets equal bytes
//! (`Put::Same`); other bytes are the store's typed `TwoValues` refusal (rule 21). Within one
//! process the make is behind [`MADE`], so threads of one test binary solve it once.
//!
//! WHAT A TEST GETS ([`front_over_records`]): front2 over `Records` whose loader serves the
//! program's own records from this call and the toolchain's from the store, the program's
//! `CrateNames` (for questions about crate 0 by name), the lang crates (for
//! `ProjectWorld::from_records`), whether this call made or read the world, and how many
//! toolchain source files the construction read (0 when it read the world).

#![allow(dead_code, reason = "a shared test module: each test binary that includes it uses a part")]

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use blaze_cas::Key;
use blaze_cas::plain::{Driven, Lent, LentError, Namespace, Settings, Tag};
use blaze_id::{CrateItemKey, ItemKey};
use blaze_ids::producer::Row;
use blaze_ids::{ContentId, Hasher};
use blaze_ladder::crates::{CratesError, DirShelf, Inputs, Texts, Toolchain, UserCrate};
use blaze_ladder::front2::source::{Load, LoadedExpansion, Log, Records};
use blaze_ladder::front2::{Front2, FrontError};
use blaze_mem::{Frame, MemError};
use blaze_own::Alloc;
use blaze_scope::crate_scope::{CrateScopeError, Ready, resolve_by_records, resolve_crate};
use blaze_scope::expansion_origins::OriginsError;
use blaze_scope::graph::{CrateId, Sysroot};
use blaze_scope::items_record::{self, ItemsRecordError};
use blaze_scope::names_record::{self, NamesRecordError};
use blaze_scope::package_path::PackagePath;
use blaze_scope::prepares::Prepares;
use blaze_scope::resolve::{CrateInput, CrateNames, Names, NamesError, SourceTexts, aligned};
use blaze_scope::task::Serial;
use blaze_toolchain::{PINNED, ToolchainId};

/// The domain every key of the test store is hashed under; a change of what a section holds
/// that neither its record's form nor the `names` row says moves this name.
const KEY_DOMAIN: &str = "blaze-ladder test store: the toolchain's world 2";

/// The environment variable that places the test store: read in [`store_base`] and nowhere
/// else.
pub const STORE_VAR: &str = "BLAZE_TEST_STORE";

/// The tag the test store files its records under: its own, apart from the command's
/// (blaze-keyed `ReservedTag::Drive`), the store's root and the startup image.
const TEST_TAG: Tag = Tag(1);

/// How long the by-records solve waits for one crate's dependencies before it stops by name.
const SOLVE_WAIT: Duration = Duration::from_secs(600);

/// The world key this process has seen made (written by it or found with its `done`): the
/// threads of one test binary make a world once, and a later construction skips the check.
static MADE: Mutex<Option<ContentId>> = Mutex::new(None);

/// The `names` producer row, walked once per process ([`names_row`]).
static NAMES_ROW: Mutex<Option<[u8; 32]>> = Mutex::new(None);

/// The toolchain shelf this harness reads: `$HOME/.blaze/v2/toolchain`, the installed one,
/// read-only. The one place a test's shelf is named.
pub fn shelf_dir() -> String {
    let home = std::env::var("HOME").expect("HOME is set");
    format!("{home}/.blaze/v2/toolchain")
}

/// Where the test stores are: `$BLAZE_TEST_STORE`, else `$CARGO_TARGET_TMPDIR/blaze-test-store`.
/// The one read of [`STORE_VAR`].
pub fn store_base() -> Result<PathBuf, TestStoreError> {
    match std::env::var_os(STORE_VAR) {
        Some(place) if place.is_empty() => Err(TestStoreError::StoreVar { why: "is set and empty" }),
        Some(place) if !Path::new(&place).is_absolute() => Err(TestStoreError::StoreVar { why: "is not an absolute path" }),
        Some(place) => Ok(PathBuf::from(place)),
        None => Ok(PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("blaze-test-store")),
    }
}

/// The store of the world `world`: one directory per key under [`store_base`].
pub fn store_root(world: ContentId) -> Result<PathBuf, TestStoreError> {
    Ok(store_base()?.join(hex(&world.0)))
}

fn hex(bytes: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push(char::from_digit(u32::from(byte >> 4), 16).expect("a nibble is a hex digit"));
        out.push(char::from_digit(u32::from(byte & 0xf), 16).expect("a nibble is a hex digit"));
    }
    out
}

/// Why the toolchain's records or the program over them could not be made, each by name.
#[derive(Debug)]
pub enum TestStoreError {
    /// [`STORE_VAR`] does not name a place.
    StoreVar { why: &'static str },
    /// The toolchain shelf did not open.
    Toolchain { shelf: String, why: CratesError },
    /// The program's crates could not be listed over the toolchain.
    Crates(CratesError),
    /// A toolchain crate has a build script's outputs: the key does not cover them, so it is
    /// refused here, never keyed without them.
    Script { krate: &'static str },
    /// The workspace file or directory at `path` (relative to the workspace root) could not be
    /// read for the producer walk.
    ProducerFile { path: String, why: std::io::Error },
    /// A directory entry under `dir` has a name that is not UTF-8.
    ProducerName { dir: String },
    /// The producer walk refused.
    Producers { why: blaze_producers::Refused<'static> },
    /// The walk could not say the `names` row.
    NamesRow { why: blaze_producers::Refused<'static> },
    /// The test store at `root` refused.
    Store { root: PathBuf, why: LentError },
    /// The by-records solve of the toolchain stopped at crate `krate`.
    Solve { krate: &'static str, why: CrateScopeError<'static> },
    /// The program crate's solve over the toolchain's records stopped.
    Program { why: CrateScopeError<'static> },
    /// The toolchain's names (for the items records' heads) could not be put together.
    Names(NamesError<'static>),
    /// A crate's names record could not be written.
    NamesRecord { krate: &'static str, why: NamesRecordError },
    /// A crate's items record could not be written.
    ItemsRecord { krate: &'static str, why: ItemsRecordError },
    /// An expansion's origins are an error other than "the text keeps none by design".
    Origins { krate: &'static str, call: ItemKey, why: OriginsError },
    /// The store's `done` key is there but a crate's section is not: a store written by
    /// another form of this harness under the same key domain.
    Missing { krate: &'static str, section: &'static str },
    /// A section the store lends is not 16-aligned, as the record readers require.
    Unaligned { krate: &'static str, section: &'static str },
    /// The command's frame could not be made.
    Frame(MemError),
    /// front2 over the records did not start.
    Front(FrontError),
}

impl core::fmt::Display for TestStoreError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            TestStoreError::StoreVar { why } => write!(f, "`{STORE_VAR}` {why}"),
            TestStoreError::Toolchain { shelf, why } => write!(f, "the toolchain at {shelf}: {why}"),
            TestStoreError::Crates(why) => write!(f, "the program's crates: {why}"),
            TestStoreError::Script { krate } => write!(f, "the toolchain crate `{krate}` has a build script's outputs, which the test store's key does not cover"),
            TestStoreError::ProducerFile { path, why } => write!(f, "the producer walk, reading `{path}`: {why}"),
            TestStoreError::ProducerName { dir } => write!(f, "the producer walk: an entry of `{dir}` has a name that is not UTF-8"),
            TestStoreError::Producers { why } => write!(f, "the producer walk: {why}"),
            TestStoreError::NamesRow { why } => write!(f, "the `names` producer row: {why}"),
            TestStoreError::Store { root, why } => write!(f, "the test store at {}: {why}", root.display()),
            TestStoreError::Solve { krate, why } => write!(f, "the toolchain's by-records solve, at crate `{krate}`: {why:?}"),
            TestStoreError::Program { why } => write!(f, "the program crate over the toolchain's records: {why:?}"),
            TestStoreError::Names(why) => write!(f, "the toolchain's names: {why}"),
            TestStoreError::NamesRecord { krate, why } => write!(f, "the names record of `{krate}`: {why:?}"),
            TestStoreError::ItemsRecord { krate, why } => write!(f, "the items record of `{krate}`: {why:?}"),
            TestStoreError::Origins { krate, call, why } => write!(f, "the origins of expansion {call:?} of `{krate}`: {why:?}"),
            TestStoreError::Missing { krate, section } => {
                write!(f, "the test store says the world is written but holds no `{section}` for crate `{krate}`")
            }
            TestStoreError::Unaligned { krate, section } => write!(f, "the test store lent `{section}` of crate `{krate}` unaligned (records need 16)"),
            TestStoreError::Frame(why) => write!(f, "the command's frame: {why:?}"),
            TestStoreError::Front(why) => write!(f, "front2 over the records: {why}"),
        }
    }
}

/// Whether this call made the toolchain's records or found them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Made {
    /// No `done` key: this call solved every toolchain crate and wrote its sections.
    Written { crates: usize },
    /// The toolchain's sections were in the store; only the program's crate was solved.
    Read,
}

/// One program over the test store's records: front2, its reader and what a test asks next.
pub struct OverRecords {
    pub front: Front2<'static, &'static Records<'static>>,
    pub records: &'static Records<'static>,
    pub frame: &'static Frame,
    /// Each crate's name by index, the program's at 0.
    pub crate_names: &'static [&'static str],
    /// The crates that may declare a lang item (the program's and the standard library's), as
    /// `Front2::from_records` and `ProjectWorld::from_records` take them.
    pub lang_crates: &'static [usize],
    /// The program crate's own names, solved in this call: crate 0's items by name.
    pub program: &'static CrateNames<'static>,
    /// The program crate's root module.
    pub root: ItemKey,
    pub made: Made,
    /// The world's key ([`world_key`]).
    pub world: ContentId,
    /// Where the store is.
    pub store: PathBuf,
    /// Toolchain source files read while this world was made: 0 when it was read.
    pub toolchain_files_read: usize,
    /// The program crate's own solve, and the whole construction.
    pub program_took: Duration,
    pub took: Duration,
    load: &'static StoreLoad,
}

impl OverRecords {
    /// Toolchain source files read since, by front2's questions (a head or body of a toolchain
    /// item lowered from its file's text): each a file a question reached, never a crate solved.
    pub fn toolchain_files_read_by_questions(&self) -> usize {
        self.load.question_reads.load(Ordering::Relaxed)
    }
}

/// The open test store: the command's own owner of every lent byte, leaked so each lend lives
/// for the test. `Lent` is not `Sync`: each construction opens its own.
struct TestStore {
    lent: &'static Lent<blaze_core::Native, Alloc<'static>>,
    root: PathBuf,
}

impl TestStore {
    fn open(root: PathBuf, alloc: Alloc<'static>) -> Result<TestStore, TestStoreError> {
        let driven = Driven::open(&root, &Settings::default(), alloc)
            .map_err(|why| TestStoreError::Store { root: root.clone(), why: LentError::Store { doing: "opening the test store", why } })?;
        let lent: &'static Lent<blaze_core::Native, Alloc<'static>> = Box::leak(Box::new(Lent::new(driven, alloc)));
        Ok(TestStore { lent, root })
    }

    fn get(&self, key: ContentId) -> Result<Option<&'static [u8]>, TestStoreError> {
        let lend = self.lent.get(&Key(key)).map_err(|why| self.stopped(why))?;
        Ok(lend.map(|l| l.bytes))
    }

    fn put(&self, key: ContentId, bytes: &[u8]) -> Result<(), TestStoreError> {
        self.lent.put(&Key(key), Namespace::LOCAL, TEST_TAG, bytes).map_err(|why| self.stopped(why))?;
        Ok(())
    }

    fn commit(&self) -> Result<(), TestStoreError> {
        self.lent.commit().map_err(|why| self.stopped(why))?;
        self.lent.seal().map_err(|why| self.stopped(why))?;
        Ok(())
    }

    fn stopped(&self, why: LentError) -> TestStoreError {
        TestStoreError::Store { root: self.root.clone(), why }
    }

    /// A section of a toolchain crate, 16-aligned; `Missing` when absent.
    fn section(&self, krate: &'static str, key: ContentId, section: &'static str) -> Result<&'static [u8], TestStoreError> {
        let bytes = self.get(key)?.ok_or(TestStoreError::Missing { krate, section })?;
        if bytes.as_ptr() as usize % 16 != 0 {
            return Err(TestStoreError::Unaligned { krate, section });
        }
        Ok(bytes)
    }
}

/// The `names` producer row (D105): blaze-producers' walk over this workspace, its files read
/// here, once per process. The rows' digests are the product's code identity of a producer;
/// the `names` row covers the crates whose functions make every record this store holds.
pub fn names_row(alloc: Alloc<'static>) -> Result<[u8; 32], TestStoreError> {
    let mut kept = NAMES_ROW.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(row) = *kept {
        return Ok(row);
    }
    let row = walk_names_row(alloc)?;
    *kept = Some(row);
    Ok(row)
}

fn walk_names_row(alloc: Alloc<'static>) -> Result<[u8; 32], TestStoreError> {
    use blaze_producers::{Digest, Entry, Step, Walk};
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut walk = Walk::new(alloc);
    loop {
        // The step's path, owned, so the walk can be answered.
        let (list, path) = match walk.next().map_err(|why| TestStoreError::Producers { why })? {
            Step::Done => break,
            Step::Read(path) => (false, path.to_owned()),
            Step::List(path) => (true, path.to_owned()),
        };
        let place = root.join(&path);
        if !list {
            let bytes = match std::fs::read(&place) {
                Ok(bytes) => Some(bytes),
                Err(why) if why.kind() == std::io::ErrorKind::NotFound => None,
                Err(why) => return Err(TestStoreError::ProducerFile { path, why }),
            };
            walk.read(bytes.as_deref()).map_err(|why| TestStoreError::Producers { why })?;
            continue;
        }
        let dir = match std::fs::read_dir(&place) {
            Ok(dir) => dir,
            Err(why) if why.kind() == std::io::ErrorKind::NotFound => {
                walk.listed(None).map_err(|why| TestStoreError::Producers { why })?;
                continue;
            }
            Err(why) => return Err(TestStoreError::ProducerFile { path, why }),
        };
        let mut found: Vec<(String, bool)> = Vec::new();
        for entry in dir {
            let entry = entry.map_err(|why| TestStoreError::ProducerFile { path: path.clone(), why })?;
            let name = entry.file_name().into_string().map_err(|_| TestStoreError::ProducerName { dir: path.clone() })?;
            // A symbolic link answers for what it points at (`Entry`'s contract).
            let is_dir = std::fs::metadata(entry.path()).map_err(|why| TestStoreError::ProducerFile { path: format!("{path}/{name}"), why })?.is_dir();
            found.push((name, is_dir));
        }
        let entries: Vec<Entry<'_>> = found.iter().map(|(name, is_dir)| Entry { name: name.as_str(), is_dir: *is_dir }).collect();
        walk.listed(Some(&entries)).map_err(|why| TestStoreError::Producers { why })?;
    }
    let digests = walk.finish().map_err(|why| TestStoreError::Producers { why })?;
    for (row, digest) in digests.rows {
        if !matches!(row, Row::Names) {
            continue;
        }
        return match digest {
            Digest::Source(digest) => Ok(digest),
            Digest::Refused(why) => Err(TestStoreError::NamesRow { why }),
        };
    }
    unreachable!("blaze-producers' rows hold `names` (Row::FROM_SOURCE)")
}

/// The key of the toolchain's world: everything its records are a function of (the module's
/// docs, THE KEY). `toolchain` is the crates as [`toolchain_only`] gives them.
pub fn world_key(shelf: ToolchainId, names_row: &[u8; 32], toolchain: &[CrateInput<'_>]) -> ContentId {
    let mut h = Hasher::new(KEY_DOMAIN);
    h.part(&shelf.0).part(names_row).part(&(toolchain.len() as u64).to_le_bytes());
    for c in toolchain {
        h.part(c.name.as_bytes()).id(&c.key.0);
        h.part(format!("{:?}", c.edition).as_bytes()).part(format!("{:?}", c.implicit).as_bytes());
        h.part(c.root.as_bytes()).part(c.shown_from.as_bytes());
        h.part(&(c.features.len() as u64).to_le_bytes());
        for f in c.features {
            h.part(f.as_bytes());
        }
        h.part(&(c.deps.len() as u64).to_le_bytes());
        for (name, at) in c.deps {
            h.part(name.as_bytes()).part(&(*at as u64).to_le_bytes());
        }
        h.part(&(c.cfg.len() as u64).to_le_bytes());
        for (name, value) in c.cfg {
            h.part(name.as_bytes()).opt_part(*value);
        }
        // A crate with a script is refused before it is keyed ([`toolchain_only`]).
        h.part(if c.script.is_some() { &b"script"[..] } else { &b"no script"[..] });
    }
    h.finish()
}

/// The key of `section` of the toolchain crate `krate` in world `world`.
fn section_key(world: ContentId, krate: CrateItemKey, section: &str) -> ContentId {
    Hasher::new(KEY_DOMAIN).id(&world).id(&krate.0).part(section.as_bytes()).finish()
}

/// The key of an expansion's `section` (its text or its origins) of call `call`.
fn call_key(world: ContentId, krate: CrateItemKey, section: &str, call: ItemKey) -> ContentId {
    Hasher::new(KEY_DOMAIN).id(&world).id(&krate.0).part(section.as_bytes()).id(&call.0).finish()
}

/// The key that says world `world`'s crates are all written.
fn done_key(world: ContentId) -> ContentId {
    Hasher::new(KEY_DOMAIN).id(&world).part(b"done").finish()
}

/// Whether the store already holds the world `world`, without making a store where none is:
/// what a lookup under a key finds before anything is solved.
pub fn is_made(world: ContentId, alloc: Alloc<'static>) -> Result<bool, TestStoreError> {
    let root = store_root(world)?;
    if !root.exists() {
        return Ok(false);
    }
    Ok(TestStore::open(root, alloc)?.get(done_key(world))?.is_some())
}

/// The sysroot as `Inputs` makes it (each role's crate at its unit's index plus one), found
/// by the crates' names: `Inputs` keeps its own private (TS1.md: `Inputs::sysroot` wanted).
fn sysroot_of(crates: &[CrateInput<'_>]) -> Sysroot {
    let at = |name: &str| crates.iter().skip(1).position(|c| c.name == name).map(|i| CrateId(i as u32 + 1));
    Sysroot { core: at("core"), alloc: at("alloc"), std: at("std"), bstd: at("bstd").or_else(|| at("blaze_std")) }
}

/// The same sysroot over the toolchain's crates alone (crate 0 dropped).
fn without_program(sysroot: Sysroot) -> Sysroot {
    let down = |id: Option<CrateId>| id.map(|CrateId(i)| CrateId(i - 1));
    Sysroot { core: down(sysroot.core), alloc: down(sysroot.alloc), std: down(sysroot.std), bstd: down(sysroot.bstd) }
}

/// The toolchain's crates alone, crate 0 (the program) dropped and each dependency's index
/// moved down by one: what the world is solved over, so no program's text reaches it.
pub fn toolchain_only(crates: &[CrateInput<'static>]) -> Result<&'static [CrateInput<'static>], TestStoreError> {
    let mut out = Vec::with_capacity(crates.len().saturating_sub(1));
    for c in crates.iter().skip(1) {
        if c.script.is_some() {
            return Err(TestStoreError::Script { krate: c.name });
        }
        let deps: &'static [(&'static str, usize)] = Vec::leak(
            c.deps
                .iter()
                .map(|&(name, at)| {
                    assert!(at >= 1, "the toolchain crate `{}` depends on the program crate", c.name);
                    (name, at - 1)
                })
                .collect(),
        );
        out.push(CrateInput {
            name: c.name,
            key: c.key,
            edition: c.edition,
            implicit: c.implicit,
            root: c.root,
            shown_from: c.shown_from,
            features: c.features,
            deps,
            cfg: c.cfg,
            script: None,
        });
    }
    Ok(Vec::leak(out))
}

/// The world's texts, crate `k` of a solve at `shift` read as crate `k + shift` of `Inputs`,
/// each read of a toolchain file counted: the guard's measure that a read world parses none.
struct Counted<'t> {
    texts: &'t Texts<'t>,
    shift: usize,
    reads: &'t AtomicUsize,
}

impl<'t> SourceTexts<'t> for Counted<'t> {
    fn read(&self, krate: usize, path: &str) -> Option<&'t str> {
        let at = krate + self.shift;
        if at != 0 {
            self.reads.fetch_add(1, Ordering::Relaxed);
        }
        self.texts.read(at, path)
    }
}

/// Solves the toolchain's crates by records and puts their sections: once per world.
fn write_toolchain(
    store: &TestStore,
    world: ContentId,
    toolchain: &'static [CrateInput<'static>],
    texts: &Counted<'static>,
    sysroot: Sysroot,
    alloc: Alloc<'static>,
) -> Result<usize, TestStoreError> {
    let bump = blaze_own::testing::bump();
    let (scopes, _needs) = resolve_by_records(toolchain, sysroot, texts, &Serial, &Ready::serial(SOLVE_WAIT), bump)
        .map_err(|(krate, why)| TestStoreError::Solve { krate: toolchain[krate].name, why })?;
    let all: Vec<&CrateNames<'_>> = scopes.iter().map(|s| s.names).collect();
    let names = Names::of_crates(toolchain, sysroot, &all, alloc).map_err(TestStoreError::Names)?;
    for (scope, c) in scopes.iter().zip(toolchain.iter()) {
        let krate = c.name;
        store.put(section_key(world, c.key, "exports"), &scope.record)?;
        let n = names_record::encode(scope.names, alloc).map_err(|why| TestStoreError::NamesRecord { krate, why })?;
        store.put(section_key(world, c.key, "names"), &n)?;
        let i = items_record::encode_with(scope.names, &names, alloc).map_err(|why| TestStoreError::ItemsRecord { krate, why })?;
        store.put(section_key(world, c.key, "items"), &i)?;
        for (call, e) in scope.names.expansions.iter() {
            let Some(m) = e.made else { continue };
            store.put(call_key(world, c.key, "expansion", *call), m.text.as_bytes())?;
            store.put(call_key(world, c.key, "origins", *call), origins_kept(krate, *call, &m.origins)?)?;
        }
    }
    store.put(done_key(world), &(toolchain.len() as u64).to_le_bytes())?;
    store.commit()?;
    Ok(toolchain.len())
}

/// The origins an expansion keeps: empty for a text that keeps none by design (blaze-scope
/// `OriginsError::Rewritten`, `Unplaced`; `Load`'s doc); any other error stops by name.
fn origins_kept<'a>(krate: &'static str, call: ItemKey, origins: &Result<&'a [u8], OriginsError>) -> Result<&'a [u8], TestStoreError> {
    match origins {
        Ok(bytes) => Ok(bytes),
        Err(OriginsError::Rewritten { .. } | OriginsError::Unplaced { .. }) => Ok(&[]),
        Err(why) => Err(TestStoreError::Origins { krate, call, why: *why }),
    }
}

/// The world `world` made in the store at `root` if no `done` says it is, once per process
/// ([`MADE`]). Gives the store it opened, when it opened one, for the reads that follow.
fn ensure_made(
    world: ContentId,
    root: &Path,
    toolchain: &'static [CrateInput<'static>],
    texts: &Counted<'static>,
    sysroot: Sysroot,
    alloc: Alloc<'static>,
) -> Result<(Made, Option<TestStore>), TestStoreError> {
    let mut made = MADE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if *made == Some(world) {
        return Ok((Made::Read, None));
    }
    let store = TestStore::open(root.to_path_buf(), alloc)?;
    let answer = match store.get(done_key(world))? {
        Some(_) => Made::Read,
        None => Made::Written { crates: write_toolchain(&store, world, toolchain, texts, sysroot, alloc)? },
    };
    *made = Some(world);
    Ok((answer, Some(store)))
}

/// The loader front2's reader asks: the program's records made in this call, the
/// toolchain's from the store. A store that fails under a question stops the test by name
/// (`Load` answers `Option`; a quiet `None` would read as "no record kept").
struct StoreLoad {
    store: TestStore,
    world: ContentId,
    /// The reader's crates: the program's at 0, then the toolchain's.
    crates: &'static [CrateInput<'static>],
    inputs: &'static Inputs<'static>,
    alloc: Alloc<'static>,
    names: &'static [u8],
    items: &'static [u8],
    files: Vec<(&'static str, &'static str)>,
    expansions: Vec<(ItemKey, &'static str, &'static [u8])>,
    /// Toolchain files read by questions ([`OverRecords::toolchain_files_read_by_questions`]).
    question_reads: AtomicUsize,
}

impl StoreLoad {
    fn toolchain(&self, krate: usize, key: ContentId) -> Option<&'static [u8]> {
        self.store.get(key).unwrap_or_else(|e| panic!("crate `{}`: {e}", self.crates[krate].name))
    }
}

impl Load<'static> for StoreLoad {
    fn names(&self, krate: usize) -> Option<&'static [u8]> {
        match krate {
            0 => Some(self.names),
            k => self.toolchain(k, section_key(self.world, self.crates.get(k)?.key, "names")),
        }
    }

    fn items(&self, krate: usize) -> Option<&'static [u8]> {
        match krate {
            0 => Some(self.items),
            k => self.toolchain(k, section_key(self.world, self.crates.get(k)?.key, "items")),
        }
    }

    fn file(&self, krate: usize, path: PackagePath<'_>) -> Option<&'static str> {
        if krate == 0 {
            return self.files.iter().find(|(p, _)| *p == path.as_str()).map(|(_, t)| *t);
        }
        // As the command's loader (blaze-cli front_load.rs `StoreLoad::file`): the path joined
        // onto the directory the crate's package is shown from, read from its pack.
        let at = path.in_package(self.crates.get(krate)?.shown_from, self.alloc)?;
        self.question_reads.fetch_add(1, Ordering::Relaxed);
        self.inputs.texts.read(krate, &at)
    }

    /// The text and its origins. The test store keeps them under two keys (`expansion`,
    /// `origins`), so a crate past the program's is two store reads here: this store's layout,
    /// not the command's, whose one expansion record holds both (blaze-cli front_load.rs
    /// `StoreLoad::expansion_read`).
    fn expansion(&self, krate: usize, call: ItemKey) -> Option<LoadedExpansion<'static>> {
        // `None` origins: the store keeps none for the call (served as "no origins held").
        let (text, bytes) = if krate == 0 {
            self.expansions.iter().find(|(c, _, _)| *c == call).map(|(_, t, o)| (*t, Some(*o)))?
        } else {
            let key = self.crates.get(krate)?.key;
            let bytes = self.toolchain(krate, call_key(self.world, key, "expansion", call))?;
            let text = core::str::from_utf8(bytes).unwrap_or_else(|_| panic!("crate `{}`: the expansion text of {call:?} in the test store is not UTF-8", self.crates[krate].name));
            (text, self.toolchain(krate, call_key(self.world, key, "origins", call)))
        };
        // The store keeps the origins in the joined form: its two parts, cut where the column
        // starts (empty: the text keeps none).
        let origins = bytes.map(|bytes| {
            if bytes.is_empty() {
                (&[][..], &[][..])
            } else {
                blaze_scope::expansion_origins::split(bytes).unwrap_or_else(|e| panic!("the test store's origins of {call:?}: {e}"))
            }
        });
        Some(LoadedExpansion { text, origins })
    }
}

fn leak(s: &str) -> &'static str {
    Box::leak(s.to_owned().into_boxed_str())
}

/// front2 over `user` and the toolchain on the shelf at `shelf_dir` (id [`PINNED`]), its
/// toolchain crates' records from the test store, made there first if absent. Everything is
/// leaked: a `Records` borrows its loader and log for its whole life, as in front2_records.rs.
/// `alloc` is the test's root heap (`blaze_own::testing::heap().alloc()`): any thread may use
/// it, as the by-records solve needs.
pub fn front_over_records(shelf_dir: &str, user: &'static UserCrate<'static>, alloc: Alloc<'static>) -> Result<OverRecords, TestStoreError> {
    let start = Instant::now();
    let tc: &'static Toolchain = Box::leak(Box::new(
        Toolchain::open(&mut DirShelf::new(shelf_dir.to_owned())).map_err(|why| TestStoreError::Toolchain { shelf: shelf_dir.to_owned(), why })?,
    ));
    let inputs: &'static Inputs<'static> = Box::leak(Box::new(Inputs::new(user, tc).map_err(TestStoreError::Crates)?));
    let crates: &'static [CrateInput<'static>] = Vec::leak(inputs.crates());
    let sysroot = without_program(sysroot_of(crates));
    let toolchain = toolchain_only(crates)?;
    let world = world_key(PINNED, &names_row(alloc)?, toolchain);
    let root = store_root(world)?;

    let reads: &'static AtomicUsize = Box::leak(Box::new(AtomicUsize::new(0)));
    let shifted = Counted { texts: &inputs.texts, shift: 1, reads };
    let (made, opened) = ensure_made(world, &root, toolchain, &shifted, sysroot, alloc)?;
    let store = match opened {
        Some(store) => store,
        None => TestStore::open(root, alloc)?,
    };

    // The program's crate alone, over the toolchain's exports records read in place.
    let mut deps: Vec<&'static [u8]> = Vec::with_capacity(toolchain.len());
    for c in toolchain {
        deps.push(store.section(c.name, section_key(world, c.key, "exports"), "exports")?);
    }
    let frame: &'static Frame = Box::leak(Box::new(Frame::new(blaze_mem::tag::Tag::Infer).map_err(TestStoreError::Frame)?));
    let bump = frame.alloc(blaze_own::Erased::new(frame)).bump();
    let prepares = Prepares::new_in(bump);
    let own_texts = Counted { texts: &inputs.texts, shift: 0, reads };
    let program_start = Instant::now();
    // Crate 0 reads the program's files (crates.rs `Texts::read`); a toolchain file it read
    // would be counted.
    let scope = resolve_crate(&crates[0], &deps, sysroot, &own_texts, 0, &Serial, prepares, bump).map_err(|why| TestStoreError::Program { why })?;
    let own: &'static CrateNames<'static> = scope.names;
    let krate = own.name;
    let names = aligned(&names_record::encode(own, alloc).map_err(|why| TestStoreError::NamesRecord { krate, why })?, bump);
    let items = aligned(&items_record::encode(own, alloc).map_err(|why| TestStoreError::ItemsRecord { krate, why })?, bump);
    let program_took = program_start.elapsed();
    let files = own.files.iter().map(|f| (leak(f.package.as_str()), f.text)).collect();
    let mut expansions = Vec::new();
    for (call, e) in own.expansions.iter() {
        let Some(m) = e.made else { continue };
        expansions.push((*call, m.text, origins_kept(krate, *call, &m.origins)?));
    }

    let store_root = store.root.clone();
    let crate_names: &'static [&'static str] = Vec::leak(crates.iter().map(|c| c.name).collect());
    let keys: &'static [CrateItemKey] = Vec::leak(crates.iter().map(|c| c.key).collect());
    let position = |name: &str| crate_names.iter().position(|n| *n == name);
    // The crates that can declare a lang item: the standard library's and the program's own.
    let lang_crates: &'static [usize] =
        Vec::leak([Some(0), position("core"), position("alloc"), position("std"), position("bstd"), position("blaze_std")].into_iter().flatten().collect());
    let load: &'static StoreLoad = Box::leak(Box::new(StoreLoad {
        store,
        world,
        crates,
        inputs,
        alloc,
        names,
        items,
        files,
        expansions,
        question_reads: AtomicUsize::new(0),
    }));
    let log: &'static Log<'static> = Box::leak(Box::new(Log::new(crate_names, alloc)));
    let records: &'static Records<'static> = Box::leak(Box::new(Records::new(keys, load, log, frame, alloc)));
    let front = Front2::from_records(records, frame, lang_crates).map_err(TestStoreError::Front)?;
    Ok(OverRecords {
        front,
        records,
        frame,
        crate_names,
        lang_crates,
        program: own,
        root: own.root,
        made,
        world,
        store: store_root,
        toolchain_files_read: reads.load(Ordering::Relaxed),
        program_took,
        took: start.elapsed(),
        load,
    })
}
