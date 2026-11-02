---
status: plan
summary: frontend-nameres — name resolution per module with facts (frontend.md item 1) and per-module impl lists under the impl buckets (items 2, 3); the design, its cost model, and what proves it.
date: 2026-10-29
---
# frontend-nameres: name resolution per module, with facts

[frontend.md](../../design/frontend.md) items 1 to 3. Today `crate_local_def_map`
(`crates/hir-def/src/nameres.rs`) runs `collector::collect_defs`, one fixed point over every
module of the crate, so an item added anywhere reruns resolution of the whole crate. The impl
buckets (`TraitImpls`, `InherentImpls`, `blaze-items`' `impls`) are read per bucket already, but
are built by walking `crate_def_map` whole.

The new resolver lives in `crates/hir-def/src/nameres/facts.rs` and `facts/`. It keeps rustc's
(and the old collector's) rules by keeping the collector's code for each rule, and changes only
the schedule and what is memoized:

- **collection is per module**, a query whose value is an *op log*: the mutations the old
  `ModCollector` made, in its order;
- **each expansion round resolves imports per strongly connected component** of an
  over-approximated import graph, components in reverse topological order, each a query that
  reads other components only through per-name facts;
- **macro resolution and expansion are per module and round**, reading the round's facts;
- `crate_local_def_map` **assembles** the `DefMap` from the per-module answers (their
  `ModuleScope` handles, not copies).

## 1. Why this shape

Three constraints decide it.

1. **No query cycles.** blaze-query (salsa's replacement) treats a cycle as an error
   (`crates/blaze-query/src/lib.rs`, "a query cycle"), so salsa's fixpoint cycles can't be the
   per-SCC fixpoint. Every cycle of glob imports has to sit inside one query, over a partition
   known before that query runs.
2. **Macros interleave with resolution.** A macro path resolves through imports, and an expansion
   adds items, imports and modules. The old driver's rounds stratify this: in round *r* the
   declarations are fixed, imports go to a fixed point, then every pending macro resolves once
   against that state and its expansion becomes declarations of round *r + 1*
   (`DefCollector::resolution_loop`). Keeping rounds as the driver turns every cross-module macro
   effect (`#[macro_export]` into the root, `#[macro_use]` into the parent) into a dependency on
   an earlier round, never a cycle.
3. **Exactness by construction.** Each rule (glob shadowing, visibility upgrades, `use_imports_*`
   chains, `$crate`, hygiene contexts, derive helpers, eager macros, the builtin-derive fast path,
   cfg, `#[macro_use]`, preludes, `extern crate`) is the collector's code moved over, not
   rewritten. What differs is only the order in which independent modules are visited.

## 2. The pieces

| file | what | port of |
|---|---|---|
| `facts.rs` | entry points (`collect_crate`, `collect_block`), shared keys and types | `collect_defs` |
| `facts/ops.rs` | `DeclOp`, `OpLog`, owned import and macro directives | the mutations of `ModCollector` |
| `facts/unit.rs` | `Unit`: the state of a set of modules resolved together; replaying ops; the import fixed point; `update` and glob propagation | `DefCollector` (imports, `update`, `record_resolved_import`, `push_res_and_update_glob_vis`) |
| `facts/collect.rs` | collecting one module's items (source or expansion) into a `Unit` while recording ops | `ModCollector`, `seed_with_top_level`, `seed_with_inner`, `collect_macro_expansion` |
| `facts/macros.rs` | one round of macro resolution for one module; reseeding an unresolved attribute; the unresolved-macro diagnostics | `resolve_macros`, `reseed_with_unresolved_attribute`, `finish` |
| `facts/graph.rs` | the over-approximated import graph of a round and its SCCs (iterative Pearce, CSR) | the "proper glob graph" FIXME |
| `facts/queries.rs` | the salsa queries below | |
| `facts/driver.rs` | the round loop, assembly of `DefMap`/`LocalDefMap`, block def maps | `collect`, `finish`, `hand_over_scopes` |
| `facts/impls.rs` | per-module impl lists for the impl buckets | |
| `facts/tests.rs`, `facts/tests/` | differential, property and trace tests | |

Each piece's own notes: [nameres-core.md](nameres-core.md), [nameres-graph.md](nameres-graph.md),
[nameres-impls.md](nameres-impls.md), [nameres-diff.md](nameres-diff.md),
[nameres-oracle.md](nameres-oracle.md).

## 3. Queries (salsa now, blaze-query later; each key is content, never a dense index)

| query | key | value | reruns when |
|---|---|---|---|
| `module_decls` | `DeclKey`: crate, module, origin (tree, mod dir), the legacy macros it inherits | `ModuleDecls`: the op log of round 0, its children with their `DeclKey`s | the module's own item tree, or what it inherits |
| `legacy_exports` | `DeclKey` | the module's legacy macros after collection (for `#[macro_use] mod`) | backdates unless they change |
| `subtree_macro_exports` | `DeclKey` | `#[macro_export]` macros of the module and its descendants (for the root's own lookups) | backdates unless they change |
| `module_round` | crate, round, module | `ModuleRound`: the ops the module gains at this round (expansions, a reseed), and the round its declarations last changed | the module's macro step of the round before |
| `import_shape` | crate, round, module | the module's import paths, aliases, kinds and child names | backdates unless an import or `mod` changes |
| `partition` | crate, round | SCCs of the round's import graph, in reverse topological order | an import shape changes; backdates if the SCCs are the same |
| `module_rows` | crate, round | a `Modules` table of every module of the round, each scope a `ScopeSlot::Fact` | the module tree changes |
| `latest` | crate, round, SCC anchor | the last round ≤ this one at which the SCC's state changed | its members' ops, or a scope it read changed that round |
| `scc_state` | crate, round, SCC anchor | `SccState`: members' `ModuleScope` handles, import statuses, the upstream reads | a fact it read (salsa edges: `ModuleScope::name`, glob scopes) |
| `fact_handle` | `FactKey` (crate, round, module) | the module's `ModuleScope` at that round | backdates (stable handle) |
| `macro_step` | crate, round, module, round kind | `MacroStep`: calls resolved, the ops of their expansions, what is left | a fact its macro paths read |
| `module_impls` | crate, module | the module's impls (trait or inherent, reservation bit) in declaration order | backdates unless an impl is added or removed |

The facts of [items.md](../items.md) 6.2 are the edges: `ModuleScope::name(n)` is "name `n` in
scope `S` resolves to X" and its `PerNs::none()` the negative fact "`n` is not in `S`"; a glob
reads the whole visible scope of its source (it *is* every name of it). An import that read a
name of its own SCC reads it in place, without a query: those reads are the worklist's
(`Imports::waits`), as before.

## 4. The round loop (`driver.rs`)

```text
decls  = module_decls(root), then each child's, DFS  (one query per module, memoized)
r = 0, kind = Normal
loop:
    progress, deferred_eager = fold macro_step(r, M, kind) over modules
    if progress:                       r += 1, kind = Normal
    elif kind == Normal && deferred_eager: r += 1, kind = EagerWithErrors
    elif let Some(attr) = first unresolved attribute (module order, then position):
                                       r += 1, kind = Reseed(attr)
    else: break
assemble from fact_handle(final, M) for every M
```

`macro_step(r, M)` reads M's facts of round *r*, which pull `scc_state` of M's SCC (and, through
`fact_handle`, the SCCs upstream) lazily. Rounds are the old driver's: a round's import fixed
point runs to completion before its macros, as `resolve_imports` did before `resolve_macros`.

`scc_state(r, X)` for `r > 0` starts from `scc_state(latest(r - 1, X), X)` when the members are
the same (the old driver kept its state across rounds the same way), replays the members' ops of
round *r*, re-pulls each upstream glob source whose scope changed this round, wakes each import
whose upstream fact changed, and runs the worklist. A component that merged at round *r* starts
from round 0's ops of its members, replayed in module order, then each later round's.

## 5. The graph (`graph.rs`)

Nodes are the round's modules. For each import of module `M` the edge set over-approximates the
modules whose scope its resolution reads:

- `cands(X, n)`: the local modules name `n` can denote in `X`'s type namespace: children named
  `n`; targets of `X`'s plain imports binding `n`; `extern crate self as n`; and, through each
  glob of `X`, `cands(T, n)` for its targets `T`;
- `targets(p)` of a path: the first segment by kind (`crate`, `self`, `super`ⁿ, `$crate` of this
  crate, plain in 2018 via `cands(M, s0)` and the local extern prelude, plain in 2015 and `::`
  via the root), then `S_{i+1} = ⋃ cands(T, s_i)`;
- edges `M → T` for every `T` in any `S_i`, and `M → M`; a glob adds `M → T` for its targets;
  a `#[prelude_import]` makes every module read its module.

A module an expansion creates belongs to its parent's node. SCCs come from Pearce's iterative
algorithm ("A space-efficient algorithm for finding strongly connected components", 2016) over
CSR edges: one `u32` index array and a `u32` low-link per node, no recursion, components emitted
in reverse topological order. Each component gets its topological index; the unit checks every
read of a non-member against it and stops with an error naming the import and the module when a
read is not upstream (a bug in the over-approximation, which the differential tests hunt for).

## 6. Cost model

Per edit, with `m` modules, `i` imports, `f` the facts an SCC read and `k` rounds:

- the driver: `O(m)` query validations per round and one `Modules` table, no scope copied;
- `module_decls`: the edited module only (its children re-validate; their inherited legacy
  macros are part of their key);
- `partition`: reruns only when an import shape changes, `O(i · segments)` set operations;
- `scc_state`: only components a changed fact reaches. Within one, the old worklist: an import is
  retried only when a slot it read is written (`O(edges)` per propagation step, no allocation);
- cross-component reads: one query call per distinct fact per state (cached in the unit), not per
  lookup.

Cold, the cost is the old collector's plus `O(k · m)` query records. Hot, adding `fn foo` to `a`
reruns `module_decls(a)`, the state of `a`'s component at each round, and the components that
read `a::foo` or glob `a`; adding `use x::Y` to `a` adds the partition run.

### What a unit is

A unit is a `DefMap` in `ModulesSlot::Building` holding every module of the round: the
component's members as `ScopeSlot::Building`, every other module as `ScopeSlot::Collected` of
its `fact_handle`. Path resolution (`path_resolution.rs`) is unchanged: it reads a module's name
through `ModuleData::name_in_scope`, which for a collected slot is the per-name query
`ModuleScope::name`. So the facts are not a second table beside the resolver: the resolver's own
reads of upstream modules are the per-name salsa edges, and a negative fact is a `name` memo
whose value is `PerNs::none()`. A glob reads `ModuleScope::scope` whole. `facts` is a child
module of `nameres`, so it builds these maps from private fields with no new API.

A block def map is one unit over the block's modules and no inner queries: `block_def_map` is
already one query per block, and its upstream (the crate's def map, enclosing blocks) is final.

### Speed of light

Per edit, the floor is the work a perfect oracle of the changed facts would do:

- re-reading the edited module's item tree: one pass over its items, `O(items)`;
- one hash probe per fact a changed module's readers hold (`ModuleScope::name` memo validation:
  a salsa memo check, ~50 ns, one or two cache misses);
- re-resolving exactly the imports whose read facts changed, each `O(path segments)` probes.

The design's overhead above that floor is the validation of the queries that did not change:
`O(m)` per round for the driver's fold over `macro_step`/`fact_handle` (a memo check each, no
execution) plus `partition` validation. For a crate of 1000 modules and 4 rounds that is ~4000
memo checks, ~200 µs at 50 ns, against a cold collection of tens of milliseconds; the target is
that a one-item edit costs under 1 ms of resolver time on such a crate, within 10% of the
floor plus that validation. The fold is the term to cut if measurement shows it dominates: a
per-round "modules with macros" list keeps it to modules that have any.

## 7. State of the art

- rustc: `rustc_resolve` reruns the whole crate's early resolution on every build; its
  incremental system starts after resolution. Nothing is reused.
- rust-analyzer: one `crate_def_map` fixed point per crate, invalidated by any item change.
- Incremental Datalog (differential dataflow, DDlog, Soufflé's incremental mode): stratify by
  SCC, recompute a stratum from scratch when its inputs change, cut off at unchanged facts. This
  design is that schedule on name resolution, with salsa's (blaze-query's) red-green edges as
  the fact table and expansion rounds as strata.

## 8. Exactness, and what differs

The per-module answer equals the old collector's, with three differences:

1. **Column order.** `ItemScope` columns record insertion order, which in the old collector
   follows its global worklist. Here it follows the component order. The tests compare a
   canonical projection: every column sorted by name, `use_imports_*` and side maps as sets,
   diagnostics sorted, modules by path. Everything else is compared exactly.
2. **Order-dependent errors.** Where the old collector's answer depended on its schedule (two
   globs bringing different items of one name: first wins; an import resolved before a later
   expansion changed its target: kept stale; which unresolved attribute is reseeded first), the
   program is rejected by rustc (E0659, "cannot find attribute"). Here the choice is
   deterministic: the component order, then module order, then position. The tests list such
   fixtures by name, each with the rustc error it gets.
3. **A within-round write is seen one round later.** A macro of module `N` whose path goes
   through a name an attribute's reseed or an expansion collected earlier in the same pass added
   resolves in the next round instead of the same one. The final state is the same; the round
   count can differ.

## 9. Impl lists (items 2, 3)

`module_impls(M)` lists M's impls from its final `ModuleScope`, read through the public
`DefMap` API (`facts/impls.rs`). The scope changes with any item of M, but the list is equal
then and backdates, so a bucket built from it reruns only when an impl is added or removed in
a module it covers ([nameres-impls.md](nameres-impls.md)). Requests to lane items
(`v2/requests/items.md`, "frontend-nameres") build `TraitImpls`/`InherentImpls` buckets and
`traits_in` from it per module and stop `in_crate_and_deps_for_trait_and_self_ty` looping
`for_crate_and_deps`.

## 10. What proves it

| claim | test |
|---|---|
| the new def map is the old one (canonical projection), on every fixture | `facts/tests/differential.rs`: every fixture of `nameres/tests*.rs` and `macro_expansion_tests`, old `collector::collect_defs` vs `facts::collect_crate` |
| same on generated crates with globs, cycles, shadowing and macros | `facts/tests/generated.rs`: proptest over module graphs |
| the partition is closed (no read leaves its component upward) | the unit's check, exercised by both of the above; `facts/tests/graph.rs` |
| SCC order is reverse topological; Pearce equals a reference Tarjan | `facts/tests/graph.rs` |
| an edit reruns only modules whose facts changed | `facts/tests/trace.rs`: `TestDB::log_executed` names of `module_decls`/`scc_state`/`macro_step` |
| a negative fact flips when the name appears | `facts/tests/trace.rs`, `blaze-v2-proof`'s nameres module |
| impl buckets rerun only for their (trait, self type) | `harness/v2/fixture.py` slots, `blaze-v2-proof` |
