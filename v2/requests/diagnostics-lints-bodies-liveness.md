# Requests of diagnostics-lints-bodies-liveness

What `lints/bodies/liveness.rs` (rustc's `check_liveness`: `unused_variables`,
`unused_assignments`) needs from other lanes. The lints are correct without 1 and 2; 3 and 4 are
cases they miss today.

## 1. hir-ty-mir: mark the guard references in `MirBody`

What: `MirBody::guard_locals: ArenaMap<LocalId, BindingId>` (or a `LocalInfo` per local), set
where `lower.rs` takes `tmp = &place` for a by-value binding of a guarded arm (rustc's
`BindingForm::RefForGuard(arm_local)`). Why: liveness merges the guard's reads into the
binding's. It now recognises the reference by its shape: an unprojected `Ref` assigned at the
binding's `PatId` to a local that is no binding's. That holds for what the lowering emits today,
but a later change to it would break it without anything failing loudly.

> forwarded to codegen: `crates/hir-ty-mir/` is lane codegen's.
> codegen-rq-mir (2026-10-30): db's field is in (`MirBody::guard_refs: Vec<(LocalId, BindingId)>`).
> applied by codegen-rq-mir (2026-10-30): `lower/pattern_matching.rs` pushes `(reference, id)`
> into the frame `Building::guard_refs` right after `self.guard_refs.insert`, and `finish`
> keeps it as `MirBody::guard_refs` at its final size (`exact`), as it does `unreached`. Not
> built; frontend-fast-mir-next2 (holder of `lower/`) told.

> taken by codegen (lead, 2026-10-29): child codegen-rq-mir.

## 2. hir-ty-mir: mark the capture copies at a closure's start

What: on `MirBody`, the statements (or the locals) that copy a capture out of the closure's own
local at its start (`cap = move _1.f` / `(*_1).f`, at `MirSpan::Unknown`), with the field.
Why: rustc has no such copies, so liveness must treat them as neither a use nor an assignment.
It now finds them as an `Assign` at `MirSpan::Unknown` of an `upvar_locals` local from the
closure's own local. `upvar_locals` already maps the capture to its local.

> forwarded to codegen: `crates/hir-ty-mir/` is lane codegen's.
> codegen-rq-mir (2026-10-30): db's field is in (`MirBody::capture_copies: Vec<(LocalId,
> FieldIndex)>`: each listed local's first `Assign` in `start_block` is the copy).
> applied by codegen-rq-mir (2026-10-30): the closure lowering pushes
> `(capture_local, FieldIndex(capture_idx))` into `Building::capture_copies` right after the
> copy's `push_assignment`, only where the copy is made (not for a borrowed closure's owned
> capture used in place), and `finish` keeps it as `MirBody::capture_copies`. Not built.

> taken by codegen (lead, 2026-10-29): child codegen-rq-mir.

## 3. the parent (`lints/bodies.rs`) or hir-ty-mir: the MIR of an inline const

What: liveness run on each `const { .. }` block (rustc's `check_liveness` runs on every body
owner, inline consts included), either through a `BodyCx` of its own or through a MIR query for
it. Why: `BodyCx::exprs` leaves inline consts out and no MIR of one is reachable from the
parent's MIR, so `let x = 1;` in a `const { }` gets no `unused_variables`.

> forwarded to diagnostics (the parent's `lints/bodies.rs`) and codegen (`hir-ty-mir`).
> codegen-rq-mir (2026-10-30), hir-ty-mir's half: nothing to add, it's there. In a function, a
> `const { .. }` is a body of its own: `hir_ty_mir::mir::inline_const_at(db,
> InferBodyId::DefWithBodyId(owner), infer, expr)` gives its `AnonConstId`, and
> `db.mir_body(InferBodyId::AnonConstId(it))` gives its MIR (hir-ty-borrowck's `lib.rs:660`
> does this already). That needs `hir-ty-mir.workspace = true` in blaze-diagnostics'
> `Cargo.toml`, which creates no cycle (it depends on hir-ty-borrowck, which depends on
> hir-ty-mir). `inline_const_at` gives `None` in a const or static, and where the type has
> errors or inference variables. There the block is lowered in place in the owner's MIR,
> with its bindings declared where the block binds them. So liveness can run on the owner's
> MIR restricted to the block's bindings (`store.binding_owner(b) == Some(block)`).

> taken by codegen (lead, 2026-10-29): the hir-ty-mir half, child codegen-rq-mir.

## 4. hir-ty-mir: a closure or coroutine whose MIR does not lower

What: nothing to change in liveness; listed so the cases are known. When
`mir_body_for_closure`/`mir_body_for_coroutine` fails for a body that is not tainted, the
closure gets no lints and the parent reads every capture as live. When `mir_body` of the body
itself fails, the body gets no lints. Each such case is a lowering gap of hir-ty-mir (its
`not_supported!`), and is reported there.

> forwarded to codegen: `crates/hir-ty-mir/` is lane codegen's.
> codegen-rq-mir (2026-10-30): noted, nothing to change. Each such body is a `not_supported!`
> in lowering, which stops with `MirLowerError::NotSupported` naming the construct (no
> fallback). The gaps are closed one at a time in lowering (`lower.rs`, frontend-fast-mir-next2
> today); `become`, the one named here, is the `TailCall` work in
> `v2/requests/codegen-rq-mir.md` R3.

> taken by codegen (lead, 2026-10-29): child codegen-rq-mir.
