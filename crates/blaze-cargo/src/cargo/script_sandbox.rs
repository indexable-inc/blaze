//! Every run of a build script, cargo's (through the stand-in `script.rs` puts where rustc
//! linked it) and the daemon's own reruns, goes through [`run`], which runs it in the sandbox
//! every test runs in ([`crate::sandbox`]): the machine read-only, `OUT_DIR` written for real,
//! empty temporary directories, no network but the sandbox's own loopback, and exactly the
//! environment cargo gave it. "Everything should by default be in sandbox" (Andrew,
//! 2026-10-26; [#147]).
//!
//! Why `OUT_DIR` alone is written: it is the one place cargo tells a script to write ("Build
//! scripts may save any output files or intermediate artifacts in the directory specified in
//! the OUT_DIR environment variable. Scripts should not modify any files outside of that
//! directory", the Cargo book's build-scripts chapter). Cargo itself enforces nothing, so a
//! script that writes into its source or the target directory works under cargo; here that
//! write is refused, and the failure says what was refused and how to take the package out
//! (the alternative, an overlay that keeps the write private, would make it vanish silently).
//! On macOS the per-user cache directory (`getconf DARWIN_USER_CACHE_DIR`) is written for real
//! too: clang's module cache, the Metal compiler's and xcrun's live there, found by `confstr`
//! and not by a variable a private one could be named in, and they check what they cache.
//!
//! The package's directory, the workspace and the target directory are seen as they are
//! ([`Spec::readable`]) even under `/tmp`, which a sandbox otherwise has empty.
//!
//! Where it can't run sandboxed it fails, saying why and how to fix it, as `blaze
//! test` does: the ways out are [`sandboxed::WORKSPACE_TABLE`] in the workspace's root
//! manifest (`unsandboxed-build-scripts`, the only place a registry's package can be named),
//! `build-script = false` under [`sandboxed::TABLE`] of a package inside the workspace, and
//! `BLAZE_SANDBOX=off`.
//!
//! A sandboxed run is given only the part of cargo's environment that is the build's
//! ([`given`]): cargo's own variables for scripts, the toolchain's and blaze's, the C
//! compiler's and pkg-config's, what tests are given, and what the workspace's root manifest
//! names in `build-script-env`, and the variables cargo has that the script says it reads
//! (`rerun-if-env-changed`), as cargo gives them: a run that said it reads one it was not
//! given ([`not_given`]) saw it unset, and runs again given it ([`Script::declared`]). What
//! it is given is all of what the store keeps its run under (`script.rs`).
//!
//! What a sandboxed run read is recorded ([`Spec::record_reads`]), and kept beside `OUT_DIR`
//! ([`READS`]) and with the run in the store (`script.rs`'s `Kept`). The rule that uses it,
//! conservative on purpose: cargo runs a script again when a file it named in
//! `rerun-if-changed` changed, when a variable of `rerun-if-env-changed` did, and, when it
//! named neither, when any file of its package did. Only in that last case does the record
//! decide anything: a run whose record is whole (no `X` line, which a program macOS starts
//! without the recorder gives) and whose every read path is as it was stands for a run now,
//! and is not run again. A script that named what it reads is run again exactly when cargo
//! says, and one whose record is not whole is run whenever cargo would run it.
//!
//! [#147]: https://github.com/indexable-inc/blaze/issues/147

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

use anyhow::Context as _;

#[cfg(unix)]
use super::hermetic;
#[cfg(unix)]
pub use super::hermetic::InView;
use super::sandboxed;
use crate::sandbox::{self, Line, Reached, Spec};

/// A run in its closure alone, which Windows has none of: `hermetic` is the closure's, and
/// the closure is Linux's.
#[cfg(not(unix))]
#[derive(Clone, Debug)]
pub enum InView {}

/// The file beside a script's `OUT_DIR` with what its last run of cargo's read, when that is
/// what decides whether it runs again ([`reads_unchanged`]): a first line [`READS_HEAD`], then
/// what [`sandboxed::stat`] and [`sandboxed::digest`] said of each path, and the path, by tabs.
pub const READS: &str = "blaze-reads";

const READS_HEAD: &str = "blaze build script reads 1";

/// One run of a build script.
pub struct Script<'a> {
    /// The script rustc or blaze linked.
    pub real: &'a Path,
    pub args: &'a [OsString],
    /// Its package's directory, where cargo runs it.
    pub cwd: &'a Path,
    /// All of its environment, as cargo gave it, with `OUT_DIR` where it writes.
    pub env: &'a [(String, String)],
    pub out_dir: &'a Path,
    /// A directory of the caller's for the sandbox's spec and record: written here, outside.
    pub scratch: &'a Path,
    /// The variables of `env` a run of it said it reads (`rerun-if-env-changed`), given to it
    /// in the sandbox whatever [`given`] says.
    pub declared: &'a [String],
}

impl Script<'_> {
    fn var(&self, name: &str) -> Option<&str> {
        self.env.iter().find(|it| it.0 == name).map(|it| it.1.as_str())
    }

    fn package(&self) -> &str {
        self.var("CARGO_PKG_NAME").unwrap_or("?")
    }
}

/// How a run went.
pub struct Ran {
    pub output: Output,
    /// Each path it read that is an input, when it ran sandboxed and the record is whole.
    pub reads: Option<Vec<String>>,
}

/// Where a script runs.
#[derive(Clone)]
pub enum Plan {
    /// Outside, why, and the environment it runs with ([`with_own_rustc`]).
    Outside(String, Vec<(String, String)>),
    Sandboxed {
        spec: Spec,
        root_manifest: Option<PathBuf>,
        /// The system directories its `rustc-link-search` may name besides those it always may
        /// (`directives.rs`): `build-script-link-dirs` of the root manifest.
        link_dirs: Vec<PathBuf>,
        /// The closure it sees alone, when it does (`BLAZE_SCRIPT_VIEW`): then its run is kept
        /// by what it is made of, and not by what it was seen reading.
        in_view: Option<InView>,
    },
}

impl Plan {
    /// All of the environment the script runs with.
    pub fn env(&self) -> &[(String, String)] {
        match self {
            Plan::Outside(_, env) => env,
            Plan::Sandboxed { spec, .. } => &spec.env,
        }
    }
}

/// The directory of a run's `scratch` that has blaze linked as `rustc`.
const OWN_RUSTC: &str = "blaze-rustc";

/// What cargo set that blaze's own rustc stands for ([`with_own_rustc`]): a script is not
/// given them, and is not missing them.
const OWN_RUSTC_FOR: [&str; 4] =
    ["RUSTC", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", crate::as_rustc::TOOLCHAIN];

/// `env`, cargo's for a script, with `RUSTC` naming blaze as rustc ([`crate::as_rustc`]),
/// linked in `scratch`, and the rustc of blaze's release in [`crate::as_rustc::TOOLCHAIN`],
/// whichever rustc cargo named: what the script asks rustc, its version, the cfgs, whether a
/// probe compiles, is blaze's to answer, and no rustc runs. Cargo's wrappers are taken out,
/// as with no `build.rustc-wrapper` configured: a script runs `$RUSTC_WRAPPER $RUSTC`, and
/// blaze's wrapper would run rustc. A script cargo gave no `RUSTC` is left as it is.
fn with_own_rustc(
    env: &[(String, String)],
    scratch: &Path,
) -> anyhow::Result<Vec<(String, String)>> {
    if !env.iter().any(|it| it.0 == "RUSTC") {
        return Ok(env.to_vec());
    }
    let sysroot = super::toolchain::sysroot().with_context(|| {
        format!("blaze's toolchain, Rust {}, is not installed", super::toolchain::OURS.trim())
    })?;
    let rustc = sysroot.join("bin").join(format!("rustc{}", std::env::consts::EXE_SUFFIX));
    let rustc = rustc.to_str().context("a toolchain whose path is not text")?.to_owned();
    let dir = scratch.join(OWN_RUSTC);
    std::fs::create_dir_all(&dir).with_context(|| format!("making {}", dir.display()))?;
    let link = dir.join(format!("{}{}", crate::as_rustc::NAME, std::env::consts::EXE_SUFFIX));
    let exe = std::env::current_exe().context("finding blaze itself")?;
    if std::fs::read_link(&link).ok().as_deref() != Some(&*exe) {
        _ = std::fs::remove_file(&link);
        #[cfg(unix)]
        std::os::unix::fs::symlink(&exe, &link)
            .with_context(|| format!("linking {}", link.display()))?;
        #[cfg(windows)]
        std::fs::hard_link(&exe, &link).with_context(|| format!("linking {}", link.display()))?;
    }
    let link = link.to_str().context("a directory whose name is not text")?.to_owned();
    let mut env: Vec<(String, String)> =
        env.iter().filter(|it| !OWN_RUSTC_FOR.contains(&it.0.as_str())).cloned().collect();
    env.push(("RUSTC".to_owned(), link));
    env.push((crate::as_rustc::TOOLCHAIN.to_owned(), rustc));
    Ok(env)
}

/// Whether sandboxes are on for a script run with `env`, as [`sandbox::on_with`] says. The
/// variable is read from the run's environment, which is cargo's, so that the daemon's reruns
/// do as cargo's runs did.
fn on_in(env: &[(String, String)]) -> bool {
    sandbox::on_with(|name| env.iter().find(|it| it.0 == name).map(|it| it.1.clone()))
}

/// The variable that names to a run the root of the workspace being built, set by the
/// daemon's load (`compile_time.rs`): a path dependency that is a workspace of its own is
/// still the build's, whose root manifest says how its script runs. The script is not given it.
pub const ROOT: &str = "BLAZE_BUILD_ROOT";

/// The workspace a script's run is part of: the one [`ROOT`] names; else the one around its
/// package, for a package not below cargo's home; else the one around the target directory
/// its `OUT_DIR` is in; else the one the shell that ran cargo was in. `PWD` comes last: it is
/// wherever the command was started, which for a command run by another program (a harness,
/// an editor) is not the workspace built.
fn workspace_of(script: &Script<'_>) -> anyhow::Result<Option<PathBuf>> {
    if let Some(root) = script.var(ROOT).filter(|it| !it.is_empty()) {
        return Ok(Some(PathBuf::from(root)));
    }
    let home = script.var("CARGO_HOME").map(Path::new);
    if !home.is_some_and(|it| script.cwd.starts_with(it))
        && let Some(root) = sandboxed::workspace_root(script.cwd)?
    {
        return Ok(Some(root));
    }
    if let Some(dir) = target_of(script.out_dir).and_then(Path::parent)
        && let Some(root) = sandboxed::workspace_root(dir)?
    {
        return Ok(Some(root));
    }
    match script.var("PWD") {
        Some(pwd) => sandboxed::workspace_root(Path::new(pwd)),
        None => Ok(None),
    }
}

/// The target directory `out_dir` is in: what is above its `build` directory's profile.
pub fn target_of(out_dir: &Path) -> Option<&Path> {
    let build = out_dir.ancestors().find(|it| it.file_name().is_some_and(|it| it == "build"))?;
    build.parent()?.parent()
}

/// Where `script` runs: outside when sandboxes are off or its package is taken out, else the
/// sandbox's spec. `Err` is a manifest that says it wrongly.
pub fn plan(script: &Script<'_>) -> anyhow::Result<Plan> {
    let package = script.package();
    let mut env = with_own_rustc(script.env, script.scratch)?;
    env.retain(|it| it.0 != ROOT);
    if !on_in(script.env) {
        return Ok(Plan::Outside(format!("{}=off", crate::sandbox::VAR), env));
    }
    if !cfg!(any(target_os = "linux", target_os = "macos")) {
        // No running it outside instead: a build that can't keep its promise stops.
        anyhow::bail!(
            "the build script of `{package}` runs in a sandbox, and there is none on {} yet \
             (#257); `{}=off` runs build scripts and tests outside a sandbox, as cargo does",
            std::env::consts::OS,
            crate::sandbox::VAR
        );
    }
    let root = workspace_of(script)?;
    let mut also = std::collections::BTreeSet::new();
    let mut link_dirs = Vec::new();
    if let Some(root) = &root {
        let out = sandboxed::workspace_opt_out(root)?;
        also = out.build_script_env;
        link_dirs = out.build_script_link_dirs.iter().map(PathBuf::from).collect();
        if out.build_scripts.contains(package) {
            let why = format!("`{package}` is in `unsandboxed-build-scripts`");
            return Ok(Plan::Outside(format!("{why} in {}", out.manifest.display()), env));
        }
        // A package of the workspace may take its script out in its own manifest; one from a
        // registry or git, which is below cargo's home, can't take itself out.
        let home = script.var("CARGO_HOME").map(PathBuf::from);
        if script.cwd.starts_with(root)
            && !home.is_some_and(|it| script.cwd.starts_with(it))
            && sandboxed::opt_out(script.cwd)?.build_script_outside
        {
            let why =
                format!("`build-script = false` in {}", script.cwd.join("Cargo.toml").display());
            return Ok(Plan::Outside(why, env));
        }
    }
    let mut writable = vec![script.out_dir.to_owned()];
    writable.extend(user_cache());
    let mut readable = vec![script.cwd.to_owned()];
    readable.extend(root.clone());
    // A path dependency's own workspace, which is not below the build's.
    let home = script.var("CARGO_HOME").map(Path::new);
    if script.var(ROOT).is_some() && !home.is_some_and(|it| script.cwd.starts_with(it)) {
        readable.extend(sandboxed::workspace_root(script.cwd)?);
    }
    readable.extend(target_of(script.out_dir).map(Path::to_owned));
    // The store, read-only as the sandbox has the rest of the machine, where it would be empty
    // (one below `/tmp`): the blaze a script runs as its `rustc` reads the standard library's
    // frozen maps from it, and without them makes them again, seconds a probe.
    readable.extend(super::daemon::store().filter(|it| it.is_dir() && emptied(it)));
    // The programs cargo names, and the directories of `PATH`, where a sandbox has empty
    // directories: `RUSTC` is blaze's own, which may be built below `/dev/shm`.
    let var = |name: &str| env.iter().find(|it| it.0 == name).map(|it| it.1.as_str());
    let programs = ["RUSTC", "RUSTDOC", "CARGO"];
    let programs = programs.into_iter().filter_map(var).filter(|it| !it.is_empty());
    let programs = programs.filter_map(|it| Path::new(it).parent().map(Path::to_owned));
    let exe = std::env::current_exe().context("finding blaze itself")?;
    let programs = programs.chain(exe.parent().map(Path::to_owned));
    let path = script.var("PATH").unwrap_or_default().split(':').map(PathBuf::from);
    // One reached through a link (NixOS's `/run/current-system/sw/bin`) is seen where the link
    // leads, which is not emptied.
    readable.extend(programs.chain(path).filter(|it| {
        it.is_dir() && emptied(it) && std::fs::canonicalize(it).is_ok_and(|real| emptied(&real))
    }));
    // What it would download, handed to it locked, as `cargo/fetches.rs` says.
    let handed = super::fetches::hand(&env, script.out_dir, root.as_deref(), &also)?;
    readable.extend(handed.readable.iter().cloned());
    readable.sort();
    readable.dedup();
    // The innermost is enough: a directory is seen with what is below it.
    let all = readable.clone();
    readable.retain(|it| !all.iter().any(|other| other != it && it.starts_with(other)));
    let mut not_inputs = vec![script.real.to_owned()];
    not_inputs.extend(std::fs::canonicalize(script.real).ok());
    let root_manifest = root.as_ref().map(|it| it.join("Cargo.toml"));
    #[cfg(unix)]
    if hermetic::wanted(script.env) && cfg!(target_os = "linux") {
        let overrides = match &root_manifest {
            Some(manifest) => hermetic::system_deps(manifest)?,
            None => Default::default(),
        };
        let version = script.var("CARGO_PKG_VERSION").unwrap_or("0.0.0");
        let closure = hermetic::closure_for(package, version, &overrides)?;
        let mut also = also;
        also.extend(script.declared.iter().cloned());
        let mut env = hermetic::env_in(&env, &also, &closure);
        env.extend(handed.env);
        let deps = hermetic::dep_out_dirs(&env);
        let mut read = deps.clone();
        read.extend(handed.readable);
        read.extend(script.real.parent().map(Path::to_owned));
        read.push(script.scratch.join(OWN_RUSTC));
        read.extend(std::fs::canonicalize(&exe).ok().and_then(|it| Some(it.parent()?.to_owned())));
        let toolchain = env.iter().find(|it| it.0 == crate::as_rustc::TOOLCHAIN);
        let toolchain =
            toolchain.and_then(|it| Some(Path::new(&it.1).parent()?.parent()?.to_owned()));
        read.extend(toolchain.and_then(|it| std::fs::canonicalize(it).ok()));
        let (view, hidden) =
            hermetic::view(script.cwd, script.out_dir, &read, &closure, script.scratch)?;
        let spec = Spec {
            cwd: script.cwd.to_owned(),
            env,
            overlays: Vec::new(),
            writable: Vec::new(),
            readable: Vec::new(),
            record: Some(script.scratch.join("blaze-record")),
            record_reads: false,
            not_inputs,
            view: Some(view),
        };
        let in_view = Some(hermetic::InView { closure, hidden, deps });
        return Ok(Plan::Sandboxed { spec, root_manifest, link_dirs, in_view });
    }
    let declared = |name: &String| script.declared.contains(name);
    let mut env: Vec<(String, String)> = env
        .into_iter()
        .filter(|(name, _)| given(name) || also.contains(name) || declared(name))
        .collect();
    env.extend(handed.env);
    let spec = Spec {
        cwd: script.cwd.to_owned(),
        env,
        overlays: Vec::new(),
        writable,
        readable,
        record: Some(script.scratch.join("blaze-record")),
        record_reads: true,
        not_inputs,
        view: None,
    };
    Ok(Plan::Sandboxed { spec, root_manifest, link_dirs, in_view: None })
}

/// `plan`, a script's, for a program the script runs under WASI (`wasi_script.rs`): the same
/// sandbox, with `env`, in `cwd`, its spec and record in `scratch`.
#[cfg_attr(not(feature = "wasi"), allow(dead_code))]
pub fn for_tool(plan: &Plan, env: Vec<(String, String)>, cwd: &Path, scratch: &Path) -> Plan {
    match plan {
        Plan::Outside(why, _) => Plan::Outside(why.clone(), env),
        Plan::Sandboxed { spec, root_manifest, link_dirs, .. } => Plan::Sandboxed {
            spec: Spec {
                cwd: cwd.to_owned(),
                env,
                record: Some(scratch.join("blaze-record")),
                ..spec.clone()
            },
            root_manifest: root_manifest.clone(),
            link_dirs: link_dirs.clone(),
            in_view: None,
        },
    }
}

/// Whether the variable `name` of cargo's environment is given to a build script in the
/// sandbox: cargo's own for scripts (`CARGO_*`, `DEP_*`, `OUT_DIR`, `TARGET`, the profile's),
/// the toolchain's and blaze's (blaze is the `RUSTC` a script runs), the C compiler's, its
/// tools' and their flags, by any target (`CC`, `CC_<target>`, `TARGET_CC`), pkg-config's and
/// CMake's, the SDKs' of Apple's platforms, Nix's compiler wrappers', the jobserver, and what
/// a test is given ([`sandboxed::KEPT`]).
pub fn given(name: &str) -> bool {
    const PREFIXES: [&str; 10] = [
        "CARGO_",
        "DEP_",
        "BLAZE_",
        "NIX_",
        "PKG_CONFIG",
        "CMAKE",
        "HOST_",
        "TARGET_",
        "CXXSTDLIB",
        "LC_",
    ];
    const NAMES: [&str; 29] = [
        "OUT_DIR",
        "TARGET",
        "HOST",
        "NUM_JOBS",
        "OPT_LEVEL",
        "DEBUG",
        "PROFILE",
        "RUSTC",
        "RUSTDOC",
        "RUSTC_LINKER",
        "RUSTC_WORKSPACE_WRAPPER",
        "MAKEFLAGS",
        "MFLAGS",
        "TMPDIR",
        "XDG_CACHE_HOME",
        "SDKROOT",
        "DEVELOPER_DIR",
        "CRATE_CC_NO_DEFAULTS",
        "RUSTC_BOOTSTRAP",
        "SOURCE_DATE_EPOCH",
        "SYSROOT",
        "CPATH",
        "C_INCLUDE_PATH",
        "CPLUS_INCLUDE_PATH",
        "OBJC_INCLUDE_PATH",
        "OBJCPLUS_INCLUDE_PATH",
        "LIBRARY_PATH",
        "COMPILER_PATH",
        "GCC_EXEC_PREFIX",
    ];
    const TOOLS: [&str; 20] = [
        "CC",
        "CXX",
        "CPP",
        "AR",
        "AS",
        "LD",
        "RANLIB",
        "NM",
        "OBJCOPY",
        "OBJDUMP",
        "STRIP",
        "READELF",
        "WINDRES",
        "CFLAGS",
        "CXXFLAGS",
        "CPPFLAGS",
        "LDFLAGS",
        "ARFLAGS",
        "ASFLAGS",
        "RANLIBFLAGS",
    ];
    let tool = |it: &&str| {
        name.strip_prefix(*it).is_some_and(|rest| rest.is_empty() || rest.starts_with('_'))
    };
    // Where cargo and Nix's shell keep what is theirs for this one run: no script's input.
    const APART: [&str; 3] = ["CARGO_RESOLVER_LOCKFILE_PATH", "NIX_BUILD_TOP", "NIX_BUILD_CORES"];
    if APART.contains(&name) {
        return false;
    }
    sandboxed::KEPT.contains(&name)
        || NAMES.contains(&name)
        || PREFIXES.iter().any(|it| name.starts_with(it))
        || TOOLS.iter().any(tool)
        || name.ends_with("_DEPLOYMENT_TARGET")
        || name == crate::as_rustc::TOOLCHAIN
}

/// The variables a run that printed `stdout` said it reads (`rerun-if-env-changed`) which
/// cargo had in `env` but the run in the sandbox was not given (`plan`): it saw them unset,
/// and a run given them ([`Script::declared`]) is the one that stands for cargo's; what blaze's
/// own rustc stands for ([`OWN_RUSTC_FOR`]) it is not missing. A run in
/// its closure is given the closure's C toolchain and pkg-config in place of the machine's,
/// so those of [`given`] it was not given are not missing.
pub fn not_given(stdout: &[u8], env: &[(String, String)], plan: &Plan) -> Vec<String> {
    let Plan::Sandboxed { spec, in_view, .. } = plan else { return Vec::new() };
    let had = |name: &str, env: &[(String, String)]| env.iter().any(|it| it.0 == name);
    let mut names: Vec<String> = String::from_utf8_lossy(stdout)
        .lines()
        .filter_map(|line| {
            let line = line.strip_prefix("cargo:")?;
            let line = line.strip_prefix(':').unwrap_or(line);
            line.strip_prefix("rerun-if-env-changed=").map(str::to_owned)
        })
        .filter(|name| had(name, env) && !had(name, &spec.env))
        .filter(|name| !OWN_RUSTC_FOR.contains(&name.as_str()))
        .filter(|name| in_view.is_none() || !given(name))
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Whether `dir` is below a directory a sandbox starts empty: what is there is not seen unless
/// it is readable.
fn emptied(dir: &Path) -> bool {
    let temps = ["/tmp", "/var/tmp", "/dev/shm", "/run", "/private/tmp", "/private/var/tmp"];
    let temp = std::env::temp_dir();
    let real = std::fs::canonicalize(&temp).unwrap_or_else(|_| temp.clone());
    dir.is_absolute()
        && (temps.iter().any(|it| dir.starts_with(it))
            || dir.starts_with(&temp)
            || dir.starts_with(&real))
}

/// The per-user cache directory of macOS, `None` elsewhere.
#[cfg(target_os = "macos")]
fn user_cache() -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt as _;
    let mut buffer = [0u8; 1024];
    // SAFETY: `buffer` is writable for its length.
    let len = unsafe {
        libc::confstr(libc::_CS_DARWIN_USER_CACHE_DIR, buffer.as_mut_ptr().cast(), buffer.len())
    };
    if len == 0 || len > buffer.len() {
        return None;
    }
    let dir = PathBuf::from(std::ffi::OsStr::from_bytes(&buffer[..len - 1]));
    Some(std::fs::canonicalize(&dir).unwrap_or(dir))
}

#[cfg(not(target_os = "macos"))]
fn user_cache() -> Option<PathBuf> {
    None
}

/// Runs `script` as `plan` says, with `stdin`, its outputs kept. A failed run in the sandbox
/// has a note after what it printed to stderr, naming the package, what the sandbox refused
/// it and how to take it out; a sandbox that could not be made, why and how to fix it.
pub fn run(script: &Script<'_>, plan: &Plan, stdin: Stdio) -> anyhow::Result<Ran> {
    let (spec, root_manifest, in_view) = match plan {
        Plan::Outside(why, env) => {
            if std::env::var_os("BLAZE_STATS").is_some() {
                eprintln!("blaze: the build script of `{}` runs outside: {why}", script.package());
            }
            let mut command = Command::new(script.real);
            command.args(script.args).env_clear().envs(env.iter().map(|(k, v)| (k, v)));
            command.current_dir(script.cwd).stdin(stdin);
            let output = command.output().with_context(|| running(script))?;
            return Ok(Ran { output, reads: None });
        }
        Plan::Sandboxed { spec, root_manifest, in_view, .. } => (spec, root_manifest, in_view),
    };
    std::fs::create_dir_all(script.scratch)
        .with_context(|| format!("making {}", script.scratch.display()))?;
    let spec_file = script.scratch.join("blaze-sandbox.json");
    std::fs::write(&spec_file, serde_json::to_vec(spec)?)
        .with_context(|| format!("writing {}", spec_file.display()))?;
    let record = spec.record.clone().context("a sandbox with no record")?;
    _ = std::fs::remove_file(&record);
    let mut command = Command::new(std::env::current_exe().context("finding blaze itself")?);
    command.arg(sandbox::COMMAND).arg(&spec_file).arg("--").arg(script.real).args(script.args);
    command.current_dir(script.cwd).stdin(stdin);
    let mut output = command.output().with_context(|| running(script))?;
    let mut lines = sandbox::read_record(&record);
    // glibc asks nscd for any user or host it looks up; in a closure, that is all it is.
    if in_view.is_some()
        && let Some(lines) = &mut lines
    {
        lines.retain(|it| !(it.reached == Reached::Net && it.what.contains("/nscd/socket")));
    }
    let package = script.package();
    if output.status.code() == Some(sandbox::COULD_NOT)
        && lines.is_none()
        && let Err(why) = sandbox::available(false)
    {
        output.stderr.extend(
            format!(
                "error: the build script of `{package}` runs in a sandbox, which can't be made \
                 here: {why}\n`{}=off` runs build scripts and tests outside a sandbox, as cargo \
                 does\n",
                crate::sandbox::VAR
            )
            .into_bytes(),
        );
    } else if !output.status.success()
        && let Some(lines) = &lines
        && let Some(note) = refused_note(lines, package, root_manifest.as_deref())
    {
        output.stderr.extend(format!("note: {note}\n").into_bytes());
        let fetches =
            super::fetches::no_override_note(package, lines, &output.stdout, &output.stderr);
        if let Some(note) = fetches {
            output.stderr.extend(format!("note: {note}\n").into_bytes());
        }
    } else if !output.status.success()
        && let Some(in_view) = in_view
    {
        let note = failed_note(package, in_view, &output.stderr, root_manifest.as_deref());
        output.stderr.extend(format!("note: {note}\n").into_bytes());
    }
    if in_view.is_some() {
        return Ok(Ran { output, reads: None });
    }
    let reads = lines.filter(|it| it.iter().all(|it| it.reached != Reached::Incomplete));
    let reads = reads.map(|lines| inputs(&lines, script, spec));
    Ok(Ran { output, reads })
}

/// What a run of `package` in its closure that failed with `stderr` is told
/// ([`hermetic::failed_note`]).
#[cfg(unix)]
fn failed_note(package: &str, in_view: &InView, stderr: &[u8], root: Option<&Path>) -> String {
    hermetic::failed_note(package, &in_view.closure, stderr, root)
}

#[cfg(not(unix))]
fn failed_note(_: &str, in_view: &InView, _: &[u8], _: Option<&Path>) -> String {
    match *in_view {}
}

fn running(script: &Script<'_>) -> String {
    format!("running the build script of `{}` ({})", script.package(), script.real.display())
}

/// What the sandbox refused a script that failed, and how to take it out; `None` when it
/// refused nothing.
fn refused_note(lines: &[Line], package: &str, root_manifest: Option<&Path>) -> Option<String> {
    let mut them: Vec<&Line> =
        lines.iter().filter(|it| matches!(it.reached, Reached::Write | Reached::Net)).collect();
    if them.is_empty() {
        return None;
    }
    them.sort_by(|a, b| (a.reached, &a.what).cmp(&(b.reached, &b.what)));
    them.dedup_by(|a, b| a.reached == b.reached && a.what == b.what);
    let root = root_manifest
        .map_or("the workspace's root manifest".to_owned(), |it| it.display().to_string());
    Some(format!(
        "the build script of `{package}` failed in the sandbox, which refused it {}; if it \
         needs that, run it outside: `unsandboxed-build-scripts = [{package:?}]` under `{}` in \
         {root} (or `build-script = false` under `{}` in its own manifest, for a package of the \
         workspace), or `{}=off` for every build script and test of the command",
        sandboxed::what_was_refused(&them),
        sandboxed::WORKSPACE_TABLE,
        sandboxed::TABLE,
        sandboxed::VAR,
    ))
}

/// The paths of `lines` that are inputs of the run: not what it wrote (`OUT_DIR`, the
/// writable cache), not the temporary directories and the kernel's views, which are empty or
/// the run's own, unless a readable directory is there (a workspace below `/dev/shm`), and not
/// the script itself, which the run's key has.
fn inputs(lines: &[Line], script: &Script<'_>, spec: &Spec) -> Vec<String> {
    let mut written: Vec<PathBuf> = spec.writable.clone();
    written.push(script.scratch.to_owned());
    let mut own: Vec<PathBuf> =
        ["/dev", "/proc", "/sys", "/tmp", "/private/tmp", "/var/tmp"].map(PathBuf::from).into();
    own.extend(["/private/var/tmp", "/private/var/folders", "/var/folders"].map(PathBuf::from));
    own.push(std::env::temp_dir());
    let input = |path: &Path| {
        if written.iter().any(|it| path.starts_with(it))
            || spec.not_inputs.iter().any(|it| it == path)
        {
            return false;
        }
        spec.readable.iter().any(|it| path.starts_with(it))
            || !own.iter().any(|it| path.starts_with(it))
    };
    let mut paths: Vec<String> = lines
        .iter()
        .filter(|it| matches!(it.reached, Reached::Read | Reached::Looked))
        .filter(|it| input(Path::new(&it.what)))
        .map(|it| it.what.clone())
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

/// Writes down beside `out_dir` what the run of cargo's that wrote it read, `reads`, for
/// [`reads_unchanged`]; or that nothing is known, with `None`, which is what a run that named
/// what it reads, or ran outside or with a record that is not whole, leaves.
pub fn keep_reads(out_dir: &Path, reads: Option<&[String]>) {
    let Some(run_dir) = out_dir.parent() else { return };
    let file = run_dir.join(READS);
    let Some(reads) = reads else {
        _ = std::fs::remove_file(&file);
        return;
    };
    let mut text = format!("{READS_HEAD}\n");
    for path in reads {
        let it = Path::new(path);
        text.push_str(&format!("{}\t{}\t{path}\n", sandboxed::stat(it), sandboxed::digest(it)));
    }
    let writing = file.with_extension("writing");
    if std::fs::write(&writing, text).and_then(|()| std::fs::rename(&writing, &file)).is_err() {
        _ = std::fs::remove_file(&file);
    }
}

/// Whether the run that wrote `out_dir` read, in the sandbox, nothing that is not as it was
/// then, by what [`keep_reads`] wrote down: a run now would print and write the same. False
/// when nothing was written down: what it read is not known.
pub fn reads_unchanged(out_dir: &Path) -> bool {
    let Some(file) = out_dir.parent().map(|it| it.join(READS)) else { return false };
    let Ok(text) = std::fs::read_to_string(file) else { return false };
    let mut lines = text.lines();
    if lines.next() != Some(READS_HEAD) {
        return false;
    }
    lines.all(|line| {
        let mut parts = line.splitn(3, '\t');
        let (Some(stat), Some(digest), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            return false;
        };
        let path = Path::new(path);
        sandboxed::stat(path) == stat || sandboxed::digest(path) == digest
    })
}

/// Whether a script that printed `stdout` said what it reads: a file or a variable cargo runs
/// it again for. One that said neither is run again for any file of its package.
pub fn says_what_it_reads(stdout: &[u8]) -> bool {
    String::from_utf8_lossy(stdout).lines().any(|line| {
        let Some(line) = line.strip_prefix("cargo:") else { return false };
        let line = line.strip_prefix(':').unwrap_or(line);
        line.starts_with("rerun-if-changed=") || line.starts_with("rerun-if-env-changed=")
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn a_run_whose_reads_are_as_they_were_needs_no_rerun() {
        let dir = std::env::temp_dir().join(format!("blaze-script-reads-{}", std::process::id()));
        let out_dir = dir.join("out");
        std::fs::create_dir_all(&out_dir).unwrap();
        let read = dir.join("read.txt");
        std::fs::write(&read, "one").unwrap();
        assert!(!reads_unchanged(&out_dir), "nothing written down");
        keep_reads(&out_dir, Some(&[read.to_str().unwrap().to_owned()]));
        assert!(reads_unchanged(&out_dir));
        std::fs::write(&read, "two").unwrap();
        assert!(!reads_unchanged(&out_dir));
        keep_reads(&out_dir, None);
        assert!(!reads_unchanged(&out_dir));
        _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_script_says_what_it_reads_by_either_spelling() {
        assert!(says_what_it_reads(b"cargo:rustc-cfg=x\ncargo::rerun-if-changed=build.rs\n"));
        assert!(says_what_it_reads(b"cargo:rerun-if-env-changed=CC\n"));
        assert!(!says_what_it_reads(b"cargo:rustc-link-lib=z\n"));
    }

    #[test]
    fn what_was_refused_names_the_package_and_the_way_out() {
        let lines = [
            Line { reached: Reached::Write, test: String::new(), what: "/src/gen.rs".into() },
            Line { reached: Reached::Net, test: String::new(), what: "1.2.3.4:443".into() },
            Line { reached: Reached::Read, test: String::new(), what: "/src/x".into() },
        ];
        let note = refused_note(&lines, "sys-crate", Some(Path::new("/ws/Cargo.toml"))).unwrap();
        assert!(
            note.starts_with("the build script of `sys-crate` failed in the sandbox"),
            "{note}"
        );
        assert!(note.contains("writing `/src/gen.rs`") && note.contains("`1.2.3.4:443`"), "{note}");
        assert!(note.contains("`unsandboxed-build-scripts = [\"sys-crate\"]`"), "{note}");
        assert!(note.contains("/ws/Cargo.toml"), "{note}");
        assert!(refused_note(&lines[2..], "x", None).is_none());
    }

    #[test]
    fn what_a_workspace_below_dev_shm_read_is_an_input() {
        let read =
            |what: &str| Line { reached: Reached::Read, test: String::new(), what: what.into() };
        let lines = [
            read("/dev/shm/ws/pkg/VERSION"),
            read("/dev/shm/ws/target/debug/build/pkg-1/out/made.rs"),
            read("/dev/null"),
            read("/tmp/x"),
            read("/usr/lib/libc.so"),
        ];
        let out_dir = Path::new("/dev/shm/ws/target/debug/build/pkg-1/out");
        let script = Script {
            real: Path::new("/dev/shm/ws/target/debug/build/pkg-0/build_script_build.real"),
            args: &[],
            cwd: Path::new("/dev/shm/ws/pkg"),
            env: &[],
            out_dir,
            scratch: Path::new("/dev/shm/ws/target/debug/build/pkg-1"),
            declared: &[],
        };
        let spec = Spec {
            writable: vec![out_dir.to_owned()],
            readable: vec!["/dev/shm/ws".into()],
            ..Spec::default()
        };
        assert_eq!(inputs(&lines, &script, &spec), ["/dev/shm/ws/pkg/VERSION", "/usr/lib/libc.so"]);
    }

    #[test]
    fn a_sandboxed_script_is_given_what_cargo_and_a_c_compiler_read() {
        for name in ["OUT_DIR", "CARGO_CFG_UNIX", "DEP_Z_INCLUDE", "CC", "CFLAGS_aarch64_linux"] {
            assert!(given(name), "{name}");
        }
        for name in ["PATH", "PKG_CONFIG_PATH", "SOURCE_DATE_EPOCH", "MACOSX_DEPLOYMENT_TARGET"] {
            assert!(given(name), "{name}");
        }
        for name in ["OPENSSL_DIR", "CCACHE", "RUSTC_WRAPPER", "NIX_BUILD_TOP", "SSH_AUTH_SOCK"] {
            assert!(!given(name), "{name}");
        }
    }

    #[test]
    fn a_script_that_says_it_reads_what_it_was_not_given_is_named() {
        let pair = |name: &str| (name.to_owned(), "x".to_owned());
        let spec = Spec { env: vec![pair("CC")], ..Spec::default() };
        let plan =
            Plan::Sandboxed { spec, root_manifest: None, link_dirs: Vec::new(), in_view: None };
        let outside = [pair("CC"), pair("OPENSSL_DIR")];
        let stdout = b"cargo:rerun-if-env-changed=CC\ncargo::rerun-if-env-changed=OPENSSL_DIR\n\
            cargo:rerun-if-env-changed=OPENSSL_DIR\ncargo:rerun-if-env-changed=UNSET\n";
        assert_eq!(not_given(stdout, &outside, &plan), ["OPENSSL_DIR"]);
    }

    #[test]
    fn a_declared_variable_is_given_and_nothing_else_of_the_shell() {
        let dir = std::env::temp_dir().join(format!("blaze-declared-{}", std::process::id()));
        let out_dir = dir.join("target/debug/build/pkg-1/out");
        std::fs::create_dir_all(&out_dir).unwrap();
        let pair = |name: &str| (name.to_owned(), "x".to_owned());
        let env = [pair("CARGO_PKG_NAME"), pair("BM_LEVEL"), pair("SECRET")];
        let declared = ["BM_LEVEL".to_owned()];
        let script = Script {
            real: Path::new("/nonexistent/build_script_build"),
            args: &[],
            cwd: &dir,
            env: &env,
            out_dir: &out_dir,
            scratch: out_dir.parent().unwrap(),
            declared: &declared,
        };
        let names = |plan: &Plan| plan.env().iter().map(|it| it.0.clone()).collect::<Vec<_>>();
        let stdout = b"cargo:rerun-if-env-changed=BM_LEVEL\n";
        let given = plan(&script).unwrap();
        assert!(matches!(given, Plan::Sandboxed { .. }));
        assert_eq!(names(&given), ["CARGO_PKG_NAME", "BM_LEVEL"]);
        assert!(not_given(stdout, &env, &given).is_empty());
        let unset = plan(&Script { declared: &[], ..script }).unwrap();
        assert_eq!(names(&unset), ["CARGO_PKG_NAME"]);
        assert_eq!(not_given(stdout, &env, &unset), ["BM_LEVEL"]);
        _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_path_dependency_in_a_workspace_of_its_own_reads_the_builds_root_manifest() {
        let dir = std::env::temp_dir().join(format!("blaze-build-root-{}", std::process::id()));
        let (root, dep) = (dir.join("app"), dir.join("vendor/vendor"));
        let out_dir = root.join("target/debug/build/vendor-1/out");
        std::fs::create_dir_all(&out_dir).unwrap();
        std::fs::create_dir_all(&dep).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\n[workspace.metadata.blaze.sandbox]\nbuild-script-env = [\"VENDOR_DIR\"]\n",
        )
        .unwrap();
        std::fs::write(dir.join("vendor/Cargo.toml"), "[workspace]\nmembers = [\"vendor\"]\n")
            .unwrap();
        std::fs::write(
            dep.join("Cargo.toml"),
            "[package]\nname = \"vendor\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let pair = |name: &str, value: &str| (name.to_owned(), value.to_owned());
        let told = [
            pair("CARGO_PKG_NAME", "vendor"),
            pair("VENDOR_DIR", "/opt/vendor"),
            pair(ROOT, root.to_str().unwrap()),
        ];
        let script = Script {
            real: Path::new("/nonexistent/build_script_build"),
            args: &[],
            cwd: &dep,
            env: &told,
            out_dir: &out_dir,
            scratch: out_dir.parent().unwrap(),
            declared: &[],
        };
        let names = |plan: &Plan| plan.env().iter().map(|it| it.0.clone()).collect::<Vec<_>>();
        assert_eq!(names(&plan(&script).unwrap()), ["CARGO_PKG_NAME", "VENDOR_DIR"]);
        let Plan::Sandboxed { spec, .. } = plan(&script).unwrap() else { panic!("outside") };
        assert!(spec.readable.contains(&dir.join("vendor")), "{:?}", spec.readable);
        let untold = plan(&Script { env: &told[..2], ..script }).unwrap();
        assert_eq!(names(&untold), ["CARGO_PKG_NAME"]);
        _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn what_blaze_s_rustc_stands_for_is_not_missing() {
        let dir = std::env::temp_dir().join(format!("blaze-own-rustc-{}", std::process::id()));
        let out_dir = dir.join("target/debug/build/pkg-1/out");
        std::fs::create_dir_all(&out_dir).unwrap();
        let pair = |name: &str, value: &str| (name.to_owned(), value.to_owned());
        let env = [
            pair("CARGO_PKG_NAME", "pkg"),
            pair("RUSTC", "rustc"),
            pair("RUSTC_WRAPPER", "/w/blaze-rustc"),
        ];
        let script = Script {
            real: Path::new("/nonexistent/build_script_build"),
            args: &[],
            cwd: &dir,
            env: &env,
            out_dir: &out_dir,
            scratch: out_dir.parent().unwrap(),
            declared: &[],
        };
        let plan = plan(&script).unwrap();
        assert!(!plan.env().iter().any(|it| it.0 == "RUSTC_WRAPPER"));
        let stdout =
            b"cargo::rerun-if-env-changed=RUSTC_WRAPPER\ncargo::rerun-if-env-changed=RUSTC\n";
        assert!(not_given(stdout, &env, &plan).is_empty());
        _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_target_directory_is_above_the_profile_of_build() {
        let out = Path::new("/w/target/debug/build/x-0123/out");
        assert_eq!(target_of(out), Some(Path::new("/w/target")));
        let out =
            Path::new("/w/target/x86_64-unknown-linux-gnu/debug/build/x/0123456789abcdef/out");
        assert_eq!(target_of(out), Some(Path::new("/w/target/x86_64-unknown-linux-gnu")));
    }
}
