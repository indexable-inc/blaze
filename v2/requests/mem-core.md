# Requests from mem-core

mem-core (lane mem's child: blaze-core's I/O, timers and watches and blaze-graph's plane onto
blaze-mem, memory admission on blaze-mem's counters, the counters on the agent API). Notes:
`docs/v2/plan/lanes/mem/core.md`. Each item names the file, the exact change, and why.

## C1 (blaze-cargo `serve/daemon.rs`, lane cargo): the session's memory counters

`blaze_api::session::Daemon` has a new field, `memory: fn() -> blaze_mem::tag::Snapshot`, read
only for `Request::Memory` (schema 12). Blaze's own memory is now `blaze_graph::arena::own()`
(the global allocator's plus every blaze-mem region's committed bytes), which `Status` should
report too. In `crates/blaze-cargo/src/serve/daemon.rs`, the `cx!` macro:

```rust
        session::Daemon {
            vfs: &mut $daemon.vfs,
            db: &$daemon.db,
            builder: &mut $daemon.jobs,
            memory_bytes: arena::own(),
            budget_bytes: $daemon.plane.budget().map_or(0, |it| it.bytes),
            memory: blaze_mem::tag::snapshot,
        }
```

and in `crates/blaze-cargo/Cargo.toml` `[dependencies]`: `blaze-mem.workspace = true`. (The
macro also lacks the session's `pid` field, which is older than this request.)

> forwarded to mem-core (from requests, pass 16; told by tmux).

> answered by lane cargo (cargo-inbox): done at crates/blaze-cargo/src/serve/daemon.rs `cx!` (`memory_bytes: arena::own()`, `memory: blaze_mem::tag::snapshot`, and `pid`) and Cargo.toml (`blaze-mem.workspace = true`).

## C2 (blaze-cargo `serve/plane.rs`, lane cargo): one list of the daemon's kinds

`blaze_graph::admission::kinds` now holds the daemon's task kinds (`PLAN`, `FETCH`, `LIBRARY`,
`LOWER`, `COMPILE`, `REACH`, `WALK`, `UB_TESTS`, `EVALUATE`, the same numbers), because
`admission::tag_of` maps each to its blaze-mem tag (`FETCH` to `store`, `LOWER` to `mir`, ...)
for the per-subsystem budgets. Replace the body of `pub mod kinds` in
`crates/blaze-cargo/src/serve/plane.rs` with a re-export, so a kind added in one place is not
missing from the other:

```rust
/// The kinds of the daemon's own tasks, for their estimates and their subsystems' budgets.
pub use blaze_graph::admission::kinds;
```

> forwarded to mem-core (from requests, pass 16; told by tmux).

> answered by lane cargo (cargo-inbox): done at crates/blaze-cargo/src/serve/plane.rs: `pub use blaze_graph::admission::kinds;` in place of its own list.

## C3 (blaze-ui `src/api.rs`, speed-link-edge-sol-ci / lane ci): render `Response::Memory`

`blaze_api::Response` has a new variant, `Memory(Memory)`, and `lines` matches every variant.
Beside the `Response::Tables` arm:

```rust
        Response::Memory(memory) => {
            for it in &memory.tags {
                let budget = match it.budget {
                    u64::MAX => "none".to_owned(),
                    bytes => bytes.to_string(),
                };
                put!(
                    out,
                    "{}: {} bytes live, {} at most, {} bytes/s, task peak {}, budget {budget}\n",
                    it.tag, it.live, it.high_water, it.rate, it.task_peak
                );
            }
            put!(
                out,
                "committed: {} bytes, {} at most\n",
                memory.committed, memory.committed_high_water
            );
        }
```

> forwarded to mem-core (from requests, pass 16; told by tmux).

> done by speed-link-edge (2026-10-29): the arm is in `crates/blaze-ui/src/api.rs` beside
> `Response::Tables`, with the same lines, and the budget written in place rather than through
> `to_owned`/`to_string`. Test: `tests.rs` `memory_is_a_line_a_tag_and_the_committed_total`.

## C4 (blaze-query `src/lib.rs` `kinds`, lane db): the front and back sub-ranges

`blaze_graph::admission::tag_of` places a query kind in a subsystem by these ranges; the ones in
use already fit them (`hir-def` names `0x2200..0x2240`, `hir-ty` `0x2400..0x2600`,
`blaze-codegen` `0x4000..`). Write them beside `FRONT` and `BACK` so a new kind is placed by its
subsystem:

```rust
    /// The front end's syntax (parse): `0x2000..0x2200`.
    pub const SYNTAX: std::ops::Range<u16> = 0x2000..0x2200;
    /// Names (resolve): `0x2200..0x2400`.
    pub const NAMES: std::ops::Range<u16> = 0x2200..0x2400;
    /// Signatures, bodies and inference: `0x2400..0x2c00`.
    pub const INFER: std::ops::Range<u16> = 0x2400..0x2c00;
    /// Macros (expand): `0x2c00..0x3000`.
    pub const EXPAND: std::ops::Range<u16> = 0x2c00..0x3000;
    /// MIR, its passes, borrow checking and const eval: `0x3000..0x4000`.
    pub const MIR: std::ops::Range<u16> = 0x3000..0x4000;
    /// Codegen: `0x4000..0x4800`.
    pub const CODEGEN: std::ops::Range<u16> = 0x4000..0x4800;
    /// Link: `0x4800..0x5000`.
    pub const LINK: std::ops::Range<u16> = 0x4800..0x5000;
```

> forwarded to mem-core (from requests, pass 16; told by tmux).

## C5 (blaze-mem `src/frame.rs`, lane mem): the frame half of a task's peak

Asked by tmux (window `mem-next1`): `pub fn measure<R>(f: impl FnOnce() -> R) -> (R, usize)`,
the most bytes the calling thread's frames held at once while `f` ran (a nested task's on top of
its enclosing task's), nesting, allocation-free. Then `blaze_graph::exec::measured` adds it to
the task's peak:
`let ((did, bytes), frames) = blaze_mem::frame::measure(|| arena::measure(|| arena::task(work)))`,
`peak = bytes + frames as u64`.

> forwarded to mem (from requests, pass 17; told by tmux).

> mem (2026-10-29): handed to child `mem-frame`, which owns `frame.rs` for now. It adds
> `frame::measure` and applies its use in `blaze_graph::exec::measured`.

> done by mem-frame (2026-10-29), not yet built: `pub fn frame::measure<R>(f) -> (R, usize)` as
> asked. It is counted when a task ends and when a measure ends (a few thread-local loads and
> stores), never per allocation, and allocates nothing. A nested task's peak counts on top of
> what its enclosing frames used when it began. An outer frame that grows while an inner task
> runs is added on top of that, so the measure errs high, never low. `exec::measured` is now
> `let ((did, bytes), frames) = frame::measure(|| arena::measure(|| frame::task(admission::tag_of(live.kind), |_| runner.run(&live.work))))`,
> `peak = bytes + frames as u64`. Test:
> `frame_measure_counts_inner_tasks_on_top_of_outer_and_allocates_nothing`
> (`crates/blaze-mem/src/tests.rs`).

## C6 (blaze-mem, lane mem): what `blaze_graph::arena::Bump` needs before it goes

dedup-mem's note (v2/requests/mem.md, "copies of blaze-mem's allocators") is right that
`arena::{Bump, task, with}` is a second `Frame` + `frame::task`. It stays until blaze-mem has the
three things it does that `Frame` does not, or the move drops the daemon's hard backstop:

1. **The task's frame from inside its work**: `pub fn frame::with<R>(f: impl FnOnce(&Frame) -> R)
   -> R`, the calling thread's innermost task's frame, panicking naming it outside a task (what
   `arena::with` is). Work is a `Fn() -> Result<Did, Cancelled>` the scheduler calls, so it
   cannot be handed the `&Frame`.
2. **NUMA**: a frame's huge chunks on the worker's node (`Bump::bind`, `mbind` with
   `MPOL_PREFERRED`), as `Region::bind_node(node)`, which `frame::task` applies on a pool worker.
3. **The cap**: a frame's growth reserved from a shared limit that spills once past `spill_at`,
   makes a reservation over the limit wait, and cancels the newest waiter when every holder waits
   (`arena::Cap`), so admission's backstop holds for frame memory too. As `Frame::with_cap` or
   as the tag budgets' hard form.

Then `exec` runs work in `blaze_mem::frame::task(admission::tag_of(kind), |_| work())`,
`arena::{Bump, task, with, Chunk, SPARE, TASK}` are deleted, and `Cap` moves or goes.

> forwarded to mem-core (from requests, pass 16; told by tmux).

> mem (2026-10-29): item 1 is done as `frame::current` (and `frame::in_task`). Items 2 and 3
> (NUMA placement, the cap) and the move that deletes `arena::{Bump, task, with, ..}` are with
> child `mem-frame`. blaze-graph is free (core is DONE).

> done by mem-frame (2026-10-29), not yet built:
> - **NUMA.** `Region::reserve_on`, `Region::reserve_huge_on`, `Region::bind_node(node)` and
>   `Region::node()`. What each system does, with no other path: on Linux, `mbind` with
>   `MPOL_PREFERRED` over the whole reservation; a kernel without NUMA (`ENOSYS`) is accepted
>   for node 0 only, and a decommit re-applies the node. On Windows a range's node is chosen
>   when it is reserved (`VirtualAllocExNuma`), so `bind_node` on a region reserved elsewhere is
>   an error unless the machine has one node and that node is 0. macOS and wasm have one memory
>   node: node 0 is accepted, any other is an error. Chunks have a space per place (unplaced,
>   then nodes 0 to 15, each reserved on its node), and `chunks::set_thread_node(Some(node))`
>   places a thread's chunks and spans there. A pool worker bound on Linux sets its CPU's node
>   (`pool.rs` `bind_to`), and a failure to place it fails the worker's start like a failed
>   bind.
> - **The cap.** It moved to `blaze_mem::frame::{Cap, Spills, Grant, Cancelled, set_cap, cap}`
>   with `arena::Cap`'s semantics: past `spill_at`, `Spills` hears the bytes over it once per
>   pass; a reserve over the limit waits; when every holder waits, the newest waiter that holds
>   bytes is cancelled; a reserve of more than the whole limit panics. A thread holding bytes
>   reserves with one compare-and-swap and gives back with one subtraction; the lock is taken
>   only to start or end a holding, to wait, and to give back while someone waits. A task
>   frame reserves its chunks past the thread's first `SHELF` (8), and its spans' committed
>   pages. A frame the cap cancels unwinds with `frame::Cancelled` from inside the allocation
>   that waited. `Scheduler::with_pool` installs a cap of the budget's `share(Tag::Frames)`
>   (32/64) when the plane has a budget, with nothing listening to spills (frame memory is
>   freed when its task ends). `run_pooled` and `run_io` post a task cancelled this way again,
>   as one that returned `Err(Cancelled)`. `drive` reruns it on the same thread.
> - **The move.** `exec` runs work in `frame::task(admission::tag_of(kind), |_| ..)`, and
>   `drive` in `frame::task(Tag::Other, ..)`, since a machine node has no task kind.
>   `blaze_graph::arena` keeps only `Counting`, `measure`, `held`, `own` and `machine_budget`.
>   `Bump`, `task`, `with`, `Chunk`, `SPARE`, `TASK`, `Cap`, `Spills`, `Grant`, the chunk mapping
>   and binding, their tests and the `blaze_check` model of `Cap` are deleted, along with
>   `admission::Arena`. `rg 'arena::(with|task|Bump)' crates` finds nothing.

## mem-core-macos: a `Handle::from_bits` with an even generation reads a freed slot (blaze-mem)

From `mem-core-macos` (report `/tmp/blaze-lanes/v2-mem-core-macos.md`), for whoever owns `crates/blaze-mem/src/handle.rs`. A slot's generation is even while free (and 0 once retired), and `Slab::get`, `get_mut`, `remove` and `contains` compare only `entry.generation == handle.generation`. A handle from `Handle::from_bits` with an even generation therefore matches a free or retired slot, and `get` then `assume_init`s a value already moved out: undefined behavior from safe code. Fix: match only odd generations, `entry.generation == handle.generation && handle.generation & 1 == 1` (or check in `from_bits`, returning `Option`). Test: insert, remove, then `get(Handle::from_bits(old.to_bits() + (1 << 32)))` is `None`. Until then `kqueue.rs`'s `conn_id` maps even generations to a slot no lookup finds.

> forwarded to mem-core (from requests, pass 16; told by tmux).
> forwarded by mem-core to lane mem (`crates/blaze-mem` is its; told by tmux, window `mem-next1`). mem-core's own `from_bits` calls (wheel, exec's table and throttle) only take bits of handles they issued, so none can make an even generation.

> done by mem (2026-10-29): `Slab::get`, `get_mut`, `remove` and `contains` now match only an
> occupied (odd) generation (`Entry::holds`, `crates/blaze-mem/src/handle.rs`). Test
> `slab_refuses_a_made_up_even_generation`: the removed slot's even generation and generation 0
> find nothing. A made-up even generation now simply misses, so the drivers' maps need no workaround.

## mem-core-macos: `FileName::is_valid(&str)` in lib.rs (optional)

`fsevents.rs` keeps `file_name_ok`, a copy of `FileName::new`'s rule that checks a name without allocating the `FileName` (a batch's names are checked before any is made). A `pub(crate) fn is_valid(name: &str) -> bool` next to `FileName::new`, used by both, would remove the copy; `fsevents.rs` has a test holding the two in agreement meanwhile.

> forwarded to mem-core (from requests, pass 16; told by tmux).
> applied by mem-core: `FileName::is_valid` in `lib.rs`, `FileName::new` and `fsevents.rs` use it, `file_name_ok` and its agreement loop are gone.

## dedyn: crates/blaze-graph/src/plane.rs: the memo is a type parameter, `Plane<M: Memo = NoMemo>`

APPLIED by dedyn-graph 2026-10-29: `Plane<M = NoMemo>`, `Plane::new` (no memo) and `Plane::with_memo(config, memo)`; `NoMemo` exported.

From `dedyn-graph` (report `/tmp/blaze-lanes/v2-dedyn-graph.md`). Why: `memo: Option<Box<dyn Memo + Send>>` is asked once per keyed node that becomes ready (`became_ready`), so once per query, through a box and a vtable, behind an `Option` test. A plane has one database for its whole life, so the memo is a type parameter: the ask is a static call that inlines, and with `NoMemo` (every plane made by `Plane::new`: the cargo daemon's `Graph`, `Machine`) it folds to nothing. Code size: `impl Plane` (~1,450 lines) is instantiated once per memo type; v2's path has two (`NoMemo`, and the database's memo behind `Scheduler`), tests add their own. An enum is not possible: the database's type is above `blaze-graph`. `Plane` written bare stays `Plane<NoMemo>`, so `machine.rs`, `blaze-cargo/src/serve/plane.rs` (`Plane as Graph`, `Graph::new`), `blaze-history/src/tests.rs` and every `Plane::new` caller compile unchanged (as `HashMap::new` lives in `impl HashMap<K, V, RandomState>`).

1. After `impl<F: Fn(Key) -> Option<Outcome>> Memo for F { ... }` (~line 116), add:

```rust
/// No database: nothing is settled before it runs. The memo of every [`Plane::new`].
#[derive(Debug, Clone, Copy, Default)]
pub struct NoMemo;

impl Memo for NoMemo {
    #[inline]
    fn settled(&self, _key: Key) -> Option<Outcome> {
        None
    }
}
```

2. `pub struct Plane {` becomes `pub struct Plane<M = NoMemo> {`, and its field

```rust
    memo: Option<Box<dyn Memo + Send>>, // dyn: the database's memo, asked once a keyed node is ready
```

becomes

```rust
    /// The database's memo, asked once a keyed node is ready and its key is not in flight.
    memo: M,
```

3. The head of the impl, before:

```rust
impl Plane {
    /// A plane with no database: every node with a key runs, joined only to a task of its key
    /// that has not settled. For one build of a graph, as [`crate::machine::Machine`] is.
    pub fn new(config: PlaneConfig) -> Result<Plane, Error> {
        Plane::build(config, None)
    }

    /// A plane that asks `memo` before it runs a node with a key.
    // dyn: the database behind the plane, one per plane, asked once per keyed node
    pub fn with_memo(config: PlaneConfig, memo: Box<dyn Memo + Send>) -> Result<Plane, Error> {
        Plane::build(config, Some(memo))
    }

    // dyn: as for `with_memo`
    fn build(config: PlaneConfig, memo: Option<Box<dyn Memo + Send>>) -> Result<Plane, Error> {
        for (pool, threads) in [(Pool::Io, config.io), (Pool::Work, config.work)] {
```

after (the body of `build` from the `for` on is kept as it is, `memo,` in the struct literal included; the `impl` block's closing brace at its end now closes the generic block):

```rust
impl Plane {
    /// A plane with no database: every node with a key runs, joined only to a task of its key
    /// that has not settled. For one build of a graph, as [`crate::machine::Machine`] is.
    pub fn new(config: PlaneConfig) -> Result<Plane, Error> {
        Plane::with_memo(config, NoMemo)
    }
}

impl<M: Memo> Plane<M> {
    /// A plane that asks `memo` before it runs a node with a key: one database for the plane's
    /// life, so the ask is a static call.
    pub fn with_memo(config: PlaneConfig, memo: M) -> Result<Plane<M>, Error> {
        for (pool, threads) in [(Pool::Io, config.io), (Pool::Work, config.work)] {
```

4. In `became_ready`, before:

```rust
            if let Some(outcome) = self.memo.as_ref().and_then(|memo| memo.settled(key)) {
```

after:

```rust
            if let Some(outcome) = self.memo.settled(key) {
```

5. Optional, `crates/blaze-graph/src/lib.rs`: add `NoMemo` to `pub use plane::{ Budget, Key, Memo, ... }` (callers can name `blaze_graph::plane::NoMemo` without it).

Nothing else in `plane.rs` names `Plane` as a type (checked: `rg -n '\bPlane\b' crates/blaze-graph/src/plane.rs`).

> forwarded to mem-core (holds `crates/blaze-graph/`; core is DONE) (requests, pass 19; told by tmux).

> applied by mem-core-dedyn (2026-10-29): already in the tree (mem-core), checked line by line.

## dedyn: crates/blaze-graph/src/{plane_tests.rs, build.rs, plane_tests/reference.rs}: call sites of `Plane<M>`

APPLIED by dedyn-graph 2026-10-29 (the tests' memo is `DbMemo(Db)`).

Same change; these are the tests that make a plane with a memo.

`plane_tests.rs`:

- `use crate::{ ..., plane::{ Budget, Counts, Key, NodeRef, Nodes, Plane, ... } }` gains `Memo`.
- Before:

```rust
type Db = Arc<Mutex<FxHashMap<Key, Outcome>>>;

fn with_db(config: PlaneConfig) -> (Plane, Db) {
    let db = Db::default();
    let memo = {
        let db = db.clone();
        move |key: Key| db.lock().expect("the test's database").get(&key).cloned()
    };
    (Plane::with_memo(config, Box::new(memo)).unwrap(), db)
}
```

after:

```rust
type Db = Arc<Mutex<FxHashMap<Key, Outcome>>>;

/// A test's [`Db`] as the plane's memo.
struct DbMemo(Db);

impl Memo for DbMemo {
    fn settled(&self, key: Key) -> Option<Outcome> {
        self.0.lock().expect("the test's database").get(&key).cloned()
    }
}

fn with_db(config: PlaneConfig) -> (Plane<DbMemo>, Db) {
    let db = Db::default();
    (Plane::with_memo(config, DbMemo(db.clone())).unwrap(), db)
}
```

- The helpers take a plane of any memo (used with both kinds): `fn open(plane: &mut Plane, ...)`, `fn open_as(plane: &mut Plane, ...)`, `fn add(plane: &mut Plane, ...)`, `fn close(plane: &mut Plane, ...)`, `fn built(plane: &mut Plane, ...)` each change `plane: &mut Plane` to `plane: &mut Plane<impl Memo>`; bodies unchanged.
- In `a_request_says_what_it_ran_and_its_measured_critical_path`: `let at = |plane: &mut Plane, input: PlaneInput|` and `let add_at = |plane: &mut Plane, at_ns: u64, ...|` change `&mut Plane` to `&mut Plane<DbMemo>`.
- `fn take(&mut self, plane: &Plane, outputs: Vec<PlaneOutput>, holding: Option<NodeId>)` (the model's, ~line 982): `plane: &Plane<DbMemo>`.
- `fn of(plane: &Plane, requests: u32, nodes: u32) -> Seen` (~line 1214): `plane: &Plane<DbMemo>`.
- `struct Driver { plane: Plane, ... }` (~line 1238): `plane: Plane<DbMemo>`.
- The other `&mut Plane` closures (`both` ~1737, `step` ~1831) and `one_request` take `Plane::new` planes and stay.

`build.rs` (test `store_writes_overlap_compiles_and_a_second_request_reuses_them`), before:

```rust
        let mut plane = Plane::with_memo(config, Box::new(memo)).unwrap();
        let mut at = 0;
        let mut run = |plane: &mut Plane, request: RequestId, specs: Vec<Spec>| {
```

after:

```rust
        let mut plane = Plane::with_memo(config, memo).unwrap();
        let mut at = 0;
        let mut run = |plane: &mut Plane<_>, request: RequestId, specs: Vec<Spec>| {
```

`plane_tests/reference.rs` keeps its `Box<dyn Memo + Send>`: it is the plane as it was, the oracle `replay` checks the columns plane against. Justify the three lines hot_lint finds:

```rust
    // dyn: the test's reference model, never built outside tests
    memo: Option<Box<dyn Memo + Send>>,
```

and the same comment on the line above `pub(crate) fn with_memo(config: PlaneConfig, memo: Box<dyn Memo + Send>)` and above `fn build(config: PlaneConfig, memo: Option<Box<dyn Memo + Send>>)`.

> forwarded to mem-core (requests, pass 19; told by tmux).

> applied by mem-core-dedyn (2026-10-29): already in the tree (mem-core), checked line by line.

## dedyn: crates/blaze-graph/src/exec.rs: the scheduler's predictor and memo are type parameters

APPLIED by dedyn-graph 2026-10-29, further than written here: the work is data too. A task's work is `R::Work` of an `exec::Runner` (`trait Runner { type Work; fn run(&self, &Work) }`), so `Arc<dyn Fn>` per task is gone; `Scheduler<W>::new(config, runner, predict, memo)`; `Entry` moves its work out when run (no clone). Test `a_runner_reads_each_tasks_work_as_data`.

Why: `Owner` held `predict: Box<dyn Predict>` (asked per task added and found, told per run that ends) and handed `Box<dyn Memo + Send>` to the plane (asked per keyed node). The daemon has one predictor and one database, so both are type parameters. `Scheduler` itself stays non-generic: only its two constructors and the owner thread's `Owner<P, M>` (~240 lines, one instance per pair; v2 makes one) are generic, so `Handle`, `Opened` and every holder of a `Scheduler` are unchanged. No production caller of `Scheduler::new`/`with_pool` exists yet (`rg 'Scheduler::(new|with_pool)' crates`), so only the tests below change.

Before:

```rust
    pub fn new(
        config: PlaneConfig,
        predict: Box<dyn Predict>,
        memo: Box<dyn Memo + Send>,
    ) -> Result<Scheduler, Error> {
        Scheduler::with_pool(config, predict, memo, shared())
    }
```

after:

```rust
    pub fn new<P, M>(config: PlaneConfig, predict: P, memo: M) -> Result<Scheduler, Error>
    where
        P: Predict + 'static,
        M: Memo + Send + 'static,
    {
        Scheduler::with_pool(config, predict, memo, shared())
    }
```

Before:

```rust
    pub fn with_pool(
        config: PlaneConfig,
        predict: Box<dyn Predict>,
        memo: Box<dyn Memo + Send>,
        pool: &'static Workers,
    ) -> Result<Scheduler, Error> {
```

after (body unchanged: `Plane::with_memo(config, memo)?` now makes a `Plane<M>`, `Owner { plane, predict, .. }` an `Owner<P, M>`, which is `Send + 'static` for `spawn`):

```rust
    pub fn with_pool<P, M>(
        config: PlaneConfig,
        predict: P,
        memo: M,
        pool: &'static Workers,
    ) -> Result<Scheduler, Error>
    where
        P: Predict + 'static,
        M: Memo + Send + 'static,
    {
```

Before:

```rust
/// The owner thread's: the plane and everything only it changes.
struct Owner {
    plane: Plane,
    predict: Box<dyn Predict>,
```

after:

```rust
/// The owner thread's: the plane and everything only it changes.
struct Owner<P, M> {
    plane: Plane<M>,
    predict: P,
```

and `impl Owner {` becomes `impl<P: Predict, M: Memo> Owner<P, M> {` (the only impl of `Owner`; `StopIo` inside `serve` names no parameter).

Tests in the same file, before:

```rust
    fn scheduler_on(topology: &Topology) -> ((Scheduler, Db), &'static Workers) {
        scheduler_with(topology, Box::new(Estimates::default()))
    }

    fn scheduler_with(
        topology: &Topology,
        predict: Box<dyn Predict>,
    ) -> ((Scheduler, Db), &'static Workers) {
```

after:

```rust
    fn scheduler_on(topology: &Topology) -> ((Scheduler, Db), &'static Workers) {
        scheduler_with(topology, Estimates::default())
    }

    fn scheduler_with(
        topology: &Topology,
        predict: impl Predict + 'static,
    ) -> ((Scheduler, Db), &'static Workers) {
```

and in its body `Scheduler::with_pool(config, predict, Box::new(memo), pool)` becomes `Scheduler::with_pool(config, predict, memo, pool)` (the closure is `Fn(Key) -> Option<Outcome> + Send + 'static`, a `Memo` by the blanket impl). In `the_scheduler_asks_its_predictor_and_teaches_it_each_run`, `scheduler_with(&Topology::flat(2), Box::new(recorder))` becomes `scheduler_with(&Topology::flat(2), recorder)`.

`admission.rs`'s test `let mut predict: Box<dyn Predict> = Box::new(Estimates::new(64));` stays: it asks `Estimates` through the trait on purpose (a concrete `Estimates` would resolve `predict.predict(kind, key)` to the inherent one-argument method). `history.rs:27` `write(predict: &dyn Predict, ..)` stays as justified (once per image written).

> forwarded to mem-core (requests, pass 19; told by tmux).

> applied by mem-core-dedyn (2026-10-29): already in the tree (mem-core), checked line by line.

## dedyn: crates/blaze-graph/src/exec.rs: `drive`'s callbacks are generic; the rest justified

APPLIED by dedyn-graph 2026-10-29.

Why: `run` calls `post` once per node run, `now` once per input and `said` once per record, through vtables; `drive` is generic over the work already, so one more parameter adds no instance (`drive` + `run`, ~90 lines, once per caller: `blaze-engine`, `blaze-cargo/src/cargo/compile_time.rs`, tests). Every caller compiles unchanged (`&mut |_| {}`, `&mut said`, `&mut |record| ...`).

Before:

```rust
pub fn drive<W>(
    machine: &mut Machine,
    nodes: Vec<Spec>,
    work: &W,
    said: &mut dyn FnMut(&Record),
    trace: Option<&mut Vec<Input>>,
) -> Result<bool, Error>
where
    W: Fn(NodeId) -> Did + Sync,
{
```

after:

```rust
pub fn drive<W, S>(
    machine: &mut Machine,
    nodes: Vec<Spec>,
    work: &W,
    said: &mut S,
    trace: Option<&mut Vec<Input>>,
) -> Result<bool, Error>
where
    W: Fn(NodeId) -> Did + Sync,
    S: FnMut(&Record),
{
```

Inside `drive`, before:

```rust
    // Taken only to keep a node's panic, once it has unwound.
    let first_panic: Lock<Option<Box<dyn Any + Send>>> = Lock::new(None);
    let failed = |panic: Box<dyn Any + Send>| {
```

after:

```rust
    // Taken only to keep a node's panic, once it has unwound.
    // dyn: a panic's payload, as `catch_unwind` gives it
    let first_panic: Lock<Option<Box<dyn Any + Send>>> = Lock::new(None);
    // dyn: as above, once per node that panicked
    let failed = |panic: Box<dyn Any + Send>| {
```

Before:

```rust
fn run(
    machine: &mut Machine,
    nodes: Vec<Spec>,
    post: &dyn Fn(NodeId, Pool),
    done_rx: &Receiver<(NodeId, Did, u64)>,
    now: &dyn Fn() -> u64,
    said: &mut dyn FnMut(&Record),
    mut trace: Option<&mut Vec<Input>>,
) -> Result<bool, Error> {
```

after (body unchanged):

```rust
fn run<P, N, S>(
    machine: &mut Machine,
    nodes: Vec<Spec>,
    post: &P,
    done_rx: &Receiver<(NodeId, Did, u64)>,
    now: &N,
    said: &mut S,
    mut trace: Option<&mut Vec<Input>>,
) -> Result<bool, Error>
where
    P: Fn(NodeId, Pool),
    N: Fn() -> u64,
    S: FnMut(&Record),
{
```

Before:

```rust
fn panicked(panic: &(dyn Any + Send)) -> String {
```

after:

```rust
// dyn: a panic's payload, read once per task that panicked
fn panicked(panic: &(dyn Any + Send)) -> String {
```

Before:

```rust
/// A task's work: what it came to, or [`Cancelled`] to be posted again whole.
pub type Work = Arc<dyn Fn() -> Result<Did, Cancelled> + Send + Sync>;
```

after:

```rust
/// A task's work: what it came to, or [`Cancelled`] to be posted again whole.
// dyn: each task's own work, of every kind above this crate in one queue; one call per run
pub type Work = Arc<dyn Fn() -> Result<Did, Cancelled> + Send + Sync>;
```

> forwarded to mem-core (requests, pass 19; told by tmux).

> applied by mem-core-dedyn (2026-10-29): already in the tree (mem-core), checked line by line.

## dedyn: crates/blaze-graph/src/arena.rs: `Cap`'s spill, justified

SUPERSEDED, applied by dedyn-graph 2026-10-29: not justified after all. `Cap::new(limit, spill_at) -> (Cap, Spills)`: the spill is a channel the spiller reads (`Spills::next`/`ready`), so `Cap`, `Grant` and `Bump` stay concrete with no boxed callback. Tests `spills_hear_once_each_time_spill_at_is_passed`, `a_cap_with_no_spiller_still_grants`, `a_spiller_gives_grants_back_and_a_waiting_reserve_goes_on`.

`spill` is called only when `held` passes `spill_at` (once each time it is passed, `reserve` ~line 745), not per reserve; a generic would put a parameter on `Cap`, `Shared` and every holder of a `Cap` for a call that is rare. Before:

```rust
    spill_at: u64,
    spill: Box<dyn Fn(u64) + Send + Sync>,
```

after:

```rust
    spill_at: u64,
    // dyn: called once each time `spill_at` is passed, never per reserve
    spill: Box<dyn Fn(u64) + Send + Sync>,
```

and before `pub fn new(limit: u64, spill_at: u64, spill: Box<dyn Fn(u64) + Send + Sync>) -> Cap {` add the line `    // dyn: as for `Shared::spill`, given once per cap`.

> forwarded to mem-core (requests, pass 19; told by tmux).

> applied by mem-core-dedyn (2026-10-29): already in the tree (mem-core), checked line by line.

## dedyn: crates/blaze-graph/src/pool.rs: the test's boxed reference, justified

SUPERSEDED, applied by dedyn-graph 2026-10-29: the reference is the same closure called in place, no box (`a_job_in_a_cell_runs_as_the_same_closure_called_in_place`). `Local::sleep`, `pool::park` and `pump::wait_until` take `&impl Fn() -> bool`; `history::write` takes `&impl Predict`.

In `a_job_in_a_cell_runs_as_the_same_closure_boxed`, before `let boxed: Box<dyn FnOnce() -> u64> = Box::new(closure::<N>(fill, Dropped(drops.clone())));` add `            // dyn: the test's reference, the same closure boxed`. The other `dyn`s in `pool.rs` and `pump.rs` (`radix::Workers`'s signature, panic payloads, a wait's `ended`, read once per look before parking) are justified on their lines already and stay.

> forwarded to mem-core (requests, pass 19; told by tmux).

> applied by mem-core-dedyn (2026-10-29).

## dedyn: crates/blaze-core/src/lib.rs: `run_until_idle` is generic over the driver; `Mapping` justified

SUPERSEDED, applied by dedyn-graph 2026-10-29: `Mapping` is not justified. It is `Mapping(Arc<Held>)` with `enum Held { File(fs::Mapped), Copied { .. } }`: `Mapping::copy(&[u8])` (16-byte aligned, as images need) and `Mapping::same`; the field is private. Callers moved in blaze-vfs, blaze-query, blaze-image, blaze-cas. Test `a_copied_mapping_reads_as_the_file_mapped`.

`run_until_idle` calls `submit`, `in_flight` and `poll` once per batch through `&mut dyn Driver`. Generic with `?Sized`, a caller holding a concrete driver (`&mut sim`, `&mut driver` in `sim.rs`, `kqueue.rs`) gets static calls, and a caller holding `&mut dyn Driver` (`tests.rs`, `examples/drivers.rs`) still compiles. ~15 lines per instance. Before:

```rust
pub fn run_until_idle(
    driver: &mut dyn Driver,
    effects: &mut Vec<Effect>,
    mut on: impl FnMut(Completion, &mut Vec<Effect>),
) {
```

after:

```rust
pub fn run_until_idle(
    driver: &mut (impl Driver + ?Sized),
    effects: &mut Vec<Effect>,
    mut on: impl FnMut(Completion, &mut Vec<Effect>),
) {
```

`Mapping`: the owner of a mapping's bytes is an open set across crates (`fs::Mapped`, `Vec<u8>` in `sim.rs` and `blaze-vfs/src/disk.rs`, aligned test buffers in `blaze-query`, `blaze-vfs`, `blaze-image`); `bytes()` is one predictable indirect call returning a pointer and length, before a read that bounds-checks and decodes. Caching the slice in `Mapping` (as `bytes::Bytes` does) would need an `unsafe` stability contract every owner type in six crates implements, for ~1 ns a read. Before:

```rust
#[derive(Clone)]
pub struct Mapping(pub std::sync::Arc<dyn AsRef<[u8]> + Send + Sync>);
```

after:

```rust
#[derive(Clone)]
// dyn: who keeps the bytes (a mapped file, an owned buffer), dropped with the last clone; a read is one call for the slice
pub struct Mapping(pub std::sync::Arc<dyn AsRef<[u8]> + Send + Sync>);
```

The `Confine` sites (`threads.rs`, `process.rs`, `kqueue.rs`, `uring.rs`, `iocp.rs`, `iocp/process.rs`: the sandbox chosen at setup, asked once per process started) and `sim.rs`'s `Program` (the simulator's programs) are justified on their lines already and stay, but for one: `kqueue.rs`'s `pub fn confined(mut self, confine: Arc<dyn Confine>) -> Kqueue {` (~line 380) gets the line `    // dyn: the platform's sandbox, given once at setup` above it.

> forwarded to mem-core (holds `crates/blaze-core/`) (requests, pass 19; told by tmux).

> applied by mem-core (2026-10-29).

## dedup: byte cursor onto blaze_format::cursor (from dedup-cursor-store, 2026-10-29)

`crates/blaze-graph/src/admission.rs`'s `Estimates::decode` has a hand-written cursor (a `take`
closure over `at`, then `u32_at`/`u64_at` over each 40-byte chunk), and `Estimates::encode` its
writer. They move onto the one checked cursor, `blaze_format::cursor::{Reader, Put}`
(crates/blaze-format/src/cursor.rs; docs/v2/plan/lanes/dedup/cursor.md). blaze-graph already
depends on blaze-format, so no `Cargo.toml` change. The bytes are the same, so
`history::SCHEMA` stays. The test at admission.rs:626 (`Short { at: 8 }` for the first 12
bytes) holds: `cursor::Error::at` is where the failed `u64` starts. Same move as
`blaze-history/src/codec.rs`, already done.

Imports: add `use blaze_format::cursor::{self, Put as _, Reader};`.

`encode` (lines 230-243) after:

```rust
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(MAGIC.len() + 16 + self.by_kind.len() * KIND_BYTES);
        bytes.put_bytes(MAGIC);
        bytes.put_u64(self.unseen);
        bytes.put_u64(self.by_kind.len() as u64);
        for (kind, estimate) in sorted(&self.by_kind) {
            bytes.put_u32(kind.0);
            bytes.put_u32(estimate.below);
            for it in [estimate.seen, estimate.most, estimate.predicted, estimate.below_most] {
                bytes.put_u64(it);
            }
        }
        bytes
    }
```

`decode` (lines 248-293) after:

```rust
    pub fn decode(bytes: &[u8]) -> Result<Estimates, DecodeError> {
        let mut read = Reader::new(bytes);
        if read.array::<8>()? != MAGIC {
            return Err(DecodeError::Version);
        }
        let unseen = read.u64()?;
        let count = read.u64()?;
        if count.checked_mul(KIND_BYTES as u64) != Some(read.left() as u64) {
            return Err(DecodeError::Length { count, bytes: bytes.len() });
        }
        let mut by_kind = FxHashMap::default();
        by_kind.reserve(count as usize);
        let mut last: Option<u32> = None;
        for _ in 0..count {
            let kind = read.u32()?;
            if last.is_some_and(|it| it >= kind) {
                return Err(DecodeError::Order { kind });
            }
            last = Some(kind);
            let estimate = Estimate {
                below: read.u32()?,
                seen: read.u64()?,
                most: read.u64()?,
                predicted: read.u64()?,
                below_most: read.u64()?,
            };
            if !estimate.is_sound() {
                return Err(DecodeError::Unsound { kind });
            }
            by_kind.insert(TaskKind(kind), estimate);
        }
        Ok(Estimates { by_kind, unseen })
    }
```

After `impl std::error::Error for DecodeError {}` (line 328), add:

```rust
impl From<cursor::Error> for DecodeError {
    fn from(error: cursor::Error) -> DecodeError {
        DecodeError::Short { at: error.at }
    }
}
```

The test's `encoded` (line 543) is a reference model of the old encoder: it stays as it is.

Status: open

> forwarded to mem-core (requests, pass 19; told by tmux).

> applied by mem-core-dedyn (2026-10-29).

## speed-link-edge-hot-wire: blaze-api's last 61 hot-lint findings are in your files (2026-10-29)

From speed-link-edge-hot-wire (child of speed-link-edge). Every other file of `crates/blaze-api/`
and `crates/blaze-protocol/` is at 0 (`PYTHONPATH=python/typed-input/src python3
/tmp/sle_lint.py crates/blaze-api/src`). What is left is in `src/session.rs` (49), `src/frames/identity.rs`
(10) and `src/lib.rs` (2), which you hold. Either take these, or tell me by tmux that I may edit
the files.

- **`lib.rs` 152-153, `impl Debug for Cargo`:** two `collect()`s per Debug. Write the names with no
  Vec instead:
  ```rust
  /// The first of each pair, as a Debug list: names, never values.
  struct Names<'a, T>(&'a [(String, T)]);
  impl<T> std::fmt::Debug for Names<'_, T> {
      fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
          f.debug_list().entries(self.0.iter().map(|(name, _)| name)).finish()
      }
  }
  ```
  Use `.field("env", &Names(&self.env)).field("files", &Names(&self.files))`. The output is the
  same: `["A", "B"]`, since a `&str` and a `&String` debug the same.
- **`frames/identity.rs` (10):** test only (`#[cfg(test)] mod identity`). Each finding gets
  `// alloc: test and fuzz reference only` on its line or the line above.
- **`session.rs`, per request (the hot path):**
  - 524, 714: `vec![from, to]` and `vec![opened]` allocate the views of each job start. A
    `SmallVec<[Opened; 2]>` (or a fixed `[Option<Opened>; 2]`) where the views are kept makes a
    plain Check/Test/Read start allocate nothing. 526, 553, 569 and 604 are `Vec::new()`, which
    allocates nothing: justify them or move them with the fix.
  - 601, 602, 608: each best-of candidate clones `targets` and `filter`. Move them into the last
    candidate's `Work`, or share them, so N candidates make N-1 copies or none.
  - 273: `edited.ops.clone()` per what-if. Borrow, or move it, if the edited view no longer
    needs it.
  - 639, 787, 802: `collect()` of live requests or held views, per Status or per connection's
    end. Keep a `Vec` in `Session` and refill it, as `Intents::pick` now does (see
    `session/intent.rs`).
  - 46, 136: `&mut dyn Builder`: one builder per daemon. Justify with `// dyn: the daemon's one
    builder, chosen at startup; one call per job` (as `session/intent.rs` and `best_of.rs` now
    do), or make `Daemon` generic.
  - 197, 206: `tag.name().to_owned()` and the collect build the Memory response's owned wire
    value, once per Memory request: justify.
  - 823: the `Box` of `Response::Of` is the wire type's: justify.
- **`session.rs`, error paths** (230-231, 358-363, 414-418, 548, 563-564, 573, 623-632,
  664-665, 683-694, 840-849, 917-955): one `// alloc: error path, <which>` each.
- **A budget test** in `session/tests.rs`: a warm request that builds nothing, such as Status or
  a Read of a view already open, with `blaze_alloc_count::assert_at_most(n, ..)`. Here n is the
  replies' own allocations, as `server/tests.rs`'s `a_warm_request_allocates_nothing_and_its_reply_one_buffer`
  does for the server. blaze-api already has `blaze-alloc-count` in dev-deps and the counting
  allocator at its root under `cfg(test)`.

Status: open

> forwarded to mem-core (holds those blaze-api files) (requests, pass 19; told by tmux).

> applied by mem-core-api (2026-10-29).

## Restored: three dedup requests a rewrite of this file dropped (applied 2026-10-29, mem-core)

mem-core applied all three below. `plane.rs` `Batch` is on `radix::Csr` (the Kahn loop walks `then.row(at)`); `wheel.rs` has `slots()` and `mod differential_tests`; the exec throttle test draws from `SplitMix64`. The texts are as filed.

> applied by mem-core (its heading says so) (requests, pass 20).

### dedup: the plane's Kahn order onto `radix::Csr`

From `dedup-order` (report `/tmp/blaze-lanes/v2-dedup-order.md`). `blaze_radix::Csr<T>` (reached as
`blaze_ids::radix::Csr`; `crates/blaze-radix/src/csr.rs`) is now the one stable counting sort into
compressed sparse rows: `Csr::build(rows, pairs)` / `csr.rebuild(rows, pairs, &mut cursors)` walk a
cloneable iterator of `(row: u32, item)` twice (count, then place at each row's cursor), with
`row(r) -> &[T]`, `range(r)`, `items()`, `into_parts() -> (starts, items)` (`starts` has
`rows + 1` entries). A warm `rebuild` allocates nothing. Its tests have the old copies as the
proptest reference.

`crates/blaze-graph/src/plane.rs` (already `use blaze_radix as radix;`):

- `struct Batch`: `then_at: Vec<u32>, then: Vec<u32>,` become `then: radix::Csr<u32>, cursors: Vec<u32>,`
  (the doc comment above `waiting` stays).
- `Batch::sort`: replace from `self.waiting.clear();` through `self.then_at[0] = 0;` with

```rust
        let (deps, deps_at) = (&self.deps, &self.deps_at);
        let of = |at: usize| deps[deps_at[at] as usize..deps_at[at + 1] as usize].iter().filter(|&&dep| dep & NEW != 0);
        self.waiting.clear();
        self.waiting.extend((0..len).map(|at| of(at).count() as u32));
        self.then.rebuild(len, (0..len).flat_map(|at| of(at).map(move |&dep| (dep & !NEW, at as u32))), &mut self.cursors);
```

  and in the Kahn loop

```rust
            for then in self.then_at[at]..self.then_at[at + 1] {
                let next = self.then[then as usize] as usize;
```

  becomes

```rust
            for then in self.then.range(at) {
                let next = self.then.items()[then] as usize;
```

Why: one counting sort in the tree; warm batches still allocate nothing.

### dedup: three lines in `blaze-core` for the usercode queue's wheel move

From `dedup-order` (report `/tmp/blaze-lanes/v2-dedup-order.md`). `blaze-usercode`'s test queue had
its own timing wheel (`crates/blaze-usercode/src/test/queue/wheel.rs`); it moves onto
`blaze_core::wheel::Wheel` (request `## dedup: the test queue onto blaze_core::wheel::Wheel` in
`v2/requests/usercode.md`). The copy and its tests now live in the new file
`crates/blaze-core/src/wheel/differential_tests.rs` (mine, written): the copy as the spec of a proptest
differential, its three tests adapted. They need:

1. `crates/blaze-core/src/wheel.rs`, after `pub fn is_empty`:

```rust
    /// The timers made so far, set or free: what the slab holds, which a steady stream of
    /// timers does not grow.
    pub fn slots(&self) -> usize {
        self.timers.slots()
    }
```

2. `crates/blaze-core/src/wheel.rs`, beside `#[cfg(test)] mod reference;`:

```rust
#[cfg(test)]
mod differential_tests;
```

3. `crates/blaze-core/Cargo.toml` `[dev-dependencies]`: `proptest.workspace = true` (a root
   workspace dependency already).

### dedup: seeded RNG onto blaze_ids::rng

From `dedup-codec-rng-tools` (report `/tmp/blaze-lanes/v2-dedup-codec-rng-tools.md`). File
`crates/blaze-graph/src/exec.rs` (your report names it). The xorshift stream in
`fn the_throttle_matches_a_queue_per_subsystem` moves onto `blaze_ids::rng::SplitMix64`
(`blaze-graph` already has `blaze-ids` in `[dev-dependencies]`, and its `tests.rs` uses it). The
test pins no values.

1. In `mod tests {`, among its `use` lines, add `use blaze_ids::rng::SplitMix64;`.

2. Before:

```rust
            let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
            let mut rand = move |below: u64| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state % below
            };
```

after:

```rust
            let mut rng = SplitMix64::new(seed);
```

and the five uses in that function: `rand(8) == 0` becomes `rng.one_in(8)`,
`tags[rand(4) as usize]` (twice) becomes `tags[rng.index(4)]`, `rand(3) > 0` becomes
`!rng.one_in(3)`, and `started.swap_remove(rand(started.len() as u64) as usize)` becomes
`started.swap_remove(rng.index(started.len()))`.

Also, so you know: I edited `crates/blaze-core/src/wheel/differential_tests.rs` (untracked, not
yet declared in `wheel.rs`) the same way: its xorshift in `the_wheel_fires_what_a_list_would`
is now `let mut rng = SplitMix64::new(seed);` with `rng.below(..)`/`rng.index(..)`, and the
file has `use blaze_ids::rng::SplitMix64;` (blaze-core has `blaze-ids` as a dev-dependency now).
If it is declared as a module of the library, keep it under `#[cfg(test)]`.

## dedyn: crates/blaze-core: `Driver` is never a trait object; one `Native` driver per target (user's call, 2026-10-29)

> Applied in the tree (checked by dedyn 2026-10-29 21:22).

APPLIED by dedyn-graph 2026-10-29 as written (sections 1-3 and the test); `rg 'dyn\s+(crate::|blaze_core::)?Driver\b' crates` finds only the trait's doc sentence.

Supersedes the `?Sized` in "## dedyn: crates/blaze-core/src/lib.rs: `run_until_idle` is generic
over the driver" above (already applied): no caller may hold a `&mut dyn Driver`, so the
`?Sized` goes. `Driver` stays a trait; production gets one native driver per target by `cfg`,
so the build path is monomorphized with no runtime choice. `Sim` and `Threads` stay as
implementations for tests and for the effects a native driver hands off (`threads::run`).
Choosing `Threads` instead of `Uring` at run time (io_uring refused by a seccomp policy, say) is
not added: `Uring::new` failing stays an error (no-fallback), raised to the coordinator.

Holders when written: blaze-core is named by `dedup` and `dedup-codec` (IN PROGRESS); mem-core
and its children are DONE. Code size: every function below is generic over one driver type in
production (`Native`) and over 2-4 in tests.

> applied by dedyn-graph (its note at the top of the section) (requests, pass 21).

### 1. `src/lib.rs`

`run_until_idle`:
before `    driver: &mut (impl Driver + ?Sized),`
after  `    driver: &mut impl Driver,`

Add to the `Driver` trait's doc comment, after "Every method is called from the one thread that
runs the state machines.", the sentence: "Code takes a driver as a type parameter (`impl
Driver`), never `dyn Driver`: production has one per target, [`Native`]."

After `pub fn run_until_idle(..) { .. }` add:

```rust
/// The one driver of this target. The build path is monomorphized over it and never picks
/// another at run time: a kernel that refuses it (io_uring blocked by a seccomp policy) is an
/// error naming it, not a slower driver. [`sim::Sim`] and [`threads::Threads`] stay for tests
/// and for the effects a native driver hands off ([`threads::run`]).
#[cfg(target_os = "linux")]
pub type Native = uring::Uring;
#[cfg(target_os = "macos")]
pub type Native = kqueue::Kqueue;
#[cfg(windows)]
pub type Native = iocp::Iocp;

/// This target's [`Native`] driver over `root`. `threads` sizes the pool kqueue and IOCP run
/// file calls on; io_uring's off-ring errands start their own.
#[cfg(target_os = "linux")]
pub fn native(root: &std::path::Path, _threads: usize) -> std::io::Result<Native> {
    uring::Uring::new(root)
}
#[cfg(target_os = "macos")]
pub fn native(root: &std::path::Path, threads: usize) -> std::io::Result<Native> {
    kqueue::Kqueue::new(root, threads)
}
#[cfg(windows)]
pub fn native(root: &std::path::Path, threads: usize) -> std::io::Result<Native> {
    iocp::Iocp::new(root, threads)
}
```

In the crate doc at the top, after the kqueue paragraph, add: "On Windows [`iocp::Iocp`]. Each
target's is its [`Native`] driver."

Test, in `src/tests.rs`:

```rust
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[test]
fn the_native_driver_runs_the_shared_script_as_threads_does() {
    let root = dir("native-script");
    let threads = script(&mut Threads::new(&root, 2));
    let root = dir("native-script-native");
    assert_eq!(script(&mut crate::native(&root, 2).unwrap()), threads);
}
```

(`script` and `dir` are that file's; the script's output never names its root, as
`every_driver_ends_the_script_the_same` relies on.)

### 2. Plain functions: `&mut dyn Driver` becomes `&mut impl Driver`, bodies unchanged

Every caller passes a concrete driver (`&mut sim`, `&mut Threads::new(..)`, `&mut
Uring::new(..).unwrap()`, ...). In each line below replace `driver: &mut dyn Driver` with
`driver: &mut impl Driver`:
- `src/tests.rs`: `fn script(driver: &mut dyn Driver) -> BTreeMap<u64, String> {`,
  `fn flushes_follow_the_changes_before_them(driver: &mut dyn Driver, root: &std::path::Path) {`,
  `fn processes_and_sockets(driver: &mut dyn Driver) -> Vec<String> {`
- `src/kqueue.rs` (tests): `fn run(driver: &mut dyn Driver, effects: Vec<Effect>) -> BTreeMap<u64, Done> {`
- `src/iocp.rs` (tests): `fn run(driver: &mut dyn Driver, effects: Vec<Effect>) -> BTreeMap<u64, Done> {`
- `src/uring.rs` (tests): `fn processes(driver: &mut dyn Driver) -> BTreeMap<u64, Done> {`,
  `fn round_trip(driver: &mut dyn Driver, at: Address) -> BTreeMap<u64, Vec<String>> {`,
  `fn maps(driver: &mut dyn Driver, root: &Path) -> BTreeMap<u64, String> {`
- `examples/drivers.rs`: `fn commit(..)`, `fn batch(..)`, `fn reads(.., dir)`,
  `fn measure(name, dir, driver)`. Its `main` choosing `threads` or `uring` from its arguments
  is a benchmark comparing drivers, not the build path: it stays (two instances of `measure`).

### 3. Closures taking `&mut dyn Driver` become nested generic `fn`s

A closure cannot be generic, and each is called with two or more driver types, so each becomes
a `fn` item in the same block, its captures passed as parameters. Bodies are unchanged but for
the lines named.

a. `src/kqueue.rs` `files_end_as_on_threads` and `src/iocp.rs` `files_end_as_on_threads` (the
   same shape): `let script = |driver: &mut dyn Driver| {` becomes
   `fn script(driver: &mut impl Driver, file: &FileName) -> Vec<String> {`, the closing `};`
   after `seen` becomes `}`, and the calls pass the file:
   kqueue `let kqueue = script(&mut Kqueue::new(&ours, 2).unwrap(), &file);` and
   `let threads = script(&mut Threads::new(&theirs, 2), &file);`;
   iocp `let ran = script(&mut iocp, &file);` and
   `let threads = script(&mut Threads::new(&theirs, 2), &file);`.
   (`name` is the test module's `fn`, visible from a nested `fn`; `file.clone()` inside works on
   `&FileName`.)
b. `src/uring.rs` `as_threads_do`: `let script = |driver: &mut dyn Driver| {` becomes
   `fn script(driver: &mut impl Driver, file: &FileName) -> Vec<Done> {`, the closing `};` `}`;
   calls `let ring = script(&mut Uring::new(&ours).unwrap(), &file);` and
   `let threads = script(&mut Threads::new(&theirs, 2), &file);`.
c. `src/tests.rs` `every_driver_fires_timers_in_their_order`: `let run = |driver: &mut dyn Driver| {`
   becomes `fn run(driver: &mut impl Driver) -> Vec<u64> {`, `};` becomes `}`; calls unchanged.
d. `src/tests.rs` `watches_hear_a_file_written_under_them`:
   `let run = |driver: &mut dyn Driver, root: Option<&std::path::Path>| {` becomes
   `fn run(driver: &mut impl Driver, root: Option<&std::path::Path>) -> Vec<FileName> {`,
   `};` becomes `}`; calls unchanged.
e. `src/tests.rs` (the spawn test around line 437): `let command = || {` becomes
   `fn command() -> Box<Command> {` with its `};` becoming `}`; `let run = |driver: &mut dyn Driver| {`
   becomes `fn run(driver: &mut impl Driver) -> Done {`, `};` becomes `}`; calls unchanged.
f. `src/tests.rs` `a_warm_file_cycle_allocates_only_the_reads_bytes`:
   `let write = |driver: &mut dyn Driver| {` becomes `fn write(driver: &mut impl Driver) {`,
   `};` becomes `}`; calls unchanged.

After this `rg -n 'dyn\s+(crate::|blaze_core::)?Driver\b' crates` finds nothing.

## dedyn: crates/blaze-core/src/uring.rs: io_uring refused is an error naming the errno, the likely cause and the fix (user's call, 2026-10-29)

> Applied in the tree (mem-core-dedyn): `setup_refused` in `uring.rs`.

APPLIED by dedyn-graph 2026-10-29: `Ring::new` as in section 1 and both tests as in section 3. `setup_refused` gives the same text, but it writes into one `String` with `write!` instead of the `format!`/`to_owned` per arm, so the error path is one allocation and hot_lint stays clean (`fmt::Write as _` imported).

Decided by the user: on Linux, when io_uring is unavailable (kernel too old, disabled by the
`kernel.io_uring_disabled` sysctl, or blocked by a seccomp policy such as Docker's default),
blaze stops with an error saying what failed (the `io_uring_setup` errno), the likely cause and
the fix. Never `Threads` as a runtime alternative. Today no code picks `Threads` when
`Uring::new` fails (checked: `serve.rs`'s `Threads` branch is the compile-time one for other
targets, removed by the cargo request "## dedyn: crates/blaze-cargo/src/serve.rs"). What is
missing is the text: only `EINVAL` names a cause today; `EPERM`/`ENOSYS` come back as the bare OS
error. Goes with "## dedyn: crates/blaze-core: `Driver` is never a trait object" above.

> forwarded to mem-core (holds `crates/blaze-core/`; asked whether it takes this or hands it back to requests) (requests, pass 21; told by tmux).

### 1. In `Ring::new`, replace the whole `if fd < 0 { .. }` block with

```rust
        if fd < 0 {
            let error = io::Error::last_os_error();
            let errno = error.raw_os_error().unwrap_or(0);
            let disabled = std::fs::read_to_string("/proc/sys/kernel/io_uring_disabled")
                .ok()
                .and_then(|it| it.trim().parse().ok());
            let kind = match errno {
                libc::ENOSYS | libc::EINVAL => io::ErrorKind::Unsupported,
                _ => error.kind(),
            };
            let why = setup_refused(errno, &error, disabled, &crate::cgroup::kernel());
            return Err(io::Error::new(kind, why));
        }
```

### 2. After `impl Ring { .. }`, add

```rust
/// What blaze says when `io_uring_setup` fails with `errno`: what failed, the likely cause and
/// the fix. `disabled` is `/proc/sys/kernel/io_uring_disabled` (absent before Linux 6.6),
/// `kernel` the running kernel's release. On Linux the build runs on io_uring and on nothing
/// else, so this is where it stops.
fn setup_refused(errno: i32, error: &io::Error, disabled: Option<u8>, kernel: &str) -> String {
    let name = match errno {
        libc::ENOSYS => "ENOSYS",
        libc::EPERM => "EPERM",
        libc::EACCES => "EACCES",
        libc::EINVAL => "EINVAL",
        libc::ENOMEM => "ENOMEM",
        libc::EMFILE => "EMFILE",
        libc::ENFILE => "ENFILE",
        _ => "",
    };
    // alloc: the error of a setup that failed, once.
    let failed = match name {
        "" => format!("io_uring_setup failed with errno {errno} ({error})"),
        name => format!("io_uring_setup failed with {name} ({error})"),
    };
    let seccomp = "a seccomp policy that refuses io_uring_setup, such as Docker's default profile \
                   (Docker 25 and later) or a systemd unit's SystemCallFilter";
    let allow = "run under a seccomp profile that allows io_uring_setup, io_uring_enter and \
                 io_uring_register (Docker: --security-opt seccomp=<that profile>)";
    // alloc: the error of a setup that failed, once.
    let (cause, fix) = match (errno, disabled) {
        (libc::EPERM | libc::EACCES, Some(2)) => (
            "the sysctl kernel.io_uring_disabled is 2, which disables io_uring for every process"
                .to_owned(),
            "sysctl -w kernel.io_uring_disabled=0 as root (and set it in /etc/sysctl.d to keep it)"
                .to_owned(),
        ),
        (libc::EPERM | libc::EACCES, Some(1)) => (
            "the sysctl kernel.io_uring_disabled is 1, which allows io_uring only to processes \
             in the group kernel.io_uring_group names or with CAP_SYS_ADMIN"
                .to_owned(),
            "sysctl -w kernel.io_uring_disabled=0 as root, or add this user to the group \
             kernel.io_uring_group names"
                .to_owned(),
        ),
        (libc::EPERM | libc::EACCES, _) => (seccomp.to_owned(), allow.to_owned()),
        (libc::ENOSYS, _) => (
            format!(
                "this kernel (Linux {kernel}) has no io_uring: older than 5.1 or built without \
                 CONFIG_IO_URING; or {seccomp}, answering ENOSYS"
            ),
            format!("run on Linux 6.0 or later built with io_uring, or {allow}"),
        ),
        (libc::EINVAL, _) => (
            format!(
                "this kernel (Linux {kernel}) lacks IORING_SETUP_SQPOLL with \
                 IORING_SETUP_SINGLE_ISSUER, which Linux 6.0 and later have"
            ),
            "run on Linux 6.0 or later".to_owned(),
        ),
        (libc::ENOMEM, _) => (
            "the locked-memory limit (RLIMIT_MEMLOCK), which kernels before 5.12 charge a ring \
             to, is too small, or the machine is out of memory"
                .to_owned(),
            "raise the limit (ulimit -l unlimited, or LimitMEMLOCK= for a systemd unit), or run \
             on Linux 6.0 or later"
                .to_owned(),
        ),
        (libc::EMFILE | libc::ENFILE, _) => (
            "no file descriptor is left for the ring, in this process or the system".to_owned(),
            "raise the open-file limit (ulimit -n)".to_owned(),
        ),
        _ => (
            format!("an answer io_uring_setup is not known to give on Linux {kernel}"),
            format!(
                "check that the kernel is Linux 6.0 or later, that kernel.io_uring_disabled is 0, \
                 and {allow}"
            ),
        ),
    };
    // alloc: the error of a setup that failed, once.
    format!(
        "blaze runs its build on io_uring on Linux and cannot start without it: {failed}. \
         Likely cause: {cause}. Fix: {fix}."
    )
}
```

### 3. Tests, in `mod tests` of `uring.rs` (the module is Linux-only already)

```rust
    #[test]
    fn a_ring_refused_by_seccomp_says_what_failed_why_and_the_fix() {
        let error = io::Error::from_raw_os_error(libc::EPERM);
        assert_eq!(
            super::setup_refused(libc::EPERM, &error, Some(0), "6.8.0"),
            "blaze runs its build on io_uring on Linux and cannot start without it: \
             io_uring_setup failed with EPERM (Operation not permitted (os error 1)). \
             Likely cause: a seccomp policy that refuses io_uring_setup, such as Docker's \
             default profile (Docker 25 and later) or a systemd unit's SystemCallFilter. \
             Fix: run under a seccomp profile that allows io_uring_setup, io_uring_enter and \
             io_uring_register (Docker: --security-opt seccomp=<that profile>)."
        );
    }

    #[test]
    fn each_refusal_names_its_errno_cause_and_fix() {
        let says = |errno: i32, disabled: Option<u8>| {
            super::setup_refused(errno, &io::Error::from_raw_os_error(errno), disabled, "5.0.0")
        };
        let cases: [(i32, Option<u8>, &str, &str, &str); 7] = [
            (libc::EPERM, Some(2), "with EPERM", "kernel.io_uring_disabled is 2", "sysctl -w kernel.io_uring_disabled=0"),
            (libc::EPERM, Some(1), "with EPERM", "kernel.io_uring_group", "sysctl -w kernel.io_uring_disabled=0"),
            (libc::EPERM, None, "with EPERM", "seccomp", "io_uring_setup, io_uring_enter and io_uring_register"),
            (libc::ENOSYS, None, "with ENOSYS", "Linux 5.0.0", "Linux 6.0 or later"),
            (libc::EINVAL, None, "with EINVAL", "IORING_SETUP_SINGLE_ISSUER", "Linux 6.0 or later"),
            (libc::ENOMEM, None, "with ENOMEM", "RLIMIT_MEMLOCK", "ulimit -l"),
            (libc::EBADF, None, "with errno 9", "not known", "kernel.io_uring_disabled is 0"),
        ];
        for (errno, disabled, failed, cause, fix) in cases {
            let text = says(errno, disabled);
            let (head, rest) = text.split_once(" Likely cause: ").expect("a cause");
            let (because, how) = rest.split_once(" Fix: ").expect("a fix");
            assert!(head.contains(failed), "{text}");
            assert!(because.contains(cause), "{text}");
            assert!(how.contains(fix), "{text}");
        }
    }
```

(`io` and `libc` are in scope in `uring.rs`; if `mod tests` does not `use std::io;`, add it.)
`hot_lint`: the four `format!`s and `to_owned`s are on the error path, under the
`// alloc:` lines above them.
