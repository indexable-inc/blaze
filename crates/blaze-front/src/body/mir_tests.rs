//! A body record's generic MIR served in another database ([`super::mir`]): a generic function
//! of `lib` with a closure, its record made by the body task for its MIR alone
//! (`blaze_keyed::kind::Steps::Mir`), then served in a snapshot where `lib` is given by its
//! surface's address and its text is another (so any read of it is refused, `hir_def`'s
//! keyed-file refusal), through the plan task's path (`GivenBody` to [`super::mir::given`]).
//! Each test has its planted control.

use std::{
    alloc::Global,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::Arc,
};

use base_db::{Crate, Db, all_crates};
use blaze_format::Aligned;
use blaze_ids::ContentId;
use blaze_item_hash::{Bodies, values};
use blaze_keyed::{
    driver::TaskFailed,
    front::{GivenBody, Table, Universe},
    kind::{Body, Key, Steps, Surface},
    record::{self, Owner, StableId},
};
use hir_def::{
    DefWithBodyId, ExpressionStoreOwnerId, deps::ImageBuf, hir::ClosureKind,
    nameres::demand::crate_body_owners, signatures::FunctionSignature,
};
use hir_expand::{fixture, name::Name};
use blaze_items::key;
use hir_ty::{
    InferBodyId,
    body_store::{
        Installed, Step, Traced, Unserved, entries, install_given, install_trace,
        testing::{Plant, planted, portable},
    },
    db::{InternedClosure, InternedClosureId, closure_exprs},
    mir::{MirOwner, MirStage},
    providers::mir_at,
};

use super::{
    body_with,
    front::Front,
    mir::{UNSORTED, failed, given},
};
use crate::surface::{Opened, ReadAt, TextIds, surface};

const BUILD: ContentId = ContentId([9; 32]);

/// `lib`'s `swap` is generic, names a struct (`Pair`, a type the MIR's items list) and has a
/// closure (a MIR owner of its own, keyed relative to the body).
const FIXTURE: &str = r#"
//- minicore: sized, copy, fn
//- /lib.rs crate:lib deps:core new_source_root:library
pub struct Pair<T> { pub a: T, pub b: T }
pub fn swap<T>(p: Pair<T>) -> Pair<T> {
    let flip = |q: Pair<T>| Pair { a: q.b, b: q.a };
    let _ = &flip;
    Pair { a: p.b, b: p.a }
}
//- /main.rs crate:main deps:core,lib new_source_root:local
pub fn main() {}
"#;

/// `swap`'s body as the serving database has it: other text, so a read of it is refused.
fn edited() -> String {
    let edited = FIXTURE.replace("Pair { a: p.b, b: p.a }", "Pair { a: p.a, b: p.b }");
    assert_ne!(edited, FIXTURE, "premise: the edit applies");
    edited
}

/// A crate of nothing `lib` names, made first, so the second writer's ids are not the first's.
const PAD: &str = "//- /pad.rs crate:pad deps:core new_source_root:library
pub fn p1() {}
pub struct P;
";

fn padded() -> String {
    let at = FIXTURE.find("//- /").expect("a fixture with a file");
    format!("{}{PAD}{}", &FIXTURE[..at], &FIXTURE[at..])
}

/// A database of `text` that knows which front end computes its bodies' identities, as a
/// snapshot does (blaze-command `snapshot.rs`, `give_build` before its first commit): without
/// it every early identity is `Err("no front-end build is set")` and the item store refuses the
/// body as having no identity. The build is the one the item store is installed with. Its fact
/// boundaries are the command's (`blaze_item_hash::reads::mark_boundaries`): with none marked,
/// every query lists its file's whole text as a read, which no item's fact can stand for, and
/// the body task is refused for it (`base_db::handles::VfsText`).
fn db_of(text: &str) -> Db {
    let mut db =
        fixture::db_with_boundaries(text, blaze_item_hash::reads::mark_boundaries, blaze_memo::Reads::Kept).into_db();
    let mut edit = db.edit();
    blaze_item_hash::identity::give_build(&mut edit, BUILD);
    _ = edit.commit();
    db
}

fn crate_named(db: &Db, name: &str) -> Crate {
    *all_crates(db)
        .iter()
        .find(|krate| krate.extra_data(db).display_name.as_ref().map(|it| it.canonical_name().as_str()) == Some(name))
        .unwrap_or_else(|| panic!("a crate named {name}"))
}

fn swap(db: &Db) -> DefWithBodyId {
    crate_body_owners(db, crate_named(db, "lib"))
        .into_iter()
        .find(|&def| match def {
            DefWithBodyId::FunctionId(it) => FunctionSignature::of(db, it).name == Name::new_root("swap"),
            _ => false,
        })
        .expect("`swap` in lib")
}

/// The driver's id of a body: its fact subject, the item key, equal in both databases.
fn item_of(db: &Db, def: DefWithBodyId) -> StableId {
    StableId(values::body_key(db, def))
}

struct Texts<'a> {
    db: &'a Db,
}

impl TextIds for Texts<'_> {
    fn text_id(&self, def: DefWithBodyId) -> Option<StableId> {
        match def {
            DefWithBodyId::VariantId(_) => None,
            _ => Some(StableId(ContentId::of("blaze-front mir test text id", &values::body_key(self.db, def).0))),
        }
    }
}

fn owner_of(db: &Db, krate: Crate) -> Owner {
    Owner::of_subject(&key::crate_key(db, krate))
}

/// The surface record of `krate` in `db` over `deps`' tables, and its address.
fn surface_of(db: &Db, krate: Crate, name: &[u8], deps: &[Table<'_>]) -> (Aligned, Key<Surface>) {
    let at = Key::<Surface>::from_id(ContentId::of("blaze-front mir test surface key", name));
    let universe = Universe { asker: 0, tables: deps, outside: &[] };
    let bytes = surface(db, krate, at, universe, &Texts { db }).expect("a surface record");
    let (at, _) = record::check_read::<Surface>(&bytes, at).expect("a read-addressed surface");
    (bytes, at)
}

/// What one writer made of `lib`: `core`'s and its surfaces, and `swap`'s body record (MIR
/// alone) with its result digest; and `swap`'s MIR and its closure's, each as [`portable`] bytes.
struct Written {
    core: Aligned,
    lib: Aligned,
    body: Aligned,
    key: Key<Body>,
    digest: ContentId,
    item: StableId,
    mir: Vec<u8>,
    closure: Vec<u8>,
}

/// The Checked key a unit's key task would give `item` here (the test's stand-in), from which
/// the driver makes the MIR-alone key (`Steps::key`).
fn checked_key(item: StableId) -> Key<Body> {
    Key::<Body>::from_id(ContentId::of("blaze-front mir test body key", &item.0.0))
}

/// The one closure of `swap` in `db`, by its expression (`closure_exprs`: the body's store
/// where it is lowered, its record's list where it is given).
fn closure_of(db: &Db, def: DefWithBodyId) -> MirOwner<'_> {
    let exprs = closure_exprs(db, ExpressionStoreOwnerId::Body(def));
    assert_eq!(exprs.len(), 1, "`swap` has one closure");
    let loc = InternedClosure { owner: InferBodyId::DefWithBodyId(def), expr: exprs[0], kind: ClosureKind::Closure };
    MirOwner::Closure(InternedClosureId::new(db, loc))
}

fn built<'db>(db: &'db Db, owner: MirOwner<'db>) -> &'db hir_ty::mir::MirBody<'db> {
    mir_at(db, owner, MirStage::Built).unwrap_or_else(|error| panic!("{owner:?} builds: {error:?}"))
}

/// Gives each crate of `records` (its name, its surface record) to `db` by the record's
/// address, as a body task's and a plan's snapshot gives every crate it reads, the body's own
/// included (blaze-command `snapshot.rs`, edit 2): its def map, signatures and impl tables then
/// read the record's pages, as they do in the command, and not the crate's text. A fixture
/// crate is one file, its root, which keeps its text, so a body of it lowers from that text
/// where the text is the one the pages were made from and is refused where it is not.
fn give_from_surfaces(db: &mut Db, records: &[(&str, &Aligned)]) {
    let mut surfaces = Vec::new_in(Global);
    for &(name, bytes) in records {
        let krate = crate_named(db, name);
        let table = Table::of(0, owner_of(db, krate), bytes, &[]).expect("a crate's table");
        let read = ReadAt::of(&table).unwrap_or_else(|error| panic!("{name}'s proof: {error:?}"));
        // dyn: a record held as the store holds it.
        let holder: ImageBuf = Arc::new(Aligned::copy_from(bytes));
        surfaces.push((holder, read));
    }
    let opened = Opened::open_read(&surfaces).expect("the surfaces open");
    // The crate ids are read before the gift and used only inside its edit: its commit makes
    // each given crate's arena again.
    let mut crates = Vec::new_in(Global);
    crates.extend(all_crates(db).iter().map(|&krate| (key::crate_key(db, krate), krate)));
    let mut edit = db.edit();
    opened
        .give(&mut edit, |crate_key| crates.iter().find(|(it, _)| it == crate_key).map(|&(_, krate)| krate))
        .expect("each crate given by its address");
    _ = edit.commit();
}

/// Each whole-table read of `def`'s inference and MIR steps, by step, the query that made it
/// and the fact's kind and subject: what a refused `TaskFailed::RefusedFact` cannot name, as it
/// has no field for the query.
fn scans_of(db: &Db, traces: &Installed, def: DefWithBodyId) -> Vec<(Step, &'static str, u16, ContentId)> {
    let mut steps = vec![(Step::Infer, entries::run_infer(db, def))];
    for place in entries::places(db, def) {
        if let Ok(schema) = entries::run_mir(db, def, place) {
            steps.push((Step::Mir, schema));
        }
    }
    let mut out = Vec::new();
    for (step, schema) in steps {
        if let Ok(traced) = traces.trace(def, step, schema)
            && let Traced::Kept { scans, .. } = &*traced
        {
            out.extend(scans.iter().map(|&(fact, query)| (step, query, fact.kind.0, fact.subject)));
        }
    }
    out
}

/// `text`'s `lib` and `swap`'s record, written by the body task of [`Steps::Mir`]. The surfaces
/// are made in one database of `text` (the surface tasks'), and the body task runs in another
/// where every crate is given from them (the body task's snapshot): a database whose crates are
/// read from their text answers some reads beneath a fact boundary from a crate's whole tables
/// (`declares_sealed` reads `TraitImpls::for_crate` of a crate not served from pages), which the
/// body's record would list as a read of a whole crate and the task refuses (`facts::WHOLE_CRATE`).
fn write(text: &str) -> Written {
    let source = db_of(text);
    let (core, lib) = (crate_named(&source, "core"), crate_named(&source, "lib"));
    let (core_bytes, _) = surface_of(&source, core, b"core", &[]);
    let core_table = Table::of(1, owner_of(&source, core), &core_bytes, &[]).expect("core's table");
    let (lib_bytes, _) = surface_of(&source, lib, b"lib", &[core_table]);
    let lib_table = Table::of(2, owner_of(&source, lib), &lib_bytes, &[]).expect("lib's table");
    let mut tables = [core_table, lib_table];
    tables.sort_unstable_by_key(|table| table.owner);

    let mut db = db_of(text);
    give_from_surfaces(&mut db, &[("core", &core_bytes), ("lib", &lib_bytes)]);
    let def = swap(&db);
    let item = item_of(&db, def);
    // Declared after the database: uninstalled before it drops.
    let traces: Installed = install_trace(&db, Arc::new(Bodies), BUILD).expect("a trace store");
    let find = move |id: StableId| (id == item).then_some(def);
    // What a refused step tells (its rows' readers), printed with the failure below.
    let lines = std::sync::Mutex::new(Vec::<String>::new());
    let told = |line: &str| lines.lock().expect("the told lines").push(line.to_owned());
    let front = Front { db: &db, traces: &traces, find: &find, told: &told };
    let universe = Universe { asker: 2, tables: &tables, outside: &[] };
    let key = Steps::Mir.key(checked_key(item), item);
    let body = body_with(&front, item, key, universe, Steps::Mir).unwrap_or_else(|failed| {
        // A refused fact about a crate names it by the crate's key: say which crate it is.
        let named = match failed {
            TaskFailed::RefusedFact { subject, .. } => ["core", "lib"]
                .into_iter()
                .find(|&name| Owner::of_subject(&key::crate_key(&db, crate_named(&db, name))) == Owner::of_subject(&subject)),
            _ => None,
        };
        panic!(
            "swap's body task: {failed:?}, of crate {named:?}, whole-table reads by query: {:?}; told: {:#?}",
            scans_of(&db, &traces, def),
            lines.lock().expect("the told lines"),
        )
    });
    let (_, root) = record::check_read::<Body>(&body, key).expect("a read-addressed body record");
    let digest = record::result_digest(root, &body).expect("the record's result digest");
    let mir = portable(&db, &Bodies, def, built(&db, MirOwner::Body(InferBodyId::DefWithBodyId(def)))).expect("swap's MIR");
    let closure = portable(&db, &Bodies, def, built(&db, closure_of(&db, def))).expect("the closure's MIR");
    Written { core: core_bytes, lib: lib_bytes, body, key, digest, item, mir, closure }
}

/// A database of [`edited`] text with every crate given by its surface's address (a plan's
/// snapshot) and `bodies` as its body records, and its item store in `Mode::Given`.
struct Serving {
    db: Db,
    store: Option<Installed>,
}

impl Drop for Serving {
    fn drop(&mut self) {
        // Uninstalled before the database drops.
        self.store.take();
    }
}

fn serve(written: &Written, bodies: &[GivenBody<'_>]) -> Serving {
    let mut db = db_of(&edited());
    give_from_surfaces(&mut db, &[("core", &written.core), ("lib", &written.lib)]);
    let records = given(bodies, Global).expect("sorted records");
    let store = install_given(&db, Arc::new(Bodies), BUILD, records).expect("a given store");
    Serving { db, store: Some(store) }
}

/// `written`'s record, round-tripped through bytes: copied out and into a fresh aligned buffer,
/// checked under its key again, as the host reads it from the store before a plan task.
fn read_back(written: &Written) -> Aligned {
    let mut bytes = Vec::new_in(Global);
    bytes.extend_from_slice(&written.body);
    let copy = Aligned::copy_from(&bytes);
    record::check_read::<Body>(&copy, written.key).expect("the copy reads under its key");
    copy
}

/// The plan task's given body of `written`, its record `record`.
fn body_of<'a>(written: &Written, record: &'a Aligned) -> GivenBody<'a> {
    GivenBody { item: written.item, record, result: written.digest }
}

/// The refusal `ask` unwound with, which must be an [`Unserved`].
fn unserved(ask: impl FnOnce()) -> Unserved {
    let Err(payload) = catch_unwind(AssertUnwindSafe(ask)) else { panic!("served, where a refusal was planted") };
    match Unserved::of_payload(&*payload) {
        Some(it) => it.clone(),
        None => panic!("the refusal unwound with another payload than `Unserved`"),
    }
}

/// The invariant: `swap`'s MIR and its closure's, served in a snapshot without `lib`'s text
/// (round-tripped through bytes), equal the MIR lowered from source, compared by their portable
/// bytes (items by key, types by content, no database id). Controls: the serving snapshot
/// refuses a read of `swap`'s text (so nothing above read it), and with no record the query
/// refuses by name, as it does a borrow check of a record made unchecked (rule 74).
#[test]
fn served_mir_equals_lowered_mir() {
    let written = write(FIXTURE);
    let record = read_back(&written);
    let serving = serve(&written, &[body_of(&written, &record)]);
    let db = &serving.db;
    let def = swap(db);
    let mir = portable(db, &Bodies, def, built(db, MirOwner::Body(InferBodyId::DefWithBodyId(def)))).expect("served MIR");
    assert_eq!(mir, written.mir, "swap's MIR served and lowered");
    let closure = portable(db, &Bodies, def, built(db, closure_of(db, def))).expect("served closure MIR");
    assert_eq!(closure, written.closure, "the closure's MIR served and lowered");

    // Control: the snapshot holds no text of `swap` to read.
    let refused = catch_unwind(AssertUnwindSafe(|| {
        _ = hir_def::expr_store::Body::of(db, def);
    }));
    let payload = refused.expect_err("swap's body lowered from text in the serving snapshot");
    let stop = hir_def::nameres::frozen::Unreadable::of_payload(&*payload).expect("a keyed-file refusal");
    assert_eq!(stop.kind, hir_def::nameres::frozen::pages::KEYED_FILE);

    // A borrow check of a record made for its MIR alone: unchecked, never a pass.
    let checked = unserved(|| {
        _ = hir_ty::body_store::lookup_passed(db, def, "any checker", || true);
    });
    assert_eq!((checked.def, checked.kind), (def, Unserved::UNCHECKED));

    // Control: no record given. The refusal is the driver's `NoBody` of the item it names, so
    // the plan's driver finds the body by it (`record::mir_id` of the same subject).
    let bare = serve(&written, &[]);
    let none = unserved(|| {
        let def = swap(&bare.db);
        _ = mir_at(&bare.db, MirOwner::Body(InferBodyId::DefWithBodyId(def)), MirStage::Built);
    });
    assert_eq!((none.kind, none.item), (Unserved::NO_RECORD, Some(written.item.0)));
    assert_eq!(failed(&none), TaskFailed::NoBody { item: written.item });

    // Control: bodies out of order are refused, never searched as if sorted.
    let twice = [body_of(&written, &record), body_of(&written, &record)];
    assert_eq!(given(&twice, Global).err(), Some(UNSORTED));
}

/// The invariant: two builds write equal bytes (rule 25), here a writer whose ids differ (a
/// crate made before `lib`). And the record is filed under the MIR-alone key the driver makes
/// (`Steps::key`), apart from the body's Checked key (rule 21), and only there. Control: the body
/// record of another text differs.
#[test]
fn two_builds_write_equal_bytes() {
    let (one, two) = (write(FIXTURE), write(&padded()));
    assert_eq!(&one.body[..], &two.body[..], "swap's body record of two writers");
    let checked = checked_key(one.item);
    assert_eq!(Steps::Checked.key(checked, one.item), checked);
    assert_ne!(one.key, checked, "a MIR-alone record under its body's Checked key");
    assert!(record::check_read::<Body>(&one.body, one.key).is_ok());
    assert!(record::check_read::<Body>(&one.body, checked).is_err(), "the record reads under the Checked key");
    let other = write(&edited());
    assert_ne!(&one.body[..], &other.body[..], "another body wrote the same record");
}

/// A body of each macro kind: a call of a dependency's `macro_rules!` (given by its surface's
/// address), of a builtin (`concat!`), and of a `macro_rules!` of its own crate; and `other`, a
/// body of the same file before them whose edit moves every later body's offsets in the file.
const MACROS: &str = r#"
//- minicore: sized, concat
//- /dep.rs crate:dep deps:core new_source_root:library
#[macro_export]
macro_rules! twice { ($e:expr) => { ($e, $e) }; }
//- /app.rs crate:app deps:core,dep new_source_root:local
macro_rules! own { ($e:expr) => { $e }; }
pub fn other() -> u32 { 7 }
pub fn by_dep() -> (u32, u32) { dep::twice!(1) }
pub fn by_builtin() -> &'static str { core::concat!("a", "b") }
pub fn by_own() -> u32 { own!(2) }
"#;

/// The bodies of [`MACROS`] that call a macro, each with the edit of its call's arguments.
const CALLERS: [(&str, &str, &str); 3] = [
    ("by_dep", "dep::twice!(1)", "dep::twice!(2)"),
    ("by_builtin", "core::concat!(\"a\", \"b\")", "core::concat!(\"a\", \"c\")"),
    ("by_own", "own!(2)", "own!(3)"),
];

/// `text` with `from` replaced by `to`, which must be in it once.
fn with(text: &str, from: &str, to: &str) -> String {
    assert_eq!(text.matches(from).count(), 1, "premise: `{from}` is in the fixture once");
    text.replace(from, to)
}

/// The body of crate `krate` named `name`.
fn body_named(db: &Db, krate: &str, name: &str) -> DefWithBodyId {
    crate_body_owners(db, crate_named(db, krate))
        .into_iter()
        .find(|&def| match def {
            DefWithBodyId::FunctionId(it) => FunctionSignature::of(db, it).name == Name::new_root(name),
            _ => false,
        })
        .unwrap_or_else(|| panic!("`{name}` in {krate}"))
}

/// `app`'s body `name` of `text` made by the body task (its MIR alone) in a snapshot where
/// `core`, `dep` and `app` are each given by its surface's address, as the command's body
/// snapshot gives them; with the lines the task told.
fn made(text: &str, name: &str) -> (Result<Aligned, TaskFailed>, Vec<String>) {
    let (out, told) = made_in(text, &["core", "dep", "app"], name);
    (out.map(|(_, record)| record), told)
}

/// What [`made`] makes, of the crates `crates` of `text` (each over the ones before it, the
/// body's crate last): `name`'s key and record, or why the task failed; with the lines it told.
/// The key is made as the unit's key task makes it, from the body's own text (the anchor's,
/// `BodySyntax::text`), not from its place in the file. Asserts too that the steps' read lists
/// (each step's work run again under `blaze_memo::record`) hold no row of the body's file where
/// the task made the record: what lets an edit elsewhere in the file find it.
fn made_in(
    text: &str,
    crates: &[&str],
    name: &str,
) -> (Result<(Key<Body>, Aligned), TaskFailed>, Vec<String>) {
    /// The tables of the surfaces `made` holds, of the first crates of `crates` in order,
    /// numbered from 1, sorted by owner as a universe holds them.
    fn tables_of<'a>(db: &Db, crates: &[&str], made: &'a [Aligned]) -> Vec<Table<'a>> {
        let mut tables: Vec<Table<'a>> = (made.iter().zip(crates).enumerate())
            .map(|(at, (bytes, krate))| {
                let number = u32::try_from(at + 1).expect("a few crates");
                Table::of(number, owner_of(db, crate_named(db, krate)), bytes, &[]).expect("a crate's table")
            })
            .collect();
        tables.sort_unstable_by_key(|table| table.owner);
        tables
    }
    let source = db_of(text);
    // Each surface reads the surfaces made before it.
    let mut bytes: Vec<Aligned> = Vec::new();
    for &krate in crates {
        let made = surface_of(&source, crate_named(&source, krate), krate.as_bytes(), &tables_of(&source, crates, &bytes)).0;
        bytes.push(made);
    }
    let tables = tables_of(&source, crates, &bytes);
    let surfaces: Vec<(&str, &Aligned)> = crates.iter().copied().zip(&bytes).collect();

    let mut db = db_of(text);
    give_from_surfaces(&mut db, &surfaces);
    let home = *crates.last().expect("the body's crate");
    let def = body_named(&db, home, name);
    let item = item_of(&db, def);
    let own = hir_def::expr_store::body::body_syntax(&db, def).as_ref().expect("a body written in a file").text();
    let key = Steps::Mir.key(Key::<Body>::from_id(ContentId::of("blaze-front mir test own text key", own.as_bytes())), item);
    // Declared after the database: uninstalled before it drops.
    let traces: Installed = install_trace(&db, Arc::new(Bodies), BUILD).expect("a trace store");
    let find = move |id: StableId| (id == item).then_some(def);
    let lines = std::sync::Mutex::new(Vec::new());
    let told = |line: &str| lines.lock().expect("the told lines").push(line.to_owned());
    let front = Front { db: &db, traces: &traces, find: &find, told: &told };
    // The asker is the body's crate, the last unit numbered above.
    let asker = u32::try_from(crates.len()).expect("a few crates");
    let universe = Universe { asker, tables: &tables, outside: &[] };
    let out = body_with(&front, item, key, universe, Steps::Mir);
    drop(front);
    if out.is_ok() {
        let memo = blaze_memo::Database::memo(&db);
        let ((), rows) = blaze_memo::record(memo, || {
            entries::rerun_unstored(&db, def, Step::Infer, entries::Place::BODY);
            for place in entries::places(&db, def) {
                entries::rerun_unstored(&db, def, Step::Mir, place);
            }
        })
        .expect("the test database keeps read lists");
        let file = format!("{home}.rs");
        let of_file: Vec<String> = (rows.iter())
            .filter_map(|row| blaze_item_hash::reads::file_of_row(&db, *row).map(|it| (row, it)))
            .filter(|(_, it)| it.contains(&file))
            .map(|(row, it)| format!("{} of `{it}`", row.kind().name()))
            .collect();
        assert!(of_file.is_empty(), "`{name}` was made with rows of its own file in its steps' lists: {of_file:#?}");
    }
    (out.map(|record| (key, record)), lines.into_inner().expect("the told lines"))
}

/// The record of `name` of `text`, which must be made; a refusal names its told readers.
fn made_ok(text: &str, name: &str) -> Aligned {
    match made(text, name) {
        (Ok(record), _) => record,
        (Err(failed), told) => panic!("`{name}`'s body task: {failed:?}; told: {told:#?}"),
    }
}

/// The invariant: a body that calls a macro (a dependency's `macro_rules!`, a builtin, its own
/// crate's) is made with every row of its read list a fact or covered by its key; its record
/// moves when the call's arguments change (the body's own text) and when the macro's
/// definition changes, and not when another body of the same file changes. Controls: each
/// edit is in the fixture once (`with`), and each record is compared with its own edit's.
#[test]
fn a_body_with_a_macro_call_is_made_and_moves_with_the_call_alone() {
    for (name, call, edited_call) in CALLERS {
        let record = made_ok(MACROS, name);
        let argued = made_ok(&with(MACROS, call, edited_call), name);
        assert_ne!(&record[..], &argued[..], "`{name}`: another call's arguments left its record");
        let other = made_ok(&with(MACROS, "{ 7 }", "{ let seven = 7; seven }"), name);
        assert_eq!(&record[..], &other[..], "`{name}`: another body of its file moved its record");
    }
    let defined = [
        ("by_dep", "{ ($e, $e) }", "{ ($e, 0) }"),
        ("by_own", "{ $e }", "{ { $e } }"),
    ];
    for (name, definition, edited) in defined {
        let record = made_ok(MACROS, name);
        let moved = made_ok(&with(MACROS, definition, edited), name);
        assert_ne!(&record[..], &moved[..], "`{name}`: its macro's definition changed and its record did not");
    }
}

/// The cause, removed and put back (rule 23): with the argument of a call written in a file read
/// as before the `call_input` boundary (`hir_expand::planted_file_call_args`, through
/// `macro_arg`, which reads the file's tree beneath no boundary), the body with a macro call is
/// refused again by `base_db::handles::VfsText`, as the real run's `main` with its `println!`
/// was, and the task tells one line per refused row naming the body, the row, which file
/// (`app.rs`), its reader and the chain of asks (rule 19). Control: `other`, with no macro
/// call, is made under the same plant, so the refused row is the call's and not the body
/// syntax's.
#[test]
fn a_refused_row_is_told_with_its_reader() {
    let (out, told) = hir_expand::planted_file_call_args(|| made(MACROS, "by_own"));
    let failed = out.expect_err("the body task, its call's argument read off the file by the plant");
    assert_eq!(failed, TaskFailed::Refused { what: "base_db::handles::VfsText" }, "told: {told:#?}");
    assert!(!told.is_empty(), "a refused body told nothing");
    assert!(
        told.iter().any(|line| line.contains("base_db::handles::VfsText") && line.contains("app.rs")),
        "no told line names the file whose text was read: {told:#?}"
    );
    for line in &told {
        assert!(line.contains("by_own"), "{line}");
        assert!(line.contains(", read by ") && line.ends_with(" <- (the step)"), "{line}");
    }
    let (plain, told) = hir_expand::planted_file_call_args(|| made(MACROS, "other"));
    assert!(plain.is_ok() && told.is_empty(), "`other` under the plant: {:?}, told {told:#?}", plain.as_ref().err());
}

/// Two bodies of one file, each calling a macro of the file's crate and a builtin, as `main`
/// calls `println!`: the record of one is found, not made again, when only the other is edited
/// inside: its key (of its own text, `made_in`) is equal, its record's bytes are equal, and its
/// steps' read lists hold no row of the file (`made_in` asserts it of every made record): the
/// call's argument is read behind `MacroCallId::call_input`, covered by the body's own text.
/// Control: the plant (`planted_file_call_args`) puts the file's row back, and the same body
/// is refused by it, so the found record is the boundary's doing.
#[test]
fn a_body_with_a_call_is_found_when_another_body_of_its_file_is_edited() {
    const TWO: &str = r#"
//- minicore: sized, concat
//- /app.rs crate:app deps:core new_source_root:local
macro_rules! own { ($e:expr) => { $e }; }
pub fn two() -> u32 { let _s: &str = core::concat!("c", "d"); own!(2) }
pub fn one() -> u32 { let _s: &str = core::concat!("a", "b"); own!(1) }
"#;
    // Inside `two`, which is written before `one`: `one` moves in the file, its text does not.
    let edited = with(TWO, "own!(2)", "own!({ let two = 2000; two })");
    let (key, bytes) = made_two(TWO, "one");
    let (moved_key, moved_bytes) = made_two(&edited, "one");
    assert_eq!(key, moved_key, "an edit inside `two` moved `one`'s key");
    assert_eq!(bytes, moved_bytes, "an edit inside `two` moved `one`'s record");
    // Premise: the edit is one `two`'s record sees.
    assert_ne!(made_two(TWO, "two").1, made_two(&edited, "two").1, "premise: the edit moves `two`");
    let (planted, told) = hir_expand::planted_file_call_args(|| made_in(TWO, &["core", "app"], "one"));
    assert_eq!(
        planted.map(|_| ()),
        Err(TaskFailed::Refused { what: "base_db::handles::VfsText" }),
        "control: the plant left no row of the file in `one`'s list; told {told:#?}"
    );
}

/// The key and the bytes of `name`'s record of `text` (crates `core` and `app`), which must be
/// made.
fn made_two(text: &str, name: &str) -> (Key<Body>, Vec<u8>) {
    match made_in(text, &["core", "app"], name) {
        (Ok((key, record)), _) => (key, record[..].to_vec()),
        (Err(failed), told) => panic!("`{name}`'s body task: {failed:?}; told: {told:#?}"),
    }
}

/// The planted controls of the transport: a type left out of the answer's items, and an item
/// stored by its id in the writer's database, are each refused by name in the serving
/// snapshot, never served as another body.
#[test]
fn a_broken_transport_is_refused_by_name() {
    for plant in [Plant::DropType, Plant::DatabaseId] {
        let written = planted(plant, || write(FIXTURE));
        let record = read_back(&written);
        let serving = serve(&written, &[body_of(&written, &record)]);
        let db = &serving.db;
        let def = swap(db);
        let refusal = unserved(|| {
            _ = mir_at(db, MirOwner::Body(InferBodyId::DefWithBodyId(def)), MirStage::Built);
        });
        assert_eq!((refusal.def, refusal.kind), (def, Unserved::UNREADABLE), "{plant:?}: {refusal}");
        assert_eq!(refusal.item, Some(written.item.0), "{plant:?} names the item");
    }
}
