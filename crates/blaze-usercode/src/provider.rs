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
use blaze_ids::Hasher;

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
/// Each process has a stream of its own, a function of the seed and of the process's number
/// alone, and is given the stream's next bytes each time it asks. What a process is given
/// therefore does not depend on when another process asked, in which order the supervisor
/// saw them, or how the process cut its reads.
///
/// The reference model: [`Answers::model`], which says what the `n`th byte a process is given
/// is with no state at all.
#[derive(Debug)]
pub struct Answers {
    seed: Seed,
    /// How many bytes of its `getrandom` stream each process has been given.
    taken: HashMap<u64, u64, Global>,
    /// How many opens of a random device each process has been answered.
    opened: HashMap<u64, u64, Global>,
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
}

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
            plant,
        }
    }

    /// Answers a `getrandom` of `process` (its number in the run: its process id in the
    /// sandbox's namespace): the next `into.len()` bytes of its stream.
    pub fn random(&mut self, process: u64, into: &mut [u8]) {
        let counted = match self.plant {
            Plant::Nothing => process,
            Plant::SharedCount => 0,
        };
        // alloc: one entry per process that asks, for the run
        let taken = self.taken.entry(counted).or_insert(0);
        derive::fill(&self.seed, derive::SYSTEM, process, *taken, into);
        *taken = taken.wrapping_add(into.len() as u64);
    }

    /// Answers an open of `/dev/urandom` or `/dev/random` by `process`: the bytes the file it
    /// is handed holds, `into.len()` of them, another stream for each open. `None` once the
    /// process has opened [`DEVICE_OPENS_MOST`]: then the open is not answered.
    pub fn device(&mut self, process: u64, into: &mut [u8]) -> Option<()> {
        // alloc: one entry per process that opens one, for the run
        let opened = self.opened.entry(process).or_insert(0);
        if *opened >= DEVICE_OPENS_MOST {
            return None;
        }
        let stream = process.wrapping_mul(DEVICE_OPENS_MOST).wrapping_add(*opened);
        *opened += 1;
        derive::fill(&self.seed, derive::DEVICE, stream, 0, into);
        Some(())
    }

    /// The reference model: the byte at `offset` of what `process` is given by `getrandom`,
    /// whatever anyone asked before.
    pub fn model(seed: &Seed, process: u64, offset: u64) -> u8 {
        derive::word(seed, derive::SYSTEM, process, offset / 8).to_le_bytes()[(offset % 8) as usize]
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
            answers.random(u64::from(process), &mut bytes);
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
            answers.device(1, &mut opens[0]).unwrap();
            answers.device(1, &mut opens[1]).unwrap();
            answers.device(2, &mut opens[2]).unwrap();
            opens
        };
        let opens = run();
        assert_eq!(opens, run());
        assert_ne!(opens[0], opens[1]);
        assert_ne!(opens[0], opens[2]);
        assert_ne!(opens[1], opens[2]);
    }
}
