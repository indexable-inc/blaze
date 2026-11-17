//! One test per property, each over every schedule of a small scenario; a seeded random test
//! for larger ones; the model's own checks (that it finds a planted bug, and that a crash
//! keeps what the file system model says); and, ignored, the findings it reports today.
//!
//! Every scenario is searched on both file systems a store meets ([`CLONES`]): one that shares
//! blocks, where the clone a store's open tries is made, and one that does not, where it is
//! refused.
//!
//! `STOREMODEL_EXECUTIONS`, `STOREMODEL_STEPS` and `STOREMODEL_CRASH_STATES` raise the bounds,
//! `STOREMODEL_RANDOM=<runs>` with `STOREMODEL_SEED` samples instead of searching, and
//! `STOREMODEL_REPLAY` runs the schedule a failure printed.

use std::collections::BTreeSet;

use blaze_core::lock::Lock;
use blaze_core::{DIRECT_ALIGN, Done, Effect, FileName, Tag};

use crate::explore::{Bounds, Chooser, Explored, Failure, Memo, Report, Search, explore};
use crate::fs::{Clones, CrashFs, Disk, Persist};
use crate::kernel::{Asker, Dials, KernelScenario, Which, run_kernel};
use crate::properties;
use crate::store::{
    Model, Op, Script, Wait, damaged, key, must_survive, open, run_open, run_store,
};

const ORDERED: Model = Model {
    persist: Persist::Ordered,
    clones: Clones::Made,
    failures: 0,
    reorder_io: false,
    wait: Wait::Everything,
};

/// The two file systems every scenario is searched on, whatever its model says of clones.
const CLONES: [Clones; 2] = [Clones::Made, Clones::Refused];

fn on(name: &str, clones: Clones) -> String {
    match clones {
        Clones::Made => format!("{name}, on a file system that shares blocks"),
        Clones::Refused => format!("{name}, on a file system that refuses clones"),
    }
}

/// `search` of `run` on each file system of [`CLONES`]: the first failure, or the sum of what
/// they covered.
fn both(
    name: &str,
    model: Model,
    search: Search,
    run: &dyn Fn(&Model, &Bounds, &mut Chooser, &mut Memo) -> Result<(), Failure>,
) -> Result<Explored, Box<Report>> {
    let bounds = Bounds::from_env(&Bounds::default());
    let search = Search::from_env(search);
    let mut all = Explored { executions: 0, crash_checks: 0 };
    for clones in CLONES {
        let model = Model { clones, ..model };
        let explored = explore(&on(name, clones), &search, &bounds, &|chooser, memo| {
            run(&model, &bounds, chooser, memo)
        })?;
        all.executions += explored.executions;
        all.crash_checks += explored.crash_checks;
    }
    Ok(all)
}

fn store(
    name: &str,
    model: Model,
    search: Search,
    scripts: &[Script],
) -> Result<Explored, Box<Report>> {
    both(name, model, search, &|model, bounds, chooser, memo| {
        run_store(scripts, model, bounds, chooser, memo)
    })
}

fn kernel(
    name: &str,
    model: Model,
    search: Search,
    scenario: &KernelScenario,
) -> Result<Explored, Box<Report>> {
    both(name, model, search, &|model, bounds, chooser, memo| {
        run_kernel(scenario, model, bounds, chooser, memo)
    })
}

fn passes(result: Result<Explored, Box<Report>>) {
    match result {
        // The test's one line of output: how much it covered.
        #[expect(clippy::print_stderr, reason = "a model test says how much it covered")]
        Ok(explored) => eprintln!(
            "{} executions, {} crash states checked",
            explored.executions, explored.crash_checks
        ),
        Err(report) => panic!("{report}"),
    }
}

fn script(name: &'static str, ops: &[Op]) -> Script {
    Script { name, ops: ops.to_vec() }
}

fn put(key: u8, value: u8) -> Op {
    Op::Put { key, value }
}

/// How a build ends: it answers at once, durable or not; then what it left settles, kept for
/// reuse across a crash once it is durable, or said lost when the store stopped.
const BUILD_ENDS: [Op; 3] = [Op::Answer, Op::Flush, Op::Settle];

fn build(name: &'static str, puts: &[Op]) -> Script {
    script(name, &[puts, &BUILD_ENDS].concat())
}

/// How a snapshot ends: it waits for durability, which is what it is asked for, then answers.
const SNAPSHOT_ENDS: [Op; 2] = [Op::Flush, Op::AwaitAndAnswer];

const WORLDS: [Dials; 2] =
    [Dials { dial: 0, facts: [10, 11, 12, 13] }, Dials { dial: 1, facts: [10, 11, 12, 13] }];

fn asker(name: &'static str, world: usize, salt: u8) -> Asker {
    Asker { name, input: 7, world, salt, finds: false }
}

/// Two builds and a reader, every schedule, a crash after every IO step.
#[test]
fn a_restore_never_serves_a_value_no_writer_fully_wrote() {
    passes(store(
        "two builds and a reader",
        ORDERED,
        Search::Exhaustive,
        &[build("w0", &[put(1, 1)]), build("w1", &[put(2, 2)]), script("r", &[Op::Get { key: 1 }])],
    ));
}

/// After every crash the restore holds what was durable or says why it cannot; and a disk
/// damaged after a clean stop (a pack cut short, the head's bytes changed, a pack gone) is
/// refused naming why, never opened empty.
#[test]
fn a_restore_never_silently_starts_empty() {
    passes(store(
        "a build, then a second",
        ORDERED,
        Search::Exhaustive,
        &[build("w0", &[put(1, 1), put(2, 2)])],
    ));
    let (disk, survive, ledger) = stopped_cleanly(&ORDERED);
    let damages: [(&str, Box<dyn Fn(&mut Disk)>); 3] = [
        ("the pack cut short", Box::new(|disk: &mut Disk| cut(disk, "store/pack-000000"))),
        ("the head's last byte changed", Box::new(|disk: &mut Disk| flip(disk, "store/head"))),
        ("the pack gone", Box::new(|disk: &mut Disk| drop(disk.remove("store/pack-000000")))),
    ];
    for (what, damage) in &damages {
        let mut disk = disk.clone();
        damage(&mut disk);
        if let Err(failure) = damaged(&disk, &survive, &ledger, &ORDERED, &Bounds::default()) {
            panic!("{what}: \"{}\" fails: {}", failure.property, failure.what);
        }
    }
}

/// Finding: a store whose head is gone opens empty and ready, though its packs hold durable
/// values, and its next commit writes over them. See docs/v2/plan/design/storemodel.md.
#[test]
#[ignore = "finding 1 of the storemodel lane: a lost head opens an empty store and says nothing"]
fn a_store_whose_head_is_lost_says_so() {
    let (mut disk, survive, ledger) = stopped_cleanly(&ORDERED);
    disk.remove("store/head");
    if let Err(failure) = damaged(&disk, &survive, &ledger, &ORDERED, &Bounds::default()) {
        panic!("\"{}\" fails: {}", failure.property, failure.what);
    }
}

/// One write of two values, flushed, and the disk a clean stop left, on `model`'s file system.
fn stopped_cleanly(model: &Model) -> (Disk, BTreeSet<blaze_cas::Key>, crate::store::Ledger) {
    let bounds = Bounds::default();
    let mut fs = model.empty();
    let mut store = open(&mut fs, &bounds, "the first open").expect("it opens");
    let mut ledger = crate::store::Ledger::default();
    for n in 1..=2 {
        let bytes = crate::store::value(n);
        let result = store.put(key(n), blaze_cas::Kind::Query, &bytes);
        ledger.put("w0", key(n), blaze_cas::Kind::Query, &bytes, &result).expect("a first value");
    }
    let mut effects = Vec::new();
    store.flush(&mut effects);
    crate::store::drive(&mut fs, &mut store, &mut effects, bounds.steps, "the flush")
        .expect("it ends");
    let survive = must_survive(&store, &ledger);
    assert_eq!(survive.len(), 2, "both puts are durable");
    (fs.synced_disk(), survive, ledger)
}

fn cut(disk: &mut Disk, file: &str) {
    let bytes = disk.get_mut(file).unwrap_or_else(|| panic!("{file} is on the disk"));
    bytes.truncate(bytes.len() / 2);
}

fn flip(disk: &mut Disk, file: &str) {
    let bytes = disk.get_mut(file).unwrap_or_else(|| panic!("{file} is on the disk"));
    *bytes.last_mut().expect("a head is not empty") ^= 1;
}

/// Two writers put two values under one key, with an IO failure anywhere; and two kernel gets
/// of an impure step put two values under one identity and reads.
#[test]
fn two_values_under_one_identity_are_refused_never_overwritten() {
    passes(store(
        "two writers, one key",
        Model { failures: 1, ..ORDERED },
        Search::Exhaustive,
        &[build("w0", &[put(1, 1)]), build("w1", &[put(1, 2)])],
    ));
    passes(kernel(
        "two salted gets, one identity",
        ORDERED,
        Search::Exhaustive,
        &KernelScenario {
            which: Which::Salted,
            worlds: WORLDS.to_vec(),
            askers: vec![asker("k0", 0, 1), asker("k1", 0, 2)],
        },
    ));
}

/// The invariant: an answer may be sent on results not yet durable; nothing derived from that
/// answer is reused across a crash unless its tickets were durable. After any crash the
/// restore serves no value whose bytes were lost, holds everything the store called durable
/// and everything an answered build kept, and every answer's settling ended kept or said lost.
///
/// Two builds on shared and separate keys with a reader, and a snapshot beside a build, each
/// build answering before it flushes: every schedule, a crash after every IO step, so at
/// every point between an answer sent and its results durable; with an IO failure anywhere,
/// so a settling that ends with the store stopped. Then two kernel gets of one identity in
/// two worlds racing for one shape slot, each answering at once and keeping what it left once
/// it and every store before it is durable, with an IO failure anywhere and effects completing
/// in any order.
#[test]
fn what_is_kept_after_an_answer_survives_any_crash_and_no_lost_value_is_served() {
    passes(store(
        "two builds that answer before they flush, and a reader",
        Model { failures: 1, ..ORDERED },
        Search::Exhaustive,
        &[build("w0", &[put(1, 1)]), build("w1", &[put(1, 1), put(2, 2)]), script("r", &[Op::Get { key: 2 }])],
    ));
    passes(store(
        "a build that answers before it flushes, and a snapshot that waits",
        Model { failures: 1, ..ORDERED },
        Search::Exhaustive,
        &[build("w0", &[put(1, 1)]), script("s", &[&[put(2, 2)][..], &SNAPSHOT_ENDS].concat())],
    ));
    passes(kernel(
        "two gets race for one shape slot",
        Model { failures: 1, reorder_io: true, ..ORDERED },
        Search::Exhaustive,
        &KernelScenario {
            which: Which::Dialed,
            worlds: WORLDS.to_vec(),
            askers: vec![asker("k0", 0, 0), asker("k1", 1, 0)],
        },
    ));
}

/// Finding: what a build left, kept once its own puts are durable, can rest on a value
/// another build put and has not yet made durable (a kernel hit, or a shape another get
/// already kept), so a crash loses what was kept. What is kept waits for everything
/// ([`Wait::Everything`]); the answer waits for nothing either way.
#[test]
#[ignore = "finding 3 of the storemodel lane: a hit carries no ticket, so waiting on a build's own puts is not enough"]
fn a_build_that_waits_on_its_own_puts_alone_keeps_what_it_answered() {
    passes(kernel(
        "two gets of one identity, each waiting on its own puts",
        Model { wait: Wait::Own, ..ORDERED },
        Search::Exhaustive,
        &KernelScenario {
            which: Which::Dialed,
            worlds: WORLDS.to_vec(),
            askers: vec![asker("k0", 0, 0), asker("k1", 0, 0)],
        },
    ));
}

/// After any crash in two racing gets and a lookup, the restore, built again, holds what a
/// fresh build holds, value for value, and so does a clean restart of it.
#[test]
fn the_restore_after_any_crash_equals_a_fresh_build() {
    passes(kernel(
        "two gets and a lookup",
        ORDERED,
        Search::Exhaustive,
        &KernelScenario {
            which: Which::Dialed,
            worlds: WORLDS.to_vec(),
            askers: vec![
                asker("k0", 0, 0),
                asker("k1", 1, 0),
                Asker { finds: true, ..asker("f", 0, 0) },
            ],
        },
    ));
}

/// Two flushes from one build and a second build, with two IO failures anywhere: every wait for
/// durability ends, and so does every restore's open.
#[test]
fn every_wait_ends() {
    passes(store(
        "a build that flushes twice, and another",
        Model { failures: 2, ..ORDERED },
        Search::Exhaustive,
        &[
            script("w0", &[put(1, 1), Op::Flush, put(2, 2), Op::Flush, Op::AwaitAndAnswer]),
            build("w1", &[put(3, 3)]),
        ],
    ));
}

/// Bigger scenarios than an exhaustive search finishes, sampled.
#[test]
fn larger_scenarios_sampled_keep_every_property() {
    let model = Model { failures: 3, reorder_io: true, ..ORDERED };
    passes(store(
        "three builds on shared keys",
        model,
        Search::Random { seed: 0, runs: 300 },
        &[
            build("w0", &[put(1, 1), put(2, 2)]),
            build("w1", &[put(2, 2), put(3, 3)]),
            build("w2", &[put(1, 9), put(4, 4)]),
            script("r", &[Op::Get { key: 1 }, Op::Get { key: 2 }]),
        ],
    ));
    passes(kernel(
        "three gets and two lookups",
        model,
        Search::Random { seed: 0, runs: 200 },
        &KernelScenario {
            which: Which::Dialed,
            worlds: WORLDS.to_vec(),
            askers: vec![
                asker("k0", 0, 0),
                asker("k1", 1, 0),
                asker("k2", 0, 0),
                Asker { finds: true, ..asker("f0", 0, 0) },
                Asker { finds: true, ..asker("f1", 1, 0) },
            ],
        },
    ));
}

/// Finding: under the letter of POSIX, where a directory entry is durable only once its
/// directory is synced and a crash keeps any subset of the rest, a new pack's entry may be
/// lost while the head that names it survives.
#[test]
#[ignore = "finding 2 of the storemodel lane: a new pack's directory entry is never synced before the head names it"]
fn a_restore_on_a_posix_file_system_opens() {
    passes(store(
        "one build, POSIX entries",
        Model { persist: Persist::Posix, ..ORDERED },
        Search::Exhaustive,
        &[build("w0", &[put(1, 1)])],
    ));
}

/// The model's own check (rule 23): an answer given before its store is durable is no bug
/// now, and a crash that loses what it answered with is no failure, as long as the loss or
/// the keeping is said once its settling ends.
#[test]
fn an_answer_given_before_its_store_is_durable_is_allowed_while_it_settles() {
    passes(store(
        "an answer before the store",
        Model { failures: 1, ..ORDERED },
        Search::Exhaustive,
        &[script("w0", &[put(1, 1), Op::Answer, Op::Flush, Op::Settle])],
    ));
}

/// The control of [`what_is_kept_after_an_answer_survives_any_crash_and_no_lost_value_is_served`]:
/// what a build answered with, kept for reuse before its tickets were durable, is caught at
/// the crash that loses it, with a schedule that says so.
#[test]
fn the_oracle_catches_a_kept_answer_relied_on_before_its_tickets_were_durable() {
    let result = store(
        "kept before the store",
        ORDERED,
        Search::Exhaustive,
        &[script("w0", &[put(1, 1), Op::Answer, Op::KeepUnwaited, Op::Flush])],
    );
    let report = result.expect_err("the model finds what was kept and a crash loses");
    assert_eq!(report.property, properties::KEPT_SURVIVES, "{report}");
    assert!(report.crash.is_some(), "{report}");
    assert!(report.steps.iter().any(|it| it == "w0: answers"), "{report}");
}

/// The control of [`what_is_kept_after_an_answer_survives_any_crash_and_no_lost_value_is_served`]:
/// a settling that ends with the store stopped and says nothing is caught: the build answered,
/// and what it left was neither kept nor said lost.
#[test]
fn the_oracle_catches_a_settling_failure_dropped_silently() {
    let result = store(
        "a settling that says nothing",
        Model { failures: 1, ..ORDERED },
        Search::Exhaustive,
        &[script("w0", &[put(1, 1), Op::Answer, Op::Flush, Op::SettleSilently])],
    );
    let report = result.expect_err("the model finds the loss no one said");
    assert_eq!(report.property, properties::LOSS_IS_SAID, "{report}");
    assert!(report.steps.iter().any(|it| it == "w0: answers"), "{report}");
    assert!(report.steps.iter().any(|it| it.starts_with("io: fail ")), "{report}");
}

/// The file system model: an unsynced append may be lost or torn, a synced one is kept, and a
/// new file's entry is durable once its directory is synced.
#[test]
fn a_crash_keeps_what_the_file_system_model_says() {
    let file = FileName::new("d/f").expect("a name");
    let mut fs = CrashFs::new(Persist::Posix, Clones::Made);
    fs.submit(&mut vec![Effect::Append { file: file.clone(), bytes: vec![7; 10], tag: Tag(0) }]);
    fs.complete(0);
    let states = fs.crash_states(64).expect("few states");
    let kept: BTreeSet<Option<usize>> =
        states.iter().map(|it| it.disk.get("d/f").map(Vec::len)).collect();
    // Gone (its entry lost), empty, torn at 1, 5 and 9 bytes, or whole.
    assert_eq!(
        kept,
        BTreeSet::from([None, Some(0), Some(1), Some(5), Some(9), Some(10)]),
        "{states:?}"
    );
    fs.submit(&mut vec![Effect::Sync { file, tag: Tag(1) }]);
    fs.complete(0);
    let kept: BTreeSet<Option<usize>> = fs
        .crash_states(64)
        .expect("few states")
        .iter()
        .map(|it| it.disk.get("d/f").map(Vec::len))
        .collect();
    assert_eq!(kept, BTreeSet::from([None, Some(10)]), "synced data, its entry not yet");
    fs.submit(&mut vec![Effect::Write {
        file: FileName::new("d/g").expect("a name"),
        bytes: vec![1],
        tag: Tag(2),
    }]);
    fs.complete(0);
    let states = fs.crash_states(64).expect("few states");
    assert_eq!(states.len(), 1, "a write syncs its directory: {states:?}");
    assert_eq!(states[0].disk.get("d/f").map(Vec::len), Some(10));
}

fn name(text: &str) -> FileName {
    FileName::new(text).expect("a name")
}

/// Asks `fs` for `effect` and completes it.
fn ran(fs: &mut CrashFs, effect: Effect) -> Done {
    fs.submit(&mut vec![effect]);
    fs.complete(0).done
}

/// What each crash state holds under `file`: nothing, or its bytes.
fn kept_of(fs: &CrashFs, file: &str) -> BTreeSet<Option<Vec<u8>>> {
    fs.crash_states(4096).expect("few states").iter().map(|it| it.disk.get(file).cloned()).collect()
}

/// A store that stopped cleanly is opened again, its open's IO in every order with a failure
/// anywhere, and a crash after every step: on a file system that shares blocks and on one
/// that refuses clones. After each crash the store holds every property a restore holds and
/// decides how it places as its file system says, and its open ends with the files a clean
/// open ends with: the files of the clone an open tries, which a crash leaves behind, are
/// removed by the next open, never kept and never what stops it. The search has to have seen
/// a crash leave each of them: the try's file and its clone where clones are made, the file
/// alone where they are refused (rule 23: without a crash that leaves them, this checks
/// nothing of them).
#[test]
fn an_open_cut_short_by_a_crash_opens_again_and_leaves_nothing_of_its_try_behind() {
    for clones in CLONES {
        let model = Model { clones, failures: 1, reorder_io: true, ..ORDERED };
        let (disk, survive, ledger) = stopped_cleanly(&model);
        let left = Lock::new("storemodel.left", BTreeSet::new());
        let bounds = Bounds::from_env(&Bounds::default());
        passes(explore(
            &on("an open of a store that stopped cleanly", clones),
            &Search::from_env(Search::Exhaustive),
            &bounds,
            &|chooser, memo| {
                run_open(&disk, &ledger, &survive, &model, &bounds, chooser, memo, &left)
            },
        ));
        let left = left.lock();
        let want = match clones {
            Clones::Made => 2,
            Clones::Refused => 1,
        };
        assert_eq!(
            left.len(),
            want,
            "clones {clones:?}: the files crashes left that the stopped store did not have: {:?}",
            *left
        );
    }
}

/// The file system model: a whole-file write that waits for no sync leaves, at a crash, the
/// name as it was (here: no file), or the new file holding nothing, all of its bytes, or
/// blocks of them; a sync of the file makes its bytes durable and, under the letter of POSIX,
/// not its entry.
#[test]
fn a_crash_may_undo_or_tear_a_write_that_waited_for_no_sync() {
    let block = DIRECT_ALIGN as usize;
    let bytes: Vec<u8> = (0..2 * block).map(|it| (it % 251) as u8 + 1).collect();
    let mut fs = CrashFs::new(Persist::Posix, Clones::Made);
    let write = Effect::WriteUnsynced { file: name("d/f"), bytes: bytes.clone(), tag: Tag(0) };
    assert_eq!(ran(&mut fs, write), Done::Written);
    let kept = kept_of(&fs, "d/f");
    assert!(kept.contains(&None), "its entry is not durable: {kept:?}");
    assert!(kept.contains(&Some(Vec::new())), "its entry kept and none of its bytes: {kept:?}");
    assert!(kept.contains(&Some(bytes.clone())), "all of it kept: {kept:?}");
    assert!(kept.contains(&Some(bytes[..block].to_vec())), "its first block alone: {kept:?}");
    assert_eq!(ran(&mut fs, Effect::Sync { file: name("d/f"), tag: Tag(1) }), Done::Synced);
    assert_eq!(
        kept_of(&fs, "d/f"),
        BTreeSet::from([None, Some(bytes.clone())]),
        "synced bytes, an entry not yet durable"
    );
    // Over a file that is durable: a crash leaves the old file whole, or the new one.
    let mut fs = CrashFs::new(Persist::Posix, Clones::Made);
    let old = Effect::Write { file: name("d/f"), bytes: vec![9; 3], tag: Tag(0) };
    assert_eq!(ran(&mut fs, old), Done::Written);
    let write = Effect::WriteUnsynced { file: name("d/f"), bytes: vec![7; 5], tag: Tag(1) };
    assert_eq!(ran(&mut fs, write), Done::Written);
    assert_eq!(
        kept_of(&fs, "d/f"),
        BTreeSet::from([Some(vec![9; 3]), Some(Vec::new()), Some(vec![7; 5])]),
        "the old file, or the new one empty or whole (one block is written or not)"
    );
}

/// The file system model: where blocks are shared a clone is a second file holding its
/// source's bytes whole, under an entry a crash may lose; where they are not, every clone
/// fails saying the file system cannot share blocks, as a store's open reads it, and nothing
/// is made. A clone onto a name that is there, or of a file that is not, fails either way.
#[test]
fn a_clone_is_made_whole_or_refused_as_the_file_system_says() {
    let clone = |tag| Effect::Clone { from: name("d/a"), to: name("d/b"), tag: Tag(tag) };
    let mut fs = CrashFs::new(Persist::Posix, Clones::Made);
    assert!(matches!(ran(&mut fs, clone(0)), Done::Failed(_)), "a clone of no file");
    // The source's bytes are not synced: the clone holds them whole all the same.
    let source = Effect::Append { file: name("d/a"), bytes: vec![3; 10], tag: Tag(1) };
    assert_eq!(ran(&mut fs, source), Done::Appended);
    assert_eq!(ran(&mut fs, clone(2)), Done::Cloned);
    assert_eq!(
        kept_of(&fs, "d/b"),
        BTreeSet::from([None, Some(vec![3; 10])]),
        "no clone, or a whole one"
    );
    assert!(matches!(ran(&mut fs, clone(3)), Done::Failed(_)), "a clone onto a file that is there");

    let mut fs = CrashFs::new(Persist::Posix, Clones::Refused);
    let source = Effect::Append { file: name("d/a"), bytes: vec![3; 10], tag: Tag(1) };
    assert_eq!(ran(&mut fs, source), Done::Appended);
    match ran(&mut fs, clone(2)) {
        Done::Failed(error) => {
            assert!(error.0.contains("cannot share blocks"), "{}", error.0);
        }
        done => panic!("a clone on a file system that shares no blocks ended {done:?}"),
    }
    assert!(!fs.names().contains("d/b"), "a refused clone makes nothing");
    assert_eq!(kept_of(&fs, "d/b"), BTreeSet::from([None]));
}

/// The file system model: a barrier makes nothing durable, and orders: a crash that keeps
/// anything asked after it completed keeps all it covers, and a sync asked after it makes
/// what it covers durable. The control: without the barrier a crash keeps the later file with
/// the earlier one cut short.
#[test]
fn a_barrier_orders_what_it_covers_before_what_is_asked_after() {
    let whole = Some(vec![5u8; 10]);
    // Whether some crash keeps `d/q` with less than all of `d/p`.
    let out_of_order = |fs: &CrashFs| {
        fs.crash_states(4096)
            .expect("few states")
            .iter()
            .any(|it| it.disk.contains_key("d/q") && it.disk.get("d/p") != whole.as_ref())
    };
    for barrier in [true, false] {
        let mut fs = CrashFs::new(Persist::Ordered, Clones::Made);
        let first = Effect::Append { file: name("d/p"), bytes: vec![5; 10], tag: Tag(0) };
        assert_eq!(ran(&mut fs, first), Done::Appended);
        if barrier {
            assert_eq!(ran(&mut fs, Effect::Barrier { file: name("d/p"), tag: Tag(1) }), Done::Ordered);
            assert!(kept_of(&fs, "d/p").contains(&None), "a barrier makes nothing durable");
        }
        let second = Effect::Append { file: name("d/q"), bytes: vec![6; 4], tag: Tag(2) };
        assert_eq!(ran(&mut fs, second), Done::Appended);
        assert_eq!(out_of_order(&fs), !barrier, "barrier: {barrier}");
        assert_eq!(ran(&mut fs, Effect::Sync { file: name("d/q"), tag: Tag(3) }), Done::Synced);
        if barrier {
            assert_eq!(
                kept_of(&fs, "d/p"),
                BTreeSet::from([whole.clone()]),
                "a sync asked after the barrier completed makes what it covers durable"
            );
        }
    }
}
