//! Who receives what a script printed, as cargo 1.99 hands it out: `build_map` in
//! `src/compiler/custom_build.rs` (which scripts each unit is linked with), `add_native_deps`,
//! `add_plugin_deps` and `add_custom_flags` in `src/compiler/mod.rs` (what rustc is given), and
//! `fingerprint/mod.rs` (when a script runs again), in rust-lang/cargo at 5f94df478.

use std::{
    borrow::Cow,
    collections::BTreeSet,
    hash::Hash,
    path::{Path, PathBuf},
    time::SystemTime,
};

use blaze_hash::{HashMap, HashSet};

use crate::{
    output::{LibraryPath, LinkArgTarget, Output},
    unit::{Dependency, Package, Target},
};

impl LinkArgTarget {
    /// Whether the argument is for `target`, built as a test when `test`.
    pub fn applies_to(&self, target: &Target, test: bool) -> bool {
        match self {
            LinkArgTarget::All => true,
            LinkArgTarget::Cdylib => !test && target.is_cdylib(),
            LinkArgTarget::Bin => target.is_bin(),
            LinkArgTarget::SingleBin(name) => target.is_bin() && target.name == *name,
            LinkArgTarget::Test => target.is_test(),
            LinkArgTarget::Bench => target.is_bench(),
            LinkArgTarget::Example => target.is_exe_example(),
        }
    }
}

/// A script's output among those a unit is linked with: whether it is the unit's own
/// package's.
#[derive(Clone, Copy, Debug)]
pub struct Linked<'a> {
    pub own: bool,
    pub output: &'a Output,
}

/// What rustc is given for one target of a package, from the scripts it is linked with.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Compile {
    /// `-L`, those inside a script's `OUT_DIR` first.
    pub search: Vec<PathBuf>,
    /// `-l`.
    pub libs: Vec<String>,
    /// `-C link-arg=`.
    pub link_args: Vec<String>,
    /// `--cfg`.
    pub cfgs: Vec<String>,
    /// `--check-cfg`.
    pub check_cfgs: Vec<String>,
    /// Variables rustc runs with, and so `env!` reads.
    pub env: Vec<(String, String)>,
}

impl Compile {
    /// What `target` of `package` is given. `linked` is what [`to_link`] lists for the unit,
    /// in its order; `own` are the outputs of the package's own script runs for the unit.
    pub fn of(
        package: &Package,
        target: &Target,
        test: bool,
        linked: &[Linked<'_>],
        own: &[&Output],
    ) -> Compile {
        // A program of a package with a library links the library, which has the `-l`s.
        let pass_l = target.is_lib() || !package.targets.iter().any(Target::is_lib);
        let mut compile = Compile::default();
        let paths = || linked.iter().flat_map(|it| &it.output.library_paths);
        compile.search.reserve_exact(paths().count());
        // Those in a script's `OUT_DIR` first, each part in the order listed.
        for external in [false, true] {
            for path in paths().filter(|it| matches!(it, LibraryPath::External(_)) == external) {
                // alloc: the edge: what codegen is given, once per unit, owned (request 7).
                compile.search.push(path.path().to_owned());
            }
        }
        for it in linked {
            if it.own && pass_l {
                compile.libs.extend(it.output.library_links.iter().cloned());
            }
            for (to, arg) in &it.output.linker_args {
                // cdylib arguments reach a cdylib from its dependencies too: cargo's issue 9562.
                if to.applies_to(target, test) && (it.own || *to == LinkArgTarget::Cdylib) {
                    // clone: the edge: what codegen is given, once per unit, owned (request 7).
                    compile.link_args.push(arg.clone());
                }
            }
        }
        for output in own {
            compile.cfgs.extend(output.cfgs.iter().cloned());
            compile.check_cfgs.extend(output.check_cfgs.iter().cloned());
            compile.env.extend(output.env.iter().cloned());
        }
        compile
    }

    /// rustc's arguments, in cargo's order, borrowed from `self` but for a path that is not
    /// UTF-8 (read lossily) and each `link-arg=`.
    pub fn args(&self) -> Vec<Cow<'_, str>> {
        let count = self.search.len()
            + self.libs.len()
            + self.link_args.len()
            + self.cfgs.len()
            + self.check_cfgs.len();
        let mut args = Vec::with_capacity(2 * count);
        for path in &self.search {
            args.extend([Cow::Borrowed("-L"), path.to_string_lossy()]);
        }
        for lib in &self.libs {
            args.extend([Cow::Borrowed("-l"), Cow::Borrowed(lib.as_str())]);
        }
        for arg in &self.link_args {
            let mut link_arg = String::with_capacity("link-arg=".len() + arg.len());
            link_arg.push_str("link-arg=");
            link_arg.push_str(arg);
            args.extend([Cow::Borrowed("-C"), Cow::Owned(link_arg)]);
        }
        for cfg in &self.cfgs {
            args.extend([Cow::Borrowed("--cfg"), Cow::Borrowed(cfg.as_str())]);
        }
        for cfg in &self.check_cfgs {
            args.extend([Cow::Borrowed("--check-cfg"), Cow::Borrowed(cfg.as_str())]);
        }
        args
    }
}

/// The scripts a unit is linked with, and those of its host-only dependencies (build
/// dependencies, proc macros), whose library directories are put on the path dynamic
/// libraries are found by. Each is a run of a script.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildScripts<R> {
    /// Topologically sorted, each once.
    pub to_link: Vec<R>,
    pub plugins: BTreeSet<R>,
}

/// One unit of the graph, as [`to_link`] reads it.
pub struct GraphUnit<U, R> {
    /// The units it depends on, sorted by their packages' ids as cargo sorts them.
    pub deps: Vec<U>,
    /// The runs of its package's script it is built with, unless it is the script itself or
    /// its package has none.
    pub own_runs: Vec<R>,
    /// A build script, proc macro or build dependency: built for the host.
    pub for_host: bool,
    /// A library that is linked into what depends on it.
    pub linkable: bool,
}

/// What [`BuildScripts`] `unit` has in `graph`, as cargo's `build_map` computes it: an owned
/// copy of [`to_link_in`]'s.
pub fn to_link<U, R, G>(
    unit: U,
    graph: &G,
    memo: &mut HashMap<U, BuildScripts<R>>,
) -> BuildScripts<R>
where
    U: Copy + Eq + Hash,
    R: Copy + Eq + Hash + Ord,
    G: Fn(&U) -> GraphUnit<U, R> + ?Sized,
{
    // clone: the owned copy this signature hands back; `to_link_in` lends it (request 8).
    to_link_in(unit, graph, memo).clone()
}

/// What [`BuildScripts`] `unit` has in `graph`, lent from `memo`, which keeps it and those of
/// every dependency it receives scripts from. Each dependency's node is made once per edge,
/// and one that is neither for the host nor linked gives nothing, so is not followed.
pub fn to_link_in<'m, U, R, G>(
    unit: U,
    graph: &G,
    memo: &'m mut HashMap<U, BuildScripts<R>>,
) -> &'m BuildScripts<R>
where
    U: Copy + Eq + Hash,
    R: Copy + Eq + Hash + Ord,
    G: Fn(&U) -> GraphUnit<U, R> + ?Sized,
{
    if !memo.contains_key(&unit) {
        let node = graph(&unit);
        fill(unit, node, graph, memo);
    }
    &memo[&unit]
}

fn fill<U, R, G>(unit: U, node: GraphUnit<U, R>, graph: &G, memo: &mut HashMap<U, BuildScripts<R>>)
where
    U: Copy + Eq + Hash,
    R: Copy + Eq + Hash + Ord,
    G: Fn(&U) -> GraphUnit<U, R> + ?Sized,
{
    let mut seen = HashSet::default();
    let mut scripts =
        BuildScripts { to_link: Vec::with_capacity(node.own_runs.len()), plugins: BTreeSet::new() };
    for &run in &node.own_runs {
        if seen.insert(run) {
            scripts.to_link.push(run);
        }
    }
    for &dep in &node.deps {
        let dep_node = graph(&dep);
        let (for_host, linkable) = (dep_node.for_host, dep_node.linkable);
        if !for_host && !linkable {
            continue;
        }
        if !memo.contains_key(&dep) {
            fill(dep, dep_node, graph, memo);
        }
        let of_dep = &memo[&dep];
        if for_host {
            scripts.plugins.extend(of_dep.to_link.iter().copied());
        } else {
            for &run in &of_dep.to_link {
                if seen.insert(run) {
                    scripts.to_link.push(run);
                }
            }
        }
    }
    memo.insert(unit, scripts);
}

impl Dependency {
    /// A dependency whose script printed `output`, as the scripts of the packages that depend
    /// on it directly see it.
    pub fn of(name: &str, links: Option<&str>, output: &Output) -> Dependency {
        Dependency {
            // alloc: setup: the unit's description, made once per dependency edge.
            name: name.to_owned(),
            links: links.map(str::to_owned),
            // clone: setup: the unit's description owns its dependencies' metadata.
            metadata: output.metadata.clone(),
        }
    }
}

/// When a script runs again.
#[derive(Debug, PartialEq, Eq)]
pub enum Rerun {
    /// It named nothing it reads: for any change to the files of its package (the package's
    /// sources' newest time for a path dependency, its version for a registry's).
    Package,
    /// For a change to these: each file (relative to the package's directory) or directory
    /// (anything below it), and each variable.
    Watched { files: Vec<PathBuf>, env: Vec<String> },
}

impl Output {
    pub fn rerun(&self, package_dir: &Path) -> Rerun {
        if self.rerun_if_changed.is_empty() && self.rerun_if_env_changed.is_empty() {
            return Rerun::Package;
        }
        let mut files = Vec::with_capacity(self.rerun_if_changed.len());
        for it in &self.rerun_if_changed {
            // alloc: setup: the rule kept with a run, made once when the run is kept.
            files.push(it.strip_prefix(package_dir).unwrap_or(it).to_owned());
        }
        // clone: setup: the rule kept with a run, made once when the run is kept.
        Rerun::Watched { files, env: self.rerun_if_env_changed.clone() }
    }
}

/// Why a script whose run is kept runs again.
#[derive(Debug, PartialEq, Eq)]
pub enum Stale {
    Missing(PathBuf),
    Newer(PathBuf),
    Env { name: String, then: Option<String>, now: Option<String> },
}

impl Rerun {
    /// Whether a run that began at `ran` must run again. A variable's value is `[env]`'s when
    /// that has it, else cargo's environment's: `then` is what it was at the run, `now` what
    /// it is. A file newer than `ran` is a change; one as old is not. `None` for
    /// [`Rerun::Package`], whose files the package's source says. Each file's path is joined
    /// in the thread's path buffer ([`blaze_mem::scratch::with_path`]): a check that finds
    /// nothing stale allocates nothing once that buffer has grown, a directory's walk aside.
    pub fn stale<'v, T, N>(
        &self,
        package_dir: &Path,
        ran: SystemTime,
        then: &T,
        now: &N,
    ) -> Option<Option<Stale>>
    where
        T: Fn(&str) -> Option<&'v str> + ?Sized,
        N: Fn(&str) -> Option<&'v str> + ?Sized,
    {
        let Rerun::Watched { files, env } = self else { return None };
        let changed = blaze_mem::scratch::with_path(|path| {
            for file in files {
                path.push(package_dir);
                path.push(file);
                match newest(path) {
                    // clone: the reason the run is stale, made once, when it is.
                    None => return Some(Stale::Missing(path.clone())),
                    // clone: as above.
                    Some(it) if it > ran => return Some(Stale::Newer(path.clone())),
                    Some(_) => {}
                }
                path.as_mut_os_string().clear();
            }
            None
        });
        if changed.is_some() {
            return Some(changed);
        }
        for name in env {
            let (then, now) = (then(name), now(name));
            if then != now {
                // clone: the reason the run is stale, made once, when it is.
                let name = name.clone();
                // alloc: as above.
                let (then, now) = (then.map(str::to_owned), now.map(str::to_owned));
                return Some(Some(Stale::Env { name, then, now }));
            }
        }
        Some(None)
    }
}

/// Which directory a walk is in, for visiting each once however links reach it: its device
/// and inode on Unix, from the metadata already read (no call to the system); elsewhere its
/// canonical path. A directory reached twice adds no time, so either gives the same newest.
#[derive(PartialEq, Eq, Hash)]
enum DirId {
    #[cfg(unix)]
    Inode(u64, u64),
    #[cfg(not(unix))]
    Path(PathBuf),
}

impl DirId {
    #[cfg(unix)]
    fn of(_: &Path, meta: &std::fs::Metadata) -> Option<DirId> {
        use std::os::unix::fs::MetadataExt;
        Some(DirId::Inode(meta.dev(), meta.ino()))
    }

    #[cfg(not(unix))]
    fn of(path: &Path, _: &std::fs::Metadata) -> Option<DirId> {
        std::fs::canonicalize(path).ok().map(DirId::Path)
    }
}

/// The newest time of the file at `path`, or of the directory there and everything below
/// it, as cargo's `mtime_recursive` takes it: links followed, a link's own time and its
/// target's, what cannot be read left out. An entry's own time is read from its directory
/// (`fstatat` on Unix), and its path made only for a link or a directory. A file's time is one
/// `stat` on a path std passes from the stack; a directory's walk allocates what std's
/// `read_dir` does (its handle and its path, per directory).
fn newest(path: &Path) -> Option<SystemTime> {
    let meta = std::fs::metadata(path).ok()?;
    let mut newest = meta.modified().ok()?;
    if !meta.is_dir() {
        return Some(newest);
    }
    let mut visited = HashSet::default();
    visited.extend(DirId::of(path, &meta));
    // alloc: setup: one stack per walk, one path per directory below.
    let mut pending = vec![path.to_owned()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let Ok(link) = entry.metadata() else { continue };
            if let Ok(it) = link.modified() {
                newest = newest.max(it);
            }
            let is_link = link.file_type().is_symlink();
            if !is_link && !link.is_dir() {
                continue;
            }
            let path = entry.path();
            let target = match is_link {
                true => std::fs::metadata(&path).ok(),
                false => Some(link),
            };
            let Some(target) = target else { continue };
            if is_link && let Ok(it) = target.modified() {
                newest = newest.max(it);
            }
            if target.is_dir() && DirId::of(&path, &target).is_some_and(|it| visited.insert(it)) {
                pending.push(path);
            }
        }
    }
    Some(newest)
}

#[cfg(test)]
mod tests;
