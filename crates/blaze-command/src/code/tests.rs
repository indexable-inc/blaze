//! The code path's rules, each with a planted control its oracle catches:
//!
//! - a plan of an instance with no body (drop glue) has a read list, and it moves when a
//!   field's type gains a `Drop` impl ([`drop_glue_reads_move_when_a_field_type_gains_drop`]);
//! - an edit of a callee renames the caller's relocation and keeps the caller's code key, so
//!   the caller compiles nothing ([`a_callee_edit_renames_a_place_and_keeps_the_callers_key`]);
//! - a program's rows with one left out are refused naming it, before any template is read
//!   ([`a_row_left_out_is_caught_by_name`]);
//! - a static's code is keyed by its value's content, never which static it is, under the
//!   key the driver's build uses, and links to its value
//!   ([`a_static_code_record_is_keyed_by_its_value_and_links_to_it`]).
//!
//! Every plan here is made through the public front end (`HirFrontend::plan_ask`, `plan`), in
//! fixture databases with the item store's boundaries marked, as a plan task makes it.

use std::alloc::Global;

use base_db::{Crate, Db, FileId, all_crates};
use blaze_codegen::{
    Arch, Target,
    plan::{Mode, OptLevel, Origin, Override, Policy, Profile},
    settings::{DebugInfo, Panic, Pgo, Relocation, Sanitize, Toolchain},
};
use blaze_ids::{ContentId, Hasher};
use blaze_item_hash::values;
use hir_expand::fixture;
use hir_ty::next_solver::goal_store::FactRef;
use hir_ty_codegen::v2::{BuildTime, Databases, Edge, HashSource, HirFrontend, ItemHashes, PlanView};

use super::{CodeError, Rows, code_key, code_static, link_rows, places};

/// A dev policy: aarch64 at `-O0`, whose units are the stencil backend's. Nothing here
/// compiles, so the stencils named are no real set.
fn policy() -> Policy {
    Policy {
        toolchain: Toolchain {
            id: ContentId([1; 32]),
            target: "aarch64-unknown-linux-gnu".to_owned(),
            arch: Arch::Aarch64,
            cpu: "neoverse-v2".to_owned(),
            features: Vec::new(),
            cranelift: "0.136".to_owned(),
            llvm: None,
            stencils: Some("ab".repeat(32)),
            frame_pointers: true,
            relocation: Relocation::Pic,
        },
        profile: Profile {
            opt_level: OptLevel::O0,
            overflow_checks: false,
            debug_assertions: false,
            debuginfo: DebugInfo::None,
            panic: Panic::Unwind,
            packages: Default::default(),
            build_override: Override::default(),
            stencils: false,
        },
        mode: Mode::Output,
        fold: false,
        sanitize: Sanitize::default(),
        pgo: Pgo::None,
        proc_macro_bridge: false,
        report: false,
        heap_to_stack: false,
        fuel: false,
    }
}

/// A fixture database whose memo keeps read lists at the item store's boundaries.
fn database(text: &str) -> Db {
    fixture::db_with_boundaries(text, blaze_item_hash::reads::mark_boundaries, blaze_memo::Reads::Kept)
        .into_db()
}

/// A front end over `db` with blaze items' hashes, every file one name.
fn front<'db>(db: &'db Db, policy: &Policy) -> HirFrontend<'db> {
    let names = ItemHashes::new(Box::new(|_: FileId| Some("named".to_owned())));
    HirFrontend::new(Databases::new(db), HashSource::Items(names), BuildTime(false)).with_policy(policy)
}

fn crate_named(db: &Db, name: &str) -> Crate {
    all_crates(db)
        .iter()
        .copied()
        .find(|it| {
            it.extra_data(db).display_name.as_ref().is_some_and(|it| it.crate_name().as_str() == name)
        })
        .unwrap_or_else(|| panic!("no crate `{name}`"))
}

/// Every plan `main::roots` reaches, as `(name, ask, bytes)`, each once, sorted by name: the
/// root's plan, then each function edge's plan, as a driver with no database walks them.
fn plans(frontend: &HirFrontend<'_>, db: &Db) -> Vec<(ContentId, Vec<u8>, Vec<u8>)> {
    let main = crate_named(db, "main");
    let root = frontend.root(hir_def::test_items::function(db, main, "roots"));
    let asked = frontend.plan_ask(&root, &Origin::Std, Global).expect("the root's ask");
    let mut next = vec![(asked.name, asked.ask.to_vec())];
    let mut all: Vec<(ContentId, Vec<u8>, Vec<u8>)> = Vec::new();
    while let Some((name, ask)) = next.pop() {
        assert!(all.len() < 512, "the program reaches more than 512 plans");
        if all.iter().any(|(it, _, _)| *it == name) {
            continue;
        }
        let planned = (frontend.plan(&ask, &name, Global))
            .unwrap_or_else(|why| panic!("no plan of {name:?}: {why:?}"));
        let view = PlanView::read(&planned.bytes).expect("a plan reads");
        for edge in view.edges() {
            if let Edge::Function { name, ask, .. } = edge.expect("an edge reads") {
                next.push((name, ask.to_vec()));
            }
        }
        all.push((name, ask, planned.bytes.to_vec()));
    }
    all.sort_by(|a, b| a.0.cmp(&b.0));
    all
}

/// The plan among `all` whose describe's path holds each of `parts`.
fn plan_of<'a>(all: &'a [(ContentId, Vec<u8>, Vec<u8>)], parts: &[&str]) -> &'a (ContentId, Vec<u8>, Vec<u8>) {
    all.iter()
        .find(|(_, _, bytes)| {
            let name = PlanView::read(bytes).expect("a plan reads").head.name;
            parts.iter().all(|it| name.contains(it))
        })
        .unwrap_or_else(|| {
            let names: Vec<&str> =
                all.iter().map(|(_, _, it)| PlanView::read(it).expect("a plan reads").head.name).collect();
            panic!("no plan of {parts:?} among {names:?}")
        })
}

/// `Noisy` has a `Drop` impl; `Outer` holds an `Inner` and a `Noisy`, so its drop glue drops
/// the `Noisy` alone. The second program gives `Inner` a `Drop` impl and nothing else.
const DROPS: &str = r#"
//- minicore: sized, drop
//- /main.rs crate:main
pub struct Noisy(pub u32);
impl Drop for Noisy { fn drop(&mut self) {} }
pub struct Inner(pub u32);
pub struct Outer { pub a: Inner, pub b: Noisy }
pub fn roots() {
    let _outer = Outer { a: Inner(1), b: Noisy(2) };
}
"#;

const DROPS_INNER: &str = r#"
//- minicore: sized, drop
//- /main.rs crate:main
pub struct Noisy(pub u32);
impl Drop for Noisy { fn drop(&mut self) {} }
pub struct Inner(pub u32);
impl Drop for Inner { fn drop(&mut self) {} }
pub struct Outer { pub a: Inner, pub b: Noisy }
pub fn roots() {
    let _outer = Outer { a: Inner(1), b: Noisy(2) };
}
"#;

/// The home crate of the plan of `ask` in the database `frontend` is made over.
fn home(frontend: &HirFrontend<'_>, ask: &[u8]) -> Crate {
    frontend.plan_home(ask, Global).expect("a home").expect("drop glue has a home")
}

/// The facts the plan of `ask` (named `name`) read in `db`, classified as a plan task
/// classifies them (`fact_of_in`, asked by the home, covering no body), each with its answer
/// in `db`. `db` is fresh (no plan made in it yet), so every read is the describe's own run.
fn read_list(frontend: &HirFrontend<'_>, db: &Db, name: &ContentId, ask: &[u8]) -> Vec<(FactRef, Option<ContentId>)> {
    let home = home(frontend, ask);
    let (made, reads) = blaze_memo::record(blaze_memo::Database::memo(db), || frontend.plan(ask, name, Global))
        .expect("the test database keeps read lists");
    made.expect("the plan, recorded");
    let mut facts = Vec::new();
    let mut refused = Vec::new();
    let mut refused_rows = Vec::new();
    for read in reads.iter().copied() {
        match blaze_item_hash::reads::fact_of_in(db, home, None, read) {
            hir_ty::body_store::Classified::Fact(fact) | hir_ty::body_store::Classified::Scan(fact, _) => {
                facts.push(fact)
            }
            other => {
                refused.push(format!("{}: {other:?}", read.kind().name()));
                refused_rows.push(read);
            }
        }
    }
    if !refused.is_empty() {
        // Who read the rows no fact stands for: the plan again in the same database (its entries
        // now hits, whose lists still hold what each run read), under `record_readers`, each
        // refused row printed with the innermost tracked query that read it, and with the part
        // of the plan (`plan_reads_by_part`) that reaches it.
        let (again, named) =
            blaze_memo::record_readers(blaze_memo::Database::memo(db), || frontend.plan(ask, name, Global))
                .expect("the test database keeps read lists");
        again.expect("the plan, made again");
        let parts = frontend.plan_reads_by_part(ask).expect("the test database keeps read lists");
        let readers = (named.rows.iter())
            .filter(|(read, _)| refused_rows.contains(read))
            .map(|(read, reader)| {
                let held = (parts.iter())
                    .filter(|(_, rows)| rows.contains(read))
                    .map(|(part, _)| format!("{part:?}"))
                    .collect::<Vec<_>>();
                // The path of asks, innermost first: the reader, its asker, up to the work.
                let mut path = String::new();
                for kind in named.chain(*reader).skip(1) {
                    path.push_str(" <- ");
                    path.push_str(kind.name());
                }
                // Which file, for a row of a file's text or root (one helper with the body
                // task's told lines).
                let file = blaze_item_hash::reads::file_of_row(db, *read)
                    .map(|it| format!(" of `{it}`"))
                    .unwrap_or_default();
                format!("{}{file} read by {reader}{path} <- (the work), in {held:?}", read.kind().name())
            })
            .collect::<Vec<_>>();
        panic!("rows of the plan no fact stands for: {refused:?}; their readers: {readers:#?}");
    }
    facts.sort_by_key(|it| blaze_front::facts::id_of(it).expect("a fact with an id"));
    facts.dedup();
    facts.into_iter().map(|it| (it, values::value(db, home, it))).collect()
}

/// Of `facts` (read in the first program, with their answers there), those whose answer in
/// `db`, asked by `home` (the plan's home there), is another: what makes the driver plan again.
fn moved(facts: &[(FactRef, Option<ContentId>)], db: &Db, home: Crate) -> Vec<FactRef> {
    (facts.iter())
        .filter(|(fact, was)| values::value(db, home, *fact) != *was)
        .map(|(fact, _)| *fact)
        .collect()
}

/// The invariant (hir-ty-codegen's `plan` doc, "An instance with no body"): the plan of
/// `Outer`'s drop glue has a read list, its name is the same in both programs, and once
/// `Inner` gains a `Drop` impl a fact of its read list answers otherwise, so the driver plans
/// it again. The planted control: the read list cut to the item store's own syntax kinds that
/// adding an impl cannot change (fields, field order, attributes, settings, crate names) is
/// shorter than the real one and misses the move, so the move is in the facts the cut drops
/// (the solver's: `Drop` implemented, drop check), which the read list holds because the plan
/// covers no body.
///
/// The glue's describe path is `core::ptr::drop_glue::<main::Outer>` (the first program
/// reaches `main::roots`, `drop_glue::<main::Outer>`, `drop_glue::<main::Noisy>` and
/// `<main::Noisy as Drop>::drop`; the second also `drop_glue::<main::Inner>`).
#[test]
fn drop_glue_reads_move_when_a_field_type_gains_drop() {
    let policy = policy();
    let glue = ["drop_glue", "main::Outer"];
    let (one, two) = (database(DROPS), database(DROPS_INNER));
    let (first, second) = (front(&one, &policy), front(&two, &policy));
    let glue_one = plan_of(&plans(&first, &one), &glue).clone();
    let glue_two = plan_of(&plans(&second, &two), &glue).clone();
    assert_eq!((glue_one.0, &glue_one.1), (glue_two.0, &glue_two.1), "one drop glue, one name and ask in both");

    // The read list is recorded in a fresh database of the first program, where no describe
    // ran yet, as a plan task's snapshot is.
    let fresh = database(DROPS);
    let facts = read_list(&front(&fresh, &policy), &fresh, &glue_one.0, &glue_one.1);
    assert!(!facts.is_empty(), "the drop glue's plan read nothing: no read list to move");
    let home_two = home(&second, &glue_two.1);
    let moved_all = moved(&facts, &two, home_two);
    assert!(!moved_all.is_empty(), "no fact of the drop glue's read list moved when `Inner` gained `Drop`");

    let cut = [
        values::kinds::FIELDS,
        values::kinds::FIELD_ORDER,
        values::kinds::ATTRS,
        values::kinds::SETTING,
        values::kinds::CRATE_SETTINGS,
        values::kinds::CRATE_NAMES,
    ];
    let control: Vec<_> = facts.iter().filter(|(fact, _)| cut.contains(&fact.kind)).copied().collect();
    assert!(control.len() < facts.len(), "the control's premise: the cut drops some of the read list");
    assert!(
        moved(&control, &two, home_two).is_empty(),
        "the control moved: the cut read list is not what misses the move"
    );
    assert!(
        moved_all.iter().any(|it| !cut.contains(&it.kind)),
        "every moved fact is of a kind the control keeps"
    );
}

/// `callee` returns 1; the second program's returns 2, and nothing else differs.
const CALLS: &str = r#"
//- minicore: sized
//- /main.rs crate:main
pub fn callee() -> u32 { 1 }
pub fn roots() -> u32 { callee() }
"#;

const CALLS_EDITED: &str = r#"
//- minicore: sized
//- /main.rs crate:main
pub fn callee() -> u32 { 2 }
pub fn roots() -> u32 { callee() }
"#;

/// `plans` as the code path takes them: `(name, bytes)`, sorted by name.
fn borrowed(all: &[(ContentId, Vec<u8>, Vec<u8>)]) -> Vec<(ContentId, &[u8])> {
    all.iter().map(|(name, _, bytes)| (*name, &bytes[..])).collect()
}

/// The invariant: an edit of `callee`'s body leaves `roots`' plan the same bytes under the
/// same name, renames the place of its call (`Target::Function` of the callee's new key), and
/// leaves `roots`' code key the same, so the driver finds its code record and compiles nothing;
/// the link instantiates the same template at the new place. The planted control: a key that
/// holds the places' targets (a callee's key) moves with the callee, which the oracle names.
#[test]
fn a_callee_edit_renames_a_place_and_keeps_the_callers_key() {
    let policy = policy();
    let (one, two) = (database(CALLS), database(CALLS_EDITED));
    let (first, second) = (front(&one, &policy), front(&two, &policy));
    let (all_one, all_two) = (plans(&first, &one), plans(&second, &two));
    let (caller_one, caller_two) = (plan_of(&all_one, &["roots"]), plan_of(&all_two, &["roots"]));
    assert_eq!(caller_one, caller_two, "the caller's plan: one name, one ask, the same bytes");
    let (callee_one, callee_two) = (plan_of(&all_one, &["callee"]), plan_of(&all_two, &["callee"]));
    assert_eq!(callee_one.0, callee_two.0, "the callee's plan keeps its name");
    assert_ne!(callee_one.2, callee_two.2, "the premise: the callee's plan records its new code");

    let view = PlanView::read(&caller_one.2).expect("the caller's plan reads");
    let at = |all: &[(ContentId, Vec<u8>, Vec<u8>)]| places::<Global>(&policy, view.edges(), &borrowed(all), Global).expect("places");
    let (at_one, at_two) = (at(&all_one), at(&all_two));
    assert_ne!(at_one, at_two, "the callee's edit renames the caller's place of it");
    let key = |at: &[Target]| code_key::<Global>(&policy, &view.head, at).expect("a code key").1;
    assert_eq!(key(&at_one), key(&at_two), "the caller's code key moved with its callee: it would compile again");

    let planted = |at: &[Target]| {
        let mut hasher = Hasher::new("planted: a code key over the places' targets");
        hasher.part(format!("{:?}", key(at)).as_bytes()).part(format!("{at:?}").as_bytes());
        hasher.finish()
    };
    assert_ne!(planted(&at_one), planted(&at_two), "the control: a key over the targets moves with the callee");
}

/// The invariant: a program's rows that leave out the callee's are refused before any template
/// is read, the error naming the callee by its plan and path and the row that calls it. The
/// control: with every row there, the rows pass the closure and the first error is the code
/// bytes' (here none are templates), so the refusal is the left-out row's.
#[test]
fn a_row_left_out_is_caught_by_name() {
    let policy = policy();
    let db = database(CALLS);
    let frontend = front(&db, &policy);
    let all = plans(&frontend, &db);
    let plans = borrowed(&all);
    let (caller, callee) = (plan_of(&all, &["roots"]).0, plan_of(&all, &["callee"]).0);
    let none: &[u8] = &[];
    let functions: Vec<(ContentId, &[u8])> = all.iter().map(|(name, _, _)| (*name, none)).collect();
    let left_out: Vec<(ContentId, &[u8])> =
        functions.iter().copied().filter(|(name, _)| *name != callee).collect();

    match link_rows(&policy, Rows { functions: &left_out, statics: &[] }, &plans, &[], Global) {
        Err(CodeError::Unlinked { callee: named, path, by }) => {
            assert_eq!((named, by), (callee, caller), "the refusal names the callee and its caller");
            assert!(path.contains("callee"), "the refusal names the callee's path: {path}");
        }
        other => panic!("rows without the callee's passed the closure: {:?}", other.map(|_| ())),
    }
    match link_rows(&policy, Rows { functions: &functions, statics: &[] }, &plans, &[], Global) {
        Err(CodeError::Blob(_)) => {}
        other => panic!("the control: every row there, the first error is the code bytes': {:?}", other.map(|_| ())),
    }
}

/// Two statics of one value and one of another.
const STATICS: &str = r#"
//- minicore: sized
//- /main.rs crate:main
pub static SEVEN: u32 = 7;
pub static ALSO_SEVEN: u32 = 7;
pub static EIGHT: u32 = 8;
pub fn roots() -> u32 { SEVEN + ALSO_SEVEN + EIGHT }
"#;

/// The invariant: a static's code record (`code_static` of its plan) is filed under the key
/// the driver's build files its template under (`static_template_key` of its describe at its
/// places), which is its value's content and not which static it is: `SEVEN` and
/// `ALSO_SEVEN` share one key, and the link instantiates each as its own static, by item, with
/// the value's bytes. The control: `EIGHT`, of another value, has another key, so the shared
/// key is the value's and not a constant.
#[test]
fn a_static_code_record_is_keyed_by_its_value_and_links_to_it() {
    use blaze_codegen::{Init, backend::Frontend as _, driver::static_template_key};
    use hir_ty_codegen::v2::{HirStatic, StaticPlanView};

    let policy = policy();
    let db = database(STATICS);
    let frontend = front(&db, &policy);
    let main = crate_named(&db, "main");
    let made = |name: &str| {
        let value = HirStatic(hir_def::test_items::static_item(&db, main, name));
        let asked = frontend.static_plan_ask(&value, Global).expect("a static's ask");
        let planned = frontend.static_plan(&asked.ask, &asked.name, Global).expect("a static's plan");
        let view = StaticPlanView::read(&planned.bytes).expect("a static plan reads");
        let at = places::<Global>(&policy, view.edges(), &[], Global).expect("its places");
        let record = code_static(&frontend, &policy, &planned.bytes, &at, Global)
            .unwrap_or_else(|why| panic!("no code of `{name}`: {why:?}"));
        let described = frontend.describe_static(&value).expect("a describe");
        let settings = policy.choose(&view.head.origin, None).expect("settings").first.settings();
        assert_eq!(
            record.key,
            static_template_key(&described, &at, &settings),
            "`{name}`: the code task and the driver's build key one template two ways"
        );
        assert_eq!((record.item, record.instance), (described.item, described.instance), "`{name}`: which static");
        (asked.name, planned.bytes.to_vec(), record)
    };
    let (seven, also, eight) = (made("SEVEN"), made("ALSO_SEVEN"), made("EIGHT"));
    assert_eq!(seven.2.key, also.2.key, "two statics of one value: one template key");
    assert_ne!(seven.2.item, also.2.item, "the premise: two statics");
    assert_ne!(seven.2.key, eight.2.key, "the control: another value, another key");

    let mut plans: Vec<(ContentId, &[u8])> =
        [&seven, &also, &eight].iter().map(|it| (it.0, &it.1[..])).collect();
    plans.sort_by(|a, b| a.0.cmp(&b.0));
    let rows: Vec<(ContentId, &[u8])> = [&seven, &also, &eight].iter().map(|it| (it.0, &it.2.bytes[..])).collect();
    let linked = link_rows(&policy, Rows { functions: &[], statics: &rows }, &[], &plans, Global)
        .unwrap_or_else(|why| panic!("the statics' link rows: {why:?}"));
    assert_eq!(linked.statics.len(), 3, "one static a row");
    for ((_, _, record), (value, linked)) in
        [&seven, &also, &eight].into_iter().zip([7u32, 7, 8].into_iter().zip(&linked.statics))
    {
        assert_eq!((linked.item, linked.instance), (record.item, record.instance), "instantiated as its own static");
        match &linked.init {
            Init::Bytes { bytes, .. } => assert_eq!(&bytes[..], &value.to_le_bytes()[..], "its value's bytes"),
            Init::Zeroed { .. } => panic!("a nonzero value linked as zeroed"),
        }
    }
}
