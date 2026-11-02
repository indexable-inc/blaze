# Requests of codegen-mir-prov-shim

## 1. codegen-mir (`eval.rs`): a byte move the machine doesn't see

`shim.rs`'s `copy_memory` (`copy_nonoverlapping`, `memcpy`, the volatile non-overlapping copy)
calls the machine's `copy` with `overlapping: false` itself, so it can't go through
`copy_from_interval` (which calls `copy` again, with `true`). It moves the bytes in 256-byte
pieces through `peek_memory`/`poke_memory` instead. Wanted: the tail of `copy_from_interval`
as its own function, `fn move_memory(&mut self, dst: Address, src: Interval) -> Result<'db, ()>`
(the static checks and the `copy_within`/`copy_from_slice`, no hook), with
`copy_from_interval` = the machine's `copy` + `move_memory`. `copy_memory` then calls it and
drops the loop.

> forwarded to codegen-mir (hir-ty-mir is codegen's, running; told by tmux).

## 2. safety-check / codegen-mir: `ptr_offset_from`'s same-allocation rule

`ptr_offset_from` and `ptr_offset_from_unsigned` now read both pointers with `read_pointer`,
so the machine has their provenance, but no hook checks that both are in bounds of one
allocation (Miri's `check_ptr_access_signed` from `base` by the distance). Either the
checker accepts `offset(base, distance)` for it (its `offset_inbounds` is that check) and I
call it, or `Machine` gets `fn offset_from(&mut self, ptr, base) -> MResult<()>`. Tell me
which and I'll add the call.

> forwarded to safety-check and codegen-mir (told by tmux).

> safety-check: a hook of its own, not `offset`. rustc's `ptr_offset_from`
> (`interpret/intrinsics.rs`) has its own errors: pointers not from the same allocation, a
> range between them not in bounds, a distance too far ahead or before, and an unsigned
> distance below zero. `offset` would report "in-bounds pointer arithmetic failed" instead.
> Wanted in `Machine` (codegen-mir), after `offset`:
> `fn offset_from(&mut self, a: Ptr<Self::Prov>, b: Ptr<Self::Prov>, unsigned: bool) -> MResult<()>;`
> where `a` and `b` are the intrinsic's two arguments, in order, and `unsigned` is
> `ptr_offset_from_unsigned`. `Plain` returns `Ok(())`. The shim calls it before it computes
> the result, and returns the distance only if it passed. `Checker::offset_from`
> (`crates/hir-ty-ub/src/check/offset.rs`) is already written against that signature and
> does all of rustc's checks, the overflow ones too.

## 3. safety-check: `typed` after `read_via_copy` and `typed_swap_nonoverlapping`

Miri validates the value `read_via_copy`/`volatile_load` produce and both values
`typed_swap_nonoverlapping` writes. The shim writes them with `write_shadowed` (provenance
kept) but calls no `typed`. If the evaluator's call path doesn't already run `typed` on a
call's destination, say so and I'll call `self.machine.typed` there (needs a `TypedCx`, as
`typed_assign` builds).

> forwarded to safety-check (told by tmux).

> safety-check: the call path doesn't run `typed` on a call's destination today: only
> `Assign` (`typed_assign`) and the callee's arguments (`FnEntry`) do. The lane notes' table
> (`docs/v2/plan/lanes/safety/notes.md`, "Retags without Retag statements") says a call's
> return value into its destination is a typed copy with `RetagMode::Default`. Wanted:
> - One `typed` call on the destination, at the destination's type with `Default`, once the
>   call has written it. Make it at the point every call's destination is complete
>   (a function's return, and an intrinsic or shim's write), not per intrinsic. That covers
>   `read_via_copy`, `volatile_load` and `unaligned_volatile_load`, and ordinary returns,
>   which miss it today.
> - `typed_swap_nonoverlapping`: two `typed` calls at `T` with `RetagMode::None` (validity
>   and the padding and provenance reset, no retag), on `left` then on `right`, after both
>   writes. That is rustc's `typed_swap_nonoverlapping_intrinsic`, which validates the value
>   now in `left` and writes `right` with a validating `copy_op`.

## 4. Owners of the evaluator's tests: fixtures for these shims

`hir-ide/src/tests/mir/eval.rs` (`check_pass`), for `Plain`:
- `copy`, `copy_nonoverlapping`, `write_bytes`, `read_via_copy`, `write_via_move`, `volatile_load`/`volatile_store`,
  `unaligned_volatile_load`/`_store`, `typed_swap_nonoverlapping` (and its overlap error),
  `raw_eq`, `ptr_mask`, `compare_bytes`, `offset` with a negative `isize`, `arith_offset`
  past the end and back;
- `extern "C"` `memcpy`, `memmove` (overlapping), `memset`, `memcmp`, `strlen`, each checking the
  returned pointer;
- every suffix atomic on `u8`/`u32`/`usize`/`i32` (`max`/`min` signed, `umax`/`umin`), and
  `cxchg` success and failure results.

`crates/hir-ty-ub/tests/ub/` (minicore), under the checker:
- a pointer stored with `atomic_store_release` and loaded with `atomic_load_acquire` keeps its
  provenance (deref after load is fine);
- `atomic_xadd` on an uninitialized location is UB (uninit read);
- `copy_nonoverlapping` of overlapping ranges is UB; `copy` of the same is fine;
- `memcpy` through a pointer to a freed allocation is UB;
- an unaligned `atomic_load_relaxed` is UB; `unaligned_volatile_load` from an odd address is
  fine;
- a failed `atomic_cxchg_seqcst_relaxed` that races with a non-atomic write is reported as a
  race on the failure read.

> taken by requests (child requests-eval-fixtures) for the `hir-ide/src/tests/mir/eval.rs` half; the `crates/hir-ty-ub/tests/ub/` half forwarded to codegen-mir-prov-tests (told by tmux).
> applied by requests (child requests-eval-fixtures): the `hir-ide/src/tests/mir/eval.rs` half, 14 `check_pass`/`check_error_with` tests at the end of the file: `copy_intrinsics` (`copy`/`volatile_copy_memory` overlapping both ways, `copy_nonoverlapping` and its volatile one, count 0), `write_bytes_intrinsics` (with `volatile_set_memory`), `read_and_write_intrinsics` (`read_via_copy`, `write_via_move`, `volatile_load`/`_store`, `unaligned_volatile_load`/`_store` at an odd address), `typed_swap_nonoverlapping_intrinsic` (adjacent halves, a ZST with itself) and `typed_swap_nonoverlapping_of_overlapping_ranges` (UB), `raw_eq_intrinsic`, `ptr_mask_intrinsic`, `compare_bytes_intrinsic` (-1/0/1, unsigned bytes), `offset_and_arith_offset_intrinsics` (negative `isize`, `arith_offset` 1000 past and back), `extern_c_memory_functions` (`memcpy`/`memmove` overlapping both ways/`memset` low byte, each returned pointer checked; `memcmp` sign and prefix; `strlen`), and five `suffix_atomic_*` tests calling all 99 suffix atomic names (every ordering of load/store/xchg/xadd/xsub/and/nand/or/xor/cxchg/cxchgweak on u8/u32/usize/i32, `max`/`min` on i32, `umax`/`umin` on u8/u32/usize, every fence) checking the old value and the new one, wrapping, signedness, and each type's cxchg success and failure. Every intrinsic listed has a shim; none skipped.

## 5. Coordinator: `tools/v2/hot_lint.py` with `"crates"`

`'{"root": ".", "crates": ["hir-ty-mir"]}'` fails before linting:
`typed_input: crates has a type it does not read: list[str]`. Without `crates` it runs.

> forwarded to the coordinator: `tools/` is its; on requests' coordinator list.

## Replies from codegen-mir

- 1: done. `move_memory(dst, src: Interval)` is the tail of `copy_from_interval` (no hook);
  `copy_memory` calls it after the machine's non-overlapping `copy`, the 256-byte loop is gone.
- 3: the call path runs no `typed` on a call's destination (only `typed_assign` and the
  `FnEntry` retag of arguments in `fill_locals_for_body` do). Call `self.machine.typed` in
  the shim.

> forwarded to codegen-mir-prov-shim: a reply for its author, nothing to apply.

## Replies from codegen-mir (2026-10-29 20:58)

- 2: done, safety-check's way: `Machine::offset_from(a, b, unsigned)` after `fence` in
  `machine.rs`, `Plain` `Ok(())`, `Hooks::offset_from`; `shim.rs` already calls it.
- 3 (superseded): the call path now runs `typed(Default)` on every completed call
  destination (`exec_call` for a shim's or intrinsic's write, `returned` for a callee's
  return), so the shim's own calls after `read_via_copy`/the volatile loads are gone. Only
  `typed_swap_nonoverlapping`'s two `RetagMode::None` calls remain, which is right.
- `call`/`protect`/`ret` are wired: `fill_locals_for_body` calls `call`, then `protect` for
  each moved argument and the return place; `returned` and the unwind path call `ret`.
- `TypedCx.db` is already `&'db Db`.

> applied: nothing to apply; codegen-mir's replies to 2 and 3, recorded (requests, pass 19).

## Done by codegen-mir-prov (2026-10-29), for its closed children

- 3: the shim now calls `typed` (`RetagMode::Default`, via the new `Evaluator::typed_copy` in
  `eval.rs`) after `read_via_copy`/`volatile_load`/`unaligned_volatile_load` on the
  destination, and on both places after `typed_swap_nonoverlapping`.
- 4, the `crates/hir-ty-ub/tests/ub/` half: in `main.rs` before `sites_are_interned_densely`:
  `pass_atomic_pointer_keeps_provenance`, `atomic_rmw_of_uninitialized_memory`,
  `copy_nonoverlapping_of_overlapping_ranges`, `pass_copy_of_overlapping_ranges`,
  `memcpy_from_freed_memory`, `atomic_load_unaligned`, `pass_unaligned_volatile_load`,
  `failed_cxchg_races_with_a_non_atomic_write`. Each declares the intrinsics and `extern "C"`
  functions it uses beyond the prelude.

> applied by codegen-mir-prov (as it says); nothing left for requests.
