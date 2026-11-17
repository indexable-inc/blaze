//! Where user code gets its randomness and its wall clock
//! (docs/v2/plan/design/determinism.md).
//!
//! User code blaze runs at build time is given a [`Provider`]: a seed made of the run's key
//! and what `SystemTime::now` says. This is the one home of the policy on blaze's side:
//!
//! - [`Provider::of`] makes a run's provider from its [`RunKey`], so the seed adds no input to
//!   the key, and from `SOURCE_DATE_EPOCH`, which the run then reads as an input.
//! - [`Provider::entry`] is the variable the run's processes are started with; blaze's std
//!   reads it (`library/std/src/sys/provider`) and derives everything it hands the program.
//! - [`Answers`] is what the sandbox answers a process that asks the kernel directly
//!   (`getrandom`, an open of `/dev/urandom`): a state machine with no IO, which the Linux
//!   supervisor drives.
//! - [`derive`] is std's own file, included here by path: what a seed gives is written once.

use std::{alloc::Global, ffi::OsStr, ffi::OsString};

use blaze_hash::HashMap;
use blaze_ids::{ContentId, Hasher};

use crate::{RunKey, RunKind};

/// What a seed gives: std's file, the one home of the derivation.
#[path = "../../../library/std/src/sys/provider/derive.rs"]
pub mod derive;

pub use derive::{Seed, VAR};

/// The variable a build names its time in (reproducible-builds.org): the seconds since the
/// Unix epoch.
pub const SOURCE_DATE_EPOCH: &str = "SOURCE_DATE_EPOCH";

/// What `SystemTime::now` says in a run.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Wall {
    /// The system's clock: reading it is what can't be traced
    /// ([`blaze_core::Untraceable::Clock`]).
    System,
    /// These seconds since the Unix epoch, however often it is asked: the build's
    /// `SOURCE_DATE_EPOCH`, which the run reads as a variable.
    Fixed(i64),
}

/// A run's provider.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Provider {
    pub seed: Seed,
    pub wall: Wall,
}

impl Provider {
    const DOMAIN: &'static str = "blaze v2 provider seed 1";

    /// The provider of the run of `key`, of `kind`, in a build whose environment has
    /// `source_date_epoch` (the variable's value, if it is set).
    ///
    /// The seed is a hash of the key: two runs of one key have one seed, and another key has
    /// another. The wall clock, by kind:
    ///
    /// - a build script and a proc macro are told `SOURCE_DATE_EPOCH` when the build sets it
    ///   (what the variable is for: the time a build is to say it was made), and the
    ///   system's clock otherwise;
    /// - a test is always told the system's clock: tests time out, sleep and check deadlines
    ///   against it.
    ///
    /// A `SOURCE_DATE_EPOCH` that is not a whole number of seconds is an error naming it.
    pub fn of(
        kind: RunKind,
        key: &RunKey,
        source_date_epoch: Option<&OsStr>,
    ) -> Result<Provider, String> {
        let mut hasher = Hasher::new(Provider::DOMAIN);
        hasher.update(&[kind.byte()]);
        hasher.update(&key.code.0);
        hasher.update(&key.given.0);
        let seed = hasher.finish().0;
        let wall = match (kind, source_date_epoch) {
            (RunKind::Test, _) | (_, None) => Wall::System,
            (RunKind::BuildScript | RunKind::ProcMacro, Some(value)) => {
                let seconds = value.to_str().and_then(|it| it.parse::<i64>().ok()).ok_or_else(
                    || {
                        // alloc: error path
                        format!(
                            "`{SOURCE_DATE_EPOCH}` is `{}`, which is not a whole number of seconds since the Unix epoch: it is the time blaze tells build scripts and proc macros",
                            value.display()
                        )
                    },
                )?;
                Wall::Fixed(seconds)
            }
        };
        Ok(Provider { seed, wall })
    }

    /// The provider of the processes of a sandboxed test binary of `suite`: what every test of
    /// the binary has in its base key (blaze-testsel's `host::suite`: the toolchain, the
    /// suite's settings and the environment blaze gives its tests). The wall clock is the
    /// system's, as [`Provider::of`] says for a test: a test that reads it is not kept.
    ///
    /// The seed adds no input to a test's key: it is a hash of `suite`, which is made of
    /// inputs every test's base key already covers, so two runs of one base have one seed.
    /// Each test is answered from a stream of its own scope ([`Answers::marked`]), derived
    /// from this seed and its libtest name, which its base key covers too.
    pub fn of_suite(suite: &ContentId) -> Provider {
        let mut hasher = Hasher::new(Provider::DOMAIN);
        hasher.update(&[RunKind::Test.byte()]);
        hasher.update(&suite.0);
        Provider { seed: hasher.finish().0, wall: Wall::System }
    }

    /// The variable the run's processes are started with: its name and its value.
    pub fn entry(&self) -> (OsString, OsString) {
        let mut value = [0; derive::VALUE_MOST];
        let wall = match self.wall {
            Wall::System => None,
            Wall::Fixed(seconds) => Some(seconds),
        };
        let len = derive::encode(&self.seed, wall, &mut value);
        // The encoder writes ASCII alone.
        let text = String::from_utf8_lossy(&value[..len]);
        // alloc: the edge, once per run: the command owns its environment
        (OsString::from(VAR), OsString::from(text.as_ref()))
    }

    /// The provider `value`, a value of [`VAR`], names; `None` for one this blaze did not
    /// write.
    pub fn parse(value: &[u8]) -> Option<Provider> {
        let (seed, wall) = derive::decode(value)?;
        Some(Provider { seed, wall: wall.map_or(Wall::System, Wall::Fixed) })
    }

    /// The provider among `env`, a run's environment: the first [`VAR`] in it, which only
    /// blaze sets. A value that is not one this blaze writes is an error naming it.
    pub fn among<'a>(
        env: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Result<Option<Provider>, String> {
        let Some((_, value)) = env.into_iter().find(|(name, _)| *name == VAR) else {
            return Ok(None);
        };
        match Provider::parse(value.as_bytes()) {
            Some(provider) => Ok(Some(provider)),
            // alloc: error path
            None => Err(format!(
                "`{VAR}` is `{value}`, which is not a provider this blaze writes (`1:<64 hex digits>:<os or seconds>`)"
            )),
        }
    }
}

/// The most opens of a random device one process is answered: each takes an index of the
/// process's 2^24.
pub const DEVICE_OPENS_MOST: u64 = 1 << 24;

/// What the sandbox answers the processes of one run that ask the kernel for randomness: a
/// state machine, with no IO and no clock.
///
/// Each process has a stream of its own and is given the stream's next bytes each time it
/// asks. Which stream:
///
/// - a process of no scope (a build script's, a test binary's fork server before any test):
///   the stream of its number in the run, a function of the run's seed and that number;
/// - a process of a scope (a test of a fork server, whose child took the scope's mark with
///   the test's libtest name, and every process it starts): a stream of the scope, a
///   function of the seed, the scope's name and the process's place in the scope: the
///   process that took the mark is the first ([`Answers::marked`]), the others are numbered
///   in the order they first ask. The process's number in the run is not used: it follows
///   the order the server forked its tests in, which which tests were asked and in what
///   order decide, so by it a test's bytes would differ between a full run and a rerun of it
///   alone.
///
/// What a process is given therefore does not depend on when another process asked, in which
/// order the supervisor saw them, how the process cut its reads, or, in a scope, how many
/// tests ran before its test and which.
///
/// The reference model: [`Answers::model`] and [`Answers::model_in`], which say what the
/// `n`th byte a process is given is with no state at all.
#[derive(Debug)]
pub struct Answers {
    seed: Seed,
    /// How many bytes of its `getrandom` stream each stream has given, by its seed (the run's
    /// or a scope's) and its number under it.
    taken: HashMap<(Seed, u64), u64, Global>,
    /// How many opens of a random device each stream has answered, keyed as `taken` is.
    opened: HashMap<(Seed, u64), u64, Global>,
    /// The place of each process of a scope in it, by the scope's seed and the process's id
    /// as the supervisor sees it.
    places: HashMap<(Seed, u64), u64, Global>,
    /// How many processes of each scope have a place, by the scope's seed.
    placed: HashMap<Seed, u64, Global>,
    /// Test-only: the bug to plant.
    plant: Plant,
}

/// A bug [`Answers`] can be made to have, for its tests' controls.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Plant {
    #[default]
    Nothing,
    /// One count for all processes: what a process is given depends on who asked before it.
    SharedCount,
    /// A process of a scope is given the stream of its number in the run, as a process of no
    /// scope is: what a test is given depends on the order its server forked.
    ByNumber,
}

/// A process that asks, as the supervisor sees it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Asker<'a> {
    /// The scope the asking thread runs in (the libtest name its process's mark gave), empty
    /// for none.
    pub scope: &'a str,
    /// Its process's id as the supervisor sees it: what [`Answers::marked`] is told.
    pub id: u64,
    /// Its process's number in the run (its id in the sandbox's PID namespace): the stream of
    /// a process of no scope.
    pub number: u64,
}

/// The domain of a scope's seed.
const SCOPE_DOMAIN: &str = "blaze v2 provider scope 1";

impl Answers {
    pub fn new(provider: &Provider) -> Answers {
        Answers::planted(provider, Plant::Nothing)
    }

    /// [`Answers::new`] with a bug planted: for tests' controls only.
    pub fn planted(provider: &Provider, plant: Plant) -> Answers {
        Answers {
            seed: provider.seed,
            taken: HashMap::new_in(Global),
            opened: HashMap::new_in(Global),
            places: HashMap::new_in(Global),
            placed: HashMap::new_in(Global),
            plant,
        }
    }

    /// The seed of the scope `scope` under the run's `seed`: a function of the two alone.
    pub fn scope_seed(seed: &Seed, scope: &str) -> Seed {
        let mut hasher = Hasher::new(SCOPE_DOMAIN);
        hasher.update(seed).part(scope.as_bytes());
        hasher.finish().0
    }

    /// The process `id` took the mark of `scope` (a fork server's child, about to run the
    /// test of that name): it is the first of the scope, and the scope's streams start over,
    /// so a test run again in the same server (a retry) is given what its first run was. An
    /// empty scope (a mark that leaves its scope) changes nothing.
    pub fn marked(&mut self, scope: &str, id: u64) {
        if scope.is_empty() {
            return;
        }
        let seed = Answers::scope_seed(&self.seed, scope);
        self.places.retain(|key, _| key.0 != seed);
        self.taken.retain(|key, _| key.0 != seed);
        self.opened.retain(|key, _| key.0 != seed);
        self.places.insert((seed, id), 0);
        self.placed.insert(seed, 1);
    }

    /// The stream `asker` is given: its seed and its number under it.
    fn stream(&mut self, asker: Asker<'_>) -> (Seed, u64) {
        if asker.scope.is_empty() || self.plant == Plant::ByNumber {
            return (self.seed, asker.number);
        }
        let seed = Answers::scope_seed(&self.seed, asker.scope);
        if let Some(&place) = self.places.get(&(seed, asker.id)) {
            return (seed, place);
        }
        let placed = self.placed.entry(seed).or_insert(0);
        let place = *placed;
        *placed += 1;
        self.places.insert((seed, asker.id), place);
        (seed, place)
    }

    /// Answers a `getrandom` of `asker`: the next `into.len()` bytes of its stream.
    pub fn random(&mut self, asker: Asker<'_>, into: &mut [u8]) {
        let (seed, stream) = self.stream(asker);
        let counted = match self.plant {
            Plant::Nothing | Plant::ByNumber => (seed, stream),
            Plant::SharedCount => (self.seed, 0),
        };
        let taken = self.taken.entry(counted).or_insert(0);
        derive::fill(&seed, derive::SYSTEM, stream, *taken, into);
        *taken = taken.wrapping_add(into.len() as u64);
    }

    /// Answers an open of `/dev/urandom` or `/dev/random` by `asker`: the bytes the file it
    /// is handed holds, `into.len()` of them, another stream for each open. `None` once its
    /// stream has opened [`DEVICE_OPENS_MOST`]: then the open is not answered.
    pub fn device(&mut self, asker: Asker<'_>, into: &mut [u8]) -> Option<()> {
        let (seed, stream) = self.stream(asker);
        let opened = self.opened.entry((seed, stream)).or_insert(0);
        if *opened >= DEVICE_OPENS_MOST {
            return None;
        }
        let device = stream.wrapping_mul(DEVICE_OPENS_MOST).wrapping_add(*opened);
        *opened += 1;
        derive::fill(&seed, derive::DEVICE, device, 0, into);
        Some(())
    }

    /// The reference model: the byte at `offset` of what the process numbered `process` in
    /// the run, of no scope, is given by `getrandom`, whatever anyone asked before.
    pub fn model(seed: &Seed, process: u64, offset: u64) -> u8 {
        derive::word(seed, derive::SYSTEM, process, offset / 8).to_le_bytes()[(offset % 8) as usize]
    }

    /// The reference model in a scope: the byte at `offset` of what the process at `place` in
    /// `scope` (0: the one that took its mark) is given by `getrandom` since the mark,
    /// whatever anyone asked before and whichever tests ran before it.
    pub fn model_in(seed: &Seed, scope: &str, place: u64, offset: u64) -> u8 {
        Answers::model(&Answers::scope_seed(seed, scope), place, offset)
    }
}

#[cfg(test)]
mod tests {
    use blaze_ids::ContentId;
    use blaze_sim::{Config, Tape};

    use super::*;

    fn key(code: u8, given: u8) -> RunKey {
        RunKey { code: ContentId([code; 32]), given: ContentId([given; 32]) }
    }

    fn provider(code: u8) -> Provider {
        Provider::of(RunKind::BuildScript, &key(code, 0), None).unwrap()
    }

    /// The derivation is the one std has, held to values worked out apart from it (SipHash-1-3
    /// by its definition, keyed by the seed's first half, over its second half and three
    /// words): a change to the file changes what every kept run was given.
    #[test]
    fn a_seed_gives_the_words_siphash_gives() {
        let seed: Seed = std::array::from_fn(|at| at as u8);
        assert_eq!(derive::word(&seed, derive::HASH, 7, 0), 0x859b_208e_3b20_9cb3);
        assert_eq!(derive::keys(&seed, 7), (0x859b_208e_3b20_9cb3, 0x3a42_bab5_1248_2c4e));
        assert_eq!(derive::word(&seed, derive::SYSTEM, 2, 0), 0x46c6_205e_d338_b6a7);
        let child = derive::child(&seed, 0);
        let hex: String = child.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(hex, "b846f60a6ac469afe80880b8eb7e69677cda39b655307b7c61f06816c329d73c");
    }

    #[test]
    fn one_key_has_one_seed_and_another_key_another() {
        let of = |kind, key: RunKey| Provider::of(kind, &key, None).unwrap().seed;
        assert_eq!(of(RunKind::BuildScript, key(1, 2)), of(RunKind::BuildScript, key(1, 2)));
        assert_ne!(of(RunKind::BuildScript, key(1, 2)), of(RunKind::BuildScript, key(3, 2)));
        assert_ne!(of(RunKind::BuildScript, key(1, 2)), of(RunKind::BuildScript, key(1, 3)));
        assert_ne!(of(RunKind::BuildScript, key(1, 2)), of(RunKind::ProcMacro, key(1, 2)));
        // So the keys of two scripts' hash maps differ, and one script's are the same on
        // every run.
        let (a, b) = (provider(1).seed, provider(2).seed);
        assert_eq!(derive::keys(&a, 1), derive::keys(&provider(1).seed, 1));
        assert_ne!(derive::keys(&a, 1), derive::keys(&b, 1));
        assert_ne!(derive::keys(&a, 1), derive::keys(&a, 2));
    }

    #[test]
    fn a_child_has_a_seed_of_its_own_that_is_the_same_on_every_run() {
        let parent = provider(1).seed;
        let first = derive::child(&parent, 0);
        assert_eq!(first, derive::child(&parent, 0));
        assert_ne!(first, parent);
        assert_ne!(first, derive::child(&parent, 1));
        assert_ne!(derive::child(&first, 0), derive::child(&parent, 0));
    }

    #[test]
    fn the_wall_clock_is_the_builds_for_a_script_and_the_systems_for_a_test() {
        let epoch = OsStr::new("1700000000");
        let of = |kind, value| Provider::of(kind, &key(1, 1), value).map(|it| it.wall);
        assert_eq!(of(RunKind::BuildScript, Some(epoch)), Ok(Wall::Fixed(1_700_000_000)));
        assert_eq!(of(RunKind::ProcMacro, Some(epoch)), Ok(Wall::Fixed(1_700_000_000)));
        assert_eq!(of(RunKind::BuildScript, None), Ok(Wall::System));
        assert_eq!(of(RunKind::Test, Some(epoch)), Ok(Wall::System));
        let refused = of(RunKind::BuildScript, Some(OsStr::new("yesterday"))).unwrap_err();
        assert!(refused.contains("`SOURCE_DATE_EPOCH` is `yesterday`"), "{refused}");
    }

    #[test]
    fn a_provider_is_read_back_from_its_variable() {
        for wall in [Wall::System, Wall::Fixed(0), Wall::Fixed(-7), Wall::Fixed(i64::MAX), Wall::Fixed(i64::MIN)] {
            let provider = Provider { seed: provider(9).seed, wall };
            let (name, value) = provider.entry();
            assert_eq!(name, VAR);
            assert_eq!(Provider::parse(value.as_encoded_bytes()), Some(provider), "{value:?}");
            let text = value.to_str().unwrap();
            assert_eq!(Provider::among([("A", "b"), (VAR, text)]), Ok(Some(provider)));
        }
        assert_eq!(Provider::among([("A", "b")]), Ok(None));
        let refused = Provider::among([(VAR, "2:00:os")]).unwrap_err();
        assert!(refused.contains("`__BLAZE_PROVIDER` is `2:00:os`"), "{refused}");
    }

    #[test]
    fn a_stream_does_not_depend_on_how_its_reads_are_cut() {
        let seed = provider(4).seed;
        let mut whole = [0u8; 61];
        derive::fill(&seed, derive::FILL, 3, 0, &mut whole);
        let mut cut = [0u8; 61];
        let mut at = 0;
        for len in [1, 7, 8, 9, 13, 23] {
            derive::fill(&seed, derive::FILL, 3, at as u64, &mut cut[at..at + len]);
            at += len;
        }
        assert_eq!(at, 61);
        assert_eq!(whole, cut);
    }

    /// What every process was given by `asks` (who asks, and for how many bytes), against the
    /// model: why it is not, if it is not.
    fn answers_are_the_models(plant: Plant, asks: &[(u8, u8)]) -> Result<(), String> {
        let provider = provider(5);
        let mut answers = Answers::planted(&provider, plant);
        let mut given: std::collections::BTreeMap<u64, Vec<u8>> = std::collections::BTreeMap::new();
        for &(process, len) in asks {
            let mut bytes = vec![0; usize::from(len)];
            answers.random(unscoped(u64::from(process)), &mut bytes);
            given.entry(u64::from(process)).or_default().extend(bytes);
        }
        for (process, bytes) in &given {
            for (offset, byte) in bytes.iter().enumerate() {
                let model = Answers::model(&provider.seed, *process, offset as u64);
                if *byte != model {
                    return Err(format!(
                        "process {process} was given {byte:#04x} as byte {offset} of its randomness, and the model says {model:#04x}"
                    ));
                }
            }
        }
        Ok(())
    }

    fn asks(tape: &Tape) -> Vec<(u8, u8)> {
        let who: Vec<u8> = tape.vec(0..=48, "who", |tape| tape.int::<u8>("process"));
        let lens: Vec<u8> = tape.vec(0..=48, "lens", |tape| tape.int::<u8>("len"));
        // A few processes, so they interleave.
        who.into_iter().map(|it| it % 4).zip(lens).collect()
    }

    /// The invariant: what a process is given by `getrandom` is its own stream's bytes in
    /// order, a function of the seed and the process alone, however the asks of the run's
    /// processes interleave and however each cut its own.
    #[test]
    fn a_process_is_given_its_own_stream_whoever_else_asks() {
        blaze_sim::check(Config::DEFAULT, |tape| answers_are_the_models(Plant::Nothing, &asks(tape)));
    }

    #[test]
    fn the_oracle_catches_a_count_shared_by_all_processes() {
        // Process 1 asks after process 0 took eight bytes: with one count it is given its
        // stream from byte 8 on.
        let caught = answers_are_the_models(Plant::SharedCount, &[(0, 8), (1, 8)]);
        assert!(caught.is_err_and(|why| why.contains("process 1")));
    }

    #[test]
    fn each_open_of_a_device_is_another_stream_and_the_same_on_every_run() {
        let run = || {
            let mut answers = Answers::new(&provider(6));
            let mut opens = [[0u8; 32]; 3];
            answers.device(unscoped(1), &mut opens[0]).unwrap();
            answers.device(unscoped(1), &mut opens[1]).unwrap();
            answers.device(unscoped(2), &mut opens[2]).unwrap();
            opens
        };
        let opens = run();
        assert_eq!(opens, run());
        assert_ne!(opens[0], opens[1]);
        assert_ne!(opens[0], opens[2]);
        assert_ne!(opens[1], opens[2]);
    }

    /// A process of no scope, numbered `number` in the run, and the same as its id.
    fn unscoped(number: u64) -> Asker<'static> {
        Asker { scope: "", id: number, number }
    }

    /// The tests of the simulated binary, by their libtest names.
    const TESTS: [&str; 5] = ["tests::adds", "tests::muls", "tests::both", "a::b", "a::c"];

    /// A fork server of [`TESTS`] run as the tape says, through `Answers` as the supervisor
    /// drives it: the server (number 2 in the sandbox's PID namespace, id 102 as the
    /// supervisor sees it) asks first, as std's start would without a provider; then the
    /// tests the tape picks, in the order it picks, each in a child forked in turn (the next
    /// number and id: so a test's number is its place in the fork order), which takes its mark
    /// and asks a few times, cutting its reads as the tape says; some run a second process
    /// that asks too. What each test's first process was given, by test.
    fn serve(tape: &Tape, plant: Plant) -> Vec<(&'static str, Vec<u8>)> {
        let mut answers = Answers::planted(&Provider::of_suite(&ContentId([7; 32])), plant);
        let mut start = [0u8; 16];
        answers.random(Asker { scope: "", id: 102, number: 2 }, &mut start);
        let mut order: Vec<usize> = (0..TESTS.len()).collect();
        let mut picked = Vec::new();
        while !order.is_empty() && tape.bool("another test") {
            let at = usize::from(tape.int::<u8>("which")) % order.len();
            picked.push(order.remove(at));
        }
        let mut given = Vec::new();
        for (forked, &test) in picked.iter().enumerate() {
            let number = 3 + 2 * forked as u64;
            let (id, scope) = (100 + number, TESTS[test]);
            answers.marked(scope, id);
            let mut bytes = Vec::new();
            for _ in 0..1 + tape.int::<u8>("asks") % 3 {
                let mut cut = vec![0u8; 1 + usize::from(tape.int::<u8>("len") % 24)];
                answers.random(Asker { scope, id, number }, &mut cut);
                bytes.extend(cut);
                if tape.bool("a child of the test asks") {
                    let mut theirs = [0u8; 8];
                    answers.random(Asker { scope, id: id + 1, number: number + 1 }, &mut theirs);
                }
            }
            given.push((scope, bytes));
        }
        given
    }

    /// Whether what each test was given is what the model says its own stream is: its scope's
    /// first, from its start.
    fn tests_are_given_their_own_streams(given: &[(&str, Vec<u8>)]) -> Result<(), String> {
        let seed = Provider::of_suite(&ContentId([7; 32])).seed;
        for (scope, bytes) in given {
            for (offset, byte) in bytes.iter().enumerate() {
                let model = Answers::model_in(&seed, scope, 0, offset as u64);
                if *byte != model {
                    return Err(format!(
                        "test `{scope}` was given {byte:#04x} as byte {offset} of its randomness, and its own stream has {model:#04x}"
                    ));
                }
            }
        }
        Ok(())
    }

    /// The invariant: what a test of a fork server is given by `getrandom` is a function of
    /// the suite's seed (made of what every test's base key covers) and the test's name
    /// alone: the same whichever tests ran before it, in whatever order, with the server's
    /// own start before them and the test's own children asking in between. So a pass kept
    /// of a full run is a pass on the bytes a rerun of the test alone is given.
    #[test]
    fn a_test_is_given_the_stream_of_its_name_whatever_ran_before_it() {
        blaze_sim::check(Config::DEFAULT, |tape| {
            tests_are_given_their_own_streams(&serve(tape, Plant::Nothing))
        });
    }

    /// The control: by its number in the run (its place in the fork order) a test is given
    /// other bytes when another test ran before it, which the oracle catches.
    #[test]
    fn the_oracle_catches_a_test_given_the_stream_of_its_fork_order() {
        let mut answers =
            Answers::planted(&Provider::of_suite(&ContentId([7; 32])), Plant::ByNumber);
        let mut first = [0u8; 8];
        answers.marked("tests::adds", 103);
        answers.random(Asker { scope: "tests::adds", id: 103, number: 3 }, &mut [0u8; 8]);
        answers.marked("tests::muls", 105);
        answers.random(Asker { scope: "tests::muls", id: 105, number: 5 }, &mut first);
        let caught = tests_are_given_their_own_streams(&[("tests::muls", first.to_vec())]);
        assert!(caught.is_err_and(|why| why.contains("tests::muls")));
    }

    /// The same test alone and after others, and twice in one server (a retry), is given the
    /// same bytes; two tests are given different ones; and the stream of a scope is not that
    /// of the run's number the process has.
    #[test]
    fn a_test_alone_after_others_and_retried_is_given_the_same_bytes() {
        let provider = Provider::of_suite(&ContentId([7; 32]));
        let of = |answers: &mut Answers, scope: &'static str, number: u64| {
            answers.marked(scope, 100 + number);
            let mut bytes = [0u8; 32];
            answers.random(Asker { scope, id: 100 + number, number }, &mut bytes);
            bytes
        };
        let mut alone = Answers::new(&provider);
        let muls = of(&mut alone, "tests::muls", 3);
        let mut full = Answers::new(&provider);
        let adds = of(&mut full, "tests::adds", 3);
        assert_eq!(of(&mut full, "tests::muls", 4), muls);
        assert_eq!(of(&mut full, "tests::muls", 5), muls, "a retry starts over");
        assert_ne!(adds, muls);
        let mut by_number = [0u8; 32];
        Answers::new(&provider).random(unscoped(3), &mut by_number);
        assert_ne!(by_number, muls);
    }

    /// A suite's provider is a function of the suite alone: another suite has another seed,
    /// the wall clock is the system's, and it is read back from its variable.
    #[test]
    fn a_suites_provider_is_its_suites_and_tells_the_systems_time() {
        let one = Provider::of_suite(&ContentId([1; 32]));
        assert_eq!(one, Provider::of_suite(&ContentId([1; 32])));
        assert_ne!(one.seed, Provider::of_suite(&ContentId([2; 32])).seed);
        assert_eq!(one.wall, Wall::System);
        let (_, value) = one.entry();
        assert_eq!(Provider::parse(value.as_encoded_bytes()), Some(one));
    }
}
