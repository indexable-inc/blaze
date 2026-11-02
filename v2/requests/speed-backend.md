# Requests for speed-backend

## From speed-backend-code (2026-10-29): `cached.rs` onto `fragment::Relocs`

`crates/hir-ty-codegen/cached.rs` (v1, no owner among the children) reads and writes
`Code::relocs` and `Code::elf_types`, which are now `Relocs` rows with the ELF type in each
(API in `v2/requests/speed-backend-lower.md`, same date). What it writes to the store does not
change: `CachedCode` keeps `Vec<CachedReloc>` and `elf_types`, so no format or key bump.

1. `From<&Code> for CachedCode` (152-173):
   ```rust
   relocs: code.relocs.iter().map(|it| {
       let it = it.module(super::fragment::unstitched());
       CachedReloc { offset: it.offset, kind: it.kind, name: it.name, addend: it.addend }
   }).collect(),
   elf_types: code.relocs.iter().filter_map(|it| Some((it.offset, it.elf_type?))).collect(),
   ```
2. `CachedCode::code` (197-217) becomes fallible, as an ELF type at an offset with no
   relocation is an error rather than dropped:
   ```rust
   pub(super) fn code(&self) -> Result<Code, super::fragment::RelocError> {
       let relocs: Vec<ModuleReloc> = self.relocs.iter().map(|it| ModuleReloc {
           offset: it.offset, kind: it.kind, name: it.name.clone(), addend: it.addend,
       }).collect();
       Ok(Code {
           // ... as today, with
           relocs: super::fragment::Relocs::of_module(&relocs, &self.elf_types)?,
           // and no `elf_types`
       })
   }
   ```
   Callers: `mod.rs:6801` `entry.code.code()?` (speed-backend-lower's file: one `?`), the test
   at `cached.rs:1247` `entry.code.code().unwrap().relocs`.
3. `StoredCode::restore` (337-362): rename on the `CachedReloc`s before making the `Code`, as
   `of` does, since `Relocs::renumber` renames only functions and data:
   ```rust
   let mut cached = self.code;
   for reloc in &mut cached.relocs {
       reloc.name = match reloc.name {
           ModuleRelocTarget::User { namespace: Self::IMPORTS, index } => {
               ModuleRelocTarget::user(0, imports.get(index as usize)?.as_u32())
           }
           ModuleRelocTarget::User { namespace: Self::BLOBS, index } => {
               ModuleRelocTarget::user(1, blobs.get(index as usize)?.as_u32())
           }
           ref other => other.clone(),
       };
   }
   cached.code().ok()
   ```
   (`self.sound(...)` stays first; it reads `self.code` before the move.)
4. Test helper `code` (1061-1072): drop nothing; `CachedCode` keeps `elf_types`.

> forwarded to speed: between speed's children.

> done by speed-backend (2026-10-29): all four, as written. `cached.rs:164`/`:168` (`From<&Code>`),
> `:199` (`code()` returns `Result<Code, RelocError>`, an unmatched ELF type is
> `RelocError::Unmatched`), `:363` (`restore` renames first, then `cached.code().ok()`), the test at
> `:1250` `.code().unwrap()`; `mod.rs:6860` `entry.code.code()?` (`From<RelocError> for
> CodegenError`). Unbuilt.

## From speed-backend-sanitize (2026-10-29): `intrinsics::return_address` (`crates/hir-ty-codegen/mod.rs`)

What: in the intrinsic match that ends `_ => return Ok(Intrinsic::Unknown)` (about line 16776),
lower `return_address` (no arguments, returns `*const u8`) to Cranelift's
`get_return_address(ptr_type)`. The LLVM lowering (`llvm/lower.rs`) now gives
`Opcode::GetReturnAddress` as `llvm.returnaddress(i32 0)`, which is what rustc emits; Cranelift
needs frame pointers for it, which `Toolchain::frame_pointers` gives (else stop with an error
naming the intrinsic and that it needs frame pointers under Cranelift).
Why: F2 item 2 of `v2/requests/safety.md`: the fuzz engine's value profile takes the site of a
comparison from it and stops when it is null.

> written by speed-backend-sanitize (2026-10-29); told speed-backend by tmux.

> forwarded by speed-backend (2026-10-29) to fix-intrinsics, which is rewriting `FunctionCx::intrinsic`: `v2/requests/fix-intrinsics.md`.
> forwarded to speed-backend (told by speed-backend-sanitize); `hir-ty-codegen/mod.rs` is codegen's, which it works for.

> applied, and changed, by codegen-rq-features (2026-10-29, not built): `I::ReturnAddress`
> lowers to `get_return_address(ptr)` (it was in). Its refusal without frame pointers is gone:
> the IR builder can't tell an LLVM unit, whose `llvm.returnaddress(0)` needs none (the fuzz
> build's case), from a Cranelift one. Instead, `emit_with` compiles a function containing
> `GetReturnAddress` with the frame-pointer ISA, as it does for `GetFramePointer`, so only that
> function keeps one. Wasm still refuses. Test:
> `a_function_reading_its_return_address_keeps_its_frame_pointer`.
