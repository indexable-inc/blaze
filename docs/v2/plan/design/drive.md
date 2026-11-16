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

1. **Start.** The cli hands the files the last answer read (path, content id, changed), the
   files gone since, the units' source and the `last` file; with no leaf table, `cold`. The
   **certificate key**: the tool, the command, the units' source and every (path, id). One `Get`
   (none when cold). A file a part names that the machine lacks is `Open { name, path, module }`,
   answered by `Opened` (its id, or none): files join as the walk reaches them.
2. **Certificate stands**: its record holds the findings, each with its file and item start.
   `Print` each, `Done`. That is the whole repeat: one store read.
3. **Miss.** Get the last answer's certificate (its findings stand for what this run does not
   redo) and the last answer's root page. Then choose the walk:
   - *Forward* when `last` names the same tool and command shape: seed a job per changed or gone
     file (`FileItems`, or a manifest's `Units`), and the root `Units` if its source moved.
   - *Full* otherwise (first run, a new tool): seed the root `Units` job; every part of every
     record is a job. Old rows still short-cut every job whose key did not move.
4. **Jobs** (one per name) advance as data arrives; a job that lacks a page, a name or a home
   waits on it by name, never by blocking.
5. **Finish** when no effect is outstanding and every job is settled: update the reverse index,
   write the touched answer pages and the root, the new certificate, `WriteLast`, then `Print`
   the findings sorted by (path, position) and `Done`.

## Names, sources and keys

A **name** is what a task is about: `(kind, subject)`. Subjects: a file's is the hash of its path;
an item's is its item key (identity.md); the root `Units`' is fixed, a manifest's its path's. A **source** is the text the task is
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

**After a restart** the repeat loads the pointer file and the certificate (keyed by the leaf ids
the stats prove current): through the plain store, from a cold page cache, the head, manifest,
one index page (`locate`), the certificate by one `pread`, the pointer file (a page per 4 KiB)
and the slot file. **Floor: 6 pages for hello-world, about 45 for reth** (not yet measured).

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
1-2 µs; a task about 50 µs. One step per task caps one coordinator near 50/2 = 25 to 50 cores;
parallel.md's row puts the serial part near 0.65 s (speedup about 17). This removes each term:

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
merges. A test steps 101,000 jobs within 2x of a 1,000-job run's bookkeeping a job. Until bstd's
`scope.must_overlap` and `PerWorker::take_all` land, the cli runs batches inline on one thread.

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

- **The sweep follows what was read; nothing walks the checkout.** The leaf table (`BCL4`, in the
  pointer file) holds exactly what the last answer read: each file (path, stamp, id); each module
  slot (`mod x;` asks `x.rs` and `x/mod.rs`: one is read, the other an absent probe); each manifest
  read or probed absent; each directory `Layout` listed for a member glob (its names' digest). A
  repeat stats those and lists those directories, nothing else: its cost is in proportion to what
  the build reads, and `.venv`, `target` or a symlinked `.claude` are never touched. A stamp
  (len, mtime, ctime, ino, dev) settled and equal keeps the id; else the file is read and hashed.
  A cold run discovers forward: `Layout` asks for the root manifest, members' manifests and the
  directories their globs name; the units name crate roots; each outline names its modules; the
  machine `Open`s each file a part names (a `Locate::File` part carries its path in the record,
  `BDR3`), and each read or probe joins the table. A new file matters only if something probes
  for it, Rust's rule: a file made at a probed-absent path re-reads exactly its slot. The settled
  table starts at 8 slots a leaf (at least 4,096) and grows (doubled, rows republished between
  batches) when a publish finds no slot: a growth step, never a panic (rule 18). **Floor**: a stat
  per leaf on every core, no byte read; measured on blaze's tree, the ratio goes here.
- **Store.** Behind one trait (`store::Store`: `get_into`, `put`, `after_answer`). The open it
  is to call is blaze-cas's `plain::Driven::open(root, settings)`, then
  `Driven::machine(&[checkout, home])` before any put to a shared namespace (a value naming the
  machine is `PutError::Machine`). The certificate is read by `locate` and one `pread`
  (`get_into`). After the answer is printed, never waited on by it: `seal()`, then `compact(n)`.
  The profile logs `report().sealed` and `report().checked`. Not reached yet: `Driven` takes
  std's `Path` and `Vec`, and blaze-cli reaches the system only through bstd, so it needs a bstd
  driver of the plain store (gap, blaze-cas's lane). Until then: a file per record under
  `.blaze/v2/records/`, a stand-in not measured against the floor above.
- **Units** (`units.rs`): blaze-manifest's `Layout`, run once a request when a manifest or
  listing row moved (else only if a units record misses). The root's source is the digest of
  those rows; it lists members (`Name::manifest`) at `Locate::Source(digest of the member's parts
  and findings)`, so a manifest edit re-makes only the members whose share moved. A dependency
  outside the members is finding 2, what `Layout` does not read (`default-members`, examples,
  tests, benches, build scripts) finding 3, a `Layout` error a finding on its span. The
  workspace's outside dependencies are one finding, 10, on the root manifest's
  `[workspace.dependencies]` (else `[dependencies]`) header: their count and the first four
  names, sorted, each once (tempo had 1,204 per-line findings before).
- **Tasks today** (`work.rs` `builds`): `Units`, `UnitGraph`, `Fetch`, `FileItems`, `Surface`,
  `MacroModule`, `CrateScope`, and `BodyCheck` for syntax only (`whole` says so). A file's
  record is its `FileItems` record bytes, each `mod x;` is a `Locate::File` part (the slot `x.rs`,
  `x/mod.rs` probed on `Open`; `#[path]` read), and each syntax error is a finding. Every other
  kind answers `Failed(NotBuilt)`. `FileItems` and `BodyCheck` run on the pool, the rest on the
  main thread. What comes next: "End state: the whole pipeline on the command", below.
- **What a command says it covered.** `Command::needs` (request.rs) lists the kinds a command
  is defined to cover; `work::builds` says which this worker builds and `work::runner_does`
  which the program runner does for `run`. The closing note names each stage left out, from
  `Kind::stage`: "this build of blaze does not read the dependencies, resolve names, expand
  macros or check types yet: `check` did read the manifests and parse". It shrinks as kinds are
  built and is gone when all are; nothing else prints a coverage sentence.
- **How to add a task kind.** (1) If it is a new kind, its discriminant, `Kind::ALL`, `KINDS`,
  `name` and `stage` in key.rs, and `Command::needs` if a command covers it; the wire takes the
  byte. (2) In work.rs, `builds` says `true` for it and `run`'s match gets its arm: an arm
  returns `Did::Done(Made)` (its name, `out` digest, parts, findings, payload),
  `Did::Need(wants)` (in the batch frame: it is entered again with them as `task.inputs`, each
  read with `work::payload(cx, store, input, scratch)`, whose `Read::payload()` is the input's
  record payload), or `Did::Failed`. Work in `scratch` (reset after the task), hold what the
  record keeps in `held` (reset after the batch). A panic, or a frame refusal, fails the task
  alone (`run_one`). (3) A part another kind reads is `Part { name, locate, place }`:
  `Locate::Source(digest)` when its source is known before its work, `Locate::File` for a file
  to open. (4) A finding is `(rule, start, end, message)` with its args joined by `\x1f`, a
  row in blaze-report's words.rs, and a line in print.rs `every_rule_the_tree_emits_has_words`.
  (5) Its sans-IO logic lives in its own crate (blaze-scope's `task`, blaze-unit-graph's) and
  the arm is the thin edge. The drive's sim covers the machine for any kind; a kind's own
  record is tested in its crate.
- **Profile.** Per kind counts and times, then `phases`, `request` and `memory` lines; written
  after the answer, never synced. **Repeat-check floor:** one open, one read, a stat per file, a
  print: 0.3 ms in main. Optimized, 2026-11-15: 0.53 ms (1.8x), the sweep 0.27 of it (no stamps
  yet); first chunk 0.31 to 0.02 ms (blaze-mem reserves 4 GiB segments as carved, not 256 GiB).
- **Tool and home forms.** The tool key is the binary's linker id (`LC_UUID`, GNU build-id; equal
  trees link to one id) and each keyed form's magic (`BDR2`, `BDP2`, `BDA1`, `BDC3`). Owed: each
  kind keyed by its own task code's identity, so a codegen change keeps the outlines. `last`
  (`BDL1`) and `leaves` (`BCL3`) are one file sealed `BCS1`, written in place: torn reads as absent.
  `file:line:col` at print time (`lines.rs`); `export` and `build` say they are not built.

## Not decided here

- The certificate's read list (files are in its key: a changed file is a miss, then forward);
  to revisit with FSEvents. Pages by name prefix or by module: measure on reth.

## For a successor (2026-11-16, the driver lane, uncommitted in blaze-fresh-1115)

- Done, tests green unless marked: a file's row keeps its edition, its findings placed from 0
  (request.rs `settle`; sim `EDITION`/`file_place`, control `EditionAsPlace`); the sim opens
  files as the cli does (`Setup::opens`); `x/mod.rs` and `#[path]` (work.rs `module_file`, test
  `mod_and_path_attributes_name_the_files_rustc_reads`); `.blaze` out of the root listing (a
  certificate missed after every cold run); reached-gate `Done` with work queued (main.rs
  `answered`); notes through `frame::note`; manifests' findings first; the runner's `run::refusals`.
- Verified 02:47: the per-task scratch frame and the per-task `catch_unwind` (tests
  `a_workspace_larger_than_one_task_frame_checks`, `an_oversize_file_fails_its_task_alone_..`,
  `a_planted_panic_fails_one_task`). Outline scratch is about 2 KiB per top-level item
  (blaze-file-items `outline_items`, one `walk::walked` per part): a 1,000-fn file needs > 2 MiB.
- Verified 03:09 to 03:11 (both crates' tests green; tempo's table: 708 tasks in 275 batches, so
  the pool is rarely wider than 3, and puts of 105 ms run on main: widen batches and put in the
  task next; a repeat is 9.2 ms of sweep in 10.2): FileItems tasks on the bstd
  pool (`answer_batch`: spawn each, run the rest here, join in ticket order; each in its own
  task frame, its record built in `Global` and copied into the batch frame: one copy owed
  away); `--workers N`; `Did::Need` and `payload` for the names lane; `holds` under a
  certificate (test `an_undo_keeps_every_file_for_the_next_edit`).
- Verified 03:29 to 03:50 (blaze-cli's 37 tests green): tempo's cold check went from 317 to 110 ms on
  18 workers. Three changes did it:
  - One pack per command (store.rs `PackStore`). Records are sorted by key, then come the index
    and a 24-byte footer.
    - A pack is named by the digest of its index, so two commands sealing at once never write
      one name with two contents.
    - The open checks each pack's footer, index and rows once. A torn pack is removed and said,
      and its records are made again.
    - Since 03:53, nothing on a command's path is synced (main's decision: the home is a cache
      that must never hand back wrong bytes, and need not survive a power cut).
      - Each footer holds the digest of the payloads.
      - The boot record (`.blaze/v2/packs/boot`) names the packs known whole in this boot.
      - A pack from another boot is read whole and checked once, then named in the record.
    - Owed: bstd's boot fact (the effect-layer lane). Until it lands, `Boot::Unknown`: every
      open reads the packs whole, 4.1 ms of tempo's 13 ms repeat.
    - The merge past 8 packs publishes the merged pack and names it in the boot record before
      it removes the packs it replaces.
    - Proof is in store.rs `tests`:
      - crashes and failures over 1,000 seeds;
      - torn packs from an older boot;
      - two commands interleaved, at the grain of store calls, not of effects;
      - a full disk;
      - one read a boot.
      Plants: `MergeRemovesFirst` is caught by the in-boot witness; `VerifySkipped` and
      `SelfVouching` by the torn test. The crash runner's versions of the last two are
      `#[ignore]` until the disk model's strict rule tears a kept unsynced rename.
  - Every batch handed out runs on one pool scope (work.rs `run_batches`).
  - Opens are probed together (work.rs `open_all`).
  The repeat's slot rows are looked at serially (sweep.rs `follow`): the pool version was
  measured slower and deleted. Owed: the rows grouped by folder, as columns, for the effect
  lane's `Effect::StatMany`.
  - Under `run`, blaze's own words go to stderr, so a clean run prints only the program
    (tests/run_output.rs).
  - The measured repeat floor, warm, for tempo's 870 read files in 241 folders: a stat each is
    0.51 ms; one `getattrlistbulk` per folder is 2.7 ms.
- Verified 04:15 to 04:23 (blaze-cli 38 and blaze-drive 28 tests green): the `Fetch` task is
  dispatched (blaze-cli fetch.rs).
  - The layout reads `Cargo.lock` as one of its reads, so a lock edit moves the units' source
    and the certificate. A lock or plan the reader refuses is finding 11, `lock-unread`.
  - `Start` hands the request each unit's key; `begin` adds a foreground `Fetch` job for each.
  - A unit's record is a file, `.blaze/v2/sources/<key>`, checked whole at lookup
    (`PackView::open_source`).
  - With the `fetch` feature (default), one blaze-fetch machine a command wants every unit, so
    the fetches overlap. Without it, no socket is opened, and a unit the store lacks is finding
    13, `cannot-fetch`. A lasting refusal is finding 12, `source-refused`, kept as a record. A
    passing failure leaves no record: the job fails with its words, and the next command asks
    again. Repeated failures of one kind print four lines and a count.
  - Done 05:0x, compiled, not yet run (lanecheck blaze-cli: "check ok (all targets)"): the
    HTTPS client. fetch.rs `net::client(cert_file, cert_dir)` calls
    `blaze_http::roots::system(Places { .. })` once, on the first send
    (`Io::client.get_or_insert_with`), and never when every record stands. `SSL_CERT_FILE` and
    `SSL_CERT_DIR` are read at the edge in main.rs and handed in through `FetchEnv`. Its failure
    is `Untrusted::Tls(words)`, which is not lasting. Untested on a real fetch: run step 4 (a cold
    fetch of tempo's reach on a copy).
  - DONE 04:5x (uncommitted), step 1 as worked out below: `Kind::UnitGraph` (key.rs, wire
    byte 2, later kinds shifted), `Start { graph }`, a `Fetch` need located by its subject
    (`Name::located_by_subject`), `unit_graph_task::run` returning `Entry::{Made, Need,
    NotFetched}` with `Unheld { failed, refused }`; blaze-cli graph.rs (members, the target's cfg
    from the shelf, records mapped with `blaze_core::Mapping::map_file` and kept for the
    command, a level wanted of the machine at once by `Lockfile::want_ahead`; the machine no
    longer wants the whole lock). Sim tests: `a_lock_fetches_only_what_the_roots_reach`,
    `an_edit_reaching_one_more_entry_fetches_one`,
    `a_failed_fetch_leaves_no_graph_and_the_next_request_asks_again` (its control: ignoring
    `Task::failed` fails it). blaze-drive "exit 0; 31 passed, 0 failed"; blaze-cli "38 passed,
    0 failed" (tree e191fc702d). Release build 2ac78a37ce: hello-world copy with its stale lock:
    no `Fetch` row in the profile, "ok · 1 file checked · 7.8 ms · 4 rebuilt", repeat 0.4 ms.
    Tempo copy, empty store, real network: "Fetch ... 900 fetched", "UnitGraph ... 9 entries,
    279.699 ms", 12.85 s real, 234,553,344 max RSS, 580M in `.blaze/v2/sources`; repeat
    "certificate stood; 13.701 ms in main". Not explained yet: RSS over the fetch lane's 85 to
    101 MiB (the mapped records are a suspect, unmeasured). Tempo prints one error,
    `dependencies-not-read` (units.rs finding 10): now the graph reads them, retire it.
  - DONE 05:1x (the next runner, uncommitted), the two regressions:
    - Finding 10, `dependencies-not-read`, is retired. It is gone from units.rs and from
      blaze-report's words.rs, where its number is kept unused. Its test became
      `outside_dependencies_are_no_finding_of_the_layout`. The closing note already says what
      is and is not checked: "this build of blaze does not resolve names, expand macros or check
      types yet: `check` did read the manifests, fetch the dependencies' sources and parse".
      Tempo copy (blaze-tempo-1116/ug/tempo, sources warm), tree d0644dfc0f: "exit 0", "ok ·
      655 files checked · 857 ms · 1617 rebuilt" with the packs removed. Profile rows:
      "UnitGraph ... 9 needs 8 278.726 ms", "Fetch ... 900 fetched ... 448.006 ms" (every held
      record is checked whole at lookup: step 3's once-a-boot rule removes this). "266126296
      peak memory footprint". The repeat: "17 ms · nothing changed".
    - hello-world's cold 7.8 ms: a first run after copying, with cold pages. Its in-main rows
      were "layout 2427041 · the target's cfg 721250 · request 2276458" (FileItems 1.112 ms),
      and "process start to runtime start 6607166" sat outside main. The old binary, run again
      cold (`.blaze` removed), was 1.9 to 2.0 ms: layout 0.66 ms (the stale 177-entry lock
      parsed), cfg 0.21 ms, UnitGraph 0.065 ms. The fix: with no dependency outside the
      workspace and the toolchain, the lock is not read and there is no graph job
      (graph.rs `reaches_outside`, `toolchain_line` as the one home of the bstd-line rule;
      units.rs `lay`; tests in units.rs check the stale lock is unread and, as the control, that
      a Rust-edition `bstd` line reads it). Tree d0644dfc0f, cold six times: "2.5, 1.6, 1.3,
      1.4, 1.5, 1.6 ms", no UnitGraph row, "layout 252875". Floor: the 1.1 ms measured before
      the graph job, so the ratio is 1.2 to 1.5. Repeat: 0.4 to 0.6 ms, "in main 465250". Its
      largest row is "machine and store open 159166", the whole-pack check owed to bstd's
      boot fact.
    - Note: `/usr/bin/time -l timeout ..` measures `timeout` itself (1.6 MB). Put `timeout`
      outside: `timeout N /usr/bin/time -l blaze ..`.
  - Owed, step 1 (the plan as built): fetch only the reach, never the whole lock. hello-world's stale lock lists
    bstd's 177 dependencies, which the toolchain gives. Worked out, nothing written yet:
    - A new kind, `Kind::UnitGraph`, placed between `Units` and `Fetch`. Shifting the bytes is
      safe: the tool key changes with every binary. Its stage is "read the manifests". Add it
      to `Command::needs`.
    - One job a command, name `Name::unit_graph()` (a constant subject), source
      `unit_graph_task::source(inputs)` (the lock's bytes, the layout's read digest, the target,
      the host, the ask). `Event::Start`'s `fetches` becomes `graph: Option<ContentId>` (`None`
      with no lock); `begin` adds that one job instead of a `Fetch` per lock unit.
    - The task replays `Resolution` from the top on each entry; nothing is held between
      entries. A `Pass::Need(ids)` whose `StoreSources::package(id)` is
      `SourceError::NotFetched { key }` returns `Need` of `Name::new(Kind::Fetch, key)` for
      the whole pass (home: the Fetch name itself), so the reach is fetched level by level
      (tempo is 9 deep).
    - A need that settled with no record (a failed Fetch) is not in `Task::inputs`, so the task
      cannot tell "not asked yet" from "failed". DONE 04:33: the general primitive
      `Task::failed: Vec<Name>` (the needs that settled with no record this command), filled
      in request.rs `emit`, which is the only place a `Task` is made. blaze-drive: "exit 0;
      28 passed, 0 failed". mixcheck blaze-cli: "check ok (all targets) at 04:33; tree
      c300222311". No test reads `failed` yet: the UnitGraph sim test should. A failed key gives
      `res.package(id, None)` (unavailable), and the task then fails with no record ("N sources
      not fetched"): an incomplete graph must never stand by its key. A lasting refusal
      (finding 12) is a record, so that package is simply unavailable, and the graph is kept.
    - `Records` reads only the Fetch inputs' files, `.blaze/v2/sources/<hex>`, MAPPED rather
      than read (blaze-core `Effect::Map { extent: Whole }` through a `blaze_core::native`, as
      fetch.rs `reserve` does; make blaze-core non-optional). Keep the `Mapping`s for the
      command in the laid state, so a re-entry maps nothing again. Open them with
      `PackView::open_source_put`: the Fetch task checked the bytes this command.
    - Members: `Package::from_member`, minus a blaze-edition member's `bstd`/`blaze-std` dep
      (the toolchain gives it: units.rs `TOOLCHAIN_NAMED`). Find each member in the lock with
      `lock.named(name)` among the `LockSource::Path` entries; a member the lock lacks is
      finding 11. Keep the `Package`s in `Laid` (Global, leaked into the frame as `Lockfile`
      is).
    - Target cfg: from the toolchain manifest (`shelf::open_manifest(PINNED)`, then
      `ToolchainView::cfg(triple)`), not a table. The CLI does not open the shelf yet:
      toolchain.rs `HomeShelf::open` takes the start folder and `$HOME`. The triple is the
      build's own, from `cfg!(target_os/arch)`.
    - Sim tests: a lock with entries nothing reaches gives zero Fetch jobs; a manifest edit
      that makes one reachable gives exactly one. Then a copy of hello-world WITH its stale
      lock: zero Fetch tasks, and `check` clean and silent.
  - For step 3 (names as driver tasks), from the names library lane (acd2f3dd892b1b198), 04:3x:
    - Macro crates reach `resolve_crates` through `SourceTexts::proc_macro_key` and
      `run_proc_macro`. Working example: blaze-world examples/names.rs `procs_of`.
    - Compiling the proc-macro modules must be its own task kind with its own records, never
      inside a names task. Whole graph cold with compiling: 54.7 s; warm: 17.3 s.
    - The names task only looks up expansion records, or runs a call against a module already
      compiled.
    - Tempo's module store (78 of 79 modules): ask the proc-macro lane for the path.
    - Effect on tempo: unresolved imports 10,306 -> 5,636.
  - From the effect-layer lane (a9351cefef148a0e0), 04:4x: two of the owed pieces are unblocked.
    - **Boot fact.** `bstd::Io::boot() -> Result<blaze_core::Boot([u8; 16]), NoBoot>`. Make
      blaze-cli `boot_of` into
      `match io.boot() { Ok(b) => Boot::Known(b.0), Err(_) => Boot::Unknown }`. Never key on
      it. This removes the 4.1 ms of whole-pack reads from tempo's 13 ms repeat; re-measure.
    - **Batched stat for the sweep.** It already exists as
      `Effect::Stamp { files: Arc<Names>, tag }` -> `Done::Stamped(Vec<Stamp>)`, in order; an
      absent name is `Stamp::GONE`. At most `STAMP_MOST` names.
    - Measured on 870 names: 544 us for the batch, against about 5.5 us for each
      `Effect::Stat`. Use it in sweep.rs `follow` in place of the serial look at each row.
  - Owed, step 2: one finding per shared cause. Add a client job (the client made once, its
    own name), which every Fetch job that must send `Need`s. Its failure is one finding naming
    the cause and every place it looked. Its Fetch waiters fail and are counted in the closing
    note ("1,173 sources not fetched"), not printed one by one. Lasting causes, kept as
    records: a digest mismatch, an archive that does not unpack, a git advertisement that is not
    git's, too big. Every other cause (network, TLS, store) is passing: no record, and the next
    command asks again.
    - Worked out 2026-11-16 (the runner after the unit-graph job), nothing written: the client
      is a COMMAND-ONLY kind, `Kind::Client`, one job, constant subject. A client is process
      state, not a fact: a kept record would let a later command's Fetch stand on a client it
      cannot make, and the failure would come back once per Fetch. So add `Kind::kept()`,
      false for `Client` only. Then in request.rs: no `get` for it (the `Looking` arm near
      `self.get(key, Purpose::Look(name), ..)` goes straight to `Queued` as `Group` does), no
      `put` in the made path (`write_whole` / `self.put(at, bytes, ..)`), and no row in the
      answer's table, so `current(name)` never finds one. A Fetch job whose record is missing
      returns `Need(Name::client())`. It is never wanted when every Fetch stands, so a warm
      repeat makes no client. The cli's Client task calls `net::client(cert_file, cert_dir)`
      once and keeps the handle in `Lockfile.net`. If that fails: ONE `Did::FailedSaying` naming
      the cause and every place it looked (`Places`). A Fetch whose `Task::failed` holds the
      client fails with `Why::Waited` (new) and prints nothing. The closing note counts them:
      "N sources not fetched: no HTTPS client". Sim tests: a failed Client fails all K
      waiters, with one finding and K counted; the next command asks again (no row); a repeat
      after a failed Fetch makes the same gets as one after none (the oracle for "never
      slower"). Control: a kept Client is caught by the "asks again" test. Tempo, network off:
      point the registry at an unroutable address through the existing override.
    - Fetching beside the parse rather than inside a batch.
  - DECIDED (coordinator, 2026-11-16 05:05), owed, step 3: source records move to the user's
    shared store, `<home>/.blaze/v2/sources/<key>`, beside the toolchain shelf, never under the
    project (fetch.md, "Record and key"). The home is passed down: main.rs reads `HOME` at the
    edge, as it already does for the shelf (`graph::target(.., home, ..)`). Change fetch.rs
    `SOURCES` and `record_path` to a `Dir` of the shared store, plus graph.rs's record mapping.
    A second COPY of tempo with the store warm must fetch 0 and write only its own records.
    Proof: a crash-and-interleave sim of two fetchers of one unit (temp name, then rename
    whole; plant: rename before the bytes are complete). Measure on tempo copies: cold into an
    empty temp home (time, bytes, peak memory footprint from `/usr/bin/time -l`, and what holds
    it; the fetch library alone peaks at 85 to 101 MiB), a second copy warm (expect "0
    fetched"), and the repeat.
    - The record checked once a boot (`open_source_put` after the first read).
- SPEC, the NAMES step (2026-11-16, for the next runner; not built):
  - Kind `CrateScope`, one job per crate the request reaches (a workspace member's lib or bin,
    an outside unit, a toolchain crate). Its key is blaze-world's `UnitInput::key`
    (`CrateItemKey`, `unit_input`): the crate's own surface digest (its items' signatures, `pub`
    and `use` lines, macro calls: from text, before the work, never a body), its cfg (unit atoms
    and build-script atoms by `Served::digest`), and its edges as the dependencies' crate
    identities (`crate_key`: package, version, source), never their surfaces. Check that first:
    if `crate_key` hashes a surface, every reverse dependency moves on a `pub fn`; make it an
    identity. The job's source is that key; nothing else is hashed.
  - Needs: each dependency's `CrateScope` record (by `Name::new(Kind::CrateScope, dep key)`),
    and for each macro call in its surface the `MacroModule` record of the macro's crate.
  - Kind `MacroModule`, its own job: one proc-macro crate compiled to wasm (rule 10), keyed by
    the crate's whole text and the toolchain id. Never inside a names job: the names lane
    measured tempo's names cold 54.7 s with compiling inside, 17.3 s without. The names job
    only looks up an expansion record or runs a call against a compiled module.
  - Record: the crate's scope (blaze-world `Names` for that crate: every public path to its
    item ids, the glob and re-export edges resolved), written once, read in place.
  - Where it lives: by whose text the key is. A toolchain crate's or an outside dependency's
    key holds no project text, so its record is the same for every project: the shared store
    (`~/.blaze/v2/`, beside `sources/`). A workspace crate's: the project's `.blaze/v2`.
  - A body edit costs nothing: bodies are not in the surface digest, so the key and every
    dependent key are equal and every record stands (floor: the sweep's stat, ~0.5 ms on
    tempo). An added `pub fn` moves that crate's key, so its scope is redone. A dependent's key
    holds the edge's crate identity, not its scope; its record's read list holds the scope's
    out digest. So each direct dependent fails its read check and resolves again. Its own out
    is equal unless it glob re-exports the new name, and there the walk stops. Floor: one
    crate's resolve plus one per direct dependent, each a few tens of microseconds for a
    1,000-item crate.
  - Unresolved names are NOT printed as errors yet: about 1,300 of tempo's imports fail by our
    known gaps. The closing note says one line: "N imports not resolved by this build (known
    gaps); not reported as errors".
  - Sim tests first: a body edit makes zero `CrateScope` jobs; an added `pub fn` makes one plus
    a compare per dependent; a macro crate's module compiled once for two users.
  - DONE 05:3x (the names runner, uncommitted; blaze-world "8 passed, 0 failed", blaze-drive
    "35 passed, 0 failed", `mixcheck blaze-cli test` "38 passed, 0 failed" at tree ac40b7d34b):
    - Step 1, the key. `unit_input`'s key was an identity (name, version, source, kind, crate
      name, relative root, edition, features, cfg, deps' keys, script digest; nothing of the
      machine) but missed two inputs: the toolchain (a std change left every unit's key equal)
      and, in toolchain.rs, the target's cfg and the dependencies (the same pack on another
      target, or over another `core`, was one key). Fixed: `implicit_key` (std or bstd, and
      `proc_macro` for a proc-macro unit) is in every unit's key; a toolchain crate's key is
      its pack, features, the target's cfg and its dependencies' keys, keyed dependencies first
      by passes (a cycle: `WorldError::ToolchainCycle`). Test
      `a_toolchain_crates_key_covers_its_target_and_dependencies`. The world key stays the
      crate's IDENTITY on purpose: item ids are made from it, so it must not hold text.
    - Step 2, in the sim only: `Kind::CrateScope` (13), `Kind::MacroModule` (14) and
      `Kind::Surface` (15, one file's surface: its source is the digest of its signatures and
      `use`/`pub` lines, its out is its source). A file's text is NOT in the crate's key: the
      CrateScope reads its files' `Surface`s, so a body edit reruns `FileItems`, the `Surface`
      stands, and no CrateScope runs. `Why::Waited { on }`: a task whose need failed. Sim tests:
      `crate_scopes_resolve_once_in_dependency_order_in_any_completion_order`,
      `a_body_edit_runs_no_crate_scope` (control: a surface holding bodies is caught, "a body
      edit ran {.., CrateScope: 1, Surface: 1}"), `an_added_pub_fn_reruns_its_crate_and_the_dependents_reading_it`
      (the unit of rerun is the whole dependent crate; floor: the dependent's paths that read
      the changed module), `a_failed_crate_leaves_its_dependents_without_a_record`. A run is an
      entry: a crate with needs is entered twice (asks all at once, then re-entered).
    - Owed, in order: (a) the cli's `FileItems` emits a `Surface` part and the units/graph job
      a `CrateScope` part per crate (source = world key) and a `MacroModule` per proc-macro
      crate; `builds()` in work.rs turns them on. (b) `procs_of` (blaze-world examples/names.rs,
      and again in blaze-real examples/names_load/mod.rs) becomes ONE library function in
      blaze-world called by both examples and the cli. (c) The CrateScope task: blaze-world's
      names for one crate over its dependencies' records (ask the names lane, OWNERS.md, for a
      per-crate entry to `resolve`), its record mapped by dependents. (d) MacroModule: a module
      store lookup; absent is ONE finding per macro crate naming the bootstrap tool. (e) The
      closing note's line and the corrected "not checked yet" sentence; then the measurements
      of the task (hello-world copy, tempo with a few leaf members, ONE whole-tempo run).
      (f) Records of toolchain and outside crates in the shared store (not built: today they
      would live in each project). (g) Kind order: the three new kinds sort after `Test` in
      the queue; move them before `Signature` when the wire is next changed.
  - DONE 05:4x (the next runner, uncommitted; release built from tree d46bce916e; `mixcheck
    blaze-cli test` "39 passed, 0 failed", blaze-world and blaze-real "check ok (all targets)"):
    - Owed (a), the `Surface` half: work.rs `outline_file` emits one `Surface` part per file,
      its source `surface_source` (per entry outside a fn body: kind, parent, `signature_hash`;
      a macro definition's or item-position call's whole `hash`); `run` answers it with out =
      source; `builds()` says yes. Test `a_body_edit_reruns_no_surface_and_a_signature_edit_one`
      (control: hashing `item.hash` for every entry fails it, run at tree c97f4ed3a6). On a
      hello-world copy, a body edit: "1 runs", profile row "Surface standing 1 found 0 made 0".
      Two tests now count the new surface's run ("runs, 2").
    - Owed (b): DONE. `blaze_world::procs::register_macro_crates` (feature `procs`, typed
      `ProcsError` and `Unregistered`), called by both examples. The cli does not call it yet.
    - Still owed, in order: (a') `MacroModule`: the graph job's part per proc-macro unit (source
      = blaze-proc-macro `module_key` of its `CrateId`), the task a `read_module` lookup in the
      module store the command's inputs name; `ModuleError::Missing` is ONE finding naming
      `toolrun.sh proc_macro_modules`. It needs blaze-proc-macro in blaze-cli (wasmtime comes
      with it: split the key and lookup out of the runtime if that costs the cli's build).
      Then (c), (d), (e), (f); the brief's steps 2 (verify a source record once per boot), 3
      (sources in the shared store) and 5 (the client job) are not started.
    - (c) is unblocked: the names lane's per-crate entry (uncommitted, blaze-scope
      src/crate_scope.rs; its design in bottom.md section 6 "Per-crate entry") is
      `resolve_crate(input: &CrateInput, deps: &[&[u8]], sysroot, sources, source, par, alloc)
      -> Result<CrateScope { names, record }, CrateScopeError>`. `deps[i]` are the
      dependencies' CrateScope record bytes, 16-byte aligned (`resolve::aligned`), with
      `CrateId(deps.len())` the crate itself. `Err(Need(keys))` comes before any work and names
      the `CrateItemKey`s of records a given dependency reaches (re-exports of re-exports). Add
      them after the direct ones and call again: one extra round at most. Give it no cycles.
      `read_record` reads a record. Its reference loop is `resolve_by_records`. The names
      lane's message has the full text.
  - DONE 06:xx (the next runner, uncommitted; `mixcheck blaze-cli test` "39 passed, 0 failed" at
    tree 8eed7a499d; `mixcheck blaze-proc-macro test` "43 passed, 0 failed" at tree 2e99c31ec8;
    `mixcheck blaze-drive test` "36 passed, 0 failed" at tree ee9e700d6d):
    - DECIDED (coordinator 06:25), recorded in proc-macros.md: the runtime is linked into
      `blaze` behind blaze-cli's feature `sandbox` (default on); blaze-proc-macro is split by
      its feature `runtime` (what moved: proc-macros.md, the DECIDED section). The module store
      is `<home>/.blaze/v2/proc-macro/modules/<key>/`, home the passed value.
    - Written: blaze-world `procs::{identify, git_folders, MacroIdentity, Unidentified}`
      (feature `procs`, no runtime; `register_macro_crates` under `procs-run` is built on it);
      blaze-proc-macro `LockedPackages` (the lock read once for 81 crates) and `find_module`
      (reads `wasm-sha256` only); blaze-cli `macros.rs`: the graph job's `MacroModule` parts
      (subject: name and version; source: the graph's source with the subject) and the task
      (Needs the `UnitGraph` record, lists every macro crate once a command into
      `Facts::macros`, looks its module up; absent: ONE `FailedSaying` with the bootstrap
      command's input). `FetchEnv::home` added. Sim test
      `an_absent_module_is_asked_again_and_a_present_one_never` (no planted control yet: flip
      the sim arm to `done` and watch the first oracle fail).
    - Owed next, in order: (1) a cli test of the task over a temp home (present, absent). (2) Path macro crates (tempo has 1) are
      "not identified": their identity is their files' digest, which no key holds yet; the
      part must read its crate's file records. (3) Measure: `blaze-drive` vs `blaze-bare`
      (`GREENMIX_FEATURES=--no-default-features` drops `fetch` too: add a feature set without
      `sandbox` only) binary size, bare `blaze`, repeat `check` of a hello-world copy; then the
      tempo copy with a temp HOME holding symlinks `.blaze/v2/toolchain` -> the real shelf and
      `.blaze/v2/proc-macro` -> port27/procs-1116/module-store/proc-macro: count `MacroModule`
      rows found/absent (expect 80 present, asn1-rs-derive absent, 1 path crate not identified).
    - Not started: the brief's steps 2 (verify a source record once per boot: fetch.rs
      `held_record` reads and BLAKE3-checks each held record whole every command; design: after
      the check, write `<record>.boot` (temp name, rename) holding the boot id; a lookup whose
      mark equals `Driver::boot()` skips the read; no boot id: check once a command; sim: a
      crash between check and mark leaves no mark; plant: mark before the check), 3 (sources in
      the shared store, above), 4 (CrateScope), 5 (the client job, above).
  - HANDED ON 07:xx (the CrateScope runner, uncommitted; `mixcheck blaze-cli test` "39 passed,
    0 failed" at tree afbbc78547; `mixcheck blaze-drive test` "36 passed, 0 failed" at tree
    437f825c79; `lanecheck blaze-drive` failed then on another lane's blaze-surface
    through.rs:200, not ours). Nothing measured: the step is not built.
    - Written: work.rs `Record::reads` (`Vec<(Name, ContentId), A>`), passed as `Output::reads`
      (was `&[]` for every cli task). Every task leaves it empty today (keyed whole by source);
      the `CrateScope` task fills it, so the machine's `made_split` keeps its record while each
      read's out is today's (request.rs 2267-2282) and asks it again when one moved.
    - What the entry reads, and why a body edit runs zero `CrateScope`: `resolve_crate` reads
      SOURCE TEXT (blaze-scope `SourceTexts::read(krate, path)`; blaze-world `Texts` serves tree 0
      = the checkout, tree n = toolchain packs then fetched packs). It never reads a `Surface`.
      The task's key is the world key (an identity, no text), so the text it read must be
      covered by its read list instead: each checkout file the resolution read (tree 0) is read
      as its `Surface` (`Name::new(Kind::Surface, Name::file(path).subject)`, the out its
      outline made). A body edit reruns `FileItems`, leaves every `Surface` out equal, so the
      read list holds and no `CrateScope` runs (the sim's `a_body_edit_runs_no_crate_scope` is
      the contract; its control hashes bodies into the surface). A pack's file is under the key
      (package, version, source pin), so it needs no read.
    - DESIGN for the next runner (decided here, build it in this order):
      (1) crates/blaze-cli/src/names.rs. `NamesWorld<'f>` built once a command (like
      macros.rs `MacroCrates`): the `UnitGraph` record (Need it, home `Name::unit_graph()`,
      `blaze_unit_graph::record::decode`), the lock (`Lockfile::lock`), the toolchain
      (`blaze_world::Toolchain::open(&mut HomeShelf, PINNED, TRIPLE, blaze_world::needs(graph),
      ..)`: `graph::target` already opens a `HomeShelf` in main.rs:454 with `fs`; keep that shelf
      in `graph::Graph` so a task opens no second one), packages
      (`blaze_world::Fetched { store: StoreSources::new(lock, R, PackView::open_source_put, ..),
      paths, lock, alloc }` where R maps `<checkout>/.blaze/v2/sources/<hex>` for ANY key the
      graph pins: graph.rs `Mapped` without its `handed` filter), the checkout
      (`blaze_world::Checkout::new(<project root>)`: blaze-world feature `std`, already on), and
      scripts: INTERIM, an input folder of exported `<name>@<version>@<host|target>.outputs`
      records (as blaze-world examples/names.rs:682-699 reads them; TEMPO-RUNS.md names the
      folder), stated as interim in the closing note; `None` gives `Script::Missing`. Every
      borrow leaked into the command frame `'f`, so `World<'f>` lives in `Facts`.
      (2) Parts: the graph job (graph.rs `Entry::Made`, beside `macros::parts`) emits one
      `CrateScope` part per `world.crates` entry (toolchain crates included): subject
      `ContentId::of("blaze-cli crate scope 1", key.0)`, source the same key digest. Subject =
      key on purpose: a dependent names a dependency's record from the key alone (its own input
      and the `Need` keys `resolve_crate` returns). A key change is a new name: right, since a
      dependent's key holds its dependencies' keys.
      (3) The task: find the crate by subject (a sorted (subject, index) list made with the
      world). Want each direct dependency's and each sysroot crate's `CrateScope` (home
      `Name::unit_graph()`); a failed one fails it `Why::Waited { on }`. Read each with
      `work::payload`, copy to 16-aligned (`blaze_scope::resolve::aligned`), call
      `resolve_crate(input, deps, sysroot remapped to local indices as resolve_by_records_with
      does at crate_scope.rs:645-679, &RecordingTexts(&world.texts), source = crate index, &Serial,
      alloc)`. `Err(Need(keys))`: Want those names too, re-entered (one extra round at most).
      `Ok`: keep the `CrateScope` in a per-command map by subject (frame `'f`); if a tree-0 path
      it read has no `Surface` among the inputs, Want them (home `Name::file(path)`) and on
      re-entry answer from the map (no second resolution). Payload: u64 exports length, pad to
      16, exports, pad to 16, inner; out = digest of the EXPORTS only (dependents key exports
      only). `reads` = the Surfaces (with their input outs) and the dependency records read.
      (4) `builds()` says yes to `CrateScope`; the closing note: "names resolved for N crates;
      M paths unresolved by known gaps (not reported as errors)" counted from the made records
      (a u32 pair at the payload's head), and `left_out` no longer names "resolve names".
      (5) To RUN macros: under the `sandbox` feature, `blaze_world::procs::register_macro_crates`
      once a command and `SourceTexts::proc_macro_key`/`run` on the recording wrapper (blaze-world
      examples/names.rs `procs_of` and `Timed` show the call); without it each macro crate's calls
      are refused, one note per crate (the library's default).
      (6) Where records live: all in the project's store for now; toolchain and outside crates'
      belong in the shared store (step 3 of the brief, not built): say so in the note.
      (7) Sim first for the one new behaviour: the first entry learns its files and Needs their
      Surfaces, the re-entry answers without a second resolution; planted bug: resolve again on
      re-entry (count resolutions per crate == 1).
  - BUILT 2026-11-16 (the names.rs runner, uncommitted; handing on). crates/blaze-cli/src/names.rs:
    the world (std shelf `<home>/.blaze/v2/toolchain`, sources folder mapped for any key, the one
    reached path package placed at the checkout root, scripts `None`, all in the global allocator),
    made in work.rs's `UnitGraph` arm after `graph::run` (which then names one `CrateScope` part per
    world crate, subject = digest of the key, source the same) or by the first `CrateScope` task;
    the task (deps + sysroot wanted; a toolchain crate only the sysroot crates in its dependency
    closure, else core and alloc wait on each other; `Need` keys wanted, re-entered; payload u64
    exports len, u32 unresolved, pad 16, exports, pad 16, inner; out = exports digest; tree-0
    files read wanted as `Surface`s and listed in `reads` with the dependency records); `builds()`
    yes; main.rs closing note "names resolved for N crates; M paths unresolved by known gaps" (M =
    the library's notes). blaze-cli gains blaze-scope and blaze-id.
    - Measured (release, greenmix tree 8ddfd1a874, a COPY of hello-world given `cfg-if = "1"`,
      because a project with no dependency reads no lock, so no `UnitGraph` and no world runs):
      cold "names resolved for 15 crates; 8 paths unresolved", "2.7 s · 38 rebuilt", CrateScope
      "made 15 ... entries 31 needs 16 2555.578 ms", peak footprint 491587000; repeat "2.8 ms ·
      nothing changed", 11665816; body edit "4.2 ms · 1 rebuilt", no CrateScope row, 12649072.
    - BROKEN: a signature edit (added `pub fn extra(_x: u8) {}`) reran FileItems and Surface but NO
      CrateScope ("7.5 ms · 2 rebuilt"). Find first: whether the hello crate's record holds its
      `src/main.rs` Surface in `reads` (the Texts path for tree 0 with dir "" may not be
      `/0/src/main.rs`, so `Recording` may list nothing), then whether the machine asks a
      `CrateScope` the command's `needs()` does not hold again when a read moved.
    - Left: (a) the signature-edit rerun above; (b) the test (B), which needs the toolchain shelf
      and a lock in the in-memory test io; (c) a project with no dependency gets no world (no lock
      read: units.rs reads the lock only when a member reaches outside); (d) 491 MB cold: the
      world and records leak into the global allocator; (e) macros, scripts, path-package dirs.
    - FIXED 2026-11-16 (runner 2, uncommitted; handing on). Neither suspect: a diagnostic print
      of each `CrateScope`'s crate and read files showed 15 crates, none of them `hello`:
      blaze-world's `held` kept only libraries and proc macros, so a binary (the project's own
      crate) had no `CrateScope` at all. Now `held` keeps target-side Lib, Bin and Test units
      (blaze-world test updated: the test target is a crate beside its library, 8 passed). Item
      (c) too: graph.rs `members_lock` writes the lock a workspace reaching nothing outside
      stands on (its members as path packages, from the manifests; no file read), and units.rs
      uses it, so every project has a graph and a world. Release tree 70e20f1e22, copies of
      hello-world: with cfg-if: cold "names resolved for 16 crates", "2.9 s · 42 rebuilt",
      494306768; body "3.7 ms · 1 rebuilt", no CrateScope; `pub fn extra` "names resolved for 1
      crates", "27 ms · 4 rebuilt", 21168824. No dependency: cold "names resolved for 15
      crates", "2.6 s · 37 rebuilt", 486638648; repeat 11665816; body "3.5 ms · 1 rebuilt";
      `pub fn` "25 ms · 4 rebuilt", 21152416. The toolchain's records live in each project's
      store: the shared store under the home is the owed fix (once per machine).
    - Left, in order: (1) blaze-cli's command tests now fail ("`PWD` or `HOME` is not set: the
      toolchain's shelf ... cannot be found"): every workspace now makes a graph, which needs
      the target from the shelf. Fix with the test (B): make the toolchain id and the shelf
      inputs of the request (FetchEnv, graph::target, names::world use `PINNED` and the home
      today: rule 75), write blaze-world's tests.rs fixture shelf (`toolchain_of`) into a temp
      home, and assert: body edit 0 CrateScope; added `pub fn` exactly the edited crate's; a
      crate another glob-imports, both; planted bug (Surface left out of `reads`) fails. (2)
      suspect (b) unchecked by a test, but the `pub fn` run above reran a CrateScope through the
      reverse index (`dirty_readers`), so `needs()` lacking `CrateScope` does not stop it. (3)
      `pub fn` costs 25 ms against a 3.5 ms body edit: the rerun builds the whole world again
      (toolchain open, sources) for one crate: profile it. (4) item (d), 487 MB cold, untouched;
      the names library's lane was not asked yet.
    - STEP 1 DONE 2026-11-16 09:4x (runner 3, uncommitted; handing on). The toolchain's id,
      the start folder (`PWD`) and the command's `Fs` are fields of `fetch::FetchEnv` (the one
      carrier, beside the clock and the home), filled once in `main` (`blaze_toolchain::PINNED`)
      and passed to `graph::target(cx, env, frame)`, `units::lay(.., toolchain, ..)` and
      `names::world(cx, root, ..)`; no `PINNED` is read below `main`. The names step reads
      through the command's Fs (rule 75; works in the sim): the shelf by `toolchain::HomeShelf`
      (DirShelf and its mmaps deleted: one shelf reader), the checkout and the sources records by
      `names::Reader`, each file read whole into a names frame (4 GiB reserve, leaked for the
      command). The fixture toolchain moved to `blaze_world::fixture::shelf` (one home: blaze-world's
      tests and blaze-cli's `tests::shelf`, written at `/h/.blaze/v2/toolchain`, start `/`). A
      probe that found no file is no longer listed as a `Surface` read (it made `NoPart`): owed,
      its absence as a read. The names note prints only when a path is unresolved.
      `mixcheck blaze-cli test` (tree 23cec98a1c, blaze-ladder from green): "37 passed, 3 failed".
    - STEP 2 test written: main.rs `a_signature_edit_in_a_binary_reruns_its_names` (cold > 1
      resolved, body 0, added `pub fn` 1): PASSES in that run; the planted-bug run (blaze-world
      `held` back to `Lib`/`ProcMacro` only) is NOT done yet: do it first and quote the line.
    - Left, in order: (1) `a_body_edit_reruns_no_surface_and_a_signature_edit_one`: a signature
      edit now runs 4 (outline, surface, the crate's `CrateScope`, and one more unnamed: name it
      from the profile, then set the expectation); (2) `a_planted_panic_fails_one_task` and
      `an_oversize_file_fails_its_task_alone_and_names_the_file`: the `CrateScope` wants the
      `Surface` of a file whose `FileItems` failed, and the drive answers `DriveError::NoPart`
      (request.rs ~2039) for the whole request: a want whose home settled FAILED must fail the
      wanter (typed), not the request; (3) the coordinator's "false pass" step ("incomplete"
      summary, exit 3; body syntax; unresolved own paths as errors) comes right after (2); (4)
      the 25 ms `pub fn` question; note the names step now READS std's packs whole instead of
      mapping them: re-measure body and `pub fn` edits, the world should not be rebuilt for one
      crate. The shared store (step 4) moved to the "Shared cache" lane.
    - RUNNER 4 2026-11-16 (uncommitted; handing on). blaze-report: "check ok (all targets)",
      its tests "39 passed, 1 failed" (the tree-wide backticks ratchet, other crates' rows).
      blaze-cli test (mixcheck, 10:2x) compiled with all below: the panic and oversize tests
      now ANSWER (no `NoPart`); their fixed runs printed "FileItems ... made 1", "CrateScope
      ... made 1 ... entries 4 needs 3": numbers set to 6 with the reason; the signature edit
      printed "CrateScope ... entries 2 needs 1": the 4th run is the crate's second entry,
      number set to 4 with the reason (re-run owed for the pass line). Still red:
      `creating_a_missing_module_file_clears_its_error`: "Drive(NoFile { file: FileItems,
      home: FileItems })": a missing `mod x;` file fails the WHOLE request in the drive
      (request.rs `advance`, `Locate::File` arm, `State::Settled` home -> `NoFile`): make it a
      finding of the home's (no request error), then the absence read is testable;
      `a_clean_run_says_nothing_of_its_own`: "blaze: machine store: 6 records found ..." is
      printed by store.rs (the Shared cache lane's; tell them). blaze-drive's own test run was
      not reached (build lock). Written:
      (1a) request.rs `advance`: a part whose home is Settled and in `failures` settles failed
      `Why::Waited { on: home }` (no `NoPart`); sim.rs `World::failing_file` + test
      `a_failed_file_fails_its_surface_and_its_readers_not_the_request` (first run without the
      sim's Signature `task.failed` check looped: "Signature(..) was entered 32 times"; it now
      answers `Waited` like `crate_scope`). The without-the-fix failing line is still owed.
      names.rs `run`: ANY `task.failed` need fails the crate `Waited` (was only `CrateScope`).
      main.rs `draw`: `Waited` failures print as one note "N task(s) not run: each needs one
      that failed above"; test helper `Checked.failed` holds only causes, `profile` and `errors`
      added; the three red tests print `profile` on a wrong count (the `runs` counts ENTRIES:
      the 4th of a signature edit is likely the `CrateScope`'s second entry after its `Need`
      of surfaces; confirm from the printed profile, then fix the doc and number). The panic
      and oversize tests may now count 3 (the crate's names, never made): check the profile.
      (1c) names.rs `Recording.absent`: a probe under `/0/` that found nothing is a read of
      its `Surface` with `key::ABSENT`; test `creating_a_missing_module_file_clears_its_error`.
      (3a) blaze-report `Closing.coverage: Option<Coverage { ran, not }>`: never `ok` while a
      stage is missing ("incomplete · no errors in: … · NOT checked: …"); main.rs
      `CHECK_STAGES` (the one list), `check_coverage`, `INCOMPLETE = Status(3)`, `blaze
      explain [status]` (`STATUSES`). mixcheck builds blaze-cli against blaze-report's LAST
      GREEN snapshot: blaze-report must pass `mixcheck blaze-report test` first.
      Left: compile and run all of the above; (2) the planted `held` run; (3b) body syntax:
      plan = a `BodyCheck` part per fn body in `outline_file` (source = body text digest),
      run as `parser::Reparser::top_level_item().parse` (blaze-lower lib.rs `lower_body`'s
      entry) emitting `SYNTAX` findings item-relative; `builds(BodyCheck)` then true, so
      `left_out_note` must stop deriving "check types" from it; (3c) unresolved own paths as
      errors, "N paths wait on: <cause> (k)", and "names resolved for N crates" always;
      "pass on": scripts/src/bin/smoke.rs may expect `check` exit 0.
    - RUNNER 5 2026-11-16 (uncommitted). Seen run: "test blaze-drive: exit 0; 37 passed, 0
      failed"; with request.rs `locate`'s home-failed check turned off,
      `a_failed_file_fails_its_surface_and_its_readers_not_the_request` fails: "the request
      failed: a task wanted Surface(53450ed73c83) of FileItems(53450ed73c83), which does not
      list it"; back on, 37 passed. Written: (2) request.rs `absent_module`: a module part its
      settled home lists, opened and found at neither place, is a drive-made record of one
      finding (`MODULE_FILE_MISSING_RULE` 0xC0F7, both paths; `PATH_FILE_MISSING_RULE` 0xC0F8
      for `#[path]`), placed at the `mod` item in the home's file (`settle`: `absent`), keyed
      by the paths; words in blaze-report words.rs, Area::Drive in print.rs. (3a) main.rs
      `close`: the closing line and status in one function `command` and the test helper `ask`
      both call (`Checked.status`); `CHECK_STAGES` splits "syntax of items" (built) from
      "syntax inside bodies" (not); tests `a_missing_module_file_is_one_finding_at_its_mod_line`,
      `a_clean_check_says_incomplete_and_exits_3`. blaze-report `closing` puts "NOT checked"
      before "no errors in": a narrow line dropped the NOT list ("incomplete · no errors in:
      manifests, syntax of items, names", seen); now "incomplete · NOT checked: syntax inside
      bodies, types, borrows". Runs (mixcheck: blaze-proc-macro script.rs:446 E0277 is
      broken underneath): "mixcheck test blaze-cli: exit 101; 46 passed, 1 failed", the one
      `records_outlive_the_packs_merge` ("a repeat after the merge: certified false, 2 runs":
      packs are store.rs, likely the Shared cache lane's; not proven). absent_module off: both
      missing-module tests "Drive(NoFile { .. })"; on: pass. `held` planted to libraries only:
      `a_signature_edit_in_a_binary_reruns_its_names` "left: 0"; restored: passes.
      blaze-report test: one failure, `no_crate_has_more_of_stds_print_macros` (other crates'
      rows in tests/prints.txt). Left: (3b), (3c) below; a release smoke run. Pass on (not this lane's files):
      scripts/src/bin/smoke.rs:45 and :163 (`Step::Check` expects exit 0; now 3);
      docs/terminal.md:162 ("one `ok` or `failed` line always").
      (3b) plan, not written: a `BodyCheck` part per fn/const/static item with a body in
      work.rs `outline_file` (name = file subject + the item's ancestor chain of kind, name,
      disambiguator; `Locate::Source(digest(item.hash, edition))`); its task reads the home's
      file (`Request::file_of`) and finds the item by `task.place` in the home `FileItems`
      record (a `Need` of the home), parses `parser::Reparser::top_level_item().parse` (the
      entry of blaze-lower `lower_body`; add `parser.workspace` to blaze-cli), and emits SYNTAX
      findings item-relative (lexer errors by `text_range`, parser `Step::Error` at the end of
      the last taken token, trivia skipped as blaze-file-items walk.rs `skip_trivia`). Then
      `builds(BodyCheck)` true and `CHECK_STAGES` "syntax inside bodies" true; `left_out`
      still names "check types" while `Signature` is unbuilt (one stage, any kind short).
      `CHECK_STAGES` and `work::left_out` are two homes of one concept: merge them.
      (3c) plan, not written: names.rs `run` counts `done.names.notes` (blaze-scope resolve.rs
      `NamesNote`: Lower, Root, Expand, Made, ExternDropped ..) into one u32; classify each
      note instead: `Expand` of a call not run (which macro) and a path under a build
      script's `OUT_DIR` / `env!` not given (which script) are waiting classes, each counted by
      cause; every other unresolved path in a project crate is a SYNTAX-like finding of the
      crate's record (`made.find`, its item-relative place from the note's `Loc`). main.rs
      :732 "names resolved for N crates; N paths unresolved by known gaps" becomes "N paths
      wait on: <cause> (k)" per cause, and "names resolved for N crates" moves to the profile.
      Test: `use nothing::here;` is one error naming the path and line.
    - RUNNER 6 2026-11-16 (uncommitted; handing on). (3b) written: blaze-cli work.rs
      `outline_file` lists one `BodyCheck` part per fn/const/static with a body under no other
      body (`checked_body`), named by `body_name` (file subject + the entry's chain of kind,
      name, disambiguator), `Locate::Source(item.hash)`, place = the entry's start; the
      outline's own errors inside those bodies are dropped (one home per error). The task
      (`body_syntax`, pooled like `FileItems`; `Job.body`) reads the home file
      (`Request::file_of`), finds the entry at its place, parses its text with
      `parser::Reparser::top_level_item().parse` (blaze-lower `lower_body`'s entry; `parser`
      added to blaze-cli), and makes each lexer/parser error inside the body a SYNTAX finding
      relative to the entry; an error within one token of the one before is recovery, dropped
      (`CASCADE`). blaze-drive request.rs `emit`: an item's task gets its home file's edition.
      One home: `work::CHECK_STAGES` (moved from main.rs) is derived from `builds` / `whole`
      (`whole(BodyCheck)` false: a body's task parses only, so "types" and "borrows" stay
      NOT checked); `left_out_note` says nothing for `check` (its closing line is the place);
      for `run` the worker's part is named by `work::does` ("check the bodies' syntax").
      Seen: "src/lib.rs:3:12: error[syntax]: expected expression" (before CASCADE a second
      "src/lib.rs:3:14: error[syntax]: expected SEMICOLON" followed it); "NOT checked: types,
      borrows". Tests: `a_syntax_error_inside_a_body_is_one_error_at_its_line`,
      `an_edit_of_one_body_reparses_that_body_alone` (profile `BodyCheck made` 2 then 1);
      run counts of four older tests raised by the bodies they now run.
      Smoke (scripts/src/bin/smoke.rs): `Step::Check` expects `CHECK_STATUS` 3 and an
      "incomplete" line; docs/terminal.md's closing-line sentence says what is true.
      (3c) NOT written; what was found for it: an unresolved `use` is not a `NamesNote` but
      a module scope error: blaze-cli names.rs `run` has `done.names` (blaze-scope
      `CrateNames`): for each `done.names.done` value `r`, `r.scope.errors` holds
      `ScopeError::Import { site, path, segment, why }` (and `::Macro`); its place is
      `done.names.tables.use_locs[site.expansion.unwrap_or(r.scope.module)][site.index]`, a
      `Loc { file, offset }` into `done.names.files` (prior art: blaze-world examples/names.rs
      `count`, ~line 960, which also sorts causes: a proc-macro crate, an `OUT_DIR` include
      whose build script did not run, `cfg_if!`). The hard part is the place: a `CrateScope`
      record has no text file (print.rs: "its name is its place"), and its key reads
      `Surface`s, which hold no positions, so a file offset in its record goes stale on a
      body edit. Plan: the finding names the path and the module path in its words, and its
      place is item-relative to the `use` item, printed through that file's `FileItems`
      record (find the item by name chain like `body_name`), or the error becomes a part of
      the file's own task. Then main.rs's "N paths unresolved by known gaps" note becomes
      "N paths wait on: <cause> (k)" per named cause.
      Runs: "mixcheck test blaze-cli: exit 101; 48 passed, 1 failed (against green:
      blaze-hash)" (lanecheck: blaze-typeck infer/facts.rs broken underneath); "scripts: check
      ok (all targets)". The one failure is still `records_outlive_the_packs_merge`. Experiment:
      with the machine store off (main.rs `Machine::of(.., home.filter(|_| false))`, since
      restored) it still fails, "certified false, 2 runs": NOT the Sharing seam's, so not the
      Shared cache lane's by that test; no message sent. Its repeat's profile (message now
      carries it; mixcheck cuts it) starts "UnitGraph standing 1 / FileItems found 1 / BodyCheck
      standing 1..": a file's record is FOUND, not standing, on a no-edit repeat after twelve
      edits: look at the project store's pack merge (store.rs `PackStore`, the Shared cache
      lane's file) against the drive's settled rows next; run the test alone, print the
      profile in full. Left: (3c); this test; step 5 (toolchain shelf through
      `Fs::home(cx, home, &Path::new(SHELF))` in toolchain.rs `HomeShelf::open`, dropping
      `At::Above` and the `start` argument at its two callers, graph.rs:198 and names.rs:207).
    - RUNNER 7 2026-11-16, step 1 (a bad import). names.rs `unresolved`: each
      `ScopeError::Import`/`Macro` of a crate whose files are the checkout's (`/0/`) is a
      finding `UNRESOLVED` (Cli 14, "unresolved": "no `x` in `a::b`") of the `CrateScope`
      record; its range is the failing segment's, RELATIVE to the item holding the `use`'s
      `Loc` (work.rs `item_holding`), and its message carries the file, the item's chain digest
      (work.rs `item_chain`: kind, name, disambiguator up to the file, as `body_name`) and the
      edition. The printer (main.rs `draw`, names.rs `placed`) outlines the file as it is and
      finds the item by its chain: a body edit above moves the printed line, not the record.
      A dependency's unresolved paths join the one counted note. Approximate: the segment is
      found by its text in order inside the item (a `use` tree with a repeated name may pick
      the first). Scopes are a map: findings are sorted before the record.
    - RUNNER 7, step 2 design (the types stage). A `BodyTypes` part per checked body (the
      `BodyCheck` body's name under its own kind), keyed by the body's item text and edition and
      read-listed by what its check read (the front end's read list; first cut, coarser and
      still correct: every `Surface` of its crate and the toolchain's id). Its task calls ONE
      entry of blaze-ladder front2 over a world made ONCE a command from the `CrateScope`
      records (never `resolve_crates` from scratch: measured "names resolved in 36.65s" on the
      owner's hello world through the ladder's own path). Its record: each checker diagnostic
      a finding (`TYPES`, the checker's code and text, range relative to the fn item), or a
      GAP with its typed reason. `CHECK_STAGES` "types": checked for the bodies with a record
      and no gap; each gap is listed "not checked: types" with a count and the first three
      reasons; never a pass. Blocked on the entry (asked of the RUN lane, 11:0x): a front2
      world from records, and `check_one(world, body item) -> {findings, gaps, reads digest}`.
    - RUNNER 7 (uncommitted; handing on). Step 1 seen: "mixcheck test blaze-cli: exit 0; 1
      passed" for `a_bad_import_is_one_error_at_its_segment` (prints "lib.rs:2:12:
      error[unresolved]: no `nope` in `crate`", status 1; a body edit above moves it to 4:12 with
      `CrateScope made 0`; the fix clears it, status 3); with the record's findings filtered out
      it fails "left: (0, Some(3))". NEW RED it exposed: `creating_a_missing_module_file_clears_its_error`
      (`mod x;` + `pub use x::f;`, then create src/x.rs): the created run's profile has NO
      `CrateScope` row, so the crate's record stood and its stale unresolved `x::f` stays: the
      `CrateScope` read list does not hold the absent `src/x.rs` (names.rs `Recording` lists
      `absent` only for probes through `/0/`; check what path the missing `mod x;` probe uses).
      A real invalidation hole, invisible while the record carried no findings: fix it first.
      Step 2: design above; the RUN lane (now a6703adf2352e6b0f, relay 24) asked for and was sent
      the exact signature (`ProjectWorld::new(names, user, alloc)`, `check(item) -> BodyVerdict
      { diagnostics (item-relative), gaps, refused, reads }`) and told the blocker: a front2 world
      from the `CrateScope` records, not `resolve_crates` (36.65 s). No code yet.
      Its answer (written, NOT compiled: blaze-typeck mid-batch): blaze-ladder
      src/front2/project.rs `ProjectWorld::new(names, frame, user, alloc)`, `bodies(&self,
      alloc) -> AVec<(BodyRef, Option<BodyAt { file, start }>)>` (use it to map a body to its
      file and item start; None = macro-written), `check(&mut self, body, alloc) ->
      BodyVerdict { diagnostics {code, text, start, end} relative to the item text, gaps
      (world_items::Gap, Display), refused: Option<Refused> (text()), reads: Option<ContentId>
      (Some only for a clean body) }`; test tests/front2_project.rs. NAMES FROM RECORDS: not
      possible today: `Names::of_crates` needs each crate's in-memory `CrateNames`; no decoder
      makes one from the `CrateScope` exports + inner records. That decoder is the missing piece
      (Names library row, main, paused): ask main for it; until then the types stage can only
      run by resolving every crate from scratch, which is not shippable.
      Step 3: NOT the merge: with `0..2` edits (under `PACKS_MOST`) it fails the same; a second
      repeat fails the same ("certified false, 2 runs"), so every repeat after a signature edit
      of m3 re-finds m3's `FileItems`/`Surface`/new body by key (not standing) and re-enters the
      project's `CrateScope` (entries 2, needs 1, made 0). Suspect: m3's stamp is racy in the
      simulation (sweep.rs `Stamp::settled`) and stays unstamped, or the edit command's settled
      rows for remade jobs are not kept; next: print `again`'s leaf row for src/m3.rs.
      Step 4 written: toolchain.rs `HomeShelf::open(cx, fs, home, frame)` opens
      `Fs::home(cx, home, SHELF)`; `At::Above`, `Ancestor` and `start` gone at graph.rs and
      names.rs; its test publishes under `u/me/` (the simulation's home). Suite: "mixcheck test
      blaze-cli: exit 101; 50 passed, 2 failed (against green: blaze-typeck blaze-lower
      blaze-ladder)": the two above.
    - RUNNER 8 2026-11-16 (uncommitted; handing on). BUG ONE cause: the CrateScope record DID
      list `(Surface src/x.rs, ABSENT)`, but the answers table kept a reader only on a name
      with a row (request.rs finish: `let Some(old) = self.old(target) else { continue }`), so
      the absent name's readers were dropped. Fix: a WATCH ROW (answers.rs `Row::watching`, zero
      key, readers only) for a name read absent or dropped; `old`/`current` hide it,
      `dirty_readers` and the new row's readers use it (`old_or_watch`); emptied, it goes.
      Then: a read of ABSENT is no hint for the next run (`to_run`), else the absent name's
      job fails and its reader fails `Waited` every run. names.rs `slot_of`: a read of
      `x/mod.rs` is the slot `x.rs`'s Surface (the drive names module files by slot; before,
      a crate with `x/mod.rs` failed `NoPart`). A want of a part its settled home does not
      list fails alone (`Refused`), not the request. BUG TWO cause, from the rows (hint
      plant on): the repeat's m3 leaf "stamp Some(..) .. was (0, [127, 7, 74, 128]); changed
      true" (the stamp settled; the table still held the COLD id) and the scope "failed
      [Surface 0225.., ..]": the hinted absent `mN/mod.rs` Surfaces failed, the scope failed
      `Waited`, no leaf table or certificate was written, so every repeat walked. Not the
      racy stamp. Racy rule (unchanged, sweep.rs `look_at`): an unsettled stamp is stored as
      none, the file is read again next sweep and its stamp kept once settled: at most one
      extra read per file changed within the granularity before a sweep (sweep's own
      property `the_oracle_catches_a_racy_stamp_trusted`). Property: main.rs
      `edits_answer_as_a_fresh_home` (findings, errors, status equal to a fresh home after
      each edit; an unchanged repeat runs 0 tasks); plants `ForgetAbsentReaders`,
      `HintAbsentReads` caught. OPEN (found by it): (a) module at BOTH places: home says an
      extra "no `x` in this scope" vs fresh (tape '0 1 0 2'; edits now keep the two places
      exclusive); (b) create x/mod.rs, edit lib.rs, delete it: `NoFile` (seed 245, tape '0 2 0
      2 0 6 0 3'; controls at 200 cases until fixed); (c) tests/run_output.rs
      `a_clean_run_prints_only_its_program` fails "3-hello: a clean run says nothing", left
      "note: names resolved for 15 crates; 2 paths unresolved..": check whether it predates
      this (git stash is banned: check out the commit in a scratch worktree). Not done:
      `#[path]` absence (resolver probes go through `Recording` so they are recorded; untested),
      target roots, include!, build.rs, members; step 4.
    - RUNNER 9 2026-11-16 (uncommitted; D70). Ids: blaze-cli fix.rs `Id`, a digest of the path,
      the item's chain and text (the innermost outline item, read in `ITEM_GRAMMAR` for every
      file so the id is text alone; a non-Rust file's item is its line), the code and the
      item-relative range; alphabet `23456789abcdefghjkmnpqrstuvwxyz`, 5 shown, longer by the
      shared prefix + 1 against its sorted neighbour. Views: blaze-report frame/plain.rs
      (`--view person|agent|json`, default by the one terminal test in `command`). `--fix`:
      main.rs `fix_then_check` (quiet check, `fix::apply`, the check again); writes through the
      `fix::Writer` seam (`Atomic` = synced publish; `InPlace` the planted bug). DRIVE BUG
      FIXED: a part whose record stood kept its findings' place from the last certificate, so
      an edit above printed them at the old offset (request.rs merge: re-placed from
      `part_locs`). FOUND, NOT FIXED: two unnamed `use` items share an item chain, so the
      second's unresolved finding is placed at the first (names.rs `placed`, work.rs
      `item_chain`; the property test draws one `use` until then). Unresolved-import fix: left;
      the names world has no scope's names with their text (typeck.md owes the question).
      `fix::checker_findings` waits for the TYPES stage (comment at main.rs `draw`).
    - RUNNER 10 2026-11-16 (uncommitted; handed on early, out of context; tree 3f8b478055
      dirty). Task: `blaze check` and `blaze test` in blaze's own checkout (198 crates) answer
      correctly and fast. DONE, one piece: `test` starts no program. request.rs
      `starts_program(command, plant)` is the one home (`run` alone); the walk's class asks it
      too. sim.rs: the commands property now refuses a `test` that started a program or
      answered as one; `Plant::TestStartsProgram` is the old behaviour, caught by
      `control_a_test_that_starts_a_program_is_caught`. Seen: "test blaze-drive: exit 0; 38
      passed, 0 failed" (37 before), "blaze-cli: check ok (all targets)". blaze-cli's `close`
      already prints "tests are not run yet" and returns FAILED for `test`; NOT yet seen on a
      real run (no release build of this tree was made).
      WHERE RUNS GO. Never in /Volumes/Projects/blaze: a run writes `<checkout>/.blaze/v2/last`
      under its own tool id and knocks out the installed blaze's standing answer there. Use the
      clone: `cd /Volumes/Projects/blaze-int-1112-notes/port27/drive-1116/main-clone` (APFS
      clone of the main checkout at 10ca641a15 dirty, `target/` cloned too: 374,012 entries)
      with `HOME=.../port27/drive-1116/home` (a clone of the toolchain shelf; the machine
      stores `own` and `shared` start empty). The 11:10 build is kept beside as `blaze-1110`.
      DEFECT, not fixed (the coordinator's order: after items 1 to 3): `last` is one file at a
      fixed path holding one tool's answer, so two builds in one checkout replace each other's
      certificate. A record is valid by its key and the tool is in the key: the pointer should
      be found by the tool's id (`.blaze/v2/last/<tool id>`, or the pointer a record under a
      key of tool + checkout), each written whole, the old ones swept by age.
      DIAGNOSIS SO FAR of the check (11:10 build in the clone, load 12 to 17): cold home,
      "42.79 real 133.99 user", 811 MB peak RSS, exit 101: "blaze_own: the allocator refused
      2850816 bytes aligned to 8 for `blaze_drive::request::Effect<'_>`" (raw.rs:170). A cold
      home FETCHED 291 crates from the network into `.blaze/v2/sources` (161 MB) before it
      died. Second run (sources present): "75.17 real 118.63 user", the same panic. No
      profile is written by a command that panics, so the phases line does not exist for
      this run: the first job is to make it exist (below). From `sample` (14,143 samples of the
      main thread, port27/drive-1116/clone-check-1110.sample.txt): 12,736 in
      `work::run_batches`, of them 9,415 parked in `Task<Pooled>::join` while 18 workers
      outline files (lexer, parser, `blaze_file_items::skeleton`), 3,303 running a pooled job
      inline on the main thread; 849 in `PackStore::put`, 832 of them `memmove` (the pack's
      buffer grown by copying: store.rs, the store lane's); 137 in `Request::step`.
      WHAT FILLS THE COMMAND'S 64 MiB FRAME (read, not yet measured: measure before fixing).
      The root frame is bstd's `MAIN_SCRATCH` = `TASK_FRAME` (blaze-std entry.rs:42). Into it
      go, for the whole command: (1) main.rs `effects: Vec<Effect, Alloc>` and `todo:
      VecDeque<Effect, Alloc>` over `alloc` = the ERASED COMMAND FRAME (main.rs `erased`): a
      bump frame keeps every outgrown buffer, and the refusal seen is this vector's growth;
      (2) work.rs `job_of`: every `FileItems` and `BodyCheck` task's path copied into the
      command's frame (one copy per BODY); (3) `answer_batch`: a failed task's path and words;
      (4) `Settled::grown_in(frame)`: each growth leaves the old table; (5) `run_one(..,
      frame, ..)` hands the command's frame to every task run on the main thread (`UnitGraph`,
      `CrateScope` in names.rs: `Facts::names: Option<Names<'f>>`, the graph, the layout,
      fetch). Each is per-task or per-batch work held for the command's life, against
      "memory is held by lifetime".
      THE FIX, in order: (a) the profile is written when the command fails too, and `Phases`
      gets the command frame's bytes per phase and per effect kind (`frame.used()` deltas at
      each `mark`, around `req.step`, around `run_one` by task kind): then quote the rows;
      (b) `effects`/`todo` move to the named global allocator like the machine itself
      (`Request::planted_in(.., Alloc::global())`: what it hands out is freed once
      performed); `job_of`'s path lives in the batch's frame or is borrowed from the request
      (`req.file_of`) for the spawn; tasks on the main thread get the scratch and batch
      frames only, and what a `CrateScope` keeps for later tasks is its record; (c) the class
      "an infallible push into a bounded frame": blaze-cli has hundreds of `Vec<_, &Frame>`
      pushes into the command's frame; close it at one seam, not per call: the request's
      state takes a `Kept<'f>` handle whose only growth is `try_` (a typed
      `CliError::FrameFull { frame, doing, asked, used, limit }`), the bare `&Frame` no
      longer passed below `request`; until then the command's edge has no `catch_unwind`, so
      a refusal on the main thread is a panic with a backtrace (tasks have one: `run_one`).
      NOT DONE (items 1, 3, 4, 5 of the brief): program.rs `run` still calls `root.list`
      (whole depth, the `target/` walk): hand it the rows' slots as `program_refusals`
      (main.rs) already reads them, the package from the units record, a typed refusal for a
      workspace; no timings taken with a build of this tree; `tests/run_output.rs
      a_clean_run_prints_only_its_program` (the "names resolved for .." note is printed by
      main.rs `request` after `draw` whenever `names.unresolved > 0`: on `run` it goes to the
      error stream with the findings; decide there whether a clean run says it).
      TWO DESIGNS ARRIVED AFTER THE HAND-ON (read both before item 3; neither is built):
      the parallelism architect's, parallel.md "End state (architect, 2026-11-16)" points 5
      and 7: the command's root frame takes `FRAME_RESERVE` (4 GiB of address space,
      blaze-mem frame.rs:64), not `TASK_FRAME`, its peak reported in `HighWater`; `Job` names
      its file by index into the `files` table instead of `job_of`'s copy; growth on the
      command frame through blaze-own's `try_*` forms with a typed error naming the frame
      ("command"), the bytes asked, its peak and reserve; a pooled task's refusal a typed
      `FrameFull { used, limit, asked }` in `pooled`, not `Why::Panicked`; the test: a few
      thousand generated files with the command frame made small by a `Sizes` plant finish,
      and with the per-task copy put back fail with the typed error. And the pipeline
      architect's build order for this lane's files, "End state: the whole pipeline on the
      command" below, "Gap" and "Pieces" (1 MEMBERS, 2 POOLED SCOPES, 3, 6, 7, 9, 11, 13; 14
      any time after 2).
- Owed, not now: `run` after `check` writes its own certificate, because the command is in
  the key. That costs 3.5 ms against the second run's 1.0 ms on hello-world. A coarse proof
  covers all under it: `check`'s certificate should stand for `run`'s check part.
- Known gaps: `LateWant` in `Phase::Held` for items that stood unwalked (the commands property
  allows it by name); a frame refusal in std's `Vec` (below).
- Owed, blaze-format's lane: crates/blaze-format-core/src/writer.rs:93, `WriteBuffer for
  alloc::vec::Vec<u8, A>`, grows through std's infallible `extend_from_slice`. Over a task's
  frame (blaze-file-items' `to_file_items` writes its record so), a full frame reaches
  `handle_alloc_error`, which aborts the process. Make the growth fallible, so a full frame is
  a typed error. Until then, work.rs `run_one` sets the one alloc-error hook, which turns that
  refusal into a panic the task's `catch_unwind` takes (test
  `an_oversize_file_fails_its_task_alone_and_names_the_file`).
- Owed: tasks on a pool across cores (one thread today); `blaze_lint::Units` as record tasks,
  step from the manifest, default `rust`; one counted dependency finding; get_many; the
  upgrade note once a command; conditions (piece 3); a test that a workspace past one frame
  checks and a planted oversize item fails one task.

## End state: the whole pipeline on the command (architect, 2026-11-16)

The finished command: `blaze check` in a real workspace runs every stage as tasks of the request
machine, in parallel, each result a record; `run` and `test` run the same tasks for what their
program reaches. Stands: "The request machine", "Names, sources and keys", "Records and
validity" (value keys, the answer table and its readers, the forward check), "Tasks and Need",
"Scheduling on many cores", "One check covers every configuration", "The gate". Key =
H(tool, kind, subject, source); the source is text known before the work; no key holds a path.

| Kind (grain) | Source | Reads (read list) | Record; out digest |
|---|---|---|---|
| `Units`, `UnitGraph` (request) | manifests, lock, target, ask | `Fetch`'s manifests | units, cfg, deps; its bytes |
| `FileItems` (file) | content id, edition | none | outline; per item a names, a signature and a body piece digest; slots; syntax findings |
| `Surface` (file) | its names pieces' digest | none | none; out = source, so a body or a type edit leaves it |
| `CrateScope` (crate) | the unit's identity row | its `Surface`s, deps' exports, its `ScriptRun`, `Expansion`s | exports, inner, names and items records (item at file + chain + piece digests; impls by resolved (trait, self head)); out = exports' digest |
| `Expansion` (call) | expander code id, tokens, attribute, edition | the macro's module | tokens; made in place on the asking worker |
| `Signature` (item head) | crate identity, item key, signature piece | its paths' name answers, aliases' heads | lowered head with lifetimes; its digest |
| `BodyCheck` (fn, const, static body; its closures) | crate identity, item key, body + signature pieces | each fact asked: names, heads, buckets, const values, Copy/Drop | types; findings and refusals, item-relative; made facts; out = made facts' digest (empty for most) |

- An impl bucket is a fact, not a task: (trait, self head) is answered from the items records of
  the crates the orphan rule allows (RFC 2451: the trait's crate and those of the nominal types
  in the trait reference; inherent: the self type's crate) plus their unresolved rows. A body's
  read list names each (crate, bucket) it asked, so an impl added elsewhere moves no body.
- Types and borrows are one task: the flow is lowered from the typed body in the same frame.
- No world in memory (rule 70): `CrateScope` reads its own crate and its dependencies' exports
  by key; `BodyCheck` opens records as its questions reach them (`front2::source::Records` over
  the store) in its worker's frame. Peak: workers times the frame cap, plus mapped pages.
- Parallel, deterministic: batches of one kind; needs answered in place (settled table); a
  record is a function of its key and answers, so any worker or machine writes equal bytes; the
  answer is the sorted merge of rows; findings are printed by (path, item start, range, rule).
- **Repeat**: the certificate, one read, no task. **Body edit**: `FileItems` of the file runs;
  its `Surface` and other parts stand unrun; one `BodyCheck` runs; its out is unchanged, so no
  reader is dirtied: two tasks. **Signature edit** (types, not names): `FileItems`, then the
  item's `Signature`; its out moved, so its readers in the answer table run: its body and N.
  **Name edit**: the `Surface` moves; its crate's `CrateScope` runs; an equal exports digest stops
  there; bodies whose name answers moved run. Re-solving the whole crate is over the floor until
  `ModuleScope`/`Group` take the solve at module grain.
- **Errors before the answer**: jobs dirtied by changed files are queued first; an error-class
  finding is shown as its record settles (`Effect::Found`, a live line in the person view on a
  terminal); the answer prints the full sorted list, the same bytes whatever the settle order.
- **A form a stage cannot take** is a refusal record of that item (`Why::Refused { what, rule }`,
  the item named), filed at its key, so a repeat does not retry it and a new tool does. Its
  readers fail `Waited` naming it. The closing line counts refused items per stage with the
  first three reasons; the status is 3 (incomplete); `ok` only when every `CHECK_STAGES` stage
  ran on every item. No fallback; a panic fails its task alone.

**Floors** (rule 67). Measured inputs: tempo's outline 194.7 ms for 816 files and its 52
members' names 0.57 s, one thread (tempo.md rows c, e); a body check 1.56 ms mean (31.2 s for
20,000, one thread) and a head 65 µs (2.4 s for 36,707), tempo-front-9.txt; a question through a
names record 0.13 to 0.45 ms cold. blaze: 5,181 files, 2.37 M lines under crates/ (git ls-files),
5.8 times tempo's 409,322, so about 87,000 bodies (estimate). Floor = work / cores + critical path.

| Operation | tempo | blaze | Derivation |
|---|---|---|---|
| Repeat | 2 ms (13 ms today) | 3 ms | ~870 / ~5,800 stats on 8 cores + one certificate read |
| Body edit | 4 ms | 4 ms | sweep + one outline (0.24 ms) + one body (1.56 ms) + pages; design 6-10 ms (cold scope decode) |
| Signature edit, N readers | (N+1) x 1.6 ms / cores | same | the head + N bodies |
| First run, deps from the bucket | 1.8 s / 18 cores; 0.3 s / 128 | 13 s / 12-core Mac; 1.3 s / 128 | CPU 32 s (tempo), ~160 s (blaze): outline + member names + bodies + borrows (+30%, estimate) |
| First run, no bucket | + 29.5 s CPU of names | (today: 42.8 s, then a panic) | the graph's names once (tempo-front-9.txt), over the crate graph's levels |

The body check's own 1.56 ms is the type checker's floor question, not the driver's.

Others (match, then what blaze does better by design): rust-analyzer/salsa, early cutoff by value
(out digests), but its graph dies with the process. Buck2/Bazel, input-keyed actions and a
shared cache, but target grain and a daemon. Zig, per-declaration dependencies, held in one
process. Sorbet, parallel per-method checks and a fast path while the symbol table's hash holds:
`Surface` is that path. Hack, CI's saved naming state and fan-out by declaration edges: the
bucket's public records and the reverse index, no server. Flow types-first, bodies independent
given annotated exports: Rust's are written. TypeScript's Go port, checkers sharded over files
but a whole check per run: here an edit costs its bodies.

### Gap (code read 2026-11-16, tree 3f8b478055 dirty)

| Needed | Today (evidence) | Missing | Reached by |
|---|---|---|---|
| Names in a multi-member workspace | cli names.rs `world`: more than one path package is `NamesError::PathPackages` | each member placed at its directory (`blaze_world::Fetched::paths`) | the command (refuses tempo and blaze) |
| Scopes in parallel | work.rs `run_batches` pools `FileItems`, `BodyCheck` only; `CrateScope` runs in `run` on main, `resolve_crate(.., &Serial, ..)` | pooled, per-task frame | the command |
| No per-command world | names.rs `world`: every crate's inputs, leaked, `Alloc::global()` | per task, by key | the command |
| Build scripts' outputs | names.rs: `blaze_world::world(.., None, ..)` | `ScriptOutputs` from `ScriptRun` | the command (missing) |
| Names and items records | names.rs `run` writes exports + inner; `names_record::encode`, `items_record::encode` only via blaze-scope `Kept` hooks | filed in the `CrateScope` record | tests, blaze-world examples/names.rs |
| Item-grain text in the items record | items_record.rs `Seen::Stale`: a file's whole digest, so any edit voids its items | per item: chain and piece digest | tests |
| Body types | work.rs `body_syntax` parses only (`whole(BodyCheck)` false); `ProjectWorld::check` | the call, over records | tests/front2_project.rs, examples/tempo_front.rs |
| Front end over records | `Front2::from_records`; `ProjectWorld::new` takes `&Names` | `ProjectWorld` over `&Records` | tests/front2_records.rs |
| One body identity | cli `body_name` (file + chain) vs front2 `BodyRef` (LibKey) | a body found by (file, chain) | neither |
| Read lists as facts | `BodyVerdict::reads`: one digest, clean bodies only | sorted (fact, digest) pairs, every body | tests |
| Signatures as records | front2 heads.rs `Store`, in memory per command; `builds(Signature)` false | a head record, a task | examples |
| Impl buckets per crate | buckets.rs `Buckets::new` reads every impl header of the index | rows per crate, orphan-bounded question | examples, tests |
| Borrow check | blaze-flow-lower `lower`, blaze-flow | in the body task | tests/flow_census.rs |
| Errors before the answer | main.rs `Effect::Print(f) => prints.push(f)`, printed at the end | `Effect::Found` at settle | the command (at the end only) |
| Expansions kept | per-command `Prepares`; `builds(Expansion)` false | records by key | the command (redone each solve) |

### Pieces (build order; one writer each; the dependencies' architect is assumed to give: each
outside package's files through its `Fetch` record or `NotFetched` by name; `ScriptRun(unit)`
answering `blaze_world::ScriptOutputs::served`; public `CrateScope` and `Signature` records of
registry and toolchain crates found by key in the machine store or bucket; `MacroModule` records)

1. MEMBERS. cli names.rs `world`: every path package at its member's directory from the
   `Units` record. Test: two members, `b` calls `a::f`, no unresolved finding; `a::g` is one.
2. POOLED SCOPES (after 1). work.rs `run_batches`/`answer_batch`: `CrateScope` on the pool, its
   world read-only and `Sync`, each solve in its task frame. Test: a six-crate diamond gives equal
   records and findings at `--workers 1` and 4; a counter shows two solves at once.
3. NAMES AND ITEMS FILED (after 2, same file). names.rs `run`: append both encodings to the
   payload, offsets in the head; `sections(payload)`. Test: the record answers `crate::m::f`
   through `RecordNames` and `f`'s row through `RecordItems`.
4. ITEM-GRAIN ITEMS RECORD (any time). blaze-scope items_record.rs: a row carries its item chain
   and piece digests; `Stale` for that item only; front2 `Load::item` by (file, chain). Test: a
   body edit of `g` leaves `f` readable; an edit of `f`'s signature is `Stale` for `f` alone.
5. PROJECT WORLD OVER RECORDS (any time). front2 project.rs: `ProjectWorld` over a
   `NamesSource`, `from_records`, `body_by_chain(file, chain)` (the chain function moved to
   blaze-file-items, one home). Test: every rung body's verdict over records equals over `&Names`.
6. STORE LOAD (after 3). NEW blaze-cli front_load.rs: front2 `Load` over the store (sections by
   crate key from the `UnitGraph` parts; files from the checkout or the `Fetch` pack). Test: a
   body checked through it equals the solved path.
7. TYPES IN THE BODY TASK (after 4, 5, 6). work.rs `BodyCheck` arm: one `Records` per worker per
   command; diagnostics as `TYPES` findings; gaps and refusals as refusal findings; status 3.
   Test: `fn f() -> u32 { "x" }` is one error at its range, a line added above moves it with the
   record reused; an `.await` body prints "not checked: types" and never `ok`.
8. READ LISTS AS FACTS (any time). front2 check.rs: `reads` as sorted (fact, digest) for every
   body. Test: of two bodies, only the one naming `a::f` lists `f`'s head.
9. FACTS AS READERS (after 7, 8). work.rs/request.rs: each fact a drive name (a `CrateScope`
   section, a `Signature`), read-listed. Test: a signature edit re-runs only its readers.
10. HEAD RECORDS (any time). NEW front2 head_record.rs: one head encoded, decoded, round trip.
    Test: every head of the rungs round-trips equal.
11. SIGNATURE TASKS (after 9, 10). `builds(Signature)`; its arm lowers one head over records;
    a body's need of it answered in place. Test: a body edit runs 0 `Signature`s; a type edit 1.
12. BUCKETS PER CRATE (after 3). items_record.rs files impls by resolved (trait, head); front2
    `Buckets` asks the orphan rule's crates only. Test: `Display for a::T` opens `core` and `a`.
13. BORROWS IN THE BODY TASK (after 7; heads' lifetimes). Test: a use after a move is one error
    at its range; a refused form is "not checked: borrows"; `whole(BodyCheck)` true with both.
14. ERRORS AS THEY SETTLE (after 2). request.rs `Effect::Found` at settle, dirtied jobs first;
    main.rs a live line. Test (sim): `Found` precedes `Done`; the JSON view is byte-equal.
15. SCRIPTS IN (after 2; the dependencies' `ScriptRun`). names.rs `world`'s `ScriptOutputs`.
    Test: `rustc-cfg=foo` from build.rs turns on `#[cfg(foo)] fn f`.
16. EXPANSION RECORDS (any time). blaze-scope: an expansion kept by key through a `Kept` hook.
    Test: re-solving a crate after a name edit expands no unchanged derive again.
17. MODULE GRAIN (after 11). `ModuleScope`/`Group` take the solve; `CrateScope` assembles.

At once: 1, 4, 5, 8, 10, 16 (separate crates or files); then 2, 3, 12, 14 and 6; then 7, 15;
then 9, 13; then 11, 17.
