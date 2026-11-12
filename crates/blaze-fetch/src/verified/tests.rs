//! The fetch core under a simulation: sources that are slow, never answer, answer short, lie,
//! fail, lack objects, and come and go, their answers delivered in any order on a virtual clock.

use std::collections::BTreeMap;

use hegel::generators as gs;
use hegel::{Hegel, Settings, TestCase};

use super::*;

/// How a simulated source answers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Behaviour {
    /// The bytes asked for.
    Honest,
    /// Never answers: every ask of it runs to its deadline.
    Silent,
    /// The bytes asked for, cut short.
    Short,
    /// Bytes of the right length with one byte changed.
    Liar,
    /// Honest but for every `n`th answer, which is a lie.
    LiesEvery(u8),
    /// Honest but for every `n`th answer, which fails.
    FailsEvery(u8),
    /// Has only the objects at even places.
    HasHalf,
}

#[derive(Debug)]
enum Event {
    /// The answer of the `n`th ask in flight (by the order asked, wrapping) arrives.
    Answer(u8),
    /// The clock moves on by this many seconds.
    Tick(u8),
    /// A new honest source appears.
    Appears,
    /// The `n`th source there is (wrapping) goes away.
    Disappears(u8),
}

#[derive(Debug)]
struct Case {
    sizes: Vec<u32>,
    sources: Vec<Behaviour>,
    events: Vec<Event>,
    seed: u64,
}

fn behaviour(tc: &TestCase) -> Behaviour {
    match tc.draw(gs::integers::<u8>().max_value(7)) {
        0 | 1 => Behaviour::Honest,
        2 => Behaviour::Silent,
        3 => Behaviour::Short,
        4 => Behaviour::Liar,
        5 => Behaviour::LiesEvery(tc.draw(gs::integers().min_value(2).max_value(5))),
        6 => Behaviour::FailsEvery(tc.draw(gs::integers().min_value(2).max_value(5))),
        _ => Behaviour::HasHalf,
    }
}

fn event(tc: &TestCase) -> Event {
    match tc.draw(gs::integers::<u8>().max_value(19)) {
        0..14 => Event::Answer(tc.draw(gs::integers())),
        14..17 => Event::Tick(tc.draw(gs::integers().max_value(25))),
        17 => Event::Appears,
        _ => Event::Disappears(tc.draw(gs::integers())),
    }
}

fn case(tc: &TestCase) -> Case {
    Case {
        sizes: tc.draw(gs::vecs(gs::integers::<u32>().max_value(5 * CHUNK as u32)).min_size(1).max_size(3)),
        sources: (0..tc.draw(gs::integers::<usize>().max_value(4))).map(|_| behaviour(tc)).collect(),
        events: (0..tc.draw(gs::integers::<usize>().max_value(120))).map(|_| event(tc)).collect(),
        seed: tc.draw(gs::integers()),
    }
}

/// The bytes of the object numbered `n` of `len` bytes: every object and chunk its own.
fn object(n: usize, len: u32, seed: u64) -> Vec<u8> {
    let mut state = seed ^ (n as u64 + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    (0..len)
        .map(|_| {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (state >> 33) as u8
        })
        .collect()
}

struct World {
    truth: Vec<Vec<u8>>,
    wants: Vec<Want>,
    /// Each source there is, with how it answers and how many asks it has answered.
    sources: Vec<(Source, Behaviour, u32)>,
    next_source: u32,
    /// Asks in flight, in the order asked.
    flying: Vec<(Ask, Source, Part)>,
    /// What the driver was told to keep, by object and offset.
    kept: BTreeMap<(usize, u64), Vec<u8>>,
    whole: Vec<bool>,
    now: Duration,
}

impl World {
    fn place(&self, root: &ContentId) -> usize {
        self.wants.iter().position(|it| it.root == *root).expect("an object that was wanted")
    }

    /// What `source` answers `part` with.
    fn answer(&mut self, source: Source, part: Part) -> Option<Answer> {
        let (_, behaviour, answered) =
            self.sources.iter_mut().find(|(id, ..)| *id == source)?;
        *answered += 1;
        let (behaviour, nth) = (*behaviour, *answered);
        let (root, honest) = match part {
            Part::Hashes { root, .. } => (root, hashes_of(&self.truth[self.place(&root)])),
            Part::Chunk { root, at, len } => {
                let bytes = &self.truth[self.place(&root)];
                (root, bytes[at as usize..(at + len) as usize].to_vec())
            }
        };
        let lie = |mut bytes: Vec<u8>| match bytes.len() {
            // Nothing to change in no bytes: a liar asked for an empty part fails it.
            0 => Answer::Failed,
            len => {
                bytes[nth as usize % len] ^= 0x40;
                Answer::Bytes(bytes)
            }
        };
        Some(match behaviour {
            Behaviour::Honest => Answer::Bytes(honest),
            Behaviour::Silent => return None,
            Behaviour::Short => Answer::Bytes(honest[..honest.len() / 2].to_vec()),
            Behaviour::Liar => lie(honest),
            Behaviour::LiesEvery(n) if nth % u32::from(n) == 0 => lie(honest),
            Behaviour::FailsEvery(n) if nth % u32::from(n) == 0 => Answer::Failed,
            Behaviour::LiesEvery(_) | Behaviour::FailsEvery(_) => Answer::Bytes(honest),
            Behaviour::HasHalf if self.place(&root) % 2 == 1 => Answer::Lacks,
            Behaviour::HasHalf => Answer::Bytes(honest),
        })
    }

    /// Carries out what the core asked, checking every byte it says to keep against the
    /// object it is of: the oracle of "no unverified byte is kept".
    fn carry_out(&mut self, fetch: &mut Fetch) -> Result<(), String> {
        for effect in fetch.take_effects() {
            match effect {
                Effect::Ask { ask, source, part } => self.flying.push((ask, source, part)),
                Effect::Keep { root, at, bytes } => {
                    let place = self.place(&root);
                    let truth = &self.truth[place];
                    if truth.get(at as usize..at as usize + bytes.len()) != Some(&bytes[..]) {
                        return Err(format!(
                            "{} bytes kept at {at} of object {place} are not the object's",
                            bytes.len()
                        ));
                    }
                    if self.kept.insert((place, at), bytes).is_some() {
                        return Err(format!("the chunk at {at} of object {place} was kept twice"));
                    }
                }
                Effect::Whole { root } => {
                    let place = self.place(&root);
                    let assembled: Vec<u8> = self
                        .kept
                        .range((place, 0)..=(place, u64::MAX))
                        .flat_map(|(_, bytes)| bytes.iter().copied())
                        .collect();
                    if assembled != self.truth[place] {
                        return Err(format!("object {place} was called whole and is not"));
                    }
                    self.whole[place] = true;
                }
                Effect::Dropped { source, .. } => self.flying.retain(|(_, it, _)| *it != source),
                Effect::Progress { kept, of } => {
                    if kept > of {
                        return Err(format!("progress says {kept} bytes kept of {of}"));
                    }
                }
            }
        }
        Ok(())
    }
}

/// Runs `case`; `Err` says what the oracle found. `plant`: the core keeps chunks unchecked.
fn runs(case: &Case, plant: bool) -> Result<(), String> {
    let truth: Vec<Vec<u8>> =
        case.sizes.iter().enumerate().map(|(n, len)| object(n, *len, case.seed)).collect();
    let wants: Vec<Want> =
        truth.iter().map(|it| Want { root: root_of(it), len: it.len() as u64 }).collect();
    let mut world = World {
        whole: vec![false; truth.len()],
        truth,
        wants,
        sources: Vec::new(),
        next_source: 0,
        flying: Vec::new(),
        kept: BTreeMap::new(),
        now: Duration::ZERO,
    };
    let mut fetch = Fetch::new(world.now, &world.wants);
    fetch.plant_unchecked = plant;
    // Whether an honest source that has everything was there from the first event to the
    // last: then the fetch has to end done.
    let mut steady = None;
    for behaviour in &case.sources {
        let source = Source(world.next_source);
        world.next_source += 1;
        world.sources.push((source, *behaviour, 0));
        if *behaviour == Behaviour::Honest && steady.is_none() {
            steady = Some(source);
        }
        fetch.add_source(source);
    }
    world.carry_out(&mut fetch)?;
    for event in &case.events {
        match event {
            Event::Answer(n) if !world.flying.is_empty() => {
                let (ask, source, part) = world.flying.remove(*n as usize % world.flying.len());
                if let Some(answer) = world.answer(source, part) {
                    fetch.answered(ask, answer);
                }
            }
            Event::Answer(_) => {}
            Event::Tick(seconds) => {
                // The steady source is slow at worst, never past a deadline: what it owes
                // arrives before the clock passes one. (A source slower than that is one
                // that does not answer, which `Silent` is.)
                let passes = fetch
                    .next_deadline()
                    .is_some_and(|it| it <= world.now + Duration::from_secs(u64::from(*seconds)));
                while let (true, Some(steady)) = (passes, steady) {
                    let Some(at) = world.flying.iter().position(|(_, it, _)| *it == steady) else {
                        break;
                    };
                    let (ask, source, part) = world.flying.remove(at);
                    if let Some(answer) = world.answer(source, part) {
                        fetch.answered(ask, answer);
                    }
                    world.carry_out(&mut fetch)?;
                }
                world.now += Duration::from_secs(u64::from(*seconds));
                fetch.advance_clock(world.now);
            }
            Event::Appears => {
                let source = Source(world.next_source);
                world.next_source += 1;
                world.sources.push((source, Behaviour::Honest, 0));
                fetch.add_source(source);
            }
            Event::Disappears(n) if !world.sources.is_empty() => {
                let (source, ..) = world.sources.remove(*n as usize % world.sources.len());
                if steady == Some(source) {
                    steady = None;
                }
                fetch.remove_source(source);
            }
            Event::Disappears(_) => {}
        }
        world.carry_out(&mut fetch)?;
    }
    // The events are over: every answer still owed arrives, oldest first, and the clock runs
    // to each deadline nothing answers. Bounded: each round answers an ask or passes a
    // deadline, and the core asks each part of each source a bounded number of times.
    for _ in 0..100_000 {
        if *fetch.outcome() != Outcome::Fetching {
            break;
        }
        if let Some((ask, source, part)) = (!world.flying.is_empty()).then(|| world.flying.remove(0)) {
            if let Some(answer) = world.answer(source, part) {
                fetch.answered(ask, answer);
            }
        } else if let Some(deadline) = fetch.next_deadline() {
            world.now = world.now.max(deadline);
            fetch.advance_clock(world.now);
        } else if world.next_source == 0 {
            // No source was ever given: it waits for one, as it says it does.
            return Ok(());
        } else {
            return Err("the fetch is neither done nor failed, and waits for nothing".into());
        }
        world.carry_out(&mut fetch)?;
    }
    match fetch.outcome() {
        Outcome::Fetching => Err("the fetch did not end within its bound".into()),
        // Two wants of one root (two empty objects) are one object.
        Outcome::Done if world.wants.iter().all(|it| world.whole[world.place(&it.root)]) => Ok(()),
        Outcome::Done => Err("the fetch ended done with an object not whole".into()),
        Outcome::Failed { why, .. } if steady.is_some() => {
            Err(format!("an honest source had every object throughout, and the fetch failed: {why}"))
        }
        Outcome::Failed { .. } => Ok(()),
    }
}

fn run_cases(cases: u64, property: impl FnMut(TestCase)) {
    let cases =
        std::env::var("HEGEL_TEST_CASES").ok().and_then(|it| it.parse().ok()).unwrap_or(cases);
    Hegel::new(property).settings(Settings::new().test_cases(cases).database(None)).run();
}

/// The invariant: whatever the sources do (answer slowly or never, short, with wrong bytes,
/// with failures, lacking objects, appearing and going away) and in whatever order their
/// answers arrive, no byte is kept that is not the object's, an object called whole is whole,
/// the fetch ends (done, or failed naming an object), and it ends done whenever one honest
/// source had every object throughout. `HEGEL_TEST_CASES=<n>` runs it longer.
#[test]
fn no_unverified_byte_is_kept_and_an_honest_source_is_enough() {
    run_cases(300, |tc| {
        let case = case(&tc);
        if let Err(why) = runs(&case, false) {
            panic!("{case:?}: {why}");
        }
    });
}

/// The planted-bug control: a core that keeps chunks without checking them
/// (`Fetch::plant_unchecked`) keeps a liar's bytes, and the oracle says so; the same case
/// passes with nothing planted, the liar dropped and its chunks fetched from the honest source.
#[test]
fn the_oracle_catches_a_chunk_kept_unchecked() {
    let directed = || Case {
        sizes: vec![3 * CHUNK as u32 + 17],
        sources: vec![Behaviour::Honest, Behaviour::LiesEvery(2)],
        events: Vec::new(),
        seed: 7,
    };
    assert_eq!(runs(&directed(), false), Ok(()), "the core as it is must pass");
    let why = runs(&directed(), true).expect_err("the oracle missed a liar's chunk kept unchecked");
    assert!(why.contains("are not the object's"), "{why}");
}

/// A peer on the network is a source like any other, and sources come and go mid-fetch:
/// with only a source that never answers the fetch waits (to its deadlines, then it would
/// fail); a peer that appears is asked at once; when it goes away what it was asked is asked
/// again of the peer that appears next, and the object ends whole. No index says who has
/// what: each source is asked.
#[test]
fn a_peer_that_appears_is_asked_and_one_that_goes_away_is_asked_no_more() {
    let truth = object(0, 6 * CHUNK as u32 + 5, 11);
    let want = Want { root: root_of(&truth), len: truth.len() as u64 };
    let mut world = World {
        whole: vec![false],
        truth: vec![truth],
        wants: vec![want],
        sources: vec![(Source(0), Behaviour::Silent, 0)],
        next_source: 1,
        flying: Vec::new(),
        kept: BTreeMap::new(),
        now: Duration::ZERO,
    };
    let mut fetch = Fetch::new(world.now, &world.wants);
    fetch.add_source(Source(0));
    world.carry_out(&mut fetch).unwrap();
    assert_eq!(world.flying.len(), 1, "the hashes are asked of the one source there is");
    // A peer appears: after the silent source's deadline the hashes are asked of it.
    world.sources.push((Source(1), Behaviour::Honest, 0));
    fetch.add_source(Source(1));
    // Sources are asked in turn: within the silent one's strikes the peer has its turn.
    for _ in 0..STRIKES {
        if world.flying.iter().any(|(_, it, _)| *it == Source(1)) {
            break;
        }
        world.now += ANSWER_WITHIN;
        fetch.advance_clock(world.now);
        world.carry_out(&mut fetch).unwrap();
    }
    let (ask, source, part) = world.flying.iter().copied().find(|(_, it, _)| *it == Source(1)).unwrap();
    world.flying.retain(|(it, ..)| *it != ask);
    let answer = world.answer(source, part).unwrap();
    fetch.answered(ask, answer);
    world.carry_out(&mut fetch).unwrap();
    // Two chunks arrive from the peer, then it goes away with asks in flight.
    for _ in 0..2 {
        let at = world.flying.iter().position(|(_, it, _)| *it == Source(1)).unwrap();
        let (ask, source, part) = world.flying.remove(at);
        let answer = world.answer(source, part).unwrap();
        fetch.answered(ask, answer);
        world.carry_out(&mut fetch).unwrap();
    }
    world.sources.retain(|(it, ..)| *it != Source(1));
    fetch.remove_source(Source(1));
    world.carry_out(&mut fetch).unwrap();
    assert!(world.flying.iter().all(|(_, it, _)| *it == Source(0)), "nothing is asked of a peer gone");
    assert_eq!(*fetch.outcome(), Outcome::Fetching);
    // Another peer appears and has the rest.
    world.sources.push((Source(2), Behaviour::Honest, 0));
    fetch.add_source(Source(2));
    world.carry_out(&mut fetch).unwrap();
    for _ in 0..1000 {
        if *fetch.outcome() != Outcome::Fetching {
            break;
        }
        match world.flying.iter().position(|(_, it, _)| *it == Source(2)) {
            Some(at) => {
                let (ask, source, part) = world.flying.remove(at);
                let answer = world.answer(source, part).unwrap();
                fetch.answered(ask, answer);
            }
            None => {
                world.now = fetch.next_deadline().expect("asks of the silent source are in flight");
                fetch.advance_clock(world.now);
            }
        }
        world.carry_out(&mut fetch).unwrap();
    }
    assert_eq!(*fetch.outcome(), Outcome::Done);
    assert!(world.whole[0]);
}

/// Sizes are the caller's, never a source's: an object past the bound is refused before
/// anything is asked, an empty one is whole at once when its root is the empty object's, and
/// with no source at all the fetch fails saying so.
#[test]
fn sizes_are_bounded_and_no_source_is_a_named_failure() {
    let huge = Want { root: ContentId([1; 32]), len: OBJECT_MOST + 1 };
    let mut fetch = Fetch::new(Duration::ZERO, &[huge]);
    assert!(matches!(fetch.outcome(), Outcome::Failed { why, .. } if why.contains("more than")));
    assert!(fetch.take_effects().is_empty(), "nothing is asked for an object refused");
    let empty = Want { root: root_of(&[]), len: 0 };
    let mut fetch = Fetch::new(Duration::ZERO, &[empty]);
    assert_eq!(*fetch.outcome(), Outcome::Done);
    assert_eq!(fetch.take_effects(), [Effect::Whole { root: empty.root }]);
    let wrong = Want { root: ContentId([2; 32]), len: 0 };
    let fetch = Fetch::new(Duration::ZERO, &[wrong]);
    assert!(matches!(fetch.outcome(), Outcome::Failed { why, .. } if why.contains("no bytes")));
    // Before any source is given the fetch waits for one; once the last is gone it fails.
    let some = Want { root: root_of(b"some bytes"), len: 10 };
    let mut fetch = Fetch::new(Duration::ZERO, &[some]);
    assert_eq!(*fetch.outcome(), Outcome::Fetching);
    fetch.add_source(Source(0));
    fetch.remove_source(Source(0));
    assert!(matches!(fetch.outcome(), Outcome::Failed { why, .. } if why.contains("no source")));
}
