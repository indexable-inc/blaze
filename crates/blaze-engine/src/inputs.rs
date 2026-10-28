//! What a checkout was loaded from besides the source: its manifests and lock file, cargo's
//! configuration, the flags for rustc in the environment, and what its build scripts read.
//! Before each build the daemon looks whether they are still what it loaded, runs the build
//! scripts again where cargo would, and says which checkouts are to be loaded again.

use std::{
    hash::{Hash as _, Hasher as _},
    path::{Path, PathBuf},
};

use ide_db::base_db::{Env, TargetKind};
use project_model::{CargoConfig, ProjectWorkspace, ProjectWorkspaceKind};
use rustc_hash::{FxHashMap, FxHasher};

/// The variables cargo takes flags for rustc from.
const FLAGS: &[&str] = &["RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_BUILD_RUSTFLAGS"];

/// What each of the daemon's checkouts that is loaded in full was loaded from.
#[derive(Default)]
pub(crate) struct Inputs {
    checkouts: Vec<Checkout>,
}

/// What the workspace at `root` was loaded from.
struct Checkout {
    root: PathBuf,
    /// The manifests and the lock file, the build scripts cargo finds by their name, and
    /// the files cargo's configuration is read from: those that are not there too.
    files: Vec<Watched>,
    /// The variables of `FLAGS` as cargo was run with them.
    flags: Vec<(&'static str, Option<String>)>,
    scripts: Option<Scripts>,
}

/// A file or a directory, and what it was when the daemon read it.
struct Watched {
    path: PathBuf,
    /// Whether the directory is a package, of which cargo leaves out what a build wrote and
    /// the packages inside.
    package: bool,
    stamp: u64,
}

impl Watched {
    fn new(path: PathBuf, package: bool) -> Watched {
        let stamp = stamp(&path, package);
        Watched { path, package, stamp }
    }

    fn changed(&self) -> bool {
        stamp(&self.path, self.package) != self.stamp
    }
}

/// The build scripts of one workspace and what they said.
struct Scripts {
    workspace: ProjectWorkspace,
    config: CargoConfig,
    /// The scripts of the workspace's own packages, with what cargo runs each again for a
    /// change of.
    scripts: Vec<Script>,
    /// The variables it runs one again for a change of, as they were when they last ran.
    variables: Vec<(String, Option<String>)>,
}

/// The build script of one package of the workspace.
struct Script {
    /// Where cargo had it write for the daemon's load.
    out_dir: PathBuf,
    /// The directories cargo's own builds run it in: `<package>-<hash>` below these.
    builds: Vec<PathBuf>,
    package: String,
    /// The package's directory, which cargo runs the script in.
    root: PathBuf,
    /// Where cargo ran rustc, which the files rustc read are named from.
    workspace: PathBuf,
    files: Vec<Watched>,
}

impl Script {
    /// Whether what it reads changed, or a clean took away what it wrote: what it watches
    /// in its package leaves out the target directory, as cargo's does.
    fn changed(&self) -> bool {
        !self.out_dir.is_dir() || self.files.iter().any(Watched::changed)
    }

    /// Whether the script, run again as cargo ran it, prints and writes what it did for the
    /// load: what cargo would find when it ran it, without asking cargo. Not when the script
    /// itself is older than its source, or was compiled reading a variable cargo does not
    /// set; cargo compiles it again.
    fn reruns_the_same(&self) -> bool {
        use crate::cargo::script;
        let Some(out_dir) = self.ran() else { return false };
        if !script::compiled_from_what_is_there(&out_dir, &self.workspace) {
            return false;
        }
        // `blaze` ran it again before it asked for this build, as it does when
        // something it reads changed: that is not done twice.
        let newest = self.files.iter().map(|it| newest(&it.path, it.package));
        let newest = newest.collect::<Option<Vec<_>>>().and_then(|it| it.into_iter().max());
        if newest.is_some_and(|it| script::checked_after(&out_dir, it))
            && script::same_run(&out_dir, &self.out_dir)
        {
            return true;
        }
        script::rerun_matches(&out_dir, &self.out_dir).is_ok()
    }

    /// Where a run of the script that can be run again wrote: the load's, or else the last
    /// one of cargo's own builds of the package, which `blaze` has write down how it
    /// ran. The script and what it reads are the same for both.
    fn ran(&self) -> Option<PathBuf> {
        if crate::cargo::script::can_rerun(&self.out_dir) {
            return Some(self.out_dir.clone());
        }
        // `build/<package>-<hash>/out` in cargo's first layout, `build/<package>/<hash>/out`
        // in its second.
        let prefix = format!("{}-", self.package);
        let first = self.builds.iter().filter_map(|it| std::fs::read_dir(it).ok()).flatten();
        let first = first
            .flatten()
            .filter(|it| it.file_name().to_str().is_some_and(|it| it.starts_with(&prefix)))
            .map(|it| it.path());
        let second =
            self.builds.iter().filter_map(|it| std::fs::read_dir(it.join(&self.package)).ok());
        let second = second.flatten().flatten().map(|it| it.path());
        let runs = first.chain(second).map(|it| it.join("out"));
        let runs =
            runs.filter(|it| crate::cargo::script::ran_in(it).is_some_and(|it| it == self.root));
        runs.max_by_key(|it| {
            let printed = crate::cargo::script::printed(it);
            printed.and_then(|it| std::fs::metadata(it).and_then(|it| it.modified()).ok())
        })
    }

    fn stamp_again(&mut self) {
        for file in &mut self.files {
            file.stamp = stamp(&file.path, file.package);
        }
    }
}

/// The variable `name` as the commands of `config` are run with it.
fn run_with(config: &CargoConfig, name: &str) -> Option<String> {
    match config.extra_env.get(name) {
        Some(value) => value.clone(),
        None => std::env::var(name).ok(),
    }
}

impl Inputs {
    /// Has the workspace at `root`, loaded with `config`, be one whose inputs are watched.
    pub(crate) fn watch(
        &mut self,
        root: &Path,
        workspace: &ProjectWorkspace,
        config: &CargoConfig,
    ) {
        let mut files = Vec::new();
        for dir in root.ancestors() {
            for name in ["config.toml", "config"] {
                files.push(Watched::new(dir.join(".cargo").join(name), false));
            }
        }
        for name in ["Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "rust-toolchain"] {
            files.push(Watched::new(root.join(name), false));
        }
        if let ProjectWorkspaceKind::Cargo { cargo, .. } = &workspace.kind {
            for package in cargo.packages().filter(|&it| cargo[it].is_local) {
                let manifest: &Path = cargo[package].manifest.as_ref();
                files.push(Watched::new(manifest.to_owned(), false));
                // A script that appears there is run without the manifest naming it.
                files.push(Watched::new(manifest.with_file_name("build.rs"), false));
                // A proc macro is executable input to its callers, rather than source
                // those callers can update in the database without loading it again.
                if cargo[package].targets.iter().any(|&it| cargo[it].kind.is_proc_macro()) {
                    files.push(Watched::new(manifest.parent().unwrap().to_owned(), true));
                }
            }
        }
        let flags = FLAGS.iter().map(|&it| (it, run_with(config, it))).collect();
        let scripts = Scripts::of(workspace, config);
        self.checkouts.retain(|it| it.root != root);
        self.checkouts.push(Checkout { root: root.to_owned(), files, flags, scripts });
    }

    /// Whether the checkout at `root` is no longer what it was loaded from. It is asked for
    /// a process with the variables `env`, or with `None` for whoever it was loaded for.
    /// An error is why its build scripts can't say.
    pub(crate) fn stale(&mut self, root: &Path, env: Option<&Env>) -> anyhow::Result<bool> {
        let Some(checkout) = self.checkouts.iter_mut().find(|it| it.root == root) else {
            return Ok(false);
        };
        if checkout.files.iter().any(Watched::changed) {
            return Ok(true);
        }
        if let Some(env) = env
            && checkout.flags.iter().any(|(name, was)| env.get(name) != *was)
        {
            return Ok(true);
        }
        match &mut checkout.scripts {
            Some(scripts) => scripts.changed(env),
            None => Ok(false),
        }
    }

    /// The variables cargo and the build scripts of the checkout at `root` read, as a
    /// process with `env` has them.
    pub(crate) fn variables(&self, root: &Path, env: &Env) -> FxHashMap<String, Option<String>> {
        let scripts = self.checkouts.iter().find(|it| it.root == root);
        let scripts = scripts.and_then(|it| it.scripts.as_ref());
        let read = scripts.into_iter().flat_map(|it| it.variables.iter().map(|it| &*it.0));
        FLAGS.iter().copied().chain(read).map(|it| (it.to_owned(), env.get(it))).collect()
    }
}

impl Scripts {
    fn of(workspace: &ProjectWorkspace, config: &CargoConfig) -> Option<Scripts> {
        let ProjectWorkspaceKind::Cargo { cargo, build_scripts, .. } = &workspace.kind else {
            return None;
        };
        let (mut scripts, mut variables) = (Vec::new(), Vec::new());
        // Every script in the sandbox is given these; a table that can't be read failed the
        // load that ran them.
        let root: &Path = cargo.workspace_root().as_ref();
        let given = crate::cargo::sandboxed::workspace_opt_out(root).map(|it| it.build_script_env);
        variables.extend(given.unwrap_or_default().into_iter().map(|it| {
            let value = run_with(config, &it);
            (it, value)
        }));
        for package in cargo.packages() {
            let Some(out_dir) = build_scripts.out_dir(package) else { continue };
            let root: &Path = cargo[package].manifest.parent().as_ref();
            let said = Said::read(out_dir.as_ref());
            variables.extend(said.variables.into_iter().map(|it| {
                let value = run_with(config, &it);
                (it, value)
            }));
            if !cargo[package].is_local {
                continue;
            }
            let script = cargo[package].targets.iter().map(|&it| &cargo[it]);
            let script = script.filter(|it| it.kind == TargetKind::BuildScript);
            let mut files: Vec<Watched> =
                script.map(|it| Watched::new(PathBuf::from(it.root.as_str()), false)).collect();
            match said.watches {
                // A script that names nothing is run again when any file of its package changes.
                false => files.push(Watched::new(root.to_owned(), true)),
                true => files
                    .extend(said.files.into_iter().map(|it| Watched::new(root.join(it), false))),
            }
            let out_dir: &Path = out_dir.as_ref();
            let workspace: &Path = cargo.workspace_root().as_ref();
            scripts.push(Script {
                out_dir: out_dir.to_owned(),
                builds: builds_of(cargo, config),
                package: cargo[package].name.clone(),
                root: root.to_owned(),
                workspace: workspace.to_owned(),
                files,
            });
        }
        variables.sort();
        variables.dedup();
        if scripts.is_empty() && variables.is_empty() {
            return None;
        }
        Some(Scripts { workspace: workspace.clone(), config: config.clone(), scripts, variables })
    }

    /// Runs the scripts again where cargo would for a process with `env`, or with `None`
    /// for one with the variables they last ran with: whether what they say is no longer
    /// what was loaded.
    fn changed(&mut self, env: Option<&Env>) -> anyhow::Result<bool> {
        let changed: Vec<usize> =
            (0..self.scripts.len()).filter(|&at| self.scripts[at].changed()).collect();
        let variables =
            env.is_some_and(|env| self.variables.iter().any(|(name, was)| env.get(name) != *was));
        if changed.is_empty() && !variables {
            return Ok(false);
        }
        // Cargo would run only the scripts whose files changed: each is run again here as
        // cargo ran it, which costs the script and not a cargo that resolves the workspace.
        if !variables && changed.iter().all(|&at| self.scripts[at].reruns_the_same()) {
            changed.into_iter().for_each(|at| self.scripts[at].stamp_again());
            return Ok(false);
        }
        let ProjectWorkspaceKind::Cargo { build_scripts: loaded, .. } = &self.workspace.kind else {
            return Ok(false);
        };
        let mut config = self.config.clone();
        for (name, was) in &self.variables {
            let value = env.map_or_else(|| was.clone(), |env| env.get(name));
            config.extra_env.insert(name.clone(), value);
        }
        let said = crate::events::within(crate::events::Step::BuildScripts, None, || {
            self.workspace.run_build_scripts(&config, &|_| {})
        })?;
        if let Some(error) = said.error() {
            anyhow::bail!("a build script failed: {}", error.trim().replace('\n', " "));
        }
        if said != *loaded {
            return Ok(true);
        }
        self.scripts.iter_mut().for_each(Script::stamp_again);
        for (name, was) in &mut self.variables {
            *was = config.extra_env.get(name).cloned().flatten();
        }
        self.config = config;
        Ok(false)
    }
}

/// The directories cargo runs the build scripts of the build `config` asks for in:
/// `build` of the profile's directory, for the target that is asked for or the host.
fn builds_of(cargo: &project_model::CargoWorkspace, config: &CargoConfig) -> Vec<PathBuf> {
    let profile = config.extra_args.iter().skip_while(|it| *it != "--profile").nth(1);
    let profile = match profile.map(String::as_str) {
        None | Some("dev" | "test") => "debug",
        Some("bench") => "release",
        Some(other) => other,
    };
    let target: &Path = cargo.target_directory().as_ref();
    let mut dirs = vec![target.to_owned()];
    dirs.extend(cargo.build_directory().map(|it| AsRef::<Path>::as_ref(it).to_owned()));
    let triples = config.target.iter();
    let under: Vec<PathBuf> =
        dirs.iter().flat_map(|dir| triples.clone().map(move |it| dir.join(it))).collect();
    dirs.extend(under);
    dirs.into_iter().map(|it| it.join(profile).join("build")).collect()
}

/// What a build script asked cargo to watch.
#[derive(Default)]
struct Said {
    /// Whether it named anything: files or variables.
    watches: bool,
    files: Vec<String>,
    variables: Vec<String>,
}

impl Said {
    /// Reads what the script that wrote to `out_dir` printed: cargo keeps it next to it.
    fn read(out_dir: &Path) -> Said {
        let mut said = Said::default();
        let output = crate::cargo::script::printed(out_dir);
        let Some(Ok(output)) = output.map(std::fs::read_to_string) else { return said };
        for line in output.lines() {
            let Some(line) = line.strip_prefix("cargo:") else { continue };
            let line = line.strip_prefix(':').unwrap_or(line);
            if let Some(file) = line.strip_prefix("rerun-if-changed=") {
                said.watches = true;
                said.files.push(file.to_owned());
            } else if let Some(variable) = line.strip_prefix("rerun-if-env-changed=") {
                said.watches = true;
                said.variables.push(variable.to_owned());
            }
        }
        said
    }
}

/// A number that changes when the file at `path`, or a file below the directory there, is
/// written, added or removed.
fn stamp(path: &Path, package: bool) -> u64 {
    let mut hasher = FxHasher::default();
    stamp_into(path, package, &mut hasher);
    hasher.finish()
}

/// When the file at `path`, or the directory there or anything below it that [`stamp`] looks
/// at, was last written. Nothing when it is not there.
fn newest(path: &Path, package: bool) -> Option<std::time::SystemTime> {
    let metadata = std::fs::metadata(path).ok()?;
    let mut newest = metadata.modified().ok()?;
    if metadata.is_dir() {
        for entry in std::fs::read_dir(path).ok()? {
            let entry = entry.ok()?.path();
            let built = entry.join("CACHEDIR.TAG").exists() || entry.ends_with(".git");
            if package && (built || entry.join("Cargo.toml").exists()) {
                continue;
            }
            newest = newest.max(self::newest(&entry, package)?);
        }
    }
    Some(newest)
}

fn stamp_into(path: &Path, package: bool, hasher: &mut FxHasher) {
    let Ok(metadata) = std::fs::metadata(path) else { return };
    if !metadata.is_dir() {
        (metadata.len(), metadata.modified().ok()).hash(hasher);
        return;
    }
    let Ok(entries) = std::fs::read_dir(path) else { return };
    let mut entries: Vec<PathBuf> = entries.filter_map(|it| Some(it.ok()?.path())).collect();
    entries.sort();
    for entry in entries {
        let built = entry.join("CACHEDIR.TAG").exists() || entry.ends_with(".git");
        if package && (built || entry.join("Cargo.toml").exists()) {
            continue;
        }
        entry.hash(hasher);
        stamp_into(&entry, package, hasher);
    }
}
