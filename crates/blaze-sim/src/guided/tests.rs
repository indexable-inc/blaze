//! The guided phase, tested with the engine itself: a needle behind three nested byte
//! comparisons, which plain random search misses, the guided phase finds, and a guided phase
//! that ignores the coverage signal (the planted bug) misses; and the corpus's form.

#![expect(
    blaze::error_debug,
    reason = "a test's failure message shows the values it compared as they are"
)]

use super::{CORPUS_FORM, CorpusError, Guided, GuidedConfig, GuidedError, Plant};
use crate::check::{check, search};
use crate::search::{Config, Ended, Failure, Next, Origin, Verdict};
use crate::tape::{Record, Tail, Tape};

/// Cases each search may run: test.md piece 8's "within 10,000 cases".
const BUDGET: u32 = 10_000;

/// Words of coverage the bodies here report: points 0 to 63.
const WORDS: usize = 1;

/// The bytes behind the needle's three comparisons.
const NEEDLE: [u64; 3] = [0x42, 0x13, 0xA7];

/// What the needle says when it is found.
const FOUND: &str = "the needle: 0x42 0x13 0xA7";

/// The bug: three nested byte comparisons, each byte drawn only once the comparison before it
/// held. Coverage: point 0 on entry, then point `k + 1` once comparison `k` held. Plain random
/// search meets it once in 2^24 cases (about 0.0006 within [`BUDGET`]); a phase that keeps the
/// tape of each new point needs about 256 cases per comparison, divided by the share of its
/// mutations that change the right byte (guided.rs's module doc; about 2,200 cases in all,
/// estimate, to verify by this test).
fn needle(tape: &Tape, coverage: &mut [u64]) -> Result<(), String> {
    coverage[0] |= 1;
    for (depth, (&byte, label)) in NEEDLE.iter().zip(["first", "second", "third"]).enumerate() {
        if tape.below(256, label) != byte {
            return Ok(());
        }
        coverage[0] |= 1 << (depth + 1);
    }
    Err(FOUND.to_owned())
}

/// One run of a guided phase: its verdict, the corpus it ended with, and a digest of every tape
/// it proposed (prefix and tail), which two runs of one seed share.
struct Run {
    verdict: Verdict,
    kept: u32,
    digest: u64,
}

/// Runs `guided` over `body`, the case's coverage cleared before each case and handed back after
/// it. The bodies here do not panic, so none is caught.
fn drive(
    guided: &mut Guided<'_>,
    mut body: impl FnMut(&Tape, &mut [u64]) -> Result<(), String>,
) -> Run {
    let tape = Tape::new(&[], Tail::Calm, guided.most_choices());
    let mut record = Record::new();
    let mut coverage = [0u64; WORDS];
    let mut digest = 0u64;
    let most_runs = guided.most_runs();
    for _ in 0..=most_runs {
        let proposal = match guided.next() {
            Next::Done(verdict) => {
                let verdict = verdict.clone();
                return Run { verdict, kept: guided.kept(), digest };
            }
            Next::Run(proposal) => proposal,
        };
        for &value in proposal.prefix {
            digest = blaze_rng::mix(digest ^ value);
        }
        digest = blaze_rng::mix(match &proposal.tail {
            Tail::Calm => digest,
            Tail::Seeded(stream) => digest ^ stream.state() ^ 1,
        });
        tape.reset(proposal.prefix, proposal.tail);
        coverage.fill(0);
        let ended = match body(&tape, &mut coverage) {
            Ok(()) => Ended::Passed,
            Err(text) => Ended::Failed(Failure { origin: Origin::Error, text }),
        };
        tape.finish(&mut record);
        if let Err(error) = guided.ran(&record, ended, &coverage) {
            panic!("the guided phase refused a case it proposed: {error}");
        }
    }
    panic!("the guided phase proposed more than its {most_runs} runs")
}

/// The guided phase's config for the needle: [`BUDGET`] cases from `seed`, a corpus of 64 tapes
/// of 16 choices.
fn needle_config(seed: u64) -> GuidedConfig {
    GuidedConfig::new(1).cases(BUDGET).seed(seed).corpus_tapes(64).tape_choices(16)
}

/// Plain random search misses the needle within the budget, the guided phase finds it within
/// the same budget and shrinks it to exactly its three bytes, a second run of one seed proposes
/// the same tapes, and the control, a guided phase that ignores the coverage signal (it keeps
/// every tape), misses it within the same budget.
///
/// Prints, once for each of the three seeds the outer check draws (N and K fixed by the seed;
/// N at most 10000, about 2,200 expected; to verify by this test):
/// `guided phase, seed S: the needle at case N of 10000 (corpus K tapes); plain search: none in 10000 cases; coverage-blind phase: none in 10000 cases`
#[test]
#[expect(
    clippy::print_stdout,
    reason = "the test prints the cases each search needed, which is what it measures"
)]
fn guided_phase_finds_a_needle_that_plain_and_coverage_blind_search_miss() {
    check(Config::cases(3).shrink_runs(0), |tape: &Tape| {
        let seed = tape.next_u64("guided seed");

        // Plain random search: seeds `seed..seed + BUDGET`.
        let plain = search(Config::cases(BUDGET).first(seed).shrink_runs(0), |tape: &Tape| {
            needle(tape, &mut [0u64; WORDS])
        });
        let Verdict::Passed(plain) = &plain else {
            return Err(format!(
                "plain random search from seed {seed} found the needle, which it meets once in 2^24 cases: {plain:?}"
            ));
        };

        let config = needle_config(seed);
        let mut memory = vec![0u64; config.memory_words().expect("the needle's config fits")];

        // The guided phase.
        let found = {
            let mut guided =
                Guided::new(config.clone(), &mut memory).expect("the memory is the config's");
            drive(&mut guided, needle)
        };
        let Verdict::Failed(failed) = &found.verdict else {
            return Err(format!(
                "the guided phase from seed {seed} did not find the needle in {BUDGET} cases: {:?}",
                found.verdict
            ));
        };
        if failed.failure.text != FOUND || failed.case > BUDGET || failed.seed.is_some() {
            return Err(format!("the guided phase's failure is not the needle's: {failed:?}"));
        }
        let least: Vec<u64> = failed.least.choices.iter().map(|choice| choice.value).collect();
        if least != NEEDLE {
            return Err(format!(
                "the guided failure shrank to {least:?}, not the needle's bytes {NEEDLE:?}"
            ));
        }

        // The same seed and the same coverage answers: the same tapes.
        let again = {
            let mut guided =
                Guided::new(config.clone(), &mut memory).expect("the memory is the config's");
            drive(&mut guided, needle)
        };
        if again.digest != found.digest || again.verdict != found.verdict {
            return Err(format!(
                "two guided runs from seed {seed} differ: digests {:#x} and {:#x}, verdicts {:?} and {:?}",
                found.digest, again.digest, found.verdict, again.verdict
            ));
        }

        // The control: the planted phase keeps every tape, new coverage or not.
        let blind = {
            let mut guided = Guided::planted(config, &mut memory, Plant::IgnoresCoverage)
                .expect("the memory is the config's");
            drive(&mut guided, needle)
        };
        let Verdict::Passed(blind_passed) = &blind.verdict else {
            return Err(format!(
                "the coverage-blind phase from seed {seed} found the needle too, so finding it does not show the coverage signal guided anything: {:?}",
                blind.verdict
            ));
        };

        println!(
            "guided phase, seed {seed}: the needle at case {} of {BUDGET} (corpus {} tapes); plain search: none in {} cases; coverage-blind phase: none in {} cases",
            failed.case, found.kept, plain.cases, blind_passed.cases
        );
        Ok(())
    });
}

/// A body with 64 points, one per value of its one draw: a corpus that grows to many tapes.
fn spread(tape: &Tape, coverage: &mut [u64]) -> Result<(), String> {
    coverage[0] |= 1 << tape.below(64, "point");
    Ok(())
}

/// A corpus written, read back into a new phase and written again gives the same bytes; bytes
/// cut short, cut mid-word, of another form or with a word after the last tape are refused by
/// name; a buffer too short is refused; the loaded corpus is replayed first and the phase then
/// goes on to pass.
///
/// Prints nothing of its own; passes.
#[test]
fn a_corpus_round_trips_and_a_cut_or_other_form_is_refused() {
    let config = GuidedConfig::new(1).cases(500).seed(7).corpus_tapes(32).tape_choices(4);
    let mut memory = vec![0u64; config.memory_words().expect("the config fits")];
    let mut bytes = Vec::new();
    {
        let mut guided =
            Guided::new(config.clone(), &mut memory).expect("the memory is the config's");
        let run = drive(&mut guided, spread);
        assert!(matches!(run.verdict, Verdict::Passed(_)), "{:?}", run.verdict);
        assert!(guided.kept() > 1, "the corpus kept {} tapes of 64 points", guided.kept());
        bytes.resize(guided.corpus_bytes(), 0u8);
        assert_eq!(guided.write_corpus(&mut bytes), Ok(bytes.len()));
        let mut short = vec![0u8; bytes.len() - 1];
        assert_eq!(
            guided.write_corpus(&mut short),
            Err(CorpusError::Short { needs: bytes.len(), has: bytes.len() - 1 })
        );
    }
    assert_eq!(bytes[..8], CORPUS_FORM.to_le_bytes());

    // Read back and written again: the same bytes.
    {
        let guided = Guided::with_corpus(config.clone(), &mut memory, &bytes)
            .expect("the bytes a phase wrote read back");
        let mut again = vec![0u8; guided.corpus_bytes()];
        assert_eq!(guided.write_corpus(&mut again), Ok(bytes.len()));
        assert_eq!(again, bytes);
    }

    // The loaded corpus replays first and the phase goes on to pass.
    {
        let mut guided = Guided::with_corpus(config.clone(), &mut memory, &bytes)
            .expect("the bytes a phase wrote read back");
        let run = drive(&mut guided, spread);
        assert!(matches!(run.verdict, Verdict::Passed(_)), "{:?}", run.verdict);
    }

    // Refused by name.
    let refused = |bytes: &[u8], memory: &mut [u64]| match Guided::with_corpus(
        config.clone(),
        memory,
        bytes,
    ) {
        Ok(_) => panic!("{} bytes read as a corpus", bytes.len()),
        Err(error) => error,
    };
    let len = bytes.len();
    assert_eq!(
        refused(&bytes[..len - 8], &mut memory),
        GuidedError::Corpus(CorpusError::Cut { at: len - 8, len: len - 8 })
    );
    assert_eq!(
        refused(&bytes[..len - 3], &mut memory),
        GuidedError::Corpus(CorpusError::NotWords { len: len - 3 })
    );
    let mut other = bytes.clone();
    other[7] = 2;
    let found = u64::from_le_bytes(other[..8].try_into().expect("a word"));
    assert_eq!(refused(&other, &mut memory), GuidedError::Corpus(CorpusError::Form { found }));
    let mut trailing = bytes.clone();
    trailing.extend_from_slice(&[0u8; 8]);
    assert_eq!(
        refused(&trailing, &mut memory),
        GuidedError::Corpus(CorpusError::Trailing { bytes: 8 })
    );
    let mut too_many = bytes.clone();
    too_many[8..16].copy_from_slice(&33u64.to_le_bytes());
    assert_eq!(
        refused(&too_many, &mut memory),
        GuidedError::Corpus(CorpusError::TooManyTapes { count: 33, cap: 32 })
    );

    // A coverage of the wrong width is refused, naming both.
    let mut guided = Guided::new(config, &mut memory).expect("the memory is the config's");
    let Next::Run(_) = guided.next() else { panic!("a fresh phase proposes a case") };
    assert_eq!(
        guided.ran(&Record::new(), Ended::Passed, &[0, 0]),
        Err(GuidedError::CoverageWords { expected: 1, got: 2 })
    );
}
