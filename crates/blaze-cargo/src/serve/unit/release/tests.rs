use blaze_graph::admission::Pressure;
use blaze_mem::Tag;
use blaze_query::db::MemoryUsage;

use super::{EDIT_CRATES, Finished, Holders, Plan, Release, Rows, Said};

/// A crate is finished once every unit of it compiled, never before, and once; a unit is open
/// from its first task to its compile's end, and a build script's run (no crate) or a unit
/// told twice changes nothing it shouldn't.
#[test]
fn a_crate_is_finished_once_its_last_unit_compiled_and_open_units_are_counted() {
    // Units 0 and 1 of crate 7 (its lib and its tests), 2 of crate 9, 3 a build script's run.
    let mut finished = Finished::new(vec![Some(7u32), Some(7), Some(9), None]);
    assert!(finished.idle() && !finished.has_ready());
    finished.began(0);
    finished.began(2);
    finished.began(2);
    assert!(!finished.idle());
    finished.compiled(0);
    assert!(!finished.has_ready(), "crate 7 still has unit 1 to compile");
    finished.compiled(2);
    assert_eq!(finished.take(), vec![9]);
    assert!(finished.idle(), "every unit that began has compiled");
    finished.began(3);
    assert!(finished.idle(), "a build script's run never opens: no compile ends it");
    finished.compiled(3);
    assert!(finished.idle() && !finished.has_ready(), "a run has no crate");
    finished.compiled(1);
    assert_eq!(finished.take(), vec![7], "a unit that compiled without a begun task counts too");
    finished.compiled(1);
    finished.compiled(0);
    finished.compiled(9);
    assert!(finished.take().is_empty(), "each crate once; a unit past the build is none");
    assert!(finished.idle());
}

/// The decisions the job loop acts on: a release is due once no unit is open and a crate is
/// finished, or once memory is short; the MIR goes only then, never while a unit is open and
/// memory is fine; a check releases only when memory is short, and then names no crate; a
/// release taken is not due again.
#[test]
fn a_release_is_due_when_no_unit_is_open_or_memory_is_short_and_takes_the_mir_only_then() {
    const GIB: u64 = 1 << 30;
    let fine = || Pressure { held: GIB, limit: 48 * GIB, over: None };
    let short = || Pressure { held: 47 * GIB, limit: 48 * GIB, over: Some(Tag::Mir) };
    // Units 0 and 1 of crate 7, 2 of crate 9.
    let mut release = Release::of(Finished::new(vec![Some(7u32), Some(7), Some(9)]), true);
    release.began(0);
    release.began(2);
    release.compiled_under(2, fine);
    assert!(!release.due(), "crate 9 is finished but unit 0 is open and memory is fine");
    release.began(1);
    release.compiled_under(0, short);
    assert!(release.due(), "memory is short");
    assert_eq!(
        release.plan(),
        Some(Plan { crates: vec![9], mir: true, pressed: true, ended: false })
    );
    assert!(!release.due() && release.plan().is_none(), "taken");
    release.compiled_under(1, fine);
    assert!(release.due(), "no unit is open and crate 7 is finished");
    assert_eq!(
        release.plan(),
        Some(Plan { crates: vec![7], mir: true, pressed: false, ended: false })
    );

    let mut open = Release::of(Finished::new(vec![Some(7u32), Some(9)]), true);
    open.began(0);
    open.began(1);
    open.compiled_under(1, fine);
    assert_eq!(
        open.plan(),
        Some(Plan { crates: vec![9], mir: false, pressed: false, ended: false }),
        "unit 0 still reads MIR"
    );

    let mut check = Release::of(Finished::new(vec![Some(7u32), Some(9)]), false);
    check.began(0);
    check.began(1);
    check.compiled_under(0, fine);
    assert!(
        !check.due() && check.plan().is_none(),
        "a check lets go of nothing while memory is fine"
    );
    check.compiled_under(1, fine);
    assert!(
        !check.due() && check.plan().is_none(),
        "nor once no unit is open: the next check reads its inference"
    );
    let mut pressed = Release::of(Finished::new(vec![Some(7u32), Some(9)]), false);
    pressed.began(0);
    pressed.began(1);
    pressed.compiled_under(0, short);
    assert!(pressed.due(), "memory is short in a check too");
    assert_eq!(
        pressed.plan(),
        Some(Plan { crates: vec![], mir: true, pressed: true, ended: false }),
        "a check's release under pressure lets go of every MIR and sweeps, but of no crate's inference by name"
    );
    assert!(!pressed.due() && pressed.plan().is_none(), "taken");
    let mut ended = Release::of(Finished::new(vec![Some(7u32), Some(9)]), false);
    ended.began(0);
    ended.compiled_under(0, fine);
    assert!(!ended.due(), "a check under way lets go of nothing while memory is fine");
    ended.end();
    assert!(ended.due(), "a check's end lets go of its MIR, under no pressure");
    assert_eq!(
        ended.plan(),
        Some(Plan { crates: vec![], mir: true, pressed: false, ended: true }),
        "every MIR, no crate's inference, no sweep: the next check reads its inference and verifies its MIR's memos"
    );
    assert!(!ended.due() && ended.plan().is_none(), "taken");
    let mut built = Release::of(Finished::new(vec![Some(7u32), Some(9)]), true);
    built.began(0);
    built.began(1);
    built.compiled_under(1, fine);
    assert_eq!(
        built.plan(),
        Some(Plan { crates: vec![9], mir: false, pressed: false, ended: false })
    );
    built.end();
    assert_eq!(
        built.plan(),
        Some(Plan { crates: vec![], mir: true, pressed: false, ended: true }),
        "a build's end lets go of every MIR though a unit (one that panicked) is still open"
    );
    let mut held_back = Release::of(Finished::new(vec![Some(7u32)]), false);
    held_back.began(0);
    held_back.short_under(short);
    assert_eq!(
        held_back.plan(),
        Some(Plan { crates: vec![], mir: true, pressed: true, ended: false }),
        "the plane holding a task back for memory makes it due too"
    );
}

/// Each release says one line naming why it ran, what went, what blaze held before and after,
/// and the limit, so a run's log shows every release (B5, coord-105 showed none), then what the
/// query storage holds: its largest holders on a release memory made due, its total on another.
#[test]
fn a_release_says_why_what_went_and_the_memory_against_the_limit() {
    const GIB: u64 = 1 << 30;
    let dropped = hir::evict::Dropped {
        bodies: 40,
        inference: 30,
        mir: 900,
        const_eval: 7,
        solver_caches: 5,
        intern_tables: 3,
    };
    let before = Pressure { held: 44 * GIB, limit: 48 * GIB, over: Some(Tag::Mir) };
    let after = Pressure { held: 38 * GIB, limit: 48 * GIB, over: None };
    let unread = blaze_query::db::Unread::default();
    // A release memory made due says the storage's holders.
    let infer = MemoryUsage {
        name: "infer",
        rows: 2048,
        used: 1500,
        row_bytes: 1 << 28,
        value_bytes: 1 << 28,
    };
    let storage = Holders::of(vec![infer]);
    let said = Said {
        crates: 2,
        held: 0,
        mir: true,
        pressed: true,
        ended: false,
        dropped,
        unread,
        before,
        after,
        storage,
    }
    .to_string();
    assert_eq!(
        said,
        "front end release (memory: blaze is past the limit's line of 42.00 GiB): let go of 30 inference results of 40 \
         bodies in 2 crates, 900 MIR bodies, 7 constant values and 5 trait solver caches, and swept the front end's \
         values to their fair shares; blaze held 44.00 GiB, now 38.00 GiB (6.00 freed), of a limit of 48.00 GiB; the \
         query storage holds 0.50 GiB: infer 0.50 (2.0k rows made, 1.5k used)"
    );
    let early = Pressure { held: 20 * GIB, ..before };
    let storage = Holders::of(Vec::new());
    let said = Said {
        crates: 1,
        held: 0,
        mir: false,
        pressed: true,
        ended: false,
        dropped,
        unread,
        before: early,
        after: early,
        storage,
    }
    .to_string();
    assert!(said.starts_with("front end release (memory: mir is past its share of "), "{said}");
    assert!(
        !said.contains("MIR bodies") && said.contains("in 1 crates and 5 trait solver caches"),
        "{said}"
    );
    assert!(said.contains("(0.00 freed)"), "{said}");
    // A release memory did not make due says the storage's total alone, read from counters.
    let idle = Pressure { held: GIB, limit: u64::MAX, over: None };
    let storage = Holders::total(3 << 29);
    let said = Said {
        crates: 1,
        held: 0,
        mir: true,
        pressed: false,
        ended: false,
        dropped,
        unread,
        before: idle,
        after: idle,
        storage,
    }
    .to_string();
    assert!(said.starts_with("front end release (no unit is open)"), "{said}");
    assert!(said.ends_with(", under no limit; the query storage holds 1.50 GiB"), "{said}");
    assert!(!said.contains("did not read"), "{said}");
    // A build's end says what two builds left unread, when anything went.
    let said = Said {
        crates: 0,
        held: 0,
        mir: true,
        pressed: false,
        ended: true,
        dropped,
        unread: blaze_query::db::Unread { values: 12, bytes: 0 },
        before: idle,
        after: idle,
        storage: Holders::total(3 << 29),
    }
    .to_string();
    assert!(said.starts_with("front end release (the build is over)"), "{said}");
    assert!(
        said.contains("trait solver caches, and 12 values the last two builds did not read; "),
        "{said}"
    );
    assert!(!said.contains("holding the inference"), "no workspace crate is held: {said}");
    // A build that holds workspace crates' inference for the next edit says how many.
    let said = Said {
        crates: 3,
        held: 2,
        mir: true,
        pressed: false,
        ended: true,
        dropped,
        unread,
        before: idle,
        after: idle,
        storage: Holders::total(3 << 29),
    }
    .to_string();
    assert!(
        said.contains(
            "trait solver caches, holding the inference of the 2 workspace crates finished last \
             for the next edit; "
        ),
        "{said}"
    );
}

/// A release's line ends with what the query storage holds and its eight largest holders,
/// most held first (a tie by name), each with its rows made and, where fewer, its rows in use,
/// so a function's pages made for keys it never ran read apart (memcap6).
#[test]
fn the_query_storage_s_largest_holders_are_said_with_their_rows_made_and_used() {
    let usage = |name: &'static str, rows, used, row_bytes, value_bytes| MemoryUsage {
        name,
        rows,
        used,
        row_bytes,
        value_bytes,
    };
    let holders = Holders::of(vec![
        usage("c", 1, 1, 1 << 20, 0),
        usage("source_place", 3_190_784, 400_000, 1 << 29, 0),
        usage("captured reads", 0, 0, 1 << 25, 0),
        usage("b", 1024, 1024, 1 << 23, 0),
        usage("dependency nodes", 4_000_000, 4_000_000, 3 << 26, 0),
        usage("infer", 2048, 1500, 1 << 28, 1 << 28),
        usage("a", 1024, 3, 1 << 24, 0),
        usage("dependency edges", 10_000_000, 10_000_000, 3 << 29, 0),
        usage("Crate", 950, 950, 1 << 26, 0),
    ]);
    assert_eq!(holders.total, 3_012_558_848, "every holder, the ninth not said included");
    assert_eq!(
        holders.to_string(),
        "the query storage holds 2.81 GiB: dependency edges 1.50 (10.00M rows), infer 0.50 (2.0k rows made, 1.5k used), \
         source_place 0.50 (3.19M rows made, 400.0k used), dependency nodes 0.19 (4.00M rows), Crate 0.06 (950 rows), \
         captured reads 0.03 (0 rows), a 0.02 (1.0k rows made, 3 used), b 0.01 (1.0k rows)"
    );
}

/// A count of rows is said in the unit it rounds to: one that rounds up past a unit's last
/// value says the next unit (999,950 rows are `1.00M`, never `1000.0k`).
#[test]
fn a_count_of_rows_is_said_in_the_unit_it_rounds_to() {
    let said = |rows| Rows(rows).to_string();
    assert_eq!(said(999), "999");
    assert_eq!(said(1_000), "1.0k");
    assert_eq!(said(1_049), "1.0k");
    assert_eq!(said(1_050), "1.1k");
    assert_eq!(said(999_949), "999.9k");
    assert_eq!(said(999_950), "1.00M");
    assert_eq!(said(999_999), "1.00M");
    assert_eq!(said(3_190_784), "3.19M");
    assert_eq!(said(9_995_000), "10.00M");
}

/// A release is made due by what this build made. A crate whose units were all
/// found kept as they began (a warm build's dependencies, every one of them before its first
/// task) is finished and makes none due; it goes with the next release, a made crate's or the
/// build's end's. The build here makes more crates than an edit does ([`EDIT_CRATES`]), so it
/// releases as a cold build does once it is past that many. The control: a build that found
/// no unit kept makes a release due as its first crate finishes.
#[test]
fn a_crate_whose_units_were_all_found_kept_makes_no_release_due_and_goes_with_the_next() {
    const GIB: u64 = 1 << 30;
    let fine = || Pressure { held: GIB, limit: 48 * GIB, over: None };
    // Units 0 and 1 of crate 7, a dependency found kept; units 2.. of crates 10.., made.
    let made_crates = EDIT_CRATES as u32 + 1;
    let mut of = vec![Some(7u32), Some(7)];
    of.extend((0..made_crates).map(|at| Some(10 + at)));
    let mut release = Release::of(Finished::new(of), true);
    for unit in [0, 1] {
        release.began(unit);
        release.kept(unit);
        release.compiled_under(unit, fine);
    }
    assert!(!release.due(), "no unit is open, and the one crate finished was found kept");
    release.began(2);
    assert!(!release.due(), "a unit is open");
    for unit in 2..2 + EDIT_CRATES {
        release.began(unit);
        release.compiled_under(unit, fine);
        assert!(!release.due(), "so far an edit's build: what it made stays");
        assert!(release.plan().is_none());
    }
    let last = 2 + EDIT_CRATES;
    release.began(last);
    release.compiled_under(last, fine);
    assert!(release.due(), "one crate more than an edit makes: a rebuild, and no unit is open");
    let mut crates = vec![7];
    crates.extend((0..made_crates).map(|at| 10 + at));
    assert_eq!(
        release.plan(),
        Some(Plan { crates, mir: true, pressed: false, ended: false }),
        "every crate finished goes, the kept one and those made while the build was an edit's"
    );
    assert!(!release.due() && release.plan().is_none(), "taken");

    let mut cold = Release::of(Finished::new(vec![Some(7u32), Some(7), Some(9)]), true);
    cold.began(0);
    cold.compiled_under(0, fine);
    cold.began(1);
    cold.compiled_under(1, fine);
    assert!(cold.due(), "no unit was found kept: crate 7, made and finished, is due");
    assert_eq!(
        cold.plan(),
        Some(Plan { crates: vec![7], mir: true, pressed: false, ended: false })
    );

    // A build that found every unit kept has nothing due until its end, which lets go of
    // what the last two builds did not read and of nothing by name: it made nothing.
    let mut kept = Release::of(Finished::new(vec![Some(7u32), Some(9)]), true);
    for unit in [0, 1] {
        kept.began(unit);
        kept.kept(unit);
        kept.compiled_under(unit, fine);
    }
    assert!(!kept.due());
    kept.end();
    assert!(kept.due(), "the build's end");
    assert_eq!(
        kept.plan(),
        Some(Plan { crates: vec![], mir: false, pressed: false, ended: true })
    );
}

/// The invariant: a build that found units kept and made at most [`EDIT_CRATES`] crates (an
/// edit's) lets go of no crate's inference by name and of no MIR, while it runs or as it
/// ends, so the next edit's check reads the values of the crate it makes again and infers and
/// lowers only what changed; memory short takes everything, as of any build.
#[test]
fn an_edit_s_build_keeps_what_it_made_until_memory_is_short() {
    const GIB: u64 = 1 << 30;
    let fine = || Pressure { held: GIB, limit: 48 * GIB, over: None };
    let short = || Pressure { held: 47 * GIB, limit: 48 * GIB, over: Some(Tag::Mir) };
    let edit = |now: fn() -> Pressure| {
        // Units 0 and 1 of crate 7, found kept; unit 2 of crate 9, the crate edited.
        let mut release = Release::of(Finished::new(vec![Some(7u32), Some(7), Some(9)]), true);
        for unit in [0, 1] {
            release.began(unit);
            release.kept(unit);
            release.compiled_under(unit, fine);
        }
        release.began(2);
        release.compiled_under(2, now);
        release
    };
    let mut release = edit(fine);
    assert!(!release.due(), "the crate the edit made is finished, and nothing of it goes");
    assert!(release.plan().is_none());
    release.end();
    assert!(release.due(), "the build's end");
    assert_eq!(
        release.plan(),
        Some(Plan { crates: vec![], mir: false, pressed: false, ended: true }),
        "no crate's inference and no MIR: the end lets go of the unread and the solver's caches"
    );
    assert!(!release.due() && release.plan().is_none(), "taken");

    let mut pressed = edit(short);
    assert!(pressed.due(), "memory is short");
    assert_eq!(
        pressed.plan(),
        Some(Plan { crates: vec![7, 9], mir: true, pressed: true, ended: false }),
        "memory takes an edit's build's values as any build's"
    );
}

/// The control of [`an_edit_s_build_keeps_what_it_made_until_memory_is_short`]: the same
/// build with no unit found kept (the planted bug's shape: a release that does not tell an
/// edit from a first build) lets go of the crate it made as it finishes and of every MIR, so
/// the oracle's plans are not those of every build.
#[test]
fn the_oracle_catches_a_release_that_lets_go_of_what_an_edit_made() {
    const GIB: u64 = 1 << 30;
    let fine = || Pressure { held: GIB, limit: 48 * GIB, over: None };
    let mut release = Release::of(Finished::new(vec![Some(7u32), Some(7), Some(9)]), true);
    for unit in [0, 1, 2] {
        release.began(unit);
        release.compiled_under(unit, fine);
    }
    assert!(release.due(), "a build that is no edit's is due as its crates finish");
    assert_eq!(
        release.plan(),
        Some(Plan { crates: vec![7, 9], mir: true, pressed: false, ended: false })
    );
    release.end();
    assert_eq!(
        release.plan(),
        Some(Plan { crates: vec![], mir: true, pressed: false, ended: true }),
        "and its end lets go of every MIR"
    );
}

/// The invariant: a build that compiles and is no edit's (a cold build, a rebuild of many
/// crates) lets go of a dependency's inference as the crate finishes and holds the inference
/// of the workspace's crates it finished last, at most [`EDIT_CRATES`], through its end, so
/// its end leaves what an edit's build's end leaves and the first edit after it infers only
/// what changed; a workspace crate goes once [`EDIT_CRATES`] finished after it, and memory
/// short takes the held ones with the rest. The MIR goes as before.
#[test]
fn a_build_that_is_no_edit_s_holds_the_inference_of_the_workspace_crates_it_finished_last() {
    const GIB: u64 = 1 << 30;
    let fine = || Pressure { held: GIB, limit: 48 * GIB, over: None };
    let short = || Pressure { held: 47 * GIB, limit: 48 * GIB, over: Some(Tag::Mir) };
    // Crates under 100 are dependencies; 100 and up are the workspace's.
    let member = |krate: &u32| *krate >= 100;
    // Unit 0 of dependency 7, then a unit each of workspace crates 100.., two more than held.
    let members = EDIT_CRATES as u32 + 2;
    let cold = || {
        let mut of = vec![Some(7u32)];
        of.extend((0..members).map(|at| Some(100 + at)));
        Release::of(Finished::new(of), true)
    };
    let mut release = cold();
    release.began(0);
    release.compiled_under(0, fine);
    assert!(release.due(), "no unit is open and a dependency this build made is finished");
    assert_eq!(
        release.plan_of(member),
        Some(Plan { crates: vec![7], mir: true, pressed: false, ended: false }),
        "a dependency's inference goes as it finishes"
    );
    assert!(release.held().is_empty());
    for at in 0..EDIT_CRATES {
        let unit = 1 + at;
        release.began(unit);
        release.compiled_under(unit, fine);
        assert!(release.due(), "no unit is open: the MIR goes");
        assert_eq!(
            release.plan_of(member),
            Some(Plan { crates: vec![], mir: true, pressed: false, ended: false }),
            "a workspace crate's inference is held; every MIR goes, as no unit is open"
        );
    }
    let first: Vec<u32> = (0..EDIT_CRATES as u32).map(|at| 100 + at).collect();
    assert_eq!(release.held(), &first[..], "the workspace crates finished, in their order");
    // One more: the first held goes, as `EDIT_CRATES` workspace crates finished after it.
    let unit = 1 + EDIT_CRATES;
    release.began(unit);
    release.compiled_under(unit, fine);
    assert_eq!(
        release.plan_of(member),
        Some(Plan { crates: vec![100], mir: true, pressed: false, ended: false }),
        "never more than `EDIT_CRATES` held: the bound on what the build holds more"
    );
    let last: Vec<u32> = (1..=EDIT_CRATES as u32).map(|at| 100 + at).collect();
    assert_eq!(release.held(), &last[..]);
    // The build's end: every MIR, and no held crate's inference.
    release.end();
    assert!(release.due(), "the build's end");
    assert_eq!(
        release.plan_of(member),
        Some(Plan { crates: vec![], mir: true, pressed: false, ended: true }),
        "the end lets go of every MIR and of no workspace crate held"
    );
    assert_eq!(release.held(), &last[..], "held past the build's end, for the next edit");
    assert!(!release.due() && release.plan_of(member).is_none(), "taken");

    // Memory short: the held crates go with what finished since, the held ones first.
    let mut pressed = cold();
    for unit in 0..3 {
        pressed.began(unit);
        pressed.compiled_under(unit, fine);
        assert!(pressed.plan_of(member).is_some());
    }
    assert_eq!(pressed.held(), &[100, 101][..]);
    pressed.began(3);
    pressed.compiled_under(3, short);
    assert!(pressed.due(), "memory is short");
    assert_eq!(
        pressed.plan_of(member),
        Some(Plan { crates: vec![100, 101, 102], mir: true, pressed: true, ended: false }),
        "memory takes the held crates' inference as any crate's"
    );
    assert!(pressed.held().is_empty(), "nothing is held once memory took it");

    // A check names no crate and holds none; an edit's build holds none: it names none.
    let mut check = Release::of(Finished::new(vec![Some(100u32)]), false);
    check.began(0);
    check.compiled_under(0, fine);
    check.end();
    assert_eq!(
        check.plan_of(member),
        Some(Plan { crates: vec![], mir: true, pressed: false, ended: true })
    );
    assert!(check.held().is_empty());
    let mut edit = Release::of(Finished::new(vec![Some(7u32), Some(100)]), true);
    edit.began(0);
    edit.kept(0);
    edit.compiled_under(0, fine);
    edit.began(1);
    edit.compiled_under(1, fine);
    edit.end();
    assert_eq!(
        edit.plan_of(member),
        Some(Plan { crates: vec![], mir: false, pressed: false, ended: true })
    );
    assert!(edit.held().is_empty(), "an edit's build lets go of nothing by name: nothing to hold");
}

/// The control of
/// [`a_build_that_is_no_edit_s_holds_the_inference_of_the_workspace_crates_it_finished_last`]:
/// the same build told that no crate is the workspace's (the planted bug's shape: a release
/// that does not tell a crate an edit can touch from a dependency, as every release was until
/// 2026-11-11) lets go of each crate's inference as it finishes and holds none, so the
/// oracle's plans are not those of every release.
#[test]
fn the_oracle_catches_a_release_that_holds_no_workspace_crate() {
    const GIB: u64 = 1 << 30;
    let fine = || Pressure { held: GIB, limit: 48 * GIB, over: None };
    let none = |_: &u32| false;
    let mut release = Release::of(Finished::new(vec![Some(7u32), Some(100), Some(101)]), true);
    for (unit, krate) in [(0, 7), (1, 100), (2, 101)] {
        release.began(unit);
        release.compiled_under(unit, fine);
        assert_eq!(
            release.plan_of(none),
            Some(Plan { crates: vec![krate], mir: true, pressed: false, ended: false }),
            "each crate's inference goes as it finishes"
        );
        assert!(release.held().is_empty());
    }
    release.end();
    assert_eq!(
        release.plan_of(none),
        Some(Plan { crates: vec![], mir: true, pressed: false, ended: true })
    );
    assert!(release.held().is_empty(), "nothing is left for the first edit");
}
