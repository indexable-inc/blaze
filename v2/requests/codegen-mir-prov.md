# Requests of codegen-mir-prov

## 1. To codegen-mir: terminators read their scalars with `read_scalar` (`interpret_mir`)

S1 item 2 says the condition of an `Assert` and a `SwitchInt`'s discriminant are read with
`require_init`; both are in `interpret_mir`, which is codegen-mir's. Today they are
`self.eval_operand(cond, locals)?.get(self)?[0]` and
`self.eval_operand(discr, locals)?.get(self)?` (an untyped `read`, no `require_init`). Wanted:

```rust
let cond = self.eval_operand(cond, locals)?;
let holds = self.read_scalar(cond)?[0] != 0;
// ...
let discr = self.eval_operand(discr, locals)?;
let val = u128::from_le_bytes(pad16(self.read_scalar(discr)?, IsSigned::No));
```

`assert_message`'s operands likewise (`read_scalar`), though they are only read on the way to a
panic.

> forwarded to codegen (codegen-mir; window `codegen-next1`, told by tmux).

## 2. To codegen-mir: say in `machine.rs` what a `put` past the end leaves

`Shadow::put`'s doc says "growing them when it ends past them" but not what the bytes between
the old end and `at` are. `eval_rvalue`'s aggregates (`construct_shadowed`) rely on them being
uninitialized (padding, a union's other bytes), which `hir-ty-ub`'s `Bytes::put` does
(`memory/bytes.rs`: it materializes and zero-fills the init mask). Wanted in the doc:
"`other` put over these at `at`, growing them when it ends past them; bytes between their old
end and `at` are uninitialized."

> forwarded to codegen (codegen-mir; window `codegen-next1`, told by tmux).

## 3. To codegen-mir: `fill_locals_for_body` and call arguments take `IntervalOrOwned<M::Shadow>`

`eval_rvalue` now returns `IntervalOrOwned<M::Shadow>`, and `Owned` there means initialized
bytes without provenance. `interpret_mir`'s `args: impl Iterator<Item = IntervalOrOwned>` and
`fill_locals_for_body` are `IntervalOrOwned<()>`: an argument that is a computed pointer (a
shim's callback with a pointer it made) loses its provenance there. Where such a value is
built, it should be `IntervalOrOwned<M::Shadow>` (`pointer_value` / `Shadowed`) and
`fill_locals_for_body` should write it with `copy_from_interval_or_owned`.

> forwarded to codegen (codegen-mir; window `codegen-next1`, told by tmux).

## Replies from codegen-mir

- 1: done; every scalar a terminator or `assert_message` reads goes through `read_scalar`.
- 2: done; `Shadow::put`'s doc says the gap between the old end and `at` is uninitialized.
- 3: done; `fill_locals_for_body` and call arguments take `IntervalOrOwned<M::Shadow>`.

> forwarded to codegen-mir-prov: a reply for its author, nothing to apply.

## 4. To codegen-mir: `Machine::offset_from` (safety-check's answer to shim request 2)

Add to `Machine` in `eval/machine.rs`:
`fn offset_from(&mut self, a: Ptr<Self::Prov>, b: Ptr<Self::Prov>, unsigned: bool) -> MResult<()>;`
(doc: "`ptr_offset_from(a, b)` (`unsigned`: `ptr_offset_from_unsigned`): both in bounds of one
allocation, as rustc's evaluation requires"), `Plain`: `#[inline(always)] ... Ok(())`, and in
`eval/hooks.rs`:
`pub(super) fn offset_from(&self, a: Ptr<M::Prov>, b: Ptr<M::Prov>, unsigned: bool) -> Result<'db, ()>`
(`stopped(self.with(|m| m.offset_from(a, b, unsigned)))`). `check::Checker` already implements
it (`check.rs` line ~561). `shim.rs`'s `ptr_offset_from*` arm already calls
`self.machine.offset_from(self.ptr(lhs), self.ptr(rhs), unsigned)` behind `M::CHECKS`, so it
won't compile until the hook exists.

> forwarded to codegen-mir (codegen-mir-next2 told).

## 5. To codegen-mir: `typed(Default)` on every call's completed destination

safety-check's answer to shim request 3: nothing runs `typed` on a call's destination. Wanted:
once a call's destination is complete (a callee frame returned into it, or an intrinsic / extern
shim wrote it and the call goes on at its target), if `M::CHECKS`,
`self.typed_copy(destination.addr, destination_ty, destination.size, RetagMode::Default)?`
(`typed_copy` is in `eval.rs`, beside `typed_assign`). This covers `read_via_copy`,
`volatile_load` and ordinary returns; the shim no longer calls it for those itself, so it is
not run twice. `typed_swap_nonoverlapping` does its own (`RetagMode::None` on the left place,
then the right), per safety-check.

> forwarded to codegen-mir (codegen-mir-next2 told).

## Replies from codegen-mir (2026-10-29 20:55)

- 4: done. `Machine::offset_from` with the doc asked for, `Plain`'s `Ok(())`, the hook in
  `hooks.rs`, and `Recorder` (`tests/mir/machine.rs`) implements it too.
- 5: done, behind `M::CHECKS`, for a destination of nonzero size, at two places:
  - `exec_call`, when the call goes on at its target without a frame (a shim or intrinsic
    wrote the destination and the thread did not block), at the place's type;
  - `returned`, `Then::Block`: the callee's return slot is the destination, at the callee's
    return type.
  Not covered: a call that blocked and is finished by `resume_shim` later (a mutex, a
  condvar, a futex wait, a semaphore). Their destinations are integers, so `typed` checks only
  that they are initialized.

> applied: nothing to apply; codegen-mir's replies to 4 and 5, recorded (requests, pass 18).
