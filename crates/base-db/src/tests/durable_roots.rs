//! A library root is written at `HIGH`: its file set, its files' root map and texts, its
//! crates' data and the library part of the crate list. What a sealed crate's cover reads of
//! base-db is then a `HIGH` read (a covered frame allows it), a library write still reruns
//! its readers, and a workspace change leaves the durable view.

use blaze_query::db::{Database as _, Durability, View};
use cfg::{CfgAtom, CfgOptions};
use intern::Symbol;
use span::Edition;
use triomphe::Arc;

use crate::{
    AbsPathBuf, ContentId, Crate, CrateBuilderId, CrateGraphBuilder, CrateName, CrateOrigin,
    CrateWorkspaceData, Db, DependencyBuilder, Env, FileChange, FileId, FileSet, SourceRoot,
    SourceRootId, TargetKind, VfsPath,
};

const LIB: FileId = FileId::from_raw(0);
const MAIN: FileId = FileId::from_raw(1);
const OTHER: FileId = FileId::from_raw(2);

const LIB_TEXT: &str = "pub fn f() {}";
const LIB_EDITED: &str = "pub fn g() {}";

fn add(graph: &mut CrateGraphBuilder, root: FileId, origin: CrateOrigin) -> CrateBuilderId {
    graph.add_crate_root(
        root,
        Edition::Edition2021,
        None,
        None,
        std::iter::empty::<CfgAtom>().collect::<CfgOptions>(),
        None,
        Env::default(),
        origin,
        Vec::new(),
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

/// A library crate on `LIB` (a library root, 0) and a workspace crate on `MAIN` depending on
/// it, `OTHER` in the workspace root (1) too; with `other`, a second workspace crate on it.
fn graph(other: bool) -> CrateGraphBuilder {
    let mut graph = CrateGraphBuilder::default();
    let origin = CrateOrigin::Library { repo: None, name: Symbol::intern("lib") };
    let lib = add(&mut graph, LIB, origin);
    let main = add(&mut graph, MAIN, CrateOrigin::Local { repo: None, name: None });
    graph.add_dep(main, DependencyBuilder::new(CrateName::new("lib").unwrap(), lib)).unwrap();
    if other {
        _ = add(&mut graph, OTHER, CrateOrigin::Local { repo: None, name: None });
    }
    graph
}

fn workspace_root() -> SourceRoot {
    let mut set = FileSet::default();
    set.insert(MAIN, VfsPath::new_virtual_path("/ws/main.rs".to_owned()));
    set.insert(OTHER, VfsPath::new_virtual_path("/ws/other.rs".to_owned()));
    SourceRoot::new_local(set)
}

/// The database of [`graph`] through the writer a build's change goes through
/// ([`FileChange`]), the library file's text `lib`; and the library crate.
fn built(lib: &str) -> (Db, Crate) {
    let mut db = Db::for_tests();
    let mut library = FileSet::default();
    library.insert(LIB, VfsPath::new_virtual_path("/lib/lib.rs".to_owned()));
    let mut change = FileChange::default();
    change.set_roots(vec![SourceRoot::new_library(library), workspace_root()]);
    change.change_file(LIB, Some(lib.to_owned()));
    change.change_file(MAIN, Some("fn main() {}".to_owned()));
    change.change_file(OTHER, Some("pub fn o() {}".to_owned()));
    change.set_crate_graph(graph(false));
    let crates = change.apply(&mut db).expect("the change sets a crate graph");
    let krate = *crates.values().find(|it| it.data(&db).root_file_id == LIB).expect("the lib");
    (db, krate)
}

/// What a sealed crate's cover reads of base-db for `krate` (its source root's content, the
/// root's crates and their `OUT_DIR`s, its root file's root), under a covered frame: a read
/// below `HIGH` panics naming it.
fn covered_root(db: &Db, krate: Crate) -> ContentId {
    blaze_query::db::covered(db, "a library crate's root content", || {
        crate::crate_root_content(db, krate)
    })
}

fn durable_view(db: &Db) -> View {
    db.storage().durable_view()
}

/// An answer kept while the durable view it was read in is the current one, as a holder of a
/// durable row keeps it (`blaze_query::db::durable`).
struct Held {
    view: View,
    value: ContentId,
}

impl Held {
    /// The kept answer while `seen` is its view, else `krate`'s root content read again.
    fn answer(&self, db: &Db, seen: View, krate: Crate) -> ContentId {
        if seen == self.view { self.value } else { covered_root(db, krate) }
    }
}

/// The invariant: a library file's new text, written at `HIGH` through [`FileChange`], moves
/// the durable view, and what read it (the library crate's root content, read under a
/// covered frame, so every read of it is `HIGH`) answers what a fresh database made with that
/// text answers, read directly and through a holder of durable answers.
/// Control: the holder shown the durable view of before the write, as a write that skipped
/// the durable bump would leave it, keeps the old answer, which the oracle refuses.
#[test]
fn a_library_text_written_at_high_reruns_its_reader() {
    let (mut db, lib) = built(LIB_TEXT);
    let before = covered_root(&db, lib);
    let held = Held { view: durable_view(&db), value: before };

    let mut change = FileChange::default();
    change.change_file(LIB, Some(LIB_EDITED.to_owned()));
    _ = change.apply(&mut db);

    let (fresh_db, fresh_lib) = built(LIB_EDITED);
    let fresh = covered_root(&fresh_db, fresh_lib);
    assert_ne!(fresh, before, "the edit changes the library root's content");
    assert_ne!(durable_view(&db), held.view, "a library text written at `HIGH` opens a view");
    assert_eq!(covered_root(&db, lib), fresh);
    assert_eq!(held.answer(&db, durable_view(&db), lib), fresh);

    // Control: the bump skipped, the holder answers what it held.
    assert_ne!(held.answer(&db, held.view, lib), fresh, "the oracle catches a skipped bump");
}

/// The invariant: a workspace change (a local file's text, a local crate added to the graph)
/// leaves the durable view where it was, and the library crate's root content still reads
/// `HIGH` inputs alone (under a covered frame).
/// Control: the local root written at `HIGH`, as a writer that took every root for a
/// library's would, moves it.
#[test]
fn a_workspace_change_leaves_the_durable_view() {
    let (mut db, lib) = built(LIB_TEXT);
    let root = covered_root(&db, lib);
    let view = durable_view(&db);

    let mut change = FileChange::default();
    change.change_file(MAIN, Some("fn main() { let _ = 1; }".to_owned()));
    _ = change.apply(&mut db);
    assert_eq!(durable_view(&db), view, "a local file's text is no `HIGH` write");

    let mut change = FileChange::default();
    change.set_crate_graph(graph(true));
    _ = change.apply(&mut db);
    assert_eq!(crate::all_crates(&db).len(), 3);
    assert_eq!(durable_view(&db), view, "a local crate added is no `HIGH` write");
    assert_eq!(covered_root(&db, lib), root);

    // Control: the workspace's root set at `HIGH`.
    db.set_source_root_with_durability(SourceRootId(1), Arc::new(workspace_root()), Durability::HIGH);
    let mut change = FileChange::default();
    change.change_file(MAIN, Some("fn main() {}".to_owned()));
    _ = change.apply(&mut db);
    assert_ne!(durable_view(&db), view, "a local root at `HIGH` moves the durable view");
}
