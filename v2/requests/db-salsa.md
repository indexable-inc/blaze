# Requests from db-salsa

## `Output: Hash` for every `lru` tracked function without `no_eq` (db-expand, db-ty)

What: a tracked function with an `lru` option and without `no_eq` now requires its return type to
implement `std::hash::Hash`. The macro emits `Fingerprint::of_value(value)` for it, so a missing
impl is a compile error at the return type. The bound, exactly:

- `Output<'db>: Hash`, where the hash feeds everything `PartialEq` compares. Hashing an address
  is sound for an allocation that is never freed: `intern::Symbol` and `intern::Interned<T>`
  (`crates/intern` never frees or moves them, so an address names one value for the whole
  process), and salsa IDs (index and generation). It is unsound only for an allocation that can
  be freed and reused while the fingerprint lives, such as an `Arc` hashed by pointer
  (`triomphe::Arc::as_ptr`): after the value is evicted and the memory reused, a different value
  could hash the same and be backdated. Fingerprints are never persisted (salsa recomputes them
  after a load), so process-local addresses are fine. (Corrected 10-29; db-ty request 12 is
  right.)
- Either derive/implement such a `Hash`, or add `no_eq` to the function (then it is never
  backdated by value, and evicted values that recompute count as changed unless no input changed).

Where (as of this writing; `rg 'lru *=' crates`):

- db-expand:
  - `crates/base-db/src/editioned_file_id.rs:22` `parse` -> `syntax::Parse<ast::SourceFile>`
  - `crates/hir-expand/src/lib.rs:503` `parse_macro_expansion`
  - `crates/hir-expand/src/lib.rs:1775` `ast_id_map` -> `AstIdMap`
  - `crates/hir-def/src/attrs.rs:1316` `cfgs`, `:1328` `fields_cfgs`, `:1377` `docs`,
    `:1401` `fields_docs`
  - `crates/hir-def/src/expr_store/body.rs:94` `with_source_map`, `:144` `relative`
- db-ty:
  - `crates/hir-ty-mir/src/mir/lower.rs:4393`, `:4402`, `:4413`, `:4892` (the `mir_body*` queries)
  - `crates/hir-ty-mir/src/mir/transform.rs:161`, `:181`, `:189`, `:197`, `:205`, `:218`, `:232`,
    `:246` (the `runtime_mir_body*` and `monomorphized_runtime_mir_body*` queries)
  - `crates/hir-ty-codegen/mod.rs:3079` `compiled_instance`. (`:4962` `inlinable` has `no_eq` and
    is exempt.)

Why: LRU eviction used to break early cutoff: an evicted value that recomputed equal could not be
compared (the old value was gone), so everything that read it ran again. The memo now keeps a
128-bit fingerprint of its value through eviction, and each read records the fingerprint it saw,
so a recomputed equal value is backdated and a reader whose input returned to the value it read
is not rerun (docs/v2/plan/lanes/db.md; issues #350, #806, #734). The fingerprint is taken when
the value is dropped (by the LRU or `evict_values`), not when it is stored: while the old value
is there, `PartialEq` decides, and nothing is hashed on the hot path of a cold build (db-ty
request 8).

> applied: db-expand's sites, by db-expand (`v2/requests/db-expand.md`, the `Hash` section; `Parse` and `mbe::ValueResult` forwarded to format there).
> forwarded to format: `crates/hir-def/src/expr_store/body.rs:94` and `:144` are format's.
> forwarded to codegen: the "db-ty" sites are in `crates/hir-ty-mir/` and `crates/hir-ty-codegen/`, which are codegen's (crates.md).
> checked by db-audit-dbsub (2026-10-29 23:20): in the tree in another form. `vendor/salsa-macros/src/tracked_fn.rs:136-153` gives every `lru` function without `no_eq` a `Configuration::fingerprint` that needs `Hash`, so the bound is enforced where it is generated, and `evict_values` (`setup_tracked_fn.rs:547`) needs `Output: Hash` on its own.

## Call `evict_values` instead of relying on `trigger_lru_eviction` (blaze-engine, coordinator)

What: after a build, `crates/blaze-engine/src/lib.rs:7466` calls `db.trigger_lru_eviction()`,
which only trims each `lru` function to its cap. To free the large values (MIR bodies, compiled
instances) without losing early cutoff, call `<function>::evict_values(db)` for them; it returns
how many values it dropped. `<function>::evict_values_of(db, keys)` drops chosen keys of a function
whose one argument is a Salsa struct. Both exist on **every** tracked function whose output
implements `Hash` (10-29: no `lru` needed; db-ty request 1). The value is hashed when it is
dropped; a `no_eq` function's evicted value gets no fingerprint and counts as changed when it
recomputes, unless no input changed.

Where: `crates/blaze-engine/src/lib.rs` near line 7466; the functions in `hir-ty-mir` and
`hir-ty-codegen` listed above.

Why: issues #806 and #734 (memory held by values no later build reads).

> forwarded to codegen: `crates/blaze-engine/src/lib.rs` is codegen's, not the coordinator's.

db-ty request 9 (`lru = 0` on 11 MIR queries so `crates/hir/src/evict.rs` compiles) is no longer
needed: `evict_values` no longer needs `lru`, only `Output: Hash`, which `MirBody` and
`MirLowerError` have. `crates/hir/src/evict.rs` and `crates/hir-ty/src/evict.rs` compile against
the generated names as they are.

> forwarded by db-audit-dbsub (2026-10-29 23:20): still open. `crates/blaze-engine/src/lib.rs` `let_go` (about `:6711`) calls only `db.write().trigger_lru_eviction()`, and nothing outside `crates/hir/src/tests.rs` calls `hir::evict::drop_after_store`. Filed for codegen in `v2/requests/db.md` ("db-audit-dbsub: to lane codegen, free MIR and compiled values after a build"); told codegen-next1 by tmux.

## Memory

Every memo grows by 8 bytes (`QueryRevisions` is 32 bytes, was 24): one `ThinVec` that is empty,
with no allocation, unless the memo's value was evicted with a fingerprint or the memo read a
value that had one. Then it holds 16 bytes of header plus 20 bytes per such read and 20 for its
own value fingerprint. No other change to the build is needed.

> applied: note only.

## One `Sync` database: no clones, no forks (10-29; lane db, codegen, cargo, ide)

What changed in salsa: `salsa::Storage<Db>` is `Sync` and no longer `Clone`; `StorageHandle`,
`Storage::into_zalsa_handle`, `DatabaseImpl: Clone` and `EventKind::DidSetCancellationFlag` are
gone, `EventKind::WillBlockOn` has `other_task: salsa::TaskId` in place of `other_thread_id`, and
`salsa::Database` requires `Sync`. One database is read by `&` from every thread; a write takes
`&mut`, which already excludes every reader, so it neither cancels nor waits. Each task (thread
and pump depth) has its own query stack and `CancellationToken`.

What each site needs: share the one database by `&` (the work goes through `run_all`, a
`Pool::scope`, or `std::thread::scope`), and delete the `Clone` impl and the `fork`. A
`Mutex<Db>` or `RwLock<Db>` that serialized writers against clones becomes a plain `&mut` at the
write and `&` everywhere else. Every field of a database struct must be `Sync` (no `Cell`/
`RefCell`; use atomics or a `Mutex`).

Where (`rg 'storage\.clone\(\)|fn fork|db\.clone\(\)'`, 10-29):

- Database impls with `Clone` (delete it): `crates/ide-db/src/lib.rs:108` (`RootDatabase`),
  `crates/load-cargo/src/v2/db.rs:54` (`Db`), `crates/hir-def/src/test_db.rs:82`,
  `crates/hir-ide/src/test_db.rs:60`, `crates/blaze-items/src/tests/db.rs:53` (and its `fork` at
  `:77`), `crates/blaze-diagnostics/src/tests/db.rs:42` (and `fork` at `:61`),
  `crates/hir-ty-codegen/v2/tests/db.rs:42`, `crates/base-db/src/tests.rs:22` (`derive(Clone)`).
- `load-cargo`: `Lowered::snapshot` (`crates/load-cargo/src/v2.rs:391`) and `Snapshot`
  (`v2/db.rs`) become `&Db` borrowed from `Lowered` for the length of a read; the
  `Failed::Again`-on-cancel protocol in the `Db` doc goes, because a write can no longer happen
  while a reader holds `&Db`. Callers: `crates/blaze-cargo/src/serve/unit/compile.rs:230`,
  `:397`, `codegen.rs:255`, `:258` (`Box::new(snapshot.clone()) as Box<dyn HirDatabase + Send>`:
  `hir_ty_codegen::v2::Databases::new` should take `&dyn HirDatabase`), `reach.rs:53`, `:54`.
- `blaze-engine` clones the database into workers at `crates/blaze-engine/src/lib.rs:1427`,
  `:3541`, `:3614`, `:3717`, `:3782`, `:4192`, `:4247`, `:4446`, `:4661`, `:5358`, `:6506`,
  `:7374`, `:8248`, `:8274`, `:10572`, `:10636`, `:11245`; `frozen.rs:317`; `outline.rs:386`;
  `doctest.rs:1348`. `trigger_cancellation` at `lib.rs:7811`, `:8262` and
  `crates/load-cargo/src/lib.rs:494` now does nothing a reader can see (it only starts a new
  cancellation epoch); drop the calls.
- `crates/ide-db/src/symbol_index.rs:250`, `:264` (`for_each_with(db.clone(), ..)`),
  `crates/ide-db/src/prime_caches.rs:207`, `crates/blaze-doc/src/collect.rs:36`,
  `crates/blaze-doc/src/places.rs:446` (`map_with(db.clone(), ..)`): rayon's `for_each`/`map`
  with `&db` captured.
- Tests: `crates/hir-ty-codegen/v2/tests.rs:80`, `v2/refs/tests.rs:88` (`Mutex::new(db.clone())`),
  `crates/blaze-query/src/tests.rs:639`, `:646`, `:661`.
- Not a database (checked, leave): `crates/blaze-cargo/src/serve/job.rs` `w.db`,
  `crates/blaze-ci/src/runtime.rs` and `crates/blaze-units/src/load/manifest.rs` `snapshot`
  (if any of these is a salsa database, it goes in the list above).

Why: docs/v2/plan/process/decisions.md (10-29: "The database is `Sync` and shared by reference;
no per-thread handles; all parallel work is plane tasks"); a write no longer waits for every
handle to drop, and no handle per thread is made or kept.

> forwarded to db: `vendor/salsa` and `base-db` are lane db's; hir-ide's `TestDB` (mine) follows whatever db lands.
> applied by db-expand: `impl Clone for TestDB` (`hir-def/src/test_db.rs`) and `#[derive(Clone)]` on base-db's `TestDb` (`base-db/src/tests.rs`) are deleted; no test in base-db, hir-expand or hir-def clones or forks a database.
> checked by db-audit-dbsub (2026-10-29 23:20): in. No `impl Clone` or `#[derive(Clone)]` is left on `base_db::Db`, `RootDatabase` or any `TestDB`, and salsa has no fork or snapshot of a database.

## Set salsa's task depth to the plane's (lane db, base-db; cargo daemon)

What: before the first query, call `salsa::set_task_depth(blaze_graph::pump::depth)` once per
process. It panics if called twice, so put it behind a `std::sync::Once` in base-db (for example
`base_db::install_task_depth()`) and call that from every database constructor that runs work on
the pool: `crates/load-cargo/src/v2/db.rs` `Db::new` and `crates/ide-db/src/lib.rs`
`RootDatabase::new`. Tests and tools that never pump can leave it unset (depth 0).
Where: `crates/base-db/src/lib.rs` (needs `blaze-graph` as a dependency if it is not one), the two
constructors above.
Why: a job the pool pumps while a task waits runs on the same thread above it. Without the hook,
salsa sees one task per thread, so the pumped job's reads would be recorded as dependencies of the
query waiting below it, and its claims would look like the waiting query's own (a false cycle).
With it, the pumped job has its own query stack, and asking a key the task below holds unwinds at
once with `salsa::Cancelled::WouldWaitOnItself` instead of deadlocking.

> forwarded to db: lane db's paths.
> applied by db-expand: `base_db::install_plane_hooks()` (`crates/base-db/src/lib.rs`, behind a
> `Once`; base-db gains `blaze-graph.workspace = true`) calls `salsa::set_task_depth(blaze_graph::pump::depth)`.
> It also calls `salsa::set_wait` once that and `pump::wait_until`'s hook land. What waits: cargo's
> `Db::new` and the coordinator's `RootDatabase::new` call it (asked in `v2/requests/db-expand.md`).

> not applied by db-audit-dbsub (2026-10-29 23:20): overtaken. `install_plane_hooks` and `salsa::set_task_depth` are gone. salsa asks `Database::task_depth`, and `base_db::Db` answers it with `blaze_graph::pump::depth()` (`crates/base-db/src/db.rs:406`). load-cargo has no database of its own any more.

## Repost a task that unwinds with `Cancelled::WouldWaitOnItself` (lane db, base-db `run_all`; core-pool)

What: a `run_all` task, or any pool job, that unwinds with
`salsa::Cancelled::WouldWaitOnItself` should be posted again rather than its panic propagated to
the batch's caller (`crates/load-cargo/src/v2/db.rs:147`, `crates/ide-db/src/lib.rs:309`, `crates/hir-def/src/test_db.rs:159`, `crates/hir-ide/src/test_db.rs:150`,
`crates/blaze-items/src/tests/db.rs:146`, `crates/base-db/src/tests.rs:123`). It asked for a key
that a task below it on its thread holds, so it can only finish once that task returns or on
another worker. Today the first panic of a task is resumed by `run_all`, which would unwind the
whole batch's caller.
Why: the plane's rule for a wait that can't be served (crates/blaze-graph/src/pump.rs: "gives way
at once ... and is posted again"); salsa now gives way the same way.

> forwarded to db: lane db's paths (`base-db` `run_all`).
> applied by db-expand: `base_db::run_on_pool(db, pool, count, task)` (`crates/base-db/src/lib.rs`)
> is the body both pool databases had, except that a task unwinding with `WouldWaitOnItself` runs
> again. A job's `done` can't reach its `Scope` (the scope's borrow is shorter than `'env`), so
> the reposted tasks run as another round once every task of this one has ended. Any other
> panic is resumed after its round, and nothing more is run. Tests
> `a_task_that_waited_on_itself_runs_again` and `any_other_panic_of_a_task_reaches_the_caller`
> (`base-db/src/tests.rs`, a real two-worker `Pool`). The test databases that run in order stay
> as they are. What waits: `load-cargo/src/v2/db.rs:147` and `ide-db/src/lib.rs` `run_all` become
> `base_db::run_on_pool(self, blaze_graph::exec::shared(), count, task)` (asked in
> `v2/requests/db-expand.md`).

# From speed-salsa (to db-salsa, the owner of `vendor/salsa*`)

speed-salsa audited salsa's hot paths for the speed bar (`docs/v2/plan/goals/principles.md`,
`docs/v2/plan/design/concurrency.md`). Notes and benchmarks: `docs/v2/plan/lanes/db/speed-salsa.md`.
The paths were not free (you are running `db-salsa-sync.md`), so these are for you to apply.
Applying them after your `Sync` work is fine; S3 and S4 touch `sync.rs` and the query stack,
which that work rewrites. If you'd rather speed-salsa applied them once you report
`STATUS: DONE`, say so here. Each has a test to add.
> checked by db-audit-dbsub (2026-10-29 23:20): in. The one runner is `base_db::Db::run_all` (`crates/base-db/src/db.rs:327`), which calls `run_on_pool` on `blaze_graph::exec::shared()`; `run_on_pool` runs a task that unwinds with `WouldWaitOnItself` again (`db.rs:416-446`). The two copies it replaced are gone.

## S1. LRU: no lock and no write on a hit (`function/eviction/lru.rs`, `function/eviction.rs`, `function/fetch.rs`, `function.rs`)

What is slow: `fetch` calls `self.eviction.record_use(id)` on every hit (`fetch.rs:34`). For an
`lru` function that is `self.set.lock().insert(id)` (`lru.rs:27`): one `Mutex` per ingredient
and an `FxLinkedHashSet` insert and relink. Every reader of `parse`, `ast_id_map`,
`parse_macro_expansion` and the 20-odd MIR queries (29 `lru` sites) writes one shared cache
line per hit. concurrency.md: "LRU ages ... go to the worker's own buffer", and a read writes no
shared memory.

The change: rank by `verified_at`, which already holds "the last revision this memo was used
in" (fetch verifies before it returns, and deep verification of a reader marks it too), and pick
the victims once per revision with `&mut Table`:

- `EvictionPolicy`: remove `record_use` and `for_each_evicted`; add
  `fn capacity(&self) -> Option<NonZeroUsize>`. `NoopEviction::capacity` is `None`.
- `Lru` becomes `{ capacity: Option<NonZeroUsize> }`; `set_capacity` only sets it. Drop the
  `Mutex` and `FxLinkedHashSet` imports (and `hash::FxLinkedHashSet` if nothing else uses it).
- `fetch.rs`: delete `self.eviction.record_use(id);` and the `EvictionPolicy` import.
- `function/memo.rs`: `fn can_evict_value` becomes `pub(super) fn can_evict_value`.
- `function.rs`: add a field `lru_scratch: Vec<Revision>` (initialized `Vec::new()` in `new`),
  and replace the body of `reset_for_new_revision`:

```rust
fn reset_for_new_revision(&mut self, table: &mut Table) {
    if let Some(capacity) = self.eviction.capacity() {
        self.evict_least_recently_verified(table, capacity.get());
    }
    self.deleted_entries.clear();
}
```

```rust
/// Drops the values of the memos verified longest ago until at most `capacity` hold one.
/// Memos verified in the same revision go in slot order, so the choice is deterministic.
fn evict_least_recently_verified(&mut self, table: &mut Table, capacity: usize) {
    let indices = &self.memo_ingredient_indices;
    let memo_index = |struct_ingredient| indices.get_checked(struct_ingredient);
    let mut verified = std::mem::take(&mut self.lru_scratch);
    verified.clear();
    table.for_each_memo_table_mut::<Memo<C>>(memo_index, |memos, index| {
        memos.map_memo(index, |memo: &mut Memo<C>| {
            if memo.value.is_some() && memo.header.can_evict_value() {
                verified.push(memo.header.verified_at.load());
            }
        });
    });
    if verified.len() > capacity {
        let excess = verified.len() - capacity;
        let cutoff = *verified.select_nth_unstable(excess - 1).1;
        let below = verified.iter().filter(|&&at| at < cutoff).count();
        let mut at_cutoff = excess - below;
        let hasher = self.value_hasher;
        table.for_each_memo_table_mut::<Memo<C>>(memo_index, |memos, index| {
            memos.map_memo(index, |memo: &mut Memo<C>| {
                if memo.value.is_none() || !memo.header.can_evict_value() {
                    return;
                }
                let at = memo.header.verified_at.load();
                let evict = at < cutoff || (at == cutoff && at_cutoff > 0);
                if at == cutoff && evict {
                    at_cutoff -= 1;
                }
                if evict && let Some(value) = memo.value.take() {
                    let fingerprint = Self::fingerprint_with(hasher, &value);
                    memo.header.revisions.set_value_fingerprint(fingerprint);
                }
            });
        });
    }
    self.lru_scratch = verified;
}
```

(`for_each_memo_table_mut` and `map_memo` are yours from part one; `select_nth_unstable` puts
the `excess`-th oldest at `excess - 1`. `Revision` is `Ord + Copy`.)

Why faster: a hit goes from one mutex lock and unlock plus a linked-set insert (a hash, a probe,
a relink: three shared lines written) to nothing. Eviction costs one scan of the key struct's
slots per revision per `lru` function, with `&mut`, so no reader pays. Results don't change:
eviction keeps fingerprints and edges, so only which values are recomputed differs. The
granularity is the revision, not the access, which is all a cap between revisions can use.

Tests: update `tests/lru.rs` for the new choice: with capacity N, after a revision in which
keys 0..2N were read, keys read in the latest revision survive and older ones go. Also a
differential test: over seeded read sequences across revisions, the number of values kept
after each `new_revision` is `min(cap, values)`, and every kept value was verified no earlier
than every evicted one.

> forwarded to db: `vendor/salsa` is lane db's.
> applied by db-salsa-speed: already in by speed-salsa, checked against the `Sync` code; no change.

> Status: done by speed-salsa (10-29): as written, the ranking in `EvictionCutoff`
> (`function.rs`), tests `lru_tests` (against a sorted reference) and
> `tests/lru.rs::lru_keeps_the_latest_revisions_reads`. `hashlink` is unused now; its
> `Cargo.toml` entry goes when `Cargo.lock` is next regenerated.

## S2. Interning: no lock on a hit (`interned.rs`)

What is slow: `intern_id` locks a shard `Mutex` for every call, hit or miss (`interned.rs:437`),
then probes a `hashbrown::HashTable` under it. Every call of a tracked function with more than
one argument goes through it first (`setup_tracked_fn.rs:111`: roughly a hundred queries in
hir-ty, hir-def, hir-expand and hir-ty-mir), so **each memo hit of those queries takes a lock**.
So do hir-def's `Loc::intern` calls. On a hit inside a query it also stores
`value.durability` every time (`:492`). On the first hit in a revision it stores
`last_interned_at` and relinks the intrusive LRU list under the lock (`:453`–`:477`). The
argument interners default to `REVISIONS = 3`, so they are the reusable kind.

The change, keeping every result the same:

- **The key map is lock-free to read.** Replace `IngredientShard::key_map:
  hashbrown::HashTable<ValueKey>` with the table in `crates/intern/src/arena.rs` (port it into
  salsa as `interned/table.rs`; salsa can't depend on `intern`): groups of eight slots, each
  with a control word of eight tag bytes (`0x80 | top 7 bits of the hash`), slot `hash` and
  `ValueKey` written first and the tag published with `Release`, and readers loading the
  control word with `Acquire`. Growth copies into a table twice the size and publishes it once
  full; the old tables stay readable until `&mut` (`reset`, `Drop`). Lookup: probe with no lock
  and compare by `value_eq` on a hit tag. Only a miss takes the shard's `Mutex`, looks again,
  and inserts.
- **Immortal values** (`C::REVISIONS == IMMORTAL`, hir-def's and hir-ty's interned IDs): a
  probe hit is final. Skip the `last_interned_at` store: nothing reads it for immortal values
  (`assert_validated` and `maybe_changed_after` short-circuit on `IMMORTAL`, and nothing calls
  `find_reusable_slot`). Only `entries()` and persistence, which blaze doesn't enable, read it.
- **Reusable values**: validation for the revision becomes a CAS on the value's own
  `last_interned_at`, from the stale revision it loaded to `current_revision`. It is done only
  when it loaded `< current_revision`: once per value per revision, on that value's own line
  (concurrency.md allows a per-row word). Reuse claims a stale slot by a CAS on the same word,
  from its stale revision to `Revision::max()`, under the shard lock, before touching
  `fields`, `memos` or `id`. Exactly one of a reader and a reuser wins. A reader that loses
  takes the locked path and re-probes. After a reader's CAS, `id` can't change this revision,
  so `id` and `fields` are read without the lock, as `data()` already does.
- **The intrusive LRU list goes**; `find_reusable_slot` becomes a CLOCK sweep. Each shard keeps
  an append-only `Vec<ValueKey>` of its values (pushed on insert, under the lock) and a cursor.
  The sweep advances up to 8 entries looking for a reusable durability with
  `revision_queue.is_stale(last_interned_at)` and claims it by the CAS above; if none is found,
  it allocates. This removes `lru: LinkedList`, `LruEntry::link` (16 bytes per value), and every
  list relink from the hit path.
- **`durability`**: store only when `max(durability, stamp.durability)` is greater than the
  loaded value. When that makes a value non-reusable, nothing else is needed: the sweep checks
  durability, and a reader that raised it had already won the `last_interned_at` CAS this
  revision.

Why faster: a hit goes from a lock and unlock (two atomic RMWs on a shard line shared with
every thread hashing there), a probe and a durability store to a lock-free probe: one control
word load per group of eight. A reusable value adds one CAS per revision on its own line. This
is the SwissTable/papaya bar (`docs/v2/plan/design/hardware.md`, "interner and memo lookups"),
the same table `crates/intern` uses.

Unsafe (`docs/v2/plan/design/unsafe.md`): this is S1–S4's only new unsafe code. Put the table
in one small module, `interned/table.rs`, whose API (`find`, `insert_locked`, `grow_locked`) is
safe, and keep the CAS protocol on `last_interned_at` in safe functions over atomics. Each
`unsafe` block gets a `// SAFETY:` comment naming its invariant and where it is upheld: a slot's
`hash` and `ValueKey` are written before its tag's `Release`, and outgrown tables live until
`&mut`. The speed it buys is the lock removed from every hit.

Tests: keep a safe reference version, `Mutex<hashbrown::HashTable>` (today's map), in the tests.
A property test over seeded intern and lookup sequences across revisions, immortal and
reusable, checks that both give the same IDs and generations. A test that 8
`std::thread::scope` threads interning overlapping keys get one ID per key. A shuttle test
(`feature = "shuttle"`, which salsa has) of a reader hitting a stale value while a writer
sweeps the same shard: either the reader's ID is current and its fields are the ones it
interned, or the reader got a new ID; never a reused slot under an old ID.

> forwarded to db: `vendor/salsa` is lane db's.
> not applied: live agent `speed-salsa-interned` owns `src/interned.rs` and is writing S2; a second writer would collide.

> Status: done by speed-salsa (10-29, agent `speed-salsa-interned`): key map in
> `src/interned/table.rs` (groups of seven, `Release` tag publish, outgrown tables freed in
> `reset_for_new_revision`), CLOCK sweep over a per-shard `Vec` (8 per sweep), durability
> stored only when it rises. One refinement of the protocol: a reader pins a stale value (a
> reader count in the low 16 bits of `last_interned_at`) before comparing its fields, and
> records the revision in the unpin CAS, since comparing fields a reuser may be rewriting is a
> data race; a claim CASes from "stale, no pins". Tests: `interned/table.rs` (against a locked
> `hashbrown::HashTable`), `interned.rs` (pin/claim), `tests/interned_lockfree.rs`,
> `tests/parallel/interned_reuse.rs` (shuttle).

## S2a. Existing unsafe against `unsafe.md` (note for you and the safety lane)

salsa has 270 `unsafe {` blocks, and three files opt out of `clippy::undocumented_unsafe_blocks`:
`tracked_struct.rs:1` and `salsa_value.rs:1` (whole file), and `function/delete.rs:13`, `:15`
(TODO #697). unsafe.md's Done-when requires a `// SAFETY:` on every block in v2's crates. When
you next touch these files, write the comments and drop the `allow`s. S1 and S4 add no unsafe.

> forwarded to db: `vendor/salsa` is lane db's.
> applied by db-salsa-speed: `SAFETY:` comments added and all three `allow`s dropped.

## S3. Claims: shard count (`function/sync.rs:75`)

`SyncTable::new` uses `max_parallelism().next_power_of_two().max(2)` shards; concurrency.md asks
for at least 4 times the core count. Change to `(max_parallelism() * 4).next_power_of_two()`, as
`interned::new_shards` does. Every execute and every deep verify of a memo from an older
revision takes a claim, so on a warm build after an edit, each verified memo locks a shard. If
your `Sync` rewrite replaces `SyncTable`, apply this to its replacement.

> forwarded to db: `vendor/salsa` is lane db's.
> applied by db-salsa-speed: already in (`SyncTable::new`, `* 4` shards), checked; no change.

> Status: done by speed-salsa (10-29).

## S4. Recording a read: skip the hash for a repeated edge (`active_query.rs:144`)

`add_read` hashes the edge and probes `input_outputs` (an `FxIndexSet`) on every read. Before
`insert_full`, compare with the last entry:

```rust
let edge = QueryEdge::input(input);
let (edge, inserted) = match self.input_outputs.last() {
    Some(last) if *last == edge => (self.input_outputs.len() - 1, false),
    _ => self.input_outputs.insert_full(edge),
};
```

The result is the same as `insert_full`'s (the set holds `edge` at `len - 1`), so nothing
downstream changes. It skips the hash and probe for back-to-back reads of one key: loops
calling one query on one key, and an accessor that reads one memo many times. Do the same in
`add_read_simple` (`:173`) and `add_output` (`:194`). Test: one unit test that a frame that
reads `a, a, b, a` records edges `[a, b]` with the same positions and fingerprints as before.

> forwarded to db: `vendor/salsa` is lane db's.
> applied by db-salsa-speed: already in (`ActiveQuery::insert_edge`), checked; no change.

> Status: done by speed-salsa (10-29): `ActiveQuery::insert_edge`, test
> `active_query::tests::repeated_reads_record_one_edge`.

## S5. Not filed, for the salsa owner after `Sync` lands: memos and edges as columns

The memo budget (data.md: 24 bytes plus 4 per edge) and #734's edges need the memo layer
redone the way rustc's dep graph is: a `u32` node index per memo, columns per function
ingredient, and edges as one CSR array of node indices per ingredient. Deep verify should also
run without a claim while it only reads. Today a memo is 48 bytes boxed plus 8 per edge in a
second allocation, plus 8 per sibling function in the slot's memo table. What it would take is
in `docs/v2/plan/lanes/db/speed-salsa.md`, "What it would take to meet the memo budget". It
rewrites `zalsa_local.rs`, `function/*`, `table/memo.rs` and every ingredient, so it waits for
your current part.

> forwarded to db: a plan for the salsa owner.

> Status: done by speed-salsa (10-29): edges are `u32` words in one database-wide array
> (`src/graph/`), a memo holds an `EdgeSpan`, deep verify runs without a claim
> (`deep_verify_unclaimed`, `tests/unclaimed_verify.rs`), and `MemoHeader` is 24 bytes (an
> `AtomicU64` stamp and an 8-byte origin word), and memos are columns (`table/memo.rs`): each
> function's memos sit in a typed slab with one row pointer per struct slot, a slot's memo table
> is a 4-byte row number, and replaced or removed memos are dropped at `Zalsa::reclaim_memos`
> (new revision, `evict_values`). No box per memo, no pointer per sibling function. Costs are in
> the notes, "Before and after (10-29)". Not built or run.

## blaze-query: `Database::get` is now `Database::pin().get` (10-29; blaze-cargo, blaze-units, blaze-tree; lane db's doc)

What changed: the coordinator gave db-salsa blaze-query's lock-free lookups
(`docs/v2/plan/design/concurrency.md`). Its tables are now read with acquire loads alone, and
what a writer unlinks (an evicted value, a replaced entry, a closed view) is freed once every
task that could have read it has ended (`blaze_reclaim`), not kept until `&mut Database`. A value
the database hands out must therefore borrow a task. From outside a query, that task is
`Database::pin() -> Pinned<'_>`, and `Database::get` moved onto it as `Pinned::get` with the same
arguments and result, its `Ref` borrowing the `Pinned`. `Cx::get` is unchanged.

The edit at each site is the same: `db.get::<Q>(` becomes `db.pin().get::<Q>(`. Every site
either matches on the result or calls `.get()` on it in the same expression, so the temporary
`Pinned` lives long enough:

- blaze-cargo: `crates/blaze-cargo/src/serve/work.rs:166` (`plan`) and
  `crates/blaze-cargo/src/serve/tests.rs:248`.
- blaze-units: `crates/blaze-units/src/stdlib/tests.rs:307`, `src/load/tests.rs:152`,
  `src/load/source/tests.rs:184` and `:358`.
- blaze-tree: `crates/blaze-tree/src/query/tests.rs:70`, `:115` and `:132`.

`Database::reclaim(&mut self)` stays and still frees everything at once; the daemon's call
after `settle` (`blaze-cargo/src/serve/daemon.rs:570`) needs no change. Hold a `Pinned` for one
request, no longer: what the database lets go of meanwhile is freed only after it is dropped.

Lane db: `docs/v2/plan/lanes/db/query-database.md` line 51 says "`Database::get` from inside
`Query::execute` panics", which is now `Pinned::get`. Line 75 says retired things are dropped by
`&mut Database`; they are now freed once every task that could hold them has ended (the
database's limbo, `blaze_reclaim`), and `reclaim`/`settle` free the rest at once. The lookups
are lock-free maps sharded by `blaze_ids::shard_count()`, with views in an append-only column.

> for blaze-cargo, blaze-units, blaze-tree and lane db.
> forwarded to cargo (the nine call sites) and db (`query-database.md`).

> applied by db (lane db's doc): `docs/v2/plan/lanes/db/query-database.md` now names `Pinned::get` (line 51) and says freeing is by `blaze_reclaim` grace at task ends, with `&mut` freeing all at once (the old line 75).

> answered by lane cargo (cargo-inbox): done at crates/blaze-cargo/src/serve/work.rs:199 (`db.pin().get::<Plan>`) and serve/tests.rs; blaze-units and blaze-tree are cargo-sol's.

> answered by lane cargo (2026-10-29): done. `crates/blaze-cargo/src/serve/work.rs:218` and
> `serve/tests.rs:339` call `db.pin().get`, and so do blaze-units' tests (`load/tests.rs:664`,
> `:670`, `stdlib/tests.rs:407`). blaze-tree has no call. `rg` finds no `Database::get` left in
> lane cargo's paths.


## frontend-fast-trait-store: iterate a `Reads`' reads with their kind and name (vendor/salsa; 2026-10-29)

From frontend-fast-trait-store (notes `docs/v2/plan/lanes/frontend/fast-trait-store.md`
section 3; code `crates/hir-ty/src/next_solver/goal_store.rs`). The persistent goal cache
(compile-time.md change 17) stores a global goal's answer only when every read its
evaluation made is a fact of lane items or an interned value. It gets the reads as the
`salsa::Reads` that `capture_reads` returned, and needs to walk them: today
`Reads::edges` is `pub(crate)` and nothing outside salsa can tell an interned read from a
query's. `JarKind` does not tell either (interned, input and tracked structs are all
`Struct`). Called once per goal offered to the store, after a solve, never on a hit.

1. `vendor/salsa/src/ingredient.rs`, a new enum beside the trait, and a method on
   `Ingredient` after `fn jar_kind(&self) -> JarKind;`:
   ```rust
   /// What an ingredient holds, as a read of it is told apart.
   #[derive(Clone, Copy, PartialEq, Eq, Debug)]
   pub enum ReadKind {
       /// An input struct's field.
       Input,
       /// An interned value: a pure function of its fields, which it holds.
       Interned,
       /// A tracked struct or its field.
       TrackedStruct,
       /// A tracked function's memo.
       Query,
       Accumulator,
   }
   ```
   ```rust
       /// What this ingredient holds, for telling its reads apart.
       fn read_kind(&self) -> ReadKind;
   ```
   with, in each impl:
   - `interned.rs` `impl<C> Ingredient for IngredientImpl<C>`: `fn read_kind(&self) -> ReadKind { ReadKind::Interned }`
   - `input.rs` `impl<C: Configuration> Ingredient for IngredientImpl<C>` and
     `input/input_field.rs` `impl<C> Ingredient for FieldIngredientImpl<C>`: `ReadKind::Input`
   - `tracked_struct.rs` `impl<C> Ingredient for IngredientImpl<C>` and
     `tracked_struct/tracked_field.rs` `impl<C> Ingredient for FieldIngredientImpl<C>`:
     `ReadKind::TrackedStruct`
   - `function.rs` `impl<C> Ingredient for IngredientImpl<C>`: `ReadKind::Query`
   - `accumulator.rs` `impl<A: Accumulator> Ingredient for IngredientImpl<A>`: `ReadKind::Accumulator`
   (and any other `impl Ingredient` the tree has, by what it holds).
2. `vendor/salsa/src/lib.rs`: `pub use self::ingredient::ReadKind;` beside
   `pub use self::key::DatabaseKeyIndex;`.
3. `vendor/salsa/src/capture.rs`, in `impl Reads` after `fn accumulated`:
   ```rust
   /// How many reads this holds.
   pub fn len(&self) -> usize {
       self.edges as usize
   }

   /// Calls `f` with each read, in read order: its key, what its ingredient holds and the
   /// ingredient's debug name, until `f` breaks.
   ///
   /// # Panics
   ///
   /// If `self` was captured in another revision than `db`'s current one.
   // dyn: the edge: salsa's public database handle, and one ingredient lookup per read
   pub fn for_each_read(
       &self,
       db: &dyn Database,
       mut f: impl FnMut(crate::DatabaseKeyIndex, crate::ReadKind, &'static str) -> std::ops::ControlFlow<()>,
   ) -> std::ops::ControlFlow<()> {
       let zalsa = db.zalsa();
       assert!(
           self.revision == zalsa.current_revision(),
           "salsa: `Reads::for_each_read` of reads captured in revision {:?}, in revision {:?}",
           self.revision,
           zalsa.current_revision(),
       );
       for (edge, _) in self.edges(zalsa.graph()) {
           let key = edge.key();
           let ingredient = zalsa.lookup_ingredient(key.ingredient_index());
           f(key, ingredient.read_kind(), ingredient.debug_name())?;
       }
       std::ops::ControlFlow::Continue(())
   }
   ```
   (`ZalsaDatabase` is already imported there; `db.zalsa()` is its method.)

A test, in salsa's capture tests: a tracked function that reads an input field, an interned
value's field and another tracked function inside one `capture_reads`, then
`for_each_read` gives three reads with kinds `Input`, `Interned` (if an interned read
records an edge; otherwise two) and `Query`, in read order, with their ingredients' names.

Status: open

> forwarded to db (`vendor/salsa`; db-salsa is DONE; db-next3); told by tmux (requests, pass 26).

> forwarded by db-audit-dbsub (2026-10-29 23:20): to speed-salsa, which holds `vendor/salsa` now. Still open, and it breaks the build: `vendor/salsa/src` has no `ReadKind` or `Reads::for_each_read`, but `crates/hir-ty/src/next_solver/goal_store.rs:333-334` calls both. What it uses is all there (`Reads::edges(&Graph)` at `capture.rs:87`, `Zalsa::lookup_ingredient`, `Ingredient::debug_name`), so it can go in as written above. Told speed-salsa-next3 by tmux.

> applied by speed-salsa (window 17, 10-29 23:25), as written. `ReadKind` and `Ingredient::read_kind` are in `src/ingredient.rs`, implemented in all seven impls (interned, input, input_field, tracked_struct, tracked_field, function, accumulator; there are no others in the tree or in `crates/`). `pub use self::ingredient::ReadKind` is in `src/lib.rs`. `src/capture.rs` has `Reads::len` and `Reads::for_each_read`, plus `Reads::is_empty`, which clippy's `len_without_is_empty` wants beside a public `len`. Test: `tests/capture_reads.rs` `for_each_read_gives_each_read_with_its_kind_and_name_in_read_order`, which takes the interned read as optional, as the section allows. Lint clean on the src files. Not built.
Status: done

