use std::{collections::BTreeMap, time::Duration};

use blaze_core::{
    Completion, Done, Driver, Effect, FileName, IoError, Tag,
    sim::{Faults, Rng, Sim},
};
use blaze_hash::HashMap;
use blaze_ids::{ContentId, Hasher};

use crate::{
    Change, ChangeKind, CommitError, DIR_DOMAIN, FILE_DOMAIN, Mode, Op, OpError, Reader, Ticket,
    Vfs, View, journal,
    tree::{DIR_MODE, NodeId, Tree},
};

fn write(path: &str, bytes: &str) -> Op {
    write_as(path, bytes, Mode::Regular)
}

fn write_as(path: &str, bytes: &str, mode: Mode) -> Op {
    Op::Write { path: path.into(), bytes: bytes.as_bytes().to_vec(), mode }
}

fn edit(path: &str, at: u64, len: u64, bytes: &str) -> Op {
    Op::Edit { path: path.into(), at, len, bytes: bytes.as_bytes().to_vec() }
}

fn delete(path: &str) -> Op {
    Op::Delete { path: path.into() }
}

fn rename(from: &str, to: &str) -> Op {
    Op::Rename { from: from.into(), to: to.into() }
}

fn name(it: &str) -> FileName {
    FileName::new(it).unwrap()
}

fn files(tree: &Tree, root: NodeId) -> BTreeMap<String, Vec<u8>> {
    moded(tree, root).into_iter().map(|(path, (_, bytes))| (path, bytes)).collect()
}

fn moded(tree: &Tree, root: NodeId) -> BTreeMap<String, (Mode, Vec<u8>)> {
    tree.files(root, Mode::Regular, "")
        .into_iter()
        .map(|(path, node, mode)| (path, (mode, tree.bytes(node).unwrap().expect("a file").to_vec())))
        .collect()
}

fn text_at(tree: &Tree, root: NodeId, path: &str) -> Option<String> {
    tree.read(root, path).unwrap().map(|it| String::from_utf8(it.to_vec()).expect("utf-8"))
}

fn new_vfs() -> Vfs {
    Vfs::recover(name("state/journal"), 0, &[]).0
}

fn contents(vfs: &Vfs, root: ContentId) -> BTreeMap<String, Vec<u8>> {
    files(&vfs.tree, vfs.root(root).expect("a root"))
}

fn text(vfs: &Vfs, root: ContentId, path: &str) -> Option<String> {
    text_at(&vfs.tree, vfs.root(root).expect("a root"), path)
}

/// Runs `effects` and every effect they lead to under `sim`; returns the tickets made durable.
fn settle(sim: &mut Sim, vfs: &mut Vfs, effects: &mut Vec<Effect>) -> Vec<Ticket> {
    vfs.flush(effects);
    let mut durable = Vec::new();
    let mut done = Vec::new();
    while !effects.is_empty() || sim.in_flight() > 0 {
        sim.submit(effects);
        sim.poll(&mut done, Duration::ZERO);
        for completion in done.drain(..) {
            assert!(vfs.owns(completion.tag));
            durable.extend(vfs.complete(completion, effects));
        }
    }
    durable
}

#[test]
fn writes_make_directories_and_read_back() {
    let mut tree = Tree::new();
    let root = tree
        .apply(tree.empty(), &[write("src/lib.rs", "fn a() {}"), write("Cargo.toml", "[package]")])
        .unwrap();
    assert_eq!(text_at(&tree, root, "src/lib.rs").as_deref(), Some("fn a() {}"));
    assert!(tree.is_dir(tree.lookup(root, "src").unwrap()));
    assert_eq!(tree.read(root, "src"), Ok(None));
    assert_eq!(tree.lookup(root, "src/main.rs"), None);
    assert_eq!(tree.lookup(root, ""), Some(root));
}

#[test]
fn the_same_files_hash_alike_however_they_were_made() {
    let mut tree = Tree::new();
    let one = tree
        .apply(tree.empty(), &[write("a/b/c", "1"), write("a/d", "2"), write("e", "3")])
        .unwrap();
    let two = tree.apply(tree.empty(), &[write("e", "3"), write("a/d", "2")]).unwrap();
    let two = tree.apply(two, &[write("a/b/c", "1")]).unwrap();
    let three = tree
        .apply(
            tree.empty(),
            &[write("x/y", "0"), write("e", "3"), write("a/b/c", "1"), write("a/d", "2")],
        )
        .unwrap();
    let three = tree.apply(three, &[delete("x")]).unwrap();
    assert_eq!(one, two);
    assert_eq!(one, three);
    assert_eq!(tree.id(one), tree.id(three));

    // A directory and a file with the same name and bytes are not the same tree.
    let file = tree.apply(tree.empty(), &[write("a", "")]).unwrap();
    let dir = tree.apply(tree.empty(), &[write("a/b", "")]).unwrap();
    assert_ne!(tree.id(file), tree.id(dir));
    // A file's ID is the hash of its bytes, under the file domain.
    let a = tree.lookup(file, "a").unwrap();
    assert_eq!(tree.id(a), ContentId::of(FILE_DOMAIN, b""));
}

#[test]
fn a_transaction_applies_whole_or_not_at_all() {
    let mut tree = Tree::new();
    let base = tree.apply(tree.empty(), &[write("a", "1")]).unwrap();
    let nodes = tree.len();
    let error = tree.apply(base, &[write("b", "2"), edit("a", 5, 1, "x")]).unwrap_err();
    assert_eq!(error, OpError::OutOfRange { path: "a".into(), at: 5, len: 1, size: 1 });
    assert_eq!(tree.len(), nodes);
    assert_eq!(files(&tree, base).len(), 1);
}

#[test]
fn ops_see_the_ops_before_them() {
    let mut tree = Tree::new();
    let root = tree
        .apply(
            tree.empty(),
            &[
                write("a", "hello"),
                edit("a", 5, 0, " world"),
                rename("a", "b/c"),
                edit("b/c", 0, 5, "goodbye"),
            ],
        )
        .unwrap();
    assert_eq!(files(&tree, root), BTreeMap::from([("b/c".into(), b"goodbye world".to_vec())]));
}

#[test]
fn deletes_and_renames_of_directories() {
    let mut tree = Tree::new();
    let base = tree
        .apply(tree.empty(), &[write("src/a.rs", "a"), write("src/x/b.rs", "b"), write("top", "t")])
        .unwrap();
    let moved = tree.apply(base, &[rename("src", "lib/src")]).unwrap();
    assert_eq!(text_at(&tree, moved, "lib/src/x/b.rs").as_deref(), Some("b"));
    assert_eq!(tree.lookup(moved, "src"), None);
    // Moving a directory reuses its node.
    assert_eq!(tree.lookup(moved, "lib/src"), tree.lookup(base, "src"));

    let gone = tree.apply(base, &[delete("src/x/b.rs")]).unwrap();
    assert_eq!(tree.lookup(gone, "src/x"), None, "an emptied directory goes");
    let all = tree.apply(base, &[delete("src"), delete("top")]).unwrap();
    assert_eq!(all, tree.empty());

    let nodes = tree.len();
    let mut error = |ops: &[Op]| tree.apply(base, ops).unwrap_err();
    assert_eq!(error(&[rename("src", "src/x/inner")]), OpError::BadPath("src/x/inner".into()));
    assert_eq!(error(&[rename("top", "src")]), OpError::Exists("src".into()));
    assert_eq!(error(&[write("top/a", "")]), OpError::NotADirectory("top/a".into()));
    assert_eq!(error(&[write("src", "")]), OpError::IsADirectory("src".into()));
    assert_eq!(error(&[delete("nope")]), OpError::NotFound("nope".into()));
    assert_eq!(error(&[write("../a", "")]), OpError::BadPath("../a".into()));
    assert_eq!(error(&[write("a//b", "")]), OpError::BadPath("a//b".into()));
    assert_eq!(tree.len(), nodes, "a refused transaction adds no node");
}

#[test]
fn a_directory_moves_with_its_staged_files_whatever_their_names_sort_as() {
    let mut tree = Tree::new();
    let base = tree
        .apply(tree.empty(), &[write("d/a-b", "1"), write("d/a/b", "2"), write("d/a.c", "3"), write("d/z", "4")])
        .unwrap();
    // Staged before the move: a new file between the base's, one replaced, one deleted.
    let ops = [
        write("d/a/a", "5"),
        write("d/a-b", "6"),
        delete("d/z"),
        rename("d", "e"),
        delete("e/a.c"),
        rename("e", "f/g"),
    ];
    let moved = tree.apply(base, &ops).unwrap();
    let want: BTreeMap<String, Vec<u8>> =
        [("f/g/a-b", "6"), ("f/g/a/a", "5"), ("f/g/a/b", "2")].map(|(path, it)| (path.into(), it.into())).into();
    assert_eq!(files(&tree, moved), want);
    // Deleting the directory takes every file under it, staged or not.
    let gone = tree.apply(base, &[write("d/a/c", "7"), delete("d"), write("top", "t")]).unwrap();
    assert_eq!(files(&tree, gone).into_keys().collect::<Vec<_>>(), vec!["top".to_owned()]);
}

#[test]
fn diffs_skip_what_is_shared() {
    let mut tree = Tree::new();
    let base = tree
        .apply(
            tree.empty(),
            &[write("a/b/c", "1"), write("a/d", "2"), write("e", "3"), write("f", "4")],
        )
        .unwrap();
    let next = tree
        .apply(
            base,
            &[edit("a/b/c", 0, 1, "9"), delete("e"), write("g/h", "5"), rename("f", "a/b")],
        )
        .unwrap_err();
    assert_eq!(next, OpError::Exists("a/b".into()));
    let next = tree
        .apply(
            base,
            &[
                edit("a/b/c", 0, 1, "9"),
                delete("e"),
                write("g/h", "5"),
                delete("f"),
                write("f/i", "6"),
            ],
        )
        .unwrap();
    let change = |path: &str, kind| Change { path: path.into(), kind };
    assert_eq!(
        tree.diff(base, next),
        vec![
            change("a/b/c", ChangeKind::Modified),
            change("e", ChangeKind::Removed),
            change("f", ChangeKind::Removed),
            change("f/i", ChangeKind::Added),
            change("g/h", ChangeKind::Added),
        ]
    );
    assert_eq!(tree.diff(next, next), vec![]);

    // A directory replaced by a file: the file first, then what was under it.
    let back = tree.apply(next, &[delete("f/i"), write("f", "4")]).unwrap();
    assert_eq!(
        tree.diff(next, back),
        vec![change("f", ChangeKind::Added), change("f/i", ChangeKind::Removed)]
    );
}

#[test]
fn diffs_are_in_path_order_by_component() {
    let mut tree = Tree::new();
    let base = tree.apply(tree.empty(), &[write("a/x", "1"), write("a.b", "2")]).unwrap();
    let next = tree.apply(base, &[write("a/x", "3"), write("a.b", "4")]).unwrap();
    let paths: Vec<String> = tree.diff(base, next).into_iter().map(|it| it.path).collect();
    assert_eq!(paths, ["a/x", "a.b"]);
    assert_eq!(
        tree.files(next, Mode::Regular, "").into_iter().map(|it| it.0).collect::<Vec<_>>(),
        paths
    );
}

#[test]
fn a_stale_transaction_rebases_or_names_the_conflict() {
    let mut vfs = new_vfs();
    assert_eq!(vfs.head("w"), None);
    let base = vfs.commit("w", vfs.empty(), &[write("a", "0123456789"), write("b", "b")]).unwrap();
    assert!(!base.rebased);
    let base = base.root;

    // Another agent edits the start of `a` and adds `c`.
    vfs.commit("w", base, &[edit("a", 0, 2, "AAAA"), write("c", "c")]).unwrap();

    // An edit of the end of `a` made against the old base moves past it.
    let commit = vfs.commit("w", base, &[edit("a", 8, 2, "zz"), write("b", "B")]).unwrap();
    assert!(commit.rebased);
    assert_eq!(text(&vfs, commit.root, "a").as_deref(), Some("AAAA234567zz"));
    assert_eq!(text(&vfs, commit.root, "b").as_deref(), Some("B"));
    assert_eq!(text(&vfs, commit.root, "c").as_deref(), Some("c"));

    // One overlapping the change, or replacing a changed file, is refused with the path.
    let head = vfs.head("w");
    assert_eq!(
        vfs.commit("w", base, &[edit("a", 1, 2, "x")]),
        Err(CommitError::Conflict { path: "a".into() })
    );
    assert_eq!(
        vfs.commit("w", base, &[write("a", "x")]),
        Err(CommitError::Conflict { path: "a".into() })
    );
    assert_eq!(
        vfs.commit("w", base, &[write("c", "x")]),
        Err(CommitError::Conflict { path: "c".into() })
    );
    assert_eq!(vfs.head("w"), head);

    let unknown = ContentId([7; 32]);
    assert_eq!(vfs.commit("w", unknown, &[]), Err(CommitError::UnknownBase(unknown)));
    // A file's ID is not a root.
    let file = ContentId::of(FILE_DOMAIN, b"B");
    assert!(!vfs.has(file));
    assert_eq!(vfs.commit("w", file, &[]), Err(CommitError::UnknownBase(file)));
    assert_eq!(
        vfs.commit("w", head.unwrap(), &[delete("nope")]),
        Err(CommitError::Op(OpError::NotFound("nope".into())))
    );
}

#[test]
fn views_read_share_and_outlive_collection_until_closed() {
    let mut vfs = new_vfs();
    let root = vfs
        .commit(
            "main",
            vfs.empty(),
            &[write("src/lib.rs", "x"), write("src/a/b.rs", "y"), write("README", "r")],
        )
        .unwrap()
        .root;
    let main = vfs.open(View { root }).unwrap();
    assert_eq!(vfs.view(main), View { root });
    let file = vfs.read(main, "src/lib.rs").unwrap().unwrap();
    assert_eq!(file.bytes, b"x");
    assert_eq!(file.id, ContentId::of(FILE_DOMAIN, b"x"));
    assert_eq!(vfs.read(main, "src").unwrap(), None);
    assert_eq!(vfs.read(main, "nope").unwrap(), None);
    assert_eq!(vfs.lookup(main, ""), Some(root));
    assert_eq!(vfs.lookup(main, "src/lib.rs"), Some(file.id));
    assert_eq!(vfs.list(main, ""), Some(vec![("README".into(), false), ("src".into(), true)]));
    assert_eq!(vfs.list(main, "src"), Some(vec![("a".into(), true), ("lib.rs".into(), false)]));
    assert_eq!(vfs.list(main, "README"), None);

    // A PR's what-if: ops on the head's root, moving no head.
    let pr = vfs.apply(root, &[edit("src/lib.rs", 0, 1, "z"), delete("README")]).unwrap();
    assert_eq!(vfs.head("main"), Some(root));
    let view = vfs.open(View { root: pr }).unwrap();
    assert_ne!(view, main);
    assert_eq!(vfs.read(view, "src/lib.rs").unwrap().unwrap().bytes, b"z");
    assert_eq!(vfs.lookup(view, "src/a"), vfs.lookup(main, "src/a"), "a subtree is shared");
    assert_eq!(
        vfs.diff(root, pr),
        vec![
            Change { path: "README".into(), kind: ChangeKind::Removed },
            Change { path: "src/lib.rs".into(), kind: ChangeKind::Modified },
        ]
    );

    // Collection keeps what open views and `keep` reach, and drops the rest.
    let ci = vfs.apply(root, &[write("ci-only", "1")]).unwrap();
    let kept = vfs.apply(root, &[write("kept", "1")]).unwrap();
    vfs.collect(&[kept]);
    assert!(!vfs.has(ci));
    assert!(vfs.has(pr) && vfs.has(kept) && vfs.has(root));
    assert_eq!(vfs.read(view, "src/lib.rs").unwrap().unwrap().bytes, b"z");
    assert_eq!(vfs.view(view), View { root: pr });
    vfs.close(view);
    vfs.collect(&[]);
    assert!(!vfs.has(pr) && !vfs.has(kept));
    assert_eq!(vfs.read(main, "src/a/b.rs").unwrap().unwrap().bytes, b"y");

    // A closed view's number is not given again, and a root that is gone does not open.
    let again = vfs.open(View { root }).unwrap();
    assert!(again != view && again != main);
    assert_eq!(vfs.open(View { root: pr }), Err(CommitError::UnknownBase(pr)));
    assert_eq!(vfs.apply(pr, &[]), Err(OpError::UnknownRoot(pr)));
}

#[test]
#[should_panic(expected = "is not open")]
fn a_closed_view_is_not_read() {
    let mut vfs = new_vfs();
    let view = vfs.open(View { root: vfs.empty() }).unwrap();
    vfs.close(view);
    let _ = vfs.read(view, "a").unwrap();
}

#[test]
fn branches_and_prs_are_heads_that_survive_recovery() {
    let file = name("state/journal");
    let mut sim = Sim::new(3, Faults::default());
    let mut vfs = Vfs::recover(file.clone(), 0, &[]).0;
    let main = vfs.commit("main", vfs.empty(), &[write("a", "1")]).unwrap().root;
    vfs.set_head("branch/x", main).unwrap();
    let pr = vfs.commit("branch/x", main, &[write("b", "2")]).unwrap().root;
    vfs.set_head("pr/7", pr).unwrap();
    assert!(vfs.remove_head("branch/x").is_some());
    assert_eq!(vfs.remove_head("branch/x"), None);
    let unknown = ContentId([1; 32]);
    assert_eq!(vfs.set_head("x", unknown), Err(CommitError::UnknownBase(unknown)));
    assert_eq!(vfs.heads(), vec![("main", main), ("pr/7", pr)]);
    assert_eq!(settle(&mut sim, &mut vfs, &mut Vec::new()).len(), 5);

    let (back, found) = Vfs::recover(file.clone(), 0, &sim.contents(&file));
    assert_eq!((found.skipped, found.heads_dropped), (0, 0));
    assert_eq!(back.heads(), vec![("main", main), ("pr/7", pr)]);
    assert_eq!(back.head("branch/x"), None);
    assert!(back.sequence("pr/7") > back.sequence("main"));
    assert_eq!(contents(&back, pr), contents(&vfs, pr));
}

#[test]
fn collecting_keeps_what_is_reached() {
    let mut vfs = new_vfs();
    let mut root = vfs.empty();
    for at in 0..50 {
        root = vfs
            .commit("w", root, &[write(&format!("d{}/f{at}", at % 4), &at.to_string())])
            .unwrap()
            .root;
    }
    let what_if = vfs.apply(root, &[write("only/here", "x")]).unwrap();
    let before = (contents(&vfs, root), contents(&vfs, what_if));
    let size = vfs.tree.len();
    vfs.collect(&[what_if]);
    assert!(vfs.tree.len() < size / 2, "{} of {size} nodes left", vfs.tree.len());
    assert_eq!((contents(&vfs, root), contents(&vfs, what_if)), before);
    assert_eq!(vfs.head("w"), Some(root));
    // The tree still works after renumbering.
    let next = vfs.commit("w", root, &[write("d0/f0", "new")]).unwrap().root;
    assert_eq!(text(&vfs, next, "d0/f0").as_deref(), Some("new"));
}

type Model = BTreeMap<String, (Mode, Vec<u8>)>;

/// The old layout as a model: random transactions against a map from path to mode and bytes,
/// with each root's ID hashed from the map alone, entry by entry, as the node table before the
/// column store did. Paths with `-` and `.` sort between a name and the same name with `/`, so
/// a component's changes are not one run of the sorted paths. Every few transactions a
/// collection renumbers the table; diffs, the reader and IDs are checked against the model
/// after each.
#[test]
fn transactions_match_a_model() {
    const PATHS: &[&str] =
        &["a", "a.b", "a-c", "b", "d", "d/a", "d/b", "d.x", "d/e/a", "d/e.f", "f/g/h", "f-g"];
    const DIRS: &[&str] = &["d", "d/e", "f", "f/g"];
    for seed in 0..300 {
        let mut rng = Rng::new(seed);
        let mut vfs = new_vfs();
        let reader = vfs.reader();
        let mut root = vfs.empty();
        let mut model = Model::new();
        let mut kept: Vec<(ContentId, Model)> = Vec::new();
        for step in 0..40 {
            let mut ops = Vec::new();
            let mut next = model.clone();
            for _ in 0..=rng.below(4) {
                let path = PATHS[rng.below(PATHS.len() as u64) as usize];
                let bytes = format!("{seed}-{}", rng.below(1000)).into_bytes();
                let op = match rng.below(5) {
                    0 | 1 => {
                        let mode =
                            [Mode::Regular, Mode::Executable, Mode::Symlink][rng.below(3) as usize];
                        Op::Write { path: path.into(), bytes, mode }
                    }
                    2 => match next.get(path) {
                        Some((_, old)) => {
                            let at = rng.below(old.len() as u64 + 1);
                            let len = rng.below(old.len() as u64 - at + 1);
                            Op::Edit { path: path.into(), at, len, bytes }
                        }
                        None => continue,
                    },
                    3 if rng.one_in(2) => delete(DIRS[rng.below(DIRS.len() as u64) as usize]),
                    3 => delete(path),
                    _ => match next.keys().nth(rng.below(next.len() as u64 + 1) as usize) {
                        Some(from) => rename(from, path),
                        None => continue,
                    },
                };
                // Only ops the tree takes: none puts a file where a directory is.
                if !model_apply(&mut next, &op) {
                    continue;
                }
                ops.push(op);
            }
            let after = vfs
                .apply(root, &ops)
                .unwrap_or_else(|error| panic!("seed {seed}: {error} for {ops:?}"));
            assert_eq!(after, model_id(&next), "seed {seed}: the root's ID for {ops:?}");
            let paths: Vec<String> = vfs.diff(root, after).into_iter().map(|it| it.path).collect();
            assert_eq!(paths, model_diff(&model, &next), "seed {seed}: the diff of {ops:?}");
            (root, model) = (after, next);
            assert_eq!(moded(&vfs.tree, vfs.root(root).unwrap()), model, "seed {seed}");
            for (path, (mode, bytes)) in &model {
                let read = reader.file(root, path).unwrap().map(|(_, mode, bytes)| (mode, bytes.to_vec()));
                assert_eq!(read, Some((*mode, bytes.clone())), "seed {seed}: {path}");
            }
            if step % 2 == 0 {
                kept.push((root, model.clone()));
            }
            if step % 7 == 6 {
                kept.drain(..kept.len().saturating_sub(2));
                let keep: Vec<ContentId> = kept.iter().map(|it| it.0).chain([root]).collect();
                vfs.collect(&keep);
                for (id, files) in &kept {
                    assert_eq!(&moded(&vfs.tree, vfs.root(*id).unwrap()), files, "seed {seed}");
                }
            }
        }
    }
}

fn model_apply(model: &mut Model, op: &Op) -> bool {
    let under = |path: &str| format!("{path}/");
    let clashes = |model: &Model, path: &str| {
        model.keys().any(|it| it.starts_with(&under(path)) || path.starts_with(&under(it)))
    };
    match op {
        Op::Write { path, bytes, mode } => {
            if clashes(model, path) {
                return false;
            }
            model.insert(path.clone(), (*mode, bytes.clone()));
        }
        Op::Edit { path, at, len, bytes } => {
            let (_, old) = model.get_mut(path).expect("edits name a file");
            old.splice(*at as usize..(at + len) as usize, bytes.iter().copied());
        }
        Op::Delete { path } => {
            let before = model.len();
            model.retain(|it, _| it != path && !it.starts_with(&under(path)));
            return model.len() < before;
        }
        Op::Rename { from, to } => {
            if from == to || to.starts_with(&under(from)) || model.contains_key(to) {
                return false;
            }
            let Some(file) = model.remove(from) else { return false };
            if clashes(model, to) {
                model.insert(from.clone(), file);
                return false;
            }
            model.insert(to.clone(), file);
        }
    }
    true
}

enum ModelNode<'a> {
    File(Mode, &'a [u8]),
    Dir(BTreeMap<&'a str, ModelNode<'a>>),
}

/// A root's ID from the model alone: each directory hashed entry by entry with an incremental
/// hasher (git's code, length-prefixed name, child ID), as the old node table did.
fn model_id(model: &Model) -> ContentId {
    let mut root = BTreeMap::new();
    for (path, (mode, bytes)) in model {
        let mut names: Vec<&str> = path.split('/').collect();
        let file = names.pop().unwrap();
        let mut dir = &mut root;
        for name in names {
            let ModelNode::Dir(next) =
                dir.entry(name).or_insert_with(|| ModelNode::Dir(BTreeMap::new()))
            else {
                panic!("{path} under a file")
            };
            dir = next;
        }
        dir.insert(file, ModelNode::File(*mode, bytes));
    }
    fn hash(node: &ModelNode<'_>) -> (u32, ContentId) {
        match node {
            ModelNode::File(mode, bytes) => (mode.git(), ContentId::of(FILE_DOMAIN, bytes)),
            ModelNode::Dir(entries) => {
                let mut hasher = Hasher::new(DIR_DOMAIN);
                for (name, child) in entries {
                    let (code, id) = hash(child);
                    hasher.update(&code.to_le_bytes()).part(name.as_bytes()).id(&id);
                }
                (DIR_MODE, hasher.finish())
            }
        }
    }
    hash(&ModelNode::Dir(root)).1
}

/// The paths a diff names, in path order by component: each file added, removed, or changed
/// in bytes or mode.
fn model_diff(old: &Model, new: &Model) -> Vec<String> {
    let mut paths: Vec<&String> =
        old.keys().chain(new.keys()).filter(|it| old.get(*it) != new.get(*it)).collect();
    paths.sort_unstable_by(|a, b| a.split('/').cmp(b.split('/')));
    paths.dedup();
    paths.into_iter().cloned().collect()
}

/// A directory's changes that sort apart as strings ("a", then "a.b", then "a/y") are one
/// group by component: the delete of `a` and the write under it are applied together.
#[test]
fn a_directory_deleted_and_written_under_again_keeps_its_dotted_sibling() {
    let mut tree = Tree::new();
    let base = tree
        .apply(tree.empty(), &[write("a/x", "1"), write("a-c/z", "0"), write("b", "2")])
        .unwrap();
    let next = tree
        .apply(base, &[delete("a"), write("a.b", "4"), write("a/y", "5"), write("a-c/w", "6")])
        .unwrap();
    let want = [("a-c/w", "6"), ("a-c/z", "0"), ("a.b", "4"), ("a/y", "5"), ("b", "2")];
    assert_eq!(
        files(&tree, next),
        want.into_iter()
            .map(|(path, bytes)| (path.to_owned(), bytes.as_bytes().to_vec()))
            .collect()
    );
}

/// Readers on several threads, each pinned to read a root whole, while the writer commits and
/// collects: every root a reader finds reads complete and self-consistent, and one a
/// collection dropped reads as none, never as part of a tree.
#[cfg(not(blaze_check))] // many threads, long loops
#[test]
fn readers_on_many_threads_see_whole_roots_while_commits_and_collections_run() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use parking_lot::Mutex;

    let mut vfs = new_vfs();
    let reader = vfs.reader();
    let published = Mutex::new(Vec::<ContentId>::new());
    let finished = AtomicUsize::new(0);
    const READERS: usize = 4;
    std::thread::scope(|scope| {
        let readers: Vec<_> = (0..READERS as u64)
            .map(|seed| {
                let (reader, published, finished) = (reader.clone(), &published, &finished);
                scope.spawn(move || {
                    let mut rng = Rng::new(seed);
                    let (mut whole, mut gone) = (0, 0);
                    while whole + gone < 3000 {
                        let root = {
                            let roots = published.lock();
                            let len = roots.len() as u64;
                            // The latest half the time: the head keeps it, so most reads find
                            // one.
                            let at =
                                if rng.one_in(2) { len.saturating_sub(1) } else { rng.below(len) };
                            roots.get(at as usize).copied()
                        };
                        let Some(root) = root else {
                            std::thread::yield_now();
                            continue;
                        };
                        let pinned = reader.pin();
                        let a = pinned.file(root, "a").unwrap().map(|it| it.bytes.to_vec());
                        let b = pinned.file(root, "d/e/b").unwrap().map(|it| it.bytes.to_vec());
                        let listed = pinned.list(root, "d").map(Iterator::count);
                        match (a, b, listed) {
                            (Some(a), Some(b), Some(2)) => {
                                assert_eq!(a, b, "one root's files are one commit's");
                                whole += 1;
                            }
                            (None, None, None) => gone += 1,
                            other => panic!("part of a root: {other:?}"),
                        }
                    }
                    finished.fetch_add(1, Ordering::Release);
                    whole
                })
            })
            .collect();
        let mut root = vfs.empty();
        let mut at = 0u64;
        while finished.load(Ordering::Acquire) < READERS || at < 300 {
            let bytes = at.to_string();
            let ops = [
                write("a", &bytes),
                write("d/e/b", &bytes),
                write("d/c", "c"),
                write(&format!("x/{}", at % 8), &bytes),
            ];
            root = vfs.commit("w", root, &ops).unwrap().root;
            published.lock().push(root);
            if at % 25 == 24 {
                let keep = {
                    let roots = published.lock();
                    roots[roots.len().saturating_sub(3)..].to_vec()
                };
                vfs.collect(&keep);
            }
            at += 1;
        }
        for reading in readers {
            assert!(reading.join().unwrap() > 0, "a reader found a whole root");
        }
    });
    vfs.collect(&[]);
    let head = vfs.head("w").unwrap();
    let read = reader.pin().file(head, "d/c").unwrap().map(|it| it.bytes.to_vec());
    assert_eq!(read, Some(b"c".to_vec()));
}

fn listed(reader: &Reader, root: ContentId, path: &str) -> Option<Vec<(String, bool)>> {
    Some(reader.list(root, path)?.collect())
}

/// What a pinned read lends, owned, to compare with [`Reader::file`].
fn pinned_file(reader: &Reader, root: ContentId, path: &str) -> Option<(ContentId, Mode, Vec<u8>)> {
    reader.pin().file(root, path).unwrap().map(|it| (it.id, it.mode, it.bytes.to_vec()))
}

fn pinned_list(reader: &Reader, root: ContentId, path: &str) -> Option<Vec<(String, bool)>> {
    Some(reader.pin().list(root, path)?.map(|(name, dir)| (name.to_owned(), dir)).collect())
}

/// A tree of about `n` files, 1 to 4 directories deep, by `seed`, of names that sort around `/`
/// (`a`, `a-b`, `a.rs`, `a0`): its VFS, its root, every directory (`""` first) and every file.
fn listing_fixture(seed: u64, n: usize) -> (Vfs, ContentId, Vec<String>, Vec<String>) {
    const PARTS: &[&str] = &["a", "a-b", "a.rs", "a0", "b", "src", "lib.rs", "z"];
    let mut rng = Rng::new(seed);
    let mut files: BTreeMap<String, String> = BTreeMap::new();
    for _ in 0..10 * n {
        if files.len() == n {
            break;
        }
        let depth = 1 + rng.below(4);
        let parts: Vec<&str> = (0..depth).map(|_| PARTS[rng.below(PARTS.len() as u64) as usize]).collect();
        let path = parts.join("/");
        let clash = files
            .keys()
            .any(|it| it.starts_with(&format!("{path}/")) || path.starts_with(&format!("{it}/")));
        if !clash {
            files.insert(path.clone(), path);
        }
    }
    let mut vfs = new_vfs();
    let ops: Vec<Op> = files.iter().map(|(path, bytes)| write(path, bytes)).collect();
    let empty = vfs.empty();
    let root = vfs.commit("w", empty, &ops).unwrap().root;
    let mut dirs: Vec<String> = vec![String::new()];
    for path in files.keys() {
        let mut at = 0;
        while let Some(slash) = path[at..].find('/') {
            at += slash;
            dirs.push(path[..at].to_owned());
            at += 1;
        }
    }
    dirs[1..].sort_unstable();
    dirs.dedup();
    (vfs, root, dirs, files.into_keys().collect())
}

/// Every directory of a tree lists the same through a pin (names lent), through
/// `Reader::list` (names owned) and through `Vfs::list` (the tree's own walk); a file, a path
/// under a file and a missing path list as none through each.
#[test]
fn every_directory_lists_the_same_lent_owned_and_through_the_vfs() {
    for seed in 0..8 {
        let (mut vfs, root, dirs, files) = listing_fixture(seed, 120);
        let view = vfs.open(View { root }).unwrap();
        let reader = vfs.reader();
        assert!(dirs.len() > 8, "seed {seed}: {dirs:?}");
        for dir in &dirs {
            let want = vfs.list(view, dir);
            assert!(want.as_ref().is_some_and(|it| !it.is_empty()), "seed {seed}: {dir:?}");
            assert_eq!(pinned_list(&reader, root, dir), want, "seed {seed}: {dir:?} lent");
            assert_eq!(listed(&reader, root, dir), want, "seed {seed}: {dir:?} owned");
        }
        let under = files.iter().map(|it| format!("{it}/x"));
        for path in files.iter().cloned().chain(under).chain(["nope".into(), "a/nope/x".into()]) {
            assert_eq!(vfs.list(view, &path), None, "seed {seed}: {path:?}");
            assert_eq!(pinned_list(&reader, root, &path), None, "seed {seed}: {path:?} lent");
            assert_eq!(listed(&reader, root, &path), None, "seed {seed}: {path:?} owned");
        }
    }
}

/// A listing through a pin lends the tree's names in place: listing every directory of a tree,
/// under one pin or a pin each, allocates nothing.
#[test]
fn a_pinned_listing_allocates_nothing() {
    use blaze_alloc_count::assert_at_most;

    assert!(blaze_alloc_count::installed());
    let (mut vfs, root, dirs, _) = listing_fixture(3, 200);
    let view = vfs.open(View { root }).unwrap();
    let want: Vec<Vec<(String, bool)>> = dirs.iter().map(|it| vfs.list(view, it).expect("a directory")).collect();
    let reader = vfs.reader();
    // The first read registers this thread with the reclaimer; reads after it are measured.
    let _ = reader.pin().list(root, "").map(Iterator::count);
    assert_at_most(0, "every directory listed under one pin", || {
        let pinned = reader.pin();
        for (dir, want) in dirs.iter().zip(&want) {
            let lent = pinned.list(root, dir).expect("a directory");
            assert!(lent.eq(want.iter().map(|(name, is_dir)| (name.as_str(), *is_dir))), "{dir:?}");
        }
    });
    let listed = assert_at_most(0, "every directory listed under a pin each", || {
        dirs.iter().map(|dir| reader.pin().list(root, dir).map_or(0, Iterator::count)).sum::<usize>()
    });
    assert_eq!(listed, want.iter().map(Vec::len).sum::<usize>());
}

/// A file looked up by path is the tree's bytes, lent under a pin or held by one count of their
/// owner: looking up every file of a tree, and paths that name a directory or nothing,
/// allocates nothing.
#[test]
fn a_file_looked_up_allocates_nothing() {
    use blaze_alloc_count::assert_at_most;

    assert!(blaze_alloc_count::installed());
    let (vfs, root, dirs, files) = listing_fixture(5, 200);
    let reader = vfs.reader();
    let missing: Vec<String> = files.iter().map(|it| format!("{it}/x")).chain(["nope".to_owned()]).collect();
    // The first read registers this thread with the reclaimer; reads after it are measured.
    let _ = reader.pin().file(root, &files[0]);
    assert_at_most(0, "every file looked up under one pin", || {
        let pinned = reader.pin();
        for path in &files {
            let file = pinned.file(root, path).unwrap().expect("a file");
            assert_eq!(file.bytes, path.as_bytes(), "{path:?}");
        }
        for path in dirs.iter().chain(&missing) {
            assert!(pinned.file(root, path).unwrap().is_none(), "{path:?}");
        }
    });
    let held = assert_at_most(0, "every file held through the reader", || {
        files
            .iter()
            .filter(|path| reader.file(root, path).unwrap().is_some_and(|(_, _, bytes)| &*bytes == path.as_bytes()))
            .count()
    });
    assert_eq!(held, files.len());
}

#[cfg(not(blaze_check))] // many threads, long loops
#[test]
fn the_reader_reads_every_root_as_the_vfs_does() {
    let mut vfs = new_vfs();
    let reader = vfs.reader();
    let empty = vfs.empty();
    assert_eq!(
        listed(&reader, empty, ""),
        Some(Vec::new()),
        "the empty root is there from the start"
    );
    let ops = [
        write("src/lib.rs", "fn a() {}"),
        write_as("run.sh", "echo", Mode::Executable),
        write_as("link", "run.sh", Mode::Symlink),
    ];
    let root = vfs.commit("w", empty, &ops).unwrap().root;
    let view = vfs.open(View { root }).unwrap();
    for path in ["src/lib.rs", "run.sh", "link", "src", "", "nope", "src/lib.rs/x", "a//b", "../x"]
    {
        let file = vfs.read(view, path).unwrap().map(|it| (it.id, it.mode, it.bytes.to_vec()));
        let read = reader.file(root, path).unwrap().map(|(id, mode, bytes)| (id, mode, bytes.to_vec()));
        assert_eq!(read, file, "{path}");
        assert_eq!(pinned_file(&reader, root, path), file, "{path}");
    }
    for path in ["", "src", "run.sh", "nope"] {
        assert_eq!(listed(&reader, root, path), vfs.list(view, path), "{path}");
        assert_eq!(pinned_list(&reader, root, path), vfs.list(view, path), "{path}");
    }
    // A read lends the tree's own bytes: no copy, whichever handle reads them.
    let lent = reader.file(root, "run.sh").unwrap().unwrap().2;
    assert!(std::ptr::eq(lent.as_ptr(), vfs.read(view, "run.sh").unwrap().unwrap().bytes.as_ptr()));
    let file_id = vfs.lookup(view, "run.sh").unwrap();
    assert_eq!(listed(&reader, file_id, ""), None, "a file is no root");
    assert_eq!(reader.file(file_id, "").unwrap(), None);

    // A what-if is readable as soon as `apply` gives its ID. Readers on other threads go on
    // while the table grows and collections swap levels under them; what a collection keeps
    // reads the same throughout.
    let what_if = vfs.apply(root, &[edit("src/lib.rs", 0, 2, "pub fn")]).unwrap();
    std::thread::scope(|scope| {
        let readers: Vec<_> = (0..4)
            .map(|_| {
                let reader = reader.clone();
                scope.spawn(move || {
                    (0..2000).all(|_| {
                        reader
                            .file(what_if, "src/lib.rs").unwrap()
                            .is_some_and(|it| *it.2 == *b"pub fn a() {}")
                    })
                })
            })
            .collect();
        for at in 0..200 {
            let head = vfs.head("w").unwrap();
            vfs.commit("w", head, &[write(&format!("f{at}"), "x")]).unwrap();
            if at % 50 == 49 {
                vfs.collect(&[what_if]);
            }
        }
        for reading in readers {
            assert!(reading.join().unwrap());
        }
    });

    // Collection takes from the reader what it takes from the tree.
    vfs.close(view);
    vfs.remove_head("w");
    vfs.collect(&[what_if]);
    assert!(vfs.has(what_if) && !vfs.has(root));
    assert_eq!(reader.file(root, "run.sh").unwrap(), None);
    assert_eq!(listed(&reader, root, ""), None);
    assert_eq!(reader.file(what_if, "run.sh").unwrap().map(|it| it.1), Some(Mode::Executable));
    let next = vfs.commit("w", empty, &[write("after", "1")]).unwrap().root;
    assert_eq!(reader.file(next, "after").unwrap().map(|it| it.2.to_vec()), Some(b"1".to_vec()));

    // A reader outlives its `Vfs`: what was retired and not reclaimed goes with the last
    // handle, and what the table holds still reads.
    vfs.collect(&[]);
    drop(vfs);
    assert_eq!(reader.file(what_if, "run.sh").unwrap(), None);
    assert_eq!(reader.file(next, "after").unwrap().map(|it| it.2.to_vec()), Some(b"1".to_vec()));
    assert_eq!(listed(&reader, empty, ""), Some(Vec::new()));
}

#[test]
fn modes_are_git_modes() {
    assert_eq!(
        [Mode::Regular, Mode::Executable, Mode::Symlink].map(Mode::git),
        [0o100644, 0o100755, 0o120000]
    );
    for mode in [Mode::Regular, Mode::Executable, Mode::Symlink] {
        assert_eq!(Mode::from_git(mode.git()), Some(mode));
    }
    assert_eq!(Mode::from_git(0o100664), Some(Mode::Regular), "git's old group-writable mode");
    assert_eq!(Mode::from_git(0o040000), None, "a directory is no file");
    assert_eq!(Mode::from_git(0o160000), None, "a submodule is no file");
}

#[test]
fn a_mode_is_in_the_tree_not_in_the_file() {
    let mut vfs = new_vfs();
    let plain = vfs.commit("w", vfs.empty(), &[write("run.sh", "echo")]).unwrap().root;
    let exec =
        vfs.commit("w", plain, &[write_as("run.sh", "echo", Mode::Executable)]).unwrap().root;
    assert_ne!(plain, exec, "a chmod is a new root");
    assert_eq!(
        vfs.diff(plain, exec),
        vec![Change { path: "run.sh".into(), kind: ChangeKind::Modified }]
    );
    let (a, b) = (vfs.open(View { root: plain }).unwrap(), vfs.open(View { root: exec }).unwrap());
    let (a, b) = (vfs.read(a, "run.sh").unwrap().unwrap(), vfs.read(b, "run.sh").unwrap().unwrap());
    assert_eq!((a.mode, b.mode), (Mode::Regular, Mode::Executable));
    assert_eq!(a.id, b.id, "the same bytes are the same file");

    // Edits and renames keep the mode; a write sets it.
    let mut tree = Tree::new();
    let root = tree
        .apply(
            tree.empty(),
            &[
                write_as("bin/run", "a", Mode::Executable),
                edit("bin/run", 1, 0, "b"),
                rename("bin", "tools"),
            ],
        )
        .unwrap();
    assert_eq!(
        moded(&tree, root),
        BTreeMap::from([("tools/run".into(), (Mode::Executable, b"ab".to_vec()))])
    );
    let back = tree.apply(root, &[write("tools/run", "ab")]).unwrap();
    assert_eq!(moded(&tree, back)["tools/run"].0, Mode::Regular);
    let again = tree.apply(back, &[write_as("tools/run", "ab", Mode::Executable)]).unwrap();
    assert_eq!(again, root, "the same files and modes hash alike");
}

#[test]
fn a_symlink_reads_as_its_target_and_follows_to_its_file() {
    let mut vfs = new_vfs();
    let root = vfs
        .commit(
            "w",
            vfs.empty(),
            &[
                write("src/lib.rs", "fn a() {}"),
                write_as("lib.rs", "src/lib.rs", Mode::Symlink),
                write_as("src/up", "../lib.rs", Mode::Symlink),
                write_as("dir", "src", Mode::Symlink),
                write_as("out", "../x", Mode::Symlink),
                write_as("abs", "/etc/passwd", Mode::Symlink),
                write_as("loop", "loop", Mode::Symlink),
                write_as("dangling", "nope", Mode::Symlink),
            ],
        )
        .unwrap()
        .root;
    let view = vfs.open(View { root }).unwrap();
    let link = vfs.read(view, "lib.rs").unwrap().unwrap();
    assert_eq!((link.mode, link.bytes), (Mode::Symlink, &b"src/lib.rs"[..]));

    let target = vfs.read(view, "src/lib.rs").unwrap().unwrap();
    for path in ["lib.rs", "src/up", "dir/lib.rs", "src/lib.rs"] {
        assert_eq!(vfs.follow(view, path), Ok(Some(target)), "{path}");
    }
    assert_eq!(vfs.follow(view, "dir"), Ok(None), "a directory");
    assert_eq!(vfs.follow(view, "dangling"), Ok(None));
    for path in ["out", "abs", "loop"] {
        assert!(matches!(vfs.follow(view, path), Err(OpError::BadLink { .. })), "{path}");
    }
}

#[test]
fn the_journal_keeps_modes() {
    let file = name("j");
    let mut sim = Sim::new(3, Faults::default());
    let mut vfs = Vfs::recover(file.clone(), 0, &[]).0;
    let ops = [
        write("a", "1"),
        write_as("bin/run", "1", Mode::Executable),
        write_as("link", "a", Mode::Symlink),
    ];
    let root = vfs.commit("w", vfs.empty(), &ops).unwrap().root;
    settle(&mut sim, &mut vfs, &mut Vec::new());
    let (recovered, found) = Vfs::recover(file, 0, &sim.contents(&name("j")));
    assert_eq!(found.skipped, 0);
    assert_eq!(recovered.head("w"), Some(root));
    let modes: Vec<(String, Mode)> = moded(&recovered.tree, recovered.root(root).unwrap())
        .into_iter()
        .map(|(path, (mode, _))| (path, mode))
        .collect();
    let want = [("a", Mode::Regular), ("bin/run", Mode::Executable), ("link", Mode::Symlink)];
    assert_eq!(modes, want.map(|(path, mode)| (path.to_owned(), mode)));
}

/// One run of the journal under the simulated machine: commits to two heads, some made against
/// a stale base, heads moved and removed, injected IO failures, collections (and the rewrites
/// they ask for), and power cuts. After each cut the recovered heads must hold every
/// acknowledged record, each head must be a whole move, and its files must be the ones that
/// move left.
fn journal_run(seed: u64) {
    let file = name("vfs/journal");
    let mut rng = Rng::new(seed ^ 0x5eed);
    let faults = Faults {
        fail_one_in: 1 + rng.below(20) as u32,
        max_latency: Duration::from_micros(300),
        ..Faults::default()
    };
    let mut sim = Sim::new(seed, faults);
    let mut vfs = Vfs::recover(file.clone(), 0, &[]).0;
    let mut effects = Vec::new();
    let mut done = Vec::new();
    // Each head move's sequence number, and the head and files it moved to.
    let mut moves: HashMap<u64, (String, BTreeMap<String, Vec<u8>>)> = HashMap::default();
    // Each ticket's head, sequence and whether it removed the head.
    let mut tickets: HashMap<Ticket, (String, u64, bool)> = HashMap::default();
    // By head: the latest record acknowledged durable, and whether it was a removal.
    let mut acknowledged: HashMap<String, (u64, bool)> = HashMap::default();
    // By head: the sequence of each removal, acknowledged or not. A crash keeps a random prefix
    // of what was appended and not synced, so an unacknowledged removal can survive one while
    // a later one of the same head (after it was made again) is cut.
    let mut removed: HashMap<String, Vec<u64>> = HashMap::default();
    let mut old_bases: Vec<ContentId> = Vec::new();

    for step in 0..300 {
        match rng.below(100) {
            0..=51 => {
                let head = if rng.one_in(3) { "b" } else { "a" };
                let at = vfs.head(head).unwrap_or(vfs.empty());
                let base = match old_bases.get(rng.below(old_bases.len() as u64 * 4) as usize) {
                    Some(&old) if vfs.has(old) => old,
                    _ => at,
                };
                let base_node = vfs.root(base).unwrap();
                let mut ops = Vec::new();
                for _ in 0..=rng.below(3) {
                    let path = format!(
                        "{}/f{}",
                        ["src", "src/x", "tests"][rng.below(3) as usize],
                        rng.below(6)
                    );
                    let bytes = format!("{seed}:{step}:{}", rng.below(1 << 20));
                    ops.push(match (rng.below(4), vfs.tree.read(base_node, &path).unwrap()) {
                        (0, Some(_)) => delete(&path),
                        (1, Some(old)) => edit(
                            &path,
                            old.len() as u64 / 2,
                            rng.below(old.len() as u64 / 2 + 1),
                            &bytes,
                        ),
                        _ => write(&path, &bytes),
                    });
                }
                match vfs.commit(head, base, &ops) {
                    Ok(commit) => {
                        let sequence = vfs.sequence(head).unwrap();
                        moves.insert(sequence, (head.into(), contents(&vfs, commit.root)));
                        tickets.insert(commit.ticket, (head.into(), sequence, false));
                        old_bases.push(commit.root);
                    }
                    Err(CommitError::Conflict { .. } | CommitError::Op(_)) => {}
                    Err(error) => panic!("seed {seed}: {error}"),
                }
                vfs.flush(&mut effects);
            }
            52..=55 => {
                // A branch made from the other, or removed.
                let (from, to) = if rng.one_in(2) { ("a", "b") } else { ("b", "a") };
                if rng.one_in(2) {
                    if let Some(ticket) = vfs.remove_head(to) {
                        let sequence = vfs.journal.last_sequence();
                        tickets.insert(ticket, (to.into(), sequence, true));
                        removed.entry(to.into()).or_default().push(sequence);
                    }
                } else if let Some(root) = vfs.head(from) {
                    let ticket = vfs.set_head(to, root).unwrap();
                    let sequence = vfs.sequence(to).unwrap();
                    moves.insert(sequence, (to.into(), contents(&vfs, root)));
                    tickets.insert(ticket, (to.into(), sequence, false));
                }
                vfs.flush(&mut effects);
            }
            56..=93 => {
                sim.submit(&mut effects);
                sim.poll(&mut done, Duration::ZERO);
                for completion in done.drain(..) {
                    for ticket in vfs.complete(completion, &mut effects) {
                        let (head, sequence, removal) =
                            tickets.remove(&ticket).expect("a ticket handed out");
                        let at = acknowledged.entry(head).or_insert((0, false));
                        if sequence > at.0 {
                            *at = (sequence, removal);
                        }
                    }
                }
            }
            94..=96 => {
                vfs.collect(&[]);
                old_bases.retain(|it| vfs.has(*it));
                vfs.flush(&mut effects);
            }
            _ => {
                sim.submit(&mut effects);
                sim.crash();
                effects.clear();
                tickets.clear();
                old_bases.clear();
                vfs = Vfs::recover(file.clone(), 0, &sim.contents(&file)).0;
                for (head, root) in vfs.heads() {
                    let sequence = vfs.sequence(head).unwrap();
                    let (by, files) = moves.get(&sequence).unwrap_or_else(|| {
                        panic!("seed {seed}: head {sequence} was never moved to")
                    });
                    assert_eq!(by, head, "seed {seed}: head {sequence}");
                    assert_eq!(&contents(&vfs, root), files, "seed {seed}: head {sequence}");
                }
                for (head, &(sequence, removal)) in &acknowledged {
                    match vfs.sequence(head) {
                        Some(recovered) => assert!(
                            recovered > sequence || (recovered == sequence && !removal),
                            "seed {seed}: {head} acknowledged {sequence}, recovered {recovered}"
                        ),
                        None => assert!(
                            removal
                                || removed
                                    .get(head)
                                    .is_some_and(|it| it.iter().any(|&at| at > sequence)),
                            "seed {seed}: {head} acknowledged {sequence}, recovered nothing"
                        ),
                    }
                }
                // Sequences after the cut may reuse numbers of moves and removals it lost.
                let last = vfs.journal.last_sequence();
                moves.retain(|&sequence, _| sequence <= last);
                for sequences in removed.values_mut() {
                    sequences.retain(|&sequence| sequence <= last);
                }
            }
        }
    }
}

#[test]
fn the_journal_survives_crashes_and_failures() {
    let seeds =
        std::env::var("BLAZE_DST_SEEDS").ok().and_then(|it| it.parse().ok()).unwrap_or(2000u64);
    let first = std::env::var("BLAZE_DST_SEED").ok().and_then(|it| it.parse().ok()).unwrap_or(0u64);
    for seed in first..first + seeds {
        journal_run(seed);
    }
}

/// A torn record left at the end must be cut off before the next append: followed by new records
/// it could borrow their first bytes for its checksum and hide them.
#[test]
fn a_recovered_journal_cuts_its_torn_tail_before_appending() {
    let file = name("j");
    let mut vfs = Vfs::recover(file.clone(), 0, &[]).0;
    vfs.commit("w", vfs.empty(), &[write("a", "1")]).unwrap();
    let mut effects = Vec::new();
    vfs.flush(&mut effects);
    let Some(Effect::Append { bytes, .. }) = effects.pop() else { panic!("an append") };
    let torn = &bytes[..bytes.len() - 1];

    let (mut vfs, found) = Vfs::recover(file, 0, torn);
    assert_eq!(found.heads_dropped, 0);
    assert!(found.skipped > 0);
    let valid = (torn.len() - found.skipped) as u64;
    let base = vfs.head("w").unwrap_or(vfs.empty());
    vfs.commit("w", base, &[write("b", "2")]).unwrap();
    vfs.flush(&mut effects);
    assert!(
        matches!(effects.as_slice(), [Effect::Truncate { len, .. }] if *len == valid),
        "{effects:?}"
    );
}

/// safety-fuzz-vfs 1 (`vfs_journal_model/torn-inside-a-file-that-is-a-journal.hex`): an
/// append torn inside a file whose bytes are another journal loads none of that journal.
#[test]
fn a_torn_file_that_is_a_journal_loads_none_of_its_records() {
    let mut sim = Sim::new(4, Faults::default());
    let file = name("j");

    // Another VFS's journal, in which `a` moves to {a: xyz} eight times.
    let mut other = Vfs::recover(name("other"), 0, &[]).0;
    let forged = other.commit("a", other.empty(), &[write("a", "xyz")]).unwrap().root;
    for _ in 0..7 {
        other.set_head("a", forged).unwrap();
    }
    let mut other_sim = Sim::new(5, Faults::default());
    settle(&mut other_sim, &mut other, &mut Vec::new());
    let embedded = other_sim.contents(&name("other"));

    let mut vfs = Vfs::recover(file.clone(), 0, &[]).0;
    let first = vfs.commit("a", vfs.empty(), &[write("a", "xyz")]).unwrap().root;
    let q = vfs.commit("a", first, &[write("a", "q")]).unwrap().root;
    settle(&mut sim, &mut vfs, &mut Vec::new());
    let before = sim.contents(&file).len();
    let ops = [Op::Write { path: "j".into(), bytes: embedded.clone(), mode: Mode::Regular }];
    vfs.commit("b", vfs.empty(), &ops).unwrap();
    settle(&mut sim, &mut vfs, &mut Vec::new());
    let all = sim.contents(&file);
    // The append's first record is the file's: torn after the whole embedded journal.
    let torn = &all[..before + journal::HEADER + embedded.len()];
    assert_eq!(&torn[before + journal::HEADER..], &embedded[..]);

    let (recovered, found) = Vfs::recover(file, 0, torn);
    assert_eq!(recovered.head("a"), Some(q), "the durable state, not the embedded one");
    assert_eq!(recovered.sequence("a"), Some(2));
    assert_eq!(recovered.head("b"), None);
    assert_eq!(found.skipped, torn.len() - before);
}

/// safety-fuzz-vfs 2 (`vfs_journal_model/lost-node-made-again-after-recovery.hex`): a head
/// record past damage stays gone after the root it named is made again.
#[test]
fn a_head_lost_to_damage_does_not_come_back_when_its_root_is_made_again() {
    let mut sim = Sim::new(6, Faults::default());
    let file = name("j");
    let mut vfs = Vfs::recover(file.clone(), 0, &[]).0;
    let first = vfs.commit("a", vfs.empty(), &[write("a", "xyz")]).unwrap().root;
    settle(&mut sim, &mut vfs, &mut Vec::new());
    let before = sim.contents(&file).len();
    let both = vfs.commit("a", first, &[write("b", "w")]).unwrap().root;
    settle(&mut sim, &mut vfs, &mut Vec::new());
    let mut damaged = sim.contents(&file);
    // The first record of the second append is `w`'s file: flip a bit of its payload.
    assert_eq!(damaged[before + journal::HEADER], b'w');
    damaged[before + journal::HEADER] ^= 1;

    let (mut vfs, found) = Vfs::recover(file.clone(), 0, &damaged);
    assert_eq!(vfs.head("a"), Some(first));
    assert_eq!(found.skipped, damaged.len() - before);
    let mut sim = Sim::new(7, Faults::default());
    sim.change(&file, &damaged);
    let made = vfs.commit("b", vfs.empty(), &[write("a", "xyz"), write("b", "w")]).unwrap().root;
    assert_eq!(made, both);
    settle(&mut sim, &mut vfs, &mut Vec::new());

    let (again, found) = Vfs::recover(file.clone(), 0, &sim.contents(&file));
    assert_eq!((found.skipped, found.heads_dropped), (0, 0));
    assert_eq!(again.head("a"), Some(first), "the dropped move stays dropped");
    assert_eq!(again.head("b"), Some(both));
}

/// safety-fuzz-vfs 3 (`vfs_journal_records/sequence-at-the-largest.hex`,
/// `head-at-the-largest-sequence.hex`): a sequence number no journal reaches is refused, so the
/// next commit's never overflows.
#[test]
fn a_sequence_number_no_journal_reaches_is_refused() {
    let (_, _, head, _, sequence) = journal::KINDS;
    let largest = journal::framed(sequence, &u64::MAX.to_le_bytes());
    let (mut vfs, found) = Vfs::recover(name("j"), 0, &largest);
    assert_eq!((found.records, found.skipped), (0, largest.len()));
    vfs.commit("w", vfs.empty(), &[write("a", "1")]).unwrap();
    assert_eq!(vfs.sequence("w"), Some(1));

    let empty = Vfs::recover(name("j"), 0, &[]).0.empty();
    let mut payload = u64::MAX.to_le_bytes().to_vec();
    payload.extend_from_slice(&empty.0);
    payload.push(b'a');
    let moved = journal::framed(head, &payload);
    let (vfs, found) = Vfs::recover(name("j"), 0, &moved);
    assert_eq!((found.records, vfs.head("a")), (0, None));

    // The largest a journal can reach is taken.
    let high = journal::framed(sequence, &((1u64 << 63) - 2).to_le_bytes());
    let (mut vfs, found) = Vfs::recover(name("j"), 0, &high);
    assert_eq!(found.records, 1);
    vfs.commit("w", vfs.empty(), &[write("a", "1")]).unwrap();
    assert_eq!(vfs.sequence("w"), Some((1 << 63) - 1));
}

/// A head record naming a root the file never held is dropped, and the file is rewritten without
/// it rather than kept, so a later commit making that root cannot bring the head back.
#[test]
fn a_dropped_head_is_rewritten_out_of_the_journal() {
    let (_, _, head, _, _) = journal::KINDS;
    let mut payload = 1u64.to_le_bytes().to_vec();
    payload.extend_from_slice(&ContentId::of(DIR_DOMAIN, b"never made").0);
    payload.push(b'a');
    let bytes = journal::framed(head, &payload);
    let (mut vfs, found) = Vfs::recover(name("j"), 0, &bytes);
    assert_eq!((found.records, found.skipped, found.heads_dropped), (1, 0, 1));
    assert_eq!(vfs.head("a"), None);
    let mut effects = Vec::new();
    vfs.flush(&mut effects);
    assert!(matches!(effects.as_slice(), [Effect::Write { .. }]), "{effects:?}");
}

/// safety-fuzz-vfs 4: a directory record whose names are not distinct path components in the
/// order the tree writes them ends recovery there.
#[test]
fn a_directory_record_naming_an_entry_twice_or_no_path_is_refused() {
    let (file, dir, _, _, _) = journal::KINDS;
    let x = journal::framed(file, b"x");
    let id = ContentId::of(FILE_DOMAIN, b"x");
    let entry = |name: &str| {
        let mut out = (name.len() as u32).to_le_bytes().to_vec();
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&Mode::Regular.git().to_le_bytes());
        out.extend_from_slice(&id.0);
        out
    };
    let good = [entry("a"), entry("b")].concat();
    let bytes = [x.clone(), journal::framed(dir, &good)].concat();
    assert_eq!(Vfs::recover(name("j"), 0, &bytes).1.records, 2);
    let refused: [&[&str]; 6] = [&["f", "f"], &["b", "a"], &[""], &["."], &[".."], &["a/b"]];
    for names in refused {
        let payload: Vec<u8> = names.iter().flat_map(|it| entry(it)).collect();
        let bytes = [x.clone(), journal::framed(dir, &payload)].concat();
        let (_, found) = Vfs::recover(name("j"), 0, &bytes);
        assert_eq!((found.records, found.skipped), (1, bytes.len() - x.len()), "{names:?}");
    }
}

#[test]
fn the_journal_round_trips_without_faults() {
    let file = name("j");
    let mut sim = Sim::new(1, Faults::default());
    let mut vfs = Vfs::recover(file.clone(), 10, &[]).0;
    let empty = vfs.empty();
    let one = vfs.commit("w", empty, &[write("a/b", "1"), write("c", "2")]).unwrap();
    let two = vfs.commit("w", one.root, &[edit("c", 0, 1, "3")]).unwrap();
    let three = vfs.commit("other", empty, &[write("a/b", "1")]).unwrap();
    assert!(!vfs.owns(Tag(9)) && vfs.owns(Tag(10)) && !vfs.owns(Tag(14)));
    let durable = settle(&mut sim, &mut vfs, &mut Vec::new());
    assert_eq!(durable, vec![one.ticket, two.ticket, three.ticket]);
    let bytes = sim.contents(&file);
    let (recovered, found) = Vfs::recover(file, 10, &bytes);
    assert_eq!((found.skipped, found.heads_dropped), (0, 0));
    // Seven nodes (`1`, `a`, `2`, the first root, `3`, the second root, the other root) and
    // three heads; `a` is shared by both heads and written once.
    assert_eq!(found.records, 10);
    assert_eq!(text(&recovered, recovered.head("w").unwrap(), "c").as_deref(), Some("3"));
    assert_eq!(contents(&recovered, recovered.head("other").unwrap()).len(), 1);
}

#[test]
fn collecting_rewrites_the_journal_as_what_the_heads_reach() {
    let file = name("state/journal");
    let mut sim = Sim::new(5, Faults::default());
    let mut vfs = Vfs::recover(file.clone(), 0, &[]).0;
    let mut root = vfs.empty();
    for at in 0..40 {
        let big = at.to_string().repeat(1000);
        root = vfs
            .commit("w", root, &[write("big", &big), write("d/small", &at.to_string())])
            .unwrap()
            .root;
    }
    vfs.set_head("gone", root).unwrap();
    vfs.remove_head("gone").unwrap();
    settle(&mut sim, &mut vfs, &mut Vec::new());
    let before = sim.contents(&file).len();

    vfs.collect(&[]);
    let mut effects = Vec::new();
    vfs.flush(&mut effects);
    let [Effect::Write { tag, .. }] = effects.as_slice() else { panic!("{effects:?}") };
    let tag = *tag;
    // A commit made while the rewrite is in flight is appended after it.
    let late = vfs.commit("w", root, &[write("late", "1")]).unwrap();
    // A failed rewrite is made again.
    effects.clear();
    let failed = Completion { tag, done: Done::Failed(IoError("no space".into())) };
    assert_eq!(vfs.complete(failed, &mut effects), vec![]);
    assert!(matches!(effects.as_slice(), [Effect::Write { .. }]), "{effects:?}");
    let durable = settle(&mut sim, &mut vfs, &mut effects);
    assert_eq!(durable, vec![late.ticket]);

    let after = sim.contents(&file);
    assert!(after.len() < before / 10, "{} bytes of {before}", after.len());
    let (back, found) = Vfs::recover(file.clone(), 0, &after);
    assert_eq!((found.skipped, found.heads_dropped), (0, 0));
    assert_eq!(back.heads(), vec![("w", late.root)]);
    assert_eq!(contents(&back, late.root), contents(&vfs, late.root));
    assert_eq!(back.sequence("w"), vfs.sequence("w"));
    assert_eq!(back.journal.last_sequence(), vfs.journal.last_sequence());

    // Nothing appended since: a collection asks for no rewrite.
    vfs.collect(&[]);
    settle(&mut sim, &mut vfs, &mut Vec::new());
    vfs.collect(&[]);
    let mut effects = Vec::new();
    vfs.flush(&mut effects);
    assert_eq!(effects, vec![]);
}

/// An op's bytes go over the wire as one byte string (the last field written), and the array
/// serde writes by default is a second encoding of them, so it is refused.
#[test]
fn an_ops_bytes_are_one_byte_string() {
    for op in [write("a/b", "\u{1}\u{2}\u{3}"), edit("a/b", 1, 2, "\u{1}\u{2}\u{3}")] {
        let item = blaze_protocol::cbor::to_vec(&op).unwrap();
        assert!(item.ends_with(&[0x43, 1, 2, 3]), "{item:x?}");
        assert_eq!(blaze_protocol::cbor::from_slice::<Op>(&item).unwrap(), op);
        let mut array = item.clone();
        let head = array.len() - 4;
        array[head] = 0x83;
        assert!(blaze_protocol::cbor::from_slice::<Op>(&array).is_err(), "{array:x?}");
    }
}

/// A [`crate::Jobs`] that runs each job on a thread of its own, as a pool would.
fn on_threads(n: usize, job: &(dyn Fn(usize) + Sync)) {
    std::thread::scope(|scope| {
        for at in 0..n {
            scope.spawn(move || job(at));
        }
    });
}

/// Hashing a commit's files in runs on workers gives each file's own ID, as one call does,
/// whatever the sizes (one file over a run, many under one, empty ones).
#[test]
fn files_hashed_on_workers_are_their_own_ids() {
    let mut rng = Rng::new(31);
    for round in 0..12 {
        let files: Vec<Vec<u8>> = (0..1 + rng.below(120))
            .map(|_| {
                let len = match rng.below(8) {
                    0 => 0,
                    1 => rng.below(2 << 20) as usize,
                    _ => rng.below(64 << 10) as usize,
                };
                let seed = rng.below(1 << 32) as u32;
                let byte = |at: u32| (at.wrapping_mul(0x9e37_79b9) ^ seed).to_le_bytes()[3];
                (0..len as u32).map(byte).collect()
            })
            .collect();
        let inputs: Vec<&[u8]> = files.iter().map(Vec::as_slice).collect();
        let want: Vec<ContentId> = inputs.iter().map(|it| ContentId::of(FILE_DOMAIN, it)).collect();
        for jobs in [None, Some(on_threads as crate::Jobs)] {
            let mut out = vec![ContentId::of("stale", b"")];
            crate::tree::hash_files(&inputs, &mut out, jobs);
            assert_eq!(out, want, "round {round}, on workers: {}", jobs.is_some());
        }
    }
}

/// A tree that hashes on workers makes the same roots as one that does not.
#[test]
fn a_vfs_hashing_on_workers_makes_the_same_roots() {
    let mut rng = Rng::new(32);
    let ops: Vec<Op> = (0..400)
        .map(|at| {
            let len = rng.below(16 << 10) as usize;
            let bytes = (0..len).map(|_| rng.below(256) as u8).collect();
            Op::Write { path: format!("d{}/f{at}", at % 7), bytes, mode: Mode::Regular }
        })
        .collect();
    let (mut serial, mut parallel) = (new_vfs(), new_vfs());
    parallel.hash_on(on_threads);
    let one = serial.commit("w", serial.empty(), &ops).unwrap().root;
    let two = parallel.commit_owned("w", parallel.empty(), ops.clone()).unwrap().root;
    assert_eq!(one, two);
    assert_eq!(contents(&serial, one), contents(&parallel, two));
}

/// An owned commit's bytes are the file's own: the store lends the very buffer the op held.
/// A borrowed commit makes the same root.
#[test]
fn an_owned_commit_keeps_its_bytes_without_a_copy() {
    let mut vfs = new_vfs();
    let bytes: Vec<u8> = (0..100_000u32).map(|it| (it * 7) as u8).collect();
    let at = bytes.as_ptr();
    let ops = vec![Op::Write { path: "a/big".into(), bytes, mode: Mode::Executable }];
    let borrowed = new_vfs().commit("w", vfs.empty(), &ops).unwrap().root;
    let root = vfs.commit_owned("w", vfs.empty(), ops).unwrap().root;
    assert_eq!(root, borrowed);
    let view = vfs.open(View { root }).unwrap();
    let file = vfs.read(view, "a/big").unwrap().expect("the file");
    assert_eq!(file.bytes.as_ptr(), at, "the op's buffer, not a copy");
    assert_eq!(file.mode, Mode::Executable);

    // Rebased onto a head that moved, an untouched write is still taken whole.
    let base = vfs.head("w").unwrap();
    vfs.commit("w", base, &[write("other", "1")]).unwrap();
    let bytes: Vec<u8> = (0..50_000u32).map(|it| (it * 13) as u8).collect();
    let at = bytes.as_ptr();
    let ops = vec![Op::Write { path: "b".into(), bytes, mode: Mode::Regular }];
    let commit = vfs.commit_owned("w", base, ops).unwrap();
    assert!(commit.rebased);
    let view = vfs.open(View { root: commit.root }).unwrap();
    assert_eq!(vfs.read(view, "b").unwrap().expect("the file").bytes.as_ptr(), at);
    assert_eq!(text(&vfs, commit.root, "other").as_deref(), Some("1"));
}

/// A directory whose names stayed and whose contents changed is diffed column against column;
/// it names what the model does, in its order.
#[test]
fn a_directory_whose_names_stayed_diffs_as_the_model() {
    let mut rng = Rng::new(33);
    let mut vfs = new_vfs();
    let mut model = Model::new();
    let ops: Vec<Op> = (0..500)
        .map(|at| {
            let path = format!("{}/{at:03}", ["a", "b/c", "d"][at % 3]);
            model.insert(path.clone(), (Mode::Regular, at.to_string().into_bytes()));
            Op::Write { path, bytes: at.to_string().into_bytes(), mode: Mode::Regular }
        })
        .collect();
    let base = vfs.apply(vfs.empty(), &ops).unwrap();
    for round in 0..40 {
        let mut next = model.clone();
        let mut ops = Vec::new();
        for _ in 0..rng.below(20) {
            let at = rng.below(500) as usize;
            let path = format!("{}/{at:03}", ["a", "b/c", "d"][at % 3]);
            let (mode, bytes) = match rng.below(3) {
                0 => (Mode::Executable, at.to_string().into_bytes()),
                _ => (Mode::Regular, format!("{at} {round}").into_bytes()),
            };
            next.insert(path.clone(), (mode, bytes.clone()));
            ops.push(Op::Write { path, bytes, mode });
        }
        let after = vfs.apply(base, &ops).unwrap();
        let paths: Vec<String> = vfs.diff(base, after).into_iter().map(|it| it.path).collect();
        assert_eq!(paths, model_diff(&model, &next), "round {round}");
        assert!(vfs.diff(base, after).iter().all(|it| it.kind == ChangeKind::Modified));
    }
}

/// A daemon collects after every job: the journal is rewritten only once as much was appended
/// since the last rewrite as it held, so rewrites are few and their bytes bounded by the
/// appends', and every state still recovers.
#[test]
fn collecting_after_every_commit_rewrites_the_journal_amortized() {
    let file = name("state/journal");
    let mut sim = Sim::new(9, Faults::default());
    let mut vfs = Vfs::recover(file.clone(), 0, &[]).0;
    let mut root = vfs.empty();
    let (mut appended, mut rewritten, mut rewrites) = (0, 0, 0);
    for at in 0..300 {
        // Directories of at most ten entries, so a commit appends about its new file.
        let path = format!("{}/{}/f{at}", at / 100, at / 10 % 10);
        let ops = [write(&path, &format!("{at:04}").repeat(500)), write("hot", &at.to_string())];
        root = vfs.commit("w", root, &ops).unwrap().root;
        vfs.collect(&[]);
        let mut effects = Vec::new();
        vfs.flush(&mut effects);
        for effect in &effects {
            match effect {
                Effect::Append { bytes, .. } => appended += bytes.len(),
                Effect::Write { bytes, .. } => {
                    rewritten += bytes.len();
                    rewrites += 1;
                }
                _ => {}
            }
        }
        settle(&mut sim, &mut vfs, &mut effects);
    }
    // Live grows by about what each commit appends, so rewrites come at a constant ratio of
    // growth (about 15 here), where rewriting after every collection is 300.
    assert!(rewrites <= 30, "{rewrites} rewrites");
    assert!(rewritten <= 2 * appended, "{rewritten} rewritten for {appended} appended");
    let (back, found) = Vfs::recover(file, 0, &sim.contents(&name("state/journal")));
    assert_eq!((found.skipped, found.heads_dropped), (0, 0));
    assert_eq!(back.heads(), vec![("w", root)]);
    assert_eq!(contents(&back, root), contents(&vfs, root));
}

/// A collection when due comes once as many nodes were made since the last as it kept, and
/// then drops what `collect` would.
#[test]
fn a_collection_if_due_waits_until_it_is_paid_for() {
    let mut vfs = new_vfs();
    let mut root = vfs.empty();
    let ops: Vec<Op> = (0..100).map(|at| write(&format!("d/f{at}"), &at.to_string())).collect();
    root = vfs.commit("w", root, &ops).unwrap().root;
    assert!(vfs.collect_if_due(&[]), "nothing was kept before");
    let kept = vfs.tree.len();
    let mut collected = 0;
    for at in 0..400 {
        let before = vfs.tree.len();
        let gone = root;
        root = vfs.commit("w", root, &[write("d/f0", &format!("x{at}"))]).unwrap().root;
        let made = vfs.tree.len() - kept;
        let due = made >= kept;
        assert_eq!(vfs.collect_if_due(&[]), due, "at {at}: {made} made, {kept} kept");
        if due {
            collected += 1;
            assert!(!vfs.has(gone) && vfs.has(root));
            assert!(vfs.tree.len() < before + 4);
            break;
        }
    }
    assert_eq!(collected, 1, "a collection came once enough was made");
}

/// A file record's checksum is over its content ID: a flipped byte of its bytes, its checksum
/// or its kind ends recovery there.
#[test]
fn a_damaged_file_record_is_refused() {
    let (file, ..) = journal::KINDS;
    let good = journal::framed(file, b"some bytes");
    assert_eq!(Vfs::recover(name("j"), 0, &good).1.records, 1);
    for at in [journal::HEADER - 1, journal::HEADER, journal::HEADER + 5, good.len() - 1] {
        let mut bad = good.clone();
        bad[at] ^= 1;
        let (_, found) = Vfs::recover(name("j"), 0, &bad);
        assert_eq!((found.records, found.skipped), (0, bad.len()), "byte {at}");
    }
}
