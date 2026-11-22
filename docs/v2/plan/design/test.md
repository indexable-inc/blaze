---
status: plan
summary: What a test is on the rebuilt path. A test unit is a unit like any other with `test` in its cfg and its own key; its tests are one record per unit built from attributes alone (crates/blaze-scope/src/tests_record.rs); the runner is blaze's own, one forked process per test over the suite's shared code area; a result is a record read-addressed by the functions the test entered and the outside reads it made, so an edit reruns only the tests that entered what changed, found by a postings record (End state, with the gap and the pieces). Built 2026-11-16: test units become crates of the names set, and the tests record with its reader; written, not compiled: the plan machine and the postings record (crates/blaze-run plan.rs, postings.rs). Not built: the runner, the head record, integration-test units, benches, doc tests. A second end state (2026-11-16) decides property tests as a kind the runner knows (case board, failing tapes as shared records, background search), differential tests (in-tree `differ`, the previous version by `previous(f)`), end-to-end tests (traced spawns, step traces, snapshots as reads) and judge-changed findings.
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

## A test build is a view of its crate (D152, coordinator, 2026-11-19; NOT built; order changed by D191 below)

**What is there today, and why it fails.** A package's library compiled for tests is a second
UNIT (`UnitKind::Test { bin: None }`; a binary's is `Test { bin: Some(name) }`) with a crate key
of its own: the kind (`unit_key`, blaze-scope units.rs) and the atom `test` (`unit_crate_key`,
blaze-world list.rs) are hashed into it, every `ItemKey` starts with that key's prefix and
hashes its parent's whole key (task/lower.rs `make_key`, `key_of`), so one text has two keys.
Nothing but a file's outline is shared; a body both units hold is checked under the first alone
(engine_bodies.rs `Holders::decide`). Seen on 2026-11-19 (rounds/wave-1119/t5): a test that
calls an ordinary library fn asks the run's table for the test unit's key, finds no row, and is
NOT RUN. Every real test calls library code.

**The design.** The test build is the SAME crate under one more fact, `test`, and its answer is
the base answer plus what that fact moves. It is the two-value case of conditions.md, built first.

1. **One crate key.** The kind `test`, the atom `test` and the dev-dependencies leave the crate
   key (the features resolved for the request stay). An item has one `ItemKey` in both views, so
   an answer that names an own item is equal bytes in both, and no key is mapped between units
   (front2 `shared_keys.rs` and its `PrefixSwap`/`ByPlace` are deleted).
2. **A key counts what is written, not what cfg keeps.** Today `Lower::item` drops a cfg'd-out
   item before `Lower::key` counts siblings per (owner, tag, name), so `#[cfg(test)] fn f` and
   `#[cfg(not(test))] fn f` would share a key and a `#[cfg(test)] impl` would renumber the impls
   after it. A key's own part becomes the outline's: the entry's sibling key and disambiguator
   (blaze-file-items digest.rs `sibling_keys`: cfg-blind, an unnamed entry's key holds its
   head), the bytes `item_chain` hashes. One scheme (rule 30); the second and later impls of a
   module get new keys once.
3. **A view is (crate, test).** The crate's names record (`CrateScope`) is made per view from
   the same outline records. Each name of the extern prelude is its own cell (today `Root(crate)`
   holds the extern prelude, the `macro_use` prelude and the exported macros as one, so a
   dev-dependency would move every path walk).
4. **cfg is read, never folded in.** `Cfg::eval` (names lowering) and `CrateCfg` (heads, bodies)
   become a host that records each atom asked with its outcome, after expansion (a `cfg!(test)`
   out of a dependency's macro is the calling crate's read). A record that read `test` is stored
   under the stable variable of ITS crate's `test` (conditions.md `VarKey`, which today spells
   `cfg:<name>` with no crate: a per-crate key is added; the drive's request-local `Var` number
   is not stored). blaze-lower already records its asks (`CfgRead`, read by nothing); blaze-expand
   records a `cfg!` as its whole predicate, and records the atoms instead. `cfg_attr`, a field, variant, statement or arm
   under cfg, and a crate attribute (`#![cfg_attr(not(test), no_std)]`) are reads of the record
   that lowers them.
5. **One engine, one answer: a view's rows are lifted from the base's.** The view is not a
   second request and not a second answer. A task's name and a cell's id may carry a view
   (`test` of one crate; the drive's `at_condition`, keyed by the stable variable). The rules,
   which are the whole mechanism (rule 53):
   - **A view cell exists only where it differs.** A cell `c@v` whose digest equals `c`'s is
     absent: the merge drops it. A read of `c` in view `v` is a read of `c@v` when it exists,
     else of `c`, and the row records both (the absence is a read).
   - **The cfg atom is a cell.** `cfg:test` of a crate is false in the base and true in its
     view; a file not held and an item not present are cells too (`not_held_cell` is there).
     So an item that exists only under `test` has a base row that says "absent" and read
     `cfg:test`.
   - **The lift.** When a view cell `c@v` appears, moves or goes, the engine enters the view
     rows that read it and, for each BASE row `T` that read `c` and has no `T@v`, plans `T@v`
     from `T`'s own plan entry (`planned_of`) and enters it. Nothing else plans a view row.
     `T@v` whose reads all fall back to the base again is taken out.
   - **The roots of a view** are entered by key: its names record (`CrateScope@v`, made whole
     from the same outline records, with the dev-dependencies) and its `cfg:test` cell. Their
     cells that differ from the base's are the first view cells; everything else follows by
     the lift. A test-only item, a `cfg!(test)` in a body, a test-only impl in a bucket a
     shared body read (front2 reads.rs lists the buckets), a `#[cfg(test)] use` that moves a
     name: each is a reader of a view cell and nothing more.
   - **Time needs no second rule.** A view row is a row: it stands or is entered by its reads
     as any row, so an edit costs in the view what it moved in the view.
   `Holders` and "the first holder checks" are deleted. The engine's proof is its simulation
   (blaze-engine sim.rs): the oracle is the same history with the view checked as a crate of its
   own, every row; the lifted answer's rows equal that one's wherever they exist and the base's
   wherever they do not; the control drops the lift.
6. **A view reads through to the base.** A worker in view `v` finds a record by `name@v`, then
   by `name` (`RecordKeys::get` already reads changes over a base this way). `blaze test`'s
   body-id table is made the same way. Code is keyed by content and a callee is found by id
   through the view's table at first call, so a shared fn's code is one record even where a
   callee differs. The entry point is per view (the harness replaces a binary's `main`);
   coherence is checked per view.
7. **Others read the base.** A dependent crate, an integration test, an example and a bench read
   the package's base view (resolve.rs: "never the library's test unit"). `test` is a variable
   only its own crate's items see as true. Doctests are a third view (`doctest`), later.
8. **A package that reaches itself through a dev-dependency** (`zone-node` and
   `tempo-precompiles` in tempo; 16 of reth's 152 packages) has BOTH builds in one test program,
   with distinct types and statics. The unit graph gives such a package's test build a crate key
   of its own and a FULL view: every task of its files is planned in the view (its keys differ,
   so nothing falls back). Correct at the old cost, for those packages alone, by the same path.

**Not covered until built, each refused by name:** a body's macro resolution is not in its read
list (`NotListed::LoweringMacros`), so until the lowering records it, a body under a module whose
textual macro scope differs between the views is entered in the test view.

**Against conditions.md.** That design keeps ONE variational record a key, its entries under
conditions, and forks a task per distinct outcome. A view row is that fork for the one variable
`test`, stored as a row of its own; nothing here is thrown away when rows become variational
(the key scheme, cfg as a read, the cells, the lift's reads are all its inputs), and a second
variable is not added by this mechanism (it would multiply rows): features and targets wait for
the variational record.

**To port first** (found by the inventory, rounds/wave-1120/SEAMS.md): two units with one crate
key are merged or refused today (blaze-world list.rs:296-305, crate_list.rs `SameKey`,
run_ids.rs `PrefixClash`); the items record stores the crate's cfg set (`items_record.rs`
`cfg_atoms`), so its bytes are per view until cfg is a read.

**The first slice** (nothing thrown away): 2, then 1; 3; 4; the engine's view cells and lift
with its simulation; 5's roots; 6; 8; delete `shared_keys.rs`. Its judge: a test that calls a library fn runs; a package with tests
enters, in its test view, only the bodies that read `test` or a moved cell (a fixture counts
them; the control is a `cfg!(test)` added to one body, which adds exactly that body); tempo's
check enters no more bodies than its base view plus its test-only code.

## The order as built (D191, coordinator, 2026-11-21)

The first slice above put point 8 last. The real run (`blaze test -p tempo-hardfork`, 2026-11-21)
needs none of the key port, cfg-as-read or the lift to run a test: it needs a ROW under the test
unit's keys for each library body the test calls. So the FULL view (point 8's permanent case) is
built first, for every package, on today's keys:

- **Every holder checks.** A body's plain name is its base row: the first holder in rank (a
  non-test unit first). Each other unit that holds the body has the row `name@view`, the view
  being that unit (one name function, beside `at_condition`). "The first holder checks"
  (`Holders::decide`) is deleted: it reported a body checked that it never checked under `test`.
- **Run and test ask by view.** The body-id table of a unit asks `name@view` where that unit is
  not the base holder. No read-through to the base while keys differ (the base's keys are another
  crate's): a missing view row is the typed refusal, never the base's row.
- **Findings print once.** One body's finding with equal item-relative range and text in several
  views is one line; a finding in one view alone names the view.
- **The seam.** `ViewPlan` with `Full` and `Lifted` (rule 78). `Lifted` is point 5 and needs
  one key a crate (points 2, 1); its oracle is `Full`, in the engine's simulation.
- **Cost, stated.** On tempo at most 9,221 more body tasks on 16,586; heads and names add
  nothing (a test unit's heads are frozen per unit today already).

Then: point 2; the unit's identity in every name; the point 1 flip; point 4; the lift; `Lifted`
for the packages whose keys are one.

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
naming the path. A `#[bstd::test]` names what it takes after `cx` (D109: each parameter a grant
made from the fake world, narrowed to it: virtual clock, files in memory, a seeded generator);
its manifest and setup (what its grants start with) are data in the test binary and are the run
spec its `TestInputs` digest covers; it is always kept. One that takes `cx` alone meets no fault
and is run once.

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
| Lib and test unit share records | none: two crate keys (above, "A test build is a view of its crate") | the view (D152) | the command |
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
18. The test build as a view of its crate (D152, above): one crate key, cfg as a read, the
    overlay. Large; first, because no test that calls library code runs without it.

## End state: property, differential and end-to-end tests (architect, 2026-11-16)

Asked (Andrew, 2026-11-16): are end-to-end, property and differential tests, and a verifier
that cannot be fooled, done well in blaze; if not, make them so. Answer from the tree: blaze has
a property engine as a library (crates/blaze-sim) that the runner does not know, and no
differential, end-to-end or judge-changed support in `blaze test`. Everything below sits inside
"End state" above, D81 and bottom.md 11 ("a test run is a program run"; a slot may hold a
version of a function with hooks, coverage among them). Nothing here retries (rule 3) and
nothing compares against rustc or cargo (rule 7).

### A. Property tests are a kind the runner knows

**Two kinds, told apart at run time, not in the tests record.** The tests record stays
attributes alone. A test is a TAPE SEARCH when it opens the case board (below) through
`blaze_sim::check`; every other test is OPAQUE. The head row keeps the kind its last run showed.

- **Opaque, unchanged Rust** (proptest, quickcheck, any `#[test]` drawing from `rand`): the
  sandbox answers `getrandom` and `/dev/urandom` from a seed H(base key, run number), as
  test-selection.md section 2 already decides ("an input of the key, not of the world"). So a
  proptest suite is deterministic and its pass is kept like any test. Run 0 is the foreground;
  searching more means more runs under seeds 1, 2, ... (each run does the library's own cases).
  A failing run's record is its seed. proptest's `proptest-regressions/` files are reads (asks);
  its write of a new one is under the root, so the sandbox refuses it and the failure prints
  that line too; blaze's failing-seed record replaces it. Nothing in proptest is special-cased.
- **Tape search** (`blaze_sim::check(config, |tape| ..)` inside a `#[test]`, blaze's own form,
  rule 75: the randomness is the argument): the runner owns the cases. What that adds over
  opaque: one case per crash instead of one run; shrinking by blaze (shrink.rs `Shrinker`);
  per-case coverage; per-case deadlines; tapes as records shared with CI; a split of one
  test's seeds across idle cores.
- **Arguments drawn by type**: `#[test] fn round_trip(header: Header, body: Vec<u8>)` is blaze's
  extension (rustc refuses a test with arguments, so blaze's own crates use it only once cargo
  no longer builds them, rule 6). It is a builtin attribute expanding to a tape search, as
  `#[blaze::contract_test]` already expands to `blaze_sim::check` (hir-expand
  builtin/attr_macro/contract_test.rs). An argument is drawn by `blaze_sim::Any`, and `Any`
  becomes ONE comptime function over the type's reflected shape (rule 56: `fields_of`,
  `variants_of`; a struct field by field, an enum by variant index then fields, the first
  variant calm), replacing draw.rs's hand impls except for primitives. A type whose fields are
  not all visible to the test's module, or that keeps an invariant, names its generator with a
  mark (`#[blaze::mark(Draw(path))]`); a type with neither is refused naming the type and field.

**The case board.** Per tape-search child the zygote maps one shared page set: the plan (given
tapes, seed range, per-case budget), a progress word (the case now running), the record of
that case's choices as they are drawn, and the case's coverage bits. The child finds it at a
fixed descriptor, 4, as it reports on 3 (wire.rs); with no board (cargo, libtest)
`blaze_sim::check` runs as today. Because the builder has no unwinding, a case's panic ends the
child: the parent reads the board and knows the failing case and its tape exactly, with no
`catch_unwind` (check.rs `drive` needs one today). The seed phase runs inside the child (no
round trip per case); after a failure the parent runs search.rs's sans-IO `Search` and
`Shrinker` itself and hands candidates to children, a fresh fork only when a candidate ends
one. The least tape is replayed once in a fresh child from the zygote; if it does not fail the
same way it is search.rs `Verdict::NotAFunction`, filed as a `CaseNotAFunction` finding (the
case read state an earlier case left): never retried, never hidden.

**Records.** A FAILING TAPE: keyed by the test's id (`TestInputs`, blaze-keyed kind.rs; no code
in it) and the kind; value: the tape (text.rs form), the failure text, the address and the
checkout where it was found, and who found it (local, CI, background). It is replayed FIRST in
every later run of that test, at any address; a test with a live failing tape is never kept,
and a replay that fails ends the test's run at once (the verdict is known; a re-shrink against
the new code runs after the answer). A replay that passes turns it into a REGRESSION TAPE,
replayed in every foreground run from then on (at most 64 per test, oldest dropped with a
printed line). A CORPUS: tapes the background search kept because they reached new coverage,
keyed by test id, at most 1,024; a tape stays a valid input across any edit, since any tape is
a case. CI writes failing tapes and corpora to the organisation's namespace, signed; a
developer's `blaze test` asks for the failing tapes of the tests it plans in one batched lookup,
so a failure CI found replays on a laptop before anything else.

**What "passed" means and when it is reused.** A pass is "the declared cases (default 256,
search.rs `Config::DEFAULT`) and every regression tape passed". Its address is the D81 address:
H(id, asks, answers), the id holding the run spec (seed range, case count, given tapes' digest)
in `TestInputs::args`, and the asks the UNION of the functions every case entered and the reads
every case made. An unmoved address keeps the pass; the declared cases never rerun for it. The
background search files a SEARCHED record per address (seed range done, coverage digest), so
no seed is searched twice at one address (rule 77); it never makes the pass stronger.

**Where the search runs.** Foreground: kept failing tapes, regression tapes, the declared
cases; nothing more. After the answer, a helper with an expiry (rule 70; it holds nothing a
record does not rebuild) searches each tape-search test whose address is new: budget 10,000
cases or 30 s per test, 120 s in all, a per-case deadline of 1 s and search.rs
`Config::most_choices`. What it finds is a failing-tape record and a finding printed at the next
command ("advice after, analyses in the background"). CI runs the same search with 100,000
cases per new address. `blaze test --search[=cases]` runs it in the foreground. A per-case
INSTRUCTION budget (deterministic, where the wall deadline is not) is used where the host gives
a counter to the process: Linux `perf_event` user-mode instructions; on macOS the deadline
alone until a counter is shown to work (rule 45 names the candidate: kpc, a private API).

**Coverage.** Function grain is on by default and costs nothing new: test children already run
with mark stubs in every slot (blaze-launch-rt observe.rs `mark`, a byte per function in a
per-thread bitmap); per case the child clears the bitmap of the functions it entered (about
1 us, estimate). Branch grain is a hooked version of a function (bottom.md 11, a code record
keyed by the function and its hooks), placed only in the background search and only for the
functions whose code identity moved since that test's last SEARCHED record; the foreground is
never instrumented, so its timing and its asks are the plain run's. A guided phase proposes
mutations of corpus tapes (splice, raise, lower one value) and keeps a tape that sets a new
bit. Cost, estimate: 5 to 15 percent on the instrumented functions only.

### B. Differential tests

**(i) Two implementations in one tree.** The library form is `blaze_sim::differ(config,
draw, a, b, Differ { normalize, approve })`: one tape, one drawn input, both called, outputs
compared after `normalize`; `approve(input, a_out, b_out) -> Option<&'static str>` returns the
reason a difference is allowed. A difference prints the shrunk tape, the input and both outputs
(by `Debug` now, a structural diff by reflection later). Rule 78 becomes a check the tool
enforces: a strategy trait carries `#[blaze::mark(Seam)]`; for each one with two or more
instantiations entered by any test in the suite's head, some passing test that called
`blaze_sim::differ` must have entered the seam's functions with BOTH instantiations, else a
`SeamUntested { seam, instantiations }` finding. The data is already exact: entered names hold
type arguments (postings.rs `FnName`). The planted divergence stays a `blaze_sim::control`.

**(ii) The previous version as the oracle, no second checkout.** `core::blaze::previous(f)`
gives a function value of `f`'s signature that calls `f` as of the run's BASE: `--base green`
(the branch's last green certificate, the default) or `--base <commit>`; the base is in the run
spec, so in the key. What it needs:
- the base's records stay live: CI files the test program's manifest (instances by key) for
  each green commit beside its certificate; the store keeps those named by the last 30 greens
  of main and of each open branch (a collection rule the store lane must accept; see the end);
- placement in ONE process: the launcher places the base manifest's instances under their own
  slots, and an instance whose code identity is equal in both shares code and slot, so only the
  bodies that moved exist twice;
- refusals, by name, before anything runs: `NoOldVersion { f, base }` (no instance at the base);
  `BoundaryTypeMoved { f, type }` (an argument or return type whose identity differs: compare at
  a boundary that did not move, such as bytes in and bytes out); `NotPure { f, effect }` from the
  effect facts (a static read or written, IO, a clock: old and new would share or race on it).
  The global allocator is allowed: one allocator instance serves both.
- inputs: drawn from the tape as in A, or SAMPLED: a recording version of `f` (bottom.md 11,
  "recording effects") placed during one suite run stores its arguments, encoded by reflection,
  into a samples record keyed by (`f`'s stable name, tree certificate), at most 1,000, chosen by
  a hash of the value (rule 25), never by time;
- approved differences live in the test's own `approve` function: code, so a change to them is
  a change to the test's judge (D). Normalising is `normalize`'s job, not a list.
- the print:
  ```
  codec::decode differs from green 3f8b478055 at tape `2 0*3 255`
    input  Header { len: 0, kind: Ping }
    green  Ok(Frame { body: [] })
    now    Err(TooShort { at: 0 })
  10,000 cases: 9,996 equal, 1 differs, 3 approved ("len 0 is an error now, #2210")
  ```
Not in this form: state across calls, IO, and types that changed shape; each is refused above,
never compared loosely. A whole program against its previous build is C plus (ii)'s base: the
same e2e test run on the base's manifest and on today's, outputs compared.

**(iii) Never against rustc or cargo** (rule 7). The base is always a blaze build.

### C. End-to-end tests

An e2e test is any test that runs the program or a command: a `tests/*.rs` file spawning a
workspace binary, a `harness = false` driver, a server started on loopback. `CARGO_BIN_EXE_<n>`
names a launcher stub for that binary's manifest (no executable is written, bottom.md 11), so
the spawned process is a launched program whose entered functions and reads join the test's
asks, as test-selection.md section 2 decides ("Child processes: kept when traced"); oscall.rs
`RULES` must change `Spawn` from `NotKept::Foreign` to traced for a workspace binary (any other
program stays not kept). Loopback servers the test starts are its own (oscall.rs
`ConnectOwnLoopback`). The monotonic and CPU clocks become KEPT, the wall clock stays not kept:
reversing oscall.rs's reading, because an async runtime reads `Instant` on every poll and would
make every tokio e2e test unkeepable; a pass that rested on timing is a pass, and a failure
always reruns, so the risk is no larger than a threaded test's under D81. Hence an e2e suite
with no ask moved costs a head read plus the read table answered (E).

**A failure names the step.** A timed-out or failed e2e test prints, from the child and every
launched process under it: the stacks at the deadline (zygote.rs `Effect::Stacks`, wire.rs
`Stacks`), symbolised; per thread the blocking call in flight and on what (a pipe from which
process, a connect to which port, a lock), read by the control thread (Linux
`/proc/self/task/*/syscall`, macOS `thread_info`); the open spans of code using blaze_core's
waits (rules 15, 44: each names what it waits for); the test crate's own functions it entered,
in item order, the one still on the stack marked; the last 20 lines of each process's captured
output. So:
```
FAILED tests::place_order: deadline 60 s
  step   place_order > wait_ready (still inside)
  waits  main: read(fd 7: pipe from `shop-server` pid 412) for 59.8 s
         shop-server main: span "load payment form" deadline 30 s, expired
  output shop-server: "error: payment config missing: config/payment.toml"
```
never `timed out`.

**Expectations (snapshots).** An expectation is a file under the root a test reads (insta's
`snapshots/*.snap`, an expect-test file); it is an ask, so a changed expectation reruns the test.
A test never rewrites its judge: a write under the root is refused by the sandbox (oscall.rs
`(OpenWrite, Root)`). `blaze test --accept <filter>` is the documented opt-out: the selected
tests may write under the root, the run is never kept or shared, the verdict line says
"accepting", and the files it changed are reported as judge changes (D). Inline expectations
(`expect![[..]]`) are source: a change to them is a body edit of the test, seen by D.

**Effects.** Network off the machine is refused (oscall.rs `ConnectRemote`); the opt-out is
per test in the manifest's blaze table (`network = ["host:port"]`), printed on the verdict line,
never kept, never shared. Writes go to the test's temporary folder only.

### D. The verifier's own properties

- **No retry**: D81 and result.rs `flaky`; for cases, `CaseNotAFunction` (A).
- **Results name the exact tree**: a result record holds the run's checkout (result.rs's form,
  32 bytes after the header); the summary line ends with the tree certificate and, when the
  tree equals a commit, that commit (`at 3f8b478055`), else `at 3f8b478055 + 4 files`.
- **A change cannot modify its own check silently.** `blaze test` and `blaze check` compare the
  tree against the base (as B) and file a JUDGE-CHANGED finding (a record keyed by kind, test id,
  base and tree certificates; rule 81) for each test whose judge changed in the same change
  that moved code its last run entered: `TestDeleted` (a row gone from the tests record; worse
  when its last result failed), `IgnoreAdded`, `ShouldPanicChanged`, `BudgetLowered` (cases,
  seeds or deadline in its run spec), `ExpectationChanged` (a file under the root it read that
  is no unit's source), `ApproveChanged` (its `approve` or `normalize` body moved),
  `TapeDropped` (a failing or regression tape removed by hand), and `JudgeAndCodeMoved` (the
  test's own body moved). All print as advice after the answer. CI enforces the first seven:
  land.py refuses a merge with one unless the pull request carries
  `Judge-changed: <test path>: <reason>` for it. `JudgeAndCodeMoved` is reported, not gated
  (a refactor moves both every day).

### E. Floors (estimates unless marked; Mac, 8 performance cores)

1. Repeat `blaze test`, nothing changed, property tests included: one head read, every pass
   kept; floor 2 ms, design 4 to 7 ms (as "Floors" above). No case runs; the helper starts
   only if a SEARCHED budget is unspent and never delays the answer.
2. One body edited, entered by one property test of 10,000 declared cases at about 20 us a case:
   the build (15 to 40 ms) + 8 children (1 ms fork each, at once) + 10,000 x 20 us / 8 = 25 ms
   + report 1 ms: about 45 to 70 ms; floor the build plus 25 ms, ratio about 1.05. The
   background then searches 10,000 more with branch counters on the edited function, 0.2 s of
   CPU after the answer.
3. Replay of a kept failing tape: head read 2 ms + the build if code moved + one fork (1 ms
   macOS, 0.3 ms Linux) + one case + report: about 4 ms with no code moved. The verdict needs
   no other case.
4. An e2e suite of 100 tests, none of whose asks moved: the head read 2 ms + the read table
   answered (about 1,000 distinct files at 1.5 us a stat, 1.5 ms) = 3.5 ms floor, 5 to 10 ms
   design. cargo or nextest rerun all 100 (seconds to minutes). Measured today: nothing; the
   nearest measured number is the no-edit names step on tempo, 27.9 to 37 ms
   (read-addressed.md, Gap), which the head read replaces.

### F. Gap (tree at 131c427750)

| Needed | Today, with evidence | Missing |
|---|---|---|
| runner knows tape searches | blaze-sim check.rs `check` reads `BLAZE_SIM_*` from the environment, `drive` uses `catch_unwind`; plan.rs has no property kind | case board, kind in head row |
| sans-IO search in the parent | search.rs `Search::next`/`ran`, shrink.rs `Shrinker::next`/`ran`: sans-IO already | a driver in blaze-run |
| failing tapes, corpus, searched | text.rs `print`/`parse` give the tape line; nothing keeps it | three records, the shared lookup |
| seeded randomness | test-selection.md section 2 decides it; oscall.rs `RULES` (`Random`, `Own`) and its doc pass the real call through | the wrapper answering from the seed |
| clocks for e2e | oscall.rs `RULES`: monotonic and CPU are `NotKept::Clock` | the row change |
| spawned workspace binaries | oscall.rs `RULES`: `Spawn` is `NotKept::Foreign` | stub for `CARGO_BIN_EXE_*`, joined asks |
| function coverage per case | observe.rs `mark` bitmap; test.md piece 10 reads it per process | per-case clear and read |
| branch coverage | blaze-launch-host record.rs refuses SanitizerCoverage code; bottom.md 11 versions with hooks: design only | a coverage hook version |
| guided search | search.rs proposes seeds `first..first + cases` or a given tape only | a guided phase |
| draws by reflection | draw.rs `Any`: hand impls per type; comptime reflection: extensions.md row 112 | `Any` over reflection, `Draw` mark |
| args-drawn tests | contract_test.rs expands one `blaze_sim::check` per function | the `#[test]`-with-arguments form |
| differ, seam check | control.rs `control` (planted bugs); rule 78 by habit (blaze-memo tests/resident.rs) | `differ`, `SeamUntested` |
| `previous(f)` | code records by code identity (code-identity.md); no base manifest kept, no primitive | primitive, base manifests, refusals |
| step trace | zygote.rs `Effect::Stacks`, wire.rs `encode_stacks_into` | blocked calls, spans, entered steps in the report |
| judge-changed | result.rs `flaky` is the one test finding | the finding, the CI gate |

### G. Pieces

One writer each, none compiles; "CHILD" = needs the test runner's child (test.md pieces 7 to 11).

1. blaze-sim board.rs: the case board's form (plan, progress word, record area, coverage
   bits); encode, open with every bound checked. Test `board_round_trips` and a cut board
   refused. Touches nothing else. Parallel, now.
2. blaze-sim check.rs: `check` drives from a board when descriptor 4 holds one: seed phase in
   process, progress and choices written before each case, no `catch_unwind` under the board.
   Test (sim): a body panicking at case 17 leaves case 17's tape on the board. After 1.
3. blaze-run property.rs: the parent's machine over a board after a child ends: passed range,
   failing tape, `Shrinker` candidates to children, final replay, `CaseNotAFunction`. Sans-IO,
   tested in blaze-run's sim with a planted body. Prints "FAILED x: least tape 2 0*3 255".
   After 1; parallel with 2.
4. blaze-run tapes.rs: the failing-tape, regression, corpus and searched records: forms, caps,
   round trips. Parallel, now.
5. blaze-run plan.rs: a property row's decide: failing tape first and never kept, seed chunks
   across idle width, regression tapes in the run spec. After 4; coordinate with plan.rs's lane.
6. blaze-run oscall.rs `RULES`: monotonic and CPU clocks kept; `Spawn` of a workspace binary
   traced; doc text corrected to test-selection.md's randomness. Test `table_covers_every_op`
   stands. Parallel, now.
7. The launcher's `getrandom`/`/dev/urandom` wrapper answering from H(base key, run). CHILD.
8. blaze-sim search.rs: a guided phase over a corpus (splice, raise, lower), keeping a tape that
   sets a new bit; control: a planted needle found guided within 10,000 cases and not unguided.
   Parallel, now; touches no other module.
9. The background search helper: after the answer, budgets, expiry, records, the next command's
   print `blaze: background search found codec::round_trip failing at tape ..`. After 3, 5. CHILD.
10. Branch-coverage hook version for moved functions (launcher, bottom.md 11 versions). CHILD.
11. blaze-sim draw.rs `Any` by reflection and the `Draw` mark. Waits on comptime on the new layer.
12. The `#[test]`-with-arguments builtin attribute, beside contract_test.rs. After 11 for types
    other than primitives; parallel otherwise.
13. blaze-sim differ.rs: `differ`, `Differ { normalize, approve }`, the difference print, a
    planted-divergence control. Parallel, now.
14. blaze-run seams.rs: `SeamUntested` from the head's entered names and the `Seam` marks;
    pure; test with a planted untested seam. After test.md piece 3; parallel.
15. `previous(f)`: (a) the refusal check, sans-IO, given both instances' descriptions; now.
    (b) the primitive in core's blaze module and placement of the base manifest. CHILD, large.
    (c) CI files green manifests and the store keeps 30 greens. After test.md piece 17.
16. Samples: the recording version storing `f`'s arguments into a samples record. After 11. CHILD.
17. `CARGO_BIN_EXE_*` as launcher stubs; a spawned workspace program's asks joined. CHILD.
18. report.rs `timed_out`: the step trace print from given stacks, blocked calls, spans and
    entered steps; a pure formatter, golden-text test. Parallel, now. Its capture (the control
    thread reading blocked calls per OS): CHILD.
19. blaze-run judge.rs: the judge-changed findings from (base and tree tests records, heads,
    moved C, reads under the root, run specs). Pure, with one planted case per kind. Now.
20. land.py: refuse a merge with a gated judge finding and no `Judge-changed:` trailer. After 19.
21. `blaze test --accept` and the per-test network opt-out in the manifest table. CHILD.

### H. What blaze's own suite changes now

- Refinement tests become `differ`: blaze-memo tests/resident.rs (memo `Resident` against
  blaze-keyed's reference), blaze-keyed's `resident_refines_records_only`, blaze-pack's model
  oracle (tests/model.rs), and each rule-78 seam gets its `Seam` mark so `SeamUntested` lists
  the rest.
- The body referee of bottom.md 16 (the old driver against blaze-typeck) is a `differ` over
  sampled bodies, with its approved differences in code, not in prose.
- The ladder's rungs (hello world, `println!`, ripgrep) become e2e tests through the real
  `blaze run` command, judged by the program's output file, so a rung's failure prints a step.
- Every `blaze_sim::check` keeps working under cargo; under `blaze test` it gets the board,
  kept tapes and the background search with no edit.

Not determined here: whether proptest's failure-persistence write, refused by the sandbox,
only warns or fails the test (its code was not read); whether macOS gives a process an
instruction counter without root; the store's collection rule that must keep 30 greens'
manifests (no store design was read); and the real cost of a case-board progress write and
of branch counters, which want a measurement on the first built piece.

## End state: the search itself: edge values first, vacuity a failure, failures kept (coordinator, 2026-11-16)

The owner, 2026-11-16: "make sure DST primitives that we have have really crazy edge case values ... pick sota
method also consider recording previous failures ... anything that would make it ... better at uncovering bugs".
Decision D95. What blaze-sim has already (crates/blaze-sim): one tape of choices, shrunk and replayed (tape.rs,
shrink.rs); `Tape::int_in` with small offsets from a range's ends; twelve special floats (draw.rs `SPECIAL_F64`);
`Tape::reached` with `Verdict::Unreached` (a point no passing case reached fails the test); `check_given`;
`control` with planted bugs; a coverage-guided phase that takes its signal as an input (guided.rs); `differ`.
blaze-core's `Sim` tears unsynced writes by sector, fails syncs, runs out of room; blaze-check explores
interleavings and checks linearizability. What is missing is below, as pieces S1 to S8.

The method, chosen, with what each part is known for: a choice-sequence tape with internal shrinking (kept);
interesting-value tables at every primitive (the boundary values fuzzers keep in a dictionary); swarm testing
(each case switches a random subset of operation and fault kinds OFF, which finds more than uniform mixing);
repetition bias (a new element or operation repeats an earlier one: collisions, duplicates, the same key twice);
bounded exhaustive enumeration where the space is small (every crash point, every schedule up to a preemption
bound) and priority schedules with a depth bound where it is not; reach points that fail a test which never
exercised what it claims (the same idea as "sometimes" assertions); kept failing tapes replayed first; and a
measure of each test's power from its planted bugs.

ROLLOUT, the same for S1, S2 and S4: a changed distribution changes what all fifty crates' tests see. So it is
built behind blaze-sim's cargo feature `edge-first` (off by default) and the SIM INTEGRATOR runs every dependent
crate's tests with the feature on, one crate at a time; each new failure is a found bug or a test with a wrong
assumption, filed as a kept tape (S3) and sent to the crate's owner; when the harvest is done the coordinator
makes the feature the default and the old path is deleted. A new report (S4) starts in observe mode: it prints,
and fails nothing, until the harvest has read its lines.

- S1a INTEGERS AND SIZES (tape.rs `int`, `int_in`, `Int::draw_int`, `more`, the sizes in `vec`; NEW
  src/edge_ints.rs). One table, used by every width, signed and unsigned: 0, 1, -1, MIN, MAX, MIN+1, MAX-1;
  every power of two with its two neighbours up to the width; the ends of each narrower width seen through a
  wider one (127, 128, 255, 256, 32767, 32768, 65535, 65536, 2^31 and 2^32 and 2^63 with their neighbours). In a
  range: its two ends, each end's neighbour, the middle with its neighbours, and every table value inside the
  range. A size (a list's length, a byte count): 0, 1, 2, the cap and cap-1, powers of two with their neighbours
  inside the cap (7, 8, 9, 15, 16, 17 ... 4095, 4096, 4097), and rarely a size near the cap however large. An
  edge kind is drawn about one case in four; the calm choice stays the simplest value; kinds are ordered so
  shrinking moves from an edge to a simple value.
- S1b BYTES, CHARACTERS, TEXT, FLOATS (draw.rs `bytes`, `char`, `text`, `text_from`, the float draws; NEW
  src/edge_text.rs). Bytes: empty; one byte repeated (0x00, 0xFF, 0x7F, 0x80); ascending; a short pattern
  repeated; valid UTF-8 followed by one byte that breaks it; a long run inside random bytes. Characters: U+0000;
  the ends of each UTF-8 length (U+007F, U+0080, U+07FF, U+0800, U+FFFF, U+10000); U+D7FF and U+E000 beside the
  surrogates; U+10FFFF; U+FEFF; a combining mark; a zero-width joiner; U+202E; the line ends (CR, LF, U+0085,
  U+2028); a tab; a case pair that changes length when folded. Text: empty; one character repeated; CR LF pairs;
  the same text in composed and decomposed form; and for a text used as a NAME (a new draw `name`): ".", "..",
  a leading or trailing space or dot, a separator ("/", "\\"), a NUL, a name of 255 and of 256 bytes. Floats,
  added to the twelve: -0.0; the least and greatest subnormal; 1.0 with its two neighbours; 2^53 with its
  neighbours; the i64 and u64 conversion ends; NaNs of both signs with a payload.
- S2 SHAPES OF COLLECTIONS AND SEQUENCES (tape.rs `vec`; NEW src/shape.rs). Repetition at the tape's level,
  so it needs no `Clone`: with some chance a new element's span REPLAYS the choices of an earlier element's
  span (an equal element), or replays them with the last choice moved by one (a neighbour). A list is also,
  rarely, sorted, reversed, or all one element where the caller gives an order (`vec_ordered`). `Swarm`:
  `tape.swarm(kinds)` draws once a case which kinds are on (each with chance one half, never none), and
  `tape.op(swarm, ..)` draws the next operation from those on, repeating the previous operation or its key with
  some chance. A test that draws operations uses it; the kinds that were off are in the failure's notes.
- S3 KEPT FAILURES (NEW src/kept.rs; check.rs `report`). A failure prints ONE line to keep:
  `blaze-sim keep: <test path> v<primitives version> <the shrunk tape as text>  # <date> <one line why>`. A
  crate keeps its lines in a checked-in `sim-kept.tapes` beside its manifest, compiled in with `include_str!`
  (nothing ambient: the file is an input of the build), and `check_kept(config, KEPT, body)` replays this
  test's lines FIRST, in file order, before any search. A line whose test no longer exists, or that does not
  parse, fails naming the line (never skipped). A line of an older primitives version is still replayed and the
  report says so. Lines are only added (a later ratchet in the gate). In `blaze test` the same lines are
  records (piece 4 of the section above): a failure CI found replays on a laptop.
- S4 VACUITY AND POWER, IN OBSERVE MODE (check.rs, search.rs, control.rs; NEW src/observe.rs). After a search:
  each labelled draw that could have given more than one value and gave one value in every case is listed
  ("never varied"); each edge kind of S1 that no case produced for a label is listed; `control` prints for each
  planted bug the case that caught it ("caught at case k of n") and a plant caught only in the last tenth of the
  budget is listed as weak. One block of lines after the test's own; nothing fails yet.
- S5 CRASH POINTS, EVERY ONE (crates/blaze-core/src/sim/, a NEW file; additive). For an operation sequence of
  bounded length, enumerate EVERY crash point (after each write, between a write and its sync, at each sector
  of a torn write up to a bound) instead of drawing one: the recovery invariant is checked at each. The random
  search stays for long sequences. Also a list, checked against the code, of what `Sim` injects and what it does
  not (a short read, a spurious wake, a completion delivered twice or late, the clock jumping or standing
  still): each missing one added behind a swarm kind.
- S6 SCHEDULES (crates/blaze-check/src/explore.rs and a NEW file; additive). State what `explore` does today
  (exhaustive, bounded, random). Add what is missing of: a preemption bound with exhaustive search under it;
  priority schedules with d change points for a depth-d bug; the schedule of a failure as a kept tape (S3).
- S7 A SIGNAL FOR THE GUIDED PHASE (after S1; guided.rs, check.rs). The coverage words of a case are: its
  reach points, the edge kind each labelled draw took, and a bucket of each labelled draw's value. No compiler
  support needed; deterministic.
- S8 THE HARVEST (the SIM INTEGRATOR; after S1 to S4 compile). Every crate that calls `blaze_sim::check`, with
  `edge-first` on, one crate at a time; the table of new failures by crate and test, each with its kept line.
