---
status: plan
summary: frontend-fast-method's notes for method resolution (fast.md section 3): traits in scope as interned IDs and per-name buckets, a lock-free pick cache shared by a crate's bodies in a revision through capture_reads/replay_reads, a cache of autoderef steps by canonical receiver, the bound with its arithmetic, and every rustc rule the probe keeps.
date: 2026-10-29
---

# frontend-fast-method: method resolution

Area: section 3 of [fast.md](fast.md). Code: `crates/hir-ty/src/method_resolution/scope_sets.rs`,
`method_resolution/pick_cache.rs` (with `pick_cache/table.rs`, `pick_cache/tests.rs`), the
hooks in `method_resolution.rs` and `method_resolution/probe.rs`, and the cases in
`harness/v2/frontend/cases/method/`.

## 1. What a method call costs today

Every `receiver.name(..)` and `Type::name` runs the whole probe, with no cache
(`probe.rs` `probe_for_name_with` then `probe_op`, line 317):

1. **Traits in scope.** `InferenceContext::get_traits_in_scope` (`infer.rs` 3210) hands out the
   body's `FxHashSet<TraitId>` (built once per body by `Resolver::traits_in_scope`, `infer.rs`
   2035), or, in a block with items, a fresh union of the block's and the body's sets.
2. **Canonicalize** the receiver with the body's opaques (`PredefinedOpaques::new_from_iter`,
   `canonicalize_query`).
3. **Autoderef steps** (`method_autoderef_steps`, line 429): a fresh `Autoderef`, each step a
   `Deref` (or `Receiver`) goal and a normalization through the solver, each step's type made a
   canonical response. rustc caches exactly this, as the `method_autoderef_steps` query.
4. **Assembly.** Per step, the inherent impls of its simplified type (two salsa lookups) and,
   for every trait in scope, `TraitValueItems::get` (a crate run lookup and a binary search,
   `method_resolution.rs` 604). The std prelude alone puts about 30 traits in scope, so a probe
   asks 30 to 60 traits, of which nearly always none or one has an item of the name
   (`assemble_extension_candidates_for_traits_in_scope`, line 1219).
5. **Pick** (`pick_all_method`, line 1396): for each step by value, autoref, autoref mut, const
   pointer, each candidate through `consider_probe` (line 1704): a snapshot, fresh arguments,
   the method signature instantiated, a relate, and the impl's where-clauses as goals through the
   solver; then the stable-only pass and the pass with unstable items (`pick_core`), collapse to
   a trait or subtrait pick, and the shadowing checks.

Counted on `v.len()` for `v: Vec<u32>`: step 0 by value considers `Vec::len` (inherent,
`&self`, no match) and `ExactSizeIterator::len` (a goal, no match); autoref considers `Vec::len`
again (match). That is 3 `consider_probe`s, 2 autoderef goals and 2 normalizations, about 40
salsa lookups for the traits in scope, and the canonicalization: about 5 to 15 µs, 17,000 to
50,000 cycles at 3.5 GHz. A body has about 30 method calls and generic paths
(speed-of-light-infer.md), so 0.15 to 0.45 ms of a body's inference is the probe.

## 2. The bound

The pick is a function of its inputs (section 4 lists them); for a receiver with no inference
variables the inputs are known before the probe, so the answer is one lookup. Figures as in
fast.md and [speed-of-light.md](../db/speed-of-light.md): 3.5 GHz, IPC 2, L1 4 to 5 cycles,
L2 14, L3 50, DRAM 110 ns.

- **The key** is 10 fields in 56 bytes (7 u64 words), read from values the caller holds: the
  database's nonce, two crate ids and the module (0 unless the answer is module-dependent), each
  a u64; `Ty::index`, `Symbol::id`, the scope bucket's number, `Clauses::index`, the typing
  mode's list index and the flags, each a u32. Hashing it with Fx (a rotate, xor and multiply
  per word, the multiplies 3 cycles each and dependent) is 7 × 4 = 28 cycles: `PickKey::hint`
  feeds the u32 fields in pairs, one word each, not one round per field as a derived `Hash`
  would (10 rounds, 40 cycles).
- **The lookup.** The value holds lists (`Pick::shadowed_candidates`,
  `Pick::unstable_candidates`) and a `salsa::Reads`, so it is bigger than a word and is
  published by pointer: a lock-free reader with no retry loop needs at least the slot's line
  (hint and pointer) and the value's line, two dependent loads. The pick table is 2^16 slots of
  16 bytes, 1 MiB, so in steady state its lines are in L2 or L3: 2 × 14 = 28 cycles from L2,
  2 × 50 = 100 from L3.
- **The answer**: a 104-byte `Pick` copied, its two lists empty (a `Vec::new` clone writes three
  words, no allocation): about 10 cycles.

Bound for a hit: 28 + 28 + 10 ≈ 66 cycles from L2 (19 ns), 140 from L3. fast.md's 6 ns is the
L1 case of the lookup alone (28 + 2 × 5 ≈ 40 cycles, 11 ns, with the key hash; 6 ns without).

The code's hit: the same 7-word hash, the 16-slot window's first slot (hint and pointer in one
16-byte slot, so one line), the entry's line, the key compare in that line, the copy: 66 cycles
from L2, within 10% of the bound (the window's second slot is read only when the first holds
another key, about 1 in 2^16 × load factor).

What a hit also pays and the bound above leaves to its owners:

- **Forming the key.** A receiver with free regions (`&self` receivers, `&'a T`) is region-erased
  first, a fold that rebuilds only the parts with regions (about 20 to 60 cycles for `&Foo`,
  one interner lookup per rebuilt node). A precomputed region-erased column in the type table
  would make it one load; that is db-ty's table, so it is a request (section 8).
- **The scope bucket**: `module_items_named(db, module, name)`, a salsa memo hit (two-argument
  function: the argument tuple's interned lookup, then the memo): speed-salsa.md's 2 to 4 lines,
  56 to 200 cycles.
- **Replaying the reads** (`salsa::replay_reads`): one `insert_edge` per edge the pick read,
  about 15 cycles each; a pick reads 10 to 40 edges, 150 to 600 cycles. This is the price of
  sharing a result across queries while each query still depends on what the result read; it is
  the solver cache's price too (`tls_cache.rs`). A query that already read an edge pays only the
  hash, since the edge set deduplicates.

Total on a hit, scope bucket and replay included: about 300 to 900 cycles against 17,000 to
50,000 today.

**Autoderef steps.** A miss of the pick cache (receivers with inference variables, the first
probe of a key, probes outside the pick cache's domain) asks the steps cache: the same lookup
(2 lines, about 60 cycles with the hash), then the probe borrows the steps in place (no copy, no
allocation) under the table's reclaim guard. Today each step costs 2 solver goals.

**Assembly by bucket.** A probe's extension candidates are the bucket's items (traits in scope
with an item of the name: nearly always 0 or 1), so assembly is one memo hit plus one
`consider_probe` per item, where today it is 30 to 60 `TraitValueItems::get` calls (each about
20 to 40 cycles: a `module(db)`, a `block(db)`, a crate run, a binary search): 1,000 to 2,400
cycles saved per miss. The bucket is also what a body depends on (request R4 of
frontend-fast-bodies): a `use` of a trait without an item of the name no longer re-infers the
body.

**Applicability** (`consider_probe`'s goals) goes through the solver and its caches. The pick
cache removes it from every hit; on a miss it is the solver's goal cost, which frontend-fast-trait
owns (section 7 has the interface agreed with it).

## 3. The design

### 3.1 Traits in scope as IDs (`scope_sets.rs`)

- `TraitSet`: a sorted, deduplicated list of `TraitId`, interned in a process-wide
  `next_solver::interner::table::Lists<TraitId>` (lock-free lookups, rows dense from 1, never
  freed). Equal sets are equal numbers.
- `module_trait_set(db, module) -> TraitSet` (tracked): the traits of `module`'s resolver walk
  (its blocks, the module, the prelude), once per module and revision.
- `ScopeItem { trait_, item }` and `ScopeItems`: the traits of a set with a value item named
  `name`, each with that item (`trait_value_item`'s answer), sorted by trait, interned in a
  second `Lists`. `module_items_named(db, module, name) -> ScopeItems` (tracked) is what a body
  reads of its scope, and it backdates when a trait without an item of the name comes or goes.
- `TraitsInScope`: `Scope { module, impl_trait }` for a position in a body (the innermost block
  or module, and the trait of the body's impl), or `Set(TraitSet)` for a caller's own set
  (`crates/hir`'s IDE queries, `sized_bound.rs`'s all-traits probe).
  `TraitsInScope::of_resolver(db, resolver)` makes the first; `named(db, name)` gives the
  bucket; `all(db)` every trait, for `probe_all`.

The impl's trait is `db.impl_trait(impl)`, the trait the impl's own header names, resolved at
the impl, as rustc's `current_trait_ref` is. Today's `Resolver::for_each_trait_in_scope`
resolves the header's path from the innermost scope of the call, so a block inside the method
that declares an item of the trait's name changed which trait was in scope. That was a bug
(rustc never resolves the impl header from inside a body); `TraitsInScope` fixes it and case
`method/043-impl-trait-in-scope-not-shadowed-by-block.rs` covers it.

### 3.2 The pick cache (`pick_cache.rs`)

**Key** (`PickKey`, every input the pick reads, from `probe.rs` and `method_resolution.rs`):

| input | where the probe reads it | in the key as |
|---|---|---|
| receiver type | `canonicalize_query`, every step | `Ty::index` of the receiver with its free regions erased |
| method name | `with_impl_or_trait_item`, the bucket | `Symbol::id` |
| traits in scope | `assemble_extension_candidates_for_traits_in_scope` | the bucket's number (`ScopeItems::id`): two scopes with the same items of the name share |
| param env | every goal, `assemble_inherent_candidates_from_param` | `Clauses::index` |
| typing mode | every goal, opaque handling | its tag and its def-id list's index |
| solver crate | lang items, recursion limit, the solver cache scope | `Crate` id |
| resolver crate | `denied_unstable_feature`, `with_incoherent_inherent_impls`, `PubCrate` visibility, the features | `Crate` id |
| visibility | `push_candidate` (`Resolver::is_visible`, inherent candidates only) | 0, or the module when some inherent candidate's visibility is `Visibility::Module` (two levels, below) |
| features | `arbitrary_self_types(_pointers)`, `supertrait_item_shadowing`, `is_enabled` | the crate's def map's; the three flags the probe reads by name are also key bits |
| edition | `SKIP_ARRAY/BOXED_SLICE_DURING_METHOD_DISPATCH` (`consider_probe`) | two bits, at least 2021 and at least 2024 |
| mode | `Mode::MethodCall` or `Mode::Path` | a bit |
| `allow_unstable` | `pick_core`'s stable-only pass | a bit |
| region answers | the interner's `regions()` (which solver cache it reads) | a bit |
| database | everything | the `Nonce` |
| revision | everything | the table's state: an entry is used only in the revision it was made in |

The pick reads nothing else: the spans (`call_span`, `receiver_span`) name obligations' causes
and variables' origins only, and the probe emits no diagnostic or lint (rustc's
`unstable_name_collisions` and the edition lints are emitted by the callers from the pick, per
call, with the call's spans: `write_unstable_candidates`, `lint_edition_dependent_dot_call`).

**Visibility in two levels.** Only inherent candidates are checked, and only a
`Visibility::Module` answer depends on where the call is (`Public` never does, `PubCrate`
depends on the resolver crate, in the key). A probe notes whether any check it made was of a
`Module` visibility. If none, its answer is stored under module 0 and shared by the whole crate.
If one was, a marker (`Cached::ByModule`) goes under module 0 and the answer under the call's
module; a later lookup that finds the marker looks again with its module.

**Domain.** Cached only when the answer cannot depend on the body's inference state:

- the receiver, after `resolve_vars_if_possible`, has no type or const inference variables, no
  placeholders, no opaque types and no errors;
- the param env has no inference variables or placeholders;
- the body's opaque type storage is empty;
- the steps had no bad type (`opt_bad_ty`, an unresolved variable or an error at the end of the
  autoderef chain) and did not reach the recursion limit;
- the answer is a pick, `NoMatch`, `Ambiguity` or `PrivateMatch`, with no inference variables or
  placeholders in it (`ErrorReported` and `IllegalSizedBound` are not cached);
- `salsa::capture_reads` returned `Some`: the probe read no provisional value, made no
  untracked read and took no goal from a query-scoped solver cache.

Every other probe runs the probe alone. This is a split by shape, not a fallback: the probe is
the one path for those shapes, and the cache is the one path for the others once filled.

**Regions.** The probe erases regions itself (autoref uses `'erased`, canonicalization maps
every region to a variable), so a pick does not depend on the receiver's regions; the key
erases them so `&'a Foo` and `&'b Foo` share. `Pick::self_ty` was the only region-carrying
part of a pick (the instantiated step type, with this body's region variables). It is now
region-erased in every pick, cached or not (`ProbeForNameChoice::choose`). Its readers:
`confirm.rs` does not read it; `sized_bound.rs`'s `differs_from` compares it (rustc compares
picks of one probe context whose region variables are equal, so the erased compare gives the
same answer) and reads its `Ref` mutability (kept).

**Value.** `Cached::Answer(Answer, Reads)`: the pick (or the cacheable error) with its
lifetime erased (an interned handle is an index into a process-wide table, valid for any `'db`
of the database the key's nonce names, in the revision the state names), and the reads. A hit
replays the reads into the asking query, so it reruns when anything the probe read changes, and
copies the pick: no allocation when its two lists are empty, which they are for every pick but
those with shadowed supertrait items or unstable candidates set aside (their copy allocates the
lists, on the edge of the feature).

**Table** (`pick_cache/table.rs`): fixed size, open-addressed, a 16-slot window per key, each
slot a 16-byte (hint, pointer) pair, the entry behind the pointer made once and never changed.
A lookup is loads only, under a `blaze_reclaim` guard; an insert is a compare-and-swap of the
slot's pointer, the replaced entry retired through the table's limbo and freed after a grace.
A full window gives away its first entry not used since the last sweep (second chance), and a
slot holding the key in an older revision is reused first. No lock and no shared write on a
read but the use mark, set once per sweep. Picks: 2^16 slots; steps: 2^14.

### 3.3 Autoderef steps by canonical receiver

`pick_cache::with_steps` keys `MethodAutoderefStepsResult` by what `method_autoderef_steps`
reads: the canonical input's value (`Ty::index`, the predefined opaques' index), its variables'
index and max universe, the param env, the typing mode, the solver crate, the
`arbitrary_self_types` flags and the regions bit. The steps are canonical responses relative to
the input's variables, so they are the same for every body; a probe instantiates them with its
own `OriginalQueryValues` as today. This is rustc's `method_autoderef_steps` query. It covers
receivers with inference variables and every name probed on one receiver. A hit borrows the
steps in place for the probe (no copy); a miss computes them inside `capture_reads` and stores
them when the capture is `Some`.

A slice per key, made once at its length, is the CSR row; a global CSR array would need a
lock-free append arena for rows of varying length and would save one pointer load.

### 3.4 Assembly by bucket

`ProbeForNameChoice` takes the bucket and assembles one extension candidate per item
(`assemble_extension_candidate`); `ProbeAllChoice` (IDE completion) walks `TraitsInScope::all`
as before. The bucket's order is sorted `TraitId`; today's was `FxHashSet` order. The order of
extension candidates changes no pick (`consider_candidates` takes all applicable candidates and
either collapses them or reports ambiguity), and the ambiguity's sources are now in a
deterministic order.

## 4. rustc behaviors kept, each with its case

The probe's algorithm is unchanged; the cache only remembers answers for equal inputs. Each rule
below has a case in `harness/v2/frontend/cases/method/`, and the differential test (section 5)
runs every probe of the fixtures cached and uncached.

| rule (rustc `method/probe.rs`) | cases |
|---|---|
| Pick order per step: by value, then `&`, then `&mut`, then `*mut T` to `*const T`, then the next autoderef step | 001 to 006 |
| Inherent before extension at one step and adjustment | 007, 008 |
| Ambiguity of two applicable trait candidates, E0034 | 009, 010 |
| Collapse to the trait pick when the candidates are one trait's impls (`collapse_candidates_to_trait_pick`) | 011 |
| Supertrait item shadowing under `#![feature(supertrait_item_shadowing)]` | 012, 013 |
| Private inherent match: E0624 when only a private method applies | 014, 015 |
| Visibility differs by module: one type's method called from inside and outside its module | 016, 017 |
| No method, E0599, including a trait method not in scope | 018, 019, 020 |
| Trait in scope through `use`, a block's `use`, the prelude, `use Trait as _` | 021 to 024 |
| Edition-dependent array `into_iter`: by reference before 2021, by value from 2021 | 025, 026, 027 |
| Boxed slice `into_iter` before 2024, by value from 2024 | 028, 029 |
| `TryFrom`/`TryInto`/`FromIterator` in the 2021 prelude, not in 2018 (E0599 there) | 030, 031, 032 |
| Unstable candidates: a stable trait method picked over an unstable inherent one, `unstable_name_collisions` | 033, 034 |
| Autoderef through `Deref` (Box, Rc, String, Vec) and the array-to-slice unsize step | 035, 036, 037 |
| Raw pointer receivers: no autoderef through them, `*mut` to `*const` | 038, 039 |
| Where-clause candidates on a type parameter, object candidates on `dyn Trait` | 040, 041 |
| Path mode (`Type::item`, `<T>::item`), consts included, no autoref | 042 |
| The impl's own trait is in scope in its methods | 043 |
| The same receiver and name probed from many bodies and modules (the cache's sharing) | 044, 045, 046 |
| Receivers with inference variables resolved later (outside the cache's domain) | 047, 048 |

## 5. Proof

- **Differential test** (`pick_cache/tests.rs`): a test-only switch makes every pick cache hit
  also run the uncached probe and assert the two answers equal (item, kind, autoderefs,
  adjustment, erased self type, receiver steps, both lists; or the same error with the same
  sources). It is run over every fixture of `crates/hir-ide/src/tests/method_resolution.rs` (read
  with `include_str!` and split at its raw strings), of `traits.rs`, and over generated bodies:
  a xorshift generator writes structs, a generic struct, traits with colliding method names,
  inherent (some private to a module) and trait impls with `self`, `&self` and `&mut self`
  receivers, a `Deref` wrapper, a trait in scope in one module only, and bodies calling each
  name on owned, borrowed, doubly borrowed, wrapped and generic receivers and through paths.
  Boxed, array and raw pointer receivers are the cases' (035 to 039) and the fixtures'. The bucket is checked against the old assembly copied into the test module: for each
  scope and name, the old loop over `Resolver::traits_in_scope` with `TraitValueItems::get`.
  Each run asserts the cache was hit, so the test fails if the cache stops being used.
- **Allocation budget**: a table hit allocates nothing (`blaze_alloc_count::assert_at_most(0,
  ..)`), and, in the end-to-end runs, every pick cache hit of a pick with empty lists measured at
  zero allocations (a test-only measurement around the lookup and the copy; the replay appends to
  the asking query's own edge list, which it would have appended to had it made the reads).
- **Concurrency**: threads looking up and inserting the same keys get one value per key and
  state, and a replaced entry is freed only after its readers left.
- **Cases**: 48 under `harness/v2/frontend/cases/method/`.

## 6. Prior art

- rustc: the `method_autoderef_steps` query, keyed by the canonical receiver and param env (the
  steps cache here); picks are not cached in rustc, since its typeck is one query per body and
  incremental reuse is per body.
- Roslyn: member lookup results are cached per binder and per type (`LookupResult` pools,
  `NamedTypeSymbol.GetMembers(name)` cached per type), and extension method candidates are
  gathered once per scope and name.
- Swift: `MemberLookupTable` per nominal type, filled lazily per name and invalidated by
  extension loading, and qualified lookup cached by (type, name).
- The table: the solver's `tls_cache.rs` `Shared` (same window, second chance and limbo), with
  the slot's hint and pointer in one line.

## 7. Interfaces

- **frontend-fast-trait** (global-goal table): a probe's impl where-clause goals in
  `consider_probe` are global once the receiver is (the cache's domain). The interface agreed:
  look goals up after `resolve_vars_if_possible`, so a goal whose variables were unified with
  global types hits the global table; the pick cache stores no goal answers of its own.
- **Lane db, `crates/hir`**: `MethodResolutionContext::traits_in_scope` is a `TraitsInScope`.
  `Type::with_method_resolution` (`hir/src/lib.rs`) and `attrs.rs` pass
  `TraitsInScope::Set(TraitSet::collect(traits_in_scope.iter().copied()))`.
- **db-ty-d-infer2, `infer/sized_bound.rs`**: its all-traits probe passes
  `TraitsInScope::Set(TraitSet::new(&mut traits))`.
- **mem-db-infer-ctx's M1** (an enum of a frame set and a caller's set) is superseded: the body
  keeps no set of its own, so `infer.rs`'s `traits_in_scope` and `block_traits` fields and
  `get_traits_in_scope` go once `sized_bound.rs` stops calling it.
- **mem-db-infer-method's P3** (the steps in the frame) is superseded for `Mode::MethodCall`: the
  steps live in the steps table and a probe borrows them. P1, P2, P4, P5 apply on top.

## 8. Requests

In `v2/requests/` as named in the report: lane db (`crates/hir` construction sites),
db-ty-d-infer2 (`sized_bound.rs`), the infer.rs holder (the three fields), db-ty (a
region-erased column in the type table, so key forming is one load).
