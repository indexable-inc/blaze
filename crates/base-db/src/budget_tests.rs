//! Allocation budgets of what every query reads first and of setting a view
//! (docs/v2/plan.md, "base-db: inputs and views"): a read allocates
//! nothing, a view's unchanged file allocates nothing, and a changed file costs a constant.
//! Each measures the setter alone, inside its edit; the commit after it is not counted.

use std::alloc::Global;

use crate::{AbsPathBuf, FileId, FileSet, FileSetConfig, VfsPath};
use span::Edition;
use triomphe::Arc;

use crate::{
    ContentId, CrateBuilderId, CrateGraphBuilder, CrateName, CrateOrigin, CrateWorkspaceData, Db,
    DependencyBuilder, EditionedFileId, Env, FileText, SourceRoot, SourceRootId, TargetKind,
    ViewChange, ViewFile, ViewRoot, relevant_crates,
    tests::{EditDb, set_graph, test_db},
};

const FILES: u32 = 64;

/// One local source root of `n` files, each `text` at `/{dir}/f{i}.rs`, committed.
fn root_of(db: &mut Db, n: u32, dir: &str, text: &str) -> Vec<FileId, Global> {
    let mut files = Vec::new_in(Global);
    files.extend((0..n).map(FileId::from_raw));
    let mut set = FileSet::default();
    let mut edit = db.edit();
    for &file in &files {
        edit.set_file_text(file, text).set_file_source_root(file, SourceRootId(0));
        set.insert(file, VfsPath::new_virtual_path(format!("/{dir}/f{}.rs", file.index())));
    }
    edit.set_source_root(SourceRootId(0), Arc::new(SourceRoot::new_local(set)));
    _ = edit.commit();
    files
}

/// One local source root of [`FILES`] files and a crate rooted at the first.
fn loaded() -> (Db, Vec<FileId, Global>) {
    let mut db = test_db();
    let files = root_of(&mut db, FILES, "budget", "fn f() { 1 }\n");
    let mut graph = CrateGraphBuilder::default();
    lib_root(&mut graph, files[0]);
    set_graph(&mut db, graph);
    (db, files)
}

/// A library crate rooted at `root`, with no cfg, env or dependencies.
fn lib_root(graph: &mut CrateGraphBuilder, root: FileId) -> CrateBuilderId {
    graph.add_crate_root(
        root,
        Edition::Edition2021,
        None,
        None,
        Default::default(),
        None,
        Env::default(),
        CrateOrigin::Local { repo: None, name: None },
        Vec::new(),
        TargetKind::Lib { is_proc_macro: false },
        // No proc macro runs here: the cwd is only hashed, so the crate's own directory.
        Arc::new(AbsPathBuf::assert_utf8(env!("CARGO_MANIFEST_DIR").into())),
        Arc::new(CrateWorkspaceData {
            target: Err("no target in budget tests".into()),
            toolchain: None,
            const_eval: Default::default(),
            trusted: Box::default(),
        }),
    )
}

#[test]
fn a_keystroke_allocates_only_its_text_and_slot() {
    let (mut db, files) = loaded();
    for _ in 0..4 {
        for text in ["fn f() { 2 }\n", "fn f() { 1 }\n"] {
            db.commit_with(|edit| {
                edit.set_file_text(files[1], text);
            });
        }
    }
    let content = db.source_root(SourceRootId(0)).content(&db);
    let mut edit = db.edit();
    let ((), used) = blaze_alloc_count::measure(|| {
        edit.set_file_text(files[1], "fn f() { 2 }\n");
    });
    _ = edit.commit();
    // The file's text and its reparse slot; the setter's batches are in its frame.
    assert!(used.allocations <= 2, "a keystroke in a root of {FILES} files: {used:?}");
    assert_ne!(db.source_root(SourceRootId(0)).content(&db), content);
    assert_eq!(db.file_text(files[1]).content(&db), FileText::content_of("fn f() { 2 }\n", None));
}

#[test]
fn crate_deps_are_memo_hits() {
    let (mut db, files) = loaded();
    let mut graph = CrateGraphBuilder::default();
    let a = lib_root(&mut graph, files[0]);
    let c = lib_root(&mut graph, files[2]);
    graph.add_dep(a, DependencyBuilder::new(CrateName::new("c").unwrap(), c)).unwrap();
    let keys = set_graph(&mut db, graph);
    let (a, c) = (db.krate(keys[&a]), db.krate(keys[&c]));
    assert_eq!(a.transitive_deps(&db), &[a, c]);
    // The reverse walk is not a query (a crate's arena reads no dependent): it writes the
    // caller's buffer, warm after the first call.
    let mut dependents = Vec::new_in(Global);
    c.transitive_rev_deps(&db, &mut dependents);
    assert_eq!(dependents, [c, a]);
    let (_, used) = blaze_alloc_count::measure(|| {
        c.transitive_rev_deps(&db, &mut dependents);
        a.transitive_deps(&db).len() + dependents.len()
    });
    assert_eq!(used.allocations, 0, "crate deps, warm: {used:?}");
    assert_eq!(dependents, [c, a], "the warm walk's answer");
}

/// Setting a graph equal to the one set allocates the call's own tables and nothing per
/// crate: no crate's data is built and none is hashed.
#[test]
fn an_unchanged_crate_graph_allocates_a_constant() {
    // The walk's tables (its ids, its done list, its order, its root files, its map) and the
    // set of the crates it walked, each at the graph's size.
    const PER_CALL: u64 = 8;
    let cost = |n: u32| {
        let mut db = test_db();
        let files = root_of(&mut db, n, "graph", "fn f() {}\n");
        // Each crate depends on the one before it.
        let graph = || {
            let mut graph = CrateGraphBuilder::default();
            let mut before = None;
            for &file in &files {
                let id = lib_root(&mut graph, file);
                if let Some(before) = before {
                    let name = CrateName::new("before").unwrap();
                    graph.add_dep(id, DependencyBuilder::new(name, before)).unwrap();
                }
                before = Some(id);
            }
            graph
        };
        set_graph(&mut db, graph());
        set_graph(&mut db, graph());
        let equal = graph();
        let mut edit = db.edit();
        let ((), used) = blaze_alloc_count::measure(|| {
            edit.set_crate_graph(equal);
        });
        let committed = edit.commit();
        assert_eq!(committed.crates.len(), n as usize);
        assert!(committed.dropped.is_empty(), "an equal graph dropped {:?}", committed.dropped);
        used.allocations
    };
    let few = cost(8);
    let many = cost(256);
    assert!(few <= PER_CALL, "an unchanged graph of 8 crates: {few} allocations");
    assert_eq!(many, few, "an unchanged graph of 256 crates against 8");
}

/// Every read the brief names, of every file: its text, whether it has text, its source root
/// and the root's file set, its crates and its parse.
fn read_all(db: &Db, files: &[FileId]) -> usize {
    let mut seen = 0;
    for &file in files {
        seen += db.file_text(file).text(db).len();
        seen += usize::from(db.has_file_text(file));
        let root = db.file_source_root(file).source_root_id(db);
        seen +=
            usize::from(db.source_root(root).source_root_ref(db).path_for_file(&file).is_some());
        seen += relevant_crates(db, file).len();
        seen += usize::from(
            EditionedFileId::new(db, file, Edition::Edition2021).parse(db).is_outline(),
        );
    }
    seen
}

#[test]
fn reads_allocate_nothing() {
    let (db, files) = loaded();
    let warm = read_all(&db, &files);
    let (again, used) = blaze_alloc_count::measure(|| read_all(&db, &files));
    assert_eq!(again, warm);
    assert_eq!(used.allocations, 0, "reads of {FILES} files, every memo warm: {used:?}");
}

/// Source roots 0 (the checkout) and 1 (everything else), both empty.
fn checkout() -> (Db, AbsPathBuf, FileSetConfig) {
    let mut db = test_db();
    // A view is held in memory and its base is never read from disk: a path that is not there.
    let base =
        AbsPathBuf::assert_utf8(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("blaze-base-db-budgets"));
    let mut partition = FileSetConfig::builder();
    partition.add_file_set(vec![VfsPath::from(base.clone())]);
    db.commit_with(|edit| {
        for root in [0, 1] {
            edit.set_source_root(SourceRootId(root), Arc::new(SourceRoot::new_local(FileSet::default())));
        }
    });
    (db, base, partition.build())
}

/// `n` files under `src/`, the first `edited` of them with another body than the rest have,
/// each in an allocation of its own as blaze-vfs holds a heap file.
fn texts(n: usize, edited: usize) -> Vec<(String, blaze_vfs::Bytes), Global> {
    let mut texts = Vec::with_capacity_in(n, Global);
    texts.extend((0..n).map(|at| {
        let body = if at < edited { 2 } else { 1 };
        let text = format!("fn f{at}() {{ {body} }}\n");
        let bytes = std::sync::Arc::<[u8]>::from(text.as_bytes());
        (format!("src/m{at}.rs"), blaze_vfs::Bytes::from(bytes))
    }));
    texts
}

fn view_files(files: &[(String, blaze_vfs::Bytes)]) -> Vec<ViewFile<'_>, Global> {
    let mut views = Vec::with_capacity_in(files.len(), Global);
    views.extend(files.iter().map(|(path, bytes)| ViewFile {
        path: path.as_str(),
        id: ContentId::of(blaze_vfs::FILE_DOMAIN, bytes),
        bytes,
    }));
    views
}

/// Sets the view and counts what that alone allocated (the view's list is made before, the
/// commit after), and commits.
fn set_view(
    db: &mut Db,
    base: &AbsPathBuf,
    built: &[AbsPathBuf],
    partition: &FileSetConfig,
    files: &[ViewFile<'_>],
) -> (ViewChange, blaze_alloc_count::Used) {
    let mut edit = db.edit();
    let measured = blaze_alloc_count::measure(|| {
        edit.set_view_root(ViewRoot { base, built, partition, other: SourceRootId(1), files })
            .expect("a view of good paths and matching IDs")
    });
    _ = edit.commit();
    measured
}

#[test]
fn an_unchanged_view_allocates_nothing() {
    for n in [8, 512] {
        let (mut db, base, partition) = checkout();
        let built = [base.join("target")];
        let files = texts(n, 0);
        let view = view_files(&files);
        let (first, _) = set_view(&mut db, &base, &[], &partition, &view);
        assert_eq!(first.added.len(), n);
        for built in [&[][..], &built[..]] {
            let (again, used) = set_view(&mut db, &base, built, &partition, &view);
            assert_eq!(again, ViewChange::default());
            assert_eq!(
                used.allocations,
                0,
                "a view of {n} unchanged files, {} built: {used:?}",
                built.len()
            );
        }
    }
}

/// A file taken out of a view, or put back, edits its root's file set and content where they
/// are: what it allocates does not grow with the root.
#[test]
fn a_file_added_or_removed_costs_a_constant() {
    let cost = |n: usize| {
        let (mut db, base, partition) = checkout();
        let files = texts(n, 0);
        let view = view_files(&files);
        set_view(&mut db, &base, &[], &partition, &view);
        set_view(&mut db, &base, &[], &partition, &view[..n - 1]);
        set_view(&mut db, &base, &[], &partition, &view);
        let (gone, removed) = set_view(&mut db, &base, &[], &partition, &view[..n - 1]);
        assert_eq!(gone.removed.len(), 1);
        let (back, added) = set_view(&mut db, &base, &[], &partition, &view);
        assert_eq!(back.added.len(), 1);
        (removed.allocations, added.allocations)
    };
    let few = cost(64);
    let many = cost(2048);
    // Put back: its joined path, its key, the set's two copies of the path and the lists the
    // call returns; taken out: the lists.
    assert!(few.0 <= 6 && few.1 <= 12, "a file of 64 taken out and put back: {few:?} allocations");
    assert!(many.0 <= few.0 && many.1 <= few.1, "of 2048: {many:?}, against {few:?} of 64");
}

#[test]
fn a_changed_file_costs_a_constant() {
    // The call's one allocation, `ViewChange::set` at its size; its batches are in its frame.
    const PER_CALL: u64 = 1;
    // Its reparse slot: the file's text is the view's bytes, not a copy.
    const PER_CHANGED_FILE: u64 = 1;
    let cost = |edited: usize| {
        let (mut db, base, partition) = checkout();
        let files = texts(64, 0);
        set_view(&mut db, &base, &[], &partition, &view_files(&files));
        let edits = texts(64, edited);
        let view = view_files(&edits);
        let (change, used) = set_view(&mut db, &base, &[], &partition, &view);
        assert_eq!(change.set.len(), edited);
        used.allocations
    };
    let one = cost(1);
    let many = cost(32);
    assert!(one <= PER_CALL + PER_CHANGED_FILE, "one changed file of 64: {one} allocations");
    assert!(
        many <= one + 31 * PER_CHANGED_FILE,
        "32 changed files of 64: {many} allocations, against {one} for one"
    );
}
