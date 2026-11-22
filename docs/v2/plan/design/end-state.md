---
status: plan
summary: "The end state the audits of 2026-11-20 agree on, as three switches to complete and the layers each deletes: regions inside types with one call rule (D156), one driver whose tasks are kinds (D157), names at module grain under one crate key with cfg read (D158). The order, the oracles and the rule that a piece is not done while the old path exists."
date: 2026-11-20
---
# The end state: the switches to complete (D156 to D159)

Four audits on 2026-11-20 (the notes, `rounds/wave-1122-arch/`: A1 borrow check, A2 the
command, A3 names, macros and units; `rounds/wave-1121-sm/`: states) found one pattern in three
areas: the intended design is written down, and sometimes built, but the live path is the older
shape with compensation layered on it. Four finer-grain names designs sit unswitched (about
14,000 lines); the engine's generic driver is called by its simulation alone; a lifetime column
was laid beside erased types (D148) and three more layers then rebuilt what the erasure lost.
So nothing here is a new design. Each part is a switch to complete and a list of what it deletes.

## The rules of this work

1. **A piece is not done while the old path exists.** Each piece is: the contract (a small crate
   of types and state machines that compiles and is simulated, rule 79), the switch of the live
   path onto it, and the deletion of what it replaces, as one unit of work. "Built to the
   harness stage" is not a state a piece may rest in.
2. **An equality oracle gates each deletion** (rule 23): the new path's answers against the old
   one's on tempo, reth and blaze, the difference zero or each difference a named improvement.
3. **No new side record, index, memo or cache.** Where work repeats, the repetition is removed.
   A derived fact that must be kept is a task of the engine, with the engine's key and read list.
4. **States are types** (states.md): each contract uses the eight forms; a planted control lives
   in a test-only constructor.
5. Dead code goes as it is found. Done 2026-11-20: the twenty parked crates (445,814 lines).

## D156. Regions are inside types; one call rule

Types carry their lifetime arguments (erased to one region in keys and in the trait solver, as
rustc does). The head lowering's one walk makes them; elision, object defaults and late-bound
marking happen there, and E0106, E0261, E0107 and E0637 are the head's reported errors. A
declared count comes from the items record (D155) where the walk fills an elided argument. The
borrow check works over one grain: a place's origins are its type's region slots, one for one.
A call is one operation: instantiate the callee's signature (early-bound from the argument
slots, late-bound fresh), normalize aliases through the impl the checker selected, relate
arguments, result and destination by variance, and turn the callee's clauses into outlives
constraints. Operators, constructors, closures, trait methods, `dyn` and fn pointers go through
it. What stays refused, by name: a type error, a malformed record, `asm!`, an unselected impl
where normalization is needed, higher-ranked subtyping until placeholders exist.

- Contract: `crates/blaze-borrow` (slots of a type, `instantiate`, `relate`, the constraints it
  emits to blaze-flow), written against a `RegionTypes` trait so it runs first over D148's
  columns and then over in-node regions unchanged.
- Oracle: rustc's `tests/ui/nll`, `regions`, `borrowck` and `lifetimes` as fixtures (accept and
  reject as rustc does); no body accepted that today's path refuses without such a test; tempo's
  refused count under 800 at the switch, then down.
- Deletes: blaze-flow-lower's nine call paths (`hidden_call.rs`, `impl_call.rs`,
  `generic_call.rs`, `trait_ties.rs`, `param_outlives.rs`, the elision path, `unify_call`), its
  position model (`npos`, `lay_positions`, `LaidSig`), front2 `lifetimes.rs`, most of
  `heads_regions.rs`, `borrows_ties.rs`, `RegionSink`, `soft!`, the `Arity` lookups,
  `CountsNoting`, `unmade_counts`, `impl_scope_from_text`, about 45 of the 54 refusal forms.

## D157. One driver; a stage is a kind of task

A task reads cells and defines cells. `trait TaskKind { fn plan(..); fn run(..) -> Done | Need(cells) | Failed }`.
One ready-set driver in blaze-engine: a task is entered when the producers of the cells it read
have merged; `Need` enters the producers and parks the task; merges keep one deterministic order;
one worker pool. A worker puts its own record; the merge files the key; answer pages are written
as tasks merge. The answer is asked by item key and by kind, so `run` and `test` read it and
build no table. Every read goes through one recording host.

- Contract: the driver and `TaskKind` in blaze-engine, with its simulation: the ready-set
  schedule's answers equal the phased schedule's on every history; controls: a task entered
  before a producer merged, a `Need` whose producer is dropped.
- Then each stage becomes a kind, in this order: Outline, Script, Lint (small), Body, Head,
  CrateNames (until D158 replaces it), UnitGraph, Manifest.
- Deletes: the seven-way dispatch in `checking`, the Names wave path, `NamesQueue`,
  `ScriptQueue`, `Pipeline`, `PoolSlots`, the per-stage scopes, `HeadsOnce` and the seeds'
  freeze-on-the-spot, member rows, listing records, the plan index, the journal checkpoint, the
  body-id table, `kept_beside`, the lines record as a second form, the page form of the answer,
  `work::Record`. The audit's estimate: 40 to 50% of blaze-cli's 30,170 engine lines.
- Utilization (D153) follows from it: U1, U3, U5, U8 are this design's properties.

## D158. Names at module grain; one crate key; cfg is read

A crate key is the package's identity: no cfg, test or side in it (D152). A module's links
(outline, uses, globs, item-position calls) are a record keyed by its text. A lookup decides
cells (module, namespace, name) on demand; every hop is a logged read; a glob or import cycle
is a group solved to its least fixpoint. An expansion is found by its call key before the work
and stored once, proc macro or `macro_rules!` alike. Each `#[cfg]` asked is a read of a
variable; a test build is a view whose rows are lifted from the base (D152). A build script's
outputs are records whose entries are read as cells. One key derivation, one cfg evaluator.

- Contract: the CELL CORE is completed (chosen 2026-11-20 by a reading of all four, the notes'
  `rounds/wave-1122-arch/NAMES-CHOICE.md`): its `decide` (blaze-scope cells/decide.rs) over
  D116's module tables (links.rs, blaze-surface module_links.rs), because the rules tempo, reth
  and blaze pass today live there and `decide` runs those same rules; its `Decided::Wait` and
  `Answered::Pending` are already `Need`s and its `HopRow` the read list. Deleted: blaze-names-demand
  and blaze-names-source (after their worlds and three plants are ported into cells' oracle, and
  one run on tempo counts where they disagree), the old per-module core (its
  `MacroRulesVsModularized` ambiguity carried over with a test), the group-and-edit drivers and
  `EditError::SolveWhole` (a fallback).
- Steps: (1) the equality check D116 already has (`decide_check`) is turned on in the command and
  run on tempo, reth and blaze: zero disagreements, or each named and fixed; (2) the lookup
  becomes a task: `Decided` answers `Need(Outline | Expansion | Root)`, the made tables are
  written per module, modules loaded per ask stay near the measured reach (2.9% on the shelf);
  (3) the readers move off `CrateScope`, which stays only as step 1's oracle, then goes.
  If step 2 shows `decide` cannot answer without loading the crate's whole module tree, the
  choice reopens.
- Oracle: D116's equality check on tempo, reth and blaze (the new path's resolutions equal
  `CrateScope`'s), then the floor test: a private fn added re-enters (1, 1).
- Deletes: `CrateScope` and its sections, `names_cells.rs`, the old per-module core, the three
  unchosen designs, the items record's secondary indexes, `expansion_records.rs`' dedupe after
  the fact, the proc-macro results table, `Holders`, `shared_keys.rs`, the extra key
  derivations, five of six cfg evaluators, `Condition`, `join_scripts`. About 30,000 lines.

## D159. One body, one store, one answer form; code made ahead

The fourth audit (A4) found the same pattern below the engine: four migrations started and not
finished.

- **One body.** A checked body is lowered three times today (the builder's walk, constant
  evaluation, the flow lowering), each with its own refusals: 115 "not built to LIR yet" sites
  in blaze-build, three pattern walkers, move errors refused in two places. D119's one lowering
  is written and unwired (blaze-body, -lower, -build, -eval, -drops, -flow; no crate depends on
  the last four; `keep_checked` has no caller). Finish it: the Body kind writes the kept body
  record; the borrow check, constant evaluation and the builder read that one record. Deletes
  the blaze-build walk (about 17,000 lines), blaze-consteval's own compiler, the per-form
  refusals. Oracle: both builders side by side on every ladder rung and tempo body.
- **Code made ahead.** A need is answered by one synchronous make on the parent's thread; the
  sans-IO run parent (blaze-run `Parent`, startup image, about 3,800 lines) is unused. One run
  parent drives the worker pool: a need makes its instance and, bounded and in parallel, what
  its record declares it calls. Deletes one of the two parents, `build_from` and its nine entry
  points, the start list or the startup image (one stays).
- **One store handle.** The old pack store is still in blaze-cas (about 49,700 lines), used by
  nothing live: deleted. Of the plain store's five faces one `Store` stays: `Sync`, lock-free
  reads over proven mappings, a writer per worker appending its own segment (U3; no arena, no
  second copy), commit by the flip.
- **Compaction with an invariant.** The planner discards any merge whose second segment passes
  64 MiB (plain/core.rs `plan`), so once a namespace holds a large segment nothing merges
  again: the cause of "compaction is behind". A size-tiered merge folded into the commit, with
  one stated bound (segments at most k times log of bytes) the simulation checks after every
  commit; reachability (`Reached`) turned on. Deletes the warning, two of three caps, blaze-gc.
- **One stored answer.** Four forms exist; the rows form (blaze-answer) "replaces" the page
  form and is not switched on. With D157's "written as tasks merge" it is the only stored form.
- **Leaks.** 122 leaks in blaze-cli to get `'static`; with results as records a phase arena's
  `keep(T) -> &'phase T` replaces them, and a lint bans `leak` outside blaze-mem.
- Dead and going: blaze-vfs, blaze-gc, blaze-storemodel, blaze-llvm, blaze-native-load, the
  pack store, about 250,000 lines off the live path in all.

## The second audit (2026-11-20): six classes, six mechanisms

The owner, 2026-11-20 16:43, on the audit page (reports under
`rounds/wave-1123-audit2/` in the notes: B1 forms and keys, B2 checker, flow, builder and run, B3
output, tests and proof, B4 homes, data files, crates and documents): "ok rewrite to be proper long
term design that u recommend here". D160 to D166 are that design. Each names what it deletes; rules
1 to 5 above hold for each.

## D160. A store's format is part of its place

The one store is `<home>/.blaze/store/f<FORMAT>s<SEGMENT_FORMAT>/`, named by one function in
blaze-cas from every constant a blaze must agree on to read the folder. A blaze of another format
opens its own folder, says once what it found beside it, and goes on; nothing is migrated and no
command stops because another blaze wrote a store. Other-format folders are the sweep's after an
expiry, and `blaze clean`'s. A store of the same format that does not read is a typed refusal. A
manifest row is one validated type whose only constructor is the check, used by commit and by
open, so a manifest its reader refuses cannot be committed.

## D161. A task that lacks a fact waits on it; nothing starts over

One mechanism at two levels. In the engine (D157) a task returns `Need(name)`, parks and resumes
when the name merges; same-stage waves, `WAVE_CAP` and the batch bookkeeping go. In the checker,
typeck.md's gate is built: a body is entered once, when the item records its text names have
settled; a fact found only while inferring (a method on an inferred type, a constant in a type, an
opaque's hidden type) is claimed in the claim table and made inline or parked on, with the body's
work kept. A cycle is a typed error naming the items.
- Deletes: the nine restart loops and their caps (blaze-typeck `infer/mod.rs` refill loop, front2
  `check.rs` re-check rounds, `world_items.rs` `again`, the head step restart in `heads.rs`, the
  constant and builder loops), the re-entry of head making from inside the solver on the caller's
  stack, and in the engine everything D157's review listed.
- Oracle: the same findings and the same record bytes as the restart path on every fixture and on
  tempo; the solver's "restarted goals" counter goes from 74% of calls to 0.

## D162. A key is subject, producer and schema; a place is named once

A record's key is `Key::of(schema, producer, inputs)` (blaze-cas, written and unused today), where
`schema` is the digest blaze-format-core derives from the record's type by reflection and
`producer` is the binary's id now and the producing function's code identity later. An old form is
a miss; nothing refuses a form. Hand-written numbers stay only for formats blaze does not own
(Mach-O, ELF, wasm, ar, the wire CBOR). The store's put and get take the typed key, so a record
cannot be stored without its schema; a lint bans a numbered hash domain and a `FORM`/`VERSION`
constant in a record module. One `Home` value, built once in `main`, names every place under the
home and the checkout; a lint bans the `".blaze"` literal elsewhere. One `pins/` folder at the
repository root holds every fact of the pinned toolchain; blaze-toolchain turns it into constants
at compile time and scripts read the same files; a lint bans `include_*` across crates.
- Deletes: about 85 form numbers and 46 numbered domains inside binary-keyed records, the payload
  magics that repeat the key, the per-read form checks, the profile history's migration, 18 path
  constants, two of three home-root rules, the copied toolchain constants.
- Order: places first (D160), then `Home` and `pins/`, then the typed key on the store with the
  numbers deleted in one batch, and only then finer producer ids.

## D163. A proof is a type in a signature

`blaze_lir::verify` returns `Verified<View>` and `emit` takes only that. The borrow check's
`Checked` is what the run gate and the builder take. `blaze run` builds from the check's own body
record (D159) and never checks a body again: the second front end (`LazyProgram`'s check in
`run/make_one.rs`, `program/lazy/fill.rs`) goes. A stored row that was validated carries a proof
its readers take, so nothing is validated on each read (rule 74).

## D164. A finding is a record; every view is a query

One finding record: a rule, a subject (a file, an item, a crate: never a task's debug form), an
item-relative range, typed arguments, and for "not checked" and "refused" a typed cause with a
cause key. Notes are findings of a note kind, worded by blaze-report's one table like errors. The
default view, `--each`, `why` and the machine form are queries over the records (grouping is
"group by cause key"), severity is a column of the rule table, and the exit status is computed
from the records. Tests of the command assert on records through the in-memory `World`
(`World::check` returns findings); the renderer has its own few snapshot tests.
- Deletes: `Said`'s `&'static str` causes and `note()`'s sentences, `Class::Under` and the five
  grouping printers, the severity `if` chain in print.rs, the status-from-tally code, and, as tests
  are moved, the text matches (about 1,288 `.contains("` in test files).

## D165. A limit is a value a simulation draws; a control is typed

Every cap of a store, an engine or a table is a field of that component's `Limits`, with the
shipped values as one constant instance and no `cfg(test)` variant; a simulation draws each down
to its smallest legal value and has a reach point at each limit. A blaze-sim control names the
invariant it must trip as a value of the oracle's own error type, never a substring; a control
whose clean run fails says so as its own failure, naming the plant; every plant variant has a
control or the build fails. Plants are a parameter of the code under test (rule 78), not
thread-locals in production modules. A test declares its kind (unit, simulation, world, scale,
process) and the kind decides its tier; scale tests run optimized.

## D166. Five layers, one store, a small document set

Crates in five layers, each depending only on those beneath it: base (bstd, allocators, ids, one
record-format crate, one lexer); front end (outline, expansion, names, types and traits, the
borrow rule, the one body: `front2` moves here out of blaze-ladder); back end and runtime; systems
(the store, fetch, toolchain, sandboxes, the engine with its stages, the journal and the sweep:
the `engine_*` files move here out of blaze-cli); the binary (arguments, `Home`, printing, exit).
Crates no binary reaches leave the workspace until wired, each with a line in parked.md saying
what would wire it; the profile crate is the exception to decide by itself, since the rules
depend on it. The toolchain shelf, sources, macro and script modules and run records become
namespaces of the one store. Heads are read in place by the solver (no copy into a job store).
A design document is `current` (names its crate) or `end-state` (names the unwired crate and the
live path it replaces); the `docs` script refuses a document of a deleted crate.

## D167. Today's errors, each closed by its design (2026-11-20)

The owner, 2026-11-20 16:49: "based on errors you are having redesign architecture to by design
fix these things". Each error of the day, its class, and the mechanism that makes the class
impossible. Where D160 to D166 already hold the mechanism the row says so; five are new here.

| The error | Its class | Closed by |
|---|---|---|
| The store committed a manifest its reader refuses (a 22 GiB merged segment) | writer and reader each define "valid" | D160: one validated row type; D165: the cap drawn by the simulation |
| A new build stopped on an older store's format | a format outside any key | D160, D162 |
| Two release steps broke on data files inside deleted crates | a fact read by path from another crate | D162: `pins/`, the include ban |
| The ready driver failed three fixes; the checker overflowed its stack | waiting done by restarting | D161 |
| Tests broke on a note's wording | a fact stored as a sentence | D164 |
| Planted-bug controls proved nothing for a day | a proof matched by substring | D165 |
| Three writers could not say whether a form number had to change | identity kept by hand | D162 |
| One macro call's bodies checked as one 28.6 s task on a 16-core machine | grain coarser than the design's | NEW (a) |
| Names waits: rounds with 0.1 s of work take 42 s and 26 s, and the profile does not say on what | a wait with no name in the answer | NEW (b) |
| The file-event model passed while the real service failed | a model never held against what it models | NEW (c) |
| A lifetime behind an attribute was miscounted, then the same in the impl reader | two hand walks of one syntax | NEW (d) |
| The install was verified on a fresh home only; a failed run's home was deleted | a check that cannot see the failing case | NEW (e) |

(a) **Body grain holds for bodies a macro wrote.** A task is one body (or a batch sized by
predicted cost, never by origin): a macro call that writes thousands of bodies is thousands of
tasks. The largest single task bounds a stage's wall time, so the profile prints it against the
stage's wall as a ratio with its floor (rule 67).
(b) **A phase's wall is accounted for.** Every stretch of a stage's wall is work or a named wait
(rule 15), and the text profile prints, per stage, wall = work + waits by what was waited on; an
unnamed remainder over 1% is a finding of the profile itself. With D161 a task waits on a name, so
the name is at hand.
(c) **A model is held against what it models.** Every simulated outside service (the file-event
journal, the file system's sync and rename, the clock, the process host) has one conformance
suite of cases that runs unchanged on the model and on the real service; a case added for a bug
found on the real one is added to both by construction.
(d) **One walk per syntax.** A generics list, an attribute list and an item header are each read
by one function in blaze-file-items over the lexer's tokens; a second walker of the same syntax is
refused by a lint naming the first.
(e) **A real run meets real state and keeps its evidence.** The real-run script has an upgrade
case (the previous binary makes a home, the new one runs on it) and a large-store case; a scratch
home is kept, with its path printed, whenever a command in it fails.

## D168. An edit costs the body it changed: the edit path's floor (2026-11-20)

The owner, 2026-11-20 17:33: "look at architecture and see how we can improve incremental times
... insanely fast ... otherworldly". MEASURED (tempo, one body edited, release build c1725wave42,
`reports/score-c1725wave42/tempo-score.edit.profile.txt`; the no-op beside it): the edit is
1.21 s and the body's own check is 31 ms of it; the no-op is 0.22 s. Everything else is a cost
that does not depend on what was edited.

| Stretch | Edit (ms) | No-op (ms) | What it does today | Floor |
|---|---|---|---|---|
| Bodies stage beside the body | 587 | 0 | opens the whole scope for one body: 1,204 names records (1.0 GB mapped) asked 96,285 times, 29,771 dependency heads listed to read 4, 150 member rows read, 624 scope crates | the 33 cells the body's last read list holds |
| Plan | 187 | 0 | reads 1,797 of 1,797 unit and file rows, stats 920 sources, writes a plan index | the changed file's row |
| Scripts | 123 | 0 | two build scripts run again (68 ms each) because a `Given` they read whole moved its key; 55 ms planning the next round | 0: no read of theirs moved |
| Journal | 99 | 141 | two file-event streams made, replayed, flushed and stopped | under 1 with a live helper holding the stream; today's cost with none |
| Sweep | 45 | 52 | stats 191 of 1,249 files for 1 event in 1 folder | the files told |
| The body | 31 | 0 | one small function; most of it is a run's setup (seeds, lang index, evaluator prefix) | 1 to 2 for a body this size |
| Print | 23 | 0 | replays 491 files' sections | the lines that moved, or one stored block |
| Open checkout | 22 | 22 | not yet sampled | 1 |
| Layout, start, rest | 20 | 7 | | 5 |

Floor: an edit of one small body on tempo in about 30 ms with the helper alive, about 130 ms
with none; a no-op in under 10 ms. The ratio to today is 40x and 20x. Six mechanisms, each the
end state of something already decided, none a new side record:

1. **A request starts from what moved, on the one driver (D157, D161).** The journal's changed
   files are outlined; the cells that moved wake their readers by the stored answer's reader
   edges; a woken task is decided against merged state and entered if a read moved. Nothing plans
   a stage, lists a member or reads a row that no moved cell reaches: "planning the next round",
   the plan's full read and the Bodies listing go with the phases.
2. **A world is opened by need.** A task's view maps a record when the task first asks a cell of
   it; the body's last read list is its gate set (D161), fetched together before it is entered.
   The directory from a crate or module to its record is one lookup, not one open per scope crate.
   A run's setup is by need too: a lang item or a seed is read when asked.
3. **A script reads what it read.** A `Given` is cells (one per file, env name or directory
   listing a script asked for), a script's read list holds the cells it touched, and it runs
   again only when one of them moved. No script reads a `Given` whole.
4. **A live helper holds the file-event stream (rule 70).** A command may leave a helper with an
   expiry that owns the journal's stream and answers "what changed since place P" over a Unix
   socket in under a millisecond; it never builds, holds only what the journal rebuilds, and a
   command with no helper replays the journal itself as today (the same answer, by the
   conformance suite of D167 c). The sweep stats the files told and nothing else.
5. **The answer is a delta.** The stored answer of a request is the previous answer's pages plus
   the rows that moved; the printed answer is the stored rendered block when no finding moved,
   else the moved files' sections spliced in.
6. **Start is nothing.** Opening the checkout and the store reads one pointer; the binary starts
   in the time it takes to map it.

Each lands with its counter printed against its floor in the edit's profile (rule 67), and the
scoreboard's edit and no-op are the gate: no piece is done while its row above is over twice
its floor.

## D169. Every stretch says what it used, not only how long it took (2026-11-20)

The owner, 2026-11-20 17:44: "the data should include allocations too ideally or other things
statistics you could find to figure out why things are slow". MEASURED on the same one-body edit
of tempo (1.21 s, the body 31 ms), from the process's own totals: 17.9 billion instructions
retired, 0.82 s of user and 0.35 s of system time, 1.06 GB allocated on the global heap (0.92 GB
freed), 154,194 page faults (2.5 GB resident at most), 21 mmap, 32 munmap and 102 mprotect calls.
The totals say an edit of one function touches gigabytes; nothing says which stretch does.

One meter, read at the points where time is already taken: a snapshot of this thread's counters
(`Meter::now()`), and a stretch's use is the difference of two. Counters: allocations and bytes
by allocator (global heap, each frame tag), frees, the largest single allocation; page faults and
pages mapped; instructions and cycles (the thread's own counters; a macOS private call is allowed
by rule 45, commented and tested); system calls by kind with their time (open, stat, read, pread,
mmap, fsync, the journal's); store gets, distinct keys, bytes read, bytes copied, puts; bytes
hashed and hashes made; lock and park waits. Every row of the profile that has a time has these
beside it: the command's phases, each stage and wave, the slowest tasks, and per body the mean
and the most. Each counter that has a floor prints it (rule 67). The meter's own cost is stated
and measured: counters are thread-local adds, read without a lock, and a build with the meter
compiled out is byte-equal in what it stores.

## D170. Everything the binary reaches is a sans-IO state machine on bstd (2026-11-20)

The owner, 2026-11-20 17:47: "is everything sans io and written with bstd? this should be a
requirement and you should rewrite everything to be this and explicit state machine"; "and if
bstd need to be changed then fix it"; "first analyze everything and then do a mass rewrite in
parallel". The analysis is three reports and one inventory in the notes
(`rounds/wave-1124-sansio/`: inventory.csv, A-base-and-bstd.md, B-middle-layers.md,
C-top-layers.md). MEASURED: 108 crates reached by `blaze` and its launcher (665,000 lines); 42
`#![no_std]` at the root; 28 marked sans-IO at the root; 2,775 `Global`; 293 `Alloc::global()`;
7,431 allocating calls with no allocator named; 85 `thread_local!`; 22 crates naming third-party
packages; no crate clean on every count; and no lint or ratchet that checks any of it runs.
FOUND by reading: most crates are already pure logic; the debt is a few shared seams, a dozen
places where IO is mixed into logic, the test plants, and allocation; about 60% of the top
layers' `Global` sits in code D156 to D168 delete; about 27,000 lines of the middle are called
by nothing reached. 55 crates no binary reaches are out of this work.

**The conventions (every brief quotes these).**
- C1 ALLOCATOR. A function that allocates takes `alloc: Alloc<'_>`; it is generic over
  `A: Allocator` only inside a collection type or a measured hot loop naming its frame. Never
  `Global`, never `Alloc::global()` (tests too: a test makes a frame or uses
  `blaze_own::testing`), never a default type parameter `= Global`, no `// alloc:` hatch.
- C2 CONSTRUCTORS. A type with choices is `T::new(TOpts { .. })`: choices required, tunables
  defaulted, a bool parameter an enum. Collections keep `new_in(alloc)` until the owner says
  whether the rule covers them (the allocator is their one choice and `new_in` names it).
- C3 NOTHING AMBIENT. A core takes no `Cx` and no capability: time is a value (`now: u64` in
  nanoseconds, a `Deadline`), environment and configuration a value struct its driver built. A
  driver takes `cx: &Cx` and named capabilities; below bstd a per-driver context built once in
  `main`. No `static` or `thread_local!` with state, except the allocator, the profile ring and
  the signal handler, each named and capped.
- C4 ERRORS. A typed enum a module, naming the item and the rule; no `String`, `format!` or
  `io::Error` in a public signature.
- C5 THE MACHINE. Logic that waits, interleaves or speaks a protocol is
  `struct M { state: State, .. }` with `State` an enum and no flags beside it;
  `new(MOpts)`, `step(&mut self, now, event) -> Result<(), E>`, `next_effect(&mut self)`,
  `next_deadline(&self)`, `state(&self)` (the form of blaze-stage's machine.rs). A codec is
  `feed(bytes) -> Progress` and `finish()`. A driver is thin straight-line code: it owns the OS
  handles, turns effects into calls and completions into events, and decides nothing.
- C6 TESTS. A planted bug is a `P: Planted` parameter with `NoPlant` in shipped code
  (blaze-own planted.rs; D165); a machine is proven in blaze-sim with `controls` and `plants!`;
  a driver has a conformance case (D167 c).
- C7 THE GATE. A crate is done when its root says `#![blaze::sans_io]` (a core) or names itself
  a driver, its ratchet rows are zero (std and alloc lines, `Global`, ambient reach, third
  party), the lints are at deny in the crate, and its tests pass. The lints
  (`sans_io_core`, `std_use`, `implicit_allocator`, `process_static`) are implemented and the
  ratchets run in the test round and CI before any crate is called done (T0).

**Decided with it.** Wasmtime stays, behind a thin driver (rule 10 names it). The Windows driver
and its package go (Windows is out of scope). `Alloc::global()` and `SharedAlloc::global()` go
everywhere. CPU feature detection goes through blaze-sys. Tools that never ship in the binary
(the model checker, the allocation counter, the derive macro) are exempt from C1. Vendored
cryptography and QUIC are not touched without the owner's word. Code the end state deletes is
not converted first; its replacement is written to these conventions from the start.

**The waves.** T0 the gate. W1 the leaf crates (41 crates, 54,000 lines: plants, root
attributes, the few ambient sites; public APIs unchanged), in parallel. The seams, each one
whole port with its callers: blaze-format-core's `Writer<Global>` default and `Persist` (13
crates); blaze-core's `Effect` and `Completion` payloads off the global heap with a typed
`IoError` (59 files, 8 crates); the parser's per-thread buffers as a scratch the caller passes
(8 caller crates); blaze-typeck's one allocator split into scratch and output. Then the middle
crates in parallel, blaze-mem and blaze-core driver by driver, bstd, the proc-macro host, and
the top layers' kept files (about 600 `Global` sites, 625 `format!` refusals, 25 plants, one
serve loop for run and test, a status machine, `main` taking its grants).

## D171. A worker's state is a value the pool owns, in typed columns (2026-11-20)

The owner, 2026-11-20 18:47, on a per-worker context: "YES add that ... should it kinda be like
an ECS too (so people can add arbitrary things)". Today a worker's state hides in thread-locals:
blaze-mem's frame stack and tag counts, the trace ring's per-thread half, the meter's counters,
test plants (85 `thread_local!` in the reached tree). A fiber that parks and resumes on another
thread reads the wrong one, a simulation cannot give each simulated worker its own, and nothing
in a signature says the state exists.

`Workers` is built once, in `main`, by the pool's options. It has the few fields bstd itself
needs (the worker's id, its scratch frame, its trace sink, its meter, its random stream) and
TYPED SLOTS for everything else. It takes the ECS's storage and not its machinery: a slot is a
column, one cache-line-separated cell a worker, indexed by the worker's id; a worker is the
entity; there are no archetypes, queries, names or `TypeId` lookups.
- A slot exists only by registration: `let key: Slot<T> = opts.slot(|worker, alloc| T::new(..))`
  before the pool is built. `Slot<T>` is a `Copy` token only that call makes, so a key without a
  column cannot be written and "missing" is not a state (rule 76). Reading it is two indexes.
- A crate adds its own (the checker's per-worker tables, the store's per-worker segment writer
  of D159, the engine's result buffer) without bstd knowing the type; `main` is where each is
  named, as it names its grants.
- A core never sees `Workers`. Its driver takes the cell and hands the core `&mut T` (C3).
- The cell is reached only inside `cx.on_worker(|w| ..)`, a closure that cannot wait, so nothing
  of a worker is held across a wait (rule 27) by construction, and a fiber that moves threads
  gets the worker it is on when it next asks.
- A simulation builds `Workers` for its simulated workers; a test builds one.
- Deletes, as each moves: blaze-mem's thread-local frame stack and tag counts, the meter's
  thread-local counters, the plants' thread-locals. The trace ring below bstd stays the one
  static C3 allows; code that has a `Cx` writes its spans through the worker's sink.

## D172. The open questions of D168 to D171, decided (2026-11-20)

The owner, 2026-11-20 18:49, asked which way each should go: "just do the best long term option
yea / whatever makes u happiest". Each decision is the one that leaves less to maintain and
nothing to remember.
- COLLECTIONS keep `new_in(alloc)`. The Opts form is for a type with choices; a collection's one
  choice is its allocator, and `new_in` names it. Rewriting 8,000 call sites would add no
  information. (C2 stands as written.)
- THIRD-PARTY PACKAGES leave every crate the binary reaches, except Wasmtime (rule 10) and the
  vendored cryptography and QUIC (rustls, ring, quinn-proto with bytes), which stay behind thin
  drivers and are not edited: an own TLS is a security decision of its own, not a cleanup.
  serde, serde_json, url, imbl, sonic-rs, tracing, stacker and rustc-stable-hash go, each
  replaced by the tree's own crate; `blaze-serde` and `blaze-serde-derive` go with their last
  user; libc calls move behind blaze-sys; objc2 moves out of bstd into the application crate
  that needs it.
- CODE NOTHING REACHED CALLS: what is plainly dead is deleted (stdx, `Screen`, the legacy
  `Report`, blaze-protocol's `follow.rs` and its serde CBOR, `HashCons`, blaze-remote's bucket
  code, blaze-launch-rt's daemon path). What is a capability blaze will want again (the
  manifest's cargo layer, blaze-resolve, blaze-source: making and updating a lock file) leaves
  the workspace and is recorded in parked.md with what would wire it, on these conventions.
- ONE HTTP CLIENT: bstd's, as a sans-IO machine over the one `Effect` boundary; blaze-http keeps
  the protocol cores (h1, h2, QUIC) and loses its own client.
- THE JOURNAL HELPER is built (D168, 4): it is the only way under a hundred milliseconds from a
  cold process, the rules allow it, and killing it changes no answer.
- RUN AND BODIES WITH NO TYPED RECORD (toolchain and dependency bodies): there is one check and
  it is the engine's. A body `run` needs that has no record is a `Need` for its `BodyCheck`
  task: the engine checks it once, stores the record, and `run` reads it. `SecondCheck` and the
  lazy program's own front end are deleted at that switch; nothing checks a body twice and
  nothing falls back.
- A FETCH'S BOUNDED RETRY (blaze-fetch `tries_most`) goes: a failed fetch is a typed error
  naming the source (rule 3); the user asks again.
- UNVERIFIED WORK IS NOT STACKED ACROSS SEAMS: while builds cannot run, new work is additive
  (a new crate, a new module, a base addition); a seam that changes a shared signature waits
  for the pile before it to compile.

## D173. There is no global allocator to reach; the heap is a grant (2026-11-20)

The owner, 2026-11-20 18:53: "ideally no global allocator and need explicit". Three things share
the word and only one may survive. (1) `Global`, the type: gone from every reached crate (D170
C1). (2) A way to get the process heap from anywhere: `Alloc::global()`,
`SharedAlloc::global()` and `Heap::process()` (blaze-own alloc_ref.rs, a `const fn` any code can
call) all go: the process heap is a GRANT. `main` names it in its signature (`heap: Heap`, as it
names `out: Out`), and it is passed down like any allocator; a test makes its own. (3) Rust's
`#[global_allocator]` static: blaze's own crates are `#![no_std]` with no `alloc` crate, so they
cannot name `Box::new`, `Vec::new`, `String` or `format!` at all: that is the compiler's
refusal, not a lint's. The static exists in exactly one place, the binary crate, for exactly one
reason: third-party code compiled into the binary (Wasmtime, the vendored cryptography)
allocates through Rust's hook. It is named the foreign heap, it is counted by the meter (D169)
as its own line, it has a cap, and the gate proves no blaze crate links `alloc`. When the last
third-party package goes, the static goes. The gate and the lint read one table of the statics
C3 allows (the foreign heap, the profile ring, the signal handler).

## D175. WebAssembly is a bridge; compile-time code runs as blaze's own code (2026-11-20)

The owner, 2026-11-20 19:01: "do we even want/need web assembly ... long term that stuff should
be comptime stuff and it is kinda just a for now thing". Agreed, and recorded so nothing new is
built on the bridge.

What wasm does today (rule 10): a proc macro or build script is arbitrary user code that runs
while compiling, so it is compiled to `wasm32-wasip1` and run in Wasmtime, which gives a sandbox
(no ambient files, network or clock), the same result on every host, and memory isolation. What
it costs: Wasmtime is the one third-party runtime left in the binary; a second target, a second
standard-library build and a WASI layer exist only for it; and compile-time code runs through a
second execution path beside the one blaze is built around.

THE END STATE. Compile-time code is ordinary code run by blaze itself: compiled by blaze's own
back end to code records and run by blaze's own launcher in its own process, as any program is.
What makes it safe is what makes blaze's own code safe: nothing ambient. Its standard library's
system layer is calls to blaze that hold only the grants the compile gave it (the tokens in, the
tokens out, the files its package declared), so there is no file, clock, environment or network
to reach; the operating system's process sandbox is the second wall; fuel and a deadline bound
it (rule 18). A proc macro is then one comptime function from tokens to tokens, and a build
script one from a request to directives; an existing crate's macro runs unchanged, because the
library under it is the one that changed.
- Deletes at the switch: Wasmtime and its two crates, the wasm target and its standard-library
  build, blaze-wasi, wasi-libc in the shelf, the proc-macro host's WASI view.
- Oracle before the switch: each of tempo's macro expansions and build-script outputs is
  byte-equal run both ways; a macro that reads what it was not granted is refused by name on the
  native path as it is on the wasm one.
- What is given up, stated: one expansion record valid on every architecture by construction
  (native runs can differ across architectures in floating-point edge cases); the record's key
  therefore names the architecture until a test shows the outputs equal.
- Until then: wasm stays for proc macros and build scripts, nothing new is added to it, and the
  C compiler never goes through it (D174).

## D177. One way to run a program hermetically, in bstd (2026-11-20)

The owner, 2026-11-20 19:09: "should bstd have api for spawning command in sandboxed
deterministic way? i think we already built some kind of api thing for this". Three partial
things exist and none is the whole: bstd's `Spawner::start` and `start_served`
(procs/linked/run.rs: a child with typed streams, a channel, a typed end, never outliving its
parent, and a simulated stand-in: the lifecycle, with no confinement and no account of what the
child read); the WebAssembly sandbox for compile-time code (confinement and a read list, for
wasm only, a bridge: D175); and the hypervisor sandbox (full determinism for native tools; not
run without the owner's word). Four users need the same thing: the C compiler (D174),
compile-time code (D175), a build script's native tools, and sandboxed tests.

ONE capability over `start_served`: `cx.hermetic(HermeticSpec { program, args, env, roots,
clock, seed, budget, deadline, streams, level })`, returning the exit, the outputs and a
`Checked<ReadList>` (blaze-roots). Nothing is inherited: the environment is the list given, the
files are the roots given (read-only trees and one output directory), the clock and the random
seed are values. What is promised is a TYPE, `level`, because the two promises differ and one
must not be mistaken for the other:
- `Level::Inputs`: the program sees only what it was given, held by the operating system
  (Seatbelt on macOS, Landlock and seccomp on Linux), and what it read is reported. Its output
  is a function of its inputs IF the program is: right for a pinned tool of blaze's own.
- `Level::Execution`: the run itself is determined (scheduling, time, addresses): for arbitrary
  programs. Its backends are a program built by blaze and run on blaze's own launcher against a
  system layer that is calls to the host (D175), and the hypervisor sandbox where it is allowed.
A level no backend on this machine can give is a typed refusal naming the level and the program
(rule 9), never a weaker run. The simulated world implements the same call, so code that runs a
tool is tested without one.

Decided 2026-11-20 (the coordinator, three answers). (1) blaze-core's one linked start gains a
step in the child and stays `clone(CLONE_VM|CLONE_VFORK)`: the step is data, not a closure, a
fixed program of raw system calls (`prctl(PR_SET_NO_NEW_PRIVS)`, `prlimit64` rows,
`landlock_restrict_self`, the seccomp filter) the child runs between `clone` and `execve` with
no allocation, no lock and no write to the parent's memory but its own stack and one error slot;
a refused step exits the child, and the parent returns a typed error naming the step. macOS keeps
`sandbox-exec`: no `posix_spawn` attribute applies a Seatbelt profile. (2) `Hermetic` is a grant
kind (`hermetic: Hermetic`), still made from a `Spawner` and a `Clock`. (3) Giving up the user
and network namespaces is accepted: the seccomp filter refuses `socket` and Landlock the files.

## D178. Telemetry is a property of bstd, not of each crate (2026-11-20)

The owner, 2026-11-20 19:22: "any more telemetry that we could add ... it would be great if
everything that uses bstd could have this option by default rather than us adding ad hoc per
crate where we have to remember". Today's telemetry was added by hand, place by place: the
command's phase marks, the engine's stage tables, the Names timeline, the bodies' counters, each
in the command crate, each for one program. Everything a bstd program does already passes
through a few choke points bstd owns (the allocator handle, the effect a driver runs, a wait, a
lock, a span, the pool that runs a task), so the measurement belongs there, once.

A program on bstd gets, with no code of its own, for every `cx.span` and for the run:
- what it USED (D169): time, instructions, allocations and bytes by allocator, page faults,
  system calls by kind, bytes hashed;
- what it WAITED on (D171): by name, with the holder for a lock, and a distribution, not only a
  sum (sixty-four power-of-two buckets a name: the median, the 99th percentile and the longest);
- IO by effect kind: count, bytes, and the same distribution of latency; syncs apart, since a
  durable write's cost is its syncs;
- the POOL: each worker's time working, parked and idle, tasks run, the longest task, the depth
  of the ready queue over the run in a hundred samples, and why a worker was idle (nothing ready,
  or everything ready was waiting);
- the CHAIN: because every wait names what it waited on, the runtime computes at exit the
  longest chain of waits and work that ended the run (the critical path), for any program, not
  only the Names stage;
- the reconciliation: the sum over spans against the process's own totals, with the gap printed.
One typed `Report` holds it; the runtime hands it to the sink `main` was granted (nothing
ambient: no environment variable switches it on); it has one deterministic encoding and one
text rendering. Its level is a value (`Totals`, `Spans`, `Detail`): `Totals` is always on and
costs thread-local adds; `Detail` (allocation by site, sampled stacks) is asked for. Telemetry
never enters a key or a record: stored bytes are equal at every level (tested). A crate adds a
counter of its own only for a domain fact (bodies checked, records read), through a typed slot
of the worker (D171), and never re-measures what bstd measures.
- Deletes: the command's hand-kept phase and stage timing (phases.rs marks, engine_times.rs's
  tables and timeline, the per-stage `used` plumbing), as each is shown equal to the default.
- The profile crate (reached by no binary today) becomes a reader of the `Report`'s encoding or
  goes; there is one telemetry core.
- **The sampler is part of it** (the owner, 2026-11-20 19:31: "ideally ... have some kind of
  sampler or something that you can figure out hot spots ... by default"). Counters say how much
  a span used; only a sampler says WHERE in the code. The runtime owns one: a sampler thread
  that, every period (a value in the runtime's options; a millisecond by default at level
  `Spans`, off at `Totals`), takes each worker's program counter and frame-pointer chain and the
  span the worker's cell says it is in, into a bounded per-worker ring (GATHERED, D179). On macOS
  by suspending a thread and reading its state (the kernel's thread calls); on Linux by a
  per-thread timer whose handler reads its own context, or the kernel's sampling ring where it
  may be opened. Parked workers are not sampled (the cell says it is parked), so the table is
  on-CPU time, which the system's `sample` cannot give without filtering. At exit the report
  holds, for the run and for each span: the functions on top of the stack by share, and for the
  top few, who called them; names come from the binary's own symbol table, read once at exit.
  Addresses, not names, are what is stored while the program runs. The same table with the
  processor's counters where the kernel grants them (instructions, cache and branch misses a
  function). This is what today's survey for vector code did by hand with the system's sampler
  and a script (tools `tempo-sample.sh`, `sample-leaves.py`); those go when this prints the same.

## D179. State shared between workers is one of three shapes; a lock is the named exception (2026-11-20)

The owner, 2026-11-20 19:23, on "state shared between workers should be one of three things":
"ok then design for this". The design is three primitives in bstd, each with one job, so that
code which shares state picks one by name and a lock is what is left when none fits.

1. **PUBLISHED: written once, then only read.** `Published<T>`: one owner fills it, and from the
   moment it is published it never changes; readers take `&T` with one atomic load and no lock.
   Reading it before it is published is not an error and not a spin: it is a `Need` (D161): the
   task parks and resumes when it is there. Its bulk form is the record: written once to the
   store, read in place by every worker. Heads, names, outlines, impl tables, the plan, the
   toolchain's facts: everything a stage produces for later stages is this.
2. **GATHERED: each worker writes its own, one owner merges.** `Gathered<T>` is a column of the
   worker cells (D171): a worker reaches only its own cell, with no atomics at all; the owner
   calls `gather` at a boundary it chooses (a tick, the end of a stage, the end of the run),
   when no worker holds a cell, and merges the cells IN WORKER-INDEPENDENT ORDER (by key, never
   by which worker or when: rule 25), so the result is the same on 1 and on 128 workers. Results
   of tasks, findings, read lists, counters and telemetry, store segments being written, IO
   submissions: everything workers produce in parallel for one consumer is this. A queue from
   workers to one owner is this shape with the merge done as items arrive.
3. **CLAIMED: the first to ask makes it; the others take what was made.** The claim table
   (blaze-core claim_table.rs, with its linear `Claim`): keyed by what is to be made; a worker
   that claims a free key makes the value and publishes it; one that finds it made reads it; one
   that finds it being made parks on it. Lock-free on every path, and what is made is a
   `Published` value, so 3 ends in 1. Anything computed on demand by whichever worker needs it
   first is this: a head, a constant, an instance, an interned value.

**The exception.** A `Lock` or `ReadLock` (always named) is for state that is mutated in place
by several workers and can be none of the three: the tree is to have very few. Each one says, at
its construction, which shape it was tried as and why that failed (an attribute the lint reads,
not a comment); the gate holds the count of locks in shipped code as a ratchet that only falls;
and the profile's `locks:` section (waits by lock, the holder that caused them, and the locks
never contended) is how a lock that should be one of the three is found.

**What becomes what** (from reading; the list is completed by the writer on the locks piece):
bstd's `World` lock on every IO submit and poll: GATHERED (a submission ring and a completion
queue a worker; the lock stays around the driver alone). The engine's results: GATHERED (it is
already merged by one thread at a tick, D157). The store: PUBLISHED records, GATHERED segment
writers a worker, one owner for the head's flip (D159). Heads, names and impl tables: PUBLISHED
through CLAIMED. Interners: CLAIMED (a value is interned once, by whoever meets it first). The
meter, the trace and telemetry: GATHERED. The proc-macro host's condition variable and threads:
a machine fed by events (D170), which is GATHERED with one owner.

## D180. No package from a registry: fifteen names to zero, the rest pinned in the tree meanwhile (2026-11-20)

The owner, 2026-11-20 19:40: "can we try to vendor all of our crates (no crates from crates.io)
or control all of our own". The scope already says blaze's own code uses no third-party crate;
this makes it a counted goal with an order. Measured on the verification tree (cargo metadata,
normal and build edges from the `blaze` binary, every platform): 107 crates of ours, 5 vendored,
and **205 registry packages**; but only **15 third-party packages are named directly**, by 21 of
our crates, and each brings a closure:

| named directly | by | closure | how it goes |
|---|---|---|---|
| wasmtime | blaze-proc-macro, blaze-wasi | 115 | D175: compile-time code runs natively; deleted at the switch |
| sonic-rs | blaze-report | 62 | the legacy report goes; JSON out by blaze's own writer |
| quinn-proto, rustls, bytes | blaze-http | 62, 51, 1 | LAST: blaze's own TLS 1.3 and HTTP client; until then vendored and pinned (they are vendored today) |
| ring | blaze-remote | 45 | the remote bucket is parked (goal revision 2026-11-17): the crate leaves the binary |
| stacker | stdx | 37 | stdx is deleted (D172); deep recursion is a frame with a stated bound |
| url, serde_json, imbl, rustc-stable-hash | blaze-manifest, blaze-resolve, blaze-source | 36, 15, 10, 1 | the cargo layer is parked (D172); manifests are read by blaze-toml and typed paths |
| windows-sys | blaze-core, blaze-mem, blaze-check, blaze-profile (blaze-graph named it unused) | 10 | blaze-sys declares the Windows imports itself (`windows/`, `raw-dylib`, no import library); the driver stays (corrected 2026-11-20: Windows is not deleted, see sys.md "Windows") |
| serde | nine crates | 7 | records are blaze-format and CBOR; the derives go with blaze-serde |
| libc | nine crates | 1 | blaze-sys is the one home of kernel calls (D170) |
| tracing | blaze-proc-macro | 4 | spans are bstd's (D178) |

- **The count is a ratchet in the gate**: registry packages reached by the binary (205 today),
  names of third-party packages in our manifests (15), and the same two for the whole graph with
  test edges (274 packages). Each only falls; a new name is refused.
- **Order**: what is already decided and blocked on nothing but the tree compiling (windows-sys,
  libc, tracing, stacker with stdx, ring with the remote crate, the cargo layer's four, sonic-rs,
  serde), then wasmtime at D175's switch, then TLS.
- **Meanwhile nothing is fetched to build blaze**: when the count has fallen to what is truly
  left (the TLS and QUIC closure), that remainder is placed in `vendor/` pinned by digest and the
  workspace builds with the registry replaced by it, so a build of blaze needs no network and no
  registry. Copying all 205 into the tree today would add tens of megabytes of code that the same
  month deletes (wasmtime's 115 first); the lock file already pins every package by checksum
  until then.
- **Test-only packages** (the official blake3 crate a cost example compares against, and the
  like) are named in one list with the reason each is there; a comparison that has done its job
  is deleted with its package.
- **Open for the owner**: TLS and its cryptography are the one place where "our own" carries a
  security cost; the lean is our own with the system's verifier for certificates, proved against
  the standard vectors, and vendored until that exists.

## D181. What a command shows while it works: the answer's own state, in the user's words (2026-11-20)

The owner, 2026-11-20 19:50: "improve blaze run / build etc to show exactly what it is building
at that certain time and progress ... think what the most SOTA design would look like". Today a
command prints, every five seconds, `t 10 s · Names 23/24 tasks · footprint 1.4 GB ... longest:
Name { kind: Script, subject: d27e37abf714 } 6 s`: a stage name of ours, a count whose total
grows as work is found (2/4, then 23/24, then 31/37), and a debug-printed key with a hash.

**What a person needs at any moment, and where each comes from (nothing is kept twice):**
1. **Is it moving, and how much is left.** An incremental tool knows more than a cold one: the
   stored answer holds the last run's tasks and what each cost. Progress is the share of
   EXPECTED WORK done (last run's cost of what is owed, by key), not tasks counted; an edit says
   up front `1 function to check, 4 scripts to ask` before it starts. Where there is no history
   (a first check) the line says so: `found so far 1,206 · still finding` and no percentage is
   invented. The estimate's error is recorded each run, and printed when asked.
2. **What it is doing now.** The workers' cells already hold the task each is on (D171): the
   view shows the few longest-running ones BY NAME: `build script of librocksdb-sys 0.17.3
   (22 s)`, `expanding sol! in crates/contracts/src/lib.rs`, `checking fn execute_block in
   reth-evm`. Every task kind has one typed subject that renders for people (rule 19 applied to
   progress): a crate with its version, a file, an item path. No hash, no stage name of ours.
3. **What the answer is waiting on.** Because every wait is named (D161, D178), the live
   critical chain is a walk from the answer: `waiting on: names of alloy-primitives ← sol!
   expansion ← build script of librocksdb-sys (22 s, 1 core)`. This is the line that tells a
   person why sixteen cores are idle.
4. **Why this work at all** (on an edit): `orderbook.rs changed → is_empty rechecked; its 3
   callers stand (signature unchanged)`. The engine has the cause; the view prints it.
5. **Errors as they are found**, above the live region, never held for the end; the live region
   is the only thing redrawn, so scrollback holds findings and the final line only.
6. **The end**: one line: `checked 16,556 functions in 1.2 s · 1 rechecked, the rest stand ·
   0 errors`; with `--why`, the chain that took the time.

**How it is built.**
- ONE EVENT STREAM, typed, in deterministic CBOR with one versioned schema (rule 33): task
  known, started, waiting on, done, finding, phase; the terminal view, a plain log for CI, an
  editor and an agent are all readers of it (`--events <fd>`). The stream and the telemetry
  report (D178) are the same facts: live, and summed.
- THE VIEW IS A PURE FUNCTION from a snapshot (the ready driver's facts, the workers' cells,
  the history's expected costs, the clock, the terminal's width, height and abilities, all
  passed in: rule 75) to a frame; a sans-IO machine decides when to draw. It is simulated:
  drawn task graphs and terminal sizes; a frame never exceeds the terminal, never shows a hash
  in the person view, and two snapshots that differ only in an unshown task draw the same frame.
- NO COST TO THE WORK: nothing is sent per task; a view thread samples the cells (GATHERED,
  D179) at most twenty times a second and draws a frame only when it differs.
- NOTHING IS SHOWN FOR A FAST ANSWER: no frame before 200 ms, so an instant check prints only
  its answer. Not a terminal: no cursor movement at all, a plain line when a phase ends or a
  task passes ten seconds.
- THE TERMINAL IS USED PROPERLY: synchronized output so a frame never tears, hyperlinks on
  `file:line`, the terminal's own progress indicator, colour only by tone, and width respected.
- `run`: the live region is gone before the program's first byte; the program's output is never
  mixed with blaze's; compiles on first call after that are silent unless asked for.
- AFTER THE FACT: the events are the run's record, so `blaze why` and a timeline view read the
  same stream; nothing is reconstructed from logs.
- Deletes: `engine_status.rs`'s line, blaze-screen, the debug names in status output.

## D182. Block storage is a capability; the store's bottom is a range of blocks (2026-11-20)

The owner, 2026-11-20 19:55: "should bstd have a raw nvme thing in case you are handed raw nvme
or a raw block thing ... capabilities can depend upon"; then "ok do this". bstd gains a grant,
`Block`: a range of durable blocks, with nothing said about what is behind it.

- **The interface** (effects of a sans-IO machine, as all IO is): read and write at
  block-aligned offsets and lengths; `Durable::Yes` on a write (durable when it completes: a
  forced-unit-access write where the device has one, else write then flush); flush; discard; and
  the device's FACTS as a typed value read once at open: logical and physical block size, the
  largest write the device completes all-or-nothing, the queue depth, whether a volatile cache
  sits in front, the size. Offsets and lengths are types that are aligned by construction
  (rule 76): an unaligned write cannot be written.
- **What stands behind it**: (1) a preallocated file (every Mac, every test); (2) the simulated
  disk, with crashes that tear unsynced writes at the device's atomic unit and no smaller;
  (3) a raw block device on Linux through the ring, buffered by default so mapped and in-place
  reads keep working (rule 51: the page cache is the only cache), direct IO a separate measured
  choice; (4) native NVMe commands only when a measurement asks. One capability, each driver
  held equal by the same conformance suite the file drivers have (D165).
- **Capabilities compose**: a store is made from a `Block` or from a directory; a directory-like
  capability can itself be made over a `Block`. `main` is handed what the machine has
  (rule 75; main names its grants) and nothing above changes.
- **Why**: on a one-function edit about a tenth of the command's CPU is `open`, with `stat`
  and `read` beside it (sample of 2026-11-20); a store in one range of blocks opens nothing by
  path. The day's durability bug class (a new folder never made durable in its parent) has no
  folder to occur in. And where the device states an atomic write unit, a page of that size
  cannot tear, which is the fact the verify-once proof of the pages wants.
- **The store's bottom becomes a seam** (rule 78): one type parameter, `OverDir` and
  `OverBlock`, each instantiation run through the store's crash simulation with the same oracle
  and tested equal in what a reader sees. `OverBlock` lays segments out in the range itself (a
  superblock pair for the head, written in turn; extents; a free map rebuilt from the head).
  `OverDir` is today's store and stays the default until `OverBlock` passes on tempo.

## D183. A CAD library in blaze: models are programs over an implicit kernel, every result a record (2026-11-20)

The owner, 2026-11-20 20:01: "make your own CAD software that is super great for building SOTA
ships ... a general builder ... it should be a crate in blaze we can import, since blaze should
have a lot of these crates ... should allow caching cross run ... really powerful models ...
mesh optimization". He builds ship models for a game today from Python over a C++ boundary-
representation kernel (build123d on Open Cascade; 2,188 lines for one ship). blaze's library
answer is its own, on bstd, with no package from a registry (D180), as crates anyone imports.

**The kernel is implicit, not boundary representation.** A solid is a function from a point to a
signed distance, built as a typed expression graph. This is the choice that makes "from scratch"
honest: unions, cuts, shells, offsets, rounds and blends are a few lines each and cannot fail,
where the same on trimmed spline surfaces is the fragile part of every boundary kernel and
decades of work. It is also where the field is (libfive, Fidget, nTop): one tape, three
interpreters: points in SIMD batches, intervals (which prove whole regions empty or full, and
shorten the tape for what is left), and derivatives (exact normals and curvature). What is given
up, stated: no STEP exchange with a machine shop, and surfaces are not splines. For models whose
product is a mesh, colliders and mass, neither is needed.
- **Exactness where it matters is in types.** A field knows whether it is a true distance or only
  a bound (rule 76): an offset or shell of a true distance is exact, of a bound it asks for
  re-distancing first. Parts meet at named FRAMES (datums), never at faces; a small solver turns
  mates and joints into transforms, so assemblies are exact by construction and articulation is
  data that is exported.
- **Lengths, angles and masses are typed quantities**; a model cannot add metres to degrees.
- **A model is a program**: a typed builder API in Rust (later compile-time code), giving a
  part graph: name, field, material zones, frames, joints, mass, collider, children; parameters
  are values, variants are parameters. Detail (panel seams, vents, fasteners) is a separate
  layer of the field that coarse levels and colliders do not see.
- **Meshing** is adaptive: an octree refined where the interval says the surface is, vertices
  placed by minimising distance to the surface's tangent planes so sharp edges survive, output
  proven watertight and manifold with a stated error bound. Then: simplification with creases
  and material borders kept, a chain of levels of detail, clusters for the renderer, cache-
  ordered and quantised buffers, convex pieces or the exact primitive for collision, and mass
  properties integrated over the octree.
- **Every result is a record** keyed by the expression's digest, the tolerance and the
  algorithm's identity (rule 52): tapes, octree cells, meshes, levels, colliders. A second run
  computes nothing; changing one part re-meshes only the cells whose shortened tape names it.
  This is the engine's incremental mechanism used by a second program: the first outside user
  of "incremental computation as a library".
- **A small change is fast, by the same floor rule as the compiler** (the owner, 20:10: "make
  sure it is fast especially if making small changes"; rule 67). Changing one parameter of one
  part does work proportional to what that parameter reaches: the nodes above it are re-keyed
  (hashing, microseconds), only the octree cells whose shortened tape names the changed node
  are evaluated again, only those cells' triangles are re-made and stitched to unchanged
  neighbours at a fixed boundary rule, only that part's levels of detail, clusters and collider
  are rebuilt, and the file is written from records. Targets, stated so they can be missed: an
  unchanged model, nothing recomputed and under 50 ms to the same bytes; one dimension of one
  part in a hundred-part ship, a new picture in under 100 ms and a new mesh file in under a
  second; each run prints cells evaluated against cells kept, as the compiler prints bodies
  checked against bodies standing.
- **A record must earn its place** (the owner, 20:19: "some small things shouldn't need saving
  to disk"). Storing costs a hash, a write and later a lookup; a result cheaper to recompute than
  to look up is never stored. So: fingerprints and compiled tapes are recomputed every run
  (microseconds); a single leaf cell's few triangles are never a record; the unit that IS stored
  is a block of work above a stated cost (a brick of cells, a part's mesh, a level of detail, a
  collider), and the cost is counted in work done (tape operations times points), not in time,
  so what is stored does not depend on how loaded the machine was. Whether a thing was stored
  never changes an output byte. Within one run, sharing equal sub-models (hash-consing) is the
  only cache and it is memory. The floor is stated per stage as recompute cost against lookup
  cost, and the run prints records read, records written and work recomputed below the line.
- **What goes wrong with keying by content, and the design's answer to each** (the owner,
  20:19: "a rotate for instance would be really disk sensitive ... should there instead be
  checkpoints? consider everything that would go wrong"):
  1. *Moving or turning a part changes every key under it.* Geometry is made in the part's OWN
     frame and placement is data on the instance, never part of the field: a rigid transform (or
     a mirror, kept as a flag) at the top of a part is hoisted out of its expression before
     anything is keyed. Rotating a part computes nothing and stores nothing; it changes one
     matrix in the scene. Two hundred equal bolts are one geometry and two hundred placements.
  2. *The same shape written two ways has two keys.* Expressions are put in a canonical form
     before hashing: chains of transforms composed, rigid transforms pushed to one place,
     commutative operands ordered, identities removed.
  3. *The same number computed two ways has two bit patterns.* Authored parameters are exact
     fixed-point quantities (a length is a whole number of nanometres), so 0.1 + 0.2 and 0.3 are
     one key; values a solver produces are snapped to that grid before they enter a key.
  4. *Dragging a value writes a record for every position passed through.* THREE TIERS, not
     one: cheap results are recomputed; results of the current session live in a bounded memory
     cache; the disk is written only at a CHECKPOINT: a commit of the model, a passed check, an
     export. A state that was only passed through is never stored. A checkpoint is also the root
     that keeps records alive; cleanup removes what no checkpoint reaches, oldest first, to a
     size budget, and never blocks a run.
  5. *A transform inside a part (a hole moved) changes more than it should.* It changes the
     cells at the old place and the new place and no others, provided the kernel can prove the
     rest independent; loose proofs are the risk, so every node carries a bounding box used
     before intervals are, and a sub-model is evaluated in its own frame (the query box turned
     into that frame) so a rotation does not blur its bounds.
  6. *Changing the tolerance changes every key.* Cells are a hierarchy: a finer run keeps every
     coarser cell and refines below it; a coarse preview and a fine export share records.
  7. *Kept and re-made cells do not meet.* What a cell stores is a function of its key alone
     (its edge crossings and vertex), and the triangles between two cells are made from the two
     cells' stored data by one rule, so a seam cannot depend on which run made which side.
  8. *One small change redoes a whole part's simplification and levels of detail.* True in the
     first version, bounded by keeping parts moderate in size; the end state simplifies in
     patches with fixed borders, so only the patches that changed are redone.
  9. *Operations that are not local* (re-distancing; a blend with a large radius) depend on a
     neighbourhood: their keys include the neighbours' keys out to the stated reach, and the
     reach is printed, so a model that defeats locality says so.
  10. *References break when shapes change* (the naming problem of boundary kernels): nothing
     refers to a face or an edge. Parts meet at named frames; material zones and detail are
     regions given as fields.
  11. *Two processes make the same thing, or one dies mid-write.* Equal keys give equal bytes, so
     a race is harmless (a claim avoids the double work), and a record is visible only whole.
  12. *The algorithm changes.* Its identity is in the key (rule 52); old records are simply never
     asked for again and are cleaned up.
- **Deterministic**: the same bytes on every machine and worker count (rule 25), which needs
  blaze's own floating-point functions (`blaze-math`: the platform's differ), a thing other
  crates will use too.
- **Out**: binary glTF with each part's metadata; a ray-marched picture for a quick look and for
  golden tests; scene description later.
- **Proof on a real model first** (rules 7, 82): the owner's existing ship, rebuilt from its
  Python source, loaded by his game.
- **Written to be used by a model as much as by a person** (the owner, 20:04: "good for LLMs to
  use and make things"). An author that cannot see needs the library to tell it what it made:
  (1) ONE way to do each thing, every argument NAMED (an options value a type, D126), no
  positional floats, typed quantities, so a wrong call does not compile; (2) errors name the
  part, the operation and the value, and say what would be accepted; (3) INSPECTION is part of
  the API: bounds, volume, mass and centre, thinnest wall, clearance and interference between
  parts, disconnected pieces, a slice at a plane, each as a typed answer; (4) PICTURES: fixed
  views (six sides, one three-quarter) and sections, rendered from the field itself to an image
  in one call, so the author looks at its work and corrects it; (5) a SUMMARY of a model as
  text: the part tree with each part's dimensions, frames and mates, small enough to read back;
  (6) checks a model can assert (`min_wall(2 mm)`, `no_interference()`, `fits_within(..)`) that
  fail with the place and the measured value; (7) one reference page of examples that are
  compiled and run by the tests, so it is never out of date.
- **The asset is one canonical value, and every tool is a view of it** (the owner pasted
  another session's design, 20:06, and asked whether there is a better one). What that design
  gets right is kept: one model of record holding parameters, named parts, frames and sockets,
  material roles, collision and mass, joints, gameplay metadata and detail rules; an author (a
  person or a model) changes it by TYPED OPERATIONS, never by editing mesh bytes; every change
  is a structured difference, checked, and rolled back when a check fails; previews and
  clearance tests come with it. Here that is not a new mechanism: it is the long-term design
  blaze already has for programs (the world as a value with typed patches, `patch` then
  `check`, a transaction), applied to a model. What differs from that design, and why: no
  wrapped C++ kernel as the core and no third-party physics or mesh-optimisation packages
  (D180; and an implicit kernel does what a ship needs without one); no round trip through a
  separate modelling program as a build step (a build must be a deterministic function of the
  model: an outside program can be a VIEWER of the exported scene, and an imported change is a
  typed patch like any other); and results are records, so a change rebuilds only what it
  reaches. Outputs for a game: render meshes with levels of detail, one acceleration-structure
  group per articulated part, colliders, mass and inertia, material roles, walkable volumes,
  entity prefab data, and binary glTF.
- **The classic C++ kernel is a test of the C++ import, and an exchange path** (the owner,
  20:07): Open Cascade's headers are rung X3 of c.md's ladder. If that import works, a model can
  be handed to it for a STEP file, which closes the one thing the implicit kernel gives up,
  through an import and not a dependency.
Crates: `blaze-math`, `blaze-units`, `blaze-cad-field` (graph, tape, interpreters),
`blaze-cad-mesh`, `blaze-cad-out`, `blaze-cad` (parts, frames, mates, patches and checks, the
builder; the crate people import).

## D184. Quantities are typed, in bstd (2026-11-20)

The owner, 2026-11-20 20:04: "bstd maybe should have units in there too ... idk if that is like
const generics". Yes: `blaze-units`, re-exported by bstd. A number with a meaning is a type that
says so (rule 76). Two things make a quantity, and both are in the type:
- its DIMENSION (length, time, mass, angle, bytes, a count): `Length * Length` is an `Area`,
  `Length / Time` a `Speed`, and adding a length to an angle does not compile;
- its KIND, where one dimension has meanings that must not mix: CPU time and wall time are both
  seconds; bytes allocated and bytes resident are both bytes; a count of tasks is not a count of
  pages. (The coordinator misread the profile three times on 2026-11-20 by mixing exactly these.)
A quantity is stored in ONE base unit (metres, seconds, kilograms, radians, bytes), so there is
no unit conversion bug inside a program; units exist only at the edges, in named constructors
and accessors (`Length::mm(2.0)`, `.as_mm()`), and in printing, which picks the unit for people.
- **Const generics, later.** The general form is one type `Quantity<const D: Dim, K>` whose
  products compute the dimension in the type. That needs arithmetic on const generics, which the
  pinned nightly has only as an incomplete feature; while cargo also builds blaze, the crate is a
  closed table of named quantities and their products, written so each name is an alias that the
  general form can stand behind without changing a caller. When blaze builds blaze, the table
  becomes the general type (rule 55: own the language).
- **Users from the first day**: the CAD crates (lengths, angles, masses, D183), and the meter
  and telemetry (`Nanos` by kind, `Bytes` by kind, counts by what is counted, D178), whose
  printed lines then cannot put CPU seconds under a wall-time heading.
- Integer and float carriers are separate types (`Nanos` is a `u64`; a `Length` is an `f64`); no
  implicit conversion between them.

## D185. No kernel floor; the best interface each kernel has, chosen once from probed facts (2026-11-20)

The owner, 2026-11-20 20:27: "no minimum kernel version I agree, but it should be the most
optimized API for each kernel version". Both hold together by one mechanism, which is the
design's own ("decide at run time: a version swapped into a slot"):
- **`KernelFacts`**: a value made ONCE at start by probing what this kernel offers, never by
  parsing a version string (distributions backport): each fact is the answer of one cheap call
  made for the purpose (the ring's own probe of its operations; a system call that reports "not
  implemented" for a number; a feature flag the kernel exposes). It is handed down like the clock
  and the allocator (rule 75), and a child blaze starts is handed its parent's.
- **A slot per operation with several forms**, each a strategy at one seam (rule 78) and each
  held equal by the same conformance tests: waiting on many things (`futex_waitv` where present,
  else one futex and a wake word), closing a range of descriptors (`close_range`, else a walk),
  starting a process (`clone3` with a pidfd, else `clone`), file facts (`statx`, else `fstatat`),
  opening beneath a root (`openat2` with resolve flags, else the checked walk), ring operations
  by what the probe lists (linked timeouts, direct descriptors, multishot accept and receive,
  zero-copy send, the NVMe passthrough command), atomic writes (D182), Landlock by its level.
- **This is not a fallback** (rule 3): nothing is tried and then quietly tried another way. The
  form is chosen from a stated fact before any work, the choice is printed in the profile
  (`kernel: ring ops N of M, waitv yes, clone3 yes, openat2 no ...`), and an operation that has NO
  form on this kernel is a typed refusal naming the missing fact.
- **The floor is whatever the oldest form needs**, stated in one table with the kernel that
  introduced each better form, so "what would a newer kernel give this machine" is a line the
  profile can print. The same on macOS by its own facts (the OS version and symbol presence,
  rule 45).

## D186. PROPOSED: one way to make, key, store and read a result (2026-11-20; under outside review, rule 66)

The owner, 2026-11-20 20:43: "redesign for optimal lack of bugs from construction and for optimal
IO, mem and CPU utilization, speed". The mechanism that took a second, third and fourth fix today
is one mechanism: how the command makes a result, keys it, stores it and reads it back. Counted on
the verification tree: the command crate has 33 `engine_*.rs` files (31,500 lines) with 44 put
sites, 33 get sites, 15 hand-written key domains and 34 record format constants; each is a place
where a person chose what goes in a key, what is read, when it is checked and how it is laid out.
Today's bugs are those choices made wrong, one at a time:

| bug found on 2026-11-20 | the choice that was free to be wrong |
|---|---|
| two values under one key (typed body record) | a key built by hand from some of the inputs |
| a failed fetch replayed as an answer | "done" and "failed for a reason outside" stored alike |
| every page hashed on every command (24% of an edit) | the reader decides whether to verify |
| 1,368 pages read for one function; all 128 header shards still | code may loop over every name |
| grains rebuilt from 624 crates' rows each command | a derived value with no home, so remade |
| a new folder never made durable; 17 store tests red | each write site chooses its syncs |
| a format-4 store stops a format-5 binary | a format is a constant, not a place |
| half a megabyte of locals in one function's frame | results passed and held by value |
| meter numbers wrapped across a switch | two snapshots subtracted without saying whose |

**The redesign: remove the choices.**
1. **A result is made only by a `Task`**: `fn run(input: &In, ask: &mut Asks) -> Out`, sans-IO. Its
   key is DERIVED from `In` by the engine (every field, by reflection: rule 56), so no key is
   written by hand and none can miss an input. Whatever else it needs it asks for through `Asks`,
   which is how the read list is made; it has no other way to reach anything, so no read can be
   unrecorded. `Out` is a typed record whose key is its bytes. The 15 domains, 44 puts and 33 gets
   become task definitions; nothing in the command crate calls the store.
2. **Reads are by need, and iteration is not offered.** A task asks for one keyed thing and gets
   it or parks (D161). The view has no "all rows" or "all names" call outside the planner, so the
   whole-workspace loops cannot be written. A value derived from many rows is itself a task, so it
   has a key, a read list and a home.
3. **An outcome is a type**: `Made(out)`, `Stood` (the proof it took), `Refused(finding)`, or
   `Owed(why)` for a cause outside the checkout. Only the first three can be certified.
4. **Verified once, in the type** (rule 74): bytes come from the store as `Proved<'a>` or not at
   all; a decoder takes `Proved`; the proof is the segment's seal checked once a boot.
5. **One layout, made for the edit**: fixed-width rows in columns, read in place from a mapping,
   never decoded into owned values; rows that are read together are stored together (by crate,
   then file, then item), so an edit's reads are one page run; the index from key to row is a
   persistent hash table of fixed slots, updated by writing only the changed buckets, so a lookup
   touches one page and an edit rewrites a handful. No shards read whole.
6. **One commit point**: a command's writes are appended unsynced and become visible at one head
   flip, group-committed; every earlier sync is deleted. After a crash the head is the old one or
   the new one. The layout under it is the bottom seam of D182.
7. **A format is a place** (D160), and a schema's digest is in every key derived under it, so two
   formats cannot meet.
8. **Nothing large moves by value**: stage state lives in the command's frame, stages take `&mut`
   to it; a lint bounds a function's frame; a test runs a whole check on a small stated stack.
9. **Every worker busy, nothing shared but what is published** (D179); hashing in batches;
   quantities typed (D184); telemetry by default (D178) prints, for each run, tasks made and
   stood, pages touched, bytes hashed, and the floor of each.

**The floor this is aimed at** (rule 67), for a one-function edit on tempo: pages read: the
edited function's read list and its index buckets (tens); bytes hashed: the changed file and the
new records (kilobytes); syncs: one group commit; tasks entered: one body, one lint unit; time:
under 100 ms. Today: 187 pages (108 MB), 4 scripts asked, 0.6 to 1.2 s.

**How it is built** (rule 79: interfaces first): one small crate of the `Task`, `Asks`, `Outcome`
and row-layout types with a simulated store; the engine's driver already has this shape (D157);
then the command crate's stages are ported onto it a stage at a time, each port deleting its
`engine_*.rs` files, each held equal to the old stage by running both on tempo.

## D187. The outsider's verdict on D186: rebuild narrower, on the seam that exists (2026-11-20)

Rule 66's outside review (a fresh reader of the code and of D186) answered: **rebuild the command
crate's stage code, on the engine seam already in the tree; do not write a new task crate; do
not replace the page layout with a hash index; nothing lands on the store until its crash
simulation is green.** The coordinator accepts it; D186 stands corrected as follows.
- **The seam exists and nothing uses it.** `TaskKind::run(cx, src: &Recorded, task, out) -> Ran`
  (blaze-engine kinds.rs), its recorded reads, the typed outcome and the `Ready` driver are D157,
  unfinished: the command still drives the old phase engine. D186's point 1 would have been a
  third driver contract (rule 30). The work is the PORT of each stage onto it.
- **The four whole-workspace loops are the planner**, not the layout: a planner that plans the
  whole workspace on every command. It is replaced by the forward wake (D168): outline the
  edited file, compare its cells, wake the readers of moved cells, recheck only those tasks'
  read lists. The 108 MB read is `pages/headers.rs`, a second index of the same rows kept so the
  planner can sweep; it goes with the planner. The answer pages themselves are right.
- **What carries validity is the read list, at cell grain.** A key over the input names the
  question; the answer depends on what was reached through asks. Today's read lists are widened
  after the fact to whole records; the real port is serving the checker's world through
  `Recorded` at cell grain. The check is forward only (woken tasks times their reads), never a
  sweep of every read.
- **The producer's code identity is in every derived key** (D162), with the schema.
- **Whole-set views that are truly needed** (impls of a trait across the graph, the exit status,
  the printed answer, dependency-head roots, reachability for cleanup, a cold plan) each become
  a derived kind with its own key, updated by delta. "Iteration only in the planner" was the
  wrong exemption: the planner is where the loops lived.
- **No in-place hash index** (it breaks written-once, deterministic bytes and needs its own
  crash protocol). Still open in both designs: the reader list of a hot cell (a lang item read
  by a hundred thousand bodies) is one entry rewritten on every change of its readers: it needs
  postings kept out of line with appended deltas.
- **One commit a command on the edit path**, by separating "mapped in this process" from
  "durable"; a cold build keeps checkpoints; two commands on one store merge at the flip. The
  "visible but unsynced" state enters the crash model first.
- **The proof's grain**: once a boot per segment means a reboot hashes whole segments (one was
  22 GiB): per-block checksums verified on first touch, or bounded segments.
- **128 cores**: `Ready` merges on one coordinator; with 30 µs tasks that is millions of merges
  a second: merges are batched a worker and the ready set is sharded.
- Sound as written: the outcome type (rename `Fault` to `Owed(cause)`), proved bytes, a format
  as a place, nothing large by value (the frame lint and small-stack test now).
- **First step, one day, on tempo**: an edit-path DRY RUN over today's pages with no checking:
  outline the edited file, compare cells, look up readers of moved cells, form the woken set,
  check each woken task's read list, count. It must show: woken = one body and its lint unit;
  at most about 50 answer pages; 0 header shards; 10 ms or less warm. If it does, the 0.6 to
  1.2 s is the driver, and Body is ported onto `Ready` next under a 100 ms bar. If it touches
  hundreds of pages, what it names (hot reader lists, scattered reads) decides the layout change.
- **Its stated risk**: while two drivers coexist each bug has two homes; and if the checker's
  world cannot be served at cell grain, read lists stay coarse and the floor stays out of reach.

## D188. What 44 scoreboards say: the utilization budget, and the changes the data asks for (2026-11-20)

The owner, 2026-11-20 21:03: "redesign for optimal IO, mem and CPU utilization based on all our
historical data". The data: every tempo scoreboard on record (reports/score-*, 18 to 20 Nov; the
table is `tools-1119/s/tempo-history.py`), tonight's function-level sample, and the profile's own
accounting. What it says, measure by measure:

**CPU.** A first check uses 35 to 45% of 16 cores (about 1,200 CPU-seconds in 190 s). The Bodies
stage is full (55 s wall, about 1,000 CPU-s); the Names stage runs on TWO cores (83 s wall, 166
CPU-s) and its own profile says why: 95.5% of its wall is "a task whose producers had ended, not
yet admitted", against a critical path of 34.9 s. And Names cost a third of this until one
nine-line change on 18 Nov 22:36 (macro modules built with debug assertions: 21 s and 50 CPU-s
before, 78 s and 153 CPU-s after). On an edit one core works for a second to check one function
for 36 ms.
**Memory.** Peak footprint is 27 to 34 GB on every run, whatever was checked: it does not follow
the work. Tonight: regions 17 GB at the end of Bodies and never given back, frames 4.9 GB, heap
4.9 GB; one crate's names task holds 2.3 GB of scratch; 668 GB of heap is allocated and freed in
one check outside any span; 10.7 million soft page faults (171 GB of pages touched once).
**IO.** No block IO at all (everything is in the page cache): the cost of IO is system calls and
copies. An edit reads 65 MB through 172 pages; a first check copies 1.0 GB of names records at
put; the stored answer is 2.8 GB in 30,000 pages.
**Correctness, as a series.** Refused by the borrow check: about 2,000 on every build until 19
Nov 13:26, then 11,055, now 6,162, with a cause that did not exist before. Nobody saw it.

**The budget** (rule 67: stated so it can be missed), for tempo on 16 cores:
| | today | budget | from |
|---|---|---|---|
| first check, wall | 190 s | 60 s, then 35 s | Names at its critical path; then the path itself shortened |
| cores busy, first check | 6 of 16 | 14 of 16 | one ready driver, no rounds, no admission wait |
| peak footprint | 34 GB | 8 GB | a fixed frame a worker, returned; records read in place |
| heap allocated | 668 GB | under 20 GB | per-task frames; no allocation where a borrow does (rule 11) |
| edit, wall | 1.0 to 1.2 s | 100 ms | D187 |
| edit, bytes read | 65 MB | under 3 MB | D187 |

**What changes** (each is the data's own finding, in the order of what it buys):
1. **The Names regression is undone first**: the macro's crates keep debug assertions; the
   standard library under them does not; a module's compiled native code is a record a machine
   keeps. Back to about 21 s and 50 CPU-s. (With a writer.)
2. **One ready driver for every stage** (D187): a task runs the moment its producers end. For
   Names alone that is 83 s to the 35 s critical path; it is also the edit path's fix. Then the
   path itself: one crate's names are one task of up to 7 s (`coins_ledger`, `zerocopy`): item
   grain (a file's or a module's names a task) is the design's own rule and is not true here.
3. **Memory follows the workers, not the project**: each worker has one frame of a stated size,
   reset per task and returned at a stage's end (the regions line must fall, not only rise);
   a task that needs more than its frame says so by name; expansion scratch for one crate is
   bounded by streaming expansions to records. The profile already prints "kept past the phase":
   it becomes a gate.
4. **No copy on the way to the store**: a record is built in the bytes it will be stored in
   (1.0 GB "copied" at put in Names becomes 0); read in place.
5. **The borrow-check regression is found from its two refs** and undone. (With a writer.)
6. **The history is read by a tool every time**: every scoreboard ends with its numbers against
   the best on record and the word REGRESSED where it is worse (done: `tempo-history.py`; tonight
   it prints eleven). A regression is then seen in the run that makes it.

## D189. The outsider's verdict on block modules: rebuild; block scopes are the body's own (2026-11-21)

Rule 66 was reached: three fixes in a day to one mechanism (crate-level name resolution renders
each function-body block that holds items or macro calls as a synthetic module). The second made
nearly every body's blocks modules (Names on tempo: 54.7 s CPU to 81.4 s; Heads 126 s to 153 s) to
serve about a hundred bodies; the third (follow a dependency macro's `$crate` callees) cut
expansions, not modules. An outside reviewer read the code and ruled: REBUILD.

The contract:
1. The scanner keeps, per body (keyed by its text), the block tree, item statements and every
   macro call with its `#[cfg]`, plus one bit `surface_by_text`: an `impl` in item position, an
   item with an attribute outside a fixed inert list (a derive, an attribute macro,
   `macro_export`, `no_mangle`, `export_name`, `lang`, `panic_handler`, `global_allocator`,
   `used`), or an extern block.
2. Names solves module-level items only. No body text goes in. A literal `#[macro_export]
   macro_rules!` in a body is lifted to the crate root by the scanner; an exported macro that comes
   out of an expansion inside a body is a typed refusal naming it.
3. One record per macro DEFINITION, `MacroEffect { emits_surface, binds_at_top }`, keyed by the
   definition, computed by following each `$crate` callee in its crate's finished names (this
   crate's too: the new order makes that possible). A proc macro is `emits_surface`.
4. A surface pass between Names and Heads, a record per file keyed by the file's text and its
   names read list: a body is a surface body if its bit is set, a call's definition emits surface,
   a call's input holds an item word, an attribute or an unproven nested call, or a call binds at
   top.
5. A surface body gets one `BodyScope(body)` task: it expands the body's calls and solves its block
   tree with the EXISTING solver over the body's own tree, module names fixed outside; it writes
   the impl rows and the heads of the local items those impls reach, keyed by (body, ordinal). The
   body's own check reuses that record.
6. Every other body builds its block scopes with the same `BodyScope` function inside its own
   lowering.
7. The floor for a body with no surface effect: no task and no record before Bodies; one name
   lookup and one record read per call. Names at or below the 22:55 profile of 20 Nov (23.5 s
   wall, 54.7 s CPU).

Why (the reviewer's count, by its own script): of tempo's 13,833 workspace bodies, 6,576 hold a
macro call and 18 have a block `impl`, derive or export in their text; of 185 thousand vendored
bodies, 1.3% are surface by text; about 2,300 serde derives' `const _: () = { .. impl .. }` blocks
become small `BodyScope` tasks. "Heads must be known before bodies" holds only for impls and what
they reach; "a text filter is unsound" holds for deciding by a macro's NAME, not once the call's
path is resolved to a definition. The real fault was the order (blocks rendered before macro names
are solved) and two homes for one concept (the body's lowering resolves block fns, consts and
uses; the names library resolves block structs, impls and mods; `scope_table.rs` bridges them).

It deletes: blaze-scope `blocks.rs` and `expanded_blocks.rs`; in `resolve.rs`: `make_blocks`, the
block-module branch of `expand_call` with `LITERAL_BUILTINS`, `block_places`, `MAX_BLOCK_DEPTH`;
the `is_block_module` cases in cells; `NestedItem::body_writes`, `Nested::writes`,
`makes_module`; the module-key derivation in front2 `scope_table.rs`, `WrittenBlock` in
`scoped.rs`, `LocalKind::unplaced`. The special cases of the second fix (extern-block lang rows,
the kept statement `#[cfg]`, a body `include!` planned as a data read) become ordinary work of the
body's lowering.

Proof before the old path is deleted: (1) a world test: an upstream body writes `impl Tr for X {
type A = Local; }` and a downstream body calls a method on `<X as Tr>::A`; (2) `use
dep::emits_impl as format; fn g() { format!(); }` is a surface body, with the planted bug of
skipping the `MacroEffect` lookup; (3) on tempo the impl table from the new path equals the
current one key for key.

## D190. The outsider's verdict on call relating: finish D156; the old call paths are frozen (2026-11-21)

Rule 66 was reached again on the borrow lowering's call relation: after D156 was decided (20 Nov
13:40) the old paths still took new paths ("closed call through the impl row", a `JoinedSuperset`
step). An outside reviewer read the code and ruled: REBUILD, by finishing the switch that is
half built. The contract crate (`crates/blaze-borrow` `inst.rs`, `relate.rs`), the seam
(`blaze-flow-lower/src/one_call.rs` `CallRule`: `OldPaths`, `OneRule`; `type Production`) and the
call-by-call oracle (`oracle.rs`) exist. Eleven relating paths are in production; each pairs two
DIFFERENT structures by counting (flow positions against column slots, an erased type against a
column laid by a second walk, typeck rows by ordinal). `Slot::Joined` cannot be made exact
without substituting the impl's value, which is the one rule.

Tempo at the verdict (02:20, c0220wave84): 14,995 checked, 1,513 refused (about 1,000 in this
mechanism), 564 distinct false findings (159 "must outlive").

What D156 gains from the verdict:
1. The type checker records only what only it knows: per call-like expression the callee as
   `FnDef(item, erased args)`, its adjustments, and written `'static`/owner lifetimes; ONE
   selection table per body from an erased projection or trait reference to its source
   (`Impl`, `ParamEnv`, `OpaqueBound`, `ObjectBound`, `Builtin`); each closure's signature, kind
   and upvars. It never records an instantiated signature or a region variable: the borrow
   lowering instantiates from the head (as rustc's MIR borrowck does).
2. Head facts the one rule and the old paths both lack: the variance of EVERY ADT parameter
   (type parameters are taken invariant today: `Option<&'static str>` into `Option<&'a str>`
   demands `'a: 'static`), ADT inferred outlives (RFC 2093), impl heads' where-clauses (never
   imposed today: unsound), opaque captures and item bounds.
3. A hole in the coarse rule: `unify` in binary operators, aggregates and inexact `relate`
   never records a coarse step, so its findings are reported.

Order (the tree compiles and tempo never gets worse):
1. `OldPaths` is frozen: it takes only honesty fixes (a stricter relation records its step).
2. A census mode: every tempo body lowered through `OldPaths` and `OneRule`, printing the
   oracle's counts and a histogram of `OneRuleWhy`. Production unchanged.
3. The grain behind the seam (positions for `OldPaths`, slots for `OneRule`); the closure,
   await, value and own-signature relations moved inside the seam.
4. The one rule's gaps closed in the census's order: the selection table, closure and future
   slots, variances and inferred outlives.
5. Flip and delete in one piece, on a census of that exact tree. Gate: at least 14,995 bodies
   checked, at most 564 findings, zero "fewer pairs" outside hub calls, rustc's `nll`,
   `regions`, `borrowck`, `lifetimes` fixtures pass with the planted `OneRuleDropOne`.
6. Regions inside head types as the second `RegionTypes`; identical constraints per call on
   tempo; then the D148 columns go.

The floor: no refusal from this mechanism but `HigherRanked`, and no false finding from a call.

## D193. What a day of real runs changed: six revisions, and the order (2026-11-21)

The owner, 2026-11-21 17:03: "given all of these revise the architecture and implement in parallel
what u think the best long term architecture is". Evidence: eleven real runs on tempo (check 0
errors; 215 of 454 swept tests; `run` stops in C) and the first three on turso (2801, 1827, 115
errors). Nothing above is withdrawn. These six follow from what stopped the runs, and each names
what it deletes.

1. COMPILE-TIME CODE RUNS AS BLAZE'S OWN CODE, NOW (D175 moves from "end state" to "next").
   Evidence: on turso the interim module builder (cargo and rustc to wasm, rule 1's largest open
   bug) failed 10 of 148 modules with four tool bugs, its keys hold the whole lock so two
   projects share almost nothing (5 of 148 reused from tempo), and bindgen scripts cannot be wasm
   at all. `blaze test` already runs std code natively from code records. A proc-macro crate is a
   program; its `proc_macro` library is blaze's (tokens in, tokens out, spans as item-relative
   values); a build script is a program whose system layer is the grants of D177. Deletes: the
   interim builder, the module stores, Wasmtime, blaze-wasi, the wasm standard library.
2. A PROBE IS ANSWERED BY THE THING IT ASKS ABOUT. A build script that runs `rustc` on a snippet
   is asking blaze: the snippet is checked by blaze's front end for the asked target and the
   answer is a record keyed by the snippet (autocfg, rustix, proc-macro2, thiserror). A script
   that runs `cc` is asking the pinned clang: it runs, hermetically (D177), and the answer is a
   record. Deletes: `pins/answers/*`, `CannotAnswerYet`, the harness that fills tables by hand.
3. ONE PINNED WORLD FOR NATIVE TOOLS (D192): the base tool set by digest, seen through hvbox's
   synthetic root (`/tools`, `/package`, `/out`, `/sdk`), `Level::Execution`. Seatbelt stays as
   `Level::Inputs` until the owner's first hvbox run passes its checklist. Deletes: `host_tools`.
4. NATIVE CODE OF A DEPENDENCY IS BUILT WHEN ITS FIRST SYMBOL IS BOUND, one image per links
   component (measured: building a test program's whole closure took tempo from 215 passing
   tests to none). The start image holds no deferred C.
5. NO ANSWER IS `None` ACROSS A CRATE BOUNDARY. Three of today's false errors were an unknown
   read as an absence (an impl that could not be filed; a glob import of a crate whose names
   failed; a constant's proc macro with no runner). D161 already says a task that lacks a fact
   waits; the guard is a type: a lookup that crosses a crate returns `Known(T)`, `Absent` or
   `Unknown(Cause)`, and only `Absent` may become an error. `Option` is banned at those seams.
6. THE EMITTER'S SCRATCH REGISTERS ARE TYPES. A flag register reused as an address scratch gave
   61 false overflow panics. In blaze-emit and in the fast tier a scratch register is a value
   taken from and returned to a small set, so two uses cannot overlap; every `test` run can be
   asked to compare a function's result with the LIR evaluator (`--differential`).

What `run` uses, stated (the owner asked): today the `Reference` tier, crates/blaze-emit: one
forward pass, every value in a stack cell, no optimization. Copy-and-patch stencils are parked
and their crate is deleted. The decided fast tier (D97 to D100, backend-next.md) is the
stencil idea with registers: one analysis pass, one emission pass with a greedy allocator,
composed from a table of templates that are data. CORRECTION (17:40, from the writer who read
the tree): it IS built (crates/blaze-emit `compose/analysis.rs`, `compose/emit.rs`, `table_gen/`,
`tier_tests.rs`) and is not the default; what was missing is typed scratch registers (now
`compose/scratch.rs`), far frames and their templates, and the switch. The switch happens when
the equality tests and every ladder rung pass at both tiers.

Order. In parallel now, disjoint files: (1) piece 0, the proc-macro program's interface crate and
its oracle against today's expansions; (2) the rustc probe; (3) the base tool set and hvbox's
root; (4) the component image. After tempo's ten packages are green: (5) as one port per seam;
D158's crate identity (a dependency bump re-keys every dependent item today: handoff e1), as
one port. (6) with the fast tier.

## Order

Now, in parallel (no file in common): the driver's contract in blaze-engine (D157); the borrow
contract crate with its rustc fixtures (D156); the choice of the names design (D158).
Then, each as it is ready: stages become kinds, smallest first; regions move into types and the
flow lowering is switched; the names switch with D152's key change (one port: both change every
`ItemKey`); the body record is wired and the builder switched (D159). Also now: the dead store
and crates deleted, and compaction's merge.

The tree is checkpointed (a round ref and the test round quoted) before each switch, and the
scoreboard is run at each: tempo's errors and unchecked items never worse than the checkpoint.

After the second audit, in this order, in parallel where files are disjoint: the store's place and
validated row (D160); `pins/` and `Home` (D162); findings as records (D164); typed controls and
drawn limits (D165); `Need` in the engine, then the checker's gate (D161); run reads the check
(D163 with D159); heads in place and regions in types with the call rule as one port (D156, D166);
the typed key and the numbers deleted (D162); the layers and the documents (D166).
