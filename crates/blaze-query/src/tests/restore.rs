//! The database across restarts, on blaze-core's simulation (rules dst-not-review and
//! persisted-state-round-trips; docs/v2/plan/design/restore.md, "Persisted state that is hard
//! to get wrong"): files edited, keys asked, images written through the placer the daemon
//! writes them with, the power cut at any effect of a placing, the daemon started again from
//! whatever the disk holds, another blaze's image left in its place, and at the end a byte of
//! the file changed under it.
//!
//! Three databases are asked the same keys over the same files: the one that restarts, a warm
//! one that never does, and a fresh one made for the ask. Their answers are the model's, which
//! is a function of the files alone.

use std::sync::Arc;

use blaze_core::{
    Driver,
    sim::{Faults, Sim},
    wait::Deadline,
};
use blaze_image::{
    place::{Outcome, Placer, Plant as PlacePlant},
    serial,
};
use hegel::generators as gs;
use hegel::{Hegel, Settings, TestCase};

use super::*;

/// The files a view may hold.
const NAMES: [&str; 4] = ["a", "b", "c", "d"];

/// What a file holds: nothing there, or one of these texts.
const TEXTS: [Option<&str>; 5] = [None, Some(""), Some("x"), Some("abc"), Some("abcdefg")];

/// Each file's text, as an index into [`TEXTS`].
type Files = [u8; NAMES.len()];

/// The reference model: what [`Double`] of file `file` is over `files`, from the files alone.
fn model(files: &Files, file: u8) -> u64 {
    match TEXTS[files[file as usize] as usize] {
        Some(text) => (text.len() as u64).wrapping_mul(2),
        None => u64::MAX.wrapping_mul(2),
    }
}

fn files_of(files: &Files) -> Vec<(&'static str, &'static str)> {
    NAMES.iter().zip(files).filter_map(|(name, text)| Some((*name, TEXTS[*text as usize]?))).collect()
}

/// [`Double`] of file `file` over `files` as `db` answers it, in a view opened for the ask.
fn asked(db: &Database, tree: &MemTree, files: &Files, file: u8) -> Result<u64, DbError> {
    let at = view(db, tree, &files_of(files));
    let answer = db.pin().get::<Double>(at, &NAMES[file as usize].to_owned());
    db.close(at);
    answer
}

/// What interrupts an image's placing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stop {
    Nothing,
    /// The power goes once this many of its effects completed.
    Crash(u8),
    /// The power goes after the placing ended.
    CrashAfter,
}

#[derive(Clone, Copy, Debug)]
enum Op {
    /// The file takes the text.
    Edit { file: u8, text: u8 },
    /// [`Double`] of the file is asked of every database.
    Ask { file: u8 },
    /// The database's image is written and placed; once placed and not cut, the database
    /// settles onto it when `settles`.
    Image { stop: Stop, settles: bool },
    /// The daemon stops with no image of its own and starts from what the disk holds.
    Restart,
    /// Another blaze's image (database sections of another schema) is at the file, and the
    /// daemon starts.
    Foreign,
}

#[hegel::composite]
fn op(tc: &TestCase) -> Op {
    let file = tc.draw(gs::integers::<u8>().max_value(NAMES.len() as u8 - 1));
    match tc.draw(gs::integers::<u8>().max_value(15)) {
        0..5 => Op::Edit { file, text: tc.draw(gs::integers::<u8>().max_value(TEXTS.len() as u8 - 1)) },
        5..10 => Op::Ask { file },
        10..14 => Op::Image {
            stop: match tc.draw(gs::integers::<u8>().max_value(5)) {
                0..3 => Stop::Nothing,
                3 | 4 => Stop::Crash(tc.draw(gs::integers::<u8>().max_value(8))),
                _ => Stop::CrashAfter,
            },
            settles: tc.draw(gs::integers::<u8>().max_value(2)) != 0,
        },
        14 => Op::Restart,
        _ => Op::Foreign,
    }
}

/// A safeguard left out of a run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Plant {
    None,
    /// A database over an image takes a kept memo unverified
    /// ([`Database::plant_trusting_restored`]).
    TrustsRestored,
    /// The image's clone is renamed over the file without its sync ([`PlacePlant::NoSync`]).
    NoSync,
}

/// The effects a placing may take before the run calls it stuck (rule bounded-progress).
const MOST_EFFECTS: usize = 64;

fn file() -> FileName {
    FileName::new("state/db/image").expect("a name")
}

fn placer(known: Option<Vec<u8>>, plant: Plant) -> Placer<Vec<u8>> {
    let mut placer = Placer::new(file(), Tag(1), known);
    if plant == Plant::NoSync {
        placer.plant = PlacePlant::NoSync;
    }
    placer
}

/// An image of another blaze: its memos section is of a schema this one does not read.
fn foreign() -> Vec<u8> {
    let mut other = Builder::new();
    other.add(SectionId::DB_MEMOS, blaze_format::schema("blaze v2 db memos 1"), &[]);
    other.finish().expect("a small image")
}

/// A database with nothing in it, as a start with no image makes.
fn empty(tree: &Arc<MemTree>) -> Database {
    let mut db = Database::new(config(tree));
    register(&mut db);
    db
}

/// How a start ended.
enum Start {
    /// It opened the image at the file.
    Restored(Database),
    /// By a rule, said in the start's log: no image, or another blaze's.
    Empty(Database),
}

/// A start from `bytes`, the image's file as the disk holds it: the database over it, an
/// empty one by a rule, or the typed refusal.
fn start(tree: &Arc<MemTree>, bytes: Option<&[u8]>) -> Result<Start, String> {
    let Some(bytes) = bytes else { return Ok(Start::Empty(empty(tree))) };
    let image = Image::open(mapping(bytes)).map_err(|error| format!("the image does not open: {error}"))?;
    match Database::open_image(config(tree), Arc::new(image)) {
        Ok(mut db) => {
            register(&mut db);
            Ok(Start::Restored(db))
        }
        Err(DbError::Refused(_)) => Ok(Start::Empty(empty(tree))),
        Err(error) => Err(format!("the database's sections: {error}")),
    }
}

/// Runs `ops` on a simulation of `seed` with `plant` planted, then changes byte `rot` of the
/// image's file (where there is one) and starts once more. `Err` says the first step at which
/// an answer is not the model's, a crash alone left a file that is neither the image before
/// the placing nor the one placed, a start refused an image no fault touched, or the changed
/// byte gave a wrong answer or a panic.
fn restores_as_the_model_says(ops: &[Op], seed: u64, rot: u64, plant: Plant) -> Result<(), String> {
    reset();
    let mut sim = Sim::new(seed, Faults::default());
    let tree = Arc::new(MemTree::default());
    let warm = empty(&tree);
    let mut db = empty(&tree);
    let mut placing = placer(None, plant);
    // The model: each file's text, and the image the file must hold, none before the first.
    let mut files: Files = [0; NAMES.len()];
    let mut on_disk: Option<Vec<u8>> = None;
    let planted = |db: &Database| {
        if plant == Plant::TrustsRestored {
            db.plant_trusting_restored();
        }
    };
    for (at, op) in ops.iter().enumerate() {
        // A start from what the disk holds, where the step ends in one.
        let mut starts = false;
        match *op {
            Op::Edit { file, text } => files[file as usize] = text,
            Op::Ask { file } => {
                let want = model(&files, file);
                let fresh = empty(&tree);
                for (which, db) in [("restored", &db), ("warm", &warm), ("fresh", &fresh)] {
                    match asked(db, &tree, &files, file) {
                        Ok(got) if got == want => {}
                        got => {
                            return Err(format!(
                                "step {at} ({op:?}): the {which} database answered {got:?} where \
                                 the files {files:?} give {want}"
                            ));
                        }
                    }
                }
            }
            Op::Image { stop, settles } => {
                let (bytes, _) = {
                    let mut builder = Builder::new();
                    let mut live = Live::new();
                    db.write_image(&mut builder, &mut live)
                        .map_err(|error| format!("step {at}: no image was made: {error}"))?;
                    (builder.finish().expect("a small image"), live)
                };
                let mut effects = Vec::new();
                placing.place(bytes.clone(), &serial, &mut effects);
                let (mut handled, mut outcome, mut crashed) = (0usize, None, false);
                while outcome.is_none() {
                    if stop == Stop::Crash(handled as u8) {
                        sim.crash();
                        crashed = true;
                        break;
                    }
                    if handled == MOST_EFFECTS {
                        return Err(format!(
                            "step {at}: the placing took {handled} effects and has not ended"
                        ));
                    }
                    sim.submit(&mut effects);
                    let mut done = Vec::new();
                    sim.poll(&mut done, Deadline::NEVER);
                    let Some(completion) = done.pop() else {
                        return Err(format!("step {at}: the effect in flight never completed"));
                    };
                    handled += 1;
                    outcome = placing.complete(completion.done, &mut effects);
                }
                if !crashed && stop == Stop::CrashAfter {
                    sim.crash();
                    crashed = true;
                }
                let found = sim.exists(&file()).then(|| sim.contents(&file()));
                let (is_new, is_old) = (found.as_ref() == Some(&bytes), found == on_disk);
                let whole = match &outcome {
                    Some(Outcome::Placed(_)) => is_new,
                    Some(Outcome::LetGo) => is_old,
                    Some(Outcome::Failed(_)) | None => is_old || is_new,
                };
                if !whole {
                    return Err(format!(
                        "step {at} ({op:?}): the placing ended as {outcome:?} and the file holds \
                         {} bytes that are neither the image before it nor the one placed",
                        found.as_ref().map_or(0, Vec::len)
                    ));
                }
                on_disk = found;
                match crashed {
                    true => starts = true,
                    false if matches!(outcome, Some(Outcome::Placed(_))) => {
                        // clone: the test's copy, as the mapping of the file placed
                        placing.holds(bytes.clone());
                        if settles {
                            let image = Image::open(mapping(&bytes))
                                .map_err(|error| format!("step {at}: the image placed does not open: {error}"))?;
                            let mut effects = Vec::new();
                            db.settle(Arc::new(image), &mut effects)
                                .map_err(|error| format!("step {at}: the database did not settle: {error}"))?;
                            planted(&db);
                        }
                    }
                    false => {}
                }
            }
            Op::Restart => starts = true,
            Op::Foreign => {
                let bytes = foreign();
                sim.change(&file(), &bytes);
                on_disk = Some(bytes);
                starts = true;
            }
        }
        if starts {
            // No fault but the power touched the file: a refusal here is the bug.
            let started = start(&tree, on_disk.as_deref())
                .map_err(|why| format!("step {at} ({op:?}): the start was refused: {why}"))?;
            // Whether the file is another blaze's image: left there by this step or an
            // earlier one, and not yet replaced by an image of this database's.
            let foreign = on_disk.as_deref() == Some(foreign().as_slice());
            db = match (started, foreign, on_disk.is_some()) {
                (Start::Restored(db), false, true) => db,
                (Start::Empty(db), true, _) | (Start::Empty(db), false, false) => db,
                (Start::Restored(_), true, _) => {
                    return Err(format!("step {at}: another blaze's image was restored"));
                }
                (Start::Empty(_), false, true) => {
                    return Err(format!(
                        "step {at} ({op:?}): the start left its own image unused and started empty"
                    ));
                }
                (Start::Restored(_), false, false) => unreachable!("restored from no image"),
            };
            planted(&db);
            // clone: the model's copy, as the start's mapping
            placing = placer(on_disk.clone(), plant);
        }
    }
    // A byte of the file changed under it: every answer is the model's or a typed refusal.
    let Some(mut rotten) = on_disk else { return Ok(()) };
    let at = (rot % rotten.len() as u64) as usize;
    rotten[at] ^= 0x40;
    let read = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<(), String> {
        let Ok(Start::Restored(db) | Start::Empty(db)) = start(&tree, Some(&rotten)) else {
            return Ok(());
        };
        for file in 0..NAMES.len() as u8 {
            match asked(&db, &tree, &files, file) {
                Ok(got) if got == model(&files, file) => {}
                Err(DbError::Corrupt(_)) => {}
                got => {
                    return Err(format!(
                        "byte {at} of the image changed: file {file} answered {got:?} where the \
                         files {files:?} give {}",
                        model(&files, file)
                    ));
                }
            }
        }
        Ok(())
    }));
    match read {
        Ok(read) => read,
        Err(_) => Err(format!("byte {at} of the image changed: the start or an answer panicked")),
    }
}

/// Runs `property` over `cases` cases, or `HEGEL_TEST_CASES` of them where that is set.
fn run_cases(cases: u64, property: impl FnMut(TestCase)) {
    let cases =
        std::env::var("HEGEL_TEST_CASES").ok().and_then(|it| it.parse().ok()).unwrap_or(cases);
    Hegel::new(property).settings(Settings::new().test_cases(cases).database(None)).run();
}

/// The invariant: after any run of edits, asks, images placed, settles, power cuts at any
/// effect of a placing and starts, a database restored from the image's file answers every
/// key as a warm database and a fresh one do, which is what the files give; the file is always
/// one image whole (the one placed, or the one before it); a start refuses no image that only
/// the power touched and restores no other blaze's; and with one byte of the file changed,
/// every answer is still the files' or a typed refusal, never a panic.
/// `HEGEL_TEST_CASES=<n>` runs it longer.
#[test]
fn a_restored_database_answers_as_a_warm_one_and_a_fresh_one() {
    run_cases(128, |tc| {
        let ops = tc.draw(gs::vecs(op()).min_size(1).max_size(24));
        let seed = tc.draw(gs::integers());
        let rot = tc.draw(gs::integers());
        if let Err(why) = restores_as_the_model_says(&ops, seed, rot, Plant::None) {
            panic!("seed {seed}, rot {rot}, {ops:?}: {why}");
        }
    });
}

/// The planted-bug control: a restored memo used after its input changed. The directed case
/// (a key asked, the image written, a restart, the file edited, the key asked again) passes
/// with nothing planted, and with the verification left out the restored database answers
/// what the file gave before the edit.
#[test]
fn the_oracle_catches_a_restored_memo_used_after_its_input_changed() {
    let directed = [
        Op::Edit { file: 0, text: 3 },
        Op::Ask { file: 0 },
        Op::Image { stop: Stop::Nothing, settles: false },
        Op::Restart,
        Op::Edit { file: 0, text: 4 },
        Op::Ask { file: 0 },
    ];
    assert_eq!(restores_as_the_model_says(&directed, 1, 0, Plant::None), Ok(()));
    let why = restores_as_the_model_says(&directed, 1, 0, Plant::TrustsRestored)
        .expect_err("the oracle missed the planted stale memo");
    assert!(why.contains("step 5") && why.contains("the restored database"), "{why}");
}

/// The seeds the crash control is run over: a crash keeps a drawn prefix of what was not
/// synced, so one seed may keep all of it.
const SEEDS: u64 = 32;

/// The planted-bug control: the image renamed into place without its sync. The directed case
/// (an image placed and settled onto, more asked, a second image placed over it and the
/// power cut) passes on every seed with nothing planted, and with the sync left out some seed
/// leaves a file that is neither image.
#[test]
fn the_oracle_catches_an_image_committed_before_its_bytes_are_durable() {
    let directed = [
        Op::Edit { file: 0, text: 3 },
        Op::Ask { file: 0 },
        Op::Image { stop: Stop::Nothing, settles: true },
        Op::Edit { file: 1, text: 4 },
        Op::Edit { file: 2, text: 2 },
        Op::Ask { file: 1 },
        Op::Ask { file: 2 },
        Op::Image { stop: Stop::CrashAfter, settles: false },
        Op::Ask { file: 1 },
    ];
    for seed in 0..SEEDS {
        let ran = restores_as_the_model_says(&directed, seed, 0, Plant::None);
        assert_eq!(ran, Ok(()), "seed {seed}: the placing as it is must pass");
    }
    let caught = (0..SEEDS)
        .find_map(|seed| restores_as_the_model_says(&directed, seed, 0, Plant::NoSync).err());
    let why = caught.expect("the oracle missed the planted rename without its sync");
    assert!(why.contains("neither the image before it nor the one placed"), "{why}");
}
