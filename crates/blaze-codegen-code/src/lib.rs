//! The content identity of blaze's code generator (`hir_ty_codegen::v2::CodegenCode`): what
//! `Toolchain::id`, and so every stored dev template, is keyed by in place of the bytes of the
//! blaze executable. The build script of the program that runs the code generator works it out
//! ([`emit`]) and sets `BLAZE_CODEGEN_CODE`; the program hands it over before any command runs.
//! An edit outside the code generator's packages (the CLI, the daemon, the lints) keeps it, so a
//! rebuilt blaze reads the templates the last one kept.
//!
//! It is of every input the code generator's code is built from ([`Worked::inputs`] lists each):
//!
//! 1. Every package the [`ROOTS`] the program links are built from: their normal and build
//!    dependencies, transitively, read from `Cargo.lock` and the manifests of the checkout's
//!    packages, never by running cargo (a build script's `$CARGO` is blaze under blaze, AGENTS.md
//!    rule 2). The lock lists every platform's and every feature's dependencies, a superset of
//!    what the build compiles; a checkout package's dev-dependencies are left out by its
//!    manifest. A registry package by name, version and its checksum in the lock file; a package
//!    of the checkout by every file under its directory, by path relative to it and its bytes,
//!    but a name starting with `.` and, at its root, `target` and the `tests`, `benches` and
//!    `examples` directories when none of its library, binaries or build script is in them. A git
//!    package is refused, naming it: it has no checksum in the lock.
//! 2. Every manifest of the workspace's members, the root's and that of each workspace a package
//!    of the checkout inherits from: features one package turns on in another, profiles,
//!    patches; and cargo's configuration files (`.cargo/config.toml`, or `.cargo/config`) in the
//!    workspace's root and every directory above it, and in `$CARGO_HOME`. Not seen: `--config`
//!    on cargo's command line, and a configuration file made later where there was none (a build
//!    script is rerun only on files that exist).
//! 3. The files outside every package that the packages read ([`EXTRA`]) and the variables
//!    they or their build scripts read ([`READ_ENV`]).
//! 4. The stencil libraries, when blaze-stencil is a package of the closure: each by the id its
//!    pin (`crates/blaze-stencil/stencils.json`) names, which blaze-stencil's build script holds
//!    every library it includes to, made by LLVM or copied (this crate includes its reader,
//!    crates/blaze-stencil/pin.rs). The pin is a file of blaze-stencil's too.
//! 5. LLVM, when `llvm-sys` is a package of the closure (blaze-stencil's build script makes the
//!    stencils with it in every build but the seed's; the `llvm` feature links it too), and the
//!    program is not built as the seed ([`SEED_FEATURE`], which has no LLVM and whose stencils
//!    are the pinned ones, step 4): the pin, as
//!    `crates/hir-ty-codegen/llvm.json` says it (version, commit and the sha256 of the release
//!    for `TARGET` and for `HOST`), since vendor/llvm-sys/pin.rs refuses every other LLVM; and,
//!    when `$LLVM_SYS_231_PREFIX` is set, its `blaze-llvm.json`, which pin.rs checks is the pin
//!    (this crate includes pin.rs: one check, one refusal). An unset prefix is no error here:
//!    llvm-sys's own build script stops the build that needs it.
//! 6. How blaze is built: `TARGET`, `HOST`, `PROFILE`, `OPT_LEVEL`, `DEBUG`, every
//!    `CARGO_CFG_*`, `CARGO_PROFILE_*` and `CARGO_FEATURE_*` the build script is given,
//!    `CARGO_ENCODED_RUSTFLAGS`, and the compiler that builds it (`$RUSTC -vV`). blaze, as
//!    `rustc`, ends its `-vV` with a `blaze-identity:` line naming its own code identity
//!    ([`BLAZE_IDENTITY`]), so a blaze built by blaze is another code than one rustc built from
//!    the same tree, and one built by another blaze another again.
//!
//! No absolute path is in it, so one tree checked out twice is one code generator. Not in it,
//! named: the C compiler, its flags and its SDK, which C of registry and vendored packages is
//! built with ([`unkeyed_env`]).
//!
//! An input that can't be read stops the build with an error naming it, never a coarser key
//! (AGENTS.md rule 3). hir-ty-codegen's test `the_code_identity_hashes_every_input_the_code_generator_read`
//! holds the list to what rustc says each package read (its dep-info) and to what each package's
//! build script said it reads (`rerun-if-env-changed`, `rerun-if-changed`).
//!
//! Code outside the closure that the code generator or front end runs through `dyn` hooks: what
//! shapes the code generator's output is a root ([`ROOTS`]); the rest is listed in
//! [`REVIEWED_HOOKS`], each with why it can't change the bytes made, and a test holds every
//! implementation of a hook trait in the checkout to one or the other.
//!
//! The window it leaves, accepted (AGENTS.md rule 19): the build script hashes the tree once,
//! and cargo compiles the closure's crates before or after it. A file saved in the live tree
//! between the two gives one binary an identity of other sources than its code, in either order.
//! [`emit`] refuses a file, lock or manifest read, or a package directory listed, that changed
//! while it worked (each is stamped before it is read and checked at the end). The runner and
//! land.py build from snapshots, which nothing writes to, so only a direct build in a tree being
//! edited is open; working the identity out again over the exact tree a checkpoint or landing
//! built and comparing it with the binary's closes that (indexable-inc/blaze#1619).
//!
//! TEMPORARY in its granularity: a whole package is one input, so an edit anywhere in a package
//! of the code generator misses every template. It is replaced by the code generator's
//! behaviour identity, `core::reflect::fn_info(<codegen entry>).code_id`, once const eval
//! answers `fn_info` (docs/v2/plan/design/code-identity.md, "Toolchain::id").

pub mod fixture;
#[cfg(test)]
mod tests;

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    process::Command,
    time::SystemTime,
};

/// The domain of the code identity; another formula is another domain.
pub const DOMAIN: &str = "blaze v2 codegen code 3";
/// The domain of one input's digest.
const INPUT_DOMAIN: &str = "blaze v2 codegen code input 1";
/// The code generator: the program must link it.
pub const CODE_GENERATOR: &str = "hir-ty-codegen";

/// A package whose code shapes the code generator's output, and every package it is built from.
#[derive(Debug)]
pub struct Root {
    pub package: &'static str,
    /// Why its code shapes the bytes the code generator makes.
    pub why: &'static str,
}

/// The packages whose code shapes the generated code, when the program links them: the code
/// generator, and the code installed into the front end or the code generator through `dyn`
/// hooks from past it (`blaze-items`' `GoalFacts` is in the code generator's closure already).
pub const ROOTS: &[Root] = &[
    Root { package: CODE_GENERATOR, why: "the code generator, the front end it reads and the backends it drives" },
    Root {
        package: "blaze-item-hash",
        why: "`Bodies` is installed as `hir_ty::body_store::BodyFacts` (blaze-cargo \
              serve/unit/deps.rs): its `identity`, `classify`, `value` and `def_of` choose the \
              built MIR and inference results the code generator compiles",
    },
];

/// A `dyn` hook of the front end or code generator, implemented past the closure by code that
/// can't change the bytes the code generator makes.
#[derive(Debug)]
pub struct Hook {
    pub package: &'static str,
    /// The hook trait, by its name.
    pub hook: &'static str,
    pub why: &'static str,
}

/// The hook traits defined in the closure whose implementations the test checks: each one in the
/// checkout, past its tests, is in the closure, or listed in [`REVIEWED_HOOKS`].
pub const HOOK_TRAITS: &[&str] = &[
    "BodyFacts",
    "GoalFacts",
    "ImageStore",
    "Held",
    "BodyChecks",
    "Lookup",
    "ProcMacroExpander",
    "Frontend",
    "Compile",
    "Prepare",
    "Hashes",
    "SymbolResolver",
];

/// Implementations of [`HOOK_TRAITS`] past the closure, reviewed: none changes the bytes made.
pub const REVIEWED_HOOKS: &[Hook] = &[
    Hook {
        package: "blaze-cargo",
        hook: "ImageStore",
        why: "`Images` gets and puts the dependency images hir-def writes and reads, by key: a \
              store, which makes no bytes",
    },
    Hook { package: "blaze-cargo", hook: "Held", why: "`InStore` holds the bytes of an image read from the store" },
    Hook {
        package: "blaze-cargo",
        hook: "BodyChecks",
        why: "`Checking` reports the diagnostics of the bodies the build reaches and stops it on an \
              error; it hands the code generator nothing",
    },
    Hook {
        package: "blaze-cargo",
        hook: "Lookup",
        why: "`Reading` reads templates from the store by the keys the code generator makes",
    },
    Hook {
        package: "blaze-macro-eval",
        hook: "ProcMacroExpander",
        why: "`Evaluated` expands proc macros: the tokens it gives are source of the HIR, which \
              the instance key hashes, so another expansion is another key",
    },
    Hook {
        package: "blaze-wasm-pack",
        hook: "ProcMacroExpander",
        why: "`Recorder` and `Replay` record and replay proc-macro expansions as tokens: source \
              the instance key hashes",
    },
    Hook { package: "blaze-wasm", hook: "Lookup", why: "`Stored` reads templates by key from its own store" },
];

/// The variable the build script sets to the identity's hex.
pub const ENV: &str = "BLAZE_CODEGEN_CODE";
/// Files outside every package that the code generator's packages read: the package that reads
/// it and the path relative to that package's directory.
pub const EXTRA: &[(&str, &str)] = &[(CODE_GENERATOR, "../../harness/toolchain")];
/// The variables the code generator's packages and their build scripts read, each an input by
/// its value: `CI` (stdx's `is_ci`), what llvm-sys's build script links LLVM's C++ runtime by
/// and builds its wrappers with, where cranelift-codegen's build script reads its ISLE (refused
/// when set: [`ISLE_SOURCE_DIR`]), and what libc's and zstd-sys's build scripts say they read
/// (`rerun-if-env-changed`).
pub const READ_ENV: &[&str] = &[
    "CI",
    "CXXSTDLIB",
    "LLVM_SYS_LIBCPP",
    "LLVM_SYS_SYSTEM_LIBRARY_DIRS",
    "LLVM_SYS_231_NO_CLEAN_CFLAGS",
    "LLVM_SYS_231_FFI_WORKAROUND",
    "LLVM_SYS_231_USE_DEBUG_MSVCRT",
    "ISLE_SOURCE_DIR",
    "LIBC_BUILD_VERBOSE",
    "RUST_LIBC_UNSTABLE_FREEBSD_VERSION",
    "ZSTD_SYS_NO_HIDE_SYMBOLS",
    "ZSTD_SYS_USE_PKG_CONFIG",
];
/// Where cranelift-codegen's build script reads its ISLE from, when set, in place of its own
/// files: a directory the identity does not hash, so a build with it set is refused.
pub const ISLE_SOURCE_DIR: &str = "ISLE_SOURCE_DIR";
/// What blaze, answering `rustc -vV`, ends with: its own code identity in hex
/// (blaze-cargo's `answers::own`).
pub const BLAZE_IDENTITY: &str = "blaze-identity: ";

/// The one LLVM blaze links (crates/hir-ty-codegen/llvm.json), checked as llvm-sys checks it:
/// pin.rs is its one home, of the pin, the check of a prefix and the message refusing one.
#[allow(dead_code, reason = "`pinned_llvm_config` is llvm-sys's build script's; this crate uses the check alone")]
mod pin {
    include!("../../../vendor/llvm-sys/pin.rs");
}

/// The variable naming the pinned LLVM (vendor/llvm-sys/pin.rs).
pub const LLVM_PREFIX: &str = pin::PIN_PREFIX;
/// The package that links LLVM: LLVM is an input when it is one.
const LLVM_PACKAGE: &str = "llvm-sys";

/// The pin of the stencil libraries and its reader (crates/blaze-stencil/pin.rs), as
/// blaze-stencil's build script reads it: one reader, one pin.
#[allow(dead_code, reason = "the pin's writer and hex are blaze-stencil's build script's; this crate reads the pin")]
mod stencil_pin {
    include!("../../blaze-stencil/pin.rs");
}

/// The package whose stencil libraries are keyed by their pin.
const STENCIL_PACKAGE: &str = "blaze-stencil";
/// The variable naming where the seed's prebuilt stencil libraries are: no input, since
/// blaze-stencil refuses any bytes but the pinned ones.
pub const STENCILS_DIR: &str = stencil_pin::PREBUILT;
/// The program's feature that builds it as the seed (todo/bootstrap/plan.md): no LLVM, its
/// stencils copied and held to their pin. blaze-engine's build script refuses it with any
/// feature that brings LLVM back.
pub const SEED_FEATURE: &str = "CARGO_FEATURE_SEED";

/// Why a variable a closure package's build script reads is no input, or `None` when it must
/// be one: the C compiler, its flags, archiver and SDK, which build the C of the closure's
/// packages (cc's variables, per target and per host), as the module doc says.
pub fn unkeyed_env(name: &str) -> Option<&'static str> {
    const C: &str = "the C compiler, its flags and SDK are no input (crates/blaze-codegen-code/src/lib.rs)";
    const TOOLS: [&str; 8] = ["CC", "CXX", "AR", "CFLAGS", "CXXFLAGS", "ARFLAGS", "RANLIB", "RANLIBFLAGS"];
    let base = name.strip_prefix("HOST_").or_else(|| name.strip_prefix("TARGET_")).unwrap_or(name);
    // `CC_aarch64-apple-darwin`, `CFLAGS_aarch64_apple_darwin`: a tool's variable for one target.
    let per_target = |tool: &str| {
        base.strip_prefix(tool).and_then(|it| it.strip_prefix('_')).is_some_and(|triple| triple.contains(['-', '_']))
    };
    let c = TOOLS.iter().any(|tool| base == *tool || per_target(tool))
        || matches!(
            base,
            "CRATE_CC_NO_DEFAULTS"
                | "CC_ENABLE_DEBUG_OUTPUT"
                | "CC_FORCE_DISABLE"
                | "CC_SHELL_ESCAPED_FLAGS"
                | "CC_KNOWN_WRAPPER_CUSTOM"
                | "MACOSX_DEPLOYMENT_TARGET"
                | "IPHONEOS_DEPLOYMENT_TARGET"
                | "SDKROOT"
        );
    c.then_some(C)
}

/// One input of the identity.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Input {
    /// What it is: `package`, `manifest`, `file`, `stencils`, `llvm`, `env`, `compiler`.
    pub kind: &'static str,
    /// Which: a package's `name version source`, a path relative to the workspace, a stencil
    /// library's file, a library's name, a variable's name.
    pub name: String,
    pub digest: [u8; 32],
}

/// What a file was when it was hashed: its length and modification time.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Stamp {
    pub len: u64,
    pub modified: Option<SystemTime>,
}

/// The identity, what it is of, and what a build script reruns on.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Worked {
    pub code: [u8; 32],
    /// In the order hashed: packages, manifests, files, stencils, LLVM, variables, the compiler.
    pub inputs: Vec<Input>,
    /// Every file hashed, by its absolute path: what the coverage check holds rustc's reads to.
    pub files: BTreeSet<PathBuf>,
    /// What each file hashed or parsed, and each package directory listed, was just before it
    /// was read ([`Worked::changed`]).
    pub stamps: BTreeMap<PathBuf, Stamp>,
    /// The directories of the packages of the checkout that are inputs.
    pub dirs: Vec<PathBuf>,
    /// The registry packages that are inputs (by their checksum), by name and version.
    pub registry: Vec<(String, String)>,
    /// Every package that is an input, by name.
    pub packages: BTreeSet<String>,
    /// The names of the libraries of every package that is an input, as rustc names a crate.
    pub crates: BTreeSet<String>,
    /// The variables whose value is an input and that cargo does not watch itself.
    pub rerun_env: Vec<String>,
    /// The pinned LLVM's prefix, checked to be the pin: everything under it is the pin's.
    pub llvm_prefix: Option<PathBuf>,
    /// In the seed, where its prebuilt stencil libraries are: everything blaze-stencil includes
    /// from there it checked to be the pinned bytes.
    pub stencils_dir: Option<PathBuf>,
}

impl Worked {
    /// The inputs, one per line: kind, name and digest, tab-separated.
    pub fn listing(&self) -> String {
        let mut out = String::new();
        for input in &self.inputs {
            out.push_str(input.kind);
            out.push('\t');
            out.push_str(&input.name);
            out.push('\t');
            out.push_str(&hex(&input.digest));
            out.push('\n');
        }
        out
    }

    /// The files hashed or parsed and the directories listed that are not now what they were
    /// when read (or are gone): saved while the identity was worked out, so it is of no one tree.
    pub fn changed(&self) -> Vec<PathBuf> {
        self.stamps.iter().filter(|(path, was)| stamp(path).ok() != Some(**was)).map(|(path, _)| path.clone()).collect()
    }
}

/// The hex of `bytes`, lowercase.
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// What a build script does: works the identity out for the package being built
/// (`CARGO_MANIFEST_DIR`, the program) in its workspace, prints
/// `cargo:rustc-env=BLAZE_CODEGEN_CODE=<hex>` and what to rerun on, and writes the listing of its
/// inputs to `OUT_DIR/codegen-code-inputs.txt`. An input that can't be had, or a file that
/// changed while it was hashed, is one `cargo::error` naming it, and the build stops.
pub fn emit() {
    match emitted() {
        Ok(()) => {}
        Err(why) => {
            println!("cargo::error=the code generator's code identity ({ENV}): {}", why.replace('\n', " "));
        }
    }
}

fn emitted() -> Result<(), String> {
    let manifest_dir = var("CARGO_MANIFEST_DIR")?;
    let workspace = Workspace::read(Path::new(&manifest_dir))?;
    let env: BTreeMap<String, String> = std::env::vars().collect();
    let mut build = BuildEnv::of(&env);
    build.compiler = Some(compiler(&var("RUSTC")?)?);
    let worked = work_out(&workspace, &build)?;
    let changed = worked.changed();
    if !changed.is_empty() {
        let named: Vec<String> = changed.iter().take(8).map(|it| it.display().to_string()).collect();
        return Err(format!(
            "{} file(s) changed while it was worked out, so it is of no one tree: {}; build again once \
             nothing writes to the tree",
            changed.len(),
            named.join(", ")
        ));
    }
    for dir in &worked.dirs {
        println!("cargo:rerun-if-changed={}", dir.display());
    }
    for file in &worked.files {
        if !worked.dirs.iter().any(|dir| file.starts_with(dir)) {
            println!("cargo:rerun-if-changed={}", file.display());
        }
    }
    println!("cargo:rerun-if-changed={}", workspace.lock_path.display());
    for name in &worked.rerun_env {
        println!("cargo:rerun-if-env-changed={name}");
    }
    let out = Path::new(&var("OUT_DIR")?).join("codegen-code-inputs.txt");
    std::fs::write(&out, worked.listing()).map_err(|why| format!("writing {}: {why}", out.display()))?;
    println!("cargo:rustc-env={ENV}={}", hex(&worked.code));
    Ok(())
}

fn var(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|why| format!("${name}, which cargo gives a build script: {why}"))
}

/// What `rustc -vV` says of the compiler that builds the program: its version, commit and host,
/// and for blaze as `rustc`, which blaze it is.
fn compiler(rustc: &str) -> Result<String, String> {
    let out = Command::new(rustc).arg("-vV").output().map_err(|why| format!("running `{rustc} -vV`: {why}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!("`{rustc} -vV` exited with {}: {}", out.status, stderr.trim()));
    }
    let said = String::from_utf8(out.stdout).map_err(|_| format!("`{rustc} -vV` printed no UTF-8"))?;
    check_compiler(rustc, said)
}

/// `said`, what `{rustc} -vV` printed, when it is a compiler's version: a rustc's, or blaze's
/// with one [`BLAZE_IDENTITY`] line of 64 hex digits. A program that is no rustc (blaze itself,
/// not under the name `rustc`, prints its usage and succeeds) would key the identity by text
/// that names no compiler. A blaze from before the line answers as the nightly does and can't
/// be told from it; no blaze built blaze before it.
pub fn check_compiler(rustc: &str, said: String) -> Result<String, String> {
    if !said.starts_with("rustc ") || !said.lines().any(|it| it.starts_with("release: ")) {
        let first = said.lines().next().unwrap_or("");
        return Err(format!("`{rustc} -vV` printed `{first}`, which is no rustc's version"));
    }
    let named: Vec<&str> = said.lines().filter_map(|it| it.strip_prefix(BLAZE_IDENTITY)).collect();
    match named[..] {
        [] => {}
        [hex] if hex.len() == 64 && hex.bytes().all(|it| it.is_ascii_hexdigit()) => {}
        _ => {
            return Err(format!(
                "`{rustc} -vV` names which blaze it is in {} `{BLAZE_IDENTITY}` line(s) ({}), not one of 64 hex digits",
                named.len(),
                named.join(", ")
            ));
        }
    }
    Ok(said)
}

/// What of the build script's environment is an input.
#[derive(Clone, Debug, Default)]
pub struct BuildEnv {
    /// Every variable of the build that is an input, by name.
    pub vars: BTreeMap<String, Option<String>>,
    /// The pinned LLVM's prefix, if set.
    pub llvm_prefix: Option<String>,
    /// What `$RUSTC -vV` printed; `None` where no compiler is asked (the coverage test).
    pub compiler: Option<String>,
    /// Where the seed's prebuilt stencil libraries are ([`STENCILS_DIR`]), if set.
    pub stencils_dir: Option<String>,
    /// Where cargo's own configuration is: `$CARGO_HOME`, else `$HOME/.cargo`.
    pub cargo_home: Option<PathBuf>,
}

impl BuildEnv {
    /// Of a build script's environment `env`.
    pub fn of(env: &BTreeMap<String, String>) -> BuildEnv {
        let mut vars = BTreeMap::new();
        for (name, value) in env {
            let keyed = matches!(name.as_str(), "TARGET" | "HOST" | "PROFILE" | "OPT_LEVEL" | "DEBUG")
                || name == "CARGO_ENCODED_RUSTFLAGS"
                || name.starts_with("CARGO_CFG_")
                || name.starts_with("CARGO_PROFILE_")
                || name.starts_with("CARGO_FEATURE_");
            if keyed {
                vars.insert(name.clone(), Some(value.clone()));
            }
        }
        for name in READ_ENV {
            vars.insert((*name).to_owned(), env.get(*name).cloned());
        }
        let llvm_prefix = env.get(LLVM_PREFIX).filter(|it| !it.is_empty()).cloned();
        let stencils_dir = env.get(STENCILS_DIR).filter(|it| !it.is_empty()).cloned();
        let cargo_home = match env.get("CARGO_HOME").filter(|it| !it.is_empty()) {
            Some(home) => Some(PathBuf::from(home)),
            None => env.get("HOME").filter(|it| !it.is_empty()).map(|it| Path::new(it).join(".cargo")),
        };
        BuildEnv { vars, llvm_prefix, compiler: None, stencils_dir, cargo_home }
    }

    /// Whether the program is built as the seed ([`SEED_FEATURE`]).
    pub fn seed(&self) -> bool {
        self.vars.contains_key(SEED_FEATURE)
    }
}

/// A package of the checkout: its manifest and where it is.
#[derive(Debug)]
struct Local {
    name: String,
    version: String,
    dir: PathBuf,
    manifest: toml::Table,
    /// The manifest of the workspace it inherits from (its own, or the nearest above it).
    workspace: Option<PathBuf>,
}

/// One `[[package]]` of `Cargo.lock`.
#[derive(Debug)]
struct Locked {
    name: String,
    version: String,
    source: Option<String>,
    checksum: Option<String>,
    dependencies: Vec<String>,
}

/// A workspace as its lock file and its packages' manifests say, read without cargo.
#[derive(Debug)]
pub struct Workspace {
    /// Its root directory, canonical.
    pub root: PathBuf,
    pub lock_path: PathBuf,
    /// The package being built, by name.
    pub program: String,
    lock: Vec<Locked>,
    locals: Vec<Local>,
    /// The manifests of its members.
    members: BTreeSet<PathBuf>,
    /// Workspace manifests by path, as read.
    workspaces: BTreeMap<PathBuf, toml::Table>,
    /// What the lock and every manifest read were just before they were read.
    stamps: BTreeMap<PathBuf, Stamp>,
}

fn read(path: &Path) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|why| format!("reading {}: {why}", path.display()))
}

fn read_toml(path: &Path, stamps: &mut BTreeMap<PathBuf, Stamp>) -> Result<toml::Table, String> {
    let bytes = read_stamped(path, stamps)?;
    let text = String::from_utf8(bytes).map_err(|_| format!("{} is not UTF-8", path.display()))?;
    text.parse::<toml::Table>().map_err(|why| format!("{} is no TOML: {}", path.display(), why.to_string().trim()))
}

fn canonical(path: &Path) -> Result<PathBuf, String> {
    std::fs::canonicalize(path).map_err(|why| format!("{}: {why}", path.display()))
}

/// The tables of `manifest` that list dependencies, with whether they are built into the
/// package (normal and build dependencies) or only into its tests (dev-dependencies), every
/// platform's.
fn dependency_tables(manifest: &toml::Table) -> Vec<(bool, &toml::Table)> {
    const KINDS: [(&str, bool); 5] = [
        ("dependencies", true),
        ("build-dependencies", true),
        ("build_dependencies", true),
        ("dev-dependencies", false),
        ("dev_dependencies", false),
    ];
    let targets = manifest
        .get("target")
        .and_then(toml::Value::as_table)
        .into_iter()
        .flat_map(|it| it.values())
        .filter_map(toml::Value::as_table);
    let mut out = Vec::new();
    for table in std::iter::once(manifest).chain(targets) {
        for (kind, built) in KINDS {
            if let Some(deps) = table.get(kind).and_then(toml::Value::as_table) {
                out.push((built, deps));
            }
        }
    }
    out
}

/// The workspace's `[workspace.dependencies]` entry `key` names, if any.
fn workspace_dependency<'a>(workspace: Option<&'a toml::Table>, key: &str) -> Option<&'a toml::Value> {
    workspace?.get("workspace")?.get("dependencies")?.get(key)
}

/// Whether a dependency entry inherits from the workspace (`key.workspace = true`).
fn inherits(value: &toml::Value) -> bool {
    value.get("workspace").and_then(toml::Value::as_bool) == Some(true)
}

/// The package a dependency entry `key = value` names: its `package`, the workspace entry's
/// `package` when it inherits, else `key`.
fn dependency_name(key: &str, value: &toml::Value, workspace: Option<&toml::Table>) -> String {
    if let Some(package) = value.get("package").and_then(toml::Value::as_str) {
        return package.to_owned();
    }
    if inherits(value)
        && let Some(package) = workspace_dependency(workspace, key).and_then(|it| it.get("package")).and_then(toml::Value::as_str)
    {
        return package.to_owned();
    }
    key.to_owned()
}

/// The directory a dependency entry's `path` names, relative to the manifest at `dir` (or the
/// workspace's directory when it inherits), if it names one.
fn dependency_path(dir: &Path, key: &str, value: &toml::Value, workspace: Option<(&Path, &toml::Table)>) -> Option<PathBuf> {
    if let Some(path) = value.get("path").and_then(toml::Value::as_str) {
        return Some(dir.join(path));
    }
    if inherits(value) {
        let (at, table) = workspace?;
        let path = workspace_dependency(Some(table), key)?.get("path")?.as_str()?;
        return Some(at.join(path));
    }
    None
}

impl Workspace {
    /// The workspace of the package at `program_dir`: the nearest manifest at or above it with
    /// a `[workspace]` table, its `Cargo.lock`, its members and every package of the checkout a
    /// manifest names by `path`. Nothing is run and nothing is written.
    pub fn read(program_dir: &Path) -> Result<Workspace, String> {
        let program_dir = canonical(program_dir)?;
        let mut stamps = BTreeMap::new();
        let program_manifest = read_toml(&program_dir.join("Cargo.toml"), &mut stamps)?;
        let program = program_manifest
            .get("package")
            .and_then(|it| it.get("name"))
            .and_then(toml::Value::as_str)
            .ok_or_else(|| format!("{} names no package", program_dir.join("Cargo.toml").display()))?
            .to_owned();
        let mut workspaces = BTreeMap::new();
        let root_manifest = workspace_of(&program_dir, &mut workspaces, &mut stamps)?
            .ok_or_else(|| format!("no manifest at or above {} has a `[workspace]` table", program_dir.display()))?;
        let root = root_manifest.parent().map(Path::to_path_buf).ok_or("a manifest has no directory")?;
        let lock_path = root.join("Cargo.lock");
        let lock = locked(&lock_path, &mut stamps)?;

        // The members: `members` globs, less `exclude`.
        let table = workspaces[&root_manifest].clone();
        let section = table.get("workspace").and_then(toml::Value::as_table).cloned().unwrap_or_default();
        let excluded: Vec<PathBuf> = section
            .get("exclude")
            .and_then(toml::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(toml::Value::as_str)
            .map(|it| root.join(it))
            .collect();
        let mut members = BTreeSet::new();
        let mut queue: Vec<PathBuf> = vec![program_dir.clone()];
        if table.contains_key("package") {
            queue.push(root.clone());
            members.insert(root_manifest.clone());
        }
        for pattern in section.get("members").and_then(toml::Value::as_array).into_iter().flatten().filter_map(toml::Value::as_str) {
            let full = root.join(pattern);
            let full = full.to_str().ok_or_else(|| format!("{} is not UTF-8", full.display()))?;
            let paths = glob::glob(full).map_err(|why| format!("member pattern `{pattern}` of {}: {why}", root_manifest.display()))?;
            for path in paths {
                let path = path.map_err(|why| format!("member pattern `{pattern}`: {why}"))?;
                if !path.join("Cargo.toml").is_file() || excluded.iter().any(|it| path.starts_with(it)) {
                    continue;
                }
                let path = canonical(&path)?;
                members.insert(path.join("Cargo.toml"));
                queue.push(path);
            }
        }
        // `path` entries of the root's workspace dependencies, patches and replacements.
        let mut tables: Vec<&toml::Table> = Vec::new();
        if let Some(deps) = section.get("dependencies").and_then(toml::Value::as_table) {
            tables.push(deps);
        }
        for patches in table.get("patch").and_then(toml::Value::as_table).into_iter().flat_map(|it| it.values()) {
            if let Some(patches) = patches.as_table() {
                tables.push(patches);
            }
        }
        if let Some(replace) = table.get("replace").and_then(toml::Value::as_table) {
            tables.push(replace);
        }
        for deps in tables {
            for value in deps.values() {
                if let Some(path) = value.get("path").and_then(toml::Value::as_str) {
                    queue.push(root.join(path));
                }
            }
        }

        let mut locals: Vec<Local> = Vec::new();
        let mut seen = BTreeSet::new();
        while let Some(dir) = queue.pop() {
            let dir = canonical(&dir)?;
            if !seen.insert(dir.clone()) {
                continue;
            }
            let manifest_path = dir.join("Cargo.toml");
            let manifest = read_toml(&manifest_path, &mut stamps)?;
            let workspace = workspace_of(&dir, &mut workspaces, &mut stamps)?;
            let inherited = workspace.as_ref().map(|path| (path.parent().unwrap_or(Path::new("")), &workspaces[path]));
            for (_, deps) in dependency_tables(&manifest) {
                for (key, value) in deps {
                    if let Some(path) = dependency_path(&dir, key, value, inherited) {
                        queue.push(path);
                    }
                }
            }
            let Some(package) = manifest.get("package").and_then(toml::Value::as_table) else { continue };
            let name = package
                .get("name")
                .and_then(toml::Value::as_str)
                .ok_or_else(|| format!("{} names no package", manifest_path.display()))?
                .to_owned();
            let version = match package.get("version") {
                None => "0.0.0".to_owned(),
                Some(version) if inherits(version) => inherited
                    .and_then(|(_, table)| table.get("workspace")?.get("package")?.get("version")?.as_str())
                    .ok_or_else(|| {
                        format!("{} inherits its version from a workspace that sets none", manifest_path.display())
                    })?
                    .to_owned(),
                Some(version) => version
                    .as_str()
                    .ok_or_else(|| format!("the version in {} is no string", manifest_path.display()))?
                    .to_owned(),
            };
            locals.push(Local { name, version, dir, manifest, workspace });
        }
        locals.sort_by(|a, b| a.dir.cmp(&b.dir));
        Ok(Workspace { root, lock_path, program, lock, locals, members, workspaces, stamps })
    }

    /// The package of the checkout the lock's path package `locked` is.
    fn local(&self, locked: &Locked) -> Result<&Local, String> {
        let found: Vec<&Local> =
            self.locals.iter().filter(|it| it.name == locked.name && it.version == locked.version).collect();
        match found[..] {
            [one] => Ok(one),
            [] => Err(format!(
                "{} {} is a package of the checkout in {}, and no manifest a member or `path` names is it",
                locked.name,
                locked.version,
                self.lock_path.display()
            )),
            _ => Err(format!(
                "{} {} is the package of {} manifests: {}",
                locked.name,
                locked.version,
                found.len(),
                found.iter().map(|it| it.dir.display().to_string()).collect::<Vec<_>>().join(", ")
            )),
        }
    }

    /// The lock's package a `dependencies` entry of the lock names: `name`, `name version` or
    /// `name version (source)`.
    fn resolve(&self, of: &Locked, entry: &str) -> Result<usize, String> {
        let mut parts = entry.split_whitespace();
        let name = parts.next().unwrap_or("");
        let version = parts.next();
        let source = parts.next().and_then(|it| it.strip_prefix('(')).and_then(|it| it.strip_suffix(')'));
        let found: Vec<usize> = (0..self.lock.len())
            .filter(|at| {
                let it = &self.lock[*at];
                it.name == name
                    && version.is_none_or(|version| it.version == version)
                    && source.is_none_or(|source| it.source.as_deref() == Some(source))
            })
            .collect();
        match found[..] {
            [one] => Ok(one),
            _ => Err(format!(
                "`{entry}`, a dependency of {} {} in {}, names {} packages of the lock, not one",
                of.name,
                of.version,
                self.lock_path.display(),
                found.len()
            )),
        }
    }

    /// The packages `at` is built from by the lock: every dependency the lock lists, but those a
    /// package of the checkout names only as a dev-dependency.
    fn built_from(&self, at: usize) -> Result<Vec<usize>, String> {
        let locked = &self.lock[at];
        let built: Option<BTreeSet<String>> = match locked.source {
            Some(_) => None,
            None => {
                let local = self.local(locked)?;
                let workspace = local.workspace.as_ref().map(|it| &self.workspaces[it]);
                let mut names = BTreeSet::new();
                for (built, deps) in dependency_tables(&local.manifest) {
                    if built {
                        names.extend(deps.iter().map(|(key, value)| dependency_name(key, value, workspace)));
                    }
                }
                Some(names)
            }
        };
        let mut out = Vec::new();
        for entry in &locked.dependencies {
            let dep = self.resolve(locked, entry)?;
            if built.as_ref().is_none_or(|names| names.contains(&self.lock[dep].name)) {
                out.push(dep);
            }
        }
        Ok(out)
    }

    /// Every package reachable from `from` by [`Workspace::built_from`], `from` too.
    fn reach(&self, from: &[usize]) -> Result<BTreeSet<usize>, String> {
        let mut seen = BTreeSet::new();
        let mut stack = from.to_vec();
        while let Some(at) = stack.pop() {
            if seen.insert(at) {
                stack.extend(self.built_from(at)?);
            }
        }
        Ok(seen)
    }

    /// The packages of the code generator's closure, by name.
    pub fn closure_names(&self) -> Result<BTreeSet<String>, String> {
        Ok(self.closure()?.into_iter().map(|at| self.lock[at].name.clone()).collect())
    }

    /// The packages of the checkout, by name, with their directories.
    pub fn checkout(&self) -> Vec<(&str, &Path)> {
        self.locals.iter().map(|it| (it.name.as_str(), it.dir.as_path())).collect()
    }

    /// Every package of the lock the [`ROOTS`] the program links are built from.
    fn closure(&self) -> Result<BTreeSet<usize>, String> {
        let program = (0..self.lock.len())
            .find(|at| self.lock[*at].source.is_none() && self.lock[*at].name == self.program)
            .ok_or_else(|| format!("the program `{}` is no package of {}", self.program, self.lock_path.display()))?;
        let linked = self.reach(&[program])?;
        let mut roots = Vec::new();
        for root in ROOTS {
            // `blaze-hir-ty-codegen` is the name .github/scripts/publish_crates.py publishes it under.
            let published = format!("blaze-{}", root.package);
            let found: Vec<usize> =
                linked.iter().copied().filter(|at| self.lock[*at].name == root.package || self.lock[*at].name == published).collect();
            match found[..] {
                [] if root.package == CODE_GENERATOR => {
                    return Err(format!("the program `{}` does not link the code generator `{CODE_GENERATOR}`", self.program));
                }
                [] => {}
                [one] => roots.push(one),
                _ => return Err(format!("the program `{}` links {} packages named `{}`", self.program, found.len(), root.package)),
            }
        }
        self.reach(&roots)
    }
}

/// The manifest of the workspace `dir` is in: the nearest at or above it with a `[workspace]`
/// table, read into `cache`.
fn workspace_of(
    dir: &Path,
    cache: &mut BTreeMap<PathBuf, toml::Table>,
    stamps: &mut BTreeMap<PathBuf, Stamp>,
) -> Result<Option<PathBuf>, String> {
    for ancestor in dir.ancestors() {
        let manifest = ancestor.join("Cargo.toml");
        if cache.get(&manifest).is_some_and(|it| it.contains_key("workspace")) {
            return Ok(Some(manifest));
        }
        if !manifest.is_file() {
            continue;
        }
        let table = read_toml(&manifest, stamps)?;
        let is = table.contains_key("workspace");
        cache.insert(manifest.clone(), table);
        if is {
            return Ok(Some(manifest));
        }
    }
    Ok(None)
}

/// The packages of `Cargo.lock`.
fn locked(path: &Path, stamps: &mut BTreeMap<PathBuf, Stamp>) -> Result<Vec<Locked>, String> {
    let table = read_toml(path, stamps)?;
    let mut out = Vec::new();
    for package in table.get("package").and_then(toml::Value::as_array).ok_or_else(|| format!("{} lists no `[[package]]`", path.display()))? {
        let field = |name: &str| package.get(name).and_then(toml::Value::as_str).map(str::to_owned);
        let name = field("name").ok_or_else(|| format!("a package of {} has no name", path.display()))?;
        let version = field("version").ok_or_else(|| format!("{name} in {} has no version", path.display()))?;
        let dependencies = package
            .get("dependencies")
            .and_then(toml::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(toml::Value::as_str)
            .map(str::to_owned)
            .collect();
        out.push(Locked { name, version, source: field("source"), checksum: field("checksum"), dependencies });
    }
    Ok(out)
}

fn stamp(path: &Path) -> Result<Stamp, String> {
    let meta = std::fs::metadata(path).map_err(|why| format!("{}: {why}", path.display()))?;
    Ok(Stamp { len: meta.len(), modified: meta.modified().ok() })
}

/// Reads `path`, noting what it was just before ([`Worked::changed`]). A file read twice (a
/// manifest, parsed for the closure and later hashed) keeps its first stamp, so a save between
/// the two reads is seen.
fn read_stamped(path: &Path, stamps: &mut BTreeMap<PathBuf, Stamp>) -> Result<Vec<u8>, String> {
    let was = stamp(path)?;
    let bytes = read(path)?;
    stamps.entry(path.to_path_buf()).or_insert(was);
    Ok(bytes)
}

/// The identity of the code generator `workspace`'s program runs, built in `env`.
pub fn work_out(workspace: &Workspace, env: &BuildEnv) -> Result<Worked, String> {
    let root = workspace.root.as_path();
    let closure = workspace.closure()?;

    let mut inputs = Vec::new();
    let mut files = BTreeSet::new();
    let mut stamps = workspace.stamps.clone();
    let mut dirs = Vec::new();
    let mut registry = Vec::new();
    let mut packages = BTreeSet::new();
    let mut crates = BTreeSet::new();
    let mut inherited = BTreeSet::new();
    let mut links_llvm = false;

    // 1. The packages, by name, version and source.
    let mut chosen: Vec<&Locked> = closure.iter().map(|at| &workspace.lock[*at]).collect();
    chosen.sort_by(|a, b| (&a.name, &a.version, &a.source).cmp(&(&b.name, &b.version, &b.source)));
    for package in chosen {
        links_llvm |= package.name == LLVM_PACKAGE;
        packages.insert(package.name.clone());
        let name = format!("{} {} {}", package.name, package.version, package.source.as_deref().unwrap_or("checkout"));
        let digest = match package.source.as_deref() {
            Some(source) if source.starts_with("registry+") || source.starts_with("sparse+") => {
                let checksum = package.checksum.as_deref().ok_or_else(|| {
                    format!(
                        "{name} has no checksum in {}: a registry package is an input by its checksum",
                        workspace.lock_path.display()
                    )
                })?;
                registry.push((package.name.clone(), package.version.clone()));
                crates.insert(package.name.replace('-', "_"));
                digest_of("package registry", &[checksum.as_bytes()])
            }
            Some(source) if source.starts_with("git+") => {
                return Err(format!(
                    "{} {} comes from git ({source}), and the code identity keys a package by its lock \
                     checksum or, in the checkout, by its files: a git package has neither (vendor it \
                     under the checkout, or key git sources by their commit in blaze-codegen-code)",
                    package.name, package.version
                ));
            }
            Some(source) => return Err(format!("{name}: the code identity knows no source `{source}`")),
            None => {
                let local = workspace.local(package)?;
                crates.insert(lib_name(local));
                let mut hasher = hasher("package checkout");
                for (relative, path) in package_files(local, &mut stamps)? {
                    let bytes = read_stamped(&path, &mut stamps)?;
                    hasher.update(&(relative.len() as u64).to_le_bytes()).update(relative.as_bytes());
                    hasher.update(&(bytes.len() as u64).to_le_bytes()).update(&bytes);
                    files.insert(path);
                }
                dirs.push(local.dir.clone());
                if let Some(manifest) = &local.workspace
                    && manifest.parent() != Some(root)
                {
                    inherited.insert(manifest.clone());
                }
                *hasher.finalize().as_bytes()
            }
        };
        inputs.push(Input { kind: "package", name, digest });
    }

    // 2. The manifests of the workspace and of the workspaces packages inherit from.
    let mut manifests = BTreeSet::new();
    manifests.insert(root.join("Cargo.toml"));
    manifests.extend(workspace.members.iter().cloned());
    manifests.extend(inherited);
    // Cargo's configuration: the root's and every directory's above it, as cargo run in the
    // workspace reads them, and `$CARGO_HOME`'s.
    for dir in root.ancestors() {
        for config in [".cargo/config.toml", ".cargo/config"] {
            let path = dir.join(config);
            if path.is_file() {
                manifests.insert(path);
            }
        }
    }
    let mut configs = Vec::new();
    for config in ["config.toml", "config"] {
        if let Some(path) = env.cargo_home.as_ref().map(|it| it.join(config))
            && path.is_file()
            && !manifests.contains(&path)
        {
            configs.push((format!("$CARGO_HOME/{config}"), path));
        }
    }
    let manifests = manifests.into_iter().map(|path| (relative_name(&path, root), path));
    for (name, path) in manifests.chain(configs) {
        let bytes = read_stamped(&path, &mut stamps)?;
        inputs.push(Input { kind: "manifest", name, digest: digest_of("manifest", &[&bytes]) });
        files.insert(path);
    }

    // 3. Files outside the packages, and the variables they read.
    for (reader, extra) in EXTRA {
        let local = closure
            .iter()
            .map(|at| &workspace.lock[*at])
            .find(|it| it.source.is_none() && it.name == *reader)
            .map(|it| workspace.local(it))
            .transpose()?
            .ok_or_else(|| format!("`{reader}`, which reads {extra}, is no package of the checkout in the closure"))?;
        let path = local.dir.join(extra);
        let path = canonical(&path).map_err(|why| format!("{extra}, which `{reader}` reads: {why}"))?;
        let bytes = read_stamped(&path, &mut stamps)?;
        inputs.push(Input { kind: "file", name: relative_name(&path, root), digest: digest_of("file", &[&bytes]) });
        files.insert(path);
    }

    // 4. The stencil libraries, by their pin.
    let stencils = closure.iter().map(|at| &workspace.lock[*at]).find(|it| it.source.is_none() && it.name == STENCIL_PACKAGE);
    if let Some(stencils) = stencils {
        let path = workspace.local(stencils)?.dir.join(stencil_pin::PIN_FILE);
        let text = String::from_utf8(read_stamped(&path, &mut stamps)?).map_err(|_| format!("{} is not UTF-8", path.display()))?;
        let pinned = stencil_pin::pin_parse(&text).map_err(|why| format!("the stencil libraries' pin {}: {why}", path.display()))?;
        for (name, id) in pinned {
            inputs.push(Input { kind: "stencils", name, digest: id });
        }
        files.insert(path);
    }
    let seed = env.seed();
    let stencils_dir = if seed { env.stencils_dir.as_ref().map(PathBuf::from) } else { None };

    // 5. LLVM: the pin, and the prefix checked to be it; none in the seed, which links no LLVM
    // and makes no stencils with it.
    let mut rerun_env: Vec<String> = READ_ENV.iter().map(|it| (*it).to_owned()).collect();
    let mut llvm_prefix = None;
    if links_llvm && !seed {
        rerun_env.push(LLVM_PREFIX.to_owned());
        let (pinned, json) = llvm_pin(workspace, &closure, &mut stamps, &mut files)?;
        let mut built_for = BTreeSet::new();
        for name in ["TARGET", "HOST"] {
            let tuple = env.vars.get(name).cloned().flatten().ok_or_else(|| {
                format!("${name}, which cargo gives a build script, names whose release of the pinned LLVM is linked")
            })?;
            built_for.insert(tuple);
        }
        for tuple in built_for {
            let sha = pin::pin_sha256(&json, &tuple).map_err(|why| format!("blaze's LLVM pin: {why}"))?;
            let released = sha.as_deref().map_or(&b"none"[..], str::as_bytes);
            let digest = digest_of("llvm pin", &[pinned.version.as_bytes(), pinned.commit.as_bytes(), tuple.as_bytes(), released]);
            inputs.push(Input { kind: "llvm", name: format!("pin {tuple}"), digest });
        }
        if let Some(prefix) = env.llvm_prefix.as_deref() {
            pin::pin_check(&pinned, Some(std::ffi::OsStr::new(prefix)), std::env::consts::EXE_SUFFIX)
                .map_err(|found| pinned.refusal(&found))?;
            let prefix = canonical(Path::new(prefix)).map_err(|why| format!("${LLVM_PREFIX}: {why}"))?;
            let marker = prefix.join(pin::PIN_MARKER);
            let bytes = read_stamped(&marker, &mut stamps)?;
            inputs.push(Input { kind: "llvm", name: pin::PIN_MARKER.to_owned(), digest: digest_of("llvm marker", &[&bytes]) });
            files.insert(marker);
            llvm_prefix = Some(prefix);
        }
    }

    // 6. How blaze is built.
    if let Some(Some(dir)) = env.vars.get(ISLE_SOURCE_DIR) {
        return Err(format!(
            "${ISLE_SOURCE_DIR} is `{dir}`: cranelift-codegen's build script then makes its code \
             from the ISLE files there, which the code identity does not hash; unset it"
        ));
    }
    for (name, value) in &env.vars {
        let digest = match value {
            Some(value) => digest_of("env set", &[value.as_bytes()]),
            None => digest_of("env unset", &[]),
        };
        inputs.push(Input { kind: "env", name: name.clone(), digest });
    }
    for name in env.vars.keys().filter(|it| it.starts_with("CARGO_PROFILE_")) {
        rerun_env.push(name.clone());
    }
    if let Some(compiler) = &env.compiler {
        inputs.push(Input { kind: "compiler", name: "rustc -vV".to_owned(), digest: digest_of("compiler", &[compiler.as_bytes()]) });
    }

    let mut hasher = hasher_of(DOMAIN);
    hasher.update(&(inputs.len() as u64).to_le_bytes());
    for input in &inputs {
        hasher.update(&(input.kind.len() as u64).to_le_bytes()).update(input.kind.as_bytes());
        hasher.update(&(input.name.len() as u64).to_le_bytes()).update(input.name.as_bytes());
        hasher.update(&input.digest);
    }
    Ok(Worked {
        code: *hasher.finalize().as_bytes(),
        inputs,
        files,
        stamps,
        dirs,
        registry,
        packages,
        crates,
        rerun_env,
        llvm_prefix,
        stencils_dir,
    })
}

/// The pin llvm-sys checks LLVM against, as pin.rs reads it from llvm-sys's directory, and
/// llvm.json's text.
fn llvm_pin(
    workspace: &Workspace,
    closure: &BTreeSet<usize>,
    stamps: &mut BTreeMap<PathBuf, Stamp>,
    files: &mut BTreeSet<PathBuf>,
) -> Result<(pin::Pin, String), String> {
    let sys = closure
        .iter()
        .map(|at| &workspace.lock[*at])
        .find(|it| it.name == LLVM_PACKAGE)
        .ok_or_else(|| format!("{LLVM_PACKAGE} is no package of the closure"))?;
    if let Some(source) = &sys.source {
        return Err(format!(
            "{LLVM_PACKAGE} {} is from {source}: blaze links only its own (vendor/llvm-sys), which checks the pin",
            sys.version
        ));
    }
    let path = workspace.local(sys)?.dir.join(pin::PIN_JSON);
    let path = canonical(&path).map_err(|why| format!("blaze's LLVM pin: {why}"))?;
    let json = String::from_utf8(read_stamped(&path, stamps)?).map_err(|_| format!("{} is not UTF-8", path.display()))?;
    files.insert(path.clone());
    let pinned = pin::Pin::parse(&json).map_err(|why| format!("blaze's LLVM pin `{}` can't be read: {why}", path.display()))?;
    Ok((pinned, json))
}

/// The name rustc gives the library of `local`: its `[lib] name`, else its package name.
fn lib_name(local: &Local) -> String {
    local
        .manifest
        .get("lib")
        .and_then(|it| it.get("name"))
        .and_then(toml::Value::as_str)
        .unwrap_or(&local.name)
        .replace('-', "_")
}

/// The directories of `local`'s library, binaries and build script, where its manifest names
/// them; the default ones (`src/`, `build.rs`) are never below `tests`, `benches` or `examples`.
fn built_dirs(local: &Local) -> Vec<PathBuf> {
    let mut paths: Vec<&str> = Vec::new();
    if let Some(path) = local.manifest.get("lib").and_then(|it| it.get("path")).and_then(toml::Value::as_str) {
        paths.push(path);
    }
    for bin in local.manifest.get("bin").and_then(toml::Value::as_array).into_iter().flatten() {
        if let Some(path) = bin.get("path").and_then(toml::Value::as_str) {
            paths.push(path);
        }
    }
    if let Some(path) = local.manifest.get("package").and_then(|it| it.get("build")).and_then(toml::Value::as_str) {
        paths.push(path);
    }
    paths.into_iter().filter_map(|it| local.dir.join(it).parent().map(Path::to_path_buf)).collect()
}

fn hasher_of(domain: &str) -> blake3::Hasher {
    blake3::Hasher::new_derive_key(domain)
}

fn hasher(kind: &str) -> blake3::Hasher {
    let mut hasher = hasher_of(INPUT_DOMAIN);
    hasher.update(&(kind.len() as u64).to_le_bytes()).update(kind.as_bytes());
    hasher
}

fn digest_of(kind: &str, parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = hasher(kind);
    for part in parts {
        hasher.update(&(part.len() as u64).to_le_bytes()).update(part);
    }
    *hasher.finalize().as_bytes()
}

/// `path` relative to `root`, with `/`, or as it is outside it.
fn relative_name(path: &Path, root: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(relative) => slashed(relative),
        // Above the workspace (an inherited root): by its depth above it, never its absolute
        // path, so the checkout's place is no input.
        Err(_) => {
            let common = root.ancestors().find(|it| path.starts_with(it)).unwrap_or(Path::new(""));
            let up = root.strip_prefix(common).map_or(0, |it| it.components().count());
            let rest = path.strip_prefix(common).map(slashed).unwrap_or_default();
            format!("{}{rest}", "../".repeat(up))
        }
    }
}

fn slashed(path: &Path) -> String {
    path.components().map(|it| it.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/")
}

/// Every file of a package of the checkout, sorted by its path relative to the package's
/// directory, with `/`.
/// Each directory listed is stamped, so a file added or removed while it works is seen.
fn package_files(local: &Local, stamps: &mut BTreeMap<PathBuf, Stamp>) -> Result<Vec<(String, PathBuf)>, String> {
    let built = built_dirs(local);
    let mut out = Vec::new();
    let mut stack = vec![local.dir.clone()];
    while let Some(dir) = stack.pop() {
        stamps.entry(dir.clone()).or_insert(stamp(&dir)?);
        let entries = std::fs::read_dir(&dir).map_err(|why| format!("listing {}: {why}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|why| format!("listing {}: {why}", dir.display()))?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            let kind = entry.file_type().map_err(|why| format!("{}: {why}", path.display()))?;
            let at_root = dir == local.dir;
            if kind.is_symlink() {
                let meta = std::fs::metadata(&path).map_err(|why| format!("{}: {why}", path.display()))?;
                if meta.is_dir() {
                    return Err(format!(
                        "{} of package {} is a link to a directory: its files are not hashed",
                        path.display(),
                        local.name
                    ));
                }
            } else if kind.is_dir() {
                let skipped = at_root
                    && (name == "target"
                        || (matches!(&*name, "tests" | "benches" | "examples") && !built.iter().any(|it| it.starts_with(&path))));
                if !skipped {
                    stack.push(path);
                }
                continue;
            }
            let relative = path.strip_prefix(&local.dir).map_err(|_| format!("{} is outside its package", path.display()))?;
            out.push((slashed(relative), path));
        }
    }
    out.sort();
    Ok(out)
}
