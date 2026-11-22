//! How a hosted run is held (run.md, "How a run is held"), on the simulation: the host's
//! `Record` writes down every call a program makes through the table and its answer, and its
//! `Replay` answers the same program from that record with the world asked nothing, stopping
//! at the first call that differs, named.
//!
//! - Coverage: a program that touches every capability the hosted wire offers once leaves a
//!   record in which every touch appears (and the control: one that skips a touch leaves a
//!   record without it).
//! - Round trip: a record read back from its bytes is the record; a record of another version,
//!   cut short or with bytes after it is refused.
//! - Replay: seeded plans of calls are recorded on one world and replayed on another; every
//!   answer and every byte an answer put in the exchange is the record's, and a plan changed at
//!   one call stops the replay at that call. Two planted bugs (a replay that does not compare
//!   the call, a recorder that drops an answer's bytes) are each caught.

use std::time::Duration;

use blaze_sim::{Config, Tape};

use super::{check, control};
use crate::{
    Plant,
    cx::Cx,
    entry::{Error, Status},
    guest,
    hosting::{Call, Hold, Holding, Hosting, Record},
    io::Io,
    wire::{self, Answer, HostTable, code},
};

const EXCHANGE: usize = 4096;

fn host(io: Io, hold: Hold) -> Box<Hosting> {
    Hosting::holding(io, vec!["toucher".to_owned()], Vec::new(), EXCHANGE, Holding::of(hold), blaze_own::testing::heap().shared())
}

/// Every capability the hosted wire offers, touched once each.
fn touch(cx: &Cx, io: &Io, said: &str) -> Result<(), Error> {
    io.need_out()?.line(cx, said)?;
    io.need_err()?.line(cx, "to err")?;
    let clock = io.need_clock()?;
    let _ = clock.now();
    let _ = clock.wall();
    let _ = io.need_args()?.get(0);
    let _ = io.need_rng()?.next_u64();
    clock.sleep(cx, Duration::from_millis(1)).map_err(|_| Error::new("the sleep stopped"))?;
    Ok(())
}

fn toucher(cx: &Cx, io: Io) -> Status {
    let Some(_out) = guest::grant::<crate::out::Out>(&io, "out") else { return Status::REFUSED };
    let ran = touch(cx, &io, "to out");
    guest::finish(&io, ran)
}

/// As `toucher`, but its first line says other words: the program a replay must stop.
fn other_toucher(cx: &Cx, io: Io) -> Status {
    let Some(_out) = guest::grant::<crate::out::Out>(&io, "out") else { return Status::REFUSED };
    let ran = touch(cx, &io, "TO OUT");
    guest::finish(&io, ran)
}

/// As `toucher` with no randomness asked: the control for the coverage test.
fn toucher_without_rng(cx: &Cx, io: Io) -> Status {
    let Some(_out) = guest::grant::<crate::out::Out>(&io, "out") else { return Status::REFUSED };
    let ran = (|| {
        io.need_out()?.line(cx, "to out")?;
        Ok::<(), Error>(())
    })();
    guest::finish(&io, ran)
}

/// What one held call touched: its entry, and which handle or effect.
fn touched(call: &Call) -> String {
    match call {
        Call::Write { stream, .. } if *stream == wire::stream::OUT => "write out".to_owned(),
        Call::Write { .. } => "write err".to_owned(),
        Call::Clock { which } if *which == wire::clock::MONOTONIC => "clock monotonic".to_owned(),
        Call::Clock { .. } => "clock wall".to_owned(),
        Call::Arg { .. } => "arg".to_owned(),
        Call::Grant { .. } => "grant".to_owned(),
        Call::Effect { bytes, .. } => match guest::codec::read_effect(bytes).map(|escaped| escaped.under(blaze_core::Tag(0))) {
            Ok(blaze_core::Effect::Random { .. }) => "effect random".to_owned(),
            Ok(blaze_core::Effect::Timer { .. }) => "effect timer".to_owned(),
            Ok(other) => format!("effect {other:?}"),
            Err(error) => format!("effect not of the wire: {error:?}"),
        },
        Call::Exit { .. } => "exit".to_owned(),
    }
}

const EVERY_TOUCH: [&str; 8] =
    ["grant", "write out", "write err", "clock monotonic", "clock wall", "arg", "effect random", "effect timer"];

/// The coverage invariant: a program touching every capability the wire offers once leaves a
/// record in which every touch appears. (`exit` is not among them: the hosted wire offers no
/// `host`, so a program has no way to it but its start's end.)
#[test]
fn a_program_touching_every_capability_leaves_each_touch_in_its_record() {
    let hosting = host(Io::sim(1), Hold::Record);
    assert_eq!(guest::start(&hosting.table(), toucher), 0);
    let seen: Vec<String> = hosting.record().calls.iter().map(|called| touched(&called.call)).collect();
    for touch in EVERY_TOUCH {
        assert!(seen.iter().any(|it| it == touch), "the record has no `{touch}`: {seen:?}");
    }
    // The control: a program that asks no randomness leaves no `effect random`.
    let hosting = host(Io::sim(1), Hold::Record);
    assert_eq!(guest::start(&hosting.table(), toucher_without_rng), 0);
    let seen: Vec<String> = hosting.record().calls.iter().map(|called| touched(&called.call)).collect();
    assert!(!seen.iter().any(|it| it == "effect random"), "{seen:?}");
}

/// The same program replayed from its record, on another world, ends the same and asks that
/// world nothing; a program that differs stops at its first different call, named.
#[test]
fn a_replay_answers_from_the_record_and_stops_at_the_first_difference() {
    let recorded = host(Io::sim(2), Hold::Record);
    assert_eq!(guest::start(&recorded.table(), toucher), 0);
    let record = Record::read(&recorded.record().write(blaze_own::testing::heap().alloc())).expect("the record reads back");

    let elsewhere = Io::sim(77);
    let replayed = host(elsewhere.clone(), Hold::Replay(record.clone()));
    assert_eq!(guest::start(&replayed.table(), toucher), 0);
    assert_eq!(replayed.diverged(), None);
    assert_eq!(replayed.unreplayed(), 0, "every recorded call was made");
    assert_eq!(elsewhere.script(|host| host.out.clone()), Some(Vec::new()), "the replay's world was asked nothing");

    let replayed = host(Io::sim(78), Hold::Replay(record.clone()));
    assert_ne!(guest::start(&replayed.table(), other_toucher), 0, "a refused write fails the program");
    let diverged = replayed.diverged().expect("the replay stopped");
    let at = record.calls.iter().position(|called| matches!(called.call, Call::Write { .. })).expect("the record has a write");
    assert_eq!(diverged.at, at, "{diverged}");
    assert_eq!(diverged.made, Call::Write { stream: wire::stream::OUT, bytes: b"TO OUT\n".to_vec() });
    assert!(diverged.to_string().contains(&format!("call {at} differs")), "{diverged}");
}

/// A record is read strictly: another version, a cut, or bytes after it are refused, named.
#[test]
fn a_record_of_another_version_or_cut_short_is_refused() {
    let hosting = host(Io::sim(3), Hold::Record);
    assert_eq!(guest::start(&hosting.table(), toucher), 0);
    let bytes = hosting.record().write(blaze_own::testing::heap().alloc());
    assert_eq!(Record::read(&bytes), Ok(hosting.record()));
    assert!(Record::read(&bytes[..bytes.len() - 1]).is_err(), "a record cut short");
    let mut longer = bytes.clone();
    longer.push(0);
    assert!(Record::read(&longer).is_err(), "bytes after the record");
    // Version 1 is the byte after the magic's text: `0x01`; 2 is another version.
    let magic = Record::MAGIC.as_bytes();
    let at = bytes.windows(magic.len()).position(|it| it == magic).expect("the magic") + magic.len();
    let mut other = bytes.clone();
    other[at] = 0x02;
    let refused = Record::read(&other).expect_err("another version");
    assert!(refused.why.contains("another version"), "{refused}");
}

/// One valid call of a seeded plan.
#[derive(Clone, Debug, PartialEq)]
enum Step {
    Write { stream: u32, bytes: Vec<u8> },
    Clock { which: u32 },
    Arg { index: u64 },
    Grant { kind: &'static str, name: &'static str },
    Random { len: u64 },
}

fn plan(tape: &Tape) -> (Vec<Step>, usize) {
    let mut steps = Vec::new();
    while steps.is_empty() || tape.more(16, "another call") {
        let step = match tape.below(5, "an entry") {
            0 => Step::Write {
                stream: *tape.one_of(&[wire::stream::OUT, wire::stream::ERR], "a stream"),
                bytes: (0..tape.below(40, "a length")).map(|at| b'a' + (at % 26) as u8).collect(),
            },
            1 => Step::Clock { which: *tape.one_of(&[wire::clock::MONOTONIC, wire::clock::WALL], "a clock") },
            2 => Step::Arg { index: tape.below(3, "an index") },
            3 => Step::Grant {
                kind: *tape.one_of(&["dir", "out"], "a kind"),
                name: *tape.one_of(&["io", "nope"], "a name"),
            },
            _ => Step::Random { len: 1 + tape.below(16, "random bytes") },
        };
        steps.push(step);
    }
    let changed = tape.below(steps.len() as u64, "the call a second run changes") as usize;
    (steps, changed)
}

/// `step` made through `table`, its bytes put in the exchange first.
#[expect(unsafe_code, reason = "a program's calls through the host's table")]
fn make(hosting: &Hosting, table: &HostTable, step: &Step) -> Answer {
    let mut effect = blaze_own::Vec::new_in(blaze_own::SharedAlloc::global());
    // SAFETY: the table's own entries and pointer, its `Hosting` alive for the test; every
    // range is inside the exchange.
    unsafe {
        match step {
            Step::Write { stream, bytes } => {
                hosting.put(0, bytes);
                (table.write)(table.host, *stream, 0, bytes.len())
            }
            Step::Clock { which } => (table.clock)(table.host, *which),
            Step::Arg { index } => (table.arg)(table.host, *index),
            Step::Grant { kind, name } => {
                hosting.put(0, kind.as_bytes());
                hosting.put(kind.len(), name.as_bytes());
                (table.grant)(table.host, 0, kind.len(), kind.len(), name.len())
            }
            Step::Random { len } => {
                blaze_cbor::array(&mut effect, 2);
                blaze_cbor::uint(&mut effect, guest::codec::RANDOM);
                blaze_cbor::uint(&mut effect, *len);
                hosting.put(0, &effect);
                (table.effect)(table.host, 0, effect.len(), 0)
            }
        }
    }
}

/// `steps` made through `hosting`: each answer, and the bytes it put in the exchange.
fn made(hosting: &Hosting, steps: &[Step]) -> Vec<(Answer, Vec<u8>)> {
    let table = hosting.table();
    steps
        .iter()
        .map(|step| {
            hosting.put(0, &[0; 64]);
            let answer = make(hosting, &table, step);
            let back = match step {
                Step::Arg { .. } | Step::Random { .. } if answer.code == code::OK => hosting.head(answer.value as usize),
                _ => Vec::new(),
            };
            (answer, back)
        })
        .collect()
}

/// The replay invariant: recorded on one world and replayed on another, every answer and
/// every byte an answer put in the exchange is the record's, the record reads back from its
/// bytes, and a plan changed at one call stops the replay at exactly that call. The host's
/// plants are its own (`Hosting`'s), carried here by a stand-in plant the control sets.
fn run(plan: &(Vec<Step>, usize), plant: Plant) -> Result<(), String> {
    let (steps, changed) = plan;
    let recorder = match plant {
        Plant::LocalDropsChunk => host(Io::sim(5), Hold::Record).plant_record_drops_back(),
        _ => host(Io::sim(5), Hold::Record),
    };
    let first = made(&recorder, steps);
    let record = Record::read(&recorder.record().write(blaze_own::testing::heap().alloc())).map_err(|error| format!("the record does not read back: {error}"))?;
    if record.calls.len() != steps.len() {
        return Err(format!("{} calls made, {} recorded", steps.len(), record.calls.len()));
    }
    let replay = |plan: &[Step]| {
        let hosting = match plant {
            Plant::KeyIgnoresLast => host(Io::sim(6), Hold::Replay(record.clone())).plant_replay_unchecked(),
            _ => host(Io::sim(6), Hold::Replay(record.clone())),
        };
        let answers = made(&hosting, plan);
        (answers, hosting.diverged(), hosting.unreplayed())
    };
    let (again, diverged, left) = replay(steps);
    if again != first || diverged.is_some() || left != 0 {
        return Err(format!("the replay differs: {again:?} against {first:?}, diverged {diverged:?}, {left} left"));
    }
    let mut other = steps.clone();
    other[*changed] = Step::Arg { index: 1000 };
    let (answers, diverged, _) = replay(&other);
    match diverged {
        Some(diverged) if diverged.at == *changed && answers[..*changed] == first[..*changed] => Ok(()),
        diverged => Err(format!("a plan changed at call {changed} replayed so: diverged {diverged:?}")),
    }
}

/// The invariant: recording captures every hosted call and answer bytes intact, and replay returns identical answers without touching the host while stopping at the first diverging call.
#[test]
fn a_recorded_plan_replays_equal_and_a_changed_one_stops_where_it_changed() {
    blaze_sim::check(Config::cases(200), |tape| check(plan(tape), run));
}

#[test]
fn the_oracle_catches_a_replay_that_does_not_compare_the_call() {
    control(Plant::KeyIgnoresLast, plan, run);
}

#[test]
fn the_oracle_catches_a_recorder_that_drops_an_answers_bytes() {
    control(Plant::LocalDropsChunk, plan, run);
}

/// The run spec says how a hosted run is held: its provider and its layers.
#[test]
fn the_run_spec_picks_the_holding() {
    use crate::spec::{Layer, Layers, Provider, RunSpec};
    use crate::{
        hosting::HoldError,
        layers::{Decide, Decision, Fakes, Question},
    };
    struct No;
    impl Decide for No {
        fn decide(&mut self, _: &Question<'_>) -> Decision {
            Decision::No
        }
    }
    let of = |spec: &RunSpec<_>, record: Option<Record>, ask: bool, fakes: Fakes| {
        let ask: Option<Box<dyn Decide + Send>> = if ask { Some(Box::new(No)) } else { None };
        Holding::of_spec(spec, record, ask, fakes).map(|holding| (holding.hold, holding.dry, holding.ask.is_some()))
    };
    let clock = || Fakes { clock: Some(5), ..Fakes::default() };
    let alloc = blaze_own::SharedAlloc::global();
    let mut spec = RunSpec::defaults(blaze_own::Vec::new_in(alloc), blaze_own::Vec::new_in(alloc), blaze_own::String::new_in(alloc), alloc);
    assert!(matches!(of(&spec, None, false, Fakes::default()), Ok((Hold::Real, false, false))));
    assert!(matches!(of(&spec, None, false, clock()), Err(HoldError::FakeNotAsked)));
    spec.layers = Layers::NONE.with(Layer::Record);
    assert!(matches!(of(&spec, None, false, Fakes::default()), Ok((Hold::Record, false, false))));
    spec.layers = Layers::NONE.with(Layer::Overlay);
    assert!(matches!(of(&spec, None, false, Fakes::default()), Ok((Hold::Real, true, false))));
    spec.layers = Layers::NONE.with(Layer::Ask);
    assert!(matches!(of(&spec, None, false, Fakes::default()), Err(HoldError::NoDecider)));
    assert!(matches!(of(&spec, None, true, Fakes::default()), Ok((Hold::Real, false, true))));
    spec.layers = Layers::NONE.with(Layer::Record).with(Layer::Overlay);
    spec.provider = Provider::Fake;
    assert!(matches!(of(&spec, None, false, Fakes::default()), Err(HoldError::NoFake)));
    assert!(matches!(of(&spec, None, false, clock()), Ok((Hold::Record, true, false))));
    spec.provider = Provider::Replay;
    assert!(matches!(of(&spec, Some(Record::default()), false, Fakes::default()), Err(HoldError::ReplayHasNoLayers)));
    spec.layers = Layers::NONE;
    assert!(matches!(of(&spec, None, false, Fakes::default()), Err(HoldError::NoRecord)));
    assert!(matches!(of(&spec, Some(Record::default()), false, Fakes::default()), Ok((Hold::Replay(_), false, false))));
}
