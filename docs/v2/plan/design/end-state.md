---
status: plan
summary: The end state of blaze's incremental design, as one idea at three levels (the request, the query engine, the front end): every result is kept with exactly what it read, at the grain of the smallest thing that changes alone; it is the answer until one of those reads differs; then only what read it is redone. Says what each level becomes, what is deleted, the order of building and the numbers that judge it.
date: 2026-11-12
---
# The end state: records with their reads

Owner: the main agent. Written under rule 66 (`end-state-not-hill-climb`) after a week of separate
fixes to one symptom. It replaces the plans for the request path, engine validity and front-end
loading in `restore.md`, `on-save.md` and `certificate.md` where they disagree; those documents keep
the detail of pieces this one names. Status of every claim below: **measured** (a run, with its
tree) or **design** (not run).

## Why a redesign

Measured 2026-11-12:

| Symptom | Number | Tree |
|---|---|---|
| A repeated `blaze run` of a hello-world | 1,162 jobs, 1,195 admissions | 7b970bbe0d, Mac |
| reth, nothing changed | 2.0-2.8 s | d45e70ddcd, VM |
| reth, a private body edit under 33 dependents | 54 units remade, 4.8 M rows verified, 46-54 s | d45e70ddcd |
| reth, restart with nothing changed | 191.6 s, 6.81 M rows run, 144.7 M verified, 827 crates resolved | d45e70ddcd |
| tempo cold build | 122 GiB with one task running; 4.2 M MIR bodies kept | d45e70ddcd |
| The competitor (Relax), hello-world, nothing changed | 52.5 ms, no daemon | Mac |

Each had fixes with real causes (terminal width in keys, lock files, environment variables, scope
order, proc-macro verdicts, links in paths). The design stayed the same: **"nothing changed" is
found by walking**, and the fast paths are exceptions to it. Three places have that shape:

1. **The request.** Every request plans and walks the unit graph. A kept answer exists in memory
   only and is refused for a list of cases.
2. **The engine.** Four layered validity mechanisms (durability levels, summaries with completion
   and a floor, `frontier::reach`, seal groups), each added to cover the one before; none deleted.
3. **The front end.** Four ways a crate's results enter a database (resolved from source, a decoded
   image, paged records, "awaited"), three stores for a function's results (check cache, item
   store, a crate-keyed MIR record), most keyed per crate.

How Relax does it (read: `front/src/crate_image.rs` `GraphImage`, `compiler/src/cache/collected.rs`
`certificate`, `restore_collected`, `front/src/lazy.rs`): one certificate for the program (every
input with its hash and stat, existence probes); one image per file of plain arrays mapped
read-only; only files the program's names reach are parsed; nothing held between runs.

## The one idea

> A result is kept with exactly what it read, at the grain of the smallest thing that can change
> alone. It is the answer until one of those reads differs. Then the reader is told which, and only
> what read it is redone.

A **record** is `(key, reads, value)`:

- **key**: the result's own content identity, computable before the work it saves (rule 65).
- **reads**: a list of `(what, digest)`; a negative read (looked for, absent) is a read whose
  digest is "nothing".
- **value**: plain data in the store, read in place a page at a time (rule 51).

A record **holds** when every read's current digest equals the recorded one. Holding is checked
from the outside in: a coarse read first (a directory's Merkle id, a crate's seal), the fine reads
only under a coarse read that moved. A coarse read is never a key (rule 65); it is only the first
check.

There is one such form at every level; the levels differ only in what a read names.

## Level 1: the request (the certificate)

- **Record.** Per request shape: reads = the checkout's root id (taken after the build's own writes
  are in the view), a Merkle id per package directory with the units that read it, each
  environment variable read (name, value hash), each outside file or listing (hash, or nothing),
  the toolchain and blaze identity; value = outputs (store key, stamp), the units' record keys, what
  a no-op says. In the store; content only, no absolute path.
- **Check.** One record read and the id compares, in the daemon's loop, before any plan, view walk
  or job. The client makes one small request (only the daemon can flush its watcher). On a hit: no
  `job`, `task` or `admission` span.
- **It stops holding only when** a recorded input, probe, variable, the request shape, the tool
  identity or a promised output differs, and it names which. Not for a restart, another request, the
  build's own writes, a link in a path, a path dependency outside the workspace or a lone file.
- **On a miss** the walk starts at the units that read what moved and goes up through their
  dependents; it does not start at the roots.
- **An untraced step** (a build script outside the sandbox's record) keeps no certificate and says
  so once per build, naming the unit.
- **Deleted:** `noop::Answers`, `LastAnswer` as the only copy, `Unkept::{Planned, Made, Said,
  Looked, Outputs}`, the outside-watch "vouching" rules as a condition for answering (the watcher
  stays as the thing that keeps directory ids current), request identity over the whole environment.
- **Shape of the code.** The request loop is a sans-IO state machine of its own module (rule 14):
  `Request -> Certified | Walk(from)`; `job.rs` loses that logic instead of gaining hunks.

## Level 2: the unit and the engine

- **Unit record.** Key = the unit's own sources and settings. Reads = the facts it resolved of other
  items (`FactRef`: a name in a scope, a signature, fields, an impl row, a constant, a macro's
  definition; negatives included) and, as separate reads taken only by units that need them,
  positions. No whole-interface digest in a key; a crate's surface digest is at most the first
  check before its facts.
- **Engine validity: one mechanism.** A row holds its reads (edges with the stamp read). Rows are
  grouped per crate; a group's seal is the coarse read: "nothing this group read from outside
  moved". A write opens its own group by index and marks as suspect the groups that read it; a
  suspect group is checked once by its imports; a broken seal re-verifies only the rows that read
  what moved.
- **Deleted, once the one mechanism passes the engine's refinement tests:** durability levels as a
  validity shortcut, summaries and their completion, the floor, `frontier::reach`. (Open question 1
  below: what each still gives that seals plus per-row reads do not.)
- **A restart** restores the groups' seals from records (group by crate content key, imports'
  stamps as content hashes), so the first request after a restart starts sealed.

## Level 3: the front end (one way in)

- **One route.** A query asks for an item's result; the result is a per-item record found by the
  item's own key; a miss runs the query for that item and keeps the record. There is no per-crate
  encode pass, no image decoded into heap values, no "awaited" state and no second store.
- **Entries.** By what changes alone: a file's index of item hashes (keyed by file content); a
  macro definition's tokens; an item's attribute flags; an impl's or trait's associated items; an
  impl row; a name binding group (the finest sound grain for name resolution is a strongly
  connected group of `(module, name, namespace)` bindings; its size on real crates is not measured);
  a function's lowering, inference, borrow check and runtime MIR as steps of one per-function store
  keyed before lowering; a constant's value.
- **Storage.** Live item sets for crates being edited, sealed sets for crates that are not, behind
  one `Entries { get, put }`; pins name an index value the collector follows.
- **Memory.** A value is read from pool pages for the length of a read, or decoded into the asking
  task's frame; nothing is a global-heap memo for the daemon's life. Edges of a sealed group are not
  held.
- **Deleted:** dependency images as decoded heap values, `check_cache`'s per-crate part, the
  crate-keyed `frozen::items` and runtime-MIR records, `eligible`'s refusal of local crates, the
  home-path refusal, `Awaited` and `lower_awaiting`, `BLAZE_DEF_MAP_PAGES` and `BLAZE_ITEM_STORE`.

## What is not redesigned

Code generation and LIR, the launcher and live patching, the store's read path and pool, cross
compilation, the terminal output, the profile. Admission and the give-back ladder are a fourth
area with the same smell (forecasts 500 times low); it is kept as is until levels 1-3 have shrunk
what the daemon holds, then judged again.

## Order of building

Each step ends with its number on a quiet machine before the next begins, and with what it replaces
deleted. No step adds a switch that outlives its own build.

| Step | What | Judged by |
|---|---|---|
| 0 | A clean baseline: the quiet test table against main and the edit-loop table (hello-world, bstd hello-world, reth; tempo if it builds) on the good tree | exists |
| 1 | Level 1: the certificate wired as the request loop's first state; the old no-op answer deleted | repeated `build` and `run`: 0 jobs; hello-world no-op in single-digit ms daemon side; reth no-op under 100 ms; the same after a restart |
| 2 | Level 2: unit keys from recorded reads; seals as the one validity mechanism; the three older mechanisms deleted | reth private body edit under 33 dependents: 1 unit remade unless a reader read what changed; rows verified bounded by the edited crate plus readers |
| 3 | Level 3 for functions: the one per-function store keyed before lowering, MIR as a step; `check_cache` and the crate-keyed record deleted | edit one function of N: N-1 hits with 0 lowerings; tempo's top-crate codegen memory |
| 4 | Level 3 for items, impls and names; images, `Awaited`, `eligible` deleted | restart with nothing changed resolves 0 crates; reth restart bounded by the edit since; daemon memory warm |
| 5 | Admission and give-back judged again on what is left | tempo cold build under a stated ceiling |

## Open questions (to settle before step 2)

1. What do durability levels, summaries, the floor and `reach` give that seals with per-row reads
   do not? (Asked of the lane that built seals.) If something, it is stated as part of the one
   mechanism, not kept as a second.
2. Can a certificate's "units that read this directory" be kept without a second index, by pointing
   at the unit records' own reads?
3. How large are name-binding groups on core, std and reth at name grain? If most of a crate is one
   group, level 3 for names is per crate in practice and the saving must come from the certificate
   and seals.
4. Proc macros: a body or item a proc macro wrote needs the expander's identity as a read (the
   compiled module's content hash) and a rule for expanders that are not pure. Not designed here.
5. A step blaze cannot trace: how many units of reth and tempo are in that state today? (A count
   from the profile.)

## Review, 2026-11-12, and what it changed

A reviewer outside the work read this document, the code and Relax's source. Where the text above
and this section disagree, this section holds; the levels above are rewritten when step 1 is built.

**Kept.** The idea; the certificate first; the daemon; crates as plan nodes.

**Changed.**

1. **One primitive first.** The form base key → read-lists → answer already exists four times
   (`hir-ty/src/body_store/record.rs`, `next_solver/goal_store/slots.rs`,
   `blaze-usercode/src/index.rs`, `serve/unit/compile/parts.rs`). It becomes one type with one
   simulation test, and the existing users are ported onto it with no change in hit rate, before
   anything new is built on it. Known defect to fix in it: a base whose four slots hold other
   read-lists is never stored again in a write-once store.
2. **The certificate is a fold, not a scrape.** Every step records its outside reads when it makes
   them, with the hash at read time (the sandbox's log, the front end's file and `env!` reads); the
   certificate folds them. Its key is `(request shape, tool)`: no root id in a key (the root is a
   read). Because the store is write-once and a certificate's bytes are not a function of that key,
   it is base → the names read (with the environment predicate's identity) → an answer keyed by
   those names' values, holding content only; stamps and "the last answer for this directory and
   shape" live in a machine-local ref written by rename, with a crash simulation. The root recorded
   is the pinned view the build read plus its own writes, never the state left afterwards (a file
   saved mid-build must miss). A stat stamp holds size, mtime, ctime and inode and is not trusted
   when taken too close to the change.
3. **The plan is a record** (manifests, lock, config, cfg probes). There is one walk, the engine's;
   a miss hands it what changed. No third walker over an old plan's dependents.
4. **The engine has one validity rule and five shortcut proofs** (`Storage::holds`: seal,
   durability, summary bits, floor, reach). They are not merged by argument. A census counts hits
   per branch on reth and the hello-worlds; a shortcut is deleted when the census shows it unused
   and the refinement tests pass without it, one per piece of work. Summaries stay while seals'
   soundness rests on them. Seals are not restored after a restart: a crate found by its record
   key needs none.
5. **A unit's key is not a crate.** A unit stays a node of the plan (build script, features, crate
   type, link); its record is a manifest: its functions' record keys, its item checks' verdicts,
   its outside reads. After an edit the work is the functions whose keys moved and an incremental
   link. `compile/parts.rs` (a per-crate read list nothing fills) is deleted.
6. **A key from the parse alone, as Relax's `compiler/src/cache/deps.rs` does** (credited: MIT):
   an item's piece is its text with bodies cut; its names are the identifiers in it; it depends on
   whatever is found under those names in its crate's universe, hashed per strongly connected
   group. That key exists before any resolution, covers absent names for free, and removes the
   glob-cycle grain problem. It is the per-item first check under blaze's semantic identity and
   fact traces, and the early identity for proc-macro bodies and closures. To measure first: how
   far common names (`new`, `from`) fan out on reth.
7. **Crates nobody is editing stay at crate grain** in the front end (a def map keyed by its own
   files and its dependencies' export digests); per-item entries are for crates being edited. Rule
   65 is met by the item key of point 6 standing in front, not by exploding quiet crates into
   millions of entries. The maintainers (incremental name resolution and impl tables) stay and
   their output is persisted per crate; most of the 191.6 s restart is waiting on them.
8. **Reads outside queries are closed as a class**: a driver gets no database handle outside a
   tracked frame. Until then no key is built from traces of work that reads that way.
9. **An untraced step is a sandbox gap, not a state** (rule 9). Count them on reth and tempo
   before step 1; one per project turns the certificate off for it.

**Order, revised.**

| Step | What | Judged by |
|---|---|---|
| 0 | The clean baseline (quiet test table against main; edit-loop table) and the span breakdown of reth's 46-54 s body edit | exists, with rows |
| 1a | The record primitive and its simulation; `body_store` and `goal_store` ported, hit rate unchanged | compiled and tested alone |
| 1b | A sans-IO `serve/request.rs` on it; read capture at each source into one sink; the `job.rs` hunks at three named anchors; the old no-op answer deleted | **reth's first no-op after a daemon restart: ≤ 100 ms client wall, 0 jobs**, on a named commit; and a mutation suite green: a body edit, a variable a build script reads, an outside file it reads, a manifest edit, a lock edit, a save mid-build: each misses and names the input |
| 2 | Decided only after 1b's numbers and step 0's breakdown: unit manifests of per-function records with the parse-level key in front; engine shortcuts pruned by census | reth body edit under 33 dependents |
| 3 | Front end of quiet crates persisted per crate with the maintainers' output; the first edit after a restart | reth: first edit after a restart |

**The risk that decides the rest.** Read capture may never be complete (driver reads, the
maintainers' pushed state, the environment predicate, build scripts). Step 1b tests it: if reth
cannot pass the mutation suite with no-ops hitting nearly every time, levels 2 and 3 key from text
as Relax does, not from traces.

**How it is cut.** No piece is written on top of another piece's uncompiled code. The primitive is
compiled before anyone builds on it. The engine's deletions are one per piece of work with the
refinement tests between. Nothing requires reading `job.rs` whole: the hunks are given by anchor.

## The whole system (added 2026-11-12, after the area designs)

The owner asked for the end state of everything, not only the three levels above. Five area
designs were written from the code and the best existing systems, each in this form (the job and
its invariant; how the best do it; today with its smells; the end state; what is deleted; the order
with its numbers; unknowns; how to cut it). They are the detail; this section is what they add up
to and where they overrule the text above.

| Area | File | End state in one line | Biggest deletion |
|---|---|---|---|
| Daemon, protocol, proof | `end-state/daemon.md` | The daemon is a cache of evidence and pages; the certificate check is one function in a crate with no compiler, run by the client; each request is a sans-IO machine; work starts only with a typed cause | `noop.rs`, `job.rs` as one shared file, the second wire schema |
| Outside inputs | `end-state/outside.md` | One read form `(ask, answer digest)` over one Merkle tree kept by one watcher; build-script runs and macro calls are records keyed before the work | the vouching layer, proc-macro verdicts, "untraceable" as a state |
| Memory and scheduling | `end-state/memory.md` | Nothing held between requests; one gate every page commit passes, so the ceiling holds by construction; evict pool pages, then pre-empt the younger task | the give-back ladder, forecasts, pressure handling, ledger turns |
| Code generation | `end-state/codegen.md` | No codegen units: instance, code, facts and program records; an edit is a delta to a program manifest, placed by the launcher or a file writer | per-unit code generation; four of five ways to run a program |
| Disk and sharing | `end-state/disk.md` | A home is a closed tree with one table of owners and one tracing collector with admission at commit; a pack is the same bytes as a store extent | the second collector, keep-by-default, separate travel formats |

**Where the area designs overrule the text above.**

- *The check runs in the client* (`daemon.md`), with evidence from the daemon's watch token, or with
  no daemon from a stat sweep under git's rule for recently changed files. A no-op then needs no
  daemon and is still sound. The daemon runs the same function for saves and agents.
- *There is no untraced class* (`outside.md`): a read set is traced, or declared for the documented
  sandbox opt-out and reused on those declared inputs; an incomplete trace is an error naming the
  call; a file that moved under a run ends raced with no record written.
- *Memory is not deferred* (`memory.md`): read-once values as frame scratch and the commit gate come
  beside step 1, because tempo cannot finish a cold build without them.
- *Storage has one physical form* (`disk.md`): sorted batches with index runs, not a live and a
  sealed set. The `Entries { get, put }` interface stays; what is under it changes.
- *Units go* (`codegen.md`), which also removes the per-crate read list and the surface digests.

**What of today's uncompiled work this supersedes.** Said plainly, so nobody builds on it: the
give-back ladder and the ledger by need (lane memgive); live and sealed item sets and pins by index
as separate forms (lanes storegc2, storegc3); surface digests, positioned interface text and
`Parts` (lane cutoff); the kept no-op answers per shape and the vouching fixes for listings and lone
files (lanes noopkeep, reth), which stay only until the certificate replaces them; `Awaited`
(records lanes); the crate-keyed item and runtime-MIR records. What carries over: the certificate's
record and tests; the per-item entry store interface; macro-definition entries and the file index;
name reads as facts and the early function key; seal groups, the precise walk and `row_reads`; the
purity summary on a transient body; the cache inventory and its guard; `Pinned::dir_id`.

**One programme, three tracks that do not share files.** Each step is whole in a few files, is
compiled before anything is built on it, and ends with its number on a quiet machine.

| Track | Step | What | Judged by |
|---|---|---|---|
| all | 0 | A gate that compiles every test target; the quiet test table against main; the edit-loop table; the span split of reth's 600 s cold build and 46-54 s edit and of tempo's last unit | the tables exist, on a named commit |
| A: the request | A1 | The record primitive, compiled alone; two existing users ported with the hit rate unchanged | its simulation; the users' own tests |
| | A2 | `blaze-certify` and the request machine with the certificate as the first state; the certificate written by the build that did the work; `noop.rs` deleted | hello-world: a hit on the second run, 0 jobs; reth: first no-op after a restart at most 100 ms, 0 jobs; the mutation suite green |
| | A3 | The one read form with sparse heads for machine paths and path dependencies and one watcher; the vouching layer deleted | runs 2 to 5 of a hello-world: 0 jobs and 0 disk looks; the first no-op after a restart hashes 0 files |
| | A4 | The client runs the check; the leaf table, the watch token, the stat sweep | hello-world no-op under 10 ms with a daemon and under 20 ms with none |
| B: memory | B1 | Read-once values are frame scratch in every per-instance walk, link and the profile's writers | tempo cold build finishes; `mir` heap 18.9 GiB to under 1 |
| | B2 | The commit gate with its simulation; the pool sized by room; per-tag budgets deleted | tempo cold under a stated 64 GiB ceiling: 0 kernel kills |
| C: disk | C1 | The closed home with its table of owners; a dry run the owner reads first | the owner's Mac: unowned bytes 860 GB to 0 |
| | C2 | One collector with admission at commit | a home filled past budget with garbage present never exceeds it |
| then | D1 | The program manifest; an edit is a delta; per-unit code generation deleted | reth private body edit under 33 dependents: 0 code generation in dependents |
| | D2 | Instance records keyed before lowering (the parse-level key in front), facts as group records | tempo: no codegen task over 10 s; hello-world cold run |
| | E | Engine shortcuts pruned by census; quiet crates' front end persisted per crate; the rest of each area file in its own order | each area file's numbers |

D waits for A2's number and step 0's span split (the review's rule: decide the edit design from
measurements, not from this text). Everything in E is ordered by its own area file.

**Decisions that are the owner's.** (1) How the work is run: `daemon.md` names the rules that
lanes only write and one runner builds as the cause of a day with no clean table, and proposes that
a builder compiles its own crate before handing over. (2) Under `edit`, errors found only when
instances are made (the recursion limit, duplicate symbols, errors after monomorphization) may
arrive after the program started: acceptable or not. (3) Which signing key blaze trusts for release
packs. (4) What under his `~/.cache/blaze` may be deleted.

**Reviewed so far:** the three levels (above). **Not yet reviewed from outside:** the five area
files and this programme; that review runs before any step beyond A1, B1 and C1's dry run.

## Second review, 2026-11-12: the shared page and the programme as it stands

A second outside reviewer read the five area files and the programme against the code and Relax's
source. Verdict: **do not build from the area files yet**; they contradict each other and three of
them undo points of the first review. This section resolves each contradiction by name. Where an
area file disagrees with this section, this section holds, and the file is corrected when its
area is built.

### The shared page (every area obeys it)

1. **One read type.** `Read = Outside(Ask) | Fact(FactRef) | Record(key)`, one digest each;
   "nothing" is a digest. `Ask` is `outside.md`'s (a node, a listing, a variable, ...). A
   certificate's directory groups are coarse Merkle reads over `Ask::Node`.
2. **One lookup form, in the store.** Base key → variants (read-lists) → answer, as
   `crates/blaze-record` (compiled, 11 tests). Past its bound the oldest variant is no longer
   searched; it is never a refusal and never a build failure. `disk.md`'s physical form must hold
   it.
3. **A base key is an identity, never content.** A certificate's key is `(request shape, tool)`;
   a program's key is its roots' identity (path and kind); content enters as reads. No root id and
   no item content in a base key. (`certificate.rs:90` has the root in the key today: wrong.)
4. **Which root a certificate records:** the pinned view the build read plus the build's own
   writes. A file saved mid-build must miss. Stat order: stat, hash, stat again; a stamp taken too
   close to the change is not trusted.
5. **One walk, the engine's.** The request machine hands it what moved. `walk_from` over an old
   plan does not survive.
6. **A producer is named by its code identity** (rule 52; `Memo.producer`), never by a hand-bumped
   version.
7. **Persisted stat evidence has one home:** the VFS journal carries input stamps (that is the
   "leaf table"); output stamps live only in the placement record.
8. **Codegen units go; unit manifests stay.** A unit is a plan node; its manifest (its functions'
   record keys, its checks' verdicts, its outside reads) is what the certificate names, what the
   collector traces through, and where a build that failed or has an untraced step still roots
   its records. The collector's roots are manifests, not certificates.
9. **Grain.** Crate grain only for crates whose sources cannot change (registry, toolchain); item
   grain for every workspace and path crate. One coarse digest per crate that survives a restart
   (its content and its dependencies' digests, rebuilt from records) stands in front as the first
   check; seals are not restored.
10. **The parse-level key is a first check, not an identity** for macro-written code: in Relax a
    macro outside item position makes the key coarse, glob imports are depended on by every item
    and the sysroot is one fingerprint. It does not remove the name-group problem.
11. **What the daemon holds between requests** is decided by one measurement (a pinned record read
    against a memo hit, in nanoseconds). Until it is taken, the engine's state and the maintainers
    stay warm; `memory.md`'s "nothing held" is conditional on it.
12. **Mapped files.** The placed image is a placed output in its own file, which the launcher
    maps. It is never a store extent.

### Claims in the area files that are false against the code

- `outside.md`, "no step is untraced": on macOS the tracer cannot see what `/bin/sh` and programs
  in `/usr/bin` read (`sandbox/macos.rs:21-25`); refusing those refuses every build script that
  runs `cc`, `sh configure` or `git`. Needed: a declared model for protected programs (argv and
  the compiler's dependency file), listed as trusted.
- `memory.md`, refusing a commit and unwinding: the unwind allocates (`serve/plane.rs:553-563`), so
  a refusing gate aborts the process. Never refuse a panicking thread; a small unwind reserve per
  worker; count a task's heap to zero at its end instead of destroying it; LLVM, C dependencies and
  the wasm runtime are outside the gate and named so. Pre-emption is wait-die (the younger asker
  fails, the eldest waits on a deadline), not wound-wait.
- `daemon.md`, "reth first no-op after a restart ≤ 100 ms" at the first step: it needs persisted
  stamps, which come later; on Linux a restart otherwise hashes every file. The number moves to the
  step that persists stamps.
- `codegen.md`: LLVM's memory is not in a task's frame (it uses the system allocator); "a no-op
  start is one mapping" must be the placed image of point 12.
- `disk.md`: roots are manifests (point 8); its rejection of per-crate files rests on a wrong count.

### Cut from scope (not built until the rest has numbers)

The client-side check and stat sweep; the store's new physical form, packs as extents and fetch
on miss; the memory gate beyond a counter, heap destruction, the machine ledger, LLVM worker
processes; macro state as a read (instead: a fresh instance per call, moved state detected by a
digest of the whole linear memory, a module that moves state run in source order as one record);
the program manifest's fact index; instance records and everything after them in `codegen.md`;
the profile's new writer (first try reusing one writer).

### Copied from Relax rather than designed (MIT, credited)

The certificate as `GraphImage` with racy stamps and probes (its racy-stat test seeds our oracle);
the store commit by two index slots with a header flip and the tail verified at open; `prune`
(roots are what a program's set names, plus a cap) as the collector's interim at pack grain; the
reload transaction with its check for old code on a thread's stack; statics by offset from a
thread-local base for tests as domains.

### The programme, re-cut

| Step | What | Judged by |
|---|---|---|
| 0a | **A compiling head.** The superseded lanes' uncompiled code is reverted, not left; every test target compiles; the gate keeps it so | `cargo check --workspace --all-targets` on a named commit |
| 0b | **The counts.** Untraced units per OS with cause on reth and tempo; the `Storage::holds` census; name-group sizes and common-name fan-out; state-moving macro modules; a pinned record read against a memo hit | rows, on a named commit |
| 0c | The quiet test table against main; the edit-loop table; the span split of reth's cold build and 46-54 s edit | the tables |
| 0d | The owner's disk: the closed-home dry run for him to read | his decision |
| A1 | The record primitive (done: compiled, 11 tests); existing copies ported, hit rate unchanged | each crate's own tests |
| A2a | Read capture: the `Read` type; every unit kind's record carries its outside reads; no database handle outside a tracked frame | the mutation suite: each real change misses and names the input |
| A2b | The daemon-side certificate as the request's first state, written by the build that worked; `noop.rs` deleted | hello-world: a hit on the second run, 0 jobs; reth no-op under 100 ms warm |
| A3 | Persisted stamps in the VFS journal; one watcher; the vouching layer deleted | the first no-op after a restart hashes 0 files; reth after a restart at most 100 ms |
| B1 | Read-once values are frame scratch | tempo cold build finishes; `mir` heap under 1 GiB |
| D1 | Decided after 0b, 0c and A2b: unit manifests; an edit as a delta | reth body edit under 33 dependents |

**The measurement that decides the most:** untraced units per OS. If it is not zero on the Mac
(expected, from protected `sh` and `cc`), certificates do not form there until the declared model
exists, and that model moves ahead of A2b.

### How the work is run (the owner's decision, proposed)

Both reviewers and the area designer name the same cause for a day with no clean table: lanes
compile against each other's half-written files (79 of the day's 377 commits were cuts of code
"written, not yet compiled"). The smallest change: a cut admits ONE lane at a time; its files are
applied onto the last green head in one scratch checkout with a warm target and
`cargo check --workspace --all-targets` is run; if it passes it is committed, if not the errors go
back to that lane and the head does not move. The measurement table is taken on the green head,
on reserved cores, before the first lane of the day is admitted. Small crates are compiled and
tested by the main agent on the Mac within minutes of a lane's report (done today for
`blaze-record` and `blaze-query`; it found a stale answer in the seal code in ten minutes).

## Five more areas (2026-11-12): written, not yet reviewed or reconciled

`end-state/engine.md`, `end-state/check-test.md`, `end-state/toolchain.md`,
`end-state/interface.md`, `end-state/delivery.md`. Each was written before the second review's
shared page and has not been read by an outside reviewer; where one disagrees with the shared
page, the shared page holds. Known disagreements to settle when each area is built: `engine.md`
moves the maintainers' algorithms to demanded records (the first review keeps the maintainers
until a measurement says otherwise); `check-test.md` rejects in-process domains for tests (a fork
from a placed zygote instead), against `codegen.md`; `delivery.md` asks for a dedicated measurement
machine and ends the integration branch as a way of working (rule 35 was rewritten the same day to
the smaller form: one lane at a time onto a green head). None of them changes steps 0 to B1 of the
programme above; they order what comes after.
