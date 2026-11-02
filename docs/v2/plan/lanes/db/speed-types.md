---
status: plan
summary: speed pass over hir-ty's MIR (speed-types): the locals an async body keeps across yields computed with reused bitsets and a conflict matrix over held locals only, MirBody hashing without a hasher per entry, and vtables read without probing, against rustc's coroutine layout and StableHasher.
date: 2026-10-29
---
# speed-types: MIR held locals, hashing and vtables

Files: `crates/hir-ty/src/mir.rs` and `crates/hir-ty/src/mir/held.rs`. Nothing here was run:
costs are counted from the code, not measured.

## Locals held across yields (`mir/held.rs`)

**Benchmark.** rustc's `locals_live_across_suspend_points` and `compute_storage_conflicts`. These
are dataflow over `DenseBitSet`s, reused through `clone_from`, and a conflict matrix over saved
locals only.

- Before:
  - Every block's entry state was cloned out (`entry_states`).
  - Each statement built a `Vec` of whole moves (`whole_moves`).
  - The liveness and forward passes allocated a new set per block per round.
  - The storage-conflict matrix was locals × locals bits.
- After:
  - `Forward::load` reads an entry state into scratch sets with `clone_from`, or `clear`s them.
  - `kill_whole_move` kills a moved local in place.
  - Liveness swaps a changed set into `live_in` instead of cloning it.
  - The early-storage callback is `impl FnMut`, not `&mut dyn`.
  - `Conflicts` keeps the matrix over positions in `saved` only: \(n^2\) bits for \(n\) held
    locals, not \(\text{locals}^2\). Each yield's state is applied once.
- Test: `conflicts_over_held_positions_are_the_full_matrix` compares the new matrix with the old
  algorithm (`model`) over 200 seeded random bodies.

## `MirBody` hashing (`mir.rs`)

**Benchmark.** rustc's `StableHasher` hashes a map by sorting its entries by a stable key and
hashing them in order.

- Before: `hash_unordered` built a `StableHasher` per entry of `upvar_locals` and combined the
  entries' hashes.
- After: `hash_by_key` sorts the entries by key into a `SmallVec` of 8 on the stack and hashes
  the length, then each key and value, into the one hasher.
- Test: `a_small_map_hashes_by_key_on_the_stack_too`. The existing order-independence tests are
  kept.

## Vtables (`mir.rs`)

- `VTableMap::id` does one hash with the `Entry` API, where it did two.
- `VTableMap::FIRST_ID` and `tys()` give the types in id order. Callers that probed ids upward
  from 0 until a miss (`form.rs`, `facts.rs`, hir-ty's `vtable_types`) can read them directly.
  This was requested in `v2/requests/items.md` (items-facts has done it) and
  `v2/requests/db-ty.md`.

## Left

- data.md's 16-byte MIR statement, columns of statements per body, would change hir-ty-mir,
  codegen, borrowck and `form.rs` together: a cross-lane change, not one file's.
