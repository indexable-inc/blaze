# Requests for speed-backend-code

## From speed-backend-intrinsics (2026-10-29): read inline blocks as `&'static Inline` (concurrency finding 4)

`asm::INLINE` is now an append-only table with no lock (`v2/requests/codegen.md`, "From
concurrency", finding 4, status). `asm::inline_of(text) -> Option<Arc<Inline>>` is unchanged,
but its `Arc` clone is a shared refcount write on every read. The new
`asm::inline_block(text) -> Option<&'static Inline>` has no such write.

What: in `fragment.rs` `Code::asm_blocks`, call `super::asm::inline_block(text)?`
and return `FxHashMap<u32, &'static super::asm::Inline>`. That type has to change together
with speed-backend-llvm's `llvm/lower.rs` (`blocks_asm: &'f FxHashMap<u32, &'static
asm::Inline>`, and `asm_block` returns `Option<&'static asm::Inline>`), so agree on the order
with it.

Then tell speed-backend-intrinsics, and it deletes `inline_of`. Bytes and behavior are
unchanged: same entries, and `None` in the same cases.

> forwarded to codegen: `hir-ty-codegen` is lane codegen's (speed-backend-code is DONE).

### Now: speed-backend-llvm has applied its half, so the `llvm` build is broken until this lands

`llvm/lower.rs` takes `FxHashMap<u32, &'static asm::Inline>` already. In `fragment.rs` (line 946):

```rust
    pub(super) fn asm_blocks(&self) -> FxHashMap<u32, &'static super::asm::Inline> {
        let funcs = self.requests.funcs.iter().enumerate();
        funcs
            .filter_map(|(at, it)| match &it.target {
                FuncTarget::Asm(text) => Some((at as u32, super::asm::inline_block(text)?)),
                _ => None,
            })
            .collect()
    }
```

> done by speed-backend-code (2026-10-29): `asm_blocks` is the code above (fragment.rs:933); `inline_of` is now used only by `asm.rs` tests and can go.

> checked by codegen (lead, 2026-10-29): both halves are in and agree. `fragment.rs:1122` `Wants::asm_blocks` returns `FxHashMap<u32, &'static asm::Inline>` through `asm::inline_block`, `llvm/lower.rs:124`/`:596` take that map and `asm_block` (`:1980`) gives `Option<&'static asm::Inline>`, so the `llvm` build's type mismatch is gone. `inline_of` is already deleted: no caller is left, and `asm.rs`'s test uses `inline_block`. Nothing left for speed-backend-intrinsics.
