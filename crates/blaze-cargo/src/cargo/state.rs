//! What a cargo build left for the builds after it: the programs and where cargo has them,
//! what the daemon is asked to build each from, and everything a change to which makes the
//! next build cargo's again.

use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io::Write as _,
    path::{Path, PathBuf},
};

use anyhow::Context as _;
use cargo_metadata::{BuildScript, Metadata, Package};
use serde::{Deserialize, Serialize};

use super::{
    cli::{Invocation, worker_env},
    daemon::{self, Checkout, WorkerConfig},
    home, package, script,
    units::{stamp_now, stamp_of},
};

/// The state of one build of a checkout: one command with one set of flags.
#[derive(Serialize, Deserialize)]
pub struct State {
    /// The version of the blaze that wrote it. Another one reads nothing of it.
    version: String,
    pub key: String,
    pub root: PathBuf,
    /// When the cargo build this is the state of started.
    built_at: u128,
    /// The profile, as cargo's `Finished` line names it.
    pub profile: String,
    pub programs: Vec<Program>,
    guards: Vec<Guard>,
    /// What the daemon is asked for: the programs that are not unit tests, and the ones
    /// that are.
    pub checkouts: Vec<Checkout>,
    /// Why every build of this is cargo's, when blaze has no part in it.
    pub refusal: Option<String>,
    /// Whether a library that was built has doc tests.
    pub doc_tests: bool,
    pub counts: Counts,
    /// The `[env]` of cargo's configuration, which cargo sets for what it runs.
    #[serde(default)]
    pub configured: Vec<Configured>,
}

/// A variable of the `[env]` tables of cargo's configuration. Cargo sets it for a program it
/// runs where the environment has none, or always when it is `force`d.
#[derive(Serialize, Deserialize, Clone)]
pub struct Configured {
    pub name: String,
    pub value: String,
    pub force: bool,
}

/// How many builds the daemon served and how many went to cargo, by the reason.
#[derive(Serialize, Deserialize, Default)]
pub struct Counts {
    pub served: u64,
    pub cargo: BTreeMap<String, u64>,
}

/// An executable cargo built.
#[derive(Serialize, Deserialize, Clone)]
pub struct Program {
    pub name: String,
    pub kind: Kind,
    pub package: String,
    /// Whether it is built as a test: `cargo test` runs it.
    pub test: bool,
    /// The root file of the target.
    pub source: PathBuf,
    /// The directory of the package, which a test runs in.
    pub package_dir: PathBuf,
    /// Where cargo has the executable.
    pub executable: PathBuf,
    /// The other names cargo has for the executable: the file rustc wrote, when
    /// `executable` is cargo's hard link to it. A cargo that finds the two apart links
    /// them again, under whoever is running the program.
    #[serde(default)]
    names: Vec<PathBuf>,
    /// A hard link to the executable rustc linked (a clone on macOS), to give cargo back
    /// before it builds.
    cargos: PathBuf,
    /// The directory of the record of the unit rustc linked it from, when it is known.
    #[serde(default)]
    unit: Option<PathBuf>,
    /// Where it is among the programs of its checkout in `checkouts`.
    pub asked_as: Option<(usize, usize)>,
    /// Why the daemon is never asked for it, when it is not.
    left_to_cargo: Option<String>,
    /// The source files of the workspace it is compiled from, its libraries' too. While
    /// none is written, the executable cargo built is the program.
    inputs: Vec<PathBuf>,
    /// What cargo sets for the program when it runs it.
    pub env: Vec<(String, String)>,
    /// Whether it is its package's `default-run`: what `cargo run` runs when it names no
    /// program.
    #[serde(default)]
    pub default_run: bool,
}

impl Program {
    /// A test the daemon built whole, the program `asked_as` of its checkouts, which runs in
    /// `package_dir` with `env`.
    #[expect(clippy::too_many_arguments)]
    pub fn of_whole(
        name: String,
        kind: Kind,
        package: String,
        source: PathBuf,
        package_dir: PathBuf,
        executable: PathBuf,
        asked_as: (usize, usize),
        env: Vec<(String, String)>,
    ) -> Program {
        Program {
            name,
            kind,
            package,
            test: true,
            source,
            package_dir,
            executable,
            names: Vec::new(),
            cargos: PathBuf::new(),
            unit: None,
            asked_as: Some(asked_as),
            left_to_cargo: None,
            inputs: Vec::new(),
            env,
            default_run: false,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Kind {
    Lib,
    Bin,
    Example,
    Test,
    Bench,
}

/// Something cargo acts on that is not source the daemon compiles. When one does not hold
/// the build is cargo's.
#[derive(Serialize, Deserialize)]
enum Guard {
    /// Files that are to be as they were when cargo built: there or not, and not written
    /// since.
    Files {
        what: String,
        files: Vec<(PathBuf, bool)>,
    },
    /// The files of the libraries a unit test is linked with as rustc built them, without
    /// `cfg(test)`, each there or not, and when it was there, when it was written and its
    /// [`without_tests`]. Never why a build is cargo's: an edit to one is the daemon's. One in
    /// which only the tests changed leaves what rustc built of it as it is, so `cargo` (cargo's
    /// dep-info of that) is made newer than the file, and cargo does not build it again.
    BesideTests {
        files: Vec<(PathBuf, Option<(u128, String)>)>,
        cargo: Vec<PathBuf>,
    },
    /// Files as rustc read them: there or not, and each not written since cargo last ran
    /// rustc for the first unit that read it, when that is known, or since cargo built.
    /// What runs during the build writes sources of units cargo builds after it: biome's
    /// build scripts touch a file of their crate's, which is no change to cargo.
    Read {
        what: String,
        files: Vec<(PathBuf, bool, Option<u128>)>,
    },
    /// A directory with nothing in it written since cargo built.
    Tree {
        what: String,
        dir: PathBuf,
    },
    Env {
        name: String,
        value: Option<String>,
    },
    /// A file cargo writes itself while it builds, `Cargo.lock`: as cargo left it, by when
    /// it was last written, or not there.
    AsLeft {
        file: PathBuf,
        written: Option<u128>,
    },
    /// A build script that cargo runs again when one of `reruns_for` does not hold, and
    /// that the daemon then runs itself: the build is cargo's when the script prints or
    /// writes something else than it did.
    Script {
        package: String,
        out_dir: PathBuf,
        reruns_for: Vec<Guard>,
        checked_at: u128,
    },
}

impl Guard {
    /// What changed, when something did.
    fn broken(&mut self, built_at: u128) -> Option<String> {
        match self {
            Guard::Script { package, out_dir, reruns_for, checked_at } => {
                let since = built_at.max(*checked_at);
                reruns_for.iter_mut().find_map(|it| it.broken(since))?;
                let now = stamp_now();
                match script::rerun_is_same(out_dir) {
                    Ok(()) => {
                        *checked_at = now;
                        None
                    }
                    Err(what) => Some(format!("the build script of `{package}` {what}")),
                }
            }
            Guard::Files { what, files } => {
                let changed = files.iter().find(|(file, was_there)| {
                    match std::fs::metadata(file).and_then(|it| it.modified()) {
                        Ok(written) => !was_there || stamp_of(written) > built_at,
                        Err(_) => *was_there,
                    }
                })?;
                Some(format!("{what} {} changed", changed.0.display()))
            }
            Guard::Read { what, files } => {
                let changed =
                    files.iter().find(|(file, was_there, read_at)| match written_at(file) {
                        Some(written) => !was_there || written > read_at.unwrap_or(built_at),
                        None => *was_there,
                    })?;
                Some(format!("{what} {} changed", changed.0.display()))
            }
            Guard::BesideTests { files, cargo } => {
                let mut tests_only = None;
                for (file, was) in files.iter() {
                    match (written_at(file), was) {
                        (Some(now), Some((then, print))) if *then <= built_at && now > built_at => {
                            match without_tests(file).as_ref() == Some(print) {
                                true => tests_only = tests_only.max(Some(now)),
                                false => return None,
                            }
                        }
                        (None, None) => {}
                        (Some(now), Some(_)) if now <= built_at => {}
                        _ => return None,
                    }
                }
                for file in cargo.iter().filter(|it| written_at(it) < tests_only) {
                    let file = std::fs::File::options().write(true).open(file);
                    _ = file.and_then(|it| it.set_modified(std::time::SystemTime::now()));
                }
                None
            }
            Guard::Tree { what, dir } => {
                let changed = newer_in(dir, built_at)?;
                Some(format!("{what} and {} changed", changed.display()))
            }
            Guard::AsLeft { file, written } => {
                (written_at(file) != *written).then(|| format!("{} changed", file.display()))
            }
            Guard::Env { name, value } => (std::env::var(&*name).ok() != *value)
                .then(|| format!("the environment variable {name} changed")),
        }
    }
}

/// A hash of the Rust source in `file` with every item marked `#[test]` or `#[cfg(test)]`
/// blanked, and the blanks it ends with dropped: the same while only those change and every
/// other token keeps its line and column. A file that isn't Rust, one `include!`d or
/// `include_str!`d, is hashed as it is. `None` when it can't be read.
fn without_tests(file: &Path) -> Option<String> {
    use sha2::Digest as _;
    use syntax::{AstNode as _, ast};
    let text = std::fs::read_to_string(file).ok()?;
    if file.extension().is_none_or(|it| it != "rs") {
        let hash = sha2::Sha256::digest(&text);
        return Some(crate::content::hex(&hash[..16]));
    }
    let parse = syntax::SourceFile::parse(&text, syntax::Edition::CURRENT);
    let mut bytes = text.clone().into_bytes();
    let only_tests = |item: &ast::Item| {
        ast::HasAttrs::attrs(item).any(|attr| {
            let attr: String =
                attr.syntax().text().to_string().chars().filter(|it| !it.is_whitespace()).collect();
            attr == "#[test]" || attr == "#[cfg(test)]"
        })
    };
    for item in parse.tree().syntax().descendants().filter_map(ast::Item::cast) {
        if only_tests(&item) {
            let range = item.syntax().text_range();
            for byte in &mut bytes[usize::from(range.start())..usize::from(range.end())] {
                if *byte != b'\n' {
                    *byte = b' ';
                }
            }
        }
    }
    let blanked = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = blanked.lines().map(str::trim_end).collect();
    let kept = lines.len() - lines.iter().rev().take_while(|it| it.is_empty()).count();
    let hash = sha2::Sha256::digest(lines[..kept].join("\n"));
    Some(crate::content::hex(&hash[..16]))
}

/// When the file was last written, as a stamp. `None` when it is not there.
fn written_at(file: &Path) -> Option<u128> {
    std::fs::metadata(file).and_then(|it| it.modified()).ok().map(stamp_of)
}

/// A file below `dir` that was written after `since`. What cargo and git keep there is not
/// the package's.
fn newer_in(dir: &Path, since: u128) -> Option<PathBuf> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let (path, name) = (entry.path(), entry.file_name());
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            if name != "target"
                && name != ".git"
                && let Some(found) = newer_in(&path, since)
            {
                return Some(found);
            }
        } else if entry.metadata().and_then(|it| it.modified()).is_ok_and(|it| stamp_of(it) > since)
        {
            return Some(path);
        }
    }
    None
}

/// What the checker of `blaze check` with the flags of `asked` loads: the workspace
/// as a build of those flags has it, with the crates of every package of it. A flag that
/// picks the tests checks them as `cargo check --tests` does, with `cfg(test)`.
pub fn checked(asked: &Invocation) -> anyhow::Result<daemon::Ahead> {
    if let Some(why) = other_toolchain() {
        anyhow::bail!(why);
    }
    let metadata = metadata(asked);
    if !asked.typed("--ignore-rust-version")
        && let Some(why) = super::toolchain::too_new(&metadata.workspace_packages())
    {
        anyhow::bail!(why);
    }
    let root = metadata.workspace_root.clone().into_std_path_buf();
    let project =
        project(asked, &std::env::current_dir()?, &metadata).map_err(anyhow::Error::msg)?;
    let crates: Vec<PathBuf> = metadata
        .workspace_packages()
        .iter()
        .flat_map(|it| &it.targets)
        .filter(|it| !it.is_custom_build())
        .map(|it| it.src_path.clone().into_std_path_buf())
        .collect();
    anyhow::ensure!(!crates.is_empty(), "the workspace has no crate to check");
    let config = WorkerConfig {
        repository: repository(&root),
        project: project.strip_prefix(&root).unwrap_or(Path::new("")).to_owned(),
        cfg_test: asked.flags.dev_targets,
        selection: asked.selection(),
        env: worker_env(),
        command_env: BTreeMap::new(),
        whole: false,
        tests_told: None,
    };
    Ok(daemon::Ahead { config, crates, root, targets: asked.target_flags() })
}

impl State {
    /// The file in the record of a unit that names the builds its program is in, a key a
    /// line.
    pub const STATES: &str = "states";

    fn path(key: &str) -> anyhow::Result<PathBuf> {
        Ok(daemon::home()?.join("states").join(format!("{key}.json")))
    }

    /// The state of the build `key`. One another build of blaze wrote, or whose programs
    /// cargo built are no longer in the target directory, is forgotten: nothing of it is
    /// what this build would have been. What it kept of cargo's stays ([`State::give_back`]).
    pub fn read(key: &str) -> Option<State> {
        State::read_at(&State::path(key).ok()?, &daemon::version())
    }

    /// The state at `path`, when `version` of blaze wrote it and its programs are there.
    fn read_at(path: &Path, version: &str) -> Option<State> {
        let text = std::fs::read_to_string(path).ok()?;
        let state = serde_json::from_str::<State>(&text).ok().filter(|state| {
            state.version == version && state.programs.iter().all(|it| it.cargos.exists())
        });
        if state.is_none() {
            let _ = std::fs::remove_file(path);
        }
        state
    }

    /// The states that have the program rustc linked from the unit recorded in `unit`, by
    /// their keys: the builds of the checkout that share the unit.
    pub fn of_unit(unit: &Path) -> Vec<String> {
        let keys = std::fs::read_to_string(unit.join(State::STATES)).unwrap_or_default();
        keys.lines().map(str::to_owned).collect()
    }

    pub fn write(&self) -> anyhow::Result<()> {
        let path = State::path(&self.key)?;
        std::fs::create_dir_all(path.parent().context("a state without a directory")?)?;
        let new = path.with_extension(format!("{}.new", std::process::id()));
        std::fs::write(&new, serde_json::to_vec(self)?)?;
        std::fs::rename(&new, &path)?;
        for unit in self.programs.iter().filter_map(|it| it.unit.as_deref()) {
            if !State::of_unit(unit).contains(&self.key) {
                let mut states =
                    OpenOptions::new().create(true).append(true).open(unit.join(State::STATES))?;
                writeln!(states, "{}", self.key)?;
            }
        }
        let kept: Vec<(&Path, &Path)> = self
            .programs
            .iter()
            .flat_map(|program| program.places().map(|it| (it, &*program.cargos)))
            .collect();
        Ok(std::fs::write(State::kept_path(&self.key)?, serde_json::to_vec(&kept)?)?)
    }

    /// Every state this blaze wrote for the user.
    pub fn all() -> Vec<State> {
        let dir = daemon::home().map(|it| it.join("states"));
        let entries = dir.ok().and_then(|it| std::fs::read_dir(it).ok()).into_iter().flatten();
        let keys = entries.flatten().filter_map(|it| {
            let name = it.file_name();
            let key = name.to_str()?.strip_suffix(".json")?;
            (!key.contains('.')).then(|| key.to_owned())
        });
        keys.filter_map(|it| State::read(&it)).collect()
    }

    /// Why the next build is cargo's: something cargo acts on changed since it built.
    pub fn broken_guard(&mut self) -> Option<String> {
        self.guards.iter_mut().find_map(|it| it.broken(self.built_at))
    }

    /// Why the next build is cargo's: a program the daemon is never asked for has new
    /// source.
    pub fn stale_cargos(&self) -> Option<String> {
        self.left_to_cargo().find_map(|(program, why)| {
            let file = program.newer_input(self.built_at)?;
            Some(format!("`{}`: {why}, and {} changed", program.name, file.display()))
        })
    }

    /// The executables of the build `key` and the hard link kept to what rustc linked of
    /// each. A file of its own, in a shape that stays: it is read when the state can't be.
    fn kept_path(key: &str) -> anyhow::Result<PathBuf> {
        Ok(daemon::home()?.join("states").join(format!("{key}.kept.json")))
    }

    /// Has every program be where cargo has it: the daemon's executable of one it built,
    /// and cargo's own of one it did not, which is the program while none of its source is
    /// written. `answers` is what the daemon said of each program of each checkout. `Ok`
    /// are the programs that stay cargo's and why; `Err` is why cargo builds, when a
    /// program the daemon did not build has new source. Under `held`, which is this build's.
    pub fn settle(
        &self,
        answers: &[Vec<Result<(), String>>],
        _held: &Held,
    ) -> Result<Vec<(&Program, String)>, String> {
        let mut cargos = Vec::new();
        for program in &self.programs {
            let answer = match program.asked_as {
                Some((checkout, at)) => answers
                    .get(checkout)
                    .and_then(|it| it.get(at))
                    .cloned()
                    .unwrap_or_else(|| Err("the daemon said nothing of it".to_owned())),
                None => Err(program.left_to_cargo.clone().unwrap_or_default()),
            };
            let from = match (&answer, program.asked_as) {
                (Ok(()), Some((checkout, at))) => &self.checkouts[checkout].programs[at].binary,
                _ => &program.cargos,
            };
            // On Linux cargo's program is the daemon's file, which whatever starts it next
            // starts with no word to the daemon, which may still be writing it.
            let _unwritten = answer.is_ok().then(|| crate::splice::unwritten(from));
            let package = match (&answer, program.asked_as) {
                (Ok(()), Some((checkout, at))) => {
                    let plan = &self.checkouts[checkout].programs[at];
                    let rustcs = plan.package.as_deref();
                    rustcs.map(|it| packed(&plan.binary, &plan.object, it)).transpose()?
                }
                _ => None,
            };
            if let Err(why) = answer {
                if let Some(file) = program.newer_input(self.built_at) {
                    let name = &program.name;
                    return Err(format!("`{name}`: {why}, and {} changed", file.display()));
                }
                cargos.push((program, why));
            }
            for to in program.places() {
                install(from, to)?;
                if let Some(package) = &package {
                    install(package, &package::beside(to))?;
                }
            }
        }
        Ok(cargos)
    }

    /// The directory the daemon builds `program` in, when it is asked for it.
    /// What a whole build of the tests `programs` by the daemon's `checkouts` leaves for their
    /// run: nothing of it is cargo's.
    pub fn of_whole(
        key: String,
        root: PathBuf,
        profile: String,
        programs: Vec<Program>,
        checkouts: Vec<Checkout>,
        configured: Vec<Configured>,
    ) -> State {
        State {
            version: daemon::version(),
            key,
            root,
            built_at: 0,
            profile,
            programs,
            guards: Vec::new(),
            checkouts,
            refusal: None,
            doc_tests: false,
            counts: Counts::default(),
            configured,
        }
    }

    pub fn plan_dir(&self, program: &Program) -> Option<&Path> {
        let (checkout, at) = program.asked_as?;
        self.checkouts.get(checkout)?.programs.get(at)?.object.parent()
    }

    /// Cargo's locks on the build directories of the programs, held until the programs this
    /// command runs have started.
    pub fn hold(&self) -> Held {
        Held { _locks: build_locks(self.programs.iter().map(|it| &*it.executable)) }
    }

    /// The programs cargo built that the daemon is never asked for, and why.
    pub fn left_to_cargo(&self) -> impl Iterator<Item = (&Program, &str)> {
        self.programs.iter().filter_map(|it| Some((it, it.left_to_cargo.as_deref()?)))
    }
}

impl Program {
    /// Every path cargo has the executable at.
    fn places(&self) -> impl Iterator<Item = &Path> {
        std::iter::once(&*self.executable).chain(self.names.iter().map(|it| &**it))
    }

    /// The first of its source files that was written after `built_at`.
    fn newer_input(&self, built_at: u128) -> Option<&Path> {
        let newer = |file: &&PathBuf| written_at(file).is_some_and(|it| it > built_at);
        self.inputs.iter().find(newer).map(PathBuf::as_path)
    }
}

/// Cargo's lock on each build directory one of `executables` is in, held: cargo takes it
/// for a build and waits for it as this does. Every build, fresh or not, cargo puts the
/// program in `deps` under its other name again, removing the file there first when the
/// two are not one file, and while a program is installed under its names one by one they
/// are not: a `cargo run` of another client of the checkout that found that moment would
/// find no program. So does a program started while a cargo writes it, and one started
/// after another client gave cargo its own back is not this build's: a client holds the
/// locks from putting its programs in place until they have started. A directory cargo
/// keeps no lock in, on NFS, has none to hold.
fn build_locks<'a>(executables: impl Iterator<Item = &'a Path>) -> Vec<std::fs::File> {
    // `debug/app`, `debug/examples/app`, `debug/deps/app-…` and, in cargo's second layout,
    // `debug/build/app/<hash>/out/app-…` are all of `debug`.
    let lock_of = |executable: &Path| {
        let profile = super::layout::unit_dir(executable).and_then(|it| it.profile());
        let above = profile.into_iter().chain(executable.ancestors().skip(1).take(2));
        above.map(|dir| dir.join(".cargo-lock")).find(|it| it.is_file())
    };
    let mut locks: Vec<PathBuf> = executables.filter_map(lock_of).collect();
    // In one order, so that two clients that each want two wait for neither.
    locks.sort();
    locks.dedup();
    let held = |lock: PathBuf| {
        let file = std::fs::File::open(lock).ok()?;
        if file.try_lock().is_err() {
            super::say::status(false, "Blocking", "waiting for file lock on build directory");
            file.lock().ok()?;
        }
        Some(file)
    };
    locks.into_iter().filter_map(held).collect()
}

/// Cargo's locks on build directories, held while this is.
pub struct Held {
    _locks: Vec<std::fs::File>,
}

/// Has `to` be the file `from` is, when it is not.
fn install(from: &Path, to: &Path) -> Result<(), String> {
    if is_placed(from, to) {
        return Ok(());
    }
    place(from, to).map_err(|it| format!("installing {}: {it}", to.display()))
}

/// Whether `to` is what `place` made of `from`: the file itself, or on macOS its copy, which
/// has its length and the time it was last written.
fn is_placed(from: &Path, to: &Path) -> bool {
    if same_file::is_same_file(from, to).unwrap_or(false) {
        return true;
    }
    let (Ok(from), Ok(to)) = (from.metadata(), to.metadata()) else { return false };
    let written = |it: &std::fs::Metadata| it.modified().ok();
    cfg!(target_os = "macos")
        && from.len() == to.len()
        && written(&from).is_some()
        && written(&from) == written(&to)
}

/// The package of what a debugger reads of the executable at `binary`, linked from `object`
/// and what rustc compiled, whose package is `rustcs`: written again when either is newer.
fn packed(binary: &Path, object: &Path, rustcs: &Path) -> Result<PathBuf, String> {
    let package = package::beside(binary);
    if !package::is_current(&package, &[object, rustcs]) {
        package::pack(binary, rustcs, &package)
            .map_err(|it| format!("packing {}: {it:#}", package.display()))?;
    }
    Ok(package)
}

/// Has `to` be the file `from` is, under a second name: what was at `to` stays as it is
/// for whoever runs it. On macOS a copy, which is a clone and as quick: there a program
/// started from a hard link just made can be killed as it starts, which is why cargo
/// copies there too (rust-lang/cargo#7821).
pub fn place(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut new = to.as_os_str().to_owned();
    new.push(format!(".{}.new", std::process::id()));
    let _ = std::fs::remove_file(&new);
    // Or another filesystem.
    if cfg!(target_os = "macos") || std::fs::hard_link(from, &new).is_err() {
        std::fs::copy(from, &new)?;
    }
    std::fs::rename(&new, to)?;
    // A rename onto another name of the same file, as `to` is after the first build, leaves
    // both names.
    let _ = std::fs::remove_file(&new);
    Ok(())
}

/// The cargo to run: the one that ran this command, or the one on the path.
pub fn cargo_path() -> PathBuf {
    std::env::var_os("CARGO").map_or_else(|| PathBuf::from("cargo"), PathBuf::from)
}

pub fn metadata(asked: &Invocation) -> Metadata {
    super::metadata::no_deps(asked.flags.manifest_path.as_deref(), &asked.flags.config)
}

pub fn package_dir(package: &Package) -> PathBuf {
    package.manifest_path.parent().map(|it| it.as_std_path().to_owned()).unwrap_or_default()
}

/// The directory of the package the build is of, whose features the daemon loads the
/// workspace with. `Err` is why the selection is one it does not load.
fn project(asked: &Invocation, cwd: &Path, metadata: &Metadata) -> Result<PathBuf, String> {
    let root = metadata.workspace_root.as_std_path();
    if let Some(named) = asked.flags.packages.first() {
        let name = named.split('@').next().unwrap_or(named);
        let package = metadata.workspace_packages().into_iter().find(|it| it.name.as_str() == name);
        return package
            .map(package_dir)
            .ok_or(format!("`-p {named}` is not a package of the workspace"));
    }
    if asked.flags.workspace {
        return Ok(root.to_owned());
    }
    let start = match &asked.flags.manifest_path {
        Some(manifest) => cwd.join(manifest).parent().map(Path::to_owned).unwrap_or_default(),
        None => cwd.to_owned(),
    };
    let nearest = start.ancestors().find(|it| it.join("Cargo.toml").exists());
    nearest.map(Path::to_owned).ok_or("no Cargo.toml here".to_owned())
}

/// Why blaze's release can't be built with, when it can't: its files are not installed.
/// Nothing is run to tell, and which toolchain the project names does not matter.
fn other_toolchain() -> Option<String> {
    super::toolchain::found().is_none().then(|| {
        format!(
            "blaze builds with Rust {}, which is not installed: `blaze toolchain` installs it",
            super::toolchain::OURS.trim()
        )
    })
}

/// What the checkouts of one repository share: the repository's own directory, which is
/// `.git` in the first checkout and is named by the `.git` file of a worktree. A directory
/// that is no checkout is its own.
pub fn repository(root: &Path) -> PathBuf {
    // git names the directory its own way, on Windows with other separators than the
    // checkout's path has: both are spelled as the file system spells them.
    let spelled = |dir: &Path| std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_owned());
    let git = root.join(".git");
    if git.is_dir() {
        return spelled(&git);
    }
    let named = std::fs::read_to_string(&git).ok();
    let common = named.as_deref().and_then(|it| {
        // `gitdir: /repo/.git/worktrees/name`
        let dir = Path::new(it.strip_prefix("gitdir:")?.trim());
        dir.ancestors().find(|it| it.file_name().is_some_and(|it| it == "worktrees"))?.parent()
    });
    common.map_or_else(|| root.to_owned(), spelled)
}

/// The variable the dynamic linker of this platform looks for libraries by.
const LIBRARY_PATH: &str = if cfg!(windows) {
    "PATH"
} else if cfg!(target_os = "macos") {
    "DYLD_FALLBACK_LIBRARY_PATH"
} else {
    "LD_LIBRARY_PATH"
};

/// Where cargo has the dynamic linker look when it runs `executable`: the dependencies of
/// the profile, the profile's own directory, the toolchain's libraries, and then wherever
/// it looked already. A test of a proc macro is linked with the toolchain's `libstd`. The
/// dependencies are `deps` in cargo's first layout and, in its second, the directory of each
/// unit that is a `dylib` ([`super::layout::dylib_dirs`]).
pub fn library_path(executable: &Path, sysroot: Option<&Path>) -> Option<(String, String)> {
    let inside = |it: &&Path| it.file_name().is_some_and(|it| it == "deps" || it == "examples");
    let dir = executable.parent()?;
    let unit = super::layout::unit_dir(dir);
    let profile = match &unit {
        Some(unit) => unit.profile()?,
        None if inside(&dir) => dir.parent()?,
        None => dir,
    };
    let already = std::env::var_os(LIBRARY_PATH);
    let already = already.iter().flat_map(std::env::split_paths);
    // The layout is the executable's, not what is in the target directory yet: before the
    // first build of a fresh one there is neither `deps` nor `build`, and a test's environment
    // would differ from the next build's (a test's cached pass is keyed by it).
    let deps = match unit.is_some() && !profile.join("deps").is_dir() {
        true => super::layout::dylib_dirs(profile),
        false => vec![profile.join("deps")],
    };
    let dirs = deps.into_iter().chain([profile.to_owned()]).chain(sysroot.map(Path::to_owned));
    let joined = std::env::join_paths(dirs.chain(already)).ok()?;
    Some((LIBRARY_PATH.to_owned(), joined.into_string().ok()?))
}

/// The variables cargo sets for a program of `package` when it runs it.
pub fn run_env(package: &Package, scripts: &[BuildScript]) -> Vec<(String, String)> {
    let version = &package.version;
    let text = |it: &Option<String>| it.clone().unwrap_or_default();
    let mut env = vec![
        ("CARGO".to_owned(), cargo_path().display().to_string()),
        ("CARGO_MANIFEST_DIR".to_owned(), package_dir(package).display().to_string()),
        ("CARGO_MANIFEST_PATH".to_owned(), package.manifest_path.to_string()),
        ("CARGO_PKG_NAME".to_owned(), package.name.to_string()),
        ("CARGO_PKG_VERSION".to_owned(), version.to_string()),
        ("CARGO_PKG_VERSION_MAJOR".to_owned(), version.major.to_string()),
        ("CARGO_PKG_VERSION_MINOR".to_owned(), version.minor.to_string()),
        ("CARGO_PKG_VERSION_PATCH".to_owned(), version.patch.to_string()),
        ("CARGO_PKG_VERSION_PRE".to_owned(), version.pre.to_string()),
        ("CARGO_PKG_AUTHORS".to_owned(), package.authors.join(":")),
        ("CARGO_PKG_DESCRIPTION".to_owned(), text(&package.description)),
        ("CARGO_PKG_HOMEPAGE".to_owned(), text(&package.homepage)),
        ("CARGO_PKG_REPOSITORY".to_owned(), text(&package.repository)),
        ("CARGO_PKG_LICENSE".to_owned(), text(&package.license)),
        (
            "CARGO_PKG_LICENSE_FILE".to_owned(),
            package.license_file.as_ref().map(|it| it.to_string()).unwrap_or_default(),
        ),
        (
            "CARGO_PKG_RUST_VERSION".to_owned(),
            package.rust_version.as_ref().map(|it| it.to_string()).unwrap_or_default(),
        ),
        (
            "CARGO_PKG_README".to_owned(),
            package.readme.as_ref().map(|it| it.to_string()).unwrap_or_default(),
        ),
    ];
    for script in scripts.iter().filter(|it| it.package_id == package.id) {
        env.push(("OUT_DIR".to_owned(), script.out_dir.to_string()));
        env.extend(script.env.iter().cloned());
    }
    env
}

/// The `[env]` of cargo's configuration as cargo reads it in `cwd`: from the `.cargo`
/// directory of each directory up from it, the nearest first, then from cargo's home. Of two
/// files in one directory cargo reads `config`, not `config.toml`. A `relative` value is a
/// path from the directory its `.cargo` is in.
pub fn configured_env(cwd: &Path) -> Vec<Configured> {
    let in_dir = |dir: &Path| {
        ["config", "config.toml"].map(|it| dir.join(it)).into_iter().find(|it| it.is_file())
    };
    let mut files: Vec<PathBuf> =
        cwd.ancestors().filter_map(|it| in_dir(&it.join(".cargo"))).collect();
    let home = home::cargo_home().and_then(|it| in_dir(&it));
    if let Some(home) = home
        && !files.iter().any(|it| same_file::is_same_file(it, &home).unwrap_or(false))
    {
        files.push(home);
    }
    let mut env: Vec<Configured> = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        let Ok(table) = text.parse::<toml::Table>() else { continue };
        let Some(toml::Value::Table(variables)) = table.get("env") else { continue };
        let base = file.parent().and_then(Path::parent).unwrap_or(Path::new(""));
        for (name, it) in variables {
            if env.iter().any(|known| known.name == *name) {
                continue;
            }
            let flag = |table: &toml::Table, name: &str| {
                table.get(name).and_then(toml::Value::as_bool).unwrap_or(false)
            };
            let (value, force, relative) = match it {
                toml::Value::String(value) => (value.clone(), false, false),
                toml::Value::Table(it) => {
                    let Some(value) = it.get("value").and_then(toml::Value::as_str) else {
                        continue;
                    };
                    (value.to_owned(), flag(it, "force"), flag(it, "relative"))
                }
                _ => continue,
            };
            let value = if relative { base.join(value).display().to_string() } else { value };
            env.push(Configured { name: name.clone(), value, force });
        }
    }
    env
}

/// `said`, the errors the daemon found in the source of the target `target` (`bin "app"`,
/// `lib test`) of `package`, as rustc writes them and cargo ends the build: with the line
/// naming what did not compile, and the note of the toolchain blaze built with in place of the
/// project's.
pub fn turned_down(said: &crate::errors::Said, package: &str, target: &str) -> String {
    let errors = match said.count() {
        0 | 1 => "1 previous error".to_owned(),
        n => format!("{n} previous errors"),
    };
    let mut notes = said.notes().join("\n");
    if !notes.is_empty() {
        notes.push('\n');
    }
    format!("{said}\n\n{notes}error: could not compile `{package}` ({target}) due to {errors}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_program_is_run_with_the_libraries_cargo_runs_it_with() {
        let sysroot = Path::new("/toolchain/lib/rustlib/host/lib");
        let dirs = |executable: &str| {
            let (name, value) = library_path(Path::new(executable), Some(sysroot)).unwrap();
            assert_eq!(name, LIBRARY_PATH);
            let dirs: Vec<PathBuf> = std::env::split_paths(&value).take(3).collect();
            dirs
        };
        let expected: Vec<PathBuf> =
            ["/t/debug/deps", "/t/debug", "/toolchain/lib/rustlib/host/lib"]
                .map(PathBuf::from)
                .into();
        assert_eq!(dirs("/t/debug/deps/unit-0123"), expected);
        assert_eq!(dirs("/t/debug/app"), expected);
        assert_eq!(dirs("/t/debug/examples/one"), expected);
        // cargo's second layout, with no `deps`: a test in its unit's directory.
        let second = std::env::temp_dir().join(format!("blaze-state-{}", std::process::id()));
        let unit = second.join("debug/build/app/0123456789abcdef/out");
        std::fs::create_dir_all(&unit).unwrap();
        let test = unit.join("app-0123456789abcdef");
        let dirs = |executable: &Path| {
            let (_, value) = library_path(executable, Some(sysroot)).unwrap();
            std::env::split_paths(&value).take(2).collect::<Vec<PathBuf>>()
        };
        assert_eq!(dirs(&test), [second.join("debug"), sysroot.to_owned()]);
        let _ = std::fs::remove_dir_all(&second);
        // Before the first build of a fresh target directory, the same.
        assert_eq!(dirs(&test), [second.join("debug"), sysroot.to_owned()]);
    }

    #[test]
    fn the_nearest_configuration_says_a_variable() {
        let top = std::env::temp_dir().join(format!("blaze-configured-{}", std::process::id()));
        let inner = top.join("ws/app");
        std::fs::create_dir_all(inner.join(".cargo")).unwrap();
        std::fs::create_dir_all(top.join(".cargo")).unwrap();
        std::fs::write(
            top.join(".cargo/config.toml"),
            "[env]\nNEAR = \"far\"\nFAR = { value = \"data\", relative = true, force = true }\n",
        )
        .unwrap();
        std::fs::write(inner.join(".cargo/config.toml"), "[env]\nNEAR = \"near\"\n").unwrap();
        let configured = configured_env(&inner);
        let said = |name: &str| configured.iter().find(|it| it.name == name).cloned();
        let near = said("NEAR").unwrap();
        assert_eq!((near.value.as_str(), near.force), ("near", false));
        let far = said("FAR").unwrap();
        assert_eq!((PathBuf::from(far.value), far.force), (top.join("data"), true));
        std::fs::remove_dir_all(&top).unwrap();
    }

    #[test]
    fn only_tests_changed_keeps_the_text_without_tests() {
        let dir = std::env::temp_dir().join(format!("blaze-beside-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("lib.rs");
        let print = |text: &str| {
            std::fs::write(&file, text).unwrap();
            without_tests(&file).unwrap()
        };
        let base = "pub fn one() -> u32 {\n    1\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {}\n}\n";
        let was = print(base);
        assert_eq!(print(&format!("{base}\n#[test]\nfn added() {{}}\n")), was);
        assert_eq!(print(&base.replace("fn t() {}", "fn t() { assert!(true); }")), was);
        assert_ne!(print(&base.replace("    1\n", "    2\n")), was);
        assert_ne!(print(&format!("\n{base}")), was);
        assert_ne!(print(&format!("{base}\n#[cfg(not(test))]\nfn other() {{}}\n")), was);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_state_another_blaze_wrote_is_forgotten() {
        let dir = std::env::temp_dir().join(format!("blaze-stale-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("key.json");
        let cargos = dir.join("app.cargos");
        let write = |version: &str| {
            let program = serde_json::json!({
                "name": "app", "kind": "Bin", "package": "app", "test": false,
                "source": "src/main.rs", "package_dir": dir, "executable": dir.join("app"),
                "cargos": cargos, "asked_as": null, "left_to_cargo": null, "inputs": [],
                "env": [],
            });
            let state = serde_json::json!({
                "version": version, "key": "key", "root": dir, "built_at": 0,
                "profile": "release", "programs": [program], "guards": [], "checkouts": [],
                "refusal": null, "doc_tests": false, "counts": {"served": 0, "cargo": {}},
            });
            std::fs::write(&path, state.to_string()).unwrap();
        };
        std::fs::write(&cargos, "").unwrap();
        write("this");
        assert!(State::read_at(&path, "this").is_some());
        assert!(path.exists());
        // A state of an older blaze, an unreadable one, and one whose program cargo built
        // is gone are each a build no build has loaded: read as nothing, and gone.
        assert!(State::read_at(&path, "newer").is_none());
        assert!(!path.exists());
        std::fs::write(&path, "{\"version\": 1").unwrap();
        assert!(State::read_at(&path, "this").is_none());
        assert!(!path.exists());
        write("this");
        std::fs::remove_file(&cargos).unwrap();
        assert!(State::read_at(&path, "this").is_none());
        assert!(!path.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
