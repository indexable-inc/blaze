//! Allocation budgets of what every query reads first and of setting a view
//! (docs/v2/plan/lanes/db/speed-of-light.md, "base-db: inputs and views"): a read allocates
//! nothing, a view's unchanged file allocates nothing, and a changed file costs a constant.

use blaze_query::db::Durability;
use span::Edition;
use triomphe::Arc;
use crate::{AbsPathBuf, FileId, FileSet, FileSetConfig, VfsPath};

use crate::{
    ContentId, CrateBuilderId, CrateGraphBuilder, CrateName, CrateOrigin, CrateWorkspaceData, Db,
    DependencyBuilder, EditionedFileId, Env, FileText, SourceRoot, SourceRootId, TargetKind,
    ViewChange, ViewFile, ViewRoot, relevant_crates,
};

const FILES: u32 = 64;

/// One local source root of [`FILES`] files and a crate rooted at the first.
fn loaded() -> (Db, Vec<FileId>) {
    let mut db = Db::for_tests();
    let files: Vec<FileId> = (0..FILES).map(FileId::from_raw).collect();
    let mut set = FileSet::default();
    for &file in &files {
        db.set_file_text_with_durability(file, "fn f() { 1 }\n", Durability::LOW);
        db.set_file_source_root_with_durability(file, SourceRootId(0), Durability::MEDIUM);
        set.insert(file, VfsPath::new_virtual_path(format!("/budget/f{}.rs", file.index())));
    }
    let root = Arc::new(SourceRoot::new_local(set));
    db.set_source_root_with_durability(SourceRootId(0), root, Durability::MEDIUM);
    let mut graph = CrateGraphBuilder::default();
    lib_root(&mut graph, files[0]);
    graph.set_in_db(&mut db);
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
        Arc::new(AbsPathBuf::assert_utf8(std::env::temp_dir())),
        Arc::new(CrateWorkspaceData {
            target: Err("no target in budget tests".into()),
            toolchain: None,
            const_eval: Default::default(),
        }),
    )
}

#[test]
fn a_keystroke_allocates_only_its_text_and_slot() {
    let (mut db, files) = loaded();
    for _ in 0..4 {
        db.set_file_text(files[1], "fn f() { 2 }\n");
        db.set_file_text(files[1], "fn f() { 1 }\n");
    }
    let content = db.source_root(SourceRootId(0)).content(&db);
    let ((), used) = blaze_alloc_count::measure(|| db.set_file_text(files[1], "fn f() { 2 }\n"));
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
    let ids = graph.set_in_db(&mut db);
    let (a, c) = (ids[&a], ids[&c]);
    assert_eq!(a.transitive_deps(&db), &[a, c]);
    assert_eq!(c.transitive_rev_deps(&db), &[c, a]);
    let (_, used) = blaze_alloc_count::measure(|| {
        a.transitive_deps(&db).len() + c.transitive_rev_deps(&db).len()
    });
    assert_eq!(used.allocations, 0, "crate deps, warm: {used:?}");
}

/// Setting a graph equal to the one set allocates the call's maps and nothing per crate: no
/// crate's data is built and none is hashed.
#[test]
fn an_unchanged_crate_graph_allocates_a_constant() {
    // `set_in_db`'s returned map, the crate list's two tables and the root files' set, each at
    // the graph's size.
    const PER_CALL: u64 = 4;
    let cost = |n: u32| {
        let mut db = Db::for_tests();
        let files: Vec<FileId> = (0..n).map(FileId::from_raw).collect();
        let mut set = FileSet::default();
        for &file in &files {
            db.set_file_text_with_durability(file, "fn f() {}\n", Durability::LOW);
            db.set_file_source_root_with_durability(file, SourceRootId(0), Durability::MEDIUM);
            set.insert(file, VfsPath::new_virtual_path(format!("/graph/c{}/lib.rs", file.index())));
        }
        let root = Arc::new(SourceRoot::new_local(set));
        db.set_source_root_with_durability(SourceRootId(0), root, Durability::MEDIUM);
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
        graph().set_in_db(&mut db);
        graph().set_in_db(&mut db);
        let equal = graph();
        let (ids, used) = blaze_alloc_count::measure(|| equal.set_in_db(&mut db));
        assert_eq!(ids.len(), n as usize);
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
        seen += usize::from(db.source_root(root).source_root_ref(db).path_for_file(&file).is_some());
        seen += relevant_crates(db, file).len();
        seen += usize::from(EditionedFileId::new(db, file, Edition::Edition2021).parse(db).is_outline());
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
    let mut db = Db::for_tests();
    let base = AbsPathBuf::assert_utf8(std::env::temp_dir().join("blaze-base-db-budgets"));
    let mut partition = FileSetConfig::builder();
    partition.add_file_set(vec![VfsPath::from(base.clone())]);
    for root in [0, 1] {
        let empty = Arc::new(SourceRoot::new_local(FileSet::default()));
        db.set_source_root_with_durability(SourceRootId(root), empty, Durability::MEDIUM);
    }
    (db, base, partition.build())
}

/// `n` files under `src/`, the first `edited` of them with another body than the rest have,
/// each in an allocation of its own as blaze-vfs holds a heap file.
fn texts(n: usize, edited: usize) -> Vec<(String, blaze_vfs::Bytes)> {
    (0..n)
        .map(|at| {
            let body = if at < edited { 2 } else { 1 };
            let text = format!("fn f{at}() {{ {body} }}\n");
            let bytes = std::sync::Arc::<[u8]>::from(text.as_bytes());
            (format!("src/m{at}.rs"), blaze_vfs::Bytes::from(bytes))
        })
        .collect()
}

fn view_files(files: &[(String, blaze_vfs::Bytes)]) -> Vec<ViewFile<'_>> {
    files
        .iter()
        .map(|(path, bytes)| ViewFile {
            path: path.as_str(),
            id: ContentId::of(blaze_vfs::FILE_DOMAIN, bytes),
            bytes,
        })
        .collect()
}

/// Sets the view and counts what that alone allocated: the view's list is made before.
fn set_view(
    db: &mut Db,
    base: &AbsPathBuf,
    built: &[AbsPathBuf],
    partition: &FileSetConfig,
    files: &[ViewFile<'_>],
) -> (ViewChange, blaze_alloc_count::Used) {
    blaze_alloc_count::measure(|| {
        db.set_view_root(ViewRoot { base, built, partition, other: SourceRootId(1), files })
            .expect("a view of good paths and matching IDs")
    })
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
                used.allocations, 0,
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
