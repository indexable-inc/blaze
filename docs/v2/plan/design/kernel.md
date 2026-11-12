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
| entry with reads | `AnswerKey(the read-list's bytes, each read's digest)` | magic, step, the read-list's length, the read-list, each read's digest (gone, or its hash), the value at a 16-byte boundary |

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
- **Nesting.** `Reads` does not nest: `Reads::new` is the kernel's, a `compute` holds no
  kernel and no store, so a step cannot ask for another step's value. A front-end step's reads
  are found while it runs, through other steps (inference asks for signatures, which ask for
  name resolution). Needed: `Reads::step::<S>(input) -> value`, which looks the child up or
  runs it with reads of its own and records one read in the parent.
- **Early cutoff.** That one read is the shared page's `Record` read: kind "a step's value",
  key the child's identity, seen the hash of the child's value bytes. A child recomputed to
  the same bytes then leaves the parent a hit. Verifying it at lookup needs the child's value
  hash now, so the kernel, not the `World`, answers reads of that kind, and a step must be
  runnable from its read's key: an input that decodes from stable bytes, and a registry entry
  holding the erased `compute`. Today `Step::Input` is any type and the identity is one-way.
- **A session.** With nesting, one build asks for a child once per parent. What this process
  verified (identity → value hash, under one pinned world) is a table of the lookup's own,
  dropped at will without changing an answer. The kernel has none; `World` is not pinned.
- **Negative reads.** Present for a key: `Seen::Gone` (`Digest::Nothing`), never equal to a
  hash. Absence over a set (no impl for a type, no such name in a scope) is a read of the set
  under its own kind, whose hash covers the set. Lacking: a read that cannot be valued now is
  an error of the whole lookup (`KernelError::World`), where the hir-ty stores pass over that
  list; a front-end world needs that as an outcome of `World::hash`, not an error.
- **A covering digest.** No new rule in the lookup: a cover is a step. Its reads are only
  covering reads (a crate's digest, a directory's Merkle id), its value is the manifest of its
  members' entry keys, and a member is fetched by key (`Entry::read` checks it is that
  identity's). What the API needs is the proof that the cover covers: `World::cover(read) ->
  Option<Read>` (the covering read a read is under), and at keep time every read of every
  member must be under one of the cover's reads, else the keep is refused naming the read.
  It sits in nesting: a child run "under cover" has its reads checked against the parent's
  covering reads instead of recorded as the parent's. Nothing else may stand in front of a
  lookup.

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
