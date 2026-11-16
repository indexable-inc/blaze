---
status: plan
summary: What a test is on the rebuilt path. A test unit is a unit like any other with `test` in its cfg and its own key; its tests are one record per unit built from attributes alone (crates/blaze-scope/src/tests_record.rs); the runner is blaze's own, one forked process per test over the suite's shared code area; a result is a record read-addressed by the functions the test entered and the outside reads it made, so an edit reruns only the tests that entered what changed, found by a postings record (End state, with the gap and the pieces). Built 2026-11-16: test units become crates of the names set, and the tests record with its reader; written, not compiled: the plan machine and the postings record (crates/blaze-run plan.rs, postings.rs). Not built: the runner, the head record, integration-test units, benches, doc tests.
date: 2026-11-16
---
# Tests

The milestone is "a real project's leaf crate passes its own tests, built by blaze", and the
libraries' own suites are the oracle for everything below. rustc's `--test` output and libtest's
tables are references, not the spec (rule 5): nothing here calls `test::test_main_static`.

## Sources, quoted

- Reference, Testing attributes: "The `test` attribute marks a function to be executed as a test.
  These functions are only compiled when in test mode." "The `ignore` attribute tells the test
  harness to not execute that function as a test." "The `should_panic` attribute makes the test
  only pass if it actually panics", with "an input string that must appear within the panic message".
- Reference, Conditional compilation: "`test`: Enabled when compiling the test harness."
- Cargo, Specifying dependencies: dev-dependencies "are not used when compiling a package for
  building, but are used for compiling tests, examples, and benchmarks."

## A test unit

A package's library compiled for tests is a second UNIT (`UnitKind::Test { bin: None }`; a
binary's is `Test { bin: Some(name) }`): the same root file, `test` in its cfg set, the
dev-dependencies among its dependencies. The unit graph makes it only when tests are asked
(`Ask::tests`), and its cfg, kind and binary name are in its key, so no record of it mixes with
the plain library's. What is shared between the two today, and what is not:

- shared: every record keyed by text alone (a file's outline) and every dependency unit whose
  own key is equal (the toolchain's crates, each dependency built once for both);
- NOT shared: anything keyed by an `ItemKey`. An item's key starts with its crate key's prefix
  (`make_key`), and the two units have two crate keys, so an item with no `cfg(test)` in or under
  it has the same text and two keys, and is solved, checked and built twice. The fix belongs to
  the key, not to a special case: a body's records keyed by code identity (code-identity.md), under
  which an instance that reaches nothing the `test` cfg changes is one record for both units.
  Until then a test unit costs a second front-end pass over its crate; say so in any measurement.

## The tests record

One record per test unit, written when its names are solved, from ATTRIBUTES ALONE: no body is
lowered to list tests. One fixed 64-byte row per `#[test]` (or `#[bench]`) function: its item key,
its path as printed (`module::name`, the crate's name left off), ignore and its reason,
should_panic and its expected text; rows sorted by (path, key), strings in the items' key order,
so any worker writes equal bytes. A true `cfg_attr` stands as its attributes
(`#[cfg_attr(miri, ignore)]`); an unread predicate, a `should_panic` form not known, `#[test]` on a
member of an impl: each a typed refusal naming the function. A function an attribute macro
returns (`#[tokio::test]`) is listed under its own key in the call's expansion, where the macro
wrote `#[::core::prelude::v1::test]`. A unit without `test` in its cfg has no tests record
(`NotATestUnit`). The reader checks bounds and finds a test by path with a binary search in place.

## The runner

Each test is a function instance the builder builds like any other; the program is a manifest
over the same code records plus one generated entry, made from the tests record: it reads a row
number from its first argument and calls that row's function. ONE PROCESS PER TEST, started by
`blaze test` (the command, not generated code), each with a deadline:

- the builder has no unwinding, so a panic ends its process; with one process per test that is
  exactly the verdict (returned: status 0; panicked: the abort, its message on stderr), and
  `should_panic(expected = "..")` is judged by the parent reading that message. One process that
  stops at the first panic would report one failure per run and could not judge `should_panic`.
- a hang, a stack overflow or a wild write fails one test with its name (rules 18, 20), tests run
  on every core without sharing state, and a test's result is a record of one process's run,
  which is the shape test-selection.md keys.
- cost: one spawn per test (the floor is the kernel's process start, about a millisecond; tempo's
  4,005 test bodies are seconds across cores), paid only by tests that rerun.

Output: one line per test in path order (`test tests::panics ... ok`, `FAILED`, `ignored, slow`),
the failures' messages, one summary line (`3 tests: 2 passed, 0 failed, 1 ignored`); status 0 when
none failed, else 1. First cut: tests returning `()`; one returning a `Termination` type is refused
by name until the entry calls `report`.

## Incremental: tests by reach

Replaced by "End state" below: a test's asks are the functions it ENTERED (exact, from the run's
mark stubs); the reach walk's set is the asks only for a test that started a second thread.

## The other targets

- Integration tests (`tests/*.rs`): each file its own unit, kind test, depending on the library's
  PLAIN unit and the dev-dependencies; same record, same runner. Not built: blaze-manifest's layout
  does not read `tests/`, `examples/` or `benches/`, and the unit graph has no kind for them.
- `#[bench]`: listed in the tests record with its own flag so nothing is dropped quietly; the
  runner prints it as not run. Measuring is its own design.
- Doc tests: later work, in doctests.md.

## End state (architect, 2026-11-16)

Stands: "A test unit", "The tests record", the verdicts of "The runner", run.md "What a program
is given" (fixed environment, run spec in the key), test-selection.md 1 to 4 (a cached pass
reads as a pass, the not-kept table, the outside form). Its section 5 is the daemon's, old.

**A test is an item**: a row of its unit's tests record, from attributes alone; no harness
binary per crate. Its id is `TestRun`'s base key (`TestInputs`: toolchain, unit settings, item,
attributes, run spec digest, arguments); nothing of its code is in it.

**A result is a record, read-addressed** (target.md, trace b). Asks: each function the run
ENTERED, by stable name (item and type arguments), answered by its code identity now (the
instance key: its body's result, the layouts and constants it read, the statics it names); each
outside read by name (a file from the root, a variable, a host fact), answered by its digest.
Filed at H(id, asks, answers). Entered is exact: during tests each slot word is a mark stub
(run.md, Tests). Why the last asks decide before the next run: a run is a function of the code
it enters and what it reads; if no answer moved it enters the same functions, and a function it
did not enter cannot change it. A test that starts a second thread may enter other functions
under another schedule, so its asks are its REACH (front2/reach.rs, a superset), basis `reached`.
Not kept, by name: before the run from the effect facts of its asks (real clock, a non-loopback
socket, inline assembly, an unbound foreign call); after it from the launcher, which binds the
OS imports (open, stat, readdir, getenv, clock_gettime, connect, getrandom, spawn) to recording
wrappers; the sandbox refuses a write outside the test's temporary folder and the test fails
naming the path. A `#[bstd::test]` gets the fake world (virtual clock, files in memory, a seeded
generator) and is always kept.

**After an edit of one body**: the request names the instances whose code moved (C) and the
head's read table, answered now, the reads that moved (R). The suite's postings record (sorted
(ask, test) rows) gives the candidates by one binary search per member: nothing scanned, no graph
held. A candidate's last asks are answered now: a pass at that address is a hit (an undo reruns
nothing), else it runs. Every other pass stands by the coarse proof. Failures and not-kept tests
always run. **No edit**: the certificate names each suite's head; the kept results are printed.
**First run**: the gate checks the scope (drive.md, `Beside`); a function is compiled at its
first call, so code no test enters is never built; CI's records answer what they can.

**One process per test.** Per suite a zygote (the launcher, the suite's code area mapped
shared, one thread) forks a child per test; the child applies the test's sandbox (macOS
`sandbox_init` while one thread; Linux namespaces, Landlock, seccomp as native_sandbox.rs), makes
its temporary folder, starts its control thread, calls the row's function. Not threads: a kept
result must be a function of its asks, and threads share statics, environment, current folder
and global pools (a pool one test started runs the next one's work: marks lose their owner); the
builder has no unwinding; a hang or a wild write must fail one test. Width: the performance
cores (`threads = n` takes n), longest last run first. Every test has a deadline (60 s, per test
in the manifest): stacks written, group killed, `TimedOut`. A panic, signal or sandbox refusal
fails that test, typed; the rest go on. The child reports on descriptor 3 (verdict, panic text,
marks), never via stderr; output is captured per test, printed for a failure as it ends, then
again in path order with the summary.

**Each kind.** `should_panic(expected)`: judged from the descriptor-3 text (a crash with no panic
fails). `#[ignore]`: printed with its reason; `--ignored`/`--include-ignored` run it, kept alike.
`harness = false`: `main` is one test; `--` args go to it and into its key. `tests/*.rs`: a unit
each, in. Doctests: an example per unit (doctests.md), in for libraries. `#[tokio::test]`: a test
after expansion; a multi-thread runtime makes it `reached`. Benches: not run; `--benches` runs
each once as a test; measuring is `blaze bench`, never cached. **Filters**: substring over
`module::name`, `--exact`, `--skip`; libtest's flags (`--nocapture` runs one at a time,
`--test-threads`, `--list`, `--format json`) are blaze's; any other is refused by name.
**Flaky**: no retry (rule 3); a failure is filed beside the pass at its address, and both at one
address is a `Flaky` finding naming the two runs. **Shared**: CI files passes in the
organisation's namespace, signed; a developer hits only an equal address, so exactly when its
code and reads answer as CI's did (target and host facts are asks). Developers never write one.

**Floors** (estimates, Mac numbers from target.md). No edit: floor 2 ms (one head read), design
4 to 7 ms. One body edited in a tempo leaf crate, entered by k tests: the build (15 to 40 ms) +
one fork per test (0.3 ms Linux, 1 ms macOS; k at once) + the slowest test's own time + 1 ms of
report; floor the same less the forks: ratio about 1.1. blaze's tests are 0.1 to 2 s simulations:
blaze adds 2 to 5 ms, ratio about 1.0. Cargo: the test binary relinked, every dependent test run.

**Against the best.** nextest: matched on a process per test, deadlines, failure output; better:
no binary per crate, results kept, no quiet retry. Bazel, Buck2: matched on kept, shared
results; better: their grain is the target (an edit below reruns every dependent target's
tests), ours the function a test entered, and reads are traced, not declared. Meta's and
Google's selection: graph or learned odds that accept misses; ours is exact. testmon: the same
idea; better: identity covers layouts and constants, reads are asks, marks have one owner.
Jest/Vitest `--changed`: file grain over imports. Zig: matched on compiler-found tests and a
protocol to the parent; better: a crash fails one test, and results are kept.

### Gap (2026-11-16, tree at 3f8b478055)

| Needed | Today, with evidence | Missing | Reached by |
|---|---|---|---|
| `blaze test` runs tests | blaze-cli main.rs `close`: prints "tests are not run yet", returns FAILED; blaze-drive request.rs `runs_program`/`units_known`: `Test` starts the package's program like `Run` | the whole runner | the command |
| Selection forward from C | blaze-run suite.rs `Tests::affected`: postings in a HashMap in memory, never a record; a failed test is not rerun | the record form, failures, reads, undo by address | only a test |
| Tests listed | blaze-scope tests_record.rs `encode`, `TestsRecord::find`: no caller outside its test | the driver filing it per test unit | only a test |
| Process per test, zygote, descriptor 3 | blaze-ladder place.rs: the program runs on blaze's own thread, every function compiled before the entry ("Shortcuts") | zygote, fork, report protocol, capture | only the ladder |
| Entered marks | blaze-run suite.rs `entered`; blaze-launch-rt observe.rs mark stub | the wire message `TestEnded`, the per-process bitmap read | only a test |
| Result records | blaze-keyed kind.rs `TestRun` (tag 8, `TestInputs`); blaze-drive run.rs `test_name`, `code_read`: no caller | the writer, the head, the flaky record | nothing |
| Sandbox per test | blaze-proc-macro native_sandbox.rs `run`: build scripts only | a test profile, temporary folder, refusals named | nothing |
| Outside reads traced | place.rs `symbol` binds imports by `dlsym` (the seam); blaze-std hosting.rs `Hold` for hosted bstd programs | recording wrappers, not-kept classes | nothing |
| Effect facts per instance | no match for an effect fact in front2 | the precheck of not-kept | nothing |
| Lib and test unit share records | none: two crate keys (above, "A test unit") | code-identity keys | the command |
| Integration tests, doctests | unit graph has no kind for `tests/`; blaze-doc doctest.rs is the old path's | units, records | nothing |
| Shared results | blaze-remote serves records; nothing for `TestRun` | CI writer, signed statements | nothing |

### Pieces, in build order (one writer each; NB = needs the builder to build real bodies)

1. blaze-run plan.rs: the plan machine (filter, ignore, candidates from postings, look, run
   queue by width and longest-first, deadlines, judge, report). Written 2026-11-16, not compiled.
2. blaze-run postings.rs: the postings record (sorted (ask, test) rows; open, find). Written with 1.
3. The head record: one row per test (id, last outcome, address, micros, asks range) and the
   read table; encode, open, round trip. Now; with 4, 5, 11, 14, 16.
4. The result value and the `Flaky` finding: the pass and failure values at an address; a fail
   beside a pass is flaky. Now.
5. The run report: failures first as they end, capped output, summary, `--format json`; a pure
   formatter in blaze-run. Now.
6. The driver files the tests record when a test unit's names are solved (blaze-drive; DRIVER
   runner 10). Now; with 7.
7. `blaze test` arguments into `plan::Config`: filters, libtest flags, refusal by name (blaze-cli;
   DRIVER runner 10). Now.
8. The suite entry: a generated function that calls row n of the tests record (front2 program.rs).
   NB.
9. The zygote and the per-test child: fork, sandbox, temporary folder, descriptor-3 report,
   deadline kill with stacks (blaze-launch). NB to run; writable now.
10. `TestEnded{test, verdict, entered}` on the wire, marks read per process (blaze-launch-rt). NB.
11. The OS-import classifier: a call name and arguments to an ask or a not-kept reason, sans-IO.
    Now; its wrappers NB.
12. The not-kept precheck from effect facts. Waits on effect facts on the new layer.
13. Basis `reached` for a test that started a thread: the reach record per test (front2 reach.rs).
    NB.
14. Integration-test units: blaze-manifest reads `tests/`; a unit kind per file. Now.
15. Doctest units on the rebuilt path. NB.
16. The drive loop: after the gate, per suite `Plan` effects to store lookups, the zygote and puts
    (blaze-drive; DRIVER runner 10). After 1 to 7.
17. Shared results: CI puts `TestRun` in its namespace, signed; lookups through the remote
    source. After 16 and the store lane.
18. Records shared between a lib unit and its test unit, by code-identity keys (front end). Large.
