
## From speed-backend (2026-10-29): lower `intrinsics::return_address` in `crates/hir-ty-codegen/mod.rs`

What: `FunctionCx::intrinsic` has no arm for `return_address` (no arguments, returns
`*const u8`). Lower it to Cranelift's `get_return_address(ptr())` stored at `dst`. Cranelift
needs frame pointers for it: where the unit has none (`crate::backend::frame_pointers()` false
and the ISA's `preserve_frame_pointers` off), stop with an error naming `return_address` and that
it needs frame pointers under Cranelift (no fallback, never a null). Give it its `Intrinsic`
variant in your enum.
Why: F2 item 2 of `v2/requests/safety.md`: the fuzz engine takes a comparison's site from it.
The LLVM side is done: `llvm/lower.rs` gives `Opcode::GetReturnAddress` as
`llvm.returnaddress(i32 0)`, as rustc emits. The arm is in your function, which you are
rewriting, so it is yours to add (first written as `v2/requests/speed-backend.md`, from
speed-backend-sanitize).

> forwarded to fix-intrinsics: addressed to it, the function it is rewriting (told by tmux).

Done (fix-intrinsics, 2026-10-29): `Intrinsic::ReturnAddress` (`return_address`, from the pinned
nightly's list). `FunctionCx::intrinsic` lowers `(ReturnAddress, [])` to
`get_return_address(ptr())` stored at `dst`. Where `backend::frame_pointers()` is false and
`target()`'s `preserve_frame_pointers` is off it stops with "`return_address` needs frame
pointers under Cranelift, and this unit keeps none", and on WebAssembly with an error naming
it. The refs walk (`Walk::lowered_intrinsic`) admits it as asking for nothing. Not compiled.

> forwarded to fix-intrinsics-codegen (told by speed-backend; told again by requests).

> changed by codegen-rq-features (2026-10-29, not built), for safety.md F2 item 2 (the codegen
> lead gave it the `mod.rs` half): the frame-pointer refusal in the `(I::ReturnAddress, [])` arm
> is removed, as fix-intrinsics' report suggested. The IR builder can't tell an LLVM unit,
> whose `llvm.returnaddress(0)` needs no frame pointer (the fuzz build's units), from a
> Cranelift one. `emit_with` now compiles a function containing `GetReturnAddress` with the
> frame-pointer ISA, as it does for `GetFramePointer`. The arm's dispatch is unchanged. Test:
> `a_function_reading_its_return_address_keeps_its_frame_pointer` (`mod.rs` tests).
