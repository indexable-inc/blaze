---
status: plan
summary: Why blaze recomputed each step of a build, said after every build and by `blaze why`, as a chain from the step through the key parts that differ down to a real input change (a file, a variable, a flag) or one of a closed set of other terminal reasons; an unexplained recompute is a blaze bug and is counted.
date: 2026-11-10
---
# Why a build recomputed what it did

Lane why-always (2026-11-10), over lane why-miss (2026-11-02). A warm build that recomputes
anything did so for a reason, and blaze says it: after every build, in a few lines grouped by
root cause, and in full with `blaze why`. A reason is a chain that ends at an input that really
changed, never at a hash and never at "its key changed".

```
$ blaze build
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.61s
note: recomputed 3 steps because crates/core/main.rs changed  [why: input]
  ┌ codegen dev template: main, rg::main
  └ unit: ripgrep v15.1.0 (bin "rg")
```

## The model

1. **A step has an identity that does not depend on its key**: which step, of which item
   (`blaze_kernel::Miss::step` and `subject`). A unit is its workspace root, package, target,
   mode, platform, profile and whether it is std's or checked as a test. A codegen dev template
   is its function (identity, which instance, whose code it is) and its codegen settings, and
   nothing of the unit that happened to ask for it: the template store is shared by every unit
   and workspace, and so is its record. A build script's run is its run unit.
2. **A key is named parts** (`blaze_kernel::Part`), each one of three kinds (`PartKind`):
   - `Input`: something the build is given: a variable, a flag, a setting, the toolchain, the
     target, the code identity of blaze itself.
   - `File`: the bytes of a file, named by its absolute path. A Rust source file of a
     workspace crate also carries its **code text** hash (`Part::code`, blaze-lower's
     `code_text`: the text with every comment no macro can see replaced by its shape), so a
     change of bytes that leaves the code text alone is known to have changed no item.
   - `Result`: the value of another step, named by that step's item (`Part::names`) where it
     has one.
   The parts are recorded when the step is computed (on a miss only, never on a hit), in the
   ledger, under the step's identity.
3. **On a recompute the reason is the diff** of the parts against the last record of the same
   identity (`RebuildCause`, unchanged from lane why-miss: `Parts`, `First`, `Lost`, ...).
4. **The chain ends in terminal reasons** (`blaze_api::Reason`, a closed set; `Rebuild::ends`),
   worked out by `Ledger::explain` for every recomputed step:

   | terminal | when |
   |---|---|
   | `InputChanged { file, input, trivia }` | an `Input` or `File` part differs. `trivia`: a file whose bytes changed and whose code text did not (comments only): no item changed, so the recompute is wasted work and a bug in the key that held the bytes |
   | `FirstTime` | no record of the identity, ever |
   | `Evicted { by: Memory, Age or Cap, when }` | it was computed before and the result or its record was dropped; the dropper recorded that it did |
   | `PreviousFailed { since }` | its key is the one of its last compute, which was in a build that failed or was stopped and kept nothing |
   | `Forced { how }` | computed because it was asked for: a build script named in `unsandboxed-build-scripts` runs every build |
   | `KeyUnstable { part }` | a `Result` part differs though the step it names was not computed in between: the same inputs gave another value, a determinism bug in blaze. Defined and printed, not yet produced: until hits are recorded it cannot be told from a reuse under an older key, so that case is `Unexplained` |
   | `Unexplained { what }` | anything else. A blaze bug: printed as one, counted (a `blaze` warning event a build, with the count, where the profile's events see it), and the fixtures assert there are none |

   A differing `Result` part is followed (`serve/why.rs`, `Ends`):
   - the step it names was recomputed in this build: its ends are this step's too;
   - it was not, and the ledger's records of that item are from a build after this step's last
     compute: their recorded ends (`Last::ends`) are this step's (it changed in a build this
     step took no part in);
   - it was not, and every record of it is older: `Unexplained`, saying so (`KeyUnstable`
     once hits are recorded);
   - it names no step (a dev template's `its code`): the unit that computed it
     (`Miss::via`) is followed, since a unit's key covers, deeply, every source its functions'
     code is made from;
   - nothing to follow: `Unexplained`, naming the part.
   The walk visits each step of the build once (bounded by the build's recomputes), and a
   step keeps at most 8 ends (`ENDS`), the first in order.
5. **What is not a real reason is `Unexplained`**, each naming what is missing: `Lost` and
   `Collected` (nothing is kept under the unchanged key, and whoever dropped it did not
   record doing so: the store's eviction does not tell the ledger yet); `Beyond` for a unit
   (the values of its traced variables and proc-macro reads are not kept); `Unnamed` and
   `Twice` (the key or the identity is coarser than it says); `Always` for a unit whose front
   end can't say its inputs; `Unrecorded`, `Unshaped`, `NotExplained`, `Failed`.

## What each step's parts are

- **unit** (`serve/unit/compile.rs`, `RecordInputs::parts`): each field of the record key as
  an `Input`, each dependency by the name it is used under, and, in place of the one part
  "the crate's identity in the front end", what that identity is made of
  (`blaze_lower::Lowered::identity_parts`, the same values `identity_of` hashes): the crate's
  own settings (root path, cfgs, env, edition, target) as an `Input`; each file it is made
  from as a `File` (its root file alone for a crate that reads no other; else every file of
  its source root, with the code text of a workspace crate's Rust files); how cargo names its
  root; and each crate it depends on as a `Result` naming that crate's unit. The parts are
  refused unless inputs gathered again make the very key the record was looked up under. A
  source root past 4096 files (`FILES_NAMED`) names its first 4096 by path and the rest as
  one part.
- **codegen dev template** (`blaze-codegen` driver): the toolchain and settings (`Input`);
  its code, its type arguments' facts and the kinds of its references (`Result`, of no
  recorded step: followed through `Miss::via`); each body built in (`Result`, by path).
- **build script run** (`serve/unit/script.rs`): the script's code (`Result`), each part of
  `Given` (`Input`), and once the run ends each read it traced (a file as a `File`, a
  variable, a directory's names; past 1024 the rest are one part).
- **A step the kernel computes** (`Kernel::why`): the first read that changed; none is in
  production yet.
- **Not given back yet**: proc-macro expansion, link, test selection (see "Not yet").

## Where it is kept

- **Ledger** (`why.ledger` in the history's directory; `serve/why.rs` the sans-IO core,
  `serve/why/file.rs` its driver on the `blaze-history` thread): per identity, its last
  compute: key, parts (name, hash, kind, code text hash, the item named), the item's name, the
  build, whether that build ended well, and the ends worked out then. One framed
  deterministic-CBOR record per build; magic and version as before (`NAMING` 6), an older
  file is moved aside.
- **The cap never turns a reason into `FirstTime`.** Past `CAP_BYTES` (64 MiB) a compaction
  shrinks the identities computed longest ago to **tombstones**: identity, build and item,
  no parts (about 100 bytes plus the name, against 0.5 to 100 KiB whole). A miss of a
  tombstoned identity is `Evicted { by: Cap, when }`. The latest record of every identity is
  kept, whole or as a tombstone, up to `TOMBS` (1,000,000) tombstones; only past that are the
  oldest tombstones removed, which is counted and said (`Rebuilt::dropped`), since their next
  miss then does read `FirstTime`.
- **History** (`misses` table): one row per recomputed step, as before; `cause` is the diff's
  kind. `key_parts` holds each miss's parts.
- **Restore, append, gap, compaction, "answered before durable"**: unchanged from lane
  why-miss and documented in `serve/why/file.rs`: a torn tail is copied aside and marks a gap;
  a failed append marks `why.ledger.gap`; a record from before a gap is compared with nothing
  (`NotExplained`, so `Unexplained`, naming both builds); another version's file is moved
  aside, never misread.

## What is printed

**After every build that recomputed something** (`blaze build`, `check`, `test`, `run`'s
build; not yet the live test view, `cli/live.rs`), once `Finished` is out: the CLI asks the daemon for the build's summary
(`Request::Rebuilt { build: Some(id), .. }`, `Rebuilt::summary`) and draws each report with
the one renderer (docs/v2/plan/design/structured-output.md). Under `--message-format=json`
each is a `{"reason":"blaze-report","report":{...}}` line. Nothing is said by a build that
recomputed nothing, under `-q`, or when no daemon ran the build. A build that recomputed
nothing says so in its `Work` (`Worked::misses` of 0, the count of the misses the summary
is made of), and its CLI then asks nothing: a no-op build makes no second round trip.

One `Report` a root cause (the set of ends a step has), the most steps first, at most 4
(`CAUSES`); past that one line counts the rest and names `blaze why`. Each has a fact a step
kind, most first, naming at most 3 items and counting the rest:

| root cause | severity, rule | headline |
|---|---|---|
| input | note, `why:input` | `recomputed N steps because PATH changed` (or `` `env CC` changed``; inputs joined by `and`; past 3, a count) |
| trivia only | warning, `why:wasted` | `recomputed N steps because PATH changed, though no item in it did (comments only): wasted work, a bug in blaze's keys` |
| first time | note, `why:first` | `computed N steps for the first time`, no facts: a cold build is one line |
| evicted | note, `why:evicted` | `recomputed N steps because blaze why's ledger dropped its records of them to stay under its cap` |
| previous failed | note, `why:failed` | `recomputed N steps because the last compute of each failed or was stopped` |
| forced | note, `why:forced` | `recomputed N steps because that was asked for: HOW` |
| key unstable | warning, `why:unstable` | ``recomputed N steps because `PART` came out different from the same inputs (a determinism bug in blaze)`` |
| unexplained | warning, `why:unexplained` | ``N steps recomputed for no recorded reason: a blaze bug (`blaze why --unexplained`)`` |

A cause with several kinds of end joins their clauses with `and`. The summary is a function of the
build's explained misses alone: sorted, no clock, no map order (`deterministic-output`).

**`blaze why`**: the groups by step and diff, as before, each item's chain with `blaze why
ITEM`, `--all`, `--json`. **`blaze why --unexplained`** lists only the steps with an
`Unexplained` end, each with what is missing.

## Cost

- **Recording**, paid on a miss only, by the worker that missed, before it computes: the
  parts hashed one by one. A unit: its record inputs gathered again (as before) and one
  content ID read per file of its crate (already held by the front end) plus the code text of
  each workspace Rust file (a memoized query over a parse the compile needs anyway). A dev
  template: no new hashing.
- **Bytes per step recorded** (`weight`): 160 + the step's and item's names + 56 a part + each
  part's name: about 0.5 KiB a dev template, 3 KiB plus 100 B a file for a unit.
- **Explaining and the summary**: on the `blaze-history` thread, after the build's `Done` has
  been sent; linear in the build's misses and their ends. The client's one extra request
  after `Finished` waits for that thread (bounded by `WHY_WAIT`, 30 s; milliseconds
  expected).
- None of this is measured yet: the numbers above are from the encoding, not a profile. The
  check is `blaze slow` on ripgrep before and after, quoted from the spans (`explain a
  build's misses`, `unit record lookup`).

## Tests

Invariants (`serve/why.rs` tests, over a model of ripgrep's three crates and every way their
files can be left, commented or edited; each with its planted-bug control, `Plant`):
- every recomputed step of a build has ends, none `Unexplained`
  (`the_oracle_catches_a_result_part_not_followed`);
- an `InputChanged` end names a file the two builds really differ in, as trivia exactly when
  its code text is the same (`the_oracle_catches_an_unchanged_part_blamed`). It holds for a
  build against the one before it; with a reuse in between (A, B, A reused, C) the diff is
  against B, which is the "hits are not tracked" gap below;
- the summary is the same whatever order the misses arrive in
  (`the_oracle_catches_ends_in_the_order_reached`).

Fixtures: each terminal reason that is produced (all but `KeyUnstable` and an eviction by
memory or age); the three edits of ripgrep that started this lane (a body edit, a comment in
the same file, a comment in a leaf crate), asserting the text names the file and, for the
comments, says no item changed. These run on the model, with the producers' misses built by
hand: the producers themselves (`identity_parts`, the driver's parts) have no test yet.

## Not yet

- **Items within a file.** A `File` end says the file and whether any item changed; it does
  not yet name the items and which of their hashes moved (interface, layout, code). The item
  layer (`blaze-item-hash`) has them per body; recording them per unit costs a query a body.
- **Evictions by memory and age.** The store's eviction does not tell the ledger, so a
  dropped result reads `Unexplained` (`Lost`), which is the bug it is. `blaze clean` likewise
  is not yet `Forced`.
- **Steps with no parts**: link (`LinkHow::Full { why }`), test selection's `Why`,
  proc-macro expansion (no call-site identity), build script compile as its own step, and
  front-end queries (the `provenance` table has them, the chain does not follow into them).
- The values behind `Beyond` for a unit (its traced variables and proc-macro reads).
- A unit that fails before its store phase gives back no misses.
- Hits are not tracked: after A, B, A (reused), C, C is compared with B.
- Frames have no checksum: a bit-rotted length in a middle record reads as a torn tail.
- `--all` after a cold build is not paged.
- A count of unexplained steps as a column of the history (`builds` or `misses`), so `blaze
  slow` and SQL can gate on it; today it is an event.
- The summary in the live test view (`cli/live.rs`).
- A dev template computed under two keys for one function and settings in one build (two
  units whose builds of a shared generic differ in what is built in) is `Twice`, so
  `Unexplained`: whether that happens on real projects is not known.
- The end-to-end path on a real project (ripgrep, the three edits) has not been run on this
  lane's tree, and nothing here has been compiled.
