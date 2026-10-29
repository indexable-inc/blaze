//! Proc macros compiled and linked by blaze (docs/no-rustc.md, step J). When rust-analyzer's
//! `cargo check --compile-time-deps` of a load into blaze's own target directory asks rustc
//! for a proc macro, and the load names a server that reads the list of macros blaze writes
//! into a library ([`SERVER`], `vendor/proc-macro-srv`), the wrapper describes the proc macro
//! and the libraries it links as it does a build script (`own_script.rs`), and blaze links it
//! as a shared library where rustc would have put it, with the dep-info cargo reads. The
//! load expands with it by that server. Cargo's own builds never read that directory, and
//! nothing there is rustc's (`load_wrapper.rs`, `for_load`): the libraries a proc macro links are
//! only described, and a proc macro without a server fails the load.
//!
//! By default a proc macro is built as a WebAssembly module instead ([`WASM`]), which blaze
//! runs in its own process (`crates/blaze-macro-wasm`) and needs no server. One that cannot
//! be built so fails the load, naming it, unless the workspace's root manifest names its
//! package in `native-proc-macros` ([`NATIVE_KEY`]): then it is expanded as it was before.

use std::{
    collections::BTreeMap,
    ffi::OsStr,
    io::Write as _,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use anyhow::Context as _;

use super::{
    own_script::{self, Made},
    sandboxed,
    units::Record,
};

/// The variable that names the proc-macro server that reads the list of macros of a library
/// blaze linked: a path, `1` for the one a release ships beside blaze ([`SHIPPED`]), which
/// is also what is used when it is not set and that server is there, or `0` for none.
pub const SERVER: &str = "BLAZE_PROC_MACRO_SERVER";

/// The server's executable a release has beside blaze's (`.github/scripts/build_release.py`).
pub const SHIPPED: &str = "blaze-proc-macro-srv";

/// The variable that has proc macros expanded natively, `0` or `off`, rather than built as
/// WebAssembly modules and run by blaze, which is what is done when it is unset.
pub const WASM: &str = "BLAZE_PROC_MACRO_WASM";

/// The target a proc macro's module is built for.
pub const WASM_TARGET: &str = "wasm32-wasip1";

/// The cfgs a module is built with beyond its description ([`host_side_cfg`]), which key its
/// build: neither a module a blaze of other cfgs built nor the expansions kept of it are taken
/// for this one's.
pub const MODULE_CFG: &str = "crates that link proc_macro are told no target_os or family";

/// What the crates that link `proc_macro` in the build of a proc macro's module from the
/// `rust-project.json` at `project` are not told of [`WASM_TARGET`]: its `target_os` and its
/// `target_family`, which rustc's build of a proc macro, for the host, does not have.
/// synstructure and syn 1 leave out `proc_macro`, and synstructure its `decl_derive!` with
/// it, for `wasm32` with `target_os = "wasi"`, where no `proc_macro` used to work: the module
/// then had none of the derives declared with it (#828). Every other crate is told the
/// target, by which libc, getrandom and wasi choose what the module's standard library is.
/// `None` when the project is not a proc macro's.
pub fn host_side_cfg(project: &Path) -> Result<Option<project_model::CfgOverrides>, String> {
    let text = std::fs::read_to_string(project)
        .map_err(|it| format!("reading {}: {it}", project.display()))?;
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|it| it.to_string())?;
    let crates = value["crates"].as_array().map(Vec::as_slice).unwrap_or_default();
    if !crates.first().is_some_and(|it| it["is_proc_macro"] == true) {
        return Ok(None);
    }
    let atom = |key: &str, value: &str| cfg::CfgAtom::KeyValue {
        key: intern::Symbol::intern(key),
        value: intern::Symbol::intern(value),
    };
    let target = vec![atom("target_os", "wasi"), atom("target_family", "wasm")];
    let diff = cfg::CfgDiff::new(Vec::new(), target);
    let mut overrides = project_model::CfgOverrides::default();
    for krate in crates {
        let Some(name) = krate["display_name"].as_str() else { continue };
        let root = krate["root_module"].as_str().map(Path::new);
        if links_proc_macro(krate["is_proc_macro"] == true, root) {
            overrides.selective.insert(name.to_owned(), diff.clone());
        }
    }
    Ok(Some(overrides))
}

/// Whether a crate of a proc macro's module links `proc_macro`: a proc macro, or a crate whose
/// root, at `root`, says `extern crate proc_macro`. It is told of the host's side
/// ([`host_side_cfg`]), and the build script of its package runs for the module as for the
/// host (`compile_time.rs`), the platform the script of a crate rustc builds for a proc macro
/// is told of and probes for (proc-macro2's).
pub fn links_proc_macro(is_proc_macro: bool, root: Option<&Path>) -> bool {
    is_proc_macro
        || root
            .and_then(|it| std::fs::read_to_string(it).ok())
            .is_some_and(|text| text.contains("extern crate proc_macro"))
}

/// Has the toolchain at `sysroot` the WASI libc a module is linked with (`link.rs`), which
/// comes with rustup's `wasm32-wasip1` target: rustup adds it when it is not there, once per
/// process. Builds that start together add it once: two rustups that download one component
/// at a time each take the other's file away. `Err` says why there is none.
pub fn wasm_target(sysroot: &Path) -> Result<(), String> {
    static ADDED: std::sync::Mutex<Vec<(PathBuf, Result<(), String>)>> =
        std::sync::Mutex::new(Vec::new());
    let libc = sysroot.join("lib/rustlib").join(WASM_TARGET).join("lib/self-contained");
    if libc.join("crt1-reactor.o").is_file() {
        return Ok(());
    }
    let mut added = ADDED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((_, said)) = added.iter().find(|(it, _)| it == sysroot) {
        return said.clone();
    }
    let toolchain = sysroot.file_name().and_then(OsStr::to_str).unwrap_or_default();
    let mut options = std::fs::OpenOptions::new();
    let lock = options.create(true).truncate(false).write(true);
    let lock = lock.open(std::env::temp_dir().join(format!("blaze-{toolchain}-{WASM_TARGET}")));
    let locked = lock.and_then(|it| it.lock().map(|()| it));
    let said = match locked {
        Ok(_) if libc.join("crt1-reactor.o").is_file() => Ok(()),
        Ok(_locked) => {
            let mut rustup = std::process::Command::new("rustup");
            rustup.args(["target", "add", WASM_TARGET, "--toolchain", toolchain]);
            match rustup.stdin(std::process::Stdio::null()).output() {
                Ok(_) if libc.join("crt1-reactor.o").is_file() => Ok(()),
                Ok(ran) => Err(String::from_utf8_lossy(&ran.stderr).trim_end().to_owned()),
                Err(error) => Err(format!("rustup did not run: {error}")),
            }
        }
        Err(error) => Err(format!("no lock to add it under: {error}")),
    };
    let said = said.map_err(|why| {
        format!(
            "the toolchain at {} has no {WASM_TARGET} target, which a proc macro's module is \
             linked with, and `rustup target add {WASM_TARGET} --toolchain {toolchain}` did not \
             add it: {why}",
            sysroot.display()
        )
    });
    added.push((sysroot.to_owned(), said.clone()));
    said
}

/// Whether proc macros are built as WebAssembly modules ([`WASM`]): not when [`turned_off`].
pub fn wasm() -> bool {
    let off =
        matches!(std::env::var_os(WASM).as_deref().and_then(OsStr::to_str), Some("0" | "off"));
    !off && !turned_off()
}

/// The key, in `[workspace.metadata.blaze]` of a workspace's root manifest, that lists the
/// packages whose proc macros are expanded natively, not as WebAssembly.
pub const NATIVE_KEY: &str = "native-proc-macros";

/// The packages, by crate name (`-` as `_`), the root manifest of the workspace at `root`
/// names in [`NATIVE_KEY`]: none when it has no manifest or does not say.
pub fn native_proc_macros(root: &Path) -> Result<Vec<String>, String> {
    let manifest = root.join("Cargo.toml");
    let Ok(text) = std::fs::read_to_string(&manifest) else { return Ok(Vec::new()) };
    let wrong = |what: &str| {
        format!("`{NATIVE_KEY}` in [workspace.metadata.blaze] of {}: {what}", manifest.display())
    };
    let value: toml::Value = toml::from_str(&text).map_err(|it| wrong(&it.to_string()))?;
    let listed = ["workspace", "metadata", "blaze", NATIVE_KEY]
        .iter()
        .try_fold(&value, |it, key| it.get(*key));
    let Some(listed) = listed else { return Ok(Vec::new()) };
    let names = listed.as_array().ok_or_else(|| wrong("a list of package names"))?;
    let names = names.iter().map(|it| it.as_str().map(|it| it.replace('-', "_")));
    names.collect::<Option<_>>().ok_or_else(|| wrong("a list of package names, as strings"))
}

/// Whether the proc macro of crate `name` that cargo asked rustc for runs as WebAssembly: when
/// the workspace cargo runs rustc in does not run it natively ([`NATIVE_KEY`]).
pub fn runs_as_wasm(name: &str) -> anyhow::Result<bool> {
    let cwd = std::env::current_dir()?;
    let root = sandboxed::workspace_root(&cwd)?.unwrap_or(cwd);
    let native = native_proc_macros(&root).map_err(anyhow::Error::msg)?;
    Ok(!native.iter().any(|it| it == &name.replace('-', "_")))
}

/// The variable blaze sets for the cargo of a load into its own target directory.
pub const OWN_TARGET: &str = "BLAZE_OWN_TARGET";

/// Whether [`SERVER`] turns blaze's proc macros off, `0` or `off`, as the harness's reference
/// builds do: a load's cargo is then told so, and rustc builds what it asks for.
pub fn turned_off() -> bool {
    matches!(std::env::var_os(SERVER).as_deref().and_then(OsStr::to_str), Some("0" | "off"))
}

/// The directory in cargo's target directory of a load's own target directory when blaze
/// links the proc macros, where libraries are only described: cargo is given empty files for
/// them (`load_wrapper.rs`, `described_only`).
pub const OWN_DIR: &str = "rust-analyzer";

/// The same when [`turned_off`], where rustc builds every unit. Cargo does not fingerprint
/// [`SERVER`], so in one directory it would take the empty files of a load with blaze's proc
/// macros for rustc's libraries, and rustc then fails reading them (E0786, #460).
pub const BY_RUSTC_DIR: &str = "rust-analyzer-rustc";

/// The load's own target directory in cargo's, for the mode [`SERVER`] says now.
pub fn own_dir() -> &'static str {
    if turned_off() { BY_RUSTC_DIR } else { OWN_DIR }
}

/// What a load whose proc macro blaze has no server for is told: where the server was
/// looked for, and the two ways through.
pub fn missing_server() -> String {
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| {
            Some(exe.parent()?.join(format!("{SHIPPED}{}", std::env::consts::EXE_SUFFIX)))
        })
        .map_or_else(|| SHIPPED.to_owned(), |it| it.display().to_string());
    format!(
        "no proc-macro server is beside blaze (looked for {beside}), and blaze links proc \
         macros only for its own. Build it there from a blaze checkout with `blaze \
         vendor/proc-macro-srv --crate rust_analyzer_proc_macro_srv --crate proc_macro --out \
         <scratch>/server.o --exe {beside}` (what `.github/scripts/build_release.py --server` \
         runs; the releases ship it on Linux and macOS), or have rustc build the load's proc macros \
         with `BLAZE_PROC_MACRO_SERVER=0`"
    )
}

/// The server that expands with the libraries blaze links, as [`SERVER`] says, relative to
/// the current directory: `None` when it says none, or no server is beside blaze when it
/// says `1` or nothing. A load names it to its cargo, or takes the variable away, so the
/// wrapper links a proc macro only for a server there is.
pub fn server() -> Option<&'static Path> {
    static FOUND: OnceLock<Option<PathBuf>> = OnceLock::new();
    FOUND.get_or_init(find).as_deref()
}

fn find() -> Option<PathBuf> {
    let named = std::env::var_os(SERVER);
    match named.as_deref().and_then(OsStr::to_str) {
        Some("0" | "off") => return None,
        Some("1") | None => {}
        Some(_) => return named.and_then(|it| std::path::absolute(it).ok()),
    }
    let exe = std::env::current_exe().ok()?;
    let shipped = exe.parent()?.join(format!("{SHIPPED}{}", std::env::consts::EXE_SUFFIX));
    if shipped.is_file() {
        return Some(shipped);
    }
    // A build of blaze that has no server, as one from crates.io, links no proc macro: a load
    // into its own target directory that has one fails.
    if named.is_some() {
        eprintln!("warning: no proc-macro server is beside {}", exe.display());
    }
    None
}

/// `blaze proc-macro-library <sysroot> <library>`: makes blaze's library of the proc macro
/// rustc compiled at `<library>` and prints where it is ([`libraries_of`]).
pub const COMMAND: &str = "proc-macro-library";

/// The directory, in the record of a proc macro rustc compiled, of blaze's library of it.
const OWN: &str = "blaze";

/// What stands for each of `libraries`, proc macros rustc compiled in a build through the
/// wrapper, which recorded them (`units.rs`), each with its crate's name, in the workspace at
/// `workspace`: the store's or a new build from source over the standard library of
/// `sysroot`, each in a process of its own. A WebAssembly module ([`wasm`]), or blaze's
/// native library for [`server`] when the workspace opts into it ([`NATIVE_KEY`]). A
/// missing sysroot or server, or a failed compilation, is an error naming the macro.
pub fn libraries_of(
    sysroot: Option<&Path>,
    workspace: &Path,
    libraries: &[Compiled],
) -> Vec<Result<Option<PathBuf>, String>> {
    libraries_of_with_env(sysroot, workspace, libraries, &rustc_hash::FxHashMap::default())
}

/// Like [`libraries_of`], with a request's environment for the compiler subprocesses.
pub fn libraries_of_with_env(
    sysroot: Option<&Path>,
    workspace: &Path,
    libraries: &[Compiled],
    env: &rustc_hash::FxHashMap<String, Option<String>>,
) -> Vec<Result<Option<PathBuf>, String>> {
    let wasm = wasm();
    if !wasm && server().is_none() {
        return vec![Err("blaze has no native proc-macro server".to_owned()); libraries.len()];
    }
    let native = match wasm {
        true => {
            let root = sandboxed::workspace_root(workspace).ok().flatten();
            match native_proc_macros(root.as_deref().unwrap_or(workspace)) {
                Ok(it) => it,
                Err(why) => return vec![Err(why); libraries.len()],
            }
        }
        false => Vec::new(),
    };
    let refused = |name: &str, why: &str| refusal(name, why, workspace);
    let Some(sysroot) = sysroot else {
        let why = "the workspace has no sysroot to build it over";
        return libraries.iter().map(|Compiled { name, .. }| Err(refused(name, why))).collect();
    };
    let Ok(exe) = std::env::current_exe() else {
        return vec![Err("blaze does not know where it is".to_owned()); libraries.len()];
    };
    // The loader supplies dependencies first. Each later compiler expands with the
    // immutable artifacts already made for this request, never the old Cargo outputs.
    let mut paths = BTreeMap::new();
    let mut made = Vec::with_capacity(libraries.len());
    for Compiled { name, library, description } in libraries {
        let as_wasm = wasm && !native.contains(name);
        let found = (|| -> Result<PathBuf, String> {
            if !as_wasm && server().is_none() {
                return Err("blaze has no native proc-macro server".to_owned());
            }
            let mut command = std::process::Command::new(&exe);
            command.current_dir(workspace);
            for (key, value) in env {
                match value {
                    Some(value) => _ = command.env(key, value),
                    None => _ = command.env_remove(key),
                }
            }
            command.arg(COMMAND);
            if let Some(description) = description {
                let description = with_macro_paths(description, &paths)?;
                command
                    .arg(if as_wasm { WASM_FLAG } else { NATIVE_FLAG })
                    .arg(described_at(library, &description)?);
            } else if as_wasm {
                return Err("the load can't describe its crate".to_owned());
            }
            if !paths.is_empty() {
                command.arg(MACROS_FLAG).arg(macro_paths_at(library, &paths)?);
            }
            command.arg(sysroot).arg(library);
            if let Some((var, dir)) = crate::trace::child_env() {
                command.env(var, dir);
            }
            let ran = tracing::info_span!(target: "blaze", "make proc macro", subject = %name)
                .in_scope(|| command.stdin(std::process::Stdio::null()).output())
                .map_err(|it| it.to_string())?;
            if !ran.status.success() {
                return Err(String::from_utf8_lossy(&ran.stderr).trim_end().to_owned());
            }
            let said = String::from_utf8_lossy(&ran.stdout).trim_end().to_owned();
            if said.is_empty() {
                return Err("the compiler produced no library".to_owned());
            }
            Ok(PathBuf::from(said))
        })();
        match found {
            Ok(path) => {
                if let Ok(original) = library.canonicalize() {
                    paths.insert(original, path.clone());
                }
                paths.insert(library.clone(), path.clone());
                made.push(Ok(Some(path)));
            }
            Err(why) => {
                let why = if as_wasm {
                    refused(name, &why)
                } else {
                    format!("blaze did not make the proc macro {name}: {why}")
                };
                made.resize(libraries.len(), Err(why));
                break;
            }
        }
    }
    made
}

/// A proc macro rustc compiled: its crate's name, its library, and the `rust-project.json`
/// of the crate and what it depends on, which its module is built from.
pub struct Compiled {
    pub name: String,
    pub library: PathBuf,
    pub description: Option<String>,
}

/// Why the load fails when the proc macro of crate `name` in the workspace at `workspace`
/// cannot be built or run as a WebAssembly module, and the ways out.
pub fn refusal(name: &str, why: &str, workspace: &Path) -> String {
    let root = sandboxed::workspace_root(workspace).ok().flatten();
    let root = root.as_deref().unwrap_or(workspace);
    format!(
        "the proc macro `{name}` cannot run as WebAssembly: {why}\n  name its package in \
         `{NATIVE_KEY}` under [workspace.metadata.blaze] in {} to expand it natively, or set \
         BLAZE_PROC_MACRO_WASM=0 for every proc macro",
        root.join("Cargo.toml").display(),
    )
}

/// The flag of [`COMMAND`] that has it make a WebAssembly module rather than a library, from
/// the description in the file after it.
const WASM_FLAG: &str = "--wasm";
const NATIVE_FLAG: &str = "--native";
const MACROS_FLAG: &str = "--macro-libraries";

fn with_macro_paths(
    description: &str,
    paths: &BTreeMap<PathBuf, PathBuf>,
) -> Result<String, String> {
    let mut value: serde_json::Value =
        serde_json::from_str(description).map_err(|it| it.to_string())?;
    let crates = value["crates"].as_array_mut().ok_or("the description has no crates")?;
    for krate in crates {
        if let Some(path) =
            krate["proc_macro_dylib_path"].as_str().and_then(|it| paths.get(Path::new(it)))
        {
            krate["proc_macro_dylib_path"] = serde_json::json!(path);
        }
    }
    Ok(value.to_string())
}

fn write_input(file: &Path, bytes: &[u8]) -> Result<(), String> {
    let dir = file.parent().ok_or("an input without a directory")?;
    std::fs::create_dir_all(dir).map_err(|it| it.to_string())?;
    let mut new = tempfile::NamedTempFile::new_in(dir).map_err(|it| it.to_string())?;
    new.write_all(bytes).map_err(|it| it.to_string())?;
    new.persist(file).map_err(|it| it.to_string())?;
    Ok(())
}

fn macro_paths_at(library: &Path, paths: &BTreeMap<PathBuf, PathBuf>) -> Result<PathBuf, String> {
    use sha2::Digest as _;
    let bytes = serde_json::to_vec(paths).map_err(|it| it.to_string())?;
    let digest = crate::content::hex(&sha2::Sha256::digest(&bytes));
    let dir = library.parent().ok_or("a library without a directory")?;
    let file = dir.join("blaze-macro-inputs").join(format!("{digest}.json"));
    write_input(&file, &bytes)?;
    Ok(file)
}

/// Publish immutable bytes so another environment or compiler cannot overwrite a loaded DLL.
fn publish(dir: &Path, made: &Path) -> Result<PathBuf, String> {
    use sha2::Digest as _;
    let bytes = std::fs::read(made).map_err(|it| it.to_string())?;
    let digest = crate::content::hex(&sha2::Sha256::digest(&bytes));
    let file = dir.join(digest).join(made.file_name().ok_or("a library unnamed")?);
    if std::fs::read(&file).ok().as_deref() != Some(&bytes) {
        write_input(&file, &bytes)?;
    }
    Ok(file)
}

/// The directory, beside rustc's library, of the modules blaze builds of it.
const WASM_DIR: &str = "blaze-wasm";

/// Writes `description`, of the proc macro rustc compiled at `library`, where its module is
/// built, a directory of its own for each description.
fn described_at(library: &Path, description: &str) -> Result<PathBuf, String> {
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(description.as_bytes());
    let digest = crate::content::hex(&digest[..8]);
    let stem = library.file_stem().and_then(OsStr::to_str).ok_or("a library unnamed")?;
    let dir = library.parent().ok_or("a library without a directory")?;
    let dir = dir.join(WASM_DIR).join(format!("{stem}-{digest}"));
    std::fs::create_dir_all(&dir).map_err(|it| format!("making {}: {it}", dir.display()))?;
    let file = dir.join("rust-project.json");
    write_input(&file, description.as_bytes())?;
    Ok(file)
}

/// [`COMMAND`]. It prints nothing for a library rustc compiled in a build that did not go
/// through the wrapper, which recorded nothing of it.
pub fn command(args: impl Iterator<Item = std::ffi::OsString>) -> anyhow::Result<()> {
    let mut args = args.peekable();
    let described = match args.next_if(|it| it == WASM_FLAG || it == NATIVE_FLAG) {
        Some(flag) => Some((
            PathBuf::from(args.next().context("a description after --wasm or --native")?),
            flag == WASM_FLAG,
        )),
        None => None,
    };
    let macros: BTreeMap<PathBuf, PathBuf> = match args.next_if(|it| it == MACROS_FLAG) {
        Some(_) => {
            let file = PathBuf::from(args.next().context("a path map after --macro-libraries")?);
            serde_json::from_slice(&std::fs::read(file)?)?
        }
        None => BTreeMap::new(),
    };
    let sysroot = PathBuf::from(args.next().context("the sysroot is named first")?);
    let library = PathBuf::from(args.next().context("then rustc's library")?);
    let made = match &described {
        Some((described, wasm)) => {
            described_library(&sysroot, &library, described, *wasm).map(Some)
        }
        None => library_of(&sysroot, &library, &macros),
    };
    match made {
        Ok(own) => {
            if let Some(own) = own {
                println!("{}", own.display());
            }
            Ok(())
        }
        Err(why) => {
            eprintln!("{why}");
            std::process::exit(1)
        }
    }
}

/// A native library or WebAssembly module built from the load's current description.
fn described_library(
    sysroot: &Path,
    library: &Path,
    described: &Path,
    wasm: bool,
) -> Result<PathBuf, String> {
    let description = std::fs::read_to_string(described).map_err(|it| it.to_string())?;
    let value: serde_json::Value =
        serde_json::from_str(&description).map_err(|it| it.to_string())?;
    let name = value["crates"][0]["display_name"].as_str().ok_or("a proc macro unnamed")?;
    let dir = described.parent().ok_or("a description without a directory")?;
    let stem = library.file_stem().and_then(OsStr::to_str).unwrap_or(name);
    let scratch = tempfile::Builder::new()
        .prefix("building-")
        .tempdir_in(dir)
        .map_err(|it| it.to_string())?;
    let made = Made {
        name,
        proc_macro: true,
        wasi: false,
        wasm,
        exe: scratch.path().join(if wasm {
            format!("{stem}.wasm")
        } else {
            library
                .file_name()
                .and_then(OsStr::to_str)
                .ok_or("a native library unnamed")?
                .to_owned()
        }),
        dep_info: scratch.path().join(format!("{stem}.d")),
    };
    own_script::compile_described(sysroot, &description, &made, scratch.path())?;
    let file = publish(dir, &made.exe)?;
    // Here, as soon as it is built and beside the other modules' builds, rather than in the
    // load, which loads one module after another.
    if wasm {
        blaze_macro_wasm::prepare(&file, own_script::store())?;
    }
    Ok(file)
}

/// blaze's library of the proc macro rustc compiled at `library`, made from its record over
/// the standard library of `sysroot`, in that record. `None` when it has no record.
fn library_of(
    sysroot: &Path,
    library: &Path,
    macros: &BTreeMap<PathBuf, PathBuf>,
) -> Result<Option<PathBuf>, String> {
    let (prefix, suffix) = (std::env::consts::DLL_PREFIX, std::env::consts::DLL_SUFFIX);
    let file = library.file_name().and_then(OsStr::to_str).ok_or("a library unnamed")?;
    let stem = file.strip_prefix(prefix).and_then(|it| it.strip_suffix(suffix));
    let stem = stem.ok_or("not a library")?;
    let dir = library.parent().ok_or("a library without a directory")?;
    let unit = &super::layout::unit_of_file(stem, dir);
    let Some((record, records)) = Record::described_or_seeded(dir, unit) else { return Ok(None) };
    let dir = records.join(unit).join(OWN);
    std::fs::create_dir_all(&dir).map_err(|it| it.to_string())?;
    let scratch = tempfile::Builder::new()
        .prefix("building-")
        .tempdir_in(&dir)
        .map_err(|it| it.to_string())?;
    let extra = record.codegen("extra-filename").unwrap_or_default();
    let made = Made {
        name: &record.crate_name,
        proc_macro: true,
        wasi: false,
        wasm: false,
        exe: scratch.path().join(file),
        dep_info: scratch.path().join(format!("{}{extra}.d", record.crate_name)),
    };
    own_script::compile_record_with_macros(sysroot, &record, &made, scratch.path(), macros)?;
    Ok(Some(publish(&dir, &made.exe)?))
}

/// What blaze makes of the proc macro `record` is of: its library, or with `wasm` its module,
/// where rustc would have put the library, and the dep-info cargo reads. `None` when cargo
/// names no place for them.
pub fn made(record: &Record, wasm: bool) -> Option<Made<'_>> {
    let (prefix, suffix) = (std::env::consts::DLL_PREFIX, std::env::consts::DLL_SUFFIX);
    let name = record.crate_name.as_str();
    let extra = record.codegen("extra-filename").unwrap_or_default();
    let out_dir = record.out_dir()?;
    Some(Made {
        name,
        proc_macro: true,
        wasi: false,
        wasm,
        exe: out_dir.join(format!("{prefix}{name}{extra}{suffix}")),
        dep_info: out_dir.join(format!("{name}{extra}.d")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_crates_that_link_proc_macro_are_told_the_host() {
        let dir = tempfile::tempdir().unwrap();
        let root = |name: &str, text: &str| {
            let file = dir.path().join(format!("{name}.rs"));
            std::fs::write(&file, text).unwrap();
            file
        };
        let gated = "#[cfg(not(all(target_arch = \"wasm32\", target_os = \"wasi\")))]\n\
                     extern crate proc_macro;\n";
        let crates = [
            ("derives", root("derives", "use synstructure::decl_derive;\n"), true),
            ("synstructure", root("synstructure", gated), false),
            ("getrandom", root("getrandom", "#[cfg(target_os = \"wasi\")]\nmod wasi;\n"), false),
        ];
        let described = |crates: &[(&str, PathBuf, bool)]| {
            let crates = crates.iter().map(|(name, root, proc_macro)| {
                serde_json::json!({
                    "display_name": name, "root_module": root, "is_proc_macro": proc_macro,
                })
            });
            let file = dir.path().join("rust-project.json");
            let crates: Vec<_> = crates.collect();
            std::fs::write(&file, serde_json::json!({ "crates": crates }).to_string()).unwrap();
            file
        };
        let told = host_side_cfg(&described(&crates)).unwrap().unwrap();
        let mut names: Vec<_> = told.selective.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(names, ["derives", "synstructure"]);
        let mut options = cfg::CfgOptions::default();
        let target = [("target_os", "wasi"), ("target_family", "wasm"), ("target_arch", "wasm32")];
        for (key, value) in target {
            options.insert_key_value(intern::Symbol::intern(key), intern::Symbol::intern(value));
        }
        let mut host = options.clone();
        told.apply(&mut host, "synstructure");
        let os = cfg::CfgAtom::KeyValue {
            key: intern::Symbol::intern("target_os"),
            value: intern::Symbol::intern("wasi"),
        };
        assert!(!host.check_atom(&os));
        told.apply(&mut options, "getrandom");
        assert!(options.check_atom(&os));
        assert!(host_side_cfg(&described(&crates[1..])).unwrap().is_none());
    }
}
