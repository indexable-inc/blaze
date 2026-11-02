# Requests from lane core-plane

## 1. A node state for a cancelled request (protocol, lane ci)

- What: add `NodeState::Cancelled` (settled, like `Skipped`) to `crates/blaze-protocol/src/events.rs`,
  rendered by `blaze-ui`/`blaze-web` like a skip.
- Where: `blaze-protocol` (ci's); then `crates/blaze-graph/src/plane.rs` (`Plane::cancel` and
  `add`) moves to it, and `settled()` covers it.
- Why: `Plane::cancel` skips what of a request has not started, but `Skipped { because }` must
  name a node. Until the variant exists the plane says a `Said { level: Note, text: "request N
  is cancelled" }` record first and skips each node `because` the request's first node, which
  reads as if that node failed.

> applied: by ci (`NodeState::Cancelled` in `crates/blaze-protocol/src/events.rs`, drawn as a skip by `blaze-ui`, decoded by `blaze-web`).

## 2. docs/v2/plan/lanes/core/plane.md (core's) says the old fair-share and starvation rules

- What: in "The plane", rule 3: the next thread goes to the request that started the fewest
  tasks of the pool for its weight (stride scheduling; a request that had no ready work comes
  back at the plane's virtual time; ties to the heavier, then the first opened). Rule 5: a task
  passed over `starve_after` times holds back every task with memory of every pool and request
  (tasks with no memory still start) until it starts. Add: a task not started is ready in the
  request of each of its nodes, at that node's priority; `Plane::cancel`.
- Where: `docs/v2/plan/lanes/core/plane.md`.
- Why: the plane's module doc says this now; the old rule (running tasks over weight) gave a
  pool of one thread to the first request opened forever.

> applied: by core (`docs/v2/plan/lanes/core/plane.md`, rules 3, 5 and 6).

## 3. The plane keeps no memo: pass it the database's (core, core-pool)

- What: `plane.rs` now has `pub trait Memo { fn settled(&self, key: Key) -> Option<Outcome>; }`
  (any `Fn(Key) -> Option<Outcome>` is one) and `Plane::with_memo(config, Box<dyn Memo + Send>)`.
  `Plane::new(config)` is a plane with no database: a key done before runs again.
  `remembers`/`forget` are gone; the database drops a value instead.
  - core: `lib.rs` re-exports `Memo` and `RequestStats` (from `Plane::stats`, for proof)
    with the other `plane::` names.
  - core: `build.rs`'s test `store_writes_overlap_compiles_and_a_second_request_reuses_them`
    relies on the old memo and now fails at `!ran.contains(&a2.compile)`. Make it
    `Plane::with_memo` over a test map that `run` fills with `key → Outcome::Built` for each
    compile before its `Done`, as the store write does (`plane_tests.rs`'s `with_db` shows the
    pattern; the spec's key is in `specs`).
  - core-pool: `Scheduler::new`/`with_pool` take the db's memo (`Box<dyn Memo + Send>`, from
    `blaze-query`) and call `Plane::with_memo`. A task's work must put its value in the db
    before it posts `Done`, or a node of its key that becomes ready in between runs again.
- Where: `crates/blaze-graph/src/{lib.rs,build.rs}` (core), `exec.rs` (core-pool),
  `blaze-query` for the impl of `Memo` over the db's results by input hash.
- Why: docs/v2/plan/lanes/core.md, "One memo, the database's": two memos of one result can disagree, and
  the plane's grew without bound in a long-lived daemon.

> applied: core's and core-pool's parts by core (`lib.rs` re-exports `Memo` and `RequestStats`; `build.rs` and `exec.rs` use `Plane::with_memo`).
> forwarded to db: `blaze-query` has no `impl blaze_graph::Memo` over its results by input hash yet.

## 4. Release finished requests in the scheduler (core-pool)

- What: call `plane.release(request)` right after a successful `PlaneInput::Open` in
  `exec.rs`'s owner (or on `PlaneOutput::Finished`). A released request's room (nodes, specs,
  names, deps, its id) is given back once it finishes and reused by the next. After that,
  `state` of its nodes is `None` and a step naming it is `UnknownRequest`, not `Over`.
- Where: `crates/blaze-graph/src/exec.rs`. `Machine` should not release (its `state` is asked
  after the build ends).
- Why: without it a daemon plane keeps every node of every request it ever ran. Release
  changes no output: `plane_tests.rs` runs 300 seeds with and without it and compares every
  output.

> applied: by core (`exec.rs` calls `plane.release(request)`).

## 5. `PlaneOutput::Run` gained `critical: bool` (cargo-daemon)

- What: in `crates/blaze-cargo/src/serve/plane.rs:494`, the pattern
  `PlaneOutput::Run { id, pool, near }` no longer compiles; make it
  `PlaneOutput::Run { id, pool, near, .. }` (or use `critical` to place the task on a fast
  core, as `exec.rs` does).
- Where: `crates/blaze-cargo/src/serve/plane.rs`.
- Why: plan design/hardware.md, "P cores for the critical path": the plane now says whether a
  task is on its request's critical path. `exec.rs` already takes it; `machine.rs` and
  `build.rs` match with `..`.

> forwarded to cargo: the daemon's match is lane cargo's (cargo.md, "From core").

> answered by lane cargo (cargo-inbox): done at crates/blaze-cargo/src/serve/plane.rs:242 (`critical` goes to `Pool::submit_critical`).

## 6. A batch of nodes as one record (ci)

- What: an `Event::Nodes { first: u32, kinds: Vec<NodeKind>, names: String, name_at: Vec<u32>,
  deps: Vec<u32>, deps_at: Vec<u32> }` in `blaze-protocol`, meaning the nodes `first..` each
  as an `Event::Node` would say (name `i` is `names[name_at[i]..name_at[i + 1]]`, its deps
  likewise). Readers of `Event::Node` (`blaze-ui/src/lib.rs:578`, `blaze-graph/src/trace.rs:61`,
  `blaze-web/assets/records.js`) take it as that many nodes.
- Where: `crates/blaze-protocol/src/events.rs`, and its readers.
- Why: cargo.md section 16. `PlaneInput::AddNodes` takes a batch as columns, but the plane
  still says one `Event::Node` a node, each owning a `String` and a `Vec<u32>`: a 10k-unit
  build still makes 20k small allocations, now in the plane. With `Event::Nodes`, the plane
  says one record a batch, copied from the columns it was given.

> forwarded to requests-nodes: a child of `requests` (ci, core and core-plane are DONE), brief `~/restart-2026-10-28/briefs/v2/requests-nodes.md`; it requests the cargo and codegen readers' parts.
> applied (requests-nodes, 2026-10-29): `Event::Nodes { first, kinds, names, name_at, deps, deps_at }` in `blaze-protocol/src/events.rs`; `name_at` and `deps_at` hold `kinds.len() + 1` offsets (0 to the end, never back; `name_at` in UTF-8 bytes, on character boundaries). A dep is a node said before or another of the record, before or after it; a cycle is malformed. Read only through `NodeBatch::new` / `Event::nodes(&mut Order)`, which refuses a malformed record with `Malformed` (never a panic), and leaves `Order::places()`: the nodes each after their deps of the batch, the first given first, the order their `Event::Node`s would have come in (one pass, no allocation, when given in order). The plane says one `Event::Nodes` per non-empty `AddNodes`, first of that step's outputs, the columns moved into it (no copy), and no per-node `Event::Node` for them; `Add` and `Done { added }` are unchanged. Readers: `blaze-ui` (graph built in `Order::places()` order; malformed said as an error line, `Build::malformed`), its oracle `tests/old.rs`, `blaze-graph/src/trace.rs` render, `plane_tests` model, `blaze-web/assets/records.js` (checks the same, exposes `event.nodes`). Tests: protocol (pinned CBOR bytes, expansion, every refusal, 4000 random columns against a plain order search), plane (reference says the record too, simulation sends `AddNodes` half the time for runs of ids, columns vs specs compares with node records read apart, one batch through `blaze_protocol::cbor` and refused malformed), blaze-ui (one record draws the same graph and tree as a record a node, in or out of order; random records include batches against the oracle), `test.mjs`. `Event` is not in `blaze-api`'s schema: no `SCHEMA` bump. Engine's crate graph and cargo's parts requested in `v2/requests/codegen.md` and `v2/requests/cargo.md` ("From requests-nodes").

## 7. `pump`'s `DIR` leaks a chunk per model-checker run (core, core-pool)

- What: `crates/blaze-graph/src/pump.rs:124` `static DIR: [AtomicPtr<Stack>; CHUNKS]`. Under
  `blaze_check`, statics restart null in every execution, so `claim` leaks a fresh 24.6 KB
  chunk each time. safety-model-graph suggests `DIR: [OnceLock<Box<[Stack]>>; CHUNKS]` (from
  `crate::sync`), which the checker resets with the execution and a normal build reads as
  cheaply as today.
- Where: `pump.rs` (core-pool's; its report is DONE).
- Why: from safety-model-graph (`/tmp/blaze-lanes/v2-safety-model-graph.md`, finding 1). A
  normal build is unaffected: there each chunk is leaked once, for the life of the process,
  by design.

> applied (core is DONE): `DIR` is `[OnceLock<Box<[Stack]>>; CHUNKS]` from `crate::sync`; `claim` fills a chunk with `get_or_init` under `FREE`, `stack` is `DIR.get(i / CHUNK)?.get()?.get(i % CHUNK)`, so its two `unsafe` blocks and the `AtomicPtr`/`ptr` imports go. A normal build still makes each chunk once per process.
> applied: already in `pump.rs`: `DIR` is `[OnceLock<Box<[Stack]>>; CHUNKS]` (with `claim` calling `get_or_init`).
