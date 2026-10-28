//! `blaze-engine --check <project> …`: the daemon's `blaze check` of a build it has no
//! programs of. It loads the workspace as the worker of that build does and builds nothing:
//! each request on its socket is answered with what rust-analyzer finds in the crates of the
//! workspace, as a build says its errors. No rustc checks the source.

pub(crate) use blaze_cargo::check::{LINT, RELOAD};

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

use anyhow::Context as _;
use ide_db::{
    ChangeWithProcMacros, RootDatabase,
    base_db::{SourceDatabase as _, TargetKind},
};
use intern::sym;
use project_model::ProjectWorkspaceKind;
use rustc_hash::{FxHashMap, FxHashSet};
use vfs::Vfs;

use crate::{Args, Parts, Sources, Watched, WatchedFile};

/// Loads the workspace of `args` and answers the check requests on its socket.
pub(crate) fn serve(args: Args) -> anyhow::Result<()> {
    let socket = args.socket.clone().context("--check needs --socket")?;
    crate::events::said_beside(&socket);
    crate::events::at(crate::events::Step::LoadWorkspace, None);
    crate::process_wide();
    let read_from = SystemTime::now();
    let cargo_config = crate::cargo_config(&args);
    let projects = [args.project.as_path()];
    let selection = args.selection.clone();
    let (mut db, vfs, checkouts) =
        crate::load(&projects, &cargo_config, crate::load_config(), selection, None, None)?;
    ide_db::base_db::set_process_env(&mut db, crate::process::own());
    crate::prepare_macro_sessions(&db);
    let Parts { root, members, lints_capped, .. } = checkouts.parts(&args.project)?;
    let mut checker = Checker::new(&db, &vfs, &checkouts, &members, read_from, root, lints_capped);
    crate::events::idle();
    let served = match crate::socket::Served::bind(&socket) {
        Ok(served) => served,
        Err(crate::socket::NotServed::Taken) => {
            anyhow::bail!("another checker serves {}", socket.display())
        }
        Err(crate::socket::NotServed::Failed(error)) => {
            anyhow::bail!("listening on {}: {error}", socket.display())
        }
    };
    eprintln!("checking {} crates", checker.crates);
    for (stream, _open) in served.incoming() {
        let Ok(request) = crate::socket::read_request(&stream) else {
            continue;
        };
        let reply = match checker.refresh(&mut db) {
            Some(reload) => {
                eprintln!("{RELOAD}: {reload}");
                RELOAD.to_owned()
            }
            None => {
                crate::events::at(
                    crate::events::Step::Check,
                    Some(&format!("{} crates", checker.crates)),
                );
                let answer = checker.answer(&db, &args, &request);
                crate::events::idle();
                answer
            }
        };
        let _ = std::io::Write::write_all(&mut &stream, format!("{reply}\n").as_bytes());
        if reply == RELOAD {
            break;
        }
    }
    Ok(())
}

/// The verdict of a `blaze check` on the crates of `project`, `krate` among them, once,
/// as a build script's probe of rustc is answered (`as_rustc.rs`): `None` when they compile,
/// else their errors.
pub(crate) fn once(project: &Path, krate: &str) -> anyhow::Result<Option<crate::Refused>> {
    let project = project.to_str().context("a path that is not text")?.to_owned();
    // A build's arguments, of which a check reads the project and the crate.
    let out = std::env::temp_dir().join("unwritten.o").to_string_lossy().into_owned();
    let args = [project, "--crate".to_owned(), krate.to_owned(), "--out".to_owned(), out];
    let args = Args::parse(args.into_iter())?;
    crate::process_wide();
    let read_from = SystemTime::now();
    let cargo_config = crate::cargo_config(&args);
    let projects = [args.project.as_path()];
    let (mut db, vfs, checkouts) = crate::load(
        &projects,
        &cargo_config,
        crate::load_config(),
        args.selection.clone(),
        None,
        None,
    )?;
    ide_db::base_db::set_process_env(&mut db, crate::process::own());
    crate::prepare_macro_sessions(&db);
    // The names of the standard library are what a build resolved and the store kept: a
    // probe's crate is a few lines, and std's names are most of what resolving it costs.
    if let Some(toolchain) = crate::toolchain_identity(&vfs, &args.symbol_files) {
        blaze_store::compiles_against(&toolchain);
    }
    let crates = crate::built_crates(&db, &args).unwrap_or_default();
    let served = crate::frozen::serve(&mut db, &crates);
    if std::env::var_os("BLAZE_STATS").is_some() {
        eprintln!("probe: def maps of {} crates from the store", served.def_maps());
    }
    let Parts { root, members, lints_capped, .. } = checkouts.parts(&args.project)?;
    let checker = Checker::new(&db, &vfs, &checkouts, &members, read_from, root, lints_capped);
    // A probe's `rust-project.json` has no cargo targets to pick from: its crates are checked.
    let scope = checker.scope(None)?;
    if let Some(refused) = checker.refused(&db, &scope) {
        return Ok(Some(refused));
    }
    Ok(scope.checked.iter().find_map(|&file| unresolved(&db, file)).map(|path| {
        crate::Refused::Error(crate::errors::Said::one(crate::errors::Diagnostic {
            code: Some("E0433".to_owned()),
            message: format!("failed to resolve `{path}`"),
            span: None,
        }))
    }))
}

/// A path of the file, outside its attributes, that names nothing: rustc's E0433, which
/// rust-analyzer does not say of a type's path, and on which a probe of a library item
/// turns (autocfg's `probe_trait("alloc::alloc::Allocator")` with no `extern crate alloc`).
fn unresolved(db: &RootDatabase, file: vfs::FileId) -> Option<String> {
    use syntax::{AstNode as _, ast};
    let sema = hir::Semantics::new(db);
    let tree = sema.parse_guess_edition(file);
    tree.syntax().descendants().filter_map(ast::Path::cast).find_map(|path| {
        let in_attr = path.syntax().ancestors().any(|it| ast::Attr::can_cast(it.kind()));
        (!in_attr && sema.resolve_path(&path).is_none()).then(|| path.to_string())
    })
}

/// A target of the workspace a check diagnoses: its root file, its kind and, of a library or
/// a binary, whether it is checked as its unit tests, which are a crate of their own.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Picked {
    root: PathBuf,
    kind: TargetKind,
    test: bool,
}

impl Picked {
    fn of(root: PathBuf, kind: TargetKind, test: bool) -> Picked {
        // A library and a binary have their unit tests as a crate of their own (cargo's
        // `checked_as_test`); every other target is one crate, with `cfg(test)` or not as the
        // load has it.
        let test = test && matches!(kind, TargetKind::Lib { is_proc_macro: false } | TargetKind::Bin);
        Picked { root, kind, test }
    }
}

/// The targets of the workspace of `project` that `cargo check` of `selection` with the
/// target flags `targets` diagnoses: the units of its unit graph, the targets picked and what
/// of the workspace they depend on, as cargo checks them.
fn picked(
    selection: &crate::plan::Selection,
    targets: &[String],
    project: &Path,
) -> anyhow::Result<FxHashSet<Picked>> {
    let manifest = match project.is_file() {
        true => project.to_owned(),
        false => project.join("Cargo.toml"),
    };
    let (cwd, manifest) = crate::plan::asked_where(&manifest, selection);
    let graph = crate::cargo::metadata::checked(selection, targets, cwd, manifest)?;
    let units = graph.units.iter().filter_map(|unit| {
        let kind = match unit.target.kind.first()?.as_str() {
            "lib" | "rlib" | "dylib" | "cdylib" | "staticlib" => {
                TargetKind::Lib { is_proc_macro: false }
            }
            "proc-macro" => TargetKind::Lib { is_proc_macro: true },
            "bin" => TargetKind::Bin,
            "example" => TargetKind::Example,
            "test" => TargetKind::Test,
            "bench" => TargetKind::Bench,
            "custom-build" => TargetKind::BuildScript,
            _ => return None,
        };
        Some(Picked::of(unit.target.src_path.clone(), kind, unit.checked_as_test))
    });
    Ok(units.collect())
}

/// A crate of the workspace, as a check picks it or not.
struct Local {
    krate: ide_db::base_db::Crate,
    /// Which target it is, when its root file has a path.
    target: Option<Picked>,
    /// Whether it is a build script or a proc macro, which run as they were built.
    runs: bool,
    script: bool,
    /// The files of its modules.
    files: Vec<vfs::FileId>,
}

/// What one check diagnoses: the crates of the targets it picks.
struct Scope {
    /// Their files.
    checked: Vec<vfs::FileId>,
    /// The crates: a file of a library is also one of its unit tests, and a file can be a
    /// module of a target the check does not pick.
    within: Arc<FxHashSet<ide_db::base_db::Crate>>,
    /// The crates `blaze clippy` lints: proc macros included, build scripts not.
    linted: Vec<ide_db::base_db::Crate>,
}

struct Checker {
    /// The source files of the workspace as they were last read.
    files: Vec<WatchedFile>,
    /// The crates of the workspace.
    locals: Vec<Local>,
    /// The root files of the targets of the workspace's packages.
    local_roots: FxHashSet<PathBuf>,
    /// What each set of target flags a check was asked with picks, by the flags.
    scopes: FxHashMap<String, Scope>,
    crates: usize,
    /// The files of the build scripts and proc macros of the workspace.
    ran: FxHashSet<vfs::FileId>,
    /// Where new source files of the crates checked would be.
    dirs: Vec<PathBuf>,
    /// Every file loaded, by path.
    loaded: FxHashSet<PathBuf>,
    /// The manifests, the lock file and cargo's configuration, as they were when loaded.
    manifests: FxHashMap<PathBuf, Option<SystemTime>>,
    sources: Sources,
    root: PathBuf,
    lints_capped: bool,
    /// Where each file loaded is, for naming them in lints.
    located: FxHashMap<vfs::FileId, PathBuf>,
}

impl Checker {
    fn new(
        db: &RootDatabase,
        vfs: &Vfs,
        checkouts: &crate::Checkouts,
        members: &FxHashSet<ide_db::base_db::Crate>,
        read_from: SystemTime,
        root: PathBuf,
        lints_capped: bool,
    ) -> Checker {
        let files = WatchedFile::sources(vfs, db, read_from);
        let paths: FxHashMap<_, _> = files
            .iter()
            .filter_map(|it| match it.file {
                Watched::Source(id) => Some((id, it.path.clone())),
                Watched::Included(_) => None,
            })
            .collect();
        let mut ran_roots = FxHashSet::default();
        let mut script_roots = FxHashSet::default();
        let mut local_roots = FxHashSet::default();
        let mut manifests = FxHashMap::default();
        for workspace in checkouts.loaded.workspaces() {
            let ProjectWorkspaceKind::Cargo { cargo, .. } = &workspace.kind else { continue };
            let root: &Path = cargo.workspace_root().as_ref();
            for file in ["Cargo.lock", ".cargo/config.toml", ".cargo/config"] {
                manifests.insert(root.join(file), None);
            }
            for package in cargo.packages().filter(|it| cargo[*it].is_local) {
                manifests.insert(cargo[package].manifest.to_path_buf().into(), None);
                for &target in &cargo[package].targets {
                    let target = &cargo[target];
                    local_roots.insert(PathBuf::from(target.root.clone()));
                    let runs = matches!(
                        target.kind,
                        TargetKind::BuildScript | TargetKind::Lib { is_proc_macro: true }
                    );
                    if runs {
                        ran_roots.insert(PathBuf::from(target.root.clone()));
                    }
                    if target.kind == TargetKind::BuildScript {
                        script_roots.insert(PathBuf::from(target.root.clone()));
                    }
                }
            }
        }
        for (path, when) in &mut manifests {
            *when = modified(path);
        }
        let path_of = |file: vfs::FileId| -> Option<PathBuf> {
            Some(vfs.file_path(file).as_path()?.to_path_buf().into())
        };
        let (mut locals, mut ran, mut dirs, mut crates) =
            (Vec::new(), FxHashSet::default(), Vec::new(), 0);
        hir::attach_db(db, || {
            for &member in members {
                let krate = hir::Crate::from(member);
                if !krate.origin(db).is_local() {
                    continue;
                }
                let root_file = krate.root_file(db);
                let runs = path_of(root_file).is_some_and(|it| ran_roots.contains(&it));
                let script = path_of(root_file).is_some_and(|it| script_roots.contains(&it));
                // A test and a binary can share a root file, and a library's unit tests are
                // a crate of its own: the kind and `cfg(test)` say which target it is.
                let target = path_of(root_file).map(|root| {
                    let kind = krate.target_kind(db);
                    let test = member.cfg_options(db).check_atom(&cfg::CfgAtom::Flag(sym::test));
                    Picked::of(root, kind, test)
                });
                let mut of_it: Vec<vfs::FileId> = krate
                    .modules(db)
                    .into_iter()
                    .map(|it| it.definition_source_file_id(db).original_file(db).file_id(db))
                    .collect();
                of_it.sort_unstable();
                of_it.dedup();
                match runs {
                    true => ran.extend(of_it.iter().copied()),
                    false => {
                        crates += 1;
                        if let Some(dir) = path_of(root_file).as_deref().and_then(Path::parent) {
                            dirs.push(dir.to_owned());
                        }
                    }
                }
                locals.push(Local { krate: member, target, runs, script, files: of_it });
            }
        });
        dirs.sort();
        dirs.dedup();
        // A directory below another is walked with it.
        let outer: Vec<PathBuf> = dirs
            .iter()
            .filter(|dir| !dirs.iter().any(|it| it != *dir && dir.starts_with(it)))
            .cloned()
            .collect();
        let loaded =
            vfs.iter().filter_map(|(_, path)| Some(path.as_path()?.to_path_buf().into())).collect();
        let located = vfs
            .iter()
            .filter_map(|(id, path)| Some((id, path.as_path()?.to_path_buf().into())))
            .collect();
        Checker {
            files,
            locals,
            local_roots,
            scopes: FxHashMap::default(),
            crates,
            ran,
            dirs: outer,
            loaded,
            manifests,
            sources: Sources { paths },
            root,
            lints_capped,
            located,
        }
    }

    /// What a check of `targets` diagnoses; of every crate of the workspace for none. A target
    /// picked that the workspace was not loaded with is an error.
    fn scope(&self, targets: Option<&FxHashSet<Picked>>) -> anyhow::Result<Scope> {
        let (mut checked, mut within, mut linted) = (Vec::new(), FxHashSet::default(), Vec::new());
        let mut found = FxHashSet::default();
        for local in &self.locals {
            let picked = match targets {
                Some(targets) => local.target.as_ref().is_some_and(|it| targets.contains(it)),
                None => true,
            };
            if !picked {
                continue;
            }
            found.extend(local.target.clone());
            if !local.script {
                linted.push(local.krate);
            }
            if !local.runs {
                checked.extend(local.files.iter().copied());
                within.insert(local.krate);
            }
        }
        // A package that is no member has no build script crate.
        let mut missing = targets
            .into_iter()
            .flatten()
            .filter(|it| it.kind != TargetKind::BuildScript)
            .filter(|it| self.local_roots.contains(&it.root) && !found.contains(*it));
        if let Some(missing) = missing.next() {
            anyhow::bail!(
                "the check picks the {:?} target of {}{}, which the workspace was not loaded with",
                missing.kind,
                missing.root.display(),
                if missing.test { " as a test" } else { "" },
            );
        }
        checked.sort_unstable();
        checked.dedup();
        Ok(Scope { checked, within: Arc::new(within), linted })
    }

    /// The answer to `request`, of [`request`]'s form, once the source is taken in: `ok`, the
    /// errors, the lints, or why there is no answer.
    fn answer(&mut self, db: &RootDatabase, args: &Args, request: &str) -> String {
        let Some((targets, request)) = request.split_once('\t') else {
            return format!("a check request names no targets: {request}");
        };
        if !self.scopes.contains_key(targets) {
            let flags: Vec<String> = match serde_json::from_str(targets) {
                Ok(flags) => flags,
                Err(error) => {
                    return format!("the targets of a check request do not read: {error}");
                }
            };
            let scope = picked(&args.selection, &flags, &args.project)
                .and_then(|picked| self.scope(Some(&picked)));
            match scope {
                Ok(scope) => self.scopes.insert(targets.to_owned(), scope),
                Err(error) => return format!("{error:#}"),
            };
        }
        let scope = &self.scopes[targets];
        match (self.verdict(db, scope), request.strip_prefix(LINT)) {
            (verdict, Some(ask)) if verdict == "ok" => self.lints(db, scope, ask),
            (verdict, _) => verdict,
        }
    }

    /// Takes in the source as it is now, or says why only loading it again does.
    fn refresh(&mut self, db: &mut RootDatabase) -> Option<String> {
        for (path, when) in &self.manifests {
            if modified(path) != *when {
                return Some(format!("{} changed", path.display()));
            }
        }
        if let Some(new) = self.dirs.iter().find_map(|dir| new_source(dir, &self.loaded)) {
            return Some(format!("{} is new", new.display()));
        }
        let mut change = ChangeWithProcMacros::default();
        for watched in &mut self.files {
            let Watched::Source(id) = watched.file else { continue };
            let Some(now) = modified(&watched.path) else {
                return Some(format!("{} is gone", watched.path.display()));
            };
            if now == watched.mtime {
                continue;
            }
            watched.mtime = now;
            let Ok(text) = crate::source_on_disk(&watched.path) else {
                return Some(format!("{} can't be read", watched.path.display()));
            };
            if *text == **db.file_text(id).text(db) {
                continue;
            }
            if self.ran.contains(&id) {
                return Some(format!("{} runs as it was built", watched.path.display()));
            }
            change.change_file(id, Some(text));
        }
        db.apply_change(change);
        crate::prepare_macro_sessions(db);
        None
    }

    /// `ok`, or the errors in the source of `scope` as rustc writes them.
    fn verdict(&self, db: &RootDatabase, scope: &Scope) -> String {
        match self.refused(db, scope) {
            Some(refused) => refused.reply(),
            None => "ok".to_owned(),
        }
    }

    /// The errors in the source of `scope`, `None` when it compiles.
    fn refused(&self, db: &RootDatabase, scope: &Scope) -> Option<crate::Refused> {
        let broken = crate::broken_in(
            db,
            &scope.checked,
            &self.sources,
            &self.root,
            Some(scope.within.clone()),
            !self.lints_capped,
        );
        hir::attach_db(db, || {
            crate::refusal(db, &scope.checked, &self.sources, &self.root, broken, None)
        })
    }

    /// The answer to the lint request `ask`: what blaze's clippy finds in the crates of
    /// `scope`.
    fn lints(&self, db: &RootDatabase, scope: &Scope, ask: &str) -> String {
        let ask: blaze_lints::serve::Ask = match serde_json::from_str(ask) {
            Ok(ask) => ask,
            Err(error) => return format!("the lint request does not read: {error}"),
        };
        let path_of = |file: vfs::FileId| self.located.get(&file).cloned();
        let answer = blaze_lints::serve::answer(db, &scope.linted, &self.root, &path_of, &ask);
        format!("{LINT}{}", serde_json::to_string(&answer).unwrap_or_default())
    }
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|it| it.modified()).ok()
}

/// A Rust source file below `dir` that was not loaded. What a build made is not looked in.
fn new_source(dir: &Path, loaded: &FxHashSet<PathBuf>) -> Option<PathBuf> {
    let mut dirs = vec![dir.to_owned()];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => {
                    if !name.starts_with('.') && name != "target" {
                        dirs.push(path);
                    }
                }
                Ok(_) if name.ends_with(".rs") && !loaded.contains(&path) => return Some(path),
                _ => {}
            }
        }
    }
    None
}
