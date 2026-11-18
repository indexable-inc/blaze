---
status: plan
summary: "The Tests stage of the engine (plan row E2): one task a test item, its result a record keyed by the test and valid by its REACH, the code records its process reached on a first call (each instance by name and code key, each static's datum) and the files and variables its sandbox let it read. An edit reruns exactly the tests whose reach holds a moved cell; nothing changed runs none. A test granted the clock, the network or unseeded randomness is never reused, by name; `--audit` reruns reusable tests and marks a difference non-deterministic. Core: crates/blaze-test-reach."
date: 2026-11-17
---
# The Tests stage

The owner: "also need incremental test (only rerun ones that changed)". Plan row E2: one body edit
reruns only the tests that reach it. This is that stage on the engine (engine.md), after `Bodies`;
runtime.md points 3 (read lists checked in order), 8 (a program is a manifest over code records)
and 9 (published records fetched, yours stay).

## The task

`Stage::Tests`, after `Bodies`, one wave. One `TestRun` task a test item of the asked test units
(the tests record, crates/blaze-scope/src/tests_record.rs), named `Name { kind: Kind::Test,
subject: the test item's key }` (blaze-drive key.rs: `Kind::Test` exists, "a test's result, whose
read list is the code identities its run entered"). It does not read Lints: a finding never
changes what a test does. A test whose body check is not `Clean` is not run (run_parent.rs
`ParentError::NotChecked`): its task fails by name and reads that body.

| target | unit of reuse |
|---|---|
| `#[test]` fn | one task, its own process (a fork of the suite's zygote, or a lazy run) |
| `#[should_panic]` | as `#[test]`; the expected text is in the test item's key, so the judged out is |
| `#[ignore]` | no task unless `--ignored`/`--include-ignored`; then as `#[test]` |
| `harness = false` target | ONE task: its whole process, its `main` the entry |
| doctests | one task each, later (each is its own unit; not in this piece) |

## Its key and its reads

Key: the test item's key (its text, attributes and `should_panic` text), the suite's unit key
(cfg, features, target, toolchain) and the runner's version. Nothing of the machine (rule 25).

Reads, its REACH, the exact dynamic set (no static call graph, no over-approximation through trait
objects): every cell the process was given or read, each once, as `(kind, name, value)`:

- `Code`: an instance its process asked for on a FIRST CALL. Name: the instance's symbol (item and
  type arguments, stable across edits). Value: its `CodeKey` (blaze-ladder code_records.rs), keyed
  from text and today's facts BEFORE any record is made, so the check never compiles (rule 77).
  A code record's key covers its own body and what it folded (a const, an inlined callee, a
  layout: rule 65); a callee called through a slot is not in its caller's key, so the callee is a
  row of its own. A tier-up record of the same instance is not a new row: tiers are equal code.
- `Datum`: a static's data record the process was sent. Name: the static's symbol; value: the
  datum's record id.
- `File`: a file the sandbox let it read. Name: H(path from the workspace root); value: H(bytes).
- `Env`: a variable it read. Name: H(variable name); value: H(value), all zero when unset.

In a zygote's fork the code placed before the fork is shared, so "first call" is counted IN THE
CHILD: each slot's first entry marks the child's entered region (test.md, the child's report), and
the parent maps each entered slot to the instance it holds. A lazily published record is the same
set from the other side (run_parent.rs `Needs::publish`). One name with two values in one run (a
file read twice that moved) is a typed refusal (`ReachError::TwoValues`, rule 21): the result is
filed not reusable, `CellMovedDuringRun`.

## Its record

All little-endian (blaze-test-reach record.rs), equal values encode to equal bytes:

| at | bytes | what |
|---|---|---|
| 0 | 4 | `BZTK` |
| 4 | 4 | version, 1 |
| 8 | 1 | out: 0 passed, 1 failed, 2 ignored |
| 9 | 1 | reusable: 0 yes, else the `NotReusable` number |
| 10 | 6 | zero |
| 16 | 32 | the test item's key |
| 48 | 32 | the reach digest: H(domain, every row's bytes in order) |
| 80 | 32 | a failure's captured output record key; zero otherwise |
| 112 | 4 | row count, at most `MAX_READS` (2^20) |
| 116 | 4 | zero |
| 120 | 72 a row | kind (u8), 7 zero bytes, name (32), value (32) |

Rows are sorted by `(kind, name, value)` and strictly ascending, so two arrival orders of one set
write equal bytes (rule 25). `open` checks, before trusting a row: the length against the header,
magic, version, both tags, every zero field, the count against the cap, the length exactly
`120 + 72 * count`, each row's kind and padding, the order, one value a name, and the digest. Each
is a typed `RecordError` naming the field (rule 19). Opened once, the view is the proof (rule 74).

## Reuse

`enter(record, moved)`: a non-reusable record reruns, saying why; else its rows are checked in
order and the first that moved reruns it, `Rerun { because: Moved(read) }`; none moved: `Reused`.
`moved` for `Code` is "today's key of this symbol is not this value" (the run walk keys an instance
from text: body_ids.rs `Walk::one`); for `Datum` the static's datum key today; for `File` and
`Env` the digest now. A record that is missing or does not open is a rerun with its reason
(`Because::NoRecord`, `Because::Unreadable(error)`), printed as a note: never a silent pass and
never a silent rerun (rule 3).

A failure stands like a pass: its record is reused while its reach stands and is printed again from
its output record (engine.md: an error stops only what reads it). What is printed:
`tests: N run, R reused (unchanged reach), F failed`; a reused test shows `ok (reused)` or
`FAILED (reused)`; `--why <test>` prints its `Because` in `blaze why`'s words ("`f::<u8>`'s code
moved", "file `x.json` changed", "never reused: it was granted the clock").

Finding the candidates forward (runtime.md point 3): the postings record (blaze-run postings.rs)
maps each cell to the tests whose reach holds it; an edit's moved cells give the candidates, and
only those are entered. Nothing changed: no candidate, 0 tests run, the certificate covers them.

## Determinism, and what is never reused

Reuse is sound for a test that reaches different code on different runs (a map's order, a branch
on data) GIVEN determinism: with every reached cell unchanged the run takes the same path, and what
it did not reach cannot change its result. The sandbox makes it so (rule 75): no ambient clock,
randomness seeded from the test's key, a fixed hash seed. A test GRANTED the clock, the network or
the OS's randomness is `NotReusable` (`Clock`, `Network`, `Randomness`) and reruns every time, by
name. `blaze test --audit` reruns a sample of reusable tests and compares out and reach (`audit`);
a difference files the test `NonDeterministic`, so it always reruns, and is reported by name.

## Floors

- One body edit: tests entered == tests whose reach holds that body's code record (the model test
  in blaze-test-reach checks equality, not inclusion).
- Nothing changed: 0 tests run, one certificate read.
- Per reused test: one record read and its rows' checks in order; per run test, one row a first
  call, kept sorted by amortised merging (no hashing per call).

## Hooks (wiring is the next piece)

run_parent.rs `Needs::publish` (lazy runs: a `Code` row per record, a `Datum` row per datum sent);
test_run.rs `Answers::answer` (zygote runs: `entered` and `reads` to rows) and `Runner::run`
(where a test's process starts: reused tests are never asked); engine_check.rs and engine_tasks.rs
(a `Stage::Tests` arm at each match on `Stage`); blaze-engine engine.rs `Stage`, `STAGES`.

## Relation to what is in the tree

blaze-run's plan.rs/result.rs/postings.rs (test.md) key a result by an address H(test, asks,
answers); blaze-testsel (test-selection.md) does the same on the old path from slot bytes. This
stage keeps their postings and judging and replaces the address with the read list, the engine's
one validity rule: one concept, one home (rule 30), once wired.

## Not in this piece

The wiring (above), the postings update, doctests, benches, the audit's sampling rate, and printing.
