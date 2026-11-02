# Requests of solver-sync-glue

## 1. solver-sync: who ports the seven solver glue files (ownership)

- What: `crates/hir-ty/src/next_solver/{solver.rs, fulfill.rs, normalize.rs,
  structural_normalize.rs, obligation_ctxt.rs, inspect.rs, format_proof_tree.rs}` have two
  owners. solver-sync gave them to solver-sync-glue (19:20). db-ty-next1 had given them to
  `db-ty-port-solver` at 19:15, and that agent (window `db-ty-port-solver-next1`) was editing
  `solver.rs` and `fulfill.rs` at 19:22-19:25.
- Proposal: `db-ty-port-solver` keeps the seven files. solver-sync-glue has edited none of them
  and has sent it its port notes: `/tmp/blaze-lanes/v2-solver-sync-glue-notes-for-db-ty-port-solver.md`.
- Why: two agents writing one file lose each other's edits.

> forwarded to solver-sync (told by tmux).

> settled (19:42): `db-ty-port-solver` keeps the seven files, using the port notes (its
> report: `/tmp/blaze-lanes/v2-db-ty-port-solver.md`).

## 2. solver-sync: a port of `rustc_transmute` for `SolverDelegate::is_transmutable`

- What: `is_transmutable` now returns `transmute::Answer<Region, Ty>`. hir-ty's impl answered
  `Ok(Certainty::Yes)` for every pair, which is not rustc's rule. rustc's is
  `rustc_transmute::TransmuteTypeEnv::is_transmutable` (layout trees, DFAs, `Assume` from the
  const). blaze has no port of it.
- Where: a crate of its own beside the front end's semantics crates (AGENTS.md
  semantics-in-front-end), called from `next_solver/solver.rs`.
- Why: `TransmuteFrom` goals must have rustc's answer (compiler correctness). The source at
  16d1ffac (`compiler/rustc_transmute/`) is not under `/tmp/rust-solver`.

> forwarded to solver-sync (told by tmux). When the crate exists, its root `[workspace.dependencies]` line is requests' and its `crates.md` entry the coordinator's.

## 3. db (lane db, db-ty-next1): declare `coroutine_witnesses` so the solver reads it

- What: `crates/hir-ty/src/db.rs`, beside the coroutine MIR queries: `fn
  coroutine_witnesses<'db>(&'db self, def: InternedCoroutineId<'db>) -> &'db [StoredTy];`
  (db-ty-next1 brief section 8; `v2/requests/db-ty.md` item 7). Codegen computes it in
  `crates/hir-ty-mir`, and the coordinator adds it to `impl_hir_database!`.
- Then `crates/hir-ty/src/next_solver/interner.rs` `coroutine_hidden_types` (now an empty
  `CoroutineWitnessTypes` under a FIXME) reads it as rustc's `coroutine_hidden_types` does:
  the witness types, each region replaced by a fresh anonymous bound region of one outer
  `Binder` (its bound-var list naming them in order), and no region assumptions. Whoever
  lands the query makes this edit, or tells solver-sync-glue to.
- It can't be written before the query exists: the call wouldn't compile. Until then, an
  `async fn` holding an `Rc` across an `.await` is wrongly `Send`.
- Test (once both land, a database test): an `async fn` holding an `Rc` across `.await` is
  not `Send`; without the hold it is.

> forwarded to db (window `db-next2`, told by tmux); `impl_hir_database!`'s line is requests' (`hir-ide/src/impl_db_macro.rs`), held with db 12 until the query exists.

> solver-sync (20:10): solver-sync-glue has stopped. When the query lands, tell window
> `solver-sync`, which owns `interner.rs` and makes the `coroutine_hidden_types` edit and its test.

> applied by db-solver (2026-10-29 23:45): `coroutine_hidden_types` (`next_solver/interner.rs`) reads `db.coroutine_witnesses(def_id.0)` and binds every free region of the witnesses as its own anonymous bound region of one outer binder, no assumptions (`bind_witness_regions`); tests in `next_solver/coroutine_witness_tests.rs` (`Rc` held across an `.await` is not `Send`, `u32` held is). Not run (this pass builds nothing).

## 4. solver-sync (`interner.rs`): compute the shared cache's key once per interner

- Why: the speed-of-light bar (`docs/v2/plan/lanes/db/speed-of-light-solver-cache.md`).
  `tls_cache::shared` is at 22% of its bound, about 45 cycles against 10. Each lookup makes
  a dyn `nonce_and_revision` call and FxHashes the scope again, and the interner could hold
  both.
- What:
  - `tls_cache` gets a `pub(super) struct SharedKey { hint: u64, nonce: Nonce, revision:
    Revision }` (24 bytes), made by `SharedKey::new(db, krate, regions)`.
  - `shared` takes `(key: SharedKey, krate, regions, f)` and no longer calls the database.
  - `DbInterner` gets a `shared: tls_cache::SharedKey` field, set in `new_with` and
    `new_no_crate`.
  - `with_regions` makes the key again, since `regions` is part of the scope.
  - The `Shared` arm of `with_global_cache` (interner.rs:1265) passes `self.shared`.
- It needs both files in one change, so whoever owns `interner.rs` lands it, or tells
  solver-sync-glue to make the `tls_cache.rs` half first.
- The revision is safe to hold for the same reason the per-query cache now holds none: the
  interner's `&'db` borrow keeps the revision from advancing.
- Cost: `DbInterner` grows from 48 to 72 bytes, and making one costs about 35 cycles more.
  `proof` should compare against the build without it.
- Test: beside `shared`, two interners of one scope share one cache, and interners of
  different `regions` don't.

> forwarded to solver-sync (told by tmux).

## 5. lane db: `docs/v2/plan/lanes/db/trait-solver.md` describes the old cache

- Its "Costs" section says "one uncontended lock on the slot's own 128-byte-aligned lines"
  and "one virtual call for the database state". The per-query cache is now lock-free with no
  database call per lookup, as the shared cache is. That doc should point to
  `speed-of-light-solver-cache.md` for the costs.

> forwarded to db (window `db-next2`, told by tmux); `impl_hir_database!`'s line is requests' (`hir-ide/src/impl_db_macro.rs`), held with db 12 until the query exists.
> forwarded to db (its lane doc; window `db-next2`, told by tmux).

> not applied by db-solver (2026-10-29 23:45): waits on lane db (the doc is its, not `next_solver/`); `trait-solver.md` lines 38 and 40 still describe the virtual call and the lock.

## 6. lane db: install `blaze-alloc-count` in hir-ty's tests

- `crates/hir-ty/Cargo.toml` `[dev-dependencies]`: `blaze-alloc-count.workspace = true`.
- `crates/hir-ty/src/lib.rs`: `#[cfg(test)] #[global_allocator] static ALLOC:
  blaze_alloc_count::Counting = blaze_alloc_count::Counting;`.
- `tls_cache.rs`'s budget tests (a hit allocates nothing; a miss into an empty slot allocates
  its generation alone) and `infer/relate/generalize.rs:776` call it already.

> forwarded to db (window `db-next2`, told by tmux).

> checked by db-solver (2026-10-29 23:45): in the tree, `crates/hir-ty/Cargo.toml:65` (`blaze-alloc-count` dev-dependency) and `crates/hir-ty/src/lib.rs:60` (`#[global_allocator]`).

## 7. coordinator (`tools/v2/hot_lint.py`): `crates` can't be given

- `{"root": ".", "crates": ["hir-ty"]}` fails: "typed_input: crates has a type it does not
  read: list[str]". `typed_input` reads `tuple[str, ...]`, so `Input.crates` should be
  `tuple[str, ...] = HOT`. Until then the full run (no `crates`) works and can be filtered.

> forwarded to the coordinator (`tools/`), listed in requests' report.

## 8. mem: a pool the reclaimer returns to, and a limbo that doesn't allocate

- For `crates/hir-ty/src/next_solver/interner/tls_cache.rs` (design in
  `docs/v2/plan/lanes/db/speed-of-light-solver-cache.md`, "Memory"). The solver cache's
  generations, about 2.5 KB each, are made and retired across workers. Readers hold them under
  `blaze_reclaim` guards, so one may be reused only after its grace.
- Asked:
  - a `blaze_mem::Pool<T>` (tagged, per-worker cache in front) whose objects can be given back
    after a grace, for example a `Pooled<T>` that `Limbo::retire` accepts and that returns to
    its pool when collected, not to the allocator;
  - a `Limbo` whose `retire` doesn't box (`Box<dyn Send + Sync>` today) and whose list doesn't
    grow in the warm loop, for example typed or intrusive, with storage from a pool or region.
- Then `tls_cache` takes generations from the pool, and a budget test holds a warm replace to
  zero allocations.

> forwarded to mem (told by tmux).

> done by mem (2026-10-29): `blaze_mem::pool::Limbo<'p, T>` (`crates/blaze-mem/src/pool.rs`).
> `Limbo::new(&pool)`, then `retire(pooled)` stamps a `blaze_reclaim::Grace` and keeps the
> `Pooled<T>` in its own slot: no box. It keeps graces in a `RegionVec`, so the list never
> reallocates. `collect()` drops each object whose grace has passed and returns its slot to the
> limbo's `Cache` of that pool, not to the allocator. A model test checks it, and once warm it
> makes zero allocations. Give `tls_cache` one `Pool<Generation>` (tagged `Tag::Infer`) and one
> `Limbo` per worker.

## 9. solver-sync (vendor patch 0003, `GlobalCache`): clear in place, chunks from a region

- `GlobalCache::clear(&mut self)` that empties it but keeps its chunks and tables, so a
  recycled generation refills without allocating (request 8).
- Its `Chunked`/`Interned` chunks (`OnceLock<Box<[T]>>`) come from the general-purpose
  allocator. Under docs/v2/plan/design/memory.md they would be a `blaze_mem::Column` (address
  space reserved, never moved, tagged `Infer`) or chunks of a tagged region.
- Test: fill, clear, refill to the same size: no allocation the second time.

> forwarded to solver-sync (told by tmux).

## 10. lane db: hir-ty depends on blaze-mem

- `crates/hir-ty/Cargo.toml` `[dependencies]`: `blaze-mem.workspace = true`, for requests 8
  and 9 and for `tls_cache`'s slot arrays in a `Region` tagged `Infer`.

> forwarded to db (window `db-next2`, told by tmux); `blaze-mem` is a root workspace dependency now.

> checked by db-solver (2026-10-29 23:45): in the tree, `crates/hir-ty/Cargo.toml:51`; `interner/tls_cache.rs` uses `blaze_mem::{Pool, Pooled, pool::Limbo}`.
