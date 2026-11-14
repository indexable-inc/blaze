---
status: plan
summary: blaze test reruns only the tests that can have changed: a test's result is an outside record keyed by its identity, its asks are the functions it executed and what it read outside, a suite index finds the tests an edit touches, and a no-op is answered by the certificate. One flag runs everything.
date: 2026-11-13
---

# Test selection

The owner, 2026-11-13: "blaze test only reruns tests that have changed". This page is that rule,
exact: a test runs again when, and only when, something it executed or read is not as it was.
Built in `crates/blaze-testsel` (`ask`, `index`, `machine`) on blaze-keyed's outside form
(`crates/blaze-keyed/src/outside.rs`), and driven for every `blaze test` by the daemon's runner
(`crates/blaze-cargo/src/serve/unit/test.rs` through `blaze_testsel::host`). This page turns
current when `blaze test` run twice on a small crate reports every test `(cached)`.

## 1. The default

`blaze test` reruns only the tests that can have changed. `blaze test --run-all` runs every test
asked (and still files what it learns). `--reached` stops being the opt-in: selection is the
default and needs no flag; a test whose execution record is not whole is kept under what it
reaches, which is the same rule with a coarser set (section 3).

A cached test is a pass, and reads as one everywhere:

- libtest text: `test parse::empty ... ok <0.012s> (cached)`. The time is the last real run's.
  The summary counts it as passed: `test result: ok. 41 passed (38 cached); 0 failed; ...`.
- JSON (`blaze-test-result`, docs/cargo.md "Which tests run"): `"outcome": "passed"`,
  `"millis": 0` (nothing ran now), and `"cached": {"basis": "executed" | "reached", "why":
  "unchanged" | "answers" | "certificate", "millis": 12, "outcome": "passed"}`. A tool that reads
  only `outcome` sees a pass.

A test that ran and is not kept says why, by name: `test net::fetch ... ok <0.300s> (not kept:
it read the wall clock)`, and in JSON `"kept": {"no": "clock"}`.

## 2. Soundness: what a kept result covers

The invariant: a test is reported cached only if rerunning it now would give the same result.
A kept result covers its asks (section 3) and nothing else, so everything a test can observe must
be an ask, or the result is not kept. Each case, decided:

| What the test touches | Decision | Why |
|---|---|---|
| The wall clock (`SystemTime`, `Instant` through a traced call) | Not kept (`clock`) | Its value is never equal twice; an ask on it would never hit. |
| The vDSO or commpage clock, `rdtsc`, raw `syscall` the tracer cannot see | Not kept (`foreign`), once the program marks code that does it | A test that executes such code cannot be traced. The mark (a flag on the instance record for inline asm, a raw syscall or a call into foreign code) is missing today; until it exists, these reads are a known hole, listed in docs/v2/plan/design/trust.md's sense. |
| Randomness | Kept when the provider answered it | The sandbox answers `getrandom` and `/dev/urandom` from a seed derived from the test's base key, so it is an input of the key, not of the world. Randomness it did not answer (`Untraceable::Random`) is not kept. |
| The network | Not kept (`network`), except loopback inside the test's own network namespace | A peer outside the sandbox is a read nobody can answer again. Loopback to a server the test started itself is the test's own code. |
| Child processes | Kept when traced | Their file, directory and environment reads are traced into the test's (Linux: the tracer follows forks). A child the tracer cannot follow (a macOS protected binary) makes the run `descriptor`: not kept. |
| Writes outside its own temporary directory | Not kept (`wrote outside`) | A write that outlives the test is an effect another test or run can read. Needs a private overlay of the home and the workspace so the sandbox can say "it wrote outside"; missing today. |
| A file read through a descriptor the sandbox did not see opened | Not kept (`descriptor`) | The trace's incomplete (X) lines. |
| Not sandboxed at all | Not kept (`unsandboxed`) | Tests always run sandboxed (target.md); a run that was not is never kept. |

A test that cannot be traced is "not kept", by name and reason, every time it runs. It is never
an error and never silently rerun under another rule.

Failures are not kept: a failing test runs every time it is asked, so a fix to the world outside
the program (a file restored, a service back) is seen. A failure is never reported as a cached
pass. An undone edit after a failure still finds the older pass by its address (section 3),
because passes are filed by what they asked and answered, not by "the last result".

Nondeterminism the asks do not cover (a data race, a timing-dependent test) is the test's own: a
test that passes and fails on equal asks is flaky by Rust's semantics too, and the queue's
retries (`attempts`) report it as flaky; the kept pass is a pass that happened on those asks.

## 3. Exactness: the outside form

A test result is blaze-keyed's `TestRun`, an outside kind (kind version 2):

- **The base** is the test's identity: the toolchain, the unit's settings, the environment blaze
  declares to tests, the entry (item and type arguments), its libtest attributes (name, ignore,
  should_panic) and its arguments. Nothing the test's code is: `TestInputs.reach` leaves the key.
  The reach digest stays in blaze-program (`reach::reach_digest`) for the reached basis and for
  blaze-mutants' verdict keys; as a key it made every reachable edit a miss, which is the opposite
  of exact.
- **The asks** (the shape, `ask::write_shape`): the functions the test executed, each by identity
  (`FnId`), plus `UNLOGGED` (all code not called through a slot), plus each outside read (a file,
  a directory listing, an environment variable) by name. Sorted, each once, with the basis byte
  (executed, or reached when the execution record was not whole).
- **The answers**: each function's bound code now (`Bound`: its code, what its refs bind to,
  its statics' values and the spans it names; absent when gone), each read's digest now.
- **The value** at the address of (base, shape, answers): a pass marker and the basis, with the
  asks and answers as facts for explain. It is a function of its address, so filing it twice is
  `Put::Same`; output and duration are not in it (the duration lives in the index row).

An edit to a function changes the answer of exactly the tests that asked it. An undone edit gives
the old answers, so the old address, so the old value: it reruns nothing. A test with several
behaviours (a read decides what else runs) has several shapes, up to `MAX_SHAPES`, past which it
is not kept (`too many shapes`).

## 4. The floor, and the cost against it

The floor of an edit to F: rerun the tests that executed F, plus the I/O nobody avoids (read the
index, write what changed). The floor of a no-op: one record read.

The suite index makes the shortcut: a head record (`TestIndex`, filed by content, the suite's
current one a root the host keeps) with one fixed-width row per kept test (base, address, shape
digest, last run's micros, basis) sorted by base, the read table (every read any row asks, with
its answer when the head was filed), and the program state the rows are valid at; and a postings
record (`TestPostings`) from each ask to the tests that asked it. A row whose asks changed since
the postings were written carries its asks inline in the head (the delta); when the delta passes
half the postings, both are rewritten (compaction).

| Operation | This design | Floor | Ratio |
|---|---|---|---|
| No-op | One certificate lookup (its shape includes the read table), one head read | One record read | 1 to 2 reads |
| Edit to F, no certificate | Head + postings read; the program's changed set C; one answer per read in the table; per changed ask a binary search of the postings; per candidate one outside lookup (one slot read, one answer of its asks, one value read); per test that misses a run | The runs of the tests that executed F | The runs are the floor; the rest is O(\|C\| log P + candidates x asks + reads) |
| Writing the result | The head again, O(kept tests) bytes; the postings only at compaction (amortized O(1) per changed ask) | The rows that changed | Above the floor by the head copy: 116 bytes per kept test (1.2 MB at 10k tests). Said plainly: content-defined chunks of the head, as the program manifest has, would bring it to the floor and are not built. |
| Undone edit | As an edit; every candidate hits by address | Nothing runs | Nothing runs |

The machine holds per test a small state (its shape, its answers) and, while a lookup is open,
the lookup's 2 KiB slot digests; nothing is per function or per row beyond one flag per head row.

## 5. Home

Selection lives in `crates/blaze-testsel`:

- `ask`: the shape (the asks as bytes, canonical) and the value payload.
- `index`: the head and postings layouts, read in place.
- `machine`: `Selection`, the sans-IO machine. Inputs: the tests asked (by base and name), the
  program state, the head key and, for a no-op, the certificate. Effects: get a record, the
  program's changed functions since a state, answer a shape's asks now, run a test, file a
  record. Outcome per test: cached (by the certificate, unchanged asks, or answers), ran and kept,
  or not kept with why.
- `table` (kept): `FnId`, `Bound`, `Image` and the unlogged hash.
- `record` (kept): what a test process says it executed.

Deleted with the wiring: `select` (`choose`/`keep` over `blaze_usercode::test::Passes`, the
history slots and the 16-pass lookback), `selector` (the in-memory postings memo), `api`'s
re-exports and its `Unreached` mapping (a cached pass is `passed` with `cached`), and
blaze-usercode's `Passes`-based selection. blaze-program's `TestRecord`, `OutsideRead`,
`write_test`, `read_test` and its `ABSENT` are gone (one concept, one home).

What the runner gives the machine (`blaze_testsel::host::Driver`): per suite, the tests asked
by base (`host::base` over the test's id) and name; the program now and the image the suite
was last filed at (`Program{now, before}`, held in the daemon's `image::Selectors`), from which
the changed functions are read by `FnId`; the head the suite last wrote; and,
per run, the bound code of each executed function (slots mapped through `Image`), the traced
reads as names and digests, whether the run was sandboxed, whether the trace exists, and whether
codegen marked the test as reaching code the tracer cannot see (`foreign`).

Not given yet, so a pass is refused or costs more rather than kept wrongly: writes outside the
sandbox's private overlay (not built, so a test that writes outside is not refused for it);
a raw syscall or the vDSO as `foreign` (codegen marks only what it sees).

The head outlives the daemon. Each head a run files is a journal mark per suite
(`serve/local.rs` `TestHeadMark`: the suite's settings, the head and postings keys, and the
program state its rows hold at), written by `Jobs::test_heads` only once every put of the
suite's runs since its last mark is durable, so a crash leaves no mark, the old one, or the new
one with its head. The store pin `test heads` (`collect::TEST_HEADS`) keeps each marked head and
its postings, moved from the old head to the new as the mark is, and queued again at each start
for every mark. A start hands the marks to the runner (`Selectors::restore`) with no image: the
head is used for a binary of its state, and another binary's is a miss by key (each test looked
up by its answers), never `UnknownState`. A head the store does not hold for all that
(`MissingIndex`) fails that one run with an error naming the suite and the key, once: the head is
not given back and its mark is let go, so the next run starts with no head.

The queue orders each test by its head row's last run (`Selection::last_micros`); a test with no
row has no expected duration. File digests are taken when the trace is assembled, not at the
moment each file was read: a pass is kept only when nothing it read (each name the answer rests
on, and each directory above it) changed since a moment before the run began
(`blaze_usercode::inputs::Began::moved`, git's racy window), else it is not kept,
`NotKept::Moved`; what changes after the assembly the settle's second answers find. Doc tests
are neither selected nor kept: the CLI runs them after the daemon's results, every time, and
says so.
