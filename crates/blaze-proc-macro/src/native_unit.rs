//! D65, D69 and D71 on the real path: a build-script unit placed by [`placed`] (its dependency
//! closure and the command's mode, before anything runs) and, when native, run by
//! [`native_outputs`]: its host binary from the module records (`script.bin`, built by the
//! interim builder's `native` mode), the same environment the wasm path gives (the unit's own
//! features, the served unit's cfg), its run key with the pinned compiler, the SDK's version
//! and any granted host tool's digest, its record found or run confined
//! ([`crate::native_script::run_script`]) and kept. A native failure is a typed error: it is
//! never tried again as wasm (rule 3), and a wasm failure never native.

use std::{path::Path, time::Duration};

use blaze_build_script::{
    env::{DepMetadata, Given, PackageFacts},
    host_tools::{self, HostTool},
    key::{self, CMode, HostToolKey, KeyInputs, RunLimits, RunnerKey},
    native::{self, Runner},
    probe::Table,
    toolchain::{self, CInputs, Mode, PIN, Shelf, Sysroot},
};
use blaze_ids::ContentId;
use blaze_own::{AVec, Alloc, Vec};
use blaze_unit_graph::{UnitGraph, UnitId};
use bstd::{
    fs::Durable,
    mem::{Bytes, TASK_FRAME},
};

use crate::{
    native_script::{NativeScript, NativeScriptError, files_under, remove, run_script},
    record::native_world,
    script::{RecordError, ScriptRun, ScriptRuntime},
    script_sandbox::{Read, Tree, answer_now},
    script_unit::module_of_native,
};

/// The deployment target every native script compiles for: a constant, so the key's domain
/// covers it (the interim builder links the scripts for the same).
pub const MACOS_MIN: &str = "11.0";

/// Where `script` (a build-script unit) runs for `mode`: the closure's verdict
/// ([`native::runner`]) placed by the mode ([`toolchain::placement`], D69 (3)).
pub fn placed(graph: &UnitGraph<'_>, script: UnitId, mode: Mode, alloc: Alloc<'_>) -> Runner {
    toolchain::placement(native::runner(graph, script, alloc), mode, graph.units[script.index()].name.as_str())
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
    /// A file blaze reads to make the run (the module, the SDK's settings, a host tool).
    Read { path: String, why: String },
    /// The pin table does not say what the run needs.
    PinTable { package: String, why: String },
    /// A host tool's path in the pin table is not its resolved real path (D71).
    ToolNotReal { package: String, tool: String, path: String, real: String },
    /// The SDK's `SDKSettings.json` has no `Version`.
    SdkVersion { sdk: String },
    /// The shelf's folder is not named by a SHA-256.
    Shelf { shelf: String },
    /// The package's or the run's folder could not be made or read.
    Files { package: String, folder: String, why: String },
    /// The record could not be found or kept.
    Record { package: String, error: RecordError },
    /// The run itself: refused, failed, or its outputs.
    Run(NativeScriptError),
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
            NativeUnitError::Read { path, why } => write!(f, "{path}: {why}"),
            NativeUnitError::PinTable { package, why } => write!(f, "the native build script of {package}: {why}"),
            NativeUnitError::ToolNotReal { package, tool, path, real } => write!(
                f,
                "{}: host tool `{tool}` of {package}'s build script is at {path}, whose real path is {real}: the grant takes only a resolved path (D71)",
                toolchain::PIN_PATH
            ),
            NativeUnitError::SdkVersion { sdk } => write!(f, "{sdk}/SDKSettings.json: no `Version`, which a native script's key holds (D69)"),
            NativeUnitError::Shelf { shelf } => write!(f, "{shelf}: the C toolchain's shelf folder is not named by its SHA-256 (D69)"),
            NativeUnitError::Files { package, folder, why } => write!(f, "{folder} (the native build script of {package}): {why}"),
            NativeUnitError::Record { package, error } => write!(f, "the native build script of {package}: {error}"),
            NativeUnitError::Run(error) => write!(f, "{error}"),
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

/// The `Version` of an `SDKSettings.json`'s text: the first `"Version"` key's string.
pub fn sdk_version(settings: &str) -> Option<&str> {
    let at = settings.find("\"Version\"")? + "\"Version\"".len();
    let rest = settings[at..].trim_start().strip_prefix(':')?.trim_start().strip_prefix('"')?;
    rest.split('"').next().filter(|v| !v.is_empty())
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
        for (name, _) in files_under(&folder).map_err(|e| failed(e.to_string()))? {
            remove(&folder, &name).map_err(|e| failed(e.to_string()))?;
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
    }
    if files.is_empty() {
        remove(&folder, marker).map_err(|e| failed(e.to_string()))?;
    }
    Ok(folder)
}

/// `script`'s outputs, run native: from its record when one answers, else run confined and
/// kept. `serves` is the unit it serves (its cfg is the script's `CARGO_CFG_*`), `because` the
/// toolchain-class unit that made it native (for the error when no setting is given).
#[allow(clippy::too_many_arguments)]
pub fn native_outputs<'a>(
    runtime: &ScriptRuntime,
    setting: &crate::script_unit::Setting<'_>,
    graph: &UnitGraph<'_>,
    script: UnitId,
    serves: UnitId,
    because: UnitId,
    tree: Tree,
    folder: String,
    deps: &[DepMetadata<'_>],
    alloc: Alloc<'a>,
) -> Result<ScriptRun<'a>, NativeUnitError> {
    let unit = &graph.units[script.index()];
    let (name, version) = (unit.name.as_str(), unit.version.as_str());
    let package = format!("{name} {version}");
    let Some(native) = setting.native else {
        let because = &graph.units[because.index()];
        return Err(NativeUnitError::NoNativeSetting { package, because: format!("{} {}", because.name.as_str(), because.version.as_str()) });
    };

    // The host binary the interim builder made with exactly the unit's features.
    let features: std::vec::Vec<&str> = unit.features.iter().map(|f| f.as_str()).collect();
    let module = module_of_native(setting.store, name, version, &features, setting.host).ok_or_else(|| NativeUnitError::NoModule {
        package: package.clone(),
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
    let facts = PackageFacts { name, version, links: unit.links.as_ref().map(|l| l.as_str()), ..PackageFacts::default() };
    let given = Given { package: facts, folder: &folder, features: &unit.features, target_cfg: &serves_unit.cfg, target: setting.target, host: setting.host, deps };
    let env = blaze_build_script::env(&given, alloc);

    // D71: the host tools the table grants this package, each at its real path, by digest.
    let pin = |why: String| NativeUnitError::PinTable { package: package.clone(), why };
    let table = host_tools::host_tools(PIN, setting.host, alloc).map_err(|e| pin(e.to_string()))?;
    let tools: AVec<'a, HostTool<'_>> = host_tools::granted(&table, name, alloc);
    let mut tool_keys: AVec<'a, HostToolKey<'_>> = Vec::new_in(alloc);
    for tool in tools.iter() {
        let real = std::fs::canonicalize(tool.path).map_err(|e| NativeUnitError::Read { path: tool.path.to_owned(), why: e.to_string() })?;
        if real.as_os_str() != tool.path {
            return Err(NativeUnitError::ToolNotReal { package: package.clone(), tool: tool.name.to_owned(), path: tool.path.to_owned(), real: real.display().to_string() });
        }
        let (dir, file) = tool.path.rsplit_once('/').unwrap_or(("/", tool.path));
        let digest = blaze_sha256::digest(&read_file(if dir.is_empty() { "/" } else { dir }, file)?);
        tool_keys.push(HostToolKey { name: tool.name, digest });
    }

    // The key: the compiler by its shelf digest, the SDK by its version (D69), the tools.
    match toolchain::sysroot(PIN, setting.host) {
        Ok(Sysroot::MacosSdk) => {}
        Ok(Sysroot::Pinned { .. }) => return Err(pin(format!("{}: {}'s pinned sysroot is not fetched yet (D69)", toolchain::PIN_PATH, setting.host))),
        Err(error) => return Err(pin(error.to_string())),
    }
    let settings = String::from_utf8(read_file(native.sdk, "SDKSettings.json")?).map_err(|_| NativeUnitError::SdkVersion { sdk: native.sdk.to_owned() })?;
    let sysroot_key = format!("macos-sdk {}", sdk_version(&settings).ok_or_else(|| NativeUnitError::SdkVersion { sdk: native.sdk.to_owned() })?);
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
    let base: ContentId = key::base(&inputs, &mut scratch);
    let record = |error| NativeUnitError::Record { package: package.clone(), error };
    if let Some(outputs) = runtime.find(&base, &tree, alloc).map_err(record)? {
        return Ok(ScriptRun { outputs, base, from_record: true, reads: std::vec::Vec::new(), fuel: 0, peak_memory: 0, spawns: 0 });
    }

    // The package on disk (a workspace member's folder as it is; a fetched package written out
    // under its key) and an empty `OUT_DIR`.
    let key_hex = hex(&base.0);
    let (package_dir, listed): (String, std::vec::Vec<String>) = match &tree {
        Tree::Dir(dir) => {
            let real = std::fs::canonicalize(dir).map_err(|e| NativeUnitError::Files { package: package.clone(), folder: dir.display().to_string(), why: e.to_string() })?;
            let real = real.to_str().ok_or_else(|| NativeUnitError::Files { package: package.clone(), folder: dir.display().to_string(), why: "not UTF-8".to_owned() })?.to_owned();
            let names = files_under(&real).map_err(|e| NativeUnitError::Files { package: package.clone(), folder: real.clone(), why: e.to_string() })?;
            let names = names.into_iter().map(|(n, _)| n).collect();
            (real, names)
        }
        Tree::Mem { files, .. } => {
            let dir = fresh_folder(native.work, &format!("pkg/{key_hex}"), files, &package)?;
            (dir, files.iter().map(|(n, _)| n.clone()).collect())
        }
    };
    let out_dir = fresh_folder(native.work, &format!("out/{key_hex}"), &[], &package)?;
    let package_name = format!("{name}-{version}");
    let c = CInputs { shelf: Shelf { dir: native.shelf }, sysroot: native.sdk, out_dir: &out_dir, package: &package_dir, package_name: &package_name, macos_min: MACOS_MIN };
    let run = NativeScript { program: &program, c, env: &env, reads: &[], host_tools: &tools, limits: RunLimits::STANDARD, deadline: native.deadline };
    let ran = run_script(&run, alloc).map_err(NativeUnitError::Run)?;

    // The read list, coarse and correct: every file of the package by its digest now (a native
    // run's own reads are not seen; build-scripts.md, "The record").
    let mut reads: std::vec::Vec<Read> = std::vec::Vec::with_capacity(listed.len());
    for rel in listed {
        let subject = format!("file:{rel}");
        let digest = answer_now(&tree, &subject).map_err(|e| NativeUnitError::Files { package: package.clone(), folder: package_dir.clone(), why: e.0 })?;
        reads.push(Read { subject, digest });
    }
    runtime.keep_as(base, &reads, &ran.outputs, host_tools::run_kind(!tools.is_empty())).map_err(record)?;
    Ok(ScriptRun { outputs: ran.outputs, base, from_record: false, reads, fuel: 0, peak_memory: 0, spawns: 0 })
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
            incomplete: false,
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
        let graph = UnitGraph { units, unavailable: AVec::new_in(alloc), resolver: Resolver::V3 };

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
            folder: "",
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

    /// The SDK's version is read with or without spaces around the colon, and refused when
    /// absent.
    #[test]
    fn the_sdk_version_is_read_from_its_settings() {
        assert_eq!(sdk_version(r#"{"CanonicalName":"macosx27.2","Version":"27.2"}"#), Some("27.2"));
        assert_eq!(sdk_version("{\n  \"Version\" : \"15.0\",\n}"), Some("15.0"));
        assert_eq!(sdk_version(r#"{"CanonicalName":"macosx"}"#), None);
    }
}
