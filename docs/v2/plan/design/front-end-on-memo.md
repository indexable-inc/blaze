---
status: plan
summary: Port base-db, hir-expand, hir-def, hir-ty, hir-ty-pure, hir-ty-mir and every crate above them from blaze-query's tracked engine to blaze-memo, in one batch with no second path. It covers the eight rules with site counts, the hard cases, nine write-only lanes, the compile order with its gates, what is deleted, and the cost of each edit-loop operation against its floor.
date: 2026-11-13
---
# The front end on blaze-memo

blaze-memo is the session memo: a command is its transaction, a change drops whole arenas, a hit is
proved by a key plus a read list (target decisions 1-4, decision 18). This spec ports every user of
blaze-query's tracked engine (`blaze_query::db`) to it in one batch and deletes that engine; the
per-site recipe is `agents/howto/H-port-query-to-memo.json`. Counts are `rg`/`ast-grep` searches of
the blaze-merge-1113 tree on 2026-11-13; nothing was compiled.

## A. The rules

| # | Rule | Sites | How |
|---|------|-------|-----|
| 1 | No revisions, edges or durabilities | `Durability::` 125, `.with_durability` 35, `settled =` 8, `witnessed` 14, sealed.rs 710 lines | mechanical + by hand |
| 2 | A key's home is its highest crate's arena | every key: 482 tracked, 37 input, 22 interned | by hand, per type |
| 3 | `lru`, `weigh`, `mem_tag`, `#[default]` go | 31, 19, 262, 24 | by hand, deleted with no replacement |
| 4 | Maintained states go | 2 `delta/` trees, 9212 lines; 126 setter calls | by hand |
| 5 | The recorder is the only source of read lists | `report_stored_read` 16, `capture_reads` 16, `replay_reads` 7 | by hand |
| 6 | `db::attach` becomes `blaze_memo::attach` | 301 | mechanical |
| 7 | Tracked structs become interned ids or plain values | 7 structs (3 were named) | by hand |
| 8 | One path: the tracked engine is deleted | 25369 lines | by hand |

**1. No revisions, edges or durabilities.** Dependency crates are sealed. Their arenas are never in
`next`'s changed set, so nothing has to witness that they did not change.
Mechanical: ast-grep `--pattern '$X.with_durability($D)' --rewrite '$X'` (35 matches). By hand: the
other 90 `Durability::$D` sites (setter arguments, `Durability::HIGH` tests), each deleted with its
argument position. Deleted wholesale: hir-ty-pure `sealed.rs`, `witnessed`, the cover tables and the
`settled =` hooks; sealed.rs is also where the cold-build hang lives, so the hang goes with it.
- Example, before: `hir-ty/src/providers.rs:292 #[blaze_query::tracked(returns(as_ref), cycle_result = mir_loop1, lru = 0, mem_tag = Mir)]`.
  After: `#[blaze_memo::tracked(returns(as_ref), cycle_result = mir_loop1)]`.

**2. Home.** `#[derive(Home)]` and the generated key impls feed `Homes::arena` with every id a key
names. `home_of` keeps the greatest `ArenaId`, compared as a raw u32. The index is in the high bits,
so the greatest index wins. `mark_borrows` then makes the home borrow each other arena the key names.

This equals "the highest crate in dependency order" only with two changes: **(a)** the driver
(blaze-cargo) calls `Memo::arena(index, tag)` with `index = 1 + topological position` (0 stays the
session arena); **(b)** `mark_borrows` panics, naming the key, when it marks an arena outside the
home's declared borrow closure. A key naming two unrelated crates is sound today, but it drops a whole
arena on an unrelated edit: it is a key missing its asking crate. By hand, each such key adds the
asking `Crate` field, as `StoredParamEnvAndCrate` already does.

**3.** `lru` 31, `weigh` 19, `mem_tag` 262 and `#[default]` 24 are deleted from the attribute
arguments by hand, in the same edit that changes the attribute path. ast-grep cannot rewrite inside
an attribute's token tree. Memory is bounded by arena drop, not by eviction. A `#[default]` input
field becomes the value passed to the creating `new`.

**4. Maintained states.** `nameres/delta/` and `method_resolution/delta/` are deleted wholesale,
including both `publish.rs`, `State` and `Published`. The crate being built runs per-crate queries
in its own arena; a dependency is served from its image arrays, read in place. The served-else-lowered shape stays: `fn served(db, id) -> Option<_>`, `fn lowered(db, id)`
(`hir-def/src/signatures.rs:185,191`). A crate is served from its image when it has one, else
lowered. The crate being built is served too when its surface key hit.

There is no `Kept::new(served, lowered)` in the tree. To avoid a clash with blaze-memo's flight guard
`Kept`, call this `Serve`. The 126 setter calls each map to one form in how-to item (8).

**5. One read-list source.** `blaze_memo::record` is the only source of read lists. Facts are read in
two ways:
- **Through input getters** (`input::<T>` notes the input's one Fact): FileText, IncludedFile,
  SourceRootInput, FileSourceRootInput, CrateSlot, FrontSettings, ProcessEnv, LibraryRoots,
  LocalRoots, NamedPaths, PathBucket, PrimaryCrates, AllCrates.
- **Around inputs.** Each `report_stored_read` becomes `blaze_memo::read(memo, Fact { stable id,
  digest })`: body_store.rs:283, deps/signatures.rs:490/496, check_cache/runtime.rs:553,
  nameres/frozen.rs:250/361, method_resolution/frozen/pages.rs:390/421, goal_store.rs:268,
  nameres/frozen/pages.rs:539/922/935/1043/1066/1102, nameres/frozen/entries.rs:120. Each
  `capture_reads` becomes `record(memo, ..)` and each `replay_reads` becomes nothing, since the memo
  keeps the record's list: body_store.rs:788, body_store/konst.rs:84, infer.rs:101,
  pick_cache.rs:148/156/425/428, goal_store.rs:259, global_goals.rs:225/250/262,
  interner.rs:1342/1355.

An input's fact is coarse: one fact per input. An image that is read item by item needs a getter that
notes no fact, so that its accessors' item facts are the reads. That needs a blaze-memo addition:
`#[input(parts)]`.

**6. attach.** Mechanical: ast-grep `--pattern 'blaze_query::db::attach($DB, $F)' --rewrite
'blaze_memo::attach($DB, $F)'`, 301 matches (rg finds 302 lines; the extra is a `use`).
Example (hir-ty-mir has 126): `blaze_query::db::attach(db, || ..)` becomes
`blaze_memo::attach(db, || ..)`. `with_attached_db::<D>` has 25 users (interner.rs 6, ir_print.rs 6,
blaze-lints 3, hir 2, others 1 each); blaze-memo has only the untyped `with_attached`, so it gains
`with_attached_db::<D>`.

**7. Tracked structs**, all seven in hir-def. `BlockIdLt` (lib.rs:605) and `ModuleIdLt` (lib.rs:664)
are identities: `#[interned]`, homed by their `krate`. `DefMapPair` (nameres.rs:742), `HeldRecords`
(304), `Modules` (568), `ModuleScope` (637) and `ItemBodySyntax` (expr_store/body.rs:660) become plain
values returned by the query that built them, their slices kept in the arena (`keep_slice`);
`DefMapPair` folds into `crate_resolution`.

**8. One path.** No flag and no second engine. What stays in blaze-query is what blaze-units uses:
the `Query` trait, `QueryKind`, `Cancelled`, `Cx`, `Config`, `Database` and `input`. That is
lib.rs, memo.rs, table/, segment.rs, subtree.rs, tree.rs, inputs.rs, sync.rs and kind_counts.rs,
about 10.3k lines. The tracked macros in blaze-query-macros go. A `.clippy.toml` ban on
`blaze_query::db` keeps them gone.

## B. Hard cases

**Sealed summaries.** A dependency's summary is its image, read in place. Its arena is never in the
changed set, so there is nothing to witness. `deps::key::sealed(db, krate)` (37 calls) becomes
`!memo.changed(arena)`, the same for the session. Who is sealed is the driver's decision: a crate is
sealed if it is not being built. The cold hang in `task_scope`/`settled = publish`
(sealed.rs:510-534) has no successor.

**The two delta maintainers.** Both are deleted (9212 lines). Their job was keeping a published
table current across edits, and a dropped arena rebuilds it per crate. The per-crate tables
(`TraitImpls`, `InherentImpls`, candidate slices, the slice directory, `ImplIdentities`) become
queries on the crate being built, and image arrays for dependencies. Cross-crate unions (all impls
of a trait) are queries over `deps` images. The setter-to-query mapping is how-to item (8).

**Cycle sites using the Id parameter** (57 `cycle_result`, 3 `cycle_initial`, 1 `cycle_fn`).
blaze-memo calls the fallback as `f(db, args..)` with no Id, so the `_: blaze_query::db::Id` parameter
is deleted.
Mechanical: ast-grep `fn $F($DB: $T, _: blaze_query::db::Id, $$$R) -> $RET { $$$B }` (8 matches)
and its generic form `fn $F<$$$G>(..)` (9), rewritten without that parameter. By hand: the other
~42 of the 59 Id-parameter lines (named `_id`/`id`, multi-line, or in impls). Example:
`hir-ty/src/lower.rs:2856 impl_trait_with_diagnostics_cycle_result(db, _: Id, impl_id)` becomes
`(db, impl_id)`. A fallback that used the Id to name itself in a diagnostic takes the key instead.

**`with_steps`/`db::step`** (6: sealed.rs:284, tests/scripts.rs:98 and 4 more in hir-ty-pure and
hir-ty). The step budget becomes a hir-ty-pure task-local counter, set by the caller of the pure
evaluator. Running out unwinds to that caller as a typed `OutOfSteps`. It is never a memoized
result, so no record depends on the budget. `pick_cache::with_steps` is local and stays.

**`synthetic_write`** (5: memo_properties.rs:79-80, budget.rs:135/137, benches/impls.rs:132), tests
and a bench forcing a new revision: each becomes `memo.next(&[])`.

**`Revision`** (51, 16 of them in base-db tests). `nonce_and_revision` (base-db db.rs:798,
check_cache/runtime.rs 3) becomes the snapshot epoch. `tls_cache.rs` (5) keys on `(arena,
generation)`. The revisions in body_store/entries.rs (7) and goal_store (2) are deleted: a kept
record's validity is its read list. Persisted engine ids (next_solver/persist.rs:430
`put_id(id.as_u32())`, check_cache/ids.rs) switch to stable ids: a `db::Id` means nothing across
processes. **No sound resolution keeps them.**

**Values that own memory** (a compile error in blaze-memo). `Box` ~59, `Option<Box>` ~14 and `Arc`
~12 become `&'db T` from `keep`. `ArenaMap` 7, `Vec` 4, `HashMap` 3, `ThinVec` 2 and `HashSet` 1
become `&'db [T]` from `keep_slice`, sorted by key and searched. `Interned` 2 are Copy ids already.
The 90 `#[derive(Value)]` types are checked one by one.

**`BlockIdLt` and its arena.** Interned, homed by its `krate` field (the module's crate). A block is
interned only while lowering a body of the crate being built, or read from a dependency's image, so
its home is that crate's arena. Never the session arena: a crate id there would make every session
borrow drop the world.

**The 14 `witnessed` sites** (hir-def 2, hir-ty-pure 12). They existed to prove that a
`Durability::HIGH` read did not change. Under rule 1 that is true by construction. Each call
`witnessed(db, &mut v, f)` becomes `f()`, and its `Witness` and cover use go. The 12 in hir-ty-pure
die with sealed.rs.

**Interned ids across crates, and `inputs.borrows`.**
The driver declares, for each crate arena, `borrows(dep)` per direct dependency (`next` closes over
the borrow bits, so transitive deps follow) and a borrow of the session arena (a settings change
drops all). The solver's `Stored*` types are interned in hir-ty's own next_solver table, not in
arenas, so a type naming crate X's ADT outlives X's dropped arena. **No sound resolution keeps that
table.** Smallest change: each interned node carries its home `ArenaId` (4 bytes, the max over its
arguments' homes, computed once at intern time), and the table is split per arena.

## C. Lanes

Lanes only write, in the one port worktree, each in its own whole files. The main agent compiles.
"Sites" counts the engine lines a lane touches.

| Lane | Files (whole) | Sites | Waits on | Gate |
|------|---------------|-------|----------|------|
| L1 memo+base | blaze-memo (`with_attached_db`, `#[input(parts)]`, `mark_borrows` closure check, the crate's own image record), base-db, hir-expand | 283 | none | `cargo check -p blaze-memo -p base-db -p hir-expand` |
| L2 def-nameres | hir-def `nameres.rs`, `nameres/` (deletes `delta/`) | 275 | L1 API | `cargo check -p hir-def` (with L3) |
| L3 def-rest | hir-def everything else (lib.rs, attrs*, expr_store, signatures.rs, item_scope.rs, hir/, deps/, span_names.rs) | 287 | L1 API | `cargo check -p hir-def` |
| L4 ty-solve | hir-ty `next_solver/`, `method_resolution.rs`, `method_resolution/` (deletes `delta/`) | 245 | L1, L3 `Serve` | `cargo check -p hir-ty` (with L5, L6) |
| L5 ty-lower | hir-ty lower.rs, lower/, providers.rs, mir.rs, db.rs, infer.rs, consteval*, layout/, all other files except L4/L6 | ~280 | L1, L3 | `cargo check -p hir-ty` |
| L6 ty-store | hir-ty body_store.rs, body_store/, check_cache/, tests/ | 155 | L1 `record` | `cargo check -p hir-ty --tests` |
| L7 pure+mir | hir-ty-pure (deletes sealed.rs), hir-ty-mir | 304 | L4-L6 | `cargo check -p hir-ty-mir -p hir-ty-pure` |
| L8 hir+analyses | hir 170, hir-ty-codegen 150, hir-ty-borrowck 78, hir-ty-identity 51, hir-ty-wfcheck 25, hir-ty-suspend 16, hir-ty-share 14, hir-firewall 9, hir-ty-ub 9, hir-ty-lean 7, hir-ty-matchck 6, hir-ty-unsafeck 4, hir-lint-levels 3, hir-ty-specialization 3, hir-ty-linear 2, hir-ty-contracts 2, hir-ty-{charon,coherence,print,tailck} 1 each | 553 | L4-L6 | `cargo check -p hir -p hir-ty-codegen` (pulls the rest) |
| L9 tools | blaze-lints 168, blaze-items 118, blaze-cargo 106 (arena indices, borrows, sealed set), blaze-lower 56, blaze-diagnostics 47, blaze-item-hash 39, blaze-bench 33, blaze-profile 31, blaze-units 29 (Query only, untouched but checked), blaze-macro-eval 20, span 16, blaze-fmt 7, blaze-ext-host 4, blaze-doc 2, blaze-kernel 2, blaze-{diff-mir,shader}, intern 1 each; blaze-query (delete db.rs, db/, tracked macros) | 681 | L1-L8 | `cargo check --workspace --all-targets` |

hir-def has two lanes (L2, L3) and hir-ty three (L4-L6). Their shared interface is fixed here, so
all lanes start together: L3 owns `Serve` and the `deps` image accessors that L2 and L4 call; L1 owns
every blaze-memo addition, and lanes write against the signatures in section A.

## D. Compile order and gates

1. **base-db, then hir-expand (L1).** Proves the inputs, setters, interned keys and blaze-memo
   additions type-check.
2. **hir-def (L2+L3).** Proves the tracked structs' replacements, `Serve` and the delta removal.
3. **hir-ty (L4-L6)**, `--tests`. Proves the recorder replacing capture/replay and the cycle
   signatures. No value owning memory is a post-monomorphization error, so `cargo check` alone is
   not enough: `cargo build -p hir-ty` is the gate.
4. **The hir-ty-\* analyses hir depends on, hir, then hir-ty-mir, blaze-items and hir-ty-pure (L7,
   L8),** in `cargo metadata` order. hir-ty-mir depends on hir, and hir-ty-pure on hir-ty-mir and
   blaze-items.
5. **`cargo check --workspace --all-targets`.** Proves the whole port compiles at one save point
   (whole-ports).
6. **`cargo test -p blaze-memo`** (model, resident, sim) **and `cargo test -p hir-def -p hir-ty`**
   on blaze-vm.
7. **Smoke**, on the built binary, against hello-world: cold; no-op (every surface key hits, 0
   lowered in the profile); body edit (only that crate's arena reruns); comment edit (digest
   unchanged, 0 dependents rerun). Each run has a timeout; each claim is quoted from `blaze debug
   sql` rows.

## E. Deleted

| What | Lines (`wc -l`) |
|------|-----------------|
| blaze-query tracked engine (`db.rs` 1046 + `db/`) | 25369 |
| blaze-query-macros tracked half | ≤4585 (whole crate; split unmeasured) |
| hir-def `nameres/delta.rs` + `delta/` | 5570 |
| hir-ty `method_resolution/delta.rs` + `delta/` | 3642 |
| hir-ty-pure `sealed.rs` | 710 |
| body_store revision/read plumbing (of 5428 in body_store*) | unmeasured |
| Total, measured | ≥35291 |

## F. Cost against the floor (rule 67)

Floor: an oracle redoing only changed items and their direct readers, plus unavoidable I/O. **No
ratio below is measured**; each is a design estimate, and the smoke step records the real one.

- **No-op.** Floor: stat the inputs. Here: `next(&[])` keeps every arena, then one surface-key hit per
  crate. Within 10% if a hit is one lookup.
- **Body edit (N items, D dependents).** Floor: re-lower N, recheck D. Here, decision 4 drops the
  edited crate's whole arena, so without more every item of that crate reruns: O(crate), not
  O(N+D). **Not within 10%.** The fix is the crate's own image: per-item records (key + read list +
  value) that the crate being built writes through `Resident::keep` and reads back after the drop. A
  hit makes no check (the record holds its proof), so only N+D rerun, plus one record probe per
  demanded item. That record is part of this port (L1 the memo side, L3 `Serve`), not a follow-up.
  Guess: on a 10k-item crate the probes may exceed 10% of the floor; if the profile shows it, the
  next change is arenas per file within a crate, not a fast path.
- **Signature edit.** Floor: the item, its direct readers, and their crates' readers. Dependent crates'
  arenas drop by borrow. Their records hit wherever the exported fact's digest (the firewall) is
  unchanged. Same shape and the same fix as a body edit.
- **Fresh process, then body edit.** Floor: open the images, then a body edit. Dependencies are read
  in place from their images. Without the crate's own image this is a cold build of the crate (not
  within 10%). With it, it is the body-edit cost plus one image open.
