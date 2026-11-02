---
status: plan
summary: frontend-nameres-graph — the over-approximated import graph of an expansion round, its strongly connected components by Pearce's iterative algorithm over CSR edges, every case of the over-approximation and why it is sound, its speed-of-light bound, and what each test proves.
date: 2026-10-29
---
# frontend-nameres-graph: the import graph and its components

Part of [nameres.md](nameres.md) §5. Code: `crates/hir-def/src/nameres/facts/graph.rs`; tests:
`crates/hir-def/src/nameres/facts/tests/graph.rs`. The interface (`ImportShape`, `ShapeModule`,
`ShapeImport`, `Partition`, `partition`) is frozen in the frontend-nameres shared brief; core
builds the shape and runs one unit per component.

## 1. The contract

If resolving an import, or a `pub(in path)` visibility of a `use` item, of module `M` can read the
scope of module `T` (`ModuleScope::name`, a glob's whole `ModuleScope::scope`, or
`ModuleScope::children`), then `may_read(of(M), of(T))`. `may_read(c, u)` is `u <= c` in the
components' order. The order is reverse topological over the graph's edges, so it holds for `T`
reachable from `M` along edges, not only for a direct edge `M -> T`. That is what lets two of the
reads below go without an edge of their own.

## 2. The over-approximation, case by case

Nodes are the round's modules, root first (index 0). For every path, `L_i` is the set of modules
segment `i` is looked up in. Every module in any `L_i` gets an edge from `M`. `L_{i+1}` is
`⋃ cands(T, s_i)` over `T ∈ L_i`, and past the last segment the path denotes `D = ⋃ cands(T, s_last)`.

`cands(X, n)` is the set of local modules name `n` can denote in `X`'s type namespace. It is the
least set holding:

- the `local_names` of `X` named `n`: `mod` children (expansions' too) and `extern crate self as n`;
- `D` of every plain import of `X` binding `n` (an alias, or the last segment; `as _` and globs
  bind nothing);
- `cands(T, n)` for every glob target `T` of `X`, where a glob target is anything in `D` of a
  glob import of `X` (not the prelude's).

Only modules matter here. An enum, variant or other item ends a path (`resolve_remaining_segments`
returns at it), and a module of another crate continues in that crate's finished def map, which
reads nothing of this round.

**First segment, by `path.kind`** (`DefMap::resolve_single`):

| kind | `L_0` and what else the first step adds |
|---|---|
| `Crate` | `{root}` |
| `DollarCrate(k)`, `k` this crate | `{root}` |
| `DollarCrate(k)`, another crate | nothing: no local read |
| `Super(k)` (`self` is `Super(0)`) | `{k-th ancestor of M}`; nothing when that is above the root |
| `Plain` or `Abs` in 2015, when `Abs` or the path is an import's (`ResolveMode::Import`) | `{root}` (`resolve_name_in_crate_root_or_extern_prelude`), plus `root ∈ L_1` when `s_0` is an `extern_prelude_self` name; with no segments the path denotes the root |
| `Plain` otherwise (2018 on; a 2015 visibility path, `ResolveMode::Other`) | `{M}` and every prelude target (`resolve_name_in_module`: the module, then the std prelude), plus `root ∈ L_1` for an `extern_prelude_self` name; no segments: nothing |
| `Abs` from 2018 | extern prelude only: `root ∈ L_1` for an `extern_prelude_self` name, else nothing |

Imports resolve in `ResolveMode::Import` (`DefCollector::resolve_import`), and `pub(in path)`
paths in `ResolveMode::Other` (`DefMap::resolve_visibility` through `resolve_path`). The std
prelude of another crate (the injected one: `inject_prelude` runs before the root's items are
collected, so it only ever finds a dependency's module) adds nothing, and neither do builtin
types, the `macro_use` prelude or other crates in the extern prelude. Legacy (`macro_rules!`)
scope reads are of `M` itself.

**Edges**:

1. `M -> T` for every `T` in any `L_i` (one `ModuleScope::name` read each).
2. `M -> T` for every glob target `T` (the glob reads `ModuleScope::scope` whole). A
   `#[prelude_import]` glob only sets the prelude (`record_resolved_import`), so it is not a glob
   target for `cands`.
3. `#[prelude_import]` in module `P_m` with targets `P`: `X -> P` for **every** module `X`, since
   every first segment of a `Plain` path reads `ModuleScope::name` of the prelude. Also
   `X -> P_m` for every `X`: which module the prelude is (`DefMap::prelude`, `PathRead::Prelude`)
   is the result of `P_m`'s import, so each module must come after the module that has it. A
   non-glob `#[prelude_import]` is treated the same, which is an over-approximation.
4. `is_test_harness` (`PathRead::Unscoped`): a module named `__blaze_test_harness` reads
   `ModuleData::children` of its parent, which for a collected slot is `ModuleScope::children`.
   So `M -> parent(M)` when the parent's `local_names` binds that name to `M`.
5. `declared_enum_visibility` (`PathRead::Unscoped` and a `Slot` of the declaring module): for
   `…::E::V` with `E` reached through an import, path resolution reads the scope of the module
   `D` that declares `E`. No edge is added for it. **Why it is still allowed:** `E` reached `M`'s
   lookup module `T ∈ L_i` (with `M -> T`, rule 1) either by declaration in `T` (`D = T`), by a
   plain import of `T` (whose last segment was looked up in some `T'` with `T -> T'`, rule 1), by
   a glob of `T` (`T -> T'`, rule 2), or through the prelude (rule 3). By induction `D` is
   reachable from `M`, so the order already puts `D` at or before `M`. Tracking a "declared in"
   set per name would cost a set the length of every glob chain per name (quadratic) and change
   no component. The brute-force soundness test reads `D` explicitly and checks it.
6. `M -> M` is implied and not stored: a module is always in its own component.

The extern prelude itself (`extern crate` items in the root) and the `macro_use` prelude are
crate-level state from the root's collection, not import resolution, so they add no edges.

**Not covered here, on purpose**: block def maps (one unit each, no partition), and the
`pub(in path)` of items other than `use` items (resolved at collection, before the round's
imports; `ShapeModule::other_paths` carries only `use` items').

Every rule over-approximates by taking every candidate at once: a name with two candidates (a
glob and a later explicit import, or two globs) gives both, whatever the real resolver's shadowing
picks, and the visibility filter is ignored. That keeps the graph a superset of every
intermediate state of the unit's fixed point.

## 3. Solving `cands`: one worklist, no recursion

`(module, name)` pairs are interned to `u32` (`FxHashMap<(u32, Name), u32>`, one probe per new
lookup). Facts are `Cand(pair, module)`, `Reach(level, module)` (path `j`'s level `i` looks
segment `i` up in `module`; level `len` is the denotation), `Glob(importer, target)`,
`GlobPair(pair, target)` (the pair's module globs `target`, so it takes the same name there) and
`Prelude(module)`. Each pair keeps a list of candidates and a list of consumers (path levels and
the pairs that glob it). A fact joins its list when it leaves the worklist, and a consumer that
subscribes replays the list, so each consumer gets each fact exactly once: difference
propagation, as in Andersen-style points-to solvers (Pearce, Kelly and Hankin's online
inclusion-constraint solving; Hardekopf and Lin's wave propagation). Lists are singly linked
through one `Vec<Link>` pool (8 bytes a link). Deduplication keeps the first element of each
set inline (`PairRow::first`, `level_first`) and sends only the second and later elements to one
`FxHashSet<u64>`. Most levels reach one module and most names denote at most one, so most pushes
are a load from a dense column.

The fixed point is the least one, since facts are only ever added. It terminates because every
fact names two of a finite set of IDs.

## 4. Components: Pearce's PEA_FIND_SCC2, iterative

The edges go to CSR by two counting sorts: by target, then stably by source. Each source's
targets come out ascending, and a repeat sits next to its first copy and is dropped in the same
pass. So the graph, and with it the `Partition`, depends on the edge set only, not on the order
the worklist found the edges in. That lets the `partition` query backdate whenever the edges are
the same.

Pearce (2016) keeps one `u32` per node (`rindex`). It holds the DFS index, then the low link,
then the component number, which counts down from `n - 1` as components complete. A finished
node's number is always at least any live index, so the one comparison `rindex[w] < rindex[v]`
serves both cases. The recursion becomes a stack of 12-byte frames (node, next edge, `root` bit),
plus Pearce's component stack. Components are numbered in completion order. A component completes
after every component it reaches, so the order is reverse topological. `members` is built by one
counting sort over the components, ascending by construction, and `anchor` is its first element.

## 5. Speed of light

Let `m` be the modules, `P` the paths, `S` the segments, `V = S + P` the levels, `R` the `Reach`
facts, `K` the pair lookups (a `Reach` at a non-last level, a bind, a `GlobPair`), `E` the edges
recorded and `E'` the distinct ones.

- **Floor.** The path walk must, for every module a level reaches, look the next segment up in
  that module: `R` lookups. A lookup is a probe in some `(module, name)` table, one cache line
  each (the same probe the real resolver makes per segment). The components need
  `Θ(m + E')`: one sequential 4-byte read per edge plus a random `rindex` read. That read is
  L1- or L2-resident up to 64 K modules (256 KB).
- **This design.** `K ≈ R` probes of the pair map. Pushes cost a dense-column check, except the
  second and later candidates of a set, which cost one more probe. Each fact costs one 12-byte
  push and pop and one link walk. The CSR costs four streaming passes over `E` (8 bytes an edge)
  plus scatter into `m` buckets. Pearce costs one pass over `E'` plus `m` frame pushes.
- **Arithmetic, a 1000-module crate** (6000 imports, 15 000 segments, so `V = 21 000`;
  `R ≈ 23 000`; `E ≈ 30 000`; `E' ≈ 10 000`). The pair map of ~28 K entries at 20 bytes plus a
  control byte (~600 KB) is L2/L3-resident at about 8 ns a probe: 23 000 × 8 ns ≈ 0.18 ms. Facts
  at about 2 ns each, ~60 000 of them: 0.12 ms. CSR, 4 × 30 000 × 8 B ≈ 1 MB streamed: ~0.05 ms.
  Pearce, 10 000 edges and 1000 nodes in L1/L2: ~0.03 ms. Total ≈ 0.4 ms, against a floor of the
  probes plus the components of ≈ 0.21 ms.
- **Gap and how to close it.** The extra ~0.2 ms is the pushes, pops and link walks of
  difference propagation, and the CSR passes over repeated edges. Both are sequential, so the
  measurement will tell whether they matter. Two levers are ready when it does. Number local
  names per module (a module's children are a dense range) so that a local lookup skips the pair
  map. Record each edge from its level once (`level_first` already marks the first reach), which
  cuts `E` to `E'`. The 10 % target is measured with `partition` on the oracle's crates once
  builds run (nothing is built before then). The query reruns only when an import shape changes,
  so it is off the hot path of a body edit entirely.
- **Allocations.** 21 per call, independent of edges, pairs and facts. The solver has 12 tables,
  the CSR 4 and the components 5. Each is sized once from the shape (exact for paths, levels, CSR
  and components; four facts per level for the fixed point's tables), and a table only grows, by
  doubling, on a shape past that estimate. The test holds a 16-module and a 4096-module chain to
  the same count.

State of the art matched: Pearce's PEA_FIND_SCC2 is the space-optimal Tarjan variant (one word a
node against Tarjan's two words and a bit, and Nuutila's); the fixed point is the
difference-propagation scheme of inclusion-constraint solvers. rustc and rust-analyzer have no
such partition: both resolve a crate's imports as one fixed point.

## 6. What proves it

| claim | test (`facts/tests/graph.rs`) |
|---|---|
| Pearce's components equal a recursive Tarjan's, as sets | `pearce_equals_tarjan` (proptest, 1024 graphs up to 48 nodes and 160 edges) |
| every edge goes to an equal or earlier component, and `may_read` allows it | same |
| the graph and partition depend on the edge set only | same: reversed and repeated edges give equal values |
| no recursion | `deep_chain_needs_no_recursion` (200 000-node chain) |
| glob cycle `a::* <-> b::*` is one component; chain `a -> b -> c` is three in order `c, b, a` | `glob_cycle_is_one_component`, `chain_is_three_components_in_order` |
| `super`, `crate`, `self`, `$crate` (this crate, another), 2015 and 2018 plain and `::` paths, `extern crate self as n` (root and not), `#[prelude_import]`, `pub(in path)`, the test harness, names through globs and renames, enum declarations upstream | one test each, comparing the exact modules each module reads |
| soundness: every scope read of a brute-force resolver is allowed | `partition_allows_every_read_of_a_brute_force_resolver` (proptest, 2048 random crates with editions, nested modules, enums, globs, preludes, `extern crate self`, `$crate` of two crates, the harness, `pub(in path)`) |
| bounded allocations | `partition_allocations_do_not_grow_with_edges` (≤ 21, equal for 16 and 4096 modules) |

The brute-force resolver keeps every binding a name ever gets, ignores visibility, and records
each read the rules of `path_resolution.rs` make (`Slot` of each looked-up module, the prelude,
the harness's parent, an imported enum's declaring module, a glob's whole scope). Its state is a
superset of every state the real resolver passes through, so its reads are too.

## 7. For core

- `may_read` is order-based: a read of a module reachable through edges is allowed, not only
  one with a direct edge (rules 5 and 6 rely on it).
- The prelude adds edges from every module to the `#[prelude_import]` module and to its
  targets. The unit takes `DefMap::prelude` from the `#[prelude_import]` module's component,
  which always comes first.
- `ImportShape::modules` must list every module the round's ops create, with `local_names`
  holding every `mod` child and every `extern crate self as n` of the module (the root's also
  in `extern_prelude_self`).
