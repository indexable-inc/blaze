---
status: plan
summary: The item layer and the commands above it, rebuilt from scratch, one consolidated spec. Pull at item grain, except names, whose per-module records are filled crate-wide the first time and updated per module after; a task never waits (it returns Need(keys) and is entered again); a dependency is only its records; the type checker asks through Items and Source. run, test and check (export later) run code records in the program's own process. Zero third-party crates, no std or alloc in blaze's own code, with dated exceptions. Floors with ratios, open questions and the docs it supersedes.
date: 2026-11-15
---
# The bottom layer, from scratch

Andrew, 2026-11-15: "we can scrap everything imo and build the lints FOR this new design ...
delete old code and just start from scratch"; "start with the right design and then iterate ...
iterate fast".

**Why.** The inherited item layer (hir-def, hir-expand, base-db: 189k lines) resolved a crate
whole, named items by arena slots, kept syntax trees alive, and let any question about any item
fall into source text. On 2026-11-15 a cold `check --no-daemon` of hello-world cost 21 to 45 s
and 3.4 to 4.6 GiB, because every item of `core`, `alloc` and `std` was resolved to answer
questions about a few hundred of them. That layer was deleted on 2026-11-15 (#2197); the type
checker was parked, not deleted.

Each decision is stated once, in its final form; detail lives in [identity.md](identity.md),
[run.md](run.md), [typeck.md](typeck.md), [reconnect.md](reconnect.md),
[std.md](std.md#two-builders), [third-party.md](../third-party.md) and [target.md](target.md).

## 1. Goal order

The statement the design is judged by, Andrew, 2026-11-15 19:53: "assume you want best
performance on 128 core machines (say graviton 5 or epyc) but a lot of people will be using on
typical macs too. we want to have fast incremental compilation for realistic huge rust monorepos
with millions of lines of code. needs to work with enormous crates. SoTa at least, structure of
arrays etc. incremental comp being short (including on large crates and large files in crates
even after restart computer so nothing running etc) = lints being fast > being able to get cached
stuff remote (r2/peer to peer etc) > memory usage > cold comp > disk gc being good/not locking >
disk usage. also need incremental test (only rerun ones that changed)".

The order, the earlier winning when two trade: (1) incremental compile short, large crates and
files and after a restart included, with lints as fast; (2) records from remote; (3) memory;
(4) cold compile; (5) disk cleanup that never blocks; (6) disk use. Replaced: the five-item order
of the first quote, because the later statements add remote as second. What each clause asks:

- **128 cores, and typical Macs**: no global lock or single-threaded stage on a hot path:
  interners read without a lock, caches sharded, a task's scratch memory its own, the first
  crate-wide fill of names parallel across module groups (section 6); one code path sized by the
  machine. Relax is what not to do here: it measured one mutex taking 20% of an app's cycles
  spinning, and its collection and import fixpoint are single-threaded.
- **Enormous crates and files**: nothing is recomputed per crate on an edit; the per-crate
  records (section 4) are lookups valid by per-module digests. A large file is re-outlined whole
  on an edit, at about 1 microsecond a line (measured): 30 ms for 30k lines; open: re-outline only
  the changed item's range (section 5).
- **Incremental test**: a test's result is a record whose read list is the code identities its
  run entered. After an edit the tests run again are exactly those whose list holds a changed
  function, plus tests with no record; every other test's stored result is reported, printed with
  its verdict and marked as from its record (not run again). Floor: one postings lookup per changed
  function plus the run of the affected tests; with nothing changed, the certificate alone.
  Mechanism: [run.md](run.md) (`Tests`).

- **After a restart** (no process, page cache cold): a repeat check reads one certificate and,
  when it stands, nothing else; records are packed with an index, never a file per record; the
  sweep is by stat; no source is re-hashed that a stat proves unchanged (sections 9, 10). **SoTA,
  structure of arrays**: columns everywhere (section 13).
- **Lints**: a task keyed from the facts it reads of one item; none walks a crate (section 12).
- **Remote**: a record holds nothing of the machine, so any builder's record is any reader's;
  never a request per item: a pack's index once, then byte ranges in parallel (section 10).
- **Memory**: records read in place; a task holds only what it pulled; the page cache is the only
  cache. **Cold**: std and public dependencies arrive as CI's records. **Cleanup, disk**:
  immutable records; collection is compaction and unlink, never a lock.

## 2. Principles

1. **Pull, at item grain.** The driver asks one question (check this body; run this program).
   Answering it asks for records; a record that exists is read in place; one that does not is
   made by its own task. Nothing is computed that no one asked for, with one stated exception:
   names (section 6), whose per-module records are filled crate-wide the first time a crate is
   seen. Replaced: "a crate is never resolved whole", because the producer decision of 18:37
   (section 6) resolves a new crate's modules together once.
2. **A task never waits** (Andrew: "why not explicit state machine sans io"). Logic whose states
   are the design is a hand-written sans-IO machine (`step(event) -> effects`; rule 14): name
   resolution, the run, the drivers. A compiler task that is recursive code (the type checker
   asks for a callee's signature in the middle of an expression) is not turned inside out and
   does not park: it is a pure function of its inputs that returns its result or `Need(keys)`.
   The driver gets those records and enters the task again from the top; what it read is a
   record, so re-entry is cheap; with no stack to keep, it can run on another machine. Replaced:
   "made by one small task, which asks the same way" and "two askers share one task (the store's
   flight)", because a task that asks and waits parks; the flight lives in the driver (section 9).
3. **A body's needs are remembered.** A checked body's record lists what its check needed; the
   driver has those before it enters the task the next time, so the usual check is one pass with
   no re-entry.
4. **A dependency is only its records.** A missing record of another crate is made by that
   crate's own task (which has its text) or is a typed stop naming the fact and the item; never a
   parse by the asker.
5. **Two traits** are everything the type checker may ask (blaze-surface):
   - `Items`: any crate; every method reads records; no method can reach text.
   - `Source: Items`: the crate a task compiles; its text methods take a `HasText` proof.
   A stage's entry point takes `&impl Items` or `&impl Source`; helpers behind it stay on the
   concrete database. Each question by `ItemKey` has a form by `Local`, the task's own number.
6. **A fork shares by content.** Two checkouts with equal text for an item find equal records.
7. **Decide at run time** (section 11): what a flag fixes elsewhere is a version in a slot here.

## 3. Identity and keys

- **Identity is `blaze_id::ItemKey`** ([identity.md](identity.md)): the hash of what the item is
  in (its module, impl, trait, enum or body), its kind, its name and where it is written (an
  index among equals, `Item::disambiguator`; a macro-made item adds the call's place). Bytes 0..8
  are its crate key's first 8; the top bit of byte 8 marks an item inside a body.
- **A rename is a new item**, whose work is reused by content: a body's key writes its own item
  as "self". No rename detection. A task's `Local` number is not `Portable`.
- **The crate key** (`CrateItemKey`) is the prefix of every item key of the crate. It never holds
  cfg, features or the target, so an item has one key on every target and feature set. Open:
  whether edition, crate attributes and dependencies belong in it (an edit of `#![..]` would then
  move every item key) or are read-list facts of `CrateRoot`. The code today hashes all of
  them, cfg included (`blaze_id::CrateItemKey`'s doc): to change.
- **A key is what the record is; a read list is what it read.** Every record has two columns,
  as in [run.md](run.md) and [target.md](target.md): the key, computed from its own text, the
  tool (blaze and its pinned nightly) and the namespace before the work; and the read list, the
  (fact, digest) pairs the work asked, absences included. A key never holds another record's
  key; a fact of another record is a read. Replaced: the first table's "key from" column, which
  held "the module's `FileItems`", "its own `ModuleDecls`" and "the keys its paths resolve to",
  because those are reads, and a key made of them cascades (read-addressed.md).
- **cfg, features, target**: each record's read list holds the cfg outcomes it read (as
  `ReadKind::Cfg` in blaze-expand); target layout (sizes, alignments, ABI) is in code keys only.
- **Positions** are item-relative in every record (rule 71). Only `FileItems` holds
  file-relative positions (an item's start), and it is keyed by the file's bytes, so a position
  moves exactly when its key does. `file:line:col` exists only when printed (section 12). Nothing
  is ordered by a local number; a sort for output or for a key is by item key.

## 4. The records

`K` is the key, `R` the read list. Crates: surface = blaze-surface, scope = blaze-scope, expand
= blaze-expand, impls = blaze-impls, typeck = blaze-typeck, run = blaze-run.

| Record | K (before the work) | R (validity) | Holds | Crate, state |
|---|---|---|---|---|
| `FileItems` | the file's bytes, edition, outline version | none | items in order: kind, name, disambiguator, content and signature hashes, item-relative ranges of signature, body and attributes; `mod`/`use`/macro-call entries. No tree | surface; built (from blaze-file-items' `Outline`) |
| `ModuleDecls` | the module's place in its file's text, the file's content, the edition | the cfg outcomes it read | names the module declares, by namespace; an item under an attribute macro is not a name until it expands | surface; built |
| `ModuleFile` | the `mod` item | negative reads of `foo.rs` and `foo/mod.rs` | which file, relative to the crate root | surface; built |
| `TextualEntry` | the module | the parent's entry, earlier `#[macro_use]` siblings | the `macro_rules!` scope at the `mod` line | surface; built |
| `CrateRoot` | the crate key, the root file's content | cfg outcomes | edition, `no_std`, preludes, extern prelude (with `extern crate self as`), `register_tool` | surface; built |
| `ModuleScope` | the module's key | its own `ModuleDecls`; per-name reads: `(module, ns, name)`, `Exports` digests, textual entries, derive helpers | bindings by (namespace, name): target key, visibility, origin, state; an `Exports` digest per visibility class | surface (form), scope (maker); built, 16/2 tests on 2026-11-15 |
| `KeyIndex` page | the module's key | its `ModuleDecls` and expansions | item key to place: `resolve` is one probe | surface; built |
| `MacroDef` | the definition's content (`MacroContent`), never its item key | none | kind, tokens, edition, flags, a derive's helpers, transparency | surface, expand; built |
| `Expansion` | the call's tokens, the definition's content, the call-site hygiene | env, cfg, included files, positions inside eager input | tokens with origins, hygiene contexts by structure, `Hole`s for `line!()` and the like outside eager input | expand; written, tests not compiling on 2026-11-15 |
| `Signature`, `Fields`, `Variants`, `AssocItems`, `Predicates`, `Supertraits`, `TypeAttrs` | the item's signature text (`signature_hash`) | what its paths resolve to, and each named item's key and signature hash (D51) | the lowered signature (types by item key), split so each is cut off on its own; `repr`, `non_exhaustive`, `fundamental`, stability, `#![feature]` | not built |
| `ImplHeader` | the impl's header text | resolutions of its paths | trait ref, self type, generics | not built |
| Impl directory and bucket | the crate key; the bucket key (trait for a self shape, blanket, inherent of a shape) | each module's impl digest (below) | the impls of one bucket, sorted by impl key | impls; built |
| `NoImplsOf`, `LangItems` | the crate key | each module's impl digest and `#[lang]` digest, from its `ModuleDecls` and expansions | "crate C has no impl of T"; lang items by name | impls and surface (duplicated); built from a list the caller hands |
| `CrateGraph` | the lock file and workspace manifests by content | none | crates by key, dependencies by name | impls and surface (duplicated); built |
| `BodyFacts` | the body's text | the signatures it read | what a body gives others: hidden types, const values, discriminants | not built |
| Checked body | the body's text, with its own item as "self" | the `World` reads, sealed (typeck.md) | types, adjustments, callees, captures, diagnostics; `needs` for next time | typeck; in writing |
| Finding | as its check's | as its check's | one lint or error, item-relative range, no level | not built |
| Fast code, optimized code, program manifest, test result | see [run.md](run.md) | see run.md | code records and what a run reached | run; machines built, drivers not |
| Certificate | the tool, the request's shape | the request's reads (section 9) | the answer: findings' and manifest's keys | not built on the new layer |

Replaced: `NoImplsOf`, `LangItems` and `CrateGraph` keyed by "the crate's roots", because that key
misses the impls and `#[lang]` items written anywhere in the crate (rule 21). Replaced: "nothing
is keyed per crate except its file list and root module": per-crate records exist where a lookup
must rule a crate out in one read (`NoImplsOf`, the impl directory, `LangItems`), each valid by
per-module digests.

Each record is one blaze-format buffer, written once in the caller's allocator, read in place by a
view that checks it once; two builders write equal bytes (rule 25; lists sorted at `finish`).

## 5. Files

- **The outline** (`blaze_file_items::outline`): text to items in one pass over the parser's
  events, no tree kept, in the caller's allocator; every item at module-level nesting; items in a
  function body belong to the body's record (the entry says whether it holds any). Section 17.
- **`FileItems`** is built from an outline and keyed by the file's bytes. Taken from Relax: packed
  tokens (4 bytes each) and LEB128 position tables for `FileItems` and expansions.
- **Bodies skipped**: a body is parsed when its own task runs (today the outline parses bodies in
  full: to build). **Not read yet**: 9 syntax errors in `core` and 1,531 in blaze's crates are
  syntax of blaze's extensions (the outline parses with none): to fix.
- **A large file** (section 1). Open: re-outline from the item boundary before the edit, reusing
  entries whose bytes did not move. Test: outline the largest files of reth and tempo.

## 6. Names

Reviewed by an outsider on 2026-11-15. No per-name, demand-driven resolver of Rust is known to
exist; rustc and rust-analyzer resolve a crate whole because expansion and imports interleave.
Replaced: the first form of this section, an "import group" found before resolution, because what
`use a::b::*` names is itself a resolution, item macros add `use`, `mod` and `#[macro_export]` only
after they expand, and grouping by module invents cycles that are not cycles of names.

**The producer** (decided with Andrew, 2026-11-15 18:37). Three grains were weighed: one record
per crate (any new name redoes the crate), per module updated incrementally, and per-name pull
from cold. The choice is the middle one:

- **First fill.** The first time a crate is seen (not fetched), all its modules are resolved
  together by the known fixpoint, in parallel across module groups (strongly connected sets).
  Each module's scope is its own record with its per-name read list.
- **After an edit**, only the modules whose read lists no longer hold run again: the edited
  module, and readers of a name that changed, appeared or went. A module in a glob cycle runs
  with its partners.
- **One machine per module** (`blaze_scope::Resolver`), stepped by the driver: `Ask(reads)`,
  `Expand(calls)`, `Cycle(modules)` (join a `Group`), `Done(scope, read list)`. The same machine
  serves "all modules of this crate" and "these modules again". It never computes another
  module's scope; its result is a function of the records alone, not of the order answers came.

The rules it holds:

- **Read per name.** A reader asks `(module, namespace, name)` or a module's `Exports` digest for
  a visibility class (what a glob reads). A private fn added to a module moves no reader that did
  not ask for that name, and no glob outside it.
- **A binding has states.** Target key, visibility, origin (declared, explicit import, glob,
  expanded), and `undetermined` or `ambiguous` as values, not errors. Explicit bindings are a
  layer below globs, so the fixpoint is well defined; an import waits until its answer is final;
  a macro path resolved early records the scope it hit and is checked against the final scope
  with rustc's ambiguity kinds, which makes any evaluation order agree with rustc.
- **Cycles are found at run time.** A glob read that reaches a module still being computed joins
  a strongly connected set, solved as a least fixpoint; each member is written as its own record
  naming its fellows.
- **What an edit redoes** (estimates, to count per edit class): a body edit, one `FileItems` and
  no scope; a private fn added, about three records; a `pub use x::*` at the root, the root's set
  and the modules that glob it; an edited `macro_rules!`, its N expansions and the scopes whose
  names changed.

**The gate for pull from cold.** Pull from cold (resolving only the modules a program reaches)
saves work only for a crate never seen and not fetched, and is the one part no Rust compiler has
built. It is built only if the measured first fill costs more than about 10 ms on reth's or
tempo's largest crate. The measurement is the whole fill: outline, expansion of every item macro,
and resolution, each reported; not item lists alone (outlining `core` alone took 132.5 ms). With
it, measure the size distribution of glob cycles on std, reth and tempo: if real cyclic sets are
large, per-module work collapses into a crate-wide task. Replaced: "The risk ... before the pull
producer is trusted", because the producer is per module and the risk now gates only pull from
cold.

## 7. Expansion

- **One pure function per call** (`blaze_expand::expand`): the definition and the call's tokens
  to an `Expansion`, or `NeedsExpanded` for an eager macro's nested calls. Every outside fact is
  asked through `ExpandCx` and recorded as a read (env, included files, cfg, positions inside
  eager input). `line!()`, `column!()`, `file!()` and `module_path!()` outside eager input are a
  placeholder and a `Hole`, filled from the outermost call when printed or lowered.
- **Hygiene** is by structure: a chain of marks over an edition root, in the record's own table,
  so `$crate` and def-site names resolve with no interned id.
- **Declarative macros** run on mbe's matcher and transcriber. The old layer is still underneath
  (mbe, tt, span, syntax-bridge, and through span `syntax` and `blaze-memo`): to cut.
- **Builtins** (derives and function-like macros) are written in blaze-expand, token to token.
- **Proc macros** compile to `wasm32-wasip1` and run in the process under a wasm runtime (rule 10;
  [proc-macros.md](proc-macros.md)). blaze-expand hands a `ProcCall` (kind, input, attribute
  arguments) to a runner it does not hold and takes back `Produced`: tokens, origins and the
  outside reads the sandbox recorded. The sandbox is the wasm module's WASI imports: no network,
  reads only through the runner, each one a read of the expansion's record. Decided (Andrew,
  2026-11-15 20:20): proc macros are the long tail, not a path to make fast. In order: (1) the
  derives that make up most calls in real workspaces (serde's, thiserror's, clap's, the std
  ones) are builtins in blaze-expand, used only when the macro crate's content digest is one
  the builtin was written against, so the result is the macro's own; (2) every other proc
  macro runs in the vendored Wasmtime (`vendor/wasmtime`, kept: no interpreter of our own is
  planned until a profile shows the runner is where the time goes); (3) an expansion is a
  record keyed by its input, so a call runs once and is shared; (4) a proc macro that
  compile-time reflection replaces is an advice finding naming the reflection form
  (rule 56), an error only in blaze's own crates. A workspace's dependencies still need
  (2): reth and tempo cannot be built without it (rule 4).
- **Build scripts** run sandboxed (rule 9) as a `ScriptRun` record ([target.md](target.md)):
  keyed by the script's program, its package, its declared environment and the settings, with its
  traced outside reads. blaze-build-script holds the environment cargo gives, the parse of what
  it prints and who receives it; constant-directive scripts are read, not run. Open: how the
  script's program runs with no `blaze build` (a `blaze run` of it inside the sandbox is the
  candidate), and the sandbox on macOS and Linux for a program placed by the launcher.

## 8. Signatures, impls, lang items, bodies

- **Signatures** are split records (table) so each is cut off on its own. Not built; the parked
  type checker answers `NotBuilt` for them on a crate without text (reconnect.md).
- **Impls.** "Which impls of T could apply to a type of shape S" (`blaze_impls::candidates`)
  reads, of each crate that can hold one, its `NoImplsOf`, and only where that does not rule the
  crate out, its directory and the one or two buckets it names: never a crate's whole table.
  Blanket impls are indexed under the trait. Every read, negative ones too, is handed to the
  caller's read list. **Lang items**: one record per crate, one binary search by name.
- **`BodyFacts`**: what a body gives others (hidden types, const values, discriminants), made by
  the body's task. The first stop of the reconnected checker on 2026-11-15 was a discriminant.
- **The type checker** (Andrew, 2026-11-15: "feel free to remake type checker from scratch in
  ideal one ... our best design from scratch will be better as fewer incremental issues").
  Kept: the type tables, the Rust project's next-generation trait solver, MIR building and the
  borrow checker. Written new: the per-body driver, `crates/blaze-typeck` ([typeck.md](typeck.md)):
  one body in, one record out, every outside fact read through a sealed recorder so a result
  cannot depend on what its read list does not hold, the walk an explicit work stack in rustc's
  order of inference (which is the specification). The old driver (hir-ty's `infer` and
  `method_resolution`) is the referee: both give the same types for every body of real `core`,
  `alloc` and `std` before the new one replaces it. Until then the parked hir-ty asks the new
  records through base-db's facade, unchanged at its call sites ([reconnect.md](reconnect.md)).

## 9. The driver

One loop per command, a sans-IO machine under a thin driver:

1. **Request.** The command's question: check these crates; run this entry; run these tests.
2. **Certificate.** One record for the request: its key is the tool and the request's shape (the
   config and arguments, the checkout's directory as a marker); its read list is what the last
   answer read (each file by content id under its stat stamp, the variables read, files outside
   the checkout); its answer is the keys of the findings and the program manifest. Checked in the
   command's own process, before anything else is loaded; when it stands, `check` prints the
   stored findings and exits, `run` places the manifest. Replaced: certificate.md's request
   certificate "checked by the daemon's loop", because no daemon exists (rule 70).
3. **Stat sweep.** The OS's journal by a stored event id where it keeps one (FSEvents), else a
   stat of every file against one small table of stamps; a file is hashed only when its stat
   changed or its stamp is inside the racy window (from Relax). Open: the window's length.
4. **Tasks, forward from what changed**: file items, declarations, scopes with expansion,
   signatures, the checker, then, for `run`, code. Each task is a pure function that returns its
   record or `Need(keys)`; it is entered with the needs its last record remembered.
5. **The shared flight.** The driver keeps one table of keys being made: a key is made once
   however many tasks need it; a task with needs is `Blocked{left}`, holds no worker, and is
   entered again from the top when it lacks nothing (run.md's Parent does exactly this for
   compiles). Every wait has a deadline (rule 18). The parked checker raises a `Need` by
   unwinding with a typed payload (reconnect.md); new code returns `Outcome::Need` (typeck.md).
6. **Scheduling.** Workers start in proportion to queued work; a no-op or one-body edit runs on
   the main thread ([target.md](target.md)). Each worker has a frame, reset per task.

Open (regret 2): a cold body with k unknowns found one after another runs k times. Test: serial
`Need` rounds per body on std with an empty store; if they cost, a stopped run saves its state.

## 10. Storage

- **One store** ([target.md](target.md)): immutable segments and a generation manifest, mapped
  once per process, read with no lock; each process writes its own segment; collection is
  compaction under a byte cap then unlink. A "pack" here is target.md's segment (local file and
  wire form); store.md's packs are the older store it replaces; blaze-format is each record's
  byte form; blaze-pack is the signed transfer pack (sorted manifest, per-record BLAKE3).
- **Reads**: small records by `pread` into the task's frame, large arrays mapped; verified once
  per fetch or boot. **Remote**: a pack's index once, then byte ranges in parallel; one format
  for R2 and a peer.
- **Restart checks.** Each pack's index holds, beside each key, its record's read-list digests,
  so checking a record after a restart touches only index pages. Open: the page bound. Test:
  count the pages a repeat check touches after a restart, on hello-world and reth.
- **Trust.** A key names inputs, not a value, so only a statement signed by a trusted builder (CI,
  later the remote workers) binds a key to a digest; the signature, digest and size limits are
  checked before a byte is mapped. A developer's machine never writes a shared namespace; private
  code, read lists and names never leave their organisation's namespace.
- **std's records** are built by CI, per toolchain, target, std features and profile settings,
  and published as a signed bundle. Offline, or with no bundle for the target, std is filled
  locally like any dependency, and the command says so by name (rule 3: said, not quiet).

## 11. Commands

Andrew, 2026-11-15 18:45: "no blaze build and just blaze export ... and blaze run / test / check
that are fast"; "everything in JIT world". The commands are `run`, `test`, `check`, and later
`export`. Detail: [run.md](run.md).

- **`blaze run` and `blaze test`** never write an executable. Code is records; the launcher places
  them in the program's own process behind one slot word a function. A function not yet compiled
  is a stub: its first call asks the run's parent for the code record (found by key, or made
  then), which is published in the slot; what is likely to run next is compiled behind it. No
  link step. A test run is a program run. **`blaze check`** makes no code.
- **`blaze export`** (later) writes one native executable (Mach-O, ELF): every function the
  program reaches, at the optimized tier, no stubs, laid out as the launcher lays them out,
  written once, whole. Not incremental; keeps nothing. Until built, the command says it is not.
- **Gone:** `blaze build`; the linker's patching of a file in place (kept slots in a file, spare
  room, growth bounds, patched unwind tables, a signature made again per edit); compiling
  everything reachable before the first instruction runs.
- **Tiers.** A function's first code is the fast tier, which calls through slots. A background
  tier optimizes hot functions across functions (inlining); its record lists, in its read list,
  the code identity of every callee body it inlined, so an edit of a callee makes the caller's
  optimized record stale and the caller runs its fast code until the tier makes it again. A
  better version is one slot word written.
- **Hot counters.** Fast code's prologue adds one to its slot's counter; the control thread
  samples the counters and, past a bound, queues the function for the optimizing tier, once per
  run. No profiler is needed. Replaced: "a function's stub counts its calls", because a stub runs
  once. Open (run.md 1): the prologue's add against sampling the program counter.
- **Decide at run time** (Andrew: "this is a JIT! ideally these things could be toggled
  dynamically at runtime"). A slot can hold another version of the same function at any moment:
  tracing a function or module, recording effects, exact counts and times, a breakpoint that
  parks the thread with its stack, coverage, allocation tracking, extra checks (overflow, bounds,
  contracts) on one function. The plain version tests no flag. A version is a code record keyed
  by the function and its set of hooks, made once and shared. A build-time switch is the
  exception and says why. What stays at compile time is what decides whether the program is
  right: types, borrows, capabilities. A toggle is a request to a live run from a paired viewer.
- **Optimizing from what was observed** (a `dyn` call's real target under a guard, real counts
  for layout, argument values a function keeps getting): the observed facts go into the
  version's key, the version runs behind a guard that leaves for the plain version when the fact
  does not hold, and such records stay local: never published to a shared namespace. Replaced:
  "the optimizing tier works from what was observed" with no word on keys, because a record that
  depends on facts outside its key breaks rule 25 and run.md's optimized key. Open (regret 3):
  measure its speedup over the plain optimized tier on reth before building it.
- **Tests that pick themselves**: section 1 ([test-selection.md](test-selection.md)).
  **Discovery**: a test is an item whose attributes hold `#[test]` once cfg and expansion are
  done (an attribute macro can make one); each module's tests are read from its `ModuleDecls`
  and expansions, not by walking the crate. The harness entry is part of the test run's program
  manifest, made by blaze, calling each selected test (run.md, `Tests`). Open: libtest's command
  line and output, `harness = false`, and doctests ([doctests.md](doctests.md)).
- **Debug info for a run.** Each code record carries its unwind table and symbol; the launcher
  registers placed code as one more image so the program's own backtrace finds names
  (blaze-launch-rt's `unwind` and `symbols`). Open: registration with a debugger (lldb, gdb) for
  code placed while the program runs, and `export`'s side file ([debug-side-file.md](debug-side-file.md)).

## 12. Diagnostics and lints

- **A finding is a record** (rule 81), keyed like its check, with no level and an item-relative
  range. A user's error is a value in the record, never a failure of the task (typeck.md).
- **Printing.** A range is placed through today's `FileItems` (the item's start) and the file's
  line table; `file:line:col` exists only on the terminal. Open: whether the line table is a
  column of `FileItems` (keyed by the same bytes, so printing after a restart needs no read of
  the source) or made from the text when printed.
- **The early record** (from Relax): a check whose certificate stands prints its stored findings
  and exits before anything is loaded. **Lints are rebuilt** as per-item tasks on the new layer. An edit re-runs the lints of the items
  whose facts changed and no others; a lint over a whole crate or program reads per-item summary
  records. The old lints were deleted on 2026-11-15. The short list first: `Send` across a wait
  (section 13; the first rebuilt), string errors, implied allocators, dropped errors, ambient
  reads, unnamed or unbounded waits, map-order output, a key ignoring an input, `todo!`.
- **Relax's failures as a checklist.** Its `KNOWN_GAPS.md` and tests (MIT) are the list of Rust
  that trips a front end written from scratch; each becomes a test here.

## 13. Code rules

**Zero third-party crates** (Andrew, 2026-11-15: "we should have 0 third party and all the things
you said worth adding"). The end state: every crate the `blaze` binary links depends only on
crates of this workspace. A gate ratchet (scripts/src/bin/thirdparty.rs, its list
tools/ratchets/third-party.txt) holds the count per crate and lets it only fall; a new crate may
depend only on workspace crates already at zero. Measured on main after the delete, 2026-11-15:
413 third-party packages in the lock graph, 102 used directly (serde by 25 crates, serde_json 16,
tracing 14, libc 12, crossbeam-channel 10); `blaze-id` pulled 58 through `blaze-ids` and
`blaze-hash` (blake3, hashbrown, crossbeam, futures). Since then blaze-blake3 and blaze-swiss
replace blake3 and the probe; hashbrown is the one edge left under blaze-id ([third-party.md](../third-party.md)).

The exceptions, each dated 2026-11-15:

| Exception | What it is | How it ends |
|---|---|---|
| The trait solver (`vendor/rustc_next_trait_solver`, `rustc_type_ir` and their small dependencies: arrayvec, bitflags, derive-where, ena, indexmap, ra-ap-rustc_index, rustc-hash, smallvec, thin-vec, tracing) | a fork of the Rust project's solver | becomes ours by stripping its dependencies and marking it `ours = true`; it is NOT rewritten from a blank page |
| Wasmtime (`vendor/wasmtime`) | the proc-macro runner (rule 10) | stays vendored (Andrew, 2026-11-15 20:20: vendoring is fine for the hard ones); replaced only if a profile names it |
| zstd (`vendor/zstd-sys`, C) | compression of stored records | stays vendored; first measure whether records should be compressed at all |
| LLVM | the optimizing backend | the Rust bindings go (declared `extern "C"` from LLVM's headers); LLVM stays a C++ library until blaze's own backend is the top tier |
| What third-party.md marks hard | TLS and signatures, zstd, the profile's file format, tree-sitter, Cargo's formats | the option third-party.md names for each |

**No `std` and no `alloc` in blaze's own code** (Andrew: "can we not even depend on the std library
for anything here? our own std library ... blaze-std"; "no alloc! we should have our own").
`core` stays: it is the language. `std` is replaced by bstd (`crates/blaze-std`), split so rustc
builds its base and blaze adds what needs its extensions under `cfg(blaze)`
([std.md](std.md#two-builders)). `alloc` is replaced by collections of our own (crates/blaze-own:
`Vec`, `Box`, `Arc`, `Rc`, `VecDeque`, sorted maps, `Cow`, the column vector `Soa`) with no
default allocator: `new_in(alloc)` is the only constructor, so rule 11 is held by the types.
bstd's entry installs the global allocator. Nightly features are allowed for it:
`coerce_unsized`, `dispatch_from_dyn`, `arbitrary_self_types`, `deref_patterns`,
`dropck_eyepatch`, `min_specialization`. Known cost: the compiler's own `Box` moves out with `*b`;
ours uses a method.

The interim, replaced crate by crate: under D25 (2026-11-11), while cargo builds blaze a new crate
may sit on `core` and `alloc` and says so in its header (blaze-id, blaze-scope, blaze-impls and
blaze-run name `alloc::vec::Vec` today; they move to blaze-own); blaze-surface is on `std` because
blaze-format's `record!` names `::std::vec::Vec`, blaze-file-items because the parser is on `std`,
and each turns `no_std` when its base does. Parked crates are moved by astlog rules once blaze
checks itself. Replaced: "a new crate is `#![no_std]` with no `extern crate alloc`", because D25
and the code hold this interim.

**Layout: columns** (Andrew: "use soa as much as possible ... and other things that will
optimize"). A record, and an in-memory table of more than a few rows, is a struct of arrays: one
packed column per field, one length, one buffer. Rows of structs only where every access reads
the whole row. With it: fields read on every lookup apart from fields read rarely; a `u32` index
into a column, never a pointer; a sorted column with binary search or a perfect hash, never a
linked map; a bitset for a yes-or-no fact; variable data in one side table by offset; no
allocation per item; LEB128 for positions. The in-memory container is `blaze_own::Soa`, declared
by `soa!`. The type checker's own trees are not reshaped by this rule; that is done where a
profile says it pays.

**Cleanup on every exit is `core::mem::DropGuard`** (stable in the pinned library; Andrew: "like
zig's drop thing"): `new(value, |v| ..)` runs the closure on every exit path, `dismiss(guard)`
gives the value back and runs nothing. It replaces a hand-written `Drop` struct, for cleanup that
cannot fail or wait; what must finish with a result or a deadline is a linear type
(`#[blaze::linear]`) with `finish(cx)`. `async_drop` is not used.

**Fibers and `Send`** (Andrew: "wont we not have proper protection against using things like ...
Rc"). Fibers are bstd's convenience for sequential code of an ordinary program (no `async`, one
kind of function), until blaze generates the state machine from straight-line code; the
compiler's own tasks do not use them (section 2). Rust checks `Send` across an `.await`, not
across a fiber's wait, so a fiber holding an `Rc`, a lock guard or a thread-local borrow that
wakes on another thread is a data race rustc accepts. So: built by rustc, a fiber of bstd never
changes its thread after it starts (new tasks are spread, a started one stays), which is sound
unchecked; built by blaze, "a value that is not `Send` is live across a call that can wait" is a
compile error (rule 27; std.md's "What the compiler checks"), and a fiber may move. That check is
the first lint rebuilt. bstd has no thread-local storage; its one-thread types check in debug
builds that they end on the thread that made them.

## 14. Observability

- **Every run answers**: it registers (run.md's `Registry`) and answers on a local socket; the
  loader is ours, so a run can pause on a panic with its stack, say why it hangs, sample itself.
- **The viewer** (Andrew: "doing separate blaze view seems annoying ... ix.dev thing show
  automatically whatever is running or building"). No command of its own: any blaze command
  starts it when nothing listens on its fixed local port. It lists live runs, serves their events
  and the profile history, never builds, and holds nothing a record does not. It exits a fixed
  time after the last registered run ended; a connected page does not keep it alive (rules 70 and
  18). Replaced: "it ends when no blaze process is alive and no page is connected", because an
  open tab would keep it alive with no expiry. Open: the length of the expiry.
- **The page and privacy.** The page is hosted on ix.dev. The viewer answers only that origin,
  read-only, after a browser was paired once (the page shows a code, the next blaze command asks).
  Data goes to the browser and no further. Because the browser runs the page's script against
  local data, the script must be fixed by the blaze release: pinned by digest, or served by the
  viewer itself. Open: which of the two.
  Toggles (section 11) are a permitted action of a paired browser only, sent to the run's parent.
- **Many machines** (Andrew: "a monitor of what might be happening even on multiple machines").
  The events (a command began or ended, what it runs, compiles or tests, on which machine and
  checkout, time and memory) are one stream. When an organisation opts in, the viewer also sends
  it to a relay on ix.dev tied to an ix account, and the page shows every machine; machines make
  outgoing connections only. Off by default; activity only, not code or item names, unless chosen.
  Makepad's devhub (warm machines per agent, a warm cache at the tip) is the comparison.
- **Every effect is an event** (Andrew: "it can log the system calls ... and where they came from
  ... in the panel and in data"). In bstd a program reaches the system only through `Io`; each
  call is one `Effect` given to the one driver under a `Cx` that knows its task and span. Effect
  logging is a hooked version of the `Io` entry functions (section 11), so with the hook off the
  entry runs its plain code. Logged per effect: its kind and argument sizes, when it was asked,
  started and done, its outcome, the task and span, and the call site. Argument values (paths,
  hosts, bytes) are kept only when asked and never leave the machine without the opt-in. The
  simulation implements the same driver, so a test can state what a function may do ("no
  network", "writes one file"). A program on Rust's own `std` does not pass through it; since
  blaze compiles `std` itself, its system layer can carry the same hook: later. Replaced: "always
  on" and "off costs nothing" inside the driver, because recording in the driver costs a test on
  every effect. Open: the call site by `#[track_caller]` passes a hidden argument; its cost is
  untested (a microbenchmark), and the hooked version may take the site from its return address
  instead.

## 15. Relax

Relax (makepad, MIT; read at commit f05459f on 2026-11-15, source only) is the compiler this one
is judged against. What its code does:

- **Names**: eager collection, an import fixpoint over the whole program's import list
  (`front/src/program.rs`), lookups memoised per (module, name, namespace), and every binding
  change clears the memos of the whole program: it resolves whole, as rustc does. It keeps a full
  AST of every file for the process's life; positions are absolute byte offsets, so a line
  inserted above a function changes that function's code key.
- **Cache**: one mmap'd file per crate version (a header, a sorted index, append-only payloads of
  raw in-memory structs, readable only by the same build), keyed by absolute worktree paths: not
  shareable; std is compiled per workspace. `check`'s verdict is per crate, keyed by every file's
  text; a signature edit drops every body verdict of the crate; a body with a warning is never
  stored.
- **Its speed** comes from elsewhere: compile on first call (stubs), a warm start that maps the
  resolved program in place after a stat-only certificate, stored diagnostics printed with no
  load, and no link step.

Taken: compile on first call (section 11; the largest part of its lead on time to first run);
the early record (12); stat stamps with a racy window (9); one mapped pack per crate with a sorted
index (10); packed tokens and LEB128 position tables, bodies skipped at load (5); a key that
covers what a name can find (its per-item name graph with a Merkle hash per strongly connected
set is the per-name read list; blanket impls indexed under the trait); its known gaps as tests.
Not taken: absolute positions and paths in keys, crate-grain verdicts, raw in-memory structs as
the stored form, whole-program memo flushes, ASTs kept alive, dependencies read from source.

### More taken from Relax (second survey, 2026-11-15 20:00; file names are Relax's)

Adopt, by value to the goal order:

| What | Why | Size | Where in Relax |
|---|---|---|---|
| Relocated fields emitted as zeros, filled at install | code bytes do not depend on their address, so a code record is shareable and cacheable | small | backend rir.rs:542-760 |
| Slot table: thunk, lazy stub, table word, tier record per id; a thunk head patched to a direct jump when in reach | first-call slots and hooks | medium | rir.rs:588; jit.rs:19596 |
| Store file: two index slots with a header flip, append-only payloads, a new generation per rewrite, never renamed over | crash safety and restart | medium | compiler store.rs:1-38 |
| Startup image: what ran before the first frame last time, relocations against identity tables, linked in one pass | warm restart | medium | cache/image.rs |
| A backend memo keyed up to renaming of slots: generic instances that differ only in callees share one codegen result | cold time, memory | medium | bmemo.rs |
| Two views of the same pages for W^X (memfd on Linux, `mach_vm_remap` on macOS), not `MAP_JIT`: a program's own `pthread_jit_write_protect_np` flips every `MAP_JIT` page of its thread | correctness | small | os/mem.rs:301, 672 |
| An entry-mark thunk in the table word, a byte in a per-thread bitmap: which functions a test entered, at no cost outside live tests | incremental tests | small | harness_reach.rs |
| Invalidated code in page-aligned groups set `PROT_NONE`, addresses never reused; a stale access parks its thread | safety of retirement | medium | invalid.rs |
| Hooks (count, break, heap) as a patch group whose removal rolls back with no recompile | decide at run time | medium | instrument.rs; heap.rs |
| Channels with credit flow control, a replay ring, a hub that reopens feeds by sequence number, an ssh relay | the monitor and the relay | medium | libs/remote |
| A flight recorder of lock-free per-thread rings; a stack sampled once a wait passes a bound | the monitor | medium | flightrec.rs |
| A fixed-table wait registry written only on blocking paths: why a program hangs | tooling | small | base waits.rs; hang.rs |
| An unwind trailer per function instead of DWARF | cold time | medium | rir.rs:2340 |
| A one-pass baseline register allocator and assembler for the fast tier | cold time | medium | regalloc.rs:3085 |
| A process-wide CPU budget so a person's work never waits behind background builds | the edit loop on a Mac | small | libs/cpu_budget |
| Random IR programs run through every tier and compared with an evaluator | correctness of tiers | medium | codegen tests/fuzz_tiers.rs |

Also ready to adapt for "no third-party crates" (MIT, with sizes): TOML (2.8k lines), a small
serializer (4.5k), strict JSON (0.8k), base64, inflate and deflate (5k, a miniz port), sha256,
tar, an HTTP client over the OS's TLS (1k), git over smart HTTP (2.3k), an ELF/Mach-O/DWARF
reader (1.9k), disassemblers for x64 and arm64 (5.7k).

Not taken: a non-SSA IR for the top tier (their own list of why their code is slower starts
with register copies); one owner thread that every change goes through (their waits of
374 to 583 ms; against the 128-core rule); code that only grows (memory); running real cargo
for C dependencies; their sandbox by address masking.

## 16. Ladder, referee, switch

- **The ladder.** First rung: `blaze run` of `fn main() {}` through the launcher, on the new
  layer, with the parked type checker reconnected. Then `println!`; then ripgrep; then reth and
  tempo, their own tests passing (rule 7). Replaced: "to a running binary through the existing
  codegen and linker", because no executable is written and there is no link step.
- **Stops are the work list**: a question not answerable yet is `NotBuilt { method, item }`.
- **Referees.** For the item layer: the tree before the delete (#2197), kept as a reference
  checkout: for every item of real `core`, `alloc` and `std`, each `Items` question answers equal.
  For bodies: the old driver against blaze-typeck (section 8). Replaced: "the old layer is the
  referee until the switch" and "until the switch the old layer is frozen", because the old item
  layer was deleted on 2026-11-15 rather than kept beside the new one.
- **Deleted** on 2026-11-15 (#2197, then a second pass): the daemon, blaze-query, blaze-lints,
  hir-def's name resolution and persistence, hir-expand's database layer, blaze-engine; 51 crates
  parked ([parked.md](../parked.md)). **Still to delete**: hir-ty's old persistence forms (about
  33k lines), the daemon-side run crates (run.md), every portable form but the item key. The type
  checker, MIR, borrowck and codegen are not deleted without Andrew's explicit word.
- **How the work runs** (Andrew, 2026-11-15 19:11): one directory and one pull request for the
  rebuild; lanes write in it in parallel, never wait on a compile or a yes, and the main agent
  compiles the tree in batches (rules 35, 48). Until `blaze run` exists again, a merge's proof is
  the whole build and its tests on the exact tree, quoted (rule 68).

## 17. Measurements, floors, open experiments

**Measured.** `blaze_file_items::outline` (a file's text to its items, hashes and ranges; one
thread, an M-series Mac, release, 2026-11-15, bodies parsed in full): `core` 299 files, 180,413
lines, 13,443 items in 132.5 ms; `alloc` 64.2 ms; `std` 170.9 ms; all of `library/` (1.53 M lines,
207,552 items) 1,671 ms; blaze's own crates (2.63 M lines, 321,674 items) 3,332 ms: about 1
microsecond a line, 32 to 51 MB/s. The old path, 2026-11-15: cold hello-world check 21 to 45 s
and 3.4 to 4.6 GiB; repeat with nothing changed 2.2 to 3.3 s.

**Floors** (rule 67). The floor is the work this design says the case is; each floor time below
is an estimate from that work, not a measurement. Each case's target is its measured floor plus
10%; the milestone is the old text's number, kept as a first checkpoint, and misses rule 67 by
the ratio shown.

| Case | Floor: the work | Floor estimate | Milestone | Milestone / floor | Measure by |
|---|---|---|---|---|---|
| Repeat check, nothing changed, warm | exec, one certificate read, the stat sweep | about 2 ms for hello-world (exec 1 ms, a few `pread`s, a few stats) | 20 ms | about 10 | profile hello-world and reth on the ladder |
| Repeat after a restart | the above plus the index pages and certificate from disk | open: needs the page count | 20 ms | open | count pages touched |
| Cold check of hello-world, std's records local | the items reached (a few hundred), each one record read and one scope probe; main's body checked | about 5 to 10 ms (a few thousand reads of 1 to 5 us); memory: the mapped records reached plus the process, tens of MiB | 1 s, 200 MiB | about 100 to 200 | profile hello-world |
| Body edit | stat sweep, re-outline of one file (1 us a line), one body task, one finding record | about 2 ms for a 1k-line file; 30 ms for 30k lines unless re-outline is per item | 100 ms | about 50 (small file) | profile a body edit on reth's largest files |
| `blaze run` to main's first instruction | certificate, one spawn, socket, mapping, entry and pre-main records placed (run.md) | about 3 ms past the certificate | 25 ms in all | about 5 | run.md's floors |
| One stub miss, record present | resolver, queue, wake, one `pread`, fixups, store, wake (run.md) | about 10 us | — | — | run.md |

**Unmeasured, and what settles each:**

| Claim | Measurement |
|---|---|
| The outline divides by N cores (a file is independent) | outline `library/` at 1, 8 and 16 threads |
| With bodies skipped the outline falls further | time the outline with bodies skipped |
| Relax builds in about a minute with no third-party crate | time it |
| `#[track_caller]` costs nothing at run time | a microbenchmark |
| The 10 ms gate for pull from cold | the whole first fill on reth's and tempo's largest crates (section 6) |
| A private fn added rewrites about three records | count records rewritten per edit class |
| Glob cycles are small | the size distribution on std, reth, tempo |
| A repeat touches a bounded number of pages | count pages touched by a repeat check after a restart |
| Re-entry is cheap | serial `Need` rounds per body on std with an empty store |
| Observed-value optimization pays | its speedup over the plain optimized tier on reth |
| A prologue counter and a test-lane store are worth their cost | reth's suite against program-counter sampling (run.md 1 and 3) |
| Zero third-party is affordable before hello-world | add up third-party.md's replacement lines; schedule after the first rung |

## 18. Superseded and disagreeing docs

- Superseded: [certificate.md](certificate.md) by section 9 (checked by the daemon's loop) and
  [store-pool.md](store-pool.md) by section 10 (its own pool; a task yields on a miss).
- Disagreeing with this page, to fix in their own changes: AGENTS.md rule 60 (no remote in its
  order) and rule 12 (names papaya and crossbeam); target.md's key rules (the target in every
  key); `blaze_id::CrateItemKey`'s doc (cfg in the crate key); blaze-surface and blaze-impls both
  define `LangItems`, `CrateGraph` and `NoImplsOf`, and surface, scope, impls and expand each
  define `Edition` (rule 30); blaze-hash's hashbrown against tools/ratchets/third-party.txt's
  `(all) 0 0 0`.
- Old section names that code cites: "Also decided on 2026-11-15" is now sections 13 and 14; "The
  commands" and "Decide at run time", 11; "How a task waits", 2; "Layout: columns" and "Known
  cost", 13; "A binding has states", 6.
