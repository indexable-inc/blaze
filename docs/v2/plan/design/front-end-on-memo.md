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
| 3 | `lru`, `weigh`, `#[default]` go; `mem_tag` stays (port decision 7) | 31, 19, 24; 262 kept | by hand, deleted with no replacement |
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
A key that names no arena is homed at the asking snapshot's own session arena (`Memo::session`, at
the generation that snapshot holds). There is no constant `ArenaId` (`ArenaId::SESSION` was removed
2026-11-14: it named generation 0, and after the first commit that remade the session every key
homed there panicked `arena 0.0 is dead`). Session-level data names nothing (`None` of an
`Option<ArenaId>`, 4 bytes by the niche); an index is `ArenaId::SESSION_INDEX`; a driver's maker
of session inputs names the arena it is making with `Builder::session()`.

This equals "the highest crate in dependency order" only with two changes: **(a)** the driver
(blaze-cargo) calls `Memo::arena(index, tag)` with `index = 1 + topological position` (0 stays the
session arena); **(b)** `mark_borrows` panics, naming the key, when it marks an arena outside the
home's declared borrow closure. A key naming two unrelated crates is sound today, but it drops a whole
arena on an unrelated edit: it is a key missing its asking crate. By hand, each such key adds the
asking `Crate` field, as `StoredParamEnvAndCrate` already does.

**3.** `lru` 31, `weigh` 19 and `#[default]` 24 are deleted from the attribute
arguments by hand, in the same edit that changes the attribute path. ast-grep cannot rewrite inside
an attribute's token tree. Memory is bounded by arena drop, not by eviction. `mem_tag` stays (port
decision 7): each run of a query's code is a `blaze_mem::frame::task` of its tag, as under
blaze-query, so a body's `frame::current` scratch has a frame and is counted under its subsystem
(`blaze_memo::plumbing::Query::TAG`, `Tag::Query` by default; one frame a run, tens of
instructions and no allocation when the run takes nothing from it). A `#[default]` input
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

**Sealed summaries.** A dependency's summary is its image, read in place. Its arena is in the
changed set only when the driver writes the crate (a build script's env, a gift), and then it is
dropped with its dependents like any arena, so there is nothing to witness. `deps::key::sealed(db, krate)` (37 calls) becomes
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

**A start gives what it found.** A slot gift (`ImageSlot`, `PagesSlot`, `DefMapSlot`,
`SurfaceSlot`) drops its crate's arena and every arena above it, so a gift given after queries
ran throws them away. A crate lowered in this session stays lowered (`deps::Lowered`). A crate a
fresh database will serve is one whose found record (plan key unchanged, this front end) names
an image filed under the crate's key alone: the record holds that key (`found::Found::base`,
layout 2), and blaze-cargo's `deps::find` knows it before any query runs. `deps::makers` gives
those images, with their impl tables and kept roots, in the commit of the session's makers
(`deps::serve_found`). The first serve walks each crate's key as always: equal to the base, the
crate is kept (no fetch, no gift, no commit); another, it is fetched under the key walked, or
withdrawn, before any task reads it. A crate filed by its reads is left to that serve, which
values them. Floor: one making of each served arena per start; the store asked once a crate.
Not yet at it: a fresh start's `give_surfaces` commit still drops every arena once more (each
crate's `SurfaceSlot` gift moves from none). End state: the surface is a tracked query of the
crate's own arena with the memo's cutoff, no gift, so nothing drops an arena for a surface,
and a cfg write drops only the arenas whose surface moved.

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

## L3 API, as written

What lanes L2, L4 and L9 call in hir-def (lane L3's files), as written on 2026-11-13. Paths are
`hir_def::...`.

**Served-else-lowered.** `signatures::Serve` (`crates/hir-def/src/signatures.rs`):

```rust
pub enum Serve<'db, T: ?Sized> { Served(&'db T), Lowered(&'db T) }   // Copy, Deref<Target = T>
impl<'db, T: ?Sized> Serve<'db, T> {
    pub fn new(served: Option<&'db T>, lowered: impl FnOnce() -> &'db T) -> Serve<'db, T>;
    pub fn get(self) -> &'db T;
    pub fn is_served(self) -> bool;
}
```

The shape per item kind is two queries, both homed by the item's id (its crate's arena):
`fn served<'db>(db: &'db Db, id: Id) -> Option<&'db V>` (`returns(copy)`, reads the image in place
and `keep`s the value) and `fn lowered<'db>(db: &'db Db, id: Id) -> &'db V` (`returns(copy)`), joined
by `Serve::new(served(db, id), || lowered(db, id))`. A crate is served exactly when
`deps::image_of(db, krate)` is `Some`: the driver gives an image to each dependency it maps and to
the crate being built when its surface key hit (its own image, spec F), so one predicate covers both.

**Images** (`crates/hir-def/src/deps.rs`):

```rust
#[blaze_memo::input(parts, debug)]
pub struct CrateImage { #[returns(ref)] pub image: Image }            // notes no fact
pub struct ImageSlot;                                                 // the crate slot kind naming it
pub fn image_slot_fact(krate_id: ContentId, mode: Mode) -> blaze_memo::Fact;
pub fn give_image(inputs: &blaze_memo::Inputs<'_>, krate: Crate,
                  krate_id: ContentId /* stable crate id */, image: Image);   // its lang items, def map, pages slots too
pub fn serve(db: &Db, crates: &[Crate], build: ContentId, images: &dyn ImageStore, mode: Mode)
    -> Result<(Served, Vec<(Crate, Image), Global>), String>;    // the driver gives each image
pub fn withdrawn(db: &Db, served: &[Crate], files: &[base_db::FileId],
                 roots: &[base_db::SourceRootId]) -> Vec<Crate, Global>;   // `Served::withdrawn`
pub fn image_of(db: &Db, krate: Crate) -> Option<&Image>;             // was a query returning &Option<Image>
pub fn is_served(db: &Db, krate: Crate) -> bool;
pub fn image(db: &Db, krate: Crate) -> Option<ImageRef<'_>>;
pub fn mode(db: &Db, krate: Crate) -> Option<Mode>;
pub fn note_item_read(db: &Db, domain: &str, item: ContentId, record: &[u8]);
```

Every accessor that reads one item of an image in place calls `note_item_read` (item = the stable
id the image's index finds it by, the digest of the record's bytes in `domain`): that is the
`blaze_memo::read(memo, Fact { stable id, digest })` of rule 5. `DepImages` (the singleton map of
every crate) is deleted: the driver holds what it served; a query reads only its crate's slot.
`Image` holds the parts name resolution reads (`lang_items: Frozen`, `def_map: Option<Frozen>`
under check and verify, `pages: Option<Paged>` under serve and verify), which `give_image` gives as
the crate's own slots. The driver calls `withdrawn` with the files and roots a snapshot changed and
gives no image to a crate it names. `a_maintained_crate_is_not_served_again` is deleted with the
delta maintainers; `GIVES_EVERY_CRATE` is deleted with `give_changed`: a test reads each crate's
slot after `build_next`.

**Crate slots** (`crates/hir-def/src/deps/given.rs`), given by the driver while it builds a
snapshot, never during a build (`give_changed` is deleted):

```rust
pub fn give<K: 'static, T: Any + Send + Sync>(inputs: &blaze_memo::Inputs<'_>, krate: Crate,
                  fact: blaze_memo::Fact, value: T);      // asserts `inputs` is krate's arena
pub fn give_each<K: 'static, T: Any + Send + Sync>(builder: &blaze_memo::Builder,
                  entries: impl IntoIterator<Item = (Crate, blaze_memo::Fact, T)>) -> usize;
pub fn surface_fact(krate_id: ContentId, surface: ContentId) -> blaze_memo::Fact;
pub fn give_surface(inputs: &blaze_memo::Inputs<'_>, krate: Crate, krate_id: ContentId, surface: ContentId);
pub fn surface(db: &Db, krate: Crate) -> Option<ContentId>;           // unchanged
```

`set_surface` is deleted (`give_surface` replaces it). L2's `frozen::set`, `frozen::set_lang_items`,
`pages::set` and L4's `method_resolution::frozen` give their slots with `give_each`.

**Sealed** (`crates/hir-def/src/deps/key.rs`): `pub fn sealed(db: &Db, krate: Crate) -> bool` is
`!memo.changed(krate.gid().arena())`. `sealed_in` is deleted (its walk memo has nothing left to
memoize); its callers call `sealed`.

What L3 assumed of L1 (base-db, blaze-memo), to be checked against "L1 API, as written":
`Memo::changed(&self, ArenaId) -> bool` (the arena was made anew in this snapshot);
`#[blaze_memo::input(parts)]`; `blaze_memo::Builder::inputs(&self, ArenaId) -> Inputs<'_>` (exists);
`base_db::Crate: blaze_memo::Id`, homed in its crate's arena; `Crate::given::<K, T>(self, &Db) ->
Option<&T>` unchanged; `Crate::give::<K>(self, &blaze_memo::Inputs<'_>, Fact, Given)` at snapshot
build; `blaze_memo::Home for blaze_ids::ContentId` (a key argument of `image_key`).

**Ids and homes** (`crates/hir-def/src/lib.rs`). `blaze_memo::Id` (`gid`, `from_gid`) and `Home`
(homed in the item's crate's arena) on every interned id hir-def defines: `FunctionId`,
`StructId`, `UnionId`, `EnumId`, `EnumVariantId`, `ConstId`, `StaticId`, `TraitId`,
`TypeAliasId`, `ImplId`, `UseId`, `ExternCrateId`, `ExternBlockId`, `Macro2Id`, `MacroRulesId`,
`ProcMacroId` (all by base-db's `impl_intern_key!`, L1's macro, over a loc that is `Home`;
`Option<Gid>` is 8 bytes, so a scope `Entry` is 36 and `PerNs` 120),
`BuiltinDeriveImplId`, and `BlockIdLt<'db>` / `ModuleIdLt<'db>` (`#[blaze_memo::interned]`, homed by
their fields: the module's crate). `#[derive(blaze_memo::Home)]` (no `Id`: they are enums or
structs of ids) on `AdtId`, `MacroId`, `ItemContainerId`, `ModuleDefId` (`BuiltinType` skipped),
`GenericParamId`, `DefWithBodyId`, `AssocItemId`, `GenericDefId`, `ExpressionStoreOwnerId` (was
`NamesIds`), `CallableDefId`, `AttrDefId`, `VariantId`, `FieldId`, `TupleId`, `TupleFieldId`,
`TypeOrConstParamId`, `TypeParamId`, `ConstParamId`, `LifetimeParamId`, `HrtbLifetimeParamId`,
`LoweringMode`; each loc (`ItemLoc<N>`, `AssocItemLoc<N>` by hand, by their container;
`EnumVariantLoc`, `Macro2Loc`, `MacroRulesLoc`, `ProcMacroLoc`, `BuiltinDeriveImplLoc`, by their
container, ast ids skipped). Not hir-def's: `ExprId`/`PatId` (`la_arena::Idx`) and `HirFileId`
(hir-expand) need theirs where they key a query.

**Session words and filed bytes** (`crates/hir-def/src/hir/persist.rs`). `Gid` is process-local:
`session_word(Gid) -> u64` (`Gid::session_bits`; never 0 for a crate-homed id), `session_id(u64)
-> Option<Gid>` (0 is `None`), `decode_session_id(bits, at, what) -> Result<Gid, FormatError>`.
Bytes holding one are `SessionBytes` (`SessionBytes::of(&value)`), readable back in this process
only; what is filed, put in an image or kept past the process is `Portable`, made only by an
encoder whose every id went through a `Relocation` table or a portable id, or by
`deps::signatures::table::relocate_store(SessionBytes, &mut Relocation) -> Portable`.
`deps::write_image(key, [&Portable; Part::ALL.len()])` takes only `Portable`.

**Syntax contexts** (port decision 3). A context is a word of one crate's arena
(`span::hygiene`), so a persisted form never holds its word. The relocation table writes each
context by structure (`table::encode`/`decode` through `nameres::frozen`'s `Enc::context`/
`Dec::context`, interned into the reading crate's arena); `frozen` keys its context rows by
`(ArenaId, SyntaxContext)` and refuses a call whose arena is not its crate's. The item tree's
`MacroCallRec.ctxt` (record 2) is a row of the tree's own context table (`Root.contexts_at`,
schema 7), each row coded by `Code for SyntaxContext` (the table's index inside `relocating`).
A lookup names its arena: `item_tree::lower::visibility_from_ast(db, table: ArenaId, node,
span_for_range)` (the file's `HirFileId::context_arena()`), `Resolver::ctx_table(&self) ->
ArenaId` (the resolver's crate), and bodies use `current_file_id().context_arena()`.

**Test support** (`crates/hir-def/src/test_db.rs`). `TestDBExt::executed_in_read_order(&self, f)`
and `executed_in_write(&mut self, f)` return the names of the queries that ran
(`blaze_memo::testing::RunLog`, `Ran::key()`), read order `LeftToReaders`. The row namer and
`name_log_rows` are deleted. `span_names::span_text` reads unrecorded through
`blaze_memo::unrecorded(memo, f)`. `expr_store::body::own_syntax_read(db, def, &Read) -> bool`
(the read is the body's own file text) is provisional until L9b gives its signature.

**Values that own.** `#[blaze_memo::tracked(.., owns)]` on the queries whose value is a tree or
owns heap memory (item trees, bodies and source maps, scopes, lang and diagnostic items, import
map, unstable features, attrs' `Box`/map values, syntax values); flat ones are `keep_slice`d
(`crate_notable_traits`, `body_items`, `include_macro_invoc`, `get_constrained_lifetimes`).

## L1 API, as written

What lanes L2-L9 call in blaze-memo (lane L1's files), as written on 2026-11-13. Paths are
`blaze_memo::...`. Re-read this section: base-db's part is appended below as it is written.

**(a) Typed attach** (`attach.rs`). `attach` keeps blaze-query's bound (a sized `'static`
database type), so rule 6's rewrite keeps every caller:

```rust
pub fn attach<D: Database + 'static, R>(db: &D, f: impl FnOnce() -> R) -> R;
pub fn with_attached<R>(f: impl FnOnce(Option<&Memo>) -> R) -> R;
pub fn with_attached_db<D: Database + 'static, R>(f: impl FnOnce(Option<&D>) -> R) -> R;
```

`with_attached_db::<D>` is `Some` only where the attached database's type is `D` exactly. The
attached record is carried with the task across fiber waits (`blaze_graph::sched::carry`).

**(b) `#[input(parts)]`** (`blaze-memo-macros`, `input.rs`). `#[blaze_memo::input(parts)]` (also
`parts, debug`): every getter notes no fact; the accessors call `blaze_memo::read(memo, Fact {..})`
per item. The input's own fact is still there for a key or a print:

```rust
pub trait Input { const PARTS: bool = false; .. }      // `true` under `parts`
pub fn input_fact<T: Input>(memo: &Memo, id: Gid) -> Fact;   // notes no read
```

Every `#[input]` also gets `impl Stable for X` (its fact id), so an input may key an imaged query.

**(c) Declared closure and the index contract** (`memo.rs`, `arena.rs`, `home.rs`).

```rust
impl Builder { pub fn arena(&mut self, index: u32, tag: Tag) -> Inputs<'_>; }   // unchanged
impl Inputs<'_> {
    pub fn borrows(&self, from: ArenaId);   // panics unless from.index() < this index
    pub fn seal(&self);                     // see (e)
}
```

The contract: a crate's arena index is `1 + its topological position` (dependencies first); 0 is
the session arena. The driver calls `borrows` for each direct dependency and for the session
arena. `build()` closes every new arena over its declared borrows (each dependency's closure
included) and panics where a declared index is not held. A key, input or interned value homed
at arena H that names an arena outside H's closure (H itself counts) panics, naming the key, its
home and the arena: "add the asking `Crate`" (`mark_borrows`, every fetch, intern and probe).
The session arena is in a closure only where declared. Plant: `Plant::MarkOutsideClosure`.

**(d) The crate's own image** (`image.rs`, `blaze_keyed::kind::MemoEntry`).

```rust
#[blaze_memo::tracked(image, returns(copy))]   // `image` is the new option
// key: every argument `Stable`; value: a `blaze_format::Record` with `PLAIN` (a const assert)

pub trait Stable { fn stable(&self, memo: &Memo, out: &mut StableKey); }
// impls: ints, bool, char, (), str, ContentId, Fact, &T, [T], [T; N], Option<T>, tuples to 8,
// every #[input]. An interned id has none (its index follows timing).
pub struct StableKey(..);   // bytes(&[u8]), u64(u64), id(&ContentId)

pub trait Image: Sync {
    fn tool(&self) -> blaze_keyed::kind::ToolId;
    fn record(&self, arena: u32, key: &blaze_keyed::kind::Key<MemoEntry>) -> Option<&[u8]>;
    fn now(&self, id: ContentId) -> Option<ContentId>;   // a fact's digest in this snapshot
    fn query_fact(&self, memo: &Memo, row: Read) -> Option<Fact>;   // a boundary row's stable fact
}
pub struct ImageOf<'a, M, F, Q> { pub tool: ToolId, pub records: &'a M, pub now: F, pub query: Q }
// impl Image for ImageOf<M: blaze_keyed::memo::Memo + Sync, F: Fn(ContentId) -> Option<ContentId> + Sync,
//                        Q: Fn(&Memo, Read) -> Option<Fact> + Sync>

impl Memo {
    pub fn serve<R>(&self, image: &dyn Image, f: impl FnOnce() -> R) -> R;
    pub fn file_records<M: blaze_keyed::memo::Memo>(&self, snap: blaze_keyed::proof::Snapshot<'_>,
                                                   records: &mut M) -> usize;
}
pub struct Imaged<K, V>;   // made by the macro: `Imaged::<Key, Value>::new()` (const)
```

The record kind is `MemoEntry` (tag 20, Traced root: reads, no facts, payload = the value's
bytes), keyed by `(toolchain, query kind id, key's Stable bytes)`, filed per arena index. Where a
tracked fn consults it: in `fetch`, on a miss, inside the flight, before the run: the record is
looked up through the `Image` lent by `Memo::serve`; found, its read list is checked once
against `Image::now` (decision 18: once per item and snapshot; after that it is an entry and
every hit is a plain lookup); every digest equal and the value is published as the entry with
that read list and replayed into the asker; a moved digest and the query runs, and the run's
record is queued. `file_records` (the driver, between commands, holding its `Resident`) keeps each
through `Resident::keep` with the proof of `blaze_keyed::proof::hit`. Outside `serve` nothing is
looked up or filed. Cycle values are never filed. A run of an imaged query may read only arenas
in its home's declared closure (it panics otherwise): a served entry sets no borrow bits.
`serve` needs `Reads::Kept`. Counters: `image_checks`, `image_served`, `image_redone`,
`image_filed`, `image_untracked`. Plant: `Plant::ImageSkipsVerify`. A filed list is stable facts:
an input row's fact, a boundary row through `Image::query_fact` (panics naming the row where the
image translates none), sorted, merged; a list with an untracked row is never filed. A served
entry's rows are `Read::Input { kind: IMAGE_FACT, fact }`, one per fact of the record.

**(e) Changed** (`memo.rs`).

```rust
impl Memo { pub fn changed(&self, arena: ArenaId) -> bool; }   // !sealed
```

`changed` is `false` exactly for an arena the driver sealed (`Inputs::seal`: a crate not being
built). `build()` panics where a sealed arena declared an unsealed dependency (other than the
session), so `!changed` means what `deps::key::sealed` meant: the crate and all its dependencies
are sealed. `next()` takes a sealed arena in the changed set like any other (2026-11-14): every
read and declared borrow sets a borrow bit, so the dropped set is closed under its borrowers,
and the remade arena is sealed again. This is not "made anew
in this snapshot": L3 assumed that wording, but `sealed` is `!changed` and a crate being built
whose arena was carried is still not sealed.

**(f) Snapshot epoch** (`memo.rs`), what replaces `nonce_and_revision`:

```rust
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Epoch(u64);
impl Epoch { pub const fn get(self) -> u64; }
impl Memo { pub fn snapshot_epoch(&self) -> Epoch; pub fn keeps_reads(&self) -> bool; }
```

Unique per snapshot in the process and increasing. A cache of answers of one arena keys on its
`ArenaId` (index + generation) instead, which outlives the snapshot while the arena is carried.

**(g) `Serve`.** Nothing from the macro: `signatures::Serve` is plain code over two tracked fns.
Where `lowered` is imaged (`#[tracked(image)]`), the own crate's image is served by (d).

**Also.** `impl Home for blaze_ids::ContentId` (names no arena), as L3 assumed.

**`owns`** (coordinator decision 1). `#[blaze_memo::tracked(.., owns)]` and
`#[blaze_memo::interned(.., owns)]`: the value (the fields) may need drop. It is kept in the arena
like any other and its destructor goes on the arena's drop list (the one input fields and
`keep_owned` values use, `Slot::drop_owned`), run when the arena is dropped, never on a carried
arena. A snapshot releases its arenas from the greatest index down (`memo::Slots`), its spill
table first, so a drop list runs while every arena its values may borrow from is alive. Cost (rule 67): an
arena drop is O(1) plus one destructor call per `owns` value in it; each `owns` value costs one
push on the arena's drop list when made. With `cycle_initial`, every round's value (the
initial one included) is placed in the arena once, listed for drop once and dropped with the
arena; no value is copied bitwise between rounds and none is dropped early, so a reference
handed out in an earlier round stays readable. Convergence compares with `PartialEq` and the
settled value is the last round's. Cost: rounds × value size, bounded by `MAX_ROUNDS`, counted
in `Counts::owned_rounds`. With `cycle_result` the fallback value is dropped the same way.
`Query::OWNS` / `Interned::OWNS: bool` (default `false`). Counted: `crates/blaze-memo/owns.txt`
(crate dir, tab, count; only shrinks; no row = 0), checked by `crates/blaze-memo/tests/owns.rs`,
which counts every attribute whose path's last segment is `tracked` or `interned` and whose
options name `owns`, however it is spelled (`#[blaze_memo::tracked(owns)]`, `#[tracked(owns)]`
after a `use`), and every call of `keep_owned`, in each crate's library source (`src/`, or the
directory of its `[lib] path`). Decision 13 asked for a marker the macro emits; a marker lives
in the expansion, which a test of blaze-memo cannot see without linking every crate that uses
the macro, so the count is of the source, spelling-independent.

**`keep_owned`** (port decision 1, for a kept value that is a real tree: a signature with its
expression store, a source map). `keep_owned<'db, T: Send + Sync + 'db>(&'db Memo, T) -> &'db T`
places the value in the run's arena and lists it on the same drop list (`Slot::drop_owned`):
dropped once, with the arena. Counted in `owns.txt` with the `owns` attributes. Test:
`tests/additions.rs` `keep_owned_drops_with_its_arena`, its control `keep_owned_unlisted_leaks`
(planted `KeepOwnedUnlisted`), and `arenas_drop_downstream_first`.

**No-drop at `cargo check`** (decision 7). Without `owns`, every tracked fn and interned struct
also expands to an item-level `const _: () = assert!(!needs_drop::<Stored>(), "the value of
<path> owns memory to drop ..")`, so `cargo check` refuses it; the monomorphization assert stays.
`keep` and `keep_iter` are generic: their assert is a `const` block evaluated at
monomorphization, so it fails when the caller is built, not when it is checked.

**No `unsafe` in expansions** (decision 6). The `'db` crossing is
`blaze_memo::plumbing::relifetime_by_macro::<A, B>(value: A) -> B` and
`relifetime_ref_by_macro::<A, B>(&A) -> &B` (`#[doc(hidden)]`, safe; sizes and alignments
const-checked; the one `unsafe` and its invariant are there). Generated code holds no `unsafe`
token, so `#![forbid(unsafe_code)]` crates keep it.

**`keep` borrows** (decision 2). `keep<'db, T: Send + Sync + 'db>(&'db Memo, T) -> &'db T`, and
`keep_slice`/`keep_iter` likewise `T: 'db` (was `'static`). Sound: the type system makes every
reference in `T` outlive `'db`; a value escaping a run is a query value whose referents are in
its home's closure, which outlives the home (declared borrows, `Memo::next`'s carry closure); no
destructor ever reads them (`!needs_drop`). Test: `tests/additions.rs`
`kept_reference_drops_with_the_borrowed_arena` (planted `IgnoreBorrows`).

**`returns(as_ref)` on `Result<T, E>`** (decision 3): `ReturnsAsRef for Result<T, E>` gives
`Result<&T, &E>`.

**Test support** (decision 4), `blaze_memo::testing` (not behind `cfg(test)`):

```rust
pub struct RunLog;   // RunLog::new(), take() -> Vec<Ran, Global>, count(&str) -> usize
pub struct Ran { pub query: &'static str, .. }   // is::<q>(&(args,)), arena(); key() -> String:
                                                  // "path::f(key)", written when read, inside `attach`
                                                  // (a run formats nothing when it starts)
impl Builder { pub fn log_runs(&mut self, log: Arc<RunLog, Global>); }   // carried by `next`
impl Memo { pub fn entries(&self, arena: ArenaId, name: &str) -> usize; }  // replaces values_of
```

`name` is the query's full path (`Kind::name`: `module::f`, `module::Type::method`).

**Ids are not persisted** (decision 8): blaze-memo has no `Gid::to_bits`/`from_bits`, and gets
none; persisted words use stable ids.

**Read rows and fact boundaries** (decision 9, `record.rs`, `task.rs`, `fetch.rs`).

```rust
#[derive(Clone, Copy)] pub struct KindId(..);   // Eq, Ord, Hash by (name hash, name); Debug = name
impl KindId { pub const fn of(&'static Kind) -> KindId; pub const fn name(self) -> &'static str;
              pub const fn kind(self) -> &'static Kind; }
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Read { Input { kind: KindId, fact: Fact }, Query { kind: KindId, key: Gid } }
impl Read { pub const fn kind(self) -> KindId; pub fn is_untracked(self) -> bool; }
impl From<Fact> for Read;                                 // Input of kind AROUND_INPUTS
pub static AROUND_INPUTS: Kind;   // "blaze_memo::read": `read(memo, fact)`
pub static UNTRACKED: Kind;       // "blaze_memo::untracked": `read_untracked`
pub static IMAGE_FACT: Kind;      // "blaze_memo::image": a served image entry's facts
pub fn read(memo: &Memo, read: impl Into<Read>);          // `read(memo, fact)` callers unchanged
pub fn read_untracked(memo: &Memo, what: &'static str);   // decision 12
pub fn record<T>(memo: &Memo, work: impl FnOnce() -> T) -> Result<(T, Vec<Read, Global>), RecorderOff>;
pub fn record_in<T, A: Allocator>(memo: &Memo, alloc: A, work: impl FnOnce() -> T)
    -> Result<(T, Vec<Read, A>), RecorderOff>;
impl Builder { pub fn mark_boundary(&mut self, name: &'static str); }   // first builder only
// generated on every tracked fn `q` (the struct `q`, `Type_method` for a method):
impl q { pub fn key_of(memo: &Memo, row: Read) -> Option<(stored args,)>;
         pub const KIND_NAME: &'static str; }                  // what mark_boundary takes
```

A list is sorted and merged; two `Input` rows of one kind and fact id with two digests panic.
An input getter lists `Input { kind: the input's kind, fact }`; a boundary query asked, hit or
run, lists one `Query { kind, key }` in its asker and none of its own rows (the firewall); every
other query is flattened into its asker; a fixpoint member is flattened whatever its kind. A
boundary entry gets an index in its arena's column (as an interned value does), so `key` is
its `Gid` in this snapshot and `q::key_of` gives its arguments back. A list depends on the entry
alone, never on who asked first. An untracked row's digest is the snapshot's epoch, so it never
matches another snapshot's. Cost (rule 67): one row (up to 88 bytes) per boundary call and per
input read in each entry's list, an append per hit, a binary search of the session's
boundaries per ask while lists are kept (none when off), a sort per run; no edge list, no walk.
Callers of `record` now get rows: `blaze_item_hash` classifies `Read::Query` rows by
`q::key_of`, and a caller wanting facts maps `Read::Input { fact, .. }`. Test:
`boundary_lists_one_row_and_keeps_its_own` (plant `BoundaryLeaks`), `untracked_read_is_never_filed`.

**`Gid` niche** (decision 10). `Gid` is 8 bytes with a niche: `size_of::<Option<Gid>>() == 8`
(the local is stored plus one in a `NonZeroU32`). Against blaze-query's 4-byte `Id`: twice the
bytes per id held in a key or value; 4-byte crate-local ids (the arena implied by the holder)
are the end state, not built here.
`Gid::session_bits(self) -> u64` and `Gid::from_session_bits(u64) -> Option<Gid>`: the id as
one word inside one process (a lock-free slot, a hash), never filed (decision 8 stands).

**`Home` for foreign types** (decision 11), in blaze-memo: `la_arena::Idx<T>`,
`la_arena::IdxRange<T>`, `intern::Symbol`, `blaze_ids::ContentId` (all name no arena). Not
here, and to be written in their own crates (blaze-memo is below them): span's
`ErasedFileAstId`, `FileAstId<N>`, `AstId<N>`, `HirFileId`, `MacroCallId`, `SyntaxContext`
(span), and hir-expand's `Name` (lane L1b writes it, naming no arena).

**base-db: the world and the snapshot** (decision 4c, the coordinator's 2026-11-13). The `Db`
is the driver's world (plain owned data by stable key) plus the current snapshot (a `Memo` and
a lookup table built at commit). Every query takes `&Db` as before; nothing below takes `&mut Db`
except `Db::edit`.

```rust
// keys, stable for the session (and, for FileId and SourceRootId, across sessions):
pub use span::FileId;                                     // the file key (the VFS's)
pub use crate::SourceRootId;                              // unchanged
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct CrateKey(u32);                                 // given when the world first has the
                                                          // crate; never reused in a session
impl blaze_memo::Database for Db { fn memo(&self) -> &Memo; }
impl Db {
    pub fn new() -> Db;          // a build's; first (empty) snapshot committed
    pub fn logged() -> Db;       // + a `blaze_memo::testing::RunLog`
    pub fn for_tests() -> Db;    // + run log, empty library/local roots, empty graph, committed
    pub fn edit(&mut self) -> Edit<'_>;
    // O(1) lookups through the snapshot's table; each panics naming the key where the world
    // has none (no fallback):
    pub fn krate(&self, key: CrateKey) -> Crate;
    pub fn file_text(&self, file: FileId) -> FileText;                     // unchanged
    pub fn source_root(&self, id: SourceRootId) -> SourceRootInput;        // unchanged
    pub fn file_source_root(&self, file: FileId) -> FileSourceRootInput;   // unchanged
    pub fn snapshot_epoch(&self) -> blaze_memo::Epoch;    // replaces `nonce_and_revision`
    pub fn runs(&self) -> Option<&blaze_memo::testing::RunLog>;   // replaces `log`/`log_executed`
}
impl Crate { pub fn key(self, db: &Db) -> CrateKey; }    // reads no fact (identity)

#[must_use = "an edit changes nothing until it is committed"]
pub struct Edit<'db> { .. }
impl Edit<'_> {    // one setter per today's setter, no durability; each returns `&mut Self`
    pub fn set_file_text(&mut self, file: FileId, text: &str) -> &mut Self;
    pub fn set_file_source_root(&mut self, file: FileId, root: SourceRootId) -> &mut Self;
    pub fn set_source_root(&mut self, id: SourceRootId, root: SourceRoot) -> &mut Self;
    pub fn set_crate_graph(&mut self, graph: CrateGraphBuilder) -> &mut Self;
    pub fn give<K: 'static>(&mut self, krate: CrateKey, value: Given) -> &mut Self;
    pub fn set_settings(&mut self, settings: FrontSettingsData) -> &mut Self;
    pub fn set_library_roots(&mut self, roots: ..) -> &mut Self;   // and set_local_roots,
    // set_included_file, set_view_root, set_appended_root_module, alias, ...: today's `Db`
    // and `Files` setters, renamed onto `Edit`, same arguments less `durability`
    pub fn commit(self) -> Committed;
}
pub struct Committed { pub crates: CratesIdMap, pub dropped: Box<[u32]> }
pub type CratesIdMap = HashMap<CrateBuilderId, CrateKey, Global>;    // was `-> Crate`
```

Arenas: index 0 is the session arena (settings, process env, library and local roots, named
paths); index 1 is the crate list's (`LIST_ARENA`: the `AllCrates` singleton, open, borrowing
the session); each source root has an arena of its own at indices `2..2 + roots`, below every
crate's; a crate's index is `2 + roots + its topological position`. A source root's files
live in the root's arena, which is open (`Inputs::open`): any arena's key may name it, and an
entry that does borrows it when written, so a crate that reads a file of another root (a
`#[path]` module, an include) is homed at the crate and dropped with that root's arena, and no
arena that only might have read it is. A crate declares the arena of its own root. Each crate
arena `borrows` each dependency and the session; `seal` for a crate not being built (all its
dependencies are). Within one database an arena's id is never handed out twice: generations
carry on across sessions, and at the generation limit the database takes a new id space under
a new nonce. `commit` compares each
written entry's content digest with the world's: an equal digest marks nothing (writing the same
text twice drops nothing), a file marks its source root's arena, a crate's data or gift that crate's,
a change of the graph's shape (indices move) every crate arena whose index or dependencies
changed, settings and env and roots the session arena. A sealed crate's write (a build script's
env, a gift, a maker) marks that crate like any other: `next` drops it and its borrowers, never
the session. The crate list (its digest of each listed crate's item key and root path, so it
moves for a crate added, taken out, moved or given another cfg) marks its own arena, never the
session's: only runs that read the list borrow it. A crate key is found by its prefix in an index
the commit builds into the table (`listed_by_prefix`): a hit reads the found crate's own input
(the asker borrows that crate), a miss reads the list; a clash of two prefixes stops the build
before any key is resolved (`listed_clash`). A crate's other `OUT_DIR`s are a field of its own
input, made at commit (`world::out_dirs_of`), marked only when they move. A gift that says where
a value can be read, never what it is (`base_db::Located`: the image's lang items, the key a
crate's constants are kept under), lives in the table, not a slot: its first gift marks nothing
(the kind's promise: every reader computes the same value with it as without it), a replacement
under another fact or a withdrawal marks the crate. A surface stays a value gift: a dependent's
image key reads it, so it marks. Floors, by write: a file, its root's arena and its readers and
their dependents; a crate's env, value gift or maker, its arena and its dependents'; its cfg, its
arena, its dependents', the list's arena and its readers; a crate added, the list's arena, its
readers and the crates of its root whose `OUT_DIR`s moved; a located gift, nothing. The driver's
writes known before any query runs (the session makers: the build target, the entries holder,
the constants' store) go in one commit (`serve::unit::deps::makers`). A new session only for a first commit, an
outgrown arena capacity, spent generations or after a failed commit. The `db commit` span says
why: `marked`, `causes` (each marked arena, sealed or not, and why, the first 8 then a count) and
`fresh`. It then calls `Memo::next(&changed)`,
remakes each dropped arena's inputs from the world (`Builder::arena`, `borrows`, `seal`), builds
the snapshot and rebuilds the table rows of the remade arenas. An input id held from before is
valid while its arena is carried; one of a dropped arena panics on use (the generation compare):
callers keep keys across a commit, never ids. Readers of the old snapshot keep answering until
they let go (`Memo::next`'s spill table); a `Reader` holds the snapshot, not the `Db`.
Cost (rule 67): a commit is O(entries written + inputs of the dropped arenas) plus `Memo::next`'s
O(arenas); a lookup is one index of a vector; nothing is checked again on a read. The persisted
codecs (`files.rs`, `content.rs`, `source_path.rs`) keep their bytes.
Tests change input through `db.edit()...commit()`; there is no thin `db.inputs()`/`db.next()`
(the world owns remaking: a lane that made inputs by hand calls the `Edit` setter instead).
Test: a Hegel property that after any edits and commits every query answer equals a fresh `Db`
built from the same world, with planted bugs (an edit marking no arena; a remake reusing a
stale input) the oracle catches.

**base-db as written** (differences from the block above win):
`Db::new`, `Db::logged`, `Db::for_tests` take `boundaries: fn(&mut blaze_memo::Builder)` (the
session's fact boundaries are fixed at its first builder: blaze-cargo and test dbs above
blaze-item-hash pass `blaze_item_hash::reads::mark_boundaries`; below it, `|_| {}`).
`Db::nonce() -> Nonce` (one per `Db`, stable across commits). `#[doc(hidden)] Db::plant(Plant)`.
`Edit` setters as written: `set_file_text`, `set_file_source_root`, `set_source_root`,
`set_appended_root_module`, `new_file_id`, `alias`, `set_alias_appended`, `set_view_root(ViewRoot)
-> Result<ViewChange, ViewError>`, `set_view_root_from_tree`, `reload_included_file`,
`set_included_file`, `settle_included_file` (each `-> bool`), `reload_included_files`,
`set_crate_graph`, `create_crate -> CrateKey`, `add_crates`, `set_all_crates`, `set_crate_data`,
`set_crate_extra_data`, `set_crate_workspace_data`, `set_crate_cfg_options`, `set_crate_env`,
`set_crate_lint_options`, `give<K>(CrateKey, Given)`, `set_settings(FrontSettingsData)`,
`set_library_roots`, `set_local_roots`, `set_process_env`, `set_primary_crates(Vec<CrateKey,
Global>)`. Gone: `Crate::create*`, every `Crate::set_*`/`seal_*`/`give`, the free
`set_all_crates_with_durability`/`add_crates`/`set_primary_crates`/`set_process_env`, `maintain`,
`executions`, `Durability`, `CratesMap`, `InternedSourceRootId`. `Crate::given::<K, T>(self, &Db)`
stays (a gift is world data, so it survives a remake). `Shared`/`Reader`: a reader holds its
snapshot; a commit never waits for it. Changed returns: `process_env_reads -> Vec<(CrateKey,
String)>`, `other_out_dirs -> &'db [&'db str]`, `Crate::transitive_rev_deps -> Vec<Crate,
Global>`, `SaidBlazeEdition<'t>`. `Edit::give_located::<K>(CrateKey, Fact, Given)`,
`Edit::take_located::<K>(CrateKey) -> bool` and `Crate::located::<K, T>(self, &Db)` for a
`K: Located`; a kind is given one way (a slot gift of a located kind, or the reverse, stops).
Known heavy drops (every arena): a new named path, a change of an included source, an edit of a
file in no crate's root. The build scripts' runs of one write pass (a restart's, found in
records, among them) are one commit (`blaze_lower::Lowered::prepare_run` checks each before
any edit, `apply_runs` writes them all, each refused alone). Located: the lang items and the
diagnostic items; slots, since what is read with them is not what is computed without: the
image and the def map pages (a served value has the image's block scopes, a paged def map
`ScopeSlot::Paged`), the def map whole (its readers are comparisons that compare only when the
def map runs) and the surface (a dependent's image key takes it in place of the key). A surface
is worked out by queries over the snapshot the write commits, so it is given in the commit
after it, dropping the written crate and its dependents a second time; the end state is the
surface as a query of the crate's own arena, read by the image key with the memo's cutoff, its
driver-held inputs (spellings, const macros, carried records) made inputs of the database.
