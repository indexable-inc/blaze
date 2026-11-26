//! A crate's deferred C requests run before the program's image is made (design A, c.md D174;
//! `run` and `test` only: `check` never reaches here, so a check after a run runs nothing).
//!
//! A build script runs with its C compiles DEFERRED (spawn_route.rs `Route::Deferred`): each
//! `cc`/`ar` it spawned is answered "exit 0" and kept in its record (`OutputsRef::c_requests`),
//! with the `OUT_DIR` state it saw. The record is the same for `check`, `run` and `test`, so the
//! mode joins no script key. The link takes no deferred record (blaze-ladder natives.rs
//! `DeferredC`). [`built_for`] turns one into the record the link takes: the script's outputs
//! with the requests run, their archives stored files of the home's file area.
//!
//! 1. ONE GET: the BUILT record's key (blaze-build-script c_tasks_built.rs `built_key`) is the
//!    script's out, the toolchain (the shelf's digest, the SDK's identity, the target), the
//!    package's pinned files (the sources store's key and the files' BLAKE3,
//!    run_ids.rs `Place::Fetched`) and the archives the crate asks for. A hit is the outputs the
//!    link takes, read in place: nothing is laid, planned or compiled.
//! 2. On a miss, the package is laid from its pack (`<work>/pkg/<digest>`, made once and
//!    renamed into place) and blaze-proc-macro native_c_tasks.rs `run_tasks` plans, looks up,
//!    compiles in parallel (one worker per command worker, each compile under the Seatbelt
//!    grant of a native C task with its own deadline), records and archives. Its assumptions
//!    are discharged there too. Then the built record is put.
//!
//! The ScriptIn the image takes is the built one: its out is the built record's key, so the
//! image's key (engine_native.rs `image_key`) moves with every input above, and a deferred
//! record never reaches the link.
//!
//! Checkout packages (`Place::Checkout`, `Place::CheckoutDep`, such as workspace members and
//! vendored path dependencies like `vendor/wasmtime`) are walked deterministically in sorted
//! relative-path order (skipping `.git`, `target`, and `.blaze`, refusing escaping symlinks and
//! invalid paths) to compute a content-addressed BLAKE3 digest (`Pinned { source: d, files: d }`).
//! On a built-record hit nothing is copied or compiled; on a miss an immutable snapshot of those
//! exact hashed bytes is laid atomically under `<work>/pkg/<hex>` and compiled against the
//! package's guest mount (`SeenRoot::member(rel_dir)`, i.e. `/workspace` or `/workspace/<dir>`).
//! Checkout `links` dependencies referenced via `/deps/<links>` are hashed and snapshotted the
//! same way.
//!
//! Not yet, each refused by name ([`CTaskWhy`]):
//! - a toolchain crate with deferred C requests (`Place::Toolchain`).
//!
//! A `links` dependency's trees a request names (c_tasks.rs `dep_roots`: librocksdb-sys reads
//! lz4-sys's and zstd-sys's) are laid read-only beside the package: a producer's `OUT_DIR` as its
//! own C step left it (its process tree's headers included; its C runs first, [`Producers`]),
//! by the content of its files (`<work>/dep/<digest>`, [`dep_out`]), its served tree
//! (`/deps/<links>`) from its pack; each joins the built key by its digest.
//!
//! Process-tree requests (tikv-jemalloc-sys's `sh configure`, `make`) run as one task before
//! the compiles (native_c_tasks.rs `ShelfTree`, blaze-build-script c_tasks_tree.rs): the host
//! tools the table grants the crate and the OS build join the built key
//! (`tree_tools_digest`).
//!
//! FLOOR (rule 67): a hit costs one hash of the key's inputs and one get; the floor of a miss
//! is clang's own time on the misses spread over the workers, plus laying the package once.

use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};
use std::io::{Read as _, Write as _};

use blaze_build_script::c_tasks::{self, CStore, CompilerKind, GuestRoots, StoreFailed, Toolchain};
use blaze_build_script::c_tasks_built::{self, Pinned, Report};
use blaze_build_script::dep_paths::DEPS_ROOT;
use blaze_build_script::outputs::OutputsRef;
use blaze_drive::Key;
use blaze_expand::seen::SeenRoot;
use blaze_ids::{ContentId, Hasher};
use blaze_own::{AString, AVec, Alloc, String, Vec};
use blaze_proc_macro::native_c::Laid;
use blaze_proc_macro::native_c_tasks::{self, COMPILE_DEADLINE, CTasksSetting};
use blaze_proc_macro::native_setting::{OwnedNativeSetting, command_native_setting};
use blaze_world::crate_list::CrateList;
use bstd::cx::Cx;
use bstd::mem::Frame;

use crate::CliError;
use crate::body_types::{PackDirs, PackHome, open_row};
use crate::engine_native::ScriptIn;
use crate::run_ids::{CrateTable, Place};
use crate::store::Store;

/// The native work folder's command name (`<home>/<native work>/c-tasks`): task folders, laid
/// packages and laid `OUT_DIR` states.
const WORK: &str = "c-tasks";

/// Domain string for the content-addressed BLAKE3 digest of a checkout package tree.
const CHECKOUT_PKG_DOMAIN: &str = "blaze-cli c tasks checkout pkg 1";
/// Domain string for hashing one file's bytes inside a checkout package tree.
const CHECKOUT_FILE_DOMAIN: &str = "blaze-cli c tasks checkout file 1";
/// Maximum directory nesting depth walked inside a checkout package folder (rule 18).
const CHECKOUT_MAX_DEPTH: usize = 64;
/// Maximum directories queued/visited inside a checkout package folder (rule 18).
const CHECKOUT_MAX_DIRS: usize = 32_768;
/// Maximum regular files hashed and snapshotted inside a checkout package folder (rule 18).
const CHECKOUT_MAX_FILES: usize = 131_072;
/// Maximum total bytes across all files of a checkout package snapshot (512 MiB, rule 18).
const CHECKOUT_MAX_BYTES: u64 = 512 << 20;
/// Streaming read/write buffer size for checkout hashing and snapshot laying (64 KiB).
const CHECKOUT_READ_CHUNK: usize = 64 << 10;

/// Per-process sequence number so concurrent snapshot staging directories never collide.
static SNAPSHOT_PART_SEQ: AtomicU64 = AtomicU64::new(0);

/// The guest spellings of the folders no request names: the SDK (a wasm script is given no
/// `SDKROOT`) and the shelf. They show only in the prefix maps, so in no output of two
/// checkouts differently (rule 25).
const GUEST_SDK: &str = "/sdk";
const GUEST_SHELF: &str = "/toolchain/clang";
/// The folder every crate's guest `OUT_DIR` is under (blaze-build-script env.rs).
const GUEST_BUILDS: &str = "/target/debug/build";

/// Why a crate's deferred C requests gave no built record: each names the crate (rule 19).
#[derive(Debug)]
pub(crate) enum CTaskWhy<'w> {
    /// Its `Script` payload is no outputs record.
    Outputs,
    /// Its files can change (`why`): no pinned key, and no pack to lay.
    NotPinned { why: &'static str },
    /// The C toolchain could not be set up (the shelf, the SDK, the work folder, the file
    /// area): the words name the folder.
    Setup { words: &'w str },
    /// Its pack could not be opened, or laid as its package's folder.
    Lay { words: &'w str },
    /// The tasks failed: a compile, an assumption, a refusal (native_c_tasks.rs `TasksError`).
    Tasks { words: &'w str },
    /// The built outputs could not be made or put.
    Built { words: &'w str },
}

impl fmt::Display for CTaskWhy<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CTaskWhy::Outputs => write!(f, "its build script's record is no outputs record"),
            CTaskWhy::NotPinned { why } => write!(f, "its deferred C requests are not run yet: {why} (native_runner_c.rs)"),
            CTaskWhy::Setup { words } => write!(f, "the C toolchain could not be set up: {words}"),
            CTaskWhy::Lay { words } => write!(f, "its package could not be laid for its compiles: {words}"),
            CTaskWhy::Tasks { words } => write!(f, "{words}"),
            CTaskWhy::Built { words } => write!(f, "its built record: {words}"),
        }
    }
}

/// One crate's report line (`blaze: C of <package>: ...`): the request count per crate, and
/// what ran.
pub(crate) struct Said<'a> {
    pub(crate) package: &'a str,
    pub(crate) report: &'a Report,
}

impl fmt::Display for Said<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Said { package, report } = self;
        if report.built_hit {
            return write!(f, "blaze: C of {package}: {} requests, found built (one get)", report.requests);
        }
        write!(
            f,
            "blaze: C of {package}: {} requests, {} objects ({} found, {} compiled), {} assumptions, {} archives ({} made, {} bytes), {} ms",
            report.requests,
            report.planned,
            report.found,
            report.compiled,
            report.assumed,
            report.archives,
            report.archives_made,
            report.archive_bytes,
            report.ms
        )
    }
}

/// The C toolchain of one command's image, set up at the first crate that has requests.
struct Setup {
    setting: OwnedNativeSetting<'static>,
    /// `<shelf>/bin/clang`, canonical (`clang-23`).
    clang: AString<'static>,
    /// The home's file area, canonical (scripts.rs `file_area`).
    area: AString<'static>,
    /// The shelf's digest: its folder's name.
    compiler: [u8; 32],
    sdk_key: [u8; blaze_apple_sdk::SdkId::KEY_LEN],
    sdk_digest: [u8; 32],
}

/// What [`built_for`] reads beyond one crate: handed by engine_native.rs `image_for`, for the
/// whole image. The tasks' results and the setup are held on `heap` (the command's, handed in:
/// compile results cross worker threads there, native_c_tasks.rs `CTasksSetting::heap`).
pub(crate) struct CTasks<'e> {
    home: Option<&'e str>,
    /// The checkout's canonical root directory resolved once at the command's edge
    /// (`Served::root_text`), used to locate `Place::Checkout | Place::CheckoutDep` packages.
    checkout_root: Option<&'e str>,
    sdk: blaze_apple_sdk::Sdk<'e>,
    fs: &'e bstd::fs::Fs,
    workers: u32,
    heap: &'static blaze_own::Heap,
    setup: Option<Setup>,
    dirs: PackDirs,
}

impl<'e> CTasks<'e> {
    pub(crate) fn new(
        home: Option<&'e str>,
        checkout_root: Option<&'e str>,
        sdk: blaze_apple_sdk::Sdk<'e>,
        fs: &'e bstd::fs::Fs,
        workers: u32,
        heap: &'static blaze_own::Heap,
    ) -> CTasks<'e> {
        CTasks { home, checkout_root, sdk, fs, workers, heap, setup: None, dirs: PackDirs::default() }
    }

    /// The toolchain, set up once.
    fn setup<'w>(&mut self, scratch: &'w Frame) -> Result<&Setup, CTaskWhy<'w>> {
        if self.setup.is_none() {
            self.setup = Some(set_up(self.home, self.sdk, self.heap.alloc(), scratch)?);
        }
        Ok(self.setup.as_ref().expect("native_runner_c: set up just above"))
    }
}

fn kept<'s>(scratch: &'s Frame, words: fmt::Arguments<'_>) -> &'s str {
    crate::work::held_str(bstd::mem::format_in!(scratch, "{words}").as_str(), scratch)
}

fn set_up<'w>(home: Option<&str>, sdk: blaze_apple_sdk::Sdk<'_>, alloc: Alloc<'static>, scratch: &'w Frame) -> Result<Setup, CTaskWhy<'w>> {
    let words = |what: fmt::Arguments<'_>| CTaskWhy::Setup { words: kept(scratch, what) };
    let Some(home) = home else { return Err(words(format_args!("HOME is not set, and the shelf and the work folder are beneath it"))) };
    let setting = command_native_setting(home, sdk.folder(), WORK, crate::graph::TRIPLE, alloc).map_err(|e| words(format_args!("{e}")))?;
    let mut clang = String::from_str_in(setting.shelf.as_str(), alloc);
    clang.push_str("/bin/clang");
    let real = std::fs::canonicalize(clang.as_str()).map_err(|e| words(format_args!("{}: {e}", clang.as_str())))?;
    let real = real.to_str().ok_or_else(|| words(format_args!("{}: its real path is not UTF-8", clang.as_str())))?;
    let clang = String::from_str_in(real, alloc);
    let compiler = hex32(setting.shelf.as_str().rsplit('/').next().unwrap_or("")).ok_or_else(|| words(format_args!("the shelf {} is not named by its digest", setting.shelf.as_str())))?;
    let mut area = String::from_str_in(home, alloc);
    area.push('/');
    area.push_str(crate::scripts::STORE_UNDER_HOME);
    area.push('/');
    area.push_str(blaze_build_script::outputs::FILE_AREA);
    std::fs::create_dir_all(area.as_str()).map_err(|e| words(format_args!("{}: {e}", area.as_str())))?;
    let real = std::fs::canonicalize(area.as_str()).map_err(|e| words(format_args!("{}: {e}", area.as_str())))?;
    let real = real.to_str().ok_or_else(|| words(format_args!("{}: its real path is not UTF-8", area.as_str())))?;
    let area = String::from_str_in(real, alloc);
    let sdk_key = sdk.identity().key();
    let mut digest = Hasher::new("blaze-cli c tasks sdk root 1");
    digest.update(&sdk_key);
    Ok(Setup { setting, clang, area, compiler, sdk_key, sdk_digest: digest.finish().0 })
}

/// 64 hex digits as 32 bytes.
fn hex32(text: &str) -> Option<[u8; 32]> {
    let bytes = text.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let digit = |b: u8| match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    };
    let mut out = [0u8; 32];
    for (at, pair) in bytes.chunks_exact(2).enumerate() {
        out[at] = (digit(pair[0])? << 4) | digit(pair[1])?;
    }
    Some(out)
}

/// The command's store as c_tasks' [`CStore`]; the first refusal is kept to be named.
struct OnCStore<'x, 'm> {
    store: &'x mut dyn Store<'m>,
    cx: &'x Cx,
    failed: Option<CliError>,
}

impl CStore for OnCStore<'_, '_> {
    fn get(&mut self, key: ContentId, into: &mut AVec<'_, u8>) -> Result<bool, StoreFailed> {
        into.clear();
        match self.store.get(self.cx, Key(key)) {
            Ok(Some(held)) => {
                into.extend_from_slice(held.bytes);
                Ok(true)
            }
            Ok(None) => Ok(false),
            Err(error) => {
                self.failed.get_or_insert(error);
                Err(StoreFailed)
            }
        }
    }

    fn put(&mut self, key: ContentId, bytes: &[u8]) -> Result<(), StoreFailed> {
        self.store.put(self.cx, Key(key), bytes).map_err(|error| {
            self.failed.get_or_insert(error);
            StoreFailed
        })
    }
}

/// Where a crate's package files come from: either a fetched pack in the sources store, or a
/// checkout directory walked and content-addressed before checking the built-record cache.
enum PkgSource<'s, 'l, 'a> {
    Fetched {
        source: ContentId,
        files: [u8; 32],
    },
    Checkout {
        /// Canonical host path of the package directory on disk.
        host_dir: &'s str,
        /// Checkout-relative package directory (`""` for workspace root, e.g. `"vendor/wasmtime"`).
        rel_dir: &'l str,
        /// Sorted file list and content-addressed BLAKE3 digest of the package folder.
        walk: CheckoutWalk<'a>,
    },
}

/// `script` of crate `r` (`package`) as the image takes it: unchanged when its record holds no
/// deferred request, else the built record's (module doc), found by its key or made now.
/// `Some(report)`: what its requests came to, for the command's line.
///
/// The archives asked for are the script's own link lines' (c_tasks.rs `script_demand`). A
/// crate's `#[link(kind = "static")]` declarations are not added yet: their kind is not named
/// outside blaze-ladder (front2 links.rs `LinkKind` is crate-private); an archive only a
/// declaration asks for is refused by the link by name (natives.rs `StaticNotFound`).
#[expect(clippy::too_many_arguments, reason = "each is one input of the crate's built record, named at the call")]
pub(crate) fn built_for<'s, 'm: 's>(
    cx: &Cx,
    c: &mut CTasks<'_>,
    store: &mut dyn Store<'m>,
    list: &CrateList<'_>,
    r: usize,
    package: &'s str,
    script: ScriptIn<'m>,
    producers: &Producers<'_>,
    scratch: &'s Frame,
) -> Result<(ScriptIn<'s>, Option<Report>), CTaskWhy<'s>> {
    let cx = cx.span("native: a crate's deferred C requests");
    let Some((ran, record)) = crate::scripts::payload_parts(script.payload) else { return Err(CTaskWhy::Outputs) };
    let outputs = OutputsRef::open(record).map_err(|_| CTaskWhy::Outputs)?;
    let requests = outputs.c_requests().count() as u32;
    if requests == 0 {
        return Ok((script, None));
    }
    let alloc = c.heap.alloc();
    let pkg_source = match <CrateList<'_> as CrateTable<'_>>::place(list, r) {
        Place::Fetched { source, files } => PkgSource::Fetched { source, files },
        Place::Checkout | Place::CheckoutDep => {
            let (host_dir, rel_dir) = resolve_checkout_pkg_dir(c.checkout_root, list, r, scratch)?;
            let walk = walk_checkout_pkg(host_dir, CheckoutLimits::DEFAULT, alloc, scratch)?;
            PkgSource::Checkout { host_dir, rel_dir, walk }
        }
        Place::Toolchain => return Err(CTaskWhy::NotPinned { why: "a toolchain crate with C requests" }),
        Place::Unplaced { why } => return Err(CTaskWhy::NotPinned { why }),
    };
    let (pinned, package_guest): (Pinned, &'s str) = match &pkg_source {
        PkgSource::Fetched { source, files } => (Pinned { source: source.0, files: *files }, blaze_expand::seen::PACKAGE_ROOT),
        PkgSource::Checkout { rel_dir, walk, .. } => {
            let seen = SeenRoot::member(rel_dir).map_err(|e| CTaskWhy::Lay { words: kept(scratch, format_args!("{e}")) })?;
            let guest = kept(scratch, format_args!("{seen}"));
            (Pinned { source: walk.digest.0, files: walk.digest.0 }, guest)
        }
    };
    let demand = c_tasks::script_demand(&outputs, alloc);
    let mut demand_text: AVec<'_, &str> = Vec::with_capacity_in(demand.len(), alloc);
    demand_text.extend(demand.iter().map(|d| d.as_str()));
    let (compiler, sdk_key) = {
        let setup = c.setup(scratch)?;
        (setup.compiler, setup.sdk_key)
    };
    let toolchain = Toolchain { kind: CompilerKind::ShelfClang, compiler, sdk: &sdk_key, target: crate::graph::TRIPLE };
    // A crate with process-tree requests (`sh configure`, `make`): the host tools the table
    // grants it and the OS build, read and keyed once (native_c_tasks.rs `tree_tools`); their
    // digest is in the built key, so a changed tool is another built record (rule 21).
    let has_tree = outputs.c_requests().any(|text| c_tasks::is_tree(text.split('\0').next().unwrap_or("")));
    let erased_scratch = blaze_own::Erased::new(scratch);
    let trees = match has_tree {
        false => None,
        true => {
            let shelf = c.setup(scratch)?.setting.shelf.as_str();
            // The table's `needed_by` names PACKAGES (`tikv-jemalloc-sys`), as the script
            // runner's grant does (native_unit.rs): never the crate's name (`tikv_jemalloc_sys`,
            // which `package` is here, for messages), which matched no row (the first real run:
            // "`sh` is not a host tool the table grants this package", with `sh` in the table).
            let tools = native_c_tasks::tree_tools(list.package(r), crate::graph::TRIPLE, shelf, erased_scratch.bump())
                .map_err(|words| CTaskWhy::Setup { words: kept(scratch, format_args!("the C of {package}: its process tree's host tools: {}", words.as_str())) })?;
            Some(tools)
        }
    };
    let tree_digest = trees.as_ref().map(native_c_tasks::tree_tools_digest);
    let roots = GuestRoots { package: package_guest, out: outputs.out_dir(), builds: GUEST_BUILDS, deps: DEPS_ROOT, sysroot: GUEST_SDK };
    // Each `links` dependency's tree the requests name (librocksdb-sys: zstd-sys's `OUT_DIR`,
    // `/deps/zstd`), found and keyed before anything is laid: a hit lays nothing.
    let named = c_tasks::dep_roots(&outputs, &roots, alloc);
    let mut graph_links: Option<AVec<'_, (AString<'_>, AString<'_>, AString<'_>)>> = None;
    let mut deps: AVec<'_, DepTree<'_, 's, '_>> = Vec::with_capacity_in(named.len(), alloc);
    for guest in named.iter().copied() {
        deps.push(dep_tree(c.checkout_root, list, producers, guest, &mut graph_links, alloc, scratch)?);
    }
    let mut dep_keys: AVec<'_, (&str, [u8; 32])> = Vec::with_capacity_in(deps.len(), alloc);
    dep_keys.extend(deps.iter().map(|dep| (dep.guest, dep.digest)));
    dep_keys.sort_unstable_by(|a, b| a.0.cmp(b.0));
    let key = c_tasks_built::built_key(script.out, &toolchain, pinned, &demand_text, &dep_keys, tree_digest);

    // One get: the built record, read in place.
    let held = store.get(&cx, Key(key)).map_err(|e| CTaskWhy::Built { words: kept(scratch, format_args!("the store: {e}")) })?;
    if let Some(held) = held {
        let Some(payload) = c_tasks_built::open_built(held.bytes) else {
            return Err(CTaskWhy::Built { words: kept(scratch, format_args!("the record at {key:?} is no built record (c_tasks_built.rs `BUILT_MAGIC`)")) });
        };
        if built_stored_files_present(c.home, payload, alloc) {
            let report = Report { requests, built_hit: true, ..Report::default() };
            return Ok((ScriptIn { name: script.name, out: key, payload }, Some(report)));
        }
    }

    // A miss: the package laid, the tasks run, the built record put.
    let (package_dir, package_digest) = match &pkg_source {
        PkgSource::Fetched { files, .. } => lay_package(&cx, c, list, r, *files, scratch)?,
        PkgSource::Checkout { host_dir, walk, .. } => {
            let work = c.setup.as_ref().expect("native_runner_c: set up before a package is laid").setting.work.as_str();
            let host = lay_checkout_snapshot(work, host_dir, walk, alloc, scratch)?;
            (host, walk.digest.0)
        }
    };
    let mut sources: AVec<'_, Laid<'_>> = Vec::with_capacity_in(1 + deps.len(), alloc);
    sources.push(Laid { guest: package_guest, host: package_dir, digest: package_digest, name: "the package" });
    // Each read-only: granted read, never written (native_c.rs `Laid`).
    for dep in deps.iter() {
        let (host, name) = match &dep.source {
            DepSource::Out { script } => (lay_dep_out(c, *script, dep.digest, scratch)?, "a `links` dependency's OUT_DIR"),
            DepSource::FetchedTree { r: producer, files } => {
                let (host, digest) = lay_package(&cx, c, list, *producer, *files, scratch)?;
                if digest != dep.digest {
                    return Err(CTaskWhy::Lay { words: kept(scratch, format_args!("{}: its package laid under another digest than it was keyed by", dep.guest)) });
                }
                (host, "a `links` dependency's package")
            }
            DepSource::CheckoutTree { host_dir, walk } => {
                let work = c.setup.as_ref().expect("native_runner_c: set up before a dependency is laid").setting.work.as_str();
                let host = lay_checkout_snapshot(work, host_dir, walk, alloc, scratch)?;
                (host, "a `links` dependency's package")
            }
        };
        sources.push(Laid { guest: dep.guest, host, digest: dep.digest, name });
    }
    let setup = c.setup.as_ref().expect("native_runner_c: set up above");
    let setup_shelf = setup.setting.shelf.as_str();
    let setting = CTasksSetting {
        clang: setup.clang.as_str(),
        shelf: Laid { guest: GUEST_SHELF, host: setup_shelf, digest: setup.compiler, name: "the pinned clang's shelf" },
        sdk: Laid { guest: GUEST_SDK, host: setup.setting.sdk.as_str(), digest: setup.sdk_digest, name: "the macOS SDK" },
        work: setup.setting.work.as_str(),
        area: setup.area.as_str(),
        workers: c.workers,
        deadline: COMPILE_DEADLINE,
        heap: c.heap,
    };
    let mut on = OnCStore { store, cx: &cx, failed: None };
    let trees = trees.unwrap_or(native_c_tasks::TreeTools { tools: &[], keys: &[], os_build: "" });
    let ran_tasks = native_c_tasks::run_tasks(&setting, package, &outputs, &sources, roots, &demand_text, &toolchain, &trees, &mut on, alloc);
    let run = match ran_tasks {
        Ok(run) => run,
        Err(error) => {
            return Err(CTaskWhy::Tasks {
                words: match on.failed.take() {
                    Some(store) => kept(scratch, format_args!("{error}: {store}")),
                    None => kept(scratch, format_args!("{error}")),
                },
            });
        }
    };
    let built = c_tasks::built(&outputs, &run.made, &run.tree, alloc).map_err(|e| CTaskWhy::Built { words: kept(scratch, format_args!("its archives as stored files: {e:?}")) })?;
    // The built record: the magic, then a `Script` payload (how it ran, the outputs).
    let mut body: AVec<'_, u8> = Vec::new_in(alloc);
    body.push(ran as u8);
    let mut encoded = Vec::new_in(alloc);
    built.encode(&mut encoded);
    body.extend_from_slice(&encoded);
    let mut bytes = Vec::new_in(alloc);
    c_tasks_built::encode_built(&body, &mut bytes);
    on.store.put(&cx, Key(key), &bytes).map_err(|e| CTaskWhy::Built { words: kept(scratch, format_args!("the store: {e}")) })?;
    let payload: &'s [u8] = scratch.alloc_slice_copy(&body);
    Ok((ScriptIn { name: script.name, out: key, payload }, Some(run.report)))
}

/// Whether every stored file referenced by a cached built-C `payload` still exists in
/// `<home>/.blaze/v2/build-script/files/<sha256>` with its recorded length.
/// Auxiliary GC (`gc.rs`) evicts `v2/build-script/files` independently of the KV store, so a
/// built record whose archive or process-tree blob was evicted must be treated as a cache miss.
fn built_stored_files_present(home: Option<&str>, payload: &[u8], alloc: Alloc<'_>) -> bool {
    let Some((_, record)) = crate::scripts::payload_parts(payload) else {
        return false;
    };
    let Ok(outputs) = OutputsRef::open(record) else {
        return false;
    };
    let mut stored = outputs.stored();
    let Some(first) = stored.next() else {
        return true;
    };
    let Some(home) = home else {
        return false;
    };
    let mut area_folder = String::new_in(alloc);
    area_folder.push_str(home);
    area_folder.push('/');
    area_folder.push_str(crate::scripts::STORE_UNDER_HOME);
    area_folder.push('/');
    area_folder.push_str(blaze_build_script::outputs::FILE_AREA);
    let area = blaze_proc_macro::native_c::FileArea { folder: area_folder.as_str() };
    use blaze_build_script::c_tasks::{Area as _, FileRef};
    core::iter::once(first)
        .chain(stored)
        .all(|file| area.verify(FileRef { sha256: *file.sha256, len: file.len }).is_some())
}

/// What the image holds for its readers' `links` trees (engine_native.rs `image_for`): every
/// crate's script record as its script ran (`scripts`), the record each crate's C step has
/// already given (`done`: the built record when its C ran, else the run's own), and the unit
/// graph record (graph_payload.rs `Sections::graph`), whose units name each package's `links`; it
/// is decoded only when a reader names `/deps/<links>`.
///
/// THE ORDER: `image_for` runs the C step over its closure REVERSED (native_image.rs `closure`
/// is a post-order of the crate list's dependency edges, reversed), so each crate's C runs after
/// every crate it depends on; a `links` producer whose `DEP_<LINKS>_*` a reader's script read is
/// one of the reader's direct dependencies (the cargo book, "links": the metadata goes to the
/// dependents' scripts), so it is in `done` when its reader asks. One that is not is refused by
/// name ([`DepOut::NotYet`]), never laid from its deferred record.
///
/// `refused`: the crates whose C step was refused (engine_native.rs `image_for` leaves their C
/// out of the image): a reader of one's `OUT_DIR` is refused naming it ([`DepOut::Refused`]),
/// so its C is left out too, never built against a deferred record.
pub(crate) struct Producers<'p> {
    pub(crate) scripts: &'p [(usize, ScriptIn<'p>)],
    pub(crate) done: &'p [(usize, ScriptIn<'p>)],
    pub(crate) refused: &'p [usize],
    pub(crate) graph: &'p [u8],
}

/// One `links` dependency's tree a reader's requests name: its guest spelling, what names it
/// in the built key, and where its files come from.
struct DepTree<'d, 's, 'a> {
    guest: &'s str,
    digest: [u8; 32],
    source: DepSource<'d, 's, 'a>,
}

enum DepSource<'d, 's, 'a> {
    /// A producer's `OUT_DIR` (`<builds>/<name>-<config>/out`) as its C step left it: its built
    /// record's files (what its process tree made: configure's and make's headers among them).
    Out { script: ScriptIn<'d> },
    /// The tree a fetched producer was served (`/deps/<links>`): crate `r`'s package, by its files.
    FetchedTree { r: usize, files: [u8; 32] },
    /// The tree a checkout producer was served (`/deps/<links>` or `/deps/<links>/<rel_dir>`):
    /// hashed from disk and snapshotted atomically under `<work>/pkg/<hex>` on a miss.
    CheckoutTree { host_dir: &'s str, walk: CheckoutWalk<'a> },
}

/// Why no producer's `OUT_DIR` is found for a reader's spelling ([`dep_out`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DepOut {
    /// No build script the program links ran with that `OUT_DIR`.
    NoProducer,
    /// Crate `r`'s script ran with it, and its C step has not run yet: the order broke
    /// ([`Producers`]), and its deferred record lacks what its C makes.
    NotYet { r: usize },
    /// Crate `r`'s C step ran and was refused: what its reader needs of it was never made.
    Refused { r: usize },
    /// The producer's record does not open as a script's outputs.
    Record { r: usize },
}

/// The producer of the `OUT_DIR` spelled `guest` as its C step left it, and the digest that names
/// it in a reader's built key: the record's `crate_digest` (its `OUT_DIR`, cfgs, env and files,
/// each stored file by its SHA-256), the CONTENT of what the reader is laid, not the producer's
/// key. So a producer whose C reran to equal files keeps its readers' built records, and one
/// whose files moved (a header its `configure` wrote) moves them (rule 21). Sans IO: the records
/// are read in place.
pub(crate) fn dep_out<'p>(producers: &Producers<'p>, guest: &str) -> Result<(ScriptIn<'p>, [u8; 32]), DepOut> {
    let out_dir_of = |script: &ScriptIn<'p>| crate::scripts::payload_parts(script.payload).and_then(|(_, record)| OutputsRef::open(record).ok());
    let Some(&(r, _)) = producers.scripts.iter().find(|(_, script)| out_dir_of(script).is_some_and(|opened| opened.out_dir() == guest)) else {
        return Err(DepOut::NoProducer);
    };
    if producers.refused.contains(&r) {
        return Err(DepOut::Refused { r });
    }
    let Some(&(_, built)) = producers.done.iter().find(|(at, _)| *at == r) else { return Err(DepOut::NotYet { r }) };
    let opened = out_dir_of(&built).ok_or(DepOut::Record { r })?;
    Ok((built, opened.crate_digest().0))
}

/// The producer of `guest` (c_tasks.rs `dep_roots`), keyed: an `OUT_DIR` by the script record
/// whose `OUT_DIR` it is (one per package and config), a `/deps/<links>` tree by the package
/// whose `links` it is (one per graph, the cargo book's "links") — fetched pack or checkout
/// package. Missing or unplaced producers are refused naming the spelling (rule 3); nothing is
/// guessed.
#[expect(clippy::too_many_arguments, reason = "each is one input for locating and keying a links dep tree")]
fn dep_tree<'d, 's, 'a>(
    checkout_root: Option<&str>,
    list: &CrateList<'_>,
    producers: &Producers<'d>,
    guest: &'s str,
    graph_links: &mut Option<AVec<'a, (AString<'a>, AString<'a>, AString<'a>)>>,
    alloc: Alloc<'a>,
    scratch: &'s Frame,
) -> Result<DepTree<'d, 's, 'a>, CTaskWhy<'s>> {
    let lay = |what: fmt::Arguments<'_>| CTaskWhy::Lay { words: kept(scratch, what) };
    let Some(links) = guest.strip_prefix(DEPS_ROOT).and_then(|rest| rest.strip_prefix('/')) else {
        return match dep_out(producers, guest) {
            Ok((script, digest)) => Ok(DepTree { guest, digest, source: DepSource::Out { script } }),
            Err(DepOut::NoProducer) => Err(lay(format_args!("a request names `{guest}`, the OUT_DIR of no build script this program links"))),
            Err(DepOut::NotYet { r }) => Err(lay(format_args!(
                "a request names `{guest}`, the OUT_DIR of `{}`, whose C step has not run before its reader's (engine_native.rs `image_for` runs dependencies first)",
                list.name(r)
            ))),
            Err(DepOut::Refused { r }) => Err(lay(format_args!("a request names `{guest}`, the OUT_DIR of `{}`, whose own C step was refused (said above)", list.name(r)))),
            Err(DepOut::Record { r }) => Err(lay(format_args!("a request names `{guest}`, and the record `{}` left does not open as a script's outputs", list.name(r)))),
        };
    };
    if graph_links.is_none() {
        let decoded = blaze_unit_graph::record::decode(producers.graph, alloc).map_err(|why| lay(format_args!("the unit graph, for `{guest}`: {why:?}")))?;
        let mut rows: AVec<'a, (AString<'a>, AString<'a>, AString<'a>)> = Vec::new_in(alloc);
        for unit in decoded.graph.units.iter() {
            if let Some(of) = unit.links.as_ref() {
                rows.push((String::from_str_in(of.as_str(), alloc), String::from_str_in(unit.name.as_str(), alloc), String::from_str_in(unit.version.as_str(), alloc)));
            }
        }
        *graph_links = Some(rows);
    }
    let rows = graph_links.as_ref().map_or(&[][..], |rows| rows.as_slice());
    let Some((_, name, version)) = rows.iter().find(|(of, _, _)| of.as_str() == links) else {
        return Err(lay(format_args!("a request names `{guest}`, and no package of the graph has `links = \"{links}\"`")));
    };
    let Some(at) = (0..list.len()).find(|&at| list.package(at) == name.as_str() && list.version(at) == version.as_str()) else {
        return Err(lay(format_args!(
            "a request names `{guest}`, and `{} {}` (`links = \"{links}\"`) is not in this program's crate list",
            name.as_str(),
            version.as_str()
        )));
    };
    match <CrateList<'_> as CrateTable<'_>>::place(list, at) {
        Place::Fetched { files, .. } => Ok(DepTree { guest, digest: package_digest(list, at, files).0, source: DepSource::FetchedTree { r: at, files } }),
        Place::Checkout | Place::CheckoutDep => {
            let (host_dir, rel_dir) = resolve_checkout_pkg_dir(checkout_root, list, at, scratch)?;
            let walk = walk_checkout_pkg(host_dir, CheckoutLimits::DEFAULT, alloc, scratch)?;
            let laid_guest = if rel_dir.is_empty() { guest } else { kept(scratch, format_args!("{guest}/{rel_dir}")) };
            let digest = walk.digest.0;
            Ok(DepTree { guest: laid_guest, digest, source: DepSource::CheckoutTree { host_dir, walk } })
        }
        Place::Toolchain => Err(lay(format_args!(
            "a request names `{guest}`, and `{} {}` (`links = \"{links}\"`) is a toolchain crate",
            name.as_str(),
            version.as_str()
        ))),
        Place::Unplaced { why } => Err(lay(format_args!(
            "a request names `{guest}`, and `{} {}` (`links = \"{links}\"`) is unplaced: {why}",
            name.as_str(),
            version.as_str()
        ))),
    }
}

/// A producer's `OUT_DIR` laid read-only from the record its C step left ([`dep_out`]:
/// `<work>/dep/<its content digest>`, made once:
/// written beside and renamed into place), its real path.
fn lay_dep_out<'s>(c: &CTasks<'_>, script: ScriptIn<'_>, key: [u8; 32], scratch: &'s Frame) -> Result<&'s str, CTaskWhy<'s>> {
    let lay = |what: fmt::Arguments<'_>| CTaskWhy::Lay { words: kept(scratch, what) };
    let hex = crate::fetch::key_hex(&ContentId(key));
    let hex = core::str::from_utf8(&hex).map_err(|_| lay(format_args!("a digest's hex is not text")))?;
    let setup = c.setup.as_ref().expect("native_runner_c: set up before a dependency is laid");
    let folder = bstd::mem::format_in!(scratch, "{}/dep/{hex}", setup.setting.work.as_str());
    let folder: &'s str = crate::work::held_str(folder.as_str(), scratch);
    if !std::fs::metadata(folder).is_ok_and(|m| m.is_dir()) {
        let (opened, area) = crate::native_image::script_outputs(script.payload, c.home).map_err(|why| lay(format_args!("a `links` dependency's record: {why:?}")))?;
        let opened = opened.with_area(&area);
        let part = bstd::mem::format_in!(scratch, "{folder}.part-{}", std::process::id());
        match std::fs::remove_dir_all(part.as_str()) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(lay(format_args!("{}: {error}", part.as_str()))),
        }
        std::fs::create_dir_all(part.as_str()).map_err(|e| lay(format_args!("{}: {e}", part.as_str())))?;
        for (name, file) in opened.all_files() {
            if name.split('/').any(|part| part.is_empty() || part == "." || part == "..") {
                return Err(lay(format_args!("a `links` dependency's record names `{name}`, which is not a path under OUT_DIR")));
            }
            let bytes = opened.bytes_of(file).map_err(|e| lay(format_args!("{}/{name}: {e}", opened.out_dir())))?;
            let path = bstd::mem::format_in!(scratch, "{}/{name}", part.as_str());
            if let Some((parent, _)) = path.as_str().rsplit_once('/') {
                std::fs::create_dir_all(parent).map_err(|e| lay(format_args!("{parent}: {e}")))?;
            }
            std::fs::write(path.as_str(), bytes).map_err(|e| lay(format_args!("{}: {e}", path.as_str())))?;
        }
        match std::fs::rename(part.as_str(), folder) {
            Ok(()) => {}
            Err(_) if std::fs::metadata(folder).is_ok_and(|m| m.is_dir()) => {
                std::fs::remove_dir_all(part.as_str()).map_err(|e| lay(format_args!("{}: {e}", part.as_str())))?;
            }
            Err(error) => return Err(lay(format_args!("{folder}: {error}"))),
        }
    }
    let real = std::fs::canonicalize(folder).map_err(|e| lay(format_args!("{folder}: {e}")))?;
    let real = real.to_str().ok_or_else(|| lay(format_args!("{folder}: its real path is not UTF-8")))?;
    Ok(crate::work::held_str(real, scratch))
}

/// What names crate `r`'s laid package: the pack's files and the package's folder in it.
fn package_digest(list: &CrateList<'_>, r: usize, files: [u8; 32]) -> ContentId {
    let shown = list.shown_from(r);
    let dir = shown.strip_prefix('/').and_then(|rest| rest.split_once('/')).map_or("", |(_, dir)| dir.trim_end_matches('/'));
    let mut digest = Hasher::new("blaze-cli c tasks laid package 1");
    digest.update(&files).update(&(dir.len() as u64).to_le_bytes()).update(dir.as_bytes());
    digest.finish()
}

/// Crate `r`'s package laid from its pack under the work folder (made once: written beside and
/// renamed into place, so a folder there is whole), and its digest (the pack's files and the
/// package's folder in it).
fn lay_package<'s>(cx: &Cx, c: &mut CTasks<'_>, list: &CrateList<'_>, r: usize, files: [u8; 32], scratch: &'s Frame) -> Result<(&'s str, [u8; 32]), CTaskWhy<'s>> {
    let lay = |what: fmt::Arguments<'_>| CTaskWhy::Lay { words: kept(scratch, what) };
    let root = list.root(r);
    let k: usize = root.strip_prefix('/').and_then(|rest| rest.split('/').next()).and_then(|k| k.parse().ok()).ok_or_else(|| lay(format_args!("its root `{root}` names no tree")))?;
    let Some(row) = list.tree(k) else { return Err(lay(format_args!("the crate list names no tree {k}"))) };
    // The package's folder in its pack: `/<k>/<dir>/` (a git commit's), `/<k>/` (a crate's).
    let shown = list.shown_from(r);
    let dir = shown.strip_prefix('/').and_then(|rest| rest.split_once('/')).map_or("", |(_, dir)| dir.trim_end_matches('/'));
    let digest = package_digest(list, r, files);
    let hex = crate::fetch::key_hex(&digest);
    let hex = core::str::from_utf8(&hex).map_err(|_| lay(format_args!("a digest's hex is not text")))?;
    let setup = c.setup.as_ref().expect("native_runner_c: set up before a package is laid");
    let folder = bstd::mem::format_in!(scratch, "{}/pkg/{hex}", setup.setting.work.as_str());
    let folder: &'s str = crate::work::held_str(folder.as_str(), scratch);
    if std::fs::metadata(folder).is_ok_and(|m| m.is_dir()) {
        return Ok((folder, digest.0));
    }
    let from = PackHome { home: c.home, fs: c.fs, heap: c.heap };
    let view = open_row(cx, from, row, k, &mut c.dirs, scratch).map_err(|words| lay(format_args!("{words}")))?;
    if view.tree().0 != files {
        return Err(lay(format_args!("its pack holds other files than its crate list row names")));
    }
    let part = bstd::mem::format_in!(scratch, "{folder}.part-{}", std::process::id());
    match std::fs::remove_dir_all(part.as_str()) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(lay(format_args!("{}: {error}", part.as_str()))),
    }
    let prefix = if dir.is_empty() { None } else { Some(dir) };
    let mut laid = 0u32;
    for entry in view.entries() {
        let rest = match prefix {
            None => entry.path,
            Some(dir) => match entry.path.strip_prefix(dir).and_then(|rest| rest.strip_prefix('/')) {
                Some(rest) => rest,
                None => continue,
            },
        };
        if rest.split('/').any(|part| part.is_empty() || part == "." || part == "..") {
            return Err(lay(format_args!("its pack names `{}`, which is not a path in a folder", entry.path)));
        }
        let bytes = entry.read_in(scratch).map_err(|e| lay(format_args!("{}: {}", entry.path, blaze_text::Shown(&e))))?;
        let path = bstd::mem::format_in!(scratch, "{}/{rest}", part.as_str());
        if let Some((parent, _)) = path.as_str().rsplit_once('/') {
            std::fs::create_dir_all(parent).map_err(|e| lay(format_args!("{parent}: {e}")))?;
        }
        std::fs::write(path.as_str(), bytes.as_slice()).map_err(|e| lay(format_args!("{}: {e}", path.as_str())))?;
        laid += 1;
    }
    if laid == 0 {
        return Err(lay(format_args!("its pack holds no file under `{dir}`")));
    }
    match std::fs::rename(part.as_str(), folder) {
        Ok(()) => {}
        // Another command laid it first: its folder is whole, and this one goes.
        Err(_) if std::fs::metadata(folder).is_ok_and(|m| m.is_dir()) => {
            std::fs::remove_dir_all(part.as_str()).map_err(|e| lay(format_args!("{}: {e}", part.as_str())))?;
        }
        Err(error) => return Err(lay(format_args!("{folder}: {error}"))),
    }
    let real = std::fs::canonicalize(folder).map_err(|e| lay(format_args!("{folder}: {e}")))?;
    let real = real.to_str().ok_or_else(|| lay(format_args!("{folder}: its real path is not UTF-8")))?;
    Ok((crate::work::held_str(real, scratch), digest.0))
}

/// Bounds enforced while walking and snapshotting a checkout package folder (rule 18).
#[derive(Clone, Copy, Debug)]
struct CheckoutLimits {
    max_depth: usize,
    max_dirs: usize,
    max_files: usize,
    max_bytes: u64,
}

impl CheckoutLimits {
    const DEFAULT: CheckoutLimits = CheckoutLimits {
        max_depth: CHECKOUT_MAX_DEPTH,
        max_dirs: CHECKOUT_MAX_DIRS,
        max_files: CHECKOUT_MAX_FILES,
        max_bytes: CHECKOUT_MAX_BYTES,
    };
}

/// The sorted relative file list and content-addressed BLAKE3 digest of a checkout package folder.
struct CheckoutWalk<'a> {
    /// Relative `/`-separated file paths under the package root, sorted in ascending byte order.
    files: AVec<'a, AString<'a>>,
    /// Content-addressed BLAKE3 digest (`CHECKOUT_PKG_DOMAIN`) over sorted file entries.
    digest: ContentId,
    /// Limits used for the walk (reused when verifying during snapshot lay).
    limits: CheckoutLimits,
}

/// Validates a non-empty `/`-separated relative path with no traversal, backslashes, or NULs.
fn validate_rel_path(path: &str) -> Result<(), &'static str> {
    if path.is_empty() {
        return Err("empty relative path");
    }
    if path.starts_with('/') || path.ends_with('/') {
        return Err("relative path must not start or end with `/`");
    }
    if path.contains('\\') {
        return Err("relative path must not contain `\\`");
    }
    if path.contains('\0') {
        return Err("relative path must not contain NUL");
    }
    for part in path.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return Err("relative path contains an invalid component (`.`, `..`, or empty)");
        }
    }
    Ok(())
}

/// Extracts the validated checkout-relative package directory from a tree-0 `shown_from` spelling
/// (`"/0/"` or `"/0"` -> `""`, `"/0/vendor/wasmtime/"` -> `"vendor/wasmtime"`).
fn checkout_rel_dir(shown_from: &str) -> Result<&str, &'static str> {
    let rest = shown_from.strip_prefix("/0").ok_or("its shown_from does not start with `/0`")?;
    let rel = match rest {
        "" => "",
        _ => {
            let after = rest.strip_prefix('/').ok_or("its shown_from does not name tree 0 (`/0/...`)")?;
            after.strip_suffix('/').unwrap_or(after)
        }
    };
    if !rel.is_empty() {
        validate_rel_path(rel)?;
    }
    Ok(rel)
}

/// Resolves checkout crate `r`'s package directory on disk from `checkout_root` and
/// `list.shown_from(r)`, verifying it is a canonical directory strictly within `checkout_root`.
/// Returns `(canonical_pkg_dir, checkout_rel_dir)`.
fn resolve_checkout_pkg_dir<'s, 'l>(
    checkout_root: Option<&str>,
    list: &CrateList<'l>,
    r: usize,
    scratch: &'s Frame,
) -> Result<(&'s str, &'l str), CTaskWhy<'s>> {
    let lay = |what: fmt::Arguments<'_>| CTaskWhy::Lay { words: kept(scratch, what) };
    let Some(checkout_root) = checkout_root else {
        return Err(lay(format_args!(
            "`{}` is a checkout crate, and no host checkout root was resolved for this command",
            list.package(r)
        )));
    };
    let shown = list.shown_from(r);
    let rel = checkout_rel_dir(shown).map_err(|why| lay(format_args!("`{}` has invalid shown_from `{shown}`: {why}", list.package(r))))?;
    let real_checkout = std::fs::canonicalize(checkout_root).map_err(|e| lay(format_args!("checkout root `{checkout_root}`: {e}")))?;
    let joined = if rel.is_empty() { real_checkout.clone() } else { real_checkout.join(rel) };
    let real_pkg = std::fs::canonicalize(&joined).map_err(|e| lay(format_args!("checkout package `{}` at `{}`: {e}", list.package(r), joined.display())))?;
    if !real_pkg.starts_with(&real_checkout) {
        return Err(lay(format_args!(
            "checkout package `{}` (`{rel}`) resolves outside checkout root `{checkout_root}`",
            list.package(r)
        )));
    }
    if !std::fs::metadata(&real_pkg).is_ok_and(|m| m.is_dir()) {
        return Err(lay(format_args!("checkout package `{}` at `{}` is not a directory", list.package(r), real_pkg.display())));
    }
    let real_str = real_pkg.to_str().ok_or_else(|| lay(format_args!("checkout package `{}`: its real path is not UTF-8", list.package(r))))?;
    Ok((crate::work::held_str(real_str, scratch), rel))
}

/// Streaming buffer, byte budget, and package-level BLAKE3 hasher shared between checkout hashing
/// and snapshot verification so both hash identical bytes under one definition (rule 30).
struct CheckoutHasher<'b> {
    buf: &'b mut [u8],
    total_bytes: u64,
    max_bytes: u64,
    pkg: Hasher,
}

impl<'b> CheckoutHasher<'b> {
    fn new(file_count: usize, buf: &'b mut [u8], max_bytes: u64) -> CheckoutHasher<'b> {
        let mut pkg = Hasher::new(CHECKOUT_PKG_DOMAIN);
        let count = u64::try_from(file_count).unwrap_or(u64::MAX);
        pkg.update(&count.to_le_bytes());
        CheckoutHasher { buf, total_bytes: 0, max_bytes, pkg }
    }

    fn feed_file<'s>(&mut self, src_path: &str, rel: &str, mut dest: Option<(&str, &mut std::fs::File)>, scratch: &'s Frame) -> Result<(), CTaskWhy<'s>> {
        let lay = |what: fmt::Arguments<'_>| CTaskWhy::Lay { words: kept(scratch, what) };
        let mut file = std::fs::File::open(src_path).map_err(|e| lay(format_args!("{src_path}: {e}")))?;
        let meta = file.metadata().map_err(|e| lay(format_args!("{src_path}: {e}")))?;
        if !meta.is_file() {
            return Err(lay(format_args!("{src_path}: expected a regular file")));
        }
        #[cfg(unix)]
        let executable = {
            use std::os::unix::fs::PermissionsExt as _;
            (meta.permissions().mode() & 0o111) != 0
        };
        #[cfg(not(unix))]
        let executable = false;

        let mut file_hasher = Hasher::new(CHECKOUT_FILE_DOMAIN);
        let mut file_len = 0u64;
        loop {
            let n = file.read(self.buf).map_err(|e| lay(format_args!("{src_path}: {e}")))?;
            if n == 0 {
                break;
            }
            let chunk = &self.buf[..n];
            let n_u64 = u64::try_from(n).unwrap_or(u64::MAX);
            self.total_bytes = self.total_bytes.saturating_add(n_u64);
            if self.total_bytes > self.max_bytes {
                return Err(lay(format_args!("{src_path}: checkout package exceeds byte budget of {} bytes", self.max_bytes)));
            }
            file_len = file_len.saturating_add(n_u64);
            file_hasher.update(chunk);
            if let Some((dst_path, dst_file)) = dest.as_mut() {
                dst_file.write_all(chunk).map_err(|e| lay(format_args!("{dst_path}: {e}")))?;
            }
        }
        #[cfg(unix)]
        if executable && let Some((dst_path, dst_file)) = dest.as_mut() {
            use std::os::unix::fs::PermissionsExt as _;
            let mut perms = dst_file.metadata().map_err(|e| lay(format_args!("{dst_path}: {e}")))?.permissions();
            perms.set_mode(perms.mode() | 0o111);
            dst_file.set_permissions(perms).map_err(|e| lay(format_args!("{dst_path}: {e}")))?;
        }
        let file_digest = file_hasher.finish();
        self.pkg.part(rel.as_bytes()).update(&[u8::from(executable)]).update(&file_len.to_le_bytes()).id(&file_digest);
        Ok(())
    }

    fn finish(self) -> ContentId {
        self.pkg.finish()
    }
}

/// Deterministically walks `pkg_dir` using a bounded iterative worklist (skipping `.git`,
/// `target`, and `.blaze`, refusing symlinks that escape `pkg_dir`, directory cycles, and
/// invalid path components), sorts relative file paths, and computes a content-addressed BLAKE3
/// digest over `(rel_path, executable, len, file_bytes)`.
fn walk_checkout_pkg<'a, 's>(pkg_dir: &str, limits: CheckoutLimits, alloc: Alloc<'a>, scratch: &'s Frame) -> Result<CheckoutWalk<'a>, CTaskWhy<'s>> {
    let lay = |what: fmt::Arguments<'_>| CTaskWhy::Lay { words: kept(scratch, what) };
    let real_pkg = std::fs::canonicalize(pkg_dir).map_err(|e| lay(format_args!("{pkg_dir}: {e}")))?;
    if !std::fs::metadata(&real_pkg).is_ok_and(|m| m.is_dir()) {
        return Err(lay(format_args!("{pkg_dir}: not a directory")));
    }
    let real_pkg_str = real_pkg.to_str().ok_or_else(|| lay(format_args!("{pkg_dir}: its real path is not UTF-8")))?;

    let mut worklist: AVec<'a, (AString<'a>, usize)> = Vec::new_in(alloc);
    worklist.push((String::new_in(alloc), 0));
    let mut dirs_queued: usize = 1;
    let mut files: AVec<'a, AString<'a>> = Vec::new_in(alloc);
    let mut host_dir_buf = String::new_in(alloc);

    while let Some((rel_dir, depth)) = worklist.pop() {
        host_dir_buf.clear();
        host_dir_buf.push_str(real_pkg_str);
        if !rel_dir.is_empty() {
            host_dir_buf.push('/');
            host_dir_buf.push_str(rel_dir.as_str());
        }
        let host_dir_real = std::fs::canonicalize(host_dir_buf.as_str()).map_err(|e| lay(format_args!("{}: {e}", host_dir_buf.as_str())))?;
        if !host_dir_real.starts_with(&real_pkg) {
            return Err(lay(format_args!("`{}` resolves outside checkout package root `{pkg_dir}`", rel_dir.as_str())));
        }
        let read_dir = std::fs::read_dir(&host_dir_real).map_err(|e| lay(format_args!("{}: {e}", host_dir_buf.as_str())))?;
        for entry_res in read_dir {
            let entry = entry_res.map_err(|e| lay(format_args!("{}: {e}", host_dir_buf.as_str())))?;
            let os_name = entry.file_name();
            let Some(name) = os_name.to_str() else {
                return Err(lay(format_args!("{}: non-UTF-8 entry name", host_dir_buf.as_str())));
            };
            if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\\') || name.contains('\0') {
                return Err(lay(format_args!("{}: invalid entry name `{name}`", host_dir_buf.as_str())));
            }
            let ft = entry.file_type().map_err(|e| lay(format_args!("{}/{name}: {e}", host_dir_buf.as_str())))?;
            let mut rel_path = String::with_capacity_in(rel_dir.len() + 1 + name.len(), alloc);
            if !rel_dir.is_empty() {
                rel_path.push_str(rel_dir.as_str());
                rel_path.push('/');
            }
            rel_path.push_str(name);
            if ft.is_symlink() {
                let resolved = std::fs::canonicalize(entry.path()).map_err(|e| lay(format_args!("symlink `{}` in `{pkg_dir}`: {e}", rel_path.as_str())))?;
                if !resolved.starts_with(&real_pkg) {
                    return Err(lay(format_args!("symlink `{}` in `{pkg_dir}` escapes package root", rel_path.as_str())));
                }
                let target_meta = std::fs::metadata(&resolved).map_err(|e| lay(format_args!("symlink `{}` in `{pkg_dir}`: {e}", rel_path.as_str())))?;
                if target_meta.is_dir() {
                    if name == ".git" || name == "target" || name == ".blaze" {
                        continue;
                    }
                    if host_dir_real.starts_with(&resolved) {
                        return Err(lay(format_args!("symlink `{}` in `{pkg_dir}` creates a directory cycle", rel_path.as_str())));
                    }
                    let next_depth = depth + 1;
                    if next_depth > limits.max_depth {
                        return Err(lay(format_args!(
                            "checkout package `{pkg_dir}` exceeds max directory depth {} at `{}`",
                            limits.max_depth,
                            rel_path.as_str()
                        )));
                    }
                    if dirs_queued >= limits.max_dirs {
                        return Err(lay(format_args!(
                            "checkout package `{pkg_dir}` exceeds max directory count {} at `{}`",
                            limits.max_dirs,
                            rel_path.as_str()
                        )));
                    }
                    dirs_queued += 1;
                    worklist.push((rel_path, next_depth));
                } else if target_meta.is_file() {
                    if name == ".git" {
                        continue;
                    }
                    if files.len() >= limits.max_files {
                        return Err(lay(format_args!(
                            "checkout package `{pkg_dir}` exceeds max file count {} at `{}`",
                            limits.max_files,
                            rel_path.as_str()
                        )));
                    }
                    files.push(rel_path);
                } else {
                    return Err(lay(format_args!("symlink `{}` in `{pkg_dir}` targets a non-regular entry", rel_path.as_str())));
                }
            } else if ft.is_dir() {
                if name == ".git" || name == "target" || name == ".blaze" {
                    continue;
                }
                let next_depth = depth + 1;
                if next_depth > limits.max_depth {
                    return Err(lay(format_args!(
                        "checkout package `{pkg_dir}` exceeds max directory depth {} at `{}`",
                        limits.max_depth,
                        rel_path.as_str()
                    )));
                }
                if dirs_queued >= limits.max_dirs {
                    return Err(lay(format_args!(
                        "checkout package `{pkg_dir}` exceeds max directory count {} at `{}`",
                        limits.max_dirs,
                        rel_path.as_str()
                    )));
                }
                dirs_queued += 1;
                worklist.push((rel_path, next_depth));
            } else if ft.is_file() {
                if name == ".git" {
                    continue;
                }
                if files.len() >= limits.max_files {
                    return Err(lay(format_args!(
                        "checkout package `{pkg_dir}` exceeds max file count {} at `{}`",
                        limits.max_files,
                        rel_path.as_str()
                    )));
                }
                files.push(rel_path);
            } else {
                return Err(lay(format_args!("`{}` in `{pkg_dir}` is neither a regular file nor a directory", rel_path.as_str())));
            }
        }
    }

    if files.is_empty() {
        return Err(lay(format_args!("checkout package `{pkg_dir}` contains no files")));
    }
    files.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));

    let mut buf: AVec<'a, u8> = Vec::new_in(alloc);
    buf.resize(CHECKOUT_READ_CHUNK, 0);
    let mut hasher = CheckoutHasher::new(files.len(), buf.as_mut_slice(), limits.max_bytes);
    let mut src_path = String::new_in(alloc);
    for rel in files.iter() {
        let rel_str = rel.as_str();
        src_path.clear();
        src_path.push_str(real_pkg_str);
        src_path.push('/');
        src_path.push_str(rel_str);
        hasher.feed_file(src_path.as_str(), rel_str, None, scratch)?;
    }
    let digest = hasher.finish();
    Ok(CheckoutWalk { files, digest, limits })
}

/// Populates staging folder `part_str` from `pkg_dir` using `walk.files`, re-hashing the exact
/// bytes written to verify they still match `walk.digest` (rule 21, rule 80).
fn populate_checkout_part<'s>(pkg_dir: &str, part_str: &str, walk: &CheckoutWalk<'_>, alloc: Alloc<'_>, scratch: &'s Frame) -> Result<(), CTaskWhy<'s>> {
    let lay = |what: fmt::Arguments<'_>| CTaskWhy::Lay { words: kept(scratch, what) };
    if walk.files.is_empty() {
        return Err(lay(format_args!("checkout package `{pkg_dir}` contains no files to snapshot")));
    }
    let mut buf: AVec<'_, u8> = Vec::new_in(alloc);
    buf.resize(CHECKOUT_READ_CHUNK, 0);
    let mut verify = CheckoutHasher::new(walk.files.len(), buf.as_mut_slice(), walk.limits.max_bytes);
    let mut src_path = String::new_in(alloc);
    let mut dst_path = String::new_in(alloc);
    for rel in walk.files.iter() {
        let rel_str = rel.as_str();
        validate_rel_path(rel_str).map_err(|why| lay(format_args!("checkout file `{rel_str}`: {why}")))?;
        src_path.clear();
        src_path.push_str(pkg_dir);
        src_path.push('/');
        src_path.push_str(rel_str);
        dst_path.clear();
        dst_path.push_str(part_str);
        dst_path.push('/');
        dst_path.push_str(rel_str);
        if let Some((parent, _)) = dst_path.as_str().rsplit_once('/') {
            std::fs::create_dir_all(parent).map_err(|e| lay(format_args!("{parent}: {e}")))?;
        }
        let mut dst_file = std::fs::File::create(dst_path.as_str()).map_err(|e| lay(format_args!("{}: {e}", dst_path.as_str())))?;
        verify.feed_file(src_path.as_str(), rel_str, Some((dst_path.as_str(), &mut dst_file)), scratch)?;
    }
    if verify.finish() != walk.digest {
        return Err(lay(format_args!(
            "checkout package `{pkg_dir}` changed while laying its snapshot; re-run to snapshot the updated files"
        )));
    }
    Ok(())
}

/// Lays an immutable snapshot of `walk` under `<work>/pkg/<hex>` (written to `<folder>.part-*`
/// and atomically renamed into place, matching [`lay_package`]), returning its canonical path.
/// Reuses `<work>/pkg/<hex>` idempotently when already present.
fn lay_checkout_snapshot<'s>(work: &str, pkg_dir: &str, walk: &CheckoutWalk<'_>, alloc: Alloc<'_>, scratch: &'s Frame) -> Result<&'s str, CTaskWhy<'s>> {
    let lay = |what: fmt::Arguments<'_>| CTaskWhy::Lay { words: kept(scratch, what) };
    let hex = crate::fetch::key_hex(&walk.digest);
    let hex = core::str::from_utf8(&hex).map_err(|_| lay(format_args!("a digest's hex is not text")))?;
    let folder = bstd::mem::format_in!(scratch, "{work}/pkg/{hex}");
    let folder: &'s str = crate::work::held_str(folder.as_str(), scratch);
    if !std::fs::metadata(folder).is_ok_and(|m| m.is_dir()) {
        let seq = SNAPSHOT_PART_SEQ.fetch_add(1, Ordering::Relaxed);
        let part = bstd::mem::format_in!(scratch, "{folder}.part-{}-{seq}", std::process::id());
        let part_str = part.as_str();
        match std::fs::remove_dir_all(part_str) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(lay(format_args!("{part_str}: {error}"))),
        }
        std::fs::create_dir_all(part_str).map_err(|e| lay(format_args!("{part_str}: {e}")))?;
        if let Err(err) = populate_checkout_part(pkg_dir, part_str, walk, alloc, scratch) {
            let _ = std::fs::remove_dir_all(part_str);
            return Err(err);
        }
        match std::fs::rename(part_str, folder) {
            Ok(()) => {}
            // Another command laid it first: its folder is whole, and this one goes.
            Err(_) if std::fs::metadata(folder).is_ok_and(|m| m.is_dir()) => {
                std::fs::remove_dir_all(part_str).map_err(|e| lay(format_args!("{part_str}: {e}")))?;
            }
            Err(error) => {
                let _ = std::fs::remove_dir_all(part_str);
                return Err(lay(format_args!("{folder}: {error}")));
            }
        }
    }
    let real = std::fs::canonicalize(folder).map_err(|e| lay(format_args!("{folder}: {e}")))?;
    let real = real.to_str().ok_or_else(|| lay(format_args!("{folder}: its real path is not UTF-8")))?;
    Ok(crate::work::held_str(real, scratch))
}

#[cfg(test)]
mod dep_out_tests {
    //! A reader's `OUT_DIR` producer as [`dep_out`] finds it: the record its C step left, by the
    //! content of its files. Records are real outputs records (blaze-build-script `Outputs`).

    use blaze_build_script::outputs::{FileLimits, Outputs};
    use blaze_drive::{Kind, Name};

    use super::*;

    const OUT: &str = "/target/debug/build/zstd-sys-1/out";

    /// A `Script` payload (scripts.rs `payload_parts`) of a run with `OUT_DIR` [`OUT`] holding
    /// `files`.
    fn payload(files: &[(&str, &[u8])]) -> &'static [u8] {
        let frame = Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).expect("the outputs' frame");
        let erased = blaze_own::Erased::new(&frame);
        let alloc = erased.handle();
        let mut outputs = Outputs::new_in(OUT, alloc);
        for (name, bytes) in files {
            outputs.add_file(name, bytes, FileLimits::DEFAULT, alloc).expect("a small file");
        }
        let mut record = AVec::new_in(alloc);
        outputs.encode(&mut record);
        let mut bytes = std::vec![1u8];
        bytes.extend_from_slice(&record);
        std::boxed::Box::leak(bytes.into_boxed_slice())
    }

    fn script(n: u8, payload: &'static [u8]) -> ScriptIn<'static> {
        ScriptIn { name: Name::new(Kind::Script, ContentId([n; 32])), out: ContentId([n; 32]), payload }
    }

    /// The producer's C step ran first: the reader is laid what it left (the generated header
    /// `zstd_config.h` its tree made, which the deferred record lacks), named by those files'
    /// content. Twice built to equal files under two keys: one digest (the reader's built record
    /// stays). A header changed: another digest. Control (the order removed): the producer's C
    /// not yet run is refused naming it, never laid from its deferred record; no producer at all
    /// is refused too.
    #[test]
    fn a_reader_is_laid_its_producers_out_dir_after_the_producers_c_ran() {
        let deferred = script(1, payload(&[("include/zstd.h", b"zstd")]));
        let built = script(2, payload(&[("include/zstd.h", b"zstd"), ("include/zstd_config.h", b"#define ZSTD_MT 1")]));
        let scripts = [(4usize, deferred)];
        let done = [(4usize, built)];
        let producers = Producers { scripts: &scripts, done: &done, refused: &[], graph: &[] };
        let (laid, digest) = dep_out(&producers, OUT).expect("the producer is found");
        assert_eq!(laid.out, built.out, "the reader is not laid the record the producer's C step left");
        let opened = OutputsRef::open(crate::scripts::payload_parts(laid.payload).expect("a run").1).expect("outputs");
        assert!(opened.all_files().any(|(name, _)| name == "include/zstd_config.h"), "the tree's header is not laid");

        let again = [(4usize, script(3, payload(&[("include/zstd.h", b"zstd"), ("include/zstd_config.h", b"#define ZSTD_MT 1")])))];
        let equal = dep_out(&Producers { scripts: &scripts, done: &again, refused: &[], graph: &[] }, OUT).expect("found").1;
        assert_eq!(equal, digest, "a producer rebuilt to equal files moved its reader's key");
        let moved = [(4usize, script(5, payload(&[("include/zstd.h", b"zstd"), ("include/zstd_config.h", b"#define ZSTD_MT 0")])))];
        let other = dep_out(&Producers { scripts: &scripts, done: &moved, refused: &[], graph: &[] }, OUT).expect("found").1;
        assert_ne!(other, digest, "a changed generated header did not move its reader's key");

        let early = Producers { scripts: &scripts, done: &[], refused: &[], graph: &[] };
        assert_eq!(dep_out(&early, OUT).map(|(_, d)| d), Err(DepOut::NotYet { r: 4 }), "a producer whose C has not run was laid");
        // A producer whose C was refused: its reader is refused naming it, never laid.
        let failed = Producers { scripts: &scripts, done: &[], refused: &[4], graph: &[] };
        assert_eq!(dep_out(&failed, OUT).map(|(_, d)| d), Err(DepOut::Refused { r: 4 }));
        assert_eq!(dep_out(&producers, "/target/debug/build/lz4-sys-9/out").map(|(_, d)| d), Err(DepOut::NoProducer));
    }
}

#[cfg(test)]
mod checkout_tests {
    use std::path::{Path, PathBuf};

    use super::*;

    static TEST_DIR_SEQ: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> TempDir {
            let seq = TEST_DIR_SEQ.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(std::format!("blaze-c-checkout-{tag}-{}-{seq}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create temp test dir");
            TempDir(dir)
        }

        fn path(&self) -> &Path {
            self.0.as_path()
        }

        fn path_str(&self) -> &str {
            self.0.to_str().expect("temp dir path is UTF-8")
        }

        fn write(&self, rel: &str, bytes: &[u8]) {
            let full = self.0.join(rel);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).expect("create parent dir");
            }
            std::fs::write(&full, bytes).expect("write test file");
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn checkout_package_hash_is_deterministic_and_skips_target_and_git() {
        let tree_a = TempDir::new("hash-a");
        let tree_b = TempDir::new("hash-b");

        // Populate identical logical trees in different creation orders.
        tree_a.write("Cargo.toml", b"[package]\nname = \"demo\"\nversion = \"0.1.0\"\n");
        tree_a.write("src/helpers.c", b"int helper(void) { return 42; }\n");
        tree_a.write("include/api.h", b"int helper(void);\n");
        // Noise in skipped directories must not affect the digest.
        tree_a.write("target/debug/junk.o", b"stale object");
        tree_a.write(".git/HEAD", b"ref: refs/heads/main\n");
        tree_a.write(".blaze/cache.bin", b"cached");

        tree_b.write("include/api.h", b"int helper(void);\n");
        tree_b.write("Cargo.toml", b"[package]\nname = \"demo\"\nversion = \"0.1.0\"\n");
        tree_b.write("src/helpers.c", b"int helper(void) { return 42; }\n");
        // Worktree-style `.git` regular file and different `target/` content must also be skipped.
        tree_b.write(".git", b"gitdir: /some/other/host/path\n");
        tree_b.write("target/release/other.a", b"different noise");

        let scratch = Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).expect("scratch frame");
        let alloc = blaze_own::testing::heap().alloc();

        let walk_a = walk_checkout_pkg(tree_a.path_str(), CheckoutLimits::DEFAULT, alloc, &scratch).expect("walk tree_a");
        let walk_b = walk_checkout_pkg(tree_b.path_str(), CheckoutLimits::DEFAULT, alloc, &scratch).expect("walk tree_b");

        assert_eq!(walk_a.digest, walk_b.digest, "equal trees must hash to the same digest regardless of target/.git noise");
        let paths_a: std::vec::Vec<&str> = walk_a.files.iter().map(|s| s.as_str()).collect();
        assert_eq!(paths_a, ["Cargo.toml", "include/api.h", "src/helpers.c"]);

        // Editing one file must move the digest.
        tree_b.write("src/helpers.c", b"int helper(void) { return 43; }\n");
        let walk_edited = walk_checkout_pkg(tree_b.path_str(), CheckoutLimits::DEFAULT, alloc, &scratch).expect("walk edited tree_b");
        assert_ne!(walk_edited.digest, walk_a.digest, "editing a source file must change the digest");

        // Adding a new file must move the digest.
        tree_a.write("include/extra.h", b"#define EXTRA 1\n");
        let walk_added = walk_checkout_pkg(tree_a.path_str(), CheckoutLimits::DEFAULT, alloc, &scratch).expect("walk tree_a with extra file");
        assert_ne!(walk_added.digest, walk_a.digest, "adding a file must change the digest");
    }

    #[test]
    fn checkout_snapshot_lays_atomically_and_reuses_idempotently() {
        let src = TempDir::new("snap-src");
        let work = TempDir::new("snap-work");
        src.write("Cargo.toml", b"[package]\nname = \"snap\"\n");
        src.write("src/vm/helpers.c", b"int vm(void) { return 7; }\n");
        src.write("target/ignored.o", b"must not be copied");
        src.write(".git/config", b"[core]\n");

        let scratch = Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).expect("scratch frame");
        let alloc = blaze_own::testing::heap().alloc();

        let walk = walk_checkout_pkg(src.path_str(), CheckoutLimits::DEFAULT, alloc, &scratch).expect("walk src");
        let hex_bytes = crate::fetch::key_hex(&walk.digest);
        let hex = core::str::from_utf8(&hex_bytes).expect("hex utf8");

        let laid_first = lay_checkout_snapshot(work.path_str(), src.path_str(), &walk, alloc, &scratch).expect("lay snapshot first time");
        assert!(laid_first.ends_with(&std::format!("/pkg/{hex}")), "snapshot path should end with /pkg/<hex>, got {laid_first}");
        assert_eq!(std::fs::read(Path::new(laid_first).join("src/vm/helpers.c")).expect("read laid c file"), b"int vm(void) { return 7; }\n");
        assert!(!Path::new(laid_first).join("target").exists(), "target/ must not be copied into snapshot");
        assert!(!Path::new(laid_first).join(".git").exists(), ".git/ must not be copied into snapshot");

        // No staging `.part-*` directory may remain under `<work>/pkg`.
        let pkg_parent = work.path().join("pkg");
        for entry in std::fs::read_dir(&pkg_parent).expect("read pkg dir") {
            let name = entry.expect("dir entry").file_name();
            let name = name.to_str().expect("utf8 name");
            assert!(!name.contains(".part-"), "stale staging folder remained: {name}");
        }

        // Mutate the source tree after laying; re-laying the same `walk` into the same `work` must
        // idempotently reuse the existing immutable snapshot without re-reading the mutated source.
        src.write("src/vm/helpers.c", b"int vm(void) { return 999; }\n");
        let laid_again = lay_checkout_snapshot(work.path_str(), src.path_str(), &walk, alloc, &scratch).expect("reuse existing snapshot");
        assert_eq!(laid_again, laid_first, "idempotent reuse must return the same canonical snapshot folder");
        assert_eq!(
            std::fs::read(Path::new(laid_again).join("src/vm/helpers.c")).expect("read reused snapshot"),
            b"int vm(void) { return 7; }\n",
            "existing snapshot must remain immutable"
        );

        // Laying the stale `walk` against the mutated source into a FRESH work dir must detect the
        // digest mismatch (TOCTOU protection) and leave no partial directory behind.
        let fresh_work = TempDir::new("snap-toctou");
        let err = lay_checkout_snapshot(fresh_work.path_str(), src.path_str(), &walk, alloc, &scratch)
            .expect_err("mutated source must be refused when laying fresh snapshot");
        let words = std::format!("{err}");
        assert!(words.contains("changed while laying its snapshot"), "unexpected error: {words}");
        assert!(!fresh_work.path().join("pkg").join(hex).exists(), "failed snapshot must not be renamed into place");
        if fresh_work.path().join("pkg").is_dir() {
            for entry in std::fs::read_dir(fresh_work.path().join("pkg")).expect("read fresh pkg dir") {
                let name = entry.expect("dir entry").file_name();
                let name = name.to_str().expect("utf8 name");
                assert!(!name.contains(".part-"), "failed snapshot must clean up staging folder: {name}");
            }
        }
    }

    #[test]
    fn checkout_refuses_path_traversal_invalid_paths_and_escaping_symlinks() {
        // Valid tree-0 `shown_from` spellings.
        assert_eq!(checkout_rel_dir("/0"), Ok(""));
        assert_eq!(checkout_rel_dir("/0/"), Ok(""));
        assert_eq!(checkout_rel_dir("/0/vendor/wasmtime/"), Ok("vendor/wasmtime"));
        assert_eq!(checkout_rel_dir("/0/crates/foo"), Ok("crates/foo"));

        // Invalid `shown_from` spellings: non-tree-0, traversal, backslashes, NUL, empty parts.
        for bad in ["/1/foo/", "/01/foo/", "/0/../escape/", "/0/a/../b/", "/0/./foo/", "/0/a//b/", "/0/a\\b/", "/0/a\0b/", "0/foo/"] {
            assert!(checkout_rel_dir(bad).is_err(), "expected checkout_rel_dir({bad:?}) to be refused");
        }

        // Direct `validate_rel_path` checks.
        for bad in ["", "/abs", "trail/", ".", "..", "a/./b", "a/../b", "a//b", "a\\b", "a\0b"] {
            assert!(validate_rel_path(bad).is_err(), "expected validate_rel_path({bad:?}) to be refused");
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let scratch = Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).expect("scratch frame");
            let alloc = blaze_own::testing::heap().alloc();

            // 1. Symlink escaping the package root must be refused.
            let outside = TempDir::new("symlink-outside");
            outside.write("secret.txt", b"outside secret");
            let pkg_escape = TempDir::new("symlink-escape");
            pkg_escape.write("Cargo.toml", b"[package]\n");
            symlink(outside.path().join("secret.txt"), pkg_escape.path().join("escape.h")).expect("create escaping symlink");
            let err = walk_checkout_pkg(pkg_escape.path_str(), CheckoutLimits::DEFAULT, alloc, &scratch)
                .err()
                .expect("escaping symlink must be refused");
            assert!(std::format!("{err}").contains("escapes package root"), "unexpected error: {err}");

            // 2. Broken symlink must be refused.
            let pkg_broken = TempDir::new("symlink-broken");
            pkg_broken.write("Cargo.toml", b"[package]\n");
            symlink(pkg_broken.path().join("does-not-exist.h"), pkg_broken.path().join("broken.h")).expect("create broken symlink");
            assert!(walk_checkout_pkg(pkg_broken.path_str(), CheckoutLimits::DEFAULT, alloc, &scratch).is_err(), "broken symlink must be refused");

            // 3. Symlink directory cycle must be refused.
            let pkg_cycle = TempDir::new("symlink-cycle");
            pkg_cycle.write("sub/file.c", b"int x;\n");
            symlink(pkg_cycle.path(), pkg_cycle.path().join("sub/loop")).expect("create directory cycle symlink");
            let err = walk_checkout_pkg(pkg_cycle.path_str(), CheckoutLimits::DEFAULT, alloc, &scratch)
                .err()
                .expect("symlink directory cycle must be refused");
            assert!(std::format!("{err}").contains("cycle"), "unexpected error: {err}");

            // 4. Internal non-escaping symlink is accepted and materialized as a regular file in the snapshot.
            let pkg_ok = TempDir::new("symlink-internal");
            pkg_ok.write("include/real.h", b"#define OK 1\n");
            std::fs::create_dir_all(pkg_ok.path().join("src")).expect("create src dir");
            symlink("../include/real.h", pkg_ok.path().join("src/alias.h")).expect("create internal symlink");
            let walk = walk_checkout_pkg(pkg_ok.path_str(), CheckoutLimits::DEFAULT, alloc, &scratch).expect("internal symlink accepted");
            let work = TempDir::new("symlink-internal-work");
            let laid = lay_checkout_snapshot(work.path_str(), pkg_ok.path_str(), &walk, alloc, &scratch).expect("lay internal symlink snapshot");
            assert_eq!(std::fs::read(Path::new(laid).join("src/alias.h")).expect("read materialized symlink"), b"#define OK 1\n");
        }
    }

    #[test]
    fn checkout_walk_enforces_file_depth_and_byte_limits() {
        let tree = TempDir::new("limits");
        tree.write("a.c", b"0123456789");
        tree.write("b.c", b"0123456789");
        tree.write("d1/d2/c.c", b"0123456789");

        let scratch = Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).expect("scratch frame");
        let alloc = blaze_own::testing::heap().alloc();

        // Exceeds max_files.
        let tight_files = CheckoutLimits { max_files: 2, ..CheckoutLimits::DEFAULT };
        assert!(walk_checkout_pkg(tree.path_str(), tight_files, alloc, &scratch).is_err(), "max_files limit must refuse");

        // Exceeds max_depth.
        let tight_depth = CheckoutLimits { max_depth: 1, ..CheckoutLimits::DEFAULT };
        assert!(walk_checkout_pkg(tree.path_str(), tight_depth, alloc, &scratch).is_err(), "max_depth limit must refuse");

        // Exceeds max_dirs.
        let tight_dirs = CheckoutLimits { max_dirs: 2, ..CheckoutLimits::DEFAULT };
        assert!(walk_checkout_pkg(tree.path_str(), tight_dirs, alloc, &scratch).is_err(), "max_dirs limit must refuse");

        // Exceeds max_bytes.
        let tight_bytes = CheckoutLimits { max_bytes: 25, ..CheckoutLimits::DEFAULT };
        assert!(walk_checkout_pkg(tree.path_str(), tight_bytes, alloc, &scratch).is_err(), "max_bytes limit must refuse");

        // Succeeds when just within limits (3 dirs: root, d1, d1/d2; depth 2; 3 files; 30 bytes).
        let exact = CheckoutLimits { max_depth: 2, max_dirs: 3, max_files: 3, max_bytes: 30 };
        assert!(walk_checkout_pkg(tree.path_str(), exact, alloc, &scratch).is_ok(), "exact limits must succeed");
    }
}
