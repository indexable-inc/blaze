use std::{collections::BTreeMap, sync::Arc, time::Duration};

use blaze_core::{
    Driver, FileName,
    sim::{Faults, Rng, Sim},
};
use rustc_hash::FxHashMap;

use crate::{
    Change, ChangeKind, CommitError, NodeId, Op, OpError, Tree, Vfs,
    journal::{Journal, Ticket},
};

fn write(path: &str, bytes: &str) -> Op {
    Op::Write { path: path.into(), bytes: Arc::from(bytes.as_bytes()) }
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

fn contents(tree: &Tree, root: NodeId) -> BTreeMap<String, Vec<u8>> {
    tree.files(root, "")
        .into_iter()
        .map(|(path, node)| (path, tree.bytes(node).expect("a file").to_vec()))
        .collect()
}

fn text(tree: &Tree, root: NodeId, path: &str) -> Option<String> {
    tree.read(root, path).map(|it| String::from_utf8(it.to_vec()).expect("utf-8"))
}

#[test]
fn writes_make_directories_and_read_back() {
    let mut tree = Tree::new();
    let root = tree
        .apply(tree.empty(), &[write("src/lib.rs", "fn a() {}"), write("Cargo.toml", "[package]")])
        .unwrap();
    assert_eq!(text(&tree, root, "src/lib.rs").as_deref(), Some("fn a() {}"));
    assert!(tree.is_dir(tree.lookup(root, "src").unwrap()));
    assert_eq!(tree.read(root, "src"), None);
    assert_eq!(tree.lookup(root, "src/main.rs"), None);
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
    assert_eq!(tree.hash(one), tree.hash(three));

    // A directory and a file with the same name and bytes are not the same tree.
    let file = tree.apply(tree.empty(), &[write("a", "")]).unwrap();
    let dir = tree.apply(tree.empty(), &[write("a/b", "")]).unwrap();
    assert_ne!(tree.hash(file), tree.hash(dir));
}

#[test]
fn a_transaction_applies_whole_or_not_at_all() {
    let mut tree = Tree::new();
    let base = tree.apply(tree.empty(), &[write("a", "1")]).unwrap();
    let nodes = tree.len();
    let error = tree.apply(base, &[write("b", "2"), edit("a", 5, 1, "x")]).unwrap_err();
    assert_eq!(error, OpError::OutOfRange { path: "a".into(), at: 5, len: 1, size: 1 });
    assert_eq!(tree.len(), nodes);
    assert_eq!(contents(&tree, base).len(), 1);
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
    assert_eq!(contents(&tree, root), BTreeMap::from([("b/c".into(), b"goodbye world".to_vec())]));
}

#[test]
fn deletes_and_renames_of_directories() {
    let mut tree = Tree::new();
    let base = tree
        .apply(tree.empty(), &[write("src/a.rs", "a"), write("src/x/b.rs", "b"), write("top", "t")])
        .unwrap();
    let moved = tree.apply(base, &[rename("src", "lib/src")]).unwrap();
    assert_eq!(text(&tree, moved, "lib/src/x/b.rs").as_deref(), Some("b"));
    assert_eq!(tree.lookup(moved, "src"), None);
    // Moving a directory reuses its node.
    assert_eq!(tree.lookup(moved, "lib/src"), tree.lookup(base, "src"));

    let gone = tree.apply(base, &[delete("src/x/b.rs")]).unwrap();
    assert_eq!(tree.lookup(gone, "src/x"), None, "an emptied directory goes");
    let all = tree.apply(base, &[delete("src"), delete("top")]).unwrap();
    assert_eq!(all, tree.empty());

    let error = |ops: &[Op]| tree.clone().apply(base, ops).unwrap_err();
    assert_eq!(error(&[rename("src", "src/x/inner")]), OpError::BadPath("src/x/inner".into()));
    assert_eq!(error(&[rename("top", "src")]), OpError::Exists("src".into()));
    assert_eq!(error(&[write("top/a", "")]), OpError::NotADirectory("top/a".into()));
    assert_eq!(error(&[write("src", "")]), OpError::IsADirectory("src".into()));
    assert_eq!(error(&[delete("nope")]), OpError::NotFound("nope".into()));
    assert_eq!(error(&[write("../a", "")]), OpError::BadPath("../a".into()));
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
}

#[test]
fn a_stale_transaction_rebases_or_names_the_conflict() {
    let mut vfs = Vfs::new();
    let empty = vfs.tree.hash(vfs.tree.empty());
    let base = vfs.commit("w", empty, &[write("a", "0123456789"), write("b", "b")]).unwrap().root;
    let base = vfs.tree.hash(base);

    // Another agent edits the start of `a` and adds `c`.
    vfs.commit("w", base, &[edit("a", 0, 2, "AAAA"), write("c", "c")]).unwrap();

    // An edit of the end of `a` made against the old base moves past it.
    let commit = vfs.commit("w", base, &[edit("a", 8, 2, "zz"), write("b", "B")]).unwrap();
    assert!(commit.rebased);
    assert_eq!(text(&vfs.tree, commit.root, "a").as_deref(), Some("AAAA234567zz"));
    assert_eq!(text(&vfs.tree, commit.root, "b").as_deref(), Some("B"));
    assert_eq!(text(&vfs.tree, commit.root, "c").as_deref(), Some("c"));

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

    let unknown = crate::Hash([7; 32]);
    assert_eq!(vfs.commit("w", unknown, &[]), Err(CommitError::UnknownBase(unknown)));
}

#[test]
fn collecting_keeps_what_is_reached() {
    let mut vfs = Vfs::new();
    let empty = vfs.tree.hash(vfs.tree.empty());
    let mut root = empty;
    for at in 0..50 {
        let next = vfs
            .commit("w", root, &[write(&format!("d{}/f{at}", at % 4), &at.to_string())])
            .unwrap()
            .root;
        root = vfs.tree.hash(next);
    }
    let what_if = vfs.tree.apply(vfs.head("w"), &[write("only/here", "x")]).unwrap();
    let before = (contents(&vfs.tree, vfs.head("w")), contents(&vfs.tree, what_if));
    let size = vfs.tree.len();
    let mut keep = vec![what_if];
    vfs.collect(&mut keep);
    assert!(vfs.tree.len() < size / 2, "{} of {size} nodes left", vfs.tree.len());
    assert_eq!((contents(&vfs.tree, vfs.head("w")), contents(&vfs.tree, keep[0])), before);
    assert_eq!(vfs.tree.hash(vfs.head("w")), root);
    // The tree still works after renumbering.
    let next = vfs.commit("w", root, &[write("d0/f0", "new")]).unwrap().root;
    assert_eq!(text(&vfs.tree, next, "d0/f0").as_deref(), Some("new"));
}

/// Random transactions against a map from path to bytes.
#[test]
fn transactions_match_a_model() {
    const PATHS: &[&str] = &["a", "b", "d/a", "d/b", "d/e/a", "f/g/h"];
    for seed in 0..300 {
        let mut rng = Rng::new(seed);
        let mut tree = Tree::new();
        let mut root = tree.empty();
        let mut model: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        for _ in 0..40 {
            let mut ops = Vec::new();
            let mut next = model.clone();
            for _ in 0..=rng.below(3) {
                let path = PATHS[rng.below(PATHS.len() as u64) as usize];
                let bytes = format!("{seed}-{}", rng.below(1000)).into_bytes();
                let op = match rng.below(3) {
                    0 => Op::Write { path: path.into(), bytes: Arc::from(bytes.as_slice()) },
                    1 => match next.get(path) {
                        Some(old) => {
                            let at = rng.below(old.len() as u64 + 1);
                            let len = rng.below(old.len() as u64 - at + 1);
                            Op::Edit { path: path.into(), at, len, bytes }
                        }
                        None => continue,
                    },
                    _ if next.contains_key(path) => delete(path),
                    _ => continue,
                };
                // The paths never put a file where a directory is.
                if !model_apply(&mut next, &op) {
                    continue;
                }
                ops.push(op);
            }
            root = tree
                .apply(root, &ops)
                .unwrap_or_else(|error| panic!("seed {seed}: {error} for {ops:?}"));
            model = next;
            assert_eq!(contents(&tree, root), model, "seed {seed}");
        }
    }
}

fn model_apply(model: &mut BTreeMap<String, Vec<u8>>, op: &Op) -> bool {
    let clashes = |model: &BTreeMap<String, Vec<u8>>, path: &str| {
        model
            .keys()
            .any(|it| it.starts_with(&format!("{path}/")) || path.starts_with(&format!("{it}/")))
    };
    match op {
        Op::Write { path, bytes } => {
            if clashes(model, path) {
                return false;
            }
            model.insert(path.clone(), bytes.to_vec());
        }
        Op::Edit { path, at, len, bytes } => {
            let old = model.get_mut(path).expect("edits name a file");
            old.splice(*at as usize..(at + len) as usize, bytes.iter().copied());
        }
        Op::Delete { path } => {
            model.remove(path);
        }
        Op::Rename { .. } => unreachable!(),
    }
    true
}

/// One run of the journal under the simulated machine: commits from two workspaces, some made
/// against a stale base, injected IO failures, collections, and power cuts. After each cut the
/// recovered heads must hold every acknowledged commit, each head must be a whole commit, and
/// its files must be the ones that commit made.
fn journal_run(seed: u64) {
    let file = FileName::new("vfs/journal").unwrap();
    let mut rng = Rng::new(seed ^ 0x5eed);
    let faults =
        Faults { fail_one_in: 1 + rng.below(20) as u32, max_latency: Duration::from_micros(300) };
    let mut sim = Sim::new(seed, faults);
    let mut journal = Journal::new(Vfs::new(), file.clone(), 0);
    let mut effects = Vec::new();
    let mut done = Vec::new();
    // Each commit's sequence number, and the files it left its head with.
    let mut commits: FxHashMap<u64, (String, BTreeMap<String, Vec<u8>>)> = FxHashMap::default();
    let mut tickets: FxHashMap<Ticket, (String, u64)> = FxHashMap::default();
    let mut acknowledged: FxHashMap<String, u64> = FxHashMap::default();
    let mut old_bases = Vec::new();

    for step in 0..300 {
        match rng.below(100) {
            0..=54 => {
                let workspace = if rng.one_in(3) { "b" } else { "a" };
                let head = journal.vfs().head(workspace);
                let tree = &journal.vfs().tree;
                let base = match old_bases.get(rng.below(old_bases.len() as u64 * 4) as usize) {
                    Some(&old) if tree.node(&old).is_some() => old,
                    _ => tree.hash(head),
                };
                let mut ops = Vec::new();
                for _ in 0..=rng.below(3) {
                    let path = format!(
                        "{}/f{}",
                        ["src", "src/x", "tests"][rng.below(3) as usize],
                        rng.below(6)
                    );
                    let bytes = format!("{seed}:{step}:{}", rng.below(1 << 20));
                    ops.push(match (rng.below(4), tree.read(tree.node(&base).unwrap(), &path)) {
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
                match journal.commit(workspace, base, &ops) {
                    Ok((commit, ticket)) => {
                        let sequence = journal.sequence(workspace).unwrap();
                        commits.insert(
                            sequence,
                            (workspace.into(), contents(&journal.vfs().tree, commit.root)),
                        );
                        tickets.insert(ticket, (workspace.into(), sequence));
                        old_bases.push(journal.vfs().tree.hash(commit.root));
                    }
                    Err(CommitError::Conflict { .. } | CommitError::Op(_)) => {}
                    Err(error) => panic!("seed {seed}: {error}"),
                }
                journal.flush(&mut effects);
            }
            55..=94 => {
                sim.submit(&mut effects);
                sim.poll(&mut done, Duration::ZERO);
                for completion in done.drain(..) {
                    for ticket in journal.complete(completion, &mut effects) {
                        let (workspace, sequence) =
                            tickets.remove(&ticket).expect("a ticket handed out");
                        let at = acknowledged.entry(workspace).or_default();
                        *at = (*at).max(sequence);
                    }
                }
            }
            95..=96 => {
                journal.collect(&mut Vec::new());
                old_bases.retain(|it| journal.vfs().tree.node(it).is_some());
            }
            _ => {
                sim.submit(&mut effects);
                sim.crash();
                effects.clear();
                tickets.clear();
                old_bases.clear();
                let (recovered, _) = Journal::recover(file.clone(), 0, &sim.contents(&file));
                journal = recovered;
                for (workspace, root) in journal.vfs().heads() {
                    let sequence = journal.sequence(workspace).unwrap();
                    let (by, files) = commits.get(&sequence).unwrap_or_else(|| {
                        panic!("seed {seed}: head {sequence} was never committed")
                    });
                    assert_eq!(by, workspace, "seed {seed}: head {sequence}");
                    assert_eq!(
                        &contents(&journal.vfs().tree, root),
                        files,
                        "seed {seed}: head {sequence}"
                    );
                }
                for (workspace, &sequence) in &acknowledged {
                    let recovered = journal.sequence(workspace).unwrap_or(0);
                    assert!(
                        recovered >= sequence,
                        "seed {seed}: {workspace} acknowledged {sequence}, recovered {recovered}"
                    );
                }
                // Sequences after the cut may reuse numbers of commits it lost.
                let last = journal
                    .vfs()
                    .heads()
                    .filter_map(|(it, _)| journal.sequence(it))
                    .max()
                    .unwrap_or(0);
                commits.retain(|&sequence, _| sequence <= last);
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
/// it could borrow their first bytes for its checksum and hide them (seed 2147 before the cut).
#[test]
fn a_recovered_journal_cuts_its_torn_tail_before_appending() {
    let file = FileName::new("j").unwrap();
    let mut journal = Journal::new(Vfs::new(), file.clone(), 0);
    let empty = journal.vfs().tree.hash(journal.vfs().tree.empty());
    journal.commit("w", empty, &[write("a", "1")]).unwrap();
    let mut effects = Vec::new();
    journal.flush(&mut effects);
    let Some(blaze_core::Effect::Append { bytes, .. }) = effects.pop() else { panic!("an append") };
    let torn = &bytes[..bytes.len() - 1];

    let (mut journal, found) = Journal::recover(file, 0, torn);
    assert_eq!((found.heads_dropped, found.valid < torn.len()), (0, true));
    let root = journal.vfs().head("w");
    journal.commit("w", journal.vfs().tree.hash(root), &[write("b", "2")]).unwrap();
    journal.flush(&mut effects);
    assert!(
        matches!(effects.as_slice(), [blaze_core::Effect::Truncate { len, .. }] if *len == found.valid as u64),
        "{effects:?}"
    );
}

#[test]
fn the_journal_round_trips_without_faults() {
    let file = FileName::new("j").unwrap();
    let mut sim = Sim::new(1, Faults::default());
    let mut journal = Journal::new(Vfs::new(), file.clone(), 10);
    let empty = journal.vfs().tree.hash(journal.vfs().tree.empty());
    let (one, first) = journal.commit("w", empty, &[write("a/b", "1"), write("c", "2")]).unwrap();
    let base = journal.vfs().tree.hash(one.root);
    let (_, second) = journal.commit("w", base, &[edit("c", 0, 1, "3")]).unwrap();
    let (_, third) = journal.commit("other", empty, &[write("a/b", "1")]).unwrap();
    let mut effects = Vec::new();
    journal.flush(&mut effects);
    let mut durable = Vec::new();
    let mut done = Vec::new();
    while !effects.is_empty() {
        sim.submit(&mut effects);
        while sim.poll(&mut done, Duration::ZERO) > 0 {
            for completion in done.drain(..) {
                assert!(journal.owns(completion.tag));
                durable.extend(journal.complete(completion, &mut effects));
            }
            sim.submit(&mut effects);
        }
    }
    assert_eq!(durable, vec![first, second, third]);
    let bytes = sim.contents(&file);
    let (recovered, found) = Journal::recover(file, 10, &bytes);
    assert_eq!((found.skipped, found.heads_dropped), (0, 0));
    // Seven nodes (`1`, `a`, `2`, the first root, `3`, the second root, the other root) and
    // three heads; `a` is shared by both workspaces and written once.
    assert_eq!(found.records, 10);
    assert_eq!(text(&recovered.vfs().tree, recovered.vfs().head("w"), "c").as_deref(), Some("3"));
    assert_eq!(contents(&recovered.vfs().tree, recovered.vfs().head("other")).len(), 1);
}
