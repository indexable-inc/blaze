---
status: plan
summary: lane items' speed of light: the bound each component's work and the hardware set (item hashes, facts, method resolution and trait selection, layout, drop and opaques), the arithmetic, today's count read from the code, and how the design reaches 90% of it.
date: 2026-10-30
---
# Lane items: speed of light

The bar is "Within 10% of the theoretical maximum, everywhere" in
[principles.md](../../goals/principles.md). Written by `speed-sol-items`. Every count of today's
code below was read from the code; nothing was run.

## The machine

The target machines are in [hardware.md](../../design/hardware.md): EPYC Turin, Graviton 5 and
Apple M-series. Unless a section says otherwise, the figures are these, not yet measured:

| what | figure |
|---|---|
| clock | 3.5 GHz |
| L1 read, streaming | 64 B a cycle |
| L2 read, streaming | 32 B a cycle (about 110 GB/s) |
| one core from DRAM, streaming | about 25 GB/s |
| a socket from DRAM | about 400 GB/s (Turin, 12 channels of DDR5 at 70% of peak) |
| cores | 16 (a dev machine), 64 (a CI machine) |
| page-cache copy (`read`, `write`, `copy_file_range`) | about 10 GB/s a core |
| NVMe write | about 7 GB/s |
| system call on an open descriptor | 0.15 µs |
| system call that resolves a path | 0.4 µs |
| `mmap` plus first touch of a 4 KiB page | 0.25 µs a page |
| `mprotect` | 0.5 µs, plus a TLB shootdown of about 2 µs on each other core running the process |
| IPC | about 2 instructions a cycle for branchy code, about 4 for straight-line integer code |
| SHA-256 with SHA-NI or ARMv8 crypto | about 2 GB/s a core |
| BLAKE3 with AVX-512 | about 6 GB/s a core |

Two more figures, from the same table: a salsa memo hit (a hash probe of the memo table and a
revision check) costs about 30 ns, and one BLAKE3 compression of a 64 B block about 40 ns.

The reference workloads:
- **A crate of 50 000 items**: about 30 000 functions, 10 000 types, 5 000 impls and 5 000 other
  items. About 500 000 references between them (a function body names about 15 items), and about
  1 MB of canonical form bytes per thousand items.
- **An edit of one function body** that calls nothing new.

## Item hashes: `crates/blaze-items/src/hash.rs`, `table.rs`

**The work.** Each item has up to four hashes: `InterfaceHash` (its signature form), `LayoutHash`
(ADTs only), `CodeHash` (its body form) and `DeepHash` (its code and the deep hashes of what it
reaches). An item in a reference cycle (a strongly connected component of the refs) is hashed as
a unit: the unit's members, ordered canonically, are hashed once, and each member's hash is that
unit hash plus its rank.

**The bound.** Each item's canonical bytes hashed once, at the hasher's rate, plus one
finalization per hash.
- Cold, 50 000 items: about 1 KB of form bytes per item gives 50 MB, or 8.3 ms at 6 GB/s.
  About 150 000 hashes at one 40 ns finalization each give 6 ms more. That is about 15 ms on one
  core, or about 1 ms on 16.
- Finding the cycles is one graph search per graph and crate: \(V + E = 50\,000 + 500\,000\)
  edges at one memo hit each, \(550\,000 \times 30\,\text{ns} = 16.5\) ms on one core.
- An edit of one body rehashes only the items whose inputs changed: the body's `CodeHash`
  (about 1 KB, 0.2 µs) and the `DeepHash` of each item that reaches it, at 32 B per ref. The
  interface and layout hashes don't change, since a plain function's interface form doesn't
  read its body (`form.rs`).

**Today, before this lane's change.**
- Each item ran its own Tarjan search over everything it reaches in its crate. Hashing a crate
  cost the sum of every item's reach. For the deep graph that is about 50 000 searches of
  thousands of nodes each, \(10^8\) memo hits or more, which is seconds.
- A cycle of \(k\) members ran the whole unit's hashing \(k\) times, once per member, so
  \(k^2\) work.
- `crate_hashes`, which `blaze-cargo`'s `reach.rs` asks of every member crate on each edit,
  read every item's key and sorted the 50 000 rows again on each edit.

**The change.**
- The unit of a cycle is a tracked memo (`interface_unit`, `layout_unit`, `deep_unit`, all
  `returns(ref)`), keyed by the cycle's first member. Every member reads the same memo, so a
  cycle is hashed once.
- The deep graph of a local crate is searched once per crate. `deep_components` runs one Tarjan
  over the crate, linear in items and refs. It reads `deep_refs`, the refs of each item that
  stay inside the crate. A body edit that calls nothing new gives the same `deep_refs`, so
  `deep_components` backdates and nothing else is searched.
- The interface and layout graphs keep the per-item search. Interface hashes are asked while a
  body is being solved (the goal store checks fact values during solving), and leaky interface
  forms (`const fn` bodies, RPIT and async hidden types, closures) read inference. A
  crate-wide search could reach the body being inferred and make a query cycle.
- `crate_items_by_key` holds the crate's items sorted by key. It doesn't re-run on a body
  edit, so `crate_hashes` rebuilds its columns with no sort and no key reads.

**After.**
- Deep graph: one search per crate, about 16.5 ms cold (at the bound). A body edit that calls
  nothing new costs one `deep_refs` re-run and a backdate.
- An edit that adds or removes a call re-runs the whole crate's search, which is the same
  16.5 ms. Keeping the components up to date edit by edit (an incremental SCC algorithm) would
  bring that to the edited item's component. That is the remaining deep gap.
- Interface and layout graphs: one search per item over its signature reach, which is about 20
  items. \(50\,000 \times 20 = 10^6\) memo hits, about 30 ms. That is about twice the hashing
  bound. Closing it is blocked on a signature graph whose forms read no inference (request
  00839, item-hash to items). With that graph, the interface search is per crate like the deep
  search.
- Dependency crates are still searched per item. They should read hashes persisted in the
  store instead.
- `crate_hashes` on each edit still reads one memo per item: \(50\,000 \times 30\,\text{ns} =
  1.5\) ms against a bound of microseconds. That is queue 00868 (dbsp): keep `HashColumns` by
  deltas.

**Proof budgets.**
- Bytes fed to the hasher per item, per hash kind.
- How many times each unit query runs, which must be once per cycle.
- `deep_components` runs per crate per edit, which must be 0 on a body edit that calls nothing
  new.
- Hashes recomputed per edit, against the reverse reach of the edited item.
- Memo reads by `crate_hashes` per edit.

## Facts: `crates/blaze-items/src/facts.rs`, `index.rs`, `lib.rs`

**The work.** A `Fact` is a row of an index (an impl, a trait item, a method name). A query
looks a fact up by its key.

**The bound.** One probe of a few cache lines per lookup, and rows within the size budgets of
[data.md](../../design/data.md).

**Today.**
- `impls` is `resolve_trait`, a memo hit, then the tracked `impls_of`, another memo hit, then
  `entry`, one probe of the sorted runs. That is three probes of one or two cache lines each,
  about 0.1 µs.
- Rows are the `Runs` and `Keyed` columns, a sorted id column and an offset column, within
  data.md's budgets.
- `Fact::key` is one BLAKE3 call. It is only used at the edge, by the API and diagnostics, never
  per lookup inside the front end.

**The gap.** None that passes 10%. The two memo hits sit in front of the probe on every lookup,
but each is in cache after the first lookup of a query. Nothing changed.

**Proof budgets.** Probes and cache lines per lookup, and bytes per row against data.md.

## Method resolution and trait selection: `crates/hir-ty/src/method_resolution.rs`, `method_resolution/`, `traits.rs`, `specialization.rs`

**The work.** A method call autoderefs its receiver (\(s\) steps, typically 1 to 3). At each step
it collects inherent and trait candidates with the method's name, then checks each candidate.

**The bound.** The candidates the receiver's simplified type can match, found by one index probe
per step: \(s\) inherent probes plus one probe for the name over the traits in scope.

**Today.**
- Inherent candidates: one `InherentImpls::in_crate_for_self_ty` probe per autoderef step and
  crate, which is rustc's shape.
- Trait candidates: one `in_crate_trait_value_items(krate, name)` lookup per crate of the traits
  in scope (about 5 to 20), then a binary search per trait in scope (about 40) of a run of a few
  entries. That is about 40 binary searches of about 20 ns, 0.8 µs per call.
- `consider_probe`, one solver goal per surviving candidate, costs several microseconds per
  candidate and dominates the call.
- `specialized_ancestor` reads the blanket impls, then the impls for its simplified self type
  (see [speed-types-probe-coherence.md](speed-types-probe-coherence.md)).
- `impl_value_item` scans an impl's items linearly with `value_item_named`. An impl has a few
  items, so this is within the bound.

**The gap.** The per-trait binary searches are about 10% of a call with one candidate, and less
with more. Iterating the name's run once, checked against a set of the traits in scope, would
make that one probe. The `TraitImpls` and `InherentImpls` readers are now dbsp-impls' to change,
so nothing changed here. `method_resolution.rs:1042`'s name sort is fix-orderkeys'.

**Proof budgets.**
- Candidates examined per call, against the candidates the simplified receiver can match.
- Index probes per autoderef step.
- Solver goals per call.

## Layout: `crates/hir-ty/src/layout.rs`, `layout/`

**The work.** A type's layout reads each field's layout once and runs the univariant or enum
calculator, which sorts the fields: \(O(f \log f)\) for \(f\) fields.

**The bound.** Once per distinct type, a pass over its fields. A crate names about 2 000
distinct concrete types, so about 2 000 layout computations at about 1 µs each, 2 ms.

**Today, before this lane's change.**
- `layout_of_ty_query` is keyed by `(type, environment)`, and `field_layout` passes the
  caller's environment down to every field.
- A concrete type such as `Vec<u8>` was laid out again, with its whole field tree, under every
  distinct environment that asked for it. Every generic body with its own bounds has a distinct
  environment.
- With about 10 000 generic functions each naming about 5 concrete types, that is about 50 000
  computations of whole trees instead of 2 000.

**The change.**
- When the type has no parameter, inference variable or placeholder (`is_global`), and every
  clause of the environment names a parameter, `layout_of_ty_query` forwards to the same type
  under the crate's empty environment.
- No such clause can match a goal about a type without parameters, or about one of its fields,
  so the answer is the same. An environment with a clause that names no parameter (a trivial
  bound such as `where u8: Copy`) keeps its own key.
- The crate stays in the key, since impls are visible per crate. rustc did the same while
  `ParamEnv::and` dropped the caller's bounds for a global value under `Reveal::All`.
- Test: `a_type_without_parameters_is_laid_out_once_for_every_environment` in
  `crates/blaze-items/src/tests/narrow.rs`.

**After.**
- A type with no parameters is laid out once per crate. Each other environment that asks
  costs one forwarding memo, about 0.1 µs.
- A type with parameters is still laid out once per environment, because its layout can depend
  on that environment.
- The remaining per-query cost is one `target_data_layout` memo hit. That is within 10%.

**Proof budgets.**
- How many times `layout_of_adt_query` and the calculator run, against the distinct types.
- Forwarding memos per type.
- Field layouts read per type, which must be one per field.

## Drop and opaques: `crates/hir-ty/src/drop.rs`, `opaques.rs`

**The work.** Drop glue and `Freeze` are answered for each type a body asks about. Opaque hidden
types are answered once per opaque.

**The bound.** Once per distinct type for drop glue and `Freeze`, a pass over its fields that
stops at the first field that settles the answer. Once per opaque for hidden types.

**Today.**
- `has_drop_glue` isn't memoized per type. Each query walks the type's tree again, with its own
  `visited` set, so a body that drops \(n\) values of one type walks the type \(n\) times. rustc
  memoizes `needs_drop_raw` and `is_freeze_raw`.
- It also walked every field after the answer was already `HasDropGlue`.
- `tait_hidden_types` returns the empty map early for an alias with no `impl Trait`.
  `define_opaque_at` runs once per file revision. `rpit_hidden_types` is one inference per
  function, which is needed anyway. These are within the bound.

**The change.** `most_glue` stops at the first `HasDropGlue` for struct fields, tuples and each
enum variant. The answer is the maximum either way. Test: `the_most_glue_is_the_max_of_every_part`,
which runs every sequence of up to four values and counts the parts walked.

**The gap.** A memo per type for drop glue and `Freeze` is queue 00867 (db), which needs query
kinds `0x2458` and `0x2459` in `db.rs`'s `kinds!`. That isn't this lane's file. With the memo,
each distinct type is walked once.

**Proof budgets.**
- Walks per distinct type for drop glue and `Freeze`, which must be one.
- Fields walked per walk.
- Inferences per opaque.
