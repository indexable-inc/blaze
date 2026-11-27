---
status: plan
summary: "`#[bstd::test]` runs a test on the deterministic simulation under 256 seeds by default, with IO failures, delays and power cuts on unless the test leaves one out by name, and a failure prints a seed that replays byte for byte. A test names its grants by type, as `main` does (D109). Built: the runner (`blaze_core::sim::seeds`) and the attribute (crates/blaze-std/src/simtest.rs). This spec keeps what is not built."
date: 2026-11-12
---
# Simulation is the default test of a bstd program

Approved by Andrew, 2026-11-12. TigerBeetle's VOPR, as the default for every bstd user.

## What

```rust
#[bstd::test(data = files("ledger/head" => b"0\n"))]
fn the_ledger_keeps_what_it_committed(cx: &Cx, data: Dir<ReadWrite>, clock: Clock) -> Result<(), FsError> { .. }

#[bstd::test(without(Crashes))]                 // an opt-out is a named argument
fn a_report_is_rendered(cx: &Cx, out: Out, data: Dir<ReadWrite>) { .. }
```

A test takes a `Cx` and names each grant it uses by its type, as `main` does (D109); a whole
`io: Io` is refused at the attribute. Each seed makes one
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

## Where it is built

- The runner, on the one simulation: `crates/blaze-core/src/sim/seeds.rs` (`Seeds`, `Plan::of`,
  `Sim::arm`, `Sim::cut_due`, `seeds::run`, `Given`), proven by `seeds/tests.rs` with two planted
  bugs.
- The attribute and the unwind of a power cut: `crates/blaze-std/src/simtest.rs` (a
  `macro_rules!` attribute rule, no proc macro), the grants by type in `simtest/granted.rs`, drawn
  inputs in `simtest/drawn.rs`; tests in `crates/blaze-std/src/tests/simtest.rs`.
  **Still to do: `blaze test -p blaze-std simtest` on the Mac.**

`seeds` is to become a client of blaze-sim's tape engine ([sim.md](sim.md), "Order").

## Not built

- `blaze test --seed N`: the replay line is an environment variable until the test harness
  blaze writes hands a typed setting to a test.
- A full disk (`Sim::fill` needs a directory) and scripted peers as fault kinds.
- A notebook that survives the crash for the checker's memory: today an oracle has to be read
  from the disk (a commit record), or the test crashes its world itself (`io.crash()`).
- In the blaze edition (S5) `#[test]` names this attribute.
- A world of the test's own making under the attribute (scripted devices, a withheld
  capability): `bstd::test::granted` makes every world with `Io::sim(seed)`, so such a test
  still makes its `Io` and loops its seeds by hand.
