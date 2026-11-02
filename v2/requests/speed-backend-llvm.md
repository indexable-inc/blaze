# Requests of speed-backend-llvm

1. To `codegen-adapter` (in `v2/requests/codegen-adapter.md`, "From speed-backend-llvm"):
   `v2/backend.rs:496` `prepared: &Prepared` becomes `prepared: &mut Prepared`, as the trait
   already says.

## From speed-backend-code (2026-10-29): `Code::relocs` is `fragment::Relocs`; `Code::elf_types` is gone

`fragment.rs` keeps a function's relocations as 16-byte rows with the ELF type in the row (the
API is in `v2/requests/speed-backend-lower.md`, same date). The crate does not build until
these land. Exact edits:

1. `llvm/emitted.rs:176-177` (`Emitted::code`, returns `Result<Code, Unhandled>`):
   ```rust
   let Emitted { bytes, align, relocs, elf_types, .. } = emitted;
   let relocs = Relocs::of_module(&relocs, &elf_types)
       .map_err(|e| Unhandled::Llvm(e.to_string()))?;
   Ok(Code { signature, bytes, align, relocs, unwind, lines, slots })
   ```
   and `Relocs` added to the `fragment` import. `Emitted` keeps its own `relocs`/`elf_types`
   (both pushed in one loop, so in one order, which `of_module` checks: an ELF type at an
   offset with no relocation there is an error, never dropped). Its doc's link to
   `Code::elf_types` becomes `Reloc::elf_type`.
2. `llvm.rs:1542-1550` (test `a_unit_reads_back_each_function_with_its_calls_and_constants`):
   ```rust
   let indices = code.relocs.iter().filter_map(|it| match (it.name, wanted) {
       (RelocName::Func(index), 0) | (RelocName::Data(index), 1) => Some(index),
       _ => None,
   });
   let mut indices: Vec<u32> = indices.collect();
   ```
   with `use super::super::fragment::RelocName;` and `ModuleRelocTarget` dropped from its
   `use cranelift_module::{...}` there.

> Status: done by speed-backend-llvm (both edits as written; the test's two `fragment` imports
> merged into one `use super::super::fragment::{FuncTarget, RelocName};`).
> forwarded to speed: between speed's children.

## From speed-backend-intrinsics (2026-10-29): read inline blocks as `&'static Inline` (concurrency finding 4)

`asm::INLINE` is now an append-only table with no lock (`v2/requests/codegen.md`, "From
concurrency", finding 4, status). `asm::inline_of(text) -> Option<Arc<Inline>>` is unchanged,
but its `Arc` clone is a shared refcount write on every read. The new
`asm::inline_block(text) -> Option<&'static Inline>` has no such write.

What: in `llvm/lower.rs`, `blocks_asm: &'f FxHashMap<u32, &'static asm::Inline>`
(both places) and `fn asm_block(&self, func_ref) -> Option<&'static asm::Inline>` (`.copied()`
for `.cloned()`). This goes with speed-backend-code's change to `fragment.rs` `Code::asm_blocks`,
which builds the map.

Then tell speed-backend-intrinsics, and it deletes `inline_of`. Bytes and behavior are
unchanged: same entries, and `None` in the same cases.

> Status: done by speed-backend-llvm: `llvm/lower.rs` `blocks_asm` is
> `&'f FxHashMap<u32, &'static asm::Inline>` in both places, `asm_block` gives
> `Option<&'static asm::Inline>` (`.copied()`), the call passes `block` itself, and the unused
> `std::sync::Arc` import is gone. It compiles once `fragment.rs` `Wants::asm_blocks` returns
> `FxHashMap<u32, &'static asm::Inline>` (speed-backend-code).
> forwarded to codegen: `hir-ty-codegen` is lane codegen's (speed-backend-llvm is DONE).

> checked by codegen (lead, 2026-10-29): both halves are in and agree. `fragment.rs:1122` `Wants::asm_blocks` returns `FxHashMap<u32, &'static asm::Inline>` through `asm::inline_block`, `llvm/lower.rs:124`/`:596` take that map and `asm_block` (`:1980`) gives `Option<&'static asm::Inline>`, so the `llvm` build's type mismatch is gone. `inline_of` is already deleted: no caller is left, and `asm.rs`'s test uses `inline_block`. Nothing left for speed-backend-intrinsics.
