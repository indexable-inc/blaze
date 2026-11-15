//! The world against a fresh database: over any run of edits and commits to a small crate graph
//! (a chain or a diamond, each crate its own source root), what the database answers is what a
//! fresh database built from the world as it now is answers.

use std::alloc::Global;

use blaze_sim::{Config, Tape};
use span::TextSize;
use triomphe::Arc;

use super::{EditDb, add, depend, ran, test_db};
use crate::{
    ContentId, CrateBuilderId, CrateGraphBuilder, CrateKey, Db, Env, FileId, FileSet, Given, Plant,
    SourceRoot, SourceRootId, VfsPath, all_crates, source_root_crates,
};

/// The kind the model's gifts are given as ([`crate::Edit::give`]).
struct Gift;

/// One file of the world: the crate whose source root it is in, its path there and its text.
#[derive(Clone, Debug)]
struct FileModel {
    root: u32,
    path: String,
    text: String,
}

/// One crate of the world: its root file (never moved), its variables (by number, see
/// [`env_of`]) and what it was given.
#[derive(Clone, Debug)]
struct CrateModel {
    root_file: u32,
    env: u8,
    gift: Option<u32>,
}

/// The world: crate `i` is rooted in source root `i` and depends on [`WorldModel::deps`].
#[derive(Clone, Debug)]
struct WorldModel {
    diamond: bool,
    crates: Vec<CrateModel, Global>,
    files: Vec<FileModel, Global>,
}

impl WorldModel {
    /// The crates `at` depends on: the one before it in a chain; in a diamond the base for the
    /// middle crates and the middle ones for the top (with the base too below four crates).
    fn deps(&self, at: usize) -> Vec<usize, Global> {
        let count = self.crates.len();
        let mut deps = Vec::new_in(Global);
        match (self.diamond, at) {
            (_, 0) => {}
            (false, at) => deps.push(at - 1),
            (true, at) if at < count - 1 => deps.push(0),
            (true, _) => {
                deps.extend(1..count - 1);
                if count <= 3 {
                    deps.push(0);
                }
            }
        }
        deps
    }

    /// The file set of source root `root`.
    fn file_set(&self, root: u32) -> FileSet {
        let mut set = FileSet::default();
        for (at, file) in self.files.iter().enumerate() {
            if file.root == root {
                set.insert(FileId::from_raw(at as u32), VfsPath::new_virtual_path(file.path.clone()));
            }
        }
        set
    }
}

fn env_of(at: u8) -> Env {
    match at {
        0 => Env::default(),
        n => [("KEY".to_owned(), n.to_string()), ("OTHER".to_owned(), "x".to_owned())].into_iter().collect(),
    }
}

/// Puts `world` in `db` as a loader does: the files and their roots in one commit, then the
/// graph and the gifts to its crates in one. The key of each crate, in order.
fn put_world(db: &mut Db, world: &WorldModel) -> Vec<CrateKey, Global> {
    db.commit_with(|edit| {
        for (at, file) in world.files.iter().enumerate() {
            let id = FileId::from_raw(at as u32);
            edit.set_file_text(id, &file.text).set_file_source_root(id, SourceRootId(file.root));
        }
        for root in 0..world.crates.len() as u32 {
            edit.set_source_root(SourceRootId(root), Arc::new(SourceRoot::new_local(world.file_set(root))));
        }
    });
    let mut graph = CrateGraphBuilder::default();
    let mut ids: Vec<CrateBuilderId, Global> = Vec::new_in(Global);
    for (at, krate) in world.crates.iter().enumerate() {
        let id = add(&mut graph, FileId::from_raw(krate.root_file), Default::default(), env_of(krate.env));
        for dep in world.deps(at) {
            depend(&mut graph, id, ids[dep], &format!("d{dep}"));
        }
        ids.push(id);
    }
    // The graph and the gifts to its new crates, in one commit: a crate the edit adds has its
    // key before the commit (`Edit::crate_key`).
    let (keys, _) = db.edit_with(|edit| {
        edit.set_crate_graph(graph);
        let mut keys = Vec::new_in(Global);
        keys.extend(ids.iter().map(|&id| edit.crate_key(id)));
        for (krate, &key) in world.crates.iter().zip(&keys) {
            if let Some(gift) = krate.gift {
                edit.give::<Gift>(key, Given::new(gift));
            }
        }
        keys
    });
    keys
}

#[derive(Clone, Debug)]
enum Op {
    /// A file's text set (`file` taken modulo the files there are).
    Text { file: u8, text: String },
    /// A crate's variables set.
    Env { krate: u8, env: u8 },
    /// A crate given a value.
    Gift { krate: u8, value: u32 },
    /// A new file put in a crate's source root.
    AddFile { krate: u8 },
    /// A file that is no crate's root moved to another crate's source root.
    Move { file: u8, to: u8 },
    /// The edit so far committed, and the database compared with a fresh one.
    Commit,
}

fn draw_text(tape: &Tape) -> String {
    tape.text_from("ab\n", 0..=20, "text")
}

fn op(tape: &Tape) -> Op {
    match tape.int_in(0..=9u8, "op") {
        0..=2 => Op::Text { file: tape.int::<u8>("file"), text: draw_text(tape) },
        3 => Op::Env { krate: tape.int::<u8>("krate"), env: tape.int_in(0..=2u8, "env") },
        4 => Op::Gift { krate: tape.int::<u8>("krate"), value: tape.int_in(0..=3u32, "gift") },
        5 => Op::AddFile { krate: tape.int::<u8>("krate") },
        6 => Op::Move { file: tape.int::<u8>("file"), to: tape.int::<u8>("to") },
        _ => Op::Commit,
    }
}

/// 2-4 crates in a chain or a diamond, each with 1-3 files in its own source root and its own
/// variables.
fn world(tape: &Tape) -> WorldModel {
    let diamond = tape.bool("diamond");
    let count = tape.int_in(2..=4u32, "crates");
    let mut crates = Vec::new_in(Global);
    let mut files = Vec::new_in(Global);
    for root in 0..count {
        let root_file = files.len() as u32;
        files.push(FileModel { root, path: format!("/c{root}/lib.rs"), text: draw_text(tape) });
        for _ in 1..tape.int_in(1..=3u32, "files") {
            let at = files.len();
            files.push(FileModel { root, path: format!("/c{root}/f{at}.rs"), text: draw_text(tape) });
        }
        let gift = tape.bool("given").then(|| tape.int_in(0..=3u32, "gift"));
        crates.push(CrateModel { root_file, env: tape.int_in(0..=2u8, "env"), gift });
    }
    WorldModel { diamond, crates, files }
}

/// Applies `op` to `world` and writes it in `edit`: whether anything was written that may
/// move an arena (an equal text or variables write moves none).
fn apply(edit: &mut crate::db::Edit<'_>, world: &mut WorldModel, keys: &[CrateKey], op: &Op) -> bool {
    let krates = world.crates.len();
    match op {
        Op::Text { file, text } => {
            let at = *file as usize % world.files.len();
            edit.set_file_text(FileId::from_raw(at as u32), text);
            let moved = world.files[at].text != *text;
            world.files[at].text = text.clone();
            moved
        }
        Op::Env { krate, env } => {
            let at = *krate as usize % krates;
            edit.set_crate_env(keys[at], env_of(*env));
            let moved = world.crates[at].env != *env;
            world.crates[at].env = *env;
            moved
        }
        Op::Gift { krate, value } => {
            let at = *krate as usize % krates;
            edit.give::<Gift>(keys[at], Given::new(*value));
            world.crates[at].gift = Some(*value);
            // Two gifts are equal only when they are one value: a gift always moves its crate.
            true
        }
        Op::AddFile { krate } => {
            let root = (*krate as usize % krates) as u32;
            let at = world.files.len() as u32;
            world.files.push(FileModel { root, path: format!("/c{root}/f{at}.rs"), text: String::new() });
            let id = FileId::from_raw(at);
            edit.set_file_text(id, "").set_file_source_root(id, SourceRootId(root));
            edit.set_source_root(SourceRootId(root), Arc::new(SourceRoot::new_local(world.file_set(root))));
            true
        }
        Op::Move { file, to } => {
            let at = *file as usize % world.files.len();
            let to = (*to as usize % krates) as u32;
            let from = world.files[at].root;
            if from == to || world.crates.iter().any(|krate| krate.root_file as usize == at) {
                return false;
            }
            world.files[at] = FileModel { root: to, path: format!("/c{to}/f{at}.rs"), ..world.files[at].clone() };
            edit.set_file_source_root(FileId::from_raw(at as u32), SourceRootId(to));
            for root in [from, to] {
                edit.set_source_root(SourceRootId(root), Arc::new(SourceRoot::new_local(world.file_set(root))));
            }
            true
        }
        Op::Commit => unreachable!("a commit is the driver's"),
    }
}

/// What a crate is, by value: what two databases' crates of one world must agree on.
#[derive(PartialEq, Eq, Debug)]
struct CrateView {
    content: ContentId,
    root_file: FileId,
    edition: span::Edition,
    deps: Vec<(String, ContentId), Global>,
    env: Env,
    gift: Option<u32>,
}

fn crate_view(db: &Db, key: CrateKey) -> CrateView {
    let krate = db.krate(key);
    let data = krate.data(db);
    let mut deps = Vec::new_in(Global);
    deps.extend(data.dependencies.iter().map(|dep| (dep.name.to_string(), dep.crate_id.content(db))));
    CrateView {
        content: krate.content(db),
        root_file: data.root_file_id,
        edition: data.edition,
        deps,
        env: krate.env(db).clone(), // clone: a test's view, compared once
        gift: krate.given::<Gift, u32>(db).copied(),
    }
}

fn contents(db: &Db, crates: &[crate::Crate]) -> Vec<ContentId, Global> {
    let mut out = Vec::new_in(Global);
    out.extend(crates.iter().map(|krate| krate.content(db)));
    out
}

/// `got` (the database's) against `want` (a fresh one's) for `what`.
fn agree<T: PartialEq + std::fmt::Debug>(step: usize, what: &str, got: T, want: T) -> Result<(), String> {
    match got == want {
        true => Ok(()),
        false => Err(format!("step {step}: {what}: the database answered {got:?}, a fresh one {want:?}")),
    }
}

/// Every answer sampled, of `db` against a fresh database built from `world`: each file's text
/// and its end's line and column (through [`crate::line_index`]), each crate's content, data,
/// variables and gift, the crate list and each source root's crates.
fn compare(step: usize, db: &Db, keys: &[CrateKey], world: &WorldModel) -> Result<(), String> {
    let mut fresh = test_db();
    let fresh_keys = put_world(&mut fresh, world);
    for (at, file) in world.files.iter().enumerate() {
        let id = FileId::from_raw(at as u32);
        let (got, want) = (db.file_text(id).text(db), fresh.file_text(id).text(&fresh));
        agree(step, &format!("the text of file {at}"), got, want)?;
        agree(step, &format!("the model's text of file {at}"), got.as_str(), file.text.as_str())?;
        let end = TextSize::of(file.text.as_str());
        let what = format!("the end of file {at}");
        agree(step, &what, db.line_column(id, end), fresh.line_column(id, end))?;
    }
    for (at, (&key, &fresh_key)) in keys.iter().zip(&fresh_keys).enumerate() {
        agree(step, &format!("crate {at}"), crate_view(db, key), crate_view(&fresh, fresh_key))?;
    }
    agree(step, "the crate list", contents(db, &all_crates(db)), contents(&fresh, &all_crates(&fresh)))?;
    for root in 0..world.crates.len() as u32 {
        let (got, want) =
            (source_root_crates(db, SourceRootId(root)), source_root_crates(&fresh, SourceRootId(root)));
        agree(step, &format!("source root {root}'s crates"), contents(db, got), contents(&fresh, want))?;
    }
    Ok(())
}

/// Runs `ops` on `start` under `plant`: the first answer of the database that a fresh one of
/// the same world does not give, or a commit that wrote nothing that moves and dropped an
/// arena.
fn check(start: &WorldModel, ops: &[Op], plant: Plant) -> Result<(), String> {
    let mut world = start.clone();
    let mut db = test_db();
    // A stale remake keeps what a crate's first making saw, so it is planted before the world
    // is put; a commit that marks nothing is planted after, so it is an edit's that is caught.
    if plant == Plant::RemakeReusesStale {
        db.plant(plant);
    }
    let keys = put_world(&mut db, &world);
    db.plant(plant);
    compare(0, &db, &keys, &world)?;
    let mut pending: Vec<&Op, Global> = Vec::new_in(Global);
    let mut end = Vec::new_in(Global);
    end.extend(ops.iter());
    // The edit left after the last commit is committed too.
    let commit = Op::Commit;
    end.push(&commit);
    for (step, op) in end.into_iter().enumerate() {
        let Op::Commit = op else {
            pending.push(op);
            continue;
        };
        let (moved, committed) = db.edit_with(|edit| {
            let mut moved = false;
            for op in pending.drain(..) {
                moved |= apply(edit, &mut world, &keys, op);
            }
            moved
        });
        if !moved && !committed.dropped.is_empty() {
            return Err(format!(
                "step {step}: a commit of equal writes dropped arenas {:?}",
                committed.dropped
            ));
        }
        compare(step + 1, &db, &keys, &world)?;
    }
    Ok(())
}

/// The invariant: after any edits and commits, every sampled query answer equals that of a fresh Db built from the same world.
/// `BLAZE_SIM_SEEDS=<n>` runs it longer.
#[test]
fn the_database_answers_what_a_fresh_one_does() {
    blaze_sim::check(Config::cases(64), |tape| {
        let start = world(tape);
        let ops = tape.vec(0..=30, "ops", op);
        check(&start, &ops, Plant::None)
    });
}

/// A chain of two crates, a file each.
fn two_crates() -> WorldModel {
    let mut crates = Vec::new_in(Global);
    let mut files = Vec::new_in(Global);
    for root in 0..2 {
        files.push(FileModel { root, path: format!("/c{root}/lib.rs"), text: "pub fn f() {}\n".to_owned() });
        crates.push(CrateModel { root_file: root, env: 1, gift: None });
    }
    WorldModel { diamond: false, crates, files }
}

/// The oracle is not vacuous: a commit that marks no arena for a changed text is caught.
#[test]
fn the_model_catches_an_edit_that_marks_nothing() {
    let ops = [Op::Text { file: 0, text: "pub fn g() {}\n".to_owned() }, Op::Commit];
    check(&two_crates(), &ops, Plant::None).unwrap();
    let why = check(&two_crates(), &ops, Plant::EditMarksNothing).unwrap_err();
    assert!(why.contains("a fresh one"), "{why}");
}

/// The oracle is not vacuous: a crate arena made again with the variables of its first making
/// is caught.
#[test]
fn the_model_catches_a_remake_that_reuses_stale_variables() {
    let ops = [Op::Env { krate: 0, env: 2 }, Op::Commit];
    check(&two_crates(), &ops, Plant::None).unwrap();
    let why = check(&two_crates(), &ops, Plant::RemakeReusesStale).unwrap_err();
    assert!(why.contains("crate 0"), "{why}");
}

#[blaze_memo::tracked(returns(copy))]
fn text_len(db: &Db, text: crate::FileText) -> u32 {
    text.text(db).len() as u32
}

/// The same text written twice drops nothing, and nothing that read it runs again.
#[test]
fn the_same_text_twice_drops_nothing_and_reruns_nothing() {
    let world = two_crates();
    let mut db = test_db();
    let keys = put_world(&mut db, &world);
    for at in 0..world.files.len() as u32 {
        assert_eq!(text_len(&db, db.file_text(FileId::from_raw(at))), 14);
    }
    let committed = db.commit_with(|edit| {
        for (at, file) in world.files.iter().enumerate() {
            edit.set_file_text(FileId::from_raw(at as u32), &file.text);
        }
    });
    assert!(committed.dropped.is_empty(), "the same text dropped {:?}", committed.dropped);
    ran(&db, "text_len");
    for at in 0..world.files.len() as u32 {
        assert_eq!(text_len(&db, db.file_text(FileId::from_raw(at))), 14);
    }
    assert_eq!(ran(&db, "text_len"), 0, "the same text reran its reader");
    compare(0, &db, &keys, &world).unwrap();
}
