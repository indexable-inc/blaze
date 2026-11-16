---
status: plan
summary: The driver of one command on the new item layer (crates/blaze-drive, crates/blaze-cli): a sans-IO request machine whose effects are store gets and puts, task runs and printing; the certificate first; tasks keyed from text that return their record or Need; one flight per key; validity by read list, checked forward from what changed through a reverse index kept in the answer pages, with no dependency graph in memory; dependencies as records only; run handed to blaze-run's Parent; findings placed when printed; a profile on every command.
date: 2026-11-15
---
# The driver

bottom.md section 9 says what the driver is; this is how it is built. Two crates:
`crates/blaze-drive` (`#![no_std]`, no `alloc`, sans-IO: blaze-own's collections, the allocator
passed in) and `crates/blaze-cli` (the `blaze` binary, the one place that touches the system,
through bstd only).

## The request machine

`Request::new_in(config, ask, alloc)`, then `step(event, out)`, which pushes effects into a
caller-owned vector. The caller performs each effect and feeds back its event; it may perform them
in any order and in parallel. The machine never waits.

| Event | Effect it answers |
|---|---|
| `Start { files, gone, last }` | (first) |
| `Got { key, bytes }` | `Get { key }` |
| `Ran { batch, results: [Outcome { ticket, ran, in_place, ns }] }` | `Run { batch }` |
| `Want(want)` (the running program waits on a name) | `StartProgram { units }` |
| `Exited { ok }` | `StartProgram { units }`, `Stop { errors }` |

`ran` is `Done(outputs) | Need(wants) | Failed(why)`. Other effects need no answer: `Put`,
`Publish(input)` and `Unpublish(name)` (the settled table, below; performed after every `Put`
before them), `Print`, `WriteLast`, `Done(answer)`; the answer carries a `Verdict`.

1. **Start.** The cli has swept the checkout (below) and hands every file as (path, content id,
   changed since the last answer), the files gone since, and the per-checkout `last` file. The
   machine computes the **certificate key**: the tool, the command and its arguments, and every
   (path, content id). One `Get`.
2. **Certificate stands**: its record holds the findings, each with its file and item start.
   `Print` each, `Done`. That is the whole repeat: one store read.
3. **Miss.** Get the last answer's certificate (its findings stand for what this run does not
   redo) and the last answer's root page. Then choose the walk:
   - *Forward* when `last` names the same tool, the same command shape and the same manifests:
     seed one `FileItems` job per changed or gone file.
   - *Full* otherwise (first run, a manifest edit, a new tool): seed the `Units` job; every part
     of every record is a job. Old rows still short-cut every job whose key did not move.
4. **Jobs** (one per name) advance as data arrives; a job that lacks a page, a name or a home
   waits on it by name, never by blocking.
5. **Finish** when no effect is outstanding and every job is settled: update the reverse index,
   write the touched answer pages and the root, the new certificate, `WriteLast`, then `Print`
   the findings sorted by (path, position) and `Done`.

## Names, sources and keys

A **name** is what a task is about: `(kind, subject)`. Subjects: a file's is the hash of its path;
an item's is its item key (identity.md); `Units`' is fixed. A **source** is the text the task is
keyed by, known before the work:

- `FileItems(file)`: the file's content id and the edition (from the part that listed it).
- every item task: its part digest in its file's `FileItems` record (signature text, body text,
  the module's declarations).
- `Fetch(crate)`'s parts carry the record key itself (a crate with no text here).

**Key** = H(tool, kind, subject, source). A key never holds another record's key (bottom.md 3);
what a task read of another record is its read list: (name, out digest) pairs.

Task kinds: `Units` (the workspace's crates: manifests' content), `Fetch(crate)`,
`FileItems(file)`, `ModuleScope(module)`, `Group(modules)` (a cycle of scopes, one task),
`Expansion(call)`, `Signature(item)`, `ImplBucket(bucket)`, `BodyCheck(body)`,
`Code(function, tier, hooks)`.

## Records and validity

The drive's envelope is one buffer: name, source, **out digest** (what a reader sees of it), read
list, parts (name, how to locate it, its start in the file), findings (rule, item-relative range,
message), and the kind's own payload (blaze-surface's record bytes, a checked body, code).

- A record with no reads is stored **whole** at its key.
- A record with reads is stored at its **value key** H(key, reads' digests), and a **shape** at the
  key lists the read names, so a lookup that knows only the key reads the shape, computes the
  value key from today's digests and reads that. Deviation from blaze-keyed's shape trie: a second
  read set under one key is stored at its own value key and found by its row, not by the shape
  (first writer of the shape wins; rule 3 holds because a shape names reads, it never answers).

**The answer table** is the checkout's index, stored as content-addressed pages under a root:
row per name, sorted by name, paged by the name's first bits (the root holds the bit count).
A row holds the key, where its record is, the source, the out digest, its home and start, its
read list, its parts, and its **readers** (the reverse index). Rows are records; nothing of them
is kept in memory but the pages a run touches.

**Forward check.** A job whose key equals its row's key and is not dirtied stands with no store
read. When a job settles with an out digest unlike its row's, every reader (the row's readers and
this run's new readers) is dirtied: re-decided at its value key from today's digests, run if
absent. A reader taken as standing before its read settled is corrected the same way, since it is
posted as a reader. Every job settles at most `most_entries` times. Nothing else is visited.

**Store reads designed** (p = answer pages touched, k = records the re-run task reads):

| case | reads | tasks |
|---|---|---|
| repeat, nothing changed | 1 (the certificate) | 0 |
| one-body edit | 4 + p + 2 + k: certificate miss, last certificate, root, p pages, the file's new `FileItems` key, the body's new key, its k inputs | `FileItems` 1, `BodyCheck` 1 |
| manifest edit | Full walk: one read per page and per re-made record | the moved ones |

The leaf table is a file of the checkout, not a store read. Floor for the edit: the two task runs
and k reads; the extra 4 + p are a certificate, an index root and its pages (ratio 1.0 on tasks).

**The early record after a restart** (adapted from Relax's `check_reuse.rs`): the repeat path
loads nothing but the `last` file, the leaf table and the certificate. Its key is computed from
the leaf table's content ids, which the stat sweep proves current without reading a source.
Through the plain store (store-next.md) it reads, from a cold page cache: the head, the manifest,
one index page of the segment that holds the certificate (`locate`), the certificate's payload
by one `pread` (`get_into`, never a mapping of the segment), the leaf table (one page per 4 KiB:
one for hello-world, about 40 for reth's 4,000 files) and the slot file. **The floor to measure
against: 6 pages for hello-world, about 45 for reth** (store-next.md's count from the code, not
yet measured). Each segment probed before the one holding the certificate adds 1 or 2 index
pages (at most 16 segments). Sealed segments are proved by their row with no byte read, so after
an orderly exit nothing else is touched before the findings print.

## Tasks and Need

A task is a pure function of its inputs: `Done(outputs)` or `Need(wants)`. A want is a name and
its **home** (the record that lists it: its file's `FileItems`, or `Fetch(crate)`). The drive
makes what is wanted and enters the task again from the top with the inputs it lacked. Rules:

- A job's old read list is its **hints**: its inputs from the first entry.
- **One flight per key**: a name has one job; a job dirtied while running is redone after.
- A job entered more than `most_entries` times is `DriveError::Reentries` naming its name, key and
  last needs.
- A wait cycle among `ModuleScope` jobs becomes one `Group` task; any other cycle is
  `DriveError::NeedCycle` naming its names.
- Work is parallel in the cli's pool, in batches (below); the queue is ordered by class, then
  kind (files, scopes, signatures, bodies, code), then home.
- **Three classes** (adapted from Relax's `cpu_budget`): `Foreground` (what the command asked, or
  what a running program waits on), `Gate` (`run`'s check of its scope beside the program) and
  `Background` (speculative code, the optimizing tier). A lower class is emitted only when no
  higher one is queued, so the person's work never waits behind it; `workers` is the machine's
  (one code path, 128 cores to a Mac).
- An output for a name no job asked for, or a read whose digest is not today's, is a typed error.

## Dependencies and std

A crate with no text here is records only: its `Fetch(crate)` record lists its parts by record
key; a missing record is `DriveError::Missing { name, key, builder }`, the builder being
`Builder::Pack(crate)` (fetch blaze-pack's pack) or `Builder::PrepareStd` for the sysroot.
`blaze prepare-std` is the same request over `library/` with the sysroot crates as text crates:
their tasks run here once, filling the local store; later requests find them by key and never
read std's text. CI runs the same command and publishes the pack.

## Run

`run` hands the units record to the cli (`StartProgram { units }`), which is blaze-run's `Parent`
for the program's life: `Compile` and `Make` become drive `Code` names (`run::code_name`), sent
back as `Want`s and answered as tasks. Today `Code` answers `NotBuilt`, so `blaze run` stops with
that error, by name.

### The gate

`run` and `test` fail when the check of their program's scope fails, lints included (bottom.md
11): the scope is the workspace packages the program's package depends on, whole. Findings carry
no level (rule 81); `Config::is_error(rule)` is the command's policy. One seam, `Ask::gate`
(rule 78):

- **`Beside`** (default). A stored certificate is read first: an error in it refuses the start at
  that one read; a clean one starts the program at once. Otherwise the program starts when the
  units' record settles, and the walk's jobs run in class `Gate`, under what the program waits
  on (`Want` lifts a job to `Foreground`). The first error-level finding settled emits
  `Stop { errors }`. A program that exits first is not reported: its success is held
  (`Phase::Held`) until the walk is done, so a test's pass waits for the check.
- **`Before`**. The walk finishes, then the program starts if it is clean.
- **`Reached`**. Only what runs is checked; the answer comes at the exit with
  `Verdict::PassedReached`, printed "checked only what ran".

Verdicts: `Checked`, `Passed`, `PassedReached`, `Failed { errors, started }`, `ProgramFailed`.
A `Want` after the walk is done, for a name the walk did not settle, is `DriveError::LateWant`.
The profile records the time to the program's start and to the verdict (the caller's clock), and
the items the gate checked while the program ran (`checked_beside`). The simulation's oracle: no
`Passed` while work is out or before the answer is written; the plant `EarlySuccess` (success at
the exit) is caught. Open: whether the gate covers every configuration (below) or only the one
run; today the one run.

**Tests.** A test's result is a `Test(test)` record whose read list is the code identities its
run entered (the run side's entry marks). It is a job like any other: after an edit, a test whose
read list holds a changed function is dirtied by the forward check and runs again; every other
test's record stands and its verdict is printed marked as reused. The answer counts both
(`Answer::tests_run`, `tests_reused`). blaze-run's `Tests::affected` is the same postings as a
run-side index; the drive's readers column is the store form of it.

## Scheduling on many cores

**Arithmetic** (assumed costs, labelled; parallel.md A1-A5). One coordinator step costs about
1-2 µs; a task about 50 µs. One step per task caps one coordinator near 50/2 = 25 to 50 cores.
parallel.md's row: "With the pool alone and the coordinator and store lock left as they are, the
serial part is about 150k completions x 2 us = 0.3 s plus 400k x 0.5 us = 0.2 s plus the serial
sweep and the critical path, about 0.65 s in all: speedup about 17." This design removes each term:

1. **Batches.** `Run { batch }` carries up to `min(1 ms / the kind's mean time, the kind's queue /
   free workers, 256)` tasks of one class, kind and home (`TARGET_BATCH_NS`, `MOST_BATCH`); a
   worker answers them together in one `Ran`. Coordinator steps fall from one a task to one a
   millisecond of work a worker: 128 steps per ms at 2 µs is 26% of one thread, not 128 threads'
   worth. 150k completions become about 11k batches: 22 ms.
2. **Needs answered off the coordinator.** The settled table (`settled.rs`): one writer (the
   coordinator performs `Publish` after the `Put`s before it), many readers, a seqlock of 11
   atomic words a slot, open addressing, 32 probes, `TableFull` typed. A worker whose task answers
   `Need` takes each want from the table (`worker::answer_in_place`) and re-enters the task in
   place; only wants with no row reach the coordinator, which keeps the one-flight rule (a worker
   never starts a job). The table never decides correctness: every read is checked against the
   settled digest in `made()`, so a stale or colliding row costs a run. Expected entries per body:
   cold 2 (one `Need` of its signatures, one in place), 1 at the coordinator; warm 1 (hints).
   Counted: `in_place`, the `entries` histogram (0, 1, 2, 3, 4, 5-8, 9-16, more).
3. **Kind order cold**: files, scopes, signatures, bodies; a body's signatures are usually
   settled before it runs, so its `Need` is answered in place.
4. **Workers.** Each has its own deque of batches (owner LIFO, thieves take half, FIFO; Chase-Lev,
   Lê et al.), its own frame reset per task, and its own producer ring of `Ran` events to the
   coordinator. Shared structures a worker touches: the store (lock-free mapped lookups; puts in a
   per-worker `Batch`, one commit, parallel.md change 4), the settled table (reads only), the sweep
   (frozen). No interner: names are content ids. No lock on the hot path.
5. **The names' first fill**: `ModuleScope` jobs in rounds over all modules of a crate, a wait cycle
   as one `Group`; expected rounds = the longest import chain, about 3-5. The crate root is decoded
   once per fill round and shared (`scope_task::SharedRoot`, `fill_round`; parallel.md change 9).
6. **Classes and workers** as above; `workers` from `Host::cores()`.
7. **Profile**: tasks, batches, steps, in place, coordinator ns, idle ns, entries histogram.
   **Floor** (rule 67): total task work / N plus the serial part: the sweep's parallel share,
   `Units`, the critical path of `Need` rounds (A5: 20 ms), and the finish (pages, certificate).
   parallel.md's after-column: about 130 ms against 106 ms, ratio 1.2.

O(1) per event (parallel.md change 3): every map a step touches is `table.rs`'s open-addressed
table (content-id bits as the hash, probes counted); the ready set is one ring per (class, kind)
with generation-stamped entries, so leaving it is O(1); a settled counter replaces the scan; a job
registers at most one wait per advance, so no wait list is searched. `finish` sorts once and
merges (reverse index, pages bucketed in one pass, findings). The test steps 101,000 jobs and
holds bookkeeping per job and edge within 2x of a 1,000-job run (`bookkeeping()`). The pool
codes against bstd's `scope.must_overlap`, `workers()` and `PerWorker::take_all`; until they
land the cli runs batches inline on one thread.

## One check covers every configuration

Superseded by conditions.md when it lands (tokio's 21 root switches are 2^21 forks). Built:

- A `Condition` is a sorted list of (variable, value), at most 16, inline and `Copy`: a target is
  one variable (it fixes every `target_*`), each package's feature a boolean, `test`,
  `debug_assertions` and each `--cfg` a boolean. The request carries the `Space`: values per
  variable and the feature dependencies as closed implications.
- A task runs under a condition. Reading a variable it does not fix answers `Need(Cfg(var))`;
  the machine re-enters it once per value `Space::fork` allows, each narrower. Its record is
  stored under the condition it **read**; a lookup takes a record whose condition the asker's
  implies, so a record that read nothing serves every configuration.
- More than 64 conditions for one item is `ConditionError::OverCap`, a finding naming the item
  and its variables; never a sample. A finding carries its reduced condition.

In the request machine: a fork is a job named `key::at_condition(base, condition)` (the base
itself under `ANY`), so records, rows and readers are an ordinary job's. `Ran::Cfg(var)` makes
the job's record a split: its parts are the forks, its read list the space's digest (another
space, other forks). Workers name reads by base; the machine walks splits down to the fork the
task's condition implies (`resolve`), and a need that splits on a variable the asker does not fix
forks the asker. Rows keep `split` and `cond`. The cap is the span of the node's variables, so the
same node is refused in any settle order. Batches are not grouped by condition. The oracle runs
on the model and through `Request`: the findings a full configuration implies are its own, each
once; the plant `RanUnderCondition` (an asker served a fork it does not imply) is caught.

## The system side (blaze-cli)

- **Sweep.** `sweep::StatSweep<S: Stamps>`: list, then read and hash only a file whose stamp
  (len, mtime, ctime, ino, dev) is unknown, differs from its leaf row (path, stamp, digest;
  `BCL2`), or is racy (changed within the clock's granularity of the sweep: stored unknown, so
  hashed again next time). Today `FromMeta` knows no stamp (bstd's `Meta` has none: the blaze-std
  lane's), so every file is read; the simulation test proves an unchanged, settled tree reads no
  byte, with a planted racy-stamp bug caught. The leaf table goes after `WriteLast`: a crash
  reports more files changed, never fewer. **Floor** (a million lines, about 4,000 files in 500
  directories): a stat per file in directory batches (`getattrlistbulk`; batched `statx`) on
  every core, (dirs x t_dir + files x t_stat) / cores, no byte read. Measured on reth at a named
  commit: a repeat check's `sweep` phase against those calls timed alone; the ratio goes here.
- **Store.** Behind one trait (`store::Store`: `get_into`, `put`, `after_answer`). The open it
  is to call is blaze-cas's `plain::Driven::open(root, settings)`, then
  `Driven::machine(&[checkout, home])` before any put to a shared namespace (a value naming the
  machine is `PutError::Machine`). The certificate is read by `locate` and one `pread`
  (`get_into`). After the answer is printed, never waited on by it: `seal()`, then `compact(n)`.
  The profile logs `report().sealed` and `report().checked`. Not reached yet: `Driven` takes
  std's `Path` and `Vec`, and blaze-cli reaches the system only through bstd, so it needs a bstd
  driver of the plain store (gap, blaze-cas's lane). Until then: a file per record under
  `.blaze/v2/records/`, a stand-in not measured against the floor above.
- **Units.** A small reader of `[package] edition` and the default targets (`src/lib.rs`,
  `src/main.rs`, `src/bin/*.rs`). It refuses by line any table other than package, profile and
  lints, and any dependency or build script. Gap: a crate root added without a manifest edit is
  outside the build (the units' source holds only manifests); fix: root candidates in its key.
- **Tasks today** (`work.rs`): `Units`, and `FileItems` (blaze-file-items' outline). A file's
  record is its `FileItems` record bytes, each `mod x;` is a `Locate::File` part (`x.rs` by
  rustc's rule; `x/mod.rs` and `#[path]` are not looked for yet), and each syntax error is a
  finding: until blaze-typeck's tasks exist, `blaze check` is a syntax check of every file the
  roots reach; other kinds answer `Failed`. Runs go one at a time (the pool waits on bstd).
- **Printing.** `file:line:col` from the file's line table at print time (`lines.rs`).
- **Profile.** Per kind: standing, found, made, fetched, entries, time; gets, hits, puts, pages;
  segments; then `phases` (each phase of the command in order, ns, on the clock `main` is
  handed) and `request` (steps and each effect kind summed). Written in place after the answer
  is printed, never synced. **Repeat-check floor:** one open, one read, a stat per file, a print:
  0.3 ms in main. Debug, 2026-11-15: 0.96 ms (3.2x): frame's first chunk 0.31, sweep 0.29.
- `export` and `build` say they are not built.

## Not decided here

- The certificate's read list (bottom.md 9 keeps files in it; here files are in its key, so a
  changed file is a miss, then forward). Equal in reads on a repeat; to revisit with FSEvents.
- Pages by name prefix against pages by module: measure on reth.
