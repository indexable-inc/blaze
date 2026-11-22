//! proc_macro_modules: the bootstrap builder of proc-macro modules (decision D52, the named
//! interim exception to rule 1 in docs/v2/plan/design/bottom.md). `DOC` below is its usage.
//!
//! The `blaze` binary runs a proc macro as a `wasm32-wasip1` module in Wasmtime
//! (crates/blaze-proc-macro, rule 10), but cannot compile syn yet. Until it can, this tool, run by
//! a person or CI and never by `blaze`, builds each proc-macro crate a checkout's lock names,
//! unchanged, with the pinned nightly's rustc and `-Zwasm-proc-macros`, and writes the module
//! records `blaze` reads. A call whose module is absent is a finding in `blaze`, never a build.
//!
//! How a crate is built without a change to it:
//!
//! - **The target.** `crates/blaze-proc-macro/guest/wasm32-wasip1-macro.json` is
//!   `wasm32-wasip1` with `only-cdylib: false`, the one thing that keeps rustc from dropping the
//!   `proc-macro` crate type for wasm; std, `panic_abort` and `proc_macro` are built for it
//!   (`-Zbuild-std`).
//! - **The crate type.** cargo builds a proc-macro crate for the host, whatever `--target`
//!   says. So each crate gets a workspace of its own whose member is a manifest made from the
//!   crate's own (`macro/Cargo.toml`): its `[lib]` points at the crate's unchanged source in the
//!   vendored copy, with `proc-macro` taken out, so cargo builds it for the wasm target with its
//!   dependencies. This binary is cargo's `RUSTC_WRAPPER`: for that one crate it puts the
//!   `proc-macro` crate type back with `-Zwasm-proc-macros`, links blaze's shim
//!   (`guest/shim.rs`, the member `shim`) into the module with its exports named, and asks
//!   rustc once more for the expanded crate (`-Zunpretty=expanded`), whose harness static lists
//!   the macros in the order the module's `__rustc_proc_macro_decls_*` global holds them.
//! - **Two steps.** Fetch: `cargo vendor` of the crates and of std's own dependencies into
//!   `<work>/vendor` (the network; no code of theirs runs). Build: `cargo build --offline` from
//!   the vendored sources inside a sandbox with no network that writes only under `<work>`
//!   (macOS: Seatbelt through `sandbox-exec`, the kernel mechanism blaze-cargo's sandbox uses;
//!   Linux: `bwrap`). Build scripts run there. With no sandbox on the host the tool stops, unless
//!   the input says `"unsandboxed_build_scripts": true`, which it then prints.
//! - **Deterministic bytes (rule 25).** Every machine path rustc could embed is remapped:
//!   `<work>` (the vendored sources, the workspaces, the target folder) to `/build`, the
//!   toolchain's library sources to `/rustc/src`. Debug info is off.
//!
//! The record (rules 21 and 65): `<store>/proc-macro/modules/<key>/`, where `<key>` is the
//! SHA-256 of `identity`, the crate's identity as text: name, version, what pins its sources
//! (a registry crate's lock checksum; a git package's commit and folder; a workspace or path
//! package's [`files_digest`], so an edit to it makes a new module), the dependencies the lock
//! pins under it ([`lock_closure`]), the features, the toolchain, the target JSON's digest, the
//! shim's digest, the builder.
//! It holds `identity`, `module.wasm`, `wasm-sha256`, `macros` (one line per macro: its index
//! in the module, its kind, its name, a derive's helper attributes) and `decls` (the name of
//! the module's global that holds the list). A record already there with other bytes is a
//! refusal naming the key: the store is write-once, and two values under one key is a key
//! that misses an input.

const SCRIPT: &str = "proc_macro_modules";

/// The packages of the standard library's own workspace (the pinned toolchain's
/// `library/Cargo.lock`) that `-Zbuild-std` builds for a module, each given
/// `[profile.release.package.<name>] debug-assertions = false` in a crate's workspace: std is
/// a release build, while the macro crate and its dependencies keep debug assertions (identity
/// form 4). Only names no registry crate a macro depends on can have: std's third-party
/// dependencies (hashbrown, cfg-if, libc, memchr, ...) are left out, since an override by name
/// would reach the macro's copies of them too. A name the build does not reach is one cargo
/// warns about and ignores.
const STD_RELEASE: [&str; 12] = [
    "core",
    "alloc",
    "std",
    "proc_macro",
    "panic_abort",
    "panic_unwind",
    "compiler_builtins",
    "std_detect",
    "unwind",
    "rustc-std-workspace-core",
    "rustc-std-workspace-alloc",
    "rustc-std-workspace-std",
];

/// The manifest of the shim's package, `shim/` beside each member.
const SHIM_MANIFEST: &str = "[package]\nname = \"blaze_pm_shim\"\nversion = \"0.0.0\"\nedition = \"2024\"\npublish = false\n\n[lib]\npath = \"shim.rs\"\n";

const DOC: &str = r#"proc_macro_modules: build proc-macro crates to wasm32-wasip1 module records (D52).

    proc_macro_modules '<json>'

    checkout   the project whose Cargo.lock pins the crates and their dependencies (read only), required
    store      the store folder the records go to, required
    work       a folder the run owns: the vendored sources, workspaces and target, required
    tree       blaze's checkout: the shim, the target JSON, pins/toolchain (".")
    crates     [{"name", "version", "features"}]: each proc-macro crate to build; "features" is the
               list the host side of the project's graph unifies to. Absent, the crate's own
               default features are used and the output says "assumed default", required
    fetch_store   blaze-fetch's store folder (records/<key>): a git source's commit is read from its
               verified pack; required only for crates the lock pins to git
    vendor_from   blaze-fetch's store folder (records/<key>): the fetch step writes <work>/vendor from
               its verified records (the crates' lock closures and std's own lock), with no
               `cargo vendor` and no network; a package it does not hold is refused by name.
               Absent: `cargo vendor` (the network)
    unsandboxed_build_scripts   run the build with no sandbox where none can be made (false)
    build_seconds   the deadline of each crate's build (900)

Prints one line per crate: its key, the module's SHA-256 and size, its macros, or why it failed.
Exits 1 when any crate failed."#;

use std::{
    ffi::{OsStr, OsString},
    fmt,
    io::Read as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use blaze_script::input::{self, Fields, Input, InputError};

// The build-script mode: a folder of its own, so cargo does not take it for a binary.
#[path = "proc_macro_modules/build_scripts.rs"]
mod build_scripts;
// `vendor_from`: the fetch step from blaze's fetch store, no network (D112 N2).
#[path = "proc_macro_modules/store_vendor.rs"]
mod store_vendor;
// A script workspace's `[features]` lines keep their build-dependency enables (D112 N6).
#[path = "proc_macro_modules/script_features.rs"]
mod script_features;
#[path = "proc_macro_modules/member_dependencies.rs"]
mod member_dependencies;

/// What a run is given.
struct Args {
    checkout: PathBuf,
    store: PathBuf,
    work: PathBuf,
    tree: PathBuf,
    crates: Vec<Wanted>,
    /// blaze-fetch's store (`records/<key>`), where a git source's commit is read from.
    fetch_store: Option<PathBuf>,
    /// blaze-fetch's store the fetch step vendors from instead of `cargo vendor` (D112 N2,
    /// [`Plan::vendor_from`]): no network.
    vendor_from: Option<PathBuf>,
    unsandboxed_build_scripts: bool,
    build_seconds: u64,
}

/// One crate asked for.
struct Wanted {
    name: String,
    version: String,
    /// `None`: not given, the crate's defaults assumed.
    features: Option<Vec<String>>,
}

impl Input for Wanted {
    const FIELDS: &'static [&'static str] = &["name", "version", "features"];

    fn parse(f: &Fields<'_>) -> Result<Wanted, InputError> {
        let features = match f.get("features") {
            None => None,
            Some(_) => Some(f.strs("features")?),
        };
        Ok(Wanted { name: f.str("name", None)?, version: f.str("version", None)?, features })
    }
}

impl Input for Args {
    const FIELDS: &'static [&'static str] =
        &["checkout", "store", "work", "tree", "crates", "fetch_store", "vendor_from", "unsandboxed_build_scripts", "build_seconds"];

    fn parse(f: &Fields<'_>) -> Result<Args, InputError> {
        let build_seconds = f.int("build_seconds", Some(900))?;
        Ok(Args {
            checkout: PathBuf::from(f.str("checkout", None)?),
            store: PathBuf::from(f.str("store", None)?),
            work: PathBuf::from(f.str("work", None)?),
            tree: PathBuf::from(f.str("tree", Some("."))?),
            crates: f.objects("crates", true)?,
            fetch_store: match f.get("fetch_store") {
                None => None,
                Some(_) => Some(PathBuf::from(f.str("fetch_store", None)?)),
            },
            vendor_from: match f.get("vendor_from") {
                None => None,
                Some(_) => Some(PathBuf::from(f.str("vendor_from", None)?)),
            },
            unsandboxed_build_scripts: f.bool("unsandboxed_build_scripts", Some(false))?,
            build_seconds: u64::try_from(build_seconds)
                .map_err(|_| InputError(format!("build_seconds is {build_seconds}, not a count of seconds")))?,
        })
    }
}

/// Why a step failed, naming what it was about.
enum Failure {
    /// A file or folder could not be read or written.
    Io { path: PathBuf, error: std::io::Error },
    /// A program could not be started.
    Start { program: String, error: std::io::Error },
    /// A program ran out of its deadline and was killed.
    Deadline { what: String, seconds: u64 },
    /// A program failed: its exit status and the tail of what it printed.
    Exited { what: String, status: i32, tail: String },
    /// The checkout's lock has no such package, or it has no checksum.
    NotLocked { name: String, version: String },
    /// No sandbox can be made on this host.
    NoSandbox { why: String },
    /// The build left no module where the wrapper said it would be.
    NoModule { name: String, version: String, why: String },
    /// The expanded crate does not list its macros as the harness writes them.
    Harness { name: String, why: String },
    /// A record under this key already holds other bytes.
    Conflict { key: String, held: String, made: String },
    /// A manifest is not one this tool can rewrite.
    Manifest { path: PathBuf, why: String },
    /// With `vendor_from`: the packages the build needs that the fetch store cannot give, each
    /// named; nothing is fetched from the network instead.
    Unvendored { store: PathBuf, missing: Vec<store_vendor::Missing> },
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failure::Io { path, error } => write!(f, "{}: {error}", path.display()),
            Failure::Start { program, error } => write!(f, "{program} did not start: {error}"),
            Failure::Deadline { what, seconds } => write!(f, "{what} ran past its deadline of {seconds} s and was killed"),
            Failure::Exited { what, status, tail } => write!(f, "{what} exited {status}:\n{tail}"),
            Failure::NotLocked { name, version } => {
                write!(f, "the checkout's Cargo.lock has no package {name} {version} (with a checksum, for a registry one)")
            }
            Failure::NoSandbox { why } => write!(
                f,
                "no sandbox can be made on this host ({why}); build scripts are not run outside one unless the input says \"unsandboxed_build_scripts\": true"
            ),
            Failure::NoModule { name, version, why } => write!(f, "{name} {version}: no module was built: {why}"),
            Failure::Harness { name, why } => write!(f, "{name}: its macros could not be read from the expanded crate: {why}"),
            Failure::Conflict { key, held, made } => write!(
                f,
                "the store holds module {key} with wasm {held}, and this build made {made}: one key, two values (rule 21); the key misses an input"
            ),
            Failure::Manifest { path, why } => write!(f, "{}: {why}", path.display()),
            Failure::Unvendored { store, missing } => {
                write!(
                    f,
                    "the fetch store {} cannot give {} package(s) the build needs, and with \"vendor_from\" nothing is fetched from the network:",
                    store.display(),
                    missing.len()
                )?;
                missing.iter().try_for_each(|it| write!(f, "\n  {it}"))
            }
        }
    }
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> Failure + '_ {
    move |error| Failure::Io { path: path.to_owned(), error }
}

fn read(path: &Path) -> Result<String, Failure> {
    std::fs::read_to_string(path).map_err(io(path))
}

fn write(path: &Path, bytes: impl AsRef<[u8]>) -> Result<(), Failure> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(io(parent))?;
    }
    std::fs::write(path, bytes).map_err(io(path))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn sha256(bytes: &[u8]) -> String {
    hex(&blaze_sha256::digest(bytes))
}

/// What a program printed and how it ended.
struct Ran {
    status: i32,
    output: Vec<u8>,
}

/// The one place this builder starts the toolchain (cargo, rustc, and the sandbox wrapping
/// cargo): `run` and both of `wrap`'s rustc calls take their `Command` from here.
///
/// RULE 1 BUG, counted here and only here: the interim bootstrap builder starts the pinned cargo
/// and rustc (D52; D110 point 2). Deleted when blaze compiles proc-macro crates itself.
#[allow(clippy::disallowed_methods, reason = "RULE 1 BUG, counted here and only here: the interim bootstrap builder starts the pinned cargo and rustc (D52; D110 point 2)")]
fn toolchain_command(program: &OsStr) -> Command {
    Command::new(program)
}

/// Runs `argv` in `cwd` with exactly `env` (nothing inherited), its output and error captured
/// together, killed at `limit`.
fn run(argv: &[OsString], cwd: &Path, env: &[(String, OsString)], limit: Duration, what: &str) -> Result<Ran, Failure> {
    let (program, args) = argv.split_first().expect("a command names its program");
    let mut command = toolchain_command(program);
    command.args(args).current_dir(cwd).env_clear().stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    for (name, value) in env {
        command.env(name, value);
    }
    let program = program.to_string_lossy().into_owned();
    let mut child = command.spawn().map_err(|error| Failure::Start { program: program.clone(), error })?;
    let drain = |pipe: Option<Box<dyn std::io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            bytes
        })
    };
    let stdout = drain(child.stdout.take().map(|it| Box::new(it) as Box<dyn std::io::Read + Send>));
    let stderr = drain(child.stderr.take().map(|it| Box::new(it) as Box<dyn std::io::Read + Send>));
    let deadline = Instant::now() + limit;
    // Bounded: it ends at the deadline at the latest, polling every 50 ms.
    let status = loop {
        match child.try_wait().map_err(|error| Failure::Start { program: program.clone(), error })? {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Failure::Deadline { what: what.to_owned(), seconds: limit.as_secs() });
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    };
    let mut output = stdout.join().unwrap_or_default();
    output.extend(stderr.join().unwrap_or_default());
    Ok(Ran { status: status.code().unwrap_or(-1), output })
}

/// The last `lines` lines of `output`.
fn tail(output: &[u8], lines: usize) -> String {
    let text = String::from_utf8_lossy(output);
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

fn ran_ok(ran: Ran, what: &str) -> Result<Ran, Failure> {
    if ran.status == 0 {
        Ok(ran)
    } else {
        Err(Failure::Exited { what: what.to_owned(), status: ran.status, tail: tail(&ran.output, 40) })
    }
}

/// The checksum the lock gives `name` `version`, from its `[[package]]` tables.
/// The digest of the macro crate's dependency closure as the lock pins it, as
/// `blaze_proc_macro::module::lock_closure` computes it (the example `tempo_derives` finds the
/// module by blaze's): every package reachable through the lock's `dependencies` lists, the
/// crate left out, each `name version pin` (checksum, else source), sorted, one a line, SHA-256.
/// The checked-in source corrections (D58): compile-time data, a few hundred bytes of JSON read
/// where asked.
fn corrections() -> blaze_fetch::correct::Table<blaze_own::Alloc<'static>> {
    blaze_fetch::correct::Table::checked_in(blaze_own::Alloc::global())
}

/// Applies the source corrections (D58) to the vendored copy of every crate a row names, in
/// place under `<work>/vendor` (the run's own copy; the registry is never written), and
/// rewrites each corrected file's entry in the copy's `.cargo-checksum.json` (its `package`
/// checksum, the lock's pin, is kept). Prints one line per crate corrected.
fn correct_vendored(vendor: &Path) -> Result<(), Failure> {
    let refuse = |path: &Path, why: String| Failure::Manifest { path: path.to_owned(), why };
    let entries = std::fs::read_dir(vendor).map_err(io(vendor))?;
    let mut dirs: Vec<PathBuf> = Vec::new();
    for entry in entries {
        dirs.push(entry.map_err(io(vendor))?.path());
    }
    dirs.sort();
    let table = corrections();
    for dir in dirs {
        let Some(base) = dir.file_name().and_then(|it| it.to_str()) else { continue };
        // `--versioned-dirs`: `<name>-<version>`, the version after the last `-` that starts one.
        let Some((name, version)) = base.char_indices().filter(|(_, c)| *c == '-').map(|(at, _)| (&base[..at], &base[at + 1..])).find(|(_, v)| v.starts_with(|c: char| c.is_ascii_digit())) else { continue };
        let Some(row) = table.find_text(name, version) else { continue };
        let mut texts: Vec<(String, Vec<u8>)> = Vec::new();
        for file in row.files.iter() {
            let path = dir.join(file.as_str());
            match std::fs::read(&path) {
                Ok(bytes) => texts.push((file.as_str().to_owned(), bytes)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(Failure::Io { path, error }),
            }
        }
        let corrected = row
            .correct_crate(version, |file| texts.iter().find(|(path, _)| path == file).map(|(_, bytes)| bytes.as_slice()), blaze_lex_cpu(), blaze_own::Alloc::global())
            .map_err(|why| refuse(&dir, why.to_string()))?;
        let checksum_path = dir.join(".cargo-checksum.json");
        let mut checksums: serde_json::Value = serde_json::from_str(&read(&checksum_path)?).map_err(|error| refuse(&checksum_path, error.to_string()))?;
        let mut count = 0;
        for (file, fixed) in corrected.iter() {
            write(&dir.join(file), fixed.text.as_str())?;
            let entry = checksums.get_mut("files").and_then(|files| files.get_mut(*file)).ok_or_else(|| refuse(&checksum_path, format!("lists no {file}")))?;
            *entry = serde_json::Value::String(sha256(fixed.text.as_str().as_bytes()));
            count += fixed.replaced.len();
        }
        write(&checksum_path, serde_json::to_string(&checksums).map_err(|error| refuse(&checksum_path, error.to_string()))?)?;
        println!("corrected: {name} {version}: {count} predicates ({})", row.decision.as_str());
    }
    Ok(())
}

/// The lexer's vector width: the target's baseline vectors, on every host.
fn blaze_lex_cpu() -> blaze_lex::Cpu {
    blaze_lex::Cpu::BASELINE
}

fn lock_closure(lock: &str, name: &str, version: &str) -> Result<String, Failure> {
    Ok(sha256(closure_lines(lock, name, version)?.join("\n").as_bytes()))
}

/// The closure's packages as [`lock_closure`] hashes them: `name version pin`, sorted.
fn closure_lines(lock: &str, name: &str, version: &str) -> Result<Vec<String>, Failure> {
    let refuse = |why: String| Failure::Manifest { path: PathBuf::from("Cargo.lock"), why: format!("{name} {version}: {why}") };
    struct Locked<'l> {
        name: &'l str,
        version: &'l str,
        source: Option<&'l str>,
        checksum: Option<&'l str>,
        deps: Vec<&'l str>,
    }
    let mut packages = Vec::new();
    for entry in lock.split("[[package]]").skip(1) {
        let field = |key: &str| entry.lines().find_map(|line| line.trim().strip_prefix(key)).map(|it| it.trim().trim_matches('"'));
        let mut deps = Vec::new();
        if let Some(at) = entry.find("dependencies = [") {
            let list = &entry[at + "dependencies = [".len()..];
            let list = &list[..list.find(']').unwrap_or(list.len())];
            deps.extend(list.split(',').map(|it| it.trim().trim_matches('"')).filter(|it| !it.is_empty()));
        }
        packages.push(Locked {
            name: field("name = ").unwrap_or(""),
            version: field("version = ").unwrap_or(""),
            source: field("source = "),
            checksum: field("checksum = "),
            deps,
        });
    }
    let root = packages.iter().position(|it| it.name == name && it.version == version).ok_or_else(|| refuse("not in the lock".to_owned()))?;
    let mut seen = vec![false; packages.len()];
    seen[root] = true;
    let mut stack = vec![root];
    while let Some(at) = stack.pop() {
        for dep in &packages[at].deps {
            let mut parts = dep.splitn(3, ' ');
            let (dep_name, dep_version, dep_source) = (parts.next().unwrap_or(""), parts.next(), parts.next().map(|it| it.trim_matches(|c| c == '(' || c == ')')));
            let mut found = packages.iter().enumerate().filter(|(_, it)| {
                it.name == dep_name && dep_version.is_none_or(|v| it.version == v) && dep_source.is_none_or(|s| it.source == Some(s))
            });
            let (Some((index, _)), None) = (found.next(), found.next()) else {
                return Err(refuse(format!("the dependency {dep:?} of {} {} names no one package of the lock", packages[at].name, packages[at].version)));
            };
            if !seen[index] {
                seen[index] = true;
                stack.push(index);
            }
        }
    }
    let mut lines = Vec::new();
    for (at, package) in packages.iter().enumerate() {
        if !seen[at] || at == root {
            continue;
        }
        let pin = package.checksum.or(package.source).ok_or_else(|| {
            refuse(format!("{} {} is a path package under it: the lock pins nothing of its sources", package.name, package.version))
        })?;
        // A package a source correction (D58) applies to is built corrected: its pin names the
        // row's digest, as blaze-proc-macro's `LockedPackages::closure` does.
        let pin = corrections().pin(package.name, package.version, pin);
        lines.push(format!("{} {} {pin}", package.name, package.version));
    }
    lines.sort();
    Ok(lines)
}

/// Cargo's `--message-format=json` output split into what the build used and what a person
/// reads: `name version manifest-dir` for each `compiler-artifact` (fresh ones too, so a
/// package built for an earlier crate in the shared target folder is still seen), and the
/// output with each message's `rendered` text in place of its JSON.
fn artifacts_of(output: &str) -> (String, String) {
    let (mut compiled, mut readable) = (String::new(), String::new());
    for line in output.lines() {
        let Ok(serde_json::Value::Object(message)) = serde_json::from_str::<serde_json::Value>(line) else {
            readable.push_str(line);
            readable.push('\n');
            continue;
        };
        let text = |key: &str| message.get(key).and_then(serde_json::Value::as_str).unwrap_or("");
        match text("reason") {
            "compiler-artifact" => {
                // `<source>#<name>@<version>`, or `<source>/<name>#<version>` where the
                // package's folder bears its name.
                let id = text("package_id");
                let (source, spec) = id.rsplit_once('#').unwrap_or((id, ""));
                let (name, version) = match spec.split_once('@') {
                    Some(pair) => pair,
                    None => (source.rsplit('/').next().unwrap_or("").split('?').next().unwrap_or(""), spec),
                };
                let dir = Path::new(text("manifest_path")).parent().map(|it| it.display().to_string()).unwrap_or_default();
                compiled.push_str(&format!("{name} {version} {dir}\n"));
            }
            "compiler-message" => {
                if let Some(rendered) = message.get("message").and_then(|it| it.get("rendered")).and_then(serde_json::Value::as_str) {
                    readable.push_str(rendered);
                }
            }
            _ => {}
        }
    }
    (compiled, readable)
}

/// Refuses a build that compiled a package the lock does not put under the macro crate: the
/// build's artifacts are `name version manifest-dir` lines ([`artifacts_of`]); the standard
/// library's own crates (under `library`), the shim and the crate itself are left out. Every
/// other one must be a `name version` of the lock's closure, or of the toolchain's
/// `library/Cargo.lock` (`-Zbuild-std` compiles std's registry dependencies, addr2line,
/// hashbrown and the rest, through the same vendor folder; the toolchain line of the identity
/// pins them). A subset, not equality: the closure lists the dependencies of every feature, a
/// build compiles those its features turn on. Not caught: a drift onto exactly a version std's
/// lock also names.
fn check_compiled(compiled: &str, closure: &[String], std_lock: &str, library: &Path, name: &str, version: &str) -> Result<usize, Failure> {
    let mut locked: Vec<(&str, &str)> = closure.iter().filter_map(|line| {
        let mut parts = line.split(' ');
        Some((parts.next()?, parts.next()?))
    }).collect();
    for entry in std_lock.split("[[package]]").skip(1) {
        let field = |key: &str| entry.lines().find_map(|line| line.trim().strip_prefix(key)).map(|it| it.trim().trim_matches('"'));
        if let (Some(std_name), Some(std_version)) = (field("name = "), field("version = ")) {
            locked.push((std_name, std_version));
        }
    }
    let sysroot = library.display().to_string();
    let mut drifted = Vec::new();
    let mut checked = Vec::new();
    for line in compiled.lines() {
        let mut parts = line.splitn(3, ' ');
        let (Some(got_name), Some(got_version), Some(dir)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let ours = (got_name == name && got_version == version) || got_name == "blaze_pm_shim";
        if ours || dir.starts_with(&sysroot) {
            continue;
        }
        if !locked.contains(&(got_name, got_version)) {
            drifted.push(format!("{got_name} {got_version}"));
        }
        checked.push((got_name, got_version));
    }
    checked.sort_unstable();
    checked.dedup();
    if drifted.is_empty() {
        return Ok(checked.len());
    }
    drifted.sort();
    drifted.dedup();
    Err(Failure::NoModule {
        name: name.to_owned(),
        version: version.to_owned(),
        why: format!("cargo compiled packages the lock does not pin under it (rule 21): {}", drifted.join(", ")),
    })
}

/// The lock for the build's workspace: the fetch's, with the macro crate's own entry made a
/// path package's (its `source` and `checksum` lines out), as the workspace's member is one.
/// Kept a registry or git entry, it matches no package of the workspace, and cargo resolves
/// the member's dependencies afresh: other versions than the lock's (syn 1 for a crate the
/// lock gives syn 3).
fn member_lock(lock: &str, name: &str, version: &str) -> String {
    let mut out = String::with_capacity(lock.len());
    let mut entries = lock.split("[[package]]");
    out.push_str(entries.next().unwrap_or(""));
    let (name_line, version_line) = (format!("name = {name:?}"), format!("version = {version:?}"));
    for entry in entries {
        out.push_str("[[package]]");
        let is_it = entry.lines().any(|line| line.trim() == name_line) && entry.lines().any(|line| line.trim() == version_line);
        if !is_it {
            out.push_str(entry);
            continue;
        }
        for line in entry.split_inclusive('\n') {
            let trimmed = line.trim();
            if !(trimmed.starts_with("source = ") || trimmed.starts_with("checksum = ")) {
                out.push_str(line);
            }
        }
    }
    out
}

/// The lock's entry of `name` `version`: `Some(source)`, `None` inside for a path package (no
/// `source` line); `None` when the lock has no such package.
fn lock_source(lock: &str, name: &str, version: &str) -> Option<Option<String>> {
    let mut entries = lock.split("[[package]]");
    let _ = entries.next();
    for entry in entries {
        let field = |key: &str| entry.lines().find_map(|line| line.trim().strip_prefix(key)).map(|it| it.trim_matches('"').to_owned());
        if field("name = ").as_deref() == Some(name) && field("version = ").as_deref() == Some(version) {
            return Some(field("source = "));
        }
    }
    None
}

/// A member of the checkout's workspace as cargo reads it: its `package.name`, its version (an
/// inherited `version.workspace = true` is the root's `workspace.package.version`; none written
/// is cargo's `0.0.0`) and its folder.
struct PathPackage {
    name: String,
    version: String,
    dir: PathBuf,
}

/// The checkout's workspace members, read with blaze-manifest's layout reader (the one reader of
/// a workspace, crates/blaze-manifest/src/layout) the first time a run asks for a path package,
/// and kept for the run. A lock's path package is a member (cargo makes a path dependency under
/// the root one too), found by its manifest's `package.name` as TOML reads it: a manifest that
/// opens with a comment (turso's `# Copyright 2024 the Limbo authors..` above `[package]`) is
/// read like any other, where a scan of the text before its first table found no name.
struct Members<'c> {
    checkout: &'c Path,
    read: std::cell::OnceCell<Result<Vec<PathPackage>, Failure>>,
}

/// The most passes the layout reader is given: a pass asks for everything it can see it needs,
/// so their number is the depth of the search (the root, the members' globs, path
/// dependencies, the target folders), never the number of files (rule 18: a bound, not a loop).
const LAYOUT_PASSES: usize = 64;

impl<'c> Members<'c> {
    fn new(checkout: &'c Path) -> Members<'c> {
        Members { checkout, read: std::cell::OnceCell::new() }
    }

    /// The folder of the member `wanted` names: matched by name, then its version checked
    /// against the lock's (inherited from the root's `[workspace.package]` where it says so).
    fn find(&self, wanted: &Wanted, alloc: blaze_own::Alloc<'_>) -> Result<PathBuf, Failure> {
        let root = self.checkout.join("Cargo.toml");
        let members = match self.read.get_or_init(|| read_members(self.checkout, alloc)) {
            Ok(members) => members,
            Err(failure) => {
                return Err(Failure::Manifest { path: root, why: format!("the workspace could not be read as cargo reads it (blaze_manifest::layout): {failure}") });
            }
        };
        let named: Vec<&PathPackage> = members.iter().filter(|it| it.name == wanted.name).collect();
        match named.as_slice() {
            [] => Err(Failure::Manifest {
                path: root,
                why: format!(
                    "the lock names the path package {} {}, and none of the workspace's {} members is named so (read as cargo reads it, blaze_manifest::layout); a path package outside the workspace's members is not built by this tool",
                    wanted.name,
                    wanted.version,
                    members.len()
                ),
            }),
            [one] if one.version == wanted.version => Ok(one.dir.clone()),
            [one] => Err(Failure::Manifest {
                path: one.dir.join("Cargo.toml"),
                why: format!(
                    "the member {} is version {} (its `package.version`, or the root's `workspace.package.version` it inherits), and the lock names {} {}",
                    one.name, one.version, wanted.name, wanted.version
                ),
            }),
            more => Err(Failure::Manifest {
                path: root,
                why: format!("{} members are named {}: {}", more.len(), wanted.name, more.iter().map(|it| it.dir.display().to_string()).collect::<Vec<_>>().join(", ")),
            }),
        }
    }
}

/// The checkout's workspace members: blaze_manifest's sans-IO layout reader, started at the
/// checkout's root and answered from its files. A file or folder that is not there is answered
/// `None`, as the reader asks; any other error reading one stops the read, naming it.
fn read_members(checkout: &Path, alloc: blaze_own::Alloc<'_>) -> Result<Vec<PathPackage>, Failure> {
    use blaze_manifest::layout::{Entry, Layout, Need, Pass};
    let absent = |error: &std::io::Error| matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory);
    let root = checkout.join("Cargo.toml");
    let mut reader = Layout::new("", alloc).map_err(|why| Failure::Manifest { path: root.clone(), why: why.to_string() })?;
    for _ in 0..LAYOUT_PASSES {
        let needs = match reader.pass().map_err(|why| Failure::Manifest { path: root.clone(), why: why.to_string() })? {
            Pass::Done(units) => {
                let mut out = Vec::with_capacity(units.members.len());
                for member in units.members.iter() {
                    let dir = member.dir.as_str();
                    out.push(PathPackage {
                        name: member.name.as_str().to_owned(),
                        version: member.version.as_ref().map_or("0.0.0", |it| it.as_str()).to_owned(),
                        dir: if dir.is_empty() { checkout.to_owned() } else { checkout.join(dir) },
                    });
                }
                return Ok(out);
            }
            Pass::Need(needs) => needs,
        };
        for need in needs.iter() {
            match need {
                Need::Manifest(path) => {
                    let file = checkout.join(path.as_str());
                    let text = match std::fs::read_to_string(&file) {
                        Ok(text) => Some(text),
                        Err(error) if absent(&error) => None,
                        Err(error) => return Err(Failure::Io { path: file, error }),
                    };
                    reader.manifest(path.as_str(), text.as_deref()).map_err(|why| Failure::Manifest { path: file.clone(), why: why.to_string() })?;
                }
                Need::Listing(dir) => {
                    let folder = checkout.join(dir.as_str());
                    let listed: Option<Vec<(String, bool)>> = match std::fs::read_dir(&folder) {
                        Ok(entries) => {
                            let mut names = Vec::new();
                            for entry in entries {
                                let entry = entry.map_err(io(&folder))?;
                                let path = entry.path();
                                let kind = entry.file_type().map_err(io(&path))?;
                                // A link to a folder is a folder, as cargo's walk follows it; a
                                // dangling link is no folder, as the OS says.
                                let is_dir = kind.is_dir() || (kind.is_symlink() && std::fs::metadata(&path).is_ok_and(|it| it.is_dir()));
                                let name = entry.file_name().into_string().map_err(|name| Failure::Manifest {
                                    path: folder.clone(),
                                    why: format!("the entry {name:?} is not UTF-8, which a workspace's paths must be"),
                                })?;
                                names.push((name, is_dir));
                            }
                            Some(names)
                        }
                        Err(error) if absent(&error) => None,
                        Err(error) => return Err(Failure::Io { path: folder, error }),
                    };
                    let entries: Option<Vec<Entry<'_>>> = listed.as_ref().map(|names| names.iter().map(|(name, is_dir)| Entry { name, is_dir: *is_dir }).collect());
                    reader.listing(dir.as_str(), entries.as_deref());
                }
            }
        }
    }
    Err(Failure::Manifest { path: root, why: format!("the layout reader did not finish in {LAYOUT_PASSES} passes") })
}

/// The `package.name` a manifest's text gives, as TOML reads it; `None` for a manifest with
/// none (a virtual workspace's root). A text that is no TOML is the error, for the caller to
/// name.
fn manifest_package_name(text: &str, alloc: blaze_own::Alloc<'_>) -> Result<Option<String>, blaze_toml::Error> {
    let doc = blaze_toml::Document::parse_in(text, blaze_toml::Limits::CARGO, alloc)?;
    Ok(doc.root().get("package").and_then(|package| package.get("name")).and_then(|name| name.as_str()).map(str::to_owned))
}

/// Where a crate's sources are, as the checkout's lock pins them.
enum Origin {
    /// From crates.io, vendored by the fetch: the lock's checksum.
    Registry { checksum: String },
    /// A workspace or path package of the checkout: its folder, [`files_digest`] of it, and its
    /// manifest with what it inherits written in ([`inherit`]).
    Path { dir: PathBuf, files: String, manifest: String },
    /// A package of a git commit the lock pins: the commit, the package's folder in it (`.` at
    /// the root), the commit's files as the fetch store's verified pack holds them written out
    /// under `<work>/git/<commit>` (`dir` is the package's folder there), and its manifest with
    /// what it inherits from the commit's root written in.
    Git { commit: String, path: String, dir: PathBuf, manifest: String },
}

/// The commit's files from the fetch store, written under `<work>/git/<commit>`, and the
/// folder in it of the package named `wanted.name`. The record is found by
/// `SourceId::Git { commit }.key()` and opened by `PackView::open_source`, which refuses it
/// unless it holds that commit's files; every path is the pack's, already held inside its
/// folder by the fetch.
fn git_origin(args: &Args, wanted: &Wanted, source: &str, alloc: blaze_own::Alloc<'_>) -> Result<Origin, Failure> {
    let lock = args.checkout.join("Cargo.lock");
    let refuse = |why: String| Failure::Manifest { path: lock.clone(), why: format!("{} {}: {why}", wanted.name, wanted.version) };
    let commit = source.rsplit_once('#').map(|it| it.1).unwrap_or("");
    let mut oid = [0u8; 20];
    if commit.len() != 40 || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(refuse(format!("the git source {source} pins no 40-digit commit")));
    }
    for (at, byte) in oid.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&commit[at * 2..at * 2 + 2], 16).map_err(|_| refuse(format!("the commit {commit} is not hex")))?;
    }
    let store = args.fetch_store.as_ref().ok_or_else(|| refuse(format!("comes from git ({source}): the input names no \"fetch_store\" to read it from")))?;
    let id = blaze_fetch::SourceId::Git { commit: blaze_git_fetch::Oid(oid) };
    let record = store.join("records").join(hex(&id.key().0));
    let bytes = std::fs::read(&record).map_err(io(&record))?;
    let view = blaze_fetch::PackView::open_source(&id, &bytes)
        .map_err(|why| Failure::Manifest { path: record.clone(), why: format!("the record of commit {commit}: {}", blaze_text::Shown(&why)) })?;
    let root = args.work.join("git").join(commit);
    let mut found: Option<String> = None;
    for file in view.files() {
        let out = root.join(file.path);
        if !out.exists() {
            write(&out, file.bytes)?;
        }
        let is_manifest = file.path == "Cargo.toml" || file.path.ends_with("/Cargo.toml");
        if !is_manifest {
            continue;
        }
        // Every manifest of the commit is a candidate, as cargo walks a git source; its name is
        // read as TOML (a comment may open it). One that is no TOML is no package, as cargo
        // skips a malformed package there; it is named, not passed over in silence.
        let name = match std::str::from_utf8(file.bytes) {
            Ok(text) => match manifest_package_name(text, alloc) {
                Ok(name) => name,
                Err(why) => {
                    println!("{SCRIPT}: {} {}: commit {commit}: {} is no TOML ({}), so no package", wanted.name, wanted.version, file.path, why.display(text));
                    None
                }
            },
            Err(_) => {
                println!("{SCRIPT}: {} {}: commit {commit}: {} is not UTF-8, so no package", wanted.name, wanted.version, file.path);
                None
            }
        };
        if name.as_deref() == Some(wanted.name.as_str()) {
            let folder = file.path.strip_suffix("Cargo.toml").unwrap_or("").trim_end_matches('/');
            // The shortest such folder: a package copied into a test fixture deeper down is not it.
            if found.as_ref().is_none_or(|held| folder.len() < held.len()) {
                found = Some(folder.to_owned());
            }
        }
    }
    let path = found.ok_or_else(|| refuse(format!("commit {commit} has no Cargo.toml naming the package")))?;
    let dir = if path.is_empty() { root.clone() } else { root.join(&path) };
    let manifest_path = dir.join("Cargo.toml");
    let manifest = inherit(&read(&manifest_path)?, &read(&root.join("Cargo.toml"))?, &root, &manifest_path)?;
    Ok(Origin::Git { commit: commit.to_owned(), path: if path.is_empty() { ".".to_owned() } else { path }, dir, manifest })
}

impl Origin {
    /// Finds `wanted` in the lock and, for a path package, among the checkout's workspace
    /// `members`; for a git one, in the fetch store.
    fn of(args: &Args, lock: &str, members: &Members<'_>, wanted: &Wanted, alloc: blaze_own::Alloc<'_>) -> Result<Origin, Failure> {
        let checkout = &args.checkout;
        let not_locked = || Failure::NotLocked { name: wanted.name.clone(), version: wanted.version.clone() };
        match lock_source(lock, &wanted.name, &wanted.version).ok_or_else(not_locked)? {
            Some(source) if source.starts_with("registry+") => {
                Ok(Origin::Registry { checksum: checksum(lock, &wanted.name, &wanted.version).ok_or_else(not_locked)? })
            }
            Some(source) if source.starts_with("git+") => git_origin(args, wanted, &source, alloc),
            Some(source) => Err(Failure::Manifest {
                path: checkout.join("Cargo.lock"),
                why: format!("{} {} comes from {source}, which this tool does not build yet", wanted.name, wanted.version),
            }),
            None => {
                let dir = members.find(wanted, alloc)?;
                let path = dir.join("Cargo.toml");
                let manifest = inherit(&read(&path)?, &read(&checkout.join("Cargo.toml"))?, checkout, &path)?;
                Ok(Origin::Path { files: files_digest(&dir)?, dir, manifest })
            }
        }
    }

    /// The line of the identity that pins the sources, as `blaze_proc_macro::module::identity`
    /// writes it.
    fn identity_line(&self) -> String {
        match self {
            Origin::Registry { checksum } => format!("checksum {checksum}"),
            Origin::Path { files, .. } => format!("files {files}"),
            Origin::Git { commit, path, .. } => format!("git {commit} {path}"),
        }
    }
}

fn checksum(lock: &str, name: &str, version: &str) -> Option<String> {
    let mut at_name = false;
    let mut at_version = false;
    for line in lock.lines() {
        let line = line.trim();
        if line == "[[package]]" {
            at_name = false;
            at_version = false;
        } else if let Some(value) = line.strip_prefix("name = ") {
            at_name = value.trim_matches('"') == name;
        } else if let Some(value) = line.strip_prefix("version = ") {
            at_version = value.trim_matches('"') == version;
        } else if let Some(value) = line.strip_prefix("checksum = ")
            && at_name
            && at_version
        {
            return Some(value.trim_matches('"').to_owned());
        }
    }
    None
}

/// The toolchain and where its files are.
struct Toolchain {
    name: String,
    sysroot: PathBuf,
}

impl Toolchain {
    fn cargo(&self) -> OsString {
        self.sysroot.join("bin/cargo").into_os_string()
    }

    fn rustc(&self) -> PathBuf {
        self.sysroot.join("bin/rustc")
    }

    /// The std workspace's manifest, whose dependencies `-Zbuild-std` builds.
    fn library(&self) -> PathBuf {
        self.sysroot.join("lib/rustlib/src/rust/library")
    }
}

fn toolchain(tree: &Path, home: &str) -> Result<Toolchain, Failure> {
    let name = read(&tree.join(blaze_pins::TOOLCHAIN_PATH))?.trim().to_owned();
    let rustup = PathBuf::from(home).join(".rustup/toolchains");
    let entries = std::fs::read_dir(&rustup).map_err(io(&rustup))?;
    let mut found = None;
    for entry in entries.flatten() {
        let file = entry.file_name();
        if file.to_string_lossy().starts_with(&format!("{name}-")) {
            found = Some(entry.path());
        }
    }
    let sysroot = found.ok_or_else(|| Failure::Io {
        path: rustup.join(&name),
        error: std::io::Error::new(std::io::ErrorKind::NotFound, format!("the pinned toolchain {name} is not installed")),
    })?;
    Ok(Toolchain { name, sysroot })
}

/// The crate's name as rustc's `--crate-name` has it.
fn crate_name(name: &str) -> String {
    name.replace('-', "_")
}

/// The member manifest made from the crate's own: everything as the crate has it, its
/// `[lib]` pointing at its unchanged source with `proc-macro` taken out, and its targets other
/// than the library, its dev-dependencies and its file references dropped.
fn member_manifest(original: &str, source: &Path, path: &Path, features: &[String], alloc: blaze_own::Alloc<'_>) -> Result<String, Failure> {
    let filtered = member_dependencies::filtered_manifest(original, path, features, alloc)?;
    let original = filtered.as_str();
    let mut out = String::with_capacity(original.len() + 256);
    let mut section = String::new();
    let mut lib_path = false;
    let mut skip = false;
    // Inside a dropped key's value that spans lines (`include = [` up to its `]`).
    let mut dropping = false;
    // Inside a multi-line string (`"""` or `'''`): its lines are the value's, never a table.
    let mut in_string: Option<&str> = None;
    for line in original.lines() {
        let trimmed = line.trim();
        let opens = |quote: &str| line.matches(quote).count() % 2 == 1;
        if let Some(quote) = in_string {
            if !skip && !dropping {
                out.push_str(line);
                out.push('\n');
            }
            if opens(quote) {
                in_string = None;
            }
            continue;
        }
        if let Some(quote) = ["\"\"\"", "'''"].into_iter().find(|quote| opens(quote)) {
            in_string = Some(quote);
            if !skip && !dropping {
                out.push_str(line);
                out.push('\n');
            }
            continue;
        }
        if dropping {
            dropping = !trimmed.starts_with(']');
            continue;
        }
        if trimmed.starts_with('[') {
            if section == "lib" && !lib_path {
                out.push_str(&format!("path = {:?}\n", source.join("src/lib.rs").display().to_string()));
            }
            section = trimmed.trim_matches(|c| c == '[' || c == ']').trim().to_owned();
            skip = matches!(section.as_str(), "bin" | "test" | "bench" | "example")
                || section == "badges"
                || section.starts_with("package.metadata")
                || section == "lints"
                || section.starts_with("lints.");
            lib_path = false;
            if !skip {
                out.push_str(line);
                out.push('\n');
            }
            continue;
        }
        if skip {
            continue;
        }
        let key = trimmed.split('=').next().unwrap_or("").trim();
        let value = trimmed.split_once('=').map_or("", |it| it.1.trim());
        // A dropped key whose array goes on below.
        let opens = value.starts_with('[') && !value.ends_with(']');
        match (section.as_str(), key) {
            // A `cdylib` to cargo, which then waits for its dependencies' whole libraries and
            // passes them, as it does for a crate it links; the wrapper gives rustc
            // `proc-macro`.
            ("lib", "proc-macro" | "proc_macro") => {
                out.push_str("crate-type = [\"cdylib\"]\n");
                continue;
            }
            ("lib", "path") => {
                let value = trimmed.split_once('=').map(|it| it.1.trim().trim_matches('"')).unwrap_or("");
                out.push_str(&format!("path = {:?}\n", source.join(value).display().to_string()));
                lib_path = true;
                continue;
            }
            ("package", "build") => {
                let value = trimmed.split_once('=').map(|it| it.1.trim()).unwrap_or("");
                if value == "false" {
                    out.push_str("build = false\n");
                } else {
                    let value = value.trim_matches('"');
                    out.push_str(&format!("build = {:?}\n", source.join(value).display().to_string()));
                }
                continue;
            }
            ("package", "readme" | "license-file" | "include" | "exclude" | "workspace")
            | ("package", "autobins" | "autoexamples" | "autotests" | "autobenches" | "autolib") => {
                dropping = opens;
                continue;
            }
            _ => {}
        }
        out.push_str(line);
        out.push('\n');
    }
    if section == "lib" && !lib_path {
        out.push_str(&format!("path = {:?}\n", source.join("src/lib.rs").display().to_string()));
    }
    if !original.contains("\n[lib]") && !original.starts_with("[lib]") {
        return Err(Failure::Manifest {
            path: path.to_owned(),
            why: "has no [lib] table; cargo's normalized manifest of a proc-macro crate always has one".to_owned(),
        });
    }
    out.push_str(
        "autobins = false\n\n[package.metadata.blaze]\nnote = \"made by proc_macro_modules from the crate's own manifest\"\n\n[dependencies.blaze_pm_shim]\npath = \"../shim\"\n",
    );
    // `autobins` belongs in [package]: put the auto flags right after the [package] header.
    let out = out.replacen("autobins = false\n\n[package.metadata.blaze]", "\n[package.metadata.blaze]", 1);
    let out = out.replacen("[package]\n", "[package]\nautobins = false\nautoexamples = false\nautotests = false\nautobenches = false\n", 1);
    Ok(out)
}

/// The optional dependencies a normalized manifest declares (`[dependencies.x]`,
/// `[build-dependencies.x]` and their `[target.*]` forms, with `optional = true`), as
/// (package, version requirement).
/// The value text of `key` among the `key = value` lines of the table `[table]` of
/// `manifest`: to the end of its line, or, for an inline table or array whose brackets open
/// over lines (`corosensei = { version = "0.3.4", features = [\n "unwind",\n] }`), to where
/// they close.
fn table_value<'m>(manifest: &'m str, table: &str, key: &str) -> Option<&'m str> {
    let mut inside = false;
    let mut offset = 0;
    for line in manifest.split_inclusive('\n') {
        let at = offset;
        offset += line.len();
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            inside = trimmed.trim_matches(|c| c == '[' || c == ']').trim() == table;
            continue;
        }
        let Some((name, _)) = trimmed.split_once('=') else { continue };
        if !inside || name.trim().trim_matches('"') != key {
            continue;
        }
        let start = at + line.find('=')? + 1;
        let (mut depth, mut quoted) = (0_i32, false);
        for (index, c) in manifest[start..].char_indices() {
            match c {
                '"' => quoted = !quoted,
                '[' | '{' if !quoted => depth += 1,
                ']' | '}' if !quoted => depth -= 1,
                '\n' if depth <= 0 => return Some(manifest[start..start + index].trim()),
                _ => {}
            }
        }
        return Some(manifest[start..].trim());
    }
    None
}

/// The entries of an inline table's text `{ a = 1, b = [x, y] }`, split at its top-level
/// commas, each trimmed; `None` for text that is not one inline table on its line.
fn inline_entries(value: &str) -> Option<Vec<&str>> {
    let inner = value.trim().strip_prefix('{')?.strip_suffix('}')?;
    let (mut out, mut depth, mut start, mut quoted) = (Vec::new(), 0_i32, 0, false);
    for (at, c) in inner.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '[' | '{' if !quoted => depth += 1,
            ']' | '}' if !quoted => depth -= 1,
            ',' if !quoted && depth == 0 => {
                out.push(inner[start..at].trim());
                start = at + 1;
            }
            _ => {}
        }
    }
    out.push(inner[start..].trim());
    out.retain(|it| !it.is_empty());
    Some(out)
}

/// A workspace member's manifest with what it inherits from the workspace's root manifest
/// written in: `key.workspace = true` in `[package]` takes the root's
/// `[workspace.package]` value, and a dependency `{ workspace = true, .. }` the root's
/// `[workspace.dependencies]` entry, its `features` joined with the member's and a `path` made
/// absolute from `root_dir`. Line by line, as cargo's manifests are written; a form it does
/// not take (a dependency table `[dependencies.x]` with `workspace = true`, a value over
/// lines) is a refusal naming it.
fn inherit(member: &str, root: &str, root_dir: &Path, path: &Path) -> Result<String, Failure> {
    let refuse = |why: String| Failure::Manifest { path: path.to_owned(), why };
    let mut out = String::with_capacity(member.len() + 512);
    let mut section = String::new();
    for line in member.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            section = trimmed.trim_matches(|c| c == '[' || c == ']').trim().to_owned();
            out.push_str(line);
            out.push('\n');
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            out.push_str(line);
            out.push('\n');
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        let dependencies = section == "dependencies"
            || section == "build-dependencies"
            || section.ends_with(".dependencies")
            || section.ends_with(".build-dependencies")
            || section == "dev-dependencies"
            || section.ends_with(".dev-dependencies");
        if section == "package" && key.ends_with(".workspace") {
            let name = key.trim_end_matches(".workspace");
            let inherited = table_value(root, "workspace.package", name)
                .ok_or_else(|| refuse(format!("`{name}.workspace = true`, but the root has no `workspace.package.{name}` on one line")))?;
            out.push_str(&format!("{name} = {inherited}\n"));
        } else if dependencies && (key.ends_with(".workspace") || inline_entries(value).is_some_and(|it| it.contains(&"workspace = true"))) {
            let name = key.trim_end_matches(".workspace").trim_matches('"');
            let theirs = table_value(root, "workspace.dependencies", name)
                .ok_or_else(|| refuse(format!("dependency `{name}` inherits, but the root has no `workspace.dependencies.{name}` on one line")))?;
            let mut entries: Vec<String> = if theirs.starts_with('"') {
                vec![format!("version = {theirs}")]
            } else {
                inline_entries(theirs)
                    .ok_or_else(|| refuse(format!("the root's `workspace.dependencies.{name}` is not a string or an inline table: {theirs}")))?
                    .into_iter()
                    .map(|entry| match entry.split_once('=') {
                        Some((k, v)) if k.trim() == "path" => {
                            format!("path = {:?}", root_dir.join(v.trim().trim_matches('"')).display().to_string())
                        }
                        _ => entry.to_owned(),
                    })
                    .collect()
            };
            let mine = if key.ends_with(".workspace") { Vec::new() } else { inline_entries(value).unwrap_or_default() };
            for entry in mine {
                let Some((k, v)) = entry.split_once('=') else { return Err(refuse(format!("dependency `{name}`: the entry {entry:?}"))) };
                match k.trim() {
                    "workspace" => {}
                    "features" => {
                        let add = v.trim().trim_start_matches('[').trim_end_matches(']').trim().trim_end_matches(',');
                        match entries.iter_mut().find(|it| it.split('=').next().is_some_and(|k| k.trim() == "features")) {
                            Some(held) if !add.is_empty() => {
                                let had = held.trim_end().trim_end_matches(']').trim_end().trim_end_matches(',').to_owned();
                                *held = format!("{had}, {add}]");
                            }
                            Some(_) => {}
                            None => entries.push(format!("features = [{add}]")),
                        }
                    }
                    "optional" | "default-features" | "package" => entries.push(entry.to_owned()),
                    other => return Err(refuse(format!("dependency `{name}` inherits and also sets `{other}`"))),
                }
            }
            out.push_str(&format!("{name} = {{ {} }}\n", entries.join(", ")));
        } else if key == "workspace" && value == "true" && dependencies {
            return Err(refuse(format!("the dependency table `[{section}]` inherits; only one-line dependencies are taken")));
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    Ok(out)
}

/// The digest a path package's sources are pinned by, as `blaze_proc_macro::module::
/// files_digest` computes it (the example `tempo_derives` finds the module by blaze's): SHA-256
/// over every regular file of `dir` the one package rule takes (blaze-build-script
/// `served::in_package`, as blaze walks it), in the byte order of their `/`-separated paths,
/// each as the path, a zero byte, its length as 8 little-endian bytes and its bytes. Walked with
/// a stack of folders.
fn files_digest(dir: &Path) -> Result<String, Failure> {
    use blaze_build_script::served::{InPackage, in_package};
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    let mut folders = vec![(dir.to_owned(), String::new())];
    while let Some((folder, relative)) = folders.pop() {
        for entry in std::fs::read_dir(&folder).map_err(io(&folder))? {
            let entry = entry.map_err(io(&folder))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            let kind = entry.file_type().map_err(io(&path))?;
            let place = if relative.is_empty() { InPackage::Root } else { InPackage::Beneath };
            if !in_package(entry.file_name().as_encoded_bytes(), kind.is_dir(), place) {
                continue;
            }
            let at = if relative.is_empty() { name.clone() } else { format!("{relative}/{name}") };
            if kind.is_dir() {
                folders.push((path, at));
            } else if kind.is_file() {
                files.push((at, std::fs::read(&path).map_err(io(&path))?));
            }
        }
    }
    files.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let mut all = Vec::new();
    for (path, bytes) in &files {
        all.extend_from_slice(path.as_bytes());
        all.push(0);
        all.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        all.extend_from_slice(bytes);
    }
    Ok(sha256(&all))
}

/// The names of a vendored manifest's `[features]` table, `default` among them when it has
/// one.
fn feature_names(manifest: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut inside = false;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            inside = trimmed == "[features]";
            continue;
        }
        if inside && let Some((key, _)) = trimmed.split_once('=') {
            let key = key.trim().trim_matches('"');
            if !key.is_empty() && !key.starts_with('#') {
                out.push(key.to_owned());
            }
        }
    }
    out
}

fn optional_dependencies(manifest: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    // The table being read: its package (its name unless `package` says), version, optional.
    let mut table: Option<(String, String, bool)> = None;
    let mut finish = |table: &mut Option<(String, String, bool)>| {
        if let Some((package, version, true)) = table.take()
            && !version.is_empty()
        {
            out.push((package, version));
        }
    };
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            finish(&mut table);
            let name = trimmed.trim_matches(|c| c == '[' || c == ']').trim();
            let dep = name
                .rsplit_once("dependencies.")
                .filter(|(before, _)| before.is_empty() || before.ends_with('.') || before.ends_with("build-"))
                .filter(|(before, _)| !before.ends_with("dev-"))
                .map(|(_, dep)| dep.trim_matches('"').to_owned());
            table = dep.map(|dep| (dep, String::new(), false));
            continue;
        }
        let Some((package, version, optional)) = table.as_mut() else { continue };
        let Some((key, value)) = trimmed.split_once('=') else { continue };
        let value = value.trim().trim_matches('"');
        match key.trim() {
            "package" => *package = value.to_owned(),
            "version" => *version = value.to_owned(),
            "optional" => *optional = value == "true",
            _ => {}
        }
    }
    finish(&mut table);
    out
}

/// The Seatbelt profile of the build on macOS: everything Seatbelt allows by default but the
/// network, and no write but under `work` and the null devices.
fn seatbelt(work: &Path) -> String {
    format!(
        "(version 1)\n(allow default)\n(deny network*)\n(deny file-write*)\n(allow file-write* (subpath {work:?}) (literal \"/dev/null\") (literal \"/dev/zero\") (literal \"/dev/tty\") (literal \"/dev/dtracehelper\") (regex #\"^/dev/fd/\"))\n",
        work = work.display().to_string()
    )
}

/// The command that runs `argv` in the sandbox, or why none can be made.
fn sandboxed(argv: Vec<OsString>, work: &Path) -> Result<Vec<OsString>, String> {
    if cfg!(target_os = "macos") {
        let exec = Path::new("/usr/bin/sandbox-exec");
        if !exec.exists() {
            return Err("/usr/bin/sandbox-exec is missing".to_owned());
        }
        let mut out = vec![exec.as_os_str().to_owned(), "-p".into(), seatbelt(work).into()];
        out.extend(argv);
        Ok(out)
    } else if cfg!(target_os = "linux") {
        let bwrap = ["/usr/bin/bwrap", "/bin/bwrap"].into_iter().map(Path::new).find(|it| it.exists());
        let Some(bwrap) = bwrap else { return Err("no bwrap at /usr/bin or /bin".to_owned()) };
        let mut out: Vec<OsString> = vec![bwrap.as_os_str().to_owned()];
        for arg in ["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc", "--unshare-net", "--die-with-parent", "--bind"] {
            out.push(arg.into());
        }
        out.push(work.as_os_str().to_owned());
        out.push(work.as_os_str().to_owned());
        out.extend(argv);
        Ok(out)
    } else {
        Err("this host has no sandbox the builder knows".to_owned())
    }
}

/// One macro of a module: its index in the harness's list, its kind, its name and helpers.
struct Macro {
    kind: &'static str,
    name: String,
    helpers: Vec<String>,
}

/// The macros of an expanded proc-macro crate, in the harness's order: the `_DECLS` static
/// lists `Client::expand1(f)` (a derive or a function-like macro) or `expand2(f)` (an
/// attribute) per macro, and the attribute on `pub fn f` at the crate's root says which and
/// by what name.
fn macros_of(expanded: &str, name: &str) -> Result<Vec<Macro>, Failure> {
    let harness = |why: &str| Failure::Harness { name: name.to_owned(), why: why.to_owned() };
    let at = expanded.find("#[rustc_proc_macro_decls]").ok_or_else(|| harness("no `#[rustc_proc_macro_decls]` static"))?;
    let list = &expanded[at..];
    let start = list.find("&[").ok_or_else(|| harness("the decls static has no list"))?;
    let end = list[start..].find("];").ok_or_else(|| harness("the decls list does not end"))? + start;
    let list = &list[start + 2..end];
    let fns: Vec<&str> = list.split("Client::expand").skip(1).collect();
    let mut out = Vec::with_capacity(fns.len());
    let lines: Vec<&str> = expanded.lines().collect();
    for entry in fns {
        let open = entry.find('(').ok_or_else(|| harness("an entry has no `(`"))?;
        let close = entry.find(')').ok_or_else(|| harness("an entry has no `)`"))?;
        let function = entry[open + 1..close].trim();
        let function = function.rsplit("::").next().unwrap_or(function);
        let head = format!("pub fn {function}(");
        let line = lines
            .iter()
            .position(|line| line.starts_with(&head))
            .ok_or_else(|| harness(&format!("no `pub fn {function}` at the crate's root")))?;
        // The function's attributes and docs: the lines above it back to a blank line or one
        // that starts an item or ends one at the crate's root. The printer wraps a long
        // attribute over lines and puts `///` docs among them.
        let mut first = line;
        while first > 0 {
            let above = lines[first - 1];
            let ends_item = above.starts_with('}')
                || ["pub ", "fn ", "use ", "mod ", "const ", "static ", "impl ", "extern ", "struct ", "enum "].iter().any(|it| above.starts_with(it));
            if above.trim().is_empty() || ends_item {
                break;
            }
            first -= 1;
        }
        let region = lines[first..line].join("\n");
        // The last proc-macro attribute in it, read to its own `]`.
        let marks = ["#[proc_macro_derive(", "#[proc_macro_attribute]", "#[proc_macro]"];
        let last = marks.iter().filter_map(|mark| region.rfind(mark).map(|at| (at, *mark))).max_by_key(|it| it.0);
        let mut found = None;
        if let Some((at, mark)) = last {
            if mark == "#[proc_macro_attribute]" {
                found = Some(Macro { kind: "attr", name: function.to_owned(), helpers: Vec::new() });
            } else if mark == "#[proc_macro]" {
                found = Some(Macro { kind: "bang", name: function.to_owned(), helpers: Vec::new() });
            } else {
                let rest = &region[at + mark.len()..];
                let mut depth = 1usize;
                let close = rest
                    .char_indices()
                    .find(|(_, c)| {
                        match c {
                            '(' => depth += 1,
                            ')' => depth -= 1,
                            _ => {}
                        }
                        depth == 0
                    })
                    .map(|it| it.0)
                    .ok_or_else(|| harness("a derive attribute whose `(` does not close"))?;
                let inner: String = rest[..close].split_whitespace().collect::<Vec<_>>().join(" ");
                let inner = inner.as_str();
                let (trait_name, helpers) = match inner.split_once(',') {
                    None => (inner.trim(), Vec::new()),
                    Some((trait_name, rest)) => {
                        let rest = rest.trim();
                        let helpers = rest
                            .strip_prefix("attributes(")
                            .and_then(|it| it.strip_suffix(')'))
                            .ok_or_else(|| harness("a derive's second argument that is not `attributes(..)`"))?;
                        (trait_name.trim(), helpers.split(',').map(|it| it.trim().to_owned()).filter(|it| !it.is_empty()).collect())
                    }
                };
                found = Some(Macro { kind: "derive", name: trait_name.to_owned(), helpers });
            }
        }
        let found = found.ok_or_else(|| harness(&format!("`pub fn {function}` has no proc-macro attribute above it")))?;
        let two = entry.starts_with('2');
        if two != (found.kind == "attr") {
            return Err(harness(&format!("`{function}` is listed as expand{} but is a {}", if two { 2 } else { 1 }, found.kind)));
        }
        out.push(found);
    }
    Ok(out)
}

/// The builder's run as cargo's `RUSTC_WRAPPER`, for every rustc cargo starts: the macro crate
/// named by `BLAZE_PM_WRAP` gets the `proc-macro` crate type back and the shim linked in;
/// everything else runs as cargo asked.
fn wrap(args: Vec<OsString>, macro_crate: &str) -> ! {
    let (rustc, rest) = args.split_first().expect("cargo passes rustc first");
    let mut rest: Vec<OsString> = rest.to_vec();
    let text: Vec<String> = rest.iter().map(|it| it.to_string_lossy().into_owned()).collect();
    let named = text.windows(2).any(|pair| pair[0] == "--crate-name" && pair[1] == macro_crate);
    let lib = text.windows(2).any(|pair| pair[0] == "--crate-type" && pair[1] == "cdylib");
    let target = text.iter().any(|it| it == "--target" || it.starts_with("--target="));
    let mut command = toolchain_command(rustc);
    let shim = text.windows(2).any(|pair| pair[0] == "--crate-name" && pair[1] == "blaze_pm_shim");
    if shim && target {
        // The shim as one object as well as its library: the macro crate links the object,
        // whole, so its exports stay and its `getenv` comes before wasi-libc's.
        let status = command.args(&rest).args(["--emit=obj", "-Ccodegen-units=1"]).status().map(|it| it.code().unwrap_or(1)).unwrap_or(1);
        blaze_script::exit(status);
    }
    if !(named && lib && target) {
        let status = command.args(&rest).status().map(|it| it.code().unwrap_or(1)).unwrap_or(1);
        blaze_script::exit(status);
    }
    // The crate's own folder, not the member's that cargo names: `include_dir!` and
    // `include_str!` paths under `CARGO_MANIFEST_DIR` are its files.
    if let Some(dir) = std::env::var_os("BLAZE_PM_MANIFEST_DIR") {
        command.env("CARGO_MANIFEST_DIR", dir);
    }
    let mut object = None;
    for (at, arg) in text.iter().enumerate() {
        if arg == "cdylib" && at > 0 && text[at - 1] == "--crate-type" {
            rest[at] = "proc-macro".into();
        }
        // The shim's object, which its build made beside its library: linked whole.
        if at > 0
            && text[at - 1] == "--extern"
            && let Some(path) = arg.strip_prefix("blaze_pm_shim=")
            && let Some(stem) = path.strip_suffix(".rlib").or_else(|| path.strip_suffix(".rmeta"))
        {
            object = Some(format!("{}.o", stem.replace("/libblaze_pm_shim", "/blaze_pm_shim")));
        }
    }
    let Some(object) = object else {
        eprintln!("{SCRIPT}: cargo passed {macro_crate} no `--extern blaze_pm_shim=<library>`");
        blaze_script::exit(1);
    };
    rest.push(format!("-Clink-arg={object}").into());
    let out_dir = text.windows(2).find(|pair| pair[0] == "--out-dir").map(|pair| pair[1].clone()).unwrap_or_default();
    let extra = text.iter().find_map(|it| it.strip_prefix("extra-filename=")).unwrap_or("").to_owned();
    // The custom target has no self-contained folder of its own in the sysroot: wasi-libc and
    // its reactor start file are `wasm32-wasip1`'s, named here.
    let sysroot = Path::new(rustc).parent().and_then(Path::parent).unwrap_or(Path::new("/"));
    let contained = sysroot.join("lib/rustlib/wasm32-wasip1/lib/self-contained");
    for arg in [
        "-Zunstable-options".to_owned(),
        "-Zwasm-proc-macros".to_owned(),
        "--extern".to_owned(),
        "proc_macro".to_owned(),
        "-Clink-self-contained=no".to_owned(),
        format!("-Lnative={}", contained.display()),
        format!("-Clink-arg={}", contained.join("crt1-reactor.o").display()),
        // The shim's `getenv` (`__wrap_getenv`) takes every call of libc's.
        "-Clink-arg=--wrap=getenv".to_owned(),
        "-Clink-arg=--export=blaze_proc_macro_alloc".to_owned(),
        "-Clink-arg=--export=blaze_proc_macro_expand".to_owned(),
    ] {
        rest.push(arg.into());
    }
    let status = command.args(&rest).status().map(|it| it.code().unwrap_or(1)).unwrap_or(1);
    if status != 0 {
        blaze_script::exit(status);
    }
    // The macros' names and kinds: the expanded crate, asked of the same rustc with the same
    // arguments but the output.
    let mut expand: Vec<OsString> = Vec::with_capacity(rest.len());
    let mut skip = false;
    for (at, arg) in rest.iter().enumerate() {
        if skip {
            skip = false;
            continue;
        }
        let arg_text = arg.to_string_lossy();
        if arg_text == "--out-dir" || arg_text == "-o" {
            skip = true;
            continue;
        }
        if arg_text.starts_with("--emit") || arg_text.starts_with("--json") || arg_text.starts_with("--error-format") {
            continue;
        }
        let _ = at;
        expand.push(arg.clone());
    }
    expand.push("-Zunpretty=expanded".into());
    let mut expand_command = toolchain_command(rustc);
    if let Some(dir) = std::env::var_os("BLAZE_PM_MANIFEST_DIR") {
        expand_command.env("CARGO_MANIFEST_DIR", dir);
    }
    let expanded = expand_command.args(&expand).stderr(Stdio::inherit()).output();
    let expanded = match expanded {
        Ok(out) if out.status.success() => out.stdout,
        Ok(out) => {
            eprintln!("{SCRIPT}: the expanded crate of {macro_crate} was not printed: exit {:?}", out.status.code());
            blaze_script::exit(1);
        }
        Err(error) => {
            eprintln!("{SCRIPT}: rustc did not start for the expanded crate of {macro_crate}: {error}");
            blaze_script::exit(1);
        }
    };
    let dir = PathBuf::from(&out_dir);
    let wasm = [format!("{macro_crate}{extra}.wasm"), format!("lib{macro_crate}{extra}.wasm")]
        .into_iter()
        .map(|it| dir.join(it))
        .find(|it| it.exists());
    let Some(wasm) = wasm else {
        eprintln!("{SCRIPT}: rustc made no {macro_crate}{extra}.wasm in {out_dir}");
        blaze_script::exit(1);
    };
    let expanded_path = dir.join(format!("{macro_crate}{extra}.expanded.rs"));
    let said = format!("{}\n{}\n", wasm.display(), expanded_path.display());
    let Some(note) = std::env::var_os("BLAZE_PM_NOTE") else {
        eprintln!("{SCRIPT}: the wrapper was started with no BLAZE_PM_NOTE");
        blaze_script::exit(1);
    };
    if std::fs::write(&expanded_path, &expanded).is_err() || std::fs::write(note, said).is_err() {
        eprintln!("{SCRIPT}: could not write beside {}", wasm.display());
        blaze_script::exit(1);
    }
    blaze_script::exit(0)
}

/// What one crate's build made.
struct Built {
    key: String,
    wasm_sha256: String,
    size: usize,
    macros: Vec<Macro>,
    assumed_default: bool,
    fresh: bool,
}

/// Everything a build shares: the toolchain, the paths, the deadline, the environment.
struct Plan<'a> {
    args: &'a Args,
    /// cargo reads only its cache: no network (`CARGO_NET_OFFLINE`); the native build-script
    /// mode asks it.
    offline: bool,
    /// Each crate's origin, in the order of `args.crates`.
    origins: Vec<Result<Origin, Failure>>,
    tool: Toolchain,
    work: PathBuf,
    lock: String,
    target_json: PathBuf,
    target_digest: String,
    shim_digest: String,
    shim_text: String,
    home: String,
    path: String,
    me: OsString,
}

impl Plan<'_> {
    /// The folder of a path package's manifest as the fetch depends on it.
    fn path_member(&self, wanted: &Wanted) -> PathBuf {
        self.work.join("path").join(format!("{}-{}", wanted.name, wanted.version))
    }

    /// The manifest of the `at`th crate as written: the vendored one, or a path package's with
    /// what it inherits written in.
    fn manifest_of(&self, at: usize) -> Result<String, Failure> {
        let wanted = &self.args.crates[at];
        match self.origin(at)? {
            Origin::Registry { .. } => read(&self.work.join("vendor").join(format!("{}-{}", wanted.name, wanted.version)).join("Cargo.toml")),
            Origin::Path { manifest, .. } | Origin::Git { manifest, .. } => Ok(manifest.clone()),
        }
    }

    /// The `at`th crate's origin, or why it has none, named.
    fn origin(&self, at: usize) -> Result<&Origin, Failure> {
        let wanted = &self.args.crates[at];
        self.origins[at].as_ref().map_err(|failure| Failure::NoModule {
            name: wanted.name.clone(),
            version: wanted.version.clone(),
            why: format!("its sources were not found: {failure}"),
        })
    }

    /// The crates whose sources were found: the ones the fetch and the builds take.
    fn located(&self) -> impl Iterator<Item = (usize, &Wanted, &Origin)> {
        self.args.crates.iter().zip(&self.origins).enumerate().filter_map(|(at, (wanted, origin))| Some((at, wanted, origin.as_ref().ok()?)))
    }

    /// The environment of every cargo this tool starts: nothing inherited but `HOME` and `PATH`.
    fn env(&self, cargo_home: &Path) -> Vec<(String, OsString)> {
        let mut env = vec![
            ("HOME".to_owned(), self.home.clone().into()),
            ("PATH".to_owned(), self.path.clone().into()),
            ("CARGO_HOME".to_owned(), cargo_home.as_os_str().to_owned()),
            ("RUSTC".to_owned(), self.tool.rustc().into_os_string()),
            ("TMPDIR".to_owned(), self.work.join("tmp").into_os_string()),
            ("LANG".to_owned(), "C.UTF-8".into()),
        ];
        if self.offline {
            env.push(("CARGO_NET_OFFLINE".to_owned(), "true".into()));
        }
        env
    }

    /// Step one: `cargo vendor` of every crate asked for, with its dependencies as the lock
    /// pins them, and of std's own dependencies. The network; no code of theirs runs.
    fn fetch(&self, alloc: blaze_own::Alloc<'_>) -> Result<(), Failure> {
        let dir = self.work.join("fetch");
        // Each macro crate with the features asked for and `more`: an optional dependency
        // deeper down that a feature turns on (`alloy-sol-macro/json` -> `alloy-json-abi`) is
        // fetched only then.
        let manifest_with = |more: &[Vec<String>]| {
            let mut manifest = String::from(
                "[package]\nname = \"blaze-proc-macro-fetch\"\nversion = \"0.0.0\"\nedition = \"2021\"\npublish = false\n\n[lib]\npath = \"lib.rs\"\n\n[dependencies]\n",
            );
            for (at, wanted, origin) in self.located() {
                let mut features = wanted.features.clone().unwrap_or_else(|| vec!["default".to_owned()]);
                features.extend(more.get(at).into_iter().flatten().cloned());
                features.sort();
                features.dedup();
                let default = features.iter().any(|it| it == "default");
                let named: Vec<&String> = features.iter().filter(|it| *it != "default").collect();
                let from = match origin {
                    Origin::Registry { .. } => format!("package = {:?}, version = \"={}\"", wanted.name, wanted.version),
                    Origin::Path { .. } | Origin::Git { .. } => {
                        format!("package = {:?}, path = {:?}", wanted.name, self.path_member(wanted).display().to_string())
                    }
                };
                manifest.push_str(&format!("m{at} = {{ {from}, default-features = {default}, features = {named:?} }}\n"));
            }
            manifest
        };
        // A path or git package is a dependency by its manifest as the build's member has it,
        // so the fetch vendors what it depends on.
        for (_, wanted, origin) in self.located() {
            if let Origin::Path { dir, manifest, .. } | Origin::Git { dir, manifest, .. } = origin {
                // The member's `../shim`, as in the build's workspace.
                let shim = self.work.join("path/shim");
                write(&shim.join("Cargo.toml"), SHIM_MANIFEST)?;
                write(&shim.join("shim.rs"), &self.shim_text)?;
                let path = dir.join("Cargo.toml");
                let features = wanted.features.clone().unwrap_or_else(|| vec!["default".to_owned()]);
                write(&self.path_member(wanted).join("Cargo.toml"), member_manifest(manifest, dir, &path, &features, alloc)?)?;
            }
        }
        let mut manifest = manifest_with(&[]);
        write(&dir.join("lib.rs"), "")?;
        write(&dir.join("Cargo.lock"), &self.lock)?;
        let vendor = self.work.join("vendor");
        let argv: Vec<OsString> = vec![
            self.tool.cargo(),
            "vendor".into(),
            "--versioned-dirs".into(),
            "--sync".into(),
            self.tool.library().join("Cargo.toml").into_os_string(),
            vendor.clone().into_os_string(),
        ];
        let home = PathBuf::from(&self.home).join(".cargo");
        write(&dir.join("Cargo.toml"), format!("{manifest}\n[workspace]\n"))?;
        let ran = run(&argv, &dir, &self.env(&home), Duration::from_secs(600), "cargo vendor")?;
        ran_ok(ran, "cargo vendor")?;
        // Cargo resolves every feature and optional dependency of a workspace member, active
        // or not, and the build's members are the macro crates: what those turn on is fetched
        // too, in a second pass, though only the requested features build.
        let mut every = vec![Vec::new(); self.args.crates.len()];
        let located: Vec<usize> = self.located().map(|it| it.0).collect();
        for &at in &located {
            every[at] = feature_names(&self.manifest_of(at)?);
        }
        manifest = manifest_with(&every);
        let mut optional = 0;
        for &at in &located {
            for (package, version) in optional_dependencies(&self.manifest_of(at)?) {
                manifest.push_str(&format!("o{optional} = {{ package = {package:?}, version = {version:?}, default-features = false }}\n"));
                optional += 1;
            }
        }
        if optional == 0 && every.iter().all(Vec::is_empty) {
            return Ok(());
        }
        write(&dir.join("Cargo.toml"), format!("{manifest}\n[workspace]\n"))?;
        let ran = run(&argv, &dir, &self.env(&home), Duration::from_secs(600), "cargo vendor (optional dependencies)")?;
        ran_ok(ran, "cargo vendor (optional dependencies)").map(drop)
    }

    /// Step two for one crate: its workspace, its sandboxed offline build, its record.
    fn build(&self, at: usize, alloc: blaze_own::Alloc<'_>) -> Result<Built, Failure> {
        let (wanted, origin) = (&self.args.crates[at], self.origin(at)?);
        let assumed_default = wanted.features.is_none();
        let mut features: Vec<String> = wanted.features.clone().unwrap_or_else(|| vec!["default".to_owned()]);
        features.sort();
        features.dedup();
        let identity = format!(
            // The same text as crates/blaze-proc-macro/src/module.rs `identity` (form 4: the
            // profile is an input; this builder sets `debug-assertions = true` for the macro
            // crate and its dependencies, `false` for std's own crates ([`STD_RELEASE`]), and
            // `-Zub-checks=no` for every crate, below).
            "blaze proc-macro module 4\nname {}\nversion {}\n{}\ndeps {}\nfeatures {}\ntoolchain {}\ntarget {}\nshim {}\nprofile debug-assertions std-release ub-checks-off\nbuilder bootstrap\n",
            wanted.name,
            wanted.version,
            origin.identity_line(),
            lock_closure(&self.lock, &wanted.name, &wanted.version)?,
            features.join(","),
            self.tool.name,
            self.target_digest,
            self.shim_digest,
        );
        let key = sha256(identity.as_bytes());

        let source = match origin {
            Origin::Registry { .. } => self.work.join("vendor").join(format!("{}-{}", wanted.name, wanted.version)),
            Origin::Path { dir, .. } | Origin::Git { dir, .. } => dir.clone(),
        };
        let original_path = source.join("Cargo.toml");
        let original = self.manifest_of(at)?;
        let ws = self.work.join("ws").join(format!("{}-{}", wanted.name, wanted.version));
        // The macro crate and its dependencies see `cfg(debug_assertions)` as cargo's dev
        // profile gives it (form 3); std's own crates, built here by `-Zbuild-std` under this
        // workspace's profile, get a release override each ([`STD_RELEASE`]).
        let mut manifest =
            String::from("[workspace]\nmembers = [\"macro\", \"shim\"]\nresolver = \"2\"\n\n[profile.release]\ndebug = false\ndebug-assertions = true\nincremental = false\npanic = \"abort\"\ncodegen-units = 16\n");
        for package in STD_RELEASE {
            manifest.push_str(&format!("\n[profile.release.package.{package}]\ndebug-assertions = false\n"));
        }
        write(&ws.join("Cargo.toml"), &manifest)?;
        write(&ws.join("macro/Cargo.toml"), member_manifest(&original, &source, &original_path, &features, alloc)?)?;
        write(&ws.join("shim/Cargo.toml"), SHIM_MANIFEST)?;
        write(&ws.join("shim/shim.rs"), &self.shim_text)?;
        write(&ws.join("Cargo.lock"), member_lock(&read(&self.work.join("fetch/Cargo.lock"))?, &wanted.name, &wanted.version))?;
        let vendor = self.work.join("vendor");
        write(
            &ws.join(".cargo/config.toml"),
            format!(
                "[source.crates-io]\nreplace-with = \"vendored\"\n\n[source.vendored]\ndirectory = {:?}\n",
                vendor.display().to_string()
            ),
        )?;
        std::fs::create_dir_all(self.work.join("tmp")).map_err(io(&self.work))?;

        // The library's crate name: its `[lib] name` where the manifest gives one
        // (`ethereum_ssz_derive`'s is `ssz_derive`).
        let lib = table_value(&original, "lib", "name").map_or_else(|| crate_name(&wanted.name), |it| it.trim_matches('"').to_owned());
        let mut argv: Vec<OsString> = vec![
            self.tool.cargo(),
            "build".into(),
            "--release".into(),
            "--offline".into(),
            "--workspace".into(),
            // Every artifact named with its package, so the build is checked against the lock.
            "--message-format=json".into(),
            "-Zbuild-std=std,panic_abort,proc_macro".into(),
            "-Zjson-target-spec".into(),
            "--target".into(),
            self.target_json.clone().into_os_string(),
        ];
        // `default` is the crate's default set, which is empty where its manifest names none:
        // cargo's own default features, never a feature named `default`.
        if !features.iter().any(|it| it == "default") {
            argv.push("--no-default-features".into());
        }
        for feature in features.iter().filter(|it| *it != "default") {
            argv.push("--features".into());
            argv.push(format!("{}/{feature}", wanted.name).into());
        }
        let argv = match sandboxed(argv.clone(), &self.work) {
            Ok(argv) => argv,
            Err(why) if self.args.unsandboxed_build_scripts => {
                println!("{SCRIPT}: UNSANDBOXED: build scripts of {} {} run with no sandbox ({why}), as the input asked", wanted.name, wanted.version);
                argv
            }
            Err(why) => return Err(Failure::NoSandbox { why }),
        };
        let mut env = self.env(&self.work.join("cargo-home"));
        let mut flags = vec![
            format!("--remap-path-prefix={}=/build", self.work.display()),
            format!("--remap-path-prefix={}=/rustc/src", self.tool.sysroot.join("lib/rustlib/src/rust").display()),
            "-Cdebuginfo=0".to_owned(),
            // No UB checks in any crate of the module. A check is decided where its function is
            // monomorphized (core intrinsics `ub_checks`: "evaluation is delayed until
            // monomorphization" in a crate with UB checks on), so std's generic and `#[inline]`
            // code inlined into the macro crate checks whenever the MACRO crate has them on, and
            // `-Zub-checks` defaults to `-Cdebug-assertions` (the pinned nightly's option text:
            // "emit runtime checks for Undefined Behavior (default: -Cdebug-assertions)"). So
            // the flag is on every crate, not only std's; `cfg(debug_assertions)` is untouched.
            // A program with no UB behaves the same either way (a check only panics on UB).
            "-Zub-checks=no".to_owned(),
        ];
        // A path package's sources are in the checkout, wherever it is: named by the package.
        if let Origin::Path { dir, .. } = origin {
            flags.push(format!("--remap-path-prefix={}=/build/path/{}-{}", dir.display(), wanted.name, wanted.version));
        }
        env.push(("CARGO_ENCODED_RUSTFLAGS".to_owned(), flags.join("\u{1f}").into()));
        env.push(("CARGO_TARGET_DIR".to_owned(), self.work.join("target").into_os_string()));
        env.push(("RUSTC_WRAPPER".to_owned(), self.me.clone()));
        env.push(("BLAZE_PM_WRAP".to_owned(), lib.clone().into()));
        env.push(("BLAZE_PM_MANIFEST_DIR".to_owned(), source.clone().into_os_string()));
        // Where the wrapper notes the module it made: its own folder is the toolchain's choice.
        let note = self.work.join("target").join(format!("{lib}.blaze-module"));
        let _ = std::fs::remove_file(&note);
        env.push(("BLAZE_PM_NOTE".to_owned(), note.clone().into()));
        let what = format!("the build of {} {}", wanted.name, wanted.version);
        let mut ran = run(&argv, &ws, &env, Duration::from_secs(self.args.build_seconds), &what)?;
        let (compiled, readable) = artifacts_of(&String::from_utf8_lossy(&ran.output));
        ran.output = readable.clone().into_bytes();
        let log = self.work.join("logs").join(format!("{}-{}.txt", wanted.name, wanted.version));
        write(&log, &readable)?;
        write(&self.work.join("logs").join(format!("{}-{}.compiled", wanted.name, wanted.version)), &compiled)?;
        let what = format!("{what} (its whole output: {})", log.display());
        ran_ok(ran, &what)?;
        let closure = closure_lines(&self.lock, &wanted.name, &wanted.version)?;
        let library = self.tool.sysroot.join("lib/rustlib/src/rust/library");
        let std_lock = read(&library.join("Cargo.lock"))?;
        check_compiled(&compiled, &closure, &std_lock, &library, &wanted.name, &wanted.version)?;

        let said = read(&note).map_err(|failure| Failure::NoModule {
            name: wanted.name.clone(),
            version: wanted.version.clone(),
            why: failure.to_string(),
        })?;
        let mut said = said.lines();
        let (Some(wasm_path), Some(expanded_path)) = (said.next(), said.next()) else {
            return Err(Failure::NoModule { name: wanted.name.clone(), version: wanted.version.clone(), why: "the wrapper's note is cut short".to_owned() });
        };
        let wasm = std::fs::read(wasm_path).map_err(io(Path::new(wasm_path)))?;
        let macros = macros_of(&read(Path::new(expanded_path))?, &wanted.name)?;
        let decls = decls_export(&wasm).ok_or_else(|| Failure::NoModule {
            name: wanted.name.clone(),
            version: wanted.version.clone(),
            why: "the module exports no `__rustc_proc_macro_decls_*` global".to_owned(),
        })?;
        let wasm_sha256 = sha256(&wasm);

        let record = self.args.store.join("proc-macro/modules").join(&key);
        let held = record.join("wasm-sha256");
        let fresh = if held.exists() {
            let held = read(&held)?.trim().to_owned();
            if held != wasm_sha256 {
                return Err(Failure::Conflict { key, held, made: wasm_sha256 });
            }
            false
        } else {
            let mut listed = String::new();
            for (at, m) in macros.iter().enumerate() {
                listed.push_str(&format!("{at} {} {} {}\n", m.kind, m.name, m.helpers.join(",")));
            }
            // Written into a folder of its own, then renamed into place: a reader sees a whole
            // record or none.
            let staging = self.args.store.join("proc-macro/staging").join(&key);
            let _ = std::fs::remove_dir_all(&staging);
            write(&staging.join("identity"), &identity)?;
            write(&staging.join("module.wasm"), &wasm)?;
            write(&staging.join("macros"), listed)?;
            write(&staging.join("decls"), format!("{decls}\n"))?;
            write(&staging.join("wasm-sha256"), format!("{wasm_sha256}\n"))?;
            std::fs::create_dir_all(record.parent().expect("a record has a parent")).map_err(io(&record))?;
            std::fs::rename(&staging, &record).map_err(io(&record))?;
            true
        };
        Ok(Built { key, wasm_sha256, size: wasm.len(), macros, assumed_default, fresh })
    }
}

/// The name of the module's exported `__rustc_proc_macro_decls_*` global, from its export
/// section.
fn decls_export(wasm: &[u8]) -> Option<String> {
    fn leb(bytes: &[u8], at: &mut usize) -> Option<u64> {
        let mut value = 0u64;
        let mut shift = 0;
        loop {
            let byte = *bytes.get(*at)?;
            *at += 1;
            value |= u64::from(byte & 0x7f) << shift;
            if byte < 0x80 {
                return Some(value);
            }
            shift += 7;
            if shift > 63 {
                return None;
            }
        }
    }
    let mut at = 8;
    while at < wasm.len() {
        let id = wasm[at];
        at += 1;
        let size = usize::try_from(leb(wasm, &mut at)?).ok()?;
        let end = at.checked_add(size)?;
        if id == 7 {
            let mut p = at;
            let count = leb(wasm, &mut p)?;
            for _ in 0..count {
                let len = usize::try_from(leb(wasm, &mut p)?).ok()?;
                let name = std::str::from_utf8(wasm.get(p..p + len)?).ok()?;
                p += len;
                let kind = *wasm.get(p)?;
                p += 1;
                leb(wasm, &mut p)?;
                if kind == 3 && name.starts_with("__rustc_proc_macro_decls_") {
                    return Some(name.to_owned());
                }
            }
            return None;
        }
        at = end;
    }
    None
}

fn main_with(args: &Args, heap: &blaze_own::Heap) -> i32 {
    let env = |name: &str| std::env::var(name).unwrap_or_default();
    let home = env("HOME");
    let fail = |failure: Failure| -> ! { blaze_script::fail(SCRIPT, &failure.to_string()) };
    let canon = |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|error| fail(Failure::Io { path: path.to_owned(), error }));
    std::fs::create_dir_all(&args.work).unwrap_or_else(|error| fail(Failure::Io { path: args.work.clone(), error }));
    std::fs::create_dir_all(&args.store).unwrap_or_else(|error| fail(Failure::Io { path: args.store.clone(), error }));
    let work = canon(&args.work);
    let tree = canon(&args.tree);
    let tool = toolchain(&tree, &home).unwrap_or_else(|failure| fail(failure));
    // The host's server speaks the bridge of the tree's `library/proc_macro`
    // (crates/blaze-proc-macro/src/protocol.rs): a toolchain whose bridge differs makes modules
    // it would misread.
    let bridge = tool.library().join("proc_macro/src/bridge");
    for entry in std::fs::read_dir(&bridge).unwrap_or_else(|error| fail(Failure::Io { path: bridge.clone(), error })).flatten() {
        let ours = tree.join("library/proc_macro/src/bridge").join(entry.file_name());
        let theirs = std::fs::read(entry.path()).unwrap_or_else(|error| fail(Failure::Io { path: entry.path(), error }));
        if std::fs::read(&ours).ok().as_deref() != Some(&theirs[..]) {
            fail(Failure::Manifest {
                path: entry.path(),
                why: format!("differs from {}: the toolchain's proc_macro bridge is not the one blaze's server speaks", ours.display()),
            });
        }
    }
    let target_source = tree.join("crates/blaze-proc-macro/guest/wasm32-wasip1-macro.json");
    let target_bytes = std::fs::read(&target_source).unwrap_or_else(|error| fail(Failure::Io { path: target_source.clone(), error }));
    // The target file's name is the target's name; its folder is under `work`, so it is remapped.
    let target_json = work.join("wasm32-wasip1-macro.json");
    write(&target_json, &target_bytes).unwrap_or_else(|failure| fail(failure));
    let shim_path = tree.join("crates/blaze-proc-macro/guest/shim.rs");
    let shim_text = read(&shim_path).unwrap_or_else(|failure| fail(failure));
    let lock = read(&args.checkout.join("Cargo.lock")).unwrap_or_else(|failure| fail(failure));
    let me = std::env::current_exe().unwrap_or_else(|error| fail(Failure::Io { path: PathBuf::from("current_exe"), error })).into_os_string();
    let members = Members::new(&args.checkout);
    let origins: Vec<Result<Origin, Failure>> = args.crates.iter().map(|wanted| Origin::of(args, &lock, &members, wanted, heap.alloc())).collect();
    let plan = Plan {
        args,
        offline: false,
        origins,
        work,
        lock,
        target_digest: sha256(&target_bytes),
        shim_digest: sha256(shim_text.as_bytes()),
        target_json,
        shim_text,
        home,
        path: env("PATH"),
        me,
        tool,
    };
    println!("{SCRIPT}: toolchain {} at {}", plan.tool.name, plan.tool.sysroot.display());
    let started = Instant::now();
    // A macro crate's member starts from its whole manifest, with optional dependencies outside
    // the requested feature closure removed before Cargo resolves it.
    let mut refused = vec![false; args.crates.len()];
    match &args.vendor_from {
        Some(store) => {
            for (at, wanted) in args.crates.iter().enumerate() {
                if let Err(failure) = plan.vendor_from_one(store, at, |manifest| manifest.to_owned(), heap.alloc()) {
                    refused[at] = true;
                    println!("{} {}: FAILED: {failure}", wanted.name, wanted.version);
                }
            }
            Ok(())
        }
        None => plan.fetch(heap.alloc()),
    }
    .unwrap_or_else(|failure| fail(failure));
    // A run where every store fetch was refused has no vendor directory to correct. The
    // per-crate refusals remain the result, while a successful crate still gets the shared
    // vendor tree checked before any build starts.
    if args.vendor_from.is_none() || refused.iter().any(|refused| !refused) {
        correct_vendored(&plan.work.join("vendor")).unwrap_or_else(|failure| fail(failure));
    }
    println!("{SCRIPT}: fetched in {:.1}s", started.elapsed().as_secs_f64());
    let mut failed = 0;
    for (at, wanted) in args.crates.iter().enumerate() {
        if refused[at] {
            failed += 1;
            continue;
        }
        let started = Instant::now();
        match plan.build(at, heap.alloc()) {
            Ok(built) => {
                let macros: Vec<String> = built
                    .macros
                    .iter()
                    .map(|m| if m.helpers.is_empty() { format!("{}:{}", m.kind, m.name) } else { format!("{}:{}[{}]", m.kind, m.name, m.helpers.join(",")) })
                    .collect();
                println!(
                    "{} {}: module {} wasm-sha256 {} {} bytes{}{} in {:.1}s; macros {}",
                    wanted.name,
                    wanted.version,
                    built.key,
                    built.wasm_sha256,
                    built.size,
                    if built.fresh { "" } else { " (already in the store, equal)" },
                    if built.assumed_default { "; features assumed default" } else { "" },
                    started.elapsed().as_secs_f64(),
                    macros.join(" "),
                );
            }
            Err(failure) => {
                failed += 1;
                println!("{} {}: FAILED: {failure}", wanted.name, wanted.version);
            }
        }
    }
    i32::from(failed > 0)
}

fn main() {
    if let Ok(macro_crate) = std::env::var("BLAZE_PM_WRAP") {
        let args: Vec<OsString> = std::env::args_os().skip(1).collect();
        wrap(args, &macro_crate);
    }
    let heap = blaze_own::Heap::process();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [one] if build_scripts::asked(one) => blaze_script::exit(build_scripts::main_with(one, heap)),
        [one] => {
            let status = main_with(&input::parse::<Args>(one, SCRIPT), heap);
            blaze_script::exit(status);
        }
        _ => {
            eprintln!("usage: {SCRIPT} '<json>'\n\n{DOC}");
            blaze_script::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCK: &str = "version = 4\n\n[[package]]\nname = \"data-encoding-macro-internal\"\nversion = \"0.1.19\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"c5\"\ndependencies = [\n \"syn 3.0.6\",\n]\n\n[[package]]\nname = \"syn\"\nversion = \"1.0.109\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"s1\"\n\n[[package]]\nname = \"syn\"\nversion = \"3.0.6\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"s3\"\n";
    const STD_LOCK: &str = "[[package]]\nname = \"hashbrown\"\nversion = \"0.17.1\"\n";

    fn checked(compiled: &str) -> Result<usize, Failure> {
        let closure = closure_lines(LOCK, "data-encoding-macro-internal", "0.1.19")?;
        check_compiled(compiled, &closure, STD_LOCK, Path::new("/sysroot/library"), "data-encoding-macro-internal", "0.1.19")
    }

    /// evm2's root writes an inherited dependency as an inline table over lines.
    #[test]
    fn an_inline_table_over_lines_is_read_whole() {
        let root = "[workspace.dependencies]\ncorosensei = { version = \"0.3.4\", default-features = false, features = [\n    \"default-stack\",\n    \"unwind\",\n] }\ncrossbeam-channel = \"0.5\"\n";
        let value = table_value(root, "workspace.dependencies", "corosensei");
        assert_eq!(value, Some("{ version = \"0.3.4\", default-features = false, features = [\n    \"default-stack\",\n    \"unwind\",\n] }"));
        assert_eq!(table_value(root, "workspace.dependencies", "crossbeam-channel"), Some("\"0.5\""));
        let member = "[package]\nname = \"m\"\n\n[dependencies]\ncorosensei = { workspace = true, features = [\"asm-unwind\"] }\n";
        let made = match inherit(member, root, Path::new("/r"), Path::new("/r/m/Cargo.toml")) {
            Ok(made) => made,
            Err(failure) => panic!("refused: {failure}"),
        };
        assert!(made.contains("\"unwind\""), "{made}");
        assert!(made.contains("\"asm-unwind\"]"), "{made}");
    }

    const OPTIONAL_MEMBER: &str = "[package]\nname = \"member\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\nproc-macro = true\npath = \"src/lib.rs\"\n\n[dependencies]\na = \"1\"\nb = { version = \"1\", optional = true }\nc = { version = \"1\", optional = true }\n\n[features]\ndefault = [\"x\"]\nx = [\"dep:b\", \"c?/std\"]\ny = [\"c/std\"]\n";

    fn generated_member(manifest: &str, features: &[&str]) -> String {
        let requested = features.iter().map(|feature| (*feature).to_owned()).collect::<Vec<_>>();
        let alloc = blaze_own::testing::heap().alloc();
        let path = Path::new("/crate/Cargo.toml");
        let made = member_manifest(manifest, Path::new("/crate"), path, &requested, alloc).unwrap_or_else(|failure| panic!("{failure}"));
        member_dependencies::parse(&made, path, alloc).unwrap_or_else(|failure| panic!("generated member: {failure}\n{made}"));
        made
    }

    fn optional_member(features: &[&str]) -> String {
        generated_member(OPTIONAL_MEMBER, features)
    }

    fn feature_line<'m>(manifest: &'m str, feature: &str) -> &'m str {
        manifest.lines().find(|line| line.starts_with(&format!("\"{feature}\" ="))).expect("the feature line")
    }

    /// Cargo's feature closure keeps `b` through `dep:b`, leaves the weak `c?/std` disabled,
    /// and rewrites every feature line after the removed dependency is omitted.
    #[test]
    fn member_manifest_drops_optional_dependencies_not_enabled_by_features() {
        let default = optional_member(&["default"]);
        assert!(default.contains("b = { version = \"1\", optional = true }"));
        assert!(!default.contains("c = { version = \"1\", optional = true }"));
        assert_eq!(feature_line(&default, "x"), "\"x\" = [\"dep:b\"]");
        assert_eq!(feature_line(&default, "y"), "\"y\" = []");

        let none = optional_member(&[]);
        assert!(!none.contains("b = { version = \"1\", optional = true }"));
        assert!(!none.contains("c = { version = \"1\", optional = true }"));
        assert_eq!(feature_line(&none, "x"), "\"x\" = []");
        assert_eq!(feature_line(&none, "y"), "\"y\" = []");

        let y = optional_member(&["y"]);
        assert!(!y.contains("b = { version = \"1\", optional = true }"));
        assert!(y.contains("c = { version = \"1\", optional = true }"));
        // y enables c, so x's weak forwarding edge still names a retained dependency.
        assert_eq!(feature_line(&y, "x"), "\"x\" = [\"c?/std\"]");
        assert_eq!(feature_line(&y, "y"), "\"y\" = [\"c/std\"]");
        println!("member manifest closure: default keeps b, [] drops b and c, y keeps c");
    }

    /// The inline form keeps strong forwarding alongside explicit dependency activation.
    #[test]
    fn member_manifest_handles_clap_and_rust_embed_optional_features() {
        let manifest = "[package]\nname = \"shape\"\nversion = \"1.0.0\"\nedition = \"2021\"\n\n[lib]\nproc-macro = true\n\n[dependencies]\npulldown-cmark = { version = \"0.10\", optional = true }\nrust-embed-impl = { version = \"8.12\", optional = true }\n\n[features]\nunstable-markdown = [\"dep:pulldown-cmark\", \"pulldown-cmark/unsafe-autolinks\"]\ninterpolate-folder-path = [\"dep:rust-embed-impl\"]\n";
        let made = generated_member(manifest, &["unstable-markdown"]);
        assert!(made.contains("pulldown-cmark = { version = \"0.10\", optional = true }"));
        assert!(!made.contains("rust-embed-impl = { version = \"8.12\", optional = true }"));
        assert_eq!(feature_line(&made, "unstable-markdown"), "\"unstable-markdown\" = [\"dep:pulldown-cmark\", \"pulldown-cmark/unsafe-autolinks\"]");
        assert_eq!(feature_line(&made, "interpolate-folder-path"), "\"interpolate-folder-path\" = []");
        println!("member manifest real shapes: clap keeps pulldown-cmark, rust-embed stays disabled");
    }

    #[test]
    fn clap_derive_normalized_dependencies_follow_unstable_markdown() {
        let manifest = r#"[package]
name = "clap_derive"
version = "4.6.7"
[features]
default = []
unstable-markdown = [
    "dep:pulldown-cmark",
    "dep:anstyle",
]
[lib]
name = "clap_derive"
path = "src/lib.rs"
proc-macro = true
[dependencies.anstyle]
version = "1.0.14"
optional = true
[dependencies.heck]
version = "0.5.0"
[dependencies.proc-macro2]
version = "1.0.106"
[dependencies.pulldown-cmark]
version = "0.13.3"
optional = true
default-features = false
[dependencies.quote]
version = "1.0.45"
[dependencies.syn]
version = "3.0.2"
features = ["full"]
"#;
        let default = generated_member(manifest, &["default"]);
        let markdown = generated_member(manifest, &["unstable-markdown"]);
        for name in ["heck", "proc-macro2", "quote", "syn"] {
            let header = format!("[dependencies.{name}]");
            assert!(default.lines().any(|line| line == header), "{default}");
            assert!(markdown.lines().any(|line| line == header), "{markdown}");
        }
        for name in ["anstyle", "pulldown-cmark"] {
            assert!(!default.contains(name), "{default}");
            assert!(markdown.contains(&format!("[dependencies.{name}]\n")), "{markdown}");
        }
        assert!(!default.contains("default-features = false"), "{default}");
        assert_eq!(feature_line(&default, "unstable-markdown"), "\"unstable-markdown\" = []");
        assert_eq!(feature_line(&markdown, "unstable-markdown"), "\"unstable-markdown\" = [\"dep:pulldown-cmark\", \"dep:anstyle\"]");
        println!("clap_derive: default keeps heck, proc-macro2, quote, syn; unstable-markdown adds pulldown-cmark and anstyle");
    }

    #[test]
    fn rust_embed_impl_normalized_dependencies_follow_interpolate_folder_path() {
        let manifest = r#"[package]
name = "rust-embed-impl"
version = "8.12.0"
[features]
"include-exclude" = ["rust-embed-utils/include-exclude"]
"interpolate-folder-path" = ["shellexpand"]
[lib]
name = "rust_embed_impl"
proc-macro = true
[dependencies.mime_guess]
version = "2.0.4"
[dependencies.proc-macro2]
version = "1"
[dependencies.quote]
version = "1"
[dependencies.rust-embed-utils]
version = "8.12.0"
[dependencies.shellexpand]
version = "3"
optional = true
[dependencies.syn]
version = "2"
features = [
    "derive",
    "parsing",
    "proc-macro",
    "printing",
]
default-features = false
[dependencies.walkdir]
version = "2.3.1"
"#;
        let none = generated_member(manifest, &[]);
        assert!(!none.contains("[dependencies.shellexpand]"), "{none}");
        // The removed dependency's implicit feature is kept, declared empty
        // (member_dependencies::implicit_features).
        assert_eq!(feature_line(&none, "shellexpand"), "\"shellexpand\" = []");
        assert_eq!(feature_line(&none, "interpolate-folder-path"), "\"interpolate-folder-path\" = []");
        assert_eq!(feature_line(&none, "include-exclude"), "\"include-exclude\" = [\"rust-embed-utils/include-exclude\"]");
        for name in ["mime_guess", "proc-macro2", "quote", "rust-embed-utils", "syn", "walkdir"] {
            assert!(none.contains(&format!("[dependencies.{name}]\n")), "{none}");
        }
        let syn = manifest.split_once("[dependencies.syn]\n").unwrap().1.split_once("[dependencies.walkdir]").unwrap().0;
        assert!(none.contains(syn), "{none}");
        let enabled = generated_member(manifest, &["interpolate-folder-path"]);
        assert!(enabled.contains("[dependencies.shellexpand]\nversion = \"3\"\noptional = true"), "{enabled}");
        assert_eq!(feature_line(&enabled, "interpolate-folder-path"), "\"interpolate-folder-path\" = [\"shellexpand\"]");
        println!("rust-embed-impl: [] keeps syn's feature array; interpolate-folder-path adds shellexpand");
    }

    #[test]
    fn target_and_build_tables_are_selected_by_alias() {
        let manifest = r#"[package]
name = "m"
version = "1.0.0"
[lib]
proc-macro = true
[features]
"render" = ["alias/syntax", "dep:inline"]
[target.'cfg(target_os = "linux")'.dependencies."alias"]
features = [
    "only-in-alias",
]
optional = true # follows the array
package = "real"
version = "1"
# only-in-alias, after the final setting
[build-dependencies]
inline = { package = "other", version = "1", optional = true }
[build-dependencies.always]
version = "1"
[dev-dependencies.unit]
version = "1"
[target.'cfg(unix)'.dev-dependencies.only-tests]
version = "1"
"#;
        let none = generated_member(manifest, &[]);
        for fragment in ["dependencies.\"alias\"", "package = \"real\"", "only-in-alias", "inline", "other", "dev-dependencies"] {
            assert!(!none.contains(fragment), "{none}");
        }
        // `alias` is an implicit feature (no `dep:alias`), kept declared empty once its dependency
        // is cut; `inline`'s is disabled by `dep:inline`, so none is written.
        assert_eq!(feature_line(&none, "alias"), "\"alias\" = []");
        let enabled = generated_member(manifest, &["render"]);
        assert!(enabled.contains("package = \"real\""), "{enabled}");
        assert!(enabled.contains("\"only-in-alias\""), "{enabled}");
        assert!(enabled.contains("inline = { package = \"other\""), "{enabled}");
        for made in [&none, &enabled] {
            assert!(made.contains("[build-dependencies.always]"), "{made}");
            assert!(!made.contains("dev-dependencies"), "{made}");
        }
        println!("render keeps the target alias and inline build dependency; both dev tables are omitted");
    }

    /// A workspace in a fresh folder: `files` as (relative path, text).
    fn workspace(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("blaze-pmm-members-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (path, text) in files {
            write(&dir.join(path), text).unwrap_or_else(|failure| panic!("{failure}"));
        }
        dir
    }

    fn wanted(name: &str, version: &str) -> Wanted {
        Wanted { name: name.to_owned(), version: version.to_owned(), features: None }
    }

    /// turso's root: a virtual workspace that opens with a comment and gives its members their
    /// version.
    const TURSO_ROOT: &str = "# Copyright 2023-2026 the Turso authors. All rights reserved. MIT license.\n\n[workspace]\nresolver = \"2\"\nmembers = [\n    \"core\",\n    \"macros\",\n]\n\n[workspace.package]\nversion = \"0.9.0-pre.6\"\nedition = \"2021\"\n";
    /// turso's macros/Cargo.toml as it is: line 1 a comment, `version.workspace = true`.
    const TURSO_MACROS: &str = "# Copyright 2024 the Limbo authors. All rights reserved. MIT license.\n\n[package]\nname = \"turso_macros\"\nversion.workspace = true\nedition.workspace = true\n\n[lib]\nproc-macro = true\n\n[dependencies]\nquote = \"1.0.38\"\n";
    const TURSO_CORE: &str = "# Copyright 2023-2025 the Turso authors. All rights reserved. MIT license.\n\n[package]\nname = \"turso_core\"\nversion.workspace = true\nedition.workspace = true\n\n[lib]\nname = \"turso_core\"\npath = \"lib.rs\"\n";

    /// turso's first check (2026-11-21): "the lock names the path package turso_macros
    /// 0.9.0-pre.6, and no Cargo.toml here names it", 1,741 errors and ~11,000 bodies behind it:
    /// the old walk read only the text before a manifest's first "\n[", which for a manifest
    /// opening with a comment is the comment. Read as cargo reads the workspace, the member is
    /// found and its inherited version matches; the twin without the comment is found the same.
    /// Controls, each removing the cause of a find: a lock version the member does not have is
    /// refused naming both; a manifest that names the package outside the workspace's members (a
    /// fixture) is not a member, so not found. Prints "members: turso_macros at macros ..".
    #[test]
    fn a_member_manifest_that_opens_with_a_comment_is_found_by_its_name() {
        let alloc = blaze_own::testing::heap().alloc();
        let fixture = "[package]\nname = \"turso_fixture\"\nversion = \"0.9.0-pre.6\"\n";
        let files = [
            ("Cargo.toml", TURSO_ROOT),
            ("macros/Cargo.toml", TURSO_MACROS),
            ("macros/src/lib.rs", ""),
            ("core/Cargo.toml", TURSO_CORE),
            ("core/lib.rs", ""),
            ("testing/fixture/Cargo.toml", fixture),
            ("testing/fixture/src/lib.rs", ""),
        ];
        let dir = workspace("comment", &files);
        let members = Members::new(&dir);
        let found = members.find(&wanted("turso_macros", "0.9.0-pre.6"), alloc).unwrap_or_else(|failure| panic!("not found: {failure}"));
        assert_eq!(found, dir.join("macros"));
        let core = members.find(&wanted("turso_core", "0.9.0-pre.6"), alloc).unwrap_or_else(|failure| panic!("not found: {failure}"));
        assert_eq!(core, dir.join("core"));

        let twin_text = TURSO_MACROS.split_once("\n\n").expect("the comment line").1;
        let twin_files = [("Cargo.toml", TURSO_ROOT), ("macros/Cargo.toml", twin_text), ("macros/src/lib.rs", ""), ("core/Cargo.toml", TURSO_CORE), ("core/lib.rs", "")];
        let twin = workspace("twin", &twin_files);
        let twin_found = Members::new(&twin).find(&wanted("turso_macros", "0.9.0-pre.6"), alloc).unwrap_or_else(|failure| panic!("twin not found: {failure}"));
        assert_eq!(twin_found, twin.join("macros"));

        let Err(Failure::Manifest { why: version, .. }) = members.find(&wanted("turso_macros", "0.9.0-pre.5"), alloc) else { panic!("another version was found") };
        assert!(version.contains("version 0.9.0-pre.6") && version.contains("turso_macros 0.9.0-pre.5"), "{version}");
        let Err(Failure::Manifest { why: outside, .. }) = members.find(&wanted("turso_fixture", "0.9.0-pre.6"), alloc) else { panic!("a manifest outside the members was taken for one") };
        assert!(outside.contains("none of the workspace's 2 members"), "{outside}");
        println!("members: turso_macros at macros, twin at macros | other version: {version} | fixture: {outside}");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&twin);
    }

    /// A git source's manifests are read for their `package.name` as TOML: one opening with a
    /// comment names its package; a virtual root names none; a text that is no TOML is the error
    /// git_origin names. Prints "git names: Some(..) None error".
    #[test]
    fn a_manifests_package_name_is_read_as_toml() {
        let alloc = blaze_own::testing::heap().alloc();
        let named = manifest_package_name(TURSO_MACROS, alloc).unwrap_or_else(|why| panic!("{}", why.display(TURSO_MACROS)));
        assert_eq!(named.as_deref(), Some("turso_macros"));
        let virtual_root = manifest_package_name(TURSO_ROOT, alloc).unwrap_or_else(|why| panic!("{}", why.display(TURSO_ROOT)));
        assert_eq!(virtual_root, None);
        assert!(manifest_package_name("[package\nname = 1", alloc).is_err());
        println!("git names: {named:?} {virtual_root:?} error");
    }

    /// The drift the first run of tempo's 79 had: syn 1.0.109 compiled where the lock gives
    /// syn 3.0.6, refused by name; the lock's own syn, std's dependencies and std pass.
    #[test]
    fn a_compiled_version_the_lock_does_not_name_is_refused() {
        let Err(Failure::NoModule { why, .. }) = checked("syn 1.0.109 /v/syn-1.0.109\n") else { panic!("syn 1.0.109 was not refused") };
        assert!(why.contains("syn 1.0.109"), "{why}");
        let fine = "syn 3.0.6 /v/syn-3.0.6\nhashbrown 0.17.1 /v/hashbrown-0.17.1\nstd 0.0.0 /sysroot/library/std\nblaze_pm_shim 0.0.0 /w/shim\ndata-encoding-macro-internal 0.1.19 /w/macro\n";
        match checked(fine) {
            Ok(count) => assert_eq!(count, 2),
            Err(failure) => panic!("the lock's own versions were refused: {failure}"),
        }
    }
}
