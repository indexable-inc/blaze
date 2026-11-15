//! A launched program's start image across a crash and a start (`serve::daemon::launch`), on
//! the simulation's daemon: the real loop, store and journal, with the launch host's side
//! played by the fakes (`Fake`'s four launch steps, driven from here). The loop puts each
//! image the host keeps and marks it once the store says the put is durable; a start hands
//! the marks back to the host. A first call no record answers, where no task holds the
//! front end, is built by the daemon sending itself the build it last answered.
//!
//! `Sim::crash` keeps a seeded prefix of each file's unsynced writes, the store's and the
//! journal's apart, so a mark written before its image is durable can outlive the image.

use super::{super::daemon::MARKS_BEFORE_DURABLE, *};
use crate::serve::work::{LaunchKept, LaunchMarked};

/// The program the fakes' launch host keeps images of, by the path its link was asked for: the
/// unit's own output, hashed, as a real launched program's is (`target/debug/deps/app-<hash>`).
const PROGRAM: &str = "/w/target/debug/deps/app-0123";
/// The path it runs from, where the fakes' build answers it (`Artifact::executable`): another
/// path than [`PROGRAM`], as cargo's uplifted copy is, so a want or a lookup by the one where
/// the other is meant finds nothing.
const RUNS_AS: &str = APP_AT;

/// Turns after which an uncrashed run has put, made durable and marked both images: far more
/// than it takes, and every one of them is a crash point.
const SCHEDULE: usize = 40;

thread_local! {
    /// Start images the fakes' launch host hands the loop at its next turn.
    static KEEPS: RefCell<Vec<LaunchKept>> = const { RefCell::new(Vec::new()) };
    /// Programs whose first calls the fakes' host says wait for a front end no task holds.
    static WANTS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    /// What each start handed the host, in order of the starts.
    static HANDED: RefCell<Vec<Vec<LaunchMarked>>> = const { RefCell::new(Vec::new()) };
    /// How each of the daemon's own builds for a first call ended, as the host was told.
    static BUILT: RefCell<Vec<(String, Result<(), String>)>> = const { RefCell::new(Vec::new()) };
}

pub(super) fn marked(_: blaze_cas::Reader, _: Option<blaze_core::Waker>, marked: Vec<LaunchMarked>) {
    HANDED.with_borrow_mut(|it| it.push(marked));
}

pub(super) fn kept() -> Vec<LaunchKept> {
    KEEPS.with_borrow_mut(std::mem::take)
}

pub(super) fn wants() -> Vec<String> {
    WANTS.with_borrow_mut(std::mem::take)
}

pub(super) fn built(program: &str, built: Result<(), String>) {
    BUILT.with_borrow_mut(|it| it.push((program.to_owned(), built)));
}

fn reset() {
    KEEPS.with_borrow_mut(Vec::clear);
    WANTS.with_borrow_mut(Vec::clear);
    HANDED.with_borrow_mut(Vec::clear);
    BUILT.with_borrow_mut(Vec::clear);
}

/// The start image of build `build` of [`PROGRAM`]: keyed by its bytes, as a real one's key
/// covers its body, and of a size that spans pages.
fn image(build: u8) -> LaunchKept {
    let bytes = format!("the start image of build {build}\n").repeat(200 * usize::from(build)).into_bytes();
    LaunchKept {
        program: PROGRAM.to_owned(),
        runs_as: RUNS_AS.to_owned(),
        record: ContentId::of("blaze serve tests launch record", &[build]),
        key: blaze_cas::Key(ContentId::of("blaze serve tests launch image", &bytes)),
        bytes,
    }
}

/// The oracle, after a start: what it handed over for [`PROGRAM`] is nothing, or one image of
/// `kept` (its record, key and the path it runs from) that the store holds byte for byte as it
/// was kept. Gives which.
fn handed(daemon: &SimDaemon, kept: &[LaunchKept]) -> Result<Option<usize>, String> {
    // clone: a test's copy
    let Some(handed) = HANDED.with_borrow(|it| it.last().cloned()) else {
        return Err("the start handed the launch host nothing".to_owned());
    };
    let ours: Vec<&LaunchMarked> = handed.iter().filter(|it| it.program == PROGRAM).collect();
    let [LaunchMarked { record, key, runs_as, .. }] = ours.as_slice() else {
        return match ours.len() {
            0 => Ok(None),
            n => Err(format!("{n} marks of one program are handed over")),
        };
    };
    let Some(at) = kept.iter().position(|it| it.record == *record && it.key == *key && it.runs_as == *runs_as) else {
        return Err(format!(
            "the mark names record {record:?}, image {key:?} and runs from {runs_as:?}, which were never kept together"
        ));
    };
    match daemon.store().get(key) {
        Ok(Some(entry)) if entry.bytes == kept[at].bytes => Ok(Some(at)),
        Ok(Some(_)) => Err(format!("the store holds other bytes under the marked image {key:?}")),
        Ok(None) => Err(format!("the mark names image {key:?}, which the store does not hold")),
        Err(why) => Err(format!("the marked image {key:?} does not read: {why}")),
    }
}

/// A daemon on seed `seed` whose launch host keeps image 1 at its first turn and image 2 at
/// a turn the seed picks, run `turns` turns, then crashed (or, with `crash` false, stopped)
/// and started again: what the oracle says of that start.
fn after(seed: u64, turns: usize, crash: bool, plant: bool) -> Result<Option<usize>, String> {
    reset();
    MARKS_BEFORE_DURABLE.set(plant);
    let kept = [image(1), image(2)];
    let second = 1 + (seed % 5) as usize;
    let mut sim = Sim::new(seed, Faults::default());
    checkout(&mut sim, "[package]\nname = \"app\"\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, seed);
    for turn in 0..turns {
        if turn == 0 {
            KEEPS.with_borrow_mut(|it| it.push(kept[0].clone())); // clone: a test's copy
        }
        if turn == second {
            KEEPS.with_borrow_mut(|it| it.push(kept[1].clone())); // clone: as above
        }
        daemon.turn();
    }
    let daemon = if crash {
        let mut sim = daemon.into_driver();
        sim.crash();
        start(sim, &steps, seed + 1000)
    } else {
        restart(daemon, &steps, seed + 1000)
    };
    let said = handed(&daemon, &kept);
    MARKS_BEFORE_DURABLE.set(false);
    said
}

/// The invariant: after a crash at any turn and a start, the launch host is handed, for each
/// program, nothing or a (record, key) whose image the store holds byte for byte as it was
/// kept; after a stop, the newest image kept. A first call with no front end and no build
/// of its program answered since the start ends by name; after one, a want by the path the
/// program runs from, as its restored mark gives it, sends that build once, however many
/// calls asked, and the host is told it ended ok. The controls: `MARKS_BEFORE_DURABLE` marks
/// an image as it is put, and a crash then leaves a mark naming an image the store lost,
/// which the oracle catches; and a want by the path the program's link was asked for (the
/// lookup the daemon's answers do not key by) ends with no build.
#[test]
fn a_start_image_is_marked_once_durable_and_a_start_hands_over_the_newest_or_nothing() {
    // 8 seeds, each crashed after every one of its turns: 328 schedules.
    let (mut restored, mut nothing, mut caught) = (0, 0, 0);
    for seed in 0..8 {
        assert_eq!(after(seed, SCHEDULE, false, false), Ok(Some(1)), "seed {seed}: a stop hands over the newest image");
        for turns in 0..=SCHEDULE {
            match after(seed, turns, true, false) {
                Ok(Some(_)) => restored += 1,
                Ok(None) => nothing += 1,
                Err(why) => panic!("seed {seed}, crash after turn {turns}: {why}"),
            }
            caught += usize::from(after(seed, turns, true, true).is_err());
        }
    }
    assert!(restored > 0 && nothing > 0, "the crash points never split: {restored} restored, {nothing} nothing");
    assert!(caught > 0, "the control is never caught: a mark before its image is durable");

    // A first call with no front end, before any build reached the daemon: ended by name.
    reset();
    let mut sim = Sim::new(9, Faults::default());
    checkout(&mut sim, "[package]\nname = \"app\"\n");
    let steps = Arc::new(Fake::default());
    let mut daemon = start(sim, &steps, 9);
    WANTS.with_borrow_mut(|it| it.push(RUNS_AS.to_owned()));
    turn_until(&mut daemon, |_| BUILT.with_borrow(|it| !it.is_empty()));
    let first = BUILT.with_borrow_mut(std::mem::take);
    assert!(
        matches!(&first[..], [(program, Err(why))] if program == RUNS_AS && why.contains("no `blaze run`")),
        "{first:?}"
    );
    // An image kept and the daemon stopped: the next start hands the program back with the
    // path it runs from, as a restored program has no build to learn it from.
    KEEPS.with_borrow_mut(|it| it.push(image(1)));
    for _ in 0..SCHEDULE {
        daemon.turn();
    }
    let mut daemon = restart(daemon, &steps, 10);
    // clone: a test's copy
    let handed = HANDED.with_borrow(|it| it.last().cloned()).expect("the start handed the launch host its marks");
    let [mark] = &handed[..] else { panic!("one mark is handed over: {handed:?}") };
    assert_eq!((mark.program.as_str(), mark.runs_as.as_str()), (PROGRAM, RUNS_AS), "{mark:?}");
    // A build answered since the start: it names the program by the path it runs from.
    let mut client = dial(&mut daemon);
    let id = client.send(&mut daemon, Request::Build { view: ViewRef::Head("/w".into()), targets: agent() });
    let got = client.wait(&mut daemon, id);
    assert_eq!(got.last(), Some(&Response::Done { ok: true }), "{got:#?}");
    let executable = got.iter().find_map(|it| match it {
        Response::Built { artifacts, .. } => artifacts.iter().find_map(|it| it.executable.clone()),
        _ => None,
    });
    assert_eq!(executable.as_deref(), Some(RUNS_AS), "the build makes the program, run from its uplifted copy");
    let answered = client.of(id).len();
    // The control: a want by the path the program's link was asked for, which no build's
    // answer names, finds no build and ends by name.
    WANTS.with_borrow_mut(|it| it.push(mark.program.clone())); // clone: a test's copy
    turn_until(&mut daemon, |_| BUILT.with_borrow(|it| !it.is_empty()));
    let planted = BUILT.with_borrow_mut(std::mem::take);
    assert!(
        matches!(&planted[..], [(program, Err(why))] if program == PROGRAM && why.contains("no `blaze run`")),
        "the control: a want by the link's path is never built: {planted:?}"
    );
    // A want by the path it runs from, the restored mark's: built by that build again, once,
    // however many calls asked.
    WANTS.with_borrow_mut(|it| it.extend([mark.runs_as.clone(), mark.runs_as.clone()])); // clone: a test's copy
    turn_until(&mut daemon, |_| BUILT.with_borrow(|it| !it.is_empty()));
    for _ in 0..SCHEDULE {
        daemon.turn();
        client.take(&mut daemon);
    }
    let again = BUILT.with_borrow(Clone::clone); // clone: a test's copy
    assert_eq!(again, [(RUNS_AS.to_owned(), Ok(()))], "one build for both calls, ended ok");
    assert_eq!(client.of(id).len(), answered, "the daemon's own build tells the client nothing");
}
