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
//! Not yet, each refused by name ([`CTaskWhy`]):
//! - a crate whose files can change (a workspace member, a path dependency): it has no pinned
//!   key (c_tasks_built.rs), and its package would be the checkout's folder, not a pack;
//! - a request that names a `links` dependency that is a workspace member (its `/deps/<links>`
//!   tree is the checkout's folder, not a pack).
//!
//! A `links` dependency's trees a request names (c_tasks.rs `dep_roots`: librocksdb-sys reads
//! lz4-sys's and zstd-sys's) are laid read-only beside the package: a producer's `OUT_DIR` from
//! its script record as the reader's script saw it (`<work>/dep/<key>`), its served tree
//! (`/deps/<links>`) from its pack; each joins the built key by its digest ([`Producers`]).
//!
//! Process-tree requests (tikv-jemalloc-sys's `sh configure`, `make`) run as one task before
//! the compiles (native_c_tasks.rs `ShelfTree`, blaze-build-script c_tasks_tree.rs): the host
//! tools the table grants the crate and the OS build join the built key
//! (`tree_tools_digest`).
//!
//! FLOOR (rule 67): a hit costs one hash of the key's inputs and one get; the floor of a miss
//! is clang's own time on the misses spread over the workers, plus laying the package once.

use core::fmt;

use blaze_build_script::c_tasks::{self, CStore, CompilerKind, GuestRoots, StoreFailed, Toolchain};
use blaze_build_script::c_tasks_built::{self, Pinned, Report};
use blaze_build_script::dep_paths::DEPS_ROOT;
use blaze_build_script::outputs::OutputsRef;
use blaze_drive::Key;
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
    sdk: blaze_apple_sdk::Sdk<'e>,
    fs: &'e bstd::fs::Fs,
    workers: u32,
    heap: &'static blaze_own::Heap,
    setup: Option<Setup>,
    dirs: PackDirs,
}

impl<'e> CTasks<'e> {
    pub(crate) fn new(home: Option<&'e str>, sdk: blaze_apple_sdk::Sdk<'e>, fs: &'e bstd::fs::Fs, workers: u32, heap: &'static blaze_own::Heap) -> CTasks<'e> {
        CTasks { home, sdk, fs, workers, heap, setup: None, dirs: PackDirs::default() }
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
    producers: &Producers<'_, 'm>,
    scratch: &'s Frame,
) -> Result<(ScriptIn<'s>, Option<Report>), CTaskWhy<'s>> {
    let cx = cx.span("native: a crate's deferred C requests");
    let Some((ran, record)) = crate::scripts::payload_parts(script.payload) else { return Err(CTaskWhy::Outputs) };
    let outputs = OutputsRef::open(record).map_err(|_| CTaskWhy::Outputs)?;
    let requests = outputs.c_requests().count() as u32;
    if requests == 0 {
        return Ok((script, None));
    }
    let (source, files) = match <CrateList<'_> as CrateTable<'_>>::place(list, r) {
        Place::Fetched { source, files } => (source, files),
        Place::Checkout => return Err(CTaskWhy::NotPinned { why: "a workspace member's files can change; its objects are not found by a pinned key yet" }),
        Place::Toolchain => return Err(CTaskWhy::NotPinned { why: "a toolchain crate with C requests" }),
        Place::Unplaced { why } => return Err(CTaskWhy::NotPinned { why }),
    };
    let alloc = c.heap.alloc();
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
    let trees = match has_tree {
        false => None,
        true => {
            let shelf = c.setup(scratch)?.setting.shelf.as_str();
            let tools = native_c_tasks::tree_tools(package, crate::graph::TRIPLE, shelf, alloc)
                .map_err(|words| CTaskWhy::Setup { words: kept(scratch, format_args!("the C of {package}: its process tree's host tools: {}", words.as_str())) })?;
            Some(tools)
        }
    };
    let tree_digest = trees.as_ref().map(native_c_tasks::tree_tools_digest);
    let roots = GuestRoots { package: blaze_expand::seen::PACKAGE_ROOT, out: outputs.out_dir(), builds: GUEST_BUILDS, deps: DEPS_ROOT, sysroot: GUEST_SDK };
    // Each `links` dependency's tree the requests name (librocksdb-sys: zstd-sys's `OUT_DIR`,
    // `/deps/zstd`), found and keyed before anything is laid: a hit lays nothing.
    let named = c_tasks::dep_roots(&outputs, &roots, alloc);
    let mut graph_links: Option<AVec<'_, (AString<'_>, AString<'_>, AString<'_>)>> = None;
    let mut deps: AVec<'_, DepTree<'m>> = Vec::with_capacity_in(named.len(), alloc);
    for guest in named.iter().copied() {
        deps.push(dep_tree(list, producers, guest, &mut graph_links, alloc, scratch)?);
    }
    let mut dep_keys: AVec<'_, (&str, [u8; 32])> = Vec::with_capacity_in(deps.len(), alloc);
    dep_keys.extend(deps.iter().map(|dep| (dep.guest, dep.digest)));
    let key = c_tasks_built::built_key(script.out, &toolchain, Pinned { source: source.0, files }, &demand_text, &dep_keys, tree_digest);

    // One get: the built record, read in place.
    let held = store.get(&cx, Key(key)).map_err(|e| CTaskWhy::Built { words: kept(scratch, format_args!("the store: {e}")) })?;
    if let Some(held) = held {
        let Some(payload) = c_tasks_built::open_built(held.bytes) else {
            return Err(CTaskWhy::Built { words: kept(scratch, format_args!("the record at {key:?} is no built record (c_tasks_built.rs `BUILT_MAGIC`)")) });
        };
        let report = Report { requests, built_hit: true, ..Report::default() };
        return Ok((ScriptIn { name: script.name, out: key, payload }, Some(report)));
    }

    // A miss: the package laid, the tasks run, the built record put.
    let (package_dir, package_digest) = lay_package(&cx, c, list, r, files, scratch)?;
    let mut sources: AVec<'_, Laid<'_>> = Vec::with_capacity_in(1 + deps.len(), alloc);
    sources.push(Laid { guest: blaze_expand::seen::PACKAGE_ROOT, host: package_dir, digest: package_digest, name: "the package" });
    // Each read-only: granted read, never written (native_c.rs `Laid`).
    for dep in deps.iter() {
        let (host, name) = match dep.source {
            DepSource::Out { script } => (lay_dep_out(c, script, dep.digest, scratch)?, "a `links` dependency's OUT_DIR"),
            DepSource::Tree { r: producer, files } => {
                let (host, digest) = lay_package(&cx, c, list, producer, files, scratch)?;
                if digest != dep.digest {
                    return Err(CTaskWhy::Lay { words: kept(scratch, format_args!("{}: its package laid under another digest than it was keyed by", dep.guest)) });
                }
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
    let payload: &'s [u8] = blaze_own::Vec::from_slice_in(&body, scratch).leak();
    Ok((ScriptIn { name: script.name, out: key, payload }, Some(run.report)))
}

/// What the image holds for its readers' `links` trees (engine_native.rs `image_for`): every
/// crate's script record as it ran (the deferred one when its C was deferred: what a reader's
/// script saw), and the unit graph record (graph_payload.rs `Sections::graph`), whose units name
/// each package's `links`; it is decoded only when a reader names `/deps/<links>`.
pub(crate) struct Producers<'p, 'm> {
    pub(crate) scripts: &'p [(usize, ScriptIn<'m>)],
    pub(crate) graph: &'p [u8],
}

/// One `links` dependency's tree a reader's requests name: its guest spelling, what names it
/// in the built key, and where its files come from.
struct DepTree<'m> {
    guest: &'m str,
    digest: [u8; 32],
    source: DepSource<'m>,
}

#[derive(Clone, Copy)]
enum DepSource<'m> {
    /// A producer's `OUT_DIR` (`<builds>/<name>-<config>/out`): its script record's files, the
    /// record by its key.
    Out { script: ScriptIn<'m> },
    /// The tree a producer was served (`/deps/<links>`): crate `r`'s package, by its files.
    Tree { r: usize, files: [u8; 32] },
}

/// The producer of `guest` (c_tasks.rs `dep_roots`), keyed: an `OUT_DIR` by the script record
/// whose `OUT_DIR` it is (one per package and config), a `/deps/<links>` tree by the package
/// whose `links` it is (one per graph, the cargo book's "links"). None, or a producer whose
/// files can change, is refused naming the spelling (rule 3); nothing is guessed.
fn dep_tree<'m, 's, 'a>(
    list: &CrateList<'_>,
    producers: &Producers<'_, 'm>,
    guest: &'m str,
    graph_links: &mut Option<AVec<'a, (AString<'a>, AString<'a>, AString<'a>)>>,
    alloc: Alloc<'a>,
    scratch: &'s Frame,
) -> Result<DepTree<'m>, CTaskWhy<'s>> {
    let lay = |what: fmt::Arguments<'_>| CTaskWhy::Lay { words: kept(scratch, what) };
    let Some(links) = guest.strip_prefix(DEPS_ROOT).and_then(|rest| rest.strip_prefix('/')) else {
        let found = producers.scripts.iter().find(|(_, script)| {
            crate::scripts::payload_parts(script.payload).and_then(|(_, record)| OutputsRef::open(record).ok()).is_some_and(|opened| opened.out_dir() == guest)
        });
        let Some((_, script)) = found else {
            return Err(lay(format_args!("a request names `{guest}`, the OUT_DIR of no build script this program links")));
        };
        return Ok(DepTree { guest, digest: script.out.0, source: DepSource::Out { script: *script } });
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
    let fetched = (0..list.len()).find_map(|at| {
        if list.package(at) != name.as_str() || list.version(at) != version.as_str() {
            return None;
        }
        match <CrateList<'_> as CrateTable<'_>>::place(list, at) {
            Place::Fetched { files, .. } => Some((at, files)),
            _ => None,
        }
    });
    let Some((at, files)) = fetched else {
        return Err(lay(format_args!(
            "a request names `{guest}`, and `{} {}` (`links = \"{links}\"`) is no fetched package of this program: a workspace member's tree is not laid for a C task yet",
            name.as_str(),
            version.as_str()
        )));
    };
    Ok(DepTree { guest, digest: package_digest(list, at, files).0, source: DepSource::Tree { r: at, files } })
}

/// A producer's `OUT_DIR` laid read-only from its script record (`<work>/dep/<key>`, made once:
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
    for file in view.files() {
        let rest = match prefix {
            None => file.path,
            Some(dir) => match file.path.strip_prefix(dir).and_then(|rest| rest.strip_prefix('/')) {
                Some(rest) => rest,
                None => continue,
            },
        };
        if rest.split('/').any(|part| part.is_empty() || part == "." || part == "..") {
            return Err(lay(format_args!("its pack names `{}`, which is not a path in a folder", file.path)));
        }
        let path = bstd::mem::format_in!(scratch, "{}/{rest}", part.as_str());
        if let Some((parent, _)) = path.as_str().rsplit_once('/') {
            std::fs::create_dir_all(parent).map_err(|e| lay(format_args!("{parent}: {e}")))?;
        }
        std::fs::write(path.as_str(), file.bytes).map_err(|e| lay(format_args!("{}: {e}", path.as_str())))?;
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
