---
status: plan
summary: How hir-ty-codegen keeps a function's relocations as 16-byte rows, asks for each request with one hash and one probe, and runs promote, small copies and slot sharing over dense entity maps and bitsets instead of hash maps; each against Cranelift's MachBuffer, lld and mold, LLVM's mem2reg and StackColoring, with what is left to match them.
date: 2026-10-29
---
# The back end's data at the hardware's speed: notes of speed-backend-code

Notes of the `speed-backend-code` lane of the speed pass, as of 2026-10-29. Code:
`crates/hir-ty-codegen/{fragment,promote,overlap,copies,home,fncache}.rs`. This lane runs
nothing (no build, test or benchmark), so every "after" below is counted from the code, not
timed; each component names the differential test that holds it to the old behavior, and
the measurement that is still owed.

## Relocations: `fragment::Relocs`

**Before.** `Code` held `Vec<ModuleReloc>` (offset `u32`, kind 1 byte, a 12-byte
`ModuleRelocTarget`, addend `i64`: 32 bytes with padding) and a separate
`Vec<(u32, u32)>` of ELF types. Every consumer (`mod.rs` `define_code`, `v2/convert.rs`)
found a relocation's ELF type by a linear search of that vector, so a function with *n*
relocations, some of them typed, cost O(*n*²) there.

**After.** `Relocs` is one `Vec` of 16-byte rows, const-asserted:
`offset: u32, index: u32, addend: i32, kind: u8, name tag: u8, elf type: u16`. The target is
the `u32` place among the function's requests, an addend outside `i32` (or equal to
`i32::MIN`, the marker) goes in a side table, and so does the namespace of a name outside
functions and data (`INLINED`, `v2::fold::BODY`). No row owns a string. Building it from
Cranelift's `FinalizedMachReloc`s is one pass; the ELF type is read from the row, so the
quadratic search is gone. `Code::hash_with` and fncache's `hash_code` hash exactly the bytes
they hashed before (`relocations_give_back_what_they_were_given_and_hash_as_before`), so no
store key moves.

**Benchmarks.**
- Cranelift's `MachBuffer` keeps `MachReloc { offset: u32, kind: Reloc, target: RelocTarget,
  addend: i64 }`, 24 bytes or more with the target enum. Rows here are smaller, and the copy
  into `ModuleReloc` that `emit` made per relocation is gone.
- lld's ELF `Relocation` is `{ RelExpr expr; RelType type; uint64_t offset; int64_t addend;
  Symbol *sym; }`, 32 bytes. mold works on the input's `ElfRel` records in place (24 bytes for
  `Elf64_Rela`) and keeps per-section arrays with no per-relocation allocation. Rows here are
  smaller than either.

**Since (codegen-v2, 2026-10-29).** `blaze_codegen::Reloc`, the form v2 hands the linker,
is a 16-byte row too (`at`, `target` index, `i32` addend, kind), in a `blaze_codegen::Relocs`
table with each distinct target once (`crates/blaze-codegen/src/relocs.rs`). `v2/convert.rs`
resolves each distinct name of a function once (`Relocs<RelocName>` mapped to
`Relocs<Target>`), and templates are `Relocs<Ref>`, so making and filling one maps the targets
and copies the rows. That is the size mold's arrays get, and lld does not reach it. Owed: time
`emit` and `define_code` on nushell's largest functions (dev-aws, then ix) before and after.

## Requests: `Wants` and `Recorder`

**Before.** `FxHashMap<FuncRequest, FuncId>` and a map from shared blobs to ids. Every ask
built an owned `FuncRequest` (a `String` for a symbol or path, a cloned `Signature`) only to
look it up. On a miss the key was hashed again on insert, and on a hit the clone was dropped.
`moved_requests` cloned every blob to rename it before looking it up. `declare_inlined`
cloned an inlined body's requests for every use.

**After.** `funcs` and `shared` are `hashbrown::HashTable<Slot { hash, at }>` over the request
vectors, so each request is kept once and costs 16 bytes of table. An ask is hashed once with
Fx (`func_hash` over a borrowed `TargetRef` and the signature) and probed once with the entry
API. Nothing is cloned on a hit: `import`/`import_path` borrow the name, and a blob is looked
up through a `BlobView` that renames it on the fly. Vtables and statics use the same entry
pattern. An inlined body is cloned and renamed once per instance a function builds in, not once per
call. Every call of that callee is given the same body and its data: one location, text or
blob of its own per body, as rustc has one per function, where before each call made its own
copies. It is keyed by the instance alone, not the body's address, so the output does not
depend on the allocator. `BLAZE_LAYOUTS` is read once, not per `Recorder`.
Tests: `requests_are_numbered_as_a_search_of_those_before_numbers_them` (2000 asks against a
linear-search model), `a_renamed_blob_is_the_blob_it_is_made_into`.

**Benchmark.** rustc's own interners (`FxHashMap` with raw-entry lookup of borrowed keys),
which this now matches: one hash, one probe, no allocation on a hit.

**Still to match.** Requests are still per function. A per-unit interner shared by every
function a unit builds would turn the second function's asks into hits as well. Owed: count
allocations per function with dhat on ripgrep.

## `promote`, against LLVM's mem2reg

**Before.** `FxHashSet` of escaped slots, `FxHashMap<Value, StackSlot>`, and a map of maps
from slot to `(offset, type)` to a `Part` with two `Vec`s each. That is a hash and an
allocation per part, run twice per function.

**After.**
- Value to slot is a `SecondaryMap<Value, PackedOption<StackSlot>>`, and the escaped and
  addressed slots are `EntitySet`s, so a lookup is one indexed load.
- The accesses go into one `Vec<Access>`, stably sorted by (slot, offset), and parts are runs
  of it.
- All four buffers live in a thread-local and are cleared, not freed, between functions.
- Total cost is O(instructions + accesses·log accesses), with no allocation once the buffers
  are warm.
- The order parts are taken in changes nothing, since each instruction belongs to one part.
  `dense_promotion_is_the_hashed_one` holds it to the hash-map version on 500 generated
  functions of four blocks, with mixed types, offsets and escaping addresses.

**Benchmark.** LLVM's `PromoteMemToReg` visits each alloca's users once and places phis with
an iterated dominance frontier, about O(users + blocks) per alloca. This pass is the
single-store subset of it, with no phis, and it now costs the same per use.

**Still to match.** Slots stored more than once are left in memory, where mem2reg would place
phis. The dominator tree is rebuilt for each call instead of reusing Cranelift's
`DominatorTree::compute` into a kept tree. Owed: time `settle` on hir-ty's largest bodies.

## `overlap`, against LLVM's StackColoring

**Before.**
- The escape analysis (`FxHashMap<Value, StackSlot>`) and the used-value set were hash maps.
- The first-fit grouping tested a slot against every member of every group: O(slots² ·
  group size) bit lookups.
- It collected every block and every instruction into `Vec`s before rewriting.

**After.**
- `SecondaryMap` for values, `EntitySet` for used values and escaping slots.
- Each group keeps one `DenseBitSet`, the OR of its members' conflict rows, so whether a slot
  fits a group is one bit. Building the sets costs O(total conflicts).
- The moves are a dense `SecondaryMap`, and the rewrite walks the layout in place.
- `shared_slots_are_those_of_the_member_lists` holds both `share` and `share_escaping` to the
  old version on 500 generated frames with random symmetric conflicts.

**Benchmark.** LLVM's `StackColoring` computes per-block liveness bitvectors per slot and
merges slots greedily by size, testing interference on live intervals. Here the interference
comes from MIR (`frame_storage_conflicts`) as a `BitMatrix`, and the grouping test is a bit
test, on par with it.

**Still to match.** First fit is still linear in the number of groups. On a frame with
thousands of locals, a bitset of open groups per conflict class would make it O(1).

## Small copies (`copies.rs`)

`chunks` was a `Vec` per copy and `loaded` grew another one. Now `chunks` is an iterator and
`loaded` is a fixed array of the 19 values a copy of at most 128 bytes needs. The call's
result is read without `to_vec`. Test: `chunks_are_the_widest_first_and_fit_the_array`.
Benchmark: LLVM FastISel's inline `memcpy`, which allocates nothing either.

## The rest

- `home.rs`: `BLAZE_NO_HOME` was read from the environment for every instance a function
  calls. It is now read once (`LazyLock`).
- `fncache.rs`:
  - `hash_code` is moved onto `Relocs`, with the same bytes.
  - `is_code` is a salsa lookup plus a compare of an interned name, cheap enough to leave.
  - `crate_name` is edge-only.
- `float_pair.rs`, `vtable.rs`, `type_id.rs`, `places.rs` and `settled.rs` were already on Fx
  or direct indexing, with no lookup-then-insert on a hot path. `type_id.rs` keeps
  `StableSipHasher128`, because rustc's `TypeId` is defined by it.

## Unsafe ([unsafe.md](../../design/unsafe.md))

Every speed-up above is safe code, so none of them needs `unsafe`. The ten files
`fragment`, `promote`, `overlap`, `copies`, `home`, `settled`, `places`, `float_pair`,
`vtable` and `type_id` say `#![forbid(unsafe_code)]`.

The one `unsafe` in these paths is in `fncache/dispatch.rs`: two `ModuleIdLt::to_static`
calls. Each now carries a `// SAFETY:` comment saying the invariant, where it is upheld and
what breaks otherwise. Because of that child module, `fncache.rs` can't forbid `unsafe_code`.

## What other lanes have to land

The crate does not build until the callers of `Code::relocs` move. The exact edits are in:
- `v2/requests/speed-backend-lower.md` (`mod.rs`)
- `v2/requests/speed-backend-llvm.md` (`llvm.rs`, `llvm/emitted.rs`)
- `v2/requests/codegen-adapter.md` (`v2/convert.rs`, `v2/shape.rs`, `v2/backend.rs`, the
  convert tests, and `hashbrown` in hir-ty-codegen's `Cargo.toml`)
- `v2/requests/speed-backend.md` (`cached.rs`, v1)
