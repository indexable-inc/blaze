//! The engine's stored answer as the run path reads it (engine_run_answer.rs `EngineAnswer`,
//! run.md "Run and test read the engine's answer", piece E2), over a real engine-check answer on
//! the simulated Io: a name's out and a cell's digest are its row's; the walk over the engine's
//! answer finds the bodies the engine planned; a body with a type error is not clean, a clean one
//! is. Each holder unit of a body has its own row at its view (D191), read with the units of
//! the stored unit graph as the run reads them. A child of main.rs's tests: its helpers are
//! theirs.

use super::*;

use blaze_drive::{Kind, Name, Tool};
use blaze_ids::ContentId;

use crate::engine_run_answer::EngineAnswer;
use crate::run_ids::{Answer, BodyScratch, Checked, Today, UnitRoot, WorkspaceBody, workspace_of};
use blaze_drive::view::{UnitView, at_view};

const MANIFEST: &str = "[package]\nname = \"one\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";

/// The `BodyCheck` kind's tool: the engine's answer takes none (its key is the row's).
const TOOL: Tool = Tool(ContentId([0; 32]));

/// Two members, `b` calling `a`'s `f`.
fn two_members(io: &Io) {
    write(io, "Cargo.toml", "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n");
    write(io, "a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(io, "a/src/lib.rs", "pub fn f() -> u8 {\n    1\n}\n");
    write(io, "b/Cargo.toml", "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\na = { path = \"../a\" }\n");
    write(io, "b/src/lib.rs", "pub fn h() -> u8 {\n    a::f()\n}\n");
}

/// One crate: `good` is clean, `bad` returns a `&str` where `u8` is declared.
const GOOD_AND_BAD: &str = "pub fn good() -> u8 {\n    1\n}\n\npub fn bad() -> u8 {\n    \"x\"\n}\n";

fn one_crate(io: &Io, source: &str) {
    write(io, "Cargo.toml", MANIFEST);
    write(io, "src/lib.rs", source);
}

/// The `BodyCheck` name of the fn `name` of `source` in `path`, as work.rs `body_name` makes it.
fn body_of(path: &[u8], source: &str, name: &str) -> Name {
    let outline = blaze_file_items::outline(source, blaze_surface::Edition::E2024, blaze_own::testing::heap().alloc());
    let item = outline.items().iter().find(|item| item.name.is_some_and(|range| item.text(source, range) == name)).expect("the fn");
    work::body_name(Name::file(path), &outline, item, source)
}

/// The engine's stored answer on the checkout `ws`, read whole, and the command's store, handed to
/// `f` as the engine's answer: what the run path asks of it.
fn asked<R>(io: &Io, f: impl FnOnce(&bstd::Cx, &mut EngineAnswer<'_, 'static, '_, '_>, &blaze_engine::pages::Kept<'static>) -> R) -> R {
    super::engine_answer_rows_tests::with_store(io, |cx, store, opts, _| {
        let root = crate::engine_answer::root_name(opts.place.checkout, cx.frame());
        let pointer = crate::engine_answer::load_pointer(cx, store, &root).expect("the store reads").expect("engine-check stored an answer").expect("its pointer reads");
        let (kept, _) = crate::engine_answer::load_answer(cx, store, pointer.root, opts.heap.shared()).expect("the store reads").expect("its pages read");
        let mut answer = EngineAnswer::new(&kept.answer, &kept, store);
        f(cx, &mut answer, &kept)
    })
}

/// (a) A crate's `CrateScope` name: `Today::out` is its row's `out`, and `Today::cell` of each
/// cell the row defines is that entry's digest. CONTROL: a name the answer holds no row of has
/// no out, and a cell its row does not define has no digest.
#[test]
fn today_is_the_engines_row() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    two_members(&io);
    let _ = super::engine_check_tests::engine_from(&io, 2, true);
    asked(&io, |_, answer, kept| {
        let answer: &EngineAnswer<'_, '_, '_, '_> = answer;
        let mut scopes = 0;
        let mut defined = 0;
        for (name, row) in kept.answer.rows.iter() {
            if name.kind != Kind::CrateScope {
                continue;
            }
            scopes += 1;
            assert_eq!(answer.out(*name), Some(row.out), "{name}: its out is its row's");
            for (cell, digest) in row.defines.iter() {
                defined += 1;
                assert_eq!(answer.cell(*name, *cell), Some(*digest), "{name}: a defined cell's digest is its row's entry");
            }
        }
        assert!(scopes >= 2, "the two members and the shelf's crates have a `CrateScope` row each: {scopes}");
        assert!(defined > 0, "a crate's scope defines at least one cell");
        let nowhere = Name::file(b"nowhere.rs");
        assert_eq!(answer.out(nowhere), None, "control: no row, no out");
        assert_eq!(answer.cell(nowhere, blaze_drive::Cell([0; 16])), None, "control: no row, no cell");
    });
}

/// (b) The walk `workspace_of` over the engine's answer, from the units down through the Units
/// and Outlines records' parts and then at each member unit's view, lists exactly the
/// `BodyCheck` names the engine's own rows hold. D191: "Every unit that holds a body checks it;
/// the first holder in rank keeps the body's plain name (the base), each other holder's row is
/// the name at its view", and "`blaze test` and `blaze run` build their body-id table for a unit
/// by asking the name at that unit's view": the two members' fns are held by each lib and its
/// lib-test unit, so 4 rows, 2 plain. CONTROL: the walk without the units (the rule before
/// D191) lists the 2 plain rows alone, each one the engine planned.
#[test]
fn the_walk_over_the_engines_answer_finds_the_planned_bodies() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    two_members(&io);
    let _ = super::engine_check_tests::engine_from(&io, 2, true);
    let planned = asked(&io, |_, _, kept| {
        let mut planned: std::vec::Vec<Name> = kept.answer.rows.keys().filter(|name| name.kind == Kind::BodyCheck).copied().collect();
        planned.sort_unstable();
        planned
    });
    assert_eq!(planned.len(), 4, "D191: each member's fn is a planned body in its lib's and its lib-test's rows: {planned:?}");
    let (walked, plain) = asked_with_units(&io, |cx, answer, units| {
        let heap = blaze_own::testing::heap();
        let units_only: std::vec::Vec<UnitRoot> = units.iter().map(|(unit, _)| *unit).collect();
        let walk = workspace_of(cx, &mut *answer, TOOL, &units_only, None, heap.alloc()).unwrap_or_else(|e| panic!("{e}"));
        let mut walked: std::vec::Vec<Name> = walk.bodies.iter().map(|body| body.check).collect();
        walked.sort_unstable();
        walked.dedup();
        let old = workspace_of(cx, &mut *answer, TOOL, &[], None, heap.alloc()).unwrap_or_else(|e| panic!("{e}"));
        let mut plain: std::vec::Vec<Name> = old
            .edges
            .iter()
            .filter_map(|edge| match edge {
                crate::run_ids::Edge::Part { child, .. } if child.kind == Kind::BodyCheck => Some(*child),
                _ => None,
            })
            .collect();
        plain.sort_unstable();
        plain.dedup();
        (walked, plain)
    });
    assert_eq!(walked, planned, "the walk at each unit's view lists the bodies the engine planned, plain and at a view (D191)");
    assert_eq!(plain.len(), 2, "CONTROL: without the units the walk lists the 2 plain rows alone: {plain:?}");
    assert!(plain.iter().all(|name| planned.binary_search(name).is_ok()), "CONTROL: each plain row is one the engine planned: {plain:?} of {planned:?}");
}

/// (c) A body with a type error is `Checked::Errors`, a clean one `Clean`: through the body's
/// facts record (engine_body_facts.rs). CONTROL: both bodies read something, and the clean one
/// covers its own item.
// needs engine_body_facts (E1)
#[test]
fn a_body_with_a_type_error_is_errors_and_a_clean_one_clean() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    one_crate(&io, GOOD_AND_BAD);
    let _ = super::engine_check_tests::engine_from(&io, 2, true);
    let (good, bad) = (body_of(b"src/lib.rs", GOOD_AND_BAD, "good"), body_of(b"src/lib.rs", GOOD_AND_BAD, "bad"));
    asked(&io, |cx, answer, kept| {
        let alloc = blaze_own::testing::heap().alloc();
        let mut scratch = BodyScratch::new(alloc);
        let held = answer.body(cx, good, TOOL, None, &mut scratch).unwrap_or_else(|e| panic!("{e}")).expect("the answer holds the clean body's row");
        assert_eq!(held.checked, Checked::Clean, "a body with no finding is clean");
        assert!(!scratch.covers.is_empty(), "its facts record names the item it covers");
        // Its reads are read by need, from the rows: the row reads something, and it stands today.
        let proof = crate::run_ids::BodyReads::proof(&kept.answer, good, crate::run_ids::Most::USUAL.reads).unwrap_or_else(|e| panic!("{e}"));
        assert!(proof.reads > 0, "its row's reads are folded into its proof");
        assert_eq!(proof.read.digest, proof.today, "right after the check every read stands");
        let held = answer.body(cx, bad, TOOL, None, &mut scratch).unwrap_or_else(|e| panic!("{e}")).expect("the answer holds the failing body's row");
        match held.checked {
            Checked::Errors { errors, .. } => assert_eq!(errors.get(), 1, "one mismatched tail"),
            other => panic!("a body with a type error was {other:?}"),
        }
    });
}

/// (d) A body whose check the engine refused to finish (a planted frame fault in the body named
/// `target`) is never `Clean`: it is `Incomplete`, or `Absent` when no facts record was kept.
/// CONTROL: the other body of the crate stays clean.
// needs engine_body_facts (E1)
#[test]
fn a_body_the_engine_did_not_finish_is_never_clean() {
    let source = "pub fn other() -> u8 {\n    1\n}\n\npub fn target() -> u8 {\n    2\n}\n";
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    one_crate(&io, source);
    let previous = crate::engine_bodies::PLANT_BODY_FAULT.with(|plant| plant.replace(Some(("target", crate::engine_bodies::BodyFault::RowFrame))));
    let _ = super::engine_check_tests::engine_from(&io, 2, true);
    crate::engine_bodies::PLANT_BODY_FAULT.with(|plant| plant.set(previous));
    let (other, target) = (body_of(b"src/lib.rs", source, "other"), body_of(b"src/lib.rs", source, "target"));
    asked(&io, |cx, answer, _| {
        let alloc = blaze_own::testing::heap().alloc();
        let mut scratch = BodyScratch::new(alloc);
        let held = answer.body(cx, target, TOOL, None, &mut scratch).unwrap_or_else(|e| panic!("{e}")).expect("the answer holds the refused body's row");
        assert!(matches!(held.checked, Checked::Incomplete { .. } | Checked::Absent(_)), "a body that did not finish was {:?}", held.checked);
        let held = answer.body(cx, other, TOOL, None, &mut scratch).unwrap_or_else(|e| panic!("{e}")).expect("the answer holds the other body's row");
        assert_eq!(held.checked, Checked::Clean, "control: the fault is one body's alone");
    });
}

/// A package whose bin `extra` is skipped by its features (`required-features`, the feature off),
/// as tempo's `localnet` is.
const SKIPPING_MANIFEST: &str = "[package]\nname = \"one\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[features]\nextra = []\n\n[[bin]]\nname = \"extra\"\npath = \"src/bin/extra.rs\"\nrequired-features = [\"extra\"]\n";
const GOOD: &str = "pub fn good() -> u8 {\n    1\n}\n";
const EXTRA_BIN: &str = "fn main() {\n    let _ = one::good();\n}\n";

/// An answer whose word on skipped files is withheld (`Answer::skipped` always `false`): the
/// planted control of [`a_features_skipped_bin_is_not_walked_over_the_stored_answer`].
struct Unsaid<'x, A: Answer + ?Sized>(&'x mut A);

impl<A: Answer + ?Sized> Today for Unsaid<'_, A> {
    fn out(&self, name: Name) -> Option<ContentId> {
        self.0.out(name)
    }
    fn cell(&self, name: Name, cell: blaze_drive::Cell) -> Option<ContentId> {
        self.0.cell(name, cell)
    }
}

impl<A: Answer + ?Sized> Answer for Unsaid<'_, A> {
    fn open(
        &mut self,
        cx: &bstd::Cx,
        name: Name,
        seen: &mut dyn FnMut(crate::run_ids::Seen<'_>) -> Result<(), crate::run_ids::RunIdsError<'static>>,
    ) -> Result<bool, crate::run_ids::RunIdsError<'static>> {
        self.0.open(cx, name, seen)
    }
    fn skipped(&self, _file: Name) -> bool {
        false
    }
    fn body(
        &mut self,
        cx: &bstd::Cx,
        check: Name,
        tool: Tool,
        known: Option<(ContentId, Name)>,
        out: &mut BodyScratch<'_>,
    ) -> Result<Option<crate::run_ids::BodyFacts>, crate::run_ids::RunIdsError<'static>> {
        self.0.body(cx, check, tool, known, out)
    }
}

/// (e) THE TEMPO CASE (2026-11-21, `blaze test -p tempo-hardfork` after a whole `blaze check`:
/// exit 125, "the walk reached BodyCheck(00a930d79f39), which this request did not settle and the
/// last answer's body id table did not walk"). A package's units record lists every target root,
/// a features-skipped bin's too; the check outlines its file and plans no body task of it
/// (engine_bodies.rs `plan`'s `skipped`), so its `main` has NO ROW (the cause, asserted). Over the
/// stored answer read in place as the command reads it (engine_check.rs `check_for_start`:
/// `Stored`, its root's places), the file is said skipped (`PLACE_SKIPPED`) and the walk does
/// not open it: the lib's body is walked, the bin's is not. CONTROL: the lib's file is not said
/// skipped. PLANTED: the same answer with that word withheld is the typed refusal of the run,
/// naming the bin's body part (rule 3: never walked past).
#[test]
fn a_features_skipped_bin_is_not_walked_over_the_stored_answer() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    write(&io, "Cargo.toml", SKIPPING_MANIFEST);
    write(&io, "src/lib.rs", GOOD);
    write(&io, "src/bin/extra.rs", EXTRA_BIN);
    let _ = super::engine_check_tests::engine_from(&io, 2, true);
    let (extra, lib) = (Name::file(b"src/bin/extra.rs"), Name::file(b"src/lib.rs"));
    let (unchecked, good) = (body_of(b"src/bin/extra.rs", EXTRA_BIN, "main"), body_of(b"src/lib.rs", GOOD, "good"));
    super::engine_answer_rows_tests::with_store(&io, |cx, store, opts, _| {
        let alloc = opts.heap.shared();
        let root = crate::engine_answer::root_name(opts.place.checkout, cx.frame());
        let pointer = crate::engine_answer::load_pointer(cx, store, &root).expect("the store reads").expect("engine-check stored an answer").expect("its pointer reads");
        let directory = crate::engine_answer::open_root(cx, store, pointer.root, alloc).expect("the store reads").expect("its root reads");
        let snapshot = store.snapshot_in(alloc).expect("a snapshot of the store");
        let stored = crate::engine_answer::Stored::new(&directory, &snapshot, &pointer.headers, blaze_engine::pages::Check::Store, alloc);
        let rows = stored.answer();
        let mut answer = EngineAnswer::new(&rows, &stored, store);
        assert!(answer.skipped(extra), "the features-skipped bin's file is said skipped by the root's places");
        assert!(!answer.skipped(lib), "control: the lib's file is checked");
        assert_eq!(answer.out(unchecked), None, "the cause: the skipped bin's body has no row");
        assert!(answer.out(good).is_some(), "control: the lib's body has a row");
        let heap = blaze_own::testing::heap();
        let walk = workspace_of(cx, &mut answer, TOOL, &[], None, heap.alloc()).unwrap_or_else(|e| panic!("the skipped bin stopped the walk: {e}"));
        assert!(walk.bodies.iter().any(|body| body.check == good), "the lib's body is walked");
        assert!(walk.bodies.iter().all(|body| body.check != unchecked), "the bin's body is not");
        assert!(walk.edges.iter().any(|edge| matches!(edge, crate::run_ids::Edge::Part { child, .. } if *child == extra)), "the bin's root stays a part of its units");
        assert!(!walk.edges.contains(&crate::run_ids::Edge::Walked(extra)), "the bin's root is not opened");
        // PLANTED: the word withheld, the bin's row-less body is the refusal seen on tempo.
        match workspace_of(cx, &mut Unsaid(&mut answer), TOOL, &[], None, heap.alloc()) {
            Err(crate::run_ids::RunIdsError::NotWalked { name }) => assert_eq!(name, unchecked, "the refusal names the bin's body part"),
            Err(other) => panic!("refused otherwise: {other}"),
            Ok(_) => panic!("planted: a body part with no row was walked past"),
        }
    });
}

// --- A workspace body's reads, BY NEED from the stored rows (run_ids.rs case 3) ---

/// (c) ROUND TRIP: `check` stores its answer; `test`, a second command, opens the stored answer
/// in place (pages, not a whole load: `Stored::answer`, as engine_start.rs `body_ids` does),
/// builds the body ids, and reads the clean body's reads from those rows at its call: one check
/// read, one proof read whose every read stands today. A body whose check does not run asks
/// none. The table written and read back answers the same id. CONTROL: the failing body is
/// refused by its verdict, and the source was not asked for it.
#[test]
fn a_second_command_reads_the_stored_answers_reads_in_place_by_need() {
    use crate::engine_run_answer::Records as _;
    use crate::run_ids::{Asked, ByNeed, Most, NotRunnable, ReadsFrom, RunIds};
    use crate::store::Store as _;
    use blaze_ladder::body_ids::BodyIds as _;
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    one_crate(&io, GOOD_AND_BAD);
    let _ = super::engine_check_tests::engine_from(&io, 2, true);
    let (good, bad) = (body_of(b"src/lib.rs", GOOD_AND_BAD, "good"), body_of(b"src/lib.rs", GOOD_AND_BAD, "bad"));
    super::engine_answer_rows_tests::with_store(&io, |cx, store, opts, _| {
        let alloc = opts.heap.shared();
        let heap = blaze_own::testing::heap();
        let root = crate::engine_answer::root_name(opts.place.checkout, cx.frame());
        let pointer = crate::engine_answer::load_pointer(cx, store, &root).expect("the store reads").expect("engine-check stored an answer").expect("its pointer reads");
        let directory = crate::engine_answer::open_root(cx, store, pointer.root, alloc).expect("the store reads").expect("its root reads");
        let snapshot = store.snapshot_in(alloc).expect("a snapshot of the store");
        let stored = crate::engine_answer::Stored::new(&directory, &snapshot, &pointer.headers, blaze_engine::pages::Check::Store, alloc);
        let rows = stored.answer();
        let at = stored.record(Name::unit_graph()).expect("the page reads").expect("the unit graph has a record");
        let graph = store.get(cx, at).expect("the store reads").expect("the unit graph is stored");
        let blaze_drive::record::Stored::Whole(view) = blaze_drive::record::open(graph.bytes).expect("the unit graph's record reads") else { panic!("a whole unit graph record") };
        let list = crate::graph_payload::sections(view.payload).expect("its sections").crates().expect("its crate list reads").expect("the checkout has crates");
        let units = crate::run_ids::unit_roots(&list, heap.alloc()).unwrap_or_else(|e| panic!("{e}"));
        let tools: [Tool; blaze_drive::key::KINDS] = core::array::from_fn(|at| Tool(ContentId([at as u8; 32])));
        let mut answer = EngineAnswer::new(&rows, &stored, store);
        let walk = workspace_of(cx, &mut answer, TOOL, &units, None, heap.alloc()).unwrap_or_else(|e| panic!("{e}"));
        let item_of = |check: Name| walk.bodies.iter().find(|body| body.check == check).map(|body| body.item).expect("the body is walked");
        let (good_item, bad_item) = (item_of(good), item_of(bad));
        let erased = blaze_own::Erased::new(cx.frame());
        let bump = erased.bump();
        let ids = RunIds::new(cx, blaze_toolchain::ToolchainId([7; 32]), &list, &tools, &walk.bodies, &walk.edges, &mut answer, None, Most::USUAL, bump)
            .unwrap_or_else(|e| panic!("{e}"));
        drop(answer);
        let asked = Asked::default();
        let built = ids.reading::<ByNeed>(ReadsFrom::new(&rows, &asked, Most::USUAL)).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(asked.bodies(), 0, "nothing is read before a call");
        let _ran = built.runnable(good_item).unwrap_or_else(|why| panic!("the clean body runs: {why}"));
        assert_eq!(asked.bodies(), 1, "its call read its check's reads, once");
        assert!(asked.reads() > 0, "and folded the row's reads");
        let id = built.body_check(good_item).expect("the clean body has an id");
        assert_eq!(id.reads.len(), 1, "one read: its check's proof");
        assert_eq!(built.today(&id.reads[0].fact), Some(id.reads[0].digest), "every read of the stored row stands today");
        let (key, read) = (id.key, id.reads[0]);
        assert!(matches!(built.runnable(bad_item), Err(NotRunnable::Checked(Checked::Errors { .. }))), "control: the failing body is refused by its verdict");
        assert_eq!(asked.bodies(), 1, "control: and its reads were not asked");
        // The table's record, read back: the same id, its proof read from the rows again.
        let bytes = built.write(heap.alloc());
        let back = RunIds::read(&bytes, bump).unwrap_or_else(|e| panic!("{e}"));
        let again = Asked::default();
        let back = back.reading::<ByNeed>(ReadsFrom::new(&rows, &again, Most::USUAL)).unwrap_or_else(|e| panic!("{e}"));
        let id = back.body_check(good_item).expect("the read table has the clean body's id");
        assert_eq!((id.key, id.reads), (key, &[read][..]), "the read table answers the same id");
        assert_eq!(again.bodies(), 1, "read by need again");
    });
}

/// (d) The stored rows as a source of proofs: past `most` reads a row is a typed
/// `TooManyReads` naming the check and the count; a check the answer has no row of is `NoRow`.
/// CONTROL: the usual cap folds the clean body's row.
#[test]
fn the_stored_rows_refuse_past_the_reads_cap_by_name() {
    use crate::run_ids::{BodyReads as _, Most, Unread};
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    one_crate(&io, GOOD_AND_BAD);
    let _ = super::engine_check_tests::engine_from(&io, 2, true);
    let good = body_of(b"src/lib.rs", GOOD_AND_BAD, "good");
    asked(&io, |_, _, kept| {
        let proof = kept.answer.proof(good, Most::USUAL.reads).unwrap_or_else(|e| panic!("control: {e}"));
        match kept.answer.proof(good, 0) {
            Err(Unread::TooManyReads { check, count, most }) => assert_eq!((check, count, most), (good, proof.reads as usize, 0), "names the check, its count and the cap"),
            other => panic!("past the cap: {other:?}"),
        }
        let nowhere = Name::new(Kind::BodyCheck, ContentId([0xee; 32]));
        assert_eq!(kept.answer.proof(nowhere, Most::USUAL.reads), Err(Unread::NoRow { check: nowhere }), "no row: a typed error");
    });
}

// --- D191: each holder unit checks a body under its own view ---
//
// These pass only once BOTH halves are in: the planner's view rows (engine_body_plan.rs, the
// planner writer's: every holder unit checks a body, the first in rank under the plain name,
// each other at `at_view(name, At(ViewKey::of_unit(its key)))`) and the run's asks (run_ids.rs
// `workspace_of` with the units). They are not ignored: until the planner's save lands they
// fail on the first assert that names a view row.

/// The checkout's member units as the run asks them (run_ids.rs `unit_roots` over the stored
/// unit graph's crate list, as engine_start.rs `body_ids` does), each with whether it is a test
/// build, and the engine's stored answer, handed to `f`.
fn asked_with_units<R>(io: &Io, f: impl FnOnce(&bstd::Cx, &mut EngineAnswer<'_, 'static, '_, '_>, &[(UnitRoot, bool)]) -> R) -> R {
    use crate::store::Store as _;
    super::engine_answer_rows_tests::with_store(io, |cx, store, opts, _| {
        let root = crate::engine_answer::root_name(opts.place.checkout, cx.frame());
        let pointer = crate::engine_answer::load_pointer(cx, store, &root).expect("the store reads").expect("engine-check stored an answer").expect("its pointer reads");
        let (kept, _) = crate::engine_answer::load_answer(cx, store, pointer.root, opts.heap.shared()).expect("the store reads").expect("its pages read");
        let graph = store.get(cx, kept.records[&Name::unit_graph()]).expect("the store reads").expect("the unit graph is stored");
        let blaze_drive::record::Stored::Whole(view) = blaze_drive::record::open(graph.bytes).expect("the unit graph's record reads") else { panic!("a whole unit graph record") };
        let list = crate::graph_payload::sections(view.payload).expect("its sections").crates().expect("its crate list reads").expect("the checkout has crates");
        let roots = crate::run_ids::unit_roots(&list, blaze_own::testing::heap().alloc()).unwrap_or_else(|e| panic!("{e}"));
        let tests = (0..blaze_world::crate_list::CrateGraph::len(&list)).filter(|&at| list.member(at)).map(|at| list.test(at));
        let units: std::vec::Vec<(UnitRoot, bool)> = roots.iter().copied().zip(tests).collect();
        let mut answer = EngineAnswer::new(&kept.answer, &kept, store);
        f(cx, &mut answer, &units)
    })
}

/// A checker key's crate prefix: its first 8 bytes (run_ids.rs `prefix_of`).
fn prefix(key: u128) -> [u8; 8] {
    let [a, b, c, d, e, f, g, h, ..] = key.to_be_bytes();
    [a, b, c, d, e, f, g, h]
}

/// The one unit rooted at `root` that is (or is not) a test build.
fn unit_at(units: &[(UnitRoot, bool)], root: &[u8], test: bool) -> UnitRoot {
    let mut found = units.iter().filter(|(unit, is_test)| unit.root == Name::file(root) && *is_test == test).map(|(unit, _)| *unit);
    let one = found.next().unwrap_or_else(|| panic!("no unit at {} (test: {test}): {units:?}", std::string::String::from_utf8_lossy(root)));
    assert!(found.next().is_none(), "two units at one root with one test flag: {units:?}");
    one
}

/// The walk's rows of the body `check`, under any name: its plain name or a view of it.
fn rows_of_body(rows: &[WorkspaceBody], check: Name, units: &[(UnitRoot, bool)]) -> std::vec::Vec<WorkspaceBody> {
    rows.iter().filter(|row| row.check == check || units.iter().any(|(unit, _)| row.check == at_view(check, UnitView::At(unit.view)))).copied().collect()
}

/// `g` has a type error only in the test build (a `#[cfg(test)]` statement); a test calls it.
const TEST_ONLY_ERROR: &str = "pub fn g() -> u8 {\n    #[cfg(test)]\n    let x: u8 = \"not a number\";\n    #[cfg(not(test))]\n    let x: u8 = 2;\n    x\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn calls_g() {\n        let _ = super::g();\n    }\n}\n";

/// (f) D191 over a real check: `g`, held by the library and its test build, has two rows, and
/// they differ as the units do: the library's (the first holder, the plain name) is clean, the
/// test build's (at its view) is `Errors`, its keys the test build's. The walk with the units
/// puts each row under its own unit's prefix. PLANTED (the rule before D191: the units not
/// asked, the base row alone): the test build's keys have no row of `g`, so the run refuses the
/// test's call by name rather than run the library's clean `g` in its place.
#[test]
fn a_body_held_by_a_library_and_its_test_build_has_a_row_under_each() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    one_crate(&io, TEST_ONLY_ERROR);
    let _ = super::engine_check_tests::engine_from(&io, 2, true);
    let g = body_of(b"src/lib.rs", TEST_ONLY_ERROR, "g");
    asked_with_units(&io, |cx, answer, units| {
        let (lib, test) = (unit_at(units, b"src/lib.rs", false), unit_at(units, b"src/lib.rs", true));
        let at_test = at_view(g, UnitView::At(test.view));
        let alloc = blaze_own::testing::heap().alloc();
        let mut scratch = BodyScratch::new(alloc);
        let base = answer.body(cx, g, TOOL, None, &mut scratch).unwrap_or_else(|e| panic!("{e}")).expect("the library's row of `g`, plain");
        assert_eq!(base.checked, Checked::Clean, "the library's `g` (not test) is clean");
        assert!(!scratch.covers.is_empty() && scratch.covers.iter().all(|key| prefix(key.0) == lib.prefix), "the base row's keys are the library's: {:?}", scratch.covers);
        let viewed = answer.body(cx, at_test, TOOL, None, &mut scratch).unwrap_or_else(|e| panic!("{e}")).expect("the test build's row of `g`, at its view (the planner's save)");
        assert!(matches!(viewed.checked, Checked::Errors { .. }), "the test build's `g` holds the test-only error: {:?}", viewed.checked);
        assert!(!scratch.covers.is_empty() && scratch.covers.iter().all(|key| prefix(key.0) == test.prefix), "the view row's keys are the test build's: {:?}", scratch.covers);

        let units_only: std::vec::Vec<UnitRoot> = units.iter().map(|(unit, _)| *unit).collect();
        let heap = blaze_own::testing::heap();
        let walk = workspace_of(cx, &mut *answer, TOOL, &units_only, None, heap.alloc()).unwrap_or_else(|e| panic!("{e}"));
        let rows = rows_of_body(&walk.bodies, g, units);
        assert!(rows.iter().any(|row| row.check == g && prefix(row.item.0) == lib.prefix), "the library's row: {rows:?}");
        assert!(rows.iter().any(|row| row.check == at_test && prefix(row.item.0) == test.prefix), "the test build's row at its view: {rows:?}");
        assert!(rows.iter().all(|row| (row.check == g) == (prefix(row.item.0) == lib.prefix)), "no unit's keys under another's name: {rows:?}");

        // PLANTED: the old rule, the units not asked.
        let old = workspace_of(cx, &mut *answer, TOOL, &[], None, heap.alloc()).unwrap_or_else(|e| panic!("{e}"));
        let old_rows = rows_of_body(&old.bodies, g, units);
        assert!(old_rows.iter().all(|row| prefix(row.item.0) != test.prefix), "planted: the test build's keys have a row under the old rule: {old_rows:?}");
    });
}

/// A file both the lib and the bin `mod`.
const SHARED: &str = "pub fn word() -> u8 {\n    7\n}\n";
const SHARED_LIB: &str = "mod shared;\n\npub fn lib_word() -> u8 {\n    shared::word()\n}\n";
const SHARED_MAIN: &str = "mod shared;\n\nfn main() {\n    let _ = shared::word();\n}\n";

/// (g) D191 for a lib and a bin: `word` (src/shared.rs, which both roots `mod`) has a row under
/// the lib's keys and one under the bin's, each under the plain name or its unit's view, never
/// one unit's keys under another unit's name. PLANTED (the old rule): only one of the two has
/// a row, so the other's call is refused by name.
#[test]
fn a_file_a_lib_and_a_bin_both_mod_has_a_row_under_each() {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    write(&io, "Cargo.toml", MANIFEST);
    write(&io, "src/lib.rs", SHARED_LIB);
    write(&io, "src/main.rs", SHARED_MAIN);
    write(&io, "src/shared.rs", SHARED);
    let _ = super::engine_check_tests::engine_from(&io, 2, true);
    let word = body_of(b"src/shared.rs", SHARED, "word");
    asked_with_units(&io, |cx, answer, units| {
        let (lib, bin) = (unit_at(units, b"src/lib.rs", false), unit_at(units, b"src/main.rs", false));
        let units_only: std::vec::Vec<UnitRoot> = units.iter().map(|(unit, _)| *unit).collect();
        let heap = blaze_own::testing::heap();
        let walk = workspace_of(cx, &mut *answer, TOOL, &units_only, None, heap.alloc()).unwrap_or_else(|e| panic!("{e}"));
        let rows = rows_of_body(&walk.bodies, word, units);
        for unit in [lib, bin] {
            assert!(rows.iter().any(|row| prefix(row.item.0) == unit.prefix), "a row of `word` under {unit:?}: {rows:?}");
        }
        for row in &rows {
            let (owner, _) = units.iter().find(|(unit, _)| unit.prefix == prefix(row.item.0)).unwrap_or_else(|| panic!("a row of no member unit: {row:?}"));
            assert!(row.check == word || row.check == at_view(word, UnitView::At(owner.view)), "a row under another unit's name: {row:?}");
        }
        let old = workspace_of(cx, &mut *answer, TOOL, &[], None, heap.alloc()).unwrap_or_else(|e| panic!("{e}"));
        let old_rows = rows_of_body(&old.bodies, word, units);
        let held = [lib, bin].iter().filter(|unit| old_rows.iter().any(|row| prefix(row.item.0) == unit.prefix)).count();
        assert_eq!(held, 1, "planted: under the old rule one of the lib and the bin has a row of `word`: {old_rows:?}");
    });
}
