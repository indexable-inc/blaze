---
status: plan
summary: mem-db-checks' plan to put the borrow checker's and item well-formedness's per-run temporaries on blaze-mem frames, keep only the owned memo on the global heap, and hold both to budget tests; the changes are requests, filed in v2/requests/mem-db-checks.md and its two children's files.
date: 2026-10-29
---
# mem-db-checks: borrowck and wfcheck onto frames

Part of [mem-db](db.md). The crates are `crates/hir-ty-borrowck/` and `crates/hir-ty-wfcheck/`.
Their owners (lane `db`, `db-ty-d-borrowck`) are not done, so every change is a request with
its file, its line, the code before and after, and why:

| part | request file |
|---|---|
| setup, containers, entry points, the analyses, wfcheck, per-call buffers, budget tests | `v2/requests/mem-db-checks.md` (B0-B5, W, S, T) |
| `regions.rs` (the constraint graph, SCCs, region errors) | `v2/requests/mem-db-checks-regions.md` |
| `check.rs`, `explain.rs` (the checks and their diagnostics) | `v2/requests/mem-db-checks-check.md` |

## The design

- **One task per query run, one per body.** `borrowck`, `borrowck_anon_const`,
  `borrowck_in_mode` and `drop_constraint` each open a `frame::task(Tag::Mir, ..)`. `wfcheck`
  and `inferred_outlives` each open a `frame::task(Tag::Infer, ..)`. They open their own task
  because salsa's tracked functions don't run in one yet, and `frame::current` outside a task
  stops. Under a salsa that does run each query in a task, this becomes a nested task: one
  frame off the thread's spares.
- **The owner's frame holds what crosses bodies**: the scan of the store, the closures'
  requirements, the diagnostics and moves found. Each body is then checked in a nested
  `frame::task(Tag::Mir, ..)`. That task's frame holds the body's cfg, dataflow, loans, moves,
  liveness, constraints, region values and polonius state, and it is given back when the body
  is done. So a run's peak is the owner's data plus the largest body's, not the sum of all
  bodies.
- **Containers.**
  - Bitsets and fixed rows are frame slices of `u64`: `BitSet<'f>` and `BitMatrix<'f>` are
    `&'f mut [u64]`.
  - Growable lists are `Vec<T, &'f Frame>`.
  - Maps and sets are `hashbrown` with `FxBuildHasher` in the frame. Every such map in these
    crates is only looked up and never iterated into output, so their order reaches nothing.
  - A CSR (compressed sparse row: one array of row starts, one of items) and the facts that
    build it keep their arrays in the frame, and `Csr::rebuild` reuses them.
- **Memos are owned and exact.**
  - `BorrowckResult`'s diagnostics, `WfResult`'s, `DropConstraint`'s three lists and
    `InferredOutlives` are made once at their length through `mem::owned` (exact
    `with_capacity`).
  - Their strings are made once, when the diagnostic is new. wfcheck compares a
    `fmt::Arguments` against the stored text without building it.
- **Per-call buffers are made once per run** (request section S): for example the per-loan
  scope walk, the per-point list of used locals, and each input's normalization buffer.
- **The hit path allocates nothing**: a memo read, `gate::checked` on a passing body, and
  `Inferred::of`.

## Budget tests (request T)

- `borrowck_in_mode` on a body whose inputs are already asked allocates exactly its result's
  blocks. That is zero for a clean body. For a body with errors, it is the diagnostics vector
  plus each string and list in it.
- A second `borrowck`, `wfcheck` or passing `gate::checked` allocates zero, including after an
  edit to another file.
- `check_item` (wfcheck's body) allocates exactly its diagnostics.

Some tests depend on other lanes:

- The "owned result alone" tests need `InferCtxt`'s tables recycled per worker (they don't come
  from the checks' frames, so nothing is threaded into them) and the solver allocating nothing
  once warm (mem-db-infer,
  mem-db-solver).
- They also need check.rs's diagnostics to make each owned `String` in one allocation, which
  is in the check child's file.
- The zero-on-hit tests need salsa's hit and revalidation paths to allocate nothing
  (mem-db-salsa).

## Speed-of-light bound

- **A memo hit's floor is a lookup**: hashing the key, probing salsa's table and checking the
  revision. It allocates and writes nothing. The tests hold it to zero allocations.
- **A run's floor is the bytes its analyses must write once.**
  - Each dataflow pass writes a bitset per block: blocks × locals / 8 bytes, read and written
    once per worklist visit.
  - Liveness writes two facts per point plus an interval set per region.
  - The loans and moves write their columns.
  - Region inference writes one interval set per SCC (strongly connected component).
- **Against that floor, the allocator's cost is**:
  - a frame bump per container, about 2 ns, against 15-25 ns for a general-purpose allocation
    and its free;
  - no frees at all, since a task's end gives the whole frame back at once;
  - no copies for the arrays made at their final size: bitsets, SCC arrays, filled rows and
    exact results.
- **What doubling still copies**: the growable lists that are not the frame's last allocation
  (the move tree's columns, the facts) are copied when they grow. That is at most twice their
  final bytes, the same as on the global heap.
- **What the bound leaves on the global heap**:
  - the result's own blocks;
  - hir-ty's `InferCtxt` tables until their per-worker recycling lands, and solver state until
    its lane does;
  - `outlives::components` for a type of five or more components (a `SmallVec` of four,
    listed as open);
  - wfcheck's per-input error lists, which hir-ty's `ElidedLifetimes::regions` takes as
    `&[Vec<Region>]` (asked of hir-ty).

## Open

- `components` onto a per-worker `Scratch<Component>`, filled by it and read by its callers
  before they call it again.
- `ElidedLifetimes::regions` taking slices, after which wfcheck's error lists go to the frame.
- Once salsa runs each query in a task of its tag, the checks' own tasks may become
  `frame::current`. The budget doesn't change either way.
