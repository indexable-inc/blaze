//! A library's doctests in the front end (docs/v2/plan/design/doctests.md): each example's program,
//! and the runner that runs them, is a crate of its own, made from the library's `Doctest`
//! unit by [`Lowered::doctests`] on the loop's writer. An example's crate is what rustdoc
//! compiles it as: a binary named `rust_out`, of its own edition, with the library's
//! dependencies (the library built as it is, its dev-dependencies, the standard library),
//! cfg and environment, but not `cfg(doc)` or `cfg(doctest)`, which only the library's
//! crate read for the examples has. A `test_harness` example's is a test, as rustdoc's
//! `--test` makes it: `cfg(test)`, and libtest's harness appended to its root.
//!
//! An example's file is named by the file its documentation is in (`src/lib.rs`), so its
//! errors and panics point there, the program's lines being the documentation's
//! (`blaze_doc::doctest::Program::source`). In the database its path is that path with the
//! example's id after it, in a source root of the example's own: one path is one file to
//! base-db, the library's file keeps its path, and what an example's crate is made of
//! (its identity, its code identity) holds no other example's file, as rustdoc compiles each
//! example alone.

use std::alloc::Global;

use base_db::{
    AbsPathBuf, CrateDisplayName, CrateKey, CrateOrigin, Edit, ExtraCrateData, FileId, FileSet,
    SourceRoot, SourceRootId, TargetKind, VfsPath, WorldCrateData,
};
use cfg::{CfgAtom, CfgDiff};
use intern::Symbol;
use span::Edition;
use triomphe::Arc;

use super::{Appended, LowerError, Lowered, RootOf, Trigger};

/// One program of a library's doctests: an example's, or the runner's.
pub struct DoctestProgram<'a> {
    /// rustdoc's id of the example (`src_lib_rs_12_0`), or the runner's own.
    pub id: &'a str,
    /// The absolute path its file is named by: the file the example is in.
    pub named: &'a str,
    pub source: &'a str,
    pub edition: Edition,
    /// Whether it is built as a test, as rustdoc builds a `test_harness` example with
    /// `--test`: a test target's crate with `cfg(test)`, whose `main` is its harness's.
    pub test: bool,
    /// The module the compiler appends to its root, after `source` (a test's harness, which
    /// the caller lists from the crate as made): the compiler's own code, as a test crate's
    /// appended harness is ([`Lowered::append_roots`]). A caller that has not listed it yet
    /// gives the one the example's file was last written with ([`Lowered::doctest_harnesses`]),
    /// so a harness that lists the same tests is written once.
    pub appended: Option<&'a Appended>,
}

/// The crates [`Lowered::doctests`] made for one `Doctest` unit, kept so a later call writes
/// the same files and crates again rather than making new ones.
pub(crate) struct OfUnit {
    /// The unit of the plan the front end is at; `usize::MAX` while that plan has no
    /// `Doctest` unit of their crate ([`Lowered::relower`]).
    pub(crate) unit: usize,
    made: Vec<Made>,
}

struct Made {
    id: String,
    file: FileId,
    /// The source root of its file alone.
    root: SourceRootId,
    krate: CrateKey,
    /// What its root's text was written with after the program's source.
    appended: Option<Appended>,
}

impl Lowered {
    /// The crate of each of `programs`, made for `Doctest` unit `unit`, in their order: an
    /// example's (or the runner's) of the same id as an earlier call's is the same crate and
    /// file, written again where it changed; one no longer given leaves the crate graph. The
    /// crates depend on what the unit's crate depends on and have its cfg but `doc` and
    /// `doctest`, and its environment. An error names the unit, or the example and why its
    /// file can't be made.
    ///
    /// The crates are given by key: the commit that makes them is this write's, and each is
    /// looked up after it ([`base_db::Db::krate`]).
    ///
    /// A write, as [`Lowered::ran`] is: one batch.
    pub fn doctests(
        &mut self,
        unit: usize,
        programs: &[DoctestProgram<'_>],
    ) -> Result<Vec<CrateKey>, LowerError> {
        let Some(lowering) = self.crates.units.get(unit) else {
            // alloc: an error
            return Err(LowerError::new(format!(
                "the build has no unit {unit} to make doctests of"
            )));
        };
        // clone: once per doctest unit written, what its trigger names
        let what = lowering.what.clone();
        let trigger = Trigger::of("a library's doctests made into crates", &what);
        self.batch(trigger, |this| this.doctest_writes(unit, programs))
    }

    /// The harness each example of `Doctest` unit `unit` was last written with
    /// ([`DoctestProgram::appended`]), by its id, copied out for the caller to give back while
    /// it writes: none before its first [`Lowered::doctests`].
    pub fn doctest_harnesses(&self, unit: usize) -> Vec<(String, Appended)> {
        let made =
            self.doctests.iter().find(|it| it.unit == unit).map_or(&[][..], |it| &it.made[..]);
        // alloc: once per doctest unit made, its test examples' ids and harnesses
        made.iter().filter_map(|it| Some((it.id.clone(), it.appended.clone()?))).collect()
    }

    fn doctest_writes(
        &mut self,
        unit: usize,
        programs: &[DoctestProgram<'_>],
    ) -> Result<Vec<CrateKey>, LowerError> {
        let lowering = &self.crates.units[unit];
        let Some(library) = lowering.krate else {
            return Err(LowerError::at(
                "a unit with no crate has no doctests",
                lowering.what.clone(),
            ));
        };
        // clone: once per doctest unit written, what its examples' crates are made of
        let what = lowering.what.clone();
        let mut edit = self.db.edit();
        // The library as the world has it: a run applied to it in this change is in it.
        let written = edit.written_crate(library);
        let base = written.data.clone();
        let workspace = written.workspace.clone();
        let env = written.env.clone();
        let mut cfg = written.cfg.clone();
        let not = |name: &str| CfgAtom::Flag(Symbol::intern(name));
        cfg.apply_diff(CfgDiff::new(Vec::new(), vec![not("doc"), not("doctest")]));
        let extra = ExtraCrateData {
            version: written.extra.version.clone(),
            display_name: Some(CrateDisplayName::from_canonical_name("rust_out")),
            potential_cfg_options: None,
            built_for: written.extra.built_for.clone(),
            build_script_runs: written.extra.build_script_runs.clone(), // clone: the example reads the library's runs
        };

        let at = match self.doctests.iter().position(|it| it.unit == unit) {
            Some(at) => at,
            None => {
                // alloc: once per doctest unit
                self.doctests.push(OfUnit { unit, made: Vec::new() });
                self.doctests.len() - 1
            }
        };
        // Each example's source root is its own: the front end's next ([`Graph::root`]), so a
        // package a later plan names takes none of them.
        let mut was = std::mem::take(&mut self.doctests[at].made);
        // alloc: once per doctest unit written, its programs' crates
        let mut made = Vec::with_capacity(programs.len());
        for program in programs {
            // alloc: a program's path, once per program written
            let path = format!("{}.{}.doctest.rs", program.named, program.id);
            let path = AbsPathBuf::try_from(path.as_str()).map_err(|_| {
                LowerError::at(
                    format!("the file of doctest `{}` is no absolute path", program.id),
                    path.clone(),
                )
            })?;
            let reused =
                was.iter().position(|it| it.id == program.id).map(|at| was.swap_remove(at));
            let (file, root) = match &reused {
                Some(it) => (it.file, it.root),
                None => {
                    let file = edit.new_file_id().ok_or_else(|| {
                        // alloc: an error
                        LowerError::at(
                            format!("base-db has no file ID left for doctest `{}`", program.id),
                            what.clone(),
                        )
                    })?;
                    (file, SourceRootId(self.graph.root(RootOf::Example) as u32))
                }
            };
            match program.appended {
                Some(appended) => {
                    // alloc: once per test program written, its root's text with its harness
                    let text = format!("{}{}", program.source, appended.after);
                    edit.set_file_text(file, &text).set_appended_root_module(file, Some(appended.module));
                }
                None => {
                    edit.set_file_text(file, program.source).set_appended_root_module(file, None);
                }
            }
            edit.set_file_source_root(file, root);
            // The origin names the example crate (a registry library's examples are `Library`,
            // any other's `Local`); its arena is never sealed, whatever the origin: its file and
            // source root are rewritten whenever the examples are.
            let origin = match &base.origin {
                CrateOrigin::Library { repo, .. } => {
                    // alloc: the crate's origin, once per program made
                    CrateOrigin::Library {
                        repo: repo.clone(),
                        name: Symbol::intern(&format!("{what} doctest {}", program.id)),
                    }
                }
                // alloc: as above
                _ => CrateOrigin::Local {
                    repo: None,
                    name: Some(Symbol::intern(&format!("{what} doctest {}", program.id))),
                },
            };
            // A test's crate is a test target's, whose `main` is its harness's, as rustc's
            // `--test` makes it; any other program's is a binary's.
            let kind = match program.test {
                true => TargetKind::Test,
                false => TargetKind::Bin,
            };
            let data = WorldCrateData {
                root_file_id: file,
                edition: program.edition,
                dependencies: base.dependencies.clone(),
                origin,
                crate_attrs: Box::default(),
                target_kind: kind,
                manifest_kind: kind,
                crate_types: kind.default_crate_types(),
                proc_macro_cwd: base.proc_macro_cwd.clone(),
                // An example crate is never sealed, whatever its library's pin: its root
                // file is in a local root, written again by each build.
                sealed: None,
            };
            // A test example's has `test` too.
            let cfg = match program.test {
                true => {
                    let mut cfg = cfg.clone(); // clone: once per program made, its crate's
                    let test = vec![CfgAtom::Flag(Symbol::intern("test"))]; // alloc: once per test program made
                    cfg.apply_diff(CfgDiff::new(test, Vec::new()));
                    cfg
                }
                false => cfg.clone(), // clone: once per program made, its crate's
            };
            // Its source root first, which a crate's root must be in when the crate is made: a
            // kept example's is set again only when it no longer holds its file alone. The
            // file's path is under its package's directory, and a view of the package's files
            // (set on an edit) takes out of its source root every file there it lacks.
            let path = VfsPath::from(path);
            let holds = reused.is_some()
                && edit.written_source_root(root).is_some_and(|set| {
                    set.file_for_path(&path) == Some(&file) && set.iter().count() == 1
                });
            if !holds {
                let mut files = FileSet::default();
                files.insert(file, path);
                edit.set_source_root(root, Arc::new(SourceRoot::new_local(files)));
            }
            made.push((program, file, root, reused.map(|it| it.krate), data, cfg));
        }
        // An example no longer given leaves its source root empty.
        for gone in &was {
            edit.set_source_root(gone.root, Arc::new(SourceRoot::new_local(FileSet::default())));
        }
        let mut kept = Vec::with_capacity(made.len());
        for (program, file, root, krate, data, cfg) in made {
            let krate = match krate {
                // Each setter compares with what the crate has, and writes only what moved.
                // clone: the library's variables, once per program made
                Some(krate) => {
                    edit.set_crate_data(krate, data)
                        .set_crate_cfg_options(krate, cfg)
                        .set_crate_env(krate, env.clone());
                    krate
                }
                // Never sealed (no pin in its data): rewritten by each build of the examples.
                // clone: as `data`
                None => edit.create_crate(data, extra.clone(), workspace.clone(), cfg, env.clone()),
            };
            // alloc: a program's id, once per program made, and a test's harness (cloned)
            kept.push(Made {
                id: program.id.to_owned(),
                file,
                root,
                krate,
                appended: program.appended.cloned(),
            });
        }
        let paths = std::sync::Arc::make_mut(&mut self.paths);
        for gone in &was {
            paths.remove(&gone.file);
        }
        for (program, it) in programs.iter().zip(&kept) {
            paths.insert(it.file, program.named.to_owned());
        }
        let crates: Vec<CrateKey> = kept.iter().map(|it| it.krate).collect();
        set_crate_graph(&mut edit, &was, &crates);
        // An example sees `CARGO_PRIMARY_PACKAGE` as its library does (`base_db::is_primary`):
        // its variables are the library's, and that one is of the request
        // ([`base_db::PRIMARY_PACKAGE`]).
        let primary = edit.written_primary_crates();
        if !base.origin.is_lang() && primary.binary_search(&library).is_ok() {
            // alloc: once per doctest unit written of a primary package, the primary crates
            let mut primary_now = Vec::with_capacity_in(primary.len() + crates.len(), Global);
            primary_now.extend(primary.iter().copied().chain(crates.iter().copied()));
            edit.set_primary_crates(primary_now);
        }
        _ = edit.commit();
        self.doctests[at].made = kept;
        self.identities.clear();
        Ok(crates)
    }
}

/// The crate graph, as `edit` has it, with `gone` out of it and each of `crates` in it, after
/// the crates it depends on.
fn set_crate_graph(edit: &mut Edit<'_>, gone: &[Made], crates: &[CrateKey]) {
    let all = edit.written_all_crates();
    let stays = |krate: &CrateKey| !gone.iter().any(|it| it.krate == *krate);
    // alloc: once per doctest unit written, its crates not listed yet
    let new: Vec<CrateKey> = crates.iter().copied().filter(|it| !all.contains(it)).collect();
    if gone.iter().all(|it| !all.contains(&it.krate)) && new.is_empty() {
        return;
    }
    // alloc: once per doctest unit whose examples moved, the list
    let mut listed = Vec::with_capacity_in(all.len() + new.len(), Global);
    listed.extend(all.iter().copied().filter(stays).chain(new));
    edit.set_all_crates(&listed);
}
