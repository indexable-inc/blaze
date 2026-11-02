---
status: plan
summary: dedup-sim-front (2026-10-29): near-copies in the front end, MIR and codegen crates, which one each moves onto and which stay apart and why.
date: 2026-10-29
---
# dedup-sim-front: near-copies in the front end, MIR and codegen

Part of the dedup lane. The groups are `/tmp/similar-front.json` (141). Every move is a request
under "## dedup: ..." in the owner's `v2/requests/<owner>.md`; the one file written directly
is `crates/blaze-format/src/placed.rs`.

## Picked, and why

- **MIR queries**: one `MirOwner` x `MirStage` driver in hir-ty (`providers.rs` `mir_at`,
  `compute`), dispatching to the 24 existing memos. Every cache key and memo stays as it was:
  the named queries remain the typed entries, and the driver only picks one. Bound: a hit
  through the driver is the same memo (pointer-equal) and allocates nothing
  (`hir-ty-mir/tests/mir/stages.rs`).
- **Span to source**: `hir-ty-mir` `location.rs` `span_source`. Both `source_range`s go
  through it and keep their own tree read (codegen's is edit-stable).
- **Type spellings**: three spellings that differ on purpose (stable across runs, rustc's
  `type_name`, the resolver's `name[hash]` key). Only the scalar leaves are shared,
  `type_name::scalar_name`.
- **Operand types in MIR passes**: `transform::Ctx::operand_ty` (raw), with each pass keeping
  its own normalizing. Eval, borrowck, codegen and diagnostics keep their own, since they type
  places from different tables.
- **Erasing regions**: rustc's `erase_and_anonymize_regions` in `monomorphization.rs`, used by
  vtables and `TypeId`. Tested against both old folders.
- **Frozen bytes**: one Check/Verify comparison, `hir_def::nameres::frozen::check_frozen`.
- **Columns**: `blaze_format::Placed`, replacing hir-ty's three copies. The written bytes are
  unchanged (proptest against the old copy).
- **Binary operator codes**: `syntax::ast::BinaryOp::{code, from_code}`, beside the enum.
  hir-def's column word is `code + 1` (0 is no operator), hir-ty's code is the code; tested
  against copies of both old encoders, so no persisted word changes.
- **Smaller ones**:
  - `arch_tag`/`arch_of` made public for premap;
  - `variant_path` for discriminant errors;
  - intern's `order_key` re-exported from blaze-ids;
  - blaze-intent's `assoc` replaced by `as_assoc_item`;
  - base-db's unused `DbPanicContext` deleted.

## Kept apart

- **Normalizers**: they differ in the ambiguous case.
- **`const-ast`**: a dependency-free `const fn` port, on purpose.
- **Trait impls and facade methods**: one per type.
- **Binder-depth folders**: they fold different things.
- **Float condition codes**: they map to different target enums.
- **Hashes**: the persisted hashes differ.

## Bound

Groups 68 to 140 each save at most 14 lines and were not worked.
