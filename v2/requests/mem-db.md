# Requests of mem-db (the query database and the front end onto blaze-mem)

The contract every change below and in mem-db's children's files programs against is in
`docs/v2/plan/lanes/mem/db.md` ("The contract"). The children's files:
`v2/requests/mem-db-salsa.md` (vendor/salsa), `v2/requests/mem-db-infer.md` (hir-ty but its
solver), `v2/requests/mem-db-solver.md` (hir-ty's `next_solver/`, rustc_type_ir,
rustc_next_trait_solver), `v2/requests/mem-db-def.md` (base-db, hir-expand, hir, hir-def),
`v2/requests/mem-db-checks.md` (borrowck, wfcheck).

## 1. lane mem (`crates/blaze-mem/src/frame.rs`): `frame::current`, the running task's frame

- Why: a query's function is called as `fn(db, key)`, through salsa's and blaze-query's
  executors, so the frame `frame::task` hands the executor's closure can't reach the code that
  allocates (an inference context built three calls down). Every front-end change in these
  requests opens `frame::current` at the top of a query's body and threads the `&Frame` it
  gets into its context structs.
- What: beside `task`:

```rust
thread_local! {
    /// The frame of the innermost task this thread is in; null in none.
    static CURRENT: Cell<*const Frame> = const { Cell::new(ptr::null()) };
}

/// Runs `f` with the frame of the innermost [`task`] the calling thread is in. Stops, naming the
/// caller, when it is in none: code that allocates for a task runs in one (no-fallback).
#[track_caller]
pub fn current<R>(f: impl FnOnce(&Frame) -> R) -> R {
    let frame = CURRENT.with(Cell::get);
    assert!(!frame.is_null(), "frame::current outside any frame::task");
    // SAFETY: `task` points `CURRENT` at the frame its `Back` holds on `task`'s own stack for as
    // long as its closure runs, and points it back at the outer one before that frame is reset
    // or moved. This call is inside that closure, deeper on the same thread's stack, so the
    // frame is alive until `f` returns, and it is not reset meanwhile: a reset takes `&mut`,
    // which nobody has while `task` lends `&Frame`.
    f(unsafe { &*frame })
}
```

  and in `task`:

```rust
pub fn task<R>(tag: Tag, f: impl FnOnce(&Frame) -> R) -> R {
    /// The frame, and the frame `CURRENT` named before this task.
    struct Back(Option<Frame>, *const Frame);
    impl Drop for Back {
        fn drop(&mut self) {
            // A thread being torn down has no `CURRENT` left to put back.
            let _ = CURRENT.try_with(|current| current.set(self.1));
            if let Some(mut frame) = self.0.take() {
                frame.reset();
                give_spare(frame);
            }
        }
    }
    let frame = match take_spare() {
        Some(frame) => frame,
        None => match Frame::new(tag) {
            Ok(frame) => frame,
            Err(error) => fail(error),
        },
    };
    frame.set_tag(tag);
    let back = Back(Some(frame), CURRENT.with(Cell::get));
    match &back.0 {
        Some(frame) => {
            CURRENT.with(|current| current.set(frame));
            f(frame)
        }
        None => unreachable!("the frame is taken back only on drop"),
    }
}
```

  (`frame.rs` already imports `Cell` and `ptr`.)
- Tests (`crates/blaze-mem/src/tests.rs`): inside `task(Tag::Infer, ..)`, `current` gives that
  frame (`current(|frame| frame.tag()) == Tag::Infer`, and an allocation through it shows in
  the task's `used`); inside a nested `task(Tag::Mir, ..)` it gives the inner one, and after the
  inner returns the outer one again; outside any task it panics (`#[should_panic]`); after a
  task unwinds (`catch_unwind`) `current` outside panics again (the outer pointer is restored).
  Budget: `assert_at_most(0, ..)` over 1,000 warm `task(.., |_| current(|f| f.used()))`.

> forwarded to mem (from requests, pass 16; told by tmux).

> done by mem (2026-10-29): `frame::current(f)` and `frame::in_task()` as asked (`task` points a
> thread-local at its frame and restores the outer one when it ends; `current` panics outside any
> task), with tests for nesting and for a call outside a task. mem-frame's rewrite of `frame.rs`
> kept them.

## 2. lane mem (`crates/blaze-mem/src/pool.rs`): a `Pooled`'s raw round trip

- Why: request 4 below. The solver cache publishes each generation as an `AtomicPtr` in a slot
  that readers load lock-free; the writer that unlinks it must take its `Pooled` back to retire
  it into a `Limbo`, as `Box::into_raw`/`Box::from_raw` do today.
- What: in `impl<T> Pooled<'_, T>`, change the impl's header to `impl<'p, T> Pooled<'p, T>` and
  add:

```rust
    /// The value's address, its slot kept: for a table that publishes it by pointer.
    /// [`Pooled::from_raw`] takes it back.
    pub fn into_raw(self) -> NonNull<T> {
        mem::ManuallyDrop::new(self).ptr
    }

    /// The `Pooled` [`Pooled::into_raw`] gave `ptr` for.
    ///
    /// # Safety
    ///
    /// `ptr` came from `into_raw` of a `Pooled` of `pool`, and is taken back once.
    pub unsafe fn from_raw(pool: &'p Pool<T>, ptr: NonNull<T>) -> Pooled<'p, T> {
        let offset = ptr.as_ptr() as usize - pool.slots.base().as_ptr() as usize;
        debug_assert!(offset % Pool::<T>::SIZE == 0 && offset / Pool::<T>::SIZE < pool.max as usize);
        Pooled { pool, index: (offset / Pool::<T>::SIZE) as u32, ptr }
    }
```

- Test: a value made, `into_raw`, `from_raw` and dropped is dropped once and its slot reused by
  the next allocation of the same cache (its `index()` is the first one's); the round trip
  allocates nothing (`assert_at_most(0, ..)`).

> forwarded to mem (from requests, pass 16; told by tmux).

> done by mem (2026-10-29): `Pooled::into_raw` and `unsafe Pooled::from_raw(pool, ptr)` as written (the impl
> is now `impl<'p, T> Pooled<'p, T>`). Test `pool_raw_round_trip_keeps_the_slot_and_drops_once`:
> dropped once, the slot is the cache's next allocation, and the round trip allocates nothing.

## 3. root `Cargo.toml` (requests): hashbrown's `nightly` feature

- What: `hashbrown = { version = "0.17.0", features = ["inline-more", "nightly"], default-features = false }`
  (line 290).
- Why: with `nightly`, hashbrown's `HashMap<K, V, S, A>` takes `core::alloc::Allocator`, so a
  run's maps and sets are `hashbrown::HashMap<K, V, FxBuildHasher, &Frame>` (contract 2); std's
  `HashMap` takes no allocator. Every crate in the tree builds on the pinned nightly, so the
  feature changes no other user's behavior. Then a crate that uses it adds
  `hashbrown.workspace = true` (each request below says which).

> applied by requests: `"nightly"` added to the root `hashbrown` features. It locks nothing new: `nightly`'s `bumpalo/allocator_api` names a dev-dependency of hashbrown, and `foldhash?/` is optional and off.

## 4. solver-sync-glue (`crates/hir-ty/src/next_solver/interner/tls_cache.rs`): generations from a pool, retired through its limbo

Closes solver-sync-glue's request 8 (`v2/requests/solver-sync-glue.md`). Needs request 2 above,
and solver-sync-glue's requests 6 (`blaze-alloc-count` in hir-ty's tests) and 10
(`blaze-mem.workspace = true` in `crates/hir-ty/Cargo.toml`), which db holds.

- Why: today a miss makes a `Box<Generation>` (about 2.5 KB) and a replace boxes the old one
  again into `blaze_reclaim::Limbo` (`Box<dyn Send + Sync>`, and its `Vec` grows). Misses are in
  the warm loop: an edit advances the revision, so each shared scope in use is replaced once,
  and every query that solves goals whose reads weren't captured makes a new `Solved`, so a
  replace in the per-query table. With a pool tagged `Infer` and its limbo, a replace takes a
  slot a grace freed and makes no allocation; the cache's bytes show in `Tag::Infer`'s budget.
- The pool's size: a replaced generation waits in the limbo until every task that was running
  when it was unlinked has ended, and a thread's outermost task can be a whole top-level query,
  so many may wait. Each pool reserves `1 << 20` generations (about 2.5 GB of address space,
  nothing committed until carved, 4 MB of links); a pool used up stops with `MemError` naming
  `Infer`, never a box (no-fallback).
- Imports: replace `use blaze_reclaim::Limbo;` with
  `use blaze_mem::{Cache, Pool, Pooled, Tag, pool::Limbo};` and add `ptr::NonNull` to the
  `std::{.. mem, ptr, ..}` list (`std::{cell::Cell, hash::BuildHasher, mem, ptr::{self, NonNull}, sync::{..}}`).
- `Generation`'s doc: "freed only through `Shared::limbo`" becomes "dropped, and its slot
  reused, only through its table's limbo".
- `Shared` (lines 99-113) becomes:

```rust
struct Shared<'p, K, S, T> {
    /// A hash of the key each slot holds, 0 for none, so a probe loads one word; the
    /// generation says which key it is.
    hints: Box<[AtomicU64]>,
    generations: Box<[AtomicPtr<Generation<K, S, T>>]>,
    /// Where generations are made, each published from [`Pooled::into_raw`].
    pool: &'p Pool<Generation<K, S, T>>,
    /// Locked only to make or replace a generation.
    slots: Mutex<Slots<'p, Generation<K, S, T>>>,
}

/// A table's way into its pool: the cache it makes generations from, and the limbo holding
/// those replaced until no task can still be reading them, then giving their slots back.
struct Slots<'p, G> {
    cache: Cache<'p, G>,
    limbo: Limbo<'p, G>,
}

/// How many generations a table's pool holds: its slots, and those replaced that wait for
/// their grace (see `pool`).
const POOLED: u32 = 1 << 20;

/// The generations of a table, tagged `Infer`; reserved, none committed until made.
fn pool<G>() -> Pool<G> {
    Pool::new(POOLED, Tag::Infer).unwrap_or_else(|error| panic!("the trait solver's cache: {error}"))
}
```

- The statics (lines 118-119 and 395):

```rust
static SHARED_GENERATIONS: LazyLock<Pool<Generation<Scope, Revision, Erased>>> = LazyLock::new(pool);

static SHARED: LazyLock<Shared<'static, Scope, Revision, Erased>> =
    LazyLock::new(|| Shared::new(SHARED_BITS, LazyLock::force(&SHARED_GENERATIONS)));

static OF_QUERY_GENERATIONS: LazyLock<Pool<Generation<Solved, (), OfQuery>>> = LazyLock::new(pool);

static OF_QUERY: LazyLock<Shared<'static, Solved, (), OfQuery>> =
    LazyLock::new(|| Shared::new(SLOT_BITS, LazyLock::force(&OF_QUERY_GENERATIONS)));
```

- `impl<K, S, T> Shared<K, S, T>` becomes `impl<'p, K, S, T> Shared<'p, K, S, T>` (same bounds);
  `new`:

```rust
    fn new(bits: u32, pool: &'p Pool<Generation<K, S, T>>) -> Shared<'p, K, S, T> {
        let len = 1usize << bits;
        Shared {
            // alloc: setup, once per table and process.
            hints: (0..len).map(|_| AtomicU64::new(EMPTY)).collect(),
            // alloc: setup, once per table and process.
            generations: (0..len).map(|_| AtomicPtr::new(ptr::null_mut())).collect(),
            pool,
            slots: Mutex::new(Slots {
                cache: pool.cache(),
                limbo: Limbo::new(pool).unwrap_or_else(|error| panic!("the trait solver's cache: {error}")),
            }),
        }
    }
```

- `get`'s SAFETY comment: "a generation is freed only by the limbo, after a grace started once it
  was unlinked from its slot" becomes "a generation is dropped and its slot reused only by the
  table's limbo, after a grace started once it was unlinked from its slot".
- `replace` (lines 266-308) becomes:

```rust
    #[cold]
    fn replace(
        &self,
        at: usize,
        held: *mut Generation<K, S, T>,
        hint: u64,
        key: K,
        state: S,
    ) -> Result<&T, Option<&T>> {
        let mut slots = self.slots.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let made = slots
            .cache
            .alloc(Generation { key, state, referenced: AtomicBool::new(true), value: T::default() })
            .into_raw();
        if let Err(won) =
            self.generations[at].compare_exchange(held, made.as_ptr(), Ordering::AcqRel, Ordering::Acquire)
        {
            // SAFETY: `made` came from `into_raw` of this pool just now and was never published.
            slots.cache.recycle(unsafe { Pooled::from_raw(self.pool, made) });
            drop(slots);
            return Err(self
                .get(won)
                .filter(|won| won.key == key && won.state == state)
                .map(|won| &won.value));
        }
        self.hints[at].store(hint, Ordering::Release);
        if let Some(held) = NonNull::new(held) {
            // SAFETY: every generation in a slot was published from `into_raw` of this table's
            // pool, and the swap above unlinked `held`: one swap alone can, so this is the one
            // place that takes it back; the limbo drops it only after every task that may have
            // loaded it has ended.
            slots.limbo.retire(unsafe { Pooled::from_raw(self.pool, held) });
            slots.limbo.collect();
        }
        drop(slots);
        // SAFETY: published above; dropped only by the limbo, after the caller's guard is
        // dropped (see `of`).
        Ok(unsafe { &(*made.as_ptr()).value })
    }
```

- `retire` (lines 310-315) goes. `clear` (lines 317-327) becomes:

```rust
    /// Unlinks every generation.
    fn clear(&self) {
        let mut slots = self.slots.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        for (hint, slot) in self.hints.iter().zip(&self.generations) {
            hint.store(EMPTY, Ordering::Release);
            if let Some(held) = NonNull::new(slot.swap(ptr::null_mut(), Ordering::AcqRel)) {
                // SAFETY: as in `replace`: the swap unlinked it, and only one swap can.
                slots.limbo.retire(unsafe { Pooled::from_raw(self.pool, held) });
            }
        }
        slots.limbo.collect();
    }
```

- `Drop` (lines 330-340) becomes:

```rust
impl<K, S, T> Drop for Shared<'_, K, S, T> {
    fn drop(&mut self) {
        for slot in &mut self.generations {
            if let Some(held) = NonNull::new(mem::replace(slot.get_mut(), ptr::null_mut())) {
                // SAFETY: `&mut self`: no reader is left; each generation is in one slot and was
                // published from `into_raw` of this table's pool.
                drop(unsafe { Pooled::from_raw(self.pool, held) });
            }
        }
        // `slots` drops next: the limbo drops what it holds, its owner being gone.
    }
}
```

- Tests (`mod tests`): `use super::{EMPTY, Key, SOLVED_BLOCK, Shared, Solved};` gains `pool`;
  add

```rust
    /// A table of a pool of its own, leaked so the table is `'static` as the process's are.
    fn table<T: Default + Send + Sync + 'static>(bits: u32) -> Shared<'static, Solved, u32, T> {
        // alloc: a test's pool, once a test.
        Shared::new(bits, Box::leak(Box::new(pool())))
    }
```

  and replace each `Shared::<Solved, u32, SharedGoals>::new(4)` with `table::<SharedGoals>(4)`,
  `Shared::<Solved, u32, Marked>::new(4)` with `table::<Marked>(4)`, `::new(5)` with
  `table::<SharedGoals>(5)`; `holding`'s parameter becomes `&Shared<'_, Solved, u32, SharedGoals>`.
  `a_miss_into_an_empty_slot_allocates_its_generation_alone` becomes
  `a_miss_into_an_empty_slot_allocates_nothing` with `assert_at_most(0, ..)` (the pool carves by
  committing reserved pages, not through the allocator). New:

```rust
    /// A replace takes a slot of the pool and hands the old generation to the limbo, which
    /// gives its slot back once its grace has passed: no allocation, warm or waiting.
    #[test]
    fn a_warm_replace_allocates_nothing() {
        let table = table::<SharedGoals>(4);
        let query = key();
        let batch = blaze_mem::pool::BATCH as u32;
        // The first batch is carved and the limbo's column committed; this thread's reclaim
        // slot is claimed.
        for state in 0..2 * batch {
            let task = blaze_reclaim::enter();
            table.of(&task, query, state);
        }
        blaze_alloc_count::assert_at_most(0, "a replace in a new state", || {
            for state in 2 * batch..64 * batch {
                let task = blaze_reclaim::enter();
                table.of(&task, query, state);
            }
        });
        assert_eq!(holding(&table, query), 1);
    }
```

  `a_replaced_value_is_freed_only_after_its_readers_left_their_task` holds as written: the
  limbo drops `Marked` when it collects.
- Left: the cache's own chunks as a solve fills it (`GlobalCache`'s `Chunked`/`Interned`) still
  come from the general-purpose allocator: solver-sync-glue's request 9 (solver-sync's) and
  `mem-db-solver`'s file cover them.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-a-solver (21:55): as written (with section 13's line refresh): `Shared<'p, ..>` with `pool` and `Slots { cache, limbo }`, the two pool statics, `replace`/`clear`/`Drop` through `Pooled`, `retire` gone, the `table` test helper, `a_miss_into_an_empty_slot_allocates_nothing` and `a_warm_replace_allocates_nothing`. Waits on request 2 (`Pooled::into_raw`/`from_raw`, lane mem, not yet in `pool.rs`).

## 5. lane db (blaze-query, `crates/blaze-query/`): each run a frame task of its query's tag

- `src/lib.rs`, first line: `#![feature(allocator_api)]` (for requests 6 and 7).
- `Query` (line 207): after `const PRODUCER`:

```rust
    /// The subsystem its runs allocate for: each run is a `blaze_mem::frame::task` of it, so
    /// what the run's code takes through `blaze_mem::frame::current` counts there.
    const TAG: blaze_mem::Tag = blaze_mem::Tag::Query;
```

  (`blaze_mem::Tag` by path: `lib.rs` imports `blaze_core::Tag`.)
- `KindInfo` (line 306): `tag: blaze_mem::Tag,`; `register` (line 966): `tag: Q::TAG,`.
- `Database::execute` (line 1685): `let computed = (info.run)(&mut cx, source)?;` becomes

```rust
        let computed = blaze_mem::frame::task(info.tag, |_| (info.run)(&mut cx, source))?;
```

  The value is written into the thread's buffer inside the task (`run_query`), and `Computed`
  owns its bytes, so nothing of the frame outlives it. A query asked inside that misses runs in
  its own task through this same line, one frame deeper.
- Test (`src/tests.rs`):

```rust
/// Its runs are tasks of `Tag::Parse`: it answers its frame's tag and [`Double`] of its key.
struct Tagged;

impl Query for Tagged {
    const KIND: QueryKind = QueryKind(0x2f0d);
    const NAME: &'static str = "tagged";
    const PRODUCER: ContentId = ContentId([15; 32]);
    const TAG: blaze_mem::Tag = blaze_mem::Tag::Parse;
    type Key = String;
    type Value = Num;
    fn input(key: &String) -> ContentId {
        key_id(key)
    }
    fn execute(cx: &mut Cx<'_>, key: &String) -> Result<Num, Cancelled> {
        // A miss: `double`, then `len`, run in tasks of their own and give the frame back.
        let inner = cx.get::<Double>(key)?;
        let tag = blaze_mem::frame::current(|frame| {
            let mut bytes = Vec::new_in(frame);
            bytes.extend_from_slice(key.as_bytes());
            assert_eq!(bytes.len(), key.len());
            frame.tag().index() as u64
        });
        Ok(Num(tag << 32 | inner))
    }
}

#[test]
fn a_run_is_a_frame_task_of_its_querys_tag() {
    let (mut db, tree) = setup();
    db.register::<Tagged>();
    let one = view(&db, &tree, &[("a", "abc")]);
    let parse = blaze_mem::Tag::Parse.index() as u64;
    assert_eq!(db.pin().get::<Tagged>(one, &"a".to_owned()), Ok(parse << 32 | 6));
}
```

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-a-query (21:54): as written (`Query::TAG`, `KindInfo.tag`, `execute` in `frame::task(info.tag, ..)`, test `a_run_is_a_frame_task_of_its_querys_tag`); `blaze_mem::{Tag, frame}` by path, as lib.rs has a private `struct Frame`.

## 6. lane db (blaze-query): a revalidation in a new view allocates nothing

- Why: every edit opens a view, and each `ViewState::new` (`lib.rs:737`) makes three `Map`s:
  each reserves two regions (system calls) and boxes its shards, and each shard's first insert
  allocates a table, which doubles as the view resolves. `close` boxes the state again into
  `blaze_reclaim::Limbo`. So revalidating an unchanged key in a new view, the warm loop's
  commonest path, allocates. The fix keeps closed views' states, emptied with their tables, for
  the next views opened.
- `table.rs`, `Column` (line 924): `push` goes through a new `push_boxed`:

```rust
    /// Appends `make(index)` at the next index.
    pub(crate) fn push(&self, make: impl FnOnce(u32) -> T) -> u32 {
        // alloc: one a value of a column (a view's state: until closed views are reused).
        self.push_boxed(|index| Box::new(make(index)))
    }

    /// Appends the value `make(index)` boxed (a box the caller kept) at the next index.
    pub(crate) fn push_boxed(&self, make: impl FnOnce(u32) -> Box<T>) -> u32 {
        let index = self.next.fetch_add(1, Ordering::Relaxed);
        assert!(index < u32::MAX, "a column of 2^32 values");
        self.place(index).store(Box::into_raw(make(index)), Ordering::Release);
        index
    }
```

- `table.rs`, `Map` (after `reclaim`, line 889):

```rust
    /// Empties the map, keeping its tables (each at its length), rows and committed pages: for
    /// a map used again once no reader can be in it.
    pub(crate) fn reset(&mut self) {
        self.reclaim();
        let rows = &self.rows;
        for shard in self.shards.iter_mut() {
            let table = *shard.table.get_mut();
            let writer = shard.writer.get_mut();
            if !table.is_null() {
                // SAFETY: the current table, owned by the map; `&mut` excludes every reader.
                for place in unsafe { places(table) } {
                    if let Some(row) = row_of(place.load(Ordering::Relaxed)) {
                        // SAFETY: a place names a live row of this map, dropped once here.
                        unsafe { rows.drop_row(row) };
                        writer.free.ready(row);
                    }
                    place.store(EMPTY, Ordering::Relaxed);
                }
            }
            writer.live = 0;
            writer.used = 0;
        }
    }
```

- `lib.rs`, `ViewState` (line 736), beside `reclaim`:

```rust
    /// Empties it for another view, its maps' tables kept: no reader can be in it.
    fn reset(&mut self) {
        self.results.reset();
        self.inputs.reset();
        self.reruns.reset();
    }

    /// Makes it the state of `view`, as `ViewState::new` would.
    fn reuse(&mut self, id: ViewId, view: View, config: &[u8]) {
        self.id = id;
        self.view = view;
        if *self.config != *config {
            // alloc: a configuration other than the one of the last view this state served.
            self.config = Arc::from(config);
        }
    }
```

- `lib.rs`, beside `Kinds`:

```rust
/// Closed views' states, kept to be the next views opened: each held until no task can still
/// read it, then emptied, its maps' tables kept, so opening a view and resolving in it
/// allocates nothing once the database has had as many views open at once.
#[derive(Default)]
struct SpareViews {
    /// Oldest first.
    held: VecDeque<(Grace, Box<ViewState>)>,
    ready: Vec<Box<ViewState>>,
}

/// Emptied states kept; one more closed is freed.
const SPARE_VIEWS: usize = 8;

impl SpareViews {
    /// An emptied state, emptying first those whose grace has passed.
    fn take(&mut self) -> Option<Box<ViewState>> {
        while self.held.front().is_some_and(|(grace, _)| grace.passed()) {
            let Some((_, mut state)) = self.held.pop_front() else { break };
            if self.ready.len() < SPARE_VIEWS {
                state.reset();
                self.ready.push(state);
            }
        }
        self.ready.pop()
    }
}
```

  `Database` gains `spare_views: ShortLock<SpareViews>,` (after `limbo`), `blank` sets
  `spare_views: ShortLock::default(),`; `use std::collections::VecDeque` joins the `std` import.
- `open_with` (line 983) and `close` (line 989) become:

```rust
    pub fn open_with(&self, view: View, config: &[u8]) -> ViewId {
        let spare = self.spare_views.lock().take();
        Idx::from_raw(self.views.push_boxed(|index| match spare {
            Some(mut state) => {
                state.reuse(Idx::from_raw(index), view, config);
                state
            }
            // alloc: a view's state, until as many views were closed as are open at once.
            None => Box::new(ViewState::new(Idx::from_raw(index), view, Arc::from(config))),
        }))
    }

    pub fn close(&self, view: ViewId) {
        if let Some(state) = self.views.take(view.raw()) {
            // The take unlinked it: a grace begun now covers every task that could read it.
            self.spare_views.lock().held.push_back((Grace::start(), state));
        }
    }

    /// Emptied view states ready for the next views opened.
    #[cfg(test)]
    fn ready_views(&self) -> usize {
        let mut spare = self.spare_views.lock();
        if let Some(state) = spare.take() {
            spare.ready.push(state);
        }
        spare.ready.len()
    }
```

- Test (`src/tests.rs`, `use std::thread;`):

```rust
/// A view opened after one closed takes its state, emptied with its tables: revalidating a key
/// whose inputs did not change there allocates nothing, and runs nothing.
#[test]
fn a_revalidation_that_changes_nothing_allocates_nothing() {
    let (db, tree) = setup();
    let first = view(&db, &tree, &[("a", "abc")]);
    assert_eq!(double(&db, first, "a"), 6);
    let root = tree.root(&[("a", b"abc".as_slice())]);
    let name = "a".to_owned();
    // Warm: the closed state's grace passes once other tests' tasks that began before it end.
    let mut warm = 0;
    while db.ready_views() == 0 {
        let again = db.open(View { root });
        assert_eq!(db.pin().get::<Double>(again, &name), Ok(6));
        db.close(again);
        warm += 1;
        assert!(warm < 100_000, "a closed view was never ready again");
        thread::yield_now();
    }
    let reused = db.open(View { root });
    let pinned = db.pin();
    let got = blaze_alloc_count::assert_at_most(0, "revalidating double(a) in a reused view", || {
        pinned.get::<Double>(reused, &name)
    });
    assert_eq!(got, Ok(6));
    assert_eq!((runs("double"), runs("len")), (1, 1), "verified, not run again");
}
```

  (`MemTree::root` names a tree by its files' content, so the same files give `first`'s root.)

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-a-query (21:54): as written, plus: `close` was `retire`'s only caller and `limbo` held only closed views, so both went; `held()`, `collect_retired` and `reclaim` use `spare_views` (`SpareViews::sweep`, a `reclaim` for `&mut`), `ready_views` calls `sweep`; `Column::push` (now test-only) is `#[cfg(test)]`.

## 7. lane db (blaze-query): the collection, the sweep and same-length rehashes allocate nothing warm

- Why: `Database::collect` (lib.rs:1045) builds two `Marks` (`vec![0; n]`), `inputs_kept` (an
  `FxSet`), `work`, `unlinked`, `live` and `cold` each time; `Store::to_check` (memo.rs:576)
  `mem::take`s `dying.held`, so the next `checked` push reallocates it; `Store::sweep`
  (memo.rs:624) collects and partitions into three `Vec`s whenever memos die;
  `Map::edit_all_with` (table.rs:778) makes `edits` and `gone` each pass; and a shard whose
  entries are removed and put back (a collection, then the next edit) rehashes into a new
  `Box<[AtomicU64]>` of the length it had (table.rs:656), boxed again into the shard's
  `blaze_reclaim::Limbo`. All of these run in the warm loop, once an edit.
- `blaze-query/Cargo.toml`: `hashbrown.workspace = true` (request 3).
- `lib.rs`, `Marks` (line 668):

```rust
struct Marks<A: Allocator = Global>(Vec<u64, A>);

impl Marks {
    fn new(end: u32) -> Marks {
        Marks::new_in(end, Global)
    }
}

impl<A: Allocator> Marks<A> {
    fn new_in(end: u32, alloc: A) -> Marks<A> {
        let words = (end as usize).div_ceil(64);
        let mut marks = Vec::with_capacity_in(words, alloc);
        marks.resize(words, 0);
        Marks(marks)
    }
    // `insert`, `contains`, `iter` as they are.
}
```

  (`use std::alloc::{Allocator, Global};`.)
- `collect` (lines 1045-1145): the body after `let started = ..` runs in
  `blaze_mem::frame::task(blaze_mem::Tag::Query, |frame| { .. })`, with:
  `let mut dying = Vec::new_in(frame); self.store.to_check(&mut dying);`,
  `Marks::new_in(self.store.end(), frame)` for `reached` and `hot`,
  `let mut inputs_kept = hashbrown::HashSet::with_hasher_in(rustc_hash::FxBuildHasher, frame);`,
  `let mut work = Vec::new_in(frame);`, `let mut unlinked = Vec::new_in(frame);`,
  `let mut live = Vec::new_in(frame);`, `self.store.checked(&dying, |memo| hot.contains(memo));`,
  `self.store.unlinked(&unlinked);`, and `cold` as `let mut cold = Vec::new_in(frame);
  cold.extend(reached.iter().filter(..).map(..));` (the `// alloc:` justifications on those lines
  go: they no longer allocate). `write_cold` keeps its own buffers (it writes a pack: an
  effect's bytes, owned).
- `memo.rs`:

```rust
    /// Memos just unlinked from their slots.
    pub(crate) fn unlinked(&self, memos: &[MemoId]) {
        if memos.is_empty() {
            return;
        }
        let grace = Grace::start();
        self.dying.lock().unlinked.extend(memos.iter().map(|&memo| (grace, memo)));
    }

    /// The memos to check against the open views, before they are walked, added to `out`:
    /// those unlinked whose grace has passed (..), and those a view held at the last check.
    pub(crate) fn to_check<A: Allocator>(&self, out: &mut Vec<MemoId, A>) {
        let mut dying = self.dying.lock();
        out.extend_from_slice(&dying.held);
        dying.held.clear();
        let done = passed(&dying.unlinked);
        out.extend(dying.unlinked.drain(..done).map(|(_, memo)| memo));
    }

    pub(crate) fn checked(&self, memos: &[MemoId], held: impl Fn(MemoId) -> bool) {
        // as now, with `for &memo in memos`
    }
```

  `sweep` (line 624): its body runs in `blaze_mem::frame::task(blaze_mem::Tag::Query, |frame| ..)`,
  `ready` filled as `let mut ready = Vec::new_in(frame); ready.extend(dying.checked.drain(..done).map(|(_, memo)| memo));`
  and the partition into `let (mut pinned, mut free) = (Vec::new_in(frame), Vec::new_in(frame));`
  pushed by `pins.contains_key`. (`use std::alloc::Allocator;`.)
- `table.rs`, `edit_all_with` (line 778): its body runs in
  `blaze_mem::frame::task(blaze_mem::Tag::Query, |frame| { let (mut edits, mut gone) = (Vec::new_in(frame), Vec::new_in(frame)); .. })`.
- `table.rs`, outgrown tables kept by length. `Writer.tables: Limbo` becomes
  `outgrown: Outgrown`:

```rust
/// A table [`new_table`] made, owned: freed when dropped.
struct Table(*mut AtomicU64);

// SAFETY: a table is its places alone, atomics any thread may touch; the owner frees it once.
unsafe impl Send for Table {}

impl Drop for Table {
    fn drop(&mut self) {
        // SAFETY: from `new_table`, unlinked (an outgrown table) or never published, and owned
        // here alone.
        drop(unsafe { table_box(self.0) });
    }
}

/// Tables a shard outgrew, each held until no reader can be probing it, then kept as the
/// shard's next table of its length: a shard whose entries are removed and put back rehashes
/// into it and allocates nothing.
#[derive(Default)]
struct Outgrown {
    /// Oldest first: their graces start under the shard's lock, in order.
    held: VecDeque<(Grace, Table)>,
    /// One past its grace for each length, at log2 of it.
    ready: [Option<Table>; 64],
}

impl Outgrown {
    fn hold(&mut self, grace: Grace, table: Table) {
        self.held.push_back((grace, table));
    }

    /// Keeps each table whose grace has passed as the one of its length (freeing the one it
    /// displaces).
    fn collect(&mut self) {
        while self.held.front().is_some_and(|(grace, _)| grace.passed()) {
            let Some((_, table)) = self.held.pop_front() else { break };
            let (_, len) = untag(table.0);
            self.ready[len.trailing_zeros() as usize] = Some(table);
        }
    }

    /// An empty table of `len` places, if one is kept.
    fn take(&mut self, len: usize) -> Option<*mut AtomicU64> {
        self.collect();
        let table = self.ready[len.trailing_zeros() as usize].take()?;
        let raw = mem::ManuallyDrop::new(table).0;
        // SAFETY: kept past its grace: no reader probes it, and this shard's lock holder owns it.
        for place in unsafe { places(raw) } {
            place.store(EMPTY, Ordering::Relaxed);
        }
        Some(raw)
    }

    fn len(&self) -> usize {
        self.held.len()
    }
}
```

  In `room` (line 656): `let len = ((writer.live + 1) * 4).next_power_of_two().max(FIRST_TABLE);
  let table = writer.outgrown.take(len).unwrap_or_else(|| new_table(len));` and the first table
  (line 646) the same with `FIRST_TABLE`; line 675 becomes
  `writer.outgrown.hold(Grace::start(), Table(current));` (same SAFETY: `current` is unlinked).
  `sweep` (line 857): `writer.outgrown.collect();` (the `is_empty` guard and `// alloc:` go).
  `held` (line 868): `writer.free.retired() + writer.outgrown.len()`. `reclaim` (line 887):
  `writer.outgrown = Outgrown::default();`. `use std::{collections::VecDeque, mem}` as needed;
  `blaze_reclaim::Limbo` leaves `table.rs`'s imports if nothing else there uses it.
- Tests: `src/table/tests.rs`:

```rust
/// A shard whose entries are removed and put back rehashes into the tables it outgrew, kept
/// past their grace: once settled, the cycle allocates nothing.
#[test]
fn a_settled_map_rehashes_without_allocating() {
    let map: Map<u64> = Map::new();
    let cycle = |map: &Map<u64>| {
        {
            let task = blaze_reclaim::enter();
            for n in 0..1024 {
                map.insert(&task, key(n), n);
            }
            map.retain(&task, |_, _| false);
        }
        freed(map);
    };
    for _ in 0..3 {
        cycle(&map);
    }
    blaze_alloc_count::assert_at_most(0, "a settled map's remove-and-insert cycle", || cycle(&map));
    let task = blaze_reclaim::enter();
    assert_eq!(map.get(&task, &key(7)), None);
}
```

  and `src/tests.rs`:

```rust
/// Once warm, a collection that finds nothing new to drop allocates nothing.
#[test]
fn a_warm_collection_allocates_nothing() {
    let (db, tree) = setup();
    let one = view(&db, &tree, &[("a", "abc")]);
    assert_eq!(double(&db, one, "a"), 6);
    let mut effects = Vec::new();
    for _ in 0..3 {
        db.collect(&[], &mut effects);
    }
    blaze_alloc_count::assert_at_most(0, "a warm collection", || db.collect(&[], &mut effects));
    assert!(effects.is_empty(), "an in-memory database writes nothing");
    assert_eq!(double(&db, one, "a"), 6);
}
```

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-a-query (21:54): `Marks` was gone (collect uses `blaze_ids::bits::BitSet`): its bitsets are `Bits<Vec<u64, &Frame>, MemoId>` grown to `store.end()`; `Outgrown` has a manual `Default` (`[Option<Table>; 64]` has none), its `collect` is named `sweep` (hot_lint); memo tests' callers moved to the slice/out-buffer signatures; both tests as written.

## 8. blaze-query: what of its tables is on reserved regions, and what is left (no change asked)

- Done (db-query's request 3): `Rows` (every map's entries, the memo rows, value rows, the
  kinds) and `Generations` are `blaze_mem::Region`s tagged `Tag::Query`, reserved once and
  never moved (`table.rs:104`).
- Request 7 above: a shard's tables are recycled by length, so the warm loop makes none; a
  table of a new length (a map that grew past its high-water mark) still comes from the
  allocator, once per doubling, which is growth, not the warm loop.
- Left, by lane mem's ruling (`v2/requests/mem.md`, "frontend-tokens", answer 2): the value,
  edge and key segments (`segment.rs:122`, 16 KiB to 4 MiB each) hold memo values, each with its
  own end, so they stay on the general-purpose allocator until memos persist into the image;
  one allocation covers at least 16 KiB of misses' copies, none a hit or revalidation.
- Left as setup: `Kinds.numbers` (once per kind registered), each `Map`'s shard array (once per
  map, and with request 6 once per view state ever made), `Column`'s chunks (once per doubling
  of the views ever opened), `Free::ready`/`retired` (grown to their high-water mark, then
  reused: the scratch pattern).

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-a-query (21:54): nothing to change; true as of sections 6 and 7 applied.

## 9. Per-call scratch on blaze-query's miss paths (lane db)

The hit (`Cx::get`, `Pinned::get`: `a_warm_hit_allocates_nothing`) and verification
(`Database::verify`, `fingerprint_in`: request 6's test) paths already allocate nothing. Two
miss paths make a buffer per call:

- `add_memo` (lib.rs:1468): `let mut all = Vec::with_capacity(memos.len() + 1);` becomes

```rust
            Some(blaze_mem::scratch::with_u32s(|all| {
                all.extend_from_slice(memos);
                all.push(made);
                self.store.slot(slot.and_then(Slot::share_key), all)
            }))
```

  (`MemoId` is `u32`, memo.rs:36; `Store::slot` copies the list.)
- `learn_key` (lib.rs:1446): `cbor::to_vec(key)` becomes

```rust
        let mut bytes = blaze_mem::scratch::lend_bytes();
        cbor::append(&mut bytes, key)
            .unwrap_or_else(|error| panic!("the key of query {} has no encoding: {error}", Q::NAME));
```

  (`Store::key_bytes` copies them into the key segment; `cbor::append` is
  `blaze_protocol::cbor::append`.)
- Budget (`src/tests.rs`): a miss in the warm state allocates at most a new segment for the
  store, the owned home of its value and edges (contract 4):

```rust
/// Once warm, a miss allocates no more than a segment the store opens for what it copies: the
/// run's frame, its value buffer, its key bytes and its slot's list are all reused.
#[test]
fn a_warm_miss_allocates_at_most_a_segment() {
    let (db, tree) = setup();
    let name = "a".to_owned();
    for n in 1..40usize {
        let text = "x".repeat(n);
        let root = tree.root(&[("a", text.as_bytes())]);
        // A closed view's state is ready again once other tests' tasks that began before it end.
        while n > 1 && db.ready_views() == 0 {
            std::thread::yield_now();
        }
        let view = db.open(View { root });
        let pinned = db.pin();
        let budget = if n < 32 { u64::MAX } else { 1 };
        let got = blaze_alloc_count::assert_at_most(budget, "a warm miss of double(a)", || {
            pinned.get::<Double>(view, &name)
        });
        assert_eq!(got, Ok(2 * n as u64));
        drop(pinned);
        db.close(view);
    }
    assert_eq!(runs("len"), 39, "each new content a miss");
}
```

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-a-query (21:54): as written (`with_u32s` in `add_memo`, `lend_bytes` + `cbor::append` in `learn_key`, test `a_warm_miss_allocates_at_most_a_segment`).

## dedup: seeded RNG onto blaze_ids::rng

From `dedup-codec-rng-front` (report `/tmp/blaze-lanes/v2-dedup-codec-rng-front.md`). Every seeded test RNG in the front-end crates moves onto `blaze_ids::rng::SplitMix64` (`new`, `next_u64`, `next_u32`, `below(u64)` = `% n` and 0 for 0, `index(usize)`, `one_in`, `coin`, `pick`, `fill`); no test here pins a stream's values, only thresholds, so the local copy goes. The crate already depends on `blaze-ids`.

In `crates/hir-ty/src/next_solver/interner/tls_cache.rs` (~736), the per-worker xorshift:

```rust
                    let mut seed = 0x2545_F491_4F6C_DD1Du64 ^ (worker + 1);
                    let mut next = move || {
                        seed ^= seed << 13;
                        seed ^= seed >> 7;
                        seed ^= seed << 17;
                        seed
                    };
```
becomes
```rust
                    let mut rng = blaze_ids::rng::SplitMix64::new(0x2545_F491_4F6C_DD1D ^ (worker + 1));
```
and in the loop `queries[next() as usize % queries.len()]` becomes `queries[rng.index(queries.len())]`, `(next() % 64) as u32` becomes `rng.below(64) as u32`. `window` (a hash multiply, not an RNG) stays.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-a-solver (21:55): as written.

## 10. lane mem (`crates/blaze-mem/src/frame.rs`): nested tasks deeper than `DEPTH` without a reservation each (mem-db-salsa's M4)

- The problem (`v2/requests/mem-db-salsa.md`, M4): with one task per query run, salsa's runs nest
  as deep as its query stack, 50 to 300 in this front end. A thread keeps 8 spare frames
  (`frame.rs:41`), so every run past depth 8 reserves a fresh 4 GB frame (`Frame::new`), faults
  its pages in zeroed, and unmaps it on return (`give_spare` drops it): two system calls and a
  fault per page per run in the warm loop, and 4 GB of address space per level per thread.
- M4's fix, one frame per thread shared by nested tasks through markers, is unsound with
  blaze-mem's safe API, so mem-db does not ask for it. An outer task's frame vector can be
  pushed while an inner task runs:

```rust
frame::task(Tag::Infer, |outer| {
    let mut v = Vec::new_in(outer);
    frame::task(Tag::Infer, |_| v.extend_from_slice(&[1u64; 1024]));
    v[1023] // with a shared frame, the inner task's `reset_to` freed this
});
```

  With frames of their own, `v` grows in the outer frame and the inner reset never touches it.
  With a shared frame, `v`'s growth lands above the inner task's marker and is freed under it.
- What mem-db asks instead: the requirement, with the design lane mem's to choose. Nested tasks
  to any depth make no system call and no global allocation once warm, and each task keeps a
  frame of its own. One way: a frame is a chain of `chunks::CHUNK` (2 MB) blocks from the chunk
  cache rather than a 4 GB reservation, so a task past the spare depth takes warm chunks and gives
  them back at its end (one compare-and-swap each). An allocation bigger than a chunk gets a
  dedicated region. The spare depth then only keeps frame headers warm.
- Test: 300 nested `task`s, each pushing 1 KB through `current` and reading it back after its
  inner task returns. The second pass allocates nothing (`assert_at_most(0, ..)`), and
  `chunks::carved_bytes()` doesn't grow between the second and third passes.

> forwarded to mem (mem-next1) (requests, pass 21; told by tmux).

> mem (2026-10-29): agreed, and the requirement stands as written: any depth, no system call and
> no global allocation once warm, and a frame per task. Handed to a new child, `mem-frame` (tmux
> window `mem-frame`, brief `~/restart-2026-10-28/briefs/v2/mem-frame.md`). It owns `frame.rs` and
> `chunks.rs` until it reports, answers here, and tells mem-db.

> written by mem-frame (2026-10-29), not yet built or run (no builds in this pass): a frame is now
> a chain of blocks, 2 MB chunks and, for an allocation over 1 MB, a span (a region of its own,
> reserved four times past it, grown in place by commit). A task's frame is a header on
> `frame::task`'s stack, so every task at any depth has its own. An inner task starts in the
> rest of its enclosing task's chunk; the outer frame leaves that rest alone until the inner
> task ends, and if it allocates meanwhile (your `v.extend_from_slice` example) it takes another
> block, so the inner task's bytes are never overwritten. 300 nested 1 KB tasks therefore share
> one chunk. Blocks past that come from the thread's shelf (8 chunks, 64 spans, no atomic), then
> the chunk cache (one compare-and-swap). The public API is unchanged (`Allocator` with in-place
> growth of the top allocation, `Marker`/`reset_to`/`scope`/`reset`/`trim`,
> `used`/`peak`/`committed`/`reserved`/`contains`, `task`, `current`, `in_task`, `spare_frames`,
> `trim_spare`). `committed()` now counts the blocks a frame holds and `reserved()` its limit,
> 4 GB by default. Tests are in `crates/blaze-mem/src/tests.rs` under `// Frame`:
> `frame_300_nested_tasks_keep_their_bytes_and_cost_nothing_once_warm` (yours, with the
> carved-bytes check made per place, `chunks::carved_on(Some(0))`, with the test thread placed
> on node 0, so chunks other tests carve in parallel don't count; `carved_bytes()` is still
> the sum), `frame_outer_task_grows_while_an_inner_task_allocates` (your example),
> `frame_inner_task_starts_in_the_rest_of_the_outer_chunk`, and
> `frame_matches_a_model_on_random_allocations_grows_shrinks_and_rollbacks`.

> mem-db (22:40): accepted. Read `frame.rs` as written:
> - While an inner task holds the lent rest of a chunk, `Frame::inner` clamps the outer frame's
>   `end` to its cursor, and it never lends inside a span. So `grow_top` refuses, and an outer
>   `Vec` growing then moves to a new block instead of growing over the inner task's bytes.
> - A move to another block clears `lent` (`place`), so `take_back` does nothing then.
> - Task code holds only `&Frame`, so it can't roll back into lent bytes.
>
> One consequence for salsa, the cap's cancel unwinding through a query's claim, is request 14.

## 11. lane mem (`crates/blaze-mem/src/column.rs`): `Column::get_mut` (mem-db-salsa's M9)

Forwarded as written in `v2/requests/mem-db-salsa.md`, M9: the code and its test are there.
salsa's page table (M5) needs the published value at an index under `&mut` for its `&mut` walks.
Sound as written: `&mut self` excludes every reader and appender.

> forwarded to mem (mem-next1) (requests, pass 21; told by tmux).

> done by mem (2026-10-29): `Column::get_mut` as written in M9, with its test (`column_get_mut_changes_a_published_value`).

## 12. lane mem (`crates/blaze-mem/src/scratch.rs`): `Shelf<B, N>` public (mem-db-solver's request 11)

Forwarded as written in `v2/requests/mem-db-solver.md`, request 11. The solver keeps hir-ty types
(`InferTables`, `SearchStorage`, `Pending`) on per-thread shelves deeper than `DEPTH = 4`,
because canonical goals nest 3 to 10 deep. mem-db prefers the `Option<B>` slots form
(`[const { None }; N]`): it needs no const `Default`.

> forwarded to mem (mem-next1) (requests, pass 21; told by tmux).

> done by mem (2026-10-29): `pub struct Shelf<B: Shelved, const N: usize = DEPTH>` with `Option<B>` slots,
> `pub const fn new()` (empty), `take` and `give` (a full shelf keeps its `N` largest), and
> `Default`. blaze-mem's own shelves use `const { RefCell::new(Shelf::new()) }`, which is what
> `recycle.rs` writes. Test `scratch_a_shelf_keeps_its_n_largest` (N = 8).

## 13. lane db (blaze-query, `interner/tls_cache.rs`): requests 4 to 7 and 9 refreshed against the tree (22:50)

blaze-query's `lib.rs` moved about 50 lines down since requests 5 to 9 were written, and one
change landed under request 7. `table.rs` and `memo.rs` did not move. Nothing below changes
what a request does; apply each request as written, with these replacements.

- **Lines.** In `lib.rs`: `Query` is at 259 (its `const PRODUCER` at 263), `KindInfo` at 359,
  `register` at 979 (`run: run_query::<Q>,` at 999), `Database` at 857 (`limbo` at
  869), `blank` at 915, `ViewState` at 723 with `new` at 764, `open_with` at 1011, `close` at
  1016, `collect` at 1069 to 1170, `learn_key` at 1468 (`cbor::to_vec` at 1473), `add_memo` at
  1483 (`Vec::with_capacity(memos.len() + 1)` at 1494), `execute` at 1725 (the
  `(info.run)(&mut cx, source)?` line at 1736). In `table.rs`, `Column` is at 927 and its `push`
  at 947. In `tls_cache.rs` (request 4): `Shared` is at 105 to 112, the statics at 117 to 118
  and 393, `new` at 162, `get`'s SAFETY at 216, `replace` at 266 to 306, `retire` at 308 to 313,
  `clear` at 316 to 325 and `Drop` at 328 to 337. Request 4's code still matches
  `blaze_mem::pool` as it is (`Cache::alloc`/`recycle`, `Limbo::new`/`retire`/`collect`,
  `Region::base`, `pool::BATCH`). It needs only request 2 (`Pooled::into_raw`/`from_raw`, still
  lane mem's). hir-ty's manifest already has `blaze-mem` and `blaze-alloc-count`.
- **Request 7, `collect`'s bitsets.** `Marks` is gone. `collect` now marks with
  `blaze_ids::bits::BitSet<MemoId>` (`lib.rs:1076-1077`), which is `Bits<Vec<u64>, MemoId>`,
  and `Bits` takes words in any allocator's `Vec` (`impl<A: Allocator> GrowWords for Vec<u64, A>`,
  `crates/blaze-ids/src/bits.rs:146`). So drop request 7's `Marks` part, and in the frame task
  make the two sets:

```rust
            let end = self.store.end() as usize;
            let mut reached = Bits::<Vec<u64, &Frame>, MemoId>::from_words(Vec::new_in(frame));
            reached.grow(end);
            let mut hot = Bits::<Vec<u64, &Frame>, MemoId>::from_words(Vec::new_in(frame));
            hot.grow(end);
```

  `insert_growing`, `contains` and `iter` work as they do now. The import becomes
  `use blaze_ids::{ContentId, Fingerprint, Hasher, Idx, bits::{BitSet, Bits}};` (`BitSet` stays
  for `reclaim` at 1337, a `&mut` path) and `use blaze_mem::Frame;`. The rest of request 7's
  `collect` (the task's extent, `dying`, `inputs_kept`, `work`, `unlinked`, `live`, `cold`)
  applies to the current body unchanged. The task closes after the `if self.config.dir.is_some()`
  block, and `self.collect_retired(&task);` stays after it, outside the frame.
- **Requests 6 and 7 together: `Map::reset` keeps the outgrown tables.** Request 6's `reset`
  calls `self.reclaim()`. After request 7, that sets `writer.outgrown = Outgrown::default()` and
  frees the kept tables `reset` is meant to keep. With both applied, `reset` becomes:

```rust
    /// Empties the map, keeping its tables (each at its length), rows and committed pages: for
    /// a map used again once no reader can be in it.
    pub(crate) fn reset(&mut self) {
        let rows = &self.rows;
        for shard in self.shards.iter_mut() {
            let table = *shard.table.get_mut();
            let writer = shard.writer.get_mut();
            // SAFETY: `&mut` excludes every reader.
            writer.free.reclaim_all(|row| unsafe { rows.drop_row(row) });
            writer.outgrown.settle();
            if !table.is_null() {
                // SAFETY: the current table, owned by the map; `&mut` excludes every reader.
                for place in unsafe { places(table) } {
                    if let Some(row) = row_of(place.load(Ordering::Relaxed)) {
                        // SAFETY: a place names a live row of this map, dropped once here.
                        unsafe { rows.drop_row(row) };
                        writer.free.ready(row);
                    }
                    place.store(EMPTY, Ordering::Relaxed);
                }
            }
            writer.live = 0;
            writer.used = 0;
        }
    }
```

  and `Outgrown` gains:

```rust
    /// Keeps every held table as the one of its length, graces or not: for an owner borrowed
    /// `&mut`, which no reader can be in.
    fn settle(&mut self) {
        while let Some((_, table)) = self.held.pop_front() {
            let (_, len) = untag(table.0);
            self.ready[len.trailing_zeros() as usize] = Some(table);
        }
    }
```

  If request 6 lands before request 7, `writer.outgrown.settle();` is
  `writer.tables = Limbo::new();` until request 7 replaces it. That frees the held tables (allowed
  under `&mut`), and the current table is kept either way, so request 6's test holds in both
  orders.
- **Request 5's tag and salsa's.** Both executors take the tag from the query:
  `Query::TAG` here, `Configuration::MEM_TAG` in salsa (mem-db-salsa M1). The tracked attribute
  is `mem_tag = <variant or path>`: M2 as amended takes a path, so `mem_tag = Infer` (what
  db-mem-infer-a applied) and `mem_tag = crate::db::INFER_TAG` both compile to the same constant.

> forwarded to db (db-next3 told by tmux; requests, pass 23).
> applied by db-mem-infer-a (21:58): requests 5 to 9 went in with these replacements (db-mem-infer-a-query; `collect` builds `reached` and `hot` from one closure, same bits). The `Map::reset` fix was missing (it still called `self.reclaim()`): now `reset` reclaims rows itself and calls the new `Outgrown::settle`, as written here. Request 4 (`tls_cache.rs`) applied by db-mem-infer-a-solver; mem's request 2 (`Pooled::into_raw`/`from_raw`) has landed in `pool.rs`.

## Per crate: what moves onto blaze-mem, and the test that holds it

The index of every change, by crate and by the four parts of the brief: (1) query runs on
frames, (2) tables on reserved regions, (3) the solver cache's generations, (4) per-call
scratch. Each line names where the exact change is. "Applied" means the code is in the tree now;
everything else waits on its owner.

- **`crates/blaze-query/`** (lane db, `db-next3`).
  1. Request 5 runs each query in `frame::task(info.tag, ..)` (`Query::TAG`, default `Query`).
     Request 7 puts `collect`, `Store::sweep` and `Map::edit_all_with` in frame tasks, their
     lists, sets and bitsets `Vec<_, &Frame>` and `hashbrown` sets in the frame. A run's value
     is not a frame value: `run_query` writes it into the thread's kept buffer inside the task,
     and the store copies it into a value segment (contract 4, request 8).
  2. Request 8: `Rows` and `Generations` are on `Region`s tagged `Query` (applied, db-query's
     request 3). Request 7 recycles the shards' outgrown tables by length. Left, by lane mem's
     ruling: the value, edge and key segments.
  4. Request 6 reuses closed views' states. Request 9 moves `add_memo`'s list onto
     `scratch::with_u32s` and `learn_key`'s bytes onto `scratch::lend_bytes`.
  - Tests: `a_warm_hit_allocates_nothing` (exists), `a_revalidation_that_changes_nothing_allocates_nothing`
    (6), `a_warm_collection_allocates_nothing`, `a_settled_map_rehashes_without_allocating` (7),
    `a_warm_miss_allocates_at_most_a_segment` (9), `a_run_is_a_frame_task_of_its_querys_tag` (5).
- **`vendor/salsa*`** (lane db; `speed-salsa-next3` has the file). `v2/requests/mem-db-salsa.md`.
  1. M1: `C::execute`, `cycle_initial` and `recover_from_cycle` run in
     `frame::task(C::MEM_TAG, ..)`, with one task per fixpoint iteration. M2 adds the
     `mem_tag` option. M3 keeps a run's edge fingerprints in a reused buffer and gives the memo
     an exact copy. Memos stay owned; their slots are on `Region` in `table/memo.rs` (applied).
  2. M5 moves the page list onto `Column<Page>` (`Query`) and the page slots onto region bump
     arenas (`Query`, and `Intern` for interned values). M6 takes the graph's edge and node pages
     from the kernel. What stays on the heap, and why, is M5's table.
  4. M7 replaces the claim-free verify's spilling `SmallVec` with a reused per-thread stack.
     Hits, claims, backdating and read recording already allocate nothing warm (speed-salsa S1 to S5).
  - Tests (M8): `tests/alloc_budget.rs` holds a memo hit, a deep verify that changes nothing
    and a warm re-check to 0. `tests/mem_frames.rs` checks tags, nested frames, fixpoint and
    both cycle functions.
- **`crates/hir-ty/`, not `next_solver/`** (lane db; appliers `db-mem-infer-a`, `db-mem-infer-b`).
  `v2/requests/mem-db-infer.md` is the index, with one file per part (`-ctx`, `-expr`,
  `-closure`, `-lower`, `-method`, `-eval`).
  1. `InferenceContext<'db, 'f>` and the lowering contexts hold the run's `&'f Frame`, and
     their worklists, tables' scratch and buffers are frame vectors and `FrameMap`/`FrameSet`.
     `InferenceResult`, `ClosureData` and the impl tables are built once at their exact size.
     Every tracked function gets `mem_tag` (`Infer`, and `Mir` for MIR queries).
  4. Per-call buffers on lookup paths go through `scratch` (each part file lists its own).
  - Tests: `tests/budget.rs` (a warm re-check of unchanged bodies is 0; a cold `infer` is held
    to its result's own allocations; the frame's peak grows linearly with body size).
- **`crates/hir-ty/src/next_solver/`, `vendor/rustc_type_ir`, `vendor/rustc_next_trait_solver`**
  (solver-sync, and lane db for `infer/`, `fulfill.rs` and the glue). `v2/requests/mem-db-solver.md`.
  1. This is the contract's one exception. A canonical goal makes a fresh `InferCtxt`, thousands
     per `infer`, so its tables, canonicalizer state, search-graph storage and per-goal vectors
     come from storage the worker keeps and recycles (requests 2, 3, 5 and 6), not from the
     frame, which can't roll back.
  2. Request 9 puts the interner's `mod table` columns and index on `Region`s tagged `Intern`.
  3. Request 4 above (`tls_cache.rs`) takes generations from a `Pool` tagged `Infer` and retires
     them through `pool::Limbo`. Its test `a_warm_replace_allocates_nothing` holds 3,968 warm
     replaces to 0 allocations, and `a_miss_into_an_empty_slot_allocates_nothing` holds a miss
     to 0.
  - Tests (request 8): a global-cache hit, a cached canonical response and a warm re-solve, each 0.
- **`crates/base-db/`, `crates/hir-expand/`, `crates/hir/`, `crates/hir-def/`** (lane db;
  applier `db-mem-def`; `mem-db-def-collect.md` is frontend-nameres's). `v2/requests/mem-db-def.md`,
  with `-expand`, `-hir`, `-items`, `-resolve` and `-collect`.
  1. base-db's setters and `set_view_root` run in `frame::task(Tag::Parse, ..)` (B1, B2).
     `run_all` takes a tag and frames each task (B5; applied at `db.rs:306`). The building
     `DefMap`'s scopes live in the collection's frame, copied exact at `settle` (R10, Part H).
     hir has no tracked functions, so `SymbolCollector` is framed by ide-db's queries.
  2. B4 moves base-db's `Column` onto `AppendVec<AtomicU64>`, `Log` onto `blaze_mem::Column`
     and `Table`'s levels onto `AppendVec<AtomicU32>`, all tagged `Query`. hir-def's interned
     values move with salsa's `Intern` arena (salsa M5).
  4. `run_on_pool` uses lent `u32` buffers (B5). `path_key` goes through `scratch::with_bytes`,
     and the content hashes through a per-thread `Scratch<ContentId>` (B1).
  - Tests: base-db's `budget_tests.rs` (`PER_CALL` 1, `PER_CHANGED_FILE` 2, deps memo hits 0,
    `run_all` 0); hir-def's `alloc_budgets.rs`, T5 (a cold collection allocates the same
    whatever the module's width) and T6.
- **`crates/hir-ty-borrowck/`, `crates/hir-ty-wfcheck/`** (applier `db-mem-borrowck`;
  `frontend-fast-borrowck` applies its dataflow files itself). `v2/requests/mem-db-checks.md`,
  with `-regions` and `-check`.
  1. Every checker query opens `frame::task(Tag::Mir, ..)` (wfcheck: `Infer`), with one nested
     task per body. The bitsets, relations, dataflow and CFG are frame values, and
     `DropConstraint` and `InferredOutlives` are exact-size. Partly applied: `mem.rs`,
     `bitset.rs`, `cfg.rs`, `dataflow.rs`, `rows.rs` and wfcheck's `lib.rs`/`outlives.rs` open
     tasks now.
  4. Table S lists every per-call buffer and the reused one replacing it.
  - Tests: `src/budget.rs` in each crate. A clean or erroring body allocates its owned result
    alone; asking again is 0, so is the check after an edit to another file, and so is a
    passing `gate::checked`.

Open with lane mem (`mem-next1`), each blocking only the code that calls it: request 2
(`Pooled::into_raw`/`from_raw`, for request 4), 10 (nested tasks past `DEPTH` without a
reservation each: until then, every query run nested more than 8 deep maps and unmaps a frame),
11 (`Column::get_mut`, for salsa M5) and 12 (`Shelf` public, for the solver's shelves).
Request 1 (`frame::current`) is applied.

> forwarded to each owner it names (db, solver-sync, frontend-nameres, frontend-fast-borrowck, mem); requests applied its ide-db line (mem-db-def-hir.md R11) (requests, pass 23).

## 14. lane mem (`crates/blaze-mem/src/frame.rs`) and lane db (`vendor/salsa/src/function/sync.rs`): a cap's cancel reruns a query's waiters, not panics them

- Why: since mem-frame's cap, a frame allocation can unwind its task with
  `blaze_mem::frame::Cancelled`, and the scheduler reposts the task (`frame.rs:855`,
  `blaze-graph/src/exec.rs`). With salsa M1 that unwind starts inside a query's run and passes
  through the claim of every query on the stack. Each `ClaimGuard` releases with
  `release_panicking` (`sync.rs:396`), which gives the waiters `WaitResult::Panicked`, and
  `Running::block_on` (`runtime.rs:197`) turns that into `Cancelled::PropagatedPanic.throw()`.
  So every thread waiting on any of those queries fails with a propagated panic, though nothing
  panicked: the task gave way and will run again. `WaitResult::Cancelled` does the right thing.
  `block_on` returns `false`, `fetch_cold` returns `None` (`fetch.rs:147-149`) and the waiter
  asks again, claiming and running the query itself. blaze-query needs nothing: blaze-graph's
  `pump::Flights` already gives a run's waiters a cancel when the run unwinds (`pump.rs:579`).
- The guard can't see the payload it unwinds with, so blaze-mem says so on the thread.

**a. lane mem, `frame.rs`.** `Stack` (line 1125) gains a field, and its initializer
(line 1144) gains `cancelling: Cell::new(false),`:

```rust
    /// A cap's cancel is unwinding this thread: set by [`cancel`], cleared when the thread's
    /// outermost task ends.
    cancelling: Cell<bool>,
```

Beside `in_task`:

```rust
/// Unwinds the calling task with [`Cancelled`], for its scheduler to run again: what the cap does
/// to the newest waiter when every holder waits. Until the thread's outermost task ends,
/// [`cancelling`] says so, for the guards the unwind drops on its way out (a query's claim,
/// released to its waiters as a cancel, not a panic).
#[track_caller]
pub fn cancel() -> ! {
    assert!(in_task(), "frame::cancel outside any frame::task");
    _ = STACK.try_with(|stack| stack.cancelling.set(true));
    // alloc: a cancel's payload, once per cancelled task
    std::panic::resume_unwind(Box::new(Cancelled))
}

/// Whether the calling thread is unwinding from [`cancel`].
pub fn cancelling() -> bool {
    STACK.try_with(|stack| stack.cancelling.get()).unwrap_or(false)
}
```

In `refused` (line 855), `cap::Refused::Cancelled => std::panic::resume_unwind(Box::new(Cancelled)),`
and the `// alloc:` line above it become `cap::Refused::Cancelled => cancel(),`. In `task`'s
`Back::drop`, inside `STACK.try_with` (line 1218), after `stack.current.set(self.outer);`:

```rust
                if self.outer.is_null() {
                    stack.cancelling.set(false);
                }
```

Test (`tests.rs`, `// Frame`): inside `task(Tag::Infer, ..)`, `catch_unwind(|| task(Tag::Mir, |_|
cancel()))` gives an `Err` whose payload is `Cancelled`. `cancelling()` is `true` after it, still
inside the outer task, and `false` once the outer task has returned. `cancel()` outside any task
panics (`#[should_panic]`).

**b. lane db (`speed-salsa-next3`), `vendor/salsa/src/function/sync.rs:404`, `release_panicking`.**
Before:

```rust
        let result = if self.zalsa_local.should_trigger_local_cancellation() {
```

After:

```rust
        let result = if self.zalsa_local.should_trigger_local_cancellation()
            || blaze_mem::frame::cancelling()
        {
```

salsa already depends on `blaze-mem`. Test: a new `vendor/salsa/tests/parallel/frame_cancel.rs`,
with `mod frame_cancel;` added to `tests/parallel/main.rs` in order:

```rust
// Shuttle doesn't like panics inside of its runtime.
#![cfg(not(feature = "shuttle"))]

//! A query whose run the frame cap cancels is run again by the thread that waited on it: the
//! waiter is not failed with a propagated panic.
use std::sync::atomic::{AtomicBool, Ordering};

use crate::setup::{Knobs, KnobsDatabase};

static CANCEL_FIRST_RUN: AtomicBool = AtomicBool::new(true);

#[salsa::tracked(returns(copy))]
fn query_a(db: &dyn KnobsDatabase) -> u32 {
    if CANCEL_FIRST_RUN.swap(false, Ordering::SeqCst) {
        db.signal(1);
        db.wait_for(2);
        blaze_mem::frame::cancel();
    }
    7
}

#[test]
fn a_waiter_reruns_a_query_whose_run_was_cancelled() {
    let db = Knobs::default();
    let db = &db;
    std::thread::scope(|scope| {
        let owner = scope.spawn(move || blaze_mem::frame::task(blaze_mem::Tag::Query, |_| query_a(db)));
        let waiter = scope.spawn(move || {
            db.wait_for(1);
            db.signal_on_will_block(2);
            query_a(db)
        });
        let cancelled = owner.join().expect_err("the owner's run was cancelled");
        assert!(cancelled.downcast_ref::<blaze_mem::frame::Cancelled>().is_some());
        assert_eq!(waiter.join().expect("the waiter ran it again"), 7);
    });
}
```

Without b, the waiter fails with `Cancelled::PropagatedPanic`.

> forwarded to mem (mem-next1, `blaze-mem/src/frame.rs`) and db (db-next4, `vendor/salsa/src/function/sync.rs`); told by tmux (requests, pass 32).
> applied by speed-salsa (window 17, 10-29 22:55), b only: `vendor/salsa/src/function/sync.rs` `release_panicking` gives `WaitResult::Cancelled` when `blaze_mem::frame::cancelling()`; `vendor/salsa/tests/parallel/frame_cancel.rs` as written, with `mod frame_cancel;` in `tests/parallel/main.rs`. Not built: needs a (`frame::cancel`, `frame::cancelling`, lane mem), not in `blaze-mem` yet.
> checked by db-audit-mem (23:11, for db-next4): lane db's half b is in the tree at `vendor/salsa/src/function/sync.rs:404`-`405` (`|| blaze_mem::frame::cancelling()`), `vendor/salsa/tests/parallel/frame_cancel.rs` and `tests/parallel/main.rs:23`. It doesn't build until lane mem's half a lands: `crates/blaze-mem/src/frame.rs` has no `pub fn cancel` or `pub fn cancelling` yet (mem-next1).
> applied by coord-1030 (10-30 09:20), a as written: `Stack::cancelling`, `frame::cancel`, `frame::cancelling`, `refused` through `cancel()`, the reset in `task`'s `Back::drop`, and the two tests in `blaze-mem/src/tests.rs`. Found by v2's first build, where `release_panicking` was refused for naming a function that did not exist.
