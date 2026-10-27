//! Every test executable `cargo ra-jit test` runs, runs in a sandbox ([`crate::sandbox`]), one
//! sandbox for the whole executable with libtest's threads inside it: the machine's files
//! read-only, the workspace and the target directory as they are but its writes kept to
//! itself, empty temporary directories, and no network but its own loopback. The test
//! harness ra-jit writes marks each test's thread ([`crate::sandbox::MARK`]), so that what
//! the sandbox records is each test's.
//!
//! What the sandbox refused a test that failed (a write, an address) is said after the run,
//! with how to run the test outside: `[package.metadata.blaze.sandbox]` in its package's
//! manifest, with `tests = false` for every test of the package or `unsandboxed-tests` for
//! some, or `RA_JIT_SANDBOX=off` for every test of the command. Where no sandbox can be made
//! the command stops before any test runs, and says why: a run that went on outside would
//! break, unseen, the promise that a test can't change the machine or reach the network.
//!
//! Under `RA_JIT_TESTS=affected` the sandbox also records what each test that reaches the
//! world outside its code read, and once it passes, what was at each of those paths is kept:
//! the test runs again when its code changed or one of them did, and is skipped otherwise, as
//! a test that reaches nothing outside is. What a thread no test marked read counts for every
//! such test that ran. Its environment is cut down to what names the toolchain and the user
//! ([`env`]), which is part of its identity.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::Context as _;
use sha2::{Digest as _, Sha256};

use super::{
    say,
    state::{Program, State},
};
use crate::sandbox::{self, Line, Reached, Spec};

pub(crate) const VAR: &str = sandbox::VAR;

/// Names of more variables of the environment a test keeps under `RA_JIT_TESTS=affected`, by
/// commas.
pub(crate) const PASS: &str = "RA_JIT_TEST_ENV";

/// The file beside a program of tests with what each sandboxed test that passed read, and
/// what was there when it passed: the test's name, the path, what [`stat`] said of it and
/// what [`digest`] did, by tabs, one path a line; or the name, `X`, `-` and why what it read
/// is not known.
const INPUTS: &str = "tests-inputs";

/// The variables of the environment a test keeps under `RA_JIT_TESTS=affected` besides those
/// cargo and ra-jit set for it: what finds programs and the toolchain, who and where the user
/// is, the locale, and what decides how a failure is shown.
pub(crate) const KEPT: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "LANG",
    "LANGUAGE",
    "TZ",
    "CARGO",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "RUSTUP_TOOLCHAIN",
    "LD_LIBRARY_PATH",
    "NIX_LD",
    "NIX_LD_LIBRARY_PATH",
    "RUST_BACKTRACE",
    "RUST_LIB_BACKTRACE",
    "RUST_MIN_STACK",
];

/// The table of a package's manifest that takes its tests out of the sandbox.
pub(crate) const TABLE: &str = "[package.metadata.blaze.sandbox]";

/// Whether this command runs its tests sandboxed: unless `RA_JIT_SANDBOX` (or `BLAZE_SANDBOX`)
/// is `off`, as [`sandbox::on`] decides for build scripts and proc macros too.
pub(crate) fn on() -> bool {
    sandbox::on()
}

/// What a package says of its tests and the sandbox, in [`TABLE`].
#[derive(Default)]
pub(crate) struct OptOut {
    pub(crate) manifest: PathBuf,
    /// `tests = false`: every test of the package runs outside.
    pub(crate) all: bool,
    /// `unsandboxed-tests`: the tests, by libtest's names, that run outside.
    pub(crate) tests: BTreeSet<String>,
    /// `build-script = false`: its build script runs outside, when the package is a member of
    /// the workspace being built (a dependency can't take itself out).
    pub(crate) build_script_outside: bool,
}

/// What the manifest of the package in `package_dir` says of its tests and the sandbox. A
/// table that says it otherwise than as [`TABLE`]'s keys are is an error.
pub(crate) fn opt_out(package_dir: &Path) -> anyhow::Result<OptOut> {
    let manifest = package_dir.join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest)
        .with_context(|| format!("reading {}", manifest.display()))?;
    let value: toml::Table =
        toml::from_str(&text).with_context(|| format!("reading {}", manifest.display()))?;
    let table = ["package", "metadata", "blaze", "sandbox"]
        .iter()
        .try_fold(&value, |table, key| table.get(*key)?.as_table());
    let mut out = OptOut { manifest: manifest.clone(), ..OptOut::default() };
    let Some(table) = table else { return Ok(out) };
    let wrong = |what: &str| anyhow::anyhow!("{} in {}: {what}", TABLE, manifest.display());
    for (key, value) in table {
        match key.as_str() {
            "tests" => {
                out.all = !value.as_bool().ok_or_else(|| wrong("`tests` is true or false"))?
            }
            "unsandboxed-tests" => {
                let names =
                    value.as_array().ok_or_else(|| wrong("`unsandboxed-tests` is a list"))?;
                for name in names {
                    let name = name.as_str().ok_or_else(|| {
                        wrong("`unsandboxed-tests` lists tests by their names, as strings")
                    })?;
                    out.tests.insert(name.to_owned());
                }
            }
            "build-script" => {
                out.build_script_outside =
                    !value.as_bool().ok_or_else(|| wrong("`build-script` is true or false"))?
            }
            other => {
                return Err(wrong(&format!(
                    "`{other}` is not known (`tests`, `unsandboxed-tests` and `build-script` are)"
                )));
            }
        }
    }
    Ok(out)
}

/// The table of the workspace's root manifest that takes packages' build scripts and proc
/// macros out of the sandbox: the only place a dependency from a registry can be named.
pub(crate) const WORKSPACE_TABLE: &str = "[workspace.metadata.blaze.sandbox]";

/// What the workspace's root manifest says of build scripts, proc macros and the sandbox, in
/// [`WORKSPACE_TABLE`].
#[derive(Default, Debug)]
pub(crate) struct WorkspaceOptOut {
    pub(crate) manifest: PathBuf,
    /// `unsandboxed-build-scripts`: the packages, by name, whose build scripts run outside.
    pub(crate) build_scripts: BTreeSet<String>,
    /// `unsandboxed-proc-macros`: the packages, by name, whose proc macros are expanded by a
    /// server outside.
    pub(crate) proc_macros: BTreeSet<String>,
    /// `build-script-env`: variables of the environment given to every build script in the
    /// sandbox besides those it is always given (`script_sandbox.rs`).
    pub(crate) build_script_env: BTreeSet<String>,
    /// `build-script-link-dirs`: directories of the system a build script in the sandbox may
    /// name in `rustc-link-search` besides those it always may (`directives.rs`).
    pub(crate) build_script_link_dirs: BTreeSet<String>,
    /// `wasi-build-scripts`: the packages, by name (`"*"`, all), whose build scripts run under
    /// WASI, an experiment (`wasi_script.rs`).
    pub(crate) wasi_build_scripts: BTreeSet<String>,
}

impl WorkspaceOptOut {
    /// Whether the proc macro of the crate `name` (a package's name with `-` as `_`, as the
    /// crate is named) is expanded outside.
    pub(crate) fn macro_outside(&self, name: &str) -> bool {
        self.proc_macros.iter().any(|it| it.replace('-', "_") == name.replace('-', "_"))
    }
}

/// What the root manifest of the workspace at `root` says in [`WORKSPACE_TABLE`]: nothing when
/// it has no such table or no manifest. A table that says it otherwise than as its keys are
/// is an error.
pub(crate) fn workspace_opt_out(root: &Path) -> anyhow::Result<WorkspaceOptOut> {
    let manifest = root.join("Cargo.toml");
    let mut out = WorkspaceOptOut { manifest: manifest.clone(), ..WorkspaceOptOut::default() };
    let Ok(text) = std::fs::read_to_string(&manifest) else { return Ok(out) };
    let value: toml::Table =
        toml::from_str(&text).with_context(|| format!("reading {}", manifest.display()))?;
    let table = ["workspace", "metadata", "blaze", "sandbox"]
        .iter()
        .try_fold(&value, |table, key| table.get(*key)?.as_table());
    let Some(table) = table else { return Ok(out) };
    let wrong =
        |what: &str| anyhow::anyhow!("{} in {}: {what}", WORKSPACE_TABLE, manifest.display());
    for (key, value) in table {
        let into = match key.as_str() {
            "unsandboxed-build-scripts" => &mut out.build_scripts,
            "unsandboxed-proc-macros" => &mut out.proc_macros,
            "build-script-env" => &mut out.build_script_env,
            "build-script-link-dirs" => &mut out.build_script_link_dirs,
            "wasi-build-scripts" => &mut out.wasi_build_scripts,
            other => {
                return Err(wrong(&format!(
                    "`{other}` is not known (`unsandboxed-build-scripts`, \
                     `unsandboxed-proc-macros`, `build-script-env`, \
                     `build-script-link-dirs` and `wasi-build-scripts` are)"
                )));
            }
        };
        let names = value.as_array().ok_or_else(|| wrong(&format!("`{key}` is a list")))?;
        for name in names {
            let name =
                name.as_str().ok_or_else(|| wrong(&format!("`{key}` lists names, as strings")))?;
            into.insert(name.to_owned());
        }
    }
    Ok(out)
}

/// The root of the workspace `dir` is in, as cargo finds it: the nearest directory at or above
/// it whose manifest has a `[workspace]` table, `None` when there is none. A manifest on the
/// way that can't be read or parsed is an error, as cargo makes it one.
pub(crate) fn workspace_root(dir: &Path) -> anyhow::Result<Option<PathBuf>> {
    for it in dir.ancestors() {
        let manifest = it.join("Cargo.toml");
        let text = match std::fs::read_to_string(&manifest) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", manifest.display()));
            }
        };
        let value: toml::Table =
            toml::from_str(&text).with_context(|| format!("reading {}", manifest.display()))?;
        if value.contains_key("workspace") {
            return Ok(Some(it.to_owned()));
        }
    }
    Ok(None)
}

/// The environment the test `command` runs would have, cut down to what a test keeps under
/// `RA_JIT_TESTS=affected`: what the command sets itself, and of the rest only [`KEPT`] and
/// what [`PASS`] names.
pub(crate) fn env(command: &Command) -> Vec<(String, String)> {
    let passed = std::env::var(PASS).unwrap_or_default();
    let kept = |name: &str| {
        KEPT.contains(&name)
            || name.starts_with("LC_")
            || passed.split(',').any(|it| it.trim() == name)
    };
    with_command(command, kept)
}

/// The whole environment the test `command` runs would have.
pub(crate) fn whole_env(command: &Command) -> Vec<(String, String)> {
    with_command(command, |_| true)
}

fn with_command(command: &Command, kept: impl Fn(&str) -> bool) -> Vec<(String, String)> {
    let mut env: BTreeMap<String, String> = std::env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .filter(|(name, _)| kept(name))
        .collect();
    for (name, value) in command.get_envs() {
        let Some(name) = name.to_str() else { continue };
        match value.and_then(|it| it.to_str()) {
            Some(value) => _ = env.insert(name.to_owned(), value.to_owned()),
            None => _ = env.remove(name),
        }
    }
    env.into_iter().collect()
}

/// What stands for the environment `env` in a sandboxed test's identity.
pub(crate) fn identity_of(env: &[(String, String)]) -> String {
    let mut hash = Sha256::new();
    for (name, value) in env {
        hash.update(name.as_bytes());
        hash.update([0]);
        hash.update(value.as_bytes());
        hash.update([0]);
    }
    let hex: String = hash.finalize().iter().take(8).map(|it| format!("{it:02x}")).collect();
    format!("~{hex}")
}

/// The target directory `executable` is in: the nearest directory above it that cargo marked
/// as its own.
fn target_of(executable: &Path) -> Option<&Path> {
    executable.ancestors().skip(1).find(|it| it.join("CACHEDIR.TAG").is_file())
}

/// Cargo's home in the environment `env`, as cargo finds it: `CARGO_HOME`, else `.cargo` in
/// the home directory.
fn cargo_home(env: &[(String, String)]) -> Option<PathBuf> {
    let var = |name: &str| env.iter().find(|it| it.0 == name).map(|it| PathBuf::from(&it.1));
    let home = var("CARGO_HOME").or_else(|| Some(var("HOME")?.join(".cargo")))?;
    Some(std::fs::canonicalize(&home).unwrap_or(home))
}

/// Where this command keeps the specs and records of its sandboxes, removed when it is done.
pub(crate) struct Scratch {
    dir: PathBuf,
    made: usize,
}

impl Scratch {
    pub(crate) fn new() -> Scratch {
        let dir = std::env::temp_dir().join(format!("ra-jit-tests-{}", std::process::id()));
        Scratch { dir, made: 0 }
    }

    /// The sandbox of a run of `test` with the environment `env`, recording reads when
    /// `reads`.
    pub(crate) fn sandbox(
        &mut self,
        state: &State,
        test: &Program,
        env: Vec<(String, String)>,
        reads: bool,
    ) -> anyhow::Result<Sandbox> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("making {}", self.dir.display()))?;
        self.made += 1;
        let (spec_file, record) =
            (self.dir.join(format!("{}.json", self.made)), self.dir.join(format!("{}", self.made)));
        let mut overlays = vec![state.root.clone()];
        if let Some(target) = target_of(&test.executable).filter(|it| !it.starts_with(&state.root))
        {
            overlays.push(target.to_owned());
        }
        // Every cargo takes its locks in its home and records there what it used, so a test
        // that runs cargo (trybuild's, a cargo plugin's) writes it, for itself alone.
        if let Some(home) = cargo_home(&env).filter(|it| it.is_dir())
            && !overlays.iter().any(|it| home.starts_with(it))
        {
            overlays.push(home);
        }
        let mut not_inputs = vec![test.executable.clone()];
        not_inputs.extend(std::fs::canonicalize(&test.executable).ok());
        let spec = Spec {
            cwd: test.package_dir.clone(),
            env,
            overlays,
            writable: Vec::new(),
            readable: Vec::new(),
            record: Some(record.clone()),
            record_reads: reads,
            not_inputs,
            view: None,
        };
        std::fs::write(&spec_file, serde_json::to_string(&spec)?)
            .with_context(|| format!("writing {}", spec_file.display()))?;
        Ok(Sandbox { spec_file, record })
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The sandbox of one run of a test executable.
pub(crate) struct Sandbox {
    spec_file: PathBuf,
    record: PathBuf,
}

impl Sandbox {
    /// The command that runs `program` in it.
    pub(crate) fn command(&self, program: &Path) -> anyhow::Result<Command> {
        let mut command = Command::new(std::env::current_exe().context("finding ra-jit itself")?);
        command.arg(sandbox::COMMAND).arg(&self.spec_file).arg("--").arg(program);
        Ok(command)
    }

    /// What it recorded, once the run has ended; `None` when it wrote nothing.
    pub(crate) fn record(&self) -> Option<Vec<Line>> {
        sandbox::read_record(&self.record)
    }
}

/// Says, of a run in a sandbox that failed, what the sandbox refused each test of `failed`
/// that it refused something, and how to run it outside: of the run as a whole when which
/// tests failed is not known. `opt_out` is the test's package's.
pub(crate) fn diagnose(lines: &[Line], failed: Option<&[String]>, shown: &str, opt_out: &OptOut) {
    let refused = |test: Option<&str>| -> Vec<&Line> {
        let mut them: Vec<&Line> = lines
            .iter()
            .filter(|it| matches!(it.reached, Reached::Write | Reached::Net))
            .filter(|it| test.is_none_or(|test| it.test.is_empty() || it.test == test))
            .collect();
        them.sort_by(|a, b| (a.reached, &a.what).cmp(&(b.reached, &b.what)));
        them.dedup_by(|a, b| a.reached == b.reached && a.what == b.what);
        them
    };
    let manifest = opt_out.manifest.display();
    let off = format!("or `{VAR}=off` for every test of the command");
    match failed {
        Some(failed) => {
            for name in failed {
                let them = refused(Some(name));
                if them.is_empty() {
                    continue;
                }
                say::note(&format!(
                    "{name} failed in the sandbox, which refused it {}; if it needs that, run \
                     it outside: `unsandboxed-tests = [{name:?}]` under `{TABLE}` in {manifest} \
                     (`tests = false` for every test of the package), {off}",
                    what_was_refused(&them)
                ));
            }
        }
        None => {
            let them = refused(None);
            if !them.is_empty() {
                say::note(&format!(
                    "{shown} failed in the sandbox, which refused its tests {}; if they need \
                     that, run them outside: `unsandboxed-tests = [...]` or `tests = false` \
                     under `{TABLE}` in {manifest}, {off}",
                    what_was_refused(&them)
                ));
            }
        }
    }
}

/// `writing `a` and `b` (the machine is read-only in a sandbox) and connecting to `c` (...)`.
pub(crate) fn what_was_refused(them: &[&Line]) -> String {
    const SHOWN: usize = 3;
    let listed = |reached: Reached| {
        let all: Vec<String> = them
            .iter()
            .filter(|it| it.reached == reached)
            .map(|it| format!("`{}`", it.what))
            .collect();
        match all.len() {
            0 => None,
            1 => Some(all[0].clone()),
            n if n <= SHOWN => Some(format!("{} and {}", all[..n - 1].join(", "), all[n - 1])),
            n => Some(format!("{} and {} more", all[..SHOWN].join(", "), n - SHOWN)),
        }
    };
    let mut parts = Vec::new();
    if let Some(paths) = listed(Reached::Write) {
        parts.push(format!("writing {paths} (the machine is read-only in a sandbox)"));
    }
    if let Some(addresses) = listed(Reached::Net) {
        parts.push(format!(
            "connecting to {addresses} (a sandbox has no network but its own loopback)"
        ));
    }
    parts.join(" and ")
}

/// What is kept of the sandboxed tests of a program of tests that reach the world outside
/// their code: what each that passed read.
pub(crate) struct Kept {
    file: PathBuf,
    /// Each test's paths, with what [`stat`] and [`digest`] said of each when it passed; or
    /// why what it read is not known.
    tests: BTreeMap<String, Result<Vec<(String, String, String)>, String>>,
    /// The tests run in the sandbox by this command whose reads are kept.
    pub(crate) ran: BTreeSet<String>,
    seen: HashMap<String, (String, String)>,
}

impl Kept {
    pub(crate) fn read(dir: &Path) -> Kept {
        let file = dir.join(INPUTS);
        let mut tests: BTreeMap<String, Result<Vec<_>, String>> = BTreeMap::new();
        for line in std::fs::read_to_string(&file).unwrap_or_default().lines() {
            let mut parts = line.splitn(4, '\t');
            let (Some(name), Some(stat), Some(digest), Some(path)) =
                (parts.next(), parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            let paths = tests.entry(name.to_owned()).or_insert_with(|| Ok(Vec::new()));
            match (stat, paths) {
                ("X", paths) => *paths = Err(path.to_owned()),
                (_, Ok(paths)) => paths.push((path.to_owned(), stat.to_owned(), digest.to_owned())),
                (_, Err(_)) => {}
            }
        }
        Kept { file, tests, ran: BTreeSet::new(), seen: HashMap::new() }
    }

    /// Why the test `name` runs although its code is as when it passed: the first path it
    /// read then that is not what it was, or why what it read is not known. `None` when each
    /// is as it was.
    pub(crate) fn changed(&mut self, name: &str) -> Option<String> {
        let paths = match self.tests.get(name) {
            None => return Some("what it read is not known".to_owned()),
            Some(Err(why)) => return Some(format!("what it read is not all known: {why}")),
            Some(Ok(paths)) => paths,
        };
        for (path, stat_then, digest_then) in paths {
            let (stat_now, digest_now) = match self.seen.get(path) {
                Some(it) => it.clone(),
                None => {
                    let stat_now = stat(Path::new(path));
                    let digest_now = match &stat_now == stat_then {
                        true => digest_then.clone(),
                        false => digest(Path::new(path)),
                    };
                    self.seen.insert(path.clone(), (stat_now.clone(), digest_now.clone()));
                    (stat_now, digest_now)
                }
            };
            if stat_now != *stat_then && digest_now != *digest_then {
                return Some(format!("it read `{path}`, which changed"));
            }
        }
        None
    }

    /// Keeps what each test of `passed` that ran in the sandbox read, by the `record` of its
    /// run, and forgets the tests that are not in `passed`. A test the sandbox recorded
    /// nothing for is taken out of `passed`, so that it runs again: what it read is not known.
    pub(crate) fn settle(mut self, passed: &mut BTreeMap<String, String>, record: Option<&[Line]>) {
        let mut known = std::mem::take(&mut self.seen);
        for (path, stat, digest) in self.tests.values().filter_map(|it| it.as_ref().ok()).flatten()
        {
            known.entry(path.clone()).or_insert_with(|| (stat.clone(), digest.clone()));
        }
        let mut now = |path: &str| {
            let stat_now = stat(Path::new(path));
            if let Some((stat, digest)) = known.get(path).filter(|(stat, _)| *stat == stat_now) {
                return (stat.clone(), digest.clone());
            }
            let digest_now = digest(Path::new(path));
            known.insert(path.to_owned(), (stat_now.clone(), digest_now.clone()));
            (stat_now, digest_now)
        };
        self.tests.retain(|name, _| passed.contains_key(name));
        for name in &self.ran {
            if !passed.contains_key(name) {
                continue;
            }
            let Some(record) = record else {
                passed.remove(name);
                self.tests.remove(name);
                continue;
            };
            let its = |it: &&Line| it.test.is_empty() || it.test == *name;
            let incomplete = record.iter().filter(its).find(|it| it.reached == Reached::Incomplete);
            let kept = match incomplete {
                Some(line) => Err(line.what.clone()),
                None => Ok(record
                    .iter()
                    .filter(its)
                    .filter(|it| it.reached == Reached::Read)
                    .map(|it| it.what.as_str())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .map(|path| {
                        let (stat, digest) = now(path);
                        (path.to_owned(), stat, digest)
                    })
                    .collect()),
            };
            self.tests.insert(name.clone(), kept);
        }
        let mut text = String::new();
        for (name, paths) in &self.tests {
            match paths {
                Ok(paths) => {
                    for (path, stat, digest) in paths {
                        text.push_str(&format!("{name}\t{stat}\t{digest}\t{path}\n"));
                    }
                }
                Err(why) => text.push_str(&format!("{name}\tX\t-\t{why}\n")),
            }
        }
        let writing = self.file.with_extension("writing");
        if std::fs::write(&writing, text)
            .and_then(|()| std::fs::rename(&writing, &self.file))
            .is_err()
        {
            _ = std::fs::remove_file(&self.file);
        }
    }
}

/// What is quick to tell of what is at `path`: whether it is there, its size and its last
/// change. Where that is the same, what is there is taken to be.
pub(crate) fn stat(path: &Path) -> String {
    let Ok(meta) = std::fs::metadata(path) else { return "-".to_owned() };
    let at = meta.modified().ok().and_then(|it| it.duration_since(std::time::UNIX_EPOCH).ok());
    #[cfg(unix)]
    let node = std::os::unix::fs::MetadataExt::ino(&meta);
    #[cfg(not(unix))]
    let node = 0;
    format!("{}:{}:{node}", meta.len(), at.map_or(0, |it| it.as_nanos()))
}

/// What is at `path`: nothing, a file's bytes, a directory's names, or something else (a
/// device, a socket), by a hash.
pub(crate) fn digest(path: &Path) -> String {
    let Ok(meta) = std::fs::metadata(path) else { return "-".to_owned() };
    let mut hash = Sha256::new();
    if meta.is_dir() {
        let mut names: Vec<_> = std::fs::read_dir(path)
            .into_iter()
            .flatten()
            .filter_map(|it| it.ok().map(|it| it.file_name()))
            .collect();
        names.sort();
        hash.update(b"d");
        for name in names {
            hash.update(name.as_encoded_bytes());
            hash.update([0]);
        }
    } else if meta.is_file() {
        hash.update(b"f");
        match std::fs::File::open(path) {
            Ok(mut file) => _ = std::io::copy(&mut file, &mut hash),
            Err(_) => hash.update(b"unreadable"),
        }
    } else {
        hash.update(b"o");
    }
    hash.finalize().iter().take(16).map(|it| format!("{it:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(reached: Reached, test: &str, what: &str) -> Line {
        Line { reached, test: test.to_owned(), what: what.to_owned() }
    }

    #[test]
    fn what_was_refused_is_said_in_a_line() {
        let w = |it| line(Reached::Write, "t", it);
        let (a, b, n) = (w("/a"), w("/b"), line(Reached::Net, "t", "1.2.3.4:443"));
        assert_eq!(
            what_was_refused(&[&a, &b, &n]),
            "writing `/a` and `/b` (the machine is read-only in a sandbox) and connecting to \
             `1.2.3.4:443` (a sandbox has no network but its own loopback)"
        );
        let (c, d) = (w("/c"), w("/d"));
        assert_eq!(
            what_was_refused(&[&a, &b, &c, &d]),
            "writing `/a`, `/b`, `/c` and 1 more (the machine is read-only in a sandbox)"
        );
    }

    #[test]
    fn a_package_takes_its_tests_out_by_its_manifest() {
        let dir = std::env::temp_dir().join(format!("ra-jit-opt-out-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let write = |text: &str| std::fs::write(dir.join("Cargo.toml"), text).unwrap();
        write("[package]\nname = \"a\"\n");
        let out = opt_out(&dir).unwrap();
        assert!(!out.all && out.tests.is_empty());
        write(
            "[package]\nname = \"a\"\n[package.metadata.blaze.sandbox]\ntests = false\n\
             unsandboxed-tests = [\"tests::x\"]\n",
        );
        let out = opt_out(&dir).unwrap();
        assert!(out.all && out.tests.contains("tests::x"));
        write("[package]\nname = \"a\"\n[package.metadata.blaze.sandbox]\ntest = false\n");
        assert!(opt_out(&dir).is_err());
        write("[package]\nname = \"a\"\n[package.metadata.blaze.sandbox]\nbuild-script = false\n");
        assert!(opt_out(&dir).unwrap().build_script_outside);
        _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_workspace_takes_build_scripts_and_proc_macros_out_by_its_root_manifest() {
        let dir = std::env::temp_dir().join(format!("ra-jit-ws-opt-out-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("member/src")).unwrap();
        let write = |text: &str| std::fs::write(dir.join("Cargo.toml"), text).unwrap();
        std::fs::write(dir.join("member/Cargo.toml"), "[package]\nname = \"m\"\n").unwrap();
        write("[workspace]\nmembers = [\"member\"]\n");
        assert_eq!(workspace_root(&dir.join("member/src")).unwrap(), Some(dir.clone()));
        std::fs::write(dir.join("member/Cargo.toml"), "[package\n").unwrap();
        assert!(workspace_root(&dir.join("member/src")).is_err());
        std::fs::write(dir.join("member/Cargo.toml"), "[package]\nname = \"m\"\n").unwrap();
        let out = workspace_opt_out(&dir).unwrap();
        assert!(out.build_scripts.is_empty() && out.proc_macros.is_empty());
        write(
            "[workspace]\n[workspace.metadata.blaze.sandbox]\n\
             unsandboxed-build-scripts = [\"llama-cpp-sys-2\"]\n\
             unsandboxed-proc-macros = [\"my-derive\"]\n",
        );
        let out = workspace_opt_out(&dir).unwrap();
        assert!(out.build_scripts.contains("llama-cpp-sys-2"));
        assert!(out.macro_outside("my_derive") && !out.macro_outside("serde_derive"));
        write("[workspace]\n[workspace.metadata.blaze.sandbox]\nunsandboxed-scripts = []\n");
        assert!(workspace_opt_out(&dir).is_err());
        write("[workspace]\n[workspace.metadata.blaze.sandbox]\nunsandboxed-proc-macros = \"x\"\n");
        assert!(workspace_opt_out(&dir).is_err());
        _ = std::fs::remove_dir_all(&dir);
    }
}
