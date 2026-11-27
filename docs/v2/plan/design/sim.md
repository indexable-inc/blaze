---
status: plan
summary: blaze owns its test engine. A test case is a function of one tape of labelled, bounded choices in which 0 is always the calm choice; `crates/blaze-sim` draws the tape from a seed, shrinks a failing tape to the fewest and smallest choices that still fail, prints it and replays it exactly, and holds the pieces every simulation re-implemented by hand (in-flight answers delivered by choice, planted-bug controls, reach points). Built and in every crate's tests; `blaze_core::sim::Sim`'s own generator and `blaze-check`'s random schedules are still to draw from the same tape. A model is held to the real service by one conformance suite.
date: 2026-11-13
---
# One tape: blaze's own test engine and simulation kit

Decided by Andrew, 2026-11-13 (decision D37): the simulated machine stays in blaze, and blaze
stops depending on Hegel. This page is the design; `crates/blaze-sim` is its interface crate
(rule interfaces-first), and row V12 of the ledger tracks it.

## The design

**A case is a function of its tape.** A tape is a sequence of choices; a choice is a value in
`0..=most` with a label (`"deliver"`, `"kill"`, `"torn sectors"`). Value 0 is the calm choice
everywhere: the oldest answer is delivered, no fault fires, no power is cut, a shuffle leaves the
order alone, a list ends. So the all-zero tape is the quiet sequential run, a tape is simpler
when it is shorter and then when its values are smaller, and a shrunk failure reads as the few
choices that were not calm.

One concrete type, `blaze_sim::Tape`, hands the choices out: a prefix of given values, then a
tail that is calm (zeros) or seeded. That one shape is every mode: a seeded case (empty prefix,
seeded tail), a replay (the recorded values, calm tail), a shrink candidate (edited values, calm
tail) and a fork (a prefix, then a new seed). It is a handle (`Clone`), passed in and named where
it is used (rule nothing-ambient); two draws at once on one tape is a refusal, since a
simulation that draws from two threads at a time is not a function of its tape.

**A seeded tape is `SplitMix64`.** `Tape`'s methods are `blaze_rng::SplitMix64`'s (`below`,
`index`, `range`, `one_in`, `coin`, `pick`, `fill`, `shuffle`, ..) plus a label, and a seeded
tape returns exactly what `SplitMix64::new(seed)` returns for the same calls. Each method records
its choice in the canonical form (`one_in` records whether it fired; `shuffle` records how far
each element moved), so the stream is the old one and 0 is still calm. The simulations pinned to
literal seeds (over 400 `Sim::new(<n>, ..)`) keep their runs when they move to the tape.

**The engine is a sans-IO state machine** (`search.rs`, `shrink.rs`): `next()` proposes a case
(prefix and tail), `ran()` takes what it drew and how it ended. It runs a given tape or seed
alone; else the cases of seeds `first..first+cases`, fixed by default so a green run is green
again, more asked for from outside (`BLAZE_SIM_SEEDS`). On the first failure it shrinks: cut the
tape, delete spans, delete blocks, zero blocks, lower each value, sort neighbours; a candidate
is kept only when it fails with the same origin and its recorded tape is simpler. Then it runs
the least tape once more: a case that does not fail again the same way is reported as not a
function of its tape, by name. Every phase is bounded (cases, shrink runs, choices per case).

**Verdicts a green run cannot fake.** A check whose cases drew nothing fails (it is no property
test); one whose cases were all discarded fails; one that names reach points
(`tape.reached("a crash between commit and consume")`) fails when a point was never reached in
the whole run. Reach points are the complement of planted bugs: a control proves the oracle sees
a bug, a reach point proves the run got to where the bug lives.

**The driver** (`check.rs`) is the one place that reads the environment (`BLAZE_SIM_SEED`,
`BLAZE_SIM_SEEDS`, `BLAZE_SIM_TAPE`, and `BLAZE_SIM_BOARD` for the runner's board descriptor in
`board.rs`), catches a panic and prints. No attribute macro: a test is
`#[test] fn f() { blaze_sim::check(Config::cases(200), |tape| ..) }`, and a generator is a
function of the tape (`fn op(tape: &Tape) -> Op`), not a combinator object. A failure prints the
seed, the least tape with each choice that is not calm by label, the notes, and one line that
replays it.

**The kit** holds what the simulations copied: `Flight<T>` (answers in flight, delivered by
choice, oldest first when calm, gone at a kill), `control` (the code passes as it is and a
planted bug is caught, saying what), `Broke` (an oracle's finding, or the harness's own fault).

## Prior art

Hypothesis: a test is a function of a choice sequence, and shrinking that sequence shrinks any
generated value (its `conjecture` engine, which Hegel wraps). The Hegel fork's `hegel::sim`
(indexable-inc/hegel, `docs/dst.md`): every simulation choice is a draw and 0 is calm; its
kernel model with page-cache, device and media layers is the reference for the disk below.
TigerBeetle's VOPR and FoundationDB: one seed a run, fault classes on per run, a calm epoch at
the end. Antithesis: `sometimes` assertions, which reach points are. shuttle and PCT
(Burckhardt et al., ASPLOS'10) for schedules; loom for exhaustive ones, which `blaze-check` is.

**Lost alternative.** Pin the fork and use `hegel::sim`: it has the tape idea, a validated-on-
paper kernel model and a wasm world that snapshots, but it rests on a std facade switched by a
feature (rule strategy-is-a-type-parameter refuses that; blaze's cores are sans-IO and need no
facade), on a native engine bstd cannot depend on, and on code blaze does not own.

## Cost against the floor

- A draw: the floor is the generator (about 2 ns). Here: an uncontended try-lock, the bound
  check and a 32-byte record, tens of ns. A case of 10,000 choices pays about 0.2 ms; under a
  few milliseconds a percentage means nothing (rule 67), and the record is what shrinking,
  replay and the report are made of.
- Memory: 32 bytes a choice, in buffers kept across cases; a case is capped (2^20 choices).
- Shrinking has no floor; it is bounded (1,000 runs by default) and reports runs used and the
  tape's length before and after.
- Replay: one run. A fork: the prefix run again; a snapshot instead is below.

## Order

Built: `crates/blaze-sim` (tape, engine, driver, kit, its own proof); every crate's tests call
`blaze_sim::check`; the fresh core's simulations draw their schedules from the tape.

1. **Next**: `Sim`'s own generator becomes the tape (its `Rng` is the tape, `Sim::new` takes
   one, `seeds` becomes a client of the engine), so its latencies, delivery order, tears and
   failures are shrunk with the plan.
2. **After**: `blaze-check`'s random and PCT schedules draw from the tape (one case then
   interleaves threads and crashes a disk); `Sim`'s disk takes the three-layer crash model
   (writes durable only through a sync, directory entries only through their directory's);
   several processes and two versions on one disk; plants passed in as a type parameter, so no
   planted bug is compiled into a shipped binary; a snapshot of a world's frame for forks whose
   prefix is expensive.

## Proof

`crates/blaze-sim/src/tests/`, with a control for each. The refinement: a seeded tape gives
what `SplitMix64` gives, on any sequence of calls. The invariants: a recorded tape replays to
the same values and the same record; the all-zero tape is calm in every method; a search over a
body with a known least failing tape ends on exactly that tape; one configuration gives one
verdict, twice, byte for byte; a body that reads something outside its tape is reported as
that; a check that drew nothing, discarded everything or missed a reach point fails by name;
`Flight` delivers oldest first when calm and reaches every order when seeded.

## A model is held against what it models

D167 part c, 2026-11-20. A model may be stricter than the real service, never more permissive,
and every bug found on the real one is added as a suite case, which adds it to both by
construction.

- **One conformance suite per outside service.** Its cases are written once, as functions over
  the service's trait, and run unchanged on the model in ordinary tests (`unit`) and on the real
  service in a scratch directory under the test's own temporary folder (`process`; no network,
  nothing outside that folder). For the file system the trait is `blaze_core::Driver`'s file
  effects plus a setup from outside, `crates/blaze-core/src/conform/fs.rs`: `Sim` against this
  host's native driver. For the journal it is a stopped and resumed journal, the `JournalCase`
  table in `crates/blaze-core/src/fsevents.rs`'s tests: fseventsd itself against the model of
  its store, which also runs cases the tape draws.
- **One outcome per case.** A case answers one typed value: each effect's answer as a kind
  (done, refused, a file of this length, a directory, not there) and the state after it looked
  at through the same trait, never an error's text. Where systems legitimately differ (a disk
  that folds case and one that keeps it), the expectation is one value per platform, the
  model's among them; a case never skips its assert.
- **Stricter, never more permissive.** Where the real disk refuses what the model did, the
  model is wrong and is fixed; where the model refuses what some real disk does (a second
  spelling of a name a disk would fold to the first), the model's expectation is its own named
  value. The first run of the file system's cases found the model more permissive in six places
  (a folder vanished with its last file; a folder a write made outlived every crash; a file was
  made under a file and where a folder is; a stat under a file answered "not there"; a stat
  followed no link; two spellings of one folded name were two), and the native driver outside
  its contract in two (a rename of nothing made the target's folders; a durable write's folders
  were made unsynced). The list, each with its fix, heads `conform/fs.rs`.
- **What the real side cannot show.** A promise about a power cut (a sync's) is tested on the
  model only, with its control: a test cannot cut its own machine's power, and the page cache it
  would read back holds every name, synced or not. The suite says so beside the case
  (`a_durable_write_into_new_folders_survives_a_crash`), and names what a trace of system calls
  could check instead.
