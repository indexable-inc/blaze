//! One id space per database: a commit that starts a new session (here the one after a commit
//! that failed, `world::Fresh::AfterFailedCommit`) makes every arena again in the database's
//! id space, so no arena id, and no crate id, of the new session is one an earlier snapshot
//! handed out. A write of a sealed crate starts none: it drops that crate's arenas and its
//! dependents', whose new makings take ids of their own too.

use std::{alloc::Global, collections::HashSet};

use blaze_memo::{ArenaId, Database as _, Gid, Id as _};
use triomphe::Arc;

use super::{EditDb, add, depend, set_graph, test_db, test_fact};
use crate::{
    ContentId, CrateGraphBuilder, CrateKey, Db, Env, FileId, FileSet, Plant, SourceRoot,
    SourceRootId, VfsPath,
};

/// A workspace crate `top` (source root 0, local) over a sealed library crate `lib` (source
/// root 1, a library's): `lib`'s file, `top`'s file, and the two crates' keys.
fn over_a_sealed_library(db: &mut Db) -> (FileId, FileId, [CrateKey; 2]) {
    let (lib_file, top_file) = (FileId::from_raw(0), FileId::from_raw(1));
    db.commit_with(|edit| {
        edit.set_file_text(lib_file, "pub fn callee() -> u32 { 1 }").set_file_source_root(lib_file, SourceRootId(1));
        edit.set_file_text(top_file, "pub fn caller() {}").set_file_source_root(top_file, SourceRootId(0));
        let mut lib_set = FileSet::default();
        lib_set.insert(lib_file, VfsPath::new_virtual_path("/lib/lib.rs".to_owned()));
        let mut top_set = FileSet::default();
        top_set.insert(top_file, VfsPath::new_virtual_path("/top/lib.rs".to_owned()));
        edit.set_source_root(SourceRootId(1), Arc::new(SourceRoot::new_library(lib_set)));
        edit.set_source_root(SourceRootId(0), Arc::new(SourceRoot::new_local(top_set)));
    });
    let mut graph = CrateGraphBuilder::default();
    let lib = add(&mut graph, lib_file, Default::default(), Env::default());
    graph.seal(lib, ContentId::of("base-db id space test pin", b"lib"));
    let top = add(&mut graph, top_file, Default::default(), Env::default());
    depend(&mut graph, top, lib, "lib");
    let crates = set_graph(db, graph);
    (lib_file, top_file, [crates[&lib], crates[&top]])
}

/// Every arena id and crate id any snapshot of the database handed out so far.
#[derive(Default)]
struct Seen {
    arenas: HashSet<ArenaId>,
    crates: HashSet<Gid>,
}

impl Seen {
    /// The ids of `db`'s snapshot: its arenas and the crates of `keys`.
    fn of(db: &Db, keys: &[CrateKey]) -> (Vec<ArenaId, Global>, Vec<Gid, Global>) {
        let mut arenas = Vec::new_in(Global);
        arenas.extend(db.memo().arenas());
        let mut crates = Vec::new_in(Global);
        crates.extend(keys.iter().map(|key| db.krate(*key).gid()));
        (arenas, crates)
    }

    /// Notes the ids of `db`'s snapshot, carried ones among them.
    fn note(&mut self, db: &Db, keys: &[CrateKey]) {
        let (arenas, crates) = Seen::of(db, keys);
        self.arenas.extend(arenas);
        self.crates.extend(crates);
    }

    /// The oracle, after a commit that made every arena again: no id of `db`'s snapshot is one
    /// an earlier snapshot had. Then notes them.
    fn none_again(&mut self, db: &Db, keys: &[CrateKey]) -> Result<(), String> {
        let (arenas, crates) = Seen::of(db, keys);
        if let Some(again) = arenas.iter().find(|it| self.arenas.contains(it)) {
            return Err(format!("{again:?} was handed out again after a new session"));
        }
        if let Some(again) = crates.iter().find(|it| self.crates.contains(it)) {
            return Err(format!("the crate id {again:?} was handed out again after a new session"));
        }
        self.arenas.extend(arenas);
        self.crates.extend(crates);
        Ok(())
    }
}

/// A commit of a write of `top_file` whose maker of `top`'s arena panics while the commit makes
/// it: caught, so the edit is dropped and the old snapshot has a successor it never got.
fn fail_a_commit(db: &mut Db, top: CrateKey, top_file: FileId, round: u32) {
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut edit = db.edit();
        edit.set_file_text(top_file, &format!("pub fn failed{round}() {{}}"));
        let panics: crate::MakeGiven = Box::new_in(
            |_: &blaze_memo::Builder, _: &blaze_memo::Inputs<'_>| panic!("a maker failed mid-commit"),
            Global,
        );
        edit.set_given(Some(top), test_fact("id space panics"), panics);
        _ = edit.commit();
    }));
    assert!(unwound.is_err(), "the maker's panic reached the caller");
}

/// Three rounds of: a write of the workspace crate's file (a carried session), a write of the
/// sealed library's file (its root's arena, `lib`'s and `top`'s made again, the session
/// carried), a commit that fails, then a commit (a new session): how many distinct arena ids
/// were handed out and how many makings there were (equal when no making took an earlier
/// id), or the first id handed out again after a new session.
fn ids_across_sessions(plant: Plant) -> Result<(usize, usize), String> {
    let mut db = test_db();
    db.plant(plant);
    let (lib_file, top_file, keys) = over_a_sealed_library(&mut db);
    let nonce = db.nonce();
    let mut seen = Seen::default();
    seen.note(&db, &keys);
    let mut makings = seen.arenas.len();
    for round in 0..3_u32 {
        let workspace = db.commit_with(|edit| {
            edit.set_file_text(top_file, &format!("pub fn caller{round}() {{}}"));
        });
        assert!(!workspace.dropped.is_empty() && !workspace.dropped.contains(&0), "a workspace file's write remakes its crate and carries the session");
        makings += workspace.dropped.len();
        seen.note(&db, &keys);
        let sealed = db.commit_with(|edit| {
            edit.set_file_text(lib_file, &format!("pub fn callee() -> u32 {{ {round} }}"));
        });
        // The library's source root's arena, then `lib`'s and `top`'s: never the session's, the
        // crate list's (1) nor the workspace root's.
        assert_eq!(&sealed.dropped[..], &[3, 4, 5], "a sealed crate's write drops it and its dependents alone");
        makings += sealed.dropped.len();
        seen.note(&db, &keys);
        fail_a_commit(&mut db, keys[1], top_file, round);
        let fresh = db.commit_with(|edit| {
            edit.set_file_text(top_file, &format!("pub fn after{round}() {{}}"));
        });
        // The session, the crate list's, the two source roots' arenas, then `lib`'s and `top`'s.
        assert_eq!(&fresh.dropped[..], &[0, 1, 2, 3, 4, 5], "the commit after a failed one makes every arena again");
        makings += fresh.dropped.len();
        seen.none_again(&db, &keys)?;
        assert_eq!(db.nonce(), nonce, "a new session goes on in the database's id space and nonce");
    }
    Ok((seen.arenas.len(), makings))
}

/// The invariant: within one database (one nonce) an `ArenaId`, and so a `Crate` id, names
/// one arena for ever: a commit after a failed one starts a new session (every arena made
/// again from the same world in the same order) whose every arena id and crate id differs
/// from every one any earlier snapshot had, and the arenas a sealed crate's write makes again
/// take ids of their own, so a cache kept outside the memo under them (the kept check in
/// crates/hir, the solver's shared caches in hir-ty) finds no entry of another arena's memory.
/// The control (the cause put back): with `Plant::FreshRestartsIds` the new session starts its
/// id space again, and the oracle names an id handed out again.
#[test]
fn a_new_session_hands_out_no_id_an_earlier_snapshot_had() {
    let (handed, makings) = ids_across_sessions(Plant::None).unwrap_or_else(|why| panic!("{why}"));
    assert_eq!(handed, makings, "each making of an arena took an id of its own");
    assert!(makings >= 5 + 3 * (2 + 3 + 5), "{makings} makings");
    let planted = ids_across_sessions(Plant::FreshRestartsIds).expect_err("a restarted id space was not caught");
    assert!(planted.contains("was handed out again"), "{planted}");
}
