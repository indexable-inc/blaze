---
status: plan
summary: The attribute `#[bstd::test]` runs a test shaped like `main` (it takes a `Cx` and an `Io`) on the deterministic simulation under 256 seeds by default, with IO failures, delays and power cuts on unless the test leaves one out by name; a failure prints its seed and a one-line replay command, and the seed replays to the same trace byte for byte. The runner is `blaze_core::sim::seeds`, on the one simulation; bstd adds the attribute and the unwind of a power cut.
date: 2026-11-12
---
# Simulation is the default test of a bstd program

Approved by Andrew, 2026-11-12. TigerBeetle's VOPR, as the default for every bstd user.

## What

```rust
#[bstd::test]                                   // 256 seeds, every fault kind on
fn the_ledger_keeps_what_it_committed(cx: &Cx, io: Io) -> Result<(), Error> { .. }

#[bstd::test(without(Crashes))]                 // an opt-out is a named argument
fn a_report_is_rendered(cx: &Cx, io: Io) { .. }
```

A test has `main`'s shape (std.md, "Decided"): the program is the test. Each seed makes one
simulated world (`Io::sim(seed)`) and runs the program in it to its end, again after each power
cut, until the plan's faults are spent; then the faults are healed and the program has to end
well. What a failure prints:

```
bstd::test `ledger::tests::keeps` failed at seed 41 (seed 42 of 256), start 3: panicked: record 2 is committed and torn
plan: Plan { seed: 41, fail_one_in: 16, max_latency: 2ms, crash_one_in: 32, cuts_most: 2 }
effects 57, failures injected 3, power cuts 2, restarts 2
the seed replays: its second run gave the same 4,812 bytes of trace
last of the trace: ..
replay: BLAZE_SIM_SEED=41 blaze test -p ledger ledger::tests::keeps -- --exact
```

## Why

`Io::sim(seed)` already makes any bstd program a simulation test (std.md, "Io"), but only for
a test that builds the world, loops the seeds, turns the faults on, crashes, restarts and prints
the seed by hand, as each of crates/blaze-std/src/tests does. The default was a plain `#[test]`
on a kind world. Rule simulate-first says the default is the simulation.

## Design

**One runner, on the one simulation: `blaze_core::sim::seeds`.** Not a second simulation and not
bstd's alone: a sans-IO core driven on `Sim` directly runs under it too (its own tests do).

- `Seeds`: which seeds (`first`, `count`, 256 from 0 by default), how many restarts before the
  faults heal, and which `Fault` kinds are left out (`Seeds::DEFAULT.without(Fault::Crashes)`).
  The default seeds are fixed, so a green run is green again (deterministic-output); more are
  asked for from outside (`BLAZE_SIM_SEEDS=10000`), as `HEGEL_TEST_CASES` is.
- `Plan::of(seed, &seeds)`: what one seed injects, drawn from the seed as a swarm (each kind off
  on some seeds, so a rare one is not drowned): `fail_one_in` (an effect fails: a short append,
  a sync that makes nothing durable), `max_latency` (late, shuffled completions) and
  `crash_one_in` with `cuts_most`. Leaving a kind out zeroes it and changes no other draw.
- `Sim::arm(&plan)`: the armed simulation counts effects, draws the power cuts, and writes the
  trace: one line an effect, a completion, a cut, a start. `Sim::cut_due()` tells the driver's
  user that power is lost now; it crashes its world and the machine is off (it takes no effect
  and tells no completion) until `power_on`. What runs between (destructors) reaches nothing.
- `seeds::one(seed, ..)`: one seed. The program is started; a power cut or an error it gives up
  with restarts it (bounded: `restarts_most`); a panic fails the seed. When it ends well with
  cuts on, power is cut once more at its end, the faults heal, and it is run a last time: its
  success has to survive a crash. With the faults healed it has to end well: an error then is
  the bug an earlier fault left behind.
- `seeds::run(seeds, given, name, boot)`: every seed; the first that breaks is run a second time
  and the two traces compared (a deliberate second run that is the check: a divergence is said
  by name, with the first line that differs), then reported with the replay line. A run whose
  programs asked the simulation for nothing in any seed fails by name: it is no simulation test.
- `Given`: the seed and count the person gave (`BLAZE_SIM_SEED`, `BLAZE_SIM_SEEDS`), parsed
  typed, a malformed one an error. A given seed runs alone and is the same run.

**bstd adds three things** (`crates/blaze-std/src/simtest.rs`):

- `bstd::test::run(name, seeds, program)`: `seeds::World` for an `Io` and a program
  `Fn(&Cx, Io) -> impl Outcome` (`()`, or a `Result` whose error says itself).
- The unwind of a power cut: `Io::submit` asks `Sim::cut_due`, crashes the world (`Io::crash`,
  the disk ledger with it) and unwinds the program as the simulated exit does (an `Exit`
  payload, which scopes pass on), so nothing of the program runs after the cut.
- `#[bstd::test]`, a `macro_rules!` attribute rule (nightly `macro_attr`): no proc macro and no
  compiler feature of bstd's own. It expands to a `#[test]` that calls `bstd::test::run`.
  blaze's `mbe` dropped an attr rule's arguments; it matches them now (`mbe::MacroCall`),
  which `without(..)` needs.

**Lost alternative.** A Hegel test that draws the seed and the faults: it shrinks, but Hegel is
a dev-dependency with a native engine, bstd cannot depend on it, and one integer that replays
is what a user can paste. Blaze's own DSTs keep Hegel for operation sequences and may call
`seeds::one` with a drawn seed. A proc-macro attribute: refused by library-over-language.

## Cost

A seed costs the program's own run times its starts (at most `restarts_most + 2`), and the
trace: a line an effect, cut at 240 bytes with the rest hashed. Nothing when not armed: one
`Option` test in `Sim::submit`, `Sim::poll` and `Sim::crash`.

## Proof

`crates/blaze-core/src/sim/seeds/tests.rs`, which cargo runs (`cargo test -p blaze-core`, 225
passed on 2026-11-12), a ledger written through `Sim`: (a) each planted bug is caught within the
default 256 seeds (a commit before its data's sync at seed 2; a failed sync ignored at seed 16);
(b) the printed seed, given back, is the same failure and the same trace byte for byte; (c) the
correct ledger passes all 256, with failures injected and power cut; (d) one seed twice gives
one trace. With crashes left out by name both plants pass: the crash is the cause.

`crates/blaze-std/src/tests/simtest.rs` states the same through `#[bstd::test]` on bstd's own
files (a counter in two files; a value published unsynced, a failed publish ignored). cargo
cannot build bstd, and this lane ran no blaze, so it was run only in a scratch copy of the crate
with the blaze-only syntax cut (the four contracts, `where const`, the reflection intrinsics'
bodies, `Shares::join`): there its 9 tests pass, and the crate's other tests that rustc
compiles pass or fail as they do on the commit before (132 passed, 3 failed, both times).
**Still to do: `blaze test -p blaze-std simtest` on the build VM.**

## Not built

- `blaze test --seed N`: the replay line is an environment variable until the test harness
  blaze writes hands a typed setting to a test.
- A full disk (`Sim::fill` needs a directory) and scripted peers as fault kinds.
- A notebook that survives the crash for the checker's memory: today an oracle has to be read
  from the disk (a commit record), or the test crashes its world itself (`io.crash()`).
- In the blaze edition (S5) `#[test]` names this attribute.
- A world of the test's own making under the attribute (scripted devices, an environment, a
  withheld capability): such a test still makes its `Io` and calls `bstd::test::seeded`-like
  code by hand.
