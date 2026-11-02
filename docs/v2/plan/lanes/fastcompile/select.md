---
status: plan
summary: blaze v2 lane fastcompile, idea 3 (exact test selection): what exists in blaze-usercode, what is missing (executed versus reached, keys that miss which function a call binds to, the cached mark), crates/blaze-testsel's executed record, function table and selection over identity and bound code, the hooks requested from usercode, lazy, codegen, items, ci and cargo, the exactness argument and tests, and the state of the art it beats.
date: 2026-10-29
---
# fastcompile: exact test selection (`crates/blaze-testsel`)

Idea 3 of [compile-time.md](../../design/compile-time.md): each test run records what it
executed; after an edit only tests that executed a changed function, or read a changed input,
run again; the rest report their cached result, marked as cached. The code is in
`crates/blaze-testsel`; what it needs of other crates is requested in
`v2/requests/fastcompile.md` under `fastcompile-ident-select <n>`.

## What exists

`blaze-usercode` already selects tests, by what codegen's walk says each one *reaches*:

| Piece | Where | What it does |
| --- | --- | --- |
| A test's key | `crates/blaze-usercode/src/test/select.rs`, `Test::identity` | Hash of its name, attributes and the sorted `CodeHash`es of what it reaches |
| Selection | same file, `select` | Walks the test's history; a pass is kept under the identity and its suite's `given`, then by what it read, through `index::slot`/`index::result_key`/`Shape` |
| Recording | same file, `record`, `keepable`, `PASS` | Keeps a pass when it passed, is not outside, and its reads are known and keepable (no network) |
| What it read | `crates/blaze-usercode/src/trace.rs` (`Inputs`, `normalize`, `encode`); `crates/blaze-usercode/src/index.rs` (`Shape`) | Files, directories, environment, traced by the sandbox, part of the key |
| The skip | `select.rs`, `Decision::Skip { took }`; `crates/blaze-usercode/src/test.rs`, `report_unrun` | A skipped test is reported as `Reported::Unreached` with the last run's duration |
| The wiring | `crates/blaze-cargo/src/serve/unit/test.rs`, `select::select` and `select::record` calls, `result` | Maps the skip to `TestOutcome::Unreached` |
| The fork server | `crates/blaze-usercode/src/test/fork_server.rs`, `test/wire.rs` (`VERSION` 1, `Frame::Ended { id, end, output }`), `test/queue.rs` (`Finished`) | Forks one child per test; nothing about what the child executed |

## What is missing

1. **Executed versus reached.** Nothing records what a test executed. `Test::identity` is over
   the static reached set, so an edit to any function a test *could* call reruns it, though
   most tests reach far more than they run (a parser test reaches every error path). There is
   no per-slot counter or log: `crates/blaze-live/src/slots.rs` has slots and no first-call
   mark; `crates/blaze-lazy` resolves a function once and then calls go straight through.
2. **Keys that miss what a call binds to.** `blaze_items::CodeHash` fills each ref by the
   callee's *interface* hash (`crates/blaze-items/src/hash.rs`, the "code" domain's doc), and an
   interface leaves out the name (`crates/blaze-items/src/form.rs`, `container` writes a module
   as one byte). So two free functions of one signature in one module hash alike as callees: a
   caller edited from `a()` to `b()` keeps its code hash, and swapping two such bodies keeps the
   reached *set* of hashes. A test that called `a` and now calls `b` keeps its key and is
   skipped with a stale pass. Usercode's reached-set key has this hole today; blaze-testsel's
   keys do not ([Keys](#keys)).
3. **The inputs a test read, as part of its key.** These exist (`index`, `trace`), with one
   hole: the sandbox drops the test binary itself from what a run read (`not_inputs` in
   `crates/blaze-cargo/src/sandbox.rs`, `Recorder::input`, for every scope), so a test that
   re-executes its own binary (a common pattern for testing `main`, panics and exit codes)
   depends on code covered neither by its reached set nor by any record. Requested: drop
   `not_inputs` only for the run's own process, not for what it spawns.
4. **The cached mark.** A skipped test is `TestOutcome::Unreached` with the last run's
   duration in `millis`: an agent can't tell it from a test whose result is unknown, can't see
   the outcome that stands, and can't see what it was selected by. Missing:
   `blaze_api::TestResult::cached: Option<Cached>` with the cached outcome, its duration and
   its basis (executed or reached, with why).
5. **Saying which set was used.** `Decision::Skip` doesn't say whether the stored pass was
   keyed by an executed set or by the reached set, so an incomplete record would be
   indistinguishable from a whole one.
6. **A failure at unchanged code.** `select` reruns a failed test whatever changed (right), but
   only looks at the newest history entry, so a revert to code a test passed at, a few edits
   back, reruns it.

## Design

### The record

A test's child writes what it executed into a region the fork server maps shared before
forking it (`crates/blaze-testsel/src/record.rs`):

- 16 bytes of header: `MAGIC` (`blzexec1`), the slot count, and `armed`, which the runtime
  writes only after arming every slot; then one byte per jump-table slot.
- In the child, before the test's function runs, the harness calls the runtime's
  `__blaze_exec_arm(region, len)`. It marks every slot the server process ran before the fork
  (the prelude: startup, `.init_array`, the harness, everything resolved or compiled ahead), so
  state those functions left behind is covered; re-points every slot so its next call stores
  `1` at `HEADER + slot` and goes on to the target the slot held; and writes `armed = slots`.
- Threads the test spawns write the same idempotent bytes; processes it forks share the
  mapping. A process it *executes* is a new image: it is covered by its executable being a
  traced file input (with item 3's fix for its own binary).
- After reaping, the server reads the region (`Record::of_region`, with the scan
  `record::hits_of`, eight bytes a step) and sends the hits in `Frame::Ended`. The record is
  whole only if the header is ours, `armed == slots`, and the child exited; a signal, a kill,
  a timeout, a short region or a runtime that did not arm make it
  `Record::Incomplete` with why.

A panicking test, a `should_panic` test, unwinding, drop glue and TLS destructors all call
through slots like any code, so they are logged; panics need nothing special because the
child ends with `_exit` after the harness catches the panic. A whole-binary run (no fork
server) has no record: `Record::incomplete("run whole")`.

### Keys

A function is keyed by two hashes (`crates/blaze-testsel/src/table.rs`):

- **`FnId`**: what it is across versions, its item's key and, for an instance, its type
  arguments by identity (idea 15). A rename or a move makes another.
- **`Bound`**: its code as it runs: its code hash, the `FnId` of each function or method each
  ref binds to, the deep hash of each constant and static it refers to, for each unsize
  coercion the identities its vtable holds, for an instance the facts of its type
  arguments its code reads, and the positions it names ([Positions](#positions)). Requested
  from items as `bound_hash` and from codegen as `ExecTable::fns`.

### Positions

Code holds no position: `line!()`, `file!()`, `column!()`, a panic's and a `#[track_caller]`
function's `Location` are a `Target::Position` relocation to a record the link fills from the
span table of the span's anchor (`crates/blaze-link/src/position.rs`), and debug lines name
spans the same way. So a line added above a function changes no code hash, and a test can
still see the move: `#[should_panic(expected = "src/lib.rs:12")]`, `assert_eq!(line!(), 12)`,
a backtrace's lines.

Two ways to put positions in the key were weighed:

- **One hash of the binary's span tables beside `unlogged`** (`ExecTable::beside`), as cargo's
  reached-set selection does (`SPANS` in `crates/blaze-cargo/src/serve/unit/compile.rs`). It
  is exact, and it reruns every test of the binary on almost every edit: a line added or
  removed in a file moves the table of every item below it, so a test that never ran any of
  them reruns too. Copy-and-patch makes such an edit cost nothing to compile; this would make
  it cost the whole suite to test.
- **Each function's `Bound` covers the span tables it names** (chosen): the tables of the
  anchors of its positions, its data's positions and its debug lines, by content. A test sees
  a position only by running code that names it: a `Location` is made by the function whose
  relocation names it, and a backtrace shows the lines of the frames on the stack. Those
  functions are in the test's set (executed, or reached when the record is not whole) or in
  `unlogged`. So a move reruns exactly the tests that executed a moved function, and a test
  that executed none of them keeps its pass. A function's anchors are its body's own (a body
  has several), and an edit inside a body changes its code anyway, so naming whole tables
  rather than each span costs no selectivity and a lookup per anchor instead of per span.

blaze-testsel needs nothing new for this: a moved function is a changed `Bound`, found by the
selector's postings like any edit (\(O(P_D)\)), and `Why::Changed` names it. Codegen folds the
tables into `ExecTable::fns` (queue 01028) from the span tables cargo passes it (queue 01029).
The same request covers a static's value, which no `Bound` held either: a test reading
`static N: u32` kept its pass when `N`'s value changed.

The rebinding edit (`a()` to `b()`) changes the caller's `Bound`; a swap of two bodies changes
both `Bound`s. Each test binary carries a table (requested from codegen): its functions as
`Version { id, bound }` in codegen's order, each slot's function, the content id of the code
in no table (the premapped dependency image, `beside`), and each test's reached functions.

Code called not through a slot (and `beside`) is folded into one hash, `Image::unlogged`,
sorted by identity so table order doesn't matter; every set carries it. Any change there
reruns every test: this is where exactness costs selectivity, and it shrinks as more code
goes through slots.

### Sets and selection

A `Set` is a test's functions, sorted and deduplicated, plus the unlogged hash, encoded as
64-byte versions and stored by content (`Set::key_of`). A pass is kept under
`RunKey { code: H(name, attrs, set key), given }` and then by what the run read, through
usercode's `index::slot`/`result_key`/`Shape`, unchanged.

Each test has a history (`select.rs`): an append-only chain of entries in the store, found by
doubling then halving. Each holds passed, kept, attributes, the set key, the duration, the
basis and, for a kept pass or a failure, what the run read. `choose` walks it newest first,
at most `LOOKBACK` (16) entries:

1. Outside: run (`Why::Outside`). All asked: run (`Why::All`). No history: run (`Why::New`).
2. Other attributes: look further.
3. The entry's set is not the binary's now (`Image::changed`: a function changed, is gone, or
   the unlogged code changed): look further. Sets shared by entries are decoded once.
4. A failure at this code: if it read nothing that changed, or what it read is unknown, run
   (`Why::Failed`); else look further.
5. A pass not kept: look further.
6. A pass kept: if the store holds it for the inputs as they are now, the test is cached
   (`Choice::Cached { took, basis }`); else look further.

The `Why` of a run is the first explanation met, newest first.

`keep` stores the set under `Basis::Executed` from a whole record, or the reached set under
`Basis::Reached { why }` when the record is incomplete, saying why. A record of another binary
(slot count or slot out of the table) is a `RecordError`, never a quiet reached set. Failures
are not cached: a failed test always runs again, as Bazel does.

### The cached mark

`blaze_api::TestResult` gains `cached: Option<Cached>`, with
`Cached { basis: Basis, millis: u64, outcome: TestOutcome }` and
`Basis { Executed, Reached { why: String } }`, and `SCHEMA` goes to 8. A cached result has
`outcome: Unreached`, `millis: 0` (it took nothing now) and `cached: Some(..)` with the
cached run's outcome (always `Passed`) and duration. `crates/blaze-testsel/src/api.rs`
converts both ways and refuses a cached outcome that is not a pass.

## Why it is exact

A test's run is a function of the code it executes and what it reads (sandboxing makes the
reads traced; the clock, randomness and network are untraceable and make a pass not
keepable). Take a pass kept under set S and inputs T, and a binary now where every version in
S is the same and the inputs read under T have the same values.

- Every function the run called through a slot is in S (whole record) or in the reached set,
  a superset (incomplete record).
- Every function called not through a slot is in the unlogged hash, which S carries.
- Each function's `Bound` covers its code and which function each of its calls binds to, so
  the same versions make the same calls to the same functions, with the same constants, data,
  statics and vtables, and see the same positions.

By induction over the run's steps, it makes the same calls with the same values and reads the
same inputs, so it passes again. A function that is new in the binary can't be reached
without some function in S changing which function it calls (its `Bound`) or which vtable it
builds (also its `Bound`). The prelude is marked, so state left by startup code is covered.

The holes this closes are item 2 (callee rebinding) and item 3 (self-execution). Fallbacks
are named: an incomplete record keeps a pass under the reached set with why, shown as
`Basis::Reached` in `--explain` and in the API, never silently.

## Hooks

Each is a request in `v2/requests/fastcompile.md`:

| n | Owner | What |
| --- | --- | --- |
| 2 | usercode | The fork server maps and arms the record region and sends `Frame::Ended { executed }` (`wire::VERSION` 2); `Finished.executed`; usercode's reached-set selection retires in favor of blaze-testsel |
| 3 | usercode (sandbox, `blaze-cargo/src/sandbox.rs`) | `not_inputs` only for the run's own process |
| 9 (was 4) | link (blaze-live, blaze-link's slot stubs) | Log stubs and a log table; `__blaze_exec_slots` and `__blaze_exec_arm` arm by one store of the table cell; slot writes while armed go through `publish`, which lazy's `Placer` uses too |
| 5 | codegen | The per-binary exec table: versions in order, slot functions, `beside`, each test's reached indices |
| queue 01028, 01029 | codegen, cargo | Each `Bound` in the exec table covers the span tables its function names and the values of the statics it refers to; cargo passes the link's span tables to `ExecTable::new` |
| 6 | items | `bound_hash` |
| 7 | cargo | Wire `choose`/`keep` in `serve/unit/test.rs`, the table from `compile.rs`, the cached result in `result` |
| 8 | ci | `blaze_api::Cached`, `Basis`, `TestResult::cached`, `SCHEMA` 8, and every constructor site |

## Proof

The tests live beside the code:

- `record/tests.rs`: the eight-bytes-a-step scan against a byte-by-byte reference over random
  maps; region completeness (unarmed, short, foreign, killed); hits validation.
- `table/tests.rs`: the open-addressing lookup against a map; table refusals; the unlogged
  hash is independent of table order and sensitive to every unlogged version and `beside`,
  and is the comparison sort's hash at every size, ids that share 8-byte prefixes included;
  sets built from ranks (bitset or sort) are `Set::of` the same versions; sets sort,
  deduplicate, round-trip and report the first change.
- `selector/tests.rs`: postings across blocks and the id tables against plain models; every
  set's fit, read from counts, is `Image::changed`'s answer over random sets, edits, reverts,
  gone functions and unlogged changes; advancing to a binary, changed or not, allocates
  nothing.
- `select/tests.rs`: an edit to a reached function that wasn't executed keeps the pass; an
  executed one reruns; a revert finds the old pass; failures rerun with the inputs they failed
  with; an incomplete record keeps the pass under the reached set and says why; unlogged
  changes rerun; a gone function, read inputs, unkeepable passes, outside, all, attributes and
  lookback; history end at every length; entry round trip.
- `sim_tests.rs`, a deterministic simulation of a program whose function versions call others
  (some only under a bit of a file they read), read or not, fail or not, and observe their
  own position or not (a `should_panic` with a location). Over random edits, reverts, moves (a
  line added or removed above a function moves it and the next three, their code unchanged),
  input changes and incomplete records, a test not run must pass now and must have a past pass
  at the same versions and positions of what it executed now and the same input. Over
  synthetic single-pass histories, a test runs exactly when a function it executed (its code
  or its positions), the unlogged code, or an input it read changed: never skipped when its
  executed set meets the changed set, and never run when it doesn't; moves of functions it did
  not execute keep its pass. A `Selector` kept across the edits (and one
  made afresh half-way, as after a restart) gives every test, at every step, the choice
  `choose` gives over the same store, with other attributes, outside tests and runs of all
  mixed in.
- `api.rs`: a cached result's round trip through `blaze_api::Envelope<Response::TestResult>`
  in deterministic CBOR, with its outcome, duration and basis.

## State of the art

- **Bazel and Pants** cache a test target's result under the hash of every input of its
  action: the test binary and its runfiles. Any change to the binary (any function in any
  linked library) reruns every test in the target; nothing below the target is tracked. Pants
  does the same per test file with its dependency inference. Both run all tests in a target
  together, and neither sees runtime reads that aren't declared inputs.
- **Microsoft Test Impact Analysis** (Azure DevOps) maps tests to the source *files* they
  touched in an instrumented run, by coverage. A file edit reruns every test that touched any
  line of the file; it is described as safe only for managed code, and falls back to running
  everything when the map is missing or stale.
- **Ekstazi** (Gligoric, Eloussi and Marinov, ISSTA 2015) records the classes each test
  touched and their checksums, and reruns a test when any of them changed. It is class-level,
  so one method edit reruns every test that loaded the class, and it misses changes outside
  the JVM (native code, files read).
- **Facebook's predictive test selection** (Machalica et al., ICSE-SEIP 2019) trains a model
  on change features and past failures to pick likely-failing tests. It catches over 95% of
  faulty changes while running a third of the tests: unsound by design, every miss is a
  regression shipped.

blaze-testsel is function-level, not file- or class-level: it reruns only what executed the
changed code, and code a test merely reaches or loads doesn't count. Its keys are code
hashes, not timestamps or line coverage: a revert or a formatting change keeps the pass, and
going back to code within 16 runs finds its pass. Through `Bound`, it sees which function a
call binds to, which file- or class-level checksums only see as a whole. Runtime reads come
from the sandbox, so files and environment a test reads are in its key without being
declared. It is exact rather than predictive: no learned model, no miss rate. When it can't
be exact (an incomplete record), it says so and uses the reached set, which is still sound.

## Speed of light

Nothing here was run: costs are counted from the code and the machine, as in
[share.md](share.md#speed-of-light) and
[speed-of-light-infer-relate.md](../db/speed-of-light-infer-relate.md), whose machine this
uses. That is an Apple M5 Max performance core at about 4.5 GHz with sustained IPC about 3.5,
L1 about 3 cycles, L2 about 16, DRAM about 100 ns, and about 25 GB/s of DRAM per core.

There are two hot paths: choosing for a suite after an edit, and keeping how a test ran. A
suite is chosen for on one core, test after test. Its working set is the binary's table
(\(F\) rows of 64 bytes), the sets the suite's passes were kept under, and the histories.
Store reads are in-memory probes: the store verifies a value's hash once, when it is
recorded.

The example used below: \(F = 100\,000\) functions, of which \(U = 20\,000\) are called not
through a slot. \(T = 2\,000\) tests, each executing \(E = 1\,500\) functions. An edit changes
\(D = 3\) functions, whose old versions are in 600 kept sets each, so \(P_D = 1\,800\)
postings. \(C = 1\,900\) tests stay cached, each having read \(R = 3\) inputs.

**The work that can't be skipped.**

- **Per relink.** Read the new table once (\(F\) rows). Join it to what the last selection
  knew, by function identity: codegen's table order is not stable, so that is one hash probe
  per function. Put the unlogged versions in identity order and hash them, because the
  unlogged hash is part of every set's key.
- **Per edit.** Every kept set that holds a changed function must be marked unfit: one
  posting per (changed version, set holding it).
- **Per test.** Answer it: find its history, hash its attributes, read one fit.
- **Per cached test.** Check that its pass is kept for the inputs as they are now: what makes
  a pass exact is its inputs' values now.
- **Per kept run.**
  - Scan the record region (a byte per slot).
  - Put the executed functions in identity order, since a set is stored by content.
  - Write and hash the set's 64-byte rows.
  - Do a constant amount of store work: the pass, its shape and the history entry.

Unit costs:

| step | cost |
|---|---|
| read one 64-byte table row, streaming | about 2.6 ns |
| BLAKE3 of 64 bytes inside a long stream, many chunks at once (about 6 GB/s) | about 11 ns |
| BLAKE3 of a short key (history slot, identity, index slot, result key), `Hasher::new` included | about 30 ns |
| a store get: one probe of the in-memory store | about 30 ns |
| a probe or insert into an open-addressed table in L2 | about 5 ns |
| one radix-sort element (eight passes over an `(u64, u32)` pair) | about 8 ns |
| one posting: a `u32` from a sequential 64-byte block, an add to a counter in L2 | about 2 ns |
| an input's value now (a file's content ID from the daemon's file state) | about 30 ns |
| a kept run's set row: gather it, order it (bitset or radix over `u32` ranks), copy it | about 7 ns |

**The bound.** Choosing for a suite after an edit costs

\[ T_{\text{choose}} \approx F\,(c_{\text{row}} + c_{\text{probe}}) + U\,(c_{\text{blake}} + c_{\text{sort}}) + P_D\,c_{\text{post}} + T\,c_{\text{test}} + C\,(c_{\text{find}} + R\,c_{\text{in}}), \]

where \(c_{\text{test}} \approx 5 + 30 + 5 = 40\) ns (the memo's probe, the attributes' hash,
the fit). \(c_{\text{find}} \approx 3 \cdot 30 + 2 \cdot 30 + 30 = 180\) ns: three short
hashes (the identity, the index slot and the result key), two store gets, and the decode of
the inputs' shape. For the example:

\[ 100\,000 \cdot 7.6 + 20\,000 \cdot 19 + 1\,800 \cdot 2 + 2\,000 \cdot 40 + 1\,900 \cdot (180 + 90)\ \text{ns} \approx 0.76 + 0.38 + 0.004 + 0.08 + 0.51 \approx 1.7\ \text{ms}. \]

The edit itself is the \(P_D\) term, 3.6 µs. The rest is per relink and per test. Keeping a
run costs

\[ T_{\text{keep}} \approx \tfrac{F_s}{25\ \text{GB/s}} + E\,(c_{\text{row,set}} + c_{\text{blake}}) + 10 \cdot 30\ \text{ns}, \]

with \(F_s\) slot bytes: \(4 + 1\,500 \cdot 18\ \text{ns} + 0.3 \approx 31\ \mu s\) for the
example, small beside the run it records.

**What the code did.** Every test was chosen from the store alone (`select::choose`), at
about 8.3 µs a test for the example:

- doubling then halving to its history's end, which is \(2 \log_2 h + 1\) store gets, each
  behind a short hash (about 0.5 µs at \(h = 16\));
- its entry;
- the set's bytes, with every one of the set's functions looked up in the binary
  (`Image::changed`, \(1\,500 \cdot 5\) ns);
- then `find`.

For the whole suite that is \(\sum_t |S_t|\) probes, about 16.6 ms, after any edit, however
small. `Image::new` sorted all \(F\) rows by comparison (about \(17\) compares of 64-byte rows
each, 6.8 ms) and hashed the unlogged versions 32 bytes per call (one compression at a time,
about 20 ns a row). Building a kept set sorted its 64-byte rows by comparison, about
\(E \log_2 E \cdot 5\) ns, or 82 µs. In all, about 24 ms a selection, 14 times the bound, and
110 µs a keep, 3.5 times.

**What the code does.**

- **[`Selector`](../../../../../crates/blaze-testsel/src/selector.rs)** is an inverted index
  held across edits for one suite.
  - Every set it has read gets a dense id with a stale count: how many of its versions the
    binary lacks.
  - Every version gets its postings, in 64-byte blocks of 15 set ids.
  - Every test keeps a memo of its history.
  - `advance` probes each known function's bound in the new binary. For the few that changed,
    it walks the postings of the old and new version, moving stale counts by one.
  - `choose` is the reference walk over the memo, with a set's fit read as two loads. The
    explanation of a changed set is made only for a test that runs. The store is asked only
    about inputs (`find`) and what the memo has not read.
  - The simulation checks it against `select::choose` choice by choice.
- **`Image::new`** orders identities with a radix sort of their 8-byte prefixes (none for a
  table already in identity order), and keeps the order and each function's rank. It hashes
  the unlogged rows 256 at a time, in whole BLAKE3 chunks, over the same bytes as before.
- **Kept sets** are built from ranks: a bitset over the table when the set is at least a
  quarter of its words, else a radix sort of `u32` ranks. Then the rows are gathered in that
  order.

For the example:

- `Image::new` is \(F\,(2.6 + 5 + 8 + 1) + U \cdot 11 \approx 1.9\) ms;
- `advance` is at most \(F \cdot 5\) ns, about 0.5 ms, plus the 3.6 µs of postings;
- `choose` over the suite is \(T \cdot 40 + C \cdot 270\) ns, about 0.59 ms.

That is about 3.0 ms, 1.7 times the bound. The edit-proportional and per-test terms are at it.
The 1.3 ms over the bound is all per relink:

- the radix sort runs over all \(F\) functions, not only the \(U\) unlogged ones, so that kept
  sets can be built from ranks;
- the function join costs two hash operations, `Image`'s map and the selector's probe, where
  the bound has one.

A keep is \(4 + 1\,500 \cdot 18 + 1.6 + 0.3 \approx 33\ \mu s\), 5% over the bound: the
bitset's scan.

After a restart, the first selection reads each history's end and each set once, as
`choose` does. After that it reads nothing again while the selector lives.
`advancing_allocates_nothing` holds `advance` to no allocation, with an edit or without. The
selector allocates only as it meets new tests, sets, versions and history entries.

**What would close the rest.**

- A table that codegen writes in identity order (`fastcompile-sol-select 2`). It drops the
  sort to the one compare a row that `by_identity` already takes, and lets the join become a
  merge at streaming speed.
- The cached-test term is the largest per-test cost, and it is per cached test, not per
  changed input. An input change feed from the daemon's file state would make it
  proportional to the inputs that changed, as the postings make the code term.

**State of the art.**

- **Bazel and Pants** answer a test target with one action-cache lookup, \(O(T)\) per build.
  They are as fast per target, but a target is all its tests at once.
- **Ekstazi** checks, per test, the checksum of every class it depends on, with class
  checksums cached by file time. That is the \(\sum_t |S_t|\) walk the reference does, at
  class granularity.
- **Microsoft TIA** keeps an inverted map from source files to tests. That is the same shape
  as the postings here, at file granularity, and it is the shape search engines use:
  Lucene's postings in blocks of 128 with skip data, walked only for the terms queried.

The selector matches the inverted-index bound, \(O(P_D)\) for the edit, and does so per
function: of these systems, it is the only one exact at that granularity.
