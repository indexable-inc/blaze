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

> Changed 2026-11-16 evening (D113; run.md, stage 4): `run` uses `Before`, and its scope is the workspace
> BODIES THE PROGRAM REACHES (the need-walk from `main`'s check), not whole packages; `Beside` is `test`'s.
> The text below is the seam as built; its default and its scope move with that piece.

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
    - RUNNER 11 2026-11-16 (uncommitted; handed on at the context limit; last build: snapshot
      9b16d92249, binary port27/drive-1116/blaze-r11-c). Runs: main-clone, the scratch home, no
      network, load 11 to 17.
      RESULT. `blaze check` on blaze's clone: "5.15 real 7.56 user 1.27 sys", exit 3
      ("incomplete: NOT checked: types, borrows", "0 errors"), 982 MB peak RSS; before
      (blaze-1354-step1): "53.95 real 130.23 user", a panic. CAVEAT: in the 5.15 s run NO
      `UnitGraph`, `Fetch` or `CrateScope` task ran (no such rows; the 13:36 build ran 8, 289
      and 549 of them, 1.1 s of main thread): another piece of the live tree between 13:36
      and 14:13 dropped the graph on this checkout: NOT investigated, the first thing to do.
      DONE 1 (the profile on failure, the frame's rows): main.rs `command` runs the request
      behind `work::unwinding` (the one home of `catch_unwind`; `CliError::Panicked`);
      `print::profile_failed` writes `stopped ..` and the phases in a frame of its own.
      phases.rs `mark_in`/`took`/`worked`/`times`: two `frame` lines (bytes by phase; bytes,
      times and main-thread ns by effect kind and task kind; pooled worker ns).
      MEASURED CAUSES (neither was what had been read): (a) the frame: "effect publish (the
      settled table) 45776916 114429x" of "64562474 of 67108864 bytes used": `Settled::
      grown_in(frame)` kept every outgrown table; then jobs' paths 4.2 MB, the effects list
      2.6 MB, the work list 1.8 MB. (b) the 120 CPU seconds: "pooled BodyCheck (worker ns)
      108606x 252125445249" against "pooled FileItems 4581x 2901540261": work.rs `body_syntax`
      read and outlined the whole home file once a BODY (sample: 6,539 of 8,463 worker samples
      in `blaze_file_items::outline`). Nothing for the outline's lane.
      DONE 2, part (by lifetime; NOT the typed seam): the settled table, `effects` and `todo`
      on the named global heap (an outgrown buffer is freed); a round's jobs' paths and body
      lists in `Frames::jobs`, reset when the round's scope has joined, grown by `try_`
      (`JOBS_FULL` fails the one task). After: "the command's frame: 15116635 of 67108864
      bytes used"; its largest row is "task Units 9387632 181x" (the layout, handed the
      command's frame by `run_one`: next). NOT DONE: `Kept`/`CliError::FrameFull`,
      `Why::FrameFull` at the job seam (parallel.md pieces 1, 3), footprint per task (7).
      NOTE: the command writes the settled table and nothing in blaze-cli reads it
      (`worker::answer_in_place` has no caller there): 21 MB and 42 ms for no reader; delete
      or wire it with the claims piece.
      DONE 4: a round's `BodyCheck` tasks are ONE pooled job a home file (work.rs `BodiesJob`,
      `Group`, `pooled_bodies`, `bodies_syntax`: read once, outline once, each body found by
      search and parsed; a body's panic fails that body). "pooled body jobs, one a file a
      round 4653x 3699235550", "pooled BodyCheck 108606x 3699184913" (252 s -> 3.7 s). Test
      `a_files_bodies_are_checked_by_one_job_a_round`: "1 passed"; without the grouping: "400
      file jobs for 400 bodies of one file". Still twice the floor on a first run (the file is
      outlined by `FileItems`, then again by its body job): the body job should take the
      item ranges from the `FileItems` record.
      WHERE THE 5.1 s GO NOW (main thread): "effect put 118207x 2362855830" (the store lane's
      `PackStore::put`), "pooled tasks joined 8064x 1255300337", "opens probed 203x
      785145917", "machine step 130596x 384067087"; workers 6.6 s over 449 rounds.
      DONE 3, the symptom not the design: `program::run` lists the package's `src/` alone,
      never the checkout. `blaze run` on the hello rung, fresh `.blaze` each: alone "real 0.05"
      and "real 0.02"; beside a 374,012-entry `target/`: "real 0.05" and "real 0.02"; the
      13:36 build beside it: "real 2.34", exit 1, "arena the packs' records: 15104 more bytes
      would pass its limit". WHY NOT THE WALK'S ROWS: under `Gate::Beside` the program starts
      before the walk has opened any file (tried: "the crate root `src/main.rs` is not among
      the files", rows 0 and 7). `program::sources` (the rows' slots under `src/`) serves
      `program_refusals` after the answer. Owed: the runner walks forward from its root
      through a loader.
      DONE 7: the names note is not printed after a program that ran. "mixcheck test
      blaze-cli: exit 0; 2 passed, 0 failed" (`a_clean_run_prints_only_its_program`,
      `a_clean_run_says_nothing_of_its_own`); before: "3-hello: a clean run says nothing".
      NOT DONE: 5 (the three timings: only the first run above), 6 (`last` -> `.blaze/v2/at/
      <hex16 of H(tool, shape)>`, 8 kept: read-addressed.md "End state", piece 1), the
      typed seam of 2. No change to the machine, so no new sim case.
      RED, NOT ATTRIBUTED (blaze-cli tests at 14:16, tree b1960bb662: "66 passed, 10 failed";
      no baseline was taken before this runner's edits): `a_planted_panic_fails_one_task`
      (the second check makes everything again: "Units 2 made, FileItems 41 made, BodyCheck
      3910 made"), `an_oversize_file_fails_its_task_alone_and_names_the_file` (green at
      14:07, red at 14:16), `a_bad_import_is_one_error_at_its_segment` (the finding printed
      twice: check now covers test targets), `a_body_edit_reruns_no_surface_and_a_signature_
      edit_one`, `a_signature_edit_in_a_binary_reruns_its_names`, three `fix_tests`,
      `members_tests::two_members..`, `refused_targets_tests::..`. Other writers changed
      main.rs:631, units.rs, sweep.rs `wanted`, names.rs, fetch.rs, graph.rs today.
      TEST TARGETS (the command-crates integrator a9e02845f61bde508, after the hand-on): since
      its 13:56 wiring a member with any `tests/*.rs` makes `blaze check` fail with
      `DriveError::NoPart` (`FileItems` of the test root; units.rs `member_units` lists only
      lib and bin parts); with main to choose a fix or a revert. NOT SEEN in this runner's
      14:13 run on the clone (exit 3, "0 errors"), where no `UnitGraph` ran either: the two
      may be one cause (the graph for `check` is made with tests since main.rs:631 changed).
      Start the search for the missing `UnitGraph` there.
      PROC MACROS (from PROC MACRO SPEED runner 2, aa75ce262a4184b21; not looked at by this
      runner, the call site in the command not searched): the command's path must call
      `procs.keep_all()` (`blaze_proc_macro::hook::ProcMacros`; `-> Result<KeptAll,
      RecordError>`) once its crates are expanded and before `take_findings()`: results are
      one table a crate written only there; without it a command keeps nothing and every
      later command runs every macro again. Its `Err` is a finding printed by name.
      THE JOIN (from ab1a42b2898694bd9, NEVER COMPILED, with the BUILD INTEGRATOR; do not
      switch before its "blaze-std: check ok"): `handle.join(&cx)` keeps its signature and
      now helps (runs queued tasks spawned from outside the pool while it waits); and a batch
      join in completion order: `scope.batch::<Pooled, _>(alloc)`, `.may_overlap(..) ->
      Result<u32, BatchError>`, `.join_each(&cx, |joined| ..)` (store `joined.result` by
      `joined.index`, step the request in index order after it: rule 25), and
      `scope.join_counts()` (`Display`) for a `phases` row. Its reason quoted the row from
      BEFORE this runner's fix ("pooled tasks joined 113209x 48174625328"); the row is now
      "8064x 1255300337": measure again before and after switching `run_batches`.
      THE `effect put` ROW (from the pack store's writer a5aadb06ee11bb5be, NEVER COMPILED,
      with the command-crates integrator): the 2.4 s was store.rs `put` inserting into a
      sorted index (n^2/4 rows moved); now rows are appended and sorted once at the seal,
      `Store::put`'s signature unchanged. OWED BY THIS LANE once it is green: re-measure
      `check` on the clone and send the `effect put` row to the coordinator (the writer
      expects tens of milliseconds for 114,429 puts: its arithmetic, not a measurement).
      It also asks for `CliError::Store(store::chunks::PutError)` in main.rs (today the
      refusal rides in `CliError::Remote(RemoteError::Put(..))`): tell it when added.
      FOR THE TYPED SEAM OF 2 (from a18ba224587a1dab7, 14:2x, UNCOMPILED, with the BSTD
      INTEGRATOR): `frame.table::<T>(most)?` -> `blaze_mem::frame::FrameTable` (its own
      reservation, `try_push -> Result<u32, TableRefused>`, never copies, never panics), or
      `blaze_own::Segs<T, &Frame>` where no single slice is needed; `frame.abandoned()`
      prints "abandoned by growth: N bytes in M growths". Once it compiles, the settled
      table, `effects` and `todo` (on the global heap since this runner) can move to it, and
      the profile's frame line can quote `abandoned()`.
      FOR THE TYPED BodyCheck (parallel.md piece 12): the RECORDS SWITCH writer
      (a6e38604c026598f8) sent the calls (`source::Records::new`, `Front2::from_records`,
      `body::lower_fn`, `check::check_body`, the log's count before and after a body).
    - RUNNER 12 2026-11-16 (uncommitted; handed on at the context limit; run files in
      port27/drive-1116/r12/; binary port27/drive-1116/blaze-r12-a = snapshot c587e87afe, which
      has steps 1's fixes and NOT step 2's).
      DONE 1 (the ten red tests; baseline "70 passed, 10 failed" at 14:24): after, "test
      blaze-cli: exit 101; 83 passed, 1 failed" (the one is the store's, below), "test
      blaze-drive: exit 0; 40 passed, 0 failed". Runner 11 read the profile's columns shifted
      (the label is BEFORE its number): nothing "reran everything". Three causes: (a) five
      tests: a library's test unit (`TestLib`) is a second `CrateScope` holding the same
      finding ("2 errors", "id gv7fz begins another"): request.rs `print` now returns (count,
      errors) and a crate's finding held by two crates prints and counts once; sim
      `one_finding_held_by_two_names_prints_and_counts_once`, control with
      `Plant::PrintEachName` ("the twins' finding printed 2 times"). (b) units.rs
      `member_units` listed only lib and bin roots as parts: any member with `tests/*.rs`
      was `DriveError::NoPart`; `member.extras` roots are parts now. (c) four stale counts,
      each test's doc says what changed (two crates' names: 106 -> 110, 16 -> 20, 1 -> 2; a
      parameter edit leaves the surface equal by names_digest.rs, and a new item reruns one
      `Surface` and two `CrateScope`s, 7 runs). `blaze build`: `CliError::NoSuchCommand`
      (process test `build_is_no_command_and_names_run_and_export` passed).
      STEP 2, THE CAUSE (read from the profile and the code, NOT yet shown by a run): nothing
      landed between 13:36 and 14:13. The 14:13 build ran over the 13:36 build's pointer:
      main.rs read the layout only when `cold || followed.layout_moved`; another tool's leaf
      table made neither true, so `laid` was `None`, `Start` got `graph: None`, the machine
      (not forward: `last.tool` differs) made the units again through work.rs's lazy `lay`
      ("Units made 181") and no `UnitGraph`, `Fetch` or `CrateScope` job existed. The same
      holds for another SHAPE's pointer (`test` or `run` before `check`) and after every
      upgrade. A cold run of blaze-r12-a (pointer and stores set aside) did reach the names:
      "error: a task wanted FileItems(8002d59876ff) of FileItems(8002d59876ff), which does
      not list it", "15.05 real 16.66 user", exit 1, 5.07 GB peak RSS.
      WRITTEN AND GREEN, 14:43 ("mixcheck blaze-cli: check ok (all targets) at 14:43; tree
      fde9505b0a", "mixcheck test blaze-cli: exit 0; 88 passed, 0 failed (against green:
      blaze-typeck blaze-ladder)": the store's streaming merge landed, `run_output` passes);
      NOT YET shown: test (iv) failing without (i) (the tree went red under the attempt:
      "test_loop.rs:897:1: error: couldn't find file `crates/blaze-cli/src/tests/
      test_loop_tests.rs`", another writer's), and no real run of a build that has them:
      (i) blaze-drive request.rs `shape_of(&Ask)` and `walks_from(last, tool, &shape)`;
      main.rs `request`: a pointer that is not this tool's and shape's is absent (cold);
      (ii) main.rs `Finished::names_held`, `check_coverage(names_held, ..)`, work.rs
      `NAMES_STAGE`: a whole walk with source files and no `CrateScope` row closes with "NOT
      checked: names, .." and exit 3; (iii) names.rs `Unlisted`, `Names::reader_of`, work.rs
      `answer_batch`: `NoPart` of a file a crate's names read becomes `CliError::Unlisted`
      naming the package and the path; (iv) tests `a_check_after_another_command_resolves_
      the_names` (must fail without (i)) and `a_check_without_names_says_names_were_not_
      checked`. NOT done: the lazy `lay` in work.rs `run` (Kind::Units) still loses the
      graph when a store lost the units record: make it loud or hand the graph late.
      THE STORE: `PackStore::merge` read every pack into one TASK_FRAME: "arena the packs'
      records: 1485760 more bytes would pass its limit" (`--test run_output`, real home, 8
      packs, 169 MB) and "15104 more bytes" (the scratch home's own store): every command
      in such a home exits 1. Sent to a5aadb06ee11bb5be; its streaming merge landed (14:43).
      AFTER THE HAND-ON, from the store's next writer (ac22ba2da3b1ba1cd; uncompiled, with
      the COMMAND-CRATES INTEGRATOR; not checked by this runner): `Layered::put` writes
      nothing when any layer holds the key; `PackStore::open_in` takes a `folder`; a layer
      merges past 8 packs or 1/4 duplicate bytes; the `run_output` process tests set HOME
      to a temp folder (this runner's `build_is_no_command_and_names_run_and_export` is in
      that file: see it still runs). Its cause for the repeated packs is the one behind
      step 2: `home::tool` (the binary's build id) is in every `task_key`, so each rebuilt
      blaze is another tool, writes every record again and reads no earlier pointer.
      SET ASIDE for the cold run (put back or delete): r12/aside/home-own (the scratch
      home's `.blaze/v2/own`), r12/aside/clone/{last,packs}; the 289 source records were
      copied to home/.blaze/v2/sources (the new place, D78).
      OWED BY DESIGN: `TestLib`/`TestBin` as crates of their own double each member's names;
      "One check covers every configuration" makes `test` a variable of one crate's
      condition. `--test run_output` reads the real `$HOME`.
      NOT STARTED: 3 (timings), 4 (`FrameFull`), 5 (the pointer a (tool, shape): it is the
      end state of step 2's fix: do it next), 6; queued from others: program.rs `Dir::walk`
      (limits folders 1 << 12, entries 1 << 16, depth 32), sweep.rs `facts_of`/`read_sized`
      (aeeae1de06257a7d3), `procs.keep_all()` before `take_findings` (a7696bf9081b5dc2b),
      the `effect put` row re-measured for a5aadb06ee11bb5be.
      FIRST THING NEXT: run test (iv)'s first test once without (i) (main.rs `request`: make
      the `walks_from` arm give `(last, leaves)`) and quote its line; greenbuild; in the
      clone run `check` cold and read which path `Unlisted` names, then a second build's
      `check` over the first's pointer and quote its `UnitGraph`/`CrateScope` rows; step 5.
    - RUNNER 13 2026-11-16 (uncommitted; handed on at the context limit; run files in
      port27/drive-1116/r13/: c1..c3 `.out`, `.err`, `.profile.txt`; binaries
      port27/drive-1116/blaze-r13-a, -b, -c, each a greenmix of the live tree: greenbuild.sh
      cannot build a mixcheck snapshot: "blaze-cli at 9544ef5819 did not build (exit 101)").
      Green at 15:17: "mixcheck test blaze-drive: exit 0; 45 passed, 0 failed" (tree
      2eda653e7d), blaze-cli's unit tests "97 passed" (tree 3cb7b2f0ee); the "1 failed" is
      `--test command_rungs` ("hello-blaze: REFUSED: error[program-refused] src/main.rs:1:14
      the type `Cx` names nothing"), the hello-world lane's, red before these changes too.
      DONE 1: the first new test without the `walks_from` arm: "thread 'tests::a_check_after_
      another_command_resolves_the_names' panicked at crates/blaze-cli/src/main.rs:1848:9: no
      crate's names in the answer:", rows "Units standing 2", "FileItems standing 1" and no
      `UnitGraph` or `CrateScope` row. Restored.
      DONE 2, two causes of an unlisted read, both fixed: (a) blaze-r13-a: "the names of
      package `blaze-diff` read `crates/blaze-diff/src/rules_gen.rs`, which no manifest's
      targets and no file's `mod` lines list": lib.rs:31 `mod rules_gen { include!(
      "rules_gen.rs"); }`. work.rs's outline task now lists an item-position
      `include!("literal")`'s file as a part (`included_file`, `in_modules`); test
      `an_included_file_is_a_part_of_the_file_that_includes_it` (its first form failed without
      the arm with "FileItems standing 0 found 0 made 1"; a simulated project has no `core`,
      so the names' side is shown by the real run only). Debt, in `included_file`'s doc: a
      `mod x;` line inside an included file is looked for beside it; rustc looks from the
      including module's directory. (b) blaze-r13-b: "the names of package `hir-ty-codegen`
      read `crates/hir-ty-codegen.rs`": the root is `[lib] path = "mod.rs"` and names.rs
      `slot_of` mapped every `x/mod.rs` to the slot `x.rs`. It now maps one only when the
      resolve also looked at `x.rs` (a `mod x;` probe); test `a_crate_root_named_mod_rs_is_
      read_under_its_own_path` (passes; NOT run without the fix). Owed: the texts interface
      (blaze-world `SourceTexts::read`) saying how it reached a file, so this is told.
      THE COLD CHECK NOW WALKS TO ITS END AND PANICS PRINTING (blaze-r13-c, tree 0326054d86
      dirty, load 12.86, r13/c3.*): "task Units 0 181x", "task UnitGraph 51376 8x
      218692375", "task Fetch 0 302x 107769037", "pooled FileItems (worker ns) 0 4704x
      3218957225", "pooled BodyCheck (worker ns) 0 110514x 3482701625", "task CrateScope 0
      3294x 35699494088", "effect print 655366 3487x"; then "thread 'main' panicked at
      crates/blaze-own/src/raw.rs:80:29: blaze_own: the allocator refused 51064 bytes aligned
      to 1 for `u8`" under `request` > `draw` > `print::listed`; "the command's frame:
      67024084 of 67108864 bytes used .. request 2528540 · stopped 55064979"; "39.64 real
      37.61 user", "13536428032 maximum resident set size", "546663010126 instructions
      retired", exit 1, nothing on stdout. So: drawing 3,487 findings takes 55 MB of the
      command's 64 MiB frame (nothing is given back a finding), and the names are 35.7 s of
      the 39.6 on the main thread. MEMORY: the profile has no row for it: its frame lines
      account 67 MB; the other 13.3 GB is outside every frame, in `Global` (names.rs `world`
      and `Names::made`, leaked: the Gap table's "No per-command world"). The 3,487 findings
      were never read (the panic came first): most are expected to be unresolved names.
      DONE 4 (the end state of step 3's fix; no real run of it yet): one pointer a (tool,
      shape) at `.blaze/v2/at/<16 hex of H(tool, shape)>`, 8 kept. blaze-drive NEW pointers.rs
      (`PointerName::of`, `evicted`, `KEPT`); `Profile::forward`; sim.rs `each_tool_and_
      shape_walks_forward_from_its_own_pointer` with `control_one_pointer_for_every_tool_and_
      shape_is_caught` (`Plant::OneName`) and `a_checkout_keeps_the_pointers_written_last`
      with `control_evicting_the_newest_pointer_is_caught` (`Plant::EvictNewest`): the four
      are in the "45 passed". blaze-cli seal.rs `At`, `write(.., at, fresh, ..)`, `evict` (one
      listing and one stat effect, only when the pointer was not there); main.rs `request`
      reads its own pointer; test `a_fresh_pointer_past_the_cap_removes_one` (passes; NOT
      run without `evict`). `walks_from` stays as the check of a pointer against its name. An
      old `.blaze/v2/last` is not read and not removed.
      ALSO: blaze-drive run.rs `code_name(function, tier)`: the hooks left the name (blaze-run
      `Effect::Compile` carries none; no caller); test `a_code_name_is_its_instance_and_tier`.
      `blaze export`'s wiring for main.rs is in its writer's message (ac46ae47065384afb):
      `export` stays `NotBuilt` until the run path gives the reached records.
      NOT DONE: 3 as a real run (two builds, the second's `UnitGraph`/`CrateScope` rows; by
      name it now reads no other build's pointer); 5 (the three timings); 6; 7; the queued
      items of runner 12's list; the `TestLib`/`TestBin` change as pieces; the lazy `lay`.
      STATE LEFT: the clone has `.blaze/v2/packs` from c3 and no `at/`; the scratch home's own
      store and r12/aside are as runner 12 left them. The COMMAND-CRATES INTEGRATOR
      a9e02845f61bde508 is stopped; lanecheck's live tree was red beneath blaze-cli all
      session (blaze-expand, blaze-build, blaze-file-items): every pass above is mixcheck's.
      FIRST THING NEXT: `print::listed` and `draw` (print.rs, main.rs): draw each finding in
      a scratch that is reset a finding (or stream it), so 3,487 findings fit; a full frame
      there is `CliError`, never a panic (step 6's typed refusal); then the cold check again
      with blaze-r13-c's successor for its closing line, then a repeat and step 3 and 5.
    - RUNNER 15 2026-11-16 (uncommitted; run files in port27/drive-1116/r15/; binary
      port27/drive-1116/blaze-r15-a, a greenmix of the live tree).
      DONE 1: printing without a panic. print.rs `list` resets `scratch.file` a file (it had
      been lost); `thousands_of_findings_are_drawn_in_frames_a_fraction_of_their_work` failed
      without the reset ("the allocator refused 360 bytes") and passes with it.
      DONE 2, the clone's three runs (blaze-r15-a; `/usr/bin/time -l`, loads 35.7, 47.7, 40.5):
      cold 49.66 s real, 487263959229 instructions, 13587120128 bytes peak; repeat 4.91 s,
      44604506101, 1134428160; one body edit (blaze-diff `differentiate`, still in the clone)
      8.01 s, 45575525092, 1121763328. Each ends "incomplete: NOT checked: types, borrows" and
      "3487 errors: blaze check", exit 1. The edit run: "FileItems found 4703 made 1",
      "BodyCheck found 110513 made 1", "store gets 121929 hits 121905". The repeat is not
      certified: 22 tasks fail every run (14 MacroModule "store lacks", 6 MacroModule
      "without module identity", 2 CrateScope tests records "marked `test` or `bench` more
      than once": blaze_cargo, blaze_diagnostics).
      IN PROGRESS, "an answer with errors is certified": work.rs files a typed failure as
      the task's record (out `failed_out`, one `task-failed` error, Cli wording 16; its reads
      are every input it was handed), so the request writes its certificate; a task handed a
      filed failure fails `Waited` and is filed the same way. Not filed: a failure on a fact
      outside the key and the reads (`Did::Hole`: a source not fetched, the home's module
      store, no home), a panic, a task handed a hole (`task.failed`). names.rs files its own
      failures with the files it read (`scope_record`, `Unwritten`); a refused tests record
      is now a finding beside an empty tests section. print.rs lists a finding in a file
      that is not UTF-8 without a place. Test `an_answer_with_errors_is_certified_and_asked_
      again_when_its_input_moves`, control `Plant::FiledWithoutReads`. HONEST LIMIT: the 14
      MacroModule "store lacks" stay holes (the home's module store is in no key), so the
      clone's repeat stays uncertified until the module store is a checked fact (a stamp of
      `proc-macro/modules` read as an input) or the modules are filled. Test green: "mixcheck
      test blaze-cli: exit 0; 112 passed, 0 failed" (tree f49ac44007), blaze-drive "45
      passed". Its first form (a non-UTF-8 module file) passed with the plant too: a
      `FileItems` key holds its text, so reads never mattered; the two-member form catches it.
      The clone with blaze-r15-b (r15/d): first 40.19 s, 487258064830 instructions, 13606109184
      bytes, "3507 errors" (the 20 more are the filed failures: 6 macro crates without an
      identity, 2 tests records, `Waited` ones); repeat 3.71 s, 36621888783, 748470272, NOT
      certified ("certificate missed"): the 14 `MacroModule` holes.
      RUN_OUTPUT ("`sized` is declared twice"): the refusal now names both crates (front2
      index.rs `LangTwice::crates`): "ba497b86.. (crate `ladder-3-hello`, /ladder-3-hello/src/
      main.rs) and 2980e808..": the `#![no_core]` program's own `#[lang = "sized"]` against
      core's, which `run_program` resolves beside it whenever a shelf is given. Fixed in
      `Index::new`: a `no_core` program's lang items are its own (it links no toolchain
      crate). Owed: such a program should resolve no toolchain crate at all.
      RUN_OUTPUT AFTER THE FIX (tree d9939596e5, 16:56): the clash is gone, and the run now
      fails differently: "`blaze run` on 3-hello past 60s" (run_output.rs:92). That is a
      deadline hit, not a refusal. Not yet looked at: profile it first. It may be the debug
      build resolving the whole toolchain for a `no_core` program, which the owed change
      (resolve no toolchain crate for `no_core`, run.rs `run_program`/`build_new`) would
      remove; that is not shown yet.
      ## HAND-ON 16:58 (runner 15, at the context limit)
      GREEN: "mixcheck test blaze-cli: exit 0; 112 passed, 0 failed" (tree f49ac44007, with
      the filed-failure test and its control); "mixcheck test blaze-drive: exit 0; 45 passed"
      (22186d6527); "mixcheck blaze-ladder: check ok (all targets)" (d561e20994, with the
      index.rs fix). blaze-cli went red at 16:5x in the PIPELINE ON RECORDS runner 2's files
      (body_types.rs, work.rs `run_batches`), not mine; `--test run_output` with the index fix
      waits on it (see the result line below if it came).
      HALF-DONE: nothing mid-edit. The store get: measured and floor written, no code changed
      (the bstd writer aa45ab6b5cc95648d offers `File::read_at` through `Io::call`, one
      function in blaze-std fs.rs; mapped packs are the end state: main assigns).
      OPEN, in order: (1) a certified repeat on the clone: make the home's module store a
      checked fact (its listing's stamp an input of `MacroModule`) so "store lacks" is filed,
      not a hole; then the repeat makes 1 get, not 122k. (2) The 31-49 µs store get (above).
      (3) `store.name_tool(tool)` after `Layered::open` (main.rs:750; `name_tool` is not in
      store.rs yet: W1-7's piece). (4) `run_after_check` NoMain (program.rs, hello-world lane).
      (5) PIPELINE ON RECORDS runner 2's `Spawned::Need` arm (theirs; told them go ahead).
      (6) The `test` arm in main.rs (plan: new test_cmd.rs; suites from the UnitGraph's
      CrateScope parts via `Request::settled_input`, added before `certificate()`; drive
      `TestLoop` with `test_run::Runner` over `Unbuilt`; exit `NOT_RUN` when it cannot run;
      tests/test_command.rs to pass args after `--` and lose `#[ignore]`). (7) Names off the
      main thread (CrateScope pooled: step 4). (8) D101's overflow-checks from the manifest's
      profile (WRONG ANSWERS runner 6 will ask).
      FILES EDITED since 15:45: blaze-cli src/work.rs (`TASK_FAILED`, `failed_out`,
      `failed_input`, `filed`, `filed_record`, `Did::Hole`, `Spawned::Waited`,
      `Plant::FiledWithoutReads`, `Unread::Text` as a task failure), names.rs (`scope_record`,
      `ScopeRecord`, `Unwritten`, `Resolved::failed`), graph.rs, fetch.rs, macros.rs (holes),
      fetch_tests.rs, print.rs (`list` reset and non-UTF-8 unplaced; `finding` prints the
      code), main.rs (test `an_answer_with_errors_is_certified_and_asked_again_when_its_input_
      moves`); blaze-drive src/request.rs (`settled_input`, `Waited` words); blaze-report
      src/frame/words.rs (Cli 16 `task-failed`); blaze-ladder src/front2/index.rs
      (`LangTwice::crates`, `keys_of`, the `no_core` rule); this file. Clone edit still in
      place: main-clone crates/blaze-diff/src/lib.rs `let _edited = var + 1;`.
      THE STORE GET, measured before any change (blaze-r15-a, the clone after the edit,
      r15/p1: `sample` 3 s of a 5.62 s repeat): "effect get 243858x 3779997663" ns for 121929
      gets, 31 µs a get (49 µs at load 40.5 in the edit run). Of the main thread's 1656
      samples, 1364 are `Layered::get_into`, 1284 `PackStore::get_into`, 1199 `File::read_at`,
      and 1188 of those wait: `Pending::wait` > `Io::poll` > `Kqueue::wait` > `kevent` (1154).
      The read itself is handed to the kqueue thread (`Io::submit` > `hand_over` >
      `Kqueue::to_threads` > `__ulock_wake`, 67 samples) and run there (`pread` 116 samples
      over all threads). So 93% of a get is the hand-off to another thread and the wait for
      its completion; the binary search over the packs' rows is 80 samples. FLOOR: a get is
      one binary search per pack (4 local packs, about 17 compares each) and one copy of the
      record from the page cache: a `pread` on the calling thread is about 1.5 µs, a copy from
      a read-only mapping of the pack under 0.5 µs for these records. Floor about 1 µs a get,
      30 to 50 times under today's. The fix of the largest part is the bstd writer's piece
      (OWNERS.md: "blocking file calls inline on the native driver", aa45ab6b5cc95648d); the
      store's own end state is the packs mapped read-only at open (a bstd `File::map` over
      the existing `Effect::Map`), a get a copy or a borrow. Beyond the floor of a get: an
      edit run makes 121929 gets for one changed body, gets in proportion to the tree, not
      the edit.
    - RUNNER 16 2026-11-16 (uncommitted; runs in port27/drive-1116/r16/; binaries
      targets/green/release/blaze-r16-a (17:26) and blaze-r16-b (17:44), greenmixes).
      ITEM 1, `blaze run` on a `no_core` program. Rule: the Rust Reference, "Preludes", extern
      prelude ("If the `no_std` attribute is specified, then the `std` crate is not added");
      `no_core` (unstable, so outside the Reference) takes `core` away too, as blaze-scope
      graph.rs already does for names. BEFORE (blaze-r15-b, which still clashed on `sized`):
      6.46 s real, 81695009964 instructions, load 34.1, then "the lang item `sized` is
      declared twice"; `sample` (b1.sample.txt): 3144 of 3149 main-thread samples in
      `run_program` > `resolve_crates`: the whole toolchain resolved for a crate that names
      none of it (the debug test binary is ~10x that: the 60 s). FIX: NEW
      blaze-ladder/src/library_set.rs (`library_of`, `toolchain_for`, `root_names_toolchain`:
      a `no_core` root that writes no `extern crate core|alloc|std` gets `Toolchain::none()`),
      wired by the RUN-PATH integrator into `build_new`, `run_program`, `build_records_new`.
      Test `a_no_core_root_is_given_no_toolchain_crate_it_did_not_name`: passes; with the rule
      removed: "left: Shelf right: None". "mixcheck test blaze-cli: exit 0; 2 passed, 0
      failed" for `--test run_output` (17:36). AFTER, cold in a fresh home (blaze-r16-a):
      "hello", exit 0, 4.05 s, 48873321669 instructions, load 26.2; sampled (a2.sample.txt):
      1997 of 2222 samples now in the CHECK beside the run, `names::run` > `resolve_crate`:
      "scopes 0 0 15 0", all 15 toolchain crates made. Two tries, both reverted: (a) leaving
      toolchain crates out of `names::parts` broke 35 tests: a `Need` locates its name among
      its home's parts; (b) handing a `no_core` crate no sysroot in names.rs `run` (reading the
      root by `names.world.texts.read(at, ..)`) broke the names tests (NAMES runner 18's
      removal proof: "4 passed, 6 failed" to "9 passed, 1 failed"): that read is likely not by
      the world's crate index. OWED: a part located but run only when wanted (drive), and the
      sysroot handed by the root's attributes read correctly.
      ITEM 2, the module store's listing as a checked fact: sweep.rs `LeafKind::HomeListing`
      (a folder under `<home>/.blaze`, followed like a listing; `read_home_listing`;
      `Follow::follow` takes the home's folder), main.rs opens the machine store before the
      sweep and reads the row `macros::MODULE_LISTING` after `units::lay`; the row is in the
      units' source, so a module that comes or goes moves the certificate, the graph's source
      and each `MacroModule` part's key; macros.rs files "store lacks" (`FailedSaying`). Test
      `a_module_put_in_the_home_store_moves_the_units_source_and_nothing_else_does`: passes;
      without the row: "a module put in the store moved nothing". Also `store.name_tool(tool)`
      after `Layered::open` (main.rs).
      CLONE, 18:20-18:50 (every store pack of an older format, so cold): blaze-r16-c (tree
      277a6b8093) ran past 900 s with no output (r16/d3, exit 124). Not a hang: `sample` at
      50 s (r16/d4a) shows `names::run` > `resolve_crate`, 1058 of 2968; at 130 s (r16/d4b)
      `body_types::prepare`, 1277 of 3213, of which 1226 are in `blaze_blake3::
      derive_key_with`. The cause: body_types.rs `input_at` hashed `name_at(at)` (a
      `ContentId::of`) INSIDE `task.inputs.iter().find(..)`, once per (crate, input) pair,
      per member, per body; names.rs `have` did the same. FIX: the name is hashed once per
      lookup (both closures). AFTER, blaze-r16-d (tree e57a81757b, greenmix 18:44): the cold
      run ends before 130 s (r16/d5); then e1 65.76 s real, 60.11 user, 993020641121
      instructions, 17101207280 bytes peak, load 21.2; e2 72.05 s, 59.64 user,
      994968232777, 17126209264, load 22.8. NOT removal-proved (the trees differ); the
      profile is the evidence. Both end exit 1 on a stop that is not mine: "the names of
      package `blaze-diagnostics` read `crates/blaze-diagnostics/src/tests/lints/bodies/
      calls/methods/non_fmt_panic/core.rs`, which no manifest's targets and no file's `mod`
      lines list". That file is an `include_str!` (non_fmt_panic.rs:28). blaze-scope
      resolve.rs `include` ignores its `IncludeAs` and reads through `SourceTexts::read`, and
      names.rs `Recording` lists every read as a module file wanting a `Surface`. A stopped
      request writes no certificate, so ITEM 2 IS NOT YET MEASURED on the clone: the repeat
      redoes it all (e2's profile: "task CrateScope 2861x 25453937584", "jobs made 782x
      36761617063" main-thread ns). Owed to NAMES: data includes are read as file content
      (a digest in the read list), never as a module's `Surface`.
      ITEM 3, THE STORE GET with the inline pread (blaze-r16-d, e2's profile): "effect get
      242660x 115651545" ns (2 effects a get: about 121330 gets), about 0.95 µs a get,
      against runner 15's 31 µs ("effect get 243858x 3779997663"): 33 times less, at the
      ~1 µs floor written above. MAPPED-PACK BRIEF (for main to assign; no longer on the
      critical path): bstd `File::map` over `Effect::Map`, read-only, at `PackStore::open`
      for each pack verified that boot; `get_into` copies from the mapping (or `get_ref`
      borrows it for the command's life); a pack a merge removed stays mapped until exit
      (its unlink does not unmap), which deletes the `pack.removed`/`gone` re-check path;
      the index rows stay as today. Floor: a binary search per pack plus one copy, under
      0.5 µs. Test: a store test that a get reads equal bytes mapped and by pread, and that
      a merge mid-command leaves the mapped get answering. The larger cost is the count:
      121k gets for a tree, in proportion to the tree, not the edit (the certificate fixes
      the repeat; the forward check fixes the edit).
      TESTS 18:53-18:56: "mixcheck test blaze-cli: exit 0; 1 passed, 0 failed" for
      `edits_of_module_files_answer_as_a_fresh_home` (tree b980692f16, live blaze-scope);
      the whole suite a minute later on tree 1af222ac1d with blaze-scope from its OLDER green
      snapshot: "121 passed, 7 failed" (fresh-home oracles, the fix tests' `NeedCycle` on a
      `BodyCheck`, "no crate's names record lists this file"). Not attributed to RUNNER 16's
      edits, and not proved otherwise: rerun the suite once blaze-scope is green live.
      ## HAND-ON 18:57 (runner 16). Next: (1) when NAMES fixes the `include_str!` stop,
      run the clone check twice with blaze-r16-d or newer (`--stats`), quoting the
      "certificate stood|missed" line and the repeat's store reads. (2) The open list:
      `run_after_check` NoMain, the `test` arm, names off the main thread (25 s of serial
      CrateScope on the clone), D101 (WRONG ANSWERS 6's batch). (3) The 17 GB footprint of
      the clone check is unexplained: ask the profile first.
    - RUNNER 17 2026-11-16 (uncommitted; run files in port27/drive-1116/r17/, `clonerun.sh <bin>
      <tag> [secs]`: scratch HOME, toolchain shelf synced, `timeout` OUTSIDE `/usr/bin/time` (b1's
      instruction count was `timeout`'s), MSL=1 adds `malloc_history -callTree`).
      ITEM 1, CrateScope ON THE POOL. Why it was main-thread: the world's texts read through a
      `!Sync` `&dyn Tree` into one frame, `&mut dyn Store`, `Prepares`/`CrateEnv` hold an `Alloc`,
      and `resolve_crate` ran on `Alloc::global()`. Now names/scope.rs: `prepare` (main: the
      deps' export bytes read once a round into `ScopeRound`, aligned, cached by key) -> a Sync
      `ScopeJob` -> `resolve` on a worker in its own frame (`SCOPE_FRAME`) -> `settle` + `answer`
      (main); blaze-scope crate_env.rs `EnvLent` lends the env (`clone_in` into the task's frame);
      job_texts.rs holds `JobTexts` (moved from body_types). A panic fails its task
      (`Why::Panicked`). Test pooled_scope_tests.rs: 4 workers vs 1 write equal record ids;
      planted `Plant::ScopeShareByPlace` fails on "ORACLE": "2 passed, 0 failed" (tree
      2fcdee417d). Clone: BEFORE blaze-r16-d (e57a81757b) 75.09 s, 993354816722 instr, 17133467448
      peak, "task CrateScope 0 2861x 26648934142"; AFTER blaze-r17-a (b183b5e3e0) 52.06 s,
      887756825138 instr, 8190172576 peak, "task CrateScope 0 1345x 68798967", "pooled CrateScope
      (worker ns) 0 1523x 33230714527". The simulation runs `Inline`: thread safety is by the
      types (ScopeJob: Sync, checked by the compiler), not by the test.
      ITEM 2, MEMORY: the profile has no Global-heap rows; vmmap: 10 GB of 10.9 is "Malloc Small"
      (frames are 300 MB). `malloc_history` at 45 s (m1.mh45.txt): 5181M in 111,403
      `Vec<key::Name>` grown at request.rs `Ran::Need` `job.needs.push` (disassembly: after
      `add_job`/`get_mut`); 997M the command frame's chunk reserve; 329M store `chunks::put`.
      FIX 1 (blaze-drive): `set_state` frees a job's needs as it settles; `Plant::KeepSettledNeeds`;
      the sim's answer oracle `kept_needs() == 0`; `control_needs_kept_past_settling_are_caught`;
      "48 passed, 0 failed". NOT THE PEAK: blaze-r17-b still "110515 (5180M)" at 45 s: those
      jobs are BodyChecks that never settle. ROOT (body_types.rs `prepare`, runner 5's): every
      body Needs EVERY workspace member's CrateScope (~1,400 names), opens each member's items
      record to find its owner, reads every member (`asked`): "jobs made: BodyCheck (main ns) 0
      331542x 51554714272" of "jobs made 0 976x 51875806723"; "machine step 0 143339x
      22729433899"; and one failed crate blocks every body ("110962 task(s) not run").
      FIX 2 (agreed with runner 5 and the coordinator; ONE batch): `Kind::Owners = 16` (leg 2
      agreed; D115's Script is 17, wiring/blaze-drive/d115-1.md goes after) and
      `key::owners_of(surface) = Name { kind: Owners, subject: surface.subject }`, plus a
      blaze-cli home.rs `row_of` arm `Owners => Row::Names`. A machine FACT, never a task: in
      `digest(read)`, an `Owners` read waits until every CrateScope job settled (one barrier: a
      counter kept in `set_state`/`add_job`, waiters woken at zero, all CrateScope parts made jobs
      on the first ask), then its out is a digest of the sorted CrateScope names `posted` lists
      as readers of the Surface; emit hands `Task.owners` sorted. Sim oracle: an edit that moves
      a file's owner moves the body; plant: a stale owner set. Runner 5 then Needs owners + their
      closure once per file job and deletes `asked`/`members`; reads only the scopes opened.
      work.rs `made_row`: "jobs made" split by kind (live).
      FIX 2 AS ABOVE IS WITHDRAWN (20:06, nothing written; the batch token was taken and given
      back unused): waiting for every CrateScope to SETTLE is today's shape under a new name (the
      coordinator's constraint: a body waits only for its owners and their closure; one failed
      scope blocks only its dependents). REPLACEMENT, owners by the MODULE TREE, not by scopes:
      a file's owners are the crate roots whose outline tree reaches it, and that tree is
      already in the drive: Units parts each crate root's file, and each FileItems record parts
      its `mod` files (`PathOf { module: true }`, kept in `part_locs` as (locate, place, parent)).
      So `owners_of(file)` = follow parents up to the roots, with NO scope run. Needed: (i)
      `part_locs` keeps EVERY parent of a part (a file that is a module of two crates has two;
      today a later insert overwrites); (ii) completeness: the fact settles once every FileItems
      reachable from the Units' roots is settled (outlines only, pooled and cheap): a barrier of
      the allowed kind (named and outlined, never resolved); an outline that fails hides only its
      own subtree; (iii) out = digest of the sorted root names, `Task.owners` = those roots (the
      cli maps a root to its crate and its CrateScope by the world); (iv) on an incremental run
      the parents come from the kept rows (old FileItems/Units rows found by key), no barrier.
      Agrees with the names lane's outline: a module under a macro, attribute or out-of-tree
      `#[path]` is already an error, so the outline tree IS the scope's file set. Test: one
      crate's scope fails; the bodies of crates not depending on it are checked and printed;
      "task(s) not run" counts only its dependents; plant: owners from one parent only.
      PART READS (coordinator's D114 ruling (1)), the drive side in ten lines, NOT built:
      (1) A read is (name, part, digest); `part` is `Whole` or a 128-bit cell key. (2) Record
      form: `reads` stays sorted by name; a parallel column `read_parts` (u8 tag + 16 bytes,
      Whole elided by a bitmap) so a reader of only Whole reads pays one bit. (3) `Row.readers`
      becomes (reader, part): `posted` keys by name and holds (reader, part) lists. (4) On a
      changed record, `dirty_readers` calls the kind's pure `part(bytes, cell) -> digest|Absent`
      once per distinct cell (sorted, deduped), compares with each reader's recorded digest,
      and dirties only movers; Whole readers compare the out as today. (5) The kind's `part`
      fn is registered by kind in the drive (a table `Kind -> fn`), sans-IO over mapped bytes.
      (6) `digest(read)` takes the part; `made_split`/stale checks compare per part. (7) Writers
      of read lists (one batch): blaze-drive record.rs/answers.rs/request.rs; blaze-cli work.rs
      (`Made.reads`), names/scope.rs `answer`, body_types.rs, macros.rs, unit_graph_task; the
      sim's `Made`. (8) Wire: answers.rs page rows add the column; old homes are rebuilt
      (rule 26). (9) Sim: a reader of cell A is not dirtied by a change to cell B; plant: all
      readers dirtied. (10) Cost: one `part` search per distinct (changed record, cell).
      OWED: (2) of the coordinator, FileItems re-reads its file at run time and files it under
      the sweep's id: hash the bytes read, compare, typed `Changed { path }`, proof type over
      verified bytes, sim with an edit between sweep and task, plant: compare skipped.
      CERTIFIED REPEAT (item 3), blaze-r17-c (d7c05cf264) twice: c1 84.09 s; c2 (repeat, load
      25.5-20.6) "332462 runs, 121497 store reads (10529 found), 4704 opens, certificate
      missed", 60.94 s, 864512373820 instr, 5359162456 peak; "store gets 121497 hits 10529
      puts 33". The repeat redoes it all: the cold run put only 33 records, its 110,962 bodies
      unrun behind failed tasks (the MacroModule "store lacks" blaze-serde-derive in the
      scratch home; source-refused path packages). Owed: why a run with failures puts no
      CrateScope/Surface records (10,529 found are FileItems-class); the owners fix unblocks
      the bodies. Item 4:
      `run_after_check` NoMain is done in the tree (it answers `NoBinary`).
    - RUNNER 18 2026-11-16 20:10-20:40 (uncommitted, tree d288fe5cc0 + the lanes' edits).
      ITEM 0, PUTS UNDER A FAILURE. Runner 17's "the cold run put only 33" was a misreading:
      c1 (r17/c1.profile.txt) "store gets 121330 hits 0 puts 10864"; c2 "gets 121497 hits
      10529 puts 33" (33 is the REPEAT's own puts) and found Units 181, UnitGraph 1, FileItems
      4704, CrateScope 450, MacroModule 20, Surface 4704, made 0 of each. What the repeat redid:
      "BodyCheck ... entries 331542 needs 221028" (no body ran: "jobs made: BodyCheck (main
      ns) 331542x 36938324366", "machine step 140277x 22604154105") and the failed CrateScopes
      (rule 20, `Ran::Failed` keeps no record): item 1's cause. `finish` withholds the
      certificate for FAILED tasks only; findings certify (the coordinator: right rule). Guard:
      sim.rs `a_request_with_a_failure_puts_every_settled_record`, control
      `control_puts_dropped_on_a_failure_are_caught` (Plant::DropPutsOnFailure in `put`):
      "test blaze-drive: exit 0; 2 passed, 0 failed".
      ITEM 2, VERIFIED BYTES, GREEN. blaze-drive verified.rs: `Verified<'b>` (private field)
      made only by `verify(name, bytes, expect, id, plant)`; `Why::Changed`;
      `Request::file_content_of(task)` (the sweep's id of the task's file). blaze-cli work.rs:
      `Job.expect` (from `job_of`), `verified_text` (the one way to text: verify, then UTF-8)
      in `outline_file` and `bodies_syntax`; `Unread::Changed` -> `Why::Changed` in `pooled`,
      `pooled_bodies`, the `Spawned::One` arm; `filed` never files `Changed`; main.rs after
      `after_answer`: `CliError::Changed { paths }` "changed while the command ran; run again
      (nothing of these was kept)". Sim: World `edited_at_run`, `verify_plant`;
      `a_file_changed_after_the_sweep_puts_nothing` (oracle: no record of the file, no
      `last`), control `control_a_skipped_content_compare_is_caught`
      (Plant::SkipContentCompare). "test blaze-drive: exit 0; 52 passed, 0 failed" (20:33);
      "mixcheck blaze-cli: check ok (all targets) at 20:32; tree b591f1c4a3; taken from their
      last green snapshots: blaze-build blaze-scope". Gap: blaze_file_items::outline takes
      `&str`, so the proof binds inside blaze-cli (no other path to text there), not by the
      parser's signature.
      ITEM 1, OWNERS, RULED (A) by the coordinator 20:30, NOT WRITTEN. Owners is a kept row
      the machine computes (never a task): owners(F) = union of owners(P) over F's parents P,
      a root (a Units part) adding itself; read list = each parent's FileItems/Units name and
      each parent's Owners name with their outs; out = digest of the sorted root names;
      payload = the sorted root FileItems names (the body reads it with `work::payload`; NOT a
      `Task.owners` field: told to PIPELINE runner 6, ad6bd1ddec7d4f387). A home settling with
      a FileItems part ADDED or REMOVED against its old parts (`settle`, the `old_parts`
      loop) dirties that part's Owners (only if it has a job or an old row); descendants
      follow by readers. Cold: settles when no Units/FileItems job is unsettled (a counter in
      `add_job`/`set_state`, waiters woken at zero; a failed outline hides only its subtree,
      named). Warm: no barrier, the row stands unless dirtied; recompute parents = old row's
      parent reads that still list F + this run's homes listing F (`part_locs` must keep
      EVERY parent: today `insert` overwrites, request.rs `settle`). Owed: `Kind::Owners = 16`,
      KINDS 16, `ALL`/`from_u8`/`name`/`stage`, blaze-cli home.rs `row_of` arm; a cycle of
      `#[path]` files must be a named failure, not a hang. Sim cases, each with a plant
      (coordinator): two parents kept (plant: last parent wins); a new `mod x;` elsewhere gives
      x a new owner and its bodies rerun (plant: added parts not dirtied); a parent removed
      drops the owner, and a body no root owns is named in the answer as not checked (never
      silently dropped). Then the brief's test (one crate's scope fails; other crates' bodies
      checked; "task(s) not run" counts only its dependents) and the clone before/after
      ("jobs made: BodyCheck", "machine step", peak) on clonerun.sh. Runner 5's condition:
      every body read-lists the Owners input itself.
      ITEM 3: not started. d115-scripts-in-command.md lines 14-26 and 65-66 say what the
      scripts lane expects of `key::content_of(path)` and `Ask::facts`; nothing in the tree.
      ITEM 4 (part reads): not started.
      FILED WITH THE DRIVE by PIPELINE runner 6 (20:45), not looked at: two NeedCycles on a
      BodyCheck, one BodyCheck left unsettled with nothing out: (1) blaze-cli main.rs test
      `edits_of_module_files`, BLAZE_SIM_TAPE='0 1 1'; (2) fix_tests (2 tests, among them
      `an_edit_above`), BLAZE_SIM_TAPE='0*3 5'. Runner 5: removing the member reads turns (1)
      back into the history-dependent note, so both may change once the owners batch deletes
      `asked`/`members`; re-run both after item 1 before reading the cycle.
    - RUNNER 19 2026-11-16 20:40-21:30 (uncommitted, tree 71eec5898c + the lanes' edits).
      ITEM 1, OWNERS, GREEN IN SIM. blaze-drive owners.rs (payload `BDO1` + u32 count + per
      root 32-byte FileItems subject and an asked byte, strictly increasing; `merge`,
      `out_of`, `write_roots`, `roots` refusing magic/count/trailing/tag/unsorted by name);
      key.rs `Kind::Owners = 16`, KINDS 16, `key::owners_of`; home.rs `row_of` Owners =>
      Whole; work.rs `builds` Owners => false. request.rs: `parents` keeps every parent (no
      overwrite); `outlines_open` counts unsettled Units/UnitGraph/FileItems jobs, and a cold
      Owners waits for zero (`owners_waiting`); warm, an Owners row stands unless dirtied; an
      added or removed FileItems part dirties its Owners; a removed part is dropped only
      when no other parent lists it; ROOTS = FileItems parts of Units (asked) or UnitGraph
      (not asked) records (coordinator option (c)); DEMAND (a): a BodyCheck part of a file
      becomes a job only once its file's Owners holds an asked root (`gate`/`release`), else
      only by a Need; empty owners with a FileItems parent is the finding `OWNERLESS_RULE`
      (0xC110); a cycle of `mod`/`#[path]` files fails its Owners by name in `break_cycle`
      (`owners_cycle`), never a hang. RULE 21: a body's read names follow from its owners, so
      a BodyCheck task key folds its file's Owners out (`locate`); before that the sim found
      "two values under one key ..., a shape of BodyCheck" when a file's owners moved. An
      Owners job puts no shape (its value key only). Sim sim_owners.rs (own toy world): two
      parents, a new mod, a removed parent + unowned file named, one failed scope, demand
      (a bad dependency body fails `check` only when a workspace body reads it, then exactly
      one dependency body runs), a cycle; controls OwnersLastParentWins,
      OwnersAddedNotDirtied, OwnersDropListedElsewhere, a one-parent plant, EveryPartAJob.
      sim.rs: Owners excluded from the "applied twice" and failure oracles (machine-made),
      one-body-edit read bound 6 + pages (the roots read). "blaze-drive: check ok (all
      targets)"; "test blaze-drive: exit 0; 64 passed, 0 failed".
      NOT MEASURED: the clone run (greenmix at ~21:15 caught leg 6's D115 batch half-applied:
      "work.rs:852:11 ... `blaze_drive::Kind::Script` not covered"). Note: in the real
      command no dependency file is a FileItems part yet (a dependency root is a store file;
      the drive locates FileItems only through the checkout sweep), so DEMAND changes no
      clone count until a store-file locate exists: asked of main.
      ITEMS 3, 4 and the Verified outline: designs sent to main 21:30, not written.
      DOUBLE MAKE (PIPELINE runner 7, owners_tests.rs `an_export_edit_in_one_member_reruns_no_
      body_of_another`, (2, 2) where (1, 1) is right): an edited file's body decided while its
      file's Surface was being remade, so its crates' CrateScopes had no job yet, read as the old
      digests, missed and ran over stale scopes, then ran again. Fix (request.rs `locate`,
      BodyCheck): wait on the home's Surface while it has an unsettled job; it settling moved
      dirties the scopes first. No store read. "test blaze-drive: exit 0; 74 passed, 0 failed"
      (21:35, after leg 6's D115 batch, tree 87e792b994 + edits). NOT RUN: owners_tests (lanes
      stopped testing 21:33). Owed: an edit in ANOTHER file of the crate still races; the class
      fix is a body waiting for every Surface of its owners' crates that has a job.
      COORDINATOR'S RULINGS 21:3x, NOT WRITTEN:
      (1) STORE-FILE LOCATE (after (2)): a dependency root is a FileItems part of UnitGraph
      located as a store file: a `Locate` with the pack's subject and the path inside it (never
      a host path), opened by blaze-cli from the sources the fetch records name, bytes verified
      against the pack's content ids (the same proof as checkout bytes); never swept or statted.
      Names half: `names::dep_roots(&names)` in names.rs (PIPELINE runner 7's file; told).
      (2) `Kind::Given` (next free number, 18 after Script 17): never a task or record, answered
      before the store. `key::content_of(path)` = Name { Given, Name::file(path).subject }:
      the sweep's id (file_at) for a path the table has; else the task's own read this command
      (the digest of the bytes it read); a read path the table lacks JOINS the leaf table as the
      command ends (an absent one joins as absent, so creating it moves the reader). A shape
      lookup over a Given name with no answer yet is a miss (run the task), never ABSENT.
      `key::fact_of(what)` = Name { Given, ContentId::of(FACT_DOMAIN, what) } and
      `Ask::facts: &[(Name, ContentId)]` (sorted, refused unsorted), seeded into `now`.
      Readers are kept by watch rows; a changed swept file seeds its Given name too (a
      `FileEntry` read only by content must not seed a FileItems job: the leaf row needs a
      content-only kind). Sim: a script reads a file the sweep never saw; the next command,
      the file edited, reruns it; plant: the first-read path not joined. The scripts lane's
      "files <sha256>" package digest is NOT content_of's form (told to D115 leg 5).
      (3) VERIFIED OUTLINE: `outline` itself takes the proof; `VerifiedText` and the compare
      move to blaze-ids: `verify(bytes, id)` (keyed callers; typed `Changed`) and `of(bytes)`
      (unkeyed callers pay one hash); no `outline(&str)` left. blaze-ids needs the whole-tree
      token: one small batch with every `outline` caller, after the open batches close.
      HAND-ON (21:50, the owner's stop). IN THE TREE, green in sim at 21:35 ("test
      blaze-drive: exit 0; 74 passed, 0 failed"): the Owners fact (owners.rs, sim_owners.rs,
      key.rs Owners = 16, request.rs, sim.rs, home.rs/work.rs arms); a BodyCheck's key folding
      its file's Owners out; the Surface wait (double make). WRITTEN, NOT IN: nothing
      (`Kind::Given` not started; no half-written file of this runner's). RULED, NOT WRITTEN,
      in order: `Kind::Given` ((2) above); the store-file Locate for dependency roots ((1));
      the race of an edit in another file of the same crate remaking a scope a body already
      ran over (a sim case with its plant first); part reads as one announced batch (RUNNER 17's
      ten lines above); `outline` taking the proof type with the proof moved to blaze-ids ((3)).
      Kind numbers: native-drive-kinds plans Links/Native as 18/19, Given was planned as 18:
      taken at landing. NOT MEASURED: the clone cold and repeat; the command is
      $N/rounds/RUNS.md section 5 (rows to quote against r17/c1: "jobs made: BodyCheck",
      "machine step", the BodyCheck needs, peak footprint and real).
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

## Check times: the end state in numbers (D114, 2026-11-16 evening; the coordinator's design)

Andrew, 19:42, after a cold `blaze check` of blaze's own clone took 52 s: "u should fix architecture to have
crazy short check times", "i want it to be incredibly impressive". The grain and the keys above stand. What
follows is why the command is 10 to 30 times off its own floors today, the rules that close that, and the
numbers the command is held to. Each number is a line of `--stats`, quoted with the tree and the load.

**Held to** (wall, a quiet machine; W = the workspace's own bodies; dependencies and std are never checked
on this machine: their records come by key from the shared cache, and until that cache exists their first
check is the one cold cost):

| Case | Any size | Today (measured 2026-11-16) |
|---|---|---|
| nothing changed | 2 ms | hello 2 to 3; tempo 13; the blaze clone: never, its check stops before the certificate |
| one body edited | 5 ms, plus 2 ms a MB of the edited file | 110 (hello: 58 one body check, 44 "prepare") |
| one signature edited | 5 ms, plus its readers x one body / cores | every body of the file (100 in the test) |
| one `pub` item added | 5 ms, plus the module group's cells and the readers of what it shadows | its whole crate re-solved |
| a branch switch of 5,000 files, checked before by anyone | 200 ms (5,000 outlines / cores, then finds) | not measured |
| cold, W = 60,000 bodies | 1 s on 128 cores, 8 s on a 10-core Mac | 52 s for the blaze clone (888 G instructions) |
| first error on the screen | 50 ms after start when it is in a changed file | after the whole answer |

**Why today is slow, by the profile** (each is a rule broken, not a tuning):
1. RECORDS ARE DECODED, NOT READ. A job's "prepare" copies whole crate records out of a pack and builds
   readers over them (44 ms for one edited body; the names reader decodes a whole module scope on first use;
   the expansion texts are copied with their crate's record on every command). RULE: a record is its own
   index. Every record form is sorted columns (blaze-columns `enc`) answered by a search in the mapped bytes:
   a lookup allocates nothing and touches the pages it needs; opening a crate's names, items or heads is one
   index search and a pointer. A fresh process answers its first lookup in microseconds. A job's setup is
   O(the crates its body names) pointer lookups, on the worker, never on the main thread.
2. THE COMMAND'S STORE COPIES. blaze-cli's `PackStore` (one pack a command, read by `pread` into frames,
   never synced, merged past eight) is a stand-in; blaze-cas's plain store (immutable segments mapped once a
   process, a two-slot head, compaction with a grace period) is the store. RULE: the command maps segments
   read-only and hands out slices; the page cache is the only cache (rule 51). After a restart a command
   verifies only the segments it reads, each once a boot; nothing is verified in proportion to the store.
3. A SERIAL SECTION GROWS WITH THE WORKSPACE. 31 of the clone's 52 s is the main thread making jobs; names
   ran on the main thread until 19:35 (26.6 s). RULE: the main thread only decides WHICH tasks are ready (1
   to 2 us a step, batches by home); everything that reads a record or text runs on the pool; no serial
   section may exceed 1 ms at any size, and `--stats` prints the longest one.
4. THE GRAIN STOPS AT THE FILE AND THE CRATE. A body's source covers its file's text around bodies, so a
   signature edit re-checks the file; `CrateScope` is a crate; a signature edited elsewhere that leaves the
   exports equal moves NO body's read (a stale check can stand: correctness, first). RULE: one kind a fact:
   `Signature` (an item's head, keyed by its text and its generics' context), `ModuleScope` and `Group` (the
   names solver's cells: a module, or the modules a glob cycle ties), `ImplBucket` (the impls of one trait
   for one self shape), `Expansion` (one call under `call_key`). A body's READ LIST names each fact it used
   with that fact's digest (read-addressed.md); it is valid by its key, or else by that list checked against
   today. Early cutoff at every kind: a fact whose digest is equal moves nobody.
5. WORK IS NOT ORDERED BY WHAT THE PERSON IS LOOKING AT. RULE: ready tasks run changed-first (the swept
   files' bodies, then their readers, then the rest), and a finding is printed when it is found, in a stable
   order within its file; the answer's order is restored at the end for the record.
6. THE CHECKER'S OWN COST IS NOT KNOWN. A body costs 1.56 ms mean in the drive and 0.35 ms in the run path's
   walk; neither has a profile by phase. OWED: spans for solve, unify, method lookup, coercion and borrow
   check on tempo's bodies; the floor for a body is set from that, not guessed.

**Lints** are the same machine at the same grain (rule 81): a lint pass over one body is a task keyed like its
check with a read list; errors print before the answer, advice after, and a repeat prints both from records.

**Pieces** (one writer or lane each; the order is by what the table loses most to): (a) the in-place readers
for names, items, heads and expansion texts, and the job's setup on the worker; (b) the command on
blaze-cas's store, the stand-in deleted; (c) the serial sections out (the driver lane has the first); (d) the
`Signature` kind and bodies' read lists by signature digest, with the stale-check test first; (e)
`ModuleScope` and `Group` from the solver's cells; (f) `ImplBucket`; (g) changed-first order and findings as
they are found; (h) the checker's spans by phase, then its floor.

## Build scripts in the command (D115, 2026-11-16 evening; the coordinator's design)

Found 19:44: the `blaze` command runs NO build script. names.rs hands the world `scripts: None` ("each crate
with a script is resolved without them"); `script_unit::Setting` is built only by an example and a test. So
`check`, `run` and `build` of any project with a `build.rs` see no `OUT_DIR`, no `cargo:rustc-cfg`, no
`env!` a script set: tempo has 106 scripts (99 give outputs in the example today), and 107 of its bodies
fail for want of one script's bindings. The harness proved the runtime; the command never called it.

**End state**: a script run is a drive task, kind `Script`, one a (script unit, target): the design's own
grain ("one body, instance, macro call or script run per task"). Not a step inside the job that makes the
world: 106 runs in one job is a serial section that grows with the workspace (D114's rule 3).
- KEY: the script runtime's producer id, the script unit's identity (its package and the digest of its
  sources and of its own dependencies' code), the target, the unit's features and cfg, the profile facts a
  script may see. Nothing of the machine: the roots are `/package` or `/workspace/<dir>` (D96).
- READ LIST: every fact the run asked for: each environment variable by name, each file it read outside its
  package by workspace-relative path, each probe answered from the facts table, and the OUTPUT RECORD of each
  script whose `DEP_<LINKS>_*` it read (an edge to another `Script` task: the links graph).
- OUT: one record: the `cargo:` lines in order, and `OUT_DIR` as a tree record (paths, modes, content ids;
  the files' bytes as their own records, so equal generated files are shared).
- READERS: the unit's `FileItems` and `CrateScope` read the record for cfg, `env!` values and an
  `include!(concat!(env!("OUT_DIR"), ..))`; a body's read list names the generated file's digest, so a
  script whose output is byte-equal after its sources changed moves nobody (early cutoff).
- WHERE IT RUNS: in blaze (the wasm or LIR runtime, sandboxed) on the pool; the scripts that spawn native
  tools through the D112 runner, whose records are local and never shared. One choice point.
- FAILURE: a finding naming the script, its package and the line it failed on; the unit that needs it is
  "not checked: its build script failed" by name, and the rest of the answer goes on (rule 20).
- A dependency's script output comes from the shared cache by key like any record; only a workspace
  member's script, or one whose inputs changed, runs here.

**Pieces**: (1) `Kind::Script` in blaze-drive key.rs with its name and source, and work.rs's task: the
`script_unit` run of ONE unit with the setting made once at the command's edge (rule 75: the facts table,
the native setting, the SDK); (2) the world takes script records from the drive's reads
(`blaze_world::ScriptRecords`), `scripts: None` and its note deleted; (3) the `DEP_*` edges as reads; (4)
`CrateInput::script` on the body path (front_load.rs); (5) tempo: the line "N of 106 scripts made by the
command", then the 107 bodies.

**Two rulings (19:48, on the implementing lane's questions).** (1) A script's output is a READ, never part
of a key: it is not known before the script runs. The crate's key is its identity without the script's digest
(blaze-world's `world` takes no scripts; `UnitKeyParts.script` goes); each task that needs what a script made
Needs its own unit's `Script` record and joins it at task time; a dependent is cut off by its dependency's
`CrateScope` out digest as today. There is no world-grain barrier and no kind `World`. A code record's crate
fact (D106) folds the crate's `Script` out digest, since the crate key no longer does. (2) The drive's record
is the only cache of a script run: the script runtime's own record store is deleted from the command's path
in the same batch (rules 30, 77).
(3, 20:33) Every unit reads the `Script` of ITS OWN side: a package built for the macro side runs its script
for that side and takes that run's cfg (proc-macro2 and syn's dependencies set theirs so); none only when
the package has no script. The script a unit reads is named by one function of the parts of its crate key,
so two units under one crate key cannot name two scripts; the world refuses the case by name all the same.
(4) A kind's number is taken when its batch lands, never reserved: `Script` is 16.
(5, 20:52) A `Script`'s source holds ITS OWN module's digest (its row of the module listing), never the
listing whole: one script rebuilt moves one `Script`. While script modules are built by the interim
bootstrap builder (D110), the row also carries the digest of the sources the module was built from, and a
`Script` task whose `build.rs` no longer matches it stops with a typed `ModuleStale` naming the unit: a
record of an older script never stands silently. (6) What was not checked is NAMED by cause, at crate grain:
the answer groups the tasks that did not run by the failed task they wait on and lists, for each, the crates
not checked because of it (at most 20 a cause, the count exact). A failed build script reads so by that rule.

## D114 rulings (2026-11-16 20:00; the coordinator's, on the implementing lanes' questions)

### Part reads (the drive; every fine fact below is read this way)

A read is (name, PART, digest). The part is `Whole`, or one CELL of the record (a 128-bit key). A kind that has
cells gives one pure function, `part(record bytes, cell) -> digest | Absent`: a search in the mapped bytes. A
reader's entry is valid when today's record gives the same digest for that cell; `Absent` is a value, so a
name that was looked up and not found is a read like any other. Forward from a change: the changed record's
readers (the drive's `Row.readers`) are each asked their cells; only a reader whose cell moved is dirty. At
5,000 readers of one module that is 5,000 searches, about 1 ms of pool time; a posting by cell is a later
step, only if a profile asks for it. One change to the read list's form, made once, all writers in the batch
(rule 24).

### Names at module grain (piece (e); replaces the lane's page where they differ)

A name is resolved by walking LINKS over tables made from text. Nothing of another module is copied into a
module's record, and no module waits for another unless a name can only come from a macro's output.

1. TWO TABLES A MODULE. Its OUTLINE, from text alone (the file's items record): each declared name by
   namespace, each `use` as a link (the written path, its rename, its visibility), each glob as a link, each
   item-position macro call (its path, its input's digest), its `macro_rules!` in order, and `#[macro_use]`,
   `#[path]` and cfg. No wait, ever. Its `ModuleScope`, ONLY for a module that has item-position macro calls:
   the names those calls made once fully expanded (declarations, use links, glob links, modules,
   `macro_rules!`, `#[macro_export]` macros), the textual macro scope at its end, and the solve's findings.
   Key: the producer, the crate's context, the module's identity, its outline's digest. Reads: the cells it
   looked up to resolve ITS CALLS' paths, and each `Expansion` by call key. A module with no such call has no
   task (rule 77). A module's EXPLICIT TABLE is its outline and what its calls made.
2. LOOKUP IS A FUNCTION, NOT A RECORD. `lookup(module, namespace, name, from)`: the outline (a declaration
   answers; a use link is followed, segment by segment); else, when the module has item-position calls, what
   they made (its `ModuleScope`); else its glob links, each followed as seen from this module, equal
   definitions merged and different ones `Ambiguous` (an error only where used); else the preludes, by
   today's rules. A lookup already on the walk's stack gives nothing (the least fixed point: a glob cycle; an
   import cycle is `Unresolved`, named). The rules have ONE home (rule 30): the walk is the cell core deciding
   one cell on demand over settled tables, not a second resolver (it can: an entry `decide(module,
   namespace, name, layer)`, an index from bound name to items, the walk's stack for waiters, modules loaded
   at first read; inside one module or group the core speculates and time-checks as today).
   WHEN A `ModuleScope` IS NOT SETTLED YET (corrected 20:31 by the count on tempo: 73.7% of 144,679 modules
   have calls, derives and attributes among them; a wait at every such read made cycles of up to 88 modules):
   two kinds of lookup. One that DRIVES AN EXPANSION (a derive's, attribute's or bang macro's path) never
   waits when it FINDS something: it is answered from the tables as they are, as rustc itself resolves, and
   the unsettled module's cell is logged as a read of `Absent`. If that module settles without the name, the
   read stands: every program rustc accepts (it refuses the rest, so there the speculative answer is the
   final one). If it settles WITH the name, the reader's read moved: it reruns over settled tables, where the
   rules give the answer and its ambiguity; a third rerun for one cell is refused, naming it (rule 18). Only
   a macro path NOT FOUND waits, on the pending modules along its walk. A lookup of a TYPE or VALUE name by
   `Imports` or a body waits for the `ModuleScope` of each module on its walk that has calls and lacks the
   name in text: one direction only. Built-in attribute names are not looked up through modules while
   solving (rustc makes one ambiguous with any other macro of its name, so in an accepted program the
   built-in is the answer); that ambiguity is an `Imports` finding, checked once.
3. READS ARE PER CELL. Every hop is a part read (table, (namespace, name), digest or `Absent`). A `pub fn`
   added to a module moves exactly the askers that looked that name up there; a change that leaves a cell
   equal moves nobody; a glob is not coarse.
4. `Imports`, a leaf kind, one a module: its own use items resolved to final definitions (column RESOLVED),
   its traits in scope (column TRAITS: declared, imported, `as _`; a glob's by a part read of the source's
   TRAITS), and the findings (unresolved, private). What the module's bodies search first: one search for an
   imported name. Materialised only for the module's own use items (tens a module), never a glob's names. It
   reads outlines and `ModuleScope`s, never another `Imports`, so it is never in a cycle.
5. WAITS AND GROUPS. Only `ModuleScope` tasks wait on each other, and only for a macro path NOT FOUND (point
   2). A cycle of such waits, found by the drive as it runs, is one `Group`: its members' calls solved in one
   cell core to quiet (the core as it is), keyed by the sorted members' identities and outline digests and
   the crate's context; each member keeps its own record. Counted on tempo 20:29 under the first form of the
   rule (a wait for any name absent from text): 254,139 distinct waits, 173,829 of them macro names, 696
   cycles, the largest 88 modules (slatedb); and 0 bytes of today's 82.7 MB of scopes are glob names (129,707
   glob edges, 8.8 MB). Counted 20:42 under the corrected rule (tree 5dbe02b3d4; a link followed one level,
   so near, not exact): of 277,672 expansion-driving macro lookups, 2,247 are not found from text, links and
   preludes (0.8%); among those waits there is NO cycle (largest 1); speculative answers the settled tables
   contradict: 0. So on tempo no `Group` forms. Build order follows: `ModuleScope` and `Imports` first; until
   a real program shows a cycle, a wait cycle among `ModuleScope` tasks is a typed refusal naming its modules,
   and `Group` is built when one does. Type and value lookups by imports that wait one way on a
   `ModuleScope`: 68,262 distinct of 139,833.
6. `ExportedMacros`, its own kind, one a crate: the `#[macro_export]` macros in text (from outlines, no wait)
   and those calls made (each `ModuleScope`'s column, by part read). A macro lookup at a crate's root answers
   from text first; only a name absent there, in a crate with pending calls, waits on this.
7. `CrateScope` stays as it is until this path gives equal answers on tempo; then it and the materialised
   `Resolved` scopes are deleted (rule 30). It does not become an assembler: a parent's cell for `mod a` holds
   a's IDENTITY, a's tables are found by name, and today's outline digest is in their key, never in the parent.
8. WHY LINKS. A materialised glob copies its source's names into every importer (libc's names times its
   importers; a prelude times its crate) and makes every export change of the source a change of every
   importer. A link costs no bytes and no re-solve; its cost is hops at read time, bounded by `Imports`' one
   search for explicit imports and a memo a job for the rest.
9. FLOORS. A `pub fn` added: one outline (2 ms a MB), the cell checks of that module's askers (1 us each), the
   re-checks of the askers of that name. A `use` added: one `Imports`, and that module's bodies whose cell
   moved. Cold: no solve for a module without calls. Today: the whole crate re-solved; tempo's names 5.1 s.

### Signatures (piece (d))

One `Signature` is one head the checker asks for (one `heads::Want`: a trait with its members' fn signatures;
an impl's fn its own), named by (file, item chain), its out the digest of the head records it makes. A body's
source is its own text's hash alone; `around_bodies` goes. Checkout crates only: another crate's heads are
covered by that crate's identity. FIRST CUT, marked coarse: a body reads its own file's `Surface` whole for
what `around` covered (uses, `macro_rules!`, attributes); when (e) lands that read becomes the cells the body
looked up. Until then an item added to a file re-checks that file's bodies.

A RECORD HOLDS NOTHING ITS READ LIST DOES NOT COVER (ruled 20:36 on the red test's "types NOT checked"
line): the crate's items row copied each entry's head hash while the scope's reads (the files' `Surface`)
leave types out, so a type-only edit left the row stale and valid. The row keeps the item's identity (file,
entry, chain) and its name-level facts; the head is read from TODAY's file record. Proof by test: scopes
over texts differing only in a parameter's type, a return type and a bound give items records of equal
bytes; the plant puts the head back.

FACTS A BODY MAKES (the same stale-check class; found by the critic of run.md; each gets its red test before
its fix): (i) HIDDEN TYPES: the type behind an `impl Trait` return or an `async fn`'s future, with what leaks
(its auto traits; for code, its layout): a part `Hidden` of the owner's `BodyCheck` record, read by digest by
whoever asked. (ii) CONST AND STATIC VALUES: kind `ConstValue`, read by the signatures whose types name it,
by bodies, by code. (iii) ITEMS IN A BODY THAT THE CRATE SEES (an `impl`, a `#[macro_export]` macro): outline
entries; the outline goes into a body whose tokens hold `impl` or `macro_export`, so the surface and
`ImplBucket` see them. (iv) POSITIONS: a body whose expansion used `line!()` or `column!()` reads its item's
place in the file's `Lines` record; a panic location is data bound from `Lines` when the program is placed;
no code record holds an absolute line (rule 71). Tests owed first: a future that stops being `Send` reaches
its caller; a const's value reaches `[u8; N]`; an impl removed inside a body is missed by its users.

### The store under the command (piece (b))

1. ONE STORE A USER, one root (`<home>/v2/store`). Every checkout and worktree reads and writes it: sharing
   across worktrees is the store being one, not a chain of three. The store writes nothing in a checkout. The
   old `shared/`, `own/` and a checkout's `.blaze/v2/packs/` are an old home: reported by name, rebuilt,
   neither migrated nor deleted by the command.
2. A NAMESPACE, NOT A PLACE. `Sharing::place` becomes the record's namespace: `PUBLIC` when every name in it
   is public AND this machine may write it (a trusted builder); else `LOCAL`. On a developer's machine
   everything computed is `LOCAL`; `PUBLIC` records arrive only verified, from an origin. `Place`, `Layered`'s
   three stores and `Machine::Off` go.
3. WHAT WAS THE CHECKOUT'S. Only the leaf table is about a checkout (path, stamp, content id): it and the
   last answer's key for EACH request shape hang from one named root a checkout (`Driven::root`), named by a
   digest of the checkout's path. Certificates, answers and pages are keyed by content and found by any
   worktree with equal content. A root's name is the one place a digest of a machine's path stands; roots are
   `LOCAL`, never sealed or published, and the put scan keeps paths out of shared namespaces (rule 25).
4. NO HOME is a typed refusal naming `HOME` (rule 3): there is no second place.
5. ONE DRIVER A PROCESS. `PlainPlace<D: Driver>` is generic (rule 78), tested on the simulation; the command
   gives it the driver bstd lends: one new bstd seam, a driver rooted at a `Dir<ReadWrite>`, which submits and
   polls on the command's backend, yields only completions of effects submitted through it, and refuses a
   path outside its directory as `Fs` does. No second native driver in the command.
6. IN PLACE. A get returns `&'m [u8]`, where `'m` borrows `Maps`: the command's one append-only owner of
   mappings and of this command's own puts, made before the store and dropped after every reader. One
   `unsafe`, inside `Maps`, its assumption stated (a mapping is never unmapped or moved while `Maps` lives; a
   retired segment's file may be unlinked, its mapping stands). No lifetime cast at a call site.
7. THE OPEN MAPS NOTHING. It reads the head and the manifest (as today: no format change; compaction keeps
   at most `SEGMENTS_MOST` segments a namespace, and a store naming more than 64 is a finding). The main
   thread's get maps a segment at its first probe, so a command whose certificate stands maps one or two.
   Pool tasks read through the lock-free handle, which cannot map: every named segment is mapped ONCE, when a
   request's first pool task is about to start. An unsealed segment of an earlier boot is length-checked at
   its first probe and checked whole at its first hit, once a boot. A get probes segments largest first (a
   key holds one value, so order changes no answer, and an old record is in the big segment); a root is a
   name in the newest manifest, not a probe. `--stats` prints the open's time, segments mapped of named, and
   probes a get. LATER, only when an after-restart profile names the store's page-ins: index runs apart from
   payload segments, merged geometrically after the answer (at most four; one interpolated page a run);
   durable and concurrent, so its own DST first.
8. WHAT IS PUBLIC IS A POSITIVE LIST BY SUBJECT (corrected 21:10: an arm keyed on "ran natively" could never
   fire, and with only `CrateScope` names public, every registry crate with a build script was shut out of the
   shared cache). A `Script` name is public exactly as a `CrateScope` name is: when its subject is in the
   public set. The command adds a script's subject only when its crate is public AND it is placed on the wasm
   path (decided where the part is named, before any run). A natively placed script's subject is never added,
   so it and every reader of it are `LOCAL`; a subject forgotten is local, never public. Placement is in the
   `Script`'s key, so a native and a wasm run are never one record. Until the deterministic sandbox replaces
   the native runner, a crate whose script must run natively, and what reads it, stay out of the shared
   cache: D112's "local records only".

### A body's owners, and what it waits for (ruled 20:07 to 20:31)

Measured on the clone (runner 17): every body Needed every workspace member's `CrateScope` (about 1,400
names): 51.6 s of main-thread "jobs made", 5.2 GB of need lists, one failed crate blocking 110,962 bodies,
and any crate's name change invalidating every body. RULE: a body waits only for its file's OWNERS and their
dependency closure, and reads only the scopes its check opened. OWNERS is a kept fact the machine computes
from the module tree it already holds, never from scopes: owners(file) is the union of its parents' owners,
a crate root adding itself; its read list is each parent's items record and each parent's owners; a parent
that gains or loses a `mod` dirties that file's owners. Cold, it settles when every file reachable from the
roots is OUTLINED (named and parsed, never resolved: the one barrier allowed); warm, there is none. Refused:
waiting for every scope to settle (today's shape under a new name). Test: one crate's scope fails; bodies of
crates that do not depend on it are checked and printed.

### The bytes a task parses are the bytes its key names (ruled 20:05; in at 20:33)

A task that reads a file by path hashes what it read; when that is not the content id in its key it stops
with a typed `Changed`, files nothing, and the command refuses its answer naming the paths. No re-read, no
retry (rules 3, 74, 80). Before this, an edit between the sweep and the task filed new text under the old
id, in a store every worktree reads. Owed: the parser's entry takes the proof type, so it holds in every
crate, not only in blaze-cli.

### A finding inside an expansion is reported where its tokens came from (ruled 20:57; not built)

Found on D104's refusal: `fn main(cx: &Cx, io: Io)` is refused at bytes 321..423 of the expanded launcher
item, which has no source file, not at `io: Io`. RULE: an expansion's record keeps, for each token it wrote,
its ORIGIN: the n-th token of the call's input, a token of the macro's definition, or made by the macro
(blaze-expand's `TokenOrigin`, kept as a column of the expansion's text; input tokens by index, never by a
position: rule 71). The task that MAKES a finding in expanded text has that record in hand (it is already in
its read list) and maps the finding's range before filing it: when the range's tokens are input tokens, the
finding is filed at those tokens' range in the user's item; otherwise at the call, and it names the macro as
written ("in the expansion of `m!`"). Mapping at the making, not at the printing, so a kept finding and a
fresh one are equal bytes. A nested expansion maps through each level in turn. Tests: the `Io` parameter
above is reported at `io: Io` in the user's file; a type error in a `sol!` input is reported inside the
call's braces; an error wholly in a macro's own tokens is reported at the call and names it; control: the
origin column dropped reports all three at the call.

### The crate list is a record (ruled 21:00; not built)

Measured on a one-line edit of the hello copy (20:58, tree d52bcecbf5): of 138.9 ms of main-thread job
making, 85.1 ms is `names::world` remade whole (the unit graph decoded, the shelf and the package sources
opened, the list and the world built, root files read), of which the body job uses a few fields. RULE: the
world is made once, by the job that already makes it to name its parts (`UnitGraph`), and what readers need
is columns of that record's payload: one row a crate, sorted by key, read in place: key, deps, side, the
package's place (package-relative), lang and sysroot ids, the scope rule, its `Script` part's name. A body
job opens its owners' rows and their closure by key; an edit makes no world on any thread. A fact of a root
file's text is not in the list: it is read from that file's outline by whoever needs it.

### A token keeps its mark (ruled 21:27 under rule 66; not built)

Found on `const_eval_select` (13 of tempo's functions; core's `const_panic!` reaches a `macro` item whose
body writes a bare name that resolves only where the macro is DEFINED). The mechanism: blaze-expand
`body_words` turns an expansion's tokens into body text and DROPS their marks. The first fix carried one
fact through by a spelled token (`$crate` as `__dollar_crate_<hex>`); the proposed second would carry one
more (the definition's module, for an opaque mark). RULING: rebuild, once. Every token of an expansion
keeps its MARK through `body_words`, the lowering and the front end: where it came from (the n-th input
token, the macro's definition, made) and, for a definition token, the definition's site (crate, module) and
the macro's kind of hygiene (a `macro` item: every name at the definition; `macro_rules!`: locals, labels and
`$crate` at the definition, everything else at the call; a proc macro: by the span it chose). It is ONE
column, the same one "A finding inside an expansion" needs. A path in expanded text is resolved by the mark
of its first segment, in one function; `$crate`, a bare name in a `macro` body and a macro call's own name
are three uses of that function, and the spelled `$crate` token and `ScopedBy::module_at`'s separate route
go in the same piece. Tests: the first fix's cases; `const_eval_select`; a bare function name in a `macro`
body resolves at its definition; an item name written by a `macro_rules!` resolves at the CALL (the control
resolves it at the definition and must fail); a local written by a `macro_rules!` does not capture the
caller's.


### An expansion is its token record (D122, 2026-11-17 00:15; the coordinator's design; not built)

The owner, 2026-11-17 00:03: "fix 'macro expansions are rendered to text and lexed again' properly". Read first
(the fact lane's report, rounds/wave-2311/G3-expansion-text-facts.md in the coordinator's notes): the parser takes
words, never text (`parser::Tokens`); an expansion in BODY position is already parsed straight from its words
(blaze-lower feed.rs); each expansion already has a token record (blaze-expand record.rs: tokens, contexts,
origins); `blaze_file_items::outline_tokens` exists, is proven equal to the text outliner, and no production path
calls it. The round trip is only in ITEM position: blaze-scope `expand_decls` renders the tokens to text
(`render_with`), lexes that text to outline it, rewrites a statement call's text into a second text, lexes a nested
call's input again out of its parent's rendered text, and front2 lexes the rendered text once more for every head
and body of an item an expansion made. Three defects of one night came from it (an integer glued to a dot and read
back as a float; `$crate` spelled into the text; marks found by the ADDRESS of a slice of that text).

END STATE. The token record is the expansion; nothing parses a rendering of it.

1. The record holds, per token: its kind, whether it is joined to the next (a flag of the token, never read off
   spacing), its lexeme by index into the record's own table of distinct lexemes, its mark, and where it came
   from (the call's input or the macro's template, by token index). No column holds a rendered text.
2. A position inside an expansion is a TOKEN RANGE of its record. Origins, findings, an item's extent and a
   body's extent in an expansion are token ranges; they are turned into a file place only when printed, by the
   rule already ruled ("a finding inside an expansion is reported where its tokens came from").
3. An expansion's items are outlined FROM ITS TOKENS (`outline_tokens`, made the production path): names from
   the lexeme table, attributes as token ranges. A statement call's items get their enclosing block's module as
   a node of the outline, not as text wrapped around them.
4. A call written inside an expansion takes its input as a sub-range of its parent's tokens.
5. Heads and bodies of items an expansion made are lowered from their token range: the same word feed body
   expansions use. A mark is read at a token's index.
6. Digests of expansion-made things are over TOKENS, by one function (kind, lexeme bytes, joined, mark, in
   order): the expansion's id, an item's outline, signature and body digests, the names digest. Item keys
   already need no text. A FILE's digests stay over its bytes (a file's identity is its text, and bytes are
   compared without lexing).
7. Text of an expansion exists in one function, `shown(record, range)`, for a person: diagnostics and
   `blaze explain`. Nothing lexes its output; a test fails if anything under crates/ passes it to a lexer.
8. The scope record's `expansions` section names each expansion's record by key (a link, D116), never a copy of
   its text.

Pieces (interfaces first; each beside the old path until its reader switches):
E0 the form itself, as one small crate every reader can depend on (blaze-tokens, unless blaze-expand already sits
   below blaze-file-items and blaze-lower): the view read in place, token ranges, the token digest, `shown`.
E1 blaze-expand: the record's form (the joined flag, the lexeme table, token ranges, the token digest, `shown`).
E2 blaze-file-items: `outline_tokens` in production form (no text argument), with its equality test against the
   text outliner on files kept as the control.
E3 blaze-scope: `expand_decls` outlines from tokens; nested inputs as sub-ranges; statement items by an outline
   node; origins by token index; `render_with` leaves the parse path.
E4 blaze-lower: `lower_signature` and `lower_body_expanding` over a token range; `signature_key` and `body_key`
   of expansion-made items over tokens.
E5 blaze-ladder front2: heads and bodies of expansion-made items through E4; marks by token index (replacing
   the lookup by address).
E6 blaze-cli: the `expansions` section as links; the record's form number moves.
Proof: every x0 rung and the std census print the same through the token path as through the text path while
both exist (rule 78); the three defects above each have a rung that fails on the text path.
