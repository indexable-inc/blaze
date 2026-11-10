//! The placer on blaze-core's simulation (rule dst-not-review): images placed one after
//! another over a disk that fails effects, refuses clones, loses power and tears what was not
//! synced, with placings let go part way; after every step the image's file is read back and
//! held to the model.

use blaze_core::{
    Driver,
    sim::{Faults, Sim},
    wait::Deadline,
};
use blaze_format::{SchemaHash, schema};
use blaze_ids::rng::SplitMix64 as Rng;
use hegel::generators as gs;
use hegel::{Hegel, Settings, TestCase};

use super::*;
use crate::{Builder, Image, SectionId, serial};

const IDS: [SectionId; 3] = [SectionId::VFS_BLOBS, SectionId::DB_BLOB, SectionId::PLANE_HISTORY];

/// The lengths a section takes: none, a few bytes, a unit and a bit, and several units, so
/// images share whole units, grow, shrink and move their later sections.
const LENS: [usize; 5] = [0, 37, UNIT + 4464, 3 * UNIT + 5, 5 * UNIT + 100];

fn section_schema() -> SchemaHash {
    schema("place test section 1")
}

/// `len` bytes of noise by `seed`: a longer run of one seed starts with a shorter one's bytes.
fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut rng = Rng::new(seed);
    (0..len).map(|_| rng.next_u64() as u8).collect()
}

/// An image's sections: each one's length (an index into [`LENS`]) and its noise's seed.
type Shape = [(u8, u8); 3];

/// The image of `shape`, as a fresh build writes it whole.
fn image(shape: Shape) -> Vec<u8> {
    let mut builder = Builder::new();
    for (id, (len, seed)) in IDS.iter().zip(shape) {
        builder.add(*id, section_schema(), &noise(LENS[len as usize], u64::from(seed)));
    }
    builder.finish().unwrap()
}

/// What interrupts a step's placing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stop {
    Nothing,
    /// The placing is let go once this many of its effects completed.
    LetGo(u8),
    /// The power goes once this many of its effects completed.
    Crash(u8),
    /// The power goes after the placing ended.
    CrashAfter,
}

/// One image placed.
#[derive(Clone, Copy, Debug)]
struct Step {
    shape: Shape,
    stop: Stop,
    /// The file is mapped once placed, so the next image shares its blocks; without it the
    /// next is written whole (a map that failed).
    maps: bool,
}

#[hegel::composite]
fn step(tc: &TestCase) -> Step {
    let part = |tc: &TestCase| {
        (tc.draw(gs::integers::<u8>().max_value(4)), tc.draw(gs::integers::<u8>().max_value(2)))
    };
    let shape = [part(tc), part(tc), part(tc)];
    let stop = match tc.draw(gs::integers::<u8>().max_value(9)) {
        0..5 => Stop::Nothing,
        5 | 6 => Stop::LetGo(tc.draw(gs::integers::<u8>().max_value(8))),
        7 | 8 => Stop::Crash(tc.draw(gs::integers::<u8>().max_value(8))),
        _ => Stop::CrashAfter,
    };
    Step { shape, stop, maps: tc.draw(gs::integers::<u8>().max_value(3)) != 0 }
}

/// The fault classes of a run, drawn as a swarm.
#[derive(Clone, Copy, Debug, Default)]
struct Swarm {
    fail_one_in: u32,
    no_clone: bool,
    /// The room left under the image's directory after its first image, none for a disk that
    /// never fills.
    room: Option<u64>,
}

fn name(it: &str) -> FileName {
    FileName::new(it).unwrap()
}

/// The effects a placing may take before the test calls it stuck (rule bounded-progress): a
/// placing asks for at most three before its writes, one a run, and three after.
const MOST_EFFECTS: usize = 64;

/// Places `steps`' images in order on a simulation of `seed` with `swarm`'s faults and
/// `plant` planted; `Err` says the first step after which the image's file is not what the
/// model says it must be.
fn places_as_the_model_says(
    steps: &[Step],
    seed: u64,
    swarm: Swarm,
    plant: Plant,
) -> Result<(), String> {
    let faults =
        Faults { fail_one_in: swarm.fail_one_in, no_clone: swarm.no_clone, ..Faults::default() };
    let mut sim = Sim::new(seed, faults);
    let file = name("state/db/image");
    let placer = |known: Option<Vec<u8>>| {
        // clone: a name, once a start
        let mut placer = Placer::new(file.clone(), Tag(1), known);
        placer.plant = plant;
        placer
    };
    let mut placing = placer(None);
    // The model: the image the file must hold, none before the first is placed.
    let mut on_disk: Option<Vec<u8>> = None;
    for (at, step) in steps.iter().enumerate() {
        let bytes = image(step.shape);
        let mut effects = Vec::new();
        placing.place(bytes.clone(), &serial, &mut effects);
        let (mut handled, mut outcome, mut crashed) = (0usize, None, false);
        while outcome.is_none() {
            if step.stop == Stop::Crash(handled as u8) {
                sim.crash();
                crashed = true;
                break;
            }
            if step.stop == Stop::LetGo(handled as u8) {
                placing.let_go();
            }
            if handled == MOST_EFFECTS {
                return Err(format!("step {at}: the placing took {handled} effects and has not ended"));
            }
            if effects.len() != 1 {
                return Err(format!(
                    "step {at}: {} effects asked at once where the placing has one in flight",
                    effects.len()
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
        if !crashed && step.stop == Stop::CrashAfter {
            sim.crash();
            crashed = true;
        }
        let found = sim.exists(&file).then(|| sim.contents(&file));
        let is_new = found.as_ref() == Some(&bytes);
        let is_old = found == on_disk;
        let said = |what: &str| {
            let len = |it: &Option<Vec<u8>>| it.as_ref().map_or(0, Vec::len);
            format!(
                "step {at} ({step:?}): {what}: the file holds {} bytes that are neither the image \
                 before it ({} bytes) nor the one placed ({} bytes)",
                len(&found),
                len(&on_disk),
                bytes.len()
            )
        };
        match (&outcome, crashed) {
            // Placed is durable: a crash after it finds the new image.
            (Some(Outcome::Placed(wrote)), _) => {
                if !is_new {
                    return Err(said("the placing said the image is placed"));
                }
                if wrote.len != bytes.len() as u64 || wrote.written > wrote.len {
                    return Err(format!("step {at}: an image of {} bytes told as {wrote:?}", bytes.len()));
                }
            }
            // Let go before its commit: the file is the image it was.
            (Some(Outcome::LetGo), _) => {
                if !is_old {
                    return Err(said("the placing was let go"));
                }
            }
            // Failed, or cut by a crash: the old image or the new, whole.
            (Some(Outcome::Failed(_)) | None, _) => {
                if !is_old && !is_new {
                    return Err(said("the placing failed or the power went"));
                }
            }
        }
        on_disk = found;
        // The image there opens and every group of it is the one its spine names.
        if let Some(bytes) = &on_disk {
            // clone: the test's copy, to open
            let opened = Image::open(bytes.clone())
                .map_err(|error| format!("step {at}: the image's file does not open: {error}"))?;
            let corrupt = opened.verify();
            if !corrupt.is_empty() {
                return Err(format!("step {at}: the image's file is corrupt: {corrupt:?}"));
            }
        }
        if crashed {
            // A start: the file as the disk has it is mapped, and the faults begin again.
            // clone: the model's copy, as the start's mapping
            placing = placer(on_disk.clone());
        } else if step.maps && matches!(outcome, Some(Outcome::Placed(_))) {
            placing.holds(bytes);
        }
        if at == 0
            && let Some(room) = swarm.room
        {
            sim.fill(name("state/db"), room);
        }
    }
    Ok(())
}

/// Runs `property` over `cases` cases, or `HEGEL_TEST_CASES` of them where that is set.
fn run_cases(cases: u64, property: impl FnMut(TestCase)) {
    let cases =
        std::env::var("HEGEL_TEST_CASES").ok().and_then(|it| it.parse().ok()).unwrap_or(cases);
    Hegel::new(property).settings(Settings::new().test_cases(cases).database(None)).run();
}

/// The invariant: after any run of images placed, let go, failed and cut by a crash, the
/// image's file holds one image whole, byte for byte what a fresh build wrote: the one placed
/// once the placing said so (and after a crash then), the one before it when the placing was
/// let go, and one of the two when it failed or the power went; and it opens and verifies.
/// `HEGEL_TEST_CASES=<n>` runs it longer.
#[test]
fn the_images_file_is_always_one_image_whole() {
    run_cases(96, |tc| {
        let steps = tc.draw(gs::vecs(step()).min_size(1).max_size(8));
        let swarm = Swarm {
            fail_one_in: [0, 0, 3, 9][tc.draw(gs::integers::<u8>().max_value(3)) as usize],
            no_clone: tc.draw(gs::integers::<u8>().max_value(3)) == 0,
            room: (tc.draw(gs::integers::<u8>().max_value(3)) == 0)
                .then(|| tc.draw(gs::integers::<u64>().max_value(12 * UNIT as u64))),
        };
        let seed = tc.draw(gs::integers());
        if let Err(why) = places_as_the_model_says(&steps, seed, swarm, Plant::None) {
            panic!("seed {seed}, {swarm:?}, {steps:?}: {why}");
        }
    });
}

fn plain(shape: Shape, stop: Stop, maps: bool) -> Step {
    Step { shape, stop, maps }
}

/// The seeds a directed control is run over: a crash keeps a drawn prefix of what was not
/// synced, so one seed may keep all of it.
const SEEDS: u64 = 32;

/// The planted-bug control: a clone renamed over the image without its sync is the new image
/// in name only. The directed case (an image placed and mapped, a second placed over it, the
/// power cut) passes on every seed with nothing planted, and with the sync left out some seed
/// tears the writes the rename named.
#[test]
fn the_oracle_catches_a_rename_without_its_sync() {
    let directed = [
        plain([(4, 0), (3, 0), (2, 0)], Stop::Nothing, true),
        plain([(4, 0), (3, 1), (4, 2)], Stop::CrashAfter, true),
    ];
    for seed in 0..SEEDS {
        let ran = places_as_the_model_says(&directed, seed, Swarm::default(), Plant::None);
        assert_eq!(ran, Ok(()), "seed {seed}: the placer as it is must pass");
    }
    let caught = (0..SEEDS).find_map(|seed| {
        places_as_the_model_says(&directed, seed, Swarm::default(), Plant::NoSync).err()
    });
    let why = caught.expect("the oracle missed the planted rename without its sync");
    assert!(why.contains("the placing said the image is placed"), "{why}");
}

/// The planted-bug control: the file's bytes kept as known across a rename no map followed
/// are the image before it, and the next image shares units the file no longer holds. The
/// directed case (three images, the second never mapped) passes with nothing planted and with
/// the plant leaves a file that is no image at all.
#[test]
fn the_oracle_catches_blocks_shared_from_bytes_the_file_no_longer_holds() {
    let directed = [
        plain([(4, 0), (3, 0), (2, 0)], Stop::Nothing, true),
        plain([(4, 1), (3, 1), (2, 1)], Stop::Nothing, false),
        plain([(4, 0), (3, 0), (2, 2)], Stop::Nothing, true),
    ];
    let ran = places_as_the_model_says(&directed, 1, Swarm::default(), Plant::None);
    assert_eq!(ran, Ok(()), "the placer as it is must pass");
    let why = places_as_the_model_says(&directed, 1, Swarm::default(), Plant::KeepsKnown)
        .expect_err("the oracle missed the planted stale bytes");
    assert!(why.contains("step 2") && why.contains("the placing said the image is placed"), "{why}");
}

/// An image that differs from the one before it in one section writes that section's units,
/// the spine and the CRC table, and shares the rest; one placed over nothing known is written
/// whole, and so is one whose clone the file system refuses, which is told.
#[test]
fn only_the_units_that_differ_are_written() {
    let file = name("state/db/image");
    let run = |placer: &mut Placer<Vec<u8>>, sim: &mut Sim, bytes: &[u8]| {
        let mut effects = Vec::new();
        // clone: the image, which the placing owns
        placer.place(bytes.to_vec(), &serial, &mut effects);
        // alloc: the kinds of effect asked, for the test to read
        let mut asked = Vec::new();
        loop {
            asked.extend(effects.iter().map(|it| match it {
                Effect::Write { .. } => "write",
                Effect::Remove { .. } => "remove",
                Effect::Clone { .. } => "clone",
                Effect::Truncate { .. } => "truncate",
                Effect::WriteAt { .. } => "write at",
                Effect::Sync { .. } => "sync",
                Effect::Rename { .. } => "rename",
                _ => "another",
            }));
            sim.submit(&mut effects);
            let mut done = Vec::new();
            sim.poll(&mut done, Deadline::NEVER);
            let done = done.pop().expect("the effect in flight completes").done;
            if let Some(outcome) = placer.complete(done, &mut effects) {
                return (outcome, asked);
            }
        }
    };
    let mut sim = Sim::new(3, Faults::default());
    // clone: a name, once
    let mut placer = Placer::new(file.clone(), Tag(1), None);
    let first = image([(4, 0), (4, 1), (2, 0)]);
    let (outcome, asked) = run(&mut placer, &mut sim, &first);
    let whole = Wrote { len: first.len() as u64, written: first.len() as u64, unshared: None };
    assert_eq!((outcome, asked), (Outcome::Placed(whole), vec!["write"]));
    // clone: the test's copy, as the mapping
    placer.holds(first.clone());

    // The last section alone changes, at its length: the first two sections' ten units and
    // more are shared.
    let second = image([(4, 0), (4, 1), (2, 7)]);
    assert_eq!(second.len(), first.len());
    let (outcome, asked) = run(&mut placer, &mut sim, &second);
    let Outcome::Placed(wrote) = outcome else { panic!("the second image: {outcome:?}") };
    assert_eq!(wrote.unshared, None);
    assert!(wrote.written <= 5 * UNIT as u64, "one section, the spine and the CRCs: {wrote:?}");
    assert!(wrote.len - wrote.written >= 10 * UNIT as u64, "two sections shared: {wrote:?}");
    assert_eq!(asked.first(), Some(&"remove"));
    assert_eq!(asked[1], "clone");
    assert_eq!(asked[asked.len() - 2..], ["sync", "rename"]);
    assert!(asked.iter().all(|it| *it != "write" && *it != "truncate"), "{asked:?}");
    assert_eq!(sim.contents(&file), second);
    assert!(!sim.exists(&name("state/db/image.next")), "the clone is the image now");

    // A file system that shares no blocks: the image whole, and why.
    let mut sim = Sim::new(3, Faults { no_clone: true, ..Faults::default() });
    // clone: a name, once
    let mut placer = Placer::new(file.clone(), Tag(1), None);
    run(&mut placer, &mut sim, &first);
    // clone: the test's copy, as the mapping
    placer.holds(first.clone());
    let (outcome, asked) = run(&mut placer, &mut sim, &second);
    let Outcome::Placed(wrote) = outcome else { panic!("the unshared image: {outcome:?}") };
    assert!(wrote.unshared.is_some() && wrote.written == wrote.len, "{wrote:?}");
    assert_eq!(asked, ["remove", "clone", "write"]);
    assert_eq!(sim.contents(&file), second);
}

/// A placing let go ends within the effect in flight and one removal, leaves the file the
/// image it was, and leaves no clone; one whose commit is asked is not let go.
#[test]
fn a_placing_let_go_leaves_the_file_as_it_was_and_no_clone() {
    let file = name("state/db/image");
    let mut sim = Sim::new(5, Faults::default());
    // clone: a name, once
    let mut placer = Placer::new(file.clone(), Tag(1), None);
    let first = image([(4, 0), (4, 1), (2, 0)]);
    let mut effects = Vec::new();
    // clone: the image, which the placing owns
    placer.place(first.clone(), &serial, &mut effects);
    assert!(!placer.let_go(), "a whole write is its own commit");
    let mut step = |placer: &mut Placer<Vec<u8>>, effects: &mut Vec<Effect>| {
        sim.submit(effects);
        let mut done = Vec::new();
        sim.poll(&mut done, Deadline::NEVER);
        placer.complete(done.pop().expect("the effect in flight completes").done, effects)
    };
    assert!(matches!(step(&mut placer, &mut effects), Some(Outcome::Placed(_))));
    // clone: the test's copy, as the mapping
    placer.holds(first.clone());

    placer.place(image([(4, 2), (4, 2), (2, 2)]), &serial, &mut effects);
    assert_eq!(step(&mut placer, &mut effects), None, "the old clone's removal");
    assert_eq!(step(&mut placer, &mut effects), None, "the clone");
    assert!(placer.let_go(), "nothing is committed yet");
    assert_eq!(step(&mut placer, &mut effects), None, "the effect in flight ends");
    assert!(matches!(effects[..], [Effect::Remove { .. }]), "{effects:?}");
    assert_eq!(step(&mut placer, &mut effects), Some(Outcome::LetGo));
    assert!(!placer.busy() && effects.is_empty());
    drop(step);
    assert_eq!(sim.contents(&file), first);
    assert!(!sim.exists(&name("state/db/image.next")));
}
