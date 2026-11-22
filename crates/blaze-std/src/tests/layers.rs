//! The `dry` and `ask` layers and the `fake` providers at the host's seam (run.md, "Dry, ask
//! and fake"), on the simulation. A plan is any sequence of calls a program can make through
//! the table (output, clocks, randomness, a sleep, and the file effects the escape carries),
//! made as a hostile program would make them.
//!
//! - Dry: whatever the plan, the world after a dry run is the world before (probed by a fixed
//!   sequence of reads and directory makes against an untouched twin), and every changing call
//!   is reported, in order, answered as performed.
//! - Ask: a run answered `Pretend` throughout is the dry run, one answered `Yes` throughout
//!   the plain run; one answered `No` throughout performs no changing call and tells none it
//!   succeeded; a run answered by a script withholds exactly what it says and leaves the world
//!   a real run of the calls answered `Yes` leaves.
//! - Fake and record: a run of fakes (a folder in memory, a fixed clock), dry or not, leaves
//!   the world untouched; its record replays on another world byte for byte, with that world
//!   asked nothing.
//!
//! Each layer has planted bugs the oracle catches: dry lets one changing effect through, ask
//! performs one unasked, a `No` reaches the host, a `No` is answered as a success, a replay
//! reaches the host. The plants are the host's own, carried here by stand-in `Plant` variants
//! the controls set (`SyncSkipped`, `CancelSkipsWait`, `CloseSkipsSync`, `JoinDropsFailure`,
//! `PublishBeforeRename`).

use std::{sync::Arc, time::Duration};

use blaze_core::{Done, Effect, FileName, Tag, lock::Lock, wait::Deadline};
use blaze_sim::{Config, Tape};

use super::{check, control};
use crate::{
    Plant, guest,
    hosting::{Hold, Holding, Hosting, Record},
    io::Io,
    layers::{By, Decide, Decision, Fakes, Folder, Question},
    wire::{self, Answer, HostTable, code},
};

const EXCHANGE: usize = 4096;
const NAMES: [&str; 4] = ["a", "b", "d", "d/c"];
/// The fixed clock of the fakes.
const FIXED: u64 = 1_000_000;

fn name(text: &str) -> FileName {
    FileName::new(text).expect("a test's name")
}

/// A simulated world of `seed` with two files in it.
fn world(seed: u64) -> Io {
    let io = Io::sim(seed);
    io.with_sim(|sim| {
        sim.change(&name("a"), b"A");
        sim.change(&name("d/c"), b"C");
    })
    .expect("a simulated world");
    io
}

fn held(io: Io, holding: Holding) -> Box<Hosting> {
    Hosting::holding(io, vec!["layered".to_owned()], Vec::new(), EXCHANGE, holding, blaze_own::testing::heap().shared())
}

fn holding(hold: Hold, dry: bool, ask: Option<Box<dyn Decide + Send>>, fakes: Fakes) -> Holding {
    Holding { hold, dry, ask, fakes }
}

/// One call a program makes.
#[derive(Clone, Debug, PartialEq)]
enum Step {
    Out { bytes: Vec<u8> },
    Clock { which: u32 },
    /// An escaped effect: randomness, a sleep or a file's.
    Escaped { effect: Effect },
}

impl Step {
    /// Whether the call changes the world: the test's own list, not the code's.
    fn changes(&self) -> bool {
        matches!(
            self,
            Step::Escaped {
                effect: Effect::Write { .. } | Effect::Append { .. } | Effect::Remove { .. } | Effect::Rename { .. } | Effect::MakeDir { .. }
            }
        )
    }
}

fn steps(tape: &Tape) -> Vec<Step> {
    let pick = |what| name(tape.one_of(&NAMES, what));
    let bytes = |tape: &Tape| (0..tape.below(12, "a length")).map(|at| b'a' + (at % 26) as u8).collect::<Vec<u8>>();
    let mut steps = Vec::new();
    while steps.is_empty() || tape.more(24, "another call") {
        let tag = Tag(0);
        let effect = match tape.below(10, "a call") {
            0 => {
                steps.push(Step::Out { bytes: bytes(tape) });
                continue;
            }
            1 => {
                steps.push(Step::Clock { which: *tape.one_of(&[wire::clock::MONOTONIC, wire::clock::WALL], "a clock") });
                continue;
            }
            2 => Effect::Random { len: 1 + tape.below(8, "random bytes") as usize, tag },
            3 => Effect::Timer { after: Duration::from_micros(tape.below(50, "a sleep")), tag },
            4 => Effect::ReadAll { file: pick("a file read"), tag },
            5 => Effect::Write { file: pick("a file written"), bytes: bytes(tape), tag },
            6 => Effect::Append { file: pick("a file appended to"), bytes: bytes(tape), tag },
            7 => Effect::Remove { file: pick("a file removed"), tag },
            8 => Effect::Rename { from: pick("a file renamed"), to: pick("its new name"), tag },
            _ => Effect::MakeDir { dir: pick("a directory made"), tag },
        };
        steps.push(Step::Escaped { effect });
    }
    steps
}

/// `step` made through `table`, its bytes put in the exchange first.
#[expect(unsafe_code, reason = "a program's calls through the host's table")]
fn make(hosting: &Hosting, table: &HostTable, step: &Step) -> Answer {
    // SAFETY: the table's own entries and pointer, its `Hosting` alive for the test; every
    // range is inside the exchange.
    unsafe {
        match step {
            Step::Out { bytes } => {
                hosting.put(0, bytes);
                (table.write)(table.host, wire::stream::OUT, 0, bytes.len())
            }
            Step::Clock { which } => (table.clock)(table.host, *which),
            Step::Escaped { effect } => {
                let mut bytes = blaze_own::Vec::new_in(blaze_own::SharedAlloc::global());
                let (_, budget) = guest::codec::effect(&mut bytes, effect).expect("an effect the escape carries");
                hosting.put(0, &bytes);
                (table.effect)(table.host, 0, bytes.len(), budget)
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
                Step::Escaped { .. } if answer.code == code::OK => hosting.head(answer.value as usize),
                Step::Out { .. } | Step::Clock { .. } | Step::Escaped { .. } => Vec::new(),
            };
            (answer, back)
        })
        .collect()
}

/// The world as a fixed sequence of the world's own effects finds it: each name read, then
/// made a directory. Two worlds that answer it alike were left alike by what came before.
fn probe(io: &Io) -> Vec<String> {
    let mut seen = Vec::new();
    for text in NAMES {
        for make in [false, true] {
            let tag = io.submit(|tag| match make {
                false => Effect::ReadAll { file: name(text), tag },
                true => Effect::MakeDir { dir: name(text), tag },
            });
            let until = Deadline::after(io.now(), Duration::from_secs(60));
            seen.push(format!("{text} {make}: {:?}", io.settle(tag, until, "a test's probe")));
        }
    }
    seen
}

/// The world a real run of only `steps` leaves (`world(seed)` at its start).
fn real_world(seed: u64, steps: &[Step]) -> Vec<String> {
    let io = world(seed);
    let hosting = held(io.clone(), Holding::of(Hold::Real));
    made(&hosting, steps);
    probe(&io)
}

/// The places of the changing calls of `steps`.
fn changing(steps: &[Step]) -> Vec<usize> {
    steps.iter().enumerate().filter(|(_, step)| step.changes()).map(|(at, _)| at).collect()
}

/// A withheld call's answer is the completion of its success.
fn answered_as_performed(steps: &[Step], answers: &[(Answer, Vec<u8>)], at: usize) -> Result<(), String> {
    let (answer, back) = &answers[at];
    let done = guest::codec::done(back).map_err(|error| format!("call {at}'s answer is no completion: {error:?}"))?;
    let expected = match &steps[at] {
        Step::Escaped { effect: Effect::Write { .. } } => Done::Written,
        Step::Escaped { effect: Effect::Append { .. } } => Done::Appended,
        Step::Escaped { effect: Effect::Remove { .. } } => Done::Removed,
        Step::Escaped { effect: Effect::Rename { .. } } => Done::Renamed,
        Step::Escaped { effect: Effect::MakeDir { .. } } => Done::MadeDir,
        other => return Err(format!("call {at} withheld, and it changes nothing: {other:?}")),
    };
    match answer.code == code::OK && done == expected {
        true => Ok(()),
        false => Err(format!("withheld call {at} answered {answer:?} {done:?}, not {expected:?}")),
    }
}

/// The dry invariant: whatever the plan, the world is left as it was, and every changing call
/// is reported in order and answered as performed.
fn run_dry(steps: &Vec<Step>, plant: Plant) -> Result<(), String> {
    let io = world(9);
    let hosting = held(io.clone(), holding(Hold::Real, true, None, Fakes::default()));
    let hosting = match plant {
        Plant::SyncSkipped => hosting.plant_dry_lets_through(),
        _ => hosting,
    };
    let answers = made(&hosting, steps);
    let withheld = hosting.withheld();
    let at: Vec<usize> = withheld.iter().map(|it| it.at).collect();
    if at != changing(steps) || withheld.iter().any(|it| it.by != By::Dry) {
        return Err(format!("withheld {withheld:?}, and the changing calls are {:?}", changing(steps)));
    }
    for at in at {
        answered_as_performed(steps, &answers, at)?;
    }
    let (after, before) = (probe(&io), real_world(9, &[]));
    match after == before {
        true => Ok(()),
        false => Err(format!("a dry run changed the world: {after:?} against {before:?}")),
    }
}

/// The invariant: dry runs leave the host world unchanged, `ask` layers obey pretend/yes/no/script decisions without leaking refused effects, and replay performs no host effects.
#[test]
fn a_dry_run_changes_nothing_whatever_the_program_does() {
    blaze_sim::check(Config::cases(200), |tape| check(steps(tape), run_dry));
}

#[test]
fn the_oracle_catches_a_dry_run_that_lets_one_change_through() {
    control(Plant::SyncSkipped, steps, run_dry);
}

/// `ask`'s answers, in turn (the last again once they run out), and every question kept.
struct Script {
    answers: Vec<Decision>,
    asked: Arc<Lock<Vec<usize>>>,
}

impl Decide for Script {
    fn decide(&mut self, question: &Question<'_>) -> Decision {
        let mut asked = self.asked.lock();
        let answer = self.answers.get(asked.len()).or(self.answers.last()).copied().unwrap_or(Decision::No);
        asked.push(question.at);
        answer
    }
}

fn scripted(answers: Vec<Decision>) -> (Box<dyn Decide + Send>, Arc<Lock<Vec<usize>>>) {
    let asked = Arc::new(Lock::new("a test's questions", Vec::new()));
    (Box::new(Script { answers, asked: asked.clone() }), asked)
}

fn asking(tape: &Tape) -> (Vec<Step>, Vec<Decision>) {
    let plan = steps(tape);
    let answers = (0..plan.len()).map(|_| *tape.one_of(&[Decision::Yes, Decision::No, Decision::Pretend], "an answer")).collect();
    (plan, answers)
}

/// `steps` run under `ask` answered by `answers` on `world(seed)`, planted as `plant` says:
/// the host, its world, its answers and the questions it asked.
fn asked_run(seed: u64, steps: &[Step], answers: Vec<Decision>, plant: Plant) -> (Box<Hosting>, Io, Vec<(Answer, Vec<u8>)>, Vec<usize>) {
    let (decide, asked) = scripted(answers);
    let io = world(seed);
    let hosting = held(io.clone(), holding(Hold::Real, false, Some(decide), Fakes::default()));
    let hosting = match plant {
        Plant::CancelSkipsWait => hosting.plant_ask_skips(),
        Plant::CloseSkipsSync => hosting.plant_no_reaches_host(),
        Plant::JoinDropsFailure => hosting.plant_no_says_ok(),
        _ => hosting,
    };
    let answered = made(&hosting, steps);
    let asked = asked.lock().clone();
    (hosting, io, answered, asked)
}

fn withheld(hosting: &Hosting) -> Vec<(usize, By)> {
    hosting.withheld().iter().map(|it| (it.at, it.by)).collect()
}

/// The ask invariants, every one asked about every changing call:
/// - answered `Pretend` throughout it is the dry run, answer for answer and world for world;
/// - answered `Yes` throughout it is the plain run;
/// - answered `No` throughout, no changing call is performed and none is told it succeeded:
///   each gets the wire's refusal, and the report names each as refused;
/// - answered by a script, it withholds exactly what the script says and leaves the world a
///   real run of the calls answered `Yes` leaves.
fn run_ask(plan: &(Vec<Step>, Vec<Decision>), plant: Plant) -> Result<(), String> {
    let (steps, answers) = plan;
    let changes = changing(steps);

    let dry_io = world(11);
    let dry = held(dry_io.clone(), holding(Hold::Real, true, None, Fakes::default()));
    let dry_answers = made(&dry, steps);
    let (pretend, pretend_io, pretend_answers, asked) = asked_run(11, steps, vec![Decision::Pretend], plant);
    let ats = |hosting: &Hosting| withheld(hosting).into_iter().map(|(at, _)| at).collect::<Vec<usize>>();
    if pretend_answers != dry_answers || ats(&pretend) != ats(&dry) || asked != changes || probe(&pretend_io) != probe(&dry_io) {
        return Err(format!("ask answered pretend is not the dry run: asked {asked:?}, withheld {:?} against {:?}", ats(&pretend), ats(&dry)));
    }

    let plain = held(world(13), Holding::of(Hold::Real));
    let plain_answers = made(&plain, steps);
    let (yes, yes_io, yes_answers, asked) = asked_run(13, steps, vec![Decision::Yes], plant);
    if yes_answers != plain_answers || !withheld(&yes).is_empty() || asked != changes || probe(&yes_io) != real_world(13, steps) {
        return Err(format!("ask answered yes is not the plain run: asked {asked:?}, withheld {:?}", withheld(&yes)));
    }

    let (no, no_io, no_answers, asked) = asked_run(14, steps, vec![Decision::No], plant);
    let refused: Vec<(usize, By)> = changes.iter().map(|at| (*at, By::Refused)).collect();
    if let Some(at) = changes.iter().find(|at| no_answers[**at].0 != Answer::refused(code::REFUSED)) {
        return Err(format!("call {at}, answered no, was answered {:?}", no_answers[*at]));
    }
    if withheld(&no) != refused || asked != changes || probe(&no_io) != real_world(14, &[]) {
        return Err(format!("ask answered no reached the world or the report: withheld {:?}", withheld(&no)));
    }

    let (scripted, io, _, asked) = asked_run(12, steps, answers.clone(), plant);
    // The oracle's own walk: what is withheld, and why.
    let answer = |n: usize| answers.get(n).or(answers.last()).copied().unwrap_or(Decision::No);
    let expect: Vec<(usize, By)> = changes
        .iter()
        .enumerate()
        .filter_map(|(n, at)| match answer(n) {
            Decision::Yes => None,
            Decision::No => Some((*at, By::Refused)),
            Decision::Pretend => Some((*at, By::Pretended)),
        })
        .collect();
    if asked != changes || withheld(&scripted) != expect {
        return Err(format!("asked {asked:?} withheld {:?}, and the script says {expect:?}", withheld(&scripted)));
    }
    let let_through: Vec<Step> =
        steps.iter().enumerate().filter(|(at, _)| !expect.iter().any(|(it, _)| it == at)).map(|(_, step)| step.clone()).collect();
    let (after, expected) = (probe(&io), real_world(12, &let_through));
    match after == expected {
        true => Ok(()),
        false => Err(format!("ask left {after:?}, and a real run of what it let through leaves {expected:?}")),
    }
}

#[test]
fn ask_pretend_is_dry_yes_is_plain_no_is_refused_and_a_script_is_obeyed() {
    blaze_sim::check(Config::cases(150), |tape| check(asking(tape), run_ask));
}

#[test]
fn the_oracle_catches_an_effect_performed_unasked() {
    control(Plant::CancelSkipsWait, asking, run_ask);
}

#[test]
fn the_oracle_catches_a_no_that_reaches_the_host() {
    control(Plant::CloseSkipsSync, asking, run_ask);
}

#[test]
fn the_oracle_catches_a_no_answered_as_a_success() {
    control(Plant::JoinDropsFailure, asking, run_ask);
}

/// The report's first line says how many later reads may differ, when a read follows a
/// pretended change of its name.
#[test]
fn a_dry_report_says_when_later_reads_may_differ() {
    let tag = Tag(0);
    let steps = [
        Step::Escaped { effect: Effect::Write { file: name("a"), bytes: b"new".to_vec(), tag } },
        Step::Escaped { effect: Effect::ReadAll { file: name("a"), tag } },
        Step::Escaped { effect: Effect::ReadAll { file: name("b"), tag } },
        Step::Escaped { effect: Effect::ReadAll { file: name("a"), tag } },
    ];
    let hosting = held(world(15), holding(Hold::Real, true, None, Fakes::default()));
    made(&hosting, &steps);
    let report = hosting.report().to_string();
    assert_eq!(report, "2 later reads may differ from a real run\ncall 0: write a: not performed (dry)\n");
    let quiet = held(world(15), holding(Hold::Real, true, None, Fakes::default()));
    made(&quiet, &steps[2..]);
    assert_eq!(quiet.report().to_string(), "");
}

fn fakes() -> Fakes {
    Fakes { clock: Some(FIXED), files: Some(Folder::new().with_file(name("a"), b"fake A".to_vec())), recorded: None }
}

/// The fake-and-record invariant: a run of a fake folder and a fixed clock, dry or not, leaves
/// the world untouched; its record reads back and replays on another world byte for byte,
/// that world asked nothing (its files as they were, nothing written out).
fn run_replay(plan: &(Vec<Step>, bool), plant: Plant) -> Result<(), String> {
    let (steps, dry) = plan;
    let io = world(5);
    let recorder = held(io.clone(), holding(Hold::Record, *dry, None, fakes()));
    let first = made(&recorder, steps);
    if probe(&io) != real_world(5, &[]) {
        return Err("a run of a fake folder changed the world's files".to_owned());
    }
    for (step, (answer, _)) in steps.iter().zip(&first) {
        if matches!(step, Step::Clock { .. }) && *answer != Answer::ok(FIXED) {
            return Err(format!("the fixed clock answered {answer:?}"));
        }
    }
    let record = Record::read(&recorder.record().write(blaze_own::testing::heap().alloc())).map_err(|error| format!("the record does not read back: {error}"))?;

    let elsewhere = world(6);
    let replayed = held(elsewhere.clone(), Holding::of(Hold::Replay(record)));
    let replayed = match plant {
        Plant::PublishBeforeRename => replayed.plant_replay_reaches_host(),
        _ => replayed,
    };
    let again = made(&replayed, steps);
    if again != first || replayed.diverged().is_some() || replayed.unreplayed() != 0 {
        return Err(format!("the replay differs: {again:?} against {first:?}, diverged {:?}", replayed.diverged()));
    }
    let out = elsewhere.script(|host| host.out.clone());
    match (probe(&elsewhere) == real_world(6, &[]), out) {
        (true, Some(out)) if out.is_empty() => Ok(()),
        (files, out) => Err(format!("the replay reached its world: files as they were {files}, written out {out:?}")),
    }
}

fn replaying(tape: &Tape) -> (Vec<Step>, bool) {
    (steps(tape), tape.below(2, "dry") == 1)
}

#[test]
fn a_faked_run_records_and_replays_with_no_host_effect() {
    blaze_sim::check(Config::cases(150), |tape| check(replaying(tape), run_replay));
}

#[test]
fn the_oracle_catches_a_replay_that_reaches_the_host() {
    control(Plant::PublishBeforeRename, replaying, run_replay);
}

/// The fake folder answers as a file tree does, and keeps what was done to it.
#[test]
fn the_fake_folder_keeps_what_the_program_did() {
    let io = world(4);
    let hosting = held(io.clone(), holding(Hold::Real, false, None, fakes()));
    let tag = Tag(0);
    let steps = [
        Step::Escaped { effect: Effect::ReadAll { file: name("a"), tag } },
        Step::Escaped { effect: Effect::Write { file: name("d/e"), bytes: b"new".to_vec(), tag } },
        Step::Escaped { effect: Effect::Rename { from: name("a"), to: name("b"), tag } },
        Step::Escaped { effect: Effect::Remove { file: name("a"), tag } },
    ];
    let answers = made(&hosting, &steps);
    let done: Vec<Done> = answers.iter().map(|(_, back)| guest::codec::done(back).expect("a completion")).collect();
    assert_eq!(done[0], Done::Read(b"fake A".to_vec()));
    assert_eq!(done[1..3], [Done::Written, Done::Renamed]);
    assert!(matches!(&done[3], Done::Failed(why) if why.0.contains("no file `a`")), "{:?}", done[3]);
    let folder = hosting.folder().expect("the run's folder");
    assert_eq!(folder.file(&name("b")), Some(&b"fake A"[..]));
    assert_eq!(folder.file(&name("d/e")), Some(&b"new"[..]));
    assert_eq!(folder.file(&name("a")), None);
    assert_eq!(probe(&io), real_world(4, &[]), "the world is untouched");
}
