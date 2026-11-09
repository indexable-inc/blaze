//! Transcripts of the planner on a fake clock (AGENTS.md rule 14): roots appear, builds pin,
//! the clock advances, the budget is crossed; each test plays the driver's part by hand.

use crate::{
    Ledger,
    pace::{Levels, Pace, Volume},
    plan::{
        Checkout, Class, Collector, Config, Decision, Fate, Inventory, Keep, Object, Outcome,
        Phase, Reason, Stop, Swept, Trigger,
    },
};

const S: u64 = 1_000_000_000;
const HOUR: u64 = 3600 * S;
const DAY: u64 = 24 * HOUR;
/// The fake clock starts here: far enough from 0 that "long ago" is a time.
const T0: u64 = 1_000 * DAY;

/// Levels in bytes for a volume of 10 000: pressure starts below 1000 free and is full at 600.
pub(crate) fn levels() -> Levels {
    Levels {
        comfortable_percent: 0,
        comfortable_bytes: 1000,
        floor_percent: 0,
        floor_bytes: 600,
        share_percent: 100,
        hysteresis_permille: 50,
    }
}

/// The test volume with `free_bytes` free.
pub(crate) fn volume(free_bytes: u64) -> Volume {
    Volume { id: 1, total_bytes: 10_000, free_bytes }
}

pub(crate) fn config() -> Config {
    Config {
        areas: vec!["/h".to_owned()],
        levels: levels(),
        pace: Pace::default(),
        grace_ns: HOUR,
        ttl_ns: 14 * DAY,
        gone_grace_ns: HOUR,
        workspace_cap_bytes: 500,
        keep_transactions: 1,
        period_ns: 6 * HOUR,
        idle_after_ns: 300 * S,
        hold_ns: 120 * S,
        head_recent_ns: 6 * HOUR,
        fuel: 64,
        run_ns: 60 * S,
        trash_after_ns: 0,
    }
}

pub(crate) fn workspace(name: &str, bytes: u64, used_ns: u64, checkout_exists: bool) -> Object {
    Object {
        path: format!("/h/build/{name}"),
        area: 0,
        class: Class::Workspace,
        bytes,
        used_ns,
        workspace: None,
        checkout: Some(Checkout { path: format!("/src/{name}"), exists: checkout_exists }),
        leased: false,
        refused: None,
    }
}

pub(crate) fn profile(ws: &str, name: &str, bytes: u64, used_ns: u64) -> Object {
    Object {
        path: format!("/h/build/{ws}/{name}"),
        area: 0,
        class: Class::Profile,
        bytes,
        used_ns,
        workspace: Some(format!("/h/build/{ws}")),
        checkout: None,
        leased: false,
        refused: None,
    }
}

/// `objects` as a scan finds them, with a workspace used two hours ago beside them: the clock
/// check (a run is refused while no build dir was used within the TTL) passes.
pub(crate) fn inventory(mut objects: Vec<Object>) -> Inventory {
    objects.push(workspace("fresh", 0, T0 - 2 * HOUR, true));
    Inventory { objects, problems: Vec::new(), next: None }
}

/// A collector whose clock is at `T0`, with a run asked and its scan taken: the run's id.
fn asked(collector: &mut Collector, dry_run: bool) -> u64 {
    collector.advance_clock(T0);
    collector.ask(dry_run);
    match collector.take_decisions().as_slice() {
        [Decision::Scan { run, .. }] => *run,
        other => panic!("a run asked begins with a scan, not {other:?}"),
    }
}

fn trash_of(decisions: &[Decision]) -> Vec<String> {
    decisions
        .iter()
        .filter_map(|it| match it {
            Decision::Trash { path, .. } => Some(path.clone()),
            _ => None,
        })
        .collect()
}

/// Plays the driver: every rename moves, every unlink frees what it names; the run's report.
pub(crate) fn drive(collector: &mut Collector, mut decisions: Vec<Decision>) -> crate::RunReport {
    for round in 0..1000 {
        // The driver answers every decision at once and moves no clock: a batch with nothing in
        // it is a run that waits on what this driver never does (or a caller that took the
        // record away), never one that ends by going round again.
        assert!(
            !decisions.is_empty(),
            "the run asked for nothing and gave no record after {round} rounds of answers: it waits on a clock or a decision taken before `drive`"
        );
        for decision in decisions {
            match decision {
                Decision::Trash { run, path, .. } => collector.trashed(run, &path, Swept::Moved),
                Decision::Unlink { run, path } => collector.unlinked(run, &path, Ok(1)),
                Decision::Record { report } => return report,
                Decision::Scan { .. } => panic!("a second scan in one run"),
            }
        }
        decisions = collector.take_decisions();
    }
    panic!("the run did not end in 1000 rounds of answers");
}

fn fate<'a>(report: &'a crate::RunReport, path: &str) -> &'a Fate {
    &report
        .verdicts
        .iter()
        .find(|it| it.path == path)
        .unwrap_or_else(|| panic!("no verdict of {path}"))
        .fate
}

#[test]
fn a_workspace_whose_checkout_is_gone_is_trashed_then_unlinked() {
    let mut gc = Collector::new(config());
    let run = asked(&mut gc, false);
    let old = T0 - 2 * HOUR;
    gc.scanned(
        run,
        Ok(inventory(vec![workspace("gone", 10, old, false), profile("gone", "debug", 50, old)])),
    );
    let decisions = gc.take_decisions();
    assert_eq!(
        trash_of(&decisions),
        ["/h/build/gone"],
        "the workspace goes whole, its profile with it"
    );
    let Decision::Trash { to, .. } = &decisions[0] else { unreachable!() };
    assert!(to.starts_with("/h/.blaze-trash/"), "into its area's trash: {to}");
    let report = drive(&mut gc, decisions);
    assert_eq!(report.outcome, Outcome::Done);
    assert_eq!((report.swept_objects, report.swept_bytes), (2, 60));
    assert_eq!(report.unlinked_objects, 1, "its trash is unlinked in the same run");
    assert_eq!(
        *fate(&report, "/h/build/gone/debug"),
        Fate::Go(Reason::Within { dir: "/h/build/gone".to_owned() })
    );
}

/// The race: a build pins an object after the mark and before its sweep; it survives.
#[test]
fn an_object_pinned_after_the_mark_and_before_its_sweep_survives() {
    let mut gc = Collector::new(config());
    let run = asked(&mut gc, false);
    let old = T0 - 30 * DAY;
    gc.scanned(
        run,
        Ok(inventory(vec![workspace("a", 10, old, true), workspace("b", 10, old, true)])),
    );
    let first = gc.take_decisions();
    assert_eq!(trash_of(&first), ["/h/build/a"], "one rename at a time");
    // A build of b starts between the mark and b's sweep.
    let pin = gc.pin("/h/build/b/debug");
    for decision in first {
        let Decision::Trash { run, path, .. } = decision else { continue };
        gc.trashed(run, &path, Swept::Moved);
    }
    let rest = gc.take_decisions();
    assert!(trash_of(&rest).is_empty(), "nothing renames b: {rest:?}");
    let report = drive(&mut gc, rest);
    assert_eq!(*fate(&report, "/h/build/b"), Fate::Keep(Keep::Pinned));
    assert_eq!(*fate(&report, "/h/build/a"), Fate::Go(Reason::Unused { for_ns: 30 * DAY }));
    gc.unpin(pin);
}

/// A use without a root (a record touched, a transaction) after the mark keeps it as well.
#[test]
fn an_object_used_after_the_mark_survives() {
    let mut gc = Collector::new(config());
    let run = asked(&mut gc, false);
    let old = T0 - 30 * DAY;
    gc.scanned(
        run,
        Ok(inventory(vec![workspace("a", 10, old, true), workspace("b", 10, old, true)])),
    );
    let first = gc.take_decisions();
    gc.used("/h/build/b");
    let report = drive(&mut gc, first);
    assert_eq!(*fate(&report, "/h/build/b"), Fate::Keep(Keep::UsedAfterMark));
}

/// A use after the scan began, before it came back, keeps what the scan may have seen stale.
#[test]
fn a_use_during_the_scan_keeps_what_it_names() {
    let mut gc = Collector::new(config());
    let run = asked(&mut gc, false);
    gc.used("/h/build/a");
    gc.scanned(run, Ok(inventory(vec![workspace("a", 10, T0 - 30 * DAY, true)])));
    let report = finish(&mut gc);
    assert_eq!(*fate(&report, "/h/build/a"), Fate::Keep(Keep::UsedAfterMark));
}

/// Drives the run under way from the decisions it has asked for: its report.
fn finish(gc: &mut Collector) -> crate::RunReport {
    let decisions = gc.take_decisions();
    drive(gc, decisions)
}

#[test]
fn a_lease_held_at_the_sweep_keeps_it() {
    let mut gc = Collector::new(config());
    let run = asked(&mut gc, false);
    gc.scanned(run, Ok(inventory(vec![workspace("a", 10, T0 - 30 * DAY, true)])));
    let decisions = gc.take_decisions();
    let [Decision::Trash { run, path, .. }] = decisions.as_slice() else { panic!("{decisions:?}") };
    gc.trashed(*run, path, Swept::Leased);
    let report = finish(&mut gc);
    assert_eq!(*fate(&report, "/h/build/a"), Fate::Keep(Keep::LeasedAtSweep));
    assert_eq!(report.swept_objects, 0);
}

#[test]
fn heads_leases_and_refusals_keep_what_they_name() {
    let mut gc = Collector::new(config());
    gc.heads(&["/src/head".to_owned()]);
    let run = asked(&mut gc, false);
    let old = T0 - 30 * DAY;
    let mut leased = profile("busy", "debug", 1, old);
    leased.leased = true;
    let mut refused = workspace("odd", 1, old, false);
    refused.refused = Some("it has no record".to_owned());
    gc.scanned(
        run,
        Ok(inventory(vec![
            workspace("head", 1, old, true),
            workspace("busy", 1, old, true),
            leased,
            refused,
        ])),
    );
    let decisions = gc.take_decisions();
    assert!(trash_of(&decisions).is_empty(), "{decisions:?}");
    let report = drive(&mut gc, decisions);
    assert_eq!(
        *fate(&report, "/h/build/head"),
        Fate::Keep(Keep::Head { checkout: "/src/head".to_owned() })
    );
    assert_eq!(
        *fate(&report, "/h/build/busy"),
        Fate::Keep(Keep::Leased),
        "a lease below holds the dir it is in"
    );
    assert!(matches!(fate(&report, "/h/build/odd"), Fate::Keep(Keep::Refused { .. })));
}

#[test]
fn nothing_goes_within_its_grace() {
    let mut gc = Collector::new(config());
    let run = asked(&mut gc, false);
    gc.scanned(run, Ok(inventory(vec![workspace("new", 10, T0 - 60 * S, false)])));
    let report = finish(&mut gc);
    assert_eq!(*fate(&report, "/h/build/new"), Fate::Keep(Keep::Grace { left_ns: HOUR - 60 * S }));
}

#[test]
fn a_workspace_past_its_cap_loses_its_least_recently_used_profiles_first() {
    let mut gc = Collector::new(config());
    let run = asked(&mut gc, false);
    let ws = workspace("w", 0, T0 - HOUR, true);
    let old = profile("w", "release", 300, T0 - 10 * DAY);
    let mid = profile("w", "debug", 300, T0 - 5 * DAY);
    let new = profile("w", "doc", 100, T0 - 2 * HOUR);
    gc.scanned(run, Ok(inventory(vec![ws, old, mid, new])));
    let decisions = gc.take_decisions();
    assert_eq!(trash_of(&decisions), ["/h/build/w/release"]);
    let report = drive(&mut gc, decisions);
    assert_eq!(
        *fate(&report, "/h/build/w/release"),
        Fate::Go(Reason::OverCap { workspace: "/h/build/w".to_owned(), cap_bytes: 500 })
    );
    assert_eq!(*fate(&report, "/h/build/w/debug"), Fate::Keep(Keep::Fresh));
}

/// Free space below comfortable: the least recently used fresh profiles go until it would be
/// past the band above comfortable again; a workspace's last transaction yields last.
#[test]
fn below_comfortable_it_evicts_least_recently_used_first() {
    let mut gc = Collector::new(Config { workspace_cap_bytes: u64::MAX, ..config() });
    gc.transaction("/h/build/a", "/h/build/a/debug");
    // 820 free: 180 below comfortable, pressure 450; back past the band at 1020, 200 to free.
    gc.volume(0, volume(820));
    assert_eq!(gc.pressure(), 450);
    let run = asked(&mut gc, false);
    let objects = vec![
        workspace("a", 0, T0 - HOUR, true),
        profile("a", "debug", 400, T0 - 9 * DAY),
        workspace("b", 0, T0 - HOUR, true),
        profile("b", "debug", 400, T0 - 3 * DAY),
        profile("b", "release", 400, T0 - 2 * DAY),
    ];
    gc.scanned(run, Ok(inventory(objects)));
    let report = finish(&mut gc);
    // b's older profile frees the 200; the newer stays, and a's, its last transaction, as well.
    assert_eq!(
        *fate(&report, "/h/build/b/debug"),
        Fate::Go(Reason::OverBudget { to_free_bytes: 200 })
    );
    assert_eq!(*fate(&report, "/h/build/b/release"), Fate::Keep(Keep::Fresh));
    assert_eq!(*fate(&report, "/h/build/a/debug"), Fate::Keep(Keep::Transaction));
    assert_eq!(report.over_budget_bytes, 0);
    assert_eq!(report.volumes.len(), 1);
    // What the driver's unlink freed is not credited: the measure after the run says (C11).
    assert_eq!(report.volumes[0].free_bytes, 820);
}

/// A live head keeps its workspace's build dir and the profiles its last build used, even
/// when older than a profile of a checkout no head builds (review C4); a profile of the head
/// that its last build did not use, and that was not used within `head_recent_ns`, yields to
/// pressure like any other, least recently used first (review N2): a daemon that watches a
/// checkout for weeks does not keep everything it ever built there.
#[test]
fn a_live_heads_long_unused_profiles_yield_to_pressure_but_its_last_builds_stay() {
    let mut gc = Collector::new(Config { workspace_cap_bytes: u64::MAX, ..config() });
    gc.heads(&["/src/a".to_owned()]);
    gc.volume(0, volume(820));
    let run = asked(&mut gc, false);
    let checkout = |name: &str| Some(Checkout { path: format!("/src/{name}"), exists: true });
    // `a`'s last build, two days ago, wrote into `debug`; `release` was last built nine days ago.
    let last_build = T0 - 2 * DAY;
    let a_debug = Object { checkout: checkout("a"), ..profile("a", "debug", 400, last_build) };
    let a_release =
        Object { checkout: checkout("a"), ..profile("a", "release", 100, T0 - 9 * DAY) };
    let b_debug = Object { checkout: checkout("b"), ..profile("b", "debug", 400, T0 - DAY) };
    let objects = vec![
        workspace("a", 0, last_build, true),
        a_debug,
        a_release,
        workspace("b", 0, T0 - HOUR, true),
        b_debug,
    ];
    gc.scanned(run, Ok(inventory(objects)));
    let report = finish(&mut gc);
    let head = Fate::Keep(Keep::Head { checkout: "/src/a".to_owned() });
    assert_eq!(*fate(&report, "/h/build/a"), head);
    assert_eq!(*fate(&report, "/h/build/a/debug"), head);
    assert!(
        matches!(fate(&report, "/h/build/a/release"), Fate::Go(Reason::OverBudget { .. })),
        "{:?}",
        fate(&report, "/h/build/a/release")
    );
    // What `release` freed was not enough: the next least recently used goes, and the head's
    // older last build does not.
    assert!(
        matches!(fate(&report, "/h/build/b/debug"), Fate::Go(Reason::OverBudget { .. })),
        "{:?}",
        fate(&report, "/h/build/b/debug")
    );
}

/// A head's profile used within `head_recent_ns` stays though its workspace's last build did
/// not use it (review N2).
#[test]
fn a_live_heads_recently_used_profile_stays_under_pressure() {
    let mut gc = Collector::new(Config { workspace_cap_bytes: u64::MAX, ..config() });
    gc.heads(&["/src/a".to_owned()]);
    gc.volume(0, volume(820));
    let run = asked(&mut gc, false);
    let checkout = Some(Checkout { path: "/src/a".to_owned(), exists: true });
    let a_check =
        Object { checkout: checkout.clone(), ..profile("a", "check", 400, T0 - 3 * HOUR) };
    let a_debug = Object { checkout, ..profile("a", "debug", 400, T0 - 2 * HOUR) };
    let objects = vec![workspace("a", 0, T0 - 2 * HOUR, true), a_check, a_debug];
    gc.scanned(run, Ok(inventory(objects)));
    let report = finish(&mut gc);
    let head = Fate::Keep(Keep::Head { checkout: "/src/a".to_owned() });
    assert_eq!(*fate(&report, "/h/build/a/check"), head);
    assert_eq!(*fate(&report, "/h/build/a/debug"), head);
}

/// A build dir with something refused inside it (a mount the driver found, which no removal
/// crosses) is never trashed whole, though its checkout is gone and it is long unused (S1).
#[test]
fn a_build_dir_with_a_mount_inside_is_never_trashed() {
    let mut gc = Collector::new(config());
    let run = asked(&mut gc, false);
    let old = T0 - 30 * DAY;
    let mounted = Object {
        refused: Some("/mnt/x is a mount point at or under it".to_owned()),
        ..profile("gone", "debug", 10, old)
    };
    gc.scanned(run, Ok(inventory(vec![workspace("gone", 10, old, false), mounted])));
    let decisions = gc.take_decisions();
    assert!(trash_of(&decisions).is_empty(), "{decisions:?}");
    let report = drive(&mut gc, decisions);
    assert!(matches!(fate(&report, "/h/build/gone"), Fate::Keep(Keep::Refused { .. })));
    assert!(matches!(fate(&report, "/h/build/gone/debug"), Fate::Keep(Keep::Refused { .. })));
}

/// A run whose scan failed never vouches for the clock: after a good run, the clock jumps a
/// month and a scan fails; a collector restored from the ledger, with no build dir to tell by,
/// is still refused against the good run's end, not the failed one's (review C6).
#[test]
fn a_failed_scan_after_the_clock_jumped_does_not_vouch_for_it() {
    let mut gc = Collector::new(config());
    let run = asked(&mut gc, false);
    gc.scanned(run, Ok(inventory(Vec::new())));
    assert!(finish(&mut gc).clock_checked);
    let later = T0 + 30 * DAY;
    gc.advance_clock(later);
    gc.ask(false);
    let run = match gc.take_decisions().as_slice() {
        [Decision::Scan { run, .. }] => *run,
        other => panic!("{other:?}"),
    };
    gc.scanned(run, Err("the disk went away".to_owned()));
    let failed = match gc.take_decisions().as_slice() {
        [Decision::Record { report }] => report.clone(),
        other => panic!("{other:?}"),
    };
    assert!(!failed.clock_checked);
    gc.ask(false);
    let run = match gc.take_decisions().as_slice() {
        [Decision::Scan { run, .. }] => *run,
        other => panic!("{other:?}"),
    };
    let store = Object {
        path: "/h/std/x".to_owned(),
        area: 0,
        class: Class::Std,
        bytes: 10,
        used_ns: T0 - DAY,
        workspace: None,
        checkout: None,
        leased: false,
        refused: None,
    };
    gc.scanned(run, Ok(Inventory { objects: vec![store], problems: Vec::new(), next: None }));
    let report = finish(&mut gc);
    assert_eq!(
        report.outcome,
        Outcome::Stopped {
            phase: Phase::Mark,
            why: Stop::Clock { now_ns: later, newest_ns: T0 - 2 * HOUR },
            done: 0,
            left: 1
        }
    );
    // Restored from the ledger, only the runs that passed the check tell the clock.
    let mut restored = Collector::new(config());
    restored.restore(&gc.ledger());
    restored.advance_clock(later);
    restored.ask(true);
    let run = match restored.take_decisions().as_slice() {
        [Decision::Scan { run, .. }] => *run,
        other => panic!("{other:?}"),
    };
    restored.scanned(run, Ok(Inventory { objects: Vec::new(), problems: Vec::new(), next: None }));
    let report = finish(&mut restored);
    assert!(matches!(
        report.outcome,
        Outcome::Stopped { why: Stop::Clock { newest_ns: T0, .. }, .. }
    ));
}

/// A wall clock set forward while the collector runs is held to what the boot clock says has
/// passed, and the next report says how far ahead it was.
#[test]
fn a_wall_clock_set_forward_is_held_to_the_boot_clock() {
    let mut gc = Collector::new(config());
    gc.advance_clocks(T0, Some(5 * S));
    gc.advance_clocks(T0 + 30 * DAY, Some(15 * S));
    assert_eq!(gc.now(), T0 + 10 * S + 300 * S);
    gc.ask(true);
    let run = match gc.take_decisions().as_slice() {
        [Decision::Scan { run, .. }] => *run,
        other => panic!("{other:?}"),
    };
    gc.scanned(run, Ok(inventory(Vec::new())));
    let report = finish(&mut gc);
    assert!(
        report.problems.iter().any(|it| it.contains("ahead of the boot clock")),
        "{:?}",
        report.problems
    );
}

/// The driver reads the collector's own clock (review N3): a wall clock stepped forward (a VM
/// paused, which its boot clock did not count, then NTP) does not put the run's deadline in
/// the past for the driver's gate, so no rename or unlink is cancelled for it; once the wall
/// clock has stayed as far ahead for `REANCHOR_NS` of boot time, the collector's clock is
/// re-anchored to it, and the report says so.
#[test]
fn a_wall_clock_step_does_not_cancel_a_runs_decisions_and_is_re_anchored_once_steady() {
    use std::sync::{Arc, atomic::AtomicU64};
    let mut gc = Collector::new(Config { run_ns: 600 * S, ..config() });
    gc.advance_clocks(T0, Some(5 * S));
    gc.ask(false);
    let run = match gc.take_decisions().as_slice() {
        [Decision::Scan { run, .. }] => *run,
        other => panic!("{other:?}"),
    };
    let deadline_ns = gc.run_deadline().expect("a run under way");
    let gate = crate::fs::Gate {
        uses: 0,
        deadline_ns,
        ended: Arc::new(AtomicU64::new(0)),
        pressure: 0,
        clock: gc.clock(),
    };
    // A day passes on the wall clock and one second on the boot clock.
    let (wall, boot) = (T0 + DAY + S, 6 * S);
    assert!(wall >= deadline_ns, "the raw wall clock is past the run's deadline");
    assert!(!gate.cancelled_at(run, wall, Some(boot)), "the gate reads the collector's clock");
    gc.advance_clocks(wall, Some(boot));
    assert!(gc.running(), "the run is not ended by the step");
    assert_eq!(gc.now(), gate.clock.read(wall, Some(boot)), "the collector and its driver agree");
    gc.scanned(run, Ok(inventory(Vec::new())));
    let report = finish(&mut gc);
    assert_eq!(report.outcome, Outcome::Done);
    assert!(
        report.problems.iter().any(|it| it.contains("ahead of the boot clock")),
        "{:?}",
        report.problems
    );
    // As far ahead for an hour of boot time: taken as right.
    let reanchor = crate::plan::REANCHOR_NS;
    gc.advance_clocks(wall + reanchor, Some(boot + reanchor));
    assert_eq!(gc.now(), wall + reanchor, "re-anchored to the wall clock");
    assert_eq!(
        gc.clock().read(wall + reanchor + S, Some(boot + reanchor + S)),
        wall + reanchor + S
    );
    gc.ask(true);
    let run = match gc.take_decisions().as_slice() {
        [Decision::Scan { run, .. }] => *run,
        other => panic!("{other:?}"),
    };
    gc.scanned(run, Ok(inventory(vec![workspace("now", 0, wall, true)])));
    let report = finish(&mut gc);
    assert!(report.problems.iter().any(|it| it.contains("re-anchored")), "{:?}", report.problems);
}

/// A workspace whose checkout is gone keeps its build dir while a profile in it is within the
/// grace (C6): the dir would go whole, and the profile with it.
#[test]
fn a_workspace_with_a_child_in_its_grace_is_not_swept() {
    let mut gc = Collector::new(config());
    let run = asked(&mut gc, false);
    let objects =
        vec![workspace("a", 10, T0 - 2 * DAY, false), profile("a", "debug", 10, T0 - 10 * 60 * S)];
    gc.scanned(run, Ok(inventory(objects)));
    // The run's decisions, kept: with nothing to rename, they hold its record.
    let decisions = gc.take_decisions();
    assert!(trash_of(&decisions).is_empty(), "nothing is renamed: {decisions:?}");
    let report = drive(&mut gc, decisions);
    assert!(matches!(fate(&report, "/h/build/a"), Fate::Keep(_)));
    assert!(matches!(fate(&report, "/h/build/a/debug"), Fate::Keep(Keep::Grace { .. })));
    assert_eq!(report.swept_objects, 0);
}

#[test]
fn a_run_out_of_fuel_stops_saying_how_far_it_got() {
    let mut gc = Collector::new(Config { fuel: 1, ..config() });
    let run = asked(&mut gc, false);
    let old = T0 - 30 * DAY;
    gc.scanned(
        run,
        Ok(inventory(vec![workspace("a", 10, old, true), workspace("b", 10, old, true)])),
    );
    let report = finish(&mut gc);
    assert_eq!(
        report.outcome,
        Outcome::Stopped { phase: Phase::Sweep, why: Stop::Fuel { fuel: 1 }, done: 1, left: 1 }
    );
    assert_eq!(*fate(&report, "/h/build/b"), Fate::Keep(Keep::NotReached));
}

#[test]
fn a_run_past_its_deadline_stops() {
    let mut gc = Collector::new(config());
    let run = asked(&mut gc, false);
    gc.advance_clock(T0 + 61 * S);
    let report = match gc.take_decisions().as_slice() {
        [Decision::Record { report }] => report.clone(),
        other => panic!("{other:?}"),
    };
    assert!(matches!(
        report.outcome,
        Outcome::Stopped { phase: Phase::Scan, why: Stop::Deadline { .. }, .. }
    ));
    // The scan's late answer is of a run that ended: dropped.
    gc.scanned(run, Ok(inventory(vec![workspace("a", 10, 0, false)])));
    assert!(gc.take_decisions().is_empty());
}

/// A rename whose answer never comes does not hold the run open: it ends at its deadline, and
/// the late answer is dropped.
#[test]
fn a_run_whose_answer_never_comes_ends_at_its_deadline() {
    let mut gc = Collector::new(config());
    let run = asked(&mut gc, false);
    gc.scanned(run, Ok(inventory(vec![workspace("a", 10, T0 - 30 * DAY, true)])));
    assert_eq!(trash_of(&gc.take_decisions()), ["/h/build/a"]);
    gc.advance_clock(T0 + 61 * S);
    let report = match gc.take_decisions().as_slice() {
        [Decision::Record { report }] => report.clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(
        report.outcome,
        Outcome::Stopped {
            phase: Phase::Sweep,
            why: Stop::Deadline { run_ns: 60 * S },
            done: 0,
            left: 1
        }
    );
    assert_eq!(*fate(&report, "/h/build/a"), Fate::Keep(Keep::NotReached));
    gc.trashed(run, "/h/build/a", Swept::Moved);
    assert!(gc.take_decisions().is_empty() && !gc.running());
}

#[test]
fn a_dry_run_says_what_would_go_and_asks_for_nothing() {
    let mut gc = Collector::new(config());
    let run = asked(&mut gc, true);
    gc.scanned(run, Ok(inventory(vec![workspace("gone", 10, T0 - 2 * HOUR, false)])));
    let decisions = gc.take_decisions();
    let [Decision::Record { report }] = decisions.as_slice() else { panic!("{decisions:?}") };
    assert!(report.dry_run);
    assert_eq!(report.swept_objects, 1);
    assert!(matches!(fate(report, "/h/build/gone"), Fate::Go(Reason::CheckoutGone { .. })));
}

/// Without pressure a slice runs only while idle: once a stretch, then once a period. Free space
/// below comfortable opens a lap under pressure; one that frees nothing waits the period.
#[test]
fn triggers_come_on_the_period_when_idle_and_under_pressure() {
    let mut gc = Collector::new(config());
    gc.advance_clock(T0);
    // While builds run and nothing presses, nothing starts until the daemon is idle: no
    // deadline (review P2).
    assert_eq!(gc.next_deadline(), None);
    gc.idle(true);
    assert_eq!(gc.next_deadline(), Some(T0 + 300 * S), "the idle stretch counts from going idle");
    gc.advance_clock(T0 + 299 * S);
    assert!(gc.take_decisions().is_empty());
    gc.advance_clock(T0 + 600 * S);
    let run = match gc.take_decisions().as_slice() {
        [Decision::Scan { run, after: None, dirs: 4, .. }] => *run,
        other => panic!("{other:?}"),
    };
    gc.scanned(run, Ok(inventory(vec![workspace("w", 900, T0, true)])));
    let report = finish(&mut gc);
    assert_eq!(report.why, Trigger::Idle);
    // Once a stretch: still idle, no second idle lap.
    gc.advance_clock(T0 + 2 * 600 * S);
    assert!(gc.take_decisions().is_empty());
    // Free space falls below comfortable: a lap under pressure, a slow gap after the last.
    gc.volume(0, volume(820));
    gc.advance_clock(T0 + 2 * 600 * S + 60 * S);
    let run = match gc.take_decisions().as_slice() {
        [Decision::Scan { run, .. }] => *run,
        other => panic!("{other:?}"),
    };
    // What there is is within its grace: the lap frees nothing.
    gc.scanned(
        run,
        Ok(Inventory {
            objects: vec![workspace("w", 900, T0 + 1200 * S, true)],
            problems: Vec::new(),
            next: None,
        }),
    );
    let report = finish(&mut gc);
    assert_eq!(report.why, Trigger::Pressure);
    assert_eq!(report.swept_bytes, 0);
    gc.advance_clock(T0 + 3 * 600 * S + 120 * S);
    assert!(gc.take_decisions().is_empty(), "a lap under pressure that freed nothing backs off");
    gc.advance_clock(T0 + 2 * 600 * S + 60 * S + 6 * HOUR);
    assert!(
        matches!(gc.take_decisions().as_slice(), [Decision::Scan { .. }]),
        "the next lap comes on the period"
    );
}

/// A clock set far forward makes every build dir look unused: the run is refused at the mark.
#[test]
fn a_clock_far_past_every_use_refuses_the_run() {
    let mut gc = Collector::new(config());
    let run = asked(&mut gc, false);
    let old = T0 - 30 * DAY;
    let objects = vec![workspace("a", 10, old, true), profile("a", "debug", 10, old)];
    gc.scanned(run, Ok(Inventory { objects, problems: Vec::new(), next: None }));
    let report = finish(&mut gc);
    assert_eq!(
        report.outcome,
        Outcome::Stopped {
            phase: Phase::Mark,
            why: Stop::Clock { now_ns: T0, newest_ns: old },
            done: 0,
            left: 1
        }
    );
    assert_eq!(report.swept_objects, 0);
}

/// A clock set forward while the daemon was down, with no build dir left to tell by (only a
/// standard library's store): the last run the clock check let through is the reference (C2b).
#[test]
fn a_clock_far_past_the_last_run_refuses_a_run_with_no_build_dir() {
    let mut gc = Collector::new(config());
    let run = asked(&mut gc, false);
    gc.scanned(run, Ok(inventory(Vec::new())));
    finish(&mut gc);
    let mut restored = Collector::new(config());
    restored.restore(&gc.ledger());
    let later = T0 + 30 * DAY;
    restored.advance_clock(later);
    restored.ask(false);
    let run = match restored.take_decisions().as_slice() {
        [Decision::Scan { run, .. }] => *run,
        other => panic!("{other:?}"),
    };
    let store = Object {
        path: "/h/std/x".to_owned(),
        area: 0,
        class: Class::Std,
        bytes: 10,
        used_ns: T0 - DAY,
        workspace: None,
        checkout: None,
        leased: false,
        refused: None,
    };
    restored.scanned(run, Ok(Inventory { objects: vec![store], problems: Vec::new(), next: None }));
    let report = finish(&mut restored);
    assert_eq!(
        report.outcome,
        Outcome::Stopped {
            phase: Phase::Mark,
            why: Stop::Clock { now_ns: later, newest_ns: T0 },
            done: 0,
            left: 1
        }
    );
    assert_eq!(report.swept_objects, 0);
}

/// A rename the job found used since it was asked for (a lease taken in the process, a head
/// changed) keeps it, as a use after the mark does.
#[test]
fn a_rename_stopped_by_a_new_use_keeps_it() {
    let mut gc = Collector::new(config());
    let run = asked(&mut gc, false);
    gc.scanned(run, Ok(inventory(vec![workspace("a", 10, T0 - 30 * DAY, true)])));
    let decisions = gc.take_decisions();
    let [Decision::Trash { run, path, .. }] = decisions.as_slice() else { panic!("{decisions:?}") };
    gc.trashed(*run, path, Swept::Rooted);
    let report = finish(&mut gc);
    assert_eq!(*fate(&report, "/h/build/a"), Fate::Keep(Keep::UsedAfterMark));
    assert_eq!(report.swept_objects, 0);
}

/// The same inventory, roots and clock give the same decisions (rule 25).
#[test]
fn decisions_are_the_same_for_the_same_inputs() {
    let transcript = || {
        let mut gc = Collector::new(config());
        let run = asked(&mut gc, false);
        let old = T0 - 30 * DAY;
        let objects = vec![
            workspace("z", 1, old, true),
            workspace("a", 1, old, false),
            workspace("m", 1, old, true),
        ];
        gc.scanned(run, Ok(inventory(objects)));
        let mut seen = Vec::new();
        let mut decisions = gc.take_decisions();
        while !decisions.is_empty() {
            for decision in &decisions {
                seen.push(format!("{decision:?}"));
                match decision {
                    Decision::Trash { run, path, .. } => gc.trashed(*run, path, Swept::Moved),
                    Decision::Unlink { run, path } => gc.unlinked(*run, path, Ok(1)),
                    _ => {}
                }
            }
            decisions = gc.take_decisions();
        }
        seen
    };
    assert_eq!(transcript(), transcript());
}

/// Rule 26: the ledger round-trips, and a collector restored from it keeps the periodic
/// deadline and the run numbers.
#[test]
fn the_ledger_round_trips_and_restores_the_deadline() {
    let mut gc = Collector::new(config());
    let run = asked(&mut gc, false);
    gc.scanned(run, Ok(inventory(vec![workspace("gone", 10, T0 - 2 * HOUR, false)])));
    finish(&mut gc);
    let ledger = gc.ledger();
    let bytes = ledger.encode();
    assert_eq!(Ledger::decode(&bytes), Ok(ledger.clone()));
    assert!(ledger.runs.iter().all(|it| it.verdicts.is_empty()), "the ledger keeps no verdicts");
    let mut restored = Collector::new(config());
    restored.restore(&Ledger::decode(&bytes).unwrap());
    restored.advance_clock(T0 + S);
    gc.advance_clock(T0 + S);
    assert_eq!(restored.next_deadline(), gc.next_deadline());
    assert_eq!(restored.ledger(), gc.ledger());
    restored.ask(true);
    assert_eq!(
        restored.take_decisions(),
        [Decision::Scan { run: run + 1, after: None, dirs: u32::MAX, until_ns: None }]
    );
    let other = Ledger { format: 99, ..ledger };
    assert!(Ledger::decode(&other.encode()).unwrap_err().contains("format 99"));
}

#[test]
fn a_record_round_trips_and_refuses_another_format() {
    let record = crate::record::Record {
        format: crate::record::FORMAT,
        checkout: Some("/src/a".to_owned()),
        used_ns: 7,
        profiles: vec![("debug".to_owned(), 7)],
    };
    assert_eq!(crate::record::decode(&crate::record::encode(&record)), Ok(record.clone()));
    let other = crate::record::Record { format: 2, ..record };
    assert!(crate::record::decode(&crate::record::encode(&other)).is_err());
}
