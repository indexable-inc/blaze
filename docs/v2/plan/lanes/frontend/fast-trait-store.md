---
status: plan
summary: frontend-fast-trait-store — the persistent trait-goal cache by content (compile-time.md change 17): a global goal's answer kept in the store under its stable form, with the facts its evaluation read, reused while every fact hashes the same and the solver matches; the bound, the design, what no read can express yet, and the requests.
date: 2026-10-29
---
# frontend-fast-trait-store: the persistent goal cache

Child of [fast-trait.md](fast-trait.md) (its section 3). Code:
`crates/hir-ty/src/next_solver/goal_store.rs` and `goal_store/{stable,record,pending,tests}.rs`.
The parent's global-goal table (`global_goals.rs`) asks it on a table miss and offers it each
answer it inserts.

## 3. The persistent goal cache

### 3.1 The work and its bound

A global goal (no inference variable, placeholder, local region or error; a typing mode
defining no opaque) has one answer per crate, typing mode and regions flag in a revision. The
parent's table makes the second ask of a revision a hash probe. What is left is the first ask
in each session: a build of an unchanged workspace solves the same goals again. The store
makes that first ask a lookup by content, the same in every checkout and machine.

The bound of a lookup is the bytes it must touch:

- the goal's stable form: for the goals of `global_goals`' differential, 30 to 300 `u32`
  words (a trait predicate over an ADT with a few arguments, its param env's clauses), each
  definition 9 of them. BLAKE3 hashes 1.2 KB in about 1 µs on one core;
- one item key per definition, a memoized query of lane items: 5 to 20 at about 50 ns;
- two reads of the store: the shape (76 + 4·words + 68·facts bytes, its content hash checked
  by the store) and the 41-byte answer. Local CAS reads of mapped files: about 1 µs each;
- one value per fact the shape lists, each a memoized query: 5 to 20 at about 100 ns.

So a hit costs 3 to 5 µs, a miss on an empty slot under 2 µs (the hash and one read). A solve
of the same goal is, by estimate (not measured here: this lane runs nothing), 10 to 100 µs
when it selects among impls. The floor is the hash of the stable form and one read
keyed by it, about 2 µs: a store holding answers by goal key alone would save the second read
but could not let two shapes of one goal live at once (3.3). On a hit nothing allocates but
what the store's read hands back (an `ImageBuf`, the store's); on a lookup with no store,
nothing at all (a load and a branch).

An offer costs the same hash's words into a per-thread buffer, the facts' values and an
uncontended lock: 1 to 3 µs, on the solving thread, once per goal per session. The flush is
two puts per new goal, on the store's threads.

### 3.2 What the code did before (2026-10-29)

Nothing persisted a goal. `rustc_type_ir`'s search graph keeps a global cache per interner
(patch `0003-shared-solver-cache.patch` shares it across interners of a crate), and the
parent's table keeps answers for a revision; both are memory, both are gone at exit.

### 3.3 The design

**Stable form** (`goal_store/stable.rs`). `persist`'s codec (`next_solver/persist.rs`, `Code`
and `Sink`) already writes every interned value as `u32` words. A goal's stable form is the
typing mode's tag, the regions flag and the crate's recursion limit, then the predicate's and
the param env's words, through a sink that writes each child inline (not as a row of a
table) and each definition as its own variant and its item key (`GoalFacts::def_key`, lane
items' `key::def_key`), never a salsa ID. Every definition the codec writes goes through
`impl Code for SolverDefId` (ADTs through `Sink::adt`, which adds the flags); the hook that
lets a sink take it over is requested from db-ty. A form past 65 536 words is not stored.

The goal's key hashes, in domain `"blaze v2 goal 1"`, the solver version, the crate's key and
the words. The solver version hashes `harness/toolchain`, the two vendored solver crates'
`PATCHES.md` and every patch (embedded with `include_bytes!`; a test fails when the
directories list a file the version does not), and `build`, the content ID of the front end's
code that `hir_def::deps::serve` is given. The crate's key is lane items': its identity and
its lang items, which an interner reads when it is made, outside any capture.

**Records** (`goal_store/record.rs`), as `hir_def::deps::serve` keeps images:

- under `shape_key(goal, n)` for `n` in `0..4`, a *shape*: magic, goal key, solver version,
  the stable form, and the facts the evaluation read (kind, subject, object, as lane items'
  `FactSet` splits a `Fact`), sorted;
- under the hash of the shape's key and those facts' values when it was solved, the *answer*.

A lookup reads the shapes slot by slot, hashes each one's facts as they are now, and asks for
the answer under that: a constructive trace (Shake's, Bazel's action cache). One goal can keep
up to four shapes, as an edit can make it read other facts (an impl added) and both
checkouts are built. A record is checked against its key when opened; a record that does not
open, or a store that cannot be read, is an error the flush reports (the goal itself is
solved, which gives the same answer, so the build stops at the flush with the goal named).

**What is stored.** Only what the parent's table inserts: root, shareable goals whose answer
left the inference context as it found it (`Answer::Holds` with certainty yes and no change,
or `Answer::Fails`), never a cycle's provisional answer (those make `capture_reads` give
`None`, so the table never sees them) nor a goal with inference variables (not global). The
offer walks the `salsa::Reads` of the evaluation: an interned read is a pure function of its
fields and needs nothing; any other read must stand for a fact (`GoalFacts::fact_of`), or the
goal is not stored and the read's ingredient name is counted (`Flushed::inexpressible`). A
fact whose value cannot be hashed now keeps the goal out too.

**Soundness tripwire.** If the store already holds another answer under the same shape and
fact values, the facts did not cover what the answer depends on: the flush stops with an
error naming the goal (`two_answers_under_the_same_facts_stop_the_flush`).

**Where the store comes from** (`goal_store.rs`). The driver installs one per database:
`goal_store::install(&db, images, facts, build)`, with the store adapter it gives
`hir_def::deps::serve` (`DepImages`, requested in `v2/requests/cargo.md` by frontend-deps),
lane items' `GoalFacts`, and the build ID. It returns a guard; dropping it uninstalls. Stores
are found by the database's nonce in a 16-slot registry of atomic pointers, read under a
`blaze_reclaim` guard and freed through a `Limbo`. The interface above hir-ty is a trait,
`GoalFacts`, as lane items depends on hir-ty; it is reached by `dyn` on a table miss or after
a solve, never on a hit of the parent's table.

**No store.** `lookup` and `offer` begin with one relaxed load of the count of installed
stores and a branch; with none they return. A database with no store installed has no
persistent tier, as one given no store by `hir_def::deps::serve` reads no images. That is not
a fallback: no other path answers the goal and nothing is approximated; the goal is solved as
it is with the store, which only ever returns the answer the solve gives.

**Writes are batched** (`goal_store/pending.rs`). An offer appends its entry (answer, shape
bytes as they will be stored, fact values) to one of 64 per-thread-shard buffers, each its
own lock on its own cache line, capped at 1 MiB (past it, goals are dropped and counted).
`Installed::flush` takes every buffer and writes on the store's threads
(`ImageStore::for_each`). The driver calls it where it calls `hir_def::deps::store`: after the
build's front-end work, with no query running (requested in `v2/requests/cargo.md`).

**Reads on a miss.** A lookup runs in a `capture_reads` frame of the asking query. On a hit
the parent's table keeps the answer with those reads (the fact values checked), so the next
ask in the revision replays them. On a miss the fact values read in a slot that did not match
stay reads of the asking query: a dependency it may not have, never one it lacks.

### 3.4 What no read can express yet

Today nothing is storable: the solver reads hir-ty and hir-def queries that stand for no fact.
These are the reads, by the query, and the fact each should become (requested from lane items,
`v2/requests/items.md`, and, where the solver must read through a narrower query, from db-ty):

| Read by the solver (query) | Fact it should be |
| --- | --- |
| `TraitImpls::for_crate`, `for_each_block`, `blanket_in_crate_and_deps` (whole impl tables) | `Implements { trait_, self_ty }`: a query of the impls of a trait for one simplified self type, read in place of the tables |
| `impl_trait`, `impl_self_ty`, `ImplSignature::of`, `GenericPredicates::query` of an impl, `impl_items`, `impl_has_unconstrained_params` | `Signature(impl)` |
| `field_types`, `StructSignature`/`EnumSignature`/`UnionSignature::of`, `variances_of`, `representability`, `value_ty` | `Signature(adt)`; `Field { ty, field }` for one field's type |
| `TraitSignature::of`, `trait_items`, `dyn_compatibility`, `all_super_traits`, `ItemBounds` | `Signature(trait)` |
| `callable_item_signature`, `fn_is_const`, `const_conditions` | `Signature(fn)` |
| `specializes`, `specialized_ancestor` | `Specializes(impl)` |
| `rpit_hidden_types`, `tait_hidden_types`, `opaque_types_defined_by` | `AutoTraits(opaque)` for auto-trait leakage; `Signature` of the owner otherwise |
| `target_data_layout` (`Sized`/transmute goals through layout) | a target fact; none exists: lane items to add one, keyed by the target |
| attribute reads (`AttrFlags`: `#[fundamental]`, `#[rustc_*]` on traits and ADTs) | part of `Signature` of the item they are on |

Interned reads (`.loc()`, `.module()`, `.krate()` of an interned ID) are pure and need nothing.
`crate_def_map` and `lang_items` are read when the interner is made, outside the capture, and
are covered by the crate's key. Until the facts exist, `Flushed::inexpressible` lists these
names by count after every build: the driver shows it under `-v`.

### 3.5 What proves it

`goal_store/tests.rs`:

- `a_goal_reads_back_from_its_stable_form_and_its_record`: every type of a fixture's bodies
  (every kind a stored goal can hold, checked: scalars, ADTs of each kind, foreign, str,
  arrays, slices, raw pointers, refs, fn defs and pointers, `dyn`, closures, async closures,
  coroutines, never, tuples, aliases, params) plus a pattern type, an unsafe binder and a
  700-tuple, under a param env with where clauses and projections; each goal's words decode
  back to the same predicate and clauses, the streamed key equals the key of the words, its
  shape record reads back from an in-memory store as written and refuses another goal,
  another solver version or a cut record;
- `an_answer_is_found_while_its_facts_hash_the_same_and_its_solver_matches`: offered, nothing
  written until the flush, found after; a changed fact refuses; the old value finds the old
  answer again; another build ID refuses;
- `two_answers_under_the_same_facts_stop_the_flush`;
- `a_goal_with_a_read_that_is_no_fact_is_never_stored`;
- `a_database_finds_only_its_own_store`;
- `a_lookup_or_offer_with_no_store_allocates_nothing` (`blaze_alloc_count::assert_at_most(0)`);
- `the_solver_version_covers_every_patch`.

### 3.6 Prior art

- Bazel's action cache and remote cache: an action's key hashes its command and input
  digests; the result is reused anywhere the key matches.
- Shake's constructive traces ("Build systems à la carte", Mokhov, Mitchell, Peyton Jones,
  2018): keep the result with the hashes of what it read, reuse when those hash the same; two
  levels here (shape, then answer) as Shake keeps several traces per key.
- rustc's incremental cache keys queries by `DefPathHash` and stable hashes, never `DefId`s,
  and checks red/green by the dependency graph; the trait solver's caches are not persisted.
- `hir_def::deps::serve` in this tree: the same two-level slots and store interface.

### 3.7 Requests

- db-salsa (`v2/requests/db-salsa.md`, "frontend-fast-trait-store: iterate a `Reads`' reads
  with their kind and name"): `ReadKind`, `Ingredient::read_kind`, `Reads::for_each_read`.
- db-ty (`v2/requests/db-ty.md`, "frontend-fast-trait-store: a definition hook in `persist`'s
  codec, an interner's crate, and `pub mod goal_store`").
- items (`v2/requests/items.md`, "frontend-fast-trait-store: implement `GoalFacts`, and the
  facts the solver's reads need").
- cargo (`v2/requests/cargo.md`, "frontend-fast-trait-store: install the goal store and flush
  it beside `deps::store`").
