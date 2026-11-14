---
status: superseded
summary: End state for the query engine and the front end: records in the store are the engine and the in-memory graph is a cache of what this process verified; a crate is sealed, suspect or open; name resolution and impl tables as demanded records. Not yet reconciled with the shared page in end-state.md.
date: 2026-11-12
superseded_by: v2/plan/design/target.md
---
# End state: the query engine and the compiler front end

Area: blaze-query (rows, edges, stamps, views, `Storage::holds`, the changed-set walk, seal groups), the maintainers, inputs, parsing to MIR building, the per-function store, how results reach a fresh or restarted database, memory of rows and edges. Tree read: `/Volumes/Projects/blaze-int-1112` (paths under `crates/` unless they start with `docs/`). Status words: **measured** (a run; source named), **read** (from the code or doc this session), **recalled** (another system's source or paper, from memory, not opened), **design** (not run).

## 1. The job and the invariant

1. Answer any front-end question about any item of the pinned sources by Rust's semantics: what a name finds, an item's signature, the impls under a key, a body's types, its borrow verdict, its MIR, a constant's value.
2. Keep each answer as a record with exactly what it read, so it is the answer until one of those reads differs.
3. After an edit, redo only the records a moved read reaches; the cost follows the edit, not the program.
4. Give a fresh, restarted or second-checkout process those answers by key from the store, with nothing parsed, expanded or resolved for what did not change.
5. Hold in memory only what the current request works on, and hand code generation a per-function key that exists before lowering, and facts with digests.

**The invariant.** At every pinned view, every answer given is byte-equal to the one a fresh process with an empty store computes from that view's sources and settings, whatever edits, restarts, evictions, crashes and interleavings came before; and no key, read or value holds a process-local id, an evaluation order, a path or a time.

## 2. How the best systems do it

- **Build Systems à la Carte** (Mokhov, Mitchell, Peyton Jones, ICFP 2018; recalled). A build system is a scheduler and a rebuilder. salsa, Shake and rustc keep *verifying traces* (one trace a key, checked by stamps or hashes); Bazel's and Buck2's action caches and cloud Shake keep *constructive traces* (key, digests of reads, value: several a key, shareable). **Take:** a suspending scheduler with constructive traces as the truth. **Not:** deep constructive traces (Nix), keyed by terminal inputs only: no early cutoff, so a comment edit rebuilds every dependent.
- **Buck2 DICE and Bazel Skyframe** (recalled: `dice/dice/src/impls/core/graph/`, DICE's `ProjectionKey`; Skyframe's `InMemoryGraph`, `DirtyBuildingState`). Dirtiness is pushed along reverse dependencies; values are pulled; a reader cuts off when a recomputed value is equal. A projection key is a cheap read of one part of a value, so a reader depends on the part, not the whole. **Take:** push carries only "may have changed", never computation; projections. **Not:** the whole graph resident for the daemon's life with the trace only in memory; Skyframe's restart of a function on a missing dependency.
- **rustc red-green** (recalled: `rustc_query_system/src/dep_graph/graph.rs` `try_mark_green`, `rustc_middle/src/query/on_disk_cache.rs`, `DefPathHash`). A fingerprint per result and ids stable across sessions let a new process re-validate the old graph; only some results are stored (`cache_on_disk_if`), the rest are recomputed and compared. **Take:** fingerprints, session-stable ids, two grains of result. **Not:** one graph file a crate loaded and saved whole each session; one trace a key (a branch switch loses the other branch); expansion and name resolution outside the queries, redone for the whole crate every session.
- **Relax** (read: `compiler/src/cache/deps.rs` header, `front/src/lazy.rs` `DemandImage`, `compiler/src/cache/front_run.rs`). An item's key from the parse alone: its piece (text with attributes and bodies cut), the identifiers in it, whatever is found under those names in its crate's universe, one Merkle hash a strongly connected group; only files the program's names reach are parsed; file images are written once and read in place. **Take:** a read list that exists before any resolution and covers absent names for free. **Not as the only key:** `always` nodes (globs, `as _`, unexpanded item macros reach every node), every key naming the whole sysroot (`atomic`), a crate fingerprint for a piece that invokes a macro (`coarse`), and an allowlist of macros that "name nothing" (`quiet_macro`, `deps.rs:66`): precision falls to a crate exactly where real Rust uses globs and macros.
- **DBSP and DRed** (Budiu et al., VLDB 2023; Gupta, Mumick, Subrahmanian, SIGMOD 1993; recalled) and **Zig** (recalled: `src/InternPool.zig`, dependees `namespace_name`, `src_hash`). Recursive rules (imports) and group-bys (impl tables) are updated by delta. **Take:** the delta algorithm, inside a demanded query. **Not:** a standing dataflow that runs at every write for every crate in the graph.

## 3. blaze today (read, unless marked)

**Shape.** Inputs (`base-db/src/input.rs`, `files.rs`) → two write-side maintainers that run at the end of every write batch (`base-db/src/maintain.rs:1`; name resolution `hir-def/src/nameres/delta.rs:73`, order 100; impl tables `hir-ty/src/method_resolution/delta.rs:107`, order 200) and publish their results as *more inputs* (`ScopeInput`, `PathBucket`, `InherentSlice`, `TraitImplsSlice`, `CandidateSlice`) → pull queries over rows with edges and view stamps (`blaze-query/src/db/`) → three stores of per-item results beside the rows (`hir-ty/src/body_store`, `next_solver/goal_store`, `check_cache`) → per-crate images for dependencies (`hir-def/src/deps*`, `nameres/frozen*`).

**Smells under rule 66.**
- *A second mechanism to cover the first, five deep.* `blaze-query/src/db/storage.rs:476` `holds`: seal, then durability, then summary bits, then the floor (in two forms), then reach. `seal.rs:5` gives the reason seals were added ("Three durability kinds for the whole database cannot say 'only this crate'") and `seal.rs:82` the outcome ("A seal only adds a proof"). Seals' own soundness rests on a summary bit (`LOOSE`, `storage.rs:124`).
- *A per-process key.* A read is compared by view stamp, so nothing of the graph outlives the process: `seal.rs:84` and `frontier.rs:48`, "Persistence: none". Measured (lane reth-1112, `d45e70ddc`, build 43; `docs/v2/plan/design/restore.md` "Restart reads records (d)"): reth restart with nothing changed 191.6 s, 6.81 M rows run, 144.7 M verified (21 a row run), 827 crates resolved from source.
- *Two mechanisms for one job.* The maintainers keep their own dependency index (`nameres/delta/state.rs:86` `reads: ReadIndex`) beside the engine's edges, and their own state outside the store; "a panic ... drops every maintainer's state: the next write maintains from empty" (`maintain.rs:35`). Because they are eager, a third crate state was added to stop them: `hir-def/src/deps/awaited.rs:3` (served, maintained, awaited). Measured: 36,408 inputs written on that restart, most by the maintainers publishing (`PathBucket` 15,119; `InherentSlice` 4,375; `ScopeInput` 2,793); self CPU name resolution about 72 s, impls about 53 s; `wait` spans 91,235 s of which `impls warm impl` 86,214 s. Measured on hello-world: inputs re-written with equal content re-ran 86 proc-macro expansions and 1,105 impl nodes; a scope compared unequal because it lists names in resolution order (rule 25).
- *A key made after the work.* `blaze-item-hash/src/identity.rs:79` `identity_parts` hashes the lowered, resolved body, so a hit still lowers; name reads are "covered by the identity" and never recorded (`reads.rs:183`, `Rule::Names`, `Rule::OwnBody`); a closure's id is its position. Measured: 215,072 lowerings, 55 s self CPU, on the no-change restart.
- *Per-crate keys.* A dependency's image "is keyed by every input of the crate" (`hir-def/src/deps.rs:6`); a goal's key holds "the crate's key (its identity and lang items)" (`next_solver/goal_store.rs:9`); `dep-bodies`, `dep-consts`, `impl-tables`, `item-pages`, `def-map-pages` are crate-keyed (`docs/v2/plan/design/caches.md`, 11 crate rows of 48).
- *A special-case list and a whole rebuild in place of a patch.* `deps/key.rs:91` `eligible` and `:635` `Ineligible` (local, above local, library not known); `blaze-lower/src/lib.rs:461` `Relowered::Anew`: "the caller lets the front end go and lowers anew" for three named cases.
- *Results that are pure functions of content, tracked by edges.* Measured, rows verified most on that restart: `parse_macro_expansion` 18.3 M, `ast_id_map` 18.3 M, `macro_arg` 16.6 M, `file_item_tree_query` 11.4 M: 64.6 M of 144.7 M.
- *Unbounded state.* Measured, hello-world after the idle give-back: 143 M dependency-edge rows, 1.62 GiB; `reverse.rs:16` records 267 MiB of readers for a hello-world. 24,700 item trees and 45,238 expansions are made cold for std alone.
- *Reads nobody records.* Drivers read the database outside any query; a unit records no reads (read by lanes).
- *An outcome that is not a value.* "Only passing borrow checks are stored" (`hir-ty/src/body_store.rs:13`), so a failing body is checked again each build.

## 4. The end state (design)

### 4.1 The hard question: records are the engine; the graph is a cache of them

| | Graph with stamps is the truth (today) | Store traces only, no graph (Shake, Rattle) | **Records are the truth; rows cache what this process verified** |
|---|---|---|---|
| Comment edit | 25 k to 1 M rows verified (measured, reth) | one file index remade; every demanded record of the crate probed top-down unless the store holds reverse columns | one file index remade; item hashes equal; the walk reaches 0 rows |
| Body edit under 33 dependents | 4.8 to 7.8 M rows verified (measured) | 1 function; 33 crates checked by one digest each; each probe a hash and a page lookup | 1 function; 33 group checks of one compare each |
| Restart | the graph is gone: 191.6 s (measured) | as warm, plus page reads | as warm, plus page reads; rows refill by demand |
| Memory | 143 M edges for a hello-world (measured) | pool pages | rows of crates being edited |
| Lost | sharing, restart, eviction safety | grain below an item; a verify in nanoseconds | nothing but the cache's code |

The dependency graph is kept, demoted. It may be dropped, wholly or in any part, at any moment with no change to any answer: that sentence is the design, and section 4.8 tests it. What must change for it to be true: a read is `(FactRef, digest)`, never `(node, stamp)`; every stored value is canonical bytes (a scope sorted by name and namespace), so equal digests mean equal values and a write of an equal digest is not a write.

### 4.2 The records (all on `blaze-record`: base key → read lists → answer)

| Record | Key, known before the work | Reads | Value |
|---|---|---|---|
| `FileIndex` | file content id, edition, the cfg atoms it tests | none | per item `ItemSource {attrs, header, body}` (position-free hashes), declared names, imports and globs, `mod` declarations, item-position macro calls, identifiers per piece; the line index is a separate record |
| `Expansion` (declarative) | definition tokens' hash, call tokens, hygiene, edition | none | tokens and their `FileIndex`. A proc macro's is `ExpansionRecord` of `outside.md` |
| `BindingGroup` | Merkle of its modules' declaration logs; crate settings | `Resolves{scope, name}` of earlier groups and dependencies (absent = nothing), `MacroDef`, `Node` probes for `mod` files | each module's scope, sorted, with a digest a name; traits in scope; diagnostics |
| `ItemFacts` | header hash, owner chain, settings | list 0: name closures (4.5); exact list: name facts and facts of the items named | signature, fields, generics, attribute flags, associated items, impl head, each with its digest |
| `ImplIndex` | the crate's set of impl-head digests | `ImplHead` facts | sorted `(trait, self-type head, impl)` rows with a digest a bucket; candidate names |
| `Goal` | stable form, solver version (no crate) | `Implements{trait, head}` buckets (empty included), lang items, item facts | the answer |
| `Fn` steps: inference, borrow verdict, runtime MIR, constant value | body hash, header hash, owner chain, settings, step, schema | list 0: name closures; exact list: names found and absent, item facts, impl buckets, goals, other functions' step answers, layouts; positions as separate reads | the answer, items named by item key, a closure by owner and the hash of its expression; errors are values |
| `CrateSeal` | the crate directory's Merkle id from the VFS tree, settings, its build script's step value, the front end's code id | each dependency's export digest, then the facts imported | the entry batch's id, the export digest, two reverse columns (name → items naming it; fact → entries that read it) |

No entry's key holds a crate's content (rule 65). `CrateSeal` is the coarse read in front of a crate's entries and the name of their sorted batch (`disk.md`); it is the request certificate one level down. Code generation's `inst` key takes the `Fn` base key; the MIR answer it used is one of its reads.

### 4.3 The engine: one rule at two grains

A group is a crate. Its states: `Absent → Sealed ⇄ Suspect`, `Sealed | Suspect → Open → Sealed`.
- **Sealed**: the seal record held. No row, no edge, no input a file. A question about one of its items is an entry read from pool pages by the item's key.
- **A write** to one of its files (the input's group word, one load, as `seal_wrote` does now) makes it Open and its dependents Suspect. Nothing else happens at the write.
- **Suspect**, first asked: compare each dependency's export digest; if one moved, the imported facts; all equal: Sealed, and the new coarse digests are filed as another read list under the same key. One moved: Open, with those facts as roots.
- **Open**: rows exist only for entries that were asked and missed, or that the walk reached. The walk starts at the file index's changed items and the moved facts and follows `fact → readers` (the batch's reverse column read in place, plus lists for rows made since). An entry it does not reach is still found by its own key. Rows carry stamps; a recomputed value with an equal digest stops the walk.
- **Sealing**: at the end of the second request in a row with no write to the group, at idle and at stop: loose entries are merged into a new batch, the seal record is written, rows and edges are freed.

`holds(row)` is: its group's seal holds, or this view's walk did not reach it. Both are the rule "every read's digest is equal", proved once for a group or once for a row. Durability is the seal with three groups; the floor is the walk's end; summary bits and `LOOSE` exist because a row's group is inferred from its edges. In the end state a row's group is in its key, a read of a group not below the reader's own is a typed refusal naming both, and plan-level inputs are fine facts of a `plan` group (`Settings(crate)`, `Dep(crate, name)`), so another crate's cfg moves no digest a crate read. One view a request: build-script results and macro modules are read as step records' values, never written as inputs mid-build (measured today: `Crate.cfg_options` of libc re-ran 10,444 memos).

A query is one of three kinds, declared: **record** (a row of 4.2), **scratch** (memoized for the task; its reads are flattened into the record that asked, `Reads::for_each_read_through`; freed at task end), **content** (a pure function of bytes named by its key: no edges, no stamp, nothing to verify). Parse, item tree, id map, macro argument and declarative expansion are content queries: the 64.6 M verifies go by construction. A driver holds no database handle outside a tracked frame, so its reads fold into the request's certificate.

### 4.4 Push and pull: one model

The maintainers' algorithms stay; their place changes. This is where `end-state.md` (review point 7, "the maintainers stay") is wrong for this area. Nothing runs at a write. Name resolution is the `BindingGroup` record and the impl tables are the `ImplIndex` record, both demanded. The engine gains two general capabilities and no special case:
- **A delta run.** A record query may be given its previous value and which of its reads moved (`run(prev, moved)`), under the contract `run(prev, moved) == run(none)` in bytes. DRed's over-delete (`nameres/delta/run.rs`, `state.rs`) and the impl arrangement (`method_resolution/delta/state.rs`) become such runs. The previous value is a record, so it survives a restart and a panic.
- **A projection.** A read may name a part of a value (`Resolves{scope, name}`, `Implements{trait, head}`) and compares that part's digest, kept in the value. An added impl moves one bucket's digest and reaches that bucket's readers.

Push remains only as "may have changed" along reverse columns. `Awaited` has no reason to exist once nothing is eager.

### 4.5 An item's key before resolution, and where semantic identity stands behind it

The base key is the item's own content (`ItemSource` hashes, owner chain, settings). Its **list 0** is Relax's idea made a read list (credited: MIT): one read `NameClosure(universe, name)` for each identifier in the item's piece or body, whose digest is the Merkle hash of every piece found under that name in the crate and its dependencies (a column of each `CrateSeal`); plus `Imports(module)` for each enclosing module (its `use`, glob and `macro_use` declarations: finer than Relax's `always`); plus one `Implicit(universe)` read for what syntax names without an identifier (operators, `?`, `for`, `.await`, autoderef, drop, closures: the lang traits' blanket impls outside the sysroot); identifiers of an expansion are those of its `Expansion` record. Evaluating list 0 resolves nothing. If every digest is equal, the answer filed under it is taken.

If list 0 moved (a common name such as `new` moves often), the **exact list** is checked: the facts the step read, in `blaze_items::Fact`, negatives included. Semantic identity stands there, and in four more places: values name other items by item key; a dependency's new version with equal facts must hit; items a proc macro wrote; generic instances. A name closure is never a key and never the only list.

### 4.6 A fresh or restarted process

It computes a crate's seal key from the plan and the VFS tree's directory id (already persisted; no file read) and looks it up. A hit: Sealed, zero rows, zero inputs a file. A miss: the crate is Open, and each entry asked is found by its own key, so a crate with one edited file since the seal recomputes that file's index and the entries its changes reach. There is no conversion between "crate grain" and "item grain": entries are always per item, and a seal is only the check that skips them. The first write to a file moves the directory id, the seal key misses, and that is the whole transition. The toolchain ships std's batches and seals.

### 4.7 Memory

Held between requests: rows and forward edges (4 bytes an edge) of Open groups, in pool-backed columns that can be evicted because every row is re-derivable from its record; ids are dense handles into a table of item keys for Open groups only. Sealed groups: nothing. Scratch: the task's frame (rule 11). Values: pool pages pinned for the length of a read (rule 51). A hello-world then holds the rows of one crate. Target (design, not measured): under 1 M edge rows and 32 MiB of engine state, against 143 M and 1.62 GiB.

### 4.8 The one soundness test

`/// The refinement:` a Hegel simulation in which the real front end (and, in a second instantiation, abstract queries over `blaze-query` alone) runs over generated crates (modules, globs and glob cycles, impls, blanket impls, macros that write items, cfg) under random steps: edits (body, signature, item added or removed, a name that shadows through a glob, a comment, a moved line, a file added, a cfg flipped, a dependency changed), demands of random records, restarts (memory dropped, store kept), evictions (any rows, edges, seals or pool pages dropped; any store record deleted), seal and unseal, crashes that tear unsynced writes, shuffled completions, a second checkout on the same store. **Oracle:** after every step, each demanded answer's digest equals that of a fresh process with an empty store on the current sources. **Bound:** records recomputed are within the set whose exact reads changed (a comment edit recomputes 0). **Planted bugs**, each caught under many seeds: a negative name read dropped; `Imports` left out of list 0; a scope digested in resolution order; a position in a `Fn` key; a write that does not open its group; a seal accepted with one import unchecked; a missing page read as empty; a delta run that differs from a full run. On real projects the same oracle is `Mode::Verify` over a replay of reth's history.

**Rules.** 3: a missing record is a miss that runs the work; a record that does not read, two answers under one key and digests, a read out of the group order, or a delta run unequal to the full run under `Verify` is an error naming the item; no `Anew`. 14: the group machine, the walk and lookup are sans-IO; the store and pool are drivers. 21: every read is in a list or the work is refused naming the read. 25: canonical values; no id, order or stamp in a record. 43: 4.8. 50/51: 4.7. 65: 4.2.

## 5. Deleted, and what survives

**Deleted:** durability levels, summary bits, `complete_summaries`, `LOOSE`/`POISONED`, the floor (`frontier::floor`, `floored`); view stamps as the comparison of a read; `base-db/src/maintain.rs`'s pass and `Db::batch` maintenance; every published input (`ScopeInput`, `PathBucket`, `InherentSlice`, `TraitImplsSlice`, `CandidateSlice`, `SliceDirectory`, `CrateResolution`, `LibraryResolutions`); `deps/awaited.rs`, `eligible`/`Ineligible`, `lower_awaiting`, `Relowered::Anew`, `withdraw_moved`; dependency images, `frozen` def maps and their pages as a second form, `BLAZE_DEF_MAP_PAGES`, `BLAZE_ITEM_STORE`; `check_cache`; `identity_parts` over the lowered body and `Rel::Target`; the crate key in a goal's key; `dep-bodies`, `dep-consts`; the memos part of the daemon image; `file_item_tree_query`, `parse_macro_expansion`, `ast_id_map`, `macro_arg` as edge-tracked rows; "only passing borrow checks are stored".
**Survives:** `blaze-record`; rows, flights and value-in-place (`row.rs`); the walk and the reverse index (`frontier.rs`, `reverse.rs`) for Open groups; seal groups as the group machine; backdating; `row_reads`; DRed and the import graph's components (`nameres/delta/{run,state,diff}.rs`, `facts/graph.rs`) as a delta run; the impl key and arrangement; `blaze_items::Fact`; the goal store's stable form; `ItemSource`, the file index and macro-definition entries; `Pinned::dir_id`.

## 6. Order of building

| Step | What | Judged by (measured how) |
|---|---|---|
| E0 | Counters and the test, no design change: hits per branch of `holds`; binding-group sizes from `facts/graph.rs` `partition`; name fan-out; rows by kind; the refinement harness of 4.8 run on today's tree | the rows exist for core, std, reth (`blaze debug sql`); the harness finds its planted bugs |
| E1 | The `Fn` record keyed before lowering, names as recorded facts, list 0, closures by content, verdicts and errors as values; `check_cache` deleted. Largest measured cost one contained change removes, in two crates | reth: edit one function, every function asked: N−1 hits, 0 `Body::of` spans; no-change restart lowerings 215,072 → 0 (55 s self CPU) |
| E2 | Content queries and canonical values: `FileIndex` and `Expansion` the only route; a write of an equal digest is no write; scopes sorted | hello-world second run: 0 expansions and 0 impl nodes re-run (86 and 1,105 today); reth restart rows verified 144.7 M → under 80 M |
| E3 | Delta runs and projections in the engine; `BindingGroup` and `ImplIndex` demanded; the maintain pass, published inputs and `Awaited` deleted | reth restart: `nameres crate` spans for unchanged crates 827 → 0; inputs written 36,408 → plan-level only; no `wait` span on impls |
| E4 | `CrateSeal` and the group machine; Sealed holds no rows; std's batches installed | hello-world idle: edge rows 143 M → under 1 M; reth first edit after a restart equals the same edit warm |
| E5 | Durability, summaries, the floor deleted, one at a time, each after the census and with the refinement green | reth comment edit: rows verified at most the items of the file (25 k to 1 M today) |

E1 needs `blaze-record` compiled (track A1). E3 and E4 are decided after E0's group sizes.

## 7. Unsound, unknown, or to measure first

1. **Is list 0 sound for all of Rust?** Relax does not compile all of Rust (`KNOWN_GAPS.md`). The risk is a dependency no identifier shows (an implicit lang trait, an auto trait, a coercion). Answer: the `Imports`-dropped and implicit-name plants in 4.8, and `Verify` on every list-0 hit over reth's history; until that is green, list 0 only orders the checks and the exact list decides.
2. **How often does list 0 hold?** Fan-out of `new`, `from`, `default` on reth. Answer: E0's counter of closure sizes; if most items' closures cover most of the universe, list 0 pays only for body edits (bodies are cut, so those always hold).
3. **How large are binding groups?** Not measured. If a crate is one group, the delta run carries name resolution's whole incremental cost and its equality contract is the main proof burden. Answer: E0's histogram.
4. **What do summaries prove that seals and the walk do not?** Answer: E0's census per `holds` branch on reth and the hello-worlds, read with the walk's state at the time of each hit.
5. **What does a record probe cost through the pool, and how many records is reth?** 6.81 M rows today. Answer: a bench of `blaze-record` lookup on a warm pool; the record/scratch line of 4.3 is drawn so a crate is 10^4 to 10^5 records.
6. **How long are exact lists for inference?** A body's trait goals may read thousands of buckets. Answer: bytes a `Fn` record on reth after E1; if large, a body reads `Goal` answers and not their facts.
7. **Crate-wide checks** (coherence, orphan rules, unused items): which record owns each? Not designed; listed from `blaze-diagnostics` before E4.
8. **Reads through a proc macro's output** depend on the `State` read of `outside.md`; an item a stateful macro wrote has no key until the call's record is found. Count such items on reth.

## 8. How to cut it

One mind, in order: the engine core (`blaze-query/src/db/{storage,function,seal,frontier}.rs`: delta runs, projections, the three query kinds, the group machine, then the deletions of E5), and with it the `Fact` vocabulary and the canonical form of each value (`blaze-items`). Everything else is whole in a few files and written against those two interfaces once they compile:
- `FileIndex`/`ItemSource` and declarative `Expansion` (`hir-expand/src/declarative/stored*`).
- The name scanner and closures for list 0 (new, in `blaze-item-hash`; from Relax's `scan`).
- The `Fn` record (`hir-ty/src/body_store/{record,facts,infer,mir,konst}.rs`, `blaze-item-hash/src/{identity,keys,reads}.rs`).
- `BindingGroup` (`hir-def/src/nameres/delta/{publish,run,state}.rs`); `ImplIndex` (`hir-ty/src/method_resolution/delta/{publish,state,read}.rs`): one builder each, after the delta interface.
- `Goal` without a crate in its key (`next_solver/goal_store/{stable,record}.rs`).
- `CrateSeal` and the deletions in `hir-def/src/deps/` and `blaze-lower/src/lib.rs` (hunks by anchor: `lower_awaiting`, `lower_writes`, `relower`).
- The generator and oracle of 4.8 (tests only; first, so every other piece lands against it).
