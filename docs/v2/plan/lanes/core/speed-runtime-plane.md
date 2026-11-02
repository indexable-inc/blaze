---
status: plan
summary: speed-runtime-plane's pass over blaze-graph's plane (single flight, fair share, critical path, memory admission) and its admission estimates: the state of the art for each, how the code compared, what changed and how it compares now.
date: 2026-10-29
---
# speed-runtime-plane: the plane and its estimates

What `speed-runtime-plane` changed in `crates/blaze-graph/src/plane.rs` and `admission.rs` for
[principles.md](../../goals/principles.md)'s "Fast code, from the first line", and why. The
plane's rules are [plane.md](plane.md)'s and did not change: every output, its order and every
state, count and stat are what they were (see "How it is proven the same").

## The plane's per-task work

**The state of the art.** Tokio's and Rayon's schedulers start a task for tens of nanoseconds:
a task is one allocation made once, queued on intrusive or ring-buffer queues, and nothing on
the way to running it allocates. Go's runtime does the same with its `g` structs reused through
free lists. Keyed single flight is Bazel's Skyframe (a node per key in one concurrent map,
its reverse deps grown in place) and buck2's DICE (keys hashed once, a map from key to a shared
future). None of them re-hashes a key that is already a hash, and none allocates per edge.

**How the plane compared before.** Per node: a `Vec<NodeId>` of dependents, the node's name and
deps cloned (a `String` and a `Vec<u32>` for its `Node` event beside the ones kept), a node of
a `BTreeSet` for its place among the ready, and `Spec` kept whole. Per task: a `Vec` of members
and a `HashMap` entry. Per step: a fresh `Vec<PlaneOutput>`, and `check` built a `HashSet` and a
`HashMap` of the batch and sorted it with Kahn's order even when given in order. Per start: a
`u128` division for the request's stride. Per pass over tasks with memory: a `HashSet` to count
each task once. The 32-byte key went through SipHash twice per lookup (the flight table and the
database's). Nodes, tasks and requests were in hash maps by id; a released request's room was
freed and allocated again by the next.

**What changed.**

- Nodes are `u32` slots. The hot row, `NodeRow` (64 bytes: priority, seq, cost, path, id,
  request, waiting, task, heap place, member link, first and last dependent edge), is apart
  from `NodeCold` (56 bytes: memory, times, deps range, blocker, group, name range, pool,
  flags), the state and the key. Tasks are `TaskRow`s (32 bytes) and their keys; requests a
  `Vec<Request>`. All three come back through free lists, so a long-lived plane holds only what
  its open requests hold, and a released request's buffers are kept for the next.
- A request's edges are one flat array of `Edge` (12 bytes: dep, owner, next): a node's deps
  are a range of it (CSR), its dependents a list threaded through it in the order added. Names
  are ranges of one `String` per request. A task's members are a list threaded through the
  node rows. Nothing is allocated per node, edge or task once the columns have grown.
- The ready nodes of a request in a pool are a 4-ary min-heap of 24-byte `Ready` entries (rank,
  seq, node); each node knows its place, so raising a priority or taking a node out is a sift,
  not a remove and insert. With a budget, the heap is walked best first (a small heap of the
  places seen) to find the first that fits, in exactly the order the `BTreeSet` gave.
- Requests with something ready are a `Vec` per pool with each request's place in it; with no
  budget the turn is a min over it, with one a sort of it, as before.
- The flight table indexes keys by their own first eight bytes (`Key`'s `Hash` writes one
  `u64`; the hasher passes it through), and is used through the entry API: one probe per ready
  keyed node. The stride is computed once a request.
- `check` marks the batch's ids in the plane's own index (rolled back if refused) instead of
  building maps of its own, and runs Kahn's order only when a node is given before one of its
  deps; `add` permutes the batch in place. The `Node` event takes the spec's name and deps
  (`Vec<NodeId>` to `Vec<u32>` in place) instead of cloning them.
- A pass stamps tasks with a counter instead of filling a `HashSet`.
- `Plane::step_into` and `Plane::cancel_into` append to the caller's buffer; `step` and
  `cancel` are them over a fresh `Vec`, so every caller still compiles.

**How it compares now.** In steady state (columns grown, a caller that keeps its buffer) a
task costs the plane no allocation but the event payloads the protocol owns: the `Node` event's
name and deps are the spec's own, moved; a `Failed` state's `String` is cloned once per member.
A lookup by key is one multiply-free probe; a lookup by id one Fx hash. The heap, edge and
member walks are over `u32`s in flat arrays. `Memo` stays a `Box<dyn Memo>`: the public
constructor takes one, and its call is per keyed node, beside a database lookup.

## Admission estimates

**The state of the art.** A per-kind table of a few dozen entries, read and written once a
task: a direct or Fx-hashed table, nothing allocated per observation.

**How it compared before.** A `BTreeMap<TaskKind, Estimate>`: a tree walk per `predict` and
`observe`, a node allocated per new kind.

**What changed.** `Estimates::by_kind` is an `FxHashMap` (`Estimate` is 40 bytes, asserted).
`encode` and serde write the kinds sorted, so the bytes are what they were and do not depend on
the order kinds were first seen; `decode` reserves the count it was given (bounded by the
bytes' length).

**How it compares now.** One Fx hash and probe per `predict` or `observe`, no allocation; the
sort is paid only when the table is written out.

## How it is proven the same

- `plane_tests/reference.rs` is the plane as it was, kept as a model. `plane_tests.rs` drives it
  and the new plane through the same seeded inputs (opens, adds in shuffled order, keyed
  batches, done, cancel, release, database writes and drops) and compares every output, and
  after every act the holding task, granted memory, counts, stats and every node's state: 300
  seeds kept and released, 400 seeds of batches given out of order or refused (twice, unknown
  dep, cycle), and 50 rounds of one long-lived plane against a fresh one per request with its
  slots bounded.
- `admission.rs` compares the table with a `BTreeMap` model over seeded observations (every
  prediction and the encoded bytes), and checks that kinds seen in another order give the same
  `encode` and serde bytes and come back from both.
- `const` size assertions hold `NodeRow`, `NodeCold`, `Edge`, `TaskRow`, `Ready` and `Estimate`.

## Unsafe

None, as [unsafe.md](../../design/unsafe.md) asks: the plane and its estimates are safe Rust,
and `plane.rs`, `admission.rs`, `plane_tests.rs` and `plane_tests/reference.rs` each say
`#![forbid(unsafe_code)]`. `blaze-graph` as a whole cannot, while `arena.rs`, `pool.rs`,
`topology.rs` and `pump.rs` hold its unsafe modules.

## Left to others

Filed: `machine.rs` and `exec.rs` keeping one buffer for `step_into` and `cancel_into`, and a
"Layout" item for [plane.md](plane.md) (`v2/requests/core.md`); the daemon's `serve/plane.rs`
the same (`v2/requests/cargo.md`). Until they land each step of those callers still allocates
its output `Vec`.
