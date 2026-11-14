//! Deterministic simulation tests of the host's effects: each runs a client on many seeds with
//! faults on, an oracle stating what the effect promises checked at every completion, and a
//! control per oracle that plants a bug in the simulation ([`Plant`]) and asserts some seed
//! catches it. A failing seed replays exactly: run its scenario with that seed.

use std::{
    collections::{BTreeMap, BTreeSet},
    net::SocketAddr,
    time::Duration,
};

use super::{Datagrams, Plant};
use crate::{
    Completion, DATAGRAM_MOST, Dgram, Done, Driver, Effect, FileName, RANDOM_MOST, Stream, Tag,
    run_until_idle,
    sim::{Faults, Rng, Sim},
    wait::Deadline,
};

/// Seeds each scenario runs on.
const SEEDS: u64 = 200;

fn name(it: &str) -> FileName {
    FileName::new(it).unwrap()
}

/// `effects` and what each completion asks for next, run to the end with the simulation at
/// hand: at most this many polls, each waiting at most ten virtual seconds.
fn drive(
    sim: &mut Sim,
    effects: &mut Vec<Effect>,
    mut on: impl FnMut(&mut Sim, Completion, &mut Vec<Effect>),
) {
    let mut done = Vec::new();
    for _ in 0..100_000 {
        sim.submit(effects);
        if sim.in_flight() == 0 {
            return;
        }
        let until = Deadline::after(sim.now(), Duration::from_secs(10));
        sim.poll(&mut done, until);
        for completion in done.drain(..) {
            on(sim, completion, effects);
        }
    }
    panic!("the run did not end in 100000 polls");
}

/// What the runs of [`datagrams`] saw happen, so the test knows its faults were exercised.
#[derive(Default)]
struct Saw {
    reordered: bool,
    doubled: bool,
    lost: bool,
}

const SENT: u64 = 24;
const RECV: Tag = Tag(3);
const AFTER: Tag = Tag(4);
const FIRST_SEND: u64 = 100;

/// Two sockets on loopback; the first sends [`SENT`] datagrams, each its number, to the
/// second, which keeps a receive in flight; once every send is done and the network's delay
/// has passed, both are unbound. The oracle: each datagram received was sent, from the
/// sender's address, and arrives at most as often as the network copies one (twice).
fn datagrams(seed: u64, plant: Option<Plant>, saw: &mut Saw) -> Result<(), String> {
    let mut sim = Sim::new(seed, Faults { fail_one_in: 0, ..Faults::default() });
    sim.datagrams(Datagrams {
        lose_one_in: 5,
        duplicate_one_in: 4,
        max_delay: Duration::from_millis(1),
    });
    if let Some(plant) = plant {
        sim.plant(plant);
    }
    let loopback: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let mut effects = vec![
        Effect::Bind { at: loopback, tag: Tag(1) },
        Effect::Bind { at: loopback, tag: Tag(2) },
    ];
    let mut bound: [Option<(Dgram, SocketAddr)>; 2] = [None, None];
    let mut sends_done = 0;
    let mut unbound = false;
    let mut copies = BTreeMap::<u64, u32>::new();
    let mut highest = None;
    let mut broke = None;
    drive(&mut sim, &mut effects, |_, Completion { tag, done }, effects| {
        let mut fail = |why: String| _ = broke.get_or_insert(format!("seed {seed}: {why}"));
        match (tag, done) {
            (Tag(n @ (1 | 2)), Done::Bound { sock, local }) => {
                bound[n as usize - 1] = Some((sock, local));
                if let [Some(_), Some((to, at))] = bound {
                    let (from, _) = bound[0].expect("both bound");
                    effects.push(Effect::RecvFrom { sock: to, into: Vec::new(), tag: RECV });
                    for n in 0..SENT {
                        let bytes = n.to_le_bytes().to_vec();
                        effects.push(Effect::SendTo {
                            sock: from,
                            to: at,
                            bytes,
                            tag: Tag(FIRST_SEND + n),
                        });
                    }
                }
            }
            (Tag(n), Done::Sent(_)) if (FIRST_SEND..FIRST_SEND + SENT).contains(&n) => {
                sends_done += 1;
                if sends_done == SENT {
                    effects.push(Effect::Timer { after: Duration::from_millis(100), tag: AFTER });
                }
            }
            (RECV, Done::Datagram { from, bytes }) => {
                let (_, sender) = bound[0].expect("bound before anything was sent");
                if from != sender {
                    fail(format!("a datagram from {from}, sent from {sender}"));
                }
                let Ok(number) = <[u8; 8]>::try_from(bytes.as_slice()) else {
                    return fail(format!("a datagram of {} bytes, none was sent", bytes.len()));
                };
                let number = u64::from_le_bytes(number);
                if number >= SENT {
                    fail(format!("datagram {number} arrived, never sent"));
                }
                let seen = copies.entry(number).or_default();
                *seen += 1;
                if *seen > 2 {
                    fail(format!("datagram {number} arrived {seen} times"));
                }
                saw.doubled |= *seen == 2;
                saw.reordered |= highest.is_some_and(|it| number < it);
                highest = highest.max(Some(number));
                let (sock, _) = bound[1].expect("bound before it received");
                effects.push(Effect::RecvFrom { sock, into: bytes, tag: RECV });
            }
            (RECV, Done::Broken { .. }) if unbound => {}
            (AFTER, Done::Fired) => {
                unbound = true;
                for (at, tag) in [(0, Tag(5)), (1, Tag(6))] {
                    let (sock, _) = bound[at].expect("bound");
                    effects.push(Effect::Unbind { sock, tag });
                }
            }
            (Tag(5 | 6), Done::Unbound) => {}
            (tag, done) => fail(format!("{tag:?} completed with {done:?}")),
        }
    });
    saw.lost |= (copies.len() as u64) < SENT;
    broke.map_or(Ok(()), Err)
}

#[test]
fn a_datagram_arrives_from_its_sender_at_most_as_often_as_the_network_copies_it() {
    let mut saw = Saw::default();
    for seed in 0..SEEDS {
        if let Err(why) = datagrams(seed, None, &mut saw) {
            panic!("{why}");
        }
    }
    assert!(saw.reordered && saw.doubled && saw.lost, "the network's faults were all exercised");
}

#[test]
fn a_datagram_too_long_or_to_a_socket_not_bound_is_refused_naming_it() {
    let mut sim = Sim::new(1, Faults::default());
    let at: SocketAddr = "127.0.0.1:5000".parse().unwrap();
    let mut answers = Vec::new();
    let mut effects = vec![Effect::Bind { at, tag: Tag(1) }];
    run_until_idle(&mut sim, &mut effects, |done, _| answers.push(done)).unwrap();
    let Done::Bound { sock, local } = answers.pop().unwrap().done else { panic!("not bound") };
    assert_eq!(local, at);
    let mut effects = vec![
        Effect::Bind { at, tag: Tag(2) },
        Effect::SendTo { sock, to: at, bytes: vec![0; DATAGRAM_MOST + 1], tag: Tag(3) },
        Effect::SendTo { sock: Dgram(99), to: at, bytes: vec![1], tag: Tag(4) },
        Effect::Unbind { sock: Dgram(99), tag: Tag(5) },
    ];
    run_until_idle(&mut sim, &mut effects, |done, _| answers.push(done)).unwrap();
    answers.sort_by_key(|it| it.tag);
    let said: Vec<String> = answers
        .into_iter()
        .map(|it| match it.done {
            Done::Failed(error) | Done::Broken { error, .. } => error.0,
            done => panic!("{:?} completed with {done:?}", it.tag),
        })
        .collect();
    assert!(said[0].contains("127.0.0.1:5000") && said[0].contains("in use"), "{}", said[0]);
    assert!(said[1].contains("127.0.0.1:5000") && said[1].contains("65508"), "{}", said[1]);
    assert!(said[2].contains("Dgram(99)"), "{}", said[2]);
    assert!(said[3].contains("Dgram(99)"), "{}", said[3]);
}

const READ: Tag = Tag(1);
const FIRST_WRITE: u64 = 1000;
const WRITES: u64 = 16;

/// A client that reads the standard input to its end, a read in flight at a time, as the
/// test gives it in pieces, while it writes [`WRITES`] pieces to the output and error, one in
/// four failing. The oracles: what the reads got, in the order told, is the input; each
/// stream holds the writes that succeeded, in the order asked, and nothing else.
fn stdio(seed: u64, plant: Option<Plant>) -> Result<(), String> {
    let mut sim = Sim::new(seed, Faults { fail_one_in: 4, ..Faults::default() });
    if let Some(plant) = plant {
        sim.plant(plant);
    }
    let mut rng = Rng::new(seed ^ 0x57d1);
    let input: Vec<u8> = (0..rng.below(300)).map(|_| rng.below(256) as u8).collect();
    let mut pieces: Vec<&[u8]> = input.chunks(1 + rng.below(40) as usize).collect();
    pieces.reverse();
    let mut writes = BTreeMap::<u64, (Stream, Vec<u8>, Option<bool>)>::new();
    let mut effects = Vec::new();
    for n in 0..WRITES {
        let stream = if rng.coin() { Stream::Stdout } else { Stream::Stderr };
        let bytes: Vec<u8> = (0..rng.below(20)).map(|_| rng.below(256) as u8).collect();
        writes.insert(n, (stream, bytes.clone(), None));
        effects.push(Effect::WriteOut { stream, bytes, tag: Tag(FIRST_WRITE + n) });
    }
    let first = pieces.pop().unwrap_or_default();
    sim.stdin(first);
    if pieces.is_empty() {
        sim.stdin_ends();
    }
    let most = 1 + rng.below(16) as usize;
    effects.push(Effect::ReadStdin { most, into: Vec::new(), tag: READ });
    let mut read = Vec::new();
    let mut ended = false;
    let mut broke = None;
    drive(&mut sim, &mut effects, |sim, Completion { tag, done }, effects| {
        let mut fail = |why: String| _ = broke.get_or_insert(format!("seed {seed}: {why}"));
        match (tag, done) {
            (READ, Done::Read(bytes)) if bytes.is_empty() => ended = true,
            (READ, Done::Read(bytes)) => {
                if bytes.len() > most {
                    fail(format!("a read of at most {most} got {}", bytes.len()));
                }
                read.extend_from_slice(&bytes);
                if let Some(piece) = pieces.pop() {
                    sim.stdin(piece);
                }
                if pieces.is_empty() {
                    sim.stdin_ends();
                }
                effects.push(Effect::ReadStdin { most, into: Vec::new(), tag: READ });
            }
            (Tag(n), done) if (FIRST_WRITE..FIRST_WRITE + WRITES).contains(&n) => {
                let ok = match done {
                    Done::Sent(bytes) => bytes.is_empty(),
                    Done::Broken { error, .. } => {
                        if !error.0.contains("standard") {
                            fail(format!("a failed write says {error:?}, naming no stream"));
                        }
                        false
                    }
                    done => return fail(format!("{tag:?} completed with {done:?}")),
                };
                writes.get_mut(&(n - FIRST_WRITE)).expect("asked").2 = Some(ok);
            }
            (tag, done) => fail(format!("{tag:?} completed with {done:?}")),
        }
    });
    if let Some(why) = broke {
        return Err(why);
    }
    if !ended || read != input {
        return Err(format!("seed {seed}: read {read:?} of the input {input:?}"));
    }
    for stream in [Stream::Stdout, Stream::Stderr] {
        let written: Vec<u8> = writes
            .values()
            .filter(|(it, _, ok)| *it == stream && *ok == Some(true))
            .flat_map(|(_, bytes, _)| bytes.iter().copied())
            .collect();
        if sim.output(stream) != written.as_slice() {
            return Err(format!(
                "seed {seed}: {stream} holds {:?}, its writes that succeeded {written:?}",
                sim.output(stream)
            ));
        }
    }
    Ok(())
}

#[test]
fn the_standard_streams_hold_what_succeeded_and_reads_get_the_input_in_order() {
    for seed in 0..SEEDS {
        if let Err(why) = stdio(seed, None) {
            panic!("{why}");
        }
    }
}

const NOW: Tag = Tag(1);
const PAUSE: Tag = Tag(2);
const DRAW: Tag = Tag(3);
const ROUNDS: usize = 20;

/// A client that asks the time and for random bytes, then waits a drawn while, [`ROUNDS`]
/// times, and asks for more than one draw gives once. The oracles: each wall time is the
/// start plus the virtual clock at some point between its ask and its answer, never before
/// the last; each draw is as long as asked, and no two are alike; the draw too long fails,
/// naming the most.
fn clock_and_random(seed: u64, plant: Option<Plant>) -> Result<(), String> {
    let mut sim = Sim::new(seed, Faults::default());
    if let Some(plant) = plant {
        sim.plant(plant);
    }
    let start = sim.wall_start();
    let mut rng = Rng::new(seed ^ 0x7a11);
    let mut asked_at = sim.now();
    let mut last = Duration::ZERO;
    let mut rounds = 0;
    let mut lens = Vec::new();
    let mut draws = BTreeSet::new();
    let len = 16 + rng.below(16) as usize;
    lens.push(len);
    let mut effects = vec![
        Effect::Now { tag: NOW },
        Effect::Random { len, tag: DRAW },
        Effect::Random { len: RANDOM_MOST + 1, tag: Tag(4) },
    ];
    let mut broke = None;
    drive(&mut sim, &mut effects, |sim, Completion { tag, done }, effects| {
        let mut fail = |why: String| _ = broke.get_or_insert(format!("seed {seed}: {why}"));
        match (tag, done) {
            (NOW, Done::Wall(wall)) => {
                let (low, high) = (start + asked_at, start + sim.now());
                if wall < low || wall > high {
                    fail(format!("the wall clock {wall:?}, outside {low:?}..={high:?}"));
                }
                if wall < last {
                    fail(format!("the wall clock went back from {last:?} to {wall:?}"));
                }
                last = wall;
                rounds += 1;
                if rounds < ROUNDS {
                    let after = Duration::from_micros(rng.below(5_000));
                    effects.push(Effect::Timer { after, tag: PAUSE });
                }
            }
            (PAUSE, Done::Fired) => {
                asked_at = sim.now();
                let len = 16 + rng.below(16) as usize;
                lens.push(len);
                effects.push(Effect::Now { tag: NOW });
                effects.push(Effect::Random { len, tag: DRAW });
            }
            (DRAW, Done::Random(bytes)) => {
                let asked = lens.remove(0);
                if bytes.len() != asked {
                    fail(format!("{} random bytes, {asked} asked for", bytes.len()));
                }
                if !draws.insert(bytes[..16.min(bytes.len())].to_vec()) {
                    fail(format!("a draw of random bytes repeated: {bytes:?}"));
                }
            }
            (Tag(4), Done::Failed(error)) if error.0.contains(&RANDOM_MOST.to_string()) => {}
            (tag, done) => fail(format!("{tag:?} completed with {done:?}")),
        }
    });
    broke.map_or(Ok(()), Err)
}

#[test]
fn the_wall_clock_moves_with_the_virtual_one_and_random_draws_differ() {
    for seed in 0..SEEDS {
        if let Err(why) = clock_and_random(seed, None) {
            panic!("{why}");
        }
    }
}

const DIRS: [&str; 3] = ["a", "a/b", "c"];
const FILES: [&str; 2] = ["a/b/f", "c/g"];
const OPS: usize = 40;

fn is_under(it: &str, dir: &str) -> bool {
    it == dir || it.strip_prefix(dir).is_some_and(|rest| rest.starts_with('/'))
}

/// The model's names: the directories made and the files written.
type Names = BTreeSet<&'static str>;

/// Whether the model has a file or a directory made under `dir`, not `dir` itself.
fn something_under(dir: &str, made: &Names, files: &Names) -> bool {
    made.iter().chain(files.iter()).any(|it| *it != dir && is_under(it, dir))
}

/// Whether the model has `dir` a directory: made, or something under it.
fn model_dir(dir: &str, made: &Names, files: &Names) -> bool {
    made.contains(dir) || something_under(dir, made, files)
}

/// A client that makes and removes directories and writes and removes files under them, one
/// at a time, one in six failing. The oracle, a model of the disk: each answer is what the
/// model says (a failure injected changing nothing), so no file goes but by a remove of it,
/// and after each the simulated disk has the model's files and directories.
fn dirs(seed: u64, plant: Option<Plant>) -> Result<(), String> {
    let mut sim = Sim::new(seed, Faults { fail_one_in: 6, ..Faults::default() });
    if let Some(plant) = plant {
        sim.plant(plant);
    }
    let mut rng = Rng::new(seed ^ 0xd125);
    let mut made = Names::new();
    let mut files = Names::new();
    for op in 0..OPS {
        let dir = DIRS[rng.index(DIRS.len())];
        let file = FILES[rng.index(FILES.len())];
        let (effect, expected) = match rng.below(4) {
            0 => {
                let parent_there = dir
                    .rsplit_once('/')
                    .is_none_or(|(parent, _)| model_dir(parent, &made, &files));
                let ok = !model_dir(dir, &made, &files) && parent_there;
                (Effect::MakeDir { dir: name(dir), tag: Tag(1) }, ok)
            }
            1 => {
                let ok = made.contains(dir) && !something_under(dir, &made, &files);
                (Effect::RemoveDir { dir: name(dir), tag: Tag(1) }, ok)
            }
            2 => (Effect::Write { file: name(file), bytes: b"x".to_vec(), tag: Tag(1) }, true),
            _ => (Effect::Remove { file: name(file), tag: Tag(1) }, true),
        };
        let what = format!("seed {seed}, op {op}: {effect:?}");
        let mut answer = None;
        run_until_idle(&mut sim, &mut vec![effect.clone()], |it, _| answer = Some(it.done))
            .map_err(|stalled| format!("{what}: {stalled:?}"))?;
        let ok = match answer {
            Some(Done::Failed(error)) if error.0.contains("injected") => continue,
            Some(Done::Failed(error)) => {
                let named = match &effect {
                    Effect::MakeDir { dir, .. } | Effect::RemoveDir { dir, .. } => dir.as_str(),
                    _ => "",
                };
                if !error.0.contains(named) {
                    return Err(format!("{what}: its error {error:?} names nothing"));
                }
                false
            }
            Some(Done::MadeDir | Done::RemovedDir | Done::Written | Done::Removed) => true,
            other => return Err(format!("{what}: completed with {other:?}")),
        };
        if ok != expected {
            return Err(format!("{what}: {ok} where the model says {expected}"));
        }
        if ok {
            match effect {
                Effect::MakeDir { .. } => _ = made.insert(dir),
                Effect::RemoveDir { .. } => _ = made.remove(dir),
                Effect::Write { .. } => _ = files.insert(file),
                _ => _ = files.remove(file),
            }
        }
        for file in FILES {
            if sim.files.contains_key(&name(file)) != files.contains(file) {
                return Err(format!("{what}: {file} there or not, not as the model says"));
            }
        }
        for dir in DIRS {
            if sim.is_dir(&name(dir)) != model_dir(dir, &made, &files) {
                return Err(format!("{what}: {dir} a directory or not, not as the model says"));
            }
        }
    }
    Ok(())
}

#[test]
fn a_directory_is_made_and_removed_only_empty_and_no_file_goes_with_it() {
    for seed in 0..SEEDS {
        if let Err(why) = dirs(seed, None) {
            panic!("{why}");
        }
    }
}

#[test]
fn a_directory_made_is_listed_stated_and_removed_empty() {
    let mut sim = Sim::new(3, Faults::default());
    let mut answers = Vec::new();
    let mut effects = vec![Effect::MakeDir { dir: name("x"), tag: Tag(1) }];
    run_until_idle(&mut sim, &mut effects, |it, _| answers.push(it.done)).unwrap();
    let mut effects = vec![
        Effect::List { dir: name("x"), tag: Tag(2) },
        Effect::Stat { file: name("x"), tag: Tag(3) },
        Effect::MakeDir { dir: name("x/y/z"), tag: Tag(4) },
        Effect::MakeDir { dir: name("x"), tag: Tag(5) },
    ];
    run_until_idle(&mut sim, &mut effects, |it, _| answers.push(it.done)).unwrap();
    sim.crash();
    let mut effects = vec![Effect::RemoveDir { dir: name("x"), tag: Tag(6) }];
    run_until_idle(&mut sim, &mut effects, |it, _| answers.push(it.done)).unwrap();
    let mut effects = vec![Effect::Stat { file: name("x"), tag: Tag(7) }];
    run_until_idle(&mut sim, &mut effects, |it, _| answers.push(it.done)).unwrap();
    assert!(matches!(answers[0], Done::MadeDir), "{:?}", answers[0]);
    let mut middle: Vec<String> = answers[1..5].iter().map(|it| format!("{it:?}")).collect();
    middle.sort();
    assert!(middle.iter().any(|it| it == "Listed([])"), "{middle:?}");
    assert!(middle.iter().any(|it| it.starts_with("Stat(Some(") && it.contains("dir: true")));
    assert!(middle.iter().any(|it| it.contains("x/y/z") && it.contains("not there")));
    assert!(middle.iter().any(|it| it.contains("x: it is there already")), "{middle:?}");
    assert!(matches!(answers[5], Done::RemovedDir), "made survives a crash: {:?}", answers[5]);
    assert!(matches!(answers[6], Done::Stat(None)), "{:?}", answers[6]);
}

/// Each planted bug is caught by its scenario's oracle on some seed: the oracles see what
/// they claim to.
#[test]
fn each_planted_bug_is_caught() {
    let caught = |plant: Plant| {
        (0..SEEDS).any(|seed| match plant {
            Plant::SenderLies | Plant::ExtraCopies => {
                datagrams(seed, Some(plant), &mut Saw::default()).is_err()
            }
            Plant::FailedWriteLands | Plant::InputSkips => stdio(seed, Some(plant)).is_err(),
            Plant::WallLags | Plant::RandomRepeats => {
                clock_and_random(seed, Some(plant)).is_err()
            }
            Plant::RemovesFull => dirs(seed, Some(plant)).is_err(),
            Plant::BarriersNotKept | Plant::RenameFlushesOthers | Plant::PutBackWithBarriers => {
                unreachable!("{plant:?} is the disk's, caught by its own controls (sim/world.rs)")
            }
        })
    };
    for plant in [
        Plant::SenderLies,
        Plant::ExtraCopies,
        Plant::FailedWriteLands,
        Plant::InputSkips,
        Plant::WallLags,
        Plant::RandomRepeats,
        Plant::RemovesFull,
    ] {
        assert!(caught(plant), "no seed caught {plant:?}");
    }
}
