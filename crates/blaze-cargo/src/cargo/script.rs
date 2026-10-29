//! Build scripts of the workspace. Cargo runs one again when what it said it reads has
//! changed, or when anything in its package has if it said nothing, and then compiles the
//! package again whatever the script printed. The daemon runs the script the same way and
//! looks at what it printed and wrote: when that is what it was, the crates compiled with
//! it are still right, and the build stays the daemon's.
//!
//! To run a script as cargo does, the wrapper puts a few lines of shell where rustc linked
//! it. Cargo runs those, and they run `blaze-engine build-script`, which writes down the
//! environment and the directory cargo ran it in and runs the script. Every run, cargo's and
//! the daemon's, is [`script_sandbox::run`]'s, in the sandbox tests run in.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    io::Write as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, SystemTime},
};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::{directives, keyed, own_script, sandboxed, script_sandbox, wasi_script};
#[cfg(unix)]
use super::{hermetic, script_sandbox::InView};

/// The variable that names the script itself to `blaze-engine build-script`.
pub const REAL: &str = "BLAZE_BUILD_SCRIPT";

/// The file next to a script's `OUT_DIR` that says how cargo ran it.
const RAN: &str = "blaze-ran.json";

/// The file next to a script's `OUT_DIR` whose time is when [`rerun_is_same`] last found it
/// the same.
const CHECKED: &str = "blaze-checked";

/// How cargo ran a build script.
#[derive(Serialize, Deserialize)]
struct Ran {
    real: PathBuf,
    cwd: PathBuf,
    env: Vec<(String, String)>,
}

/// Has cargo run the script rustc linked at `linked` through `blaze-engine build-script`.
#[cfg(unix)]
pub fn stand_in_front(linked: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let real = real_of(linked);
    std::fs::rename(linked, &real)?;
    let exe = std::env::current_exe()?;
    let quoted = |path: &Path| -> anyhow::Result<String> {
        let path = path.to_str().context("a path that is not text")?;
        Ok(shlex::try_quote(path)?.into_owned())
    };
    let lines = format!(
        "#!/bin/sh\n{REAL}={} exec {} build-script \"$@\"\n",
        quoted(&real)?,
        quoted(&exe)?
    );
    std::fs::write(linked, lines)?;
    Ok(std::fs::set_permissions(linked, std::fs::Permissions::from_mode(0o755))?)
}

/// Has cargo run the script rustc linked at `linked` through `blaze-engine build-script`. There is
/// no shell to say so in a file cargo runs, so this executable is linked there under the
/// script's name, and [`stood_in_front_of`] is how it knows. Where it can't be linked, on
/// another volume, cargo runs the script itself and a change it reruns for is cargo's to
/// build.
#[cfg(windows)]
pub fn stand_in_front(linked: &Path) -> anyhow::Result<()> {
    let real = real_of(linked);
    std::fs::rename(linked, &real)?;
    if std::fs::hard_link(std::env::current_exe()?, linked).is_err() {
        std::fs::rename(&real, linked)?;
    }
    Ok(())
}

/// The script this executable stands in front of, when it runs under the name of one:
/// the name rustc linked it under, or the one cargo gives it in the same directory.
#[cfg(windows)]
pub fn stood_in_front_of() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let name = exe.file_stem()?.to_str()?;
    if !name.starts_with("build_script_") && !name.starts_with("build-script-") {
        return None;
    }
    let beside = std::fs::read_dir(exe.parent()?).ok()?.flatten().map(|it| it.path());
    beside.into_iter().find(|it| it.to_str().is_some_and(|it| it.ends_with(".exe.real")))
}

fn real_of(linked: &Path) -> PathBuf {
    let mut real = linked.as_os_str().to_owned();
    real.push(".real");
    PathBuf::from(real)
}

/// `blaze-engine build-script`: writes down how cargo runs the script and becomes it.
pub fn command(args: impl Iterator<Item = OsString>) -> anyhow::Result<()> {
    let real = PathBuf::from(std::env::var_os(REAL).context("no build script is named")?);
    run_as_cargo_does(real, args)
}

/// Writes down how cargo runs the script at `real`, and runs it as [`script_sandbox::plan`]
/// says, or has a run the store kept stand for it.
///
/// A run in the sandbox is kept in the store ([`keyed`]): first by what the script was built
/// from ([`script_of`]), its arguments, the environment it was given, which the sandbox cut to
/// the build's ([`script_sandbox::given`]) and what it said it reads (`rerun-if-env-changed`,
/// [`script_sandbox::Script::declared`]), and the toolchain; then by every path the sandbox
/// saw it read, with the files of `rerun-if-changed`, and what each has. A run outside the
/// sandbox is not kept: what it read is not known. With [`keyed::CHECK`] a run the store has
/// is run again, and one that prints or writes something else is an error.
pub fn run_as_cargo_does(
    real: PathBuf,
    args: impl Iterator<Item = OsString>,
) -> anyhow::Result<()> {
    let env: Vec<(String, String)> = std::env::vars_os()
        .filter(|(name, _)| name != REAL)
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .collect();
    let out_dir =
        env.iter().find(|(name, _)| name == "OUT_DIR").map(|(_, value)| PathBuf::from(value));
    let args: Vec<OsString> = args.collect();
    let cwd = std::env::current_dir()?;
    let mut script = Command::new(&real);
    script.args(&args).env_remove(REAL);
    // Not run by cargo: there is nowhere for it to write.
    let Some(out_dir) = out_dir else { return become_it(&mut script) };
    let run_dir = out_dir.parent().context("an OUT_DIR at the root")?.to_owned();
    let places = Places::of(&env);
    let ran = Ran { real: real.clone(), cwd: cwd.clone(), env };
    // Without it the script is not run again by the daemon, and cargo builds.
    let _ = std::fs::write(run_dir.join(RAN), serde_json::to_vec(&ran)?);
    let package = ran.env.iter().find(|it| it.0 == "CARGO_PKG_NAME").map_or("?", |it| &it.1);
    blaze_store::compiles_against(RUNS);
    let store = blaze_store::Store::global_or(super::daemon::store().as_deref());
    let placed_now =
        |path: &str| keyed::digest(&String::from_utf8_lossy(&places.place(path.as_bytes())));
    // What a run said it reads that cargo has: a run not given it saw it unset, and the run
    // that stands for cargo's is one given it, kept under its value.
    let mut declared: Vec<String> = Vec::new();
    loop {
        let running = script_sandbox::Script {
            real: &real,
            args: &args,
            cwd: &cwd,
            env: &ran.env,
            out_dir: &out_dir,
            scratch: &run_dir,
            declared: &declared,
        };
        let plan = script_sandbox::plan(&running)?;
        // Under `wasi`, the module runs for cargo and the process ends as it did; under
        // `wasi-compare`, it runs first and what it did is set aside.
        if declared.is_empty()
            && let Some(mode) = wasi_script::mode(package, &cwd, Some(&out_dir))?
        {
            wasi_script::as_cargo_does(&running, &plan, mode, &places)?;
        }
        let (link_dirs, in_view) = match &plan {
            script_sandbox::Plan::Sandboxed { link_dirs, in_view, .. } => {
                (link_dirs.clone(), in_view.clone())
            }
            script_sandbox::Plan::Outside(..) => {
                script_sandbox::keep_reads(&out_dir, None);
                script.env_clear().envs(plan.env().iter().map(|(k, v)| (k, v)));
                return become_it(&mut script);
            }
        };
        let directives =
            directives::Ran { package_dir: &cwd, out_dir: &out_dir, link_dirs: &link_dirs };
        #[cfg(not(unix))]
        if let Some(in_view) = &in_view {
            match *in_view {}
        }
        #[cfg(unix)]
        if let Some(in_view) = &in_view {
            let key = view_key(&real, &cwd, &args, plan.env(), &places, in_view)?;
            // What a run left that names where it ran in a file that is no text is kept for
            // this place alone.
            let mut here = keyed::Key::new(IN_VIEW);
            here.field(&key);
            for (place, _) in &places.0 {
                here.field(place);
            }
            let here = here.done();
            let checking = keyed::check();
            let get =
                |key: &[u8; 32]| store.and_then(|it| it.get(key)).and_then(|it| Kept::read(&it));
            let found = get(&key).map(|it| it.placed(&places)).or_else(|| get(&here));
            if let Some(kept) = &found
                && !checking
            {
                let unset = unset_now(&kept.stdout, &ran.env, &plan, &declared);
                if !unset.is_empty() {
                    declared.extend(unset);
                    continue;
                }
                kept.restore(&out_dir)?;
                script_sandbox::keep_reads(&out_dir, None);
                if std::env::var_os("BLAZE_STATS").is_some() {
                    eprintln!("blaze: a run the store kept, of its closure");
                }
                std::io::stdout().write_all(&hermetic::for_cargo(&kept.stdout))?;
                std::io::stderr().write_all(&kept.stderr)?;
                return Ok(());
            }
            let script_sandbox::Ran { output, .. } =
                script_sandbox::run(&running, &plan, Stdio::inherit())?;
            let unset = unset_now(&output.stdout, &ran.env, &plan, &declared);
            let refused = directives::refused(&output.stdout, &directives);
            if unset.is_empty() {
                std::io::stdout().write_all(&hermetic::for_cargo(&output.stdout))?;
                std::io::stderr().write_all(&output.stderr)?;
                if output.status.success() && !refused.is_empty() {
                    eprintln!("error: {}", directives::refused_error(package, &refused));
                    std::process::exit(1);
                }
            }
            script_sandbox::keep_reads(&out_dir, None);
            let made = (output.status.success() && refused.is_empty())
                .then(|| Kept::of(&output.stdout, &output.stderr, &out_dir))
                .flatten();
            if checking && let (Some(kept), Some(made)) = (&found, &made) {
                same_as_kept(package, kept, made, &out_dir);
            }
            if let (Some(store), Some(made)) = (store, made) {
                store.settled();
                match made.unplaced(&places) {
                    Some(unplaced) => store.insert(&key, &unplaced.bytes()),
                    None => store.insert(&here, &made.bytes()),
                }
                store.flush();
            }
            if !unset.is_empty() {
                std::fs::remove_dir_all(&out_dir)
                    .and_then(|()| std::fs::create_dir_all(&out_dir))
                    .with_context(|| format!("emptying {}", out_dir.display()))?;
                declared.extend(unset);
                continue;
            }
            std::process::exit(output.status.code().unwrap_or(1))
        }
        let key = |places: Option<&Places>| run_key(&real, &cwd, &args, plan.env(), places);
        let keys = store.zip(key(Some(&places))).zip(key(None));
        // A run another checkout or target directory kept, or, when what it left names where it
        // ran in a file that is no text, one of this place.
        let found = keys.as_ref().and_then(|((store, shared), here)| {
            let read = |(bytes, reads): (Vec<u8>, Vec<String>)| Some((Kept::read(&bytes)?, reads));
            let shared =
                keyed::find(store, shared, &placed_now).and_then(read).map(|(kept, reads)| {
                    let reads = reads.iter().map(|it| {
                        String::from_utf8_lossy(&places.place(it.as_bytes())).into_owned()
                    });
                    (kept.placed(&places), reads.collect::<Vec<_>>())
                });
            shared.or_else(|| keyed::find(store, here, &keyed::digest).and_then(read))
        });
        let checking = keyed::check();
        if let Some((kept, reads)) = &found
            && !checking
        {
            let unset = unset_now(&kept.stdout, &ran.env, &plan, &declared);
            if !unset.is_empty() {
                declared.extend(unset);
                continue;
            }
            kept.restore(&out_dir)?;
            let reads: Vec<String> = reads.iter().map(|it| keyed::path_of(it).to_owned()).collect();
            let reads = (!script_sandbox::says_what_it_reads(&kept.stdout)).then_some(&reads[..]);
            script_sandbox::keep_reads(&out_dir, reads);
            // Cargo keeps what a script printed to stderr beside its `OUT_DIR`.
            if std::env::var_os("BLAZE_STATS").is_some() {
                eprintln!("blaze: a run the store kept");
            }
            std::io::stdout().write_all(&kept.stdout)?;
            std::io::stderr().write_all(&kept.stderr)?;
            return Ok(());
        }
        let script_sandbox::Ran { output, reads } =
            script_sandbox::run(&running, &plan, Stdio::inherit())?;
        // A run that failed without what it said it reads (`env::var("X").expect(..)`, or a
        // download the sandbox refused for want of the local copy `X` names) failed as cargo's
        // would not: its failure is the one of the run given it.
        let unset = unset_now(&output.stdout, &ran.env, &plan, &declared);
        let refused = directives::refused(&output.stdout, &directives);
        if unset.is_empty() {
            std::io::stdout().write_all(&output.stdout)?;
            std::io::stderr().write_all(&output.stderr)?;
            if output.status.success() && !refused.is_empty() {
                eprintln!("error: {}", directives::refused_error(package, &refused));
                std::process::exit(1);
            }
        }
        let undeclared =
            reads.as_deref().filter(|_| !script_sandbox::says_what_it_reads(&output.stdout));
        script_sandbox::keep_reads(&out_dir, undeclared);
        let made = (output.status.success() && refused.is_empty())
            .then(|| Kept::of(&output.stdout, &output.stderr, &out_dir))
            .flatten();
        if checking && let (Some((kept, _)), Some(made)) = (&found, &made) {
            same_as_kept(package, kept, made, &out_dir);
        }
        if let (Some(((store, shared), here)), Some(made), Some(mut reads)) = (keys, made, reads) {
            reads.extend(watched(&output.stdout, &cwd));
            let reads = kept_reads(reads, &real, &cwd, sysroot_of());
            store.settled();
            match made.unplaced(&places).zip(places.unplaced_reads(&reads)) {
                Some((made, reads)) => {
                    keyed::keep(store, &shared, &reads, &placed_now, &made.bytes())
                }
                None => keyed::keep(store, &here, &reads, &keyed::digest, &made.bytes()),
            }
            store.flush();
        }
        if !unset.is_empty() {
            // What it left is of the variables unset: the run given them starts as cargo's would.
            std::fs::remove_dir_all(&out_dir)
                .and_then(|()| std::fs::create_dir_all(&out_dir))
                .with_context(|| format!("emptying {}", out_dir.display()))?;
            declared.extend(unset);
            continue;
        }
        std::process::exit(output.status.code().unwrap_or(1))
    }
}

/// What a run that printed `stdout` said it reads and was not given
/// ([`script_sandbox::not_given`]), but for what it was `declared` already: a variable it is
/// given and still lacks can't be given, and a run again would be the same.
fn unset_now(
    stdout: &[u8],
    env: &[(String, String)],
    plan: &script_sandbox::Plan,
    declared: &[String],
) -> Vec<String> {
    let mut unset = script_sandbox::not_given(stdout, env, plan);
    unset.retain(|it| !declared.contains(it));
    unset
}

/// With [`keyed::CHECK`]: ends the process, failed, when `made`, what a run of `package`'s
/// script into `out_dir` did, is not `kept`, what the store kept of a run with its inputs.
fn same_as_kept(package: &str, kept: &Kept, made: &Kept, out_dir: &Path) {
    if kept != made {
        let what = match kept.stdout == made.stdout {
            true => "wrote something else in its OUT_DIR",
            false => "printed something else",
        };
        eprintln!(
            "error: {}=check: the build script of `{package}` {what} than the run the store kept \
             of it with the same inputs ({})",
            keyed::CHECK,
            out_dir.display()
        );
        std::process::exit(1);
    }
    if std::env::var_os("BLAZE_STATS").is_some() {
        eprintln!("blaze: a run the store kept is what this one did");
    }
}

/// What a run in its closure is kept under: what its script was built from, the toolchain its
/// `RUSTC` answers for, its arguments and its environment, which [`hermetic::env_in`] made of
/// cargo's and the closure (each with what stands for the places of this checkout), and what
/// [`hermetic::key_of`] adds: its package's source, the closure's NarHashes and its
/// dependencies' `OUT_DIR`s. Nothing of what it was seen reading: it sees nothing else.
#[cfg(unix)]
fn view_key(
    real: &Path,
    cwd: &Path,
    args: &[OsString],
    env: &[(String, String)],
    places: &Places,
    in_view: &InView,
) -> anyhow::Result<[u8; 32]> {
    let mut key = keyed::Key::new(IN_VIEW);
    key.field(script_of(real).context("a build script that can't be read")?.as_bytes());
    key.field(sysroot_of().as_deref().map(keyed::toolchain).unwrap_or_default().as_bytes());
    for arg in args {
        key.field(&places.unplace(arg.as_encoded_bytes()));
    }
    let mut env: Vec<&(String, String)> = env
        .iter()
        .filter(|(name, _)| !UNREAD.contains(&name.as_str()) && name != crate::as_rustc::TOOLCHAIN)
        .collect();
    env.sort();
    for (name, value) in env {
        key.field(name.as_bytes());
        match name.as_str() {
            // Which cargo runs is not seen: the run has no cargo to run.
            "CARGO" => key.field(b"cargo"),
            _ => key.field(&places.unplace(value.as_bytes())),
        };
    }
    let unplace = |bytes: &[u8]| places.unplace(bytes);
    let InView { closure, hidden, deps } = in_view;
    hermetic::key_of(&mut key, cwd, hidden, closure, deps, &unplace)?;
    Ok(key.done())
}

/// The files a run that printed `stdout` in `cwd` named in `rerun-if-changed`, each file below
/// one that is a directory: inputs of the run whether or not the sandbox saw them read.
fn watched(stdout: &[u8], cwd: &Path) -> Vec<String> {
    let mut paths = Vec::new();
    for line in String::from_utf8_lossy(stdout).lines() {
        let Some(line) = line.strip_prefix("cargo:") else { continue };
        let line = line.strip_prefix(':').unwrap_or(line);
        let Some(path) = line.strip_prefix("rerun-if-changed=") else { continue };
        let mut pending = vec![cwd.join(path)];
        while let Some(path) = pending.pop() {
            if let Ok(entries) = std::fs::read_dir(&path) {
                pending.extend(entries.flatten().map(|it| it.path()));
            }
            paths.extend(path.to_str().map(str::to_owned));
        }
    }
    paths
}

/// What the store's runs of build scripts are kept under in place of a toolchain: what a run
/// prints and writes is told by the script, which its key has. 4 since a run is kept by what
/// the sandbox saw it read ([`keyed`]), 5 since what one printed is checked before it is kept
/// ([`directives`]).
pub const RUNS: &str = "build script runs 5";

/// The kind of the key of a run that saw its closure alone ([`view_key`]).
#[cfg(unix)]
const IN_VIEW: &str = "build script runs in their closure 1";

/// The variables of a run that change nothing it prints or writes: the jobs it may start, and
/// the directories the shell that ran cargo was in, which cargo leaves as they were (it runs
/// the script in its package's directory). Whether it runs in a sandbox changes nothing it
/// prints or writes either: a run that the sandbox refused something fails, and a failed run
/// is not kept. Nor does where blaze keeps its store, its traces and its statistics, or
/// whether it checks the store's runs.
pub const UNREAD: [&str; 13] = [
    "CARGO_MAKEFLAGS",
    "MAKEFLAGS",
    "MFLAGS",
    "NUM_JOBS",
    "PWD",
    "OLDPWD",
    "TMPDIR",
    sandboxed::VAR,
    "BLAZE_CACHE",
    "BLAZE_TRACE",
    "BLAZE_STATS",
    "BLAZE_HOME",
    keyed::CHECK,
];

/// The most a run may leave for the store to keep it.
const MOST_KEPT: usize = 256 << 20;

/// What the script at `real` is, for the key of its run: what blaze built it from
/// ([`own_script::made_of`]), beside the name it was linked under (`real` without `.real`
/// when the wrapper stood in front of it) or at `real`; else the digest of the executable.
fn script_of(real: &Path) -> Option<String> {
    let linked = real.to_str()?.strip_suffix(".real").map(PathBuf::from);
    let built = [linked.as_deref(), Some(real)].into_iter().flatten();
    match built.into_iter().find_map(|named| own_script::made_of(named, real)) {
        Some(key) => Some(format!("built of {key}")),
        None => Some(format!("executable {}", keyed::digest(real.to_str()?))),
    }
}

/// The key of a run of the script `real` in `cwd` with `args` and `env`, its environment in
/// the sandbox, before what it reads ([`keyed`]). With `places`, what names the checkout and
/// the target directory names them by what stands for them, so a run in another checkout or
/// target directory has the same key. `None` when the script cannot be read.
fn run_key(
    real: &Path,
    cwd: &Path,
    args: &[OsString],
    env: &[(String, String)],
    places: Option<&Places>,
) -> Option<[u8; 32]> {
    let mut key = keyed::Key::new(RUNS);
    let text = |bytes: &[u8]| match places {
        Some(places) => places.unplace(bytes),
        None => bytes.to_vec(),
    };
    if places.is_some() {
        key.field(b"places apart");
    }
    key.field(script_of(real)?.as_bytes());
    // The toolchain a script's `RUSTC` answers for: its rustc is named by path, which a
    // toolchain of the same name moved to another release keeps.
    key.field(sysroot_of().as_deref().map(keyed::toolchain).unwrap_or_default().as_bytes());
    key.field(&text(cwd.as_os_str().as_encoded_bytes()));
    for arg in args {
        key.field(&text(arg.as_encoded_bytes()));
    }
    let mut env: Vec<&(String, String)> =
        env.iter().filter(|(name, _)| !UNREAD.contains(&name.as_str())).collect();
    env.sort();
    for (name, value) in env {
        key.field(name.as_bytes());
        key.field(&text(value.as_bytes()));
    }
    Some(key.done())
}

/// The toolchain a run's `RUSTC` answers for: blaze's release, whatever the run was given.
pub fn sysroot_of() -> Option<PathBuf> {
    super::toolchain::sysroot()
}

/// What of `reads`, the files a run of the script `real` in `cwd` read, its kept run is kept
/// by. A file of the script's build, the toolchain (`sysroot`), blaze or its store is left out:
/// the first key has the script and the toolchain, the store keeps nothing across builds of
/// blaze, and what blaze as the script's rustc reads of the store (the standard library's
/// names) is what it would make without it. A directory outside the package counts only as
/// one: a program reaches those on its way to others (git, the loader, a search on PATH).
fn kept_reads(
    reads: Vec<String>,
    real: &Path,
    cwd: &Path,
    sysroot: Option<PathBuf>,
) -> Vec<String> {
    let blaze = std::env::current_exe().ok().and_then(|it| Some(it.parent()?.to_owned()));
    let script = real.parent().map(Path::to_owned);
    let store = super::daemon::store();
    let apart: Vec<PathBuf> = [sysroot, blaze, script, store].into_iter().flatten().collect();
    reads
        .into_iter()
        .filter(|it| !apart.iter().any(|dir| Path::new(it).starts_with(dir)))
        .map(|it| match !Path::new(&it).starts_with(cwd) && Path::new(&it).is_dir() {
            true => keyed::kind(&it),
            false => it,
        })
        .collect()
}

/// The places of one checkout a run of a build script names, each with what stands for it in
/// what the store keeps: the target directory its `OUT_DIR` is below, up to cargo's `build`
/// directory, and its package's directory, where cargo runs it. No path holds a NUL.
#[derive(Debug)]
pub struct Places(Vec<(Vec<u8>, &'static [u8])>);

impl Places {
    const TARGET: &[u8] = b"\0TARGET\0";
    const PACKAGE: &[u8] = b"\0PACKAGE\0";

    pub fn of(env: &[(String, String)]) -> Places {
        let var = |name: &str| {
            let value = env.iter().find(|it| it.0 == name)?.1.trim_end_matches(['/', '\\']);
            (!value.is_empty()).then_some(value)
        };
        let mut places = Vec::new();
        if let Some(out) = var("OUT_DIR")
            && let Some(at) = out.replace('\\', "/").rfind("/build/")
            && at > 0
        {
            places.push((out.as_bytes()[..at].to_vec(), Self::TARGET));
        }
        if let Some(package) = var("CARGO_MANIFEST_DIR") {
            places.push((package.as_bytes().to_vec(), Self::PACKAGE));
        }
        // The longer first: the target directory is often in the package's.
        places.sort_by_key(|it| std::cmp::Reverse(it.0.len()));
        Places(places)
    }

    /// `bytes` with what stands for each place in place of it.
    pub fn unplace(&self, bytes: &[u8]) -> Vec<u8> {
        let mut bytes = bytes.to_vec();
        for (place, mark) in &self.0 {
            bytes = replaced(&bytes, place, mark);
        }
        bytes
    }

    /// `bytes` with each place where [`Places::unplace`] put what stands for it.
    fn place(&self, bytes: &[u8]) -> Vec<u8> {
        let mut bytes = bytes.to_vec();
        for (place, mark) in &self.0 {
            bytes = replaced(&bytes, mark, place);
        }
        bytes
    }

    /// `reads`, a run's read set, with what stands for each place in place of it, for a run in
    /// any checkout or target directory to be found by. `None` when a path holds what stands
    /// for a place already, and so could not be told from one in the place.
    fn unplaced_reads(&self, reads: &[String]) -> Option<Vec<String>> {
        reads
            .iter()
            .map(|it| match self.marked(it.as_bytes()) {
                true => None,
                false => String::from_utf8(self.unplace(it.as_bytes())).ok(),
            })
            .collect()
    }

    /// Whether `bytes` name a place.
    fn named_in(&self, bytes: &[u8]) -> bool {
        self.0.iter().any(|(place, _)| holds(bytes, place))
    }

    /// Whether `bytes` hold what stands for a place, and so could not be told from bytes that
    /// named it.
    fn marked(&self, bytes: &[u8]) -> bool {
        self.0.iter().any(|(_, mark)| holds(bytes, mark))
    }
}

fn holds(bytes: &[u8], part: &[u8]) -> bool {
    !part.is_empty() && bytes.windows(part.len()).any(|it| it == part)
}

/// `bytes` with every `from` in them `to`.
fn replaced(bytes: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut rest = bytes;
    while !from.is_empty()
        && let Some(at) = rest.windows(from.len()).position(|it| it == from)
    {
        out.extend_from_slice(&rest[..at]);
        out.extend_from_slice(to);
        rest = &rest[at + from.len()..];
    }
    out.extend_from_slice(rest);
    out
}

/// What a run of a build script printed and left in its `OUT_DIR`.
#[derive(Debug, PartialEq)]
pub struct Kept {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Each file below `OUT_DIR`, by its path there, whether it is executable, and its bytes.
    files: Vec<(String, bool, Vec<u8>)>,
}

impl Kept {
    /// The run that printed `stdout` and `stderr` and wrote `out_dir`. `None` when it left too
    /// much, or a file there can't be read.
    pub fn of(stdout: &[u8], stderr: &[u8], out_dir: &Path) -> Option<Kept> {
        let mut files = Vec::new();
        let mut size = stdout.len() + stderr.len();
        let mut pending = vec![out_dir.to_owned()];
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir).ok()?.flatten() {
                let path = entry.path();
                let kind = entry.file_type().ok()?;
                if kind.is_dir() {
                    pending.push(path);
                    continue;
                }
                let name = path.strip_prefix(out_dir).ok()?.to_str()?.to_owned();
                let bytes = std::fs::read(&path).ok()?;
                size += bytes.len();
                if size > MOST_KEPT {
                    return None;
                }
                files.push((name, is_executable(&entry.metadata().ok()?), bytes));
            }
        }
        files.sort();
        Some(Kept { stdout: stdout.to_vec(), stderr: stderr.to_vec(), files })
    }

    /// It with what stands for each place in place of it, for a run in any checkout or target
    /// directory to stand for. `None` when a file that is no text names a place, whose bytes
    /// a place of another length would break, or when something it holds could not be told
    /// from what stands for one.
    pub fn unplaced(&self, places: &Places) -> Option<Kept> {
        let texts = [&self.stdout, &self.stderr].into_iter();
        let files = self.files.iter().map(|it| &it.2);
        let names = self.files.iter().map(|it| it.0.as_bytes());
        if texts.chain(files).map(Vec::as_slice).chain(names).any(|it| places.marked(it)) {
            return None;
        }
        let mut files = Vec::with_capacity(self.files.len());
        for (name, executable, bytes) in &self.files {
            let bytes = match places.named_in(bytes) {
                true if std::str::from_utf8(bytes).is_err() => return None,
                true => places.unplace(bytes),
                false => bytes.clone(),
            };
            files.push((name.clone(), *executable, bytes));
        }
        Some(Kept {
            stdout: places.unplace(&self.stdout),
            stderr: places.unplace(&self.stderr),
            files,
        })
    }

    /// What [`Kept::unplaced`] made of a run, with the places of this one.
    pub fn placed(self, places: &Places) -> Kept {
        let files = self.files.into_iter().map(|(name, executable, bytes)| {
            let bytes = if places.marked(&bytes) { places.place(&bytes) } else { bytes };
            (name, executable, bytes)
        });
        Kept {
            stdout: places.place(&self.stdout),
            stderr: places.place(&self.stderr),
            files: files.collect(),
        }
    }

    /// Writes the files it left into `out_dir`, as a run would.
    pub fn restore(&self, out_dir: &Path) -> anyhow::Result<()> {
        for (name, executable, bytes) in &self.files {
            let path = out_dir.join(name);
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
            if *executable {
                make_executable(&path)?;
            }
        }
        Ok(())
    }

    pub fn bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut put = |bytes: &[u8]| {
            out.extend((bytes.len() as u64).to_le_bytes());
            out.extend(bytes);
        };
        put(RUNS.as_bytes());
        put(&self.stdout);
        put(&self.stderr);
        put(&(self.files.len() as u64).to_le_bytes());
        for (name, executable, bytes) in &self.files {
            put(name.as_bytes());
            put(&[u8::from(*executable)]);
            put(bytes);
        }
        out
    }

    pub fn read(bytes: &[u8]) -> Option<Kept> {
        let rest = &mut &*bytes;
        let count = |it: &[u8]| Some(u64::from_le_bytes(it.try_into().ok()?) as usize);
        let text = |it: &[u8]| String::from_utf8(it.to_vec()).ok();
        if take(rest)? != RUNS.as_bytes() {
            return None;
        }
        let (stdout, stderr) = (take(rest)?.to_vec(), take(rest)?.to_vec());
        let mut files = Vec::new();
        for _ in 0..count(take(rest)?)? {
            let name = text(take(rest)?)?;
            let executable = take(rest)? == [1];
            files.push((name, executable, take(rest)?.to_vec()));
        }
        Some(Kept { stdout, stderr, files })
    }
}

/// The next field of `rest`, which is its length and then its bytes.
fn take<'a>(rest: &mut &'a [u8]) -> Option<&'a [u8]> {
    let len = usize::try_from(u64::from_le_bytes(rest.get(..8)?.try_into().ok()?)).ok()?;
    let field = rest.get(8..8usize.checked_add(len)?)?;
    *rest = &rest[8 + len..];
    Some(field)
}

#[cfg(unix)]
pub fn is_executable(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
pub fn is_executable(_: &std::fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
pub fn make_executable(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
}

#[cfg(not(unix))]
pub fn make_executable(_: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn become_it(script: &mut Command) -> anyhow::Result<()> {
    Err(std::os::unix::process::CommandExt::exec(script).into())
}

#[cfg(not(unix))]
fn become_it(script: &mut Command) -> anyhow::Result<()> {
    std::process::exit(script.status()?.code().unwrap_or(1))
}

/// The file cargo kept what the script that wrote `out_dir` printed in, beside it: `output`
/// in cargo's first layout of its build directory, `run/stdout` in the second
/// (`build/<package>/<hash>/out`, the default of nightly cargo since 2026).
pub fn printed(out_dir: &Path) -> Option<PathBuf> {
    let run_dir = out_dir.parent()?;
    let second = run_dir.join("run").join("stdout");
    Some(if second.exists() { second } else { run_dir.join("output") })
}

/// Whether `path` can be in a script's `OUT_DIR`: below `build/<unit>/out` in cargo's first
/// layout or `build/<package>/<hash>/out` in its second, where the objects of a library are
/// too.
pub fn in_out_dir(path: &Path) -> bool {
    let names: Vec<&str> = path.components().filter_map(|c| c.as_os_str().to_str()).collect();
    names.windows(3).any(|it| it[0] == "build" && it[2] == "out")
        || names.windows(4).any(|it| it[0] == "build" && it[3] == "out")
}

/// Whether a script can be run again as cargo ran it into `out_dir`.
pub fn can_rerun(out_dir: &Path) -> bool {
    out_dir.parent().is_some_and(|it| it.join(RAN).exists())
}

/// The directory cargo ran the script that wrote `out_dir` in, which is its package's, when
/// it was written down.
pub fn ran_in(out_dir: &Path) -> Option<PathBuf> {
    let ran = std::fs::read(out_dir.parent()?.join(RAN)).ok()?;
    serde_json::from_slice::<Ran>(&ran).ok().map(|it| it.cwd)
}

/// Whether the script that wrote `out_dir` is as cargo would compile it now: no file rustc
/// read for it (`.d` beside it, named from `workspace`, where cargo runs rustc) is newer than
/// it, and it read no variable while compiled but cargo's own, as cargo set them to run it:
/// cargo compiles it again when one changes.
pub fn compiled_from_what_is_there(out_dir: &Path, workspace: &Path) -> bool {
    let Some(run_dir) = out_dir.parent() else { return false };
    let Ok(ran) = std::fs::read(run_dir.join(RAN)) else { return false };
    let Ok(ran) = serde_json::from_slice::<Ran>(&ran) else { return false };
    let Some(real) = ran.real.to_str() else { return false };
    // Cargo's script was moved aside to `.real`; one blaze compiled (`compile_time.rs`) is run
    // where it was linked.
    let linked = real.strip_suffix(".real").unwrap_or(real);
    let compiled = std::fs::metadata(&ran.real).and_then(|it| it.modified());
    let Ok(read) = std::fs::read_to_string(Path::new(linked).with_extension("d")) else {
        return false;
    };
    let Ok(compiled) = compiled else { return false };
    let read = crate::dep_info::DepInfo::parse(&read);
    // Cargo sets its own from the manifest, which a change of has the checkout loaded again;
    // whoever runs cargo sets the others.
    let set = |name: &str| ran.env.iter().find(|it| it.0 == name).map(|it| it.1.as_str());
    if read
        .env
        .iter()
        .any(|(name, value)| !name.starts_with("CARGO_") || set(name) != value.as_deref())
    {
        return false;
    }
    !read.files.is_empty()
        && read.files.iter().all(|it| {
            let written = std::fs::metadata(workspace.join(it)).and_then(|it| it.modified());
            written.is_ok_and(|it| it <= compiled)
        })
}

/// Runs the script that wrote `out_dir` again, as cargo ran it but into a directory of its
/// own: what cargo's holds is read by the compilers of a build under way, and cargo
/// compiles the package again when a file there is newer than it was. `Ok` when the script
/// printed and wrote what it did then; `Err` is what is different, which a script that
/// writes the name of its directory into a file always is.
pub fn rerun_is_same(out_dir: &Path) -> Result<(), String> {
    // A file written as the script reads it can have a time a little before now: the
    // kernel stamps files with a clock that lags by up to a tick.
    let began = SystemTime::now() - 2 * file_clock_tick();
    rerun_matches(out_dir, out_dir)?;
    if let Some(run_dir) = out_dir.parent()
        && let Ok(checked) = std::fs::File::create(run_dir.join(CHECKED))
    {
        let _ = checked.set_modified(began);
    }
    Ok(())
}

/// How far behind the clock the kernel stamps a file it writes can be: Linux takes the time
/// of the last tick.
#[cfg(target_os = "linux")]
fn file_clock_tick() -> Duration {
    let mut tick = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `tick` is a timespec to write to.
    match unsafe { libc::clock_getres(libc::CLOCK_REALTIME_COARSE, &mut tick) } {
        0 => Duration::new(tick.tv_sec as u64, tick.tv_nsec as u32).max(Duration::from_millis(1)),
        _ => Duration::from_millis(20),
    }
}

/// Windows stamps files with the system time, which moves in steps of about 16 ms.
#[cfg(windows)]
fn file_clock_tick() -> Duration {
    Duration::from_millis(16)
}

#[cfg(not(any(target_os = "linux", windows)))]
fn file_clock_tick() -> Duration {
    Duration::from_millis(1)
}

/// Whether the script that wrote `out_dir` was run again by [`rerun_is_same`], and printed
/// and wrote what it had, after `newest`: when nothing it reads is newer, a run now is the
/// same again.
pub fn checked_after(out_dir: &Path, newest: SystemTime) -> bool {
    let checked = out_dir.parent().map(|it| it.join(CHECKED));
    let checked = checked.and_then(|it| std::fs::metadata(it).and_then(|it| it.modified()).ok());
    checked.is_some_and(|it| newest < it)
}

/// Whether the run of a script that wrote `out_dir` printed and wrote what the run that wrote
/// `other` did, but for the name of its directory.
pub fn same_run(out_dir: &Path, other: &Path) -> bool {
    let printed = |dir: &Path| std::fs::read(printed(dir)?).ok();
    let (Some(ours), Some(theirs)) = (printed(out_dir), printed(other)) else { return false };
    let (Some(named), Some(names)) = (other.to_str(), out_dir.to_str()) else { return false };
    String::from_utf8_lossy(&ours).replace(names, named).as_bytes() == theirs
        && digest_of(out_dir) == digest_of(other)
}

/// Runs the script that wrote `out_dir` again as [`rerun_is_same`] does, and compares what
/// it prints and writes with what a run of the same script left in `other`: another build of
/// the package, which ran it with the same source and inputs.
///
/// A script that said nothing of what it reads, whose run cargo's sandbox saw read only what
/// is as it was ([`script_sandbox::reads_unchanged`]), is not run: a run now would read the
/// same and so print and write the same.
pub fn rerun_matches(out_dir: &Path, other: &Path) -> Result<(), String> {
    if script_sandbox::reads_unchanged(out_dir) && (out_dir == other || same_run(out_dir, other)) {
        return Ok(());
    }
    let run_dir = out_dir.parent().ok_or("a build script without a directory")?;
    let other_dir = other.parent().ok_or("a build script without a directory")?;
    let read = |dir: &Path, name: &str| {
        std::fs::read(dir.join(name)).map_err(|it| format!("{name}: {it}"))
    };
    let ran: Ran = serde_json::from_slice(&read(run_dir, RAN)?).map_err(|it| it.to_string())?;
    let printed = printed(other).ok_or("a build script without a directory")?;
    let printed = std::fs::read(&printed).map_err(|it| {
        let name = printed.strip_prefix(other_dir).unwrap_or(&printed);
        format!("{}: {it}", name.display())
    })?;
    let again_dir = run_dir.join(format!("blaze-out-{}", std::process::id()));
    let scratch = run_dir.join(format!("blaze-again-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&again_dir);
    std::fs::create_dir(&again_dir).map_err(|it| it.to_string())?;
    let same = rerun_into(&ran, &again_dir, &scratch, other, &printed);
    let _ = std::fs::remove_dir_all(&again_dir);
    let _ = std::fs::remove_dir_all(&scratch);
    same
}

/// Runs the script as cargo ran it, but into `again_dir`, in the sandbox cargo's run was in
/// ([`script_sandbox::run`]), with its spec and record in `scratch`.
fn rerun_into(
    ran: &Ran,
    again_dir: &Path,
    scratch: &Path,
    out_dir: &Path,
    printed: &[u8],
) -> Result<(), String> {
    let again_text = again_dir.to_str().ok_or("has a directory whose name is not text")?;
    let env: Vec<(String, String)> = ran
        .env
        .iter()
        .map(|(name, value)| match name == "OUT_DIR" {
            true => (name.clone(), again_text.to_owned()),
            false => (name.clone(), value.clone()),
        })
        .collect();
    let script = script_sandbox::Script {
        real: &ran.real,
        args: &[],
        cwd: &ran.cwd,
        env: &env,
        out_dir: again_dir,
        scratch,
        declared: &[],
    };
    let plan = script_sandbox::plan(&script).map_err(|it| format!("{it:#}"))?;
    // Given what cargo's run said it reads, as `run_as_cargo_does` gave it.
    let declared = script_sandbox::not_given(printed, &env, &plan);
    let script = script_sandbox::Script { declared: &declared, ..script };
    let plan = script_sandbox::plan(&script).map_err(|it| format!("{it:#}"))?;
    let again =
        script_sandbox::run(&script, &plan, Stdio::null()).map_err(|it| format!("{it:#}"))?;
    let again = again.output;
    if !again.status.success() {
        return Err(format!("failed ({})", again.status));
    }
    let (Some(named), Some(names)) = (out_dir.to_str(), again_dir.to_str()) else {
        return Err("has a directory whose name is not text".to_owned());
    };
    if String::from_utf8_lossy(&again.stdout).replace(names, named).as_bytes() != printed {
        return Err("printed something else".to_owned());
    }
    if digest_of(again_dir) != digest_of(out_dir) {
        let names = different_files(again_dir, out_dir);
        let more = names.len().saturating_sub(5);
        let mut names = names.into_iter().take(5).collect::<Vec<_>>().join(", ");
        if more > 0 {
            names = format!("{names} and {more} more");
        }
        return Err(format!("wrote something else ({names})"));
    }
    Ok(())
}

/// A hash of the names, as they are below `dir`, and the contents of the files there.
fn digest_of(dir: &Path) -> Vec<u8> {
    let mut hash = Sha256::new();
    let mut pending = vec![dir.to_owned()];
    while let Some(below) = pending.pop() {
        let entries = std::fs::read_dir(&below).into_iter().flatten().flatten();
        let mut paths: Vec<PathBuf> = entries.map(|it| it.path()).collect();
        paths.sort();
        for path in paths {
            let name = path.strip_prefix(dir).unwrap_or(&path);
            hash.update(name.as_os_str().as_encoded_bytes());
            match path.is_dir() {
                true => pending.push(path),
                false => hash.update(std::fs::read(&path).unwrap_or_default()),
            }
        }
    }
    hash.finalize().to_vec()
}

/// The files below `dir` and `other` whose contents differ, or that only one of them has.
fn different_files(dir: &Path, other: &Path) -> Vec<String> {
    let files = |root: &Path| {
        let mut files = BTreeMap::new();
        let mut pending = vec![root.to_owned()];
        while let Some(below) = pending.pop() {
            let entries = std::fs::read_dir(&below).into_iter().flatten().flatten();
            let mut paths: Vec<PathBuf> = entries.map(|it| it.path()).collect();
            paths.sort();
            for path in paths {
                match path.is_dir() {
                    true => pending.push(path),
                    false => {
                        let name = path.strip_prefix(root).unwrap_or(&path).to_owned();
                        files.insert(name, std::fs::read(&path).unwrap_or_default());
                    }
                }
            }
        }
        files
    };
    let ours = files(dir);
    let theirs = files(other);
    ours.keys()
        .chain(theirs.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|name| ours.get(*name) != theirs.get(*name))
        .map(|name| name.to_string_lossy().into_owned())
        .collect()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A run directory as cargo and `blaze-engine build-script` leave one, for a script that
    /// writes `wrote` and prints where.
    fn ran(name: &str, wrote: &str) -> PathBuf {
        let run_dir =
            std::env::temp_dir().join(format!("blaze-script-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&run_dir);
        let out_dir = run_dir.join("out");
        std::fs::create_dir_all(&out_dir).unwrap();
        let real = run_dir.join("script");
        let lines =
            "#!/bin/sh\nprintf %s \"$WROTE\" > \"$OUT_DIR/made.rs\"\necho \"cargo:in=$OUT_DIR\"\n";
        // A process a test thread starts while this one has the script open to write holds it
        // open too until that process execs, and the script is refused meanwhile ("text file
        // busy"): another process writes it.
        let written = Command::new("sh")
            .args(["-c", "printf %s \"$1\" > \"$0\" && chmod 755 \"$0\""])
            .arg(&real)
            .arg(lines)
            .status()
            .unwrap();
        assert!(written.success());
        let env = vec![
            ("OUT_DIR".to_owned(), out_dir.to_str().unwrap().to_owned()),
            ("WROTE".to_owned(), wrote.to_owned()),
            ("PATH".to_owned(), std::env::var("PATH").unwrap()),
            // The test's executable is no `blaze sandbox`.
            (sandboxed::VAR.to_owned(), "off".to_owned()),
        ];
        let ran = Ran { real, cwd: run_dir.clone(), env };
        std::fs::write(run_dir.join(RAN), serde_json::to_vec(&ran).unwrap()).unwrap();
        std::fs::write(run_dir.join("output"), format!("cargo:in={}\n", out_dir.display()))
            .unwrap();
        out_dir
    }

    #[test]
    fn a_rerun_leaves_what_cargo_has_alone() {
        let out_dir = ran("same", "fn made() {}");
        let made = out_dir.join("made.rs");
        std::fs::write(&made, "fn made() {}").unwrap();
        let written = std::fs::metadata(&made).unwrap().modified().unwrap();
        assert_eq!(rerun_is_same(&out_dir), Ok(()));
        assert_eq!(std::fs::metadata(&made).unwrap().modified().unwrap(), written);
        let left: Vec<_> =
            std::fs::read_dir(out_dir.parent().unwrap()).unwrap().flatten().collect();
        assert_eq!(left.len(), 5, "{left:?}");
        let before = SystemTime::now() - Duration::from_secs(1);
        assert!(checked_after(&out_dir, before));
        assert!(!checked_after(&out_dir, SystemTime::now()), "a file written since");
    }

    #[test]
    fn a_run_is_the_same_as_another_but_for_its_directory() {
        let out_dir = ran("run", "fn made() {}");
        let other = ran("run-other", "fn made() {}");
        std::fs::write(out_dir.join("made.rs"), "fn made() {}").unwrap();
        std::fs::write(other.join("made.rs"), "fn made() {}").unwrap();
        assert!(same_run(&out_dir, &other));
        std::fs::write(other.join("made.rs"), "fn made_before() {}").unwrap();
        assert!(!same_run(&out_dir, &other));
    }

    #[test]
    fn a_rerun_is_compared_with_what_the_other_run_left() {
        let out_dir = ran("matches", "fn made() {}");
        let other = ran("matches-other", "fn made() {}");
        std::fs::write(other.join("made.rs"), "fn made() {}").unwrap();
        assert_eq!(rerun_matches(&out_dir, &other), Ok(()));
        std::fs::write(other.join("made.rs"), "fn made_for_the_load() {}").unwrap();
        assert_eq!(
            rerun_matches(&out_dir, &other),
            Err("wrote something else (made.rs)".to_owned())
        );
    }

    #[test]
    fn a_kept_run_is_what_it_printed_and_wrote() {
        let out_dir = ran("kept", "fn made() {}");
        let cwd = out_dir.parent().unwrap().to_owned();
        std::fs::create_dir_all(out_dir.join("deep")).unwrap();
        std::fs::write(out_dir.join("deep/made.rs"), "fn made() {}").unwrap();
        let stdout = b"cargo::rerun-if-changed=read.txt\ncargo:rustc-cfg=kept\n";
        let kept = Kept::of(stdout, b"a warning", &out_dir).unwrap();
        let bytes = kept.bytes();
        assert_eq!(Kept::read(&bytes), Some(kept));
        let kept = Kept::read(&bytes).unwrap();
        let again = cwd.join("again");
        kept.restore(&again).unwrap();
        assert_eq!(std::fs::read_to_string(again.join("deep/made.rs")).unwrap(), "fn made() {}");
    }

    #[test]
    fn a_run_is_kept_by_what_it_read_but_the_toolchain_and_its_script() {
        let root = std::env::temp_dir().join(format!("blaze-kept-reads-{}", std::process::id()));
        let at = |it: &str| root.join(it).to_str().unwrap().to_owned();
        for dir in ["pkg/src", "sysroot/bin", "sysroot/lib", "build/s-1/out", "home/.git"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(root.join("pkg/src/a.rs"), "").unwrap();
        let reads =
            ["pkg/src", "pkg/src/a.rs", "sysroot/lib", "build/s-1/out/x", "home", "home/.git"];
        let reads = reads.map(at).to_vec();
        let real = root.join("build/s-1/out/build_script_build.real");
        let kept = kept_reads(reads, &real, &root.join("pkg"), Some(root.join("sysroot")));
        let kind = |it: &str| keyed::kind(&at(it));
        assert_eq!(kept, [at("pkg/src"), at("pkg/src/a.rs"), kind("home"), kind("home/.git")]);
        assert_eq!(keyed::digest(&kind("home")), "d");
        assert_eq!(keyed::path_of(&kind("home")), at("home"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn what_rerun_if_changed_names_is_read_every_file_of_a_directory() {
        let out_dir = ran("watched", "fn made() {}");
        let cwd = out_dir.parent().unwrap().to_owned();
        std::fs::create_dir_all(cwd.join("src/deep")).unwrap();
        std::fs::write(cwd.join("src/deep/a.c"), "int a;").unwrap();
        std::fs::write(cwd.join("build.rs"), "fn main() {}").unwrap();
        let stdout = b"cargo:rerun-if-changed=build.rs\ncargo::rerun-if-changed=src\n";
        let mut paths = watched(stdout, &cwd);
        paths.sort();
        let at = |it: &str| cwd.join(it).to_str().unwrap().to_owned();
        assert_eq!(paths, [at("build.rs"), at("src"), at("src/deep"), at("src/deep/a.c")]);
    }

    #[test]
    fn a_run_in_another_checkout_has_its_key_and_what_it_left_names_that_checkout() {
        let root = std::env::temp_dir().join(format!("blaze-script-places-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let real = root.join("script");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&real, "a script").unwrap();
        let checkout = |name: &str| {
            let at = root.join(name);
            let (package, out) = (at.join("pkg"), at.join("target/debug/build/pkg-0123/out"));
            std::fs::create_dir_all(&out).unwrap();
            std::fs::create_dir_all(&package).unwrap();
            std::fs::write(package.join("read.txt"), "one").unwrap();
            let env = vec![
                ("CARGO_MANIFEST_DIR".to_owned(), package.to_str().unwrap().to_owned()),
                ("OUT_DIR".to_owned(), out.to_str().unwrap().to_owned()),
                ("PATH".to_owned(), "/bin".to_owned()),
                ("PWD".to_owned(), at.to_str().unwrap().to_owned()),
            ];
            (package, out, env)
        };
        let (package, out, env) = checkout("one");
        let (other_package, other_out, other_env) = checkout("other");
        let (places, other_places) = (Places::of(&env), Places::of(&other_env));
        let key = |cwd: &Path, env: &[(String, String)], places: Option<&Places>| {
            run_key(&real, cwd, &[], env, places)
        };
        assert_eq!(
            key(&package, &env, Some(&places)),
            key(&other_package, &other_env, Some(&other_places))
        );
        assert_ne!(key(&package, &env, None), key(&other_package, &other_env, None));
        let read = |package: &Path, out: &Path| {
            let reads = [package.join("read.txt"), out.join("x.rs"), PathBuf::from("/etc/hosts")];
            reads.map(|it| it.to_str().unwrap().to_owned()).to_vec()
        };
        let reads = places.unplaced_reads(&read(&package, &out));
        assert!(reads.is_some(), "a run that read its package's files is kept for every checkout");
        assert_eq!(reads, other_places.unplaced_reads(&read(&other_package, &other_out)));
        let marked = String::from_utf8(places.unplace(package.as_os_str().as_encoded_bytes()));
        assert_eq!(places.unplaced_reads(&[marked.unwrap()]), None);
        let named = |out: &Path, package: &Path| Kept {
            stdout: format!(
                "cargo:rustc-link-search=native={}\ncargo:rerun-if-changed={}\n",
                out.display(),
                package.join("read.txt").display()
            )
            .into_bytes(),
            stderr: Vec::new(),
            files: vec![
                (
                    "made.rs".to_owned(),
                    false,
                    format!("include!(\"{}/x.rs\");", out.display()).into_bytes(),
                ),
                ("lib.a".to_owned(), false, vec![0xff, 0, 1]),
            ],
        };
        let unplaced = named(&out, &package).unplaced(&places).unwrap();
        let read = Kept::read(&unplaced.bytes()).unwrap();
        let placed = read.placed(&other_places);
        assert_eq!(placed, named(&other_out, &other_package));
        let mut binary = named(&out, &package);
        binary.files[1].2 = [&[0xff][..], out.to_str().unwrap().as_bytes()].concat();
        assert_eq!(binary.unplaced(&places), None, "a file that is no text naming the place");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Has the script of `out_dir` be one rustc compiled from `sources`, as `.d` says.
    fn compiled(out_dir: &Path, sources: &str) {
        let run_dir = out_dir.parent().unwrap();
        let mut ran: Ran =
            serde_json::from_slice(&std::fs::read(run_dir.join(RAN)).unwrap()).unwrap();
        let linked = run_dir.join("build_script_build-0123");
        if ran.real != linked.with_extension("real") {
            std::fs::copy(&ran.real, linked.with_extension("real")).unwrap();
        }
        ran.real = linked.with_extension("real");
        // A copy on macOS keeps the time of the file it was copied from.
        let real = std::fs::File::options().write(true).open(&ran.real).unwrap();
        real.set_modified(SystemTime::now()).unwrap();
        std::fs::write(run_dir.join(RAN), serde_json::to_vec(&ran).unwrap()).unwrap();
        std::fs::write(linked.with_extension("d"), format!("{}: {sources}\n", linked.display()))
            .unwrap();
    }

    #[test]
    fn a_script_newer_than_its_source_is_as_cargo_would_compile_it() {
        let out_dir = ran("fresh", "fn made() {}");
        let source = out_dir.parent().unwrap().join("build source.rs");
        std::fs::write(&source, "fn main() {}").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        compiled(&out_dir, "build\\ source.rs");
        let workspace = out_dir.parent().unwrap();
        assert!(compiled_from_what_is_there(&out_dir, workspace));
        assert!(
            !compiled_from_what_is_there(&out_dir, Path::new("/elsewhere")),
            "a source that is not there"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&source, "fn main() { }").unwrap();
        assert!(!compiled_from_what_is_there(&out_dir, workspace));
    }

    #[test]
    fn a_script_blaze_compiled_newer_than_its_source_is_as_cargo_would_compile_it() {
        let out_dir = ran("own", "fn made() {}");
        let run_dir = out_dir.parent().unwrap();
        let source = run_dir.join("build.rs");
        std::fs::write(&source, "fn main() {}").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let at = run_dir.join(RAN);
        let mut ran: Ran = serde_json::from_slice(&std::fs::read(&at).unwrap()).unwrap();
        let exe = run_dir.join("build-script-build");
        std::fs::copy(&ran.real, &exe).unwrap();
        let written = std::fs::File::options().write(true).open(&exe).unwrap();
        written.set_modified(SystemTime::now()).unwrap();
        ran.real = exe.clone();
        std::fs::write(&at, serde_json::to_vec(&ran).unwrap()).unwrap();
        let info = format!("{}: {}\n", exe.display(), source.display());
        std::fs::write(exe.with_extension("d"), info).unwrap();
        assert!(compiled_from_what_is_there(&out_dir, run_dir));
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&source, "fn main() { }").unwrap();
        assert!(!compiled_from_what_is_there(&out_dir, run_dir));
    }

    #[test]
    fn a_script_that_read_a_variable_or_has_no_dep_info_is_cargos() {
        let out_dir = ran("env", "fn made() {}");
        let workspace = out_dir.parent().unwrap();
        assert!(!compiled_from_what_is_there(&out_dir, workspace), "no .d");
        let source = out_dir.parent().unwrap().join("build.rs");
        std::fs::write(&source, "fn main() {}").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        compiled(&out_dir, "build.rs\n\n# env-dep:SOME_VARIABLE=1");
        assert!(!compiled_from_what_is_there(&out_dir, workspace));
        // One of cargo's own is as cargo set it to run the script, or not.
        let at = workspace.join(RAN);
        let mut ran: Ran = serde_json::from_slice(&std::fs::read(&at).unwrap()).unwrap();
        ran.env.push(("CARGO_PKG_VERSION".to_owned(), "1.0.0".to_owned()));
        std::fs::write(&at, serde_json::to_vec(&ran).unwrap()).unwrap();
        compiled(&out_dir, "build.rs\n\n# env-dep:CARGO_PKG_VERSION=1.0.0");
        assert!(compiled_from_what_is_there(&out_dir, workspace));
        compiled(&out_dir, "build.rs\n\n# env-dep:CARGO_PKG_VERSION=2.0.0");
        assert!(!compiled_from_what_is_there(&out_dir, workspace));
    }

    /// cargo's second layout of its build directory keeps what a script printed in
    /// `run/stdout`, not in `output`.
    #[test]
    fn a_run_in_the_second_layout_is_read_from_run_stdout() {
        let out_dir = ran("second", "fn made() {}");
        let run_dir = out_dir.parent().unwrap();
        std::fs::create_dir(run_dir.join("run")).unwrap();
        std::fs::rename(run_dir.join("output"), run_dir.join("run/stdout")).unwrap();
        std::fs::write(out_dir.join("made.rs"), "fn made() {}").unwrap();
        assert_eq!(printed(&out_dir), Some(run_dir.join("run/stdout")));
        assert_eq!(rerun_is_same(&out_dir), Ok(()));
        let other = ran("second-other", "fn made() {}");
        std::fs::write(other.join("made.rs"), "fn made() {}").unwrap();
        assert_eq!(rerun_matches(&out_dir, &other), Ok(()));
        assert!(same_run(&out_dir, &other));
        assert!(in_out_dir(Path::new("/t/debug/build/app/0123/out/made.rs")));
        assert!(in_out_dir(Path::new("/t/debug/build/app-0123/out/made.rs")));
        assert!(!in_out_dir(Path::new("/t/debug/deps/app-0123.d")));
    }

    #[test]
    fn a_rerun_names_each_file_that_differs() {
        let out_dir = ran("different", "fn made() {}");
        let other = ran("different-other", "fn made() {}");
        std::fs::write(out_dir.join("made.rs"), "fn made() {}").unwrap();
        std::fs::write(other.join("made.rs"), "fn made_other() {}").unwrap();
        std::fs::write(out_dir.join("also.rs"), "fn also() {}").unwrap();
        std::fs::write(other.join("also.rs"), "fn also_other() {}").unwrap();
        assert_eq!(
            rerun_matches(&out_dir, &other),
            Err("wrote something else (also.rs, made.rs)".to_owned())
        );
    }

    #[test]
    fn a_rerun_that_writes_something_else_is_told() {
        let out_dir = ran("other", "fn made() {}");
        std::fs::write(out_dir.join("made.rs"), "fn made_before() {}").unwrap();
        assert_eq!(rerun_is_same(&out_dir), Err("wrote something else (made.rs)".to_owned()));
        assert_eq!(std::fs::read(out_dir.join("made.rs")).unwrap(), b"fn made_before() {}");
    }
}
