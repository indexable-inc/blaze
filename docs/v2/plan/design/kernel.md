---
status: plan
summary: blaze v2's kernel: registered pure compute steps from MIR down, each value kept in the store under its identity, its step's code and flags, with the narrow reads it made and their hashes, verified on lookup; codegen's dev templates are its first entries.
date: 2026-11-02
---
# The kernel: verifying traces from MIR down

Part 1 of the content-keyed incremental kernel. Lane pure checks that a step is pure, lane
identity gives the stable identities and code identities, and [verify](verify.md)
checks the store's hits. The code is `crates/blaze-kernel`.

## What it is

A compute step is a registered, pure function of its input. It reads the world only through
a recording context, `Reads`. Each read is narrow (the layout of `Foo`, not `Foo`), is named
by stable bytes (a kind and a key: content IDs and item keys, never a dense id or a path in
the checkout), and is recorded with the hash it saw. The value is kept in the
[store](store.md) as an entry:

    identity (step, of, code, flags) -> { reads: [(read, hash)], value }

A lookup re-hashes each recorded read now and takes the entry only when every hash is the
same. That is a verifying trace (Mokhov, Mitchell and Peyton Jones, "Build systems à la
carte", 2018).

- `Identity { step: StepId, of, code: CodeId, flags }`:
  - `of` is the stable identity of what is computed, from lane identity.
  - `code` is the step's code identity: its `compute` and everything that calls, also from
    lane identity.
  - `flags` is everything else outside the reads that changes the value (target, settings).
- Two values under one identity and set of reads are refused (`KernelError::Conflict`, rule
  18), and the first value stays. Either the identity misses an input or the step is not
  deterministic, so the fix is the key or the step, never the check.
- A run whose read failed keeps nothing, even if the step swallowed the error, because the
  context remembers the failure. So does a run in which one read saw two hashes
  (`KernelError::Unsteady`: the world changed under the step).
- An entry's reads are sorted by kind and key, each once, so read order, maps and threads
  never reach the key (rule 22).

## Beside blaze-query, under codegen

The front end keeps its engine. `blaze_query::db` is derived from salsa: dense ids, in
memory, one view at a time, rebuilt or restored with the workspace (lane warm-restart). The
kernel is for what is worth keeping across processes and checkouts, from MIR down:

- codegen of a function instance,
- later, release templates, summaries, statics, folded bodies, const eval and layouts.

A kernel step reads the front end through a `World` that its wiring crate implements:

- `World::hash(read)` answers a read by its stable key, e.g. a `blaze_items::Fact` resolved
  back to a dense id with `blaze_items::key::resolve`.
- `World::describe(read)` names the read for errors and checks.
- A typed `Fact` gives the value and its hash in one call. The kernel's contract is that
  `World::hash` equals the hash `Fact::get` gives.

The kernel does not depend on `blaze-query`. The crate root's content-keyed engine in
blaze-query (`Database`, `Query::PRODUCER`, memos verified per view) stays for blaze-units and
blaze-tree. Its persisted memos could later be filed as kernel entries.

## In the store

Every value is an ordinary blaze-cas value, so it is restored as the store is, with nothing
of its own to persist (rule 23).

The storage form is `blaze-record`'s (base key → read-lists → answer; since 2026-11-12, when
`entry.rs`'s own shape slots and entry keys moved onto it). The base key is `Identity::id`; a
read's name in a list is its kind (`u16`, big-endian) then its key's bytes.

| what | key | bytes |
|---|---|---|
| entry with no reads | `whole::key(identity)` = `AnswerKey(empty_list(id))` | the bare value |
| read-list in slot `n` | `blaze_record::list_key(id, generation 0, n)` | `blaze_record::write_list`: the base, no note, each read's name (no hashes) |
| entry with reads | `AnswerKey(the read-list's bytes, each read's digest)` | magic (layout 3; a layout-2 entry is refused by name), step, the read-list's length, the read-list, each read's digest (gone, or its hash), the hash of the value's bytes, the value at a 16-byte boundary |

- **Lookup** probes the entry with no reads first (one probe: a step whose identity is its
  whole content costs nothing more). Otherwise it walks the identity's read-lists from slot 0
  (`blaze_record::walk`). For each list it hashes the reads now and probes the entry those
  hashes name. An empty slot ends the walk as a miss. The chain is one generation of
  `MOST_SHAPES` (1024) slots, `Bounds::new(1024, 1)`: full is `KernelError::TooManyShapes`
  (rule 18), as before the move. The generational bound (a full chain stops searching its
  oldest lists and is never a refusal) changes what a lookup finds, so it is its own step.
- **Keep** puts the entry first (`blaze_record::keep_answer`: other bytes under its key are
  `KernelError::Conflict`), then makes sure its read-list is in the chain
  (`blaze_record::Filing::place`): a slot holding the same list ends the walk, and an empty
  slot is taken. A slot another writer took between the get and the put (a `Mismatch`) is
  read again and passed over for the next slot. The store must read its own puts at once,
  which blaze-cas does through `Store`, `Writer` (`blaze_kernel::Writing`) and `Reader`
  (`blaze_kernel::Reading`).
- `Entry::read` refuses an entry filed under a key its own identity and reads don't name: an
  entry holds its read-list as filed and each read's digest, so it re-derives its key in place.
- An entry with no reads is the bare value, so a million codegen templates pay no header.
  Read-lists exist only for steps that read, and a pure step that reads nothing for one input
  reads nothing for every world, so the two layouts never mix under one identity. Its key is
  the answer key of the list with no reads, which is known from the identity and never filed.
- The move changed every kernel key (the two domains `blaze v2 kernel entry 1` and `blaze v2
  kernel shape slot 1` are gone; the entry's layout number is 2): each entry kept before it
  misses once, dev templates included. An old entry is never under a key this blaze looks
  under; handed in by key (`Kernel::why`), it is named as layout 1, not read.

## Purity (lane pure)

`Step` and `Compute` carry `#[blaze::pure]`, so lane pure's check walks every method of every
impl. `Reads::get` and `Reads::hash` carry `#[blaze::pure(trusted = "the kernel records every
read ...")]`: they are the trusted door, and their bodies (the world) are not walked. A step
that reads anything outside `Reads` is refused by the lint, and in a build a sampled check
catches it (below). The trusted functions a step reaches (`PureProof::trusted`) belong in its
`CodeId`; lane identity hashes them in.

## Identity (lane identity)

The kernel takes `Identity::of` and `CodeId` as given. `Kernel::register::<S>(code)` records
a step's `CodeId` once per process, and `Kernel::identity::<S>(input)` puts it in. Until lane
identity gives code identities, codegen passes `Toolchain::id`: the pinned nightly and the
code generator's code identity, of the packages it is built from (`hir_ty_codegen::v2::code`,
`crates/blaze-codegen-code`), never the executable's bytes. The step's numbers live in one table, `blaze_kernel::steps`, and
registration refuses a second step of one number or one name.

## Checks (lane verify)

On a hit that `BLAZE_VERIFY_STORE` samples, `Kernel::get` runs the step again with fresh
reads. It implements `blaze_store_check::Recompute`. The second run must make the entry's reads
and its bytes, and either difference is a `KernelError::Verify` naming the step, the input,
the identity and each read with its hash. Sampling is by `Identity::id`.

## The first step: codegen's dev template

`blaze_codegen::kernel::DevTemplate` is the kernel step of a function instance's dev
template:

- `of` is what the driver keeps the instance's code as: `placed(stored(instance key,
  built_in), refs)`.
- `flags` are its `Settings`, and `code` is `Toolchain::id`.
- Its entry has no reads, so it lives under `whole::key`, and its bytes are the template's
  blob as before.

Every dev template the driver reads or puts goes through `blaze_codegen::kernel::template`:
`driver::dev`, the lazy path in `driver/later.rs` (its own tier and other tiers), and
`blaze_global`'s rule for public dev functions. `query::keys::template` is gone. Existing
stores miss their dev templates once.

The driver still compiles templates in batches (the memo, flights, folding), so
`DevTemplate` is a `Step` the kernel keys, not a `Compute` it runs. Next, in order:

1. Done at package grain (2026-11-02): `Settings` has no executable hash left;
   `Toolchain::id` is the code generator's code identity, so a blaze rebuilt without changes
   to the code generator's packages reads the last one's templates. Next, lane identity gives
   `DevTemplate` the `CodeId` of the codegen code it runs, finer than a package.
2. The instance key's `args` (the facts of the type arguments the code reads) become recorded
   reads of a `World` over `blaze_items` facts, and `of` becomes lane identity's identity of
   the generic. One entry then serves every instantiation whose facts hash alike.
3. Release templates, summaries, statics, folded bodies and thunks become steps.

## Found on the way

- The "store: N functions found, 0 compiled and added" line is the pinned v1 engine's: its
  LLVM per-function cache counts hits and misses, and v1 stores no checkout code once a patch
  base exists (diagnosed by lane warm-restart). The v2 driver puts every compiled template.
- `driver::Out::finish` deduped its puts by key, so a second put of other bytes under one
  key in a build was dropped before the store saw it. It now refuses that pair (rule 18),
  naming both puts (`put_twice`, driver/tests.rs).
- `blaze_global`'s rule for public dev functions keyed a function by `stored(instance,
  built_in)` without `placed(.., refs)`. Since `placed` always rehashes, no key it marked
  public was ever one the driver keeps. `DevFunctionMade` now carries `refs`, and the rule
  keys by `blaze_codegen::kernel::template` of `placed(..)`, as the driver does.

## The kernel as the home (2026-11-12)

`end-state.md`, "The kernel is the home of all of this": `blaze-record` is the kernel's storage
layer and `blaze-kernel` is the API. `entry.rs` is on `blaze-record` (above, "In the store").
This section says what else in the tree is the kernel's form without being a kernel step, what
the API lacks before a front-end step can be one, and what a lookup costs. Read from the code on
int-1112 at 5bf3ba45d9; nothing here was measured.

### (a) The same form, not yet steps

For each: is its read a kernel read (a kind, a stable key, a hash)? Is its producer named by a
code identity (rule 52) or by a hand-numbered string? Does it read outside a recording context?

| place | its read | its producer | reads outside a recording context | between it and a step |
|---|---|---|---|---|
| per-function store (`hir-ty/src/body_store`; facts from `blaze-item-hash`) | Yes: a `FactRef` (fact kind, subject id, object), 68 stable bytes, valued by `BodyFacts::value`. The value is "as `def`'s crate sees it", so the world is per crate. | Half: `solver_version(build)` is the front end's code identity at package grain (one version for every step); beside it the domain `item store 1 body` and schema hashes. No step is registered. | Yes. The step runs on a whole `&Db`; the engine's dependency list is classified afterwards by ingredient debug name (`blaze_item_hash::reads::RULES`: covered, a fact, refused). Completeness is that table's audit, not the context. | `BodyFacts` is a `World` by another name (`identity` is `Identity::of`, `value` is `World::hash`) plus `classify`, which `Reads` makes unnecessary. Its base key hashes the body's content (its HIR, what its paths resolve to); a kernel identity is `of` plus reads. Needs: a step kept with a trace made elsewhere (below), buffered keeps (it flushes per thread), `None` from `value` as a typed outcome. |
| goal store (`hir-ty/src/next_solver/goal_store`) | Yes: the same `FactRef`. | The same `solver_version`; the goal's stable form is hashed under `goal 1`. | Yes, the same way: `blaze_query::db::Reads` of the evaluation, each an interned value or a fact, else the goal is left out (`Flushed::inexpressible`). | The same as the per-function store. Its base is content too (the goal's whole stable form), which is right here: a goal has no identity but its content. |
| build-script index (`blaze-usercode/src/index.rs`) | In shape yes (a file's hash or nothing, a listing's hash, a variable's value), but keyed by absolute paths, which a kernel read must not hold; `depends_key` (a veto of a hit across checkouts) exists because of it. Needs the shared page's `Ask`, named from a root. | `RunKey.code` is the content identity of the program run: rule 52's form for user code. The trace's rule is the hand-numbered domains `run traced 2`, `run shape slot 1`. | Yes: the run is a process, and what the sandbox cannot trace is listed in the shape (`Untraceable`), not refused. | Not on `blaze-record` yet (its own 4096-slot chain; `/tmp/primitive/next.md` lists what a port needs: only `walk`). It can never be a `Compute` (the kernel cannot run it): a `Step` kept with a trace made elsewhere. |
| image read slots (`hir-def/src/deps/{reads,filed}.rs`) | Not at this grain: one read per list, named by the whole encoded `Reads` (variables, files, listings, a place in the closure) and valued all at once. Each name inside is an `Ask`. | Hand-numbered: `dependency image key 7`, `dependency image reads 4`. Whether `deps::key` also holds the front end's code identity was not read. | Yes: names come from what expansions and `env!` logged while the crate resolved; an expansion that could not be traced refuses the image. | On `blaze-record` (`Bounds::new(16, 1)`). Crate grain, and the base key is the crate's content. To be a step: one read per name, and the logs replaced by the context. |
| unit read slots (`blaze-cargo/src/serve/unit/reads.rs`) | As the image slots: variables by name, files by absolute path, expansions by their names. | Hand-numbered: `cargo unit record reads 4` and `VERSION = 4`. | Yes: "the names come from the database's logs" (the `env!` log, the expansion log), gathered after the unit compiled. | Not on `blaze-record` (its own `SLOTS` loops: the seventh copy). The same two changes as the image slots, after its port. |

Three things recur, and each is one piece of work for all five, not five:

1. **Reads gathered after the fact** (an engine's dependency list classified by name, a
   database's logs, a tracer's report) instead of made through the context. `Reads` is the only
   form in which a missing read cannot exist.
2. **Content in the base key.** A body's identity and a crate's key hash content, so an edit
   moves the base and the old read-lists are never searched again. The kernel's `of` is an
   identity (a path and a kind); content enters as reads (shared page, point 3).
3. **A producer named by hand.** A numbered domain or `VERSION` is bumped by whoever
   remembers. Registration with a `CodeId` is the one place a producer's identity is given.

### (b) What the API lacks for front-end steps

- **A step kept with a trace made elsewhere.** `keep_whole` keeps a value computed elsewhere
  only when it has no reads. Every place in the table computes outside the kernel and has
  reads. Needed: a `Trace` a wiring may build read by read (kind, key, seen), and
  `Kernel::keep::<S>(store, input, trace, value)` beside `Kernel::find`. It is the door for
  what the kernel cannot run (a process); for what it can run, it is an interim that keeps
  point 1 above open, and is listed as such per step.
  **Built**: `Trace::new`, `Trace::read` (`reads.rs`; one read with two hashes is `TwoSeen`),
  `Kernel::keep`, and `Kernel::find` for any `Step` (`lib.rs`). `Kernel<W>` is now of one
  world type. Tests (`tests.rs`): `a_step_kept_with_a_trace_made_elsewhere_is_found_while_its_reads_hold`,
  `two_values_under_one_identity_and_trace_made_elsewhere_are_refused`,
  `a_trace_made_elsewhere_keeps_the_entry_a_run_keeps`.
- **Nesting.** `Reads` does not nest: `Reads::new` is the kernel's, a `compute` holds no
  kernel and no store, so a step cannot ask for another step's value. A front-end step's reads
  are found while it runs, through other steps (inference asks for signatures, which ask for
  name resolution). Needed: `Reads::step::<S>(input) -> value`, which looks the child up or
  runs it with reads of its own and records one read in the parent.
  **Built**: `Reads::step`, trait `Ask` (an input's stable bytes), `Kernel::register_ask`,
  the engine in `nest.rs` (a cycle is `KernelError::Cycle` naming its identities; no
  fixpoint; depth bounded by `MOST_NESTED`). Tests: `exhaustive/nested.rs`
  (`a_cycle_is_refused_naming_its_identities`, `a_stale_list_that_asks_back_is_a_miss_not_a_cycle`).
- **Early cutoff.** That one read is the shared page's `Record` read: kind "a step's value",
  key the child's identity, seen the hash of the child's value bytes. A child recomputed to
  the same bytes then leaves the parent a hit. Verifying it at lookup needs the child's value
  hash now, so the kernel, not the `World`, answers reads of that kind, and a step must be
  runnable from its read's key: an input that decodes from stable bytes, and a registry entry
  holding the erased `compute`. Today `Step::Input` is any type and the identity is one-way.
  **Built**: kind `ReadKind::STEP`, key the child's step number and input bytes (not its code:
  the registry gives the code now), seen `content_of` the value. A read-list holds names, not
  the hashes an entry saw, so a lookup values every step read its lists name; a child that
  cannot be valued then makes the list not hold. **Built** (cost): one walk, in which a child
  is looked up through the store the walk reads and remembered for the request
  (`nest::Looker`, `Engine::lookup`); only a child that must be run is run after the walk,
  and the lists then walked once more. The entry keeps its value's hash (layout 3,
  `Entry::value_hash`), so a hit of a child with reads hashes and copies nothing. A hit's
  cost per shape is pinned in `tests/cost.rs` (probes, gets, reads valued): no reads 1, 1, 0;
  R reads 2, 2, R; each child with reads 3 gets and its reads, once a request however many
  parents name it; a child with no reads 1 get and one hash of its bare value. Still paid:
  the answer's probe before its get at the top (the borrow of the store), `Entry::read`
  deriving the entry's key again (one hash of its list and digests), and a read shared by
  two lists of one identity valued once a list. Not built: stopping at the first changed
  read (the record form holds names only). Test:
  `every_nested_sequence_in_scope_gives_what_a_fresh_run_gives` (20,736 sequences, counts early
  cutoffs), controls `a_parent_that_records_its_childs_reads_but_one_is_caught`,
  `a_cutoff_by_the_childs_identity_alone_is_caught`.
- **A session.** With nesting, one build asks for a child once per parent. What this process
  verified (identity → value hash, under one pinned world) is a table of the lookup's own,
  dropped at will without changing an answer. The kernel has none; `World` is not pinned.
  **Built**: `Session` (`nest.rs`), `Kernel::get_in`/`find_in`, required `World::pin`; a
  world not at the session's pin, before or after a request, is `KernelError::Moved` (after:
  the rows are dropped and the answer is not given). `Kernel::get` is a session of one
  request. In a session a value found at the top before is one get, and a child is verified
  once over all requests (`tests/cost.rs`). Test: the nested exhaustive check runs every
  sequence in one session with a drop at every place, control
  `a_session_that_outlives_its_pin_is_caught`. The session is now in `session.rs`, is of one
  registry of steps as well as one pin (`Kernel::registry`, `KernelError::Reregistered`), and
  is carried from pin to pin: "Carrying a session" below.
- **Negative reads.** Present for a key: `Seen::Gone` (`Digest::Nothing`), never equal to a
  hash. Absence over a set (no impl for a type, no such name in a scope) is a read of the set
  under its own kind, whose hash covers the set. Lacking: a read that cannot be valued now is
  an error of the whole lookup (`KernelError::World`), where the hir-ty stores pass over that
  list; a front-end world needs that as an outcome of `World::hash`, not an error.
  **Built**: `WorldError::not_now` (an outcome of `World::hash`, documented on `World` with
  the other three and with absence over a set): a lookup passes over the list that names
  the read and goes on; a run that asks for it fails and keeps nothing. Test:
  `a_read_that_cannot_be_valued_now_passes_over_its_list`.
- **A covering digest.** No new rule in the lookup: a cover is a step. Its reads are only
  covering reads (a crate's digest, a directory's Merkle id), its value is the manifest of its
  members' entry keys, and a member is fetched by key (`Entry::read` checks it is that
  identity's). What the API needs is the proof that the cover covers: `World::cover(read) ->
  Option<Read>` (the covering read a read is under), and at keep time every read of every
  member must be under one of the cover's reads, else the keep is refused naming the read.
  It sits in nesting: a child run "under cover" has its reads checked against the parent's
  covering reads instead of recorded as the parent's. Nothing else may stand in front of a
  lookup. **Built** as an operation of the kernel, not a run: `Kernel::register_cover`,
  `Kernel::member` (a member verified in the session: its entry is read for its key, value
  hash and reads), `Kernel::cover` (values the covering reads, checks every member's read:
  a world read under a covering read by `World::cover`, at most 8 steps up; a step's value
  only of a member; else `KernelError::Uncovered` naming member and read; keeps the manifest,
  `blaze kernel c 1`), `Kernel::seed` (the cover's ordinary lookup, then each member admitted
  from the manifest). A covering read is a world read: an interface's digest is the hash of
  the interface step's value as the world gives it (the test's digest ignores each fact's
  low bit, which no value shows). Covers are read only by `seed` (a cold start, another
  checkout); a carry never looks one up. Cost (`tests/cost.rs`): a seed through a cover of
  17 members values 1 read (the floor) and no member's; its gets are the cover's own (its
  list and entry; printed, to pin); each member is then one get. Not built: a covering read
  that is a read of a step's value through the kernel; a cover over members of another
  cover; the way "seed through a cover" in the exhaustive check (`exhaustive/nested.rs`:
  the control is a direct one in `tests/cost.rs`).

### Carrying a session

Built (`session.rs`, `nest::carry`, `Kernel::carry`); the design is docs/v2/plan/design/end-state.md,
"The bar: within 10% of the floor", item 1. A session keeps, beside its rows (identity → entry
key, value hash, height), **postings**: a read's name → the rows whose entry's list names it. A
read of a step's value is posted under its name like any other, and that name says which row is
the step's own, so a row's readers are the postings of its name.

`Kernel::carry(session, store, world, changed, verify)` takes the session from its pin to the
world's. `Changed` is what the driver says differs between the two pins, total by kind of read:
the exact names, or "any read of this kind". The carry drops and queues the rows a changed name
reaches, then repairs the queue lowest height first: a row a step has read is looked up again by
the ordinary lookup (a stored entry is a hit, else it runs) and its value hash compared with the
one its readers saw; the same hash stops there, another queues its readers. A queued row no step
has read is only dropped, and looked up when next asked for. An entry the repair never reaches
is verified with no work: its next hit is one get.

Repair is eager, inside the carry. Lazy repair needs, for a top to know that something under it
is dirty, either a mark on every transitive reader plus each row's children in memory, or a walk
of every reader's lists; eager needs only the postings and each read row's own key.

| Invariant | Where it holds | Control (`exhaustive/nested.rs`) |
|---|---|---|
| I1 every row is indexed | `Session::admit` is the only place a row goes live and takes a `Posted`, which only `Session::post` makes; both are private to `session.rs` | `a_hit_admitted_without_postings_is_caught`, `a_posting_written_at_the_next_carry_is_caught` |
| I2 the seed is complete | `Changed` is total by kind (`CarryError::Unsaid` names a kind left out); a registry that differs drops the session; that the names are all of them is the driver's to prove | `an_absence_left_out_of_the_change_set_is_caught` (the bug is in the driver's model) |
| I3 children before parents | the queue pops the lowest; a queued row is dropped at once, so a lookup that asks for it repairs it first; a changed row queues its readers even if they were repaired | `a_parent_repaired_before_its_child_is_caught` |
| I4 valued at the pin | `carry` takes `&mut Session`; `Moved` before and after; a change set from another pin is `CarryError::From` | `a_session_that_outlives_its_pin_is_caught` |
| I5 covers post | `Session::cover` admits a member as a reader of the cover's row (`By::Cover`), and a carry drops a cover with every reader, never looking it up | `a_member_admitted_through_a_cover_is_dropped_with_it` (`tests/cost.rs`, `Bug::CoverUnposted`) |

The theorem is stated once, on `Kernel::carry` (`/// The invariant:`), and checked by
`every_nested_sequence_in_scope_gives_what_a_fresh_run_gives`: 16^4 sequences from an empty
store and from a store another session filled, the session carried at every moved get and at
any step, each get against a computation with no kernel.

Cost (`tests/cost.rs`, a session of 256 entries, each number pinned exactly): a carry that
reaches nothing reads nothing and values nothing; one changed fact whose entry comes out the same
is 5 gets and 2 reads valued; one that changes its entry costs the path to the top (a leaf, its
group of 16 and the whole of 15 groups: 45 gets, of which 31 are the runs reading their
children's values); every other entry is one get when asked for. The floor is the changed
entries and their direct readers rerun: each its run and one put. Over it, a rerun entry pays 5
gets: 2 for a lookup that can find an entry another session kept (its read-list, the entry its
reads name now), 1 for the end of its chain of lists, 2 in the keep (`blaze_record` reads the
entry's key before the put, and the list's slot again). The last three are owed; so is the
entry get of a queued row whose reads come to the key it had (2 gets where 1 would do, for
each entry a kind with no exact difference reaches). A row that had reads is never asked for
its entry with no reads (`Session::listed`).

**In the store** (`trie.rs`, `Session::save`, `Session::open`). The store is write-once and a
session is not a function of its checkout and pin (it depends on what was asked), so nothing of
it is keyed by either: rows and postings are entries of maps kept as 16-way tries on a key's
nibbles, each node a record under the hash of its own bytes (`blaze kernel t 2`; a subtree of at
most 64 entries is one leaf, so a node's bytes are a function of the entries under it). One map
holds the rows by identity (entry key, value hash, height, posted slots, the name its value is
read by); one map for each kind of read holds the postings by the hash of the name (the name,
its step's own row, the readers' identities in order). A root record (`blaze kernel s 2`: pin,
registry, live count, the maps' roots) names them, and its key is the one thing whoever holds
the session keeps: `save` gives it, `open` takes it.

- `save` writes the paths of the rows and postings that changed since the last save (a row
  admitted, filled, named or dropped; a posting that took a reader), children before parents,
  the root record last. A save that stops part way leaves nodes no root names; the old root is
  whole. Every node no change is under is shared with the root before.
- `open` reads the root record alone. A row is read when its identity is first looked for; a
  posting when a carry seeds from its name or asks for a changed row's readers; a reader is a
  row not yet read until the carry drops it. `Changed::everything` reads the kind's map whole.
  So the memory of an opened session is what its requests and carries reached, and a saved
  session can be dropped whenever nothing is running in it.
- A node a parent names that the store lacks is `KernelError::Store` naming the node, and the
  carry that met it drops the session whole (some rows were dropped, their readers not
  reached); a root or a node of another layout (`s 1`, `t 1`: leaves of 8, lengths of four
  bytes) is refused naming the layout. Neither is read as empty.

Disk and gets against their floors (rule 67; `tests/cost.rs`, the session of 256 entries):

- **Records.** A full save is 52 records, 0.2 an entry: three maps (rows, the facts' postings,
  the steps' postings) of a root and 16 leaves each, and the root record. In general a map of
  `n` keys is `n/64` to `n/4` leaves and a sixteenth as many branches. Layout 1 (leaves of 8)
  wrote 537: by the shape's arithmetic each map of about 250 keys was a root, 16 branches and
  a leaf for each of about 162 of 256 second nibbles (not counted map by map). A save after a
  carry that repaired 2 entries and dropped the top is 5 records (the rows' root, three
  leaves, the root record; 8 in layout 1): `1 + log16(n/64)` nodes a changed row, the floor of
  a map on a write-once store. What it trades: a changed row writes its leaf's other rows
  again, at most 63 of about 100 bytes.
- **Bytes.** Counts and lengths are one byte where they are under 128 (layout 1: four and
  eight). The saved session is 73,642 bytes, 287 an entry, beside the entries' own 68,615, 268
  an entry (their read-lists and entry records, 512 records: two an entry, where one record an
  entry with its list inline, for an identity of one list, would halve the store's overhead a
  record; that is `blaze-record`'s to build). So the session is 107% of the entries' bytes,
  not within a tenth, and not at its own floor (a row: its identity, entry key and value hash; a posting: its name and
  32 bytes a reader). Owed, each a change of layout: the 32-byte key of a posting in its leaf
  is the hash of the name beside it (about a fifth of the bytes: derive it when a leaf is
  written, find by name when it is read); a step's name is kept in its row and in its posting,
  and the posting names its own row by identity (33 bytes) where the row could be found from
  the name; readers are 32-byte identities where a row number kept with the session would be
  one to four bytes, which is what decides the bytes once entries have many reads.
- **Gets of an opened session.** A node is read from the store once in one carry or one
  request (`trie::Nodes`, emptied at its end), so rows and postings cost the nodes they are
  in, not a path each: a get of the top is 3 gets (its row's root and leaf, its entry), a
  carry of one fact that cuts off is 9 (its 5, and a root and a leaf for the posting and for
  the row), a carry of the path is its 26 and 17 nodes that hold the 21 rows and postings it
  reaches (43 gets; 89 in layout 1: 3 nodes for each of the 21). The floor is one get for each record
  that holds something needed; over it, each map's root is one get more a carry.

Tests (`tests/cost.rs`): a session saved and opened carries with the same `Carried` and the
same bytes as one never saved; a save after a carry that repaired 2 of 256 entries writes 5
records; an opened session's get of the top reads its row's path and its entry; a carry of
one fact reads that fact's posting and the rows it reaches; a count in a record has one form.
`exhaustive/nested.rs` has the
way "save, drop, open" at every step (16^4 sequences a start) and the control
`a_session_opened_without_its_postings_is_caught`.

Owed: a deterministic simulation of the driver's root swap (a crash at each put of a save,
then `open` of the old root) once a driver holds a root, in `blaze-storemodel`; the save is
not in that model yet. Not built: compaction of postings left by rows that were dropped,
heights raised when a child grows taller without its value changing (such a parent may be
repaired twice in a later carry; never wrongly).

### (c) What a lookup costs

For an identity with `S` read-lists, the hit in list `j` (lists are searched in order), `r_i`
reads in list `i`, `L_i` its bytes (`48 + Σ(6 + key bytes)`), `V` the value's bytes:

    probes  = 1 (no-reads key) + j (list gets) + j (answer probes) + 1 (the entry's get)
    asks    = Σ_{i≤j} r_i                               calls of World::hash
    hashed  = Σ_{i≤j} (L_i [the get's check] + L_i + 33 r_i [the answer key])
              + (24 + L_j + 33 r_j + 8 + V) [the entry's check] + L_j + 33 r_j [Entry::read]
    T_hit   = probes · t_probe + asks · t_fact + hashed · t_byte
    T_miss  = the same with j = S and no entry get, then the step, then a keep:
              1 get + 1 put of the entry, up to S + 1 gets and 1 put of the list

A step with no reads is `1 probe + 1 get`, whatever the world (the dev template today). For a
front-end step `asks` dominates: `r` is the facts one body's inference read, and `t_fact` is a
fact's hash now through the engine or the pool. With one list per identity (the usual case
where content is not in the base key), `T_hit ≈ 3 t_probe + r · t_fact + (3 L + 66 r + V) · t_byte`.

Without a graph, an edit costs one lookup for every entry the build asks for, changed or not:

    T_edit = Σ_{crates not covered} Σ_{entries asked} T_hit  +  Σ_{entries that missed} (step + keep)
             + (covered crates) · T_hit(cover)

A crate's digest holds its dependencies' digests, so an edit uncovers the edited crate and
every dependent: there the cost is per entry, at item grain.

What must be measured on reth before the front end moves (each on a named commit, from spans):

1. `r` per entry by step (infer, MIR, borrow check, const, goal): mean, p99, max; and `S` per
   identity. Both stores already count what they flush.
2. `t_fact`: nanoseconds for a fact's hash now, as an engine memo hit and as a pinned record
   read through the pool (the shared page's point 11 is this number).
3. `t_probe`: nanoseconds for a `has` and for a get of a 200-byte record through the pool.
4. Entries asked per edit: for a body edit and a signature edit, the entries looked up in the
   edited crate and in its dependents, and how many of those miss.
5. The share of steps that cannot be an entry today, by the read that refuses them
   (`Flushed::refused`, `Flushed::inexpressible`).

The kernel goes as far up as `T_edit` stays inside the edit loop's budget with covers at crate
grain. If item-grain lookups in the dependents alone exceed it, the next cover is finer than a
crate (a module's digest, an item's interface digest), never a proof from a view.
