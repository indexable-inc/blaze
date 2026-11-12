//! The hub's core held to a model (rules dst-not-review, invariants-are-stated): agents'
//! requests drawn by Hegel and run against the real [`Hub`] over a store in memory, with a
//! flat map of paths beside it; after every step each lane's root must be the root of the
//! model's files built from nothing. The planted bugs are in the cores (`crate::Plant`).

use std::collections::BTreeMap;

use blaze_vfs::{Mode, Op};
use bstd::id::ContentId;
use hegel::generators as gs;
use hegel::{Hegel, Settings, TestCase};

use crate::{
    Config, Hub, Memory, Plant,
    git::{self, Format, GitError, Index, Objects, Oid},
    tree::{self, Entry, Kind, Store},
    wire::{self, Change, Envelope, Request, Response, WireError},
};

/// The reference model: a tree is a flat map from each file's path to its mode and bytes,
/// with no directories of its own; a path is a directory while some file is under it.
#[derive(Clone, Default, PartialEq, Debug)]
struct Files(BTreeMap<String, (Mode, Vec<u8>)>);

impl Files {
    fn of(files: &[(&str, &str)]) -> Files {
        Files(
            files
                .iter()
                .map(|(path, text)| (path.to_string(), (Mode::Regular, text.as_bytes().to_vec())))
                .collect(),
        )
    }

    fn is_dir(&self, path: &str) -> bool {
        let under = format!("{path}/");
        self.0.keys().any(|key| key.starts_with(&under))
    }

    /// Whether a path above `path` is a file.
    fn file_above(&self, path: &str) -> bool {
        path.match_indices('/').any(|(at, _)| self.0.contains_key(&path[..at]))
    }

    /// `op` done, or `false` with the files as they were.
    fn apply(&mut self, op: &Op) -> bool {
        match op {
            Op::Write { mode, path, bytes } => {
                if self.is_dir(path) || self.file_above(path) {
                    return false;
                }
                self.0.insert(path.clone(), (*mode, bytes.clone()));
                true
            }
            Op::Edit { at, len, path, bytes } => {
                let Some((_, old)) = self.0.get_mut(path) else { return false };
                let (at, len) = (*at as usize, *len as usize);
                if at + len > old.len() {
                    return false;
                }
                let _ = old.splice(at..at + len, bytes.iter().copied());
                true
            }
            Op::Delete { path } => {
                if self.0.remove(path).is_some() {
                    return true;
                }
                if !self.is_dir(path) {
                    return false;
                }
                let under = format!("{path}/");
                self.0.retain(|key, _| !key.starts_with(&under));
                true
            }
            Op::Rename { to, from } => {
                let under = format!("{from}/");
                if to == from || to.starts_with(&under) {
                    return false;
                }
                if !self.0.contains_key(from) && !self.is_dir(from) {
                    return false;
                }
                if self.0.contains_key(to) || self.is_dir(to) {
                    return false;
                }
                let mut rest = self.clone();
                let mut moved = Vec::new();
                rest.0.retain(|key, file| {
                    let new = if key == from {
                        to.clone()
                    } else if let Some(tail) = key.strip_prefix(&under) {
                        format!("{to}/{tail}")
                    } else {
                        return true;
                    };
                    moved.push((new, file.clone()));
                    false
                });
                if rest.file_above(to) {
                    return false;
                }
                rest.0.extend(moved);
                *self = rest;
                true
            }
        }
    }

    /// Every op, or none.
    fn change(&mut self, ops: &[Op]) -> bool {
        let mut next = self.clone();
        if !ops.iter().all(|op| next.apply(op)) {
            return false;
        }
        *self = next;
        true
    }

    /// The root of these files built from nothing, a directory at a time.
    fn root(&self, store: &mut Memory) -> ContentId {
        let files: Vec<(&str, &(Mode, Vec<u8>))> =
            self.0.iter().map(|(path, file)| (path.as_str(), file)).collect();
        fresh(store, &files)
    }
}

fn fresh(store: &mut Memory, files: &[(&str, &(Mode, Vec<u8>))]) -> ContentId {
    let mut entries = Vec::new();
    let mut dirs: BTreeMap<&str, Vec<(&str, &(Mode, Vec<u8>))>> = BTreeMap::new();
    for (path, file) in files {
        match path.split_once('/') {
            None => entries.push(Entry {
                name: path.to_string(),
                kind: Kind::File(file.0),
                id: tree::write(store, file.1.clone()),
            }),
            Some((name, rest)) => dirs.entry(name).or_default().push((rest, *file)),
        }
    }
    for (name, under) in dirs {
        entries.push(Entry { name: name.to_string(), kind: Kind::Dir, id: fresh(store, &under) });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    tree::build(store, &entries).expect("a small directory builds")
}

const WHO: [&str; 2] = ["ann", "bob"];
const LANES: [&str; 3] = ["x", "y", "z"];
const PATHS: [&str; 7] = ["a", "b", "b/c", "b/d", "e/f/g", "e/f", "h.rs"];
const BYTES: [&[u8]; 4] = [b"", b"one\n", b"fn main() {}\n", b"two two two two\n"];

fn base() -> Files {
    Files::of(&[("a", "base a\n"), ("b/c", "base c\n"), ("e/f/g", "base g\n")])
}

#[derive(Clone, Copy, Debug)]
enum On {
    Any,
    Here,
    Elsewhere,
}

#[derive(Clone, Debug)]
enum Step {
    Open { who: usize, lane: usize },
    Fork { who: usize, from: usize, lane: usize },
    Change { who: usize, lane: usize, on: On, ops: Vec<Op> },
    Read { who: usize, lane: usize, path: usize },
    Close { who: usize, lane: usize },
    Tick { ms: u64 },
}

fn below(tc: &TestCase, most: u8) -> usize {
    tc.draw(gs::integers::<u8>().max_value(most)) as usize
}

fn op(tc: &TestCase) -> Op {
    let path = PATHS[below(tc, 6)].to_string();
    let bytes = BYTES[below(tc, 3)].to_vec();
    match below(tc, 5) {
        0..=2 => Op::Write { mode: Mode::Regular, path, bytes },
        3 => Op::Edit { at: below(tc, 8) as u64, len: below(tc, 8) as u64, path, bytes },
        4 => Op::Delete { path },
        _ => Op::Rename { to: PATHS[below(tc, 6)].to_string(), from: path },
    }
}

#[hegel::composite]
fn step(tc: &TestCase) -> Step {
    let (who, lane) = (below(tc, 1), below(tc, 2));
    match below(tc, 11) {
        0 | 1 => Step::Open { who, lane },
        2 => Step::Fork { who, from: below(tc, 2), lane },
        3..=7 => {
            let on = [On::Any, On::Any, On::Here, On::Elsewhere][below(tc, 3)];
            let ops = (0..=below(tc, 3)).map(|_| op(tc)).collect();
            Step::Change { who, lane, on, ops }
        }
        8 | 9 => Step::Read { who, lane, path: below(tc, 6) },
        10 => Step::Close { who, lane },
        _ => Step::Tick { ms: below(tc, 200) as u64 },
    }
}

/// The hub and its model, stepped together.
struct Sim {
    hub: Hub<Memory>,
    base: ContentId,
    /// Each lane the model holds: whose it is and its files.
    lanes: BTreeMap<String, (usize, Files)>,
    now: u64,
}

impl Sim {
    fn new(plant: Plant) -> Sim {
        let config = Config {
            idle_for: core::time::Duration::from_secs(3600),
            lanes_most: 8,
            eager: true,
        };
        let mut hub = Hub::planted(Memory::new(), config, plant);
        let base = base().root(hub.store_mut());
        hub.add_base(base);
        Sim { hub, base, lanes: BTreeMap::new(), now: 0 }
    }

    /// One step on both; `Err` says how the hub's answer is not the model's.
    fn step(&mut self, step: &Step) -> Result<(), String> {
        match step {
            Step::Open { who, lane } => {
                let name = LANES[*lane];
                let allowed = self.lanes.get(name).is_none_or(|(owner, _)| owner == who);
                let answer = self.hub.receive(WHO[*who], Request::Open { base: self.base, lane: name.to_string() });
                agree(allowed, matches!(answer, Response::Opened { .. }), &answer)?;
                if allowed {
                    self.lanes.entry(name.to_string()).or_insert((*who, base()));
                }
            }
            Step::Fork { who, from, lane } => {
                let (from, name) = (LANES[*from], LANES[*lane]);
                let source = self.lanes.get(from).filter(|(owner, _)| owner == who).cloned();
                let allowed = source.is_some() && !self.lanes.contains_key(name);
                let request = Request::Fork { from: from.to_string(), lane: name.to_string() };
                let answer = self.hub.receive(WHO[*who], request);
                agree(allowed, matches!(answer, Response::Opened { .. }), &answer)?;
                if let (true, Some(source)) = (allowed, source) {
                    self.lanes.insert(name.to_string(), source);
                }
            }
            Step::Change { who, lane, on, ops } => {
                let name = LANES[*lane];
                let on = match on {
                    On::Any => None,
                    On::Here => self.hub.root(name),
                    On::Elsewhere => Some(ContentId([7; 32])),
                };
                let stale = on.is_some() && on != self.hub.root(name);
                let allowed = match self.lanes.get_mut(name) {
                    Some((owner, files)) if *owner == *who && !stale => files.change(ops),
                    _ => false,
                };
                let change = Change { on, ops: ops.clone(), lane: name.to_string() };
                let answer = self.hub.receive(WHO[*who], Request::Change(change));
                agree(allowed, matches!(answer, Response::Root { .. }), &answer)?;
                if let Response::Root { root, .. } = answer
                    && Some(root) != self.hub.root(name)
                {
                    return Err("the change's answer names a root the lane is not at".to_string());
                }
            }
            Step::Read { who, lane, path } => {
                let (name, path) = (LANES[*lane], PATHS[*path]);
                let file = match self.lanes.get(name) {
                    Some((owner, files)) if owner == who => files.0.get(path),
                    _ => None,
                };
                let request =
                    Request::Read { at: 0, len: 1 << 20, lane: name.to_string(), path: path.to_string() };
                let answer = self.hub.receive(WHO[*who], request);
                match (&answer, file) {
                    (Response::File(part), Some((mode, bytes)))
                        if part.bytes == *bytes && part.mode == *mode && part.size == bytes.len() as u64 => {}
                    (Response::Refused(_), None) => {}
                    _ => return Err(format!("a read is not the model's file: {answer:?}")),
                }
            }
            Step::Close { who, lane } => {
                let name = LANES[*lane];
                let allowed = self.lanes.get(name).is_some_and(|(owner, _)| owner == who);
                let answer = self.hub.receive(WHO[*who], Request::Close { lane: name.to_string() });
                agree(allowed, matches!(answer, Response::Closed { .. }), &answer)?;
                if allowed {
                    self.lanes.remove(name);
                }
            }
            Step::Tick { ms } => {
                self.now += ms;
                self.hub.advance_clock(core::time::Duration::from_millis(self.now));
            }
        }
        self.hub.take_decisions();
        self.oracle()
    }

    /// The oracle: the hub holds the model's lanes and no other, each at the root of the
    /// model's files built from nothing in a store of their own.
    fn oracle(&mut self) -> Result<(), String> {
        let Response::Lanes(rows) = self.hub.receive("ann", Request::Ls) else {
            return Err("`ls` did not answer with the lanes".to_string());
        };
        let held: Vec<&str> = rows.iter().map(|row| row.lane.as_str()).collect();
        let modelled: Vec<&str> = self.lanes.keys().map(String::as_str).collect();
        if held != modelled {
            return Err(format!("the hub holds the lanes {held:?} and the model {modelled:?}"));
        }
        for (name, (_, files)) in &self.lanes {
            let fresh = files.root(&mut Memory::new());
            if self.hub.root(name) != Some(fresh) {
                return Err(format!(
                    "lane {name}: its root is not the root of the files the model holds, built \
                     from nothing"
                ));
            }
        }
        Ok(())
    }
}

fn agree(model: bool, hub: bool, answer: &Response) -> Result<(), String> {
    match (model, hub) {
        (true, false) => Err(format!("the hub refuses what the model does: {answer:?}")),
        (false, true) => Err(format!("the hub does what the model refuses: {answer:?}")),
        _ => Ok(()),
    }
}

fn run(steps: &[Step], plant: Plant) -> Result<(), String> {
    let mut sim = Sim::new(plant);
    for (at, step) in steps.iter().enumerate() {
        sim.step(step).map_err(|why| format!("step {at} ({step:?}): {why}"))?;
    }
    Ok(())
}

/// Runs `property` over `cases` cases, or `HEGEL_TEST_CASES` of them where that is set.
fn run_cases(cases: u64, property: impl FnMut(TestCase)) {
    let cases =
        std::env::var("HEGEL_TEST_CASES").ok().and_then(|it| it.parse().ok()).unwrap_or(cases);
    Hegel::new(property).settings(Settings::new().test_cases(cases).database(None)).run();
}

/// The invariant: whatever two agents ask in whatever order (opens, forks, changes written
/// against the lane's root, a stale one or none, reads, closes), a lane's root is the root
/// of its files built from nothing: it does not depend on how the lane got there; a change
/// applies whole or not at all; only a lane's owner reads or moves it; and a read answers
/// the model's bytes. `HEGEL_TEST_CASES=<n>` runs it longer.
#[test]
fn a_lanes_root_is_the_root_of_its_files_whatever_was_asked() {
    run_cases(300, |tc| {
        let steps = tc.draw(gs::vecs(step()).min_size(1).max_size(24));
        if let Err(why) = run(&steps, Plant::None) {
            panic!("{why}");
        }
    });
}

fn write(path: &str, bytes: &[u8]) -> Op {
    Op::Write { mode: Mode::Regular, path: path.to_string(), bytes: bytes.to_vec() }
}

fn change(who: usize, on: On, ops: Vec<Op>) -> Step {
    Step::Change { who, lane: 0, on, ops }
}

/// A directed case passes as the hub is and fails, naming `says`, with `plant`.
fn caught(steps: &[Step], plant: Plant, says: &str) {
    assert_eq!(run(steps, Plant::None), Ok(()), "the hub as it is must pass");
    let why = run(steps, plant).expect_err("the oracle missed the planted bug");
    assert!(why.contains(says), "{why}");
}

/// The planted-bug control: with the owner check off, bob's write into ann's lane lands.
#[test]
fn the_oracle_catches_a_lane_changed_by_someone_else() {
    let steps = [Step::Open { who: 0, lane: 0 }, change(1, On::Any, vec![write("a", b"bob's\n")])];
    caught(&steps, Plant::AnyoneMayUse, "the hub does what the model refuses");
}

/// The planted-bug control: a change written against a root the lane is not at is applied.
#[test]
fn the_oracle_catches_a_change_applied_to_a_root_it_was_not_written_for() {
    let steps =
        [Step::Open { who: 0, lane: 0 }, change(0, On::Elsewhere, vec![write("a", b"late\n")])];
    caught(&steps, Plant::ChangeIgnoresOn, "the hub does what the model refuses");
}

/// The planted-bug control: a change whose second op fails keeps its first.
#[test]
fn the_oracle_catches_half_a_change_kept() {
    let ops = vec![write("a", b"half\n"), Op::Delete { path: "nope".to_string() }];
    let steps = [Step::Open { who: 0, lane: 0 }, change(0, On::Any, ops)];
    caught(&steps, Plant::KeepsHalfAChange, "its root is not the root of the files");
}

/// The planted-bug control: a directory emptied by a delete stays in the tree, so the root
/// says how the tree was made.
#[test]
fn the_oracle_catches_a_root_that_depends_on_history() {
    let steps =
        [Step::Open { who: 0, lane: 0 }, change(0, On::Any, vec![Op::Delete { path: "b/c".to_string() }])];
    caught(&steps, Plant::KeepsEmptyDirs, "its root is not the root of the files");
}

fn wide(count: usize) -> Vec<Entry> {
    (0..count)
        .map(|at| Entry {
            name: format!("n{at:05}.rs"),
            kind: Kind::File(Mode::Regular),
            id: ContentId::of(tree::FILE_DOMAIN, &(at as u64).to_le_bytes()),
        })
        .collect()
}

/// How many new nodes a directory of 3000 entries costs when one entry is added before
/// every other.
fn new_nodes(plant: Plant) -> usize {
    let mut store = Memory::new();
    let mut entries = wide(3000);
    tree::build_planted(&mut store, &entries, plant).unwrap();
    let before = store.len();
    let first = Entry { name: "a-first.rs".to_string(), kind: Kind::File(Mode::Regular), id: entries[0].id };
    entries.insert(0, first);
    tree::build_planted(&mut store, &entries, plant).unwrap();
    store.len() - before
}

/// The invariant: a directory's nodes end where its names say, so one entry added to a
/// directory of 3000 makes a few new nodes (at most two a layer), wherever it is added.
#[test]
fn one_entry_more_makes_a_few_new_nodes() {
    let new = new_nodes(Plant::None);
    assert!(new <= 10, "{new} new nodes for one entry");
}

/// The planted-bug control: nodes that end where they are full all move when an entry is
/// added before them.
#[test]
fn the_oracle_catches_nodes_that_end_by_count() {
    let new = new_nodes(Plant::NodesEndByCount);
    assert!(new > 10, "only {new} new nodes: the plant did nothing");
}

#[test]
fn a_wide_directory_lists_and_finds_what_was_built() {
    let mut store = Memory::new();
    let entries = wide(3000);
    let root = tree::build(&mut store, &entries).unwrap();
    assert_eq!(tree::list(&store, root).unwrap(), entries);
    for entry in entries.iter().step_by(97) {
        assert_eq!(tree::entry(&store, root, &entry.name).unwrap(), Some((entry.kind, entry.id)));
    }
    assert_eq!(tree::entry(&store, root, "n00000.rt").unwrap(), None);
    assert_eq!(tree::entry(&store, root, "zzz").unwrap(), None);
    // The same entries make the same root, and a node is more than one entry.
    assert_eq!(tree::build(&mut Memory::new(), &entries).unwrap(), root);
    assert!(store.len() < 400, "{} nodes for 3000 entries", store.len());
}

#[test]
fn a_node_that_is_not_the_encoding_is_refused() {
    let mut store = Memory::new();
    let root = base().root(&mut store);
    let good = store.get(&root).unwrap().to_vec();
    assert!(tree::decode(&good).is_ok());
    // Cut short, bytes after the end, a layer past the bound, names out of order.
    assert!(tree::decode(&good[..good.len() - 1]).is_err());
    let mut long = good.clone();
    long.push(0);
    assert!(tree::decode(&long).is_err());
    let mut high = good.clone();
    high[0] = tree::LAYERS_MOST + 1;
    assert!(tree::decode(&high).is_err());
    let swapped =
        [&[0u8, 2, 0][..], &[0u8, 1, b'b'], &[0u8; 32], &[0u8, 1, b'a'], &[0u8; 32]].concat();
    assert!(tree::decode(&swapped).is_err());
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[test]
fn sha1_and_git_ids_are_gits() {
    assert_eq!(hex(&git::sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
    assert_eq!(hex(&git::sha1(b"abc")), "a9993e364706816aba3e25717850c26c9cd0d89d");
    let long = [b'a'; 200];
    assert_eq!(git::sha1(&long[..119]).len(), 20);
    let id = |kind, body: &[u8]| git::object_id(Format::Sha1, kind, body).to_string();
    assert_eq!(id("blob", b""), "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391");
    assert_eq!(id("blob", b"hello world\n"), "3b18e512dba79e4c8300dd08aeb37f8e728b8dad");
    assert_eq!(id("tree", b""), "4b825dc642cb6eb9a060e54bf8d69288fbee4904");
}

/// A git repository's objects in memory, written by the test's own encoder.
#[derive(Default)]
struct Repo(BTreeMap<Oid, Vec<u8>>);

impl Objects for Repo {
    fn object(&self, oid: &Oid) -> Option<&[u8]> {
        self.0.get(oid).map(Vec::as_slice)
    }
}

impl Repo {
    /// The git tree of `files`, as git writes it: each directory's entries in the order of
    /// their names with `/` after a directory's.
    fn tree(&mut self, format: Format, files: &[(&str, &(Mode, Vec<u8>))]) -> Oid {
        let mut lines: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        let mut dirs: BTreeMap<&str, Vec<(&str, &(Mode, Vec<u8>))>> = BTreeMap::new();
        let line = |mode: &str, name: &str, oid: Oid| {
            [mode.as_bytes(), b" ", name.as_bytes(), b"\0", oid.as_bytes()].concat()
        };
        for (path, file) in files {
            match path.split_once('/') {
                None => {
                    let oid = git::object_id(format, "blob", &file.1);
                    self.0.insert(oid, file.1.clone());
                    let mode = if file.0 == Mode::Executable { "100755" } else { "100644" };
                    lines.push((path.as_bytes().to_vec(), line(mode, path, oid)));
                }
                Some((name, rest)) => dirs.entry(name).or_default().push((rest, *file)),
            }
        }
        for (name, under) in dirs {
            let oid = self.tree(format, &under);
            lines.push((format!("{name}/").into_bytes(), line("40000", name, oid)));
        }
        lines.sort();
        let body: Vec<u8> = lines.into_iter().flat_map(|(_, line)| line).collect();
        let oid = git::object_id(format, "tree", &body);
        self.0.insert(oid, body);
        oid
    }

    fn of(format: Format, files: &Files) -> (Repo, Oid) {
        let mut repo = Repo::default();
        let list: Vec<(&str, &(Mode, Vec<u8>))> =
            files.0.iter().map(|(path, file)| (path.as_str(), file)).collect();
        let tree = repo.tree(format, &list);
        (repo, tree)
    }
}

/// Files whose git order is not their name order (`b.rs` sorts before the directory `b`),
/// with an executable among them.
fn checkout() -> Files {
    let mut files = base();
    files.0.insert("b.rs".to_string(), (Mode::Regular, b"mod c;\n".to_vec()));
    files.0.insert("run.sh".to_string(), (Mode::Executable, b"#!/bin/sh\n".to_vec()));
    files
}

/// The invariant: git tree to root to git tree gives the same object ids. A commit's tree
/// imported is the root the hub itself builds of those files; that root, hashed again by an
/// index that has seen nothing, is the tree's id, and its tree object is git's bytes; and
/// after a change, the lane's root exports as the tree git makes of the changed files.
#[test]
fn a_git_tree_round_trips_through_a_root() {
    for format in [Format::Sha1, Format::Sha256] {
        let files = checkout();
        let (repo, tree_id) = Repo::of(format, &files);
        let mut store = Memory::new();
        let mut index = Index::new(format);
        let root = index.import(&mut store, &repo, tree_id).unwrap();
        assert_eq!(root, files.root(&mut Memory::new()), "the import is the hub's own root");
        assert_eq!(index.id_of(&tree_id), Some(root));
        assert_eq!(index.known(&root), Some(tree_id));
        let mut again = Index::new(format);
        assert_eq!(again.oid(&store, Kind::Dir, root).unwrap(), tree_id);
        assert_eq!(again.tree_object(&store, root).unwrap(), repo.0[&tree_id]);
        // An agent's change, exported: the directories on the changed path are hashed, and
        // the ids are the ones git gives the changed checkout.
        let ops = [write("b/new.rs", b"pub fn new() {}\n"), Op::Delete { path: "a".to_string() }];
        let mut changed = files.clone();
        assert!(changed.change(&ops));
        let moved = tree::apply(&mut store, root, &ops).unwrap();
        let (_, expected) = Repo::of(format, &changed);
        assert_eq!(index.oid(&store, Kind::Dir, moved).unwrap(), expected);
    }
}

/// The planted-bug control, on the import's own check: a tree whose bytes are not the ones
/// git writes for its entries (here, in name order where git puts the directory `b` after
/// `b.rs`) would export under another id, and is refused.
#[test]
fn the_oracle_catches_a_git_tree_that_would_not_round_trip() {
    let format = Format::Sha1;
    let (mut repo, good) = Repo::of(format, &checkout());
    let body = repo.0[&good].clone();
    // The entries, cut where each ends (a NUL and a 20-byte id), then put in name order.
    let mut entries: Vec<Vec<u8>> = Vec::new();
    let mut rest = &body[..];
    while !rest.is_empty() {
        let end = rest.iter().position(|byte| *byte == 0).unwrap() + 21;
        entries.push(rest[..end].to_vec());
        rest = &rest[end..];
    }
    let name = |entry: &Vec<u8>| {
        let space = entry.iter().position(|byte| *byte == b' ').unwrap();
        entry[space + 1..entry.len() - 21].to_vec()
    };
    entries.sort_by_key(name);
    let unsorted = entries.concat();
    assert_ne!(unsorted, body, "the case needs a tree whose two orders differ");
    let bad = git::object_id(format, "tree", &unsorted);
    repo.0.insert(bad, unsorted);
    let mut index = Index::new(format);
    let refused = index.import(&mut Memory::new(), &repo, bad);
    assert!(matches!(refused, Err(GitError::NotCanonical { .. })), "{refused:?}");
    assert!(index.import(&mut Memory::new(), &repo, good).is_ok());
}

#[test]
fn git_objects_that_are_not_what_they_say_are_refused() {
    let format = Format::Sha1;
    let (mut repo, tree_id) = Repo::of(format, &checkout());
    let blob = git::object_id(format, "blob", b"base a\n");
    repo.0.insert(blob, b"forged\n".to_vec());
    let refused = Index::new(format).import(&mut Memory::new(), &repo, tree_id);
    assert!(matches!(refused, Err(GitError::Forged { .. })), "{refused:?}");

    let entry = |mode: &str, name: &str| {
        [mode.as_bytes(), b" ", name.as_bytes(), b"\0", blob.as_bytes()].concat()
    };
    for (mode, says) in [("160000", "submodule"), ("100664", "mode")] {
        let body = entry(mode, "x");
        let oid = git::object_id(format, "tree", &body);
        let mut repo = Repo::default();
        repo.0.insert(oid, body);
        let refused = Index::new(format).import(&mut Memory::new(), &repo, oid).unwrap_err();
        assert!(refused.to_string().contains(says), "{refused}");
    }
}

#[test]
fn a_message_is_the_same_bytes_every_time_and_only_of_this_schema() {
    let change = Change {
        on: Some(ContentId([3; 32])),
        ops: vec![write("src/lib.rs", b"pub fn f() {}\n"), Op::Delete { path: "old.rs".to_string() }],
        lane: "x".to_string(),
    };
    let message = Envelope::new(7, Request::Change(change));
    let bytes = wire::encode(&message).unwrap();
    assert_eq!(wire::encode(&message).unwrap(), bytes);
    assert_eq!(wire::decode::<Request>(&bytes).unwrap(), message);

    let other = Envelope { schema: wire::SCHEMA + 1, ..message };
    let bytes = wire::encode(&other).unwrap();
    assert_eq!(wire::decode::<Request>(&bytes), Err(WireError::Schema { got: wire::SCHEMA + 1 }));
    let long = vec![0; wire::FRAME_MOST + 1];
    assert_eq!(wire::decode::<Request>(&long), Err(WireError::TooLong { len: wire::FRAME_MOST + 1 }));
}

#[test]
fn a_job_names_its_root_and_is_counted_warm_or_cold() {
    let mut sim = Sim::new(Plant::None);
    sim.step(&Step::Open { who: 0, lane: 0 }).unwrap();
    let answer = sim.hub.receive(
        "ann",
        Request::Change(Change { on: None, ops: vec![write("a", b"1\n")], lane: "x".to_string() }),
    );
    let Response::Root { root, check: Some(first), .. } = answer else { panic!("{answer:?}") };
    // Asking for the check of that root is the eager check itself.
    let spec = |at| wire::JobSpec {
        at,
        api: 0,
        all: false,
        kind: wire::Kind::Check,
        lane: "x".to_string(),
        task: "T1".to_string(),
        targets: Vec::new(),
    };
    let asked = sim.hub.receive("ann", Request::Job(spec(root)));
    assert_eq!(asked, Response::Queued { job: first, lane: "x".to_string() });
    // A root the lane was never at, and another's lane, are refused.
    assert!(matches!(sim.hub.receive("ann", Request::Job(spec(ContentId([9; 32])))), Response::Refused(_)));
    assert!(matches!(sim.hub.receive("bob", Request::Job(spec(root))), Response::Refused(_)));
    sim.hub.started(first, "m1", wire::Warmth::Cold(wire::Cold::Base)).unwrap();
    // The next change's check takes the first one's place.
    let answer = sim.hub.receive(
        "ann",
        Request::Change(Change { on: Some(root), ops: vec![write("a", b"2\n")], lane: "x".to_string() }),
    );
    let Response::Root { check: Some(second), .. } = answer else { panic!("{answer:?}") };
    assert!(sim.hub.take_decisions().contains(&crate::Decision::Cancel { job: first }));
    sim.hub.started(second, "m1", wire::Warmth::Warm).unwrap();
    sim.hub.ended(second, wire::Outcome::Passed).unwrap();
    assert!(sim.hub.ended(second, wire::Outcome::Passed).is_err());
    let Response::Lane(detail) = sim.hub.receive("ann", Request::Inspect { lane: "x".to_string() }) else {
        panic!("inspect did not answer with the lane");
    };
    assert_eq!((detail.row.counts.jobs, detail.row.counts.warm, detail.row.counts.cold.base), (2, 1, 1));
    let ended: Vec<_> = detail.jobs.iter().map(|job| (job.job, job.stage.clone())).collect();
    assert_eq!(
        ended,
        [
            (first, wire::Stage::Ended(wire::Outcome::Superseded { by: second })),
            (second, wire::Stage::Ended(wire::Outcome::Passed)),
        ]
    );
}
