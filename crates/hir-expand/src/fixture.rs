//! Test databases from fixture text: a fixture ([`FixtureWithProjectMeta`]) read into a [`Db`]
//! through the inputs a build sets. Each source root's files are one view
//! ([`Db::set_view_root`]), the crate graph is set by [`CrateGraphBuilder::set_in_db`], and the
//! test proc macros by [`ProcMacrosBuilder`]. [`db`] makes one; the [`Fixture`] it returns
//! owns the database and answers the files, the `$0` position or range and the test crate.
//!
//! # Fixture syntax
//!
//! A fixture without metadata is one file, `/main.rs`. Metadata follows a `//-` prefix:
//!
//! ```text
//! //- /main.rs crate:main deps:helper
//! use helper::greet;
//! fn main() { greet(); }
//!
//! //- /lib.rs crate:helper
//! pub fn greet() {}
//! ```
//!
//! Each file's metadata, after its path (which starts with `/`):
//!
//! - **`crate:<name>`**: a crate with this file for its root; `crate:foo@0.1.0,https://repo`
//!   gives a version and repository, `crate:bin:foo` a target kind (`lib`, `proc-macro`, `bin`,
//!   `example`, `test`, `bench`, `custom-build`, `other`), and `crate:r#core` a crate named
//!   like a language crate that is not one.
//! - **`deps:<a>,<b>`**: its dependencies (with `crate:`).
//! - **`extern-prelude:<a>,<b>`**: the dependencies in its extern prelude; all by default.
//! - **`edition:<year>`**: its edition; the current one by default.
//! - **`cfg:<key>=<value>,<flag>`**, **`env:<KEY>=<value>`**, **`crate-attr:<attr>`**.
//! - **`new_source_root:local|library`**: this file and those after it are a new source root.
//! - **`library`**: the crate is not a workspace member.
//!
//! The fixture's own metadata, at its top in this order: `//- toolchain: nightly|stable`,
//! `//- target_data_layout: <layout>`, `//- target_arch: <arch>`,
//! `//- proc_macros: <name>,<name>` (the test proc macros below, as crate `proc_macros`, a
//! dependency of every crate) and `//- minicore: <flag>, <flag>` (a `core` of the parts of
//! `test-utils`' minicore named, a dependency of every crate).
//!
//! One `$0` marks a position and two a range ([`Fixture::position`], [`Fixture::range`]);
//! `\$0` is a literal `$0`.
//!
//! The test proc macros: `identity`, `DeriveIdentity`, `input_replace`, `mirror`, `shorten`,
//! `issue_18089`, `issue_18840`, `issue_17479`, `issue_18898`, `disallow_cfg` and
//! `generate_suffixed_type`.
//!
//! Each file is at its fixture path under `/`, the one directory the fixture's views are of;
//! minicore is `/sysroot/core/lib.rs` and the proc macros' crate `/sysroot/proc_macros/lib.rs`.
//! A fixture's files have the file IDs from 0 in its order, then minicore's, then the proc
//! macros'. An edit after setup is [`Db::set_file_text`].

use std::{any::TypeId, str::FromStr, sync};

use base_db::{
    AbsPathBuf, ConstEvalBudget, ContentId, Crate, CrateDisplayName, CrateGraphBuilder, CrateName,
    CrateOrigin, CrateWorkspaceData, Db, DependencyBuilder, Env, FileId, FileSet, FxIndexMap,
    LangCrateOrigin, LibraryRoots, LocalRoots, SourceRoot, SourceRootId, TargetKind, Version,
    VfsPath, ViewFile, ViewRoot, all_crates,
    target::{Arch, TargetData},
};
use blaze_hash::HashSet;
use blaze_query::db::{Durability, Setter};
use cfg::CfgOptions;
use intern::{Symbol, sym};
use itertools::Itertools;
use span::Edition;
use test_utils::{
    CURSOR_MARKER, ESCAPED_CURSOR_MARKER, Fixture as FixtureFile, FixtureWithProjectMeta, MiniCore,
    RangeOrOffset, extract_range_or_offset,
};
use triomphe::Arc;
use tt::{Leaf, TokenTree, TopSubtree, TopSubtreeBuilder, TtElement, TtIter};

use crate::{
    EditionedFileId, FileRange,
    builtin::quote::quote,
    files::FilePosition,
    proc_macro::{
        ProcMacro, ProcMacroCall, ProcMacroExpander, ProcMacroExpansionError,
        ProcMacroKind, ProcMacros, ProcMacrosBuilder,
    },
};

#[cfg(test)]
mod tests;

/// The directory every fixture's files are under, as their views' base.
const BASE: &str = "/";

const CORE_PATH: &str = "/sysroot/core/lib.rs";
const PROC_MACROS_PATH: &str = "/sysroot/proc_macros/lib.rs";

/// The env variable every fixture crate has.
const FIXTURE_ENV: &str = "__ra_is_test_fixture";

/// A database made from a fixture, with the files it has and where its `$0` is.
pub struct Fixture {
    db: Db,
    /// The fixture's own files, in its order.
    files: Vec<EditionedFileId>,
    paths: Vec<String>,
    /// The line of the fixture each file's text starts at.
    lines: Vec<usize>,
    /// Minicore's file and the proc macros', those it has.
    sysroot_files: Vec<FileId>,
    marked: Option<(EditionedFileId, RangeOrOffset)>,
}

/// The database of `fixture`, with every test proc macro its `//- proc_macros:` names.
#[track_caller]
pub fn db(#[rust_analyzer::rust_fixture] fixture: &str) -> Fixture {
    db_with_proc_macros(fixture, Vec::new())
}

/// [`db`] with `extra` beside the test proc macros, each with the source of its declaration
/// in the proc macros' crate: `//- proc_macros:` names each one it has, by its name in snake
/// case.
#[track_caller]
pub fn db_with_proc_macros(
    #[rust_analyzer::rust_fixture] fixture: &str,
    extra: Vec<(String, ProcMacro)>,
) -> Fixture {
    build(fixture, extra)
}

/// The crate a fixture is about: the first crate of the graph that is neither a language crate
/// nor a dependency of another (`ra_test_fixture` for a fixture that names no crate). The graph
/// lists a crate's dependencies before it, so the first that is not a language crate would be
/// a dependency: `proc_macros`, or `foo` of `crate:main deps:foo`.
pub fn test_crate(db: &Db) -> Crate {
    let all = all_crates(db);
    let depended_on: HashSet<Crate> = all
        .iter()
        .flat_map(|krate| krate.data(db).dependencies.iter().map(|dep| dep.crate_id))
        .collect();
    all.iter()
        .copied()
        .find(|&krate| !krate.data(db).origin.is_lang() && !depended_on.contains(&krate))
        .expect("the fixture has a crate that is neither a language crate nor a dependency")
}

impl Fixture {
    pub fn db(&self) -> &Db {
        &self.db
    }

    pub fn db_mut(&mut self) -> &mut Db {
        &mut self.db
    }

    pub fn into_db(self) -> Db {
        self.db
    }

    /// The fixture's own files, in its order (not minicore's or the proc macros').
    pub fn files(&self) -> &[EditionedFileId] {
        &self.files
    }

    /// The fixture's `at`th file.
    #[track_caller]
    pub fn file(&self, at: usize) -> EditionedFileId {
        match self.files.get(at) {
            Some(&file) => file,
            None => panic!("the fixture has {} files, not {}", self.files.len(), at + 1),
        }
    }

    /// The fixture's one file.
    #[track_caller]
    pub fn only_file(&self) -> EditionedFileId {
        assert_eq!(self.files.len(), 1, "the fixture has {} files, not one", self.files.len());
        self.files[0]
    }

    /// The file at `path` (`/main.rs`).
    #[track_caller]
    pub fn file_at(&self, path: &str) -> EditionedFileId {
        match self.paths.iter().position(|it| it == path) {
            Some(at) => self.files[at],
            None => panic!("no file `{path}` in the fixture: {:?}", self.paths),
        }
    }

    /// The path of each of [`Fixture::files`].
    pub fn paths(&self) -> &[String] {
        &self.paths
    }

    /// The line of the fixture each of [`Fixture::files`] starts at.
    pub fn lines(&self) -> &[usize] {
        &self.lines
    }

    /// The files of minicore and the proc macros' crate, those the fixture has.
    pub fn sysroot_files(&self) -> &[FileId] {
        &self.sysroot_files
    }

    /// The file and position or range `$0` marks, if it marks one.
    pub fn marked(&self) -> Option<(EditionedFileId, RangeOrOffset)> {
        self.marked
    }

    #[track_caller]
    pub fn range_or_offset(&self) -> (EditionedFileId, RangeOrOffset) {
        self.marked.expect("Could not find file position in fixture. Did you forget to add an `$0`?")
    }

    /// The position one `$0` marks.
    #[track_caller]
    pub fn position(&self) -> FilePosition {
        let (file_id, marked) = self.range_or_offset();
        FilePosition { file_id, offset: marked.expect_offset() }
    }

    /// The range two `$0` mark.
    #[track_caller]
    pub fn range(&self) -> FileRange {
        let (file_id, marked) = self.range_or_offset();
        FileRange { file_id, range: marked.expect_range() }
    }

    /// [`test_crate`].
    #[track_caller]
    pub fn krate(&self) -> Crate {
        test_crate(&self.db)
    }

    /// Gives `krate` the proc macros `macros` in place of those it had, as loading its dylib
    /// would: a fixture's own `crate:proc-macro:name` has none until it is given some.
    pub fn set_proc_macros(&mut self, krate: Crate, macros: Vec<ProcMacro>) {
        ProcMacros::give(&mut self.db, [(krate, Ok(macros))]);
    }
}

/// One file to set, in the order its ID is taken.
struct Planned {
    /// Absolute, starting with `/`.
    path: String,
    text: String,
    /// Where its source root is in `roots`.
    root: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceRootKind {
    Local,
    Library,
}

/// A crate the fixture declares, by the index of its root in `Planned`.
struct PlannedCrate {
    file: usize,
    name: CrateName,
    meta: FileMeta,
    krate: CrateMeta,
}

#[track_caller]
fn build(fixture: &str, mut extra: Vec<(String, ProcMacro)>) -> Fixture {
    let FixtureWithProjectMeta {
        fixture,
        mini_core,
        proc_macro_names,
        toolchain,
        target_data_layout,
        target_arch,
    } = FixtureWithProjectMeta::parse(fixture);
    let target = Ok(TargetData {
        tuple: Symbol::intern("x86_64-unknown-linux-gnu"),
        arch: parse_target_arch(&target_arch),
        data_layout: target_data_layout.into(),
        has_thumb_interworking: false,
    });
    let toolchain = Some({
        let channel = toolchain.as_deref().unwrap_or("stable");
        Version::parse(&format!("1.76.0-{channel}")).unwrap()
    });
    let workspace = Arc::new(CrateWorkspaceData {
        target,
        toolchain,
        const_eval: ConstEvalBudget { steps: 10_000_000, ..ConstEvalBudget::default() },
    });
    let base = AbsPathBuf::try_from(BASE).expect("`/` is an absolute path");
    let proc_macro_cwd = Arc::new(base.clone());

    // The fixture's source roots, in the order of their files: each `new_source_root` starts one.
    let mut fixture_roots = vec![SourceRootKind::Local];
    let mut planned: Vec<Planned> = Vec::with_capacity(fixture.len() + 2);
    let mut lines = Vec::with_capacity(fixture.len());
    let mut editions = Vec::with_capacity(fixture.len());
    let mut crates: Vec<PlannedCrate> = Vec::new();
    let mut default_root: Option<(usize, FileMeta)> = None;
    let mut marked = None;
    for entry in fixture {
        lines.push(entry.line);
        let mut range_or_offset = None;
        let text = if entry.text.contains(CURSOR_MARKER) {
            if entry.text.contains(ESCAPED_CURSOR_MARKER) {
                entry.text.replace(ESCAPED_CURSOR_MARKER, CURSOR_MARKER)
            } else {
                let (marks, text) = extract_range_or_offset(&entry.text);
                assert!(marked.is_none(), "the fixture marks two files with `$0`");
                range_or_offset = Some(marks);
                text
            }
        } else {
            entry.text.clone()
        };

        let current = *fixture_roots.last().expect("a fixture has a first source root");
        let (meta, krate) = FileMeta::from_fixture(entry, current);
        let at = planned.len();
        if let Some(marks) = range_or_offset {
            marked = Some((at, meta.edition, marks));
        }
        assert!(meta.path.starts_with(BASE), "a fixture's path starts with `/`: {}", meta.path);
        if !meta.deps.is_empty() {
            assert!(krate.is_some(), "can't specify deps without naming the crate");
        }
        if let Some(kind) = meta.introduce_new_source_root {
            assert!(krate.is_some(), "new_source_root meta doesn't make sense without crate meta");
            fixture_roots.push(kind);
        }
        assert!(
            !planned.iter().any(|it| it.path == meta.path),
            "the fixture has `{}` twice",
            meta.path
        );
        planned.push(Planned {
            path: meta.path.clone(),
            text,
            root: fixture_roots.len() - 1,
        });
        editions.push(meta.edition);
        match krate {
            Some(krate) => crates.push(PlannedCrate {
                file: at,
                name: CrateName::normalize_dashes(&krate.name),
                meta,
                krate,
            }),
            None if meta.path == "/main.rs" || meta.path == "/lib.rs" => {
                assert!(default_root.is_none(), "the fixture has both `/main.rs` and `/lib.rs`");
                default_root = Some((at, meta));
            }
            None => {}
        }
    }
    let own_files = planned.len();

    // `roots[i]` is the source root of the files `Planned::root` puts in `i`: the fixture's,
    // then minicore's, then the proc macros'. Their IDs are as rust-analyzer's fixture numbered
    // them: each fixture root but the last, then minicore's, then the proc macros', then the
    // fixture's last.
    let last_fixture_root = fixture_roots.len() - 1;
    let sysroot_roots = usize::from(mini_core.is_some()) + usize::from(!proc_macro_names.is_empty());
    let mut roots: Vec<(SourceRootId, SourceRootKind)> = fixture_roots
        .iter()
        .enumerate()
        .map(|(at, &kind)| {
            let id = if at == last_fixture_root { at + sysroot_roots } else { at };
            (SourceRootId(id as u32), kind)
        })
        .collect();
    let has_core = mini_core.is_some();
    if let Some(mini_core) = mini_core {
        roots.push((SourceRootId(last_fixture_root as u32), SourceRootKind::Library));
        planned.push(Planned {
            path: CORE_PATH.to_owned(),
            text: mini_core.source_code(MiniCore::RAW_SOURCE),
            root: roots.len() - 1,
        });
    }
    let has_proc_macros = !proc_macro_names.is_empty();
    let mut proc_macros = Vec::new();
    if has_proc_macros {
        extra.extend(default_test_proc_macros());
        let (macros, source) = filter_test_proc_macros(&proc_macro_names, extra);
        proc_macros = macros;
        let id = last_fixture_root + usize::from(has_core);
        roots.push((SourceRootId(id as u32), SourceRootKind::Library));
        planned.push(Planned {
            path: PROC_MACROS_PATH.to_owned(),
            text: source,
            root: roots.len() - 1,
        });
    }

    let mut db = Db::for_tests();
    let ids = set_files(&mut db, &base, &planned, &roots);
    let sysroot_files = ids[own_files..].to_vec();
    let core_file = has_core.then(|| ids[own_files]);
    let proc_macros_file = has_proc_macros.then(|| ids[planned.len() - 1]);

    let mut graph = CrateGraphBuilder::default();
    let fixture_env = || Env::from_iter([(FIXTURE_ENV.to_owned(), FIXTURE_ENV.to_owned())]);
    let mut by_name = FxIndexMap::default();
    let mut deps = Vec::new();
    for PlannedCrate { file, name, mut meta, krate } in crates {
        if let Some(attrs) = sysroot_attr(&meta, &planned[file].text, has_core) {
            meta.crate_attrs.extend(attrs.iter().map(|&it| it.to_owned()));
        }
        let id = graph.add_crate_root(
            ids[file],
            meta.edition,
            Some(name.clone().into()),
            krate.version,
            meta.cfg.clone(),
            Some(meta.cfg),
            meta.env,
            krate.origin,
            meta.crate_attrs,
            krate.target_kind,
            proc_macro_cwd.clone(),
            workspace.clone(),
        );
        let prev = by_name.insert(name.clone(), id);
        assert!(prev.is_none(), "multiple crates with same name: {name}");
        for dep in meta.deps {
            let prelude = meta.extern_prelude.as_ref().is_none_or(|it| it.contains(&dep));
            deps.push((name.clone(), CrateName::normalize_dashes(&dep), prelude));
        }
    }

    let core = core_file.map(|file| {
        graph.add_crate_root(
            file,
            Edition::CURRENT,
            Some(CrateDisplayName::from_canonical_name("core")),
            None,
            Default::default(),
            Default::default(),
            fixture_env(),
            CrateOrigin::Lang(LangCrateOrigin::Core),
            // alloc: one crate attribute, once per fixture
            vec!["no_core".to_owned()],
            TargetKind::Lib { is_proc_macro: false },
            proc_macro_cwd.clone(),
            workspace.clone(),
        )
    });
    let core_dep = |core| {
        DependencyBuilder::with_prelude(CrateName::new("core").unwrap(), core, true, true)
    };

    if by_name.is_empty() {
        let (file, meta) =
            default_root.expect("missing default crate root, specify a main.rs or lib.rs");
        let mut env = fixture_env();
        env.extend_from_other(&meta.env);
        let attrs = sysroot_attr(&meta, &planned[file].text, has_core).unwrap_or_default();
        let root = graph.add_crate_root(
            ids[file],
            meta.edition,
            Some(CrateName::new("ra_test_fixture").unwrap().into()),
            None,
            meta.cfg.clone(),
            Some(meta.cfg),
            env,
            CrateOrigin::Local { repo: None, name: None },
            attrs.iter().map(|&it| it.to_owned()).collect(),
            TargetKind::Lib { is_proc_macro: false },
            proc_macro_cwd.clone(),
            workspace.clone(),
        );
        if let Some(core) = core {
            graph.add_dep(root, core_dep(core)).unwrap();
        }
    } else {
        // Minicore first, as `project-model` adds the sysroot's crates.
        if let Some(core) = core {
            let all: Vec<_> = graph.iter().filter(|&it| it != core).collect();
            for krate in all {
                graph.add_dep(krate, core_dep(core)).unwrap();
            }
        }
        for (from, to, prelude) in deps {
            let from_id = by_name[&from];
            // `deps:core` beside `//- minicore:` names minicore's, a dependency of every crate.
            if core.is_some() && to.as_str() == "core" && !by_name.contains_key(&to) {
                continue;
            }
            let Some(&to_id) = by_name.get(&to) else {
                panic!("crate `{from}` depends on `{to}`, which the fixture has no crate of")
            };
            let sysroot = graph[to_id].basic.origin.is_lang();
            graph
                .add_dep(from_id, DependencyBuilder::with_prelude(to, to_id, prelude, sysroot))
                .unwrap();
        }
    }

    let mut macros = ProcMacrosBuilder::default();
    if let Some(file) = proc_macros_file {
        let all: Vec<_> = graph.iter().collect();
        let proc_macros_crate = graph.add_crate_root(
            file,
            Edition::CURRENT,
            Some(CrateDisplayName::from_canonical_name("proc_macros")),
            None,
            Default::default(),
            Default::default(),
            fixture_env(),
            CrateOrigin::Local { repo: None, name: None },
            // alloc: one crate attribute, once per fixture
            vec!["no_core".to_owned()],
            TargetKind::Lib { is_proc_macro: true },
            proc_macro_cwd,
            workspace,
        );
        macros.insert(proc_macros_crate, Ok(proc_macros));
        for krate in all {
            graph
                .add_dep(
                    krate,
                    DependencyBuilder::new(CrateName::new("proc_macros").unwrap(), proc_macros_crate),
                )
                .unwrap();
        }
    }

    // Setting the crate graph maintains every crate's def map, which reads the proc macros: one
    // write sets both, so the maintainers run once and see the fixture's.
    let crates_id_map = db.batch(|db| {
        let crates_id_map = graph.set_in_db(db);
        macros.build_in(db, &crates_id_map);
        crates_id_map
    });

    let files = ids[..own_files]
        .iter()
        .zip(&editions)
        .map(|(&file, &edition)| EditionedFileId::new(&db, file, edition))
        .collect();
    let marked = marked.map(|(at, edition, marks)| (EditionedFileId::new(&db, ids[at], edition), marks));
    let paths = planned.into_iter().take(own_files).map(|it| it.path).collect();
    Fixture { db, files, paths, lines, sysroot_files, marked }
}

/// Makes the source roots, empty, then sets each one's files (those `Planned::root` puts in
/// it) as a view of `base`, in `planned`'s order so that their file IDs are taken in it.
/// Returns each planned file's ID.
fn set_files(
    db: &mut Db,
    base: &AbsPathBuf,
    planned: &[Planned],
    roots: &[(SourceRootId, SourceRootKind)],
) -> Vec<FileId> {
    let mut by_id = roots.to_vec();
    by_id.sort_unstable_by_key(|(id, _)| id.0);
    let mut local = HashSet::default();
    let mut library = HashSet::default();
    for (id, kind) in by_id {
        let root = match kind {
            SourceRootKind::Local => {
                local.insert(id);
                SourceRoot::new_local(FileSet::default())
            }
            SourceRootKind::Library => {
                library.insert(id);
                SourceRoot::new_library(FileSet::default())
            }
        };
        db.set_source_root_with_durability(id, Arc::new(root), Durability::MEDIUM);
    }
    LocalRoots::get(&*db).set_roots(db).to(local);
    LibraryRoots::get(&*db).set_roots(db).to(library);

    let bytes: Vec<blaze_vfs::Bytes> = planned
        .iter()
        .map(|it| blaze_vfs::Bytes::from(sync::Arc::<[u8]>::from(it.text.as_bytes())))
        .collect();
    let abs: Vec<AbsPathBuf> = planned.iter().map(|it| base.join(&it.path[BASE.len()..])).collect();
    // Every root's view is of the one base, so each leaves the others' files as they are.
    let partition = Default::default();
    for (root, &(id, _)) in roots.iter().enumerate() {
        let files: Vec<ViewFile<'_>> = planned
            .iter()
            .zip(&bytes)
            .filter(|(it, _)| it.root == root)
            .map(|(it, bytes)| ViewFile {
                path: &it.path[BASE.len()..],
                id: ContentId::of(blaze_vfs::FILE_DOMAIN, bytes),
                bytes,
            })
            .collect();
        if files.is_empty() {
            continue;
        }
        let built: Vec<AbsPathBuf> = planned
            .iter()
            .zip(&abs)
            .filter(|(it, _)| it.root != root)
            .map(|(_, path)| path.clone())
            .collect();
        let view = ViewRoot {
            base,
            built: &built,
            partition: &partition,
            other: id,
            files: &files,
        };
        let change = db.set_view_root(view).unwrap_or_else(|error| panic!("the fixture's files: {error}"));
        assert!(change.removed.is_empty(), "a fixture's view removed {:?}", change.removed);
        assert!(change.no_text.is_empty(), "a fixture's file has no text: {:?}", change.no_text);
    }

    planned
        .iter()
        .zip(abs)
        .map(|(it, path)| {
            let root = db.source_root(roots[it.root].0).source_root_ref(&*db);
            match root.file_for_path(&VfsPath::from(path)) {
                Some(&file) => file,
                None => panic!("the fixture's `{}` is in no source root", it.path),
            }
        })
        .collect()
}

/// The crate attributes a fixture crate with no `std` dependency takes, so that rustc's injected
/// `extern crate std;` is not asked for: `no_std` beside a `core` (minicore's or a `core` it
/// depends on), whose prelude it gets, and `no_core` with none, behind its feature gate as
/// `-Zcrate-attr` UI tests write it (rustc refuses an ungated `no_core`, E0658). None when it
/// depends on `std` or its crate attributes or root already say either.
fn sysroot_attr(meta: &FileMeta, root_text: &str, has_core: bool) -> Option<&'static [&'static str]> {
    let says = |attr: &str| {
        meta.crate_attrs.iter().any(|it| it == attr)
            || root_text.lines().any(|line| {
                line.trim().strip_prefix("#![").and_then(|it| it.strip_suffix(']')) == Some(attr)
            })
    };
    if meta.deps.iter().any(|it| it == "std") || says("no_std") || says("no_core") {
        return None;
    }
    match has_core || meta.deps.iter().any(|it| it == "core") {
        true => Some(&["no_std"]),
        false => Some(&["feature(no_core)", "no_core"]),
    }
}

fn parse_target_arch(arch: &str) -> Arch {
    match arch {
        "wasm32" => Arch::Wasm32,
        "wasm64" => Arch::Wasm64,
        _ => Arch::Other,
    }
}

#[derive(Debug)]
struct CrateMeta {
    name: String,
    origin: CrateOrigin,
    version: Option<String>,
    target_kind: TargetKind,
}

#[derive(Debug)]
struct FileMeta {
    path: String,
    deps: Vec<String>,
    extern_prelude: Option<Vec<String>>,
    cfg: CfgOptions,
    edition: Edition,
    env: Env,
    crate_attrs: Vec<String>,
    introduce_new_source_root: Option<SourceRootKind>,
}

impl FileMeta {
    fn from_fixture(f: FixtureFile, current: SourceRootKind) -> (FileMeta, Option<CrateMeta>) {
        let mut cfg = CfgOptions::default();
        for (k, v) in f.cfgs {
            match v {
                Some(v) => cfg.insert_key_value(Symbol::intern(&k), Symbol::intern(&v)),
                None => cfg.insert_atom(Symbol::intern(&k)),
            }
        }
        let introduce_new_source_root = f.introduce_new_source_root.map(|kind| match &*kind {
            "local" => SourceRootKind::Local,
            "library" => SourceRootKind::Library,
            invalid => panic!("invalid source root kind '{invalid}'"),
        });
        let current = introduce_new_source_root.unwrap_or(current);
        let krate = f.krate.map(|it| parse_crate(it, current, f.library));
        let meta = FileMeta {
            path: f.path,
            deps: f.deps,
            extern_prelude: f.extern_prelude,
            cfg,
            edition: f.edition.map_or(Edition::CURRENT, |v| Edition::from_str(&v).unwrap()),
            env: f.env.into_iter().collect(),
            crate_attrs: f.crate_attrs,
            introduce_new_source_root,
        };
        (meta, krate)
    }
}

/// `crate:` metadata: `name`, `name@0.0.1,http://example.com`, `bin:name`,
/// `bin:name@0.0.1,http://example.com`, and any of these after `r#` for a crate named like a
/// language crate that is not one.
fn parse_crate(crate_str: String, current: SourceRootKind, library: bool) -> CrateMeta {
    let (crate_str, not_lang) = match crate_str.strip_prefix("r#") {
        Some(rest) => (rest.to_owned(), true),
        None => (crate_str, false),
    };
    let (name, repo, version) = match crate_str.split_once('@') {
        Some((name, rest)) => {
            let (version, repo) =
                rest.split_once(',').expect("crate meta: found '@' without version and url");
            (name.to_owned(), Some(repo.to_owned()), Some(version.to_owned()))
        }
        None => (crate_str, None, None),
    };
    let (target_kind, name) = match name.split_once(':') {
        Some((kind, name)) => {
            let kind = match kind {
                "lib" => TargetKind::Lib { is_proc_macro: false },
                "proc-macro" => TargetKind::Lib { is_proc_macro: true },
                "bin" => TargetKind::Bin,
                "example" => TargetKind::Example,
                "test" => TargetKind::Test,
                "bench" => TargetKind::Bench,
                "custom-build" => TargetKind::BuildScript,
                "other" => TargetKind::Other,
                _ => panic!("crate meta: unexpected target kind {kind}"),
            };
            (kind, name.to_owned())
        }
        None => (TargetKind::Lib { is_proc_macro: false }, name),
    };
    let not_member = library || current == SourceRootKind::Library;
    let lang = match not_lang {
        true => LangCrateOrigin::Other,
        false => LangCrateOrigin::from(&*name),
    };
    let origin = match lang {
        LangCrateOrigin::Other => {
            let name = Symbol::intern(&name);
            match not_member {
                true => CrateOrigin::Library { repo, name },
                false => CrateOrigin::Local { repo, name: Some(name) },
            }
        }
        lang => CrateOrigin::Lang(lang),
    };
    CrateMeta { name, origin, version, target_kind }
}

fn test_proc_macro(name: &str, kind: ProcMacroKind, expander: sync::Arc<dyn ProcMacroExpander>) -> ProcMacro { // dyn: hir-expand's `ProcMacro::expander`, made once per fixture
    ProcMacro { name: Symbol::intern(name), kind, expander, disabled: false }
}

fn default_test_proc_macros() -> [(String, ProcMacro); 11] {
    [
        (
            r#"
#[proc_macro_attribute]
pub fn identity(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}
"#
            .into(),
            test_proc_macro("identity", ProcMacroKind::Attr, sync::Arc::new(IdentityProcMacroExpander)),
        ),
        (
            r#"
#[proc_macro_derive(DeriveIdentity)]
pub fn derive_identity(item: TokenStream) -> TokenStream {
    item
}
"#
            .into(),
            test_proc_macro(
                "DeriveIdentity",
                ProcMacroKind::CustomDerive,
                sync::Arc::new(IdentityProcMacroExpander),
            ),
        ),
        (
            r#"
#[proc_macro_attribute]
pub fn input_replace(attr: TokenStream, _item: TokenStream) -> TokenStream {
    attr
}
"#
            .into(),
            test_proc_macro(
                "input_replace",
                ProcMacroKind::Attr,
                sync::Arc::new(AttributeInputReplaceProcMacroExpander),
            ),
        ),
        (
            r#"
#[proc_macro]
pub fn mirror(input: TokenStream) -> TokenStream {
    input
}
"#
            .into(),
            test_proc_macro("mirror", ProcMacroKind::Bang, sync::Arc::new(MirrorProcMacroExpander)),
        ),
        (
            r#"
#[proc_macro]
pub fn shorten(input: TokenStream) -> TokenStream {
    loop {}
}
"#
            .into(),
            test_proc_macro("shorten", ProcMacroKind::Bang, sync::Arc::new(ShortenProcMacroExpander)),
        ),
        (
            r#"
#[proc_macro_attribute]
pub fn issue_18089(_attr: TokenStream, _item: TokenStream) -> TokenStream {
    loop {}
}
"#
            .into(),
            test_proc_macro(
                "issue_18089",
                ProcMacroKind::Attr,
                sync::Arc::new(Issue18089ProcMacroExpander),
            ),
        ),
        (
            r#"
#[proc_macro_attribute]
pub fn issue_18840(_attr: TokenStream, _item: TokenStream) -> TokenStream {
    loop {}
}
"#
            .into(),
            test_proc_macro(
                "issue_18840",
                ProcMacroKind::Attr,
                sync::Arc::new(Issue18840ProcMacroExpander),
            ),
        ),
        (
            r#"
#[proc_macro]
pub fn issue_17479(input: TokenStream) -> TokenStream {
    input
}
"#
            .into(),
            test_proc_macro(
                "issue_17479",
                ProcMacroKind::Bang,
                sync::Arc::new(Issue17479ProcMacroExpander),
            ),
        ),
        (
            r#"
#[proc_macro_attribute]
pub fn issue_18898(_attr: TokenStream, input: TokenStream) -> TokenStream {
    input
}
"#
            .into(),
            test_proc_macro(
                "issue_18898",
                ProcMacroKind::Bang,
                sync::Arc::new(Issue18898ProcMacroExpander),
            ),
        ),
        (
            r#"
#[proc_macro_attribute]
pub fn disallow_cfg(_attr: TokenStream, input: TokenStream) -> TokenStream {
    input
}
"#
            .into(),
            test_proc_macro(
                "disallow_cfg",
                ProcMacroKind::Attr,
                sync::Arc::new(DisallowCfgProcMacroExpander),
            ),
        ),
        (
            r#"
#[proc_macro_attribute]
pub fn generate_suffixed_type(_attr: TokenStream, input: TokenStream) -> TokenStream {
    input
}
"#
            .into(),
            test_proc_macro(
                "generate_suffixed_type",
                ProcMacroKind::Attr,
                sync::Arc::new(GenerateSuffixedTypeProcMacroExpander),
            ),
        ),
    ]
}

/// The macros of `defs` that `names` names (in snake case), and the source of their
/// declarations: only for paths to them to resolve.
fn filter_test_proc_macros(
    names: &[String],
    defs: Vec<(String, ProcMacro)>,
) -> (Vec<ProcMacro>, String) {
    let mut source = String::new();
    let mut proc_macros = Vec::new();
    for (code, proc_macro) in defs {
        let name = stdx::to_lower_snake_case(proc_macro.name.as_str());
        if !names.iter().any(|it| *it == name) {
            continue;
        }
        proc_macros.push(proc_macro);
        source += &code;
    }
    (proc_macros, source)
}

/// Its input, as it is.
#[derive(Debug)]
struct IdentityProcMacroExpander;
impl ProcMacroExpander for IdentityProcMacroExpander {
    fn expand(
        &self,
        _: &Db,
        ProcMacroCall { subtree, .. }: ProcMacroCall<'_>,
    ) -> Result<TopSubtree, ProcMacroExpansionError> {
        Ok(subtree.copy())
    }

    fn eq_dyn(&self, other: &dyn ProcMacroExpander) -> bool { // dyn: hir-expand's `ProcMacroExpander::eq_dyn`; compared when a crate's macros are set
        other.type_id() == TypeId::of::<Self>()
    }
}

/// Its input after a `macro_rules!` macro it exports under the name of its second token,
/// for issue #18089.
#[derive(Debug)]
struct Issue18089ProcMacroExpander;
impl ProcMacroExpander for Issue18089ProcMacroExpander {
    fn expand(
        &self,
        _: &Db,
        ProcMacroCall { subtree, call_site, .. }: ProcMacroCall<'_>,
    ) -> Result<TopSubtree, ProcMacroExpansionError> {
        let Some(TtElement::Leaf(macro_name)) = subtree.iter().nth(1) else {
            return Err(ProcMacroExpansionError::Panic("incorrect input".to_owned()));
        };
        Ok(quote! { call_site =>
            #[macro_export]
            macro_rules! my_macro___ {
                ($($token:tt)*) => {{
                }};
            }

            pub use my_macro___ as #macro_name;

            #subtree
        })
    }

    fn eq_dyn(&self, other: &dyn ProcMacroExpander) -> bool { // dyn: hir-expand's `ProcMacroExpander::eq_dyn`; compared when a crate's macros are set
        other.type_id() == TypeId::of::<Self>()
    }
}

/// The attribute's input, in place of the item.
#[derive(Debug)]
struct AttributeInputReplaceProcMacroExpander;
impl ProcMacroExpander for AttributeInputReplaceProcMacroExpander {
    fn expand(
        &self,
        _: &Db,
        ProcMacroCall { attrs, .. }: ProcMacroCall<'_>,
    ) -> Result<TopSubtree, ProcMacroExpansionError> {
        attrs
            .map(|it| it.copy())
            .ok_or_else(|| ProcMacroExpansionError::Panic("Expected attribute input".into()))
    }

    fn eq_dyn(&self, other: &dyn ProcMacroExpander) -> bool { // dyn: hir-expand's `ProcMacroExpander::eq_dyn`; compared when a crate's macros are set
        other.type_id() == TypeId::of::<Self>()
    }
}

/// A `compile_error!` at the span the fixup gave a `fn foo() { let loop {} }`'s body, for
/// issue #18840.
#[derive(Debug)]
struct Issue18840ProcMacroExpander;
impl ProcMacroExpander for Issue18840ProcMacroExpander {
    fn expand(
        &self,
        _: &Db,
        ProcMacroCall { subtree: fn_, def_site, .. }: ProcMacroCall<'_>,
    ) -> Result<TopSubtree, ProcMacroExpansionError> {
        let mut iter = fn_.iter();
        iter.nth(2);
        let (_, mut fn_body) = iter.expect_subtree().unwrap();
        let fixed_up_span = fn_body.nth(1).unwrap().first_span();
        let mut result =
            quote! {fixed_up_span => ::core::compile_error! { "my cool compile_error!" } };
        // So that the top subtree is kept when the fixups are reversed.
        result.set_top_subtree_delimiter_span(tt::DelimSpan::from_single(def_site));
        Ok(result)
    }

    fn eq_dyn(&self, other: &dyn ProcMacroExpander) -> bool { // dyn: hir-expand's `ProcMacroExpander::eq_dyn`; compared when a crate's macros are set
        other.type_id() == TypeId::of::<Self>()
    }
}

/// Its input's token trees in reverse, each subtree's too.
#[derive(Debug)]
struct MirrorProcMacroExpander;
impl ProcMacroExpander for MirrorProcMacroExpander {
    fn expand(
        &self,
        _: &Db,
        ProcMacroCall { subtree: input, .. }: ProcMacroCall<'_>,
    ) -> Result<TopSubtree, ProcMacroExpansionError> {
        fn traverse(builder: &mut TopSubtreeBuilder, iter: TtIter<'_>) {
            for tt in iter.collect_vec().into_iter().rev() {
                match tt {
                    TtElement::Leaf(leaf) => builder.push(leaf),
                    TtElement::Subtree(subtree, subtree_iter) => {
                        builder.open(subtree.delimiter.kind, subtree.delimiter.open);
                        traverse(builder, subtree_iter);
                        builder.close(subtree.delimiter.close);
                    }
                }
            }
        }
        let mut builder = TopSubtreeBuilder::new(input.top_subtree().delimiter);
        traverse(&mut builder, input.iter());
        Ok(builder.build())
    }

    fn eq_dyn(&self, other: &dyn ProcMacroExpander) -> bool { // dyn: hir-expand's `ProcMacroExpander::eq_dyn`; compared when a crate's macros are set
        other.type_id() == TypeId::of::<Self>()
    }
}

/// Every literal made an empty string and every identifier its first letter, each token's
/// span kept: so tests show nothing assumes a token a macro kept the span of is unchanged.
#[derive(Debug)]
struct ShortenProcMacroExpander;
impl ProcMacroExpander for ShortenProcMacroExpander {
    fn expand(
        &self,
        _: &Db,
        ProcMacroCall { subtree: input, .. }: ProcMacroCall<'_>,
    ) -> Result<TopSubtree, ProcMacroExpansionError> {
        let mut result = input.copy();
        for (idx, it) in input.as_token_trees().iter_flat_tokens().enumerate() {
            if let TokenTree::Leaf(mut leaf) = it {
                modify_leaf(&mut leaf);
                result.set_token(idx, leaf);
            }
        }
        return Ok(result);

        fn modify_leaf(leaf: &mut Leaf) {
            match leaf {
                Leaf::Literal(it) => {
                    it.text_and_suffix = Symbol::empty();
                    it.suffix_len = 0;
                }
                Leaf::Punct(_) => {}
                Leaf::Ident(it) => {
                    it.sym = Symbol::intern(&it.sym.as_str().chars().take(1).collect::<String>());
                }
            }
        }
    }

    fn eq_dyn(&self, other: &dyn ProcMacroExpander) -> bool { // dyn: hir-expand's `ProcMacroExpander::eq_dyn`; compared when a crate's macros are set
        other.type_id() == TypeId::of::<Self>()
    }
}

/// A call of the function its string literal names, for issue #17479.
#[derive(Debug)]
struct Issue17479ProcMacroExpander;
impl ProcMacroExpander for Issue17479ProcMacroExpander {
    fn expand(
        &self,
        _: &Db,
        ProcMacroCall { subtree, .. }: ProcMacroCall<'_>,
    ) -> Result<TopSubtree, ProcMacroExpansionError> {
        let mut iter = subtree.iter();
        let Some(TtElement::Leaf(Leaf::Literal(lit))) = iter.next() else {
            return Err(ProcMacroExpansionError::Panic("incorrect Input".into()));
        };
        let symbol = Symbol::intern(lit.text());
        let span = lit.span;
        Ok(quote! { span =>
            #symbol()
        })
    }

    fn eq_dyn(&self, other: &dyn ProcMacroExpander) -> bool { // dyn: hir-expand's `ProcMacroExpander::eq_dyn`; compared when a crate's macros are set
        other.type_id() == TypeId::of::<Self>()
    }
}

/// A function whose body is a long block at its input's last span, for issue #18898.
#[derive(Debug)]
struct Issue18898ProcMacroExpander;
impl ProcMacroExpander for Issue18898ProcMacroExpander {
    fn expand(
        &self,
        _: &Db,
        ProcMacroCall { subtree, def_site, .. }: ProcMacroCall<'_>,
    ) -> Result<TopSubtree, ProcMacroExpansionError> {
        let span = subtree
            .token_trees()
            .last_span()
            .ok_or_else(|| ProcMacroExpansionError::Panic("malformed input".to_owned()))?;
        let overly_long_subtree = quote! {span =>
            {
                let a = 5;
                let a = 5;
                let a = 5;
                let a = 5;
                let a = 5;
                let a = 5;
                let a = 5;
                let a = 5;
                let a = 5;
                let a = 5;
                let a = 5;
                let a = 5;
                let a = 5;
                let a = 5;
                let a = 5;
                let a = 5;
                let a = 5;
                let a = 5;
                let a = 5;
            }
        };
        Ok(quote! { def_site =>
            fn foo() {
                #overly_long_subtree
            }
        })
    }

    fn eq_dyn(&self, other: &dyn ProcMacroExpander) -> bool { // dyn: hir-expand's `ProcMacroExpander::eq_dyn`; compared when a crate's macros are set
        other.type_id() == TypeId::of::<Self>()
    }
}

/// Its input, or an error if a `cfg` or `cfg_attr` is left in it.
#[derive(Debug)]
struct DisallowCfgProcMacroExpander;
impl ProcMacroExpander for DisallowCfgProcMacroExpander {
    fn expand(
        &self,
        _: &Db,
        ProcMacroCall { subtree, .. }: ProcMacroCall<'_>,
    ) -> Result<TopSubtree, ProcMacroExpansionError> {
        for tt in subtree.token_trees().iter_flat_tokens() {
            if let TokenTree::Leaf(Leaf::Ident(ident)) = tt
                && (ident.sym == sym::cfg || ident.sym == sym::cfg_attr)
            {
                return Err(ProcMacroExpansionError::Panic(
                    "cfg or cfg_attr found in DisallowCfgProcMacroExpander".to_owned(),
                ));
            }
        }
        Ok(subtree.copy())
    }

    fn eq_dyn(&self, other: &dyn ProcMacroExpander) -> bool { // dyn: hir-expand's `ProcMacroExpander::eq_dyn`; compared when a crate's macros are set
        other.type_id() == TypeId::of::<Self>()
    }
}

/// Its input, a struct or enum, and a unit struct named after it with `Suffix` added.
#[derive(Debug)]
struct GenerateSuffixedTypeProcMacroExpander;
impl ProcMacroExpander for GenerateSuffixedTypeProcMacroExpander {
    fn expand(
        &self,
        _: &Db,
        ProcMacroCall { subtree, call_site, .. }: ProcMacroCall<'_>,
    ) -> Result<TopSubtree, ProcMacroExpansionError> {
        let mut iter = subtree.iter();
        let Some(TtElement::Leaf(Leaf::Ident(ident))) = iter.next() else {
            return Err(ProcMacroExpansionError::Panic("incorrect Input".into()));
        };
        let ident = match ident.sym.as_str() {
            "struct" => {
                let Some(TtElement::Leaf(Leaf::Ident(ident))) = iter.next() else {
                    return Err(ProcMacroExpansionError::Panic("incorrect Input".into()));
                };
                ident
            }
            "enum" => {
                iter.next();
                let (_, mut iter) = iter.expect_subtree().unwrap();
                let Some(TtElement::Leaf(Leaf::Ident(ident))) = iter.next() else {
                    return Err(ProcMacroExpansionError::Panic("incorrect Input".into()));
                };
                ident
            }
            _ => return Err(ProcMacroExpansionError::Panic("incorrect Input".into())),
        };
        let generated_ident = tt::Ident {
            sym: Symbol::intern(&format!("{}Suffix", ident.sym)),
            span: ident.span,
            is_raw: tt::IdentIsRaw::No,
        };
        Ok(quote! { call_site =>
            #subtree

            struct #generated_ident;
        })
    }

    fn eq_dyn(&self, other: &dyn ProcMacroExpander) -> bool { // dyn: hir-expand's `ProcMacroExpander::eq_dyn`; compared when a crate's macros are set
        other.type_id() == TypeId::of::<Self>()
    }
}
