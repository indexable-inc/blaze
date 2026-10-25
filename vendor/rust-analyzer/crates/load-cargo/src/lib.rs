//! Loads a Cargo project into a static instance of analysis, without support
//! for incorporating changes.
// Note, don't remove any public api from this. This API is consumed by external tools
// to run rust-analyzer as a library.

#![cfg_attr(feature = "in-rust-tree", feature(rustc_private))]

#[cfg(feature = "in-rust-tree")]
extern crate rustc_driver as _;

mod listed;
mod wrap;
pub use listed::{is_listed, serve_listed_with};
pub use wrap::{Wrap, asked, wrap_expanders};

use std::{any::Any, collections::hash_map::Entry, mem, path::Path, sync};

use crossbeam_channel::unbounded;
use hir_expand::proc_macro::{
    ProcMacro, ProcMacroExpander, ProcMacroExpansionError, ProcMacroKind, ProcMacroLoadResult,
};
use ide_db::{
    ChangeWithProcMacros, FxHashMap, FxHashSet, RootDatabase,
    base_db::{
        Crate, CrateGraphBuilder, Env, LibraryRoots, LocalRoots, ProcMacroLoadingError,
        SourceDatabase, SourceRoot, SourceRootId,
        salsa::{Database as _, Durability, Setter},
    },
    prime_caches,
};
use itertools::Itertools;
use proc_macro_api::{
    MacroDylib, ProcMacroClient,
    bidirectional_protocol::msg::{ParentSpan, SubRequest, SubResponse},
};
use project_model::{CargoConfig, PackageRoot, ProjectManifest, ProjectWorkspace};
use span::{Span, SpanAnchor, SyntaxContext};
use tt::{TextRange, TextSize};
use vfs::{
    AbsPath, AbsPathBuf, FileId, VfsPath,
    file_set::FileSetConfig,
    loader::{Handle, LoadingProgress},
};

#[derive(Debug)]
pub struct LoadCargoConfig {
    pub load_out_dirs_from_check: bool,
    pub with_proc_macro_server: ProcMacroServerChoice,
    pub prefill_caches: bool,
    pub num_worker_threads: usize,
    pub proc_macro_processes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcMacroServerChoice {
    Sysroot,
    Explicit(AbsPathBuf),
    None,
}

pub fn load_workspace_at(
    root: &Path,
    cargo_config: &CargoConfig,
    load_config: &LoadCargoConfig,
    progress: &(dyn Fn(String) + Sync),
) -> anyhow::Result<(RootDatabase, vfs::Vfs, Option<ProcMacroClient>)> {
    let root = AbsPathBuf::assert_utf8(std::env::current_dir()?.join(root));
    let root = ProjectManifest::discover_single(&root)?;
    let manifest_path = root.manifest_path().clone();
    let mut workspace = ProjectWorkspace::load(root, cargo_config, progress)?;

    if load_config.load_out_dirs_from_check {
        let build_scripts = workspace.run_build_scripts(cargo_config, progress)?;
        if let Some(error) = build_scripts.error() {
            tracing::debug!(
                "Errors occurred while running build scripts for {}: {}",
                manifest_path,
                error
            );
        }
        workspace.set_build_scripts(build_scripts)
    }

    load_workspace(workspace, &cargo_config.extra_env, load_config)
}

pub fn load_workspace(
    ws: ProjectWorkspace,
    extra_env: &FxHashMap<String, Option<String>>,
    load_config: &LoadCargoConfig,
) -> anyhow::Result<(RootDatabase, vfs::Vfs, Option<ProcMacroClient>)> {
    let lru_cap = std::env::var("RA_LRU_CAP").ok().and_then(|it| it.parse::<u16>().ok());
    let mut db = RootDatabase::new(lru_cap);

    let (vfs, proc_macro_server) = load_workspace_into_db(ws, extra_env, load_config, &mut db)?;

    Ok((db, vfs, proc_macro_server))
}

// This variant of `load_workspace` allows deferring the loading of rust-analyzer
// into an existing database, which is useful in certain third-party scenarios,
// now that `salsa` supports extending foreign databases (e.g. `RootDatabase`).
pub fn load_workspace_into_db(
    ws: ProjectWorkspace,
    extra_env: &FxHashMap<String, Option<String>>,
    load_config: &LoadCargoConfig,
    db: &mut RootDatabase,
) -> anyhow::Result<(vfs::Vfs, Option<ProcMacroClient>)> {
    load_workspaces_into_db(vec![ws], extra_env, load_config, db)
}

/// Loads several workspaces into one database. A crate that is the same in two of them, as
/// the dependencies of two checkouts of a project are, is one crate of the database.
///
/// The proc-macro server is that of the first workspace's toolchain.
pub fn load_workspaces_into_db(
    workspaces: Vec<ProjectWorkspace>,
    extra_env: &FxHashMap<String, Option<String>>,
    load_config: &LoadCargoConfig,
    db: &mut RootDatabase,
) -> anyhow::Result<(vfs::Vfs, Option<ProcMacroClient>)> {
    let mut vfs = vfs::Vfs::default();
    let mut loaded = Loaded::default();
    loaded.add(workspaces, extra_env, load_config, &mut vfs, db)?;

    if load_config.prefill_caches {
        let all = ide_db::base_db::all_crates(db);
        prime_caches::parallel_prime_caches(db, &all, load_config.num_worker_threads, &|_| ());
    }

    Ok((vfs, loaded.proc_macro_server.and_then(Result::ok)))
}

/// The workspaces of a database, to which more can be added while it is in use.
#[derive(Default)]
pub struct Loaded {
    workspaces: Vec<ProjectWorkspace>,
    /// The path prefixes of each source root and whether it is local. A source root keeps
    /// its place here, and so its id, when workspaces are added.
    file_sets: Vec<(Vec<VfsPath>, bool)>,
    /// The crates of each workspace. One that is the same in two of them is of both.
    crates: Vec<Vec<Crate>>,
    proc_macro_server: Option<Result<ProcMacroClient, ProcMacroLoadingError>>,
    /// The server of the libraries that list their macros ([`serve_listed_with`]), started
    /// when the first of them is loaded.
    listed_server: Option<Result<ProcMacroClient, ProcMacroLoadingError>>,
}

impl Loaded {
    pub fn workspaces(&self) -> &[ProjectWorkspace] {
        &self.workspaces
    }

    /// The crates of the workspace at `index` of [`Self::workspaces`].
    pub fn crates(&self, index: usize) -> &[Crate] {
        &self.crates[index]
    }

    /// Puts `workspace` where the one at `index` is. It is loaded by the next [`Self::add`].
    pub fn replace(&mut self, index: usize, workspace: ProjectWorkspace) {
        self.workspaces[index] = workspace;
    }

    /// Adds `workspaces` to those of `db`, whose files `vfs` holds. What the database has of
    /// the workspaces before stays as it is: their crates and source roots keep their ids.
    pub fn add(
        &mut self,
        workspaces: Vec<ProjectWorkspace>,
        extra_env: &FxHashMap<String, Option<String>>,
        load_config: &LoadCargoConfig,
        vfs: &mut vfs::Vfs,
        db: &mut RootDatabase,
    ) -> anyhow::Result<()> {
        self.workspaces.extend(workspaces);
        let known: FxHashSet<vfs::FileId> = vfs.iter().map(|(file, _)| file).collect();
        let ws = self.workspaces.first().ok_or_else(|| anyhow::anyhow!("no workspace to load"))?;
        let (sender, receiver) = unbounded();
        let mut loader = Box::new(vfs_notify::NotifyHandle::spawn(sender));

        // The graph of all of them: the database keeps the crates it has that are in it.
        let mut crate_graph = CrateGraphBuilder::default();
        let mut proc_macros = FxHashMap::default();
        let mut members = Vec::new();
        for ws in &self.workspaces {
            let (other, mut paths) = ws.to_crate_graph(
                &mut |path: &AbsPath| {
                    let path = vfs::VfsPath::from(path.to_path_buf());
                    if let Some((file_id, excluded)) = vfs.file_id(&path) {
                        return (excluded == vfs::FileExcluded::No).then_some(file_id);
                    }
                    let contents = loader.load_sync(path.as_path()?);
                    vfs.set_file_contents(path.clone(), contents);
                    vfs.file_id(&path).and_then(|(file_id, excluded)| {
                        (excluded == vfs::FileExcluded::No).then_some(file_id)
                    })
                },
                extra_env,
            );
            let ids = crate_graph.extend(other, &mut paths);
            members.push(ids.into_values().collect::<Vec<_>>());
            proc_macros.extend(paths);
        }
        let listed: FxHashSet<AbsPathBuf> = match listed::server() {
            Some(_) => proc_macros
                .values()
                .filter_map(|it| it.as_ref().ok())
                .map(|(_, path)| path.clone())
                .filter(|path| is_listed(path))
                .collect(),
            None => FxHashSet::default(),
        };
        // The toolchain's server is started for the libraries the other does not serve only,
        // so a load whose proc macros are all listed starts none of the toolchain's.
        let unlisted = proc_macros.values().any(|it| match it {
            Ok((_, path)) => !listed.contains(path),
            Err(_) => false,
        });
        if self.proc_macro_server.is_none() && unlisted {
            self.proc_macro_server = spawn_proc_macro_server(ws, extra_env, load_config);
        }
        if let Some(server) = listed::server().filter(|_| !listed.is_empty())
            && self.listed_server.is_none()
        {
            let spawned =
                ProcMacroClient::spawn(server, extra_env, None, load_config.proc_macro_processes);
            self.listed_server = Some(spawned.map_err(|e| {
                ProcMacroLoadingError::ProcMacroSrvError(e.to_string().into_boxed_str())
            }));
        }
        let proc_macros = {
            fn running(
                server: &Option<Result<ProcMacroClient, ProcMacroLoadingError>>,
            ) -> Result<&ProcMacroClient, ProcMacroLoadingError> {
                match server {
                    Some(Ok(it)) => Ok(it),
                    Some(Err(e)) => Err(e.clone()),
                    None => Err(ProcMacroLoadingError::ProcMacroSrvError(
                        "proc-macro-srv is not running, workspace is missing a sysroot".into(),
                    )),
                }
            }
            let (proc_macro_server, listed_server) =
                (running(&self.proc_macro_server), running(&self.listed_server));
            proc_macros
                .into_iter()
                .map(|(crate_id, path)| {
                    (
                        crate_id,
                        path.and_then(|(_, path)| {
                            let server = match listed.contains(&path) {
                                true => &listed_server,
                                false => &proc_macro_server,
                            };
                            server.as_ref().map_err(Clone::clone).and_then(|proc_macro_server| {
                                load_proc_macro(proc_macro_server, &path, &[])
                            })
                        }),
                    )
                })
                .collect()
        };

        let was = self.file_sets.len();
        let mut claimed: FxHashSet<VfsPath> =
            self.file_sets.iter().flat_map(|(paths, _)| paths.iter().cloned()).collect();
        let folders = ProjectFolders::new(&self.workspaces, &[], None);
        let mut load = Vec::new();
        for ((paths, is_local), entry) in folders.file_sets.into_iter().zip(folders.load) {
            if paths.iter().all(|it| claimed.contains(it)) {
                continue;
            }
            claimed.extend(paths.iter().cloned());
            self.file_sets.push((paths, is_local));
            load.push(entry);
        }
        loader.set_config(vfs::loader::Config { load, watch: vec![], version: 0 });
        // wait until Vfs has loaded all roots
        for task in &receiver {
            match task {
                vfs::loader::Message::Progress { n_done, .. } => {
                    if n_done == LoadingProgress::Finished {
                        break;
                    }
                }
                vfs::loader::Message::Loaded { files }
                | vfs::loader::Message::Changed { files } => {
                    for (path, contents) in files {
                        vfs.set_file_contents(path.into(), contents);
                    }
                }
            }
        }

        db.enable_proc_attr_macros();
        let mut fsc = FileSetConfig::builder();
        for (paths, _) in &self.file_sets {
            fsc.add_file_set(paths.clone());
        }
        let roots = fsc.build().partition(vfs);
        let (mut local_roots, mut library_roots) = (FxHashSet::default(), FxHashSet::default());
        for (idx, file_set) in roots.into_iter().enumerate() {
            // The last is of the files no root has, and is local.
            let is_local = self.file_sets.get(idx).is_none_or(|it| it.1);
            let root_id = SourceRootId(idx as u32);
            if is_local { &mut local_roots } else { &mut library_roots }.insert(root_id);
            // The roots there were have the files they had, and those of them that are new
            // to the database: what a workspace loaded again names that was not source
            // before. The one of the files no root had was where the first new one is now.
            if was != 0 && idx < was {
                join_root(db, root_id, file_set, &known);
                continue;
            }
            // The files of a root change far less often than what is in them.
            let durability = Durability::MEDIUM;
            let root = match is_local {
                true => SourceRoot::new_local(file_set),
                false => SourceRoot::new_library(file_set),
            };
            for file_id in root.iter() {
                db.set_file_source_root_with_durability(file_id, root_id, durability);
            }
            db.set_source_root_with_durability(root_id, triomphe::Arc::new(root), durability);
        }
        LocalRoots::get(db).set_roots(db).to(local_roots);
        LibraryRoots::get(db).set_roots(db).to(library_roots);

        let mut change = ChangeWithProcMacros::default();
        for (_, file) in vfs.take_changes() {
            if let vfs::Change::Create(v, _) | vfs::Change::Modify(v, _) = file.change
                && let Ok(text) = String::from_utf8(v)
            {
                let rust = vfs.file_path(file.file_id).name_and_extension();
                let rust = matches!(rust, Some((_, Some("rs"))));
                let text = if rust { source_text(text) } else { text };
                change.change_file(file.file_id, Some(text))
            }
        }
        change.set_crate_graph(crate_graph);
        change.set_proc_macros(proc_macros);
        db.trigger_cancellation();
        let ids = change.apply(db).expect("the crate graph is set");
        self.crates = members.into_iter().map(|it| it.iter().map(|id| ids[id]).collect()).collect();
        Ok(())
    }
}

/// Puts the files of `files` that are not among `known`, and so in no source root yet, in
/// the root `root_id`.
fn join_root(
    db: &mut RootDatabase,
    root_id: SourceRootId,
    files: vfs::file_set::FileSet,
    known: &FxHashSet<vfs::FileId>,
) {
    let was = db.source_root(root_id).source_root(db);
    let new: Vec<_> = files.iter().filter(|it| !known.contains(it)).collect();
    if new.is_empty() {
        return;
    }
    let mut joined = vfs::file_set::FileSet::default();
    let of_both = was.iter().map(|it| (it, was.path_for_file(&it)));
    let of_both = of_both.chain(new.iter().map(|&it| (it, files.path_for_file(&it))));
    for (file, path) in of_both {
        if let Some(path) = path {
            joined.insert(file, path.clone());
        }
    }
    let (root, durability) = match was.is_library {
        true => (SourceRoot::new_library(joined), Durability::MEDIUM),
        false => (SourceRoot::new_local(joined), Durability::LOW),
    };
    for file in new {
        db.set_file_source_root_with_durability(file, root_id, durability);
    }
    db.set_source_root_with_durability(root_id, triomphe::Arc::new(root), durability);
}

fn spawn_proc_macro_server(
    ws: &ProjectWorkspace,
    extra_env: &FxHashMap<String, Option<String>>,
    load_config: &LoadCargoConfig,
) -> Option<Result<ProcMacroClient, ProcMacroLoadingError>> {
    let spawn = |path: &AbsPath| {
        ProcMacroClient::spawn(
            path,
            extra_env,
            ws.toolchain.as_ref(),
            load_config.proc_macro_processes,
        )
        .map_err(|e| ProcMacroLoadingError::ProcMacroSrvError(e.to_string().into_boxed_str()))
    };
    match &load_config.with_proc_macro_server {
        ProcMacroServerChoice::Sysroot => ws.find_sysroot_proc_macro_srv().map(|it| {
            it.map_err(|e| ProcMacroLoadingError::ProcMacroSrvError(e.to_string().into_boxed_str()))
                .and_then(|it| spawn(&it))
        }),
        ProcMacroServerChoice::Explicit(path) => Some(spawn(path)),
        ProcMacroServerChoice::None => Some(Err(ProcMacroLoadingError::Disabled)),
    }
}

#[derive(Default)]
pub struct ProjectFolders {
    pub load: Vec<vfs::loader::Entry>,
    pub watch: Vec<usize>,
    /// The path prefixes of the file set of each entry of `load`, and whether it is local.
    pub file_sets: Vec<(Vec<VfsPath>, bool)>,
    pub source_root_config: SourceRootConfig,
}

impl ProjectFolders {
    pub fn new(
        workspaces: &[ProjectWorkspace],
        global_excludes: &[AbsPathBuf],
        user_config_dir_path: Option<&AbsPath>,
    ) -> ProjectFolders {
        let mut res = ProjectFolders::default();
        let mut fsc = FileSetConfig::builder();
        let mut local_filesets = vec![];

        // Dedup source roots
        // Depending on the project setup, we can have duplicated source roots, or for example in
        // the case of the rustc workspace, we can end up with two source roots that are almost the
        // same but not quite, like:
        // PackageRoot { is_local: false, include: [AbsPathBuf(".../rust/src/tools/miri/cargo-miri")], exclude: [] }
        // PackageRoot {
        //     is_local: true,
        //     include: [AbsPathBuf(".../rust/src/tools/miri/cargo-miri"), AbsPathBuf(".../rust/build/x86_64-pc-windows-msvc/stage0-tools/x86_64-pc-windows-msvc/release/build/cargo-miri-85801cd3d2d1dae4/out")],
        //     exclude: [AbsPathBuf(".../rust/src/tools/miri/cargo-miri/.git"), AbsPathBuf(".../rust/src/tools/miri/cargo-miri/target")]
        // }
        //
        // The first one comes from the explicit rustc workspace which points to the rustc workspace itself
        // The second comes from the rustc workspace that we load as the actual project workspace
        // These `is_local` differing in this kind of way gives us problems, especially when trying to filter diagnostics as we don't report diagnostics for external libraries.
        // So we need to deduplicate these, usually it would be enough to deduplicate by `include`, but as the rustc example shows here that doesn't work,
        // so we need to also coalesce the includes if they overlap.

        let mut roots: Vec<_> = workspaces
            .iter()
            .flat_map(|ws| ws.to_roots())
            .update(|root| root.include.sort())
            .sorted_by(|a, b| a.include.cmp(&b.include))
            .collect();

        // map that tracks indices of overlapping roots
        let mut overlap_map = FxHashMap::<_, Vec<_>>::default();
        let mut done = false;

        while !mem::replace(&mut done, true) {
            // maps include paths to indices of the corresponding root
            let mut include_to_idx = FxHashMap::default();
            // Find and note down the indices of overlapping roots
            for (idx, root) in roots.iter().enumerate().filter(|(_, it)| !it.include.is_empty()) {
                for include in &root.include {
                    match include_to_idx.entry(include) {
                        Entry::Occupied(e) => {
                            overlap_map.entry(*e.get()).or_default().push(idx);
                        }
                        Entry::Vacant(e) => {
                            e.insert(idx);
                        }
                    }
                }
            }
            for (k, v) in overlap_map.drain() {
                done = false;
                for v in v {
                    let r = mem::replace(
                        &mut roots[v],
                        PackageRoot { is_local: false, include: vec![], exclude: vec![] },
                    );
                    roots[k].is_local |= r.is_local;
                    roots[k].include.extend(r.include);
                    roots[k].exclude.extend(r.exclude);
                }
                roots[k].include.sort();
                roots[k].exclude.sort();
                roots[k].include.dedup();
                roots[k].exclude.dedup();
            }
        }

        // Collect workspace roots not already covered by a local PackageRoot
        // (e.g. virtual workspaces where no package lives at the workspace root).
        // We need these to load workspace-root rust-analyzer.toml into a local source root.
        let uncovered_ws_roots: Vec<AbsPathBuf> = workspaces
            .iter()
            .filter_map(|ws| {
                let ws_root = ws.workspace_root().to_path_buf();
                let dominated =
                    roots.iter().any(|root| root.is_local && root.include.contains(&ws_root));
                (!dominated).then_some(ws_root)
            })
            .collect();

        for root in roots.into_iter().filter(|it| !it.include.is_empty()) {
            let file_set_roots: Vec<VfsPath> =
                root.include.iter().cloned().map(VfsPath::from).collect();

            let entry = {
                let mut dirs = vfs::loader::Directories::default();
                dirs.extensions.push("rs".into());
                dirs.extensions.push("toml".into());
                dirs.extensions.push("md".into());
                dirs.include.extend(root.include);
                dirs.exclude.extend(root.exclude);
                for excl in global_excludes {
                    if dirs
                        .include
                        .iter()
                        .any(|incl| incl.starts_with(excl) || excl.starts_with(incl))
                    {
                        dirs.exclude.push(excl.clone());
                    }
                }

                vfs::loader::Entry::Directories(dirs)
            };

            if root.is_local {
                res.watch.push(res.load.len());
            }
            res.load.push(entry);

            if root.is_local {
                local_filesets.push(fsc.len() as u64);
            }
            {
                res.file_sets.push((file_set_roots.clone(), root.is_local));
                fsc.add_file_set(file_set_roots)
            }
        }

        for ws in workspaces.iter() {
            let mut file_set_roots: Vec<VfsPath> = vec![];
            let mut entries = vec![];

            for buildfile in ws.buildfiles() {
                file_set_roots.push(VfsPath::from(buildfile.to_owned()));
                entries.push(buildfile.to_owned());
            }

            if !file_set_roots.is_empty() {
                let entry = vfs::loader::Entry::Files(entries);
                res.watch.push(res.load.len());
                res.load.push(entry);
                local_filesets.push(fsc.len() as u64);
                {
                    res.file_sets.push((file_set_roots.clone(), true));
                    fsc.add_file_set(file_set_roots)
                }
            }
        }

        // For virtual workspaces, the workspace root has no local PackageRoot, so
        // rust-analyzer.toml there would fall into a library source root and be
        // ignored. Load it explicitly via Entry::Files and register the workspace
        // root as a local file-set root so the file is classified as local.
        for ws_root in &uncovered_ws_roots {
            let ratoml_path = ws_root.join("rust-analyzer.toml");
            let file_set_roots = vec![VfsPath::from(ws_root.clone())];
            let entry = vfs::loader::Entry::Files(vec![ratoml_path]);
            res.watch.push(res.load.len());
            res.load.push(entry);
            local_filesets.push(fsc.len() as u64);
            {
                res.file_sets.push((file_set_roots.clone(), true));
                fsc.add_file_set(file_set_roots)
            };
        }

        if let Some(user_config_path) = user_config_dir_path {
            let ratoml_path = {
                let mut p = user_config_path.to_path_buf();
                p.push("rust-analyzer.toml");
                p
            };

            let file_set_roots = vec![VfsPath::from(ratoml_path.to_owned())];
            let entry = vfs::loader::Entry::Files(vec![ratoml_path]);

            res.watch.push(res.load.len());
            res.load.push(entry);
            local_filesets.push(fsc.len() as u64);
            {
                res.file_sets.push((file_set_roots.clone(), true));
                fsc.add_file_set(file_set_roots)
            }
        }

        let fsc = fsc.build();
        res.source_root_config = SourceRootConfig { fsc, local_filesets };

        res
    }
}

#[derive(Default, Debug)]
pub struct SourceRootConfig {
    pub fsc: FileSetConfig,
    pub local_filesets: Vec<u64>,
}

impl SourceRootConfig {
    pub fn partition(&self, vfs: &vfs::Vfs) -> Vec<SourceRoot> {
        self.fsc
            .partition(vfs)
            .into_iter()
            .enumerate()
            .map(|(idx, file_set)| {
                let is_local = self.local_filesets.contains(&(idx as u64));
                if is_local {
                    SourceRoot::new_local(file_set)
                } else {
                    SourceRoot::new_library(file_set)
                }
            })
            .collect()
    }

    /// Returns whether `path` belongs to a library (non-local) source root, such as the
    /// sysroot sources or a cargo registry dependency.
    ///
    /// Paths that belong to no configured file set are *not* considered library files, as
    /// files outside of any loaded workspace (for example scratch files) fall into the
    /// catch-all file set despite being client-editable.
    pub fn path_is_library(&self, path: &VfsPath) -> bool {
        match self.fsc.classify_path(path) {
            Some(idx) => !self.local_filesets.contains(&(idx as u64)),
            None => false,
        }
    }

    /// Maps local source roots to their parent source roots by bytewise comparing of root paths .
    /// If a `SourceRoot` doesn't have a parent and is local then it is not contained in this mapping but it can be asserted that it is a root `SourceRoot`.
    pub fn source_root_parent_map(&self) -> FxHashMap<SourceRootId, SourceRootId> {
        let roots = self.fsc.roots();

        let mut map = FxHashMap::default();

        // See https://github.com/rust-lang/rust-analyzer/issues/17409
        //
        // We can view the connections between roots as a graph. The problem is
        // that this graph may contain cycles, so when adding edges, it is necessary
        // to check whether it will lead to a cycle.
        //
        // Since we ensure that each node has at most one outgoing edge (because
        // each SourceRoot can have only one parent), we can use a disjoint-set to
        // maintain the connectivity between nodes. If an edge’s two nodes belong
        // to the same set, they are already connected.
        let mut dsu = FxHashMap::default();
        fn find_parent(dsu: &mut FxHashMap<u64, u64>, id: u64) -> u64 {
            if let Some(&parent) = dsu.get(&id) {
                let parent = find_parent(dsu, parent);
                dsu.insert(id, parent);
                parent
            } else {
                id
            }
        }

        for (idx, (root, root_id)) in roots.iter().enumerate() {
            if !self.local_filesets.contains(root_id)
                || map.contains_key(&SourceRootId(*root_id as u32))
            {
                continue;
            }

            for (root2, root2_id) in roots[..idx].iter().rev() {
                if self.local_filesets.contains(root2_id)
                    && root_id != root2_id
                    && root.starts_with(root2)
                {
                    // check if the edge will create a cycle
                    if find_parent(&mut dsu, *root_id) != find_parent(&mut dsu, *root2_id) {
                        map.insert(SourceRootId(*root_id as u32), SourceRootId(*root2_id as u32));
                        dsu.insert(*root_id, *root2_id);
                    }

                    break;
                }
            }
        }

        map
    }
}

/// Load the proc-macros for the given lib path, disabling all expanders whose names are in `ignored_macros`.
pub fn load_proc_macro(
    server: &ProcMacroClient,
    path: &AbsPath,
    ignored_macros: &[Box<str>],
) -> ProcMacroLoadResult {
    let res: Result<Vec<_>, _> = (|| {
        let dylib = MacroDylib::new(path.to_path_buf());
        let vec = server.load_dylib(dylib).map_err(|e| {
            ProcMacroLoadingError::ProcMacroSrvError(format!("{e}").into_boxed_str())
        })?;
        if vec.is_empty() {
            return Err(ProcMacroLoadingError::NoProcMacros);
        }
        Ok(vec
            .into_iter()
            .map(|expander| expander_to_proc_macro(expander, path, ignored_macros))
            .collect())
    })();
    match res {
        Ok(proc_macros) => {
            tracing::info!(
                "Loaded proc-macros for {path}: {:?}",
                proc_macros.iter().map(|it| it.name.clone()).collect::<Vec<_>>()
            );
            Ok(proc_macros)
        }
        Err(e) => {
            tracing::warn!("proc-macro loading for {path} failed: {e}");
            Err(e)
        }
    }
}

fn expander_to_proc_macro(
    expander: proc_macro_api::ProcMacro,
    dylib: &AbsPath,
    ignored_macros: &[Box<str>],
) -> ProcMacro {
    let name = expander.name();
    let kind = match expander.kind() {
        proc_macro_api::ProcMacroKind::CustomDerive => ProcMacroKind::CustomDerive,
        proc_macro_api::ProcMacroKind::Bang => ProcMacroKind::Bang,
        proc_macro_api::ProcMacroKind::Attr => ProcMacroKind::Attr,
    };
    let disabled = ignored_macros.iter().any(|replace| **replace == *name);
    let name = intern::Symbol::intern(name);
    ProcMacro {
        expander: wrap::wrapped(sync::Arc::new(Expander(expander)), dylib, name.as_str(), kind),
        name,
        kind,
        disabled,
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Expander(proc_macro_api::ProcMacro);

impl ProcMacroExpander for Expander {
    fn expand(
        &self,
        db: &dyn SourceDatabase,
        subtree: &tt::TopSubtree,
        attrs: Option<&tt::TopSubtree>,
        env: &Env,
        def_site: Span,
        call_site: Span,
        mixed_site: Span,
        current_dir: String,
    ) -> Result<tt::TopSubtree, ProcMacroExpansionError> {
        let cb = |req| match req {
            SubRequest::LocalFilePath { file_id } => {
                let file_id = FileId::from_raw(file_id);
                let source_root_id = db.file_source_root(file_id).source_root_id(db);
                let source_root = db.source_root(source_root_id).source_root(db);
                let name = source_root
                    .path_for_file(&file_id)
                    .and_then(|path| path.as_path())
                    .map(|path| path.to_string());

                Ok(SubResponse::LocalFilePathResult { name })
            }
            // Not incremental: requires full file text.
            SubRequest::SourceText { file_id, ast_id, start, end } => {
                let range = resolve_sub_span(
                    db,
                    file_id,
                    ast_id,
                    TextRange::new(TextSize::from(start), TextSize::from(end)),
                );
                let source = db.file_text(range.file_id.file_id(db)).text(db);
                let text = source
                    .get(usize::from(range.range.start())..usize::from(range.range.end()))
                    .map(ToOwned::to_owned);

                Ok(SubResponse::SourceTextResult { text })
            }
            // Not incremental: requires building line index.
            SubRequest::LineColumn { file_id, ast_id, offset } => {
                let range =
                    resolve_sub_span(db, file_id, ast_id, TextRange::empty(TextSize::from(offset)));
                let (line, column) = db
                    .line_column(range.file_id.file_id(db), range.range.start())
                    .map(|(line, col)| (line + 1, col + 1))
                    .unwrap_or((1, 1));
                // proc_macro::Span line/column are 1-based
                Ok(SubResponse::LineColumnResult { line, column })
            }
            SubRequest::FilePath { file_id } => {
                let file_id = FileId::from_raw(file_id);
                let source_root_id = db.file_source_root(file_id).source_root_id(db);
                let source_root = db.source_root(source_root_id).source_root(db);
                let name = source_root
                    .path_for_file(&file_id)
                    .and_then(|path| path.as_path())
                    .map(|path| path.to_string())
                    .unwrap_or_default();

                Ok(SubResponse::FilePathResult { name })
            }
            // Not incremental: requires global span resolution.
            SubRequest::ByteRange { file_id, ast_id, start, end } => {
                let range = resolve_sub_span(
                    db,
                    file_id,
                    ast_id,
                    TextRange::new(TextSize::from(start), TextSize::from(end)),
                );

                Ok(SubResponse::ByteRangeResult { range: range.range.into() })
            }
            SubRequest::SpanSource { file_id, ast_id, start, end, ctx } => {
                let span = Span {
                    range: TextRange::new(TextSize::from(start), TextSize::from(end)),
                    anchor: SpanAnchor {
                        file_id: span::EditionedFileId::from_raw(file_id),
                        ast_id: span::ErasedFileAstId::from_raw(ast_id),
                    },
                    // SAFETY: We only receive spans from the server. If someone mess up the communication UB can happen,
                    // but that will be their problem.
                    ctx: unsafe { SyntaxContext::from_u32(ctx) },
                };

                let mut current_span = span;
                let mut current_ctx = span.ctx;

                while let Some(macro_call_id) = current_ctx.outer_expn(db) {
                    let macro_call_loc = hir_expand::MacroCallId::from(macro_call_id).loc(db);

                    let call_site_file = macro_call_loc.kind.file_id();

                    let resolved = hir_expand::resolve_span(db, current_span);

                    current_ctx = macro_call_loc.ctxt;
                    current_span = Span {
                        range: resolved.range,
                        anchor: SpanAnchor {
                            file_id: resolved.file_id.span_file_id(db),
                            ast_id: span::ROOT_ERASED_FILE_AST_ID,
                        },
                        ctx: current_ctx,
                    };

                    if call_site_file.file_id().is_some() {
                        break;
                    }
                }

                let resolved = hir_expand::resolve_span(db, current_span);

                Ok(SubResponse::SpanSourceResult {
                    file_id: resolved.file_id.span_file_id(db).as_u32(),
                    ast_id: span::ROOT_ERASED_FILE_AST_ID.into_raw(),
                    start: u32::from(resolved.range.start()),
                    end: u32::from(resolved.range.end()),
                    ctx: current_span.ctx.into_u32(),
                })
            }
            SubRequest::SpanParent { file_id, ast_id, start, end, ctx } => {
                let span = Span {
                    range: TextRange::new(TextSize::from(start), TextSize::from(end)),
                    anchor: SpanAnchor {
                        file_id: span::EditionedFileId::from_raw(file_id),
                        ast_id: span::ErasedFileAstId::from_raw(ast_id),
                    },
                    // SAFETY: We only receive spans from the server. If someone mess up the communication UB can happen,
                    // but that will be their problem.
                    ctx: unsafe { SyntaxContext::from_u32(ctx) },
                };

                if let Some(macro_call_id) = span.ctx.outer_expn(db) {
                    let macro_call_loc = hir_expand::MacroCallId::from(macro_call_id).loc(db);

                    let call_site_file = macro_call_loc.kind.file_id();
                    let call_site_ast_id = macro_call_loc.kind.erased_ast_id();

                    if let Some(editioned_file_id) = call_site_file.file_id() {
                        let range = hir_expand::HirFileId::from(editioned_file_id)
                            .ast_id_map(db)
                            .get_erased(call_site_ast_id)
                            .text_range();

                        let parent_span = Some(ParentSpan {
                            file_id: editioned_file_id.span_file_id(db).as_u32(),
                            ast_id: span::ROOT_ERASED_FILE_AST_ID.into_raw(),
                            start: u32::from(range.start()),
                            end: u32::from(range.end()),
                            ctx: macro_call_loc.ctxt.into_u32(),
                        });

                        return Ok(SubResponse::SpanParentResult { parent_span });
                    }
                }

                Ok(SubResponse::SpanParentResult { parent_span: None })
            }
            // FIXME: implement this
            SubRequest::SpanJoin { .. } => Ok(SubResponse::SpanJoinResult { span: None }),
        };
        match self.0.expand(
            subtree.view(),
            attrs.map(|attrs| attrs.view()),
            env.clone().into(),
            def_site,
            call_site,
            mixed_site,
            current_dir,
            Some(&|req| {
                wrap::ask();
                cb(req)
            }),
        ) {
            Ok(Ok(subtree)) => Ok(subtree),
            Ok(Err(err)) => Err(ProcMacroExpansionError::Panic(err)),
            Err(err) => Err(ProcMacroExpansionError::System(err.to_string())),
        }
    }

    fn eq_dyn(&self, other: &dyn ProcMacroExpander) -> bool {
        (other as &dyn Any).downcast_ref::<Self>() == Some(self)
    }
}

fn resolve_sub_span(
    db: &dyn SourceDatabase,
    file_id: u32,
    ast_id: u32,
    range: TextRange,
) -> hir_expand::FileRange {
    let ast_id = span::ErasedFileAstId::from_raw(ast_id);
    let editioned_file_id = span::EditionedFileId::from_raw(file_id);
    let span = Span {
        range,
        anchor: SpanAnchor { file_id: editioned_file_id, ast_id },
        ctx: SyntaxContext::root(editioned_file_id.edition()),
    };
    hir_expand::resolve_span(db, span)
}

/// The text of a Rust source file as rustc reads it: a line that ends in CR LF ends in LF.
/// Otherwise a string that runs over lines has other bytes than rustc gives it, and one
/// that leaves a line with a backslash does not lex.
pub fn source_text(text: String) -> String {
    if text.contains("\r\n") { text.replace("\r\n", "\n") } else { text }
}

#[cfg(test)]
mod tests {
    use ide_db::base_db::all_crates;
    use vfs::file_set::FileSetConfigBuilder;

    use super::*;

    #[test]
    fn test_loading_rust_analyzer() {
        let cargo_toml_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("Cargo.toml");
        let cargo_toml_path = AbsPathBuf::assert_utf8(cargo_toml_path);
        let manifest = ProjectManifest::from_manifest_file(cargo_toml_path).unwrap();

        let cargo_config = CargoConfig { set_test: true, ..CargoConfig::default() };
        let load_cargo_config = LoadCargoConfig {
            load_out_dirs_from_check: false,
            with_proc_macro_server: ProcMacroServerChoice::None,
            prefill_caches: false,
            num_worker_threads: 1,
            proc_macro_processes: 1,
        };
        let workspace = ProjectWorkspace::load(manifest, &cargo_config, &|_| {}).unwrap();
        let (db, _vfs, _proc_macro) =
            load_workspace(workspace, &cargo_config.extra_env, &load_cargo_config).unwrap();

        let n_crates = all_crates(&db).len();
        // RA has quite a few crates, but the exact count doesn't matter
        assert!(n_crates > 20);
    }

    #[test]
    fn unrelated_sources() {
        let mut builder = FileSetConfigBuilder::default();
        builder.add_file_set(vec![VfsPath::new_virtual_path("/ROOT/abc".to_owned())]);
        builder.add_file_set(vec![VfsPath::new_virtual_path("/ROOT/def".to_owned())]);
        let fsc = builder.build();
        let src = SourceRootConfig { fsc, local_filesets: vec![0, 1] };
        let vc = src.source_root_parent_map().into_iter().collect::<Vec<_>>();

        assert_eq!(vc, vec![])
    }

    #[test]
    fn unrelated_source_sharing_dirname() {
        let mut builder = FileSetConfigBuilder::default();
        builder.add_file_set(vec![VfsPath::new_virtual_path("/ROOT/abc".to_owned())]);
        builder.add_file_set(vec![VfsPath::new_virtual_path("/ROOT/def/abc".to_owned())]);
        let fsc = builder.build();
        let src = SourceRootConfig { fsc, local_filesets: vec![0, 1] };
        let vc = src.source_root_parent_map().into_iter().collect::<Vec<_>>();

        assert_eq!(vc, vec![])
    }

    #[test]
    fn basic_child_parent() {
        let mut builder = FileSetConfigBuilder::default();
        builder.add_file_set(vec![VfsPath::new_virtual_path("/ROOT/abc".to_owned())]);
        builder.add_file_set(vec![VfsPath::new_virtual_path("/ROOT/abc/def".to_owned())]);
        let fsc = builder.build();
        let src = SourceRootConfig { fsc, local_filesets: vec![0, 1] };
        let vc = src.source_root_parent_map().into_iter().collect::<Vec<_>>();

        assert_eq!(vc, vec![(SourceRootId(1), SourceRootId(0))])
    }

    #[test]
    fn basic_child_parent_with_unrelated_parents_sib() {
        let mut builder = FileSetConfigBuilder::default();
        builder.add_file_set(vec![VfsPath::new_virtual_path("/ROOT/abc".to_owned())]);
        builder.add_file_set(vec![VfsPath::new_virtual_path("/ROOT/def".to_owned())]);
        builder.add_file_set(vec![VfsPath::new_virtual_path("/ROOT/def/abc".to_owned())]);
        let fsc = builder.build();
        let src = SourceRootConfig { fsc, local_filesets: vec![0, 1, 2] };
        let vc = src.source_root_parent_map().into_iter().collect::<Vec<_>>();

        assert_eq!(vc, vec![(SourceRootId(2), SourceRootId(1))])
    }

    #[test]
    fn deep_sources_with_parent_missing() {
        let mut builder = FileSetConfigBuilder::default();
        builder.add_file_set(vec![VfsPath::new_virtual_path("/ROOT/abc".to_owned())]);
        builder.add_file_set(vec![VfsPath::new_virtual_path("/ROOT/ghi".to_owned())]);
        builder.add_file_set(vec![VfsPath::new_virtual_path("/ROOT/def/abc".to_owned())]);
        let fsc = builder.build();
        let src = SourceRootConfig { fsc, local_filesets: vec![0, 1, 2] };
        let vc = src.source_root_parent_map().into_iter().collect::<Vec<_>>();

        assert_eq!(vc, vec![])
    }

    #[test]
    fn ancestor_can_be_parent() {
        let mut builder = FileSetConfigBuilder::default();
        builder.add_file_set(vec![VfsPath::new_virtual_path("/ROOT/abc".to_owned())]);
        builder.add_file_set(vec![VfsPath::new_virtual_path("/ROOT/def".to_owned())]);
        builder.add_file_set(vec![VfsPath::new_virtual_path("/ROOT/def/ghi/jkl".to_owned())]);
        let fsc = builder.build();
        let src = SourceRootConfig { fsc, local_filesets: vec![0, 1, 2] };
        let vc = src.source_root_parent_map().into_iter().collect::<Vec<_>>();

        assert_eq!(vc, vec![(SourceRootId(2), SourceRootId(1))])
    }

    #[test]
    fn ancestor_can_be_parent_2() {
        let mut builder = FileSetConfigBuilder::default();
        builder.add_file_set(vec![VfsPath::new_virtual_path("/ROOT/abc".to_owned())]);
        builder.add_file_set(vec![VfsPath::new_virtual_path("/ROOT/def".to_owned())]);
        builder.add_file_set(vec![VfsPath::new_virtual_path("/ROOT/def/ghi/jkl".to_owned())]);
        builder.add_file_set(vec![VfsPath::new_virtual_path("/ROOT/def/ghi/klm".to_owned())]);
        let fsc = builder.build();
        let src = SourceRootConfig { fsc, local_filesets: vec![0, 1, 2, 3] };
        let mut vc = src.source_root_parent_map().into_iter().collect::<Vec<_>>();
        vc.sort_by_key(|x| x.0.0);

        assert_eq!(vc, vec![(SourceRootId(2), SourceRootId(1)), (SourceRootId(3), SourceRootId(1))])
    }

    #[test]
    fn non_locals_are_skipped() {
        let mut builder = FileSetConfigBuilder::default();
        builder.add_file_set(vec![VfsPath::new_virtual_path("/ROOT/abc".to_owned())]);
        builder.add_file_set(vec![VfsPath::new_virtual_path("/ROOT/def".to_owned())]);
        builder.add_file_set(vec![VfsPath::new_virtual_path("/ROOT/def/ghi/jkl".to_owned())]);
        builder.add_file_set(vec![VfsPath::new_virtual_path("/ROOT/def/klm".to_owned())]);
        let fsc = builder.build();
        let src = SourceRootConfig { fsc, local_filesets: vec![0, 1, 3] };
        let mut vc = src.source_root_parent_map().into_iter().collect::<Vec<_>>();
        vc.sort_by_key(|x| x.0.0);

        assert_eq!(vc, vec![(SourceRootId(3), SourceRootId(1)),])
    }

    #[test]
    fn child_binds_ancestor_if_parent_nonlocal() {
        let mut builder = FileSetConfigBuilder::default();
        builder.add_file_set(vec![VfsPath::new_virtual_path("/ROOT/abc".to_owned())]);
        builder.add_file_set(vec![VfsPath::new_virtual_path("/ROOT/def".to_owned())]);
        builder.add_file_set(vec![VfsPath::new_virtual_path("/ROOT/def/klm".to_owned())]);
        builder.add_file_set(vec![VfsPath::new_virtual_path("/ROOT/def/klm/jkl".to_owned())]);
        let fsc = builder.build();
        let src = SourceRootConfig { fsc, local_filesets: vec![0, 1, 3] };
        let mut vc = src.source_root_parent_map().into_iter().collect::<Vec<_>>();
        vc.sort_by_key(|x| x.0.0);

        assert_eq!(vc, vec![(SourceRootId(3), SourceRootId(1)),])
    }

    #[test]
    fn parents_with_identical_root_id() {
        let mut builder = FileSetConfigBuilder::default();
        builder.add_file_set(vec![
            VfsPath::new_virtual_path("/ROOT/def".to_owned()),
            VfsPath::new_virtual_path("/ROOT/def/abc/def".to_owned()),
        ]);
        builder.add_file_set(vec![VfsPath::new_virtual_path("/ROOT/def/abc/def/ghi".to_owned())]);
        let fsc = builder.build();
        let src = SourceRootConfig { fsc, local_filesets: vec![0, 1] };
        let mut vc = src.source_root_parent_map().into_iter().collect::<Vec<_>>();
        vc.sort_by_key(|x| x.0.0);

        assert_eq!(vc, vec![(SourceRootId(1), SourceRootId(0)),])
    }

    #[test]
    fn circular_reference() {
        let mut builder = FileSetConfigBuilder::default();
        builder.add_file_set(vec![
            VfsPath::new_virtual_path("/ROOT/def".to_owned()),
            VfsPath::new_virtual_path("/ROOT/def/abc/def".to_owned()),
        ]);
        builder.add_file_set(vec![VfsPath::new_virtual_path("/ROOT/def/abc".to_owned())]);
        let fsc = builder.build();
        let src = SourceRootConfig { fsc, local_filesets: vec![0, 1] };
        let mut vc = src.source_root_parent_map().into_iter().collect::<Vec<_>>();
        vc.sort_by_key(|x| x.0.0);

        assert_eq!(vc, vec![(SourceRootId(1), SourceRootId(0)),])
    }
}
