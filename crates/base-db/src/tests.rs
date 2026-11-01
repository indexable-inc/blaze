use std::{
    cell::Cell,
    sync::atomic::{AtomicUsize, Ordering},
};

use blaze_ids::rng::SplitMix64 as Rng;
use cfg::{CfgAtom, CfgOptions};
use intern::Symbol;
use blaze_query::db::Durability;
use span::{Edition, TextSize};
use syntax::TextRange;
use triomphe::Arc;
use crate::{AbsPathBuf, AnchoredPath, FileId, FileSet, FileSetConfig, VfsPath};

use crate::{
    BuiltDependency, ContentId, Crate, CrateBuilderId, CrateGraphBuilder, CrateName, CrateOrigin,
    CrateType, CrateTypes, CrateWorkspaceData, Db, DependencyBuilder, EditionedFileId, Env,
    FileText, SourceRoot, SourceRootId, TargetKind, ViewChange, ViewError, ViewFile, ViewRoot,
    all_crates_content, cfg_options_content,
    reparse::{
        REPARSED, TextEdit, assert_whole, common_prefix, common_prefix_scalar, common_suffix,
        common_suffix_scalar,
    },
    target::{Arch, TargetData},
};

mod memo_properties;

#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

fn revision(db: &Db) -> blaze_query::db::View {
    db.nonce_and_revision().1
}

/// Puts `files` (path, text) in source root 0, as file IDs from `first` on.
fn load(db: &mut Db, first: u32, files: &[(&str, &str)]) -> Vec<FileId> {
    let mut set = FileSet::default();
    let mut ids = Vec::new();
    for (n, (path, text)) in files.iter().enumerate() {
        let id = FileId::from_raw(first + n as u32);
        db.set_file_text_with_durability(id, text, Durability::LOW);
        db.set_file_source_root_with_durability(id, SourceRootId(0), Durability::MEDIUM);
        set.insert(id, VfsPath::new_virtual_path(path.to_string()));
        ids.push(id);
    }
    let root = Arc::new(SourceRoot::new_local(set));
    db.set_source_root_with_durability(SourceRootId(0), root, Durability::MEDIUM);
    ids
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
        Arc::new(AbsPathBuf::assert_utf8(std::env::temp_dir())),
        Arc::new(CrateWorkspaceData {
            target: Err("no target in tests".into()),
            toolchain: None,
            const_eval: Default::default(),
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
        let mut db = Db::for_tests();
        let files = load(&mut db, first, FILES);
        let mut graph = CrateGraphBuilder::default();
        let a = add(&mut graph, files[0], cfg(&[flag("unix")]), Env::default());
        let c = add(&mut graph, files[2], cfg(&[flag("unix")]), Env::default());
        depend(&mut graph, a, c, "c");
        let crates = graph.set_in_db(&mut db);
        (
            db.file_text(files[1]).content(&db),
            db.source_root(SourceRootId(0)).content(&db),
            crates[&a].content(&db),
            crates[&c].content(&db),
            all_crates_content(&db),
        )
    };
    // The file IDs differ between the two; nothing of them is hashed.
    assert_eq!(ids(0), ids(40));
}

#[test]
fn a_file_text_edit_changes_its_id_and_its_source_roots() {
    let mut db = Db::for_tests();
    let files = load(&mut db, 0, FILES);
    let ids = |db: &Db| {
        (db.file_text(files[1]).content(db), db.source_root(SourceRootId(0)).content(db))
    };
    let before = ids(&db);
    assert_eq!(before.0, FileText::content_of("pub fn f() {}", None));

    db.set_file_text(files[1], "pub fn g() {}");
    let edited = ids(&db);
    assert_ne!(edited.0, before.0);
    assert_ne!(edited.1, before.1);
    assert_eq!(db.file_text(files[0]).content(&db), FileText::content_of("pub mod b;", None));

    db.set_file_text(files[1], "pub fn f() {}");
    assert_eq!(ids(&db), before);

    let root = db.source_root(SourceRootId(0)).content(&db);
    db.set_appended_root_module(files[0], Some("mod __harness {}"), Durability::LOW);
    assert_eq!(
        db.file_text(files[0]).content(&db),
        FileText::content_of("pub mod b;", Some("mod __harness {}"))
    );
    assert_ne!(db.source_root(SourceRootId(0)).content(&db), root);
}

#[test]
fn a_files_parse_shares_its_text() {
    let mut db = Db::for_tests();
    let files = load(&mut db, 0, FILES);
    let parse = EditionedFileId::new(&db, files[1], Edition::Edition2021).parse(&db);
    let tree = parse.root().file();
    assert!(tree.source().ptr_eq(db.file_text(files[1]).text(&db)));
}

/// #1294: a dependency's file is an outline, whose `fn` bodies stay unparsed until reached.
#[test]
fn a_library_files_parse_leaves_bodies_unparsed() {
    let mut db = Db::for_tests();
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
    db.set_source_root_with_durability(SourceRootId(0), root, Durability::HIGH);
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
        let mut db = Db::for_tests();
        let files = load(&mut db, 0, FILES);
        let mut graph = CrateGraphBuilder::default();
        let a = add(&mut graph, files[0], cfg(atoms), Env::default());
        graph.set_in_db(&mut db)[&a].content(&db)
    };
    assert_eq!(crate_id(&atoms), crate_id(&reversed));
    assert_ne!(crate_id(&atoms), crate_id(&atoms[1..]));
}

#[test]
fn a_dependencys_change_changes_its_dependents_ids() {
    let mut db = Db::for_tests();
    let files = load(&mut db, 0, FILES);
    let mut graph = CrateGraphBuilder::default();
    let a = add(&mut graph, files[0], CfgOptions::default(), Env::default());
    let b = add(&mut graph, files[2], CfgOptions::default(), Env::default());
    let c = add(&mut graph, files[1], CfgOptions::default(), Env::default());
    depend(&mut graph, a, b, "b");
    let crates = graph.set_in_db(&mut db);
    let (a, b, c): (Crate, Crate, Crate) = (crates[&a], crates[&b], crates[&c]);
    let ids = |db: &Db| (a.content(db), b.content(db), c.content(db), all_crates_content(db));
    let before = ids(&db);

    let env: Env = [("KEY".to_owned(), "value".to_owned())].into_iter().collect();
    b.set_env(&mut db, env.clone());
    assert_eq!(b.env(&db), &env);
    let after = ids(&db);
    assert_ne!(after.1, before.1);
    assert_ne!(after.0, before.0);
    assert_eq!(after.2, before.2);
    assert_ne!(after.3, before.3);

    b.set_env(&mut db, Env::default());
    assert_eq!(ids(&db), before);
}

#[test]
fn a_crates_types_read_back_and_are_in_its_id() {
    let mut db = Db::for_tests();
    let files = load(&mut db, 0, FILES);
    let mut graph = CrateGraphBuilder::default();
    let a = add(&mut graph, files[0], CfgOptions::default(), Env::default());
    let a = graph.set_in_db(&mut db)[&a];
    assert_eq!(a.data(&db).crate_types, CrateTypes::of(&[CrateType::Lib]));
    let as_lib = a.content(&db);

    let mut data = a.data(&db).clone();
    data.crate_types = CrateTypes::of(&[CrateType::Cdylib, CrateType::Rlib]);
    a.set_data(&mut db, data);
    let types = a.data(&db).crate_types;
    assert_eq!(types.iter().collect::<Vec<_>>(), [CrateType::Rlib, CrateType::Cdylib]);
    assert!(!types.contains(CrateType::Lib));
    assert_ne!(a.content(&db), as_lib);
}

#[test]
fn a_dependency_added_outside_the_prelude_says_so() {
    let mut db = Db::for_tests();
    let files = load(&mut db, 0, FILES);
    let mut graph = CrateGraphBuilder::default();
    let a = add(&mut graph, files[0], CfgOptions::default(), Env::default());
    let b = add(&mut graph, files[2], CfgOptions::default(), Env::default());
    let crates = graph.set_in_db(&mut db);
    let (a, b): (Crate, Crate) = (crates[&a], crates[&b]);
    let name = CrateName::new("b").unwrap();
    let depend_on = |db: &mut Db, dependency: BuiltDependency| {
        let mut data = a.data(&*db).clone();
        data.dependencies = vec![dependency];
        a.set_data(db, data);
        a.content(&*db)
    };

    let in_prelude = depend_on(&mut db, BuiltDependency::new(name.clone(), b));
    let outside = depend_on(&mut db, BuiltDependency::with_prelude(name.clone(), b, false, true));
    let dependency = &a.data(&db).dependencies[0];
    assert!(!dependency.is_prelude());
    assert!(dependency.is_sysroot());
    assert_ne!(outside, in_prelude);
    assert_eq!(depend_on(&mut db, BuiltDependency::with_prelude(name, b, true, false)), in_prelude);
}

/// The texts of `krate`'s crate attributes' files, in order.
fn attr_texts(db: &Db, krate: Crate) -> Vec<String> {
    let text = |file: &FileId| -> String { (**db.file_text(*file).text(db)).to_owned() };
    krate.crate_attr_files(db).iter().map(text).collect()
}

/// A graph with crate `a` on `FILES`' first file, its crate attributes `attrs`.
fn set_attrs(db: &mut Db, files: &[FileId], attrs: &[&str]) -> Crate {
    let mut graph = CrateGraphBuilder::default();
    let a = add_with_attrs(&mut graph, files[0], CfgOptions::default(), Env::default(), attrs);
    graph.set_in_db(db)[&a]
}

#[test]
fn each_crate_attr_is_a_file_of_its_own_in_the_roots_source_root() {
    let mut db = Db::for_tests();
    let files = load(&mut db, 0, FILES);
    let a = set_attrs(&mut db, &files, &["no_std", "feature(x)"]);
    assert_eq!(attr_texts(&db, a), ["#![no_std]", "#![feature(x)]"]);
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

#[test]
fn an_equal_graph_keeps_the_crate_attr_files_and_writes_nothing() {
    let mut db = Db::for_tests();
    let files = load(&mut db, 0, FILES);
    let a = set_attrs(&mut db, &files, &["no_std", "feature(x)"]);
    let before = a.crate_attr_files(&db).to_vec();
    let then = revision(&db);
    assert_eq!(set_attrs(&mut db, &files, &["no_std", "feature(x)"]), a);
    assert_eq!(a.crate_attr_files(&db), before);
    assert_eq!(revision(&db), then);
}

#[test]
fn a_changed_crate_attr_reuses_a_file_and_a_gone_one_is_taken_out() {
    let mut db = Db::for_tests();
    let files = load(&mut db, 0, FILES);
    let a = set_attrs(&mut db, &files, &["no_std", "feature(x)"]);
    let first = a.crate_attr_files(&db).to_vec();

    // `feature(x)` keeps its file; `feature(y)` gets the one `no_std` left.
    assert_eq!(set_attrs(&mut db, &files, &["feature(x)", "feature(y)"]), a);
    assert_eq!(a.crate_attr_files(&db), [first[1], first[0]]);
    assert_eq!(attr_texts(&db, a), ["#![feature(x)]", "#![feature(y)]"]);

    assert_eq!(set_attrs(&mut db, &files, &["feature(y)"]), a);
    assert_eq!(a.crate_attr_files(&db), [first[0]]);
    assert!(!db.has_file_text(first[1]), "the file of a gone attribute is taken out");

    // The ID taken out is the next attribute's.
    assert_eq!(set_attrs(&mut db, &files, &["feature(y)", "no_core"]), a);
    assert_eq!(a.crate_attr_files(&db), [first[0], first[1]]);
    assert_eq!(attr_texts(&db, a), ["#![feature(y)]", "#![no_core]"]);
}

#[test]
fn a_crate_that_goes_takes_its_crate_attr_files_out() {
    let mut db = Db::for_tests();
    let files = load(&mut db, 0, FILES);
    let a = set_attrs(&mut db, &files, &["no_std"]);
    let attr = a.crate_attr_files(&db)[0];
    let mut graph = CrateGraphBuilder::default();
    add(&mut graph, files[2], CfgOptions::default(), Env::default());
    graph.set_in_db(&mut db);
    assert!(a.crate_attr_files(&db).is_empty());
    assert!(!db.has_file_text(attr));
}

#[test]
#[should_panic(expected = "is the file of a crate attribute")]
fn the_vfs_setters_refuse_a_crate_attr_file() {
    let mut db = Db::for_tests();
    let files = load(&mut db, 0, FILES);
    let a = set_attrs(&mut db, &files, &["no_std"]);
    let attr = a.crate_attr_files(&db)[0];
    db.set_file_text(attr, "fn f() {}");
}

#[test]
fn a_new_file_id_meets_no_vfs_file_nor_crate_attr_file() {
    let mut db = Db::for_tests();
    let files = load(&mut db, 0, FILES);
    let given = db.new_file_id().expect("an unused file ID");
    assert!(!files.contains(&given), "{given:?} is a file of the VFS");
    let a = set_attrs(&mut db, &files, &["no_std", "feature(x)"]);
    let attrs = a.crate_attr_files(&db).to_vec();
    assert!(!attrs.contains(&given), "a crate attribute took {given:?}");
    let after = db.new_file_id().expect("another unused file ID");
    assert!(after != given && !attrs.contains(&after) && !files.contains(&after));

    // A reload's new files, set after the graph, are not refused as an attribute's.
    db.set_file_text(given, "fn f() {}");
    db.set_file_text(after, "fn g() {}");
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
    let mut db = Db::for_tests();
    let files = load(&mut db, 0, FILES);
    let attrs: Vec<&str> = cases.iter().map(|(attr, _)| *attr).collect();
    let a = set_attrs(&mut db, &files, &attrs);
    let attr_files = crate::crate_attr_files(&db, a);
    let mut applied = Vec::new();
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
    let base = AbsPathBuf::assert_utf8(std::env::temp_dir().join("blaze-base-db-view"));
    let mut partition = FileSetConfig::builder();
    partition.add_file_set(vec![VfsPath::from(base.clone())]);
    for root in [0, 1] {
        let empty = Arc::new(SourceRoot::new_local(FileSet::default()));
        db.set_source_root_with_durability(SourceRootId(root), empty, Durability::MEDIUM);
    }
    (base, partition.build())
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
    let bytes: Vec<blaze_vfs::Bytes> = files.iter().map(|&(_, bytes)| vfs_bytes(bytes)).collect();
    let files: Vec<ViewFile<'_>> =
        files.iter().zip(&bytes).map(|(&(path, _), bytes)| view_file(path, bytes)).collect();
    db.set_view_root(ViewRoot { base, built: &[], partition, other: SourceRootId(1), files: &files })
}

fn file_at(db: &Db, base: &AbsPathBuf, path: &str) -> Option<FileId> {
    let root = db.source_root(SourceRootId(0)).source_root(db);
    root.file_for_path(&VfsPath::from(base.join(path))).copied()
}

const VIEW: &[(&str, &[u8])] =
    &[("src/lib.rs", b"mod a;"), ("src/a.rs", b"fn f() {}"), ("README.md", b"hello")];

#[test]
fn setting_a_view_twice_writes_nothing() {
    let mut db = Db::for_tests();
    let (base, partition) = checkout(&mut db);
    let first = set_view(&mut db, &base, &partition, VIEW).unwrap();
    assert_eq!(first.added.len(), 3);
    assert_eq!(first.roots, vec![SourceRootId(0)]);
    let a = file_at(&db, &base, "src/a.rs").unwrap();
    assert_eq!(&**db.file_text(a).text(&db), "fn f() {}");
    assert_eq!(db.file_source_root(a).source_root_id(&db), SourceRootId(0));

    let then = revision(&db);
    let root = db.source_root(SourceRootId(0)).content(&db);
    let second = set_view(&mut db, &base, &partition, VIEW).unwrap();
    assert_eq!(second, ViewChange::default());
    assert_eq!(revision(&db), then);
    assert_eq!(db.source_root(SourceRootId(0)).content(&db), root);
}

#[test]
fn a_views_text_is_its_bytes() {
    let mut db = Db::for_tests();
    let (base, partition) = checkout(&mut db);
    let bytes = vfs_bytes(b"fn f() {}");
    let files = [view_file("src/lib.rs", &bytes)];
    let view =
        ViewRoot { base: &base, built: &[], partition: &partition, other: SourceRootId(1), files: &files };
    db.set_view_root(view).unwrap();
    let file = file_at(&db, &base, "src/lib.rs").unwrap();
    let text = db.file_text(file).text(&db);
    assert!(std::ptr::eq(text.as_ptr(), bytes.as_ptr()), "a copy of the view's bytes");
}

#[test]
fn a_view_with_one_changed_file_sets_that_one() {
    let mut db = Db::for_tests();
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
    let mut db = Db::for_tests();
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
    let mut db = Db::for_tests();
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

#[test]
fn a_content_id_that_is_not_its_bytes_is_refused() {
    let mut db = Db::for_tests();
    let (base, partition) = checkout(&mut db);
    let then = revision(&db);
    let (lib, main) = (vfs_bytes(b"mod a;"), vfs_bytes(b"fn main() {}"));
    let files = [
        view_file("src/lib.rs", &lib),
        ViewFile {
            path: "src/main.rs",
            id: ContentId::of(blaze_vfs::FILE_DOMAIN, b"other"),
            bytes: &main,
        },
    ];
    let view =
        ViewRoot { base: &base, built: &[], partition: &partition, other: SourceRootId(1), files: &files };
    let error = db.set_view_root(view).unwrap_err();
    assert!(matches!(&error, ViewError::Mismatch { path, .. } if path == "src/main.rs"));
    assert_eq!(revision(&db), then);
    assert_eq!(file_at(&db, &base, "src/lib.rs"), None);
}

/// [`VIEW`] set, and an alias of its `src/lib.rs`: (base, partition, lib, alias).
fn aliased_view(db: &mut Db) -> (AbsPathBuf, FileSetConfig, FileId, FileId) {
    let (base, partition) = checkout(db);
    set_view(db, &base, &partition, VIEW).unwrap();
    let lib = file_at(db, &base, "src/lib.rs").unwrap();
    let alias = db.alias(lib).unwrap();
    (base, partition, lib, alias)
}

#[test]
fn an_alias_is_found_by_its_file_never_by_its_path() {
    let mut db = Db::for_tests();
    let (base, partition) = checkout(&mut db);
    set_view(&mut db, &base, &partition, VIEW).unwrap();
    let lib = file_at(&db, &base, "src/lib.rs").unwrap();
    let a = file_at(&db, &base, "src/a.rs").unwrap();
    let content = db.source_root(SourceRootId(0)).content(&db);
    let alias = db.alias(lib).unwrap();
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
    assert_eq!(db.source_root(SourceRootId(0)).content(&db), content, "the root's files are as they were");

    assert_eq!(db.file_source_root(alias).source_root_id(&db), SourceRootId(0));
    assert_eq!(&**db.file_text(alias).text(&db), "mod a;");
    assert_eq!(db.file_text(alias).content(&db), db.file_text(lib).content(&db));
    assert_eq!(db.files().aliases_of(lib).as_slice(), &[alias]);
    assert_eq!(db.files().alias_of(alias), Some(lib));
    assert_eq!(db.files().alias_of(lib), None);
}

#[test]
fn a_view_set_again_keeps_every_alias() {
    let mut db = Db::for_tests();
    let (base, partition, lib, alias) = aliased_view(&mut db);
    let path = VfsPath::from(base.join("src/lib.rs"));
    // A view that adds a file rebuilds nothing of the alias, and one the same again writes nothing.
    let more: &[(&str, &[u8])] =
        &[("src/lib.rs", b"mod a;"), ("src/a.rs", b"fn f() {}"), ("README.md", b"hello"), ("src/b.rs", b"")];
    for view in [VIEW, more, VIEW, VIEW] {
        let change = set_view(&mut db, &base, &partition, view).unwrap();
        assert!(!change.removed.contains(&alias), "{change:?}");
        let root = db.source_root(SourceRootId(0)).source_root(&db);
        assert_eq!(root.path_for_file(&alias), Some(&path));
        assert_eq!(root.file_for_path(&path), Some(&lib));
        assert_eq!(&**db.file_text(alias).text(&db), "mod a;");
    }
    let then = revision(&db);
    assert_eq!(set_view(&mut db, &base, &partition, VIEW).unwrap(), ViewChange::default());
    assert_eq!(revision(&db), then);
}

#[test]
fn an_edit_of_a_root_reaches_every_alias_of_it() {
    let mut db = Db::for_tests();
    let (base, partition, lib, first) = aliased_view(&mut db);
    let second = db.alias(lib).unwrap();
    assert_eq!(db.files().aliases_of(lib).as_slice(), &[first, second]);
    db.set_alias_appended(second, Some(("__harness", "\nmod __harness {}"))).unwrap();

    let edited: &[(&str, &[u8])] =
        &[("src/lib.rs", b"mod a; mod b;"), ("src/a.rs", b"fn f() {}"), ("README.md", b"hello")];
    let change = set_view(&mut db, &base, &partition, edited).unwrap();
    assert_eq!(change.set, vec![lib, first, second], "the aliases are written in the same change");
    assert_eq!(&**db.file_text(first).text(&db), "mod a; mod b;");
    assert_eq!(db.file_text(first).content(&db), FileText::content_of("mod a; mod b;", None));
    let appended = "mod a; mod b;\nmod __harness {}";
    assert_eq!(&**db.file_text(second).text(&db), appended, "its suffix is kept after the new text");
    assert_eq!(db.file_text(second).appended_root_module(&db), Some("__harness"));
    assert_eq!(db.file_text(second).content(&db), FileText::content_of(appended, Some("__harness")));
    assert_eq!(&**db.file_text(lib).text(&db), "mod a; mod b;");
    assert_eq!(db.file_text(lib).appended_root_module(&db), None);

    db.set_file_text(lib, "mod c;");
    assert_eq!(&**db.file_text(first).text(&db), "mod c;");
    assert_eq!(&**db.file_text(second).text(&db), "mod c;\nmod __harness {}");
    assert_eq!(db.file_text(second).appended_root_module(&db), Some("__harness"));
}

#[test]
fn an_alias_owns_its_suffix() {
    let mut db = Db::for_tests();
    let (_, _, lib, alias) = aliased_view(&mut db);
    let shared = db.file_text(lib).text(&db).as_ptr();
    assert!(std::ptr::eq(db.file_text(alias).text(&db).as_ptr(), shared), "with no suffix it shares the bytes");

    db.set_alias_appended(alias, Some(("__list", "\nmod __list {}"))).unwrap();
    assert_eq!(&**db.file_text(alias).text(&db), "mod a;\nmod __list {}");
    assert_eq!(db.file_text(alias).appended_root_module(&db), Some("__list"));
    assert_eq!(&**db.file_text(lib).text(&db), "mod a;", "the file is as on disk");
    let then = revision(&db);
    db.set_alias_appended(alias, Some(("__list", "\nmod __list {}"))).unwrap();
    assert_eq!(revision(&db), then, "the same suffix again writes nothing");

    db.set_alias_appended(alias, None).unwrap();
    assert_eq!(&**db.file_text(alias).text(&db), "mod a;");
    assert_eq!(db.file_text(alias).appended_root_module(&db), None);
    assert_eq!(db.file_text(alias).content(&db), db.file_text(lib).content(&db));
    assert_eq!(db.set_alias_appended(lib, None), Err(crate::AliasError::NotAnAlias(lib)));
}

#[test]
#[should_panic = "set_alias_appended"]
fn an_aliass_module_is_set_with_its_suffix() {
    let mut db = Db::for_tests();
    let (_, _, _, alias) = aliased_view(&mut db);
    db.set_appended_root_module(alias, Some("__harness"), Durability::LOW);
}

#[test]
fn a_file_gone_takes_its_aliases_with_it() {
    let mut db = Db::for_tests();
    let (base, partition, lib, alias) = aliased_view(&mut db);
    let change = set_view(&mut db, &base, &partition, &VIEW[1..]).unwrap();
    let mut removed = vec![lib, alias];
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
    let again = db.alias(lib).unwrap();
    assert_ne!(again, alias, "a gone alias's ID keeps its parked text, so it is not handed out again");
}

#[test]
fn a_root_set_whole_keeps_its_aliases() {
    let mut db = Db::for_tests();
    let files = load(&mut db, 0, FILES);
    let alias = db.alias(files[0]).unwrap();
    // `load` again sets source root 0 to a set made from scratch.
    load(&mut db, 0, FILES);
    let root = db.source_root(SourceRootId(0)).source_root(&db);
    assert_eq!(root.path_for_file(&alias), root.path_for_file(&files[0]));
    assert_eq!(root.resolve_path(AnchoredPath { anchor: alias, path: "b.rs" }), Some(files[1]));
    assert_eq!(db.files().aliases_of(files[0]).as_slice(), &[alias]);
}

#[test]
fn an_alias_is_of_a_file_with_a_path() {
    let mut db = Db::for_tests();
    let (_, _, lib, alias) = aliased_view(&mut db);
    assert_eq!(db.alias(alias), Err(crate::AliasError::OfAnAlias(alias)));
    let nowhere = db.new_file_id().unwrap();
    assert_eq!(db.alias(nowhere), Err(crate::AliasError::NotInASourceRoot(nowhere)));
    assert_eq!(db.files().aliases_of(lib).as_slice(), &[alias]);
}

#[test]
#[should_panic = "is an alias of"]
fn an_aliass_text_is_its_files() {
    let mut db = Db::for_tests();
    let (_, _, _, alias) = aliased_view(&mut db);
    db.set_file_text(alias, "fn other() {}");
}

#[test]
fn a_table_finds_what_a_hash_map_does() {
    use crate::table::{Table, hash_of};
    use blaze_hash::HashMap;

    let table: Table<Box<str>> = Table::new(1 << 16);
    let mut model: HashMap<String, u32> = HashMap::default();
    let mut order = Vec::new();
    let mut rng = Rng::new(7);
    for _ in 0..20_000 {
        let key = format!("key {}", rng.below(5_000));
        let hash = hash_of(key.as_str());
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
        assert_eq!(table.find(hash_of(key.as_str()), |it| **it == **key), Some(at));
    }
    assert_eq!(table.find(hash_of("absent"), |it| &**it == "absent"), None);
    let kept: Vec<&str> = table.iter().map(|it| &**it).collect();
    assert_eq!(kept, order.iter().map(String::as_str).collect::<Vec<_>>());
}

#[test]
fn threads_inserting_one_key_get_one_value() {
    use crate::table::{Table, hash_of};

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
                        let (got, _) = table.insert(hash_of(key), |it| *it == key, || key);
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
        let at = table.find(hash_of(key), |it| *it == key);
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
fn a_log_and_a_column_give_back_what_was_put() {
    use crate::table::{Column, Log};
    use blaze_query::db::Id;

    let log: Log<u32> = Log::new(1 << 16);
    for value in 0..5_000u32 {
        assert_eq!(log.push(value * 3), value);
    }
    for value in 0..5_000u32 {
        assert_eq!(log.get(value), Some(&(value * 3)));
    }
    assert_eq!(log.get(5_000), None);
    assert!(log.iter().copied().eq((0..5_000).map(|it| it * 3)));

    let column: Column<Id> = Column::new();
    let mut model = std::collections::BTreeMap::new();
    let mut rng = Rng::new(11);
    for _ in 0..10_000 {
        let raw = rng.below(1 << 16) as u32;
        if rng.below(4) == 0 {
            column.set(raw, None);
            model.remove(&raw);
        } else {
            let id = Id::from_u32(rng.below(u64::from(Id::MAX_U32)) as u32 + 1);
            column.set(raw, Some(id));
            model.insert(raw, id);
        }
    }
    for raw in 0..1 << 16 {
        assert_eq!(column.get(raw), model.get(&raw).copied(), "{raw}");
    }
    assert_eq!(column.get(Column::<Id>::MAX + 1), None);
    assert!(column.iter().eq(model.iter().map(|(&raw, &id)| (raw, id))));
}

#[test]
fn what_queries_note_is_kept_once_and_sorted() {
    let mut db = Db::for_tests();
    let files = load(&mut db, 0, FILES);
    let mut graph = CrateGraphBuilder::default();
    let a = add(&mut graph, files[0], CfgOptions::default(), Env::default());
    let c = add(&mut graph, files[2], CfgOptions::default(), Env::default());
    let crates = graph.set_in_db(&mut db);
    let (a, c) = (crates[&a], crates[&c]);
    for (krate, key) in [(c, "B"), (a, "B"), (a, "A"), (c, "B"), (a, "B")] {
        db.files().env_is_read(krate, key);
    }
    db.files().env_is_unset(a, "X");
    db.files().env_is_unset(a, "X");
    let mut expected = vec![(a, "A".to_owned()), (a, "B".to_owned()), (c, "B".to_owned())];
    expected.sort();
    assert_eq!(db.files().env_read(), expected);
    assert_eq!(db.files().env_unset(), vec![(a, "X".to_owned())]);

    let dir = AbsPathBuf::assert_utf8(std::env::temp_dir());
    let (x, y) = (dir.join("blaze-base-db-x.rs"), dir.join("blaze-base-db-y.rs"));
    for (path, krate) in [(&y, a), (&x, c), (&y, a), (&y, c)] {
        db.files().include_source(path, SourceRootId(0), krate);
    }
    assert_eq!(
        db.files().included_sources(),
        vec![(x.clone(), SourceRootId(0)), (y.clone(), SourceRootId(0))]
    );
    let mut named = vec![(a, y.clone()), (c, x.clone()), (c, y.clone())];
    named.sort();
    assert_eq!(db.files().sources_included_by(), named);
}

/// A file `include_bytes!` names that can't be read keeps why, in `std::io::Error`'s words
/// (rustc's "couldn't read" message prints them), and a different error is a different input.
#[test]
fn an_unreadable_included_file_keeps_its_error() {
    let mut db = Db::for_tests();
    let files = load(&mut db, 0, FILES);
    let mut graph = CrateGraphBuilder::default();
    let a = add(&mut graph, files[0], CfgOptions::default(), Env::default());
    let a = graph.set_in_db(&mut db)[&a];
    let dir = AbsPathBuf::assert_utf8(std::env::temp_dir());
    let missing = dir.join("blaze-base-db-unreadable-include.rs");
    let _ = std::fs::remove_file(&missing);
    let read = |path: &AbsPathBuf| {
        let file = db.files().included_file(&db, a, path, Durability::LOW);
        (file.read(&db).clone(), file.content(&db))
    };
    let expected = |path: &AbsPathBuf| std::fs::read(path).unwrap_err().to_string();
    let (missing_read, missing_content) = read(&missing);
    let (dir_read, dir_content) = read(&dir);
    assert_eq!(missing_read.unwrap_err().to_string(), expected(&missing));
    assert_eq!(dir_read.unwrap_err().to_string(), expected(&dir));
    assert_ne!(missing_content, dir_content, "a different error is a different input");
    assert!(db.files().included_is_read(&db, &missing), "the same error is what was read");
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
    let mut db = Db::for_tests();
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
        let root = ViewRoot { base: &base, built: &built, partition: &partition, other: SourceRootId(1), files: &files };
        let change = db.set_view_root(root).unwrap();

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
                    if was != bytes {
                        let both_binary =
                            std::str::from_utf8(was).is_err() && std::str::from_utf8(bytes).is_err();
                        if !both_binary {
                            set.push(id);
                        }
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
        assert_eq!((change.set, change.added, change.no_text, change.removed), (set, added, no_text, removed), "step {step}");

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
        let mut fresh = Db::for_tests();
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
fn a_task_that_waited_on_itself_runs_again() {
    let db = Db::new();
    let pool = two_workers();
    let runs: Vec<AtomicUsize> = (0..8).map(|_| AtomicUsize::new(0)).collect();
    pool.install(|| {
        db.run_all(blaze_mem::Tag::Parse, runs.len(), |_: &Db, at: usize| {
            if runs[at].fetch_add(1, Ordering::Relaxed) == 0 && at % 3 == 0 {
                std::panic::resume_unwind(Box::new(
                    blaze_query::db::Cancelled::WouldWaitOnItself { asked: None },
                ));
            }
        });
    });
    let runs: Vec<usize> = runs.iter().map(|it| it.load(Ordering::Relaxed)).collect();
    assert_eq!(runs, [2, 1, 1, 2, 1, 1, 2, 1]);
}

#[test]
fn any_other_panic_of_a_task_reaches_the_caller() {
    let db = Db::new();
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
        let mut db = Db::for_tests();
        let files = load(&mut db, 0, FILES);
        let mut graph = CrateGraphBuilder::default();
        let a = add(&mut graph, files[0], CfgOptions::default(), Env::default());
        let c = add(&mut graph, files[2], CfgOptions::default(), Env::default());
        let dependency = DependencyBuilder::new(CrateName::new("c").unwrap(), c).with_private(private);
        graph.add_dep(a, dependency).unwrap();
        let a = graph.set_in_db(&mut db)[&a];
        assert_eq!(a.data(&db).dependencies[0].is_private(), private);
        a.content(&db)
    };
    let public = crate_id(false);
    assert_ne!(crate_id(true), public);
    assert_eq!(crate_id(false), public);

    let mut db = Db::for_tests();
    let files = load(&mut db, 0, FILES);
    let mut graph = CrateGraphBuilder::default();
    let a = add(&mut graph, files[0], CfgOptions::default(), Env::default());
    let c = add(&mut graph, files[2], CfgOptions::default(), Env::default());
    let crates = graph.set_in_db(&mut db);
    let (a, c): (Crate, Crate) = (crates[&a], crates[&c]);
    let depend_on = |db: &mut Db, private: bool| {
        let mut data = a.data(&*db).clone();
        let name = CrateName::new("c").unwrap();
        data.dependencies = vec![BuiltDependency::new(name, c).with_private(private)];
        a.set_data(db, data);
        (a.data(&*db).dependencies[0].is_private(), a.content(&*db), all_crates_content(&*db))
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
        CrateWorkspaceData { target: Ok(target), toolchain: None, const_eval: Default::default() }
            .content()
    };
    assert_ne!(workspace(interworking.clone()), workspace(without.clone()));

    let crate_id = |target: TargetData| {
        let mut db = Db::for_tests();
        let files = load(&mut db, 0, FILES);
        let mut graph = CrateGraphBuilder::default();
        let a = add(&mut graph, files[0], CfgOptions::default(), Env::default());
        let a = graph.set_in_db(&mut db)[&a];
        let workspace = CrateWorkspaceData {
            target: Ok(target),
            toolchain: None,
            const_eval: Default::default(),
        };
        a.set_workspace_data(&mut db, Arc::new(workspace));
        a.content(&db)
    };
    assert_ne!(crate_id(interworking), crate_id(without));
}

/// Puts `files` (ID, path, text) in source root `root`.
fn put_root(db: &mut Db, root: u32, files: &[(FileId, &str, &str)]) {
    let mut set = FileSet::default();
    for &(id, path, text) in files {
        db.set_file_text_with_durability(id, text, Durability::LOW);
        db.set_file_source_root_with_durability(id, SourceRootId(root), Durability::MEDIUM);
        set.insert(id, VfsPath::new_virtual_path(path.to_owned()));
    }
    let set = Arc::new(SourceRoot::new_local(set));
    db.set_source_root_with_durability(SourceRootId(root), set, Durability::MEDIUM);
}

thread_local! {
    static SHARED_MODULE_RUNS: Cell<usize> = const { Cell::new(0) };
}

/// What a `#[path = "../../shared/m.rs"] mod m;` in `anchor` reads: the file and its text's
/// length, as a def map would.
#[blaze_query::tracked(returns(copy))]
fn shared_module(db: &Db, anchor: EditionedFileId) -> Option<(u32, u32)> {
    SHARED_MODULE_RUNS.with(|it| it.set(it.get() + 1));
    let (anchor, _) = anchor.unpack(db);
    let file = db.resolve_path(AnchoredPath { anchor, path: "../../shared/m.rs" })?;
    Some((file.index(), db.file_text(file).text(db).len() as u32))
}

fn shared_module_runs() -> usize {
    SHARED_MODULE_RUNS.with(Cell::get)
}

#[test]
fn a_path_module_outside_its_package_resolves_and_its_edit_reruns_the_reader() {
    let mut db = Db::for_tests();
    let (lib, own, m) = (FileId::from_raw(0), FileId::from_raw(1), FileId::from_raw(2));
    put_root(
        &mut db,
        0,
        &[(lib, "/ws/member/src/lib.rs", "#[path = \"../../shared/m.rs\"] mod m;"), (own, "/ws/member/src/a.rs", "")],
    );
    put_root(&mut db, 1, &[(m, "/ws/shared/m.rs", "pub fn f() {}")]);

    // The anchor's own source root first, as before.
    assert_eq!(db.resolve_path(AnchoredPath { anchor: lib, path: "a.rs" }), Some(own));
    let anchor = EditionedFileId::new(&db, lib, Edition::Edition2021);
    let runs = shared_module_runs();
    assert_eq!(shared_module(&db, anchor), Some((m.index(), 13)));
    assert_eq!(shared_module_runs(), runs + 1);
    assert_eq!(shared_module(&db, anchor), Some((m.index(), 13)));
    assert_eq!(shared_module_runs(), runs + 1);

    // An edit of the file in the other source root runs the reader again.
    db.set_file_text(m, "pub fn f() { 1 }");
    assert_eq!(shared_module(&db, anchor), Some((m.index(), 16)));
    assert_eq!(shared_module_runs(), runs + 2);

    // Taken out of its source root: found in none, and nothing read from disk.
    put_root(&mut db, 1, &[]);
    assert_eq!(shared_module(&db, anchor), None);
    assert_eq!(shared_module_runs(), runs + 3);

    // Named again by another source root, as another file: the miss runs again and finds it.
    let moved = FileId::from_raw(3);
    put_root(&mut db, 2, &[(moved, "/ws/shared/m.rs", "pub fn g() {}")]);
    assert_eq!(shared_module(&db, anchor), Some((moved.index(), 13)));
    assert_eq!(shared_module_runs(), runs + 4);
}

#[test]
fn a_path_no_source_root_names_is_found_once_one_does() {
    let mut db = Db::for_tests();
    let lib = FileId::from_raw(0);
    put_root(&mut db, 0, &[(lib, "/ws/member/src/lib.rs", "")]);
    let anchor = EditionedFileId::new(&db, lib, Edition::Edition2021);
    assert_eq!(shared_module(&db, anchor), None);
    let runs = shared_module_runs();

    // A source root that names the path: the miss runs again.
    let m = FileId::from_raw(1);
    put_root(&mut db, 1, &[(m, "/ws/shared/m.rs", "fn m() {}")]);
    assert_eq!(shared_module(&db, anchor), Some((m.index(), 9)));
    assert_eq!(shared_module_runs(), runs + 1);
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
        "", "x", " ", "\n", "{", "}", "(", ")", ";", "\"", "'a", "//", "/*", "*/", "é", "fn g() {}",
        "1 + 2", "struct S;", "let y = 0;", "mod m { }", "#[cfg(test)]",
    ];
    let boundaries: Vec<usize> =
        (0..=text.len()).filter(|&at| text.is_char_boundary(at)).collect();
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
    let same = TextEdit::between("fn f() {}", "fn f() {}", ContentId::of(blaze_vfs::FILE_DOMAIN, b""));
    assert_eq!((same.delete.len(), same.insert), (TextSize::from(0), TextSize::from(0)));
}

fn reparsed() -> usize {
    REPARSED.with(Cell::get)
}

/// The property test of db's decision D8: over random edits of real files, in full and as an
/// outline, each parse is the one a whole parse of the text makes, and most are reparses.
#[test]
fn a_parse_after_random_edits_is_the_whole_parse() {
    for (seed, source, library) in
        [(29, include_str!("target.rs"), false), (31, include_str!("change.rs"), false), (37, include_str!("target.rs"), true)]
    {
        let mut db = Db::for_tests();
        let file = FileId::from_raw(0);
        put_root(&mut db, 0, &[(file, "/a/lib.rs", source)]);
        if library {
            let mut set = FileSet::default();
            set.insert(file, VfsPath::new_virtual_path("/a/lib.rs".to_owned()));
            let root = Arc::new(SourceRoot::new_library(set));
            db.set_source_root_with_durability(SourceRootId(0), root, Durability::HIGH);
        }
        let id = EditionedFileId::new(&db, file, Edition::Edition2021);
        assert_eq!(id.parse(&db).is_outline(), library);
        let mut rng = Rng::new(seed);
        let mut text = source.to_owned();
        let start = reparsed();
        for step in 0..80 {
            text = random_edit(&mut rng, &text);
            db.set_file_text(file, &text);
            let parse = id.parse(&db);
            assert_eq!(parse.root().text(), text, "step {step}");
            assert_whole(parse, db.file_text(file).text(&db), Edition::Edition2021, library);
        }
        assert_eq!(reparsed() - start, 80, "seed {seed}: every edit after a kept parse reparses");
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
        let mut db = Db::for_tests();
        let file = FileId::from_raw(0);
        put_root(&mut db, 0, &[(file, "/a/lib.rs", before)]);
        let id = EditionedFileId::new(&db, file, Edition::Edition2021);
        let _ = id.parse(&db);
        db.set_file_text(file, after);
        let parse = id.parse(&db);
        assert_eq!(parse.root().text(), after, "edit {i}");
        assert_whole(parse, db.file_text(file).text(&db), Edition::Edition2021, false);
    }
}

#[test]
fn a_reparse_is_made_from_the_last_text_that_was_parsed() {
    let mut db = Db::for_tests();
    let file = FileId::from_raw(0);
    put_root(&mut db, 0, &[(file, "/a/lib.rs", "fn f() { 1 }\nstruct S;\n")]);
    let id = EditionedFileId::new(&db, file, Edition::Edition2021);
    let _ = id.parse(&db);
    let parsed = ContentId::of(blaze_vfs::FILE_DOMAIN, b"fn f() { 1 }\nstruct S;\n");

    // Two edits with no parse between: the second is one edit from the text last parsed.
    db.set_file_text(file, "fn f() { 1 + 2 }\nstruct S;\n");
    db.set_file_text(file, "fn f() { 1 + 2 }\nstruct S { a: u8 }\n");
    let edit = db.file_text(file).reparse_field(&db).edit().expect("an edit from the parse");
    assert_eq!(edit.before, parsed);
    assert_eq!(edit.delete, TextRange::new(TextSize::from(11), TextSize::from(22)));
    let start = reparsed();
    let parse = id.parse(&db);
    assert_eq!(reparsed(), start + 1);
    assert_whole(parse, db.file_text(file).text(&db), Edition::Edition2021, false);

    // A text never parsed before its edit, with none before it: parsed whole.
    let fresh = FileId::from_raw(1);
    put_root(&mut db, 0, &[(file, "/a/lib.rs", "fn f() {}"), (fresh, "/a/b.rs", "fn b() {}")]);
    db.set_file_text(fresh, "fn b() { 2 }");
    assert_eq!(db.file_text(fresh).reparse_field(&db).edit(), None);
    let start = reparsed();
    let fresh_id = EditionedFileId::new(&db, fresh, Edition::Edition2021);
    assert_eq!(fresh_id.parse(&db).root().text(), "fn b() { 2 }");
    assert_eq!(reparsed(), start);

    // Another edition's parse is not made from this one's.
    let other = EditionedFileId::new(&db, file, Edition::Edition2018);
    db.set_file_text(file, "fn f() { 3 }");
    let start = reparsed();
    let _ = other.parse(&db);
    assert_eq!(reparsed(), start);
}

#[test]
fn a_cache_hit_of_parse_allocates_nothing() {
    let mut db = Db::for_tests();
    let file = FileId::from_raw(0);
    put_root(&mut db, 0, &[(file, "/a/lib.rs", include_str!("target.rs"))]);
    let id = EditionedFileId::new(&db, file, Edition::Edition2021);
    let _ = id.parse(&db);
    db.set_file_text(file, &[include_str!("target.rs"), "\nfn added() {}\n"].concat());
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
    let mut db = Db::for_tests();
    let (base, partition) = checkout(&mut db);
    for step in 0..60 {
        let view = seeded_view(&mut rng);
        let before = root_files(&db);
        let at_before = db.source_root(SourceRootId(0)).source_root_ref(&db) as *const SourceRoot;
        let files: Vec<(&str, &[u8])> = view.iter().map(|(path, bytes)| (path.as_str(), bytes.as_slice())).collect();
        let change = set_view(&mut db, &base, &partition, &files).unwrap();

        let mut expected: Vec<(FileId, VfsPath)> =
            before.into_iter().filter(|(id, _)| !change.removed.contains(id)).collect();
        for (path, _) in &view {
            let path = VfsPath::from(base.join(path));
            let id = db.source_root(SourceRootId(0)).source_root_ref(&db).file_for_path(&path).copied();
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
    let root = Arc::new(SourceRoot::new_local(set));
    db.set_source_root_with_durability(SourceRootId(0), root, Durability::MEDIUM);
}

/// Every crate of the graph has the content ID hashing its fields gives.
fn assert_crates_fresh(db: &Db, what: &str) {
    for &krate in crate::all_crates(db).iter() {
        assert_eq!(krate.content(db), krate.content_from_fields(db), "{krate:?} after {what}");
    }
}

/// The comparison that lets an unchanged crate keep its content ID unhashed gives what
/// hashing every crate gave, over random sequences of graphs, setters and moved root files.
#[test]
fn a_crate_graph_set_keeps_every_content_id_what_hashing_gives() {
    const FILES: usize = 6;
    for seed in 0..16 {
        let mut rng = Rng::new(seed);
        let mut db = Db::for_tests();
        let texts: Vec<(String, &str)> =
            (0..FILES).map(|at| (format!("/p{at}/lib.rs"), "pub fn f() {}")).collect();
        let texts: Vec<(&str, &str)> = texts.iter().map(|(path, text)| (path.as_str(), *text)).collect();
        let files = load(&mut db, 0, &texts);
        let mut moved = [false; FILES];
        let mut graph: Vec<DrawnCrate> = Vec::new();
        for step in 0..60 {
            match rng.below(6) {
                // A crate's ID follows its root file's path only once the graph is set again,
                // as a loader does after a move.
                0 => {
                    let at = rng.index(FILES);
                    moved[at] = !moved[at];
                    root_at(&mut db, &files, &moved);
                    build_graph(&graph, &files).set_in_db(&mut db);
                    assert_crates_fresh(&db, &format!("a move, seed {seed} step {step}"));
                }
                1 => {
                    let all = crate::all_crates(&db);
                    if let Some(&krate) = rng.pick(&all) {
                        krate.set_env(&mut db, drawn_env(rng.index(3)));
                        assert_crates_fresh(&db, &format!("a setter, seed {seed} step {step}"));
                    }
                }
                _ => {
                    graph = draw_graph(&mut rng, FILES);
                    build_graph(&graph, &files).set_in_db(&mut db);
                    assert_crates_fresh(&db, &format!("{graph:?}, seed {seed} step {step}"));
                }
            }
        }
    }
}

#[test]
fn an_equal_crate_graph_hashes_no_crate() {
    let mut db = Db::for_tests();
    let files = load(&mut db, 0, FILES);
    let drawn = vec![
        DrawnCrate { root: 2, cfg: 1, env: 0, deps: Vec::new() },
        DrawnCrate { root: 0, cfg: 0, env: 1, deps: vec![0] },
    ];
    build_graph(&drawn, &files).set_in_db(&mut db);
    let (hashed, before) = (crate::input::crates_hashed(&db), revision(&db));
    build_graph(&drawn, &files).set_in_db(&mut db);
    assert_eq!(crate::input::crates_hashed(&db), hashed);
    assert_eq!(revision(&db), before);

    // The dependency changed: it and its dependent are hashed again, and only they.
    let mut changed = drawn.clone();
    changed[0].env = 2;
    build_graph(&changed, &files).set_in_db(&mut db);
    assert_eq!(crate::input::crates_hashed(&db), hashed + 2);
    assert_crates_fresh(&db, "a changed dependency");
}

/// A crate left out of the graph while its dependency changed is hashed again when it comes
/// back with the fields it had.
#[test]
fn a_crate_back_after_its_dependency_changed_is_hashed_again() {
    let mut db = Db::for_tests();
    let files = load(&mut db, 0, FILES);
    let dependency = |env| DrawnCrate { root: 2, cfg: 0, env, deps: Vec::new() };
    let dependent = DrawnCrate { root: 0, cfg: 0, env: 0, deps: vec![0] };
    build_graph(&[dependency(0), dependent.clone()], &files).set_in_db(&mut db);
    build_graph(&[dependency(1)], &files).set_in_db(&mut db);
    let hashed = crate::input::crates_hashed(&db);
    build_graph(&[dependency(1), dependent], &files).set_in_db(&mut db);
    assert_eq!(crate::input::crates_hashed(&db), hashed + 1, "the dependent alone");
    assert_crates_fresh(&db, "the dependent's return");
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
