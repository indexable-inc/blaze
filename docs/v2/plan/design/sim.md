---
status: plan
summary: blaze owns its test engine. A test case is a function of one tape of labelled, bounded choices in which 0 is always the calm choice; `crates/blaze-sim` draws the tape from a seed, shrinks a failing tape to the fewest and smallest choices that still fail, prints it and replays it exactly, and holds the pieces every simulation re-implemented by hand (in-flight answers delivered by choice, planted-bug controls, reach points). Hegel and `vendor/hegeltest` go; `blaze_core::sim::Sim`, the fresh core's simulation and `blaze-check`'s random schedules come to draw from the same tape.
date: 2026-11-13
---
# One tape: blaze's own test engine and simulation kit

Decided by Andrew, 2026-11-13 (decision D37): the simulated machine stays in blaze, and blaze
stops depending on Hegel. This page is the design; `crates/blaze-sim` is its interface crate
(rule interfaces-first), and row V12 of the ledger tracks it.

## What was wrong

Measured on main at e6635c9ba7:

- **A schedule is one opaque number.** Every simulation draws a `u64` from Hegel and seeds a
  `SplitMix64` with it (`crates/blaze-keyed/src/tests.rs`, every `Sim::new(tc.draw(..), ..)`).
  Hegel shrinks the operations and never the delivery order, the kill point or which write tore,
  so a failure prints a minimal plan under an arbitrary schedule.
- **Three machines that do not meet.** `blaze_core::sim::Sim` has a disk, crashes and faults on
  one thread; `crates/blaze-check` interleaves atomics and locks with no disk; the fresh core's
  crates each carry a hand-rolled loop. Eleven more `struct Sim` exist beside those.
- **The scaffolding is copied.** 18 `trait Draw`, 21 `fn control(`, 92 `enum Plant`, 53
  `fn drain(`, 5 identical `Broke`, 527 `the_oracle_catches_*` each with its own seed loop.
- **Two runners.** `blaze_core::sim::seeds` sweeps fixed seeds and cannot shrink; a Hegel test
  shrinks and draws fresh seeds each run, so a green run is not green again (annoyance A11).
- **A vendored engine nobody controls.** `vendor/hegeltest` is a native engine pinned 13 commits
  behind its fork; bstd cannot depend on it (bstd-sim-test.md, "Lost alternative").

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
`BLAZE_SIM_SEEDS`, `BLAZE_SIM_TAPE`), catches a panic and prints. No attribute macro: a test is
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

1. **The interface crate** (done, on main in #2124): `crates/blaze-sim` with tape, engine,
   driver, kit and its own proof.
2. **Ports** (done): every crate's tests call `blaze_sim::check` and `blaze_sim::control`. The
   fresh core's crates (blaze-keyed, -findings, -expansion, -program, -mutants, -remote, -memo)
   draw their simulations' schedules from the tape; a test on `Sim` draws its seed from the tape
   as `"sim seed"`. `vendor/hegeltest` is deleted.
3. **Next**: `Sim`'s own generator becomes the tape (its `Rng` is the tape, `Sim::new` takes
   one, `seeds` becomes a client of the engine), so its latencies, delivery order, tears and
   failures are shrunk with the plan.
4. **After**: `blaze-check`'s random and PCT schedules draw from the tape (one case then
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

## What the first night found

On fixed seeds, the store's ported tests found two bugs that were on main: a batch read handed
one file's bytes to its neighbour, and a derive chain two deep left its lower base in `placed/`.
Both were fixed, each with its case pinned, in the same change.
