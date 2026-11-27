---
status: plan
summary: The drive's names, keys, records and task types on the new item layer (crates/blaze-drive), which the one driver in blaze-engine (end-state.md D157) and the `blaze` binary (crates/blaze-cli) build on. Built - names and keys from text, the record envelope, the answer table with its readers, part reads by cell, conditions, the task types, verified bytes, what did not run by cause, a unit's view, the last-answer pointers, the content-id table and the profile. Not built - the plain store under the command, findings mapped through expansions, the crate list as a record, marks carried by every token, item-position expansions read from their token record.
date: 2026-11-15
---
# The driver

bottom.md section 9 says what the driver is. The driver is blaze-engine's ready-set scheduler
over `TaskKind` (end-state.md D157); names are solved at module grain (D158); a task that lacks a
fact returns `Need`, parks and resumes (D161). This doc is the layer they build on:
`crates/blaze-drive` (`#![no_std]`, no `alloc`, sans-IO: blaze-own's collections, the allocator
passed in) and the system side in `crates/blaze-cli` (the `blaze` binary, through bstd only).
How a configuration space is checked in one run is conditions.md's; this doc says only what
`condition.rs` holds.

## Names, sources and keys

A **name** is what a task is about: `(kind, subject)`. Subjects: a file's is the hash of its path;
an item's is its item key (bottom.md 3); the root `Units`' is fixed, a manifest's its path's. A
**source** is the text the task is keyed by, known before the work:

- `FileItems(file)`: the file's content id and the edition (from the part that listed it).
- every item task: its part digest in its file's `FileItems` record (signature text, body text,
  the module's declarations).
- `Fetch(crate)`'s parts carry the record key itself (a crate with no text here).

**Key** = H(tool, kind, subject, source). A key never holds another record's key (bottom.md 3);
what a task read of another record is its read list: (name, out digest) pairs.

Task kinds (`key.rs` `Kind`, discriminants the wire's): `Units`, `UnitGraph`, `Fetch`,
`FileItems`, `ModuleScope`, `Group` (a cycle of scopes, one task), `Expansion`, `Signature`,
`ImplBucket`, `BodyCheck`, `Code`, `Test`, `CrateScope`, `MacroModule`, `Surface`, `Owners`,
`Script`, `Links`, `Native`, `Given` (what the command gives: a file's content read by path, a
fact), and the lint unit. What a command gives a task is a `Given` name like any other
(`key::content_of`, `key::fact_of`), never an ambient input.

## Records and validity

The envelope (`record.rs`) is one buffer: name, source, **out digest** (what a reader sees of
it), read list, parts (name, how to locate it, its start in the file), findings (rule,
item-relative range, message), and the kind's own payload.

- A record with no reads is stored **whole** at its key.
- A record with reads is stored at its **value key** H(key, reads' digests), and a **shape** at the
  key lists the read names, so a lookup that knows only the key reads the shape, computes the
  value key from today's digests and reads that. A second read set under one key is stored at its
  own value key and found by its row, not by the shape (first writer of the shape wins; a shape
  names reads, it never answers).

**The answer table** (`answers.rs`) is the checkout's index, stored as content-addressed pages
under a root: a row per name, sorted by name, paged by the name's first bits. A row holds the
key, where its record is, the source, the out digest, its home and start, its read list, its
parts, and its **readers** (the reverse index). Pages are read in place (`PageView`); nothing of
the table is held in memory but the pages a run touches.

**Forward check.** A job whose key equals its row's key and is not dirtied stands with no store
read. When a job settles with an out digest unlike its row's, its readers are dirtied: each is
re-decided at its value key from today's digests and run if absent. Nothing else is visited.

### Part reads

A read is (name, PART, digest) (`cells.rs`). The part is `Whole`, or one CELL of the record (a
128-bit key). A kind that has cells gives one pure function, `CellOf`: a search in the mapped
bytes answering the cell's digest or `Seen::Absent`. A reader's entry is valid when today's
record gives the same digest for that cell; `Absent` is a value, so a name that was looked up and
not found is a read like any other. Forward from a change, the changed record's readers are each
asked their cells; only a reader whose cell moved is dirty.

## Tasks and Need

`task.rs` holds a task as the engine hands it to a worker: the command it serves, its `Input`s
(records handed to it, each with the digest a read of it records), its `Want`s, its
`TaskFailure`, the `Print` it files, and its `Class`: `Foreground` (what the command asked, or
what a running program waits on), `Gate` (`run`'s and `test`'s check beside the program) and
`Background` (speculative code). A task is a pure function of its inputs: `Done(outputs)` or
`Need(wants)`; a want is a name and its **home** (the record that lists it: its file's
`FileItems`, or `Fetch(crate)`). The scheduling rules are D157's and D161's.

- **What did not run, by cause** (`not_run.rs`). A task that failed `Why::Waited { on }` names
  only the need that stopped it directly; the answer puts each waited task under the first task
  down its `on` chain that failed for a reason of its own, and lists, for each cause, the crates
  not checked because of it (at most 20 a cause, the count exact).
- **The bytes a task parses are the bytes its key names** (`verified.rs`). A task that reads a
  file by path hashes what it read; when that is not the content id in its key it stops with a
  typed `Changed`, files nothing, and the command refuses its answer naming the paths. No
  re-read, no retry. The parse takes the `Verified` proof. Open: the parser's entry takes the
  proof type, so it holds in every crate, not only in blaze-cli.
- **A unit's view** (`view.rs`, test.md D191). A body in a file several units hold is checked by
  each; the name at a view and the `Full` plan say which rows each unit reads.
- **A body's owners.** A body waits only for its file's owners and their dependency closure, and
  reads only the scopes its check opened. `Owners` is a kept fact computed from the module tree:
  owners(file) is the union of its parents' owners, a crate root adding itself; its read list is
  each parent's items record and owners. Cold, it settles when every file reachable from the
  roots is outlined (the one barrier allowed); warm, there is none.
- **Tests.** A test's result is a `Test(test)` record whose read list is the code identities its
  run entered: after an edit, a test whose read list holds a changed function runs again, and
  every other test's record stands and its verdict is printed marked as reused.
- **The gate.** `run` and `test` fail when the check of their program's scope fails, lints
  included (bottom.md 11). `run` checks before the start, over the workspace bodies the program
  reaches; `Beside` is `test`'s (run.md, "The gate").

`fetch_task.rs` is the `Fetch` task (fetch.md): one lock entry's sources in the store.
`unit_graph_task.rs` is the `UnitGraph` task (units.md): which crates a request compiles, with
which features and cfgs, as one record.

## One check covers every configuration

`condition.rs`: a `Condition` is a sorted list of (variable, value), at most 16, inline and
`Copy`: a target is one variable (it fixes every `target_*`), each package's feature a boolean,
`test`, `debug_assertions` and each `--cfg` a boolean. The `Space` holds values per variable and
the feature dependencies as closed implications. Reading a variable a task does not fix answers
`Need(Cfg(var))`, re-entered once per value `Space::fork` allows; its record is stored under the
condition it read (`key::at_condition`), and a lookup takes a record whose condition the asker's
implies. More than 64 conditions for one item is `ConditionError::OverCap`, a finding naming the
item and its variables. D158 replaces `Condition` with cfg read as a variable; conditions.md is
the design.

## Scheduling on many cores

The engine's pool is D157's. `table.rs` is the open-addressed map keyed by content ids that its
code pool uses: linear probing, backward-shift removal, at most half full, doubling; every lookup,
insert and remove is constant time on average.

## The system side (blaze-cli)

- **The sweep follows what was read; nothing walks the checkout** (`crates/blaze-cli/src/sweep.rs`).
  The leaf table (`BCL4`) holds exactly what the last answer read: each file (path, stamp, id);
  each module slot (`mod x;` asks `x.rs` and `x/mod.rs`: one is read, the other an absent probe);
  each manifest read or probed absent; each directory listed for a member glob. A repeat stats
  those and lists those directories, nothing else. A stamp (len, mtime, ctime, ino, dev) settled
  and equal keeps the id; else the file is read and hashed. A new file matters only if something
  probes for it, Rust's rule.
- **Pointers** (`pointers.rs`, read-addressed.md). One last-answer pointer per (tool, request
  shape), a file `.blaze/v2/at/<name>` named by 16 hex digits of H(tool, shape), at most `KEPT`.
- **Units** (`crates/blaze-cli/src/units.rs`): blaze-manifest's `Layout`, run when a manifest or
  listing row moved. The root's source is the digest of those rows; it lists members at
  `Locate::Source`, so a manifest edit re-makes only the members whose share moved.
- **Profile** (`profile.rs`): per task kind, how many jobs stood on their row, were found in the
  store, were made, were fetched, and how many times tasks were entered; and the store traffic.
  Time is the caller's (the drive has no clock).
- **Forms.** The tool key is the binary's linker id (`LC_UUID`, GNU build-id) and each keyed
  form's magic (`FORMATS` in lib.rs: record, answer page, answer root, last answer). `file:line:col`
  is made at print time.

## Check times: the end state in numbers

The edit path's floors are D168's. The numbers the command is held to (D114):

**Held to** (wall, a quiet machine; W = the workspace's own bodies; dependencies and std are checked once a
machine: their records are found by key in the machine's own content-deduplicated store, so their first
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

## Build scripts in the command

A script run is a task, kind `Script`, one a (script unit, target), planned from text and run
before the names that read it (`crates/blaze-cli/src/engine_scripts.rs`).

- KEY: the script runtime's producer id, the script unit's identity, the target, the unit's
  features and cfg, the profile facts a script may see. Nothing of the machine. A script's output
  is a read, never part of a key: the crate's key is its identity without the script's digest.
- READ LIST: every fact the run asked for (each environment variable by name, each file it read
  outside its package, each probe) and the output record of each script whose `DEP_<LINKS>_*` it
  read (the links graph).
- OUT: the `cargo:` lines in order, and `OUT_DIR` as a tree record (paths, modes, content ids).
  A body's read list names the generated file's digest, so a script whose output is byte-equal
  after its sources changed moves nobody.
- WHERE IT RUNS: a pure-Rust script is compiled by blaze to wasm64 and run in-process under
  Wasmtime, sandboxed; a script that spawns native tools or links C runs them in the hypervisor
  micro-VM sandbox.
- Every unit reads the `Script` of its own side; a `Script`'s source holds its own module's
  digest, never the module listing whole.
- FAILURE: a finding naming the script, its package and the line it failed on; the unit that
  needs it is "not checked: its build script failed", and the rest of the answer goes on.
- A dependency's script output comes from the machine's store by key like any record; only a workspace
  member's script, or one whose inputs changed, runs here.

## The store under the command (not built)

The command reads records through blaze-cli's `PackStore`. The design:

1. One store a user, one root (`<home>/v2/store`), read and written by every checkout and
   worktree; the store writes nothing in a checkout. No home is a typed refusal naming `HOME`.
2. A NAMESPACE, NOT A PLACE. `Sharing::place` becomes the record's namespace: `PUBLIC` when every name in it
   is public AND this machine may write it (a trusted builder); else `LOCAL`. On a developer's machine
   everything computed is `LOCAL`; `PUBLIC` records arrive only verified, from an origin, and only when
   fetching them is chosen (built, off unless chosen). `Place`, `Layered`'s
   three stores and `Machine::Off` go.
3. Only the leaf table is about a checkout: it and the last answer's key for each request shape
   hang from one named root a checkout, named by a digest of the checkout's path. Certificates,
   answers and pages are keyed by content and found by any worktree with equal content.
4. One driver a process: `PlainPlace<D: Driver>` is generic, tested on the simulation; the
   command gives it a bstd driver rooted at a `Dir<ReadWrite>`.
5. In place: a get returns `&'m [u8]` borrowing `Maps`, the command's one append-only owner of
   mappings; one `unsafe`, inside `Maps`, its assumption stated. The open maps nothing; a
   segment is mapped at its first probe and checked once a boot.
8. WHAT IS PUBLIC IS A POSITIVE LIST BY SUBJECT (corrected 21:10: an arm keyed on "ran natively" could never
   fire, and with only `CrateScope` names public, every registry crate with a build script was shut out of the
   shared cache). A `Script` name is public exactly as a `CrateScope` name is: when its subject is in the
   public set. The command adds a script's subject only when its crate is public AND it is placed on the wasm
   path (decided where the part is named, before any run). A natively placed script's subject is never added,
   so it and every reader of it are `LOCAL`; a subject forgotten is local, never public. Placement is in the
   `Script`'s key, so a native and a wasm run are never one record. Until the deterministic sandbox replaces
   the native runner, a crate whose script must run natively, and what reads it, stay out of the shared
   cache: D112's "local records only".

## Not built

- **A finding inside an expansion is reported where its tokens came from.** The task that makes
  a finding in expanded text maps its range through the expansion record's origin column before
  filing it: input tokens are reported at their range in the user's item; otherwise at the call,
  naming the macro ("in the expansion of `m!`"). Mapped at the making, so a kept finding and a
  fresh one are equal bytes; a nested expansion maps through each level.
- **The crate list is a record.** The world is made once, by `UnitGraph`, and readers read
  columns of that record in place: one row a crate, sorted by key (key, deps, side, the
  package's place, lang and sysroot ids, the scope rule, its `Script` part's name). An edit makes
  no world on any thread.
- **A token keeps its mark** through the lowering and the front end: where it came from and, for
  a definition token, the definition's site and the macro's kind of hygiene. A path in expanded
  text is resolved by the mark of its first segment, in one function (`$crate`, a bare name in a
  `macro` body and a macro call's own name are three uses of it).
- **An expansion is its token record** (D122). The record is built (`crates/blaze-tokens`:
  per-token word, joined bit, lexeme, hygiene context and origin; `TokenRange`; one digest; one
  rendering, `shown`, for a person). Not switched: item-position expansions in blaze-scope still
  render tokens to text (`render_with`) and lex it, and `outline_tokens` in blaze-file-items has
  no production caller. End state: expansions' items are outlined and lowered from token ranges,
  digests of expansion-made things are over tokens, and nothing lexes a rendering.
- The certificate's read list (files are in its key: a changed file is a miss, then forward); to
  revisit with FSEvents. Pages by name prefix or by module: measure on reth.
