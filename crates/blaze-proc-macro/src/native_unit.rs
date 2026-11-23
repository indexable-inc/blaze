//! D65, D69 and D71 on the real path: a build-script unit placed by [`placed`] (its dependency
//! closure and the command's mode, before anything runs) and, when native, run by
//! [`native_outputs`]: its host binary from the module records (`script.bin`, built by the
//! interim builder's `native` mode), the same environment the wasm path gives (the unit's own
//! features, the served unit's cfg), its run key with the pinned compiler, the SDK's version
//! and any granted host tool's digest, run confined ([`crate::native_script::run_script`];
//! nothing kept here: the `Script` task's record is the cache, D115). A native failure is a
//! typed error: it is never tried again as wasm (rule 3), and a wasm failure never native.

use std::{path::Path, time::Duration};

use blaze_build_script::{
    env::{DepMetadata, Given, PackageFacts},
    host_tools::{self, HostTool},
    key::{self, CMode, HostToolKey, KeyInputs, RunLimits, RunnerKey},
    native::{self, Runner},
    probe::Table,
    toolchain::{self, CInputs, Mode, Shelf, Sysroot},
};
use blaze_pins::{CLANG_JSON, CLANG_PATH};
use blaze_ids::ContentId;
use blaze_own::{AVec, Alloc, Vec};
use blaze_unit_graph::{UnitGraph, UnitId};
use bstd::{
    fs::Durable,
    mem::{Bytes, TASK_FRAME},
};

use crate::{
    native_facts,
    native_script::{NativeScript, NativeScriptError, listed_under, remove, run_script},
    record::native_world,
    script::ScriptRun,
    script_sandbox::{
        Read, Tree, answer_now,
        dep_mount::{self, DepProducer, native::LayError},
    },
    script_unit::module_of_native,
};

/// The deployment target every native script compiles for: its one home is blaze-build-script
/// toolchain.rs, which the keyed environment gives every script (the interim builder links the
/// scripts for the same).
pub use blaze_build_script::toolchain::MACOS_MIN;

/// Where `script` (a build-script unit) runs for `mode`: the closure's verdict
/// ([`native::closure`]: a crate of the toolchain class, and one that does not compile for
/// wasm) placed by the mode ([`toolchain::placement`], D69 (3)).
///
/// This is THE one place a script is given the native runner; nothing else chooses it.
/// D112: the scripts whose closure cannot be wasm (`native::NO_WASM_CLASS`: libclang's chain),
/// those named in `toolchain::NATIVE_EVERY_MODE`, and tikv-jemalloc-sys for build/run run in
/// the Seatbelt native runner until the deterministic
/// sandbox runs; records local. A script placed native runs with the command's
/// [`NativeSetting`] (made once by [`crate::native_setting::command_native_setting`]) or is
/// refused by name ([`NativeUnitError::NoNativeSetting`]); it is never tried as wasm (rule 3).
pub fn placed(graph: &UnitGraph<'_>, script: UnitId, mode: Mode, alloc: Alloc<'_>) -> Runner {
    toolchain::placement(native::closure(graph, script, alloc), mode, graph.units[script.index()].name.as_str())
}

/// Like [`placed`], but honors the D175 zero-Wasm fast path: whenever `store` holds a native
/// host binary record (`script.bin`, `target <host>`) for `script` and does not hold a legacy
/// `module.wasm` record, places `script` on [`Runner::Native`] so it runs confined under
/// `bstd::procs::hermetic` (macOS Seatbelt / Linux Landlock + seccomp) with zero Wasm.
pub fn placed_in_store(
    store: &Path,
    graph: &UnitGraph<'_>,
    script: UnitId,
    mode: Mode,
    host: &str,
    alloc: Alloc<'_>,
) -> Runner {
    let base = placed(graph, script, mode, alloc);
    if matches!(base, Runner::Native { .. }) {
        return base;
    }
    let Some(unit) = graph.units.get(script.index()) else { return base };
    let features: std::vec::Vec<&str> = unit.features.iter().map(|f| f.as_str()).collect();
    let has_native = crate::script_unit::module_of_native(
        store,
        unit.name.as_str(),
        unit.version.as_str(),
        &features,
        host,
    )
    .ok()
    .flatten()
    .is_some();
    if has_native {
        let has_wasm = crate::script_unit::module_of(
            store,
            unit.name.as_str(),
            unit.version.as_str(),
            &features,
        )
        .ok()
        .flatten()
        .is_some();
        if !has_wasm {
            return Runner::Native { because: script };
        }
    }
    base
}

/// What a command gives its native scripts. Every path absolute and canonical (Seatbelt matches
/// resolved paths).
#[derive(Clone, Copy, Debug)]
pub struct NativeSetting<'s> {
    /// The unpacked pin, `<home>/.blaze/v2/toolchain/clang/<sha256 hex>`: its name is the
    /// compiler's digest in the key.
    pub shelf: &'s str,
    /// The macOS SDK folder, resolved once by the command.
    pub sdk: &'s str,
    /// A folder of the command's own where packages held in records are written out and each
    /// run's `OUT_DIR` is made (`pkg/<key>`, `out/<key>`).
    pub work: &'s str,
    /// The deadline of one run (rule 18).
    pub deadline: Duration,
    /// The command's jobserver ([`crate::jobserver`]): each run's children share the command's
    /// workers through it. Never in a key (`blaze_build_script::env::Jobs`).
    pub jobs: &'s crate::jobserver::Jobserver,
}

/// The native part of a run key, known before the run.
#[derive(Clone, Copy, Debug)]
pub struct NativeKeyParts<'k> {
    pub compiler: [u8; 32],
    pub target: &'k str,
    pub sysroot: &'k str,
    pub host_tools: &'k [HostToolKey<'k>],
}

/// The run key's runner for a placed script: a wasm script's adds nothing, a native one's the
/// compiler, target, headers and host tools.
pub fn runner_key<'k>(placed: Runner, parts: &NativeKeyParts<'k>) -> RunnerKey<'k> {
    match placed {
        Runner::Wasm => RunnerKey::Wasm,
        Runner::Native { .. } => RunnerKey::Native { compiler: parts.compiler, target: parts.target, sysroot: parts.sysroot, host_tools: parts.host_tools },
    }
}

/// Why a native script gave no outputs; each names the package.
#[derive(Debug)]
pub enum NativeUnitError {
    /// The command gave no native setting (no shelf, no SDK), so a native script cannot run.
    NoNativeSetting { package: String, because: String },
    /// No native module record (`script.bin`) of the package with exactly these features.
    NoModule { package: String, features: std::vec::Vec<String>, store: String },
    /// The module store could not answer the lookup.
    Lookup(crate::script_unit::ModuleLookupError),
    /// A file blaze reads to make the run (the module, the SDK's settings, a host tool).
    Read { path: String, why: String },
    /// The pin table does not say what the run needs.
    PinTable { package: String, why: String },
    /// A host tool's path in the pin table is not its resolved real path (D71).
    ToolNotReal { package: String, tool: String, path: String, real: String },
    /// The shelf's folder is not named by a SHA-256.
    Shelf { shelf: String },
    /// The package's or the run's folder could not be made or read.
    Files { package: String, folder: String, why: String },
    /// The run itself: refused, failed, or its outputs.
    Run(NativeScriptError),
    /// The fact shim could not be placed, prepared or read (D112, N7).
    Facts { package: String, why: String },
    /// The run failed after the fact shim refused questions, each named (D112, N7).
    FactRefused { package: String, refused: std::vec::Vec<String>, run: NativeScriptError },
    /// A `links` dependency's metadata could not be resolved or laid for the run (brief N8).
    DepLinks { package: String, why: LayError },
}

impl std::fmt::Display for NativeUnitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NativeUnitError::NoNativeSetting { package, because } => write!(
                f,
                "the build script of {package} runs native (its closure holds {because}, D65) and this command gave no pinned C toolchain or SDK; it is not tried as wasm (rule 3)"
            ),
            NativeUnitError::NoModule { package, features, store } => write!(
                f,
                "no native build-script binary of {package} with features [{}] in {store}/build-script/modules: build it with proc_macro_modules' build-script mode and `native` (D69)",
                features.join(",")
            ),
            NativeUnitError::Lookup(error) => write!(f, "{error}"),
            NativeUnitError::Read { path, why } => write!(f, "{path}: {why}"),
            NativeUnitError::PinTable { package, why } => write!(f, "the native build script of {package}: {why}"),
            NativeUnitError::ToolNotReal { package, tool, path, real } => write!(
                f,
                "{}: host tool `{tool}` of {package}'s build script is at {path}, whose real path is {real}: the grant takes only a resolved path (D71)",
                CLANG_PATH
            ),
            NativeUnitError::Shelf { shelf } => write!(f, "{shelf}: the C toolchain's shelf folder is not named by its SHA-256 (D69)"),
            NativeUnitError::Files { package, folder, why } => write!(f, "{folder} (the native build script of {package}): {why}"),
            NativeUnitError::Run(error) => write!(f, "{error}"),
            NativeUnitError::DepLinks { package, why } => write!(f, "the native build script of {package}: {why}"),
            NativeUnitError::Facts { package, why } => write!(f, "the native build script of {package}: {why}"),
            NativeUnitError::FactRefused { package, refused, run } => write!(
                f,
                "the native build script of {package} asked what no pinned fact answers (the fact shim refused with exit 127, D112): {}; then: {run}",
                refused.join("; ")
            ),
        }
    }
}

impl std::error::Error for NativeUnitError {}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 15)]));
    }
    out
}

fn hex32(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

/// The `Version` of an `SDKSettings.json`'s text as written: its top-level key's string, read
/// by the SDK's one home (blaze-apple-sdk), so the run key and the linker hold one identity.
pub fn sdk_version(settings: &str) -> Option<&str> {
    blaze_apple_sdk::settings_version(settings).map(|(written, _)| written)
}

/// A file's bytes through the effect layer.
fn read_file(folder: &str, name: &str) -> Result<std::vec::Vec<u8>, NativeUnitError> {
    let failed = |why: String| NativeUnitError::Read { path: format!("{folder}/{name}"), why };
    let (io, root) = native_world(Path::new(folder)).map_err(|e| failed(e.to_string()))?;
    let cx = io.cx(Duration::from_secs(60), TASK_FRAME).map_err(|_| failed("its world stopped".to_owned()))?;
    let mut bytes = Bytes::new_in(cx.frame(), "a native build script's input");
    let path = bstd::path::Path::new(name).map_err(|_| failed("not a relative path".to_owned()))?;
    root.read_all(&cx, &path, &mut bytes).map_err(|e| failed(e.to_string()))?;
    Ok(bytes.as_slice().to_vec())
}

/// `work/<rel>/` made to hold exactly `files` (none when `files` is empty: an empty folder):
/// what an earlier run left there is removed first.
fn fresh_folder(work: &str, rel: &str, files: &[(String, std::vec::Vec<u8>)], package: &str) -> Result<String, NativeUnitError> {
    let folder = format!("{work}/{rel}");
    let failed = |why: String| NativeUnitError::Files { package: package.to_owned(), folder: folder.clone(), why };
    if Path::new(&folder).exists() {
        // Listed, never read: an earlier run's `OUT_DIR` may hold a C library's archive.
        for left in listed_under(&folder).map_err(|e| failed(e.to_string()))? {
            remove(&folder, &left.name).map_err(|e| failed(e.to_string()))?;
        }
    }
    let (io, root) = native_world(Path::new(work)).map_err(|e| failed(e.to_string()))?;
    let cx = io.cx(Duration::from_secs(60), TASK_FRAME).map_err(|_| failed("its world stopped".to_owned()))?;
    let marker = ".blaze-made";
    let made: [(String, std::vec::Vec<u8>); 1] = [(marker.to_owned(), std::vec::Vec::new())];
    let all = if files.is_empty() { &made[..] } else { files };
    for (name, bytes) in all {
        let under = format!("{rel}/{name}");
        let path = bstd::path::Path::new(&under).map_err(|_| failed(format!("{name}: not a relative path")))?;
        let file = root.create(&cx, &path).map_err(|e| failed(format!("{name}: {e}")))?;
        file.write_at(&cx, 0, bytes, Durable::No).map_err(|e| failed(format!("{name}: {e}")))?;
        file.close(&cx, Durable::No).map_err(|e| failed(format!("{name}: {e}")))?;
        #[cfg(unix)]
        if bytes.starts_with(b"#!") {
            use std::os::unix::fs::PermissionsExt as _;
            let full = format!("{work}/{under}");
            let _ = std::fs::set_permissions(&full, std::fs::Permissions::from_mode(0o755));
        }
    }
    if files.is_empty() {
        remove(&folder, marker).map_err(|e| failed(e.to_string()))?;
    }
    Ok(folder)
}

/// `script`'s outputs, run native and confined. `serves` is the unit it serves (its cfg is the
/// script's `CARGO_CFG_*`), `because` the toolchain-class unit that made it native (for the
/// error when no setting is given).
/// `producers` are the runs of its `links` dependencies, each with its `links` (brief N8):
/// `deps`' paths are resolved against them, laid read-only for the run.
#[allow(clippy::too_many_arguments)]
pub fn native_outputs<'a>(
    setting: &crate::script_unit::Setting<'_>,
    graph: &UnitGraph<'_>,
    script: UnitId,
    serves: UnitId,
    because: UnitId,
    tree: Tree,
    root: crate::script_sandbox::ScriptRoot,
    deps: &[DepMetadata<'_>],
    producers: &[(&str, &DepProducer)],
    alloc: Alloc<'a>,
) -> Result<ScriptRun<'a>, NativeUnitError> {
    let unit = &graph.units[script.index()];
    let (name, version) = (unit.name.as_str(), unit.version.as_str());
    let pkg = || format!("{name} {version}");
    let Some(native) = setting.native else {
        let because = &graph.units[because.index()];
        return Err(NativeUnitError::NoNativeSetting { package: pkg(), because: format!("{} {}", because.name.as_str(), because.version.as_str()) });
    };

    // The host binary the interim builder made with exactly the unit's features.
    let features: std::vec::Vec<&str> = unit.features.iter().map(|f| f.as_str()).collect();
    let module = module_of_native(setting.store, name, version, &features, setting.host).map_err(NativeUnitError::Lookup)?.ok_or_else(|| NativeUnitError::NoModule {
        package: pkg(),
        features: features.iter().map(|f| (*f).to_owned()).collect(),
        store: setting.store.display().to_string(),
    })?;
    // Seatbelt matches resolved paths: the store may be named through a link.
    let module = std::fs::canonicalize(&module).map_err(|e| NativeUnitError::Read { path: module.display().to_string(), why: e.to_string() })?;
    let module = module.to_str().ok_or_else(|| NativeUnitError::Read { path: module.display().to_string(), why: "not UTF-8".to_owned() })?.to_owned();
    let program = format!("{module}/script.bin");
    let module_digest = blaze_sha256::digest(&read_file(&module, "script.bin")?);

    // The same environment as the wasm path: the unit's features, the served unit's cfg.
    let serves_unit = &graph.units[serves.index()];
    let facts = PackageFacts { name, version, meta: graph.meta_of(unit.package).map(|meta| meta.vars()), links: unit.links.as_ref().map(|l| l.as_str()) };
    // Brief N8: the key holds each `links` value in the record's form (the wasm sight: a
    // function of the producers' records alone) and what the producers serve (`views.key`
    // below), as a wasm reader's key does; the run is given the values as IT sees them (laid
    // below). A value under neither of a producer's trees is refused here, naming it.
    let dep_error = |why: LayError| NativeUnitError::DepLinks { package: pkg(), why };
    let viewed = dep_mount::view_with_stored(&tree, producers, deps, alloc).map_err(|e| dep_error(LayError::View(e)))?;
    let mut keyed: AVec<'a, DepMetadata<'_>> = Vec::new_in(alloc);
    for (dep, value) in deps.iter().zip(viewed.values.iter()) {
        keyed.push(DepMetadata { links: dep.links, key: dep.key, value: value.as_str() });
    }
    let given = Given { package: facts, seen: root.seen(), features: &unit.features, target_cfg: &serves_unit.cfg, target: setting.target, host: setting.host, deps: &keyed };
    let env = blaze_build_script::env(&given, alloc);

    // D71: the host tools the table grants this package, each at its real path, by digest.
    let pin = |why: String| NativeUnitError::PinTable { package: pkg(), why };
    let table = host_tools::host_tools(CLANG_JSON, setting.host, alloc).map_err(|e| pin(e.to_string()))?;
    let tools: AVec<'a, HostTool<'_>> = host_tools::granted(&table, name, alloc);
    // Every granted program is keyed at its real path: the tool under its name, then each
    // program it starts under `<tool>+<path>` (sorted after the tool's own name, in the table's
    // sorted order); a shelf row's program (nm-row: nm, the shelf's llvm-nm) under the shelf.
    let mut programs: AVec<'a, (blaze_own::AString<'a>, blaze_own::AString<'a>, HostTool<'_>)> = Vec::new_in(alloc);
    for tool in tools.iter() {
        for (at, program) in tool.programs_in(native.shelf, alloc).into_iter().enumerate() {
            let mut name: blaze_own::AString<'a> = blaze_own::String::from_str_in(tool.name, alloc);
            if at > 0 {
                name.push('+');
                name.push_str(program.as_str());
            }
            programs.push((name, program, *tool));
        }
    }
    let mut tool_keys: AVec<'a, HostToolKey<'_>> = Vec::new_in(alloc);
    for (name, program, tool) in programs.iter() {
        let program = program.as_str();
        let real = std::fs::canonicalize(program).map_err(|e| NativeUnitError::Read { path: program.to_owned(), why: e.to_string() })?;
        if real.as_os_str() != program {
            return Err(NativeUnitError::ToolNotReal { package: pkg(), tool: tool.name.to_owned(), path: program.to_owned(), real: real.display().to_string() });
        }
        let (dir, file) = program.rsplit_once('/').unwrap_or(("/", program));
        let digest = blaze_sha256::digest(&read_file(if dir.is_empty() { "/" } else { dir }, file)?);
        tool_keys.push(HostToolKey::of(tool, name.as_str(), digest));
    }

    // The key: the compiler by its shelf digest, the SDK by its version (D69), the tools.
    match toolchain::sysroot(CLANG_JSON, setting.host) {
        Ok(Sysroot::MacosSdk) => {}
        Ok(Sysroot::Pinned { .. }) => return Err(pin(format!("{}: {}'s pinned sysroot is not fetched yet (D69)", CLANG_PATH, setting.host))),
        Err(error) => return Err(pin(error.to_string())),
    }
    let text = |file: &str| -> Result<String, NativeUnitError> {
        String::from_utf8(read_file(native.sdk, file)?).map_err(|_| NativeUnitError::Read { path: format!("{}/{file}", native.sdk), why: "it is not UTF-8".to_owned() })
    };
    let (settings, built) = (text(blaze_apple_sdk::SETTINGS)?, text(blaze_apple_sdk::SYSTEM_VERSION)?);
    let (_, id) = blaze_apple_sdk::identity_of(&settings, &built).map_err(|why| NativeUnitError::Read { path: native.sdk.to_owned(), why: format!("{why}, which a native script's key holds (D69)") })?;
    let sysroot_key = format!("macos-sdk {id}");
    let compiler = hex32(native.shelf.rsplit('/').next().unwrap_or("")).ok_or_else(|| NativeUnitError::Shelf { shelf: native.shelf.to_owned() })?;
    let parts = NativeKeyParts { compiler, target: setting.host, sysroot: &sysroot_key, host_tools: &tool_keys };
    let inputs = KeyInputs {
        module: module_digest,
        env: &env,
        answers: Table::EMPTY.id(),
        c_mode: CMode::Built,
        limits: RunLimits::STANDARD,
        runner: runner_key(Runner::Native { because }, &parts),
    };
    let mut scratch: AVec<'_, u8> = Vec::new_in(alloc);
    // What the producers serve is in the key, as for a wasm reader (rule 21).
    let base: ContentId = viewed.views.key(key::base(&inputs, &mut scratch));
    // The package on disk (a workspace member's folder as it is; a fetched package written out
    // under its key) and an empty `OUT_DIR`.
    let key_hex = hex(&base.0);
    let (package_dir, workspace_read, listed): (String, Option<String>, std::vec::Vec<String>) = match &tree {
        // Already the resolved root (`RealRoot`, the edge's one realpath, as Seatbelt matches
        // paths): not resolved again. For a workspace member (`root.seen()` = `/workspace/<rel>`),
        // `package_dir` (CARGO_MANIFEST_DIR + cwd) is `<workspace>/<rel>`, while the workspace
        // root itself remains granted read-only so workspace-relative reads still succeed.
        Tree::Dir(dir) => {
            let ws_real = dir.path().to_str().ok_or_else(|| NativeUnitError::Files { package: pkg(), folder: dir.path().display().to_string(), why: "not UTF-8".to_owned() })?.to_owned();
            let seen_str = root.seen().to_string();
            let rel = seen_str.strip_prefix("/workspace").unwrap_or("").trim_start_matches('/');
            let pkg_path = if rel.is_empty() { dir.path().to_path_buf() } else { std::fs::canonicalize(dir.path().join(rel)).unwrap_or_else(|_| dir.path().join(rel)) };
            let pkg_real = pkg_path.to_str().ok_or_else(|| NativeUnitError::Files { package: pkg(), folder: pkg_path.display().to_string(), why: "not UTF-8".to_owned() })?.to_owned();
            let names = match listed_under(&ws_real) {
                Ok(names) => names,
                Err(e) => return Err(NativeUnitError::Files { package: pkg(), folder: ws_real, why: e.to_string() }),
            };
            let names = names.into_iter().map(|listed| listed.name).collect();
            let extra_read = (pkg_real != ws_real).then_some(ws_real);
            (pkg_real, extra_read, names)
        }
        Tree::Mem { files, .. } => {
            let dir = fresh_folder(native.work, &format!("pkg/{key_hex}"), files, &pkg())?;
            (dir, None, files.iter().map(|(n, _)| n.clone()).collect())
        }
    };
    let out_dir = fresh_folder(native.work, &format!("out/{key_hex}"), &[], &pkg())?;
    let clang_major = toolchain::clang_major(CLANG_JSON).map_err(|e| pin(e.to_string()))?;
    let c = CInputs { shelf: Shelf { dir: native.shelf, clang_major }, sysroot: native.sdk, out_dir: &out_dir, package: &package_dir, seen: root.seen(), macos_min: MACOS_MIN };
    // N7: the fact shim the command placed in its work folder, and this run's facts folder.
    // A native run's probes are answered from the pins through the fact shim; the questions
    // asked become reads in the wasm path's fact form (native_facts::reads).
    let pins = setting.spawn_setting.pins.as_ref();
    let facts_error = |e: native_facts::NativeFactsError| NativeUnitError::Facts { package: pkg(), why: e.to_string() };
    let shim = native_facts::placed_shim(native.work).map_err(facts_error)?;
    let facts = native_facts::prepare(&shim, native.work, &key_hex, pins, alloc).map_err(facts_error)?;
    // The home's file area: each file the script leaves goes there by digest, renamed, never
    // read whole (librocksdb-sys's 810 MB archive); made once, canonical as Seatbelt's paths.
    let area = setting.store.join(blaze_build_script::outputs::FILE_AREA);
    let area_failed = |why: String| NativeUnitError::Files { package: pkg(), folder: area.display().to_string(), why };
    std::fs::create_dir_all(&area).map_err(|e| area_failed(e.to_string()))?;
    let area = std::fs::canonicalize(&area).map_err(|e| area_failed(e.to_string()))?;
    let area = area.to_str().ok_or_else(|| area_failed("not UTF-8".to_owned()))?.to_owned();
    // Brief N8: the producers' trees laid read-only under this run's own folder, each value as
    // this run sees it; what the script prints under them is kept in the record's form.
    let laid = dep_mount::native::lay_with_area(native.work, &format!("deps/{key_hex}"), Some(Path::new(&area)), producers, deps, alloc).map_err(dep_error)?;
    let mut run_deps: AVec<'a, DepMetadata<'_>> = Vec::new_in(alloc);
    for (dep, value) in deps.iter().zip(laid.values.iter()) {
        run_deps.push(DepMetadata { links: dep.links, key: dep.key, value: value.as_str() });
    }
    let run_env = blaze_build_script::env(&Given { deps: &run_deps, ..given }, alloc);
    let mut dep_reads: AVec<'a, &str> = Vec::new_in(alloc);
    if let Some(ref ws) = workspace_read {
        dep_reads.push(ws.as_str());
    }
    for folder in laid.folders.iter() {
        dep_reads.push(folder.as_str());
    }
    let mut dep_shown: AVec<'a, (&str, &str)> = Vec::new_in(alloc);
    for (real, shown) in laid.shown.iter() {
        dep_shown.push((real.as_str(), shown.as_str()));
    }
    let run = NativeScript {
        key: base,
        files: &area,
        program: &program,
        c,
        env: &run_env,
        reads: &dep_reads,
        shown: &dep_shown,
        host_tools: &tools,
        facts: Some(&facts),
        jobs: Some(native.jobs),
        limits: RunLimits::STANDARD,
        deadline: native.deadline,
    };
    let ran = run_script(&run, alloc);
    let asked = native_facts::reads(&facts, pins, alloc).map_err(facts_error)?;
    let ran = match ran {
        Ok(ran) => ran,
        Err(run) if !asked.refused.is_empty() => return Err(NativeUnitError::FactRefused { package: pkg(), refused: asked.refused, run }),
        Err(run) => return Err(NativeUnitError::Run(run)),
    };

    // The read list, coarse and correct: every file of the package by its digest now (a native
    // run's own reads are not seen; build-scripts.md, "The record").
    let mut reads: std::vec::Vec<Read> = std::vec::Vec::with_capacity(listed.len());
    for rel in listed {
        let subject = format!("file:{rel}");
        let digest = match answer_now(&tree, &subject) {
            Ok(digest) => digest,
            Err(e) => return Err(NativeUnitError::Files { package: pkg(), folder: package_dir, why: e.0 }),
        };
        reads.push(Read { subject, digest });
    }
    // The facts the script asked (N7), each with the pins' answer now (`None` where refused).
    reads.extend(asked.reads);
    Ok(ScriptRun { outputs: ran.outputs, base, reads, fuel: 0, peak_memory: 0, spawns: 0 })
}

#[cfg(test)]
mod tests {
    use blaze_build_script::{
        env::{Given, PackageFacts},
        key::{self, CMode, KeyInputs, RunLimits},
        native::Runner,
        probe::Table,
        toolchain::Mode,
    };
    use blaze_manifest::lock::LockId;
    use blaze_own::{AVec, Alloc, Vec};
    use blaze_unit_graph::{CfgAtom, Resolver, Text, Unit, UnitDep, UnitGraph, UnitId, UnitKind};
    use edition::PackageEdition;

    use super::{NativeKeyParts, placed, runner_key, sdk_version};

    fn t(s: &str) -> Text<'static> {
        Text::from_str_in(s, Alloc::global())
    }

    fn unit(name: &str, kind: UnitKind<'static>, deps: &[u32], build_script: Option<u32>) -> Unit<'static> {
        let mut d = AVec::new_in(Alloc::global());
        for u in deps {
            d.push(UnitDep { name: t("dep"), unit: UnitId(*u) });
        }
        Unit {
            package: LockId(0),
            name: t(name),
            version: t("1.0.0"),
            kind,
            for_host: true,
            edition: PackageEdition::of_manifest("2021").expect("an edition"),
            crate_name: t(name),
            root: t("build.rs"),
            features: AVec::new_in(Alloc::global()),
            cfg: AVec::new_in(Alloc::global()),
            deps: d,
            build_script: build_script.map(UnitId),
            links: None,
        }
    }

    /// The real path's choice (D65, D69 (3)): zstd-sys's script, whose closure holds `cc`, runs
    /// native for `build` and as wasm for `check` (its C requests deferred); a script whose
    /// closure holds only `autocfg` is wasm in both. The two run keys for `build` differ, so a
    /// record of one runner never answers the other. A placement that ignored the closure
    /// (everything wasm, or everything native) fails the first or second assertion.
    /// Prints "placed: zstd-sys build Native { because: UnitId(1) }, pure build Wasm".
    #[test]
    fn script_unit_places_a_native_class_script_native_for_build_and_keys_it_apart() {
        let alloc = Alloc::global();
        let mut units = AVec::new_in(alloc);
        units.push(unit("zstd-sys", UnitKind::BuildScript, &[1], None)); // 0
        units.push(unit("cc", UnitKind::Lib, &[], None)); // 1
        units.push(unit("pure", UnitKind::BuildScript, &[3], None)); // 2
        units.push(unit("autocfg", UnitKind::Lib, &[], None)); // 3
        units.push(unit("zstd-sys", UnitKind::Lib, &[], Some(0))); // 4
        units.push(unit("pure", UnitKind::Lib, &[], Some(2))); // 5
        let graph = UnitGraph { units, resolver: Resolver::V3, meta: AVec::new_in(alloc), left_out: AVec::new_in(alloc) };

        let native = placed(&graph, UnitId(0), Mode::Build, alloc);
        let wasm = placed(&graph, UnitId(2), Mode::Build, alloc);
        assert_eq!(native, Runner::Native { because: UnitId(1) });
        assert_eq!(wasm, Runner::Wasm);
        assert_eq!(placed(&graph, UnitId(0), Mode::Run, alloc), native);
        assert_eq!(placed(&graph, UnitId(0), Mode::Check, alloc), Runner::Wasm);
        assert_eq!(placed(&graph, UnitId(2), Mode::Check, alloc), Runner::Wasm);

        let cfg = [CfgAtom::flag("unix", alloc)];
        let given = Given {
            package: PackageFacts { name: "zstd-sys", version: "1.0.0", ..PackageFacts::default() },
            seen: blaze_expand::seen::SeenRoot::Package,
            features: &[],
            target_cfg: &cfg,
            target: "aarch64-apple-darwin",
            host: "aarch64-apple-darwin",
            deps: &[],
        };
        let env = blaze_build_script::env(&given, alloc);
        let parts = NativeKeyParts { compiler: [4; 32], target: "aarch64-apple-darwin", sysroot: "macos-sdk 27.2", host_tools: &[] };
        let key_of = |runner: Runner| {
            let inputs = KeyInputs {
                module: [1; 32],
                env: &env,
                answers: Table::EMPTY.id(),
                c_mode: CMode::Built,
                limits: RunLimits::STANDARD,
                runner: runner_key(runner, &parts),
            };
            key::base(&inputs, &mut Vec::new_in(Alloc::global()))
        };
        assert_ne!(key_of(native), key_of(wasm));
        println!("placed: zstd-sys build {native:?}, pure build {wasm:?}");
    }

    /// turso 2026-11-21: turso_sdk_kit's script holds bindgen -> clang-sys -> libloading, which
    /// do not compile for wasm32-wasip1 (bs-1's log: `cannot find Library in libloading`): native
    /// in `check` too, though no name list holds it. TWIN: pg_query, whose closure also holds cc.
    /// CONTROL (rule 23): a script with cc alone (the libloading chain removed) checks as wasm,
    /// its C requests deferred.
    #[test]
    fn a_script_whose_closure_cannot_be_wasm_is_native_in_check() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut units = AVec::new_in(alloc);
        units.push(unit("turso_sdk_kit", UnitKind::BuildScript, &[1], None)); // 0
        units.push(unit("bindgen", UnitKind::Lib, &[2], None)); // 1
        units.push(unit("clang-sys", UnitKind::Lib, &[3], None)); // 2
        units.push(unit("libloading", UnitKind::Lib, &[], None)); // 3
        units.push(unit("pg_query", UnitKind::BuildScript, &[1, 5], None)); // 4
        units.push(unit("cc", UnitKind::Lib, &[], None)); // 5
        units.push(unit("spawns_only", UnitKind::BuildScript, &[5], None)); // 6
        let graph = UnitGraph { units, resolver: Resolver::V3, meta: AVec::new_in(alloc), left_out: AVec::new_in(alloc) };
        for mode in [Mode::Check, Mode::Build, Mode::Run] {
            assert_eq!(placed(&graph, UnitId(0), mode, alloc), Runner::Native { because: UnitId(1) }, "{mode:?}");
            assert_eq!(placed(&graph, UnitId(4), mode, alloc), Runner::Native { because: UnitId(1) }, "{mode:?}");
        }
        assert_eq!(placed(&graph, UnitId(6), Mode::Check, alloc), Runner::Wasm, "CONTROL: cc alone checks as wasm");
        assert_eq!(placed(&graph, UnitId(6), Mode::Build, alloc), Runner::Native { because: UnitId(5) });
    }

    /// The SDK's version is read with or without spaces around the colon, and refused when
    /// absent.
    #[test]
    fn the_sdk_version_is_read_from_its_settings() {
        assert_eq!(sdk_version(r#"{"CanonicalName":"macosx27.2","Version":"27.2"}"#), Some("27.2"));
        assert_eq!(sdk_version("{\n  \"Version\" : \"15.0\",\n}"), Some("15.0"));
        assert_eq!(sdk_version(r#"{"CanonicalName":"macosx"}"#), None);
    }
}
