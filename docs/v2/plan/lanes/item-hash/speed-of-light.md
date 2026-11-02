---
status: plan
summary: The bounds of lane item-hash (body identities and the item store), their arithmetic, and how the code reaches them.
date: 2026-10-30
---
# item-hash: speed of light

For [design/item-hash.md](../../design/item-hash.md). Machine: one core of the warm VM, ~4 GHz,
BLAKE3 at ~6 GB/s a core with AVX-512 (~3 GB/s with NEON on the Mac), DRAM ~20 GB/s a core,
mapped store pages in the page cache.

## 1. The identity of one body, cold

Work: BLAKE3 the portable body bytes, then hash the table entries and names.

- A typical body is 200-400 HIR nodes; the codec writes ~8 bytes a node plus names: **2-4 KB**.
  At 6 GB/s: **0.35-0.7 µs**.
- Writing the bytes: the codec copies columns (`memcpy` speed, ~20 GB/s) and walks side tables:
  **~1 µs**.
- Entries and names: k ≈ 20-60 (items, locals, segments). Each is a memoized salsa lookup
  (interface hash, item key; ~20-40 ns with the lock-free memo table) plus 40-72 bytes of
  preimage: **~1.5-3 µs**.
- Resolution of first segments: each a def-map lookup the resolver does anyway (~50 ns): k ≈ 20:
  **~1 µs**.

Bound: **~4-6 µs** a body, against an inference of 50-500 µs. Reached by: one `Hasher` stream
for the body bytes (no intermediate `Vec` past the codec's frame buffer), entries hashed as one
batch with `ContentId::of_many` (`blaze_items::hash::Batch`'s rule), names written into the
frame's scratch, never a `String`.

## 2. The identity, warm

After an edit, salsa re-verifies `identity(def)`: its inputs are the body (unchanged for every
body but the edited one), the interface hashes it read (early-cut at the edited item), the
resolutions (dbsp's slices) and `crate_inputs` (durability high). Bound: one memo check per
input, ~20 ns each, k ≈ 30: **~0.6 µs** a body, zero allocation. With lane graph's input-closure
Bloom filter, a body whose filter misses the edit is green in **~5 ns**.

## 3. A hit

- One shape read, one answer read: mapped pages, a hash-table probe in the store's index each
  (~100-300 ns), the bytes checked against their key once per mapping (BLAKE3 of a 1-10 KB
  answer: 0.2-2 µs; skipped on later reads of the same mapping, which the store tracks).
- Fact values: f ≈ 5-30 memoized lookups: **0.1-0.6 µs**.
- Decode: the inference record is read in place where its columns allow; what must be rebuilt
  (interned types through the type table) is ~1 intern per distinct type, f ≈ 50-200 types at
  ~30 ns: **2-6 µs**.

Bound: **~3-10 µs** a body against 50-500 µs fresh: the store wins 10-100x per body, and the decode
(type interning) is the floor. Allocation: only the decoded `InferenceResult`'s own storage
(budget test in `body_store/tests.rs`).

## 4. An offer

Classifying r ≈ 50-300 reads: one perfect-hash probe on the ingredient name (~5 ns) and, for a
fact, its subject's key (~20 ns): **~2-6 µs**, off the critical path (buffered per thread, written
by `flush` on the store's threads). The answer's bytes are written once (the encoder's speed,
~1 GB/s: 1-10 µs).

## 5. A whole cold build's overhead

nushell-sized: ~60k bodies. Identities: 60k × 5 µs = 0.3 s of CPU over 16-64 cores: ~5-20 ms of
wall time. Offers: 60k × 10 µs = 0.6 s CPU, off the critical path. Under 1% of the cold front end
(~30-60 s CPU), within the 10% budget.

## 6. Built MIR (`mir/persist.rs`, `body_store/mir.rs`)

A typical built body: ~30 blocks, ~200 statements, ~60 locals, ~50 distinct types, ~200 places.
The codec writes ~5 words a statement: **~1000 `u32` words (4 KB)** plus the type table.

- Encode: one pass pushing words into a column sized up front (`words_hint`), ~1 ns a word:
  **~1 µs**; each type, constant and argument list is one probe of the type table's map
  (~20 ns, ~250 references): **~5 µs**. Off the critical path, as an offer.
- Decode: one pass over the words, each a bounds check and a `match` on its tag (~1-2 ns):
  **~2 µs**. Every list is allocated once at its checked length (~30 block lists and ~20 side
  lists at ~20 ns: ~1 µs). The floor is interning: ~50 types through the table (~30 ns:
  1.5 µs) and ~200 places (`Projection::new_from_slice` plus `store`, ~30 ns: ~6 µs).

Bound: **~10 µs** a hit beside the two store reads of section 3, against 100-1000 µs to build the
body (inference read from its own record, then lowering): the decode is the interning it must do
and one branch a word. A closure, coroutine or anonymous constant of the body is a record of its
own, so a hit decodes only the owner asked for. With no store installed, the hook is one load
and a branch. Allocation: the decoded body's own storage, the type table's places and one slot
per allocation (budget tests in `mir/persist/tests.rs` and `body_store/mir/tests.rs`).

## Fastest known

- Unison and rustc's `hir_owner` hash both hash a definition's AST once; ours hashes the
  lowered body's columns with SIMD BLAKE3 in one stream, at memory speed.
- Bazel's action cache and Shake's cloud traces read one record per key; ours reads two (shape,
  answer), the second keyed by fact values, so a trace that changed misses in one probe.
- Finer than rustc's red-green graph: rustc re-verifies every dependency edge of `typeck(def)`;
  a hit here checks the identity and f facts, independent of the size of what inference read.
