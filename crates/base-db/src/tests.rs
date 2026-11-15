use std::{
    alloc::Global,
    cell::Cell,
    sync::atomic::{AtomicUsize, Ordering},
};

use crate::{AbsPathBuf, AnchoredPath, FileId, FileSet, FileSetConfig, Looks, VfsPath};
use blaze_ids::rng::SplitMix64 as Rng;
use cfg::{CfgAtom, CfgOptions};
use intern::Symbol;
use span::{Edition, TextSize};
use syntax::TextRange;
use triomphe::Arc;

use crate::{
    ContentId, Crate, CrateBuilderId, CrateGraphBuilder, CrateName, CrateOrigin,
    CrateType, CrateTypes, CrateWorkspaceData, Db, DependencyBuilder, EditionedFileId, Env,
    FileText, SourceRoot, SourceRootId, TargetKind, ViewChange, ViewError, ViewFile, ViewRoot,
    all_crates_content, cfg_options_content,
    input::Dependency,
    reparse::{
        REPARSED, TextEdit, assert_whole, common_prefix, common_prefix_scalar, common_suffix,
        common_suffix_scalar,
    },
    target::{Arch, TargetData},
    db::{Committed, Edit},
    world::{CrateKey, Plant},
};

mod id_space;
mod memo_properties;
mod world_model;

#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

/// The fact boundaries of base-db's own test databases: none. base-db declares no boundary
/// kind of its own; a build's database is given `blaze_item_hash::reads::mark_boundaries` by the
/// crates above, which base-db cannot name.
pub(crate) fn no_boundaries(_: &mut blaze_memo::Builder) {}

/// A test database ([`Db::for_tests`]) with base-db's boundaries.
pub(crate) fn test_db() -> Db {
    Db::for_tests(no_boundaries)
}

/// One edit of a database, made and committed in one call: [`Edit::commit`] takes the edit by
/// value, so a setter's `&mut Edit` cannot end a chain.
pub(crate) trait EditDb {
    /// `change` made in one edit, committed: what the commit did.
    fn commit_with(&mut self, change: impl FnOnce(&mut Edit<'_>)) -> Committed;

    /// `change` made in one edit, committed: what it gave back and what the commit did.
    fn edit_with<R>(&mut self, change: impl FnOnce(&mut Edit<'_>) -> R) -> (R, Committed);
}

impl EditDb for Db {
    fn commit_with(&mut self, change: impl FnOnce(&mut Edit<'_>)) -> Committed {
        self.edit_with(change).1
    }

    fn edit_with<R>(&mut self, change: impl FnOnce(&mut Edit<'_>) -> R) -> (R, Committed) {
        let mut edit = self.edit();
        let out = change(&mut edit);
        (out, edit.commit())
    }
}

/// Puts `files` (path, text) in source root 0, as file IDs from `first` on, and commits.
pub(crate) fn load(db: &mut Db, first: u32, files: &[(&str, &str)]) -> Vec<FileId> {
    let mut set = FileSet::default();
    let mut ids = Vec::new_in(Global);
    let mut edit = db.edit();
    for (n, (path, text)) in files.iter().enumerate() {
        let id = FileId::from_raw(first + n as u32);
        edit.set_file_text(id, text).set_file_source_root(id, SourceRootId(0));
        set.insert(id, VfsPath::new_virtual_path(path.to_string()));
        ids.push(id);
    }
    edit.set_source_root(SourceRootId(0), Arc::new(SourceRoot::new_local(set)));
    _ = edit.commit();
    ids
}

/// Sets `graph` and commits: the keys of its crates.
pub(crate) fn set_graph(db: &mut Db, graph: CrateGraphBuilder) -> crate::CratesIdMap {
    db.commit_with(|edit| {
        edit.set_crate_graph(graph);
    })
    .crates
}

/// A dependency on `crate_id` by `name`, in the prelude or not, a sysroot one or not.
fn keyed_dep(name: CrateName, crate_id: CrateKey, prelude: bool, sysroot: bool) -> Dependency<CrateKey> {
    Dependency { crate_id, name, prelude, sysroot, private: false }
}

fn cfg(atoms: &[CfgAtom]) -> CfgOptions {
    atoms.iter().cloned().collect()
}

fn flag(name: &str) -> CfgAtom {
    CfgAtom::Flag(Symbol::intern(name))
}

fn key_value(key: &str, value: &str) -> CfgAtom {
    CfgAtom::KeyValue { key: Symbol::intern(key), value: Symbol::intern(value) }
}

fn add(graph: &mut CrateGraphBuilder, root: FileId, cfg: CfgOptions, env: Env) -> CrateBuilderId {
    add_with_attrs(graph, root, cfg, env, &[])
}

/// [`add`], with `attrs` its crate attributes given on the command line.
fn add_with_attrs(
    graph: &mut CrateGraphBuilder,
    root: FileId,
    cfg: CfgOptions,
    env: Env,
    attrs: &[&str],
) -> CrateBuilderId {
    graph.add_crate_root(
        root,
        Edition::Edition2021,
        None,
        None,
        cfg,
        None,
        env,
        CrateOrigin::Local { repo: None, name: None },
        attrs.iter().map(|it| it.to_string()).collect(),
        TargetKind::Lib { is_proc_macro: false },
        // No proc macro runs here: the cwd is only hashed, so the crate's own directory.
        Arc::new(AbsPathBuf::assert_utf8(env!("CARGO_MANIFEST_DIR").into())),
        Arc::new(CrateWorkspaceData {
            target: Err("no target in tests".into()),
            toolchain: None,
            const_eval: Default::default(),
            trusted: Box::default(),
        }),
    )
}

fn depend(graph: &mut CrateGraphBuilder, from: CrateBuilderId, to: CrateBuilderId, name: &str) {
    graph.add_dep(from, DependencyBuilder::new(CrateName::new(name).unwrap(), to)).unwrap();
}

const FILES: &[(&str, &str)] =
    &[("/a/lib.rs", "pub mod b;"), ("/a/b.rs", "pub fn f() {}"), ("/c/lib.rs", "pub struct C;")];

#[test]
fn equal_content_gives_equal_ids_across_databases() {
    let ids = |first: u32| {
        let mut db = test_db();
        let files = load(&mut db, first, FILES);
        let mut graph = CrateGraphBuilder::default();
        let a = add(&mut graph, files[0], cfg(&[flag("unix")]), Env::default());
        let c = add(&mut graph, files[2], cfg(&[flag("unix")]), Env::default());
        depend(&mut graph, a, c, "c");
        let crates = set_graph(&mut db, graph);
        (
            db.file_text(files[1]).content(&db),
            db.source_root(SourceRootId(0)).content(&db),
            db.krate(crates[&a]).content(&db),
            db.krate(crates[&c]).content(&db),
            all_crates_content(&db),
        )
    };
    // The file IDs differ between the two; nothing of them is hashed.
    assert_eq!(ids(0), ids(40));
}

#[test]
fn a_file_text_edit_changes_its_id_and_its_source_roots() {
    let mut db = test_db();
    let files = load(&mut db, 0, FILES);
    let ids =
        |db: &Db| (db.file_text(files[1]).content(db), db.source_root(SourceRootId(0)).content(db));
    let before = ids(&db);
    assert_eq!(before.0, FileText::content_of("pub fn f() {}", None));

    db.commit_with(|edit| { edit.set_file_text(files[1], "pub fn g() {}"); });
    let edited = ids(&db);
    assert_ne!(edited.0, before.0);
    assert_ne!(edited.1, before.1);
    assert_eq!(db.file_text(files[0]).content(&db), FileText::content_of("pub mod b;", None));

    db.commit_with(|edit| { edit.set_file_text(files[1], "pub fn f() {}"); });
    assert_eq!(ids(&db), before);

    let root = db.source_root(SourceRootId(0)).content(&db);
    db.commit_with(|edit| { edit.set_appended_root_module(files[0], Some("mod __harness {}")); });
    assert_eq!(
        db.file_text(files[0]).content(&db),
        FileText::content_of("pub mod b;", Some("mod __harness {}"))
    );
    assert_ne!(db.source_root(SourceRootId(0)).content(&db), root);
}

#[test]
fn a_files_parse_shares_its_text() {
    let mut db = test_db();
    let files = load(&mut db, 0, FILES);
    let parse = EditionedFileId::new(&db, files[1], Edition::Edition2021).parse(&db);
    let tree = parse.root().file();
    assert!(tree.source().ptr_eq(db.file_text(files[1]).text(&db)));
}

/// #1294: a dependency's file is an outline, whose `fn` bodies stay unparsed until reached.
#[test]
fn a_library_files_parse_leaves_bodies_unparsed() {
    let mut db = test_db();
    let files = load(&mut db, 0, &[("/a/lib.rs", "fn f() { let x = 1 + 1; }")]);
    let body_is_lazy = |db: &Db| {
        let parse = EditionedFileId::new(db, files[0], Edition::Edition2021).parse(db);
        let function = parse.root().children().next().expect("the `fn`");
        let body = function.children().find(|it| it.kind() == syntax::SyntaxKind::BLOCK_EXPR);
        body.expect("its body").is_lazy()
    };
    assert!(!body_is_lazy(&db));

    let mut set = FileSet::default();
    set.insert(files[0], VfsPath::new_virtual_path("/a/lib.rs".to_owned()));
    let root = Arc::new(SourceRoot::new_library(set));
    db.commit_with(|edit| { edit.set_source_root(SourceRootId(0), root); });
    assert!(body_is_lazy(&db));
}

#[test]
fn the_order_of_cfg_atoms_does_not_change_a_crates_id() {
    let atoms = [flag("unix"), key_value("feature", "a"), key_value("feature", "b"), flag("test")];
    let mut reversed = atoms.clone();
    reversed.reverse();
    assert_eq!(cfg_options_content(&cfg(&atoms)), cfg_options_content(&cfg(&reversed)));
    assert_ne!(cfg_options_content(&cfg(&atoms)), cfg_options_content(&cfg(&atoms[1..])));

    let crate_id = |atoms: &[CfgAtom]| {
        let mut db = test_db();
        let files = load(&mut db, 0, FILES);
        let mut graph = CrateGraphBuilder::default();
        let a = add(&mut graph, files[0], cfg(atoms), Env::default());
        let a = set_graph(&mut db, graph)[&a];
        db.krate(a).content(&db)
    };
    assert_eq!(crate_id(&atoms), crate_id(&reversed));
    assert_ne!(crate_id(&atoms), crate_id(&atoms[1..]));
}

#[test]
fn a_dependencys_change_changes_its_dependents_ids() {
    let mut db = test_db();
    let files = load(&mut db, 0, FILES);
    let mut graph = CrateGraphBuilder::default();
    let a = add(&mut graph, files[0], CfgOptions::default(), Env::default());
    let b = add(&mut graph, files[2], CfgOptions::default(), Env::default());
    let c = add(&mut graph, files[1], CfgOptions::default(), Env::default());
    depend(&mut graph, a, b, "b");
    let crates = set_graph(&mut db, graph);
    let (a, b, c): (CrateKey, CrateKey, CrateKey) = (crates[&a], crates[&b], crates[&c]);
    let ids = |db: &Db| {
        let content = |key| db.krate(key).content(db);
        (content(a), content(b), content(c), all_crates_content(db))
    };
    let before = ids(&db);

    let env: Env = [("KEY".to_owned(), "value".to_owned())].into_iter().collect();
    db.commit_with(|edit| { edit.set_crate_env(b, env.clone()); });
    assert_eq!(db.krate(b).env(&db), &env);
    let after = ids(&db);
    assert_ne!(after.1, before.1);
    assert_ne!(after.0, before.0);
    assert_eq!(after.2, before.2);
    // The list is of each crate's item key, which a crate's variables are not in: the session
    // arena, which every arena borrows, stays (`world::List`).
    assert_eq!(after.3, before.3, "a crate's variables moved the crate list");

    db.commit_with(|edit| { edit.set_crate_env(b, Env::default()); });
    assert_eq!(ids(&db), before);

    // A crate's cfg is in its item key and its dependents', so in the list.
    db.commit_with(|edit| { edit.set_crate_cfg_options(b, cfg(&[flag("moved")])); });
    let after = ids(&db);
    assert_ne!(after.1, before.1);
    assert_ne!(after.0, before.0);
    assert_eq!(after.2, before.2);
    assert_ne!(after.3, before.3, "a crate's cfg left the crate list as it was");

    db.commit_with(|edit| { edit.set_crate_cfg_options(b, CfgOptions::default()); });
    assert_eq!(ids(&db), before);
}

/// The world as an edit reads it ([`Edit::written_file_text`] and the rest): what a dropped
/// edit must leave as it found it.
#[derive(Debug, PartialEq)]
struct Written {
    text: Option<(String, ContentId)>,
    moved_path: Option<FileId>,
    cfg: CfgOptions,
    env: Env,
    /// How many crates are listed: a crate an edit makes takes a new key each time.
    listed: usize,
    primary: Vec<CrateKey, Global>,
    gift: Option<u32>,
}

/// The kind of the gifts [`written`] reads.
struct TestGift;

fn written(edit: &Edit<'_>, file: FileId, krate: CrateKey) -> Written {
    let moved = VfsPath::new_virtual_path("/a/moved.rs".to_owned());
    let crate_ = edit.written_crate(krate);
    let mut primary = Vec::new_in(Global);
    primary.extend_from_slice(edit.written_primary_crates());
    Written {
        text: edit.written_file_text(file).map(|(text, content)| (text.to_string(), content)),
        moved_path: edit.written_source_root(SourceRootId(0)).and_then(|it| it.file_for_path(&moved).copied()),
        cfg: crate_.cfg.clone(),
        env: crate_.env.clone(),
        listed: edit.written_all_crates().len(),
        primary,
        gift: edit.written_given::<TestGift, u32>(krate).copied(),
    }
}

/// The writes [`an_edit_reads_its_writes_and_one_dropped_puts_them_back`] and its panicking
/// twin make: a file's text, a crate's cfg and env, root 0 again with `b.rs` under another
/// path, the primary crates, a gift, a session maker and a crate made and listed.
fn write_everything(edit: &mut Edit<'_>, files: &[FileId], a: CrateKey) {
    edit.set_file_text(files[1], "pub fn g() {}");
    edit.set_crate_cfg_options(a, cfg(&[flag("now")]));
    edit.set_crate_env(a, [("NOW".to_owned(), "1".to_owned())].into_iter().collect());
    let mut moved = FileSet::default();
    for (file, path) in files.iter().zip(["/a/lib.rs", "/a/moved.rs", "/c/lib.rs"]) {
        moved.insert(*file, VfsPath::new_virtual_path(path.to_owned()));
    }
    edit.set_source_root(SourceRootId(0), Arc::new(SourceRoot::new_local(moved)));
    let mut primary = Vec::new_in(Global);
    primary.push(a);
    edit.set_primary_crates(primary);
    edit.give::<TestGift>(a, crate::Given::new(5_u32));
    let (runs, arena) = counters();
    edit.set_given(None, test_fact("dropped"), held_maker(&runs, &arena, test_fact("dropped"), Some(3)));
    let like_a = edit.written_crate(a);
    // clone: a second crate like `a`
    let (data, extra, workspace) = (like_a.data.clone(), like_a.extra.clone(), Arc::clone(like_a.workspace));
    let made = edit.create_crate(data, extra, workspace, CfgOptions::default(), Env::default());
    edit.add_crates(&[made]);
}

/// The invariant: an edit's world reads (`Edit::written_*`) see its own writes while the
/// snapshot it reads through (`Edit::db`) is the last commit's; an edit dropped uncommitted
/// puts every write back (the next edit reads the world as the last commit left it, and the
/// snapshot and its epoch are the same); and the same writes committed are all there.
#[test]
fn an_edit_reads_its_writes_and_one_dropped_puts_them_back() {
    let mut db = test_db();
    let files = load(&mut db, 0, FILES);
    let mut graph = CrateGraphBuilder::default();
    let a = add(&mut graph, files[0], CfgOptions::default(), Env::default());
    let crates = set_graph(&mut db, graph);
    let a = crates[&a];
    let (text_before, content_before) = (db.file_text(files[1]).text(&db).to_string(), db.file_content(files[1]));
    let epoch = db.snapshot_epoch();
    let before = written(&db.edit(), files[1], a);
    assert_eq!(before.gift, None);

    let mut edit = db.edit();
    write_everything(&mut edit, &files, a);
    let during = written(&edit, files[1], a);
    let (text, content) = during.text.clone().expect("a file given a text has one");
    assert_eq!(text, "pub fn g() {}");
    assert_ne!(content, content_before);
    assert_eq!(during.cfg, cfg(&[flag("now")]));
    assert_eq!(during.moved_path, Some(files[1]));
    assert_eq!(during.listed, 2, "the crate made and listed");
    assert_eq!(during.primary.as_slice(), &[a]);
    assert_eq!(during.gift, Some(5));
    assert!(edit.written_source_root(SourceRootId(9)).is_none(), "a root never set is none");
    // The snapshot is the last commit's.
    let read = edit.db();
    assert_eq!(read.file_text(files[1]).text(read).to_string(), text_before);
    assert_eq!(read.krate(a).cfg_options(read), &CfgOptions::default());
    let named = read.source_root(SourceRootId(0)).source_root(read);
    assert_eq!(named.file_for_path(&VfsPath::new_virtual_path("/a/moved.rs".to_owned())), None);
    drop(edit);

    assert_eq!(db.snapshot_epoch(), epoch, "a dropped edit makes no snapshot");
    assert_eq!(db.file_content(files[1]), content_before);
    assert_eq!(written(&db.edit(), files[1], a), before, "a dropped edit puts every write back");
    // The control: nothing is left to commit, so an empty commit keeps the snapshot.
    let committed = db.commit_with(|_| {});
    assert!(!committed.moved, "a dropped edit left a write behind: {committed:?}");
    assert_eq!(db.snapshot_epoch(), epoch);

    // The same writes committed are all there.
    let committed = db.commit_with(|edit| write_everything(edit, &files, a));
    assert!(committed.moved);
    assert_ne!(db.snapshot_epoch(), epoch);
    assert_eq!(written(&db.edit(), files[1], a), during, "the commit has every write");
    assert_eq!(&**db.file_text(files[1]).text(&db), "pub fn g() {}");
    assert_eq!(db.krate(a).cfg_options(&db), &cfg(&[flag("now")]));
    assert_eq!(&*crate::primary_crates(&db), &[db.krate(a)]);
    assert_eq!(crate::all_crates(&db).len(), 2);
    assert_eq!(Held::try_get(&db).map(|it| it.value(&db)), Some(3), "the session maker ran");
}

/// The invariant: an edit unwound by a panic, mid-edit or mid-commit (a maker that panics as
/// the commit makes its arena), puts every write back, and the database goes on: the old
/// snapshot answers, and the next commit makes a whole one.
#[test]
fn an_edit_unwound_by_a_panic_puts_its_writes_back() {
    let mut db = test_db();
    let files = load(&mut db, 0, FILES);
    let mut graph = CrateGraphBuilder::default();
    let a = add(&mut graph, files[0], CfgOptions::default(), Env::default());
    let a = set_graph(&mut db, graph)[&a];
    let epoch = db.snapshot_epoch();
    let before = written(&db.edit(), files[1], a);

    // Mid-edit.
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut edit = db.edit();
        write_everything(&mut edit, &files, a);
        panic!("a writer failed mid-edit");
    }));
    assert!(unwound.is_err());
    assert_eq!(db.snapshot_epoch(), epoch);
    assert_eq!(written(&db.edit(), files[1], a), before, "a panic mid-edit puts every write back");

    // Mid-commit: a maker of `a`'s arena panics while the commit makes it.
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut edit = db.edit();
        write_everything(&mut edit, &files, a);
        let panics: crate::MakeGiven = Box::new_in(
            |_: &blaze_memo::Builder, _: &blaze_memo::Inputs<'_>| panic!("a maker failed mid-commit"),
            Global,
        );
        edit.set_given(Some(a), test_fact("panics"), panics);
        _ = edit.commit();
    }));
    assert!(unwound.is_err());
    assert_eq!(db.snapshot_epoch(), epoch, "a commit that panicked made no snapshot");
    assert_eq!(&**db.file_text(files[1]).text(&db), FILES[1].1, "the old snapshot answers");
    assert_eq!(written(&db.edit(), files[1], a), before, "a panic mid-commit puts every write back");

    // The database goes on: the same writes, with no maker that panics, commit whole.
    let committed = db.commit_with(|edit| write_everything(edit, &files, a));
    assert!(committed.moved);
    assert_eq!(&**db.file_text(files[1]).text(&db), "pub fn g() {}");
    assert_eq!(db.krate(a).cfg_options(&db), &cfg(&[flag("now")]));
    assert_eq!(db.krate(a).given::<TestGift, u32>(&db), Some(&5));
}

/// The invariant: a commit that moved nothing (no write, or each a write of the value held)
/// makes no snapshot: [`Committed::moved`] is false, nothing is dropped, and the epoch stays.
/// The control: one real write moves it.
#[test]
fn a_commit_of_nothing_keeps_the_snapshot() {
    let mut db = test_db();
    let files = load(&mut db, 0, FILES);
    let mut graph = CrateGraphBuilder::default();
    let a = add(&mut graph, files[0], CfgOptions::default(), Env::default());
    let a = set_graph(&mut db, graph)[&a];
    db.commit_with(|edit| {
        edit.give::<TestGift>(a, crate::Given::new(1_u32));
    });
    let epoch = db.snapshot_epoch();
    let text = db.file_text(files[1]).text(&db).to_string();

    let committed = db.commit_with(|_| {});
    assert!(!committed.moved && committed.dropped.is_empty(), "an empty edit: {committed:?}");
    assert_eq!(db.snapshot_epoch(), epoch, "an empty edit moved the epoch");

    let held_gift = db.krate(a).given::<TestGift, u32>(&db).copied();
    let committed = db.commit_with(|edit| {
        let gift = edit.written_given::<TestGift, crate::Given>(a).is_some();
        assert!(!gift, "a gift is read as its own type, never as a `Given`");
        edit.set_file_text(files[1], &text)
            .set_crate_env(a, Env::default())
            .set_crate_cfg_options(a, CfgOptions::default())
            .set_library_roots(blaze_hash::HashSet::new_in(Global))
            .set_primary_crates(Vec::new_in(Global));
    });
    assert_eq!(held_gift, Some(1));
    assert!(!committed.moved && committed.dropped.is_empty(), "writes of the values held: {committed:?}");
    assert_eq!(db.snapshot_epoch(), epoch, "writes of the values held moved the epoch");

    // The control.
    let committed = db.commit_with(|edit| {
        edit.set_file_text(files[1], "pub fn h() {}");
    });
    assert!(committed.moved && !committed.dropped.is_empty(), "a write: {committed:?}");
    assert_ne!(db.snapshot_epoch(), epoch);
}

/// The invariant: [`Edit::written_given`] reads the gift of a kind as the world has it, with
/// the edit's own gifts: none before, the value given after, none as another type or kind; a
/// dropped edit's gift is gone, a committed one's is read by the snapshot.
#[test]
fn written_given_reads_the_edits_own_gifts() {
    struct OtherKind;
    let mut db = test_db();
    let files = load(&mut db, 0, FILES);
    let mut graph = CrateGraphBuilder::default();
    let a = add(&mut graph, files[0], CfgOptions::default(), Env::default());
    let a = set_graph(&mut db, graph)[&a];

    let mut edit = db.edit();
    assert_eq!(edit.written_given::<TestGift, u32>(a), None);
    edit.give::<TestGift>(a, crate::Given::new(7_u32));
    assert_eq!(edit.written_given::<TestGift, u32>(a), Some(&7));
    assert_eq!(edit.written_given::<TestGift, u64>(a), None, "another type");
    assert_eq!(edit.written_given::<OtherKind, u32>(a), None, "another kind");
    assert_eq!(edit.db().krate(a).given::<TestGift, u32>(edit.db()), None, "the snapshot's");
    drop(edit);

    let mut edit = db.edit();
    assert_eq!(edit.written_given::<TestGift, u32>(a), None, "a dropped edit's gift is gone");
    edit.give::<TestGift>(a, crate::Given::new(8_u32));
    edit.give::<TestGift>(a, crate::Given::new(9_u32));
    assert_eq!(edit.written_given::<TestGift, u32>(a), Some(&9), "the last gift");
    _ = edit.commit();
    assert_eq!(db.krate(a).given::<TestGift, u32>(&db), Some(&9));
    assert_eq!(db.edit().written_given::<TestGift, u32>(a), Some(&9));
}

#[test]
fn a_crates_build_target_changes_its_content_and_its_dependents() {
    let mut db = test_db();
    let files = load(&mut db, 0, FILES);
    let mut graph = CrateGraphBuilder::default();
    let a = add(&mut graph, files[0], CfgOptions::default(), Env::default());
    let b = add(&mut graph, files[2], CfgOptions::default(), Env::default());
    depend(&mut graph, a, b, "b");
    let crates = set_graph(&mut db, graph);
    let (a, b) = (crates[&a], crates[&b]);
    let ids = |db: &Db| (db.krate(a).content(db), db.krate(b).content(db), all_crates_content(db));
    let before = ids(&db);
    let original = db.krate(b).extra_data(&db).clone(); // clone: test restores the original input
    let mut seen = Vec::new_in(Global);
    seen.push(before.1);
    for built_for in [
        crate::BuiltFor { platform: Some(Symbol::intern("x86_64-unknown-linux-gnu")), macro_wasm: false },
        crate::BuiltFor { platform: Some(Symbol::intern("wasm32-wasip1")), macro_wasm: false },
        crate::BuiltFor { platform: Some(Symbol::intern("wasm32-wasip1")), macro_wasm: true },
    ] {
        let mut extra = original.clone(); // clone: change only what the crate is built for
        extra.built_for = built_for;
        db.commit_with(|edit| { edit.set_crate_extra_data(b, extra); });
        let after = ids(&db);
        assert_ne!(after.0, before.0, "the dependent's content follows its dependency");
        assert_ne!(after.2, before.2, "the graph's content follows its crates");
        assert!(!seen.contains(&after.1), "host, target and proc-macro builds are distinct");
        seen.push(after.1);
    }
    db.commit_with(|edit| { edit.set_crate_extra_data(b, original); });
    assert_eq!(ids(&db), before);
}

#[test]
fn a_crates_types_read_back_and_are_in_its_id() {
    let mut db = test_db();
    let files = load(&mut db, 0, FILES);
    let mut graph = CrateGraphBuilder::default();
    let a = add(&mut graph, files[0], CfgOptions::default(), Env::default());
    let a = set_graph(&mut db, graph)[&a];
    assert_eq!(db.krate(a).data(&db).crate_types, CrateTypes::of(&[CrateType::Lib]));
    let as_lib = db.krate(a).content(&db);

    let mut data = db.krate(a).data(&db).keyed(&db);
    data.crate_types = CrateTypes::of(&[CrateType::Cdylib, CrateType::Rlib]);
    db.commit_with(|edit| { edit.set_crate_data(a, data); });
    let types = db.krate(a).data(&db).crate_types;
    assert_eq!(types.iter().collect::<Vec<_>>(), [CrateType::Rlib, CrateType::Cdylib]);
    assert!(!types.contains(CrateType::Lib));
    assert_ne!(db.krate(a).content(&db), as_lib);
}

#[test]
fn a_dependency_added_outside_the_prelude_says_so() {
    let mut db = test_db();
    let files = load(&mut db, 0, FILES);
    let mut graph = CrateGraphBuilder::default();
    let a = add(&mut graph, files[0], CfgOptions::default(), Env::default());
    let b = add(&mut graph, files[2], CfgOptions::default(), Env::default());
    let crates = set_graph(&mut db, graph);
    let (a, b): (CrateKey, CrateKey) = (crates[&a], crates[&b]);
    let name = CrateName::new("b").unwrap();
    let depend_on = |db: &mut Db, dependency: Dependency<CrateKey>| {
        let mut data = db.krate(a).data(&*db).keyed(&*db);
        let mut dependencies = Vec::new_in(Global);
        dependencies.push(dependency);
        data.dependencies = dependencies;
        db.commit_with(|edit| { edit.set_crate_data(a, data); });
        db.krate(a).content(&*db)
    };

    let in_prelude = depend_on(&mut db, keyed_dep(name.clone(), b, true, false));
    let outside = depend_on(&mut db, keyed_dep(name.clone(), b, false, true));
    let krate = db.krate(a);
    let dependency = &krate.data(&db).dependencies[0];
    assert!(!dependency.is_prelude());
    assert!(dependency.is_sysroot());
    assert_eq!(dependency.crate_id, db.krate(b), "the dependency is read back as its crate");
    assert_ne!(outside, in_prelude);
    assert_eq!(depend_on(&mut db, keyed_dep(name, b, true, false)), in_prelude);
}

/// The texts of `krate`'s crate attributes' files, in order.
fn attr_texts(db: &Db, krate: CrateKey) -> Vec<String> {
    let text = |file: &FileId| -> String { (**db.file_text(*file).text(db)).to_owned() };
    db.krate(krate).crate_attr_files(db).iter().map(text).collect()
}

/// The files of `krate`'s crate attributes, in order.
fn attr_files(db: &Db, krate: CrateKey) -> Vec<FileId, Global> {
    let mut files = Vec::new_in(Global);
    files.extend_from_slice(db.krate(krate).crate_attr_files(db));
    files
}

/// A graph with crate `a` on `FILES`' first file, its crate attributes `attrs`, committed.
fn set_attrs(db: &mut Db, files: &[FileId], attrs: &[&str]) -> (CrateKey, Committed) {
    let mut graph = CrateGraphBuilder::default();
    let a = add_with_attrs(&mut graph, files[0], CfgOptions::default(), Env::default(), attrs);
    let committed = db.commit_with(|edit| {
        edit.set_crate_graph(graph);
    });
    (committed.crates[&a], committed)
}

#[test]
fn each_crate_attr_is_a_file_of_its_own_in_the_roots_source_root() {
    let mut db = test_db();
    let files = load(&mut db, 0, FILES);
    let (key, _) = set_attrs(&mut db, &files, &["no_std", "feature(x)"]);
    assert_eq!(attr_texts(&db, key), ["#![no_std]", "#![feature(x)]"]);
    let a = db.krate(key);
    let root = db.source_root(SourceRootId(0)).source_root(&db);
    for &file in a.crate_attr_files(&db) {
        assert!(!files.contains(&file), "{file:?} is a file of the VFS");
        assert_eq!(db.file_source_root(file).source_root_id(&db), SourceRootId(0));
        assert_eq!(root.path_for_file(&file), None, "no `mod` reaches {file:?}");
        assert_eq!(crate::crate_attr_owner(&db, file), Some(a));
    }
    let editioned = crate::crate_attr_files(&db, a);
    assert!(editioned.iter().all(|file| file.edition(&db) == Edition::Edition2021));
    assert_eq!(crate::applied_crate_attr_files(&db, a), editioned);
}

/// An equal graph set again keeps the attribute files and drops nothing: the arena of the crate
/// that holds them is kept.
#[test]
fn an_equal_graph_keeps_the_crate_attr_files_and_drops_nothing() {
    let mut db = test_db();
    let files = load(&mut db, 0, FILES);
    let (a, _) = set_attrs(&mut db, &files, &["no_std", "feature(x)"]);
    let before = attr_files(&db, a);
    let (again, committed) = set_attrs(&mut db, &files, &["no_std", "feature(x)"]);
    assert_eq!(again, a);
    assert_eq!(attr_files(&db, a), before);
    assert!(committed.dropped.is_empty(), "an equal graph dropped {:?}", committed.dropped);
}

#[test]
fn a_changed_crate_attr_reuses_a_file_and_a_gone_one_is_taken_out() {
    let mut db = test_db();
    let files = load(&mut db, 0, FILES);
    let (a, _) = set_attrs(&mut db, &files, &["no_std", "feature(x)"]);
    let first = attr_files(&db, a);

    // `feature(x)` keeps its file; `feature(y)` gets the one `no_std` left.
    assert_eq!(set_attrs(&mut db, &files, &["feature(x)", "feature(y)"]).0, a);
    assert_eq!(attr_files(&db, a), [first[1], first[0]]);
    assert_eq!(attr_texts(&db, a), ["#![feature(x)]", "#![feature(y)]"]);

    assert_eq!(set_attrs(&mut db, &files, &["feature(y)"]).0, a);
    assert_eq!(attr_files(&db, a), [first[0]]);
    assert!(!db.has_file_text(first[1]), "the file of a gone attribute is taken out");

    // The ID taken out is the next attribute's.
    assert_eq!(set_attrs(&mut db, &files, &["feature(y)", "no_core"]).0, a);
    assert_eq!(attr_files(&db, a), [first[0], first[1]]);
    assert_eq!(attr_texts(&db, a), ["#![feature(y)]", "#![no_core]"]);
}

#[test]
fn a_crate_that_goes_takes_its_crate_attr_files_out() {
    let mut db = test_db();
    let files = load(&mut db, 0, FILES);
    let (a, _) = set_attrs(&mut db, &files, &["no_std"]);
    let attr = attr_files(&db, a)[0];
    let mut graph = CrateGraphBuilder::default();
    let c = add(&mut graph, files[2], CfgOptions::default(), Env::default());
    let crates = set_graph(&mut db, graph);
    assert_ne!(crates[&c], a, "the crate that goes keeps no key in the new graph");
    assert!(!db.has_file_text(attr));
}

#[test]
#[should_panic(expected = "is the file of a crate attribute")]
fn the_vfs_setters_refuse_a_crate_attr_file() {
    let mut db = test_db();
    let files = load(&mut db, 0, FILES);
    let (a, _) = set_attrs(&mut db, &files, &["no_std"]);
    let attr = attr_files(&db, a)[0];
    db.commit_with(|edit| { edit.set_file_text(attr, "fn f() {}"); });
}

#[test]
fn a_new_file_id_meets_no_vfs_file_nor_crate_attr_file() {
    let mut db = test_db();
    let files = load(&mut db, 0, FILES);
    let given = new_file_id(&mut db);
    assert!(!files.contains(&given), "{given:?} is a file of the VFS");
    // An ID handed out stays taken: the next is another, whatever the commit between.
    let (a, _) = set_attrs(&mut db, &files, &["no_std", "feature(x)"]);
    let attrs = attr_files(&db, a);
    assert!(!attrs.contains(&given), "a crate attribute took {given:?}");
    let after = new_file_id(&mut db);
    assert!(after != given && !attrs.contains(&after) && !files.contains(&after));

    // A reload's new files, set after the graph, are not refused as an attribute's.
    db.commit_with(|edit| {
        edit.set_file_text(given, "fn f() {}").set_file_text(after, "fn g() {}");
    });
    assert_eq!(attr_texts(&db, a), ["#![no_std]", "#![feature(x)]"]);
}

/// rustc's rule for one crate attribute (`rustc_builtin_macros/src/cmdline_attrs.rs`): the
/// wrapped `#![<attr>]` balanced, one attribute, nothing after it. Ranges are in the wrapped
/// text, as rustc's columns are (`tests/ui/attributes/z-crate-attr/*.stderr`).
#[test]
fn a_crate_attr_is_one_attribute_or_rustcs_error() {
    use crate::CrateAttrError;
    let range = |start: u32, end: u32| TextRange::new(start.into(), end.into());
    let cases: &[(&str, Option<CrateAttrError>)] = &[
        ("no_std", None),
        ("feature(a, b)", None),
        ("doc = \"x\"", None),
        ("cfg_attr(unix, no_std)", None),
        // `garbage.stderr`: not an attribute.
        ("`%~@$#", Some(CrateAttrError::Syntax)),
        ("a b", Some(CrateAttrError::Syntax)),
        // `multiple.stderr`: "expected `]`, found `,`".
        ("feature(a),feature(b)", Some(CrateAttrError::Syntax)),
        ("a, b", Some(CrateAttrError::Syntax)),
        // `unbalanced-paren.stderr`: "mismatched closing delimiter: `]`".
        ("(", Some(CrateAttrError::MismatchedClose { open: range(3, 4), close: range(4, 5) })),
        (")", Some(CrateAttrError::MismatchedClose { open: range(2, 3), close: range(3, 4) })),
        ("a(", Some(CrateAttrError::MismatchedClose { open: range(4, 5), close: range(5, 6) })),
        ("a(]", Some(CrateAttrError::MismatchedClose { open: range(4, 5), close: range(5, 6) })),
        ("a]]", Some(CrateAttrError::UnexpectedClose { close: range(5, 6) })),
        ("a] ([", Some(CrateAttrError::Unclosed { open: range(6, 7) })),
        // `injection.stderr`, `injection2.stderr`: "unexpected token: `#`" after a written `]`.
        ("a] #![b", Some(CrateAttrError::UnexpectedToken { token: range(6, 7) })),
        ("feature(x)]#![no_std", Some(CrateAttrError::UnexpectedToken { token: range(14, 15) })),
        ("a b] #![c", Some(CrateAttrError::Syntax)),
        ("a = ", Some(CrateAttrError::Syntax)),
        ("\"a", Some(CrateAttrError::Syntax)),
    ];
    let mut db = test_db();
    let files = load(&mut db, 0, FILES);
    let mut attrs = Vec::new_in(Global);
    attrs.extend(cases.iter().map(|(attr, _)| *attr));
    let (a, _) = set_attrs(&mut db, &files, &attrs);
    let a = db.krate(a);
    let attr_files = crate::crate_attr_files(&db, a);
    let mut applied = Vec::new_in(Global);
    for (&(attr, want), &file) in cases.iter().zip(attr_files) {
        assert_eq!(crate::crate_attr_error(&db, file), want, "{attr:?}");
        if want.is_none() {
            applied.push(file);
        }
    }
    assert_eq!(crate::applied_crate_attr_files(&db, a), applied);
}

/// Source roots 0 (the checkout) and 1 (everything else), both empty.
fn checkout(db: &mut Db) -> (AbsPathBuf, FileSetConfig) {
    // A view is held in memory and its base is never read from disk: a path that is not there.
    let base = AbsPathBuf::assert_utf8(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("blaze-base-db-view"));
    let mut partition = FileSetConfig::builder();
    partition.add_file_set(vec![VfsPath::from(base.clone())]);
    let mut edit = db.edit();
    for root in [0, 1] {
        let empty = Arc::new(SourceRoot::new_local(FileSet::default()));
        edit.set_source_root(SourceRootId(root), empty);
    }
    _ = edit.commit();
    (base, partition.build())
}

/// Asserts `committed` dropped no arena: what an edit that changes nothing does.
#[track_caller]
pub(crate) fn assert_dropped_nothing(committed: &Committed, what: &str) {
    assert!(committed.dropped.is_empty(), "{what} dropped arenas {:?}", committed.dropped);
}

/// An alias of `of`, made and committed.
fn make_alias(db: &mut Db, of: FileId) -> Result<FileId, crate::AliasError> {
    db.edit_with(|edit| edit.alias(of)).0
}

/// [`Edit::set_alias_appended`], committed.
fn set_alias_appended(
    db: &mut Db,
    alias: FileId,
    appended: Option<(&'static str, &'static str)>,
) -> (Result<(), crate::AliasError>, Committed) {
    db.edit_with(|edit| edit.set_alias_appended(alias, appended))
}

/// A file ID no file has, taken and committed.
fn new_file_id(db: &mut Db) -> FileId {
    db.edit_with(|edit| edit.new_file_id()).0.expect("an unused file ID")
}

/// `bytes` in an allocation of their own, as blaze-vfs holds a heap file.
fn vfs_bytes(bytes: &[u8]) -> blaze_vfs::Bytes {
    blaze_vfs::Bytes::from(std::sync::Arc::<[u8]>::from(bytes))
}

fn view_file<'a>(path: &'a str, bytes: &'a blaze_vfs::Bytes) -> ViewFile<'a> {
    ViewFile { path, id: ContentId::of(blaze_vfs::FILE_DOMAIN, bytes), bytes }
}

fn set_view(
    db: &mut Db,
    base: &AbsPathBuf,
    partition: &FileSetConfig,
    files: &[(&str, &[u8])],
) -> Result<ViewChange, ViewError> {
    set_view_committed(db, base, partition, files).0
}

/// [`set_view`], with what its commit did.
fn set_view_committed(
    db: &mut Db,
    base: &AbsPathBuf,
    partition: &FileSetConfig,
    files: &[(&str, &[u8])],
) -> (Result<ViewChange, ViewError>, Committed) {
    let mut bytes = Vec::new_in(Global);
    bytes.extend(files.iter().map(|&(_, bytes)| vfs_bytes(bytes)));
    let mut views = Vec::new_in(Global);
    views.extend(files.iter().zip(&bytes).map(|(&(path, _), bytes)| view_file(path, bytes)));
    let view = ViewRoot { base, built: &[], partition, other: SourceRootId(1), files: &views };
    db.edit_with(|edit| edit.set_view_root(view))
}

fn file_at(db: &Db, base: &AbsPathBuf, path: &str) -> Option<FileId> {
    let root = db.source_root(SourceRootId(0)).source_root(db);
    root.file_for_path(&VfsPath::from(base.join(path))).copied()
}

const VIEW: &[(&str, &[u8])] =
    &[("src/lib.rs", b"mod a;"), ("src/a.rs", b"fn f() {}"), ("README.md", b"hello")];

#[test]
fn setting_a_view_twice_writes_nothing() {
    let mut db = test_db();
    let (base, partition) = checkout(&mut db);
    let first = set_view(&mut db, &base, &partition, VIEW).unwrap();
    assert_eq!(first.added.len(), 3);
    assert_eq!(first.roots, [SourceRootId(0)]);
    let a = file_at(&db, &base, "src/a.rs").unwrap();
    assert_eq!(&**db.file_text(a).text(&db), "fn f() {}");
    assert_eq!(db.file_source_root(a).source_root_id(&db), SourceRootId(0));

    let root = db.source_root(SourceRootId(0)).content(&db);
    let (second, committed) = set_view_committed(&mut db, &base, &partition, VIEW);
    assert_eq!(second.unwrap(), ViewChange::default());
    assert_dropped_nothing(&committed, "the same view again");
    assert_eq!(db.source_root(SourceRootId(0)).content(&db), root);
}

/// The invariant: a view read from a tree, whose IDs are taken without hashing, writes the
/// same inputs, file IDs and source roots as the same view checked against its bytes.
/// Control: the checked entry refuses a lying ID, so the two differ only by the hash.
#[test]
fn a_view_from_a_tree_writes_what_a_checked_view_writes() {
    let mut checked = test_db();
    let mut from_tree = test_db();
    let (base, partition) = checkout(&mut checked);
    checkout(&mut from_tree);
    let mut bytes = Vec::new_in(Global);
    bytes.extend(VIEW.iter().map(|&(_, bytes)| vfs_bytes(bytes)));
    let mut files = Vec::new_in(Global);
    files.extend(VIEW.iter().zip(&bytes).map(|(&(path, _), bytes)| view_file(path, bytes)));
    let view = ViewRoot {
        base: &base,
        built: &[],
        partition: &partition,
        other: SourceRootId(1),
        files: &files,
    };
    let a = checked.edit_with(|edit| edit.set_view_root(view)).0.unwrap();
    let b = from_tree.edit_with(|edit| edit.set_view_root_from_tree(view)).0.unwrap();
    assert_eq!(a, b);
    for &(path, text) in VIEW {
        let x = file_at(&checked, &base, path).unwrap();
        let y = file_at(&from_tree, &base, path).unwrap();
        assert_eq!(x, y, "{path}");
        assert_eq!(checked.file_text(x).text(&checked).as_bytes(), text, "{path}");
        assert_eq!(from_tree.file_text(y).text(&from_tree).as_bytes(), text, "{path}");
        assert_eq!(checked.file_content(x), from_tree.file_content(y), "{path}");
    }
    let (again, committed) = from_tree.edit_with(|edit| edit.set_view_root_from_tree(view));
    assert_eq!(again.unwrap(), ViewChange::default());
    assert_dropped_nothing(&committed, "the same view from a tree again");

    // Control: an ID that is not its bytes' is refused where IDs are checked (a new ID, so the
    // file is not skipped as unchanged before its bytes are hashed).
    let edited = vfs_bytes(b"fn g() {}");
    let other = ContentId::of(blaze_vfs::FILE_DOMAIN, b"fn h() {}");
    let lying = [files[0], ViewFile { path: "src/a.rs", id: other, bytes: &edited }, files[2]];
    let lie = ViewRoot { files: &lying, ..view };
    let refused = checked.edit_with(|edit| edit.set_view_root(lie)).0;
    assert!(matches!(refused, Err(ViewError::Mismatch { .. })));
}

#[test]
fn a_views_text_is_its_bytes() {
    let mut db = test_db();
    let (base, partition) = checkout(&mut db);
    let bytes = vfs_bytes(b"fn f() {}");
    let files = [view_file("src/lib.rs", &bytes)];
    let view = ViewRoot {
        base: &base,
        built: &[],
        partition: &partition,
        other: SourceRootId(1),
        files: &files,
    };
    db.edit_with(|edit| edit.set_view_root(view)).0.unwrap();
    let file = file_at(&db, &base, "src/lib.rs").unwrap();
    let text = db.file_text(file).text(&db);
    assert!(std::ptr::eq(text.as_ptr(), bytes.as_ptr()), "a copy of the view's bytes");
}

#[test]
fn a_view_with_one_changed_file_sets_that_one() {
    let mut db = test_db();
    let (base, partition) = checkout(&mut db);
    set_view(&mut db, &base, &partition, VIEW).unwrap();
    let a = file_at(&db, &base, "src/a.rs").unwrap();
    let root = db.source_root(SourceRootId(0)).content(&db);

    let edited: &[(&str, &[u8])] =
        &[("src/lib.rs", b"mod a;"), ("src/a.rs", b"fn g() {}"), ("README.md", b"hello")];
    let change = set_view(&mut db, &base, &partition, edited).unwrap();
    assert_eq!(change, ViewChange { set: vec![a], ..ViewChange::default() });
    assert_eq!(&**db.file_text(a).text(&db), "fn g() {}");
    assert_eq!(db.file_text(a).content(&db), FileText::content_of("fn g() {}", None));
    assert_ne!(db.source_root(SourceRootId(0)).content(&db), root);
}

#[test]
fn a_view_without_a_file_takes_it_out_of_its_source_root() {
    let mut db = test_db();
    let (base, partition) = checkout(&mut db);
    set_view(&mut db, &base, &partition, VIEW).unwrap();
    let readme = file_at(&db, &base, "README.md").unwrap();

    let change = set_view(&mut db, &base, &partition, &VIEW[..2]).unwrap();
    assert_eq!(change.removed, vec![readme]);
    assert_eq!(change.roots, vec![SourceRootId(0)]);
    assert_eq!(file_at(&db, &base, "README.md"), None);

    // Back again, with the file ID it had.
    let change = set_view(&mut db, &base, &partition, VIEW).unwrap();
    assert_eq!(change.added, vec![readme]);
    assert_eq!(file_at(&db, &base, "README.md"), Some(readme));
}

#[test]
fn bytes_that_are_not_utf8_are_a_file_without_text() {
    let mut db = test_db();
    let (base, partition) = checkout(&mut db);
    let binary: &[(&str, &[u8])] = &[("src/lib.rs", b"mod a;"), ("data.bin", &[0xff, 0xfe])];
    let change = set_view(&mut db, &base, &partition, binary).unwrap();
    let data = file_at(&db, &base, "data.bin").unwrap();
    assert_eq!(change.no_text, vec![data]);
    assert!(!db.has_file_text(data));

    let text: &[(&str, &[u8])] = &[("src/lib.rs", b"mod a;"), ("data.bin", b"now text")];
    let change = set_view(&mut db, &base, &partition, text).unwrap();
    assert_eq!(change.set, vec![data]);
    assert!(db.has_file_text(data));
    assert_eq!(&**db.file_text(data).text(&db), "now text");

    let change = set_view(&mut db, &base, &partition, binary).unwrap();
    assert_eq!(change.no_text, vec![data]);
    assert!(!db.has_file_text(data));
}

/// A file whose bytes are not UTF-8 has them as its input, set by the view loader alone: its
/// content in its source root ([`Db::file_content`], which an image's key, a crate's facts and
/// a macro's code hash) moves with its bytes and with nothing else, and is its text's once it
/// has text. The same bytes give the same content, however the file got them.
#[test]
fn the_bytes_of_a_file_without_text_are_its_content() {
    let mut db = test_db();
    let (base, partition) = checkout(&mut db);
    let root = |db: &Db| db.source_root(SourceRootId(0)).content(db);
    let first: &[(&str, &[u8])] = &[("src/lib.rs", b"mod a;"), ("data.bin", &[0xff, 0xfe])];
    set_view(&mut db, &base, &partition, first).unwrap();
    let data = file_at(&db, &base, "data.bin").unwrap();
    // The handle is the snapshot's: asked for again after each commit.
    let bytes = |db: &Db| -> Option<Vec<u8, Global>> {
        let raw = db.raw_file(data)?;
        let mut out = Vec::new_in(Global);
        out.extend_from_slice(&raw.bytes(db)?[..]);
        Some(out)
    };
    assert_eq!(bytes(&db).as_deref(), Some(&[0xff, 0xfe][..]), "the loader keeps the bytes");
    let (content, root_was) = (db.file_content(data), root(&db));
    assert_eq!(content, crate::IncludedFile::content_of(Ok(&[0xff, 0xfe][..])));
    let (again, committed) = set_view_committed(&mut db, &base, &partition, first);
    assert_eq!(again.unwrap(), ViewChange::default(), "the same bytes write nothing");
    assert_dropped_nothing(&committed, "the same bytes again");

    let edited: &[(&str, &[u8])] = &[("src/lib.rs", b"mod a;"), ("data.bin", &[0xff, 0xfd])];
    let change = set_view(&mut db, &base, &partition, edited).unwrap();
    assert_eq!((change.set, change.no_text), (vec![data], vec![data]));
    assert_eq!(bytes(&db).as_deref(), Some(&[0xff, 0xfd][..]));
    assert_ne!(db.file_content(data), content);
    assert_ne!(root(&db), root_was, "the source root's content moves with the bytes");

    let text: &[(&str, &[u8])] = &[("src/lib.rs", b"mod a;"), ("data.bin", b"now text")];
    set_view(&mut db, &base, &partition, text).unwrap();
    assert_eq!(bytes(&db), None, "a file with text has no bytes input");
    assert_eq!(db.file_content(data), db.file_text(data).content(&db));

    set_view(&mut db, &base, &partition, first).unwrap();
    assert_eq!(bytes(&db).as_deref(), Some(&[0xff, 0xfe][..]), "the input comes back");
    assert_eq!((db.file_content(data), root(&db)), (content, root_was));
}

#[test]
fn a_content_id_that_is_not_its_bytes_is_refused() {
    let mut db = test_db();
    let (base, partition) = checkout(&mut db);
    let (lib, main) = (vfs_bytes(b"mod a;"), vfs_bytes(b"fn main() {}"));
    let files = [
        view_file("src/lib.rs", &lib),
        ViewFile {
            path: "src/main.rs",
            id: ContentId::of(blaze_vfs::FILE_DOMAIN, b"other"),
            bytes: &main,
        },
    ];
    let view = ViewRoot {
        base: &base,
        built: &[],
        partition: &partition,
        other: SourceRootId(1),
        files: &files,
    };
    let (refused, committed) = db.edit_with(|edit| edit.set_view_root(view));
    let error = refused.unwrap_err();
    assert!(matches!(&error, ViewError::Mismatch { path, .. } if path == "src/main.rs"));
    assert_dropped_nothing(&committed, "a refused view");
    assert_eq!(file_at(&db, &base, "src/lib.rs"), None);
}

/// [`VIEW`] set, and an alias of its `src/lib.rs`: (base, partition, lib, alias).
fn aliased_view(db: &mut Db) -> (AbsPathBuf, FileSetConfig, FileId, FileId) {
    let (base, partition) = checkout(db);
    set_view(db, &base, &partition, VIEW).unwrap();
    let lib = file_at(db, &base, "src/lib.rs").unwrap();
    let alias = make_alias(db, lib).unwrap();
    (base, partition, lib, alias)
}

#[test]
fn an_alias_is_found_by_its_file_never_by_its_path() {
    let mut db = test_db();
    let (base, partition) = checkout(&mut db);
    set_view(&mut db, &base, &partition, VIEW).unwrap();
    let lib = file_at(&db, &base, "src/lib.rs").unwrap();
    let a = file_at(&db, &base, "src/a.rs").unwrap();
    let content = db.source_root(SourceRootId(0)).content(&db);
    let alias = make_alias(&mut db, lib).unwrap();
    assert_ne!(alias, lib);

    let root = db.source_root(SourceRootId(0)).source_root(&db);
    let path = VfsPath::from(base.join("src/lib.rs"));
    assert_eq!(root.path_for_file(&alias), Some(&path));
    assert_eq!(root.file_for_path(&path), Some(&lib), "a path has one answer, the disk file");
    assert!(!root.iter().any(|it| it == alias), "an alias is not among the root's files");
    assert_eq!(root.aliases().collect::<Vec<_>>(), vec![(alias, lib)]);
    let from_alias = AnchoredPath { anchor: alias, path: "a.rs" };
    assert_eq!(root.resolve_path(from_alias), Some(a));
    assert_eq!(db.resolve_path(from_alias), Some(a));
    assert_eq!(file_at(&db, &base, "src/lib.rs"), Some(lib));
    assert_eq!(
        db.source_root(SourceRootId(0)).content(&db),
        content,
        "the root's files are as they were"
    );

    assert_eq!(db.file_source_root(alias).source_root_id(&db), SourceRootId(0));
    assert_eq!(&**db.file_text(alias).text(&db), "mod a;");
    assert_eq!(db.file_text(alias).content(&db), db.file_text(lib).content(&db));
    assert_eq!(db.files().aliases_of(lib).as_slice(), &[alias]);
    assert_eq!(db.files().alias_of(alias), Some(lib));
    assert_eq!(db.files().alias_of(lib), None);
}

#[test]
fn a_view_set_again_keeps_every_alias() {
    let mut db = test_db();
    let (base, partition, lib, alias) = aliased_view(&mut db);
    let path = VfsPath::from(base.join("src/lib.rs"));
    // A view that adds a file rebuilds nothing of the alias, and one the same again writes nothing.
    let more: &[(&str, &[u8])] = &[
        ("src/lib.rs", b"mod a;"),
        ("src/a.rs", b"fn f() {}"),
        ("README.md", b"hello"),
        ("src/b.rs", b""),
    ];
    for view in [VIEW, more, VIEW, VIEW] {
        let change = set_view(&mut db, &base, &partition, view).unwrap();
        assert!(!change.removed.contains(&alias), "{change:?}");
        let root = db.source_root(SourceRootId(0)).source_root(&db);
        assert_eq!(root.path_for_file(&alias), Some(&path));
        assert_eq!(root.file_for_path(&path), Some(&lib));
        assert_eq!(&**db.file_text(alias).text(&db), "mod a;");
    }
    let (again, committed) = set_view_committed(&mut db, &base, &partition, VIEW);
    assert_eq!(again.unwrap(), ViewChange::default());
    assert_dropped_nothing(&committed, "the same view again");
}

#[test]
fn an_edit_of_a_root_reaches_every_alias_of_it() {
    let mut db = test_db();
    let (base, partition, lib, first) = aliased_view(&mut db);
    let second = make_alias(&mut db, lib).unwrap();
    assert_eq!(db.files().aliases_of(lib).as_slice(), &[first, second]);
    set_alias_appended(&mut db, second, Some(("__harness", "\nmod __harness {}"))).0.unwrap();

    let edited: &[(&str, &[u8])] =
        &[("src/lib.rs", b"mod a; mod b;"), ("src/a.rs", b"fn f() {}"), ("README.md", b"hello")];
    let change = set_view(&mut db, &base, &partition, edited).unwrap();
    assert_eq!(change.set, vec![lib, first, second], "the aliases are written in the same change");
    assert_eq!(&**db.file_text(first).text(&db), "mod a; mod b;");
    assert_eq!(db.file_text(first).content(&db), FileText::content_of("mod a; mod b;", None));
    let appended = "mod a; mod b;\nmod __harness {}";
    assert_eq!(
        &**db.file_text(second).text(&db),
        appended,
        "its suffix is kept after the new text"
    );
    assert_eq!(db.file_text(second).appended_root_module(&db), Some("__harness"));
    assert_eq!(
        db.file_text(second).content(&db),
        FileText::content_of(appended, Some("__harness"))
    );
    assert_eq!(&**db.file_text(lib).text(&db), "mod a; mod b;");
    assert_eq!(db.file_text(lib).appended_root_module(&db), None);

    db.commit_with(|edit| {
        edit.set_file_text(lib, "mod c;");
    });
    assert_eq!(&**db.file_text(first).text(&db), "mod c;");
    assert_eq!(&**db.file_text(second).text(&db), "mod c;\nmod __harness {}");
    assert_eq!(db.file_text(second).appended_root_module(&db), Some("__harness"));
}

#[test]
fn an_alias_owns_its_suffix() {
    let mut db = test_db();
    let (_, _, lib, alias) = aliased_view(&mut db);
    let shared = db.file_text(lib).text(&db).as_ptr();
    assert!(
        std::ptr::eq(db.file_text(alias).text(&db).as_ptr(), shared),
        "with no suffix it shares the bytes"
    );

    set_alias_appended(&mut db, alias, Some(("__list", "\nmod __list {}"))).0.unwrap();
    assert_eq!(&**db.file_text(alias).text(&db), "mod a;\nmod __list {}");
    assert_eq!(db.file_text(alias).appended_root_module(&db), Some("__list"));
    assert_eq!(&**db.file_text(lib).text(&db), "mod a;", "the file is as on disk");
    let (again, committed) = set_alias_appended(&mut db, alias, Some(("__list", "\nmod __list {}")));
    again.unwrap();
    assert_dropped_nothing(&committed, "the same suffix again");

    set_alias_appended(&mut db, alias, None).0.unwrap();
    assert_eq!(&**db.file_text(alias).text(&db), "mod a;");
    assert_eq!(db.file_text(alias).appended_root_module(&db), None);
    assert_eq!(db.file_text(alias).content(&db), db.file_text(lib).content(&db));
    assert_eq!(set_alias_appended(&mut db, lib, None).0, Err(crate::AliasError::NotAnAlias(lib)));
}

#[test]
#[should_panic = "set_alias_appended"]
fn an_aliass_module_is_set_with_its_suffix() {
    let mut db = test_db();
    let (_, _, _, alias) = aliased_view(&mut db);
    db.commit_with(|edit| {
        edit.set_appended_root_module(alias, Some("__harness"));
    });
}

#[test]
fn a_file_gone_takes_its_aliases_with_it() {
    let mut db = test_db();
    let (base, partition, lib, alias) = aliased_view(&mut db);
    let change = set_view(&mut db, &base, &partition, &VIEW[1..]).unwrap();
    let mut removed = [lib, alias];
    removed.sort();
    assert_eq!(change.removed, removed);
    assert!(!db.has_file_text(alias));
    let root = db.source_root(SourceRootId(0)).source_root(&db);
    assert_eq!(root.path_for_file(&alias), None);
    assert!(db.files().aliases_of(lib).is_empty());
    assert_eq!(db.files().alias_of(alias), None);

    // Back, the file has its ID and no alias: a new alias is a new file.
    set_view(&mut db, &base, &partition, VIEW).unwrap();
    assert_eq!(file_at(&db, &base, "src/lib.rs"), Some(lib));
    assert!(db.files().aliases_of(lib).is_empty());
    let again = make_alias(&mut db, lib).unwrap();
    assert_ne!(
        again, alias,
        "a gone alias's ID keeps its parked text, so it is not handed out again"
    );
}

#[test]
fn a_root_set_whole_keeps_its_aliases() {
    let mut db = test_db();
    let files = load(&mut db, 0, FILES);
    let alias = make_alias(&mut db, files[0]).unwrap();
    // `load` again sets source root 0 to a set made from scratch.
    load(&mut db, 0, FILES);
    let root = db.source_root(SourceRootId(0)).source_root(&db);
    assert_eq!(root.path_for_file(&alias), root.path_for_file(&files[0]));
    assert_eq!(root.resolve_path(AnchoredPath { anchor: alias, path: "b.rs" }), Some(files[1]));
    assert_eq!(db.files().aliases_of(files[0]).as_slice(), &[alias]);
}

#[test]
fn an_alias_is_of_a_file_with_a_path() {
    let mut db = test_db();
    let (_, _, lib, of) = aliased_view(&mut db);
    assert_eq!(make_alias(&mut db, of), Err(crate::AliasError::OfAnAlias(of)));
    let nowhere = new_file_id(&mut db);
    assert_eq!(make_alias(&mut db, nowhere), Err(crate::AliasError::NotInASourceRoot(nowhere)));
    assert_eq!(db.files().aliases_of(lib).as_slice(), &[of]);
}

#[test]
#[should_panic = "is an alias of"]
fn an_aliass_text_is_its_files() {
    let mut db = test_db();
    let (_, _, _, alias) = aliased_view(&mut db);
    db.commit_with(|edit| {
        edit.set_file_text(alias, "fn other() {}");
    });
}

#[test]
fn a_table_finds_what_a_hash_map_does() {
    use crate::table::Table;
    use blaze_hash::HashMap;
    use std::alloc::Global;

    let table: Table<Box<str>> = Table::new(1 << 16);
    let mut model: HashMap<String, u32, Global> = HashMap::new_in(Global);
    let mut order = Vec::new();
    let mut rng = Rng::new(7);
    for _ in 0..20_000 {
        let key = format!("key {}", rng.below(5_000));
        let hash = table.hash(key.as_str());
        let (at, inserted) = table.insert(hash, |it| **it == *key, || key.as_str().into());
        match model.get(&key) {
            Some(&known) => assert_eq!((at, inserted), (known, false), "{key}"),
            None => {
                assert!(inserted, "{key}");
                model.insert(key.clone(), at);
                order.push(key.clone());
            }
        }
        assert_eq!(&**table.get(at), key.as_str());
    }
    for (key, &at) in &model {
        assert_eq!(table.find(table.hash(key.as_str()), |it| **it == **key), Some(at));
    }
    assert_eq!(table.find(table.hash("absent"), |it| &**it == "absent"), None);
    let kept: Vec<&str> = table.iter().map(|it| &**it).collect();
    assert_eq!(kept, order.iter().map(String::as_str).collect::<Vec<_>>());
}

#[test]
fn threads_inserting_one_key_get_one_value() {
    use crate::table::Table;

    let table: Table<u64> = Table::new(1 << 16);
    let found: Vec<Vec<u32>> = std::thread::scope(|scope| {
        let threads: Vec<_> = (0..8)
            .map(|thread| {
                let table = &table;
                scope.spawn(move || {
                    let mut rng = Rng::new(thread);
                    let mut at = vec![u32::MAX; 2_000];
                    for _ in 0..10_000 {
                        let key = rng.below(2_000);
                        let (got, _) = table.insert(table.hash(key), |it| *it == key, || key);
                        assert!(at[key as usize] == u32::MAX || at[key as usize] == got);
                        at[key as usize] = got;
                    }
                    at
                })
            })
            .collect();
        threads.into_iter().map(|it| it.join().unwrap()).collect()
    });
    assert!(table.iter().count() <= 2_000);
    let mut seen = std::collections::BTreeSet::new();
    for value in table.iter() {
        assert!(seen.insert(*value), "{value} kept twice");
    }
    for key in 0..2_000u64 {
        let at = table.find(table.hash(key), |it| *it == key);
        for thread in &found {
            if thread[key as usize] != u32::MAX {
                assert_eq!(Some(thread[key as usize]), at);
            }
        }
    }
}

#[test]
fn threads_pushing_to_a_log_each_get_their_own_index() {
    use crate::table::Log;

    let log: Log<u32> = Log::new(1 << 16);
    let pushed: Vec<Vec<(u32, u32)>> = std::thread::scope(|scope| {
        let threads: Vec<_> = (0..4u32)
            .map(|thread| {
                let log = &log;
                scope.spawn(move || {
                    (0..10_000u32)
                        .map(|at| {
                            let value = thread * 10_000 + at;
                            (log.push(value), value)
                        })
                        .collect()
                })
            })
            .collect();
        threads.into_iter().map(|it| it.join().unwrap()).collect()
    });
    let mut by_index = vec![u32::MAX; 40_000];
    for (index, value) in pushed.into_iter().flatten() {
        assert_eq!(log.get(index), Some(&value));
        assert_eq!(by_index[index as usize], u32::MAX, "index {index} handed out twice");
        by_index[index as usize] = value;
    }
    assert!(log.iter().copied().eq(by_index.iter().copied()), "`iter` is in index order");
    assert_eq!(log.get(40_000), None);
}

#[test]
fn a_log_gives_back_what_was_put() {
    use crate::table::Log;

    let log: Log<u32> = Log::new(1 << 16);
    for value in 0..5_000u32 {
        assert_eq!(log.push(value * 3), value);
    }
    for value in 0..5_000u32 {
        assert_eq!(log.get(value), Some(&(value * 3)));
    }
    assert_eq!(log.get(5_000), None);
    assert!(log.iter().copied().eq((0..5_000).map(|it| it * 3)));
}

#[test]
fn what_queries_note_is_kept_once_and_sorted() {
    let mut db = test_db();
    let files = load(&mut db, 0, FILES);
    let mut graph = CrateGraphBuilder::default();
    let a = add(&mut graph, files[0], CfgOptions::default(), Env::default());
    let c = add(&mut graph, files[2], CfgOptions::default(), Env::default());
    let crates = set_graph(&mut db, graph);
    let (a, c) = (crates[&a], crates[&c]);
    let (crate_a, crate_c) = (db.krate(a), db.krate(c));
    for (krate, key) in [(crate_c, "B"), (crate_a, "B"), (crate_a, "A"), (crate_c, "B"), (crate_a, "B")] {
        db.env_is_read(krate, key);
    }
    db.env_is_unset(crate_a, "X");
    db.env_is_unset(crate_a, "X");
    // What was noted is kept by crate key: it outlives the arena of the crate that read it.
    let mut expected = Vec::new_in(Global);
    expected.extend([(a, "A".to_owned()), (a, "B".to_owned()), (c, "B".to_owned())]);
    expected.sort();
    assert_eq!(db.files().env_read(), expected);
    assert_eq!(db.files().env_unset(), [(a, "X".to_owned())]);

    let held = blaze_core::temp::TempDir::for_test("blaze-base-db-included-");
    let dir = AbsPathBuf::assert_utf8(held.path().to_path_buf());
    let (x, y) = (dir.join("blaze-base-db-x.rs"), dir.join("blaze-base-db-y.rs"));
    for (path, krate) in [(&y, a), (&x, c), (&y, a), (&y, c)] {
        db.files().include_source(path, SourceRootId(0), krate);
    }
    assert_eq!(
        db.files().included_sources(),
        [(x.clone(), SourceRootId(0)), (y.clone(), SourceRootId(0))]
    );
    let mut named = [(a, y.clone()), (c, x.clone()), (c, y.clone())];
    named.sort();
    assert_eq!(db.files().sources_included_by(), named);
}

/// A file `include_bytes!` names that can't be read keeps why, in `std::io::Error`'s words
/// (rustc's "couldn't read" message prints them), and a different error is a different input.
#[test]
fn an_unreadable_included_file_keeps_its_error() {
    let mut db = test_db();
    let files = load(&mut db, 0, FILES);
    let mut graph = CrateGraphBuilder::default();
    let a = add(&mut graph, files[0], CfgOptions::default(), Env::default());
    let a = set_graph(&mut db, graph)[&a];
    let held = blaze_core::temp::TempDir::for_test("blaze-base-db-unreadable-");
    let dir = AbsPathBuf::assert_utf8(held.path().to_path_buf());
    let missing = dir.join("blaze-base-db-unreadable-include.rs");
    let read = |path: &AbsPathBuf| {
        let disk = db.files().disk().expect("a test database reads the disk");
        let file = db.files().included_file(disk, &db, a, path);
        (file.read(&db).clone(), file.content(&db))
    };
    let expected = |path: &AbsPathBuf| std::fs::read(path).unwrap_err().to_string();
    let (missing_read, missing_content) = read(&missing);
    let (dir_read, dir_content) = read(&dir);
    assert_eq!(missing_read.unwrap_err().to_string(), expected(&missing));
    assert_eq!(dir_read.unwrap_err().to_string(), expected(&dir));
    assert_ne!(missing_content, dir_content, "a different error is a different input");
    let disk = db.files().disk().expect("a test database reads the disk");
    assert!(db.files().included_is_read(disk, &db, &missing, &Looks::none()), "the same error is what was read");
}

/// The invariant: a read of an included file is listed under its own kind
/// ([`crate::INCLUDED_READ`]), not the wider kind of every read around the inputs, in a
/// database that keeps read lists ([`Db::for_tests_with_reads`]); one that keeps none
/// ([`Db::for_tests`]) refuses to record.
#[test]
fn an_included_files_read_is_listed_under_its_own_kind() {
    let held = blaze_core::temp::TempDir::for_test("blaze-base-db-included-kind-");
    let dir = AbsPathBuf::assert_utf8(held.path().to_path_buf());
    let path = dir.join("blaze-base-db-included-kind.rs");
    std::fs::write(&path, "fn included() {}\n").expect("a file in the test's own directory");

    let mut db = Db::for_tests_with_reads(no_boundaries);
    let files = load(&mut db, 0, FILES);
    let mut graph = CrateGraphBuilder::default();
    let a = add(&mut graph, files[0], CfgOptions::default(), Env::default());
    let a = set_graph(&mut db, graph)[&a];
    let disk = db.files().disk().expect("a test database reads the disk");
    let file = db.files().included_file(disk, &db, a, &path);
    let ((), reads) = blaze_memo::record(blaze_memo::Database::memo(&db), || {
        _ = file.content(&db);
    })
    .unwrap_or_else(|off| panic!("a database made to keep read lists: {off}"));
    let kinds = |kind: blaze_memo::KindId| {
        reads.iter().filter(|read| matches!(read, blaze_memo::Read::Input { kind: it, .. } if *it == kind)).count()
    };
    assert_eq!(kinds(crate::INCLUDED_READ), 1, "{reads:?}");
    assert_eq!(kinds(blaze_memo::KindId::of(&blaze_memo::AROUND_INPUTS)), 0, "{reads:?}");

    // The control: the same read in a database that keeps no read lists is not recorded.
    let mut off = test_db();
    let files = load(&mut off, 0, FILES);
    let mut graph = CrateGraphBuilder::default();
    let a = add(&mut graph, files[0], CfgOptions::default(), Env::default());
    let a = set_graph(&mut off, graph)[&a];
    let disk = off.files().disk().expect("a test database reads the disk");
    let file = off.files().included_file(disk, &off, a, &path);
    assert!(blaze_memo::record(blaze_memo::Database::memo(&off), || _ = file.content(&off)).is_err());
}

/// The view a seed makes at a step: some of a fixed set of paths, each with one of a few
/// contents, some of which are not UTF-8.
fn seeded_view(rng: &mut Rng) -> Vec<(String, Vec<u8>)> {
    const CONTENTS: &[&[u8]] = &[b"fn a() {}", b"fn b() {}", b"mod c;", &[0xff, 0x00], &[0xfe]];
    let mut view = Vec::new();
    for at in 0..16 {
        if rng.below(10) < 7 {
            let path = match at % 4 {
                0 => format!("src/m{at}.rs"),
                1 => format!("src/deep/n{at}.rs"),
                2 => format!("data/b{at}.bin"),
                _ => format!("target/out{at}.rs"),
            };
            view.push((path, CONTENTS[rng.below(CONTENTS.len() as u64) as usize].to_vec()));
        }
    }
    view
}

/// Everything a view's files are as inputs, by path: present, has text, its text ID and
/// source root; then each source root's content ID.
fn view_state(
    db: &Db,
    base: &AbsPathBuf,
    paths: &[String],
) -> (Vec<Option<(bool, Option<ContentId>, SourceRootId)>>, [ContentId; 2]) {
    let files = paths
        .iter()
        .map(|path| {
            let id = file_at(db, base, path)?;
            let text = db.has_file_text(id);
            let content = text.then(|| db.file_text(id).content(db));
            Some((text, content, db.file_source_root(id).source_root_id(db)))
        })
        .collect();
    let roots = [0, 1].map(|root| db.source_root(SourceRootId(root)).content(db));
    (files, roots)
}

#[test]
fn a_view_set_over_others_is_the_view_set_alone() {
    let mut rng = Rng::new(3);
    let mut db = test_db();
    let (base, partition) = checkout(&mut db);
    let built = [base.join("target")];
    let mut every_path: Vec<String> = Vec::new();
    let mut before: Vec<(String, Vec<u8>)> = Vec::new();
    for step in 0..40 {
        let view = seeded_view(&mut rng);
        let id_of = |db: &Db, path: &str| file_at(db, &base, path);
        let ids_before: Vec<(String, Option<FileId>)> =
            before.iter().map(|(path, _)| (path.clone(), id_of(&db, path))).collect();
        let bytes: Vec<blaze_vfs::Bytes> = view.iter().map(|(_, bytes)| vfs_bytes(bytes)).collect();
        let files: Vec<ViewFile<'_>> =
            view.iter().zip(&bytes).map(|((path, _), bytes)| view_file(path, bytes)).collect();
        let root = ViewRoot {
            base: &base,
            built: &built,
            partition: &partition,
            other: SourceRootId(1),
            files: &files,
        };
        let change = db.edit_with(|edit| edit.set_view_root(root)).0.unwrap();

        // What the change says, from the two views alone. A file under `target` is built: one
        // the view lacks stays; one it has is set like any other.
        let old: std::collections::BTreeMap<&str, &[u8]> =
            before.iter().map(|(path, bytes)| (path.as_str(), bytes.as_slice())).collect();
        let new: std::collections::BTreeMap<&str, &[u8]> =
            view.iter().map(|(path, bytes)| (path.as_str(), bytes.as_slice())).collect();
        let in_root: std::collections::BTreeSet<&str> = old
            .keys()
            .copied()
            .filter(|path| ids_before.iter().any(|(known, id)| known == path && id.is_some()))
            .collect();
        let mut set = Vec::new();
        let mut added = Vec::new();
        let mut no_text = Vec::new();
        for (&path, &bytes) in &new {
            let id = id_of(&db, path).unwrap();
            let written = match old.get(path) {
                Some(&was) if in_root.contains(path) => {
                    // Bytes that are not text are an input too: a change of them is a set.
                    if was != bytes {
                        set.push(id);
                    }
                    was != bytes
                }
                _ => {
                    added.push(id);
                    true
                }
            };
            if written && std::str::from_utf8(bytes).is_err() {
                no_text.push(id);
            }
        }
        let mut removed: Vec<FileId> = ids_before
            .iter()
            .filter(|(path, _)| !new.contains_key(path.as_str()) && !path.starts_with("target/"))
            .filter_map(|(_, id)| *id)
            .collect();
        for list in [&mut set, &mut added, &mut no_text, &mut removed] {
            list.sort_unstable();
        }
        assert_eq!(
            (change.set, change.added, change.no_text, change.removed),
            (set, added, no_text, removed),
            "step {step}"
        );

        // Built files the view lacks stay; take them into the view for the comparison.
        let mut whole = view.clone();
        for (path, bytes) in &before {
            if path.starts_with("target/") && !new.contains_key(path.as_str()) {
                whole.push((path.clone(), bytes.clone()));
            }
        }
        for (path, _) in &whole {
            if !every_path.contains(path) {
                every_path.push(path.clone());
            }
        }
        let mut fresh = test_db();
        let (fresh_base, fresh_partition) = checkout(&mut fresh);
        let fresh_files: Vec<(&str, &[u8])> =
            whole.iter().map(|(path, bytes)| (path.as_str(), bytes.as_slice())).collect();
        set_view(&mut fresh, &fresh_base, &fresh_partition, &fresh_files).unwrap();
        assert_eq!(
            view_state(&db, &base, &every_path),
            view_state(&fresh, &fresh_base, &every_path),
            "step {step}"
        );
        before = whole;
    }
}

fn two_workers() -> blaze_graph::pool::Pool {
    blaze_graph::pool::Pool::new(&blaze_graph::topology::Topology::flat(2), false).unwrap()
}

#[test]
fn a_task_that_gave_way_runs_again() {
    let db = Db::new(no_boundaries, blaze_memo::Reads::Off);
    let pool = two_workers();
    let mut runs = Vec::new_in(Global);
    runs.extend((0..8).map(|_| AtomicUsize::new(0)));
    pool.install(|| {
        db.run_all(blaze_mem::Tag::Parse, runs.len(), |_: &Db, at: usize| {
            if runs[at].fetch_add(1, Ordering::Relaxed) == 0 && at % 3 == 0 {
                blaze_memo::Cancelled::GaveWay.throw();
            }
        });
    });
    let mut counts = Vec::new_in(Global);
    counts.extend(runs.iter().map(|it| it.load(Ordering::Relaxed)));
    assert_eq!(counts, [2, 1, 1, 2, 1, 1, 2, 1]);
}

#[test]
fn any_other_panic_of_a_task_reaches_the_caller() {
    let db = Db::new(no_boundaries, blaze_memo::Reads::Off);
    let pool = two_workers();
    let ran = AtomicUsize::new(0);
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        pool.install(|| {
            db.run_all(blaze_mem::Tag::Parse, 4, |_: &Db, at: usize| {
                ran.fetch_add(1, Ordering::Relaxed);
                if at == 2 {
                    std::panic::resume_unwind(Box::new("task 2 failed"));
                }
            });
        });
    }));
    assert_eq!(caught.unwrap_err().downcast_ref::<&str>(), Some(&"task 2 failed"));
    assert_eq!(ran.load(Ordering::Relaxed), 4);
}

#[test]
fn a_private_dependency_reads_back_private_and_changes_the_id() {
    let crate_id = |private: bool| {
        let mut db = test_db();
        let files = load(&mut db, 0, FILES);
        let mut graph = CrateGraphBuilder::default();
        let a = add(&mut graph, files[0], CfgOptions::default(), Env::default());
        let c = add(&mut graph, files[2], CfgOptions::default(), Env::default());
        let dependency =
            DependencyBuilder::new(CrateName::new("c").unwrap(), c).with_private(private);
        graph.add_dep(a, dependency).unwrap();
        let key = set_graph(&mut db, graph)[&a];
        let a = db.krate(key);
        assert_eq!(a.data(&db).dependencies[0].is_private(), private);
        a.content(&db)
    };
    let public = crate_id(false);
    assert_ne!(crate_id(true), public);
    assert_eq!(crate_id(false), public);

    let mut db = test_db();
    let files = load(&mut db, 0, FILES);
    let mut graph = CrateGraphBuilder::default();
    let a = add(&mut graph, files[0], CfgOptions::default(), Env::default());
    let c = add(&mut graph, files[2], CfgOptions::default(), Env::default());
    let crates = set_graph(&mut db, graph);
    let (a, c): (CrateKey, CrateKey) = (crates[&a], crates[&c]);
    let depend_on = |db: &mut Db, private: bool| {
        let mut data = db.krate(a).data(&*db).keyed(&*db);
        let name = CrateName::new("c").unwrap();
        let mut dependencies = Vec::new_in(Global);
        dependencies.push(keyed_dep(name, c, true, false).with_private(private));
        data.dependencies = dependencies;
        db.commit_with(|edit| {
            edit.set_crate_data(a, data);
        });
        let krate = db.krate(a);
        (krate.data(&*db).dependencies[0].is_private(), krate.content(&*db), all_crates_content(&*db))
    };
    let (read, public, graph_public) = depend_on(&mut db, false);
    assert!(!read);
    let (read, private, graph_private) = depend_on(&mut db, true);
    assert!(read);
    assert_ne!(private, public);
    assert_ne!(graph_private, graph_public);
    assert_eq!(depend_on(&mut db, false), (false, public, graph_public));
}

fn arm_target(tuple: &str, has_thumb_interworking: bool) -> TargetData {
    TargetData {
        tuple: Symbol::intern(tuple),
        data_layout: "e-m:e-p:32:32-Fi8-i64:64-v128:64:128-a:0:32-n32-S64".into(),
        arch: Arch::Other,
        has_thumb_interworking,
    }
}

#[test]
fn targets_differing_in_the_tuple_or_thumb_interworking_have_different_ids() {
    let interworking = arm_target("armv7-unknown-linux-gnueabihf", true);
    let without = arm_target("armv7-unknown-linux-gnueabihf", false);
    let other_tuple = arm_target("armv7-unknown-linux-musleabihf", true);
    assert_ne!(interworking.content(), without.content());
    assert_ne!(interworking.content(), other_tuple.content());
    assert_eq!(interworking.content(), arm_target("armv7-unknown-linux-gnueabihf", true).content());

    let workspace = |target: TargetData| {
        CrateWorkspaceData {
            target: Ok(target),
            toolchain: None,
            const_eval: Default::default(),
            trusted: Box::default(),
        }
            .content()
    };
    assert_ne!(workspace(interworking.clone()), workspace(without.clone()));

    let crate_id = |target: TargetData| {
        let mut db = test_db();
        let files = load(&mut db, 0, FILES);
        let mut graph = CrateGraphBuilder::default();
        let a = add(&mut graph, files[0], CfgOptions::default(), Env::default());
        let a = set_graph(&mut db, graph)[&a];
        let workspace = CrateWorkspaceData {
            target: Ok(target),
            toolchain: None,
            const_eval: Default::default(),
            trusted: Box::default(),
        };
        db.commit_with(|edit| {
            edit.set_crate_workspace_data(a, Arc::new(workspace));
        });
        db.krate(a).content(&db)
    };
    assert_ne!(crate_id(interworking), crate_id(without));
}

/// Puts `files` (ID, path, text) in source root `root`, and commits.
fn put_root(db: &mut Db, root: u32, files: &[(FileId, &str, &str)]) {
    let mut set = FileSet::default();
    let mut edit = db.edit();
    for &(id, path, text) in files {
        edit.set_file_text(id, text).set_file_source_root(id, SourceRootId(root));
        set.insert(id, VfsPath::new_virtual_path(path.to_owned()));
    }
    edit.set_source_root(SourceRootId(root), Arc::new(SourceRoot::new_local(set)));
    _ = edit.commit();
}

/// How many runs of the tracked fn `name` (the last segment of its path) `db`'s run log holds,
/// emptying it: the runs since the last call.
pub(crate) fn ran(db: &Db, name: &str) -> usize {
    let runs = db.runs().expect("a test database keeps a run log").take();
    runs.iter().filter(|it| it.query.rsplit("::").next() == Some(name)).count()
}

/// What a `#[path = "../../shared/m.rs"] mod m;` in `anchor` reads: the file and its text's
/// length, as a def map would.
#[blaze_memo::tracked(returns(copy))]
fn shared_module(db: &Db, anchor: EditionedFileId) -> Option<(u32, u32)> {
    let (anchor, _) = anchor.unpack(db);
    let file = db.resolve_path(AnchoredPath { anchor, path: "../../shared/m.rs" })?;
    Some((file.index(), db.file_text(file).text(db).len() as u32))
}

#[test]
fn a_path_module_outside_its_package_resolves_and_its_edit_reruns_the_reader() {
    let mut db = test_db();
    let (lib, own, m) = (FileId::from_raw(0), FileId::from_raw(1), FileId::from_raw(2));
    put_root(
        &mut db,
        0,
        &[
            (lib, "/ws/member/src/lib.rs", "#[path = \"../../shared/m.rs\"] mod m;"),
            (own, "/ws/member/src/a.rs", ""),
        ],
    );
    put_root(&mut db, 1, &[(m, "/ws/shared/m.rs", "pub fn f() {}")]);
    // The file's id is homed with its text: made again in each snapshot.
    let read = |db: &Db| shared_module(db, EditionedFileId::new(db, lib, Edition::Edition2021));

    // The anchor's own source root first, as before.
    assert_eq!(db.resolve_path(AnchoredPath { anchor: lib, path: "a.rs" }), Some(own));
    ran(&db, "shared_module");
    assert_eq!(read(&db), Some((m.index(), 13)));
    assert_eq!(read(&db), Some((m.index(), 13)));
    assert_eq!(ran(&db, "shared_module"), 1);

    // An edit of the file in the other source root runs the reader again.
    db.commit_with(|edit| {
        edit.set_file_text(m, "pub fn f() { 1 }");
    });
    assert_eq!(read(&db), Some((m.index(), 16)));
    assert_eq!(ran(&db, "shared_module"), 1);

    // Taken out of its source root: found in none, and nothing read from disk.
    put_root(&mut db, 1, &[]);
    assert_eq!(read(&db), None);
    assert_eq!(ran(&db, "shared_module"), 1);

    // Named again by another source root, as another file: the miss runs again and finds it.
    let moved = FileId::from_raw(3);
    put_root(&mut db, 2, &[(moved, "/ws/shared/m.rs", "pub fn g() {}")]);
    assert_eq!(read(&db), Some((moved.index(), 13)));
    assert_eq!(ran(&db, "shared_module"), 1);
}

/// What a `#[path = "../../shared/m.rs"] mod m;` in the root of `krate` reads when no source
/// root has that file: the file read from disk and its text's length, as a def map would.
#[blaze_memo::tracked(returns(copy))]
fn disk_module(db: &Db, krate: Crate) -> Option<(u32, u32)> {
    let anchor = krate.data(db).root_file_id;
    let path = AnchoredPath { anchor, path: "../../shared/m.rs" };
    let file = db.resolve_path(path).or_else(|| db.module_source(krate, path))?;
    Some((file.index(), db.file_text(file).text(db).len() as u32))
}

/// A module file outside every source root (crates/blaze-inproc-rt's `#[path]` into
/// library/) is read from disk as rustc reads it, noted as the crate's read, and its own
/// modules resolve beside it; an edit of it, or its removal, reruns the reader once the disk
/// is read again.
#[test]
fn a_module_file_outside_every_source_root_is_read_from_disk_and_its_edit_reruns_the_reader() {
    let held = blaze_core::temp::TempDir::for_test("blaze-base-db-module-");
    let dir = AbsPathBuf::assert_utf8(held.path().to_path_buf());
    let (src, shared) = (dir.join("member/src"), dir.join("shared"));
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(&shared).unwrap();
    let (m, n) = (shared.join("m.rs"), shared.join("n.rs"));
    std::fs::write(&m, "pub fn f() {}").unwrap();
    std::fs::write(&n, "").unwrap();

    let mut db = test_db();
    let lib = FileId::from_raw(0);
    let mut set = FileSet::default();
    set.insert(lib, VfsPath::from(src.join("lib.rs")));
    db.commit_with(|edit| {
        edit.set_file_text(lib, "")
            .set_file_source_root(lib, SourceRootId(0))
            .set_source_root(SourceRootId(0), Arc::new(SourceRoot::new_local(set)));
    });
    let mut graph = CrateGraphBuilder::default();
    let krate = add(&mut graph, lib, CfgOptions::default(), Env::default());
    let key = set_graph(&mut db, graph)[&krate];

    let path = AnchoredPath { anchor: lib, path: "../../shared/m.rs" };
    assert_eq!(db.resolve_path(path), None, "no source root has it");
    ran(&db, "disk_module");
    let krate = db.krate(key);
    let (file, len) = disk_module(&db, krate).unwrap();
    assert_eq!(len, 13);
    assert_eq!(disk_module(&db, krate), Some((file, len)));
    assert_eq!(ran(&db, "disk_module"), 1);
    let mut read = Vec::new_in(Global);
    read.extend(db.files().included_by(&db, key).into_iter().map(|it| it.0));
    assert!(read.contains(&m), "the crate's keys cover what it read: {read:?}");

    // Its own `mod n;` resolves beside it, from its own one-file source root.
    let file = FileId::from_raw(file);
    let nested = db.module_source(krate, AnchoredPath { anchor: file, path: "n.rs" }).unwrap();
    assert_eq!(db.path_for_file(nested), Some(&VfsPath::from(n.clone())));

    // An edit on disk, read again: the reader runs again and reads the new text.
    std::fs::write(&m, "pub fn f() { 1 }").unwrap();
    let (moved, _) = db.edit_with(|edit| edit.reload_included_files(&Looks::none()));
    assert!(moved.contains(&m));
    assert_eq!(disk_module(&db, db.krate(key)), Some((file.index(), 16)));
    assert_eq!(ran(&db, "disk_module"), 1);

    // Gone from disk: no module file, never the old text.
    std::fs::remove_file(&m).unwrap();
    let (moved, _) = db.edit_with(|edit| edit.reload_included_files(&Looks::none()));
    assert!(moved.contains(&m));
    assert_eq!(disk_module(&db, db.krate(key)), None);
    assert_eq!(ran(&db, "disk_module"), 1);
}

/// The invariant (rule 14): a database with no disk ([`Db::sealed`], what a command's snapshot
/// is) reads no file outside its source roots. The `#[path]` module a database with a disk
/// reads from disk (the control, the test above, on the same files) is no module file here,
/// an `include_bytes!` of it finds nothing, and no read is noted.
#[test]
fn a_database_with_no_disk_reads_no_module_file_from_disk() {
    let held = blaze_core::temp::TempDir::for_test("blaze-base-db-sealed-");
    let dir = AbsPathBuf::assert_utf8(held.path().to_path_buf());
    let (src, shared) = (dir.join("member/src"), dir.join("shared"));
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(&shared).unwrap();
    std::fs::write(shared.join("m.rs"), "pub fn f() {}").unwrap();

    let mut db = Db::for_tests_sealed(no_boundaries);
    assert!(db.files().disk().is_none(), "a sealed database holds no disk");
    let lib = FileId::from_raw(0);
    let mut set = FileSet::default();
    set.insert(lib, VfsPath::from(src.join("lib.rs")));
    db.commit_with(|edit| {
        edit.set_file_text(lib, "")
            .set_file_source_root(lib, SourceRootId(0))
            .set_source_root(SourceRootId(0), Arc::new(SourceRoot::new_local(set)));
    });
    let mut graph = CrateGraphBuilder::default();
    let krate = add(&mut graph, lib, CfgOptions::default(), Env::default());
    let key = set_graph(&mut db, graph)[&krate];
    let krate = db.krate(key);

    let path = AnchoredPath { anchor: lib, path: "../../shared/m.rs" };
    assert_eq!(disk_module(&db, krate), None, "no disk: the file on disk is no module file");
    assert!(db.included_file(krate, path).is_none(), "no disk: nothing an include reads");
    assert_eq!(db.files().included_paths().count(), 0, "no file outside the VFS was read");
    assert_eq!(db.files().included_by(&db, key), Vec::new(), "no read is noted as the crate's");
}

#[test]
fn a_path_no_source_root_names_is_found_once_one_does() {
    let mut db = test_db();
    let lib = FileId::from_raw(0);
    put_root(&mut db, 0, &[(lib, "/ws/member/src/lib.rs", "")]);
    let read = |db: &Db| shared_module(db, EditionedFileId::new(db, lib, Edition::Edition2021));
    assert_eq!(read(&db), None);
    ran(&db, "shared_module");

    // A source root that names the path: the miss runs again.
    let m = FileId::from_raw(1);
    put_root(&mut db, 1, &[(m, "/ws/shared/m.rs", "fn m() {}")]);
    assert_eq!(read(&db), Some((m.index(), 9)));
    assert_eq!(ran(&db, "shared_module"), 1);
}

#[test]
fn the_simd_prefix_and_suffix_equal_their_scalar_reference() {
    let mut rng = Rng::new(19);
    for _ in 0..4_000 {
        let common = rng.below(160) as usize;
        let mut a: Vec<u8> = (0..common).map(|_| rng.below(4) as u8).collect();
        let mut b = a.clone();
        a.extend((0..rng.below(80)).map(|_| rng.below(4) as u8));
        b.extend((0..rng.below(80)).map(|_| rng.below(4) as u8));
        let tail: Vec<u8> = (0..rng.below(160)).map(|_| rng.below(4) as u8).collect();
        a.extend(&tail);
        b.extend(&tail);
        assert_eq!(common_prefix(&a, &b), common_prefix_scalar(&a, &b), "{a:?} {b:?}");
        assert_eq!(common_suffix(&a, &b), common_suffix_scalar(&a, &b), "{a:?} {b:?}");
        assert_eq!(common_prefix(&a, &a), a.len());
        assert_eq!(common_suffix(&b, &b), b.len());
    }
}

/// A random edit of `text` at character boundaries: some characters taken out, a snippet put
/// in, or both.
fn random_edit(rng: &mut Rng, text: &str) -> String {
    const SNIPPETS: &[&str] = &[
        "",
        "x",
        " ",
        "\n",
        "{",
        "}",
        "(",
        ")",
        ";",
        "\"",
        "'a",
        "//",
        "/*",
        "*/",
        "é",
        "fn g() {}",
        "1 + 2",
        "struct S;",
        "let y = 0;",
        "mod m { }",
        "#[cfg(test)]",
    ];
    let boundaries: Vec<usize> = (0..=text.len()).filter(|&at| text.is_char_boundary(at)).collect();
    let start = boundaries[rng.below(boundaries.len() as u64) as usize];
    let end = boundaries
        .iter()
        .copied()
        .filter(|&at| at >= start)
        .nth(rng.below(9) as usize)
        .unwrap_or(text.len());
    let insert = SNIPPETS[rng.below(SNIPPETS.len() as u64) as usize];
    [&text[..start], insert, &text[end..]].concat()
}

#[test]
fn the_edit_between_two_texts_makes_one_the_other() {
    let mut rng = Rng::new(23);
    let mut old = include_str!("target.rs").to_owned();
    for _ in 0..500 {
        let new = random_edit(&mut rng, &old);
        let before = ContentId::of(blaze_vfs::FILE_DOMAIN, old.as_bytes());
        let edit = TextEdit::between(&old, &new, before);
        assert_eq!(edit.before, before);
        let (start, end) = (usize::from(edit.delete.start()), usize::from(edit.delete.end()));
        let insert = usize::from(edit.insert);
        assert!(old.is_char_boundary(start) && old.is_char_boundary(end));
        assert_eq!([&old[..start], &new[start..start + insert], &old[end..]].concat(), new);
        old = new;
    }
    let same =
        TextEdit::between("fn f() {}", "fn f() {}", ContentId::of(blaze_vfs::FILE_DOMAIN, b""));
    assert_eq!((same.delete.len(), same.insert), (TextSize::from(0), TextSize::from(0)));
}

fn reparsed() -> usize {
    REPARSED.with(Cell::get)
}

/// The property test of db's decision D8: over random edits of real files, in full and as an
/// outline, each parse is the one a whole parse of the text makes, and most are reparses.
#[test]
fn a_parse_after_random_edits_is_the_whole_parse() {
    for (seed, source, library) in [
        (29, include_str!("target.rs"), false),
        (31, include_str!("change.rs"), false),
        (37, include_str!("target.rs"), true),
    ] {
        let mut db = test_db();
        let file = FileId::from_raw(0);
        put_root(&mut db, 0, &[(file, "/a/lib.rs", source)]);
        if library {
            let mut set = FileSet::default();
            set.insert(file, VfsPath::new_virtual_path("/a/lib.rs".to_owned()));
            let root = Arc::new(SourceRoot::new_library(set));
            db.commit_with(|edit| {
                edit.set_source_root(SourceRootId(0), root);
            });
        }
        // Homed with the file's text: made again after each edit.
        let id = |db: &Db| EditionedFileId::new(db, file, Edition::Edition2021);
        assert_eq!(id(&db).parse(&db).is_outline(), library);
        let mut rng = Rng::new(seed);
        let mut text = source.to_owned();
        let start = reparsed();
        // An edit that gives the text the file has writes nothing, so nothing is parsed.
        let mut changed = 0;
        for step in 0..80 {
            let edited = random_edit(&mut rng, &text);
            changed += usize::from(edited != text);
            text = edited;
            db.commit_with(|edit| {
                edit.set_file_text(file, &text);
            });
            let parse = id(&db).parse(&db);
            assert_eq!(parse.root().text(), text, "step {step}");
            assert_whole(parse, db.file_text(file).text(&db), Edition::Edition2021, library);
        }
        assert!(changed >= 70, "seed {seed}: {changed} of 80 edits changed the text");
        assert_eq!(
            reparsed() - start,
            changed,
            "seed {seed}: every edit after a kept parse that changes the text reparses, and no other"
        );
    }
}

#[test]
fn an_edit_before_an_item_is_the_whole_parse() {
    // Each edit's one changed range starts at the first item's start, so trivia before it
    // (the file's) or a comment attached to it or not are in the item's new text.
    let edits: &[(&str, &str)] = &[
        // a reformat: blank lines before the item and its body indented again
        (
            "pub fn f(x: u32) -> u32 {\n    x + 1\n}\n",
            "\n\npub fn f(x: u32) -> u32 {\n        x + 1\n}\n",
        ),
        ("fn f() {}\n", "// c\nfn f() {}\n"),
        ("fn f() {}\n", "// c\n\nfn f() {}\n"),
        ("/// a\nfn f() {}\n", "/// a\n\nfn f() {}\n"),
        ("/// a\nfn f() {}\n", "/// ab\nfn f() {}\n"),
    ];
    for (i, &(before, after)) in edits.iter().enumerate() {
        let mut db = test_db();
        let file = FileId::from_raw(0);
        put_root(&mut db, 0, &[(file, "/a/lib.rs", before)]);
        let _ = EditionedFileId::new(&db, file, Edition::Edition2021).parse(&db);
        db.commit_with(|edit| {
            edit.set_file_text(file, after);
        });
        let parse = EditionedFileId::new(&db, file, Edition::Edition2021).parse(&db);
        assert_eq!(parse.root().text(), after, "edit {i}");
        assert_whole(parse, db.file_text(file).text(&db), Edition::Edition2021, false);
    }
}

#[test]
fn a_reparse_is_made_from_the_last_text_that_was_parsed() {
    let mut db = test_db();
    let file = FileId::from_raw(0);
    put_root(&mut db, 0, &[(file, "/a/lib.rs", "fn f() { 1 }\nstruct S;\n")]);
    let id = |db: &Db, edition| EditionedFileId::new(db, file, edition);
    let _ = id(&db, Edition::Edition2021).parse(&db);
    let parsed = ContentId::of(blaze_vfs::FILE_DOMAIN, b"fn f() { 1 }\nstruct S;\n");
    let set_text = |db: &mut Db, file: FileId, text: &str| {
        db.commit_with(|edit| {
            edit.set_file_text(file, text);
        });
    };

    // Two edits with no parse between: the second is one edit from the text last parsed.
    set_text(&mut db, file, "fn f() { 1 + 2 }\nstruct S;\n");
    set_text(&mut db, file, "fn f() { 1 + 2 }\nstruct S { a: u8 }\n");
    let edit = db.file_text(file).reparse(&db).edit().expect("an edit from the parse");
    assert_eq!(edit.before, parsed);
    assert_eq!(edit.delete, TextRange::new(TextSize::from(11), TextSize::from(22)));
    let start = reparsed();
    let parse = id(&db, Edition::Edition2021).parse(&db);
    assert_eq!(reparsed(), start + 1);
    assert_whole(parse, db.file_text(file).text(&db), Edition::Edition2021, false);

    // A text never parsed before its edit, with none before it: parsed whole.
    let fresh = FileId::from_raw(1);
    put_root(&mut db, 0, &[(file, "/a/lib.rs", "fn f() {}"), (fresh, "/a/b.rs", "fn b() {}")]);
    set_text(&mut db, fresh, "fn b() { 2 }");
    assert_eq!(db.file_text(fresh).reparse(&db).edit(), None);
    let start = reparsed();
    let fresh_id = EditionedFileId::new(&db, fresh, Edition::Edition2021);
    assert_eq!(fresh_id.parse(&db).root().text(), "fn b() { 2 }");
    assert_eq!(reparsed(), start);

    // Another edition's parse is not made from this one's.
    let _ = id(&db, Edition::Edition2021).parse(&db);
    set_text(&mut db, file, "fn f() { 3 }");
    let start = reparsed();
    let _ = id(&db, Edition::Edition2018).parse(&db);
    assert_eq!(reparsed(), start);
}

#[test]
fn a_cache_hit_of_parse_allocates_nothing() {
    let mut db = test_db();
    let file = FileId::from_raw(0);
    put_root(&mut db, 0, &[(file, "/a/lib.rs", include_str!("target.rs"))]);
    let _ = EditionedFileId::new(&db, file, Edition::Edition2021).parse(&db);
    let text = [include_str!("target.rs"), "\nfn added() {}\n"].concat();
    db.commit_with(|edit| {
        edit.set_file_text(file, &text);
    });
    let id = EditionedFileId::new(&db, file, Edition::Edition2021);
    let _ = id.parse(&db);
    blaze_alloc_count::assert_at_most(0, "a cache hit of `parse` after a reparse", || {
        id.parse(&db).is_outline()
    });
}

/// Source root 0's files in order, with their paths.
fn root_files(db: &Db) -> Vec<(FileId, VfsPath)> {
    let root = db.source_root(SourceRootId(0)).source_root_ref(db);
    root.iter().map(|id| (id, root.path_for_file(&id).unwrap().clone())).collect()
}

/// A view's files added and taken out are edited into its root's file set where it is, with
/// no copy, and give the set made again from the old one's files in their order with the
/// added ones after, as base-db made it before.
#[test]
fn a_view_edits_its_roots_files_in_place_as_making_them_again_does() {
    let mut rng = Rng::new(11);
    let mut db = test_db();
    let (base, partition) = checkout(&mut db);
    for step in 0..60 {
        let view = seeded_view(&mut rng);
        let before = root_files(&db);
        let at_before = db.source_root(SourceRootId(0)).source_root_ref(&db) as *const SourceRoot;
        let files: Vec<(&str, &[u8])> =
            view.iter().map(|(path, bytes)| (path.as_str(), bytes.as_slice())).collect();
        let change = set_view(&mut db, &base, &partition, &files).unwrap();

        let mut expected: Vec<(FileId, VfsPath)> =
            before.into_iter().filter(|(id, _)| !change.removed.contains(id)).collect();
        for (path, _) in &view {
            let path = VfsPath::from(base.join(path));
            let id =
                db.source_root(SourceRootId(0)).source_root_ref(&db).file_for_path(&path).copied();
            if let Some(id) = id
                && change.added.contains(&id)
            {
                expected.push((id, path));
            }
        }
        assert_eq!(root_files(&db), expected, "step {step}");
        let root = db.source_root(SourceRootId(0)).source_root_ref(&db);
        assert_eq!(root.len(), expected.len());
        for (id, path) in &expected {
            assert_eq!(root.file_for_path(path), Some(id), "step {step}");
        }
        if !change.roots.is_empty() {
            assert!(std::ptr::eq(at_before, root), "step {step}: the set was copied");
        }
    }
}

/// One crate of a drawn graph: the file it is rooted at, its cfg and env (by number, see
/// [`drawn_cfg`] and [`drawn_env`]) and the crates before it it depends on.
#[derive(Clone, Debug)]
struct DrawnCrate {
    root: usize,
    cfg: usize,
    env: usize,
    deps: Vec<usize>,
}

fn drawn_cfg(at: usize) -> CfgOptions {
    match at {
        0 => CfgOptions::default(),
        1 => cfg(&[flag("unix")]),
        _ => cfg(&[key_value("feature", "x")]),
    }
}

fn drawn_env(at: usize) -> Env {
    match at {
        0 => Env::default(),
        n => [("KEY".to_owned(), n.to_string())].into_iter().collect(),
    }
}

/// A crate on most of `files` files, each depending on some before it, and now and then a
/// second crate on the first one's root file told apart by its cfg.
fn draw_graph(rng: &mut Rng, files: usize) -> Vec<DrawnCrate> {
    let mut crates: Vec<DrawnCrate> = Vec::new();
    for root in 0..files {
        if rng.below(10) < 2 {
            continue;
        }
        let before = crates.len();
        let deps = (0..before).filter(|_| rng.coin()).collect();
        crates.push(DrawnCrate { root, cfg: rng.index(3), env: rng.index(3), deps });
    }
    if let Some(first) = crates.first().cloned()
        && rng.one_in(3)
    {
        crates.push(DrawnCrate { cfg: (first.cfg + 1) % 3, deps: Vec::new(), ..first });
    }
    crates
}

fn build_graph(drawn: &[DrawnCrate], files: &[FileId]) -> CrateGraphBuilder {
    let mut graph = CrateGraphBuilder::default();
    let mut ids: Vec<CrateBuilderId> = Vec::new();
    for krate in drawn {
        let id = add(&mut graph, files[krate.root], drawn_cfg(krate.cfg), drawn_env(krate.env));
        for &dep in &krate.deps {
            depend(&mut graph, id, ids[dep], &format!("d{dep}"));
        }
        ids.push(id);
    }
    graph
}

/// Source root 0 with `files[i]` at `/p{i}/lib.rs`, or at `/moved/p{i}.rs` where `moved[i]`.
fn root_at(db: &mut Db, files: &[FileId], moved: &[bool]) {
    let mut set = FileSet::default();
    for (at, (&file, &moved)) in files.iter().zip(moved).enumerate() {
        let path = if moved { format!("/moved/p{at}.rs") } else { format!("/p{at}/lib.rs") };
        set.insert(file, VfsPath::new_virtual_path(path));
    }
    db.commit_with(|edit| {
        edit.set_source_root(SourceRootId(0), Arc::new(SourceRoot::new_local(set)));
    });
}

/// The files of [`a_crate_graph_set_keeps_every_content_id_a_fresh_database_gives`]: `/p{i}/lib.rs`.
fn graph_files(db: &mut Db, count: usize) -> Vec<FileId> {
    let mut paths = Vec::new_in(Global);
    paths.extend((0..count).map(|at| format!("/p{at}/lib.rs")));
    let mut texts = Vec::new_in(Global);
    texts.extend(paths.iter().map(|path| (path.as_str(), "pub fn f() {}")));
    load(db, 0, &texts)
}

/// The content IDs of the listed crates, in order, and the list's: what a graph's crates are.
fn listed_contents(db: &Db) -> (Vec<ContentId, Global>, ContentId) {
    let mut contents = Vec::new_in(Global);
    contents.extend(crate::all_crates(db).iter().map(|krate| krate.content(db)));
    (contents, all_crates_content(db))
}

/// The invariant: each listed crate has the content ID a fresh database of the same files,
/// moves and graph gives, after any sequence of graphs, setters and moved root files.
fn assert_crates_fresh(db: &Db, files: usize, moved: &[bool], graph: &[DrawnCrate], what: &str) {
    let mut fresh = test_db();
    let fresh_files = graph_files(&mut fresh, files);
    root_at(&mut fresh, &fresh_files, moved);
    set_graph(&mut fresh, build_graph(graph, &fresh_files));
    assert_eq!(listed_contents(db), listed_contents(&fresh), "after {what}");
}

/// A crate's content ID, kept for an unchanged crate and made again for a moved one, is what a
/// fresh database gives, over random sequences of graphs, setters and moved root files.
#[test]
fn a_crate_graph_set_keeps_every_content_id_a_fresh_database_gives() {
    const FILES: usize = 6;
    for seed in 0..16 {
        let mut rng = Rng::new(seed);
        let mut db = test_db();
        let files = graph_files(&mut db, FILES);
        let mut moved = [false; FILES];
        let mut graph: Vec<DrawnCrate> = Vec::new_in(Global);
        let mut keys: Vec<CrateKey, Global> = Vec::new_in(Global);
        let set = |db: &mut Db, graph: &[DrawnCrate], keys: &mut Vec<CrateKey, Global>| {
            let builder = build_graph(graph, &files);
            let mut ids = Vec::new_in(Global);
            ids.extend(builder.iter());
            let crates = set_graph(db, builder);
            keys.clear();
            keys.extend(ids.iter().map(|id| crates[id]));
        };
        for step in 0..60 {
            match rng.below(6) {
                // A crate's ID follows its root file's path only once the graph is set again,
                // as a loader does after a move.
                0 => {
                    let at = rng.index(FILES);
                    moved[at] = !moved[at];
                    root_at(&mut db, &files, &moved);
                    set(&mut db, &graph, &mut keys);
                    let what = format!("a move, seed {seed} step {step}");
                    assert_crates_fresh(&db, FILES, &moved, &graph, &what);
                }
                1 => {
                    if !graph.is_empty() {
                        let at = rng.index(graph.len());
                        graph[at].env = rng.index(3);
                        let env = drawn_env(graph[at].env);
                        db.commit_with(|edit| {
                            edit.set_crate_env(keys[at], env);
                        });
                        let what = format!("a setter, seed {seed} step {step}");
                        assert_crates_fresh(&db, FILES, &moved, &graph, &what);
                    }
                }
                _ => {
                    graph = draw_graph(&mut rng, FILES);
                    set(&mut db, &graph, &mut keys);
                    let what = format!("{graph:?}, seed {seed} step {step}");
                    assert_crates_fresh(&db, FILES, &moved, &graph, &what);
                }
            }
        }
    }
}


/// An equal graph set again drops nothing; a changed dependency drops its own arena and its
/// dependent's. A change of its variables leaves its item key, so the crate list
/// (`world::List`) and the session arena, as they were.
#[test]
fn an_equal_crate_graph_drops_nothing() {
    let mut db = test_db();
    let files = load(&mut db, 0, FILES);
    let mut drawn = Vec::new_in(Global);
    drawn.push(DrawnCrate { root: 2, cfg: 1, env: 0, deps: Vec::new() });
    drawn.push(DrawnCrate { root: 0, cfg: 0, env: 1, deps: vec![0] });
    set_graph(&mut db, build_graph(&drawn, &files));
    let before = listed_contents(&db);
    let again = db.commit_with(|edit| {
        edit.set_crate_graph(build_graph(&drawn, &files));
    });
    assert_dropped_nothing(&again, "an equal graph");
    assert_eq!(listed_contents(&db), before);

    let mut changed = drawn.clone();
    changed[0].env = 2;
    let committed = db.commit_with(|edit| {
        edit.set_crate_graph(build_graph(&changed, &files));
    });
    let mut keys = Vec::new_in(Global);
    keys.extend(crate::all_crates(&db).iter().map(|krate| krate.key(&db)));
    for key in keys {
        let index = arena_of(&db, key);
        assert!(committed.dropped.contains(&index), "arena {index} of {key:?} kept: {:?}", committed.dropped);
    }
    assert!(!committed.dropped.contains(&0), "a crate's variables dropped the session: {:?}", committed.dropped);
}

/// A crate left out of the graph while its dependency changed comes back with the content ID
/// its fields and its dependency's new one give.
#[test]
fn a_crate_back_after_its_dependency_changed_has_a_fresh_content_id() {
    let mut db = test_db();
    let files = graph_files(&mut db, 3);
    let dependency = |env| DrawnCrate { root: 2, cfg: 0, env, deps: Vec::new() };
    let dependent = DrawnCrate { root: 0, cfg: 0, env: 0, deps: vec![0] };
    set_graph(&mut db, build_graph(&[dependency(0), dependent.clone()], &files));
    set_graph(&mut db, build_graph(&[dependency(1)], &files));
    let back = [dependency(1), dependent];
    set_graph(&mut db, build_graph(&back, &files));
    assert_crates_fresh(&db, 3, &[false; 3], &back, "the dependent's return");
}

/// The copies of a crate origin's hash that `Crate::content_of`, hir-def's dependency key and
/// blaze-items' crate key each had before [`CrateOrigin::hash_into`].
fn old_origin_hash(origin: &CrateOrigin) -> ContentId {
    use crate::LangCrateOrigin;
    use blaze_ids::Hasher;

    fn opt<'h>(hasher: &'h mut Hasher, part: Option<&str>) -> &'h mut Hasher {
        match part {
            None => hasher.update(&[0]),
            Some(part) => hasher.update(&[1]).part(part.as_bytes()),
        }
    }
    let mut hasher = Hasher::new("blaze v2 test origin");
    match origin {
        CrateOrigin::Rustc { name } => hasher.update(&[0]).part(name.as_str().as_bytes()),
        CrateOrigin::Local { repo, name } => {
            hasher.update(&[1]);
            opt(&mut hasher, repo.as_deref());
            opt(&mut hasher, name.as_ref().map(|it| it.as_str()))
        }
        CrateOrigin::Library { repo, name } => {
            hasher.update(&[2]);
            opt(&mut hasher, repo.as_deref());
            hasher.part(name.as_str().as_bytes())
        }
        CrateOrigin::Lang(lang) => hasher.update(&[
            3,
            match lang {
                LangCrateOrigin::Alloc => 0,
                LangCrateOrigin::Core => 1,
                LangCrateOrigin::ProcMacro => 2,
                LangCrateOrigin::Std => 3,
                LangCrateOrigin::Test => 4,
                LangCrateOrigin::Dependency => 5,
                LangCrateOrigin::Other => 6,
            },
        ]),
    };
    hasher.finish()
}

#[test]
fn an_origins_hash_is_the_old_copies() {
    use crate::LangCrateOrigin;
    use blaze_ids::Hasher;

    let repos = [None, Some(String::new()), Some("https://github.com/a/b".to_owned())];
    let names = [Symbol::intern(""), Symbol::intern("serde"), Symbol::intern("a-b")];
    let mut origins = Vec::new();
    for name in &names {
        origins.push(CrateOrigin::Rustc { name: name.clone() });
        for repo in &repos {
            origins.push(CrateOrigin::Local { repo: repo.clone(), name: None });
            origins.push(CrateOrigin::Local { repo: repo.clone(), name: Some(name.clone()) });
            origins.push(CrateOrigin::Library { repo: repo.clone(), name: name.clone() });
        }
    }
    origins.extend(
        [
            LangCrateOrigin::Alloc,
            LangCrateOrigin::Core,
            LangCrateOrigin::ProcMacro,
            LangCrateOrigin::Std,
            LangCrateOrigin::Test,
            LangCrateOrigin::Dependency,
            LangCrateOrigin::Other,
        ]
        .map(CrateOrigin::Lang),
    );
    for origin in &origins {
        let mut hasher = Hasher::new("blaze v2 test origin");
        origin.hash_into(&mut hasher);
        assert_eq!(hasher.finish(), old_origin_hash(origin), "{origin:?}");
    }
}

#[test]
fn a_target_kinds_tag_is_the_old_match() {
    let old = |kind: TargetKind| match kind {
        TargetKind::Bin => 0,
        TargetKind::Lib { is_proc_macro: false } => 1,
        TargetKind::Lib { is_proc_macro: true } => 2,
        TargetKind::Example => 3,
        TargetKind::Test => 4,
        TargetKind::Bench => 5,
        TargetKind::BuildScript => 6,
        TargetKind::Other => 7,
    };
    for kind in [
        TargetKind::Bin,
        TargetKind::Lib { is_proc_macro: false },
        TargetKind::Lib { is_proc_macro: true },
        TargetKind::Example,
        TargetKind::Test,
        TargetKind::Bench,
        TargetKind::BuildScript,
        TargetKind::Other,
    ] {
        assert_eq!(kind.tag(), old(kind), "{kind:?}");
    }
}

/// The process's variables `of`.
fn process_vars(of: &[(&str, &str)]) -> Env {
    Env::from_iter(of.iter().map(|(name, value)| ((*name).into(), (*value).into())))
}

/// `FOO` as read after the process's variables are set to `before` and then to `after`, and
/// what the second commit dropped.
fn foo_after_env(before: &[(&str, &str)], after: &[(&str, &str)]) -> (Option<String>, Committed) {
    let mut db = test_db();
    db.commit_with(|edit| {
        edit.set_process_env(process_vars(before));
    });
    assert_eq!(crate::process_env_var(&db, "FOO"), Some("1".to_owned()));
    let committed = db.commit_with(|edit| {
        edit.set_process_env(process_vars(after));
    });
    (crate::process_env_var(&db, "FOO"), committed)
}

/// The invariant: the process's variables set again equal drop nothing, and any other set is
/// read back by `process_env_var`, never an older value. A set that moves any variable drops
/// the session arena, and every arena with it (the floor the design states: every query may
/// read the process's variables).
#[test]
fn the_process_variables_set_again_drop_nothing_and_a_change_is_read() {
    let before = [("FOO", "1"), ("UNRELATED", "a")];
    let (foo, committed) = foo_after_env(&before, &before);
    assert_eq!(foo, Some("1".to_owned()));
    assert_dropped_nothing(&committed, "the same variables again");
    for after in [
        &[("FOO", "1"), ("UNRELATED", "b")][..],
        &[("FOO", "1")][..],
        &[("FOO", "1"), ("UNRELATED", "a"), ("MORE", "x")][..],
    ] {
        let (foo, committed) = foo_after_env(&before, after);
        assert_eq!(foo, Some("1".to_owned()), "{after:?}");
        assert!(committed.dropped.contains(&0), "{after:?} moved the session: {committed:?}");
    }
    let (foo, _) = foo_after_env(&before, &[("FOO", "2"), ("UNRELATED", "a")]);
    assert_eq!(foo, Some("2".to_owned()), "the read variable's new value");
}

/// Two crates, `a` on source root 0 and `c` on source root 1, neither depending on the other,
/// committed, and `c`'s file: an edit of that file drops `c`'s arena alone, carrying `a`'s and
/// the session's.
fn two_roots(db: &mut Db) -> (CrateKey, CrateKey, FileId) {
    let (fa, fc) = (FileId::from_raw(0), FileId::from_raw(1));
    put_root(db, 0, &[(fa, "/a/lib.rs", "pub fn a() {}")]);
    put_root(db, 1, &[(fc, "/c/lib.rs", "pub fn c() {}")]);
    let mut graph = CrateGraphBuilder::default();
    let a = add(&mut graph, fa, CfgOptions::default(), Env::default());
    let c = add(&mut graph, fc, CfgOptions::default(), Env::default());
    let crates = set_graph(db, graph);
    (crates[&a], crates[&c], fc)
}

/// The arena index of `key`'s crate in `db`'s snapshot.
fn arena_of(db: &Db, key: CrateKey) -> u32 {
    use blaze_memo::Id as _;
    db.krate(key).gid().arena().index()
}

/// A fact of the tests' gifts and makers: one id, its digest of `digest`.
fn test_fact(digest: &str) -> blaze_memo::Fact {
    blaze_memo::Fact {
        id: ContentId::of("base-db test given", &[]),
        digest: ContentId::of("base-db test given digest", digest.as_bytes()),
    }
}

/// Variables that move a crate's content ID, so its arena is made again.
fn some_env() -> Env {
    [("KEY".to_owned(), "moved".to_owned())].into_iter().collect()
}

/// The kind of [`a_gift_by_fact_is_read_back_and_kept_until_its_fact_moves`]'s gifts.
struct ByFact;

/// The invariant: a gift by the giver's fact ([`Edit::give_by`]) is read back after its commit,
/// is kept by a commit that does not touch its crate (its arena carried) and by one that makes
/// its crate's arena again for another reason (the world keeps it), moves nothing when given
/// again under the fact the slot holds (the slot keeps its value), and moves its crate when
/// given under another fact.
#[test]
fn a_gift_by_fact_is_read_back_and_kept_until_its_fact_moves() {
    let mut db = test_db();
    let (a, _, fc) = two_roots(&mut db);
    let given = |db: &Db| db.krate(a).given::<ByFact, u32>(db).copied();
    db.commit_with(|edit| {
        edit.give_by::<ByFact>(a, test_fact("1"), crate::Given::new(1_u32));
    });
    assert_eq!(given(&db), Some(1));

    let arena = arena_of(&db, a);
    let committed = db.commit_with(|edit| {
        edit.set_file_text(fc, "pub fn c2() {}");
    });
    assert!(!committed.dropped.contains(&arena), "another crate's file dropped `a`: {committed:?}");
    assert_eq!(given(&db), Some(1), "carried with its arena");

    let committed = db.commit_with(|edit| {
        edit.give_by::<ByFact>(a, test_fact("1"), crate::Given::new(2_u32));
    });
    assert_dropped_nothing(&committed, "a gift under the fact the slot holds");
    assert_eq!(given(&db), Some(1), "the slot keeps the value it has");

    let committed = db.commit_with(|edit| {
        edit.set_crate_env(a, some_env());
    });
    assert!(committed.dropped.contains(&arena), "its variables moved: {committed:?}");
    assert_eq!(given(&db), Some(1), "made again from the world");

    let arena = arena_of(&db, a);
    let committed = db.commit_with(|edit| {
        edit.give_by::<ByFact>(a, test_fact("2"), crate::Given::new(3_u32));
    });
    assert!(committed.dropped.contains(&arena), "a gift under another fact: {committed:?}");
    assert_eq!(given(&db), Some(3));
}

/// A singleton the tests' makers set in the session arena ([`Edit::set_given`]).
#[blaze_memo::input(singleton, debug)]
struct Held {
    #[returns(copy)]
    value: u32,
}

/// A maker of [`Held`] `value` read by `fact`, counting its runs in `runs` and, in `arena`, the
/// index of the arena it last ran in.
fn held_maker(
    runs: &std::sync::Arc<AtomicUsize, Global>,
    arena: &std::sync::Arc<std::sync::atomic::AtomicU32, Global>,
    fact: blaze_memo::Fact,
    value: Option<u32>,
) -> crate::MakeGiven {
    let runs = std::sync::Arc::clone(runs);
    let arena = std::sync::Arc::clone(arena);
    Box::new_in(
        move |_: &blaze_memo::Builder, inputs: &blaze_memo::Inputs<'_>| {
            runs.fetch_add(1, Ordering::Relaxed);
            arena.store(inputs.id().index(), Ordering::Relaxed);
            if let Some(value) = value {
                Held::set(inputs, fact, value);
            }
        },
        Global,
    )
}

/// Counters a [`held_maker`] writes.
fn counters() -> (std::sync::Arc<AtomicUsize, Global>, std::sync::Arc<std::sync::atomic::AtomicU32, Global>) {
    (
        std::sync::Arc::new_in(AtomicUsize::new(0), Global),
        std::sync::Arc::new_in(std::sync::atomic::AtomicU32::new(u32::MAX), Global),
    )
}

/// The invariant: what a maker of the session arena sets ([`Edit::set_given`] of no crate) is
/// read back after its commit; a commit that carries the session runs it not again and keeps
/// what it made, one that makes the session again runs it again (the world keeps it); a maker
/// under the fact held moves nothing and never runs, one under another fact takes its place.
#[test]
fn a_session_maker_is_read_back_and_kept_until_its_fact_moves() {
    let mut db = test_db();
    let (a, _, fc) = two_roots(&mut db);
    let held = |db: &Db| Held::try_get(db).map(|it| it.value(db));
    let (runs, arena) = counters();
    db.commit_with(|edit| {
        edit.set_given(None, test_fact("1"), held_maker(&runs, &arena, test_fact("1"), Some(7)));
    });
    assert_eq!(held(&db), Some(7));
    assert_eq!((runs.load(Ordering::Relaxed), arena.load(Ordering::Relaxed)), (1, 0));

    let committed = db.commit_with(|edit| {
        edit.set_file_text(fc, "pub fn c2() {}");
    });
    assert!(!committed.dropped.contains(&0), "a crate's file dropped the session: {committed:?}");
    assert_eq!(held(&db), Some(7), "carried with the session arena");
    assert_eq!(runs.load(Ordering::Relaxed), 1, "a carried arena runs no maker");

    let committed = db.commit_with(|edit| {
        edit.set_crate_env(a, some_env());
    });
    let remade = usize::from(committed.dropped.contains(&0));
    assert_eq!(runs.load(Ordering::Relaxed), 1 + remade, "run once a making: {committed:?}");
    assert_eq!(held(&db), Some(7));

    let (other, other_arena) = counters();
    let committed = db.commit_with(|edit| {
        edit.set_given(None, test_fact("1"), held_maker(&other, &other_arena, test_fact("1"), Some(8)));
    });
    assert_dropped_nothing(&committed, "a maker under the fact held");
    assert_eq!(held(&db), Some(7));
    assert_eq!(other.load(Ordering::Relaxed), 0, "dropped, never run");

    let committed = db.commit_with(|edit| {
        edit.set_given(None, test_fact("2"), held_maker(&other, &other_arena, test_fact("2"), Some(9)));
    });
    assert!(committed.dropped.contains(&0), "a maker under another fact: {committed:?}");
    assert_eq!(held(&db), Some(9));
    assert_eq!(other.load(Ordering::Relaxed), 1);
}

/// The invariant: a maker of a crate's arena ([`Edit::set_given`] of the crate) runs in that
/// crate's arena at its commit, not again while the arena is carried, again each time the
/// arena is made again; one under the fact held moves nothing.
#[test]
fn a_crate_maker_runs_in_its_crates_arena_each_time_it_is_made() {
    let mut db = test_db();
    let (a, _, fc) = two_roots(&mut db);
    let (runs, arena) = counters();
    db.commit_with(|edit| {
        edit.set_given(Some(a), test_fact("1"), held_maker(&runs, &arena, test_fact("1"), None));
    });
    assert_eq!(runs.load(Ordering::Relaxed), 1);
    assert_eq!(arena.load(Ordering::Relaxed), arena_of(&db, a));

    let committed = db.commit_with(|edit| {
        edit.set_file_text(fc, "pub fn c2() {}");
    });
    assert!(!committed.dropped.contains(&arena_of(&db, a)), "{committed:?}");
    assert_eq!(runs.load(Ordering::Relaxed), 1, "a carried arena runs no maker");

    db.commit_with(|edit| {
        edit.set_crate_env(a, some_env());
    });
    assert_eq!(runs.load(Ordering::Relaxed), 2, "its arena made again");
    assert_eq!(arena.load(Ordering::Relaxed), arena_of(&db, a));

    let (other, other_arena) = counters();
    let committed = db.commit_with(|edit| {
        edit.set_given(Some(a), test_fact("1"), held_maker(&other, &other_arena, test_fact("1"), None));
    });
    assert_dropped_nothing(&committed, "a crate maker under the fact held");
    assert_eq!(other.load(Ordering::Relaxed), 0);
}

/// A sealed library crate `lib` (source root 1), a workspace crate `top` over it (root 0) and
/// a workspace crate `other` beside them (root 2), committed: `[lib, top, other]`.
fn beside_a_sealed_library(db: &mut Db) -> [CrateKey; 3] {
    let (lib_file, top_file, other_file) = (FileId::from_raw(0), FileId::from_raw(1), FileId::from_raw(2));
    put_root(db, 0, &[(top_file, "/top/lib.rs", "pub fn caller() {}")]);
    put_root(db, 1, &[(lib_file, "/lib/lib.rs", "pub fn callee() {}")]);
    put_root(db, 2, &[(other_file, "/other/lib.rs", "pub fn other() {}")]);
    let mut graph = CrateGraphBuilder::default();
    let lib = add(&mut graph, lib_file, CfgOptions::default(), Env::default());
    graph.seal(lib, ContentId::of("base-db sealed write test pin", b"lib"));
    let top = add(&mut graph, top_file, CfgOptions::default(), Env::default());
    depend(&mut graph, top, lib, "lib");
    let other = add(&mut graph, other_file, CfgOptions::default(), Env::default());
    let crates = set_graph(db, graph);
    [crates[&lib], crates[&top], crates[&other]]
}

/// The oracle of a commit that wrote `lib` alone ([`beside_a_sealed_library`]): it dropped
/// exactly `lib`'s arena and its dependent `top`'s, never the session's, the roots' or
/// `other`'s (so it started no new session).
fn dropped_lib_and_top(before: &[u32; 3], committed: &Committed) -> Result<(), String> {
    let mut want = Vec::new_in(Global);
    want.extend_from_slice(&before[..2]);
    want.sort_unstable();
    match committed.dropped[..] == want[..] {
        true => Ok(()),
        false => Err(format!(
            "a write for the sealed crate dropped arenas {:?}, not {want:?} (its own and its dependent's; `other` was at {})",
            committed.dropped, before[2]
        )),
    }
}

/// The arenas of `keys` in `db`'s snapshot.
fn arenas_of(db: &Db, keys: [CrateKey; 3]) -> [u32; 3] {
    keys.map(|key| arena_of(db, key))
}

/// Each write of a sealed crate the driver makes after the crate was first made: a build
/// script's variables, a gift (a surface, an image, a key of its check results) and a maker
/// of its arena.
fn sealed_writes() -> [(&'static str, fn(&mut Edit<'_>, CrateKey)); 3] {
    fn env(edit: &mut Edit<'_>, lib: CrateKey) {
        edit.set_crate_env(lib, some_env());
    }
    fn gift(edit: &mut Edit<'_>, lib: CrateKey) {
        edit.give_by::<ByFact>(lib, test_fact("sealed gift"), crate::Given::new(5_u32));
    }
    fn maker(edit: &mut Edit<'_>, lib: CrateKey) {
        let (runs, arena) = counters();
        edit.set_given(Some(lib), test_fact("sealed maker"), held_maker(&runs, &arena, test_fact("sealed maker"), None));
    }
    [("its variables", env), ("a gift", gift), ("a maker", maker)]
}

/// One sealed write `write` under `plant`, through the oracle [`dropped_lib_and_top`], and
/// the write read back.
fn a_sealed_write(plant: Plant, what: &str, write: fn(&mut Edit<'_>, CrateKey)) -> Result<(), String> {
    let mut db = test_db();
    let keys = beside_a_sealed_library(&mut db);
    let [lib, _, _] = keys;
    db.plant(plant);
    let before = arenas_of(&db, keys);
    let committed = db.commit_with(|edit| write(edit, lib));
    dropped_lib_and_top(&before, &committed).map_err(|why| format!("{what}: {why}"))?;
    let after = arenas_of(&db, keys);
    if after[2] != before[2] {
        return Err(format!("{what}: `other` moved from arena {} to {}", before[2], after[2]));
    }
    Ok(())
}

/// The invariant: a write to a sealed crate (its variables, a gift, a maker: what a build
/// script's run, a served surface or image and a key of its check results write after the
/// crate was made) drops that crate's arena and its dependents' (each borrowed it), and
/// nothing else: not the session, not a source root, not a crate beside it; no new session.
/// The control: with `Plant::SealedRestartsSession` (a sealed arena marked starts a new
/// session, as a commit did while `Memo::next` refused one) the oracle names the session.
#[test]
fn a_write_to_a_sealed_crate_drops_it_and_its_dependents_alone() {
    for (what, write) in sealed_writes() {
        a_sealed_write(Plant::None, what, write).unwrap_or_else(|why| panic!("{why}"));
        let planted = a_sealed_write(Plant::SealedRestartsSession, what, write)
            .expect_err("a sealed write that started a new session was not caught");
        assert!(planted.contains("dropped arenas"), "{planted}");
    }
    // Read back: the variables written are the arena's made again.
    let mut db = test_db();
    let [lib, top, _] = beside_a_sealed_library(&mut db);
    let before = db.krate(top).content(&db);
    db.commit_with(|edit| { edit.set_crate_env(lib, some_env()); });
    assert_eq!(db.krate(lib).env(&db), &some_env());
    assert_ne!(db.krate(top).content(&db), before, "the dependent's content follows its dependency");
}

/// A driver's giver of one value per crate (the key a crate's constants are kept under,
/// `hir_ty::consteval::deps`), with a session input beside it (the store): given `value` for
/// `lib`. `in_session`: the value is in the session maker's fact (one session input for
/// every crate's value, as the constants' were given), not a gift of `lib`'s own.
fn give_for_lib(edit: &mut Edit<'_>, lib: CrateKey, value: u32, in_session: bool) {
    let (runs, arena) = counters();
    match in_session {
        true => {
            let fact = test_fact(&format!("store, lib {value}"));
            edit.set_given(None, fact, held_maker(&runs, &arena, fact, Some(value)));
        }
        false => {
            let fact = test_fact("store");
            edit.set_given(None, fact, held_maker(&runs, &arena, fact, Some(0)));
            edit.give_by::<ByFact>(lib, test_fact(&format!("lib {value}")), crate::Given::new(value));
        }
    }
}

/// A value for `lib` given, then another (a crate checked, then its key stored), through the
/// oracle [`dropped_lib_and_top`], and the value read back.
fn a_given_for_one_crate(plant: Plant, in_session: bool) -> Result<(), String> {
    let mut db = test_db();
    let keys = beside_a_sealed_library(&mut db);
    let [lib, _, _] = keys;
    db.commit_with(|edit| give_for_lib(edit, lib, 1, in_session));
    db.plant(plant);
    let before = arenas_of(&db, keys);
    let committed = db.commit_with(|edit| give_for_lib(edit, lib, 2, in_session));
    dropped_lib_and_top(&before, &committed)?;
    let read = match in_session {
        true => Held::try_get(&db).map(|it| it.value(&db)),
        false => db.krate(lib).given::<ByFact, u32>(&db).copied(),
    };
    match read {
        Some(2) => Ok(()),
        other => Err(format!("the value read back is {other:?}, not 2")),
    }
}

/// The invariant: a value a driver gives for one crate, given as that crate's own gift beside
/// a session input whose fact does not move, keeps every other arena (the session's, the
/// roots', a crate beside it) when it moves, and starts no new session, though the crate is
/// sealed: it drops that crate and its dependents alone. The controls: given in the session
/// maker's fact (what the constants' kept fact was), the oracle names the session; and with
/// `Plant::SealedRestartsSession` it names it too.
#[test]
fn a_given_for_one_crate_keeps_every_other_crates_arena() {
    a_given_for_one_crate(Plant::None, false).unwrap_or_else(|why| panic!("{why}"));
    let in_session = a_given_for_one_crate(Plant::None, true)
        .expect_err("a value for one crate in the session's fact was not caught");
    assert!(in_session.contains("dropped arenas"), "{in_session}");
    let planted = a_given_for_one_crate(Plant::SealedRestartsSession, false)
        .expect_err("a sealed write that started a new session was not caught");
    assert!(planted.contains("dropped arenas"), "{planted}");
}

/// The kind of [`a_crate_added_and_given_in_one_edit_is_one_commit`]'s gift, as a driver's
/// proc macros are given.
struct ProcMacrosGift;

/// The invariant: a crate the edit adds has its key before the commit ([`Edit::crate_key`]),
/// so the graph and a gift to its new crate are one commit; the key is the one the commit
/// reports.
#[test]
fn a_crate_added_and_given_in_one_edit_is_one_commit() {
    let mut db = test_db();
    let files = load(&mut db, 0, FILES);
    let mut graph = CrateGraphBuilder::default();
    let a = add(&mut graph, files[0], CfgOptions::default(), Env::default());
    let (in_edit, committed) = db.edit_with(|edit| {
        edit.set_crate_graph(graph);
        let key = edit.crate_key(a);
        edit.give::<ProcMacrosGift>(key, crate::Given::new(41_u32));
        key
    });
    let key = committed.crates[&a];
    assert_eq!(in_edit, key, "the key the commit reports");
    assert_eq!(db.krate(key).given::<ProcMacrosGift, u32>(&db), Some(&41));
}

#[test]
#[should_panic(expected = "no crate graph was set in this edit")]
fn a_crate_key_without_a_graph_set_names_the_builder_id() {
    let mut db = test_db();
    let mut graph = CrateGraphBuilder::default();
    let files = load(&mut db, 0, FILES);
    let a = add(&mut graph, files[0], CfgOptions::default(), Env::default());
    let edit = db.edit();
    _ = edit.crate_key(a);
}

/// The invariant: a crate's data read back keyed ([`crate::BuiltCrateData::keyed`]) is the
/// world's: given back, it moves nothing, and each dependency's key and flags are as set.
#[test]
fn a_crates_keyed_data_round_trips_through_the_world() {
    let mut db = test_db();
    let files = load(&mut db, 0, FILES);
    let mut graph = CrateGraphBuilder::default();
    let a = add(&mut graph, files[0], CfgOptions::default(), Env::default());
    let c = add(&mut graph, files[2], CfgOptions::default(), Env::default());
    let dependency =
        DependencyBuilder::with_prelude(CrateName::new("c").unwrap(), c, false, true).with_private(true);
    graph.add_dep(a, dependency).unwrap();
    let crates = set_graph(&mut db, graph);
    let (a, c) = (crates[&a], crates[&c]);

    let data = db.krate(a).data(&db).keyed(&db);
    let [dep] = &data.dependencies[..] else { panic!("one dependency: {data:?}") };
    assert_eq!(dep.crate_id, c);
    assert_eq!((dep.is_prelude(), dep.is_sysroot(), dep.is_private()), (false, true, true));
    assert_eq!(data.root_file_id, files[0]);

    let content = db.krate(a).content(&db);
    let committed = db.commit_with(|edit| {
        edit.set_crate_data(a, data);
    });
    assert_dropped_nothing(&committed, "a crate's own data given back");
    assert_eq!(db.krate(a).content(&db), content);
}

/// How [`len_while_another_asks`]'s run and its second asker meet.
struct Meeting {
    /// The runs of the query begun.
    runs: u32,
    /// The second asker has its answer.
    answered: bool,
}

static MEETING: blaze_core::lock::Lock<Meeting> =
    blaze_core::lock::Lock::new("base-db.tests.meeting", Meeting { runs: 0, answered: false });
static MET: blaze_core::lock::Signal = blaze_core::lock::Signal::new("base-db.tests.met");

/// The length of `file`'s text. Its run lets the second asker go, then gives it a bounded
/// while to ask: on the scheduler's flight that asker waits for this run, so it has no answer
/// before the run ends and the while runs out; on a flight that refused it, it ends at once.
#[blaze_memo::tracked(returns(copy))]
fn len_while_another_asks(db: &Db, file: EditionedFileId) -> u32 {
    let (file, _) = file.unpack(db);
    let len = db.file_text(file).text(db).len() as u32;
    let mut meeting = MEETING.lock();
    meeting.runs += 1;
    MET.notify_all();
    _ = MET.wait_while_for(&mut meeting, |it| !it.answered, std::time::Duration::from_millis(200));
    len
}

/// The invariant: a test database that several threads drive ([`Db::for_tests_driven`]) asked
/// for one query from two threads at once runs it once, and both get its value: the second
/// asker waits for the first one's run, never told it is in a cycle with a run on another
/// thread's stack. The interleavings are proven in blaze-memo's simulation
/// (`two_tasks_ask_one_key`); this is the database's flight as tests take it.
#[test]
fn a_driven_test_database_asked_one_query_from_two_threads_runs_it_once() {
    let mut db = Db::for_tests_driven(no_boundaries, blaze_memo::Reads::Off);
    let file = load(&mut db, 0, &[("/lib.rs", "fn f() {}")])[0];
    let file = EditionedFileId::new(&db, file, Edition::Edition2021);
    ran(&db, "len_while_another_asks");
    let (first, second) = std::thread::scope(|scope| {
        let db = &db;
        let second = scope.spawn(move || {
            {
                let mut meeting = MEETING.lock();
                let timed_out = MET.wait_while_for(
                    &mut meeting,
                    |it| it.runs == 0,
                    std::time::Duration::from_secs(60),
                );
                assert!(!timed_out, "the first asker's run did not begin in 60 s");
            }
            let got = len_while_another_asks(db, file);
            MEETING.lock().answered = true;
            MET.notify_all();
            got
        });
        let first = len_while_another_asks(db, file);
        let second = second.join().unwrap_or_else(|payload| std::panic::resume_unwind(payload));
        (first, second)
    });
    assert_eq!(first, 9, "the text's length");
    assert_eq!(second, first, "both askers get the one run's value");
    assert_eq!(ran(&db, "len_while_another_asks"), 1, "the query ran once");
    assert_eq!(MEETING.lock().runs, 1, "one run of the query began");
}

/// Has every crate of `keys` evaluate a query in its own arena ([`crate::crate_root_content`]:
/// its root's files, its other `OUT_DIR`s), so each arena holds something a drop would lose.
fn evaluate_in(db: &Db, keys: &[CrateKey]) {
    for &key in keys {
        _ = crate::crate_root_content(db, db.krate(key));
    }
}

/// [`beside_a_sealed_library`] with each crate's arena holding a query's value, then a located
/// gift to the sealed `lib` (the image's lang items, a crate's constants' key), under `plant`:
/// the commit must drop nothing and the gift read back; given again under the fact held, it
/// moves nothing; withdrawn, it drops `lib` and `top` alone ([`dropped_lib_and_top`]).
fn a_located_gift(plant: Plant) -> Result<(), String> {
    let mut db = test_db();
    let keys = beside_a_sealed_library(&mut db);
    let [lib, _, _] = keys;
    evaluate_in(&db, &keys);
    db.plant(plant);
    let give = |edit: &mut Edit<'_>, value: u32| {
        edit.give_located::<crate::LocatedForTests>(lib, test_fact("located"), crate::Given::new(value));
    };
    let committed = db.commit_with(|edit| give(edit, 5));
    if !committed.dropped.is_empty() {
        return Err(format!("a located gift's first gift dropped arenas {:?}", committed.dropped));
    }
    match db.krate(lib).located::<crate::LocatedForTests, u32>(&db) {
        Some(5) => {}
        other => return Err(format!("the located gift read back is {other:?}, not 5")),
    }
    // A slot of the crate's arena it is not: `given` of the kind finds none.
    assert_eq!(db.krate(lib).given::<crate::LocatedForTests, u32>(&db), None, "no slot");
    let committed = db.commit_with(|edit| give(edit, 6));
    assert_dropped_nothing(&committed, "a located gift under the fact held");
    let before = arenas_of(&db, keys);
    let committed = db.commit_with(|edit| {
        edit.take_located::<crate::LocatedForTests>(lib);
    });
    dropped_lib_and_top(&before, &committed).map_err(|why| format!("withdrawn: {why}"))?;
    assert_eq!(db.krate(lib).located::<crate::LocatedForTests, u32>(&db), None, "withdrawn");
    Ok(())
}

/// The invariant: a gift that says where a value can be read ([`crate::Located`]) is no input
/// of any arena, so its first gift to a sealed crate whose arena holds evaluated queries
/// drops nothing, and it is read back off the table; a withdrawal drops the crate and its
/// dependents alone. The control: with `Plant::LocatedMarks` (the first gift marks its crate,
/// as the lang items' and the constants' key's slots did, 28 and 24 arenas in round 27's
/// first body edit) the oracle names the drop.
#[test]
fn a_located_gift_to_a_sealed_crate_drops_nothing() {
    a_located_gift(Plant::None).unwrap_or_else(|why| panic!("{why}"));
    let planted = a_located_gift(Plant::LocatedMarks).expect_err("a located gift that marked its crate was not caught");
    assert!(planted.contains("first gift dropped arenas"), "{planted}");
}

/// The invariant's control of [`a_located_gift_to_a_sealed_crate_drops_nothing`]'s rule that a
/// kind is given one way: a slot gift of a kind held located stops, naming the kind.
#[test]
#[should_panic(expected = "which holds a gift of that kind given as located")]
fn a_slot_gift_of_a_located_kind_stops() {
    let mut db = test_db();
    let [lib, _, _] = beside_a_sealed_library(&mut db);
    db.commit_with(|edit| {
        edit.give_located::<crate::LocatedForTests>(lib, test_fact("located"), crate::Given::new(1_u32));
    });
    let mut edit = db.edit();
    edit.give_by::<crate::LocatedForTests>(lib, test_fact("slot"), crate::Given::new(2_u32));
}

/// [`beside_a_sealed_library`] with each crate's arena holding a query's value, then `lib`'s
/// cfg written under `plant` (a build script's `cargo:rustc-cfg`, which moves its item key and
/// so the crate list): the commit must drop `lib` and its dependent `top`, and nothing of the
/// session's, the roots' or `other`'s (the crate list's own arena may go: nothing here read
/// it); the cfg read back.
fn a_cfg_write(plant: Plant) -> Result<(), String> {
    let mut db = test_db();
    let keys = beside_a_sealed_library(&mut db);
    let [lib, top, other] = keys;
    evaluate_in(&db, &keys);
    db.plant(plant);
    let before = arenas_of(&db, keys);
    // The arenas themselves (index and generation): a crate made again keeps its index (the
    // order did not move) under a new generation, so an index alone cannot show a remake.
    let made = |db: &Db, key: CrateKey| {
        use blaze_memo::Id as _;
        db.krate(key).gid().arena()
    };
    let (top_was, other_was) = (made(&db, top), made(&db, other));
    let committed = db.commit_with(|edit| {
        edit.set_crate_cfg_options(lib, cfg(&[flag("from_build_script")]));
    });
    let list = crate::world::LIST_ARENA;
    let stray: Vec<u32, Global> = {
        let mut stray = Vec::new_in(Global);
        stray.extend(committed.dropped.iter().copied().filter(|&it| it != list && it != before[0] && it != before[1]));
        stray
    };
    if !stray.is_empty() || !committed.dropped.contains(&before[0]) || !committed.dropped.contains(&before[1]) {
        return Err(format!(
            "a cfg write to `lib` dropped arenas {:?}: not `lib` ({}) and `top` ({}) alone, and `other` was at {}",
            committed.dropped, before[0], before[1], before[2]
        ));
    }
    assert_eq!(db.krate(lib).cfg_options(&db), &cfg(&[flag("from_build_script")]));
    assert_eq!(made(&db, other), other_was, "`other` kept its arena");
    assert_ne!(made(&db, top), top_was, "`top`, its dependent, made again");
    assert_eq!(made(&db, top).index(), before[1], "`top` made again at its index");
    Ok(())
}

/// The invariant: a cfg written for one crate (each of a cold build's build scripts' runs)
/// drops that crate's arena and its dependents' and keeps every other crate's arena, the
/// session's and the roots': the crate list it moves has an arena of its own, which only the
/// runs that read the list borrow. The control: with `Plant::ListMarksSession` (the list a
/// singleton of the session's, as it was until round 28: four cfg writes, 32 arenas dropped
/// each) the oracle names the session.
#[test]
fn a_cfg_write_drops_its_crate_and_dependents_alone() {
    a_cfg_write(Plant::None).unwrap_or_else(|why| panic!("{why}"));
    let planted = a_cfg_write(Plant::ListMarksSession).expect_err("a cfg write that dropped the session was not caught");
    assert!(planted.contains("dropped arenas"), "{planted}");
}

/// Two source roots committed with no crate, then under `plant` a crate graph whose `lib` is
/// sealed and rooted in root 1, beside `top` on root 0 depending on it: the root's arena,
/// carried from the first commit unsealed, must be made again sealed, so the snapshot is
/// built (a sealed arena borrows only sealed ones) and `lib`'s root's arena is dropped; the
/// panic's text when the build refuses it.
fn a_root_sealed_after_it_was_made(plant: Plant) -> Result<(), String> {
    let mut db = test_db();
    let (lib_file, top_file) = (FileId::from_raw(0), FileId::from_raw(1));
    put_root(&mut db, 0, &[(top_file, "/top/lib.rs", "pub fn caller() {}")]);
    put_root(&mut db, 1, &[(lib_file, "/lib/lib.rs", "pub fn callee() {}")]);
    db.plant(plant);
    let mut graph = CrateGraphBuilder::default();
    let lib = add(&mut graph, lib_file, CfgOptions::default(), Env::default());
    graph.seal(lib, ContentId::of("base-db root sealed later test pin", b"lib"));
    let top = add(&mut graph, top_file, CfgOptions::default(), Env::default());
    depend(&mut graph, top, lib, "lib");
    let built = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        db.commit_with(|edit| {
            edit.set_crate_graph(graph);
        })
    }));
    let committed = built.map_err(|payload| match payload.downcast_ref::<String>() {
        Some(text) => text.clone(), // clone: the panic's text, for the oracle
        None => "a commit panicked with no text".to_owned(),
    })?;
    // Roots are at 2 and 3 (session 0, list 1): root 1's, which `lib` is rooted in, is 3.
    assert!(committed.dropped.contains(&3), "the root sealed now was made again: {:?}", committed.dropped);
    assert!(!committed.dropped.contains(&0), "sealing a root dropped the session: {:?}", committed.dropped);
    Ok(())
}

/// The invariant: an arena whose seal moved since it was made (a source root a sealed crate
/// came to be rooted in, with no write of the root) is made again with its new seal, so no
/// sealed arena borrows an unsealed one, and nothing else is dropped for it. While the crate
/// list was the session's, every crate listed dropped every arena and hid this. The control:
/// with `Plant::SealCarried` (the seal not compared) the snapshot's build refuses the sealed
/// `lib` borrowing its unsealed root.
#[test]
fn a_root_sealed_after_it_was_made_is_made_again_sealed() {
    a_root_sealed_after_it_was_made(Plant::None).unwrap_or_else(|why| panic!("{why}"));
    let planted = a_root_sealed_after_it_was_made(Plant::SealCarried).expect_err("a root carried with its old seal was not caught");
    assert!(planted.contains("is sealed but its dependency"), "{planted}");
}

/// [`two_roots`] with `a`'s and `c`'s arenas holding a query's value, then a crate `d` added
/// to the list under `plant`, on `c`'s root file with another cfg (a second crate of one
/// package: `check` after `run` adds the test crate beside the binary): the commit must keep
/// the session's arena, the roots', `a`'s and `c`'s (neither depends on `d`).
fn a_crate_added(plant: Plant) -> Result<(), String> {
    let mut db = test_db();
    let (a, c, _) = two_roots(&mut db);
    evaluate_in(&db, &[a, c]);
    db.plant(plant);
    let before = [arena_of(&db, a), arena_of(&db, c)];
    let (fa, fc) = (FileId::from_raw(0), FileId::from_raw(1));
    let mut graph = CrateGraphBuilder::default();
    add(&mut graph, fa, CfgOptions::default(), Env::default());
    add(&mut graph, fc, CfgOptions::default(), Env::default());
    let d = add(&mut graph, fc, cfg(&[flag("test")]), Env::default());
    let committed = db.commit_with(|edit| {
        edit.set_crate_graph(graph);
    });
    let d = committed.crates[&d];
    let kept = |index: u32| !committed.dropped.contains(&index);
    if !(kept(0) && kept(before[0]) && kept(before[1])) {
        return Err(format!(
            "adding `d` dropped arenas {:?}: the session's (0), `a`'s ({}) or `c`'s ({})",
            committed.dropped, before[0], before[1]
        ));
    }
    assert_eq!([arena_of(&db, a), arena_of(&db, c)], before, "`a` and `c` kept their arenas");
    assert!(crate::all_crates(&db).contains(&db.krate(d)), "`d` is listed");
    Ok(())
}

/// The invariant: a crate added to the list keeps every arena that does not depend on it (the
/// session's, the roots', every other crate's); what moves is the list's own arena, and a
/// crate whose root's other `OUT_DIR`s moved. The control: with `Plant::ListMarksSession` (the
/// list a singleton of the session's, round 27's `check` after `run`: the second `hi` crate
/// added at index 32 dropped all 32 arenas) the oracle names the drop.
#[test]
fn a_crate_added_to_the_list_keeps_every_arena_not_depending_on_it() {
    a_crate_added(Plant::None).unwrap_or_else(|why| panic!("{why}"));
    let planted = a_crate_added(Plant::ListMarksSession).expect_err("a crate added that dropped the session was not caught");
    assert!(planted.contains("dropped arenas"), "{planted}");
}

/// The invariant: a crate's other `OUT_DIR`s are those of the listed crates of its root, its
/// own left out, sorted, each once, as the query read them off the list; a crate of its root
/// given an `OUT_DIR` marks its arena (its field moved) and not the arena of a crate on
/// another root. The control is the same write read back on `a`, whose root holds no other
/// crate: its dirs stay empty and its arena is kept.
#[test]
fn a_crates_other_out_dirs_are_its_roots_others_and_move_its_arena_alone() {
    let mut db = test_db();
    let (a, c, _) = two_roots(&mut db);
    let fc = FileId::from_raw(1);
    let out = |dir: &str| -> Env { [("OUT_DIR".to_owned(), dir.to_owned())].into_iter().collect() };
    let mut graph = CrateGraphBuilder::default();
    add(&mut graph, FileId::from_raw(0), CfgOptions::default(), Env::default());
    add(&mut graph, fc, CfgOptions::default(), Env::default());
    let d = add(&mut graph, fc, cfg(&[flag("test")]), out("/target/out/d/"));
    let d = set_graph(&mut db, graph)[&d];
    fn dirs(db: &Db, key: CrateKey) -> &[&str] {
        crate::other_out_dirs(db, db.krate(key))
    }
    assert_eq!(dirs(&db, c), ["/target/out/d"], "`d`'s, its trailing separator gone");
    assert!(dirs(&db, d).is_empty(), "its own left out");
    assert!(dirs(&db, a).is_empty(), "another root's");
    let before = [arena_of(&db, a), arena_of(&db, c)];
    let committed = db.commit_with(|edit| {
        edit.set_crate_env(d, out("/target/out/d2"));
    });
    assert!(committed.dropped.contains(&before[1]), "`c`'s dirs moved: {committed:?}");
    assert!(!committed.dropped.contains(&before[0]), "`a` is on another root: {committed:?}");
    assert_eq!(dirs(&db, c), ["/target/out/d2"]);
}
