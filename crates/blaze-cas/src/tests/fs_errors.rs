//! The store against a file system that loses writes and says so (Linux 7.0's fserror,
//! `FAN_FS_ERROR`, as [`Sim::fs_error`] plays it): any run of puts, flushes, crashes and lost
//! writes reopens to a store that serves every put it answered durable, intact, and nothing
//! torn. A commit the error overlaps is never answered durable until it is written again and
//! synced after the error: the file's own sync says it is done, so only the event tells. The
//! planted bug (`Store::plant_ignore_fs_errors`) ignores the event, as a store that trusts its
//! fsyncs alone does.

use hegel::TestCase;
use hegel::generators as gs;

use super::*;

/// The errno of a failed writeback.
const EIO: i32 = 5;

/// Completions a flush may take before the run is a failure of its own (a loop of retries).
const POLLS_MOST: u32 = 20_000;

#[derive(Clone, Debug)]
enum Step {
    /// Puts of this many new values.
    Put(u8),
    /// A flush, driven one poll at a time: the file system loses every write of the store not
    /// yet synced once `error_after` completions are in (or after the flush, if it ends first),
    /// and the machine crashes once `crash_after` are.
    Flush { error_after: Option<u8>, crash_after: Option<u8> },
}

#[derive(Clone, Debug)]
struct Case {
    seed: u64,
    /// Injected IO failures besides (0: none), so a lost write meets retries of failed ones.
    fail_one_in: u32,
    steps: Vec<Step>,
}

fn value(n: u64) -> Vec<u8> {
    noise(200 + (n as usize * 37) % 3000, n)
}

/// What the store holds, as the run told it.
#[derive(Default)]
struct Model {
    /// Every value put, under its key: a reopened store serves it intact or not at all.
    put: BTreeMap<Key, Vec<u8>>,
    /// The keys of the puts answered durable: a reopened store serves each.
    durable: Vec<Key>,
}

/// The machine crashed (or the store was dropped), and the store opened again: it must open,
/// watch its file system's errors, serve every durable put intact and nothing torn.
fn reopen(sim: &mut Sim, model: &Model, plant: bool) -> Result<Store, String> {
    sim.crash();
    let mut effects = Vec::new();
    let mut store = Store::open(config("store"), &mut effects);
    drive(sim, &mut store, &mut effects);
    if !store.ready() {
        return Err(format!("the store did not open after a crash: {:?}", store.broken()));
    }
    if store.fs_errors() != &FsErrors::Watched {
        return Err(format!("the store does not watch its errors: {:?}", store.fs_errors()));
    }
    store.plant_ignore_fs_errors = plant;
    for key in &model.durable {
        match store.get(key) {
            Ok(Some(entry)) if entry.bytes == model.put[key] => {}
            Ok(Some(_)) => return Err(format!("a durable put {key:?} is served other bytes")),
            Ok(None) => return Err(format!("a put answered durable was lost: {key:?}")),
            Err(torn) => return Err(format!("a durable put is torn: {torn:?}")),
        }
    }
    for (key, bytes) in &model.put {
        match store.get(key) {
            Ok(None) => {}
            Ok(Some(entry)) if entry.bytes == bytes.as_slice() => {}
            Ok(Some(_)) => return Err(format!("a put {key:?} is served other bytes")),
            Err(torn) => return Err(format!("a put is torn: {torn:?}")),
        }
    }
    match store.verify(None) {
        bad if bad.is_empty() => Ok(store),
        bad => Err(format!("the reopened store does not verify: {bad:?}")),
    }
}

/// Runs one case; `Err` says how the oracle failed.
fn run(case: &Case, plant: bool) -> Result<(), String> {
    let faults = Faults { fail_one_in: case.fail_one_in, ..Faults::default() };
    let mut sim = Sim::new(case.seed, faults);
    let mut model = Model::default();
    let mut store = reopen(&mut sim, &model, plant)?;
    let mut tickets: BTreeMap<Ticket, Key> = BTreeMap::new();
    let mut next = 0u64;
    for step in &case.steps {
        let (error_after, crash_after) = match *step {
            Step::Put(count) => {
                for _ in 0..count {
                    next += 1;
                    let bytes = value(next);
                    let ticket = store
                        .put(key(next), Kind::Code, &bytes)
                        .map_err(|it| format!("put {next} refused: {it:?}"))?;
                    tickets.insert(ticket, key(next));
                    model.put.insert(key(next), bytes);
                }
                continue;
            }
            Step::Flush { error_after, crash_after } => (error_after, crash_after),
        };
        let mut effects = Vec::new();
        store.flush(&mut effects);
        let (mut seen, mut errored, mut crashed, mut done) = (0u32, false, false, Vec::new());
        let mut polls = 0;
        loop {
            if !errored && error_after.is_some_and(|at| seen >= u32::from(at)) {
                errored = true;
                sim.fs_error(&name("store"), EIO);
            }
            if crash_after.is_some_and(|at| seen >= u32::from(at)) {
                crashed = true;
                break;
            }
            sim.submit(&mut effects);
            if sim.in_flight() == 0 {
                break;
            }
            polls += 1;
            if polls > POLLS_MOST {
                return Err(format!("a flush never settled: {seen} completions"));
            }
            sim.poll(&mut done, Duration::from_secs(60));
            for completion in done.drain(..) {
                seen += 1;
                if store.owns(completion.tag) {
                    for ticket in store.complete(completion, &mut effects) {
                        model.durable.extend(tickets.remove(&ticket));
                    }
                }
            }
        }
        // The flush ended before the error: it loses nothing, and the next commit hears it.
        if !errored && error_after.is_some() {
            sim.fs_error(&name("store"), EIO);
        }
        if let Some(broken) = store.broken() {
            // Its commits failed `FAILURES_MOST` times in a row, as on a failing disk: it stops,
            // and the reopen checks it lost nothing durable.
            let named = broken.what == "injected by the simulation"
                || broken.what.contains("file system reported a write it lost");
            if !named || store.failures < FAILURES_MOST {
                return Err(format!("{broken:?} after {} failures in a row", store.failures));
            }
        }
        if crashed || store.broken().is_some() {
            drop(store);
            tickets.clear();
            store = reopen(&mut sim, &model, plant)?;
        }
    }
    drop(store);
    reopen(&mut sim, &model, plant).map(drop)
}

#[hegel::composite]
fn step_draw(tc: &TestCase) -> Step {
    match tc.draw(gs::integers::<u8>().max_value(2)) {
        0 => Step::Put(tc.draw(gs::integers().min_value(1).max_value(12))),
        _ => Step::Flush {
            error_after: tc.draw(gs::optional(gs::integers().max_value(8))),
            crash_after: tc.draw(gs::optional(gs::integers().max_value(10))),
        },
    }
}

#[hegel::composite]
fn case_draw(tc: &TestCase) -> Case {
    Case {
        seed: tc.draw(gs::integers()),
        fail_one_in: if tc.draw(gs::booleans()) { 11 } else { 0 },
        steps: tc.draw(gs::vecs(step_draw()).min_size(1).max_size(12)),
    }
}

/// The DST: any run of puts, flushes, crashes and lost writes reopens to a store that serves
/// every durable put intact and nothing torn. `HEGEL_TEST_CASES=<n>` runs it longer.
#[test]
fn a_write_the_file_system_lost_is_committed_again_before_it_is_answered_durable() {
    super::model_check::run_cases(48, |tc| {
        let case = tc.draw(case_draw());
        if let Err(why) = run(&case, false) {
            panic!("{case:?}: {why}");
        }
    });
}

/// The planted-bug control: a store that ignores the event answers durable a commit whose
/// writes the file system lost, and the same oracle catches it after the crash.
#[test]
fn the_fs_error_oracle_catches_a_store_that_ignores_the_event() {
    let mut caught = None;
    'search: for seed in 0..32 {
        for error_after in 0..8 {
            let case = Case {
                seed,
                fail_one_in: 0,
                steps: vec![
                    Step::Put(6),
                    Step::Flush { error_after: Some(error_after), crash_after: None },
                ],
            };
            assert_eq!(run(&case, false), Ok(()), "{case:?}: the fixed store must pass");
            if let Err(why) = run(&case, true) {
                caught = Some((case, why));
                break 'search;
            }
        }
    }
    let (case, why) = caught.expect("the oracle missed the store that ignores lost writes");
    assert!(!why.contains("never settled"), "{case:?}: caught, but not as a lost write: {why}");
}

/// Where the file system's errors cannot be watched (no `CAP_SYS_ADMIN`, or a file system
/// that reports none), the store says so as a named state and fsync stays the signal: puts
/// still become durable by their syncs.
#[test]
fn a_store_that_cannot_watch_fs_errors_says_why_and_its_synced_puts_stay_durable() {
    let mut sim = Sim::new(3, quiet());
    sim.refuse_fs_errors("fanotify_init: the daemon lacks CAP_SYS_ADMIN");
    let mut store = open(&mut sim, "store");
    match store.fs_errors() {
        FsErrors::Unavailable(why) => assert!(
            why.starts_with(blaze_core::FS_ERRORS_UNAVAILABLE) && why.contains("CAP_SYS_ADMIN"),
            "{why}"
        ),
        other => panic!("the refused watch is not a named state: {other:?}"),
    }
    let ticket = store.put(key(1), Kind::Code, &value(1)).unwrap();
    assert_eq!(flush(&mut sim, &mut store), vec![ticket]);
    drop(store);
    sim.crash();
    let store = open(&mut sim, "store");
    assert_eq!(bytes_of(&store, &key(1)), Some(value(1)));
    assert!(matches!(store.fs_errors(), FsErrors::Unavailable(_)), "{:?}", store.fs_errors());
}
