//! The toolchain's crate records for one shelf, from the test store, made if absent: the
//! shared harness that lets a test stop re-solving core, alloc and std (lane TS1, wave 2311).
//!
//! WHERE: one plain store (blaze-cas `Driven` under a `Lent`, the command's store code) at
//! `$CARGO_TARGET_TMPDIR/blaze-test-store/<shelf id in hex>`: under the cargo target dir, never
//! the user's home store (`~/.blaze/v2/store`), which tests must not write. One store per
//! toolchain shelf id ([`blaze_toolchain::PINNED`]); every test binary of this crate shares it.
//!
//! KEYS (rule 65, per crate, from text before the work): for each toolchain crate and each
//! section, `Hasher(KEY_DOMAIN)` over the shelf id, the crate's key (its pack digest and its
//! features, crates.rs `unit_key`) and the section's name; an expansion's text and origins add
//! the call's key. The sections: the crate's exports record (what a dependent's solve reads),
//! its names record (`names_record::encode`) and its items record (`items_record::encode_with`
//! over the whole toolchain's names). A last key, `done`, says every crate of the shelf was
//! written: it is put in the same commit, so a reader that finds it finds the rest.
//!
//! WHAT IS NOT IN THE KEY: the code identity of blaze-scope (decision D105's `Names` row). The
//! records' own magic and form version are checked by their readers, so a record of an older
//! form is refused by name, but a change to blaze-scope that keeps the form reads the older
//! records. TS1.md names the wanted fix (the producers digest in a build step of this crate).
//!
//! THE RACE: two processes that both find no `done` both solve and both put. Each record is a
//! function of its key's inputs (rule 25), so the second put meets equal bytes (`Put::Same`);
//! other bytes are the store's typed `TwoValues` refusal (rule 21), never a lock.
//!
//! FLOW, for every caller (the first one too: what it wrote it then reads, so one path is
//! tested): open the store; if `done` is absent, solve every crate by records
//! (`resolve_by_records`) and put the toolchain's sections, then commit and seal; then solve
//! the program's crate alone over the toolchain's exports records read in place
//! (`resolve_crate`), and hand front2 a `Records` reader whose loader serves the program's own
//! records from this call and the toolchain's from the store (`Front2::from_records`).

use std::path::PathBuf;
use std::time::Duration;

use blaze_cas::Key;
use blaze_cas::plain::{Driven, Lent, LentError, Namespace, Settings, Tag};
use blaze_id::{CrateItemKey, ItemKey};
use blaze_ids::{ContentId, Hasher};
use blaze_ladder::crates::{CratesError, DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::source::{Load, Log, Records};
use blaze_ladder::front2::{Front2, FrontError};
use blaze_mem::{Frame, MemError};
use blaze_own::Alloc;
use blaze_scope::crate_scope::{CrateScopeError, Ready, resolve_by_records, resolve_crate};
use blaze_scope::graph::{CrateId, Sysroot};
use blaze_scope::items_record::{self, ItemsRecordError};
use blaze_scope::names_record::{self, NamesRecordError};
use blaze_scope::package_path::PackagePath;
use blaze_scope::prepares::Prepares;
use blaze_scope::resolve::{CrateInput, CrateNames, Names, NamesError, SourceTexts, aligned};
use blaze_scope::task::Serial;
use blaze_toolchain::{PINNED, ToolchainId};

/// The domain every key of the test store is hashed under; a change of what a section holds
/// that its record's form does not say moves this name.
const KEY_DOMAIN: &str = "blaze-ladder test store: a toolchain crate's section 1";

/// The tag the test store files its records under: its own, apart from the command's
/// (`DRIVE_TAG`, `u16::MAX - 2`), the store's root and the startup image.
const TEST_TAG: Tag = Tag(1);

/// How long the by-records solve waits for one crate's dependencies before it stops by name.
const SOLVE_WAIT: Duration = Duration::from_secs(600);

/// The test store's place for shelf `shelf`: under the cargo target dir's test folder, never
/// a home.
pub fn store_root(shelf: ToolchainId) -> PathBuf {
    let mut hex = String::with_capacity(64);
    for byte in shelf.0 {
        hex.push(char::from_digit(u32::from(byte >> 4), 16).expect("a nibble is a hex digit"));
        hex.push(char::from_digit(u32::from(byte & 0xf), 16).expect("a nibble is a hex digit"));
    }
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("blaze-test-store").join(hex)
}

/// Why the toolchain's records or the program over them could not be made, each by name.
#[derive(Debug)]
pub enum TestStoreError {
    /// The toolchain shelf did not open.
    Toolchain { shelf: String, why: CratesError },
    /// The program's crates could not be listed over the toolchain.
    Crates(CratesError),
    /// The test store at `root` refused while `doing`.
    Store { root: PathBuf, why: LentError },
    /// The by-records solve of every crate stopped at crate `krate`.
    Solve { krate: &'static str, why: CrateScopeError<'static> },
    /// The program crate's solve over the toolchain's records stopped.
    Program { why: CrateScopeError<'static> },
    /// The whole toolchain's names (for the items records' heads) could not be put together.
    Names(NamesError<'static>),
    /// A crate's names record could not be written.
    NamesRecord { krate: &'static str, why: NamesRecordError },
    /// A crate's items record could not be written.
    ItemsRecord { krate: &'static str, why: ItemsRecordError },
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
            TestStoreError::Toolchain { shelf, why } => write!(f, "the toolchain at {shelf}: {why}"),
            TestStoreError::Crates(why) => write!(f, "the program's crates: {why}"),
            TestStoreError::Store { root, why } => write!(f, "the test store at {}: {why}", root.display()),
            TestStoreError::Solve { krate, why } => write!(f, "the by-records solve, at crate `{krate}`: {why:?}"),
            TestStoreError::Program { why } => write!(f, "the program crate over the toolchain's records: {why:?}"),
            TestStoreError::Names(why) => write!(f, "the toolchain's names: {why}"),
            TestStoreError::NamesRecord { krate, why } => write!(f, "the names record of `{krate}`: {why:?}"),
            TestStoreError::ItemsRecord { krate, why } => write!(f, "the items record of `{krate}`: {why:?}"),
            TestStoreError::Missing { krate, section } => {
                write!(f, "the test store says the shelf is written but holds no `{section}` for crate `{krate}`")
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
    /// No `done` key: this call solved every crate and wrote the toolchain's sections.
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
    /// The program crate's root module.
    pub root: ItemKey,
    pub made: Made,
    /// Where the store is.
    pub store: PathBuf,
}

/// The open test store: the command's own owner of every lent byte, leaked so each lend lives
/// for the test.
struct TestStore {
    lent: &'static Lent<blaze_core::Native, Alloc<'static>>,
    root: PathBuf,
}

impl TestStore {
    fn open(shelf: ToolchainId, alloc: Alloc<'static>) -> Result<TestStore, TestStoreError> {
        let root = store_root(shelf);
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

/// The key of `section` of the toolchain crate `krate` on shelf `shelf`.
fn section_key(shelf: ToolchainId, krate: CrateItemKey, section: &str) -> ContentId {
    Hasher::new(KEY_DOMAIN).part(&shelf.0).id(&krate.0).part(section.as_bytes()).finish()
}

/// The key of an expansion's `section` (its text or its origins) of call `call`.
fn call_key(shelf: ToolchainId, krate: CrateItemKey, section: &str, call: ItemKey) -> ContentId {
    Hasher::new(KEY_DOMAIN).part(&shelf.0).id(&krate.0).part(section.as_bytes()).id(&call.0).finish()
}

/// The key that says shelf `shelf`'s crates are all written.
fn done_key(shelf: ToolchainId, crates: &[CrateInput<'_>]) -> ContentId {
    let mut hasher = Hasher::new(KEY_DOMAIN);
    hasher.part(&shelf.0).part(b"done");
    for c in crates.iter().skip(1) {
        hasher.id(&c.key.0);
    }
    hasher.finish()
}

/// The sysroot as `Inputs` makes it (each role's crate at its unit's index plus one), found
/// by the crates' names: `Inputs` keeps its own private (TS1.md: `Inputs::sysroot` wanted).
fn sysroot_of(crates: &[CrateInput<'_>]) -> Sysroot {
    let at = |name: &str| crates.iter().skip(1).position(|c| c.name == name).map(|i| CrateId(i as u32 + 1));
    Sysroot { core: at("core"), alloc: at("alloc"), std: at("std"), bstd: at("bstd").or_else(|| at("blaze_std")) }
}

/// The same sysroot over the toolchain's crates alone (crate 0 dropped): what the program's
/// solve indexes its `deps` by.
fn without_program(sysroot: Sysroot) -> Sysroot {
    let down = |id: Option<CrateId>| id.map(|CrateId(i)| CrateId(i - 1));
    Sysroot { core: down(sysroot.core), alloc: down(sysroot.alloc), std: down(sysroot.std), bstd: down(sysroot.bstd) }
}

fn leak(s: &str) -> &'static str {
    Box::leak(s.to_owned().into_boxed_str())
}

/// Solves every crate by records and puts the toolchain crates' sections: the first caller's
/// work for a shelf, once.
fn write_toolchain(
    store: &TestStore,
    shelf: ToolchainId,
    crates: &'static [CrateInput<'static>],
    inputs: &'static Inputs<'static>,
    sysroot: Sysroot,
    alloc: Alloc<'static>,
) -> Result<usize, TestStoreError> {
    let (scopes, _needs) = resolve_by_records(crates, sysroot, &inputs.texts, &Serial, &Ready::serial(SOLVE_WAIT), alloc)
        .map_err(|(krate, why)| TestStoreError::Solve { krate: crates[krate].name, why })?;
    let all: Vec<&'static CrateNames<'static>> = scopes.iter().map(|s| s.names).collect();
    let names: &'static Names<'static> = Box::leak(Box::new(Names::of_crates(crates, sysroot, &all, alloc).map_err(TestStoreError::Names)?));
    for (scope, c) in scopes.iter().zip(crates.iter()).skip(1) {
        let krate = c.name;
        store.put(section_key(shelf, c.key, "exports"), &scope.record)?;
        let n = names_record::encode(scope.names, alloc).map_err(|why| TestStoreError::NamesRecord { krate, why })?;
        store.put(section_key(shelf, c.key, "names"), &n)?;
        let i = items_record::encode_with(scope.names, names, alloc).map_err(|why| TestStoreError::ItemsRecord { krate, why })?;
        store.put(section_key(shelf, c.key, "items"), &i)?;
        for (call, e) in scope.names.expansions.iter() {
            let Some(m) = e.made else { continue };
            store.put(call_key(shelf, c.key, "expansion", *call), m.text.as_bytes())?;
            // A text whose origins could not be made keeps none: the empty form (Load's doc).
            store.put(call_key(shelf, c.key, "origins", *call), m.origins.unwrap_or(&[]))?;
        }
    }
    store.put(done_key(shelf, crates), &(crates.len() as u64 - 1).to_le_bytes())?;
    store.commit()?;
    Ok(crates.len() - 1)
}

/// The loader front2's reader asks: the program's records made in this call, the
/// toolchain's from the store. A store that fails under a question stops the test by name
/// (`Load` answers `Option`; a quiet `None` would read as "no record kept").
struct StoreLoad {
    store: TestStore,
    shelf: ToolchainId,
    crates: &'static [CrateInput<'static>],
    inputs: &'static Inputs<'static>,
    alloc: Alloc<'static>,
    names: &'static [u8],
    items: &'static [u8],
    files: Vec<(&'static str, &'static str)>,
    expansions: Vec<(ItemKey, &'static str, &'static [u8])>,
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
            k => self.toolchain(k, section_key(self.shelf, self.crates.get(k)?.key, "names")),
        }
    }

    fn items(&self, krate: usize) -> Option<&'static [u8]> {
        match krate {
            0 => Some(self.items),
            k => self.toolchain(k, section_key(self.shelf, self.crates.get(k)?.key, "items")),
        }
    }

    fn file(&self, krate: usize, path: PackagePath<'_>) -> Option<&'static str> {
        if krate == 0 {
            return self.files.iter().find(|(p, _)| *p == path.as_str()).map(|(_, t)| *t);
        }
        // As the command's loader (blaze-cli front_load.rs `StoreLoad::file`): the path joined
        // onto the directory the crate's package is shown from, read from its pack.
        let at = path.in_package(self.crates.get(krate)?.shown_from, self.alloc)?;
        self.inputs.texts.read(krate, &at)
    }

    fn expansion(&self, krate: usize, call: ItemKey) -> Option<&'static str> {
        if krate == 0 {
            return self.expansions.iter().find(|(c, _, _)| *c == call).map(|(_, t, _)| *t);
        }
        let bytes = self.toolchain(krate, call_key(self.shelf, self.crates.get(krate)?.key, "expansion", call))?;
        Some(core::str::from_utf8(bytes).unwrap_or_else(|_| panic!("crate `{}`: the expansion text of {call:?} in the test store is not UTF-8", self.crates[krate].name)))
    }

    fn expansion_origins(&self, krate: usize, call: ItemKey) -> Option<(&'static [u8], &'static [u8])> {
        let bytes = if krate == 0 {
            self.expansions.iter().find(|(c, _, _)| *c == call).map(|(_, _, o)| *o)?
        } else {
            self.toolchain(krate, call_key(self.shelf, self.crates.get(krate)?.key, "origins", call))?
        };
        // The fixture and its shelf keep the joined form: its two parts, cut where the column
        // starts (empty: the text keeps none).
        if bytes.is_empty() {
            return Some((&[], &[]));
        }
        Some(blaze_scope::expansion_origins::split(bytes).unwrap_or_else(|e| panic!("the fixture's origins of {call:?}: {e}")))
    }
}

/// front2 over `user` and the toolchain on the shelf at `shelf_dir` (id [`PINNED`]), its
/// toolchain crates' records from the test store, made there first if absent. Everything is
/// leaked: a `Records` borrows its loader and log for its whole life, as in front2_records.rs.
/// `alloc` is the test's root heap (`blaze_own::testing::heap().alloc()`): any thread may use
/// it, as the by-records solve needs.
pub fn front_over_records(shelf_dir: &str, user: &'static UserCrate<'static>, alloc: Alloc<'static>) -> Result<OverRecords, TestStoreError> {
    let shelf = PINNED;
    let tc: &'static Toolchain = Box::leak(Box::new(
        Toolchain::open(&mut DirShelf::new(shelf_dir.to_owned())).map_err(|why| TestStoreError::Toolchain { shelf: shelf_dir.to_owned(), why })?,
    ));
    let inputs: &'static Inputs<'static> = Box::leak(Box::new(Inputs::new(user, tc).map_err(TestStoreError::Crates)?));
    let crates: &'static [CrateInput<'static>] = Vec::leak(inputs.crates());
    let sysroot = sysroot_of(crates);
    let store = TestStore::open(shelf, alloc)?;

    let made = match store.get(done_key(shelf, crates))? {
        Some(_) => Made::Read,
        None => Made::Written { crates: write_toolchain(&store, shelf, crates, inputs, sysroot, alloc)? },
    };

    // The program's crate alone, over the toolchain's exports records read in place.
    let mut deps: Vec<&'static [u8]> = Vec::with_capacity(crates.len() - 1);
    for c in crates.iter().skip(1) {
        deps.push(store.section(c.name, section_key(shelf, c.key, "exports"), "exports")?);
    }
    let prepares = Prepares::new_in(alloc);
    // Crate 0 reads the program's files (crates.rs `Texts::read`); no other index is asked.
    let scope = resolve_crate(&crates[0], &deps, without_program(sysroot), &inputs.texts, 0, &Serial, prepares, alloc).map_err(|why| TestStoreError::Program { why })?;
    let own = scope.names;
    let krate = own.name;
    let names = aligned(&names_record::encode(own, alloc).map_err(|why| TestStoreError::NamesRecord { krate, why })?, alloc);
    let items = aligned(&items_record::encode(own, alloc).map_err(|why| TestStoreError::ItemsRecord { krate, why })?, alloc);
    let files = own.files.iter().map(|f| (leak(f.package.as_str()), f.text)).collect();
    let expansions = own.expansions.iter().filter_map(|(call, e)| e.made.map(|m| (*call, m.text, m.origins.unwrap_or(&[])))).collect();

    let store_root = store.root.clone();
    let crate_names: &'static [&'static str] = Vec::leak(crates.iter().map(|c| c.name).collect());
    let keys: &'static [CrateItemKey] = Vec::leak(crates.iter().map(|c| c.key).collect());
    let position = |name: &str| crate_names.iter().position(|n| *n == name);
    // The crates that can declare a lang item: the standard library's and the program's own.
    let lang_crates: Vec<usize> =
        [Some(0), position("core"), position("alloc"), position("std"), position("bstd"), position("blaze_std")].into_iter().flatten().collect();
    let load: &'static StoreLoad = Box::leak(Box::new(StoreLoad { store, shelf, crates, inputs, alloc, names, items, files, expansions }));
    let frame: &'static Frame = Box::leak(Box::new(Frame::new(blaze_mem::tag::Tag::Infer).map_err(TestStoreError::Frame)?));
    let log: &'static Log<'static> = Box::leak(Box::new(Log::new(crate_names, alloc)));
    let records: &'static Records<'static> = Box::leak(Box::new(Records::new(keys, load, log, frame, alloc)));
    let front = Front2::from_records(records, frame, &lang_crates).map_err(TestStoreError::Front)?;
    Ok(OverRecords { front, records, frame, crate_names, root: own.root, made, store: store_root })
}
