//! The pacer on a fake clock (docs/gc.md, "Pacing"): the rate rises smoothly as free space falls,
//! a level crossed back and forth does not switch collection on and off, and a build short of
//! space with everything held gets the typed error and no deletion.

use crate::{
    pace::{FULL, Levels, Pace, Volume},
    plan::{
        Checkout, Class, Collector, Decision, Inventory, Keep, Object, Outcome, Phase, Priority,
        Released, Stop, Trigger,
    },
    tests::{config, drive, volume, workspace},
};

const S: u64 = 1_000_000_000;
const GIB: u64 = 1 << 30;
const T0: u64 = 1_000 * 24 * 3600 * S;

/// Bytes a second the slices for `pressure` may free: a slice's bytes over the gap to the next.
fn rate(pace: &Pace, pressure: u32) -> u64 {
    let slice = pace.slice(pressure, false);
    (u128::from(slice.bytes) * u128::from(S) / u128::from(slice.gap_ns.max(1))) as u64
}

/// A transcript: free space on a 1 TiB volume falls from past comfortable to the floor in a
/// hundred measures. The collector's pressure, and the rate its slices may free at, never fall,
/// rise by a small step each measure, and reach the fast rate at the floor.
#[test]
fn the_deletion_rate_rises_smoothly_as_free_space_falls() {
    let levels = Levels::default();
    let pace = Pace::default();
    let mut gc = Collector::new(crate::Config {
        levels,
        pace,
        ..crate::Config::daemon(vec!["/h".to_owned()])
    });
    let total = 1024 * GIB;
    let probe = Volume { id: 1, total_bytes: total, free_bytes: 0 };
    let (comfortable, floor) = (levels.comfortable(&probe), levels.floor(&probe));
    let band = comfortable - floor;
    let span = pace.fast_bytes_per_s - pace.slow_bytes_per_s;
    let mut last_rate = 0;
    let mut last_pressure = 0;
    let mut rates = Vec::new(); // alloc: a test's transcript
    for step in 0..=110u64 {
        let free = match step {
            110 => floor,
            _ => (comfortable + band / 10).saturating_sub(band * step / 100).max(floor),
        };
        gc.volume(0, Volume { id: 1, total_bytes: total, free_bytes: free });
        let pressure = gc.pressure();
        let now = if pressure == 0 { 0 } else { rate(&pace, pressure) };
        assert!(
            pressure >= last_pressure,
            "pressure fell from {last_pressure} to {pressure} at step {step}"
        );
        assert!(now >= last_rate, "the rate fell from {last_rate} to {now} at step {step}");
        let rise = now - last_rate;
        if last_rate == 0 {
            // Switching on starts gently: at most the slow rate and the band's share of the span.
            assert!(
                rise <= pace.slow_bytes_per_s + span / 10,
                "a jump of {rise} switching on at step {step}"
            );
        } else {
            // One measure moves free space a hundredth of the band: at most 2% of the span.
            assert!(rise <= span / 50, "a jump of {rise} at step {step}");
        }
        rates.push(now);
        (last_rate, last_pressure) = (now, pressure);
    }
    assert_eq!(gc.pressure(), FULL);
    assert_eq!(last_rate, pace.fast_bytes_per_s);
    assert!(
        rates.iter().filter(|it| **it > 0).count() > 80,
        "the rate was paced over most of the band: {rates:?}"
    );
}

/// Free space hovering at a level does not switch collection on and off: inside the band about
/// comfortable it never comes on, and once on it stays on until free space is past the band.
#[test]
fn pressure_does_not_oscillate_around_a_level() {
    let mut gc = Collector::new(config());
    // Test levels: comfortable at 1000 free, floor at 600, band 400, hysteresis 5% (20 bytes).
    let mut switches = 0;
    let mut on = false;
    let mut count = |gc: &Collector, on: &mut bool| {
        let now = gc.pressure() > 0;
        if now != *on {
            switches += 1;
            *on = now;
        }
    };
    // Within 5% either side of comfortable: off throughout.
    for step in 0..100u64 {
        gc.volume(0, volume(if step % 2 == 0 { 1015 } else { 985 }));
        count(&gc, &mut on);
    }
    assert!(!on);
    // Around the point it comes on (980 free, pressure 50): it comes on once and stays.
    for step in 0..100u64 {
        gc.volume(0, volume(if step % 2 == 0 { 975 } else { 990 + step % 7 }));
        count(&gc, &mut on);
    }
    drop(count);
    assert!(on);
    assert_eq!(switches, 1, "one switch on, none back");
}

/// Free space drops below the floor and recovers: pressure comes on, is full at the floor, and
/// goes off only once free space is past the band above comfortable.
#[test]
fn free_space_falls_below_the_floor_and_recovers_past_comfortable() {
    let mut gc = Collector::new(config());
    gc.volume(0, volume(2000));
    assert_eq!(gc.pressure(), 0);
    gc.volume(0, volume(800));
    assert_eq!(gc.pressure(), 500);
    gc.volume(0, volume(500));
    assert_eq!(gc.pressure(), FULL);
    // Back at comfortable: still on, at the band's start, so the last of the work is done gently.
    gc.volume(0, volume(1000));
    assert_eq!(gc.pressure(), 50);
    gc.volume(0, volume(1019));
    assert_eq!(gc.pressure(), 50);
    gc.volume(0, volume(1020));
    assert_eq!(gc.pressure(), 0);
}

/// A build short of space while everything it could free is leased: the urgent run asks for no
/// rename or unlink of it, and its report names what holds the space.
#[test]
fn everything_leased_and_no_space_gives_the_typed_error_never_a_deletion() {
    let mut gc = Collector::new(config());
    gc.advance_clock(T0);
    gc.volume(0, volume(500));
    gc.urgent();
    let run = match gc.take_decisions().as_slice() {
        [Decision::Scan { run, after: None, dirs: u32::MAX, until_ns: None }] => *run,
        other => panic!("an urgent run begins with a scan, not {other:?}"),
    };
    let leased = |name: &str, bytes: u64| Object {
        path: format!("/h/build/{name}"),
        area: 0,
        class: Class::Workspace,
        bytes,
        used_ns: T0 - 2 * 24 * 3600 * S,
        workspace: None,
        // Its checkout gone and past its grace: it would go, were it not leased.
        checkout: Some(Checkout { path: format!("/src/{name}"), exists: false }),
        leased: true,
        refused: None,
    };
    let objects = vec![leased("a", 3000), leased("b", 2000)];
    gc.scanned(run, Ok(Inventory { objects, problems: Vec::new(), next: None }));
    let decisions = gc.take_decisions();
    let [Decision::Record { report }] = decisions.as_slice() else {
        panic!("asked for more than its record: {decisions:?}")
    };
    assert_eq!(report.why, Trigger::Urgent);
    assert_eq!(report.swept_objects, 0);
    let short = report.short.as_ref().expect("the volume is still below its floor");
    assert_eq!(short.free_bytes, 500);
    assert_eq!(short.floor_bytes, 600);
    assert_eq!(
        short.kept,
        [
            ("/h/build/a".to_owned(), 3000, Keep::Leased),
            ("/h/build/b".to_owned(), 2000, Keep::Leased)
        ]
    );
    let message = short.to_string();
    assert!(message.contains("/h/build/a (3000 bytes, Leased)"), "{message}");
}

/// The share's pressure is never below 0: it must not hold the signed pressure up at 0 once
/// free space is far past comfortable, or collection never turns off (review C1).
#[test]
fn collection_turns_off_once_free_space_is_past_the_band() {
    let mut gc = Collector::new(config());
    gc.volume(0, volume(700));
    assert!(gc.pressure() > 0);
    gc.volume(0, volume(5000));
    assert_eq!(gc.pressure(), 0, "far past comfortable, with blaze far under its share");
    assert_eq!(gc.next_deadline(), None, "nothing presses, and the daemon is not idle");
}

/// Advances to `at`, where a slice under pressure starts: its run.
fn a_busy_slice(gc: &mut Collector, at: u64) -> u64 {
    gc.advance_clock(at);
    match gc.take_decisions().as_slice() {
        [Decision::Scan { run, until_ns: Some(until), .. }] => {
            assert_eq!(*until, at + Pace::default().slice_ns / 2, "a slice's scan stops halfway");
            *run
        }
        other => panic!("a slice begins with a scan that has a time to stop, not {other:?}"),
    }
}

/// A slice under pressure whose scan answers after the slice's own deadline: the run waits for
/// it (up to its hard bound), the cursor moves, and the next slice goes on past it. Before, the
/// run ended at the slice's deadline, the late answer was dropped, and every slice scanned the
/// same first units again (review C2).
#[test]
fn a_scan_slower_than_its_slice_still_moves_the_cursor() {
    let mut gc = Collector::new(config());
    gc.advance_clock(T0);
    // 700 free: pressure 750, past `busy_from`, so a slice runs while builds do.
    gc.volume(0, volume(700));
    let run = a_busy_slice(&mut gc, T0 + S);
    gc.advance_clock(T0 + S + 6 * S);
    assert!(gc.running(), "the slice's deadline passed with its scan out: the run waits");
    assert!(gc.take_decisions().is_empty());
    let found = Inventory {
        objects: vec![workspace("w1", 0, T0 - 2 * 3600 * S, true)],
        problems: Vec::new(),
        next: Some("/h/build/w1".to_owned()),
    };
    gc.scanned(run, Ok(found));
    let decisions = gc.take_decisions();
    let report = drive(&mut gc, decisions);
    assert_eq!(report.outcome, Outcome::Done);
    // The next slice goes on past the cursor.
    let gap = Pace::default().slice(750, false).gap_ns;
    gc.advance_clock(T0 + 7 * S + gap);
    match gc.take_decisions().as_slice() {
        [Decision::Scan { after: Some(after), .. }] => assert_eq!(after, "/h/build/w1"),
        other => panic!("the next slice goes on from the cursor, not {other:?}"),
    }
}

/// A slice whose scan never came back ends at its hard deadline; the next slice waits for that
/// answer (two runs' work never overlaps in the driver), an urgent run does not, and the
/// decisions say how soon they are to be done: a busy slice with builds, an urgent run ahead of
/// them (review C3).
#[test]
fn an_urgent_run_does_not_wait_for_a_stale_answer() {
    let mut gc = Collector::new(config());
    gc.advance_clock(T0);
    gc.volume(0, volume(700));
    let stale = a_busy_slice(&mut gc, T0 + S);
    assert_eq!(gc.priority(), Priority::Normal, "a busy slice goes with builds");
    // The test config's run bound is 60 s.
    gc.advance_clock(T0 + S + 61 * S);
    match gc.take_decisions().as_slice() {
        [Decision::Record { report }] => {
            assert!(matches!(report.outcome, Outcome::Stopped { why: Stop::Deadline { .. }, .. }))
        }
        other => panic!("{other:?}"),
    }
    // A gap later, the next slice waits for the stale scan's answer.
    gc.advance_clock(T0 + S + 61 * S + 30 * S);
    assert!(gc.take_decisions().is_empty() && !gc.running());
    assert_eq!(
        gc.next_deadline(),
        Some(T0 + S + 61 * S + 60 * S),
        "until the answer is given up on"
    );
    // Free space falls to the floor: an urgent run starts at once, stale answer or not.
    gc.volume(0, volume(500));
    assert!(gc.below_floor());
    gc.urgent();
    match gc.take_decisions().as_slice() {
        [Decision::Scan { until_ns: None, dirs: u32::MAX, .. }] => {}
        other => panic!("an urgent run begins with a whole scan, not {other:?}"),
    }
    assert_eq!(gc.priority(), Priority::Critical);
    // The stale answer, when it comes, is dropped: it is of a run long over.
    gc.scanned(stale, Ok(Inventory::default()));
    assert!(gc.take_decisions().is_empty() && gc.running());
}

/// Without pressure a slice's decisions wait for an idle pool.
#[test]
fn a_slice_without_pressure_is_background_work() {
    let mut gc = Collector::new(config());
    gc.advance_clock(T0);
    gc.idle(true);
    gc.advance_clock(T0 + 600 * S);
    assert!(matches!(gc.take_decisions().as_slice(), [Decision::Scan { .. }]));
    assert_eq!(gc.priority(), Priority::Background);
}

/// A build out of space is held: an urgent run starts at once, whatever the rate limit, and
/// the build is let go once the volume is measured after the run, with what holds the space
/// when it is still below its floor; never on a report restored or from before it (ENOSPC
/// hold, review decision b).
#[test]
fn a_build_out_of_space_is_held_until_the_urgent_run_is_measured() {
    let mut gc = Collector::new(config());
    gc.advance_clock(T0);
    gc.volume(0, volume(500));
    let hold = gc.hold_for_space(None);
    let run = match gc.take_decisions().as_slice() {
        [Decision::Scan { run, until_ns: None, .. }] => *run,
        other => panic!("{other:?}"),
    };
    let leased = Object { leased: true, ..workspace("a", 3000, T0 - 2 * 24 * 3600 * S, false) };
    gc.scanned(run, Ok(Inventory { objects: vec![leased], problems: Vec::new(), next: None }));
    assert!(matches!(gc.take_decisions().as_slice(), [Decision::Record { .. }]));
    assert!(gc.take_released().is_empty(), "not before the volume is measured after the run");
    assert!(gc.no_space().is_none());
    gc.volume(0, volume(500));
    let released = gc.take_released();
    let [(id, Released::NoSpace(short))] = released.as_slice() else { panic!("{released:?}") };
    assert_eq!(*id, hold);
    assert_eq!(short.kept, [("/h/build/a".to_owned(), 3000, Keep::Leased)]);
    assert!(gc.no_space().is_some());
    gc.volume(0, volume(2000));
    assert!(gc.no_space().is_none(), "let go once the volume is above its floor");

    // A second build out of space, right after, with nothing freed since and no lease let go:
    // let go at once with what holds the space, and no run walks everything again (review N4).
    gc.volume(0, volume(500));
    let hold = gc.hold_for_space(None);
    assert!(gc.take_decisions().is_empty(), "no second urgent walk of what is held");
    let released = gc.take_released();
    let [(id, Released::NoSpace(short))] = released.as_slice() else { panic!("{released:?}") };
    assert_eq!(*id, hold);
    assert_eq!(short.kept, [("/h/build/a".to_owned(), 3000, Keep::Leased)]);

    // A third, once a build ended and let its lease go: the rate limit does not hold it back,
    // and the run frees what it can; measured after, the volume is above its floor.
    gc.lease_released();
    let hold = gc.hold_for_space(None);
    let run = match gc.take_decisions().as_slice() {
        [Decision::Scan { run, .. }] => *run,
        other => panic!("{other:?}"),
    };
    let gone = workspace("b", 1000, T0 - 2 * 24 * 3600 * S, false);
    gc.scanned(run, Ok(Inventory { objects: vec![gone], problems: Vec::new(), next: None }));
    let decisions = gc.take_decisions();
    let report = drive(&mut gc, decisions);
    assert_eq!(report.why, Trigger::Urgent);
    gc.volume(0, volume(1500));
    assert_eq!(gc.take_released(), [(hold, Released::Freed { run, freed_bytes: 1000 })]);

    // A fourth, whose run's scan never answers: let go when its hold runs out.
    gc.volume(0, volume(500));
    gc.lease_released();
    let hold = gc.hold_for_space(Some(0));
    assert!(matches!(gc.take_decisions().as_slice(), [Decision::Scan { .. }]));
    let held_at = gc.now();
    gc.advance_clock(held_at + 120 * S);
    let released = gc.take_released();
    assert_eq!(
        released,
        [(hold, Released::TimedOut { waited_ns: 120 * S, area: Some("/h".to_owned()) })]
    );
    assert!(released[0].1.to_string().contains("no urgent gc run ended"));
}

/// A build held for space while a paced slice runs is let go within its hold (review N1): the
/// slice, whose scan waits in the pool's background queue behind builds, ends at once
/// (preempted), and the urgent run starts in the same step, ahead of the builds. Before, the
/// urgent run waited for the slice's hard deadline (`max(slice, run_ns)`, ten minutes for the
/// daemon), past the two-minute hold.
#[test]
fn a_build_held_during_a_paced_slice_is_released_within_its_hold() {
    let config = config();
    let hold_ns = config.hold_ns;
    let mut gc = Collector::new(config);
    gc.advance_clock(T0);
    gc.idle(true);
    gc.advance_clock(T0 + 600 * S);
    let slice = match gc.take_decisions().as_slice() {
        [Decision::Scan { run, .. }] => *run,
        other => panic!("{other:?}"),
    };
    assert_eq!(gc.priority(), Priority::Background);
    // Builds start, and the slice's scan waits behind them; one runs out of space.
    gc.idle(false);
    let held_at = T0 + 610 * S;
    gc.advance_clock(held_at);
    gc.volume(0, volume(500));
    let hold = gc.hold_for_space(Some(0));
    // The preempted slice counts as ended before its record is drained, so the driver's gate
    // stops what of it is queued at once (review round 5, L1).
    assert_eq!(gc.ended_through(), slice);
    let decisions = gc.take_decisions();
    let [Decision::Record { report }, Decision::Scan { run: urgent, until_ns: None, .. }] =
        decisions.as_slice()
    else {
        panic!("the slice ends and the urgent run starts in the same step, not {decisions:?}")
    };
    let urgent = *urgent;
    assert_eq!(report.run, slice);
    assert!(
        matches!(
            report.outcome,
            Outcome::Stopped {
                phase: Phase::Scan,
                why: Stop::Preempted { by: Trigger::Urgent },
                ..
            }
        ),
        "{:?}",
        report.outcome
    );
    assert_eq!(gc.priority(), Priority::Critical);
    // The slice's scan, when it comes, is of a run that ended: dropped.
    gc.scanned(slice, Ok(Inventory::default()));
    assert!(gc.take_decisions().is_empty() && gc.running());
    gc.advance_clock(held_at + 5 * S);
    let gone = workspace("b", 1000, T0 - 2 * 24 * 3600 * S, false);
    gc.scanned(urgent, Ok(Inventory { objects: vec![gone], problems: Vec::new(), next: None }));
    let decisions = gc.take_decisions();
    let report = drive(&mut gc, decisions);
    assert_eq!((report.run, report.why), (urgent, Trigger::Urgent));
    gc.volume(0, volume(1500));
    assert_eq!(gc.take_released(), [(hold, Released::Freed { run: urgent, freed_bytes: 1000 })]);
    assert!(gc.now() < held_at + hold_ns, "let go within its hold");
}

/// A hold is let go as soon as the urgent run under way has freed space on its volume, as
/// measured, not only once the whole walk ends (review N5); and a hold's bound grows with the
/// last whole walk's time.
#[test]
fn a_held_build_is_let_go_once_its_volume_is_freed_mid_run() {
    // A run bound past the walk's 100 s, so the walk ends whole.
    let config = crate::Config { run_ns: 600 * S, ..config() };
    let hold_ns = config.hold_ns;
    let mut gc = Collector::new(config);
    gc.advance_clock(T0);
    gc.volume(0, volume(500));
    let hold = gc.hold_for_space(Some(0));
    let run = match gc.take_decisions().as_slice() {
        [Decision::Scan { run, until_ns: None, .. }] => *run,
        other => panic!("{other:?}"),
    };
    let old = T0 - 2 * 24 * 3600 * S;
    let objects = vec![workspace("b", 1000, old, false), workspace("c", 1000, old, false)];
    gc.scanned(run, Ok(Inventory { objects, problems: Vec::new(), next: None }));
    // The first rename is asked for and moves; the measure sees space freed.
    let first = match gc.take_decisions().as_slice() {
        [Decision::Trash { path, .. }] => path.clone(),
        other => panic!("{other:?}"),
    };
    gc.trashed(run, &first, crate::plan::Swept::Moved);
    gc.volume(0, volume(1500));
    assert!(gc.running(), "the run goes on");
    assert_eq!(gc.take_released(), [(hold, Released::Freed { run, freed_bytes: 1000 })]);
    // The walk takes 100 s in all: the next hold is bounded by twice that, past hold_ns.
    let decisions = gc.take_decisions();
    gc.advance_clock(T0 + 100 * S);
    let report = drive(&mut gc, decisions);
    assert_eq!(report.outcome, Outcome::Done);
    gc.volume(0, volume(500));
    gc.lease_released();
    let hold = gc.hold_for_space(None);
    _ = gc.take_decisions();
    gc.advance_clock(T0 + 100 * S + hold_ns);
    assert!(gc.take_released().is_empty(), "twice the last walk is more than hold_ns: still held");
    gc.advance_clock(T0 + 100 * S + 200 * S);
    let released = gc.take_released();
    assert!(
        matches!(released.as_slice(), [(id, Released::TimedOut { .. })] if *id == hold),
        "{released:?}"
    );
}
